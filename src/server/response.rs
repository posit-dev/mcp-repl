use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rmcp::model::{AnnotateAble, CallToolResult, Content, Meta, RawContent, RawImageContent};
use serde_json::json;
use tempfile::Builder;

use crate::worker_process::WorkerError;
use crate::worker_protocol::{ContentOrigin, WorkerContent, WorkerErrorCode, WorkerReply};

const INLINE_TEXT_BUDGET: usize = 3500;
const IMAGE_OUTPUT_BUNDLE_THRESHOLD: usize = 5;
const INLINE_IMAGE_COST: usize = 900;
const HEAD_TEXT_BUDGET: usize = INLINE_TEXT_BUDGET / 3;
const PRE_LAST_TEXT_BUDGET: usize = INLINE_TEXT_BUDGET / 5;
const POST_LAST_TEXT_BUDGET: usize = INLINE_TEXT_BUDGET / 8;
const TEXT_ROW_OVERHEAD_BYTES: usize = 160;
const DEFAULT_OUTPUT_BUNDLE_MAX_COUNT: usize = 20;
const DEFAULT_OUTPUT_BUNDLE_MAX_BYTES: u64 = 1 << 30;
const DEFAULT_OUTPUT_BUNDLE_MAX_TOTAL_BYTES: u64 = 2 << 30;
const OUTPUT_BUNDLE_MAX_COUNT_ENV: &str = "MCP_REPL_OUTPUT_BUNDLE_MAX_COUNT";
const OUTPUT_BUNDLE_MAX_BYTES_ENV: &str = "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES";
const OUTPUT_BUNDLE_MAX_TOTAL_BYTES_ENV: &str = "MCP_REPL_OUTPUT_BUNDLE_MAX_TOTAL_BYTES";
const OUTPUT_BUNDLE_HEADER: &[u8] = b"v1\ntext transcript.txt\nimages images/\n";
const OUTPUT_BUNDLE_OMITTED_NOTICE: &str = "output bundle quota reached; later content omitted";

pub(crate) struct ResponseState {
    output_store: OutputStore,
    active_timeout_bundle: Option<ActiveOutputBundle>,
}

struct OutputStore {
    root: Option<tempfile::TempDir>,
    next_id: u64,
    total_bytes: u64,
    limits: OutputStoreLimits,
    bundles: VecDeque<StoredBundle>,
}

struct ActiveOutputBundle {
    id: u64,
    paths: OutputBundlePaths,
    next_image_number: usize,
    transcript_bytes: usize,
    transcript_lines: usize,
    omitted_tail: bool,
    omission_recorded: bool,
}

struct BundleAppendResult {
    retained_items: Vec<ReplyItem>,
    omitted_this_reply: bool,
}

#[derive(Clone)]
struct OutputBundlePaths {
    transcript: PathBuf,
    events_log: PathBuf,
    images_dir: PathBuf,
}

struct StoredBundle {
    id: u64,
    dir: PathBuf,
    bytes_on_disk: u64,
}

struct OutputStoreLimits {
    max_bundle_count: usize,
    max_bundle_bytes: u64,
    max_total_bytes: u64,
}

#[derive(Clone)]
enum ReplyItem {
    WorkerText(String),
    ServerText(String),
    Image(ReplyImage),
}

#[derive(Clone)]
struct ReplyImage {
    data: String,
    mime_type: String,
    id: String,
    is_new: bool,
}

struct ReplyMaterial {
    items: Vec<ReplyItem>,
    worker_text: String,
    is_error: bool,
    error_code: Option<WorkerErrorCode>,
    image_count: usize,
    estimated_cost: usize,
}

impl ResponseState {
    pub(crate) fn new() -> Result<Self, WorkerError> {
        Ok(Self {
            output_store: OutputStore::new()?,
            active_timeout_bundle: None,
        })
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), WorkerError> {
        self.active_timeout_bundle = None;
        self.output_store.cleanup_now()
    }

    /// Converts a worker result into the final MCP reply, including transcript updates and
    /// oversized reply compaction.
    pub(crate) fn finalize_worker_result(
        &mut self,
        result: Result<WorkerReply, WorkerError>,
        pending_request_after: bool,
    ) -> CallToolResult {
        match result {
            Ok(reply) => self.finalize_reply(reply, pending_request_after),
            Err(err) => {
                eprintln!("worker write stdin error: {err}");
                finalize_batch(vec![Content::text(format!("worker error: {err}"))], true)
            }
        }
    }

    /// Splits worker-originated text from server-only notices, keeps timeout polls on one
    /// transcript path, and only discloses that path once text actually needs compaction.
    fn finalize_reply(
        &mut self,
        reply: WorkerReply,
        pending_request_after: bool,
    ) -> CallToolResult {
        let material = prepare_reply_material(reply);

        if material.error_code == Some(WorkerErrorCode::Timeout)
            && self.active_timeout_bundle.is_none()
        {
            let bundle = self
                .output_store
                .new_bundle()
                .expect("failed to create timeout output bundle");
            self.active_timeout_bundle = Some(bundle);
        }

        let active_append = if let Some(active) = self.active_timeout_bundle.as_mut() {
            Some(
                active
                    .append_items(&mut self.output_store, &material.items)
                    .expect("failed to append timeout output bundle"),
            )
        } else {
            None
        };

        let contents = if let Some(active) = self.active_timeout_bundle.as_ref() {
            let append = active_append
                .as_ref()
                .expect("active timeout bundle append result should exist");
            let retained_worker_text = worker_text_from_items(&append.retained_items);
            if append.omitted_this_reply {
                compact_text_bundle_items(
                    append.retained_items.clone(),
                    &retained_worker_text,
                    active,
                )
            } else if active.next_image_number > 0
                && should_use_output_bundle(
                    material.image_count.max(active.next_image_number),
                    material.estimated_cost,
                )
            {
                compact_output_bundle_items(&append.retained_items, active)
            } else if material.worker_text.chars().count() > INLINE_TEXT_BUDGET {
                compact_text_bundle_items(
                    append.retained_items.clone(),
                    &retained_worker_text,
                    active,
                )
            } else {
                materialize_items(append.retained_items.clone())
            }
        } else if material.image_count > 0
            && should_use_output_bundle(material.image_count, material.estimated_cost)
        {
            let mut bundle = self
                .output_store
                .new_bundle()
                .expect("failed to create output bundle");
            let append = bundle
                .append_items(&mut self.output_store, &material.items)
                .expect("failed to append output bundle");
            compact_output_bundle_items(&append.retained_items, &bundle)
        } else if material.worker_text.chars().count() > INLINE_TEXT_BUDGET {
            let mut bundle = self
                .output_store
                .new_bundle()
                .expect("failed to create output bundle");
            let append = bundle
                .append_items(&mut self.output_store, &material.items)
                .expect("failed to append output bundle");
            let retained_worker_text = worker_text_from_items(&append.retained_items);
            compact_text_bundle_items(append.retained_items, &retained_worker_text, &bundle)
        } else {
            materialize_items(material.items)
        };

        if !pending_request_after {
            self.active_timeout_bundle = None;
        }

        finalize_batch(contents, material.is_error)
    }
}

impl OutputStore {
    fn new() -> Result<Self, WorkerError> {
        let limits = OutputStoreLimits::from_env()?;
        let root = Builder::new()
            .prefix("mcp-repl-output-")
            .tempdir()
            .map_err(WorkerError::Io)?;
        Ok(Self {
            root: Some(root),
            next_id: 0,
            total_bytes: 0,
            limits,
            bundles: VecDeque::new(),
        })
    }

    fn cleanup_now(&mut self) -> Result<(), WorkerError> {
        if let Some(root) = self.root.take() {
            root.close().map_err(WorkerError::Io)?;
        }
        self.bundles.clear();
        self.total_bytes = 0;
        Ok(())
    }

    fn root_path(&self) -> &Path {
        self.root
            .as_ref()
            .expect("output store root should exist")
            .path()
    }

    fn new_bundle(&mut self) -> Result<ActiveOutputBundle, WorkerError> {
        self.prune_for_new_bundle(OUTPUT_BUNDLE_HEADER.len() as u64)?;
        self.next_id = self.next_id.saturating_add(1);
        let dir = self.root_path().join(format!("output-{:04}", self.next_id));
        let images_dir = dir.join("images");
        fs::create_dir_all(&images_dir).map_err(WorkerError::Io)?;
        let transcript = dir.join("transcript.txt");
        let events_log = dir.join("events.log");
        std::fs::File::create(&transcript).map_err(WorkerError::Io)?;
        let mut events = std::fs::File::create(&events_log).map_err(WorkerError::Io)?;
        events
            .write_all(OUTPUT_BUNDLE_HEADER)
            .map_err(WorkerError::Io)?;
        self.bundles.push_back(StoredBundle {
            id: self.next_id,
            dir: dir.clone(),
            bytes_on_disk: OUTPUT_BUNDLE_HEADER.len() as u64,
        });
        self.total_bytes = self
            .total_bytes
            .saturating_add(OUTPUT_BUNDLE_HEADER.len() as u64);
        Ok(ActiveOutputBundle {
            id: self.next_id,
            paths: OutputBundlePaths {
                transcript,
                events_log,
                images_dir,
            },
            next_image_number: 0,
            transcript_bytes: 0,
            transcript_lines: 0,
            omitted_tail: false,
            omission_recorded: false,
        })
    }

    fn append_bundle_bytes(
        &mut self,
        bundle_id: u64,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), WorkerError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(WorkerError::Io)?;
        file.write_all(bytes).map_err(WorkerError::Io)?;
        self.record_append(bundle_id, bytes.len() as u64);
        Ok(())
    }

    fn prepare_append_capacity(
        &mut self,
        bundle_id: u64,
        requested_bytes: u64,
    ) -> Result<u64, WorkerError> {
        let bundle_bytes = self
            .bundle_bytes(bundle_id)
            .expect("bundle metadata should exist for append");
        let bundle_remaining = self.limits.max_bundle_bytes.saturating_sub(bundle_bytes);
        let target = requested_bytes.min(bundle_remaining);
        self.prune_until_total_capacity(bundle_id, target)?;
        let total_remaining = self.limits.max_total_bytes.saturating_sub(self.total_bytes);
        Ok(target.min(total_remaining))
    }

    fn bundle_bytes(&self, bundle_id: u64) -> Option<u64> {
        self.bundles
            .iter()
            .find(|bundle| bundle.id == bundle_id)
            .map(|bundle| bundle.bytes_on_disk)
    }

    fn record_append(&mut self, bundle_id: u64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let bundle = self
            .bundles
            .iter_mut()
            .find(|bundle| bundle.id == bundle_id)
            .expect("bundle metadata should exist for append");
        bundle.bytes_on_disk = bundle.bytes_on_disk.saturating_add(bytes);
        self.total_bytes = self.total_bytes.saturating_add(bytes);
    }

    fn prune_for_new_bundle(&mut self, initial_bytes: u64) -> Result<(), WorkerError> {
        while self.bundles.len() >= self.limits.max_bundle_count {
            if !self.prune_oldest_inactive_bundle(None)? {
                return Err(WorkerError::Protocol(
                    "output bundle count quota left no room for a new bundle".to_string(),
                ));
            }
        }
        self.prune_until_total_capacity(0, initial_bytes)?;
        if self.total_bytes.saturating_add(initial_bytes) > self.limits.max_total_bytes {
            return Err(WorkerError::Protocol(
                "output bundle total quota is too small for a new bundle".to_string(),
            ));
        }
        Ok(())
    }

    fn prune_until_total_capacity(
        &mut self,
        active_bundle_id: u64,
        needed_bytes: u64,
    ) -> Result<(), WorkerError> {
        while self.total_bytes.saturating_add(needed_bytes) > self.limits.max_total_bytes {
            if !self.prune_oldest_inactive_bundle(Some(active_bundle_id))? {
                break;
            }
        }
        Ok(())
    }

    fn prune_oldest_inactive_bundle(
        &mut self,
        active_bundle_id: Option<u64>,
    ) -> Result<bool, WorkerError> {
        let Some(index) = self
            .bundles
            .iter()
            .position(|bundle| Some(bundle.id) != active_bundle_id)
        else {
            return Ok(false);
        };
        let bundle = self
            .bundles
            .remove(index)
            .expect("bundle index should exist");
        fs::remove_dir_all(&bundle.dir).map_err(WorkerError::Io)?;
        self.total_bytes = self.total_bytes.saturating_sub(bundle.bytes_on_disk);
        Ok(true)
    }
}

impl OutputStoreLimits {
    fn from_env() -> Result<Self, WorkerError> {
        let max_bundle_count =
            parse_limit_env::<usize>(OUTPUT_BUNDLE_MAX_COUNT_ENV, DEFAULT_OUTPUT_BUNDLE_MAX_COUNT)?;
        let max_bundle_bytes =
            parse_limit_env::<u64>(OUTPUT_BUNDLE_MAX_BYTES_ENV, DEFAULT_OUTPUT_BUNDLE_MAX_BYTES)?;
        let max_total_bytes = parse_limit_env::<u64>(
            OUTPUT_BUNDLE_MAX_TOTAL_BYTES_ENV,
            DEFAULT_OUTPUT_BUNDLE_MAX_TOTAL_BYTES,
        )?;
        if max_bundle_count == 0 {
            return Err(WorkerError::Protocol(
                "output bundle count quota must be greater than zero".to_string(),
            ));
        }
        if max_bundle_bytes < OUTPUT_BUNDLE_HEADER.len() as u64 {
            return Err(WorkerError::Protocol(format!(
                "{OUTPUT_BUNDLE_MAX_BYTES_ENV} must be at least {} bytes",
                OUTPUT_BUNDLE_HEADER.len()
            )));
        }
        if max_total_bytes < OUTPUT_BUNDLE_HEADER.len() as u64 {
            return Err(WorkerError::Protocol(format!(
                "{OUTPUT_BUNDLE_MAX_TOTAL_BYTES_ENV} must be at least {} bytes",
                OUTPUT_BUNDLE_HEADER.len()
            )));
        }
        Ok(Self {
            max_bundle_count,
            max_bundle_bytes,
            max_total_bytes,
        })
    }
}

impl ActiveOutputBundle {
    fn append_items(
        &mut self,
        store: &mut OutputStore,
        items: &[ReplyItem],
    ) -> Result<BundleAppendResult, WorkerError> {
        let mut retained_items = Vec::with_capacity(items.len());
        let mut omitted_this_reply = false;
        if self.omitted_tail {
            return Ok(BundleAppendResult {
                retained_items,
                omitted_this_reply,
            });
        }

        for item in items {
            let append = match item {
                ReplyItem::WorkerText(text) => self.append_worker_text(store, text)?,
                ReplyItem::ServerText(text) => self.append_server_text(store, text)?,
                ReplyItem::Image(image) => self.append_image(store, image)?,
            };
            if let Some(retained_item) = append {
                let partial_worker_text = matches!(
                    (item, &retained_item),
                    (ReplyItem::WorkerText(original), ReplyItem::WorkerText(retained))
                        if retained.len() < original.len()
                );
                retained_items.push(retained_item);
                if partial_worker_text {
                    omitted_this_reply = true;
                    self.apply_omission(store)?;
                    break;
                }
            } else {
                omitted_this_reply = true;
                self.apply_omission(store)?;
                break;
            }
        }

        Ok(BundleAppendResult {
            retained_items,
            omitted_this_reply,
        })
    }

    fn append_worker_text(
        &mut self,
        store: &mut OutputStore,
        text: &str,
    ) -> Result<Option<ReplyItem>, WorkerError> {
        if text.is_empty() {
            return Ok(None);
        }
        let start_byte = self.transcript_bytes;
        let start_line = self.transcript_lines.saturating_add(1);
        let omission_reserve = if self.omission_recorded {
            0
        } else {
            omission_event_line_len()
        };
        let granted = store.prepare_append_capacity(
            self.id,
            (text.len() + TEXT_ROW_OVERHEAD_BYTES + omission_reserve) as u64,
        )? as usize;
        if granted == 0 {
            return Ok(None);
        }
        let initial_retained = truncate_utf8_prefix(text, granted);
        if initial_retained.is_empty() {
            return Ok(None);
        }
        let mut retained = initial_retained;
        loop {
            let line_len = count_lines(retained);
            let end_byte = start_byte.saturating_add(retained.len());
            let end_line = self
                .transcript_lines
                .saturating_add(line_len)
                .max(start_line);
            let row = format!("T lines={start_line}-{end_line} bytes={start_byte}-{end_byte}\n");
            let reserve = if retained.len() < text.len() {
                omission_reserve
            } else {
                0
            };
            if retained
                .len()
                .saturating_add(row.len())
                .saturating_add(reserve)
                <= granted
            {
                store.append_bundle_bytes(self.id, &self.paths.transcript, retained.as_bytes())?;
                store.append_bundle_bytes(self.id, &self.paths.events_log, row.as_bytes())?;
                self.transcript_bytes = self.transcript_bytes.saturating_add(retained.len());
                self.transcript_lines = self.transcript_lines.saturating_add(line_len);
                return Ok(Some(ReplyItem::WorkerText(retained.to_string())));
            }
            let allowed_text_bytes = granted.saturating_sub(row.len().saturating_add(reserve));
            let next = truncate_utf8_prefix(retained, allowed_text_bytes);
            if next.is_empty() || next.len() == retained.len() {
                return Ok(None);
            }
            retained = next;
        }
    }

    fn append_server_text(
        &mut self,
        store: &mut OutputStore,
        text: &str,
    ) -> Result<Option<ReplyItem>, WorkerError> {
        let retained = self.append_events_log_text(store, text)?;
        Ok(retained.map(|text| ReplyItem::ServerText(text.to_string())))
    }

    fn append_events_log_text<'a>(
        &mut self,
        store: &mut OutputStore,
        text: &'a str,
    ) -> Result<Option<&'a str>, WorkerError> {
        if text.is_empty() {
            return Ok(None);
        }
        let line = build_events_log_server_line(text);
        let granted = store.prepare_append_capacity(self.id, line.len() as u64)?;
        if granted < line.len() as u64 {
            return Ok(None);
        }
        store.append_bundle_bytes(self.id, &self.paths.events_log, line.as_bytes())?;
        Ok(Some(text))
    }

    fn append_image(
        &mut self,
        store: &mut OutputStore,
        image: &ReplyImage,
    ) -> Result<Option<ReplyItem>, WorkerError> {
        let next_number = self.next_image_number.saturating_add(1);
        let extension = image_extension(&image.mime_type);
        let file_name = format!("{next_number:03}.{extension}");
        let path = self.paths.images_dir.join(&file_name);
        let bytes = STANDARD
            .decode(image.data.as_bytes())
            .map_err(|err| WorkerError::Protocol(format!("invalid image data: {err}")))?;
        let row = format!("I images/{file_name}\n");
        let granted = store.prepare_append_capacity(self.id, (bytes.len() + row.len()) as u64)?;
        if granted < (bytes.len() + row.len()) as u64 {
            return Ok(None);
        }
        fs::write(&path, &bytes).map_err(WorkerError::Io)?;
        store.record_append(self.id, bytes.len() as u64);
        store.append_bundle_bytes(self.id, &self.paths.events_log, row.as_bytes())?;
        self.next_image_number = next_number;
        Ok(Some(ReplyItem::Image(image.clone())))
    }

    fn apply_omission(&mut self, store: &mut OutputStore) -> Result<(), WorkerError> {
        self.omitted_tail = true;
        if self.omission_recorded {
            return Ok(());
        }
        let _ = self.append_events_log_text(store, OUTPUT_BUNDLE_OMITTED_NOTICE)?;
        self.omission_recorded = true;
        Ok(())
    }

    fn image_path(&self, index: usize) -> PathBuf {
        let stem = format!("{index:03}");
        for extension in ["png", "jpg", "jpeg", "gif", "webp", "svg"] {
            let path = self.paths.images_dir.join(format!("{stem}.{extension}"));
            if path.exists() {
                return path;
            }
        }
        self.paths.images_dir.join(format!("{stem}.png"))
    }
}

fn parse_limit_env<T>(name: &str, default: T) -> Result<T, WorkerError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let Some(value) = std::env::var_os(name) else {
        return Ok(default);
    };
    let value = value.to_string_lossy();
    value
        .parse::<T>()
        .map_err(|err| WorkerError::Protocol(format!("invalid {name}: {err}")))
}

fn truncate_utf8_prefix(text: &str, limit_bytes: usize) -> &str {
    let mut end = limit_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn build_events_log_server_line(text: &str) -> String {
    let escaped = serde_json::to_string(text).unwrap_or_else(|_| "\"<server_text>\"".to_string());
    format!("S {escaped}\n")
}

fn omission_event_line_len() -> usize {
    build_events_log_server_line(OUTPUT_BUNDLE_OMITTED_NOTICE).len()
}

/// Normalizes one worker reply into renderable items while preserving the split between
/// worker-originated transcript text and inline-only server notices.
fn prepare_reply_material(reply: WorkerReply) -> ReplyMaterial {
    let (contents, is_error, error_code) = match reply {
        WorkerReply::Output {
            contents,
            is_error,
            error_code,
            prompt: _,
            prompt_variants: _,
        } => (contents, is_error, error_code),
    };

    let contents = collapse_image_updates(contents);
    let mut items = Vec::with_capacity(contents.len());
    let mut worker_text = String::new();
    let mut image_count = 0usize;
    let mut estimated_cost = 0usize;

    for content in contents {
        match content {
            WorkerContent::ContentText { text, origin, .. } => {
                let text = if matches!(origin, ContentOrigin::Worker) {
                    normalize_error_prompt(text, is_error)
                } else {
                    text
                };
                if text.is_empty() {
                    continue;
                }
                match origin {
                    ContentOrigin::Worker => {
                        worker_text.push_str(&text);
                        estimated_cost = estimated_cost.saturating_add(text.chars().count());
                        items.push(ReplyItem::WorkerText(text));
                    }
                    ContentOrigin::Server => {
                        estimated_cost = estimated_cost.saturating_add(text.chars().count());
                        items.push(ReplyItem::ServerText(text))
                    }
                }
            }
            WorkerContent::ContentImage {
                data,
                mime_type,
                id,
                is_new,
            } => {
                image_count = image_count.saturating_add(1);
                estimated_cost = estimated_cost.saturating_add(INLINE_IMAGE_COST);
                items.push(ReplyItem::Image(ReplyImage {
                    data,
                    mime_type,
                    id,
                    is_new,
                }));
            }
        }
    }

    ReplyMaterial {
        items,
        worker_text,
        is_error,
        error_code,
        image_count,
        estimated_cost,
    }
}

pub(crate) fn finalize_batch(mut contents: Vec<Content>, is_error: bool) -> CallToolResult {
    ensure_nonempty_contents(&mut contents);
    let _ = is_error;
    CallToolResult::success(contents)
}

fn materialize_items(items: Vec<ReplyItem>) -> Vec<Content> {
    items
        .into_iter()
        .map(|item| match item {
            ReplyItem::WorkerText(text) | ReplyItem::ServerText(text) => Content::text(text),
            ReplyItem::Image(image) => image_to_content(&image),
        })
        .collect()
}

fn image_to_content(image: &ReplyImage) -> Content {
    content_image_with_meta(
        image.data.clone(),
        image.mime_type.clone(),
        image.id.clone(),
        image.is_new,
    )
}

fn worker_text_from_items(items: &[ReplyItem]) -> String {
    let mut out = String::new();
    for item in items {
        if let ReplyItem::WorkerText(text) = item {
            out.push_str(text);
        }
    }
    out
}

fn compact_text_bundle_items(
    items: Vec<ReplyItem>,
    worker_text: &str,
    bundle: &ActiveOutputBundle,
) -> Vec<Content> {
    let preview = build_preview(worker_text, &bundle.paths.events_log, bundle.omitted_tail);
    let mut out = Vec::new();
    let mut worker_inserted = false;
    for item in items {
        match item {
            ReplyItem::WorkerText(_) => {
                if !worker_inserted {
                    out.push(Content::text(preview.clone()));
                    worker_inserted = true;
                }
            }
            ReplyItem::ServerText(text) => out.push(Content::text(text)),
            ReplyItem::Image(image) => out.push(image_to_content(&image)),
        }
    }
    out
}

fn compact_output_bundle_items(items: &[ReplyItem], bundle: &ActiveOutputBundle) -> Vec<Content> {
    let first_image_idx = items
        .iter()
        .position(|item| matches!(item, ReplyItem::Image(_)));
    let last_image_idx = items
        .iter()
        .rposition(|item| matches!(item, ReplyItem::Image(_)));
    let mut out = Vec::new();

    let head_text = collect_prefix_text(
        items,
        first_image_idx.unwrap_or(items.len()),
        HEAD_TEXT_BUDGET,
    );
    if !head_text.is_empty() {
        out.push(Content::text(head_text));
    }
    if bundle.next_image_number > 0 {
        out.push(load_output_bundle_image_content(bundle, 1));
    }
    out.push(Content::text(build_output_bundle_notice(bundle)));
    let pre_last_text = collect_suffix_text_before(items, last_image_idx, PRE_LAST_TEXT_BUDGET);
    if !pre_last_text.is_empty() {
        out.push(Content::text(pre_last_text));
    }
    if bundle.next_image_number > 1 {
        out.push(load_output_bundle_image_content(
            bundle,
            bundle.next_image_number,
        ));
    }
    let post_last_text = collect_prefix_text_after(items, last_image_idx, POST_LAST_TEXT_BUDGET);
    if !post_last_text.is_empty() {
        out.push(Content::text(post_last_text));
    }
    out
}

fn should_use_output_bundle(image_count: usize, estimated_cost: usize) -> bool {
    image_count >= IMAGE_OUTPUT_BUNDLE_THRESHOLD || estimated_cost > INLINE_TEXT_BUDGET
}

fn build_output_bundle_notice(bundle: &ActiveOutputBundle) -> String {
    let omitted = if bundle.omitted_tail {
        "; later content omitted"
    } else {
        ""
    };
    match bundle.next_image_number {
        0 => format!(
            "...[middle truncated; ordered output bundle index: {}{}]...",
            bundle.paths.events_log.display(),
            omitted
        ),
        1 => format!(
            "...[middle truncated; first image shown inline; ordered output bundle index: {}{}]...",
            bundle.paths.events_log.display(),
            omitted
        ),
        _ => format!(
            "...[middle truncated; first and last images shown inline; ordered output bundle index: {}{}]...",
            bundle.paths.events_log.display(),
            omitted
        ),
    }
}

fn collect_prefix_text(items: &[ReplyItem], end_exclusive: usize, budget: usize) -> String {
    let mut out = String::new();
    for item in items.iter().take(end_exclusive) {
        let Some(text) = item_text(item) else {
            continue;
        };
        push_prefix_text(&mut out, text, budget);
        if out.chars().count() >= budget {
            break;
        }
    }
    out
}

fn collect_suffix_text_before(items: &[ReplyItem], index: Option<usize>, budget: usize) -> String {
    let Some(index) = index else {
        return String::new();
    };
    let mut parts = Vec::new();
    let mut remaining = budget;
    for item in items[..index].iter().rev() {
        let Some(text) = item_text(item) else {
            continue;
        };
        let suffix = take_suffix_chars(text, remaining);
        if suffix.is_empty() {
            continue;
        }
        remaining = remaining.saturating_sub(suffix.chars().count());
        parts.push(suffix);
        if remaining == 0 {
            break;
        }
    }
    parts.reverse();
    parts.concat()
}

fn collect_prefix_text_after(items: &[ReplyItem], index: Option<usize>, budget: usize) -> String {
    let start = index.map(|index| index.saturating_add(1)).unwrap_or(0);
    collect_prefix_text(&items[start..], items[start..].len(), budget)
}

fn item_text(item: &ReplyItem) -> Option<&str> {
    match item {
        ReplyItem::WorkerText(text) | ReplyItem::ServerText(text) => Some(text),
        ReplyItem::Image(_) => None,
    }
}

fn push_prefix_text(out: &mut String, text: &str, budget: usize) {
    if budget == 0 {
        return;
    }
    let used = out.chars().count();
    let remaining = budget.saturating_sub(used);
    if remaining == 0 {
        return;
    }
    let prefix = take_prefix_chars(text, remaining);
    out.push_str(&prefix);
}

fn take_prefix_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn take_suffix_chars(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(limit);
    chars[start..].iter().collect()
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let newline_count = text.bytes().filter(|byte| *byte == b'\n').count();
    if text.ends_with('\n') {
        newline_count
    } else {
        newline_count.saturating_add(1)
    }
}

fn image_extension(mime_type: &str) -> &str {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

fn mime_type_from_path(path: &Path) -> String {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "webp" => "image/webp".to_string(),
        "svg" => "image/svg+xml".to_string(),
        _ => "image/png".to_string(),
    }
}

fn load_output_bundle_image_content(bundle: &ActiveOutputBundle, index: usize) -> Content {
    let path = bundle.image_path(index);
    let bytes =
        fs::read(&path).unwrap_or_else(|err| panic!("failed to read output bundle image: {err}"));
    let mime_type = mime_type_from_path(&path);
    let data = STANDARD.encode(bytes);
    content_image_with_meta(data, mime_type, format!("plot-{index}"), true)
}

fn build_preview(text: &str, path: &Path, omitted_tail: bool) -> String {
    if omitted_tail && text.chars().count() <= INLINE_TEXT_BUDGET {
        return build_short_preview(text, path);
    }
    if let Some(preview) = build_line_preview(text, path, omitted_tail) {
        return preview;
    }
    build_char_preview(text, path, omitted_tail)
}

fn build_line_preview(text: &str, path: &Path, omitted_tail: bool) -> Option<String> {
    if !text.contains('\n') {
        return None;
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() < 3 {
        return None;
    }

    let head_budget = INLINE_TEXT_BUDGET * 2 / 3;
    let tail_budget = INLINE_TEXT_BUDGET / 3;

    let mut head_count = 0usize;
    let mut head_len = 0usize;
    while head_count < lines.len() {
        let next = head_len + lines[head_count].chars().count();
        if next > head_budget && head_count > 0 {
            break;
        }
        head_len = next;
        head_count += 1;
    }

    let mut tail_count = 0usize;
    let mut tail_len = 0usize;
    while tail_count < lines.len().saturating_sub(head_count) {
        let line = lines[lines.len() - 1 - tail_count];
        let next = tail_len + line.chars().count();
        if next > tail_budget && tail_count > 0 {
            break;
        }
        tail_len = next;
        tail_count += 1;
    }

    if head_count + tail_count >= lines.len() || head_count == 0 || tail_count == 0 {
        return None;
    }

    let head = lines[..head_count].concat();
    let tail = lines[lines.len() - tail_count..].concat();
    let omitted = if omitted_tail {
        "; later content omitted"
    } else {
        ""
    };
    let marker = format!(
        "...[middle truncated; shown lines 1-{head_count} and {}-{} of {} total; full output: {}{}]...",
        lines.len() - tail_count + 1,
        lines.len(),
        lines.len(),
        path.display(),
        omitted
    );

    Some(format!("{head}{marker}\n{tail}"))
}

fn build_char_preview(text: &str, path: &Path, omitted_tail: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let head_chars = INLINE_TEXT_BUDGET * 2 / 3;
    let tail_chars = INLINE_TEXT_BUDGET / 3;
    let head_end = head_chars.min(total);
    let tail_start = total.saturating_sub(tail_chars);
    let head: String = chars[..head_end].iter().collect();
    let tail: String = chars[tail_start..].iter().collect();
    let omitted = if omitted_tail {
        "; later content omitted"
    } else {
        ""
    };
    let marker = format!(
        "...[middle truncated; shown chars 1-{head_end} and {}-{} of {} total; full output: {}{}]...",
        tail_start.saturating_add(1),
        total,
        total,
        path.display(),
        omitted
    );
    format!("{head}\n{marker}\n{tail}")
}

fn build_short_preview(text: &str, path: &Path) -> String {
    let mut out = String::new();
    out.push_str(text);
    if !text.is_empty() && !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "...[full output: {}; later content omitted]...",
        path.display()
    ));
    out
}

fn ensure_nonempty_contents(contents: &mut Vec<Content>) {
    if contents.is_empty() {
        contents.push(Content::text(String::new()));
    }
}

fn collapse_image_updates(contents: Vec<WorkerContent>) -> Vec<WorkerContent> {
    let mut group_for_index: Vec<Option<usize>> = vec![None; contents.len()];
    let mut last_in_group: Vec<usize> = Vec::new();
    let mut current_group: Option<usize> = None;

    for (idx, content) in contents.iter().enumerate() {
        if let WorkerContent::ContentImage { is_new, .. } = content {
            if *is_new || current_group.is_none() {
                current_group = Some(last_in_group.len());
                last_in_group.push(idx);
            }
            let group = current_group.expect("image group should be set");
            group_for_index[idx] = Some(group);
            last_in_group[group] = idx;
        }
    }

    contents
        .into_iter()
        .enumerate()
        .filter_map(|(idx, content)| match &content {
            WorkerContent::ContentImage { .. } => match group_for_index[idx] {
                Some(group) if last_in_group.get(group).copied() == Some(idx) => Some(content),
                _ => None,
            },
            _ => Some(content),
        })
        .collect()
}

fn normalize_error_prompt(text: String, is_error: bool) -> String {
    if !is_error {
        return text;
    }
    let mut normalized = String::with_capacity(text.len());
    let mut normalized_any = false;
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("> ")
            && rest.starts_with("Error")
        {
            normalized.push_str(rest);
            normalized_any = true;
        } else {
            normalized.push_str(line);
        }
    }

    if normalized_any && !normalized.ends_with("\n> ") && !normalized.ends_with("> ") {
        if !normalized.ends_with('\n') {
            normalized.push('\n');
        }
        normalized.push_str("> ");
    }

    if normalized_any { normalized } else { text }
}

fn content_image_with_meta(data: String, mime_type: String, id: String, is_new: bool) -> Content {
    let mut meta = Meta::new();
    let image_id = normalize_plot_id(&id);
    meta.0.insert(
        "mcpConsole".to_string(),
        json!({
            "imageId": image_id,
            "isNewPage": is_new,
        }),
    );
    RawContent::Image(RawImageContent {
        data,
        mime_type,
        meta: Some(meta),
    })
    .no_annotation()
}

fn normalize_plot_id(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix("plot-") else {
        return raw.to_string();
    };
    let mut parts = rest.splitn(2, '-');
    let _pid = parts.next();
    let Some(counter) = parts.next() else {
        return raw.to_string();
    };
    if counter.chars().all(|ch| ch.is_ascii_digit()) {
        format!("plot-{counter}")
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_error_prompt;

    #[test]
    fn compact_search_cards_do_not_trigger_error_prompt_normalization() {
        let text = "[pager] search for `Error` @10\n[match] Error: boom\n".to_string();
        assert_eq!(normalize_error_prompt(text.clone(), true), text);
    }
}

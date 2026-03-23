use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rmcp::model::{AnnotateAble, CallToolResult, Content, RawContent, RawImageContent};
use tempfile::Builder;

use crate::worker_process::WorkerError;
use crate::worker_protocol::{ContentOrigin, WorkerContent, WorkerErrorCode, WorkerReply};

const INLINE_TEXT_BUDGET: usize = 3500;
const INLINE_TEXT_HARD_SPILL_THRESHOLD_NUMERATOR: usize = 5;
const INLINE_TEXT_HARD_SPILL_THRESHOLD_DENOMINATOR: usize = 4;
const INLINE_TEXT_HARD_SPILL_THRESHOLD: usize = INLINE_TEXT_BUDGET
    * INLINE_TEXT_HARD_SPILL_THRESHOLD_NUMERATOR
    / INLINE_TEXT_HARD_SPILL_THRESHOLD_DENOMINATOR;
const IMAGE_OUTPUT_BUNDLE_THRESHOLD: usize = 5;
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

type OutputStoreRootFactory = fn() -> std::io::Result<tempfile::TempDir>;

struct OutputStore {
    root: Option<tempfile::TempDir>,
    create_root: OutputStoreRootFactory,
    next_id: u64,
    total_bytes: u64,
    limits: OutputStoreLimits,
    bundles: VecDeque<StoredBundle>,
}

struct ActiveOutputBundle {
    id: u64,
    paths: OutputBundlePaths,
    next_image_number: usize,
    current_image_history_number: usize,
    history_image_count: usize,
    transcript_bytes: usize,
    transcript_lines: usize,
    transcript_has_partial_line: bool,
    omitted_tail: bool,
    omission_recorded: bool,
    pre_index_image_paths: Vec<String>,
    disclosed: bool,
}

struct BundleAppendResult {
    retained_items: Vec<ReplyItem>,
    omitted_this_reply: bool,
}

#[derive(Clone)]
struct OutputBundlePaths {
    dir: PathBuf,
    transcript: PathBuf,
    events_log: PathBuf,
    images_dir: PathBuf,
    images_history_dir: PathBuf,
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
    is_new: bool,
}

struct ReplyMaterial {
    inline_items: Vec<ReplyItem>,
    bundle_items: Vec<ReplyItem>,
    worker_text: String,
    is_error: bool,
    error_code: Option<WorkerErrorCode>,
    bundle_image_count: usize,
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
        reuse_active_timeout_bundle: bool,
    ) -> CallToolResult {
        match result {
            Ok(reply) => {
                self.finalize_reply(reply, pending_request_after, reuse_active_timeout_bundle)
            }
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
        reuse_active_timeout_bundle: bool,
    ) -> CallToolResult {
        let material = prepare_reply_material(reply);
        let mut active_timeout_bundle = self.active_timeout_bundle.take();
        if !reuse_active_timeout_bundle
            && let Some(active) = active_timeout_bundle.take()
            && let Err(err) = self.finish_timeout_bundle(active)
        {
            eprintln!("dropping closed timeout bundle after output-bundle error: {err}");
        }
        if material.error_code == Some(WorkerErrorCode::Timeout) && active_timeout_bundle.is_none()
        {
            match self.output_store.new_bundle() {
                Ok(bundle) => active_timeout_bundle = Some(bundle),
                Err(err) => {
                    eprintln!("dropping timeout bundle setup after output-bundle error: {err}");
                }
            }
        }

        let contents = if let Some(mut active) = active_timeout_bundle {
            match active.append_items(&mut self.output_store, &material.bundle_items) {
                Ok(append) => {
                    let retained_worker_text = worker_text_from_items(&append.retained_items);
                    let contents = if append.omitted_this_reply {
                        active.disclosed = true;
                        compact_text_bundle_items(
                            append.retained_items.clone(),
                            &retained_worker_text,
                            &active,
                        )
                    } else if active.next_image_number > 0
                        && should_use_output_bundle(
                            active.history_image_count,
                            material.worker_text.chars().count(),
                        )
                    {
                        active.disclosed = true;
                        compact_output_bundle_items(&append.retained_items, &active)
                    } else if text_should_spill(material.worker_text.chars().count()) {
                        active.disclosed = true;
                        compact_text_bundle_items(
                            append.retained_items.clone(),
                            &retained_worker_text,
                            &active,
                        )
                    } else {
                        materialize_items(material.inline_items.clone())
                    };
                    if pending_request_after {
                        self.active_timeout_bundle = Some(active);
                    } else if let Err(err) = self.finish_timeout_bundle(active) {
                        eprintln!(
                            "dropping closed timeout bundle after output-bundle error: {err}"
                        );
                    }
                    contents
                }
                Err(err) => {
                    eprintln!("dropping timeout bundle content after output-bundle error: {err}");
                    if let Err(err) = self.finish_timeout_bundle(active) {
                        eprintln!(
                            "dropping closed timeout bundle after output-bundle error: {err}"
                        );
                    }
                    compact_without_output_bundle(&material)
                }
            }
        } else if material.bundle_image_count > 0
            && should_use_output_bundle(
                material.bundle_image_count,
                material.worker_text.chars().count(),
            )
        {
            match self.output_store.new_bundle() {
                Ok(mut bundle) => {
                    match bundle.append_items(&mut self.output_store, &material.bundle_items) {
                        Ok(append) => compact_output_bundle_items(&append.retained_items, &bundle),
                        Err(err) => {
                            eprintln!(
                                "dropping output-bundled content after output-bundle error: {err}"
                            );
                            compact_without_output_bundle(&material)
                        }
                    }
                }
                Err(err) => {
                    eprintln!("dropping output-bundle setup after output-bundle error: {err}");
                    compact_without_output_bundle(&material)
                }
            }
        } else if text_should_spill(material.worker_text.chars().count()) {
            match self.output_store.new_bundle() {
                Ok(mut bundle) => {
                    match bundle.append_items(&mut self.output_store, &material.bundle_items) {
                        Ok(append) => {
                            let retained_worker_text =
                                worker_text_from_items(&append.retained_items);
                            compact_text_bundle_items(
                                append.retained_items,
                                &retained_worker_text,
                                &bundle,
                            )
                        }
                        Err(err) => {
                            eprintln!(
                                "dropping output-bundled content after output-bundle error: {err}"
                            );
                            compact_without_output_bundle(&material)
                        }
                    }
                }
                Err(err) => {
                    eprintln!("dropping output-bundle setup after output-bundle error: {err}");
                    compact_without_output_bundle(&material)
                }
            }
        } else {
            materialize_items(material.inline_items)
        };

        finalize_batch(contents, material.is_error)
    }
}

impl OutputStore {
    fn new() -> Result<Self, WorkerError> {
        let limits = OutputStoreLimits::from_env()?;
        Ok(Self {
            root: None,
            create_root: create_output_store_root,
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

    fn ensure_root_path(&mut self) -> Result<&Path, WorkerError> {
        if self.root.is_none() {
            self.root = Some((self.create_root)().map_err(WorkerError::Io)?);
        }
        Ok(self
            .root
            .as_ref()
            .expect("output store root should exist")
            .path())
    }

    fn new_bundle(&mut self) -> Result<ActiveOutputBundle, WorkerError> {
        self.prune_for_new_bundle(0)?;
        self.next_id = self.next_id.saturating_add(1);
        let bundle_id = self.next_id;
        let root_path = self.ensure_root_path()?.to_path_buf();
        let dir = root_path.join(format!("output-{bundle_id:04}"));
        fs::create_dir_all(&dir).map_err(WorkerError::Io)?;
        let images_dir = dir.join("images");
        let images_history_dir = images_dir.join("history");
        let transcript = dir.join("transcript.txt");
        let events_log = dir.join("events.log");
        self.bundles.push_back(StoredBundle {
            id: bundle_id,
            dir: dir.clone(),
            bytes_on_disk: 0,
        });
        Ok(ActiveOutputBundle {
            id: bundle_id,
            paths: OutputBundlePaths {
                dir,
                transcript,
                events_log,
                images_dir,
                images_history_dir,
            },
            next_image_number: 0,
            current_image_history_number: 0,
            history_image_count: 0,
            transcript_bytes: 0,
            transcript_lines: 0,
            transcript_has_partial_line: false,
            omitted_tail: false,
            omission_recorded: false,
            pre_index_image_paths: Vec::new(),
            disclosed: false,
        })
    }

    fn remove_bundle(&mut self, bundle_id: u64) -> Result<(), WorkerError> {
        let Some(index) = self
            .bundles
            .iter()
            .position(|bundle| bundle.id == bundle_id)
        else {
            return Ok(());
        };
        self.remove_bundle_at(index)
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

    fn record_file_replace(&mut self, bundle_id: u64, old_bytes: u64, new_bytes: u64) {
        if old_bytes == new_bytes {
            return;
        }
        let bundle = self
            .bundles
            .iter_mut()
            .find(|bundle| bundle.id == bundle_id)
            .expect("bundle metadata should exist for file replacement");
        if new_bytes > old_bytes {
            let delta = new_bytes - old_bytes;
            bundle.bytes_on_disk = bundle.bytes_on_disk.saturating_add(delta);
            self.total_bytes = self.total_bytes.saturating_add(delta);
        } else {
            let delta = old_bytes - new_bytes;
            bundle.bytes_on_disk = bundle.bytes_on_disk.saturating_sub(delta);
            self.total_bytes = self.total_bytes.saturating_sub(delta);
        }
    }

    fn record_file_removal(&mut self, bundle_id: u64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let bundle = self
            .bundles
            .iter_mut()
            .find(|bundle| bundle.id == bundle_id)
            .expect("bundle metadata should exist for file removal");
        bundle.bytes_on_disk = bundle.bytes_on_disk.saturating_sub(bytes);
        self.total_bytes = self.total_bytes.saturating_sub(bytes);
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
        self.remove_bundle_at(index)?;
        Ok(true)
    }

    fn remove_bundle_at(&mut self, index: usize) -> Result<(), WorkerError> {
        let bundle = self
            .bundles
            .remove(index)
            .expect("bundle index should exist");
        fs::remove_dir_all(&bundle.dir).map_err(WorkerError::Io)?;
        self.total_bytes = self.total_bytes.saturating_sub(bundle.bytes_on_disk);
        Ok(())
    }
}

fn create_output_store_root() -> std::io::Result<tempfile::TempDir> {
    Builder::new().prefix("mcp-repl-output-").tempdir()
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
        Ok(Self {
            max_bundle_count,
            max_bundle_bytes,
            max_total_bytes,
        })
    }
}

impl ActiveOutputBundle {
    fn was_disclosed(&self) -> bool {
        self.disclosed
    }

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
        if self.has_images() && !self.has_events_log() {
            self.materialize_events_log(store)?;
        }
        self.ensure_transcript(store)?;
        let start_byte = self.transcript_bytes;
        let omission_reserve = if self.omission_recorded {
            0
        } else {
            usize::from(self.has_events_log()) * omission_event_line_len()
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
            let (start_line, end_line, next_line_count, next_has_partial_line) =
                append_text_line_span(
                    retained,
                    self.transcript_lines,
                    self.transcript_has_partial_line,
                );
            let end_byte = start_byte.saturating_add(retained.len());
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
                if self.has_events_log() {
                    store.append_bundle_bytes(self.id, &self.paths.events_log, row.as_bytes())?;
                }
                self.transcript_bytes = self.transcript_bytes.saturating_add(retained.len());
                self.transcript_lines = next_line_count;
                self.transcript_has_partial_line = next_has_partial_line;
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
        if !self.has_events_log() {
            return Ok(Some(ReplyItem::ServerText(text.to_string())));
        }
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
        self.ensure_images_dir()?;
        if self.has_text() && !self.has_events_log() {
            self.materialize_events_log(store)?;
        }
        let extension = image_extension(&image.mime_type);
        let starts_new_image = image.is_new || self.next_image_number == 0;
        let image_number = if starts_new_image {
            self.next_image_number.saturating_add(1)
        } else {
            self.next_image_number
        };
        let history_number = if starts_new_image {
            1
        } else {
            self.current_image_history_number.saturating_add(1)
        };
        let history_rel_path =
            format!("images/history/{image_number:03}/{history_number:03}.{extension}");
        let history_path = self
            .paths
            .images_history_dir
            .join(format!("{image_number:03}/{history_number:03}.{extension}"));
        let alias_path = self
            .paths
            .images_dir
            .join(format!("{image_number:03}.{extension}"));
        let bytes = STANDARD
            .decode(image.data.as_bytes())
            .map_err(|err| WorkerError::Protocol(format!("invalid image data: {err}")))?;
        let alias_old_path = self.existing_image_alias_path(image_number);
        let alias_old_len = alias_old_path
            .as_ref()
            .and_then(|path| fs::metadata(path).ok())
            .map_or(0, |metadata| metadata.len());
        let alias_growth = (bytes.len() as u64).saturating_sub(alias_old_len);
        let row = format!("I {history_rel_path}\n");
        let required = bytes.len() as u64 + row.len() as u64 + alias_growth;
        let granted = store.prepare_append_capacity(self.id, required)?;
        if granted < required {
            return Ok(None);
        }
        let history_parent = history_path
            .parent()
            .expect("history file should have a parent directory");
        fs::create_dir_all(history_parent).map_err(WorkerError::Io)?;
        fs::write(&history_path, &bytes).map_err(WorkerError::Io)?;
        store.record_append(self.id, bytes.len() as u64);
        if let Some(old_path) = alias_old_path.as_ref()
            && old_path != &alias_path
        {
            fs::remove_file(old_path).map_err(WorkerError::Io)?;
            store.record_file_removal(self.id, alias_old_len);
        }
        let replace_old_len = if alias_old_path.as_ref() == Some(&alias_path) {
            alias_old_len
        } else {
            0
        };
        fs::write(&alias_path, &bytes).map_err(WorkerError::Io)?;
        store.record_file_replace(self.id, replace_old_len, bytes.len() as u64);
        if self.has_events_log() {
            store.append_bundle_bytes(self.id, &self.paths.events_log, row.as_bytes())?;
        } else {
            self.pre_index_image_paths.push(history_rel_path);
        }
        self.next_image_number = image_number;
        self.current_image_history_number = history_number;
        self.history_image_count = self.history_image_count.saturating_add(1);
        Ok(Some(ReplyItem::Image(image.clone())))
    }

    fn apply_omission(&mut self, store: &mut OutputStore) -> Result<(), WorkerError> {
        self.omitted_tail = true;
        if self.omission_recorded || !self.has_events_log() {
            return Ok(());
        }
        let _ = self.append_events_log_text(store, OUTPUT_BUNDLE_OMITTED_NOTICE)?;
        self.omission_recorded = true;
        Ok(())
    }

    fn ensure_transcript(&self, _store: &mut OutputStore) -> Result<(), WorkerError> {
        if self.paths.transcript.exists() {
            return Ok(());
        }
        std::fs::File::create(&self.paths.transcript).map_err(WorkerError::Io)?;
        Ok(())
    }

    fn ensure_images_dir(&self) -> Result<(), WorkerError> {
        if self.paths.images_dir.exists() {
            return Ok(());
        }
        fs::create_dir_all(&self.paths.images_dir).map_err(WorkerError::Io)
    }

    fn materialize_events_log(&mut self, store: &mut OutputStore) -> Result<(), WorkerError> {
        if self.has_events_log() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(OUTPUT_BUNDLE_HEADER);
        if self.has_text() {
            bytes.extend_from_slice(self.backfill_text_row().as_bytes());
        } else {
            for image_path in &self.pre_index_image_paths {
                bytes.extend_from_slice(format!("I {image_path}\n").as_bytes());
            }
        }
        if self.omitted_tail {
            bytes.extend_from_slice(
                build_events_log_server_line(OUTPUT_BUNDLE_OMITTED_NOTICE).as_bytes(),
            );
            self.omission_recorded = true;
        }
        let granted = store.prepare_append_capacity(self.id, bytes.len() as u64)?;
        if granted < bytes.len() as u64 {
            return Err(WorkerError::Protocol(
                "output bundle could not materialize events.log within quota".to_string(),
            ));
        }
        std::fs::File::create(&self.paths.events_log).map_err(WorkerError::Io)?;
        store.append_bundle_bytes(self.id, &self.paths.events_log, &bytes)?;
        self.pre_index_image_paths.clear();
        Ok(())
    }

    fn has_text(&self) -> bool {
        self.transcript_bytes > 0
    }

    fn has_images(&self) -> bool {
        self.next_image_number > 0
    }

    fn has_events_log(&self) -> bool {
        self.paths.events_log.exists()
    }

    fn backfill_text_row(&self) -> String {
        format!(
            "T lines=1-{} bytes=0-{}\n",
            self.transcript_lines.max(1),
            self.transcript_bytes
        )
    }

    fn disclosure_path(&self) -> &Path {
        if self.has_events_log() {
            &self.paths.events_log
        } else if self.has_text() {
            &self.paths.transcript
        } else if self.has_images() {
            &self.paths.images_dir
        } else {
            &self.paths.dir
        }
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

    fn existing_image_alias_path(&self, index: usize) -> Option<PathBuf> {
        let stem = format!("{index:03}");
        for extension in ["png", "jpg", "jpeg", "gif", "webp", "svg"] {
            let path = self.paths.images_dir.join(format!("{stem}.{extension}"));
            if path.exists() {
                return Some(path);
            }
        }
        None
    }
}

impl ResponseState {
    fn finish_timeout_bundle(&mut self, active: ActiveOutputBundle) -> Result<(), WorkerError> {
        if active.was_disclosed() {
            return Ok(());
        }
        self.output_store.remove_bundle(active.id)
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

    let mut bundle_items = Vec::with_capacity(contents.len());
    let mut worker_text = String::new();

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
                        bundle_items.push(ReplyItem::WorkerText(text));
                    }
                    ContentOrigin::Server => bundle_items.push(ReplyItem::ServerText(text)),
                }
            }
            WorkerContent::ContentImage {
                data,
                mime_type,
                id: _,
                is_new,
            } => bundle_items.push(ReplyItem::Image(ReplyImage {
                data,
                mime_type,
                is_new,
            })),
        }
    }

    let inline_items = collapse_image_updates(bundle_items.clone());
    let bundle_image_count = count_images(&bundle_items);

    ReplyMaterial {
        inline_items,
        bundle_items,
        worker_text,
        is_error,
        error_code,
        bundle_image_count,
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
    content_image(image.data.clone(), image.mime_type.clone())
}

fn count_images(items: &[ReplyItem]) -> usize {
    items
        .iter()
        .filter(|item| matches!(item, ReplyItem::Image(_)))
        .count()
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
    let preview = build_preview(
        worker_text,
        Some(bundle.disclosure_path()),
        bundle.omitted_tail,
    );
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

fn compact_text_without_bundle_items(items: Vec<ReplyItem>, worker_text: &str) -> Vec<Content> {
    let preview = build_preview(worker_text, None, false);
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
    let first_anchor = (bundle.next_image_number > 0)
        .then(|| load_output_bundle_image_content(bundle, 1))
        .flatten();
    let last_anchor = (bundle.next_image_number > 1)
        .then(|| load_output_bundle_image_content(bundle, bundle.next_image_number))
        .flatten();
    let displayed_anchor_count =
        usize::from(first_anchor.is_some()) + usize::from(last_anchor.is_some());

    let head_text = collect_prefix_text(
        items,
        first_image_idx.unwrap_or(items.len()),
        HEAD_TEXT_BUDGET,
    );
    if !head_text.is_empty() {
        out.push(Content::text(head_text));
    }
    if let Some(image) = first_anchor {
        out.push(image);
    }
    out.push(Content::text(build_output_bundle_notice(
        bundle,
        displayed_anchor_count,
    )));
    let pre_last_text = collect_suffix_text_before(items, last_image_idx, PRE_LAST_TEXT_BUDGET);
    if !pre_last_text.is_empty() {
        out.push(Content::text(pre_last_text));
    }
    if let Some(image) = last_anchor {
        out.push(image);
    }
    let post_last_text = collect_prefix_text_after(items, last_image_idx, POST_LAST_TEXT_BUDGET);
    if !post_last_text.is_empty() {
        out.push(Content::text(post_last_text));
    }
    out
}

fn compact_output_without_bundle_items(items: &[ReplyItem]) -> Vec<Content> {
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
    if let Some(index) = first_image_idx
        && let ReplyItem::Image(image) = &items[index]
    {
        out.push(image_to_content(image));
    }
    out.push(Content::text(build_output_bundle_unavailable_notice(
        count_images(items),
    )));
    let pre_last_text = collect_suffix_text_before(items, last_image_idx, PRE_LAST_TEXT_BUDGET);
    if !pre_last_text.is_empty() {
        out.push(Content::text(pre_last_text));
    }
    if let Some(index) = last_image_idx
        && Some(index) != first_image_idx
        && let ReplyItem::Image(image) = &items[index]
    {
        out.push(image_to_content(image));
    }
    let post_last_text = collect_prefix_text_after(items, last_image_idx, POST_LAST_TEXT_BUDGET);
    if !post_last_text.is_empty() {
        out.push(Content::text(post_last_text));
    }
    out
}

fn compact_without_output_bundle(material: &ReplyMaterial) -> Vec<Content> {
    if material.bundle_image_count > 0
        && should_use_output_bundle(
            material.bundle_image_count,
            material.worker_text.chars().count(),
        )
    {
        return compact_output_without_bundle_items(&material.inline_items);
    }
    compact_text_without_bundle_items(material.inline_items.clone(), &material.worker_text)
}

fn should_use_output_bundle(image_count: usize, worker_text_chars: usize) -> bool {
    image_count >= IMAGE_OUTPUT_BUNDLE_THRESHOLD || text_should_spill(worker_text_chars)
}

fn text_should_spill(worker_text_chars: usize) -> bool {
    worker_text_chars > INLINE_TEXT_HARD_SPILL_THRESHOLD
}

fn build_output_bundle_notice(
    bundle: &ActiveOutputBundle,
    displayed_anchor_count: usize,
) -> String {
    let omitted = if bundle.omitted_tail {
        "; later content omitted"
    } else {
        ""
    };
    let path = bundle.disclosure_path();
    let label = if bundle.has_events_log() {
        "ordered output bundle index"
    } else if bundle.has_images() && !bundle.has_text() {
        "output bundle images"
    } else {
        "full output"
    };
    match displayed_anchor_count {
        0 => format!(
            "...[middle truncated; {label}: {}{}]...",
            path.display(),
            omitted
        ),
        1 if bundle.next_image_number <= 1 => format!(
            "...[middle truncated; first image shown inline; {label}: {}{}]...",
            path.display(),
            omitted
        ),
        1 => format!(
            "...[middle truncated; one image shown inline; {label}: {}{}]...",
            path.display(),
            omitted
        ),
        _ => format!(
            "...[middle truncated; first and last images shown inline; {label}: {}{}]...",
            path.display(),
            omitted
        ),
    }
}

fn build_output_bundle_unavailable_notice(image_count: usize) -> String {
    match image_count {
        0 => "...[middle truncated; output bundle unavailable]...".to_string(),
        1 => "...[middle truncated; first image shown inline; output bundle unavailable]..."
            .to_string(),
        _ => "...[middle truncated; first and last images shown inline; output bundle unavailable]..."
            .to_string(),
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
    let Some(index) = index else {
        return String::new();
    };
    let start = index.saturating_add(1);
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

fn append_text_line_span(
    text: &str,
    transcript_lines: usize,
    transcript_has_partial_line: bool,
) -> (usize, usize, usize, bool) {
    assert!(!text.is_empty(), "text line spans require non-empty text");
    let newline_count = text.bytes().filter(|byte| *byte == b'\n').count();
    let start_line = if transcript_lines == 0 {
        1
    } else if transcript_has_partial_line {
        transcript_lines
    } else {
        transcript_lines.saturating_add(1)
    };
    let next_line_count = if transcript_has_partial_line {
        transcript_lines
            .saturating_add(newline_count)
            .saturating_add(usize::from(!text.ends_with('\n')))
            .saturating_sub(1)
    } else {
        transcript_lines
            .saturating_add(newline_count)
            .saturating_add(usize::from(!text.ends_with('\n')))
    }
    .max(start_line);
    (
        start_line,
        next_line_count,
        next_line_count,
        !text.ends_with('\n'),
    )
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

fn load_output_bundle_image_content(bundle: &ActiveOutputBundle, index: usize) -> Option<Content> {
    let path = bundle.image_path(index);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!(
                "skipping unreadable output bundle image {}: {err}",
                path.display()
            );
            return None;
        }
    };
    let mime_type = mime_type_from_path(&path);
    let data = STANDARD.encode(bytes);
    Some(content_image(data, mime_type))
}

fn build_preview(text: &str, path: Option<&Path>, omitted_tail: bool) -> String {
    if omitted_tail && text.chars().count() <= INLINE_TEXT_BUDGET {
        return build_short_preview(text, path);
    }
    if let Some(preview) = build_line_preview(text, path, omitted_tail) {
        return preview;
    }
    build_char_preview(text, path, omitted_tail)
}

fn build_line_preview(text: &str, path: Option<&Path>, omitted_tail: bool) -> Option<String> {
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
    let storage = preview_storage_clause(path);
    let marker = format!(
        "...[middle truncated; shown lines 1-{head_count} and {}-{} of {} total; {storage}{omitted}]...",
        lines.len() - tail_count + 1,
        lines.len(),
        lines.len(),
    );

    Some(format!("{head}{marker}\n{tail}"))
}

fn build_char_preview(text: &str, path: Option<&Path>, omitted_tail: bool) -> String {
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
    let storage = preview_storage_clause(path);
    let marker = format!(
        "...[middle truncated; shown chars 1-{head_end} and {}-{} of {} total; {storage}{omitted}]...",
        tail_start.saturating_add(1),
        total,
        total,
    );
    format!("{head}\n{marker}\n{tail}")
}

fn build_short_preview(text: &str, path: Option<&Path>) -> String {
    let mut out = String::new();
    out.push_str(text);
    if !text.is_empty() && !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "...[{}; later content omitted]...",
        preview_storage_clause(path)
    ));
    out
}

fn preview_storage_clause(path: Option<&Path>) -> String {
    match path {
        Some(path) => format!("full output: {}", path.display()),
        None => "output bundle unavailable".to_string(),
    }
}

fn ensure_nonempty_contents(contents: &mut Vec<Content>) {
    if contents.is_empty() {
        contents.push(Content::text(String::new()));
    }
}

fn collapse_image_updates(items: Vec<ReplyItem>) -> Vec<ReplyItem> {
    let mut group_for_index: Vec<Option<usize>> = vec![None; items.len()];
    let mut last_in_group: Vec<usize> = Vec::new();
    let mut current_group: Option<usize> = None;

    for (idx, item) in items.iter().enumerate() {
        if let ReplyItem::Image(image) = item {
            if image.is_new || current_group.is_none() {
                current_group = Some(last_in_group.len());
                last_in_group.push(idx);
            }
            let group = current_group.expect("image group should be set");
            group_for_index[idx] = Some(group);
            last_in_group[group] = idx;
        }
    }

    items
        .into_iter()
        .enumerate()
        .filter_map(|(idx, item)| match &item {
            ReplyItem::Image(_) => match group_for_index[idx] {
                Some(group) if last_in_group.get(group).copied() == Some(idx) => Some(item),
                _ => None,
            },
            _ => Some(item),
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

fn content_image(data: String, mime_type: String) -> Content {
    RawContent::Image(RawImageContent {
        data,
        mime_type,
        meta: None,
    })
    .no_annotation()
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use std::io;

    use rmcp::model::RawContent;

    use super::{OutputStore, ReplyImage, ReplyItem, ResponseState, normalize_error_prompt};
    use crate::worker_protocol::WorkerContent;

    fn result_text(result: &rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|item| match &item.raw {
                RawContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn fail_output_store_root_creation() -> io::Result<tempfile::TempDir> {
        Err(io::Error::other("simulated tempdir failure"))
    }

    #[test]
    fn compact_search_cards_do_not_trigger_error_prompt_normalization() {
        let text = "[pager] search for `Error` @10\n[match] Error: boom\n".to_string();
        assert_eq!(normalize_error_prompt(text.clone(), true), text);
    }

    #[test]
    fn events_log_text_rows_preserve_partial_line_state_across_images() {
        let mut store = OutputStore::new().expect("output store should initialize");
        let mut bundle = store.new_bundle().expect("bundle should initialize");

        let first = bundle
            .append_worker_text(&mut store, "a")
            .expect("first worker text should append");
        assert!(matches!(first, Some(ReplyItem::WorkerText(text)) if text == "a"));

        let image = ReplyImage {
            data: base64::engine::general_purpose::STANDARD.encode([0_u8]),
            mime_type: "image/png".to_string(),
            is_new: true,
        };
        let retained_image = bundle
            .append_image(&mut store, &image)
            .expect("image should append");
        assert!(matches!(retained_image, Some(ReplyItem::Image(_))));

        let second = bundle
            .append_worker_text(&mut store, "b\n")
            .expect("second worker text should append");
        assert!(matches!(second, Some(ReplyItem::WorkerText(text)) if text == "b\n"));

        let transcript = std::fs::read_to_string(&bundle.paths.transcript)
            .expect("transcript should be readable");
        let events = std::fs::read_to_string(&bundle.paths.events_log)
            .expect("events log should be readable");

        assert_eq!(transcript, "ab\n");
        assert!(
            events.contains("T lines=1-1 bytes=0-1\n"),
            "expected first text row to cover the initial partial line, got: {events:?}"
        );
        assert!(
            events.contains("T lines=1-1 bytes=1-3\n"),
            "expected text after the image to continue the same line, got: {events:?}"
        );
    }

    #[test]
    fn output_bundle_setup_failure_returns_pathless_truncated_reply() {
        let mut state = ResponseState::new().expect("response state should initialize");
        state.output_store.create_root = fail_output_store_root_creation;

        let oversized_text = format!(
            "START{}END",
            "a".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let result = state.finalize_worker_result(
            Ok(crate::worker_protocol::WorkerReply::Output {
                contents: vec![WorkerContent::worker_stdout(oversized_text.clone())],
                is_error: false,
                error_code: None,
                prompt: None,
                prompt_variants: None,
            }),
            false,
            false,
        );

        let text = result_text(&result);
        assert!(
            text.contains("output bundle unavailable"),
            "expected truncated fallback notice when bundle setup fails, got: {text:?}"
        );
        assert!(
            !text.contains("/transcript.txt") && !text.contains("/events.log"),
            "did not expect bundle path in fallback reply, got: {text:?}"
        );
        assert!(
            text.contains("START") && text.contains("END"),
            "expected truncated fallback reply to preserve worker output preview, got: {text:?}"
        );
    }
}

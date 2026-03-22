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

pub(crate) struct ResponseState {
    output_store: OutputStore,
    active_timeout_transcript: Option<ActiveTranscript>,
    active_timeout_bundle: Option<ActiveOutputBundle>,
}

struct OutputStore {
    root: tempfile::TempDir,
    next_id: u64,
}

#[derive(Clone)]
struct ActiveTranscript {
    path: PathBuf,
}

struct ActiveOutputBundle {
    paths: OutputBundlePaths,
    next_image_number: usize,
    transcript_bytes: usize,
    transcript_lines: usize,
}

#[derive(Clone)]
struct OutputBundlePaths {
    transcript: PathBuf,
    events_log: PathBuf,
    images_dir: PathBuf,
}

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
            active_timeout_transcript: None,
            active_timeout_bundle: None,
        })
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
            && material.image_count == 0
            && self.active_timeout_bundle.is_none()
            && self.active_timeout_transcript.is_none()
        {
            let path = self
                .output_store
                .new_transcript_path()
                .expect("failed to create timeout transcript path");
            self.active_timeout_transcript = Some(ActiveTranscript { path });
        }

        if material.error_code == Some(WorkerErrorCode::Timeout)
            && material.image_count > 0
            && self.active_timeout_bundle.is_none()
        {
            let bundle = self
                .output_store
                .new_bundle()
                .expect("failed to create timeout output bundle");
            self.active_timeout_bundle = Some(bundle);
        }

        if material.image_count > 0
            && self.active_timeout_bundle.is_none()
            && self.active_timeout_transcript.is_some()
        {
            let active = self
                .active_timeout_transcript
                .take()
                .expect("active timeout transcript should exist");
            let bundle = self
                .output_store
                .new_bundle_from_transcript(&active.path)
                .expect("failed to backfill timeout output bundle from transcript");
            self.active_timeout_bundle = Some(bundle);
        }

        if let Some(active) = self.active_timeout_bundle.as_mut() {
            active
                .append_items(&material.items)
                .expect("failed to append timeout output bundle");
        } else if !material.worker_text.is_empty()
            && let Some(active) = self.active_timeout_transcript.as_ref()
        {
            self.output_store
                .append(&active.path, &material.worker_text)
                .expect("failed to append timeout transcript");
        }

        let contents = if let Some(active) = self.active_timeout_bundle.as_ref() {
            if should_use_output_bundle(
                material.image_count.max(active.next_image_number),
                material.estimated_cost,
            ) {
                compact_output_bundle_items(&material.items, active)
            } else {
                materialize_items(material.items)
            }
        } else if material.image_count > 0
            && should_use_output_bundle(material.image_count, material.estimated_cost)
        {
            let mut bundle = self
                .output_store
                .new_bundle()
                .expect("failed to create output bundle");
            bundle
                .append_items(&material.items)
                .expect("failed to append output bundle");
            compact_output_bundle_items(&material.items, &bundle)
        } else if material.worker_text.chars().count() > INLINE_TEXT_BUDGET {
            let path = match self.active_timeout_transcript.as_ref() {
                Some(active) => active.path.clone(),
                None => {
                    let path = self
                        .output_store
                        .new_transcript_path()
                        .expect("failed to create transcript path");
                    self.output_store
                        .append(&path, &material.worker_text)
                        .expect("failed to append transcript");
                    path
                }
            };
            compact_items(material.items, &material.worker_text, &path)
        } else {
            materialize_items(material.items)
        };

        if !pending_request_after {
            self.active_timeout_transcript = None;
            self.active_timeout_bundle = None;
        }

        finalize_batch(contents, material.is_error)
    }
}

impl OutputStore {
    fn new() -> Result<Self, WorkerError> {
        let root = Builder::new()
            .prefix("mcp-repl-output-")
            .tempdir()
            .map_err(WorkerError::Io)?;
        Ok(Self { root, next_id: 0 })
    }

    /// Allocates a stable absolute path for the next transcript file under the server-owned temp
    /// root.
    fn new_transcript_path(&mut self) -> Result<PathBuf, WorkerError> {
        self.next_id = self.next_id.saturating_add(1);
        let path = self
            .root
            .path()
            .join(format!("output-{:04}.txt", self.next_id));
        std::fs::File::create(&path).map_err(WorkerError::Io)?;
        Ok(path)
    }

    fn new_bundle(&mut self) -> Result<ActiveOutputBundle, WorkerError> {
        self.next_id = self.next_id.saturating_add(1);
        let dir = self.root.path().join(format!("output-{:04}", self.next_id));
        let images_dir = dir.join("images");
        fs::create_dir_all(&images_dir).map_err(WorkerError::Io)?;
        let transcript = dir.join("transcript.txt");
        let events_log = dir.join("events.log");
        std::fs::File::create(&transcript).map_err(WorkerError::Io)?;
        let mut events = std::fs::File::create(&events_log).map_err(WorkerError::Io)?;
        events
            .write_all(b"v1\ntext transcript.txt\nimages images/\n")
            .map_err(WorkerError::Io)?;
        Ok(ActiveOutputBundle {
            paths: OutputBundlePaths {
                transcript,
                events_log,
                images_dir,
            },
            next_image_number: 0,
            transcript_bytes: 0,
            transcript_lines: 0,
        })
    }

    fn new_bundle_from_transcript(
        &mut self,
        transcript_path: &Path,
    ) -> Result<ActiveOutputBundle, WorkerError> {
        let mut bundle = self.new_bundle()?;
        let existing = fs::read_to_string(transcript_path).map_err(WorkerError::Io)?;
        if !existing.is_empty() {
            bundle.append_worker_text(&existing)?;
        }
        Ok(bundle)
    }

    /// Appends worker-originated text exactly as surfaced to the client, without server-only
    /// status markers such as timeout notices.
    fn append(&self, path: &Path, text: &str) -> Result<(), WorkerError> {
        if text.is_empty() {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(WorkerError::Io)?;
        file.write_all(text.as_bytes()).map_err(WorkerError::Io)?;
        Ok(())
    }
}

impl ActiveOutputBundle {
    fn append_items(&mut self, items: &[ReplyItem]) -> Result<(), WorkerError> {
        for item in items {
            match item {
                ReplyItem::WorkerText(text) => self.append_worker_text(text)?,
                ReplyItem::ServerText(text) => self.append_server_text(text)?,
                ReplyItem::Image(image) => self.append_image(image)?,
            }
        }
        Ok(())
    }

    fn append_worker_text(&mut self, text: &str) -> Result<(), WorkerError> {
        if text.is_empty() {
            return Ok(());
        }
        let start_byte = self.transcript_bytes;
        let start_line = self.transcript_lines.saturating_add(1);
        let byte_len = text.len();
        let line_len = count_lines(text);
        OutputStore::append_impl(&self.paths.transcript, text.as_bytes())?;
        self.transcript_bytes = self.transcript_bytes.saturating_add(byte_len);
        self.transcript_lines = self.transcript_lines.saturating_add(line_len);
        let end_byte = self.transcript_bytes;
        let end_line = self.transcript_lines.max(start_line);
        let line = format!("T lines={start_line}-{end_line} bytes={start_byte}-{end_byte}\n");
        OutputStore::append_impl(&self.paths.events_log, line.as_bytes())
    }

    fn append_server_text(&mut self, text: &str) -> Result<(), WorkerError> {
        if text.is_empty() {
            return Ok(());
        }
        let escaped =
            serde_json::to_string(text).unwrap_or_else(|_| "\"<server_text>\"".to_string());
        let line = format!("S {escaped}\n");
        OutputStore::append_impl(&self.paths.events_log, line.as_bytes())
    }

    fn append_image(&mut self, image: &ReplyImage) -> Result<(), WorkerError> {
        self.next_image_number = self.next_image_number.saturating_add(1);
        let extension = image_extension(&image.mime_type);
        let file_name = format!("{:03}.{extension}", self.next_image_number);
        let path = self.paths.images_dir.join(&file_name);
        let bytes = STANDARD
            .decode(image.data.as_bytes())
            .map_err(|err| WorkerError::Protocol(format!("invalid image data: {err}")))?;
        fs::write(&path, bytes).map_err(WorkerError::Io)?;
        let line = format!("I images/{file_name}\n");
        OutputStore::append_impl(&self.paths.events_log, line.as_bytes())
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

impl OutputStore {
    fn append_impl(path: &Path, bytes: &[u8]) -> Result<(), WorkerError> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(WorkerError::Io)?;
        file.write_all(bytes).map_err(WorkerError::Io)?;
        Ok(())
    }
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

fn compact_items(items: Vec<ReplyItem>, worker_text: &str, path: &Path) -> Vec<Content> {
    let preview = build_preview(worker_text, path);
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
    match bundle.next_image_number {
        0 => format!(
            "...[middle truncated; ordered output bundle index: {}]...",
            bundle.paths.events_log.display()
        ),
        1 => format!(
            "...[middle truncated; first image shown inline; ordered output bundle index: {}]...",
            bundle.paths.events_log.display()
        ),
        _ => format!(
            "...[middle truncated; first and last images shown inline; ordered output bundle index: {}]...",
            bundle.paths.events_log.display()
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

fn build_preview(text: &str, path: &Path) -> String {
    if let Some(preview) = build_line_preview(text, path) {
        return preview;
    }
    build_char_preview(text, path)
}

fn build_line_preview(text: &str, path: &Path) -> Option<String> {
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
    let marker = format!(
        "...[middle truncated; shown lines 1-{head_count} and {}-{} of {} total; full output: {}]...",
        lines.len() - tail_count + 1,
        lines.len(),
        lines.len(),
        path.display()
    );

    Some(format!("{head}{marker}\n{tail}"))
}

fn build_char_preview(text: &str, path: &Path) -> String {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let head_chars = INLINE_TEXT_BUDGET * 2 / 3;
    let tail_chars = INLINE_TEXT_BUDGET / 3;
    let head_end = head_chars.min(total);
    let tail_start = total.saturating_sub(tail_chars);
    let head: String = chars[..head_end].iter().collect();
    let tail: String = chars[tail_start..].iter().collect();
    let marker = format!(
        "...[middle truncated; shown chars 1-{head_end} and {}-{} of {} total; full output: {}]...",
        tail_start.saturating_add(1),
        total,
        total,
        path.display()
    );
    format!("{head}\n{marker}\n{tail}")
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

use base64::Engine as _;
use rmcp::model::{AnnotateAble, CallToolResult, Content, Meta, RawContent, RawImageContent};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::{Builder, TempDir};

use crate::worker_protocol::{WorkerContent, WorkerReply};

const INLINE_TEXT_LIMIT_BYTES: usize = 10 * 1024;
const INLINE_IMAGE_LIMIT: usize = 4;
const TOOL_NAME_COMPONENT_MAX_BYTES: usize = 24;
const REQUEST_ID_COMPONENT_MAX_BYTES: usize = 48;
const OVERFLOW_ROOT_PREFIX: &str = "mcp-console-overflow-";

#[derive(Clone)]
pub(crate) struct OverflowFileStore {
    inner: Arc<OverflowFileStoreInner>,
}

struct OverflowFileStoreInner {
    root_path: PathBuf,
    cleanup_root_on_drop: bool,
    _temp_dir: Option<TempDir>,
}

impl Drop for OverflowFileStoreInner {
    fn drop(&mut self) {
        if self.cleanup_root_on_drop {
            let _ = fs::remove_dir_all(&self.root_path);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct OverflowMetadata {
    pub(crate) tool_name: String,
    pub(crate) turn_number: u64,
    pub(crate) request_id: String,
}

impl OverflowFileStore {
    pub(crate) fn new() -> io::Result<Self> {
        match Builder::new().prefix(OVERFLOW_ROOT_PREFIX).tempdir() {
            Ok(temp_dir) => {
                let root_path = temp_dir.path().to_path_buf();
                Ok(Self {
                    inner: Arc::new(OverflowFileStoreInner {
                        root_path,
                        cleanup_root_on_drop: false,
                        _temp_dir: Some(temp_dir),
                    }),
                })
            }
            Err(_) => {
                let root_path = fallback_overflow_root();
                fs::create_dir_all(&root_path)?;
                Ok(Self {
                    inner: Arc::new(OverflowFileStoreInner {
                        root_path,
                        cleanup_root_on_drop: true,
                        _temp_dir: None,
                    }),
                })
            }
        }
    }

    fn root_path(&self) -> &Path {
        &self.inner.root_path
    }

    fn overflow_path(&self, metadata: &OverflowMetadata) -> PathBuf {
        self.root_path().join(overflow_filename(metadata))
    }

    #[cfg(test)]
    fn from_root_for_tests(root_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(OverflowFileStoreInner {
                root_path,
                cleanup_root_on_drop: false,
                _temp_dir: None,
            }),
        }
    }
}

pub(crate) fn worker_reply_to_contents(reply: WorkerReply) -> (Vec<Content>, bool) {
    let (contents, is_error) = match reply {
        WorkerReply::Output {
            contents,
            is_error,
            error_code: _,
            prompt: _,
            prompt_variants: _,
        } => (contents, is_error),
    };
    let contents = collapse_image_updates(contents);
    let contents = contents
        .into_iter()
        .map(|content| match content {
            WorkerContent::ContentText { text, .. } => {
                Content::text(normalize_error_prompt(text, is_error))
            }
            WorkerContent::ContentImage {
                data,
                mime_type,
                id,
                is_new,
            } => content_image_with_meta(data, mime_type, id, is_new),
        })
        .collect();

    (contents, is_error)
}

pub(crate) fn finalize_batch(
    mut contents: Vec<Content>,
    is_error: bool,
    overflow_store: Option<&OverflowFileStore>,
    overflow_metadata: OverflowMetadata,
) -> CallToolResult {
    contents = maybe_overflow_image_contents(contents, overflow_store, &overflow_metadata);
    contents = maybe_overflow_text_contents(contents, overflow_store, &overflow_metadata);
    ensure_nonempty_contents(&mut contents);
    // Preserve backend error detection (for prompt normalization, paging decisions, etc.) but
    // do not map it to MCP tool errors.
    let _ = is_error;
    CallToolResult::success(contents)
}

fn ensure_nonempty_contents(contents: &mut Vec<Content>) {
    if contents.is_empty() {
        contents.push(Content::text(String::new()));
    }
}

fn maybe_overflow_text_contents(
    contents: Vec<Content>,
    overflow_store: Option<&OverflowFileStore>,
    overflow_metadata: &OverflowMetadata,
) -> Vec<Content> {
    let total_text_bytes: usize = contents
        .iter()
        .filter_map(content_text)
        .map(|text| text.len())
        .sum();
    if total_text_bytes <= INLINE_TEXT_LIMIT_BYTES {
        return contents;
    }

    let Some(first_text_idx) = contents
        .iter()
        .position(|content| matches!(&content.raw, RawContent::Text(_)))
    else {
        return contents;
    };

    let full_text = collect_text_contents(&contents);
    let overflow_notice = match overflow_store {
        Some(store) => {
            let overflow_path = store.overflow_path(overflow_metadata);
            match write_text_file(&overflow_path, &full_text) {
                Ok(()) => overflow_notice_prefix(Some(&overflow_path)),
                Err(err) => {
                    log_overflow_write_failure(Some(store.root_path()), overflow_metadata, &err);
                    overflow_notice_prefix(None)
                }
            }
        }
        None => overflow_notice_prefix(None),
    };
    let overflow_notice =
        utf8_prefix_by_bytes(&overflow_notice, INLINE_TEXT_LIMIT_BYTES).to_string();
    let mut preview_budget = INLINE_TEXT_LIMIT_BYTES.saturating_sub(overflow_notice.len());

    let mut rewritten = Vec::with_capacity(contents.len());
    let mut inserted_notice = false;
    for (idx, content) in contents.into_iter().enumerate() {
        let Some(text) = content_text(&content) else {
            rewritten.push(content);
            continue;
        };

        let mut replacement = String::new();
        if idx == first_text_idx {
            replacement.push_str(&overflow_notice);
            inserted_notice = true;
        }
        if preview_budget > 0 {
            let preview = utf8_prefix_by_bytes(text, preview_budget);
            if !preview.is_empty() {
                replacement.push_str(preview);
                preview_budget = preview_budget.saturating_sub(preview.len());
            }
        }
        if !replacement.is_empty() {
            rewritten.push(Content::text(replacement));
        }
    }

    debug_assert!(inserted_notice);
    rewritten
}

fn maybe_overflow_image_contents(
    contents: Vec<Content>,
    overflow_store: Option<&OverflowFileStore>,
    overflow_metadata: &OverflowMetadata,
) -> Vec<Content> {
    let total_images = contents
        .iter()
        .filter(|content| matches!(&content.raw, RawContent::Image(_)))
        .count();
    if total_images <= INLINE_IMAGE_LIMIT || overflow_store.is_none() {
        return contents;
    }

    let overflow_store = overflow_store.expect("checked above");
    let mut rewritten = Vec::with_capacity(contents.len());
    let mut inline_images_seen = 0usize;

    for content in contents {
        let Some(image) = raw_image(&content) else {
            rewritten.push(content);
            continue;
        };

        inline_images_seen += 1;
        if inline_images_seen <= INLINE_IMAGE_LIMIT {
            rewritten.push(content);
            continue;
        }

        match write_image_file(overflow_store, overflow_metadata, inline_images_seen, image) {
            Ok(path) => rewritten.push(Content::text(image_overflow_notice(
                inline_images_seen,
                &path,
            ))),
            Err(err) => {
                log_overflow_image_write_failure(
                    Some(overflow_store.root_path()),
                    overflow_metadata,
                    inline_images_seen,
                    &err,
                );
                rewritten.push(content);
            }
        }
    }

    rewritten
}

fn write_text_file(path: &Path, text: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(text.as_bytes())
}

fn write_image_file(
    overflow_store: &OverflowFileStore,
    metadata: &OverflowMetadata,
    image_index: usize,
    image: &rmcp::model::RawImageContent,
) -> io::Result<PathBuf> {
    let path = overflow_store.root_path().join(overflow_image_filename(
        metadata,
        image_index,
        &image.mime_type,
    ));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image.data.as_bytes())
        .map_err(|err| io::Error::other(err.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&bytes)?;
    Ok(path)
}

fn collect_text_contents(contents: &[Content]) -> String {
    let total_bytes: usize = contents
        .iter()
        .filter_map(content_text)
        .map(|text| text.len())
        .sum();
    let mut out = String::with_capacity(total_bytes);
    for content in contents {
        if let Some(text) = content_text(content) {
            out.push_str(text);
        }
    }
    out
}

fn content_text(content: &Content) -> Option<&str> {
    match &content.raw {
        RawContent::Text(text) => Some(text.text.as_str()),
        _ => None,
    }
}

fn raw_image(content: &Content) -> Option<&rmcp::model::RawImageContent> {
    match &content.raw {
        RawContent::Image(image) => Some(image),
        _ => None,
    }
}

fn overflow_notice_prefix(overflow_path: Option<&Path>) -> String {
    match overflow_path {
        Some(path) => format!(
            "[repl] output truncated; full response at {}\n\n",
            path.display()
        ),
        None => {
            "[repl] output truncated; full response could not be persisted by the server\n"
                .to_string()
                + "\n"
        }
    }
}

fn image_overflow_notice(image_index: usize, path: &Path) -> String {
    format!(
        "[repl] image {image_index} omitted from inline response; full image at {}\n",
        path.display()
    )
}

fn utf8_prefix_by_bytes(text: &str, max_bytes: usize) -> &str {
    if max_bytes >= text.len() {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn overflow_filename(metadata: &OverflowMetadata) -> String {
    let tool_name = sanitize_filename_component(&metadata.tool_name, TOOL_NAME_COMPONENT_MAX_BYTES);
    let tool_name = if tool_name.is_empty() {
        "tool".to_string()
    } else {
        tool_name
    };
    let request_id =
        sanitize_filename_component(&metadata.request_id, REQUEST_ID_COMPONENT_MAX_BYTES);
    if request_id.is_empty() {
        format!("{tool_name}-response-{:03}.txt", metadata.turn_number)
    } else {
        format!(
            "{tool_name}-response-{:03}-{request_id}.txt",
            metadata.turn_number
        )
    }
}

fn overflow_image_filename(
    metadata: &OverflowMetadata,
    image_index: usize,
    mime_type: &str,
) -> String {
    let tool_name = sanitize_filename_component(&metadata.tool_name, TOOL_NAME_COMPONENT_MAX_BYTES);
    let tool_name = if tool_name.is_empty() {
        "tool".to_string()
    } else {
        tool_name
    };
    let request_id =
        sanitize_filename_component(&metadata.request_id, REQUEST_ID_COMPONENT_MAX_BYTES);
    let extension = image_extension_for_mime(mime_type);
    if request_id.is_empty() {
        format!(
            "{tool_name}-response-{:03}-image-{image_index:02}.{extension}",
            metadata.turn_number
        )
    } else {
        format!(
            "{tool_name}-response-{:03}-{request_id}-image-{image_index:02}.{extension}",
            metadata.turn_number
        )
    }
}

fn sanitize_filename_component(raw: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let mut sanitized = String::with_capacity(raw.len().min(max_len));
    let mut last_was_separator = false;
    for ch in raw.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            ch
        } else {
            '-'
        };

        if matches!(mapped, '-' | '_') {
            if sanitized.is_empty() || last_was_separator {
                continue;
            }
            sanitized.push(mapped);
            last_was_separator = true;
        } else {
            sanitized.push(mapped);
            last_was_separator = false;
        }

        if sanitized.len() >= max_len {
            break;
        }
    }

    while sanitized.ends_with(['-', '_']) {
        sanitized.pop();
    }

    sanitized
}

fn image_extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

fn fallback_overflow_root() -> PathBuf {
    let mut root = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    root.push(format!("{OVERFLOW_ROOT_PREFIX}{pid}-{nanos}"));
    root
}

fn log_overflow_write_failure(
    root_path: Option<&Path>,
    metadata: &OverflowMetadata,
    err: &io::Error,
) {
    crate::event_log::log(
        "tool_response_overflow_write_failed",
        json!({
            "root_path": root_path.map(|path| path.to_string_lossy().to_string()),
            "tool_name": metadata.tool_name,
            "turn_number": metadata.turn_number,
            "request_id": metadata.request_id,
            "error": err.to_string(),
        }),
    );
}

fn log_overflow_image_write_failure(
    root_path: Option<&Path>,
    metadata: &OverflowMetadata,
    image_index: usize,
    err: &io::Error,
) {
    crate::event_log::log(
        "tool_response_image_overflow_write_failed",
        json!({
            "root_path": root_path.map(|path| path.to_string_lossy().to_string()),
            "tool_name": metadata.tool_name,
            "turn_number": metadata.turn_number,
            "request_id": metadata.request_id,
            "image_index": image_index,
            "error": err.to_string(),
        }),
    );
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
    use super::{
        INLINE_IMAGE_LIMIT, INLINE_TEXT_LIMIT_BYTES, OverflowFileStore, OverflowMetadata,
        content_image_with_meta, finalize_batch, normalize_error_prompt, overflow_filename,
        overflow_image_filename,
    };
    use base64::Engine as _;
    use rmcp::model::RawContent;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::{NamedTempFile, tempdir};

    fn overflow_metadata(tool_name: &str, turn_number: u64, request_id: &str) -> OverflowMetadata {
        OverflowMetadata {
            tool_name: tool_name.to_string(),
            turn_number,
            request_id: request_id.to_string(),
        }
    }

    fn result_text(result: &rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| match &content.raw {
                RawContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn extract_overflow_path(text: &str) -> Option<PathBuf> {
        let marker = "full response at ";
        let start = text.find(marker)? + marker.len();
        let end = text[start..]
            .find('\n')
            .map(|idx| start + idx)
            .unwrap_or(text.len());
        Some(PathBuf::from(text[start..end].trim()))
    }

    fn extract_all_paths(text: &str, marker: &str) -> Vec<PathBuf> {
        text.lines()
            .filter_map(|line| {
                let start = line.find(marker)? + marker.len();
                Some(PathBuf::from(line[start..].trim()))
            })
            .collect()
    }

    fn image_content(seed: u8) -> rmcp::model::Content {
        let bytes = vec![seed; 8];
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        content_image_with_meta(
            data,
            "image/png".to_string(),
            format!("plot-{seed}-1"),
            true,
        )
    }

    #[test]
    fn compact_search_cards_do_not_trigger_error_prompt_normalization() {
        let text = "[repl] search for `Error` @10\n[match] Error: boom\n".to_string();
        assert_eq!(normalize_error_prompt(text.clone(), true), text);
    }

    #[test]
    fn finalize_batch_passes_through_small_text() {
        let store = OverflowFileStore::new().expect("overflow store");
        let result = finalize_batch(
            vec![rmcp::model::Content::text("ok\n".to_string())],
            false,
            Some(&store),
            overflow_metadata("repl", 1, "call_123"),
        );
        assert_eq!(result_text(&result), "ok\n");
    }

    #[test]
    fn overflow_writes_file_and_formats_filename() {
        let store = OverflowFileStore::new().expect("overflow store");
        let full_text = "x".repeat(INLINE_TEXT_LIMIT_BYTES + 256);
        let result = finalize_batch(
            vec![rmcp::model::Content::text(full_text.clone())],
            false,
            Some(&store),
            overflow_metadata("repl", 1, "call_123"),
        );

        let text = result_text(&result);
        assert!(text.contains("output truncated"));
        let path = extract_overflow_path(&text).expect("overflow path");
        assert_eq!(
            path.file_name()
                .and_then(|value| value.to_str())
                .expect("filename"),
            "repl-response-001-call_123.txt"
        );
        assert_eq!(fs::read_to_string(path).expect("overflow file"), full_text);
        assert!(text.len() <= INLINE_TEXT_LIMIT_BYTES);
    }

    #[test]
    fn overflow_filename_sanitizes_and_caps_request_id_suffix() {
        let filename = overflow_filename(&overflow_metadata(
            "repl/reset",
            12,
            " call id/with spaces and $$$ unicode-ą and lots more text that should be cut down ",
        ));

        assert!(
            filename.starts_with(
                "repl-reset-response-012-call-id-with-spaces-and-unicode-and-lots-more"
            )
        );
        assert!(filename.ends_with(".txt"));
        assert!(!filename.contains(' '));
        assert!(!filename.contains('/'));
        assert!(filename.len() < 100);
    }

    #[test]
    fn overflow_filename_omits_empty_request_id_suffix() {
        let filename = overflow_filename(&overflow_metadata("repl", 7, "$$$"));
        assert_eq!(filename, "repl-response-007.txt");
    }

    #[test]
    fn overflow_image_filename_uses_index_and_extension() {
        let filename =
            overflow_image_filename(&overflow_metadata("repl", 5, "call_123"), 6, "image/png");
        assert_eq!(filename, "repl-response-005-call_123-image-06.png");
    }

    #[test]
    fn overflow_preview_trims_on_utf8_boundary() {
        let store = OverflowFileStore::new().expect("overflow store");
        let full_text = "😀".repeat((INLINE_TEXT_LIMIT_BYTES / 4) + 32);
        let result = finalize_batch(
            vec![rmcp::model::Content::text(full_text.clone())],
            false,
            Some(&store),
            overflow_metadata("repl", 2, "call_utf8"),
        );

        let text = result_text(&result);
        let preview = text
            .split_once("\n\n")
            .map(|(_, preview)| preview)
            .expect("preview separator");
        assert!(full_text.starts_with(preview));
        assert!(preview.chars().all(|ch| ch == '😀'));
        assert!(text.len() <= INLINE_TEXT_LIMIT_BYTES);
    }

    #[test]
    fn overflow_preserves_image_content() {
        let store = OverflowFileStore::new().expect("overflow store");
        let result = finalize_batch(
            vec![
                rmcp::model::Content::text("x".repeat(INLINE_TEXT_LIMIT_BYTES + 256)),
                content_image_with_meta(
                    "image-data".to_string(),
                    "image/png".to_string(),
                    "plot-123-1".to_string(),
                    true,
                ),
            ],
            false,
            Some(&store),
            overflow_metadata("repl", 3, "call_image"),
        );

        let image_count = result
            .content
            .iter()
            .filter(|content| matches!(content.raw, RawContent::Image(_)))
            .count();
        assert_eq!(image_count, 1);
        assert!(result_text(&result).contains("output truncated"));
    }

    #[test]
    fn text_overflow_preserves_interleaved_image_order() {
        let store = OverflowFileStore::new().expect("overflow store");
        let trailing_text = "tail".repeat((INLINE_TEXT_LIMIT_BYTES / 2) + 64);
        let result = finalize_batch(
            vec![
                rmcp::model::Content::text("head\n".to_string()),
                image_content(1),
                rmcp::model::Content::text(trailing_text),
            ],
            false,
            Some(&store),
            overflow_metadata("repl", 4, "call_mixed"),
        );

        assert!(matches!(result.content[0].raw, RawContent::Text(_)));
        assert!(matches!(result.content[1].raw, RawContent::Image(_)));
        assert!(matches!(result.content[2].raw, RawContent::Text(_)));

        let leading_text = match &result.content[0].raw {
            RawContent::Text(text) => text.text.as_str(),
            _ => unreachable!("expected text"),
        };
        assert!(leading_text.contains("output truncated"));
        assert!(leading_text.contains("head"));

        let trailing_text = match &result.content[2].raw {
            RawContent::Text(text) => text.text.as_str(),
            _ => unreachable!("expected text"),
        };
        assert!(trailing_text.starts_with("tail"));
    }

    #[test]
    fn image_overflow_keeps_first_four_inline_and_writes_remaining_files() {
        let store = OverflowFileStore::new().expect("overflow store");
        let contents = (1..=6).map(image_content).collect();
        let result = finalize_batch(
            contents,
            false,
            Some(&store),
            overflow_metadata("repl", 8, "call_images"),
        );

        let inline_image_count = result
            .content
            .iter()
            .filter(|content| matches!(content.raw, RawContent::Image(_)))
            .count();
        assert_eq!(inline_image_count, INLINE_IMAGE_LIMIT);

        let text = result_text(&result);
        assert!(text.contains("image 5 omitted from inline response"));
        assert!(text.contains("image 6 omitted from inline response"));
        let paths = extract_all_paths(&text, "full image at ");
        assert_eq!(paths.len(), 2);
        for path in paths {
            assert!(
                path.exists(),
                "expected overflow image file to exist: {path:?}"
            );
            assert_eq!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("png"),
                "expected png overflow image extension: {path:?}"
            );
        }
    }

    #[test]
    fn image_overflow_notice_stays_in_original_order() {
        let store = OverflowFileStore::new().expect("overflow store");
        let result = finalize_batch(
            vec![
                rmcp::model::Content::text("before\n".to_string()),
                image_content(1),
                image_content(2),
                image_content(3),
                image_content(4),
                rmcp::model::Content::text("between\n".to_string()),
                image_content(5),
                rmcp::model::Content::text("after\n".to_string()),
            ],
            false,
            Some(&store),
            overflow_metadata("repl", 9, "call_order"),
        );

        assert!(matches!(result.content[0].raw, RawContent::Text(_)));
        assert!(matches!(result.content[1].raw, RawContent::Image(_)));
        assert!(matches!(result.content[2].raw, RawContent::Image(_)));
        assert!(matches!(result.content[3].raw, RawContent::Image(_)));
        assert!(matches!(result.content[4].raw, RawContent::Image(_)));
        assert!(matches!(result.content[5].raw, RawContent::Text(_)));
        assert!(matches!(result.content[6].raw, RawContent::Text(_)));
        assert!(matches!(result.content[7].raw, RawContent::Text(_)));

        let notice = match &result.content[6].raw {
            RawContent::Text(text) => text.text.as_str(),
            _ => unreachable!("expected text"),
        };
        assert!(notice.contains("image 5 omitted from inline response"));
        assert!(notice.contains("full image at "));

        let trailing_text = match &result.content[7].raw {
            RawContent::Text(text) => text.text.as_str(),
            _ => unreachable!("expected text"),
        };
        assert_eq!(trailing_text, "after\n");
    }

    #[test]
    fn image_overflow_falls_back_to_inline_images_when_write_fails() {
        let temp = tempdir().expect("tempdir");
        let blocked_root = temp.path().join("not-a-directory");
        fs::write(&blocked_root, b"blocked").expect("blocked root file");
        let store = OverflowFileStore::from_root_for_tests(blocked_root);
        let contents = (1..=5).map(image_content).collect();
        let result = finalize_batch(
            contents,
            false,
            Some(&store),
            overflow_metadata("repl", 10, "call_image_fail"),
        );

        let inline_image_count = result
            .content
            .iter()
            .filter(|content| matches!(content.raw, RawContent::Image(_)))
            .count();
        assert_eq!(inline_image_count, 5);
        assert!(!result_text(&result).contains("full image at "));
    }

    #[test]
    fn image_overflow_notices_are_capped_by_text_overflow_guard() {
        let store = OverflowFileStore::new().expect("overflow store");
        let total_images = INLINE_IMAGE_LIMIT + 240;
        let contents = (0..total_images)
            .map(|idx| image_content((idx % (u8::MAX as usize)) as u8))
            .collect();
        let result = finalize_batch(
            contents,
            false,
            Some(&store),
            overflow_metadata("repl", 11, "call_many_images"),
        );

        let inline_image_count = result
            .content
            .iter()
            .filter(|content| matches!(content.raw, RawContent::Image(_)))
            .count();
        assert_eq!(inline_image_count, INLINE_IMAGE_LIMIT);

        let text = result_text(&result);
        assert!(text.contains("output truncated"));
        assert!(text.len() <= INLINE_TEXT_LIMIT_BYTES);

        let path = extract_overflow_path(&text).expect("overflow path");
        let overflow_text = fs::read_to_string(path).expect("overflow file");
        assert!(overflow_text.contains("image 5 omitted from inline response"));
        assert!(overflow_text.contains(&format!(
            "image {total_images} omitted from inline response"
        )));
    }

    #[test]
    fn overflow_falls_back_to_inline_preview_when_write_fails() {
        let temp = tempdir().expect("tempdir");
        let blocked_root = temp.path().join("not-a-directory");
        fs::write(&blocked_root, b"blocked").expect("blocked root file");
        let store = OverflowFileStore::from_root_for_tests(blocked_root);
        let result = finalize_batch(
            vec![rmcp::model::Content::text(
                "x".repeat(INLINE_TEXT_LIMIT_BYTES + 128),
            )],
            false,
            Some(&store),
            overflow_metadata("repl", 4, "call_fail"),
        );

        let text = result_text(&result);
        assert!(text.contains("output truncated"));
        assert!(text.contains("could not be persisted"));
        assert!(!text.contains("full response at "));
        assert!(text.len() <= INLINE_TEXT_LIMIT_BYTES);
    }

    #[test]
    fn overflow_without_store_keeps_server_response_usable() {
        let result = finalize_batch(
            vec![rmcp::model::Content::text(
                "x".repeat(INLINE_TEXT_LIMIT_BYTES + 128),
            )],
            false,
            None,
            overflow_metadata("repl", 12, "call_no_store"),
        );

        let text = result_text(&result);
        assert!(text.contains("output truncated"));
        assert!(text.contains("could not be persisted"));
        assert!(!text.contains("full response at "));
        assert!(text.len() <= INLINE_TEXT_LIMIT_BYTES);
    }

    #[test]
    fn overflow_file_store_test_constructor_allows_non_directory_roots() {
        let blocked = NamedTempFile::new().expect("named temp file");
        let store = OverflowFileStore::from_root_for_tests(blocked.path().to_path_buf());
        assert_eq!(store.root_path(), blocked.path());
    }
}

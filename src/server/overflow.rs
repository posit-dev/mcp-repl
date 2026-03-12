use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;

use crate::reply_overflow::{ReplyOverflowBehavior, ReplyOverflowSettings};
use crate::worker_protocol::{TextStream, WorkerContent, WorkerReply};

static REPLY_FILES_ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default)]
pub(crate) struct ReplyFilesManager {
    root: Option<PathBuf>,
    next_sequence: u64,
    dirs: VecDeque<PathBuf>,
}

impl ReplyFilesManager {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self {
            root: Some(build_reply_files_root()?),
            next_sequence: 0,
            dirs: VecDeque::new(),
        })
    }

    pub(crate) fn clear(&mut self) -> std::io::Result<()> {
        if let Some(root) = self.root.take() {
            let _ = std::fs::remove_dir_all(&root);
        }
        self.root = Some(build_reply_files_root()?);
        self.next_sequence = 0;
        self.dirs.clear();
        Ok(())
    }

    pub(crate) fn apply_to_reply(
        &mut self,
        reply: WorkerReply,
        settings: &ReplyOverflowSettings,
    ) -> WorkerReply {
        if settings.behavior != ReplyOverflowBehavior::Files {
            return reply;
        }

        match reply {
            WorkerReply::Output {
                contents,
                is_error,
                error_code,
                prompt,
                prompt_variants,
            } => {
                let contents =
                    render_files_reply_contents(self, contents, prompt.as_deref(), settings);
                WorkerReply::Output {
                    contents,
                    is_error,
                    error_code,
                    prompt,
                    prompt_variants,
                }
            }
        }
    }

    fn next_reply_dir(&mut self, retention_max_dirs: usize) -> std::io::Result<PathBuf> {
        if self.root.is_none() {
            self.root = Some(build_reply_files_root()?);
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        let dir = self
            .root
            .as_ref()
            .expect("reply files root should be initialized")
            .join(format!("reply-files-{:04}", self.next_sequence));
        std::fs::create_dir_all(&dir)?;
        self.dirs.push_back(dir.clone());
        while self.dirs.len() > retention_max_dirs {
            if let Some(path) = self.dirs.pop_front() {
                let _ = std::fs::remove_dir_all(path);
            }
        }
        Ok(dir)
    }
}

fn render_files_reply_contents(
    files: &mut ReplyFilesManager,
    contents: Vec<WorkerContent>,
    prompt: Option<&str>,
    settings: &ReplyOverflowSettings,
) -> Vec<WorkerContent> {
    let contents = collapse_image_updates(contents);
    let (body_contents, prompt_chunk) = split_prompt_chunk(contents, prompt);

    let text_spans = body_contents
        .iter()
        .filter_map(|content| match content {
            WorkerContent::ContentText { text, .. } => Some(text.as_str()),
            WorkerContent::ContentImage { .. } => None,
        })
        .collect::<Vec<_>>();
    let full_text = text_spans.concat();
    let images = body_contents
        .iter()
        .filter(|content| matches!(content, WorkerContent::ContentImage { .. }))
        .cloned()
        .collect::<Vec<_>>();

    let text_budget = usize::try_from(settings.text.preview_bytes).unwrap_or(usize::MAX);
    let text_spills =
        full_text.len() > usize::try_from(settings.text.spill_bytes).unwrap_or(usize::MAX);
    let image_spills = images.len() > settings.images.spill_count;

    if !text_spills && !image_spills {
        return reattach_prompt(body_contents, prompt_chunk);
    }

    let inline_image_limit = if image_spills {
        settings.images.preview_count.min(images.len())
    } else {
        images.len()
    };

    let mut rendered = Vec::new();
    if text_spills {
        let preview = build_text_preview(&full_text, text_budget);
        if !preview.text.is_empty() {
            rendered.push(WorkerContent::stdout(preview.text.clone()));
        }
        for image in images.iter().take(inline_image_limit) {
            rendered.push(image.clone());
        }
        let mut annotations = write_reply_files(
            files,
            Some(&full_text),
            image_spills.then(|| {
                images
                    .iter()
                    .skip(inline_image_limit)
                    .cloned()
                    .collect::<Vec<_>>()
            }),
            Some(TextPreviewSummary {
                total_lines: preview.total_lines,
            }),
            inline_image_limit,
            images.len(),
            settings.retention.max_dirs,
        );
        if !preview.text.is_empty() && !annotations.is_empty() {
            if preview.text.ends_with('\n') || preview.text.ends_with('\r') {
                annotations.insert(0, '\n');
            } else {
                annotations.insert_str(0, "\n\n");
            }
        }
        if !annotations.is_empty() {
            rendered.push(WorkerContent::stderr(annotations));
        }
    } else {
        let mut seen_images = 0usize;
        for content in body_contents {
            match content {
                WorkerContent::ContentText { .. } => rendered.push(content),
                WorkerContent::ContentImage { .. } => {
                    if seen_images < inline_image_limit {
                        rendered.push(content);
                    }
                    seen_images = seen_images.saturating_add(1);
                }
            }
        }
        let annotations = write_reply_files(
            files,
            None,
            image_spills.then(|| {
                images
                    .iter()
                    .skip(inline_image_limit)
                    .cloned()
                    .collect::<Vec<_>>()
            }),
            None,
            inline_image_limit,
            images.len(),
            settings.retention.max_dirs,
        );
        if !annotations.is_empty() {
            rendered.push(WorkerContent::stderr(annotations));
        }
    }

    reattach_prompt(rendered, prompt_chunk)
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

fn split_prompt_chunk(
    mut contents: Vec<WorkerContent>,
    prompt: Option<&str>,
) -> (Vec<WorkerContent>, Option<WorkerContent>) {
    let Some(prompt) = prompt else {
        return (contents, None);
    };
    let Some(WorkerContent::ContentText {
        text,
        stream: TextStream::Stdout,
    }) = contents.last()
    else {
        return (contents, None);
    };
    if text != prompt {
        return (contents, None);
    }
    let prompt_chunk = contents.pop();
    (contents, prompt_chunk)
}

fn reattach_prompt(
    mut contents: Vec<WorkerContent>,
    prompt_chunk: Option<WorkerContent>,
) -> Vec<WorkerContent> {
    if let Some(prompt_chunk) = prompt_chunk {
        contents.push(prompt_chunk);
    }
    contents
}

struct TextPreview {
    text: String,
    total_lines: usize,
}

#[derive(Clone, Copy)]
struct TextPreviewSummary {
    total_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedImageRange {
    extension: String,
    start_index: usize,
    end_index: usize,
}

fn write_reply_files(
    files: &mut ReplyFilesManager,
    full_text: Option<&str>,
    omitted_images: Option<Vec<WorkerContent>>,
    text_preview: Option<TextPreviewSummary>,
    inline_images: usize,
    total_images: usize,
    retention_max_dirs: usize,
) -> String {
    let need_text = full_text.is_some();
    let need_images = omitted_images
        .as_ref()
        .is_some_and(|images| !images.is_empty());
    if !need_text && !need_images {
        return String::new();
    }

    let dir = match files.next_reply_dir(retention_max_dirs) {
        Ok(path) => path,
        Err(err) => return format!("[repl] failed to write reply files: {err}\n"),
    };

    let mut lines = Vec::new();

    if let Some(text) = full_text {
        let text_path = dir.join("output.log");
        match std::fs::write(&text_path, text) {
            Ok(()) => {
                let summary =
                    text_preview.expect("text preview should be present when text spills");
                lines.push(format!(
                    "[full output ({} {}) written to {}]",
                    summary.total_lines,
                    pluralize(summary.total_lines, "line", "lines"),
                    text_path.display()
                ));
            }
            Err(err) => lines.push(format!(
                "[repl] failed to write full text file {}: {err}]",
                text_path.display()
            )),
        }
    }

    if let Some(images) = omitted_images
        && !images.is_empty()
    {
        lines.push(format!(
            "[inline images={}, saved images={}]",
            inline_images,
            total_images.saturating_sub(inline_images)
        ));
        let mut saved_ranges = Vec::new();
        for (idx, image) in images.into_iter().enumerate() {
            let WorkerContent::ContentImage {
                data, mime_type, ..
            } = image
            else {
                continue;
            };
            let extension = image_extension(&mime_type);
            let path = dir.join(format!("image-{:04}.{}", idx + 1, extension));
            match base64::engine::general_purpose::STANDARD.decode(data.as_bytes()) {
                Ok(bytes) => match std::fs::write(&path, bytes) {
                    Ok(()) => push_saved_image_range(&mut saved_ranges, idx + 1, extension),
                    Err(err) => lines.push(format!(
                        "[repl] failed to write image file {}: {err}]",
                        path.display()
                    )),
                },
                Err(err) => lines.push(format!(
                    "[repl] failed to decode image for {}: {err}]",
                    path.display()
                )),
            }
        }
        for range in saved_ranges {
            lines.push(format_saved_image_range_line(&dir, &range));
        }
    }

    format!("{}\n", lines.join("\n"))
}

fn build_reply_files_root() -> std::io::Result<PathBuf> {
    let parent = std::env::temp_dir();
    for _ in 0..128 {
        let sequence = REPLY_FILES_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            "mcp-repl-reply-files-{}-{}",
            std::process::id(),
            sequence
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to allocate reply files root",
    ))
}

fn build_text_preview(text: &str, preview_bytes: usize) -> TextPreview {
    let total_chars = text.chars().count();
    let total_lines = count_text_lines(text);
    if preview_bytes == 0 {
        return TextPreview {
            text: String::new(),
            total_lines,
        };
    }
    let mut preview = take_prefix_chars(text, preview_bytes);
    if total_chars > preview.chars().count() {
        while preview.ends_with('\n') || preview.ends_with('\r') {
            preview.pop();
        }
        preview.push_str(&format!("...[truncated, total {total_chars} chars]"));
    }
    TextPreview {
        text: preview,
        total_lines,
    }
}

fn count_text_lines(text: &str) -> usize {
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

fn pluralize<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn push_saved_image_range(ranges: &mut Vec<SavedImageRange>, index: usize, extension: &str) {
    if let Some(last) = ranges.last_mut()
        && last.extension == extension
        && last.end_index.saturating_add(1) == index
    {
        last.end_index = index;
        return;
    }
    ranges.push(SavedImageRange {
        extension: extension.to_string(),
        start_index: index,
        end_index: index,
    });
}

fn format_saved_image_range_line(dir: &Path, range: &SavedImageRange) -> String {
    let pattern = dir.join(format!("image-NNNN.{}", range.extension));
    let numbers = if range.start_index == range.end_index {
        format!("{:04}", range.start_index)
    } else {
        format!("{:04}..{:04}", range.start_index, range.end_index)
    };
    format!(
        "[saved images: {} where NNNN={}]",
        pattern.display(),
        numbers
    )
}

fn take_prefix_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in text.char_indices() {
        if idx >= max_bytes {
            break;
        }
        out.push(ch);
    }
    out
}

fn image_extension(mime_type: &str) -> &'static str {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_preview_caps_large_multiline_output() {
        let text = format!("{}\nOK\n", "x".repeat(8_192));
        let preview = build_text_preview(&text, 64);
        assert!(preview.text.len() < 256, "preview was too large");
        assert!(preview.text.contains("[truncated, total 8196 chars]"));
        assert_eq!(preview.total_lines, 2);
    }

    #[test]
    fn zero_byte_preview_shows_only_file_annotation_summary() {
        let preview = build_text_preview("abcdef", 0);
        assert!(preview.text.is_empty());
        assert_eq!(preview.total_lines, 1);
    }

    #[test]
    fn count_text_lines_handles_trailing_newline() {
        assert_eq!(count_text_lines(""), 0);
        assert_eq!(count_text_lines("abc"), 1);
        assert_eq!(count_text_lines("abc\n"), 1);
        assert_eq!(count_text_lines("a\nb\n"), 2);
    }

    #[test]
    fn saved_image_ranges_are_reported_compactly() {
        let mut ranges = Vec::new();
        push_saved_image_range(&mut ranges, 1, "png");
        push_saved_image_range(&mut ranges, 2, "png");
        push_saved_image_range(&mut ranges, 4, "png");
        push_saved_image_range(&mut ranges, 5, "jpg");

        assert_eq!(
            ranges,
            vec![
                SavedImageRange {
                    extension: "png".to_string(),
                    start_index: 1,
                    end_index: 2,
                },
                SavedImageRange {
                    extension: "png".to_string(),
                    start_index: 4,
                    end_index: 4,
                },
                SavedImageRange {
                    extension: "jpg".to_string(),
                    start_index: 5,
                    end_index: 5,
                },
            ]
        );

        let dir = Path::new("/tmp/reply-files-0001");
        assert_eq!(
            format_saved_image_range_line(dir, &ranges[0]),
            "[saved images: /tmp/reply-files-0001/image-NNNN.png where NNNN=0001..0002]"
        );
    }

    #[test]
    fn retains_prompt_after_spilling_text() {
        let mut files = ReplyFilesManager::new().expect("manager");
        let settings = ReplyOverflowSettings::default();
        let reply = WorkerReply::Output {
            contents: vec![
                WorkerContent::stdout("x".repeat(4_000)),
                WorkerContent::stdout("> "),
            ],
            is_error: false,
            error_code: None,
            prompt: Some("> ".to_string()),
            prompt_variants: None,
        };

        let WorkerReply::Output { contents, .. } = files.apply_to_reply(reply, &settings);
        assert!(
            matches!(contents.last(), Some(WorkerContent::ContentText { text, .. }) if text == "> ")
        );
        let rendered = contents
            .into_iter()
            .filter_map(|content| match content {
                WorkerContent::ContentText { text, .. } => Some(text),
                WorkerContent::ContentImage { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(rendered.contains("[full output (1 line) written to "));
    }
}

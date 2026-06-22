use crate::output_capture::{OutputBuffer, OutputEventKind};
use crate::pager;
use crate::pending_output_tape::FormattedPendingOutput;
use crate::reply_presentation::append_protocol_warnings;
use crate::worker_protocol::WorkerContent;

pub(crate) struct SnapshotWithImages {
    pub(crate) contents: Vec<WorkerContent>,
    pub(crate) pages_left: u64,
    pub(crate) buffer: Option<pager::PagerBuffer>,
    pub(crate) last_range: Option<(u64, u64)>,
}

pub(crate) struct CompletionSnapshot {
    pub(crate) snapshot: SnapshotWithImages,
    pub(crate) saw_stderr: bool,
}

pub(crate) fn snapshot_page_with_images(
    output: &OutputBuffer,
    end_offset: u64,
    target_bytes: u64,
) -> SnapshotWithImages {
    let start_offset = output.current_offset().unwrap_or(end_offset);
    let image_groups = collect_image_groups(output, start_offset, end_offset);
    let pager::SnapshotPage {
        mut contents,
        pages_left,
        buffer,
        last_range,
        last_range_end_byte,
    } = pager::take_snapshot_page_from_ring(output, end_offset, target_bytes);
    if pages_left == 0
        && pager::MAX_IMAGES_PER_PAGE > 0
        && contents
            .iter()
            .all(|content| !matches!(content, WorkerContent::ContentImage { .. }))
        && !image_groups.is_empty()
    {
        let max = pager::MAX_IMAGES_PER_PAGE.min(image_groups.len());
        for (_, image) in image_groups.into_iter().take(max) {
            contents.push(image);
        }
        return SnapshotWithImages {
            contents,
            pages_left,
            buffer,
            last_range,
        };
    }
    let page_end = page_end_offset(start_offset, end_offset, pages_left, last_range_end_byte);
    let mut remaining_images = pager::MAX_IMAGES_PER_PAGE;
    if remaining_images > 0 {
        let already = contents
            .iter()
            .filter(|content| matches!(content, WorkerContent::ContentImage { .. }))
            .count();
        remaining_images = remaining_images.saturating_sub(already);
    }
    if remaining_images > 0 && page_end < end_offset {
        append_image_groups_after_page(&mut contents, page_end, image_groups, remaining_images);
    }
    SnapshotWithImages {
        contents,
        pages_left,
        buffer,
        last_range,
    }
}

pub(crate) fn snapshot_pending_timeout_page_with_images(
    output: &OutputBuffer,
    end_offset: u64,
    target_bytes: u64,
) -> SnapshotWithImages {
    let start_offset = output.current_offset().unwrap_or(end_offset);
    let range = output.read_range(start_offset, end_offset);
    if range.bytes.is_empty()
        && !range.events.is_empty()
        && range
            .events
            .iter()
            .all(|event| matches!(event.kind, OutputEventKind::InputEcho { .. }))
    {
        return SnapshotWithImages {
            contents: Vec::new(),
            pages_left: 0,
            buffer: None,
            last_range: None,
        };
    }

    snapshot_page_with_images(output, end_offset, target_bytes)
}

pub(crate) fn snapshot_after_completion(
    output: &OutputBuffer,
    start_offset: u64,
    end_offset: u64,
    target_bytes: u64,
) -> CompletionSnapshot {
    let saw_stderr = output.saw_stderr_in_range(start_offset.min(end_offset), end_offset);
    let snapshot = snapshot_page_with_images(output, end_offset, target_bytes);
    CompletionSnapshot {
        snapshot,
        saw_stderr,
    }
}

pub(crate) fn take_range_from_ring_after_completion(
    output: &OutputBuffer,
    start_offset: u64,
    end_offset: u64,
    protocol_warnings: &[String],
) -> FormattedPendingOutput {
    let saw_stderr = output.saw_stderr_in_range(start_offset.min(end_offset), end_offset);
    let mut contents = pager::take_range_from_ring(output, end_offset);
    append_protocol_warnings(&mut contents, protocol_warnings);
    FormattedPendingOutput {
        contents,
        saw_stderr,
    }
}

fn page_end_offset(
    start_offset: u64,
    end_offset: u64,
    pages_left: u64,
    last_range_end_byte: Option<u64>,
) -> u64 {
    if pages_left == 0 {
        return end_offset;
    }
    if let Some(end_byte) = last_range_end_byte {
        return start_offset.saturating_add(end_byte);
    }
    start_offset
}

fn collect_image_groups(
    output: &OutputBuffer,
    start_offset: u64,
    end_offset: u64,
) -> Vec<(u64, WorkerContent)> {
    let range = output.read_range(start_offset, end_offset);
    let mut groups: Vec<(u64, WorkerContent)> = Vec::new();
    let mut current: Option<(u64, WorkerContent)> = None;

    for event in range.events.iter() {
        let (is_new, content) = match &event.kind {
            OutputEventKind::Image {
                data,
                mime_type,
                id,
                is_new,
                ..
            } => (
                *is_new,
                WorkerContent::ContentImage {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                    id: id.clone(),
                    is_new: *is_new,
                },
            ),
            _ => continue,
        };

        if is_new || current.is_none() {
            if let Some(prev) = current.take() {
                groups.push(prev);
            }
            current = Some((event.offset, content));
        } else {
            current = Some((event.offset, content));
        }
    }
    if let Some(prev) = current.take() {
        groups.push(prev);
    }

    groups
}

fn append_image_groups_after_page(
    contents: &mut Vec<WorkerContent>,
    page_end_offset: u64,
    groups: Vec<(u64, WorkerContent)>,
    max_images: usize,
) {
    let mut appended = 0usize;
    let mut last_offset = page_end_offset;
    for (offset, content) in groups {
        if offset <= page_end_offset {
            continue;
        }
        if appended >= max_images {
            break;
        }
        if offset > last_offset {
            contents.push(WorkerContent::server_stderr(format!(
                "[pager] elided output: @{last_offset}..{offset}\n"
            )));
        }
        contents.push(content);
        appended = appended.saturating_add(1);
        last_offset = offset;
    }
}

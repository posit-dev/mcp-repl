use super::WorkerManager;
use crate::completion_reply::ReplyWithOffset;
use crate::output_snapshot::{SnapshotWithImages, snapshot_page_with_images};
use crate::pager;
use crate::pending_output_tape::FormattedPendingOutput;
use crate::worker_protocol::{WorkerContent, WorkerReply};

impl WorkerManager {
    pub(super) fn build_session_reset_reply_files(&mut self, meta: &str) -> ReplyWithOffset {
        let formatted = self.drain_sealed_formatted_output();
        self.build_session_reset_reply_files_from_formatted(meta, formatted)
    }

    pub(super) fn build_session_reset_reply_files_from_formatted(
        &mut self,
        meta: &str,
        formatted: FormattedPendingOutput,
    ) -> ReplyWithOffset {
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = formatted;
        contents.retain(|content| match content {
            WorkerContent::ContentText { text, .. } => !text.trim().is_empty(),
            _ => true,
        });
        let is_error = saw_stderr;
        if !meta.is_empty() {
            contents.push(WorkerContent::server_stderr(format!("[repl] {meta}")));
        }

        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: None,
                prompt: None,
                prompt_variants: None,
            },
        }
    }

    pub(super) fn build_session_reset_reply_pager(
        &mut self,
        page_bytes: u64,
        meta: &str,
    ) -> ReplyWithOffset {
        let end_offset = self.output.end_offset().unwrap_or(0);
        self.build_session_reset_reply_pager_to_offset(page_bytes, meta, end_offset)
    }

    pub(super) fn build_session_reset_reply_pager_to_offset(
        &mut self,
        page_bytes: u64,
        meta: &str,
        end_offset: u64,
    ) -> ReplyWithOffset {
        let mut is_error = false;

        let SnapshotWithImages {
            mut contents,
            pages_left,
            buffer,
            last_range,
        } = snapshot_page_with_images(&self.output, end_offset, page_bytes);

        contents.retain(|content| match content {
            WorkerContent::ContentText { text, .. } => !text.trim().is_empty(),
            _ => true,
        });

        if !contents.is_empty() {
            let start_offset = self.output.current_offset().unwrap_or(end_offset);
            is_error = self
                .output
                .saw_stderr_in_range(start_offset.min(end_offset), end_offset);
        }

        if !meta.is_empty() {
            contents.push(WorkerContent::server_stderr(format!("[repl] {meta}")));
        }

        pager::maybe_activate_and_append_footer(
            &mut self.pager,
            &mut contents,
            pages_left,
            is_error,
            buffer,
            last_range,
        );

        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: None,
                prompt: None,
                prompt_variants: None,
            },
        }
    }
}

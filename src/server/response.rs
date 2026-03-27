use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rmcp::model::{
    AnnotateAble, CallToolResult, Content, Meta, RawContent, RawImageContent, RawTextContent,
};
use serde_json::Value;
use tempfile::Builder;

use crate::worker_process::WorkerError;
use crate::worker_protocol::{
    ContentOrigin, TextStream, WorkerContent, WorkerErrorCode, WorkerReply,
};

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
const TEXT_STREAM_META_KEY: &str = "mcpReplTextStream";

pub(crate) struct ResponseState {
    output_store: OutputStore,
    active_timeout_bundle: Option<ActiveOutputBundle>,
    staged_timeout_output: Option<StagedTimeoutOutput>,
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
    WorkerText { text: String, stream: TextStream },
    ServerText { text: String, stream: TextStream },
    Image(ReplyImage),
}

impl ReplyItem {
    fn worker_text(text: impl Into<String>, stream: TextStream) -> Self {
        Self::WorkerText {
            text: text.into(),
            stream,
        }
    }

    fn server_text(text: impl Into<String>, stream: TextStream) -> Self {
        Self::ServerText {
            text: text.into(),
            stream,
        }
    }
}

#[derive(Clone)]
struct ReplyImage {
    data: String,
    mime_type: String,
    is_new: bool,
}

#[derive(Clone)]
struct StagedTimeoutOutput {
    items: Vec<ReplyItem>,
}

struct ReplyMaterial {
    inline_items: Vec<ReplyItem>,
    bundle_items: Vec<ReplyItem>,
    worker_text: String,
    detached_prefix_items: Vec<ReplyItem>,
    detached_prefix_inline_items: Vec<ReplyItem>,
    detached_prefix_worker_text: String,
    reply_inline_items: Vec<ReplyItem>,
    reply_bundle_items: Vec<ReplyItem>,
    reply_worker_text: String,
    is_error: bool,
    error_code: Option<WorkerErrorCode>,
}

struct FollowUpDetachedPrefix {
    contents: Vec<Content>,
    protected_bundle_id: Option<u64>,
    retained_active_timeout_bundle: Option<ActiveOutputBundle>,
    retained_staged_timeout_output: Option<StagedTimeoutOutput>,
}

struct TimeoutReplySegment {
    contents: Vec<Content>,
    retained_active_timeout_bundle: Option<ActiveOutputBundle>,
    retained_staged_timeout_output: Option<StagedTimeoutOutput>,
}

struct TimeoutReplyView<'a> {
    bundle_items: &'a [ReplyItem],
    inline_items: &'a [ReplyItem],
    worker_text: &'a str,
    error_code: Option<WorkerErrorCode>,
    protected_bundle_id: Option<u64>,
}

#[derive(Clone, Copy)]
pub(crate) enum TimeoutBundleReuse {
    None,
    FullReply,
    FollowUpInput,
}

pub(crate) fn timeout_bundle_reuse_for_input(input: &str) -> TimeoutBundleReuse {
    if input.is_empty() {
        return TimeoutBundleReuse::FullReply;
    }

    let Some(first) = input.chars().next() else {
        return TimeoutBundleReuse::FullReply;
    };
    let tail = &input[first.len_utf8()..];
    let tail = if let Some(rest) = tail.strip_prefix("\r\n") {
        rest
    } else if let Some(rest) = tail.strip_prefix('\n') {
        rest
    } else if let Some(rest) = tail.strip_prefix('\r') {
        rest
    } else {
        tail
    };

    match first {
        '\u{3}' if tail.is_empty() => TimeoutBundleReuse::FullReply,
        '\u{3}' => TimeoutBundleReuse::FollowUpInput,
        '\u{4}' => TimeoutBundleReuse::None,
        _ => TimeoutBundleReuse::FollowUpInput,
    }
}

impl ResponseState {
    pub(crate) fn new() -> Result<Self, WorkerError> {
        Ok(Self {
            output_store: OutputStore::new()?,
            active_timeout_bundle: None,
            staged_timeout_output: None,
        })
    }

    pub(crate) fn clear_active_timeout_bundle(&mut self) -> Result<(), WorkerError> {
        if let Some(active) = self.active_timeout_bundle.take() {
            self.finish_bundle(active)?;
        }
        self.staged_timeout_output = None;
        Ok(())
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), WorkerError> {
        self.active_timeout_bundle = None;
        self.staged_timeout_output = None;
        self.output_store.cleanup_now()
    }

    #[cfg(test)]
    pub(crate) fn has_active_timeout_bundle(&self) -> bool {
        self.active_timeout_bundle.is_some() || self.staged_timeout_output.is_some()
    }

    fn materialize_staged_timeout_output(
        &mut self,
        staged: &StagedTimeoutOutput,
        protected_bundle_id: Option<u64>,
    ) -> Result<ActiveOutputBundle, WorkerError> {
        let mut bundle = self
            .output_store
            .new_bundle_preserving(protected_bundle_id)?;
        if !staged.items.is_empty()
            && let Err(err) = bundle.append_items(&mut self.output_store, &staged.items)
        {
            if let Err(cleanup_err) = self.finish_bundle(bundle) {
                eprintln!(
                    "dropping closed timeout bundle after output-bundle error: {cleanup_err}"
                );
            }
            return Err(err);
        }
        Ok(bundle)
    }

    /// Converts a worker result into the final MCP reply, including transcript updates and
    /// oversized reply compaction.
    pub(crate) fn finalize_worker_result(
        &mut self,
        result: Result<WorkerReply, WorkerError>,
        pending_request_after: bool,
        timeout_bundle_reuse: TimeoutBundleReuse,
        detached_prefix_item_count: usize,
    ) -> CallToolResult {
        match result {
            Ok(reply) => self.finalize_reply(
                reply,
                pending_request_after,
                timeout_bundle_reuse,
                detached_prefix_item_count,
            ),
            Err(err) => {
                eprintln!("worker write stdin error: {err}");
                if let Err(cleanup_err) = self.clear_active_timeout_bundle() {
                    eprintln!(
                        "dropping closed timeout bundle after output-bundle error: {cleanup_err}"
                    );
                }
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
        timeout_bundle_reuse: TimeoutBundleReuse,
        detached_prefix_item_count: usize,
    ) -> CallToolResult {
        let material = prepare_reply_material(reply, detached_prefix_item_count);
        let mut active_timeout_bundle = self.active_timeout_bundle.take();
        let mut staged_timeout_output = self.staged_timeout_output.take();
        if matches!(timeout_bundle_reuse, TimeoutBundleReuse::FollowUpInput) {
            let contents = self.finalize_follow_up_reply(
                &material,
                pending_request_after,
                active_timeout_bundle,
                staged_timeout_output,
            );
            return finalize_batch(contents, material.is_error);
        }

        let reuse_active_timeout_bundle =
            matches!(timeout_bundle_reuse, TimeoutBundleReuse::FullReply);
        if !reuse_active_timeout_bundle {
            staged_timeout_output = None;
            if let Some(active) = active_timeout_bundle.take()
                && let Err(err) = self.finish_bundle(active)
            {
                eprintln!("dropping closed timeout bundle after output-bundle error: {err}");
            }
        }

        let contents = if active_timeout_bundle.is_none()
            && staged_timeout_output.is_none()
            && should_spill_detached_prefix_only(&material)
        {
            self.finalize_reply_with_spilled_detached_prefix(&material, pending_request_after)
        } else {
            let TimeoutReplySegment {
                contents,
                retained_active_timeout_bundle,
                retained_staged_timeout_output,
            } = self.render_timeout_reply_segment(
                TimeoutReplyView {
                    bundle_items: &material.bundle_items,
                    inline_items: &material.inline_items,
                    worker_text: &material.worker_text,
                    error_code: material.error_code,
                    protected_bundle_id: None,
                },
                pending_request_after,
                active_timeout_bundle,
                staged_timeout_output,
            );
            self.active_timeout_bundle = retained_active_timeout_bundle;
            self.staged_timeout_output = retained_staged_timeout_output;
            contents
        };

        finalize_batch(contents, material.is_error)
    }

    fn finalize_reply_with_spilled_detached_prefix(
        &mut self,
        material: &ReplyMaterial,
        pending_request_after: bool,
    ) -> Vec<Content> {
        let FollowUpDetachedPrefix {
            mut contents,
            protected_bundle_id,
            retained_active_timeout_bundle,
            retained_staged_timeout_output,
        } = self.render_follow_up_detached_prefix(material, None, None);
        let TimeoutReplySegment {
            contents: reply_contents,
            retained_active_timeout_bundle: retained_reply_timeout_bundle,
            retained_staged_timeout_output: retained_reply_staged_timeout_output,
        } = self.render_timeout_reply_segment(
            TimeoutReplyView {
                bundle_items: &material.reply_bundle_items,
                inline_items: &material.reply_inline_items,
                worker_text: &material.reply_worker_text,
                error_code: material.error_code,
                protected_bundle_id,
            },
            pending_request_after,
            None,
            None,
        );
        contents.extend(reply_contents);
        self.active_timeout_bundle = retained_reply_timeout_bundle;
        self.staged_timeout_output = retained_reply_staged_timeout_output;

        debug_assert!(retained_active_timeout_bundle.is_none());
        debug_assert!(retained_staged_timeout_output.is_none());

        contents
    }

    fn finalize_follow_up_reply(
        &mut self,
        material: &ReplyMaterial,
        pending_request_after: bool,
        active_timeout_bundle: Option<ActiveOutputBundle>,
        staged_timeout_output: Option<StagedTimeoutOutput>,
    ) -> Vec<Content> {
        let FollowUpDetachedPrefix {
            mut contents,
            protected_bundle_id,
            mut retained_active_timeout_bundle,
            mut retained_staged_timeout_output,
        } = self.render_follow_up_detached_prefix(
            material,
            active_timeout_bundle,
            staged_timeout_output,
        );
        let reply_is_server_only_follow_up = material.reply_worker_text.is_empty()
            && count_images(&material.reply_bundle_items) == 0;

        let TimeoutReplySegment {
            contents: reply_contents,
            retained_active_timeout_bundle: mut retained_reply_timeout_bundle,
            retained_staged_timeout_output: mut retained_reply_staged_timeout_output,
        } = self.render_timeout_reply_segment(
            TimeoutReplyView {
                bundle_items: &material.reply_bundle_items,
                inline_items: &material.reply_inline_items,
                worker_text: &material.reply_worker_text,
                error_code: material.error_code,
                protected_bundle_id,
            },
            pending_request_after,
            None,
            None,
        );
        contents.extend(reply_contents);
        if pending_request_after && reply_is_server_only_follow_up {
            if let Some(active) = retained_reply_timeout_bundle.take()
                && let Err(err) = self.finish_bundle(active)
            {
                eprintln!("dropping closed timeout bundle after output-bundle error: {err}");
            }
            retained_reply_staged_timeout_output = None;
        }
        self.active_timeout_bundle = retained_reply_timeout_bundle;
        self.staged_timeout_output = retained_reply_staged_timeout_output;

        if pending_request_after
            && (material.error_code != Some(WorkerErrorCode::Timeout)
                || reply_is_server_only_follow_up)
            && self.active_timeout_bundle.is_none()
            && self.staged_timeout_output.is_none()
        {
            self.active_timeout_bundle = retained_active_timeout_bundle.take();
            self.staged_timeout_output = retained_staged_timeout_output.take();
        }
        if let Some(active) = retained_active_timeout_bundle.take()
            && let Err(err) = self.finish_bundle(active)
        {
            eprintln!("dropping closed timeout bundle after output-bundle error: {err}");
        }

        contents
    }

    fn render_follow_up_detached_prefix(
        &mut self,
        material: &ReplyMaterial,
        active_timeout_bundle: Option<ActiveOutputBundle>,
        staged_timeout_output: Option<StagedTimeoutOutput>,
    ) -> FollowUpDetachedPrefix {
        if let Some(active) = active_timeout_bundle {
            return self.render_follow_up_detached_prefix_with_active_bundle(material, active);
        }

        let detached_prefix_image_count = count_images(&material.detached_prefix_items);
        let combined_image_count =
            staged_timeout_output
                .as_ref()
                .map_or(detached_prefix_image_count, |staged| {
                    staged
                        .image_count()
                        .saturating_add(detached_prefix_image_count)
                });
        let use_output_bundle = combined_image_count > 0
            && should_use_output_bundle(
                combined_image_count,
                material.detached_prefix_worker_text.chars().count(),
            );

        if let Some(mut staged) = staged_timeout_output {
            if use_output_bundle
                || text_should_spill(material.detached_prefix_worker_text.chars().count())
            {
                match self.materialize_staged_timeout_output(&staged, None) {
                    Ok(active) => {
                        return self
                            .render_follow_up_detached_prefix_with_active_bundle(material, active);
                    }
                    Err(err) => {
                        eprintln!("dropping output-bundle setup after output-bundle error: {err}");
                        return FollowUpDetachedPrefix {
                            contents: compact_detached_prefix_without_output_bundle(material),
                            protected_bundle_id: None,
                            retained_active_timeout_bundle: None,
                            retained_staged_timeout_output: None,
                        };
                    }
                }
            }
            staged.extend(&material.detached_prefix_items);
            return FollowUpDetachedPrefix {
                contents: materialize_items(material.detached_prefix_inline_items.clone()),
                protected_bundle_id: None,
                retained_active_timeout_bundle: None,
                retained_staged_timeout_output: staged
                    .has_retained_worker_output()
                    .then_some(staged),
            };
        }

        if use_output_bundle
            || text_should_spill(material.detached_prefix_worker_text.chars().count())
        {
            match self.output_store.new_bundle() {
                Ok(mut bundle) => {
                    match bundle
                        .append_items(&mut self.output_store, &material.detached_prefix_items)
                    {
                        Ok(append) => {
                            bundle.disclosed = true;
                            let contents = if use_output_bundle {
                                compact_output_bundle_items(&append.retained_items, &bundle)
                            } else {
                                let retained_worker_text =
                                    worker_text_from_items(&append.retained_items);
                                compact_text_bundle_items(
                                    append.retained_items,
                                    &retained_worker_text,
                                    &bundle,
                                )
                            };
                            return FollowUpDetachedPrefix {
                                contents,
                                protected_bundle_id: Some(bundle.id),
                                retained_active_timeout_bundle: None,
                                retained_staged_timeout_output: None,
                            };
                        }
                        Err(err) => {
                            eprintln!(
                                "dropping detached idle bundle after output-bundle error: {err}"
                            );
                            if let Err(cleanup_err) = self.finish_bundle(bundle) {
                                eprintln!(
                                    "dropping closed output bundle after output-bundle error: {cleanup_err}"
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    eprintln!("dropping output-bundle setup after output-bundle error: {err}");
                }
            }
            return FollowUpDetachedPrefix {
                contents: compact_detached_prefix_without_output_bundle(material),
                protected_bundle_id: None,
                retained_active_timeout_bundle: None,
                retained_staged_timeout_output: None,
            };
        }
        FollowUpDetachedPrefix {
            contents: materialize_items(material.detached_prefix_inline_items.clone()),
            protected_bundle_id: None,
            retained_active_timeout_bundle: None,
            retained_staged_timeout_output: None,
        }
    }

    fn render_follow_up_detached_prefix_with_active_bundle(
        &mut self,
        material: &ReplyMaterial,
        mut active: ActiveOutputBundle,
    ) -> FollowUpDetachedPrefix {
        let contents = if material.detached_prefix_items.is_empty() {
            Vec::new()
        } else {
            match render_active_bundle_contents(
                &mut self.output_store,
                &mut active,
                &material.detached_prefix_items,
                &material.detached_prefix_inline_items,
                material.detached_prefix_worker_text.chars().count(),
            ) {
                Ok(contents) => contents,
                Err(err) => {
                    eprintln!("dropping timeout bundle content after output-bundle error: {err}");
                    let protected_bundle_id = active.was_disclosed().then_some(active.id);
                    if let Err(err) = self.finish_bundle(active) {
                        eprintln!(
                            "dropping closed timeout bundle after output-bundle error: {err}"
                        );
                    }
                    return FollowUpDetachedPrefix {
                        contents: compact_detached_prefix_without_output_bundle(material),
                        protected_bundle_id,
                        retained_active_timeout_bundle: None,
                        retained_staged_timeout_output: None,
                    };
                }
            }
        };
        let protected_bundle_id = active.was_disclosed().then_some(active.id);
        FollowUpDetachedPrefix {
            contents,
            protected_bundle_id,
            retained_active_timeout_bundle: Some(active),
            retained_staged_timeout_output: None,
        }
    }

    fn render_timeout_reply_segment(
        &mut self,
        view: TimeoutReplyView<'_>,
        pending_request_after: bool,
        active_timeout_bundle: Option<ActiveOutputBundle>,
        staged_timeout_output: Option<StagedTimeoutOutput>,
    ) -> TimeoutReplySegment {
        let TimeoutReplyView {
            bundle_items,
            inline_items,
            worker_text,
            error_code,
            protected_bundle_id,
        } = view;
        if let Some(mut active) = active_timeout_bundle {
            match render_active_bundle_contents(
                &mut self.output_store,
                &mut active,
                bundle_items,
                inline_items,
                worker_text.chars().count(),
            ) {
                Ok(contents) => {
                    if pending_request_after {
                        return TimeoutReplySegment {
                            contents,
                            retained_active_timeout_bundle: Some(active),
                            retained_staged_timeout_output: None,
                        };
                    }
                    if let Err(err) = self.finish_bundle(active) {
                        eprintln!(
                            "dropping closed timeout bundle after output-bundle error: {err}"
                        );
                    }
                    return TimeoutReplySegment {
                        contents,
                        retained_active_timeout_bundle: None,
                        retained_staged_timeout_output: None,
                    };
                }
                Err(err) => {
                    eprintln!("dropping timeout bundle content after output-bundle error: {err}");
                    if let Err(err) = self.finish_bundle(active) {
                        eprintln!(
                            "dropping closed timeout bundle after output-bundle error: {err}"
                        );
                    }
                    return TimeoutReplySegment {
                        contents: compact_items_without_output_bundle(
                            bundle_items,
                            inline_items,
                            worker_text,
                        ),
                        retained_active_timeout_bundle: None,
                        retained_staged_timeout_output: None,
                    };
                }
            }
        }

        let current_image_count = count_images(bundle_items);
        let staged_worker_text_chars = staged_timeout_output
            .as_ref()
            .map_or(0, StagedTimeoutOutput::worker_text_chars);
        let combined_worker_text_chars =
            staged_worker_text_chars.saturating_add(worker_text.chars().count());
        let combined_image_count = staged_timeout_output
            .as_ref()
            .map_or(current_image_count, |staged| {
                staged.image_count().saturating_add(current_image_count)
            });
        let use_output_bundle = combined_image_count > 0
            && should_use_output_bundle(combined_image_count, combined_worker_text_chars);
        let text_spills = text_should_spill(combined_worker_text_chars);

        if let Some(mut staged) = staged_timeout_output {
            if use_output_bundle || text_spills {
                match self.materialize_staged_timeout_output(&staged, protected_bundle_id) {
                    Ok(mut active) => {
                        match render_active_bundle_contents(
                            &mut self.output_store,
                            &mut active,
                            bundle_items,
                            inline_items,
                            combined_worker_text_chars,
                        ) {
                            Ok(contents) => {
                                if pending_request_after {
                                    return TimeoutReplySegment {
                                        contents,
                                        retained_active_timeout_bundle: Some(active),
                                        retained_staged_timeout_output: None,
                                    };
                                }
                                if let Err(err) = self.finish_bundle(active) {
                                    eprintln!(
                                        "dropping closed timeout bundle after output-bundle error: {err}"
                                    );
                                }
                                return TimeoutReplySegment {
                                    contents,
                                    retained_active_timeout_bundle: None,
                                    retained_staged_timeout_output: None,
                                };
                            }
                            Err(err) => {
                                eprintln!(
                                    "dropping timeout bundle content after output-bundle error: {err}"
                                );
                                if let Err(err) = self.finish_bundle(active) {
                                    eprintln!(
                                        "dropping closed timeout bundle after output-bundle error: {err}"
                                    );
                                }
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("dropping timeout bundle setup after output-bundle error: {err}");
                    }
                }
                return TimeoutReplySegment {
                    contents: compact_items_without_output_bundle(
                        bundle_items,
                        inline_items,
                        worker_text,
                    ),
                    retained_active_timeout_bundle: None,
                    retained_staged_timeout_output: None,
                };
            }
            let contents = materialize_items(inline_items.to_vec());
            if pending_request_after {
                staged.extend(bundle_items);
                return TimeoutReplySegment {
                    contents,
                    retained_active_timeout_bundle: None,
                    retained_staged_timeout_output: Some(staged),
                };
            }
            return TimeoutReplySegment {
                contents,
                retained_active_timeout_bundle: None,
                retained_staged_timeout_output: None,
            };
        }

        if error_code == Some(WorkerErrorCode::Timeout) {
            if use_output_bundle || text_spills {
                match self.output_store.new_bundle_preserving(protected_bundle_id) {
                    Ok(mut bundle) => {
                        match render_active_bundle_contents(
                            &mut self.output_store,
                            &mut bundle,
                            bundle_items,
                            inline_items,
                            worker_text.chars().count(),
                        ) {
                            Ok(contents) => {
                                if pending_request_after {
                                    return TimeoutReplySegment {
                                        contents,
                                        retained_active_timeout_bundle: Some(bundle),
                                        retained_staged_timeout_output: None,
                                    };
                                }
                                if let Err(err) = self.finish_bundle(bundle) {
                                    eprintln!(
                                        "dropping closed timeout bundle after output-bundle error: {err}"
                                    );
                                }
                                return TimeoutReplySegment {
                                    contents,
                                    retained_active_timeout_bundle: None,
                                    retained_staged_timeout_output: None,
                                };
                            }
                            Err(err) => {
                                eprintln!(
                                    "dropping timeout bundle content after output-bundle error: {err}"
                                );
                                if let Err(err) = self.finish_bundle(bundle) {
                                    eprintln!(
                                        "dropping closed timeout bundle after output-bundle error: {err}"
                                    );
                                }
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("dropping timeout bundle setup after output-bundle error: {err}");
                    }
                }
                return TimeoutReplySegment {
                    contents: compact_items_without_output_bundle(
                        bundle_items,
                        inline_items,
                        worker_text,
                    ),
                    retained_active_timeout_bundle: None,
                    retained_staged_timeout_output: None,
                };
            }
            return TimeoutReplySegment {
                contents: materialize_items(inline_items.to_vec()),
                retained_active_timeout_bundle: None,
                retained_staged_timeout_output: pending_request_after
                    .then(|| StagedTimeoutOutput::from_items(bundle_items))
                    .flatten(),
            };
        }

        TimeoutReplySegment {
            contents: render_reply_items(
                &mut self.output_store,
                bundle_items,
                inline_items,
                worker_text,
                protected_bundle_id,
            ),
            retained_active_timeout_bundle: None,
            retained_staged_timeout_output: None,
        }
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
        self.new_bundle_preserving(None)
    }

    fn new_bundle_preserving(
        &mut self,
        protected_bundle_id: Option<u64>,
    ) -> Result<ActiveOutputBundle, WorkerError> {
        self.prune_for_new_bundle(0, protected_bundle_id)?;
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

    fn prune_for_new_bundle(
        &mut self,
        initial_bytes: u64,
        protected_bundle_id: Option<u64>,
    ) -> Result<(), WorkerError> {
        while self.bundles.len() >= self.limits.max_bundle_count {
            if !self.prune_oldest_inactive_bundle(protected_bundle_id)? {
                return Err(WorkerError::Protocol(
                    "output bundle count quota left no room for a new bundle".to_string(),
                ));
            }
        }
        self.prune_until_total_capacity(protected_bundle_id.unwrap_or(0), initial_bytes)?;
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
        let bundle = self.bundles.get(index).expect("bundle index should exist");
        match fs::remove_dir_all(&bundle.dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(WorkerError::Io(err)),
        }
        let bundle = self
            .bundles
            .remove(index)
            .expect("bundle index should still exist");
        self.total_bytes = self.total_bytes.saturating_sub(bundle.bytes_on_disk);
        Ok(())
    }
}

fn create_output_store_root() -> std::io::Result<tempfile::TempDir> {
    match Builder::new().prefix("mcp-repl-output-").tempdir() {
        Ok(root) => Ok(root),
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound
                && output_store_temp_env_has_non_unicode_value() =>
        {
            Builder::new()
                .prefix("mcp-repl-output-")
                .tempdir_in(fallback_output_store_root_dir())
        }
        Err(err) => Err(err),
    }
}

fn output_store_temp_env_has_non_unicode_value() -> bool {
    ["TMPDIR", "TMP", "TEMP"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some() && std::env::var(name).is_err())
}

fn fallback_output_store_root_dir() -> PathBuf {
    let candidate = std::env::temp_dir();
    if candidate.exists() {
        return candidate;
    }

    #[cfg(target_family = "unix")]
    {
        PathBuf::from("/tmp")
    }

    #[cfg(not(target_family = "unix"))]
    {
        std::env::current_dir().unwrap_or(candidate)
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

        for item in items {
            if self.omitted_tail {
                if let ReplyItem::ServerText { text, stream } = item {
                    retained_items.push(ReplyItem::server_text(text.clone(), *stream));
                }
                continue;
            }

            match item {
                ReplyItem::WorkerText { text, stream } => {
                    let append = self.append_worker_text(store, text, *stream)?;
                    if let Some(retained_item) = append {
                        let partial_worker_text = matches!(
                            &retained_item,
                            ReplyItem::WorkerText { text: retained, .. } if retained.len() < text.len()
                        );
                        retained_items.push(retained_item);
                        if partial_worker_text {
                            omitted_this_reply = true;
                            self.apply_omission(store)?;
                        }
                    } else {
                        omitted_this_reply = true;
                        self.apply_omission(store)?;
                    }
                }
                ReplyItem::ServerText { text, stream } => {
                    match self.append_server_text(store, text, *stream)? {
                        Some(retained_item) => retained_items.push(retained_item),
                        None => {
                            omitted_this_reply = true;
                            self.apply_omission(store)?;
                            retained_items.push(ReplyItem::server_text(text.clone(), *stream));
                        }
                    }
                }
                ReplyItem::Image(image) => {
                    if let Some(retained_item) = self.append_image(store, image)? {
                        retained_items.push(retained_item);
                    } else {
                        omitted_this_reply = true;
                        self.apply_omission(store)?;
                    }
                }
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
        stream: TextStream,
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
                return Ok(Some(ReplyItem::worker_text(retained.to_string(), stream)));
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
        stream: TextStream,
    ) -> Result<Option<ReplyItem>, WorkerError> {
        let _ = store;
        Ok(Some(ReplyItem::server_text(text.to_string(), stream)))
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
        if self
            .append_events_log_text(store, OUTPUT_BUNDLE_OMITTED_NOTICE)?
            .is_some()
        {
            self.omission_recorded = true;
        }
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
    fn finish_bundle(&mut self, active: ActiveOutputBundle) -> Result<(), WorkerError> {
        if active.was_disclosed() {
            return Ok(());
        }
        self.output_store.remove_bundle(active.id)
    }
}

impl StagedTimeoutOutput {
    fn from_items(items: &[ReplyItem]) -> Option<Self> {
        let items = Self::retained_items(items);
        (!items.is_empty()).then_some(Self { items })
    }

    fn extend(&mut self, items: &[ReplyItem]) {
        self.items.extend(Self::retained_items(items));
    }

    fn image_count(&self) -> usize {
        count_images(&self.items)
    }

    fn worker_text_chars(&self) -> usize {
        self.items
            .iter()
            .map(|item| match item {
                ReplyItem::WorkerText { text, .. } => text.chars().count(),
                _ => 0,
            })
            .sum()
    }

    fn has_retained_worker_output(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item, ReplyItem::WorkerText { .. } | ReplyItem::Image(_)))
    }

    fn retained_items(items: &[ReplyItem]) -> Vec<ReplyItem> {
        items
            .iter()
            .filter(|item| matches!(item, ReplyItem::WorkerText { .. } | ReplyItem::Image(_)))
            .cloned()
            .collect()
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
fn prepare_reply_material(reply: WorkerReply, detached_prefix_item_count: usize) -> ReplyMaterial {
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
    let mut detached_prefix_items = Vec::new();
    let mut detached_prefix_worker_text = String::new();
    let mut reply_bundle_items = Vec::new();
    let mut reply_worker_text = String::new();

    for (index, content) in contents.into_iter().enumerate() {
        let is_detached_prefix = index < detached_prefix_item_count;
        match content {
            WorkerContent::ContentText {
                text,
                origin,
                stream,
            } => {
                let text = if matches!(origin, ContentOrigin::Worker) {
                    normalize_error_prompt(text, is_error)
                } else {
                    text
                };
                if text.is_empty() {
                    continue;
                }
                let item = match origin {
                    ContentOrigin::Worker => {
                        worker_text.push_str(&text);
                        if is_detached_prefix {
                            detached_prefix_worker_text.push_str(&text);
                        } else {
                            reply_worker_text.push_str(&text);
                        }
                        ReplyItem::worker_text(text, stream)
                    }
                    ContentOrigin::Server => ReplyItem::server_text(text, stream),
                };
                if is_detached_prefix {
                    detached_prefix_items.push(item.clone());
                } else {
                    reply_bundle_items.push(item.clone());
                }
                bundle_items.push(item);
            }
            WorkerContent::ContentImage {
                data,
                mime_type,
                id: _,
                is_new,
            } => {
                let item = ReplyItem::Image(ReplyImage {
                    data,
                    mime_type,
                    is_new,
                });
                if is_detached_prefix {
                    detached_prefix_items.push(item.clone());
                } else {
                    reply_bundle_items.push(item.clone());
                }
                bundle_items.push(item);
            }
        }
    }

    let inline_items = collapse_image_updates(bundle_items.clone());
    let detached_prefix_inline_items = collapse_image_updates(detached_prefix_items.clone());
    let reply_inline_items = collapse_image_updates(reply_bundle_items.clone());

    ReplyMaterial {
        inline_items,
        bundle_items,
        worker_text,
        detached_prefix_items,
        detached_prefix_inline_items,
        detached_prefix_worker_text,
        reply_inline_items,
        reply_bundle_items,
        reply_worker_text,
        is_error,
        error_code,
    }
}

pub(crate) fn finalize_batch(mut contents: Vec<Content>, is_error: bool) -> CallToolResult {
    ensure_nonempty_contents(&mut contents);
    let _ = is_error;
    CallToolResult::success(contents)
}

pub(crate) fn strip_text_stream_meta(result: &mut CallToolResult) {
    for item in &mut result.content {
        let RawContent::Text(text) = &mut item.raw else {
            continue;
        };
        let Some(meta) = &mut text.meta else {
            continue;
        };
        meta.remove(TEXT_STREAM_META_KEY);
        if meta.is_empty() {
            text.meta = None;
        }
    }
}

fn materialize_items(items: Vec<ReplyItem>) -> Vec<Content> {
    items
        .into_iter()
        .map(|item| match item {
            ReplyItem::WorkerText { text, stream } | ReplyItem::ServerText { text, stream } => {
                content_text(text, stream)
            }
            ReplyItem::Image(image) => image_to_content(&image),
        })
        .collect()
}

fn image_to_content(image: &ReplyImage) -> Content {
    content_image(image.data.clone(), image.mime_type.clone())
}

pub(crate) fn text_stream_from_content(content: &Content) -> Option<TextStream> {
    let RawContent::Text(text) = &content.raw else {
        return None;
    };
    text.meta.as_ref().and_then(text_stream_from_meta)
}

fn content_text(text: String, stream: TextStream) -> Content {
    RawContent::Text(RawTextContent {
        text,
        meta: text_stream_meta(stream),
    })
    .no_annotation()
}

fn text_stream_meta(stream: TextStream) -> Option<Meta> {
    if !matches!(stream, TextStream::Stderr) {
        return None;
    }
    let mut meta = Meta::new();
    meta.insert(
        TEXT_STREAM_META_KEY.to_string(),
        Value::String("stderr".to_string()),
    );
    Some(meta)
}

fn text_stream_from_meta(meta: &Meta) -> Option<TextStream> {
    match meta.get(TEXT_STREAM_META_KEY).and_then(Value::as_str) {
        Some("stderr") => Some(TextStream::Stderr),
        Some("stdout") => Some(TextStream::Stdout),
        _ => None,
    }
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
        if let ReplyItem::WorkerText { text, .. } = item {
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
            ReplyItem::WorkerText { .. } => {
                if !worker_inserted {
                    out.push(Content::text(preview.clone()));
                    worker_inserted = true;
                }
            }
            ReplyItem::ServerText { text, stream } => out.push(content_text(text, stream)),
            ReplyItem::Image(image) => out.push(image_to_content(&image)),
        }
    }
    if !worker_inserted {
        out.insert(0, Content::text(preview));
    }
    out
}

fn compact_text_without_bundle_items(items: Vec<ReplyItem>, worker_text: &str) -> Vec<Content> {
    let preview = build_preview(worker_text, None, false);
    let mut out = Vec::new();
    let mut worker_inserted = false;
    for item in items {
        match item {
            ReplyItem::WorkerText { .. } => {
                if !worker_inserted {
                    out.push(Content::text(preview.clone()));
                    worker_inserted = true;
                }
            }
            ReplyItem::ServerText { text, stream } => out.push(content_text(text, stream)),
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
    let (first_anchor, last_anchor) = match bundle.next_image_number {
        0 => (None, None),
        1 => (load_output_bundle_image_content(bundle, 1), None),
        _ => (
            load_output_bundle_history_image_content(bundle, 1, 1),
            load_output_bundle_image_content(bundle, bundle.next_image_number),
        ),
    };
    let displayed_anchor_count =
        usize::from(first_anchor.is_some()) + usize::from(last_anchor.is_some());

    let head_text = collect_prefix_text(
        items,
        first_image_idx.unwrap_or(items.len()),
        HEAD_TEXT_BUDGET,
    );
    if !head_text.is_empty() {
        out.push(Content::text(head_text.clone()));
    }
    if let Some(image) = first_anchor {
        out.push(image);
    }
    out.push(Content::text(build_output_bundle_notice(
        bundle,
        displayed_anchor_count,
    )));
    let pre_last_text = if last_image_idx == first_image_idx {
        collect_non_overlapping_suffix_text_before(
            items,
            last_image_idx,
            &head_text,
            PRE_LAST_TEXT_BUDGET,
        )
    } else {
        collect_suffix_text_before(items, last_image_idx, PRE_LAST_TEXT_BUDGET)
    };
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

fn materialize_items_with_output_bundle_notice(
    items: Vec<ReplyItem>,
    bundle: &ActiveOutputBundle,
    displayed_anchor_count: usize,
) -> Vec<Content> {
    let mut out = materialize_items(items);
    out.push(Content::text(build_output_bundle_notice(
        bundle,
        displayed_anchor_count,
    )));
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
        out.push(Content::text(head_text.clone()));
    }
    if let Some(index) = first_image_idx
        && let ReplyItem::Image(image) = &items[index]
    {
        out.push(image_to_content(image));
    }
    out.push(Content::text(build_output_bundle_unavailable_notice(
        count_images(items),
    )));
    let pre_last_text = if last_image_idx == first_image_idx {
        collect_non_overlapping_suffix_text_before(
            items,
            last_image_idx,
            &head_text,
            PRE_LAST_TEXT_BUDGET,
        )
    } else {
        collect_suffix_text_before(items, last_image_idx, PRE_LAST_TEXT_BUDGET)
    };
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

fn render_active_bundle_contents(
    output_store: &mut OutputStore,
    active: &mut ActiveOutputBundle,
    bundle_items: &[ReplyItem],
    inline_items: &[ReplyItem],
    spill_worker_text_chars: usize,
) -> Result<Vec<Content>, WorkerError> {
    let append = active.append_items(output_store, bundle_items)?;
    let retained_image_count = count_images(&append.retained_items);
    let retained_worker_text = worker_text_from_items(&append.retained_items);
    let has_incremental_content = !append.retained_items.is_empty();
    let image_bundle_still_needed = active.next_image_number > 0
        && should_use_output_bundle(active.history_image_count, spill_worker_text_chars);

    if append.omitted_this_reply {
        active.disclosed = true;
        if active.next_image_number > 0 {
            Ok(compact_output_bundle_items(&append.retained_items, active))
        } else {
            Ok(compact_text_bundle_items(
                append.retained_items.clone(),
                &retained_worker_text,
                active,
            ))
        }
    } else if retained_image_count > 0 && image_bundle_still_needed {
        active.disclosed = true;
        Ok(compact_output_bundle_items(&append.retained_items, active))
    } else if text_should_spill(spill_worker_text_chars) {
        active.disclosed = true;
        Ok(compact_text_bundle_items(
            append.retained_items.clone(),
            &retained_worker_text,
            active,
        ))
    } else if active.was_disclosed() && image_bundle_still_needed && has_incremental_content {
        active.disclosed = true;
        Ok(materialize_items_with_output_bundle_notice(
            inline_items.to_vec(),
            active,
            0,
        ))
    } else {
        Ok(materialize_items(inline_items.to_vec()))
    }
}

fn compact_items_without_output_bundle(
    bundle_items: &[ReplyItem],
    inline_items: &[ReplyItem],
    worker_text: &str,
) -> Vec<Content> {
    let image_count = count_images(bundle_items);
    if image_count > 0 && should_use_output_bundle(image_count, worker_text.chars().count()) {
        return compact_output_without_bundle_items(bundle_items);
    }
    if text_should_spill(worker_text.chars().count()) {
        return compact_text_without_bundle_items(inline_items.to_vec(), worker_text);
    }
    materialize_items(inline_items.to_vec())
}

fn compact_detached_prefix_without_output_bundle(material: &ReplyMaterial) -> Vec<Content> {
    compact_items_without_output_bundle(
        &material.detached_prefix_items,
        &material.detached_prefix_inline_items,
        &material.detached_prefix_worker_text,
    )
}

fn render_reply_items(
    output_store: &mut OutputStore,
    reply_bundle_items: &[ReplyItem],
    reply_inline_items: &[ReplyItem],
    reply_worker_text: &str,
    protected_bundle_id: Option<u64>,
) -> Vec<Content> {
    let reply_image_count = count_images(reply_bundle_items);
    if reply_image_count > 0
        && should_use_output_bundle(reply_image_count, reply_worker_text.chars().count())
    {
        return compact_reply_items_with_new_bundle(
            output_store,
            reply_bundle_items,
            reply_inline_items,
            reply_worker_text,
            false,
            protected_bundle_id,
        );
    }
    if text_should_spill(reply_worker_text.chars().count()) {
        return compact_reply_items_with_new_bundle(
            output_store,
            reply_bundle_items,
            reply_inline_items,
            reply_worker_text,
            true,
            protected_bundle_id,
        );
    }
    materialize_items(reply_inline_items.to_vec())
}

fn compact_reply_items_with_new_bundle(
    output_store: &mut OutputStore,
    reply_bundle_items: &[ReplyItem],
    reply_inline_items: &[ReplyItem],
    reply_worker_text: &str,
    text_only: bool,
    protected_bundle_id: Option<u64>,
) -> Vec<Content> {
    match output_store.new_bundle_preserving(protected_bundle_id) {
        Ok(mut bundle) => match bundle.append_items(output_store, reply_bundle_items) {
            Ok(append) => {
                if text_only {
                    let retained_worker_text = worker_text_from_items(&append.retained_items);
                    compact_text_bundle_items(append.retained_items, &retained_worker_text, &bundle)
                } else {
                    compact_output_bundle_items(&append.retained_items, &bundle)
                }
            }
            Err(err) => {
                eprintln!("dropping output-bundled content after output-bundle error: {err}");
                if let Err(cleanup_err) = output_store.remove_bundle(bundle.id) {
                    eprintln!(
                        "dropping closed output bundle after output-bundle error: {cleanup_err}"
                    );
                }
                if text_only {
                    compact_text_without_bundle_items(
                        reply_inline_items.to_vec(),
                        reply_worker_text,
                    )
                } else {
                    compact_output_without_bundle_items(reply_inline_items)
                }
            }
        },
        Err(err) => {
            eprintln!("dropping output-bundle setup after output-bundle error: {err}");
            if text_only {
                compact_text_without_bundle_items(reply_inline_items.to_vec(), reply_worker_text)
            } else {
                compact_output_without_bundle_items(reply_inline_items)
            }
        }
    }
}

fn should_spill_detached_prefix_only(material: &ReplyMaterial) -> bool {
    should_spill_detached_prefix(material)
        && !should_use_output_bundle(
            count_images(&material.reply_bundle_items),
            material.reply_worker_text.chars().count(),
        )
}

fn should_spill_detached_prefix(material: &ReplyMaterial) -> bool {
    !material.detached_prefix_items.is_empty()
        && count_images(&material.detached_prefix_items) == 0
        && text_should_spill(material.detached_prefix_worker_text.chars().count())
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
        1 if bundle.next_image_number <= 1 && bundle.history_image_count <= 1 => format!(
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

fn collect_non_overlapping_suffix_text_before(
    items: &[ReplyItem],
    index: Option<usize>,
    head_text: &str,
    budget: usize,
) -> String {
    let tail_text = collect_suffix_text_before(items, index, budget);
    let Some(index) = index else {
        return tail_text;
    };
    let total_chars = items[..index]
        .iter()
        .filter_map(item_text)
        .map(|text| text.chars().count())
        .sum::<usize>();
    let overlap = head_text
        .chars()
        .count()
        .saturating_add(tail_text.chars().count())
        .saturating_sub(total_chars);
    drop_prefix_chars(&tail_text, overlap)
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
        ReplyItem::WorkerText { text, .. } | ReplyItem::ServerText { text, .. } => Some(text),
        ReplyItem::Image(_) => None,
    }
}

fn drop_prefix_chars(text: &str, count: usize) -> String {
    if count == 0 {
        return text.to_string();
    }
    text.chars().skip(count).collect()
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
    load_output_bundle_image_content_at_path(&path)
}

fn load_output_bundle_history_image_content(
    bundle: &ActiveOutputBundle,
    image_index: usize,
    history_index: usize,
) -> Option<Content> {
    let stem = format!("images/history/{image_index:03}/{history_index:03}");
    for extension in ["png", "jpg", "jpeg", "gif", "webp", "svg"] {
        let path = bundle.paths.dir.join(format!("{stem}.{extension}"));
        if path.exists() {
            return load_output_bundle_image_content_at_path(&path);
        }
    }
    load_output_bundle_image_content(bundle, image_index)
}

fn load_output_bundle_image_content_at_path(path: &Path) -> Option<Content> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!(
                "skipping unreadable output bundle image {}: {err}",
                path.display()
            );
            return None;
        }
    };
    let mime_type = mime_type_from_path(path);
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
    use std::fs;
    use std::io;
    use std::path::PathBuf;

    use rmcp::model::RawContent;
    use tempfile::Builder;

    use super::{
        OutputStore, ReplyImage, ReplyItem, ResponseState, TimeoutBundleReuse,
        compact_output_bundle_items, normalize_error_prompt,
    };
    use crate::worker_process::WorkerError;
    use crate::worker_protocol::{TextStream, WorkerContent, WorkerErrorCode, WorkerReply};

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

    fn result_images(result: &rmcp::model::CallToolResult) -> Vec<Vec<u8>> {
        result
            .content
            .iter()
            .filter_map(|item| match &item.raw {
                RawContent::Image(image) => base64::engine::general_purpose::STANDARD
                    .decode(image.data.as_bytes())
                    .ok(),
                _ => None,
            })
            .collect()
    }

    fn disclosed_path(text: &str, suffix: &str) -> Option<PathBuf> {
        let end = text.find(suffix)?.saturating_add(suffix.len());
        let start = text[..end]
            .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
            .map_or(0, |idx| idx.saturating_add(1));
        Some(PathBuf::from(&text[start..end]))
    }

    fn disclosed_paths(text: &str, suffix: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut offset = 0;
        while let Some(relative_end) = text[offset..].find(suffix) {
            let end = offset
                .saturating_add(relative_end)
                .saturating_add(suffix.len());
            let start = text[..end]
                .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
                .map_or(0, |idx| idx.saturating_add(1));
            paths.push(PathBuf::from(&text[start..end]));
            offset = end;
        }
        paths
    }

    fn fail_output_store_root_creation() -> io::Result<tempfile::TempDir> {
        Err(io::Error::other("simulated tempdir failure"))
    }

    fn output_store_root_with_text_conflict() -> io::Result<tempfile::TempDir> {
        let root = Builder::new().prefix("mcp-repl-output-test-").tempdir()?;
        fs::create_dir_all(root.path().join("output-0001/transcript.txt"))?;
        Ok(root)
    }

    fn worker_reply(
        contents: Vec<WorkerContent>,
        error_code: Option<WorkerErrorCode>,
    ) -> WorkerReply {
        WorkerReply::Output {
            contents,
            is_error: false,
            error_code,
            prompt: None,
            prompt_variants: None,
        }
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
            .append_worker_text(&mut store, "a", TextStream::Stdout)
            .expect("first worker text should append");
        assert!(matches!(first, Some(ReplyItem::WorkerText { text, .. }) if text == "a"));

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
            .append_worker_text(&mut store, "b\n", TextStream::Stdout)
            .expect("second worker text should append");
        assert!(matches!(second, Some(ReplyItem::WorkerText { text, .. }) if text == "b\n"));

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
    fn detached_prefix_spills_without_swallowing_follow_up_reply() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let detached_prefix = format!(
            "IDLE_START\n{}\nIDLE_END\n",
            "x".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let follow_up = "FOLLOWUP_OK\n".to_string();
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(detached_prefix),
                    WorkerContent::worker_stdout(follow_up.clone()),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::None,
            1,
        );

        let text = result_text(&result);
        let transcript_path = disclosed_path(&text, "transcript.txt")
            .unwrap_or_else(|| panic!("expected detached prefix transcript path, got: {text:?}"));
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected transcript to be readable: {err}"));

        assert!(
            text.contains(&follow_up),
            "expected follow-up reply inline, got: {text:?}"
        );
        assert!(
            transcript.contains("IDLE_START") && transcript.contains("IDLE_END"),
            "expected detached prefix transcript content, got: {transcript:?}"
        );
        assert!(
            !transcript.contains("FOLLOWUP_OK"),
            "did not expect follow-up reply to be appended to detached prefix bundle: {transcript:?}"
        );
    }

    #[test]
    fn detached_prefix_timeout_poll_preserves_later_timeout_bundle_state() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let detached_prefix = format!(
            "IDLE_START\n{}\nIDLE_END\n",
            "x".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let first_timeout_chunk = "FIRST_TIMEOUT\n".to_string();
        let first = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(detached_prefix),
                    WorkerContent::worker_stdout(first_timeout_chunk.clone()),
                ],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FullReply,
            1,
        );

        let first_text = result_text(&first);
        let detached_transcript_path = disclosed_path(&first_text, "transcript.txt")
            .unwrap_or_else(|| {
                panic!("expected detached prefix transcript path, got: {first_text:?}")
            });
        let detached_transcript =
            fs::read_to_string(&detached_transcript_path).unwrap_or_else(|err| {
                panic!("expected detached prefix transcript to be readable: {err}")
            });

        assert!(
            first_text.contains(&first_timeout_chunk),
            "expected the first timed-out chunk to stay inline, got: {first_text:?}"
        );
        assert!(
            state.has_active_timeout_bundle(),
            "expected the timed-out poll to retain timeout state after detached-prefix compaction"
        );
        assert!(
            detached_transcript.contains("IDLE_START") && detached_transcript.contains("IDLE_END"),
            "expected detached-prefix transcript content, got: {detached_transcript:?}"
        );
        assert!(
            !detached_transcript.contains(&first_timeout_chunk),
            "did not expect the timed-out chunk in the detached-prefix transcript: {detached_transcript:?}"
        );

        let later_timeout_chunk = format!(
            "SECOND_START\n{}\nSECOND_END\n",
            "y".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let second = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout(later_timeout_chunk.clone())],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FullReply,
            0,
        );

        let second_text = result_text(&second);
        let timeout_transcript_path = disclosed_path(&second_text, "transcript.txt")
            .unwrap_or_else(|| panic!("expected timeout transcript path, got: {second_text:?}"));
        let timeout_transcript = fs::read_to_string(&timeout_transcript_path)
            .unwrap_or_else(|err| panic!("expected timeout transcript to be readable: {err}"));

        assert_ne!(
            timeout_transcript_path, detached_transcript_path,
            "expected the later timeout spill to use a separate transcript path"
        );
        assert!(
            timeout_transcript.contains(&first_timeout_chunk),
            "expected the later timeout transcript to backfill the first timed-out chunk, got: {timeout_transcript:?}"
        );
        assert!(
            timeout_transcript.contains("SECOND_START")
                && timeout_transcript.contains("SECOND_END"),
            "expected the later timeout transcript to include the new timed-out chunk, got: {timeout_transcript:?}"
        );
        assert!(
            !timeout_transcript.contains("IDLE_START"),
            "did not expect detached-prefix output in the later timeout transcript: {timeout_transcript:?}"
        );
    }

    #[test]
    fn active_timeout_bundle_keeps_detached_prefix_on_same_path() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        let detached_prefix = format!(
            "TAIL_START\n{}\nTAIL_END\n",
            "x".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        bundle
            .append_worker_text(&mut state.output_store, "HEAD\n", TextStream::Stdout)
            .expect("existing timeout text should append");
        let transcript_path = bundle.paths.transcript.clone();
        state.active_timeout_bundle = Some(bundle);

        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(detached_prefix),
                    WorkerContent::worker_stdout("NEW_TURN\n"),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let text = result_text(&result);
        let disclosed_path = disclosed_path(&text, "transcript.txt").unwrap_or_else(|| {
            panic!("expected timeout bundle path in follow-up reply, got: {text:?}")
        });
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected transcript to be readable: {err}"));

        assert!(
            text.contains("NEW_TURN"),
            "expected new request output inline, got: {text:?}"
        );
        assert_eq!(
            disclosed_path, transcript_path,
            "expected follow-up disclosure to reuse the existing timeout bundle path"
        );
        assert!(
            transcript.contains("HEAD\nTAIL_START\n") && transcript.contains("TAIL_END\n"),
            "expected detached prefix to stay on the existing timeout bundle path, got: {transcript:?}"
        );
        assert!(
            !transcript.contains("NEW_TURN"),
            "did not expect new request output to append to the timeout bundle: {transcript:?}"
        );
    }

    #[test]
    fn large_follow_up_reply_still_compacts_after_detached_timeout_tail() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        bundle
            .append_worker_text(&mut state.output_store, "HEAD\n", TextStream::Stdout)
            .expect("existing timeout text should append");
        bundle.disclosed = true;
        let timeout_transcript_path = bundle.paths.transcript.clone();
        state.active_timeout_bundle = Some(bundle);

        let large_follow_up = format!(
            "FOLLOW_UP_START\n{}\nFOLLOW_UP_END\n",
            "y".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout("TAIL\n"),
                    WorkerContent::worker_stdout(large_follow_up.clone()),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let text = result_text(&result);
        let follow_up_transcript_path = disclosed_path(&text, "transcript.txt")
            .unwrap_or_else(|| panic!("expected oversized follow-up bundle path, got: {text:?}"));
        let timeout_transcript = fs::read_to_string(&timeout_transcript_path)
            .unwrap_or_else(|err| panic!("expected timeout transcript to be readable: {err}"));
        let follow_up_transcript = fs::read_to_string(&follow_up_transcript_path)
            .unwrap_or_else(|err| panic!("expected follow-up transcript to be readable: {err}"));

        assert_ne!(
            follow_up_transcript_path, timeout_transcript_path,
            "expected the large follow-up reply to use its own bundle path"
        );
        assert!(
            timeout_transcript.contains("HEAD\nTAIL\n"),
            "expected detached timeout tail to stay on the timeout bundle path, got: {timeout_transcript:?}"
        );
        assert!(
            !timeout_transcript.contains("FOLLOW_UP_START"),
            "did not expect fresh follow-up output on the timeout bundle path: {timeout_transcript:?}"
        );
        assert!(
            follow_up_transcript.contains("FOLLOW_UP_START")
                && follow_up_transcript.contains("FOLLOW_UP_END"),
            "expected follow-up transcript to contain the large fresh reply, got: {follow_up_transcript:?}"
        );
        assert!(
            !follow_up_transcript.contains("TAIL\n"),
            "did not expect the detached timeout tail in the fresh follow-up bundle: {follow_up_transcript:?}"
        );
    }

    #[test]
    fn detached_prefix_and_large_follow_up_each_compact_on_follow_up_input() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let detached_prefix = format!(
            "DETACHED_START\n{}\nDETACHED_END\n",
            "x".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let large_follow_up = format!(
            "FOLLOW_UP_START\n{}\nFOLLOW_UP_END\n",
            "y".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(detached_prefix),
                    WorkerContent::worker_stdout(large_follow_up),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let text = result_text(&result);
        let transcript_paths = disclosed_paths(&text, "transcript.txt");
        assert_eq!(
            transcript_paths.len(),
            2,
            "expected detached prefix and large follow-up to disclose separate bundle paths, got: {text:?}"
        );
        assert_ne!(
            transcript_paths[0], transcript_paths[1],
            "expected detached prefix and large follow-up to use separate bundle paths"
        );

        let detached_transcript = fs::read_to_string(&transcript_paths[0]).unwrap_or_else(|err| {
            panic!("expected detached-prefix transcript to be readable: {err}")
        });
        let follow_up_transcript = fs::read_to_string(&transcript_paths[1])
            .unwrap_or_else(|err| panic!("expected follow-up transcript to be readable: {err}"));

        assert!(
            detached_transcript.contains("DETACHED_START")
                && detached_transcript.contains("DETACHED_END"),
            "expected detached-prefix transcript content, got: {detached_transcript:?}"
        );
        assert!(
            !detached_transcript.contains("FOLLOW_UP_START"),
            "did not expect large follow-up output in detached-prefix transcript: {detached_transcript:?}"
        );
        assert!(
            follow_up_transcript.contains("FOLLOW_UP_START")
                && follow_up_transcript.contains("FOLLOW_UP_END"),
            "expected large follow-up transcript content, got: {follow_up_transcript:?}"
        );
        assert!(
            !follow_up_transcript.contains("DETACHED_START"),
            "did not expect detached-prefix output in follow-up transcript: {follow_up_transcript:?}"
        );
    }

    #[test]
    fn detached_prefix_image_updates_collapse_on_follow_up_input() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::ContentImage {
                        data: base64::engine::general_purpose::STANDARD.encode([0_u8]),
                        mime_type: "image/png".to_string(),
                        id: "plot-1".to_string(),
                        is_new: true,
                    },
                    WorkerContent::ContentImage {
                        data: base64::engine::general_purpose::STANDARD.encode([1_u8]),
                        mime_type: "image/png".to_string(),
                        id: "plot-1".to_string(),
                        is_new: false,
                    },
                    WorkerContent::ContentImage {
                        data: base64::engine::general_purpose::STANDARD.encode([2_u8]),
                        mime_type: "image/png".to_string(),
                        id: "plot-1".to_string(),
                        is_new: false,
                    },
                    WorkerContent::worker_stdout("FOLLOW_UP_OK\n"),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::FollowUpInput,
            3,
        );

        let text = result_text(&result);
        let images = result_images(&result);

        assert!(
            text.contains("FOLLOW_UP_OK"),
            "expected follow-up reply inline, got: {text:?}"
        );
        assert_eq!(
            images.len(),
            1,
            "expected collapsed detached-prefix image updates to keep one inline image"
        );
        assert_eq!(
            images[0],
            vec![2_u8],
            "expected the final detached-prefix image update to remain inline"
        );
        assert!(
            !text.contains("output bundle images"),
            "did not expect a collapsed detached-prefix image update sequence to disclose a bundle, got: {text:?}"
        );
    }

    #[test]
    fn image_heavy_detached_prefix_spills_on_follow_up_input() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut contents: Vec<_> = (0..super::IMAGE_OUTPUT_BUNDLE_THRESHOLD)
            .map(|index| WorkerContent::ContentImage {
                data: base64::engine::general_purpose::STANDARD.encode([index as u8]),
                mime_type: "image/png".to_string(),
                id: format!("plot-{index}"),
                is_new: true,
            })
            .collect();
        contents.push(WorkerContent::worker_stdout("FOLLOW_UP_OK\n"));

        let result = state.finalize_worker_result(
            Ok(worker_reply(contents, None)),
            false,
            TimeoutBundleReuse::FollowUpInput,
            super::IMAGE_OUTPUT_BUNDLE_THRESHOLD,
        );

        let text = result_text(&result);
        let images = result_images(&result);

        assert!(
            text.contains("FOLLOW_UP_OK"),
            "expected follow-up reply inline, got: {text:?}"
        );
        assert!(
            text.contains("output bundle images"),
            "expected detached-prefix image burst to disclose an image bundle, got: {text:?}"
        );
        assert_eq!(
            images.len(),
            2,
            "expected bundled detached-prefix image burst to keep only anchor images inline"
        );
        assert_eq!(
            images[0],
            vec![0_u8],
            "expected the first detached-prefix image to remain as the first inline anchor"
        );
        assert_eq!(
            images[1],
            vec![(super::IMAGE_OUTPUT_BUNDLE_THRESHOLD - 1) as u8],
            "expected the last detached-prefix image to remain as the last inline anchor"
        );
    }

    #[test]
    fn repeated_image_updates_spill_to_bundle_to_preserve_history() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let update_count = super::IMAGE_OUTPUT_BUNDLE_THRESHOLD;
        let contents = (0..update_count)
            .map(|index| WorkerContent::ContentImage {
                data: base64::engine::general_purpose::STANDARD.encode([index as u8]),
                mime_type: "image/png".to_string(),
                id: "plot-1".to_string(),
                is_new: index == 0,
            })
            .collect();

        let result = state.finalize_worker_result(
            Ok(worker_reply(contents, None)),
            false,
            TimeoutBundleReuse::None,
            0,
        );

        let text = result_text(&result);
        let images = result_images(&result);
        assert!(
            text.contains("output bundle images"),
            "expected repeated image updates to disclose an image bundle, got: {text:?}"
        );
        assert_eq!(
            images.len(),
            1,
            "expected repeated updates to one image to keep a single inline anchor"
        );
        assert_eq!(
            images[0],
            vec![(update_count - 1) as u8],
            "expected the inline anchor to use the final image state"
        );

        let bundle_dir = state
            .output_store
            .bundles
            .back()
            .map(|bundle| bundle.dir.clone())
            .expect("expected disclosed output bundle metadata");
        let history_dir = bundle_dir.join("images/history/001");
        let mut history_files: Vec<_> = fs::read_dir(&history_dir)
            .unwrap_or_else(|err| panic!("expected history dir to be readable: {err}"))
            .map(|entry| {
                entry
                    .unwrap_or_else(|err| panic!("expected history dir entry: {err}"))
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        history_files.sort();

        assert_eq!(
            history_files.len(),
            update_count,
            "expected every repeated image update to be preserved in bundle history"
        );
        assert_eq!(
            history_files.first().map(String::as_str),
            Some("001.png"),
            "expected the first history frame to be preserved"
        );
        assert_eq!(
            history_files.last().map(String::as_str),
            Some(format!("{update_count:03}.png").as_str()),
            "expected the final history frame to be preserved"
        );

        let final_alias = fs::read(bundle_dir.join("images/001.png"))
            .unwrap_or_else(|err| panic!("expected final image alias to be readable: {err}"));
        let final_history = fs::read(history_dir.join(format!("{update_count:03}.png")))
            .unwrap_or_else(|err| panic!("expected final history frame to be readable: {err}"));
        assert_eq!(
            final_alias, final_history,
            "expected the final alias to match the final history frame"
        );
    }

    #[test]
    fn timed_out_follow_up_reply_gets_its_own_active_bundle() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        bundle
            .append_worker_text(&mut state.output_store, "HEAD\n", TextStream::Stdout)
            .expect("existing timeout text should append");
        bundle.disclosed = true;
        let timeout_transcript_path = bundle.paths.transcript.clone();
        state.active_timeout_bundle = Some(bundle);

        let large_follow_up = format!(
            "FOLLOW_UP_START\n{}\nFOLLOW_UP_END\n",
            "q".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout("TAIL\n"),
                    WorkerContent::worker_stdout(large_follow_up.clone()),
                ],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let text = result_text(&result);
        let follow_up_transcript_path = disclosed_path(&text, "transcript.txt")
            .unwrap_or_else(|| panic!("expected timed-out follow-up bundle path, got: {text:?}"));
        let timeout_transcript = fs::read_to_string(&timeout_transcript_path)
            .unwrap_or_else(|err| panic!("expected timeout transcript to be readable: {err}"));
        let follow_up_transcript = fs::read_to_string(&follow_up_transcript_path)
            .unwrap_or_else(|err| panic!("expected follow-up transcript to be readable: {err}"));
        let active_transcript_path = state
            .active_timeout_bundle
            .as_ref()
            .map(|active| active.paths.transcript.clone())
            .expect("expected timed-out follow-up to install a new active timeout bundle");

        assert_ne!(
            follow_up_transcript_path, timeout_transcript_path,
            "expected the timed-out follow-up reply to use a new bundle path"
        );
        assert_eq!(
            active_transcript_path, follow_up_transcript_path,
            "expected the fresh follow-up turn to own the active timeout bundle"
        );
        assert!(
            timeout_transcript.contains("HEAD\nTAIL\n"),
            "expected detached timeout tail to stay on the previous timeout bundle path, got: {timeout_transcript:?}"
        );
        assert!(
            !timeout_transcript.contains("FOLLOW_UP_START"),
            "did not expect fresh follow-up output on the previous timeout bundle path: {timeout_transcript:?}"
        );
        assert!(
            follow_up_transcript.contains("FOLLOW_UP_START")
                && follow_up_transcript.contains("FOLLOW_UP_END"),
            "expected fresh follow-up output in the new timeout bundle, got: {follow_up_transcript:?}"
        );
        assert!(
            !follow_up_transcript.contains("TAIL\n"),
            "did not expect detached timeout tail in the fresh follow-up bundle: {follow_up_transcript:?}"
        );
    }

    #[test]
    fn disclosed_timeout_image_bundle_keeps_later_small_polls_incremental() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        for index in 0..super::IMAGE_OUTPUT_BUNDLE_THRESHOLD {
            let image = ReplyImage {
                data: base64::engine::general_purpose::STANDARD.encode([index as u8]),
                mime_type: "image/png".to_string(),
                is_new: true,
            };
            let retained = bundle
                .append_image(&mut state.output_store, &image)
                .expect("timeout image should append");
            assert!(matches!(retained, Some(ReplyItem::Image(_))));
        }
        bundle.disclosed = true;
        let transcript_path = bundle.paths.transcript.clone();
        state.active_timeout_bundle = Some(bundle);

        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout("TAIL\n".to_string())],
                None,
            )),
            false,
            TimeoutBundleReuse::FullReply,
            0,
        );

        let text = result_text(&result);
        let images = result_images(&result);
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected timeout transcript to be readable: {err}"));

        assert!(
            disclosed_path(&text, "events.log").is_some()
                || disclosed_path(&text, "transcript.txt").is_some(),
            "expected later small poll to keep disclosing the existing bundle path, got: {text:?}"
        );
        assert!(
            images.is_empty(),
            "did not expect anchor images on later small poll"
        );
        assert!(
            text.contains("TAIL\n"),
            "expected later small poll to keep the new text visible, got: {text:?}"
        );
        assert!(
            transcript.contains("TAIL\n"),
            "expected later small poll output to append to the existing timeout bundle, got: {transcript:?}"
        );
    }

    #[test]
    fn disclosed_timeout_image_bundle_spills_later_oversized_text_to_existing_bundle() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        for index in 0..super::IMAGE_OUTPUT_BUNDLE_THRESHOLD {
            let image = ReplyImage {
                data: base64::engine::general_purpose::STANDARD.encode([index as u8]),
                mime_type: "image/png".to_string(),
                is_new: true,
            };
            let retained = bundle
                .append_image(&mut state.output_store, &image)
                .expect("timeout image should append");
            assert!(matches!(retained, Some(ReplyItem::Image(_))));
        }
        bundle.disclosed = true;
        let transcript_path = bundle.paths.transcript.clone();
        state.active_timeout_bundle = Some(bundle);

        let oversized = (0..1200)
            .map(|index| format!("line{index:04}\n"))
            .collect::<String>();
        assert!(
            super::text_should_spill(oversized.chars().count()),
            "expected later timeout text to cross the hard spill threshold"
        );

        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout(oversized.clone())],
                None,
            )),
            false,
            TimeoutBundleReuse::FullReply,
            0,
        );

        let text = result_text(&result);
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected timeout transcript to be readable: {err}"));

        assert!(
            disclosed_path(&text, "events.log").is_some(),
            "expected later oversized poll to keep disclosing the existing bundle path, got: {text:?}"
        );
        assert!(
            !text.contains("line0600\n"),
            "did not expect a middle line from the oversized text to stay inline, got: {text:?}"
        );
        assert!(
            transcript.contains("line0600\n"),
            "expected the existing bundle transcript to receive the oversized text, got: {transcript:?}"
        );
    }

    #[test]
    fn disclosed_detached_prefix_bundle_survives_timeout_follow_up_quota_pressure() {
        let mut state = ResponseState::new().expect("response state should initialize");
        state.output_store.limits.max_bundle_count = 1;
        let detached_prefix = format!(
            "IDLE_START\n{}\nIDLE_END\n",
            "d".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(detached_prefix),
                    WorkerContent::worker_stdout("FOLLOW_UP_TIMEOUT\n"),
                ],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let text = result_text(&result);
        let transcript_path = disclosed_path(&text, "transcript.txt")
            .unwrap_or_else(|| panic!("expected detached-prefix transcript path, got: {text:?}"));
        let transcript = fs::read_to_string(&transcript_path).unwrap_or_else(|err| {
            panic!("expected disclosed detached-prefix path to stay readable: {err}")
        });

        assert!(
            transcript.contains("IDLE_START") && transcript.contains("IDLE_END"),
            "expected detached-prefix transcript to survive same-reply timeout allocation, got: {transcript:?}"
        );
        assert!(
            !transcript.contains("FOLLOW_UP_TIMEOUT"),
            "did not expect fresh follow-up output in detached-prefix transcript: {transcript:?}"
        );
    }

    #[test]
    fn hidden_timeout_bundle_survives_server_only_follow_up_until_later_spill() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let first = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout("HEAD\n")],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::None,
            0,
        );
        assert!(
            disclosed_path(&result_text(&first), "transcript.txt").is_none(),
            "did not expect the initial timed-out reply to disclose a bundle path"
        );
        assert!(
            state.has_active_timeout_bundle(),
            "expected the timed-out reply to keep an active timeout bundle"
        );

        let second = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::server_stdout("<<repl status: busy>>\n"),
                    WorkerContent::worker_stdout("FOLLOW_UP_INLINE\n"),
                ],
                None,
            )),
            true,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );
        let second_text = result_text(&second);
        assert!(
            second_text.contains("FOLLOW_UP_INLINE"),
            "expected the follow-up reply to stay inline, got: {second_text:?}"
        );
        assert!(
            state.has_active_timeout_bundle(),
            "expected a server-only detached prefix to preserve the hidden timeout bundle"
        );

        let detached_prefix = format!(
            "TAIL_START\n{}\nTAIL_END\n",
            "z".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let third = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(detached_prefix),
                    WorkerContent::worker_stdout("DONE\n"),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let third_text = result_text(&third);
        let transcript_path = disclosed_path(&third_text, "transcript.txt").unwrap_or_else(|| {
            panic!("expected the later follow-up spill to disclose a transcript path, got: {third_text:?}")
        });
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected transcript to be readable: {err}"));

        assert!(
            transcript.contains("HEAD\nTAIL_START\n") && transcript.contains("TAIL_END\n"),
            "expected the preserved timeout bundle to keep the earlier timed-out bytes, got: {transcript:?}"
        );
        assert!(
            !transcript.contains("DONE\n"),
            "did not expect fresh follow-up output in the detached-prefix transcript: {transcript:?}"
        );
    }

    #[test]
    fn hidden_timeout_bundle_survives_worker_follow_up_until_later_spill() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let first = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout("HEAD\n")],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::None,
            0,
        );
        assert!(
            disclosed_path(&result_text(&first), "transcript.txt").is_none(),
            "did not expect the initial timed-out reply to disclose a bundle path"
        );
        assert!(
            state.has_active_timeout_bundle(),
            "expected the timed-out reply to keep hidden timeout state"
        );

        let second = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout("MID\n"),
                    WorkerContent::server_stdout("<<repl status: busy>>\n"),
                ],
                None,
            )),
            true,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );
        let second_text = result_text(&second);
        assert!(
            second_text.contains("MID\n"),
            "expected the small busy follow-up to keep worker output inline, got: {second_text:?}"
        );
        assert!(
            disclosed_path(&second_text, "transcript.txt").is_none(),
            "did not expect the small busy follow-up to spill yet, got: {second_text:?}"
        );
        assert!(
            state.has_active_timeout_bundle(),
            "expected hidden timeout state to survive the small worker detached prefix"
        );

        let detached_prefix = format!(
            "TAIL_START\n{}\nTAIL_END\n",
            "z".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let third = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(detached_prefix),
                    WorkerContent::worker_stdout("DONE\n"),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let third_text = result_text(&third);
        let transcript_path = disclosed_path(&third_text, "transcript.txt").unwrap_or_else(|| {
            panic!("expected the later spill to disclose a transcript path, got: {third_text:?}")
        });
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected transcript to be readable: {err}"));

        assert!(
            transcript.contains("HEAD\nMID\nTAIL_START\n") && transcript.contains("TAIL_END\n"),
            "expected the later spill to backfill both earlier timeout chunks, got: {transcript:?}"
        );
        assert!(
            !transcript.contains("DONE\n"),
            "did not expect fresh follow-up output in the detached-prefix transcript: {transcript:?}"
        );
    }

    #[test]
    fn timeout_bundle_spills_once_cumulative_staged_text_crosses_threshold() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let chunk_body = "x".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD / 2);
        let first_chunk = format!("FIRST_START\n{chunk_body}\nFIRST_END\n");
        let second_chunk = format!("SECOND_START\n{chunk_body}\nSECOND_END\n");

        assert!(
            !super::text_should_spill(first_chunk.chars().count()),
            "expected each staged timeout chunk to stay under the spill threshold"
        );
        assert!(
            super::text_should_spill(
                first_chunk
                    .chars()
                    .count()
                    .saturating_add(second_chunk.chars().count())
            ),
            "expected staged timeout chunks to exceed the spill threshold cumulatively"
        );

        let first = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout(first_chunk.clone())],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FullReply,
            0,
        );
        let first_text = result_text(&first);
        assert!(
            disclosed_path(&first_text, "transcript.txt").is_none(),
            "did not expect the first under-threshold timeout poll to disclose a bundle path"
        );
        assert!(
            state.has_active_timeout_bundle(),
            "expected the first under-threshold timeout poll to retain hidden timeout state"
        );

        let second = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout(second_chunk.clone())],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FullReply,
            0,
        );
        let second_text = result_text(&second);
        let transcript_path = disclosed_path(&second_text, "transcript.txt").unwrap_or_else(|| {
            panic!("expected the cumulative timeout poll to disclose a transcript path, got: {second_text:?}")
        });
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected transcript to be readable: {err}"));

        assert!(
            transcript.contains("FIRST_START") && transcript.contains("FIRST_END"),
            "expected the disclosed timeout bundle to backfill the first staged chunk, got: {transcript:?}"
        );
        assert!(
            transcript.contains("SECOND_START") && transcript.contains("SECOND_END"),
            "expected the disclosed timeout bundle to include the later poll chunk, got: {transcript:?}"
        );
    }

    #[test]
    fn server_only_timeout_poll_does_not_accumulate_in_staged_timeout_state() {
        let mut state = ResponseState::new().expect("response state should initialize");

        let first = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout("HEAD\n")],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FullReply,
            0,
        );
        let first_text = result_text(&first);
        assert!(
            first_text.contains("HEAD\n"),
            "expected the initial timed-out reply to stay inline, got: {first_text:?}"
        );
        let staged = state
            .staged_timeout_output
            .as_ref()
            .expect("expected the initial timed-out reply to retain staged timeout state");
        assert_eq!(staged.items.len(), 1);
        assert!(matches!(
            staged.items.first(),
            Some(ReplyItem::WorkerText { text, .. }) if text == "HEAD\n"
        ));

        let second = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::server_stdout("<<repl status: busy>>\n")],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FullReply,
            0,
        );
        let second_text = result_text(&second);
        assert!(
            second_text.contains("<<repl status: busy>>"),
            "expected the server-only timeout poll to stay inline, got: {second_text:?}"
        );

        let staged = state
            .staged_timeout_output
            .as_ref()
            .expect("expected staged timeout state to survive the server-only poll");
        assert_eq!(
            staged.items.len(),
            1,
            "did not expect server-only timeout status text to accumulate in staged timeout state"
        );
        assert!(matches!(
            staged.items.first(),
            Some(ReplyItem::WorkerText { text, .. }) if text == "HEAD\n"
        ));
    }

    #[test]
    fn disclosed_timeout_bundle_survives_busy_follow_up_until_later_poll() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        bundle
            .append_worker_text(&mut state.output_store, "HEAD\n", TextStream::Stdout)
            .expect("existing timeout text should append");
        bundle.disclosed = true;
        let transcript_path = bundle.paths.transcript.clone();
        state.active_timeout_bundle = Some(bundle);

        let second = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::server_stdout("<<repl status: busy>>\n")],
                None,
            )),
            true,
            TimeoutBundleReuse::FollowUpInput,
            0,
        );
        let second_text = result_text(&second);
        assert!(
            second_text.contains("<<repl status: busy>>"),
            "expected a busy follow-up marker, got: {second_text:?}"
        );
        assert!(
            state.has_active_timeout_bundle(),
            "expected the disclosed timeout bundle to stay active through the busy follow-up"
        );

        let third = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout("TAIL\n")],
                None,
            )),
            false,
            TimeoutBundleReuse::FullReply,
            0,
        );
        let third_text = result_text(&third);
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected timeout transcript to be readable: {err}"));

        assert!(
            third_text.contains("TAIL\n"),
            "expected the later poll to keep its small reply inline, got: {third_text:?}"
        );
        assert!(
            transcript.contains("HEAD\nTAIL\n"),
            "expected the disclosed timeout bundle to keep appending after the busy follow-up, got: {transcript:?}"
        );
    }

    #[test]
    fn disclosed_timeout_bundle_survives_timeout_coded_busy_follow_up_until_later_poll() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        bundle
            .append_worker_text(&mut state.output_store, "HEAD\n", TextStream::Stdout)
            .expect("existing timeout text should append");
        bundle.disclosed = true;
        let transcript_path = bundle.paths.transcript.clone();
        state.active_timeout_bundle = Some(bundle);

        let second = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::server_stdout("<<repl status: busy>>\n")],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FollowUpInput,
            0,
        );
        let second_text = result_text(&second);
        assert!(
            second_text.contains("<<repl status: busy>>"),
            "expected a busy follow-up marker, got: {second_text:?}"
        );
        assert!(
            state.has_active_timeout_bundle(),
            "expected timeout-coded busy follow-up replies to keep the disclosed timeout bundle active"
        );

        let third = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout("TAIL\n")],
                None,
            )),
            false,
            TimeoutBundleReuse::FullReply,
            0,
        );
        let third_text = result_text(&third);
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected timeout transcript to be readable: {err}"));

        assert!(
            third_text.contains("TAIL\n"),
            "expected the later poll to keep its small reply inline, got: {third_text:?}"
        );
        assert!(
            transcript.contains("HEAD\nTAIL\n"),
            "expected timeout-coded busy follow-up replies to keep appending to the existing timeout bundle, got: {transcript:?}"
        );
    }

    #[test]
    fn follow_up_bundle_does_not_prune_disclosed_timeout_bundle() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        bundle
            .append_worker_text(&mut state.output_store, "HEAD\n", TextStream::Stdout)
            .expect("existing timeout text should append");
        bundle.disclosed = true;
        let timeout_transcript_path = bundle.paths.transcript.clone();
        state.output_store.limits.max_bundle_count = 1;
        state.active_timeout_bundle = Some(bundle);

        let large_follow_up = format!(
            "FOLLOW_UP_START\n{}\nFOLLOW_UP_END\n",
            "z".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout("TAIL\n"),
                    WorkerContent::worker_stdout(large_follow_up),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let text = result_text(&result);
        let timeout_transcript =
            fs::read_to_string(&timeout_transcript_path).unwrap_or_else(|err| {
                panic!("expected timeout transcript to survive quota fallback: {err}")
            });

        assert!(
            timeout_transcript.contains("HEAD\nTAIL\n"),
            "expected timeout transcript to remain readable after follow-up compaction fallback, got: {timeout_transcript:?}"
        );
        assert!(
            !timeout_transcript.contains("FOLLOW_UP_START"),
            "did not expect fresh follow-up output in the preserved timeout transcript: {timeout_transcript:?}"
        );
        assert!(
            disclosed_path(&text, "transcript.txt").is_none(),
            "did not expect a fresh follow-up bundle path when the active timeout bundle is the only quota slot: {text:?}"
        );
    }

    #[test]
    fn small_timeout_does_not_prune_disclosed_bundle_before_any_new_disclosure() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("disclosed bundle should initialize");
        bundle
            .append_worker_text(&mut state.output_store, "PERSIST\n", TextStream::Stdout)
            .expect("disclosed bundle text should append");
        bundle.disclosed = true;
        let transcript_path = bundle.paths.transcript.clone();
        state.output_store.limits.max_bundle_count = 1;

        let timed_out = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout("HEAD\n")],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::None,
            0,
        );
        let timed_out_text = result_text(&timed_out);

        assert!(
            timed_out_text.contains("HEAD\n"),
            "expected the timed-out reply to stay inline, got: {timed_out_text:?}"
        );
        assert!(
            disclosed_path(&timed_out_text, "transcript.txt").is_none(),
            "did not expect the timed-out reply to disclose a new bundle path, got: {timed_out_text:?}"
        );
        let persisted_after_timeout = fs::read_to_string(&transcript_path).unwrap_or_else(|err| {
            panic!("expected disclosed bundle path to survive timeout: {err}")
        });

        let completed = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout("DONE\n")],
                None,
            )),
            false,
            TimeoutBundleReuse::FullReply,
            0,
        );
        let completed_text = result_text(&completed);
        let persisted_after_completion =
            fs::read_to_string(&transcript_path).unwrap_or_else(|err| {
                panic!("expected disclosed bundle path to survive inline completion: {err}")
            });

        assert!(
            completed_text.contains("DONE\n"),
            "expected the completion poll to stay inline, got: {completed_text:?}"
        );
        assert!(
            disclosed_path(&completed_text, "transcript.txt").is_none(),
            "did not expect the completion poll to disclose a new bundle path, got: {completed_text:?}"
        );
        assert!(
            persisted_after_timeout.contains("PERSIST\n"),
            "expected the original disclosed bundle content to remain readable after timeout, got: {persisted_after_timeout:?}"
        );
        assert_eq!(
            persisted_after_completion, persisted_after_timeout,
            "did not expect the original disclosed bundle to change when the hidden timeout state never spilled"
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
            TimeoutBundleReuse::None,
            0,
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

    #[test]
    fn timeout_busy_marker_survives_bundle_quota_truncation() {
        let mut state = ResponseState::new().expect("response state should initialize");
        state.output_store.limits.max_bundle_bytes = 2048;
        let oversized_text = format!(
            "START\n{}\nEND\n",
            "q".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let busy_marker = "<<repl status: busy, write_stdin timeout reached; elapsed_ms=50>>";
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(oversized_text),
                    WorkerContent::server_stdout(busy_marker),
                ],
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::None,
            0,
        );

        let text = result_text(&result);
        let transcript_path = disclosed_path(&text, "transcript.txt").unwrap_or_else(|| {
            panic!("expected transcript path in timed-out reply, got: {text:?}")
        });
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected timeout transcript to be readable: {err}"));

        assert!(
            text.contains("later content omitted"),
            "expected omission notice after bundle cap, got: {text:?}"
        );
        assert!(
            text.contains(busy_marker),
            "expected timeout busy marker to remain inline after bundle truncation, got: {text:?}"
        );
        assert!(
            !transcript.contains(busy_marker),
            "did not expect timeout busy marker in transcript bundle, got: {transcript:?}"
        );
    }

    #[test]
    fn mixed_timeout_bundle_events_log_excludes_server_status_lines() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let busy_marker = "<<repl status: busy>>\n";
        let mut contents = vec![WorkerContent::worker_stdout("HEAD\n")];
        contents.extend((0..super::IMAGE_OUTPUT_BUNDLE_THRESHOLD).map(|index| {
            WorkerContent::ContentImage {
                data: base64::engine::general_purpose::STANDARD.encode([index as u8]),
                mime_type: "image/png".to_string(),
                id: format!("plot-{index}"),
                is_new: true,
            }
        }));
        contents.push(WorkerContent::server_stdout(busy_marker));

        let result = state.finalize_worker_result(
            Ok(worker_reply(contents, Some(WorkerErrorCode::Timeout))),
            true,
            TimeoutBundleReuse::None,
            0,
        );

        let text = result_text(&result);
        let events_path = disclosed_path(&text, "events.log").unwrap_or_else(|| {
            panic!("expected events log path in mixed timeout reply, got: {text:?}")
        });
        let events = fs::read_to_string(&events_path)
            .unwrap_or_else(|err| panic!("expected events log to be readable: {err}"));

        assert!(
            text.contains(busy_marker),
            "expected busy marker to remain inline, got: {text:?}"
        );
        assert!(
            !events.contains(busy_marker),
            "did not expect server-only busy marker in events.log, got: {events:?}"
        );
    }

    #[test]
    fn quota_truncated_active_image_bundle_keeps_inline_preview_compact() {
        let initial_image_count = super::IMAGE_OUTPUT_BUNDLE_THRESHOLD;
        let later_image_count = 5usize;
        let mut sizing_state = ResponseState::new().expect("response state should initialize");
        let mut sizing_bundle = sizing_state
            .output_store
            .new_bundle()
            .expect("bundle should initialize");
        for index in 0..initial_image_count {
            let retained = sizing_bundle
                .append_image(
                    &mut sizing_state.output_store,
                    &ReplyImage {
                        data: base64::engine::general_purpose::STANDARD.encode([index as u8]),
                        mime_type: "image/png".to_string(),
                        is_new: true,
                    },
                )
                .expect("initial image should append");
            assert!(matches!(retained, Some(ReplyItem::Image(_))));
        }
        let current_bytes = sizing_state
            .output_store
            .bundle_bytes(sizing_bundle.id)
            .expect("bundle metadata should exist");
        for offset in 0..3 {
            let retained = sizing_bundle
                .append_image(
                    &mut sizing_state.output_store,
                    &ReplyImage {
                        data: base64::engine::general_purpose::STANDARD
                            .encode([(initial_image_count + offset) as u8]),
                        mime_type: "image/png".to_string(),
                        is_new: true,
                    },
                )
                .expect("sizing image should append");
            assert!(matches!(retained, Some(ReplyItem::Image(_))));
        }
        let quota_cap = sizing_state
            .output_store
            .bundle_bytes(sizing_bundle.id)
            .expect("bundle metadata should exist after sizing");
        assert!(
            quota_cap > current_bytes,
            "expected sizing pass to consume additional bundle bytes"
        );

        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("bundle should initialize");
        for index in 0..initial_image_count {
            let retained = bundle
                .append_image(
                    &mut state.output_store,
                    &ReplyImage {
                        data: base64::engine::general_purpose::STANDARD.encode([index as u8]),
                        mime_type: "image/png".to_string(),
                        is_new: true,
                    },
                )
                .expect("initial image should append");
            assert!(matches!(retained, Some(ReplyItem::Image(_))));
        }
        bundle.disclosed = true;
        state.output_store.limits.max_bundle_bytes = quota_cap;
        state.active_timeout_bundle = Some(bundle);

        let result = state.finalize_worker_result(
            Ok(worker_reply(
                (0..later_image_count)
                    .map(|offset| WorkerContent::ContentImage {
                        data: base64::engine::general_purpose::STANDARD
                            .encode([(initial_image_count + offset) as u8]),
                        mime_type: "image/png".to_string(),
                        id: format!("later-{offset}"),
                        is_new: true,
                    })
                    .collect(),
                Some(WorkerErrorCode::Timeout),
            )),
            true,
            TimeoutBundleReuse::FullReply,
            0,
        );

        let text = result_text(&result);
        let images = result_images(&result);

        assert!(
            text.contains("later content omitted"),
            "expected quota-truncated image bundle to report omitted content, got: {text:?}"
        );
        assert!(
            !images.is_empty() && images.len() <= 2,
            "expected quota-truncated image bundle to keep only compact anchor previews, got {} images",
            images.len()
        );
    }

    #[test]
    fn worker_error_clears_active_timeout_bundle() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let bundle = state
            .output_store
            .new_bundle()
            .expect("timeout bundle should initialize");
        let bundle_dir = bundle.paths.dir.clone();
        state.active_timeout_bundle = Some(bundle);
        assert!(
            state.has_active_timeout_bundle(),
            "expected test setup to install an active timeout bundle"
        );

        let result = state.finalize_worker_result(
            Err(WorkerError::Protocol(
                "simulated worker failure".to_string(),
            )),
            false,
            TimeoutBundleReuse::FullReply,
            0,
        );

        let text = result_text(&result);
        assert!(
            text.contains("simulated worker failure"),
            "expected worker error text, got: {text:?}"
        );
        assert!(
            state.active_timeout_bundle.is_none(),
            "expected worker error to clear the active timeout bundle"
        );
        assert!(
            state.output_store.bundles.is_empty(),
            "expected worker error to drop the hidden timeout bundle"
        );
        assert!(
            !bundle_dir.exists(),
            "expected dropped timeout bundle directory to be removed: {bundle_dir:?}"
        );
    }

    #[test]
    fn text_spill_append_failure_cleans_up_undisclosed_bundle() {
        let mut state = ResponseState::new().expect("response state should initialize");
        state.output_store.create_root = output_store_root_with_text_conflict;

        let oversized_text = format!(
            "START{}END",
            "a".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::worker_stdout(oversized_text)],
                None,
            )),
            false,
            TimeoutBundleReuse::None,
            0,
        );

        let text = result_text(&result);
        assert!(
            text.contains("output bundle unavailable"),
            "expected inline fallback after append failure, got: {text:?}"
        );
        assert!(
            state.output_store.bundles.is_empty(),
            "expected failed text bundle append to remove the undisclosed bundle"
        );
    }

    #[test]
    fn detached_prefix_text_spill_append_failure_returns_pathless_truncated_reply() {
        let mut state = ResponseState::new().expect("response state should initialize");
        state.output_store.create_root = output_store_root_with_text_conflict;

        let detached_prefix = format!(
            "HEAD\n{}\nMIDDLE\n{}\nTAIL\n",
            "a".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200),
            "b".repeat(super::INLINE_TEXT_HARD_SPILL_THRESHOLD + 200)
        );
        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![
                    WorkerContent::worker_stdout(detached_prefix),
                    WorkerContent::worker_stdout("FOLLOW_UP_OK\n"),
                ],
                None,
            )),
            false,
            TimeoutBundleReuse::FollowUpInput,
            1,
        );

        let text = result_text(&result);
        assert!(
            text.contains("output bundle unavailable"),
            "expected truncated detached-prefix fallback when bundle append fails, got: {text:?}"
        );
        assert!(
            !text.contains("/transcript.txt") && !text.contains("/events.log"),
            "did not expect bundle path in detached-prefix fallback reply, got: {text:?}"
        );
        assert!(
            text.contains("HEAD\n") && text.contains("TAIL\n"),
            "expected truncated detached-prefix preview in fallback reply, got: {text:?}"
        );
        assert!(
            !text.contains("MIDDLE"),
            "did not expect the full detached-prefix transcript inline after append failure, got: {text:?}"
        );
        assert!(
            text.contains("FOLLOW_UP_OK"),
            "expected follow-up reply inline after detached-prefix append failure, got: {text:?}"
        );
    }

    #[test]
    fn image_bundle_append_failure_cleans_up_undisclosed_bundle() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let contents = (0..super::IMAGE_OUTPUT_BUNDLE_THRESHOLD)
            .map(|index| WorkerContent::ContentImage {
                data: "!not-base64!".to_string(),
                mime_type: "image/png".to_string(),
                id: format!("image-{index}"),
                is_new: true,
            })
            .collect();

        let result = state.finalize_worker_result(
            Ok(worker_reply(contents, None)),
            false,
            TimeoutBundleReuse::None,
            0,
        );

        let text = result_text(&result);
        assert!(
            text.contains("output bundle unavailable"),
            "expected inline fallback after image bundle append failure, got: {text:?}"
        );
        assert!(
            state.output_store.bundles.is_empty(),
            "expected failed image bundle append to remove the undisclosed bundle"
        );
    }

    #[test]
    fn removing_missing_bundle_dir_still_releases_quota() {
        let mut store = OutputStore::new().expect("output store should initialize");
        let mut bundle = store.new_bundle().expect("bundle should initialize");
        bundle
            .append_worker_text(&mut store, "quota", TextStream::Stdout)
            .expect("worker text should append");
        let bundle_id = bundle.id;
        let bundle_dir = bundle.paths.dir.clone();
        let bytes_before = store
            .bundle_bytes(bundle_id)
            .expect("bundle metadata should exist before removal");
        assert!(bytes_before > 0, "expected test bundle to consume quota");

        fs::remove_dir_all(&bundle_dir).expect("bundle dir should be removable");
        store
            .remove_bundle(bundle_id)
            .expect("missing bundle dir should still clean up metadata");

        assert_eq!(
            store.total_bytes, 0,
            "expected quota accounting to be released"
        );
        assert!(
            store.bundle_bytes(bundle_id).is_none(),
            "expected bundle metadata to be removed after cleanup"
        );
    }

    #[test]
    fn single_image_update_bundle_uses_final_image_as_inline_anchor() {
        let mut store = OutputStore::new().expect("output store should initialize");
        let mut bundle = store.new_bundle().expect("bundle should initialize");
        let first_bytes = vec![0_u8];
        let last_bytes = vec![1_u8];
        let first = ReplyImage {
            data: base64::engine::general_purpose::STANDARD.encode(&first_bytes),
            mime_type: "image/png".to_string(),
            is_new: true,
        };
        let last = ReplyImage {
            data: base64::engine::general_purpose::STANDARD.encode(&last_bytes),
            mime_type: "image/png".to_string(),
            is_new: false,
        };
        let retained_first = bundle
            .append_image(&mut store, &first)
            .expect("first image should append")
            .expect("first image should be retained");
        let retained_last = bundle
            .append_image(&mut store, &last)
            .expect("updated image should append")
            .expect("updated image should be retained");

        let result = super::finalize_batch(
            compact_output_bundle_items(&[retained_first, retained_last], &bundle),
            false,
        );
        let images = result_images(&result);

        assert_eq!(images.len(), 1, "expected exactly one inline anchor image");
        assert_eq!(
            images[0], last_bytes,
            "expected inline anchor to use the final image state"
        );
    }

    #[test]
    fn omission_recorded_stays_false_when_notice_cannot_be_written() {
        let mut store = OutputStore::new().expect("output store should initialize");
        let mut bundle = store.new_bundle().expect("bundle should initialize");

        let worker_text = bundle
            .append_worker_text(&mut store, "a", TextStream::Stdout)
            .expect("worker text should append");
        assert!(matches!(worker_text, Some(ReplyItem::WorkerText { text, .. }) if text == "a"));

        let image = ReplyImage {
            data: base64::engine::general_purpose::STANDARD.encode([0_u8]),
            mime_type: "image/png".to_string(),
            is_new: true,
        };
        let retained_image = bundle
            .append_image(&mut store, &image)
            .expect("image should append");
        assert!(matches!(retained_image, Some(ReplyItem::Image(_))));
        assert!(
            bundle.has_events_log(),
            "expected mixed bundle to materialize events.log"
        );

        let events_before =
            fs::read_to_string(&bundle.paths.events_log).expect("events log should exist");
        store.limits.max_bundle_bytes = store
            .bundle_bytes(bundle.id)
            .expect("bundle metadata should exist");

        bundle
            .apply_omission(&mut store)
            .expect("omission should degrade to inline state");

        let events_after =
            fs::read_to_string(&bundle.paths.events_log).expect("events log should still exist");
        assert!(bundle.omitted_tail, "expected omission state to be set");
        assert!(
            !bundle.omission_recorded,
            "did not expect omission row to be marked recorded when quota blocked the write"
        );
        assert_eq!(
            events_after, events_before,
            "did not expect events.log to change when omission row could not be appended"
        );
    }

    #[test]
    fn timeout_omission_without_worker_text_still_discloses_bundle() {
        let mut state = ResponseState::new().expect("response state should initialize");
        let mut bundle = state
            .output_store
            .new_bundle()
            .expect("bundle should initialize");
        let image = ReplyImage {
            data: base64::engine::general_purpose::STANDARD.encode([0_u8]),
            mime_type: "image/png".to_string(),
            is_new: true,
        };
        let retained_image = bundle
            .append_image(&mut state.output_store, &image)
            .expect("first image should append");
        assert!(matches!(retained_image, Some(ReplyItem::Image(_))));
        state.output_store.limits.max_bundle_bytes = state
            .output_store
            .bundle_bytes(bundle.id)
            .expect("bundle metadata should exist");
        let images_dir = bundle.paths.images_dir.clone();
        state.active_timeout_bundle = Some(bundle);

        let result = state.finalize_worker_result(
            Ok(worker_reply(
                vec![WorkerContent::ContentImage {
                    data: base64::engine::general_purpose::STANDARD.encode([1_u8]),
                    mime_type: "image/png".to_string(),
                    id: "image-2".to_string(),
                    is_new: true,
                }],
                Some(WorkerErrorCode::Timeout),
            )),
            false,
            TimeoutBundleReuse::FullReply,
            0,
        );

        let text = result_text(&result);
        assert!(
            text.contains("later content omitted"),
            "expected omission notice even without worker text, got: {text:?}"
        );
        assert!(
            text.contains(images_dir.to_string_lossy().as_ref()),
            "expected omission notice to disclose bundle path, got: {text:?}"
        );
    }

    #[test]
    fn strip_text_stream_meta_removes_internal_marker() {
        let mut result = super::finalize_batch(
            vec![super::content_text(
                "stderr: boom\n".to_string(),
                TextStream::Stderr,
            )],
            false,
        );

        assert_eq!(
            super::text_stream_from_content(&result.content[0]),
            Some(TextStream::Stderr)
        );

        super::strip_text_stream_meta(&mut result);

        assert_eq!(super::text_stream_from_content(&result.content[0]), None);
        let value = serde_json::to_value(&result).expect("result should serialize");
        assert!(
            !value.to_string().contains(super::TEXT_STREAM_META_KEY),
            "did not expect stripped result to expose internal stream metadata: {value}"
        );
    }
}

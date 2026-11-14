//! Bounded ordered text projection, independent of raw-file retention.

use super::{Content, utf8_prefix_length};
use std::sync::Arc;

/// Complete rendered tool-result text, including all Console notices.
pub(crate) const TEXT_BYTES: usize = 8 * 1024;
const COLLECT_BYTES: usize = 2 * TEXT_BYTES;
const CONTROL_BYTES: usize = TEXT_BYTES / 2;
pub(super) const IMAGE_BYTES: usize = 8 * 1024 * 1024;
const IMAGE_METADATA_BYTES: usize = 64 * 1024;
const IMAGE_EVENTS: usize = 4096;

mod source;
pub(super) use source::Source;
use source::{Location, Summary};

#[derive(Clone)]
pub(super) enum Part {
    Text(String),
    Gap(Gap),
    Notice(Control),
    Information(String),
    // Ephemeral text projections share the retained image until MCP rendering.
    Image(Arc<Content>),
    ImageGap,
    Source(Source),
    Summary(Summary),
}

#[derive(Clone, Copy, Default)]
pub(super) struct Gap {
    bytes: u64,
    notices: u64,
}

#[derive(Clone, Default)]
pub(super) struct Preview {
    pub(super) parts: Vec<Part>,
    collected_bytes: usize,
    image_bytes: usize,
    image_metadata_bytes: usize,
    image_events: usize,
    omitted_images: u64,
    omitted_image_bytes: u64,
    attributed_omitted_images: u64,
    omitted_image_owner: Option<crate::transcript::Owner>,
    recorded_omitted_images: u64,
    omitted_artifacts: Location,
    omitted_controls: u64,
    omitted_control_bytes: u64,
    /// The outcome of the reply's bounded wait for the recording writer.
    writer: crate::transcript::WriterState,
}

#[derive(Clone, Copy)]
enum ProjectionMode {
    Measure,
    Inspect,
    Record,
}

impl Preview {
    pub(super) fn inspection_copy(&self) -> Self {
        let mut preview = self.clone();
        for part in &mut preview.parts {
            if let Part::Source(source) = part {
                source.file = source
                    .file
                    .as_ref()
                    .map(crate::transcript::OutputRecord::inspection_copy);
            }
        }
        preview
    }

    /// Whether the bounded rendering carries an omission marker that names
    /// retained files a reader may open. Omitted output that no file retained
    /// does not count: the recording writer cannot change what its marker
    /// says. `phase` is the phase notice the reply will add, with whether it
    /// joins the cell clock.
    pub(super) fn cites_recording(&self, phase: Option<(&str, bool)>) -> bool {
        let mut probe = self.inspection_copy();
        probe.bound();
        if let Some((phase, cell_clock)) = phase {
            probe.phase_before_terminal(phase, cell_clock);
            probe.bound();
        }
        probe.omission().cites(&probe.omitted_artifacts)
    }

    /// The combined accounting of every omitted text interval.
    fn omission(&self) -> Summary {
        let mut summary = Summary::default();
        let mut gap = Gap::default();
        for part in &self.parts {
            match part {
                Part::Gap(omitted) => {
                    gap.bytes += omitted.bytes;
                    gap.notices += omitted.notices;
                }
                Part::Source(source) => {
                    if gap.bytes != 0 {
                        summary.gap.bytes += gap.bytes;
                        summary.gap.notices += gap.notices;
                        summary.source(source);
                    }
                    gap = Gap::default();
                }
                Part::Summary(omitted) => summary.extend(omitted.clone()),
                _ => {}
            }
        }
        if gap.bytes != 0 {
            summary.gap.bytes += gap.bytes;
            summary.gap.notices += gap.notices;
            summary.source(&Source::default());
        }
        summary
    }

    /// Apply the outcome of the reply's wait for the recording writer: place
    /// each write failure the writer reported before the first interval of the
    /// file it truncated, and let the omission marker qualify the files it
    /// names when the writer is still behind or recording stopped.
    ///
    /// The writer hands over each report once. `recoverable` holds the copies
    /// that return an aggregate reply's sessions when it is not delivered, so
    /// they receive the same outcome as the reply.
    pub(super) fn recording_settled<'a>(
        &'a mut self,
        writer: crate::transcript::WriterState,
        recoverable: impl Iterator<Item = &'a mut Preview>,
    ) {
        let failures: Vec<_> = self
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Source(Source {
                    file: Some(file), ..
                }) => file.take_notice().map(|notice| (file.clone(), notice)),
                _ => None,
            })
            .collect();
        for preview in recoverable.chain(std::iter::once(self)) {
            preview.writer = writer;
            let mut failures: Vec<_> = failures.iter().collect();
            for part in std::mem::take(&mut preview.parts) {
                if let Part::Source(Source {
                    file: Some(file), ..
                }) = &part
                    && let Some(index) = failures
                        .iter()
                        .position(|(failed, _)| failed.same_file(file))
                {
                    let (_, notice) = failures.swap_remove(index);
                    preview.notice(format!("[{notice}]\n"));
                }
                preview.parts.push(part);
            }
        }
    }

    pub(super) fn phase_before_terminal(&mut self, phase: &str, cell_clock: bool) {
        if let Some(progress) = self.parts.iter_mut().rev().find_map(|part| match part {
            Part::Notice(control) if cell_clock && control.head.starts_with("\n[elapsed: ") => {
                Some(control)
            }
            _ => None,
        }) {
            assert!(
                progress.head.ends_with(']') && progress.tail.is_empty() && progress.omitted == 0
            );
            progress.head.pop();
            progress.head.push_str(&format!("; phase: {phase}]"));
            return;
        }
        let Some(Part::Notice(terminal)) = self.parts.pop() else {
            unreachable!("a phase response has a terminal banner");
        };
        let prefix = if self.ends_with_newline() { "" } else { "\n" };
        self.notice(format!("{prefix}[phase: {phase}]"));
        self.control(terminal);
    }

    pub(super) fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Never copy an oversized publication in full into retained state.
        if text.len() > COLLECT_BYTES {
            let head = utf8_prefix_length(text, TEXT_BYTES);
            let tail = suffix_start(text, TEXT_BYTES);
            self.push_text(&text[..head]);
            self.parts.push(Part::Gap(Gap {
                bytes: (tail - head) as u64,
                notices: 0,
            }));
            self.push_text(&text[tail..]);
        } else {
            self.push_text(text);
        }
        // Amortize compaction over a bounded batch of new text. Tiny writes
        // append in place; they do not recopy the retained head and tail.
        if self.collected_bytes > COLLECT_BYTES + TEXT_BYTES {
            self.trim(COLLECT_BYTES);
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.collected_bytes += text.len();
        if let Some(Part::Text(previous)) = self.parts.last_mut() {
            previous.push_str(text);
        } else {
            self.parts.push(Part::Text(text.to_owned()));
        }
    }

    pub(super) fn gap(&mut self, bytes: u64) {
        self.omitted(Gap { bytes, notices: 0 });
    }

    fn omitted(&mut self, gap: Gap) {
        if gap.bytes != 0 {
            if let Some(Part::Gap(previous)) = self.parts.last_mut() {
                previous.bytes += gap.bytes;
                previous.notices += gap.notices;
            } else {
                self.parts.push(Part::Gap(gap));
            }
        }
    }

    pub(super) fn notice(&mut self, text: String) {
        self.control(Control::new(text));
    }

    fn control(&mut self, mut control: Control) {
        // A sealed interval can begin with a notice before it is composed with
        // earlier output. Apply its line boundary at that composition point too.
        if !self.is_empty() && !self.ends_with_newline() && !control.head.starts_with('\n') {
            control.head.insert(0, '\n');
        }
        self.parts.push(Part::Notice(control));
        self.trim_controls(COLLECT_BYTES, TEXT_BYTES);
    }

    fn trim_controls(&mut self, limit: usize, each: usize) {
        // Repeated input reports may accumulate before a cut. Preserve the latest
        // active report and summarize older reports when their storage is exhausted.
        let mut remaining = limit;
        for part in self.parts.iter_mut().rev() {
            if let Part::Notice(control) = part {
                if !control.trim(remaining.min(each)) {
                    self.omitted_controls += 1;
                    self.omitted_control_bytes += control.bytes();
                    control.head.clear();
                    control.tail.clear();
                    control.omitted = 0;
                }
                remaining -= control.len();
            }
        }
        self.parts
            .retain(|part| !matches!(part, Part::Notice(control) if control.bytes() == 0));
    }

    pub(super) fn information(&mut self, text: String) {
        if text.len() > COLLECT_BYTES {
            self.omitted(Gap {
                bytes: text.len() as u64,
                notices: text.len() as u64,
            });
        } else {
            self.collected_bytes += text.len();
            self.parts.push(Part::Information(text));
            if self.collected_bytes > COLLECT_BYTES + TEXT_BYTES {
                self.trim(COLLECT_BYTES);
            }
        }
    }

    pub(super) fn admits_image(&self, data: &str, mime_type: &str) -> bool {
        self.image_events < IMAGE_EVENTS
            && data.len() <= IMAGE_BYTES.saturating_sub(self.image_bytes)
            && mime_type.len() <= IMAGE_METADATA_BYTES.saturating_sub(self.image_metadata_bytes)
    }

    pub(super) fn omit_image(&mut self, bytes: usize) {
        if self.omitted_images == 0 {
            self.parts.push(Part::ImageGap);
        }
        self.omitted_images = self.omitted_images.saturating_add(1);
        self.omitted_image_bytes = self.omitted_image_bytes.saturating_add(bytes as u64);
    }

    pub(super) fn image(&mut self, content: Arc<Content>) {
        let Content::Image {
            data,
            mime_type,
            artifact,
        } = content.as_ref()
        else {
            unreachable!("only image content enters the independent image budget")
        };
        if !self.admits_image(data, mime_type) {
            self.recorded_omitted_images += u64::from(artifact.is_some());
            if let Some(artifact) = artifact {
                self.omitted_artifacts.add_artifact(artifact);
            }
            self.omit_image(data.len());
            return;
        }
        self.image_bytes += data.len();
        self.image_metadata_bytes += mime_type.len();
        self.image_events += 1;
        self.parts.push(Part::Image(content));
    }

    pub(super) fn source(&mut self, mut source: Source) {
        // Attribute image omissions before empty image-only receipts retire.
        self.attribute_image_omissions(
            self.omitted_images - self.attributed_omitted_images,
            source.owner,
        );
        let interval = || {
            self.parts
                .iter()
                .rev()
                .take_while(|part| !matches!(part, Part::Source(_)))
        };
        let has_text = interval()
            .any(|part| matches!(part, Part::Text(_) | Part::Gap(_) | Part::Information(_)));
        if !has_text {
            // Notices and images need ownership, but no raw-text receipt. Release empty
            // logs so completion can publish without another poll.
            if let Some(file) = source.file.take() {
                file.publish();
            }
            if !interval()
                .any(|part| matches!(part, Part::Notice(_) | Part::Image(_) | Part::ImageGap))
            {
                return;
            }
        }
        self.parts.push(Part::Source(source));
        self.summarize_sources();
    }

    fn attribute_image_omissions(&mut self, count: u64, owner: Option<crate::transcript::Owner>) {
        if count == 0 {
            return;
        }
        if self.attributed_omitted_images == 0 {
            self.omitted_image_owner = owner;
        } else if self.omitted_image_owner != owner {
            self.omitted_image_owner = None;
        }
        self.attributed_omitted_images += count;
    }

    pub(super) fn extend(&mut self, other: Self) {
        for part in other.parts {
            match part {
                Part::Text(text) => self.text(&text),
                Part::Gap(gap) => self.omitted(gap),
                Part::Notice(control) => self.control(control),
                Part::Information(text) => self.information(text),
                Part::ImageGap => {
                    self.attribute_image_omissions(
                        other.attributed_omitted_images,
                        other.omitted_image_owner,
                    );
                    if self.omitted_images == 0 {
                        self.parts.push(Part::ImageGap);
                    }
                    self.omitted_images += other.omitted_images;
                    self.omitted_image_bytes += other.omitted_image_bytes;
                    self.recorded_omitted_images += other.recorded_omitted_images;
                    self.omitted_artifacts
                        .extend(other.omitted_artifacts.clone());
                }
                Part::Image(image) => self.image(image),
                Part::Source(source) => self.source(source),
                Part::Summary(summary) => self.parts.push(Part::Summary(summary)),
            }
        }
        self.omitted_controls += other.omitted_controls;
        self.omitted_control_bytes += other.omitted_control_bytes;
        self.summarize_sources();
    }

    pub(super) fn is_empty(&self) -> bool {
        self.parts
            .iter()
            .all(|part| matches!(part, Part::Source(_)))
            && self.omitted_images == 0
            && self.omitted_controls == 0
    }

    pub(super) fn ends_with_newline(&self) -> bool {
        match self.last_visible() {
            Some(Part::Text(text) | Part::Information(text)) => text.ends_with('\n'),
            Some(Part::Notice(control)) => control.ends_with_newline(),
            Some(Part::Summary(_)) => true,
            _ => false,
        }
    }

    /// Whether the last visible part is text that ends mid-line. A notice
    /// occupies its own line, so it never leaves a line open for later output.
    pub(super) fn ends_with_partial_text_line(&self) -> bool {
        matches!(
            self.last_visible(),
            Some(Part::Text(text) | Part::Information(text)) if !text.ends_with('\n')
        )
    }

    pub(super) fn last_visible(&self) -> Option<&Part> {
        self.parts
            .iter()
            .rev()
            .find(|part| !matches!(part, Part::Source(_)))
    }

    pub(super) fn starts_with_text(&self) -> bool {
        matches!(
            self.parts
                .iter()
                .find(|part| !matches!(part, Part::Source(_))),
            Some(Part::Text(_) | Part::Notice(_) | Part::Information(_) | Part::Gap(_))
        )
    }

    fn trim(&mut self, limit: usize) {
        let total: u64 = self
            .parts
            .iter()
            .map(|part| match part {
                Part::Text(text) | Part::Information(text) => text.len() as u64,
                Part::Gap(gap) => gap.bytes,
                Part::Summary(summary) => summary.gap.bytes,
                _ => 0,
            })
            .sum();
        if total <= limit as u64 {
            return;
        }
        let head_end = (limit / 2) as u64;
        let tail_start = total.saturating_sub((limit - limit / 2) as u64);
        let mut position = 0u64;
        let mut parts = Vec::new();
        for part in std::mem::take(&mut self.parts) {
            match part {
                Part::Text(text) => {
                    let head =
                        utf8_prefix_length(&text, head_end.saturating_sub(position) as usize);
                    let tail = if position + text.len() as u64 > tail_start {
                        suffix_start(&text, (position + text.len() as u64 - tail_start) as usize)
                    } else {
                        text.len()
                    }
                    .max(head);
                    if head != 0 {
                        parts.push(Part::Text(text[..head].to_owned()));
                    }
                    if tail > head {
                        parts.push(Part::Gap(Gap {
                            bytes: (tail - head) as u64,
                            notices: 0,
                        }));
                    }
                    if tail < text.len() {
                        parts.push(Part::Text(text[tail..].to_owned()));
                    }
                    position += text.len() as u64;
                }
                Part::Gap(gap) => {
                    position += gap.bytes;
                    parts.push(Part::Gap(gap));
                }
                Part::Summary(summary) => {
                    position += summary.gap.bytes;
                    parts.push(Part::Summary(summary));
                }
                Part::Information(text) => {
                    let end = position + text.len() as u64;
                    if end <= head_end || position >= tail_start {
                        parts.push(Part::Information(text));
                    } else {
                        parts.push(Part::Gap(Gap {
                            bytes: text.len() as u64,
                            notices: text.len() as u64,
                        }));
                    }
                    position = end;
                }
                other => parts.push(other),
            }
        }
        for part in parts {
            match (self.parts.last_mut(), part) {
                (Some(Part::Gap(previous)), Part::Gap(gap)) => {
                    previous.bytes += gap.bytes;
                    previous.notices += gap.notices;
                }
                (Some(Part::Text(previous)), Part::Text(text)) => previous.push_str(&text),
                (_, part) => self.parts.push(part),
            }
        }
        self.collected_bytes = self
            .parts
            .iter()
            .map(|part| match part {
                Part::Text(text) | Part::Information(text) => text.len(),
                _ => 0,
            })
            .sum();
        self.summarize_sources();
    }

    /// Fully omitted intervals no longer need live per-file receipts. Account
    /// them once, then keep one summary at the first such omission. Text-bearing
    /// intervals retain their own receipts so later trims can update their counts.
    fn summarize_sources(&mut self) {
        let mut start = 0;
        for end in 0..self.parts.len() {
            let Part::Source(source) = &self.parts[end] else {
                continue;
            };
            let parts = &self.parts[start..end];
            if !parts.iter().any(|part| {
                matches!(
                    part,
                    Part::Text(_) | Part::Information(_) | Part::Notice(_) | Part::Image(_)
                )
            }) {
                let mut gap = Gap::default();
                for part in parts {
                    if let Part::Gap(omitted) = part {
                        gap.bytes += omitted.bytes;
                        gap.notices += omitted.notices;
                    }
                }
                let mut summary = Summary::new(source, gap);
                for part in &mut self.parts[start..=end] {
                    if matches!(part, Part::Gap(_) | Part::Source(_)) {
                        *part = Part::Summary(std::mem::take(&mut summary));
                    }
                }
            }
            start = end + 1;
        }
        let mut combined = Summary::default();
        let mut first = None;
        for (index, part) in self.parts.iter_mut().enumerate() {
            if let Part::Summary(summary) = part {
                if summary.gap.bytes != 0 {
                    first.get_or_insert(index);
                }
                combined.extend(std::mem::take(summary));
            }
        }
        if let Some(first) = first {
            self.parts[first] = Part::Summary(combined);
        }
        self.parts
            .retain(|part| !matches!(part, Part::Summary(summary) if summary.gap.bytes == 0));
    }

    /// Reserve notices first, then divide ordinary text between its head and tail.
    pub(super) fn render_recorded(&mut self) -> (Vec<Content>, Vec<crate::transcript::ResultPart>) {
        self.bound();
        let projection = self.project(ProjectionMode::Record);
        (
            projection.content.expect("rendered projection"),
            projection.parts,
        )
    }

    /// Inspect the same bounded rendering without publishing receipts or
    /// accounting omissions as if a client had received the output. `writer`
    /// is what the recording already knows; see `Transcript::writer_state`.
    pub(super) fn render_preview(
        &mut self,
        writer: crate::transcript::WriterState,
    ) -> Vec<Content> {
        self.writer = writer;
        self.bound();
        self.project(ProjectionMode::Inspect)
            .content
            .expect("rendered preview")
    }

    pub(super) fn bound(&mut self) {
        let mut allowance = TEXT_BYTES;
        loop {
            self.trim(allowance);
            let mut bytes = self.project(ProjectionMode::Measure).text_bytes;
            if bytes > TEXT_BYTES {
                self.trim_controls(CONTROL_BYTES, TEXT_BYTES / 8);
                bytes = self.project(ProjectionMode::Measure).text_bytes;
            }
            if bytes <= TEXT_BYTES {
                return;
            }
            assert!(
                allowance > 0,
                "bounded response notices exceed the complete result budget"
            );
            // Reserve notices in small blocks so incidental path and count widths
            // do not continually move the visible text boundaries.
            allowance = allowance.saturating_sub(bytes - TEXT_BYTES) / 128 * 128;
        }
    }

    pub(super) fn publish_output_records(&self) {
        for part in &self.parts {
            if let Part::Source(Source {
                file: Some(file), ..
            }) = part
            {
                file.publish();
            }
        }
    }

    fn project(&self, mode: ProjectionMode) -> Projection {
        let account = matches!(mode, ProjectionMode::Record);
        let mut projection = Projection {
            content: (!matches!(mode, ProjectionMode::Measure)).then(Vec::new),
            text_bytes: 0,
            owner: None,
            parts: Vec::new(),
        };
        if self.omitted_controls != 0 {
            projection.text(&format!(
                    "[control preview: omitted {} earlier notices ({} rendered UTF-8 bytes); not retained]\n",
                    self.omitted_controls, self.omitted_control_bytes,
                ));
        }
        // Recompute after each trim. Sizing and delivery use exactly the same
        // combined notice, without retaining a list of omitted source paths.
        let mut summary = self.omission();
        let mut marker = (summary.gap.bytes != 0 || self.omitted_images != 0).then(|| {
            summary.notice(
                self.omitted_images,
                self.omitted_image_bytes,
                self.recorded_omitted_images,
                &self.omitted_artifacts,
                self.writer,
            )
        });
        // A combined omission can span cells or workers. Preserve one owner only
        // when the retained receipts establish it; never borrow a later source.
        if self.omitted_images != 0 {
            summary.source(&Source {
                owner: if self.attributed_omitted_images == self.omitted_images {
                    self.omitted_image_owner
                } else {
                    None
                },
                ..Source::default()
            });
        }
        let marker_owner = summary.owner;
        let mut image_marker = true;
        // Capture boundaries can split one interval into several receipts.
        // Coalesce adjacent gaps only when their interval ownership agrees.
        let mut at_gap = false;
        let mut gap_owner = None;
        let mut start = 0;
        for end in 0..=self.parts.len() {
            let source = match self.parts.get(end) {
                Some(Part::Source(source)) => source.clone(),
                None => Source::default(),
                _ => continue,
            };
            projection.owner = source.owner;
            let parts = &self.parts[start..end];
            let omitted: u64 = parts
                .iter()
                .map(|part| {
                    if let Part::Gap(gap) = part {
                        gap.bytes
                    } else {
                        0
                    }
                })
                .sum();
            let mut text_marker = true;
            for part in parts {
                match part {
                    Part::Text(text) | Part::Information(text) => {
                        projection.text(text);
                        at_gap = false;
                    }
                    Part::Notice(control) => {
                        projection.text(&control.render());
                        at_gap = false;
                    }
                    Part::Gap(_) => {
                        if text_marker && (!at_gap || gap_owner != source.owner) {
                            projection.owner = marker_owner;
                            projection.text(&marker.take().unwrap_or_else(|| "\n[…]\n".to_owned()));
                            projection.owner = source.owner;
                            at_gap = true;
                            gap_owner = source.owner;
                        }
                        text_marker = false;
                    }
                    Part::Summary(summary) => {
                        if !at_gap || gap_owner != summary.owner {
                            projection.owner = marker_owner;
                            projection.text(&marker.take().unwrap_or_else(|| "\n[…]\n".to_owned()));
                            projection.owner = source.owner;
                            at_gap = true;
                            gap_owner = summary.owner;
                        }
                    }
                    Part::Image(image) => {
                        at_gap = false;
                        if let Some(content) = &mut projection.content {
                            let owner = match image.as_ref() {
                                Content::Image {
                                    artifact: Some(artifact),
                                    ..
                                } => artifact.owner,
                                _ => projection.owner,
                            };
                            projection.parts.push(crate::transcript::ResultPart {
                                block: content.len(),
                                start: 0,
                                end: 0,
                                owner,
                            });
                            content.push(image.as_ref().clone());
                        }
                    }
                    Part::ImageGap => {
                        if image_marker && (!at_gap || gap_owner != source.owner) {
                            projection.owner = marker_owner;
                            projection.text(&marker.take().unwrap_or_else(|| "\n[…]\n".to_owned()));
                            projection.owner = source.owner;
                            at_gap = true;
                            gap_owner = source.owner;
                        }
                        image_marker = false;
                    }
                    Part::Source(_) => unreachable!(),
                }
            }
            if account && let Some(file) = &source.file {
                file.note_inline_omission(omitted);
            }
            start = end + 1;
        }
        if account {
            self.publish_output_records();
        }
        projection
    }
}

/// Sizing visits the same text and notices as rendering, without allocating
/// ordinary text blocks or cloning image payloads.
struct Projection {
    content: Option<Vec<Content>>,
    text_bytes: usize,
    owner: Option<crate::transcript::Owner>,
    parts: Vec<crate::transcript::ResultPart>,
}

impl Projection {
    fn text(&mut self, text: &str) {
        self.text_bytes += text.len();
        if let Some(content) = &mut self.content {
            if text.is_empty() {
                return;
            }
            let (block, start) = if let Some(Content::Text(previous)) = content.last_mut() {
                let start = previous.len();
                previous.push_str(text);
                (content.len() - 1, start)
            } else {
                content.push(Content::Text(text.to_owned()));
                (content.len() - 1, 0)
            };
            if let Some(previous) = self.parts.last_mut()
                && previous.block == block
                && previous.end == start
                && previous.owner == self.owner
            {
                previous.end += text.len();
            } else {
                self.parts.push(crate::transcript::ResultPart {
                    block,
                    start,
                    end: start + text.len(),
                    owner: self.owner,
                });
            }
        }
    }
}

pub(super) fn suffix_start(text: &str, limit: usize) -> usize {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

/// Retain the two ends of dynamic control details without losing omission
/// accounting when an unclaimed response is composed and bounded again.
#[derive(Clone, Default)]
pub(super) struct Control {
    head: String,
    tail: String,
    omitted: u64,
}

impl Control {
    fn new(text: String) -> Self {
        let mut control = Self {
            head: text,
            ..Self::default()
        };
        control.trim(TEXT_BYTES);
        control
    }

    fn bytes(&self) -> u64 {
        self.head.len() as u64 + self.tail.len() as u64 + self.omitted
    }

    fn marker(&self) -> String {
        if self.omitted == 0 {
            String::new()
        } else {
            format!(
                "[… omitted {} rendered UTF-8 bytes; not retained …]",
                self.omitted
            )
        }
    }

    fn render(&self) -> String {
        format!("{}{}{}", self.head, self.marker(), self.tail)
    }

    fn len(&self) -> usize {
        self.head.len() + self.marker().len() + self.tail.len()
    }

    fn ends_with_newline(&self) -> bool {
        if self.omitted == 0 {
            self.head.ends_with('\n')
        } else {
            self.tail.ends_with('\n')
        }
    }

    fn trim(&mut self, limit: usize) -> bool {
        if self.len() <= limit {
            return true;
        }
        let total = self.bytes();
        let mut omitted = total;
        loop {
            let marker = format!("[… omitted {omitted} rendered UTF-8 bytes; not retained …]");
            if marker.len() > limit {
                return false;
            }
            let budget = limit - marker.len();
            let head = utf8_prefix_length(&self.head, budget / 2);
            let tail_text = if self.omitted == 0 {
                &self.head
            } else {
                &self.tail
            };
            let tail = suffix_start(tail_text, budget - budget / 2);
            let next = total - head as u64 - (tail_text.len() - tail) as u64;
            if next == omitted {
                self.tail = tail_text[tail..].to_owned();
                // Drop the original allocation as well as its visible middle.
                self.head = self.head[..head].to_owned();
                self.omitted = omitted;
                return true;
            }
            omitted = next;
        }
    }
}

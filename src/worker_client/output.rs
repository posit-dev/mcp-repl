use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

mod events;
mod terminal;

mod preview;
mod tape;
use preview::{Part, Preview};
pub(super) use tape::CellEntryGap;
use tape::OutputTapeState;

pub(super) const WORKER_STARTING_NOTICE: &str = "starting new worker";
const WORKER_STARTING_STATE: &str = "worker starting";
pub(super) const WORKER_STOPPED_NOTICE: &str = "worker stopped: in-memory state lost";
pub(super) const WORKER_IDLE_NOTICE: &str = "idle";
pub(super) const EVALUATION_STOPPED_BY_RESTART_NOTICE: &str =
    "stopped by session restart request before evaluation finished";
pub(super) const ACTIVE_EVALUATION_STOPPED_NOTICE: &str =
    "active evaluation stopped by session restart request";

/// Stores pending session output in publication order until one response drains it.
#[derive(Clone)]
pub(super) struct OutputTape(Arc<Mutex<OutputTapeState>>, tokio::sync::watch::Sender<()>);

/// An opaque boundary between sealed response intervals.
#[derive(Clone, Copy)]
pub(super) struct OutputCut {
    sequence: u64,
    pub(super) worker_revision: u64,
}

pub(super) struct DirectOutput {
    output: OutputTape,
    stream: DirectOutputStream,
}

#[derive(Clone, Copy)]
enum DirectOutputStream {
    Stdout,
    Stderr,
}

#[derive(Default)]
pub(crate) struct Response {
    preview: Box<Preview>,
    is_error: bool,
    delivery: Option<Box<ResponseDeliveryTarget>>,
    terminal: Option<TerminalState>,
    phase: Option<super::status::Source>,
    /// The phase `settle_recording` read. The reply renders this one, so its
    /// omission marker is the one that wait examined.
    settled_phase: Option<Option<&'static str>>,
    execution_error: Option<Box<crate::worker_protocol::ExecutionError>>,
}

pub(super) enum ResponseAcknowledgment {
    Delivered,
    Unclaimed(Response),
}

enum ResponseDeliveryTarget {
    Evaluation(
        SyncSender<ResponseAcknowledgment>,
        Arc<tokio::sync::Notify>,
        OutputTape,
    ),
    Output(OutputTape),
}

/// Reports whether an assembled console response reached the MCP transport.
///
/// The bounded recovery response remains owned here after MCP projection so transport
/// cancellation or write failure can return the complete reply to restart.
pub(crate) struct ResponseDelivery {
    target: Option<Box<ResponseDeliveryTarget>>,
    unclaimed: Option<Response>,
}

#[derive(Clone)]
pub(crate) enum Content {
    Text(String),
    Image {
        data: String,
        mime_type: String,
        artifact: Option<crate::transcript::Artifact>,
    },
}

/// Constructs response regions and control state before bounded MCP projection.
#[derive(Default)]
pub(super) struct ResponseBuilder {
    response: Response,
    /// Whether earlier text of the same stream, already sealed into another
    /// interval, ended mid-line. Notices start on their own line regardless of
    /// where polls cut the stream.
    continues_open_line: bool,
}

#[derive(Clone, Copy)]
pub(super) enum TerminalState {
    Completed,
    Running,
    StdinNeeded,
    Idle,
    Stopped,
    WorkerStarting,
    ReplacementReady,
}

pub(super) enum SendResponse {
    Idle(Response),
    Stopped(Response),
    Failed(Response),
    Running(Response),
    InputRequested(Response),
    Completed(Response),
    ReplacementStarting(Response),
    ReplacementReady(Response),
    Restarted(Response),
}

pub(super) struct SendFailure {
    pub(super) message: String,
    pub(super) worker_stopped: bool,
    preceded_restart: bool,
    worker_outcome: Option<super::WorkerProcessOutcome>,
}

impl From<String> for SendFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            worker_stopped: false,
            preceded_restart: false,
            worker_outcome: None,
        }
    }
}

impl SendFailure {
    pub(super) fn worker_stopped(mut self) -> Self {
        self.worker_stopped = true;
        self
    }

    pub(super) fn worker_outcome(mut self, outcome: Option<super::WorkerProcessOutcome>) -> Self {
        self.worker_outcome = outcome;
        self
    }

    pub(super) fn preceded_restart(mut self) -> Self {
        self.preceded_restart = true;
        self
    }

    pub(super) fn should_survive_restart(&self) -> bool {
        self.worker_stopped || self.preceded_restart
    }
}

impl Response {
    /// Inspect drained output before adding a synthetic status observation.
    pub(super) fn has_pending_output(&self) -> bool {
        !self.is_empty() || self.is_error
    }

    pub(crate) fn is_empty_stopped(&mut self) -> bool {
        !self.is_error
            && matches!(self.terminal, Some(TerminalState::Stopped))
            && matches!(self.preview.render_recorded().0.as_slice(),
                [Content::Text(text)] if text == "\n[stopped]")
    }

    /// Combine session previews under the ordinary complete-result bounds while
    /// keeping each session's original response available for delivery recovery.
    /// The caller holds the returned copy until `settle_recording` has seen it.
    pub(crate) fn append_session(
        &mut self,
        name: &str,
        status: &str,
        mut response: Self,
    ) -> Option<ResponseDelivery> {
        let phase = response.observed_phase();
        let delivery = response.delivery.take().map(|target| ResponseDelivery {
            target: Some(target),
            unclaimed: Some(Response {
                preview: response.preview.clone(),
                is_error: response.is_error,
                terminal: response.terminal,
                phase: response.phase.take(),
                settled_phase: None,
                delivery: None,
                execution_error: response.execution_error.clone(),
            }),
        });
        self.push_notice_line(format!(
            "session: {}; status: {status}",
            serde_json::json!(name)
        ));
        if let Some(phase) = phase {
            response.preview.bound();
            response
                .preview
                .phase_before_terminal(phase, response.phase_joins_cell_clock());
        }
        self.extend(response);
        delivery
    }

    /// The same observation used by the text response, without parsing notices.
    pub(crate) fn session_status(&self) -> Option<&'static str> {
        let terminal = self.terminal?;
        if matches!(
            terminal,
            TerminalState::Running | TerminalState::WorkerStarting
        ) && self
            .phase
            .as_ref()
            .is_some_and(|source| source.phase().is_some())
        {
            return Some("starting");
        }
        Some(match terminal {
            TerminalState::Completed | TerminalState::Idle | TerminalState::ReplacementReady => {
                "idle"
            }
            TerminalState::Running => "running",
            TerminalState::StdinNeeded => "waiting_for_stdin",
            TerminalState::WorkerStarting => "starting",
            TerminalState::Stopped => "stopped",
        })
    }

    #[cfg(unix)]
    pub(crate) fn execution_error(&self) -> Option<&crate::worker_protocol::ExecutionError> {
        self.execution_error.as_deref()
    }

    pub(super) fn set_execution_error(
        &mut self,
        error: Option<crate::worker_protocol::ExecutionError>,
    ) {
        self.execution_error = error.map(Box::new);
    }

    /// Copy presentation state without claiming or acknowledging its delivery.
    pub(super) fn snapshot(&self) -> Self {
        Self {
            preview: Box::new(self.preview.inspection_copy()),
            is_error: self.is_error,
            delivery: None,
            terminal: None,
            phase: None,
            settled_phase: None,
            execution_error: self.execution_error.clone(),
        }
    }

    pub(super) fn preview(&self, writer: crate::transcript::WriterState) -> Vec<Content> {
        self.preview.inspection_copy().render_preview(writer)
    }

    /// The integrator collector owns its result even if its socket disconnects.
    /// Acknowledge that ownership so MCP cannot recover the private response.
    pub(super) fn settle_delivery(&mut self) {
        if let Some(target) = self.delivery.take() {
            target.delivered();
        }
    }

    pub(crate) fn tool_error(message: String) -> Self {
        let mut response = Self::default();
        response.push_tool_error(message);
        response
    }

    /// Give the recording writer a bounded chance to catch up before a reply
    /// names retained files. Other replies never wait for it; see
    /// `Transcript::settle`. `sessions` are an aggregate reply's sessions, each
    /// holding the copy that returns its output to the session if this reply
    /// is not delivered.
    pub(crate) async fn settle_recording(
        &mut self,
        transcript: &crate::transcript::Transcript,
        sessions: &mut [ResponseDelivery],
    ) {
        let phase = self.observed_phase();
        self.settled_phase = Some(phase);
        // A phase notice can itself push the reply past its text budget.
        let phase = phase.map(|phase| (phase, self.phase_joins_cell_clock()));
        if !self.preview.cites_recording(phase) {
            return;
        }
        let recorder = transcript.clone();
        let writer = tokio::task::spawn_blocking(move || recorder.settle())
            .await
            .expect("recording settle task");
        // A cancelled aggregate reply returns each session's copy, which must
        // still report the file failures this wait collected.
        let recoverable = sessions
            .iter_mut()
            .filter_map(|session| session.unclaimed.as_mut())
            .map(|response| &mut *response.preview);
        self.preview.recording_settled(writer, recoverable);
    }

    /// Consumes the response for the MCP adapter.
    pub(crate) fn into_parts(
        mut self,
    ) -> (
        Vec<Content>,
        bool,
        Option<ResponseDelivery>,
        Vec<crate::transcript::ResultPart>,
    ) {
        // Phase observations are rendered inside the ordinary complete-response
        // budget, but never enter retained output or delivery recovery history.
        let phase = self
            .settled_phase
            .take()
            .unwrap_or_else(|| self.observed_phase());
        let (content, recorded_parts) = match phase {
            Some(phase) => {
                self.preview.bound();
                let mut preview = self.preview.clone();
                preview.phase_before_terminal(phase, self.phase_joins_cell_clock());
                preview.render_recorded()
            }
            None => self.preview.render_recorded(),
        };
        let is_error = self.is_error;
        let delivery = self.delivery.take().map(|target| ResponseDelivery {
            target: Some(target),
            unclaimed: Some(Response {
                preview: std::mem::take(&mut self.preview),
                is_error,
                delivery: None,
                terminal: self.terminal,
                phase: self.phase.take(),
                settled_phase: None,
                execution_error: self.execution_error.take(),
            }),
        });
        (content, is_error, delivery, recorded_parts)
    }

    /// The phase notice this response would carry if rendered now.
    fn observed_phase(&self) -> Option<&'static str> {
        self.phase
            .as_ref()
            .filter(|_| {
                matches!(
                    self.terminal,
                    Some(TerminalState::Running | TerminalState::WorkerStarting)
                )
            })
            .and_then(|source| source.phase())
    }

    fn phase_joins_cell_clock(&self) -> bool {
        matches!(self.terminal, Some(TerminalState::Running))
    }

    pub(super) fn observe_phase(&mut self, source: Option<super::status::Source>) {
        // Preserve a retained observation only when current capture is unavailable.
        if let Some(source) = source {
            self.phase = Some(source);
        }
    }

    pub(super) fn extend(&mut self, mut other: Self) {
        self.with_builder(|builder| builder.append_response(&mut other));
    }

    /// Appends another logical response region without inserting a server notice.
    pub(super) fn extend_logical_region(&mut self, other: Self) {
        self.with_builder(|builder| builder.append_logical_region(other));
    }

    /// Appends later output from the same session stream.
    ///
    /// Recovered replies and earlier prelude regions may end with a notice,
    /// which occupies its own line; output that ended mid-line continues
    /// that line.
    pub(super) fn extend_stream(&mut self, other: Self) {
        self.with_builder(|builder| builder.append_stream(other));
    }

    /// Appends cell output after this response's owned idle prelude.
    ///
    /// The canonical builder inserts the separator only when both regions are
    /// nonempty, preserving images and their order on either side.
    pub(super) fn extend_cell_after_idle_prelude(&mut self, other: Self) {
        self.with_builder(|builder| builder.append_cell_after_idle_prelude(other));
    }

    pub(super) fn acknowledge_with(
        &mut self,
        acknowledgment: SyncSender<ResponseAcknowledgment>,
        changed: Arc<tokio::sync::Notify>,
        output: OutputTape,
    ) {
        assert!(
            self.delivery.is_none(),
            "a response can carry only one acknowledgment"
        );
        self.delivery = Some(Box::new(ResponseDeliveryTarget::Evaluation(
            acknowledgment,
            changed,
            output,
        )));
    }

    pub(super) fn recover_to(&mut self, output: OutputTape) {
        assert!(
            self.delivery.is_none(),
            "a response can carry only one delivery target"
        );
        self.delivery = Some(Box::new(ResponseDeliveryTarget::Output(output)));
    }

    fn is_empty(&self) -> bool {
        self.preview.is_empty()
    }

    fn is_error(&self) -> bool {
        self.is_error
    }

    pub(crate) fn push_notice(&mut self, message: impl Into<String>) {
        self.with_builder(|builder| builder.notice(message));
    }

    /// Adds a server notice and ends its line for any output appended later.
    pub(super) fn push_notice_line(&mut self, message: impl Into<String>) {
        self.with_builder(|builder| builder.notice_line(message));
    }

    pub(super) fn evaluation_progress(
        &mut self,
        elapsed: std::time::Duration,
        no_new_output: bool,
    ) {
        self.with_builder(|builder| {
            let silence = if no_new_output { "; no new output" } else { "" };
            builder.state_banner(&format!(
                "elapsed: {:.1}s since admission{silence}",
                elapsed.as_secs_f64()
            ));
        });
    }

    pub(super) fn push_tool_error(&mut self, message: impl Into<String>) {
        self.with_builder(|builder| builder.tool_error(message));
    }

    pub(super) fn push_failure(&mut self, failure: SendFailure) {
        self.with_builder(|builder| builder.send_failure(failure));
    }

    pub(super) fn mark_error(&mut self) {
        self.with_builder(ResponseBuilder::mark_error);
    }

    fn with_builder(&mut self, operation: impl FnOnce(&mut ResponseBuilder)) {
        let mut builder = ResponseBuilder::from_response(std::mem::take(self));
        operation(&mut builder);
        *self = builder.finish();
    }
}

impl ResponseBuilder {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn from_response(response: Response) -> Self {
        Self {
            response,
            continues_open_line: false,
        }
    }

    pub(super) fn finish(self) -> Response {
        self.response
    }

    pub(super) fn image(
        &mut self,
        data: String,
        mime_type: String,
        artifact: Option<crate::transcript::Artifact>,
    ) {
        self.response.preview.image(Arc::new(Content::Image {
            data,
            mime_type,
            artifact,
        }));
    }

    pub(super) fn append_response(&mut self, other: &mut Response) {
        if other.delivery.is_some() {
            assert!(
                self.response.delivery.is_none(),
                "a response can carry only one delivery target"
            );
            self.response.delivery = other.delivery.take();
        }
        self.response
            .preview
            .extend(*std::mem::take(&mut other.preview));
        self.response.is_error |= other.is_error;
        if other.execution_error.is_some() {
            self.response.execution_error = other.execution_error.take();
        }
    }

    pub(super) fn append_logical_region(&mut self, mut other: Response) {
        if matches!(
            self.response.preview.last_visible(),
            Some(Part::Text(_) | Part::Notice(_))
        ) && !self.response.preview.ends_with_newline()
            && other.preview.starts_with_text()
        {
            self.response.preview.notice("\n".to_owned());
        }
        self.append_response(&mut other);
    }

    fn append_stream(&mut self, mut other: Response) {
        if matches!(self.response.preview.last_visible(), Some(Part::Notice(_)))
            && !self.response.preview.ends_with_newline()
            && other.preview.starts_with_text()
        {
            self.response.preview.notice("\n".to_owned());
        }
        self.append_response(&mut other);
    }

    pub(super) fn append_cell_after_idle_prelude(&mut self, mut cell: Response) {
        if !self.response.is_empty() && !cell.is_empty() {
            self.notice_line("output produced while idle");
        }
        self.append_response(&mut cell);
    }

    pub(super) fn notice(&mut self, message: impl Into<String>) {
        self.control_text(render_notice(message));
    }

    pub(super) fn notice_line(&mut self, message: impl Into<String>) {
        self.control_text(format!("{}\n", render_notice(message)));
    }

    fn control_text(&mut self, text: String) {
        let prefix = if self.line_open() { "\n" } else { "" };
        self.response.preview.notice(format!("{prefix}{text}"));
    }

    pub(super) fn server_failure(&mut self, message: impl Into<String>) {
        self.notice(message);
        self.mark_error();
    }

    fn send_failure(&mut self, failure: SendFailure) {
        self.server_failure(failure.message);
        if let Some(outcome) = failure.worker_outcome {
            self.notice(outcome.diagnostic());
        }
        if failure.worker_stopped {
            self.notice(WORKER_STOPPED_NOTICE);
        }
    }

    pub(super) fn tool_error(&mut self, message: impl Into<String>) {
        self.control_text(message.into());
        self.mark_error();
    }

    pub(super) fn terminal(&mut self, state: TerminalState) {
        self.response.terminal = Some(state);
        match state {
            TerminalState::Completed => {
                if self.response.is_empty() {
                    self.notice("done");
                }
            }
            TerminalState::Running => self.state_banner("running; poll with an empty send"),
            TerminalState::StdinNeeded => {
                let prefix = if self.response.preview.ends_with_newline() {
                    ""
                } else {
                    "\n"
                };
                self.response
                    .preview
                    .notice(format!("{prefix}{}", render_notice("waiting for stdin")));
            }
            TerminalState::Idle => {
                if !self.response.is_error() {
                    self.state_banner(WORKER_IDLE_NOTICE);
                }
            }
            TerminalState::Stopped => self.state_banner("stopped"),
            TerminalState::WorkerStarting => self.notice(WORKER_STARTING_STATE),
            TerminalState::ReplacementReady => self.notice(WORKER_IDLE_NOTICE),
        }
    }

    pub(super) fn mark_error(&mut self) {
        self.response.is_error = true;
    }

    fn state_banner(&mut self, state: &str) {
        self.response
            .preview
            .notice(format!("\n{}", render_notice(state)));
    }

    /// Whether output appended next continues a partial line of this stream.
    pub(super) fn line_open(&self) -> bool {
        if self.response.is_empty() {
            self.continues_open_line
        } else {
            !self.response.preview.ends_with_newline()
        }
    }

    /// Starts the next interval of the same stream.
    pub(super) fn continuation(&self) -> Self {
        let continues_open_line = if self.response.is_empty() {
            self.continues_open_line
        } else {
            self.response.preview.ends_with_partial_text_line()
        };
        Self {
            response: Response::default(),
            continues_open_line,
        }
    }

    /// Records that a separator line has ended the stream's open line.
    pub(super) fn start_line(&mut self) {
        self.continues_open_line = false;
    }
}

impl ResponseDelivery {
    pub(crate) fn delivered(mut self) {
        self.unclaimed = None;
        if let Some(target) = self.target.take() {
            target.delivered();
        }
    }

    pub(crate) fn unclaimed(mut self) {
        self.return_unclaimed();
    }

    fn return_unclaimed(&mut self) {
        let Some(target) = self.target.take() else {
            return;
        };
        let response = self
            .unclaimed
            .take()
            .expect("response delivery with a target must retain its reply");
        target.unclaimed(response);
    }
}

impl Drop for ResponseDelivery {
    fn drop(&mut self) {
        self.return_unclaimed();
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        let Some(target) = self.delivery.take() else {
            return;
        };
        let response = Self {
            preview: std::mem::take(&mut self.preview),
            is_error: self.is_error,
            delivery: None,
            terminal: self.terminal,
            phase: self.phase.take(),
            settled_phase: None,
            execution_error: self.execution_error.take(),
        };
        target.unclaimed(response);
    }
}

impl ResponseDeliveryTarget {
    fn delivered(self) {
        if let Self::Evaluation(acknowledgment, changed, output) = self {
            let _ = acknowledgment.send(ResponseAcknowledgment::Delivered);
            changed.notify_waiters();
            output.notify_observers();
        }
    }

    fn unclaimed(self, response: Response) {
        match self {
            Self::Evaluation(acknowledgment, changed, output) => {
                let _ = acknowledgment.send(ResponseAcknowledgment::Unclaimed(response));
                changed.notify_waiters();
                output.notify_observers();
            }
            Self::Output(output) => output.recover(response),
        }
    }
}

pub(super) fn project_completed(output: Response) -> Response {
    project_terminal(output, TerminalState::Completed)
}

pub(super) fn project_controlled_completed(output: Response) -> Response {
    if output.is_error() {
        return output;
    }
    let mut builder = ResponseBuilder::from_response(output);
    builder.notice("done");
    builder.finish()
}

pub(super) fn project_replacement_ready(output: Response) -> Response {
    project_terminal(output, TerminalState::ReplacementReady)
}

pub(super) fn render_response(response: SendResponse) -> Response {
    let (output, terminal) = match response {
        SendResponse::Completed(output) => (output, Some(TerminalState::Completed)),
        SendResponse::Failed(output) | SendResponse::Restarted(output) => (output, None),
        SendResponse::InputRequested(output) => (output, Some(TerminalState::StdinNeeded)),
        SendResponse::Running(output) => (output, Some(TerminalState::Running)),
        SendResponse::Idle(output) => (output, Some(TerminalState::Idle)),
        SendResponse::Stopped(output) => (output, Some(TerminalState::Stopped)),
        SendResponse::ReplacementStarting(output) => (output, Some(TerminalState::WorkerStarting)),
        SendResponse::ReplacementReady(output) => (output, Some(TerminalState::ReplacementReady)),
    };
    match terminal {
        Some(terminal) => project_terminal(output, terminal),
        None => output,
    }
}

fn project_terminal(output: Response, terminal: TerminalState) -> Response {
    let mut builder = ResponseBuilder::from_response(output);
    builder.terminal(terminal);
    builder.finish()
}

pub(super) fn direct_failure(message: impl Into<String>) -> Response {
    let mut builder = ResponseBuilder::new();
    builder.server_failure(message);
    builder.finish()
}

fn render_notice(message: impl Into<String>) -> String {
    format!("[{}]", message.into())
}

/// Typed diagnostics have a separate text allowance and must not retain an
/// unbounded exception beside the ordinary bounded output projection.
pub(super) fn bounded_execution_error(
    mut error: crate::worker_protocol::ExecutionError,
) -> crate::worker_protocol::ExecutionError {
    fn trim(text: &mut String, remaining: &mut usize, truncated: &mut bool) {
        let length = utf8_prefix_length(text, *remaining);
        if length < text.len() {
            *text = text[..length].to_owned();
            *truncated = true;
        }
        *remaining -= length;
    }
    let mut remaining = preview::TEXT_BYTES
        - 2 * std::mem::size_of::<String>()
        - std::mem::size_of::<Vec<String>>();
    trim(&mut error.ename, &mut remaining, &mut error.truncated);
    trim(&mut error.evalue, &mut remaining, &mut error.truncated);
    let mut retained = 0;
    for frame in &mut error.traceback {
        if remaining < std::mem::size_of::<String>() {
            break;
        }
        remaining -= std::mem::size_of::<String>();
        trim(frame, &mut remaining, &mut error.truncated);
        retained += 1;
    }
    error.truncated |= retained < error.traceback.len();
    error.traceback.truncate(retained);
    error.traceback.shrink_to_fit();
    error
}

fn utf8_prefix_length(text: &str, limit: usize) -> usize {
    let mut length = text.len().min(limit);
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    length
}

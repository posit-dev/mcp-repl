use std::time::Instant;

#[derive(Default)]
pub(crate) struct InputState {
    active: bool,
    ready_for_input: bool,
    ready_from_input_wait: bool,
    ready_observed_at: Option<Instant>,
    completed_observed_at: Option<Instant>,
    protocol_error: Option<LatchedProtocolError>,
    session_end: bool,
    session_end_final: bool,
}

struct LatchedProtocolError {
    message: String,
    observed_at: Instant,
}

impl InputState {
    pub(crate) fn begin_input(&mut self) -> Result<(), String> {
        if !self.ready_for_input {
            return Err("input_batch sent while worker is not ready for input".to_string());
        }
        self.active = true;
        self.ready_for_input = false;
        self.ready_from_input_wait = false;
        self.completed_observed_at = None;
        Ok(())
    }

    pub(crate) fn clear_request_progress(&mut self) {
        self.active = false;
        self.completed_observed_at = None;
    }

    pub(crate) fn has_active_input(&self) -> bool {
        self.active
    }

    pub(crate) fn ready_for_input(&self) -> bool {
        self.ready_for_input
    }

    pub(crate) fn readiness_observed_after(&self, since: Instant) -> bool {
        self.ready_for_input
            && self
                .ready_observed_at
                .is_some_and(|observed_at| observed_at > since)
    }

    pub(crate) fn input_wait_readiness_available(&self) -> bool {
        self.ready_for_input && self.ready_from_input_wait
    }

    pub(crate) fn validate_active_input(&self, event_type: &str) -> Result<(), String> {
        if !self.active {
            return Err(format!("{event_type} reported with no active input"));
        }
        if self.completed_observed_at.is_some() {
            return Err(format!("{event_type} arrived after input_wait"));
        }
        Ok(())
    }

    pub(crate) fn record_input_wait(&mut self, observed_at: Instant) {
        self.record_ready_at(observed_at, true);
    }

    pub(crate) fn record_ready(&mut self, observed_at: Instant) {
        self.record_ready_at(observed_at, false);
    }

    fn record_ready_at(&mut self, observed_at: Instant, from_input_wait: bool) {
        self.ready_for_input = true;
        self.ready_from_input_wait = from_input_wait;
        self.ready_observed_at = Some(observed_at);
        if self.active {
            self.completed_observed_at = Some(observed_at);
        }
    }

    pub(crate) fn note_interrupt_sent(&mut self) {
        // Readiness is a fact reported by input_wait and consumed by input_batch.
        // Interrupt delivery does not change that state by itself.
    }

    pub(crate) fn request_completion_ready(&self) -> bool {
        self.active && self.completed_observed_at.is_some()
    }

    pub(crate) fn request_completion_precedes_protocol_error(&self) -> bool {
        let Some(error) = self.protocol_error.as_ref() else {
            return false;
        };
        self.active
            && self
                .completed_observed_at
                .is_some_and(|observed_at| observed_at <= error.observed_at)
    }

    pub(crate) fn request_completion_observed_before(&self, deadline: Instant) -> bool {
        self.active
            && self
                .completed_observed_at
                .is_some_and(|observed_at| observed_at <= deadline)
    }

    pub(crate) fn latch_protocol_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        crate::event_log::log(
            "worker_protocol_error_latched",
            serde_json::json!({
                "message": message.clone(),
            }),
        );
        self.protocol_error = Some(LatchedProtocolError {
            message,
            observed_at: Instant::now(),
        });
    }

    pub(crate) fn take_protocol_error(&mut self) -> Option<String> {
        self.protocol_error.take().map(|error| error.message)
    }

    pub(crate) fn note_session_end(&mut self) {
        self.session_end = true;
        self.session_end_final = true;
    }

    pub(crate) fn take_session_end(&mut self) -> bool {
        let seen = self.session_end;
        self.session_end = false;
        seen
    }

    pub(crate) fn session_end_final(&self) -> bool {
        self.session_end_final
    }
}

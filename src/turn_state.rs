use std::time::Instant;

#[derive(Default)]
pub(crate) struct TurnState {
    active: Option<ActiveTurn>,
    protocol_error: Option<LatchedProtocolError>,
    session_end: bool,
    session_end_final: bool,
}

struct ActiveTurn {
    id: u64,
    completed_observed_at: Option<Instant>,
}

struct LatchedProtocolError {
    message: String,
    observed_at: Instant,
}

impl TurnState {
    pub(crate) fn begin_turn(&mut self, turn_id: u64) {
        self.active = Some(ActiveTurn {
            id: turn_id,
            completed_observed_at: None,
        });
    }

    pub(crate) fn clear_request_progress(&mut self) {
        self.active = None;
    }

    pub(crate) fn has_active_turn(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn validate_active_turn_id(
        &self,
        turn_id: u64,
        event_type: &str,
    ) -> Result<(), String> {
        match self.active.as_ref().map(|turn| turn.id) {
            Some(active) if active == turn_id => Ok(()),
            Some(active) => Err(format!(
                "{event_type} turn_id {turn_id} does not match active turn_id {active}"
            )),
            None => Err(format!(
                "{event_type} reported turn_id {turn_id} with no active turn"
            )),
        }
    }

    pub(crate) fn validate_open_active_turn_id(
        &self,
        turn_id: u64,
        event_type: &str,
    ) -> Result<(), String> {
        self.validate_active_turn_id(turn_id, event_type)?;
        if self
            .active
            .as_ref()
            .is_some_and(|turn| turn.completed_observed_at.is_some())
        {
            return Err(format!("{event_type} turn_id {turn_id} arrived after idle"));
        }
        Ok(())
    }

    pub(crate) fn record_idle(&mut self, turn_id: u64, observed_at: Instant) -> Result<(), String> {
        self.validate_open_active_turn_id(turn_id, "idle")?;
        if let Some(active) = self.active.as_mut() {
            active.completed_observed_at = Some(observed_at);
        }
        Ok(())
    }

    pub(crate) fn request_completion_ready(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|turn| turn.completed_observed_at.is_some())
    }

    pub(crate) fn request_completion_precedes_protocol_error(&self) -> bool {
        let Some(error) = self.protocol_error.as_ref() else {
            return false;
        };
        self.active
            .as_ref()
            .and_then(|turn| turn.completed_observed_at)
            .is_some_and(|observed_at| observed_at <= error.observed_at)
    }

    pub(crate) fn request_completion_observed_before(&self, deadline: Instant) -> bool {
        self.active
            .as_ref()
            .and_then(|turn| turn.completed_observed_at)
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

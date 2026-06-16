use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct BuiltinAdapterRequestState {
    active_stdin: Option<VecDeque<u8>>,
    readline_result_count: u64,
    readline_unmatched_starts: usize,
    readline_unmatched_since: Option<Instant>,
}

impl BuiltinAdapterRequestState {
    pub(crate) fn begin_with_stdin(&mut self, payload: &[u8]) {
        self.active_stdin = Some(payload.iter().copied().collect());
    }

    pub(crate) fn record_readline_start(&mut self, observed_at: Instant) {
        if !self.waiting_for_new_input() {
            return;
        }
        self.readline_unmatched_starts = self.readline_unmatched_starts.saturating_add(1);
        if self.readline_unmatched_starts == 1 {
            self.readline_unmatched_since = Some(observed_at);
        }
    }

    pub(crate) fn account_stdin(&mut self, bytes: &[u8], event_type: &str) -> Result<(), String> {
        let Some(active_stdin) = self.active_stdin.as_mut() else {
            if bytes.is_empty() {
                return Ok(());
            }
            return Err(format!("{event_type} reported input with no active turn"));
        };
        if bytes.len() > active_stdin.len() {
            return Err(format!(
                "{event_type} reported {} bytes but only {} active stdin bytes remain",
                bytes.len(),
                active_stdin.len()
            ));
        }
        for (idx, expected) in bytes.iter().enumerate() {
            if active_stdin.get(idx) != Some(expected) {
                let actual = active_stdin.get(idx).copied();
                return Err(format!(
                    "{event_type} bytes does not match active stdin at byte {idx}: expected {actual:?}, got {expected}"
                ));
            }
        }
        for _ in bytes {
            active_stdin.pop_front();
        }
        Ok(())
    }

    pub(crate) fn record_readline_result(&mut self) {
        self.readline_result_count = self.readline_result_count.saturating_add(1);
        if self.readline_unmatched_starts > 0 {
            self.readline_unmatched_starts -= 1;
            if self.readline_unmatched_starts == 0 {
                self.readline_unmatched_since = None;
            }
        }
    }

    pub(crate) fn readline_result_count(&self) -> usize {
        self.readline_result_count as usize
    }

    pub(crate) fn request_completion_ready(&self, stable_wait: Duration) -> bool {
        let Some(since) = self.readline_unmatched_since else {
            return false;
        };
        self.readline_unmatched_starts > 0 && since.elapsed() >= stable_wait
    }

    pub(crate) fn request_completion_precedes_protocol_error(
        &self,
        error_observed_at: Instant,
        stable_wait: Duration,
    ) -> bool {
        let Some(since) = self.readline_unmatched_since else {
            return false;
        };
        let Some(stable_at) = since.checked_add(stable_wait) else {
            return false;
        };
        self.readline_unmatched_starts > 0 && stable_at <= error_observed_at
    }

    pub(crate) fn request_completion_observed_before(
        &self,
        deadline: Instant,
        allow_completion_settle_after_deadline: bool,
    ) -> bool {
        allow_completion_settle_after_deadline
            && self.readline_unmatched_starts > 0
            && self
                .readline_unmatched_since
                .is_some_and(|since| since <= deadline)
    }

    pub(crate) fn completion_wait_duration(
        &self,
        deadline: Instant,
        stable_wait: Duration,
        allow_completion_settle_after_deadline: bool,
    ) -> Duration {
        let now = Instant::now();
        let until_deadline = deadline.saturating_duration_since(now);
        let Some(since) = self.readline_unmatched_since else {
            return until_deadline;
        };
        let elapsed = since.elapsed();
        if elapsed >= stable_wait {
            Duration::from_millis(0)
        } else if allow_completion_settle_after_deadline && since <= deadline {
            stable_wait.saturating_sub(elapsed)
        } else {
            until_deadline.min(stable_wait.saturating_sub(elapsed))
        }
    }

    pub(crate) fn reset_request_progress(&mut self) {
        self.active_stdin = None;
        self.readline_result_count = 0;
        self.readline_unmatched_starts = 0;
        self.readline_unmatched_since = None;
    }

    fn waiting_for_new_input(&self) -> bool {
        self.active_stdin
            .as_ref()
            .is_none_or(|active_stdin| active_stdin.is_empty())
    }
}

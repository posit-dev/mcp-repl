#[cfg(target_family = "unix")]
use std::collections::VecDeque;

#[cfg(target_family = "unix")]
#[derive(Debug)]
pub(crate) struct PythonInputQueue {
    active_input: bool,
    queued_bytes: VecDeque<u8>,
}

#[cfg(target_family = "unix")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RuntimeStdinRead {
    pub(crate) protocol_bytes: Vec<u8>,
}

#[cfg(target_family = "unix")]
impl PythonInputQueue {
    pub(crate) fn new() -> Self {
        Self {
            active_input: false,
            queued_bytes: VecDeque::new(),
        }
    }

    pub(crate) fn begin_input(&mut self, payload: Vec<u8>) -> Result<(), String> {
        if self.active_input {
            return Err("input_batch arrived while input is active".to_string());
        }
        self.active_input = true;
        self.queued_bytes.extend(payload);
        Ok(())
    }

    pub(crate) fn clear_for_protocol_failure(&mut self) {
        self.active_input = false;
        self.queued_bytes.clear();
    }

    pub(crate) fn clear_after_interrupt(&mut self) {
        self.queued_bytes.clear();
    }

    pub(crate) fn clear_after_cell_finish(&mut self) {
        self.active_input = false;
        self.queued_bytes.clear();
    }

    pub(crate) fn has_active_input(&self) -> bool {
        self.active_input
    }

    pub(crate) fn take_completed_input(&mut self) -> bool {
        if self.queued_bytes.is_empty() && self.active_input {
            self.active_input = false;
            true
        } else {
            false
        }
    }

    pub(crate) fn consume_line(&mut self) -> Result<Option<RuntimeStdinRead>, String> {
        let byte_count = self
            .queued_bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(self.queued_bytes.len(), |idx| idx.saturating_add(1));
        self.consume_bytes(byte_count)
    }

    pub(crate) fn consume_bytes(
        &mut self,
        byte_count: usize,
    ) -> Result<Option<RuntimeStdinRead>, String> {
        if byte_count == 0 || self.queued_bytes.is_empty() {
            return Ok(None);
        }
        if !self.active_input {
            return Err("runtime stdin was read with no active input".to_string());
        }
        let protocol_bytes = self
            .queued_bytes
            .drain(..byte_count.min(self.queued_bytes.len()))
            .collect::<Vec<_>>();
        Ok(Some(RuntimeStdinRead { protocol_bytes }))
    }
}

#[cfg(target_family = "unix")]
pub(crate) fn normalize_pty_input_payload(payload: Vec<u8>) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(payload.len());
    let mut idx = 0;
    while idx < payload.len() {
        match payload[idx] {
            b'\r' => {
                normalized.push(b'\n');
                idx += 1;
                if payload.get(idx) == Some(&b'\n') {
                    idx += 1;
                }
            }
            byte => {
                normalized.push(byte);
                idx += 1;
            }
        }
    }
    normalized
}

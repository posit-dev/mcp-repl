#[cfg(target_family = "unix")]
use std::collections::VecDeque;

#[cfg(target_family = "unix")]
#[derive(Debug)]
pub(crate) struct PythonInputQueue {
    active_input_id: Option<u64>,
    queued_bytes: VecDeque<u8>,
}

#[cfg(target_family = "unix")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RuntimeStdinRead {
    pub(crate) input_id: u64,
    pub(crate) protocol_bytes: Vec<u8>,
}

#[cfg(target_family = "unix")]
impl PythonInputQueue {
    pub(crate) fn new() -> Self {
        Self {
            active_input_id: None,
            queued_bytes: VecDeque::new(),
        }
    }

    pub(crate) fn begin_input(&mut self, input_id: u64, payload: Vec<u8>) -> Result<(), String> {
        if let Some(active) = self.active_input_id {
            return Err(format!(
                "input_batch input_id {input_id} arrived while input_id {active} is active"
            ));
        }
        self.active_input_id = Some(input_id);
        self.queued_bytes.extend(payload);
        Ok(())
    }

    pub(crate) fn clear_for_protocol_failure(&mut self) {
        self.active_input_id = None;
        self.queued_bytes.clear();
    }

    pub(crate) fn clear_after_interrupt(&mut self) {
        self.queued_bytes.clear();
    }

    pub(crate) fn has_active_input(&self) -> bool {
        self.active_input_id.is_some()
    }

    pub(crate) fn take_completed_input(&mut self) -> Option<u64> {
        if self.queued_bytes.is_empty() {
            self.active_input_id.take()
        } else {
            None
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
        let input_id = self
            .active_input_id
            .ok_or_else(|| "runtime stdin was read with no active input".to_string())?;
        let protocol_bytes = self
            .queued_bytes
            .drain(..byte_count.min(self.queued_bytes.len()))
            .collect::<Vec<_>>();
        Ok(Some(RuntimeStdinRead {
            input_id,
            protocol_bytes,
        }))
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

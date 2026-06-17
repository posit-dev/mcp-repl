#[cfg(target_family = "unix")]
use std::collections::VecDeque;

#[cfg(target_family = "unix")]
const PTY_FEED_TARGET_BYTES: usize = 128;

#[cfg(target_family = "unix")]
#[derive(Debug)]
pub(crate) struct PythonTurnInput {
    active_turn_id: Option<u64>,
    queued_bytes: VecDeque<u8>,
    pty_feed_in_flight: Option<Vec<u8>>,
    next_pty_feed_seq: u64,
    discard_untracked_after_interrupt: bool,
}

#[cfg(target_family = "unix")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PtyFeed {
    pub(crate) turn_id: u64,
    pub(crate) seq: u64,
    pub(crate) bytes: Vec<u8>,
}

#[cfg(target_family = "unix")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RuntimeStdinRead {
    pub(crate) turn_id: u64,
    pub(crate) protocol_bytes: Vec<u8>,
}

#[cfg(target_family = "unix")]
impl PythonTurnInput {
    pub(crate) fn new() -> Self {
        Self {
            active_turn_id: None,
            queued_bytes: VecDeque::new(),
            pty_feed_in_flight: None,
            next_pty_feed_seq: 1,
            discard_untracked_after_interrupt: false,
        }
    }

    pub(crate) fn begin_or_append(&mut self, turn_id: u64, payload: Vec<u8>) -> Result<(), String> {
        match self.active_turn_id {
            Some(active) if active != turn_id && !self.queued_bytes.is_empty() => Err(format!(
                "turn_input turn_id {turn_id} does not match active turn_id {active}"
            )),
            Some(active) if active != turn_id && self.pty_feed_in_flight.is_some() => Err(format!(
                "turn_input turn_id {turn_id} arrived while turn_id {active} has a feed in flight"
            )),
            _ => {
                if self.active_turn_id != Some(turn_id) {
                    self.next_pty_feed_seq = 1;
                }
                self.active_turn_id = Some(turn_id);
                self.discard_untracked_after_interrupt = false;
                self.queued_bytes.extend(payload);
                Ok(())
            }
        }
    }

    pub(crate) fn clear_for_protocol_failure(&mut self) {
        self.active_turn_id = None;
        self.queued_bytes.clear();
        self.pty_feed_in_flight = None;
        self.discard_untracked_after_interrupt = false;
    }

    pub(crate) fn clear_after_interrupt(&mut self, runtime_discarded: &[u8]) {
        let _ = self.take_matching_runtime_bytes(runtime_discarded);
        self.queued_bytes.clear();
        self.pty_feed_in_flight = None;
        self.discard_untracked_after_interrupt = true;
    }

    pub(crate) fn queued_input_exhausted(&self) -> bool {
        self.queued_bytes.is_empty()
    }

    pub(crate) fn pty_feed_in_flight(&self) -> bool {
        self.pty_feed_in_flight.is_some()
    }

    pub(crate) fn prepare_pty_feed(
        &mut self,
        runtime_pending_byte_count: Option<usize>,
    ) -> Result<Option<PtyFeed>, String> {
        self.reconcile_runtime_pty_reads(runtime_pending_byte_count)?;
        if self.pty_feed_in_flight.is_some() {
            return Ok(None);
        }
        let Some(turn_id) = self.active_turn_id else {
            return Ok(None);
        };
        if self.queued_bytes.is_empty() {
            return Ok(None);
        }

        let bytes = pending_queued_input(&self.queued_bytes);
        assert!(
            !bytes.is_empty(),
            "queued Python turn input must prepare a non-empty PTY feed"
        );
        let seq = self.next_pty_feed_seq;
        self.next_pty_feed_seq = self.next_pty_feed_seq.saturating_add(1);
        self.pty_feed_in_flight = Some(bytes.clone());
        Ok(Some(PtyFeed {
            turn_id,
            seq,
            bytes,
        }))
    }

    pub(crate) fn take_consumed_turn(&mut self) -> Option<u64> {
        assert!(
            self.pty_feed_in_flight.is_none(),
            "cannot finish a Python turn while PTY feed bytes are in flight"
        );
        if self.queued_bytes.is_empty() {
            self.active_turn_id.take()
        } else {
            None
        }
    }

    pub(crate) fn consume_runtime_read(
        &mut self,
        runtime_bytes: &[u8],
    ) -> Result<Option<RuntimeStdinRead>, String> {
        if self.discard_untracked_after_interrupt && self.queued_bytes.is_empty() {
            return Ok(None);
        }
        let turn_id = self
            .active_turn_id
            .ok_or_else(|| "runtime stdin was read with no active turn".to_string())?;
        if self.queued_bytes.len() < runtime_bytes.len() {
            return Err(format!(
                "runtime stdin read {} bytes but only {} protocol stdin bytes remain",
                runtime_bytes.len(),
                self.queued_bytes.len()
            ));
        }

        let Some(in_flight) = self.pty_feed_in_flight.as_ref() else {
            return Err("runtime stdin was read with no pty_feed in flight".to_string());
        };
        if runtime_bytes.len() > in_flight.len() {
            return Err(format!(
                "runtime stdin read {} bytes but pty_feed only had {} bytes in flight",
                runtime_bytes.len(),
                in_flight.len()
            ));
        }

        let protocol_bytes = self
            .queued_bytes
            .iter()
            .take(runtime_bytes.len())
            .copied()
            .collect::<Vec<_>>();
        for (idx, ((&expected_feed, &expected_protocol), &actual)) in in_flight
            .iter()
            .zip(protocol_bytes.iter())
            .zip(runtime_bytes)
            .enumerate()
        {
            if !protocol_byte_matches_runtime(expected_feed, actual) {
                return Err(format!(
                    "runtime stdin byte {idx} did not match pty_feed: expected {expected_feed:?}, got {actual:?}"
                ));
            }
            if !protocol_byte_matches_runtime(expected_protocol, actual) {
                return Err(format!(
                    "runtime stdin byte {idx} did not match queued protocol input: expected {expected_protocol:?}, got {actual:?}"
                ));
            }
        }

        if let Some(in_flight) = self.pty_feed_in_flight.as_mut() {
            in_flight.drain(..runtime_bytes.len());
            if in_flight.is_empty() {
                self.pty_feed_in_flight = None;
            }
        }
        self.queued_bytes.drain(..runtime_bytes.len());
        Ok(Some(RuntimeStdinRead {
            turn_id,
            protocol_bytes,
        }))
    }

    fn reconcile_runtime_pty_reads(
        &mut self,
        runtime_pending_byte_count: Option<usize>,
    ) -> Result<(), String> {
        let Some(pending_byte_count) = runtime_pending_byte_count else {
            return Ok(());
        };
        let Some(in_flight) = self.pty_feed_in_flight.as_mut() else {
            return Ok(());
        };
        if pending_byte_count >= in_flight.len() {
            return Ok(());
        }

        let consumed = in_flight.len() - pending_byte_count;
        if consumed > self.queued_bytes.len() {
            return Err(format!(
                "runtime stdin consumed {consumed} bytes but only {} protocol stdin bytes remain",
                self.queued_bytes.len()
            ));
        }
        in_flight.drain(..consumed);
        if in_flight.is_empty() {
            self.pty_feed_in_flight = None;
        }
        self.queued_bytes.drain(..consumed);
        Ok(())
    }

    fn take_matching_runtime_bytes(&mut self, runtime_bytes: &[u8]) -> Vec<u8> {
        if self.queued_bytes.is_empty() {
            return runtime_bytes.to_vec();
        }

        let mut protocol_bytes = Vec::with_capacity(runtime_bytes.len());
        if self.queued_bytes.len() < runtime_bytes.len() {
            return runtime_bytes.to_vec();
        }

        for (&original_byte, &runtime_byte) in self.queued_bytes.iter().zip(runtime_bytes) {
            protocol_bytes.push(original_byte);
            if !protocol_byte_matches_runtime(original_byte, runtime_byte) {
                return runtime_bytes.to_vec();
            }
        }
        self.queued_bytes.drain(..protocol_bytes.len());
        protocol_bytes
    }
}

#[cfg(target_family = "unix")]
fn pending_queued_input(bytes: &VecDeque<u8>) -> Vec<u8> {
    let mut byte_count = 0;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line_end = idx.saturating_add(1);
        if line_end > PTY_FEED_TARGET_BYTES && byte_count > 0 {
            break;
        }
        byte_count = line_end;
        if byte_count >= PTY_FEED_TARGET_BYTES {
            break;
        }
    }
    if byte_count == 0 {
        byte_count = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |idx| idx.saturating_add(1));
    }
    bytes.iter().take(byte_count).copied().collect()
}

#[cfg(target_family = "unix")]
pub(crate) fn normalize_pty_turn_payload(payload: Vec<u8>) -> Vec<u8> {
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

#[cfg(target_family = "unix")]
fn protocol_byte_matches_runtime(protocol_byte: u8, runtime_byte: u8) -> bool {
    protocol_byte == runtime_byte || (protocol_byte == b'\r' && runtime_byte == b'\n')
}

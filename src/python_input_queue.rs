use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) struct PythonInputQueue {
    payloads: VecDeque<String>,
    stdin_bytes: VecDeque<u8>,
    active_read_consumer: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RuntimeStdinRead {
    pub(crate) protocol_bytes: Vec<u8>,
}

impl PythonInputQueue {
    pub(crate) fn new() -> Self {
        Self {
            payloads: VecDeque::new(),
            stdin_bytes: VecDeque::new(),
            active_read_consumer: false,
        }
    }

    pub(crate) fn push_payload(&mut self, input: String) {
        self.payloads.push_back(input);
    }

    pub(crate) fn take_cell_payload(&mut self) -> Option<String> {
        if self.active_read_consumer {
            return None;
        }
        self.payloads.pop_front()
    }

    pub(crate) fn has_active_read_consumer(&self) -> bool {
        self.active_read_consumer
    }

    pub(crate) fn begin_read_consumer(&mut self) -> bool {
        if self.active_read_consumer {
            return false;
        }
        self.active_read_consumer = true;
        true
    }

    pub(crate) fn end_read_consumer(&mut self) {
        self.active_read_consumer = false;
    }

    pub(crate) fn clear_after_interrupt(&mut self) {
        self.payloads.clear();
        self.stdin_bytes.clear();
        self.active_read_consumer = false;
    }

    pub(crate) fn clear_after_cell_finish(&mut self) {
        if !self.active_read_consumer {
            self.stdin_bytes.clear();
        }
    }

    pub(crate) fn clear_after_detached_read(&mut self) {
        if !self.active_read_consumer {
            self.stdin_bytes.clear();
        }
    }

    pub(crate) fn consume_line(&mut self) -> Option<RuntimeStdinRead> {
        self.refill_stdin_bytes();
        let byte_count = self
            .stdin_bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(self.stdin_bytes.len(), |idx| idx.saturating_add(1));
        self.consume_bytes(byte_count)
    }

    pub(crate) fn consume_bytes(&mut self, byte_count: usize) -> Option<RuntimeStdinRead> {
        self.refill_stdin_bytes();
        if byte_count == 0 || self.stdin_bytes.is_empty() {
            return None;
        }
        let protocol_bytes = self
            .stdin_bytes
            .drain(..byte_count.min(self.stdin_bytes.len()))
            .collect::<Vec<_>>();
        Some(RuntimeStdinRead { protocol_bytes })
    }

    fn refill_stdin_bytes(&mut self) {
        if !self.stdin_bytes.is_empty() {
            return;
        }
        let Some(payload) = self.payloads.pop_front() else {
            return;
        };
        self.stdin_bytes
            .extend(normalize_pty_input_payload(prepare_worker_stdin_payload(
                &payload,
            )));
    }
}

fn prepare_worker_stdin_payload(input: &str) -> Vec<u8> {
    let mut payload = input.as_bytes().to_vec();
    if !payload.is_empty() && !payload.ends_with(b"\n") {
        payload.push(b'\n');
    }
    payload
}

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

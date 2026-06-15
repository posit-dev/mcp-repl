#[derive(Default)]
pub(crate) struct BuiltinAdapterAckState {
    stdin_write_acks: usize,
    python_interrupt_acks: usize,
}

impl BuiltinAdapterAckState {
    pub(crate) fn record_stdin_write_ack(&mut self) {
        self.stdin_write_acks += 1;
    }

    pub(crate) fn take_stdin_write_ack(&mut self) -> bool {
        if self.stdin_write_acks == 0 {
            return false;
        }
        self.stdin_write_acks -= 1;
        true
    }

    pub(crate) fn record_python_interrupt_ack(&mut self) {
        self.python_interrupt_acks += 1;
    }

    pub(crate) fn take_python_interrupt_ack(&mut self) -> bool {
        if self.python_interrupt_acks == 0 {
            return false;
        }
        self.python_interrupt_acks -= 1;
        true
    }

    pub(crate) fn clear(&mut self) {
        self.stdin_write_acks = 0;
        self.python_interrupt_acks = 0;
    }

    #[cfg(test)]
    pub(crate) fn has_stdin_write_ack(&self) -> bool {
        self.stdin_write_acks > 0
    }
}

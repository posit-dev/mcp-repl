#[derive(Default)]
pub(crate) struct WindowsPtyOutputFilter {
    state: WindowsPtyOutputFilterState,
    pending: Vec<u8>,
    emitted_output: bool,
}

#[derive(Default)]
enum WindowsPtyOutputFilterState {
    #[default]
    Ground,
    Escape,
    Csi,
    StringControl,
    StringControlEscape,
}

impl WindowsPtyOutputFilter {
    pub(crate) fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            match self.state {
                WindowsPtyOutputFilterState::Ground => {
                    if byte == 0x1b {
                        self.pending.clear();
                        self.pending.push(byte);
                        self.state = WindowsPtyOutputFilterState::Escape;
                    } else {
                        output.push(byte);
                        self.emitted_output = true;
                    }
                }
                WindowsPtyOutputFilterState::Escape => {
                    self.pending.push(byte);
                    if byte == b'[' {
                        self.state = WindowsPtyOutputFilterState::Csi;
                    } else if is_ansi_string_control_start(byte) {
                        self.pending.clear();
                        self.state = WindowsPtyOutputFilterState::StringControl;
                    } else {
                        output.extend_from_slice(&self.pending);
                        self.emitted_output = true;
                        self.pending.clear();
                        self.state = WindowsPtyOutputFilterState::Ground;
                    }
                }
                WindowsPtyOutputFilterState::Csi => {
                    self.pending.push(byte);
                    if is_csi_final_byte(byte) {
                        if !is_conpty_screen_control_csi(&self.pending)
                            && (self.emitted_output || !is_sgr_reset_csi(&self.pending))
                        {
                            output.extend_from_slice(&self.pending);
                            self.emitted_output = true;
                        }
                        self.pending.clear();
                        self.state = WindowsPtyOutputFilterState::Ground;
                    } else if self.pending.len() > 128 {
                        output.extend_from_slice(&self.pending);
                        self.emitted_output = true;
                        self.pending.clear();
                        self.state = WindowsPtyOutputFilterState::Ground;
                    }
                }
                WindowsPtyOutputFilterState::StringControl => {
                    if byte == 0x07 {
                        self.state = WindowsPtyOutputFilterState::Ground;
                    } else if byte == 0x1b {
                        self.state = WindowsPtyOutputFilterState::StringControlEscape;
                    }
                }
                WindowsPtyOutputFilterState::StringControlEscape => {
                    if byte == b'\\' || byte == 0x07 {
                        self.state = WindowsPtyOutputFilterState::Ground;
                    } else {
                        self.state = WindowsPtyOutputFilterState::StringControl;
                    }
                }
            }
        }
        output
    }
}

fn is_sgr_reset_csi(sequence: &[u8]) -> bool {
    matches!(sequence, b"\x1b[m" | b"\x1b[0m")
}

fn is_ansi_string_control_start(byte: u8) -> bool {
    matches!(byte, b']' | b'P' | b'X' | b'^' | b'_')
}

fn is_csi_final_byte(byte: u8) -> bool {
    (0x40..=0x7e).contains(&byte)
}

fn is_conpty_screen_control_csi(sequence: &[u8]) -> bool {
    if !sequence.starts_with(b"\x1b[") {
        return false;
    }
    match sequence.last().copied() {
        Some(b'@' | b'A'..=b'K' | b'P' | b'S' | b'T' | b'X' | b'f' | b'r' | b's' | b'u') => true,
        Some(b'h' | b'l') => sequence
            .get(2..sequence.len().saturating_sub(1))
            .is_some_and(|params| params.starts_with(b"?")),
        _ => false,
    }
}

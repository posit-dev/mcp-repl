#[cfg(not(target_family = "windows"))]
pub(crate) const INPUT_FRAME_PREFIX: &str = "MCP_REPL_INPUT ";

#[cfg(not(target_family = "windows"))]
pub(crate) fn format_input_frame_header(len: usize) -> String {
    format!("{INPUT_FRAME_PREFIX}{len}\n")
}

#[cfg(not(target_family = "windows"))]
pub(crate) fn parse_input_frame_header(line: &str) -> Option<usize> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let rest = trimmed.strip_prefix(INPUT_FRAME_PREFIX)?;
    rest.trim().parse().ok()
}

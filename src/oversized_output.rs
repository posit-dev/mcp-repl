#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OversizedOutputMode {
    Files,
    #[default]
    Pager,
}

impl OversizedOutputMode {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "files" => Ok(Self::Files),
            "pager" => Ok(Self::Pager),
            _ => Err("invalid --oversized-output value (expected files|pager)"),
        }
    }
}

use serde::{Deserialize, Serialize};

use crate::sandbox_cli::{SandboxConfigOperation, parse_sandbox_config_override};

pub(crate) const ENV_BEHAVIOR: &str = "MCP_CONSOLE_REPLY_OVERFLOW_BEHAVIOR";
pub(crate) const ENV_TEXT_PREVIEW_BYTES: &str = "MCP_CONSOLE_REPLY_OVERFLOW_TEXT_PREVIEW_BYTES";
pub(crate) const ENV_TEXT_SPILL_BYTES: &str = "MCP_CONSOLE_REPLY_OVERFLOW_TEXT_SPILL_BYTES";
pub(crate) const ENV_IMAGES_PREVIEW_COUNT: &str = "MCP_CONSOLE_REPLY_OVERFLOW_IMAGES_PREVIEW_COUNT";
pub(crate) const ENV_IMAGES_SPILL_COUNT: &str = "MCP_CONSOLE_REPLY_OVERFLOW_IMAGES_SPILL_COUNT";
pub(crate) const ENV_RETENTION_MAX_DIRS: &str = "MCP_CONSOLE_REPLY_OVERFLOW_RETENTION_MAX_DIRS";

pub(crate) const DEFAULT_TEXT_PREVIEW_BYTES: u64 = 3_500;
pub(crate) const DEFAULT_TEXT_SPILL_BYTES: u64 = 3_500;
pub(crate) const DEFAULT_IMAGES_PREVIEW_COUNT: usize = 2;
pub(crate) const DEFAULT_IMAGES_SPILL_COUNT: usize = 2;
pub(crate) const DEFAULT_RETENTION_MAX_DIRS: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReplyOverflowBehavior {
    Files,
    Pager,
}

impl ReplyOverflowBehavior {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "files" => Ok(Self::Files),
            "pager" => Ok(Self::Pager),
            other => Err(format!(
                "invalid reply overflow behavior: {other} (expected files|pager)"
            )),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Pager => "pager",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReplyOverflowTextSettings {
    pub(crate) preview_bytes: u64,
    pub(crate) spill_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReplyOverflowImageSettings {
    pub(crate) preview_count: usize,
    pub(crate) spill_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReplyOverflowRetentionSettings {
    pub(crate) max_dirs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReplyOverflowSettings {
    pub(crate) behavior: ReplyOverflowBehavior,
    pub(crate) text: ReplyOverflowTextSettings,
    pub(crate) images: ReplyOverflowImageSettings,
    pub(crate) retention: ReplyOverflowRetentionSettings,
}

impl Default for ReplyOverflowSettings {
    fn default() -> Self {
        Self {
            behavior: ReplyOverflowBehavior::Files,
            text: ReplyOverflowTextSettings {
                preview_bytes: DEFAULT_TEXT_PREVIEW_BYTES,
                spill_bytes: DEFAULT_TEXT_SPILL_BYTES,
            },
            images: ReplyOverflowImageSettings {
                preview_count: DEFAULT_IMAGES_PREVIEW_COUNT,
                spill_count: DEFAULT_IMAGES_SPILL_COUNT,
            },
            retention: ReplyOverflowRetentionSettings {
                max_dirs: DEFAULT_RETENTION_MAX_DIRS,
            },
        }
    }
}

impl ReplyOverflowSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.text.preview_bytes > self.text.spill_bytes {
            return Err(format!(
                "reply overflow text preview_bytes ({}) must be <= spill_bytes ({})",
                self.text.preview_bytes, self.text.spill_bytes
            ));
        }
        if self.images.preview_count > self.images.spill_count {
            return Err(format!(
                "reply overflow images preview_count ({}) must be <= spill_count ({})",
                self.images.preview_count, self.images.spill_count
            ));
        }
        if self.retention.max_dirs == 0 {
            return Err("reply overflow retention max_dirs must be >= 1".to_string());
        }
        Ok(())
    }

    pub(crate) fn apply(&mut self, op: &ReplyOverflowConfigOperation) -> Result<(), String> {
        let previous = self.clone();
        match op {
            ReplyOverflowConfigOperation::Behavior(value) => self.behavior = *value,
            ReplyOverflowConfigOperation::TextPreviewBytes(value) => {
                self.text.preview_bytes = *value;
            }
            ReplyOverflowConfigOperation::TextSpillBytes(value) => {
                self.text.spill_bytes = *value;
            }
            ReplyOverflowConfigOperation::ImagesPreviewCount(value) => {
                self.images.preview_count = *value;
            }
            ReplyOverflowConfigOperation::ImagesSpillCount(value) => {
                self.images.spill_count = *value;
            }
            ReplyOverflowConfigOperation::RetentionMaxDirs(value) => {
                self.retention.max_dirs = *value;
            }
        }
        if let Err(err) = self.validate() {
            *self = previous;
            return Err(err);
        }
        Ok(())
    }

    pub(crate) fn env_pairs(&self) -> [(&'static str, String); 6] {
        [
            (ENV_BEHAVIOR, self.behavior.as_str().to_string()),
            (ENV_TEXT_PREVIEW_BYTES, self.text.preview_bytes.to_string()),
            (ENV_TEXT_SPILL_BYTES, self.text.spill_bytes.to_string()),
            (
                ENV_IMAGES_PREVIEW_COUNT,
                self.images.preview_count.to_string(),
            ),
            (ENV_IMAGES_SPILL_COUNT, self.images.spill_count.to_string()),
            (ENV_RETENTION_MAX_DIRS, self.retention.max_dirs.to_string()),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplyOverflowConfigOperation {
    Behavior(ReplyOverflowBehavior),
    TextPreviewBytes(u64),
    TextSpillBytes(u64),
    ImagesPreviewCount(usize),
    ImagesSpillCount(usize),
    RetentionMaxDirs(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppConfigOperation {
    Sandbox(SandboxConfigOperation),
    ReplyOverflow(ReplyOverflowConfigOperation),
}

pub(crate) fn parse_app_config_override(raw: &str) -> Result<AppConfigOperation, String> {
    match parse_sandbox_config_override(raw) {
        Ok(value) => return Ok(AppConfigOperation::Sandbox(value)),
        Err(err) if !err.starts_with("unsupported --config key:") => return Err(err),
        Err(_) => {}
    }

    let (raw_key, raw_value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid --config override (missing '='): {raw}"))?;
    let key = raw_key.trim();
    let value = raw_value.trim();
    match key {
        "reply_overflow.behavior" => Ok(AppConfigOperation::ReplyOverflow(
            ReplyOverflowConfigOperation::Behavior(ReplyOverflowBehavior::parse(
                &parse_string_value(value),
            )?),
        )),
        "reply_overflow.text.preview_bytes" => Ok(AppConfigOperation::ReplyOverflow(
            ReplyOverflowConfigOperation::TextPreviewBytes(parse_u64_value(value)?),
        )),
        "reply_overflow.text.spill_bytes" => Ok(AppConfigOperation::ReplyOverflow(
            ReplyOverflowConfigOperation::TextSpillBytes(parse_u64_value(value)?),
        )),
        "reply_overflow.images.preview_count" => Ok(AppConfigOperation::ReplyOverflow(
            ReplyOverflowConfigOperation::ImagesPreviewCount(parse_usize_value(value)?),
        )),
        "reply_overflow.images.spill_count" => Ok(AppConfigOperation::ReplyOverflow(
            ReplyOverflowConfigOperation::ImagesSpillCount(parse_usize_value(value)?),
        )),
        "reply_overflow.retention.max_dirs" => Ok(AppConfigOperation::ReplyOverflow(
            ReplyOverflowConfigOperation::RetentionMaxDirs(parse_usize_value(value)?),
        )),
        _ => Err(format!("unsupported --config key: {key}")),
    }
}

fn parse_u64_value(raw: &str) -> Result<u64, String> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| format!("expected non-negative integer value, got: {raw}"))
}

fn parse_usize_value(raw: &str) -> Result<usize, String> {
    raw.trim()
        .parse::<usize>()
        .map_err(|_| format!("expected non-negative integer value, got: {raw}"))
}

fn parse_string_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_validate() {
        ReplyOverflowSettings::default()
            .validate()
            .expect("default reply overflow settings should validate");
    }

    #[test]
    fn reply_overflow_config_parses() {
        let parsed =
            parse_app_config_override("reply_overflow.behavior=pager").expect("behavior override");
        assert_eq!(
            parsed,
            AppConfigOperation::ReplyOverflow(ReplyOverflowConfigOperation::Behavior(
                ReplyOverflowBehavior::Pager
            ))
        );
    }

    #[test]
    fn validation_rejects_preview_above_spill() {
        let mut settings = ReplyOverflowSettings::default();
        let err = settings
            .apply(&ReplyOverflowConfigOperation::TextPreviewBytes(4_000))
            .expect_err("expected validation error");
        assert!(err.contains("preview_bytes"));
        assert_eq!(settings.text.preview_bytes, DEFAULT_TEXT_PREVIEW_BYTES);
    }
}

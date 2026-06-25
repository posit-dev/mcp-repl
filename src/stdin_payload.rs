#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteStdinControlAction {
    Interrupt,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeoutBundleReuse {
    None,
    FullReply,
    FollowUpInput,
}

pub(crate) fn split_write_stdin_control_prefix(
    input: &str,
) -> Option<(WriteStdinControlAction, &str)> {
    let first = input.chars().next()?;
    let action = match first {
        '\u{3}' => WriteStdinControlAction::Interrupt,
        '\u{4}' => WriteStdinControlAction::Restart,
        _ => return None,
    };
    Some((action, &input[first.len_utf8()..]))
}

pub(crate) fn timeout_bundle_reuse_for_input(input: &str) -> TimeoutBundleReuse {
    if input.is_empty() {
        return TimeoutBundleReuse::FullReply;
    }

    match split_write_stdin_control_prefix(input) {
        Some((WriteStdinControlAction::Interrupt, "")) => TimeoutBundleReuse::FullReply,
        Some((WriteStdinControlAction::Interrupt, _)) => TimeoutBundleReuse::FollowUpInput,
        Some((WriteStdinControlAction::Restart, _)) => TimeoutBundleReuse::None,
        None => TimeoutBundleReuse::FollowUpInput,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_prefix_accepts_immediate_tail_without_newline() {
        let (action, remaining) =
            split_write_stdin_control_prefix("\u{3}1+1").expect("expected control prefix");
        assert!(matches!(action, WriteStdinControlAction::Interrupt));
        assert_eq!(remaining, "1+1");
    }

    #[test]
    fn control_prefix_preserves_immediate_newline_tail() {
        let (action, remaining) =
            split_write_stdin_control_prefix("\u{4}\nprint(1)").expect("expected control prefix");
        assert!(matches!(action, WriteStdinControlAction::Restart));
        assert_eq!(remaining, "\nprint(1)");
    }
}

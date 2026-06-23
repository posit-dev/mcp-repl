#[test]
fn repl_tool_descriptions_are_backend_specific() {
    let r = super::repl_tool_description_for_backend(
        crate::backend::Backend::R,
        crate::oversized_output::OversizedOutputMode::Files,
    );
    let python = super::repl_tool_description_for_backend(
        crate::backend::Backend::Python,
        crate::oversized_output::OversizedOutputMode::Files,
    );

    assert_ne!(r, python, "expected backend-specific repl descriptions");
    assert!(r.contains("R code"));
    assert!(python.contains("Python REPL"));
}

#[test]
fn repl_tool_descriptions_include_language_specific_affordances() {
    let r = super::repl_tool_description_for_backend(
        crate::backend::Backend::R,
        crate::oversized_output::OversizedOutputMode::Files,
    );
    let python = super::repl_tool_description_for_backend(
        crate::backend::Backend::Python,
        crate::oversized_output::OversizedOutputMode::Files,
    );

    for description in [r, python] {
        let lower = description.to_lowercase();
        assert!(lower.contains("poll"));
        assert!(lower.contains("large output"));
        assert!(lower.contains("images"));
        assert!(lower.contains("debug"));
    }
    assert!(r.contains("help()"));
    assert!(python.contains("help()"));
}

#[test]
fn repl_tool_descriptions_are_mode_specific() {
    let files = super::repl_tool_description_for_backend(
        crate::backend::Backend::R,
        crate::oversized_output::OversizedOutputMode::Files,
    );
    let pager = super::repl_tool_description_for_backend(
        crate::backend::Backend::R,
        crate::oversized_output::OversizedOutputMode::Pager,
    );

    assert_ne!(files, pager, "expected mode-specific repl descriptions");
    assert!(files.contains("output bundle"));
    assert!(pager.contains("modal pager"));
    assert!(pager.contains(":q"));
}

#[test]
fn repl_tool_annotations_mark_local_mutation_without_open_world_access() {
    let router = super::RFilesToolServer::tool_router();
    let tool = router.get("repl").expect("repl tool should exist");
    let annotations = tool.annotations.as_ref().expect("repl annotations");
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.open_world_hint, Some(false));
}

#[test]
fn timeout_bundle_reuse_treats_blank_lines_as_fresh_input() {
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input(""),
        super::response::TimeoutBundleReuse::FullReply
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\n"),
        super::response::TimeoutBundleReuse::FollowUpInput
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\r\n"),
        super::response::TimeoutBundleReuse::FollowUpInput
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\u{3}"),
        super::response::TimeoutBundleReuse::FullReply
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\u{3}\n"),
        super::response::TimeoutBundleReuse::FollowUpInput
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\u{3}\r\n"),
        super::response::TimeoutBundleReuse::FollowUpInput
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\u{4}"),
        super::response::TimeoutBundleReuse::None
    ));
}

#[test]
fn timeout_bundle_reuse_treats_newline_ctrl_c_as_follow_up_input() {
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\u{3}"),
        super::response::TimeoutBundleReuse::FullReply
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\u{3}\n"),
        super::response::TimeoutBundleReuse::FollowUpInput
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\u{3}\r\n"),
        super::response::TimeoutBundleReuse::FollowUpInput
    ));
}

#[test]
fn workflow_uses_the_repository_toolchain_pin() {
    let toolchain = include_str!("../rust-toolchain.toml");
    let channel = toolchain
        .lines()
        .find_map(|line| line.strip_prefix("channel = \"")?.strip_suffix('"'))
        .expect("rust-toolchain.toml declares a channel");
    let workflow = include_str!("../.github/workflows/ci.yml");

    assert!(
        workflow
            .lines()
            .any(|line| line.trim() == format!("toolchain: {channel}"))
    );
}

#[test]
fn workflow_pins_actions_and_uses_the_committed_lockfile() {
    let workflow = include_str!("../.github/workflows/ci.yml");

    assert!(workflow.contains("\npermissions:\n  contents: read\n"));
    assert!(!workflow.contains("cargo generate-lockfile"));

    for action in workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- uses: "))
    {
        let revision = action
            .split_once('@')
            .expect("action has a revision")
            .1
            .split_whitespace()
            .next()
            .expect("action revision is not empty");
        assert_eq!(revision.len(), 40, "action is not pinned: {action}");
        assert!(
            revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "action is not pinned: {action}"
        );
    }

    assert!(include_str!("../Cargo.lock").starts_with("# This file is automatically @generated"));
}

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

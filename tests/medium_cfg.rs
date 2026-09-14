use std::{fs, process::Command};
use tempfile::TempDir;

fn workspace(source: &str) -> TempDir {
    let workspace = TempDir::new().unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    for (name, dependencies, source) in [
        (
            "app",
            "[features]\nextra = []\n[dependencies]\ndep = { path = \"../dep\" }\n",
            "",
        ),
        ("dep", "", source),
    ] {
        let path = workspace.path().join(name);
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(path.join("Cargo.toml"), format!("[package]\nname = {name:?}\nversion = \"0.1.0\"\nedition = \"2024\"\n{dependencies}")).unwrap();
        fs::write(path.join("src/lib.rs"), source).unwrap();
    }
    let output = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(workspace.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    workspace
}

fn query(workspace: &TempDir, options: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use dep::S;", "--root"])
        .arg(workspace.path())
        .args(["--package", "app"])
        .args(options)
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", workspace.path().join("target"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn disabled_modules_remove_impls_on_types_declared_elsewhere() {
    let workspace = workspace(
        r#"
pub struct S;
pub trait GhostTrait {}
macro_rules! methods { () => { impl crate::S { pub fn macro_ghost(&self) {} } }; }
impl S { pub fn live(&self) {} }
#[cfg(doc)]
mod docs_only {
    methods!();
    impl crate::GhostTrait for crate::S {}
    impl crate::S { pub fn ghost(&self) {} }
    mod nested { impl crate::S { pub fn nested_ghost(&self) {} } }
    pub use crate::S;
}
#[cfg(doc)] mod r#async { impl crate::S { pub fn raw_ghost(&self) {} } }
#[cfg(doc)] mod café { impl crate::S { pub fn unicode_ghost(&self) {} } }
#[cfg(doc)] mod external;
"#,
    );
    fs::write(
        workspace.path().join("dep/src/external.rs"),
        "impl crate::S { pub fn external_ghost(&self) {} }\nmod nested_external;\n",
    )
    .unwrap();
    fs::create_dir_all(workspace.path().join("dep/src/external")).unwrap();
    fs::write(
        workspace.path().join("dep/src/external/nested_external.rs"),
        "impl crate::S { pub fn deep_ghost(&self) {} }\n",
    )
    .unwrap();
    let report = query(&workspace, &[]);
    assert!(report.contains("pub struct S;"), "{report}");
    assert!(
        report.contains("impl S { pub fn live(self: &Self) }"),
        "{report}"
    );
    assert!(!report.contains("ghost"), "{report}");
    assert!(!report.contains("GhostTrait"), "{report}");
    let json_path = report
        .lines()
        .find_map(|line| line.strip_prefix("source: "))
        .unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(json_path).unwrap()).unwrap();
    let items = raw["index"].as_object().unwrap();
    let ghost = items.values().find(|item| item["name"] == "ghost").unwrap();
    assert_eq!(ghost["attrs"], serde_json::json!([]));
    let ghost_impl = items
        .values()
        .find(|item| {
            item["inner"]["impl"]["items"]
                .as_array()
                .is_some_and(|ids| ids.contains(&ghost["id"]))
        })
        .unwrap();
    assert_eq!(ghost_impl["attrs"], serde_json::json!([]));
    let module = items
        .values()
        .find(|item| item["name"] == "docs_only")
        .unwrap();
    assert!(
        !module["inner"]["module"]["items"]
            .as_array()
            .unwrap()
            .contains(&ghost_impl["id"])
    );
}

#[test]
fn feature_metadata_preserves_workspace_config_directory() {
    let workspace = workspace("pub struct S;\n");
    fs::create_dir_all(workspace.path().join("app/.cargo")).unwrap();
    fs::write(
        workspace.path().join("app/.cargo/config.toml"),
        "[build]\nrustc = \"/definitely/missing/member-only-rustc\"\n",
    )
    .unwrap();
    for options in [
        &[][..],
        &["--no-default-features"][..],
        &["--all-features"][..],
        &["--features", "extra"][..],
    ] {
        assert!(query(&workspace, options).contains("pub struct S;"));
    }
}

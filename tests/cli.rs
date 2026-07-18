use std::fs;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn binary_reports_dependency_item() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use cargo_metadata::MetadataCommand;", "--root", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("crate: cargo_metadata"));
    assert!(stdout.contains("item: struct MetadataCommand"));
}

#[test]
fn binary_reports_batch_brace_imports() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use cargo_metadata::{Metadata, Package};", "--root", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("import: use cargo_metadata::Metadata;"));
    assert!(stdout.contains("item: struct Metadata"));
    assert!(stdout.contains("import: use cargo_metadata::Package;"));
    assert!(stdout.contains("item: struct Package"));
}

#[test]
fn binary_reports_transitive_item_through_external_module_reexport() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use cargo_metadata::camino::Utf8PathBuf;", "--root", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("crate: camino"));
    assert!(stdout.contains("item: struct Utf8PathBuf"));
}

#[test]
fn binary_reports_transitive_external_crate_root_reexport() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use cargo_metadata::camino;", "--root", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("crate: camino"));
    assert!(stdout.contains("item: module camino"));
}

#[test]
fn binary_reports_direct_dependency_reexport_from_transitive_crate() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use serde::Serialize;", "--root", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("crate: serde_core"));
    assert!(stdout.contains("import: use serde::Serialize;"));
    assert!(stdout.contains("resolved item: trait Serialize"));
}

#[test]
fn binary_reports_visible_use_item() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["pub(crate) use syn::ItemUse;", "--root", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("crate: syn"));
    assert!(stdout.contains("item: struct ItemUse"));
}

#[test]
fn binary_formats_trait_methods_without_pub() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use syn::parse::Parse;", "--root", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn parse(input: ParseStream<'_>) -> Result<Self>;"));
    assert!(!stdout.contains("pub fn parse(input: ParseStream<'_>) -> Result<Self>"));
}

#[test]
fn binary_supports_virtual_workspace_package_selection() {
    let workspace = TempDir::new().unwrap();
    fs::create_dir_all(workspace.path().join("app/src")).unwrap();
    fs::create_dir_all(workspace.path().join("dep_crate/src")).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        r#"
[workspace]
members = ["app", "dep_crate"]
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join("app/Cargo.toml"),
        r#"
[package]
name = "app"
version = "0.1.0"
edition = "2024"

[dependencies]
dep_crate = { path = "../dep_crate" }
"#,
    )
    .unwrap();
    fs::write(workspace.path().join("app/src/lib.rs"), "").unwrap();
    fs::write(
        workspace.path().join("dep_crate/Cargo.toml"),
        r#"
[package]
name = "dep_crate"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join("dep_crate/src/lib.rs"),
        "pub struct Thing;\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args([
            "use dep_crate::Thing;",
            "--root",
            workspace.path().to_str().unwrap(),
            "--package",
            "app",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("crate: dep_crate"));
    assert!(stdout.contains("item: struct Thing"));
}

#[test]
fn binary_reports_deprecation_and_semantic_attributes() {
    let workspace = TempDir::new().unwrap();
    fs::create_dir_all(workspace.path().join("app/src")).unwrap();
    fs::create_dir_all(workspace.path().join("metadata_dep/src")).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        r#"
[workspace]
members = ["app", "metadata_dep"]
resolver = "3"
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join("app/Cargo.toml"),
        r#"
[package]
name = "app"
version = "0.1.0"
edition = "2024"

[dependencies]
metadata_dep = { path = "../metadata_dep" }
"#,
    )
    .unwrap();
    fs::write(workspace.path().join("app/src/lib.rs"), "").unwrap();
    fs::write(
        workspace.path().join("metadata_dep/Cargo.toml"),
        r#"
[package]
name = "metadata_dep"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join("metadata_dep/src/lib.rs"),
        r#"
#[deprecated(since = "1.2.3", note = "use Replacement")]
#[must_use = "inspect the value"]
#[non_exhaustive]
#[repr(C, align(8))]
pub struct Annotated {
    pub value: u8,
}
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args([
            "use metadata_dep::Annotated;",
            "--root",
            workspace.path().to_str().unwrap(),
            "--package",
            "app",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("deprecation:\n  since: 1.2.3\n  note: use Replacement\n"));
    assert!(stdout.contains("  #[must_use = \"inspect the value\"]\n"));
    assert!(stdout.contains("  #[non_exhaustive]\n"));
    assert!(stdout.contains("  #[repr(C, align(8))]\n"));
}

#[test]
fn binary_rejects_bad_import() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .arg("use crate::local::Thing;")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("only external crate"));
}

#[test]
fn binary_rejects_rust_standard_library_items() {
    for import in [
        "use std::fs::File;",
        "use core::fmt::Debug;",
        "use alloc::vec::Vec;",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
            .arg(import)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Rust standard library"));
        assert!(stderr.contains("https://doc.rust-lang.org/std/"));
    }
}

#[test]
fn binary_reports_unknown_argument() {
    for args in [["--bad", ""], ["use syn::ItemUse;", "--bad"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
            .args(args.into_iter().filter(|arg| !arg.is_empty()))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "check-docs: unknown argument: --bad\nusage: check-docs '<use crate_name::module::item;>' [--root PATH] [--package NAME_OR_ID] [--target TRIPLE] [--include-dev] [--include-build]\n"
        );
    }
}

#[test]
fn binary_rejects_malformed_import_syntax() {
    for import in [
        "use cargo_metadata::::Metadata;",
        "use cargo_metadata::{Metadata,,Package};",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
            .args([import, "--root", "."])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn binary_reports_missing_root_value() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .arg("--root")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--root requires PATH"));
}

#[test]
fn binary_prints_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("usage: check-docs"));
}

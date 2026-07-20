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
fn binary_resolves_enum_variants_raw_identifiers_markdown_and_absolute_locations() {
    let workspace = TempDir::new().unwrap();
    fs::create_dir_all(workspace.path().join("app/src")).unwrap();
    fs::create_dir_all(workspace.path().join("raw_dep/src")).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        r#"
[workspace]
members = ["app", "raw_dep"]
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

[dependencies.type]
package = "raw_dep"
path = "../raw_dep"
"#,
    )
    .unwrap();
    fs::write(workspace.path().join("app/src/lib.rs"), "").unwrap();
    fs::write(
        workspace.path().join("raw_dep/Cargo.toml"),
        r#"
[package]
name = "raw_dep"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join("raw_dep/src/lib.rs"),
        r#"
pub enum Number {
    One,
}

pub mod r#match {
    pub struct r#type;
}

#[doc = "First paragraph.\n\n- parent\n  - child\n\n    code"]
pub struct Documented;
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args([
            "use r#type::{Number::One, r#match::r#type, Documented};",
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
    assert!(stdout.contains("import: use r#type::Number::One;\nitem: variant One\n"));
    assert!(stdout.contains("definition: One\n"));
    assert!(stdout.contains("import: use r#type::r#match::r#type;\nitem: struct type\n"));
    assert!(
        stdout.contains("docs:\n  First paragraph.\n  \n  - parent\n    - child\n  \n      code\n")
    );
    assert!(stdout.contains(&format!(
        "location: {}/raw_dep/src/lib.rs:",
        workspace.path().display()
    )));
}

#[test]
fn binary_does_not_misclassify_a_missing_item_beside_an_external_glob() {
    let workspace = TempDir::new().unwrap();
    for member in ["app", "facade", "middle"] {
        fs::create_dir_all(workspace.path().join(member).join("src")).unwrap();
    }
    fs::write(
        workspace.path().join("Cargo.toml"),
        r#"
[workspace]
members = ["app", "facade", "middle"]
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
facade = { path = "../facade" }
"#,
    )
    .unwrap();
    fs::write(workspace.path().join("app/src/lib.rs"), "").unwrap();
    fs::write(
        workspace.path().join("facade/Cargo.toml"),
        r#"
[package]
name = "facade"
version = "0.1.0"
edition = "2024"

[dependencies]
middle = { path = "../middle" }
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join("facade/src/lib.rs"),
        "pub use middle::*;\n",
    )
    .unwrap();
    fs::write(
        workspace.path().join("middle/Cargo.toml"),
        r#"
[package]
name = "middle"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join("middle/src/lib.rs"),
        "pub struct Present;\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args([
            "use facade::Missing;",
            "--root",
            workspace.path().to_str().unwrap(),
            "--package",
            "app",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("item 'Missing' not found"), "{stderr}");
    assert!(!stderr.contains("is a public re-export"), "{stderr}");
}

#[test]
fn binary_renders_advanced_item_kinds_without_inventing_definitions() {
    let workspace = TempDir::new().unwrap();
    for member in ["app", "advanced"] {
        fs::create_dir_all(workspace.path().join(member).join("src")).unwrap();
    }
    fs::write(
        workspace.path().join("Cargo.toml"),
        r#"
[workspace]
members = ["app", "advanced"]
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
advanced = { path = "../advanced" }
"#,
    )
    .unwrap();
    fs::write(workspace.path().join("app/src/lib.rs"), "").unwrap();
    fs::write(
        workspace.path().join("advanced/Cargo.toml"),
        r#"
[package]
name = "advanced"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join("advanced/src/lib.rs"),
        r#"
#![feature(auto_traits, trait_alias)]

pub trait Alias = Send + Sync;
pub auto trait Marker {}
pub mod api {}
"#,
    )
    .unwrap();
    let advanced = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args([
            "use advanced::{Alias, Marker, api};",
            "--root",
            workspace.path().to_str().unwrap(),
            "--package",
            "app",
        ])
        .output()
        .unwrap();
    assert!(
        advanced.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&advanced.stderr)
    );
    let stdout = String::from_utf8_lossy(&advanced.stdout);
    assert!(stdout.contains("definition: pub trait Alias = Send + Sync;\n"));
    assert!(stdout.contains("definition: pub auto trait Marker { ... }\n"));
    assert!(stdout.contains("definition: pub mod api;\n"));
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

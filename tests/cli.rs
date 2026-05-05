use std::process::Command;

#[test]
fn binary_reports_dependency_item() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use syn::ItemUse;", "--root", "."])
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("crate: syn"));
    assert!(stdout.contains("item: struct ItemUse"));
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
fn binary_reports_std_item() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use std::fs::File;"])
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("item: struct File"));
}

#[test]
fn binary_reports_unknown_argument() {
    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args(["use syn::ItemUse;", "--bad"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown argument"));
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

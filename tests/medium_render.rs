use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn write_member(workspace: &TempDir, name: &str, manifest: &str, source: &str) {
    fs::create_dir_all(workspace.path().join(name).join("src")).unwrap();
    fs::write(workspace.path().join(name).join("Cargo.toml"), manifest).unwrap();
    fs::write(workspace.path().join(name).join("src/lib.rs"), source).unwrap();
}

#[test]
fn type_reports_retain_inherent_constants_constraints_and_nested_metadata() {
    let workspace = TempDir::new().unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    write_member(
        &workspace,
        "app",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\ndep = { path = \"../dep\" }\n",
        "",
    );
    write_member(
        &workspace,
        "dep",
        "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        r#"pub struct Limits<T>(pub T);

impl<T> Limits<T>
where
    T: Copy,
{
    /// Largest supported value.
    pub const MAX: usize = 8;

    /// Inspect the old limit.
    #[deprecated(since = "0.2.0", note = "use MAX")]
    #[must_use = "inspect the limit"]
    pub fn old(&self) -> usize { Self::MAX }

    /// Reads without checks.
    ///
    /// # Safety
    /// The caller must uphold the limit invariant.
    pub unsafe fn unchecked(&self) -> usize { Self::MAX }
}

pub enum Choice {
    /// Cannot be exhaustively constructed.
    #[non_exhaustive]
    Record {
        /// Stored byte.
        #[deprecated(note = "use replacement")]
        value: u8,
    },
}

pub trait Contract {
    /// Check the contract.
    #[must_use]
    fn check(&self) -> bool;

    /// Stable identifier.
    #[deprecated(note = "use NEW_ID")]
    const ID: u8 = 4;
}
"#,
    );
    let lock = Command::new("cargo")
        .args([
            "generate-lockfile",
            "--offline",
            "--manifest-path",
            workspace.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );

    let output = Command::new(env!("CARGO_BIN_EXE_check-docs"))
        .args([
            "use dep::{Limits, Choice, Contract};",
            "--root",
            workspace.path().to_str().unwrap(),
            "--package",
            "app",
        ])
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", workspace.path().join("target"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(stdout.contains(
        "methods:\n  impl<T> Limits<T> where T: Copy { pub fn old(self: &Self) -> usize }\n    deprecation:\n      since: 0.2.0\n      note: use MAX\n    attributes:\n      #[must_use = \"inspect the limit\"]\n    docs:\n      Inspect the old limit.\n"
    ), "{stdout}");
    assert!(stdout.contains(
        "  impl<T> Limits<T> where T: Copy { pub unsafe fn unchecked(self: &Self) -> usize }\n    docs:\n      Reads without checks.\n      \n      # Safety\n      The caller must uphold the limit invariant.\n"
    ), "{stdout}");
    assert!(stdout.contains(
        "associated constants:\n  impl<T> Limits<T> where T: Copy { pub const MAX: usize = 8; }\n    docs:\n      Largest supported value.\n"
    ), "{stdout}");

    assert!(stdout.contains(
        "details:\n  Record { value: u8 }\n    attributes:\n      #[non_exhaustive]\n    docs:\n      Cannot be exhaustively constructed.\n    members:\n      value: u8\n        deprecation:\n          note: use replacement\n        docs:\n          Stored byte.\n"
    ), "{stdout}");
    assert!(stdout.contains(
        "details:\n  fn check(self: &Self) -> bool;\n    attributes:\n      #[must_use]\n    docs:\n      Check the contract.\n  const ID: u8 = 4;\n    deprecation:\n      note: use NEW_ID\n    docs:\n      Stable identifier.\n"
    ), "{stdout}");
}

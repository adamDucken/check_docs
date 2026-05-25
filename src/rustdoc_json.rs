use crate::resolver::package_spec;
use cargo_metadata::{Metadata, Package, Target};
use rustdoc_types::{Crate, FORMAT_VERSION};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn load_or_generate(
    manifest_path: PathBuf,
    metadata: &Metadata,
    package: &Package,
    target: &Target,
) -> Result<(Crate, PathBuf), String> {
    let json_path = metadata
        .target_directory
        .as_std_path()
        .join("doc")
        .join(format!("{}.json", target.name.replace('-', "_")));
    let _lock = JsonGenerationLock::acquire(json_path.with_extension("json.lock"))?;

    // Always regenerate. Existing JSON does not encode enough of Cargo's resolved state
    // to prove it matches the selected package's current features/source graph.
    generate_json(manifest_path, package, target)?;
    let krate = load_valid_json(&json_path, package)?;
    Ok((krate, json_path))
}

struct JsonGenerationLock {
    path: PathBuf,
}

impl JsonGenerationLock {
    fn acquire(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create rustdoc JSON lock directory {}: {err}",
                    parent.display()
                )
            })?;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "timed out waiting for rustdoc JSON lock {}",
                            path.display()
                        ));
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    return Err(format!(
                        "failed to acquire rustdoc JSON lock {}: {err}",
                        path.display()
                    ));
                }
            }
        }
    }
}

impl Drop for JsonGenerationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn load_valid_json(path: &PathBuf, package: &Package) -> Result<Crate, String> {
    let file = File::open(path)
        .map_err(|err| format!("rustdoc JSON missing at {}: {err}", path.display()))?;
    let krate: Crate = serde_json::from_reader(file)
        .map_err(|err| format!("failed to parse rustdoc JSON {}: {err}", path.display()))?;
    if krate.format_version != FORMAT_VERSION {
        return Err(format!(
            "rustdoc JSON format {} unsupported; supported: {}",
            krate.format_version, FORMAT_VERSION
        ));
    }
    if let Some(version) = &krate.crate_version
        && version != &package.version.to_string()
    {
        return Err(format!(
            "rustdoc JSON stale for {}: found version {}, expected {}",
            package.name, version, package.version
        ));
    }
    Ok(krate)
}

fn generate_json(manifest_path: PathBuf, package: &Package, target: &Target) -> Result<(), String> {
    let toolchain = env::var("CHECK_DOCS_TOOLCHAIN").unwrap_or_else(|_| "nightly".to_string());
    generate_json_with_toolchain(manifest_path, package, target, &toolchain)
}

fn generate_json_with_toolchain(
    manifest_path: PathBuf,
    package: &Package,
    target: &Target,
    toolchain: &str,
) -> Result<(), String> {
    let spec = package_spec(package);
    let selector = target_selector(target)?;
    let output = Command::new("cargo")
        .arg(format!("+{toolchain}"))
        .args(["rustdoc", "--manifest-path"])
        .arg(&manifest_path)
        .args(["-p", &spec])
        .args(selector)
        .args(["--", "-Z", "unstable-options", "--output-format", "json"])
        .output()
        .map_err(|err| {
            format!(
                "failed to run cargo +{toolchain} rustdoc for {} {}: {err}; install nightly with `rustup toolchain install nightly` or set CHECK_DOCS_TOOLCHAIN",
                package.name, package.version
            )
        })?;
    handle_generate_output(package, toolchain, output)
}

fn target_selector(target: &Target) -> Result<Vec<String>, String> {
    if target
        .kind
        .iter()
        .any(|kind| kind == "lib" || kind == "proc-macro")
    {
        return Ok(vec!["--lib".to_string()]);
    }
    if target.kind.iter().any(|kind| kind == "bin") {
        return Ok(vec!["--bin".to_string(), target.name.clone()]);
    }
    if target.kind.iter().any(|kind| kind == "example") {
        return Ok(vec!["--example".to_string(), target.name.clone()]);
    }
    if target.kind.iter().any(|kind| kind == "test") {
        return Ok(vec!["--test".to_string(), target.name.clone()]);
    }
    if target.kind.iter().any(|kind| kind == "bench") {
        return Ok(vec!["--bench".to_string(), target.name.clone()]);
    }
    Err(format!(
        "target {} has unsupported rustdoc target kind {:?}",
        target.name, target.kind
    ))
}

fn handle_generate_output(
    package: &Package,
    toolchain: &str,
    output: std::process::Output,
) -> Result<(), String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format_generate_error(package, toolchain, &stderr));
    }
    Ok(())
}

fn format_generate_error(package: &Package, _toolchain: &str, stderr: &str) -> String {
    let hint = if stderr.contains("toolchain") || stderr.contains("not installed") {
        "; nightly toolchain not found; install with `rustup toolchain install nightly` or set CHECK_DOCS_TOOLCHAIN"
    } else if stderr.contains("unstable-options") || stderr.contains("output-format") {
        "; rustdoc JSON requires nightly and `-Z unstable-options`"
    } else {
        ""
    };
    format!(
        "failed to generate rustdoc JSON for {} {}{hint}: {}",
        package.name,
        package.version,
        stderr.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::MetadataCommand;
    use rustdoc_types::{Id, Item, ItemEnum, Module, Target as RustdocTarget, Visibility};
    use std::collections::HashMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    fn metadata() -> Metadata {
        MetadataCommand::new()
            .manifest_path("Cargo.toml")
            .exec()
            .unwrap()
    }

    fn package() -> Package {
        let metadata = metadata();
        metadata.root_package().unwrap().clone()
    }

    fn minimal_crate(version: Option<String>, format_version: u32) -> Crate {
        let root = Id(1);
        let item = Item {
            id: root,
            crate_id: 0,
            name: Some("root".into()),
            span: None,
            visibility: Visibility::Public,
            docs: None,
            links: HashMap::new(),
            attrs: Vec::new(),
            deprecation: None,
            inner: ItemEnum::Module(Module {
                is_crate: true,
                items: Vec::new(),
                is_stripped: false,
            }),
        };
        Crate {
            root,
            crate_version: version,
            includes_private: false,
            index: HashMap::from([(root, item)]),
            paths: HashMap::new(),
            external_crates: HashMap::new(),
            target: RustdocTarget {
                triple: "x86_64-unknown-linux-gnu".into(),
                target_features: Vec::new(),
            },
            format_version,
        }
    }

    fn status(code: i32) -> ExitStatus {
        #[cfg(unix)]
        {
            ExitStatus::from_raw(code << 8)
        }
        #[cfg(windows)]
        {
            ExitStatus::from_raw(code as u32)
        }
    }

    #[test]
    fn load_valid_json_covers_success_and_validation_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let pkg = package();
        let good = dir.path().join("good.json");
        fs::write(
            &good,
            serde_json::to_vec(&minimal_crate(
                Some(pkg.version.to_string()),
                FORMAT_VERSION,
            ))
            .unwrap(),
        )
        .unwrap();
        let loaded = load_valid_json(&good, &pkg).unwrap();
        assert_eq!(loaded.format_version, FORMAT_VERSION);

        let no_version = dir.path().join("no_version.json");
        fs::write(
            &no_version,
            serde_json::to_vec(&minimal_crate(None, FORMAT_VERSION)).unwrap(),
        )
        .unwrap();
        assert!(load_valid_json(&no_version, &pkg).is_ok());

        let missing = load_valid_json(&dir.path().join("missing.json"), &pkg).unwrap_err();
        assert!(missing.contains("rustdoc JSON missing"));

        let bad_json = dir.path().join("bad.json");
        fs::write(&bad_json, b"not json").unwrap();
        assert!(
            load_valid_json(&bad_json, &pkg)
                .unwrap_err()
                .contains("failed to parse rustdoc JSON")
        );

        let bad_format = dir.path().join("bad_format.json");
        fs::write(
            &bad_format,
            serde_json::to_vec(&minimal_crate(
                Some(pkg.version.to_string()),
                FORMAT_VERSION + 1,
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(
            load_valid_json(&bad_format, &pkg)
                .unwrap_err()
                .contains("unsupported")
        );

        let stale = dir.path().join("stale.json");
        fs::write(
            &stale,
            serde_json::to_vec(&minimal_crate(Some("0.0.0".into()), FORMAT_VERSION)).unwrap(),
        )
        .unwrap();
        assert!(load_valid_json(&stale, &pkg).unwrap_err().contains("stale"));
    }

    #[test]
    fn generate_output_error_hints_are_actionable() {
        let pkg = package();
        let base = Output {
            status: status(1),
            stdout: Vec::new(),
            stderr: b"toolchain 'nightly' is not installed".to_vec(),
        };
        let err = handle_generate_output(&pkg, "nightly", base).unwrap_err();
        assert!(err.contains("rustup toolchain install nightly"));

        let unstable = Output {
            status: status(1),
            stdout: Vec::new(),
            stderr: b"the option `Z` is only accepted with unstable-options output-format".to_vec(),
        };
        let err = handle_generate_output(&pkg, "stable", unstable).unwrap_err();
        assert!(err.contains("rustdoc JSON requires nightly"));

        let plain = format_generate_error(&pkg, "nightly", "plain failure");
        assert!(plain.contains("plain failure"));
        assert!(!plain.contains("requires nightly"));

        let ok = Output {
            status: status(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(handle_generate_output(&pkg, "nightly", ok).is_ok());
    }

    #[test]
    fn target_selector_filters_cargo_rustdoc_to_one_target() {
        let pkg = package();
        let mut target = pkg.targets[0].clone();

        target.kind = vec!["lib".into()];
        assert_eq!(target_selector(&target).unwrap(), vec!["--lib"]);

        target.kind = vec!["proc-macro".into()];
        assert_eq!(target_selector(&target).unwrap(), vec!["--lib"]);

        target.kind = vec!["bin".into()];
        target.name = "tool".into();
        assert_eq!(target_selector(&target).unwrap(), vec!["--bin", "tool"]);

        target.kind = vec!["custom-build".into()];
        assert!(
            target_selector(&target)
                .unwrap_err()
                .contains("unsupported")
        );
    }

    #[test]
    fn generate_json_reports_missing_toolchain() {
        let pkg = package();
        let target = pkg.targets[0].clone();
        let err = generate_json_with_toolchain(
            PathBuf::from("Cargo.toml"),
            &pkg,
            &target,
            "definitely_missing_check_docs_toolchain",
        )
        .unwrap_err();
        assert!(err.contains("failed to generate rustdoc JSON"));
    }
}

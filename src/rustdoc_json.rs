use crate::resolver::{DependencyContext, is_library_target, package_spec};
use cargo_metadata::{DependencyKind, Metadata, Package, Target};
use rustdoc_types::{Crate, FORMAT_VERSION};
use serde::Deserialize;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const PINNED_TOOLCHAIN: &str = "nightly-2025-09-10";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoTargetSelection {
    pub(crate) effective_triple: String,
    pub(crate) cargo_platform: Option<String>,
    pub(crate) command_line_override: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CargoUnitIdentity {
    pub(crate) features: Vec<String>,
    pub(crate) mode: String,
    pub(crate) platform: Option<String>,
    pub(crate) profile: String,
}

pub(crate) struct RustdocRequest<'a> {
    pub(crate) manifest_path: PathBuf,
    pub(crate) metadata: &'a Metadata,
    pub(crate) root_package: &'a Package,
    pub(crate) package: &'a Package,
    pub(crate) target: &'a Target,
    pub(crate) contexts: &'a [DependencyContext],
    pub(crate) target_selection: &'a CargoTargetSelection,
    pub(crate) unit: &'a CargoUnitIdentity,
}

pub(crate) fn load_or_generate(
    mut request: RustdocRequest<'_>,
) -> Result<(Crate, PathBuf), String> {
    request.manifest_path = request.manifest_path.canonicalize().map_err(|err| {
        format!(
            "failed to resolve manifest path {}: {err}",
            request.manifest_path.display()
        )
    })?;
    let invocation_dir = request
        .manifest_path
        .parent()
        .ok_or_else(|| {
            format!(
                "manifest path {} has no parent",
                request.manifest_path.display()
            )
        })?
        .to_path_buf();
    let target_dir = generation_target_dir(&request);
    let mut doc_dir = target_dir.clone();
    if let Some(platform) = &request.unit.platform {
        doc_dir.push(platform);
    }
    let json_path = doc_dir
        .join("doc")
        .join(format!("{}.json", request.target.name.replace('-', "_")));
    let _lock = JsonGenerationLock::acquire(json_path.with_extension("json.lock"))?;

    // Always regenerate. Existing JSON does not encode enough of Cargo's resolved state
    // to prove it matches the selected package's current features/source graph.
    generate_json(
        &request,
        &target_dir,
        json_path.parent().expect("JSON path has doc directory"),
    )?;
    let mut krate = load_valid_json(&json_path, request.package)?;
    normalize_span_paths(&mut krate, &invocation_dir);
    Ok((krate, json_path))
}

fn generation_target_dir(request: &RustdocRequest<'_>) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    request.root_package.id.hash(&mut hasher);
    request.package.id.hash(&mut hasher);
    request.target.name.hash(&mut hasher);
    request.unit.hash(&mut hasher);
    for context in request.contexts {
        context.kind.hash(&mut hasher);
        context.target.hash(&mut hasher);
        context.via.hash(&mut hasher);
    }
    request
        .metadata
        .target_directory
        .as_std_path()
        .join("check-docs")
        .join(format!("{:016x}", hasher.finish()))
}

#[derive(Debug)]
struct JsonGenerationLock {
    _file: File,
}

impl JsonGenerationLock {
    fn acquire(path: PathBuf) -> Result<Self, String> {
        Self::acquire_with_timeout(path, LOCK_WAIT_TIMEOUT)
    }

    fn acquire_with_timeout(path: PathBuf, wait_timeout: Duration) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create rustdoc JSON lock directory {}: {err}",
                    parent.display()
                )
            })?;
        }
        let deadline = Instant::now() + wait_timeout;
        loop {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|err| {
                    format!("failed to open rustdoc JSON lock {}: {err}", path.display())
                })?;
            match file.try_lock() {
                Ok(()) => {
                    write_lock_metadata(&mut file, &path)?;
                    return Ok(Self { _file: file });
                }
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(lock_timeout_message(&path));
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(TryLockError::Error(err)) => {
                    return Err(format!(
                        "failed to acquire rustdoc JSON lock {}: {err}",
                        path.display()
                    ));
                }
            }
        }
    }
}

fn write_lock_metadata(file: &mut File, path: &Path) -> Result<(), String> {
    file.set_len(0).map_err(|err| {
        format!(
            "failed to truncate rustdoc JSON lock metadata {}: {err}",
            path.display()
        )
    })?;
    file.rewind().map_err(|err| {
        format!(
            "failed to seek rustdoc JSON lock metadata {}: {err}",
            path.display()
        )
    })?;
    file.write_all(lock_metadata().as_bytes()).map_err(|err| {
        format!(
            "failed to write rustdoc JSON lock metadata {}: {err}",
            path.display()
        )
    })
}

fn lock_metadata() -> String {
    format!(
        "pid={}\ncreated_unix_secs={}\n",
        std::process::id(),
        current_unix_secs()
    )
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn lock_timeout_message(path: &Path) -> String {
    format!(
        "timed out waiting for the active rustdoc JSON lock {}",
        path.display()
    )
}

fn normalize_span_paths(krate: &mut Crate, invocation_dir: &Path) {
    for item in krate.index.values_mut() {
        let Some(span) = item.span.as_mut() else {
            continue;
        };
        if span.filename.is_relative() {
            span.filename = invocation_dir.join(&span.filename);
        }
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

fn generate_json(
    request: &RustdocRequest<'_>,
    target_dir: &Path,
    doc_dir: &Path,
) -> Result<(), String> {
    let toolchain =
        env::var("CHECK_DOCS_TOOLCHAIN").unwrap_or_else(|_| PINNED_TOOLCHAIN.to_string());
    generate_json_with_toolchain(request, target_dir, doc_dir, &toolchain)
}

fn generate_json_with_toolchain(
    request: &RustdocRequest<'_>,
    target_dir: &Path,
    doc_dir: &Path,
    toolchain: &str,
) -> Result<(), String> {
    target_selector(request.target)?;
    let context_kind = exact_context_kind(request.contexts)?;
    let mut command = Command::new("cargo");
    if let Some(invocation_dir) = request
        .manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        command.current_dir(invocation_dir);
    }
    command.arg(format!("+{toolchain}"));
    if context_kind == DependencyKind::Development {
        command.args(["test", "--no-run"]);
    } else {
        command.arg("rustdoc");
    }
    command
        .args(["--manifest-path"])
        .arg(&request.manifest_path)
        .args([
            "--locked",
            "-p",
            &package_spec(request.root_package),
            "--target-dir",
        ])
        .arg(target_dir);
    if let Some(target_triple) = &request.target_selection.command_line_override {
        command.args(["--target", target_triple]);
    }
    let current_exe = env::current_exe()
        .map_err(|err| format!("failed to locate check-docs executable for Rustdoc: {err}"))?;
    command
        .env("RUSTC_WRAPPER", current_exe)
        .env("CHECK_DOCS_RUSTC_WRAPPER_MODE", "1")
        .env("CHECK_DOCS_WRAPPER_PACKAGE_NAME", &request.package.name)
        .env(
            "CHECK_DOCS_WRAPPER_PACKAGE_VERSION",
            request.package.version.to_string(),
        )
        .env(
            "CHECK_DOCS_WRAPPER_MANIFEST_DIR",
            request
                .package
                .manifest_path
                .parent()
                .expect("package manifest has parent")
                .as_std_path(),
        )
        .env("CHECK_DOCS_WRAPPER_TARGET_NAME", &request.target.name)
        .env(
            "CHECK_DOCS_WRAPPER_FEATURES",
            serde_json::to_string(&request.unit.features).expect("feature names serialize"),
        )
        .env("CHECK_DOCS_WRAPPER_UNIT_MODE", &request.unit.mode)
        .env(
            "CHECK_DOCS_WRAPPER_UNIT_PLATFORM",
            serde_json::to_string(&request.unit.platform).expect("unit platform serializes"),
        )
        .env("CHECK_DOCS_WRAPPER_DOC_DIR", doc_dir);
    let output = command
        .output()
        .map_err(|err| {
            format!(
                "failed to run cargo +{toolchain} rustdoc for {} {}: {err}; install the pinned toolchain with `rustup toolchain install {PINNED_TOOLCHAIN}` or set CHECK_DOCS_TOOLCHAIN",
                request.package.name, request.package.version
            )
        })?;
    handle_generate_output(request.package, toolchain, output)
}

pub(crate) fn run_rustc_wrapper() -> ! {
    let command_arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let Some(compiler) = command_arguments.first() else {
        eprintln!("check-docs Rust compiler wrapper was not given a compiler executable");
        std::process::exit(1);
    };
    let status = Command::new(compiler)
        .args(&command_arguments[1..])
        .status()
        .unwrap_or_else(|err| {
            eprintln!("check-docs Rust compiler wrapper failed to run rustc: {err}");
            std::process::exit(1);
        });
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    let invocation = rustc_invocation(&command_arguments);
    if !wrapper_matches_selected_unit(invocation.arguments) {
        std::process::exit(0);
    }

    let Some(doc_dir) = env::var_os("CHECK_DOCS_WRAPPER_DOC_DIR").map(PathBuf::from) else {
        eprintln!("check-docs Rust compiler wrapper is missing its Rustdoc output directory");
        std::process::exit(1);
    };
    if let Err(err) = fs::create_dir_all(&doc_dir) {
        eprintln!(
            "check-docs Rust compiler wrapper failed to create {}: {err}",
            doc_dir.display()
        );
        std::process::exit(1);
    }
    let mut rustdoc = PathBuf::from(invocation.rustc);
    rustdoc.set_file_name(if cfg!(windows) {
        "rustdoc.exe"
    } else {
        "rustdoc"
    });
    let rustdoc_arguments = rustdoc_arguments(invocation.arguments);
    let mut command = if let Some(workspace_wrapper) = invocation.workspace_wrapper {
        let mut command = Command::new(workspace_wrapper);
        command.arg(&rustdoc);
        command
    } else {
        Command::new(&rustdoc)
    };
    let status = command
        .args(rustdoc_arguments)
        .args(["-Z", "unstable-options", "--output-format", "json", "-o"])
        .arg(&doc_dir)
        .status()
        .unwrap_or_else(|err| {
            eprintln!(
                "check-docs Rust compiler wrapper failed to run {}: {err}",
                rustdoc.display()
            );
            std::process::exit(1);
        });
    std::process::exit(status.code().unwrap_or(1));
}

struct RustcInvocation<'a> {
    workspace_wrapper: Option<&'a OsStr>,
    rustc: &'a OsStr,
    arguments: &'a [OsString],
}

fn rustc_invocation(arguments: &[OsString]) -> RustcInvocation<'_> {
    let compiler = arguments
        .first()
        .expect("compiler wrapper invocation has a compiler");
    // Cargo puts an option immediately after rustc for its direct invocations. When
    // RUSTC_WORKSPACE_WRAPPER is also active, Cargo's documented nesting instead puts
    // the actual rustc executable in this position:
    // `$RUSTC_WRAPPER $RUSTC_WORKSPACE_WRAPPER $RUSTC ...`.
    let nested = arguments
        .get(1)
        .is_some_and(|argument| !argument.to_string_lossy().starts_with('-'));
    if nested {
        RustcInvocation {
            workspace_wrapper: Some(compiler.as_os_str()),
            rustc: arguments[1].as_os_str(),
            arguments: &arguments[2..],
        }
    } else {
        RustcInvocation {
            workspace_wrapper: None,
            rustc: compiler.as_os_str(),
            arguments: &arguments[1..],
        }
    }
}

fn wrapper_matches_selected_unit(arguments: &[OsString]) -> bool {
    let expected_name = env::var("CHECK_DOCS_WRAPPER_PACKAGE_NAME").ok();
    let expected_version = env::var("CHECK_DOCS_WRAPPER_PACKAGE_VERSION").ok();
    let expected_manifest_dir = env::var_os("CHECK_DOCS_WRAPPER_MANIFEST_DIR");
    if expected_name.as_deref() != env::var("CARGO_PKG_NAME").ok().as_deref()
        || expected_version.as_deref() != env::var("CARGO_PKG_VERSION").ok().as_deref()
        || expected_manifest_dir.as_deref() != env::var_os("CARGO_MANIFEST_DIR").as_deref()
    {
        return false;
    }
    let arguments = arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>();
    let expected_target = env::var("CHECK_DOCS_WRAPPER_TARGET_NAME")
        .unwrap_or_default()
        .replace('-', "_");
    if argument_value(&arguments, "--crate-name") != Some(expected_target.as_str()) {
        return false;
    }

    let expected_mode = env::var("CHECK_DOCS_WRAPPER_UNIT_MODE").unwrap_or_default();
    if rustc_compile_mode(&arguments) != Some(expected_mode.as_str()) {
        return false;
    }
    let expected_platform = env::var("CHECK_DOCS_WRAPPER_UNIT_PLATFORM")
        .ok()
        .and_then(|platform| serde_json::from_str::<Option<String>>(&platform).ok())
        .flatten();
    if argument_value(&arguments, "--target") != expected_platform.as_deref() {
        return false;
    }

    let mut actual_features = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        let cfg = if argument == "--cfg" {
            arguments.get(index + 1).map(|value| value.as_ref())
        } else {
            argument.strip_prefix("--cfg=")
        };
        if let Some(feature) = cfg
            .and_then(|cfg| cfg.strip_prefix("feature=\""))
            .and_then(|feature| feature.strip_suffix('"'))
        {
            actual_features.push(feature.to_string());
        }
    }
    actual_features.sort();
    actual_features.dedup();
    let mut expected_features = env::var("CHECK_DOCS_WRAPPER_FEATURES")
        .ok()
        .and_then(|features| serde_json::from_str::<Vec<String>>(&features).ok())
        .unwrap_or_default();
    expected_features.sort();
    expected_features.dedup();
    actual_features == expected_features
}

fn argument_value<'a>(arguments: &'a [std::borrow::Cow<'a, str>], flag: &str) -> Option<&'a str> {
    arguments.iter().enumerate().find_map(|(index, argument)| {
        if argument == flag {
            return arguments.get(index + 1).map(AsRef::as_ref);
        }
        argument
            .strip_prefix(flag)
            .and_then(|value| value.strip_prefix('='))
    })
}

fn rustc_compile_mode(arguments: &[std::borrow::Cow<'_, str>]) -> Option<&'static str> {
    let emit = argument_value(arguments, "--emit")?;
    if emit.split(',').any(|kind| kind == "link") {
        Some("build")
    } else if emit.split(',').any(|kind| kind == "metadata") {
        Some("check")
    } else {
        None
    }
}

fn rustdoc_arguments(arguments: &[OsString]) -> Vec<OsString> {
    let mut filtered = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        if matches!(argument.as_ref(), "--emit" | "--out-dir" | "-o") {
            index += 2;
            continue;
        }
        if argument.starts_with("--emit=") || argument.starts_with("--out-dir=") {
            index += 1;
            continue;
        }
        if argument == "-C"
            && arguments.get(index + 1).is_some_and(|value| {
                matches!(
                    value.to_string_lossy().split('=').next(),
                    Some("incremental" | "metadata" | "extra-filename")
                )
            })
        {
            index += 2;
            continue;
        }
        filtered.push(arguments[index].clone());
        index += 1;
    }
    filtered
}

#[derive(Debug, Deserialize)]
struct UnitGraph {
    version: u32,
    units: Vec<Unit>,
}

#[derive(Debug, Deserialize)]
struct Unit {
    pkg_id: String,
    target: UnitTarget,
    mode: String,
    platform: Option<String>,
    features: Vec<String>,
    profile: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct UnitTarget {
    name: String,
}

fn exact_context_kind(contexts: &[DependencyContext]) -> Result<DependencyKind, String> {
    let Some(first) = contexts.first() else {
        return Err("cannot select a Cargo unit without a dependency context".to_string());
    };
    if contexts.iter().any(|context| context.kind != first.kind) {
        return Err(format!(
            "dependency contexts resolve to different Cargo units ({}); query each exact context separately",
            contexts
                .iter()
                .map(DependencyContext::label)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(first.kind)
}

#[derive(Debug, Deserialize)]
struct CargoConfig {
    build: CargoBuildConfig,
}

#[derive(Debug, Deserialize)]
struct CargoBuildConfig {
    target: ConfiguredCargoTargets,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConfiguredCargoTargets {
    One(String),
    Many(Vec<String>),
}

pub(crate) fn target_selection(
    manifest_path: &Path,
    command_line_target: Option<&str>,
    host_triple: &str,
) -> Result<CargoTargetSelection, String> {
    if let Some(target) = command_line_target {
        return Ok(CargoTargetSelection {
            effective_triple: target.to_string(),
            cargo_platform: Some(target.to_string()),
            command_line_override: Some(target.to_string()),
        });
    }

    let Some(configured_target) = configured_build_target(manifest_path)? else {
        return Ok(CargoTargetSelection {
            effective_triple: host_triple.to_string(),
            cargo_platform: None,
            command_line_override: None,
        });
    };
    Ok(CargoTargetSelection {
        effective_triple: configured_target.clone(),
        cargo_platform: Some(configured_target),
        command_line_override: None,
    })
}

fn configured_build_target(manifest_path: &Path) -> Result<Option<String>, String> {
    let toolchain =
        env::var("CHECK_DOCS_TOOLCHAIN").unwrap_or_else(|_| PINNED_TOOLCHAIN.to_string());
    let mut command = Command::new("cargo");
    if let Some(invocation_dir) = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        command.current_dir(invocation_dir);
    }
    let output = command
        .arg(format!("+{toolchain}"))
        .args([
            "-Z",
            "unstable-options",
            "config",
            "get",
            "build.target",
            "--format",
            "json",
        ])
        .output()
        .map_err(|err| {
            format!("failed to ask Cargo for the configured build target with +{toolchain}: {err}")
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("config value `build.target` is not set") {
            return Ok(None);
        }
        return Err(format!(
            "failed to read Cargo's configured build target: {}",
            stderr.trim()
        ));
    }
    let config: CargoConfig = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "failed to parse Cargo's configured build target: {err}: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        )
    })?;
    match config.build.target {
        ConfiguredCargoTargets::One(target) => Ok(Some(target)),
        ConfiguredCargoTargets::Many(targets) => Err(format!(
            "Cargo build.target selects multiple targets ({}); pass --target TRIPLE to select one documentation target",
            targets.join(", ")
        )),
    }
}

pub(crate) fn resolved_unit(
    manifest_path: &Path,
    root_package: &Package,
    package: &Package,
    target: &Target,
    contexts: &[DependencyContext],
    target_selection: &CargoTargetSelection,
) -> Result<CargoUnitIdentity, String> {
    let toolchain =
        env::var("CHECK_DOCS_TOOLCHAIN").unwrap_or_else(|_| PINNED_TOOLCHAIN.to_string());
    let context_kind = exact_context_kind(contexts)?;
    let only_dev = context_kind == DependencyKind::Development;
    let host_unit = context_kind == DependencyKind::Build
        || target.kind.iter().any(|kind| kind == "proc-macro");
    let mut command = Command::new("cargo");
    if let Some(invocation_dir) = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        command.current_dir(invocation_dir);
    }
    command.arg(format!("+{toolchain}"));
    if only_dev {
        command.args(["test", "--no-run"]);
    } else {
        command.arg("rustdoc");
    }
    command.args(["--manifest-path"]).arg(manifest_path).args([
        "--locked",
        "-p",
        &package_spec(root_package),
        "--unit-graph",
        "-Z",
        "unstable-options",
    ]);
    if let Some(target_triple) = &target_selection.command_line_override {
        command.args(["--target", target_triple]);
    }
    let output = command.output().map_err(|err| {
        format!(
            "failed to run cargo +{toolchain} to resolve the exact feature unit for {} {}: {err}",
            package.name, package.version
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "failed to resolve the exact Cargo feature unit for {} {} without changing Cargo.lock: {}; run `cargo check` or `cargo build` to create or refresh the lockfile, then retry",
            package.name,
            package.version,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let graph: UnitGraph = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "failed to parse Cargo unit graph while resolving features for {} {}: {err}",
            package.name, package.version
        )
    })?;
    if graph.version != 1 {
        return Err(format!(
            "Cargo unit graph version {} is unsupported; supported: 1",
            graph.version
        ));
    }

    let expected_mode = if only_dev || host_unit {
        "build"
    } else {
        "check"
    };
    let expected_platform = if host_unit {
        None
    } else {
        target_selection.cargo_platform.as_deref()
    };
    let mut candidates = graph
        .units
        .into_iter()
        .filter(|unit| unit.pkg_id == package.id.to_string() && unit.target.name == target.name)
        .filter(|unit| unit.mode == expected_mode && unit.platform.as_deref() == expected_platform)
        .map(|mut unit| {
            unit.features.sort();
            CargoUnitIdentity {
                features: unit.features,
                mode: unit.mode,
                platform: unit.platform,
                profile: serde_json::to_string(&unit.profile).expect("Cargo profile serializes"),
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.features
            .cmp(&right.features)
            .then_with(|| left.profile.cmp(&right.profile))
    });
    candidates.dedup();
    match candidates.as_slice() {
        [unit] => Ok(unit.clone()),
        [] => Err(format!(
            "Cargo unit graph did not contain the selected {} unit for {} {} target {} on {}",
            expected_mode,
            package.name,
            package.version,
            target.name,
            expected_platform.unwrap_or("the host platform")
        )),
        _ => Err(format!(
            "Cargo unit graph contained multiple {} units for {} {} target {} on {}; query a dependency context with one exact Cargo unit",
            expected_mode,
            package.name,
            package.version,
            target.name,
            expected_platform.unwrap_or("the host platform")
        )),
    }
}

fn target_selector(target: &Target) -> Result<Vec<String>, String> {
    if is_library_target(target) {
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
        "; compatible nightly toolchain not found; install with `rustup toolchain install nightly-2025-09-10` or set CHECK_DOCS_TOOLCHAIN"
    } else if stderr.contains("lock file") && stderr.contains("needs to be updated") {
        "; Cargo.lock is missing or stale; run `cargo check` or `cargo build` to refresh it, then retry"
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
    fn default_toolchain_matches_the_repository_pin() {
        let toolchain_file = include_str!("../rust-toolchain.toml");
        assert!(toolchain_file.contains(&format!("channel = \"{PINNED_TOOLCHAIN}\"")));
    }

    #[test]
    fn target_selector_filters_cargo_rustdoc_to_one_target() {
        let pkg = package();
        let mut target = pkg.targets[0].clone();

        target.kind = vec!["lib".into()];
        assert_eq!(target_selector(&target).unwrap(), vec!["--lib"]);

        target.kind = vec!["proc-macro".into()];
        assert_eq!(target_selector(&target).unwrap(), vec!["--lib"]);

        for kind in ["rlib", "dylib", "cdylib", "staticlib"] {
            target.kind = vec![kind.into()];
            assert_eq!(target_selector(&target).unwrap(), vec!["--lib"]);
        }

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
        let output = tempfile::TempDir::new().unwrap();
        let contexts = [DependencyContext {
            kind: DependencyKind::Normal,
            target: None,
            via: None,
        }];
        let target_selection = CargoTargetSelection {
            effective_triple: "x86_64-unknown-linux-gnu".into(),
            cargo_platform: None,
            command_line_override: None,
        };
        let unit = CargoUnitIdentity {
            features: Vec::new(),
            mode: "check".into(),
            platform: None,
            profile: "{}".into(),
        };
        let request = RustdocRequest {
            manifest_path: PathBuf::from("Cargo.toml"),
            metadata: &metadata(),
            root_package: &pkg,
            package: &pkg,
            target: &target,
            contexts: &contexts,
            target_selection: &target_selection,
            unit: &unit,
        };
        let err = generate_json_with_toolchain(
            &request,
            output.path(),
            output.path(),
            "definitely_missing_check_docs_toolchain",
        )
        .unwrap_err();
        assert!(err.contains("failed to generate rustdoc JSON"));
    }

    #[test]
    fn compiler_wrapper_parses_direct_and_nested_cargo_invocations() {
        let direct = vec![
            OsString::from("/toolchain/bin/rustc"),
            OsString::from("--crate-name"),
            OsString::from("fixture"),
        ];
        let invocation = rustc_invocation(&direct);
        assert!(invocation.workspace_wrapper.is_none());
        assert_eq!(invocation.rustc, OsStr::new("/toolchain/bin/rustc"));
        assert_eq!(invocation.arguments, &direct[1..]);

        let nested = vec![
            OsString::from("/workspace/recording-wrapper"),
            OsString::from("/toolchain/bin/rustc"),
            OsString::from("--crate-name"),
            OsString::from("fixture"),
        ];
        let invocation = rustc_invocation(&nested);
        assert_eq!(
            invocation.workspace_wrapper,
            Some(OsStr::new("/workspace/recording-wrapper"))
        );
        assert_eq!(invocation.rustc, OsStr::new("/toolchain/bin/rustc"));
        assert_eq!(invocation.arguments, &nested[2..]);
    }

    #[test]
    fn compiler_wrapper_distinguishes_compile_mode_and_platform() {
        let target_check = [
            OsString::from("--crate-name"),
            OsString::from("shared"),
            OsString::from("--emit=dep-info,metadata"),
            OsString::from("--target"),
            OsString::from("wasm32-unknown-unknown"),
        ];
        let target_check = target_check
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert_eq!(rustc_compile_mode(&target_check), Some("check"));
        assert_eq!(
            argument_value(&target_check, "--target"),
            Some("wasm32-unknown-unknown")
        );

        let host_build = [
            OsString::from("--crate-name=shared"),
            OsString::from("--emit"),
            OsString::from("dep-info,metadata,link"),
        ];
        let host_build = host_build
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert_eq!(rustc_compile_mode(&host_build), Some("build"));
        assert_eq!(argument_value(&host_build, "--target"), None);
        assert_eq!(argument_value(&host_build, "--crate-name"), Some("shared"));
    }

    #[test]
    fn cargo_unit_selection_rejects_mixed_contexts() {
        let contexts = [
            DependencyContext {
                kind: DependencyKind::Normal,
                target: None,
                via: None,
            },
            DependencyContext {
                kind: DependencyKind::Development,
                target: None,
                via: None,
            },
        ];

        let error = exact_context_kind(&contexts).unwrap_err();
        assert!(error.contains("different Cargo units"));
        assert!(error.contains("normal, dev"));
    }

    #[test]
    fn lock_writes_metadata_and_releases_exclusive_lock_on_drop() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("crate.json.lock");
        let first =
            JsonGenerationLock::acquire_with_timeout(lock_path.clone(), Duration::ZERO).unwrap();
        let text = fs::read_to_string(&lock_path).unwrap();
        assert!(text.contains("pid="));
        assert!(text.contains("created_unix_secs="));

        let err = JsonGenerationLock::acquire_with_timeout(lock_path.clone(), Duration::ZERO)
            .unwrap_err();
        assert_eq!(
            err,
            format!(
                "timed out waiting for the active rustdoc JSON lock {}",
                lock_path.display()
            )
        );

        drop(first);
        assert!(JsonGenerationLock::acquire_with_timeout(lock_path, Duration::ZERO).is_ok());
    }

    #[test]
    fn lock_ignores_stale_or_malformed_metadata_when_no_owner_is_active() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("stale.json.lock");
        fs::write(&lock_path, "not metadata").unwrap();
        let _lock =
            JsonGenerationLock::acquire_with_timeout(lock_path.clone(), Duration::ZERO).unwrap();
        let text = fs::read_to_string(&lock_path).unwrap();
        assert!(text.contains(&format!("pid={}", std::process::id())));
        assert!(!text.contains("not metadata"));
    }

    #[test]
    fn metadata_write_failure_does_not_leave_an_owned_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("metadata-error.json.lock");
        fs::write(&lock_path, "old").unwrap();
        let mut read_only = File::open(&lock_path).unwrap();
        assert!(write_lock_metadata(&mut read_only, &lock_path).is_err());
        drop(read_only);

        assert!(JsonGenerationLock::acquire_with_timeout(lock_path, Duration::ZERO).is_ok());
    }

    #[test]
    fn relative_rustdoc_spans_are_normalized_against_invocation_directory() {
        let mut krate = minimal_crate(None, FORMAT_VERSION);
        krate.index.get_mut(&krate.root).unwrap().span = Some(rustdoc_types::Span {
            filename: PathBuf::from("dep/src/lib.rs"),
            begin: (7, 1),
            end: (7, 2),
        });
        normalize_span_paths(&mut krate, Path::new("/workspace"));
        assert_eq!(
            krate.index[&krate.root].span.as_ref().unwrap().filename,
            PathBuf::from("/workspace/dep/src/lib.rs")
        );
    }
}

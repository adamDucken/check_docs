use crate::cli::FeatureSelection;
use crate::resolver::{DependencyContext, is_library_target, package_spec};
use cargo_metadata::{DependencyKind, Metadata, Package, Target};
use rustdoc_types::{Crate, FORMAT_VERSION};
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use syn::parse::Parser;

const PINNED_TOOLCHAIN: &str = "nightly-2025-09-10";
const GENERATION_MARKER: &str = "check-docs managed generation\n";
pub(crate) const CFG_UNAVAILABLE_ATTRIBUTE: &str = "#[check_docs_cfg_unavailable]";

pub(crate) fn selected_toolchain() -> String {
    env::var("CHECK_DOCS_TOOLCHAIN").unwrap_or_else(|_| PINNED_TOOLCHAIN.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoTargetSelection {
    pub(crate) toolchain: String,
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
    pub(crate) feature_selection: &'a FeatureSelection,
    pub(crate) unit: &'a CargoUnitIdentity,
}

pub(crate) struct CargoUnitRequest<'a> {
    pub(crate) manifest_path: &'a Path,
    pub(crate) metadata: &'a Metadata,
    pub(crate) root_package: &'a Package,
    pub(crate) package: &'a Package,
    pub(crate) target: &'a Target,
    pub(crate) contexts: &'a [DependencyContext],
    pub(crate) target_selection: &'a CargoTargetSelection,
    pub(crate) feature_selection: &'a FeatureSelection,
}

pub(crate) struct GenerationSession {
    generation_root: PathBuf,
    target_dir: PathBuf,
    next_unit: usize,
    _lock: JsonGenerationLock,
}

impl GenerationSession {
    pub(crate) fn start(metadata: &Metadata) -> Result<Self, String> {
        let generation_root = prepare_generation_root(metadata.target_directory.as_std_path())?;
        let lock = JsonGenerationLock::acquire(generation_root.join("generation.lock"))?;
        let target_dir = reset_generation_target_dir(&generation_root)?;
        Ok(Self {
            generation_root,
            target_dir,
            next_unit: 0,
            _lock: lock,
        })
    }

    fn next_target_dir(&mut self, metadata: &Metadata) -> Result<PathBuf, String> {
        let requested_root = prepare_generation_root(metadata.target_directory.as_std_path())?;
        if requested_root != self.generation_root {
            return Err(format!(
                "cannot retain rustdoc JSON from target directories {} and {} in one generation session",
                self.generation_root.display(),
                requested_root.display()
            ));
        }
        let target_dir = self.target_dir.join(format!("unit-{}", self.next_unit));
        self.next_unit += 1;
        Ok(target_dir)
    }
}

pub(crate) fn load_or_generate(
    generation: &mut GenerationSession,
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
    let target_dir = generation.next_target_dir(request.metadata)?;
    let mut doc_dir = target_dir.clone();
    if let Some(platform) = &request.unit.platform {
        doc_dir.push(platform);
    }
    let json_path = doc_dir
        .join("doc")
        .join(format!("{}.json", request.target.name.replace('-', "_")));
    generate_json(
        &request,
        &target_dir,
        json_path.parent().expect("JSON path has doc directory"),
    )?;
    let mut krate = load_valid_json(&json_path, request.package)?;
    let cfg = load_rustc_cfg(&json_path.with_extension("cfg"))?;
    apply_non_doc_cfg(&mut krate, &cfg);
    normalize_span_paths(&mut krate, &invocation_dir);
    Ok((krate, json_path))
}

fn prepare_generation_root(target_directory: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(target_directory).map_err(|err| {
        format!(
            "failed to create Cargo target directory {}: {err}",
            target_directory.display()
        )
    })?;
    let target_directory = target_directory.canonicalize().map_err(|err| {
        format!(
            "failed to resolve Cargo target directory {}: {err}",
            target_directory.display()
        )
    })?;
    let generation_root = target_directory.join("check-docs");
    match fs::symlink_metadata(&generation_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!(
                "refusing to use non-directory or symlink check-docs managed root {}",
                generation_root.display()
            ));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Err(err) = fs::create_dir(&generation_root) {
                if err.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(format!(
                        "failed to create check-docs managed root {}: {err}",
                        generation_root.display()
                    ));
                }
                let metadata = fs::symlink_metadata(&generation_root).map_err(|err| {
                    format!(
                        "failed to inspect concurrently created check-docs managed root {}: {err}",
                        generation_root.display()
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(format!(
                        "refusing to use non-directory or symlink check-docs managed root {}",
                        generation_root.display()
                    ));
                }
            }
        }
        Err(err) => {
            return Err(format!(
                "failed to inspect check-docs managed root {}: {err}",
                generation_root.display()
            ));
        }
    }
    let resolved_root = generation_root.canonicalize().map_err(|err| {
        format!(
            "failed to resolve check-docs managed root {}: {err}",
            generation_root.display()
        )
    })?;
    if resolved_root.parent() != Some(target_directory.as_path()) {
        return Err(format!(
            "refusing to use check-docs managed root {} outside Cargo target directory {}",
            resolved_root.display(),
            target_directory.display()
        ));
    }
    Ok(resolved_root)
}

fn reset_generation_target_dir(generation_root: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(generation_root).map_err(|err| {
        format!(
            "failed to create check-docs generation root {}: {err}",
            generation_root.display()
        )
    })?;
    let target_dir = generation_root.join("generation");
    let target_exists = match fs::symlink_metadata(&target_dir) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "refusing to replace unowned check-docs generation path {}",
                    target_dir.display()
                ));
            }
            let marker = target_dir.join(".check-docs-generation");
            let marker_metadata = fs::symlink_metadata(&marker).map_err(|err| {
                format!(
                    "refusing to replace unowned check-docs generation directory {}: failed to inspect ownership marker {}: {err}",
                    target_dir.display(),
                    marker.display()
                )
            })?;
            if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
                return Err(format!(
                    "refusing to replace unowned check-docs generation directory {}: ownership marker {} is not a regular non-symlink file",
                    target_dir.display(),
                    marker.display()
                ));
            }
            let contents = fs::read_to_string(&marker).map_err(|err| {
                format!(
                    "refusing to replace unowned check-docs generation directory {}: failed to read ownership marker {}: {err}",
                    target_dir.display(),
                    marker.display()
                )
            })?;
            if contents != GENERATION_MARKER {
                return Err(format!(
                    "refusing to replace unowned check-docs generation directory {}: invalid ownership marker",
                    target_dir.display()
                ));
            }
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(format!(
                "failed to inspect managed generation directory {}: {err}",
                target_dir.display()
            ));
        }
    };

    cleanup_owned_generation_staging(generation_root)?;
    let staging = create_marked_generation_staging(generation_root)?;
    if target_exists {
        fs::remove_dir_all(&target_dir).map_err(|err| {
            format!(
                "failed to reset managed check-docs generation directory {}: {err}",
                target_dir.display()
            )
        })?;
    }
    fs::rename(&staging, &target_dir).map_err(|err| {
        format!(
            "failed to atomically install managed check-docs generation directory {} from {}: {err}",
            target_dir.display(),
            staging.display()
        )
    })?;
    Ok(target_dir)
}

fn cleanup_owned_generation_staging(generation_root: &Path) -> Result<(), String> {
    let entries = fs::read_dir(generation_root).map_err(|err| {
        format!(
            "failed to inspect check-docs generation root {}: {err}",
            generation_root.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to inspect an entry in check-docs generation root {}: {err}",
                generation_root.display()
            )
        })?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("generation.staging-") {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let marker = path.join(".check-docs-generation");
        let Ok(marker_metadata) = fs::symlink_metadata(&marker) else {
            continue;
        };
        if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
            continue;
        }
        if !fs::read_to_string(&marker).is_ok_and(|contents| contents == GENERATION_MARKER) {
            continue;
        }
        fs::remove_dir_all(&path).map_err(|err| {
            format!(
                "failed to remove interrupted managed generation staging directory {}: {err}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn create_marked_generation_staging(generation_root: &Path) -> Result<PathBuf, String> {
    for index in 0_u64.. {
        let staging = generation_root.join(format!("generation.staging-{index}"));
        match fs::create_dir(&staging) {
            Ok(()) => {
                if let Err(err) =
                    fs::write(staging.join(".check-docs-generation"), GENERATION_MARKER)
                {
                    let _ = fs::remove_file(staging.join(".check-docs-generation"));
                    let _ = fs::remove_dir(&staging);
                    return Err(format!(
                        "failed to write ownership marker in generation staging directory {}: {err}",
                        staging.display()
                    ));
                }
                return Ok(staging);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "failed to create managed generation staging directory {}: {err}",
                    staging.display()
                ));
            }
        }
    }
    unreachable!("the generation staging index space is not exhausted")
}

#[derive(Debug)]
struct JsonGenerationLock {
    _file: File,
}

impl JsonGenerationLock {
    fn acquire(path: PathBuf) -> Result<Self, String> {
        Self::acquire_until(path, None)
    }

    #[cfg(test)]
    fn acquire_with_timeout(path: PathBuf, wait_timeout: Duration) -> Result<Self, String> {
        Self::acquire_until(path, Some(Instant::now() + wait_timeout))
    }

    fn acquire_until(path: PathBuf, deadline: Option<Instant>) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create rustdoc JSON lock directory {}: {err}",
                    parent.display()
                )
            })?;
        }
        loop {
            let file = open_lock_file(&path)?;
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Self { _file: file });
                }
                Err(TryLockError::WouldBlock) => {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
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

fn open_lock_file(path: &Path) -> Result<File, String> {
    let file = match File::create_new(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_lock_path(path)?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|error| {
                    format!(
                        "failed to open rustdoc JSON lock {}: {error}",
                        path.display()
                    )
                })?
        }
        Err(error) => {
            return Err(format!(
                "failed to create rustdoc JSON lock {}: {error}",
                path.display()
            ));
        }
    };
    let path_metadata = validate_lock_path(path)?;
    let file_metadata = file.metadata().map_err(|error| {
        format!(
            "failed to inspect open rustdoc JSON lock {}: {error}",
            path.display()
        )
    })?;
    if !file_metadata.is_file() {
        return Err(invalid_lock_path_message(path));
    }
    validate_lock_identity(path, &path_metadata, &file_metadata)?;
    Ok(file)
}

fn validate_lock_path(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to inspect rustdoc JSON lock {}: {error}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_lock_path_message(path));
    }
    Ok(metadata)
}

fn invalid_lock_path_message(path: &Path) -> String {
    format!(
        "refusing to use non-regular or symlink rustdoc JSON lock path {}",
        path.display()
    )
}

#[cfg(unix)]
fn validate_lock_identity(
    path: &Path,
    path_metadata: &fs::Metadata,
    file_metadata: &fs::Metadata,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino() {
        return Err(format!(
            "rustdoc JSON lock path {} changed while it was being opened",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_lock_identity(
    _path: &Path,
    _path_metadata: &fs::Metadata,
    _file_metadata: &fs::Metadata,
) -> Result<(), String> {
    Ok(())
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

#[derive(Debug, Default)]
struct RustcCfg {
    flags: HashSet<String>,
    values: HashSet<(String, String)>,
}

impl RustcCfg {
    fn parse(output: &[u8]) -> Self {
        let mut cfg = Self::default();
        for line in String::from_utf8_lossy(output).lines().map(str::trim) {
            if let Some((name, value)) = line.split_once('=') {
                if let Ok(value) = syn::parse_str::<syn::LitStr>(value) {
                    cfg.values.insert((name.to_string(), value.value()));
                }
            } else if !line.is_empty()
                && line
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric())
            {
                cfg.flags.insert(line.to_string());
            }
        }
        cfg
    }

    fn contains_flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn contains_value(&self, name: &str, value: &str) -> bool {
        self.values.contains(&(name.to_string(), value.to_string()))
    }
}

fn load_rustc_cfg(path: &Path) -> Result<RustcCfg, String> {
    let output = fs::read(path).map_err(|err| {
        format!(
            "non-doc rustc cfg output missing at {}: {err}",
            path.display()
        )
    })?;
    Ok(RustcCfg::parse(&output))
}

fn apply_non_doc_cfg(krate: &mut Crate, cfg: &RustcCfg) {
    let unavailable = krate
        .index
        .iter()
        .filter_map(|(id, item)| (!item_matches_cfg(item, cfg)).then_some(*id))
        .collect::<HashSet<_>>();
    for item in krate.index.values_mut() {
        activate_cfg_attr_derives(&mut item.attrs, cfg);
    }
    for id in &unavailable {
        if let Some(item) = krate.index.get_mut(id) {
            item.attrs.push(rustdoc_types::Attribute::Other(
                CFG_UNAVAILABLE_ATTRIBUTE.to_string(),
            ));
        }
    }
    for item in krate.index.values_mut() {
        prune_unavailable_references(&mut item.inner, &unavailable);
    }
}

fn activate_cfg_attr_derives(attrs: &mut Vec<rustdoc_types::Attribute>, cfg: &RustcCfg) {
    let mut derives = Vec::new();
    for attr in attrs.iter() {
        let rustdoc_types::Attribute::Other(attribute) = attr else {
            continue;
        };
        let Some(expression) = retained_attribute_expression(attribute, "cfg_attr") else {
            continue;
        };
        collect_cfg_attr_derives(expression, cfg, &mut derives);
    }
    derives.sort();
    derives.dedup();
    if !derives.is_empty() {
        attrs.push(rustdoc_types::Attribute::Other(format!(
            "#[derive({})]",
            derives.join(", ")
        )));
    }
}

fn collect_cfg_attr_derives(expression: &str, cfg: &RustcCfg, derives: &mut Vec<String>) {
    let Ok(nested) = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
        .parse_str(expression)
    else {
        return;
    };
    let Some(predicate) = nested.first() else {
        return;
    };
    if !cfg_meta_matches(predicate, cfg) {
        return;
    }
    for attribute in nested.iter().skip(1) {
        let syn::Meta::List(list) = attribute else {
            continue;
        };
        match cfg_path(&list.path).as_str() {
            "derive" => {
                let Ok(paths) =
                    syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated
                        .parse2(list.tokens.clone())
                else {
                    continue;
                };
                derives.extend(paths.iter().filter_map(derive_path));
            }
            "cfg_attr" => collect_cfg_attr_derives(&list.tokens.to_string(), cfg, derives),
            _ => {}
        }
    }
}

fn derive_path(path: &syn::Path) -> Option<String> {
    if path.segments.is_empty()
        || path
            .segments
            .iter()
            .any(|segment| !matches!(segment.arguments, syn::PathArguments::None))
    {
        return None;
    }
    Some(cfg_path(path))
}

fn item_matches_cfg(item: &rustdoc_types::Item, cfg: &RustcCfg) -> bool {
    item.attrs.iter().all(|attribute| {
        let rustdoc_types::Attribute::Other(attribute) = attribute else {
            return true;
        };
        if let Some(expression) = retained_attribute_expression(attribute, "cfg") {
            cfg_expression_matches(expression, cfg)
        } else if let Some(expression) = retained_attribute_expression(attribute, "cfg_attr") {
            cfg_attr_expression_matches(expression, cfg)
        } else {
            true
        }
    })
}

fn retained_attribute_expression<'a>(attribute: &'a str, name: &str) -> Option<&'a str> {
    let retained_prefix = format!("#[<{name}>(");
    let source_prefix = format!("#[{name}(");
    attribute
        .strip_prefix(&retained_prefix)
        .or_else(|| attribute.strip_prefix(&source_prefix))?
        .strip_suffix(")]")
}

fn cfg_expression_matches(expression: &str, cfg: &RustcCfg) -> bool {
    syn::parse_str::<syn::Meta>(expression).is_ok_and(|meta| cfg_meta_matches(&meta, cfg))
}

fn cfg_attr_expression_matches(expression: &str, cfg: &RustcCfg) -> bool {
    let Ok(nested) = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
        .parse_str(expression)
    else {
        return false;
    };
    let Some(predicate) = nested.first() else {
        return false;
    };
    !cfg_meta_matches(predicate, cfg)
        || nested
            .iter()
            .skip(1)
            .all(|attribute| cfg_attr_output_matches(attribute, cfg))
}

fn cfg_attr_output_matches(attribute: &syn::Meta, cfg: &RustcCfg) -> bool {
    let syn::Meta::List(list) = attribute else {
        return true;
    };
    match cfg_path(&list.path).as_str() {
        "cfg" => syn::parse2::<syn::Meta>(list.tokens.clone())
            .is_ok_and(|meta| cfg_meta_matches(&meta, cfg)),
        "cfg_attr" => syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
            .parse2(list.tokens.clone())
            .is_ok_and(|nested| {
                let Some(predicate) = nested.first() else {
                    return false;
                };
                !cfg_meta_matches(predicate, cfg)
                    || nested
                        .iter()
                        .skip(1)
                        .all(|attribute| cfg_attr_output_matches(attribute, cfg))
            }),
        _ => true,
    }
}

fn cfg_meta_matches(meta: &syn::Meta, cfg: &RustcCfg) -> bool {
    match meta {
        syn::Meta::Path(path) => cfg.contains_flag(&cfg_lookup_path(path)),
        syn::Meta::NameValue(name_value) => {
            let syn::Expr::Lit(expression) = &name_value.value else {
                return false;
            };
            let syn::Lit::Str(value) = &expression.lit else {
                return false;
            };
            cfg.contains_value(&cfg_lookup_path(&name_value.path), &value.value())
        }
        syn::Meta::List(list) => {
            let Ok(nested) =
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
                    .parse2(list.tokens.clone())
            else {
                return false;
            };
            match cfg_path(&list.path).as_str() {
                "all" => nested.iter().all(|meta| cfg_meta_matches(meta, cfg)),
                "any" => nested.iter().any(|meta| cfg_meta_matches(meta, cfg)),
                "not" if nested.len() == 1 => !cfg_meta_matches(&nested[0], cfg),
                _ => false,
            }
        }
    }
}

fn cfg_path(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn cfg_lookup_path(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| {
            let identifier = segment.ident.to_string();
            identifier
                .strip_prefix("r#")
                .unwrap_or(&identifier)
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("::")
}

fn prune_unavailable_references(
    inner: &mut rustdoc_types::ItemEnum,
    unavailable: &HashSet<rustdoc_types::Id>,
) {
    fn retain(ids: &mut Vec<rustdoc_types::Id>, unavailable: &HashSet<rustdoc_types::Id>) {
        ids.retain(|id| !unavailable.contains(id));
    }

    fn retain_optional(
        ids: &mut Vec<Option<rustdoc_types::Id>>,
        unavailable: &HashSet<rustdoc_types::Id>,
    ) {
        ids.retain(|id| id.is_none_or(|id| !unavailable.contains(&id)));
    }

    match inner {
        rustdoc_types::ItemEnum::Module(module) => retain(&mut module.items, unavailable),
        rustdoc_types::ItemEnum::Struct(struct_) => {
            retain(&mut struct_.impls, unavailable);
            match &mut struct_.kind {
                rustdoc_types::StructKind::Tuple(fields) => retain_optional(fields, unavailable),
                rustdoc_types::StructKind::Plain { fields, .. } => retain(fields, unavailable),
                rustdoc_types::StructKind::Unit => {}
            }
        }
        rustdoc_types::ItemEnum::Union(union_) => {
            retain(&mut union_.fields, unavailable);
            retain(&mut union_.impls, unavailable);
        }
        rustdoc_types::ItemEnum::Enum(enum_) => {
            retain(&mut enum_.variants, unavailable);
            retain(&mut enum_.impls, unavailable);
        }
        rustdoc_types::ItemEnum::Variant(variant) => match &mut variant.kind {
            rustdoc_types::VariantKind::Tuple(fields) => retain_optional(fields, unavailable),
            rustdoc_types::VariantKind::Struct { fields, .. } => retain(fields, unavailable),
            rustdoc_types::VariantKind::Plain => {}
        },
        rustdoc_types::ItemEnum::Trait(trait_) => {
            retain(&mut trait_.items, unavailable);
            retain(&mut trait_.implementations, unavailable);
        }
        rustdoc_types::ItemEnum::Impl(impl_) => retain(&mut impl_.items, unavailable),
        _ => {}
    }
}

fn generate_json(
    request: &RustdocRequest<'_>,
    target_dir: &Path,
    doc_dir: &Path,
) -> Result<(), String> {
    generate_json_with_toolchain(
        request,
        target_dir,
        doc_dir,
        &request.target_selection.toolchain,
    )
}

fn generate_json_with_toolchain(
    request: &RustdocRequest<'_>,
    target_dir: &Path,
    doc_dir: &Path,
    toolchain: &str,
) -> Result<(), String> {
    target_selector(request.target)?;
    let context_kind = exact_context_kind(request.contexts)?;
    let target_default_panic = target_default_panic(toolchain, request.unit.platform.as_deref())?;
    let original_rustc_wrapper =
        effective_general_rustc_wrapper(&request.manifest_path, toolchain)?;
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
    command.args(request.feature_selection.cargo_args());
    if let Some(target_triple) = &request.target_selection.command_line_override {
        command.args(["--target", target_triple]);
    }
    let current_exe = env::current_exe()
        .map_err(|err| format!("failed to locate check-docs executable for Rustdoc: {err}"))?;
    command
        .env("RUSTC_WRAPPER", current_exe)
        .env_remove("CHECK_DOCS_ORIGINAL_RUSTC_WRAPPER")
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
        .env("CHECK_DOCS_WRAPPER_UNIT_PROFILE", &request.unit.profile)
        .env(
            "CHECK_DOCS_WRAPPER_TARGET_DEFAULT_PANIC",
            target_default_panic,
        )
        .env(
            "CHECK_DOCS_WRAPPER_CFG_PATH",
            doc_dir.join(format!("{}.cfg", request.target.name.replace('-', "_"))),
        )
        .env("CHECK_DOCS_WRAPPER_DOC_DIR", doc_dir);
    if let Some(wrapper) = original_rustc_wrapper {
        command.env("CHECK_DOCS_ORIGINAL_RUSTC_WRAPPER", wrapper);
    }
    let output = command
        .output()
        .map_err(|err| {
            format!(
                "failed to run cargo +{toolchain} rustdoc for {} {}: {err}; install it with `rustup toolchain install {toolchain}` or set CHECK_DOCS_TOOLCHAIN",
                request.package.name, request.package.version
            )
        })?;
    handle_generate_output(request.package, toolchain, output)
}

fn target_default_panic(toolchain: &str, platform: Option<&str>) -> Result<&'static str, String> {
    let mut command = Command::new("rustc");
    command.arg(format!("+{toolchain}"));
    if let Some(platform) = platform {
        command.args(["--target", platform]);
    }
    let output = command.arg("--print=cfg").output().map_err(|error| {
        format!("failed to inspect the default panic cfg with rustc +{toolchain}: {error}")
    })?;
    if !output.status.success() {
        return Err(format!(
            "failed to inspect the default panic cfg with rustc +{toolchain}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let cfg = RustcCfg::parse(&output.stdout);
    for strategy in ["abort", "unwind"] {
        if cfg.contains_value("panic", strategy) {
            return Ok(strategy);
        }
    }
    Err(format!(
        "failed to parse the default panic cfg from rustc +{toolchain} --print=cfg"
    ))
}

pub(crate) fn run_rustc_wrapper() -> ! {
    let command_arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let Some(compiler) = command_arguments.first() else {
        eprintln!("check-docs Rust compiler wrapper was not given a compiler executable");
        std::process::exit(1);
    };
    let original_wrapper = env::var_os("CHECK_DOCS_ORIGINAL_RUSTC_WRAPPER");
    let mut compile = if let Some(wrapper) = &original_wrapper {
        let mut command = Command::new(wrapper);
        command.arg(compiler);
        command
    } else {
        Command::new(compiler)
    };
    let status = compile
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

    let mut print_cfg = if let Some(wrapper) = &original_wrapper {
        let mut command = Command::new(wrapper);
        command.arg(compiler);
        command
    } else {
        Command::new(compiler)
    };
    let cfg_output = print_cfg
        .args(&command_arguments[1..])
        .arg("--print=cfg")
        .output()
        .unwrap_or_else(|err| {
            eprintln!("check-docs Rust compiler wrapper failed to inspect rustc cfgs: {err}");
            std::process::exit(1);
        });
    if !cfg_output.status.success() {
        eprintln!(
            "check-docs Rust compiler wrapper failed to inspect rustc cfgs: {}",
            String::from_utf8_lossy(&cfg_output.stderr).trim()
        );
        std::process::exit(cfg_output.status.code().unwrap_or(1));
    }
    let cfg = RustcCfg::parse(&cfg_output.stdout);
    if !profile_matches_selected_unit(&cfg, invocation.arguments) {
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
    let Some(cfg_path) = env::var_os("CHECK_DOCS_WRAPPER_CFG_PATH").map(PathBuf::from) else {
        eprintln!("check-docs Rust compiler wrapper is missing its rustc cfg output path");
        std::process::exit(1);
    };
    if let Err(err) = fs::write(&cfg_path, &cfg_output.stdout) {
        eprintln!(
            "check-docs Rust compiler wrapper failed to write {}: {err}",
            cfg_path.display()
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
    let mut command = match (original_wrapper, invocation.workspace_wrapper) {
        (Some(wrapper), Some(workspace_wrapper)) => {
            let mut command = Command::new(wrapper);
            command.arg(workspace_wrapper).arg(&rustdoc);
            command
        }
        (Some(wrapper), None) => {
            let mut command = Command::new(wrapper);
            command.arg(&rustdoc);
            command
        }
        (None, Some(workspace_wrapper)) => {
            let mut command = Command::new(workspace_wrapper);
            command.arg(&rustdoc);
            command
        }
        (None, None) => Command::new(&rustdoc),
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

fn profile_matches_selected_unit(cfg: &RustcCfg, arguments: &[OsString]) -> bool {
    let Some(profile) = env::var("CHECK_DOCS_WRAPPER_UNIT_PROFILE")
        .ok()
        .and_then(|profile| serde_json::from_str::<serde_json::Value>(&profile).ok())
    else {
        return false;
    };
    let Ok(target_default_panic) = env::var("CHECK_DOCS_WRAPPER_TARGET_DEFAULT_PANIC") else {
        return false;
    };
    let arguments = arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>();
    profile_matches_cfg(&profile, cfg, &arguments, &target_default_panic)
}

fn profile_matches_cfg(
    profile: &serde_json::Value,
    cfg: &RustcCfg,
    arguments: &[std::borrow::Cow<'_, str>],
    target_default_panic: &str,
) -> bool {
    let Some(opt_level) = profile.get("opt_level").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(lto) = profile.get("lto").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(codegen_backend) = optional_profile_string(profile, "codegen_backend") else {
        return false;
    };
    let Some(codegen_units) = optional_profile_u64(profile, "codegen_units") else {
        return false;
    };
    let Some(debuginfo) = profile_debuginfo(profile) else {
        return false;
    };
    let Some(split_debuginfo) = optional_profile_string(profile, "split_debuginfo") else {
        return false;
    };
    let Some(debug_assertions) = profile
        .get("debug_assertions")
        .and_then(serde_json::Value::as_bool)
    else {
        return false;
    };
    let Some(overflow_checks) = profile
        .get("overflow_checks")
        .and_then(serde_json::Value::as_bool)
    else {
        return false;
    };
    let Some(rpath) = profile.get("rpath").and_then(serde_json::Value::as_bool) else {
        return false;
    };
    let Some(incremental) = profile
        .get("incremental")
        .and_then(serde_json::Value::as_bool)
    else {
        return false;
    };
    let Some(panic) = profile.get("panic").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(strip) = profile_strip(profile) else {
        return false;
    };
    let explicit_panic = codegen_value(arguments, "panic");
    let effective_panic = explicit_panic.unwrap_or(target_default_panic);

    codegen_value(arguments, "opt-level").unwrap_or("0") == opt_level
        && profile_lto_matches(lto, arguments)
        && unstable_value(arguments, "codegen-backend") == codegen_backend
        && codegen_units.is_none_or(|expected| {
            codegen_value(arguments, "codegen-units").and_then(|actual| actual.parse::<u64>().ok())
                == Some(expected)
        })
        && (codegen_units.is_some() || !codegen_option_present(arguments, "codegen-units"))
        && codegen_value(arguments, "debuginfo").unwrap_or("0") == debuginfo
        && codegen_value(arguments, "split-debuginfo") == split_debuginfo
        && cfg.contains_flag("debug_assertions") == debug_assertions
        && cfg.contains_flag("overflow_checks") == overflow_checks
        && codegen_bool(arguments, "rpath") == rpath
        && codegen_option_present(arguments, "incremental") == incremental
        && explicit_panic.is_none_or(|actual| actual == panic)
        && cfg.contains_value("panic", effective_panic)
        && codegen_value(arguments, "strip").unwrap_or("none") == strip
}

fn optional_profile_string<'a>(
    profile: &'a serde_json::Value,
    field: &str,
) -> Option<Option<&'a str>> {
    match profile.get(field)? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::String(value) => Some(Some(value)),
        _ => None,
    }
}

fn optional_profile_u64(profile: &serde_json::Value, field: &str) -> Option<Option<u64>> {
    match profile.get(field)? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::Number(value) => value.as_u64().map(Some),
        _ => None,
    }
}

fn profile_debuginfo(profile: &serde_json::Value) -> Option<String> {
    match profile.get("debuginfo")? {
        serde_json::Value::Null => Some("0".into()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::String(value) => Some(value.clone()),
        _ => None,
    }
}

fn profile_strip(profile: &serde_json::Value) -> Option<&str> {
    let strip = profile.get("strip")?.as_object()?;
    let value = if let Some(value) = strip.get("deferred") {
        value.as_str()?
    } else {
        strip.get("resolved")?.as_object()?.get("Named")?.as_str()?
    };
    match value {
        "None" | "none" => Some("none"),
        "Debuginfo" | "debuginfo" => Some("debuginfo"),
        "Symbols" | "symbols" => Some("symbols"),
        _ => None,
    }
}

fn profile_lto_matches(lto: &str, arguments: &[std::borrow::Cow<'_, str>]) -> bool {
    let linker_plugin = codegen_option_present(arguments, "linker-plugin-lto");
    let explicit_lto = codegen_value(arguments, "lto");
    match lto {
        "false" => !linker_plugin && explicit_lto.is_none(),
        "off" => !linker_plugin && explicit_lto.is_none_or(|value| value == "off"),
        "true" | "fat" | "thin" => {
            linker_plugin
                || explicit_lto
                    .is_some_and(|value| value == lto || (lto == "true" && value == "fat"))
        }
        _ => false,
    }
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

fn codegen_value<'a>(arguments: &'a [std::borrow::Cow<'a, str>], option: &str) -> Option<&'a str> {
    compiler_option_value(arguments, "-C", option)
}

fn unstable_value<'a>(arguments: &'a [std::borrow::Cow<'a, str>], option: &str) -> Option<&'a str> {
    compiler_option_value(arguments, "-Z", option)
}

fn compiler_option_value<'a>(
    arguments: &'a [std::borrow::Cow<'a, str>],
    prefix: &str,
    option: &str,
) -> Option<&'a str> {
    arguments
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| {
            let value = if argument == prefix {
                arguments.get(index + 1).map(AsRef::as_ref)
            } else {
                argument.strip_prefix(prefix)
            }?;
            value
                .strip_prefix(option)
                .and_then(|value| value.strip_prefix('='))
        })
        .next_back()
}

fn codegen_option_present(arguments: &[std::borrow::Cow<'_, str>], option: &str) -> bool {
    arguments.iter().enumerate().any(|(index, argument)| {
        let value = if argument == "-C" {
            arguments.get(index + 1).map(AsRef::as_ref)
        } else {
            argument.strip_prefix("-C")
        };
        value.is_some_and(|value| {
            value == option
                || value
                    .strip_prefix(option)
                    .is_some_and(|value| value.starts_with('='))
        })
    })
}

fn codegen_bool(arguments: &[std::borrow::Cow<'_, str>], option: &str) -> bool {
    if let Some(value) = codegen_value(arguments, option) {
        matches!(value, "yes" | "on" | "true" | "y")
    } else {
        codegen_option_present(arguments, option)
    }
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
    roots: Vec<usize>,
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
    dependencies: Vec<UnitDependency>,
}

#[derive(Debug, Deserialize)]
struct UnitTarget {
    name: String,
    kind: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UnitDependency {
    index: usize,
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
struct CargoRustcWrapperConfig {
    build: CargoRustcWrapperBuildConfig,
}

#[derive(Debug, Deserialize)]
struct CargoRustcWrapperBuildConfig {
    #[serde(rename = "rustc-wrapper")]
    rustc_wrapper: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConfiguredCargoTargets {
    One(String),
    Many(Vec<String>),
}

fn effective_general_rustc_wrapper(
    manifest_path: &Path,
    toolchain: &str,
) -> Result<Option<OsString>, String> {
    for name in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
        if let Some(wrapper) = env::var_os(name) {
            return Ok((!wrapper.is_empty()).then_some(wrapper));
        }
    }

    configured_general_rustc_wrapper(manifest_path, toolchain, OsStr::new("cargo"))
}

fn configured_general_rustc_wrapper(
    manifest_path: &Path,
    toolchain: &str,
    cargo: &OsStr,
) -> Result<Option<OsString>, String> {
    let mut command = Command::new(cargo);
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
            "build.rustc-wrapper",
            "--format",
            "json",
        ])
        .output()
        .map_err(|error| {
            format!("failed to ask Cargo +{toolchain} for the configured rustc wrapper: {error}")
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("config value `build.rustc-wrapper` is not set") {
            return Ok(None);
        }
        return Err(format!(
            "failed to read Cargo +{toolchain}'s configured rustc wrapper: {}",
            stderr.trim()
        ));
    }
    let config: CargoRustcWrapperConfig =
        serde_json::from_slice(&output.stdout).map_err(|error| {
            format!(
                "failed to parse Cargo's configured rustc wrapper: {error}: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            )
        })?;
    let wrapper = config.build.rustc_wrapper;
    if wrapper.is_empty() {
        return Ok(None);
    }
    let wrapper_path = Path::new(&wrapper);
    if wrapper_path.is_absolute() || wrapper_path.components().count() == 1 {
        return Ok(Some(wrapper.into()));
    }

    let origin = configured_value_origin(manifest_path, toolchain, cargo)?;
    let config_directory = origin.parent().ok_or_else(|| {
        format!(
            "Cargo rustc-wrapper configuration origin {} has no parent directory",
            origin.display()
        )
    })?;
    let relative_base = if config_directory.file_name() == Some(OsStr::new(".cargo")) {
        config_directory.parent().unwrap_or(config_directory)
    } else {
        config_directory
    };
    let relative_wrapper = wrapper_path
        .components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect::<PathBuf>();
    Ok(Some(relative_base.join(relative_wrapper).into_os_string()))
}

fn configured_value_origin(
    manifest_path: &Path,
    toolchain: &str,
    cargo: &OsStr,
) -> Result<PathBuf, String> {
    let mut command = Command::new(cargo);
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
            "build.rustc-wrapper",
            "--show-origin",
        ])
        .output()
        .map_err(|error| {
            format!(
                "failed to ask Cargo +{toolchain} for the rustc wrapper configuration origin: {error}"
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "failed to read Cargo +{toolchain}'s configured rustc wrapper origin: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let origin = stdout
        .lines()
        .find_map(|line| line.rsplit_once(" # ").map(|(_, origin)| origin.trim()))
        .filter(|origin| !origin.is_empty())
        .ok_or_else(|| {
            format!(
                "failed to parse Cargo's configured rustc wrapper origin: {}",
                stdout.trim()
            )
        })?;
    Ok(PathBuf::from(origin))
}

pub(crate) fn target_selection(
    manifest_path: &Path,
    command_line_target: Option<&str>,
    host_triple: &str,
    toolchain: &str,
) -> Result<CargoTargetSelection, String> {
    if let Some(target) = command_line_target {
        let effective_target = if target == "host" {
            host_triple
        } else {
            target
        };
        return Ok(CargoTargetSelection {
            toolchain: toolchain.to_string(),
            effective_triple: effective_target.to_string(),
            cargo_platform: Some(effective_target.to_string()),
            command_line_override: Some(target.to_string()),
        });
    }

    let Some(configured_target) = configured_build_target(manifest_path, toolchain)? else {
        return Ok(CargoTargetSelection {
            toolchain: toolchain.to_string(),
            effective_triple: host_triple.to_string(),
            cargo_platform: None,
            command_line_override: None,
        });
    };
    let effective_target = if configured_target == "host" {
        host_triple.to_string()
    } else {
        configured_target
    };
    Ok(CargoTargetSelection {
        toolchain: toolchain.to_string(),
        effective_triple: effective_target.clone(),
        cargo_platform: Some(effective_target),
        command_line_override: None,
    })
}

fn configured_build_target(
    manifest_path: &Path,
    toolchain: &str,
) -> Result<Option<String>, String> {
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
    select_configured_targets(config.build.target)
}

fn select_configured_targets(targets: ConfiguredCargoTargets) -> Result<Option<String>, String> {
    match targets {
        ConfiguredCargoTargets::One(target) => Ok(Some(target)),
        ConfiguredCargoTargets::Many(targets) if targets.is_empty() => Ok(None),
        ConfiguredCargoTargets::Many(mut targets) if targets.len() == 1 => Ok(targets.pop()),
        ConfiguredCargoTargets::Many(targets) => Err(format!(
            "Cargo build.target selects multiple targets ({}); pass --target TRIPLE to select one documentation target",
            targets.join(", ")
        )),
    }
}

pub(crate) fn resolved_unit(request: CargoUnitRequest<'_>) -> Result<CargoUnitIdentity, String> {
    let CargoUnitRequest {
        manifest_path,
        metadata,
        root_package,
        package,
        target,
        contexts,
        target_selection,
        feature_selection,
    } = request;
    let toolchain = &target_selection.toolchain;
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
    command.args(feature_selection.cargo_args());
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        let hint = if is_lockfile_failure(&stderr) {
            "; Cargo.lock is missing or stale; run `cargo check` or `cargo build` to refresh it, then retry"
        } else {
            ""
        };
        return Err(format!(
            "failed to resolve the exact Cargo feature unit for {} {} without changing Cargo.lock: {}{hint}",
            package.name,
            package.version,
            stderr.trim()
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
    let mut candidates = units_for_context_edges(
        &graph,
        metadata,
        root_package,
        package,
        target,
        &contexts[0],
    )
    .into_iter()
    .filter_map(|index| graph.units.get(index))
    .filter(|unit| unit.mode == expected_mode && unit.platform.as_deref() == expected_platform)
    .map(unit_identity)
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

fn units_for_context_edges(
    graph: &UnitGraph,
    metadata: &Metadata,
    root_package: &Package,
    package: &Package,
    target: &Target,
    context: &DependencyContext,
) -> Vec<usize> {
    let root_id = root_package.id.to_string();
    let roots = graph
        .roots
        .iter()
        .copied()
        .filter(|index| {
            graph
                .units
                .get(*index)
                .is_some_and(|unit| unit.pkg_id == root_id)
        })
        .collect::<Vec<_>>();
    let (anchors, exclude_custom_build) = if context.kind == DependencyKind::Build {
        (
            reachable_units(graph, &roots, false)
                .into_iter()
                .filter(|index| {
                    graph
                        .units
                        .get(*index)
                        .is_some_and(|unit| unit.pkg_id == root_id && is_custom_build_unit(unit))
                })
                .collect::<Vec<_>>(),
            false,
        )
    } else {
        (roots, true)
    };

    let parents = if let Some(via) = &context.via {
        let parent_ids = metadata
            .packages
            .iter()
            .filter(|candidate| candidate.name == *via)
            .map(|candidate| candidate.id.to_string())
            .collect::<HashSet<_>>();
        reachable_units(graph, &anchors, exclude_custom_build)
            .into_iter()
            .filter(|index| {
                graph
                    .units
                    .get(*index)
                    .is_some_and(|unit| parent_ids.contains(&unit.pkg_id))
            })
            .collect::<Vec<_>>()
    } else {
        anchors
    };

    let package_id = package.id.to_string();
    let mut candidates = parents
        .into_iter()
        .filter_map(|index| graph.units.get(index))
        .flat_map(|unit| unit.dependencies.iter())
        .filter_map(|dependency| {
            let unit = graph.units.get(dependency.index)?;
            (unit.pkg_id == package_id && unit.target.name == target.name)
                .then_some(dependency.index)
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

fn reachable_units(graph: &UnitGraph, roots: &[usize], exclude_custom_build: bool) -> Vec<usize> {
    let mut visited = HashSet::new();
    let mut pending = roots.to_vec();
    while let Some(index) = pending.pop() {
        if !visited.insert(index) {
            continue;
        }
        let Some(unit) = graph.units.get(index) else {
            continue;
        };
        for dependency in &unit.dependencies {
            if exclude_custom_build
                && graph
                    .units
                    .get(dependency.index)
                    .is_some_and(is_custom_build_unit)
            {
                continue;
            }
            pending.push(dependency.index);
        }
    }
    let mut reachable = visited.into_iter().collect::<Vec<_>>();
    reachable.sort_unstable();
    reachable
}

fn is_custom_build_unit(unit: &Unit) -> bool {
    unit.target.kind.iter().any(|kind| kind == "custom-build")
}

fn unit_identity(unit: &Unit) -> CargoUnitIdentity {
    let mut features = unit.features.clone();
    features.sort();
    CargoUnitIdentity {
        features,
        mode: unit.mode.clone(),
        platform: unit.platform.clone(),
        profile: serde_json::to_string(&unit.profile).expect("Cargo profile serializes"),
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

fn format_generate_error(package: &Package, toolchain: &str, stderr: &str) -> String {
    let hint = if is_missing_toolchain_diagnostic(stderr) {
        format!(
            "; compatible nightly toolchain not found; install with `rustup toolchain install {toolchain}` or set CHECK_DOCS_TOOLCHAIN"
        )
    } else if stderr.contains("lock file") && stderr.contains("needs to be updated") {
        "; Cargo.lock is missing or stale; run `cargo check` or `cargo build` to refresh it, then retry".to_string()
    } else if stderr.contains("unstable-options") || stderr.contains("output-format") {
        "; rustdoc JSON requires nightly and `-Z unstable-options`".to_string()
    } else {
        String::new()
    };
    format!(
        "failed to generate rustdoc JSON for {} {}{hint}: {}",
        package.name,
        package.version,
        stderr.trim()
    )
}

fn is_missing_toolchain_diagnostic(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        (line.contains("toolchain") && line.contains("is not installed"))
            || line.contains("no such installed toolchain")
    })
}

fn is_lockfile_failure(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("lock file")
        && (lower.contains("needs to be updated")
            || lower.contains("needs to be generated")
            || lower.contains("--locked"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::MetadataCommand;
    use rustdoc_types::{Id, Item, ItemEnum, Module, Target as RustdocTarget, Visibility};
    use std::collections::HashMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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
        assert_eq!(
            err,
            format!(
                "failed to generate rustdoc JSON for {} {}; compatible nightly toolchain not found; install with `rustup toolchain install nightly` or set CHECK_DOCS_TOOLCHAIN: toolchain 'nightly' is not installed",
                pkg.name, pkg.version
            )
        );

        let unrelated = format_generate_error(
            &pkg,
            "nightly",
            "error[E0425]: cannot find value `toolchain` in this scope",
        );
        assert_eq!(
            unrelated,
            format!(
                "failed to generate rustdoc JSON for {} {}: error[E0425]: cannot find value `toolchain` in this scope",
                pkg.name, pkg.version
            )
        );

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
            toolchain: PINNED_TOOLCHAIN.into(),
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
        let feature_selection = FeatureSelection::default();
        let request = RustdocRequest {
            manifest_path: PathBuf::from("Cargo.toml"),
            metadata: &metadata(),
            root_package: &pkg,
            package: &pkg,
            target: &target,
            contexts: &contexts,
            target_selection: &target_selection,
            feature_selection: &feature_selection,
            unit: &unit,
        };
        let err = generate_json_with_toolchain(
            &request,
            output.path(),
            output.path(),
            "definitely_missing_check_docs_toolchain",
        )
        .unwrap_err();
        assert!(
            err.contains("definitely_missing_check_docs_toolchain"),
            "{err}"
        );
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
    fn compiler_wrapper_distinguishes_complete_profile_identity() {
        let selected = serde_json::json!({
            "name": "test",
            "opt_level": "1",
            "lto": "thin",
            "codegen_backend": "llvm",
            "codegen_units": 1,
            "debuginfo": 1,
            "split_debuginfo": "packed",
            "debug_assertions": true,
            "overflow_checks": true,
            "rpath": true,
            "incremental": true,
            "panic": "unwind",
            "strip": { "resolved": { "Named": "debuginfo" } },
        });
        let selected_cfg = RustcCfg::parse(
            b"debug_assertions\noverflow_checks\npanic=\"unwind\"\ntarget_os=\"linux\"\n",
        );
        let arguments = [
            OsString::from("-Copt-level=1"),
            OsString::from("-Clinker-plugin-lto"),
            OsString::from("-Z"),
            OsString::from("codegen-backend=llvm"),
            OsString::from("-Ccodegen-units=1"),
            OsString::from("-C"),
            OsString::from("debuginfo=1"),
            OsString::from("-Csplit-debuginfo=packed"),
            OsString::from("-Crpath"),
            OsString::from("-Cincremental=/tmp/incremental"),
            OsString::from("-Cpanic=unwind"),
            OsString::from("-Cstrip=debuginfo"),
        ];
        let arguments = arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();

        assert!(profile_matches_cfg(
            &selected,
            &selected_cfg,
            &arguments,
            "unwind"
        ));

        for (field, value) in [
            ("opt_level", serde_json::json!("0")),
            ("lto", serde_json::json!("false")),
            ("codegen_backend", serde_json::Value::Null),
            ("codegen_units", serde_json::json!(2)),
            ("debuginfo", serde_json::json!(2)),
            ("split_debuginfo", serde_json::Value::Null),
            ("rpath", serde_json::json!(false)),
            ("incremental", serde_json::json!(false)),
            ("panic", serde_json::json!("abort")),
            (
                "strip",
                serde_json::json!({ "resolved": { "Named": "symbols" } }),
            ),
        ] {
            let mut different = selected.clone();
            different[field] = value;
            assert!(
                !profile_matches_cfg(&different, &selected_cfg, &arguments, "unwind"),
                "profile field {field} was not matched"
            );
        }

        let build_cfg = RustcCfg::parse(b"panic=\"unwind\"\ntarget_os=\"linux\"\n");
        assert!(!profile_matches_cfg(
            &selected, &build_cfg, &arguments, "unwind"
        ));
        let missing_panic = RustcCfg::parse(b"debug_assertions\noverflow_checks\n");
        assert!(!profile_matches_cfg(
            &selected,
            &missing_panic,
            &arguments,
            "unwind"
        ));

        let defaults = serde_json::json!({
            "name": "test",
            "opt_level": "0",
            "lto": "false",
            "codegen_backend": null,
            "codegen_units": null,
            "debuginfo": 0,
            "split_debuginfo": null,
            "debug_assertions": false,
            "overflow_checks": false,
            "rpath": false,
            "incremental": false,
            "panic": "unwind",
            "strip": { "deferred": "None" },
        });
        let default_cfg = RustcCfg::parse(b"panic=\"unwind\"\n");
        assert!(profile_matches_cfg(&defaults, &default_cfg, &[], "unwind"));
        let abort_default_cfg = RustcCfg::parse(b"panic=\"abort\"\n");
        assert!(profile_matches_cfg(
            &defaults,
            &abort_default_cfg,
            &[],
            "abort"
        ));
    }

    #[test]
    fn retained_cfgs_are_evaluated_without_rustdocs_doc_flag() {
        let mut docs = minimal_crate(None, FORMAT_VERSION);
        let root = docs.root;
        let mut doc_only = Item {
            id: Id(2),
            crate_id: 0,
            name: Some("DocOnly".into()),
            span: None,
            visibility: Visibility::Public,
            docs: None,
            links: HashMap::new(),
            attrs: vec![rustdoc_types::Attribute::Other(
                "#[<cfg>(any(doc, windows))]".into(),
            )],
            deprecation: None,
            inner: ItemEnum::Module(Module {
                is_crate: false,
                items: Vec::new(),
                is_stripped: false,
            }),
        };
        let unix_only = Item {
            id: Id(3),
            name: Some("UnixOnly".into()),
            attrs: vec![rustdoc_types::Attribute::Other(
                "#[<cfg>(all(unix, target_os = \"linux\"))]".into(),
            )],
            ..doc_only.clone()
        };
        let cfg_attr_only = Item {
            id: Id(4),
            name: Some("CfgAttrOnly".into()),
            attrs: vec![rustdoc_types::Attribute::Other(
                "#[<cfg_attr>(not(doc), cfg(windows))]".into(),
            )],
            ..doc_only.clone()
        };
        doc_only.id = Id(2);
        docs.index.insert(Id(2), doc_only);
        docs.index.insert(Id(3), unix_only);
        docs.index.insert(Id(4), cfg_attr_only);
        let ItemEnum::Module(root_module) = &mut docs.index.get_mut(&root).unwrap().inner else {
            panic!("crate root is a module");
        };
        root_module.items = vec![Id(2), Id(3), Id(4)];
        let cfg = RustcCfg::parse(b"unix\ntarget_os=\"linux\"\npanic=\"unwind\"\n");

        apply_non_doc_cfg(&mut docs, &cfg);

        let ItemEnum::Module(root_module) = &docs.index[&root].inner else {
            panic!("crate root is a module");
        };
        assert_eq!(root_module.items, [Id(3)]);
        assert!(docs.index[&Id(2)].attrs.iter().any(|attribute| {
            matches!(attribute, rustdoc_types::Attribute::Other(attribute) if attribute == CFG_UNAVAILABLE_ATTRIBUTE)
        }));
        assert!(docs.index[&Id(4)].attrs.iter().any(|attribute| {
            matches!(attribute, rustdoc_types::Attribute::Other(attribute) if attribute == CFG_UNAVAILABLE_ATTRIBUTE)
        }));
    }

    #[test]
    fn retained_cfg_attr_derives_follow_the_captured_non_doc_cfg() {
        let mut attrs = vec![
            rustdoc_types::Attribute::Other(
                "#[<cfg_attr>(feature = \"enabled\", derive(Enabled, marker::Qualified))]".into(),
            ),
            rustdoc_types::Attribute::Other(
                "#[<cfg_attr>(feature = \"disabled\", derive(Disabled))]".into(),
            ),
            rustdoc_types::Attribute::Other(
                "#[<cfg_attr>(feature = \"enabled\", cfg_attr(unix, derive(Nested)))]".into(),
            ),
        ];
        let cfg = RustcCfg::parse(b"feature=\"enabled\"\nunix\n");

        activate_cfg_attr_derives(&mut attrs, &cfg);

        assert!(attrs.iter().any(|attribute| {
            matches!(
                attribute,
                rustdoc_types::Attribute::Other(attribute)
                    if attribute == "#[derive(Enabled, Nested, marker::Qualified)]"
            )
        }));
        assert!(!attrs.iter().any(|attribute| {
            matches!(
                attribute,
                rustdoc_types::Attribute::Other(attribute)
                    if attribute.starts_with("#[derive(") && attribute.contains("Disabled")
            )
        }));
    }

    #[test]
    fn cfg_lookup_normalizes_raw_identifiers_without_changing_rendered_paths() {
        let cfg = RustcCfg::parse(b"async\nmode=\"fast\"\n");

        assert!(cfg_expression_matches("r#async", &cfg));
        assert!(cfg_expression_matches("r#mode = \"fast\"", &cfg));
        let path = syn::parse_str::<syn::Path>("r#async::r#type").unwrap();
        assert_eq!(cfg_path(&path), "r#async::r#type");
        assert_eq!(cfg_lookup_path(&path), "async::type");

        let mut attrs = vec![rustdoc_types::Attribute::Other(
            "#[<cfg_attr>(r#async, derive(RawEnabled))]".into(),
        )];
        activate_cfg_attr_derives(&mut attrs, &cfg);
        assert!(attrs.iter().any(|attribute| {
            matches!(
                attribute,
                rustdoc_types::Attribute::Other(attribute)
                    if attribute == "#[derive(RawEnabled)]"
            )
        }));
    }

    #[test]
    fn configured_target_forms_follow_cargo_semantics() {
        assert_eq!(
            select_configured_targets(ConfiguredCargoTargets::One("host".into())).unwrap(),
            Some("host".into())
        );
        assert_eq!(
            select_configured_targets(ConfiguredCargoTargets::Many(vec!["host".into()])).unwrap(),
            Some("host".into())
        );
        assert_eq!(
            select_configured_targets(ConfiguredCargoTargets::Many(Vec::new())).unwrap(),
            None
        );
        let error = select_configured_targets(ConfiguredCargoTargets::Many(vec![
            "host".into(),
            "wasm32-unknown-unknown".into(),
        ]))
        .unwrap_err();
        assert!(error.contains("selects multiple targets (host, wasm32-unknown-unknown)"));
    }

    #[test]
    fn generation_directory_requires_and_refreshes_its_ownership_marker() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("check-docs");
        let first = reset_generation_target_dir(&root).unwrap();
        fs::write(first.join("stale"), "stale").unwrap();

        let second = reset_generation_target_dir(&root).unwrap();
        assert_eq!(first, second);
        assert!(!second.join("stale").exists());
        assert_eq!(
            fs::read_to_string(second.join(".check-docs-generation")).unwrap(),
            GENERATION_MARKER
        );

        fs::write(second.join(".check-docs-generation"), "not ours\n").unwrap();
        assert!(
            reset_generation_target_dir(&root)
                .unwrap_err()
                .contains("refusing to replace unowned")
        );
    }

    #[test]
    fn generation_directory_recovers_past_interrupted_staging_initialization() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("check-docs");
        fs::create_dir_all(&root).unwrap();
        let unmarked = root.join("generation.staging-0");
        fs::create_dir(&unmarked).unwrap();
        fs::write(unmarked.join("keep.txt"), "not owned\n").unwrap();
        let owned = root.join("generation.staging-1");
        fs::create_dir(&owned).unwrap();
        fs::write(owned.join(".check-docs-generation"), GENERATION_MARKER).unwrap();
        fs::write(owned.join("stale"), "stale\n").unwrap();

        let generation = reset_generation_target_dir(&root).unwrap();

        assert_eq!(
            fs::read_to_string(generation.join(".check-docs-generation")).unwrap(),
            GENERATION_MARKER
        );
        assert_eq!(
            fs::read_to_string(unmarked.join("keep.txt")).unwrap(),
            "not owned\n"
        );
        assert!(!owned.exists());
    }

    #[test]
    fn generation_directory_without_a_marker_remains_unowned() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("check-docs");
        let generation = root.join("generation");
        fs::create_dir_all(&generation).unwrap();
        fs::write(generation.join("keep.txt"), "keep me\n").unwrap();

        let error = reset_generation_target_dir(&root).unwrap_err();

        assert!(error.contains("refusing to replace unowned"));
        assert_eq!(
            fs::read_to_string(generation.join("keep.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn generation_root_rejects_a_symlink_and_preserves_its_destination() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("target");
        let victim = temp.path().join("victim");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("keep.txt"), "keep me\n").unwrap();
        symlink(&victim, target.join("check-docs")).unwrap();

        let error = prepare_generation_root(&target).unwrap_err();

        assert!(error.contains("non-directory or symlink check-docs managed root"));
        assert_eq!(
            fs::read_to_string(victim.join("keep.txt")).unwrap(),
            "keep me\n"
        );
        assert!(
            fs::symlink_metadata(target.join("check-docs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn generation_directory_rejects_a_symlink_ownership_marker() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("check-docs");
        let generation = reset_generation_target_dir(&root).unwrap();
        let marker = generation.join(".check-docs-generation");
        let victim = temp.path().join("victim");
        fs::write(&victim, GENERATION_MARKER).unwrap();
        fs::remove_file(&marker).unwrap();
        symlink(&victim, &marker).unwrap();

        let error = reset_generation_target_dir(&root).unwrap_err();

        assert!(error.contains("ownership marker"));
        assert!(error.contains("not a regular non-symlink file"));
        assert_eq!(fs::read_to_string(&victim).unwrap(), GENERATION_MARKER);
        assert!(generation.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn configured_wrapper_uses_selected_toolchain_and_cargo_origin() {
        let temp = tempfile::TempDir::new().unwrap();
        let project = temp.path().join("project");
        let cargo_dir = temp.path().join(".cargo");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&cargo_dir).unwrap();
        let fake_cargo = temp.path().join("fake-cargo");
        let origin = cargo_dir.join("config.toml");
        fs::write(
            &fake_cargo,
            format!(
                "#!/bin/sh\nif [ \"$1\" != \"+alternate-nightly\" ]; then exit 41; fi\ncase \" $* \" in\n  *\" --show-origin \"*) printf '%s\\n' 'build.rustc-wrapper = \"./tools/wrapper\" # {}' ;;\n  *) printf '%s\\n' '{{\"build\":{{\"rustc-wrapper\":\"./tools/wrapper\"}}}}' ;;\nesac\n",
                origin.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_cargo).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_cargo, permissions).unwrap();

        let wrapper = configured_general_rustc_wrapper(
            &project.join("Cargo.toml"),
            "alternate-nightly",
            fake_cargo.as_os_str(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(wrapper, temp.path().join("tools/wrapper").into_os_string());
    }

    #[test]
    fn cargo_unit_selection_follows_contextual_dependency_edges() {
        fn unit(
            pkg_id: String,
            name: &str,
            kind: &str,
            features: &[&str],
            dependencies: &[usize],
        ) -> Unit {
            Unit {
                pkg_id,
                target: UnitTarget {
                    name: name.into(),
                    kind: vec![kind.into()],
                },
                mode: "build".into(),
                platform: None,
                features: features.iter().map(|feature| (*feature).into()).collect(),
                profile: serde_json::json!({"name": "test"}),
                dependencies: dependencies
                    .iter()
                    .map(|index| UnitDependency { index: *index })
                    .collect(),
            }
        }

        let metadata = metadata();
        let root = metadata.root_package().unwrap();
        let dependency = metadata
            .packages
            .iter()
            .find(|package| package.name == "cargo_metadata")
            .unwrap();
        let target = crate::resolver::library_target(dependency).unwrap();
        let graph = UnitGraph {
            version: 1,
            roots: vec![0],
            units: vec![
                unit(root.id.to_string(), "check-docs", "bin", &[], &[1, 2]),
                unit(
                    dependency.id.to_string(),
                    &target.name,
                    "lib",
                    &["dev-api"],
                    &[],
                ),
                unit(
                    root.id.to_string(),
                    "build-script-build",
                    "custom-build",
                    &[],
                    &[3],
                ),
                unit(
                    dependency.id.to_string(),
                    &target.name,
                    "lib",
                    &["build-api"],
                    &[],
                ),
            ],
        };
        let dev = DependencyContext {
            kind: DependencyKind::Development,
            target: None,
            via: None,
        };
        let build = DependencyContext {
            kind: DependencyKind::Build,
            target: None,
            via: None,
        };

        assert_eq!(
            units_for_context_edges(&graph, &metadata, root, dependency, target, &dev),
            vec![1]
        );
        assert_eq!(
            units_for_context_edges(&graph, &metadata, root, dependency, target, &build),
            vec![3]
        );
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
    fn lock_uses_an_empty_file_and_releases_exclusive_lock_on_drop() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("crate.json.lock");
        let first =
            JsonGenerationLock::acquire_with_timeout(lock_path.clone(), Duration::ZERO).unwrap();
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), "");

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
    fn lock_preserves_existing_contents_when_no_owner_is_active() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("stale.json.lock");
        fs::write(&lock_path, "existing contents\n").unwrap();
        let _lock =
            JsonGenerationLock::acquire_with_timeout(lock_path.clone(), Duration::ZERO).unwrap();
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            "existing contents\n"
        );
    }

    #[test]
    fn lock_waits_for_a_healthy_owner_to_release() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("concurrent.json.lock");
        let first = JsonGenerationLock::acquire(lock_path.clone()).unwrap();
        let holder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(first);
        });

        let second = JsonGenerationLock::acquire(lock_path).unwrap();
        holder.join().unwrap();
        drop(second);
    }

    #[test]
    fn lock_rejects_non_regular_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("lock-directory");
        fs::create_dir(&lock_path).unwrap();
        let error = JsonGenerationLock::acquire_with_timeout(lock_path.clone(), Duration::ZERO)
            .unwrap_err();
        assert_eq!(error, invalid_lock_path_message(&lock_path));
    }

    #[cfg(unix)]
    #[test]
    fn lock_rejects_symlinks_without_modifying_the_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let victim = dir.path().join("victim");
        let lock_path = dir.path().join("symlink.lock");
        fs::write(&victim, "keep me\n").unwrap();
        symlink(&victim, &lock_path).unwrap();

        let error = JsonGenerationLock::acquire_with_timeout(lock_path.clone(), Duration::ZERO)
            .unwrap_err();
        assert_eq!(error, invalid_lock_path_message(&lock_path));
        assert_eq!(fs::read_to_string(victim).unwrap(), "keep me\n");
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

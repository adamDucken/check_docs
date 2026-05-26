use cargo_metadata::{DependencyKind, Metadata, Package, PackageId, Target};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct ResolvedDependency<'a> {
    pub(crate) package: &'a Package,
    pub(crate) target: &'a Target,
    pub(crate) contexts: Vec<DependencyContext>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DependencyFilter {
    pub(crate) include_dev: bool,
    pub(crate) include_build: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DependencyContext {
    pub(crate) kind: DependencyKind,
    pub(crate) target: Option<String>,
    pub(crate) via: Option<String>,
}

impl DependencyContext {
    pub(crate) fn label(&self) -> String {
        let kind = match self.kind {
            DependencyKind::Normal => "normal",
            DependencyKind::Development => "dev",
            DependencyKind::Build => "build",
            _ => "unknown",
        };
        let label = match &self.target {
            Some(target) => format!("{kind} ({target})"),
            None => kind.to_string(),
        };
        match &self.via {
            Some(via) => format!("transitive via {via} ({label})"),
            None => label,
        }
    }
}

fn transitive_context(
    kind: DependencyKind,
    target: Option<String>,
    source_crate: &str,
) -> DependencyContext {
    DependencyContext {
        kind,
        target,
        via: Some(source_crate.to_string()),
    }
}

fn normalized_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

fn package_by_id<'a>(packages: &'a [Package], package_id: &PackageId) -> Option<&'a Package> {
    packages.iter().find(|pkg| &pkg.id == package_id)
}

fn dependency_matches_crate_name(
    packages: &[Package],
    dep: &cargo_metadata::NodeDep,
    crate_name: &str,
) -> bool {
    if normalized_crate_name(&dep.name) == crate_name {
        return true;
    }
    let Some(package) = package_by_id(packages, &dep.pkg) else {
        return false;
    };
    normalized_crate_name(&package.name) == crate_name
        || library_target(package)
            .map(|target| normalized_crate_name(&target.name) == crate_name)
            .unwrap_or(false)
}

pub(crate) fn resolve_dependency_from_package<'a>(
    metadata: &Metadata,
    packages: &'a [Package],
    source_package: &Package,
    crate_name: &str,
) -> Result<ResolvedDependency<'a>, String> {
    let Some(resolve) = metadata.resolve.as_ref() else {
        return Err("cargo metadata did not include a dependency graph".to_string());
    };
    let Some(node) = resolve
        .nodes
        .iter()
        .find(|node| node.id == source_package.id)
    else {
        return Err(format!(
            "package '{}' missing from cargo metadata dependency graph",
            source_package.name
        ));
    };

    let mut matches = Vec::<DependencyEntry>::new();
    for dep in &node.deps {
        if !dependency_matches_crate_name(packages, dep, crate_name) {
            continue;
        }
        for dep_kind in &dep.dep_kinds {
            push_dependency_context(
                &mut matches,
                dep.pkg.clone(),
                transitive_context(
                    dep_kind.kind,
                    dep_kind.target.as_ref().map(ToString::to_string),
                    &source_package.name,
                ),
            );
        }
    }

    let mut package_ids = matches
        .iter()
        .map(|entry| entry.package_id.clone())
        .collect::<Vec<_>>();
    package_ids.sort_by(|left, right| left.repr.cmp(&right.repr));
    package_ids.dedup();

    match package_ids.as_slice() {
        [] => Err(format!(
            "crate '{crate_name}' is not a dependency of direct dependency '{}'",
            source_package.name
        )),
        [_] => {
            let entry = matches
                .iter()
                .find(|entry| entry.package_id == package_ids[0])
                .expect("entry exists for deduplicated package id");
            let package = package_by_id(packages, &entry.package_id).ok_or_else(|| {
                format!(
                    "dependency '{crate_name}' of '{}' missing from cargo metadata",
                    source_package.name
                )
            })?;
            let target = library_target(package)?;
            Ok(ResolvedDependency {
                package,
                target,
                contexts: entry.contexts.clone(),
            })
        }
        _ => {
            let candidates = package_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "crate '{crate_name}' matched multiple dependencies of '{}': {candidates}",
                source_package.name
            ))
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DependencyEntry {
    pub(crate) package_id: PackageId,
    pub(crate) contexts: Vec<DependencyContext>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DependencyIndex {
    entries: HashMap<String, Vec<DependencyEntry>>,
}

impl DependencyIndex {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, crate_name: &str) -> bool {
        self.entries.contains_key(crate_name)
    }

    #[cfg(test)]
    pub(crate) fn insert(&mut self, crate_name: String, entry: DependencyEntry) {
        self.entries.entry(crate_name).or_default().push(entry);
    }
}

fn push_dependency_context(
    entries: &mut Vec<DependencyEntry>,
    package_id: PackageId,
    context: DependencyContext,
) {
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.package_id == package_id)
    {
        if !entry.contexts.contains(&context) {
            entry.contexts.push(context);
            sort_dependency_contexts(&mut entry.contexts);
        }
        return;
    }

    entries.push(DependencyEntry {
        package_id,
        contexts: vec![context],
    });
}

fn sort_dependency_contexts(contexts: &mut [DependencyContext]) {
    contexts.sort_by(|left, right| {
        dependency_kind_order(left.kind)
            .cmp(&dependency_kind_order(right.kind))
            .then_with(|| left.target.cmp(&right.target))
            .then_with(|| left.via.cmp(&right.via))
    });
}

fn dependency_kind_order(kind: DependencyKind) -> u8 {
    match kind {
        DependencyKind::Normal => 0,
        DependencyKind::Development => 1,
        DependencyKind::Build => 2,
        _ => 3,
    }
}

pub(crate) fn is_rust_library_crate(crate_name: &str) -> bool {
    matches!(crate_name, "std" | "core" | "alloc")
}

pub(crate) fn select_package<'a>(
    metadata: &'a Metadata,
    manifest_path: &Path,
    package_selector: Option<&str>,
) -> Result<&'a Package, String> {
    if let Some(selector) = package_selector {
        let mut matches = metadata
            .workspace_members
            .iter()
            .filter_map(|id| package_by_id(&metadata.packages, id))
            .filter(|package| package.name == selector || package.id.to_string() == selector)
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.id.repr.cmp(&right.id.repr));
        return match matches.as_slice() {
            [] => Err(format!("package '{selector}' not found in workspace")),
            [package] => Ok(*package),
            packages => {
                let candidates = packages
                    .iter()
                    .map(|package| package.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!(
                    "package '{selector}' matched multiple workspace packages: {candidates}"
                ))
            }
        };
    }

    if let Some(package) = metadata.root_package() {
        return Ok(package);
    }

    let root = manifest_path
        .parent()
        .unwrap_or(manifest_path)
        .canonicalize()
        .unwrap_or_else(|_| {
            manifest_path
                .parent()
                .unwrap_or(manifest_path)
                .to_path_buf()
        });
    Err(format!(
        "virtual workspace root {} has no root package; pass --package <name-or-id>",
        root.display()
    ))
}

pub(crate) fn package_dependencies(
    metadata: &Metadata,
    package_id: &PackageId,
    filter: DependencyFilter,
) -> DependencyIndex {
    let mut deps = DependencyIndex::default();
    if let Some(resolve) = metadata.resolve.as_ref()
        && let Some(node) = resolve.nodes.iter().find(|node| &node.id == package_id)
    {
        for dep in &node.deps {
            for dep_kind in &dep.dep_kinds {
                if !dependency_kind_allowed(dep_kind.kind, filter) {
                    continue;
                }
                let crate_name = dep.name.replace('-', "_");
                let entries = deps.entries.entry(crate_name).or_default();
                push_dependency_context(
                    entries,
                    dep.pkg.clone(),
                    DependencyContext {
                        kind: dep_kind.kind,
                        target: dep_kind.target.as_ref().map(ToString::to_string),
                        via: None,
                    },
                );
            }
        }
    }
    deps
}

fn dependency_kind_allowed(kind: DependencyKind, filter: DependencyFilter) -> bool {
    match kind {
        DependencyKind::Normal => true,
        DependencyKind::Development => filter.include_dev,
        DependencyKind::Build => filter.include_build,
        _ => false,
    }
}

pub(crate) fn resolve_dependency<'a>(
    packages: &'a [Package],
    dependencies: &DependencyIndex,
    crate_name: &str,
) -> Result<ResolvedDependency<'a>, String> {
    let Some(entries) = dependencies.entries.get(crate_name) else {
        return Err(format!(
            "crate '{crate_name}' is not a direct dependency of the selected package"
        ));
    };
    if entries.len() > 1 {
        let candidates = entries
            .iter()
            .map(|entry| {
                let contexts = entry
                    .contexts
                    .iter()
                    .map(DependencyContext::label)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} [{contexts}]", entry.package_id)
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "crate '{crate_name}' matched multiple direct dependencies: {candidates}"
        ));
    }
    let entry = &entries[0];

    let package = package_by_id(packages, &entry.package_id)
        .ok_or_else(|| format!("direct dependency '{crate_name}' missing from cargo metadata"))?;
    let target = library_target(package)?;
    Ok(ResolvedDependency {
        package,
        target,
        contexts: entry.contexts.clone(),
    })
}

pub(crate) fn library_target(package: &Package) -> Result<&Target, String> {
    package
        .targets
        .iter()
        .find(|target| {
            target
                .kind
                .iter()
                .any(|kind| kind == "lib" || kind == "proc-macro")
        })
        .ok_or_else(|| format!("package {} has no doc-able library target", package.name))
}

pub(crate) fn package_spec(package: &Package) -> String {
    package.id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::MetadataCommand;
    use std::fs;
    use tempfile::TempDir;

    fn metadata() -> Metadata {
        MetadataCommand::new()
            .manifest_path("Cargo.toml")
            .exec()
            .unwrap()
    }

    fn default_filter() -> DependencyFilter {
        DependencyFilter {
            include_dev: false,
            include_build: false,
        }
    }

    fn virtual_workspace() -> TempDir {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("member_a/src")).unwrap();
        fs::create_dir_all(temp.path().join("member_b/src")).unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            r#"
[workspace]
members = ["member_a", "member_b"]
"#,
        )
        .unwrap();
        fs::write(
            temp.path().join("member_a/Cargo.toml"),
            r#"
[package]
name = "member_a"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();
        fs::write(temp.path().join("member_a/src/lib.rs"), "").unwrap();
        fs::write(
            temp.path().join("member_b/Cargo.toml"),
            r#"
[package]
name = "member_b"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();
        fs::write(temp.path().join("member_b/src/lib.rs"), "").unwrap();
        temp
    }

    fn metadata_for(manifest_path: &Path) -> Metadata {
        MetadataCommand::new()
            .manifest_path(manifest_path)
            .exec()
            .unwrap()
    }

    #[test]
    fn detects_unsupported_rust_library_crates() {
        assert!(is_rust_library_crate("std"));
        assert!(is_rust_library_crate("core"));
        assert!(is_rust_library_crate("alloc"));
        assert!(!is_rust_library_crate("syn"));
    }

    #[test]
    fn resolves_package_for_exact_manifest_and_its_dependencies() {
        let metadata = metadata();
        let package = select_package(&metadata, Path::new("Cargo.toml"), None).unwrap();
        assert_eq!(package.name, "check_docs");

        let deps = package_dependencies(&metadata, &package.id, default_filter());
        assert!(deps.contains_key("cargo_metadata"));

        let dep = resolve_dependency(&metadata.packages, &deps, "cargo_metadata").unwrap();
        assert_eq!(dep.package.name, "cargo_metadata");
        assert!(library_target(dep.package).is_ok());
    }

    #[test]
    fn selects_workspace_member_by_package_name_or_id() {
        let temp = virtual_workspace();
        let manifest_path = temp.path().join("Cargo.toml");
        let metadata = metadata_for(&manifest_path);

        let member_a = select_package(&metadata, &manifest_path, Some("member_a")).unwrap();
        assert_eq!(member_a.name, "member_a");

        let by_id =
            select_package(&metadata, &manifest_path, Some(&member_a.id.to_string())).unwrap();
        assert_eq!(by_id.id, member_a.id);
    }

    #[test]
    fn virtual_workspace_requires_package_selector() {
        let temp = virtual_workspace();
        let manifest_path = temp.path().join("Cargo.toml");
        let metadata = metadata_for(&manifest_path);

        let err = select_package(&metadata, &manifest_path, None).unwrap_err();
        assert!(err.contains("virtual workspace root"));
        assert!(err.contains("pass --package <name-or-id>"));
    }

    #[test]
    fn unknown_package_selector_reports_workspace_miss() {
        let temp = virtual_workspace();
        let manifest_path = temp.path().join("Cargo.toml");
        let metadata = metadata_for(&manifest_path);

        let err = select_package(&metadata, &manifest_path, Some("missing")).unwrap_err();
        assert_eq!(err, "package 'missing' not found in workspace");
    }

    #[test]
    fn package_spec_preserves_resolved_package_identity() {
        let metadata = metadata();
        let package = select_package(&metadata, Path::new("Cargo.toml"), None).unwrap();
        let deps = package_dependencies(&metadata, &package.id, default_filter());
        let dep = resolve_dependency(&metadata.packages, &deps, "cargo_metadata").unwrap();
        let spec = package_spec(dep.package);

        assert_eq!(spec, dep.package.id.to_string());
        assert_ne!(
            spec,
            format!("{}@{}", dep.package.name, dep.package.version)
        );
        assert!(spec.contains("registry+"));
        assert!(spec.contains("#cargo_metadata@"));
    }

    #[test]
    fn resolver_error_paths_are_explicit() {
        let metadata = metadata();
        let package = select_package(&metadata, Path::new("Cargo.toml"), None).unwrap();
        let deps = package_dependencies(&metadata, &package.id, default_filter());

        let missing =
            resolve_dependency(&metadata.packages, &deps, "definitely_missing_crate").unwrap_err();
        assert!(missing.contains("not a direct dependency"));

        let mut broken_deps = deps.clone();
        broken_deps.insert(
            "missing_dep".into(),
            DependencyEntry {
                package_id: deps.entries["cargo_metadata"][0].package_id.clone(),
                contexts: deps.entries["cargo_metadata"][0].contexts.clone(),
            },
        );
        let missing_metadata = resolve_dependency(&[], &broken_deps, "missing_dep").unwrap_err();
        assert!(missing_metadata.contains("missing from cargo metadata"));

        let no_lib = library_target(package).unwrap_err();
        assert!(no_lib.contains("no doc-able library target"));
    }

    #[test]
    fn dependency_lookup_is_empty_for_unknown_package_id() {
        let metadata = metadata();
        let fake = PackageId {
            repr: "path+file:///missing#0.0.0".to_string(),
        };
        assert!(package_dependencies(&metadata, &fake, default_filter()).is_empty());
    }

    #[test]
    fn filters_dev_dependencies_by_default() {
        let metadata = metadata();
        let package = select_package(&metadata, Path::new("Cargo.toml"), None).unwrap();

        let default_deps = package_dependencies(&metadata, &package.id, default_filter());
        assert!(!default_deps.contains_key("tempfile"));
        assert!(
            resolve_dependency(&metadata.packages, &default_deps, "tempfile")
                .unwrap_err()
                .contains("not a direct dependency")
        );

        let dev_deps = package_dependencies(
            &metadata,
            &package.id,
            DependencyFilter {
                include_dev: true,
                include_build: false,
            },
        );
        let dep = resolve_dependency(&metadata.packages, &dev_deps, "tempfile").unwrap();
        assert_eq!(dep.contexts[0].kind, DependencyKind::Development);
    }

    #[test]
    fn resolves_reexport_target_from_direct_dependency_graph() {
        let metadata = metadata();
        let package = select_package(&metadata, Path::new("Cargo.toml"), None).unwrap();
        let deps = package_dependencies(&metadata, &package.id, default_filter());
        let serde = resolve_dependency(&metadata.packages, &deps, "serde").unwrap();

        let serde_core = resolve_dependency_from_package(
            &metadata,
            &metadata.packages,
            serde.package,
            "serde_core",
        )
        .unwrap();

        assert_eq!(serde_core.package.name, "serde_core");
        assert_eq!(
            serde_core.contexts[0].label(),
            "transitive via serde (normal)"
        );
    }

    #[test]
    fn duplicate_dependency_contexts_for_same_package_are_merged() {
        let metadata = metadata();
        let package = select_package(&metadata, Path::new("Cargo.toml"), None).unwrap();
        let mut deps = package_dependencies(&metadata, &package.id, default_filter());
        let package_id = deps.entries["cargo_metadata"][0].package_id.clone();

        push_dependency_context(
            deps.entries.get_mut("cargo_metadata").unwrap(),
            package_id,
            DependencyContext {
                kind: DependencyKind::Development,
                target: None,
                via: None,
            },
        );

        let dep = resolve_dependency(&metadata.packages, &deps, "cargo_metadata").unwrap();
        assert_eq!(dep.package.name, "cargo_metadata");
        assert_eq!(
            dep.contexts
                .iter()
                .map(DependencyContext::label)
                .collect::<Vec<_>>(),
            vec!["normal", "dev"]
        );
    }

    #[test]
    fn same_crate_name_with_different_package_ids_is_ambiguous() {
        let metadata = metadata();
        let package = select_package(&metadata, Path::new("Cargo.toml"), None).unwrap();
        let mut deps = package_dependencies(&metadata, &package.id, default_filter());
        deps.insert(
            "cargo_metadata".into(),
            DependencyEntry {
                package_id: PackageId {
                    repr: "registry+https://example.invalid#index#cargo_metadata@99.0.0"
                        .to_string(),
                },
                contexts: vec![DependencyContext {
                    kind: DependencyKind::Development,
                    target: None,
                    via: None,
                }],
            },
        );

        let err = resolve_dependency(&metadata.packages, &deps, "cargo_metadata").unwrap_err();
        assert!(err.contains("matched multiple direct dependencies"));
        assert!(err.contains("normal"));
        assert!(err.contains("dev"));
    }
}

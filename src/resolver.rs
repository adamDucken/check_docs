use cargo_metadata::{DependencyKind, Metadata, Package, PackageId, Target};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct ResolvedDependency<'a> {
    pub(crate) package: &'a Package,
    pub(crate) target: &'a Target,
    pub(crate) context: DependencyContext,
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
            if matches
                .iter()
                .any(|entry| entry.package_id == dep.pkg && entry.context.kind == dep_kind.kind)
            {
                continue;
            }
            matches.push(DependencyEntry {
                package_id: dep.pkg.clone(),
                context: transitive_context(
                    dep_kind.kind,
                    dep_kind.target.as_ref().map(ToString::to_string),
                    &source_package.name,
                ),
            });
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
                context: entry.context.clone(),
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
    pub(crate) context: DependencyContext,
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

pub(crate) fn is_rust_library_crate(crate_name: &str) -> bool {
    matches!(crate_name, "std" | "core" | "alloc")
}

pub(crate) fn package_for_manifest<'a>(
    metadata: &'a Metadata,
    manifest_path: &Path,
) -> Result<&'a Package, String> {
    let manifest_path = manifest_path
        .canonicalize()
        .map_err(|err| format!("failed to canonicalize {}: {err}", manifest_path.display()))?;

    metadata
        .packages
        .iter()
        .find(|package| package.manifest_path.as_std_path() == manifest_path)
        .ok_or_else(|| {
            format!(
                "package for manifest {} not found in cargo metadata",
                manifest_path.display()
            )
        })
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
                deps.entries
                    .entry(dep.name.replace('-', "_"))
                    .or_default()
                    .push(DependencyEntry {
                        package_id: dep.pkg.clone(),
                        context: DependencyContext {
                            kind: dep_kind.kind,
                            target: dep_kind.target.as_ref().map(ToString::to_string),
                            via: None,
                        },
                    });
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
            .map(|entry| format!("{} [{}]", entry.package_id, entry.context.label()))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "crate '{crate_name}' matched multiple direct dependency contexts: {candidates}"
        ));
    }
    let entry = &entries[0];

    let package = package_by_id(packages, &entry.package_id)
        .ok_or_else(|| format!("direct dependency '{crate_name}' missing from cargo metadata"))?;
    let target = library_target(package)?;
    Ok(ResolvedDependency {
        package,
        target,
        context: entry.context.clone(),
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
        let package = package_for_manifest(&metadata, Path::new("Cargo.toml")).unwrap();
        assert_eq!(package.name, "check_docs");

        let deps = package_dependencies(&metadata, &package.id, default_filter());
        assert!(deps.contains_key("cargo_metadata"));

        let dep = resolve_dependency(&metadata.packages, &deps, "cargo_metadata").unwrap();
        assert_eq!(dep.package.name, "cargo_metadata");
        assert!(library_target(dep.package).is_ok());
    }

    #[test]
    fn package_spec_preserves_resolved_package_identity() {
        let metadata = metadata();
        let package = package_for_manifest(&metadata, Path::new("Cargo.toml")).unwrap();
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
        let package = package_for_manifest(&metadata, Path::new("Cargo.toml")).unwrap();
        let deps = package_dependencies(&metadata, &package.id, default_filter());

        let missing =
            resolve_dependency(&metadata.packages, &deps, "definitely_missing_crate").unwrap_err();
        assert!(missing.contains("not a direct dependency"));

        let mut broken_deps = deps.clone();
        broken_deps.insert(
            "missing_dep".into(),
            DependencyEntry {
                package_id: deps.entries["cargo_metadata"][0].package_id.clone(),
                context: deps.entries["cargo_metadata"][0].context.clone(),
            },
        );
        let missing_metadata = resolve_dependency(&[], &broken_deps, "missing_dep").unwrap_err();
        assert!(missing_metadata.contains("missing from cargo metadata"));

        let no_lib = library_target(package).unwrap_err();
        assert!(no_lib.contains("no doc-able library target"));

        let temp = TempDir::new().unwrap();
        let bad = package_for_manifest(&metadata, &temp.path().join("Cargo.toml")).unwrap_err();
        assert!(bad.contains("failed to canonicalize"));
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
        let package = package_for_manifest(&metadata, Path::new("Cargo.toml")).unwrap();

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
        assert_eq!(dep.context.kind, DependencyKind::Development);
    }

    #[test]
    fn resolves_reexport_target_from_direct_dependency_graph() {
        let metadata = metadata();
        let package = package_for_manifest(&metadata, Path::new("Cargo.toml")).unwrap();
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
        assert_eq!(serde_core.context.label(), "transitive via serde (normal)");
    }

    #[test]
    fn duplicate_dependency_contexts_are_rejected() {
        let metadata = metadata();
        let package = package_for_manifest(&metadata, Path::new("Cargo.toml")).unwrap();
        let mut deps = package_dependencies(&metadata, &package.id, default_filter());
        let duplicate = deps.entries["cargo_metadata"][0].clone();
        deps.insert("cargo_metadata".into(), duplicate);

        let err = resolve_dependency(&metadata.packages, &deps, "cargo_metadata").unwrap_err();
        assert!(err.contains("matched multiple direct dependency contexts"));
        assert!(err.contains("normal"));
    }
}

use cargo_metadata::{Metadata, Package, PackageId, Target};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct ResolvedDependency<'a> {
    pub(crate) package: &'a Package,
    pub(crate) target: &'a Target,
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
) -> HashMap<String, PackageId> {
    let mut deps = HashMap::new();
    if let Some(resolve) = metadata.resolve.as_ref()
        && let Some(node) = resolve.nodes.iter().find(|node| &node.id == package_id)
    {
        for dep in &node.deps {
            deps.insert(dep.name.replace('-', "_"), dep.pkg.clone());
        }
    }
    deps
}

pub(crate) fn resolve_dependency<'a>(
    packages: &'a [Package],
    dependencies: &HashMap<String, PackageId>,
    crate_name: &str,
) -> Result<ResolvedDependency<'a>, String> {
    let Some(package_id) = dependencies.get(crate_name) else {
        return Err(format!(
            "crate '{crate_name}' is not a direct dependency of the selected package"
        ));
    };

    let package = packages
        .iter()
        .find(|pkg| &pkg.id == package_id)
        .ok_or_else(|| format!("direct dependency '{crate_name}' missing from cargo metadata"))?;
    let target = library_target(package)?;
    Ok(ResolvedDependency { package, target })
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

        let deps = package_dependencies(&metadata, &package.id);
        assert!(deps.contains_key("cargo_metadata"));

        let dep = resolve_dependency(&metadata.packages, &deps, "cargo_metadata").unwrap();
        assert_eq!(dep.package.name, "cargo_metadata");
        assert!(library_target(dep.package).is_ok());
    }

    #[test]
    fn package_spec_preserves_resolved_package_identity() {
        let metadata = metadata();
        let package = package_for_manifest(&metadata, Path::new("Cargo.toml")).unwrap();
        let deps = package_dependencies(&metadata, &package.id);
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
        let deps = package_dependencies(&metadata, &package.id);

        let missing =
            resolve_dependency(&metadata.packages, &deps, "definitely_missing_crate").unwrap_err();
        assert!(missing.contains("not a direct dependency"));

        let mut broken_deps = deps.clone();
        broken_deps.insert("missing_dep".into(), deps["cargo_metadata"].clone());
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
        assert!(package_dependencies(&metadata, &fake).is_empty());
    }
}

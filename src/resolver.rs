use cargo_metadata::{Metadata, Package, PackageId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) fn is_rust_library_crate(crate_name: &str) -> bool {
    matches!(crate_name, "std" | "core" | "alloc")
}

pub(crate) fn rust_library_src(crate_name: &str) -> Result<PathBuf, String> {
    let output = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .map_err(|err| format!("failed to run `rustc --print sysroot`: {err}"))?;
    if !output.status.success() {
        return Err("`rustc --print sysroot` failed".to_string());
    }
    let sysroot = String::from_utf8(output.stdout)
        .map_err(|err| format!("rustc sysroot output was not utf8: {err}"))?;
    Ok(PathBuf::from(sysroot.trim())
        .join("lib/rustlib/src/rust/library")
        .join(crate_name)
        .join("src"))
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
        .ok_or_else(|| format!("package for manifest {} not found in cargo metadata", manifest_path.display()))
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

pub(crate) fn library_root(package: &Package) -> Result<PathBuf, String> {
    package
        .targets
        .iter()
        .find(|target| target.kind.iter().any(|kind| kind == "lib"))
        .map(|target| target.src_path.as_std_path().to_path_buf())
        .ok_or_else(|| format!("package {} has no library target", package.name))
}

pub(crate) fn resolve_package<'a>(
    packages: &'a [Package],
    dependencies: &HashMap<String, PackageId>,
    crate_name: &str,
) -> Result<&'a Package, String> {
    let mut matches: Vec<&Package> = dependencies
        .get(crate_name)
        .into_iter()
        .filter_map(|id| packages.iter().find(|pkg| &pkg.id == id))
        .collect();

    if matches.is_empty() {
        matches = packages
            .iter()
            .filter(|pkg| pkg.name.replace('-', "_") == crate_name)
            .collect();
    }

    match matches.as_slice() {
        [package] => Ok(*package),
        [] => Err(format!("crate '{crate_name}' not found in cargo metadata")),
        many => Err(format!(
            "crate '{crate_name}' is ambiguous: {}",
            many.iter()
                .map(|pkg| format!("{} {}", pkg.name, pkg.version))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

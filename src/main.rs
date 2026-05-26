mod cli;
mod imports;
mod resolver;
mod rustdoc_json;
mod symbols;

use cargo_metadata::{Metadata, MetadataCommand, Package, PackageId, Target};
use cli::parse_args;
use imports::ImportPath;
use resolver::{
    DependencyContext, DependencyFilter, is_rust_library_crate, package_dependencies,
    package_for_manifest, resolve_dependency, resolve_dependency_from_package,
};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use symbols::{SymbolDoc, SymbolReport};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RustdocCacheKey {
    package_id: PackageId,
    target_name: String,
    target_triple: Option<String>,
}

type RustdocCache = HashMap<RustdocCacheKey, (rustdoc_types::Crate, PathBuf)>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("check-docs: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let imports = imports::parse_use_lines(&args.use_line)?;

    for import in &imports {
        if is_rust_library_crate(&import.crate_name) {
            return Err(format!(
                "{} is part of the Rust standard library and is not supported; use the official Rust docs: https://doc.rust-lang.org/std/",
                import.crate_name
            ));
        }
    }

    let manifest_path = args.root.join("Cargo.toml");
    if !manifest_path.exists() {
        return Err(format!(
            "no Cargo.toml found at {}",
            manifest_path.display()
        ));
    }
    let selected_target = match args.target {
        Some(target) => target,
        None => host_target_triple()?,
    };

    let mut metadata_command = MetadataCommand::new();
    metadata_command.manifest_path(&manifest_path);
    metadata_command.other_options(vec![
        "--filter-platform".to_string(),
        selected_target.clone(),
    ]);
    let metadata = metadata_command
        .exec()
        .map_err(|err| format!("failed to read cargo metadata: {err}"))?;

    let root_package = package_for_manifest(&metadata, &manifest_path)?;
    let dependency_filter = DependencyFilter {
        include_dev: args.include_dev,
        include_build: args.include_build,
    };
    let root_dependencies = package_dependencies(&metadata, &root_package.id, dependency_filter);
    let mut rustdoc_cache = RustdocCache::new();
    for (index, import) in imports.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let dep = resolve_dependency(&metadata.packages, &root_dependencies, &import.crate_name)?;
        let (krate, json_path) = load_docs_cached(
            &mut rustdoc_cache,
            &manifest_path,
            &metadata,
            dep.package,
            dep.target,
            Some(&selected_target),
        )?;
        let (found, report_crate, report_version, report_json_path, context, target_triple) =
            match symbols::find_symbol_report(&krate, import) {
                Ok(found) => (
                    found,
                    dep.package.name.clone(),
                    dep.package.version.to_string(),
                    json_path,
                    dep.context,
                    krate.target.triple.clone(),
                ),
                Err(err) => {
                    let imported_reexport =
                        symbols::imported_reexport(&krate, import).ok().flatten();
                    let Some(external) =
                        symbols::external_reexport(&krate, import).map_err(|external_err| {
                            format!("{err}; failed to inspect external re-export: {external_err}")
                        })?
                    else {
                        return Err(not_found_message(
                            import,
                            &dep.package.name,
                            Some(&dep.package.version.to_string()),
                            &json_path,
                            Some(&err),
                        ));
                    };
                    let external_dep =
                        match resolve_dependency(&metadata.packages, &root_dependencies, &external.crate_name) {
                            Ok(dep) => dep,
                            Err(dep_err) if dep_err.contains("not a direct dependency") => {
                                resolve_dependency_from_package(
                                    &metadata,
                                    &metadata.packages,
                                    dep.package,
                                    &external.crate_name,
                                )
                                .map_err(|graph_err| {
                                    format!(
                                        "item '{}' is re-exported from external crate '{}' but exact docs could not resolve that crate through direct dependency '{}': {graph_err}",
                                        import.item, external.crate_name, dep.package.name
                                    )
                                })?
                            }
                            Err(dep_err) => {
                                return Err(format!(
                                    "item '{}' is re-exported from external crate '{}' but exact docs could not resolve that crate: {dep_err}",
                                    import.item, external.crate_name
                                ));
                            }
                        };
                    let (external_krate, external_json_path) = load_docs_cached(
                        &mut rustdoc_cache,
                        &manifest_path,
                        &metadata,
                        external_dep.package,
                        external_dep.target,
                        Some(&selected_target),
                    )?;
                    let found = if let Some(external_import) = external.import_path() {
                        find_external_symbol_with_fallback(
                            &external_krate,
                            &external_import,
                            &external_dep.package.name,
                            &external_dep.package.version.to_string(),
                            &external_json_path,
                        )?
                    } else {
                        SymbolReport {
                            imported: symbols::format_crate_root(&external_krate)?,
                            resolved: None,
                        }
                    };
                    let found = if let Some(imported) = imported_reexport {
                        let resolved = found.resolved.unwrap_or(found.imported);
                        SymbolReport {
                            imported,
                            resolved: Some(resolved),
                        }
                    } else {
                        found
                    };
                    (
                        found,
                        external_dep.package.name.clone(),
                        external_dep.package.version.to_string(),
                        external_json_path,
                        external_dep.context,
                        external_krate.target.triple.clone(),
                    )
                }
            };

        print_report(
            &report_crate,
            Some(&report_version),
            &report_json_path,
            &format_use(import),
            &context,
            &target_triple,
            &found,
        );
    }

    Ok(())
}

fn host_target_triple() -> Result<String, String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|err| format!("failed to run rustc -vV to detect host target: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to detect host target with rustc -vV: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_string)
        .ok_or_else(|| "failed to parse host target from rustc -vV output".to_string())
}

fn load_docs_cached(
    cache: &mut RustdocCache,
    manifest_path: &Path,
    metadata: &Metadata,
    package: &Package,
    target: &Target,
    target_triple: Option<&str>,
) -> Result<(rustdoc_types::Crate, PathBuf), String> {
    let key = rustdoc_cache_key(package, target, target_triple);
    if let Some(cached) = cache.get(&key) {
        return Ok(cached.clone());
    }

    let loaded = rustdoc_json::load_or_generate(
        manifest_path.to_path_buf(),
        metadata,
        package,
        target,
        target_triple,
    )?;
    cache.insert(key, loaded.clone());
    Ok(loaded)
}

fn rustdoc_cache_key(
    package: &Package,
    target: &Target,
    target_triple: Option<&str>,
) -> RustdocCacheKey {
    RustdocCacheKey {
        package_id: package.id.clone(),
        target_name: target.name.clone(),
        target_triple: target_triple.map(str::to_string),
    }
}

fn find_external_symbol_with_fallback(
    krate: &rustdoc_types::Crate,
    import: &ImportPath,
    crate_name: &str,
    version: &str,
    json_path: &Path,
) -> Result<SymbolReport, String> {
    match symbols::find_symbol_report(krate, import) {
        Ok(found) => Ok(found),
        Err(first_err) if !import.segments.is_empty() => {
            let root_import = ImportPath {
                crate_name: import.crate_name.clone(),
                segments: Vec::new(),
                item: import.item.clone(),
            };
            symbols::find_symbol_report(krate, &root_import).map_err(|second_err| {
                not_found_message(
                    import,
                    crate_name,
                    Some(version),
                    json_path,
                    Some(&format!(
                        "{first_err}; also failed root re-export fallback '{}': {second_err}",
                        root_import.item
                    )),
                )
            })
        }
        Err(err) => Err(not_found_message(
            import,
            crate_name,
            Some(version),
            json_path,
            Some(&err),
        )),
    }
}

fn format_use(import: &ImportPath) -> String {
    let mut parts = vec![import.crate_name.clone()];
    parts.extend(import.segments.clone());
    parts.push(import.item.clone());
    format!("use {};", parts.join("::"))
}

fn print_report(
    crate_name: &str,
    version: Option<&str>,
    source: &Path,
    use_line: &str,
    context: &DependencyContext,
    target_triple: &str,
    report: &SymbolReport,
) {
    if let Some(version) = version {
        println!("crate: {crate_name} {version}");
    } else {
        println!("crate: {crate_name}");
    }
    println!("dependency: {}", context.label());
    println!("target: {target_triple}");
    println!("source: {}", source.display());
    println!("import: {}", use_line.trim());
    print_doc("item", &report.imported);
    if let Some(resolved) = &report.resolved {
        print_doc("resolved item", resolved);
    }
}

fn print_doc(label: &str, found: &SymbolDoc) {
    println!("{label}: {} {}", found.kind, found.name);
    if found.path.as_os_str().is_empty() {
        println!("location: (unknown)");
    } else {
        println!("location: {}:{}", found.path.display(), found.line);
    }
    println!("definition: {}", found.definition);
    if !found.derives.is_empty() {
        println!("derives: {}", found.derives.join(", "));
    }
    if !found.details.is_empty() {
        println!("details:");
        for line in &found.details {
            println!("  {line}");
        }
    }
    if !found.methods.is_empty() {
        println!("methods:");
        for line in &found.methods {
            println!("  {line}");
        }
    }
    if !found.impls.is_empty() {
        println!("impls:");
        for line in &found.impls {
            println!("  {line}");
        }
    }
    if found.docs.is_empty() {
        println!("docs: (none)");
    } else {
        println!("docs:");
        for line in &found.docs {
            println!("  {line}");
        }
    }
}

fn not_found_message(
    import: &ImportPath,
    crate_name: &str,
    version: Option<&str>,
    source: &Path,
    context: Option<&str>,
) -> String {
    let crate_label = if let Some(version) = version {
        format!("{crate_name} {version}")
    } else {
        crate_name.to_string()
    };
    let Some(context) = context else {
        let mut message = format!(
            "item '{}' not found in {} ({}): no matching public rustdoc item",
            import.item,
            crate_label,
            source.display(),
        );
        if looks_like_module_name(&import.item) {
            message.push_str(&format!(
                "; '{}' appears to be a module — query a concrete item inside that module",
                import.item
            ));
        }
        return message;
    };

    if context.contains("external re-export") {
        return format!(
            "item '{}' is a public re-export with unsupported rustdoc external id in {} ({}): {}; query/add the external crate directly if available",
            import.item,
            crate_label,
            source.display(),
            context
        );
    }

    let mut message = format!(
        "item '{}' not found in {} ({}): {}",
        import.item,
        crate_label,
        source.display(),
        context
    );
    if looks_like_module_name(&import.item) {
        message.push_str(&format!(
            "; '{}' appears to be a module — query a concrete item inside that module",
            import.item
        ));
    }
    message
}

fn looks_like_module_name(name: &str) -> bool {
    name.chars()
        .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::MetadataCommand;
    use rustdoc_types::{Crate, Id, Item, ItemEnum, Module, Struct, StructKind, Visibility};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn doc(name: &str, docs: Vec<String>) -> SymbolDoc {
        SymbolDoc {
            path: PathBuf::from("/tmp/src/lib.rs"),
            line: 7,
            kind: "struct",
            name: name.into(),
            definition: format!("pub struct {name};"),
            details: vec!["field: usize".into()],
            docs,
            derives: vec!["Debug".into()],
            methods: vec!["pub fn new() -> Self".into()],
            impls: vec!["impl Clone".into()],
        }
    }

    fn report(doc: SymbolDoc) -> SymbolReport {
        SymbolReport {
            imported: doc,
            resolved: None,
        }
    }

    fn dependency_context() -> DependencyContext {
        DependencyContext {
            kind: cargo_metadata::DependencyKind::Normal,
            target: None,
            via: None,
        }
    }

    fn rustdoc_item(id: u32, name: &str, inner: ItemEnum) -> Item {
        Item {
            id: Id(id),
            crate_id: 0,
            name: Some(name.into()),
            span: None,
            visibility: Visibility::Public,
            docs: None,
            links: HashMap::new(),
            attrs: Vec::new(),
            deprecation: None,
            inner,
        }
    }

    #[test]
    fn rustdoc_cache_key_uses_package_identity_and_target_name() {
        let metadata = MetadataCommand::new()
            .manifest_path("Cargo.toml")
            .exec()
            .unwrap();
        let package = package_for_manifest(&metadata, Path::new("Cargo.toml")).unwrap();
        let deps = package_dependencies(
            &metadata,
            &package.id,
            DependencyFilter {
                include_dev: false,
                include_build: false,
            },
        );
        let dep = resolve_dependency(&metadata.packages, &deps, "cargo_metadata").unwrap();
        let key = rustdoc_cache_key(dep.package, dep.target, None);

        assert_eq!(&key.package_id, &dep.package.id);
        assert_eq!(&key.target_name, &dep.target.name);
    }

    #[test]
    fn not_found_message_handles_modules_and_plain_items() {
        let module = ImportPath {
            crate_name: "tokio".into(),
            segments: vec!["sync".into()],
            item: "mpsc".into(),
        };
        let message = not_found_message(&module, "tokio", Some("1.0.0"), Path::new("/src"), None);
        assert!(message.contains("appears to be a module"));
        assert!(message.contains("query a concrete item inside that module"));
        assert!(!message.contains("Sender"));

        let item = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "Thing".into(),
        };
        let message = not_found_message(&item, "x", None, Path::new("/src"), None);
        assert!(!message.contains("appears to be a module"));
        assert!(looks_like_module_name("module_2"));
        assert!(!looks_like_module_name("TypeName"));
    }

    #[test]
    fn print_report_covers_output_branches() {
        let src = Path::new("/tmp/src");
        print_report(
            "x",
            Some("1.2.3"),
            src,
            "use x::Thing;",
            &dependency_context(),
            "x86_64-unknown-linux-gnu",
            &report(doc("Thing", vec!["docs".into()])),
        );
        print_report(
            "x",
            None,
            src,
            "use x::Thing;",
            &dependency_context(),
            "x86_64-unknown-linux-gnu",
            &report(doc("Thing", Vec::new())),
        );
    }

    #[test]
    fn external_lookup_falls_back_to_root_reexport_name() {
        let root = rustdoc_item(
            1,
            "digest",
            ItemEnum::Module(Module {
                is_crate: true,
                items: vec![Id(2)],
                is_stripped: false,
            }),
        );
        let mac = rustdoc_item(
            2,
            "Mac",
            ItemEnum::Struct(Struct {
                kind: StructKind::Unit,
                generics: rustdoc_types::Generics {
                    params: Vec::new(),
                    where_predicates: Vec::new(),
                },
                impls: Vec::new(),
            }),
        );
        let krate = Crate {
            root: Id(1),
            crate_version: Some("1.0.0".into()),
            includes_private: false,
            index: HashMap::from([(Id(1), root), (Id(2), mac)]),
            paths: HashMap::new(),
            external_crates: HashMap::new(),
            target: rustdoc_types::Target {
                triple: "x86_64-unknown-linux-gnu".into(),
                target_features: Vec::new(),
            },
            format_version: rustdoc_types::FORMAT_VERSION,
        };
        let import = ImportPath {
            crate_name: "digest".into(),
            segments: vec!["mac".into()],
            item: "Mac".into(),
        };

        let found =
            find_external_symbol_with_fallback(&krate, &import, "digest", "1.0.0", Path::new("/x"))
                .unwrap();
        assert_eq!(found.imported.name, "Mac");
    }
}

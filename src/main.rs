mod cli;
mod imports;
mod resolver;
mod rustdoc_json;
mod symbols;

use cargo_metadata::{Metadata, MetadataCommand, Package, PackageId, Target};
use cli::{ParsedCommand, parse_command};
use imports::ImportPath;
use resolver::{
    DependencyContext, DependencyFilter, ResolveError, is_rust_library_crate, package_dependencies,
    resolve_dependency, resolve_dependency_from_package, select_package,
};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use symbols::{SymbolDoc, SymbolError, SymbolReport};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RustdocCacheKey {
    package_id: PackageId,
    target_name: String,
    target_triple: Option<String>,
}

type LoadedDocs = (Arc<rustdoc_types::Crate>, PathBuf);
type RustdocCache = HashMap<RustdocCacheKey, LoadedDocs>;

#[derive(Debug)]
struct OutputReport {
    crate_name: String,
    version: Option<String>,
    dependency: String,
    target_triple: String,
    source: PathBuf,
    import_line: String,
    symbols: SymbolReport,
}

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
    let args = match parse_command()? {
        ParsedCommand::Run(args) => args,
        ParsedCommand::Help => {
            println!("{}", cli::usage());
            return Ok(());
        }
    };
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

    let root_package = select_package(&metadata, &manifest_path, args.package.as_deref())?;
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
        let dep = resolve_dependency(&metadata.packages, &root_dependencies, &import.crate_name)
            .map_err(|err| err.to_string())?;
        let (krate, json_path) = load_docs_cached(
            &mut rustdoc_cache,
            &manifest_path,
            &metadata,
            dep.package,
            dep.target,
            Some(&selected_target),
        )?;
        let (found, report_crate, report_version, report_json_path, contexts, target_triple) =
            match symbols::find_symbol_report(&krate, import) {
                Ok(found) => (
                    found,
                    dep.package.name.clone(),
                    dep.package.version.to_string(),
                    json_path,
                    dep.contexts,
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
                            Err(ResolveError::NotDirectDependency(_)) => {
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
                        find_external_symbol(
                            &external_krate,
                            &external_import,
                            &external_dep.package.name,
                            &external_dep.package.version.to_string(),
                            &external_json_path,
                        )?
                    } else {
                        SymbolReport {
                            imported: symbols::format_crate_root(&external_krate)
                                .map_err(|err| err.to_string())?,
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
                        external_dep.contexts,
                        external_krate.target.triple.clone(),
                    )
                }
            };

        let output = OutputReport {
            crate_name: report_crate,
            version: Some(report_version),
            dependency: format_dependency_contexts(&contexts),
            target_triple,
            source: report_json_path,
            import_line: format_use(import),
            symbols: found,
        };
        print_report(&output);
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
) -> Result<LoadedDocs, String> {
    let key = rustdoc_cache_key(package, target, target_triple);
    if let Some(cached) = cache.get(&key) {
        return Ok(cached.clone());
    }

    let (krate, json_path) = rustdoc_json::load_or_generate(
        manifest_path.to_path_buf(),
        metadata,
        package,
        target,
        target_triple,
    )?;
    let loaded = (Arc::new(krate), json_path);
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

fn find_external_symbol(
    krate: &rustdoc_types::Crate,
    import: &ImportPath,
    crate_name: &str,
    version: &str,
    json_path: &Path,
) -> Result<SymbolReport, String> {
    symbols::find_symbol_report(krate, import)
        .map_err(|err| not_found_message(import, crate_name, Some(version), json_path, Some(&err)))
}

fn format_use(import: &ImportPath) -> String {
    let mut parts = vec![import.crate_name.clone()];
    parts.extend(import.segments.clone());
    parts.push(import.item.clone());
    format!("use {};", parts.join("::"))
}

fn print_report(report: &OutputReport) {
    print!("{}", render_report(report));
}

fn render_report(report: &OutputReport) -> String {
    let mut output = String::new();
    if let Some(version) = &report.version {
        output.push_str(&format!("crate: {} {version}\n", report.crate_name));
    } else {
        output.push_str(&format!("crate: {}\n", report.crate_name));
    }
    output.push_str(&format!("dependency: {}\n", report.dependency));
    output.push_str(&format!("target: {}\n", report.target_triple));
    output.push_str(&format!("source: {}\n", report.source.display()));
    output.push_str(&format!("import: {}\n", report.import_line.trim()));
    push_doc(&mut output, "item", &report.symbols.imported);
    if let Some(resolved) = &report.symbols.resolved {
        push_doc(&mut output, "resolved item", resolved);
    }
    output
}

fn format_dependency_contexts(contexts: &[DependencyContext]) -> String {
    contexts
        .iter()
        .map(DependencyContext::label)
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_doc(output: &mut String, label: &str, found: &SymbolDoc) {
    output.push_str(&format!("{label}: {} {}\n", found.kind, found.name));
    if found.path.as_os_str().is_empty() {
        output.push_str("location: (unknown)\n");
    } else {
        output.push_str(&format!(
            "location: {}:{}\n",
            found.path.display(),
            found.line
        ));
    }
    output.push_str(&format!("definition: {}\n", found.definition));
    if let Some(deprecation) = &found.deprecation {
        output.push_str("deprecation:\n");
        if let Some(since) = &deprecation.since {
            output.push_str(&format!("  since: {since}\n"));
        }
        if let Some(note) = &deprecation.note {
            output.push_str(&format!("  note: {note}\n"));
        }
        if deprecation.since.is_none() && deprecation.note.is_none() {
            output.push_str("  (no details)\n");
        }
    }
    if !found.attributes.is_empty() {
        output.push_str("attributes:\n");
        for attribute in &found.attributes {
            output.push_str(&format!("  {}\n", attribute.render()));
        }
    }
    if !found.derives.is_empty() {
        output.push_str(&format!("derives: {}\n", found.derives.join(", ")));
    }
    if !found.details.is_empty() {
        output.push_str("details:\n");
        for line in &found.details {
            output.push_str(&format!("  {line}\n"));
        }
    }
    if !found.methods.is_empty() {
        output.push_str("methods:\n");
        for line in &found.methods {
            output.push_str(&format!("  {line}\n"));
        }
    }
    if !found.impls.is_empty() {
        output.push_str("impls:\n");
        for line in &found.impls {
            output.push_str(&format!("  {line}\n"));
        }
    }
    if found.docs.is_empty() {
        output.push_str("docs: (none)\n");
    } else {
        output.push_str("docs:\n");
        for line in &found.docs {
            output.push_str(&format!("  {line}\n"));
        }
    }
}

fn not_found_message(
    import: &ImportPath,
    crate_name: &str,
    version: Option<&str>,
    source: &Path,
    context: Option<&SymbolError>,
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

    if matches!(context, SymbolError::ExternalReexport(_)) {
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
            deprecation: None,
            attributes: Vec::new(),
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
        let package = select_package(&metadata, Path::new("Cargo.toml"), None).unwrap();
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
    fn output_report_preserves_structured_fields_and_renders_text() {
        let mut symbol = doc("Thing", vec!["docs".into()]);
        symbol.deprecation = Some(symbols::DeprecationDoc {
            since: Some("1.2.3".into()),
            note: Some("use NewThing".into()),
        });
        symbol.attributes = vec![
            symbols::ReportedAttribute::NonExhaustive,
            symbols::ReportedAttribute::MustUse {
                reason: Some("inspect the value".into()),
            },
        ];
        let output = OutputReport {
            crate_name: "x".into(),
            version: Some("1.2.3".into()),
            dependency: dependency_context().label(),
            target_triple: "x86_64-unknown-linux-gnu".into(),
            source: PathBuf::from("/tmp/src"),
            import_line: "use x::Thing;".into(),
            symbols: report(symbol),
        };

        assert_eq!(output.crate_name, "x");
        assert_eq!(output.version.as_deref(), Some("1.2.3"));
        assert_eq!(output.dependency, "normal");
        assert_eq!(output.target_triple, "x86_64-unknown-linux-gnu");
        assert_eq!(output.import_line, "use x::Thing;");

        let rendered = render_report(&output);
        assert!(rendered.contains("crate: x 1.2.3\n"));
        assert!(rendered.contains("dependency: normal\n"));
        assert!(rendered.contains("item: struct Thing\n"));
        assert!(rendered.contains(
            "deprecation:\n  since: 1.2.3\n  note: use NewThing\nattributes:\n  #[non_exhaustive]\n  #[must_use = \"inspect the value\"]\n"
        ));
        assert!(rendered.contains("docs:\n  docs\n"));
    }

    #[test]
    fn output_report_renders_resolved_items_and_empty_docs() {
        let output = OutputReport {
            crate_name: "x".into(),
            version: None,
            dependency: dependency_context().label(),
            target_triple: "x86_64-unknown-linux-gnu".into(),
            source: PathBuf::from("/tmp/src"),
            import_line: "use x::Thing;".into(),
            symbols: SymbolReport {
                imported: doc("Thing", Vec::new()),
                resolved: Some(doc("ResolvedThing", Vec::new())),
            },
        };

        let rendered = render_report(&output);
        assert!(rendered.starts_with("crate: x\n"));
        assert!(rendered.contains("docs: (none)\n"));
        assert!(rendered.contains("resolved item: struct ResolvedThing\n"));
    }

    #[test]
    fn output_preserves_markdown_indentation_and_blank_lines() {
        let output = OutputReport {
            crate_name: "x".into(),
            version: None,
            dependency: dependency_context().label(),
            target_triple: "x86_64-unknown-linux-gnu".into(),
            source: PathBuf::from("/tmp/src"),
            import_line: "use x::Thing;".into(),
            symbols: report(doc(
                "Thing",
                vec![
                    "First paragraph.".into(),
                    "".into(),
                    "- parent".into(),
                    "  - child".into(),
                ],
            )),
        };

        assert!(
            render_report(&output)
                .contains("docs:\n  First paragraph.\n  \n  - parent\n    - child\n")
        );
    }

    #[test]
    fn typed_not_found_classification_does_not_promote_glob_diagnostics() {
        let import = ImportPath {
            crate_name: "facade".into(),
            segments: vec![],
            item: "Missing".into(),
        };
        let error = SymbolError::NotFound(
            "glob branches failed: external re-export target unavailable".into(),
        );
        let message = not_found_message(
            &import,
            "facade",
            Some("1.0.0"),
            Path::new("/tmp/facade.json"),
            Some(&error),
        );
        assert!(message.starts_with("item 'Missing' not found"));
        assert!(!message.contains("is a public re-export"));
    }

    #[test]
    fn cache_entries_share_the_rustdoc_graph() {
        let docs = Arc::new(root_mac_crate());
        let key = RustdocCacheKey {
            package_id: PackageId {
                repr: "path+file:///fixture#1.0.0".into(),
            },
            target_name: "fixture".into(),
            target_triple: None,
        };
        let mut cache = RustdocCache::new();
        cache.insert(
            key.clone(),
            (Arc::clone(&docs), PathBuf::from("fixture.json")),
        );
        let (cached, _) = cache.get(&key).unwrap().clone();

        assert!(Arc::ptr_eq(&docs, &cached));
        assert_eq!(Arc::strong_count(&docs), 3);
    }

    #[test]
    fn dependency_context_output_supports_multiple_contexts() {
        let contexts = [
            DependencyContext {
                kind: cargo_metadata::DependencyKind::Normal,
                target: None,
                via: None,
            },
            DependencyContext {
                kind: cargo_metadata::DependencyKind::Development,
                target: Some("cfg(test)".into()),
                via: None,
            },
        ];

        assert_eq!(
            format_dependency_contexts(&contexts),
            "normal, dev (cfg(test))"
        );
    }

    fn root_mac_crate() -> Crate {
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
        Crate {
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
        }
    }

    #[test]
    fn external_lookup_does_not_fall_back_to_root_reexport_name() {
        let krate = root_mac_crate();
        let import = ImportPath {
            crate_name: "digest".into(),
            segments: vec!["mac".into()],
            item: "Mac".into(),
        };

        let err =
            find_external_symbol(&krate, &import, "digest", "1.0.0", Path::new("/x")).unwrap_err();

        assert!(err.contains("'mac' not found under digest"));
        assert!(!err.contains("root re-export fallback"));
    }

    #[test]
    fn external_lookup_reports_exact_root_item() {
        let krate = root_mac_crate();
        let import = ImportPath {
            crate_name: "digest".into(),
            segments: Vec::new(),
            item: "Mac".into(),
        };

        let found =
            find_external_symbol(&krate, &import, "digest", "1.0.0", Path::new("/x")).unwrap();

        assert_eq!(found.imported.name, "Mac");
    }
}

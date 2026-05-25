mod cli;
mod imports;
mod resolver;
mod rustdoc_json;
mod symbols;

use cargo_metadata::MetadataCommand;
use cli::parse_args;
use imports::ImportPath;
use resolver::{
    is_rust_library_crate, package_dependencies, package_for_manifest, resolve_dependency,
};
use std::path::Path;
use std::process::ExitCode;
use symbols::SymbolDoc;

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
    let metadata = MetadataCommand::new()
        .manifest_path(&manifest_path)
        .exec()
        .map_err(|err| format!("failed to read cargo metadata: {err}"))?;

    let root_package = package_for_manifest(&metadata, &manifest_path)?;
    let root_dependencies = package_dependencies(&metadata, &root_package.id);
    for (index, import) in imports.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let dep = resolve_dependency(&metadata.packages, &root_dependencies, &import.crate_name)?;
        let (krate, json_path) = rustdoc_json::load_or_generate(
            manifest_path.clone(),
            &metadata,
            dep.package,
            dep.target,
        )?;
        let (found, report_crate, report_version, report_json_path) = match symbols::find_symbol(
            &krate, import,
        ) {
            Ok(found) => (
                found,
                dep.package.name.clone(),
                dep.package.version.to_string(),
                json_path,
            ),
            Err(err) => {
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
                let external_dep = resolve_dependency(
                    &metadata.packages,
                    &root_dependencies,
                    &external.crate_name,
                )
                .map_err(|dep_err| {
                    format!(
                        "item '{}' is re-exported from external crate '{}' but exact docs require that crate to be a direct dependency of the selected package; add/query '{}' directly: {dep_err}",
                        import.item, external.crate_name, external.crate_name
                    )
                })?;
                let (external_krate, external_json_path) = rustdoc_json::load_or_generate(
                    manifest_path.clone(),
                    &metadata,
                    external_dep.package,
                    external_dep.target,
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
                    symbols::format_crate_root(&external_krate)?
                };
                (
                    found,
                    external_dep.package.name.clone(),
                    external_dep.package.version.to_string(),
                    external_json_path,
                )
            }
        };

        print_report(
            &report_crate,
            Some(&report_version),
            &report_json_path,
            &format_use(import),
            import,
            &found,
        );
    }

    Ok(())
}

fn find_external_symbol_with_fallback(
    krate: &rustdoc_types::Crate,
    import: &ImportPath,
    crate_name: &str,
    version: &str,
    json_path: &Path,
) -> Result<SymbolDoc, String> {
    match symbols::find_symbol(krate, import) {
        Ok(found) => Ok(found),
        Err(first_err) if !import.segments.is_empty() => {
            let root_import = ImportPath {
                crate_name: import.crate_name.clone(),
                segments: Vec::new(),
                item: import.item.clone(),
            };
            symbols::find_symbol(krate, &root_import).map_err(|second_err| {
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
    _import: &ImportPath,
    found: &SymbolDoc,
) {
    if let Some(version) = version {
        println!("crate: {crate_name} {version}");
    } else {
        println!("crate: {crate_name}");
    }
    println!("source: {}", source.display());
    println!("import: {}", use_line.trim());
    println!("item: {} {}", found.kind, found.name);
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
        let import = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "Thing".into(),
        };
        let src = Path::new("/tmp/src");
        print_report(
            "x",
            Some("1.2.3"),
            src,
            "use x::Thing;",
            &import,
            &doc("Thing", vec!["docs".into()]),
        );
        print_report(
            "x",
            None,
            src,
            "use x::Thing;",
            &import,
            &doc("Thing", Vec::new()),
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
        assert_eq!(found.name, "Mac");
    }
}

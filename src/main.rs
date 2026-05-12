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
        let found = symbols::find_symbol(&krate, import).map_err(|err| {
            not_found_message(
                import,
                &dep.package.name,
                Some(&dep.package.version.to_string()),
                &json_path,
                Some(&err),
            )
        })?;

        print_report(
            &dep.package.name,
            Some(&dep.package.version.to_string()),
            &json_path,
            &format_use(import),
            import,
            &found,
        );
    }

    Ok(())
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
}

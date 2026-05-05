mod cli;
mod imports;
mod resolver;
mod symbols;

use cargo_metadata::MetadataCommand;
use cli::parse_args;
use imports::{ImportPath, parse_use_line};
use resolver::{is_rust_library_crate, library_root, package_dependencies, package_for_manifest, resolve_package, rust_library_src};
use std::path::Path;
use std::process::ExitCode;
use symbols::{SymbolDoc, add_reexported_matches, find_symbols, find_symbols_lossy, rank_matches};

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
    let import = parse_use_line(&args.use_line)?;

    if is_rust_library_crate(&import.crate_name) {
        let src = rust_library_src(&import.crate_name)?;
        if !src.exists() {
            return Err(format!(
                "rust source not found for {} at {}; install with `rustup component add rust-src`",
                import.crate_name,
                src.display()
            ));
        }

        let mut matches = find_symbols_lossy(&src, &import.item)?;
        if matches.is_empty() {
            return Err(not_found_message(&import, &import.crate_name, None, &src));
        }
        rank_matches(&mut matches, &import);
        print_report(
            &import.crate_name,
            None,
            &src,
            &args.use_line,
            &import,
            &matches,
            &matches[0],
        );
        return Ok(());
    }

    let metadata = MetadataCommand::new()
        .manifest_path(args.root.join("Cargo.toml"))
        .exec()
        .map_err(|err| format!("failed to read cargo metadata: {err}"))?;

    let root_package = package_for_manifest(&metadata, &args.root.join("Cargo.toml"))?;
    let root_dependencies = package_dependencies(&metadata, &root_package.id);
    let package = resolve_package(&metadata.packages, &root_dependencies, &import.crate_name)?;
    let package_dependencies = package_dependencies(&metadata, &package.id);
    let root_file = library_root(package)?;
    let src = root_file
        .parent()
        .ok_or_else(|| format!("library target {} has no parent", root_file.display()))?
        .to_path_buf();

    if !root_file.exists() {
        return Err(format!(
            "source not found for {} at {}",
            package.name,
            root_file.display()
        ));
    }

    let mut matches = find_symbols(&root_file, &import.item, &import.segments)?;
    add_reexported_matches(
        &root_file,
        &import,
        &metadata.packages,
        &package_dependencies,
        &mut matches,
    )?;
    if matches.is_empty() {
        return Err(not_found_message(
            &import,
            &package.name,
            Some(&package.version.to_string()),
            &src,
        ));
    }
    rank_matches(&mut matches, &import);

    print_report(
        &package.name,
        Some(&package.version.to_string()),
        &src,
        &args.use_line,
        &import,
        &matches,
        &matches[0],
    );

    Ok(())
}

fn print_report(
    crate_name: &str,
    version: Option<&str>,
    src: &Path,
    use_line: &str,
    import: &ImportPath,
    matches: &[SymbolDoc],
    found: &SymbolDoc,
) {
    if let Some(version) = version {
        println!("crate: {crate_name} {version}");
    } else {
        println!("crate: {crate_name}");
    }
    println!("source: {}", src.display());
    println!("import: {}", use_line.trim());
    println!("item: {} {}", found.kind, found.name);
    println!(
        "location: {}:{}",
        found
            .path
            .strip_prefix(src)
            .unwrap_or(&found.path)
            .display(),
        found.line
    );
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
    if found.docs.is_empty() {
        println!("docs: (none)");
    } else {
        println!("docs:");
        for line in &found.docs {
            println!("  {line}");
        }
    }
    if matches.len() > 1 {
        println!(
            "note: {} items named '{}' found; best match shown",
            matches.len(),
            import.item
        );
    }
}

fn not_found_message(
    import: &ImportPath,
    crate_name: &str,
    version: Option<&str>,
    src: &Path,
) -> String {
    let crate_label = if let Some(version) = version {
        format!("{crate_name} {version}")
    } else {
        crate_name.to_string()
    };
    let mut message = format!(
        "item '{}' not found in {} ({})",
        import.item,
        crate_label,
        src.display()
    );
    if looks_like_module_name(&import.item) {
        message.push_str(&format!(
            "; '{}' appears to be a module — query a concrete item inside it, e.g. `use {}::Sender;`",
            import.item,
            import.full_path()
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

    fn doc(name: &str, docs: Vec<String>) -> SymbolDoc {
        SymbolDoc {
            path: Path::new("/tmp/src/lib.rs").to_path_buf(),
            line: 7,
            kind: "struct",
            name: name.into(),
            definition: format!("pub struct {name};"),
            details: vec!["field: usize".into()],
            docs,
            derives: vec!["Debug".into()],
            public: true,
            reexported: false,
        }
    }

    #[test]
    fn not_found_message_handles_modules_and_plain_items() {
        let module = ImportPath {
            crate_name: "tokio".into(),
            segments: vec!["sync".into()],
            item: "mpsc".into(),
        };
        let message = not_found_message(&module, "tokio", Some("1.0.0"), Path::new("/src"));
        assert!(message.contains("appears to be a module"));
        assert!(message.contains("tokio::sync::mpsc::Sender"));

        let item = ImportPath { crate_name: "x".into(), segments: vec![], item: "Thing".into() };
        let message = not_found_message(&item, "x", None, Path::new("/src"));
        assert!(!message.contains("appears to be a module"));
        assert!(looks_like_module_name("module_2"));
        assert!(!looks_like_module_name("TypeName"));
    }

    #[test]
    fn print_report_covers_output_branches() {
        let import = ImportPath { crate_name: "x".into(), segments: vec![], item: "Thing".into() };
        let src = Path::new("/tmp/src");
        let first = doc("Thing", vec!["docs".into()]);
        let second = doc("Thing", Vec::new());
        print_report("x", Some("1.2.3"), src, "use x::Thing;", &import, &[first, second], &doc("Thing", vec!["docs".into()]));
        print_report("x", None, src, "use x::Thing;", &import, &[doc("Thing", Vec::new())], &doc("Thing", Vec::new()));
    }
}

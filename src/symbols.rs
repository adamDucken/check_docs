use crate::imports::ImportPath;
use crate::resolver::resolve_package;
use cargo_metadata::{Package, PackageId};
use quote::ToTokens;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use walkdir::WalkDir;

#[derive(Debug)]
pub(crate) struct SymbolDoc {
    pub(crate) path: PathBuf,
    pub(crate) line: usize,
    pub(crate) kind: &'static str,
    pub(crate) name: String,
    pub(crate) definition: String,
    pub(crate) details: Vec<String>,
    pub(crate) docs: Vec<String>,
    pub(crate) derives: Vec<String>,
    pub(crate) public: bool,
    pub(crate) reexported: bool,
}

pub(crate) fn find_symbols(
    root_file: &Path,
    target: &str,
    module_segments: &[String],
) -> Result<Vec<SymbolDoc>, String> {
    let module = resolve_module(root_file, module_segments, true)?;
    let mut out = Vec::new();
    collect_items_direct(&module.path, &module.items, target, &mut out);
    out.retain(|doc| doc.public);
    Ok(out)
}

pub(crate) fn find_symbols_lossy(src: &Path, target: &str) -> Result<Vec<SymbolDoc>, String> {
    find_symbols_with_parse_mode(src, target, true)
}

fn find_symbols_with_parse_mode(
    src: &Path,
    target: &str,
    skip_parse_errors: bool,
) -> Result<Vec<SymbolDoc>, String> {
    let mut out = Vec::new();
    for file in rust_files(src) {
        let source = std::fs::read_to_string(&file)
            .map_err(|err| format!("failed to read {}: {err}", file.display()))?;
        let parsed = match syn::parse_file(&source) {
            Ok(parsed) => parsed,
            Err(err) if skip_parse_errors => {
                if let Some(doc) = fallback_rust_src_doc(file.clone(), &source, target) {
                    out.push(doc);
                } else {
                    eprintln!(
                        "check-docs: warning: skipped unparsable Rust source file {}: {err}",
                        file.display()
                    );
                }
                continue;
            }
            Err(err) => return Err(format!("failed to parse {}: {err}", file.display())),
        };
        collect_items_recursive(&file, &parsed.items, target, &mut out);
    }
    Ok(out)
}

fn fallback_rust_src_doc(path: PathBuf, source: &str, target: &str) -> Option<SymbolDoc> {
    let needles = [
        ("struct", format!("struct {target}")),
        ("enum", format!("enum {target}")),
        ("trait", format!("trait {target}")),
        ("type", format!("type {target}")),
        ("fn", format!("fn {target}")),
    ];
    for (kind, needle) in needles {
        let Some((line_index, line)) = source
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(&needle))
        else {
            continue;
        };
        let public = line.contains(&format!("pub {needle}"));
        return Some(SymbolDoc {
            path,
            line: line_index + 1,
            kind,
            name: target.to_string(),
            definition: line.trim().to_string(),
            details: Vec::new(),
            docs: Vec::new(),
            derives: Vec::new(),
            public,
            reexported: false,
        });
    }
    None
}

pub(crate) fn add_reexported_matches(
    root_file: &Path,
    import: &ImportPath,
    packages: &[Package],
    dependencies: &HashMap<String, PackageId>,
    matches: &mut Vec<SymbolDoc>,
) -> Result<(), String> {
    let mut visited = HashSet::new();
    if let Ok(mut docs) = resolve_reexports_in_module(
        root_file,
        &import.segments,
        &import.item,
        packages,
        dependencies,
        &mut visited,
    ) {
        matches.append(&mut docs);
    }
    add_reexported_module_path_matches(root_file, import, packages, dependencies, matches)?;
    Ok(())
}

fn add_reexported_module_path_matches(
    root_file: &Path,
    import: &ImportPath,
    packages: &[Package],
    dependencies: &HashMap<String, PackageId>,
    matches: &mut Vec<SymbolDoc>,
) -> Result<(), String> {
    for index in 0..import.segments.len() {
        let parent_segments = &import.segments[..index];
        let alias = &import.segments[index];
        let remaining_segments = &import.segments[index + 1..];
        let Ok(module) = resolve_module(root_file, parent_segments, false) else {
            continue;
        };
        let mut paths = Vec::new();
        collect_reexported_module_paths(&module.items, alias, &mut paths)?;
        for path in paths {
            let mut full_path = path.clone();
            full_path.extend_from_slice(remaining_segments);
            full_path.push(import.item.clone());
            let Ok((target_root, target_segments)) = resolve_path_root(
                root_file,
                parent_segments,
                &full_path,
                packages,
                dependencies,
            ) else {
                continue;
            };
            push_resolved_docs(&target_root, &import.item, &target_segments, matches);
            let mut visited = HashSet::new();
            if let Ok(mut docs) = resolve_reexports_in_module(
                &target_root,
                &target_segments,
                &import.item,
                packages,
                dependencies,
                &mut visited,
            ) {
                matches.append(&mut docs);
            }
        }
    }
    Ok(())
}

fn collect_reexported_module_paths(
    items: &[syn::Item],
    alias: &str,
    out: &mut Vec<Vec<String>>,
) -> Result<(), String> {
    for item in items {
        if let syn::Item::Use(item) = item
            && matches!(item.vis, syn::Visibility::Public(_))
        {
            collect_reexport_module_use_paths(&item.tree, Vec::new(), alias, out)?;
        }
    }
    Ok(())
}

fn collect_reexport_module_use_paths(
    tree: &syn::UseTree,
    mut prefix: Vec<String>,
    alias: &str,
    out: &mut Vec<Vec<String>>,
) -> Result<(), String> {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_reexport_module_use_paths(&path.tree, prefix, alias, out)
        }
        syn::UseTree::Name(name) => {
            if name.ident == alias {
                prefix.push(name.ident.to_string());
                out.push(prefix);
            }
            Ok(())
        }
        syn::UseTree::Rename(rename) => {
            if rename.rename == alias {
                prefix.push(rename.ident.to_string());
                out.push(prefix);
            }
            Ok(())
        }
        syn::UseTree::Glob(_) => Ok(()),
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_reexport_module_use_paths(item, prefix.clone(), alias, out)?;
            }
            Ok(())
        }
    }
}

fn resolve_reexports_in_module(
    root_file: &Path,
    module_segments: &[String],
    target: &str,
    packages: &[Package],
    dependencies: &HashMap<String, PackageId>,
    visited: &mut HashSet<String>,
) -> Result<Vec<SymbolDoc>, String> {
    let key = format!(
        "{}::{}::{}",
        root_file.display(),
        module_segments.join("::"),
        target
    );
    if !visited.insert(key) {
        return Ok(Vec::new());
    }

    let module = resolve_module(root_file, module_segments, false)?;
    let mut out = Vec::new();
    for item in module.items {
        let syn::Item::Use(item) = item else {
            continue;
        };
        if !matches!(item.vis, syn::Visibility::Public(_)) {
            continue;
        }
        collect_reexport_use_matches(
            root_file,
            module_segments,
            &item.tree,
            Vec::new(),
            target,
            packages,
            dependencies,
            visited,
            &mut out,
        )?;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn collect_reexport_use_matches(
    root_file: &Path,
    current_segments: &[String],
    tree: &syn::UseTree,
    mut prefix: Vec<String>,
    target: &str,
    packages: &[Package],
    dependencies: &HashMap<String, PackageId>,
    visited: &mut HashSet<String>,
    out: &mut Vec<SymbolDoc>,
) -> Result<(), String> {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_reexport_use_matches(
                root_file,
                current_segments,
                &path.tree,
                prefix,
                target,
                packages,
                dependencies,
                visited,
                out,
            )
        }
        syn::UseTree::Name(name) => {
            if name.ident == target {
                prefix.push(name.ident.to_string());
                let _ = resolve_reexport_path(
                    root_file,
                    current_segments,
                    &prefix,
                    target,
                    packages,
                    dependencies,
                    visited,
                    out,
                );
            }
            Ok(())
        }
        syn::UseTree::Rename(rename) => {
            if rename.rename == target {
                prefix.push(rename.ident.to_string());
                let _ = resolve_reexport_path(
                    root_file,
                    current_segments,
                    &prefix,
                    &rename.ident.to_string(),
                    packages,
                    dependencies,
                    visited,
                    out,
                );
            }
            Ok(())
        }
        syn::UseTree::Glob(_) => {
            let _ = resolve_glob_reexport_path(
                root_file,
                current_segments,
                &prefix,
                target,
                packages,
                dependencies,
                visited,
                out,
            );
            Ok(())
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                let _ = collect_reexport_use_matches(
                    root_file,
                    current_segments,
                    item,
                    prefix.clone(),
                    target,
                    packages,
                    dependencies,
                    visited,
                    out,
                );
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_reexport_path(
    root_file: &Path,
    current_segments: &[String],
    path: &[String],
    target: &str,
    packages: &[Package],
    dependencies: &HashMap<String, PackageId>,
    visited: &mut HashSet<String>,
    out: &mut Vec<SymbolDoc>,
) -> Result<(), String> {
    if path.len() < 2 {
        return Ok(());
    }
    let (target_root, segments) =
        resolve_path_root(root_file, current_segments, path, packages, dependencies)?;
    let item_name = path.last().expect("reexport path is non-empty");
    push_resolved_docs(&target_root, item_name, &segments, out);
    if let Ok(mut nested) = resolve_reexports_in_module(
        &target_root,
        &segments,
        target,
        packages,
        dependencies,
        visited,
    ) {
        out.append(&mut nested);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn resolve_glob_reexport_path(
    root_file: &Path,
    current_segments: &[String],
    path: &[String],
    target: &str,
    packages: &[Package],
    dependencies: &HashMap<String, PackageId>,
    visited: &mut HashSet<String>,
    out: &mut Vec<SymbolDoc>,
) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    let mut full_path = path.to_vec();
    full_path.push(target.to_string());
    let (target_root, segments) = resolve_path_root(
        root_file,
        current_segments,
        &full_path,
        packages,
        dependencies,
    )?;
    push_resolved_docs(&target_root, target, &segments, out);
    if let Ok(mut nested) = resolve_reexports_in_module(
        &target_root,
        &segments,
        target,
        packages,
        dependencies,
        visited,
    ) {
        out.append(&mut nested);
    }
    Ok(())
}

fn push_resolved_docs(root_file: &Path, item: &str, segments: &[String], out: &mut Vec<SymbolDoc>) {
    if let Ok(docs) = find_symbols_internal(root_file, item, segments) {
        for mut doc in docs {
            doc.reexported = true;
            out.push(doc);
        }
        return;
    }

    let Some(src) = root_file.parent() else {
        return;
    };
    let Ok(mut docs) = find_symbols_lossy(src, item) else {
        return;
    };
    docs.retain(|doc| path_components_match(&doc.path, src, segments));
    for mut doc in docs {
        doc.reexported = true;
        out.push(doc);
    }
}

fn path_components_match(path: &Path, src: &Path, segments: &[String]) -> bool {
    let Ok(relative) = path.strip_prefix(src) else {
        return false;
    };
    let text = relative.display().to_string().replace('-', "_");
    segments.iter().all(|segment| {
        text.split(['/', '\\', '.'])
            .any(|component| component == segment)
    })
}

fn resolve_path_root(
    root_file: &Path,
    current_segments: &[String],
    path: &[String],
    packages: &[Package],
    dependencies: &HashMap<String, PackageId>,
) -> Result<(PathBuf, Vec<String>), String> {
    let first = path[0].replace('-', "_");
    match first.as_str() {
        "crate" => Ok((
            root_file.to_path_buf(),
            normalize_relative_segments(&[], &path[1..path.len() - 1]),
        )),
        "self" => Ok((
            root_file.to_path_buf(),
            normalize_relative_segments(current_segments, &path[1..path.len() - 1]),
        )),
        "super" => Ok((
            root_file.to_path_buf(),
            normalize_relative_segments(current_segments, &path[..path.len() - 1]),
        )),
        crate_name => {
            if let Ok(package) = resolve_package(packages, dependencies, crate_name)
                && let Some(root) = package
                    .targets
                    .iter()
                    .find(|target| target.kind.iter().any(|kind| kind == "lib"))
                    .map(|target| target.src_path.as_std_path().to_path_buf())
            {
                return Ok((root, path[1..path.len() - 1].to_vec()));
            }
            Ok((
                root_file.to_path_buf(),
                normalize_relative_segments(current_segments, &path[..path.len() - 1]),
            ))
        }
    }
}

fn normalize_relative_segments(base: &[String], relative: &[String]) -> Vec<String> {
    let mut segments = base.to_vec();
    for segment in relative {
        match segment.as_str() {
            "self" => {}
            "super" => {
                segments.pop();
            }
            "crate" => segments.clear(),
            _ => segments.push(segment.clone()),
        }
    }
    segments
}

struct LoadedModule {
    path: PathBuf,
    child_dir: PathBuf,
    items: Vec<syn::Item>,
}

fn find_symbols_internal(
    root_file: &Path,
    target: &str,
    module_segments: &[String],
) -> Result<Vec<SymbolDoc>, String> {
    let module = resolve_module(root_file, module_segments, false)?;
    let mut out = Vec::new();
    collect_items_direct(&module.path, &module.items, target, &mut out);
    out.retain(|doc| doc.public);
    Ok(out)
}

fn resolve_module(
    root_file: &Path,
    segments: &[String],
    require_public_modules: bool,
) -> Result<LoadedModule, String> {
    let mut module = load_module_file(root_file)?;
    for segment in segments {
        module = resolve_child_module(&module, segment, require_public_modules)?;
    }
    Ok(module)
}

fn load_module_file(path: &Path) -> Result<LoadedModule, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let parsed = syn::parse_file(&source)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    Ok(LoadedModule {
        path: path.to_path_buf(),
        child_dir: module_child_dir(path),
        items: parsed.items,
    })
}

fn module_child_dir(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if path
        .file_stem()
        .is_some_and(|stem| stem == "lib" || stem == "main" || stem == "mod")
    {
        parent.to_path_buf()
    } else {
        parent.join(path.file_stem().unwrap_or_default())
    }
}

fn resolve_child_module(
    parent: &LoadedModule,
    segment: &str,
    require_public: bool,
) -> Result<LoadedModule, String> {
    let module = parent
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Mod(module)
                if module.ident == segment
                    && (!require_public || matches!(module.vis, syn::Visibility::Public(_))) =>
            {
                Some(module)
            }
            _ => None,
        })
        .ok_or_else(|| {
            format!(
                "module '{segment}' not found from {}",
                parent.path.display()
            )
        })?;

    if let Some((_, items)) = &module.content {
        return Ok(LoadedModule {
            path: parent.path.clone(),
            child_dir: parent.child_dir.join(segment),
            items: items.clone(),
        });
    }

    let path = explicit_module_path(module)
        .map(|relative| parent.child_dir.join(relative))
        .or_else(|| {
            let file_path = parent.child_dir.join(format!("{segment}.rs"));
            file_path.exists().then_some(file_path)
        })
        .or_else(|| {
            let mod_path = parent.child_dir.join(segment).join("mod.rs");
            mod_path.exists().then_some(mod_path)
        })
        .ok_or_else(|| {
            format!(
                "module file for '{segment}' not found from {}",
                parent.path.display()
            )
        })?;

    load_module_file(&path)
}

fn explicit_module_path(module: &syn::ItemMod) -> Option<PathBuf> {
    module.attrs.iter().find_map(|attr| {
        if !attr.path().is_ident("path") {
            return None;
        }
        match &attr.meta {
            syn::Meta::NameValue(value) => match &value.value {
                syn::Expr::Lit(expr_lit) => match &expr_lit.lit {
                    syn::Lit::Str(value) => Some(PathBuf::from(value.value())),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    })
}

fn collect_items_direct(path: &Path, items: &[syn::Item], target: &str, out: &mut Vec<SymbolDoc>) {
    for item in items {
        collect_item(path, item, target, out);
    }
}

fn collect_items_recursive(
    path: &Path,
    items: &[syn::Item],
    target: &str,
    out: &mut Vec<SymbolDoc>,
) {
    for item in items {
        collect_item(path, item, target, out);
        if let syn::Item::Mod(module) = item
            && let Some((_, items)) = &module.content
        {
            collect_items_recursive(path, items, target, out);
        }
    }
}

fn collect_item(path: &Path, item: &syn::Item, target: &str, out: &mut Vec<SymbolDoc>) {
    match item {
        syn::Item::Fn(function) if function.sig.ident == target => {
            out.push(function_doc(path.to_path_buf(), function));
        }
        syn::Item::Struct(item) if item.ident == target => {
            out.push(struct_doc(path.to_path_buf(), item));
        }
        syn::Item::Enum(item) if item.ident == target => {
            out.push(enum_doc(path.to_path_buf(), item));
        }
        syn::Item::Trait(item) if item.ident == target => {
            out.push(trait_doc(path.to_path_buf(), item));
        }
        syn::Item::Type(item) if item.ident == target => {
            out.push(type_doc(path.to_path_buf(), item));
        }
        syn::Item::Const(item) if item.ident == target => {
            out.push(const_doc(path.to_path_buf(), item));
        }
        syn::Item::Static(item) if item.ident == target => {
            out.push(static_doc(path.to_path_buf(), item));
        }
        syn::Item::Union(item) if item.ident == target => {
            out.push(union_doc(path.to_path_buf(), item));
        }
        syn::Item::Macro(item) => {
            if let Some(doc) = macro_symbol_doc(path.to_path_buf(), item, target) {
                out.push(doc);
            }
        }
        _ => {}
    }
}

fn function_doc(path: PathBuf, function: &syn::ItemFn) -> SymbolDoc {
    SymbolDoc {
        path,
        line: function.sig.span().start().line,
        kind: "fn",
        name: function.sig.ident.to_string(),
        definition: function.sig.to_token_stream().to_string(),
        details: Vec::new(),
        docs: doc_lines(&function.attrs),
        derives: Vec::new(),
        reexported: false,
        public: matches!(function.vis, syn::Visibility::Public(_)),
    }
}

fn struct_doc(path: PathBuf, item: &syn::ItemStruct) -> SymbolDoc {
    SymbolDoc {
        path,
        line: item.ident.span().start().line,
        kind: "struct",
        name: item.ident.to_string(),
        definition: format!(
            "{}struct {}{}{} {}",
            visibility(&item.vis),
            item.ident,
            item.generics.to_token_stream(),
            where_clause(&item.generics),
            fields_definition(&item.fields)
        ),
        details: fields_details(&item.fields),
        docs: doc_lines(&item.attrs),
        derives: derive_lines(&item.attrs),
        reexported: false,
        public: matches!(item.vis, syn::Visibility::Public(_)),
    }
}

fn enum_doc(path: PathBuf, item: &syn::ItemEnum) -> SymbolDoc {
    let details = item
        .variants
        .iter()
        .map(|variant| match &variant.fields {
            syn::Fields::Unit => variant.ident.to_string(),
            fields => format!("{} {}", variant.ident, fields_inline(fields)),
        })
        .collect();
    SymbolDoc {
        path,
        line: item.ident.span().start().line,
        kind: "enum",
        name: item.ident.to_string(),
        definition: format!(
            "{}enum {}{}{} {{ {} }}",
            visibility(&item.vis),
            item.ident,
            item.generics.to_token_stream(),
            where_clause(&item.generics),
            enum_variants_definition(item)
        ),
        details,
        docs: doc_lines(&item.attrs),
        derives: derive_lines(&item.attrs),
        reexported: false,
        public: matches!(item.vis, syn::Visibility::Public(_)),
    }
}

fn trait_doc(path: PathBuf, item: &syn::ItemTrait) -> SymbolDoc {
    let items_definition = item
        .items
        .iter()
        .map(|item| item.to_token_stream().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let details = item
        .items
        .iter()
        .map(|item| match item {
            syn::TraitItem::Fn(f) => f.sig.to_token_stream().to_string(),
            syn::TraitItem::Type(t) => format!("type {}{}", t.ident, t.generics.to_token_stream()),
            syn::TraitItem::Const(c) => format!("const {} : {}", c.ident, c.ty.to_token_stream()),
            syn::TraitItem::Macro(m) => m.mac.to_token_stream().to_string(),
            _ => item.to_token_stream().to_string(),
        })
        .collect();
    SymbolDoc {
        path,
        line: item.ident.span().start().line,
        kind: "trait",
        name: item.ident.to_string(),
        definition: format!(
            "{}{}trait {}{}{}{} {{ {} }}",
            visibility(&item.vis),
            if item.unsafety.is_some() {
                "unsafe "
            } else {
                ""
            },
            item.ident,
            item.generics.to_token_stream(),
            supertraits(item),
            where_clause(&item.generics),
            items_definition
        ),
        details,
        docs: doc_lines(&item.attrs),
        derives: Vec::new(),
        reexported: false,
        public: matches!(item.vis, syn::Visibility::Public(_)),
    }
}

fn type_doc(path: PathBuf, item: &syn::ItemType) -> SymbolDoc {
    SymbolDoc {
        path,
        line: item.ident.span().start().line,
        kind: "type",
        name: item.ident.to_string(),
        definition: format!(
            "{}type {}{} = {} ;",
            visibility(&item.vis),
            item.ident,
            item.generics.to_token_stream(),
            item.ty.to_token_stream()
        ),
        details: Vec::new(),
        docs: doc_lines(&item.attrs),
        derives: Vec::new(),
        reexported: false,
        public: matches!(item.vis, syn::Visibility::Public(_)),
    }
}

fn const_doc(path: PathBuf, item: &syn::ItemConst) -> SymbolDoc {
    SymbolDoc {
        path,
        line: item.ident.span().start().line,
        kind: "const",
        name: item.ident.to_string(),
        definition: format!(
            "{}const {} : {} = ... ;",
            visibility(&item.vis),
            item.ident,
            item.ty.to_token_stream()
        ),
        details: Vec::new(),
        docs: doc_lines(&item.attrs),
        derives: Vec::new(),
        reexported: false,
        public: matches!(item.vis, syn::Visibility::Public(_)),
    }
}

fn static_doc(path: PathBuf, item: &syn::ItemStatic) -> SymbolDoc {
    SymbolDoc {
        path,
        line: item.ident.span().start().line,
        kind: "static",
        name: item.ident.to_string(),
        definition: format!(
            "{}static {} : {} = ... ;",
            visibility(&item.vis),
            item.ident,
            item.ty.to_token_stream()
        ),
        details: Vec::new(),
        docs: doc_lines(&item.attrs),
        derives: Vec::new(),
        reexported: false,
        public: matches!(item.vis, syn::Visibility::Public(_)),
    }
}

fn union_doc(path: PathBuf, item: &syn::ItemUnion) -> SymbolDoc {
    SymbolDoc {
        path,
        line: item.ident.span().start().line,
        kind: "union",
        name: item.ident.to_string(),
        definition: format!(
            "{}union {}{}{} {{ {} }}",
            visibility(&item.vis),
            item.ident,
            item.generics.to_token_stream(),
            where_clause(&item.generics),
            fields_definition(&syn::Fields::Named(item.fields.clone()))
        ),
        details: item
            .fields
            .named
            .iter()
            .map(|field| {
                format!(
                    "{}{}: {}",
                    visibility(&field.vis),
                    field.ident.as_ref().unwrap(),
                    field.ty.to_token_stream()
                )
            })
            .collect(),
        docs: doc_lines(&item.attrs),
        derives: derive_lines(&item.attrs),
        reexported: false,
        public: matches!(item.vis, syn::Visibility::Public(_)),
    }
}

fn macro_symbol_doc(path: PathBuf, item: &syn::ItemMacro, target: &str) -> Option<SymbolDoc> {
    let tokens = item.mac.tokens.to_string();
    let needle_struct = format!("struct {target}");
    let needle_enum = format!("enum {target}");
    let needle_trait = format!("trait {target}");
    let (kind, needle) = if macro_tokens_contain_item(&item.mac.tokens, "struct", target) {
        ("struct", needle_struct)
    } else if macro_tokens_contain_item(&item.mac.tokens, "enum", target) {
        ("enum", needle_enum)
    } else if macro_tokens_contain_item(&item.mac.tokens, "trait", target) {
        ("trait", needle_trait)
    } else {
        return None;
    };

    let mut docs = doc_lines(&item.attrs);
    if docs.is_empty() {
        docs = macro_doc_lines(&tokens, &needle);
    }

    Some(SymbolDoc {
        path,
        line: item.mac.span().start().line,
        kind,
        name: target.to_string(),
        definition: macro_definition(&tokens, &needle, kind, target),
        details: macro_details(&tokens, &needle),
        docs,
        derives: macro_derives(&tokens),
        reexported: false,
        public: tokens.contains(&format!("pub {needle}")),
    })
}

fn macro_tokens_contain_item(tokens: &proc_macro2::TokenStream, kind: &str, target: &str) -> bool {
    let mut previous_ident: Option<String> = None;
    for token in tokens.clone() {
        match token {
            proc_macro2::TokenTree::Ident(ident) => {
                let ident = ident.to_string();
                if previous_ident.as_deref() == Some(kind) && ident == target {
                    return true;
                }
                previous_ident = Some(ident);
            }
            proc_macro2::TokenTree::Group(group) => {
                if macro_tokens_contain_item(&group.stream(), kind, target) {
                    return true;
                }
                previous_ident = None;
            }
            _ => previous_ident = None,
        }
    }
    false
}

fn macro_definition(tokens: &str, needle: &str, kind: &str, target: &str) -> String {
    let visibility = if tokens.contains(&format!("pub {needle}")) {
        "pub "
    } else {
        ""
    };
    if kind == "trait" {
        let body = macro_body(tokens, needle);
        return format!("{visibility}trait {target} {{ {body} }}");
    }
    if kind == "enum" {
        let body = macro_body(tokens, needle);
        return format!("{visibility}enum {target} {{ {body} }}");
    }
    if kind == "struct" {
        let body = macro_body(tokens, needle);
        return format!("{visibility}struct {target} {{ {body} }}");
    }
    format!("macro-generated {kind} {target}")
}

fn macro_body(tokens: &str, needle: &str) -> String {
    let Some(start) = tokens.find(needle) else {
        return String::new();
    };
    let rest = &tokens[start + needle.len()..];
    let Some(open) = rest.find('{') else {
        return String::new();
    };
    balanced_body(&rest[open..])
}

fn macro_doc_lines(tokens: &str, needle: &str) -> Vec<String> {
    let Some(index) = tokens.find(needle) else {
        return Vec::new();
    };
    let before = &tokens[..index];
    let mut docs = Vec::new();
    let mut rest = before;
    while let Some(doc_index) = rest.find("doc = \"") {
        rest = &rest[doc_index + 7..];
        let Some((value, consumed)) = read_token_string(rest) else {
            break;
        };
        docs.push(value.trim().to_string());
        rest = &rest[consumed..];
    }
    docs
}

fn read_token_string(text: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '"' => '"',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some((out, index + 1));
        } else {
            out.push(ch);
        }
    }
    None
}

fn macro_details(tokens: &str, needle: &str) -> Vec<String> {
    let Some(start) = tokens.find(needle) else {
        return Vec::new();
    };
    let rest = &tokens[start + needle.len()..];
    let Some(open) = rest.find('{') else {
        return Vec::new();
    };
    let body = balanced_body(&rest[open..]);
    split_top_level(&body)
        .into_iter()
        .map(|line| line.trim().trim_end_matches(',').to_string())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn balanced_body(text: &str) -> String {
    let mut depth = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '{' => {
                depth += 1;
                if depth > 1 {
                    out.push(ch);
                }
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
                out.push(ch);
            }
            _ if depth > 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

fn split_top_level(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut brace = 0usize;
    for ch in text.chars() {
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '{' => brace += 1,
            '}' => brace = brace.saturating_sub(1),
            ',' if paren == 0 && bracket == 0 && brace == 0 => {
                out.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn macro_derives(tokens: &str) -> Vec<String> {
    let Some(index) = tokens.find("derive") else {
        return Vec::new();
    };
    let rest = &tokens[index + "derive".len()..];
    let Some(open) = rest.find('(') else {
        return Vec::new();
    };
    let Some(close) = rest[open + 1..].find(')') else {
        return Vec::new();
    };
    rest[open + 1..open + 1 + close]
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn attrs_prefix(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter(|attr| !attr.path().is_ident("doc"))
        .map(|attr| normalize_tokens(attr.to_token_stream().to_string()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_tokens(tokens: String) -> String {
    tokens.replace("# [", "#[")
}

fn fields_definition(fields: &syn::Fields) -> String {
    match fields {
        syn::Fields::Named(fields) => {
            let body = fields
                .named
                .iter()
                .map(|field| {
                    let attrs = attrs_prefix(&field.attrs);
                    let prefix = if attrs.is_empty() {
                        String::new()
                    } else {
                        format!("{attrs}\n    ")
                    };
                    format!(
                        "    {}{}{}: {},",
                        prefix,
                        visibility(&field.vis),
                        field.ident.as_ref().unwrap(),
                        field.ty.to_token_stream()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("{{\n{body}\n}}")
        }
        syn::Fields::Unnamed(fields) => format!(
            "({});",
            fields
                .unnamed
                .iter()
                .map(|field| {
                    let attrs = attrs_prefix(&field.attrs);
                    let prefix = if attrs.is_empty() {
                        String::new()
                    } else {
                        format!("{attrs} ")
                    };
                    format!(
                        "{}{}{}",
                        prefix,
                        visibility(&field.vis),
                        field.ty.to_token_stream()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        syn::Fields::Unit => ";".to_string(),
    }
}

fn enum_variants_definition(item: &syn::ItemEnum) -> String {
    item.variants
        .iter()
        .map(|variant| {
            let attrs = attrs_prefix(&variant.attrs);
            let prefix = if attrs.is_empty() {
                String::new()
            } else {
                format!("{attrs}\n    ")
            };
            format!(
                "    {}{}{},",
                prefix,
                variant.ident,
                match &variant.fields {
                    syn::Fields::Unit => String::new(),
                    fields => format!(" {}", fields_definition(fields).trim_end_matches(';')),
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fields_details(fields: &syn::Fields) -> Vec<String> {
    match fields {
        syn::Fields::Named(fields) => fields
            .named
            .iter()
            .map(|field| {
                format!(
                    "{}{}: {}",
                    visibility(&field.vis),
                    field.ident.as_ref().unwrap(),
                    field.ty.to_token_stream()
                )
            })
            .collect(),
        syn::Fields::Unnamed(fields) => fields
            .unnamed
            .iter()
            .enumerate()
            .map(|(index, field)| {
                format!(
                    "#{index}: {}{}",
                    visibility(&field.vis),
                    field.ty.to_token_stream()
                )
            })
            .collect(),
        syn::Fields::Unit => Vec::new(),
    }
}

fn fields_inline(fields: &syn::Fields) -> String {
    match fields {
        syn::Fields::Named(fields) => format!(
            "{{ {} }}",
            fields
                .named
                .iter()
                .map(|field| format!(
                    "{}: {}",
                    field.ident.as_ref().unwrap(),
                    field.ty.to_token_stream()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        syn::Fields::Unnamed(fields) => format!(
            "({})",
            fields
                .unnamed
                .iter()
                .map(|field| field.ty.to_token_stream().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        syn::Fields::Unit => String::new(),
    }
}

fn visibility(vis: &syn::Visibility) -> String {
    match vis {
        syn::Visibility::Inherited => String::new(),
        _ => format!("{} ", vis.to_token_stream()),
    }
}

fn where_clause(generics: &syn::Generics) -> String {
    generics
        .where_clause
        .as_ref()
        .map(|clause| format!(" {}", clause.to_token_stream()))
        .unwrap_or_default()
}

fn supertraits(item: &syn::ItemTrait) -> String {
    if item.supertraits.is_empty() {
        String::new()
    } else {
        format!(" : {}", item.supertraits.to_token_stream())
    }
}

fn doc_lines(attrs: &[syn::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            syn::Meta::NameValue(value) => match &value.value {
                syn::Expr::Lit(expr_lit) => match &expr_lit.lit {
                    syn::Lit::Str(value) => Some(value.value().trim().to_string()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn derive_lines(attrs: &[syn::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter_map(|attr| match &attr.meta {
            syn::Meta::List(list) if list.path.is_ident("derive") => Some(list.tokens.to_string()),
            _ => None,
        })
        .flat_map(|line| {
            line.split(',')
                .map(|part| part.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|line| !line.is_empty())
        .collect()
}

pub(crate) fn rank_matches(matches: &mut [SymbolDoc], import: &ImportPath) {
    matches.sort_by_cached_key(|doc| {
        let path_display = doc.path.display().to_string();
        let path_text = path_display.replace('-', "_");
        let module_hits = import
            .segments
            .iter()
            .filter(|segment| path_text.contains(segment.as_str()))
            .count();
        (
            !doc.reexported,
            std::cmp::Reverse(module_hits),
            !doc.public,
            path_display,
            doc.line,
        )
    });
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != "target" && entry.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn resolves_public_module_path_and_item_kinds() {
        let dir = TempDir::new().unwrap();
        let dir = dir.path();
        let lib = dir.join("lib.rs");
        write(&lib, "pub mod api; mod private;");
        write(
            &dir.join("api.rs"),
            r#"
            /// docs
            #[derive(Clone, Debug)]
            pub struct Config<T> where T: Clone { pub name: String, value: T }
            pub enum Mode { Fast, Slow(u8), Named { yes: bool } }
            pub trait Worker: Send { type Job; const ID: u8; fn run(&self); }
            pub type Alias = Result<(), ()>;
            pub const LIMIT: usize = 3;
            pub static FLAG: bool = true;
            pub union Bits { pub i: u32, f: f32 }
            pub fn make() -> Config<u8> { todo!() }
            struct Hidden;
            "#,
        );
        write(&dir.join("private.rs"), "pub struct Config;");

        let segment = vec!["api".to_string()];
        for (name, kind) in [
            ("Config", "struct"),
            ("Mode", "enum"),
            ("Worker", "trait"),
            ("Alias", "type"),
            ("LIMIT", "const"),
            ("FLAG", "static"),
            ("Bits", "union"),
            ("make", "fn"),
        ] {
            let docs = find_symbols(&lib, name, &segment).unwrap();
            assert_eq!(docs[0].kind, kind);
            assert!(docs[0].public);
        }
        assert!(find_symbols(&lib, "Hidden", &segment).unwrap().is_empty());
        assert!(find_symbols(&lib, "Config", &["private".to_string()]).is_err());
    }

    #[test]
    fn resolves_inline_and_path_modules_and_reexports() {
        let dir = TempDir::new().unwrap();
        let dir = dir.path();
        let lib = dir.join("lib.rs");
        write(
            &lib,
            r#"
            mod hidden { pub struct Secret; }
            pub use hidden::Secret;
            #[path = "actual.rs"] pub mod renamed;
            pub mod inline { pub struct Inside; }
            "#,
        );
        write(&dir.join("actual.rs"), "pub struct Actual;");

        let import = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "Secret".into(),
        };
        let mut matches = Vec::new();
        add_reexported_matches(&lib, &import, &[], &HashMap::new(), &mut matches).unwrap();
        assert_eq!(matches[0].name, "Secret");
        assert!(matches[0].reexported);

        assert_eq!(
            find_symbols(&lib, "Actual", &["renamed".into()]).unwrap()[0].name,
            "Actual"
        );
        assert_eq!(
            find_symbols(&lib, "Inside", &["inline".into()]).unwrap()[0].name,
            "Inside"
        );
    }

    #[test]
    fn reexport_negative_and_alias_cases() {
        let dir = TempDir::new().unwrap();
        let dir = dir.path();
        let lib = dir.join("lib.rs");
        write(
            &lib,
            r#"
            mod hidden { pub struct Secret; pub struct Original; pub struct Globbed; }
            mod facade { pub use super::hidden::Secret; }
            pub mod nested { pub use super::hidden::Secret as NestedOnly; }
            pub use hidden::Original as PublicAlias;
            pub use hidden::*;
            pub use facade::Secret as RecursiveSecret;
            mod private_inline { pub struct Boundary; }
            "#,
        );

        let nested_parent_import = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "NestedOnly".into(),
        };
        let mut matches = Vec::new();
        add_reexported_matches(
            &lib,
            &nested_parent_import,
            &[],
            &HashMap::new(),
            &mut matches,
        )
        .unwrap();
        assert!(
            matches.is_empty(),
            "nested pub use leaked into parent import"
        );

        let alias_import = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "PublicAlias".into(),
        };
        add_reexported_matches(&lib, &alias_import, &[], &HashMap::new(), &mut matches).unwrap();
        assert_eq!(matches[0].name, "Original");
        assert!(matches[0].reexported);

        let glob_import = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "Globbed".into(),
        };
        let mut glob_matches = Vec::new();
        add_reexported_matches(&lib, &glob_import, &[], &HashMap::new(), &mut glob_matches)
            .unwrap();
        assert_eq!(glob_matches[0].name, "Globbed");

        let recursive_import = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "RecursiveSecret".into(),
        };
        let mut recursive_matches = Vec::new();
        add_reexported_matches(
            &lib,
            &recursive_import,
            &[],
            &HashMap::new(),
            &mut recursive_matches,
        )
        .unwrap();
        assert_eq!(recursive_matches[0].name, "Secret");

        assert!(find_symbols(&lib, "Boundary", &["private_inline".into()]).is_err());
    }

    #[test]
    fn reexports_handle_repeated_super_and_unresolved_globs() {
        let dir = TempDir::new().unwrap();
        let dir = dir.path();
        let lib = dir.join("lib.rs");
        write(
            &lib,
            r#"
            mod hidden { pub struct Thing; }
            pub mod a { pub mod b { pub use super::super::hidden::Thing; } }
            pub use cfg_hidden::*;
            "#,
        );

        let import = ImportPath {
            crate_name: "x".into(),
            segments: vec!["a".into(), "b".into()],
            item: "Thing".into(),
        };
        let mut matches = Vec::new();
        add_reexported_matches(&lib, &import, &[], &HashMap::new(), &mut matches).unwrap();
        assert_eq!(matches[0].name, "Thing");

        let missing = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "Missing".into(),
        };
        let mut missing_matches = Vec::new();
        add_reexported_matches(&lib, &missing, &[], &HashMap::new(), &mut missing_matches).unwrap();
        assert!(missing_matches.is_empty());
    }

    #[test]
    fn lossy_scan_and_macro_helpers_work() {
        let dir = TempDir::new().unwrap();
        let dir = dir.path();
        write(&dir.join("a.rs"), "/// bad\npub struct Bad {\n");
        write(
            &dir.join("b.rs"),
            r#"
            macro_rules! m { () => {
                #[doc = "macro docs"]
                #[derive(Clone, Copy)]
                pub enum Generated { A, B(u8) }
            }}
            "#,
        );
        let docs = find_symbols_lossy(dir, "Bad").unwrap();
        assert_eq!(docs[0].kind, "struct");

        let source = std::fs::read_to_string(dir.join("b.rs")).unwrap();
        let parsed = syn::parse_file(&source).unwrap();
        let mut out = Vec::new();
        collect_items_recursive(&dir.join("b.rs"), &parsed.items, "Generated", &mut out);
        assert_eq!(out[0].kind, "enum");
        assert!(out[0].definition.contains("Generated"));
        assert!(out[0].docs.iter().any(|line| line.contains("macro docs")));
        assert!(out[0].derives.iter().any(|line| line.contains("Clone")));
    }

    #[test]
    fn external_reexports_and_reexport_errors() {
        let dir = TempDir::new().unwrap();
        let dir = dir.path();
        let lib = dir.join("lib.rs");
        write(&lib, "pub use cargo_metadata::MetadataCommand;");
        let glob_lib = dir.join("glob.rs");
        write(&glob_lib, "pub use syn::*;");

        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path("Cargo.toml")
            .exec()
            .unwrap();
        let root = metadata.root_package().unwrap();
        let deps = crate::resolver::package_dependencies(&metadata, &root.id);
        let import = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "MetadataCommand".into(),
        };
        let mut matches = Vec::new();
        add_reexported_matches(&lib, &import, &metadata.packages, &deps, &mut matches).unwrap();
        assert_eq!(matches[0].name, "MetadataCommand");
        assert!(matches[0].reexported);

        let glob = ImportPath {
            crate_name: "x".into(),
            segments: vec![],
            item: "Nope".into(),
        };
        let mut glob_matches = Vec::new();
        add_reexported_matches(
            &glob_lib,
            &glob,
            &metadata.packages,
            &deps,
            &mut glob_matches,
        )
        .unwrap();
        assert!(glob_matches.is_empty());
    }

    #[test]
    fn module_files_parse_errors_and_macro_branches() {
        let dir = TempDir::new().unwrap();
        let dir = dir.path();
        let lib = dir.join("lib.rs");
        write(&lib, "pub mod outer;");
        write(&dir.join("outer/mod.rs"), "pub struct FromModRs;");
        assert_eq!(
            find_symbols(&lib, "FromModRs", &["outer".into()]).unwrap()[0].name,
            "FromModRs"
        );

        write(&dir.join("bad.rs"), "pub struct Nope {");
        assert!(
            find_symbols_with_parse_mode(dir, "Nope", false)
                .unwrap_err()
                .contains("failed to parse")
        );
        assert!(find_symbols_lossy(dir, "Missing").unwrap().is_empty());

        let macro_source = r#"
            macro_rules! s { () => { pub struct MadeStruct { pub id: usize } } }
            macro_rules! t { () => { pub trait MadeTrait { fn go(&self); } } }
            macro_rules! private { () => { struct PrivateMade { id: usize } } }
            macro_rules! none { () => { pub fn ignored() {} } }
        "#;
        let parsed = syn::parse_file(macro_source).unwrap();
        let mut structs = Vec::new();
        collect_items_recursive(&lib, &parsed.items, "MadeStruct", &mut structs);
        assert_eq!(structs[0].kind, "struct");
        let mut traits = Vec::new();
        collect_items_recursive(&lib, &parsed.items, "MadeTrait", &mut traits);
        assert_eq!(traits[0].kind, "trait");
        let mut private = Vec::new();
        collect_items_recursive(&lib, &parsed.items, "PrivateMade", &mut private);
        assert!(!private[0].public);
        let mut none = Vec::new();
        collect_items_recursive(&lib, &parsed.items, "NotThere", &mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn ranking_prefers_reexported_public_module_hits() {
        let import = ImportPath {
            crate_name: "x".into(),
            segments: vec!["api".into()],
            item: "Thing".into(),
        };
        let mut docs = vec![
            SymbolDoc {
                path: PathBuf::from("src/other.rs"),
                line: 2,
                kind: "struct",
                name: "Thing".into(),
                definition: String::new(),
                details: Vec::new(),
                docs: Vec::new(),
                derives: Vec::new(),
                public: false,
                reexported: false,
            },
            SymbolDoc {
                path: PathBuf::from("src/api.rs"),
                line: 1,
                kind: "struct",
                name: "Thing".into(),
                definition: String::new(),
                details: Vec::new(),
                docs: Vec::new(),
                derives: Vec::new(),
                public: true,
                reexported: true,
            },
        ];
        rank_matches(&mut docs, &import);
        assert!(docs[0].reexported);
    }

    #[test]
    fn formatting_helpers_cover_edge_cases() {
        assert_eq!(normalize_tokens("# [cfg(test)]".into()), "#[cfg(test)]");
        assert_eq!(balanced_body("{a,{b},c} tail"), "a,{b},c");
        assert_eq!(split_top_level("a, b(c,d), e { f, g }").len(), 3);
        assert_eq!(read_token_string("a\\n\\\"b\" rest").unwrap().0, "a\n\"b");
        assert!(macro_derives("#[derive(Clone, Debug)] struct X;").contains(&"Clone".into()));
        assert!(doc_lines(&[]).is_empty());
        assert!(derive_lines(&[]).is_empty());
    }
}

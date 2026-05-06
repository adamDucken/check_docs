use crate::imports::ImportPath;
use rustdoc_types::{
    Crate, GenericParamDefKind, Id, Item, ItemEnum, MacroKind, StructKind, Type, VariantKind,
    Visibility,
};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct SymbolDoc {
    pub(crate) path: PathBuf,
    pub(crate) line: usize,
    pub(crate) kind: &'static str,
    pub(crate) name: String,
    pub(crate) definition: String,
    pub(crate) details: Vec<String>,
    pub(crate) docs: Vec<String>,
    pub(crate) derives: Vec<String>,
}

pub(crate) fn find_symbol(krate: &Crate, import: &ImportPath) -> Result<SymbolDoc, String> {
    let mut current = krate.root;
    let mut parts = import.segments.clone();
    parts.push(import.item.clone());

    for (index, part) in parts.iter().enumerate() {
        let is_last = index + 1 == parts.len();
        current = find_child(krate, current, part, is_last, &mut HashSet::new())?;
        current = follow_use(krate, current, &mut HashSet::new())?;
        if !is_last && !matches!(item(krate, current)?.inner, ItemEnum::Module(_)) {
            return Err(format!("path segment '{part}' resolved to non-module item"));
        }
    }

    let item = item(krate, current)?;
    Ok(format_item(krate, item))
}

fn find_child(
    krate: &Crate,
    module_id: Id,
    name: &str,
    is_last: bool,
    visited: &mut HashSet<Id>,
) -> Result<Id, String> {
    if !visited.insert(module_id) {
        return Err(format!("cycle while resolving '{name}'"));
    }
    let module_item = item(krate, module_id)?;
    let ItemEnum::Module(module) = &module_item.inner else {
        return Err(format!(
            "item '{}' is not a module",
            module_item.name.clone().unwrap_or_default()
        ));
    };

    for child_id in &module.items {
        let child = item(krate, *child_id)?;
        if !is_public(child) {
            continue;
        }
        if exported_name(child).as_deref() == Some(name) {
            return Ok(*child_id);
        }
    }

    for child_id in &module.items {
        let child = item(krate, *child_id)?;
        if !is_public(child) {
            continue;
        }
        let ItemEnum::Use(use_item) = &child.inner else {
            continue;
        };
        if use_item.is_glob {
            let Some(glob_id) = use_item.id else {
                if is_last {
                    return Err(format!(
                        "glob import '{}' has no resolved id",
                        use_item.source
                    ));
                }
                continue;
            };
            let target = follow_use(krate, glob_id, &mut HashSet::new())?;
            if matches!(item(krate, target)?.inner, ItemEnum::Module(_))
                && let Ok(found) = find_child(krate, target, name, is_last, visited)
            {
                return Ok(found);
            }
        }
    }

    Err(format!(
        "'{name}' not found under {}",
        path_label(krate, module_id)
    ))
}

fn follow_use(krate: &Crate, mut id: Id, visited: &mut HashSet<Id>) -> Result<Id, String> {
    loop {
        if !visited.insert(id) {
            return Err("cycle while following rustdoc use item".to_string());
        }
        let current = item(krate, id)?;
        let ItemEnum::Use(use_item) = &current.inner else {
            return Ok(id);
        };
        let Some(next) = use_item.id else {
            return Err(format!("use '{}' has no resolved id", use_item.source));
        };
        id = next;
    }
}

fn item(krate: &Crate, id: Id) -> Result<&Item, String> {
    krate
        .index
        .get(&id)
        .ok_or_else(|| format!("rustdoc item id {:?} missing from index", id))
}

fn exported_name(item: &Item) -> Option<String> {
    match &item.inner {
        ItemEnum::Use(use_item) => Some(use_item.name.clone()),
        _ => item.name.clone(),
    }
}

fn is_public(item: &Item) -> bool {
    matches!(item.visibility, Visibility::Public | Visibility::Default)
}

fn path_label(krate: &Crate, id: Id) -> String {
    krate
        .paths
        .get(&id)
        .map(|summary| summary.path.join("::"))
        .or_else(|| krate.index.get(&id).and_then(|item| item.name.clone()))
        .unwrap_or_else(|| format!("{:?}", id))
}

fn format_item(krate: &Crate, item: &Item) -> SymbolDoc {
    let name = item
        .name
        .clone()
        .unwrap_or_else(|| exported_name(item).unwrap_or_default());
    let (kind, definition, details) = match &item.inner {
        ItemEnum::Struct(s) => (
            "struct",
            struct_def(krate, &name, s),
            struct_details(krate, s),
        ),
        ItemEnum::Enum(e) => ("enum", enum_def(krate, &name, e), enum_details(krate, e)),
        ItemEnum::Trait(t) => ("trait", trait_def(krate, &name, t), trait_details(krate, t)),
        ItemEnum::Function(f) => ("fn", fn_def(&name, f), Vec::new()),
        ItemEnum::TypeAlias(t) => (
            "type",
            format!(
                "pub type {}{} = {};",
                name,
                generics(&t.generics),
                type_str(&t.type_)
            ),
            Vec::new(),
        ),
        ItemEnum::Constant { type_, .. } => (
            "const",
            format!("pub const {name}: {} = ...;", type_str(type_)),
            Vec::new(),
        ),
        ItemEnum::Static(s) => (
            "static",
            format!(
                "pub static {}{name}: {} = ...;",
                if s.is_mutable { "mut " } else { "" },
                type_str(&s.type_)
            ),
            Vec::new(),
        ),
        ItemEnum::Union(u) => ("union", union_def(krate, &name, u), union_details(krate, u)),
        ItemEnum::Macro(_) => (
            "macro",
            format!("macro_rules! {name} {{ ... }}"),
            Vec::new(),
        ),
        ItemEnum::ProcMacro(pm) => match pm.kind {
            MacroKind::Bang => ("macro", format!("pub macro {name}!(...)"), Vec::new()),
            MacroKind::Attr => ("proc-attribute", format!("#[{name}]"), Vec::new()),
            MacroKind::Derive => ("proc-derive", format!("#[derive({name})]"), Vec::new()),
        },
        ItemEnum::Use(u) => (
            "use",
            format!("pub use {} as {};", u.source, u.name),
            Vec::new(),
        ),
        _ => (
            "item",
            format!("pub {} {name}", kind_name(&item.inner)),
            Vec::new(),
        ),
    };
    SymbolDoc {
        path: item
            .span
            .as_ref()
            .map(|span| span.filename.clone())
            .unwrap_or_default(),
        line: item.span.as_ref().map(|span| span.begin.0).unwrap_or(0),
        kind,
        name,
        definition,
        details,
        docs: item
            .docs
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect(),
        derives: Vec::new(),
    }
}

fn kind_name(inner: &ItemEnum) -> &'static str {
    match inner {
        ItemEnum::Module(_) => "module",
        ItemEnum::ExternCrate { .. } => "extern crate",
        ItemEnum::Use(_) => "use",
        ItemEnum::StructField(_) => "field",
        ItemEnum::Variant(_) => "variant",
        ItemEnum::TraitAlias(_) => "trait alias",
        ItemEnum::Impl(_) => "impl",
        ItemEnum::ExternType => "extern type",
        ItemEnum::Primitive(_) => "primitive",
        ItemEnum::AssocConst { .. } => "assoc const",
        ItemEnum::AssocType { .. } => "assoc type",
        _ => "item",
    }
}

fn struct_def(krate: &Crate, name: &str, s: &rustdoc_types::Struct) -> String {
    match &s.kind {
        StructKind::Unit => format!("pub struct {name}{};", generics(&s.generics)),
        StructKind::Tuple(fields) => format!(
            "pub struct {name}{}({});",
            generics(&s.generics),
            fields
                .iter()
                .map(|id| id
                    .and_then(|id| field_type(krate, id))
                    .unwrap_or_else(|| "_".into()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        StructKind::Plain { fields, .. } => format!(
            "pub struct {name}{} {{ {} }}",
            generics(&s.generics),
            fields
                .iter()
                .filter_map(|id| field_line(krate, *id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn struct_details(krate: &Crate, s: &rustdoc_types::Struct) -> Vec<String> {
    match &s.kind {
        StructKind::Plain { fields, .. } => fields
            .iter()
            .filter_map(|id| field_line(krate, *id))
            .collect(),
        StructKind::Tuple(fields) => fields
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                id.and_then(|id| field_type(krate, id))
                    .map(|ty| format!("#{i}: {ty}"))
            })
            .collect(),
        StructKind::Unit => Vec::new(),
    }
}

fn enum_def(krate: &Crate, name: &str, e: &rustdoc_types::Enum) -> String {
    format!(
        "pub enum {name}{} {{ {} }}",
        generics(&e.generics),
        enum_details(krate, e).join(", ")
    )
}

fn enum_details(krate: &Crate, e: &rustdoc_types::Enum) -> Vec<String> {
    e.variants
        .iter()
        .filter_map(|id| {
            let item = krate.index.get(id)?;
            let name = item.name.clone()?;
            let ItemEnum::Variant(v) = &item.inner else {
                return Some(name);
            };
            Some(match &v.kind {
                VariantKind::Plain => name,
                VariantKind::Tuple(fields) => format!(
                    "{}({})",
                    name,
                    fields
                        .iter()
                        .map(|id| id
                            .and_then(|id| field_type(krate, id))
                            .unwrap_or_else(|| "_".into()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                VariantKind::Struct { fields, .. } => format!(
                    "{} {{ {} }}",
                    name,
                    fields
                        .iter()
                        .filter_map(|id| field_line(krate, *id))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })
        })
        .collect()
}

fn trait_def(_krate: &Crate, name: &str, t: &rustdoc_types::Trait) -> String {
    let prefix = if t.is_unsafe {
        "pub unsafe trait"
    } else {
        "pub trait"
    };
    format!("{prefix} {name}{} {{ ... }}", generics(&t.generics))
}

fn trait_details(krate: &Crate, t: &rustdoc_types::Trait) -> Vec<String> {
    t.items
        .iter()
        .filter_map(|id| krate.index.get(id))
        .filter_map(|item| {
            let name = item.name.clone().unwrap_or_default();
            match &item.inner {
                ItemEnum::Function(f) => Some(fn_def(&name, f)),
                ItemEnum::AssocType { .. } => Some(format!("type {name};")),
                ItemEnum::AssocConst { type_, .. } => {
                    Some(format!("const {name}: {};", type_str(type_)))
                }
                _ => None,
            }
        })
        .collect()
}

fn fn_def(name: &str, f: &rustdoc_types::Function) -> String {
    let mut prefix = String::from("pub ");
    if f.header.is_const {
        prefix.push_str("const ");
    }
    if f.header.is_async {
        prefix.push_str("async ");
    }
    if f.header.is_unsafe {
        prefix.push_str("unsafe ");
    }
    let inputs = f
        .sig
        .inputs
        .iter()
        .map(|(name, ty)| format!("{name}: {}", type_str(ty)))
        .collect::<Vec<_>>()
        .join(", ");
    let output = f
        .sig
        .output
        .as_ref()
        .map(|ty| format!(" -> {}", type_str(ty)))
        .unwrap_or_default();
    format!(
        "{prefix}fn {name}{}({inputs}){output}",
        generics(&f.generics)
    )
}

fn union_def(krate: &Crate, name: &str, u: &rustdoc_types::Union) -> String {
    format!(
        "pub union {name}{} {{ {} }}",
        generics(&u.generics),
        union_details(krate, u).join(", ")
    )
}

fn union_details(krate: &Crate, u: &rustdoc_types::Union) -> Vec<String> {
    u.fields
        .iter()
        .filter_map(|id| field_line(krate, *id))
        .collect()
}

fn field_line(krate: &Crate, id: Id) -> Option<String> {
    let field = krate.index.get(&id)?;
    let ItemEnum::StructField(ty) = &field.inner else {
        return None;
    };
    Some(format!(
        "{}: {}",
        field.name.clone().unwrap_or_else(|| "_".into()),
        type_str(ty)
    ))
}

fn field_type(krate: &Crate, id: Id) -> Option<String> {
    let field = krate.index.get(&id)?;
    let ItemEnum::StructField(ty) = &field.inner else {
        return None;
    };
    Some(type_str(ty))
}

fn generics(g: &rustdoc_types::Generics) -> String {
    if g.params.is_empty() {
        return String::new();
    }
    format!(
        "<{}>",
        g.params
            .iter()
            .map(|p| match &p.kind {
                GenericParamDefKind::Lifetime { .. } => format!("'{}", p.name),
                GenericParamDefKind::Type { .. } | GenericParamDefKind::Const { .. } =>
                    p.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn type_str(ty: &Type) -> String {
    match ty {
        Type::ResolvedPath(path) => path.path.clone(),
        Type::DynTrait(_) => "dyn Trait".to_string(),
        Type::Generic(name) => name.clone(),
        Type::Primitive(name) => name.clone(),
        Type::FunctionPointer(_) => "fn(...)".to_string(),
        Type::Tuple(items) => format!(
            "({})",
            items.iter().map(type_str).collect::<Vec<_>>().join(", ")
        ),
        Type::Slice(inner) => format!("[{}]", type_str(inner)),
        Type::Array { type_, len } => format!("[{}; {len}]", type_str(type_)),
        Type::Pat { type_, .. } => type_str(type_),
        Type::ImplTrait(_) => "impl Trait".to_string(),
        Type::Infer => "_".to_string(),
        Type::RawPointer { is_mutable, type_ } => format!(
            "*{} {}",
            if *is_mutable { "mut" } else { "const" },
            type_str(type_)
        ),
        Type::BorrowedRef {
            lifetime,
            is_mutable,
            type_,
        } => format!(
            "&{}{}{}",
            lifetime
                .as_ref()
                .map(|l| format!("'{l} "))
                .unwrap_or_default(),
            if *is_mutable { "mut " } else { "" },
            type_str(type_)
        ),
        Type::QualifiedPath { name, .. } => name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustdoc_types::{
        Abi, Attribute, Constant, Enum, Function, FunctionHeader, FunctionSignature,
        GenericParamDef, GenericParamDefKind, Generics, Item, Module, Path, ProcMacro, Static,
        Struct, Trait, TypeAlias, Union, Use, Variant,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn generics_empty() -> Generics {
        Generics {
            params: Vec::new(),
            where_predicates: Vec::new(),
        }
    }

    fn generics_all() -> Generics {
        Generics {
            params: vec![
                GenericParamDef {
                    name: "a".into(),
                    kind: GenericParamDefKind::Lifetime { outlives: vec![] },
                },
                GenericParamDef {
                    name: "T".into(),
                    kind: GenericParamDefKind::Type {
                        bounds: vec![],
                        default: None,
                        is_synthetic: false,
                    },
                },
                GenericParamDef {
                    name: "N".into(),
                    kind: GenericParamDefKind::Const {
                        type_: Type::Primitive("usize".into()),
                        default: None,
                    },
                },
            ],
            where_predicates: Vec::new(),
        }
    }

    fn span(line: usize) -> rustdoc_types::Span {
        rustdoc_types::Span {
            filename: PathBuf::from(format!("src/{line}.rs")),
            begin: (line, 1),
            end: (line, 10),
        }
    }

    fn item(id: u32, name: Option<&str>, visibility: Visibility, inner: ItemEnum) -> Item {
        Item {
            id: Id(id),
            crate_id: 0,
            name: name.map(ToOwned::to_owned),
            span: Some(span(id as usize)),
            visibility,
            docs: name.map(|n| format!("docs for {n}\n\nmore")),
            links: HashMap::new(),
            attrs: vec![Attribute::Other("#[cfg(test)]".into())],
            deprecation: None,
            inner,
        }
    }

    fn krate(items: Vec<Item>, root: Id) -> Crate {
        Crate {
            root,
            crate_version: Some("1.0.0".into()),
            includes_private: false,
            index: items.into_iter().map(|i| (i.id, i)).collect(),
            paths: HashMap::new(),
            external_crates: HashMap::new(),
            target: rustdoc_types::Target {
                triple: "x86_64-unknown-linux-gnu".into(),
                target_features: Vec::new(),
            },
            format_version: rustdoc_types::FORMAT_VERSION,
        }
    }

    fn function() -> Function {
        Function {
            sig: FunctionSignature {
                inputs: vec![("x".into(), Type::Primitive("u8".into()))],
                output: Some(Type::Primitive("bool".into())),
                is_c_variadic: false,
            },
            generics: generics_all(),
            header: FunctionHeader {
                is_const: true,
                is_unsafe: true,
                is_async: true,
                abi: Abi::Rust,
            },
            has_body: true,
        }
    }

    #[test]
    fn graph_resolves_direct_modules_uses_globs_and_cycles() {
        let root = item(
            1,
            Some("fixture"),
            Visibility::Public,
            ItemEnum::Module(Module {
                is_crate: true,
                items: vec![Id(2), Id(8), Id(9), Id(10)],
                is_stripped: false,
            }),
        );
        let api = item(
            2,
            Some("api"),
            Visibility::Public,
            ItemEnum::Module(Module {
                is_crate: false,
                items: vec![Id(3), Id(4), Id(5), Id(6), Id(7)],
                is_stripped: false,
            }),
        );
        let hidden = item(
            3,
            Some("Hidden"),
            Visibility::Crate,
            ItemEnum::Struct(Struct {
                kind: StructKind::Unit,
                generics: generics_empty(),
                impls: vec![],
            }),
        );
        let field = item(
            4,
            Some("name"),
            Visibility::Public,
            ItemEnum::StructField(Type::Primitive("String".into())),
        );
        let config = item(
            5,
            Some("Config"),
            Visibility::Public,
            ItemEnum::Struct(Struct {
                kind: StructKind::Plain {
                    fields: vec![Id(4)],
                    has_stripped_fields: false,
                },
                generics: generics_all(),
                impls: vec![],
            }),
        );
        let alias = item(
            6,
            Some("Alias"),
            Visibility::Public,
            ItemEnum::Use(Use {
                source: "api::Config".into(),
                name: "Alias".into(),
                id: Some(Id(5)),
                is_glob: false,
            }),
        );
        let cycle = item(
            7,
            Some("Cycle"),
            Visibility::Public,
            ItemEnum::Use(Use {
                source: "api::Cycle".into(),
                name: "Cycle".into(),
                id: Some(Id(7)),
                is_glob: false,
            }),
        );
        let glob_mod = item(
            8,
            Some("glob_mod"),
            Visibility::Public,
            ItemEnum::Module(Module {
                is_crate: false,
                items: vec![Id(11)],
                is_stripped: false,
            }),
        );
        let glob_use = item(
            9,
            Some("glob"),
            Visibility::Public,
            ItemEnum::Use(Use {
                source: "glob_mod::*".into(),
                name: "glob".into(),
                id: Some(Id(8)),
                is_glob: true,
            }),
        );
        let bad_glob = item(
            10,
            Some("bad"),
            Visibility::Public,
            ItemEnum::Use(Use {
                source: "missing::*".into(),
                name: "bad".into(),
                id: None,
                is_glob: true,
            }),
        );
        let globbed = item(
            11,
            Some("Globbed"),
            Visibility::Public,
            ItemEnum::Enum(Enum {
                generics: generics_empty(),
                has_stripped_variants: false,
                variants: vec![],
                impls: vec![],
            }),
        );
        let krate = krate(
            vec![
                root, api, hidden, field, config, alias, cycle, glob_mod, glob_use, bad_glob,
                globbed,
            ],
            Id(1),
        );

        let direct = find_symbol(
            &krate,
            &ImportPath {
                crate_name: "x".into(),
                segments: vec!["api".into()],
                item: "Config".into(),
            },
        )
        .unwrap();
        assert_eq!(direct.kind, "struct");
        assert!(direct.definition.contains("Config<'a, T, N>"));
        assert!(direct.details[0].contains("name: String"));
        assert!(direct.docs.iter().any(|line| line == "docs for Config"));

        let via_use = find_symbol(
            &krate,
            &ImportPath {
                crate_name: "x".into(),
                segments: vec!["api".into()],
                item: "Alias".into(),
            },
        )
        .unwrap();
        assert_eq!(via_use.name, "Config");

        let via_glob = find_symbol(
            &krate,
            &ImportPath {
                crate_name: "x".into(),
                segments: vec![],
                item: "Globbed".into(),
            },
        )
        .unwrap();
        assert_eq!(via_glob.kind, "enum");

        assert!(
            find_symbol(
                &krate,
                &ImportPath {
                    crate_name: "x".into(),
                    segments: vec!["api".into()],
                    item: "Hidden".into()
                }
            )
            .unwrap_err()
            .contains("not found")
        );
        assert!(
            find_symbol(
                &krate,
                &ImportPath {
                    crate_name: "x".into(),
                    segments: vec!["api".into()],
                    item: "Cycle".into()
                }
            )
            .unwrap_err()
            .contains("cycle")
        );
        assert!(
            find_child(&krate, Id(1), "Nope", true, &mut HashSet::new())
                .unwrap_err()
                .contains("glob import")
        );
        assert!(
            find_child(&krate, Id(5), "Nope", true, &mut HashSet::new())
                .unwrap_err()
                .contains("not a module")
        );
        assert!(
            super::item(&krate, Id(999))
                .unwrap_err()
                .contains("missing")
        );
    }

    #[test]
    fn formatter_covers_item_kinds() {
        let f1 = item(
            1,
            Some("a"),
            Visibility::Public,
            ItemEnum::StructField(Type::Primitive("u8".into())),
        );
        let f2 = item(
            2,
            None,
            Visibility::Default,
            ItemEnum::StructField(Type::BorrowedRef {
                lifetime: Some("a".into()),
                is_mutable: true,
                type_: Box::new(Type::Primitive("str".into())),
            }),
        );
        let v1 = item(
            3,
            Some("Plain"),
            Visibility::Public,
            ItemEnum::Variant(Variant {
                kind: VariantKind::Plain,
                discriminant: None,
            }),
        );
        let v2 = item(
            4,
            Some("Tuple"),
            Visibility::Public,
            ItemEnum::Variant(Variant {
                kind: VariantKind::Tuple(vec![Some(Id(1)), None]),
                discriminant: None,
            }),
        );
        let v3 = item(
            5,
            Some("Structy"),
            Visibility::Public,
            ItemEnum::Variant(Variant {
                kind: VariantKind::Struct {
                    fields: vec![Id(2)],
                    has_stripped_fields: false,
                },
                discriminant: None,
            }),
        );
        let mut items = vec![f1, f2, v1, v2, v3];
        let cases = vec![
            item(
                10,
                Some("Unit"),
                Visibility::Public,
                ItemEnum::Struct(Struct {
                    kind: StructKind::Unit,
                    generics: generics_empty(),
                    impls: vec![],
                }),
            ),
            item(
                11,
                Some("TupleStruct"),
                Visibility::Public,
                ItemEnum::Struct(Struct {
                    kind: StructKind::Tuple(vec![Some(Id(1)), None]),
                    generics: generics_empty(),
                    impls: vec![],
                }),
            ),
            item(
                12,
                Some("E"),
                Visibility::Public,
                ItemEnum::Enum(Enum {
                    generics: generics_empty(),
                    has_stripped_variants: false,
                    variants: vec![Id(3), Id(4), Id(5)],
                    impls: vec![],
                }),
            ),
            item(
                13,
                Some("run"),
                Visibility::Public,
                ItemEnum::Function(function()),
            ),
            item(
                14,
                Some("Alias"),
                Visibility::Public,
                ItemEnum::TypeAlias(TypeAlias {
                    type_: Type::Array {
                        type_: Box::new(Type::Primitive("u8".into())),
                        len: "4".into(),
                    },
                    generics: generics_empty(),
                }),
            ),
            item(
                15,
                Some("C"),
                Visibility::Public,
                ItemEnum::Constant {
                    type_: Type::Primitive("usize".into()),
                    const_: Constant {
                        expr: "1".into(),
                        value: Some("1".into()),
                        is_literal: true,
                    },
                },
            ),
            item(
                16,
                Some("S"),
                Visibility::Public,
                ItemEnum::Static(Static {
                    type_: Type::Primitive("bool".into()),
                    is_mutable: true,
                    expr: "false".into(),
                    is_unsafe: false,
                }),
            ),
            item(
                17,
                Some("U"),
                Visibility::Public,
                ItemEnum::Union(Union {
                    generics: generics_empty(),
                    has_stripped_fields: false,
                    fields: vec![Id(1)],
                    impls: vec![],
                }),
            ),
            item(
                18,
                Some("m"),
                Visibility::Public,
                ItemEnum::Macro("() => {}".into()),
            ),
            item(
                19,
                Some("bang"),
                Visibility::Public,
                ItemEnum::ProcMacro(ProcMacro {
                    kind: MacroKind::Bang,
                    helpers: vec![],
                }),
            ),
            item(
                20,
                Some("attr"),
                Visibility::Public,
                ItemEnum::ProcMacro(ProcMacro {
                    kind: MacroKind::Attr,
                    helpers: vec![],
                }),
            ),
            item(
                21,
                Some("Der"),
                Visibility::Public,
                ItemEnum::ProcMacro(ProcMacro {
                    kind: MacroKind::Derive,
                    helpers: vec!["helper".into()],
                }),
            ),
            item(
                22,
                Some("import"),
                Visibility::Public,
                ItemEnum::Use(Use {
                    source: "a::b".into(),
                    name: "import".into(),
                    id: None,
                    is_glob: false,
                }),
            ),
            item(
                23,
                Some("mod"),
                Visibility::Public,
                ItemEnum::Module(Module {
                    is_crate: false,
                    items: vec![],
                    is_stripped: false,
                }),
            ),
        ];
        items.extend(cases.clone());
        let docs = krate(items, Id(23));
        for it in &cases {
            let doc = format_item(&docs, it);
            assert!(!doc.definition.is_empty());
        }

        let trait_item = item(
            30,
            Some("go"),
            Visibility::Default,
            ItemEnum::Function(function()),
        );
        let assoc_type = item(
            31,
            Some("Out"),
            Visibility::Default,
            ItemEnum::AssocType {
                generics: generics_empty(),
                bounds: vec![],
                type_: None,
            },
        );
        let assoc_const = item(
            32,
            Some("ID"),
            Visibility::Default,
            ItemEnum::AssocConst {
                type_: Type::Primitive("u8".into()),
                value: None,
            },
        );
        let tr = item(
            33,
            Some("Worker"),
            Visibility::Public,
            ItemEnum::Trait(Trait {
                is_auto: false,
                is_unsafe: true,
                is_dyn_compatible: true,
                items: vec![Id(30), Id(31), Id(32)],
                generics: generics_empty(),
                bounds: vec![],
                implementations: vec![],
            }),
        );
        let krate = krate(
            vec![trait_item, assoc_type, assoc_const, tr.clone()],
            Id(33),
        );
        let doc = format_item(&krate, &tr);
        assert_eq!(doc.kind, "trait");
        assert_eq!(doc.details.len(), 3);
    }

    #[test]
    fn type_formatting_handles_common_shapes() {
        assert_eq!(type_str(&Type::Primitive("usize".into())), "usize");
        assert_eq!(
            type_str(&Type::Tuple(vec![Type::Primitive("u8".into())])),
            "(u8)"
        );
        assert_eq!(
            type_str(&Type::Slice(Box::new(Type::Primitive("u8".into())))),
            "[u8]"
        );
        assert_eq!(
            type_str(&Type::RawPointer {
                is_mutable: false,
                type_: Box::new(Type::Primitive("u8".into()))
            }),
            "*const u8"
        );
        assert_eq!(type_str(&Type::Infer), "_");
        assert_eq!(
            type_str(&Type::FunctionPointer(Box::new(
                rustdoc_types::FunctionPointer {
                    sig: FunctionSignature {
                        inputs: vec![],
                        output: None,
                        is_c_variadic: false
                    },
                    generic_params: vec![],
                    header: FunctionHeader {
                        is_const: false,
                        is_unsafe: false,
                        is_async: false,
                        abi: Abi::Rust
                    }
                }
            ))),
            "fn(...)"
        );
        assert_eq!(
            type_str(&Type::ResolvedPath(Path {
                path: "std::vec::Vec".into(),
                id: Id(1),
                args: None
            })),
            "std::vec::Vec"
        );
        assert_eq!(
            type_str(&Type::Pat {
                type_: Box::new(Type::Primitive("u8".into())),
                __pat_unstable_do_not_use: "1..".into()
            }),
            "u8"
        );
        assert_eq!(
            type_str(&Type::QualifiedPath {
                name: "Item".into(),
                args: None,
                self_type: Box::new(Type::Generic("T".into())),
                trait_: None
            }),
            "Item"
        );
        assert_eq!(generics(&generics_all()), "<'a, T, N>");
    }
}

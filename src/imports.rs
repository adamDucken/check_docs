#[derive(Debug, Clone)]
pub(crate) struct ImportPath {
    pub(crate) crate_name: String,
    pub(crate) segments: Vec<String>,
    pub(crate) item: String,
}

impl ImportPath {
    pub(crate) fn full_path(&self) -> String {
        let mut parts = vec![self.crate_name.clone()];
        parts.extend(self.segments.clone());
        parts.push(self.item.clone());
        parts.join("::")
    }
}

pub(crate) fn parse_use_line(line: &str) -> Result<ImportPath, String> {
    let mut text = line.trim().to_string();
    if !text.starts_with("use ") {
        text = format!("use {text}");
    }
    if !text.trim_end().ends_with(';') {
        text.push(';');
    }

    let item_use: syn::ItemUse =
        syn::parse_str(&text).map_err(|err| format!("failed to parse use line with syn: {err}"))?;

    if item_use.leading_colon.is_some() {
        return Err("absolute use paths with leading `::` are not supported".to_string());
    }

    let mut paths = Vec::new();
    collect_use_paths(&item_use.tree, Vec::new(), &mut paths)?;

    let mut paths = paths.into_iter();
    let parts = paths
        .next()
        .ok_or_else(|| format!("expected external use path, got '{line}'"))?;
    if paths.next().is_some() {
        return Err("brace imports must contain one item for now".to_string());
    }

    if parts.len() < 2 {
        return Err(format!("expected external use path, got '{line}'"));
    }
    if matches!(parts[0].as_str(), "crate" | "self" | "super") {
        return Err("only external crate use paths are supported".to_string());
    }

    Ok(ImportPath {
        crate_name: parts[0].replace('-', "_"),
        segments: parts[1..parts.len() - 1].to_vec(),
        item: parts.last().unwrap().clone(),
    })
}

pub(crate) fn collect_use_paths(
    tree: &syn::UseTree,
    mut prefix: Vec<String>,
    paths: &mut Vec<Vec<String>>,
) -> Result<(), String> {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_paths(&path.tree, prefix, paths)
        }
        syn::UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            paths.push(prefix);
            Ok(())
        }
        syn::UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            paths.push(prefix);
            Ok(())
        }
        syn::UseTree::Glob(_) => Err("glob imports are not supported".to_string()),
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_use_paths(item, prefix.clone(), paths)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_use() {
        let import = parse_use_line("use syn::ItemUse;").unwrap();
        assert_eq!(import.crate_name, "syn");
        assert_eq!(import.segments, Vec::<String>::new());
        assert_eq!(import.item, "ItemUse");
    }

    #[test]
    fn parses_alias_without_use_or_semicolon() {
        let import = parse_use_line("syn::ItemUse as IU").unwrap();
        assert_eq!(import.crate_name, "syn");
        assert_eq!(import.item, "ItemUse");
    }

    #[test]
    fn parses_nested_single_brace_use() {
        let import = parse_use_line("use syn::{item::UseTree};").unwrap();
        assert_eq!(import.crate_name, "syn");
        assert_eq!(import.segments, vec!["item"]);
        assert_eq!(import.item, "UseTree");
    }

    #[test]
    fn rejects_multi_item_brace_use() {
        let err = parse_use_line("use syn::{ItemUse, UseTree};").unwrap_err();
        assert_eq!(err, "brace imports must contain one item for now");
    }

    #[test]
    fn rejects_glob_use() {
        let err = parse_use_line("use syn::*;").unwrap_err();
        assert_eq!(err, "glob imports are not supported");
    }

    #[test]
    fn rejects_absolute_and_too_short_paths() {
        let err = parse_use_line("use ::syn::ItemUse;").unwrap_err();
        assert_eq!(err, "absolute use paths with leading `::` are not supported");

        let err = parse_use_line("use syn;").unwrap_err();
        assert!(err.contains("expected external use path"));

        let err = parse_use_line("use self::Thing;").unwrap_err();
        assert_eq!(err, "only external crate use paths are supported");
    }

    #[test]
    fn full_path_joins_all_segments() {
        let import = ImportPath {
            crate_name: "tokio".into(),
            segments: vec!["sync".into(), "mpsc".into()],
            item: "Sender".into(),
        };
        assert_eq!(import.full_path(), "tokio::sync::mpsc::Sender");
    }
}

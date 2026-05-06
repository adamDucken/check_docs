#[derive(Debug, Clone)]
pub(crate) struct ImportPath {
    pub(crate) crate_name: String,
    pub(crate) segments: Vec<String>,
    pub(crate) item: String,
}

#[cfg(test)]
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
    if let Some(rest) = text.strip_prefix("use ") {
        text = rest.trim().to_string();
    }
    if let Some(rest) = text.strip_suffix(';') {
        text = rest.trim().to_string();
    }
    if let Some((before_as, _)) = text.rsplit_once(" as ") {
        text = before_as.trim().to_string();
    }
    if text.starts_with("::") {
        return Err("absolute use paths with leading `::` are not supported".to_string());
    }
    text = expand_single_brace_path(&text)?;
    if text.contains('*') {
        return Err("glob imports are not supported".to_string());
    }

    let parts: Vec<String> = text
        .split("::")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if parts.len() < 2 {
        return Err(format!("expected external use path, got '{line}'"));
    }
    if matches!(parts[0].as_str(), "crate" | "self" | "super") {
        return Err("only external crate use paths are supported".to_string());
    }

    Ok(ImportPath {
        crate_name: parts[0].replace('-', "_"),
        segments: parts[1..parts.len() - 1].to_vec(),
        item: parts.last().expect("parts has len >= 2").clone(),
    })
}

fn expand_single_brace_path(text: &str) -> Result<String, String> {
    let Some(open) = text.find('{') else {
        return Ok(text.to_string());
    };
    let Some(close) = text.rfind('}') else {
        return Err(format!("expected external use path, got '{text}'"));
    };
    let prefix = text[..open].trim_end_matches("::").trim();
    let inner = text[open + 1..close].trim();
    if inner.contains(',') {
        return Err("brace imports must contain one item for now".to_string());
    }
    if inner.contains('{') || text[close + 1..].trim().len() != 0 {
        return Err("brace imports must contain one item for now".to_string());
    }
    Ok(format!("{prefix}::{inner}"))
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
        assert_eq!(
            err,
            "absolute use paths with leading `::` are not supported"
        );

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

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct FeatureSelection {
    pub(crate) features: Vec<String>,
    pub(crate) all_features: bool,
    pub(crate) no_default_features: bool,
}

impl FeatureSelection {
    pub(crate) fn cargo_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.features.is_empty() {
            args.push("--features".to_string());
            args.push(self.features.join(","));
        }
        if self.all_features {
            args.push("--all-features".to_string());
        }
        if self.no_default_features {
            args.push("--no-default-features".to_string());
        }
        args
    }

    pub(crate) fn is_default(&self) -> bool {
        self.features.is_empty() && !self.all_features && !self.no_default_features
    }

    pub(crate) fn label(&self) -> String {
        if self.is_default() {
            "default".to_string()
        } else {
            self.cargo_args().join(" ")
        }
    }
}

#[derive(Debug)]
pub(crate) struct Args {
    pub(crate) root: PathBuf,
    pub(crate) use_line: String,
    pub(crate) package: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) include_dev: bool,
    pub(crate) include_build: bool,
    pub(crate) feature_selection: FeatureSelection,
}

#[derive(Debug)]
pub(crate) enum ParsedCommand {
    Run(Args),
    Help,
}

pub(crate) fn parse_command() -> Result<ParsedCommand, String> {
    parse_command_from(env::args_os().skip(1))
}

fn parse_command_from(args: impl IntoIterator<Item = OsString>) -> Result<ParsedCommand, String> {
    let mut root = PathBuf::from(".");
    let mut package = None;
    let mut target = None;
    let mut include_dev = false;
    let mut include_build = false;
    let mut feature_selection = FeatureSelection::default();
    let mut use_line = None;
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--root") => {
                let Some(value) = args.next() else {
                    return Err("--root requires PATH".to_string());
                };
                root = PathBuf::from(value);
            }
            Some("--target") => {
                let Some(value) = args.next() else {
                    return Err("--target requires TRIPLE".to_string());
                };
                target = Some(unicode_value(value, "--target value")?);
            }
            Some("--package" | "-p") => {
                let Some(value) = args.next() else {
                    return Err("--package requires NAME_OR_ID".to_string());
                };
                package = Some(unicode_value(value, "--package value")?);
            }
            Some("--features") => {
                let Some(value) = args.next() else {
                    return Err("--features requires FEATURES".to_string());
                };
                let value = unicode_value(value, "--features value")?;
                let features = value
                    .split([',', ' '])
                    .filter(|feature| !feature.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                if features.is_empty() {
                    return Err("--features requires at least one feature".to_string());
                }
                feature_selection.features.extend(features);
            }
            Some("--all-features") => feature_selection.all_features = true,
            Some("--no-default-features") => feature_selection.no_default_features = true,
            Some("--include-dev") => include_dev = true,
            Some("--include-build") => include_build = true,
            Some("-h" | "--help") => return Ok(ParsedCommand::Help),
            Some(option) if option.starts_with('-') => {
                return Err(format!("unknown argument: {option}\n{}", usage()));
            }
            Some(value) if use_line.is_none() => use_line = Some(value.to_string()),
            Some(other) => return Err(format!("unknown argument: {other}\n{}", usage())),
            None if starts_with_dash(&arg) => {
                return Err(format!(
                    "unknown non-Unicode argument: {}\n{}",
                    arg.to_string_lossy(),
                    usage()
                ));
            }
            None if use_line.is_none() => {
                return Err("use expression must be valid Unicode".to_string());
            }
            None => return Err("unexpected non-Unicode argument".to_string()),
        }
    }

    feature_selection.features.sort();
    feature_selection.features.dedup();
    let use_line = use_line.ok_or_else(usage)?;
    Ok(ParsedCommand::Run(Args {
        root,
        use_line,
        package,
        target,
        include_dev,
        include_build,
        feature_selection,
    }))
}

fn unicode_value(value: OsString, label: &str) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| format!("{label} must be valid Unicode"))
}

fn starts_with_dash(value: &OsStr) -> bool {
    value.to_string_lossy().starts_with('-')
}

pub(crate) fn usage() -> String {
    "usage: check-docs '<use crate_name::module::item;>' [--root PATH] [--package NAME_OR_ID] [--target TRIPLE] [--features FEATURES] [--all-features] [--no-default-features] [--include-dev] [--include-build]\n  --include-dev    include direct dev-dependencies (excluded by default)\n  --include-build  include direct build-dependencies (excluded by default)".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    fn args(values: &[&str]) -> Result<Args, String> {
        match parse_command_from(values.iter().map(OsString::from))? {
            ParsedCommand::Run(args) => Ok(args),
            ParsedCommand::Help => Err("expected run command".to_string()),
        }
    }

    #[test]
    fn parses_target_and_dependency_context_flags() {
        let parsed = args(&[
            "use serde::Serialize;",
            "--root",
            "/tmp/project",
            "--package",
            "member_a",
            "--target",
            "wasm32-unknown-unknown",
            "--include-dev",
            "--include-build",
        ])
        .unwrap();

        assert_eq!(parsed.root, PathBuf::from("/tmp/project"));
        assert_eq!(parsed.use_line, "use serde::Serialize;");
        assert_eq!(parsed.package.as_deref(), Some("member_a"));
        assert_eq!(parsed.target.as_deref(), Some("wasm32-unknown-unknown"));
        assert!(parsed.include_dev);
        assert!(parsed.include_build);
    }

    #[test]
    fn parses_package_short_flag() {
        let parsed = args(&["use serde::Serialize;", "-p", "member_a"]).unwrap();

        assert_eq!(parsed.package.as_deref(), Some("member_a"));
    }

    #[test]
    fn parses_help_without_exiting() {
        assert!(matches!(
            parse_command_from([OsString::from("--help")]).unwrap(),
            ParsedCommand::Help
        ));
        assert!(matches!(
            parse_command_from([OsString::from("-h")]).unwrap(),
            ParsedCommand::Help
        ));
    }

    #[test]
    fn reports_missing_target_value() {
        assert!(
            args(&["use serde::Serialize;", "--target"])
                .unwrap_err()
                .contains("--target requires TRIPLE")
        );
    }

    #[test]
    fn reports_missing_package_value() {
        assert!(
            args(&["use serde::Serialize;", "--package"])
                .unwrap_err()
                .contains("--package requires NAME_OR_ID")
        );
        assert!(
            args(&["use serde::Serialize;", "-p"])
                .unwrap_err()
                .contains("--package requires NAME_OR_ID")
        );
    }

    #[test]
    fn reports_unknown_option_before_use_line() {
        let error = args(&["--bad"]).unwrap_err();

        assert_eq!(error, format!("unknown argument: --bad\n{}", usage()));
    }

    #[test]
    fn normalizes_cargo_feature_selection() {
        let parsed = args(&[
            "use serde::Serialize;",
            "--features",
            "z,a b",
            "--features",
            "a",
            "--all-features",
            "--no-default-features",
        ])
        .unwrap();

        assert_eq!(parsed.feature_selection.features, ["a", "b", "z"]);
        assert_eq!(
            parsed.feature_selection.cargo_args(),
            [
                "--features",
                "a,b,z",
                "--all-features",
                "--no-default-features"
            ]
        );
        assert_eq!(
            parsed.feature_selection.label(),
            "--features a,b,z --all-features --no-default-features"
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepts_non_unicode_root_but_rejects_non_unicode_semantic_values() {
        let root = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
        let parsed = parse_command_from([
            OsString::from("use serde::Serialize;"),
            OsString::from("--root"),
            root.clone(),
        ])
        .unwrap();
        let ParsedCommand::Run(parsed) = parsed else {
            panic!("expected run command");
        };
        assert_eq!(parsed.root, PathBuf::from(root));

        let error = parse_command_from([
            OsString::from("--target"),
            OsString::from_vec(vec![0xff]),
            OsString::from("use serde::Serialize;"),
        ])
        .unwrap_err();
        assert_eq!(error, "--target value must be valid Unicode");
    }
}

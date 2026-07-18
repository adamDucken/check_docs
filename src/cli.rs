use std::env;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct Args {
    pub(crate) root: PathBuf,
    pub(crate) use_line: String,
    pub(crate) package: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) include_dev: bool,
    pub(crate) include_build: bool,
}

#[derive(Debug)]
pub(crate) enum ParsedCommand {
    Run(Args),
    Help,
}

pub(crate) fn parse_command() -> Result<ParsedCommand, String> {
    parse_command_from(env::args().skip(1))
}

fn parse_command_from(args: impl IntoIterator<Item = String>) -> Result<ParsedCommand, String> {
    let mut root = PathBuf::from(".");
    let mut package = None;
    let mut target = None;
    let mut include_dev = false;
    let mut include_build = false;
    let mut use_line = None;
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                let Some(value) = args.next() else {
                    return Err("--root requires PATH".to_string());
                };
                root = PathBuf::from(value);
            }
            "--target" => {
                let Some(value) = args.next() else {
                    return Err("--target requires TRIPLE".to_string());
                };
                target = Some(value);
            }
            "--package" | "-p" => {
                let Some(value) = args.next() else {
                    return Err("--package requires NAME_OR_ID".to_string());
                };
                package = Some(value);
            }
            "--include-dev" => include_dev = true,
            "--include-build" => include_build = true,
            "-h" | "--help" => return Ok(ParsedCommand::Help),
            option if option.starts_with('-') => {
                return Err(format!("unknown argument: {option}\n{}", usage()));
            }
            value if use_line.is_none() => use_line = Some(value.to_string()),
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
    }

    let use_line = use_line.ok_or_else(usage)?;
    Ok(ParsedCommand::Run(Args {
        root,
        use_line,
        package,
        target,
        include_dev,
        include_build,
    }))
}

pub(crate) fn usage() -> String {
    "usage: check-docs '<use crate_name::module::item;>' [--root PATH] [--package NAME_OR_ID] [--target TRIPLE] [--include-dev] [--include-build]".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Result<Args, String> {
        match parse_command_from(values.iter().map(|value| value.to_string()))? {
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
            parse_command_from(["--help".to_string()]).unwrap(),
            ParsedCommand::Help
        ));
        assert!(matches!(
            parse_command_from(["-h".to_string()]).unwrap(),
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
}

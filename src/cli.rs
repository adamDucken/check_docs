use std::env;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct Args {
    pub(crate) root: PathBuf,
    pub(crate) use_line: String,
}

pub(crate) fn parse_args() -> Result<Args, String> {
    let mut root = PathBuf::from(".");
    let mut use_line = None;
    let mut args = env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                let Some(value) = args.next() else {
                    return Err("--root requires PATH".to_string());
                };
                root = PathBuf::from(value);
            }
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            value if use_line.is_none() => use_line = Some(value.to_string()),
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
    }

    let use_line = use_line.ok_or_else(usage)?;
    Ok(Args { root, use_line })
}

fn usage() -> String {
    "usage: check-docs '<use crate_name::module::item;>' [--root PATH]".to_string()
}

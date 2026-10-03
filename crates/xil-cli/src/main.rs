//! `xil` — unified dispatcher for the pipeline.
//!
//! Behaviour mirrors the Python `xil.py`: `xil <command> [args...]`,
//! `--help` lists the commands, `--version` prints the version, an unknown
//! command exits 2. The `xil-<command>` aliases work through `argv[0]`.

mod cmd;
mod commands;
mod man;
mod mix;
mod sfxgen;
mod tts;
mod workers;

use std::env;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let code = match real_main() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("[ERROR] {err:#}");
            1
        }
    };
    process::exit(code);
}

fn real_main() -> anyhow::Result<i32> {
    let mut argv: Vec<OsString> = env::args_os().collect();
    let program = argv.remove(0);

    // `xil-parse ...` behaves as `xil parse ...`.
    if let Some(alias) = alias_command(&program) {
        argv.insert(0, OsString::from(alias));
    }

    // The Python dispatcher configures logging before it looks at argv, so
    // its own answers (help, version, an unknown command) still create the
    // day's log file. Commands initialise the logger under their own stage.
    let Some(first) = argv.first().map(|a| a.to_string_lossy().into_owned()) else {
        xil_core::log::init("xil");
        print!("{}", commands::help_text());
        return Ok(0);
    };

    match first.as_str() {
        "-h" | "--help" | "help" => {
            xil_core::log::init("xil");
            print!("{}", commands::help_text());
            return Ok(0);
        }
        "-V" | "--version" | "version" => {
            xil_core::log::init("xil");
            println!("{VERSION}");
            return Ok(0);
        }
        // Hidden: regenerate the man pages (default man/man1).
        "--generate-man" => {
            let dir = argv
                .get(1)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "man/man1".into());
            let n = man::generate(&dir)?;
            println!("wrote {n} man pages to {}", dir.display());
            return Ok(0);
        }
        _ => {}
    }

    // What Python's sys.argv[0] would be: "xil-parse" via an alias, else
    // "xil parse" (the dispatcher rewrites argv[0] that way before handing off).
    match alias_command(&program) {
        Some(_) => cmd::set_prog(
            &Path::new(&program)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        ),
        None => cmd::set_prog(&format!("xil {first}")),
    }

    let Some(spec) = commands::find(&first) else {
        xil_core::log::init("xil");
        xil_core::log::error(&format!("Unknown command: {first}"));
        eprint!("{}", commands::help_text());
        return Ok(2);
    };
    let args = &argv[1..];

    // `xil status --toolchain`: which build this is and what the ML workers
    // will run under.
    if spec.name == "status" && args.iter().any(|a| a == "--toolchain") {
        let mut out = std::io::stdout().lock();
        writeln!(out, "rxil {VERSION}")?;
        write!(out, "{}", workers::describe())?;
        writeln!(out, "commands: {}", commands::COMMANDS.len())?;
        return Ok(0);
    }

    (spec.run)(args)
}

/// `Some("parse")` for a program named `xil-parse`, `None` for plain `xil`.
fn alias_command(program: &OsString) -> Option<String> {
    let stem = Path::new(program).file_name()?.to_str()?;
    let stem = stem.strip_suffix(".exe").unwrap_or(stem);
    let rest = stem.strip_prefix("xil-")?;
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_from_program_name() {
        assert_eq!(
            alias_command(&OsString::from("/usr/local/bin/xil-parse")),
            Some("parse".into())
        );
        assert_eq!(
            alias_command(&OsString::from("xil-sfx-match.exe")),
            Some("sfx-match".into())
        );
        assert_eq!(alias_command(&OsString::from("xil")), None);
        assert_eq!(alias_command(&OsString::from("xil-")), None);
        assert_eq!(alias_command(&OsString::from("target/debug/xil")), None);
    }
}

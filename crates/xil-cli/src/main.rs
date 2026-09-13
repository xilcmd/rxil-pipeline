//! `xil` — unified dispatcher for the pipeline.
//!
//! Behaviour mirrors the Python `xil.py`: `xil <command> [args...]`,
//! `--help` lists the commands, `--version` prints the version, an unknown
//! command exits 2. The `xil-<command>` aliases work through `argv[0]`.

mod cmd;
mod commands;
mod delegate;
mod mix;

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

    let Some(first) = argv.first().map(|a| a.to_string_lossy().into_owned()) else {
        print!("{}", commands::help_text());
        return Ok(0);
    };

    match first.as_str() {
        "-h" | "--help" | "help" => {
            print!("{}", commands::help_text());
            return Ok(0);
        }
        "-V" | "--version" | "version" => {
            println!("{VERSION}");
            return Ok(0);
        }
        // Hidden: one native command name per line. The parity harness uses
        // it to know which commands must NOT have been delegated.
        "--native-list" => {
            for c in commands::COMMANDS.iter().filter(|c| c.native.is_some()) {
                println!("{}", c.name);
            }
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
        eprintln!("[ERROR] Unknown command: {first}");
        eprint!("{}", commands::help_text());
        return Ok(2);
    };
    let args = &argv[1..];

    // The one bit of phase-0 native behaviour: report where delegation goes.
    if spec.name == "status" && args.iter().any(|a| a == "--toolchain") {
        let mut out = std::io::stdout().lock();
        writeln!(out, "rxil {VERSION}")?;
        writeln!(out, "{}", delegate::describe())?;
        let native = commands::COMMANDS
            .iter()
            .filter(|c| c.native.is_some())
            .count();
        writeln!(
            out,
            "native commands: {native}/{}",
            commands::COMMANDS.len()
        )?;
        return Ok(0);
    }

    let native = spec.native.filter(|_| !delegate::forced(spec.name));
    // XIL_TRACE_IMPL=1 announces which implementation serves this run, so a
    // test can prove the Rust code actually ran rather than the Python fallback.
    if env::var_os("XIL_TRACE_IMPL").is_some() {
        eprintln!(
            "rxil-impl: {}",
            if native.is_some() {
                "native"
            } else {
                "delegated"
            }
        );
    }
    match native {
        Some(run) => run(args),
        None => delegate::run(spec.name, args),
    }
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

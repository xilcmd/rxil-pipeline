//! Native command implementations, one module per subcommand.
//!
//! Each `run(args)` receives the arguments after the command name — what a
//! Python `main()` sees in `sys.argv[1:]` — and returns the exit code.

// The `///` lines on clap fields ARE the `--help` text, copied verbatim from
// argparse so the parity suite can diff it. Placeholders like `<slug>` and
// `<TAG>` look like HTML to rustdoc; wrapping them in backticks would change
// the help output, so silence that one lint here instead.
#![allow(rustdoc::invalid_html_tags)]

use std::ffi::OsString;
use std::sync::OnceLock;

use clap::Parser;

pub mod assemble;
pub mod cleanup;
pub mod csv_join;
pub mod cues;
pub mod daw;
pub mod db_profile;
pub mod episode_summary;
pub mod gui;
pub mod import;
pub mod init;
pub mod master;
pub mod migrate;
pub mod migrate_workspace;
pub mod mp3_hash;
pub mod parse;
pub mod parsed_csv;
pub mod produce;
pub mod publish;
pub mod regen;
pub mod removal;
pub mod remove_episode;
pub mod remove_show;
pub mod sample;
pub mod scan;
pub mod sfx;
pub mod sfx_csv;
pub mod sfx_hydrate;
pub mod sfx_impact;
pub mod sfx_lib;
pub mod sfx_match;
pub mod sfx_restore;
pub mod splice;
pub mod status;
pub mod stem_compare;
pub mod stem_log;
pub mod stem_verify;
pub mod studio_onboard;
pub mod use_cmd;
pub mod voices;

/// What Python sees as `sys.argv[0]` for this run: `"xil parse"` through the
/// dispatcher, `"xil-parse"` through an alias. Feeds the run banner.
static PROG: OnceLock<String> = OnceLock::new();

pub fn set_prog(prog: &str) {
    let _ = PROG.set(prog.to_string());
}

pub fn prog() -> &'static str {
    PROG.get().map(String::as_str).unwrap_or("xil")
}

/// `" ".join(sys.argv)` — the BEGIN record's `argv=` field.
pub fn argv_line(args: &[OsString]) -> String {
    let mut s = prog().to_string();
    for a in args {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

/// A `sys.exit(<message>)` from inside a command, reported the way Python
/// reports it once the run banner has closed: through the `xil` dispatcher
/// (`_normalize_exit_code`) the message is logged as an error; through an
/// `xil-<command>` entry point the interpreter prints it to stderr. Either
/// way the status is 1.
#[derive(Debug)]
pub struct SysExit(pub String);

impl std::fmt::Display for SysExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SysExit {}

/// Finish a command whose body may have raised [`SysExit`]. Call after the
/// banner guard has been dropped.
pub fn finish(result: anyhow::Result<i32>) -> anyhow::Result<i32> {
    match result {
        Err(e) => match e.downcast::<SysExit>() {
            Ok(SysExit(msg)) => {
                if prog().starts_with("xil ") {
                    xil_core::log::error(&msg);
                } else {
                    eprintln!("{msg}");
                }
                Ok(1)
            }
            Err(other) => Err(other),
        },
        ok => ok,
    }
}

/// Parse `args` with a clap derive type, using `prog` as `argv[0]` so help and
/// usage lines name the command the way argparse does (`xil-use`).
///
/// On a usage error or `--help`, prints what clap would print and returns
/// the exit code to hand back (2 for errors, 0 for help/version), matching
/// argparse.
pub fn parse_or_exit<T: Parser>(prog: &str, args: &[OsString]) -> Result<T, i32> {
    let argv = std::iter::once(OsString::from(prog)).chain(args.iter().cloned());
    match T::try_parse_from(argv) {
        Ok(t) => Ok(t),
        Err(e) => {
            let _ = e.print();
            Err(e.exit_code())
        }
    }
}

/// `data.get(key, "")` — the value when the key is present (even `null`),
/// an empty string when it is absent.
pub fn get_or_empty(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> serde_json::Value {
    map.get(key)
        .cloned()
        .unwrap_or(serde_json::Value::String(String::new()))
}

/// `entry.get(key) or ""` — falsy values (`null`, `""`, `0`, `false`) become
/// an empty string, anything else passes through.
pub fn get_or_blank(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> serde_json::Value {
    use serde_json::Value;
    match map.get(key) {
        Some(v) if truthy(v) => v.clone(),
        _ => Value::String(String::new()),
    }
}

/// `bool(value)` for a JSON value: `null`, `false`, `0`, `""`, `[]` and
/// `{}` are false, everything else true.
/// `str(value)` for a JSON value printed through an f-string.
pub fn py_str(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        other => xil_core::pycsv::cell(other),
    }
}

pub fn truthy(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

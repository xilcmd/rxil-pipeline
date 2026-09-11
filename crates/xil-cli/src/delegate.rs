//! Hand a subcommand to the Python `xil` while it has no Rust implementation.
//!
//! This whole module goes away in the final phase of the port.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Context, Result};

/// Environment variable that names the Python `xil` outright.
pub const PY_BIN_ENV: &str = "XIL_PY_BIN";
/// Comma-separated command names (or `all`) forced through Python even when a
/// Rust implementation exists. The parity harness relies on this.
pub const FORCE_PY_ENV: &str = "XIL_FORCE_PY";
/// Set on the child so a Python log line can be told apart from a direct run.
const DELEGATED_MARK_ENV: &str = "XIL_DELEGATED_FROM";

/// Locate the Python `xil` entry point.
///
/// Order: `$XIL_PY_BIN`, then `$XIL_CODEROOT/venv/bin/xil`, then `xil-py`
/// on `PATH`. `xil-py` rather than `xil` so the search can never find this
/// binary and recurse.
pub fn find_python_xil() -> Result<PathBuf> {
    let mut tried = Vec::new();

    if let Some(p) = env::var_os(PY_BIN_ENV) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        tried.push(format!("${PY_BIN_ENV}={}", p.display()));
    } else {
        tried.push(format!("${PY_BIN_ENV} (unset)"));
    }

    if let Some(root) = env::var_os("XIL_CODEROOT") {
        let p = PathBuf::from(root).join("venv").join("bin").join("xil");
        if p.is_file() {
            return Ok(p);
        }
        tried.push(format!("$XIL_CODEROOT/venv/bin/xil={}", p.display()));
    } else {
        tried.push("$XIL_CODEROOT/venv/bin/xil (XIL_CODEROOT unset)".to_string());
    }

    if let Some(p) = which("xil-py") {
        return Ok(p);
    }
    tried.push("xil-py on PATH".to_string());

    Err(anyhow!(
        "cannot find the Python xil to delegate to; looked at:\n  {}",
        tried.join("\n  ")
    ))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// True when `$XIL_FORCE_PY` names this command (or `all`).
pub fn forced(command: &str) -> bool {
    let Some(raw) = env::var_os(FORCE_PY_ENV) else {
        return false;
    };
    let raw = raw.to_string_lossy();
    raw.split(',')
        .map(str::trim)
        .any(|item| item == "all" || item == command)
}

/// Run `xil <command> args...` under Python with inherited stdio and return
/// its exit code. A child killed by a signal maps to `128 + signal`, the
/// same number a shell would report.
pub fn run(command: &str, args: &[OsString]) -> Result<i32> {
    let py = find_python_xil()?;
    let status = Command::new(&py)
        .arg(command)
        .args(args)
        .env(DELEGATED_MARK_ENV, "rxil")
        .status()
        .with_context(|| format!("failed to start {}", py.display()))?;

    if let Some(code) = status.code() {
        return Ok(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Ok(128 + sig);
        }
    }
    Ok(1)
}

/// Print where delegation would go. Backs `xil status --toolchain`.
pub fn describe() -> String {
    match find_python_xil() {
        Ok(p) => {
            let version = Command::new(&p)
                .arg("--version")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|| "?".to_string());
            format!("python xil: {} (version {version})", p.display())
        }
        Err(e) => format!("python xil: NOT FOUND\n{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_matches_list_and_all() {
        // Each test process gets its own environment, so this is safe under
        // nextest; under plain `cargo test` keep this the only env-mutating test.
        env::set_var(FORCE_PY_ENV, "parse, daw");
        assert!(forced("parse"));
        assert!(forced("daw"));
        assert!(!forced("master"));
        env::set_var(FORCE_PY_ENV, "all");
        assert!(forced("master"));
        env::remove_var(FORCE_PY_ENV);
        assert!(!forced("parse"));
    }
}

//! Where the Python ML worker scripts live.
//!
//! Chatterbox Turbo, Whisper and MMAudio stay Python, each in its own venv;
//! `xil` only speaks to them over JSON lines. Their scripts ship inside the
//! `xil_pipeline` package, so that is the directory to find.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// The `xil_pipeline` package directory — `os.path.dirname(__file__)` of the
/// package that holds `chatterbox_turbo_worker.py` and its siblings.
///
/// Tried in order: `$XIL_CODEROOT/src/xil_pipeline` (a source checkout), then
/// the interpreter beside `$XIL_PY_BIN` (a pip install, as in CI), then
/// `python3` on `PATH`.
pub fn python_package_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(root) = env::var_os("XIL_CODEROOT") {
            let p = PathBuf::from(root).join("src").join("xil_pipeline");
            if p.join("__init__.py").is_file() {
                return Some(p);
            }
        }
        let beside_py_bin = env::var_os("XIL_PY_BIN")
            .map(PathBuf::from)
            .and_then(|p| p.parent().map(|d| d.join("python")));
        beside_py_bin
            .into_iter()
            .chain([PathBuf::from("python3")])
            .find_map(|python| ask(&python))
    })
    .clone()
}

fn ask(python: &std::path::Path) -> Option<PathBuf> {
    let out = Command::new(python)
        .args([
            "-c",
            "import os, xil_pipeline; print(os.path.dirname(xil_pipeline.__file__))",
        ])
        .output()
        .ok()?;
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !dir.is_empty()).then(|| PathBuf::from(dir))
}

/// The `xil status --toolchain` report: this build, and what the workers
/// will run under.
pub fn describe() -> String {
    let mut out = String::new();
    match python_package_dir() {
        Some(d) => out.push_str(&format!("worker scripts: {}\n", d.display())),
        None => out.push_str(
            "worker scripts: NOT FOUND (set XIL_CODEROOT to the xil-pipeline checkout)\n",
        ),
    }
    let package = python_package_dir();
    for venv in ["venv-chatterbox", "venv-whisper", "venv-mmaudio"] {
        let python = xil_core::workspace::resolve_venv_python(venv, None, package.as_deref());
        out.push_str(&format!(
            "{venv}: {}\n",
            python.as_deref().unwrap_or("not found")
        ));
    }
    out
}

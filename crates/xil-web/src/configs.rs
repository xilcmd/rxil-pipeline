//! Config file listing, the workspace path boundary, and JSON load/save for
//! the Project, Speakers, Cast Config and SFX Config tabs.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use xil_core::fsutil::{basename, glob_children};
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::{active_show, show_slug, workspace_root};

use crate::activity;
use crate::episodes::{child_dirs, is_legacy_cast_name};

static LEGACY_SFX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^sfx_(.+?)_([A-Z0-9]+)\.json$").unwrap());

fn strings(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// `_find_speakers_configs()`.
pub fn find_speakers_configs() -> Vec<String> {
    let root = workspace_root();
    let mut v: Vec<PathBuf> = child_dirs(&root.join("configs"))
        .into_iter()
        .map(|d| d.join("speakers.json"))
        .filter(|p| p.is_file())
        .collect();
    let legacy = root.join("speakers.json");
    if legacy.exists() {
        v.push(legacy);
    }
    strings(v)
}

/// `_find_cast_configs()`.
pub fn find_cast_configs() -> Vec<String> {
    let root = workspace_root();
    let mut v = Vec::new();
    for d in child_dirs(&root.join("configs")) {
        v.extend(glob_children(&d, "cast_", ".json"));
    }
    v.sort();
    v.extend(
        glob_children(&root, "cast_", ".json")
            .into_iter()
            .filter(|p| is_legacy_cast_name(&basename(p))),
    );
    strings(v)
}

/// `_find_sfx_configs()`.
pub fn find_sfx_configs() -> Vec<String> {
    let root = workspace_root();
    let mut v = Vec::new();
    for d in child_dirs(&root.join("configs")) {
        v.extend(glob_children(&d, "sfx_", ".json"));
    }
    v.sort();
    v.extend(
        glob_children(&root, "sfx_", ".json")
            .into_iter()
            .filter(|p| LEGACY_SFX_RE.is_match(&basename(p))),
    );
    strings(v)
}

/// `Path.resolve()` without `strict`: symlinks followed as far as the path
/// exists, the rest normalised lexically.
pub fn resolve_lenient(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut existing = abs.clone();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = fs::canonicalize(&existing).unwrap_or(existing);
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    let mut norm = PathBuf::new();
    for c in out.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other),
        }
    }
    norm
}

/// `_check_workspace_path(path)`: `Err` when `path` resolves outside the
/// workspace root.
pub fn check_workspace_path(path: &Path) -> Result<(), String> {
    let workspace = resolve_lenient(&workspace_root());
    if resolve_lenient(path).starts_with(&workspace) {
        Ok(())
    } else {
        Err(format!(
            "Path is outside the workspace root: {}",
            xil_core::script::hints::py_repr(&path.to_string_lossy())
        ))
    }
}

/// `_load_config_file(path, label)`: the file's text for an editor, or a
/// `//` comment explaining why not.
pub fn load_config_file(path: &str, label: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    if let Err(e) = check_workspace_path(Path::new(path)) {
        return format!("// {e}");
    }
    activity::log(&format!("SELECT {label} → {path}"));
    if !Path::new(path).exists() {
        return format!("// File not found: {path}");
    }
    fs::read_to_string(path).unwrap_or_else(|e| format!("// {e}"))
}

/// `json.dump(data, f, indent=2)` plus a newline.
fn write_json(path: &Path, data: &Value) -> std::io::Result<()> {
    fs::write(path, dumps(data, Style::INDENT2) + "\n")
}

/// `save_cast_config` / `save_speakers_config` / `save_sfx_config`: validate,
/// pretty-print and write. `what` names the file in the activity log.
pub fn save_config_file(path: &str, text: &str, what: &str) -> String {
    if path.is_empty() {
        return "No file selected.".into();
    }
    let p = Path::new(path);
    if let Err(e) = check_workspace_path(p) {
        return e;
    }
    let data: Value = match serde_json::from_str(text) {
        Ok(d) => d,
        Err(e) => return format!("Invalid JSON — not saved: {e}"),
    };
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Err(e) = fs::create_dir_all(dir) {
            return e.to_string();
        }
    }
    if let Err(e) = write_json(p, &data) {
        return e.to_string();
    }
    activity::log(&format!("SAVE {what} → {path}"));
    format!("Saved {path}")
}

/// `_get_project_json_path()`: the active show's project.json when it exists,
/// else the legacy root one.
pub fn project_json_path() -> PathBuf {
    let root = workspace_root();
    if let Some(slug) = active_show().filter(|s| !s.is_empty()) {
        let candidate = root.join("configs").join(slug).join("project.json");
        if candidate.exists() {
            return candidate;
        }
    }
    root.join("project.json")
}

/// `load_project_json()` → `(content, path)`.
pub fn load_project_json() -> (String, PathBuf) {
    let path = project_json_path();
    match fs::read_to_string(&path) {
        Ok(s) => (s, path),
        Err(_) => ("{\n  \"show\": \"\",\n  \"season\": 1\n}".into(), path),
    }
}

/// `save_project_json(text)`.
pub fn save_project_json(text: &str) -> String {
    let data: Value = match serde_json::from_str(text) {
        Ok(d) => d,
        Err(e) => return format!("Invalid JSON — not saved: {e}"),
    };
    let path = project_json_path();
    if let Err(e) = write_json(&path, &data) {
        return e.to_string();
    }
    activity::log(&format!("SAVE project.json → {}", path.display()));
    format!("Saved {}", path.display())
}

/// `_list_available_shows()`: show names from `configs/*/project.json`.
pub fn list_available_shows() -> Vec<String> {
    let configs = workspace_root().join("configs");
    let mut names: Vec<String> = fs::read_dir(&configs)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            let pj = configs.join(&name).join("project.json");
            if !configs.join(&name).is_dir() || !pj.exists() {
                return None;
            }
            let show = fs::read_to_string(&pj)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .map(|d| match d.get("show") {
                    Some(Value::String(s)) => s.clone(),
                    _ => name.clone(),
                });
            Some(show.unwrap_or(name))
        })
        .collect()
}

/// The show whose slug is the active one, for the Setup dropdown.
pub fn active_show_name(shows: &[String]) -> Option<String> {
    let active = active_show()?;
    shows.iter().find(|n| show_slug(n) == active).cloned()
}

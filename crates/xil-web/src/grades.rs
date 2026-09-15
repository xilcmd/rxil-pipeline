//! The Audio Grading tab: SFX library grades kept in each MP3's
//! `TXXX:XIL_GRADE` frame, with a persisted `(size, mtime)` cache so a rescan
//! of a NAS library is one walk plus stats.
//!
//! The cache file (`SFX/.xil_grade_cache.json`) has the Python GUI's exact
//! format, so both dashboards share it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{Map, Value};
use xil_audio::tags::{read_sfx_grade, write_sfx_grade, SFX_GRADE_ACCURATE, SFX_GRADE_REJECTED};
use xil_core::fsutil::{basename, glob_recursive, relpath};
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::workspace_root;

use crate::AppState;

const CACHE_VERSION: i64 = 1;

pub fn is_grade(g: &str) -> bool {
    g == SFX_GRADE_ACCURATE || g == SFX_GRADE_REJECTED
}

/// `_GRADE_GLYPH`.
pub fn glyph(grade: &str) -> &'static str {
    match grade {
        SFX_GRADE_ACCURATE => "✓",
        SFX_GRADE_REJECTED => "✗",
        _ => "•",
    }
}

/// `_sfx_dir(slug)`: the SFX library, scoped to `SFX/{slug}` when it exists.
pub fn sfx_dir(slug: &str) -> PathBuf {
    let root = workspace_root();
    if !slug.is_empty() {
        let per_show = root.join("SFX").join(slug);
        if per_show.is_dir() {
            return per_show;
        }
    }
    root.join("SFX")
}

/// `_grade_cache_path()`.
pub fn grade_cache_path() -> PathBuf {
    sfx_dir("").join(".xil_grade_cache.json")
}

/// `_load_grade_cache_file()`: `{rel_path: {grade, size, mtime_ns}}`, empty
/// when missing, corrupt or another version.
pub fn load_grade_cache_file() -> Map<String, Value> {
    let Ok(text) = fs::read_to_string(grade_cache_path()) else {
        return Map::new();
    };
    let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&text) else {
        return Map::new();
    };
    if data.get("version").and_then(Value::as_i64) != Some(CACHE_VERSION) {
        return Map::new();
    }
    match data.get("files") {
        Some(Value::Object(files)) => files.clone(),
        _ => Map::new(),
    }
}

/// `_save_grade_cache_file(files)`. Best-effort.
pub fn save_grade_cache_file(files: &Map<String, Value>) {
    let mut data = Map::new();
    data.insert("version".into(), CACHE_VERSION.into());
    data.insert("files".into(), Value::Object(files.clone()));
    let _ = fs::write(
        grade_cache_path(),
        dumps(&Value::Object(data), Style::INDENT2) + "\n",
    );
}

fn stat(path: &Path) -> Option<(u64, u128)> {
    let m = fs::metadata(path).ok()?;
    let mtime_ns = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((m.len(), mtime_ns))
}

fn record(grade: &str, size: u64, mtime_ns: u128) -> Value {
    let mut r = Map::new();
    r.insert("grade".into(), grade.into());
    r.insert("size".into(), size.into());
    // Nanoseconds since 1970 fit an i64 until 2262.
    r.insert("mtime_ns".into(), (mtime_ns as i64).into());
    Value::Object(r)
}

/// `_scan_sfx_grades()`: rebuild the in-memory grade map for every
/// `SFX/**/*.mp3`, reading ID3 only for files whose size or mtime changed.
pub fn scan_sfx_grades(state: &AppState) {
    let mut cache = state.grade_cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.clear();
    let dir = sfx_dir("");
    if !dir.is_dir() {
        return;
    }
    let persisted = load_grade_cache_file();
    let mut fresh = Map::new();
    let mut changed = false;
    for path in glob_recursive(&dir, "", ".mp3") {
        let Some((size, mtime_ns)) = stat(&path) else {
            continue;
        };
        let rel = relpath(&path, &dir).to_string_lossy().into_owned();
        let hit = persisted.get(&rel).filter(|r| {
            r.get("size").and_then(Value::as_u64) == Some(size)
                && r.get("mtime_ns").and_then(Value::as_i64) == Some(mtime_ns as i64)
        });
        let grade = match hit {
            Some(r) => r
                .get("grade")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            None => {
                changed = true;
                read_sfx_grade(&path)
            }
        };
        fresh.insert(rel, record(&grade, size, mtime_ns));
        cache.insert(path.to_string_lossy().into_owned(), grade);
    }
    let same_keys =
        fresh.len() == persisted.len() && fresh.keys().all(|k| persisted.contains_key(k));
    if changed || !same_keys {
        save_grade_cache_file(&fresh);
    }
}

/// `_update_grade_cache_entry(path, grade)`: store the post-write stat so the
/// next scan stays read-free.
pub fn update_grade_cache_entry(path: &Path, grade: &str) {
    let Some((size, mtime_ns)) = stat(path) else {
        return;
    };
    let rel = relpath(path, &sfx_dir("")).to_string_lossy().into_owned();
    let mut files = load_grade_cache_file();
    files.insert(
        rel,
        record(if is_grade(grade) { grade } else { "" }, size, mtime_ns),
    );
    save_grade_cache_file(&files);
}

/// `_sfx_show_label(path, root)`: the per-show subdirectory, or `""` for the
/// shared pool and anything not under `root`.
pub fn sfx_show_label(path: &Path, root: &Path) -> String {
    let Some(dir) = path.parent() else {
        return String::new();
    };
    let rel = relpath(dir, root);
    let s = rel.to_string_lossy();
    if s.is_empty() || s == "." || s.starts_with("..") {
        return String::new();
    }
    rel.components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `_sfx_choices(grade_filter)`: `[(glyph  [show] name, path)]`.
pub fn sfx_choices(state: &AppState, filter: &str) -> Vec<(String, String)> {
    let root = sfx_dir("");
    let cache = state.grade_cache.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .iter()
        .filter(|(_, g)| match filter {
            "ungraded" => g.is_empty(),
            f if is_grade(f) => g.as_str() == f,
            _ => true,
        })
        .map(|(path, grade)| {
            let p = Path::new(path);
            let show = sfx_show_label(p, &root);
            let name = if show.is_empty() {
                basename(p)
            } else {
                format!("[{show}] {}", basename(p))
            };
            (format!("{}  {name}", glyph(grade)), path.clone())
        })
        .collect()
}

/// `_sfx_summary()`.
pub fn sfx_summary(state: &AppState) -> String {
    let cache = state.grade_cache.lock().unwrap_or_else(|e| e.into_inner());
    let total = cache.len();
    if total == 0 {
        return "No SFX files found (click Load to scan SFX/).".into();
    }
    let acc = cache
        .values()
        .filter(|g| g.as_str() == SFX_GRADE_ACCURATE)
        .count();
    let rej = cache
        .values()
        .filter(|g| g.as_str() == SFX_GRADE_REJECTED)
        .count();
    format!(
        "{total} files — {acc} ✓ accurate · {rej} ✗ rejected · {} • ungraded",
        total - acc - rej
    )
}

/// The grade of one cached path, `""` when ungraded or unknown.
pub fn grade_of(state: &AppState, path: &str) -> String {
    state
        .grade_cache
        .lock()
        .map(|c| c.get(path).cloned().unwrap_or_default())
        .unwrap_or_default()
}

/// Write a grade to the file, the in-memory map and the persisted cache.
/// Only paths the last scan found are accepted.
pub fn apply_grade(state: &AppState, path: &str, grade: &str) -> std::io::Result<()> {
    {
        let cache = state.grade_cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.contains_key(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not a scanned SFX file",
            ));
        }
    }
    write_sfx_grade(Path::new(path), grade)?;
    let g = if is_grade(grade) { grade } else { "" };
    if let Ok(mut cache) = state.grade_cache.lock() {
        cache.insert(path.to_string(), g.to_string());
    }
    update_grade_cache_entry(Path::new(path), grade);
    Ok(())
}

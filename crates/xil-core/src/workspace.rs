//! Workspace root, show slug, active show, and the two on-disk layouts.
//!
//! Port of the path half of `models.py`.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Fallback slug when neither `--show` nor `project.json` names one.
pub const DEFAULT_SLUG: &str = "sample";

/// Per-type production defaults: gap between dialogue stems and a stability
/// hint. `stability` is `None` where Python has `None`.
pub fn type_defaults(kind: &str) -> Option<(u32, Option<f64>)> {
    match kind {
        "podcast" => Some((600, None)),
        "audiobook" => Some((400, Some(0.75))),
        "drama" => Some((800, None)),
        "special" => Some((600, None)),
        _ => None,
    }
}

/// `~` expansion the way `Path.expanduser()` does it: only a leading `~`.
fn expanduser(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            if let Some(home) = env::var_os("HOME") {
                return PathBuf::from(home).join(rest.trim_start_matches('/'));
            }
        }
    }
    PathBuf::from(p)
}

/// `Path.resolve()` — absolute, symlinks followed when the path exists.
fn resolve(p: PathBuf) -> PathBuf {
    match fs::canonicalize(&p) {
        Ok(c) => c,
        Err(_) => std::path::absolute(&p).unwrap_or(p),
    }
}

/// The active workspace root: `$XIL_PROJECTROOT` (expanded, resolved), else
/// the current directory.
pub fn workspace_root() -> PathBuf {
    match env::var("XIL_PROJECTROOT") {
        Ok(v) if !v.is_empty() => resolve(expanduser(&v)),
        _ => env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// `$XIL_CODEROOT` — where the software and the heavy model venvs live.
pub fn code_root() -> Option<PathBuf> {
    match env::var("XIL_CODEROOT") {
        Ok(v) if !v.is_empty() => Some(resolve(expanduser(&v))),
        _ => None,
    }
}

/// Slug from `.active_show`, or `None` when the file is absent.
pub fn active_show() -> Option<String> {
    let f = workspace_root().join(".active_show");
    fs::read_to_string(f).ok().map(|s| s.trim().to_string())
}

/// Write `slug` to `.active_show` in the workspace root (no trailing newline,
/// same as Python's `write_text`).
pub fn set_active_show(slug: &str) -> io::Result<()> {
    fs::write(workspace_root().join(".active_show"), slug)
}

/// Show title → filesystem-safe slug: lowercase, keep only `[a-z0-9]`.
///
/// Python lowercases with `str.lower()`, which is Unicode-aware; the filter
/// then drops anything outside ASCII alphanumerics, so the result is the
/// same either way.
pub fn show_slug(show_name: &str) -> String {
    show_name
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// Every standard pipeline path for one episode, keyed by the same logical
/// names as the Python dict.
pub type Paths = BTreeMap<&'static str, PathBuf>;

/// Normalized layout (0.1.8+).
fn derive_paths_new(root: &Path, slug: &str, tag: &str) -> Paths {
    let mut p = Paths::new();
    p.insert(
        "cast",
        root.join("configs")
            .join(slug)
            .join(format!("cast_{tag}.json")),
    );
    p.insert(
        "sfx",
        root.join("configs")
            .join(slug)
            .join(format!("sfx_{tag}.json")),
    );
    p.insert(
        "parsed",
        root.join("parsed")
            .join(slug)
            .join(format!("parsed_{tag}.json")),
    );
    p.insert(
        "parsed_csv",
        root.join("parsed")
            .join(slug)
            .join(format!("parsed_{tag}.csv")),
    );
    p.insert(
        "annotated_csv",
        root.join("parsed")
            .join(slug)
            .join(format!("annotated_{tag}.csv")),
    );
    p.insert(
        "master",
        root.join("masters")
            .join(slug)
            .join(format!("{tag}_master.mp3")),
    );
    p.insert(
        "cues",
        root.join("cues").join(slug).join(format!("cues_{tag}.md")),
    );
    p.insert(
        "cues_manifest",
        root.join("cues")
            .join(slug)
            .join(format!("cues_manifest_{tag}.json")),
    );
    p.insert(
        "orig_parsed",
        root.join("parsed")
            .join(slug)
            .join(format!("orig_parsed_{tag}.json")),
    );
    p.insert(
        "revised_script",
        root.join("scripts")
            .join(slug)
            .join(format!("revised_{slug}_{tag}.md")),
    );
    p.insert("stems", root.join("stems").join(slug).join(tag));
    p.insert("daw", root.join("daw").join(slug).join(tag));
    p.insert(
        "posts",
        root.join("posts")
            .join(slug)
            .join(format!("{tag}_posts.md")),
    );
    p
}

/// Legacy layout (pre-0.1.8) — what `xil migrate-workspace` moves away from.
pub fn derive_paths_legacy(slug: &str, tag: &str) -> Paths {
    let root = workspace_root();
    let mut p = Paths::new();
    p.insert("cast", root.join(format!("cast_{slug}_{tag}.json")));
    p.insert("sfx", root.join(format!("sfx_{slug}_{tag}.json")));
    p.insert(
        "parsed",
        root.join("parsed")
            .join(format!("parsed_{slug}_{tag}.json")),
    );
    p.insert(
        "parsed_csv",
        root.join("parsed").join(format!("parsed_{slug}_{tag}.csv")),
    );
    p.insert(
        "annotated_csv",
        root.join("parsed")
            .join(format!("parsed_{slug}_{tag}_annotated.csv")),
    );
    p.insert("master", root.join(format!("{slug}_{tag}_master.mp3")));
    p.insert(
        "cues",
        root.join("cues").join(format!("cues_{slug}_{tag}.md")),
    );
    p.insert(
        "cues_manifest",
        root.join("cues").join(format!("cues_manifest_{tag}.json")),
    );
    p.insert(
        "orig_parsed",
        root.join("parsed")
            .join(format!("orig_parsed_{slug}_{tag}.json")),
    );
    p.insert(
        "revised_script",
        root.join("scripts")
            .join(format!("revised_{slug}_{tag}.md")),
    );
    p.insert("stems", root.join("stems").join(slug).join(tag));
    p.insert("daw", root.join("daw").join(tag));
    p
}

/// Auto-detect the layout: legacy when the cast config exists only at the
/// legacy root location, normalized otherwise.
pub fn derive_paths(slug: &str, tag: &str) -> Paths {
    let new = derive_paths_new(&workspace_root(), slug, tag);
    let legacy = derive_paths_legacy(slug, tag);
    let use_legacy = legacy["cast"].exists() && !new["cast"].exists();
    if use_legacy {
        legacy
    } else {
        new
    }
}

/// Parsed `project.json`, or an empty object when absent.
///
/// A relative `project_path` is anchored at the workspace root. The bare
/// default name additionally honours `.active_show`: when that names a slug
/// with its own `configs/<slug>/project.json`, that file wins.
pub fn read_project(project_path: &str) -> Value {
    let mut path = PathBuf::from(project_path);
    if !path.is_absolute() {
        let root = workspace_root();
        if project_path == "project.json" {
            if let Some(slug) = active_show() {
                let candidate = root.join("configs").join(&slug).join("project.json");
                if candidate.exists() {
                    if let Some(v) = read_json_object(&candidate) {
                        return v;
                    }
                }
            }
        }
        path = root.join(path);
    }
    if path.exists() {
        if let Some(v) = read_json_object(&path) {
            return v;
        }
    }
    Value::Object(Default::default())
}

fn read_json_object(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Content type from `project.json`, defaulting to `"podcast"`.
pub fn resolve_project_type(project_path: &str) -> String {
    read_project(project_path)
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("podcast")
        .to_string()
}

/// Slug from `--show`, else `project.json`'s `show`, else [`DEFAULT_SLUG`].
pub fn resolve_slug(show_arg: Option<&str>, project_path: &str) -> String {
    if let Some(s) = show_arg.filter(|s| !s.is_empty()) {
        return show_slug(s);
    }
    match read_project(project_path).get("show") {
        Some(Value::String(s)) => show_slug(s),
        Some(other) => show_slug(&python_str(other)),
        None => DEFAULT_SLUG.to_string(),
    }
}

/// Season/arc title from an explicit value, else `project.json`, else `None`.
/// An empty string in the file counts as absent (Python: `or None`).
pub fn resolve_season_title(arg: Option<&str>, project_path: &str) -> Option<String> {
    if let Some(a) = arg {
        return Some(a.to_string());
    }
    match read_project(project_path).get("season_title") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Season number from an explicit value, else `project.json`, else `None`.
pub fn resolve_season(arg: Option<i64>, project_path: &str) -> Option<i64> {
    if arg.is_some() {
        return arg;
    }
    match read_project(project_path).get("season") {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `S01E01` when a season is known, `E01` otherwise.
pub fn episode_tag(season: Option<i64>, episode: i64) -> String {
    match season {
        Some(s) => format!("S{s:02}E{episode:02}"),
        None => format!("E{episode:02}"),
    }
}

/// How Python's `str()` would render a scalar JSON value. Only the cases
/// that can plausibly land in a config file are covered.
pub fn python_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_strips_everything_but_ascii_alnum() {
        assert_eq!(show_slug("THE 413"), "the413");
        assert_eq!(show_slug("Night Owls!"), "nightowls");
        assert_eq!(show_slug("Café-Über 9"), "cafber9");
        assert_eq!(show_slug(""), "");
    }

    #[test]
    fn episode_tag_formats() {
        assert_eq!(episode_tag(Some(1), 1), "S01E01");
        assert_eq!(episode_tag(Some(12), 7), "S12E07");
        assert_eq!(episode_tag(None, 3), "E03");
        assert_eq!(episode_tag(Some(100), 100), "S100E100");
    }

    #[test]
    fn new_layout_paths() {
        let p = derive_paths_new(Path::new("/ws"), "the413", "S01E01");
        assert_eq!(p["cast"], Path::new("/ws/configs/the413/cast_S01E01.json"));
        assert_eq!(
            p["parsed"],
            Path::new("/ws/parsed/the413/parsed_S01E01.json")
        );
        assert_eq!(
            p["revised_script"],
            Path::new("/ws/scripts/the413/revised_the413_S01E01.md")
        );
        assert_eq!(p["daw"], Path::new("/ws/daw/the413/S01E01"));
        assert_eq!(p["posts"], Path::new("/ws/posts/the413/S01E01_posts.md"));
        assert_eq!(p.len(), 13);
    }

    // $HOME is a Unix variable; Windows uses USERPROFILE and this would
    // panic on the unwrap. Tilde expansion only matters where xil runs.
    #[test]
    #[cfg(unix)]
    fn expanduser_only_leading_tilde() {
        let home = env::var("HOME").unwrap();
        assert_eq!(expanduser("~/x"), Path::new(&home).join("x"));
        assert_eq!(expanduser("~"), Path::new(&home));
        assert_eq!(expanduser("/a/~/b"), Path::new("/a/~/b"));
        assert_eq!(expanduser("~bob/x"), Path::new("~bob/x"));
    }

    #[test]
    fn python_str_renders_scalars() {
        assert_eq!(python_str(&Value::Null), "None");
        assert_eq!(python_str(&Value::Bool(true)), "True");
        assert_eq!(python_str(&serde_json::json!(3)), "3");
        assert_eq!(python_str(&serde_json::json!("x")), "x");
    }
}

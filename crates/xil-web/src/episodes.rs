//! Episode discovery, dropdown labels, the Episodes table and stem listing.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;
use xil_core::fsutil::{basename, glob_children};
use xil_core::pyfmt::{head, pad_right};
use xil_core::workspace::{derive_paths, workspace_root};

use crate::AppState;

static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^cast_(.+?)_([A-Z0-9]+)\.json$").unwrap());
static NEW_CAST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^cast_([A-Z0-9]+)\.json$").unwrap());
static SLUG_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").unwrap());
static EP_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9]+$").unwrap());
static SEQ_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^n?(-?\d+)_").unwrap());

/// How long Episodes-table rows stay memoised (`_EPISODES_TTL_S`).
pub const EPISODES_TTL: Duration = Duration::from_secs(300);

/// Legacy cast file names: `cast_{slug}_{tag}.json` in the workspace root.
pub fn is_legacy_cast_name(name: &str) -> bool {
    TAG_RE.is_match(name)
}

/// Subdirectories of `dir`, sorted, hidden ones skipped (`glob("dir/*")`).
pub(crate) fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir() && !basename(p).starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// `_find_episodes()`: `(slug, tag)` pairs from both cast layouts, plus
/// `(slug, "")` stubs for shows with a project.json and no episode yet.
pub fn find_episodes() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut seen = HashSet::new();
    let mut results = Vec::new();

    for p in glob_children(&root, "cast_", ".json") {
        if let Some(c) = TAG_RE.captures(&basename(&p)) {
            let pair = (c[1].to_string(), c[2].to_string());
            if seen.insert(pair.clone()) {
                results.push(pair);
            }
        }
    }
    for dir in child_dirs(&root.join("configs")) {
        let slug = basename(&dir);
        for p in glob_children(&dir, "cast_", ".json") {
            if let Some(c) = NEW_CAST_RE.captures(&basename(&p)) {
                let pair = (slug.clone(), c[1].to_string());
                if seen.insert(pair.clone()) {
                    results.push(pair);
                }
            }
        }
    }

    let with_episodes: HashSet<String> = results.iter().map(|(s, _)| s.clone()).collect();
    let configs = root.join("configs");
    if configs.is_dir() {
        let mut names: Vec<String> = fs::read_dir(&configs)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        for name in names {
            if !with_episodes.contains(&name) && configs.join(&name).join("project.json").exists() {
                results.push((name, String::new()));
            }
        }
    }
    results.sort();
    results
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `_ep_meta(slug, tag)` → `(title, season_title)` from the cast config.
pub fn ep_meta(slug: &str, tag: &str) -> (String, String) {
    let root = workspace_root();
    for path in [
        root.join("configs")
            .join(slug)
            .join(format!("cast_{tag}.json")),
        root.join(format!("cast_{slug}_{tag}.json")),
    ] {
        if path.exists() {
            if let Some(data) = read_json(&path) {
                return (str_field(&data, "title"), str_field(&data, "season_title"));
            }
        }
    }
    (String::new(), String::new())
}

/// `_ep_choice(slug, tag)`: the dropdown label.
pub fn ep_choice(slug: &str, tag: &str) -> String {
    if tag.is_empty() {
        let pj = workspace_root()
            .join("configs")
            .join(slug)
            .join("project.json");
        let show = read_json(&pj)
            .and_then(|d| d.get("show").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| slug.to_string());
        return format!("{slug}  [show]  —  {show}");
    }
    let (title, season_title) = ep_meta(slug, tag);
    let mut label = format!("{slug}  {tag}");
    if !season_title.is_empty() {
        label.push_str(&format!("  [{season_title}]"));
    }
    if !title.is_empty() {
        label.push_str(&format!("  —  {title}"));
    }
    label
}

/// `_episode_choices()`.
pub fn episode_choices() -> Vec<String> {
    find_episodes()
        .iter()
        .map(|(s, t)| ep_choice(s, t))
        .collect()
}

/// `_is_safe_slug_or_tag(value)`: a bare path component, checked before any
/// path is built from request data.
pub fn is_safe_slug_or_tag(value: &str) -> bool {
    SLUG_TAG_RE.is_match(value)
}

/// `True` for a raw episode tag such as `S04E04`.
pub fn looks_like_tag(value: &str) -> bool {
    EP_TAG_RE.is_match(value)
}

/// `_parse_choice(choice)`: a dropdown label back to `(slug, tag)`. An unsafe
/// slug yields `("", "")`.
pub fn parse_choice(choice: &str) -> (String, String) {
    let parts: Vec<&str> = choice.split_whitespace().collect();
    let slug = parts.first().copied().unwrap_or("");
    if !slug.is_empty() && !is_safe_slug_or_tag(slug) {
        return (String::new(), String::new());
    }
    match parts.get(1) {
        Some(t) if looks_like_tag(t) => (slug.to_string(), t.to_string()),
        _ => (slug.to_string(), String::new()),
    }
}

/// `_refresh_episodes(force)`: Episodes-table rows, memoised for
/// [`EPISODES_TTL`] per workspace root. The staleness scan stats every stem,
/// so rows are evaluated on up to eight threads.
pub fn refresh_episodes(state: &AppState, force: bool) -> Vec<Vec<String>> {
    let root = workspace_root();
    if !force {
        if let Ok(cache) = state.episodes_cache.lock() {
            if let Some((at, rows)) = cache.get(&root) {
                if at.elapsed() < EPISODES_TTL {
                    return rows.clone();
                }
            }
        }
    }
    let episodes = find_episodes();
    let eval = |(slug, tag): &(String, String)| -> Vec<String> {
        let st = (state.status)(slug, tag);
        let (title, season_title) = ep_meta(slug, tag);
        let desc = match (season_title.is_empty(), title.is_empty()) {
            (true, _) => title,
            (false, false) => format!("[{season_title}]  —  {title}"),
            (false, true) => format!("[{season_title}]"),
        };
        vec![
            tag.clone(),
            slug.clone(),
            desc,
            st.parse,
            st.produce,
            st.daw,
            st.master,
            st.overall,
        ]
    };
    let mut rows: Vec<Option<Vec<String>>> = vec![None; episodes.len()];
    if !episodes.is_empty() {
        let workers = episodes.len().min(8);
        let chunk = episodes.len().div_ceil(workers);
        std::thread::scope(|s| {
            for (eps, out) in episodes.chunks(chunk).zip(rows.chunks_mut(chunk)) {
                s.spawn(move || {
                    for (e, slot) in eps.iter().zip(out.iter_mut()) {
                        *slot = Some(eval(e));
                    }
                });
            }
        });
    }
    let rows: Vec<Vec<String>> = rows.into_iter().flatten().collect();
    if let Ok(mut cache) = state.episodes_cache.lock() {
        cache.insert(root, (Instant::now(), rows.clone()));
    }
    rows
}

/// `_load_stems(slug, tag, filter_type)`: `[(label, path)]` for the stem
/// dropdown, sorted by file name, labelled from the parsed script.
pub fn load_stems(slug: &str, tag: &str, filter: &str) -> Vec<(String, PathBuf)> {
    let p = derive_paths(slug, tag);
    let stems_dir = &p["stems"];
    if !stems_dir.is_dir() {
        return Vec::new();
    }
    let mut index: std::collections::HashMap<i64, Value> = std::collections::HashMap::new();
    if let Some(data) = read_json(&p["parsed"]) {
        for e in data
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let seq = e.get("seq").and_then(Value::as_i64).unwrap_or(-99999);
            index.insert(seq, e.clone());
        }
    }
    let mut out = Vec::new();
    for path in glob_children(stems_dir, "", ".mp3") {
        let name = basename(&path);
        let stem = name.strip_suffix(".mp3").unwrap_or(&name).to_string();
        let seq = SEQ_RE
            .captures(&stem)
            .and_then(|c| c[1].parse::<i64>().ok())
            .unwrap_or(-99999);
        let entry = index.get(&seq);
        let get = |k: &str| {
            entry
                .and_then(|e| e.get(k))
                .and_then(Value::as_str)
                .unwrap_or("")
        };
        let (etype, dtype) = (get("type"), get("direction_type"));
        let keep = match filter {
            "dialogue" => etype == "dialogue",
            "sfx" => dtype == "SFX" || dtype == "BEAT",
            "music" => dtype == "MUSIC",
            "ambience" => dtype == "AMBIENCE",
            _ => true,
        };
        if !keep {
            continue;
        }
        let label = match entry {
            Some(_) => {
                let speaker = [get("speaker"), dtype]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or("?");
                format!(
                    "{seq:4}  {}  {}  {}",
                    pad_right(speaker, 12),
                    pad_right(&head(get("section"), 14), 14),
                    head(get("text"), 52)
                )
            }
            None => stem,
        };
        out.push((label, path));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_choice_reads_labels_and_refuses_unsafe_slugs() {
        assert_eq!(
            parse_choice("the413  S03E03  [Arc]  —  Title"),
            ("the413".into(), "S03E03".into())
        );
        assert_eq!(
            parse_choice("mypodcast  [show]  —  My Podcast"),
            ("mypodcast".into(), "".into())
        );
        assert_eq!(parse_choice(""), ("".into(), "".into()));
        assert_eq!(parse_choice("../etc  S01E01"), ("".into(), "".into()));
        assert_eq!(parse_choice("a/b  S01E01"), ("".into(), "".into()));
    }

    #[test]
    fn safe_slug_allowlist() {
        assert!(is_safe_slug_or_tag("the413"));
        assert!(is_safe_slug_or_tag("S01E01"));
        assert!(is_safe_slug_or_tag("my-show_2"));
        assert!(!is_safe_slug_or_tag(""));
        assert!(!is_safe_slug_or_tag(".."));
        assert!(!is_safe_slug_or_tag("a b"));
        assert!(!is_safe_slug_or_tag("x\0"));
    }
}

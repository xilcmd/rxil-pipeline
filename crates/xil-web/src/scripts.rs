//! The Scripts sub-tab: script listing, header analysis and save.

use std::fs;
use std::sync::LazyLock;

use regex::Regex;
use xil_core::fsutil::{glob_children, relpath, sort_py};
use xil_core::script::parse_script_header;
use xil_core::workspace::{show_slug, workspace_root};

use crate::activity;
use crate::episodes::child_dirs;

static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9]+").unwrap());
static FILENAME_SLUG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[A-Z]\d+[A-Z]\d+_)?([a-z0-9]+)_").unwrap());

/// `_script_choices()`: `scripts/*/*.md` relative to the workspace, else the
/// flat `scripts/*.md` as absolute paths.
pub fn script_choices() -> Vec<String> {
    let root = workspace_root();
    let mut per_show = Vec::new();
    for dir in child_dirs(&root.join("scripts")) {
        per_show.extend(glob_children(&dir, "", ".md"));
    }
    sort_py(&mut per_show);
    if !per_show.is_empty() {
        return per_show
            .iter()
            .map(|p| relpath(p, &root).to_string_lossy().into_owned())
            .collect();
    }
    glob_children(&root.join("scripts"), "", ".md")
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// What Analyze Header fills in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeaderFields {
    pub show: String,
    pub season: String,
    pub episode: String,
    pub title: String,
    pub arc: String,
    /// The suggested file name, or a warning when the header is not recognised.
    pub filename: String,
}

/// `_analyze_script_header(text)`.
pub fn analyze_script_header(text: &str) -> HeaderFields {
    let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return HeaderFields::default();
    };
    activity::log("ANALYZE header");
    let Some(h) = parse_script_header(first) else {
        return HeaderFields {
            filename: "⚠️ Header not recognized — expected: SHOW Season N: Episode N: \"Title\""
                .into(),
            ..HeaderFields::default()
        };
    };
    let slug = show_slug(&h.show);
    let safe_title = NON_ALNUM
        .replace_all(&h.title, "_")
        .trim_matches('_')
        .to_string();
    let season = h
        .season
        .map(|s| format!("{s:02}"))
        .unwrap_or_else(|| "XX".into());
    HeaderFields {
        filename: format!("S{season}E{:02}_{slug}_{safe_title}_v1.md", h.episode),
        show: h.show,
        season: h.season.map(|s| s.to_string()).unwrap_or_default(),
        episode: h.episode.to_string(),
        title: h.title,
        arc: h.season_title.unwrap_or_default(),
    }
}

/// `_save_script_file(text, filename)`: write under `scripts/{slug}/` when
/// that directory exists, else `scripts/`. Never overwrites.
pub fn save_script_file(text: &str, filename: &str) -> String {
    if text.trim().is_empty() {
        return "⚠️ No script content to save.".into();
    }
    let mut filename = filename.trim().to_string();
    if filename.is_empty() {
        return "⚠️ Filename is empty — run Analyze Header first.".into();
    }
    if !filename.ends_with(".md") {
        filename.push_str(".md");
    }
    let root = workspace_root();
    let slug = FILENAME_SLUG
        .captures(&filename)
        .map(|c| c[1].to_string())
        .unwrap_or_default();
    let (dir, rel) = if !slug.is_empty() && root.join("scripts").join(&slug).is_dir() {
        (
            root.join("scripts").join(&slug),
            format!("scripts/{slug}/{filename}"),
        )
    } else {
        (root.join("scripts"), format!("scripts/{filename}"))
    };
    // The name comes from the browser: refuse anything that is not a plain
    // file name before it is joined onto a path.
    if filename.contains('/')
        || filename.contains('\\')
        || filename.contains('\0')
        || filename.starts_with("..")
    {
        return format!("⚠️ Invalid filename: {filename}");
    }
    if let Err(e) = fs::create_dir_all(&dir) {
        return format!("⚠️ {e}");
    }
    let dest = dir.join(&filename);
    if dest.exists() {
        return format!(
            "⚠️ Already exists: {rel} — edit the filename above to save a new version."
        );
    }
    if let Err(e) = fs::write(&dest, text) {
        return format!("⚠️ {e}");
    }
    activity::log(&format!("SAVE script → {rel}"));
    format!("✅ Saved: {rel}")
}

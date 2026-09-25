//! The Edit Parsed JSON tab: load and save `parsed/{slug}/parsed_{tag}.json`
//! for an episode chosen from the usual Episode dropdown.
//!
//! The config editors in [`crate::configs`] key off a file path the user picks
//! from a list; this one derives its path from `(slug, tag)` instead, so it
//! shares the Episode dropdown with Run Stage and Audio Preview.
//!
//! Saving writes the file the way `xil parse` writes it —
//! [`Style::INDENT2_UTF8`] with no trailing newline — rather than the
//! [`Style::INDENT2`] the config editors use, so a hand-edit and a re-parse
//! produce the same bytes and the non-ASCII punctuation in a script (`—`,
//! `“`) is not rewritten as `\uXXXX` escapes.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::derive_paths;

use crate::configs::check_workspace_path;
use crate::{activity, episodes};

/// The parsed JSON path behind an Episode dropdown label, or the reason there
/// is none.
pub fn parsed_path(ep: &str) -> Result<PathBuf, String> {
    let (slug, tag) = episodes::parse_choice(ep);
    if slug.is_empty() {
        return Err("Select an episode first.".into());
    }
    if tag.is_empty() {
        return Err("Select an episode, not a show.".into());
    }
    let path = derive_paths(&slug, &tag)["parsed"].clone();
    check_workspace_path(&path)?;
    Ok(path)
}

/// The file's text for the editor, or a `//` comment explaining why not.
/// Mirrors `configs::load_config_file`.
pub fn load(ep: &str) -> String {
    let path = match parsed_path(ep) {
        Ok(p) => p,
        Err(e) => return format!("// {e}"),
    };
    activity::log(&format!("SELECT parsed → {}", path.display()));
    if !path.exists() {
        return format!("// File not found: {}", path.display());
    }
    fs::read_to_string(&path).unwrap_or_else(|e| format!("// {e}"))
}

/// `{name}.bak` beside the file itself, so `parsed_S01E01.json` backs up to
/// `parsed_S01E01.json.bak`.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// Validate, back up and write. Returns the status line.
pub fn save(ep: &str, text: &str) -> String {
    let path = match parsed_path(ep) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let data: Value = match serde_json::from_str(text) {
        Ok(d) => d,
        Err(e) => return format!("Invalid JSON — not saved: {e}"),
    };
    // A failed backup aborts the save: never overwrite what we could not copy.
    if path.exists() {
        if let Err(e) = fs::copy(&path, backup_path(&path)) {
            return format!("Backup failed — not saved: {e}");
        }
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Err(e) = fs::create_dir_all(dir) {
            return e.to_string();
        }
    }
    if let Err(e) = fs::write(&path, dumps(&data, Style::INDENT2_UTF8)) {
        return e.to_string();
    }
    activity::log(&format!("SAVE parsed → {}", path.display()));
    format!("Saved {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_sits_beside_the_file() {
        assert_eq!(
            backup_path(Path::new("/ws/parsed/the413/parsed_S01E01.json")),
            PathBuf::from("/ws/parsed/the413/parsed_S01E01.json.bak")
        );
    }

    #[test]
    fn a_show_stub_and_an_unsafe_slug_have_no_path() {
        assert_eq!(
            parsed_path("mypodcast  [show]  —  My Podcast"),
            Err("Select an episode, not a show.".into())
        );
        assert_eq!(
            parsed_path("../etc  S01E01"),
            Err("Select an episode first.".into())
        );
        assert_eq!(parsed_path(""), Err("Select an episode first.".into()));
    }
}

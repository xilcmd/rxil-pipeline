//! The Dialogue sub-tab and the Timeline's dialogue-edit modal: read,
//! search and edit one dialogue line's speaker and text in
//! `parsed/{slug}/parsed_{tag}.json`.
//!
//! Both surfaces call [`save_entry`] — there is exactly one place that
//! mutates a dialogue line. Scope is deliberately narrow: only entries with
//! `"type": "dialogue"` can be edited here; sound cues, scene headers and
//! everything else stay the Raw JSON tab's job.

use std::fs;
use std::path::{Path, PathBuf};

use std::collections::HashMap;

use serde_json::{Map, Value};
use xil_core::fsutil::basename;
use xil_core::journal::{append_dialogue_edit, dialogue_edits_path};
use xil_core::pyjson::{dumps, Style};
use xil_core::script::{nfc_normalize, stats_from_entries_json, speakers::load_speakers_registry};
use xil_core::workspace::{derive_paths, workspace_root};

use crate::configs::check_workspace_path;
use crate::episodes::is_safe_slug_or_tag;
use crate::activity;

/// One dialogue row, as surfaced to either editor.
#[derive(Clone, Debug, PartialEq)]
pub struct DialogueRow {
    pub seq: i64,
    pub speaker: String,
    pub text: String,
}

/// `(slug, tag)` → the parsed JSON path, jailed — the slug/tag-keyed sibling
/// of [`crate::parsed::parsed_path`], for callers that already have
/// `(slug, tag)` (the Timeline's JS globals) instead of an Episode-dropdown
/// label.
pub fn path_for(slug: &str, tag: &str) -> Result<PathBuf, String> {
    if !(is_safe_slug_or_tag(slug) && is_safe_slug_or_tag(tag)) {
        return Err("invalid slug or tag".into());
    }
    let path = derive_paths(slug, tag)["parsed"].clone();
    check_workspace_path(&path)?;
    Ok(path)
}

fn load_entries(path: &Path) -> Result<(Map<String, Value>, Vec<Value>), String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let data: Value =
        serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let Value::Object(top) = data else {
        return Err(format!("{} is not a JSON object", path.display()));
    };
    let entries = top
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((top, entries))
}

/// `{name}.bak` beside the file, matching [`crate::parsed::save`]'s scheme.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// `configs/<slug>/speakers.json` (legacy fallback: the workspace root), as
/// dropdown-ready `(label, key)` pairs in the file's own order. Empty when
/// no speakers file exists yet for this show — callers then accept any
/// speaker key rather than blocking the editor on a file that hasn't been
/// authored.
pub fn speaker_choices(slug: &str) -> Vec<(String, String)> {
    let root = workspace_root();
    let candidate = root.join("configs").join(slug).join("speakers.json");
    let path = if candidate.exists() {
        candidate
    } else {
        root.join("speakers.json")
    };
    if !path.exists() {
        return Vec::new();
    }
    load_speakers_registry(Some(&path))
        .into_iter()
        .map(|(key, entry)| {
            let label = entry
                .get("display")
                .and_then(Value::as_str)
                .or_else(|| entry.get("full_name").and_then(Value::as_str))
                .unwrap_or(&key)
                .to_string();
            (label, key)
        })
        .collect()
}

fn is_dialogue(e: &Value) -> bool {
    e.get("type").and_then(Value::as_str) == Some("dialogue")
}

fn row_of(e: &Value) -> Option<DialogueRow> {
    if !is_dialogue(e) {
        return None;
    }
    Some(DialogueRow {
        seq: e.get("seq").and_then(Value::as_i64)?,
        speaker: e
            .get("speaker")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        text: e
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Dialogue rows matching `q` (a case-insensitive substring of speaker or
/// text), paginated. `(this page's rows, total matches)`. `(empty, 0)` when
/// the episode has no parsed file yet.
pub fn list_dialogue(
    slug: &str,
    tag: &str,
    q: &str,
    page: i64,
    per_page: i64,
) -> Result<(Vec<DialogueRow>, usize), String> {
    let path = path_for(slug, tag)?;
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }
    let (_, entries) = load_entries(&path)?;
    let needle = q.trim().to_lowercase();
    let mut rows: Vec<DialogueRow> = entries.iter().filter_map(row_of).collect();
    if !needle.is_empty() {
        rows.retain(|r| {
            r.speaker.to_lowercase().contains(&needle) || r.text.to_lowercase().contains(&needle)
        });
    }
    let total = rows.len();
    let per_page = per_page.max(1) as usize;
    let start = (page.max(0) as usize) * per_page;
    let page_rows = rows.into_iter().skip(start).take(per_page).collect();
    Ok((page_rows, total))
}

/// One dialogue entry's current speaker/text, or why not (unknown seq, or
/// the seq is not a dialogue line).
pub fn get_entry(slug: &str, tag: &str, seq: i64) -> Result<DialogueRow, String> {
    let path = path_for(slug, tag)?;
    let (_, entries) = load_entries(&path)?;
    let entry = entries
        .iter()
        .find(|e| e.get("seq").and_then(Value::as_i64) == Some(seq))
        .ok_or_else(|| format!("No entry with seq {seq}."))?;
    row_of(entry).ok_or_else(|| {
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("?");
        format!("seq {seq} is a {kind} line, not dialogue.")
    })
}

/// Validate, mutate, recompute `stats`, back up and write — the one save
/// both the dashboard's Dialogue tab and the Timeline's dialogue modal call.
///
/// No locking between the two surfaces or the Raw JSON textarea editor:
/// last write wins, the same as the existing whole-file editor already
/// behaves — not worth new machinery for a single-operator local tool.
pub fn save_entry(
    slug: &str,
    tag: &str,
    seq: i64,
    speaker: &str,
    text: &str,
) -> Result<DialogueRow, String> {
    let path = path_for(slug, tag)?;
    let (mut top, mut entries) = load_entries(&path)?;

    let idx = entries
        .iter()
        .position(|e| e.get("seq").and_then(Value::as_i64) == Some(seq))
        .ok_or_else(|| format!("No entry with seq {seq}."))?;
    if !is_dialogue(&entries[idx]) {
        let kind = entries[idx]
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("?");
        return Err(format!(
            "seq {seq} is a {kind} line, not dialogue — use the Raw JSON tab."
        ));
    }

    let text = nfc_normalize(text.trim());
    if text.is_empty() {
        return Err("Dialogue text cannot be empty.".into());
    }
    let speaker = speaker.trim();
    if speaker.is_empty() {
        return Err("Speaker is required.".into());
    }
    let known = speaker_choices(slug);
    if !known.is_empty() && !known.iter().any(|(_, key)| key == speaker) {
        return Err(format!("Unknown speaker key {speaker:?}."));
    }

    {
        let obj = entries[idx]
            .as_object_mut()
            .ok_or("entry is not an object")?;
        obj.insert("speaker".into(), Value::String(speaker.to_string()));
        obj.insert("text".into(), Value::String(text.clone()));
    }
    top.insert(
        "stats".into(),
        Value::Object(stats_from_entries_json(&entries)),
    );
    top.insert("entries".into(), Value::Array(entries));

    // A failed backup aborts the save: never overwrite what we could not copy.
    if path.exists() {
        fs::copy(&path, backup_path(&path)).map_err(|e| format!("Backup failed — not saved: {e}"))?;
    }
    fs::write(&path, dumps(&Value::Object(top), Style::INDENT2_UTF8)).map_err(|e| e.to_string())?;

    if let Err(e) = append_dialogue_edit(&path, seq, speaker, &text) {
        activity::log(&format!("[WARN] dialogue edit journal write failed: {e}"));
    }
    activity::log(&format!("SAVE parsed dialogue → {} seq={seq}", path.display()));

    Ok(DialogueRow {
        seq,
        speaker: speaker.to_string(),
        text,
    })
}

/// The most recent value per key, comparing later entries as they arrive.
/// `ts`/`generated_at` are both `%Y-%m-%dT%H:%M:%S+00:00`, fixed-width and
/// always UTC, so plain string comparison sorts them chronologically —
/// no need to parse them as dates.
fn keep_latest(map: &mut HashMap<i64, String>, key: i64, value: &str) {
    map.entry(key)
        .and_modify(|cur| {
            if value > cur.as_str() {
                *cur = value.to_string();
            }
        })
        .or_insert_with(|| value.to_string());
}

/// Sequence numbers whose dialogue was edited after that seq's stem was
/// last produced — "changed since last produce". Empty when nothing is
/// pending, the episode has never been produced (no stem manifest yet), or
/// no dialogue journal exists.
///
/// Self-clearing by construction: once `xil produce --seq-list <seq>
/// --force` reruns, that seq's manifest `generated_at` moves forward again
/// and it drops off this list on its own — nothing here is written or
/// cleared by hand, it is recomputed fresh from the journal and the
/// manifest every time.
pub fn pending_reprocess(slug: &str, tag: &str) -> Result<Vec<i64>, String> {
    let parsed_path = path_for(slug, tag)?;
    let journal_path = dialogue_edits_path(&parsed_path);
    if !journal_path.exists() {
        return Ok(Vec::new());
    }

    let mut edited_at: HashMap<i64, String> = HashMap::new();
    let journal_text = fs::read_to_string(&journal_path).map_err(|e| e.to_string())?;
    for line in journal_text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let (Some(seq), Some(ts)) = (
            record.get("seq").and_then(Value::as_i64),
            record.get("ts").and_then(Value::as_str),
        ) else {
            continue;
        };
        keep_latest(&mut edited_at, seq, ts);
    }
    if edited_at.is_empty() {
        return Ok(Vec::new());
    }

    // The stem manifest's path convention, mirrored from `xil produce`
    // (`crates/xil-cli/src/cmd/produce.rs::manifest_path`) — read-only here.
    let stems_dir = derive_paths(slug, tag)["stems"].clone();
    let manifest_path = stems_dir.join(format!("{}_stem_manifest.json", basename(&stems_dir)));
    let manifest_entries: Vec<Value> = fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("entries").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    if manifest_entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut produced_at: HashMap<i64, String> = HashMap::new();
    for e in &manifest_entries {
        let (Some(seq), Some(at)) = (
            e.get("seq_at_generation").and_then(Value::as_i64),
            e.get("generated_at").and_then(Value::as_str),
        ) else {
            continue;
        };
        keep_latest(&mut produced_at, seq, at);
    }

    let mut pending: Vec<i64> = edited_at
        .into_iter()
        .filter(|(seq, edited)| produced_at.get(seq).is_some_and(|produced| edited.as_str() > produced.as_str()))
        .map(|(seq, _)| seq)
        .collect();
    pending.sort_unstable();
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `path_for`/`load_entries`/etc. all resolve against `XIL_PROJECTROOT`,
    // which every test in the process shares — exercising them needs the
    // `Workspace` harness's env-var mutex, so those cases live in
    // `tests/parsed_entries.rs` (an integration test, one process-wide env
    // var at a time) rather than here. `backup_path` alone touches no global
    // state and is worth covering directly, matching `parsed.rs`'s own
    // `backup_sits_beside_the_file` test.
    #[test]
    fn backup_sits_beside_the_file() {
        assert_eq!(
            backup_path(Path::new("/ws/parsed/the413/parsed_S01E01.json")),
            PathBuf::from("/ws/parsed/the413/parsed_S01E01.json.bak")
        );
    }

    #[test]
    fn path_for_refuses_unsafe_slug_or_tag() {
        assert!(path_for("../etc", "S01E01").is_err());
        assert!(path_for("the413", "a/b").is_err());
    }
}

//! `xil splice` — insert or delete parsed entries with automatic seq
//! renumbering. Port of `XILU006_splice_parsed.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value};
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::{derive_paths, resolve_slug};
use xil_core::{banner, log};

#[derive(Parser)]
#[command(
    name = "xil-splice",
    about = "Splice Parsed JSON — insert/delete entries with automatic renumbering"
)]
struct Args {
    /// Episode tag (e.g. S02E03) — derives target parsed JSON path
    #[arg(long, required_unless_present = "tag", conflicts_with = "tag")]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Override target parsed JSON path
    #[arg(long)]
    parsed: Option<PathBuf>,
    /// Seq number to insert after
    #[arg(long)]
    insert_after: Option<i64>,
    /// Source parsed JSON to extract entries from
    #[arg(long)]
    from_parsed: Option<PathBuf>,
    /// Seq range to extract from source (e.g. 232-233)
    #[arg(long)]
    from_seq_range: Option<String>,
    /// Path to a JSON array of entries to insert
    #[arg(long)]
    from_json: Option<PathBuf>,
    /// Override section on inserted entries
    #[arg(long)]
    section: Option<String>,
    /// Override scene on inserted entries
    #[arg(long)]
    scene: Option<String>,
    /// Seq range to delete (e.g. 100-105)
    #[arg(long)]
    delete_seq_range: Option<String>,
    /// Show plan without writing files
    #[arg(long)]
    dry_run: bool,
    /// Skip backup file
    #[arg(long)]
    no_backup: bool,
    /// Summary only, no per-entry detail
    #[arg(long)]
    quiet: bool,
}

type Entry = Map<String, Value>;

fn seq_of(e: &Entry) -> i64 {
    e.get("seq").and_then(Value::as_i64).unwrap_or(0)
}

fn str_of(e: &Entry, key: &str) -> String {
    e.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `e.get(key, "")` inside an f-string. The default only applies when the
/// key is absent: a key present with a null value renders as `None`, which
/// is what the report lines actually print for a direction entry.
fn py_get_str(e: &Entry, key: &str) -> String {
    match e.get(key) {
        None => String::new(),
        Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Entries within `[start, end]` inclusive.
pub fn extract_seq_range(entries: &[Entry], start: i64, end: i64) -> Vec<Entry> {
    entries
        .iter()
        .filter(|e| (start..=end).contains(&seq_of(e)))
        .cloned()
        .collect()
}

/// Insert `new_entries` after `insert_after_seq`, then renumber the body.
///
/// Preamble entries (seq <= 0) are never renumbered. New entries inherit
/// section and scene from the insertion point unless overridden.
pub fn splice_entries(
    entries: &[Entry],
    insert_after_seq: i64,
    new_entries: &[Entry],
    section_override: Option<&str>,
    scene_override: Option<&str>,
) -> Result<Vec<Entry>, String> {
    if insert_after_seq <= 0 {
        return Err(format!(
            "Cannot insert after seq {insert_after_seq} (preamble zone)"
        ));
    }
    let preamble: Vec<Entry> = entries.iter().filter(|e| seq_of(e) <= 0).cloned().collect();
    let mut body: Vec<Entry> = entries.iter().filter(|e| seq_of(e) > 0).cloned().collect();

    let Some(idx) = body.iter().position(|e| seq_of(e) == insert_after_seq) else {
        return Err(format!("seq {insert_after_seq} not found in entries"));
    };
    let anchor = body[idx].clone();

    let prepared: Vec<Entry> = new_entries
        .iter()
        .map(|e| {
            let mut ne = e.clone();
            ne.insert(
                "section".into(),
                match section_override {
                    Some(s) => Value::String(s.to_string()),
                    None => anchor.get("section").cloned().unwrap_or(Value::Null),
                },
            );
            ne.insert(
                "scene".into(),
                match scene_override {
                    Some(s) => Value::String(s.to_string()),
                    None => anchor.get("scene").cloned().unwrap_or(Value::Null),
                },
            );
            ne
        })
        .collect();

    body.splice(idx + 1..idx + 1, prepared);
    for (i, e) in body.iter_mut().enumerate() {
        e.insert("seq".into(), Value::from(i as i64 + 1));
    }
    Ok(preamble.into_iter().chain(body).collect())
}

/// Remove `[start, end]` inclusive and renumber the remainder.
pub fn delete_entries(entries: &[Entry], start: i64, end: i64) -> Result<Vec<Entry>, String> {
    if start <= 0 {
        return Err(format!(
            "Cannot delete preamble entries (seq_range starts at {start})"
        ));
    }
    let preamble: Vec<Entry> = entries.iter().filter(|e| seq_of(e) <= 0).cloned().collect();
    let mut body: Vec<Entry> = entries
        .iter()
        .filter(|e| seq_of(e) > 0 && !(start..=end).contains(&seq_of(e)))
        .cloned()
        .collect();
    for (i, e) in body.iter_mut().enumerate() {
        e.insert("seq".into(), Value::from(i as i64 + 1));
    }
    Ok(preamble.into_iter().chain(body).collect())
}

/// Recompute `stats` from the body entries (seq > 0).
pub fn update_stats(data: &mut Map<String, Value>) {
    let entries: Vec<Entry> = data
        .get("entries")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|e| e.as_object().cloned()).collect())
        .unwrap_or_default();
    let body: Vec<&Entry> = entries.iter().filter(|e| seq_of(e) > 0).collect();
    let dialogue: Vec<&&Entry> = body
        .iter()
        .filter(|e| str_of(e, "type") == "dialogue")
        .collect();
    let directions = body
        .iter()
        .filter(|e| str_of(e, "type") == "direction")
        .count();

    let mut speakers: Vec<String> = dialogue
        .iter()
        .filter_map(|e| {
            e.get("speaker")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .collect();
    speakers.sort();
    speakers.dedup();
    let mut sections: Vec<String> = body
        .iter()
        .filter_map(|e| {
            e.get("section")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .collect();
    sections.sort();
    sections.dedup();
    let tts_chars: usize = dialogue
        .iter()
        .map(|e| str_of(e, "text").chars().count())
        .sum();

    let mut stats = Map::new();
    stats.insert("total_entries".into(), Value::from(body.len()));
    stats.insert("dialogue_lines".into(), Value::from(dialogue.len()));
    stats.insert("direction_lines".into(), Value::from(directions));
    stats.insert("characters_for_tts".into(), Value::from(tts_chars));
    stats.insert(
        "speakers".into(),
        Value::Array(speakers.into_iter().map(Value::String).collect()),
    );
    stats.insert(
        "sections".into(),
        Value::Array(sections.into_iter().map(Value::String).collect()),
    );
    data.insert("stats".into(), Value::Object(stats));
}

/// `'N-M'` → `(N, M)`.
fn parse_range(s: &str) -> Option<(i64, i64)> {
    let (a, b) = s.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// First `n` characters, as Python's `text[:n]` slices.
fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let paths = derive_paths(&slug, &tag);
    let target = a.parsed.clone().unwrap_or_else(|| paths["parsed"].clone());

    if !target.exists() {
        log::error(&format!(
            "Target parsed JSON not found: {}",
            target.display()
        ));
        return Ok(0);
    }
    log::info(&format!("  Target: {}", target.display()));

    // Resolve the entries to insert.
    let mut new_entries: Option<Vec<Entry>> = None;
    if a.insert_after.is_some() {
        if let (Some(from), Some(range)) = (&a.from_parsed, &a.from_seq_range) {
            let Some((start, end)) = parse_range(range) else {
                log::error(&format!("Expected N-M range, got: {range}"));
                return Ok(2);
            };
            let source: Value = serde_json::from_str(&fs::read_to_string(from)?)?;
            let source_entries: Vec<Entry> = source
                .get("entries")
                .and_then(Value::as_array)
                .map(|v| v.iter().filter_map(|e| e.as_object().cloned()).collect())
                .unwrap_or_default();
            let picked = extract_seq_range(&source_entries, start, end);
            if picked.is_empty() {
                log::warning(&format!(
                    "No entries found in seq range {start}–{end} of {}",
                    from.display()
                ));
                return Ok(0);
            }
            log::info(&format!(
                "  Source: {} seq {start}–{end} ({} entries)",
                from.display(),
                picked.len()
            ));
            new_entries = Some(picked);
        } else if let Some(from_json) = &a.from_json {
            let v: Value = serde_json::from_str(&fs::read_to_string(from_json)?)?;
            let picked: Vec<Entry> = v
                .as_array()
                .map(|a| a.iter().filter_map(|e| e.as_object().cloned()).collect())
                .unwrap_or_default();
            log::info(&format!(
                "  Source: {} ({} entries)",
                from_json.display(),
                picked.len()
            ));
            new_entries = Some(picked);
        } else {
            log::error("--insert-after requires --from-parsed + --from-seq-range or --from-json");
            return Ok(0);
        }
    }

    let delete_range = match &a.delete_seq_range {
        Some(r) => match parse_range(r) {
            Some(v) => Some(v),
            None => {
                log::error(&format!("Expected N-M range, got: {r}"));
                return Ok(2);
            }
        },
        None => None,
    };

    if new_entries.is_none() && delete_range.is_none() {
        log::error("Nothing to do — specify --insert-after or --delete-seq-range");
        return Ok(0);
    }

    let backup_path = (!a.no_backup && !a.dry_run).then(|| {
        let name = format!("pre_splice_parsed_{slug}_{tag}.json");
        match target.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(d) => d.join(name),
            None => PathBuf::from(name),
        }
    });

    run_splice(
        &target,
        a.insert_after,
        new_entries.as_deref(),
        delete_range,
        a.section.as_deref(),
        a.scene.as_deref(),
        a.dry_run,
        backup_path.as_deref(),
        a.quiet,
    )?;

    if !a.dry_run {
        log::info("\n  Next steps:");
        log::info(&format!(
            "    1. python XILP007_stem_migrator.py --episode {tag} --orig-prefix pre_splice_"
        ));
        log::info(&format!(
            "    2. python XILP002_producer.py --episode {tag}"
        ));
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
fn run_splice(
    target: &Path,
    insert_after: Option<i64>,
    new_entries: Option<&[Entry]>,
    delete_range: Option<(i64, i64)>,
    section_override: Option<&str>,
    scene_override: Option<&str>,
    dry_run: bool,
    backup_path: Option<&Path>,
    quiet: bool,
) -> anyhow::Result<()> {
    let original_content = fs::read_to_string(target)?;
    let mut data: Map<String, Value> = serde_json::from_str(&original_content)?;
    let mut entries: Vec<Entry> = data
        .get("entries")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|e| e.as_object().cloned()).collect())
        .unwrap_or_default();
    let original_count = entries.iter().filter(|e| seq_of(e) > 0).count();

    if let Some((start, end)) = delete_range {
        let deleted: Vec<Entry> = entries
            .iter()
            .filter(|e| (start..=end).contains(&seq_of(e)))
            .cloned()
            .collect();
        entries = delete_entries(&entries, start, end).map_err(|e| anyhow::anyhow!(e))?;
        if !quiet {
            log::info(&format!(
                "\n  DELETE seq {start}–{end}: {} entries removed",
                deleted.len()
            ));
            for e in &deleted {
                log::info(&format!(
                    "    - seq {} [{}] {} — {}",
                    seq_of(e),
                    str_of(e, "type"),
                    py_get_str(e, "speaker"),
                    head(&str_of(e, "text"), 60)
                ));
            }
        }
    }

    if let (Some(after), Some(new)) = (insert_after, new_entries) {
        if !new.is_empty() {
            if !quiet {
                // Python's conditional expression binds over the whole
                // logger.info call, so with no anchor it logs an empty line.
                match entries.iter().find(|e| seq_of(e) == after) {
                    Some(anchor) => log::info(&format!(
                        "\n  INSERT {} entries after seq {after} ({}...)",
                        new.len(),
                        head(&str_of(anchor, "text"), 40)
                    )),
                    None => log::info(""),
                }
                for e in new {
                    let label = section_override.unwrap_or("(inherit)");
                    log::info(&format!(
                        "    + [{}] {} — {}  [section={label}]",
                        str_of(e, "type"),
                        py_get_str(e, "speaker"),
                        head(&str_of(e, "text"), 60)
                    ));
                }
            }
            entries = splice_entries(&entries, after, new, section_override, scene_override)
                .map_err(|e| anyhow::anyhow!(e))?;
        }
    }

    data.insert(
        "entries".into(),
        Value::Array(entries.iter().cloned().map(Value::Object).collect()),
    );
    update_stats(&mut data);

    let new_count = entries.iter().filter(|e| seq_of(e) > 0).count();
    log::info(&format!(
        "\n  Summary: {original_count} → {new_count} entries (body)"
    ));

    if dry_run {
        log::info("  [DRY RUN] No files written.");
        return Ok(());
    }
    if let Some(b) = backup_path {
        fs::write(b, &original_content)?;
        log::info(&format!("  Backup written: {}", b.display()));
    }
    fs::write(
        target,
        format!("{}\n", dumps(&Value::Object(data), Style::INDENT2_UTF8)),
    )?;
    log::info(&format!("  Updated: {}", target.display()));
    Ok(())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("splice");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-splice", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    execute(&a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entries(v: Value) -> Vec<Entry> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_object().unwrap().clone())
            .collect()
    }

    fn seqs(es: &[Entry]) -> Vec<i64> {
        es.iter().map(seq_of).collect()
    }

    #[test]
    fn insert_renumbers_the_body_and_spares_the_preamble() {
        let base = entries(json!([
            {"seq": -1, "type": "dialogue", "text": "pre", "section": "preamble", "scene": null},
            {"seq": 1, "type": "dialogue", "text": "a", "section": "act1", "scene": "scene-1"},
            {"seq": 2, "type": "dialogue", "text": "b", "section": "act1", "scene": "scene-1"}
        ]));
        let new = entries(
            json!([{"seq": 99, "type": "dialogue", "text": "n", "section": "x", "scene": "y"}]),
        );
        let out = splice_entries(&base, 1, &new, None, None).unwrap();
        assert_eq!(seqs(&out), vec![-1, 1, 2, 3]);
        assert_eq!(str_of(&out[2], "text"), "n");
        assert_eq!(
            str_of(&out[2], "section"),
            "act1",
            "inherited from the anchor"
        );
        assert_eq!(str_of(&out[2], "scene"), "scene-1");
    }

    #[test]
    fn overrides_win_over_inheritance() {
        let base = entries(
            json!([{"seq": 1, "type": "dialogue", "text": "a", "section": "act1", "scene": "scene-1"}]),
        );
        let new = entries(json!([{"seq": 9, "type": "dialogue", "text": "n"}]));
        let out = splice_entries(&base, 1, &new, Some("post-interview"), Some("scene-9")).unwrap();
        assert_eq!(str_of(&out[1], "section"), "post-interview");
        assert_eq!(str_of(&out[1], "scene"), "scene-9");
    }

    #[test]
    fn insert_rejects_the_preamble_zone_and_missing_anchors() {
        let base = entries(
            json!([{"seq": 1, "type": "dialogue", "text": "a", "section": null, "scene": null}]),
        );
        assert_eq!(
            splice_entries(&base, 0, &[], None, None).unwrap_err(),
            "Cannot insert after seq 0 (preamble zone)"
        );
        assert_eq!(
            splice_entries(&base, 7, &[], None, None).unwrap_err(),
            "seq 7 not found in entries"
        );
    }

    #[test]
    fn delete_closes_the_gap() {
        let base = entries(json!([
            {"seq": -1, "type": "dialogue", "text": "pre"},
            {"seq": 1, "type": "dialogue", "text": "a"},
            {"seq": 2, "type": "dialogue", "text": "b"},
            {"seq": 3, "type": "dialogue", "text": "c"},
            {"seq": 4, "type": "dialogue", "text": "d"}
        ]));
        let out = delete_entries(&base, 2, 3).unwrap();
        assert_eq!(seqs(&out), vec![-1, 1, 2]);
        assert_eq!(str_of(&out[2], "text"), "d");
        assert_eq!(
            delete_entries(&base, 0, 1).unwrap_err(),
            "Cannot delete preamble entries (seq_range starts at 0)"
        );
    }

    #[test]
    fn stats_count_body_entries_only() {
        let mut data: Map<String, Value> = json!({"entries": [
            {"seq": -1, "type": "dialogue", "text": "pre", "speaker": "tina", "section": "preamble"},
            {"seq": 1, "type": "dialogue", "text": "Café", "speaker": "adam", "section": "act1"},
            {"seq": 2, "type": "direction", "text": "SFX: X", "section": "act1"},
            {"seq": 3, "type": "dialogue", "text": "hi", "speaker": "adam", "section": "act2"}
        ]})
        .as_object()
        .unwrap()
        .clone();
        update_stats(&mut data);
        let s = &data["stats"];
        assert_eq!(s["total_entries"], 3, "the preamble entry is excluded");
        assert_eq!(s["dialogue_lines"], 2);
        assert_eq!(s["direction_lines"], 1);
        assert_eq!(s["characters_for_tts"], 6, "4 + 2, counted in characters");
        assert_eq!(s["speakers"], json!(["adam"]));
        assert_eq!(s["sections"], json!(["act1", "act2"]));
    }

    #[test]
    fn absent_and_null_speakers_render_differently() {
        let e = entries(
            json!([{"seq": 1, "type": "direction", "speaker": null, "text": "t"},
                               {"seq": 2, "type": "direction", "text": "t"}]),
        );
        assert_eq!(
            py_get_str(&e[0], "speaker"),
            "None",
            "present but null prints None"
        );
        assert_eq!(py_get_str(&e[1], "speaker"), "", "absent takes the default");
    }

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("232-233"), Some((232, 233)));
        assert_eq!(parse_range("5-5"), Some((5, 5)));
        assert_eq!(parse_range("nope"), None);
        assert_eq!(parse_range("1-"), None);
    }
}

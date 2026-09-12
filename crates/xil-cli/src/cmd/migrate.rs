//! `xil migrate` — carry stems across a script revision. Port of
//! `XILP007_stem_migrator.py`.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use clap::Parser;
use regex::Regex;
use serde_json::{Map, Value};
use xil_core::workspace::{derive_paths, resolve_slug, workspace_root};
use xil_core::{banner, log};

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static EM_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\u{2014}\s*").unwrap());

pub const COPY: &str = "COPY";
pub const SPEAKER: &str = "SPEAKER";
pub const NEW: &str = "NEW";
pub const MISSING: &str = "MISSING";
pub const SKIP: &str = "SKIP";

/// Entry types that produce a stem file.
const STEM_TYPES: [&str; 3] = ["dialogue", "direction", "silence"];

/// Characters shown before a text snippet is truncated.
const SNIP: usize = 55;

const USAGE: &str =
    "usage: xil-migrate [-h] [--episode TAG] [--tag TAG] [--show SHOW] [--old PATH]\n\
                     \x20                  [--new PATH] [--stems DIR] [--orig-prefix ORIG_PREFIX]\n\
                     \x20                  [--dry-run] [--strict] [--quiet]";

fn parser_error(msg: &str) -> i32 {
    eprintln!("{USAGE}");
    eprintln!("xil-migrate: error: {msg}");
    2
}

#[derive(Parser)]
#[command(
    name = "xil-migrate",
    about = "Migrate episode stems from an old parsed JSON to a revised one. Copies unchanged stems to their \
             new seq-numbered filenames; reports what still needs TTS/SFX generation. Run XILP002 afterwards \
             to fill the gaps."
)]
struct Args {
    /// Episode tag (e.g. S02E03); derives --old, --new, and --stems paths automatically
    #[arg(long, value_name = "TAG")]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01); same as --episode but skips format validation
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Old parsed JSON (overrides --episode)
    #[arg(long, value_name = "PATH")]
    old: Option<PathBuf>,
    /// New parsed JSON (overrides --episode)
    #[arg(long, value_name = "PATH")]
    new: Option<PathBuf>,
    /// Stems directory (overrides --episode)
    #[arg(long, value_name = "DIR")]
    stems: Option<PathBuf>,
    /// Filename prefix for the old parsed JSON (default: orig_)
    #[arg(long, default_value = "orig_")]
    orig_prefix: String,
    /// Show plan without copying any files
    #[arg(long)]
    dry_run: bool,
    /// Exact text match only. Default is fuzzy: ignores em-dash/ellipsis variants so punctuation-only edits don't force unnecessary regen.
    #[arg(long)]
    strict: bool,
    /// Print only the summary, not per-stem details
    #[arg(long)]
    quiet: bool,
}

type Entry = Map<String, Value>;

/// What should happen to one new parsed entry.
#[derive(Debug, Clone)]
pub struct Action {
    pub status: &'static str,
    pub new_stem: String,
    pub old_seq: Option<i64>,
    pub old_stem: Option<String>,
    pub reason: String,
    pub new_text: String,
}

/// Collapse whitespace; in fuzzy mode also fold em-dash, ellipsis and
/// curly quotes so punctuation-only edits do not force regeneration.
pub fn normalize_text(text: Option<&str>, strict: bool) -> String {
    let Some(text) = text else {
        return String::new();
    };
    let text = WHITESPACE.replace_all(text.trim(), " ").into_owned();
    if strict {
        return text;
    }
    EM_DASH
        .replace_all(&text, " - ")
        .replace('\u{2026}', "...")
        .replace(['\u{2018}', '\u{2019}'], "'")
        .replace(['\u{201c}', '\u{201d}'], "\"")
}

/// Expected stem filename for a parsed entry. Preamble entries (seq < 0)
/// keep the legacy `n`-prefixed form.
pub fn make_stem_name(entry: &Entry) -> String {
    let seq = entry.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let section = entry
        .get("section")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    let scene = entry
        .get("scene")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let speaker = entry
        .get("speaker")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("sfx");

    let prefix = if seq < 0 {
        format!("n{:03}", seq.abs())
    } else {
        format!("{seq:03}")
    };
    let mid = match scene {
        Some(s) => format!("{section}-{s}"),
        None => section.to_string(),
    };
    if entry.get("type").and_then(Value::as_str) == Some("dialogue") {
        format!("{prefix}_{mid}_{speaker}.mp3")
    } else {
        format!("{prefix}_{mid}_sfx.mp3")
    }
}

fn snip(text: Option<&str>) -> String {
    let Some(t) = text.filter(|t| !t.is_empty()) else {
        return String::new();
    };
    let t = t.trim();
    if t.chars().count() > SNIP {
        format!("{}\u{2026}", t.chars().take(SNIP).collect::<String>())
    } else {
        t.to_string()
    }
}

fn match_key(entry: &Entry, strict: bool) -> (String, String) {
    let role = entry
        .get("speaker")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("sfx");
    (
        normalize_text(entry.get("text").and_then(Value::as_str), strict),
        role.to_string(),
    )
}

struct Record {
    entry: Entry,
    old_stem: String,
    exists: bool,
}

/// Compare old and new entries and produce one action per new entry.
///
/// Matching is two-phase: exact on (text, speaker), then a text-only
/// fallback for dialogue so a speaker reassignment is reported as such
/// rather than as a brand-new line.
pub fn plan_migration(
    old_entries: &[Entry],
    new_entries: &[Entry],
    stems_dir: &Path,
    strict: bool,
) -> Vec<Action> {
    let mut exact: HashMap<(String, String), Record> = HashMap::new();
    let mut text_only: HashMap<String, Record> = HashMap::new();
    for entry in old_entries {
        let etype = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if !STEM_TYPES.contains(&etype) {
            continue;
        }
        let stem_name = make_stem_name(entry);
        let exists = stems_dir.join(&stem_name).is_file();
        let ekey = match_key(entry, strict);
        // The first occurrence wins, so repeated cues favour reuse.
        exact.entry(ekey).or_insert(Record {
            entry: entry.clone(),
            old_stem: stem_name.clone(),
            exists,
        });
        let tkey = normalize_text(entry.get("text").and_then(Value::as_str), strict);
        text_only.entry(tkey).or_insert(Record {
            entry: entry.clone(),
            old_stem: stem_name,
            exists,
        });
    }

    let mut used_exact: HashSet<(String, String)> = HashSet::new();
    let mut used_text: HashSet<String> = HashSet::new();
    let mut actions = Vec::new();

    for entry in new_entries {
        let etype = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if !STEM_TYPES.contains(&etype) {
            actions.push(Action {
                status: SKIP,
                new_stem: String::new(),
                old_seq: None,
                old_stem: None,
                reason: String::new(),
                new_text: String::new(),
            });
            continue;
        }

        let new_stem = make_stem_name(entry);
        let new_speaker = entry.get("speaker").and_then(Value::as_str);
        let ekey = match_key(entry, strict);
        let tkey = normalize_text(entry.get("text").and_then(Value::as_str), strict);

        if let Some(m) = exact.get(&ekey) {
            if !used_exact.contains(&ekey) {
                used_exact.insert(ekey.clone());
                used_text.insert(tkey.clone());
                let old_seq = m.entry.get("seq").and_then(Value::as_i64);
                actions.push(Action {
                    status: if m.exists { COPY } else { MISSING },
                    new_stem,
                    old_seq,
                    old_stem: Some(m.old_stem.clone()),
                    reason: if m.exists {
                        String::new()
                    } else {
                        "old stem file not on disk".into()
                    },
                    new_text: snip(entry.get("text").and_then(Value::as_str)),
                });
                continue;
            }
        }

        if etype == "dialogue" {
            if let Some(tm) = text_only.get(&tkey) {
                if !used_text.contains(&tkey) {
                    used_text.insert(tkey.clone());
                    let old_speaker = tm.entry.get("speaker").and_then(Value::as_str);
                    if old_speaker != new_speaker {
                        actions.push(Action {
                            status: SPEAKER,
                            new_stem,
                            old_seq: tm.entry.get("seq").and_then(Value::as_i64),
                            old_stem: Some(tm.old_stem.clone()),
                            reason: format!(
                                "speaker: {} → {}",
                                py_opt(old_speaker),
                                py_opt(new_speaker)
                            ),
                            new_text: snip(entry.get("text").and_then(Value::as_str)),
                        });
                        continue;
                    }
                }
            }
        }

        actions.push(Action {
            status: NEW,
            new_stem,
            old_seq: None,
            old_stem: None,
            reason: "no matching old entry".into(),
            new_text: snip(entry.get("text").and_then(Value::as_str)),
        });
    }
    actions
}

/// How Python renders an optional speaker inside an f-string.
fn py_opt(v: Option<&str>) -> String {
    v.map(str::to_string).unwrap_or_else(|| "None".into())
}

/// Copy the COPY actions; return per-status counts.
pub fn execute_migration(
    actions: &[Action],
    stems_dir: &Path,
    dry_run: bool,
) -> std::io::Result<HashMap<&'static str, usize>> {
    let mut counts: HashMap<&'static str, usize> =
        [(COPY, 0), (SPEAKER, 0), (NEW, 0), (MISSING, 0), (SKIP, 0)]
            .into_iter()
            .collect();
    for a in actions {
        *counts.entry(a.status).or_insert(0) += 1;
        if a.status != COPY {
            continue;
        }
        let Some(old) = &a.old_stem else { continue };
        let (src, dst) = (stems_dir.join(old), stems_dir.join(&a.new_stem));
        if src == dst || dry_run {
            continue;
        }
        fs::copy(&src, &dst)?;
    }
    Ok(counts)
}

fn print_report(actions: &[Action], dry_run: bool) {
    let label = if dry_run { "[DRY RUN] " } else { "" };
    let stem_actions: Vec<&Action> = actions.iter().filter(|a| a.status != SKIP).collect();
    log::info(&format!(
        "\n{label}Migration plan ({} stem entries):\n",
        stem_actions.len()
    ));
    for a in stem_actions {
        match a.status {
            COPY => {
                if a.old_stem.as_deref() == Some(a.new_stem.as_str()) {
                    log::info(&format!("  COPY     {}  (seq unchanged)", a.new_stem));
                } else {
                    log::info(&format!(
                        "  COPY     {}  ← {}",
                        a.new_stem,
                        a.old_stem.clone().unwrap_or_default()
                    ));
                }
            }
            SPEAKER => log::info(&format!("  SPEAKER  {}  ({})", a.new_stem, a.reason)),
            MISSING => log::info(&format!(
                "  MISSING  {}  (matched seq {} but file absent)",
                a.new_stem,
                a.old_seq
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "None".into())
            )),
            NEW => log::info(&format!("  NEW      {}  ({})", a.new_stem, a.reason)),
            _ => {}
        }
        if !a.new_text.is_empty() {
            log::info(&format!("           \"{}\"", a.new_text));
        }
    }
}

fn print_summary(counts: &HashMap<&'static str, usize>, dry_run: bool) {
    let g = |k: &str| *counts.get(k).unwrap_or(&0);
    let need_gen = g(SPEAKER) + g(NEW) + g(MISSING);
    let label = if dry_run { "[DRY RUN] " } else { "" };
    log::info(&format!("\n{label}─── Summary ───"));
    log::info(&format!(
        "  COPY    : {:4}  (unchanged — reused, no TTS call)",
        g(COPY)
    ));
    log::info(&format!(
        "  SPEAKER : {:4}  (speaker changed → must regenerate)",
        g(SPEAKER)
    ));
    log::info(&format!(
        "  NEW     : {:4}  (no old match → must generate)",
        g(NEW)
    ));
    log::info(&format!(
        "  MISSING : {:4}  (old match but file absent → generate)",
        g(MISSING)
    ));
    log::info(&format!(
        "  SKIP    : {:4}  (non-stem entries, no action)",
        g(SKIP)
    ));
    log::info("  ─────────────────────────────────────");
    log::info(&format!("  Need generation : {need_gen}"));
    log::info("");
    if dry_run {
        log::info("  Re-run without --dry-run to copy the COPY stems.");
    } else {
        log::info(&format!("  {} stems copied.", g(COPY)));
    }
    log::info("  Then run:  python XILP002_producer.py --episode <TAG>");
    log::info("  XILP002 skips stems already on disk — only gaps get generated.");
}

fn entries_of(v: &Value) -> Vec<Entry> {
    v.get("entries")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|e| e.as_object().cloned()).collect())
        .unwrap_or_default()
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let (old_path, new_path, stems_dir) = match a.episode.as_ref().or(a.tag.as_ref()) {
        Some(tag) => {
            let slug = resolve_slug(a.show.as_deref(), "project.json");
            let p = derive_paths(&slug, tag);
            (
                a.old.clone().unwrap_or_else(|| {
                    workspace_root()
                        .join("parsed")
                        .join(format!("{}parsed_{slug}_{tag}.json", a.orig_prefix))
                }),
                a.new.clone().unwrap_or_else(|| p["parsed"].clone()),
                a.stems.clone().unwrap_or_else(|| p["stems"].clone()),
            )
        }
        None => match (&a.old, &a.new, &a.stems) {
            (Some(o), Some(n), Some(s)) => (o.clone(), n.clone(), s.clone()),
            _ => {
                return Ok(parser_error(
                    "Provide --episode, or all three of --old, --new, and --stems.",
                ))
            }
        },
    };

    for (p, label) in [(&old_path, "--old"), (&new_path, "--new")] {
        if !p.is_file() {
            return Ok(parser_error(&format!(
                "{label} file not found: {}",
                p.display()
            )));
        }
    }

    log::info(&format!("  Old parsed : {}", old_path.display()));
    log::info(&format!("  New parsed : {}", new_path.display()));
    log::info(&format!("  Stems dir  : {}", stems_dir.display()));
    log::info(&format!(
        "  Match mode : {}",
        if a.strict {
            "strict"
        } else {
            "fuzzy (ignores em-dash / ellipsis variants)"
        }
    ));
    log::info(&format!(
        "  Dry run    : {}",
        if a.dry_run { "True" } else { "False" }
    ));

    let old_data: Value = serde_json::from_str(&fs::read_to_string(&old_path)?)?;
    let new_data: Value = serde_json::from_str(&fs::read_to_string(&new_path)?)?;
    let actions = plan_migration(
        &entries_of(&old_data),
        &entries_of(&new_data),
        &stems_dir,
        a.strict,
    );

    if !a.quiet {
        print_report(&actions, a.dry_run);
    }
    let counts = execute_migration(&actions, &stems_dir, a.dry_run)?;
    print_summary(&counts, a.dry_run);
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("migrate");
    let _banner = banner::begin("XILP007 stem migrator", &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-migrate", args) {
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

    #[test]
    fn fuzzy_normalization_folds_punctuation() {
        assert_eq!(normalize_text(Some("  a   b  "), false), "a b");
        assert_eq!(normalize_text(Some("a \u{2014} b"), false), "a - b");
        assert_eq!(normalize_text(Some("a\u{2026}"), false), "a...");
        assert_eq!(
            normalize_text(Some("\u{2018}q\u{2019} \u{201c}d\u{201d}"), false),
            "'q' \"d\""
        );
        assert_eq!(
            normalize_text(Some("a \u{2014} b"), true),
            "a \u{2014} b",
            "strict keeps the dash"
        );
        assert_eq!(normalize_text(None, false), "");
    }

    #[test]
    fn stem_names_cover_dialogue_direction_and_preamble() {
        let d = entries(
            json!([{"seq": 19, "section": "act1", "scene": "scene-1", "type": "dialogue", "speaker": "maya"}]),
        );
        assert_eq!(make_stem_name(&d[0]), "019_act1-scene-1_maya.mp3");
        let x = entries(json!([{"seq": 4, "section": "act1", "scene": null, "type": "direction"}]));
        assert_eq!(make_stem_name(&x[0]), "004_act1_sfx.mp3");
        let p = entries(
            json!([{"seq": -2, "section": "preamble", "type": "dialogue", "speaker": "tina"}]),
        );
        assert_eq!(make_stem_name(&p[0]), "n002_preamble_tina.mp3");
        let u = entries(json!([{"seq": 1, "section": null, "type": "dialogue", "speaker": "x"}]));
        assert_eq!(make_stem_name(&u[0]), "001_unknown_x.mp3");
    }

    #[test]
    fn plan_covers_copy_speaker_new_missing_and_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("001_act1_adam.mp3"), "x").unwrap();
        // 002's old file is deliberately absent → MISSING.

        let old = entries(json!([
            {"seq": 1, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "Kept line."},
            {"seq": 2, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "Gone from disk."},
            {"seq": 3, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "Reassigned."}
        ]));
        let new = entries(json!([
            {"seq": 1, "type": "section_header", "section": "act1", "text": "ACT ONE"},
            {"seq": 2, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "Kept line."},
            {"seq": 3, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "Gone from disk."},
            {"seq": 4, "type": "dialogue", "section": "act1", "speaker": "maya", "text": "Reassigned."},
            {"seq": 5, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "Brand new."}
        ]));

        let plan = plan_migration(&old, &new, d, false);
        let got: Vec<&str> = plan.iter().map(|a| a.status).collect();
        assert_eq!(got, vec![SKIP, COPY, MISSING, SPEAKER, NEW]);
        assert_eq!(plan[1].new_stem, "002_act1_adam.mp3");
        assert_eq!(plan[1].old_stem.as_deref(), Some("001_act1_adam.mp3"));
        assert_eq!(plan[3].reason, "speaker: adam → maya");
        assert_eq!(plan[4].new_text, "Brand new.");
    }

    #[test]
    fn fuzzy_matching_survives_a_punctuation_edit() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("001_act1_adam.mp3"), "x").unwrap();
        let old = entries(
            json!([{"seq": 1, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "A \u{2014} B"}]),
        );
        let new = entries(
            json!([{"seq": 1, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "A - B"}]),
        );
        assert_eq!(
            plan_migration(&old, &new, tmp.path(), false)[0].status,
            COPY
        );
        assert_eq!(
            plan_migration(&old, &new, tmp.path(), true)[0].status,
            NEW,
            "strict sees a different line"
        );
    }

    #[test]
    fn copy_only_runs_when_the_name_actually_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("001_act1_adam.mp3"), "payload").unwrap();
        let old = entries(
            json!([{"seq": 1, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "t"}]),
        );
        let new = entries(
            json!([{"seq": 2, "type": "dialogue", "section": "act1", "speaker": "adam", "text": "t"}]),
        );
        let plan = plan_migration(&old, &new, d, false);
        let counts = execute_migration(&plan, d, false).unwrap();
        assert_eq!(counts[COPY], 1);
        assert_eq!(
            fs::read_to_string(d.join("002_act1_adam.mp3")).unwrap(),
            "payload"
        );

        // A dry run touches nothing.
        let plan2 = plan_migration(&old, &new, d, false);
        fs::remove_file(d.join("002_act1_adam.mp3")).unwrap();
        execute_migration(&plan2, d, true).unwrap();
        assert!(!d.join("002_act1_adam.mp3").exists());
    }
}

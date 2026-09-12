//! `xil parse` — production script markdown → structured JSON, plus the
//! cast and SFX config skeletons. Port of `XILP001_script_parser.py`'s CLI.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use clap::Parser;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value};
use xil_core::fsutil::basename;
use xil_core::journal::{append_sfx_edit, replay_sfx_edits, sfx_edits_path};
use xil_core::pycsv;
use xil_core::pyjson::{dumps, Style};
use xil_core::script::hints::filter_sfx_overrides;
use xil_core::script::speakers::{key_to_display, load_speakers, load_speakers_registry};
use xil_core::script::{parse_script, Entry, ParseOpts, Parsed};
use xil_core::workspace::{
    derive_paths, episode_tag, resolve_project_type, resolve_season, resolve_season_title,
    resolve_slug, show_slug, workspace_root,
};
use xil_core::{banner, log};

static BEAT_SECONDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s+SECOND").unwrap());

const DEBUG_TRUNCATE: usize = 200;

#[derive(Parser)]
#[command(
    name = "xil-parse",
    about = "Parse production script markdown into structured JSON"
)]
struct Args {
    /// Path to the production script markdown file
    script: PathBuf,
    /// Episode tag (e.g. S01E01) — validates header and auto-generates absent cast/sfx configs
    #[arg(long, conflicts_with = "tag")]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01, CH003) — skips season/episode header validation
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Output JSON path (default: parsed/parsed_<slug>_<TAG>.json)
    #[arg(long, short = 'o')]
    output: Option<PathBuf>,
    /// Show first N dialogue lines (default: show all)
    #[arg(long)]
    preview: Option<usize>,
    /// Only output JSON, skip summary/preview
    #[arg(long)]
    quiet: bool,
    /// Write diagnostic CSV alongside JSON output
    #[arg(long)]
    debug: bool,
    /// Print per-speaker dialogue distribution (lines, words, chars, %)
    #[arg(long)]
    stats: bool,
    /// Path to speakers.json (default: auto-detect from CWD, then built-in)
    #[arg(long)]
    speakers: Option<PathBuf>,
}

/// Parse the file at `path` with the project fallbacks applied.
fn run_parse(path: &Path, speakers: Option<&Path>) -> anyhow::Result<Parsed> {
    let raw = fs::read_to_string(path)?;
    let opts = ParseOpts {
        project_type: &resolve_project_type("project.json"),
        speakers_path: speakers,
        season_fallback: resolve_season(None, "project.json"),
        season_title_fallback: resolve_season_title(None, "project.json"),
    };
    Ok(parse_script(&raw, &basename(path), &opts, &mut |m| {
        log::warning(&m)
    }))
}

// ── config skeletons ────────────────────────────────────────────────────

/// Write the cast skeleton: one entry per speaker found in the script.
fn generate_cast_config(
    parsed: &Value,
    cast_path: &Path,
    tag_override: Option<&str>,
    registry: &IndexMap<String, Map<String, Value>>,
    key_display: &IndexMap<String, String>,
) -> anyhow::Result<()> {
    let speakers: Vec<String> = parsed["stats"]["speakers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();

    let mut cast = Map::new();
    for key in &speakers {
        let empty = Map::new();
        let reg = registry.get(key).unwrap_or(&empty);
        let display = key_display.get(key).cloned().unwrap_or_else(|| key.clone());
        let default_name = title_case(&display.replace('_', " "));

        let mut member = Map::new();
        member.insert(
            "full_name".into(),
            or_default(reg.get("full_name"), Value::String(default_name)),
        );
        member.insert(
            "voice_id".into(),
            or_default(reg.get("voice_id"), Value::String("TBD".into())),
        );
        member.insert(
            "pan".into(),
            reg.get("pan").cloned().unwrap_or(Value::from(0.0)),
        );
        member.insert(
            "filter".into(),
            reg.get("filter").cloned().unwrap_or(Value::Bool(false)),
        );
        member.insert(
            "role".into(),
            or_default(reg.get("role"), Value::String("TBD".into())),
        );
        for field in [
            "stability",
            "similarity_boost",
            "style",
            "use_speaker_boost",
            "language_code",
        ] {
            if let Some(v) = reg.get(field).filter(|v| !v.is_null()) {
                member.insert(field.into(), v.clone());
            }
        }
        cast.insert(key.clone(), Value::Object(member));
    }

    let mut config = Map::new();
    config.insert(
        "show".into(),
        parsed
            .get("show")
            .cloned()
            .unwrap_or(Value::String("Unknown Show".into())),
    );
    config.insert(
        "season".into(),
        if tag_override.is_some() {
            Value::Null
        } else {
            parsed["season"].clone()
        },
    );
    config.insert(
        "episode".into(),
        if tag_override.is_some() {
            Value::Null
        } else {
            parsed["episode"].clone()
        },
    );
    config.insert(
        "title".into(),
        parsed
            .get("title")
            .cloned()
            .unwrap_or(Value::String(String::new())),
    );
    config.insert(
        "season_title".into(),
        parsed.get("season_title").cloned().unwrap_or(Value::Null),
    );
    config.insert("cast".into(), Value::Object(cast));
    if let Some(t) = tag_override {
        config.insert("tag_override".into(), Value::String(t.to_string()));
    }

    write_json_no_newline(cast_path, &Value::Object(config))?;
    log::info(&format!(
        "Created {} with {} speakers (voice_id=TBD — run XILU001 to assign)",
        cast_path.display(),
        speakers.len()
    ));
    Ok(())
}

/// `x or default` — a null, empty string or absent value takes the default.
fn or_default(v: Option<&Value>, default: Value) -> Value {
    match v {
        Some(Value::Null) | None => default,
        Some(Value::String(s)) if s.is_empty() => default,
        Some(Value::Bool(false)) => default,
        Some(other) => other.clone(),
    }
}

/// Python `str.title()`: first letter of each run of letters uppercased.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_alpha = false;
    for c in s.chars() {
        if c.is_alphabetic() {
            if prev_alpha {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_alpha = true;
        } else {
            out.push(c);
            prev_alpha = false;
        }
    }
    out
}

/// Apply attribute hints to one effect entry, in place. A `play_duration`
/// hint clears `duration_seconds` on source-backed cues — the two are
/// mutually exclusive and the skeleton default would contradict the hint.
fn apply_sfx_overrides(
    key: &str,
    entry: &mut Map<String, Value>,
    overrides: &IndexMap<String, f64>,
) -> usize {
    let is_silence = entry.get("type").and_then(Value::as_str) == Some("silence");
    let applied = filter_sfx_overrides(key, is_silence, overrides, Some(&mut |m| log::warning(&m)));
    if applied.is_empty() {
        return 0;
    }
    for (k, v) in &applied {
        entry.insert(k.clone(), json_f64(*v));
    }
    if applied.contains_key("play_duration") && entry.get("source").is_some_and(|s| !s.is_null()) {
        entry.shift_remove("duration_seconds");
    }
    applied.len()
}

fn json_f64(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Overrides carried on a parsed direction entry, as the JSON holds them.
fn entry_overrides(e: &Value) -> IndexMap<String, f64> {
    e.get("sfx_overrides")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect()
        })
        .unwrap_or_default()
}

/// Default generation length by cue category.
fn default_duration(text: &str) -> f64 {
    if text.starts_with("AMBIENCE:") {
        30.0
    } else if text.starts_with("MUSIC:") {
        15.0
    } else {
        5.0
    }
}

/// Write the SFX skeleton: one entry per unique direction.
fn generate_sfx_config(
    parsed: &Value,
    sfx_path: &Path,
    tag_override: Option<&str>,
) -> anyhow::Result<()> {
    let mut effects: Map<String, Value> = Map::new();
    let (mut silence_count, mut sfx_count) = (0usize, 0usize);

    for e in parsed["entries"].as_array().cloned().unwrap_or_default() {
        if e.get("type").and_then(Value::as_str) != Some("direction") {
            continue;
        }
        let text = e
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if effects.contains_key(&text) {
            continue;
        }
        let source = e
            .get("sfx_source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let overrides = entry_overrides(&e);

        let mut entry = Map::new();
        if text == "BEAT" {
            entry.insert("type".into(), Value::String("silence".into()));
            entry.insert("duration_seconds".into(), json_f64(1.0));
            silence_count += 1;
        } else if text == "LONG BEAT" {
            entry.insert("type".into(), Value::String("silence".into()));
            entry.insert("duration_seconds".into(), json_f64(2.0));
            silence_count += 1;
        } else if text.starts_with("BEAT") {
            // "BEAT — 3 SECONDS", "BEAT — LONG, 5 SECONDS", …
            let dur = BEAT_SECONDS
                .captures(&text)
                .and_then(|c| c[1].parse().ok())
                .unwrap_or(1.0);
            entry.insert("type".into(), Value::String("silence".into()));
            entry.insert("duration_seconds".into(), json_f64(dur));
            silence_count += 1;
        } else if text == "AMBIENCE: STOP" || text.ends_with("FADES OUT") {
            entry.insert("type".into(), Value::String("silence".into()));
            entry.insert("duration_seconds".into(), json_f64(0.0));
            silence_count += 1;
        } else if text.starts_with("FILM AUDIO")
            || text.starts_with("SPEAKERPHONE")
            || text.starts_with("PHONE FILTER")
        {
            // A span marker: the treatment lands on dialogue stems in the
            // mixer, so the cue has no audio of its own. Zero duration keeps
            // it out of the generation set. Must stay ahead of the source
            // branch so a stray pipe hint cannot create one.
            entry.insert("type".into(), Value::String("silence".into()));
            entry.insert("duration_seconds".into(), json_f64(0.0));
            silence_count += 1;
        } else if let Some(src) = &source {
            entry.insert("source".into(), Value::String(src.clone()));
            entry.insert("duration_seconds".into(), json_f64(default_duration(&text)));
            if text.starts_with("AMBIENCE:") {
                entry.insert("loop".into(), Value::Bool(true));
            }
            sfx_count += 1;
        } else if text.starts_with("AMBIENCE:") {
            entry.insert("prompt".into(), Value::String(text.clone()));
            entry.insert("duration_seconds".into(), json_f64(30.0));
            entry.insert("loop".into(), Value::Bool(true));
            sfx_count += 1;
        } else if text.starts_with("MUSIC:") {
            entry.insert("prompt".into(), Value::String(text.clone()));
            entry.insert("duration_seconds".into(), json_f64(15.0));
            sfx_count += 1;
        } else {
            entry.insert("prompt".into(), Value::String(text.clone()));
            entry.insert("duration_seconds".into(), json_f64(5.0));
            sfx_count += 1;
        }

        apply_sfx_overrides(&text, &mut entry, &overrides);
        effects.insert(text, Value::Object(entry));
    }

    let mut config = Map::new();
    config.insert("_docs".into(), docs_block());
    config.insert(
        "show".into(),
        parsed
            .get("show")
            .cloned()
            .unwrap_or(Value::String("Unknown Show".into())),
    );
    config.insert(
        "season".into(),
        if tag_override.is_some() {
            Value::Null
        } else {
            parsed["season"].clone()
        },
    );
    config.insert(
        "episode".into(),
        if tag_override.is_some() {
            Value::Null
        } else {
            parsed["episode"].clone()
        },
    );
    config.insert("defaults".into(), defaults_block());
    config.insert("effects".into(), Value::Object(effects));
    if let Some(t) = tag_override {
        config.insert("tag_override".into(), Value::String(t.to_string()));
    }

    write_json_no_newline(sfx_path, &Value::Object(config))?;
    let total = silence_count + sfx_count;
    log::info(&format!(
        "Created {} with {total} effects ({silence_count} silence, {sfx_count} sfx — review prompts before generation)",
        sfx_path.display()
    ));

    // A fresh skeleton wipes hand-tuned overrides — replay the journal.
    let replay = replay_sfx_edits(sfx_path, false, &mut |m| log::warning(&m))?;
    if replay.applied > 0 {
        let note = if replay.orphans.is_empty() {
            String::new()
        } else {
            format!(" ({} orphaned key(s))", replay.orphans.len())
        };
        log::info(&format!(
            "Reapplied {} timeline sound edit(s) from {}{note}",
            replay.applied,
            sfx_edits_path(sfx_path).display()
        ));
    }
    Ok(())
}

fn docs_block() -> Value {
    let mut d = Map::new();
    d.insert(
        "defaults".into(),
        Value::String(
            "Episode-wide fallbacks. Category prefixes: ambience_, music_, sfx_, vintage_filter_. \
             E.g. 'ambience_volume_percentage': 30. Global fallbacks: volume_percentage, ramp_in_seconds, \
             ramp_out_seconds."
                .into(),
        ),
    );
    d.insert(
        "per_effect_override".into(),
        Value::String(
            "Use plain 'volume_percentage' (no prefix) inside an effect entry to override the category default \
             for that one cue only."
                .into(),
        ),
    );
    d.insert(
        "source".into(),
        Value::String(
            "Relative path from workspace root, e.g. 'SFX/filename.mp3'. Explicit source always wins over the \
             shared pool cache."
                .into(),
        ),
    );
    d.insert(
        "play_duration".into(),
        Value::String(
            "Percentage of clip to play (0–100, e.g. 50 = half). Not applicable to AMBIENCE."
                .into(),
        ),
    );
    d.insert(
        "prompt".into(),
        Value::String(
            "ElevenLabs SFX API natural-language description for generated effects.".into(),
        ),
    );
    d.insert(
        "type".into(),
        Value::String("'sfx' for API-generated audio, 'silence' for local silence padding.".into()),
    );
    Value::Object(d)
}

fn defaults_block() -> Value {
    let mut d = Map::new();
    d.insert("prompt_influence".into(), json_f64(0.3));
    d.insert("volume_percentage".into(), Value::from(20));
    d.insert("ramp_in_seconds".into(), json_f64(1.0));
    d.insert("ramp_out_seconds".into(), json_f64(1.0));
    d.insert("ambience_volume_percentage".into(), Value::from(30));
    d.insert("ambience_ramp_in_seconds".into(), json_f64(1.0));
    d.insert("ambience_ramp_out_seconds".into(), json_f64(1.0));
    Value::Object(d)
}

/// Both skeleton writers use `json.dump` with no trailing newline.
fn write_json_no_newline(path: &Path, v: &Value) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        if !d.as_os_str().is_empty() {
            fs::create_dir_all(d)?;
        }
    }
    fs::write(path, dumps(v, Style::INDENT2_UTF8))?;
    Ok(())
}

/// Does a hint's `SFX/...` path resolve on disk? Guards forced replacement:
/// plenty of working sources are bare `SFX/<file>` with no slug-form twin.
fn hint_target_exists(source: &str) -> bool {
    let p = Path::new(source);
    if p.is_absolute() {
        p.exists()
    } else {
        workspace_root().join(source).exists()
    }
}

/// Fill missing `source` fields on an existing SFX config from parsed hints.
///
/// A hint never replaces an existing source unless `force`, and then only
/// when the hinted file resolves. Attribute hints go the other way: the
/// script is authoritative and overwrites whatever the config holds.
fn backfill_sfx_sources(parsed: &Value, sfx_path: &Path, force: bool) -> anyhow::Result<()> {
    let text = fs::read_to_string(sfx_path)?;
    let mut data: Value = serde_json::from_str(&text)?;
    let obj = data
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("sfx config is not an object"))?;
    obj.entry("effects")
        .or_insert_with(|| Value::Object(Map::new()));

    // Piped keys left by a pre-fix parse, mapped to their clean form.
    let mut stale_key_map: Vec<(String, String)> = Vec::new();
    for existing in obj["effects"]
        .as_object()
        .cloned()
        .unwrap_or_default()
        .keys()
    {
        let h = xil_core::script::hints::parse_direction_hint(existing, "", &mut |_| {});
        if h.source.is_some() && &h.clean != existing {
            stale_key_map.push((existing.clone(), h.clean));
        }
    }

    let mut updated = 0usize;
    let mut journaled: Vec<(String, String)> = Vec::new();
    let mut seen_clean: Vec<String> = Vec::new();

    for e in parsed["entries"].as_array().cloned().unwrap_or_default() {
        if e.get("type").and_then(Value::as_str) != Some("direction") {
            continue;
        }
        let source = e
            .get("sfx_source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let overrides = entry_overrides(&e);
        if source.is_none() && overrides.is_empty() {
            continue;
        }
        let text = e
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if seen_clean.contains(&text) {
            continue;
        }
        seen_clean.push(text.clone());

        let effects = obj["effects"].as_object_mut().expect("effects");
        if effects.contains_key(&text) {
            let current = effects[&text]
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_string);
            let entry = effects[&text].as_object_mut().expect("effect object");
            if let (Some(src), None) = (&source, &current) {
                entry.insert("source".into(), Value::String(src.clone()));
                if entry.get("prompt").and_then(Value::as_str) == Some(text.as_str()) {
                    entry.shift_remove("prompt");
                }
                journaled.push((text.clone(), src.clone()));
                updated += 1;
            } else if force && source.is_some() && current.as_deref() != source.as_deref() {
                let src = source.clone().expect("checked");
                if hint_target_exists(&src) {
                    entry.insert("source".into(), Value::String(src.clone()));
                    entry.shift_remove("prompt");
                    journaled.push((text.clone(), src));
                    updated += 1;
                } else {
                    log::warning(&format!(
                        "  Keeping '{text}' source {} — hint target {src} not found on disk",
                        current.as_deref().unwrap_or("None")
                    ));
                }
            }
        } else if let Some(stale) = stale_key_map
            .iter()
            .find(|(_, clean)| *clean == text)
            .map(|(k, _)| k.clone())
        {
            // A stale piped key: rename it and attach the source.
            if let Some(mut old) = effects.shift_remove(&stale) {
                if let (Some(src), Some(o)) = (&source, old.as_object_mut()) {
                    o.insert("source".into(), Value::String(src.clone()));
                    o.shift_remove("prompt");
                    journaled.push((text.clone(), src.clone()));
                }
                effects.insert(text.clone(), old);
                updated += 1;
            }
        } else {
            // Absent entirely: create it.
            let mut new_entry = Map::new();
            new_entry.insert("duration_seconds".into(), json_f64(default_duration(&text)));
            match &source {
                Some(src) => new_entry.insert("source".into(), Value::String(src.clone())),
                // A volume-only hint on an unknown cue still needs a prompt.
                None => new_entry.insert("prompt".into(), Value::String(text.clone())),
            };
            if text.starts_with("AMBIENCE:") {
                new_entry.insert("loop".into(), Value::Bool(true));
            }
            effects.insert(text.clone(), Value::Object(new_entry));
            if let Some(src) = &source {
                journaled.push((text.clone(), src.clone()));
            }
            updated += 1;
        }

        // The script wins for attribute hints.
        let effects = obj["effects"].as_object_mut().expect("effects");
        let entry = effects[&text].as_object_mut().expect("effect object");
        let changed: IndexMap<String, f64> = overrides
            .into_iter()
            .filter(|(k, v)| entry.get(k).and_then(Value::as_f64) != Some(*v))
            .collect();
        if apply_sfx_overrides(&text, entry, &changed) > 0 {
            updated += 1;
        }
    }

    if updated > 0 {
        write_json_no_newline(sfx_path, &data)?;
        // Journal after the config write: a record that survives a failed
        // write would reinstate a source the config never had.
        for (key, src) in journaled {
            let mut fields = Map::new();
            fields.insert("source".into(), Value::String(src));
            append_sfx_edit(sfx_path, &key, &fields)?;
        }
        log::info(&format!(
            "Backfilled {updated} script hint(s) in {}",
            sfx_path.display()
        ));
    }
    Ok(())
}

// ── reporting ───────────────────────────────────────────────────────────

fn write_debug_csv(path: &Path, parsed: &Parsed) -> anyhow::Result<()> {
    let mut rows = Vec::new();
    for (line_num, raw_line, idx) in &parsed.debug_line_map {
        let e: &Entry = &parsed.entries[*idx];
        let mut row = Map::new();
        row.insert("md_line_num".into(), Value::from(*line_num));
        row.insert("md_raw".into(), Value::String(truncate(raw_line)));
        row.insert("seq".into(), Value::from(e.seq));
        row.insert("type".into(), Value::from(e.kind));
        row.insert(
            "section".into(),
            Value::String(e.section.clone().unwrap_or_default()),
        );
        row.insert(
            "scene".into(),
            Value::String(e.scene.clone().unwrap_or_default()),
        );
        row.insert(
            "speaker".into(),
            Value::String(e.speaker.clone().unwrap_or_default()),
        );
        row.insert(
            "direction".into(),
            Value::String(e.direction.clone().unwrap_or_default()),
        );
        row.insert(
            "direction_type".into(),
            Value::String(e.direction_type.unwrap_or("").to_string()),
        );
        row.insert("text".into(), Value::String(truncate(&e.text)));
        rows.push(row);
    }
    let cols = [
        "md_line_num",
        "md_raw",
        "seq",
        "type",
        "section",
        "scene",
        "speaker",
        "direction",
        "direction_type",
        "text",
    ];
    let mut f = fs::File::create(path)?;
    pycsv::write_dicts(&mut f, &cols, &rows)?;
    Ok(())
}

fn truncate(s: &str) -> String {
    s.chars().take(DEBUG_TRUNCATE).collect()
}

/// `f"{n:,}"` — thousands separators.
fn commas(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn print_speaker_stats(parsed: &Parsed) {
    let rows = xil_core::script::compute_speaker_stats(&parsed.entries);
    if rows.is_empty() {
        log::info("  No dialogue entries found.");
        return;
    }
    let (tl, tw, tc): (usize, usize, usize) = rows.iter().fold((0, 0, 0), |a, r| {
        (a.0 + r.lines, a.1 + r.words, a.2 + r.chars)
    });

    log::info(&format!(
        "\n{:<15} {:>6} {:>6} {:>7} {:>6} {:>8} {:>6}",
        "Speaker", "Lines", "%", "Words", "%", "Chars", "%"
    ));
    let d = |n: usize| "-".repeat(n);
    log::info(&format!(
        "{} {} {} {} {} {} {}",
        d(15),
        d(6),
        d(6),
        d(7),
        d(6),
        d(8),
        d(6)
    ));
    for r in &rows {
        log::info(&format!(
            "{:<15} {:>6} {:>5.1}% {:>7} {:>5.1}% {:>8} {:>5.1}%",
            r.speaker,
            r.lines,
            r.pct_lines,
            commas(r.words),
            r.pct_words,
            commas(r.chars),
            r.pct_chars
        ));
    }
    log::info(&format!(
        "{} {} {} {} {} {} {}",
        d(15),
        d(6),
        d(6),
        d(7),
        d(6),
        d(8),
        d(6)
    ));
    log::info(&format!(
        "{:<15} {:>6}        {:>7}        {:>8}",
        "TOTAL",
        tl,
        commas(tw),
        commas(tc)
    ));
    log::info("");
}

fn print_summary(parsed: &Parsed, json: &Value) {
    let stats = &json["stats"];
    let tag = episode_tag(parsed.season, parsed.episode);
    let bar = "=".repeat(60);
    log::info(&format!("\n{bar}"));
    log::info(&format!("PARSED: {} {tag} — {}", parsed.show, parsed.title));
    log::info(&format!("Source: {}", parsed.source_file));
    log::info(&bar);
    log::info(&format!("  Total entries:      {}", stats["total_entries"]));
    log::info(&format!(
        "  Dialogue lines:     {}",
        stats["dialogue_lines"]
    ));
    log::info(&format!(
        "  Stage directions:   {}",
        stats["direction_lines"]
    ));
    log::info(&format!(
        "  TTS characters:     {}",
        commas(stats["characters_for_tts"].as_u64().unwrap_or(0) as usize)
    ));
    log::info(&format!(
        "  Speakers:           {}",
        join_strs(&stats["speakers"])
    ));
    log::info(&format!(
        "  Sections:           {}",
        join_strs(&stats["sections"])
    ));
    log::info(&bar);
    print_speaker_stats(parsed);
}

fn join_strs(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn print_dialogue_preview(parsed: &Parsed, limit: Option<usize>) {
    let all: Vec<&Entry> = parsed
        .entries
        .iter()
        .filter(|e| e.kind == "dialogue")
        .collect();
    let shown = match limit.filter(|l| *l > 0) {
        Some(l) => &all[..l.min(all.len())],
        None => &all[..],
    };
    log::info(&format!(
        "\n--- Dialogue Preview ({} lines) ---\n",
        shown.len()
    ));
    for e in shown {
        let scene_label = e
            .scene
            .clone()
            .or_else(|| e.section.clone())
            .unwrap_or_else(|| "?".to_string());
        let direction_label = e
            .direction
            .as_ref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        let text_preview = if e.text.chars().count() > 80 {
            format!("{}...", e.text.chars().take(80).collect::<String>())
        } else {
            e.text.clone()
        };
        log::info(&format!(
            "  {:03} | {scene_label:<16} | {:<14}{direction_label}",
            e.seq,
            e.speaker.clone().unwrap_or_default()
        ));
        log::info(&format!("       {text_preview}"));
        log::info("");
    }
}

// ── entry point ─────────────────────────────────────────────────────────

fn execute(a: &Args) -> anyhow::Result<i32> {
    let mut parsed = run_parse(&a.script, a.speakers.as_deref())?;

    let tag = match &a.tag {
        Some(t) => t.clone(),
        None => {
            let derived = episode_tag(parsed.season, parsed.episode);
            if let Some(ep) = &a.episode {
                if ep != &derived {
                    log::error(&format!(
                        "Script header indicates {derived} but --episode {ep} was specified"
                    ));
                    return Ok(1);
                }
            }
            derived
        }
    };

    if let Some(s) = &a.show {
        parsed.show = s.clone();
    }
    let slug = {
        let from_header = show_slug(&parsed.show);
        if from_header.is_empty() {
            resolve_slug(a.show.as_deref(), "project.json")
        } else {
            from_header
        }
    };
    let paths = derive_paths(&slug, &tag);
    let output = a.output.clone().unwrap_or_else(|| paths["parsed"].clone());
    if let Some(d) = output.parent() {
        if !d.as_os_str().is_empty() {
            fs::create_dir_all(d)?;
        }
    }

    if a.debug {
        // Python re-parses with the debug path set; the CSV name comes from
        // the resolved output path.
        let csv_path = with_extension_csv(&output);
        parsed = run_parse(&a.script, a.speakers.as_deref())?;
        if let Some(s) = &a.show {
            parsed.show = s.clone();
        }
        write_debug_csv(&csv_path, &parsed)?;
    }

    let json = parsed.to_json();
    // Skip the write when nothing changed, so a re-parse of an unmodified
    // script does not bump the mtime and falsely stale the later stages.
    let new_content = dumps(&json, Style::INDENT2_UTF8);
    let existing = fs::read_to_string(&output).ok();
    if existing.as_deref() == Some(new_content.as_str()) {
        log::info("Parsed output unchanged — skipping write (mtime preserved)");
    } else {
        let mut f = fs::File::create(&output)?;
        f.write_all(new_content.as_bytes())?;
    }

    if !a.quiet {
        print_summary(&parsed, &json);
        print_dialogue_preview(&parsed, a.preview);
        log::info(&format!("JSON written to: {}", output.display()));
        if a.debug {
            log::info(&format!(
                "Debug CSV written to: {}",
                with_extension_csv(&output).display()
            ));
        }
    }
    if a.stats && a.quiet {
        print_speaker_stats(&parsed);
    }

    let trigger = a.tag.as_ref().or(a.episode.as_ref());
    if trigger.is_none() {
        log::warning(&format!(
            "No --episode tag given — cast and SFX skeleton configs were NOT created. \
             Re-run with --episode {tag} to generate them."
        ));
        return Ok(0);
    }

    let cast_path = &paths["cast"];
    let sfx_path = &paths["sfx"];
    if !cast_path.exists() {
        let registry = load_speakers_registry(a.speakers.as_deref());
        // generate_cast_config reads the module-level speaker table, which
        // main() has loaded without any CAST-block seeding.
        let module_speakers = load_speakers(a.speakers.as_deref(), &[]);
        generate_cast_config(
            &json,
            cast_path,
            a.tag.as_deref(),
            &registry,
            &key_to_display(&module_speakers),
        )?;
    }
    if !sfx_path.exists() {
        generate_sfx_config(&json, sfx_path, a.tag.as_deref())?;
    } else {
        backfill_sfx_sources(&json, sfx_path, false)?;
    }
    Ok(0)
}

/// `os.path.splitext(p)[0] + ".csv"`.
fn with_extension_csv(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let stem = match (s.rfind('.'), s.rfind('/')) {
        (Some(dot), Some(slash)) if dot > slash + 1 => &s[..dot],
        (Some(dot), None) if dot > 0 => &s[..dot],
        _ => &s[..],
    };
    PathBuf::from(format!("{stem}.csv"))
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("parse");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-parse", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    execute(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_case_matches_python() {
        assert_eq!(title_case("mr patterson"), "Mr Patterson");
        assert_eq!(title_case("NORA WALSH"), "Nora Walsh");
        assert_eq!(title_case("t-bone"), "T-Bone");
        assert_eq!(title_case("o'brien"), "O'Brien");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1000), "1,000");
        assert_eq!(commas(1234567), "1,234,567");
    }

    #[test]
    fn csv_path_swaps_the_extension() {
        assert_eq!(
            with_extension_csv(Path::new("parsed/s/parsed_S01E01.json")),
            Path::new("parsed/s/parsed_S01E01.csv")
        );
        assert_eq!(
            with_extension_csv(Path::new("noext")),
            Path::new("noext.csv")
        );
    }

    #[test]
    fn or_default_treats_falsy_as_absent() {
        assert_eq!(or_default(None, Value::from("d")), Value::from("d"));
        assert_eq!(
            or_default(Some(&Value::Null), Value::from("d")),
            Value::from("d")
        );
        assert_eq!(
            or_default(Some(&Value::from("")), Value::from("d")),
            Value::from("d")
        );
        assert_eq!(
            or_default(Some(&Value::from("x")), Value::from("d")),
            Value::from("x")
        );
    }

    #[test]
    fn beat_durations_come_from_the_text() {
        assert_eq!(
            BEAT_SECONDS.captures("BEAT — 3 SECONDS").unwrap()[1]
                .parse::<f64>()
                .unwrap(),
            3.0
        );
        assert_eq!(
            BEAT_SECONDS.captures("BEAT — LONG, 5 SECONDS").unwrap()[1]
                .parse::<f64>()
                .unwrap(),
            5.0
        );
        assert!(BEAT_SECONDS.captures("BEAT — LONG").is_none());
    }

    #[test]
    fn play_duration_hint_clears_duration_on_source_cues_only() {
        let mut o = IndexMap::new();
        o.insert("play_duration".to_string(), 35.0);

        let mut sourced = Map::new();
        sourced.insert("source".into(), Value::from("SFX/a.mp3"));
        sourced.insert("duration_seconds".into(), json_f64(5.0));
        apply_sfx_overrides("MUSIC: X", &mut sourced, &o);
        assert!(!sourced.contains_key("duration_seconds"));
        assert_eq!(sourced["play_duration"], 35.0);

        let mut generated = Map::new();
        generated.insert("prompt".into(), Value::from("MUSIC: X"));
        generated.insert("duration_seconds".into(), json_f64(15.0));
        apply_sfx_overrides("MUSIC: X", &mut generated, &o);
        assert_eq!(
            generated["duration_seconds"], 15.0,
            "generation length is not a trim"
        );
    }
}

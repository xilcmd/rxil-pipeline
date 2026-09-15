//! `xil regen` — parsed JSON back to a production script. Port of
//! `XILP009_script_regenerator.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use indexmap::IndexMap;
use serde_json::Value;
use xil_core::fsutil::basename;
use xil_core::script::hints::{format_hint_attr, HINT_ATTRS};
use xil_core::script::speakers::{load_speakers, Speakers};
use xil_core::workspace::{derive_paths, resolve_slug};
use xil_core::{banner, log};

const SCRIPT_NAME: &str = "XILP009_script_regenerator.py";

#[derive(Parser)]
#[command(
    name = "xil-regen",
    about = "Regenerate a production script markdown from parsed JSON."
)]
struct Args {
    /// Episode tag (e.g. S02E03)
    #[arg(long, required_unless_present = "tag", conflicts_with = "tag")]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Override parsed JSON path
    #[arg(long)]
    parsed: Option<PathBuf>,
    /// Override cast config path
    #[arg(long)]
    cast: Option<PathBuf>,
    /// Override SFX config path (sfx_<TAG>.json); when supplied, direction entries are emitted with a pipe-hint suffix carrying the source filename and any play_volume_pct / play_duration_pct override
    #[arg(long)]
    sfx: Option<PathBuf>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Output markdown path (default: scripts/revised_<slug>_{TAG}.md)
    #[arg(long)]
    output: Option<PathBuf>,
    /// Path to speakers.json (default: auto-detect from CWD, then built-in)
    #[arg(long)]
    speakers: Option<PathBuf>,
}

/// Speaker key → display name, for turning parsed fields back into prose.
///
/// The Python also builds a section slug → header map, but its
/// `regenerate_script` emits `section_header` text verbatim and never
/// consults it, so that half is deliberately not ported.
pub struct Reverse {
    speaker: IndexMap<String, String>,
}

/// The first display name registered for a key wins.
pub fn build_reverse(speakers: &Speakers) -> Reverse {
    let mut speaker: IndexMap<String, String> = IndexMap::new();
    for (display, key) in &speakers.keys {
        speaker
            .entry(key.clone())
            .or_insert_with(|| display.clone());
    }
    Reverse { speaker }
}

impl Reverse {
    pub fn speaker_display(&self, key: &str) -> String {
        self.speaker
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_uppercase())
    }
}

/// Direction text → pipe-hint suffix, from an SFX config.
///
/// The suffix carries the source basename, any attribute hints, or both.
/// A cue with neither produces no hint, so it regenerates as plain `[TEXT]`.
pub fn build_sfx_lookup(sfx_config: &Value) -> IndexMap<String, String> {
    let mut lookup = IndexMap::new();
    let effects = sfx_config
        .get("effects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (key, effect) in effects {
        let mut parts: Vec<String> = Vec::new();
        if let Some(src) = effect
            .get("source")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            parts.push(basename(Path::new(src)));
        }
        for (_, field) in HINT_ATTRS {
            if let Some(v) = effect.get(field).and_then(Value::as_f64) {
                parts.push(format_hint_attr(field, v));
            }
        }
        if !parts.is_empty() {
            lookup.insert(key, parts.join(" | "));
        }
    }
    lookup
}

/// Rebuild the markdown script.
pub fn regenerate_script(
    parsed: &Value,
    cast: Option<&Value>,
    sfx_config: Option<&Value>,
    rev: &Reverse,
) -> String {
    let sfx_lookup = sfx_config.map(build_sfx_lookup).unwrap_or_default();

    let show = parsed
        .get("show")
        .and_then(Value::as_str)
        .unwrap_or("Unknown Show");
    let season = parsed.get("season").and_then(Value::as_i64);
    let episode = parsed.get("episode").and_then(Value::as_i64).unwrap_or(1);
    let title = parsed.get("title").and_then(Value::as_str).unwrap_or("");
    let season_title = parsed
        .get("season_title")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut lines: Vec<String> = Vec::new();

    let mut header = show.to_string();
    if let Some(s) = season {
        header.push_str(&format!(" Season {s}:"));
    }
    header.push_str(&format!(" Episode {episode}:"));
    if !title.is_empty() {
        header.push_str(&format!(" \"{title}\""));
    }
    if !season_title.is_empty() {
        header.push_str(&format!(" Arc: \"{season_title}\""));
    }
    lines.push(header);
    lines.push(String::new());

    if let Some(characters) = cast.and_then(|c| c.get("cast")).and_then(Value::as_object) {
        if !characters.is_empty() {
            lines.push("CAST:".into());
            for (key, char) in characters {
                let display = char
                    .get("full_name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| key.to_uppercase());
                let role = char.get("role").and_then(Value::as_str).unwrap_or("");
                let role = role.split('\n').next().unwrap_or("");
                lines.push(format!("* {display} — {role}"));
            }
            lines.push(String::new());
        }
    }

    let entries = parsed
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for entry in entries {
        // Skip the synthetic seq-0 preamble stems of the old XILP002 format.
        if entry.get("seq").and_then(Value::as_i64).unwrap_or(0) < 1 {
            continue;
        }
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let text = entry.get("text").and_then(Value::as_str).unwrap_or("");

        match kind {
            "section_header" => {
                lines.push("===".into());
                lines.push(String::new());
                lines.push(text.to_string());
                lines.push(String::new());
            }
            "scene_header" => {
                lines.push(text.to_string());
                lines.push(String::new());
            }
            "direction" => {
                let suffix = sfx_lookup
                    .get(text)
                    .map(|h| format!(" | {h}"))
                    .unwrap_or_default();
                lines.push(format!("[{text}{suffix}]"));
                lines.push(String::new());
            }
            "dialogue" => {
                let display = match entry
                    .get("speaker")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    Some(k) => rev.speaker_display(k),
                    None => "UNKNOWN".to_string(),
                };
                match entry
                    .get("direction")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                {
                    Some(d) => lines.push(format!("{display} ({d})")),
                    None => lines.push(display),
                }
                lines.push(text.to_string());
                lines.push(String::new());
            }
            _ => {}
        }
    }

    lines.push("END OF EPISODE".into());
    lines.push(String::new());
    lines.join("\n")
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let speakers = load_speakers(a.speakers.as_deref(), &[]);
    let rev = build_reverse(&speakers);

    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let p = derive_paths(&slug, &tag);
    let parsed_path = a.parsed.clone().unwrap_or_else(|| p["parsed"].clone());
    let cast_path = a.cast.clone().unwrap_or_else(|| p["cast"].clone());
    let sfx_path = a.sfx.clone().unwrap_or_else(|| p["sfx"].clone());
    let output_path = a
        .output
        .clone()
        .unwrap_or_else(|| p["revised_script"].clone());

    if !parsed_path.exists() {
        log::error(&format!("Parsed JSON not found: {}", parsed_path.display()));
        return Ok(1);
    }
    let parsed: Value = serde_json::from_str(&fs::read_to_string(&parsed_path)?)?;
    let cast = cast_path.exists().then(|| read_json(&cast_path)).flatten();

    let sfx_config = if sfx_path.exists() {
        log::info(&format!("  SFX config loaded: {}", sfx_path.display()));
        read_json(&sfx_path)
    } else {
        log::debug(&format!(
            "  No SFX config found at {} — pipe-hints disabled",
            sfx_path.display()
        ));
        None
    };

    let script_text = regenerate_script(&parsed, cast.as_ref(), sfx_config.as_ref(), &rev);
    if let Some(d) = output_path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(d)?;
    }
    fs::write(&output_path, script_text)?;

    let entry_count = parsed
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let dialogue_count = parsed
        .get("stats")
        .and_then(|s| s.get("dialogue_lines"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    log::info(&format!(
        "  Regenerated script from {entry_count} entries ({dialogue_count} dialogue)"
    ));
    log::info(&format!("  Written to: {}", output_path.display()));
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("regen");
    let a: Args = match super::parse_or_exit("xil-regen", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    // The banner opens after parsing here: Python reads the tag first.
    let _banner = banner::begin(SCRIPT_NAME, &super::argv_line(args));
    execute(&a)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rev() -> Reverse {
        build_reverse(&load_speakers(Some(Path::new("/nonexistent")), &[]))
    }

    #[test]
    fn speaker_keys_map_back_to_the_first_display() {
        let r = rev();
        assert_eq!(r.speaker_display("adam"), "ADAM");
        assert_eq!(
            r.speaker_display("film_audio"),
            "FILM AUDIO (MARGARET'S VOICE)"
        );
        assert_eq!(r.speaker_display("nobody"), "NOBODY");
    }

    #[test]
    fn sfx_lookup_joins_source_and_attributes() {
        let cfg = json!({"effects": {
            "OUTRO MUSIC": {"source": "SFX/the413/sundy3M4_v3.mp3", "volume_percentage": 20.0},
            "SFX: PLAIN": {"prompt": "p"},
            "BEAT": {"type": "silence"},
            "MUSIC: TRIMMED": {"source": "SFX/a.mp3", "play_duration": 35.0},
            "SFX: VOL ONLY": {"prompt": "p", "volume_percentage": 15.0}
        }});
        let lookup = build_sfx_lookup(&cfg);
        assert_eq!(
            lookup["OUTRO MUSIC"],
            "sundy3M4_v3.mp3 | play_volume_pct=20%"
        );
        assert_eq!(lookup["MUSIC: TRIMMED"], "a.mp3 | play_duration_pct=35%");
        assert_eq!(lookup["SFX: VOL ONLY"], "play_volume_pct=15%");
        assert!(
            !lookup.contains_key("SFX: PLAIN"),
            "a bare prompt yields no hint"
        );
        assert!(!lookup.contains_key("BEAT"));
    }

    #[test]
    fn regenerated_script_round_trips_the_shapes() {
        let parsed = json!({
            "show": "THE 413", "season": 2, "episode": 5, "title": "T", "season_title": "Arc",
            "entries": [
                {"seq": 0, "type": "dialogue", "speaker": "tina", "text": "synthetic", "direction": null},
                {"seq": 1, "type": "section_header", "text": "COLD OPEN"},
                {"seq": 2, "type": "scene_header", "text": "SCENE 1: ROOM"},
                {"seq": 3, "type": "direction", "text": "SFX: DOOR"},
                {"seq": 4, "type": "direction", "text": "OUTRO MUSIC"},
                {"seq": 5, "type": "dialogue", "speaker": "adam", "direction": "quietly", "text": "Hi."},
                {"seq": 6, "type": "dialogue", "speaker": null, "direction": null, "text": "Who?"}
            ]
        });
        let cast =
            json!({"cast": {"adam": {"full_name": "Adam Santos", "role": "Host\nsecond line"}}});
        let sfx = json!({"effects": {"OUTRO MUSIC": {"source": "SFX/out.mp3"}}});
        let out = regenerate_script(&parsed, Some(&cast), Some(&sfx), &rev());
        assert_eq!(
            out,
            "THE 413 Season 2: Episode 5: \"T\" Arc: \"Arc\"\n\n\
             CAST:\n* Adam Santos — Host\n\n\
             ===\n\nCOLD OPEN\n\n\
             SCENE 1: ROOM\n\n\
             [SFX: DOOR]\n\n\
             [OUTRO MUSIC | out.mp3]\n\n\
             ADAM (quietly)\nHi.\n\n\
             UNKNOWN\nWho?\n\n\
             END OF EPISODE\n"
        );
    }

    #[test]
    fn a_seasonless_untitled_script_keeps_a_minimal_header() {
        let parsed = json!({"show": "S", "season": null, "episode": 3, "title": "", "season_title": null, "entries": []});
        let out = regenerate_script(&parsed, None, None, &rev());
        assert_eq!(out, "S Episode 3:\n\nEND OF EPISODE\n");
    }
}

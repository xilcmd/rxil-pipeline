//! `xil produce` — generate one voice stem per dialogue line (ElevenLabs,
//! gTTS or Chatterbox Turbo), plus optional SFX stems, with a manifest that
//! lets `--reconcile` re-link stems after a re-parse. Port of
//! `XILP002_producer.py`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cmd::py_str;
use anyhow::bail;
use clap::Parser;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use xil_audio::fx::py_repr;
use xil_core::fsutil::basename;
use xil_core::pyfmt::{commas, pad_right, round_to};
use xil_core::pyjson::{dumps, float_repr, py_float, Style};
use xil_core::workspace::{derive_paths, resolve_slug, resolve_venv_python, workspace_root};
use xil_core::{banner, log};

use crate::mix::config::{CastConfig, Num, SfxConfig};
use crate::sfxgen;
use crate::tts::{self, Flavor};

#[derive(Parser)]
#[command(
    name = "xil-produce",
    about = "Voice Generation — generate voice stems via ElevenLabs"
)]
#[command(group(clap::ArgGroup::new("tag_group").required(true).args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S01E01) — derives cast and SFX config paths
    #[arg(long)]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Path to parsed script JSON (default: derived from cast config)
    #[arg(long)]
    script: Option<String>,
    /// Preview all lines and TTS cost without API calls
    #[arg(long)]
    dry_run: bool,
    /// Re-link existing stems to new seq filenames after a re-parse (reads stem manifest, proposes renames, no TTS calls). Add --apply to execute.
    #[arg(long)]
    reconcile: bool,
    /// With --reconcile: execute the renames instead of dry-run preview.
    #[arg(long)]
    apply: bool,
    /// Overwrite existing stem files instead of skipping them. Use with --start-from/--stop-at or --seq-list to regenerate specific lines. WARNING: incurs ElevenLabs API cost for every stem in range.
    #[arg(long)]
    force: bool,
    /// Start generation from sequence number N (for resuming)
    #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
    start_from: i64,
    /// Stop generation at sequence number N, inclusive (for previewing a section)
    #[arg(long, allow_negative_numbers = true)]
    stop_at: Option<i64>,
    /// Comma-separated list of exact sequence numbers to process (e.g. "12,45,88,203"), for regenerating a specific non-contiguous set of dialogue lines in one run — the TTS worker/subprocess is started once and reused across all listed seqs, instead of once per --start-from/--stop-at invocation. Takes precedence over --start-from/--stop-at when given. Dialogue only — does not affect --gen-sfx/--gen-music/--gen-ambience filtering. Combine with --force to actually overwrite stems that already exist on disk.
    #[arg(long, value_name = "N,N,...", value_parser = parse_seq_list, allow_hyphen_values = true)]
    seq_list: Option<BTreeSet<i64>>,
    /// Truncate each line to 3 words to minimize TTS character cost
    #[arg(long)]
    terse: bool,
    /// Generate SFX and BEAT stems
    #[arg(long)]
    gen_sfx: bool,
    /// Generate music stems
    #[arg(long)]
    gen_music: bool,
    /// Generate ambience stems
    #[arg(long)]
    gen_ambience: bool,
    /// (deprecated) shorthand for --gen-sfx --gen-music --gen-ambience
    #[arg(long)]
    sfx_music: bool,
    /// Only place stems for effects already present in SFX/; skip API generation
    #[arg(long)]
    local_only: bool,
    /// TTS backend for dialogue voice stems.
    #[arg(long, default_value = "elevenlabs", value_parser = ["elevenlabs", "gtts", "chatterbox", "chatterbox-turbo"], value_name = "BACKEND")]
    backend: String,
    /// Path to the Python executable in the chatterbox venv (default: auto-detect ./venv-chatterbox/bin/python3). Used with --backend chatterbox or chatterbox-turbo.
    #[arg(long, value_name = "PATH")]
    chatterbox_python: Option<String>,
    /// Directory of <speaker_key>.wav reference clips for Chatterbox zero-shot voice cloning (default: <workspace>/voice_refs/). Missing refs fall back to Chatterbox's default voice.
    #[arg(long, value_name = "DIR")]
    voice_refs: Option<String>,
    /// Device for --backend chatterbox-turbo (default: cuda).
    #[arg(long, default_value = "cuda", value_parser = ["cuda", "cpu"], value_name = "DEVICE")]
    device: String,
    /// Backend for SFX/music/ambience generation, independent of the dialogue --backend.
    #[arg(long, default_value = "elevenlabs", value_parser = ["elevenlabs", "mmaudio"], value_name = "BACKEND")]
    sfx_backend: String,
    /// Path to the Python executable in the MMAudio venv (default: auto-detect ./venv-mmaudio/bin/python3). Used only with --sfx-backend mmaudio.
    #[arg(long, value_name = "PATH")]
    mmaudio_python: Option<String>,
    /// Generation length in seconds before trimming (default: 8.0, MMAudio's training duration).
    #[arg(
        long,
        default_value_t = 8.0,
        value_name = "FLOAT",
        allow_negative_numbers = true
    )]
    mmaudio_duration: f64,
    /// MMAudio classifier-free guidance strength (default: 4.5).
    #[arg(
        long,
        default_value_t = 4.5,
        value_name = "FLOAT",
        allow_negative_numbers = true
    )]
    mmaudio_cfg: f64,
    /// MMAudio flow-matching sampling steps (default: 25).
    #[arg(
        long,
        default_value_t = 25,
        value_name = "INT",
        allow_negative_numbers = true
    )]
    mmaudio_steps: i64,
    /// Optional MMAudio negative prompt (default: none).
    #[arg(long, default_value = "", value_name = "STR")]
    mmaudio_negative_prompt: String,
    /// MMAudio reproducibility seed (default: nondeterministic).
    #[arg(long, value_name = "INT", allow_negative_numbers = true)]
    mmaudio_seed: Option<i64>,
    /// Acknowledge that MMAudio's weights are CC BY-NC 4.0 (NON-COMMERCIAL USE ONLY) and that generated audio must not appear in a monetised production. Required for --sfx-backend mmaudio.
    #[arg(long)]
    mmaudio_accept_noncommercial: bool,
}

/// `_parse_seq_list`: comma-separated ints, blanks ignored.
fn parse_seq_list(s: &str) -> Result<BTreeSet<i64>, String> {
    let tokens: Vec<&str> = s
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return Err(format!("no sequence numbers found in {}", py_repr(s)));
    }
    tokens
        .iter()
        .map(|t| {
            t.replace('_', "")
                .parse::<i64>()
                .map_err(|_| format!("invalid sequence number list: {}", py_repr(s)))
        })
        .collect()
}

/// A speaker's config dict (`VoiceConfig` plus the voice-setting fields).
struct SpeakerCfg {
    id: String,
    full_name: String,
    stability: Option<f64>,
    similarity_boost: Option<f64>,
    style: Option<f64>,
    use_speaker_boost: Option<bool>,
    language_code: Option<String>,
    speed: Option<f64>,
}

struct Dialogue {
    speaker: String,
    text: String,
    stem_name: String,
    seq: i64,
    section: Option<String>,
    direction: Option<String>,
}

/// `load_production(script, cast)` → `(config, dialogue, tag)`.
fn load_production(
    script: &Path,
    cast_path: &Path,
) -> anyhow::Result<(IndexMap<String, SpeakerCfg>, Vec<Dialogue>, String)> {
    if !cast_path.exists() {
        bail!(
            "FileNotFoundError: Cast config not found: {}\nRun XILP001 first or check your --episode flag.",
            cast_path.display()
        );
    }
    if !script.exists() {
        bail!(
            "FileNotFoundError: Parsed script not found: {}\nRun XILP001 first or check your --script flag.",
            script.display()
        );
    }
    let data: Value = serde_json::from_str(&fs::read_to_string(script)?)?;
    let cast = CastConfig::load(cast_path)?;
    let mut config = IndexMap::new();
    for (key, v) in &cast.cast {
        let f = |k: &str| v.raw.get(k).and_then(Num::from_value).map(Num::f);
        config.insert(
            key.clone(),
            SpeakerCfg {
                id: v.voice_id.clone(),
                full_name: v.full_name.clone(),
                stability: f("stability"),
                similarity_boost: f("similarity_boost"),
                style: f("style"),
                use_speaker_boost: v.raw.get("use_speaker_boost").and_then(Value::as_bool),
                language_code: v
                    .raw
                    .get("language_code")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                speed: f("speed"),
            },
        );
    }
    let mut dialogue = Vec::new();
    for e in data["entries"].as_array().into_iter().flatten() {
        if e.get("type").and_then(Value::as_str) != Some("dialogue") {
            continue;
        }
        let seq = e["seq"].as_i64().unwrap_or(0);
        let section = e.get("section").cloned().unwrap_or(Value::Null);
        let mut stem = format!("{seq:03}_{}", py_str(&section));
        if let Some(scene) = e
            .get("scene")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            stem.push('-');
            stem.push_str(scene);
        }
        let speaker = e.get("speaker").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!(
                "1 validation error for DialogueEntry\nspeaker\n  Input should be a valid string"
            )
        })?;
        stem.push('_');
        stem.push_str(speaker);
        dialogue.push(Dialogue {
            speaker: speaker.to_string(),
            text: e
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            stem_name: stem,
            seq,
            section: section.as_str().map(str::to_string),
            direction: e
                .get("direction")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok((config, dialogue, cast.tag))
}

fn in_selection(seq: i64, start: i64, stop: Option<i64>, list: Option<&BTreeSet<i64>>) -> bool {
    match list {
        Some(l) => l.contains(&seq),
        None => seq >= start && stop.map_or(true, |s| seq <= s),
    }
}

fn format_seq_list(list: &BTreeSet<i64>) -> String {
    let v: Vec<String> = list.iter().map(|s| s.to_string()).collect();
    if v.len() <= 10 {
        v.join(",")
    } else {
        format!("{},... and {} more", v[..10].join(","), v.len() - 10)
    }
}

// ── ElevenLabs quota helpers (the producer's wording) ───────────────────────

fn subscription(
    client: &xil_api::elevenlabs::Client,
) -> anyhow::Result<Option<(i64, i64, String)>> {
    match client.user_get() {
        Ok(u) => {
            let s = &u["subscription"];
            Ok(Some((
                s["character_count"].as_i64().unwrap_or(0),
                s["character_limit"].as_i64().unwrap_or(0),
                s["tier"].as_str().unwrap_or("").to_string(),
            )))
        }
        Err(xil_api::ApiError::Status { status, body }) => {
            log::debug(&format!("user.get failed: {status} {body}"));
            Ok(None)
        }
        Err(e) => bail!("httpx.TransportError: {e}"),
    }
}

fn check_quota(client: &xil_api::elevenlabs::Client) -> anyhow::Result<()> {
    match client.user_get() {
        Ok(u) => {
            let s = &u["subscription"];
            let (used, limit) = (
                s["character_count"].as_i64().unwrap_or(0),
                s["character_limit"].as_i64().unwrap_or(0),
            );
            log::info(&format!("\n{}", "=".repeat(40)));
            log::info("ELEVENLABS API STATUS:");
            log::info(&format!(
                "  Tier:      {}",
                s["tier"].as_str().unwrap_or("").to_uppercase()
            ));
            log::info(&format!(
                "  Usage:     {} / {} characters",
                commas(used),
                commas(limit)
            ));
            log::info(&format!("  Remaining: {}", commas(limit - used)));
            log::info(&format!("{}\n", "=".repeat(40)));
        }
        Err(xil_api::ApiError::Status { status, body }) => {
            log::warning("API Error: Unable to fetch user subscription data.");
            log::warning(&format!("    Details: status_code: {status}, body: {body}"));
        }
        Err(e) => bail!("httpx.TransportError: {e}"),
    }
    Ok(())
}

fn best_model(client: &xil_api::elevenlabs::Client) -> anyhow::Result<String> {
    match subscription(client)? {
        Some((used, limit, _)) => {
            let remaining = limit - used;
            if remaining > 5000 {
                log::info(&format!(
                    " [Budget] Healthy Balance: {} left. Using 'eleven_v3'.",
                    commas(remaining)
                ));
            } else {
                log::warning(&format!(
                    " [Budget] LOW BALANCE: {} left. Continuing with 'eleven_v3' — audio tags like [pause] require v3 and cannot fall back to flash.",
                    commas(remaining)
                ));
            }
        }
        None => log::info(" [Budget] API Check Failed. Defaulting to 'eleven_v3'."),
    }
    Ok("eleven_v3".into())
}

fn has_enough(client: &xil_api::elevenlabs::Client, text: &str) -> anyhow::Result<bool> {
    let required = text.chars().count() as i64;
    match subscription(client)? {
        Some((used, limit, _)) => {
            let remaining = limit - used;
            if remaining >= required {
                log::info(&format!(
                    " [Guard] Quota OK: {required} required, {} left.",
                    commas(remaining)
                ));
                Ok(true)
            } else {
                log::info(&format!(
                    " [Guard] STOP: Line requires {required} chars, but only {} remain.",
                    commas(remaining)
                ));
                Ok(false)
            }
        }
        None => {
            log::warning(" [Guard] Permission 'user_read' missing. Skipping quota check.");
            Ok(true)
        }
    }
}

// ── Stem manifest ───────────────────────────────────────────────────────────

type Key = (String, String, u64, u64, u64, String);

fn content_key(
    text: &str,
    voice_id: &str,
    speed: Option<f64>,
    stability: Option<f64>,
    similarity: Option<f64>,
    backend: &str,
) -> Key {
    let or = |v: Option<f64>, d: f64| round_to(v.filter(|x| *x != 0.0).unwrap_or(d), 4).to_bits();
    (
        text.to_string(),
        voice_id.to_string(),
        or(speed, 1.0),
        or(stability, 0.5),
        or(similarity, 0.75),
        backend.to_string(),
    )
}

fn entry_key(m: &Map<String, Value>) -> Key {
    let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let f = |k: &str| m.get(k).and_then(Value::as_f64);
    content_key(
        &s("text"),
        &s("voice_id"),
        f("speed"),
        f("stability"),
        f("similarity_boost"),
        &s("backend"),
    )
}

fn manifest_path(stems_dir: &Path) -> PathBuf {
    stems_dir.join(format!("{}_stem_manifest.json", basename(stems_dir)))
}

fn load_manifest(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({"version": 1, "entries": []}))
}

fn save_manifest(path: &Path, manifest: &Value) -> std::io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&tmp, dumps(manifest, Style::INDENT2))?;
    fs::rename(&tmp, path)
}

fn has_key(manifest: &Value, key: &Key) -> bool {
    manifest["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .any(|m| &entry_key(m) == key)
}

fn upsert(manifest: &mut Value, entry: Map<String, Value>) {
    let key = entry_key(&entry);
    let list = manifest
        .get_mut("entries")
        .and_then(Value::as_array_mut)
        .expect("manifest entries");
    for existing in list.iter_mut().rev() {
        if let Some(o) = existing.as_object_mut() {
            if entry_key(o) == key {
                for (k, v) in entry {
                    o.insert(k, v);
                }
                return;
            }
        }
    }
    list.push(Value::Object(entry));
}

fn opt_f(v: Option<f64>, default: Option<f64>) -> Value {
    // `cfg.get("speed", 1.0)`: the key always exists, so a None stays None.
    let _ = default;
    v.map(py_float).unwrap_or(Value::Null)
}

fn manifest_entry(
    d: &Dialogue,
    cfg: Option<&SpeakerCfg>,
    backend: &str,
    model: &str,
    sha: String,
    stem: &Path,
    generated_at: String,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("text".into(), d.text.clone().into());
    m.insert("speaker".into(), d.speaker.clone().into());
    match cfg {
        Some(c) => {
            m.insert("voice_id".into(), c.id.clone().into());
            m.insert("speed".into(), opt_f(c.speed, Some(1.0)));
            m.insert("stability".into(), opt_f(c.stability, Some(0.5)));
            m.insert(
                "similarity_boost".into(),
                opt_f(c.similarity_boost, Some(0.75)),
            );
        }
        None => {
            m.insert("voice_id".into(), "".into());
            m.insert("speed".into(), py_float(1.0));
            m.insert("stability".into(), py_float(0.5));
            m.insert("similarity_boost".into(), py_float(0.75));
        }
    }
    m.insert("backend".into(), backend.into());
    m.insert("model".into(), model.into());
    m.insert("sha256".into(), sha.into());
    m.insert("seq_at_generation".into(), d.seq.into());
    m.insert("stem_filename".into(), basename(stem).into());
    m.insert("generated_at".into(), generated_at.into());
    m
}

fn cfg_key(d: &Dialogue, cfg: Option<&SpeakerCfg>, backend: &str) -> Key {
    match cfg {
        Some(c) => content_key(
            &d.text,
            &c.id,
            c.speed,
            c.stability,
            c.similarity_boost,
            backend,
        ),
        None => content_key(&d.text, "", Some(1.0), Some(0.5), Some(0.75), backend),
    }
}

// ── Dry run, voice refs, reconcile ──────────────────────────────────────────

struct DryRunOpts<'a> {
    start: i64,
    stop: Option<i64>,
    list: Option<&'a BTreeSet<i64>>,
    stems_dir: &'a Path,
    force: bool,
    backend: &'a str,
}

fn dry_run(
    config: &IndexMap<String, SpeakerCfg>,
    dialogue: &[Dialogue],
    o: &DryRunOpts,
    sfx: Option<(&[sfxgen::PlanEntry], &SfxConfig, &Path, &str)>,
) {
    let bar = "=".repeat(70);
    log::info(&format!("\n{bar}"));
    log::info(&format!("DRY RUN — {} dialogue lines", dialogue.len()));
    log::info(&bar);
    log::info(&format!(
        " [.] {:<3} | {:<14} | {:>10} | voice check [lang]",
        "seq", "speaker", "chars"
    ));
    log::info(&format!(" {}", "-".repeat(67)));
    let mut total_chars = 0i64;
    let mut to_generate = 0;
    let mut gen: IndexMap<String, (i64, i64)> = IndexMap::new();
    let mut skip: IndexMap<String, (i64, i64)> = IndexMap::new();
    for e in dialogue {
        let chars = e.text.chars().count() as i64;
        total_chars += chars;
        let in_range = in_selection(e.seq, o.start, o.stop, o.list);
        let exists = !o.force && o.stems_dir.join(format!("{}.mp3", e.stem_name)).exists();
        let marker = if exists {
            let b = skip.entry(e.speaker.clone()).or_default();
            b.0 += 1;
            b.1 += chars;
            "="
        } else if in_range {
            let b = gen.entry(e.speaker.clone()).or_default();
            b.0 += 1;
            b.1 += chars;
            to_generate += 1;
            " "
        } else {
            "x"
        };
        let direction = e
            .direction
            .as_ref()
            .filter(|d| !d.is_empty())
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        let preview = if e.text.chars().count() > 75 {
            format!("{}...", e.text.chars().take(75).collect::<String>())
        } else {
            e.text.clone()
        };
        let cfg = config.get(&e.speaker);
        let voice_id = cfg.map(|c| c.id.as_str()).unwrap_or("???");
        let status = if voice_id == "TBD" && o.backend == "elevenlabs" {
            "TBD"
        } else {
            "OK"
        };
        let mut parts = Vec::new();
        if let Some(c) = cfg {
            for (k, v) in [
                ("stability", c.stability),
                ("similarity_boost", c.similarity_boost),
                ("style", c.style),
            ] {
                if let Some(x) = v {
                    parts.push(format!("{k}={}", float_repr(x)));
                }
            }
            if c.use_speaker_boost == Some(true) {
                parts.push("speaker_boost".into());
            }
            if let Some(l) = c.language_code.as_ref().filter(|l| !l.is_empty()) {
                parts.push(format!("lang={l}"));
            }
        }
        let vs_note = if parts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", parts.join(", "))
        };
        log::info(&format!(
            " [{marker}] {:03} | {} | {chars:4} chars | voice: {status}{vs_note}{direction}",
            e.seq,
            pad_right(&e.speaker, 14)
        ));
        log::info(&format!("          {preview}"));
        log::info(&format!("          stem: {}.mp3", e.stem_name));
        log::info("");
    }
    if let Some((entries, cfg, stems_dir, backend)) = sfx {
        sfxgen::dry_run_sfx(entries, cfg, o.stems_dir, stems_dir, backend);
    }
    let chars_in_range: i64 = dialogue
        .iter()
        .filter(|e| in_selection(e.seq, o.start, o.stop, o.list))
        .map(|e| e.text.chars().count() as i64)
        .sum();
    let tbd: Vec<&str> = if o.backend == "elevenlabs" {
        config
            .iter()
            .filter(|(_, c)| c.id == "TBD")
            .map(|(k, _)| k.as_str())
            .collect()
    } else {
        Vec::new()
    };
    log::info(&bar);
    log::info(&format!(
        "TOTAL:  {} lines, {} TTS characters",
        dialogue.len(),
        commas(total_chars)
    ));
    let range_label = if let Some(l) = o.list {
        Some(format!("SEQ-LIST {}", format_seq_list(l)))
    } else if o.start > 1 || o.stop.is_some() {
        Some(match (o.start > 1, o.stop) {
            (true, Some(s)) => format!("FROM {}–{s}", o.start),
            (false, Some(s)) => format!("THRU {s}"),
            _ => format!("FROM {}", o.start),
        })
    } else {
        None
    };
    if let Some(label) = range_label {
        log::info(&format!(
            "{label}: {to_generate} lines, {} TTS characters",
            commas(chars_in_range)
        ));
    }
    if !tbd.is_empty() {
        log::warning(&format!(
            "\n  {} voices still need voice_id assignment: {}",
            tbd.len(),
            tbd.join(", ")
        ));
        log::info(
            "  Use XILU001_discover_voices_T2S.py to browse voices, then update the cast config",
        );
    }
    log::info(&format!("{bar}\n"));

    if !gen.is_empty() {
        let mut rows: Vec<&String> = gen.keys().collect();
        rows.sort_by(|a, b| gen[*b].1.cmp(&gen[*a].1));
        let mut skip_only: Vec<&String> = skip.keys().filter(|s| !gen.contains_key(*s)).collect();
        skip_only.sort_by(|a, b| skip[*b].1.cmp(&skip[*a].1));
        rows.extend(skip_only);
        let sep = format!(
            "{}  {}  {}      {}  {}",
            "-".repeat(16),
            "-".repeat(5),
            "-".repeat(8),
            "-".repeat(5),
            "-".repeat(8)
        );
        log::info("SPEAKER COST BREAKDOWN  ([ ]=generate  [=]=skip  [x]=out of range)");
        log::info(&format!(
            "{:<16}  {:>5}  {:>8}      {:>5}  {:>8}",
            "Speaker", "Lines", "Chars", "Lines", "Chars"
        ));
        log::info(&format!(
            "{:<16}  {:>5}  {:>8}      {:>5}  {:>8}",
            "", "gen", "gen", "skip", "skip"
        ));
        log::info(&sep);
        for spk in rows {
            let g = gen.get(spk).copied().unwrap_or_default();
            let s = skip.get(spk).copied().unwrap_or_default();
            log::info(&format!(
                "{}  {:>5}  {:>8}      {:>5}  {:>8}",
                pad_right(spk, 16),
                g.0,
                commas(g.1),
                s.0,
                commas(s.1)
            ));
        }
        log::info(&sep);
        let (gl, gc) = gen.values().fold((0, 0), |a, v| (a.0 + v.0, a.1 + v.1));
        let (sl, sc) = skip.values().fold((0, 0), |a, v| (a.0 + v.0, a.1 + v.1));
        log::info(&format!(
            "{:<16}  {gl:>5}  {:>8}      {sl:>5}  {:>8}",
            "TOTAL",
            commas(gc),
            commas(sc)
        ));
        log::info("");
    }
}

fn print_voice_refs_table(config: &IndexMap<String, SpeakerCfg>, dir: &str) {
    let mut speakers: Vec<&String> = config.keys().collect();
    speakers.sort();
    let has = |s: &str| {
        Path::new(dir).join(format!("{s}.wav")).exists()
            || Path::new(dir).join(format!("{s}.conds.pt")).exists()
    };
    let missing: Vec<&str> = speakers
        .iter()
        .filter(|s| !has(s))
        .map(|s| s.as_str())
        .collect();
    log::info(&format!("VOICE REFS  ({dir})"));
    log::info(&format!("  {:<18}  Ref", "Speaker"));
    log::info(&format!("  {}", "-".repeat(30)));
    for s in &speakers {
        let mark = if has(s) {
            "✓"
        } else {
            "✗  (fallback to default voice)"
        };
        log::info(&format!("  {}  {mark}", pad_right(s, 18)));
    }
    log::info(&format!("  {}", "-".repeat(30)));
    log::info(&format!(
        "  {} / {} have voice refs",
        speakers.len() - missing.len(),
        speakers.len()
    ));
    if !missing.is_empty() {
        log::warning(&format!("  Missing refs: {}", missing.join(", ")));
        log::warning(&format!(
            "  Add <key>.wav to {dir} to use a reference voice."
        ));
    }
    log::info("");
}

fn reconcile(
    config: &IndexMap<String, SpeakerCfg>,
    dialogue: &[Dialogue],
    stems_dir: &Path,
    backend: &str,
    apply: bool,
) -> anyhow::Result<()> {
    let mf = manifest_path(stems_dir);
    if !mf.exists() {
        log::error(&format!(
            "No stem manifest at {} — run 'xil produce' first to build it.",
            mf.display()
        ));
        return Ok(());
    }
    let mut manifest = load_manifest(&mf);
    let entries: Vec<Map<String, Value>> = manifest["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_object().cloned())
        .collect();
    // key → index of the last entry with that key (dict assignment).
    let mut by_key: IndexMap<Key, usize> = IndexMap::new();
    for (i, m) in entries.iter().enumerate() {
        by_key.insert(entry_key(m), i);
    }
    let mut to_rename: Vec<(PathBuf, PathBuf, usize, &Dialogue)> = Vec::new();
    let mut to_generate: Vec<(&Dialogue, String)> = Vec::new();
    let mut correct = 0;
    for d in dialogue {
        let key = cfg_key(d, config.get(&d.speaker), backend);
        let expected = stems_dir.join(format!("{}.mp3", d.stem_name));
        if expected.exists() {
            correct += 1;
            continue;
        }
        let Some(&idx) = by_key.get(&key) else {
            to_generate.push((d, "not in manifest".into()));
            continue;
        };
        let fname = py_str(entries[idx].get("stem_filename").unwrap_or(&Value::Null));
        let current = stems_dir.join(&fname);
        if !current.exists() {
            to_generate.push((d, format!("manifest file missing: {fname}")));
            continue;
        }
        let actual = crate::cmd::mp3_hash::hash_file(&current).ok();
        if actual.as_deref() != entries[idx].get("sha256").and_then(Value::as_str) {
            to_generate.push((d, format!("SHA-256 mismatch for {fname}")));
            continue;
        }
        to_rename.push((current, expected, idx, d));
    }
    log::info(&format!(
        "--- Reconcile: {correct} correct, {} to re-link, {} need new TTS ---",
        to_rename.len(),
        to_generate.len()
    ));
    for (src, dst, _, _) in &to_rename {
        log::info(&format!("  RELINK  {} → {}", basename(src), basename(dst)));
    }
    for (d, reason) in &to_generate {
        log::info(&format!(
            "  MISSING seq {:03} {}: {reason}",
            d.seq, d.speaker
        ));
    }
    if !apply {
        log::info("  (dry-run — pass --apply to execute renames)");
        return Ok(());
    }
    for (src, dst, idx, d) in &to_rename {
        fs::rename(src, dst)?;
        let m = manifest["entries"][*idx].as_object_mut().expect("entry");
        m.insert("stem_filename".into(), basename(dst).into());
        m.insert("seq_at_generation".into(), d.seq.into());
        log::info(&format!("  Relinked: {}", basename(dst)));
    }
    if !to_rename.is_empty() {
        save_manifest(&mf, &manifest)?;
        log::info(&format!("  Manifest updated: {}", basename(&mf)));
    }
    Ok(())
}

// ── Generation ──────────────────────────────────────────────────────────────

struct GenOpts<'a> {
    start: i64,
    stop: Option<i64>,
    list: Option<&'a BTreeSet<i64>>,
    show: &'a str,
    backend: &'a str,
    force: bool,
    speed_overrides: &'a IndexMap<&'static str, f64>,
}

/// `text` still has something to say once `[tags]` and punctuation go.
fn speakable(text: &str) -> bool {
    static TAG: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\[[^\]]*\]").unwrap());
    static PUNCT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[^\w\s]").unwrap());
    let s = TAG.replace_all(text, "");
    !PUNCT.replace_all(&s, "").trim().is_empty()
}

fn generate_voices(
    config: &IndexMap<String, SpeakerCfg>,
    dialogue: &[Dialogue],
    stems_dir: &Path,
    o: &GenOpts,
    client: &xil_api::elevenlabs::Client,
    mut chatterbox: Option<&mut tts::Chatterbox>,
) -> anyhow::Result<()> {
    fs::create_dir_all(stems_dir)?;
    let run_started_at = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let mf = manifest_path(stems_dir);
    let mut manifest = load_manifest(&mf);

    if o.backend == "elevenlabs" {
        let mut needed: Vec<&str> = dialogue
            .iter()
            .filter(|e| in_selection(e.seq, o.start, o.stop, o.list))
            .map(|e| e.speaker.as_str())
            .filter(|s| config.get(*s).is_some_and(|c| c.id == "TBD"))
            .collect();
        needed.sort();
        needed.dedup();
        if !needed.is_empty() {
            log::error(&format!(
                "Cannot generate: {} speaker(s) in range have no voice_id: {}\n  Assign voice IDs in the cast config, then re-run.",
                needed.len(),
                needed.join(", ")
            ));
            return Ok(());
        }
    }
    let todo: Vec<&Dialogue> = dialogue
        .iter()
        .filter(|e| in_selection(e.seq, o.start, o.stop, o.list))
        .collect();
    let range_note = if let Some(l) = o.list {
        format!(" (seq list: {})", format_seq_list(l))
    } else if let Some(s) = o.stop {
        format!(" (seq {}–{s})", o.start)
    } else if o.start > 1 {
        format!(" (from seq {})", o.start)
    } else {
        String::new()
    };
    log::info(&format!(
        "--- Phase 1: Generating {} voice stems{range_note} ---",
        todo.len()
    ));
    let current_model = best_model(client)?;
    let mut generated = 0;

    for e in todo {
        let tts_comment = if o.backend == "elevenlabs" {
            current_model.clone()
        } else {
            o.backend.to_string()
        };
        let stem = stems_dir.join(format!("{}.mp3", e.stem_name));
        let cfg = config.get(&e.speaker);
        if fs::metadata(&stem).map(|m| m.len() > 0).unwrap_or(false) {
            if !o.force {
                log::info(&format!("   Exists: {} — skipping", stem.display()));
                let key = cfg_key(e, cfg, o.backend);
                if !has_key(&manifest, &key) {
                    if let Ok(sha) = crate::cmd::mp3_hash::hash_file(&stem) {
                        upsert(
                            &mut manifest,
                            manifest_entry(
                                e,
                                cfg,
                                o.backend,
                                &tts_comment,
                                sha,
                                &stem,
                                String::new(),
                            ),
                        );
                    }
                }
                continue;
            }
            log::warning(&format!("   Force: overwriting {}", basename(&stem)));
        }
        if o.backend == "elevenlabs" && cfg.is_some_and(|c| c.id == "TBD") {
            log::warning(&format!(
                "No voice_id for {} — skipping {}",
                e.speaker, e.stem_name
            ));
            continue;
        }
        if o.backend == "elevenlabs" && !has_enough(client, &e.text)? {
            log::info(&format!(
                " !!! Production halted at seq {} to save credits.",
                e.seq
            ));
            break;
        }
        if !speakable(&e.text) {
            log::warning(&format!(
                "   SKIP seq {} ({}): text {} is empty after stripping speaker tags/emojis — convert this entry to a direction or replace the text in the script.",
                e.seq,
                e.speaker,
                py_repr(&e.text)
            ));
            continue;
        }
        // A section override rebinds `cfg` to a copy with the new speed — for
        // the request and for the manifest entry written afterwards.
        let override_speed = e
            .section
            .as_deref()
            .and_then(|sec| o.speed_overrides.get(sec).copied());
        let speed = override_speed.or_else(|| cfg.and_then(|c| c.speed));
        let mut vs = Map::new();
        if let Some(c) = cfg {
            if let Some(x) = c.stability {
                vs.insert("stability".into(), py_float(x));
            }
            if let Some(x) = c.similarity_boost {
                vs.insert("similarity_boost".into(), py_float(x));
            }
            if let Some(x) = c.style {
                vs.insert("style".into(), py_float(x));
            }
            if let Some(b) = c.use_speaker_boost {
                vs.insert("use_speaker_boost".into(), b.into());
            }
        }
        if let Some(x) = speed {
            vs.insert("speed".into(), py_float(x));
        }
        let voice_settings: Option<Value> = (!vs.is_empty()).then_some(Value::Object(vs));

        let tmp = tempfile::Builder::new()
            .prefix("tmp")
            .rand_bytes(8)
            .suffix(".mp3.tmp")
            .tempfile_in(stems_dir)?
            .into_temp_path();
        let rendered = (|| -> anyhow::Result<()> {
            let chars = e.text.chars().count();
            match o.backend {
                "gtts" => {
                    log::info(&format!(
                        " > [{:03}] {} via gTTS ({chars} chars)...",
                        e.seq, e.speaker
                    ));
                    tts::gtts_generate(&e.text, &tmp, Flavor::Produce)?;
                }
                "chatterbox-turbo" => {
                    log::info(&format!(
                        " > [{:03}] {} via Chatterbox Turbo ({chars} chars)...",
                        e.seq, e.speaker
                    ));
                    chatterbox
                        .as_deref_mut()
                        .expect("chatterbox client")
                        .generate(&e.text, &tmp, &e.speaker)?;
                }
                _ => {
                    log::info(&format!(
                        " > [{:03}] {} with {current_model} ({chars} chars)...",
                        e.seq, e.speaker
                    ));
                    let Some(c) = cfg else {
                        bail!("KeyError: {}", py_repr(&e.speaker));
                    };
                    let audio = client
                        .text_to_speech(
                            &c.id,
                            &e.text,
                            &current_model,
                            "mp3_44100_128",
                            Some(voice_settings.clone().unwrap_or(Value::Null)),
                        )
                        .map_err(|err| {
                            anyhow::anyhow!("elevenlabs.core.api_error.ApiError: {err}")
                        })?;
                    fs::write(&tmp, audio)?;
                }
            }
            if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
                bail!(
                    "RuntimeError: TTS produced an empty file for seq {} ({}) via {}",
                    e.seq,
                    e.speaker,
                    o.backend
                );
            }
            Ok(())
        })();
        if let Err(err) = rendered {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        fs::rename(&tmp, &stem)?;
        let _ = tmp.keep();

        let full_name = cfg
            .map(|c| c.full_name.clone())
            .unwrap_or_else(|| super::parse::title_case(&e.speaker));
        let first_five: Vec<&str> = e.text.split_whitespace().take(5).collect();
        xil_audio::tags::tag_mp3_full(
            &stem,
            o.show,
            Some(&format!("{full_name}: {}", first_five.join(" "))),
            Some(&full_name),
            Some(&e.text),
            Some(&tts_comment),
            None,
        )?;
        log::info(&format!("   Saved: {}", stem.display()));
        if let Ok(sha) = crate::cmd::mp3_hash::hash_file(&stem) {
            log::info(&format!("   SHA256: {sha}"));
            let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            let mut entry = manifest_entry(e, cfg, o.backend, &tts_comment, sha, &stem, now);
            if let Some(sp) = override_speed {
                entry.insert("speed".into(), py_float(sp));
                if cfg.is_none() {
                    entry.insert("stability".into(), Value::Null);
                    entry.insert("similarity_boost".into(), Value::Null);
                }
            }
            upsert(&mut manifest, entry);
        }
        generated += 1;
    }
    let stem_count = fs::read_dir(stems_dir)?
        .filter_map(Result::ok)
        .filter(|d| d.file_name().to_string_lossy().ends_with(".mp3"))
        .count();
    log::info(&format!(
        "--- Phase 1 Complete: {generated} new, {stem_count} total stems in {}/ ---",
        stems_dir.display()
    ));
    let n = manifest["entries"].as_array().map_or(0, Vec::len);
    match save_manifest(&mf, &manifest) {
        Ok(()) => {
            log::info(&format!("   Manifest: {} ({n} entries)", basename(&mf)));
            let snap = PathBuf::from(
                mf.to_string_lossy()
                    .replace(".json", &format!("_{run_started_at}.json")),
            );
            match save_manifest(&snap, &manifest) {
                Ok(()) => log::info(&format!("   Snapshot: {}", basename(&snap))),
                Err(err) => log::warning(&format!("Could not write stem manifest: {err}")),
            }
        }
        Err(err) => log::warning(&format!("Could not write stem manifest: {err}")),
    }
    Ok(())
}

/// `sfx_common.sfx_dir(slug)`: the per-show library when it exists.
fn sfx_dir_for(slug: &str) -> PathBuf {
    let per_show = workspace_root().join("SFX").join(slug);
    if per_show.is_dir() {
        per_show
    } else {
        workspace_root().join("SFX")
    }
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("produce");
    let result = {
        let _banner = banner::begin(super::prog(), &super::argv_line(args));
        execute(args)
    };
    match result {
        Err(e)
            if e.downcast_ref::<super::SysExit>()
                .is_some_and(|s| s.0.is_empty()) =>
        {
            Ok(1)
        }
        other => super::finish(other),
    }
}

fn execute(args: &[OsString]) -> anyhow::Result<i32> {
    let mut a: Args = match super::parse_or_exit("xil-produce", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    if a.backend == "chatterbox" {
        log::warning("--backend chatterbox was removed; using chatterbox-turbo instead. Stems will be recorded as chatterbox-turbo.");
        a.backend = "chatterbox-turbo".into();
    }
    let sfx_requested = a.gen_sfx || a.gen_music || a.gen_ambience || a.sfx_music;
    let needs_key = a.backend == "elevenlabs" || (a.sfx_backend == "elevenlabs" && sfx_requested);
    let key = std::env::var("ELEVENLABS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    if !a.dry_run && needs_key && key.is_none() {
        return Err(super::SysExit(
            "Error: ELEVENLABS_API_KEY environment variable is not set.".into(),
        )
        .into());
    }
    let arg_tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let paths = derive_paths(&slug, &arg_tag);
    let cast_path = paths["cast"].clone();
    let sfx_path = paths["sfx"].clone();
    if !cast_path.exists() {
        return Err(super::SysExit(format!(
            "Error: Cast config not found: {}\nRun XILP001 first or check your --episode flag.",
            cast_path.display()
        ))
        .into());
    }
    let cast_doc = CastConfig::load(&cast_path)?;
    let raw_cast: Value = serde_json::from_str(&fs::read_to_string(&cast_path)?)?;
    let script = a
        .script
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| paths["parsed"].clone());

    let (config, mut dialogue, tag) = load_production(&script, &cast_path)?;
    let stems_dir = workspace_root().join("stems").join(&slug).join(&tag);
    if a.terse {
        for d in &mut dialogue {
            d.text = d
                .text
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
    let mut sfx_cfg: Option<SfxConfig> = None;

    let gen_sfx = a.gen_sfx || a.sfx_music;
    let gen_music = a.gen_music || a.sfx_music;
    let gen_ambience = a.gen_ambience || a.sfx_music;
    let mut sfx_entries: Option<Vec<sfxgen::PlanEntry>> = None;
    if gen_sfx || gen_music || gen_ambience {
        let mut types = std::collections::HashSet::new();
        if gen_sfx {
            types.insert("SFX");
            types.insert("BEAT");
        }
        if gen_music {
            types.insert("MUSIC");
        }
        if gen_ambience {
            types.insert("AMBIENCE");
            types.insert("VINTAGE FILTER");
        }
        if !sfx_path.exists() {
            bail!(
                "FileNotFoundError: [Errno 2] No such file or directory: '{}'",
                sfx_path.display()
            );
        }
        let cfg = SfxConfig::load(&sfx_path)?;
        let mut entries = sfxgen::load_sfx_entries(
            &script,
            &cfg,
            None,
            Some(&types),
            a.local_only,
            &sfx_dir_for(&slug),
        )?;
        sfx_cfg = Some(cfg);
        if let Some(stop) = a.stop_at {
            entries.retain(|e| e.seq <= stop);
        }
        sfx_entries = Some(entries);
    }

    let mut speed_overrides: IndexMap<&'static str, f64> = IndexMap::new();
    for section in ["preamble", "postamble"] {
        if let Some(s) = raw_cast
            .get(section)
            .and_then(|p| p.get("speed"))
            .and_then(Num::from_value)
        {
            speed_overrides.insert(section, s.f());
        }
    }
    let voice_refs = a.voice_refs.clone().unwrap_or_else(|| {
        workspace_root()
            .join("voice_refs")
            .to_string_lossy()
            .into_owned()
    });
    if a.backend == "chatterbox-turbo" {
        print_voice_refs_table(&config, &voice_refs);
    }

    let client = xil_api::elevenlabs::Client::new(key);
    let sfx_live: Option<(&Vec<sfxgen::PlanEntry>, &SfxConfig)> = match (&sfx_entries, &sfx_cfg) {
        (Some(e), Some(c)) if !e.is_empty() => Some((e, c)),
        _ => None,
    };

    if a.reconcile {
        return reconcile(&config, &dialogue, &stems_dir, &a.backend, a.apply).map(|_| 0);
    }
    if a.dry_run {
        let sfx_dir = sfx_dir_for(&slug);
        dry_run(
            &config,
            &dialogue,
            &DryRunOpts {
                start: a.start_from,
                stop: a.stop_at,
                list: a.seq_list.as_ref(),
                stems_dir: &stems_dir,
                force: a.force,
                backend: &a.backend,
            },
            sfx_live.map(|(e, c)| (e.as_slice(), c, sfx_dir.as_path(), a.sfx_backend.as_str())),
        );
        return Ok(0);
    }

    if a.backend == "elevenlabs" {
        check_quota(&client)?;
    }
    if let Some((entries, cfg)) = sfx_live {
        let root = workspace_root();
        let missing: Vec<String> = entries
            .iter()
            .filter_map(|e| {
                let src = cfg.effects.get(&e.text)?.source.as_ref()?;
                let p = if Path::new(src).is_absolute() {
                    PathBuf::from(src)
                } else {
                    root.join(src)
                };
                (!p.exists()).then(|| format!("  '{}' → {src}", e.text))
            })
            .collect();
        if !missing.is_empty() {
            log::error(&format!(
                "{} SFX source file(s) declared but missing — fix sfx config before generating:",
                missing.len()
            ));
            for m in &missing {
                log::error(m);
            }
            return Ok(1);
        }
    }
    let mut chatterbox = None;
    if a.backend == "chatterbox-turbo" {
        let package_dir = crate::workers::python_package_dir();
        let Some(python) = resolve_venv_python(
            "venv-chatterbox",
            a.chatterbox_python.as_deref(),
            package_dir.as_deref(),
        ) else {
            log::error(
                "Cannot find chatterbox venv Python. Pass --chatterbox-python PATH, set XIL_CODEROOT to the directory containing venv-chatterbox/, or create venv-chatterbox/ in the workspace or repo root.",
            );
            return Ok(1);
        };
        chatterbox = Some(tts::Chatterbox::new(
            &python,
            &voice_refs,
            &a.device,
            Flavor::Produce,
        ));
    }
    let mut sfx_backend = None;
    if sfx_live.is_some() {
        let mm = sfxgen::MMAudioOptions {
            python: a.mmaudio_python.as_deref(),
            cfg: a.mmaudio_cfg,
            steps: a.mmaudio_steps,
            negative_prompt: &a.mmaudio_negative_prompt,
            seed: a.mmaudio_seed,
            duration: a.mmaudio_duration,
            accept_noncommercial: a.mmaudio_accept_noncommercial,
        };
        match sfxgen::make_sfx_backend(&a.sfx_backend, client.clone(), &mm)? {
            Some(b) => sfx_backend = Some(b),
            None => {
                if let Some(c) = chatterbox.as_mut() {
                    c.close();
                }
                return Ok(1);
            }
        }
    }
    let opts = GenOpts {
        start: a.start_from,
        stop: a.stop_at,
        list: a.seq_list.as_ref(),
        show: &cast_doc.show,
        backend: &a.backend,
        force: a.force,
        speed_overrides: &speed_overrides,
    };
    let result = (|| -> anyhow::Result<()> {
        generate_voices(
            &config,
            &dialogue,
            &stems_dir,
            &opts,
            &client,
            chatterbox.as_mut(),
        )?;
        if let (Some((entries, cfg)), Some(backend)) = (sfx_live, sfx_backend.as_mut()) {
            sfxgen::generate_sfx(
                entries,
                cfg,
                &stems_dir,
                &sfx_dir_for(&slug),
                a.start_from,
                backend.as_mut(),
            )?;
        }
        Ok(())
    })();
    if let Some(c) = chatterbox.as_mut() {
        c.close();
    }
    if let Some(b) = sfx_backend.as_mut() {
        let _ = b.close();
    }
    result?;
    Ok(0)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_lists_parse_like_argparse_type() {
        assert_eq!(
            parse_seq_list("12, 45, 88,").unwrap(),
            BTreeSet::from([12, 45, 88])
        );
        assert!(parse_seq_list(" , ").is_err());
        assert!(parse_seq_list("1,x").is_err());
    }

    #[test]
    fn manifest_keys_default_missing_settings() {
        assert_eq!(
            content_key("a", "v", None, None, None, "b"),
            content_key("a", "v", Some(1.0), Some(0.5), Some(0.75), "b")
        );
        assert!(!speakable("[sigh] ... !!"));
        assert!(speakable("[sigh] okay"));
    }
}

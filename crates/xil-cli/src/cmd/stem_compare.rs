//! `xil stem-compare` — flag dialogue stems whose Whisper transcript
//! strays from the scripted line. Port of `XILU016_stem_compare.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use regex::Regex;
use serde_json::{Map, Value};
use xil_core::pycsv;
use xil_core::pyfmt::{head, round_to};
use xil_core::pyjson::{dumps, py_float, Style};
use xil_core::textsim::SequenceMatcher;
use xil_core::workspace::{resolve_slug, workspace_root};
use xil_core::{banner, log};

#[derive(Parser)]
#[command(
    name = "xil-stem-compare",
    about = "Cross-reference Whisper transcripts against parsed script dialogue to flag garbled stems."
)]
struct Args {
    /// Show slug (default: resolved from project.json)
    #[arg(long, short = 's', value_name = "SLUG")]
    show: Option<String>,
    /// Episode tag, e.g. S01E01 (derives both JSON paths if not overridden)
    #[arg(long, short = 'e', value_name = "TAG")]
    episode: Option<String>,
    /// Path to stem_verify JSON (default: <workspace>/parsed/<slug>/stem_verify_<episode>.json)
    #[arg(long, value_name = "FILE")]
    stem_verify: Option<String>,
    /// Path to parsed script JSON (default: <workspace>/parsed/<slug>/parsed_<episode>.json)
    #[arg(long, value_name = "FILE")]
    parsed: Option<String>,
    /// Similarity below this marks a stem as garbled (default: 0.75)
    #[arg(
        long,
        default_value_t = 0.75,
        value_name = "FLOAT",
        allow_negative_numbers = true
    )]
    threshold: f64,
    /// Write full JSON report to this file (in addition to terminal output)
    #[arg(long, short = 'o', value_name = "FILE")]
    output: Option<String>,
    /// Print flagged entries as CSV to stdout instead of the banner summary
    #[arg(long)]
    csv: bool,
}

/// `_normalize`: lower-case, punctuation to spaces, runs of space collapsed.
fn normalize(text: &str) -> String {
    static PUNCT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static SPACE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let lower = text.to_lowercase();
    let t = PUNCT
        .get_or_init(|| Regex::new(r"[^\w\s]").unwrap())
        .replace_all(&lower, " ");
    SPACE
        .get_or_init(|| Regex::new(r"\s+").unwrap())
        .replace_all(&t, " ")
        .trim()
        .to_string()
}

fn similarity(a: &str, b: &str) -> f64 {
    SequenceMatcher::new(&normalize(a), &normalize(b)).ratio()
}

struct Flag {
    seq: Value,
    section: Value,
    scene: Value,
    speaker: Value,
    status: &'static str,
    similarity: Option<f64>,
    original: Value,
    transcript: Value,
}

impl Flag {
    fn json(&self) -> Value {
        let mut m = Map::new();
        m.insert("seq".into(), self.seq.clone());
        m.insert("section".into(), self.section.clone());
        m.insert("scene".into(), self.scene.clone());
        m.insert("speaker".into(), self.speaker.clone());
        m.insert("status".into(), self.status.into());
        m.insert(
            "similarity".into(),
            self.similarity.map(py_float).unwrap_or(Value::Null),
        );
        m.insert("original".into(), self.original.clone());
        m.insert("transcript".into(), self.transcript.clone());
        Value::Object(m)
    }
}

const STATUSES: [&str; 5] = ["ok", "garbled", "silent", "no_stem", "not_transcribed"];

fn load_json(p: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&fs::read_to_string(p)?)?)
}

fn compare(
    dialogue: &[Map<String, Value>],
    stems: &indexmap::IndexMap<i64, Map<String, Value>>,
    threshold: f64,
) -> (Vec<Flag>, [usize; 5]) {
    let mut counts = [0usize; 5];
    let mut flags = Vec::new();
    for entry in dialogue {
        let seq_v = entry.get("seq").cloned().unwrap_or(Value::Null);
        let get = |k: &str| entry.get(k).cloned().unwrap_or(Value::Null);
        let original = get("text");
        let make = |status, sim, transcript| Flag {
            seq: seq_v.clone(),
            section: get("section"),
            scene: get("scene"),
            speaker: get("speaker"),
            status,
            similarity: sim,
            original: original.clone(),
            transcript,
        };
        let Some(stem) = seq_v.as_i64().and_then(|s| stems.get(&s)) else {
            counts[3] += 1;
            flags.push(make("no_stem", None, Value::Null));
            continue;
        };
        let transcript = match stem.get("transcript") {
            None | Some(Value::Null) => {
                counts[4] += 1;
                flags.push(make("not_transcribed", None, Value::Null));
                continue;
            }
            Some(t) => t,
        };
        let text = transcript.get("text").and_then(Value::as_str).unwrap_or("");
        if text.trim().is_empty() {
            counts[2] += 1;
            flags.push(make("silent", Some(0.0), Value::String(String::new())));
            continue;
        }
        let sim = round_to(similarity(original.as_str().unwrap_or(""), text), 4);
        if sim < threshold {
            counts[1] += 1;
            flags.push(make("garbled", Some(sim), Value::String(text.to_string())));
        } else {
            counts[0] += 1;
        }
    }
    (flags, counts)
}

fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        other => pycsv::cell(other),
    }
}

fn print_summary(counts: &[usize; 5], flags: &[Flag], threshold: f64) {
    let total: usize = counts.iter().sum();
    let ok_pct = if total > 0 {
        counts[0] as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    log::info(&format!("  Threshold      : {threshold:.2}"));
    log::info(&format!("  Dialogue stems : {total}"));
    log::info(&format!("  OK             : {}  ({ok_pct:.1}%)", counts[0]));
    log::info(&format!("  Garbled        : {}", counts[1]));
    log::info(&format!("  Silent         : {}", counts[2]));
    log::info(&format!("  No stem        : {}", counts[3]));
    log::info(&format!("  Not transcribed: {}", counts[4]));
    if flags.is_empty() {
        log::info("");
        log::info("  No issues found.");
        return;
    }
    log::info("");
    log::info("--- Flagged entries ---");
    for f in flags {
        let seq = match &f.seq {
            Value::Number(n) if n.as_i64().is_some() => format!("{:03}", n.as_i64().unwrap()),
            other => str_of(other),
        };
        let speaker_full = match &f.speaker {
            Value::String(s) if !s.is_empty() => s.clone(),
            _ => "?".into(),
        };
        let speaker = head(&speaker_full, 12);
        let original = str_of(&f.original);
        match f.status {
            "garbled" => {
                log::info(&format!(
                    "[garbled] seq={seq}  {speaker:<12} sim={:.2}",
                    f.similarity.unwrap_or(0.0)
                ));
                log::info(&format!("  ORIGINAL  : {original}"));
                log::info(&format!("  TRANSCRIPT: {}", str_of(&f.transcript)));
            }
            "silent" => {
                log::info(&format!("[silent]  seq={seq}  {speaker}"));
                log::info(&format!("  ORIGINAL  : {original}"));
            }
            "no_stem" => {
                log::info(&format!("[no_stem] seq={seq}  {speaker}"));
                log::info(&format!("  ORIGINAL  : {original}"));
            }
            _ => {
                log::info(&format!("[no_xscr] seq={seq}  {speaker}"));
                log::info(&format!("  ORIGINAL  : {original}"));
            }
        }
    }
}

fn print_csv(flags: &[Flag]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let header: Vec<String> = [
        "seq",
        "section",
        "scene",
        "speaker",
        "status",
        "similarity",
        "original",
        "transcript",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    pycsv::write_row(&mut out, &header)?;
    for f in flags {
        let sim = f.similarity.map(py_float).unwrap_or(Value::Null);
        let row: Vec<String> = [
            &f.seq,
            &f.section,
            &f.scene,
            &f.speaker,
            &Value::from(f.status),
            &sim,
            &f.original,
            &f.transcript,
        ]
        .iter()
        .map(|v| pycsv::cell(v))
        .collect();
        pycsv::write_row(&mut out, &row)?;
    }
    out.flush()
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("stem-compare");
    let a: Args = match super::parse_or_exit("xil-stem-compare", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    execute(&a)
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let workspace = workspace_root();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let episode = a.episode.clone().filter(|e| !e.is_empty());
    let episode_label = episode.clone().unwrap_or_else(|| "unknown".into());

    let stem_verify_path = match a.stem_verify.as_ref().filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => match &episode {
            Some(ep) => workspace
                .join("parsed")
                .join(&slug)
                .join(format!("stem_verify_{ep}.json")),
            None => {
                log::error("--episode is required unless --stem-verify is provided");
                return Ok(1);
            }
        },
    };
    let parsed_path = match a.parsed.as_ref().filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => match &episode {
            Some(ep) => workspace
                .join("parsed")
                .join(&slug)
                .join(format!("parsed_{ep}.json")),
            None => {
                log::error("--episode is required unless --parsed is provided");
                return Ok(1);
            }
        },
    };
    if !stem_verify_path.exists() {
        log::error(&format!(
            "stem_verify JSON not found: {}",
            stem_verify_path.display()
        ));
        return Ok(1);
    }
    if !parsed_path.exists() {
        log::error(&format!("parsed JSON not found: {}", parsed_path.display()));
        return Ok(1);
    }
    log::info(&format!("  stem_verify : {}", stem_verify_path.display()));
    log::info(&format!("  parsed      : {}", parsed_path.display()));

    let verify = load_json(&stem_verify_path)?;
    let mut stems = indexmap::IndexMap::new();
    for f in verify["files"].as_array().into_iter().flatten() {
        let Some(o) = f.as_object() else { continue };
        let Some(seq) = o.get("seq").filter(|s| !s.is_null()) else {
            continue;
        };
        if o.get("speaker").and_then(Value::as_str) == Some("sfx") {
            continue;
        }
        if let Some(s) = seq.as_i64() {
            stems.insert(s, o.clone());
        }
    }
    let parsed = load_json(&parsed_path)?;
    let dialogue: Vec<Map<String, Value>> = parsed["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e.as_object())
        .filter(|e| {
            e.get("type").and_then(Value::as_str) == Some("dialogue")
                && e.get("direction_type").map_or(true, Value::is_null)
        })
        .cloned()
        .collect();

    let (flags, counts) = compare(&dialogue, &stems, a.threshold);
    if a.csv {
        print_csv(&flags)?;
    } else {
        print_summary(&counts, &flags, a.threshold);
    }

    if let Some(out) = a.output.as_ref().filter(|s| !s.is_empty()) {
        let out = PathBuf::from(out);
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let mut summary = Map::new();
        summary.insert(
            "total_dialogue".into(),
            Value::from(counts.iter().sum::<usize>()),
        );
        for (k, n) in STATUSES.iter().zip(counts) {
            summary.insert((*k).into(), Value::from(n));
        }
        let mut report = Map::new();
        report.insert("show".into(), slug.clone().into());
        report.insert("episode".into(), episode_label.into());
        report.insert(
            "generated".into(),
            chrono::Local::now()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
                .into(),
        );
        report.insert("threshold".into(), py_float(a.threshold));
        report.insert(
            "stem_verify_path".into(),
            fs::canonicalize(&stem_verify_path)?
                .to_string_lossy()
                .into_owned()
                .into(),
        );
        report.insert(
            "parsed_path".into(),
            fs::canonicalize(&parsed_path)?
                .to_string_lossy()
                .into_owned()
                .into(),
        );
        report.insert("summary".into(), Value::Object(summary));
        report.insert(
            "flags".into(),
            Value::Array(flags.iter().map(Flag::json).collect()),
        );
        fs::write(&out, dumps(&Value::Object(report), Style::INDENT2))?;
        log::info(&format!("Written: {}", out.display()));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_punctuation_and_case() {
        assert_eq!(normalize("Hello, World!  It's  me."), "hello world it s me");
        assert_eq!(similarity("Hello there!", "hello there"), 1.0);
    }
}

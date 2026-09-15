//! `xil stem-log` — daily logs → stem generation chronology CSV. Port of
//! `XILU008_stem_log_report.py`.
//!
//! This one talks on stderr and stdout directly (the Python never calls the
//! logger), but the dispatcher still configures logging, so the day's log
//! file exists — empty — after a run.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use clap::Parser;
use regex::Regex;
use serde_json::{Map, Value};
use xil_core::fsutil::{basename, glob_children};
use xil_core::log;
use xil_core::pycsv;
use xil_core::workspace::workspace_root;

static RE_ELEVEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*>\s*\[(\d+)\]\s+(\S+)\s+with\s+(eleven_\S+)\s+\((\d+)\s+chars\)").unwrap()
});
static RE_GTTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*>\s*\[(\d+)\]\s+(\S+)\s+via\s+gTTS\s+\((\d+)\s+chars\)").unwrap()
});
static RE_CHATTERBOX_TURBO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*>\s*\[(\d+)\]\s+(\S+)\s+via\s+Chatterbox\s+Turbo\s+\((\d+)\s+chars\)")
        .unwrap()
});
static RE_CHATTERBOX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*>\s*\[(\d+)\]\s+(\S+)\s+via\s+Chatterbox\s+\((\d+)\s+chars\)").unwrap()
});
static RE_SAVED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*Saved:\s+(\S+)").unwrap());
static RE_SHA256: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*SHA256:\s+([0-9a-fA-F]+)").unwrap());
static RE_PHASE1: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^---\s*Phase 1:\s*Generating").unwrap());
static RE_V2_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[^|]*\|[A-Z]+\|[^|]*\|[^|]*\|").unwrap()
});
static RE_RUN_BEGIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^BEGIN\b").unwrap());
static RE_DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{4}-\d{2}-\d{2})").unwrap());

const FIELDNAMES: [&str; 11] = [
    "log_date",
    "log_file",
    "run_index",
    "log_line",
    "seq",
    "speaker",
    "backend",
    "char_count",
    "stem_path",
    "stem_filename",
    "sha256",
];

#[derive(Parser)]
#[command(
    name = "xil-stem-log",
    about = "Parse xil-pipeline logs into a stem generation chronology CSV."
)]
struct Args {
    /// Directory containing xil_YYYY-MM-DD.log files (default: <workspace>/logs/)
    #[arg(long, value_name = "DIR")]
    logs_dir: Option<PathBuf>,
    /// Output CSV path (default: stem_log_report.csv); use - for stdout
    #[arg(
        long,
        short = 'o',
        value_name = "PATH",
        default_value = "stem_log_report.csv"
    )]
    output: String,
    /// Only include log files on or after this date
    #[arg(long, value_name = "YYYY-MM-DD")]
    since: Option<String>,
    /// Filter records to a specific episode tag (e.g. S03E03)
    #[arg(long, visible_alias = "tag", value_name = "TAG")]
    episode: Option<String>,
    /// Filter records to a specific show slug (e.g. the413)
    #[arg(long, value_name = "SLUG")]
    slug: Option<String>,
    /// Print CSV to stdout (equivalent to --output -)
    #[arg(long)]
    show: bool,
    /// Instead of writing CSV: compare logged char_counts against the given parsed script JSON and flag stems whose audio may not match current text.
    #[arg(long, value_name = "PARSED_JSON")]
    audit: Option<PathBuf>,
    /// Char-count delta threshold for --audit flagging (default: 20)
    #[arg(long, value_name = "N", default_value_t = 20)]
    audit_threshold: i64,
}

/// One generated stem, as recorded in a log.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub log_date: String,
    pub log_file: String,
    pub run_index: u64,
    pub log_line: Option<u64>,
    pub seq: i64,
    pub speaker: String,
    pub backend: String,
    pub char_count: i64,
    pub stem_path: Option<String>,
    pub stem_filename: Option<String>,
    pub sha256: Option<String>,
}

fn strip_v2_prefix(line: &str) -> &str {
    match RE_V2_PREFIX.find(line) {
        Some(m) => &line[m.end()..],
        None => line,
    }
}

/// `YYYY-MM-DD` from a log filename, else today.
pub fn date_from_filename(name: &str) -> String {
    RE_DATE
        .captures(name)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string())
}

pub fn parse_log(log_path: &Path) -> Vec<Record> {
    let name = basename(log_path);
    let log_date = date_from_filename(&name);
    let bytes = fs::read(log_path).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);

    let mut records = Vec::new();
    let mut run_index = 0u64;
    let mut pending: Option<Record> = None;

    for (i, raw) in text.split_inclusive('\n').enumerate() {
        let lineno = (i + 1) as u64;
        let stripped = raw.strip_suffix('\n').unwrap_or(raw);
        let is_v2 = RE_V2_PREFIX.is_match(stripped);
        let line = strip_v2_prefix(stripped);

        let boundary = if is_v2 {
            RE_RUN_BEGIN.is_match(line)
        } else {
            RE_PHASE1.is_match(line)
        };
        if boundary {
            run_index += 1;
            pending = None;
            continue;
        }

        let hit = RE_ELEVEN
            .captures(line)
            .map(|c| {
                (
                    c[1].to_string(),
                    c[2].to_string(),
                    c[3].to_string(),
                    c[4].to_string(),
                )
            })
            .or_else(|| {
                RE_GTTS.captures(line).map(|c| {
                    (
                        c[1].to_string(),
                        c[2].to_string(),
                        "gtts".into(),
                        c[3].to_string(),
                    )
                })
            })
            .or_else(|| {
                RE_CHATTERBOX_TURBO.captures(line).map(|c| {
                    (
                        c[1].to_string(),
                        c[2].to_string(),
                        "chatterbox-turbo".into(),
                        c[3].to_string(),
                    )
                })
            })
            .or_else(|| {
                RE_CHATTERBOX.captures(line).map(|c| {
                    (
                        c[1].to_string(),
                        c[2].to_string(),
                        "chatterbox".into(),
                        c[3].to_string(),
                    )
                })
            });
        if let Some((seq, speaker, backend, chars)) = hit {
            pending = Some(Record {
                log_date: log_date.clone(),
                log_file: name.clone(),
                run_index,
                log_line: None,
                seq: seq.parse().unwrap_or(0),
                speaker,
                backend,
                char_count: chars.parse().unwrap_or(0),
                stem_path: None,
                stem_filename: None,
                sha256: None,
            });
        }

        let Some(p) = pending.as_mut() else { continue };

        if let Some(m) = RE_SAVED.captures(line) {
            if p.stem_path.is_none() {
                let path = m[1].to_string();
                p.stem_filename = Some(basename(Path::new(&path)));
                p.stem_path = Some(path);
                p.log_line = Some(lineno);
                continue;
            }
        }
        if let Some(m) = RE_SHA256.captures(line) {
            if p.stem_path.is_some() && p.sha256.is_none() {
                p.sha256 = Some(m[1].to_string());
                records.push(p.clone());
                pending = None;
            }
        }
    }
    records
}

fn to_row(r: &Record) -> Map<String, Value> {
    let opt = |s: &Option<String>| s.clone().map(Value::String).unwrap_or(Value::Null);
    let mut m = Map::new();
    m.insert("log_date".into(), Value::String(r.log_date.clone()));
    m.insert("log_file".into(), Value::String(r.log_file.clone()));
    m.insert("run_index".into(), Value::from(r.run_index));
    m.insert(
        "log_line".into(),
        r.log_line.map(Value::from).unwrap_or(Value::Null),
    );
    m.insert("seq".into(), Value::from(r.seq));
    m.insert("speaker".into(), Value::String(r.speaker.clone()));
    m.insert("backend".into(), Value::String(r.backend.clone()));
    m.insert("char_count".into(), Value::from(r.char_count));
    m.insert("stem_path".into(), opt(&r.stem_path));
    m.insert("stem_filename".into(), opt(&r.stem_filename));
    m.insert("sha256".into(), opt(&r.sha256));
    m
}

fn audit(records: &[Record], parsed_json: &Path, threshold: i64) -> anyhow::Result<()> {
    let text = fs::read_to_string(parsed_json)?;
    let parsed: Value = serde_json::from_str(&text)?;
    let mut by_seq_spk = std::collections::HashMap::new();
    for e in parsed
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if e.get("type").and_then(Value::as_str) == Some("dialogue") {
            let seq = e.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let spk = e
                .get("speaker")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let txt = e
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .count() as i64;
            by_seq_spk.insert((seq, spk), txt);
        }
    }
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{:>4}  {:<14} {:>9}  {:>10}  {:>6}  status",
        "seq", "speaker", "log_chars", "json_chars", "delta"
    )?;
    writeln!(out, "{}", "-".repeat(65))?;
    let (mut ok, mut flagged, mut unmatched) = (0, 0, 0);
    for rec in records {
        let Some(json_len) = by_seq_spk.get(&(rec.seq, rec.speaker.clone())) else {
            unmatched += 1;
            continue;
        };
        let delta = (json_len - rec.char_count).abs();
        if delta > threshold {
            flagged += 1;
            writeln!(
                out,
                "{:>4}  {:<14} {:>9}  {:>10}  {:>6}  ⚠ MISMATCH",
                rec.seq, rec.speaker, rec.char_count, json_len, delta
            )?;
        } else {
            ok += 1;
        }
    }
    writeln!(out, "{}", "-".repeat(65))?;
    writeln!(
        out,
        "  {ok} OK, {flagged} flagged (delta > {threshold}), {unmatched} not in current JSON"
    )?;
    Ok(())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("stem-log");
    let a: Args = match super::parse_or_exit("xil-stem-log", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let logs_dir = a
        .logs_dir
        .clone()
        .unwrap_or_else(|| workspace_root().join("logs"));
    if !logs_dir.is_dir() {
        eprintln!("[ERROR] Logs directory not found: {}", logs_dir.display());
        return Ok(1);
    }

    let mut log_files = glob_children(&logs_dir, "xil_", ".log");
    log_files.sort_by_key(|p| (date_from_filename(&basename(p)), basename(p)));
    if let Some(since) = &a.since {
        log_files.retain(|p| date_from_filename(&basename(p)).as_str() >= since.as_str());
    }
    if log_files.is_empty() {
        eprintln!("[!] No matching log files found.");
        return Ok(0);
    }

    let mut all = Vec::new();
    for lf in &log_files {
        let recs = parse_log(lf);
        eprintln!("  {}: {} stems", basename(lf), recs.len());
        all.extend(recs);
    }
    if let Some(tag) = &a.episode {
        let tag = tag.to_uppercase();
        all.retain(|r| {
            r.stem_path
                .as_ref()
                .map(|p| p.to_uppercase().contains(&tag))
                .unwrap_or(false)
        });
    }
    if let Some(slug) = &a.slug {
        let slug = slug.to_lowercase();
        all.retain(|r| {
            r.stem_path
                .as_ref()
                .map(|p| p.to_lowercase().contains(&slug))
                .unwrap_or(false)
        });
    }
    eprintln!("Total: {} stem records", all.len());

    if let Some(pj) = &a.audit {
        audit(&all, pj, a.audit_threshold)?;
        return Ok(0);
    }

    let rows: Vec<_> = all.iter().map(to_row).collect();
    if a.show || a.output == "-" {
        let mut so = std::io::stdout().lock();
        pycsv::write_dicts(&mut so, &FIELDNAMES, &rows)?;
        so.flush()?;
    } else {
        let mut f = fs::File::create(&a.output)?;
        pycsv::write_dicts(&mut f, &FIELDNAMES, &rows)?;
        eprintln!("Written: {}", a.output);
    }
    Ok(0)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2: &str = "\
2026-08-01T10:00:00-0400|RUN|h|produce|BEGIN argv=\"xil produce\" pid=1 ver=0.3.2 cwd=/x
2026-08-01T10:00:01-0400|INFO|h|produce|  > [006] adam via Chatterbox Turbo (282 chars)...
2026-08-01T10:00:05-0400|INFO|h|produce|   Saved: stems/the413/S01E01/006_act1_adam.mp3
2026-08-01T10:00:05-0400|INFO|h|produce|   SHA256: abc123
2026-08-01T10:00:06-0400|INFO|h|produce|  > [007] sarah with eleven_v3 (120 chars)...
2026-08-01T10:00:09-0400|INFO|h|produce|   Saved: stems/the413/S01E01/007_act1_sarah.mp3
2026-08-01T10:00:09-0400|INFO|h|produce|   SHA256: def456
2026-08-01T11:00:00-0400|RUN|h|produce|BEGIN argv=\"xil produce\" pid=2 ver=0.3.2 cwd=/x
2026-08-01T11:00:03-0400|INFO|h|produce|  > [004] maya via chatterbox (50 chars)...
2026-08-01T11:00:04-0400|INFO|h|produce|   Saved: stems/nightowls/S01E02/004_cold-open_maya.mp3
";

    #[test]
    fn v2_state_machine_and_run_index() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("xil_v2_2026-08-01_h.log");
        fs::write(&p, V2).unwrap();
        let recs = parse_log(&p);
        assert_eq!(recs.len(), 2, "record without SHA256 is dropped");
        assert_eq!(recs[0].backend, "chatterbox-turbo");
        assert_eq!(
            (
                recs[0].seq,
                recs[0].char_count,
                recs[0].run_index,
                recs[0].log_line
            ),
            (6, 282, 1, Some(3))
        );
        assert_eq!(recs[0].stem_filename.as_deref(), Some("006_act1_adam.mp3"));
        assert_eq!(recs[1].backend, "eleven_v3");
        assert_eq!(recs[1].log_date, "2026-08-01");
    }

    #[test]
    fn v1_uses_phase1_header_as_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("xil_2026-07-10.log");
        fs::write(
            &p,
            "--- Phase 1: Generating voices ---\n  > [001] adam with eleven_v3 (10 chars)...\n   Saved: s/001.mp3\n   SHA256: aa\n\
             --- Phase 1: Generating voices ---\n  > [002] maya via gTTS (5 chars)...\n   Saved: s/002.mp3\n   SHA256: bb\n",
        )
        .unwrap();
        let recs = parse_log(&p);
        assert_eq!(
            recs.iter().map(|r| r.run_index).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(recs[1].backend, "gtts");
    }

    #[test]
    fn date_from_filename_or_today() {
        assert_eq!(date_from_filename("xil_v1_2026-07-15.log"), "2026-07-15");
        assert_eq!(date_from_filename("nodate.log").len(), 10);
    }
}

//! `xil episode-summary` — one-row-per-episode CSV. Port of `XILU014_episode_summary.py`.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value};
use xil_core::fsutil::{basename, glob_recursive};
use xil_core::pycsv;
use xil_core::workspace::workspace_root;
use xil_core::{banner, log};

const SCRIPT_NAME: &str = "XILU014_episode_summary.py";
const SKIP_PREFIXES: [&str; 2] = ["roundtrip_", "pre_splice_"];
const ALL_COLS: [&str; 9] = [
    "show",
    "tag",
    "season",
    "episode",
    "title",
    "season_title",
    "dialogue_lines",
    "words",
    "tts_chars",
];

#[derive(Parser)]
#[command(
    name = "xil-episode-summary",
    about = "Write a one-row-per-episode summary CSV from all parsed_<tag>.json files."
)]
struct Args {
    /// Output CSV path (default: <workspace>/episode_summary.csv)
    #[arg(long, short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,
    /// Filter to a single show name, e.g. 'THE 413' (case-insensitive)
    #[arg(long, value_name = "NAME")]
    show: Option<String>,
    /// Write CSV to stdout — no banner, safe to pipe
    #[arg(long)]
    stdout: bool,
}

fn collect_files(parsed_root: &Path) -> Vec<PathBuf> {
    glob_recursive(parsed_root, "parsed_", ".json")
        .into_iter()
        .filter(|p| {
            let name = basename(p);
            !SKIP_PREFIXES.iter().any(|pfx| name.starts_with(pfx))
        })
        .collect()
}

/// `int(x)` for a CSV cell that may be an int, a numeric string, or blank;
/// 999 when Python would raise.
fn sort_int(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(999),
        Value::String(s) => s.trim().parse().unwrap_or(999),
        Value::Bool(b) => *b as i64,
        _ => 999,
    }
}

pub fn build_summary(parsed_root: &Path, show_filter: Option<&str>) -> Vec<Map<String, Value>> {
    let mut rows = Vec::new();
    for path in collect_files(parsed_root) {
        let data: Value = match fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
        {
            Ok(v) => v,
            Err(e) => {
                log::warning(&format!("Skipping {} — {e}", basename(&path)));
                continue;
            }
        };
        let obj = data.as_object().cloned().unwrap_or_default();
        let show = obj
            .get("show")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(f) = show_filter {
            if show.to_lowercase() != f.to_lowercase() {
                continue;
            }
        }
        let name = basename(&path);
        let tag = name
            .strip_prefix("parsed_")
            .unwrap_or(&name)
            .strip_suffix(".json")
            .unwrap_or(&name)
            .to_string();
        let stats = obj
            .get("stats")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let words: usize = obj
            .get("entries")
            .and_then(Value::as_array)
            .map(|es| {
                es.iter()
                    .filter(|e| e.get("type").and_then(Value::as_str) == Some("dialogue"))
                    .map(|e| {
                        e.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .split_whitespace()
                            .count()
                    })
                    .sum()
            })
            .unwrap_or(0);

        let none_to_blank = |k: &str| match obj.get(k) {
            None | Some(Value::Null) => Value::String(String::new()),
            Some(v) => v.clone(),
        };
        let mut row = Map::new();
        row.insert("show".into(), Value::String(show));
        row.insert("tag".into(), Value::String(tag));
        row.insert("season".into(), none_to_blank("season"));
        row.insert("episode".into(), none_to_blank("episode"));
        row.insert("title".into(), super::get_or_empty(&obj, "title"));
        row.insert(
            "season_title".into(),
            super::get_or_empty(&obj, "season_title"),
        );
        row.insert(
            "dialogue_lines".into(),
            stats
                .get("dialogue_lines")
                .cloned()
                .unwrap_or(Value::from(0)),
        );
        row.insert("words".into(), Value::from(words));
        row.insert(
            "tts_chars".into(),
            stats
                .get("characters_for_tts")
                .cloned()
                .unwrap_or(Value::from(0)),
        );
        rows.push(row);
    }
    rows.sort_by(|a, b| {
        let key = |r: &Map<String, Value>| {
            (
                r["show"].as_str().unwrap_or("").to_string(),
                sort_int(&r["season"]),
                sort_int(&r["episode"]),
                r["tag"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
    rows
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let workspace = workspace_root();
    let parsed_root = workspace.join("parsed");
    if !parsed_root.is_dir() {
        log::error(&format!(
            "parsed/ directory not found at: {}",
            parsed_root.display()
        ));
        return Ok(1);
    }
    let rows = build_summary(&parsed_root, a.show.as_deref());
    if rows.is_empty() {
        log::warning(&format!(
            "No parsed_*.json files found under {}",
            parsed_root.display()
        ));
        return Ok(0);
    }
    if a.stdout {
        let mut out = std::io::stdout().lock();
        pycsv::write_dicts(&mut out, &ALL_COLS, &rows)?;
        out.flush()?;
        return Ok(0);
    }
    let output = a
        .output
        .clone()
        .unwrap_or_else(|| workspace.join("episode_summary.csv"));
    let mut f = fs::File::create(&output)?;
    pycsv::write_dicts(&mut f, &ALL_COLS, &rows)?;
    log::info(&format!("  Episodes:  {}", rows.len()));
    log::info(&format!("  Written:   {}", output.display()));
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("episode-summary");
    let a: Args = match super::parse_or_exit("xil-episode-summary", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    if a.stdout {
        execute(&a)
    } else {
        let _banner = banner::begin(SCRIPT_NAME, &super::argv_line(args));
        execute(&a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn summary_counts_words_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        parsed(
            &r.join("s"),
            "parsed_S01E02.json",
            r#"{"show":"S","season":1,"episode":2,"title":"t2","stats":{"dialogue_lines":1,"characters_for_tts":9},
                "entries":[{"type":"dialogue","text":"one two  three"},{"type":"direction","text":"x y"}]}"#,
        );
        parsed(
            &r.join("s"),
            "parsed_S01E01.json",
            r#"{"show":"S","season":1,"episode":1,"entries":[]}"#,
        );
        parsed(
            &r.join("s"),
            "pre_splice_parsed_S01E03.json",
            r#"{"show":"S"}"#,
        );
        let rows = build_summary(r, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["tag"], "S01E01");
        assert_eq!(rows[1]["tag"], "S01E02");
        assert_eq!(rows[1]["words"], 3);
        assert_eq!(rows[1]["tts_chars"], 9);
        assert_eq!(rows[0]["dialogue_lines"], 0);
        assert_eq!(rows[0]["title"], "");
        assert!(build_summary(r, Some("other")).is_empty());
        assert_eq!(build_summary(r, Some("s")).len(), 2);
    }

    #[test]
    fn sort_int_falls_back_to_999() {
        assert_eq!(sort_int(&Value::from(3)), 3);
        assert_eq!(sort_int(&Value::from("4")), 4);
        assert_eq!(sort_int(&Value::from("")), 999);
        assert_eq!(sort_int(&Value::Null), 999);
    }
}

//! `xil parsed-csv` — parsed JSON → one row per entry. Port of `XILU012_parsed_csv.py`.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value};
use xil_core::fsutil::{abspath, basename, glob_children, glob_recursive};
use xil_core::pycsv;
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::workspace_root;
use xil_core::{banner, log};

const TRUNCATE: usize = 200;
const ALL_COLS: [&str; 13] = [
    "file",
    "tag",
    "show",
    "season",
    "episode",
    "seq",
    "type",
    "section",
    "scene",
    "speaker",
    "direction",
    "direction_type",
    "text",
];
const DEFAULT_PATH: &str = "__parsed_default__";

#[derive(Parser)]
#[command(
    name = "xil-parsed-csv",
    about = "Export parsed_<tag>.json entries to CSV — one row per entry"
)]
struct Args {
    /// parsed JSON file, or directory containing parsed_*.json files (default: workspace parsed/)
    #[arg(default_value = DEFAULT_PATH)]
    path: String,
    /// Write CSV to FILE (default: stdout)
    #[arg(long, short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,
    /// Output a JSON array to stdout — no banner, safe to pipe to jq
    #[arg(long)]
    json: bool,
}

fn tag_from_path(path: &Path, data: &Map<String, Value>) -> Value {
    let name = basename(path);
    if let Some(mid) = name
        .strip_prefix("parsed_")
        .and_then(|s| s.strip_suffix(".json"))
    {
        return Value::String(mid.to_string());
    }
    data.get("tag_override")
        .cloned()
        .unwrap_or(Value::String(name))
}

/// Rows for one parsed JSON. `Err` carries the message Python's `except
/// Exception as exc` would show.
pub fn rows_from_file(path: &Path) -> Result<Vec<Map<String, Value>>, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let data: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let obj = data.as_object().cloned().unwrap_or_default();

    let mut rows = Vec::new();
    for entry in obj
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let e = entry.as_object().cloned().unwrap_or_default();
        let seq = e.get("seq").cloned().ok_or_else(|| "'seq'".to_string())?;
        let kind = e.get("type").cloned().ok_or_else(|| "'type'".to_string())?;
        let text: String = e
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(TRUNCATE)
            .collect();
        let mut row = Map::new();
        row.insert("file".into(), Value::String(basename(path)));
        row.insert("tag".into(), tag_from_path(path, &obj));
        row.insert("show".into(), super::get_or_empty(&obj, "show"));
        row.insert("season".into(), super::get_or_empty(&obj, "season"));
        row.insert("episode".into(), super::get_or_empty(&obj, "episode"));
        row.insert("seq".into(), seq);
        row.insert("type".into(), kind);
        for k in ["section", "scene", "speaker", "direction", "direction_type"] {
            row.insert(k.into(), super::get_or_blank(&e, k));
        }
        row.insert("text".into(), Value::String(text));
        rows.push(row);
    }
    Ok(rows)
}

fn collect_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        vec![abspath(path)]
    } else if path.is_dir() {
        glob_children(path, "parsed_", ".json")
            .into_iter()
            .map(|p| abspath(&p))
            .collect()
    } else {
        Vec::new()
    }
}

fn execute(a: &Args) -> anyhow::Result<()> {
    let quiet = a.json || a.output.is_none();

    let files = if a.path == DEFAULT_PATH {
        glob_recursive(&workspace_root().join("parsed"), "parsed_", ".json")
            .into_iter()
            .map(|p| abspath(&p))
            .collect()
    } else {
        collect_files(&abspath(Path::new(&a.path)))
    };

    if files.is_empty() {
        log::error(&format!("No parsed_*.json files found at: {}", a.path));
        return Ok(());
    }
    if !quiet {
        log::info(&format!("Processing {} parsed JSON file(s)…", files.len()));
    }

    let mut all_rows = Vec::new();
    for p in &files {
        match rows_from_file(p) {
            Ok(rows) => {
                if !quiet {
                    log::info(&format!("  {} → {} entries", basename(p), rows.len()));
                }
                all_rows.extend(rows);
            }
            Err(e) => log::warning(&format!("Skipping {} — {e}", basename(p))),
        }
    }

    if all_rows.is_empty() {
        if !quiet {
            log::info("No rows produced.");
        }
        return Ok(());
    }

    if a.json {
        let arr = Value::Array(all_rows.into_iter().map(Value::Object).collect());
        println!("{}", dumps(&arr, Style::INDENT2));
        return Ok(());
    }

    if let Some(out) = &a.output {
        let mut f = fs::File::create(out)?;
        pycsv::write_dicts(&mut f, &ALL_COLS, &all_rows)?;
        log::info(&format!(
            "Written: {}  ({} rows)",
            out.display(),
            all_rows.len()
        ));
    } else {
        let mut so = std::io::stdout().lock();
        pycsv::write_dicts(&mut so, &ALL_COLS, &all_rows)?;
        so.flush()?;
    }

    if !quiet {
        log::info(&format!(
            "Total: {} entry rows from {} file(s)",
            all_rows.len(),
            files.len()
        ));
    }
    Ok(())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("parsed-csv");
    let a: Args = match super::parse_or_exit("xil-parsed-csv", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    if a.json || a.output.is_none() {
        execute(&a)?;
    } else {
        let _banner = banner::begin(super::prog(), &super::argv_line(args));
        execute(&a)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_flatten_and_truncate() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("parsed_S01E01.json");
        let long = "x".repeat(250);
        fs::write(
            &p,
            format!(
                r#"{{"show":"S","season":null,"entries":[
                    {{"seq":1,"type":"dialogue","section":"act1","scene":null,"speaker":"a","text":"{long}"}},
                    {{"seq":2,"type":"direction","direction_type":"SFX","text":"boom"}}]}}"#
            ),
        )
        .unwrap();
        let rows = rows_from_file(&p).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["tag"], "S01E01");
        assert_eq!(rows[0]["season"], Value::Null);
        assert_eq!(rows[0]["episode"], "");
        assert_eq!(rows[0]["scene"], "");
        assert_eq!(rows[0]["text"].as_str().unwrap().len(), 200);
        assert_eq!(rows[1]["direction_type"], "SFX");
        assert_eq!(rows[1]["speaker"], "");
    }

    #[test]
    fn missing_seq_is_a_keyerror() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("parsed_x.json");
        fs::write(&p, r#"{"entries":[{"type":"dialogue"}]}"#).unwrap();
        assert_eq!(rows_from_file(&p).unwrap_err(), "'seq'");
    }

    #[test]
    fn tag_falls_back_to_override_then_name() {
        let mut d = Map::new();
        assert_eq!(
            tag_from_path(Path::new("/x/parsed_S02E03.json"), &d),
            "S02E03"
        );
        assert_eq!(tag_from_path(Path::new("/x/other.json"), &d), "other.json");
        d.insert("tag_override".into(), Value::from("V01C01"));
        assert_eq!(tag_from_path(Path::new("/x/other.json"), &d), "V01C01");
    }
}

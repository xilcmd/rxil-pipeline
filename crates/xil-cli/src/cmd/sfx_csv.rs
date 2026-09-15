//! `xil sfx-csv` — sfx config → one row per effect. Port of `XILU011_sfx_csv.py`.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value};
use xil_core::fsutil::{abspath, basename, glob_children};
use xil_core::pycsv;
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::workspace_root;
use xil_core::{banner, log};

const META_COLS: [&str; 5] = ["file", "tag", "show", "season", "episode"];
const EFFECT_COLS: [&str; 11] = [
    "effect_key",
    "type",
    "prompt",
    "source",
    "duration_seconds",
    "loop",
    "play_duration",
    "volume_percentage",
    "ramp_in_seconds",
    "ramp_out_seconds",
    "prompt_influence",
];
const DEFAULT_COLS: [&str; 10] = [
    "default_prompt_influence",
    "default_volume_percentage",
    "default_ramp_in_seconds",
    "default_ramp_out_seconds",
    "default_music_volume_percentage",
    "default_music_ramp_in_seconds",
    "default_music_ramp_out_seconds",
    "default_ambience_volume_percentage",
    "default_ambience_ramp_in_seconds",
    "default_ambience_ramp_out_seconds",
];
const COPIED_FIELDS: [&str; 10] = [
    "prompt",
    "type",
    "source",
    "duration_seconds",
    "loop",
    "play_duration",
    "volume_percentage",
    "ramp_in_seconds",
    "ramp_out_seconds",
    "prompt_influence",
];
const DEFAULT_PATH: &str = "__configs_default__";

fn all_cols() -> Vec<&'static str> {
    META_COLS
        .iter()
        .chain(EFFECT_COLS.iter())
        .chain(DEFAULT_COLS.iter())
        .copied()
        .collect()
}

#[derive(Parser)]
#[command(
    name = "xil-sfx-csv",
    about = "Flatten sfx_<tag>.json configs to CSV — one row per effect"
)]
struct Args {
    /// sfx JSON file, or directory containing sfx_*.json files (default: workspace configs/)
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
    if let Some(t) = data.get("tag_override") {
        return t.clone();
    }
    let name = basename(path);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&name);
    Value::String(stem.strip_prefix("sfx_").unwrap_or(stem).to_string())
}

/// Flat rows for one sfx config. Key order matches Python: every column
/// first (blank), then any extra `default_*` keys appended.
pub fn flatten(path: &Path) -> Result<Vec<Map<String, Value>>, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let data: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let obj = data.as_object().cloned().unwrap_or_default();
    let tag = tag_from_path(path, &obj);
    let defaults = obj
        .get("defaults")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let cols = all_cols();
    let mut rows = Vec::new();
    for (key, effect) in obj
        .get("effects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
    {
        let mut row = Map::new();
        for c in &cols {
            row.insert((*c).to_string(), Value::String(String::new()));
        }
        row.insert("file".into(), Value::String(basename(path)));
        row.insert("tag".into(), tag.clone());
        row.insert("show".into(), super::get_or_empty(&obj, "show"));
        row.insert("season".into(), super::get_or_empty(&obj, "season"));
        row.insert("episode".into(), super::get_or_empty(&obj, "episode"));
        for (k, v) in &defaults {
            row.insert(format!("default_{k}"), v.clone());
        }
        row.insert("effect_key".into(), Value::String(key));
        if let Some(e) = effect.as_object() {
            for f in COPIED_FIELDS {
                if let Some(v) = e.get(f) {
                    row.insert(f.to_string(), v.clone());
                }
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

fn collect_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        vec![abspath(path)]
    } else if path.is_dir() {
        glob_children(path, "sfx_", ".json")
            .into_iter()
            .map(|p| abspath(&p))
            .collect()
    } else {
        Vec::new()
    }
}

fn execute(a: &Args) -> anyhow::Result<()> {
    let quiet = a.json || a.output.is_none();
    let resolved = if a.path == DEFAULT_PATH {
        workspace_root().join("configs")
    } else {
        abspath(Path::new(&a.path))
    };

    let files = collect_files(&resolved);
    if files.is_empty() {
        log::error(&format!(
            "No sfx_*.json files found at: {}",
            resolved.display()
        ));
        return Ok(());
    }
    if !quiet {
        log::info(&format!("Processing {} sfx config file(s)…", files.len()));
    }

    let mut all_rows = Vec::new();
    for p in &files {
        match flatten(p) {
            Ok(rows) => {
                if !quiet {
                    log::info(&format!("  {} → {} effect(s)", basename(p), rows.len()));
                }
                all_rows.extend(rows);
            }
            Err(e) => log::warning(&format!("Skipping {} — {e}", basename(p))),
        }
    }

    if all_rows.is_empty() {
        if !quiet {
            log::info("No effect rows produced.");
        }
        return Ok(());
    }

    if a.json {
        let arr = Value::Array(all_rows.into_iter().map(Value::Object).collect());
        println!("{}", dumps(&arr, Style::INDENT2));
        return Ok(());
    }

    let cols = all_cols();
    if let Some(out) = &a.output {
        let mut f = fs::File::create(out)?;
        pycsv::write_dicts(&mut f, &cols, &all_rows)?;
        log::info(&format!(
            "Written: {}  ({} rows)",
            out.display(),
            all_rows.len()
        ));
    } else {
        let mut so = std::io::stdout().lock();
        pycsv::write_dicts(&mut so, &cols, &all_rows)?;
        so.flush()?;
    }

    if !quiet {
        log::info(&format!(
            "Total: {} effect rows from {} file(s)",
            all_rows.len(),
            files.len()
        ));
    }
    Ok(())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("sfx-csv");
    let a: Args = match super::parse_or_exit("xil-sfx-csv", args) {
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

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_orders_columns_then_extra_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("sfx_S01E01.json");
        fs::write(
            &p,
            r#"{"show":"S","season":1,"episode":1,"defaults":{"prompt_influence":0.3,"odd_key":7},
                "effects":{"BEAT":{"type":"silence","duration_seconds":1.0},"SFX: X":{"prompt":"p","loop":true}}}"#,
        )
        .unwrap();
        let rows = flatten(&p).unwrap();
        assert_eq!(rows.len(), 2);
        let keys: Vec<&str> = rows[0].keys().map(String::as_str).collect();
        assert_eq!(keys[0], "file");
        assert_eq!(keys[5], "effect_key");
        assert_eq!(*keys.last().unwrap(), "default_odd_key");
        assert_eq!(rows[0]["tag"], "S01E01");
        assert_eq!(rows[0]["effect_key"], "BEAT");
        assert_eq!(rows[0]["duration_seconds"], 1.0);
        assert_eq!(rows[0]["prompt"], "");
        assert_eq!(rows[0]["default_prompt_influence"], 0.3);
        assert_eq!(rows[1]["loop"], true);
    }

    #[test]
    fn tag_override_wins_even_when_null() {
        let mut d = Map::new();
        assert_eq!(tag_from_path(Path::new("/c/sfx_S01E01.json"), &d), "S01E01");
        assert_eq!(tag_from_path(Path::new("/c/other.json"), &d), "other");
        d.insert("tag_override".into(), Value::Null);
        assert_eq!(
            tag_from_path(Path::new("/c/sfx_S01E01.json"), &d),
            Value::Null
        );
    }
}

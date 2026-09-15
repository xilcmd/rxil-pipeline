//! `xil cleanup` — delete stems whose seq no longer matches the parsed
//! script. Port of `XILP008_stale_stem_cleanup.py`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value};
use xil_core::fsutil::{basename, glob_children};
use xil_core::stems::{entries_index, expected_stem_basename, extract_seq};
use xil_core::workspace::{derive_paths, resolve_slug};
use xil_core::{banner, log};

#[derive(Parser)]
#[command(
    name = "xil-cleanup",
    about = "Remove stale stems that no longer match the current parsed script.  Use --dry-run first to \
             review what would be deleted."
)]
struct Args {
    /// Episode tag (e.g. S02E03); derives --parsed and --stems
    #[arg(long, value_name = "TAG")]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01); same as --episode
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Parsed script JSON (overrides --episode)
    #[arg(long, value_name = "PATH")]
    parsed: Option<PathBuf>,
    /// Stems directory (overrides --episode)
    #[arg(long, value_name = "DIR")]
    stems: Option<PathBuf>,
    /// List stale stems without deleting them
    #[arg(long)]
    dry_run: bool,
}

/// argparse's own usage block, wrapped at its default 80 columns. The
/// three `parser.error` sites print this verbatim before their message,
/// and clap's usage rendering is not the same shape.
const USAGE: &str = "usage: xil-cleanup [-h] [--episode TAG] [--tag TAG] [--show SHOW]\n\
                     \x20                  [--parsed PATH] [--stems DIR] [--dry-run]";

/// `parser.error(msg)`: usage on stderr, the message, exit 2.
fn parser_error(msg: &str) -> i32 {
    eprintln!("{USAGE}");
    eprintln!("xil-cleanup: error: {msg}");
    2
}

/// One stale stem and why it is stale.
pub struct Stale {
    pub path: PathBuf,
    pub reason: String,
}

/// Stems that disagree with the current parsed entries.
///
/// A stem is stale when its seq is gone, its seq is now a header, its
/// `_sfx`/speaker suffix contradicts the entry type, its speaker changed,
/// or it is a duplicate of the canonically named file for that seq.
pub fn find_stale_stems(
    stems_dir: &Path,
    index: &std::collections::HashMap<i64, Map<String, Value>>,
) -> Vec<Stale> {
    let mut by_seq: BTreeMap<i64, Vec<PathBuf>> = BTreeMap::new();
    for path in glob_children(stems_dir, "", ".mp3") {
        if let Some(seq) = extract_seq(&path) {
            by_seq.entry(seq).or_default().push(path);
        }
    }

    let mut stale = Vec::new();
    for (seq, paths) in by_seq {
        let Some(entry) = index.get(&seq) else {
            for p in paths {
                stale.push(Stale {
                    path: p,
                    reason: "seq not in parsed JSON".into(),
                });
            }
            continue;
        };
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");

        // Header entries never have stems.
        if entry_type != "dialogue" && entry_type != "direction" {
            for p in paths {
                stale.push(Stale {
                    path: p,
                    reason: format!("seq {seq} is now a {entry_type} entry"),
                });
            }
            continue;
        }

        if paths.len() > 1 {
            let expected = expected_stem_basename(entry);
            for p in paths {
                if stem_of(&p) != expected {
                    stale.push(Stale {
                        path: p,
                        reason: format!("seq {seq} duplicate (expected {expected})"),
                    });
                }
            }
            continue;
        }

        let path = paths.into_iter().next().expect("one path");
        let base = stem_of(&path);
        let suffix = base.rsplit('_').next().unwrap_or("");
        let is_sfx_stem = suffix == "sfx";

        if is_sfx_stem && entry_type == "dialogue" {
            stale.push(Stale {
                path,
                reason: format!("seq {seq} is now a dialogue entry"),
            });
        } else if !is_sfx_stem && entry_type == "direction" {
            stale.push(Stale {
                path,
                reason: format!("seq {seq} is now a direction entry"),
            });
        } else if entry_type == "dialogue" {
            let speaker = entry.get("speaker").and_then(Value::as_str).unwrap_or("");
            if !base.ends_with(&format!("_{speaker}")) {
                stale.push(Stale {
                    path,
                    reason: format!("seq {seq} speaker is now {speaker}"),
                });
            }
        }
    }
    stale
}

/// `os.path.splitext(os.path.basename(p))[0]`.
fn stem_of(p: &Path) -> String {
    let name = basename(p);
    match name.rfind('.') {
        Some(dot) if dot > 0 => name[..dot].to_string(),
        _ => name,
    }
}

/// Pad to `width` by character count (`f"{s:40s}"`).
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let (parsed_path, stems_dir) = match a.episode.as_ref().or(a.tag.as_ref()) {
        Some(tag) => {
            let slug = resolve_slug(a.show.as_deref(), "project.json");
            let p = derive_paths(&slug, tag);
            (
                a.parsed.clone().unwrap_or_else(|| p["parsed"].clone()),
                a.stems.clone().unwrap_or_else(|| p["stems"].clone()),
            )
        }
        None => match (&a.parsed, &a.stems) {
            (Some(p), Some(s)) => (p.clone(), s.clone()),
            _ => {
                return Ok(parser_error(
                    "Provide --episode, or both --parsed and --stems.",
                ))
            }
        },
    };

    if !parsed_path.is_file() {
        return Ok(parser_error(&format!(
            "Parsed JSON not found: {}",
            parsed_path.display()
        )));
    }
    if !stems_dir.is_dir() {
        return Ok(parser_error(&format!(
            "Stems directory not found: {}",
            stems_dir.display()
        )));
    }

    let parsed: Value = serde_json::from_str(&fs::read_to_string(&parsed_path)?)?;
    let index = entries_index(&parsed);
    let stale = find_stale_stems(&stems_dir, &index);

    if stale.is_empty() {
        log::info("No stale stems found — stems directory is clean.");
        return Ok(0);
    }

    let label = if a.dry_run { "[DRY RUN] " } else { "" };
    log::info(&format!("\n{label}Stale stems ({}):\n", stale.len()));
    for s in &stale {
        log::info(&format!(
            "  {}  ({})",
            pad(&basename(&s.path), 40),
            s.reason
        ));
    }
    log::info("");
    if a.dry_run {
        log::info(&format!("  {} stale stems would be deleted.", stale.len()));
        log::info("  Re-run without --dry-run to delete them.");
    } else {
        for s in &stale {
            fs::remove_file(&s.path)?;
        }
        log::info(&format!("  Deleted {} stale stems.", stale.len()));
    }
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("cleanup");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-cleanup", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
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
    use std::collections::HashMap;

    fn index(v: Value) -> HashMap<i64, Map<String, Value>> {
        entries_index(&v)
    }

    fn touch(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), "x").unwrap();
    }

    #[test]
    fn every_staleness_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        touch(d, "001_cold-open_adam.mp3"); // fine
        touch(d, "002_cold-open_sfx.mp3"); // now dialogue
        touch(d, "003_cold-open_adam.mp3"); // now direction
        touch(d, "004_cold-open_adam.mp3"); // speaker changed
        touch(d, "005_cold-open_sfx.mp3"); // now a header
        touch(d, "006_cold-open_adam.mp3"); // seq gone
        touch(d, "007_cold-open_adam.mp3"); // duplicate pair
        touch(d, "007_act1_maya.mp3");
        touch(d, "notaseq.mp3"); // unparseable, ignored

        let idx = index(json!({"entries": [
            {"seq": 1, "type": "dialogue", "section": "cold-open", "speaker": "adam"},
            {"seq": 2, "type": "dialogue", "section": "cold-open", "speaker": "adam"},
            {"seq": 3, "type": "direction", "section": "cold-open"},
            {"seq": 4, "type": "dialogue", "section": "cold-open", "speaker": "maya"},
            {"seq": 5, "type": "section_header", "section": "cold-open"},
            {"seq": 7, "type": "dialogue", "section": "cold-open", "speaker": "adam"}
        ]}));

        let stale = find_stale_stems(d, &idx);
        let got: Vec<(String, String)> = stale
            .iter()
            .map(|s| (basename(&s.path), s.reason.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "002_cold-open_sfx.mp3".to_string(),
                    "seq 2 is now a dialogue entry".to_string()
                ),
                (
                    "003_cold-open_adam.mp3".into(),
                    "seq 3 is now a direction entry".into()
                ),
                (
                    "004_cold-open_adam.mp3".into(),
                    "seq 4 speaker is now maya".into()
                ),
                (
                    "005_cold-open_sfx.mp3".into(),
                    "seq 5 is now a section_header entry".into()
                ),
                (
                    "006_cold-open_adam.mp3".into(),
                    "seq not in parsed JSON".into()
                ),
                (
                    "007_act1_maya.mp3".into(),
                    "seq 7 duplicate (expected 007_cold-open_adam)".into()
                ),
            ]
        );
    }

    #[test]
    fn a_clean_directory_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "001_cold-open_adam.mp3");
        let idx = index(
            json!({"entries": [{"seq": 1, "type": "dialogue", "section": "cold-open", "speaker": "adam"}]}),
        );
        assert!(find_stale_stems(tmp.path(), &idx).is_empty());
    }
}

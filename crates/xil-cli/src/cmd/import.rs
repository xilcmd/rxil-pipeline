//! `xil import` — extract an ElevenLabs Studio export ZIP into pipeline
//! stems. Port of `XILP010_studio_import.py`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use clap::Parser;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use xil_core::workspace::{derive_paths, resolve_slug};
use xil_core::{banner, log};

use super::migrate::make_stem_name;

const SCRIPT_NAME: &str = "XILP010 · Studio Import";

#[derive(Parser)]
#[command(
    name = "xil-import",
    about = "Import ElevenLabs Studio export ZIP into pipeline stems."
)]
#[command(group(clap::ArgGroup::new("tag_group").required(true).args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S02E02) — derives parsed JSON and stems dir
    #[arg(long)]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Path to the ElevenLabs Studio export ZIP file
    #[arg(long = "zip", required = true)]
    zip_path: String,
    /// Override parsed JSON path (default: parsed/parsed_<slug>_{TAG}.json)
    #[arg(long)]
    parsed: Option<String>,
    /// Override stems output directory (default: stems/{TAG})
    #[arg(long)]
    stems_dir: Option<String>,
    /// Show extraction plan without writing files
    #[arg(long)]
    dry_run: bool,
    /// Overwrite existing stems on disk
    #[arg(long)]
    force: bool,
    /// Include SFX direction entries
    #[arg(long)]
    gen_sfx: bool,
    /// Include MUSIC direction entries
    #[arg(long)]
    gen_music: bool,
    /// Include BEAT direction entries
    #[arg(long)]
    gen_beats: bool,
    /// Include all direction types (SFX, MUSIC, BEAT, AMBIENCE)
    #[arg(long = "all")]
    all_types: bool,
}

/// `_parse_zip_seq`: the leading `NNN_` of a member's basename.
fn parse_zip_seq(member: &str) -> Option<i64> {
    let base = member.rsplit('/').next().unwrap_or(member);
    let digits: String = base.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || !base[digits.len()..].starts_with('_') {
        return None;
    }
    digits.parse().ok()
}

#[derive(Default)]
struct Stats {
    extracted: usize,
    skipped_exists: usize,
    skipped_type: usize,
    skipped_header: usize,
    missing_seq: usize,
}

fn extract_stems(
    zip_path: &Path,
    entries: &[Map<String, Value>],
    stems_dir: &str,
    dry_run: bool,
    force: bool,
    include: &BTreeSet<&str>,
) -> anyhow::Result<Stats> {
    let mut by_seq: IndexMap<i64, &Map<String, Value>> = IndexMap::new();
    for e in entries {
        if let Some(seq) = e.get("seq").and_then(Value::as_i64) {
            by_seq.insert(seq, e);
        }
    }
    let mut stats = Stats::default();
    if !dry_run {
        fs::create_dir_all(stems_dir)?;
    }
    let mut zf = zip::ZipArchive::new(fs::File::open(zip_path)?)?;
    let mut members: Vec<String> = zf.file_names().map(str::to_string).collect();
    members.sort();
    for member in members {
        if !member.to_lowercase().ends_with(".mp3") {
            continue;
        }
        let Some(seq) = parse_zip_seq(&member) else {
            continue;
        };
        let Some(entry) = by_seq.get(&seq) else {
            stats.missing_seq += 1;
            log::info(&format!(
                "  [MISSING]  {member}  → seq {seq} not in parsed JSON"
            ));
            continue;
        };
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let text: String = entry
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(50)
            .collect();
        let direction_type = entry
            .get("direction_type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if entry_type == "section_header" || entry_type == "scene_header" {
            stats.skipped_header += 1;
            log::info(&format!("  [HEADER]   {member}  — {entry_type}: {text}"));
            continue;
        }
        if entry_type == "direction" && !include.contains(direction_type) {
            stats.skipped_type += 1;
            let label = if direction_type.is_empty() {
                String::new()
            } else {
                format!("{direction_type}: ")
            };
            log::info(&format!("  [SKIP]     {member}  — {label}{text}"));
            continue;
        }
        let stem_name = make_stem_name(entry);
        let dest = Path::new(stems_dir).join(&stem_name);
        if dest.exists() && !force {
            stats.skipped_exists += 1;
            log::info(&format!("  [EXISTS]   {member}  → {stem_name}"));
            continue;
        }
        let marker = if dry_run { "DRY-RUN" } else { "EXTRACT" };
        let speaker = entry
            .get("speaker")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("sfx");
        log::info(&format!(
            "  [{marker}]  {member}  → {stem_name}  ({speaker}: {text})"
        ));
        if !dry_run {
            let mut data = Vec::new();
            zf.by_name(&member)?.read_to_end(&mut data)?;
            fs::write(&dest, data)?;
        }
        stats.extracted += 1;
    }
    Ok(stats)
}

fn print_summary(s: &Stats, dry_run: bool) {
    let mode = if dry_run { "DRY-RUN" } else { "COMPLETE" };
    let rule = "─".repeat(50);
    log::info(&format!("\n{rule}"));
    log::info(&format!("  SUMMARY ({mode})"));
    log::info(&rule);
    log::info(&format!("  Extracted:       {:>4}", s.extracted));
    log::info(&format!("  Skipped (exist): {:>4}", s.skipped_exists));
    log::info(&format!("  Skipped (type):  {:>4}", s.skipped_type));
    log::info(&format!("  Skipped (header):{:>4}", s.skipped_header));
    if s.missing_seq > 0 {
        log::info(&format!("  Missing seq:     {:>4}  ⚠", s.missing_seq));
    }
    log::info("");
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("import");
    let a: Args = match super::parse_or_exit("xil-import", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let mut include: BTreeSet<&str> = BTreeSet::new();
    if a.gen_sfx || a.all_types {
        include.insert("SFX");
    }
    if a.gen_music || a.all_types {
        include.insert("MUSIC");
    }
    if a.gen_beats || a.all_types {
        include.insert("BEAT");
    }
    if a.all_types {
        include.insert("AMBIENCE");
    }
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let p = derive_paths(&slug, &tag);
    let parsed_path: PathBuf = a
        .parsed
        .clone()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| p["parsed"].clone());
    let stems_dir = a
        .stems_dir
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p["stems"].to_string_lossy().into_owned());

    let _banner = banner::begin(SCRIPT_NAME, &super::argv_line(args));
    if !Path::new(&a.zip_path).is_file() {
        log::error(&format!("ZIP file not found: {}", a.zip_path));
        return Ok(0);
    }
    if !parsed_path.is_file() {
        log::error(&format!("Parsed JSON not found: {}", parsed_path.display()));
        return Ok(0);
    }
    let parsed: Value = serde_json::from_str(&fs::read_to_string(&parsed_path)?)?;
    let entries: Vec<Map<String, Value>> = parsed["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e.as_object().cloned())
        .collect();
    let count = |t: &str| {
        entries
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some(t))
            .count()
    };
    let (total, dialogue, direction) = (entries.len(), count("dialogue"), count("direction"));
    log::info(&format!("  Episode:    {tag}"));
    log::info(&format!("  ZIP:        {}", a.zip_path));
    log::info(&format!(
        "  Parsed:     {}  ({total} entries)",
        parsed_path.display()
    ));
    log::info(&format!("  Stems dir:  {stems_dir}"));
    log::info(&format!(
        "  Entries:    {dialogue} dialogue, {direction} directions, {} headers",
        total - dialogue - direction
    ));
    let mut mode = Vec::new();
    if a.dry_run {
        mode.push("dry-run".to_string());
    }
    if a.force {
        mode.push("force".to_string());
    }
    if !include.is_empty() {
        mode.push(format!(
            "include: {}",
            include.iter().copied().collect::<Vec<_>>().join(", ")
        ));
    }
    if !mode.is_empty() {
        log::info(&format!("  Mode:       {}", mode.join(", ")));
    }
    log::info("");
    let stats = extract_stems(
        Path::new(&a.zip_path),
        &entries,
        &stems_dir,
        a.dry_run,
        a.force,
        &include,
    )?;
    print_summary(&stats, a.dry_run);
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
    fn zip_member_sequence() {
        assert_eq!(parse_zip_seq("042_Chapter 1.mp3"), Some(42));
        assert_eq!(parse_zip_seq("export/007_Chapter 2.mp3"), Some(7));
        assert_eq!(parse_zip_seq("Chapter_1.mp3"), None);
        assert_eq!(parse_zip_seq("12.mp3"), None);
    }
}

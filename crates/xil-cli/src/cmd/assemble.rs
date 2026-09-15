//! `xil assemble` — mix stems straight to a master MP3: a two-pass
//! foreground/background mix when a parsed script exists, a plain
//! sequential join otherwise. Port of `XILP003_audio_assembly.py`.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::anyhow;
use clap::Parser;
use indexmap::IndexMap;
use xil_audio::segment::Segment;
use xil_core::fsutil::abspath;
use xil_core::workspace::{derive_paths, resolve_slug, workspace_root};
use xil_core::{banner, log};

use crate::mix::config::{CastConfig, SfxConfig, Voice};
use crate::mix::{self, AMBIENCE_LEVEL_DB, MUSIC_LEVEL_DB};

const SILENCE_GAP_MS: i64 = 600;

#[derive(Parser)]
#[command(
    name = "xil-assemble",
    about = "Audio Assembly — assemble voice stems into master MP3"
)]
#[command(group(clap::ArgGroup::new("tag_group").required(true).args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S01E01) — derives cast config path
    #[arg(long)]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Output master MP3 path (default: <slug>_<TAG>_master.mp3)
    #[arg(long)]
    output: Option<String>,
    /// Path to parsed script JSON (default: parsed/parsed_<slug>_<TAG>.json)
    #[arg(long)]
    parsed: Option<String>,
    /// Silence gap between foreground stems in ms (default: 600)
    #[arg(long, default_value_t = SILENCE_GAP_MS, allow_negative_numbers = true)]
    gap_ms: i64,
}

/// `export(final_output, format="mp3")` — pydub opens the destination
/// before encoding, so an unwritable path fails first.
fn export_mp3(seg: &Segment, out: &str) -> anyhow::Result<()> {
    fs::File::create(out)?;
    seg.export_ffmpeg(Path::new(out), "mp3", &[])?;
    Ok(())
}

/// `subprocess.run(["mpg123", abspath(out)], check=False)` — a missing
/// player raises `FileNotFoundError` after the file is written.
fn play(out: &str) -> anyhow::Result<()> {
    let abs = abspath(Path::new(out));
    match Command::new("mpg123").arg(&abs).status() {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "FileNotFoundError: [Errno 2] No such file or directory: 'mpg123'"
        )),
        Err(e) => Err(e.into()),
    }
}

/// `assemble_audio` — the sequential fallback.
fn assemble_audio(
    cast: &IndexMap<String, Voice>,
    stems_dir: &str,
    out: &str,
    gap_ms: i64,
) -> anyhow::Result<()> {
    let mut stems: Vec<String> = xil_core::fsutil::glob_children(Path::new(stems_dir), "", ".mp3")
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    stems.sort();
    if stems.is_empty() {
        log::warning(&format!(
            "No stems found in {stems_dir}/. Run XILP002 first."
        ));
        return Ok(());
    }
    log::info(&format!(
        "--- Phase 2: Assembling {} stems (sequential) ---",
        stems.len()
    ));
    let mut full = Segment::empty();
    for stem in &stems {
        let speaker = mix::stem_basename(stem)
            .rsplit('_')
            .next()
            .unwrap_or("")
            .to_string();
        log::info(&format!("   Loading: {stem} ({speaker})"));
        let mut segment = Segment::from_file(Path::new(stem))?;
        if let Some(v) = cast.get(&speaker) {
            segment = mix::apply_speaker_filters(segment, &v.filter)?;
            segment = segment.pan(v.pan);
        }
        full = full.append(segment.append(Segment::silent(gap_ms as f64)));
    }
    export_mp3(&full, out)?;
    log::info(&format!(
        "--- Success! Created: {out} (Duration: {:.1}s) ---",
        full.len_ms() as f64 / 1000.0
    ));
    play(out)
}

/// `assemble_multitrack` — foreground plus ducked ambience and music.
fn assemble_multitrack(
    cast: &IndexMap<String, Voice>,
    stems_dir: &str,
    parsed_path: &Path,
    out: &str,
    sfx: Option<&SfxConfig>,
    gap_ms: i64,
) -> anyhow::Result<()> {
    let index = mix::load_entries_index(parsed_path)?;
    let plans = mix::collect_stem_plans(Path::new(stems_dir), &index, sfx);
    if plans.is_empty() {
        log::warning(&format!(
            "No stems found in {stems_dir}/. Run XILP002 first."
        ));
        return Ok(());
    }
    log::info(&format!(
        "--- Phase 2: Assembling {} stems (multi-track) ---",
        plans.len()
    ));
    let vintage_scenes: Vec<String> = sfx.map(|s| s.vintage_scenes.clone()).unwrap_or_default();
    let (foreground, timeline) = mix::build_foreground(&plans, cast, gap_ms, &vintage_scenes)?;
    if foreground.len_ms() == 0 {
        log::warning("No foreground stems found — only background stems present.");
        return Ok(());
    }
    let total_ms = foreground.len_ms();
    let bg = plans.iter().filter(|p| p.is_background()).count();
    let master = if bg > 0 {
        log::info(&format!(
            "   Mixing {bg} background stems (ambience/music)..."
        ));
        let (ambience, _) =
            mix::build_ambience_layer(&plans, &timeline, total_ms, AMBIENCE_LEVEL_DB)?;
        let (music, _) =
            mix::build_music_layer(&plans, &timeline, total_ms, MUSIC_LEVEL_DB, false)?;
        let background = ambience.overlay(music, 0);
        foreground.overlay(background, 0)
    } else {
        log::info("   No background stems found — skipping overlay pass.");
        foreground
    };
    export_mp3(&master, out)?;
    log::info(&format!(
        "--- Success! Created: {out} (Duration: {:.1}s) ---",
        master.len_ms() as f64 / 1000.0
    ));
    play(out)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("assemble");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-assemble", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    execute(&a)?;
    Ok(0)
}

fn execute(a: &Args) -> anyhow::Result<()> {
    let arg_tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let p = derive_paths(&slug, &arg_tag);
    if !p["cast"].exists() {
        log::error(&format!("Cast config not found: {}", p["cast"].display()));
        log::info("Run XILP001 first or check your --episode flag.");
        return Ok(());
    }
    let cast = CastConfig::load(&p["cast"])?;
    let stems_dir = mix::join(&workspace_root().join("stems").join(&slug), &cast.tag);
    let output = a
        .output
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p["master"].to_string_lossy().into_owned());
    let parsed_path = a
        .parsed
        .clone()
        .filter(|s| !s.is_empty())
        .map(Into::into)
        .unwrap_or_else(|| p["parsed"].clone());
    let sfx = if p["sfx"].exists() {
        Some(SfxConfig::load(&p["sfx"])?)
    } else {
        None
    };

    if parsed_path.exists() {
        assemble_multitrack(
            &cast.cast,
            &stems_dir,
            &parsed_path,
            &output,
            sfx.as_ref(),
            a.gap_ms,
        )
    } else {
        log::info(&format!(
            "   No parsed JSON at {} — using sequential assembly.",
            xil_audio::fx::py_repr(&parsed_path.to_string_lossy())
        ));
        assemble_audio(&cast.cast, &stems_dir, &output, a.gap_ms)
    }
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

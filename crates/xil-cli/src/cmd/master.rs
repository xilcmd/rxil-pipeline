//! `xil master` — overlay the DAW layer WAVs into one stereo 48 kHz VBR
//! MP3 with ID3 tags and cover art. Port of `XILP011_master_export.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::Value;
use xil_audio::segment::Segment;
use xil_audio::tags::tag_mp3;
use xil_core::workspace::{derive_paths, resolve_slug, workspace_root};
use xil_core::{banner, log};

use crate::mix::config::CastConfig;

const LAYER_SUFFIXES: [&str; 5] = ["dialogue", "ambience", "music", "sfx", "vintage_filter"];
const SAMPLE_RATE: u32 = 48000;

#[derive(Parser)]
#[command(
    name = "xil-master",
    about = "Final Master MP3 Export — mix DAW layers into a single podcast-ready MP3"
)]
#[command(group(clap::ArgGroup::new("tag_group").required(true).args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S02E03) — derives DAW layer paths
    #[arg(long)]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// DAW layer directory (default: daw/<TAG>/)
    #[arg(long)]
    daw_dir: Option<String>,
    /// Output MP3 path (default: masters/<TAG>_<slug>_<date>.mp3)
    #[arg(long)]
    output: Option<String>,
    /// Show what would be exported without writing files
    #[arg(long)]
    dry_run: bool,
}

/// `_find_cover_art(slug)`.
fn find_cover_art(slug: &str) -> Option<PathBuf> {
    let dir = workspace_root().join("configs").join(slug);
    [
        "cover_art.PNG",
        "cover_art.png",
        "cover_art.jpg",
        "cover_art.jpeg",
    ]
    .iter()
    .map(|n| dir.join(n))
    .find(|p| p.exists())
}

/// `load_layer_wavs(daw_dir, tag)`.
fn load_layer_wavs(daw_dir: &str, tag: &str) -> Vec<(&'static str, String)> {
    LAYER_SUFFIXES
        .iter()
        .map(|s| (*s, Path::new(daw_dir).join(format!("{tag}_layer_{s}.wav"))))
        .filter(|(_, p)| p.exists())
        .map(|(s, p)| (s, p.to_string_lossy().into_owned()))
        .collect()
}

/// `mix_layers(layer_paths)`.
fn mix_layers(layers: &[(&str, String)]) -> anyhow::Result<Segment> {
    let mut combined: Option<Segment> = None;
    for (_, path) in layers {
        let seg = Segment::from_file(Path::new(path))?;
        combined = Some(match combined {
            None => seg,
            Some(c) => c.overlay(seg, 0),
        });
    }
    Ok(combined.expect("at least one layer"))
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("master");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-master", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    execute(&a)?;
    Ok(0)
}

fn execute(a: &Args) -> anyhow::Result<()> {
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let p = derive_paths(&slug, &tag);
    let daw_dir = a
        .daw_dir
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p["daw"].to_string_lossy().into_owned());

    let (mut show_name, mut episode_title, mut artist) =
        (None::<String>, None::<String>, None::<String>);
    if p["cast"].exists() {
        let cast = CastConfig::load(&p["cast"])?;
        show_name = Some(cast.show);
        episode_title = cast.title;
        artist = Some(cast.artist);
    }
    if (show_name.is_none() || episode_title.is_none()) && p["parsed"].exists() {
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&p["parsed"])?)?;
        let field = |k: &str| match parsed.get(k) {
            Some(Value::Null) | None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(other) => Some(other.to_string()),
        };
        if show_name.is_none() {
            show_name = field("show");
        }
        if episode_title.is_none() {
            episode_title = field("title");
        }
        let truthy = |v: &Option<String>| v.as_ref().is_some_and(|s| !s.is_empty());
        if truthy(&show_name) || truthy(&episode_title) {
            log::info("  Metadata sourced from parsed JSON (cast config fields absent)");
        }
    }
    let show_name = show_name
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Sample Show".into());
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let output_path = match a.output.as_ref().filter(|s| !s.is_empty()) {
        Some(o) => o.clone(),
        None => {
            let masters = workspace_root().join("masters");
            fs::create_dir_all(&masters)?;
            masters
                .join(format!("{tag}_{slug}_{today}.mp3"))
                .to_string_lossy()
                .into_owned()
        }
    };

    let layers = load_layer_wavs(&daw_dir, &tag);
    let missing: Vec<&str> = LAYER_SUFFIXES
        .iter()
        .copied()
        .filter(|s| !layers.iter().any(|(n, _)| n == s))
        .collect();

    log::info(&format!("  Episode    : {tag}"));
    log::info(&format!("  Show       : {show_name} (slug: {slug})"));
    log::info(&format!("  DAW dir    : {daw_dir}"));
    log::info(&format!("  Output     : {output_path}"));
    log::info(&format!(
        "  Format     : Stereo, {SAMPLE_RATE} Hz, VBR MP3 (~145-185 kbps)"
    ));
    log::info(&format!(
        "  Layers     : {}/{} found",
        layers.len(),
        LAYER_SUFFIXES.len()
    ));
    for (name, path) in &layers {
        log::info(&format!("    [{name:>9}] {path}"));
    }
    if !missing.is_empty() {
        log::info(&format!("  Missing    : {}", missing.join(", ")));
    }
    log::info("");

    if layers.is_empty() {
        log::warning("No layer WAVs found. Run XILP005 first.");
        return Ok(());
    }
    if a.dry_run {
        log::info("--- Dry run — no files written ---");
        return Ok(());
    }

    log::info("--- Mixing layers ---");
    let combined = mix_layers(&layers)?;
    let duration_s = combined.len_ms() as f64 / 1000.0;
    let minutes = (duration_s / 60.0).floor() as i64;
    let seconds = duration_s.rem_euclid(60.0);
    log::info(&format!("  Duration   : {minutes}:{seconds:05.2}"));
    log::info("--- Exporting master MP3 ---");

    let title = match episode_title.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!("{show_name} — {t}"),
        None => tag.clone(),
    };
    let cover = find_cover_art(&slug);
    if let Some(c) = &cover {
        log::info(&format!("  Cover art  : {}", c.display()));
    }

    // export_master: stereo, 48 kHz, LAME VBR q2, then ID3.
    let out = Path::new(&output_path);
    fs::File::create(out)?;
    let mut combined = combined;
    if combined.channels == 1 {
        combined = combined.set_channels(2);
    }
    let combined = combined.set_frame_rate(SAMPLE_RATE);
    combined.export_ffmpeg(out, "mp3", &["-q:a", "2", "-ar", "48000"])?;
    drop(combined);
    let title_or_tag = if title.is_empty() { tag.clone() } else { title };
    tag_mp3(
        out,
        &show_name,
        Some(&title_or_tag),
        artist.as_deref(),
        cover.as_deref(),
    )?;

    let size_mb = fs::metadata(out)?.len() as f64 / (1024.0 * 1024.0);
    log::info(&format!("  Written    : {output_path} ({size_mb:.1} MB)"));
    log::info("--- Done! ---");
    Ok(())
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

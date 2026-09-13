//! `xil daw` — export an episode as five aligned DAW layer WAVs, Audacity
//! label tracks, an import helper script and a timeline. Port of
//! `XILP005_daw_export.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::bail;
use clap::Parser;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use xil_audio::tags::tag_wav;
use xil_core::fsutil::abspath;
use xil_core::workspace::{derive_paths, resolve_slug, show_slug, workspace_root, DEFAULT_SLUG};
use xil_core::{banner, log};

use crate::mix::config::{CastConfig, SfxConfig, Voice};
use crate::mix::timeline::{
    build_timeline_data, render_html_timeline, render_terminal_timeline, render_text_timeline_map,
};
use crate::mix::{self, Label, StemPlan};

const SILENCE_GAP_MS: i64 = 600;

const SCRIPT_TEMPLATE: &str = include_str!("../mix/templates/open_in_audacity.py.tmpl");
const SCRIPT_TEMPLATE_AUP3: &str = include_str!("../mix/templates/open_in_audacity_aup3.py.tmpl");

/// `(key, filename suffix, description)` for each layer.
const LAYERS: [(&str, &str, &str); 5] = [
    (
        "dialogue",
        "layer_dialogue",
        "Spoken dialogue (audio filter chain + pan applied per speaker)",
    ),
    (
        "ambience",
        "layer_ambience",
        "Looped environmental background (no ducking)",
    ),
    (
        "music",
        "layer_music",
        "Music stings and themes (no ducking)",
    ),
    (
        "sfx",
        "layer_sfx",
        "One-shot sound effects and beat silences",
    ),
    (
        "vintage_filter",
        "layer_vintage_filter",
        "Record player crackle (vintage filter active spans)",
    ),
];

#[derive(Parser)]
#[command(
    name = "xil-daw",
    about = "DAW Export — export episode as layered WAV files for Audacity"
)]
#[command(group(clap::ArgGroup::new("tag_group").required(true).args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S01E02) — derives cast config, stems, and parsed JSON paths
    #[arg(long)]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Path to parsed script JSON (default: parsed/parsed_<slug>_<TAG>.json)
    #[arg(long)]
    parsed: Option<String>,
    /// Output directory for layer WAVs (default: daw/<TAG>/)
    #[arg(long)]
    output_dir: Option<String>,
    /// Show export summary without writing files
    #[arg(long)]
    dry_run: bool,
    /// Include SaveProject2 step in the Audacity helper script (requires mod-script-pipe; Audacity 3 only)
    #[arg(long)]
    save_aup3: bool,
    /// Write an Audacity macro to %APPDATA%\audacity\Macros\ for one-click import (Audacity 3 only — Audacity 4 has no Macro Manager)
    #[arg(long = "macro")]
    macro_: bool,
    /// Print an ASCII timeline visualization of asset placement to stdout
    #[arg(long)]
    timeline: bool,
    /// Write an interactive HTML timeline to daw/<TAG>/<TAG>_timeline.html
    #[arg(long)]
    timeline_html: bool,
    /// Silence gap between foreground stems in ms (default: 600)
    #[arg(long, default_value_t = SILENCE_GAP_MS, allow_negative_numbers = true)]
    gap_ms: i64,
}

/// `_validate_tag_for_script` — tags are pasted into generated Python.
fn validate_tag_for_script(tag: &str) -> anyhow::Result<()> {
    let ok = !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        bail!(
            "Tag {} contains characters that are not safe for script generation. Tags must match ^[A-Za-z0-9_-]+$ (letters, digits, hyphens, underscores only).",
            xil_audio::fx::py_repr(tag)
        );
    }
    Ok(())
}

/// `_write_labels`: tab-separated start, end, text.
fn write_labels(output_dir: &str, fname: &str, labels: &[Label]) -> std::io::Result<()> {
    let mut out = String::new();
    for l in labels {
        out.push_str(&format!("{:.3}\t{:.3}\t{}\n", l.start_s, l.end_s, l.text));
    }
    fs::write(Path::new(output_dir).join(fname), out)
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `_audacity_config_dir`: `%APPDATA%/audacity`, via `cmd.exe` + `wslpath` on WSL.
fn audacity_config_dir() -> Option<PathBuf> {
    let appdata = match std::env::var("APPDATA").ok().filter(|v| !v.is_empty()) {
        Some(a) => a,
        None => {
            let win = command_output("cmd.exe", &["/c", "echo %APPDATA%"])?;
            command_output("wslpath", &["-u", &win])?
        }
    };
    let dir = Path::new(&appdata).join("audacity");
    dir.is_dir().then_some(dir)
}

fn find_audacity_macros_dir() -> Option<PathBuf> {
    let dir = audacity_config_dir()?.join("Macros");
    dir.is_dir().then_some(dir)
}

/// `detect_audacity_generations()` — does this machine have Audacity 4 config?
fn has_audacity4() -> bool {
    audacity_config_dir()
        .is_some_and(|d| d.join("Audacity4.ini").exists() || d.join("Audacity4").exists())
}

fn to_windows_path(linux_path: &str) -> String {
    command_output("wslpath", &["-w", linux_path]).unwrap_or_else(|| linux_path.to_string())
}

fn macro_slug(show: &str) -> String {
    if show.is_empty() {
        DEFAULT_SLUG.to_uppercase()
    } else {
        show_slug(show).to_uppercase()
    }
}

/// `generate_audacity_macro(...)` — `None` when there is no Macros directory.
fn generate_audacity_macro(
    output_dir: &str,
    tag: &str,
    layer_files: &[(String, String)],
    show: &str,
    season_title: Option<&str>,
    episode_title: Option<&str>,
    artist: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(macros_dir) = find_audacity_macros_dir() else {
        return Ok(None);
    };
    let abs_output = abspath(Path::new(output_dir));
    let mut lines = Vec::new();
    for (_, filename) in layer_files {
        if !filename.ends_with(".wav") {
            continue;
        }
        let win = to_windows_path(&abs_output.join(filename).to_string_lossy());
        lines.push(format!("Import2: Filename=\"{win}\""));
    }
    let title = match (
        season_title.filter(|s| !s.is_empty()),
        episode_title.filter(|s| !s.is_empty()),
    ) {
        (Some(s), Some(e)) => format!("{tag}: {s} - {e}"),
        (None, Some(e)) => format!("{tag}: {e}"),
        _ => tag.to_string(),
    };
    use chrono::Datelike;
    let year = chrono::Local::now().year();
    lines.push(format!(
        "SetProject: X-Genre=\"Podcast\" X-Album=\"{show}\" X-Artist=\"{artist}\" X-Title=\"{title}\" X-Year=\"{year}\""
    ));
    let path = macros_dir.join(format!("{}_{tag}.txt", macro_slug(show)));
    fs::write(&path, lines.join("\n") + "\n")?;
    Ok(Some(path))
}

/// `repr(layer_files)` for a list of `(str, str)` tuples.
fn layers_repr(layer_files: &[(String, String)]) -> String {
    let items: Vec<String> = layer_files
        .iter()
        .map(|(a, b)| {
            format!(
                "({}, {})",
                xil_audio::fx::py_repr(a),
                xil_audio::fx::py_repr(b)
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// `_make_audacity_script(tag, layer_files, save_aup3, show)`.
fn make_audacity_script(
    tag: &str,
    layer_files: &[(String, String)],
    save_aup3: bool,
    show: &str,
) -> String {
    let template = if save_aup3 {
        SCRIPT_TEMPLATE_AUP3
    } else {
        SCRIPT_TEMPLATE
    };
    let show_label = if show.is_empty() { "Episode" } else { show };
    template
        .replace("@@XIL_LAYERS_REPR@@", &layers_repr(layer_files))
        .replace("@@XIL_SHOW_LABEL@@", show_label)
        .replace("@@XIL_TAG@@", tag)
}

/// `derive_vintage_scenes_from_parsed(parsed_path)`.
fn derive_vintage_scenes_from_parsed(parsed_path: &Path) -> Vec<String> {
    let Some(parsed) = fs::read_to_string(parsed_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Vec::new();
    };
    let mut scenes: Vec<String> = Vec::new();
    for e in parsed
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(o) = e.as_object() else { continue };
        let s = |k: &str| o.get(k).and_then(Value::as_str);
        if s("type") == Some("direction")
            && s("direction_type") == Some("VINTAGE FILTER")
            && s("text").unwrap_or("").to_uppercase() == "VINTAGE FILTER ENGAGES"
        {
            if let Some(scene) = s("scene").filter(|x| !x.is_empty()) {
                if !scenes.iter().any(|x| x == scene) {
                    scenes.push(scene.to_string());
                }
            }
        }
    }
    scenes
}

fn fmt_pct_1(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}%"))
        .unwrap_or_else(|| "100% (full)".into())
}

fn fmt_vol(v: Option<mix::config::Num>) -> String {
    v.map(|n| format!("{:.0}%", n.f()))
        .unwrap_or_else(|| "unity".into())
}

fn fmt_secs(v: Option<mix::config::Num>) -> String {
    v.map(|n| format!("{:.1}s", n.f()))
        .unwrap_or_else(|| "none".into())
}

fn speaker_filter_lines(cast: &IndexMap<String, Voice>) -> Vec<(String, String)> {
    cast.iter()
        .filter(|(_, v)| v.filter.truthy())
        .map(|(k, v)| (k.clone(), v.filter.py_str()))
        .collect()
}

/// `dry_run_daw(...)`.
fn dry_run_daw(
    tag: &str,
    plans: &[StemPlan],
    output_dir: &str,
    stems_dir: &str,
    cast: &IndexMap<String, Voice>,
    vintage_scenes: &[String],
) -> anyhow::Result<()> {
    validate_tag_for_script(tag)?;
    let bg: Vec<&StemPlan> = plans.iter().filter(|p| p.is_background()).collect();
    let count = |dt: &str| bg.iter().filter(|p| p.dt() == Some(dt)).count();
    let sfx = plans
        .iter()
        .filter(|p| matches!(p.dt(), Some("SFX") | Some("BEAT")))
        .count();
    let dialogue: Vec<&StemPlan> = plans.iter().filter(|p| p.is_dialogue()).collect();
    let vintage_count = if vintage_scenes.is_empty() {
        0
    } else {
        dialogue
            .iter()
            .filter(|p| p.scene.as_ref().is_some_and(|s| vintage_scenes.contains(s)))
            .count()
    };
    let stems_shown = if stems_dir.is_empty() {
        format!("stems/{tag}")
    } else {
        stems_dir.to_string()
    };

    log::info(&format!("\n--- DAW Export Dry Run: {tag} ---"));
    log::info(&format!("   Stems directory : {stems_shown}"));
    log::info(&format!("   Output directory: {output_dir}/"));
    log::info("");
    log::info("   Layer             Stems");
    log::info("   ─────────────────────────────");
    log::info(&format!("   dialogue          {:3} stems", dialogue.len()));
    if !vintage_scenes.is_empty() {
        log::info(&format!(
            "     vintage scenes : {}  ({vintage_count} stems — mono collapse + LPF 5kHz)",
            vintage_scenes.join(", ")
        ));
    }
    let filtered = speaker_filter_lines(cast);
    if !filtered.is_empty() {
        let parts: Vec<String> = filtered.iter().map(|(k, v)| format!("{k}={v}")).collect();
        log::info(&format!("     per-speaker    : {}", parts.join("  ")));
    }
    log::info(&format!(
        "   ambience          {:3} stems  (looped to scene boundaries)",
        count("AMBIENCE")
    ));
    log::info(&format!(
        "   music             {:3} stems  (one-shot at cue points)",
        count("MUSIC")
    ));
    log::info(&format!(
        "   vintage filter    {:3} stems  (crackle looped to DISENGAGES)",
        count("VINTAGE FILTER")
    ));
    log::info(&format!("   sfx               {sfx:3} stems"));
    log::info("");
    log::info("   Output files (all same duration as foreground track):");
    for (_, suffix, desc) in LAYERS {
        log::info(&format!("     {output_dir}/{tag}_{suffix}.wav  — {desc}"));
    }
    log::info(&format!("     {output_dir}/{tag}_open_in_audacity.py"));
    log::info("");
    Ok(())
}

struct Ctx<'a> {
    cast: &'a CastConfig,
    stems_dir: String,
    parsed_path: PathBuf,
    output_dir: String,
    tag: String,
    slug: String,
    sfx: Option<&'a SfxConfig>,
    a: &'a Args,
}

fn write_timeline_outputs(
    c: &Ctx,
    plans: &[StemPlan],
    index: &IndexMap<i64, Map<String, Value>>,
    total_ms: i64,
    timeline: &IndexMap<i64, i64>,
    layers: [Vec<Label>; 4],
) -> anyhow::Result<()> {
    let [amb, mus, sfx, vf] = layers;
    let dlg = mix::compute_dialogue_labels(plans, timeline)?;
    let td = build_timeline_data(
        &c.tag,
        total_ms as f64 / 1000.0,
        dlg,
        amb,
        mus,
        sfx,
        vf,
        mix::derive_structure_bands(index, timeline, total_ms, "section"),
        mix::derive_structure_bands(index, timeline, total_ms, "scene"),
    );
    let txt = Path::new(&c.output_dir).join(format!("{}_timeline.txt", c.tag));
    render_text_timeline_map(&td, &txt, &c.slug)?;
    log::info(&format!("    Written: {}", txt.display()));
    if c.a.timeline {
        println!("{}", render_terminal_timeline(&td));
    }
    if c.a.timeline_html {
        let html = Path::new(&c.output_dir).join(format!("{}_timeline.html", c.tag));
        render_html_timeline(
            &td,
            &html,
            Some(Path::new(&c.stems_dir)),
            &c.slug,
            &c.tag,
            Some(Path::new(&c.output_dir)),
        )?;
        log::info(&format!("    Written: {}", html.display()));
    }
    Ok(())
}

fn export_layer(
    c: &Ctx,
    layer: xil_audio::segment::Segment,
    key: &str,
    title: &str,
    layer_files: &mut Vec<(String, String)>,
) -> anyhow::Result<()> {
    let fname = format!("{}_layer_{key}.wav", c.tag);
    let wav = Path::new(&c.output_dir).join(&fname);
    layer.export_wav(&wav)?;
    drop(layer);
    tag_wav(
        &wav,
        &c.cast.show,
        Some(&format!("{} {title}", c.tag)),
        Some(&c.cast.artist),
    )?;
    layer_files.push((title.to_string(), fname.clone()));
    log::info(&format!("    Written: {}/{fname}", c.output_dir));
    Ok(())
}

fn export_labels(
    c: &Ctx,
    key: &str,
    title: &str,
    labels: &[Label],
    layer_files: &mut Vec<(String, String)>,
) -> anyhow::Result<()> {
    let fname = format!("{}_labels_{key}.txt", c.tag);
    write_labels(&c.output_dir, &fname, labels)?;
    layer_files.push((format!("Labels ({title})"), fname.clone()));
    log::info(&format!("    Written: {}/{fname}", c.output_dir));
    Ok(())
}

/// `export_daw_layers(...)`.
fn export_daw_layers(c: &Ctx) -> anyhow::Result<()> {
    validate_tag_for_script(&c.tag)?;
    let index = mix::load_entries_index(&c.parsed_path)?;
    let plans = mix::collect_stem_plans(Path::new(&c.stems_dir), &index, c.sfx);
    if plans.is_empty() {
        log::warning(&format!(
            "No stems found in {}/. Run XILP002 first.",
            c.stems_dir
        ));
        return Ok(());
    }
    log::info(&format!(
        "--- Building foreground timeline from {} stems ---",
        plans.len()
    ));
    let parsed_scenes = derive_vintage_scenes_from_parsed(&c.parsed_path);
    let config_scenes: Vec<String> = c.sfx.map(|s| s.vintage_scenes.clone()).unwrap_or_default();
    let vintage_scenes = if parsed_scenes.is_empty() {
        config_scenes.clone()
    } else {
        parsed_scenes.clone()
    };
    if !parsed_scenes.is_empty() && parsed_scenes != config_scenes && !config_scenes.is_empty() {
        log::info(&format!(
            "    vintage_scenes: using parsed JSON ({} scenes) — sfx_config list ignored",
            parsed_scenes.len()
        ));
    }
    let cast = &c.cast.cast;
    let (foreground, cue_timeline) =
        mix::build_foreground(&plans, cast, c.a.gap_ms, &vintage_scenes)?;
    if foreground.len_ms() == 0 {
        log::warning("No foreground stems — cannot determine episode duration.");
        return Ok(());
    }
    let total_ms = foreground.len_ms();
    drop(foreground);
    log::info(&format!(
        "    Episode duration: {:.1}s",
        total_ms as f64 / 1000.0
    ));
    fs::create_dir_all(&c.output_dir)?;
    let mut layer_files: Vec<(String, String)> = Vec::new();

    log::info("--- Building dialogue layer ---");
    if !vintage_scenes.is_empty() {
        let n = plans
            .iter()
            .filter(|p| {
                p.is_dialogue() && p.scene.as_ref().is_some_and(|s| vintage_scenes.contains(s))
            })
            .count();
        log::info(&format!(
            "    vintage scenes : {}  ({n} stems — mono collapse + LPF 5kHz)",
            vintage_scenes.join(", ")
        ));
    }
    for (speaker, fval) in speaker_filter_lines(cast) {
        log::info(&format!("    per-speaker    : {speaker} → {fval}"));
    }
    let (dlg, labels) =
        mix::build_dialogue_layer(&plans, &cue_timeline, total_ms, cast, &vintage_scenes)?;
    export_layer(c, dlg, "dialogue", "Dialogue", &mut layer_files)?;
    export_labels(c, "dialogue", "Dialogue", &labels, &mut layer_files)?;

    log::info("--- Building ambience layer ---");
    for plan in mix::by_seq(&plans) {
        if plan.dt() != Some("AMBIENCE") || plan.filepath.is_empty() {
            continue;
        }
        log::info(&format!(
            "    seq {}: vol={}  trim={}  ramp_in={}  ramp_out={}  {}",
            plan.seq,
            fmt_vol(plan.volume_percentage),
            fmt_pct_1(plan.play_duration),
            fmt_secs(plan.ramp_in_seconds),
            fmt_secs(plan.ramp_out_seconds),
            xil_core::fsutil::basename(Path::new(&plan.filepath))
        ));
    }
    let (amb, amb_labels) = mix::build_ambience_layer(&plans, &cue_timeline, total_ms, 0.0)?;
    export_layer(c, amb, "ambience", "Ambience", &mut layer_files)?;
    export_labels(c, "ambience", "Ambience", &amb_labels, &mut layer_files)?;

    log::info("--- Building music layer ---");
    for plan in mix::by_seq(&plans) {
        if plan.dt() != Some("MUSIC") || plan.filepath.is_empty() {
            continue;
        }
        log::info(&format!(
            "    seq {}: vol={}  trim={}  ramp_out={}  {}",
            plan.seq,
            fmt_vol(plan.volume_percentage),
            fmt_pct_1(plan.play_duration),
            fmt_secs(plan.ramp_out_seconds),
            xil_core::fsutil::basename(Path::new(&plan.filepath))
        ));
    }
    let (mus, mus_labels) = mix::build_music_layer(&plans, &cue_timeline, total_ms, 0.0, true)?;
    export_layer(c, mus, "music", "Music", &mut layer_files)?;
    export_labels(c, "music", "Music", &mus_labels, &mut layer_files)?;

    log::info("--- Building SFX layer ---");
    let (sfx, sfx_labels) = mix::build_sfx_layer(&plans, &cue_timeline, total_ms)?;
    export_layer(c, sfx, "sfx", "SFX", &mut layer_files)?;
    export_labels(c, "sfx", "SFX", &sfx_labels, &mut layer_files)?;

    log::info("--- Building vintage filter layer ---");
    let (vf, vf_labels) = mix::build_vintage_filter_layer(&plans, &cue_timeline, total_ms, 0.0)?;
    export_layer(c, vf, "vintage_filter", "Vintage Filter", &mut layer_files)?;
    export_labels(
        c,
        "vintage_filter",
        "Vintage Filter",
        &vf_labels,
        &mut layer_files,
    )?;

    let script_fname = format!("{}_open_in_audacity.py", c.tag);
    let script_path = Path::new(&c.output_dir).join(&script_fname);
    fs::write(
        &script_path,
        make_audacity_script(&c.tag, &layer_files, c.a.save_aup3, &c.cast.show),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;
    }
    log::info(&format!("    Written: {}/{script_fname}", c.output_dir));

    if c.a.macro_ {
        let path = generate_audacity_macro(
            &c.output_dir,
            &c.tag,
            &layer_files,
            &c.cast.show,
            c.cast.season_title.as_deref(),
            c.cast.title.as_deref(),
            &c.cast.artist,
        )?;
        match path {
            Some(p) => {
                log::info(&format!("    Written: {}", p.display()));
                if has_audacity4() {
                    log::warning(
                        "Audacity 4 has no Macro Manager — this macro runs in Audacity 3 only. In Audacity 4, import the layer WAVs manually (see the helper script).",
                    );
                }
            }
            None => log::warning("Audacity Macros directory not found — macro not written."),
        }
    }

    write_timeline_outputs(
        c,
        &plans,
        &index,
        total_ms,
        &cue_timeline,
        [amb_labels, mus_labels, sfx_labels, vf_labels],
    )?;

    log::info("");
    log::info(&format!(
        "--- Done! {} layer WAVs in {}/ ---",
        layer_files.len(),
        c.output_dir
    ));
    log::info(&format!(
        "    Import into Audacity: python {}/{script_fname}",
        c.output_dir
    ));
    if c.a.macro_ {
        log::info(&format!(
            "    Audacity macro:       Tools → Macros → {}_{} → Apply to Project  (Audacity 3 only)",
            macro_slug(&c.cast.show),
            c.tag
        ));
    }
    if c.a.save_aup3 {
        log::info(&format!(
            "    Will save project:    {}/{}.aup3",
            c.output_dir, c.tag
        ));
    }
    Ok(())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("daw");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-daw", args) {
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
    let cast_path = &p["cast"];
    if !cast_path.exists() {
        log::error(&format!("Cast config not found: {}", cast_path.display()));
        log::info("Run XILP001 first or check your --episode flag.");
        return Ok(());
    }
    let cast = CastConfig::load(cast_path)?;
    let tag = cast.tag.clone();
    let stems_dir = mix::join(&workspace_root().join("stems").join(&slug), &tag);
    let parsed_path = a
        .parsed
        .clone()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| p["parsed"].clone());
    let output_dir = a
        .output_dir
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p["daw"].to_string_lossy().into_owned());

    if !parsed_path.exists() {
        log::warning(&format!(
            "Parsed JSON not found: {}. Run XILP001 first.",
            xil_audio::fx::py_repr(&parsed_path.to_string_lossy())
        ));
        return Ok(());
    }
    let sfx = if p["sfx"].exists() {
        Some(SfxConfig::load(&p["sfx"])?)
    } else {
        None
    };

    let index = mix::load_entries_index(&parsed_path)?;
    let plans = mix::collect_stem_plans(Path::new(&stems_dir), &index, sfx.as_ref());
    let derived = derive_vintage_scenes_from_parsed(&parsed_path);
    let config_scenes: Vec<String> = sfx
        .as_ref()
        .map(|s| s.vintage_scenes.clone())
        .unwrap_or_default();
    let resolved = if derived.is_empty() {
        config_scenes.clone()
    } else {
        derived.clone()
    };
    if !derived.is_empty() && derived != config_scenes && !config_scenes.is_empty() {
        log::info(&format!(
            "  vintage_scenes: derived from parsed JSON ({} scenes)",
            derived.len()
        ));
    }

    let c = Ctx {
        cast: &cast,
        stems_dir,
        parsed_path,
        output_dir,
        tag,
        slug,
        sfx: sfx.as_ref(),
        a,
    };

    if a.dry_run {
        dry_run_daw(
            &c.tag,
            &plans,
            &c.output_dir,
            &c.stems_dir,
            &cast.cast,
            &resolved,
        )?;
        let (total_ms, timeline) = mix::build_foreground_timeline_only(&plans, a.gap_ms)?;
        let amb = mix::compute_ambience_labels(&plans, &timeline, total_ms);
        let mus = mix::compute_music_labels(&plans, &timeline, total_ms, true)?;
        let sfx_l = mix::compute_sfx_labels(&plans, &timeline)?;
        let vf = mix::compute_vintage_filter_labels(&plans, &timeline, total_ms);
        return write_timeline_outputs(
            &c,
            &plans,
            &index,
            total_ms,
            &timeline,
            [amb, mus, sfx_l, vf],
        );
    }
    export_daw_layers(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_tags_are_refused() {
        assert!(validate_tag_for_script("S01E01").is_ok());
        assert!(validate_tag_for_script("V01-C_3").is_ok());
        assert!(validate_tag_for_script("S01E01\"; rm").is_err());
        assert!(validate_tag_for_script("").is_err());
    }

    #[test]
    fn layers_repr_is_python_repr() {
        let files = vec![(
            "Dialogue".to_string(),
            "S01E01_layer_dialogue.wav".to_string(),
        )];
        assert_eq!(
            layers_repr(&files),
            "[('Dialogue', 'S01E01_layer_dialogue.wav')]"
        );
    }

    #[test]
    fn script_template_is_filled() {
        let files = vec![("SFX".to_string(), "T_layer_sfx.wav".to_string())];
        let s = make_audacity_script("T", &files, true, "");
        assert!(!s.contains("@@XIL_"));
        assert!(s.contains("\"\"\"Open Episode T DAW layers in Audacity."));
        assert!(s.contains("T.aup3"));
    }
}

//! `xil sfx` — generate SFX / music / ambience stems from an episode's SFX
//! config, standalone from the producer. Port of `XILU002_generate_SFX.py`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::Parser;
use xil_core::workspace::{derive_paths, resolve_slug, show_slug, workspace_root};
use xil_core::{banner, log};

use crate::mix::config::{CastConfig, SfxConfig};
use crate::sfxgen;

#[derive(Parser)]
#[command(
    name = "xil-sfx",
    about = "Generate SFX stems from an SFX config (standalone utility)"
)]
#[command(group(clap::ArgGroup::new("tag_group").required(true).args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S01E01) — derives cast and SFX config paths
    #[arg(long)]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Path to parsed script JSON (default: derived from cast config)
    #[arg(long)]
    script: Option<String>,
    /// Preview existing vs. new stems and estimated credit cost
    #[arg(long)]
    dry_run: bool,
    /// Only process effects with duration_seconds <= this value
    #[arg(long, allow_negative_numbers = true)]
    max_duration: Option<f64>,
    /// Limit to SFX and BEAT entries only
    #[arg(long)]
    gen_sfx: bool,
    /// Limit to MUSIC entries only
    #[arg(long)]
    gen_music: bool,
    /// Limit to AMBIENCE entries only
    #[arg(long)]
    gen_ambience: bool,
    /// (deprecated) shorthand for --gen-sfx --gen-music --gen-ambience
    #[arg(long)]
    sfx_music: bool,
    /// Only place stems for effects already present in SFX/; skip API generation
    #[arg(long)]
    local_only: bool,
    /// Backend for SFX/music/ambience generation, independent of the dialogue --backend. 'elevenlabs' (default) calls the ElevenLabs Sound Effects API. 'mmaudio' runs MMAudio locally in venv-mmaudio — free and GPU-accelerated, writes backend-tagged assets to SFX/<slug>.mmaudio.mp3. MMAudio's weights are CC BY-NC 4.0 (non-commercial only) and require --mmaudio-accept-noncommercial.
    #[arg(long, default_value = "elevenlabs", value_parser = ["elevenlabs", "mmaudio"], value_name = "BACKEND")]
    sfx_backend: String,
    /// Path to the Python executable in the MMAudio venv (default: auto-detect ./venv-mmaudio/bin/python3). Used only with --sfx-backend mmaudio.
    #[arg(long, value_name = "PATH")]
    mmaudio_python: Option<String>,
    /// Generation length in seconds before trimming (default: 8.0, MMAudio's training duration). The result is trimmed to each cue's duration_seconds; generating at the native length and trimming gives better audio than asking for a short clip.
    #[arg(
        long,
        default_value_t = 8.0,
        value_name = "FLOAT",
        allow_negative_numbers = true
    )]
    mmaudio_duration: f64,
    /// MMAudio classifier-free guidance strength (default: 4.5).
    #[arg(
        long,
        default_value_t = 4.5,
        value_name = "FLOAT",
        allow_negative_numbers = true
    )]
    mmaudio_cfg: f64,
    /// MMAudio flow-matching sampling steps (default: 25).
    #[arg(
        long,
        default_value_t = 25,
        value_name = "INT",
        allow_negative_numbers = true
    )]
    mmaudio_steps: i64,
    /// Optional MMAudio negative prompt (default: none).
    #[arg(long, default_value = "", value_name = "STR")]
    mmaudio_negative_prompt: String,
    /// MMAudio reproducibility seed (default: nondeterministic).
    #[arg(long, value_name = "INT", allow_negative_numbers = true)]
    mmaudio_seed: Option<i64>,
    /// Acknowledge that MMAudio's weights are CC BY-NC 4.0 (NON-COMMERCIAL USE ONLY) and that generated audio must not appear in a monetised production. Required for --sfx-backend mmaudio.
    #[arg(long)]
    mmaudio_accept_noncommercial: bool,
}

fn exit_with(msg: &str) -> anyhow::Result<i32> {
    Err(super::SysExit(msg.to_string()).into())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("sfx");
    let result = {
        let _banner = banner::begin(super::prog(), &super::argv_line(args));
        execute(args)
    };
    super::finish(result)
}

fn execute(args: &[OsString]) -> anyhow::Result<i32> {
    let a: Args = match super::parse_or_exit("xil-sfx", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let key = std::env::var("ELEVENLABS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    if !a.dry_run && a.sfx_backend == "elevenlabs" && key.is_none() {
        return exit_with("Error: ELEVENLABS_API_KEY environment variable is not set.");
    }
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let p = derive_paths(&slug, &tag);
    let cast_path = p["cast"].clone();
    let sfx_path = p["sfx"].clone();

    let script: PathBuf = match &a.script {
        Some(s) => PathBuf::from(s),
        None => {
            if !cast_path.exists() {
                return exit_with(&format!(
                    "Error: Cast config not found: {}\nRun XILP001 first or check your --episode flag.",
                    cast_path.display()
                ));
            }
            CastConfig::load(&cast_path)?;
            p["parsed"].clone()
        }
    };

    let direction_types: Option<HashSet<&str>> =
        if a.gen_sfx || a.gen_music || a.gen_ambience || a.sfx_music {
            let mut t = HashSet::new();
            if a.gen_sfx || a.sfx_music {
                t.insert("SFX");
                t.insert("BEAT");
            }
            if a.gen_music || a.sfx_music {
                t.insert("MUSIC");
            }
            if a.gen_ambience || a.sfx_music {
                t.insert("AMBIENCE");
            }
            Some(t)
        } else {
            None
        };

    // load_sfx_plan: the cast config names the stems directory.
    if !cast_path.exists() {
        anyhow::bail!(
            "FileNotFoundError: Cast config not found: {}\nRun XILP001 first or check your --episode flag.",
            cast_path.display()
        );
    }
    let cast = CastConfig::load(&cast_path)?;
    let stems_dir = workspace_root()
        .join("stems")
        .join(show_slug(&cast.show))
        .join(&cast.tag);
    let sfx_dir = workspace_root().join("SFX");
    if !Path::new(&script).exists() {
        anyhow::bail!(
            "FileNotFoundError: [Errno 2] No such file or directory: '{}'",
            script.display()
        );
    }
    if !sfx_path.exists() {
        anyhow::bail!(
            "FileNotFoundError: [Errno 2] No such file or directory: '{}'",
            sfx_path.display()
        );
    }
    let cfg = SfxConfig::load(&sfx_path)?;
    let entries = sfxgen::load_sfx_entries(
        &script,
        &cfg,
        a.max_duration,
        direction_types.as_ref(),
        a.local_only,
        &sfx_dir,
    )?;

    if a.dry_run {
        sfxgen::dry_run_sfx(&entries, &cfg, &stems_dir, &sfx_dir, &a.sfx_backend);
        return Ok(0);
    }
    let mm = sfxgen::MMAudioOptions {
        python: a.mmaudio_python.as_deref(),
        cfg: a.mmaudio_cfg,
        steps: a.mmaudio_steps,
        negative_prompt: &a.mmaudio_negative_prompt,
        seed: a.mmaudio_seed,
        duration: a.mmaudio_duration,
        accept_noncommercial: a.mmaudio_accept_noncommercial,
    };
    let Some(mut backend) =
        sfxgen::make_sfx_backend(&a.sfx_backend, xil_api::elevenlabs::Client::new(key), &mm)?
    else {
        return Ok(1);
    };
    let result = sfxgen::generate_sfx(&entries, &cfg, &stems_dir, &sfx_dir, 1, backend.as_mut());
    backend.close()?;
    result?;
    Ok(0)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

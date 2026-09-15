//! `xil sample` — audition each cast voice with a short generated line.
//! Port of `XILU004_sample_voices_T2S.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::bail;
use clap::Parser;
use xil_core::pyfmt::{commas, pad_right};
use xil_core::workspace::{derive_paths, resolve_slug, workspace_root};
use xil_core::{banner, log};

use crate::mix::config::CastConfig;
use crate::tts;

#[derive(Parser)]
#[command(
    name = "xil-sample",
    about = "Generate a voice sample MP3 for each cast member via the chosen TTS backend."
)]
#[command(group(clap::ArgGroup::new("target").required(true).args(["episode", "cast"])))]
struct Args {
    /// Episode tag (e.g. S02E03); derives cast config path
    #[arg(long, value_name = "TAG")]
    episode: Option<String>,
    /// Explicit path to cast JSON file
    #[arg(long, value_name = "PATH")]
    cast: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    // Help text as an attribute: rustdoc would read its [tag] list as links.
    #[arg(long, help = "TTS backend for sample generation. 'elevenlabs' (default) calls the ElevenLabs API and uses the voice_id from the cast config. 'gtts' generates a flat-voice draft via Google Translate TTS at no cost (ignores voice_id). 'chatterbox' uses local GPU TTS with zero-shot voice cloning from voice_refs/<key>.wav clips ('chatterbox' is a deprecated alias that warns and uses Turbo). 'chatterbox-turbo' uses the Chatterbox Turbo model in the same venv-chatterbox — it renders 19 native paralinguistic tags ([angry] [fear] [surprised] [happy] [crying] [sarcastic] [whispering] [dramatic] [narration] [advertisement] [laugh] [chuckle] [sigh] [gasp] [groan] [cough] [sniff] [shush] [clear throat]; exact spelling, no plurals), strips all other tags and needs reference clips >5s. Put a tag in --sample-text to audition a cue. Output lands in voice_samples/<TAG>/<backend>/ for side-by-side comparison", default_value = "elevenlabs", value_parser = ["elevenlabs", "gtts", "chatterbox", "chatterbox-turbo"])]
    backend: String,
    /// Path to the chatterbox venv Python (default: auto-detect ./venv-chatterbox/bin/python3). Used with --backend chatterbox or chatterbox-turbo.
    #[arg(long, value_name = "PATH")]
    chatterbox_python: Option<String>,
    /// Directory containing <speaker_key>.wav reference clips for Chatterbox zero-shot voice cloning (default: voice_refs/). Used with --backend chatterbox or chatterbox-turbo.
    #[arg(long, default_value = "voice_refs", value_name = "DIR")]
    voice_refs: String,
    /// Device for --backend chatterbox-turbo (default: cuda). The worker auto-falls back to cpu when cuda is requested but unavailable, so this rarely needs setting explicitly — pass 'cpu' to force it even when cuda would work. Slower on cpu, but functional.
    #[arg(long, default_value = "cuda", value_parser = ["cuda", "cpu"], value_name = "DEVICE")]
    device: String,
    /// Override the sample text spoken by each voice. Use {name} as a placeholder for the speaker's full name. Default: "I am {name} not yo momma"
    #[arg(long, value_name = "TEXT")]
    sample_text: Option<String>,
    /// Print what would be generated without calling any TTS API
    #[arg(long)]
    dry_run: bool,
    /// Regenerate samples even if files already exist on disk
    #[arg(long)]
    force: bool,
}

/// `template.format(name=...)` — `{name}` and doubled braces only.
fn format_name(template: &str, name: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("{{") {
            out.push('{');
            rest = r;
        } else if let Some(r) = rest.strip_prefix("}}") {
            out.push('}');
            rest = r;
        } else if let Some(r) = rest.strip_prefix("{name}") {
            out.push_str(name);
            rest = r;
        } else if let Some(r) = rest.strip_prefix('{') {
            let field: String = r.chars().take_while(|&c| c != '}').collect();
            bail!("KeyError: '{field}'");
        } else if rest.starts_with('}') {
            bail!("ValueError: Single '}}' encountered in format string");
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    Ok(out)
}

/// `check_elevenlabs_quota()` in this module's wording.
fn check_quota(client: &xil_api::elevenlabs::Client) -> anyhow::Result<()> {
    match client.user_get() {
        Ok(u) => {
            let sub = &u["subscription"];
            let (used, limit) = (
                sub["character_count"].as_i64().unwrap_or(0),
                sub["character_limit"].as_i64().unwrap_or(0),
            );
            log::info(&format!("\n{}", "=".repeat(40)));
            log::info("ELEVENLABS API STATUS:");
            log::info(&format!(
                "  Tier:      {}",
                sub["tier"].as_str().unwrap_or("").to_uppercase()
            ));
            log::info(&format!(
                "  Usage:     {} / {} characters",
                commas(used),
                commas(limit)
            ));
            log::info(&format!("  Remaining: {}", commas(limit - used)));
            log::info(&format!("{}\n", "=".repeat(40)));
            Ok(())
        }
        Err(xil_api::ApiError::Status { status, body }) => {
            log::warning("API Error: Unable to fetch user subscription data.");
            log::warning(&format!("    Details: status_code: {status}, body: {body}"));
            Ok(())
        }
        Err(e) => bail!("httpx.TransportError: {e}"),
    }
}

fn remaining(client: &xil_api::elevenlabs::Client) -> Result<Option<i64>, anyhow::Error> {
    match client.user_get() {
        Ok(u) => {
            let sub = &u["subscription"];
            Ok(Some(
                sub["character_limit"].as_i64().unwrap_or(0)
                    - sub["character_count"].as_i64().unwrap_or(0),
            ))
        }
        Err(xil_api::ApiError::Status { .. }) => Ok(None),
        Err(e) => bail!("httpx.TransportError: {e}"),
    }
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("sample");
    let result = {
        let _banner = banner::begin(super::prog(), &super::argv_line(args));
        execute(args)
    };
    super::finish(result)
}

fn execute(args: &[OsString]) -> anyhow::Result<i32> {
    let a: Args = match super::parse_or_exit("xil-sample", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let mut backend = a.backend.clone();
    if backend == "chatterbox" {
        log::warning("--backend chatterbox was removed; using chatterbox-turbo instead.");
        backend = "chatterbox-turbo".into();
    }
    let key = std::env::var("ELEVENLABS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    if !a.dry_run && backend == "elevenlabs" && key.is_none() {
        return Err(super::SysExit(
            "Error: ELEVENLABS_API_KEY environment variable is not set.".into(),
        )
        .into());
    }
    let cast_path: PathBuf = match a.cast.as_ref().filter(|c| !c.is_empty()) {
        Some(c) => PathBuf::from(c),
        None => {
            let slug = resolve_slug(a.show.as_deref(), "project.json");
            derive_paths(&slug, a.episode.as_deref().unwrap_or(""))["cast"].clone()
        }
    };
    if !cast_path.exists() {
        log::warning(&format!("Cast config not found: {}", cast_path.display()));
        return Ok(1);
    }
    let cast = CastConfig::load(&cast_path)?;
    let out_dir = workspace_root()
        .join("voice_samples")
        .join(&cast.tag)
        .join(&backend);
    log::info(&format!("Cast config : {}", cast_path.display()));
    log::info(&format!("Episode tag : {}", cast.tag));
    log::info(&format!("Backend     : {backend}"));
    log::info(&format!("Output dir  : {}", out_dir.display()));
    log::info(&format!("Cast members: {}", cast.cast.len()));
    log::info("");

    let client = xil_api::elevenlabs::Client::new(key);
    if !a.dry_run {
        if backend == "elevenlabs" {
            check_quota(&client)?;
        }
        fs::create_dir_all(&out_dir)?;
    }
    let mut chatterbox = None;
    if backend == "chatterbox-turbo" && !a.dry_run {
        let python = a
            .chatterbox_python
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| {
                Path::new("venv-chatterbox")
                    .join("bin")
                    .join("python3")
                    .to_string_lossy()
                    .into_owned()
            });
        if !Path::new(&python).exists() {
            return Err(super::SysExit(format!(
                "Error: Chatterbox Python not found at {python}. Use --chatterbox-python to specify the path."
            ))
            .into());
        }
        chatterbox = Some(tts::Chatterbox::new(
            &python,
            &a.voice_refs,
            &a.device,
            tts::Flavor::Sample,
        ));
    }

    let (mut generated, mut skipped_tbd, mut skipped_exists) = (0, 0, 0);
    let result = (|| -> anyhow::Result<()> {
        for (k, member) in &cast.cast {
            if backend == "elevenlabs" && member.voice_id == "TBD" {
                log::info(&format!("  [ SKIP] {}  voice_id=TBD", pad_right(k, 12)));
                skipped_tbd += 1;
                continue;
            }
            let out_path = out_dir.join(format!("{k}.mp3"));
            let template = a
                .sample_text
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "I am {name} not yo momma".into());
            let text = format_name(&template, &member.full_name)?;
            if !a.force && out_path.exists() {
                log::info(&format!(
                    "  [EXISTS] {}  {}",
                    pad_right(k, 12),
                    out_path.display()
                ));
                skipped_exists += 1;
                continue;
            }
            if a.dry_run {
                let ref_note = if backend == "chatterbox-turbo" {
                    let r = Path::new(&a.voice_refs).join(format!("{k}.wav"));
                    format!(
                        "  ref={}",
                        if r.exists() {
                            "✓"
                        } else {
                            "✗ (default voice)"
                        }
                    )
                } else {
                    String::new()
                };
                log::info(&format!(
                    "  [DRY RUN] {}  ({})  →  {}{ref_note}",
                    pad_right(k, 12),
                    member.full_name,
                    out_path.display()
                ));
                generated += 1;
                continue;
            }
            if backend == "elevenlabs" {
                let required = text.chars().count() as i64;
                match remaining(&client)? {
                    Some(left) if left >= required => log::info(&format!(
                        " [Guard] Quota OK: {required} required, {} left.",
                        commas(left)
                    )),
                    Some(left) => {
                        log::info(&format!(
                            " [Guard] STOP: Line requires {required} chars, but only {} remain.",
                            commas(left)
                        ));
                        log::info(&format!(
                            "  [ STOP] {}  insufficient quota",
                            pad_right(k, 12)
                        ));
                        break;
                    }
                    None => log::info(
                        " [Guard] Warning: Permission 'user_read' missing. Skipping quota check.",
                    ),
                }
            }
            log::info(&format!(
                "  [   GEN] {}  {}  …",
                pad_right(k, 12),
                member.full_name
            ));
            let tts_comment = match backend.as_str() {
                "gtts" => {
                    tts::gtts_generate(&text, &out_path, tts::Flavor::Sample)?;
                    "gtts".to_string()
                }
                "chatterbox-turbo" => {
                    chatterbox
                        .as_mut()
                        .expect("started")
                        .generate(&text, &out_path, k)?;
                    backend.clone()
                }
                _ => {
                    let model = match remaining(&client)? {
                        Some(left) if left > 5000 => {
                            log::info(&format!(
                                " [Budget] Healthy Balance: {} left. Using 'eleven_v3'.",
                                commas(left)
                            ));
                            "eleven_v3"
                        }
                        Some(left) => {
                            log::info(&format!(
                                " [Budget] LOW BALANCE: {} left. Switching to 'eleven_flash_v2_5'.",
                                commas(left)
                            ));
                            "eleven_flash_v2_5"
                        }
                        None => {
                            log::info(" [Budget] API Check Failed. Defaulting to 'eleven_v3'.");
                            "eleven_v3"
                        }
                    };
                    // The SDK's stream opens only once iteration starts, by
                    // which point the output file already exists.
                    fs::File::create(&out_path)?;
                    let audio = client
                        .text_to_speech(&member.voice_id, &text, model, "mp3_44100_128", None)
                        .map_err(|e| anyhow::anyhow!("elevenlabs.core.api_error.ApiError: {e}"))?;
                    fs::write(&out_path, audio)?;
                    model.to_string()
                }
            };
            xil_audio::tags::tag_mp3_full(
                &out_path,
                "Sample Show",
                Some(&format!("Sample: {}", member.full_name)),
                Some(&member.full_name),
                Some(&text),
                Some(&tts_comment),
                None,
            )?;
            log::info(&format!("  saved → {}", out_path.display()));
            generated += 1;
        }
        Ok(())
    })();
    if let Some(c) = chatterbox.as_mut() {
        c.close();
    }
    result?;
    log::info("");
    if a.dry_run {
        log::info(&format!(
            "Dry run: {generated} would be generated, {skipped_tbd} TBD skipped."
        ));
    } else {
        log::info(&format!(
            "Done: {generated} generated, {skipped_exists} already existed, {skipped_tbd} TBD skipped."
        ));
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
    fn name_template() {
        assert_eq!(format_name("I am {name}", "Nora").unwrap(), "I am Nora");
        assert_eq!(
            format_name("{{literal}} {name}", "X").unwrap(),
            "{literal} X"
        );
        assert!(format_name("{other}", "X").is_err());
    }
}

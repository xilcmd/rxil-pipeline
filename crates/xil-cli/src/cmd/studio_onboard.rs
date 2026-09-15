//! `xil studio-onboard` — build an ElevenLabs Studio project from a parsed
//! episode. Port of `XILP004_studio_onboard.py`.

use std::ffi::OsString;
use std::fs;

use clap::Parser;
use serde_json::{json, Map, Value};
use xil_core::pyfmt::commas;
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::{derive_paths, resolve_slug};
use xil_core::{banner, log};

#[derive(Parser)]
#[command(
    name = "xil-studio-onboard",
    about = "Onboard an episode to an ElevenLabs Studio project."
)]
#[command(group(clap::ArgGroup::new("tag_group").required(true).args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S01E02)
    #[arg(long)]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Build and display content JSON without calling the API
    #[arg(long)]
    dry_run: bool,
    /// Quality preset (default: standard)
    #[arg(long, default_value = "standard", value_parser = ["standard", "high", "ultra", "ultra_lossless"])]
    quality: String,
    /// TTS model ID (default: eleven_v3)
    #[arg(long, default_value = "eleven_v3")]
    model: String,
}

fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        other => xil_core::pycsv::cell(other),
    }
}

/// The narrator: the first `Host/Narrator`, else the first cast member.
fn narrator_voice(cast: &Map<String, Value>) -> Value {
    cast.values()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("Host/Narrator"))
        .or_else(|| cast.values().next())
        .and_then(|m| m.get("voice_id").cloned())
        .unwrap_or(Value::Null)
}

/// `build_content_json(parsed, cast)`.
fn build_content_json(parsed: &Value, cast: &Map<String, Value>) -> Vec<Value> {
    let narrator = narrator_voice(cast);
    let mut chapters: Vec<Value> = Vec::new();
    let node = |text: &Value, voice: &Value, sub: &str| json!({"sub_type": sub, "nodes": [{"type": "tts_node", "text": text, "voice_id": voice}]});
    for entry in parsed["entries"].as_array().into_iter().flatten() {
        let text = entry.get("text").cloned().unwrap_or(Value::Null);
        match entry.get("type").and_then(Value::as_str) {
            Some("section_header") => chapters.push(json!({"name": text, "blocks": []})),
            Some(kind @ ("scene_header" | "dialogue")) => {
                if chapters.is_empty() {
                    chapters.push(json!({"name": "Untitled", "blocks": []}));
                }
                let block = if kind == "scene_header" {
                    node(&text, &narrator, "h2")
                } else {
                    let voice = entry
                        .get("speaker")
                        .and_then(Value::as_str)
                        .and_then(|s| cast.get(s))
                        .and_then(|m| m.get("voice_id").cloned())
                        .unwrap_or_else(|| narrator.clone());
                    node(&text, &voice, "p")
                };
                chapters.last_mut().unwrap()["blocks"]
                    .as_array_mut()
                    .unwrap()
                    .push(block);
            }
            _ => {}
        }
    }
    chapters
}

fn dry_run(chapters: &[Value], cast: &Map<String, Value>) {
    let mut voice_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (key, info) in cast {
        let name = info
            .get("full_name")
            .map(str_of)
            .unwrap_or_else(|| key.clone());
        voice_map.insert(str_of(&info["voice_id"]), name);
    }
    let bar = "=".repeat(60);
    log::info(&format!("\n{bar}"));
    log::info("STUDIO PROJECT — DRY RUN");
    log::info(&bar);
    let (mut total_blocks, mut total_chars) = (0usize, 0i64);
    for ch in chapters {
        log::info(&format!("\n  Chapter: {}", str_of(&ch["name"])));
        let blocks = ch["blocks"].as_array().cloned().unwrap_or_default();
        total_blocks += blocks.len();
        let nodes: Vec<&Value> = blocks
            .iter()
            .flat_map(|b| b["nodes"].as_array().into_iter().flatten())
            .collect();
        let chars: i64 = nodes
            .iter()
            .map(|n| n["text"].as_str().map_or(0, |t| t.chars().count()) as i64)
            .sum();
        total_chars += chars;
        let mut used: Vec<String> = Vec::new();
        for n in &nodes {
            if let Some(vid) = n.get("voice_id").filter(|v| crate::cmd::truthy(v)) {
                let label = voice_map
                    .get(&str_of(vid))
                    .cloned()
                    .unwrap_or_else(|| str_of(vid));
                if !used.contains(&label) {
                    used.push(label);
                }
            }
        }
        used.sort();
        log::info(&format!(
            "    Blocks: {}  |  Characters: {}",
            blocks.len(),
            commas(chars)
        ));
        log::info(&format!("    Voices: {}", used.join(", ")));
    }
    log::info(&format!(
        "\n  TOTAL: {} chapters, {total_blocks} blocks, {} characters",
        chapters.len(),
        commas(total_chars)
    ));
    log::info(&format!("{bar}\n"));
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("studio-onboard");
    let result = {
        let _banner = banner::begin(super::prog(), &super::argv_line(args));
        execute(args)
    };
    super::finish(result)
}

fn execute(args: &[OsString]) -> anyhow::Result<i32> {
    let a: Args = match super::parse_or_exit("xil-studio-onboard", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let key = std::env::var("ELEVENLABS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    if !a.dry_run && key.is_none() {
        return Err(super::SysExit(
            "Error: ELEVENLABS_API_KEY environment variable is not set.".into(),
        )
        .into());
    }
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let p = derive_paths(&slug, &tag);
    if !p["parsed"].exists() {
        log::error(&format!("Parsed file not found: {}", p["parsed"].display()));
        return Ok(1);
    }
    if !p["cast"].exists() {
        log::error(&format!("Cast file not found: {}", p["cast"].display()));
        return Ok(1);
    }
    let parsed: Value = serde_json::from_str(&fs::read_to_string(&p["parsed"])?)?;
    let cast_doc: Value = serde_json::from_str(&fs::read_to_string(&p["cast"])?)?;
    let cast = cast_doc["cast"].as_object().cloned().unwrap_or_default();
    let tbd: Vec<&str> = cast
        .iter()
        .filter(|(_, m)| m.get("voice_id").map_or(true, |v| v == "TBD"))
        .map(|(k, _)| k.as_str())
        .collect();
    if !tbd.is_empty() {
        log::error(&format!("TBD voice_id for: {}", tbd.join(", ")));
        log::error("        Assign voice IDs in cast config before onboarding.");
        return Ok(1);
    }
    let chapters = build_content_json(&parsed, &cast);
    if a.dry_run {
        dry_run(&chapters, &cast);
        return Ok(0);
    }

    let narrator = narrator_voice(&cast);
    let show = parsed
        .get("show")
        .map(str_of)
        .unwrap_or_else(|| "Unknown Show".into());
    let title = parsed
        .get("title")
        .map(str_of)
        .unwrap_or_else(|| tag.clone());
    let project_name = format!("XILP004 - {show} — {title} ({tag})");
    log::info(&format!("Creating Studio project: {project_name}"));

    let client = xil_api::elevenlabs::Client::new(key);
    match client.user_get() {
        Ok(user) => {
            let sub = &user["subscription"];
            let used = sub["character_count"].as_i64().unwrap_or(0);
            let limit = sub["character_limit"].as_i64().unwrap_or(0);
            let bar = "=".repeat(40);
            log::info(&format!("\n{bar}"));
            log::info("ELEVENLABS API STATUS:");
            log::info(&format!(
                "  Tier:      {}",
                str_of(&sub["tier"]).to_uppercase()
            ));
            log::info(&format!(
                "  Usage:     {} / {} characters",
                commas(used),
                commas(limit)
            ));
            log::info(&format!("  Remaining: {}", commas(limit - used)));
            log::info(&format!("{bar}\n"));
        }
        Err(xil_api::ApiError::Status { status, body }) => {
            log::warning("API Error: Unable to fetch subscription data.");
            log::warning(&format!("    Details: status_code: {status}, body: {body}"));
        }
        Err(e) => return Err(anyhow::anyhow!("httpx.TransportError: {e}")),
    }

    let fields = [
        ("name", project_name),
        ("default_title_voice_id", str_of(&narrator)),
        ("default_paragraph_voice_id", str_of(&narrator)),
        ("default_model_id", a.model.clone()),
        (
            "from_content_json",
            dumps(&Value::Array(chapters), Style::COMPACT),
        ),
        ("quality_preset", a.quality.clone()),
    ];
    let response = client
        .studio_projects_create(&fields, None)
        .map_err(|e| anyhow::anyhow!("elevenlabs.core.api_error.ApiError: {e}"))?;
    log::info("\nProject created successfully!");
    log::info(&format!(
        "  Project ID: {}",
        str_of(&response["project"]["project_id"])
    ));
    Ok(0)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

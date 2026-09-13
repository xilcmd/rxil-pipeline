//! `xil cues` — parse a sound cues & music prompts sheet into an asset
//! manifest, audit the SFX library, and optionally generate new assets or
//! enrich the episode SFX config. Port of `XILP006_cues_ingester.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::cmd::py_str;
use clap::Parser;
use regex::Regex;
use serde_json::{Map, Value};
use xil_audio::fx::py_repr;
use xil_core::pyjson::{dumps, py_float, Style};
use xil_core::workspace::{derive_paths, resolve_slug, workspace_root};
use xil_core::{banner, log};

const DEFAULT_SFX_DURATION: f64 = 5.0;
const API_MAX_DURATION: f64 = 30.0;
const CREDITS_PER_SECOND: f64 = 40.0;

#[derive(Parser)]
#[command(
    name = "xil-cues",
    about = "Parse a sound cues & music prompts markdown file into an asset manifest, audit the SFX library, and optionally generate new assets or enrich the episode sfx config."
)]
#[command(group(clap::ArgGroup::new("tag_group").required(true).args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S02E03) — derives sfx config path
    #[arg(long)]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Path to cues markdown file (auto-detected from cues/ if omitted)
    #[arg(long)]
    cues: Option<String>,
    /// Show audit report and enrichment diff without API calls or sfx config writes (manifest is always written)
    #[arg(long)]
    dry_run: bool,
    /// Generate NEW assets via ElevenLabs API into SFX/
    #[arg(long)]
    generate: bool,
    /// Update episode SFX config with cues-sheet prompts/durations
    #[arg(long)]
    enrich_sfx_config: bool,
}

#[derive(Clone, Debug)]
struct Asset {
    asset_id: String,
    category: &'static str,
    reuse: bool,
    prompt: Option<String>,
    duration_seconds: Option<f64>,
    loop_: bool,
    scene: Option<String>,
}

impl Asset {
    fn json(&self) -> Value {
        let mut m = Map::new();
        m.insert("asset_id".into(), self.asset_id.clone().into());
        m.insert("category".into(), self.category.into());
        m.insert("reuse".into(), self.reuse.into());
        m.insert(
            "prompt".into(),
            self.prompt.clone().map(Value::from).unwrap_or(Value::Null),
        );
        m.insert(
            "duration_seconds".into(),
            self.duration_seconds.map(py_float).unwrap_or(Value::Null),
        );
        m.insert("loop".into(), self.loop_.into());
        m.insert(
            "scene".into(),
            self.scene.clone().map(Value::from).unwrap_or(Value::Null),
        );
        Value::Object(m)
    }
}

static LOOP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bloop").unwrap());
static DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(minutes?|min|seconds?|sec|s)\b").unwrap());
static STARS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*+").unwrap());
static BLOCK_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^([\w][\w-]*(?:-\d+)?)\s*\(?(NEW|REUSE)\)?").unwrap());
static PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*{1,2}Prompt:\*{1,2}\s*(.*?)\s*\*{1,2}Duration:").unwrap());
static DURATION_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*{1,2}Duration:\*{1,2}\s*(.*?)\s*\*{1,2}Used:").unwrap());
static SCENE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^###\s+(?:Scene\s+\d+:\s*)?(.*)").unwrap());
static ASSET_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^Asset\s+Name").unwrap());
static CAPS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Z]{2,}").unwrap());
static SFX_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\*{0,2}([\w][\w-]*-\d+)\s*\((NEW|REUSE)\)\*{0,2}").unwrap());

/// `parse_duration(text)`.
fn parse_duration(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() || LOOP.is_match(t) {
        return None;
    }
    let m = DURATION.captures(t)?;
    let value: f64 = m[1].parse().ok()?;
    Some(if m[2].to_lowercase().starts_with('m') {
        value * 60.0
    } else {
        value
    })
}

fn chars_from(s: &str, n: usize) -> String {
    s.chars().skip(n).collect()
}

/// `parse_cues_markdown(path)`.
fn parse_cues_markdown(path: &Path) -> anyhow::Result<Vec<Asset>> {
    let text = fs::read_to_string(path)?;
    let mut assets = Vec::new();
    let mut section: Option<&'static str> = None;
    let mut scene: Option<String> = None;
    let mut pending: Option<Asset> = None;
    for raw in text.split_inclusive('\n') {
        let s = raw.trim();
        if s.starts_with("## ") {
            let heading = STARS
                .replace_all(&chars_from(s, 3), "")
                .trim()
                .to_uppercase();
            section = if heading.contains("MUSIC") && heading.contains("CUE") {
                Some("MUSIC")
            } else if heading == "AMBIENCE" {
                Some("AMBIENCE")
            } else if heading.contains("SOUND EFFECT") {
                Some("SFX")
            } else {
                None
            };
            pending = None;
            scene = None;
            continue;
        }
        let Some(sec) = section else { continue };
        if (sec == "MUSIC" || sec == "AMBIENCE") && s.starts_with("### ") {
            let heading = STARS.replace_all(&chars_from(s, 4), "").trim().to_string();
            if let Some(m) = BLOCK_HEADING.captures(&heading) {
                pending = Some(Asset {
                    asset_id: m[1].to_uppercase(),
                    category: sec,
                    reuse: m[2].to_uppercase() == "REUSE",
                    prompt: None,
                    duration_seconds: None,
                    loop_: sec == "AMBIENCE",
                    scene: None,
                });
            }
            continue;
        }
        if (sec == "MUSIC" || sec == "AMBIENCE") && pending.is_some() && s.contains("**Prompt:**") {
            let mut a = pending.take().unwrap();
            if let Some(pm) = PROMPT.captures(s) {
                a.prompt = Some(pm[1].trim().to_string());
            }
            if let Some(dm) = DURATION_FIELD.captures(s) {
                let dur_raw = dm[1].trim();
                a.duration_seconds = parse_duration(dur_raw);
                if LOOP.is_match(dur_raw) {
                    a.loop_ = true;
                }
            }
            assets.push(a);
            continue;
        }
        if sec == "SFX" {
            if s.starts_with("###") {
                scene = Some(match SCENE.captures(s) {
                    Some(m) => STARS.replace_all(&m[1], "").trim().to_string(),
                    None => chars_from(s, 4).trim().to_string(),
                });
                continue;
            }
            if s.starts_with('|') {
                let cols: Vec<&str> = s
                    .split('|')
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .collect();
                if cols.len() < 2 || ASSET_NAME.is_match(cols[0]) || !CAPS.is_match(cols[0]) {
                    continue;
                }
                if let Some(m) = SFX_ROW.captures(cols[0]) {
                    assets.push(Asset {
                        asset_id: m[1].to_uppercase(),
                        category: "SFX",
                        reuse: m[2].to_uppercase() == "REUSE",
                        prompt: Some(cols[1].to_string()),
                        duration_seconds: None,
                        loop_: false,
                        scene: scene.clone(),
                    });
                }
            }
        }
    }
    Ok(assets)
}

fn library_path(asset_id: &str, sfx_dir: &Path) -> PathBuf {
    sfx_dir.join(format!("{}.mp3", asset_id.to_lowercase()))
}

fn status(a: &Asset, sfx_dir: &Path) -> &'static str {
    if crate::sfxgen::file_nonempty(&library_path(&a.asset_id, sfx_dir)) {
        "EXISTS"
    } else if a.reuse {
        " REUSE"
    } else {
        "   NEW"
    }
}

fn generation_duration(a: &Asset) -> f64 {
    match a.duration_seconds {
        Some(d) if d > 0.0 => d.min(API_MAX_DURATION),
        _ => DEFAULT_SFX_DURATION,
    }
}

fn credits(duration: f64) -> i64 {
    (duration * CREDITS_PER_SECOND).ceil() as i64
}

fn prompt_or_crash(a: &Asset) -> anyhow::Result<&str> {
    a.prompt
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("TypeError: 'NoneType' object is not subscriptable"))
}

fn dry_run_report(assets: &[Asset], sfx_dir: &Path) -> anyhow::Result<()> {
    let new_assets: Vec<&Asset> = assets
        .iter()
        .filter(|a| !a.reuse && status(a, sfx_dir).trim() == "NEW")
        .collect();
    let total_new_dur: f64 = new_assets.iter().map(|a| generation_duration(a)).sum();
    let total_credits: i64 = new_assets
        .iter()
        .map(|a| credits(generation_duration(a)))
        .sum();
    let capped = new_assets
        .iter()
        .filter(|a| a.duration_seconds.unwrap_or(0.0) > API_MAX_DURATION)
        .count();
    let exists = assets
        .iter()
        .filter(|a| status(a, sfx_dir) == "EXISTS")
        .count();
    let reuse_missing = assets
        .iter()
        .filter(|a| a.reuse && status(a, sfx_dir).trim() == "REUSE")
        .count();
    let bar = "=".repeat(72);
    log::info(&format!("\n{bar}"));
    log::info(&format!("CUES SHEET AUDIT — {} assets total", assets.len()));
    log::info(&format!(
        "  {exists} in library  |  {} new to generate  |  {reuse_missing} REUSE not yet in library",
        new_assets.len()
    ));
    log::info(&format!("{bar}\n"));
    for cat in ["MUSIC", "AMBIENCE", "SFX"] {
        let cat_assets: Vec<&Asset> = assets.iter().filter(|a| a.category == cat).collect();
        if cat_assets.is_empty() {
            continue;
        }
        log::info(&format!("  ── {cat} ──"));
        for a in cat_assets {
            let st = status(a, sfx_dir);
            let api_dur = generation_duration(a);
            let dur = a.duration_seconds.filter(|d| *d != 0.0);
            let dur_str = match dur {
                Some(d) => format!("{d:.0}s"),
                None => format!("~{api_dur:.0}s"),
            };
            let cap_note = match dur {
                Some(d) if d > API_MAX_DURATION => format!(" [CAPPED→{API_MAX_DURATION:.0}s]"),
                _ => String::new(),
            };
            let credits_note = if st.trim() == "NEW" {
                format!("  ~{} cr", credits(api_dur))
            } else {
                String::new()
            };
            let loop_note = if a.loop_ { " [loop]" } else { "" };
            log::info(&format!(
                "    [{st}] {:<32} {dur_str:>8}{cap_note}{credits_note}{loop_note}",
                a.asset_id
            ));
            if st.trim() == "NEW" {
                let p = prompt_or_crash(a)?;
                let mut t: String = p.chars().take(72).collect();
                if p.chars().count() > 72 {
                    t.push('…');
                }
                log::info(&format!("            prompt: {t}"));
            }
        }
        log::info("");
    }
    if capped > 0 {
        log::info(&format!(
            "  NOTE: {capped} asset(s) exceed the {API_MAX_DURATION:.0}s API cap and will be generated at 30s."
        ));
        log::info("");
    }
    log::info(&bar);
    log::info(&format!(
        "  New generation: {total_new_dur:.1}s total, ~{total_credits} credits"
    ));
    log::info(&format!("{bar}\n"));
    Ok(())
}

fn generate_new_assets(
    assets: &[Asset],
    sfx_dir: &Path,
    client: &xil_api::elevenlabs::Client,
) -> anyhow::Result<()> {
    fs::create_dir_all(sfx_dir)?;
    let todo: Vec<&Asset> = assets
        .iter()
        .filter(|a| !a.reuse && status(a, sfx_dir).trim() == "NEW")
        .collect();
    if todo.is_empty() {
        log::info("All NEW assets already exist in library — nothing to generate.");
        return Ok(());
    }
    log::info(&format!(
        "Generating {} new asset(s) into {}/…",
        todo.len(),
        sfx_dir.display()
    ));
    for a in &todo {
        let path = library_path(&a.asset_id, sfx_dir);
        let dur = generation_duration(a);
        if let Some(orig) = a
            .duration_seconds
            .filter(|d| *d != 0.0 && *d > API_MAX_DURATION)
        {
            log::warning(&format!(
                "{}: {orig:.0}s capped to {API_MAX_DURATION:.0}s for API",
                a.asset_id
            ));
        }
        log::info(&format!("  Generating {} ({dur:.1}s)…", a.asset_id));
        let bytes = client
            .sound_generation(a.prompt.as_deref().unwrap_or(""), Some(dur), Some(0.3))
            .map_err(|e| anyhow::anyhow!("elevenlabs.core.api_error.ApiError: {e}"))?;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new()
            .prefix("tmp")
            .rand_bytes(8)
            .suffix(".tmp")
            .tempfile_in(dir)?;
        fs::write(tmp.path(), &bytes)?;
        tmp.persist(&path).map_err(|e| anyhow::anyhow!(e.error))?;
        log::info(&format!("    → {}", path.display()));
    }
    log::info(&format!("Done. {} asset(s) generated.", todo.len()));
    Ok(())
}

fn enrich_sfx_config(assets: &[Asset], path: &Path, dry_run: bool) -> anyhow::Result<()> {
    let mut config: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let mut updates = 0usize;
    {
        let mut empty = Map::new();
        let effects = match config.get_mut("effects").and_then(Value::as_object_mut) {
            Some(e) => e,
            None => &mut empty,
        };
        for a in assets {
            let id = a.asset_id.to_uppercase();
            let keys: Vec<String> = effects
                .keys()
                .filter(|k| k.to_uppercase().contains(&id))
                .cloned()
                .collect();
            if keys.is_empty() {
                continue;
            }
            let new_prompt = a.prompt.clone().unwrap_or_default();
            let new_duration = generation_duration(a);
            for key in keys {
                let entry = effects
                    .get_mut(&key)
                    .and_then(Value::as_object_mut)
                    .expect("effect entries are objects");
                let old_prompt = entry
                    .get("prompt")
                    .cloned()
                    .unwrap_or_else(|| Value::from(""));
                let old_duration = entry
                    .get("duration_seconds")
                    .cloned()
                    .unwrap_or_else(|| py_float(0.0));
                let prompt_changed =
                    !new_prompt.is_empty() && old_prompt.as_str() != Some(new_prompt.as_str());
                let dur_changed =
                    (new_duration - old_duration.as_f64().unwrap_or(0.0)).abs() >= 0.5;
                if !prompt_changed && !dur_changed {
                    continue;
                }
                updates += 1;
                if dry_run {
                    log::info(&format!("  WOULD UPDATE: {key}"));
                    if prompt_changed {
                        let old = match &old_prompt {
                            Value::String(s) => py_repr(s),
                            other => py_str(other),
                        };
                        log::info(&format!("    prompt: {old}"));
                        log::info(&format!("         → {}", py_repr(&new_prompt)));
                    }
                    if dur_changed {
                        log::info(&format!(
                            "    duration: {}s → {}s",
                            py_str(&old_duration),
                            xil_core::pyjson::float_repr(new_duration)
                        ));
                    }
                } else {
                    if prompt_changed {
                        entry.insert("prompt".into(), new_prompt.clone().into());
                    }
                    entry.insert("duration_seconds".into(), py_float(new_duration));
                    if a.loop_ {
                        entry.insert("loop".into(), true.into());
                    }
                }
            }
        }
    }
    let noun = if updates == 1 { "y" } else { "ies" };
    if !dry_run && updates > 0 {
        fs::write(path, dumps(&config, Style::INDENT2))?;
        log::info(&format!(
            "Updated {updates} entr{noun} in {}",
            path.display()
        ));
    } else if updates == 0 {
        log::info("No sfx config entries matched cues sheet assets — nothing to update.");
    } else {
        log::info(&format!(
            "\n{updates} entr{noun} would be updated (pass --enrich-sfx-config without --dry-run to apply)."
        ));
    }
    Ok(())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("cues");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-cues", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let p = derive_paths(&slug, &tag);
    let root = workspace_root();
    let cues_dir = root.join("cues");
    let sfx_dir = root.join("SFX");

    let cues_path: Option<PathBuf> = match a.cues.as_ref().filter(|c| !c.is_empty()) {
        Some(c) => Some(PathBuf::from(c)),
        None if cues_dir.is_dir() => {
            if p["cues"].exists() {
                Some(p["cues"].clone())
            } else {
                let md: Vec<PathBuf> = xil_core::fsutil::list_dir_raw(&cues_dir)
                    .into_iter()
                    .filter(|x| {
                        let n = xil_core::fsutil::basename(x);
                        n.ends_with(".md") && !n.starts_with('.')
                    })
                    .collect();
                if md.len() == 1 {
                    md.into_iter().next()
                } else {
                    None
                }
            }
        }
        None => None,
    };
    let Some(cues_path) = cues_path else {
        eprintln!("usage: xil-cues [-h] (--episode EPISODE | --tag TAG) [--show SHOW] [--cues CUES] [--dry-run] [--generate] [--enrich-sfx-config]");
        eprintln!(
            "xil-cues: error: No cues file found for {tag}. Pass --cues PATH or name your file {}",
            p["cues"].display()
        );
        return Ok(2);
    };

    log::info(&format!("Parsing: {}", cues_path.display()));
    let assets = parse_cues_markdown(&cues_path)?;
    let new_count = assets.iter().filter(|x| !x.reuse).count();
    log::info(&format!(
        "Found {} assets ({new_count} new, {} reuse)",
        assets.len(),
        assets.len() - new_count
    ));

    let mut manifest = Map::new();
    manifest.insert("episode".into(), tag.clone().into());
    manifest.insert(
        "source".into(),
        xil_core::fsutil::basename(&cues_path).into(),
    );
    manifest.insert("total_assets".into(), assets.len().into());
    manifest.insert("new_count".into(), new_count.into());
    manifest.insert("reuse_count".into(), (assets.len() - new_count).into());
    manifest.insert(
        "assets".into(),
        Value::Array(assets.iter().map(Asset::json).collect()),
    );
    fs::create_dir_all(&cues_dir)?;
    let out = cues_dir.join(format!("cues_manifest_{tag}.json"));
    fs::write(&out, dumps(&Value::Object(manifest), Style::INDENT2))?;
    log::info(&format!("Manifest written: {}", out.display()));
    dry_run_report(&assets, &sfx_dir)?;

    if a.generate {
        if a.dry_run {
            log::info("--dry-run active: skipping API generation.");
        } else {
            match std::env::var("ELEVENLABS_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
            {
                None => log::error("ELEVENLABS_API_KEY not set. Cannot generate assets."),
                Some(key) => {
                    let client = xil_api::elevenlabs::Client::new(Some(key));
                    generate_new_assets(&assets, &sfx_dir, &client)?;
                }
            }
        }
    }
    if a.enrich_sfx_config {
        let sfx = &p["sfx"];
        if !sfx.exists() {
            log::warning(&format!(
                "{} not found — skipping sfx config enrichment.",
                sfx.display()
            ));
        } else {
            log::info(&format!("\nEnriching {}…", sfx.display()));
            enrich_sfx_config(&assets, sfx, a.dry_run)?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_like_python() {
        assert_eq!(parse_duration("60 seconds"), Some(60.0));
        assert_eq!(parse_duration("2 minutes"), Some(120.0));
        assert_eq!(parse_duration("1.5 min"), Some(90.0));
        assert_eq!(parse_duration("Loop"), None);
        assert_eq!(parse_duration("loopable, 30s"), None);
        assert_eq!(parse_duration("about 45s"), Some(45.0));
        assert_eq!(parse_duration("forever"), None);
    }
}

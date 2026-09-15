//! `xil publish` — draft social media posts for an episode through the
//! Claude API. Port of `XILP012_publish.py`.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use clap::Parser;
use serde_json::{json, Value};
use xil_core::workspace::{derive_paths, resolve_slug, workspace_root};
use xil_core::{banner, log};

const SYSTEM_PROMPT: &str = "You are a social media copywriter for the Berkshire Talking Chronicle, a radio reading service for people with visual impairments and print disabilities broadcasting from Pittsfield, Massachusetts.\n\nYou write warm, community-focused Facebook posts for The 413, an original radio drama series about WRRS Radio and the people of Berkshire County. The tone is: welcoming, local, proud of the community, enthusiastic about storytelling. Avoid corporate-speak. This is a volunteer-run community radio station.\n\nYou will produce exactly three post variants — Hype, Quote, and Spotlight — with those exact markdown headings. Each post should be complete, ready to copy-paste into Facebook with minimal editing. Include relevant emoji sparingly. Keep posts under 280 words each.";

#[derive(Parser)]
#[command(
    name = "xil-publish",
    about = "Generate social media post drafts from parsed episode data. Reads a parsed episode JSON and calls the Claude API to draft three ready-to-edit post variants (Hype, Quote, Spotlight) for Facebook or Instagram, written to posts/<slug>/<tag>_posts.md.",
    after_help = "Requires the ANTHROPIC_API_KEY environment variable (except with --dry-run) and the [publish] extra: pip install 'xil-pipeline[publish]'."
)]
#[command(group(clap::ArgGroup::new("tag_group").args(["episode", "tag"])))]
struct Args {
    /// Episode tag (e.g. S04E01)
    #[arg(long)]
    episode: Option<String>,
    /// Raw content tag (e.g. V01C03)
    #[arg(long)]
    tag: Option<String>,
    /// Generate posts for every parsed episode under the current show slug
    #[arg(long)]
    all: bool,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Target platform — affects post length/style guidance (default: facebook)
    #[arg(long, default_value = "facebook", value_parser = ["facebook", "instagram"])]
    platform: String,
    /// Print prompt and token estimate without making an API call or writing files
    #[arg(long)]
    dry_run: bool,
    /// Claude model ID (default: claude-haiku-4-5-20251001)
    #[arg(long, default_value = "claude-haiku-4-5-20251001")]
    model: String,
}

fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        other => xil_core::pycsv::cell(other),
    }
}

/// `entry.get(k, default)` — a present `null` stays `None`.
fn get_or(o: &Value, k: &str, default: Value) -> Value {
    o.get(k).cloned().unwrap_or(default)
}

fn section_label(slug: &str) -> String {
    match slug {
        "cold-open" => "Cold Open".into(),
        "opening-credits" => "Opening Credits".into(),
        "act1" => "Act One".into(),
        "act2" => "Act Two".into(),
        "act3" => "Act Three".into(),
        "mid-break" => "Mid-Episode Break".into(),
        "post-interview" => "Post-Interview".into(),
        "closing" => "Closing".into(),
        "prologue" => "Prologue".into(),
        "epilogue" => "Epilogue".into(),
        other => super::parse::title_case(&other.replace('-', " ")),
    }
}

struct Summary {
    show: Value,
    episode: Value,
    tag: String,
    title: Value,
    season_title: Value,
    cold_open_scene: Value,
    cold_open_lines: Vec<(Value, Value)>,
    cast: Vec<(String, Value, String)>,
    section_arc: String,
    runtime_minutes: Option<i64>,
}

fn extract_summary(
    parsed: &Value,
    cast_cfg: Option<&Value>,
    master: &Path,
) -> anyhow::Result<Summary> {
    let entries = parsed
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut cold_open_scene = Value::from("");
    let mut lines = Vec::new();
    for e in &entries {
        if e.get("section").and_then(Value::as_str) != Some("cold-open") {
            continue;
        }
        let kind = e.get("type").and_then(Value::as_str);
        if kind == Some("scene_header") && !super::truthy(&cold_open_scene) {
            cold_open_scene = get_or(e, "text", Value::from(""));
        } else if kind == Some("dialogue") && lines.len() < 3 {
            lines.push((
                get_or(e, "speaker", Value::from("")),
                get_or(e, "text", Value::from("")),
            ));
        }
    }
    let cast_dict = cast_cfg
        .and_then(|c| c.get("cast"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut cast = Vec::new();
    let speakers = parsed
        .get("stats")
        .and_then(|s| s.get("speakers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for sk in speakers {
        let Some(key) = sk.as_str() else { continue };
        if let Some(cfg) = cast_dict.get(key) {
            let full = get_or(cfg, "full_name", Value::from(key));
            let role = match cfg.get("role") {
                None => String::new(),
                Some(Value::String(r)) => {
                    r.trim().split('\n').next().unwrap_or("").trim().to_string()
                }
                Some(other) => anyhow::bail!(
                    "AttributeError: '{}' object has no attribute 'strip'",
                    py_type(other)
                ),
            };
            cast.push((key.to_string(), full, role));
        }
    }
    let mut seen: Vec<String> = Vec::new();
    let mut labels = Vec::new();
    for e in &entries {
        if let Some(sec) = e
            .get("section")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if !seen.iter().any(|x| x == sec) && sec != "preamble" && sec != "postamble" {
                seen.push(sec.to_string());
                labels.push(section_label(sec));
            }
        }
    }
    let section_arc = if labels.is_empty() {
        "unknown".to_string()
    } else {
        labels.join(" → ")
    };
    let runtime_minutes = if master.exists() {
        xil_audio::mpeg::info(master)
            .ok()
            .map(|i| (i.length / 60.0).floor() as i64)
    } else {
        None
    };
    let season = parsed.get("season").and_then(Value::as_i64);
    let episode = parsed.get("episode").and_then(Value::as_i64);
    let (Some(season), Some(ep)) = (season, episode) else {
        anyhow::bail!("TypeError: unsupported format string passed to NoneType.__format__");
    };
    Ok(Summary {
        show: get_or(parsed, "show", Value::from("")),
        episode: get_or(parsed, "episode", Value::Null),
        tag: format!("S{season:02}E{ep:02}"),
        title: get_or(parsed, "title", Value::from("")),
        season_title: get_or(parsed, "season_title", Value::Null),
        cold_open_scene,
        cold_open_lines: lines,
        cast,
        section_arc,
        runtime_minutes,
    })
}

fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn build_user_message(s: &Summary, platform: &str, spotlight_index: i64) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Show: {}", str_of(&s.show)));
    lines.push(format!("Episode: {} — \"{}\"", s.tag, str_of(&s.title)));
    if super::truthy(&s.season_title) {
        lines.push(format!("Arc/Season title: {}", str_of(&s.season_title)));
    }
    lines.push(format!("Platform: {platform}"));
    if let Some(r) = s.runtime_minutes.filter(|r| *r != 0) {
        lines.push(format!("Runtime: approximately {r} minutes"));
    }
    lines.push(String::new());
    lines.push("Section arc:".into());
    lines.push(format!("  {}", s.section_arc));
    lines.push(String::new());
    if super::truthy(&s.cold_open_scene) {
        lines.push(format!("Cold open setting: {}", str_of(&s.cold_open_scene)));
    }
    if !s.cold_open_lines.is_empty() {
        lines.push("Cold open excerpt (first 3 lines):".into());
        for (speaker, text) in &s.cold_open_lines {
            let mut display = str_of(speaker);
            if let Some(cm) = s
                .cast
                .iter()
                .find(|c| Some(c.0.as_str()) == speaker.as_str())
            {
                display = str_of(&cm.1);
            }
            let mut t = str_of(text);
            if t.chars().count() > 200 {
                t = t.chars().take(197).collect::<String>() + "…";
            }
            lines.push(format!("  {display}: \"{t}\""));
        }
    }
    lines.push(String::new());
    if !s.cast.is_empty() {
        lines.push("Cast:".into());
        for (_, full, role) in &s.cast {
            let role_str = if role.is_empty() {
                String::new()
            } else {
                format!(" — {role}")
            };
            lines.push(format!("  {}{role_str}", str_of(full)));
        }
        lines.push(String::new());
        let target = &s.cast[spotlight_index.rem_euclid(s.cast.len() as i64) as usize];
        lines.push(format!(
            "Spotlight post subject: {} ({})",
            str_of(&target.1),
            target.2
        ));
        lines.push(String::new());
    }
    lines.push(
        "Write three Facebook post variants using exactly these markdown headings:\n## Hype Post\n## Quote Post\n## Spotlight Post\n\nHype: New episode announcement, teaser tone. Mention the show name, episode title, and Berkshire Talking Chronicle. No spoilers beyond the cold open setting.\nQuote: Pull a memorable line from the cold open excerpt above. Format as a blockquote or quoted text. Add a brief tune-in call to action.\nSpotlight: Feature the spotlight subject. Connect their character to the episode theme."
            .into(),
    );
    lines.join("\n")
}

/// `publish_episode(...)` → success. `Err(SysExit)` for the key check.
fn publish_episode(
    slug: &str,
    tag: &str,
    platform: &str,
    dry_run: bool,
    model: &str,
) -> anyhow::Result<bool> {
    let p = derive_paths(slug, tag);
    let posts_path = workspace_root()
        .join("posts")
        .join(slug)
        .join(format!("{tag}_posts.md"));
    if !p["parsed"].exists() {
        log::warning(&format!(
            "  Skipping {tag} — parsed JSON not found: {}",
            p["parsed"].display()
        ));
        return Ok(false);
    }
    let parsed: Value = serde_json::from_str(&fs::read_to_string(&p["parsed"])?)?;
    let cast_cfg: Option<Value> = if p["cast"].exists() {
        Some(serde_json::from_str(&fs::read_to_string(&p["cast"])?)?)
    } else {
        log::warning(&format!(
            "  Cast config not found: {} — cast list will be empty",
            p["cast"].display()
        ));
        None
    };
    let summary = extract_summary(&parsed, cast_cfg.as_ref(), &p["master"])?;
    let episode_number = summary.episode.as_i64().filter(|e| *e != 0).unwrap_or(0);
    let cast_count = summary.cast.len().max(1) as i64;
    let spotlight = (episode_number - 1).rem_euclid(cast_count);
    let user_message = build_user_message(&summary, platform, spotlight);

    if dry_run {
        log::info(&format!("\n--- Dry run: {tag} ---"));
        log::info(&format!("\n[SYSTEM PROMPT]\n{SYSTEM_PROMPT}"));
        log::info(&format!("\n[USER MESSAGE]\n{user_message}"));
        let sys_tokens = (SYSTEM_PROMPT.chars().count() as f64 / 4.0).ceil() as i64;
        let user_tokens = (user_message.chars().count() as f64 / 4.0).ceil() as i64;
        log::info(&format!(
            "\nEstimated input tokens: ~{} (system: ~{sys_tokens}, user: ~{user_tokens})",
            sys_tokens + user_tokens
        ));
        log::info(&format!(
            "Output would be written to: {}",
            posts_path.display()
        ));
        return Ok(true);
    }

    log::info(&format!("  Generating posts for {tag}..."));
    let Some(api_key) = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    else {
        log::error("ANTHROPIC_API_KEY environment variable not set. Export your API key before running xil publish.");
        return Err(super::SysExit(String::new()).into());
    };
    let body = json!({
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": user_message}],
        "model": model,
        "system": [{"type": "text", "text": SYSTEM_PROMPT, "cache_control": {"type": "ephemeral"}}],
    });
    let text = match xil_api::anthropic::Client::new(&api_key).messages_create(&body) {
        Ok(resp) => resp["content"][0]["text"].as_str().map(str::to_string),
        Err(e) => {
            log::error(&format!("  API error for {tag}: {e}"));
            return Ok(false);
        }
    };
    let Some(posts) = text else {
        log::error(&format!(
            "  API error for {tag}: 'NoneType' object is not subscriptable"
        ));
        return Ok(false);
    };
    if let Some(parent) = posts_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let today = chrono::Local::now().format("%Y-%m-%d");
    let mut out = format!(
        "# {} — {} \"{}\" Social Posts\nGenerated: {today}  |  Platform: {platform}\n\n---\n\n",
        str_of(&summary.show),
        summary.tag,
        str_of(&summary.title)
    );
    out.push_str(&posts);
    if !posts.ends_with('\n') {
        out.push('\n');
    }
    fs::write(&posts_path, out)?;
    log::info(&format!("  Written: {}", posts_path.display()));
    Ok(true)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("publish");
    let result = {
        let _banner = banner::begin(super::prog(), &super::argv_line(args));
        execute(args)
    };
    match result {
        // A bare sys.exit(1) after an error already logged.
        Err(e)
            if e.downcast_ref::<super::SysExit>()
                .is_some_and(|s| s.0.is_empty()) =>
        {
            Ok(1)
        }
        other => super::finish(other),
    }
}

fn execute(args: &[OsString]) -> anyhow::Result<i32> {
    let a: Args = match super::parse_or_exit("xil-publish", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let episode = a.episode.clone().filter(|s| !s.is_empty());
    let tag = a.tag.clone().filter(|s| !s.is_empty());
    if episode.is_none() && tag.is_none() && !a.all {
        log::error("Specify --episode TAG, --tag TAG, or --all");
        return Ok(1);
    }
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    if a.all {
        let dir = Path::new("parsed").join(&slug);
        let mut paths: Vec<String> = xil_core::fsutil::glob_children(&dir, "parsed_", ".json")
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        if paths.is_empty() {
            log::warning(&format!("No parsed JSON files found under parsed/{slug}/"));
            return Ok(1);
        }
        log::info(&format!(
            "Batch mode: {} episode(s) found for '{slug}'",
            paths.len()
        ));
        let mut success = 0;
        for path in &paths {
            let base = xil_core::fsutil::basename(Path::new(path));
            let ep = base.strip_prefix("parsed_").unwrap_or(&base);
            let ep = ep.strip_suffix(".json").unwrap_or(ep);
            if publish_episode(&slug, ep, &a.platform, a.dry_run, &a.model)? {
                success += 1;
            }
        }
        log::info(&format!("\n{success}/{} episodes processed.", paths.len()));
        return Ok(0);
    }
    let t = episode.or(tag).unwrap_or_default();
    if publish_episode(&slug, &t, &a.platform, a.dry_run, &a.model)? {
        Ok(0)
    } else {
        Ok(1)
    }
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

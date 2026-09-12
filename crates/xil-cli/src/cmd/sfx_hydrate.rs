//! `xil sfx-hydrate` — write pipe-hint fields from a parsed JSON into the
//! SFX config without re-parsing. Port of `XILU013_sfx_hydrate.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use xil_core::fsutil::basename;
use xil_core::script::hints::{filter_sfx_overrides, format_hint_attr};
use xil_core::workspace::{derive_paths, resolve_slug};
use xil_core::{banner, log};

use super::parse::{backfill_sfx_sources, hint_target_exists};

const SCRIPT_NAME: &str = "XILU013_sfx_hydrate.py";

#[derive(Parser)]
#[command(
    name = "xil-sfx-hydrate",
    about = "Write pipe-hint source and attribute fields (play_volume_pct, play_duration_pct) from parsed JSON into the SFX config without re-parsing the script."
)]
struct Args {
    /// Episode tag (e.g. S04E04)
    #[arg(long, required_unless_present = "tag", conflicts_with = "tag")]
    episode: Option<String>,
    /// Raw non-episodic tag (e.g. V01C03)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Override parsed JSON path
    #[arg(long)]
    parsed: Option<PathBuf>,
    /// Override SFX config path
    #[arg(long)]
    sfx: Option<PathBuf>,
    /// Replace a cue's existing 'source' when the script hint differs, instead of only filling in missing ones — the only way to correct a 'NEW STEM NEEDED' placeholder or a stale path. A replacement is skipped (with a warning) if the hinted file is not on disk. Replacements are written to the edit journal, so they survive a later rebuild from a fresh script. Preview with --dry-run first.
    #[arg(long)]
    force: bool,
    /// Report changes without writing
    #[arg(long)]
    dry_run: bool,
}

enum Action {
    Add(String),
    Replace(String),
    Skip(String),
}

/// What one cue would get: the source action, the attribute hints that
/// differ from the config, and the source it holds today.
struct Pending {
    key: String,
    action: Option<Action>,
    changed: IndexMap<String, f64>,
    current: Option<String>,
}

/// Overrides carried on a parsed direction entry.
fn entry_overrides(e: &Value) -> IndexMap<String, f64> {
    e.get("sfx_overrides")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect()
        })
        .unwrap_or_default()
}

/// Apply the hints in `parsed` to the config at `sfx_path`; returns how many
/// entries would be (or were) updated.
pub fn hydrate_sfx_config(
    parsed: &Value,
    sfx_path: &Path,
    dry_run: bool,
    force: bool,
) -> anyhow::Result<usize> {
    let sfx_data: Value = serde_json::from_str(&fs::read_to_string(sfx_path)?)?;
    let effects = sfx_data
        .get("effects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let empty = Map::new();

    let mut pending: Vec<Pending> = Vec::new();
    let mut seen_clean: Vec<String> = Vec::new();
    for entry in parsed
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if entry.get("type").and_then(Value::as_str) != Some("direction") {
            continue;
        }
        let sfx_source = entry
            .get("sfx_source")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let overrides = entry_overrides(&entry);
        if sfx_source.is_none() && overrides.is_empty() {
            continue;
        }
        let text = entry
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if seen_clean.contains(&text) {
            continue;
        }
        seen_clean.push(text.clone());

        let effect = effects
            .get(&text)
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let current = effect
            .get("source")
            .filter(|v| super::truthy(v))
            .map(xil_core::workspace::python_str);
        // Classify so --dry-run shows what a forced run would actually do.
        let action = match (&sfx_source, &current) {
            (Some(src), None) => Some(Action::Add(src.clone())),
            (Some(src), Some(cur)) if force && cur != src => Some(if hint_target_exists(src) {
                Action::Replace(src.clone())
            } else {
                Action::Skip(src.clone())
            }),
            _ => None,
        };
        // Filter with the same rule the write path uses, so the report never
        // promises a field backfill_sfx_sources will drop.
        let is_silence = effect.get("type").and_then(Value::as_str) == Some("silence");
        let mut warn = |m: String| log::warning(&m);
        let usable = filter_sfx_overrides(
            &text,
            is_silence,
            &overrides,
            if dry_run { Some(&mut warn) } else { None },
        );
        let changed: IndexMap<String, f64> = usable
            .into_iter()
            .filter(|(k, v)| effect.get(k).and_then(Value::as_f64) != Some(*v))
            .collect();
        if action.is_some() || !changed.is_empty() {
            pending.push(Pending {
                key: text,
                action,
                changed,
                current,
            });
        }
    }

    if pending.is_empty() {
        log::info("  Nothing to apply — SFX config already matches the script hints.");
        return Ok(0);
    }

    let prefix = if dry_run { "[dry-run] " } else { "" };
    let name = |s: &str| basename(Path::new(s));
    let mut skipped = 0usize;
    for p in &pending {
        let mut parts: Vec<String> = Vec::new();
        let current = p.current.as_deref().unwrap_or("?");
        match &p.action {
            Some(Action::Add(src)) => parts.push(format!("+ {}", name(src))),
            Some(Action::Replace(src)) => parts.push(format!("{} → {}", name(current), name(src))),
            Some(Action::Skip(src)) => {
                skipped += 1;
                log::warning(&format!(
                    "  {prefix}  {}  SKIP  {} not found on disk — keeping {}",
                    p.key,
                    name(src),
                    name(current)
                ));
                continue;
            }
            None => {}
        }
        parts.extend(p.changed.iter().map(|(k, v)| format_hint_attr(k, *v)));
        log::info(&format!("  {prefix}  {}  →  {}", p.key, parts.join(", ")));
    }

    if skipped > 0 {
        log::warning(&format!(
            "  {skipped} replacement(s) skipped because the hinted file is missing."
        ));
    }

    if !dry_run {
        backfill_sfx_sources(parsed, sfx_path, force)?;
    }
    Ok(pending.len())
}

fn execute(a: &Args, tag: &str) -> anyhow::Result<i32> {
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let p = derive_paths(&slug, tag);
    let parsed_path = a.parsed.clone().unwrap_or_else(|| p["parsed"].clone());
    let sfx_path = a.sfx.clone().unwrap_or_else(|| p["sfx"].clone());

    if !parsed_path.exists() {
        log::error(&format!("Parsed JSON not found: {}", parsed_path.display()));
        return Ok(1);
    }
    if !sfx_path.exists() {
        log::error(&format!("SFX config not found: {}", sfx_path.display()));
        log::info("Run `xil parse --episode TAG` first to generate it.");
        return Ok(1);
    }

    let parsed: Value = serde_json::from_str(&fs::read_to_string(&parsed_path)?)?;
    let count = hydrate_sfx_config(&parsed, &sfx_path, a.dry_run, a.force)?;
    if count > 0 {
        let action = if a.dry_run { "Would update" } else { "Updated" };
        log::info(&format!(
            "  {action} {count} source field(s) in {}",
            sfx_path.display()
        ));
    }
    if a.dry_run && count > 0 {
        log::info("  Re-run without --dry-run to apply changes.");
    }
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("sfx-hydrate");
    let a: Args = match super::parse_or_exit("xil-sfx-hydrate", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let _banner = banner::begin(SCRIPT_NAME, &super::argv_line(args));
    execute(&a, &tag)
}

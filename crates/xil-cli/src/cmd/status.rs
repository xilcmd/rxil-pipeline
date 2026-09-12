//! `xil status` — make-style staleness check of an episode's artifacts.
//! Port of `XILU019_episode_status.py`.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use chrono::{Local, TimeZone};
use clap::Parser;
use regex::Regex;
use serde_json::{json, Value};
use xil_audio::tags::{read_sfx_grade, SFX_GRADE_REJECTED};
use xil_core::fsutil::{basename, list_dir_raw, pathlib_glob, pathlib_rglob_contains, sort_py};
use xil_core::log;
use xil_core::pyjson::{dumps, Style};
use xil_core::sfxlib::shared_sfx_path;
use xil_core::workspace::{derive_paths, resolve_slug, workspace_root};

static TAG_SEARCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)S\d+E\d+").unwrap());
static TAG_FULL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^S\d+E\d+$").unwrap());

const OK: &str = "OK";
const STALE: &str = "STALE";
const MISSING: &str = "MISSING";
const NONE: &str = "-";

#[derive(Parser)]
#[command(
    name = "xil-status",
    about = "Make-style staleness checker for episode pipeline artifacts. Reports, per stage, whether outputs \
             are up to date with their inputs, and prints the xil commands needed to refresh anything stale. \
             Nothing is rebuilt.",
    after_help = "Examples:\n  xil status --episode S01E01\n  xil status S01E01 --show \"Night Owls\"\n  \
                  xil status --all\n  xil status --episode S01E01 --json\n"
)]
struct Args {
    /// Episode tag to check (e.g. S01E01). Omit with --all.
    #[arg(value_name = "TAG")]
    episode: Option<String>,
    /// Episode tag (alternative to the positional argument)
    #[arg(long = "episode", short = 'e', value_name = "TAG")]
    episode_flag: Option<String>,
    /// Show name or slug (default: resolved from project.json / XIL_PROJECTROOT)
    #[arg(long, short = 's', value_name = "SHOW")]
    show: Option<String>,
    /// Check every episode of the show (summary row per episode)
    #[arg(long, short = 'a')]
    all: bool,
    /// Google Drive dir holding the source .gdoc (default: $XIL_GDOC_DIR or /mnt/i/My Drive)
    #[arg(long, value_name = "DIR")]
    gdoc_dir: Option<PathBuf>,
    /// Emit results as JSON (single-episode mode only)
    #[arg(long)]
    json: bool,
    /// List each output file with its mtime below the stage row
    #[arg(long, short = 'v')]
    verbose: bool,
}

/// Freshness result for one pipeline stage.
#[derive(Debug, Clone)]
pub struct StageStatus {
    pub name: &'static str,
    pub status: &'static str,
    pub newest_input: Option<f64>,
    pub newest_output: Option<f64>,
    pub oldest_output: Option<f64>,
    pub output_count: usize,
    pub note: String,
    pub refresh: String,
    pub output_files: Vec<PathBuf>,
}

/// `st_mtime` as CPython computes it: `sec + nsec * 1e-9` in doubles.
fn mtime(p: &Path) -> Option<f64> {
    let d = fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    Some(d.as_secs() as f64 + d.subsec_nanos() as f64 * 1e-9)
}

/// mtimes of every regular file in `paths` (directories walked recursively).
fn mtimes(paths: &[PathBuf]) -> Vec<f64> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut stack = vec![p.clone()];
            while let Some(d) = stack.pop() {
                for e in list_dir_raw(&d) {
                    if e.is_dir() {
                        stack.push(e);
                    } else if e.is_file() {
                        out.extend(mtime(&e));
                    }
                }
            }
        } else if p.is_file() {
            out.extend(mtime(p));
        }
    }
    out
}

fn fmt_time(t: Option<f64>) -> String {
    match t {
        None => "—".to_string(),
        Some(t) => {
            let secs = t.floor() as i64;
            let nanos = ((t - t.floor()) * 1e9) as u32;
            Local
                .timestamp_opt(secs, nanos)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "—".to_string())
        }
    }
}

fn gdoc_files(gdoc_dir: &Path, tag: &str) -> Vec<PathBuf> {
    if !gdoc_dir.is_dir() {
        return Vec::new();
    }
    let mut set: BTreeSet<PathBuf> = pathlib_glob(gdoc_dir, tag, ".gdoc").into_iter().collect();
    for p in list_dir_raw(gdoc_dir) {
        if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
            if let Some(stem) = n.strip_suffix(".gdoc") {
                if stem.contains(tag) {
                    set.insert(p);
                }
            }
        }
    }
    let mut v: Vec<PathBuf> = set.into_iter().collect();
    sort_py(&mut v);
    v
}

fn script_files(root: &Path, tag: &str) -> Vec<PathBuf> {
    let scripts = root.join("scripts");
    if !scripts.is_dir() {
        return Vec::new();
    }
    pathlib_rglob_contains(&scripts, tag, ".md")
        .into_iter()
        .filter(|p| !basename(p).starts_with("revised_"))
        .collect()
}

fn glob_in(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    if dir.is_dir() {
        pathlib_glob(dir, "", suffix)
    } else {
        Vec::new()
    }
}

fn master_files(root: &Path, slug: &str, tag: &str) -> Vec<PathBuf> {
    let masters = root.join("masters");
    let mut found = Vec::new();
    if masters.is_dir() {
        found.extend(pathlib_glob(&masters, &format!("{tag}_{slug}_"), ".mp3"));
        let sub = masters.join(slug);
        if sub.is_dir() {
            found.extend(pathlib_glob(&sub, &format!("{tag}_"), ".mp3"));
        }
    }
    let legacy = root.join(format!("{slug}_{tag}_master.mp3"));
    if legacy.is_file() {
        found.push(legacy);
    }
    let set: BTreeSet<PathBuf> = found.into_iter().filter(|p| p.is_file()).collect();
    let mut v: Vec<PathBuf> = set.into_iter().collect();
    sort_py(&mut v);
    v
}

fn fmax(v: &[f64]) -> Option<f64> {
    v.iter().copied().reduce(f64::max)
}
fn fmin(v: &[f64]) -> Option<f64> {
    v.iter().copied().reduce(f64::min)
}

fn evaluate_stage(
    name: &'static str,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    refresh: &str,
    count: Option<usize>,
) -> StageStatus {
    let in_times = mtimes(inputs);
    let out_times = mtimes(outputs);
    let newest_in = fmax(&in_times);
    let newest_out = fmax(&out_times);
    let oldest_out = fmin(&out_times);
    let (status, suggested) = if out_times.is_empty() {
        (MISSING, refresh.to_string())
    } else if matches!((newest_in, newest_out), (Some(i), Some(o)) if i > o) {
        (STALE, refresh.to_string())
    } else {
        (OK, String::new())
    };
    StageStatus {
        name,
        status,
        newest_input: newest_in,
        newest_output: newest_out,
        oldest_output: oldest_out,
        output_count: count.unwrap_or(out_times.len()),
        note: String::new(),
        refresh: suggested,
        output_files: outputs.iter().filter(|p| p.is_file()).cloned().collect(),
    }
}

pub fn evaluate_episode(
    slug: &str,
    tag: &str,
    gdoc_dir: &Path,
    include_source: bool,
) -> Vec<StageStatus> {
    let root = workspace_root();
    let paths = derive_paths(slug, tag);

    let gdocs = if include_source {
        gdoc_files(gdoc_dir, tag)
    } else {
        Vec::new()
    };
    let scripts = script_files(&root, tag);
    let parsed = vec![paths["parsed"].clone()];
    let stems = glob_in(&paths["stems"], ".mp3");
    let stems_manifest = glob_in(&paths["stems"], "_stem_manifest.json");
    let daw = glob_in(&paths["daw"], ".wav");
    let masters = master_files(&root, slug, tag);

    let script_refresh = "(re-import the production doc in xil-gui)";
    // Python's max() keeps the first of equal keys; fold the same way.
    let newest_script = scripts.iter().fold(None::<(&PathBuf, f64)>, |best, p| {
        let t = mtime(p).unwrap_or(0.0);
        match best {
            Some((_, bt)) if bt >= t => best,
            _ => Some((p, t)),
        }
    });
    let parse_refresh = match newest_script {
        Some((p, _)) => format!(
            "xil parse {} --episode {tag}",
            p.strip_prefix(&root)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| p.display().to_string())
        ),
        None => format!("xil parse <script> --episode {tag}"),
    };

    let mut stems_outputs = stems.clone();
    stems_outputs.extend(stems_manifest);

    let mut stages = Vec::new();
    if include_source {
        stages.push(evaluate_stage("source", &[], &gdocs, "", None));
        stages.push(evaluate_stage(
            "script",
            &gdocs,
            &scripts,
            script_refresh,
            None,
        ));
        if gdocs.is_empty() {
            let src = &mut stages[0];
            src.status = NONE;
            src.note = if gdoc_dir.is_dir() {
                "no source doc"
            } else {
                "no gdoc dir"
            }
            .to_string();
            src.refresh = String::new();
        }
    }

    let mut daw_inputs = stems.clone();
    if paths["sfx"].exists() {
        daw_inputs.push(paths["sfx"].clone());
    }

    stages.push(evaluate_stage(
        "parsed",
        &scripts,
        &parsed,
        &parse_refresh,
        None,
    ));
    stages.push(evaluate_stage(
        "stems",
        &parsed,
        &stems_outputs,
        &format!("xil produce --episode {tag}"),
        Some(stems.len()),
    ));
    stages.push(evaluate_stage(
        "daw",
        &daw_inputs,
        &daw,
        &format!("xil daw --episode {tag}"),
        None,
    ));
    stages.push(evaluate_stage(
        "master",
        &daw,
        &masters,
        &format!("xil master --episode {tag}"),
        None,
    ));
    stages
}

fn worst(stages: &[StageStatus]) -> &'static str {
    let relevant: Vec<&str> = stages
        .iter()
        .filter(|s| s.name != "source")
        .map(|s| s.status)
        .collect();
    if relevant.contains(&MISSING) {
        MISSING
    } else if relevant.contains(&STALE) {
        STALE
    } else {
        OK
    }
}

/// Effect keys whose shared pool file is graded `rejected`. Best-effort,
/// never fails.
pub fn rejected_sfx(slug: &str, tag: &str) -> Vec<String> {
    let root = workspace_root();
    let cfg_path = root
        .join("configs")
        .join(slug)
        .join(format!("sfx_{tag}.json"));
    let Ok(text) = fs::read_to_string(&cfg_path) else {
        return Vec::new();
    };
    let Ok(cfg) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let sfx_dir = root.join("SFX");
    let mut rejected = Vec::new();
    for (key, eff) in cfg
        .get("effects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
    {
        let mut candidates: Vec<PathBuf> = Vec::new();
        let src = eff
            .get("source")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if let Some(src) = src {
            let p = Path::new(src);
            candidates.push(if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            });
        } else {
            let base = shared_sfx_path(&sfx_dir, &key, "elevenlabs");
            let stem = basename(&base).trim_end_matches(".mp3").to_string();
            candidates.push(base);
            // backend-tagged variants, e.g. SFX/{slug}.audioldm2.mp3
            for p in list_dir_raw(&sfx_dir) {
                let n = basename(&p);
                if !n.starts_with('.')
                    && n.starts_with(&format!("{stem}."))
                    && n.ends_with(".mp3")
                    && n.len() >= stem.len() + 5
                {
                    candidates.push(p);
                }
            }
        }
        if candidates
            .iter()
            .any(|c| c.exists() && read_sfx_grade(c) == SFX_GRADE_REJECTED)
        {
            rejected.push(key);
        }
    }
    rejected.sort();
    rejected
}

fn print_episode(slug: &str, tag: &str, stages: &[StageStatus], verbose: bool) {
    let root = workspace_root();
    log::info(&format!("Episode {tag} (show: {slug})"));
    log::info("");
    log::info(&format!(
        "  {:<9} {:<8} {:<18} {:<18} FILES",
        "STAGE", "STATUS", "NEWEST INPUT", "NEWEST OUTPUT"
    ));
    for s in stages {
        let in_str = fmt_time(s.newest_input);
        let out_str = if s.note.is_empty() {
            fmt_time(s.newest_output)
        } else {
            format!("({})", s.note)
        };
        let count = if s.name == "source" && s.status == NONE {
            "—".to_string()
        } else {
            s.output_count.to_string()
        };
        let marker = match s.status {
            STALE => "   ← input is newer (stage not re-run)",
            MISSING => "   ← not built yet",
            _ => "",
        };
        log::info(&format!(
            "  {:<9} {:<8} {:<18} {:<18} {count}{marker}",
            s.name, s.status, in_str, out_str
        ));
        if verbose && !s.output_files.is_empty() {
            for f in &s.output_files {
                let rel = f
                    .strip_prefix(&root)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| f.display().to_string());
                log::info(&format!("             {}  {rel}", fmt_time(mtime(f))));
            }
        }
    }
    let refreshes: Vec<&str> = stages
        .iter()
        .filter(|s| !s.refresh.is_empty())
        .map(|s| s.refresh.as_str())
        .collect();
    if !refreshes.is_empty() {
        log::info("");
        log::info("Stale/missing — refresh with:");
        for cmd in refreshes {
            log::info(&format!("  {cmd}"));
        }
    }
    let rejected = rejected_sfx(slug, tag);
    if !rejected.is_empty() {
        log::info("");
        log::warning(&format!(
            "{} SFX graded rejected — omitted from production (fill the gap or re-grade in xil-gui):",
            rejected.len()
        ));
        for key in rejected {
            log::info(&format!("    ✗ {key}"));
        }
    }
}

fn opt_f(v: Option<f64>) -> Value {
    v.map(Value::from).unwrap_or(Value::Null)
}

fn emit_json(slug: &str, tag: &str, stages: &[StageStatus]) {
    let payload = json!({
        "show": slug,
        "episode": tag,
        "overall": worst(stages),
        "rejected_sfx": rejected_sfx(slug, tag),
        "stages": stages.iter().map(|s| json!({
            "name": s.name,
            "status": s.status,
            "newest_input": opt_f(s.newest_input),
            "newest_output": opt_f(s.newest_output),
            "oldest_output": opt_f(s.oldest_output),
            "output_count": s.output_count,
            "note": s.note,
            "refresh": s.refresh,
        })).collect::<Vec<_>>(),
    });
    println!("{}", dumps(&payload, Style::INDENT2));
}

fn stem_of(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn discover_tags(slug: &str) -> Vec<String> {
    let root = workspace_root();
    let mut tags = BTreeSet::new();
    let parsed_dir = root.join("parsed").join(slug);
    if parsed_dir.is_dir() {
        for p in pathlib_glob(&parsed_dir, "parsed_", ".json") {
            if let Some(m) = TAG_SEARCH.find(&stem_of(&p)) {
                tags.insert(m.as_str().to_uppercase());
            }
        }
    }
    for sub in ["stems", "daw"] {
        let base = root.join(sub).join(slug);
        if base.is_dir() {
            for child in list_dir_raw(&base) {
                if child.is_dir() && TAG_FULL.is_match(&basename(&child)) {
                    tags.insert(basename(&child).to_uppercase());
                }
            }
        }
    }
    let masters_dir = root.join("masters");
    if masters_dir.is_dir() {
        for p in list_dir_raw(&masters_dir) {
            let n = basename(&p);
            if n.contains(&format!("_{slug}_")) && n.ends_with(".mp3") && n.len() > slug.len() + 6 {
                if let Some(m) = TAG_SEARCH.find(&stem_of(&p)) {
                    tags.insert(m.as_str().to_uppercase());
                }
            }
        }
        let sub = masters_dir.join(slug);
        if sub.is_dir() {
            for p in pathlib_glob(&sub, "", ".mp3") {
                if let Some(m) = TAG_SEARCH.find(&stem_of(&p)) {
                    tags.insert(m.as_str().to_uppercase());
                }
            }
        }
    }
    tags.into_iter().collect()
}

fn print_all(slug: &str, tags: &[String], gdoc_dir: &Path) -> i32 {
    if tags.is_empty() {
        log::info(&format!("No episodes found for show '{slug}'."));
        return 0;
    }
    log::info(&format!("Show: {slug} — {} episode(s)", tags.len()));
    log::info("");
    log::info(&format!("  {:<10} {:<8} NEXT STEP", "EPISODE", "STATUS"));
    let mut exit_code = 0;
    for tag in tags {
        let stages = evaluate_episode(slug, tag, gdoc_dir, true);
        let w = worst(&stages);
        if w != OK {
            exit_code = 1;
        }
        let mut next_step = stages
            .iter()
            .find(|s| !s.refresh.is_empty())
            .map(|s| s.refresh.clone())
            .unwrap_or_default();
        let rej = rejected_sfx(slug, tag);
        if !rej.is_empty() {
            let prefix = if next_step.is_empty() {
                String::new()
            } else {
                format!("{next_step}   ")
            };
            next_step = format!("{prefix}⚠ {} SFX rejected", rej.len());
        }
        log::info(&format!("  {tag:<10} {w:<8} {next_step}"));
    }
    exit_code
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("status");
    let a: Args = match super::parse_or_exit("xil-status", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let gdoc_dir = a.gdoc_dir.clone().unwrap_or_else(|| {
        PathBuf::from(env::var("XIL_GDOC_DIR").unwrap_or_else(|_| "/mnt/i/My Drive".into()))
    });
    if !gdoc_dir.is_dir() {
        log::warning(&format!(
            "Google Drive dir not available: {} — skipping source check.",
            gdoc_dir.display()
        ));
    }

    if a.all {
        if a.json {
            log::error("--json is not supported with --all.");
            return Ok(2);
        }
        let tags = discover_tags(&slug);
        return Ok(print_all(&slug, &tags, &gdoc_dir));
    }

    let Some(tag) = a.episode_flag.clone().or(a.episode.clone()) else {
        log::error("Provide an episode tag (e.g. S01E01) or use --all.");
        return Ok(2);
    };
    let stages = evaluate_episode(&slug, &tag, &gdoc_dir, true);
    if a.json {
        emit_json(&slug, &tag, &stages);
    } else {
        print_episode(&slug, &tag, &stages, a.verbose);
    }
    Ok(if worst(&stages) == OK { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path, secs: i64) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "x").unwrap();
        let t = filetime::FileTime::from_unix_time(secs, 0);
        filetime::set_file_mtime(p, t).unwrap();
    }

    #[test]
    fn stage_status_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        touch(&r.join("in.txt"), 1_700_000_010);
        touch(&r.join("out_old.txt"), 1_700_000_005);
        touch(&r.join("out_new.txt"), 1_700_000_020);
        let missing = evaluate_stage("x", &[r.join("in.txt")], &[r.join("nope")], "fix", None);
        assert_eq!(
            (
                missing.status,
                missing.refresh.as_str(),
                missing.output_count
            ),
            (MISSING, "fix", 0)
        );
        let stale = evaluate_stage(
            "x",
            &[r.join("in.txt")],
            &[r.join("out_old.txt")],
            "fix",
            None,
        );
        assert_eq!(stale.status, STALE);
        let ok = evaluate_stage(
            "x",
            &[r.join("in.txt")],
            &[r.join("out_old.txt"), r.join("out_new.txt")],
            "fix",
            Some(7),
        );
        assert_eq!(
            (ok.status, ok.refresh.as_str(), ok.output_count),
            (OK, "", 7)
        );
        assert_eq!(ok.oldest_output, Some(1_700_000_005.0));
        assert_eq!(worst(&[missing.clone(), stale.clone()]), MISSING);
        assert_eq!(worst(&[ok.clone(), stale]), STALE);
        assert_eq!(worst(&[ok]), OK);
    }

    #[test]
    fn tag_discovery_across_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        std::env::set_var("XIL_PROJECTROOT", r);
        touch(&r.join("parsed/s/parsed_S01E01.json"), 1);
        touch(&r.join("stems/s/s01e02/a.mp3"), 1);
        touch(&r.join("daw/s/notatag/a.wav"), 1);
        touch(&r.join("masters/S01E03_s_2026-01-01.mp3"), 1);
        touch(&r.join("masters/S01E09_other_2026-01-01.mp3"), 1);
        touch(&r.join("masters/s/S01E04_master.mp3"), 1);
        assert_eq!(
            discover_tags("s"),
            vec!["S01E01", "S01E02", "S01E03", "S01E04"]
        );
        std::env::remove_var("XIL_PROJECTROOT");
    }
}

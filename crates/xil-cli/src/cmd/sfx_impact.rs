//! `xil sfx-impact` — which source-backed cues are clipped short by
//! `duration_seconds`, and by how much. Port of `XILU021_sfx_impact.py`.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value};
use xil_core::fsutil::{basename, pathlib_glob, sort_py};
use xil_core::pycsv;
use xil_core::pyfmt::{fixed, head, html_escape, pad_left, pad_right, round_to};
use xil_core::pyjson::py_float;
use xil_core::workspace::{resolve_slug, workspace_root};
use xil_core::{banner, log};

const SCRIPT_NAME: &str = "XILU021_sfx_impact";

/// Tier thresholds in seconds of lost audio.
const NOCHANGE_S: f64 = 0.1;
const MINOR_S: f64 = 3.0;

pub const TIERS: [&str; 5] = ["1-nochange", "2-minor", "3-review", "EXCLUDED", "MISSING"];

const CSV_COLUMNS: [&str; 15] = [
    "show",
    "episode",
    "cue",
    "source_file",
    "duration_seconds",
    "play_duration",
    "loop",
    "natural_s",
    "plays_now_s",
    "delta_s",
    "lost_pct",
    "tier",
    "placement",
    "remediation",
    "note",
];

#[derive(Parser)]
#[command(
    name = "xil-sfx-impact",
    about = "Report which source-backed SFX cues are clipped by duration_seconds, how much audio each loses, and the config change that would restore it. Read-only — no config is ever modified."
)]
struct Args {
    /// Restrict to one show slug (default: every show in the workspace)
    #[arg(long)]
    show: Option<String>,
    /// Restrict to one episode tag (e.g. S01E01)
    #[arg(long, alias = "tag")]
    episode: Option<String>,
    /// CSV output path, or '-' for stdout (default: reports/sfx_impact_<date>.csv)
    #[arg(long)]
    output: Option<String>,
    /// Also write a standalone HTML review page (default path: reports/sfx_impact_<date>.html)
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    html: Option<String>,
    /// Only report cues at this tier ('actionable' = tiers 2 and 3)
    #[arg(long, value_parser = ["2-minor", "3-review", "actionable"])]
    tier: Option<String>,
    /// Suppress the console summary (CSV only)
    #[arg(long)]
    quiet: bool,
}

/// One source-backed cue measured against its file on disk.
#[derive(Debug, Clone)]
pub struct CueImpact {
    pub show: String,
    pub episode: String,
    pub cue: String,
    pub source_file: String,
    /// The config values as they are, so the CSV prints them as Python
    /// would (`5.0`, `50`, empty for absent).
    pub duration_seconds: Value,
    pub play_duration: Value,
    pub loop_: bool,
    pub natural_s: Option<f64>,
    pub plays_now_s: Option<f64>,
    pub delta_s: Option<f64>,
    pub lost_pct: Option<f64>,
    pub tier: String,
    pub placement: String,
    pub remediation: String,
    pub note: String,
}

impl CueImpact {
    /// The CSV row, floats rounded to one place.
    fn row(&self) -> Map<String, Value> {
        let opt = |v: Option<f64>| match v {
            Some(x) => py_float(round_to(x, 1)),
            None => Value::String(String::new()),
        };
        let mut m = Map::new();
        m.insert("show".into(), self.show.clone().into());
        m.insert("episode".into(), self.episode.clone().into());
        m.insert("cue".into(), self.cue.clone().into());
        m.insert("source_file".into(), self.source_file.clone().into());
        m.insert("duration_seconds".into(), self.duration_seconds.clone());
        m.insert("play_duration".into(), self.play_duration.clone());
        m.insert("loop".into(), Value::Bool(self.loop_));
        m.insert("natural_s".into(), opt(self.natural_s));
        m.insert("plays_now_s".into(), opt(self.plays_now_s));
        m.insert("delta_s".into(), opt(self.delta_s));
        m.insert("lost_pct".into(), opt(self.lost_pct));
        m.insert("tier".into(), self.tier.clone().into());
        m.insert("placement".into(), self.placement.clone().into());
        m.insert("remediation".into(), self.remediation.clone().into());
        m.insert("note".into(), self.note.clone().into());
        m
    }
}

#[derive(Default)]
pub struct ImpactReport {
    pub impacts: Vec<CueImpact>,
    pub configs_scanned: usize,
}

impl ImpactReport {
    fn actionable(&self) -> Vec<&CueImpact> {
        self.impacts
            .iter()
            .filter(|i| i.tier == "2-minor" || i.tier == "3-review")
            .collect()
    }

    fn tally(&self) -> indexmap::IndexMap<String, usize> {
        let mut counts: indexmap::IndexMap<String, usize> =
            TIERS.iter().map(|t| (t.to_string(), 0)).collect();
        for i in &self.impacts {
            *counts.entry(i.tier.clone()).or_insert(0) += 1;
        }
        counts
    }

    fn by_show(&self) -> indexmap::IndexMap<String, indexmap::IndexMap<String, usize>> {
        let mut out: indexmap::IndexMap<String, indexmap::IndexMap<String, usize>> =
            indexmap::IndexMap::new();
        for i in &self.impacts {
            let tiers = out
                .entry(i.show.clone())
                .or_insert_with(|| TIERS.iter().map(|t| (t.to_string(), 0)).collect());
            *tiers.entry(i.tier.clone()).or_insert(0) += 1;
        }
        out
    }
}

/// Where a cue sits in the mix, from its key.
pub fn classify_placement(cue: &str) -> &'static str {
    let key = cue.to_uppercase();
    let key = key.trim();
    let before_colon = key.split(':').next().unwrap_or("");
    if key.starts_with("MUSIC")
        || key.starts_with("INTRO MUSIC")
        || key.starts_with("OUTRO MUSIC")
        || before_colon.contains(" MUSIC")
    {
        "MUSIC(bg)"
    } else if key.starts_with("AMBIEN") {
        "AMBI(bg)"
    } else if key.starts_with("BEAT") {
        "BEAT(fg)"
    } else {
        "SFX(fg)"
    }
}

fn tier_for(delta_s: f64) -> &'static str {
    if delta_s < NOCHANGE_S {
        "1-nochange"
    } else if delta_s < MINOR_S {
        "2-minor"
    } else {
        "3-review"
    }
}

/// `float(x)` on a config value the way Python would coerce it.
fn as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Duration probe: path → milliseconds, or the Python exception name.
pub type DurationProbe<'a> = &'a dyn Fn(&Path) -> Result<i64, String>;

/// Measure one entry; `None` when it has no `source`.
pub fn measure_cue(
    show: &str,
    episode: &str,
    cue: &str,
    effect: &Map<String, Value>,
    duration_fn: DurationProbe,
    workspace: &Path,
) -> Option<CueImpact> {
    let source = effect.get("source").filter(|v| super::truthy(v))?;
    let source = xil_core::workspace::python_str(source);

    let mut impact = CueImpact {
        show: show.to_string(),
        episode: episode.to_string(),
        cue: cue.to_string(),
        source_file: basename(Path::new(&source)),
        duration_seconds: effect
            .get("duration_seconds")
            .cloned()
            .unwrap_or(Value::Null),
        play_duration: effect.get("play_duration").cloned().unwrap_or(Value::Null),
        loop_: effect.get("loop").is_some_and(super::truthy),
        natural_s: None,
        plays_now_s: None,
        delta_s: None,
        lost_pct: None,
        tier: "EXCLUDED".into(),
        placement: classify_placement(cue).into(),
        remediation: String::new(),
        note: String::new(),
    };

    let mut path = PathBuf::from(&source);
    if !path.is_absolute() {
        path = workspace.join(&source);
    }
    let natural = match duration_fn(&path) {
        Ok(ms) => ms as f64 / 1000.0,
        Err(name) => {
            impact.tier = "MISSING".into();
            impact.note = format!("source file unreadable ({name})");
            return Some(impact);
        }
    };
    if natural <= 0.0 {
        impact.tier = "MISSING".into();
        impact.note = "source file reports zero duration".into();
        return Some(impact);
    }
    impact.natural_s = Some(natural);

    let play_duration = if impact.play_duration.is_null() {
        None
    } else {
        Some(as_float(&impact.play_duration).unwrap_or(0.0))
    };
    let duration_seconds = if impact.duration_seconds.is_null() {
        None
    } else {
        Some(as_float(&impact.duration_seconds).unwrap_or(0.0))
    };

    let plays_now = if impact.loop_ {
        impact.note = "looped bed (fills cue span; not clipped)".into();
        natural
    } else if let Some(pd) = play_duration {
        impact.note = "explicit play_duration kept (takes precedence)".into();
        natural * pd / 100.0
    } else if duration_seconds.map_or(true, |d| d <= 0.0) {
        impact.note = "plays full length (duration_seconds 0 or absent)".into();
        natural
    } else {
        let d = duration_seconds.unwrap_or(0.0);
        let p = d.min(natural);
        impact.tier = tier_for(natural - p).into();
        impact.note = String::new();
        p
    };
    impact.plays_now_s = Some(plays_now);
    let delta = (natural - plays_now).max(0.0);
    impact.delta_s = Some(delta);
    impact.lost_pct = Some(delta / natural * 100.0);
    impact.remediation = if matches!(impact.tier.as_str(), "1-nochange" | "EXCLUDED" | "MISSING") {
        String::new()
    } else {
        "play_duration: 100".into()
    };
    Some(impact)
}

/// Sorted `(show, episode, path)` for every `sfx_<tag>.json` in scope.
pub fn discover_configs(
    workspace: &Path,
    show: Option<&str>,
    episode: Option<&str>,
) -> Vec<(String, String, PathBuf)> {
    let configs_dir = workspace.join("configs");
    if !configs_dir.is_dir() {
        return Vec::new();
    }
    let mut show_dirs: Vec<PathBuf> = fs::read_dir(&configs_dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    sort_py(&mut show_dirs);
    let (prefix, suffix) = match episode {
        Some(tag) => (format!("sfx_{tag}"), ".json"),
        None => ("sfx_".to_string(), ".json"),
    };
    let mut found = Vec::new();
    for show_dir in show_dirs {
        let name = basename(&show_dir);
        if !show_dir.is_dir() || show.is_some_and(|s| s != name) {
            continue;
        }
        let mut paths = pathlib_glob(&show_dir, &prefix, suffix);
        if episode.is_some() {
            // `sfx_<tag>.json` exactly, not `sfx_<tag>x.json`.
            paths.retain(|p| basename(p) == format!("{prefix}{suffix}"));
        }
        for path in paths {
            let stem = basename(&path);
            let stem = stem.strip_suffix(".json").unwrap_or(&stem);
            let tag = &stem["sfx_".len()..];
            if tag.is_empty() {
                continue;
            }
            found.push((name.clone(), tag.to_string(), path));
        }
    }
    found
}

fn mutagen_probe(path: &Path) -> Result<i64, String> {
    xil_audio::mpeg::duration_ms(path).map_err(|e| e.python_name().to_string())
}

pub fn analyze(workspace: &Path, show: Option<&str>, episode: Option<&str>) -> ImpactReport {
    let mut report = ImpactReport::default();
    for (slug, tag, path) in discover_configs(workspace, show, episode) {
        let data: Value = match fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
        {
            Ok(v) => v,
            Err(e) => {
                log::warning(&format!(
                    "  Skipping unreadable config {}: {e}",
                    path.display()
                ));
                continue;
            }
        };
        report.configs_scanned += 1;
        let effects = data
            .get("effects")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (cue, effect) in &effects {
            let Some(effect) = effect.as_object() else {
                continue;
            };
            if let Some(i) = measure_cue(&slug, &tag, cue, effect, &mutagen_probe, workspace) {
                report.impacts.push(i);
            }
        }
    }
    report
}

fn write_csv<W: Write>(report: &ImpactReport, out: &mut W) -> std::io::Result<()> {
    let rows: Vec<Map<String, Value>> = report.impacts.iter().map(CueImpact::row).collect();
    pycsv::write_dicts(out, &CSV_COLUMNS, &rows)
}

fn log_summary(report: &ImpactReport) {
    let tally = report.tally();
    log::info("");
    log::info(&format!(
        "  Scanned {} SFX config(s), {} source-backed cue(s)",
        report.configs_scanned,
        report.impacts.len()
    ));
    log::info("");
    let header = format!(
        "  {} {} {} {} {} {}",
        pad_right("show", 24),
        pad_left("3-review", 9),
        pad_left("2-minor", 8),
        pad_left("1-nochange", 11),
        pad_left("EXCLUDED", 9),
        pad_left("MISSING", 8)
    );
    let rule = format!("  {}", "-".repeat(header.chars().count() - 2));
    let line = |name: &str, t: &indexmap::IndexMap<String, usize>| {
        format!(
            "  {} {} {} {} {} {}",
            pad_right(name, 24),
            pad_left(&t["3-review"].to_string(), 9),
            pad_left(&t["2-minor"].to_string(), 8),
            pad_left(&t["1-nochange"].to_string(), 11),
            pad_left(&t["EXCLUDED"].to_string(), 9),
            pad_left(&t["MISSING"].to_string(), 8)
        )
    };
    log::info(&header);
    log::info(&rule);
    let mut shows: Vec<(String, indexmap::IndexMap<String, usize>)> =
        report.by_show().into_iter().collect();
    shows.sort_by(|a, b| a.0.cmp(&b.0));
    for (slug, tiers) in &shows {
        log::info(&line(slug, tiers));
    }
    log::info(&rule);
    log::info(&line("TOTAL", &tally));

    let actionable = report.actionable();
    if actionable.is_empty() {
        log::info("");
        log::info("  No cues are losing audio — nothing to review.");
        return;
    }
    let lost: f64 = actionable.iter().map(|i| i.delta_s.unwrap_or(0.0)).sum();
    log::info("");
    log::info(&format!(
        "  {} cue(s) lose audio, {}s total",
        actionable.len(),
        fixed(lost, 0)
    ));
    log::info("");
    log::info("  Worst offenders:");
    let mut worst = actionable.clone();
    // Stable sort descending, like `sorted(..., reverse=True)`.
    worst.sort_by(|a, b| {
        b.delta_s
            .unwrap_or(0.0)
            .partial_cmp(&a.delta_s.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for i in worst.iter().take(10) {
        log::info(&format!(
            "    {}s lost  {}/{}  {}  ({}s of {}s)",
            pad_left(&fixed(i.delta_s.unwrap_or(0.0), 1), 6),
            i.show,
            i.episode,
            head(&i.cue, 44),
            fixed(i.plays_now_s.unwrap_or(0.0), 1),
            fixed(i.natural_s.unwrap_or(0.0), 1)
        ));
    }
}

const HTML_CSS: &str = r#"
:root { color-scheme: light dark; }
body { font-family: -apple-system, Segoe UI, Roboto, sans-serif; margin: 0;
       padding: 2rem 1.25rem; line-height: 1.5; background: #fff; color: #1a1a1a; }
h1 { font-size: 1.5rem; margin: 0 0 .25rem; }
.sub { color: #666; margin: 0 0 1.5rem; font-size: .9rem; }
.cards { display: flex; flex-wrap: wrap; gap: .75rem; margin-bottom: 1.75rem; }
.card { border: 1px solid #e0e0e0; border-radius: 8px; padding: .75rem 1rem; min-width: 8rem; }
.card .n { font-size: 1.5rem; font-weight: 600; }
.card .l { font-size: .75rem; color: #666; text-transform: uppercase; letter-spacing: .04em; }
.wrap { overflow-x: auto; }
table { border-collapse: collapse; width: 100%; font-size: .85rem; }
th, td { text-align: left; padding: .4rem .6rem; border-bottom: 1px solid #ececec;
         white-space: nowrap; }
th { position: sticky; top: 0; background: #fafafa; font-weight: 600; }
td.num { text-align: right; font-variant-numeric: tabular-nums; }
td.cue { white-space: normal; min-width: 16rem; }
.t3 { color: #b3261e; font-weight: 600; }
.t2 { color: #9a6700; font-weight: 600; }
.t1, .tE { color: #888; }
.tM { color: #b3261e; }
code { background: #f3f3f3; padding: .1rem .3rem; border-radius: 3px; font-size: .9em; }
@media (prefers-color-scheme: dark) {
  body { background: #14161a; color: #e6e6e6; }
  .card, th, td { border-color: #2c2f36; }
  th { background: #1b1e24; }
  .sub, .card .l, .t1, .tE { color: #9aa0a6; }
  code { background: #23262c; }
  .t3, .tM { color: #ff8a80; }
  .t2 { color: #ffd180; }
}
"#;

fn tier_class(tier: &str) -> &'static str {
    match tier {
        "3-review" => "t3",
        "2-minor" => "t2",
        "1-nochange" => "t1",
        "EXCLUDED" => "tE",
        "MISSING" => "tM",
        _ => "",
    }
}

/// `datetime.now().astimezone().strftime("%Y-%m-%d %H:%M %Z")`.
fn generated_stamp() -> String {
    let now = chrono::Local::now();
    format!("{} {}", now.format("%Y-%m-%d %H:%M"), tz_abbreviation())
}

/// `%Z` as C `strftime` spells it (`EDT`, `UTC`). chrono only knows the
/// numeric offset, so ask `date`, and fall back to the offset.
fn tz_abbreviation() -> String {
    std::process::Command::new("date")
        .arg("+%Z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| chrono::Local::now().format("%:z").to_string())
}

fn render_html(report: &ImpactReport, scope: &str) -> String {
    let tally = report.tally();
    let actionable = report.actionable();
    let lost: f64 = actionable.iter().map(|i| i.delta_s.unwrap_or(0.0)).sum();
    let generated = generated_stamp();

    let cards: Vec<(&str, String)> = vec![
        ("3-review", tally["3-review"].to_string()),
        ("2-minor", tally["2-minor"].to_string()),
        ("1-nochange", tally["1-nochange"].to_string()),
        ("excluded", tally["EXCLUDED"].to_string()),
        ("missing", tally["MISSING"].to_string()),
        ("seconds lost", fixed(lost, 0)),
    ];
    let card_html: String = cards
        .iter()
        .map(|(label, n)| {
            format!(
                "<div class=\"card\"><div class=\"n\">{}</div><div class=\"l\">{}</div></div>",
                html_escape(n),
                html_escape(label)
            )
        })
        .collect();

    let order = |t: &str| match t {
        "3-review" => 0,
        "2-minor" => 1,
        "MISSING" => 2,
        "1-nochange" => 3,
        "EXCLUDED" => 4,
        _ => 9,
    };
    let mut rows: Vec<&CueImpact> = report.impacts.iter().collect();
    rows.sort_by(|a, b| {
        order(&a.tier)
            .cmp(&order(&b.tier))
            .then_with(|| {
                (-b.delta_s.unwrap_or(0.0))
                    .partial_cmp(&(-a.delta_s.unwrap_or(0.0)))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .reverse()
            })
            .then_with(|| a.show.cmp(&b.show))
            .then_with(|| a.episode.cmp(&b.episode))
    });

    let text_cell = |v: &str| {
        if v.is_empty() {
            "<td></td>".to_string()
        } else {
            format!("<td class=\"\">{}</td>", html_escape(v))
        }
    };
    let num_cell = |v: Option<f64>| match v {
        None => "<td class=\"num\"></td>".to_string(),
        Some(x) => format!("<td class=\"num\">{}</td>", html_escape(&fixed(x, 1))),
    };

    let mut body_rows = String::new();
    for i in rows {
        body_rows.push_str("<tr>");
        body_rows.push_str(&text_cell(&i.show));
        body_rows.push_str(&text_cell(&i.episode));
        body_rows.push_str(&format!("<td class=\"cue\">{}</td>", html_escape(&i.cue)));
        body_rows.push_str(&text_cell(&i.source_file));
        body_rows.push_str(&text_cell(&i.placement));
        body_rows.push_str(&num_cell(i.natural_s));
        body_rows.push_str(&num_cell(i.plays_now_s));
        body_rows.push_str(&num_cell(i.delta_s));
        body_rows.push_str(&num_cell(i.lost_pct));
        body_rows.push_str(&format!(
            "<td class=\"{}\">{}</td>",
            tier_class(&i.tier),
            html_escape(&i.tier)
        ));
        if i.remediation.is_empty() {
            body_rows.push_str("<td></td>");
        } else {
            body_rows.push_str(&format!(
                "<td><code>{}</code></td>",
                html_escape(&i.remediation)
            ));
        }
        body_rows.push_str(&format!("<td>{}</td>", html_escape(&i.note)));
        body_rows.push_str("</tr>");
    }

    let headers = [
        "show",
        "episode",
        "cue",
        "source file",
        "placement",
        "natural s",
        "plays now s",
        "lost s",
        "lost %",
        "tier",
        "remediation",
        "note",
    ];
    let head_html: String = headers
        .iter()
        .map(|h| format!("<th>{}</th>", html_escape(h)))
        .collect();

    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>SFX clipping impact — {scope}</title>
<style>{css}</style></head>
<body>
<h1>SFX source-clipping impact</h1>
<p class="sub">{scope} · {configs} config(s) ·
{cues} source-backed cue(s) · generated {generated}</p>
<div class="cards">{card_html}</div>
<p class="sub"><strong>How to read this:</strong> for a <code>source=</code> cue,
<code>duration_seconds</code> clips the file at mix time — the parser writes a default of
<code>5.0</code> into every skeleton entry. <em>Excluded</em> cues are looped beds or cues with a
deliberate <code>play_duration</code>, which are never clipped by <code>duration_seconds</code>.
Nothing here has been changed; the remediation column is the edit that would restore full length.</p>
<p class="sub"><strong>Why <code>play_duration: 100</code> and not <code>duration_seconds: 0</code>?</strong>
Both play the whole file, but only <code>play_duration</code> is replayed by the timeline edit
journal. A <code>duration_seconds</code> edit is silently reset to <code>5.0</code> the next time the
config is rebuilt from a skeleton — re-clipping a cue that was already approved.</p>
<div class="wrap"><table><thead><tr>{head_html}</tr></thead>
<tbody>{body_rows}</tbody></table></div>
</body></html>
"#,
        scope = html_escape(scope),
        css = HTML_CSS,
        configs = report.configs_scanned,
        cues = report.impacts.len(),
        generated = html_escape(&generated),
    )
}

fn filter_tier(report: ImpactReport, tier: Option<&str>) -> ImpactReport {
    let Some(tier) = tier else {
        return report;
    };
    let wanted: &[&str] = if tier == "actionable" {
        &["2-minor", "3-review"]
    } else {
        std::slice::from_ref(&tier)
    };
    ImpactReport {
        impacts: report
            .impacts
            .into_iter()
            .filter(|i| wanted.contains(&i.tier.as_str()))
            .collect(),
        configs_scanned: report.configs_scanned,
    }
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let workspace = workspace_root();
    // An explicit --episode with no --show still means "this show".
    let show = match (&a.show, &a.episode) {
        (None, Some(_)) => Some(resolve_slug(None, "project.json")),
        (s, _) => s.clone(),
    };
    let to_stdout = a.output.as_deref() == Some("-");

    let report = analyze(&workspace, show.as_deref(), a.episode.as_deref());
    if report.configs_scanned == 0 {
        let scope = match &show {
            Some(s) => format!("show={s}"),
            None => "workspace".to_string(),
        };
        let msg = format!(
            "No SFX configs found ({scope}) under {}",
            workspace.join("configs").display()
        );
        if to_stdout {
            eprintln!("{msg}");
        } else {
            log::error(&msg);
        }
        return Ok(1);
    }

    let report = filter_tier(report, a.tier.as_deref());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();

    if to_stdout {
        let mut out = std::io::stdout().lock();
        write_csv(&report, &mut out)?;
        out.flush()?;
    } else {
        let out_path = match &a.output {
            Some(p) => PathBuf::from(p),
            None => workspace
                .join("reports")
                .join(format!("sfx_impact_{date}.csv")),
        };
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&out_path)?;
        write_csv(&report, &mut f)?;
        log::info(&format!(
            "  Wrote {} ({} row(s))",
            out_path.display(),
            report.impacts.len()
        ));
    }

    if let Some(html) = &a.html {
        let html_path = if html.is_empty() {
            workspace
                .join("reports")
                .join(format!("sfx_impact_{date}.html"))
        } else {
            PathBuf::from(html)
        };
        if let Some(parent) = html_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut scope = show.clone().unwrap_or_else(|| "all shows".to_string());
        if let Some(ep) = &a.episode {
            scope = format!("{scope} · {ep}");
        }
        fs::write(&html_path, render_html(&report, &scope))?;
        if to_stdout {
            eprintln!("Wrote {}", html_path.display());
        } else {
            log::info(&format!("  Wrote {}", html_path.display()));
        }
    }

    if !a.quiet && !to_stdout {
        log_summary(&report);
    }
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("sfx-impact");
    let a: Args = match super::parse_or_exit("xil-sfx-impact", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    // `--output -` makes stdout the CSV stream; the banner would corrupt it.
    if a.output.as_deref() == Some("-") {
        execute(&a)
    } else {
        let _banner = banner::begin(SCRIPT_NAME, &super::argv_line(args));
        execute(&a)
    }
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn probe_ms(ms: i64) -> impl Fn(&Path) -> Result<i64, String> {
        move |_| Ok(ms)
    }

    fn measure(effect: Value, ms: i64) -> CueImpact {
        let f = probe_ms(ms);
        measure_cue(
            "s",
            "E1",
            "SFX: X",
            effect.as_object().unwrap(),
            &f,
            Path::new("/ws"),
        )
        .unwrap()
    }

    #[test]
    fn placement_buckets() {
        assert_eq!(classify_placement("OUTRO MUSIC"), "MUSIC(bg)");
        assert_eq!(classify_placement("MUSIC: THEME"), "MUSIC(bg)");
        assert_eq!(classify_placement("sting music: x"), "MUSIC(bg)");
        assert_eq!(classify_placement("AMBIENCE: RAIN"), "AMBI(bg)");
        assert_eq!(classify_placement("BEAT"), "BEAT(fg)");
        assert_eq!(classify_placement("SFX: DOOR: MUSIC ROOM"), "SFX(fg)");
    }

    #[test]
    fn precedence_mirrors_the_mixer() {
        let looped = measure(
            json!({"source": "a.mp3", "loop": true, "duration_seconds": 5.0}),
            20000,
        );
        assert_eq!(looped.tier, "EXCLUDED");
        assert_eq!(looped.delta_s, Some(0.0));

        let pd = measure(
            json!({"source": "a.mp3", "play_duration": 50, "duration_seconds": 5.0}),
            20000,
        );
        assert_eq!(pd.tier, "EXCLUDED");
        assert_eq!(pd.plays_now_s, Some(10.0));

        let clipped = measure(json!({"source": "a.mp3", "duration_seconds": 5.0}), 20000);
        assert_eq!(clipped.tier, "3-review");
        assert_eq!(clipped.remediation, "play_duration: 100");
        assert_eq!(clipped.lost_pct, Some(75.0));

        let short = measure(json!({"source": "a.mp3", "duration_seconds": 5.0}), 1500);
        assert_eq!(short.tier, "1-nochange");
        assert_eq!(short.delta_s, Some(0.0));

        let zero = measure(json!({"source": "a.mp3", "duration_seconds": 0}), 1500);
        assert_eq!(zero.tier, "EXCLUDED");
        assert!(zero.note.starts_with("plays full length"));

        let f = |_: &Path| Err("HeaderNotFoundError".to_string());
        let missing = measure_cue(
            "s",
            "E1",
            "SFX: X",
            json!({"source": "a.mp3"}).as_object().unwrap(),
            &f,
            Path::new("/ws"),
        )
        .unwrap();
        assert_eq!(missing.tier, "MISSING");
        assert_eq!(missing.note, "source file unreadable (HeaderNotFoundError)");

        let none = measure_cue(
            "s",
            "E1",
            "BEAT",
            json!({"type": "silence"}).as_object().unwrap(),
            &probe_ms(1),
            Path::new("/ws"),
        );
        assert!(none.is_none());
    }

    #[test]
    fn csv_row_rounds_and_blanks() {
        let i = measure(json!({"source": "x/a.mp3", "duration_seconds": 5.0}), 12345);
        let r = i.row();
        assert_eq!(pycsv::cell(&r["natural_s"]), "12.3");
        assert_eq!(pycsv::cell(&r["delta_s"]), "7.3");
        assert_eq!(pycsv::cell(&r["lost_pct"]), "59.5");
        assert_eq!(pycsv::cell(&r["play_duration"]), "");
        assert_eq!(pycsv::cell(&r["loop"]), "False");
        assert_eq!(pycsv::cell(&r["duration_seconds"]), "5.0");
        assert_eq!(r["source_file"], "a.mp3");
    }
}

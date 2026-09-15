//! `xil sfx-match` — find existing library assets for cues whose declared
//! source is not on disk. Port of `XILU022_sfx_match.py`.
//!
//! Each missing cue is scored against every pool asset's title (else its
//! prompt, else its filename) on coverage (`|cue ∩ cand| / |cue|`), with
//! jaccard as the tie-breaker. A category gate keeps AMBIENCE cues off
//! MUSIC assets, and a margin rule sends near-ties to human review.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use clap::Parser;
use regex::Regex;
use serde_json::{Map, Value};
use xil_core::fsutil::{abspath, basename, relpath};
use xil_core::journal::append_sfx_edit;
use xil_core::pycsv;
use xil_core::pyfmt::{fixed, head, pad_right, round_to};
use xil_core::pyjson::{dumps, py_float, Style};
use xil_core::sfxlib::{shared_sfx_path, slugify_effect_key};
use xil_core::workspace::{resolve_slug, workspace_root};
use xil_core::{banner, log};

use super::sfx_impact::{classify_placement, discover_configs};
use super::sfx_lib::fetch_local_records;

const SCRIPT_NAME: &str = "XILU022_sfx_match";

const TIERS: [&str; 4] = ["EXACT", "STRONG", "REVIEW", "NONE"];

/// Tokens with no discriminating signal. "up", "out", "down", "off" and
/// "back" are deliberately kept: cue text leans on them.
const STOPWORDS: [&str; 32] = [
    "sfx",
    "ambience",
    "ambient",
    "music",
    "beat",
    "new",
    "stem",
    "needed",
    "mp3",
    "wav",
    "elevenlabs",
    "a",
    "an",
    "and",
    "as",
    "at",
    "by",
    "for",
    "from",
    "in",
    "into",
    "it",
    "its",
    "of",
    "on",
    "or",
    "s",
    "the",
    "then",
    "to",
    "with",
    "",
];

const REVIEW_FLOOR: f64 = 0.5;
const STRONG_COVERAGE: f64 = 0.8;
const STRONG_JACCARD: f64 = 0.4;
const STRONG_MARGIN: f64 = 0.15;
const JACCARD_WEIGHT: f64 = 0.25;

const CSV_COLUMNS: [&str; 14] = [
    "show",
    "episode",
    "cue",
    "current_source",
    "tier",
    "rank",
    "candidate",
    "candidate_title",
    "candidate_scope",
    "coverage",
    "jaccard",
    "score",
    "duration_s",
    "also_in",
];

static TOKEN_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());

#[derive(Parser)]
#[command(
    name = "xil-sfx-match",
    about = "Find existing SFX library assets for cues whose declared source is not on disk — the cues that make 'xil produce' refuse to start. Reports by default; --apply copies the match into the show's pool and journals the new source."
)]
struct Args {
    /// Restrict to one show slug (default: every show in the workspace)
    #[arg(long)]
    show: Option<String>,
    /// Restrict to one episode tag (e.g. S01E01)
    #[arg(long, alias = "tag")]
    episode: Option<String>,
    /// Candidates to report per cue (default: 3)
    #[arg(long, default_value_t = 3)]
    top: usize,
    /// Coverage a candidate must reach to be reported (default: 0.5)
    #[arg(long, default_value_t = REVIEW_FLOOR)]
    min_coverage: f64,
    /// CSV output path, or '-' for stdout (default: reports/sfx_match_<date>.csv)
    #[arg(long)]
    output: Option<String>,
    /// Also write a paste-ready pipe-hint block (default path: reports/sfx_match_hints_<date>.md)
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
    emit_hints: Option<String>,
    /// Copy each accepted match into the show's SFX pool under the cue's own slug, retitle the copy, and journal the new source. Acts on EXACT and STRONG only unless --accept-review is given.
    #[arg(long)]
    apply: bool,
    /// Widen --apply to REVIEW matches. Read the CSV first — a REVIEW tier means the top candidate had no clear margin over its runners-up.
    #[arg(long)]
    accept_review: bool,
    /// With --apply, report every copy and journal record without writing
    #[arg(long)]
    dry_run: bool,
    /// Suppress the console summary (CSV only)
    #[arg(long)]
    quiet: bool,
}

// ── text handling ──────────────────────────────────────────────────────────

/// Fold a simple plural so CHAIRS matches CHAIR (`glass` is protected).
fn singular(token: &str) -> String {
    if token.chars().count() >= 4 && token.ends_with('s') && !token.ends_with("ss") {
        token[..token.len() - 1].to_string()
    } else {
        token.to_string()
    }
}

/// Lowercase, stopword-stripped, singularised token set.
pub fn tokenize(text: &str) -> BTreeSet<String> {
    let lower = text.to_lowercase();
    TOKEN_SPLIT
        .split(&lower)
        .filter(|t| !STOPWORDS.contains(t))
        .map(singular)
        .collect()
}

/// `(coverage, jaccard)` of a cue against a candidate.
fn score_pair(cue: &BTreeSet<String>, cand: &BTreeSet<String>) -> (f64, f64) {
    if cue.is_empty() || cand.is_empty() {
        return (0.0, 0.0);
    }
    let shared = cue.intersection(cand).count() as f64;
    let union = cue.union(cand).count() as f64;
    (shared / cue.len() as f64, shared / union)
}

fn composite(coverage: f64, jaccard: f64) -> f64 {
    coverage + JACCARD_WEIGHT * jaccard
}

/// `f[3].isalpha()` on the lowercased filename.
fn fourth_is_alpha(f: &str) -> bool {
    f.chars().nth(3).is_some_and(char::is_alphabetic)
}

/// Classify an untitled asset by filename prefix.
fn placement_from_filename(filename: &str) -> &'static str {
    let f = filename.to_lowercase();
    if ["ambience_", "ambience-", "amb-", "amb_"]
        .iter()
        .any(|p| f.starts_with(p))
        || (f.starts_with("amb") && fourth_is_alpha(&f))
    {
        return "AMBI(bg)";
    }
    if ["music_", "music-", "mus-", "mus_"]
        .iter()
        .any(|p| f.starts_with(p))
        || (f.starts_with("mus") && fourth_is_alpha(&f))
    {
        return "MUSIC(bg)";
    }
    if f.starts_with("beat") {
        return "BEAT(fg)";
    }
    "SFX(fg)"
}

// ── pool ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PoolAsset {
    filename: String,
    path: String,
    /// Show slug, or "" for the flat SFX/ root.
    scope: String,
    title: String,
    prompt: String,
    duration_s: Option<f64>,
    placement: &'static str,
    tokens: BTreeSet<String>,
    also_in: Vec<String>,
}

impl PoolAsset {
    /// Title, else prompt, else filename.
    fn match_text(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if !self.prompt.is_empty() {
            &self.prompt
        } else {
            &self.filename
        }
    }
}

fn scope_rank(scope: &str, own_show: Option<&str>) -> u8 {
    if own_show.is_some_and(|s| s == scope) {
        0
    } else if scope.is_empty() {
        1
    } else {
        2
    }
}

/// The filename still agrees with the title it carries.
fn is_canonical_name(a: &PoolAsset) -> bool {
    !a.title.is_empty() && a.filename == format!("{}.mp3", slugify_effect_key(&a.title))
}

fn asset_from_record(rec: &Map<String, Value>) -> PoolAsset {
    let s = |k: &str| rec.get(k).and_then(Value::as_str).unwrap_or("");
    let title = s("title").trim().to_string();
    let filename = s("filename").to_string();
    let placement = if title.is_empty() {
        placement_from_filename(&filename)
    } else {
        classify_placement(&title)
    };
    let mut a = PoolAsset {
        filename,
        path: s("path").to_string(),
        scope: s("show").to_string(),
        title,
        prompt: s("prompt").trim().to_string(),
        duration_s: rec.get("duration_seconds").and_then(Value::as_f64),
        placement,
        tokens: BTreeSet::new(),
        also_in: Vec::new(),
    };
    a.tokens = tokenize(a.match_text());
    a
}

fn load_assets(workspace: &Path) -> Vec<PoolAsset> {
    fetch_local_records(&workspace.join("SFX"))
        .iter()
        .map(asset_from_record)
        .collect()
}

/// One entry per distinct sound, grouped by `(match tokens, duration)`.
fn build_pool(assets: &[PoolAsset], own_show: Option<&str>) -> Vec<PoolAsset> {
    let mut groups: indexmap::IndexMap<(String, Option<u64>), Vec<&PoolAsset>> =
        indexmap::IndexMap::new();
    for a in assets {
        let key = (
            a.tokens.iter().cloned().collect::<Vec<_>>().join(" "),
            a.duration_s.map(f64::to_bits),
        );
        groups.entry(key).or_default().push(a);
    }
    let mut pool = Vec::new();
    for members in groups.values_mut() {
        members.sort_by(|x, y| {
            (
                scope_rank(&x.scope, own_show),
                !is_canonical_name(x),
                x.filename.chars().count(),
                &x.filename,
            )
                .cmp(&(
                    scope_rank(&y.scope, own_show),
                    !is_canonical_name(y),
                    y.filename.chars().count(),
                    &y.filename,
                ))
        });
        let also_in: BTreeSet<String> = members[1..]
            .iter()
            .map(|m| {
                if m.scope.is_empty() {
                    "SFX/".to_string()
                } else {
                    m.scope.clone()
                }
            })
            .collect();
        let mut winner = members[0].clone();
        winner.also_in = also_in.into_iter().collect();
        pool.push(winner);
    }
    pool
}

/// filename → preferred copy, over every asset rather than the deduped pool.
fn build_exact_index(
    assets: &[PoolAsset],
    own_show: Option<&str>,
) -> indexmap::IndexMap<String, PoolAsset> {
    let mut index: indexmap::IndexMap<String, PoolAsset> = indexmap::IndexMap::new();
    for a in assets {
        let replace = match index.get(&a.filename) {
            None => true,
            Some(cur) => scope_rank(&a.scope, own_show) < scope_rank(&cur.scope, own_show),
        };
        if replace {
            index.insert(a.filename.clone(), a.clone());
        }
    }
    index
}

// ── matching ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Candidate {
    asset: PoolAsset,
    coverage: f64,
    jaccard: f64,
}

impl Candidate {
    fn score(&self) -> f64 {
        composite(self.coverage, self.jaccard)
    }
}

struct CueMatch {
    show: String,
    episode: String,
    cue: String,
    current_source: String,
    config_path: PathBuf,
    tier: &'static str,
    candidates: Vec<Candidate>,
}

impl CueMatch {
    fn best(&self) -> Option<&Candidate> {
        self.candidates.first()
    }

    fn rows(&self) -> Vec<Map<String, Value>> {
        let base = || {
            let mut m = Map::new();
            m.insert("show".into(), self.show.clone().into());
            m.insert("episode".into(), self.episode.clone().into());
            m.insert("cue".into(), self.cue.clone().into());
            m.insert("current_source".into(), self.current_source.clone().into());
            m.insert("tier".into(), self.tier.into());
            m
        };
        if self.candidates.is_empty() {
            let mut m = base();
            for c in &CSV_COLUMNS[5..] {
                m.insert((*c).into(), "".into());
            }
            return vec![m];
        }
        self.candidates
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut m = base();
                m.insert("rank".into(), Value::from(i + 1));
                m.insert("candidate".into(), c.asset.filename.clone().into());
                m.insert("candidate_title".into(), c.asset.match_text().into());
                m.insert(
                    "candidate_scope".into(),
                    if c.asset.scope.is_empty() {
                        "SFX/".into()
                    } else {
                        c.asset.scope.clone().into()
                    },
                );
                m.insert("coverage".into(), py_float(round_to(c.coverage, 2)));
                m.insert("jaccard".into(), py_float(round_to(c.jaccard, 2)));
                m.insert("score".into(), py_float(round_to(c.score(), 3)));
                m.insert(
                    "duration_s".into(),
                    c.asset.duration_s.map_or("".into(), py_float),
                );
                m.insert("also_in".into(), c.asset.also_in.join(", ").into());
                m
            })
            .collect()
    }
}

fn match_cue(
    cue: &str,
    pool: &[PoolAsset],
    exact_index: &indexmap::IndexMap<String, PoolAsset>,
    top: usize,
    min_coverage: f64,
) -> (&'static str, Vec<Candidate>) {
    let exact_name = format!("{}.mp3", slugify_effect_key(cue));
    let hit = exact_index
        .get(&exact_name)
        .or_else(|| pool.iter().find(|a| a.filename == exact_name));
    if let Some(hit) = hit {
        return (
            "EXACT",
            vec![Candidate {
                asset: hit.clone(),
                coverage: 1.0,
                jaccard: 1.0,
            }],
        );
    }

    let cue_tokens = tokenize(cue);
    let wanted = classify_placement(cue);
    let mut scored: Vec<Candidate> = pool
        .iter()
        .filter(|a| a.placement == wanted)
        .filter_map(|a| {
            let (cov, jac) = score_pair(&cue_tokens, &a.tokens);
            (cov >= min_coverage).then(|| Candidate {
                asset: a.clone(),
                coverage: cov,
                jaccard: jac,
            })
        })
        .collect();
    scored.sort_by(|x, y| {
        y.score()
            .partial_cmp(&x.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.asset.filename.cmp(&y.asset.filename))
    });
    scored.truncate(top);

    let Some(leader) = scored.first() else {
        return ("NONE", scored);
    };
    let margin = leader.score() - scored.get(1).map_or(0.0, Candidate::score);
    let tier = if leader.coverage >= STRONG_COVERAGE
        && leader.jaccard >= STRONG_JACCARD
        && margin >= STRONG_MARGIN
    {
        "STRONG"
    } else {
        "REVIEW"
    };
    (tier, scored)
}

fn source_missing(source: &str, workspace: &Path) -> bool {
    if source.is_empty() {
        return false;
    }
    let p = Path::new(source);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(source)
    };
    !p.is_file()
}

fn analyze(
    workspace: &Path,
    show: Option<&str>,
    episode: Option<&str>,
    top: usize,
    min_coverage: f64,
) -> (Vec<CueMatch>, usize) {
    let configs = discover_configs(workspace, show, episode);
    let mut pools: indexmap::IndexMap<String, Vec<PoolAsset>> = indexmap::IndexMap::new();
    let mut indexes: indexmap::IndexMap<String, indexmap::IndexMap<String, PoolAsset>> =
        indexmap::IndexMap::new();
    let mut assets: Option<Vec<PoolAsset>> = None;
    let mut matches = Vec::new();

    for (slug, tag, path) in &configs {
        let data: Value = match fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
        {
            Ok(v) => v,
            Err(e) => {
                log::warning(&format!("  Skipping {} — {e}", path.display()));
                continue;
            }
        };
        let effects = data
            .get("effects")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (cue, effect) in &effects {
            let source = effect
                .get("source")
                .filter(|v| super::truthy(v))
                .map(xil_core::workspace::python_str)
                .unwrap_or_default();
            if !source_missing(&source, workspace) {
                continue;
            }
            let all = assets.get_or_insert_with(|| {
                // Deferred so a workspace where every source resolves never
                // pays for a full library scan.
                log::info("  Indexing the SFX library…");
                let a = load_assets(workspace);
                log::info(&format!("  Indexed {} asset(s)", a.len()));
                a
            });
            if !pools.contains_key(slug) {
                pools.insert(slug.clone(), build_pool(all, Some(slug)));
                indexes.insert(slug.clone(), build_exact_index(all, Some(slug)));
            }
            let (tier, candidates) =
                match_cue(cue, &pools[slug], &indexes[slug], top, min_coverage);
            matches.push(CueMatch {
                show: slug.clone(),
                episode: tag.clone(),
                cue: cue.clone(),
                current_source: source,
                config_path: path.clone(),
                tier,
                candidates,
            });
        }
    }
    (matches, configs.len())
}

// ── repair ─────────────────────────────────────────────────────────────────

/// `sfx_common.sfx_dir`: the per-show pool when it exists, else flat SFX/.
fn sfx_dir(slug: &str) -> PathBuf {
    let root = workspace_root();
    let per_show = root.join("SFX").join(slug);
    if per_show.is_dir() {
        per_show
    } else {
        root.join("SFX")
    }
}

fn write_config_source(config_path: &Path, cue: &str, source: &str) -> anyhow::Result<()> {
    let mut data: Value = serde_json::from_str(&fs::read_to_string(config_path)?)?;
    let obj = data
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config is not an object"))?;
    let effects = obj
        .entry("effects")
        .or_insert_with(|| Value::Object(Map::new()));
    let entry = effects
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("effects is not an object"))?
        .entry(cue.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(e) = entry.as_object_mut() {
        e.insert("source".into(), source.into());
    }
    fs::write(config_path, dumps(&data, Style::INDENT2_UTF8) + "\n")?;
    Ok(())
}

/// `shutil.copy2`: contents plus the modification time.
fn copy2(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::copy(src, dst)?;
    if let Ok(m) = fs::metadata(src).and_then(|m| m.modified()) {
        let f = fs::File::options().write(true).open(dst)?;
        f.set_modified(m)?;
    }
    Ok(())
}

fn apply_match(m: &CueMatch, workspace: &Path, dry_run: bool) -> anyhow::Result<()> {
    let Some(best) = m.best() else {
        return Ok(());
    };
    let dest = shared_sfx_path(&sfx_dir(&m.show), &m.cue, "elevenlabs");
    let rel_dest = relpath(&dest, workspace).to_string_lossy().into_owned();
    let src = PathBuf::from(&best.asset.path);
    let rel_src = relpath(&src, workspace).to_string_lossy().into_owned();

    let action = if abspath(&dest) == abspath(&src) {
        "already in place".to_string()
    } else if dest.exists() {
        "destination exists, reusing".to_string()
    } else {
        if !dry_run {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            copy2(&src, &dest)?;
            xil_audio::tags::retag_title_comment(
                &dest,
                &m.cue,
                &format!("copied from {rel_src} by xil sfx-match"),
            )?;
        }
        format!("copy {rel_src}")
    };

    log::info(&format!(
        "    {}{}  →  {}  ({})",
        if dry_run { "[dry-run] " } else { "" },
        head(&m.cue, 44),
        rel_dest,
        action
    ));

    if !dry_run {
        write_config_source(&m.config_path, &m.cue, &rel_dest)?;
        let mut fields = Map::new();
        fields.insert("source".into(), rel_dest.clone().into());
        append_sfx_edit(&m.config_path, &m.cue, &fields)?;
    }
    Ok(())
}

// ── output ─────────────────────────────────────────────────────────────────

fn write_csv<W: Write>(matches: &[CueMatch], out: &mut W) -> std::io::Result<()> {
    let rows: Vec<Map<String, Value>> = matches.iter().flat_map(CueMatch::rows).collect();
    pycsv::write_dicts(out, &CSV_COLUMNS, &rows)
}

fn render_hints(matches: &[CueMatch], workspace: &Path) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d");
    let mut out: Vec<String> = vec![
        "# SFX Pipe-Hint Repairs".into(),
        format!("# Generated {today} by xil sfx-match"),
        "# Replace the matching direction line in the script with the [CUE | file] line.".into(),
        "# Alternates are listed beneath each match — swap in whichever is right.".into(),
        String::new(),
    ];
    for tier in ["EXACT", "STRONG", "REVIEW"] {
        let rows: Vec<&CueMatch> = matches.iter().filter(|m| m.tier == tier).collect();
        if rows.is_empty() {
            continue;
        }
        out.push(format!("## {tier} ({})", rows.len()));
        out.push(String::new());
        for m in rows {
            let dest = basename(&shared_sfx_path(&sfx_dir(&m.show), &m.cue, "elevenlabs"));
            out.push(format!("[{} | {dest}]", m.cue));
            for c in &m.candidates {
                let rel = relpath(Path::new(&c.asset.path), workspace)
                    .to_string_lossy()
                    .into_owned();
                out.push(format!(
                    "#   cov {}  “{}”",
                    fixed(c.coverage, 2),
                    c.asset.match_text()
                ));
                out.push(format!("#     {rel}"));
            }
            out.push(String::new());
        }
    }
    let none_rows: Vec<&CueMatch> = matches.iter().filter(|m| m.tier == "NONE").collect();
    if !none_rows.is_empty() {
        out.push(format!(
            "## NONE — no existing asset, needs generation ({})",
            none_rows.len()
        ));
        out.push(String::new());
        out.extend(
            none_rows
                .iter()
                .map(|m| format!("# {}/{}  {}", m.show, m.episode, m.cue)),
        );
        out.push(String::new());
    }
    out.join("\n")
}

fn log_summary(matches: &[CueMatch], configs_scanned: usize) {
    let mut by_show: indexmap::IndexMap<&str, indexmap::IndexMap<&str, usize>> =
        indexmap::IndexMap::new();
    for m in matches {
        *by_show
            .entry(m.show.as_str())
            .or_insert_with(|| TIERS.iter().map(|t| (*t, 0)).collect())
            .entry(m.tier)
            .or_insert(0) += 1;
    }

    log::info("");
    log::info(&format!(
        "  Scanned {configs_scanned} SFX config(s), {} cue(s) with an unresolvable source",
        matches.len()
    ));
    if matches.is_empty() {
        log::info("");
        log::info("  Every declared source resolves — nothing to match.");
        return;
    }

    log::info("");
    let row = |name: &str, t: &indexmap::IndexMap<&str, usize>| {
        format!(
            "  {} {:>7} {:>7} {:>7} {:>7}",
            pad_right(name, 24),
            t["EXACT"],
            t["STRONG"],
            t["REVIEW"],
            t["NONE"]
        )
    };
    let header = format!(
        "  {} {:>7} {:>7} {:>7} {:>7}",
        pad_right("show", 24),
        "EXACT",
        "STRONG",
        "REVIEW",
        "NONE"
    );
    let rule = format!("  {}", "-".repeat(header.chars().count() - 2));
    log::info(&header);
    log::info(&rule);
    let mut totals: indexmap::IndexMap<&str, usize> = TIERS.iter().map(|t| (*t, 0)).collect();
    let mut shows: Vec<_> = by_show.iter().collect();
    shows.sort_by(|a, b| a.0.cmp(b.0));
    for (slug, tiers) in shows {
        log::info(&row(slug, tiers));
        for t in TIERS {
            totals[t] += tiers[t];
        }
    }
    log::info(&rule);
    log::info(&row("TOTAL", &totals));

    let review: Vec<&CueMatch> = matches.iter().filter(|m| m.tier == "REVIEW").collect();
    if !review.is_empty() {
        log::info("");
        log::info(&format!("  {} cue(s) need a human decision:", review.len()));
        for m in review.iter().take(15) {
            let Some(best) = m.best() else { continue };
            log::info(&format!(
                "    {}  cov {}  “{}”",
                pad_right(&head(&m.cue, 42), 42),
                fixed(best.coverage, 2),
                head(best.asset.match_text(), 52)
            ));
        }
        if review.len() > 15 {
            log::info(&format!(
                "    … and {} more — see the CSV",
                review.len() - 15
            ));
        }
    }

    let nothing = matches.iter().filter(|m| m.tier == "NONE").count();
    if nothing > 0 {
        log::info("");
        log::info(&format!(
            "  {nothing} cue(s) have no existing asset and need generation."
        ));
    }
}

// ── CLI ────────────────────────────────────────────────────────────────────

fn execute(a: &Args) -> anyhow::Result<i32> {
    let workspace = workspace_root();
    let show = match (&a.show, &a.episode) {
        (None, Some(_)) => Some(resolve_slug(None, "project.json")),
        (s, _) => s.clone(),
    };
    let to_stdout = a.output.as_deref() == Some("-");
    let (matches, configs_scanned) = analyze(
        &workspace,
        show.as_deref(),
        a.episode.as_deref(),
        a.top,
        a.min_coverage,
    );

    if configs_scanned == 0 {
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

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    if to_stdout {
        let mut out = std::io::stdout().lock();
        write_csv(&matches, &mut out)?;
        out.flush()?;
    } else {
        let out_path = match &a.output {
            Some(p) => PathBuf::from(p),
            None => workspace
                .join("reports")
                .join(format!("sfx_match_{date}.csv")),
        };
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&out_path)?;
        write_csv(&matches, &mut f)?;
        log::info(&format!(
            "  Wrote {} ({} cue(s))",
            out_path.display(),
            matches.len()
        ));
    }

    if let Some(hints) = &a.emit_hints {
        let hints_path = if hints.is_empty() {
            workspace
                .join("reports")
                .join(format!("sfx_match_hints_{date}.md"))
        } else {
            PathBuf::from(hints)
        };
        if let Some(parent) = hints_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&hints_path, render_hints(&matches, &workspace))?;
        let msg = format!("Wrote {}", hints_path.display());
        if to_stdout {
            eprintln!("{msg}");
        } else {
            log::info(&format!("  {msg}"));
        }
    }

    if a.apply {
        let accepted: &[&str] = if a.accept_review {
            &["EXACT", "STRONG", "REVIEW"]
        } else {
            &["EXACT", "STRONG"]
        };
        let todo: Vec<&CueMatch> = matches
            .iter()
            .filter(|m| accepted.contains(&m.tier) && m.best().is_some())
            .collect();
        log::info("");
        log::info(&format!(
            "  {}Applying {} match(es) [{}]:",
            if a.dry_run { "[dry-run] " } else { "" },
            todo.len(),
            accepted.join(", ")
        ));
        for m in &todo {
            apply_match(m, &workspace, a.dry_run)?;
        }
        if a.dry_run && !todo.is_empty() {
            log::info("  Re-run without --dry-run to apply.");
        }
    }

    if !a.quiet && !to_stdout {
        log_summary(&matches, configs_scanned);
    }
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("sfx-match");
    let a: Args = match super::parse_or_exit("xil-sfx-match", args) {
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

    fn asset(filename: &str, scope: &str, title: &str, dur: Option<f64>) -> PoolAsset {
        let mut rec = Map::new();
        rec.insert("filename".into(), filename.into());
        rec.insert("path".into(), format!("/ws/SFX/{scope}/{filename}").into());
        rec.insert("show".into(), scope.into());
        rec.insert("title".into(), title.into());
        rec.insert("prompt".into(), "".into());
        rec.insert("duration_seconds".into(), dur.map_or(Value::Null, py_float));
        asset_from_record(&rec)
    }

    #[test]
    fn tokens_drop_stopwords_and_fold_plurals() {
        let t: Vec<String> = tokenize("SFX: CHAIRS — the Glass, KEYS up")
            .into_iter()
            .collect();
        assert_eq!(t, vec!["chair", "glass", "key", "up"]);
        assert!(tokenize("NEW STEM NEEDED: x.mp3").contains("x"));
    }

    #[test]
    fn filename_placement_matches_export_kit_buckets() {
        assert_eq!(placement_from_filename("AMBGras-x.mp3"), "AMBI(bg)");
        assert_eq!(placement_from_filename("amb1.mp3"), "SFX(fg)");
        assert_eq!(placement_from_filename("MUSFolk-theme.mp3"), "MUSIC(bg)");
        assert_eq!(placement_from_filename("beat_x.mp3"), "BEAT(fg)");
    }

    #[test]
    fn pool_dedupes_by_tokens_and_duration_preferring_own_show() {
        let assets = vec![
            asset("sfx_door.mp3", "", "SFX: DOOR", Some(1.0)),
            asset("sfx_door.mp3", "myshow", "SFX: DOOR", Some(1.0)),
            asset("SFX-_DOORS.mp3", "other", "SFX: DOORS", Some(1.0)),
            asset("sfx_door.mp3", "", "SFX: DOOR", Some(2.0)),
        ];
        let pool = build_pool(&assets, Some("myshow"));
        assert_eq!(pool.len(), 2);
        assert_eq!(pool[0].scope, "myshow");
        assert_eq!(pool[0].also_in, vec!["SFX/", "other"]);
        let idx = build_exact_index(&assets, Some("myshow"));
        assert_eq!(idx["sfx_door.mp3"].scope, "myshow");
    }

    #[test]
    fn tiers_follow_thresholds_and_margin() {
        let assets = vec![
            asset("sfx_phone-buzz.mp3", "", "SFX: PHONE BUZZ", Some(1.0)),
            asset(
                "sfx_phone-buzz-twice.mp3",
                "",
                "SFX: PHONE BUZZ TWICE",
                Some(2.0),
            ),
            asset("a.mp3", "", "AMBIENCE: HEAVY RAIN ON A WINDOW", Some(2.0)),
        ];
        let pool = build_pool(&assets, None);
        let idx = build_exact_index(&assets, None);
        assert_eq!(match_cue("SFX: PHONE BUZZ", &pool, &idx, 3, 0.5).0, "EXACT");
        let (t, c) = match_cue("SFX: PHONE BUZZ LOUD", &pool, &idx, 3, 0.5);
        assert_eq!(t, "REVIEW");
        assert_eq!(c[0].asset.filename, "sfx_phone-buzz.mp3");
        assert_eq!(
            match_cue("AMBIENCE: RAIN ON WINDOW", &pool, &idx, 3, 0.5).0,
            "STRONG"
        );
        assert_eq!(
            match_cue("MUSIC: RAIN ON WINDOW", &pool, &idx, 3, 0.5).0,
            "NONE"
        );
    }
}

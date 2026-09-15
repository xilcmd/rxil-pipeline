//! `xil scan` — pre-flight check of a production script. Port of
//! `XILP000_script_scanner.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use clap::Parser;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;
use xil_core::fsutil::{basename, glob_recursive};
use xil_core::pyjson::{dumps, Style};
use xil_core::script::hints::parse_direction_hint;
use xil_core::script::parse_script_header;
use xil_core::script::sections::get_section_map;
use xil_core::script::speakers::{
    display_to_key, extract_cast_from_script, load_speakers, try_match_speaker, Speakers,
};
use xil_core::script::text::{
    is_divider, is_scene_header, is_stage_direction, strip_markdown_escapes,
    strip_markdown_formatting,
};
use xil_core::textsim::get_close_matches;
use xil_core::workspace::{derive_paths, resolve_slug, show_slug, workspace_root};
use xil_core::{banner, log};

/// Native paralinguistic cues Chatterbox Turbo renders. Anything else is
/// silently stripped at generation time, which is what makes a near-miss
/// worth flagging. Mirrors `ALLOWED_TAGS` in `chatterbox_turbo_worker.py`.
const ALLOWED_TAGS: [&str; 19] = [
    "advertisement",
    "angry",
    "chuckle",
    "clear throat",
    "cough",
    "crying",
    "dramatic",
    "fear",
    "gasp",
    "groan",
    "happy",
    "laugh",
    "narration",
    "sarcastic",
    "shush",
    "sigh",
    "sniff",
    "surprised",
    "whispering",
];

/// Similarity floor for suggesting a Turbo cue. Genuine mistakes score
/// >= 0.73; legitimate ElevenLabs-only tags top out at 0.55.
const NEAR_MISS_CUTOFF: f64 = 0.72;

/// Variants difflib scores too low because the wording diverges.
const TAG_ALIASES: [(&str, &str); 5] = [
    ("throat clearing", "clear throat"),
    ("throat clear", "clear throat"),
    ("cries", "crying"),
    ("cry", "crying"),
    ("crys", "crying"),
];

static INLINE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\[\]]+)\]").unwrap());

#[derive(Parser)]
#[command(
    name = "xil-scan",
    about = "Pre-flight scanner: check a production script for unknown speakers/sections."
)]
struct Args {
    /// Path to the markdown production script (required unless --harvest-cast or --backfill-cast)
    path: Option<PathBuf>,
    /// Output machine-readable JSON instead of the human report
    #[arg(long)]
    json: bool,
    /// Path to speakers.json (default: auto-detect from CWD, then built-in)
    #[arg(long)]
    speakers: Option<PathBuf>,
    /// Show name override — selects configs/{slug}/speakers.json (default: from project.json)
    #[arg(long, value_name = "NAME")]
    show: Option<String>,
    /// Path to sfx_<TAG>.json — enables direction-text audit against existing config
    #[arg(long, value_name = "PATH")]
    sfx: Option<PathBuf>,
    /// Episode tag (e.g. S04E04) — auto-discovers sfx config when --sfx is omitted
    #[arg(long, value_name = "TAG")]
    episode: Option<String>,
    /// Scan all scripts in scripts/ for CAST: blocks and report speakers missing from speakers.json
    #[arg(long)]
    harvest_cast: bool,
    /// Add CAST: blocks to scripts that don't have one, inferring speakers from parsed JSON or body scan
    #[arg(long)]
    backfill_cast: bool,
    /// Scripts directory for --harvest-cast / --backfill-cast (default: scripts/ under workspace root)
    #[arg(long, value_name = "DIR")]
    scripts_dir: Option<PathBuf>,
    /// Apply changes without confirmation (for --harvest-cast and --backfill-cast)
    #[arg(long, short = 'y')]
    yes: bool,
    /// Preview backfill changes without writing files (implied when --yes is absent for --backfill-cast)
    #[arg(long)]
    dry_run: bool,
}

/// Read and apply the parser's two-pass normalization.
fn load_and_normalize(path: &Path) -> std::io::Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    let text: String = text.nfc().collect();
    let text = strip_markdown_escapes(&text);
    let text = strip_markdown_formatting(&text);
    Ok(text.split('\n').map(str::to_string).collect())
}

/// A bare ALL-CAPS line worth classifying.
fn is_all_caps_candidate(line: &str) -> bool {
    let len = line.chars().count();
    if !(2..80).contains(&len) {
        return false;
    }
    if line != line.to_uppercase() {
        return false;
    }
    !(is_divider(line) || is_stage_direction(line) || is_scene_header(line) || line.ends_with(':'))
}

struct Scan {
    sections: Vec<Value>,
    speakers: IndexMap<String, (String, usize, Vec<usize>)>,
    unrecognized: IndexMap<String, Vec<usize>>,
}

/// Classify every ALL-CAPS candidate into section, speaker or unknown.
fn scan_script(
    lines: &[String],
    speakers_tbl: &Speakers,
    section_map: &IndexMap<String, String>,
) -> Scan {
    let mut sections = Vec::new();
    let mut speakers: IndexMap<String, (String, usize, Vec<usize>)> = IndexMap::new();
    let mut unrecognized: IndexMap<String, Vec<usize>> = IndexMap::new();

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let lineno = i + 1;
        if line.is_empty() {
            continue;
        }
        if line.starts_with("END OF") {
            break;
        }
        // Never mine a stage direction for speaker names.
        if is_stage_direction(line) {
            continue;
        }

        if let Some(m) = try_match_speaker(line, speakers_tbl) {
            // Report the speaker prefix that matched, not the whole line.
            let display = speakers_tbl
                .known
                .iter()
                .find(|s| line.starts_with(s.as_str()))
                .cloned()
                .unwrap_or_else(|| line.split('(').next().unwrap_or(line).trim().to_string());
            let slot = speakers.entry(m.key).or_insert((display, 0, Vec::new()));
            slot.1 += 1;
            slot.2.push(lineno);
            continue;
        }

        if !is_all_caps_candidate(line) {
            continue;
        }

        // `is_section_header` tests the legacy SECTION_MAP, and the slug is
        // read from that same table.
        if let Some(slug) = section_map.get(line) {
            let mut s = Map::new();
            s.insert("text".into(), Value::String(line.to_string()));
            s.insert("slug".into(), Value::String(slug.clone()));
            s.insert("line".into(), Value::from(lineno));
            sections.push(Value::Object(s));
            continue;
        }

        unrecognized
            .entry(line.to_string())
            .or_default()
            .push(lineno);
    }

    Scan {
        sections,
        speakers,
        unrecognized,
    }
}

/// Every `<marker> ENGAGES` must have a matching `DISENGAGES`.
fn scan_span_pairing(lines: &[String], marker: &str, allow_colon: bool) -> Vec<Value> {
    // A trailing token is the span's speaker scope ([PHONE FILTER: ENGAGES
    // DEZ]); pairing ignores it, but the pattern must accept it or a scoped
    // ENGAGES reads as no marker and its DISENGAGES looks orphaned.
    let pattern = Regex::new(&format!(
        r"^{}{}\s+(ENGAGES|DISENGAGES)(?:[\s:,-]+\S+)?$",
        regex::escape(marker),
        if allow_colon { ":?" } else { "" }
    ))
    .expect("static pattern");

    let mut stack: Vec<Value> = Vec::new();
    let mut unpaired: Vec<Value> = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if !is_stage_direction(line) {
            continue;
        }
        let inner = inner_text(line);
        let Some(m) = pattern.captures(&inner) else {
            continue;
        };
        let kind = if &m[1] == "ENGAGES" {
            "ENGAGES"
        } else {
            "DISENGAGES"
        };
        let mut item = Map::new();
        item.insert("text".into(), Value::String(inner.clone()));
        item.insert("line".into(), Value::from(i + 1));
        item.insert("type".into(), Value::String(kind.to_string()));
        if kind == "ENGAGES" {
            stack.push(Value::Object(item));
        } else if stack.pop().is_none() {
            unpaired.push(Value::Object(item));
        }
    }
    unpaired.extend(stack);
    unpaired
}

/// `line[1:-1].strip()` — the bracket interior, as the scanner slices it.
fn inner_text(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() < 2 {
        return String::new();
    }
    chars[1..chars.len() - 1]
        .iter()
        .collect::<String>()
        .trim()
        .to_string()
}

/// Every looping AMBIENCE needs a stop marker.
fn scan_ambience_coverage(lines: &[String]) -> Vec<Value> {
    let mut open: Vec<Value> = Vec::new();
    let mut unclosed: Vec<Value> = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if !is_stage_direction(line) {
            continue;
        }
        let inner = inner_text(line);
        let Some(body) = inner.strip_prefix("AMBIENCE:") else {
            continue;
        };
        let body = body.trim();
        if body == "STOP" || body.to_uppercase().ends_with("FADES OUT") {
            open.pop(); // a spurious STOP with nothing open is ignored
        } else {
            if let Some(prev) = open.pop() {
                unclosed.push(prev);
            }
            let mut item = Map::new();
            item.insert("text".into(), Value::String(inner.clone()));
            item.insert("line".into(), Value::from(i + 1));
            open.push(Value::Object(item));
        }
    }
    unclosed.extend(open);
    unclosed
}

/// Inline `[tags]` that look like misspelled Turbo cues.
fn scan_paralinguistic_tags(lines: &[String]) -> Vec<Value> {
    let candidates: Vec<String> = {
        let mut v: Vec<String> = ALLOWED_TAGS.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    };
    let mut seen: IndexMap<String, (String, String, Vec<usize>)> = IndexMap::new();

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Filter per token, not per line: a line may open with a cue and
        // continue into dialogue ("[sarcastic] Oh, great.").
        for m in INLINE_TAG.captures_iter(line) {
            let token = m[1].trim().to_string();
            let name = token
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if name.is_empty() || name.contains(':') || ALLOWED_TAGS.contains(&name.as_str()) {
                continue;
            }
            let suggestion = match TAG_ALIASES.iter().find(|(k, _)| *k == name) {
                Some((_, v)) => v.to_string(),
                None => match get_close_matches(&name, &candidates, 1, NEAR_MISS_CUTOFF).first() {
                    Some(s) => s.clone(),
                    None => continue,
                },
            };
            seen.entry(name)
                .or_insert((token, suggestion, Vec::new()))
                .2
                .push(i + 1);
        }
    }

    seen.into_values()
        .map(|(text, suggestion, lines)| {
            let mut m = Map::new();
            m.insert("text".into(), Value::String(text));
            m.insert("suggestion".into(), Value::String(suggestion));
            m.insert(
                "lines".into(),
                Value::Array(lines.into_iter().map(Value::from).collect()),
            );
            Value::Object(m)
        })
        .collect()
}

/// Audit direction texts against an existing SFX config.
fn scan_direction_texts(lines: &[String], sfx_effects: &Map<String, Value>) -> Value {
    let mut seen: IndexMap<String, (String, Option<String>, Vec<usize>)> = IndexMap::new();
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if !is_stage_direction(line) {
            continue;
        }
        let inner = inner_text(line);
        if inner.is_empty() {
            continue;
        }
        // Only SFX/MUSIC/AMBIENCE are config-keyed.
        if !["SFX:", "MUSIC:", "AMBIENCE:"]
            .iter()
            .any(|dt| inner.starts_with(dt))
        {
            continue;
        }
        let h = parse_direction_hint(&inner, "", &mut |_| {});
        let slot =
            seen.entry(h.clean.clone())
                .or_insert((h.clean.clone(), h.source.clone(), Vec::new()));
        slot.2.push(i + 1);
        if slot.1.is_none() {
            if let Some(src) = h.source {
                slot.1 = Some(src);
            }
        }
    }

    let (mut matched, mut hinted, mut new) = (Vec::new(), Vec::new(), Vec::new());
    for (text, hint, line_nos) in seen.into_values() {
        let mut info = Map::new();
        info.insert("text".into(), Value::String(text.clone()));
        info.insert(
            "hint".into(),
            hint.clone().map(Value::String).unwrap_or(Value::Null),
        );
        info.insert(
            "lines".into(),
            Value::Array(line_nos.into_iter().map(Value::from).collect()),
        );
        let info = Value::Object(info);

        let has_source = sfx_effects
            .get(&text)
            .and_then(|e| e.get("source"))
            .is_some_and(|s| !s.is_null() && s.as_str() != Some(""));
        if has_source {
            matched.push(info);
        } else if hint.is_some() {
            hinted.push(info);
        } else {
            new.push(info);
        }
    }
    let mut m = Map::new();
    m.insert("matched".into(), Value::Array(matched));
    m.insert("hinted".into(), Value::Array(hinted));
    m.insert("new".into(), Value::Array(new));
    Value::Object(m)
}

/// The full scan result, in the key order the JSON output uses.
fn build_scan(
    lines: &[String],
    speakers_tbl: &Speakers,
    section_map: &IndexMap<String, String>,
) -> Map<String, Value> {
    let scan = scan_script(lines, speakers_tbl, section_map);

    let mut speakers_obj = Map::new();
    for (key, (display, count, line_nos)) in &scan.speakers {
        let mut info = Map::new();
        info.insert("display".into(), Value::String(display.clone()));
        info.insert("count".into(), Value::from(*count));
        info.insert(
            "lines".into(),
            Value::Array(line_nos.iter().map(|n| Value::from(*n)).collect()),
        );
        speakers_obj.insert(key.clone(), Value::Object(info));
    }
    let unrecognized: Vec<Value> = scan
        .unrecognized
        .iter()
        .map(|(text, line_nos)| {
            let mut u = Map::new();
            u.insert("text".into(), Value::String(text.clone()));
            u.insert(
                "lines".into(),
                Value::Array(line_nos.iter().map(|n| Value::from(*n)).collect()),
            );
            Value::Object(u)
        })
        .collect();

    let slugs: Vec<&str> = scan
        .sections
        .iter()
        .filter_map(|s| s["slug"].as_str())
        .collect();
    let mut pp = Map::new();
    pp.insert("preamble".into(), Value::Bool(slugs.contains(&"preamble")));
    pp.insert(
        "postamble".into(),
        Value::Bool(slugs.contains(&"postamble")),
    );

    let mut out = Map::new();
    out.insert("sections".into(), Value::Array(scan.sections));
    out.insert("speakers".into(), Value::Object(speakers_obj));
    out.insert("unrecognized".into(), Value::Array(unrecognized));
    out.insert("preamble_postamble".into(), Value::Object(pp));
    // VINTAGE FILTER deliberately does not accept the colon form: widening
    // it would newly fail scripts that pass today.
    out.insert(
        "vintage_filter_unpaired".into(),
        Value::Array(scan_span_pairing(lines, "VINTAGE FILTER", false)),
    );
    out.insert(
        "film_audio_unpaired".into(),
        Value::Array(scan_span_pairing(lines, "FILM AUDIO", true)),
    );
    out.insert(
        "speakerphone_unpaired".into(),
        Value::Array(scan_span_pairing(lines, "SPEAKERPHONE", true)),
    );
    out.insert(
        "phone_filter_unpaired".into(),
        Value::Array(scan_span_pairing(lines, "PHONE FILTER", true)),
    );
    out.insert(
        "ambience_unclosed".into(),
        Value::Array(scan_ambience_coverage(lines)),
    );
    out.insert(
        "paralinguistic_near_misses".into(),
        Value::Array(scan_paralinguistic_tags(lines)),
    );
    out
}

// ── report ──────────────────────────────────────────────────────────────

fn arr(scan: &Map<String, Value>, key: &str) -> Vec<Value> {
    scan.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Pad to `width` by character count, as Python's `:<N` does.
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

fn line_list(line_nos: &[Value]) -> String {
    let shown: Vec<String> = line_nos.iter().take(5).map(|v| v.to_string()).collect();
    let mut s = shown.join(", ");
    if line_nos.len() > 5 {
        s.push_str(&format!(" (+{} more)", line_nos.len() - 5));
    }
    s
}

fn format_report(scan: &Map<String, Value>, header: &Map<String, Value>) -> String {
    let mut out: Vec<String> = Vec::new();

    let show = header.get("show").and_then(Value::as_str).unwrap_or("");
    let title = header.get("title").and_then(Value::as_str).unwrap_or("");
    let season = header.get("season").and_then(Value::as_i64);
    let episode = header.get("episode").and_then(Value::as_i64);
    // `if season and episode` — zero is falsy in Python too.
    let ep_tag = match (season, episode) {
        (Some(s), Some(e)) if s != 0 && e != 0 => format!("S{s:02}E{e:02}"),
        _ => String::new(),
    };
    let headline = [show, ep_tag.as_str(), title]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" — ");
    if !headline.is_empty() {
        out.push(format!("=== {headline} ==="));
    }
    out.push(String::new());

    let sections = arr(scan, "sections");
    out.push(format!("SECTIONS ({} found)", sections.len()));
    if sections.is_empty() {
        out.push("  (none)".into());
    } else {
        for s in &sections {
            out.push(format!(
                "  ✓  {} → {}",
                pad(s["text"].as_str().unwrap_or(""), 30),
                s["slug"].as_str().unwrap_or("")
            ));
        }
    }
    out.push(String::new());

    let speakers = scan
        .get("speakers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    out.push(format!("SPEAKERS ({} found)", speakers.len()));
    if speakers.is_empty() {
        out.push("  (none)".into());
    } else {
        let mut keys: Vec<&String> = speakers.keys().collect();
        keys.sort();
        for key in keys {
            let info = &speakers[key];
            out.push(format!(
                "  ✓  {} → {} ({} lines)",
                pad(info["display"].as_str().unwrap_or(""), 18),
                pad(key, 18),
                info["count"]
            ));
        }
    }
    out.push(String::new());

    let unknown = arr(scan, "unrecognized");
    if !unknown.is_empty() {
        out.push(format!(
            "UNRECOGNIZED CANDIDATES ({} — action needed before XILP001)",
            unknown.len()
        ));
        for u in &unknown {
            let nos = u["lines"].as_array().cloned().unwrap_or_default();
            out.push(format!(
                "  ⚠  {}  lines: {}",
                pad(u["text"].as_str().unwrap_or(""), 30),
                line_list(&nos)
            ));
        }
        out.push(String::new());
        out.push(format!(
            "⚠️  {} unrecognized candidate(s). Add to speakers.json or SECTION_MAP before parsing.",
            unknown.len()
        ));
    } else {
        out.push("UNRECOGNIZED CANDIDATES".into());
        out.push("  (none)".into());
    }

    if let Some(pp) = scan.get("preamble_postamble").and_then(Value::as_object) {
        out.push(String::new());
        out.push("PREAMBLE / POSTAMBLE".into());
        for key in ["preamble", "postamble"] {
            let present = pp.get(key).and_then(Value::as_bool).unwrap_or(false);
            let mark = if present { "✓" } else { "⚠" };
            let status = if present {
                "present"
            } else {
                "MISSING — add before broadcast production"
            };
            out.push(format!("  {mark}  {} {status}", pad(key, 12)));
        }
    }

    for (title, key) in [
        ("VINTAGE FILTER PAIRING", "vintage_filter_unpaired"),
        ("FILM AUDIO PAIRING", "film_audio_unpaired"),
        ("SPEAKERPHONE PAIRING", "speakerphone_unpaired"),
        ("PHONE FILTER PAIRING", "phone_filter_unpaired"),
    ] {
        let items = arr(scan, key);
        out.push(String::new());
        out.push(title.into());
        if items.is_empty() {
            out.push("  ✓  all markers paired (or none present)".into());
        } else {
            for item in &items {
                out.push(format!(
                    "  ⚠  {} unpaired  line {}",
                    pad(item["type"].as_str().unwrap_or(""), 12),
                    item["line"]
                ));
            }
        }
    }

    let amb = arr(scan, "ambience_unclosed");
    out.push(String::new());
    out.push("AMBIENCE LOOP COVERAGE".into());
    if amb.is_empty() {
        out.push("  ✓  all ambience loops have stop markers (or none present)".into());
    } else {
        for item in &amb {
            out.push(format!(
                "  ⚠  no stop marker for [{}]  line {}",
                item["text"].as_str().unwrap_or(""),
                item["line"]
            ));
        }
    }

    let near = arr(scan, "paralinguistic_near_misses");
    if !near.is_empty() {
        out.push(String::new());
        out.push(format!("PARALINGUISTIC TAG NEAR-MISSES ({})", near.len()));
        for item in &near {
            let nos = item["lines"].as_array().cloned().unwrap_or_default();
            out.push(format!(
                "  ⚠  [{}] → did you mean [{}]?  lines: {}",
                item["text"].as_str().unwrap_or(""),
                item["suggestion"].as_str().unwrap_or(""),
                line_list(&nos)
            ));
        }
        out.push(
            "\n  ℹ  Chatterbox Turbo renders only its exact cue tokens and silently strips the rest; \
             other backends are unaffected."
                .into(),
        );
    }

    if let Some(dt) = scan.get("direction_texts").and_then(Value::as_object) {
        let get = |k: &str| {
            dt.get(k)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let (matched, hinted, new) = (get("matched"), get("hinted"), get("new"));
        out.push(String::new());
        out.push(format!(
            "DIRECTION TEXT AUDIT  ({} matched / {} hinted / {} new)",
            matched.len(),
            hinted.len(),
            new.len()
        ));
        for i in &matched {
            out.push(format!(
                "  ✓  [reuse]  {}",
                i["text"].as_str().unwrap_or("")
            ));
        }
        for i in &hinted {
            let fname = i["hint"]
                .as_str()
                .map(|h| basename(Path::new(h)))
                .unwrap_or_else(|| "?".into());
            out.push(format!(
                "  +  [hydrate] {}  → {fname}",
                i["text"].as_str().unwrap_or("")
            ));
        }
        for i in &new {
            out.push(format!(
                "  ·  [new]    {}",
                i["text"].as_str().unwrap_or("")
            ));
        }
        if !hinted.is_empty() {
            out.push(format!(
                "\n  ℹ  {} hinted — run `xil sfx-hydrate` to write source fields",
                hinted.len()
            ));
        }
        if !new.is_empty() {
            out.push(format!(
                "  ℹ  {} new — add prompts to sfx config before producing",
                new.len()
            ));
        }
    }

    out.push(String::new());
    let fatal = !unknown.is_empty() || !arr(scan, "vintage_filter_unpaired").is_empty();
    out.push(if fatal {
        "❌  Errors found — resolve before running XILP001.".into()
    } else {
        "✅  All sections and speakers recognized — safe to run XILP001.".to_string()
    });
    out.join("\n")
}

// ── migration modes ─────────────────────────────────────────────────────

/// `--speakers`, else `configs/{slug}/speakers.json` when `--show` names one.
fn resolve_speakers_path(speakers: Option<&Path>, show: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = speakers {
        return Some(p.to_path_buf());
    }
    let show = show?;
    let candidate = PathBuf::from("configs")
        .join(show_slug(show))
        .join("speakers.json");
    candidate.exists().then_some(candidate)
}

/// `glob(dir/**/*.md, recursive=True) or glob(dir/*.md)`.
fn all_scripts(scripts_dir: &Path) -> Vec<PathBuf> {
    glob_recursive(scripts_dir, "", ".md")
}

fn harvest_cast(
    scripts_dir: &Path,
    speakers_path: Option<&Path>,
    apply: bool,
) -> anyhow::Result<()> {
    let scripts = all_scripts(scripts_dir);
    if scripts.is_empty() {
        log::info(&format!(
            "No .md scripts found in {}",
            scripts_dir.display()
        ));
        return Ok(());
    }

    let mut all_cast: IndexMap<String, (String, Vec<String>)> = IndexMap::new();
    for path in &scripts {
        let Ok(lines) = load_and_normalize(path) else {
            continue;
        };
        for entry in extract_cast_from_script(&lines) {
            all_cast
                .entry(entry.key.clone())
                .or_insert((entry.display.clone(), Vec::new()))
                .1
                .push(basename(path));
        }
    }

    if all_cast.is_empty() {
        log::info(&format!(
            "No CAST: blocks found in {} script(s).  Run --backfill-cast to add them.",
            scripts.len()
        ));
        return Ok(());
    }

    let mut existing: Vec<Value> = Vec::new();
    let mut existing_keys: Vec<String> = Vec::new();
    if let Some(p) = speakers_path.filter(|p| p.exists()) {
        if let Ok(v) = serde_json::from_str::<Value>(&fs::read_to_string(p)?) {
            existing = v.as_array().cloned().unwrap_or_default();
            existing_keys = existing
                .iter()
                .filter_map(|e| e.get("key")?.as_str().map(str::to_string))
                .collect();
        }
    }

    let mut sorted_keys: Vec<&String> = all_cast.keys().collect();
    sorted_keys.sort();
    let new_entries: Vec<(String, String, Vec<String>)> = sorted_keys
        .into_iter()
        .filter(|k| !existing_keys.contains(k))
        .map(|k| {
            let (display, scripts) = &all_cast[k];
            (k.clone(), display.clone(), scripts.clone())
        })
        .collect();

    log::info(&format!(
        "CAST harvest — {} script(s) scanned, {} unique speaker(s) found, {} new",
        scripts.len(),
        all_cast.len(),
        new_entries.len()
    ));
    for (key, display, in_scripts) in &new_entries {
        log::info(&format!(
            "  +  {}  key: {}  in: {}",
            pad(display, 30),
            pad(key, 22),
            in_scripts.join(", ")
        ));
    }
    if new_entries.is_empty() {
        log::info("✅  All CAST-declared speakers already in speakers.json.");
        return Ok(());
    }
    if !apply {
        log::info("(dry-run) Re-run with --yes to add these to speakers.json.");
        return Ok(());
    }

    let write_path = match speakers_path {
        Some(p) => p.to_path_buf(),
        None => {
            let show = xil_core::workspace::read_project("project.json")
                .get("show")
                .and_then(Value::as_str)
                .unwrap_or("Sample Show")
                .to_string();
            workspace_root()
                .join("configs")
                .join(show_slug(&show))
                .join("speakers.json")
        }
    };
    if let Some(d) = write_path.parent() {
        fs::create_dir_all(d)?;
    }
    for (key, display, _) in &new_entries {
        let mut e = Map::new();
        e.insert("display".into(), Value::String(display.clone()));
        e.insert("key".into(), Value::String(key.clone()));
        existing.push(Value::Object(e));
    }
    fs::write(&write_path, dumps(&Value::Array(existing), Style::INDENT2))?;
    log::info(&format!(
        "✅  Added {} new speaker(s) to {}",
        new_entries.len(),
        write_path.display()
    ));
    Ok(())
}

fn backfill_cast(
    scripts_dir: &Path,
    speakers_path: Option<&Path>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let root = workspace_root();
    let scripts = all_scripts(scripts_dir);
    if scripts.is_empty() {
        log::info(&format!(
            "No .md scripts found in {}",
            scripts_dir.display()
        ));
        return Ok(());
    }

    let mut registry: IndexMap<String, Map<String, Value>> = IndexMap::new();
    if let Some(p) = speakers_path.filter(|p| p.exists()) {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&fs::read_to_string(p)?) {
            for e in items {
                if let (Some(o), Some(k)) = (e.as_object(), e.get("key").and_then(Value::as_str)) {
                    registry.insert(k.to_string(), o.clone());
                }
            }
        }
    }
    let speakers_tbl = load_speakers(speakers_path, &[]);
    let section_map = get_section_map(&xil_core::workspace::resolve_project_type("project.json"));
    let parsed_dir = root.join("parsed");

    let mut modified = 0usize;
    for script_path in &scripts {
        let Ok(lines) = load_and_normalize(script_path) else {
            continue;
        };
        let name = basename(script_path);
        if lines.iter().any(|l| l.trim() == "CAST:") {
            log::info(&format!("  SKIP  {name}  (CAST: block already present)"));
            continue;
        }

        // Prefer the parsed JSON whose source_file names this script.
        let mut cast_display: Vec<String> = Vec::new();
        if parsed_dir.is_dir() {
            for parsed_path in glob_recursive(&parsed_dir, "parsed_", ".json") {
                let Ok(parsed) = fs::read_to_string(&parsed_path)
                    .map_err(|e| e.to_string())
                    .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
                else {
                    continue;
                };
                let source_file = parsed
                    .get("source_file")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !source_file.contains(&name) {
                    continue;
                }
                let mut seen: Vec<String> = Vec::new();
                for entry in parsed
                    .get("entries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                {
                    if entry.get("type").and_then(Value::as_str) != Some("dialogue") {
                        continue;
                    }
                    let k = entry.get("speaker").and_then(Value::as_str).unwrap_or("");
                    if !k.is_empty() && !seen.iter().any(|s| s == k) {
                        seen.push(k.to_string());
                    }
                }
                for k in seen {
                    cast_display.push(
                        match registry
                            .get(&k)
                            .and_then(|r| r.get("display"))
                            .and_then(Value::as_str)
                        {
                            Some(d) => d.to_string(),
                            None => k.replace('_', " ").to_uppercase(),
                        },
                    );
                }
                break;
            }
        }

        if cast_display.is_empty() {
            let body = scan_script(&lines, &speakers_tbl, &section_map);
            cast_display = body.speakers.values().map(|(d, _, _)| d.clone()).collect();
        }
        if cast_display.is_empty() {
            log::info(&format!("  SKIP  {name}  (no speakers found)"));
            continue;
        }

        let mut cast_block: Vec<String> = vec!["CAST:".into()];
        for display in &cast_display {
            let role = registry
                .get(&display_to_key(display))
                .and_then(|r| r.get("role"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut entry_line = format!("* {display}");
            if !role.is_empty() && role != "TBD" {
                entry_line.push_str(&format!(" — {role}"));
            }
            cast_block.push(entry_line);
        }
        cast_block.push(String::new());

        if dry_run {
            log::info(&format!("  ADD   {name}:"));
            for bl in &cast_block {
                log::info(&format!("        {bl}"));
            }
            continue;
        }

        let original = fs::read_to_string(script_path)?;
        let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
        let Some(insert_at) = original_lines.iter().position(|l| {
            let t = l.trim();
            t == "===" || t == "---"
        }) else {
            log::warning(&format!("  WARN  {name}  (no === divider found, skipping)"));
            continue;
        };
        let mut new_text = String::new();
        new_text.push_str(&original_lines[..insert_at].concat());
        for bl in &cast_block {
            new_text.push_str(bl);
            new_text.push('\n');
        }
        new_text.push_str(&original_lines[insert_at..].concat());
        fs::write(script_path, new_text)?;
        log::info(&format!(
            "  WROTE {name}  ({} speaker(s))",
            cast_display.len()
        ));
        modified += 1;
    }

    if dry_run {
        log::info("(dry-run) Re-run without --dry-run to write changes.");
    } else {
        log::info(&format!(
            "✅  Backfill complete — {modified} script(s) updated."
        ));
    }
    Ok(())
}

// ── entry point ─────────────────────────────────────────────────────────

fn execute(a: &Args) -> anyhow::Result<i32> {
    let speakers_path = resolve_speakers_path(a.speakers.as_deref(), a.show.as_deref());
    let scripts_dir = a
        .scripts_dir
        .clone()
        .unwrap_or_else(|| workspace_root().join("scripts"));

    if a.harvest_cast {
        harvest_cast(&scripts_dir, speakers_path.as_deref(), a.yes)?;
        return Ok(0);
    }
    if a.backfill_cast {
        backfill_cast(&scripts_dir, speakers_path.as_deref(), !a.yes)?;
        return Ok(0);
    }

    let Some(path) = &a.path else {
        log::error("path argument required (or use --harvest-cast / --backfill-cast)");
        return Ok(1);
    };
    if !path.exists() {
        log::error(&format!("File not found: {}", path.display()));
        return Ok(1);
    }

    let lines = load_and_normalize(path)?;
    let cast_entries = extract_cast_from_script(&lines);
    let speakers_tbl = load_speakers(speakers_path.as_deref(), &cast_entries);

    // The header is read from the first non-blank of the opening ten lines.
    let mut header = Map::new();
    for line in lines.iter().take(10) {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(h) = parse_script_header(line) {
            header.insert("show".into(), Value::String(h.show));
            header.insert(
                "season".into(),
                h.season.map(Value::from).unwrap_or(Value::Null),
            );
            header.insert("episode".into(), Value::from(h.episode));
            header.insert("title".into(), Value::String(h.title));
        }
        break;
    }

    // The scanner reads slugs from the legacy SECTION_MAP, not a per-type map.
    let section_map = get_section_map("__legacy__");
    let mut scan = build_scan(&lines, &speakers_tbl, &section_map);

    let sfx_path = a.sfx.clone().or_else(|| {
        let tag = a.episode.as_ref()?;
        let slug = resolve_slug(a.show.as_deref(), "project.json");
        derive_paths(&slug, tag).get("sfx").cloned()
    });
    if let Some(p) = sfx_path.filter(|p| p.exists()) {
        let data: Value = serde_json::from_str(&fs::read_to_string(&p)?)?;
        let effects = data
            .get("effects")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        scan.insert(
            "direction_texts".into(),
            scan_direction_texts(&lines, &effects),
        );
    }

    if a.json {
        println!("{}", dumps(&Value::Object(scan.clone()), Style::INDENT2));
    } else {
        log::info(&format_report(&scan, &header));
    }

    let fatal = !arr(&scan, "unrecognized").is_empty()
        || !arr(&scan, "vintage_filter_unpaired").is_empty()
        || !arr(&scan, "film_audio_unpaired").is_empty()
        || !arr(&scan, "speakerphone_unpaired").is_empty();
    Ok(if fatal { 1 } else { 0 })
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("scan");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-scan", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    execute(&a)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    #[test]
    fn all_caps_candidates_exclude_structure() {
        assert!(is_all_caps_candidate("ADAM"));
        assert!(!is_all_caps_candidate("A"), "too short");
        assert!(!is_all_caps_candidate(&"X".repeat(80)), "too long");
        assert!(!is_all_caps_candidate("Adam"), "not all caps");
        assert!(!is_all_caps_candidate("CAST:"), "metadata label");
        assert!(!is_all_caps_candidate("==="));
        assert!(!is_all_caps_candidate("[SFX: X]"));
        assert!(!is_all_caps_candidate("SCENE 1: ROOM"));
    }

    #[test]
    fn span_pairing_reports_both_orphan_kinds() {
        let l = lines("[VINTAGE FILTER ENGAGES]\n[VINTAGE FILTER DISENGAGES]\n[VINTAGE FILTER DISENGAGES]\n[VINTAGE FILTER ENGAGES]");
        let un = scan_span_pairing(&l, "VINTAGE FILTER", false);
        assert_eq!(un.len(), 2);
        assert_eq!(un[0]["type"], "DISENGAGES");
        assert_eq!(un[0]["line"], 3);
        assert_eq!(un[1]["type"], "ENGAGES");
        assert_eq!(un[1]["line"], 4);
    }

    #[test]
    fn colon_form_only_matches_where_allowed() {
        let l = lines("[VINTAGE FILTER: ENGAGES]");
        assert!(
            scan_span_pairing(&l, "VINTAGE FILTER", false).is_empty(),
            "colon form is not matched"
        );
        let l = lines("[PHONE FILTER: ENGAGES DEZ]");
        let un = scan_span_pairing(&l, "PHONE FILTER", true);
        assert_eq!(un.len(), 1, "a scoped ENGAGES still counts as open");
        assert_eq!(un[0]["type"], "ENGAGES");
    }

    #[test]
    fn ambience_needs_a_stop_marker() {
        let l = lines("[AMBIENCE: RAIN]\n[AMBIENCE: STOP]\n[AMBIENCE: WIND]\n[AMBIENCE: TRAFFIC]");
        let un = scan_ambience_coverage(&l);
        assert_eq!(
            un.len(),
            2,
            "WIND is replaced without a stop, TRAFFIC is left open"
        );
        assert_eq!(un[0]["text"], "AMBIENCE: WIND");
        assert_eq!(un[1]["text"], "AMBIENCE: TRAFFIC");

        let fades = lines("[AMBIENCE: RAIN]\n[AMBIENCE: RAIN FADES OUT]");
        assert!(scan_ambience_coverage(&fades).is_empty());
    }

    #[test]
    fn near_misses_suggest_the_real_cue() {
        let l = lines("ADAM [laughs] Hello.\nMAYA [clears throat] Yes.\nDEZ [exhausted] No.\n[SFX: DOOR]\nAVA [laughs] Again.");
        let near = scan_paralinguistic_tags(&l);
        assert_eq!(
            near.len(),
            2,
            "[exhausted] is a valid ElevenLabs tag, not a near-miss"
        );
        assert_eq!(near[0]["text"], "laughs");
        assert_eq!(near[0]["suggestion"], "laugh");
        assert_eq!(near[0]["lines"], serde_json::json!([1, 5]));
        assert_eq!(near[1]["suggestion"], "clear throat");
    }

    #[test]
    fn tag_aliases_cover_what_difflib_misses() {
        let near = scan_paralinguistic_tags(&lines("[throat clearing] x\n[cries] y"));
        assert_eq!(near[0]["suggestion"], "clear throat");
        assert_eq!(near[1]["suggestion"], "crying");
    }

    #[test]
    fn direction_audit_sorts_into_three_buckets() {
        let l = lines(
            "[SFX: HAS SOURCE]\n[SFX: HINTED | a.mp3]\n[SFX: BRAND NEW]\n[BEAT]\n[MUSIC: NO KEY]",
        );
        let mut effects = Map::new();
        effects.insert(
            "SFX: HAS SOURCE".into(),
            serde_json::json!({"source": "SFX/x.mp3"}),
        );
        effects.insert("MUSIC: NO KEY".into(), serde_json::json!({"prompt": "p"}));
        let dt = scan_direction_texts(&l, &effects);
        assert_eq!(dt["matched"].as_array().unwrap().len(), 1);
        assert_eq!(dt["hinted"].as_array().unwrap().len(), 1);
        assert_eq!(
            dt["new"].as_array().unwrap().len(),
            2,
            "BEAT is not config-keyed and is skipped"
        );
        assert_eq!(dt["hinted"][0]["hint"], "SFX/a.mp3");
    }

    #[test]
    fn inner_text_slices_like_python() {
        assert_eq!(inner_text("[SFX: X]"), "SFX: X");
        assert_eq!(inner_text("[]"), "");
        assert_eq!(inner_text("["), "");
        assert_eq!(
            inner_text("[CAFÉ]"),
            "CAFÉ",
            "sliced by character, not byte"
        );
    }

    #[test]
    fn pad_counts_characters() {
        assert_eq!(pad("CAFÉ", 6), "CAFÉ  ");
        assert_eq!(pad("toolong", 3), "toolong");
    }
}

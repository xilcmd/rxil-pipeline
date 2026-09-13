//! Shared multi-track mixing for `assemble` and `daw`. Port of
//! `mix_common.py`.
//!
//! Stems are classified by the parsed script's `direction_type`, then built
//! into a foreground (dialogue, SFX, beats — laid end to end with a gap)
//! and background layers (ambience looped to the next cue, music at its
//! cue, the vintage crackle between its markers). Every audio operation is
//! a [`Segment`] call, so the arithmetic is pydub's.

pub mod config;
pub mod timeline;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value};
use xil_audio::fx::{self, AudioFxError};
use xil_audio::segment::Segment;
use xil_core::fsutil::basename;
use xil_core::log;

use config::{Filter, Num, SfxConfig, SfxEntry, Voice};

/// Direction types routed to the background layers.
pub const BACKGROUND_DIRECTION_TYPES: [&str; 6] = [
    "AMBIENCE",
    "MUSIC",
    "VINTAGE FILTER",
    "FILM AUDIO",
    "SPEAKERPHONE",
    "PHONE FILTER",
];

/// Span markers and the treatment each engages, in stacking order.
pub const SPAN_DIRECTION_TREATMENTS: [(&str, &str); 4] = [
    ("VINTAGE FILTER", "vintage"),
    ("FILM AUDIO", "film"),
    ("SPEAKERPHONE", "speakerphone"),
    ("PHONE FILTER", "phone"),
];

/// Which markers of each span type get a synthetic boundary plan.
fn span_sentinel_markers(direction_type: &str) -> &'static [&'static str] {
    match direction_type {
        "VINTAGE FILTER" => &["DISENGAGES"],
        "FILM AUDIO" | "SPEAKERPHONE" | "PHONE FILTER" => &["ENGAGES", "DISENGAGES"],
        _ => &[],
    }
}

/// Treatments that must not stack with the same cast filter.
const DEDUPED_TREATMENTS: [&str; 1] = ["phone"];

pub const AMBIENCE_LEVEL_DB: f64 = -10.0;
pub const MUSIC_LEVEL_DB: f64 = -6.0;

/// The speaker keys `FILTER_REGISTRY` knows, for the unknown-filter warning.
const FILTER_REGISTRY: [&str; 4] = ["phone", "vintage", "film", "speakerphone"];

/// `os.path.splitext(os.path.basename(p))[0]`.
pub fn stem_basename(p: &str) -> String {
    let name = basename(Path::new(p));
    match name.rfind('.') {
        Some(dot) if dot > 0 && !name[..dot].chars().all(|c| c == '.') => name[..dot].to_string(),
        _ => name,
    }
}

/// `basename.rsplit("_", 1)[-1]`.
fn last_underscore_part(s: &str) -> &str {
    s.rsplit('_').next().unwrap_or(s)
}

fn span_marker(text: Option<&str>) -> Option<&'static str> {
    let body = text.unwrap_or("");
    if body.contains("DISENGAGES") {
        Some("DISENGAGES")
    } else if body.contains("ENGAGES") {
        Some("ENGAGES")
    } else {
        None
    }
}

/// Python `str.strip(chars)`.
fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}

fn span_scope(text: Option<&str>, direction_type: &str) -> Option<String> {
    let mut body: String = text.unwrap_or("").trim().to_string();
    if body.to_uppercase().starts_with(direction_type) {
        body = body.chars().skip(direction_type.chars().count()).collect();
    }
    let upper = body.to_uppercase();
    // `find` on the upper-cased copy gives a *character* index in Python.
    let idx_bytes = upper.find("ENGAGES")?;
    let idx_chars = upper[..idx_bytes].chars().count();
    let rest: String = body.chars().skip(idx_chars + "ENGAGES".len()).collect();
    let scope = strip_chars(&rest, " :,-").trim().to_lowercase();
    if scope.is_empty() {
        None
    } else {
        Some(scope)
    }
}

/// Resolved metadata for one stem file (or a boundary sentinel).
#[derive(Clone, Debug)]
pub struct StemPlan {
    pub seq: i64,
    /// Empty for a sentinel with no audio.
    pub filepath: String,
    pub direction_type: Option<String>,
    pub entry_type: Option<String>,
    pub text: Option<String>,
    pub scene: Option<String>,
    pub foreground_override: bool,
    pub volume_percentage: Option<Num>,
    pub ramp_in_seconds: Option<Num>,
    pub ramp_out_seconds: Option<Num>,
    pub play_duration: Option<f64>,
    pub tts_model: Option<String>,
    pub pre_trimmed: bool,
    pub loop_: bool,
}

impl StemPlan {
    fn new(
        seq: i64,
        filepath: String,
        direction_type: Option<String>,
        entry_type: Option<String>,
        text: Option<String>,
    ) -> StemPlan {
        StemPlan {
            seq,
            filepath,
            direction_type,
            entry_type,
            text,
            scene: None,
            foreground_override: false,
            volume_percentage: None,
            ramp_in_seconds: None,
            ramp_out_seconds: None,
            play_duration: None,
            tts_model: None,
            pre_trimmed: false,
            loop_: true,
        }
    }

    pub fn is_background(&self) -> bool {
        if self.foreground_override {
            return false;
        }
        self.dt()
            .is_some_and(|d| BACKGROUND_DIRECTION_TYPES.contains(&d))
    }

    pub fn dt(&self) -> Option<&str> {
        self.direction_type.as_deref()
    }

    pub fn is_dialogue(&self) -> bool {
        self.entry_type.as_deref() == Some("dialogue")
    }

    /// The speaker key from the stem name (`…_<speaker>.mp3`).
    pub fn speaker_raw(&self) -> String {
        last_underscore_part(&stem_basename(&self.filepath)).to_string()
    }

    fn speaker_lower(&self) -> String {
        if self.filepath.is_empty() {
            return String::new();
        }
        self.speaker_raw().to_lowercase()
    }
}

/// `int(prefix)` for a stem prefix — whitespace, sign and `_` separators
/// between digits are all accepted by Python.
fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let clean: String = digits.chars().filter(|&c| c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    clean.parse::<i64>().ok().map(|n| if neg { -n } else { n })
}

/// `extract_seq(filepath)`; `None` where Python raises `ValueError`.
pub fn extract_seq(filepath: &str) -> Option<i64> {
    let base = stem_basename(filepath);
    let prefix = base.split('_').next().unwrap_or("");
    if let Some(d) = prefix.strip_prefix('n') {
        if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) {
            return d.parse::<i64>().ok().map(|n| -n);
        }
    }
    py_int(prefix)
}

/// `load_entries_index(parsed_path)` — insertion-ordered, last entry wins.
pub fn load_entries_index(parsed_path: &Path) -> anyhow::Result<IndexMap<i64, Map<String, Value>>> {
    let text = std::fs::read_to_string(parsed_path)?;
    let data: Value = serde_json::from_str(&text)?;
    let mut index = IndexMap::new();
    for e in data["entries"].as_array().into_iter().flatten() {
        if let (Some(seq), Some(o)) = (e.get("seq").and_then(Value::as_i64), e.as_object()) {
            index.insert(seq, o.clone());
        }
    }
    Ok(index)
}

fn get_str(o: &Map<String, Value>, k: &str) -> Option<String> {
    o.get(k).and_then(Value::as_str).map(str::to_string)
}

/// `_volume_pct_to_db`.
pub fn volume_pct_to_db(pct: f64) -> f64 {
    if pct <= 0.0 {
        return f64::NEG_INFINITY;
    }
    20.0 * xil_audio::segment::libm::log10(pct / 100.0)
}

/// `_apply_clip_effects`.
fn apply_clip_effects(
    mut clip: Segment,
    volume: Option<Num>,
    ramp_in_ms: i64,
    ramp_out_ms: i64,
    level_db: f64,
) -> Segment {
    if let Some(v) = volume {
        clip = clip.gain(volume_pct_to_db(v.f()));
    }
    if level_db != 0.0 {
        clip = clip.gain(level_db);
    }
    if ramp_in_ms > 0 {
        clip = clip.fade_in(ramp_in_ms as f64);
    }
    if ramp_out_ms > 0 {
        clip = clip.fade_out(ramp_out_ms as f64);
    }
    clip
}

fn normalize_effect_key(text: &str) -> String {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*\x{2014}\s*").unwrap())
        .replace_all(text, " - ")
        .into_owned()
}

/// `_find_effect_entry`: exact key, then em-dash-normalised.
pub fn find_effect_entry<'a>(cfg: &'a SfxConfig, text: &str) -> Option<&'a SfxEntry> {
    if text.is_empty() {
        return None;
    }
    if let Some(e) = cfg.effects.get(text) {
        return Some(e);
    }
    let norm = normalize_effect_key(text);
    cfg.effects
        .iter()
        .find(|(k, _)| normalize_effect_key(k) == norm)
        .map(|(_, v)| v)
}

type AudioParams = (Option<Num>, Option<Num>, Option<Num>, Option<f64>);

/// `_resolve_audio_params`.
fn resolve_audio_params(plan: &StemPlan, cfg: Option<&SfxConfig>) -> AudioParams {
    let Some(cfg) = cfg else {
        return (None, None, None, None);
    };
    let entry = find_effect_entry(cfg, plan.text.as_deref().unwrap_or(""));
    let prefix = match plan.dt() {
        Some("MUSIC") => "music",
        Some("AMBIENCE") => "ambience",
        Some("SFX") | Some("BEAT") => "sfx",
        Some("VINTAGE FILTER") => "vintage_filter",
        _ => {
            let vol = entry.and_then(|e| e.volume_percentage).map(Num::Float);
            return (vol, None, None, None);
        }
    };
    let pick = |own: Option<f64>, field: &str| -> Option<Num> {
        match own {
            Some(v) => Some(Num::Float(v)),
            None => cfg.default_num(&format!("{prefix}_{field}"), field),
        }
    };
    let vol = pick(entry.and_then(|e| e.volume_percentage), "volume_percentage");
    let ri = pick(entry.and_then(|e| e.ramp_in_seconds), "ramp_in_seconds");
    let ro = pick(entry.and_then(|e| e.ramp_out_seconds), "ramp_out_seconds");
    let pd = match plan.dt() {
        Some("MUSIC") | Some("SFX") | Some("BEAT") => entry.and_then(|e| e.play_duration),
        _ => None,
    };
    (vol, ri, ro, pd)
}

/// `_mp3_duration_ms` — mutagen's header read, truncated to whole ms.
pub fn mp3_duration_ms(path: &str) -> anyhow::Result<i64> {
    Ok(xil_audio::mpeg::duration_ms(Path::new(path))?)
}

/// The `COMM::eng` text of a dialogue stem, only if mutagen could open it
/// as an MP3 at all (it raises, and the Python swallows, otherwise).
fn tts_model_of(path: &str) -> Option<String> {
    let p = Path::new(path);
    xil_audio::mpeg::info(p).ok()?;
    let tag = id3::Tag::read_from_path(p).ok()?;
    let comm = tag
        .comments()
        .find(|c| c.lang == "eng" && c.description.is_empty())?;
    let first = comm.text.split('\0').next().unwrap_or("");
    Some(first.to_string())
}

/// `glob.glob(os.path.join(dir, "*.mp3"))`, sorted.
fn glob_mp3(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = xil_core::fsutil::glob_children(dir, "", ".mp3")
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// `os.path.join(stems_dir, name)` spelled the way glob returns it.
pub fn join(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

/// `collect_stem_plans(stems_dir, entries_index, sfx_config)`.
pub fn collect_stem_plans(
    stems_dir: &Path,
    index: &IndexMap<i64, Map<String, Value>>,
    cfg: Option<&SfxConfig>,
) -> Vec<StemPlan> {
    let mut plans = Vec::new();
    for filepath in glob_mp3(stems_dir) {
        let Some(seq) = extract_seq(&filepath) else {
            continue;
        };
        let name = basename(Path::new(&filepath));
        let Some(entry) = index.get(&seq).filter(|e| !e.is_empty()) else {
            log::warning(&format!(
                "Stale stem skipped: {name} (seq {seq} not in parsed JSON)"
            ));
            continue;
        };
        let entry_type = get_str(entry, "type");
        let base = stem_basename(&filepath);
        let is_sfx_stem = last_underscore_part(&base) == "sfx";
        match entry_type.as_deref() {
            Some("dialogue") | Some("direction") => {}
            other => {
                let shown = other.map(str::to_string).unwrap_or_else(|| "None".into());
                log::warning(&format!(
                    "Stale stem skipped: {name} (seq {seq} is now a {shown} entry)"
                ));
                continue;
            }
        }
        if is_sfx_stem && entry_type.as_deref() == Some("dialogue") {
            log::warning(&format!(
                "Stale stem skipped: {name} (seq {seq} is now a dialogue entry)"
            ));
            continue;
        }
        if !is_sfx_stem && entry_type.as_deref() == Some("direction") {
            log::warning(&format!(
                "Stale stem skipped: {name} (seq {seq} is now a direction entry)"
            ));
            continue;
        }
        if entry_type.as_deref() == Some("dialogue") {
            if let Some(speaker) = entry
                .get("speaker")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                if !base.ends_with(&format!("_{speaker}")) {
                    log::warning(&format!(
                        "Stale stem skipped: {name} (seq {seq} speaker is now {speaker})"
                    ));
                    continue;
                }
            }
        }

        let mut plan = StemPlan::new(
            seq,
            filepath.clone(),
            get_str(entry, "direction_type"),
            entry_type.clone(),
            get_str(entry, "text"),
        );
        plan.scene = get_str(entry, "scene");
        let section = get_str(entry, "section");
        if plan.dt() == Some("MUSIC")
            && (matches!(section.as_deref(), Some("preamble") | Some("postamble")) || plan.seq < 0)
        {
            plan.foreground_override = true;
        }
        let (vol, ri, ro, pd) = resolve_audio_params(&plan, cfg);
        plan.volume_percentage = vol;
        plan.ramp_in_seconds = ri;
        plan.ramp_out_seconds = ro;
        plan.play_duration = pd;
        if plan.is_dialogue() {
            if let Some(model) = tts_model_of(&filepath) {
                plan.tts_model = Some(model);
            }
        }
        let src_entry = cfg.and_then(|c| find_effect_entry(c, plan.text.as_deref().unwrap_or("")));
        if let Some(src) = src_entry {
            if !src.loop_ {
                plan.loop_ = false;
            }
            if src.source.is_some() && plan.play_duration.is_none() && src.duration_seconds > 0.0 {
                match mp3_duration_ms(&filepath) {
                    Ok(clip_ms) => {
                        if clip_ms > 0 {
                            let target_ms = src.duration_seconds * 1000.0;
                            let pct = target_ms / clip_ms as f64 * 100.0;
                            plan.play_duration = Some(if pct < 100.0 { pct } else { 100.0 });
                        }
                    }
                    Err(_) => log::debug(&format!(
                        "Could not read duration for {name} — duration_seconds ignored"
                    )),
                }
            }
        }
        plans.push(plan);
    }

    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for plan in plans {
        if !seen.insert(plan.seq) {
            log::warning(&format!(
                "Duplicate stem skipped: {} (seq {} already loaded)",
                basename(Path::new(&plan.filepath)),
                plan.seq
            ));
            continue;
        }
        deduped.push(plan);
    }
    let mut plans = deduped;

    let seen: HashSet<i64> = plans.iter().map(|p| p.seq).collect();
    for (&seq, entry) in index {
        if seen.contains(&seq) {
            continue;
        }
        let text = get_str(entry, "text").unwrap_or_default();
        let dt = get_str(entry, "direction_type");
        if dt.as_deref() == Some("AMBIENCE")
            && (text == "AMBIENCE: STOP" || text.ends_with("FADES OUT"))
        {
            plans.push(StemPlan::new(
                seq,
                String::new(),
                Some("AMBIENCE".into()),
                get_str(entry, "type"),
                Some(text.clone()),
            ));
        }
        if let Some(d) = dt.as_deref() {
            let markers = span_sentinel_markers(d);
            if !markers.is_empty() && span_marker(Some(&text)).is_some_and(|m| markers.contains(&m))
            {
                plans.push(StemPlan::new(
                    seq,
                    String::new(),
                    dt.clone(),
                    get_str(entry, "type"),
                    Some(text.clone()),
                ));
            }
        }
    }
    plans
}

/// Stable sort by seq, as `sorted(plans, key=lambda p: p.seq)`.
pub fn by_seq(plans: &[StemPlan]) -> Vec<&StemPlan> {
    let mut v: Vec<&StemPlan> = plans.iter().collect();
    v.sort_by_key(|p| p.seq);
    v
}

static WARNED_UNKNOWN_FILTERS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// `_apply_named_filter`.
fn apply_named_filter(segment: Segment, name: &str) -> Result<Segment, AudioFxError> {
    match name {
        "phone" | "film" | "speakerphone" => fx::apply_treatment(&segment, name),
        "vintage" => Ok(apply_vintage_filter(&segment)),
        _ => {
            let mut guard = WARNED_UNKNOWN_FILTERS
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if guard
                .get_or_insert_with(HashSet::new)
                .insert(name.to_string())
            {
                let mut known = FILTER_REGISTRY.to_vec();
                known.sort();
                log::warning(&format!(
                    "Unknown speaker filter {} — ignored. Known filters: {}",
                    fx::py_repr(name),
                    known.join(", ")
                ));
            }
            Ok(segment)
        }
    }
}

/// `apply_vintage_filter` — mono collapse, pydub single-pole EQ, -3 dB.
pub fn apply_vintage_filter(segment: &Segment) -> Segment {
    let mono = segment
        .clone()
        .set_channels(1)
        .set_channels(segment.channels);
    mono.low_pass_filter(5000.0)
        .high_pass_filter(150.0)
        .gain(-3.0)
}

/// `_cast_filter_names`.
pub fn cast_filter_names(filter: &Filter) -> Vec<String> {
    if !filter.truthy() {
        return Vec::new();
    }
    match filter {
        Filter::Bool(true) => vec!["phone".into()],
        other => other
            .py_str()
            .split(',')
            .map(|n| n.trim().to_lowercase())
            .filter(|n| !n.is_empty())
            .collect(),
    }
}

pub fn apply_speaker_filters(
    mut segment: Segment,
    filter: &Filter,
) -> Result<Segment, AudioFxError> {
    for name in cast_filter_names(filter) {
        segment = apply_named_filter(segment, &name)?;
    }
    Ok(segment)
}

fn span_names_to_apply(span_names: &[&'static str], cast_names: &[String]) -> Vec<&'static str> {
    if cast_names.is_empty() {
        return span_names.to_vec();
    }
    span_names
        .iter()
        .copied()
        .filter(|n| !(DEDUPED_TREATMENTS.contains(n) && cast_names.iter().any(|c| c == n)))
        .collect()
}

/// `_span_treatments`: dialogue seq → treatments of the spans open there.
pub fn span_treatments(plans: &[StemPlan]) -> IndexMap<i64, Vec<&'static str>> {
    let mut active: IndexMap<&'static str, Option<String>> = IndexMap::new();
    let mut engaged = IndexMap::new();
    for plan in by_seq(plans) {
        let span = SPAN_DIRECTION_TREATMENTS
            .iter()
            .find(|(d, _)| Some(*d) == plan.dt());
        if let Some((direction, _)) = span {
            match span_marker(plan.text.as_deref()) {
                Some("DISENGAGES") => {
                    active.shift_remove(direction);
                }
                Some("ENGAGES") => {
                    active.insert(direction, span_scope(plan.text.as_deref(), direction));
                }
                _ => {}
            }
        } else if plan.is_dialogue() {
            let speaker = plan.speaker_lower();
            let names: Vec<&'static str> = SPAN_DIRECTION_TREATMENTS
                .iter()
                .filter(|(d, _)| match active.get(d) {
                    Some(None) => true,
                    Some(Some(scope)) => *scope == speaker,
                    None => false,
                })
                .map(|(_, t)| *t)
                .collect();
            if !names.is_empty() {
                engaged.insert(plan.seq, names);
            }
        }
    }
    engaged
}

/// `_warn_missing_treatment_codecs`.
fn warn_missing_treatment_codecs(
    cast: &IndexMap<String, Voice>,
    span_map: &IndexMap<i64, Vec<&'static str>>,
) {
    let mut names: Vec<String> = Vec::new();
    for v in cast.values() {
        names.extend(cast_filter_names(&v.filter));
    }
    for t in span_map.values() {
        names.extend(t.iter().map(|s| s.to_string()));
    }
    if names.is_empty() {
        return;
    }
    for (treatment, codec) in fx::missing_codecs(names.iter().map(String::as_str)) {
        fx::warn_once(
            &treatment,
            "codec-preflight",
            &format!(
                "This ffmpeg build has no {} encoder, so the {} treatment will render band-limited but without its codec character. The audio is usable, but it will NOT match a render from a machine that has the encoder — check before publishing a master.",
                fx::py_repr(&codec),
                fx::py_repr(&treatment)
            ),
        );
    }
}

/// Load a stem, as `AudioSegment.from_file`. Decode failures are fatal in
/// the builders that do not catch them, as they are in Python.
fn load(path: &str) -> anyhow::Result<Segment> {
    Ok(Segment::from_file(Path::new(path))?)
}

/// `segment[:max(1, int(len(segment) * pct / 100.0))]`.
fn trim_to_pct(segment: &Segment, pct: f64) -> Segment {
    let n = ((segment.len_ms() as f64 * pct / 100.0) as i64).max(1);
    segment.slice(None, Some(n as f64))
}

/// Dialogue treatment shared by the foreground and the dialogue layer.
fn treat_dialogue(
    mut segment: Segment,
    plan: &StemPlan,
    cast: &IndexMap<String, Voice>,
    span_map: &IndexMap<i64, Vec<&'static str>>,
    vintage_scenes: &[String],
    dialogue_only: bool,
) -> anyhow::Result<Segment> {
    let speaker = plan.speaker_raw();
    let mut cast_names = Vec::new();
    if let Some(v) = cast.get(&speaker) {
        cast_names = cast_filter_names(&v.filter);
        segment = apply_speaker_filters(segment, &v.filter)?;
        segment = segment.pan(v.pan);
    }
    if dialogue_only || plan.is_dialogue() {
        if let Some(names) = span_map.get(&plan.seq).filter(|n| !n.is_empty()) {
            for name in span_names_to_apply(names, &cast_names) {
                segment = apply_named_filter(segment, name)?;
            }
        } else if !vintage_scenes.is_empty()
            && plan
                .scene
                .as_ref()
                .is_some_and(|s| vintage_scenes.contains(s))
        {
            segment = apply_vintage_filter(&segment);
        }
    }
    Ok(segment)
}

/// `build_foreground` → `(foreground, timeline)`.
pub fn build_foreground(
    plans: &[StemPlan],
    cast: &IndexMap<String, Voice>,
    gap_ms: i64,
    vintage_scenes: &[String],
) -> anyhow::Result<(Segment, IndexMap<i64, i64>)> {
    let mut foreground = Segment::empty();
    let mut timeline = IndexMap::new();
    let mut current_ms = 0i64;
    let span_map = span_treatments(plans);
    warn_missing_treatment_codecs(cast, &span_map);

    for plan in by_seq(plans) {
        timeline.insert(plan.seq, current_ms);
        if plan.is_background() {
            continue;
        }
        let mut segment = load(&plan.filepath)?;
        if let (Some(pd), false) = (plan.play_duration, plan.pre_trimmed) {
            segment = trim_to_pct(&segment, pd);
        }
        if matches!(plan.dt(), Some("SFX") | Some("BEAT")) {
            if let Some(v) = plan.volume_percentage {
                segment = segment.gain(volume_pct_to_db(v.f()));
            }
        }
        segment = treat_dialogue(segment, plan, cast, &span_map, vintage_scenes, false)?;
        let len = segment.len_ms();
        // `foreground += segment + silent`: the stem and its gap are synced
        // and joined first, then that block is synced onto the foreground.
        // Resampling the block as one piece is not the same as resampling
        // the stem and the gap separately.
        foreground = foreground.append(segment.append(Segment::silent(gap_ms as f64)));
        current_ms += len + gap_ms;
    }
    Ok((foreground, timeline))
}

/// `_loop_clip(clip, duration_ms)`.
fn loop_clip(clip: &Segment, duration_ms: i64) -> Segment {
    let len = clip.len_ms();
    if len == 0 || duration_ms <= 0 {
        return Segment::silent(duration_ms.max(0) as f64);
    }
    let repeats = -((-duration_ms).div_euclid(len));
    clip.repeat(repeats).slice(None, Some(duration_ms as f64))
}

/// A label / timeline span: the widest Python label tuple, unset fields `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Label {
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
    pub ramp_in_s: Option<Num>,
    pub ramp_out_s: Option<Num>,
    pub play_duration: Option<f64>,
    pub snippet: Option<String>,
    pub volume_pct: Option<Num>,
    pub seq: Option<i64>,
    pub tts_model: Option<String>,
}

fn label_text(plan: &StemPlan, fallback: &str) -> String {
    plan.text
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| plan.direction_type.clone().filter(|t| !t.is_empty()))
        .unwrap_or_else(|| fallback.to_string())
}

/// The end of a looped span: the next cue of the same kind after this one.
fn loop_end(plan: &StemPlan, start_ms: i64, cues: &[(i64, i64)], total_ms: i64) -> i64 {
    for &(cue_ms, cue_seq) in cues {
        if cue_seq > plan.seq && cue_ms > start_ms {
            return cue_ms.min(total_ms);
        }
    }
    total_ms
}

fn cues_for(plans: &[StemPlan], timeline: &IndexMap<i64, i64>, dt: &str) -> Vec<(i64, i64)> {
    let mut cues: Vec<(i64, i64)> = plans
        .iter()
        .filter(|p| p.dt() == Some(dt))
        .map(|p| (*timeline.get(&p.seq).unwrap_or(&0), p.seq))
        .collect();
    cues.sort_by_key(|c| c.0);
    cues
}

fn sorted_of<'a>(plans: &'a [StemPlan], dt: &str) -> Vec<&'a StemPlan> {
    by_seq(plans)
        .into_iter()
        .filter(|p| p.dt() == Some(dt))
        .collect()
}

/// What distinguishes the two looped layers: which plans, which skip rule,
/// which fallback label, and how a corrupt stem is named in the warning.
struct LoopSpec {
    dt: &'static str,
    what: &'static str,
    skip: fn(&StemPlan) -> bool,
    text: fn(&StemPlan) -> String,
}

fn amb_skip(p: &StemPlan) -> bool {
    p.filepath.is_empty()
}

fn amb_text(p: &StemPlan) -> String {
    label_text(p, "AMBIENCE")
}

fn vf_skip(p: &StemPlan) -> bool {
    p.filepath.is_empty() || p.text.as_deref().unwrap_or("").contains("DISENGAGES")
}

fn vf_text(p: &StemPlan) -> String {
    p.text
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "VINTAGE FILTER".into())
}

const AMBIENCE_LOOP: LoopSpec = LoopSpec {
    dt: "AMBIENCE",
    what: "ambience",
    skip: amb_skip,
    text: amb_text,
};

const VINTAGE_LOOP: LoopSpec = LoopSpec {
    dt: "VINTAGE FILTER",
    what: "vintage filter",
    skip: vf_skip,
    text: vf_text,
};

/// Shared loop for the ambience and vintage-filter layers. With `render`
/// off it computes only the labels (`compute_*_labels`), without decoding.
fn build_looped_layer(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
    level_db: f64,
    spec: &LoopSpec,
    render: bool,
) -> anyhow::Result<(Segment, Vec<Label>)> {
    let mut layer = Segment::silent(total_ms as f64);
    let mut labels = Vec::new();
    let own = sorted_of(plans, spec.dt);
    if own.is_empty() {
        return Ok((layer, labels));
    }
    // Ambience ends at any AMBIENCE cue in the episode; the crackle only at
    // its own markers. Both lists are ordered by cue time.
    let cues: Vec<(i64, i64)> = if spec.dt == "AMBIENCE" {
        cues_for(plans, timeline, spec.dt)
    } else {
        let mut c: Vec<(i64, i64)> = own
            .iter()
            .map(|p| (*timeline.get(&p.seq).unwrap_or(&0), p.seq))
            .collect();
        c.sort_by_key(|x| x.0);
        c
    };
    for plan in own {
        if (spec.skip)(plan) {
            continue;
        }
        let start_ms = *timeline.get(&plan.seq).unwrap_or(&0);
        if start_ms >= total_ms {
            continue;
        }
        let end_ms = loop_end(plan, start_ms, &cues, total_ms);
        let needed = end_ms - start_ms;
        if needed <= 0 {
            continue;
        }
        if render {
            let clip = match Segment::from_file(Path::new(&plan.filepath)) {
                Ok(c) => c,
                Err(e) => {
                    log::warning(&format!(
                        "Skipping corrupt {} stem: {} ({e})",
                        spec.what, plan.filepath
                    ));
                    continue;
                }
            };
            let ri = plan
                .ramp_in_seconds
                .filter(|n| n.truthy())
                .map_or(0, Num::to_ms);
            let ro = plan
                .ramp_out_seconds
                .filter(|n| n.truthy())
                .map_or(0, Num::to_ms);
            let looped = if plan.loop_ {
                loop_clip(&clip, needed)
            } else {
                clip.slice(None, Some(needed as f64))
            };
            let looped = apply_clip_effects(looped, plan.volume_percentage, ri, ro, level_db);
            layer = layer.overlay(looped, start_ms);
        }
        labels.push(Label {
            start_s: start_ms as f64 / 1000.0,
            end_s: end_ms as f64 / 1000.0,
            text: (spec.text)(plan),
            ramp_in_s: plan.ramp_in_seconds,
            ramp_out_s: plan.ramp_out_seconds,
            volume_pct: plan.volume_percentage,
            seq: Some(plan.seq),
            ..Label::default()
        });
    }
    Ok((layer, labels))
}

pub fn build_ambience_layer(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
    level_db: f64,
) -> anyhow::Result<(Segment, Vec<Label>)> {
    build_looped_layer(plans, timeline, total_ms, level_db, &AMBIENCE_LOOP, true)
}

pub fn build_vintage_filter_layer(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
    level_db: f64,
) -> anyhow::Result<(Segment, Vec<Label>)> {
    build_looped_layer(plans, timeline, total_ms, level_db, &VINTAGE_LOOP, true)
}

pub fn build_music_layer(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
    level_db: f64,
    include_foreground_override: bool,
) -> anyhow::Result<(Segment, Vec<Label>)> {
    let mut layer = Segment::silent(total_ms as f64);
    let mut labels = Vec::new();
    for plan in by_seq(plans) {
        if plan.dt() != Some("MUSIC") || (plan.foreground_override && !include_foreground_override)
        {
            continue;
        }
        let start_ms = *timeline.get(&plan.seq).unwrap_or(&0);
        if start_ms >= total_ms {
            continue;
        }
        let mut clip = load(&plan.filepath)?;
        if let (Some(pd), false) = (plan.play_duration, plan.pre_trimmed) {
            clip = trim_to_pct(&clip, pd);
        }
        let ri = plan
            .ramp_in_seconds
            .filter(|n| n.truthy())
            .map_or(0, Num::to_ms);
        let ro = plan
            .ramp_out_seconds
            .filter(|n| n.truthy())
            .map_or(0, Num::to_ms);
        let clip = apply_clip_effects(clip, plan.volume_percentage, ri, ro, level_db);
        let clip_len = clip.len_ms();
        layer = layer.overlay(clip, start_ms);
        labels.push(Label {
            start_s: start_ms as f64 / 1000.0,
            end_s: (start_ms + clip_len) as f64 / 1000.0,
            text: label_text(plan, "MUSIC"),
            ramp_in_s: plan.ramp_in_seconds,
            ramp_out_s: plan.ramp_out_seconds,
            play_duration: plan.play_duration,
            volume_pct: plan.volume_percentage,
            seq: Some(plan.seq),
            ..Label::default()
        });
    }
    Ok((layer, labels))
}

pub fn build_dialogue_layer(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
    cast: &IndexMap<String, Voice>,
    vintage_scenes: &[String],
) -> anyhow::Result<(Segment, Vec<Label>)> {
    let mut layer = Segment::silent(total_ms as f64);
    let mut labels = Vec::new();
    let span_map = span_treatments(plans);
    warn_missing_treatment_codecs(cast, &span_map);
    for plan in by_seq(plans) {
        if !plan.is_dialogue() {
            continue;
        }
        let start_ms = *timeline.get(&plan.seq).unwrap_or(&0);
        let segment = load(&plan.filepath)?;
        let segment = treat_dialogue(segment, plan, cast, &span_map, vintage_scenes, true)?;
        let end_ms = start_ms + segment.len_ms();
        labels.push(Label {
            start_s: start_ms as f64 / 1000.0,
            end_s: end_ms as f64 / 1000.0,
            text: plan.speaker_raw(),
            seq: Some(plan.seq),
            ..Label::default()
        });
        layer = layer.overlay(segment, start_ms);
    }
    Ok((layer, labels))
}

pub fn build_sfx_layer(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
) -> anyhow::Result<(Segment, Vec<Label>)> {
    let mut layer = Segment::silent(total_ms as f64);
    let mut labels = Vec::new();
    for plan in by_seq(plans) {
        if !matches!(plan.dt(), Some("SFX") | Some("BEAT")) {
            continue;
        }
        let start_ms = *timeline.get(&plan.seq).unwrap_or(&0);
        let mut segment = load(&plan.filepath)?;
        if let (Some(pd), false) = (plan.play_duration, plan.pre_trimmed) {
            segment = trim_to_pct(&segment, pd);
        }
        if let Some(v) = plan.volume_percentage {
            segment = segment.gain(volume_pct_to_db(v.f()));
        }
        let len = segment.len_ms();
        layer = layer.overlay(segment, start_ms);
        labels.push(Label {
            start_s: start_ms as f64 / 1000.0,
            end_s: (start_ms + len) as f64 / 1000.0,
            text: label_text(plan, "SFX"),
            play_duration: plan.play_duration,
            volume_pct: plan.volume_percentage,
            seq: Some(plan.seq),
            ..Label::default()
        });
    }
    Ok((layer, labels))
}

/// `build_foreground_timeline_only` — header durations, no decoding.
pub fn build_foreground_timeline_only(
    plans: &[StemPlan],
    gap_ms: i64,
) -> anyhow::Result<(i64, IndexMap<i64, i64>)> {
    let mut timeline = IndexMap::new();
    let mut current = 0i64;
    for plan in by_seq(plans) {
        timeline.insert(plan.seq, current);
        if plan.is_background() {
            continue;
        }
        current += mp3_duration_ms(&plan.filepath)? + gap_ms;
    }
    Ok((current, timeline))
}

/// `compute_dialogue_labels`.
pub fn compute_dialogue_labels(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
) -> anyhow::Result<Vec<Label>> {
    let mut labels = Vec::new();
    for plan in by_seq(plans) {
        if !plan.is_dialogue() {
            continue;
        }
        let start_ms = *timeline.get(&plan.seq).unwrap_or(&0);
        let end_ms = start_ms + mp3_duration_ms(&plan.filepath)?;
        let text = plan.text.clone().unwrap_or_default();
        let words: Vec<&str> = text.split_whitespace().collect();
        let snippet = if words.is_empty() {
            None
        } else {
            Some(words.iter().take(5).copied().collect::<Vec<_>>().join(" "))
        };
        labels.push(Label {
            start_s: start_ms as f64 / 1000.0,
            end_s: end_ms as f64 / 1000.0,
            text: plan.speaker_raw(),
            snippet,
            seq: Some(plan.seq),
            tts_model: plan.tts_model.clone(),
            ..Label::default()
        });
    }
    Ok(labels)
}

pub fn compute_ambience_labels(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
) -> Vec<Label> {
    build_looped_layer(plans, timeline, total_ms, 0.0, &AMBIENCE_LOOP, false)
        .map(|(_, l)| l)
        .unwrap_or_default()
}

pub fn compute_vintage_filter_labels(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
) -> Vec<Label> {
    build_looped_layer(plans, timeline, total_ms, 0.0, &VINTAGE_LOOP, false)
        .map(|(_, l)| l)
        .unwrap_or_default()
}

pub fn compute_music_labels(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
    include_foreground_override: bool,
) -> anyhow::Result<Vec<Label>> {
    let mut labels = Vec::new();
    for plan in by_seq(plans) {
        if plan.dt() != Some("MUSIC") || (plan.foreground_override && !include_foreground_override)
        {
            continue;
        }
        let start_ms = *timeline.get(&plan.seq).unwrap_or(&0);
        if start_ms >= total_ms {
            continue;
        }
        let mut duration = mp3_duration_ms(&plan.filepath)?;
        if let (Some(pd), false) = (plan.play_duration, plan.pre_trimmed) {
            duration = ((duration as f64 * pd / 100.0) as i64).max(1);
        }
        labels.push(Label {
            start_s: start_ms as f64 / 1000.0,
            end_s: (start_ms + duration) as f64 / 1000.0,
            text: label_text(plan, "MUSIC"),
            ramp_in_s: plan.ramp_in_seconds,
            ramp_out_s: plan.ramp_out_seconds,
            play_duration: plan.play_duration,
            volume_pct: plan.volume_percentage,
            seq: Some(plan.seq),
            ..Label::default()
        });
    }
    Ok(labels)
}

pub fn compute_sfx_labels(
    plans: &[StemPlan],
    timeline: &IndexMap<i64, i64>,
) -> anyhow::Result<Vec<Label>> {
    let mut labels = Vec::new();
    for plan in by_seq(plans) {
        if !matches!(plan.dt(), Some("SFX") | Some("BEAT")) {
            continue;
        }
        let start_ms = *timeline.get(&plan.seq).unwrap_or(&0);
        let mut duration = mp3_duration_ms(&plan.filepath)?;
        if let (Some(pd), false) = (plan.play_duration, plan.pre_trimmed) {
            duration = ((duration as f64 * pd / 100.0) as i64).max(1);
        }
        labels.push(Label {
            start_s: start_ms as f64 / 1000.0,
            end_s: (start_ms + duration) as f64 / 1000.0,
            text: label_text(plan, "SFX"),
            play_duration: plan.play_duration,
            volume_pct: plan.volume_percentage,
            seq: Some(plan.seq),
            ..Label::default()
        });
    }
    Ok(labels)
}

/// `derive_structure_bands(entries_index, timeline, total_ms, key)`.
pub fn derive_structure_bands(
    index: &IndexMap<i64, Map<String, Value>>,
    timeline: &IndexMap<i64, i64>,
    total_ms: i64,
    key: &str,
) -> Vec<Label> {
    let mut seqs: Vec<i64> = index.keys().copied().collect();
    seqs.sort();
    let mut groups: Vec<(String, i64)> = Vec::new();
    let mut current: Option<Value> = None;
    for seq in seqs {
        let value = index[&seq].get(key).cloned().unwrap_or(Value::Null);
        let value_opt = if value.is_null() {
            None
        } else {
            Some(value.clone())
        };
        if value_opt != current {
            current = value_opt.clone();
            if let Some(Value::String(s)) = &value_opt {
                if !s.is_empty() {
                    groups.push((s.clone(), -1));
                }
            }
        }
        let truthy = matches!(&value_opt, Some(Value::String(s)) if !s.is_empty());
        if truthy {
            if let Some(last) = groups.last_mut() {
                if last.1 < 0 {
                    if let Some(&t) = timeline.get(&seq) {
                        last.1 = t;
                    }
                }
            }
        }
    }
    let placed: Vec<(String, i64)> = groups.into_iter().filter(|g| g.1 >= 0).collect();
    let mut bands = Vec::new();
    for (i, (label, start)) in placed.iter().enumerate() {
        let end = placed.get(i + 1).map(|p| p.1).unwrap_or(total_ms);
        if end > *start {
            bands.push(Label {
                start_s: *start as f64 / 1000.0,
                end_s: end as f64 / 1000.0,
                text: label.clone(),
                ..Label::default()
            });
        }
    }
    bands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_extraction() {
        assert_eq!(extract_seq("stems/S01E01/003_cold-open_adam.mp3"), Some(3));
        assert_eq!(extract_seq("n002_preamble_tina.mp3"), Some(-2));
        assert_eq!(extract_seq("preamble_tina.mp3"), None);
        assert_eq!(extract_seq("1_0_x.mp3"), Some(1));
    }

    #[test]
    fn span_scope_reads_the_speaker() {
        assert_eq!(
            span_scope(Some("PHONE FILTER: ENGAGES DEZ"), "PHONE FILTER"),
            Some("dez".into())
        );
        assert_eq!(
            span_scope(Some("PHONE FILTER: ENGAGES"), "PHONE FILTER"),
            None
        );
        assert_eq!(
            span_marker(Some("VINTAGE FILTER DISENGAGES")),
            Some("DISENGAGES")
        );
    }

    #[test]
    fn cast_filter_names_normalise() {
        assert_eq!(cast_filter_names(&Filter::Bool(true)), vec!["phone"]);
        assert_eq!(
            cast_filter_names(&Filter::Str(" Vintage, PHONE ,".into())),
            vec!["vintage", "phone"]
        );
        assert!(cast_filter_names(&Filter::Bool(false)).is_empty());
    }

    #[test]
    fn loop_clip_fills_exactly() {
        let clip = Segment::new([1, 0].repeat(441), 2, 44100, 1); // 10 ms
        let l = loop_clip(&clip, 25);
        assert_eq!(l.len_ms(), 25);
    }
}

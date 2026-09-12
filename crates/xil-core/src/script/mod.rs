//! Script parsing: the markdown production script → structured entries.
//! Port of `XILP001_script_parser.py`'s library half.

pub mod hints;
pub mod sections;
pub mod speakers;
pub mod text;

use std::path::Path;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;

use crate::pyjson::float_repr;
use crate::workspace::{episode_tag, show_slug};
use hints::{parse_direction_hint, Hints};
use sections::{get_section_map, match_section};
use speakers::{extract_cast_from_script, load_speakers, try_match_speaker, CastEntry, Speakers};
use text::*;

static EPISODE_N: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Episode\s+(\d+)").unwrap());
static SEASON_OR_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:Season\s+\d+|Episode\s+\d+)").unwrap());
static SEASON_N: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Season\s+(\d+)").unwrap());
static EP_REST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Episode\s+\d+[:\s]+(.*)").unwrap());
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).unwrap());
static ARC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\bArc:\s*"([^"]+)""#).unwrap());

/// One parsed script entry. Field order matches the pydantic model, which
/// is the order they are serialized in.
#[derive(Clone, Debug)]
pub struct Entry {
    pub seq: i64,
    pub kind: &'static str,
    pub section: Option<String>,
    pub scene: Option<String>,
    pub speaker: Option<String>,
    pub direction: Option<String>,
    pub text: String,
    pub direction_type: Option<&'static str>,
    pub sfx_source: Option<String>,
    pub sfx_overrides: IndexMap<String, f64>,
}

impl Entry {
    /// Serialize with every model field present, as `model_dump()` does.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("seq".into(), Value::from(self.seq));
        m.insert("type".into(), Value::from(self.kind));
        m.insert("section".into(), opt_str(&self.section));
        m.insert("scene".into(), opt_str(&self.scene));
        m.insert("speaker".into(), opt_str(&self.speaker));
        m.insert("direction".into(), opt_str(&self.direction));
        m.insert("text".into(), Value::String(self.text.clone()));
        m.insert(
            "direction_type".into(),
            self.direction_type.map(Value::from).unwrap_or(Value::Null),
        );
        m.insert("sfx_source".into(), opt_str(&self.sfx_source));
        m.insert(
            "sfx_overrides".into(),
            if self.sfx_overrides.is_empty() {
                Value::Null
            } else {
                Value::Object(
                    self.sfx_overrides
                        .iter()
                        .map(|(k, v)| (k.clone(), json_f64(*v)))
                        .collect(),
                )
            },
        );
        Value::Object(m)
    }
}

fn opt_str(v: &Option<String>) -> Value {
    v.clone().map(Value::String).unwrap_or(Value::Null)
}

/// A float that serializes the way Python writes it (`20.0`, not `20`).
fn json_f64(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Header metadata: show, season, episode, title, season title.
#[derive(Debug, PartialEq)]
pub struct Header {
    pub show: String,
    pub season: Option<i64>,
    pub episode: i64,
    pub title: String,
    pub season_title: Option<String>,
}

/// Parse the first non-empty script line. `None` when it carries no
/// `Episode N`.
pub fn parse_script_header(line: &str) -> Option<Header> {
    let ep = EPISODE_N.captures(line)?;
    let show = match SEASON_OR_EPISODE.find(line) {
        Some(m) => line[..m.start()].trim().to_string(),
        None => "Unknown Show".to_string(),
    };
    let season = SEASON_N.captures(line).and_then(|c| c[1].parse().ok());
    let episode: i64 = ep[1].parse().unwrap_or(1);
    let title = match EP_REST.captures(line) {
        Some(c) => {
            let rest = c[1].to_string();
            match QUOTED.captures(&rest) {
                Some(q) => q[1].to_string(),
                None => rest.trim().to_string(),
            }
        }
        None => String::new(),
    };
    let season_title = ARC.captures(line).map(|c| c[1].to_string());
    Some(Header {
        show,
        season,
        episode,
        title,
        season_title,
    })
}

/// Everything `parse_script` produces.
pub struct Parsed {
    pub show: String,
    pub season: Option<i64>,
    pub episode: i64,
    pub title: String,
    pub season_title: Option<String>,
    pub source_file: String,
    pub entries: Vec<Entry>,
    /// `(1-based line number, raw line, entry index)` for the debug CSV.
    pub debug_line_map: Vec<(usize, String, usize)>,
}

impl Parsed {
    pub fn tag(&self) -> String {
        episode_tag(self.season, self.episode)
    }

    /// The full `model_dump()` shape, ready for the JSON writer.
    pub fn to_json(&self) -> Value {
        let dialogue: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.kind == "dialogue")
            .collect();
        let tts_chars: usize = dialogue.iter().map(|e| e.text.chars().count()).sum();

        let mut speakers: Vec<String> = dialogue.iter().filter_map(|e| e.speaker.clone()).collect();
        speakers.sort();
        speakers.dedup();
        let mut sections: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| e.section.clone())
            .filter(|s| !s.is_empty())
            .collect();
        sections.sort();
        sections.dedup();

        let mut stats = Map::new();
        stats.insert("total_entries".into(), Value::from(self.entries.len()));
        stats.insert("dialogue_lines".into(), Value::from(dialogue.len()));
        stats.insert(
            "direction_lines".into(),
            Value::from(
                self.entries
                    .iter()
                    .filter(|e| e.kind == "direction")
                    .count(),
            ),
        );
        stats.insert("characters_for_tts".into(), Value::from(tts_chars));
        stats.insert(
            "speakers".into(),
            Value::Array(speakers.into_iter().map(Value::String).collect()),
        );
        stats.insert(
            "sections".into(),
            Value::Array(sections.into_iter().map(Value::String).collect()),
        );

        let mut m = Map::new();
        m.insert("show".into(), Value::String(self.show.clone()));
        m.insert(
            "season".into(),
            self.season.map(Value::from).unwrap_or(Value::Null),
        );
        m.insert("episode".into(), Value::from(self.episode));
        m.insert("title".into(), Value::String(self.title.clone()));
        m.insert("season_title".into(), opt_str(&self.season_title));
        m.insert(
            "source_file".into(),
            Value::String(self.source_file.clone()),
        );
        m.insert(
            "entries".into(),
            Value::Array(self.entries.iter().map(Entry::to_json).collect()),
        );
        m.insert("stats".into(), Value::Object(stats));
        Value::Object(m)
    }
}

/// Options the caller supplies; `season`/`season_title` are the
/// `project.json` fallbacks already resolved.
pub struct ParseOpts<'a> {
    pub project_type: &'a str,
    pub speakers_path: Option<&'a Path>,
    /// Applied when the script header declares no season.
    pub season_fallback: Option<i64>,
    /// Applied when the script header declares no arc.
    pub season_title_fallback: Option<String>,
}

/// Parse a markdown production script into sequence-numbered entries.
///
/// `warn` receives the messages Python logs for malformed pipe-hints.
pub fn parse_script(
    raw: &str,
    source_file: &str,
    opts: &ParseOpts,
    warn: &mut dyn FnMut(String),
) -> Parsed {
    let section_map = get_section_map(opts.project_type);

    // NFC first: an accented character can arrive decomposed depending on the
    // editor, which would make an otherwise identical cue a different string
    // from its config key and its asset filename.
    let raw: String = raw.nfc().collect();
    let raw = strip_markdown_escapes(&raw);
    let raw = strip_markdown_formatting(&raw);
    let lines: Vec<String> = raw.split('\n').map(str::to_string).collect();

    let mut entries: Vec<Entry> = Vec::new();
    let mut debug_line_map: Vec<(usize, String, usize)> = Vec::new();
    let mut seq: i64 = 0;
    let mut current_section: Option<String> = None;
    let mut current_scene: Option<String> = None;
    let mut in_metadata = false;
    let mut last_dialogue_idx: Option<usize> = None;
    let mut pending_speaker: Option<(String, Option<String>)> = None;

    // The header is the first NON-EMPTY line: a stray leading blank must not
    // send us to the "Unknown Show / S01E01" defaults.
    let mut hdr_idx = 0;
    while hdr_idx < lines.len() && lines[hdr_idx].trim().is_empty() {
        hdr_idx += 1;
    }
    let first_line = lines.get(hdr_idx).map(|l| l.trim()).unwrap_or("");
    let header = if first_line.is_empty() {
        None
    } else {
        parse_script_header(first_line)
    };
    let mut start = 0;
    let (show, mut season, episode, title, mut season_title) = match header {
        Some(h) => {
            start = hdr_idx + 1;
            (h.show, h.season, h.episode, h.title, h.season_title)
        }
        None => ("Unknown Show".to_string(), None, 1, String::new(), None),
    };

    let script_slug = show_slug(&show);
    if season.is_none() {
        season = opts.season_fallback;
    }
    if season_title.is_none() {
        season_title = opts.season_title_fallback.clone();
    }

    // The CAST block seeds speaker recognition, then start advances past it.
    let cast_entries: Vec<CastEntry> = extract_cast_from_script(&lines[start.min(lines.len())..]);
    let mut in_cast = false;
    for (i, raw) in lines.iter().enumerate().skip(start) {
        let line = raw.trim();
        if line == "CAST:" {
            in_cast = true;
            continue;
        }
        if in_cast && (line == "===" || (!line.is_empty() && !line.starts_with('*'))) {
            start = i;
            break;
        }
    }

    let speakers = load_speakers(opts.speakers_path, &cast_entries);

    for (i, raw_line) in lines.iter().enumerate().skip(start) {
        let line = raw_line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        // Multi-line dialogue: a speaker name is waiting for its text.
        if let Some((p_key, p_direction)) = pending_speaker.clone() {
            if line.starts_with('(') && line.ends_with(')') {
                let d = line[1..line.len() - 1].trim().to_string();
                pending_speaker = Some((p_key, Some(d)));
                continue;
            }
            // A direction between the name and the dialogue is emitted, and
            // the pending speaker stays alive for the next line.
            if is_stage_direction(&line) {
                emit_directions(
                    &line,
                    i,
                    raw_line,
                    &script_slug,
                    &current_section,
                    &current_scene,
                    &mut seq,
                    &mut entries,
                    &mut debug_line_map,
                    warn,
                );
                continue;
            }
            seq += 1;
            entries.push(Entry {
                seq,
                kind: "dialogue",
                section: current_section.clone(),
                scene: current_scene.clone(),
                speaker: Some(p_key),
                direction: p_direction,
                text: line.clone(),
                direction_type: None,
                sfx_source: None,
                sfx_overrides: IndexMap::new(),
            });
            debug_line_map.push((i + 1, raw_line.to_string(), entries.len() - 1));
            last_dialogue_idx = Some(entries.len() - 1);
            pending_speaker = None;
            continue;
        }

        if is_divider(&line) {
            continue;
        }
        if is_metadata_section(&line) {
            in_metadata = true;
            continue;
        }
        if in_metadata {
            continue;
        }
        if line.starts_with("END OF EPISODE") || line.starts_with("END OF PRODUCTION") {
            break;
        }

        if let Some(slug) = match_section(&line, &section_map) {
            current_section = Some(slug);
            current_scene = None;
            seq += 1;
            entries.push(Entry {
                seq,
                kind: "section_header",
                section: current_section.clone(),
                scene: None,
                speaker: None,
                direction: None,
                text: line.trim().to_string(),
                direction_type: None,
                sfx_source: None,
                sfx_overrides: IndexMap::new(),
            });
            debug_line_map.push((i + 1, raw_line.to_string(), entries.len() - 1));
            last_dialogue_idx = None;
            continue;
        }

        if is_scene_header(&line) {
            let (scene_num, _) = parse_scene_header(&line);
            if let Some(n) = scene_num {
                current_scene = Some(format!("scene-{n}"));
            }
            seq += 1;
            entries.push(Entry {
                seq,
                kind: "scene_header",
                section: current_section.clone(),
                scene: current_scene.clone(),
                speaker: None,
                direction: None,
                text: strip_brackets(&line),
                direction_type: None,
                sfx_source: None,
                sfx_overrides: IndexMap::new(),
            });
            debug_line_map.push((i + 1, raw_line.to_string(), entries.len() - 1));
            emit_directions(
                &line,
                i,
                raw_line,
                &script_slug,
                &current_section,
                &current_scene,
                &mut seq,
                &mut entries,
                &mut debug_line_map,
                warn,
            );
            last_dialogue_idx = None;
            continue;
        }

        if is_stage_direction(&line) {
            emit_directions(
                &line,
                i,
                raw_line,
                &script_slug,
                &current_section,
                &current_scene,
                &mut seq,
                &mut entries,
                &mut debug_line_map,
                warn,
            );
            last_dialogue_idx = None;
            continue;
        }

        if let Some(m) = try_match_speaker(&line, &speakers) {
            if !m.text.is_empty() {
                seq += 1;
                entries.push(Entry {
                    seq,
                    kind: "dialogue",
                    section: current_section.clone(),
                    scene: current_scene.clone(),
                    speaker: Some(m.key),
                    direction: m.direction,
                    text: m.text,
                    direction_type: None,
                    sfx_source: None,
                    sfx_overrides: IndexMap::new(),
                });
                debug_line_map.push((i + 1, raw_line.to_string(), entries.len() - 1));
                last_dialogue_idx = Some(entries.len() - 1);
            } else {
                // Speaker name alone: direction and text follow on later lines.
                pending_speaker = Some((m.key, m.direction));
                last_dialogue_idx = None;
            }
            continue;
        }

        // Continuation of the previous dialogue line.
        if let Some(idx) = last_dialogue_idx {
            if entries[idx].kind == "dialogue" {
                // A standalone parenthetical is an acting note, not speech.
                if line.starts_with('(') && line.ends_with(')') {
                    continue;
                }
                entries[idx].text.push(' ');
                entries[idx].text.push_str(&line);
                continue;
            }
        }
        // Anything else is skipped silently.
    }

    Parsed {
        show,
        season,
        episode,
        title,
        season_title,
        source_file: source_file.to_string(),
        entries,
        debug_line_map,
    }
}

/// Emit one direction entry per recognized bracket on a line.
#[allow(clippy::too_many_arguments)]
fn emit_directions(
    line: &str,
    i: usize,
    raw_line: &str,
    slug: &str,
    section: &Option<String>,
    scene: &Option<String>,
    seq: &mut i64,
    entries: &mut Vec<Entry>,
    debug_line_map: &mut Vec<(usize, String, usize)>,
    warn: &mut dyn FnMut(String),
) {
    for bracket_text in find_brackets(line) {
        let Hints {
            clean,
            source,
            overrides,
        } = parse_direction_hint(bracket_text.trim(), slug, warn);
        let Some(direction_type) = classify_direction(&clean) else {
            // An acting note in square brackets, not a technical cue.
            continue;
        };
        *seq += 1;
        entries.push(Entry {
            seq: *seq,
            kind: "direction",
            section: section.clone(),
            scene: scene.clone(),
            speaker: None,
            direction: None,
            text: clean,
            direction_type: Some(direction_type),
            sfx_source: source,
            sfx_overrides: overrides,
        });
        debug_line_map.push((i + 1, raw_line.to_string(), entries.len() - 1));
    }
}

/// Per-speaker dialogue distribution, sorted by lines descending.
pub struct SpeakerStat {
    pub speaker: String,
    pub lines: usize,
    pub words: usize,
    pub chars: usize,
    pub pct_lines: f64,
    pub pct_words: f64,
    pub pct_chars: f64,
}

pub fn compute_speaker_stats(entries: &[Entry]) -> Vec<SpeakerStat> {
    let mut accum: IndexMap<String, (usize, usize, usize)> = IndexMap::new();
    for e in entries.iter().filter(|e| e.kind == "dialogue") {
        let sp = e.speaker.clone().unwrap_or_default();
        let slot = accum.entry(sp).or_insert((0, 0, 0));
        slot.0 += 1;
        slot.1 += e.text.split_whitespace().count();
        slot.2 += e.text.chars().count();
    }
    let total_lines = accum.values().map(|v| v.0).sum::<usize>().max(1) as f64;
    let total_words = accum.values().map(|v| v.1).sum::<usize>().max(1) as f64;
    let total_chars = accum.values().map(|v| v.2).sum::<usize>().max(1) as f64;

    let mut out: Vec<SpeakerStat> = accum
        .into_iter()
        .map(|(speaker, (lines, words, chars))| SpeakerStat {
            speaker,
            lines,
            words,
            chars,
            pct_lines: py_round1(lines as f64 / total_lines * 100.0),
            pct_words: py_round1(words as f64 / total_words * 100.0),
            pct_chars: py_round1(chars as f64 / total_chars * 100.0),
        })
        .collect();
    // Python's sort is stable, so equal line counts keep insertion order.
    out.sort_by_key(|a| std::cmp::Reverse(a.lines));
    out
}

/// `round(x, 1)` — banker's rounding on the decimal representation, as
/// CPython does it.
fn py_round1(x: f64) -> f64 {
    let s = format!("{:.1}", x);
    s.parse().unwrap_or(x)
}

/// `float_repr` re-exported for callers writing stat values into JSON.
pub fn repr_f64(v: f64) -> String {
    float_repr(v)
}

/// Speaker display order used by the cast skeleton.
pub fn speakers_for_cast(speakers: &Speakers) -> IndexMap<String, String> {
    speakers::key_to_display(speakers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Parsed {
        let opts = ParseOpts {
            project_type: "podcast",
            speakers_path: Some(Path::new("/nonexistent")),
            season_fallback: None,
            season_title_fallback: None,
        };
        parse_script(src, "t.md", &opts, &mut |_| {})
    }

    #[test]
    fn header_shapes() {
        let h =
            parse_script_header(r#"THE 413 Season 1: Episode 2: "The Booth" Arc: "Holiday Shift""#)
                .unwrap();
        assert_eq!(
            h,
            Header {
                show: "THE 413".into(),
                season: Some(1),
                episode: 2,
                title: "The Booth".into(),
                season_title: Some("Holiday Shift".into()),
            }
        );
        let bare = parse_script_header("My Show Episode 7: Untitled Thing").unwrap();
        assert_eq!(
            (bare.season, bare.episode, bare.title.as_str()),
            (None, 7, "Untitled Thing")
        );
        assert!(parse_script_header("Just a line").is_none());
    }

    #[test]
    fn leading_blank_lines_do_not_defeat_the_header() {
        let p = parse("\n\n\nMy Show Episode 3: \"T\"\n\nCOLD OPEN\n");
        assert_eq!((p.show.as_str(), p.episode), ("My Show", 3));
    }

    #[test]
    fn dialogue_direction_and_continuation() {
        let p = parse(
            "S Episode 1: \"T\"\n\nCOLD OPEN\n\nSCENE 1: ROOM\n\n\
             [SFX: DOOR OPENS | door.mp3]\n\n\
             ADAM (quietly)\nFirst line.\nsecond line continues.\n(beat)\nthird line.\n",
        );
        let kinds: Vec<&str> = p.entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec!["section_header", "scene_header", "direction", "dialogue"]
        );
        let d = &p.entries[2];
        assert_eq!(d.text, "SFX: DOOR OPENS");
        assert_eq!(d.sfx_source.as_deref(), Some("SFX/s/door.mp3"));
        assert_eq!(d.direction_type, Some("SFX"));
        let dial = &p.entries[3];
        assert_eq!(dial.speaker.as_deref(), Some("adam"));
        assert_eq!(dial.direction.as_deref(), Some("quietly"));
        assert_eq!(
            dial.text, "First line. second line continues. third line.",
            "(beat) is dropped"
        );
        assert_eq!(dial.scene.as_deref(), Some("scene-1"));
        assert_eq!(dial.section.as_deref(), Some("cold-open"));
    }

    #[test]
    fn direction_between_speaker_and_text_keeps_the_speaker_pending() {
        let p = parse("S Episode 1: \"T\"\n\nADAM\n[BEAT]\nThe line.\n");
        let kinds: Vec<&str> = p.entries.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec!["direction", "dialogue"]);
        assert_eq!(p.entries[1].speaker.as_deref(), Some("adam"));
        assert_eq!(p.entries[1].text, "The line.");
        assert_eq!(
            p.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn standalone_parenthetical_becomes_the_direction() {
        let p = parse("S Episode 1: \"T\"\n\nADAM\n(shouting)\nHey!\n");
        assert_eq!(p.entries[0].direction.as_deref(), Some("shouting"));
        assert_eq!(p.entries[0].text, "Hey!");
    }

    #[test]
    fn scene_header_emits_its_embedded_directions() {
        let p = parse("S Episode 1: \"T\"\n\nSCENE 2A: THE ALLEY [AMBIENCE: rain]\n");
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.entries[0].text, "SCENE 2A: THE ALLEY");
        assert_eq!(p.entries[0].scene.as_deref(), Some("scene-2A"));
        assert_eq!(p.entries[1].text, "AMBIENCE: rain");
        assert_eq!(p.entries[1].direction_type, Some("AMBIENCE"));
    }

    #[test]
    fn parsing_stops_at_end_markers_and_metadata() {
        let p = parse("S Episode 1: \"T\"\n\nADAM Yes.\n\nEND OF EPISODE\n\nADAM No.\n");
        assert_eq!(p.entries.len(), 1);
        let m = parse("S Episode 1: \"T\"\n\nADAM Yes.\n\nPRODUCTION NOTES:\n\nADAM No.\n");
        assert_eq!(m.entries.len(), 1);
    }

    #[test]
    fn unrecognized_brackets_are_skipped_without_consuming_a_seq() {
        let p = parse("S Episode 1: \"T\"\n\n[drawn out]\n[SFX: X]\n");
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].seq, 1);
    }

    #[test]
    fn stats_count_characters_not_bytes() {
        let p = parse("S Episode 1: \"T\"\n\nADAM Café.\n");
        let j = p.to_json();
        assert_eq!(j["stats"]["characters_for_tts"], 5);
        assert_eq!(j["stats"]["dialogue_lines"], 1);
        assert_eq!(j["stats"]["speakers"], serde_json::json!(["adam"]));
    }

    #[test]
    fn entry_json_carries_every_model_field() {
        let p = parse("S Episode 1: \"T\"\n\n[SFX: X | a.mp3 | play_volume_pct=20]\n");
        let e = &p.to_json()["entries"][0];
        assert_eq!(e["sfx_source"], "SFX/s/a.mp3");
        assert_eq!(
            e["sfx_overrides"],
            serde_json::json!({"volume_percentage": 20.0})
        );
        assert_eq!(e["speaker"], Value::Null);
        assert!(e.as_object().unwrap().contains_key("direction"));
    }

    #[test]
    fn speaker_stats_sorted_and_rounded() {
        let p = parse("S Episode 1: \"T\"\n\nADAM one two three.\nMAYA hi.\nADAM four.\n");
        let s = compute_speaker_stats(&p.entries);
        assert_eq!(s[0].speaker, "adam");
        assert_eq!((s[0].lines, s[0].words), (2, 4));
        assert_eq!(s[0].pct_lines, 66.7);
        assert_eq!(s[1].pct_lines, 33.3);
    }
}

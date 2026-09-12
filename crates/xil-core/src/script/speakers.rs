//! Speaker recognition: the CAST block, `speakers.json`, and the built-in
//! fallback list. Port of the speaker half of `XILP001_script_parser.py`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value};

use crate::workspace::{show_slug, workspace_root};

static PAREN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\([^)]*?\)\s*").unwrap());
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\w\s]").unwrap());
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static ROLE_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*[—–]\s*").unwrap());

/// Built-in display names, already longest-first as the Python literal is.
const BUILTIN: [(&str, &str); 21] = [
    ("FILM AUDIO (MARGARET'S VOICE)", "film_audio"),
    ("STRANGER (MALE VOICE, FLAT)", "stranger"),
    ("MARGARET (V.O.)", "margaret"),
    ("MR. PATTERSON", "mr_patterson"),
    ("FILM AUDIO", "film_audio"),
    ("STRANGER", "stranger"),
    ("MARGARET", "margaret"),
    ("MARTHA", "martha"),
    ("GERALD", "gerald"),
    ("KAREN", "karen"),
    ("SARAH", "sarah"),
    ("ELENA", "elena"),
    ("CLERK", "clerk"),
    ("ADAM", "adam"),
    ("DEZ", "dez"),
    ("MAYA", "maya"),
    ("AVA", "ava"),
    ("RÍAN", "rian"),
    ("RÍÁN", "rian"),
    ("FRANK", "frank"),
    ("TINA", "tina"),
];

/// Display names (match order) plus the display → key map.
#[derive(Clone, Debug, Default)]
pub struct Speakers {
    pub known: Vec<String>,
    pub keys: IndexMap<String, String>,
}

impl Speakers {
    fn builtin() -> Speakers {
        Speakers {
            known: BUILTIN.iter().map(|(d, _)| d.to_string()).collect(),
            // The Python key dict lists MR. PATTERSON before FILM AUDIO and
            // MARGARET (V.O.) after MARGARET; only insertion order differs
            // from the list, and generate_cast_config depends on it.
            keys: builtin_key_order(),
        }
    }
}

fn builtin_key_order() -> IndexMap<String, String> {
    const ORDER: [(&str, &str); 21] = [
        ("FILM AUDIO (MARGARET'S VOICE)", "film_audio"),
        ("STRANGER (MALE VOICE, FLAT)", "stranger"),
        ("MR. PATTERSON", "mr_patterson"),
        ("FILM AUDIO", "film_audio"),
        ("STRANGER", "stranger"),
        ("MARGARET (V.O.)", "margaret"),
        ("MARGARET", "margaret"),
        ("MARTHA", "martha"),
        ("GERALD", "gerald"),
        ("CLERK", "clerk"),
        ("KAREN", "karen"),
        ("SARAH", "sarah"),
        ("ELENA", "elena"),
        ("ADAM", "adam"),
        ("DEZ", "dez"),
        ("MAYA", "maya"),
        ("AVA", "ava"),
        ("RÍAN", "rian"),
        ("RÍÁN", "rian"),
        ("FRANK", "frank"),
        ("TINA", "tina"),
    ];
    ORDER
        .iter()
        .map(|(d, k)| (d.to_string(), k.to_string()))
        .collect()
}

/// Display name → normalized key: drop parentheticals and punctuation,
/// lowercase, collapse whitespace to underscores.
pub fn display_to_key(display: &str) -> String {
    let name = PAREN.replace_all(display, " ");
    let name = NON_WORD.replace_all(name.trim(), "");
    let name = name.to_lowercase();
    let name = name.trim();
    SPACES.replace_all(name, "_").trim_matches('_').to_string()
}

/// One CAST-block entry.
#[derive(Clone, Debug, PartialEq)]
pub struct CastEntry {
    pub display: String,
    pub key: String,
}

/// Parse the `CAST:` bullet block from the script header.
pub fn extract_cast_from_script(lines: &[String]) -> Vec<CastEntry> {
    let mut entries = Vec::new();
    let mut seen = Vec::new();
    let mut in_cast = false;
    for line in lines {
        let stripped = line.trim();
        if stripped == "CAST:" {
            in_cast = true;
            continue;
        }
        if !in_cast {
            continue;
        }
        if let Some(raw) = stripped.strip_prefix('*') {
            // Only an em/en dash ends a name: plain hyphens appear in names
            // (T-BONE) and parentheses can be part of the label.
            let name = ROLE_DASH.split(raw.trim()).next().unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            let key = display_to_key(name);
            if !key.is_empty() && !seen.contains(&key) {
                entries.push(CastEntry {
                    display: name.to_string(),
                    key: key.clone(),
                });
                seen.push(key);
            }
        } else if stripped == "===" || stripped == "---" || !stripped.is_empty() {
            break;
        }
    }
    entries
}

/// Which `speakers.json` to load, without reading it.
pub fn resolve_speakers_file(path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = path {
        return Some(p.to_path_buf());
    }
    let root = workspace_root();
    let show = crate::workspace::read_project("project.json")
        .get("show")
        .and_then(Value::as_str)
        .unwrap_or("Sample Show")
        .to_string();
    let normalized = root
        .join("configs")
        .join(show_slug(&show))
        .join("speakers.json");
    if normalized.exists() {
        return Some(normalized);
    }
    let legacy = root.join("speakers.json");
    legacy.exists().then_some(legacy)
}

fn read_speakers_json(path: &Path) -> Vec<Map<String, Value>> {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .map(|a| {
            a.into_iter()
                .filter_map(|e| e.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// Merge CAST-block entries with `speakers.json`, falling back to the
/// built-ins only when neither exists.
///
/// JSON keys always win over auto-derived ones, and a multi-word display
/// name also registers its underscore form. The result is sorted
/// longest-first so compound names match before short ones.
pub fn load_speakers(path: Option<&Path>, cast_entries: &[CastEntry]) -> Speakers {
    let speakers_file = resolve_speakers_file(path).filter(|p| p.exists());
    if cast_entries.is_empty() && speakers_file.is_none() {
        return Speakers::builtin();
    }

    let mut known: Vec<String> = Vec::new();
    let mut keys: IndexMap<String, String> = IndexMap::new();

    for entry in cast_entries {
        if !keys.contains_key(&entry.display) {
            known.push(entry.display.clone());
            keys.insert(entry.display.clone(), entry.key.clone());
        }
        if entry.display.contains(' ') {
            let underscore = entry.display.replace(' ', "_");
            if !keys.contains_key(&underscore) {
                known.push(underscore.clone());
            }
            keys.insert(underscore, entry.key.clone());
        }
    }

    if let Some(f) = &speakers_file {
        for entry in read_speakers_json(f) {
            let (Some(display), Some(key)) = (
                entry.get("display").and_then(Value::as_str),
                entry.get("key").and_then(Value::as_str),
            ) else {
                continue;
            };
            if !keys.contains_key(display) {
                known.push(display.to_string());
            }
            keys.insert(display.to_string(), key.to_string());
            if display.contains(' ') {
                let underscore = display.replace(' ', "_");
                if !keys.contains_key(&underscore) {
                    known.push(underscore.clone());
                }
                keys.insert(underscore, key.to_string());
            }
        }
    }

    // Python's list.sort is stable and keys on len() alone, so equal-length
    // names keep insertion order.
    known.sort_by_key(|a| std::cmp::Reverse(a.chars().count()));
    Speakers { known, keys }
}

/// Full `speakers.json` entries keyed by speaker key, for cast skeletons.
pub fn load_speakers_registry(path: Option<&Path>) -> IndexMap<String, Map<String, Value>> {
    let Some(f) = resolve_speakers_file(path).filter(|p| p.exists()) else {
        return IndexMap::new();
    };
    read_speakers_json(&f)
        .into_iter()
        .filter_map(|e| {
            e.get("key")
                .and_then(Value::as_str)
                .map(|k| (k.to_string(), e.clone()))
        })
        .collect()
}

/// A speaker matched at the start of a line.
pub struct SpeakerMatch {
    pub key: String,
    pub direction: Option<String>,
    pub text: String,
}

/// Match a known speaker prefix, splitting off any `(direction)`.
pub fn try_match_speaker(line: &str, speakers: &Speakers) -> Option<SpeakerMatch> {
    for speaker in &speakers.known {
        let Some(rest) = line.strip_prefix(speaker.as_str()) else {
            continue;
        };
        // Must be followed by a space, '(' or end of line.
        match rest.chars().next() {
            Some(c) if c != ' ' && c != '(' => continue,
            _ => {}
        }
        let mut rest = rest.trim_start();
        let mut direction = None;
        if rest.starts_with('(') {
            if let Some(end) = rest.find(')') {
                direction = Some(rest[1..end].trim().to_string());
                rest = rest[end + 1..].trim();
            }
        }
        return Some(SpeakerMatch {
            key: speakers.keys[speaker].clone(),
            direction,
            text: rest.to_string(),
        });
    }
    None
}

/// Reverse map key → first display name, as `generate_cast_config` builds it
/// from the module-level `SPEAKER_KEYS`.
pub fn key_to_display(speakers: &Speakers) -> IndexMap<String, String> {
    let mut out: IndexMap<String, String> = IndexMap::new();
    for (display, key) in &speakers.keys {
        out.entry(key.clone()).or_insert_with(|| display.clone());
    }
    out
}

/// The built-in table, for callers that need it without any script context.
pub fn builtin_speakers() -> Speakers {
    Speakers::builtin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    #[test]
    fn key_derivation_matches_docstring() {
        assert_eq!(display_to_key("ADAM"), "adam");
        assert_eq!(display_to_key("MR. PATTERSON"), "mr_patterson");
        assert_eq!(
            display_to_key("FILM AUDIO (MARGARET'S VOICE)"),
            "film_audio"
        );
        assert_eq!(
            display_to_key("DETECTIVE NORA WALSH"),
            "detective_nora_walsh"
        );
        assert_eq!(display_to_key("RÍAN"), "rían", "word chars include accents");
    }

    #[test]
    fn cast_block_stops_at_the_first_non_bullet() {
        let c = extract_cast_from_script(&lines(
            "HEADER\nCAST:\n* ADAM — Host\n* MR. PATTERSON — Caller\n* T-BONE\n* ADAM — dupe\n\n===\nCOLD OPEN\n* LATE",
        ));
        assert_eq!(
            c,
            vec![
                CastEntry {
                    display: "ADAM".into(),
                    key: "adam".into()
                },
                CastEntry {
                    display: "MR. PATTERSON".into(),
                    key: "mr_patterson".into()
                },
                CastEntry {
                    display: "T-BONE".into(),
                    key: "tbone".into()
                },
            ]
        );
    }

    #[test]
    fn cast_seeds_speakers_and_registers_underscore_forms() {
        let cast = vec![CastEntry {
            display: "NORA WALSH".into(),
            key: "nora_walsh".into(),
        }];
        let s = load_speakers(Some(Path::new("/nonexistent")), &cast);
        // Equal lengths, so the stable sort keeps insertion order: the
        // display name was registered before its underscore form.
        assert_eq!(s.known, vec!["NORA WALSH", "NORA_WALSH"]);
        assert_eq!(s.keys["NORA WALSH"], "nora_walsh");
        assert_eq!(s.keys["NORA_WALSH"], "nora_walsh");
    }

    #[test]
    fn builtins_only_when_nothing_else_exists() {
        let s = load_speakers(Some(Path::new("/nonexistent")), &[]);
        assert_eq!(s.known.len(), 21);
        assert_eq!(s.known[0], "FILM AUDIO (MARGARET'S VOICE)");
        assert_eq!(s.keys["MARGARET (V.O.)"], "margaret");
    }

    #[test]
    fn json_key_overrides_the_cast_derived_one() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("speakers.json");
        fs::write(&f, r#"[{"display": "NORA WALSH", "key": "nora"}, {"display": "NEW GUY", "key": "newguy"}]"#).unwrap();
        let cast = vec![CastEntry {
            display: "NORA WALSH".into(),
            key: "nora_walsh".into(),
        }];
        let s = load_speakers(Some(&f), &cast);
        assert_eq!(s.keys["NORA WALSH"], "nora");
        assert_eq!(s.keys["NORA_WALSH"], "nora");
        assert_eq!(s.keys["NEW GUY"], "newguy");
    }

    #[test]
    fn speaker_match_splits_direction_and_text() {
        let s = load_speakers(Some(Path::new("/nonexistent")), &[]);
        let m = try_match_speaker("ADAM (quietly) Hello there.", &s).unwrap();
        assert_eq!(
            (m.key.as_str(), m.direction.as_deref(), m.text.as_str()),
            ("adam", Some("quietly"), "Hello there.")
        );
        let bare = try_match_speaker("ADAM", &s).unwrap();
        assert_eq!(
            (bare.key.as_str(), bare.direction, bare.text.as_str()),
            ("adam", None, "")
        );
        assert!(
            try_match_speaker("ADAMANT sounds", &s).is_none(),
            "must end on space, ( or EOL"
        );
        assert!(try_match_speaker("Adam lowercase", &s).is_none());
    }

    #[test]
    fn longest_name_wins_over_its_prefix() {
        let s = load_speakers(Some(Path::new("/nonexistent")), &[]);
        let m = try_match_speaker("MARGARET (V.O.) distant", &s).unwrap();
        assert_eq!(m.key, "margaret");
        assert_eq!(
            m.text, "distant",
            "the (V.O.) form matched whole, not MARGARET + direction"
        );
    }
}

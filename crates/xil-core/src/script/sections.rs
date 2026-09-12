//! Section-header maps, one per content type. Port of the `*_SECTIONS`
//! tables in `XILP001_script_parser.py`.

use indexmap::IndexMap;

/// The legacy map: every show-specific header ever added, merged under all
/// the type maps so old scripts keep parsing.
const LEGACY: [(&str, &str); 34] = [
    ("COLD OPEN", "cold-open"),
    ("OPENING CREDITS", "opening-credits"),
    ("CHAPTER ONE", "chapter1"),
    ("CHAPTER 1", "chapter1"),
    ("CHAPTER TWO", "chapter2"),
    ("CHAPTER 2", "chapter2"),
    ("CHAPTER THREE", "chapter3"),
    ("CHAPTER 3", "chapter3"),
    ("ACT ONE", "act1"),
    ("ACT 1", "act1"),
    ("ACT TWO", "act2"),
    ("ACT 2", "act2"),
    ("ACT THREE", "act3"),
    ("ACT 3", "act3"),
    ("ACT FOUR", "act4"),
    ("ACT 4", "act4"),
    ("ACT FIVE", "act5"),
    ("ACT 5", "act5"),
    ("ACT SIX", "act6"),
    ("ACT 6", "act6"),
    ("MID-EPISODE BREAK", "mid-break"),
    ("CLOSING", "closing"),
    ("CLOSING — RADIO STATION", "closing"),
    ("CLOSING — ADAM'S SIGN-OFF", "closing"),
    ("CLOSING \u{2014} ADAM\u{2019}S SIGN-OFF", "closing"),
    ("POST-INTERVIEW", "post-interview"),
    ("POST-INTERVIEW: ADAM & TINA", "post-interview"),
    ("POST-CREDITS SCENE", "post-credits"),
    ("DEZ'S CLOSING NARRATION", "dez-closing"),
    ("DEZ\u{2019}S CLOSING NARRATION", "dez-closing"),
    ("PRODUCTION NOTES", "production-notes"),
    // The Woonsocket Wonders intro/outro labels map onto preamble/postamble.
    ("EPISODE THEME", "preamble"),
    ("PRE-SHOW MUSIC", "preamble"),
    ("CLOSING TAG", "postamble"),
];

const LEGACY_TAIL: [(&str, &str); 2] = [("PREAMBLE", "preamble"), ("POSTAMBLE", "postamble")];

const PODCAST: [(&str, &str); 22] = [
    ("COLD OPEN", "cold-open"),
    ("OPENING CREDITS", "opening-credits"),
    ("ACT ONE", "act1"),
    ("ACT 1", "act1"),
    ("ACT TWO", "act2"),
    ("ACT 2", "act2"),
    ("ACT THREE", "act3"),
    ("ACT 3", "act3"),
    ("ACT FOUR", "act4"),
    ("ACT 4", "act4"),
    ("ACT FIVE", "act5"),
    ("ACT 5", "act5"),
    ("ACT SIX", "act6"),
    ("ACT 6", "act6"),
    ("MID-EPISODE BREAK", "mid-break"),
    ("CLOSING", "closing"),
    ("POST-CREDITS SCENE", "post-credits"),
    ("INTRO", "intro"),
    ("OUTRO", "outro"),
    ("PREAMBLE", "preamble"),
    ("POSTAMBLE", "postamble"),
    // Padding entry so the array length is a constant; removed below.
    ("", ""),
];

const CHAPTER_WORDS: [&str; 30] = [
    "ONE",
    "TWO",
    "THREE",
    "FOUR",
    "FIVE",
    "SIX",
    "SEVEN",
    "EIGHT",
    "NINE",
    "TEN",
    "ELEVEN",
    "TWELVE",
    "THIRTEEN",
    "FOURTEEN",
    "FIFTEEN",
    "SIXTEEN",
    "SEVENTEEN",
    "EIGHTEEN",
    "NINETEEN",
    "TWENTY",
    "TWENTY-ONE",
    "TWENTY-TWO",
    "TWENTY-THREE",
    "TWENTY-FOUR",
    "TWENTY-FIVE",
    "TWENTY-SIX",
    "TWENTY-SEVEN",
    "TWENTY-EIGHT",
    "TWENTY-NINE",
    "THIRTY",
];

const DRAMA: [(&str, &str); 17] = [
    ("PROLOGUE", "prologue"),
    ("EPILOGUE", "epilogue"),
    ("INTERMISSION", "intermission"),
    ("ACT ONE", "act1"),
    ("ACT 1", "act1"),
    ("ACT TWO", "act2"),
    ("ACT 2", "act2"),
    ("ACT THREE", "act3"),
    ("ACT 3", "act3"),
    ("ACT FOUR", "act4"),
    ("ACT 4", "act4"),
    ("ACT FIVE", "act5"),
    ("ACT 5", "act5"),
    ("ACT SIX", "act6"),
    ("ACT 6", "act6"),
    ("COLD OPEN", "cold-open"),
    ("CLOSING", "closing"),
];

type Map = IndexMap<String, String>;

fn from_pairs(pairs: &[(&str, &str)]) -> Map {
    pairs
        .iter()
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn legacy_map() -> Map {
    let mut m = from_pairs(&LEGACY);
    m.extend(from_pairs(&LEGACY_TAIL));
    m
}

fn podcast_map() -> Map {
    from_pairs(&PODCAST)
}

fn audiobook_map() -> Map {
    let mut m: Map = IndexMap::new();
    m.insert("PROLOGUE".into(), "prologue".into());
    m.insert("EPILOGUE".into(), "epilogue".into());
    m.insert("AUTHOR'S NOTE".into(), "authors-note".into());
    m.insert("AUTHOR\u{2019}S NOTE".into(), "authors-note".into());
    for (i, word) in CHAPTER_WORDS.iter().enumerate() {
        m.insert(format!("CHAPTER {word}"), format!("chapter{}", i + 1));
    }
    for n in 1..=30 {
        m.insert(format!("CHAPTER {n}"), format!("chapter{n}"));
    }
    m
}

fn drama_map() -> Map {
    let mut m = from_pairs(&DRAMA);
    m.insert("POST-CREDITS SCENE".into(), "post-credits".into());
    m
}

fn special_map() -> Map {
    let mut m = podcast_map();
    m.extend(audiobook_map());
    m.extend(drama_map());
    for n in 1..=15 {
        m.insert(format!("SEGMENT {n}"), format!("segment{n}"));
    }
    m
}

/// Section map for a content type: the legacy table with the type's own
/// entries laid over it.
pub fn get_section_map(project_type: &str) -> Map {
    let base = match project_type {
        "podcast" => podcast_map(),
        "audiobook" => audiobook_map(),
        "drama" => drama_map(),
        "special" => special_map(),
        _ => return legacy_map(),
    };
    let mut merged = legacy_map();
    merged.extend(base);
    merged
}

/// Section slug for a header line, or `None`. A subtitle-qualified header
/// (`ACT ONE: "Yesterday"`) matches on the part before the first colon.
pub fn match_section(line: &str, section_map: &Map) -> Option<String> {
    let stripped = line.trim();
    if let Some(s) = section_map.get(stripped) {
        return Some(s.clone());
    }
    if let Some((base, _)) = stripped.split_once(':') {
        if let Some(s) = section_map.get(base.trim()) {
            return Some(s.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn podcast_keeps_legacy_entries() {
        let m = get_section_map("podcast");
        assert_eq!(m["ACT ONE"], "act1");
        assert_eq!(
            m["DEZ'S CLOSING NARRATION"], "dez-closing",
            "legacy entry survives"
        );
        assert_eq!(m["INTRO"], "intro", "type entry present");
        assert_eq!(m["CHAPTER ONE"], "chapter1", "legacy chapters still parse");
    }

    #[test]
    fn audiobook_has_thirty_chapters_both_spellings() {
        let m = get_section_map("audiobook");
        assert_eq!(m["CHAPTER ONE"], "chapter1");
        assert_eq!(m["CHAPTER THIRTY"], "chapter30");
        assert_eq!(m["CHAPTER 30"], "chapter30");
        assert_eq!(m["AUTHOR\u{2019}S NOTE"], "authors-note");
    }

    #[test]
    fn special_merges_all_three_plus_segments() {
        let m = get_section_map("special");
        assert_eq!(m["SEGMENT 15"], "segment15");
        assert_eq!(m["PROLOGUE"], "prologue");
        assert_eq!(m["MID-EPISODE BREAK"], "mid-break");
        assert!(!m.contains_key("SEGMENT 16"));
    }

    #[test]
    fn unknown_type_falls_back_to_legacy_only() {
        let m = get_section_map("nonsense");
        assert!(m.contains_key("EPISODE THEME"));
        assert!(!m.contains_key("INTRO"), "podcast-only entry absent");
    }

    #[test]
    fn subtitled_headers_match_on_the_stem() {
        let m = get_section_map("podcast");
        assert_eq!(match_section("ACT ONE", &m).as_deref(), Some("act1"));
        assert_eq!(
            match_section("  ACT ONE: \"Yesterday\"  ", &m).as_deref(),
            Some("act1")
        );
        assert_eq!(
            match_section("POST-INTERVIEW: ADAM & TINA", &m).as_deref(),
            Some("post-interview")
        );
        assert_eq!(match_section("SOMETHING ELSE", &m), None);
    }
}

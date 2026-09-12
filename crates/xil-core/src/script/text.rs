//! Markdown normalization and line classification. Port of the standalone
//! text helpers in `XILP001_script_parser.py`.

use std::sync::LazyLock;

use regex::Regex;

/// Direction subtypes, in the order `classify_direction` tests them.
pub const DIRECTION_TYPES: [&str; 8] = [
    "SFX",
    "MUSIC",
    "AMBIENCE",
    "BEAT",
    "VINTAGE FILTER",
    "FILM AUDIO",
    "SPEAKERPHONE",
    "PHONE FILTER",
];

static ESCAPE_ANY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\(.)").unwrap());
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#{1,6}\s*").unwrap());
static SCENE_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^SCENE (\d+[A-Za-z]*):\s*(.+)").unwrap());
static SCENE_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^SCENE \d+[A-Za-z]*:").unwrap());
static DIVIDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^={3,}$|^-{3,}$").unwrap());
static BRACKETS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]").unwrap());
static BRACKET_STRIP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\[[^\]]+\]").unwrap());

/// Remove markdown backslash escapes.
pub fn strip_markdown_escapes(text: &str) -> String {
    let text = text
        .replace("\\[", "[")
        .replace("\\]", "]")
        .replace("\\===", "===")
        .replace("\\=", "=");
    ESCAPE_ANY.replace_all(&text, "$1").into_owned()
}

/// Remove heading prefixes, bold markers and trailing whitespace, per line.
pub fn strip_markdown_formatting(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            HEADING
                .replace(line, "")
                .replace("**", "")
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Classify bracket-interior text into a sound category.
pub fn classify_direction(text: &str) -> Option<&'static str> {
    let t = text.trim();
    for dt in DIRECTION_TYPES {
        if t.starts_with(dt) {
            return Some(dt);
        }
    }
    if t == "BEAT" || t == "LONG BEAT" {
        return Some("BEAT");
    }
    if t == "INTRO MUSIC" || t == "OUTRO MUSIC" {
        return Some("MUSIC");
    }
    None
}

/// `[SFX: ...]`-style line: starts with `[` and contains `]`.
pub fn is_stage_direction(line: &str) -> bool {
    line.starts_with('[') && line.contains(']')
}

/// Every bracketed segment on a line, in order.
pub fn find_brackets(line: &str) -> Vec<String> {
    BRACKETS
        .captures_iter(line)
        .map(|c| c[1].to_string())
        .collect()
}

/// Drop bracketed directions from a scene-header line.
pub fn strip_brackets(line: &str) -> String {
    BRACKET_STRIP
        .replace_all(line.trim(), "")
        .trim()
        .to_string()
}

pub fn is_scene_header(line: &str) -> bool {
    SCENE_START.is_match(line)
}

/// `(scene_number, scene_name)` from a `SCENE N: Name` line.
pub fn parse_scene_header(line: &str) -> (Option<String>, Option<String>) {
    match SCENE_HEAD.captures(line) {
        Some(c) => (Some(c[1].to_string()), Some(c[2].trim().to_string())),
        None => (None, None),
    }
}

pub fn is_divider(line: &str) -> bool {
    DIVIDER.is_match(line.trim())
}

/// Headers that begin the post-script metadata block.
pub fn is_metadata_section(line: &str) -> bool {
    matches!(
        line.trim(),
        "PRODUCTION NOTES:"
            | "SOCIAL MEDIA PROMPT:"
            | "KEY CHANGES FROM ORIGINAL:"
            | "ACCESSIBILITY NOTES:"
            | "VOICES NEEDED THIS EPISODE:"
            | "KEY SOUND EFFECTS:"
            | "MUSIC CUES:"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_then_formatting() {
        assert_eq!(
            strip_markdown_escapes(r"\[SFX\] \=\== a\.b \~ \*"),
            "[SFX] === a.b ~ *"
        );
        assert_eq!(
            strip_markdown_formatting("## Head\n**bold** text  \nplain"),
            "Head\nbold text\nplain"
        );
        assert_eq!(
            strip_markdown_formatting("####### seven hashes"),
            "# seven hashes"
        );
    }

    #[test]
    fn direction_classification() {
        assert_eq!(classify_direction("SFX: DOOR"), Some("SFX"));
        assert_eq!(classify_direction("  BEAT  "), Some("BEAT"));
        assert_eq!(classify_direction("LONG BEAT"), Some("BEAT"));
        assert_eq!(classify_direction("BEAT — 3 SECONDS"), Some("BEAT"));
        assert_eq!(classify_direction("INTRO MUSIC"), Some("MUSIC"));
        assert_eq!(classify_direction("OUTRO MUSIC"), Some("MUSIC"));
        assert_eq!(
            classify_direction("PHONE FILTER: ENGAGES"),
            Some("PHONE FILTER")
        );
        assert_eq!(classify_direction("drawn out"), None);
        assert_eq!(classify_direction(""), None);
    }

    #[test]
    fn scene_and_divider_shapes() {
        assert!(is_scene_header("SCENE 5A: THE ALLEY"));
        assert!(!is_scene_header("SCENE: THE ALLEY"));
        assert_eq!(
            parse_scene_header("SCENE 12: THE  ROOM "),
            (Some("12".into()), Some("THE  ROOM".into()))
        );
        assert_eq!(
            parse_scene_header("SCENE 3:"),
            (None, None),
            "a name is required"
        );
        assert!(is_divider("==="));
        assert!(is_divider("  ----- "));
        assert!(!is_divider("--"));
    }

    #[test]
    fn brackets_found_and_stripped() {
        assert_eq!(
            find_brackets("[MUSIC: A] and [SFX: B]"),
            vec!["MUSIC: A", "SFX: B"]
        );
        assert_eq!(
            find_brackets("[]"),
            Vec::<String>::new(),
            "empty brackets do not match"
        );
        assert_eq!(
            strip_brackets("SCENE 1: ROOM [AMBIENCE: hum]"),
            "SCENE 1: ROOM"
        );
        assert!(is_stage_direction("[BEAT]"));
        assert!(!is_stage_direction("text [BEAT]"));
    }
}

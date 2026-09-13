//! Google Translate TTS, driven exactly as `gTTS` 2.5 drives it.
//!
//! Text is pre-processed and, past 100 characters, tokenised with gTTS's
//! rules, then each chunk is posted to the `batchexecute` RPC and the
//! base64 audio in each answer is concatenated. The rules use regex
//! lookbehind, which the `regex` crate does not have, so they are written
//! out by hand below — in the same order and with the same alternation
//! priority as the compiled Python pattern.

use serde_json::Value;

use crate::{agent, base_url, call, read_bytes, ApiError};

pub const DEFAULT_BASE_URL: &str = "https://translate.google.com";
const MAX_CHARS: usize = 100;
const RPC: &str = "jQ1olc";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/47.0.2526.106 Safari/537.36";

const TONE_MARKS: [char; 4] = ['?', '!', '？', '！'];
const ABBREVIATIONS: [&str; 9] = ["dr", "jr", "mr", "mrs", "ms", "msgr", "prof", "sr", "st"];
/// `symbols.ALL_PUNC`.
const ALL_PUNC: &str = "?!？！.,¡()[]¿…‥،;:—。，、：\n";

/// `pre_processors.tone_marks`: a space after every tone mark.
fn pp_tone_marks(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        out.push(c);
        if TONE_MARKS.contains(&c) {
            out.push(' ');
        }
    }
    out
}

/// `pre_processors.end_of_line`: drop `-\n`.
fn pp_end_of_line(text: &str) -> String {
    text.replace("-\n", "")
}

/// `pre_processors.abbreviations`: remove the period after each listed
/// abbreviation (case-insensitive, no word boundary — gTTS's own regex),
/// one abbreviation per pass, each pass judged on that pass's input.
fn pp_abbreviations(text: &str) -> String {
    let mut text = text.to_string();
    for abbr in ABBREVIATIONS {
        let chars: Vec<char> = text.chars().collect();
        let a: Vec<char> = abbr.chars().collect();
        let mut out = String::with_capacity(text.len());
        for (i, &c) in chars.iter().enumerate() {
            if c == '.' && i >= a.len() {
                let before = &chars[i - a.len()..i];
                if before
                    .iter()
                    .zip(&a)
                    .all(|(x, y)| x.to_lowercase().eq(y.to_lowercase()))
                {
                    continue;
                }
            }
            out.push(c);
        }
        text = out;
    }
    text
}

/// `pre_processors.word_sub`: `Esq.` → `Esquire`, case-insensitive.
fn pp_word_sub(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        // Non-ASCII case mapping changed byte offsets; fall back to char walk.
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let window: String = chars[i..(i + 4).min(chars.len())].iter().collect();
            if window.to_lowercase() == "esq." {
                out.push_str("Esquire");
                i += 4;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        return out;
    }
    let mut out = String::new();
    let mut i = 0;
    while i < text.len() {
        if lower[i..].starts_with("esq.") {
            out.push_str("Esquire");
            i += 4;
        } else {
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn is_other_punc(c: char) -> bool {
    ALL_PUNC.contains(c) && !TONE_MARKS.contains(&c) && c != '.' && c != ',' && c != ':'
}

/// `Tokenizer([tone_marks, period_comma, colon, other_punctuation]).run`:
/// `re.split` on the combined, case-insensitive pattern.
fn tokenize(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // (?<=[?!？！]). — any character but a newline after a tone mark.
        let tone = i > 0 && TONE_MARKS.contains(&chars[i - 1]) && c != '\n';
        // (?<!\.[a-z])[.,] followed by a space.
        let period_comma = (c == '.' || c == ',')
            && chars.get(i + 1) == Some(&' ')
            && !(i >= 2 && chars[i - 2] == '.' && chars[i - 1].is_ascii_alphabetic());
        // (?<!\d):
        let colon = c == ':' && !(i > 0 && chars[i - 1].is_ascii_digit());
        let len = if tone {
            1
        } else if period_comma {
            2
        } else if colon || is_other_punc(c) {
            1
        } else {
            0
        };
        if len > 0 {
            tokens.push(chars[start..i].iter().collect());
            i += len;
            start = i;
        } else {
            i += 1;
        }
    }
    tokens.push(chars[start..].iter().collect());
    tokens
}

/// `_ALL_PUNC_OR_SPACE.match(t)` — nothing but punctuation and ASCII whitespace.
fn is_punc_or_space(t: &str) -> bool {
    t.chars()
        .all(|c| ALL_PUNC.contains(c) || " \t\n\r\x0b\x0c".contains(c))
}

/// Python `str.strip()`.
fn py_strip(s: &str) -> String {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
        .to_string()
}

fn clean_tokens(tokens: Vec<String>) -> Vec<String> {
    tokens
        .into_iter()
        .filter(|t| !is_punc_or_space(t))
        .map(|t| py_strip(&t))
        .collect()
}

/// `_minimize(the_string, " ", max_size)`.
fn minimize(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars: Vec<char> = s.chars().collect();
    loop {
        if chars.first() == Some(&' ') {
            chars.remove(0);
        }
        if chars.len() <= max {
            out.push(chars.iter().collect());
            return out;
        }
        let idx = chars[..max].iter().rposition(|&c| c == ' ').unwrap_or(max);
        out.push(chars[..idx].iter().collect());
        chars = chars[idx..].to_vec();
    }
}

/// `gTTS._tokenize(text)` — the chunks that become requests.
pub fn text_parts(text: &str) -> Vec<String> {
    let mut t = py_strip(text);
    t = pp_tone_marks(&t);
    t = pp_end_of_line(&t);
    t = pp_abbreviations(&t);
    t = pp_word_sub(&t);
    if t.chars().count() <= MAX_CHARS {
        return clean_tokens(vec![t]);
    }
    let tokens = clean_tokens(tokenize(&t));
    tokens
        .iter()
        .flat_map(|tok| minimize(tok, MAX_CHARS))
        .filter(|s| !s.is_empty())
        .collect()
}

/// `urllib.parse.quote(s)` with the default `safe="/"`.
fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `json.dumps(obj, separators=(",", ":"))` — ASCII-escaped.
fn compact_ascii(v: &Value) -> String {
    let s = xil_core::pyjson::dumps(v, xil_core::pyjson::Style::COMPACT);
    // pyjson's compact style uses ", " / ": "; gTTS asks for none.
    reserialize_tight(&s)
}

/// Remove the spaces Python's default separators add, outside strings.
fn reserialize_tight(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut escaped = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            ',' | ':' => {
                out.push(c);
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// `gTTS._package_rpc(text)` → the form body.
fn package_rpc(text: &str, lang: &str) -> String {
    let parameter = serde_json::json!([text, lang, null, "null"]);
    let escaped = compact_ascii(&parameter);
    let rpc = serde_json::json!([[[RPC, escaped, null, "generic"]]]);
    format!("f.req={}&", quote(&compact_ascii(&rpc)))
}

#[derive(Debug, thiserror::Error)]
pub enum GttsError {
    #[error("{0}")]
    Api(#[from] ApiError),
    #[error("200 (OK) from TTS API. Probable cause: Unknown")]
    NoAudio,
    #[error("No text to send to TTS API")]
    NoText,
}

/// `gTTS(text=, lang=).save(path)` → the concatenated MP3 bytes.
pub fn synthesize(text: &str, lang: &str) -> Result<Vec<u8>, GttsError> {
    let parts = text_parts(text);
    if parts.is_empty() {
        return Err(GttsError::NoText);
    }
    let url = format!(
        "{}/_/TranslateWebserverUi/data/batchexecute",
        base_url("XIL_GTTS_BASE_URL", DEFAULT_BASE_URL)
    );
    let mut audio = Vec::new();
    for part in parts {
        let body = package_rpc(&part, lang);
        let req = agent().post(&url).set("User-Agent", USER_AGENT).set(
            "Content-Type",
            "application/x-www-form-urlencoded;charset=utf-8",
        );
        let resp = call(req.send_string(&body))?;
        let bytes = read_bytes(resp)?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            if !line.contains(RPC) {
                continue;
            }
            let needle = format!("{RPC}\",\"[\\\"");
            let Some(pos) = line.find(&needle) else {
                return Err(GttsError::NoAudio);
            };
            let rest = &line[pos + needle.len()..];
            let Some(end) = rest.rfind("\\\"]") else {
                return Err(GttsError::NoAudio);
            };
            audio.extend(decode_base64(&rest[..end]).ok_or(GttsError::NoAudio)?);
        }
    }
    Ok(audio)
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_part() {
        assert_eq!(text_parts("  Hello there!  "), vec!["Hello there!"]);
        assert!(text_parts(" ... ").is_empty());
    }

    #[test]
    fn preprocessors() {
        assert_eq!(
            pp_abbreviations("Dr. Who and Mrs. Smith, first."),
            "Dr Who and Mrs Smith, first"
        );
        assert_eq!(pp_word_sub("John Doe, esq."), "John Doe, Esquire");
        assert_eq!(pp_tone_marks("Hi!Yes?"), "Hi! Yes? ");
    }

    #[test]
    fn rpc_body_is_quoted_compact_json() {
        let body = package_rpc("hello world", "en");
        assert_eq!(
            body,
            "f.req=%5B%5B%5B%22jQ1olc%22%2C%22%5B%5C%22hello%20world%5C%22%2C%5C%22en%5C%22%2Cnull%2C%5C%22null%5C%22%5D%22%2Cnull%2C%22generic%22%5D%5D%5D&"
        );
    }

    #[test]
    fn base64_round_trip() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
    }
}

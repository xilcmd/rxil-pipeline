//! `json.dumps` the way CPython writes it, so files and stdout compare byte
//! for byte with the Python pipeline.
//!
//! Covers what the pipeline uses: `indent=2` and compact form, `ensure_ascii`
//! on and off, Python float `repr`, insertion-ordered objects (serde_json is
//! built with `preserve_order`).

use std::fmt::Write;

use serde_json::Value;

/// How to render.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    /// `indent=2` when `Some(2)`, compact (`", "` / `": "`) when `None`.
    pub indent: Option<usize>,
    /// Escape every non-ASCII character as `\uXXXX` (Python's default).
    pub ensure_ascii: bool,
}

impl Style {
    /// `json.dumps(obj, indent=2)`
    pub const INDENT2: Style = Style {
        indent: Some(2),
        ensure_ascii: true,
    };
    /// `json.dumps(obj, indent=2, ensure_ascii=False)`
    pub const INDENT2_UTF8: Style = Style {
        indent: Some(2),
        ensure_ascii: false,
    };
    /// `json.dumps(obj)`
    pub const COMPACT: Style = Style {
        indent: None,
        ensure_ascii: true,
    };
    /// `json.dumps(obj, ensure_ascii=False)`
    pub const COMPACT_UTF8: Style = Style {
        indent: None,
        ensure_ascii: false,
    };
}

/// Marker prefix for a value that must be emitted verbatim, unquoted.
///
/// Python writes non-finite floats as the bare tokens `NaN`, `Infinity`
/// and `-Infinity`. JSON has no syntax for them and `serde_json::Number`
/// cannot hold them, so they travel as strings carrying this prefix. A
/// NUL cannot occur in the data this serializes, which is what makes it
/// safe as a sentinel.
pub const RAW_MARKER: char = '\u{0}';

/// Wrap a float the way Python's json module would write it: an ordinary
/// number when finite, a bare token when not.
pub fn py_float(v: f64) -> Value {
    if v.is_finite() {
        return serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    let token = if v.is_nan() {
        "NaN"
    } else if v > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    };
    Value::String(format!("{RAW_MARKER}{token}"))
}

/// Serialize `v` in the given style. Never fails: every `Value` is representable.
pub fn dumps(v: &Value, style: Style) -> String {
    let mut out = String::new();
    write_value(&mut out, v, style, 0);
    out
}

fn write_value(out: &mut String, v: &Value, style: Style, depth: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else if let Some(f) = n.as_f64() {
                out.push_str(&float_repr(f));
            }
        }
        Value::String(s) => match s.strip_prefix(RAW_MARKER) {
            Some(token) => out.push_str(token),
            None => write_string(out, s, style.ensure_ascii),
        },
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, style, depth + 1);
                write_value(out, item, style, depth + 1);
            }
            newline(out, style, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, style, depth + 1);
                write_string(out, k, style.ensure_ascii);
                out.push_str(": ");
                write_value(out, item, style, depth + 1);
            }
            newline(out, style, depth);
            out.push('}');
        }
    }
}

/// Between items: a newline plus indentation when indenting, a single space
/// after the comma in compact mode (Python's default `", "` separator).
fn newline(out: &mut String, style: Style, depth: usize) {
    match style.indent {
        Some(n) => {
            out.push('\n');
            for _ in 0..depth * n {
                out.push(' ');
            }
        }
        None => {
            // Compact: `[1, 2]` — the space goes after the comma, but not
            // before the first item or the closing bracket.
            if out.ends_with(',') {
                out.push(' ');
            }
        }
    }
}

/// Python's string escaping: `"` `\` and the C escapes, other control
/// characters as `\u00XX`; with `ensure_ascii`, everything above U+007F as
/// `\uXXXX` (surrogate pairs beyond the BMP). `/` is never escaped.
pub fn write_string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ensure_ascii && (c as u32) > 0x7f => {
                let cp = c as u32;
                if cp > 0xffff {
                    let v = cp - 0x10000;
                    let hi = 0xd800 + (v >> 10);
                    let lo = 0xdc00 + (v & 0x3ff);
                    let _ = write!(out, "\\u{hi:04x}\\u{lo:04x}");
                } else {
                    let _ = write!(out, "\\u{cp:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `repr(float)`: shortest round-trip digits, fixed notation when the
/// decimal exponent is in `-4..16`, otherwise `d.ddde±XX` with a two-digit
/// minimum exponent. Integral values keep a `.0`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    // Rust's `{:e}` is shortest-round-trip: "1.5e-5", "1e16", "-2.5e-7".
    let sci = format!("{f:e}");
    let (mant, exp) = sci
        .split_once('e')
        .expect("LowerExp always has an exponent");
    let exp: i32 = exp.parse().expect("exponent is an integer");
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        // Fixed notation. `digits` has the decimal point after the first digit.
        let point = exp + 1; // digits before the decimal point
        if point <= 0 {
            out.push_str("0.");
            for _ in 0..(-point) {
                out.push('0');
            }
            out.push_str(&digits);
        } else if (point as usize) >= digits.len() {
            out.push_str(&digits);
            for _ in 0..(point as usize - digits.len()) {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            out.push_str(&digits[..point as usize]);
            out.push('.');
            out.push_str(&digits[point as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (0.000123456, "0.000123456"),
            (1234567890123456.0, "1234567890123456.0"),
            (12345678901234567.0, "1.2345678901234568e+16"),
            (-0.0, "-0.0"),
            (2.5e-7, "2.5e-07"),
            (5.0, "5.0"),
            (0.3, "0.3"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (-1.5, "-1.5"),
            (123.456e-7, "1.23456e-05"),
        ];
        for (f, want) in cases {
            assert_eq!(float_repr(f), want, "repr({f})");
        }
    }

    #[test]
    fn indent2_layout_matches_python() {
        let v = json!({"a": [1, 2.0, "x"], "e": {}, "l": [], "n": null, "b": true});
        let want = "{\n  \"a\": [\n    1,\n    2.0,\n    \"x\"\n  ],\n  \"e\": {},\n  \"l\": [],\n  \"n\": null,\n  \"b\": true\n}";
        assert_eq!(dumps(&v, Style::INDENT2), want);
    }

    #[test]
    fn compact_uses_python_separators() {
        let v = json!({"a": [1, 2], "b": "c"});
        assert_eq!(dumps(&v, Style::COMPACT), r#"{"a": [1, 2], "b": "c"}"#);
        assert_eq!(dumps(&json!([]), Style::COMPACT), "[]");
        assert_eq!(dumps(&json!({}), Style::COMPACT), "{}");
    }

    #[test]
    fn non_finite_floats_are_bare_tokens() {
        let v =
            json!({"a": py_float(f64::NEG_INFINITY), "b": py_float(1.5), "c": py_float(f64::NAN)});
        assert_eq!(
            dumps(&v, Style::COMPACT),
            r#"{"a": -Infinity, "b": 1.5, "c": NaN}"#
        );
        assert_eq!(dumps(&py_float(f64::INFINITY), Style::COMPACT), "Infinity");
    }

    #[test]
    fn ensure_ascii_escapes_like_python() {
        let v = json!(["é", "—", "😀", "tab\there", "q\"", "bs\\", "\u{01}", "a/b"]);
        assert_eq!(
            dumps(&v, Style::COMPACT),
            "[\"\\u00e9\", \"\\u2014\", \"\\ud83d\\ude00\", \"tab\\there\", \"q\\\"\", \"bs\\\\\", \"\\u0001\", \"a/b\"]"
        );
        assert_eq!(
            dumps(&v, Style::COMPACT_UTF8),
            "[\"\u{e9}\", \"\u{2014}\", \"\u{1F600}\", \"tab\\there\", \"q\\\"\", \"bs\\\\\", \"\\u0001\", \"a/b\"]"
        );
    }
}

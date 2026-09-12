//! Python number and string formatting corners that show up in console
//! output: `round(x, n)`, `f"{x:.1f}"`, width padding by character count,
//! `s[:n]` slicing, and `html.escape`.

/// `round(x, ndigits)`: correctly rounded on the binary value, ties to
/// even — the same digits CPython's `float.__round__` produces.
pub fn round_to(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.ndigits$}").parse().unwrap_or(x)
}

/// `f"{x:.{prec}f}"`, spelling infinities and NaN the way Python does.
pub fn fixed(x: f64, prec: usize) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else if x.is_infinite() {
        (if x > 0.0 { "inf" } else { "-inf" }).to_string()
    } else {
        format!("{x:.prec$}")
    }
}

/// `f"{s:<width}"` — pad on the right to `width` characters.
pub fn pad_right(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// `f"{s:>width}"` — pad on the left to `width` characters.
pub fn pad_left(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - n))
    }
}

/// `s[:n]` — the first `n` characters.
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `html.escape(s)` with `quote=True`.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values printed by CPython 3.13.
    #[test]
    fn rounding_and_fixed_match_cpython() {
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(0.15, 1), 0.1);
        assert_eq!(round_to(1.25, 1), 1.2);
        assert_eq!(round_to(2.35, 1), 2.4);
        assert_eq!(round_to(100.0 / 3.0, 1), 33.3);
        assert_eq!(fixed(2.5, 0), "2");
        assert_eq!(fixed(3.5, 0), "4");
        assert_eq!(fixed(0.25, 1), "0.2");
        assert_eq!(fixed(0.35, 1), "0.3");
        assert_eq!(fixed(f64::NEG_INFINITY, 1), "-inf");
        assert_eq!(pad_left(&fixed(12345.678, 1), 6), "12345.7");
    }

    #[test]
    fn padding_and_slicing_count_characters() {
        assert_eq!(pad_right("ééé", 6), "ééé   ");
        assert_eq!(pad_left("ab", 4), "  ab");
        assert_eq!(head("日本語テキスト", 3), "日本語");
        assert_eq!(head("ab", 5), "ab");
    }

    #[test]
    fn escape_matches_html_module() {
        assert_eq!(
            html_escape("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &#x27; f"
        );
    }
}

//! Small HTML builders. Every value from disk or from a request goes through
//! [`esc`] before it lands in markup.

/// Escape text for an element body or a double-quoted attribute.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// `<option>`s whose value is also the label.
pub fn options(values: &[String], selected: Option<&str>) -> String {
    values
        .iter()
        .map(|v| {
            let sel = if Some(v.as_str()) == selected {
                " selected"
            } else {
                ""
            };
            format!("<option value=\"{0}\"{sel}>{0}</option>", esc(v))
        })
        .collect()
}

/// `<option>`s from `(label, value)` pairs.
pub fn labelled_options(pairs: &[(String, String)], selected: Option<&str>) -> String {
    pairs
        .iter()
        .map(|(label, value)| {
            let sel = if Some(value.as_str()) == selected {
                " selected"
            } else {
                ""
            };
            format!(
                "<option value=\"{}\"{sel}>{}</option>",
                esc(value),
                esc(label)
            )
        })
        .collect()
}

/// A labelled checkbox that posts `on` when ticked.
pub fn checkbox(name: &str, label: &str, checked: bool) -> String {
    format!(
        "<label class=\"check\"><input type=\"checkbox\" name=\"{name}\"{}> {}</label>",
        if checked { " checked" } else { "" },
        esc(label)
    )
}

/// A labelled text input.
pub fn text_input(name: &str, label: &str, placeholder: &str) -> String {
    format!(
        "<label class=\"field\"><span>{}</span><input name=\"{name}\" placeholder=\"{}\"></label>",
        esc(label),
        esc(placeholder)
    )
}

/// A labelled number input.
pub fn number_input(name: &str, label: &str, value: i64) -> String {
    format!(
        "<label class=\"field\"><span>{}</span><input type=\"number\" min=\"0\" step=\"1\" name=\"{name}\" value=\"{value}\"></label>",
        esc(label)
    )
}

/// A status line.
pub fn status(text: &str) -> String {
    format!("<div class=\"status\">{}</div>", esc(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_quotes() {
        assert_eq!(
            esc("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
        assert_eq!(
            options(&["a<b".into(), "c".into()], Some("c")),
            "<option value=\"a&lt;b\">a&lt;b</option><option value=\"c\" selected>c</option>"
        );
    }
}

//! `csv.writer` / `csv.DictWriter` the way CPython writes them.
//!
//! Default dialect: `,` delimiter, `"` quote doubled on escape,
//! QUOTE_MINIMAL, `\r\n` line terminator. A field is quoted when it
//! contains the delimiter, the quote character, `\r` or `\n`. A row that is
//! a single empty field is written as `""` so it is not an empty line.
//!
//! Values are rendered with Python `str()` semantics via [`cell`]:
//! `None` → empty, `True`/`False`, ints plain, floats by `repr`.

use std::io::{self, Write};

use serde_json::Value;

use crate::pyjson::float_repr;

/// Python `str()` of a JSON scalar as csv would stringify it.
pub fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                float_repr(n.as_f64().unwrap_or(0.0))
            }
        }
        // A non-finite float arrives carrying pyjson's raw marker. The
        // csv writer calls str() on it, which spells them -inf / inf /
        // nan — not the Infinity / NaN tokens the json module uses.
        Value::String(s) => match s.strip_prefix(crate::pyjson::RAW_MARKER) {
            Some("Infinity") => "inf".to_string(),
            Some("-Infinity") => "-inf".to_string(),
            Some("NaN") => "nan".to_string(),
            Some(other) => other.to_string(),
            None => s.clone(),
        },
        // Lists/dicts are rare in a CSV cell; Python would print their repr.
        // Close enough for the audit exports this serves.
        other => crate::pyjson::dumps(other, crate::pyjson::Style::COMPACT),
    }
}

fn needs_quotes(field: &str) -> bool {
    field.contains(',') || field.contains('"') || field.contains('\r') || field.contains('\n')
}

/// Write one row. `fields` are already stringified.
pub fn write_row<W: Write>(out: &mut W, fields: &[String]) -> io::Result<()> {
    let mut line = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            line.push(',');
        }
        if needs_quotes(f) || (fields.len() == 1 && f.is_empty()) {
            line.push('"');
            line.push_str(&f.replace('"', "\"\""));
            line.push('"');
        } else {
            line.push_str(f);
        }
    }
    line.push_str("\r\n");
    out.write_all(line.as_bytes())
}

/// `DictWriter(fieldnames=cols)`: header, then each row's values in column
/// order. A missing key writes an empty cell (`restval=""`); extra keys are
/// ignored (`extrasaction="ignore"`).
pub fn write_dicts<W: Write>(
    out: &mut W,
    cols: &[&str],
    rows: &[serde_json::Map<String, Value>],
) -> io::Result<()> {
    write_row(out, &cols.iter().map(|c| c.to_string()).collect::<Vec<_>>())?;
    for row in rows {
        let fields: Vec<String> = cols
            .iter()
            .map(|c| row.get(*c).map(cell).unwrap_or_default())
            .collect();
        write_row(out, &fields)?;
    }
    Ok(())
}

/// `csv.reader` on a file opened with `newline=""`: the C state machine
/// from `_csv.c`, default dialect. Quoted fields may span lines and keep
/// their line endings verbatim; a quote inside an unquoted field is
/// literal; a doubled quote inside a quoted field is one quote. A blank
/// line yields an empty row.
pub fn read_rows(text: &str) -> Vec<Vec<String>> {
    #[derive(PartialEq)]
    enum S {
        StartRecord,
        StartField,
        InField,
        InQuoted,
        QuoteInQuoted,
        EatCrnl,
    }
    struct Machine {
        rows: Vec<Vec<String>>,
        fields: Vec<String>,
        field: String,
        state: S,
    }
    impl Machine {
        fn save(&mut self) {
            self.fields.push(std::mem::take(&mut self.field));
        }
        /// The sentinel the C reader feeds after every physical line. A
        /// record closes when it leaves the machine in StartRecord.
        fn eol(&mut self) {
            match self.state {
                S::StartField | S::InField | S::QuoteInQuoted => {
                    self.save();
                    self.state = S::StartRecord;
                }
                S::EatCrnl => self.state = S::StartRecord,
                S::InQuoted => return, // the field continues on the next line
                S::StartRecord => {}
            }
            self.rows.push(std::mem::take(&mut self.fields));
        }
    }
    let mut m = Machine {
        rows: Vec::new(),
        fields: Vec::new(),
        field: String::new(),
        state: S::StartRecord,
    };

    // Lines split the way `newline=""` does — on \n, \r or \r\n, with the
    // terminator kept as ordinary characters.
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let Machine {
            ref mut fields,
            ref mut field,
            ref mut state,
            ..
        } = m;
        match state {
            S::StartRecord => {
                if c == '\n' || c == '\r' {
                    *state = S::EatCrnl;
                } else {
                    *state = S::StartField;
                    continue; // re-dispatch this char
                }
            }
            S::StartField => {
                if c == '\n' || c == '\r' {
                    fields.push(std::mem::take(field));
                    *state = S::EatCrnl;
                } else if c == '"' {
                    *state = S::InQuoted;
                } else if c == ',' {
                    fields.push(std::mem::take(field));
                } else {
                    field.push(c);
                    *state = S::InField;
                }
            }
            S::InField => {
                if c == '\n' || c == '\r' {
                    fields.push(std::mem::take(field));
                    *state = S::EatCrnl;
                } else if c == ',' {
                    fields.push(std::mem::take(field));
                    *state = S::StartField;
                } else {
                    field.push(c);
                }
            }
            S::InQuoted => {
                if c == '"' {
                    *state = S::QuoteInQuoted;
                } else {
                    field.push(c);
                }
            }
            S::QuoteInQuoted => {
                if c == '"' {
                    field.push('"');
                    *state = S::InQuoted;
                } else if c == ',' {
                    fields.push(std::mem::take(field));
                    *state = S::StartField;
                } else if c == '\n' || c == '\r' {
                    fields.push(std::mem::take(field));
                    *state = S::EatCrnl;
                } else {
                    // Non-strict dialect: text after a closing quote is kept.
                    field.push(c);
                    *state = S::InField;
                }
            }
            S::EatCrnl => {
                // Anything but a line ending here is a Python csv.Error;
                // a file this crate wrote never produces one.
            }
        }
        i += 1;
        if c == '\n' || (c == '\r' && chars.get(i) != Some(&'\n')) {
            m.eol();
        }
    }
    // A final line with no terminator still gets its sentinel.
    if !text.is_empty() && !text.ends_with('\n') && !text.ends_with('\r') {
        m.eol();
    }
    // End of input inside a quoted field: the non-strict reader keeps
    // what it has.
    if m.state == S::InQuoted {
        m.save();
        m.rows.push(std::mem::take(&mut m.fields));
    }
    m.rows
}

/// `csv.DictReader`: the first row names the columns. A short row leaves
/// its missing columns absent (Python's `None`); a long row's extras are
/// dropped; a blank line is skipped.
pub fn read_dicts(text: &str) -> Vec<indexmap::IndexMap<String, String>> {
    let mut rows = read_rows(text).into_iter();
    let Some(header) = rows.next() else {
        return Vec::new();
    };
    rows.filter(|r| !r.is_empty())
        .map(|r| header.iter().cloned().zip(r).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(text: &str) -> Vec<Vec<&'static str>> {
        read_rows(text)
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|s| &*Box::leak(s.into_boxed_str()))
                    .collect()
            })
            .collect()
    }

    /// Every case here was run through CPython's csv module; the expected
    /// values are what it returned.
    #[test]
    fn reader_matches_cpython_state_machine() {
        assert_eq!(rows("a,b\r\n1,2\r\n"), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(
            rows("a,b\r\n\"x,y\",\"q\"\"r\"\r\n"),
            vec![vec!["a", "b"], vec!["x,y", "q\"r"]]
        );
        assert_eq!(
            rows("a,b\r\n\"multi\nline\",z\r\n"),
            vec![vec!["a", "b"], vec!["multi\nline", "z"]]
        );
        assert_eq!(
            rows("a,b\r\n\"a\r\nb\",c\r\n"),
            vec![vec!["a", "b"], vec!["a\r\nb", "c"]]
        );
        assert_eq!(
            rows("a,b\r\n\r\n1,2\r\n"),
            vec![vec!["a", "b"], vec![], vec!["1", "2"]]
        );
        assert_eq!(rows("a,b\r\n1\r\n"), vec![vec!["a", "b"], vec!["1"]]);
        assert_eq!(rows("a,b\n1,2\n"), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(rows("a,b\r1,2\r"), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(rows("a,b\r\n1,2"), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(
            rows("a,b\r\nab\"c,d\r\n"),
            vec![vec!["a", "b"], vec!["ab\"c", "d"]]
        );
        assert_eq!(
            rows("a,b\r\n\"ab\"c,d\r\n"),
            vec![vec!["a", "b"], vec!["abc", "d"]]
        );
        assert_eq!(rows("a,b\r\n\"\",\r\n"), vec![vec!["a", "b"], vec!["", ""]]);
        assert_eq!(rows("a,b\r\n ,\r\n"), vec![vec!["a", "b"], vec![" ", ""]]);
        assert_eq!(
            rows("a,b\r\n1,\"2\"\r\n\r\n"),
            vec![vec!["a", "b"], vec!["1", "2"], vec![]]
        );
        assert_eq!(rows(""), Vec::<Vec<&str>>::new());
    }

    #[test]
    fn dict_reader_short_long_and_blank_rows() {
        let d = read_dicts("a,b\r\n1\r\n\r\n1,2,3\r\n");
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].get("a").map(String::as_str), Some("1"));
        assert_eq!(d[0].get("b"), None, "short row: column absent, like None");
        assert_eq!(d[1].get("b").map(String::as_str), Some("2"));
        assert_eq!(d[1].len(), 2, "extras under None are dropped");
    }

    #[test]
    fn writer_output_reads_back_verbatim() {
        let mut buf = Vec::new();
        let fields = ["plain", "with,comma", "with \"quote\"", "multi\nline", ""];
        write_row(&mut buf, &fields.map(String::from)).unwrap();
        let back = read_rows(std::str::from_utf8(&buf).unwrap());
        assert_eq!(back, vec![fields.map(String::from).to_vec()]);
    }

    fn row(fields: &[&str]) -> String {
        let mut buf = Vec::new();
        write_row(
            &mut buf,
            &fields.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn quoting_matches_python_minimal() {
        assert_eq!(
            row(&["a", "b,c", "d\"e", "f\ng", "", "x y", " lead"]),
            "a,\"b,c\",\"d\"\"e\",\"f\ng\",,x y, lead\r\n"
        );
        assert_eq!(row(&[""]), "\"\"\r\n");
        assert_eq!(row(&["", ""]), ",\r\n");
    }

    #[test]
    fn cells_render_like_python_str() {
        assert_eq!(cell(&json!(5)), "5");
        assert_eq!(cell(&json!(5.0)), "5.0");
        assert_eq!(cell(&json!(0.3)), "0.3");
        assert_eq!(cell(&json!(1e-5)), "1e-05");
        assert_eq!(cell(&json!(true)), "True");
        assert_eq!(cell(&Value::Null), "");
        assert_eq!(cell(&json!("é")), "é");
    }

    #[test]
    fn non_finite_floats_use_python_str_spelling() {
        use crate::pyjson::py_float;
        assert_eq!(cell(&py_float(f64::NEG_INFINITY)), "-inf");
        assert_eq!(cell(&py_float(f64::INFINITY)), "inf");
        assert_eq!(cell(&py_float(f64::NAN)), "nan");
        assert_eq!(cell(&py_float(-45.45)), "-45.45");
    }

    #[test]
    fn dictwriter_fills_missing_and_ignores_extra() {
        let mut buf = Vec::new();
        let r1: serde_json::Map<String, Value> = json!({"a": 1, "b": "x", "zzz": 9})
            .as_object()
            .unwrap()
            .clone();
        let r2: serde_json::Map<String, Value> = json!({"b": "y"}).as_object().unwrap().clone();
        write_dicts(&mut buf, &["a", "b"], &[r1, r2]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "a,b\r\n1,x\r\n,y\r\n");
    }
}

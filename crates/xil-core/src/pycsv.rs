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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

//! Stem filename conventions, shared by the migrator, the cleanup pass and
//! (later) the mixer. Port of the naming helpers in `mix_common.py` and
//! `XILP007_stem_migrator.py`.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value};

use crate::fsutil::basename;

/// `{seq:03d}_{section}[-{scene}]_{speaker|sfx}` — the stem basename for a
/// parsed entry, with no extension.
pub fn expected_stem_basename(entry: &Map<String, Value>) -> String {
    let seq = entry.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let section = entry.get("section").and_then(Value::as_str).unwrap_or("");
    let mut name = format!("{seq:03}_{section}");
    if let Some(scene) = entry
        .get("scene")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        name.push('-');
        name.push_str(scene);
    }
    if entry.get("type").and_then(Value::as_str) == Some("dialogue") {
        name.push('_');
        name.push_str(entry.get("speaker").and_then(Value::as_str).unwrap_or(""));
    } else {
        name.push_str("_sfx");
    }
    name
}

/// Sequence number from a stem filename. Legacy preamble stems used an
/// `n` prefix for negative seqs and are still parsed.
///
/// `None` where Python would raise `ValueError`.
pub fn extract_seq(filepath: &Path) -> Option<i64> {
    let name = basename(filepath);
    let stem = match (name.rfind('.'), name.len()) {
        (Some(dot), _) if dot > 0 => &name[..dot],
        _ => &name[..],
    };
    let prefix = stem.split('_').next().unwrap_or("");
    if let Some(digits) = prefix.strip_prefix('n') {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return digits.parse::<i64>().ok().map(|n| -n);
        }
    }
    // Python's int() accepts surrounding whitespace and a sign.
    prefix.trim().parse::<i64>().ok()
}

/// `{seq: entry}` index over a parsed script's entries.
pub fn entries_index(parsed: &Value) -> HashMap<i64, Map<String, Value>> {
    parsed
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let o = e.as_object()?;
                    Some((o.get("seq")?.as_i64()?, o.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn basename_shapes() {
        assert_eq!(
            expected_stem_basename(&obj(
                json!({"seq": 3, "section": "cold-open", "scene": "scene-1", "type": "dialogue", "speaker": "adam"})
            )),
            "003_cold-open-scene-1_adam"
        );
        assert_eq!(
            expected_stem_basename(&obj(
                json!({"seq": 12, "section": "act1", "scene": null, "type": "direction"})
            )),
            "012_act1_sfx"
        );
        assert_eq!(
            expected_stem_basename(&obj(
                json!({"seq": 100, "section": "", "type": "dialogue", "speaker": "x"})
            )),
            "100__x"
        );
    }

    #[test]
    fn seq_extraction_including_legacy_negatives() {
        assert_eq!(
            extract_seq(Path::new("stems/S01E01/003_cold-open_adam.mp3")),
            Some(3)
        );
        assert_eq!(extract_seq(Path::new("n002_preamble_tina.mp3")), Some(-2));
        assert_eq!(extract_seq(Path::new("nope_x.mp3")), None);
        assert_eq!(extract_seq(Path::new("notanumber.mp3")), None);
        assert_eq!(extract_seq(Path::new("017.mp3")), Some(17));
    }

    #[test]
    fn index_keys_on_seq() {
        let parsed =
            json!({"entries": [{"seq": 1, "type": "dialogue"}, {"seq": 5, "type": "direction"}]});
        let idx = entries_index(&parsed);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[&5]["type"], "direction");
        assert!(entries_index(&json!({})).is_empty());
    }
}

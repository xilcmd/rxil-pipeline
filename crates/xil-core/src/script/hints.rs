//! Scriptwriter pipe-hints on a direction: a source filename and per-cue
//! attribute overrides. Port of `_parse_direction_hint` and friends.

use indexmap::IndexMap;

/// Script-facing hint name → the `SfxEntry` field it populates. Add a key
/// here to support a new attribute; nothing else needs to change.
pub const HINT_ATTRS: [(&str, &str); 2] = [
    ("play_volume_pct", "volume_percentage"),
    ("play_duration_pct", "play_duration"),
];

/// Accepted range per target field, mirroring the SfxEntry validators.
pub fn hint_range(field: &str) -> Option<(f64, f64)> {
    match field {
        "volume_percentage" => Some((0.0, 200.0)),
        "play_duration" => Some((0.0, 100.0)),
        _ => None,
    }
}

/// Cue prefixes whose layers loop, so a play_duration percentage is
/// meaningless — the mixer only honours it for MUSIC / SFX / BEAT.
pub const LOOPED_CUE_PREFIXES: [&str; 2] = ["AMBIENCE:", "VINTAGE FILTER"];

/// What a direction's pipe-hints yielded.
#[derive(Debug, Default, PartialEq)]
pub struct Hints {
    /// The direction text with consumed segments removed.
    pub clean: String,
    /// `SFX/<slug>/<file>` when a filename segment was present.
    pub source: Option<String>,
    /// Config-field-keyed overrides.
    pub overrides: IndexMap<String, f64>,
}

/// Split `raw` into its direction text, source hint and attribute hints.
///
/// Segments after the first are classified independently and order-free: a
/// `.mp3`/`.wav` is the source (first wins), a `key=value` whose key is in
/// [`HINT_ATTRS`] is an override, anything else is re-joined onto the text
/// so a writer's prose note survives verbatim.
///
/// `warn` receives the message Python logs for a malformed value; one bad
/// hint must never abort a whole script parse.
pub fn parse_direction_hint(raw: &str, slug: &str, warn: &mut dyn FnMut(String)) -> Hints {
    if !raw.contains(" | ") {
        return Hints {
            clean: raw.trim().to_string(),
            source: None,
            overrides: IndexMap::new(),
        };
    }

    let parts: Vec<&str> = raw.split(" | ").collect();
    let head = parts[0].trim().to_string();
    let mut source: Option<String> = None;
    let mut overrides: IndexMap<String, f64> = IndexMap::new();
    let mut unconsumed: Vec<String> = Vec::new();

    for seg in &parts[1..] {
        let seg = seg.trim();
        if seg.ends_with(".mp3") || seg.ends_with(".wav") {
            if source.is_none() {
                let prefix = if slug.is_empty() {
                    "SFX".to_string()
                } else {
                    format!("SFX/{slug}")
                };
                source = Some(format!("{prefix}/{seg}"));
            } else {
                unconsumed.push(seg.to_string());
            }
            continue;
        }
        let Some((key, value)) = seg.split_once('=') else {
            unconsumed.push(seg.to_string());
            continue;
        };
        let field = HINT_ATTRS
            .iter()
            .find(|(k, _)| *k == key.trim())
            .map(|(_, f)| *f);
        let Some(field) = field else {
            unconsumed.push(seg.to_string());
            continue;
        };
        match parse_hint_value(field, value.trim(), raw, warn) {
            Some(num) => {
                overrides.insert(field.to_string(), num);
            }
            None => unconsumed.push(seg.to_string()),
        }
    }

    let clean = if unconsumed.is_empty() {
        head
    } else {
        let mut all = vec![head];
        all.extend(unconsumed);
        all.join(" | ")
    };
    Hints {
        clean,
        source,
        overrides,
    }
}

fn parse_hint_value(
    field: &str,
    value: &str,
    raw: &str,
    warn: &mut dyn FnMut(String),
) -> Option<f64> {
    let (lo, hi) = hint_range(field)?;
    let cleaned = value.trim_end_matches('%').trim();
    let Ok(num) = cleaned.parse::<f64>() else {
        warn(format!(
            "  Ignoring non-numeric hint value in [{raw}]: {}",
            py_repr(value)
        ));
        return None;
    };
    if !(lo..=hi).contains(&num) {
        warn(format!(
            "  Ignoring out-of-range {field} in [{raw}]: {value} (expected {}–{})",
            fmt_g(lo),
            fmt_g(hi)
        ));
        return None;
    }
    Some(num)
}

/// Python `repr()` of a string: single quotes unless the value contains one.
pub fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// Python's `%g` / `f"{v:g}"`: shortest form, trailing zeros dropped.
pub fn fmt_g(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e16 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:e}");
    let exp: i32 = s
        .split_once('e')
        .map(|(_, e)| e.parse().unwrap_or(0))
        .unwrap_or(0);
    if (-5..6).contains(&exp) {
        let mut out = format!("{v:.*}", (5 - exp).max(0) as usize);
        if out.contains('.') {
            out = out.trim_end_matches('0').trim_end_matches('.').to_string();
        }
        out
    } else {
        format!("{v:e}")
    }
}

/// Render an override back into script-hint form (`play_volume_pct=20%`).
/// Inverse of the [`HINT_ATTRS`] lookup, used by the script regenerator.
pub fn format_hint_attr(field: &str, value: f64) -> String {
    let name = HINT_ATTRS
        .iter()
        .find(|(_, f)| *f == field)
        .map(|(k, _)| *k)
        .unwrap_or(field);
    format!("{name}={}%", fmt_g(value))
}

/// Narrow attribute hints to the ones this cue can use: silence cues take
/// none, looped layers drop `play_duration`.
pub fn filter_sfx_overrides(
    key_text: &str,
    entry_is_silence: bool,
    overrides: &IndexMap<String, f64>,
    warn: Option<&mut dyn FnMut(String)>,
) -> IndexMap<String, f64> {
    if overrides.is_empty() || entry_is_silence {
        return IndexMap::new();
    }
    let mut allowed = overrides.clone();
    if allowed.contains_key("play_duration")
        && LOOPED_CUE_PREFIXES.iter().any(|p| key_text.starts_with(p))
    {
        allowed.shift_remove("play_duration");
        if let Some(w) = warn {
            w(format!(
                "  Ignoring play_duration_pct on looped cue [{key_text}]"
            ));
        }
    }
    allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints(raw: &str, slug: &str) -> (Hints, Vec<String>) {
        let mut warns = Vec::new();
        let h = parse_direction_hint(raw, slug, &mut |m| warns.push(m));
        (h, warns)
    }

    #[test]
    fn no_pipe_means_no_hints() {
        let (h, w) = hints("  SFX: DOOR OPENS  ", "the413");
        assert_eq!(
            h,
            Hints {
                clean: "SFX: DOOR OPENS".into(),
                source: None,
                overrides: IndexMap::new()
            }
        );
        assert!(w.is_empty());
    }

    #[test]
    fn source_and_attributes_are_order_free() {
        let (h, _) = hints(
            "OUTRO MUSIC | play_volume_pct=20% | sundy.mp3 | play_duration_pct=35",
            "the413",
        );
        assert_eq!(h.clean, "OUTRO MUSIC");
        assert_eq!(h.source.as_deref(), Some("SFX/the413/sundy.mp3"));
        assert_eq!(h.overrides["volume_percentage"], 20.0);
        assert_eq!(h.overrides["play_duration"], 35.0);
    }

    #[test]
    fn slugless_source_lands_in_the_flat_dir() {
        let (h, _) = hints("SFX: X | a.wav", "");
        assert_eq!(h.source.as_deref(), Some("SFX/a.wav"));
    }

    #[test]
    fn unknown_segments_are_rejoined_not_swallowed() {
        let (h, _) = hints(
            "SFX: DOOR | some notes | second.mp3 | first.mp3 | nope=1",
            "s",
        );
        assert_eq!(
            h.source.as_deref(),
            Some("SFX/s/second.mp3"),
            "first filename wins"
        );
        assert_eq!(h.clean, "SFX: DOOR | some notes | first.mp3 | nope=1");
    }

    #[test]
    fn bad_values_warn_and_are_kept_as_text() {
        let (h, w) = hints("SFX: X | play_volume_pct=abc", "s");
        assert_eq!(h.clean, "SFX: X | play_volume_pct=abc");
        assert_eq!(
            w,
            vec!["  Ignoring non-numeric hint value in [SFX: X | play_volume_pct=abc]: 'abc'"]
        );

        let (h, w) = hints("SFX: X | play_volume_pct=500", "s");
        assert_eq!(h.clean, "SFX: X | play_volume_pct=500");
        assert_eq!(w[0], "  Ignoring out-of-range volume_percentage in [SFX: X | play_volume_pct=500]: 500 (expected 0–200)");
    }

    #[test]
    fn looped_cues_drop_play_duration_only() {
        let mut o = IndexMap::new();
        o.insert("play_duration".to_string(), 50.0);
        o.insert("volume_percentage".to_string(), 30.0);
        let mut warns = Vec::new();
        let kept = filter_sfx_overrides("AMBIENCE: RAIN", false, &o, Some(&mut |m| warns.push(m)));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept["volume_percentage"], 30.0);
        assert_eq!(
            warns,
            vec!["  Ignoring play_duration_pct on looped cue [AMBIENCE: RAIN]"]
        );
        assert!(
            filter_sfx_overrides("BEAT", true, &o, None).is_empty(),
            "silence takes none"
        );
        assert_eq!(filter_sfx_overrides("SFX: X", false, &o, None).len(), 2);
    }

    #[test]
    fn hint_attr_round_trips_without_trailing_zeros() {
        assert_eq!(
            format_hint_attr("volume_percentage", 20.0),
            "play_volume_pct=20%"
        );
        assert_eq!(
            format_hint_attr("play_duration", 35.5),
            "play_duration_pct=35.5%"
        );
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(100.0), "100");
    }
}

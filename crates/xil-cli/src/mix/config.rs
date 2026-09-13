//! The slices of the cast and SFX config models the mixing stages read.
//! Port of the relevant parts of `CastConfiguration`, `VoiceConfig`,
//! `SfxEntry` and `SfxConfiguration` in `models.py`.
//!
//! Numbers keep their Python type. Pydantic coerces a model field declared
//! `float` to a float, but the `defaults` block is a plain dict, so a
//! category default of `80` stays an int — and prints as `80`, not `80.0`,
//! in the timeline's JSON.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;
use xil_core::pyjson::py_float;
use xil_core::workspace::episode_tag;

/// A JSON number with its Python type preserved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    pub fn from_value(v: &Value) -> Option<Num> {
        match v {
            Value::Number(n) => Some(match n.as_i64() {
                Some(i) if !n.is_f64() => Num::Int(i),
                _ => Num::Float(n.as_f64()?),
            }),
            Value::Bool(b) => Some(Num::Int(*b as i64)),
            _ => None,
        }
    }

    pub fn f(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(x) => x,
        }
    }

    /// `bool(x)`.
    pub fn truthy(self) -> bool {
        self.f() != 0.0
    }

    pub fn to_json(self) -> Value {
        match self {
            Num::Int(i) => Value::from(i),
            Num::Float(x) => py_float(x),
        }
    }

    /// `int(x * 1000)` — ms from seconds, truncating like Python.
    pub fn to_ms(self) -> i64 {
        match self {
            Num::Int(i) => i * 1000,
            Num::Float(x) => (x * 1000.0) as i64,
        }
    }
}

/// A float field of a pydantic model: an int in the JSON becomes a float.
fn model_float(v: Option<&Value>) -> Option<f64> {
    v.and_then(Num::from_value).map(Num::f)
}

/// The `filter` field: `str | bool | None`.
#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    None,
    Bool(bool),
    Str(String),
}

impl Filter {
    fn from_value(v: Option<&Value>) -> Filter {
        match v {
            Some(Value::Bool(b)) => Filter::Bool(*b),
            Some(Value::String(s)) => Filter::Str(s.clone()),
            _ => Filter::None,
        }
    }

    /// `bool(filter)`.
    pub fn truthy(&self) -> bool {
        match self {
            Filter::None => false,
            Filter::Bool(b) => *b,
            Filter::Str(s) => !s.is_empty(),
        }
    }

    /// `str(filter)` for a truthy value, as f-strings print it.
    pub fn py_str(&self) -> String {
        match self {
            Filter::None => "None".into(),
            Filter::Bool(true) => "True".into(),
            Filter::Bool(false) => "False".into(),
            Filter::Str(s) => s.clone(),
        }
    }
}

/// `VoiceConfig(...).model_dump()` — what the mixers look a speaker up in.
#[derive(Clone, Debug)]
pub struct Voice {
    pub pan: f64,
    pub filter: Filter,
    pub full_name: String,
    pub voice_id: String,
    /// The member's JSON object, for fields only one stage reads.
    #[allow(dead_code)]
    pub raw: Map<String, Value>,
}

#[derive(Clone, Debug)]
pub struct CastConfig {
    pub show: String,
    pub title: Option<String>,
    pub season_title: Option<String>,
    pub artist: String,
    pub tag: String,
    /// Speaker key → voice settings, in file order.
    pub cast: IndexMap<String, Voice>,
}

fn opt_str(o: &Map<String, Value>, k: &str) -> Option<String> {
    o.get(k).and_then(Value::as_str).map(str::to_string)
}

/// Pydantic's checks on one `CastMember`, as the first error raised.
fn validate_member(key: &str, v: &Value) -> anyhow::Result<()> {
    let fail = |loc: &str, what: &str| {
        anyhow!("1 validation error for CastConfiguration\ncast.{key}.{loc}\n  {what}")
    };
    let Some(o) = v.as_object() else {
        return Err(fail(
            "",
            "Input should be a valid dictionary or instance of CastMember",
        ));
    };
    for field in ["full_name", "voice_id", "role"] {
        match o.get(field) {
            None => return Err(fail(field, "Field required")),
            Some(Value::String(_)) => {}
            Some(_) => return Err(fail(field, "Input should be a valid string")),
        }
    }
    if !o.contains_key("filter") {
        return Err(fail("filter", "Field required"));
    }
    let ranged = |field: &str, lo: f64, hi: f64, required: bool| -> anyhow::Result<()> {
        match o.get(field) {
            None if required => Err(fail(field, "Field required")),
            None | Some(Value::Null) if !required => Ok(()),
            Some(val) => match Num::from_value(val).map(Num::f) {
                Some(x) if x >= lo && x <= hi => Ok(()),
                Some(_) => Err(fail(field, "Input should be within range")),
                None => Err(fail(field, "Input should be a valid number")),
            },
            None => Ok(()),
        }
    };
    ranged("pan", -1.0, 1.0, true)?;
    ranged("stability", 0.0, 1.0, false)?;
    ranged("similarity_boost", 0.0, 1.0, false)?;
    ranged("style", 0.0, 1.0, false)?;
    ranged("speed", 0.7, 1.5, false)?;
    Ok(())
}

impl CastConfig {
    pub fn load(path: &Path) -> anyhow::Result<CastConfig> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let o = v
            .as_object()
            .ok_or_else(|| anyhow!("1 validation error for CastConfiguration"))?;
        let show = opt_str(o, "show").ok_or_else(|| {
            anyhow!("1 validation error for CastConfiguration\nshow\n  Field required")
        })?;
        let tag = match opt_str(o, "tag_override").filter(|s| !s.is_empty()) {
            Some(t) => t,
            None => {
                let episode = o.get("episode").and_then(Value::as_i64).ok_or_else(|| {
                    anyhow!("CastConfiguration requires either tag_override or episode")
                })?;
                episode_tag(o.get("season").and_then(Value::as_i64), episode)
            }
        };
        let Some(members_raw) = o.get("cast").and_then(Value::as_object) else {
            anyhow::bail!("1 validation error for CastConfiguration\ncast\n  Field required");
        };
        for (key, m) in members_raw {
            validate_member(key, m)?;
        }
        let mut cast = IndexMap::new();
        if let Some(members) = Some(members_raw) {
            for (key, m) in members {
                let m = m.as_object().cloned().unwrap_or_default();
                cast.insert(
                    key.clone(),
                    Voice {
                        pan: model_float(m.get("pan")).unwrap_or(0.0),
                        filter: Filter::from_value(m.get("filter")),
                        full_name: opt_str(&m, "full_name").unwrap_or_default(),
                        voice_id: opt_str(&m, "voice_id").unwrap_or_default(),
                        raw: m.clone(),
                    },
                );
            }
        }
        Ok(CastConfig {
            show,
            title: opt_str(o, "title"),
            season_title: opt_str(o, "season_title"),
            artist: opt_str(o, "artist").unwrap_or_else(|| "XIL Pipeline".into()),
            tag,
            cast,
        })
    }
}

/// One effect entry, with pydantic's defaults applied.
#[derive(Clone, Debug)]
pub struct SfxEntry {
    pub prompt: Option<String>,
    /// `"sfx"` or `"silence"`.
    pub type_: String,
    pub prompt_influence: Option<f64>,
    pub volume_percentage: Option<f64>,
    pub ramp_in_seconds: Option<f64>,
    pub ramp_out_seconds: Option<f64>,
    pub play_duration: Option<f64>,
    pub loop_: bool,
    pub source: Option<String>,
    pub duration_seconds: f64,
}

impl SfxEntry {
    fn from_value(v: &Value) -> SfxEntry {
        let o = v.as_object().cloned().unwrap_or_default();
        SfxEntry {
            prompt: opt_str(&o, "prompt"),
            type_: opt_str(&o, "type").unwrap_or_else(|| "sfx".into()),
            prompt_influence: model_float(o.get("prompt_influence")),
            volume_percentage: model_float(o.get("volume_percentage")),
            ramp_in_seconds: model_float(o.get("ramp_in_seconds")),
            ramp_out_seconds: model_float(o.get("ramp_out_seconds")),
            play_duration: model_float(o.get("play_duration")),
            loop_: o.get("loop").and_then(Value::as_bool).unwrap_or(false),
            source: opt_str(&o, "source"),
            duration_seconds: model_float(o.get("duration_seconds")).unwrap_or(5.0),
        }
    }
}

/// Pydantic's checks on one `SfxEntry`, as the first error it would raise.
fn validate_entry(key: &str, v: &Value) -> anyhow::Result<()> {
    let fail =
        |what: String| anyhow!("1 validation error for SfxConfiguration\neffects.{key}\n  {what}");
    let Some(o) = v.as_object() else {
        return Err(fail(
            "Input should be a valid dictionary or instance of SfxEntry".into(),
        ));
    };
    let bad: Vec<&str> = o
        .keys()
        .map(String::as_str)
        .filter(|k| {
            matches!(
                *k,
                "ambience_volume_percentage"
                    | "music_volume_percentage"
                    | "sfx_volume_percentage"
                    | "vintage_filter_volume_percentage"
            )
        })
        .collect();
    if !bad.is_empty() {
        return Err(fail(format!(
            "Value error, Unknown field(s) in SfxEntry: {bad:?}"
        )));
    }
    let ty = o.get("type").and_then(Value::as_str).unwrap_or("sfx");
    if o.contains_key("type") && !(ty == "sfx" || ty == "silence") {
        return Err(fail("Input should be 'sfx' or 'silence'".into()));
    }
    let range = |k: &str, lo: f64, hi: Option<f64>| -> anyhow::Result<()> {
        match o.get(k) {
            None | Some(Value::Null) if k != "duration_seconds" => Ok(()),
            None => Ok(()),
            Some(val) => match Num::from_value(val).map(Num::f) {
                Some(x) if x >= lo && hi.is_none_or_le(x) => Ok(()),
                Some(_) => Err(fail(format!("{k}: Input should be within range"))),
                None => Err(fail(format!("{k}: Input should be a valid number"))),
            },
        }
    };
    range("duration_seconds", 0.0, None)?;
    range("prompt_influence", 0.0, Some(1.0))?;
    range("volume_percentage", 0.0, Some(200.0))?;
    range("ramp_in_seconds", 0.0, Some(30.0))?;
    range("ramp_out_seconds", 0.0, Some(30.0))?;
    range("play_duration", 0.0, Some(100.0))?;
    let source_is_none = o.get("source").is_none_or_null();
    let duration = model_float(o.get("duration_seconds")).unwrap_or(5.0);
    if ty == "sfx" && source_is_none {
        if duration == 0.0 {
            return Err(fail(
                "Value error, duration_seconds must be > 0 for API-generated effects; use type='silence' for stop markers".into(),
            ));
        }
        if duration > 30.0 {
            return Err(fail(format!(
                "Value error, duration_seconds must be ≤ 30.0 for API-generated effects (got {}); set source= for pre-existing files",
                crate::mix::config::py_float_str(duration)
            )));
        }
    }
    Ok(())
}

/// `str(float)` in Python.
pub fn py_float_str(x: f64) -> String {
    xil_core::pyjson::float_repr(x)
}

trait OptLe {
    fn is_none_or_le(&self, x: f64) -> bool;
}

impl OptLe for Option<f64> {
    fn is_none_or_le(&self, x: f64) -> bool {
        self.map_or(true, |hi| x <= hi)
    }
}

trait NullishExt {
    fn is_none_or_null(&self) -> bool;
}

impl NullishExt for Option<&Value> {
    fn is_none_or_null(&self) -> bool {
        self.map_or(true, Value::is_null)
    }
}

#[derive(Clone, Debug)]
pub struct SfxConfig {
    pub show: String,
    pub defaults: Map<String, Value>,
    /// NFC-normalised effect key → entry, in file order.
    pub effects: IndexMap<String, SfxEntry>,
    pub vintage_scenes: Vec<String>,
}

impl SfxConfig {
    pub fn load(path: &Path) -> anyhow::Result<SfxConfig> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let o = v
            .as_object()
            .ok_or_else(|| anyhow!("1 validation error for SfxConfiguration"))?;
        let show = match o.get("show") {
            Some(Value::String(s)) => s.clone(),
            _ => anyhow::bail!("1 validation error for SfxConfiguration\nshow\n  Field required"),
        };
        let Some(raw_effects) = o.get("effects").and_then(Value::as_object) else {
            anyhow::bail!("1 validation error for SfxConfiguration\neffects\n  Field required");
        };
        for (k, entry) in raw_effects {
            validate_entry(k, entry)?;
        }
        let mut effects = IndexMap::new();
        if let Some(e) = Some(raw_effects) {
            for (k, entry) in e {
                let mut entry = SfxEntry::from_value(entry);
                entry.source = entry.source.map(|s| s.nfc().collect());
                effects.insert(k.nfc().collect::<String>(), entry);
            }
        }
        Ok(SfxConfig {
            show,
            defaults: o
                .get("defaults")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            effects,
            vintage_scenes: o
                .get("vintage_scenes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// `defaults.get(key, defaults.get(fallback))` — a present `null` wins.
    pub fn default_num(&self, key: &str, fallback: &str) -> Option<Num> {
        match self.defaults.get(key) {
            Some(v) => Num::from_value(v),
            None => self.defaults.get(fallback).and_then(Num::from_value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_keep_their_python_type() {
        assert_eq!(Num::from_value(&json!(80)), Some(Num::Int(80)));
        assert_eq!(Num::from_value(&json!(80.0)), Some(Num::Float(80.0)));
        assert_eq!(Num::from_value(&json!(null)), None);
        assert_eq!(Num::Float(1.5).to_ms(), 1500);
        assert_eq!(Num::Float(0.0015).to_ms(), 1);
    }

    #[test]
    fn filter_prints_like_python() {
        assert_eq!(Filter::from_value(Some(&json!(true))).py_str(), "True");
        assert!(!Filter::from_value(Some(&json!(""))).truthy());
        assert!(!Filter::from_value(Some(&json!(null))).truthy());
    }
}

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
        let mut cast = IndexMap::new();
        if let Some(members) = o.get("cast").and_then(Value::as_object) {
            for (key, m) in members {
                let m = m.as_object().cloned().unwrap_or_default();
                cast.insert(
                    key.clone(),
                    Voice {
                        pan: model_float(m.get("pan")).unwrap_or(0.0),
                        filter: Filter::from_value(m.get("filter")),
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

#[derive(Clone, Debug)]
pub struct SfxConfig {
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
        let mut effects = IndexMap::new();
        if let Some(e) = o.get("effects").and_then(Value::as_object) {
            for (k, entry) in e {
                let mut entry = SfxEntry::from_value(entry);
                entry.source = entry.source.map(|s| s.nfc().collect());
                effects.insert(k.nfc().collect::<String>(), entry);
            }
        }
        Ok(SfxConfig {
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

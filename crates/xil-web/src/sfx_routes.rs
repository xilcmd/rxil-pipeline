//! The timeline editor's JSON routes, with the contracts of
//! `_register_sfx_routes` in `xil_gui.py`:
//!
//! - `GET /xil/get-sfx?slug=&tag=&key=` → `{effect, defaults, natural_s}`
//! - `POST /xil/update-sfx` → sets or clears one cue's four sound fields
//! - `POST /xil/update-sfx-defaults` → sets or clears one layer's defaults
//!
//! Every save rewrites `sfx_{tag}.json` and appends to its `_edits.jsonl`
//! journal; a journal failure never fails the save.

// Helpers return the finished HTTP reply as their `Err`, which the handler
// sends straight back; boxing it would only add an allocation per request.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use xil_core::journal::{append_sfx_defaults_edit, append_sfx_edit, SFX_EDIT_FIELDS};
use xil_core::log;
use xil_core::pyjson::{dumps, Style};
use xil_core::script::hints::py_repr;
use xil_core::workspace::{derive_paths, workspace_root};

use crate::activity;
use crate::configs::check_workspace_path;
use crate::episodes::is_safe_slug_or_tag;

const CUE_FIELDS: [&str; 4] = [
    "volume_percentage",
    "ramp_in_seconds",
    "ramp_out_seconds",
    "play_duration",
];
const DEFAULT_FIELDS: [&str; 3] = ["volume_percentage", "ramp_in_seconds", "ramp_out_seconds"];

fn reply(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// Python's `repr()` of a JSON request body, for the debug log.
fn body_repr(v: &Value) -> String {
    dumps(v, Style::COMPACT_UTF8)
}

/// The episode's sfx config, or the error response to send instead.
fn sfx_config_path(slug: &str, tag: &str, ok_key: bool, route: &str) -> Result<PathBuf, Response> {
    let err = |code, msg: &str| {
        let mut body = Map::new();
        if ok_key {
            body.insert("ok".into(), false.into());
        }
        body.insert("error".into(), msg.into());
        reply(code, Value::Object(body))
    };
    if !(is_safe_slug_or_tag(slug) && is_safe_slug_or_tag(tag)) {
        log::warning(&format!(
            "{route} rejected: invalid slug/tag slug={} tag={}",
            py_repr(slug),
            py_repr(tag)
        ));
        return Err(err(StatusCode::BAD_REQUEST, "invalid slug or tag"));
    }
    let path = derive_paths(slug, tag)["sfx"].clone();
    if check_workspace_path(&path).is_err() {
        return Err(err(StatusCode::BAD_REQUEST, "invalid slug or tag"));
    }
    if !path.exists() {
        let msg = format!("{route}: sfx config not found at {}", path.display());
        if ok_key {
            log::warning(&msg);
        } else {
            log::debug(&msg);
        }
        return Err(err(StatusCode::NOT_FOUND, "sfx config not found"));
    }
    Ok(path)
}

fn read_config(path: &Path) -> Result<Map<String, Value>, Response> {
    let parsed = fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    match parsed {
        Some(Value::Object(m)) => Ok(m),
        _ => Err(reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"ok": false, "error": "sfx config is not valid JSON"}),
        )),
    }
}

fn write_config(path: &Path, data: &Map<String, Value>) -> std::io::Result<()> {
    fs::write(
        path,
        dumps(&Value::Object(data.clone()), Style::INDENT2_UTF8) + "\n",
    )
}

/// `_source_duration_s(source)`: a cue's source file length, or `None` for a
/// generated cue or an unreadable file. Relative sources are workspace paths.
fn source_duration_s(source: Option<&str>) -> Option<f64> {
    let source = source.filter(|s| !s.is_empty())?;
    let p = Path::new(source);
    let path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace_root().join(p)
    };
    check_workspace_path(&path).ok()?;
    match xil_audio::mpeg::duration_ms(&path) {
        Ok(ms) if ms > 0 => Some(ms as f64 / 1000.0),
        Ok(_) => None,
        Err(e) => {
            log::debug(&format!("get-sfx: could not probe {source} ({e})"));
            None
        }
    }
}

/// `GET /xil/get-sfx`.
pub async fn get_sfx(Query(q): Query<HashMap<String, String>>) -> Response {
    let get = |k: &str| q.get(k).cloned();
    let (Some(slug), Some(tag), Some(key)) = (get("slug"), get("tag"), get("key")) else {
        return reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"error": "slug, tag and key are required"}),
        );
    };
    log::debug(&format!(
        "get-sfx request: slug={} tag={} key={}",
        py_repr(&slug),
        py_repr(&tag),
        py_repr(&key)
    ));
    let path = match sfx_config_path(&slug, &tag, false, "get-sfx") {
        Ok(p) => p,
        Err(r) => return r,
    };
    let data = match read_config(&path) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let effect = data
        .get("effects")
        .and_then(|e| e.get(&key))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let defaults = data.get("defaults").cloned().unwrap_or_else(|| json!({}));
    let natural_s = source_duration_s(effect.get("source").and_then(Value::as_str));
    log::debug(&format!(
        "get-sfx: returned effect={} defaults={} natural_s={} for key={}",
        body_repr(&effect),
        body_repr(&defaults),
        natural_s
            .map(|n| n.to_string())
            .unwrap_or_else(|| "None".into()),
        py_repr(&key)
    ));
    let mut body = Map::new();
    body.insert("effect".into(), effect);
    body.insert("defaults".into(), defaults);
    body.insert(
        "natural_s".into(),
        natural_s
            .map(xil_core::pyjson::py_float)
            .unwrap_or(Value::Null),
    );
    reply(StatusCode::OK, Value::Object(body))
}

fn body_str(body: &Value, key: &str) -> Result<String, Response> {
    match body.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(reply(
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "error": format!("missing {key}")}),
        )),
    }
}

/// `POST /xil/update-sfx`.
pub async fn update_sfx(Json(body): Json<Value>) -> Response {
    log::debug(&format!("update-sfx request body: {}", body_repr(&body)));
    let fields = (|| {
        Ok::<_, Response>((
            body_str(&body, "slug")?,
            body_str(&body, "tag")?,
            body_str(&body, "key")?,
        ))
    })();
    let (slug, tag, key) = match fields {
        Ok(f) => f,
        Err(r) => return r,
    };
    let path = match sfx_config_path(&slug, &tag, true, "update-sfx") {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut data = match read_config(&path) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut written = Map::new();
    {
        let effects = data.entry("effects").or_insert_with(|| json!({}));
        let Some(effects) = effects.as_object_mut() else {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"ok": false, "error": "effects is not an object"}),
            );
        };
        let effect = effects.entry(key.clone()).or_insert_with(|| json!({}));
        let Some(effect) = effect.as_object_mut() else {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"ok": false, "error": "effect is not an object"}),
            );
        };
        for field in CUE_FIELDS {
            let val = body.get(field).cloned().unwrap_or(Value::Null);
            if val.is_null() {
                effect.shift_remove(field);
            } else {
                effect.insert(field.into(), val.clone());
            }
            written.insert(field.into(), val);
        }
    }
    if let Err(e) = write_config(&path, &data) {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"ok": false, "error": e.to_string()}),
        );
    }
    log::debug(&format!(
        "update-sfx: wrote {} (key={} fields={})",
        path.display(),
        py_repr(&key),
        body_repr(&Value::Object(written))
    ));
    let journal: Map<String, Value> = SFX_EDIT_FIELDS
        .iter()
        .map(|f| (f.to_string(), body.get(*f).cloned().unwrap_or(Value::Null)))
        .collect();
    if let Err(e) = append_sfx_edit(&path, &key, &journal) {
        activity::log(&format!("[WARN] sfx edit journal write failed: {e}"));
        log::warning(&format!(
            "sfx edit journal write failed for {} (key={}): {e}",
            path.display(),
            py_repr(&key)
        ));
    }
    activity::log(&format!(
        "SFX edit via timeline: {slug}/{tag} → {}",
        py_repr(&key)
    ));
    log::info(&format!(
        "SFX edit via timeline: {slug}/{tag} → {}",
        py_repr(&key)
    ));
    reply(
        StatusCode::OK,
        json!({"ok": true, "message": format!("Saved {} — re-run xil daw to apply.", py_repr(&key))}),
    )
}

/// `POST /xil/update-sfx-defaults`: the `{layer}_{field}` defaults.
pub async fn update_sfx_defaults(Json(body): Json<Value>) -> Response {
    log::debug(&format!(
        "update-sfx-defaults request body: {}",
        body_repr(&body)
    ));
    let fields = (|| {
        Ok::<_, Response>((
            body_str(&body, "slug")?,
            body_str(&body, "tag")?,
            body_str(&body, "layer")?,
        ))
    })();
    let (slug, tag, layer) = match fields {
        Ok(f) => f,
        Err(r) => return r,
    };
    if !(is_safe_slug_or_tag(&slug) && is_safe_slug_or_tag(&tag)) {
        log::warning(&format!(
            "update-sfx-defaults rejected: invalid slug/tag slug={} tag={}",
            py_repr(&slug),
            py_repr(&tag)
        ));
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "error": "invalid slug or tag"}),
        );
    }
    if !matches!(layer.as_str(), "music" | "sfx" | "ambience") {
        log::warning(&format!(
            "update-sfx-defaults rejected: invalid layer {}",
            py_repr(&layer)
        ));
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "error": "invalid layer"}),
        );
    }
    let path = match sfx_config_path(&slug, &tag, true, "update-sfx-defaults") {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut data = match read_config(&path) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut written = Map::new();
    {
        let defaults = data.entry("defaults").or_insert_with(|| json!({}));
        let Some(defaults) = defaults.as_object_mut() else {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"ok": false, "error": "defaults is not an object"}),
            );
        };
        for field in DEFAULT_FIELDS {
            let val = body.get(field).cloned().unwrap_or(Value::Null);
            let prefixed = format!("{layer}_{field}");
            if val.is_null() {
                defaults.shift_remove(&prefixed);
            } else {
                defaults.insert(prefixed.clone(), val.clone());
            }
            written.insert(prefixed, val);
        }
    }
    if let Err(e) = write_config(&path, &data) {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"ok": false, "error": e.to_string()}),
        );
    }
    log::debug(&format!(
        "update-sfx-defaults: wrote {} (layer={} fields={})",
        path.display(),
        py_repr(&layer),
        body_repr(&Value::Object(written.clone()))
    ));
    if let Err(e) = append_sfx_defaults_edit(&path, &written) {
        activity::log(&format!(
            "[WARN] sfx defaults edit journal write failed: {e}"
        ));
        log::warning(&format!(
            "sfx defaults edit journal write failed for {} (layer={}): {e}",
            path.display(),
            py_repr(&layer)
        ));
    }
    activity::log(&format!(
        "SFX defaults edit via timeline: {slug}/{tag} → {}",
        py_repr(&layer)
    ));
    log::info(&format!(
        "SFX defaults edit via timeline: {slug}/{tag} → {}",
        py_repr(&layer)
    ));
    reply(
        StatusCode::OK,
        json!({"ok": true, "message": format!("Saved {} defaults — re-run xil daw to apply.", py_repr(&layer))}),
    )
}

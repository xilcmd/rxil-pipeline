//! The timeline editor's `/xil/*` routes. Translated from `TestSfxRoutes`,
//! `TestSfxRouteJournaling` and `TestSfxDefaultsRoute` in the Python
//! `tests/test_xil_gui.py`.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};

const CFG: &str = "configs/myshow/sfx_S01E01.json";
const JOURNAL: &str = "configs/myshow/sfx_S01E01_edits.jsonl";

fn workspace() -> Workspace {
    let ws = Workspace::new();
    ws.write(
        CFG,
        &json!({
            "defaults": {"music_volume_percentage": 40, "volume_percentage": 70},
            "effects": {"MUSIC: THEME": {"volume_percentage": 80}},
        })
        .to_string(),
    );
    ws
}

fn get_uri(slug: &str, tag: &str, key: &str) -> String {
    format!(
        "/xil/get-sfx?slug={}&tag={}&key={}",
        enc(slug),
        enc(tag),
        enc(key)
    )
}

#[tokio::test]
async fn get_returns_effect_and_defaults() {
    let _ws = workspace();
    let (code, body) = get(state(), &get_uri("myshow", "S01E01", "MUSIC: THEME")).await;
    assert_eq!(code, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["effect"], json!({"volume_percentage": 80}));
    assert_eq!(v["defaults"]["music_volume_percentage"], 40);
    assert_eq!(v["natural_s"], Value::Null);
    // Key order is part of the contract the timeline page reads.
    assert!(body.starts_with("{\"effect\":"), "{body}");
}

#[tokio::test]
async fn get_unknown_key_returns_empty_effect() {
    let _ws = workspace();
    let (code, body) = get(state(), &get_uri("myshow", "S01E01", "SFX: NOPE")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["effect"],
        json!({})
    );
}

#[tokio::test]
async fn get_missing_config_404s() {
    let _ws = workspace();
    let (code, _) = get(state(), &get_uri("ghostshow", "S09E99", "X")).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_returns_natural_s_for_source_cue() {
    let ws = Workspace::new();
    ws.write(
        CFG,
        &json!({
            "defaults": {},
            "effects": {
                "OUTRO MUSIC": {"source": "SFX/outro.mp3", "duration_seconds": 1.0},
                "MUSIC: STING": {"prompt": "a sting", "duration_seconds": 15.0},
                "SFX: GONE": {"source": "SFX/absent.mp3", "duration_seconds": 5.0},
            },
        })
        .to_string(),
    );
    let outro = ws.root().join("SFX/outro.mp3");
    if silent_mp3(&outro, 3.0).is_none() {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let expected = xil_audio::mpeg::duration_ms(&outro).unwrap() as f64 / 1000.0;
    let natural = |key: &'static str| async move {
        let (code, body) = get(state(), &get_uri("myshow", "S01E01", key)).await;
        assert_eq!(code, StatusCode::OK);
        serde_json::from_str::<Value>(&body).unwrap()["natural_s"].clone()
    };
    assert_eq!(natural("OUTRO MUSIC").await, json!(expected));
    assert_eq!(natural("MUSIC: STING").await, Value::Null);
    assert_eq!(natural("SFX: GONE").await, Value::Null);
}

#[tokio::test]
async fn post_updates_and_clears_fields() {
    let ws = workspace();
    let (code, body) = post_json(
        state(),
        "/xil/update-sfx",
        json!({"slug": "myshow", "tag": "S01E01", "key": "MUSIC: THEME",
               "volume_percentage": null, "ramp_in_seconds": 2.5,
               "ramp_out_seconds": null, "play_duration": 66}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["message"],
        "Saved 'MUSIC: THEME' — re-run xil daw to apply."
    );
    let effect = &ws.json(CFG)["effects"]["MUSIC: THEME"];
    assert!(effect.get("volume_percentage").is_none());
    assert_eq!(effect["ramp_in_seconds"], 2.5);
    assert_eq!(effect["play_duration"], 66);
}

#[tokio::test]
async fn post_writes_python_json_layout() {
    let ws = workspace();
    post_json(
        state(),
        "/xil/update-sfx",
        json!({"slug": "myshow", "tag": "S01E01", "key": "SFX: CAFÉ", "volume_percentage": 55}),
    )
    .await;
    let text = ws.read(CFG);
    // json.dump(indent=2, ensure_ascii=False) + "\n"
    assert!(text.ends_with("}\n"));
    assert!(
        text.contains("\"SFX: CAFÉ\": {\n      \"volume_percentage\": 55\n    }"),
        "{text}"
    );
}

#[tokio::test]
async fn post_creates_effect_entry_when_absent() {
    let ws = workspace();
    let (code, _) = post_json(
        state(),
        "/xil/update-sfx",
        json!({"slug": "myshow", "tag": "S01E01", "key": "SFX: NEW DOOR", "volume_percentage": 55}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        ws.json(CFG)["effects"]["SFX: NEW DOOR"],
        json!({"volume_percentage": 55})
    );
}

#[tokio::test]
async fn post_missing_config_404s() {
    let _ws = workspace();
    let (code, _) = post_json(
        state(),
        "/xil/update-sfx",
        json!({"slug": "ghostshow", "tag": "S09E99", "key": "X", "volume_percentage": 10}),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_rejects_unsafe_slug_and_tag() {
    let _ws = workspace();
    for bad in [
        "../../etc",
        "..\\..\\windows",
        "foo/bar",
        "foo\\bar",
        "..",
        ".",
        "a/../../b",
        "show\0name",
    ] {
        let (code, _) = get(state(), &get_uri(bad, "S01E01", "X")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "slug {bad:?}");
    }
    for bad in ["../S01E01", "S01/../E01", "S01E01/../../secrets", "."] {
        let (code, _) = get(state(), &get_uri("myshow", bad, "X")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "tag {bad:?}");
    }
}

#[tokio::test]
async fn post_rejects_unsafe_slug_or_tag_without_touching_disk() {
    let ws = workspace();
    let before = ws.read(CFG);
    for (slug, tag) in [("../../etc", "S01E01"), ("myshow", "../../S01E01")] {
        let (code, _) = post_json(
            state(),
            "/xil/update-sfx",
            json!({"slug": slug, "tag": tag, "key": "X", "volume_percentage": 10}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }
    assert_eq!(before, ws.read(CFG));
    assert!(!ws.root().join(JOURNAL).exists());
}

#[tokio::test]
async fn legit_slug_and_tag_with_hyphen_underscore_still_work() {
    let ws = workspace();
    ws.write("configs/my-show_2/sfx_S01E01-alt.json", "{\"effects\": {}}");
    let (code, _) = get(state(), &get_uri("my-show_2", "S01E01-alt", "X")).await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test]
async fn successful_save_appends_one_journal_record() {
    let ws = workspace();
    let (code, _) = post_json(
        state(),
        "/xil/update-sfx",
        json!({"slug": "myshow", "tag": "S01E01", "key": "MUSIC: THEME",
               "volume_percentage": 33, "play_duration": 66}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let text = ws.read(JOURNAL);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let rec: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["key"], "MUSIC: THEME");
    assert_eq!(rec["fields"]["volume_percentage"], 33);
    assert_eq!(rec["fields"]["play_duration"], 66);
    assert_eq!(rec["fields"]["ramp_in_seconds"], Value::Null);
    assert_eq!(rec["fields"]["source"], Value::Null);
}

fn defaults_workspace() -> Workspace {
    let ws = Workspace::new();
    ws.write(
        CFG,
        &json!({
            "defaults": {"volume_percentage": 20, "music_volume_percentage": 80},
            "effects": {"MUSIC: THEME": {}},
        })
        .to_string(),
    );
    ws
}

async fn defaults(body: Value) -> (StatusCode, String) {
    post_json(state(), "/xil/update-sfx-defaults", body).await
}

#[tokio::test]
async fn defaults_sets_prefixed_key_only() {
    let ws = defaults_workspace();
    let (code, body) = defaults(
        json!({"slug": "myshow", "tag": "S01E01", "layer": "music", "volume_percentage": 42}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["message"],
        "Saved 'music' defaults — re-run xil daw to apply."
    );
    let d = &ws.json(CFG)["defaults"];
    assert_eq!(d["music_volume_percentage"], 42);
    assert_eq!(d["volume_percentage"], 20, "global fallback untouched");
}

#[tokio::test]
async fn defaults_null_pops_prefixed_key() {
    let ws = defaults_workspace();
    defaults(
        json!({"slug": "myshow", "tag": "S01E01", "layer": "music", "volume_percentage": null}),
    )
    .await;
    assert!(ws.json(CFG)["defaults"]
        .get("music_volume_percentage")
        .is_none());
}

#[tokio::test]
async fn defaults_other_layers_untouched() {
    let ws = defaults_workspace();
    defaults(
        json!({"slug": "myshow", "tag": "S01E01", "layer": "ambience", "volume_percentage": 15}),
    )
    .await;
    let d = &ws.json(CFG)["defaults"];
    assert_eq!(d["music_volume_percentage"], 80);
    assert_eq!(d["ambience_volume_percentage"], 15);
}

#[tokio::test]
async fn defaults_rejects_unsafe_slug_bad_layer_and_missing_config() {
    let ws = defaults_workspace();
    let (code, _) = defaults(
        json!({"slug": "../../etc", "tag": "S01E01", "layer": "music", "volume_percentage": 10}),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = defaults(
        json!({"slug": "myshow", "tag": "S01E01", "layer": "dialogue", "volume_percentage": 10}),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(ws.json(CFG)["defaults"]
        .get("dialogue_volume_percentage")
        .is_none());
    let (code, _) = defaults(
        json!({"slug": "ghostshow", "tag": "S09E99", "layer": "music", "volume_percentage": 10}),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(
        !ws.root().join(JOURNAL).exists(),
        "rejected requests journal nothing"
    );
}

#[tokio::test]
async fn defaults_journals_defaults_scope_record() {
    let ws = defaults_workspace();
    defaults(json!({"slug": "myshow", "tag": "S01E01", "layer": "music",
                    "volume_percentage": 42, "ramp_in_seconds": 1.5}))
    .await;
    let text = ws.read(JOURNAL);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let rec: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["scope"], "defaults");
    assert!(rec.get("key").is_none());
    assert_eq!(rec["fields"]["music_volume_percentage"], 42);
    assert_eq!(rec["fields"]["music_ramp_in_seconds"], 1.5);
    assert_eq!(rec["fields"]["music_ramp_out_seconds"], Value::Null);
}

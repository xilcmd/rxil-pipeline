//! The Dialogue sub-tab (dashboard) and the Timeline's dialogue-edit modal
//! (`/xil/get-parsed-entry` / `/xil/update-parsed-entry`) — both call
//! [`xil_web::entries::save_entry`], exercised here end to end through the
//! router.

mod common;
use common::*;

use axum::http::StatusCode;
use serde_json::json;

fn write_cast(ws: &Workspace, slug: &str, tag: &str) {
    ws.write(
        &format!("configs/{slug}/cast_{tag}.json"),
        &json!({"title": "", "season_title": "", "cast": {}}).to_string(),
    );
}

fn entry(seq: i64, kind: &str, speaker: &str, text: &str) -> serde_json::Value {
    json!({
        "seq": seq, "type": kind, "section": "act1", "scene": null,
        "speaker": if kind == "dialogue" { serde_json::Value::String(speaker.into()) } else { serde_json::Value::Null },
        "direction": null, "text": text,
        "direction_type": if kind == "direction" { serde_json::Value::String("SFX".into()) } else { serde_json::Value::Null },
        "sfx_source": null, "sfx_overrides": null,
    })
}

fn write_parsed(ws: &Workspace, entries: Vec<serde_json::Value>) {
    let data = json!({
        "show": "the413", "season": 1, "episode": 1, "title": "T",
        "season_title": null, "source_file": "s.md",
        "entries": entries,
        "stats": {},
    });
    ws.write(
        "parsed/the413/parsed_S01E01.json",
        &serde_json::to_string_pretty(&data).unwrap(),
    );
}

fn small_fixture() -> Vec<serde_json::Value> {
    vec![
        entry(1, "scene_header", "", "COLD OPEN"),
        entry(2, "dialogue", "adam", "Hello there."),
        entry(3, "direction", "", "DOOR"),
        entry(4, "dialogue", "ava", "Hi Adam."),
    ]
}

const EP: &str = "the413  S01E01";

#[tokio::test]
async fn dialogue_list_shows_only_dialogue_rows() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    write_parsed(&ws, small_fixture());

    let (code, body) = get(
        state(),
        &format!("/parsed/dialogue?ep={}", enc(EP)),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("Hello there."), "{body}");
    assert!(body.contains("Hi Adam."), "{body}");
    assert!(!body.contains("COLD OPEN"), "{body}");
    assert!(!body.contains("DOOR"), "{body}");
    assert!(body.contains("2 of 2"), "{body}");
}

#[tokio::test]
async fn dialogue_list_search_matches_speaker_and_text_case_insensitively() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    write_parsed(
        &ws,
        vec![
            entry(2, "dialogue", "adam", "Nice weather today."),
            entry(4, "dialogue", "ava", "Sure thing, boss."),
        ],
    );

    // "ADAM" matches only by speaker (case-insensitive) — neither line's
    // text mentions the other speaker's name.
    let (_, body) = get(state(), &format!("/parsed/dialogue?ep={}&q=ADAM", enc(EP))).await;
    assert!(body.contains("Nice weather today."), "{body}");
    assert!(!body.contains("Sure thing, boss."), "{body}");

    // "boss" matches only by text.
    let (_, body) = get(state(), &format!("/parsed/dialogue?ep={}&q=boss", enc(EP))).await;
    assert!(!body.contains("Nice weather today."), "{body}");
    assert!(body.contains("Sure thing, boss."), "{body}");
}

#[tokio::test]
async fn dialogue_list_paginates_at_twenty_five() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    let mut entries = Vec::new();
    for i in 0..30 {
        entries.push(entry(i * 2, "dialogue", "adam", &format!("Line {i}")));
    }
    write_parsed(&ws, entries);

    let (_, body) = get(state(), &format!("/parsed/dialogue?ep={}", enc(EP))).await;
    assert!(body.contains("1–25 of 30"), "{body}");
    assert!(body.contains("Line 0"), "{body}");
    assert!(!body.contains(">Line 25<"), "{body}");
    assert!(body.contains("Page 1 of 2"), "{body}");
    assert!(!body.contains("← Prev\">"), "prev must be disabled on page 1: {body}");

    let (_, body) = get(
        state(),
        &format!("/parsed/dialogue?ep={}&page=1", enc(EP)),
    )
    .await;
    assert!(body.contains("26–30 of 30"), "{body}");
    assert!(body.contains("Line 25"), "{body}");
    assert!(!body.contains("Line 0<"), "{body}");
}

#[tokio::test]
async fn entry_edit_panel_offers_a_speaker_dropdown_when_a_registry_exists() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    write_parsed(&ws, small_fixture());
    ws.write(
        "configs/the413/speakers.json",
        r#"[{"display": "ADAM", "key": "adam"}, {"display": "AVA", "key": "ava"}]"#,
    );

    let (code, body) = get(
        state(),
        &format!("/parsed/entry?ep={}&seq=2", enc(EP)),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("<select name=\"speaker\">"), "{body}");
    assert!(body.contains("value=\"adam\" selected"), "{body}");
    assert!(body.contains("value=\"ava\""), "{body}");
    assert!(body.contains("Hello there."), "{body}");
}

#[tokio::test]
async fn entry_edit_panel_falls_back_to_free_text_with_no_registry() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    write_parsed(&ws, small_fixture());

    let (_, body) = get(
        state(),
        &format!("/parsed/entry?ep={}&seq=2", enc(EP)),
    )
    .await;
    assert!(!body.contains("<select name=\"speaker\">"), "{body}");
    assert!(
        body.contains("<input name=\"speaker\" value=\"adam\">"),
        "{body}"
    );
}

#[tokio::test]
async fn entry_save_round_trips_and_journals_the_edit() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    let original_entries = small_fixture();
    write_parsed(&ws, original_entries.clone());
    let original = ws.read("parsed/the413/parsed_S01E01.json");

    let (_, body) = post_form(
        state(),
        "/parsed/entry",
        &[
            ("ep", EP),
            ("seq", "2"),
            ("speaker", "adam"),
            ("text", "Well — hi \u{201c}really\u{201d}"),
        ],
    )
    .await;
    assert!(body.contains("Saved seq 2"), "{body}");

    let on_disk = ws.read("parsed/the413/parsed_S01E01.json");
    assert!(on_disk.contains("Well — hi \u{201c}really\u{201d}"), "{on_disk}");
    assert!(!on_disk.contains("\\u2014"), "{on_disk}");
    assert!(on_disk.ends_with('}'), "{on_disk:?}");
    assert_eq!(ws.read("parsed/the413/parsed_S01E01.json.bak"), original);

    let data: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
    assert_eq!(data["stats"]["dialogue_lines"], 2);
    assert_eq!(data["stats"]["total_entries"], 4);

    let journal = ws.read("parsed/the413/parsed_S01E01_edits.jsonl");
    let line: serde_json::Value = serde_json::from_str(journal.trim()).unwrap();
    assert_eq!(line["seq"], 2);
    assert_eq!(line["fields"]["speaker"], "adam");
}

#[tokio::test]
async fn entry_save_rejects_unknown_seq_non_dialogue_and_bad_input() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    write_parsed(&ws, small_fixture());

    let (_, body) = post_form(
        state(),
        "/parsed/entry",
        &[("ep", EP), ("seq", "99"), ("speaker", "adam"), ("text", "hi")],
    )
    .await;
    assert!(body.contains("No entry with seq 99"), "{body}");

    let (_, body) = post_form(
        state(),
        "/parsed/entry",
        &[("ep", EP), ("seq", "3"), ("speaker", "adam"), ("text", "hi")],
    )
    .await;
    assert!(body.contains("not dialogue"), "{body}");

    let (_, body) = post_form(
        state(),
        "/parsed/entry",
        &[("ep", EP), ("seq", "2"), ("speaker", "adam"), ("text", "   ")],
    )
    .await;
    assert!(body.contains("cannot be empty"), "{body}");

    ws.write(
        "configs/the413/speakers.json",
        r#"[{"display": "ADAM", "key": "adam"}]"#,
    );
    let (_, body) = post_form(
        state(),
        "/parsed/entry",
        &[("ep", EP), ("seq", "2"), ("speaker", "nobody"), ("text", "hi")],
    )
    .await;
    assert!(body.contains("Unknown speaker"), "{body}");

    // None of the above rejections touched the file.
    let data: serde_json::Value = ws.json("parsed/the413/parsed_S01E01.json");
    assert_eq!(data["entries"][1]["text"], "Hello there.");
}

#[tokio::test]
async fn reprocess_banner_flags_lines_edited_after_their_last_produce() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    write_parsed(&ws, small_fixture());

    // No journal yet: no banner.
    let (_, body) = get(state(), &format!("/parsed/dialogue?ep={}", enc(EP))).await;
    assert!(!body.contains("Send to Produce"), "{body}");

    // seq 2 was produced at T, then edited at a later T' (journal after
    // manifest) — pending. seq 4 was produced after its only edit (manifest
    // after journal) — not pending.
    ws.write(
        "stems/the413/S01E01/S01E01_stem_manifest.json",
        &json!({"version": 1, "entries": [
            {"seq_at_generation": 2, "generated_at": "2026-01-01T00:00:00+00:00"},
            {"seq_at_generation": 4, "generated_at": "2026-01-03T00:00:00+00:00"},
        ]})
        .to_string(),
    );
    ws.write(
        "parsed/the413/parsed_S01E01_edits.jsonl",
        &format!(
            "{}\n{}\n",
            json!({"ts": "2026-01-02T00:00:00+00:00", "seq": 2, "fields": {"speaker": "adam", "text": "x"}}),
            json!({"ts": "2026-01-01T00:00:00+00:00", "seq": 4, "fields": {"speaker": "ava", "text": "y"}}),
        ),
    );

    let (_, body) = get(state(), &format!("/parsed/dialogue?ep={}", enc(EP))).await;
    assert!(body.contains("Send to Produce"), "{body}");
    assert!(body.contains("data-seqs=\"2\""), "{body}");
    assert!(!body.contains("data-seqs=\"2,4\""), "{body}");
    assert!(body.contains(&format!("data-ep=\"{}\"", EP)), "{body}");
}

#[tokio::test]
async fn timeline_json_routes_round_trip_by_slug_and_tag() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    write_parsed(&ws, small_fixture());

    let (code, body) = get(
        state(),
        "/xil/get-parsed-entry?slug=the413&tag=S01E01&seq=2",
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["speaker"], "adam");
    assert_eq!(v["text"], "Hello there.");

    let (code, body) = post_json(
        state(),
        "/xil/update-parsed-entry",
        json!({"slug": "the413", "tag": "S01E01", "seq": 2, "speaker": "adam", "text": "New line"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("\"ok\":true"), "{body}");
    assert!(body.contains("--seq-list 2 --force"), "{body}");

    let on_disk = ws.read("parsed/the413/parsed_S01E01.json");
    assert!(on_disk.contains("New line"), "{on_disk}");
}

#[tokio::test]
async fn timeline_json_routes_refuse_unsafe_slug_or_tag() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01");
    write_parsed(&ws, small_fixture());

    let (code, _) = get(
        state(),
        "/xil/get-parsed-entry?slug=../etc&tag=S01E01&seq=2",
    )
    .await;
    assert_ne!(code, StatusCode::OK);

    let (code, _) = post_json(
        state(),
        "/xil/update-parsed-entry",
        json!({"slug": "../etc", "tag": "S01E01", "seq": 2, "speaker": "adam", "text": "x"}),
    )
    .await;
    assert_ne!(code, StatusCode::OK);

    // Unchanged.
    let data: serde_json::Value = ws.json("parsed/the413/parsed_S01E01.json");
    assert_eq!(data["entries"][1]["text"], "Hello there.");
}

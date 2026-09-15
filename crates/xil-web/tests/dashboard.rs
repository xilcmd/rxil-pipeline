//! Dashboard helpers and pages. Translated from the Python
//! `tests/test_xil_gui.py` (header analysis, script save, episode labels and
//! table cache, SFX grading and its cache file, the audio cache, config
//! loader guards, the timeline iframe), plus tests of the htmx pages and the
//! stage-runner stream that replace Gradio's callbacks.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use axum::http::StatusCode;
use common::*;
use filetime::{set_file_mtime, FileTime};
use serde_json::json;
use xil_web::{audio, configs, episodes, grades, scripts, AppState, StageCells};

fn write_cast(ws: &Workspace, slug: &str, tag: &str, title: &str, season_title: &str) {
    ws.write(
        &format!("configs/{slug}/cast_{tag}.json"),
        &json!({"title": title, "season_title": season_title, "cast": {}}).to_string(),
    );
}

// ── _analyze_script_header ───────────────────────────────────────────────

#[test]
fn analyze_full_header_all_fields() {
    let h = scripts::analyze_script_header(
        "THE 413 Season 3: Episode 3: \"The Covered Bridge\" Arc: \"The Architect\"\n\nCAST:",
    );
    assert_eq!(h.show, "THE 413");
    assert_eq!(h.season, "3");
    assert_eq!(h.episode, "3");
    assert_eq!(h.title, "The Covered Bridge");
    assert_eq!(h.arc, "The Architect");
    assert_eq!(h.filename, "S03E03_the413_The_Covered_Bridge_v1.md");
}

#[test]
fn analyze_episode_zero_teaser_and_sanitised_title() {
    let h = scripts::analyze_script_header(
        "THE 413 Season 6: Episode 0: \"Teaser — The Berkshire Ghost Train\" Arc: \"The Berkshire Ghost Train\"",
    );
    assert_eq!(h.episode, "0");
    assert_eq!(
        h.filename,
        "S06E00_the413_Teaser_The_Berkshire_Ghost_Train_v1.md"
    );
}

#[test]
fn analyze_no_arc_blank_lines_and_unrecognised() {
    let h = scripts::analyze_script_header("\n\n  \nTHE 413 Season 1: Episode 2: \"The Return\"\n");
    assert_eq!(
        (h.show.as_str(), h.arc.as_str(), h.title.as_str()),
        ("THE 413", "", "The Return")
    );
    assert!(h.filename.starts_with("S01E02_"));
    let bad = scripts::analyze_script_header("no episode marker here");
    assert_eq!(
        (bad.show.as_str(), bad.season.as_str(), bad.episode.as_str()),
        ("", "", "")
    );
    assert!(bad.filename.starts_with("⚠️"));
    assert_eq!(
        scripts::analyze_script_header(""),
        scripts::HeaderFields::default()
    );
}

// ── _save_script_file ────────────────────────────────────────────────────

#[test]
fn save_script_file_rules() {
    let ws = Workspace::new();
    assert_eq!(
        scripts::save_script_file("script content", "S01E01_test_v1.md"),
        "✅ Saved: scripts/S01E01_test_v1.md"
    );
    assert_eq!(ws.read("scripts/S01E01_test_v1.md"), "script content");
    let again = scripts::save_script_file("second version", "S01E01_test_v1.md");
    assert!(again.starts_with("⚠️ Already exists"), "{again}");
    assert_eq!(ws.read("scripts/S01E01_test_v1.md"), "script content");
    assert_eq!(
        scripts::save_script_file("content", "S01E01_no_ext"),
        "✅ Saved: scripts/S01E01_no_ext.md"
    );
    assert!(scripts::save_script_file("   ", "S01E01_x_v1.md").starts_with("⚠️ No script content"));
    assert!(scripts::save_script_file("content", "   ").starts_with("⚠️ Filename is empty"));
    assert!(scripts::save_script_file("content", "../escape.md").starts_with("⚠️ Invalid filename"));
}

#[test]
fn save_script_file_uses_existing_show_directory() {
    let ws = Workspace::new();
    std::fs::create_dir_all(ws.root().join("scripts/the413")).unwrap();
    assert_eq!(
        scripts::save_script_file("x", "S01E01_the413_Title_v1.md"),
        "✅ Saved: scripts/the413/S01E01_the413_Title_v1.md"
    );
}

// ── _ep_meta / _ep_choice / _find_episodes ───────────────────────────────

#[test]
fn episode_labels() {
    let ws = Workspace::new();
    write_cast(
        &ws,
        "the413",
        "S03E03",
        "The Covered Bridge",
        "The Architect",
    );
    assert_eq!(
        episodes::ep_meta("the413", "S03E03"),
        ("The Covered Bridge".into(), "The Architect".into())
    );
    assert_eq!(
        episodes::ep_choice("the413", "S03E03"),
        "the413  S03E03  [The Architect]  —  The Covered Bridge"
    );
    write_cast(&ws, "the413", "S03E04", "", "The Architect");
    assert_eq!(
        episodes::ep_choice("the413", "S03E04"),
        "the413  S03E04  [The Architect]"
    );
    write_cast(&ws, "the413", "S03E05", "Only Title", "");
    assert_eq!(
        episodes::ep_choice("the413", "S03E05"),
        "the413  S03E05  —  Only Title"
    );
    assert_eq!(episodes::ep_choice("the413", "S99E99"), "the413  S99E99");
    ws.write("configs/the413/cast_S01E01.json", "not json {{{");
    assert_eq!(
        episodes::ep_meta("the413", "S01E01"),
        (String::new(), String::new())
    );
}

#[test]
fn find_episodes_both_layouts_and_show_stubs() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S02E01", "", "");
    write_cast(&ws, "the413", "S01E01", "", "");
    ws.write("cast_legacy_S01E01.json", "{}");
    ws.write("configs/newshow/project.json", "{\"show\": \"New Show\"}");
    assert_eq!(
        episodes::find_episodes(),
        vec![
            ("legacy".to_string(), "S01E01".to_string()),
            ("newshow".into(), "".into()),
            ("the413".into(), "S01E01".into()),
            ("the413".into(), "S02E01".into()),
        ]
    );
    assert_eq!(
        episodes::ep_choice("newshow", ""),
        "newshow  [show]  —  New Show"
    );
}

// ── _refresh_episodes ────────────────────────────────────────────────────

fn counting_state(calls: Arc<AtomicUsize>) -> Arc<AppState> {
    AppState::new(
        "/bin/echo".into(),
        Arc::new(move |s, t| {
            calls.fetch_add(1, Ordering::SeqCst);
            fixed_status(s, t)
        }),
    )
}

#[test]
fn refresh_episodes_rows_sorted_and_cached() {
    let ws = Workspace::new();
    for t in ["S03E01", "S01E01", "S02E01"] {
        write_cast(&ws, "the413", t, "", "");
    }
    write_cast(
        &ws,
        "the413",
        "S01E01",
        "The Empty Booth",
        "The Holiday Shift",
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let state = counting_state(calls.clone());
    let rows = episodes::refresh_episodes(&state, false);
    let tags: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(tags, ["S01E01", "S02E01", "S03E01"]);
    assert_eq!(rows[0][2], "[The Holiday Shift]  —  The Empty Booth");
    assert_eq!(rows[0][3..], ["✓", "✓ 3", "○", "○", "○ missing"]);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    assert_eq!(episodes::refresh_episodes(&state, false), rows);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "second call within the TTL rescans nothing"
    );

    write_cast(&ws, "the413", "S04E01", "", "");
    assert_eq!(
        episodes::refresh_episodes(&state, true).len(),
        4,
        "force rescans"
    );
    assert_eq!(
        episodes::refresh_episodes(&state, false).len(),
        4,
        "the forced rows are cached"
    );
}

// ── config loaders ───────────────────────────────────────────────────────

#[test]
fn config_loaders_refuse_paths_outside_the_workspace() {
    let ws = Workspace::new();
    let outside = ws.dir.path().join("outside.json");
    std::fs::write(&outside, "{\"secret\": true}").unwrap();
    for label in ["cast", "speakers", "sfx"] {
        let out = configs::load_config_file(&outside.to_string_lossy(), label);
        assert!(!out.contains("secret"));
        assert!(out.contains("outside the workspace"), "{out}");
    }
    let inside = ws.write("cast_S01E01.json", "{\"cast\": {}}");
    assert_eq!(
        configs::load_config_file(&inside.to_string_lossy(), "cast"),
        "{\"cast\": {}}"
    );
    let dotdot = ws.root().join("configs/../../outside.json");
    assert!(configs::load_config_file(&dotdot.to_string_lossy(), "cast")
        .contains("outside the workspace"));
}

#[test]
fn config_save_pretty_prints_and_refuses_bad_json() {
    let ws = Workspace::new();
    let p = ws.write("configs/s/cast_S01E01.json", "{}");
    let path = p.to_string_lossy().into_owned();
    assert!(configs::save_config_file(&path, "{nope", "cast config")
        .starts_with("Invalid JSON — not saved"));
    assert_eq!(
        configs::save_config_file(&path, "{\"a\":1,\"b\":\"é\"}", "cast config"),
        format!("Saved {path}")
    );
    assert_eq!(
        ws.read("configs/s/cast_S01E01.json"),
        "{\n  \"a\": 1,\n  \"b\": \"\\u00e9\"\n}\n"
    );
    assert_eq!(
        configs::save_config_file("", "{}", "cast config"),
        "No file selected."
    );
}

// ── grades ───────────────────────────────────────────────────────────────

/// A real (silent) MP3 so ID3 writes have audio frames to preserve.
fn make_mp3(path: &Path) -> bool {
    silent_mp3(path, 0.2).is_some()
}

#[test]
fn grade_scan_labels_filters_and_summary() {
    let ws = Workspace::new();
    let sfx = ws.root().join("SFX");
    for rel in ["one.mp3", "two.mp3", "showa/beat.mp3", "showb/beat.mp3"] {
        if !make_mp3(&sfx.join(rel)) {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
    }
    xil_audio::tags::write_sfx_grade(&sfx.join("one.mp3"), "accurate").unwrap();
    let state = state();
    grades::scan_sfx_grades(&state);
    let all = grades::sfx_choices(&state, "all");
    let labels: Vec<&str> = all.iter().map(|c| c.0.as_str()).collect();
    assert_eq!(
        labels,
        [
            "✓  one.mp3",
            "•  [showa] beat.mp3",
            "•  [showb] beat.mp3",
            "•  two.mp3"
        ]
    );
    assert_eq!(grades::sfx_choices(&state, "accurate").len(), 1);
    assert_eq!(grades::sfx_choices(&state, "ungraded").len(), 3);
    assert_eq!(grades::sfx_choices(&state, "rejected").len(), 0);
    assert_eq!(
        grades::sfx_summary(&state),
        "4 files — 1 ✓ accurate · 0 ✗ rejected · 3 • ungraded"
    );

    let cache: serde_json::Value =
        serde_json::from_str(&ws.read("SFX/.xil_grade_cache.json")).unwrap();
    assert_eq!(cache["version"], 1);
    assert_eq!(cache["files"]["one.mp3"]["grade"], "accurate");
    assert_eq!(cache["files"]["showa/beat.mp3"]["grade"], "");
}

#[test]
fn grade_cache_file_is_trusted_while_size_and_mtime_match() {
    let ws = Workspace::new();
    let one = ws.root().join("SFX/one.mp3");
    if !make_mp3(&one) {
        return;
    }
    let state = state();
    grades::scan_sfx_grades(&state);
    // Forge the cached grade: a scan that trusts the cache reports it without
    // reading the tag; one that re-reads would say "".
    let mut files = grades::load_grade_cache_file();
    files["one.mp3"]["grade"] = json!("rejected");
    grades::save_grade_cache_file(&files);
    grades::scan_sfx_grades(&state);
    assert_eq!(grades::grade_of(&state, &one.to_string_lossy()), "rejected");

    // A changed mtime forces the one file to be read again.
    set_file_mtime(&one, FileTime::from_unix_time(12345, 0)).unwrap();
    grades::scan_sfx_grades(&state);
    assert_eq!(grades::grade_of(&state, &one.to_string_lossy()), "");

    // Corrupt cache: full rescan, and a valid file written back.
    ws.write("SFX/.xil_grade_cache.json", "{not json");
    grades::scan_sfx_grades(&state);
    assert_eq!(grades::load_grade_cache_file()["one.mp3"]["grade"], "");

    // A deleted file leaves both the map and the file.
    std::fs::remove_file(&one).unwrap();
    grades::scan_sfx_grades(&state);
    assert!(grades::sfx_choices(&state, "all").is_empty());
    assert!(grades::load_grade_cache_file().get("one.mp3").is_none());
}

#[test]
fn applying_a_grade_updates_tag_map_and_cache_without_forcing_a_reread() {
    let ws = Workspace::new();
    let one = ws.root().join("SFX/one.mp3");
    if !make_mp3(&one) {
        return;
    }
    let state = state();
    grades::scan_sfx_grades(&state);
    let path = one.to_string_lossy().into_owned();
    grades::apply_grade(&state, &path, "rejected").unwrap();
    assert_eq!(xil_audio::tags::read_sfx_grade(&one), "rejected");
    assert_eq!(grades::grade_of(&state, &path), "rejected");
    let rec = &grades::load_grade_cache_file()["one.mp3"];
    let mtime_ns = std::fs::metadata(&one)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    assert_eq!(rec["mtime_ns"], json!(mtime_ns as i64));
    assert!(grades::apply_grade(&state, "/not/scanned.mp3", "accurate").is_err());
    grades::update_grade_cache_entry(&ws.root().join("SFX/gone.mp3"), "accurate");
}

// ── audio cache ──────────────────────────────────────────────────────────

#[test]
fn cached_audio_path_copies_hits_and_rolls_over() {
    let ws = Workspace::new();
    let src = ws.write("a.mp3", &"x".repeat(1000));
    let first = audio::cached_audio_path(&src);
    assert_ne!(first, src);
    assert!(first.starts_with(ws.dir.path().join("xdg-cache")));
    assert!(first.to_string_lossy().ends_with(".mp3"));
    assert_eq!(std::fs::read(&first).unwrap(), vec![b'x'; 1000]);
    assert_eq!(audio::cached_audio_path(&src), first);
    set_file_mtime(&src, FileTime::from_unix_time(11111, 0)).unwrap();
    assert_ne!(audio::cached_audio_path(&src), first);
    let missing = ws.root().join("nope.mp3");
    assert_eq!(audio::cached_audio_path(&missing), missing);
    assert!(audio::cache_url(&first).unwrap().starts_with("/cache/"));
}

#[test]
fn eviction_deletes_oldest_beyond_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let big = (audio::AUDIO_CACHE_MAX_BYTES / 2 + 1) as usize;
    let files: Vec<_> = (0..3)
        .map(|i| {
            let p = dir.join(format!("f{i}.mp3"));
            let f = std::fs::File::create(&p).unwrap();
            f.set_len(big as u64).unwrap();
            set_file_mtime(&p, FileTime::from_unix_time(1000 + i, 0)).unwrap();
            p
        })
        .collect();
    audio::evict_audio_cache(dir, &files[2]);
    assert!(!files[0].exists(), "oldest evicted");
    assert!(!files[1].exists());
    assert!(files[2].exists(), "the kept file survives");
}

#[test]
fn concatenated_preview_is_cached_by_input_signature() {
    let ws = Workspace::new();
    let stems = ws.root().join("stems/the413/S01E01");
    for n in ["001_intro_host.mp3", "002_beat_fx.mp3"] {
        if !make_mp3(&stems.join(n)) {
            return;
        }
    }
    let first = audio::concatenate_stems("the413", "S01E01", "all").expect("concat");
    assert!(first.starts_with(ws.dir.path().join("xdg-cache")));
    assert!(xil_audio::mpeg::duration_ms(&first).unwrap() > 0);
    assert_eq!(
        audio::concatenate_stems("the413", "S01E01", "all").unwrap(),
        first
    );
    set_file_mtime(
        stems.join("001_intro_host.mp3"),
        FileTime::from_unix_time(22222, 0),
    )
    .unwrap();
    assert_ne!(
        audio::concatenate_stems("the413", "S01E01", "all").unwrap(),
        first
    );
    assert!(audio::concatenate_stems("the413", "S01E01", "music").is_none());
}

#[test]
fn stem_labels_come_from_the_parsed_script() {
    let ws = Workspace::new();
    ws.write(
        "parsed/the413/parsed_S01E01.json",
        &json!({"entries": [
            {"seq": 1, "type": "dialogue", "speaker": "host", "section": "intro", "text": "Hello and welcome to a very long line of dialogue that runs on"},
            {"seq": 2, "type": "direction", "direction_type": "SFX", "section": "intro", "text": "DOOR"},
        ]})
        .to_string(),
    );
    ws.write("stems/the413/S01E01/001_intro_host.mp3", "");
    ws.write("stems/the413/S01E01/002_intro_sfx.mp3", "");
    ws.write("stems/the413/S01E01/extra.mp3", "");
    let all = episodes::load_stems("the413", "S01E01", "all");
    assert_eq!(
        all[0].0,
        "   1  host          intro           Hello and welcome to a very long line of dialogue th"
    );
    assert_eq!(all[1].0, "   2  SFX           intro           DOOR");
    assert_eq!(all[2].0, "extra");
    assert_eq!(
        episodes::load_stems("the413", "S01E01", "dialogue").len(),
        1
    );
    assert_eq!(episodes::load_stems("the413", "S01E01", "sfx").len(), 1);
    assert_eq!(episodes::load_stems("the413", "S01E01", "music").len(), 0);
}

// ── pages ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn index_page_has_every_tab_and_escapes_labels() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01", "<script>alert(1)</script>", "");
    let (code, body) = get(state(), "/").await;
    assert_eq!(code, StatusCode::OK);
    for tab in [
        "Setup",
        "Project",
        "Episodes",
        "Run Stage",
        "Speakers",
        "Cast Config",
        "SFX Config",
        "Audio Preview",
        "Audio Grading",
        "Timeline",
    ] {
        assert!(body.contains(&format!(">{tab}</button>")), "{tab}");
    }
    assert!(!body.contains("<script>alert(1)</script>"));
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    let (code, js) = get(state(), "/assets/htmx.min.js").await;
    assert_eq!(code, StatusCode::OK);
    assert!(js.starts_with("var htmx="));
}

#[tokio::test]
async fn timeline_tab_embeds_a_cache_busted_workspace_url() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01", "", "");
    let (_, missing) = get(state(), "/timeline?ep=the413%20%20S01E01").await;
    assert!(missing.contains("xil daw --episode S01E01 --timeline-html"));
    let html = ws.write("daw/the413/S01E01/S01E01_timeline.html", "<html></html>");
    set_file_mtime(&html, FileTime::from_unix_time(1_780_000_000, 0)).unwrap();
    let (_, body) = get(state(), "/timeline?ep=the413%20%20S01E01").await;
    assert!(
        body.contains("src=\"/ws/daw/the413/S01E01/S01E01_timeline.html?v=1780000000\""),
        "{body}"
    );
    let (code, served) = get(state(), "/ws/daw/the413/S01E01/S01E01_timeline.html?v=1").await;
    assert_eq!((code, served.as_str()), (StatusCode::OK, "<html></html>"));
    let (code, _) = get(state(), "/ws/../outside.json").await;
    assert_ne!(code, StatusCode::OK);
}

#[tokio::test]
async fn run_stage_validates_then_streams_the_process() {
    let ws = Workspace::new();
    write_cast(&ws, "the413", "S01E01", "", "");
    let (_, body) = post_form(state(), "/run/produce", &[]).await;
    assert!(body.contains("Select an episode first."));
    let (_, body) = post_form(
        state(),
        "/run/produce",
        &[("ep", "the413  [show]  —  The 413")],
    )
    .await;
    assert!(body.contains("No episode tag"));
    let (_, body) = post_form(state(), "/run/scan", &[("ep", "the413  S01E01")]).await;
    assert!(body.contains("Scan requires a script"));

    let st = state();
    let (_, body) = post_form(
        st.clone(),
        "/run/daw",
        &[
            ("ep", "the413  S01E01"),
            ("dry_run", "on"),
            ("gap_ms", "400"),
            ("timeline_html", "on"),
        ],
    )
    .await;
    assert!(body.contains("data-job=\"1\""), "{body}");
    assert!(
        body.contains("$ /bin/echo daw --episode S01E01 --dry-run --gap-ms 400 --timeline-html")
    );
    if cfg!(windows) {
        // The stand-in executable is /bin/echo; Windows has none to stream.
        return;
    }
    let (code, stream) =
        tokio::time::timeout(Duration::from_secs(10), get(st.clone(), "/jobs/1/stream"))
            .await
            .expect("stream ends when the process exits");
    assert_eq!(code, StatusCode::OK);
    assert!(
        stream.contains(
            "event: line\ndata: daw --episode S01E01 --dry-run --gap-ms 400 --timeline-html"
        ),
        "{stream}"
    );
    assert!(stream.contains("event: exit\ndata: [exit 0]"), "{stream}");
    let (code, _) = get(st, "/jobs/1/stream").await;
    assert_eq!(code, StatusCode::NOT_FOUND, "a job is followed once");
}

#[tokio::test]
async fn parse_accepts_a_typed_tag_for_the_active_show() {
    let ws = Workspace::new();
    ws.write(".active_show", "night");
    ws.write("scripts/night/S01E01_night_Pilot_v1.md", "x");
    let st = state();
    let (_, body) = post_form(st, "/run/parse", &[("ep", "S01E01"), ("debug", "on")]).await;
    let script = ws.root().join("scripts/night/S01E01_night_Pilot_v1.md");
    assert!(
        body.contains(&format!(
            "$ /bin/echo parse {} --episode S01E01 --debug",
            script.display()
        )),
        "{body}"
    );
}

#[tokio::test]
async fn grading_pages_round_trip() {
    let ws = Workspace::new();
    let one = ws.root().join("SFX/one.mp3");
    if !make_mp3(&one) {
        return;
    }
    let st = state();
    let (_, panel) = get(st.clone(), "/grades/list?refresh=1&filter=all").await;
    assert!(
        panel.contains("1 files — 0 ✓ accurate · 0 ✗ rejected · 1 • ungraded"),
        "{panel}"
    );
    let path = one.to_string_lossy().into_owned();
    let (_, panel) = post_form(
        st.clone(),
        "/grades/apply",
        &[("path", &path), ("grade", "accurate"), ("filter", "all")],
    )
    .await;
    assert!(panel.contains("1 files — 1 ✓ accurate"), "{panel}");
    assert_eq!(xil_audio::tags::read_sfx_grade(&one), "accurate");
    let (_, player) = get(st.clone(), &format!("/grades/select?path={}", enc(&path))).await;
    assert!(
        player.contains("<audio") && player.contains("✓ accurate"),
        "{player}"
    );
    let (_, nothing) = get(st, "/grades/select?path=%2Fetc%2Fpasswd").await;
    assert_eq!(nothing, "", "only scanned files are served");
}

#[tokio::test]
async fn project_tab_follows_the_active_show() {
    let ws = Workspace::new();
    ws.write("configs/night/project.json", "{\"show\": \"Night\"}");
    let (_, panel) = get(state(), "/project").await;
    assert!(panel.contains(
        &ws.root()
            .join("project.json")
            .to_string_lossy()
            .into_owned()
    ));
    ws.write(".active_show", "night");
    let (_, panel) = get(state(), "/project").await;
    assert!(panel.contains(
        &Path::new("configs")
            .join("night")
            .join("project.json")
            .to_string_lossy()
            .into_owned()
    ));
    let (_, saved) = post_form(
        state(),
        "/project/save",
        &[("text", "{\"show\":\"Night\",\"season\":2}")],
    )
    .await;
    assert!(saved.contains("Saved"));
    assert_eq!(
        ws.read("configs/night/project.json"),
        "{\n  \"show\": \"Night\",\n  \"season\": 2\n}\n"
    );
}

#[test]
fn stage_cells_default_is_blank() {
    assert_eq!(StageCells::default().overall, "");
}

//! The dashboard page and the htmx fragments behind each tab.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Form, Path as UrlPath, Query, State};
use axum::http::{header, HeaderName, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;
use xil_core::workspace::{active_show, resolve_slug, resolve_venv_python, workspace_root};

use crate::html::{checkbox, esc, labelled_options, number_input, options, status, text_input};
use crate::runner::{self, DawOpts, ParseOpts, ProduceOpts};
use crate::{activity, audio, configs, episodes, grades, scripts, AppState, JobEvent};

type Params = HashMap<String, String>;
type Shared = State<Arc<AppState>>;

const INDEX: &str = include_str!("../assets/index.html");

fn param<'a>(p: &'a Params, key: &str) -> &'a str {
    p.get(key).map(String::as_str).unwrap_or("")
}

fn on(p: &Params, key: &str) -> bool {
    p.contains_key(key)
}

fn int(p: &Params, key: &str, default: i64) -> i64 {
    p.get(key)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|f| f as i64)
        .unwrap_or(default)
}

/// Run blocking filesystem work off the async executor.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("blocking task panicked")
}

fn log_pre(text: &str) -> Html<String> {
    Html(format!("<pre class=\"log\">{}</pre>", esc(text)))
}

/// A fragment plus an `HX-Trigger` event for other elements to react to.
fn with_trigger(body: String, event: &'static str) -> Response {
    ([(HeaderName::from_static("hx-trigger"), event)], Html(body)).into_response()
}

// ── page and assets ──────────────────────────────────────────────────────

fn default_python(venv: &str) -> String {
    resolve_venv_python(venv, None, None).unwrap_or_default()
}

fn config_tab(kind: &str, title: &str, choices: &[String]) -> String {
    let first = choices.first().cloned();
    let content = first
        .as_deref()
        .map(|p| configs::load_config_file(p, kind_label(kind)))
        .unwrap_or_default();
    format!(
        r##"<section class="tab" data-group="main" data-tab="{kind}">
  <form id="{kind}-form">
    <label class="field"><span>{title}</span>
      <select name="path" hx-get="/config/{kind}/load" hx-trigger="change" hx-target="#{kind}-editor" hx-swap="outerHTML">{opts}</select>
    </label>
    <textarea id="{kind}-editor" name="text" rows="30">{content}</textarea>
    <div class="row">
      <button type="button" class="small" hx-get="/config/{kind}/load" hx-include="#{kind}-form" hx-target="#{kind}-editor" hx-swap="outerHTML">↺ Reload</button>
      <button type="button" class="small primary" hx-post="/config/{kind}/save" hx-include="#{kind}-form" hx-target="#{kind}-status">💾 Save</button>
    </div>
    <div id="{kind}-status"></div>
  </form>
</section>"##,
        opts = options(choices, first.as_deref()),
        content = esc(&content),
    )
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "speakers" => "speakers",
        "cast" => "cast",
        _ => "sfx",
    }
}

fn is_config_kind(kind: &str) -> bool {
    matches!(kind, "speakers" | "cast" | "sfx")
}

fn config_choice_list(state: &AppState, kind: &str) -> Option<Vec<String>> {
    let c = state.choices();
    match kind {
        "speakers" => Some(c.speakers.clone()),
        "cast" => Some(c.cast.clone()),
        "sfx" => Some(c.sfx.clone()),
        _ => None,
    }
}

fn produce_form() -> String {
    let cb_default = default_python("venv-chatterbox");
    let mm_default = default_python("venv-mmaudio");
    format!(
        r##"<div class="row">{dry}
  <label class="field"><span>--backend  (dialogue voice generator)</span>
    <select name="backend" id="prod-backend"><option>elevenlabs</option><option>gtts</option><option selected>chatterbox-turbo</option></select>
  </label></div>
<div class="row">{sfx}{music}{amb}{local}{terse}</div>
<label class="field"><span>--sfx-backend  (SFX / music / ambience generator)</span>
  <select name="sfx_backend"><option selected>elevenlabs</option><option>mmaudio</option></select>
</label>
{mm_python}
{mm_nc}
<div class="row">{start}{stop}</div>
{cb_python}
<div class="row">{force}</div>"##,
        dry = checkbox("dry_run", "--dry-run", true),
        sfx = checkbox("gen_sfx", "--gen-sfx", false),
        music = checkbox("gen_music", "--gen-music", false),
        amb = checkbox("gen_ambience", "--gen-ambience", false),
        local = checkbox("local_only", "--local-only", true),
        terse = checkbox("terse", "--terse", false),
        mm_python = text_input("mmaudio_python", "--mmaudio-python  (blank = auto-detect venv-mmaudio/)", &mm_default),
        mm_nc = checkbox(
            "mmaudio_accept_nc",
            "--mmaudio-accept-noncommercial  ⚠️ MMAudio weights are CC BY-NC 4.0 — generated audio must NOT be used commercially",
            false
        ),
        start = number_input("start_from", "--start-from  (seq, 0 = beginning)", 0),
        stop = number_input("stop_at", "--stop-at  (seq, 0 = all)", 0),
        cb_python = text_input("chatterbox_python", "--chatterbox-python  (blank = auto-detect venv-chatterbox/)", &cb_default),
        force = checkbox("force", "--force  ⚠️ overwrite existing stems (API cost!)", false),
    )
}

fn script_meta(h: &scripts::HeaderFields) -> String {
    let ro = |label: &str, v: &str| {
        format!(
            "<label class=\"field\"><span>{}</span><input readonly value=\"{}\"></label>",
            esc(label),
            esc(v)
        )
    };
    format!(
        "<p><b>Derived metadata</b></p><div class=\"row\">{}{}{}</div><div class=\"row\">{}{}</div>\
         <label class=\"field\"><span>Filename (editable)</span><input name=\"filename\" value=\"{}\"></label>",
        ro("Show", &h.show),
        ro("Season", &h.season),
        ro("Episode", &h.episode),
        ro("Title", &h.title),
        ro("Arc / Season Title", &h.arc),
        esc(&h.filename)
    )
}

/// `GET /`: the whole dashboard.
pub async fn index(State(state): Shared) -> Html<String> {
    Html(
        blocking(move || {
            let workspace = workspace_root().to_string_lossy().into_owned();
            let c = state.choices();
            let (episodes, shows, scripts) = (&c.episodes, &c.shows, &c.scripts);
            let active = configs::active_show_name(shows);
            let configs_html = [
                config_tab("speakers", "Speakers file", &c.speakers),
                config_tab("cast", "Cast config file", &c.cast),
                config_tab("sfx", "SFX config file", &c.sfx),
            ]
            .join("\n");
            let speakers_placeholder = "configs/the413/speakers.json";
            INDEX
                .replace("@@WORKSPACE@@", &esc(&workspace))
                .replace("@@SHOW_OPTIONS@@", &options(shows, active.as_deref()))
                .replace("@@EPISODE_OPTIONS@@", &options(episodes, None))
                .replace(
                    "@@AUDIO_EPISODE_OPTIONS@@",
                    &options(episodes, episodes.first().map(String::as_str)),
                )
                .replace("@@SCRIPT_OPTIONS@@", &options(scripts, None))
                .replace(
                    "@@SCRIPT_META@@",
                    &script_meta(&scripts::HeaderFields::default()),
                )
                .replace(
                    "@@SCAN_SPEAKERS@@",
                    &text_input(
                        "speakers",
                        "--speakers (optional override)",
                        speakers_placeholder,
                    ),
                )
                .replace(
                    "@@SCAN_JSON@@",
                    &checkbox("json", "--json  (machine-readable output)", false),
                )
                .replace(
                    "@@PARSE_SPEAKERS@@",
                    &text_input(
                        "speakers",
                        "--speakers (optional override)",
                        speakers_placeholder,
                    ),
                )
                .replace(
                    "@@PARSE_OPTIONS@@",
                    &[
                        number_input("preview", "--preview  (show first N entries, 0 = all)", 0),
                        checkbox("quiet", "--quiet  (JSON only, skip summary)", false),
                        checkbox("debug", "--debug  (write diagnostic CSV)", true),
                        checkbox(
                            "stats",
                            "--stats  (per-speaker line/word/char distribution)",
                            false,
                        ),
                    ]
                    .concat(),
                )
                .replace("@@PRODUCE_FORM@@", &produce_form())
                .replace(
                    "@@ASSEMBLE_FORM@@",
                    &[
                        "<div class=\"row\">".to_string(),
                        number_input("gap_ms", "--gap-ms  (silence between stems, ms)", 600),
                        "</div><div class=\"row\">".into(),
                        text_input(
                            "parsed",
                            "--parsed  (override parsed JSON path, blank = auto)",
                            "parsed/the413/parsed_S01E01.json",
                        ),
                        text_input(
                            "output",
                            "--output  (override master MP3 path, blank = auto)",
                            "masters/S01E01_the413_master.mp3",
                        ),
                        "</div>".into(),
                    ]
                    .concat(),
                )
                .replace(
                    "@@DAW_FORM@@",
                    &[
                        "<div class=\"row\">".to_string(),
                        checkbox("dry_run", "--dry-run", true),
                        number_input("gap_ms", "--gap-ms  (ms)", 600),
                        "</div><div class=\"row\">".into(),
                        checkbox("timeline", "--timeline  (ASCII)", false),
                        checkbox("timeline_html", "--timeline-html", true),
                        checkbox("macro", "--macro  (Audacity)", true),
                        "</div>".into(),
                        text_input("output_dir", "--output-dir  (blank = auto)", "daw/S01E01/"),
                    ]
                    .concat(),
                )
                .replace(
                    "@@MASTER_FORM@@",
                    &[
                        checkbox("dry_run", "--dry-run", true),
                        "<div class=\"row\">".into(),
                        text_input(
                            "output",
                            "--output  (blank = auto)",
                            "masters/S01E01_the413_2026-04-26.mp3",
                        ),
                        text_input("daw_dir", "--daw-dir  (blank = auto)", "daw/S01E01/"),
                        "</div>".into(),
                    ]
                    .concat(),
                )
                .replace("@@CONFIG_TABS@@", &configs_html)
        })
        .await,
    )
}

/// `GET /assets/{name}`: the page's script, style and the vendored htmx.
pub async fn asset(UrlPath(name): UrlPath<String>) -> Response {
    let (body, mime): (&'static str, &str) = match name.as_str() {
        "htmx.min.js" => (include_str!("../assets/htmx.min.js"), "text/javascript"),
        "app.js" => (include_str!("../assets/app.js"), "text/javascript"),
        "app.css" => (include_str!("../assets/app.css"), "text/css"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, mime)], body).into_response()
}

// ── choices ──────────────────────────────────────────────────────────────

/// `GET /choices/episodes?ep=`: episode `<option>`s, keeping the current
/// selection.
pub async fn episode_options(State(state): Shared, Query(p): Query<Params>) -> Html<String> {
    let selected = param(&p, "ep").to_string();
    Html(blocking(move || options(&state.choices().episodes, Some(&selected))).await)
}

/// `GET /choices/scripts`.
pub async fn script_options(State(state): Shared) -> Html<String> {
    Html(blocking(move || options(&state.choices().scripts, None)).await)
}

/// `GET /choices/shows`: shows with the active one selected.
pub async fn show_options(State(state): Shared) -> Html<String> {
    Html(
        blocking(move || {
            let c = state.choices();
            options(&c.shows, configs::active_show_name(&c.shows).as_deref())
        })
        .await,
    )
}

/// `POST /choices/invalidate`: the ⟳ Refresh button, before it asks every
/// list to redraw.
pub async fn invalidate_choices(State(state): Shared) -> StatusCode {
    state.invalidate_choices();
    StatusCode::NO_CONTENT
}

// ── Setup and Project ────────────────────────────────────────────────────

/// `POST /setup/use`: `xil use SHOW`, then tell the Project tab to reload.
pub async fn setup_use(State(state): Shared, Form(p): Form<Params>) -> Response {
    let show = param(&p, "show").to_string();
    if show.is_empty() {
        return Html(status("No show selected.")).into_response();
    }
    activity::log(&format!("USE show → {show}"));
    let out = tokio::process::Command::new(&state.xil_exe)
        .arg("use")
        .arg(&show)
        .current_dir(workspace_root())
        .output()
        .await;
    let text = match out {
        Ok(o) => {
            let s = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let s = s.trim().to_string();
            if s.is_empty() {
                format!("Active show set to: {show}")
            } else {
                s
            }
        }
        Err(e) => format!("Error: {e}"),
    };
    with_trigger(status(&text), "project-changed")
}

fn job_fragment(cmd: &[String], id: u64) -> Html<String> {
    Html(format!(
        "<pre class=\"log\" data-job=\"{id}\">{}</pre>",
        esc(&runner::header(cmd))
    ))
}

/// `POST /setup/init`: `xil init`, streamed.
pub async fn setup_init(State(state): Shared, Form(p): Form<Params>) -> Html<String> {
    let show = param(&p, "show");
    if show.trim().is_empty() {
        return log_pre("Show name is required.");
    }
    let cmd = runner::cmd_init(
        &state.xil_exe,
        show,
        match param(&p, "type") {
            "" => "podcast",
            t => t,
        },
        param(&p, "season"),
        param(&p, "season_title"),
    );
    let id = runner::start_job(&state, cmd.clone());
    job_fragment(&cmd, id)
}

fn project_panel(content: &str, path: &Path, note: &str) -> String {
    format!(
        r##"<form id="project-form">
  <label class="field"><span>File</span><input readonly value="{path}"></label>
  <label class="field"><span>project.json</span><textarea name="text" rows="20">{content}</textarea></label>
  <div class="row">
    <button type="button" class="small" hx-get="/project" hx-target="#project-panel">↺ Reload</button>
    <button type="button" class="small primary" hx-post="/project/save" hx-include="#project-form" hx-target="#project-status">💾 Save</button>
  </div>
  <div id="project-status">{note}</div>
</form>"##,
        path = esc(&path.to_string_lossy()),
        content = esc(content),
    )
}

/// `GET /project`.
pub async fn project_load() -> Html<String> {
    Html(
        blocking(|| {
            let (content, path) = configs::load_project_json();
            project_panel(&content, &path, "")
        })
        .await,
    )
}

/// `POST /project/save`.
pub async fn project_save(Form(p): Form<Params>) -> Html<String> {
    let text = param(&p, "text").to_string();
    Html(status(
        &blocking(move || configs::save_project_json(&text)).await,
    ))
}

// ── Scripts ──────────────────────────────────────────────────────────────

/// `POST /scripts/analyze`.
pub async fn scripts_analyze(Form(p): Form<Params>) -> Html<String> {
    Html(script_meta(&scripts::analyze_script_header(param(
        &p, "text",
    ))))
}

/// `POST /scripts/save`: then refresh every script and episode list.
pub async fn scripts_save(State(state): Shared, Form(p): Form<Params>) -> Response {
    let (text, filename) = (
        param(&p, "text").to_string(),
        param(&p, "filename").to_string(),
    );
    let msg = blocking(move || scripts::save_script_file(&text, &filename)).await;
    state.invalidate_choices();
    with_trigger(status(&msg), "scripts-changed")
}

// ── Run Stage ────────────────────────────────────────────────────────────

const NO_TAG: &str = "⚠️ No episode tag — run Parse first (Run Stage → Parse tab).";

/// `POST /run/{stage}`: validate, build the command, start it, and return a
/// log that follows the job.
pub async fn run_stage(
    State(state): Shared,
    UrlPath(stage): UrlPath<String>,
    Form(p): Form<Params>,
) -> Html<String> {
    let ep = param(&p, "ep").trim().to_string();
    let exe = state.xil_exe.clone();
    let cmd: Result<Vec<String>, String> = match stage.as_str() {
        "scan" => {
            if ep.is_empty() {
                return log_pre("Select an episode first.");
            }
            let (slug, _) = episodes::parse_choice(&ep);
            runner::cmd_scan(
                &exe,
                &slug,
                param(&p, "script"),
                param(&p, "speakers"),
                on(&p, "json"),
            )
        }
        "parse" => {
            if ep.is_empty() {
                return log_pre("Select an episode or type a new episode tag (e.g. S04E04).");
            }
            let (mut slug, mut tag) = episodes::parse_choice(&ep);
            if tag.is_empty() {
                if episodes::looks_like_tag(&ep) {
                    slug = active_show().filter(|s| !s.is_empty()).unwrap_or_else(|| {
                        resolve_slug(
                            None,
                            &workspace_root().join("project.json").to_string_lossy(),
                        )
                    });
                    tag = ep.clone();
                } else {
                    return log_pre(
                        "⚠️ Show selected but no episode tag. Type an episode tag in the field above (e.g. S01E01) and try again.",
                    );
                }
            }
            let (script, speakers) = (
                param(&p, "script").to_string(),
                param(&p, "speakers").to_string(),
            );
            let (preview, quiet, debug, stats) = (
                int(&p, "preview", 0),
                on(&p, "quiet"),
                on(&p, "debug"),
                on(&p, "stats"),
            );
            blocking(move || {
                let o = ParseOpts {
                    script: &script,
                    preview,
                    quiet,
                    debug,
                    stats,
                    speakers: &speakers,
                };
                runner::cmd_parse(&exe, &slug, &tag, &o)
            })
            .await
        }
        "produce" | "assemble" | "daw" | "master" => {
            if ep.is_empty() {
                return log_pre("Select an episode first.");
            }
            let (_, tag) = episodes::parse_choice(&ep);
            if tag.is_empty() {
                return log_pre(NO_TAG);
            }
            Ok(match stage.as_str() {
                "produce" => runner::cmd_produce(
                    &exe,
                    &tag,
                    &ProduceOpts {
                        dry_run: on(&p, "dry_run"),
                        backend: param(&p, "backend"),
                        gen_sfx: on(&p, "gen_sfx"),
                        gen_music: on(&p, "gen_music"),
                        gen_ambience: on(&p, "gen_ambience"),
                        local_only: on(&p, "local_only"),
                        terse: on(&p, "terse"),
                        start_from: int(&p, "start_from", 0),
                        stop_at: int(&p, "stop_at", 0),
                        chatterbox_python: param(&p, "chatterbox_python"),
                        force: on(&p, "force"),
                        sfx_backend: match param(&p, "sfx_backend") {
                            "" => "elevenlabs",
                            s => s,
                        },
                        mmaudio_python: param(&p, "mmaudio_python"),
                        mmaudio_accept_nc: on(&p, "mmaudio_accept_nc"),
                    },
                ),
                "assemble" => runner::cmd_assemble(
                    &exe,
                    &tag,
                    match int(&p, "gap_ms", 600) {
                        0 => 600,
                        g => g,
                    },
                    param(&p, "parsed"),
                    param(&p, "output"),
                ),
                "daw" => runner::cmd_daw(
                    &exe,
                    &tag,
                    &DawOpts {
                        dry_run: on(&p, "dry_run"),
                        gap_ms: match int(&p, "gap_ms", 600) {
                            0 => 600,
                            g => g,
                        },
                        timeline: on(&p, "timeline"),
                        timeline_html: on(&p, "timeline_html"),
                        macro_: on(&p, "macro"),
                        save_aup3: false,
                        output_dir: param(&p, "output_dir"),
                    },
                ),
                _ => runner::cmd_master(
                    &exe,
                    &tag,
                    on(&p, "dry_run"),
                    param(&p, "output"),
                    param(&p, "daw_dir"),
                ),
            })
        }
        other => Err(format!(
            "Unknown stage: {}",
            xil_core::script::hints::py_repr(other)
        )),
    };
    match cmd {
        Ok(cmd) => {
            let id = runner::start_job(&state, cmd.clone());
            job_fragment(&cmd, id)
        }
        Err(msg) => log_pre(&msg),
    }
}

/// `GET /jobs/{id}/stream`: the job's lines as server-sent events, ending
/// with one `exit` event. A job can be followed once.
pub async fn job_stream(State(state): Shared, UrlPath(id): UrlPath<u64>) -> Response {
    let rx = state.jobs.lock().ok().and_then(|mut j| j.remove(&id));
    let Some(rx) = rx else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let stream = UnboundedReceiverStream::new(rx).map(|ev| {
        Ok::<_, Infallible>(match ev {
            // A carriage return redraws a terminal line; keep what the
            // terminal would finally show.
            JobEvent::Line(l) => Event::default()
                .event("line")
                .data(l.rsplit('\r').next().unwrap_or("")),
            JobEvent::Exit(e) => Event::default().event("exit").data(e),
        })
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ── config editors ───────────────────────────────────────────────────────

/// `GET /config/{kind}/choices`.
pub async fn config_choices(
    State(state): Shared,
    UrlPath(kind): UrlPath<String>,
    Query(p): Query<Params>,
) -> Response {
    let selected = param(&p, "path").to_string();
    match blocking(move || config_choice_list(&state, &kind).map(|c| options(&c, Some(&selected))))
        .await
    {
        Some(html) => Html(html).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /config/{kind}/load?path=`: a fresh editor textarea.
pub async fn config_load(UrlPath(kind): UrlPath<String>, Query(p): Query<Params>) -> Response {
    if !is_config_kind(&kind) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = param(&p, "path").to_string();
    let label = kind_label(&kind);
    let content = blocking(move || configs::load_config_file(&path, label)).await;
    Html(format!(
        "<textarea id=\"{kind}-editor\" name=\"text\" rows=\"30\">{}</textarea>",
        esc(&content)
    ))
    .into_response()
}

/// `POST /config/{kind}/save`.
pub async fn config_save(UrlPath(kind): UrlPath<String>, Form(p): Form<Params>) -> Response {
    let what = match kind.as_str() {
        "speakers" => "speakers.json",
        "cast" => "cast config",
        "sfx" => "sfx config",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let (path, text) = (param(&p, "path").to_string(), param(&p, "text").to_string());
    Html(status(
        &blocking(move || configs::save_config_file(&path, &text, what)).await,
    ))
    .into_response()
}

// ── Audio Preview ────────────────────────────────────────────────────────

fn player(path: Option<PathBuf>) -> String {
    let Some(path) = path else {
        return String::new();
    };
    let url = audio::cache_url(&path).or_else(|| {
        path.strip_prefix(workspace_root())
            .ok()
            .map(|rel| format!("/ws/{}", rel.to_string_lossy()))
    });
    match url {
        Some(u) => format!(
            "<audio controls preload=\"auto\" src=\"{}\"></audio>",
            esc(&u)
        ),
        None => String::new(),
    }
}

/// `GET /audio/stems?ep=&filter=`: the stem dropdown, which loads the first
/// stem into the player as soon as it lands.
pub async fn audio_stems(Query(p): Query<Params>) -> Html<String> {
    let (ep, filter) = (param(&p, "ep").to_string(), param(&p, "filter").to_string());
    Html(
        blocking(move || {
            if ep.is_empty() {
                return String::new();
            }
            activity::log(&format!("PREVIEW episode → {ep} [{filter}]"));
            let (slug, tag) = episodes::parse_choice(&ep);
            let stems: Vec<(String, String)> = episodes::load_stems(&slug, &tag, &filter)
                .into_iter()
                .map(|(l, p)| (l, p.to_string_lossy().into_owned()))
                .collect();
            let first = stems.first().map(|s| s.1.clone());
            format!(
                "<label class=\"field\"><span>Stem</span>\
                 <select name=\"stem\" class=\"mono\" hx-get=\"/audio/play\" hx-include=\"#audio-form\" \
                 hx-trigger=\"load, change\" hx-target=\"#audio-player\">{}</select></label>",
                labelled_options(&stems, first.as_deref())
            )
        })
        .await,
    )
}

/// `GET /audio/play?ep=&filter=&stem=`: copy the stem into the local cache and
/// return a player. Only a stem of the chosen episode is accepted.
pub async fn audio_play(Query(p): Query<Params>) -> Html<String> {
    let (ep, filter, stem) = (
        param(&p, "ep").to_string(),
        param(&p, "filter").to_string(),
        param(&p, "stem").to_string(),
    );
    Html(
        blocking(move || {
            if ep.is_empty() || stem.is_empty() {
                return String::new();
            }
            let (slug, tag) = episodes::parse_choice(&ep);
            let found = episodes::load_stems(&slug, &tag, &filter)
                .into_iter()
                .find(|(_, path)| path.to_string_lossy() == stem);
            match found {
                Some((label, path)) => {
                    activity::log(&format!("PREVIEW stem → {label}"));
                    player(Some(audio::cached_audio_path(&path)))
                }
                None => String::new(),
            }
        })
        .await,
    )
}

/// `POST /audio/play-all`: every stem of one kind, joined.
pub async fn audio_play_all(Form(p): Form<Params>) -> Html<String> {
    let (ep, filter) = (param(&p, "ep").to_string(), param(&p, "filter").to_string());
    Html(
        blocking(move || {
            if ep.is_empty() {
                return String::new();
            }
            activity::log(&format!("PLAY {filter} → {ep}"));
            let (slug, tag) = episodes::parse_choice(&ep);
            match audio::concatenate_stems(&slug, &tag, &filter) {
                Some(out) => player(Some(out)),
                None => format!("<p class=\"muted\">No {} stems to play.</p>", esc(&filter)),
            }
        })
        .await,
    )
}

// ── Audio Grading ────────────────────────────────────────────────────────

fn grade_status_line(state: &AppState, path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let label = match grades::grade_of(state, path).as_str() {
        "accurate" => "✓ accurate",
        "rejected" => "✗ rejected",
        _ => "• ungraded",
    };
    format!(
        "<p><b>{}</b> — {label}</p>",
        esc(&xil_core::fsutil::basename(Path::new(path)))
    )
}

fn grade_panel(state: &AppState, filter: &str, selected: Option<&str>) -> String {
    let choices = grades::sfx_choices(state, filter);
    let sel = selected
        .filter(|s| choices.iter().any(|(_, p)| p == s))
        .map(str::to_string)
        .or_else(|| choices.first().map(|c| c.1.clone()));
    format!(
        r##"<p>{summary}</p>
<form id="grade-select-form">
  <input type="hidden" name="filter" value="{filter}">
  <label class="field"><span>SFX file</span>
    <select name="path" hx-get="/grades/select" hx-include="#grade-select-form" hx-trigger="load, change" hx-target="#grade-player">{opts}</select>
  </label>
  <div id="grade-player"></div>
  <div class="row">
    <button type="button" class="small primary" hx-post="/grades/apply" hx-include="#grade-select-form" hx-vals='{{"grade": "accurate"}}' hx-target="#grade-panel">✓ Mark Accurate</button>
    <button type="button" class="small stop" hx-post="/grades/apply" hx-include="#grade-select-form" hx-vals='{{"grade": "rejected"}}' hx-target="#grade-panel">✗ Mark Rejected</button>
    <button type="button" class="small" hx-post="/grades/apply" hx-include="#grade-select-form" hx-vals='{{"grade": ""}}' hx-target="#grade-panel">Clear grade</button>
  </div>
</form>"##,
        summary = esc(&grades::sfx_summary(state)),
        filter = esc(filter),
        opts = labelled_options(&choices, sel.as_deref()),
    )
}

/// `GET /grades/list?filter=&refresh=`.
pub async fn grades_list(State(state): Shared, Query(p): Query<Params>) -> Html<String> {
    let filter = match param(&p, "filter") {
        "" => "all".to_string(),
        f => f.to_string(),
    };
    let refresh = on(&p, "refresh");
    Html(
        blocking(move || {
            if refresh {
                grades::scan_sfx_grades(&state);
            }
            grade_panel(&state, &filter, None)
        })
        .await,
    )
}

/// `GET /grades/select?path=`: player plus the grade line.
pub async fn grades_select(State(state): Shared, Query(p): Query<Params>) -> Html<String> {
    let path = param(&p, "path").to_string();
    Html(
        blocking(move || {
            let known = state
                .grade_cache
                .lock()
                .map(|c| c.contains_key(&path))
                .unwrap_or(false);
            if !known {
                return String::new();
            }
            activity::log(&format!(
                "GRADE preview → {}",
                xil_core::fsutil::basename(Path::new(&path))
            ));
            format!(
                "{}{}",
                player(Some(audio::cached_audio_path(Path::new(&path)))),
                grade_status_line(&state, &path)
            )
        })
        .await,
    )
}

/// `POST /grades/apply`: write the grade, then show the list again with the
/// same file selected, or the first one when it left the filter.
pub async fn grades_apply(State(state): Shared, Form(p): Form<Params>) -> Html<String> {
    let (path, grade, filter) = (
        param(&p, "path").to_string(),
        param(&p, "grade").to_string(),
        match param(&p, "filter") {
            "" => "all".to_string(),
            f => f.to_string(),
        },
    );
    Html(
        blocking(move || {
            if path.is_empty() {
                return grade_panel(&state, &filter, None);
            }
            match grades::apply_grade(&state, &path, &grade) {
                Ok(()) => activity::log(&format!(
                    "GRADE {} → {}",
                    if grade.is_empty() { "cleared" } else { &grade },
                    xil_core::fsutil::basename(Path::new(&path))
                )),
                Err(e) => {
                    return format!(
                        "{}{}",
                        status(&e.to_string()),
                        grade_panel(&state, &filter, Some(&path))
                    )
                }
            }
            grade_panel(&state, &filter, Some(&path))
        })
        .await,
    )
}

// ── Timeline ─────────────────────────────────────────────────────────────

/// `_timeline_iframe_html(html_path)`: the timeline in an iframe, with its
/// mtime as a cache-buster so a regenerated file is always refetched.
pub fn timeline_iframe_html(html_path: &Path) -> String {
    let v = std::fs::metadata(html_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rel = html_path
        .strip_prefix(workspace_root())
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let url: String = rel
        .split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "<iframe src=\"/ws/{}?v={v}\" style=\"width:100%;height:600px;border:none;\"></iframe>",
        esc(&url)
    )
}

/// `GET /timeline?ep=`.
pub async fn timeline(Query(p): Query<Params>) -> Html<String> {
    let ep = param(&p, "ep").to_string();
    Html(
        blocking(move || {
            if ep.is_empty() {
                return "<p>Select an episode above.</p>".to_string();
            }
            let (slug, tag) = episodes::parse_choice(&ep);
            if tag.is_empty() {
                return "<p>Select an episode above.</p>".to_string();
            }
            let paths = xil_core::workspace::derive_paths(&slug, &tag);
            let html_path = paths["daw"].join(format!("{tag}_timeline.html"));
            if !html_path.exists() {
                return format!(
                    "<p>No timeline found for <b>{t}</b>.<br>Generate it first:<br><code>xil daw --episode {t} --timeline-html</code></p>",
                    t = esc(&tag)
                );
            }
            timeline_iframe_html(&html_path)
        })
        .await,
    )
}

// ── Episodes ─────────────────────────────────────────────────────────────

/// `GET /episodes/table?force=`.
pub async fn episodes_table(State(state): Shared, Query(p): Query<Params>) -> Html<String> {
    let force = on(&p, "force");
    Html(
        blocking(move || {
            let rows = episodes::refresh_episodes(&state, force);
            let head = ["Tag", "Slug", "Title  [Arc]", "Parse", "Stems", "DAW", "Master", "Overall"]
                .iter()
                .map(|h| format!("<th>{}</th>", esc(h)))
                .collect::<String>();
            let body = rows
                .iter()
                .map(|r| format!("<tr>{}</tr>", r.iter().map(|c| format!("<td>{}</td>", esc(c))).collect::<String>()))
                .collect::<String>();
            format!("<div class=\"scroll\"><table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table></div>")
        })
        .await,
    )
}

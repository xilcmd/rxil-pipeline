//! The `xil gui` web dashboard. Port of `xil_gui.py`, with Gradio replaced by
//! axum and htmx.
//!
//! The Python dashboard is a Gradio `Blocks` app with twelve tabs and three
//! FastAPI routes the timeline editor calls. This crate serves the same tabs
//! as server-rendered HTML, swapped in place by htmx, and keeps the three
//! `/xil/*` JSON routes to their exact contracts, so a timeline written by
//! either implementation's `xil daw --timeline-html` works against either
//! server.
//!
//! What does not carry over is Gradio's plumbing: `/gradio_api/file=` becomes
//! a workspace file server under `/ws/` (the timeline's audio links are
//! relative, so they resolve the same way), progress bars become htmx
//! indicators, and generator callbacks become a job table streamed over
//! server-sent events. Stages run as subprocesses of the Rust `xil`.

pub mod activity;
pub mod audio;
pub mod configs;
pub mod episodes;
pub mod grades;
mod html;
mod pages;
pub mod runner;
pub mod scripts;
pub mod sfx_routes;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::mpsc::UnboundedReceiver;
use tower_http::services::ServeDir;
use xil_core::workspace::workspace_root;

/// Freshness glyphs for one Episodes-table row, from `xil status`'s engine.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StageCells {
    pub parse: String,
    pub produce: String,
    pub daw: String,
    pub master: String,
    pub overall: String,
}

/// `_stage_status(slug, tag)`. The engine lives in the `xil` binary crate, so
/// the binary hands it in.
pub type StatusFn = Arc<dyn Fn(&str, &str) -> StageCells + Send + Sync>;

/// Episodes-table rows: tag, slug, title, then the five status cells.
pub type Rows = Vec<Vec<String>>;

/// One line of a running stage's output, or its exit.
#[derive(Debug)]
pub enum JobEvent {
    Line(String),
    Exit(String),
}

/// The dropdown lists every tab draws from. Listing configs and scripts
/// crosses the network on a NAS workspace, so they are built once and
/// rebuilt only on ⟳ Refresh or after something that can change them (a
/// finished stage, a saved script) — as the Python app builds them at startup
/// and refreshes them from the same events.
#[derive(Clone, Debug, Default)]
pub struct Choices {
    pub episodes: Vec<String>,
    pub scripts: Vec<String>,
    pub shows: Vec<String>,
    pub speakers: Vec<String>,
    pub cast: Vec<String>,
    pub sfx: Vec<String>,
}

impl Choices {
    pub fn load() -> Choices {
        Choices {
            episodes: episodes::episode_choices(),
            scripts: scripts::script_choices(),
            shows: configs::list_available_shows(),
            speakers: configs::find_speakers_configs(),
            cast: configs::find_cast_configs(),
            sfx: configs::find_sfx_configs(),
        }
    }
}

/// Everything a handler needs. Workspace paths are read from the environment
/// on each call, as the Python module does.
pub struct AppState {
    /// The `xil` executable stages run under.
    pub xil_exe: PathBuf,
    pub status: StatusFn,
    pub(crate) jobs: Mutex<HashMap<u64, UnboundedReceiver<JobEvent>>>,
    next_job: AtomicU64,
    /// `_EPISODES_CACHE`: rows memoised per workspace root.
    pub(crate) episodes_cache: Mutex<HashMap<PathBuf, (Instant, Rows)>>,
    /// `_sfx_grade_cache`: `{path: grade}`, rebuilt only on Load/Refresh.
    pub grade_cache: Mutex<BTreeMap<String, String>>,
    choices: Mutex<Option<Arc<Choices>>>,
}

impl AppState {
    pub fn new(xil_exe: PathBuf, status: StatusFn) -> Arc<AppState> {
        Arc::new(AppState {
            xil_exe,
            status,
            jobs: Mutex::new(HashMap::new()),
            next_job: AtomicU64::new(1),
            episodes_cache: Mutex::new(HashMap::new()),
            grade_cache: Mutex::new(BTreeMap::new()),
            choices: Mutex::new(None),
        })
    }

    /// The cached dropdown lists, built on first use. Blocking.
    pub fn choices(&self) -> Arc<Choices> {
        let mut g = self.choices.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(|| Arc::new(Choices::load())).clone()
    }

    /// Drop the cached lists so the next request rebuilds them.
    pub fn invalidate_choices(&self) {
        if let Ok(mut g) = self.choices.lock() {
            *g = None;
        }
    }

    pub(crate) fn job_id(&self) -> u64 {
        self.next_job.fetch_add(1, Ordering::Relaxed)
    }
}

/// Every route the dashboard serves.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(pages::index))
        .route("/assets/{name}", get(pages::asset))
        .route("/episodes/table", get(pages::episodes_table))
        .route("/choices/episodes", get(pages::episode_options))
        .route("/choices/scripts", get(pages::script_options))
        .route("/choices/shows", get(pages::show_options))
        .route("/choices/invalidate", post(pages::invalidate_choices))
        .route("/setup/use", post(pages::setup_use))
        .route("/setup/init", post(pages::setup_init))
        .route("/project", get(pages::project_load))
        .route("/project/save", post(pages::project_save))
        .route("/scripts/analyze", post(pages::scripts_analyze))
        .route("/scripts/save", post(pages::scripts_save))
        .route("/run/{stage}", post(pages::run_stage))
        .route("/jobs/{id}/stream", get(pages::job_stream))
        .route("/config/{kind}/choices", get(pages::config_choices))
        .route("/config/{kind}/load", get(pages::config_load))
        .route("/config/{kind}/save", post(pages::config_save))
        .route("/audio/stems", get(pages::audio_stems))
        .route("/audio/play", get(pages::audio_play))
        .route("/audio/play-all", post(pages::audio_play_all))
        .route("/grades/list", get(pages::grades_list))
        .route("/grades/select", get(pages::grades_select))
        .route("/grades/apply", post(pages::grades_apply))
        .route("/timeline", get(pages::timeline))
        .route("/xil/get-sfx", get(sfx_routes::get_sfx))
        .route("/xil/update-sfx", post(sfx_routes::update_sfx))
        .route(
            "/xil/update-sfx-defaults",
            post(sfx_routes::update_sfx_defaults),
        )
        .nest_service("/ws", ServeDir::new(workspace_root()))
        .nest_service("/cache", ServeDir::new(audio::audio_cache_dir()))
        .with_state(state)
}

/// `_print_workspace_banner()`: say which workspace this process is frozen to
/// before anything else starts.
pub fn print_workspace_banner() -> PathBuf {
    let workspace = workspace_root();
    println!("xil-gui: workspace root = {}", workspace.display());
    if !workspace.is_dir() {
        println!(
            "xil-gui: WARNING — workspace root does not exist: {}",
            workspace.display()
        );
    }
    workspace
}

/// Bind and serve until the process is stopped.
pub async fn serve(host: &str, port: u16, state: Arc<AppState>) -> anyhow::Result<()> {
    let addr: SocketAddr = tokio::net::lookup_host((host, port))
        .await?
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve {host}"))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("* Running on local URL:  http://{addr}");
    // Warm the dropdown lists so the first page load does not pay for them.
    let warm = state.clone();
    tokio::task::spawn_blocking(move || {
        warm.choices();
    });
    axum::serve(listener, router(state)).await?;
    Ok(())
}

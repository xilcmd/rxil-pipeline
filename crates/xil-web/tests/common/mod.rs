//! Shared test scaffolding: a temporary workspace bound to
//! `XIL_PROJECTROOT`, and one-shot requests against the router.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;
use xil_web::{AppState, StageCells};

/// Tests that set process environment variables take this lock, so a
/// threaded `cargo test` run cannot interleave two workspaces.
static ENV: Mutex<()> = Mutex::new(());

pub struct Workspace {
    pub dir: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl Workspace {
    pub fn new() -> Workspace {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cache = dir.path().join("xdg-cache");
        std::env::set_var("XIL_PROJECTROOT", &root);
        std::env::set_var("XDG_CACHE_HOME", &cache);
        Workspace { dir, _guard: guard }
    }

    pub fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path().join("workspace")).unwrap()
    }

    pub fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.root().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root().join(rel)).unwrap()
    }

    pub fn json(&self, rel: &str) -> serde_json::Value {
        serde_json::from_str(&self.read(rel)).unwrap()
    }
}

pub fn fixed_status(_: &str, _: &str) -> StageCells {
    StageCells {
        parse: "✓".into(),
        produce: "✓ 3".into(),
        daw: "○".into(),
        master: "○".into(),
        overall: "○ missing".into(),
    }
}

pub fn state_with(exe: &Path) -> Arc<AppState> {
    AppState::new(exe.to_path_buf(), Arc::new(fixed_status))
}

pub fn state() -> Arc<AppState> {
    state_with(Path::new("/bin/echo"))
}

pub async fn send(state: Arc<AppState>, req: Request<Body>) -> (StatusCode, String) {
    let resp = xil_web::router(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

pub async fn get(state: Arc<AppState>, uri: &str) -> (StatusCode, String) {
    send(state, Request::get(uri).body(Body::empty()).unwrap()).await
}

pub async fn post_json(
    state: Arc<AppState>,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, req).await
}

pub async fn post_form(
    state: Arc<AppState>,
    uri: &str,
    pairs: &[(&str, &str)],
) -> (StatusCode, String) {
    let body: String = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(state, req).await
}

/// Percent-encode a query or form component.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A short silent MP3, or `None` when ffmpeg is not installed.
pub fn silent_mp3(path: &Path, seconds: f64) -> Option<()> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("anullsrc=r=44100:cl=mono")
        .args(["-t", &seconds.to_string(), "-b:a", "64k"])
        .arg(path)
        .status()
        .ok()?
        .success();
    ok.then_some(())
}

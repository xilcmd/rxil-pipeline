//! Network clients for the pipeline: ElevenLabs (text-to-speech, sound
//! effects, user quota, voices, Studio), Anthropic (the publish stage) and
//! Google Translate TTS (gTTS, the free draft voice backend).
//!
//! Each client speaks the same HTTP the Python SDKs do, so the requests a
//! Rust run makes can be compared against a Python run's request for request
//! (see `tools/parity/mockapi.py`). Base URLs can be redirected with
//! `XIL_ELEVENLABS_BASE_URL`, `ANTHROPIC_BASE_URL` and `XIL_GTTS_BASE_URL`.

pub mod anthropic;
pub mod elevenlabs;
pub mod gtts;

use std::io::Read;
use std::time::Duration;

/// A failed call, split the way the pipeline's retry logic needs it.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The server answered with a non-2xx status (the SDK's `ApiError`).
    #[error("status_code: {status}, body: {body}")]
    Status { status: u16, body: String },
    /// No usable answer: DNS, connect, TLS, reset (httpx's `TransportError`).
    #[error("{0}")]
    Transport(String),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Status { status, .. } => Some(*status),
            ApiError::Transport(_) => None,
        }
    }
}

/// One shared agent: connection reuse, and a generous read timeout for
/// long TTS renders.
pub(crate) fn agent() -> ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(30))
                .timeout_read(Duration::from_secs(240))
                .build()
        })
        .clone()
}

/// httpx logs every response at INFO, and the pipeline's root logger
/// passes it to both sinks: `HTTP Request: GET <url> "HTTP/1.1 200 OK"`.
fn log_response(method: &str, resp: &ureq::Response) {
    xil_core::log::info(&format!(
        "HTTP Request: {method} {} \"{} {} {}\"",
        resp.get_url(),
        resp.http_version(),
        resp.status(),
        resp.status_text()
    ));
}

/// Run a request, logging the response like httpx and turning HTTP errors
/// into [`ApiError`].
pub(crate) fn call_logged(
    method: &str,
    result: Result<ureq::Response, ureq::Error>,
) -> Result<ureq::Response, ApiError> {
    match result {
        Ok(r) => {
            log_response(method, &r);
            Ok(r)
        }
        Err(ureq::Error::Status(status, resp)) => {
            log_response(method, &resp);
            Err(ApiError::Status {
                status,
                body: resp.into_string().unwrap_or_default(),
            })
        }
        Err(ureq::Error::Transport(t)) => Err(ApiError::Transport(t.to_string())),
    }
}

/// As [`call_logged`] but silent — `requests` (gTTS) logs nothing at INFO.
pub(crate) fn call(
    result: Result<ureq::Response, ureq::Error>,
) -> Result<ureq::Response, ApiError> {
    match result {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(status, resp)) => Err(ApiError::Status {
            status,
            body: resp.into_string().unwrap_or_default(),
        }),
        Err(ureq::Error::Transport(t)) => Err(ApiError::Transport(t.to_string())),
    }
}

/// POST a JSON body. The result goes straight into [`call_logged`], so
/// `ureq::Error` stays unboxed.
#[allow(clippy::result_large_err)]
pub(crate) fn send_json(
    req: ureq::Request,
    body: &serde_json::Value,
) -> Result<ureq::Response, ureq::Error> {
    req.set("Content-Type", "application/json")
        .send_string(&serde_json::to_string(body).unwrap_or_default())
}

pub(crate) fn read_bytes(resp: ureq::Response) -> Result<Vec<u8>, ApiError> {
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| ApiError::Transport(e.to_string()))?;
    Ok(buf)
}

pub(crate) fn read_json(resp: ureq::Response) -> Result<serde_json::Value, ApiError> {
    let bytes = read_bytes(resp)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::Transport(format!("invalid JSON response: {e}")))
}

/// `$name` when set and non-empty, else the default.
pub(crate) fn base_url(env: &str, default: &str) -> String {
    std::env::var(env)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
        .trim_end_matches('/')
        .to_string()
}

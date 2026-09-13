//! The ElevenLabs endpoints the pipeline calls, shaped as `elevenlabs`
//! 2.x sends them.

use serde_json::{Map, Value};

use crate::{agent, base_url, call_logged, read_bytes, read_json, send_json, ApiError};

pub const DEFAULT_BASE_URL: &str = "https://api.elevenlabs.io";

/// `ElevenLabs(api_key=os.environ.get("ELEVENLABS_API_KEY"))`.
#[derive(Clone, Debug)]
pub struct Client {
    api_key: Option<String>,
    base: String,
}

impl Client {
    pub fn new(api_key: Option<String>) -> Client {
        Client {
            api_key: api_key.filter(|k| !k.is_empty()),
            base: base_url("XIL_ELEVENLABS_BASE_URL", DEFAULT_BASE_URL),
        }
    }

    /// From `ELEVENLABS_API_KEY`, absent or not.
    pub fn from_env() -> Client {
        Client::new(std::env::var("ELEVENLABS_API_KEY").ok())
    }

    pub fn has_key(&self) -> bool {
        self.api_key.is_some()
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        let mut req = agent().request(method, &format!("{}{path}", self.base));
        // The SDK only sends the header when it was given a key.
        if let Some(k) = &self.api_key {
            req = req.set("xi-api-key", k);
        }
        req
    }

    /// `client.user.get()`.
    pub fn user_get(&self) -> Result<Value, ApiError> {
        read_json(call_logged("GET", self.request("GET", "/v1/user").call())?)
    }

    /// `client.voices.get_all()`.
    pub fn voices_get_all(&self) -> Result<Value, ApiError> {
        read_json(call_logged(
            "GET",
            self.request("GET", "/v1/voices").call(),
        )?)
    }

    /// `client.text_to_speech.convert(voice_id, text=, model_id=,
    /// output_format=, voice_settings=)` collected into bytes.
    ///
    /// `voice_settings`: `None` leaves the field out; `Some(Value::Null)`
    /// sends `null`, which is what passing `voice_settings=None` explicitly
    /// does in the SDK.
    pub fn text_to_speech(
        &self,
        voice_id: &str,
        text: &str,
        model_id: &str,
        output_format: &str,
        voice_settings: Option<Value>,
    ) -> Result<Vec<u8>, ApiError> {
        let mut body = Map::new();
        body.insert("text".into(), text.into());
        body.insert("model_id".into(), model_id.into());
        if let Some(vs) = voice_settings {
            body.insert("voice_settings".into(), vs);
        }
        let req = self
            .request(
                "POST",
                &format!("/v1/text-to-speech/{}", encode_path(voice_id)),
            )
            .query("output_format", output_format);
        read_bytes(call_logged("POST", send_json(req, &Value::Object(body)))?)
    }

    /// `client.text_to_sound_effects.convert(text=, duration_seconds=,
    /// prompt_influence=)` collected into bytes.
    pub fn sound_generation(
        &self,
        text: &str,
        duration_seconds: Option<f64>,
        prompt_influence: Option<f64>,
    ) -> Result<Vec<u8>, ApiError> {
        let mut body = Map::new();
        body.insert("text".into(), text.into());
        if let Some(d) = duration_seconds {
            body.insert("duration_seconds".into(), num(d));
        }
        if let Some(p) = prompt_influence {
            body.insert("prompt_influence".into(), num(p));
        }
        read_bytes(call_logged(
            "POST",
            send_json(
                self.request("POST", "/v1/sound-generation"),
                &Value::Object(body),
            ),
        )?)
    }

    /// One page of `GET /v1/sound-generation/history`, as `XILU005` fetches
    /// it with plain httpx (not the SDK).
    pub fn sound_generation_history(
        &self,
        page_size: u32,
        start_after: Option<&str>,
    ) -> Result<Value, ApiError> {
        let mut req = self
            .request("GET", "/v1/sound-generation/history")
            .set("accept", "application/json")
            .query("page_size", &page_size.to_string());
        if let Some(id) = start_after {
            req = req.query("start_after_history_item_id", id);
        }
        read_json(call_logged("GET", req.call())?)
    }

    /// `client.studio.projects.create(...)` — a multipart form.
    pub fn studio_projects_create(
        &self,
        fields: &[(&str, String)],
        file: Option<(&str, &str, Vec<u8>)>,
    ) -> Result<Value, ApiError> {
        let boundary = "xil-rxil-boundary-7c1f0e5a";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        }
        if let Some((name, filename, data)) = file {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(&data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let req = self.request("POST", "/v1/studio/projects").set(
            "Content-Type",
            &format!("multipart/form-data; boundary={boundary}"),
        );
        read_json(call_logged("POST", req.send_bytes(&body))?)
    }
}

fn num(x: f64) -> Value {
    serde_json::Number::from_f64(x)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Percent-encode a path segment the way httpx would.
fn encode_path(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

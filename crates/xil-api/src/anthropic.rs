//! Anthropic Messages API, as `anthropic.Anthropic().messages.create` calls it.

use serde_json::Value;

use crate::{agent, base_url, call_logged, read_json, send_json, ApiError};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

pub struct Client {
    api_key: String,
    base: String,
}

impl Client {
    pub fn new(api_key: &str) -> Client {
        Client {
            api_key: api_key.to_string(),
            base: base_url("ANTHROPIC_BASE_URL", DEFAULT_BASE_URL),
        }
    }

    /// `client.messages.create(**body)`, returning the response JSON.
    pub fn messages_create(&self, body: &Value) -> Result<Value, ApiError> {
        let req = agent()
            .post(&format!("{}/v1/messages", self.base))
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", API_VERSION);
        read_json(call_logged("POST", send_json(req, body))?)
    }
}

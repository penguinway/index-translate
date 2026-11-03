use crate::config::USER_AGENT;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(120);
pub const READ_TIMEOUT: Duration = Duration::from_secs(120);

pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .pool_idle_timeout(Duration::from_secs(90))
        .user_agent(USER_AGENT)
        .build()
        .context("failed to build HTTP client")
}

/// Mirror the Python bridge: keep reasoning disabled unless the caller opted in,
/// and default to greedy decoding.
pub fn patch_payload(raw: &[u8]) -> Vec<u8> {
    let Ok(Value::Object(mut payload)) = serde_json::from_slice::<Value>(raw) else {
        return raw.to_vec();
    };

    match payload.get_mut("chat_template_kwargs") {
        Some(Value::Object(kwargs)) => {
            kwargs
                .entry("enable_thinking")
                .or_insert(Value::Bool(false));
        }
        _ => {
            payload.insert(
                "chat_template_kwargs".to_owned(),
                json!({ "enable_thinking": false }),
            );
        }
    }

    match payload.get("temperature") {
        None | Some(Value::Null) => {
            payload.insert("temperature".to_owned(), json!(0.0));
        }
        Some(_) => {}
    }

    serde_json::to_vec(&Value::Object(payload)).unwrap_or_else(|_| raw.to_vec())
}

/// Map a client endpoint to the upstream path. The base URL already ends in `/v1`.
pub fn upstream_path(request_path: &str) -> Option<&'static str> {
    match request_path.trim_end_matches('/') {
        "/v1/chat/completions" | "/chat/completions" => Some("/chat/completions"),
        "/v1/completions" | "/completions" => Some("/completions"),
        "/v1/responses" | "/responses" => Some("/responses"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_thinking_and_temperature_defaults() {
        let patched = patch_payload(br#"{"model":"m","messages":[]}"#);
        let value: Value = serde_json::from_slice(&patched).unwrap();
        assert_eq!(value["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(value["temperature"], 0.0);
        assert_eq!(value["model"], "m");
    }

    #[test]
    fn preserves_explicit_options() {
        let patched = patch_payload(
            br#"{"temperature":0.7,"chat_template_kwargs":{"enable_thinking":true}}"#,
        );
        let value: Value = serde_json::from_slice(&patched).unwrap();
        assert_eq!(value["temperature"], 0.7);
        assert_eq!(value["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn replaces_invalid_chat_template_kwargs() {
        let patched = patch_payload(br#"{"temperature":null,"chat_template_kwargs":"unexpected"}"#);
        let value: Value = serde_json::from_slice(&patched).unwrap();
        assert_eq!(value["temperature"], 0.0);
        assert_eq!(value["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn leaves_non_json_bodies_untouched() {
        let raw = b"not json";
        assert_eq!(patch_payload(raw), raw);
    }

    #[test]
    fn maps_client_paths_to_upstream_paths() {
        assert_eq!(
            upstream_path("/v1/chat/completions"),
            Some("/chat/completions")
        );
        assert_eq!(
            upstream_path("/v1/chat/completions/"),
            Some("/chat/completions")
        );
        assert_eq!(
            upstream_path("/chat/completions"),
            Some("/chat/completions")
        );
        assert_eq!(upstream_path("/v1/completions"), Some("/completions"));
        assert_eq!(upstream_path("/v1/responses"), Some("/responses"));
        assert_eq!(upstream_path("/v1/embeddings"), None);
    }
}

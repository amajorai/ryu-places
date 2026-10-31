//! The side-model callback into Core, used by the AI import and trip-drafting
//! features. Same edge as News: `POST /api/host/model/complete` on loopback,
//! authenticated with the token Core injects at spawn. It needs the
//! `hook:side-model` grant in BOTH the sidecar's `host_api.grants` and the
//! manifest's top-level `permission_grants`.
//!
//! [`Host::from_env`] is `None` when Core's callback env is missing; AI routes
//! then answer 503 with [`no_host_message`] while every non-AI route keeps
//! working. (The HTTP bearer gate still needs `RYU_EXT_TOKEN`, so a run with no
//! token at all answers 401 everywhere.)

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

const ENV_TOKEN: &str = "RYU_EXT_TOKEN";
const ENV_PLUGIN_ID: &str = "RYU_EXT_PLUGIN_ID";
const ENV_CORE_PORT: &str = "RYU_CORE_PORT";

#[derive(Debug, thiserror::Error)]
#[error("{}", no_host_message())]
pub struct NoHost;

/// Anything a model-backed request can complete against. The trait lets tests
/// script model replies without a running Core.
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    async fn complete(&self, system: &str, prompt: &str) -> Result<String>;
}

#[derive(Debug, Clone)]
pub struct Host {
    base: String,
    token: String,
    plugin_id: String,
    http: reqwest::Client,
}

impl Host {
    #[must_use]
    pub fn from_env() -> Option<Host> {
        let token = non_empty(ENV_TOKEN)?;
        let plugin_id = non_empty(ENV_PLUGIN_ID)?;
        let port = non_empty(ENV_CORE_PORT)?;
        Some(Host {
            base: format!("http://127.0.0.1:{port}"),
            token,
            plugin_id,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(90))
                .build()
                .unwrap_or_default(),
        })
    }
}

#[async_trait::async_trait]
impl Model for Host {
    async fn complete(&self, system: &str, prompt: &str) -> Result<String> {
        let response = self
            .http
            .post(format!("{}/api/host/model/complete", self.base))
            .bearer_auth(&self.token)
            .header("x-ryu-plugin-id", &self.plugin_id)
            .json(&json!({ "system": system, "prompt": prompt }))
            .send()
            .await
            .context("the model callback to Core failed")?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .context("Core's model response was not JSON")?;
        if !status.is_success() {
            let detail = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no detail");
            return Err(anyhow!("Core refused the model call ({status}): {detail}"));
        }
        completion_text(&payload).ok_or_else(|| anyhow!("Core's model response carried no text"))
    }
}

/// Core's `/api/host/model/complete` answers `{ "result": <value> }`, where the
/// value is the `host.sideModel` bridge result: a bare string, or a
/// `{ text | content | output }` object (or an array of them) depending on the
/// provider. Same reading as RLM's host. Empty text counts as no text.
fn completion_text(payload: &Value) -> Option<String> {
    let text = render_result(payload.get("result")?);
    (!text.trim().is_empty()).then_some(text)
}

fn render_result(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .or_else(|| map.get("output"))
            .map(render_result)
            .unwrap_or_default(),
        Value::Array(items) => items.iter().map(render_result).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

fn non_empty(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

#[must_use]
pub fn no_host_message() -> &'static str {
    "AI features need Ryu's model, and this app is not connected to Ryu right now; \
     open Places from the Ryu app"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cores_result_envelope() {
        assert_eq!(
            completion_text(&json!({ "result": "[{\"name\":\"Cafe\"}]" })).as_deref(),
            Some("[{\"name\":\"Cafe\"}]")
        );
        assert_eq!(
            completion_text(&json!({ "result": { "text": "hi" } })).as_deref(),
            Some("hi")
        );
        assert_eq!(
            completion_text(&json!({ "result": [{ "text": "a" }, { "content": "b" }] })).as_deref(),
            Some("ab")
        );
    }

    #[test]
    fn a_response_without_a_result_carries_no_text() {
        // The pre-fix reader accepted a top-level `text`; Core never sends one.
        assert_eq!(completion_text(&json!({ "text": "hi" })), None);
        assert_eq!(completion_text(&json!({ "result": "  " })), None);
        assert_eq!(completion_text(&json!({ "result": null })), None);
    }

    #[tokio::test]
    async fn complete_parses_a_live_core_result_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text[..head_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let body = r#"{"result":"drafted"}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        let host = Host {
            base: format!("http://127.0.0.1:{port}"),
            token: "tok".to_owned(),
            plugin_id: "@ryu/places".to_owned(),
            http: reqwest::Client::new(),
        };
        let text = host.complete("sys", "prompt").await.unwrap();
        assert_eq!(text, "drafted");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /api/host/model/complete "));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer tok"));
    }
}

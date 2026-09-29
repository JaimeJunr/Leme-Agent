//! OpenRouter client: streaming chat completions with tool calls, reasoning
//! (including `reasoning_details` round-tripping), usage/cost accounting,
//! prompt-cache breakpoints and robust retries.

pub mod catalog;
pub mod stream;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub use catalog::{Catalog, ModelInfo};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON argument string exactly as the model produced it.
    pub arguments: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// Dollar cost as reported by OpenRouter (or estimated from pricing).
    pub cost: f64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.prompt_tokens += o.prompt_tokens;
        self.completion_tokens += o.completion_tokens;
        self.cached_tokens += o.cached_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
        self.cost += o.cost;
    }
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub text: String,
    pub reasoning: String,
    pub reasoning_details: Vec<Value>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Usage,
    pub model: String,
    pub provider: Option<String>,
    /// Citations returned by web search (`url_citation` annotations).
    pub annotations: Vec<Value>,
}

/// Incremental events surfaced while a completion streams.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    /// A tool call started streaming (index, name).
    ToolCallStart(#[allow(dead_code)] usize, String),
    /// The stream failed mid-way and is being retried from scratch.
    Restart,
    /// Waiting before a retry (message).
    Retrying(String),
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub reasoning: Option<Value>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub provider: Option<Value>,
    pub fallback_models: Vec<String>,
    pub session_id: Option<String>,
    pub plugins: Option<Value>,
    pub extra: Option<Value>,
    pub parallel_tool_calls: Option<bool>,
}

impl ChatRequest {
    pub fn to_body(&self, stream: bool) -> Value {
        let mut b = json!({
            "model": self.model,
            "messages": self.messages,
            "stream": stream,
            "usage": {"include": true},
        });
        let o = b.as_object_mut().unwrap();
        if !self.tools.is_empty() {
            o.insert("tools".into(), Value::Array(self.tools.clone()));
            o.insert("tool_choice".into(), json!("auto"));
            if let Some(p) = self.parallel_tool_calls {
                o.insert("parallel_tool_calls".into(), json!(p));
            }
        }
        if let Some(r) = &self.reasoning {
            o.insert("reasoning".into(), r.clone());
        }
        if let Some(m) = self.max_tokens {
            o.insert("max_tokens".into(), json!(m));
        }
        if let Some(t) = self.temperature {
            o.insert("temperature".into(), json!(t));
        }
        if let Some(p) = &self.provider {
            o.insert("provider".into(), p.clone());
        }
        if !self.fallback_models.is_empty() {
            let mut models = vec![self.model.clone()];
            models.extend(self.fallback_models.iter().cloned());
            o.insert("models".into(), json!(models));
        }
        if let Some(s) = &self.session_id {
            o.insert("session_id".into(), json!(s));
        }
        if let Some(p) = &self.plugins {
            o.insert("plugins".into(), p.clone());
        }
        if let Some(Value::Object(extra)) = &self.extra {
            for (k, v) in extra {
                o.insert(k.clone(), v.clone());
            }
        }
        b
    }
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub max_retries: u32,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OpenRouter error {}: {}", self.status, self.message)
    }
}
impl std::error::Error for ApiError {}

impl ApiError {
    fn retryable(&self) -> bool {
        matches!(
            self.status,
            408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 520..=599
        )
    }
}

impl LlmClient {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent(concat!("leme/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            max_retries: 6,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", "https://github.com/JaimeJunr/Leme-Agent")
            .header("X-Title", "leme")
    }

    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", "https://github.com/JaimeJunr/Leme-Agent")
            .header("X-Title", "leme")
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Stream a completion, retrying transient failures (including streams
    /// that die half-way) with exponential backoff + jitter.
    pub async fn stream(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Completion> {
        let body = req.to_body(true);
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let res = self.stream_once(&body, cancel, on_event).await;
            match res {
                Ok(c) => return Ok(c),
                Err(e) => {
                    if cancel.is_cancelled() {
                        bail!("interrupted");
                    }
                    let (retryable, wait_hint, partial) = classify(&e);
                    if !retryable || attempt > self.max_retries {
                        return Err(e);
                    }
                    let backoff = wait_hint.unwrap_or_else(|| backoff_delay(attempt));
                    if partial {
                        on_event(StreamEvent::Restart);
                    }
                    on_event(StreamEvent::Retrying(format!(
                        "{} — retrying in {:.0}s (attempt {}/{})",
                        short_err(&e),
                        backoff.as_secs_f64().max(1.0),
                        attempt,
                        self.max_retries
                    )));
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => bail!("interrupted"),
                    }
                }
            }
        }
    }

    async fn stream_once(
        &self,
        body: &Value,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Completion> {
        let send = self.post("/chat/completions").json(body).send();
        let resp = tokio::select! {
            r = send => r.map_err(|e| anyhow!(TransportError{ msg: e.to_string(), partial: false }))?,
            _ = cancel.cancelled() => bail!("interrupted"),
        };
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|s| Duration::from_secs_f64(s.min(120.0)));
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(ApiError {
                status: status.as_u16(),
                message: extract_error_message(&text),
                retry_after,
            }));
        }
        stream::consume(resp, cancel, on_event).await
    }

    /// Non-streaming convenience wrapper (still uses SSE under the hood so
    /// that retries/cancellation behave the same).
    pub async fn complete(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<Completion> {
        let mut noop = |_e: StreamEvent| {};
        self.stream(req, cancel, &mut noop).await
    }

    /// Credit/limit info for the current key.
    pub async fn key_info(&self) -> Result<Value> {
        let r = self.get("/key").send().await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{}: {}", status, extract_error_message(&v.to_string()));
        }
        Ok(v.get("data").cloned().unwrap_or(v))
    }
}

#[derive(Debug)]
pub struct TransportError {
    pub msg: String,
    /// Some output had already been streamed before the failure.
    pub partial: bool,
}
impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "connection error: {}", self.msg)
    }
}
impl std::error::Error for TransportError {}

/// Error returned inside an SSE stream (OpenRouter sends `{"error":{...}}` chunks).
#[derive(Debug)]
pub struct StreamApiError {
    pub code: Option<i64>,
    pub message: String,
    pub partial: bool,
}
impl std::fmt::Display for StreamApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "provider error {c}: {}", self.message),
            None => write!(f, "provider error: {}", self.message),
        }
    }
}
impl std::error::Error for StreamApiError {}

fn classify(e: &anyhow::Error) -> (bool, Option<Duration>, bool) {
    if let Some(a) = e.downcast_ref::<ApiError>() {
        return (a.retryable(), a.retry_after, false);
    }
    if let Some(t) = e.downcast_ref::<TransportError>() {
        return (true, None, t.partial);
    }
    if let Some(s) = e.downcast_ref::<StreamApiError>() {
        let retry = match s.code {
            Some(c) => matches!(c, 408 | 429 | 500..=599),
            None => true,
        };
        return (retry, None, s.partial);
    }
    (false, None, false)
}

fn short_err(e: &anyhow::Error) -> String {
    crate::util::ellipsize(&e.to_string(), 160)
}

fn backoff_delay(attempt: u32) -> Duration {
    let base = 1.0f64 * 2f64.powi(attempt as i32 - 1);
    let capped = base.min(30.0);
    // Deterministic-enough jitter without pulling in a RNG crate.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter = (nanos % 1000) as f64 / 1000.0 * 0.3 * capped;
    Duration::from_secs_f64(capped + jitter)
}

pub fn extract_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let err = v.get("error").unwrap_or(&v);
        let mut msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        // OpenRouter nests the upstream provider's error in metadata.raw.
        if let Some(raw) = err.pointer("/metadata/raw").and_then(|r| r.as_str()) {
            let raw_msg = serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|r| {
                    r.pointer("/error/message")
                        .and_then(|m| m.as_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| raw.to_string());
            msg = format!("{msg} ({})", crate::util::ellipsize(&raw_msg, 300));
        }
        if let Some(p) = err
            .pointer("/metadata/provider_name")
            .and_then(|r| r.as_str())
        {
            msg = format!("{msg} [provider: {p}]");
        }
        if !msg.is_empty() {
            return msg;
        }
    }
    crate::util::ellipsize(body.trim(), 400)
}

/// Build the OpenRouter `reasoning` parameter for a given effort string.
pub fn reasoning_param(effort: &str, info: Option<&ModelInfo>) -> Option<Value> {
    let effort = effort.trim().to_lowercase();
    let supports = info.map(|i| i.supports_reasoning).unwrap_or(false);
    if effort.is_empty() {
        return None;
    }
    if !supports {
        return None;
    }
    if effort == "none" || effort == "off" {
        if info.map(|i| i.reasoning_mandatory).unwrap_or(false) {
            return None;
        }
        return Some(json!({"effort": "none"}));
    }
    // Map to the closest effort the model supports.
    let effort = info.map(|i| i.closest_effort(&effort)).unwrap_or(effort);
    Some(json!({ "effort": effort }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_message_extraction() {
        let body = r#"{"error":{"code":400,"message":"Provider returned error","metadata":{"raw":"{\"error\":{\"message\":\"prompt is too long\"}}","provider_name":"Anthropic"}}}"#;
        let m = extract_error_message(body);
        assert!(m.contains("prompt is too long"));
        assert!(m.contains("Anthropic"));
    }

    #[test]
    fn body_has_fallbacks_and_usage() {
        let r = ChatRequest {
            model: "a/b".into(),
            fallback_models: vec!["c/d".into()],
            session_id: Some("s1".into()),
            ..Default::default()
        };
        let b = r.to_body(true);
        assert_eq!(b["models"][1], "c/d");
        assert_eq!(b["usage"]["include"], true);
        assert_eq!(b["session_id"], "s1");
        assert!(b.get("tools").is_none());
    }
}

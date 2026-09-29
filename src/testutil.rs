//! Test helpers: isolated dirs and a scripted mock of the OpenRouter API.

use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Point config/data/cache dirs at a process-wide temp dir (once).
pub fn env() {
    static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("harness-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        unsafe {
            std::env::set_var("HARNESS_DATA_DIR", d.join("data"));
            std::env::set_var("HARNESS_CACHE_DIR", d.join("cache"));
            std::env::set_var("HARNESS_CONFIG_DIR", d.join("config"));
        }
        d
    });
}

pub enum Resp {
    Sse(Vec<Value>),
    Status(u16, String),
}

pub fn text(s: &str) -> Value {
    json!({"choices": [{"delta": {"content": s}}]})
}

pub fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}}]})
}

pub fn calls(list: &[(&str, &str, Value)]) -> Value {
    let tc: Vec<Value> = list
        .iter()
        .enumerate()
        .map(|(i, (id, name, args))| json!({"index": i, "id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}))
        .collect();
    json!({"choices": [{"delta": {"tool_calls": tc}}]})
}

pub fn finish(reason: &str, prompt_tokens: u64) -> Value {
    json!({"choices": [{"delta": {}, "finish_reason": reason}], "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": 10, "cost": 0.001, "prompt_tokens_details": {"cached_tokens": prompt_tokens / 2}}})
}

/// Shorthand: a text-only response.
pub fn say(s: &str) -> Resp {
    Resp::Sse(vec![text(s), finish("stop", 1000)])
}

/// Shorthand: a single tool call response.
pub fn tool(id: &str, name: &str, args: Value) -> Resp {
    Resp::Sse(vec![call(id, name, args), finish("tool_calls", 1000)])
}

pub struct Mock {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
}

pub fn catalog() -> Value {
    json!({"data": [
        {"id": "test/model", "name": "Test", "context_length": 100000,
         "pricing": {"prompt": "0.000001", "completion": "0.000002"},
         "supported_parameters": ["tools", "reasoning"],
         "architecture": {"input_modalities": ["text", "image"]},
         "top_provider": {"max_completion_tokens": 8000}},
        {"id": "openai/gpt-5.6-sol", "name": "GPT", "context_length": 100000,
         "pricing": {"prompt": "0.000001", "completion": "0.000002"},
         "supported_parameters": ["tools"]}
    ]})
}

impl Mock {
    pub async fn start(responses: Vec<Resp>) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let reqs = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let reqs = reqs.clone();
                let queue = queue.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    let header_end;
                    loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = p + 4;
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + len {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let first = head.lines().next().unwrap_or("").to_string();
                    if first.starts_with("GET") && first.contains("/models") {
                        let body = catalog().to_string();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }
                    let body: Value = serde_json::from_slice(&buf[header_end..header_end + len])
                        .unwrap_or(Value::Null);
                    // Background title generation gets a canned answer.
                    if body["messages"][0]["content"]
                        .as_str()
                        .map(|s| s.contains("title for a coding session"))
                        .unwrap_or(false)
                    {
                        let out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            text("Test session"),
                            finish("stop", 10)
                        );
                        let _ = sock.write_all(out.as_bytes()).await;
                        return;
                    }
                    reqs.lock().push(body);
                    let next = queue.lock().pop_front();
                    match next {
                        Some(Resp::Sse(chunks)) => {
                            let mut out = String::from(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                            );
                            out.push_str(": OPENROUTER PROCESSING\n\n");
                            for c in chunks {
                                out.push_str(&format!("data: {c}\n\n"));
                            }
                            out.push_str("data: [DONE]\n\n");
                            let _ = sock.write_all(out.as_bytes()).await;
                        }
                        Some(Resp::Status(code, msg)) => {
                            let body = json!({"error": {"code": code, "message": msg}}).to_string();
                            let resp = format!(
                                "HTTP/1.1 {code} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                        None => {
                            let body = json!({"error": {"code": 500, "message": "mock: no more scripted responses"}}).to_string();
                            let resp = format!(
                                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });
        Mock {
            url: format!("http://{addr}"),
            requests,
        }
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().clone()
    }
}

pub async fn boot(
    root: &std::path::Path,
    mock: &Mock,
    mode: &str,
    tweak: impl FnOnce(&mut crate::config::Config),
) -> crate::app::Booted {
    env();
    let mut cfg = crate::config::Config {
        base_url: format!("{}/api/v1", mock.url),
        model: "test/model".into(),
        small_model: "test/model".into(),
        mode: mode.into(),
        notify: false,
        ..Default::default()
    };
    tweak(&mut cfg);
    let opts = crate::app::BootOptions {
        interactive: false,
        ..Default::default()
    };
    crate::app::boot_with(
        opts,
        root.to_path_buf(),
        root.to_path_buf(),
        cfg,
        "test-key".into(),
    )
    .await
    .unwrap()
}

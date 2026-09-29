//! Server-sent-events consumer for OpenAI-compatible streaming responses.

use super::{Completion, StreamApiError, StreamEvent, ToolCall, TransportError, Usage};
use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
pub(crate) struct Accumulator {
    pub c: Completion,
    calls: Vec<(usize, PartialCall)>,
    got_output: bool,
    done: bool,
}

impl Accumulator {
    fn call_slot(&mut self, index: usize, id: Option<&str>) -> usize {
        // Some providers reuse index 0 for several calls but give each a new id.
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            if let Some(pos) = self.calls.iter().position(|(_, c)| c.id == id) {
                return pos;
            }
            if let Some(pos) = self.calls.iter().position(|(i, _)| *i == index) {
                if self.calls[pos].1.id.is_empty() {
                    return pos;
                }
                // Same index, different id → a new call.
                let new_index = self.calls.iter().map(|(i, _)| *i).max().unwrap_or(0) + 1;
                self.calls.push((new_index, PartialCall::default()));
                return self.calls.len() - 1;
            }
        } else if let Some(pos) = self.calls.iter().rposition(|(i, _)| *i == index) {
            return pos;
        }
        self.calls.push((index, PartialCall::default()));
        self.calls.len() - 1
    }

    /// Feed one decoded SSE JSON payload.
    pub(crate) fn feed(
        &mut self,
        v: &Value,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<()> {
        if let Some(err) = v.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            let message = match err.pointer("/metadata/raw").and_then(|r| r.as_str()) {
                Some(raw) => format!("{message} ({})", crate::util::ellipsize(raw, 300)),
                None => message,
            };
            return Err(anyhow!(StreamApiError {
                code: err.get("code").and_then(|c| c.as_i64()),
                message,
                partial: self.got_output,
            }));
        }
        if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
            self.c.model = m.to_string();
        }
        if let Some(p) = v.get("provider").and_then(|m| m.as_str()) {
            self.c.provider = Some(p.to_string());
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.c.usage = parse_usage(u);
        }
        let Some(choice) = v.pointer("/choices/0") else {
            return Ok(());
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.c.finish_reason = Some(fr.to_string());
            if fr == "error" {
                let msg = choice
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("stream terminated with finish_reason=error");
                return Err(anyhow!(StreamApiError {
                    code: None,
                    message: msg.to_string(),
                    partial: self.got_output,
                }));
            }
        }
        // Non-streaming responses put everything in `message`.
        let delta = choice.get("delta").or_else(|| choice.get("message"));
        let Some(delta) = delta else { return Ok(()) };

        let mut reasoning_emitted = false;
        if let Some(r) = delta.get("reasoning").and_then(|r| r.as_str()) {
            if !r.is_empty() {
                self.got_output = true;
                self.c.reasoning.push_str(r);
                on_event(StreamEvent::Reasoning(r.to_string()));
                reasoning_emitted = true;
            }
        }
        if let Some(details) = delta.get("reasoning_details").and_then(|d| d.as_array()) {
            for d in details {
                let text = merge_reasoning_detail(&mut self.c.reasoning_details, d);
                if !reasoning_emitted && !text.is_empty() {
                    self.got_output = true;
                    self.c.reasoning.push_str(&text);
                    on_event(StreamEvent::Reasoning(text));
                }
            }
        }
        if let Some(t) = delta.get("content").and_then(|t| t.as_str()) {
            if !t.is_empty() {
                self.got_output = true;
                self.c.text.push_str(t);
                on_event(StreamEvent::Text(t.to_string()));
            }
        }
        if let Some(anns) = delta.get("annotations").and_then(|a| a.as_array()) {
            self.c.annotations.extend(anns.iter().cloned());
        }
        if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for (n, tc) in calls.iter().enumerate() {
                let index = tc
                    .get("index")
                    .and_then(|i| i.as_u64())
                    .map(|i| i as usize)
                    .unwrap_or(n);
                let id = tc.get("id").and_then(|i| i.as_str());
                let slot = self.call_slot(index, id);
                let pc = &mut self.calls[slot].1;
                if let Some(id) = id.filter(|s| !s.is_empty()) {
                    pc.id = id.to_string();
                }
                if let Some(f) = tc.get("function") {
                    if let Some(name) = f.get("name").and_then(|n| n.as_str()) {
                        if !name.is_empty() && pc.name.is_empty() {
                            pc.name = name.to_string();
                            self.got_output = true;
                            on_event(StreamEvent::ToolCallStart(slot, name.to_string()));
                        } else if !name.is_empty() && pc.name != name && pc.args.is_empty() {
                            pc.name.push_str(name);
                        }
                    }
                    match f.get("arguments") {
                        Some(Value::String(a)) => pc.args.push_str(a),
                        // Some providers send already-parsed objects.
                        Some(obj @ Value::Object(_)) => pc.args.push_str(&obj.to_string()),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Completion {
        self.calls.sort_by_key(|(i, _)| *i);
        for (_, pc) in self.calls {
            if pc.name.is_empty() {
                continue;
            }
            let id = if pc.id.is_empty() {
                format!("call_{}", crate::util::short_id())
            } else {
                pc.id
            };
            self.c.tool_calls.push(ToolCall {
                id,
                name: pc.name,
                arguments: pc.args,
            });
        }
        self.c
    }
}

/// Merge a streamed `reasoning_details` fragment into the accumulated list.
/// Returns any human-readable text contained in the fragment.
pub(crate) fn merge_reasoning_detail(acc: &mut Vec<Value>, d: &Value) -> String {
    let ty = d
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let idx = d.get("index").and_then(|i| i.as_u64());
    let text = match ty.as_str() {
        "reasoning.text" => d.get("text").and_then(|t| t.as_str()).unwrap_or(""),
        "reasoning.summary" => d.get("summary").and_then(|t| t.as_str()).unwrap_or(""),
        _ => "",
    }
    .to_string();
    let existing = acc.iter_mut().rev().find(|e| {
        e.get("type").and_then(|t| t.as_str()) == Some(ty.as_str())
            && e.get("index").and_then(|i| i.as_u64()) == idx
    });
    match existing {
        Some(e) if idx.is_some() => {
            let eo = e.as_object_mut().unwrap();
            if let Some(src) = d.as_object() {
                for (k, v) in src {
                    match (k.as_str(), v) {
                        ("text" | "summary" | "data", Value::String(s)) => {
                            let cur = eo.entry(k.clone()).or_insert(Value::String(String::new()));
                            if let Value::String(c) = cur {
                                c.push_str(s);
                            }
                        }
                        (_, Value::Null) => {}
                        _ => {
                            eo.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
        _ => acc.push(d.clone()),
    }
    text
}

pub fn parse_usage(u: &Value) -> Usage {
    let g = |p: &str| u.pointer(p).and_then(|x| x.as_u64()).unwrap_or(0);
    Usage {
        prompt_tokens: g("/prompt_tokens"),
        completion_tokens: g("/completion_tokens"),
        cached_tokens: g("/prompt_tokens_details/cached_tokens"),
        cache_write_tokens: g("/prompt_tokens_details/cache_write_tokens"),
        reasoning_tokens: g("/completion_tokens_details/reasoning_tokens"),
        cost: u.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0),
    }
}

/// Incremental SSE line decoder.
#[derive(Default)]
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
    data: String,
}

impl SseDecoder {
    /// Push bytes; returns complete event payloads (`data:` joined).
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data));
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // keep-alive comment (": OPENROUTER PROCESSING")
            }
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest);
            }
            // `event:`, `id:`, `retry:` are ignored.
        }
        out
    }

    pub(crate) fn flush(&mut self) -> Option<String> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).to_string();
            self.buf.clear();
            if let Some(rest) = line.trim_end().strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest.trim_start());
            }
        }
        if self.data.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.data))
        }
    }
}

pub(crate) async fn consume(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<Completion> {
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.contains("event-stream"))
        .unwrap_or(true);
    let mut acc = Accumulator::default();
    if !is_sse {
        // Some gateways ignore `stream: true`; accept a plain JSON body.
        let v: Value = resp.json().await.map_err(|e| {
            anyhow!(TransportError {
                msg: e.to_string(),
                partial: false
            })
        })?;
        acc.feed(&v, on_event)?;
        return Ok(acc.finish());
    }
    let mut dec = SseDecoder::default();
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::select! {
            n = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => n,
            _ = cancel.cancelled() => bail!("interrupted"),
        };
        let chunk = match next {
            Err(_) => {
                return Err(anyhow!(TransportError {
                    msg: format!("no data for {}s", IDLE_TIMEOUT.as_secs()),
                    partial: acc.got_output,
                }));
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                return Err(anyhow!(TransportError {
                    msg: e.to_string(),
                    partial: acc.got_output
                }));
            }
            Ok(Some(Ok(b))) => b,
        };
        for payload in dec.push(&chunk) {
            if payload.trim() == "[DONE]" {
                acc.done = true;
                continue;
            }
            match serde_json::from_str::<Value>(&payload) {
                Ok(v) => acc.feed(&v, on_event)?,
                Err(_) => continue, // tolerate junk lines
            }
        }
    }
    if let Some(payload) = dec.flush() {
        if payload.trim() == "[DONE]" {
            acc.done = true;
        } else if let Ok(v) = serde_json::from_str::<Value>(&payload) {
            acc.feed(&v, on_event)?;
        }
    }
    if !acc.done && acc.c.finish_reason.is_none() {
        return Err(anyhow!(TransportError {
            msg: "stream ended unexpectedly".into(),
            partial: acc.got_output,
        }));
    }
    Ok(acc.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sse_decoder_handles_split_chunks_and_comments() {
        let mut d = SseDecoder::default();
        let mut out = d.push(b": OPENROUTER PROCESSING\n\ndata: {\"a\"");
        assert!(out.is_empty());
        out.extend(d.push(b":1}\r\n\r\ndata: [DONE]\n\n"));
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
    }

    #[test]
    fn accumulates_tool_calls_and_reasoning() {
        let mut acc = Accumulator::default();
        let mut events = vec![];
        let mut cb = |e: StreamEvent| events.push(format!("{e:?}"));
        let chunks = vec![
            json!({"choices":[{"delta":{"reasoning":"think ","reasoning_details":[{"type":"reasoning.text","text":"think ","index":0,"format":"anthropic-claude-v1"}]}}]}),
            json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"more","index":0,"signature":"sig"}], "reasoning":"more"}}]}),
            json!({"choices":[{"delta":{"content":"Hello"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"grep","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"cost":0.001,"prompt_tokens_details":{"cached_tokens":80}}}),
        ];
        for c in &chunks {
            acc.feed(c, &mut cb).unwrap();
        }
        let c = acc.finish();
        assert_eq!(c.text, "Hello");
        assert_eq!(c.reasoning, "think more");
        assert_eq!(c.reasoning_details.len(), 1);
        assert_eq!(c.reasoning_details[0]["text"], "think more");
        assert_eq!(c.reasoning_details[0]["signature"], "sig");
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[0].arguments, "{\"path\":\"a\"}");
        assert_eq!(c.tool_calls[1].name, "grep");
        assert_eq!(c.usage.cached_tokens, 80);
        assert_eq!(c.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn same_index_different_ids_are_separate_calls() {
        let mut acc = Accumulator::default();
        let mut cb = |_e: StreamEvent| {};
        acc.feed(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{}"}}]}}]}), &mut cb).unwrap();
        acc.feed(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"b","function":{"name":"ls","arguments":"{}"}}]}}]}), &mut cb).unwrap();
        let c = acc.finish();
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[1].name, "ls");
    }

    #[test]
    fn stream_error_chunk_is_error() {
        let mut acc = Accumulator::default();
        let mut cb = |_e: StreamEvent| {};
        let r = acc.feed(&json!({"error":{"code":502,"message":"upstream"}}), &mut cb);
        assert!(r.is_err());
    }
}

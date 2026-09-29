//! Web tools. Search runs through OpenRouter's web plugin, so no extra API key
//! is needed; fetch converts HTML to Markdown and can digest long pages with
//! the small model.

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_opt_str, arg_str, arg_u64};
use crate::agent::events::AgentEvent;
use crate::llm::ChatRequest;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

const MAX_FETCH_BYTES: usize = 6 * 1024 * 1024;
const MAX_RETURN_CHARS: usize = 40_000;

pub struct FetchTool;

pub fn html_to_markdown(html: &str) -> String {
    let conv = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "noscript", "svg", "iframe", "nav", "footer", "head",
        ])
        .build();
    let md = conv.convert(html).unwrap_or_else(|_| html.to_string());
    // Collapse runs of blank lines.
    let mut out = String::with_capacity(md.len());
    let mut blank = 0;
    for line in md.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

async fn fetch_text(ctx: &ToolCtx, url: &str) -> Result<(String, String), String> {
    let client = ctx.shared.client.http().clone();
    let req = client
        .get(url)
        .timeout(Duration::from_secs(40))
        .header(
            "User-Agent",
            "Mozilla/5.0 (compatible; leme/0.1; +https://github.com/JaimeJunr/Leme-Agent)",
        )
        .header(
            "Accept",
            "text/markdown, text/html;q=0.9, text/plain;q=0.8, application/json;q=0.8, */*;q=0.5",
        );
    let resp = tokio::select! {
        r = req.send() => r.map_err(|e| format!("request failed: {e}"))?,
        _ = ctx.cancel.cancelled() => return Err("interrupted".into()),
    };
    let status = resp.status();
    let final_url = resp.url().to_string();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("reading body: {e}"))?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(2000)]).to_string();
        return Err(format!("HTTP {status} for {final_url}\n{body}"));
    }
    if bytes.len() > MAX_FETCH_BYTES {
        return Err(format!("response too large ({} bytes)", bytes.len()));
    }
    if ctype.contains("pdf") || ctype.starts_with("image/") || ctype.contains("octet-stream") {
        return Err(format!("unsupported content type `{ctype}` (binary)"));
    }
    let text = String::from_utf8_lossy(&bytes).to_string();
    let text = if ctype.contains("html")
        || text.trim_start().starts_with("<!")
        || text.trim_start().starts_with("<html")
    {
        html_to_markdown(&text)
    } else {
        text
    };
    Ok((final_url, text))
}

#[async_trait]
impl Tool for FetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> String {
        "Fetch a URL and return its content as Markdown (HTML is converted). For long pages pass `prompt` — a \
question or extraction instruction — and a fast model will answer it from the page instead of returning the \
whole page. Use for documentation, issues, API references. Read-only."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "prompt": {"type": "string", "description": "What to extract from the page (optional)"}
            },
            "required": ["url"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Network
    }
    fn summarize(&self, args: &Value) -> String {
        arg_opt_str(args, "url").unwrap_or("?").to_string()
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let url = match arg_str(&args, "url") {
            Ok(u) => u.trim().to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return ToolOutput::err("url must start with http:// or https://");
        }
        let (final_url, text) = match fetch_text(ctx, &url).await {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let chars = text.chars().count();
        if let Some(prompt) = arg_opt_str(&args, "prompt")
            && chars > 6000
        {
            let cfg = ctx.shared.cfg();
            let page = crate::util::truncate_bytes(&text, 400_000);
            let req = ChatRequest {
                model: cfg.small_model.clone(),
                messages: vec![
                    json!({"role": "system", "content": "You extract information from web pages. Answer only from the page content. Quote code, commands, versions and API signatures exactly. Be concise but complete. If the page does not contain the answer, say so."}),
                    json!({"role": "user", "content": format!("Page: {final_url}\n\n<page>\n{page}\n</page>\n\nTask: {prompt}")}),
                ],
                max_tokens: Some(4000),
                ..Default::default()
            };
            match ctx.shared.client.complete(&req, &ctx.cancel).await {
                Ok(c) => {
                    ctx.events.send(AgentEvent::Notice(format!(
                        "web_fetch digest via {} ({})",
                        cfg.small_model,
                        crate::util::fmt_cost(c.usage.cost)
                    )));
                    return ToolOutput::ok(format!(
                        "Answer extracted from {final_url} ({chars} chars) for: {prompt}\n\n{}",
                        c.text
                    ))
                    .with_summary(format!("{chars} chars → digest"));
                }
                Err(e) => {
                    ctx.events.send(AgentEvent::Warning(format!(
                        "digest failed ({e}); returning raw page"
                    )));
                }
            }
        }
        let mut out = format!("Content of {final_url}:\n\n");
        if chars > MAX_RETURN_CHARS {
            let cut: String = text.chars().take(MAX_RETURN_CHARS).collect();
            out.push_str(&cut);
            if let Some(p) = ctx.shared.spill("web", &text) {
                out.push_str(&format!("\n\n[truncated: {chars} chars total; full page saved to {} — use read with offset, or re-fetch with a `prompt`]", p.display()));
            }
        } else {
            out.push_str(&text);
        }
        ToolOutput::ok(out).with_summary(format!("{chars} chars"))
    }
}

pub struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> String {
        "Search the web (via OpenRouter's web plugin) and get a sourced summary. Use for recent information: \
library versions, error messages, changelogs, docs you don't know. Follow up with web_fetch on the best source \
when you need details."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "description": "1-10 (default 5)"}
            },
            "required": ["query"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Network
    }
    fn summarize(&self, args: &Value) -> String {
        format!("\"{}\"", arg_opt_str(args, "query").unwrap_or("?"))
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let query = match arg_str(&args, "query") {
            Ok(q) => q.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let n = arg_u64(&args, "max_results").unwrap_or(5).clamp(1, 10);
        let cfg = ctx.shared.cfg();
        let today = chrono::Local::now().format("%Y-%m-%d");
        let req = ChatRequest {
            model: cfg.small_model.clone(),
            messages: vec![
                json!({"role": "system", "content": format!("You are a web research assistant. Today is {today}. Using the search results, write a factual, dense summary answering the query. Include concrete details (versions, commands, dates, code). Cite sources inline as [n] and end with a numbered list of source URLs.")}),
                json!({"role": "user", "content": query}),
            ],
            plugins: Some(json!([{"id": "web", "max_results": n}])),
            max_tokens: Some(3000),
            ..Default::default()
        };
        match ctx.shared.client.complete(&req, &ctx.cancel).await {
            Ok(c) => {
                let mut out = c.text.clone();
                let mut urls = vec![];
                for a in &c.annotations {
                    if let Some(u) = a.pointer("/url_citation/url").and_then(|u| u.as_str()) {
                        let title = a
                            .pointer("/url_citation/title")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        if !urls.iter().any(|(x, _): &(String, String)| x == u) {
                            urls.push((u.to_string(), title.to_string()));
                        }
                    }
                }
                if !urls.is_empty() && !urls.iter().all(|(u, _)| out.contains(u.as_str())) {
                    out.push_str("\n\nSources:\n");
                    for (i, (u, t)) in urls.iter().enumerate() {
                        out.push_str(&format!("[{}] {} {}\n", i + 1, t, u));
                    }
                }
                if out.trim().is_empty() {
                    return ToolOutput::err("search returned no content");
                }
                ToolOutput::ok(out).with_summary(format!(
                    "{} sources · {}",
                    urls.len(),
                    crate::util::fmt_cost(c.usage.cost)
                ))
            }
            Err(e) => ToolOutput::err(format!("web search failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn html_conversion_strips_scripts() {
        let md = super::html_to_markdown(
            "<html><head><title>x</title></head><body><script>bad()</script><h1>Title</h1><p>Hello <b>world</b></p></body></html>",
        );
        assert!(md.contains("# Title"));
        assert!(md.contains("**world**"));
        assert!(!md.contains("bad()"));
    }
}

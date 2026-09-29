//! Model Context Protocol client: stdio and streamable-HTTP transports.

use crate::config::McpServerConfig;
use crate::conversation::Image;
use crate::tools::{Tool, ToolCtx, ToolKind, ToolOutput};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

const PROTOCOL_VERSION: &str = "2025-06-18";

enum Transport {
    Stdio {
        stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
        pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
        _child: tokio::sync::Mutex<tokio::process::Child>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
        session: Mutex<Option<String>>,
        client: reqwest::Client,
    },
}

pub struct McpServer {
    pub name: String,
    transport: Transport,
    next_id: AtomicU64,
    pub instructions: Option<String>,
}

#[derive(Clone, Debug)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl McpServer {
    async fn start(name: &str, cfg: &McpServerConfig, root: &std::path::Path) -> Result<McpServer> {
        let transport = if let Some(url) = &cfg.url {
            Transport::Http {
                url: url.clone(),
                headers: cfg
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), expand_env(v)))
                    .collect(),
                session: Mutex::new(None),
                client: reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(15))
                    .build()?,
            }
        } else {
            let command = cfg
                .command
                .as_ref()
                .context("MCP server needs `command` or `url`")?;
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(&cfg.args)
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            for (k, v) in &cfg.env {
                cmd.env(k, expand_env(v));
            }
            let mut child = cmd
                .spawn()
                .with_context(|| format!("starting `{command}`"))?;
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let p2 = pending.clone();
            let root_uri = format!("file://{}", root.display());
            let (resp_tx, mut resp_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if v.get("method").is_some() && v.get("id").is_some() {
                        // Server → client request (roots/list, ping, …).
                        let id = v["id"].clone();
                        let result = match v["method"].as_str() {
                            Some("roots/list") => {
                                json!({"roots": [{"uri": root_uri, "name": "workspace"}]})
                            }
                            Some("ping") => json!({}),
                            _ => {
                                let _ = resp_tx.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported"}}));
                                continue;
                            }
                        };
                        let _ = resp_tx.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
                        continue;
                    }
                    if let Some(id) = v.get("id").and_then(|i| i.as_u64())
                        && let Some(tx) = p2.lock().remove(&id)
                    {
                        let _ = tx.send(v);
                    }
                }
            });
            let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
            let reply_stdin = stdin.clone();
            // Replies to server-initiated requests.
            tokio::spawn(async move {
                while let Some(msg) = resp_rx.recv().await {
                    let mut line = msg.to_string();
                    line.push('\n');
                    let mut s = reply_stdin.lock().await;
                    if s.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = s.flush().await;
                }
            });
            Transport::Stdio {
                stdin,
                pending,
                _child: tokio::sync::Mutex::new(child),
            }
        };
        let mut server = McpServer {
            name: name.to_string(),
            transport,
            next_id: AtomicU64::new(1),
            instructions: None,
        };
        let init = server
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"roots": {"listChanged": false}},
                    "clientInfo": {"name": "harness", "version": env!("CARGO_PKG_VERSION")}
                }),
                Duration::from_secs(30),
            )
            .await?;
        server
            .notify("notifications/initialized", json!({}))
            .await?;
        server.instructions = init
            .get("instructions")
            .and_then(|i| i.as_str())
            .map(String::from);
        Ok(server)
    }

    async fn write_line(&self, msg: &Value) -> Result<()> {
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut s = stdin.lock().await;
                let mut line = msg.to_string();
                line.push('\n');
                s.write_all(line.as_bytes()).await?;
                s.flush().await?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &self.transport {
            Transport::Stdio { .. } => self.write_line(&msg).await,
            Transport::Http { .. } => {
                let _ = self.http_post(&msg, None).await;
                Ok(())
            }
        }
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = match &self.transport {
            Transport::Stdio { pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.lock().insert(id, tx);
                self.write_line(&msg).await?;
                match tokio::time::timeout(timeout, rx).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(_)) => bail!("MCP server `{}` closed the connection", self.name),
                    Err(_) => {
                        pending.lock().remove(&id);
                        bail!("MCP request `{method}` timed out")
                    }
                }
            }
            Transport::Http { .. } => tokio::time::timeout(timeout, self.http_post(&msg, Some(id)))
                .await
                .map_err(|_| anyhow!("MCP request `{method}` timed out"))??,
        };
        if let Some(err) = resp.get("error") {
            bail!(
                "{}",
                err.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("MCP error")
            );
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn http_post(&self, msg: &Value, id: Option<u64>) -> Result<Value> {
        let Transport::Http {
            url,
            headers,
            session,
            client,
        } = &self.transport
        else {
            bail!("not http")
        };
        let mut req = client
            .post(url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(msg);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(s) = session.lock().clone() {
            req = req.header("Mcp-Session-Id", s);
        }
        let resp = req.send().await?;
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|h| h.to_str().ok())
        {
            *session.lock() = Some(s.to_string());
        }
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("MCP HTTP {status}: {}", crate::util::ellipsize(&body, 300));
        }
        let Some(id) = id else { return Ok(Value::Null) };
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await?;
        if ctype.contains("event-stream") {
            for block in body.split("\n\n") {
                let data: String = block
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|l| l.trim_start())
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Ok(v) = serde_json::from_str::<Value>(&data)
                    && v.get("id").and_then(|i| i.as_u64()) == Some(id)
                {
                    return Ok(v);
                }
            }
            bail!("no response in MCP event stream")
        } else {
            Ok(serde_json::from_str(&body)?)
        }
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let mut out = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self
                .request("tools/list", params, Duration::from_secs(30))
                .await?;
            for t in r
                .get("tools")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default()
            {
                let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                out.push(McpToolInfo {
                    server: self.name.clone(),
                    name: name.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(json!({"type": "object", "properties": {}})),
                });
            }
            cursor = r
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

pub struct McpManager {
    pub servers: Vec<Arc<McpServer>>,
    pub tools: Vec<McpToolInfo>,
    pub errors: Vec<String>,
}

impl McpManager {
    pub async fn start(
        cfgs: &std::collections::BTreeMap<String, McpServerConfig>,
        root: &std::path::Path,
    ) -> McpManager {
        let mut futs = vec![];
        for (name, cfg) in cfgs {
            if cfg.enabled == Some(false) {
                continue;
            }
            let name = name.clone();
            let cfg = cfg.clone();
            let root = root.to_path_buf();
            futs.push(async move {
                let r = tokio::time::timeout(Duration::from_secs(45), async {
                    let s = McpServer::start(&name, &cfg, &root).await?;
                    let tools = s.list_tools().await?;
                    Ok::<_, anyhow::Error>((s, tools))
                })
                .await;
                (name, r)
            });
        }
        let results = futures_util::future::join_all(futs).await;
        let mut m = McpManager {
            servers: vec![],
            tools: vec![],
            errors: vec![],
        };
        for (name, r) in results {
            match r {
                Ok(Ok((s, tools))) => {
                    m.servers.push(Arc::new(s));
                    m.tools.extend(tools);
                }
                Ok(Err(e)) => m.errors.push(format!("MCP `{name}`: {e:#}")),
                Err(_) => m.errors.push(format!("MCP `{name}`: startup timed out")),
            }
        }
        m
    }

    pub fn server(&self, name: &str) -> Option<Arc<McpServer>> {
        self.servers.iter().find(|s| s.name == name).cloned()
    }

    pub fn tool_objects(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        self.tools
            .iter()
            .map(|t| {
                Arc::new(McpTool {
                    info: t.clone(),
                    qualified: qualified_name(&t.server, &t.name),
                    mgr: self.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }
}

pub fn qualified_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let mut n = format!("mcp__{}__{}", clean(server), clean(tool));
    n.truncate(64);
    n
}

fn expand_env(v: &str) -> String {
    let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").unwrap();
    re.replace_all(v, |c: &regex::Captures| {
        std::env::var(&c[1]).unwrap_or_default()
    })
    .to_string()
}

pub struct McpTool {
    info: McpToolInfo,
    qualified: String,
    mgr: Arc<McpManager>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }
    fn description(&self) -> String {
        format!(
            "[MCP server `{}`] {}",
            self.info.server,
            crate::util::ellipsize(&self.info.description, 2000)
        )
    }
    fn schema(&self) -> Value {
        let mut s = self.info.schema.clone();
        if s.get("type").is_none() {
            s["type"] = json!("object");
        }
        s
    }
    fn kind(&self) -> ToolKind {
        ToolKind::External
    }
    fn summarize(&self, args: &Value) -> String {
        crate::util::ellipsize(&args.to_string(), 120)
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let Some(server) = self.mgr.server(&self.info.server) else {
            return ToolOutput::err("MCP server not running");
        };
        let fut = server.request(
            "tools/call",
            json!({"name": self.info.name, "arguments": args}),
            Duration::from_secs(600),
        );
        let r = tokio::select! {
            r = fut => r,
            _ = ctx.cancel.cancelled() => return ToolOutput::err("interrupted"),
        };
        match r {
            Ok(res) => {
                let mut text = String::new();
                let mut images = vec![];
                for part in res
                    .get("content")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    match part.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            text.push_str(part.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                            text.push('\n');
                        }
                        Some("image") => images.push(Image {
                            mime: part
                                .get("mimeType")
                                .and_then(|m| m.as_str())
                                .unwrap_or("image/png")
                                .to_string(),
                            data: part
                                .get("data")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                            label: self.qualified.clone(),
                        }),
                        Some("resource") => {
                            if let Some(t) = part.pointer("/resource/text").and_then(|t| t.as_str())
                            {
                                text.push_str(t);
                                text.push('\n');
                            }
                        }
                        _ => {
                            text.push_str(&part.to_string());
                            text.push('\n');
                        }
                    }
                }
                if let Some(sc) = res.get("structuredContent")
                    && text.trim().is_empty()
                {
                    text = serde_json::to_string_pretty(sc).unwrap_or_default();
                }
                let (body, truncated) = crate::util::head_tail(&text, 2000, 60_000);
                let mut body = body;
                if truncated && let Some(p) = ctx.shared.spill("mcp", &text) {
                    body.push_str(&format!("\n[full output saved to {}]", p.display()));
                }
                let is_error = res
                    .get("isError")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false);
                ToolOutput {
                    content: body,
                    images,
                    is_error,
                    summary: if is_error {
                        "error".into()
                    } else {
                        "ok".into()
                    },
                    display: None,
                }
            }
            Err(e) => ToolOutput::err(format!("MCP call failed: {e}")),
        }
    }
}

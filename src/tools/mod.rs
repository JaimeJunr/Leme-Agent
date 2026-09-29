//! Tool framework and registry.

pub mod ask;
pub mod bash;
pub mod consult;
pub mod edit_match;
pub mod fs;
pub mod grep;
pub mod patch;
pub mod plan;
pub mod skill;
pub mod task;
pub mod todo;
pub mod web;

use crate::agent::events::{AgentEvent, EventSink};
use crate::agent::shared::Shared;
use crate::conversation::Image;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// No side effects outside the harness (read, grep, todo…).
    Read,
    /// Modifies files in the workspace.
    Edit,
    /// Executes arbitrary commands.
    Exec,
    /// Talks to the network (fetch/search) — read-only but external.
    Network,
    /// External MCP tool with unknown effects.
    External,
}

impl ToolKind {
    /// Safe to run concurrently with other calls of the same batch.
    pub fn parallel_safe(self) -> bool {
        matches!(self, ToolKind::Read | ToolKind::Network)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    pub images: Vec<Image>,
    pub is_error: bool,
    /// Optional rich display for the UI (e.g. a unified diff).
    pub display: Option<Display>,
    /// Short one-line result summary for the UI ("42 lines", "exit 1").
    pub summary: String,
}

#[derive(Debug, Clone)]
pub enum Display {
    Diff { path: String, diff: String },
    Text(String),
    Todos(Vec<todo::TodoItem>),
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            ..Default::default()
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        let c = content.into();
        ToolOutput {
            summary: crate::util::first_line(&c, 80),
            content: c,
            is_error: true,
            ..Default::default()
        }
    }
    pub fn with_summary(mut self, s: impl Into<String>) -> Self {
        self.summary = s.into();
        self
    }
    pub fn with_display(mut self, d: Display) -> Self {
        self.display = Some(d);
        self
    }
}

pub struct ToolCtx {
    pub shared: Arc<Shared>,
    pub call_id: String,
    pub cancel: CancellationToken,
    pub events: EventSink,
    /// Run shell commands inside the OS sandbox.
    pub sandbox: bool,
    /// Nesting depth (0 = main agent).
    pub depth: u32,
}

impl ToolCtx {
    pub fn progress(&self, line: impl Into<String>) {
        self.events.send(AgentEvent::ToolProgress {
            id: self.call_id.clone(),
            line: line.into(),
        });
    }
    pub fn root(&self) -> &std::path::Path {
        &self.shared.root
    }
    pub fn cwd(&self) -> std::path::PathBuf {
        self.shared.cwd.lock().clone()
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    fn kind(&self) -> ToolKind;
    /// One-line human summary of a call (used in UI and approval prompts).
    fn summarize(&self, args: &Value) -> String {
        let _ = args;
        String::new()
    }
    /// Optional preview (e.g. diff) shown in approval prompts.
    fn preview(&self, ctx: &ToolCtx, args: &Value) -> Option<String> {
        let _ = (ctx, args);
        None
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput;
}

pub fn definition(t: &dyn Tool) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": t.name(),
            "description": t.description(),
            "parameters": t.schema(),
        }
    })
}

#[derive(Clone, Default)]
pub struct Registry {
    pub tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .cloned()
            .or_else(|| {
                // Tolerate case / prefix mistakes from weaker models.
                let lower = name.to_lowercase();
                let lower = lower
                    .trim_start_matches("functions.")
                    .trim_start_matches("tools.");
                self.tools
                    .iter()
                    .find(|t| t.name().eq_ignore_ascii_case(lower))
                    .cloned()
            })
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools.iter().map(|t| definition(t.as_ref())).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    pub fn retain(&mut self, f: impl Fn(&str) -> bool) {
        self.tools.retain(|t| f(t.name()));
    }
}

/// Which edit tools to expose for a model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditFormat {
    Replace,
    Patch,
}

pub fn edit_format_for(cfg_value: &str, model: &str) -> EditFormat {
    match cfg_value {
        "patch" => EditFormat::Patch,
        "replace" => EditFormat::Replace,
        _ => {
            // OpenAI models are trained on the apply_patch format.
            if model.starts_with("openai/") && !model.contains("oss") {
                EditFormat::Patch
            } else {
                EditFormat::Replace
            }
        }
    }
}

pub struct RegistryOptions {
    pub edit_format: EditFormat,
    /// Include the `task` (subagent) tool.
    pub subagents: bool,
    pub interactive: bool,
    pub plan_mode: bool,
    pub disabled: Vec<String>,
}

/// Build the built-in tool set.
pub fn builtin(opts: &RegistryOptions) -> Registry {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(fs::ReadTool),
        Arc::new(fs::LsTool),
        Arc::new(fs::GlobTool),
        Arc::new(grep::GrepTool),
    ];
    if !opts.plan_mode {
        match opts.edit_format {
            EditFormat::Replace => {
                tools.push(Arc::new(fs::EditTool));
                tools.push(Arc::new(fs::MultiEditTool));
                tools.push(Arc::new(fs::WriteTool));
            }
            EditFormat::Patch => {
                tools.push(Arc::new(patch::ApplyPatchTool));
                tools.push(Arc::new(fs::WriteTool));
            }
        }
    }
    tools.push(Arc::new(bash::BashTool));
    tools.push(Arc::new(bash::JobsTool));
    tools.push(Arc::new(web::FetchTool));
    tools.push(Arc::new(web::SearchTool));
    tools.push(Arc::new(todo::TodoTool));
    tools.push(Arc::new(consult::ConsultTool));
    tools.push(Arc::new(skill::SkillTool));
    if opts.subagents {
        tools.push(Arc::new(task::TaskTool));
    }
    if opts.interactive {
        tools.push(Arc::new(ask::AskTool));
    }
    if opts.plan_mode {
        tools.push(Arc::new(plan::ProposePlanTool));
    }
    let mut r = Registry { tools };
    r.retain(|n| !opts.disabled.iter().any(|d| d == n));
    r
}

/// Helper: fetch a required string argument.
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(Value::Null) | None => Err(format!("missing required parameter `{key}`")),
        Some(other) => Err(format!("parameter `{key}` must be a string, got {other}")),
    }
}

pub fn arg_opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

pub fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn arg_bool(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

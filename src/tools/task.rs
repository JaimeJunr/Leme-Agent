//! `task`: delegate work to a subagent with a fresh context window. Several
//! task calls in one response run in parallel.

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_opt_str, arg_str};
use crate::agent::events::StopReason;
use crate::agent::{Agent, UserInput};
use crate::conversation::Item;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> String {
        "Launch a subagent to handle a task autonomously in its own context window, and get its final report back. \
Use it to keep your context clean (broad searches, investigating unfamiliar code, reading many files) or to \
run independent workstreams in parallel (call `task` several times in one response). The subagent cannot see \
this conversation: the `prompt` must be complete — goal, relevant paths, constraints and exactly what to report \
back. Available agent types are listed in the system prompt (default: general). Its report is not shown to \
the user; summarize what matters."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {"type": "string", "description": "3-6 word label for the task"},
                "prompt": {"type": "string", "description": "Complete instructions for the subagent"},
                "agent": {"type": "string", "description": "Agent type (e.g. explore, general)"},
                "model": {"type": "string", "description": "Optional OpenRouter model override"}
            },
            "required": ["description", "prompt"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        format!(
            "{} · {}",
            arg_opt_str(args, "agent").unwrap_or("general"),
            arg_opt_str(args, "description").unwrap_or("task")
        )
    }
    async fn run(&self, _ctx: &ToolCtx, _args: Value) -> ToolOutput {
        // Executed by the agent loop via `run_task` (needs the parent agent).
        ToolOutput::err("task must be run by the agent loop")
    }
}

pub fn resolve_model(
    alias: Option<&str>,
    parent_model: &str,
    cfg: &crate::config::Config,
) -> String {
    match alias.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None | Some("main") | Some("inherit") => parent_model.to_string(),
        Some("small") | Some("fast") => cfg.small_model.clone(),
        Some("oracle") | Some("expert") => cfg.oracle_model.clone(),
        Some(m) => m.to_string(),
    }
}

pub fn run_task<'a>(
    parent: &'a Agent,
    args: &'a Value,
    call_id: &'a str,
    cancel: &'a CancellationToken,
) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
    Box::pin(async move {
        let prompt = match arg_str(args, "prompt") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let name = arg_opt_str(args, "agent")
            .or_else(|| arg_opt_str(args, "subagent_type"))
            .unwrap_or("general");
        let Some(def) = parent.shared.ext.agent(name).cloned() else {
            let names: Vec<String> = parent
                .shared
                .ext
                .agents
                .iter()
                .map(|a| a.name.clone())
                .collect();
            return ToolOutput::err(format!(
                "unknown agent `{name}`. Available: {}",
                names.join(", ")
            ));
        };
        let cfg = parent.shared.cfg();
        let model = resolve_model(
            arg_opt_str(args, "model").or(def.model.as_deref()),
            &parent.model,
            &cfg,
        );
        let label = arg_opt_str(args, "description")
            .unwrap_or(&def.name)
            .to_string();
        let events = parent.events.sub(call_id, &label);
        let mut sub = Agent::sub(parent, def.clone(), model.clone(), events);
        let child_cancel = cancel.child_token();
        let started = std::time::Instant::now();
        let fut: Pin<Box<dyn Future<Output = StopReason> + Send + '_>> =
            Box::pin(sub.run_turn(UserInput::from(prompt.as_str()), child_cancel));
        let reason = fut.await;
        let report = sub
            .items
            .iter()
            .rev()
            .find_map(|i| match i {
                Item::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let tool_calls = sub
            .items
            .iter()
            .filter(|i| matches!(i, Item::Tool { .. }))
            .count();
        let meta = format!(
            "{} · {} tool calls · {} · {}",
            model,
            tool_calls,
            crate::util::fmt_duration(started.elapsed()),
            crate::util::fmt_cost(sub.usage.cost)
        );
        match reason {
            StopReason::Done => ToolOutput::ok(format!(
                "Report from subagent `{}` ({meta}):\n\n{report}",
                def.name
            ))
            .with_summary(meta),
            StopReason::Interrupted => ToolOutput::err("Subagent interrupted by the user."),
            other => ToolOutput {
                content: format!(
                    "Subagent `{}` stopped early ({other:?}). Partial report:\n\n{}",
                    def.name,
                    if report.is_empty() {
                        "(none)".into()
                    } else {
                        report
                    }
                ),
                is_error: true,
                summary: format!("stopped: {other:?}"),
                ..Default::default()
            },
        }
    })
}

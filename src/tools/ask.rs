//! `ask_user`: let the model ask a clarifying question mid-task.

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_str};
use crate::agent::events::{AgentEvent, AskRequest};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct AskTool;

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> String {
        "Ask the user a question and wait for the answer. Use only when a decision genuinely needs their input \
(ambiguous requirements, choosing between materially different approaches, missing credentials). Offer \
short `options` when possible. Do not ask for permission to run tools — approvals are handled for you."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string"},
                "options": {"type": "array", "items": {"type": "string"}, "description": "Suggested answers (2-5)"}
            },
            "required": ["question"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        crate::util::first_line(
            args.get("question").and_then(|q| q.as_str()).unwrap_or("?"),
            100,
        )
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let question = match arg_str(&args, "question") {
            Ok(q) => q.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let options: Vec<String> = args
            .get("options")
            .and_then(|o| o.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.events.send(AgentEvent::Ask(AskRequest {
            question,
            options,
            reply: tx,
        }));
        tokio::select! {
            r = rx => match r {
                Ok(answer) if !answer.trim().is_empty() => ToolOutput::ok(format!("User answered: {answer}")).with_summary(crate::util::first_line(&answer, 60)),
                _ => ToolOutput::ok("The user did not answer. Proceed with your best judgement and state your assumption."),
            },
            _ = ctx.cancel.cancelled() => ToolOutput::err("interrupted"),
        }
    }
}

//! Plan mode: the model investigates read-only and proposes a plan; the user
//! approves it (switching to an editing mode) or asks for revisions.

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_str};
use crate::agent::events::{AgentEvent, PlanDecision, PlanRequest};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct ProposePlanTool;

#[async_trait]
impl Tool for ProposePlanTool {
    fn name(&self) -> &str {
        "propose_plan"
    }
    fn description(&self) -> String {
        "Present your implementation plan to the user for approval (plan mode only). Call it once you have \
investigated enough to propose a concrete plan: the goal, the files to change and how, risks, and how you will \
verify. Markdown is supported. If approved you can start implementing immediately."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"plan": {"type": "string", "description": "The plan in Markdown"}},
            "required": ["plan"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, _args: &Value) -> String {
        String::new()
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let plan = match arg_str(&args, "plan") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.events
            .send(AgentEvent::Plan(PlanRequest { plan, reply: tx }));
        let decision = tokio::select! {
            r = rx => r.ok(),
            _ = ctx.cancel.cancelled() => return ToolOutput::err("interrupted"),
        };
        match decision {
            Some(PlanDecision::Approve(mode)) => ToolOutput::ok(format!(
                "The user approved the plan. Plan mode is off (permission mode: {mode}); editing tools are now available. Implement the plan now, tracking progress with the todo tool."
            ))
            .with_summary("approved"),
            Some(PlanDecision::Revise(feedback)) => ToolOutput::ok(format!(
                "The user wants changes to the plan before approving:\n{feedback}\nRevise the plan (stay in plan mode) and propose it again."
            ))
            .with_summary("revise"),
            None => ToolOutput::ok("No decision was made. Stay in plan mode.").with_summary("no decision"),
        }
    }
}

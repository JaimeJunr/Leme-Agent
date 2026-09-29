//! `consult`: ask a different, stronger reasoning model for a second opinion
//! (planning, debugging dead-ends, reviews). Multi-model by design — any
//! OpenRouter model can be the oracle.

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_opt_str, arg_str};
use crate::agent::events::AgentEvent;
use crate::llm::ChatRequest;
use crate::util::{display_path, resolve_path};
use async_trait::async_trait;
use serde_json::{Value, json};

const MAX_CONTEXT_BYTES: usize = 300_000;

pub struct ConsultTool;

#[async_trait]
impl Tool for ConsultTool {
    fn name(&self) -> &str {
        "consult"
    }
    fn description(&self) -> String {
        "Ask an expert model (a different, stronger reasoning model) for a second opinion. Use it when stuck \
after a couple of failed attempts, for tricky debugging, to review a plan or architecture before a large \
change, or to double-check subtle code. It cannot see the conversation: give it a self-contained question \
with the relevant facts and list the files it should read in `files`. It is slower and more expensive — use \
it for hard problems, not routine work."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "Self-contained question with all relevant context"},
                "files": {"type": "array", "items": {"type": "string"}, "description": "Files to include"},
                "model": {"type": "string", "description": "Override the expert model (OpenRouter id)"}
            },
            "required": ["question"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        crate::util::first_line(arg_opt_str(args, "question").unwrap_or("?"), 100)
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let question = match arg_str(&args, "question") {
            Ok(q) => q.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let cfg = ctx.shared.cfg();
        let model = arg_opt_str(&args, "model")
            .map(String::from)
            .unwrap_or_else(|| cfg.oracle_model.clone());
        let mut context = String::new();
        if let Some(files) = args.get("files").and_then(|f| f.as_array()) {
            for f in files.iter().filter_map(|f| f.as_str()) {
                let p = resolve_path(&ctx.cwd(), f);
                match std::fs::read_to_string(&p) {
                    Ok(s) => {
                        if context.len() + s.len() > MAX_CONTEXT_BYTES {
                            context.push_str(&format!(
                                "\n[{} omitted: context budget exceeded]\n",
                                display_path(ctx.root(), &p)
                            ));
                            continue;
                        }
                        context.push_str(&format!(
                            "\n<file path=\"{}\">\n{}\n</file>\n",
                            display_path(ctx.root(), &p),
                            s
                        ));
                    }
                    Err(e) => context.push_str(&format!("\n[{}: {e}]\n", p.display())),
                }
            }
        }
        let catalog = ctx.shared.catalog();
        let info = catalog.get(&model);
        let reasoning = crate::llm::reasoning_param("high", info);
        let req = ChatRequest {
            model: model.clone(),
            messages: vec![
                json!({"role": "system", "content": "You are a principal software engineer acting as a consultant to another AI coding agent. Think carefully, then give a direct, actionable answer: the diagnosis or recommendation first, then concise reasoning, concrete steps and code where useful. Point out risks and wrong assumptions in the question. Do not pad."}),
                json!({"role": "user", "content": format!("{question}\n{context}")}),
            ],
            reasoning,
            max_tokens: Some(16_000),
            ..Default::default()
        };
        ctx.events.send(AgentEvent::ToolProgress {
            id: ctx.call_id.clone(),
            line: format!("consulting {model}…"),
        });
        match ctx.shared.client.complete(&req, &ctx.cancel).await {
            Ok(c) => {
                ctx.events.send(AgentEvent::Notice(format!(
                    "consult: {model} · {}",
                    crate::util::fmt_cost(c.usage.cost)
                )));
                ToolOutput::ok(format!("Expert ({model}) says:\n\n{}", c.text))
                    .with_summary(format!("{model} · {}", crate::util::fmt_cost(c.usage.cost)))
            }
            Err(e) => ToolOutput::err(format!("consult failed: {e}")),
        }
    }
}

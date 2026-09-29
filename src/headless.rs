//! Non-interactive mode (`leme -p "…"`): for scripts and CI.

use crate::agent::events::{AgentEvent, StopReason};
use crate::agent::{Agent, UserInput};
use crate::conversation::Item;
use serde_json::json;
use std::io::Write;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Text,
    Json,
    StreamJson,
}

impl Format {
    pub fn parse(s: &str) -> Format {
        match s {
            "json" => Format::Json,
            "stream-json" | "jsonl" | "ndjson" => Format::StreamJson,
            _ => Format::Text,
        }
    }
}

fn event_json(e: &AgentEvent) -> Option<serde_json::Value> {
    Some(match e {
        AgentEvent::StepStart { step } => json!({"type": "step", "step": step}),
        AgentEvent::Text(t) => json!({"type": "text", "text": t}),
        AgentEvent::Reasoning(t) => json!({"type": "reasoning", "text": t}),
        AgentEvent::AssistantEnd => json!({"type": "assistant_end"}),
        AgentEvent::ToolStart { id, name, summary } => {
            json!({"type": "tool_start", "id": id, "name": name, "summary": summary})
        }
        AgentEvent::ToolEnd {
            id,
            name,
            summary,
            result,
            is_error,
            display,
        } => {
            let mut v = json!({"type": "tool_end", "id": id, "name": name, "summary": summary, "result": result, "is_error": is_error});
            match display {
                Some(crate::tools::Display::Diff { path, diff }) => {
                    v["path"] = json!(path);
                    v["diff"] = json!(diff);
                }
                Some(crate::tools::Display::Todos(t)) => v["todos"] = json!(t),
                None => {}
            }
            v
        }
        AgentEvent::Usage {
            last,
            total,
            context,
            window,
        } => {
            json!({"type": "usage", "last": last, "total": total, "context_tokens": context, "context_window": window})
        }
        AgentEvent::Notice(m) => json!({"type": "notice", "message": m}),
        AgentEvent::Warning(m) => json!({"type": "warning", "message": m}),
        AgentEvent::Retrying(m) => json!({"type": "retry", "message": m}),
        AgentEvent::Restart => json!({"type": "restart"}),
        AgentEvent::Todos(t) => json!({"type": "todos", "todos": t}),
        AgentEvent::Compacted { before, after } => {
            json!({"type": "compacted", "before": before, "after": after})
        }
        AgentEvent::Sub {
            task_id,
            label,
            event,
        } => {
            let inner = event_json(event)?;
            json!({"type": "subagent", "task_id": task_id, "label": label, "event": inner})
        }
        AgentEvent::TurnEnd { reason } => {
            json!({"type": "turn_end", "reason": format!("{reason:?}")})
        }
        AgentEvent::ToolProgress { .. } | AgentEvent::ToolPreparing(_) => return None,
        AgentEvent::Approval(_) | AgentEvent::Ask(_) | AgentEvent::Plan(_) => return None,
    })
}

pub async fn run(
    mut agent: Agent,
    mut rx: UnboundedReceiver<AgentEvent>,
    prompt: String,
    format: Format,
    verbose: bool,
) -> i32 {
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            c2.cancel();
        }
    });
    let started = std::time::Instant::now();
    let session_id = agent.shared.session().id.clone();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut steps = 0u32;
    let reason = {
        let fut = agent.run_turn(
            UserInput {
                text: prompt,
                images: vec![],
            },
            cancel.clone(),
        );
        tokio::pin!(fut);
        let mut at_line_start = true;
        loop {
            tokio::select! {
                r = &mut fut => break r,
                Some(ev) = rx.recv() => {
                    if let AgentEvent::StepStart { step } = &ev { steps = *step; }
                    match format {
                        Format::StreamJson => {
                            if let Some(v) = event_json(&ev) {
                                let _ = writeln!(stdout, "{v}");
                                let _ = stdout.flush();
                            }
                        }
                        Format::Text => match &ev {
                            AgentEvent::Text(t) => {
                                let _ = write!(stdout, "{t}");
                                let _ = stdout.flush();
                                at_line_start = t.ends_with('\n');
                            }
                            AgentEvent::AssistantEnd => {
                                if !at_line_start {
                                    let _ = writeln!(stdout);
                                    at_line_start = true;
                                }
                            }
                            AgentEvent::ToolEnd { name, summary, result, is_error, .. } if verbose => {
                                let _ = writeln!(stderr, "{} {name} {summary} · {result}", if *is_error { "✗" } else { "●" });
                            }
                            AgentEvent::Warning(m) => { let _ = writeln!(stderr, "warning: {m}"); }
                            AgentEvent::Retrying(m) if verbose => { let _ = writeln!(stderr, "retry: {m}"); }
                            AgentEvent::Notice(m) if verbose => { let _ = writeln!(stderr, "· {m}"); }
                            _ => {}
                        },
                        Format::Json => {
                            if let AgentEvent::Warning(m) = &ev
                                && verbose { let _ = writeln!(stderr, "warning: {m}"); }
                        }
                    }
                }
            }
        }
    };
    while let Ok(ev) = rx.try_recv() {
        if format == Format::StreamJson
            && let Some(v) = event_json(&ev)
        {
            let _ = writeln!(stdout, "{v}");
        }
    }
    let final_text = agent
        .items
        .iter()
        .rev()
        .find_map(|i| match i {
            Item::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let total = agent.shared.total_usage.lock().clone();
    let code = match &reason {
        StopReason::Done => 0,
        StopReason::Interrupted => 130,
        StopReason::Budget | StopReason::MaxSteps => 2,
        StopReason::Error(_) => 1,
    };
    if let StopReason::Error(e) = &reason
        && format == Format::Text
    {
        let _ = writeln!(stderr, "error: {e}");
    }
    if matches!(format, Format::Json | Format::StreamJson) {
        let v = json!({
            "type": "result",
            "result": final_text,
            "stop_reason": match &reason { StopReason::Error(e) => format!("error: {e}"), r => format!("{r:?}").to_lowercase() },
            "is_error": code != 0,
            "session_id": session_id,
            "steps": steps,
            "duration_ms": started.elapsed().as_millis() as u64,
            "cost_usd": total.cost,
            "usage": total,
            "model": agent.model,
        });
        let _ = writeln!(stdout, "{v}");
    }
    let _ = stdout.flush();
    agent.shared.jobs.kill_all();
    code
}

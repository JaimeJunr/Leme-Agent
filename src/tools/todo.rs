//! Task list the model maintains for multi-step work. It survives context
//! compaction and is shown live in the UI.

use super::{Display, Tool, ToolCtx, ToolKind, ToolOutput};
use crate::agent::events::AgentEvent;
use crate::session::Record;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TodoItem {
    pub content: String,
    /// pending | in_progress | completed | cancelled
    pub status: String,
}

pub fn render(todos: &[TodoItem]) -> String {
    let mut s = String::new();
    for t in todos {
        let mark = match t.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[~]",
            "cancelled" => "[-]",
            _ => "[ ]",
        };
        s.push_str(&format!("{mark} {}\n", t.content));
    }
    s
}

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }
    fn description(&self) -> String {
        "Create/update your task list (send the complete list every time). Use it for any task with 3+ steps or \
when the user gives several requests: it keeps you on track and shows the user progress. Keep exactly one \
item `in_progress` while working, mark items `completed` as soon as they are done (not in batches), and add \
items you discover along the way. Skip it for trivial single-step requests."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        let n = args
            .get("todos")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        format!("{n} items")
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let raw = match args.get("todos") {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(s)) => serde_json::from_str::<Vec<Value>>(s).unwrap_or_default(),
            _ => return ToolOutput::err("`todos` must be an array"),
        };
        let mut todos = vec![];
        for t in raw {
            let content = t
                .get("content")
                .or_else(|| t.get("task"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if content.is_empty() {
                continue;
            }
            let status = match t
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("pending")
            {
                "done" | "complete" | "completed" => "completed",
                "in_progress" | "in-progress" | "active" | "doing" => "in_progress",
                "cancelled" | "canceled" | "skipped" => "cancelled",
                _ => "pending",
            };
            todos.push(TodoItem {
                content,
                status: status.into(),
            });
        }
        *ctx.shared.todos.lock() = todos.clone();
        if ctx.depth == 0 {
            ctx.shared.session().append(&Record::Todos {
                todos: todos.clone(),
            });
        }
        ctx.events.send(AgentEvent::Todos(todos.clone()));
        let done = todos.iter().filter(|t| t.status == "completed").count();
        let open = todos
            .iter()
            .filter(|t| t.status == "pending" || t.status == "in_progress")
            .count();
        let mut msg = format!(
            "Task list updated ({done}/{} done).\n{}",
            todos.len(),
            render(&todos)
        );
        if open == 0 && !todos.is_empty() {
            msg.push_str("All tasks are complete — verify the work and report back.");
        }
        ToolOutput::ok(msg)
            .with_summary(format!("{done}/{} done", todos.len()))
            .with_display(Display::Todos(todos))
    }
}

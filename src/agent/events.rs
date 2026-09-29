//! Events flowing from the agent core to a front-end (TUI, headless, …).

use crate::llm::Usage;
use crate::tools::Display;
use crate::tools::todo::TodoItem;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    Done,
    Interrupted,
    MaxSteps,
    Budget,
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    /// Allow and remember the given rule for this project.
    AllowAlways(String),
    /// Allow for the rest of this session.
    AllowSession(String),
    Deny(Option<String>),
}

pub struct ApprovalRequest {
    pub tool: String,
    pub summary: String,
    pub reason: String,
    pub preview: Option<String>,
    /// Suggested rule for "always allow".
    pub rule: String,
    pub reply: oneshot::Sender<Decision>,
}

pub struct AskRequest {
    pub question: String,
    pub options: Vec<String>,
    pub reply: oneshot::Sender<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanDecision {
    /// Approved; switch to the given permission mode.
    Approve(String),
    Revise(String),
}

pub struct PlanRequest {
    pub plan: String,
    pub reply: oneshot::Sender<PlanDecision>,
}

pub enum AgentEvent {
    StepStart {
        step: u32,
    },
    Reasoning(String),
    Text(String),
    /// The assistant message finished streaming.
    AssistantEnd,
    ToolStart {
        id: String,
        name: String,
        summary: String,
    },
    ToolProgress {
        id: String,
        line: String,
    },
    ToolEnd {
        id: String,
        name: String,
        summary: String,
        result: String,
        is_error: bool,
        display: Option<Display>,
    },
    Usage {
        last: Usage,
        total: Usage,
        context: u64,
        window: u64,
    },
    Notice(String),
    Warning(String),
    /// Transient API failure; the request is being retried.
    Retrying(String),
    /// Partial streamed output must be discarded (stream restart).
    Restart,
    Todos(Vec<TodoItem>),
    Approval(ApprovalRequest),
    Ask(AskRequest),
    Plan(PlanRequest),
    /// Event from a subagent.
    Sub {
        task_id: String,
        label: String,
        event: Box<AgentEvent>,
    },
    Compacted {
        before: u64,
        after: u64,
    },
    TurnEnd {
        reason: StopReason,
    },
}

impl AgentEvent {
    fn interactive(&self) -> bool {
        matches!(
            self,
            AgentEvent::Approval(_) | AgentEvent::Ask(_) | AgentEvent::Plan(_)
        )
    }
}

#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::UnboundedSender<AgentEvent>,
    wrap: Option<(String, String)>,
}

impl EventSink {
    pub fn new(tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        EventSink { tx, wrap: None }
    }

    pub fn channel() -> (EventSink, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (EventSink::new(tx), rx)
    }

    pub fn send(&self, e: AgentEvent) {
        let e = match &self.wrap {
            Some((id, label)) if !e.interactive() => AgentEvent::Sub {
                task_id: id.clone(),
                label: label.clone(),
                event: Box::new(e),
            },
            _ => e,
        };
        let _ = self.tx.send(e);
    }

    /// Sink for a subagent: events are wrapped, interactive ones pass through.
    pub fn sub(&self, task_id: &str, label: &str) -> EventSink {
        EventSink {
            tx: self.tx.clone(),
            wrap: Some((task_id.to_string(), label.to_string())),
        }
    }

    pub fn is_sub(&self) -> bool {
        self.wrap.is_some()
    }

    pub fn notice(&self, s: impl Into<String>) {
        self.send(AgentEvent::Notice(s.into()));
    }

    pub fn warn(&self, s: impl Into<String>) {
        self.send(AgentEvent::Warning(s.into()));
    }
}

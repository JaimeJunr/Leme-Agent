//! Interactive terminal UI.

pub mod commands;
pub mod composer;
pub mod markdown;
pub mod style;
pub mod term;

use crate::agent::events::{
    AgentEvent, ApprovalRequest, AskRequest, Decision, PlanDecision, PlanRequest, StopReason,
};
use crate::agent::shared::Shared;
use crate::agent::{Agent, UserInput};
use crate::llm::Usage;
use crate::permissions::Mode;
use crate::tools::Display;
use crate::tools::todo::TodoItem;
use crate::util::{fmt_cost, fmt_duration, fmt_tokens};
use composer::Composer;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use markdown::Markdown;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use style::{Line, Style, theme};
use term::Screen;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct RunningTool {
    id: String,
    name: String,
    summary: String,
    started: Instant,
    lines: VecDeque<String>,
}

struct SubState {
    id: String,
    label: String,
    tools: usize,
    current: String,
}

#[derive(Clone)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
    pub value: String,
}

#[derive(Clone, Copy, PartialEq)]
pub enum PickKind {
    Model,
    Resume,
    Rewind,
    Effort,
    Mode,
}

pub struct Picker {
    pub title: String,
    pub items: Vec<PickItem>,
    pub filter: String,
    pub sel: usize,
    pub kind: PickKind,
}

impl Picker {
    fn visible(&self) -> Vec<&PickItem> {
        let terms: Vec<String> = self
            .filter
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        self.items
            .iter()
            .filter(|i| {
                let hay = format!("{} {}", i.label, i.detail).to_lowercase();
                terms.iter().all(|t| hay.contains(t.as_str()))
            })
            .collect()
    }
}

struct Popup {
    items: Vec<(String, String, String)>, // (insert, label, description)
    sel: usize,
    range: (usize, usize),
}

#[derive(PartialEq, Clone, Copy)]
enum Block {
    None,
    User,
    Text,
    Tool,
    Info,
}

enum Prompt {
    Approval {
        req: ApprovalRequest,
        sel: usize,
        feedback: Option<Composer>,
    },
    Ask {
        req: AskRequest,
        sel: usize,
        input: Composer,
    },
    Plan {
        req: PlanRequest,
        sel: usize,
        feedback: Option<Composer>,
    },
}

pub struct App {
    screen: Screen,
    composer: Composer,
    agent: Option<Agent>,
    pub shared: Arc<Shared>,
    steer: Arc<Mutex<Vec<String>>>,
    md: Markdown,
    partial: String,
    reasoning: String,
    reasoning_start: Option<Instant>,
    cancel: Option<CancellationToken>,
    turn_started: Option<Instant>,
    turn_handle: Option<tokio::task::JoinHandle<Agent>>,
    tools: Vec<RunningTool>,
    subs: Vec<SubState>,
    prompts: VecDeque<Prompt>,
    picker: Option<Picker>,
    popup: Option<Popup>,
    todos: Vec<TodoItem>,
    commit: Vec<Line>,
    hint: Option<(String, Instant)>,
    retry: Option<String>,
    usage: Usage,
    last_usage: Usage,
    context: (u64, u64),
    verbose: bool,
    spinner: usize,
    quit: bool,
    last_ctrl_c: Option<Instant>,
    last_esc: Option<Instant>,
    files: Option<Vec<String>>,
    last_block: Block,
    pub model: String,
    pub effort: String,
    rx: UnboundedReceiver<AgentEvent>,
    pending_rewind_text: Option<String>,
    dirty: bool,
    tool_outputs: std::collections::HashMap<String, VecDeque<String>>,
    preparing: Option<String>,
}

fn tool_label(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp__") {
        return rest.replacen("__", "·", 1);
    }
    match name {
        "multi_edit" => "MultiEdit".into(),
        "apply_patch" => "Patch".into(),
        "web_fetch" => "Fetch".into(),
        "web_search" => "Search".into(),
        "ask_user" => "Ask".into(),
        "propose_plan" => "Plan".into(),
        "ls" => "List".into(),
        n => {
            let mut c = n.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

fn mode_badge(m: Mode, sandbox: bool) -> (String, Style) {
    match m {
        Mode::Default => ("default".into(), Style::fg(theme::MUTED)),
        Mode::AcceptEdits => (
            "⏵⏵ accept edits".into(),
            Style::fg(crossterm::style::Color::Magenta),
        ),
        Mode::Auto => (
            if sandbox {
                "⚡ auto · sandboxed".into()
            } else {
                "⚡ auto".into()
            },
            Style::fg(theme::WARN),
        ),
        Mode::Yolo => ("‼ yolo".into(), Style::fg(theme::ERR).bold()),
        Mode::Plan => ("⏸ plan mode".into(), Style::fg(theme::ACCENT)),
    }
}

impl App {
    pub fn new(agent: Agent, rx: UnboundedReceiver<AgentEvent>) -> std::io::Result<App> {
        let screen = Screen::new()?;
        let width = screen.width as usize;
        let shared = agent.shared.clone();
        let steer = agent.steer.clone();
        let usage = shared.total_usage.lock().clone();
        let todos = shared.todos.lock().clone();
        let history = crate::config::data_dir().join("history.jsonl");
        Ok(App {
            screen,
            composer: Composer::with_history(history),
            model: agent.model.clone(),
            effort: agent.effort.clone(),
            agent: Some(agent),
            shared,
            steer,
            md: Markdown::new(width.saturating_sub(2)),
            partial: String::new(),
            reasoning: String::new(),
            reasoning_start: None,
            cancel: None,
            turn_started: None,
            turn_handle: None,
            tools: vec![],
            subs: vec![],
            prompts: VecDeque::new(),
            picker: None,
            popup: None,
            todos,
            commit: vec![],
            hint: None,
            retry: None,
            usage,
            last_usage: Usage::default(),
            context: (0, 0),
            verbose: false,
            spinner: 0,
            quit: false,
            last_ctrl_c: None,
            last_esc: None,
            files: None,
            last_block: Block::None,
            rx,
            pending_rewind_text: None,
            dirty: true,
            tool_outputs: Default::default(),
            preparing: None,
        })
    }

    pub fn running(&self) -> bool {
        self.turn_handle.is_some()
    }

    pub fn agent_mut(&mut self) -> Option<&mut Agent> {
        self.agent.as_mut()
    }

    // ───────────────────────── output helpers ─────────────────────────

    fn gap(&mut self, b: Block) {
        let need = !matches!(
            (self.last_block, b),
            (Block::None, _) | (Block::Tool, Block::Tool) | (Block::Info, Block::Info)
        );
        if need && self.commit.last().map(|l| !l.is_empty()).unwrap_or(true) {
            self.commit.push(Line::new());
        }
        self.last_block = b;
    }

    pub fn print(&mut self, l: Line) {
        self.commit.push(l);
        self.dirty = true;
    }

    pub fn info(&mut self, s: &str) {
        self.gap(Block::Info);
        for (i, line) in s.lines().enumerate() {
            let prefix = if i == 0 { "· " } else { "  " };
            self.print(Line::styled(
                format!("{prefix}{line}"),
                Style::fg(theme::MUTED),
            ));
        }
    }

    pub fn warn(&mut self, s: &str) {
        self.gap(Block::Info);
        self.print(Line::styled(format!("! {s}"), Style::fg(theme::WARN)));
    }

    pub fn error(&mut self, s: &str) {
        self.gap(Block::Info);
        for (i, line) in s.lines().enumerate() {
            let prefix = if i == 0 { "✗ " } else { "  " };
            self.print(Line::styled(
                format!("{prefix}{line}"),
                Style::fg(theme::ERR),
            ));
        }
    }

    /// Print a block of Markdown (used by commands).
    pub fn markdown(&mut self, md: &str) {
        self.gap(Block::Text);
        let mut m = Markdown::new(self.screen.width as usize - 2);
        for l in md.lines() {
            for x in m.line(l) {
                self.print(x);
            }
        }
        for x in m.finish() {
            self.print(x);
        }
    }

    fn flush_partial(&mut self, all: bool) {
        while let Some(pos) = self.partial.find('\n') {
            let line: String = self.partial[..pos].to_string();
            self.partial.drain(..=pos);
            for l in self.md.line(&line) {
                self.commit.push(l);
            }
        }
        if all {
            if !self.partial.is_empty() {
                let line = std::mem::take(&mut self.partial);
                for l in self.md.line(&line) {
                    self.commit.push(l);
                }
            }
            let rest = self.md.finish();
            self.commit.extend(rest);
        }
        self.dirty = true;
    }

    fn end_reasoning(&mut self) {
        if let Some(start) = self.reasoning_start.take() {
            let dur = start.elapsed();
            let show = self.shared.cfg().show_reasoning;
            if show && !self.reasoning.trim().is_empty() {
                self.gap(Block::Info);
                self.print(Line::styled(
                    format!("✻ Thought for {}", fmt_duration(dur)),
                    Style::fg(theme::MUTED).italic(),
                ));
                if self.verbose {
                    for l in self.reasoning.clone().lines() {
                        self.print(Line::styled(
                            format!("  {l}"),
                            Style::fg(theme::MUTED).italic(),
                        ));
                    }
                }
            }
            self.reasoning.clear();
        }
    }

    // ───────────────────────── agent events ─────────────────────────

    fn on_agent_event(&mut self, ev: AgentEvent) {
        self.dirty = true;
        match ev {
            AgentEvent::StepStart { .. } => {
                self.retry = None;
                self.preparing = None;
            }
            AgentEvent::ToolPreparing(name) => {
                self.end_reasoning();
                self.flush_partial(true);
                self.preparing = Some(name);
            }
            AgentEvent::Reasoning(r) => {
                if self.reasoning_start.is_none() {
                    self.reasoning_start = Some(Instant::now());
                }
                self.reasoning.push_str(&r);
            }
            AgentEvent::Text(t) => {
                self.end_reasoning();
                self.retry = None;
                if self.last_block != Block::Text {
                    self.gap(Block::Text);
                }
                self.partial.push_str(&t);
                self.flush_partial(false);
            }
            AgentEvent::AssistantEnd => {
                self.end_reasoning();
                self.flush_partial(true);
            }
            AgentEvent::Restart => {
                self.partial.clear();
                self.md.reset();
                self.reasoning.clear();
                self.reasoning_start = None;
                self.warn("stream interrupted — retrying the response");
            }
            AgentEvent::Retrying(m) => self.retry = Some(m),
            AgentEvent::ToolStart { id, name, summary } => {
                self.preparing = None;
                self.end_reasoning();
                self.flush_partial(true);
                self.tools.push(RunningTool {
                    id,
                    name,
                    summary,
                    started: Instant::now(),
                    lines: VecDeque::new(),
                });
            }
            AgentEvent::ToolProgress { id, line } => {
                if let Some(t) = self.tools.iter_mut().find(|t| t.id == id) {
                    t.lines.push_back(line.clone());
                    if t.lines.len() > 200 {
                        t.lines.pop_front();
                    }
                }
            }
            AgentEvent::ToolEnd {
                id,
                name,
                summary,
                result,
                is_error,
                display,
            } => {
                let lines = self
                    .tools
                    .iter()
                    .position(|t| t.id == id)
                    .map(|i| self.tools.remove(i).lines)
                    .unwrap_or_default();
                self.tool_outputs.insert(id.clone(), lines.clone());
                self.render_tool_end(&name, &summary, &result, is_error, display, lines);
            }
            AgentEvent::Usage {
                last,
                total,
                context,
                window,
            } => {
                self.last_usage = last;
                self.usage = total;
                self.context = (context, window);
            }
            AgentEvent::Notice(m) => self.info(&m),
            AgentEvent::Warning(m) => self.warn(&m),
            AgentEvent::Todos(t) => self.todos = t,
            AgentEvent::Approval(req) => self.prompts.push_back(Prompt::Approval {
                req,
                sel: 0,
                feedback: None,
            }),
            AgentEvent::Ask(req) => self.prompts.push_back(Prompt::Ask {
                req,
                sel: 0,
                input: Composer::default(),
            }),
            AgentEvent::Plan(req) => {
                self.flush_partial(true);
                self.gap(Block::Text);
                self.print(Line::styled(
                    "Proposed plan",
                    Style::fg(theme::ACCENT).bold(),
                ));
                let plan = req.plan.clone();
                self.markdown(&plan);
                self.prompts.push_back(Prompt::Plan {
                    req,
                    sel: 0,
                    feedback: None,
                });
            }
            AgentEvent::Sub {
                task_id,
                label,
                event,
            } => self.on_sub_event(task_id, label, *event),
            AgentEvent::Compacted { before, after } => {
                self.info(&format!(
                    "context compacted: {} → {} tokens",
                    fmt_tokens(before),
                    fmt_tokens(after)
                ));
            }
            AgentEvent::TurnEnd { reason } => {
                self.end_reasoning();
                self.flush_partial(true);
                self.tools.clear();
                self.subs.clear();
                self.retry = None;
                let elapsed = self.turn_started.map(|t| t.elapsed()).unwrap_or_default();
                match reason {
                    StopReason::Done => {}
                    StopReason::Interrupted => self.warn("interrupted"),
                    StopReason::MaxSteps => self.warn("stopped: step limit reached (max_steps)"),
                    StopReason::Budget => self.warn("stopped: budget limit reached (max_cost)"),
                    StopReason::Error(e) => self.error(&e),
                }
                if elapsed > Duration::from_secs(20) {
                    self.gap(Block::Info);
                    self.print(Line::styled(
                        format!(
                            "─ {} · {} this session",
                            fmt_duration(elapsed),
                            fmt_cost(self.usage.cost)
                        ),
                        Style::fg(theme::MUTED),
                    ));
                    if self.shared.cfg().notify {
                        // Bell + OSC 9 desktop notification (iTerm2, WezTerm, kitty, Windows Terminal…)
                        self.screen.raw("\x07\x1b]9;harness: turn finished\x07");
                    }
                }
            }
        }
    }

    fn on_sub_event(&mut self, id: String, label: String, ev: AgentEvent) {
        let idx = match self.subs.iter().position(|s| s.id == id) {
            Some(i) => i,
            None => {
                self.subs.push(SubState {
                    id: id.clone(),
                    label: label.clone(),
                    tools: 0,
                    current: String::new(),
                });
                self.subs.len() - 1
            }
        };
        match ev {
            AgentEvent::ToolStart { name, summary, .. } => {
                let s = &mut self.subs[idx];
                s.tools += 1;
                s.current = format!("{} {}", tool_label(&name), summary);
                if self.verbose {
                    let line = format!(
                        "  ↳ {label}: {} {}",
                        tool_label(&name),
                        crate::util::ellipsize(&summary, 80)
                    );
                    self.print(Line::styled(line, Style::fg(theme::MUTED)));
                }
            }
            AgentEvent::Reasoning(_) | AgentEvent::Text(_) => {
                self.subs[idx].current = "thinking…".into();
            }
            AgentEvent::Warning(m) => self.warn(&format!("{label}: {m}")),
            AgentEvent::Retrying(m) => self.retry = Some(format!("{label}: {m}")),
            AgentEvent::TurnEnd { .. } => {
                self.subs.remove(idx);
            }
            AgentEvent::Sub {
                task_id,
                label,
                event,
            } => self.on_sub_event(task_id, label, *event),
            _ => {}
        }
    }

    fn render_tool_end(
        &mut self,
        name: &str,
        summary: &str,
        result: &str,
        is_error: bool,
        display: Option<Display>,
        lines: VecDeque<String>,
    ) {
        self.gap(Block::Tool);
        let (mark, mstyle) = if is_error {
            ("✗ ", Style::fg(theme::ERR))
        } else {
            ("● ", Style::fg(theme::OK))
        };
        let mut l = Line::styled(mark, mstyle);
        l.push(tool_label(name), Style::default().bold());
        if !summary.is_empty() {
            l.push(
                format!(" {}", crate::util::first_line(summary, 160)),
                Style::default(),
            );
        }
        if !result.is_empty() {
            l.push(
                format!(" · {}", crate::util::first_line(result, 100)),
                if is_error {
                    Style::fg(theme::ERR)
                } else {
                    Style::fg(theme::MUTED)
                },
            );
        }
        self.print(l);
        let max_lines = if self.verbose { 400 } else { 40 };
        match display {
            Some(Display::Diff { diff, .. }) => {
                let body: Vec<&str> = diff
                    .lines()
                    .filter(|l| !l.starts_with("---") && !l.starts_with("+++"))
                    .collect();
                for (shown, dl) in body.iter().enumerate() {
                    if shown >= max_lines {
                        self.print(Line::styled(
                            format!(
                                "    … {} more diff lines (ctrl+o for verbose)",
                                body.len() - shown
                            ),
                            Style::fg(theme::MUTED),
                        ));
                        break;
                    }
                    let st = if dl.starts_with('+') {
                        Style::fg(theme::OK)
                    } else if dl.starts_with('-') {
                        Style::fg(theme::ERR)
                    } else if dl.starts_with("@@") {
                        Style::fg(theme::ACCENT).dimmed()
                    } else {
                        Style::fg(theme::MUTED)
                    };
                    self.print(Line::styled(format!("    {dl}"), st));
                }
            }
            Some(Display::Todos(todos)) => {
                for t in &todos {
                    let (icon, st) = match t.status.as_str() {
                        "completed" => ("☑ ", Style::fg(theme::MUTED)),
                        "in_progress" => ("◐ ", Style::fg(theme::ACCENT).bold()),
                        "cancelled" => ("☒ ", Style::fg(theme::MUTED)),
                        _ => ("☐ ", Style::default()),
                    };
                    let mut st2 = st;
                    if t.status == "completed" || t.status == "cancelled" {
                        st2.strike = true;
                    }
                    self.print(
                        Line::styled(format!("    {icon}"), st).with(t.content.clone(), st2),
                    );
                }
            }
            None => {
                if name == "bash" || name == "verify" {
                    let n = if is_error {
                        12
                    } else if self.verbose {
                        60
                    } else {
                        4
                    };
                    let v: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
                    let start = v.len().saturating_sub(n);
                    if start > 0 {
                        self.print(Line::styled(
                            format!("    … {start} lines"),
                            Style::fg(theme::MUTED),
                        ));
                    }
                    for x in &v[start..] {
                        self.print(Line::styled(
                            format!("    {}", crate::util::ellipsize(x, 300)),
                            Style::fg(theme::MUTED),
                        ));
                    }
                }
            }
        }
    }

    // ───────────────────────── live region ─────────────────────────

    fn live_lines(&mut self) -> (Vec<Line>, Option<(u16, u16)>) {
        let w = self.screen.width as usize;
        let mut lines: Vec<Line> = vec![];
        let spin = SPINNER[self.spinner % SPINNER.len()];
        if self.running() {
            if self.reasoning_start.is_some() && self.shared.cfg().show_reasoning {
                let text: String = self
                    .reasoning
                    .chars()
                    .rev()
                    .take(400)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                let flat = text.replace('\n', " ");
                let tail: String = flat
                    .chars()
                    .rev()
                    .take(w.saturating_sub(6))
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                lines.push(Line::styled(
                    format!("✻ {}", tail.trim()),
                    Style::fg(theme::MUTED).italic(),
                ));
            }
            if !self.partial.is_empty() {
                let mut l = Line::new();
                markdown::inline(&self.partial, Style::default(), &mut l);
                for x in style::wrap(&l, w, 0) {
                    lines.push(x);
                }
            }
            for t in &self.tools {
                let mut l = Line::styled(format!("{spin} "), Style::fg(theme::ACCENT));
                l.push(tool_label(&t.name), Style::default().bold());
                l.push(
                    format!(" {}", crate::util::first_line(&t.summary, 120)),
                    Style::default(),
                );
                let el = t.started.elapsed();
                if el > Duration::from_secs(2) {
                    l.push(format!(" ({})", fmt_duration(el)), Style::fg(theme::MUTED));
                }
                lines.push(l);
                let n = if self.verbose { 10 } else { 4 };
                let start = t.lines.len().saturating_sub(n);
                for x in t.lines.iter().skip(start) {
                    lines.push(Line::styled(format!("    {x}"), Style::fg(theme::MUTED)));
                }
            }
            for s in &self.subs {
                let mut l = Line::styled(format!("  ↳ {}", s.label), Style::fg(theme::ACCENT));
                l.push(
                    format!(
                        " · {} tools · {}",
                        s.tools,
                        crate::util::ellipsize(&s.current, 80)
                    ),
                    Style::fg(theme::MUTED),
                );
                lines.push(l);
            }
            if let Some(r) = &self.retry {
                lines.push(Line::styled(format!("⟳ {r}"), Style::fg(theme::WARN)));
            }
            let el = self.turn_started.map(|t| t.elapsed()).unwrap_or_default();
            let doing = match &self.preparing {
                // A long tool call (e.g. writing a big file) is streaming.
                Some(t) if self.tools.is_empty() => format!("Preparing {}", tool_label(t)),
                _ => self
                    .todos
                    .iter()
                    .find(|t| t.status == "in_progress")
                    .map(|t| t.content.clone())
                    .unwrap_or_else(|| {
                        if self.tools.is_empty() {
                            "Thinking".into()
                        } else {
                            "Working".into()
                        }
                    }),
            };
            let mut l = Line::styled(format!("{spin} "), Style::fg(theme::ACCENT));
            l.push(
                format!("{}…", crate::util::ellipsize(&doing, 60)),
                Style::fg(theme::ACCENT),
            );
            l.push(
                format!(" ({} · esc to interrupt)", fmt_duration(el)),
                Style::fg(theme::MUTED),
            );
            lines.push(l);
            let steer = self.steer.lock().clone();
            for q in steer {
                lines.push(Line::styled(
                    format!("  ↪ queued: {}", crate::util::first_line(&q, 100)),
                    Style::fg(theme::MUTED),
                ));
            }
        }
        // Todo panel while working.
        if self.running()
            && self
                .todos
                .iter()
                .any(|t| t.status != "completed" && t.status != "cancelled")
        {
            let done = self
                .todos
                .iter()
                .filter(|t| t.status == "completed")
                .count();
            lines.push(Line::styled(
                format!("  tasks {done}/{}", self.todos.len()),
                Style::fg(theme::MUTED),
            ));
            for t in self
                .todos
                .iter()
                .filter(|t| t.status != "completed" && t.status != "cancelled")
                .take(5)
            {
                let (icon, st) = if t.status == "in_progress" {
                    ("◐", Style::fg(theme::ACCENT))
                } else {
                    ("☐", Style::fg(theme::MUTED))
                };
                lines.push(Line::styled(
                    format!(
                        "  {icon} {}",
                        crate::util::ellipsize(&t.content, w.saturating_sub(6))
                    ),
                    st,
                ));
            }
        }

        let cursor;
        let rule = Line::styled("─".repeat(w), Style::fg(theme::MUTED));
        if let Some(p) = self.prompts.front() {
            lines.push(rule.clone());
            let (pl, cur) = self.render_prompt(p, w);
            let base = lines.len() as u16;
            lines.extend(pl);
            cursor = cur.map(|(r, c)| (base + r, c));
        } else if let Some(p) = &self.picker {
            lines.push(rule.clone());
            lines.push(Line::styled(
                p.title.clone(),
                Style::fg(theme::ACCENT).bold(),
            ));
            let vis = p.visible();
            let sel = p.sel.min(vis.len().saturating_sub(1));
            let page = 10usize;
            let start = sel.saturating_sub(page - 1);
            for (i, it) in vis.iter().enumerate().skip(start).take(page) {
                let selected = i == sel;
                let mut l =
                    Line::styled(if selected { "❯ " } else { "  " }, Style::fg(theme::ACCENT));
                l.push(
                    it.label.clone(),
                    if selected {
                        Style::fg(theme::ACCENT).bold()
                    } else {
                        Style::default()
                    },
                );
                if !it.detail.is_empty() {
                    l.push(format!("  {}", it.detail), Style::fg(theme::MUTED));
                }
                lines.push(l);
            }
            if vis.is_empty() {
                lines.push(Line::styled("  (no matches)", Style::fg(theme::MUTED)));
            }
            let base = lines.len() as u16;
            lines.push(
                Line::styled("filter › ", Style::fg(theme::MUTED))
                    .with(p.filter.clone(), Style::default()),
            );
            cursor = Some((
                base,
                (9 + unicode_width::UnicodeWidthStr::width(p.filter.as_str())) as u16,
            ));
            lines.push(Line::styled(
                format!(
                    "  {} of {} · ↑↓ select · enter confirm · esc cancel",
                    vis.len(),
                    p.items.len()
                ),
                Style::fg(theme::MUTED),
            ));
        } else {
            if let Some(pp) = &self.popup {
                for (i, (_, label, desc)) in pp.items.iter().enumerate() {
                    let selected = i == pp.sel;
                    let mut l =
                        Line::styled(if selected { "❯ " } else { "  " }, Style::fg(theme::ACCENT));
                    l.push(
                        format!("{label:<22}"),
                        if selected {
                            Style::fg(theme::ACCENT).bold()
                        } else {
                            Style::default()
                        },
                    );
                    l.push(format!(" {}", desc), Style::fg(theme::MUTED));
                    lines.push(l);
                }
            }
            lines.push(rule.clone());
            let placeholder = if self.running() {
                "type to queue a message for the agent…"
            } else {
                "ask anything · / commands · @ files · ! shell · # memory"
            };
            let prompt_style = if self.composer.text.starts_with('!') {
                Style::fg(theme::WARN).bold()
            } else {
                Style::fg(theme::ACCENT).bold()
            };
            let (cl, (r, c)) = self.composer.render(w, "› ", prompt_style, placeholder);
            let base = lines.len() as u16;
            lines.extend(cl);
            cursor = Some((base + r, c));
        }
        lines.push(self.status_line(w));
        (lines, cursor)
    }

    fn render_prompt(&self, p: &Prompt, w: usize) -> (Vec<Line>, Option<(u16, u16)>) {
        let mut lines = vec![];
        let mut cursor = None;
        let options_line = |opts: &[&str], sel: usize| -> Vec<Line> {
            opts.iter()
                .enumerate()
                .map(|(i, o)| {
                    let s = i == sel;
                    Line::styled(if s { "❯ " } else { "  " }, Style::fg(theme::ACCENT)).with(
                        format!("{}. {o}", i + 1),
                        if s {
                            Style::fg(theme::ACCENT).bold()
                        } else {
                            Style::default()
                        },
                    )
                })
                .collect()
        };
        match p {
            Prompt::Approval { req, sel, feedback } => {
                lines.push(
                    Line::styled(
                        format!("Allow {}? ", tool_label(&req.tool)),
                        Style::fg(theme::WARN).bold(),
                    )
                    .with(format!("({})", req.reason), Style::fg(theme::MUTED)),
                );
                for l in req.summary.lines().take(8) {
                    lines.push(Line::styled(format!("  {l}"), Style::default().bold()));
                }
                if let Some(pv) = &req.preview {
                    let body: Vec<&str> = pv
                        .lines()
                        .filter(|l| !l.starts_with("---") && !l.starts_with("+++"))
                        .collect();
                    let maxp = (self.screen.height as usize / 2).max(6);
                    for dl in body.iter().take(maxp) {
                        let st = if dl.starts_with('+') {
                            Style::fg(theme::OK)
                        } else if dl.starts_with('-') {
                            Style::fg(theme::ERR)
                        } else {
                            Style::fg(theme::MUTED)
                        };
                        lines.push(Line::styled(format!("  {dl}"), st));
                    }
                    if body.len() > maxp {
                        lines.push(Line::styled(
                            format!("  … {} more lines", body.len() - maxp),
                            Style::fg(theme::MUTED),
                        ));
                    }
                }
                let always = if req.rule == "edit" {
                    "Yes, allow all edits this session".to_string()
                } else {
                    format!("Yes, always allow `{}` in this project", req.rule)
                };
                let opts = [
                    "Yes",
                    always.as_str(),
                    "No, and tell the agent what to do instead",
                ];
                lines.extend(options_line(&opts, *sel));
                if let Some(f) = feedback {
                    let (cl, (r, c)) = f.render(
                        w,
                        "  ✎ ",
                        Style::fg(theme::WARN),
                        "what should it do instead? (enter to send)",
                    );
                    let base = lines.len() as u16;
                    lines.extend(cl);
                    cursor = Some((base + r, c));
                } else {
                    lines.push(Line::styled(
                        "  y/a/n · enter select · esc deny",
                        Style::fg(theme::MUTED),
                    ));
                }
            }
            Prompt::Ask { req, sel, input } => {
                lines.push(
                    Line::styled("? ", Style::fg(theme::ACCENT).bold())
                        .with(req.question.clone(), Style::default().bold()),
                );
                let mut opts: Vec<&str> = req.options.iter().map(|s| s.as_str()).collect();
                opts.push("Type an answer…");
                lines.extend(options_line(&opts, *sel));
                if *sel == opts.len() - 1 {
                    let (cl, (r, c)) =
                        input.render(w, "  › ", Style::fg(theme::ACCENT), "your answer");
                    let base = lines.len() as u16;
                    lines.extend(cl);
                    cursor = Some((base + r, c));
                }
            }
            Prompt::Plan { sel, feedback, .. } => {
                lines.push(Line::styled(
                    "Approve this plan?",
                    Style::fg(theme::ACCENT).bold(),
                ));
                let opts = [
                    "Yes, and auto-accept edits",
                    "Yes, and ask before each edit",
                    "No, keep planning (give feedback)",
                ];
                lines.extend(options_line(&opts, *sel));
                if let Some(f) = feedback {
                    let (cl, (r, c)) =
                        f.render(w, "  ✎ ", Style::fg(theme::WARN), "what should change?");
                    let base = lines.len() as u16;
                    lines.extend(cl);
                    cursor = Some((base + r, c));
                }
            }
        }
        (lines, cursor)
    }

    fn status_line(&self, w: usize) -> Line {
        let mode = *self.shared.mode.read();
        let sandbox = self.shared.sandbox_available && self.shared.cfg().sandbox != "off";
        let (badge, bstyle) = mode_badge(mode, sandbox);
        let mut l = Line::styled(format!("  {badge}"), bstyle);
        let short = self
            .model
            .rsplit('/')
            .next()
            .unwrap_or(&self.model)
            .to_string();
        l.push(format!(" · {short}"), Style::fg(theme::MUTED));
        if !self.effort.is_empty() {
            l.push(format!(" · {}", self.effort), Style::fg(theme::MUTED));
        }
        let (ctx, win) = self.context;
        if win > 0 && ctx > 0 {
            let pct = (ctx as f64 / win as f64 * 100.0).round() as u64;
            let st = if pct >= 85 {
                Style::fg(theme::ERR)
            } else if pct >= 60 {
                Style::fg(theme::WARN)
            } else {
                Style::fg(theme::MUTED)
            };
            l.push(format!(" · ctx {pct}%"), st);
        }
        if self.usage.cost > 0.0 {
            l.push(
                format!(" · {}", fmt_cost(self.usage.cost)),
                Style::fg(theme::MUTED),
            );
        }
        if self.last_usage.prompt_tokens > 2000 && self.last_usage.cached_tokens > 0 {
            let pct = self.last_usage.cached_tokens * 100 / self.last_usage.prompt_tokens.max(1);
            l.push(format!(" · cache {pct}%"), Style::fg(theme::MUTED));
        }
        let jobs = self.shared.jobs.running();
        if jobs > 0 {
            l.push(
                format!(" · {jobs} bg job{}", if jobs == 1 { "" } else { "s" }),
                Style::fg(theme::WARN),
            );
        }
        if let Some((h, t)) = &self.hint {
            if t.elapsed() < Duration::from_secs(3) {
                l.push(format!("   {h}"), Style::fg(theme::WARN));
            }
        } else if !self.running() && self.composer.is_empty() && self.prompts.is_empty() {
            let hint = "   shift+tab mode · /help";
            if l.width() + hint.len() < w {
                l.push(hint, Style::fg(theme::MUTED));
            }
        }
        l
    }

    pub fn draw_now(&mut self) {
        self.draw();
    }

    fn draw(&mut self) {
        let (live, cursor) = self.live_lines();
        let commit = std::mem::take(&mut self.commit);
        let _ = self.screen.render(&commit, &live, cursor);
        self.dirty = false;
    }

    // ───────────────────────── input ─────────────────────────

    fn set_hint(&mut self, s: &str) {
        self.hint = Some((s.to_string(), Instant::now()));
    }

    fn update_popup(&mut self) {
        self.popup = None;
        let text = &self.composer.text;
        if text.starts_with('/') && !text.contains(' ') && !text.contains('\n') {
            let q = &text[1..].to_lowercase();
            let mut items: Vec<(String, String, String)> = vec![];
            for (name, desc) in commands::BUILTIN {
                if name.starts_with(q.as_str()) {
                    items.push((format!("/{name} "), format!("/{name}"), desc.to_string()));
                }
            }
            for c in &self.shared.ext.commands {
                if c.name.to_lowercase().contains(q.as_str()) {
                    items.push((
                        format!("/{} ", c.name),
                        format!("/{}", c.name),
                        format!("{} {}", c.description, c.argument_hint),
                    ));
                }
            }
            if !items.is_empty() && (items.len() != 1 || items[0].1 != *text) {
                items.truncate(8);
                self.popup = Some(Popup {
                    items,
                    sel: 0,
                    range: (0, text.len()),
                });
            }
            return;
        }
        let (start, end, tok) = self.composer.current_token();
        if let Some(q) = tok.strip_prefix('@') {
            let q = q.to_lowercase();
            if self.files.is_none() {
                self.files = Some(list_files(&self.shared.root));
            }
            let files = self.files.as_ref().unwrap();
            let mut scored: Vec<(i64, &String)> = files
                .iter()
                .filter_map(|f| fuzzy_score(&q, f).map(|s| (s, f)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
            let items: Vec<(String, String, String)> = scored
                .iter()
                .take(8)
                .map(|(_, f)| (format!("@{f} "), f.to_string(), String::new()))
                .collect();
            if !items.is_empty() {
                self.popup = Some(Popup {
                    items,
                    sel: 0,
                    range: (start, end),
                });
            }
        }
    }

    fn accept_popup(&mut self) -> bool {
        if let Some(p) = self.popup.take()
            && let Some((insert, _, _)) = p.items.get(p.sel)
        {
            self.composer.replace_range(p.range.0, p.range.1, insert);
            self.update_popup();
            return true;
        }
        false
    }

    async fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        self.dirty = true;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);

        // Global keys.
        if ctrl && k.code == KeyCode::Char('c') {
            if let Some(p) = self.prompts.pop_front() {
                self.reject_prompt(p);
                if let Some(c) = &self.cancel {
                    c.cancel();
                }
                return;
            }
            if self.picker.is_some() {
                self.picker = None;
                return;
            }
            if !self.composer.is_empty() {
                self.composer.clear();
                self.popup = None;
                return;
            }
            if let Some(c) = &self.cancel {
                c.cancel();
                self.set_hint("interrupting…");
                return;
            }
            if self
                .last_ctrl_c
                .map(|t| t.elapsed() < Duration::from_millis(1500))
                .unwrap_or(false)
            {
                self.quit = true;
            } else {
                self.last_ctrl_c = Some(Instant::now());
                self.set_hint("press ctrl+c again to exit");
            }
            return;
        }
        if ctrl && k.code == KeyCode::Char('o') {
            self.verbose = !self.verbose;
            self.set_hint(if self.verbose {
                "verbose on"
            } else {
                "verbose off"
            });
            return;
        }
        if k.code == KeyCode::BackTab {
            let m = self.shared.mode.read().next();
            self.set_mode(m);
            return;
        }

        if !self.prompts.is_empty() {
            self.on_prompt_key(k);
            return;
        }
        if self.picker.is_some() {
            self.on_picker_key(k).await;
            return;
        }

        // Popup navigation.
        if self.popup.is_some() {
            match k.code {
                KeyCode::Up => {
                    let p = self.popup.as_mut().unwrap();
                    p.sel = p.sel.checked_sub(1).unwrap_or(p.items.len() - 1);
                    return;
                }
                KeyCode::Down => {
                    let p = self.popup.as_mut().unwrap();
                    p.sel = (p.sel + 1) % p.items.len();
                    return;
                }
                KeyCode::Tab => {
                    self.accept_popup();
                    return;
                }
                KeyCode::Enter if !shift && !alt => {
                    let is_cmd = self.composer.text.starts_with('/');
                    if self.accept_popup() && !is_cmd {
                        return;
                    }
                    if is_cmd {
                        self.popup = None;
                        self.submit().await;
                        return;
                    }
                }
                KeyCode::Esc => {
                    self.popup = None;
                    return;
                }
                _ => {}
            }
        }

        match k.code {
            KeyCode::Esc => {
                if let Some(c) = &self.cancel {
                    c.cancel();
                    self.set_hint("interrupting…");
                    return;
                }
                if self.composer.is_empty()
                    && self
                        .last_esc
                        .map(|t| t.elapsed() < Duration::from_millis(800))
                        .unwrap_or(false)
                {
                    self.last_esc = None;
                    commands::open_rewind(self);
                    return;
                }
                self.last_esc = Some(Instant::now());
                if self.composer.is_empty() {
                    self.set_hint("esc again to rewind");
                }
            }
            KeyCode::Enter if shift || alt => self.composer.insert("\n"),
            KeyCode::Char('j') if ctrl => self.composer.insert("\n"),
            KeyCode::Enter => {
                // Trailing backslash = newline (works in any terminal).
                if self.composer.text[..self.composer.cursor].ends_with('\\') {
                    self.composer.backspace();
                    self.composer.insert("\n");
                } else {
                    self.submit().await;
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.composer.is_empty() && !self.running() {
                    self.quit = true;
                } else {
                    self.composer.delete();
                }
            }
            KeyCode::Char('a') if ctrl => self.composer.home(),
            KeyCode::Char('e') if ctrl => self.composer.end(),
            KeyCode::Char('k') if ctrl => self.composer.kill_to_end(),
            KeyCode::Char('u') if ctrl => self.composer.kill_to_start(),
            KeyCode::Char('w') if ctrl => self.composer.delete_word(),
            KeyCode::Char('l') if ctrl => {
                self.screen.raw("\x1b[2J\x1b[3J\x1b[H");
                let _ = self.screen.render(&[], &[], None);
            }
            KeyCode::Char('b') if alt => self.composer.word_left(),
            KeyCode::Char('f') if alt => self.composer.word_right(),
            KeyCode::Backspace if alt || ctrl => self.composer.delete_word(),
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left if ctrl || alt => self.composer.word_left(),
            KeyCode::Right if ctrl || alt => self.composer.word_right(),
            KeyCode::Left => self.composer.left(),
            KeyCode::Right => self.composer.right(),
            KeyCode::Home => self.composer.home(),
            KeyCode::End => self.composer.end(),
            KeyCode::Up => {
                if !self.composer.up() {
                    self.composer.history_prev();
                }
            }
            KeyCode::Down => {
                if !self.composer.down() {
                    self.composer.history_next();
                }
            }
            KeyCode::Tab => {
                self.update_popup();
                self.accept_popup();
            }
            KeyCode::Char(c) if !ctrl => {
                let mut b = [0u8; 4];
                self.composer.insert(c.encode_utf8(&mut b));
            }
            _ => {}
        }
        self.update_popup();
    }

    fn reject_prompt(&mut self, p: Prompt) {
        match p {
            Prompt::Approval { req, .. } => {
                let _ = req.reply.send(Decision::Deny(None));
            }
            Prompt::Ask { req, .. } => {
                let _ = req.reply.send(String::new());
            }
            Prompt::Plan { req, .. } => {
                let _ = req
                    .reply
                    .send(PlanDecision::Revise("The user dismissed the plan.".into()));
            }
        }
    }

    fn on_prompt_key(&mut self, k: KeyEvent) {
        let Some(mut p) = self.prompts.pop_front() else {
            return;
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Text entry sub-modes.
        let is_ask = matches!(p, Prompt::Ask { .. });
        let mut ask_up = false;
        let editing = match &mut p {
            Prompt::Approval {
                feedback: Some(f), ..
            }
            | Prompt::Plan {
                feedback: Some(f), ..
            } => Some(f),
            Prompt::Ask { req, sel, input } if *sel == req.options.len() => Some(input),
            _ => None,
        };
        if let Some(ed) = editing {
            match k.code {
                KeyCode::Enter => {
                    let text = ed.take();
                    match p {
                        Prompt::Approval { req, .. } => {
                            let _ = req.reply.send(Decision::Deny(Some(text)));
                        }
                        Prompt::Plan { req, .. } => {
                            let _ = req.reply.send(PlanDecision::Revise(text));
                        }
                        Prompt::Ask { req, .. } => {
                            let _ = req.reply.send(text);
                        }
                    }
                    return;
                }
                KeyCode::Esc => match &mut p {
                    Prompt::Approval { feedback, .. } | Prompt::Plan { feedback, .. } => {
                        *feedback = None
                    }
                    Prompt::Ask { sel, .. } => *sel = 0,
                },
                KeyCode::Backspace => ed.backspace(),
                KeyCode::Left => ed.left(),
                KeyCode::Right => ed.right(),
                KeyCode::Up if is_ask => ask_up = true,
                KeyCode::Char('u') if ctrl => ed.kill_to_start(),
                KeyCode::Char(c) if !ctrl => {
                    let mut b = [0u8; 4];
                    ed.insert(c.encode_utf8(&mut b));
                }
                _ => {}
            }
            if ask_up && let Prompt::Ask { sel, .. } = &mut p {
                *sel = sel.saturating_sub(1);
            }
            self.prompts.push_front(p);
            return;
        }
        match p {
            Prompt::Approval {
                req,
                mut sel,
                feedback,
            } => {
                let choose =
                    |i: usize, req: ApprovalRequest, app: &mut App| -> Option<ApprovalRequest> {
                        match i {
                            0 => {
                                let _ = req.reply.send(Decision::Allow);
                                None
                            }
                            1 => {
                                let d = if req.rule == "edit" {
                                    Decision::AllowSession("edit".into())
                                } else {
                                    app.info(&format!(
                                        "saved rule `{}` to .harness/local.toml",
                                        req.rule
                                    ));
                                    Decision::AllowAlways(req.rule.clone())
                                };
                                let _ = req.reply.send(d);
                                None
                            }
                            _ => Some(req),
                        }
                    };
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('1') => {
                        choose(0, req, self);
                    }
                    KeyCode::Char('a') | KeyCode::Char('2') => {
                        choose(1, req, self);
                    }
                    KeyCode::Char('n') | KeyCode::Char('3') => {
                        self.prompts.push_front(Prompt::Approval {
                            req,
                            sel: 2,
                            feedback: Some(Composer::default()),
                        });
                    }
                    KeyCode::Esc => {
                        let _ = req.reply.send(Decision::Deny(None));
                    }
                    KeyCode::Up => {
                        sel = sel.saturating_sub(1);
                        self.prompts
                            .push_front(Prompt::Approval { req, sel, feedback });
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        sel = (sel + 1).min(2);
                        self.prompts
                            .push_front(Prompt::Approval { req, sel, feedback });
                    }
                    KeyCode::Enter => {
                        if let Some(req) = choose(sel, req, self) {
                            self.prompts.push_front(Prompt::Approval {
                                req,
                                sel: 2,
                                feedback: Some(Composer::default()),
                            });
                        }
                    }
                    _ => self
                        .prompts
                        .push_front(Prompt::Approval { req, sel, feedback }),
                }
            }
            Prompt::Ask {
                req,
                mut sel,
                input,
            } => {
                let n = req.options.len() + 1;
                match k.code {
                    KeyCode::Up => sel = sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Tab => sel = (sel + 1).min(n - 1),
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let i = c.to_digit(10).unwrap() as usize;
                        if i >= 1 && i <= req.options.len() {
                            let _ = req.reply.send(req.options[i - 1].clone());
                            return;
                        }
                    }
                    KeyCode::Enter => {
                        if sel < req.options.len() {
                            let _ = req.reply.send(req.options[sel].clone());
                            return;
                        }
                    }
                    KeyCode::Esc => {
                        let _ = req.reply.send(String::new());
                        return;
                    }
                    _ => {}
                }
                self.prompts.push_front(Prompt::Ask { req, sel, input });
            }
            Prompt::Plan {
                req,
                mut sel,
                feedback,
            } => {
                let decide = |i: usize, req: PlanRequest, app: &mut App| -> Option<PlanRequest> {
                    match i {
                        0 => {
                            app.set_mode(Mode::AcceptEdits);
                            let _ = req.reply.send(PlanDecision::Approve("accept-edits".into()));
                            None
                        }
                        1 => {
                            app.set_mode(Mode::Default);
                            let _ = req.reply.send(PlanDecision::Approve("default".into()));
                            None
                        }
                        _ => Some(req),
                    }
                };
                match k.code {
                    KeyCode::Up => {
                        sel = sel.saturating_sub(1);
                        self.prompts.push_front(Prompt::Plan { req, sel, feedback });
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        sel = (sel + 1).min(2);
                        self.prompts.push_front(Prompt::Plan { req, sel, feedback });
                    }
                    KeyCode::Char('1') => {
                        decide(0, req, self);
                    }
                    KeyCode::Char('2') => {
                        decide(1, req, self);
                    }
                    KeyCode::Char('3') | KeyCode::Char('n') => {
                        self.prompts.push_front(Prompt::Plan {
                            req,
                            sel: 2,
                            feedback: Some(Composer::default()),
                        });
                    }
                    KeyCode::Enter => {
                        if let Some(req) = decide(sel, req, self) {
                            self.prompts.push_front(Prompt::Plan {
                                req,
                                sel: 2,
                                feedback: Some(Composer::default()),
                            });
                        }
                    }
                    KeyCode::Esc => {
                        let _ = req.reply.send(PlanDecision::Revise(
                            "The user dismissed the plan without feedback.".into(),
                        ));
                    }
                    _ => self.prompts.push_front(Prompt::Plan { req, sel, feedback }),
                }
            }
        }
    }

    async fn on_picker_key(&mut self, k: KeyEvent) {
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        let n = p.visible().len();
        match k.code {
            KeyCode::Esc => {
                self.picker = None;
            }
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => p.sel = (p.sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
            KeyCode::PageDown => p.sel = (p.sel + 10).min(n.saturating_sub(1)),
            KeyCode::Backspace => {
                p.filter.pop();
                p.sel = 0;
            }
            KeyCode::Enter => {
                let vis = p.visible();
                let chosen = vis
                    .get(p.sel.min(vis.len().saturating_sub(1)))
                    .map(|i| i.value.clone());
                let kind = p.kind;
                self.picker = None;
                if let Some(v) = chosen {
                    commands::picked(self, kind, v).await;
                }
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                p.filter.push(c);
                p.sel = 0;
            }
            _ => {}
        }
    }

    pub fn set_mode(&mut self, m: Mode) {
        *self.shared.mode.write() = m;
        if let Some(a) = self.agent.as_mut() {
            a.invalidate_system_prompt();
        }
        let (badge, _) = mode_badge(m, self.shared.sandbox_available);
        self.set_hint(&format!("mode: {badge}"));
        self.dirty = true;
    }

    pub fn open_picker(&mut self, title: &str, kind: PickKind, items: Vec<PickItem>, sel: usize) {
        self.picker = Some(Picker {
            title: title.to_string(),
            items,
            filter: String::new(),
            sel,
            kind,
        });
        self.dirty = true;
    }

    async fn submit(&mut self) {
        let text = self.composer.take();
        self.popup = None;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(cmd) = trimmed.strip_prefix('/') {
            let (name, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
            if commands::is_command(self, name) {
                commands::run(self, name, args.trim()).await;
                return;
            }
        }
        if self.running() {
            self.steer.lock().push(text);
            self.set_hint("queued — the agent will see it at its next step");
            return;
        }
        if let Some(cmd) = trimmed.strip_prefix('!') {
            commands::shell(self, cmd.trim()).await;
            return;
        }
        if let Some(note) = trimmed.strip_prefix('#')
            && !note.starts_with('#')
            && !note.trim().is_empty()
        {
            commands::remember(self, note.trim());
            return;
        }
        self.send(text);
    }

    /// Echo and start a turn with this user text.
    pub fn send(&mut self, text: String) {
        self.gap(Block::User);
        let w = self.screen.width as usize;
        for (i, l) in text.lines().enumerate() {
            let prefix = if i == 0 { "› " } else { "  " };
            let line = Line::styled(prefix, Style::fg(theme::ACCENT).bold())
                .with(l.to_string(), Style::default().bold());
            for x in style::wrap(&line, w, 2) {
                self.print(x);
            }
        }
        let input = self.expand_mentions(&text);
        self.start_turn(input);
    }

    /// `@path` mentions: inline file contents / attach images.
    fn expand_mentions(&mut self, text: &str) -> UserInput {
        let mut input = UserInput {
            text: text.to_string(),
            images: vec![],
        };
        let cwd = self.shared.cwd.lock().clone();
        let mut attached = String::new();
        for tok in text.split_whitespace() {
            let Some(p) = tok.strip_prefix('@') else {
                continue;
            };
            let p = p.trim_end_matches([',', '.', ';', ':', ')', '?', '!']);
            if p.is_empty() {
                continue;
            }
            let path = crate::util::resolve_path(&cwd, p);
            if !path.exists() {
                continue;
            }
            if path.is_dir() {
                let listing: Vec<String> = std::fs::read_dir(&path)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.file_name().to_string_lossy().to_string())
                            .take(200)
                            .collect()
                    })
                    .unwrap_or_default();
                attached.push_str(&format!(
                    "\n<directory path=\"{p}\">\n{}\n</directory>",
                    listing.join("\n")
                ));
                continue;
            }
            if let Ok(img) = crate::tools::fs::read_image(&path) {
                input.images.push(img);
                self.info(&format!("attached image {p}"));
                continue;
            }
            match std::fs::read(&path) {
                Ok(b) if b.len() <= 256 * 1024 && !crate::util::looks_binary(&b) => {
                    let s = String::from_utf8_lossy(&b);
                    attached.push_str(&format!("\n<file path=\"{p}\">\n{s}\n</file>"));
                    self.shared.files.lock().mark_read(&path);
                }
                _ => {}
            }
        }
        if !attached.is_empty() {
            input.text.push_str("\n\nReferenced files:");
            input.text.push_str(&attached);
        }
        input
    }

    pub fn start_turn(&mut self, input: UserInput) {
        let Some(mut agent) = self.agent.take() else {
            self.warn("agent busy");
            return;
        };
        self.model = agent.model.clone();
        self.effort = agent.effort.clone();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.turn_started = Some(Instant::now());
        self.turn_handle = Some(tokio::spawn(async move {
            agent.run_turn(input, cancel).await;
            agent
        }));
        self.dirty = true;
    }

    fn on_turn_finished(&mut self, agent: Agent) {
        self.model = agent.model.clone();
        self.effort = agent.effort.clone();
        self.agent = Some(agent);
        self.cancel = None;
        self.turn_handle = None;
        self.turn_started = None;
        // Drain late events.
        while let Ok(ev) = self.rx.try_recv() {
            self.on_agent_event(ev);
        }
        // Reject prompts that can no longer be answered.
        while let Some(p) = self.prompts.pop_front() {
            self.reject_prompt(p);
        }
        if let Some(text) = self.pending_rewind_text.take() {
            self.composer.set(&text);
        }
        // Messages queued after the agent's last step start a new turn.
        let queued: Vec<String> = std::mem::take(&mut *self.steer.lock());
        if !queued.is_empty() {
            self.send(queued.join("\n\n"));
        }
        if let Some(t) = self.shared.title.lock().clone() {
            self.screen.raw(&format!(
                "\x1b]0;harness · {}\x07",
                crate::util::ellipsize(&t, 60)
            ));
        }
        self.dirty = true;
    }

    pub fn print_banner(&mut self, warnings: &[String], resumed: bool) {
        let root = self.shared.root.display().to_string();
        let mut l = Line::styled("◆ harness", Style::fg(theme::ACCENT).bold());
        l.push(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Style::fg(theme::MUTED),
        );
        l.push(format!("  {}", self.model), Style::default());
        self.print(l);
        let mut info = format!(
            "  {}",
            crate::util::display_path(
                &dirs::home_dir().unwrap_or_default(),
                std::path::Path::new(&root)
            )
        );
        if !self.shared.instructions.is_empty() {
            let names: Vec<String> = self
                .shared
                .instructions
                .iter()
                .map(|(p, _)| crate::util::display_path(&self.shared.root, p))
                .collect();
            info.push_str(&format!(" · {}", names.join(", ")));
        }
        if let Some(m) = &self.shared.mcp {
            info.push_str(&format!(" · {} MCP tools", m.tools.len()));
        }
        if !self.shared.ext.skills.is_empty() {
            let n = self.shared.ext.skills.len();
            info.push_str(&format!(" · {n} skill{}", if n == 1 { "" } else { "s" }));
        }
        self.print(Line::styled(info, Style::fg(theme::MUTED)));
        if resumed && let Some(a) = &self.agent {
            let n = a.user_turns();
            // Replay the last exchange for context.
            let last_user = a.items.iter().rev().find_map(|i| match i {
                crate::conversation::Item::User {
                    content,
                    synthetic: false,
                    ..
                } => Some(content.clone()),
                _ => None,
            });
            let last_asst = a.items.iter().rev().find_map(|i| match i {
                crate::conversation::Item::Assistant { text, .. } if !text.is_empty() => {
                    Some(text.clone())
                }
                _ => None,
            });
            self.print(Line::styled(
                format!(
                    "  resumed session {} ({n} messages)",
                    self.shared.session().id
                ),
                Style::fg(theme::MUTED),
            ));
            if let Some(u) = last_user {
                self.print(
                    Line::styled("› ", Style::fg(theme::ACCENT))
                        .with(crate::util::first_line(&u, 200), Style::default().bold()),
                );
            }
            if let Some(t) = last_asst {
                let snippet: String = t
                    .lines()
                    .rev()
                    .take(6)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                self.markdown(&snippet);
            }
        }
        for w in warnings {
            self.warn(w);
        }
        self.print(Line::new());
    }

    pub async fn run(mut self, initial: Option<String>) -> anyhow::Result<()> {
        self.screen.enter()?;
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(80));
        if let Some(t) = self.shared.title.lock().clone() {
            self.screen.raw(&format!(
                "\x1b]0;harness · {}\x07",
                crate::util::ellipsize(&t, 60)
            ));
        } else {
            self.screen.raw("\x1b]0;harness\x07");
        }
        if let Some(text) = initial
            && !text.trim().is_empty()
        {
            self.send(text);
        }
        self.draw();
        let mut last_draw = Instant::now();
        loop {
            let handle = self.turn_handle.as_mut();
            let turn_done = async {
                match handle {
                    Some(h) => h.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                ev = events.next() => {
                    match ev {
                        Some(Ok(Event::Key(k))) => self.on_key(k).await,
                        Some(Ok(Event::Paste(s))) => {
                            if let Some(Prompt::Approval { feedback: Some(f), .. } | Prompt::Plan { feedback: Some(f), .. }) = self.prompts.front_mut() {
                                f.paste(&s);
                            } else if let Some(Prompt::Ask { input, .. }) = self.prompts.front_mut() {
                                input.paste(&s);
                            } else if let Some(p) = self.picker.as_mut() {
                                p.filter.push_str(s.trim());
                            } else {
                                // Dropped/pasted image paths become @mentions.
                                let t = s.trim().trim_matches(['\'', '"']);
                                let is_img = std::path::Path::new(t).is_file() && crate::tools::fs::read_image(std::path::Path::new(t)).is_ok();
                                if is_img {
                                    self.composer.insert(&format!("@{} ", t.replace(' ', "\\ ")));
                                } else {
                                    self.composer.paste(&s);
                                }
                                self.update_popup();
                            }
                            self.dirty = true;
                        }
                        Some(Ok(Event::Resize(w, h))) => {
                            self.screen.resize(w, h);
                            self.md.set_width((w as usize).saturating_sub(2));
                            self.dirty = true;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => { self.quit = true; }
                    }
                }
                Some(ev) = self.rx.recv() => self.on_agent_event(ev),
                res = turn_done => {
                    match res {
                        Ok(agent) => self.on_turn_finished(agent),
                        Err(e) => {
                            self.error(&format!("agent task crashed: {e}"));
                            self.turn_handle = None;
                            self.cancel = None;
                            self.quit = true;
                        }
                    }
                }
                _ = tick.tick() => {
                    if self.running() || self.hint.is_some() {
                        self.spinner += 1;
                        self.dirty = true;
                    }
                    if let Some((_, t)) = &self.hint
                        && t.elapsed() > Duration::from_secs(3) { self.hint = None; }
                }
            }
            if self.quit {
                break;
            }
            if self.dirty
                && (last_draw.elapsed() >= Duration::from_millis(16) || !self.commit.is_empty())
            {
                self.draw();
                last_draw = Instant::now();
            }
        }
        if let Some(c) = &self.cancel {
            c.cancel();
        }
        self.shared.jobs.kill_all();
        let usage = self.usage.clone();
        let session = self.shared.session().id.clone();
        let _ = self.screen.render(&[], &[], None);
        self.screen.leave();
        drop(self);
        println!(
            "\x1b[2m{} · resume with: harness -r {}\x1b[0m",
            if usage.cost > 0.0 {
                format!("session cost {}", fmt_cost(usage.cost))
            } else {
                "bye".into()
            },
            session
        );
        Ok(())
    }
}

fn list_files(root: &std::path::Path) -> Vec<String> {
    let mut out = vec![];
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for e in walker.flatten().take(20_000) {
        if e.path() == root {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(root)
            .unwrap_or(e.path())
            .display()
            .to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(if is_dir { format!("{rel}/") } else { rel });
    }
    out
}

/// Subsequence fuzzy match; higher is better.
pub fn fuzzy_score(q: &str, s: &str) -> Option<i64> {
    if q.is_empty() {
        return Some(0);
    }
    let sl = s.to_lowercase();
    if let Some(i) = sl.find(q) {
        let base = sl.rfind('/').map(|j| j + 1).unwrap_or(0);
        return Some(1000 - i as i64 + if i >= base { 500 } else { 0 });
    }
    let mut score = 0i64;
    let mut it = sl.chars().enumerate();
    let mut last = 0usize;
    for qc in q.chars() {
        loop {
            let (i, c) = it.next()?;
            if c == qc {
                score += if i == last + 1 { 5 } else { 1 };
                last = i;
                break;
            }
        }
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy() {
        assert!(
            fuzzy_score("main", "src/main.rs").unwrap()
                > fuzzy_score("main", "src/domain/x.rs").unwrap_or(0)
        );
        assert!(fuzzy_score("smr", "src/main.rs").is_some());
        assert!(fuzzy_score("zzz", "src/main.rs").is_none());
    }
}

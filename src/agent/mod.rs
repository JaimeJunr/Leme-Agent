//! The agent loop.

pub mod compact;
pub mod events;
pub mod prompt;
pub mod shared;

use crate::conversation::{self, Image, Item, WireOptions};
use crate::extensions::AgentDef;
use crate::llm::{self, ChatRequest, Completion, ModelInfo, StreamEvent, ToolCall, Usage};
use crate::permissions::{Checker, Mode, Verdict};
use crate::session::Record;
use crate::tools::{self, Registry, RegistryOptions, ToolCtx, ToolOutput};
use events::{AgentEvent, ApprovalRequest, Decision, EventSink, StopReason};
use parking_lot::Mutex;
use serde_json::{Value, json};
use shared::Shared;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

/// User input for one turn.
#[derive(Debug, Clone, Default)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<Image>,
}

impl From<&str> for UserInput {
    fn from(s: &str) -> Self {
        UserInput {
            text: s.to_string(),
            images: vec![],
        }
    }
}

pub struct Agent {
    pub shared: Arc<Shared>,
    pub items: Vec<Item>,
    pub model: String,
    pub effort: String,
    pub depth: u32,
    pub events: EventSink,
    /// Messages typed while the agent works; injected at the next step.
    pub steer: Arc<Mutex<Vec<String>>>,
    /// Persist to the session file (main agent only).
    pub persist: bool,
    pub usage: Usage,
    last_context: u64,
    items_at_last: usize,
    system_cache: Option<(String, String)>,
    pub def: Option<AgentDef>,
    recent_calls: VecDeque<(u64, u64)>,
    title_done: bool,
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn is_context_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    [
        "context length",
        "context_length",
        "maximum context",
        "too many tokens",
        "prompt is too long",
        "context window",
        "input is too long",
        "exceeds the context",
    ]
    .iter()
    .any(|k| s.contains(k))
}

impl Agent {
    pub fn new(shared: Arc<Shared>, model: String, effort: String, events: EventSink) -> Agent {
        Agent {
            shared,
            items: vec![],
            model,
            effort,
            depth: 0,
            events,
            steer: Arc::new(Mutex::new(vec![])),
            persist: true,
            usage: Usage::default(),
            last_context: 0,
            items_at_last: 0,
            system_cache: None,
            def: None,
            recent_calls: VecDeque::new(),
            title_done: false,
        }
    }

    /// A subagent sharing files/session state but with its own context.
    pub fn sub(parent: &Agent, def: AgentDef, model: String, events: EventSink) -> Agent {
        let mut a = Agent::new(parent.shared.clone(), model, parent.effort.clone(), events);
        a.depth = parent.depth + 1;
        a.persist = false;
        a.def = Some(def);
        a.title_done = true;
        a
    }

    pub fn mode(&self) -> Mode {
        *self.shared.mode.read()
    }

    pub fn model_info(&self) -> Option<ModelInfo> {
        self.shared.catalog().get(&self.model).cloned()
    }

    /// Context window we allow ourselves to use.
    pub fn window(&self) -> u64 {
        let cfg = self.shared.cfg();
        let model_ctx = self
            .model_info()
            .map(|i| i.context_length)
            .filter(|c| *c > 0)
            .unwrap_or(128_000);
        if cfg.context_limit > 0 {
            model_ctx.min(cfg.context_limit)
        } else {
            model_ctx
        }
    }

    fn max_tokens(&self) -> u32 {
        let cfg = self.shared.cfg();
        let mut m = cfg.max_output_tokens.max(1024) as u64;
        if let Some(i) = self.model_info()
            && i.max_output > 0
        {
            m = m.min(i.max_output);
        }
        m.min(self.window() / 3) as u32
    }

    /// Estimated tokens currently in context.
    pub fn context_tokens(&self) -> u64 {
        if self.last_context > 0 && self.items_at_last <= self.items.len() {
            let added: usize = self.items[self.items_at_last..]
                .iter()
                .map(|i| i.estimate_tokens())
                .sum();
            self.last_context + added as u64
        } else {
            let sys = self
                .system_cache
                .as_ref()
                .map(|(_, s)| crate::util::estimate_tokens(s))
                .unwrap_or(4000);
            (sys + 3000
                + self
                    .items
                    .iter()
                    .map(|i| i.estimate_tokens())
                    .sum::<usize>()) as u64
        }
    }

    fn record(&self, r: Record) {
        if self.persist {
            self.shared.session().append(&r);
        }
    }

    pub fn push(&mut self, item: Item) {
        self.record(Record::Item { item: item.clone() });
        self.items.push(item);
    }

    /// Number of real user messages (the turn index used for checkpoints).
    pub fn user_turns(&self) -> usize {
        self.items.iter().filter(|i| i.is_real_user()).count()
    }

    pub fn registry(&self) -> Registry {
        let cfg = self.shared.cfg();
        let mode = self.mode();
        let mut r = tools::builtin(&RegistryOptions {
            edit_format: tools::edit_format_for(&cfg.edit_format, &self.model),
            subagents: self.depth == 0,
            interactive: self.shared.interactive && self.depth == 0,
            plan_mode: mode == Mode::Plan && self.depth == 0,
            disabled: cfg.disabled_tools.clone(),
        });
        if self.depth > 0 {
            r.retain(|n| !matches!(n, "todo" | "ask_user" | "task" | "propose_plan"));
        }
        if mode == Mode::Plan && self.depth > 0 {
            r.retain(|n| !matches!(n, "edit" | "multi_edit" | "write" | "apply_patch"));
        }
        let allowed = self.def.as_ref().and_then(|d| d.tools.clone());
        if let Some(mcp) = &self.shared.mcp
            && (allowed.is_none()
                || allowed
                    .as_ref()
                    .map(|a| a.iter().any(|t| t.starts_with("mcp__")))
                    .unwrap_or(false))
        {
            r.tools.extend(mcp.tool_objects());
        }
        if let Some(allowed) = allowed {
            let allowed: Vec<String> = allowed
                .iter()
                .map(|t| crate::extensions::map_tool_name(t))
                .collect();
            r.retain(|n| {
                allowed.iter().any(|a| {
                    a == n
                        || crate::util::wildcard_match(a, n)
                        || (a == "edit" && matches!(n, "multi_edit" | "apply_patch" | "write"))
                        || (a == "bash" && n == "jobs")
                })
            });
        }
        r
    }

    pub fn system_prompt(&mut self) -> String {
        let mode = self.mode();
        let key = format!(
            "{}|{}|{}",
            self.model,
            mode.name(),
            self.def.as_ref().map(|d| d.name.as_str()).unwrap_or("")
        );
        if let Some((k, s)) = &self.system_cache
            && *k == key
        {
            return s.clone();
        }
        let cfg = self.shared.cfg();
        let cwd = self.shared.cwd.lock().clone();
        let mcp_names: Vec<String> = self
            .shared
            .mcp
            .as_ref()
            .map(|m| m.servers.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default();
        let mut s = prompt::build(&prompt::PromptInputs {
            root: &self.shared.root,
            cwd: &cwd,
            model: &self.model,
            mode: if self.depth > 0 { Mode::Default } else { mode },
            instructions: &self.shared.instructions,
            ext: &self.shared.ext,
            extra: cfg.instructions.as_deref(),
            git: self.shared.git.as_deref(),
            subagents: self.depth == 0,
            mcp_servers: &mcp_names,
        });
        if let Some(m) = &self.shared.mcp {
            for srv in &m.servers {
                if let Some(ins) = &srv.instructions {
                    s.push_str(&format!(
                        "\n# MCP server `{}` instructions\n{}\n",
                        srv.name,
                        ins.trim()
                    ));
                }
            }
        }
        if let Some(def) = &self.def {
            s.push_str(prompt::SUBAGENT_SUFFIX);
            if !def.prompt.trim().is_empty() {
                s.push_str(&format!(
                    "\n\n# Your role: {}\n{}",
                    def.name,
                    def.prompt.trim()
                ));
            }
        }
        self.system_cache = Some((key, s.clone()));
        s
    }

    fn build_request(&mut self, registry: &Registry) -> ChatRequest {
        let cfg = self.shared.cfg();
        let system = self.system_prompt();
        let info = self.model_info();
        let opts = WireOptions {
            cache_control: cfg.cache != "off" && ModelInfo::wants_cache_control(&self.model),
            vision: info.as_ref().map(|i| i.input_images).unwrap_or(true),
            include_reasoning: true,
        };
        let messages = conversation::to_wire(&system, &self.items, &opts);
        ChatRequest {
            model: self.model.clone(),
            messages,
            tools: registry.definitions(),
            reasoning: llm::reasoning_param(&self.effort, info.as_ref()),
            max_tokens: Some(self.max_tokens()),
            temperature: cfg.temperature,
            provider: cfg
                .provider
                .as_ref()
                .and_then(|p| serde_json::to_value(p).ok()),
            fallback_models: cfg.fallback_models.clone(),
            session_id: Some(format!("{}-{}", self.shared.session().id, self.depth)),
            plugins: None,
            extra: cfg
                .extra_body
                .as_ref()
                .and_then(|p| serde_json::to_value(p).ok()),
            parallel_tool_calls: None,
        }
    }

    async fn call_model(
        &mut self,
        registry: &Registry,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Completion> {
        let req = self.build_request(registry);
        let events = self.events.clone();
        let mut on_event = move |e: StreamEvent| match e {
            StreamEvent::Text(t) => events.send(AgentEvent::Text(t)),
            StreamEvent::Reasoning(r) => events.send(AgentEvent::Reasoning(r)),
            StreamEvent::ToolCallStart(_, name) => events.send(AgentEvent::ToolPreparing(name)),
            StreamEvent::Restart => events.send(AgentEvent::Restart),
            StreamEvent::Retrying(m) => events.send(AgentEvent::Retrying(m)),
        };
        let mut c = self
            .shared
            .client
            .stream(&req, cancel, &mut on_event)
            .await?;
        if c.usage.cost == 0.0
            && let Some(i) = self.model_info()
        {
            c.usage.cost = i.estimate_cost(
                c.usage.prompt_tokens,
                c.usage.cached_tokens,
                c.usage.cache_write_tokens,
                c.usage.completion_tokens,
            );
        }
        Ok(c)
    }

    fn account(&mut self, c: &Completion) {
        self.usage.add(&c.usage);
        self.shared.total_usage.lock().add(&c.usage);
        if c.usage.prompt_tokens > 0 {
            self.last_context = c.usage.prompt_tokens + c.usage.completion_tokens;
            self.items_at_last = self.items.len();
        }
        self.record(Record::Usage {
            usage: c.usage.clone(),
            model: c.model.clone(),
        });
        let total = self.shared.total_usage.lock().clone();
        self.events.send(AgentEvent::Usage {
            last: c.usage.clone(),
            total,
            context: self.context_tokens(),
            window: self.window(),
        });
    }

    fn drain_steer(&mut self) {
        let msgs: Vec<String> = std::mem::take(&mut *self.steer.lock());
        for m in msgs {
            self.push(Item::User {
                content: format!("[The user sent this message while you were working — take it into account]\n{m}"),
                images: vec![],
                synthetic: false,
            });
        }
    }

    /// Run one user turn to completion.
    pub async fn run_turn(&mut self, input: UserInput, cancel: CancellationToken) -> StopReason {
        let reason = self.run_turn_inner(input, &cancel).await;
        conversation::repair_dangling_calls(&mut self.items);
        self.events.send(AgentEvent::TurnEnd {
            reason: reason.clone(),
        });
        reason
    }

    async fn run_turn_inner(&mut self, input: UserInput, cancel: &CancellationToken) -> StopReason {
        let cfg = self.shared.cfg();
        let mut text = input.text.clone();
        if self.depth == 0 {
            let cwd = self.shared.cwd.lock().clone();
            let h = crate::hooks::run(
                &cfg.hooks,
                "user_prompt",
                None,
                &json!({"prompt": text, "session_id": self.shared.session().id}),
                &cwd,
            )
            .await;
            for w in h.warnings {
                self.events.warn(w);
            }
            if let Some(b) = h.block {
                self.events.warn(format!("prompt blocked by hook: {b}"));
                return StopReason::Error("blocked by hook".into());
            }
            if !h.context.is_empty() {
                text.push_str(&format!(
                    "\n\n<hook-context>\n{}\n</hook-context>",
                    h.context.join("\n")
                ));
            }
            self.shared.turn.store(self.user_turns(), Ordering::SeqCst);
            self.shared.modified.lock().clear();
        }
        self.push(Item::User {
            content: text,
            images: input.images,
            synthetic: false,
        });
        if self.depth == 0 && !self.title_done && self.shared.interactive {
            self.title_done = true;
            self.spawn_title(input.text.clone());
        }

        let max_steps = cfg.max_steps.max(1);
        let mut verify_rounds = 0u32;
        let mut length_continues = 0u32;
        let mut empty_retries = 0u32;
        let mut compacted_for_error = false;
        let mut step = 0u32;
        loop {
            step += 1;
            if step > max_steps {
                self.events
                    .warn(format!("stopped after {max_steps} steps (max_steps)"));
                return StopReason::MaxSteps;
            }
            if cancel.is_cancelled() {
                return StopReason::Interrupted;
            }
            self.drain_steer();
            if let Err(e) = self.maybe_compact(cancel).await {
                if cancel.is_cancelled() {
                    return StopReason::Interrupted;
                }
                self.events.warn(format!("compaction failed: {e:#}"));
            }
            let registry = self.registry();
            self.events.send(AgentEvent::StepStart { step });
            let completion = match self.call_model(&registry, cancel).await {
                Ok(c) => c,
                Err(e) => {
                    if cancel.is_cancelled() {
                        return StopReason::Interrupted;
                    }
                    if is_context_error(&e) && !compacted_for_error {
                        compacted_for_error = true;
                        self.events
                            .warn("context window exceeded — compacting and retrying");
                        if let Err(ce) = self.compact(None, cancel).await {
                            return StopReason::Error(format!(
                                "{e:#}; compaction also failed: {ce:#}"
                            ));
                        }
                        step -= 1;
                        continue;
                    }
                    return StopReason::Error(format!("{e:#}"));
                }
            };
            let empty = completion.text.trim().is_empty() && completion.tool_calls.is_empty();
            if empty && empty_retries < 2 && completion.finish_reason.as_deref() != Some("length") {
                empty_retries += 1;
                self.account(&completion);
                self.events
                    .warn("model returned an empty response — retrying");
                continue;
            }
            self.push(Item::Assistant {
                text: completion.text.clone(),
                reasoning: completion.reasoning.clone(),
                reasoning_details: completion.reasoning_details.clone(),
                tool_calls: completion.tool_calls.clone(),
                model: completion.model.clone(),
            });
            self.events.send(AgentEvent::AssistantEnd);
            self.account(&completion);

            let total_cost = self.shared.total_usage.lock().cost;
            let cfg = self.shared.cfg();
            if cfg.max_cost > 0.0 && total_cost >= cfg.max_cost {
                conversation::repair_dangling_calls(&mut self.items);
                self.events.warn(format!(
                    "budget reached: {} ≥ max_cost {}",
                    crate::util::fmt_cost(total_cost),
                    crate::util::fmt_cost(cfg.max_cost)
                ));
                return StopReason::Budget;
            }

            if completion.tool_calls.is_empty() {
                if completion.finish_reason.as_deref() == Some("length") && length_continues < 3 {
                    length_continues += 1;
                    self.push(Item::synthetic("Your previous response was cut off by the output token limit. Continue exactly where you left off (if you were writing a large file, split it into smaller edits)."));
                    continue;
                }
                if !self.steer.lock().is_empty() {
                    continue;
                }
                if self.depth == 0 {
                    if let Some(feedback) = self.verify(verify_rounds, cancel).await {
                        verify_rounds += 1;
                        self.push(Item::synthetic(feedback));
                        continue;
                    }
                    let cwd = self.shared.cwd.lock().clone();
                    let h = crate::hooks::run(&cfg.hooks, "stop", None, &json!({"session_id": self.shared.session().id, "last_message": completion.text}), &cwd).await;
                    for w in h.warnings {
                        self.events.warn(w);
                    }
                    if let Some(b) = h.block
                        && verify_rounds < cfg.verify_rounds + 2
                    {
                        verify_rounds += 1;
                        self.push(Item::synthetic(format!(
                            "A stop hook reported a problem; address it before finishing:\n{b}"
                        )));
                        continue;
                    }
                }
                return StopReason::Done;
            }

            let stop = self
                .execute_calls(&registry, &completion.tool_calls, cancel)
                .await;
            if cancel.is_cancelled() {
                return StopReason::Interrupted;
            }
            if let Some(r) = stop {
                return r;
            }
        }
    }

    /// Run the configured verify commands if files changed this turn.
    /// Returns feedback for the model when something fails.
    async fn verify(&mut self, rounds: u32, cancel: &CancellationToken) -> Option<String> {
        let cfg = self.shared.cfg();
        if cfg.verify.is_empty() || rounds >= cfg.verify_rounds {
            return None;
        }
        let modified: Vec<_> = std::mem::take(&mut *self.shared.modified.lock())
            .into_iter()
            .collect();
        if modified.is_empty() {
            return None;
        }
        let ctx = self.tool_ctx("verify".into(), cancel.clone(), false);
        let mut failures = vec![];
        for cmd in &cfg.verify {
            self.events.send(AgentEvent::ToolStart {
                id: "verify".into(),
                name: "verify".into(),
                summary: cmd.clone(),
            });
            let res = tools::bash::run_command(
                &ctx,
                cmd,
                std::time::Duration::from_secs(cfg.bash_timeout.max(300)),
            )
            .await;
            let (ok, out) = match res {
                Ok(r) => (r.exit_code == Some(0), r.output),
                Err(e) => (false, e.to_string()),
            };
            self.events.send(AgentEvent::ToolEnd {
                id: "verify".into(),
                name: "verify".into(),
                summary: cmd.clone(),
                result: if ok { "passed".into() } else { "failed".into() },
                is_error: !ok,
                display: None,
            });
            if !ok {
                let (tail, _) = crate::util::head_tail(out.trim_end(), 150, 12_000);
                failures.push(format!("$ {cmd}\n{tail}"));
            }
            if cancel.is_cancelled() {
                return None;
            }
        }
        if failures.is_empty() {
            return None;
        }
        Some(format!(
            "Automatic verification failed after your changes (round {}/{}). Fix the problems below, then finish:\n\n{}",
            rounds + 1,
            cfg.verify_rounds,
            failures.join("\n\n")
        ))
    }

    fn tool_ctx(&self, call_id: String, cancel: CancellationToken, sandbox: bool) -> ToolCtx {
        ToolCtx {
            shared: self.shared.clone(),
            call_id,
            cancel,
            events: self.events.clone(),
            sandbox,
            depth: self.depth,
        }
    }

    /// Execute tool calls: consecutive parallel-safe calls run concurrently,
    /// everything else sequentially, results appended in call order.
    async fn execute_calls(
        &mut self,
        registry: &Registry,
        calls: &[ToolCall],
        cancel: &CancellationToken,
    ) -> Option<StopReason> {
        let mut i = 0;
        let mut outputs: Vec<(ToolCall, ToolOutput)> = vec![];
        while i < calls.len() {
            let tool = registry.get(&calls[i].name);
            let parallel = tool
                .as_ref()
                .map(|t| t.kind().parallel_safe())
                .unwrap_or(true)
                || calls[i].name == "task";
            if parallel {
                let mut j = i;
                while j < calls.len() {
                    let ok = registry
                        .get(&calls[j].name)
                        .map(|t| t.kind().parallel_safe())
                        .unwrap_or(true)
                        || calls[j].name == "task";
                    if !ok {
                        break;
                    }
                    j += 1;
                }
                let batch = &calls[i..j];
                let futs = batch.iter().map(|c| self.exec_one(registry, c, cancel));
                let results = futures_util::future::join_all(futs).await;
                for (c, o) in batch.iter().zip(results) {
                    outputs.push((c.clone(), o));
                }
                i = j;
            } else {
                let o = self.exec_one(registry, &calls[i], cancel).await;
                outputs.push((calls[i].clone(), o));
                i += 1;
            }
            if cancel.is_cancelled() {
                break;
            }
        }
        let mut stop = None;
        for (call, mut out) in outputs {
            if let Some(r) = self.loop_check(&call, &mut out) {
                stop = Some(r);
            }
            self.push(Item::Tool {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content: out.content,
                is_error: out.is_error,
                images: out.images,
                pruned: false,
            });
        }
        // Calls skipped because of an interruption get a result too.
        conversation::repair_dangling_calls(&mut self.items);
        stop
    }

    fn loop_check(&mut self, call: &ToolCall, out: &mut ToolOutput) -> Option<StopReason> {
        let key = hash_str(&format!("{}\0{}", call.name, call.arguments));
        let res = hash_str(&out.content);
        self.recent_calls.push_back((key, res));
        if self.recent_calls.len() > 12 {
            self.recent_calls.pop_front();
        }
        let repeats = self
            .recent_calls
            .iter()
            .filter(|(k, r)| *k == key && *r == res)
            .count();
        if repeats >= 5 {
            self.events.warn(format!(
                "stopping: `{}` was called {repeats} times with identical arguments and results",
                call.name
            ));
            out.content.push_str("\n\n[harness] This exact call has now been repeated 5 times with the same result. The turn was stopped; explain to the user what is blocking you.");
            return Some(StopReason::Error(
                "repeated identical tool calls (loop detected)".into(),
            ));
        }
        if repeats >= 3 {
            out.content.push_str(&format!(
                "\n\n[harness] You have made this exact `{}` call {repeats} times with identical results. Repeating it will not help — change your approach (re-read the relevant code, check your assumptions, or ask for help).",
                call.name
            ));
        }
        None
    }

    async fn exec_one(
        &self,
        registry: &Registry,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let Some(tool) = registry.get(&call.name) else {
            return ToolOutput::err(format!(
                "Unknown tool `{}`. Available tools: {}",
                call.name,
                registry.names().join(", ")
            ));
        };
        let args: Value = match crate::util::parse_json_lenient(&call.arguments) {
            Ok(v) if v.is_object() => v,
            Ok(_) => return ToolOutput::err("Tool arguments must be a JSON object."),
            Err(e) => {
                return ToolOutput::err(format!(
                    "Invalid JSON in tool arguments ({e}). If the arguments were long they may have been cut off by the output limit — send smaller pieces (e.g. write a file in parts)."
                ));
            }
        };
        let summary = tool.summarize(&args);
        self.events.send(AgentEvent::ToolStart {
            id: call.id.clone(),
            name: tool.name().to_string(),
            summary: summary.clone(),
        });

        let cfg = self.shared.cfg();
        let cwd = self.shared.cwd.lock().clone();
        let session_rules = self.shared.session_rules.lock().clone();
        let verdict = Checker {
            mode: self.mode(),
            cfg: &cfg.permissions,
            session_rules: &session_rules,
            root: &self.shared.root,
            cwd: &cwd,
            sandbox_available: self.shared.sandbox_available,
            sandbox_setting: &cfg.sandbox,
        }
        .check(tool.as_ref(), &args);
        let mut sandbox = false;
        let finish = |out: ToolOutput| -> ToolOutput {
            self.events.send(AgentEvent::ToolEnd {
                id: call.id.clone(),
                name: tool.name().to_string(),
                summary: summary.clone(),
                result: out.summary.clone(),
                is_error: out.is_error,
                display: out.display.clone(),
            });
            out
        };
        match verdict {
            Verdict::Allow { sandbox: s } => sandbox = s,
            Verdict::Deny(why) => {
                return finish(ToolOutput::err(format!("Permission denied: {why}.")));
            }
            Verdict::Ask { reason, rule } => {
                if !self.shared.interactive {
                    return finish(ToolOutput::err(format!(
                        "Permission required ({reason}) but the session is non-interactive. The user can allow it with --mode or an allow rule like `{rule}`. Try another approach or report what you need."
                    )));
                }
                let ctx = self.tool_ctx(call.id.clone(), cancel.clone(), false);
                let preview = tool.preview(&ctx, &args);
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.events.send(AgentEvent::Approval(ApprovalRequest {
                    tool: tool.name().to_string(),
                    summary: summary.clone(),
                    reason,
                    preview,
                    rule: rule.clone(),
                    reply: tx,
                }));
                let decision = tokio::select! {
                    d = rx => d.unwrap_or(Decision::Deny(None)),
                    _ = cancel.cancelled() => return finish(ToolOutput::err("Interrupted by user.")),
                };
                match decision {
                    Decision::Allow => {}
                    Decision::AllowSession(r) => self.shared.session_rules.lock().push(r),
                    Decision::AllowAlways(r) => {
                        self.shared.session_rules.lock().push(r.clone());
                        if let Err(e) = crate::config::persist_allow_rule(&self.shared.root, &r) {
                            self.events.warn(format!("could not save rule: {e}"));
                        }
                    }
                    Decision::Deny(fb) => {
                        let msg = match fb {
                            Some(f) if !f.trim().is_empty() => format!("The user denied this action and said: {f}"),
                            _ => "The user denied this action. Do not retry it; adjust your approach or ask what they prefer.".into(),
                        };
                        return finish(ToolOutput::err(msg));
                    }
                }
            }
        }
        if self.depth == 0 || !cfg.hooks.is_empty() {
            let h = crate::hooks::run(&cfg.hooks, "pre_tool", Some(tool.name()), &json!({"tool": tool.name(), "input": args, "session_id": self.shared.session().id}), &cwd).await;
            for w in h.warnings {
                self.events.warn(w);
            }
            if let Some(b) = h.block {
                return finish(ToolOutput::err(format!("Blocked by a pre_tool hook: {b}")));
            }
        }

        let out = if tool.name() == "task" {
            tools::task::run_task(self, &args, &call.id, cancel).await
        } else {
            let ctx = self.tool_ctx(call.id.clone(), cancel.clone(), sandbox);
            tool.run(&ctx, args.clone()).await
        };
        let mut out = out;
        if !cfg.hooks.is_empty() {
            let h = crate::hooks::run(
                &cfg.hooks,
                "post_tool",
                Some(tool.name()),
                &json!({"tool": tool.name(), "input": args, "output": crate::util::ellipsize(&out.content, 20_000), "is_error": out.is_error}),
                &cwd,
            )
            .await;
            for w in h.warnings {
                self.events.warn(w);
            }
            if let Some(b) = h.block {
                out.content
                    .push_str(&format!("\n\n[post_tool hook feedback]\n{b}"));
            }
        }
        finish(out)
    }

    async fn maybe_compact(&mut self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let cfg = self.shared.cfg();
        let window = self.window();
        let reserve = (self.max_tokens() as u64).min(window / 5);
        let limit = ((window.saturating_sub(reserve)) as f64
            * cfg.compact_threshold.clamp(0.3, 0.98)) as u64;
        let ctx = self.context_tokens();
        if ctx < limit {
            return Ok(());
        }
        // Tier 1: prune stale tool output (keeps the conversation shape).
        let before = ctx;
        let protect = (window / 8).max(20_000) as usize;
        let freed = compact::prune_tool_outputs(
            &mut self.items,
            protect,
            (window / 20).max(10_000) as usize,
        );
        if freed > 0 {
            self.record(Record::Compaction {
                items: self.items.clone(),
                summary: String::new(),
            });
            self.last_context = self.last_context.saturating_sub(freed as u64);
            let after = self.context_tokens();
            self.events.send(AgentEvent::Compacted { before, after });
            if after < limit * 85 / 100 {
                return Ok(());
            }
        }
        // Tier 2: summarize.
        self.compact(None, cancel).await
    }

    /// Summarize the conversation and replace history with the summary.
    pub async fn compact(
        &mut self,
        instructions: Option<&str>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        if self.items.len() < 2 {
            return Ok(());
        }
        let before = self.context_tokens();
        self.events.notice("compacting conversation…");
        let cfg = self.shared.cfg();
        let prompt_text = compact::summary_instructions(instructions);
        conversation::repair_dangling_calls(&mut self.items);

        // Preferred path: same prefix (cache hit) + summary request.
        let mut summary = String::new();
        if cfg.compact_model().is_empty() || cfg.compact_model() == self.model {
            let registry = self.registry();
            let mut req = self.build_request(&registry);
            let msg = json!({"role": "user", "content": prompt_text});
            req.messages.push(msg);
            req.max_tokens = Some(12_000.min(self.max_tokens().max(4000)));
            match self.shared.client.complete(&req, cancel).await {
                Ok(c) => {
                    self.usage.add(&c.usage);
                    self.shared.total_usage.lock().add(&c.usage);
                    self.record(Record::Usage {
                        usage: c.usage.clone(),
                        model: c.model.clone(),
                    });
                    summary = c.text;
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        anyhow::bail!("interrupted");
                    }
                    self.events.warn(format!(
                        "in-context summary failed ({e}); using transcript summary"
                    ));
                }
            }
        }
        // Fallback: flattened transcript to the compaction model.
        if summary.trim().len() < 40 {
            let model = cfg.compact_model().to_string();
            let info = self.shared.catalog().get(&model).cloned();
            let budget_chars =
                (info.map(|i| i.context_length).unwrap_or(128_000) as usize * 3).min(1_500_000) * 6
                    / 10;
            let t = compact::transcript(&self.items, budget_chars);
            let req = ChatRequest {
                model,
                messages: vec![
                    json!({"role": "system", "content": "You summarize coding-agent sessions so the agent can continue seamlessly."}),
                    json!({"role": "user", "content": format!("<transcript>\n{t}\n</transcript>\n\n{prompt_text}")}),
                ],
                max_tokens: Some(12_000),
                ..Default::default()
            };
            let c = self.shared.client.complete(&req, cancel).await?;
            self.usage.add(&c.usage);
            self.shared.total_usage.lock().add(&c.usage);
            summary = c.text;
        }
        if summary.trim().is_empty() {
            anyhow::bail!("empty summary");
        }
        let todos = self.shared.todos.lock().clone();
        let modified: Vec<String> = self
            .shared
            .checkpoints
            .lock()
            .files_since(0)
            .iter()
            .map(|p| crate::util::display_path(&self.shared.root, p))
            .collect();
        let tail_from = compact::tail_start(&self.items, (self.window() / 10).min(30_000) as usize);
        let last_user_idx = self.items.iter().rposition(|i| i.is_real_user());
        let last_user = match last_user_idx {
            Some(i) if i < tail_from => match &self.items[i] {
                Item::User { content, .. } => Some(content.clone()),
                _ => None,
            },
            _ => None,
        };
        let mut new_items = vec![Item::synthetic(compact::continuation_message(
            &summary,
            &todos,
            &modified,
            last_user.as_deref(),
        ))];
        new_items.extend(self.items[tail_from..].iter().cloned());
        // A tail that starts with an assistant message must follow a user message — it does (the summary).
        self.items = new_items;
        self.record(Record::Compaction {
            items: self.items.clone(),
            summary: summary.clone(),
        });
        self.last_context = 0;
        self.items_at_last = 0;
        // The model no longer has file contents in view.
        *self.shared.files.lock() = shared::FileTracker::default();
        let after = self.context_tokens();
        self.events.send(AgentEvent::Compacted { before, after });
        Ok(())
    }

    fn spawn_title(&self, first: String) {
        let shared = self.shared.clone();
        let persist = self.persist;
        tokio::spawn(async move {
            let cfg = shared.cfg();
            let req = ChatRequest {
                model: cfg.small_model.clone(),
                messages: vec![
                    json!({"role": "system", "content": "Write a 3-7 word title for a coding session that starts with the user's message below. Reply with the title only — no quotes, no trailing period."}),
                    json!({"role": "user", "content": crate::util::ellipsize(&first, 2000)}),
                ],
                max_tokens: Some(40),
                ..Default::default()
            };
            if let Ok(c) = shared
                .client
                .complete(&req, &CancellationToken::new())
                .await
            {
                let t = c
                    .text
                    .trim()
                    .trim_matches('"')
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !t.is_empty() && persist {
                    shared.session().append(&Record::Title { title: t.clone() });
                    *shared.title.lock() = Some(t);
                }
                shared.total_usage.lock().add(&c.usage);
            }
        });
    }

    /// Rewind to before user turn `turn` (0-based): restores files and
    /// truncates history. Returns the removed user message text.
    pub fn rewind(
        &mut self,
        turn: usize,
        restore_files: bool,
    ) -> anyhow::Result<(Option<String>, Vec<std::path::PathBuf>)> {
        let mut seen = 0;
        let mut cut = None;
        for (i, it) in self.items.iter().enumerate() {
            if it.is_real_user() {
                if seen == turn {
                    cut = Some(i);
                    break;
                }
                seen += 1;
            }
        }
        let Some(cut) = cut else {
            anyhow::bail!("no such turn")
        };
        let text = match &self.items[cut] {
            Item::User { content, .. } => Some(content.clone()),
            _ => None,
        };
        let restored = if restore_files {
            self.shared.restore_to(turn)?
        } else {
            vec![]
        };
        self.items.truncate(cut);
        self.record(Record::Truncate { len: cut });
        self.last_context = 0;
        self.items_at_last = 0;
        Ok((text, restored))
    }

    /// Load history from a replayed session.
    pub fn restore(&mut self, items: Vec<Item>) {
        self.items = items;
        conversation::repair_dangling_calls(&mut self.items);
        self.title_done = true;
        self.last_context = 0;
        self.items_at_last = 0;
    }

    pub fn invalidate_system_prompt(&mut self) {
        self.system_cache = None;
    }
}

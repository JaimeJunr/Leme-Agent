//! Slash commands.

use super::style::{Line, Style, theme};
use super::{App, PickItem, PickKind};
use crate::conversation::Item;
use crate::permissions::Mode;
use crate::util::{fmt_cost, fmt_tokens};
use base64::Engine;

pub const BUILTIN: &[(&str, &str)] = &[
    ("help", "Show commands and keyboard shortcuts"),
    (
        "model",
        "Switch model — fuzzy search the whole OpenRouter catalog",
    ),
    (
        "effort",
        "Reasoning effort: none|minimal|low|medium|high|xhigh|max",
    ),
    (
        "mode",
        "Permission mode: default|accept-edits|auto|yolo|plan",
    ),
    (
        "plan",
        "Plan mode: investigate read-only, then propose a plan",
    ),
    (
        "compact",
        "Summarize the conversation to free context [focus]",
    ),
    ("clear", "Start a fresh session"),
    ("resume", "Resume a previous session"),
    ("fork", "Fork this conversation into a new session"),
    (
        "rewind",
        "Rewind conversation and files to an earlier message",
    ),
    ("undo", "Undo the last turn (files + conversation)"),
    ("diff", "Show uncommitted changes (git diff)"),
    ("review", "Review uncommitted changes for bugs"),
    ("init", "Create or improve AGENTS.md for this project"),
    ("cost", "Token usage, cache hits and cost"),
    ("context", "Context window usage breakdown"),
    ("todo", "Show the task list"),
    ("export", "Export the conversation to Markdown [path]"),
    ("copy", "Copy the last response to the clipboard"),
    ("tools", "List available tools"),
    ("mcp", "MCP servers and their tools"),
    ("skills", "List skills"),
    ("agents", "List subagent types"),
    ("jobs", "List background jobs"),
    ("status", "Session, model, key credits and sandbox status"),
    ("config", "Show configuration file locations"),
    ("verbose", "Toggle verbose output (ctrl+o)"),
    ("quit", "Exit harness"),
];

const RUNNING_OK: &[&str] = &[
    "help", "cost", "context", "todo", "mode", "verbose", "status", "tools", "mcp", "skills",
    "agents", "jobs", "config", "copy", "diff", "quit", "exit",
];

pub fn is_command(app: &App, name: &str) -> bool {
    let n = name.to_lowercase();
    BUILTIN.iter().any(|(c, _)| *c == n)
        || matches!(
            n.as_str(),
            "exit" | "q" | "new" | "sessions" | "models" | "?" | "reset"
        )
        || app.shared.ext.commands.iter().any(|c| c.name == name)
}

pub async fn run(app: &mut App, name: &str, args: &str) {
    let n = name.to_lowercase();
    if app.running() && !RUNNING_OK.contains(&n.as_str()) {
        app.warn(&format!(
            "/{n} is not available while the agent is working (esc to interrupt)"
        ));
        return;
    }
    match n.as_str() {
        "help" | "?" => help(app),
        "model" | "models" => model(app, args),
        "effort" => effort(app, args),
        "mode" => {
            if args.is_empty() {
                let items = ["default", "accept-edits", "auto", "yolo", "plan"]
                    .iter()
                    .map(|m| PickItem { label: m.to_string(), detail: mode_desc(m).into(), value: m.to_string() })
                    .collect();
                app.open_picker("Permission mode", PickKind::Mode, items, 0);
            } else {
                match Mode::parse(args) {
                    Some(m) => app.set_mode(m),
                    None => app.warn("unknown mode (default|accept-edits|auto|yolo|plan)"),
                }
            }
        }
        "plan" => {
            app.set_mode(Mode::Plan);
            if !args.is_empty() {
                app.send(args.to_string());
            }
        }
        "compact" => compact(app, args).await,
        "clear" | "new" | "reset" => clear(app),
        "resume" | "sessions" => resume(app),
        "fork" => fork(app),
        "rewind" => open_rewind(app),
        "undo" => undo(app),
        "diff" => diff(app).await,
        "review" => app.send(format!(
            "Review the uncommitted changes in this repository (`git status`, `git diff HEAD`, and read surrounding code as needed). Report real problems only — bugs, edge cases, security issues, missing error handling, broken tests, and deviations from the codebase's conventions — ranked by severity, each with `path:line` and a concrete fix. Do not modify files.{}",
            if args.is_empty() { String::new() } else { format!(" Focus: {args}") }
        )),
        "init" => app.send(INIT_PROMPT.to_string() + if args.is_empty() { "" } else { args }),
        "cost" => cost(app).await,
        "context" => context(app),
        "todo" | "todos" => {
            let todos = app.shared.todos.lock().clone();
            if todos.is_empty() {
                app.info("no tasks");
            } else {
                app.info(&crate::tools::todo::render(&todos));
            }
        }
        "export" => export(app, args),
        "copy" => copy(app),
        "tools" => {
            let names = app.agent_mut().map(|a| a.registry().names()).unwrap_or_default();
            app.info(&format!("{} tools: {}", names.len(), names.join(", ")));
        }
        "mcp" => mcp(app),
        "skills" => {
            let s: Vec<String> = app.shared.ext.skills.iter().map(|s| format!("{} — {}", s.name, crate::util::ellipsize(&s.description, 100))).collect();
            app.info(&if s.is_empty() { "no skills (add them in .harness/skills/<name>/SKILL.md)".into() } else { s.join("\n") });
        }
        "agents" => {
            let s: Vec<String> = app
                .shared
                .ext
                .agents
                .iter()
                .map(|a| format!("{}{} — {}", a.name, if a.builtin { " (built-in)" } else { "" }, crate::util::ellipsize(&a.description, 100)))
                .collect();
            app.info(&s.join("\n"));
        }
        "jobs" => {
            let n = app.shared.jobs.running();
            app.info(&format!("{n} running background job(s). Ask the agent to use the `jobs` tool to inspect them."));
        }
        "status" => status(app).await,
        "config" => {
            let root = app.shared.root.clone();
            let mut s = String::from("config layers (low → high):\n");
            for p in crate::config::config_layers(&root) {
                s.push_str(&format!("  {} {}\n", if p.exists() { "●" } else { "○" }, p.display()));
            }
            s.push_str(&format!("sessions: {}\n", crate::session::sessions_dir(&root).display()));
            s.push_str("run `harness config init` to create a commented template");
            app.info(&s);
        }
        "verbose" => {
            app.verbose = !app.verbose;
            app.info(if app.verbose { "verbose on" } else { "verbose off" });
        }
        "quit" | "exit" | "q" => app.quit = true,
        _ => custom(app, name, args).await,
    }
}

fn mode_desc(m: &str) -> &'static str {
    match m {
        "default" => "ask before edits and non-read-only commands",
        "accept-edits" => "edits automatic, commands ask",
        "auto" => "everything automatic, commands sandboxed, destructive ones ask",
        "yolo" => "no questions, no sandbox",
        "plan" => "read-only investigation, then a plan for approval",
        _ => "",
    }
}

fn help(app: &mut App) {
    let mut s = String::from("**Commands**\n");
    for (c, d) in BUILTIN {
        s.push_str(&format!("- `/{c}` — {d}\n"));
    }
    if !app.shared.ext.commands.is_empty() {
        s.push_str("\n**Custom commands**\n");
        for c in &app.shared.ext.commands {
            s.push_str(&format!(
                "- `/{}` {} — {}\n",
                c.name, c.argument_hint, c.description
            ));
        }
    }
    s.push_str(
        "\n**Input**\n- `@path` attach a file or image · `!cmd` run a shell command (output goes to context) · `# note` save to AGENTS.md\n- enter send · shift+enter / alt+enter / ctrl+j / `\\` newline · ↑↓ history · tab complete\n\n**Keys**\n- esc interrupt · esc esc rewind · shift+tab cycle mode · ctrl+o verbose · ctrl+c clear/interrupt/exit · ctrl+l redraw\n- Messages typed while the agent works are delivered at its next step.",
    );
    app.markdown(&s);
}

fn model(app: &mut App, args: &str) {
    let catalog = app.shared.catalog();
    if !args.is_empty() && (args.contains('/') || catalog.get(args).is_some()) {
        set_model(app, args.to_string());
        return;
    }
    let mut ms = catalog.search(args);
    ms.retain(|m| m.supports_tools);
    if ms.is_empty() {
        if !args.is_empty() {
            set_model(app, args.to_string());
        } else {
            app.warn("model catalog unavailable; use /model <provider/model-id>");
        }
        return;
    }
    let current = app.model.clone();
    let items: Vec<PickItem> = ms
        .iter()
        .map(|m| PickItem {
            label: m.id.clone(),
            detail: format!(
                "{} ctx · ${:.2}/${:.2} per M{}",
                fmt_tokens(m.context_length),
                m.price_in * 1e6,
                m.price_out * 1e6,
                if m.supports_reasoning {
                    " · reasoning"
                } else {
                    ""
                }
            ),
            value: m.id.clone(),
        })
        .collect();
    let sel = items.iter().position(|i| i.value == current).unwrap_or(0);
    app.open_picker("Switch model (type to filter)", PickKind::Model, items, sel);
    if let Some(p) = app.picker.as_mut() {
        p.filter = String::new();
    }
}

fn set_model(app: &mut App, id: String) {
    let known = app.shared.catalog().get(&id).cloned();
    if let Some(a) = app.agent_mut() {
        a.model = id.clone();
        a.invalidate_system_prompt();
        if a.persist {
            a.shared
                .session()
                .append(&crate::session::Record::Model { model: id.clone() });
        }
    }
    app.model = id.clone();
    match known {
        Some(m) => {
            let mut s = format!("model → {} ({} context", m.id, fmt_tokens(m.context_length));
            if m.supports_reasoning && !m.reasoning_efforts.is_empty() {
                s.push_str(&format!(", efforts: {}", m.reasoning_efforts.join("/")));
            }
            s.push(')');
            app.info(&s);
        }
        None => app.warn(&format!(
            "model → {id} (not found in the catalog; requests may fail)"
        )),
    }
}

fn effort(app: &mut App, args: &str) {
    if args.is_empty() {
        let efforts = app
            .shared
            .catalog()
            .get(&app.model)
            .map(|m| m.reasoning_efforts.clone())
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| {
                vec![
                    "none".into(),
                    "minimal".into(),
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                    "max".into(),
                ]
            });
        let mut items: Vec<PickItem> = vec![PickItem {
            label: "default".into(),
            detail: "model default".into(),
            value: String::new(),
        }];
        items.extend(efforts.into_iter().map(|e| PickItem {
            label: e.clone(),
            detail: String::new(),
            value: e,
        }));
        app.open_picker("Reasoning effort", PickKind::Effort, items, 0);
        return;
    }
    let e = if args == "default" {
        String::new()
    } else {
        args.to_lowercase()
    };
    if let Some(a) = app.agent_mut() {
        a.effort = e.clone();
    }
    app.effort = e.clone();
    app.info(&format!(
        "reasoning effort → {}",
        if e.is_empty() { "model default" } else { &e }
    ));
}

async fn compact(app: &mut App, args: &str) {
    // Runs in the background like a turn: the UI stays live and esc cancels.
    let instructions = args.to_string();
    app.start_task("Compacting context", move |mut agent, cancel| async move {
        let focus = if instructions.is_empty() {
            None
        } else {
            Some(instructions.as_str())
        };
        if let Err(e) = agent.compact(focus, &cancel).await
            && !cancel.is_cancelled()
        {
            agent.events.warn(format!("compaction failed: {e:#}"));
        }
        agent
    });
}

fn clear(app: &mut App) {
    let Some(agent) = app.agent.as_mut() else {
        return;
    };
    match crate::session::Session::create(&app.shared.root, &agent.model, None) {
        Ok(s) => {
            *app.shared.session.write() = std::sync::Arc::new(s);
            agent.restore(vec![]);
            agent.usage = Default::default();
            app.shared.todos.lock().clear();
            app.shared.checkpoints.lock().entries.clear();
            *app.shared.total_usage.lock() = Default::default();
            *app.shared.title.lock() = None;
            app.todos.clear();
            app.usage = Default::default();
            app.context = (0, 0);
            app.info(&format!("new session {}", app.shared.session().id));
        }
        Err(e) => app.error(&format!("could not create session: {e}")),
    }
}

fn resume(app: &mut App) {
    let list = crate::session::list(&app.shared.root);
    if list.is_empty() {
        app.info("no previous sessions for this project");
        return;
    }
    let items = list
        .iter()
        .map(|s| {
            let age = s
                .modified
                .elapsed()
                .map(|d| crate::util::fmt_duration(d) + " ago")
                .unwrap_or_default();
            PickItem {
                label: crate::util::ellipsize(&s.title, 60),
                detail: format!("{age} · {} msgs · {}", s.messages, fmt_cost(s.cost)),
                value: s.path.display().to_string(),
            }
        })
        .collect();
    app.open_picker("Resume session", PickKind::Resume, items, 0);
}

fn load_session(app: &mut App, path: &str) {
    let session = match crate::session::Session::open(std::path::Path::new(path)) {
        Ok(s) => s,
        Err(e) => return app.error(&format!("{e}")),
    };
    let rep = match session.replay() {
        Ok(r) => r,
        Err(e) => return app.error(&format!("{e}")),
    };
    *app.shared.session.write() = std::sync::Arc::new(session);
    *app.shared.todos.lock() = rep.todos.clone();
    app.shared.checkpoints.lock().entries = rep.checkpoints.clone();
    app.shared.next_turn.store(
        crate::session::next_turn_id(&rep),
        std::sync::atomic::Ordering::SeqCst,
    );
    *app.shared.total_usage.lock() = rep.usage.clone();
    *app.shared.title.lock() = rep.title.clone();
    app.todos = rep.todos.clone();
    app.usage = rep.usage.clone();
    if let Some(a) = app.agent.as_mut() {
        a.restore(rep.items.clone());
        a.usage = rep.usage.clone();
        if let Some(m) = &rep.model {
            a.model = m.clone();
            a.invalidate_system_prompt();
        }
        app.model = a.model.clone();
    }
    let n = rep.items.iter().filter(|i| i.is_real_user()).count();
    app.info(&format!(
        "resumed {} — {} ({n} messages)",
        app.shared.session().id,
        rep.title.unwrap_or_default()
    ));
    if let Some(t) = rep.items.iter().rev().find_map(|i| match i {
        Item::Assistant { text, .. } if !text.is_empty() => Some(text.clone()),
        _ => None,
    }) {
        let snippet: String = t
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        app.markdown(&snippet);
    }
}

fn fork(app: &mut App) {
    let Some(agent) = app.agent.as_ref() else {
        return;
    };
    let items = agent.items.clone();
    let model = agent.model.clone();
    let old = app.shared.session();
    match old.fork(&app.shared.root, &model, &items) {
        Ok(s) => {
            let id = s.id.clone();
            *app.shared.session.write() = std::sync::Arc::new(s);
            app.info(&format!(
                "forked {} → {id} (the original session is unchanged)",
                old.id
            ));
        }
        Err(e) => app.error(&format!("fork failed: {e}")),
    }
}

pub fn open_rewind(app: &mut App) {
    let Some(agent) = app.agent.as_ref() else {
        app.warn("wait for the agent to finish (esc to interrupt)");
        return;
    };
    let users: Vec<(usize, String)> = agent
        .items
        .iter()
        .filter_map(|i| match i {
            Item::User {
                content,
                turn: Some(t),
                ..
            } => Some((*t, content.clone())),
            _ => None,
        })
        .collect();
    if users.is_empty() {
        app.info("nothing to rewind");
        return;
    }
    let ck = app.shared.checkpoints.lock();
    let items: Vec<PickItem> = users
        .iter()
        .rev()
        .map(|(i, u)| {
            let i = *i;
            let files = ck.files_since(i).len();
            PickItem {
                label: crate::util::first_line(u, 70),
                detail: if files > 0 {
                    format!("restores {files} file(s)")
                } else {
                    String::new()
                },
                value: i.to_string(),
            }
        })
        .collect();
    drop(ck);
    app.open_picker(
        "Rewind to before… (conversation + files)",
        PickKind::Rewind,
        items,
        0,
    );
}

fn rewind_to(app: &mut App, turn: usize) {
    let Some(agent) = app.agent.as_mut() else {
        return;
    };
    match agent.rewind(turn, true) {
        Ok((text, restored)) => {
            let files: Vec<String> = restored
                .iter()
                .map(|p| crate::util::display_path(&app.shared.root, p))
                .collect();
            app.info(&format!(
                "rewound to before that message{}",
                if files.is_empty() {
                    String::new()
                } else {
                    format!("; restored {}", files.join(", "))
                }
            ));
            if let Some(t) = text {
                app.composer.set(&t);
            }
        }
        Err(e) => app.error(&format!("rewind failed: {e}")),
    }
}

fn undo(app: &mut App) {
    let last = app
        .agent
        .as_ref()
        .and_then(|a| a.items.iter().rev().find_map(|i| i.turn()));
    match last {
        Some(t) => rewind_to(app, t),
        None => app.info("nothing to undo"),
    }
}

async fn diff(app: &mut App) {
    let root = app.shared.root.clone();
    let out = tokio::process::Command::new("git")
        .args(["--no-pager", "diff", "HEAD", "--stat"])
        .current_dir(&root)
        .output()
        .await;
    let full = tokio::process::Command::new("git")
        .args(["--no-pager", "diff", "HEAD"])
        .current_dir(&root)
        .output()
        .await;
    match (out, full) {
        (Ok(o), Ok(f)) if o.status.success() => {
            let stat = String::from_utf8_lossy(&o.stdout).to_string();
            let body = String::from_utf8_lossy(&f.stdout).to_string();
            if body.trim().is_empty() {
                app.info("no uncommitted changes");
                return;
            }
            app.info(stat.trim_end());
            let limit = if app.verbose { 2000 } else { 200 };
            let lines: Vec<&str> = body.lines().collect();
            for l in lines.iter().take(limit) {
                let st = if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") {
                    Style::default().bold()
                } else if l.starts_with('+') {
                    Style::fg(theme::OK)
                } else if l.starts_with('-') {
                    Style::fg(theme::ERR)
                } else if l.starts_with("@@") {
                    Style::fg(theme::ACCENT)
                } else {
                    Style::fg(theme::MUTED)
                };
                app.print(Line::styled(format!("  {l}"), st));
            }
            if lines.len() > limit {
                app.info(&format!(
                    "… {} more lines (ctrl+o then /diff for all)",
                    lines.len() - limit
                ));
            }
        }
        _ => app.warn("not a git repository (or git failed)"),
    }
}

async fn cost(app: &mut App) {
    let u = app.shared.total_usage.lock().clone();
    let hit = if u.prompt_tokens > 0 {
        u.cached_tokens as f64 / u.prompt_tokens as f64 * 100.0
    } else {
        0.0
    };
    let mut s = format!(
        "session cost {}\n  input {} tokens ({} cached, {:.0}% hit · {} cache writes)\n  output {} tokens ({} reasoning)",
        fmt_cost(u.cost),
        fmt_tokens(u.prompt_tokens),
        fmt_tokens(u.cached_tokens),
        hit,
        fmt_tokens(u.cache_write_tokens),
        fmt_tokens(u.completion_tokens),
        fmt_tokens(u.reasoning_tokens)
    );
    if let Ok(k) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.shared.client.key_info(),
    )
    .await
        && let Ok(k) = k
    {
        let used = k.get("usage").and_then(|v| v.as_f64()).unwrap_or(0.0);
        match k.get("limit").and_then(|v| v.as_f64()) {
            Some(limit) => s.push_str(&format!(
                "\n  key: {} used of {} limit",
                fmt_cost(used),
                fmt_cost(limit)
            )),
            None => s.push_str(&format!("\n  key: {} used (no limit)", fmt_cost(used))),
        }
    }
    app.info(&s);
}

fn context(app: &mut App) {
    let Some(agent) = app.agent.as_mut() else {
        return;
    };
    let system = agent.system_prompt();
    let tools: usize = agent
        .registry()
        .definitions()
        .iter()
        .map(|d| crate::util::estimate_tokens(&d.to_string()))
        .sum();
    let sys = crate::util::estimate_tokens(&system);
    let (mut user, mut asst, mut tool) = (0usize, 0usize, 0usize);
    for i in &agent.items {
        match i {
            Item::User { .. } => user += i.estimate_tokens(),
            Item::Assistant { .. } => asst += i.estimate_tokens(),
            Item::Tool { .. } => tool += i.estimate_tokens(),
        }
    }
    let window = agent.window();
    let total = agent.context_tokens();
    let bar = |n: usize| -> String {
        let w = ((n as f64 / window as f64) * 40.0).ceil() as usize;
        "█".repeat(w.min(40))
    };
    let s = format!(
        "context {} / {} ({:.0}%)\n  system prompt  {:>7}  {}\n  tool schemas   {:>7}  {}\n  user           {:>7}  {}\n  assistant      {:>7}  {}\n  tool results   {:>7}  {}\n  auto-compact at {:.0}% of the window",
        fmt_tokens(total),
        fmt_tokens(window),
        total as f64 / window as f64 * 100.0,
        fmt_tokens(sys as u64),
        bar(sys),
        fmt_tokens(tools as u64),
        bar(tools),
        fmt_tokens(user as u64),
        bar(user),
        fmt_tokens(asst as u64),
        bar(asst),
        fmt_tokens(tool as u64),
        bar(tool),
        app.shared.cfg().compact_threshold * 100.0
    );
    app.info(&s);
}

fn export(app: &mut App, args: &str) {
    let Some(agent) = app.agent.as_ref() else {
        return;
    };
    let title = app
        .shared
        .title
        .lock()
        .clone()
        .unwrap_or_else(|| "harness session".into());
    let md = crate::session::to_markdown(&agent.items, &title);
    let path = if args.is_empty() {
        format!("harness-{}.md", app.shared.session().id)
    } else {
        args.to_string()
    };
    let cwd = app.shared.cwd.lock().clone();
    let full = crate::util::resolve_path(&cwd, &path);
    match std::fs::write(&full, md) {
        Ok(_) => app.info(&format!("exported to {}", full.display())),
        Err(e) => app.error(&format!("export failed: {e}")),
    }
}

fn copy(app: &mut App) {
    let text = app.agent.as_ref().and_then(|a| {
        a.items.iter().rev().find_map(|i| match i {
            Item::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
    });
    match text {
        Some(t) => {
            // OSC 52: works in most terminals, including over SSH/tmux.
            let b64 = base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
            app.screen.raw(&format!("\x1b]52;c;{b64}\x07"));
            app.info(&format!(
                "copied {} characters to the clipboard",
                t.chars().count()
            ));
        }
        None => app.info("nothing to copy yet"),
    }
}

fn mcp(app: &mut App) {
    let cfg = app.shared.cfg();
    if cfg.mcp.is_empty() {
        app.info(
            "no MCP servers configured (add [mcp.<name>] to .harness/config.toml or a .mcp.json)",
        );
        return;
    }
    let mut s = String::new();
    if let Some(m) = &app.shared.mcp {
        for srv in &m.servers {
            let tools: Vec<&str> = m
                .tools
                .iter()
                .filter(|t| t.server == srv.name)
                .map(|t| t.name.as_str())
                .collect();
            s.push_str(&format!(
                "● {} — {} tools: {}\n",
                srv.name,
                tools.len(),
                tools.join(", ")
            ));
        }
        for e in &m.errors {
            s.push_str(&format!("✗ {e}\n"));
        }
    } else {
        s.push_str("MCP disabled for this session (--no-mcp)");
    }
    app.info(s.trim_end());
}

async fn status(app: &mut App) {
    let cfg = app.shared.cfg();
    let mut s = format!(
        "harness v{}\n  session {}\n  project {}\n  model {} · small {} · oracle {}\n  mode {} · sandbox {}\n  instructions: {}",
        env!("CARGO_PKG_VERSION"),
        app.shared.session().id,
        app.shared.root.display(),
        app.model,
        cfg.small_model,
        cfg.oracle_model,
        app.shared.mode.read().name(),
        if app.shared.sandbox_available {
            "available"
        } else {
            "unavailable"
        },
        if app.shared.instructions.is_empty() {
            "none (try /init)".to_string()
        } else {
            app.shared
                .instructions
                .iter()
                .map(|(p, _)| crate::util::display_path(&app.shared.root, p))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    if let Ok(Ok(k)) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.shared.client.key_info(),
    )
    .await
    {
        let label = k.get("label").and_then(|v| v.as_str()).unwrap_or("");
        let used = k.get("usage").and_then(|v| v.as_f64()).unwrap_or(0.0);
        s.push_str(&format!("\n  api key {label} · {} used", fmt_cost(used)));
        if let Some(l) = k.get("limit").and_then(|v| v.as_f64()) {
            s.push_str(&format!(" / {} limit", fmt_cost(l)));
        }
    }
    app.info(&s);
}

async fn custom(app: &mut App, name: &str, args: &str) {
    let Some(c) = app
        .shared
        .ext
        .commands
        .iter()
        .find(|c| c.name == name)
        .cloned()
    else {
        app.warn(&format!("unknown command /{name}"));
        return;
    };
    let cwd = app.shared.cwd.lock().clone();
    let body = crate::extensions::expand_command(&c.body, args, &cwd).await;
    if let Some(m) = &c.model {
        let model = crate::tools::task::resolve_model(Some(m), &app.model, &app.shared.cfg());
        if let Some(a) = app.agent_mut()
            && a.model != model
        {
            a.model = model.clone();
            a.invalidate_system_prompt();
            app.model = model;
        }
    }
    app.send(body);
}

/// `!cmd`: run a shell command directly; output is shown and added to context.
pub async fn shell(app: &mut App, cmd: &str) {
    if cmd.is_empty() {
        return;
    }
    app.print(
        Line::styled("! ", Style::fg(theme::WARN).bold())
            .with(cmd.to_string(), Style::default().bold()),
    );
    app.draw_now();
    let cwd = app.shared.cwd.lock().clone();
    let out = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(format!("{cmd}\n") + "__ec=$?; exit $__ec")
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    let (text, code) = match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (
                crate::tools::bash::strip_ansi(&s),
                o.status.code().unwrap_or(-1),
            )
        }
        Err(e) => (e.to_string(), -1),
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .len()
        .saturating_sub(if app.verbose { 500 } else { 40 });
    if start > 0 {
        app.print(Line::styled(
            format!("  … {start} lines"),
            Style::fg(theme::MUTED),
        ));
    }
    for l in &lines[start..] {
        app.print(Line::styled(format!("  {l}"), Style::fg(theme::MUTED)));
    }
    if code != 0 {
        app.print(Line::styled(
            format!("  exit {code}"),
            Style::fg(theme::ERR),
        ));
    }
    let (body, _) = crate::util::head_tail(&text, 300, 20_000);
    if let Some(a) = app.agent_mut() {
        a.push(Item::User {
            content: format!("[I ran a shell command myself]\n$ {cmd}\n{body}\n[exit {code}]"),
            images: vec![],
            synthetic: false,
            turn: None,
        });
    }
}

/// `# note`: append to the project's AGENTS.md (persistent memory).
pub fn remember(app: &mut App, note: &str) {
    let root = app.shared.root.clone();
    let path = crate::instructions::NAMES
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())
        .unwrap_or_else(|| root.join("AGENTS.md"));
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("- {note}\n"));
    match std::fs::write(&path, content) {
        Ok(_) => app.info(&format!(
            "saved to {} (applies from the next session; the agent was told now)",
            crate::util::display_path(&root, &path)
        )),
        Err(e) => app.error(&format!("could not write {}: {e}", path.display())),
    }
    if let Some(a) = app.agent_mut() {
        a.push(Item::User {
            content: format!("[Remember for the rest of this project] {note}"),
            images: vec![],
            synthetic: true,
            turn: None,
        });
    }
}

pub async fn picked(app: &mut App, kind: PickKind, value: String) {
    match kind {
        PickKind::Model => set_model(app, value),
        PickKind::Effort => effort(app, if value.is_empty() { "default" } else { &value }),
        PickKind::Mode => {
            if let Some(m) = Mode::parse(&value) {
                app.set_mode(m);
            }
        }
        PickKind::Resume => load_session(app, &value),
        PickKind::Rewind => {
            if let Ok(t) = value.parse::<usize>() {
                rewind_to(app, t);
            }
        }
    }
}

const INIT_PROMPT: &str = "Analyze this codebase and create an AGENTS.md file at the project root to guide AI coding agents working here (if AGENTS.md or CLAUDE.md already exists, improve it instead of starting over). Investigate first: README, build/package manifests, CI config, linters/formatters, test setup, directory layout, and a few representative source files. Include only what is useful and specific to this repo:\n- Build, lint, format and test commands (including how to run a single test)\n- High-level architecture: the main components, how they fit together, where things live\n- Code style and conventions actually used (naming, error handling, patterns, imports)\n- Gotchas: generated files, required env vars, things that must not be edited\nKeep it concise (under ~150 lines), no generic advice, no invented commands. ";

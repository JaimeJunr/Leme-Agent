//! Bootstrapping: config, catalog, MCP, session and the main agent.

use crate::agent::Agent;
use crate::agent::events::{AgentEvent, EventSink};
use crate::agent::shared::{Checkpoints, FileTracker, Shared};
use crate::config::Config;
use crate::extensions::Extensions;
use crate::llm::{Catalog, LlmClient, Usage};
use crate::mcp::McpManager;
use crate::permissions::Mode;
use crate::session::Session;
use anyhow::{Context, Result, bail};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Debug, Clone, Default)]
pub enum Resume {
    #[default]
    New,
    Last,
    Id(String),
}

#[derive(Debug, Clone, Default)]
pub struct BootOptions {
    pub model: Option<String>,
    pub mode: Option<String>,
    pub effort: Option<String>,
    pub interactive: bool,
    pub resume: Resume,
    pub no_mcp: bool,
    pub max_cost: Option<f64>,
    pub max_steps: Option<u32>,
    pub verify: Vec<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub disabled_tools: Vec<String>,
    pub append_system: Option<String>,
    pub cwd: Option<PathBuf>,
}

pub struct Booted {
    pub agent: Agent,
    pub rx: UnboundedReceiver<AgentEvent>,
    pub warnings: Vec<String>,
    pub resumed: bool,
}

pub fn load_config(opts: &BootOptions) -> Result<(PathBuf, PathBuf, Config)> {
    let cwd = match &opts.cwd {
        Some(c) => c.clone(),
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let root = crate::config::project_root(&cwd);
    let mut cfg = Config::load(&root)?;
    if let Some(m) = &opts.model {
        cfg.model = m.clone();
    }
    if let Some(m) = &opts.mode {
        cfg.mode = m.clone();
    }
    if let Some(e) = &opts.effort {
        cfg.effort = e.clone();
    }
    if let Some(c) = opts.max_cost {
        cfg.max_cost = c;
    }
    if let Some(s) = opts.max_steps {
        cfg.max_steps = s;
    }
    if !opts.verify.is_empty() {
        cfg.verify = opts.verify.clone();
    }
    cfg.permissions.allow.extend(opts.allow.iter().cloned());
    cfg.permissions.deny.extend(opts.deny.iter().cloned());
    cfg.disabled_tools
        .extend(opts.disabled_tools.iter().cloned());
    if let Some(a) = &opts.append_system {
        cfg.instructions = Some(match cfg.instructions.take() {
            Some(i) => format!("{i}\n{a}"),
            None => a.clone(),
        });
    }
    Ok((cwd, root, cfg))
}

pub async fn load_catalog(client: &LlmClient, warnings: &mut Vec<String>) -> Catalog {
    let (cached, stale) = Catalog::load_cached();
    if cached.is_empty() {
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            Catalog::fetch(client.http(), client.base_url()),
        )
        .await
        {
            Ok(Ok(c)) => return c,
            Ok(Err(e)) => warnings.push(format!(
                "could not load the model catalog ({e}); using defaults"
            )),
            Err(_) => warnings.push("model catalog request timed out; using defaults".into()),
        }
        return cached;
    }
    if stale {
        let http = client.http().clone();
        let base = client.base_url().to_string();
        tokio::spawn(async move {
            let _ = Catalog::fetch(&http, &base).await;
        });
    }
    cached
}

pub async fn boot(opts: BootOptions, api_key: String) -> Result<Booted> {
    let (cwd, root, cfg) = load_config(&opts)?;
    boot_with(opts, cwd, root, cfg, api_key).await
}

pub async fn boot_with(
    opts: BootOptions,
    cwd: PathBuf,
    root: PathBuf,
    cfg: Config,
    api_key: String,
) -> Result<Booted> {
    let mut warnings = vec![];
    let client = LlmClient::new(&cfg.base_url, &api_key)?;
    let catalog = load_catalog(&client, &mut warnings).await;
    if !catalog.is_empty() && catalog.get(&cfg.model).is_none() && !cfg.model.starts_with('~') {
        let hint: Vec<String> = catalog
            .search(cfg.model.split('/').next_back().unwrap_or(&cfg.model))
            .iter()
            .take(3)
            .map(|m| m.id.clone())
            .collect();
        warnings.push(format!(
            "model `{}` is not in the OpenRouter catalog{}",
            cfg.model,
            if hint.is_empty() {
                String::new()
            } else {
                format!(" — did you mean {}?", hint.join(", "))
            }
        ));
    }
    if let Some(info) = catalog.get(&cfg.model)
        && !info.supports_tools
    {
        warnings.push(format!(
            "model `{}` does not advertise tool calling; the agent may not work",
            cfg.model
        ));
    }
    let mode = Mode::parse(&cfg.mode).unwrap_or(Mode::Default);
    let ext = Extensions::load(&root);

    let mcp = if !opts.no_mcp && !cfg.mcp.is_empty() {
        let m = McpManager::start(&cfg.mcp, &root).await;
        warnings.extend(m.errors.iter().cloned());
        Some(Arc::new(m))
    } else {
        None
    };

    // Session: new or resumed.
    let (session, replay) = match &opts.resume {
        Resume::New => (Session::create(&root, &cfg.model, None)?, None),
        Resume::Last => {
            let list = crate::session::list(&root);
            match list.first() {
                Some(s) => {
                    let sess = Session::open(&s.path)?;
                    let r = sess.replay()?;
                    (sess, Some(r))
                }
                None => {
                    warnings.push("no previous session to continue; starting a new one".into());
                    (Session::create(&root, &cfg.model, None)?, None)
                }
            }
        }
        Resume::Id(id) => {
            let path = if std::path::Path::new(id).is_file() {
                PathBuf::from(id)
            } else {
                crate::session::find(&root, id)?
            };
            let sess = Session::open(&path)?;
            let r = sess.replay()?;
            (sess, Some(r))
        }
    };

    let instructions = crate::instructions::load(&root, &cwd);
    let loaded: HashSet<PathBuf> = instructions.iter().map(|(p, _)| p.clone()).collect();
    let git = crate::agent::prompt::git_snapshot(&root);
    let sandbox_available = crate::sandbox::available();
    if mode == Mode::Auto && !sandbox_available && cfg.sandbox != "off" {
        warnings.push(
            "OS sandbox unavailable on this system: `auto` mode will run commands unsandboxed"
                .into(),
        );
    }
    let spill_dir = crate::config::data_dir().join("spill").join(&session.id);
    let model = replay
        .as_ref()
        .and_then(|r| r.model.clone())
        .filter(|_| opts.model.is_none())
        .unwrap_or(cfg.model.clone());
    let effort = cfg.effort.clone();

    let mut checkpoints = Checkpoints::default();
    let mut todos = vec![];
    let mut usage = Usage::default();
    let mut title = None;
    if let Some(r) = &replay {
        checkpoints.entries = r.checkpoints.clone();
        todos = r.todos.clone();
        usage = r.usage.clone();
        title = r.title.clone();
    }

    let shared = Arc::new(Shared {
        cfg: RwLock::new(Arc::new(cfg.clone())),
        root: root.clone(),
        cwd: Mutex::new(cwd.clone()),
        client,
        catalog: RwLock::new(Arc::new(catalog)),
        files: Mutex::new(FileTracker::default()),
        todos: Mutex::new(todos),
        jobs: Default::default(),
        checkpoints: Mutex::new(checkpoints),
        turn: AtomicUsize::new(0),
        modified: Mutex::new(BTreeSet::new()),
        loaded_instructions: Mutex::new(loaded),
        session: RwLock::new(Arc::new(session)),
        ext,
        mcp,
        spill_dir,
        mode: RwLock::new(mode),
        session_rules: Mutex::new(vec![]),
        interactive: opts.interactive,
        instructions,
        git,
        sandbox_available,
        total_usage: Mutex::new(usage.clone()),
        title: Mutex::new(title),
    });

    let (sink, rx) = EventSink::channel();
    let mut agent = Agent::new(shared.clone(), model, effort, sink);
    agent.usage = usage;
    let resumed = replay.is_some();
    if let Some(r) = replay {
        agent.restore(r.items);
    }

    // session_start hooks may inject context.
    if !cfg.hooks.is_empty() {
        let h = crate::hooks::run(
            &cfg.hooks,
            "session_start",
            None,
            &serde_json::json!({"session_id": shared.session().id, "cwd": cwd}),
            &cwd,
        )
        .await;
        warnings.extend(h.warnings);
        if !h.context.is_empty() {
            let mut c = (*shared.cfg()).clone();
            c.instructions = Some(format!(
                "{}\n{}",
                c.instructions.unwrap_or_default(),
                h.context.join("\n")
            ));
            *shared.cfg.write() = Arc::new(c);
        }
    }
    Ok(Booted {
        agent,
        rx,
        warnings,
        resumed,
    })
}

/// Resolve the API key or explain how to set one.
pub fn require_api_key(cfg: &Config) -> Result<String> {
    match cfg.resolve_api_key() {
        Some(k) => Ok(k),
        None => bail!(
            "no OpenRouter API key found.\n  Get one at https://openrouter.ai/keys and either:\n    export OPENROUTER_API_KEY=sk-or-...\n  or run `harness login`, or set `api_key_cmd` in {}",
            crate::config::config_dir().join("config.toml").display()
        ),
    }
}

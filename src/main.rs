//! harness — a fast, native coding agent for the terminal, powered by any
//! model on OpenRouter.

mod agent;
mod app;
mod config;
mod conversation;
mod extensions;
mod headless;
mod hooks;
mod instructions;
mod llm;
mod mcp;
mod permissions;
mod sandbox;
mod session;
mod tools;
mod ui;
mod util;

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod testutil;

use anyhow::{Context, Result};
use app::{BootOptions, Resume};
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Read, Write};

#[derive(Parser, Debug)]
#[command(
    name = "harness",
    version,
    about = "A fast, native coding agent for your terminal — any model on OpenRouter.",
    after_help = "Examples:\n  harness                                  interactive session\n  harness \"fix the failing test\"           start with a prompt\n  harness -p \"summarize src/\" --output-format json   scripted / CI\n  git diff | harness -p \"review this\"        pipe context in\n  harness -c                               continue the last session\n  harness -m openai/gpt-5.6-sol --mode auto  pick model and mode"
)]
struct Cli {
    /// Initial prompt.
    prompt: Vec<String>,
    /// Non-interactive: run the prompt, print the result and exit.
    #[arg(short, long)]
    print: bool,
    /// Model id (any OpenRouter model, e.g. anthropic/claude-sonnet-5.5).
    #[arg(short, long, env = "HARNESS_MODEL")]
    model: Option<String>,
    /// Reasoning effort: none|minimal|low|medium|high|xhigh|max.
    #[arg(short, long)]
    effort: Option<String>,
    /// Permission mode: default|accept-edits|auto|yolo|plan.
    #[arg(long)]
    mode: Option<String>,
    /// Shortcut for --mode auto (sandboxed autonomy).
    #[arg(long)]
    auto: bool,
    /// Shortcut for --mode yolo (no approvals, no sandbox).
    #[arg(long, alias = "dangerously-skip-permissions")]
    yolo: bool,
    /// Start in plan mode.
    #[arg(long)]
    plan: bool,
    /// Continue the most recent session in this project.
    #[arg(short = 'c', long = "continue")]
    cont: bool,
    /// Resume a session by id (or pick one interactively).
    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
    /// Output format for --print: text|json|stream-json.
    #[arg(long, default_value = "text")]
    output_format: String,
    /// Stop when the session costs more than this many dollars.
    #[arg(long)]
    max_cost: Option<f64>,
    /// Maximum agent steps per turn.
    #[arg(long)]
    max_steps: Option<u32>,
    /// Command to verify changes (repeatable); failures are fed back to the agent.
    #[arg(long)]
    verify: Vec<String>,
    /// Extra allow rule, e.g. "bash(npm test*)" (repeatable).
    #[arg(long)]
    allow: Vec<String>,
    /// Extra deny rule (repeatable).
    #[arg(long)]
    deny: Vec<String>,
    /// Disable a tool by name (repeatable).
    #[arg(long)]
    disable_tool: Vec<String>,
    /// Don't start MCP servers.
    #[arg(long)]
    no_mcp: bool,
    /// Extra text appended to the system prompt.
    #[arg(long)]
    append_system_prompt: Option<String>,
    /// Working directory.
    #[arg(short = 'C', long)]
    cwd: Option<std::path::PathBuf>,
    /// Verbose output (headless: tool calls on stderr).
    #[arg(short, long)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Save an OpenRouter API key to the config file.
    Login,
    /// List models from the OpenRouter catalog.
    Models {
        /// Filter (all terms must match).
        query: Vec<String>,
        /// Include models without tool support.
        #[arg(long)]
        all: bool,
    },
    /// List sessions for the current project.
    Sessions,
    /// Configuration helpers.
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Write a commented config template (global, or --project).
    Init {
        #[arg(long)]
        project: bool,
    },
    /// Print config file locations.
    Path,
    /// Print the effective configuration.
    Show,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|a| a == "__sandbox").unwrap_or(false) {
        sandbox::helper_main(&args[2..]);
    }
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(async_main(cli)).unwrap_or_else(|e| {
        eprintln!("\x1b[31merror:\x1b[0m {e:#}");
        1
    });
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    std::process::exit(code);
}

async fn async_main(cli: Cli) -> Result<i32> {
    match &cli.cmd {
        Some(Cmd::Login) => return login().await,
        Some(Cmd::Models { query, all }) => return models(&query.join(" "), *all).await,
        Some(Cmd::Sessions) => return sessions(),
        Some(Cmd::Config { action }) => return config_cmd(action.as_ref(), &cli),
        None => {}
    }

    let mut prompt = cli.prompt.join(" ");
    let stdin_piped = !std::io::stdin().is_terminal();
    let print = cli.print || stdin_piped || !std::io::stdout().is_terminal();
    if stdin_piped {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).ok();
        if !input.trim().is_empty() {
            prompt = if prompt.trim().is_empty() {
                input
            } else {
                format!("{prompt}\n\n<stdin>\n{}\n</stdin>", input.trim_end())
            };
        }
    }
    let mode = if cli.yolo {
        Some("yolo".to_string())
    } else if cli.auto {
        Some("auto".to_string())
    } else if cli.plan {
        Some("plan".to_string())
    } else {
        cli.mode.clone()
    };
    let resume = if cli.cont {
        Resume::Last
    } else {
        match &cli.resume {
            Some(id) if !id.is_empty() => Resume::Id(id.clone()),
            _ => Resume::New,
        }
    };
    let want_picker = matches!(&cli.resume, Some(id) if id.is_empty());
    let opts = BootOptions {
        model: cli.model.clone(),
        mode,
        effort: cli.effort.clone(),
        interactive: !print,
        resume,
        no_mcp: cli.no_mcp,
        max_cost: cli.max_cost,
        max_steps: cli.max_steps,
        verify: cli.verify.clone(),
        allow: cli.allow.clone(),
        deny: cli.deny.clone(),
        disabled_tools: cli.disable_tool.clone(),
        append_system: cli.append_system_prompt.clone(),
        cwd: cli.cwd.clone(),
    };
    let (_, _, cfg) = app::load_config(&opts)?;
    let key = match app::require_api_key(&cfg) {
        Ok(k) => k,
        Err(e) => {
            if print {
                return Err(e);
            }
            eprintln!("{e}\n");
            match prompt_for_key().await? {
                Some(k) => k,
                None => return Ok(1),
            }
        }
    };

    if print {
        if prompt.trim().is_empty() {
            anyhow::bail!("--print needs a prompt (argument or stdin)");
        }
        let booted = app::boot(opts, key).await?;
        for w in &booted.warnings {
            eprintln!("warning: {w}");
        }
        let fmt = headless::Format::parse(&cli.output_format);
        return Ok(headless::run(booted.agent, booted.rx, prompt, fmt, cli.verbose).await);
    }

    let booted = app::boot(opts, key).await?;
    let mut app = ui::App::new(booted.agent, booted.rx)?;
    app.print_banner(&booted.warnings, booted.resumed);
    if want_picker {
        ui::commands::run(&mut app, "resume", "").await;
    }
    app.run(if prompt.trim().is_empty() {
        None
    } else {
        Some(prompt)
    })
    .await?;
    Ok(0)
}

async fn prompt_for_key() -> Result<Option<String>> {
    eprint!("Paste your OpenRouter API key (https://openrouter.ai/keys), or press enter to quit: ");
    std::io::stderr().flush().ok();
    let key = read_hidden_line()?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Ok(None);
    }
    save_key(&key)?;
    Ok(Some(key))
}

fn read_hidden_line() -> Result<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    crossterm::terminal::enable_raw_mode()?;
    let mut s = String::new();
    let res = loop {
        match read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.clear();
                    break Ok(());
                }
                KeyCode::Backspace => {
                    s.pop();
                }
                KeyCode::Char(c) => s.push(c),
                _ => {}
            },
            Ok(Event::Paste(p)) => s.push_str(&p),
            Ok(_) => {}
            Err(e) => break Err(e),
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    eprintln!();
    res?;
    Ok(s)
}

fn save_key(key: &str) -> Result<()> {
    let dir = config::config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_else(|_| config::TEMPLATE.to_string());
    let mut doc: toml::Table = toml::from_str(&existing).unwrap_or_default();
    doc.insert("api_key".into(), toml::Value::String(key.to_string()));
    // Keep the commented template when creating the file for the first time.
    let body = if existing == config::TEMPLATE {
        format!(
            "api_key = \"{}\"\n{}",
            key.replace('"', ""),
            config::TEMPLATE
        )
    } else {
        toml::to_string_pretty(&doc)?
    };
    std::fs::write(&path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    eprintln!("saved key to {}", path.display());
    Ok(())
}

async fn login() -> Result<i32> {
    eprint!("OpenRouter API key: ");
    std::io::stderr().flush().ok();
    let key = if std::io::stdin().is_terminal() {
        read_hidden_line()?
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    };
    let key = key.trim();
    if key.is_empty() {
        return Ok(1);
    }
    let client = llm::LlmClient::new(config::DEFAULT_BASE_URL, key)?;
    match client.key_info().await {
        Ok(info) => {
            let label = info.get("label").and_then(|v| v.as_str()).unwrap_or("");
            eprintln!("key ok {label}");
        }
        Err(e) => eprintln!("warning: could not verify the key ({e}); saving anyway"),
    }
    save_key(key)?;
    Ok(0)
}

async fn models(query: &str, all: bool) -> Result<i32> {
    let http = reqwest::Client::new();
    let base =
        std::env::var("OPENROUTER_BASE_URL").unwrap_or_else(|_| config::DEFAULT_BASE_URL.into());
    let cat = match llm::Catalog::fetch(&http, &base).await {
        Ok(c) => c,
        Err(e) => {
            let (c, _) = llm::Catalog::load_cached();
            if c.is_empty() {
                return Err(e).context("fetching the model catalog");
            }
            c
        }
    };
    let mut list = cat.search(query);
    if !all {
        list.retain(|m| m.supports_tools);
    }
    println!(
        "{:<48} {:>8} {:>9} {:>9}  FEATURES",
        "MODEL", "CONTEXT", "IN $/M", "OUT $/M"
    );
    for m in &list {
        let mut feats = vec![];
        if m.supports_reasoning {
            feats.push("reasoning");
        }
        if m.input_images {
            feats.push("vision");
        }
        if m.price_cache_read > 0.0 {
            feats.push("cache");
        }
        println!(
            "{:<48} {:>8} {:>9.3} {:>9.3}  {}",
            m.id,
            util::fmt_tokens(m.context_length),
            m.price_in * 1e6,
            m.price_out * 1e6,
            feats.join(",")
        );
    }
    eprintln!("{} models", list.len());
    Ok(0)
}

fn sessions() -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let root = config::project_root(&cwd);
    let list = session::list(&root);
    if list.is_empty() {
        println!("no sessions for {}", root.display());
        return Ok(0);
    }
    for s in list {
        let age = s
            .modified
            .elapsed()
            .map(util::fmt_duration)
            .unwrap_or_default();
        println!(
            "{:<26} {:>8} ago  {:>3} msgs  {:>8}  {}",
            s.id,
            age,
            s.messages,
            util::fmt_cost(s.cost),
            s.title
        );
    }
    Ok(0)
}

fn config_cmd(action: Option<&ConfigCmd>, cli: &Cli) -> Result<i32> {
    let cwd = cli
        .cwd
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    let root = config::project_root(&cwd);
    match action {
        Some(ConfigCmd::Init { project }) => {
            let path = if *project {
                root.join(".harness").join("config.toml")
            } else {
                config::config_dir().join("config.toml")
            };
            if path.exists() {
                println!("{} already exists", path.display());
                return Ok(1);
            }
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, config::TEMPLATE)?;
            println!("wrote {}", path.display());
        }
        Some(ConfigCmd::Show) => {
            let mut cfg = config::Config::load(&root)?;
            if cfg.api_key.is_some() {
                cfg.api_key = Some("<redacted>".into());
            }
            println!("{}", toml::to_string_pretty(&cfg)?);
        }
        Some(ConfigCmd::Path) | None => {
            for p in config::config_layers(&root) {
                println!("{} {}", if p.exists() { "●" } else { "○" }, p.display());
            }
        }
    }
    Ok(0)
}

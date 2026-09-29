//! Layered TOML configuration.
//!
//! Precedence (low → high): built-in defaults, `~/.config/leme/config.toml`,
//! `<project>/.leme/config.toml`, `<project>/.leme/local.toml`
//! (personal, meant to be git-ignored), environment variables, CLI flags.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5.5";
pub const DEFAULT_SMALL_MODEL: &str = "google/gemini-3.5-flash-lite";
pub const DEFAULT_ORACLE_MODEL: &str = "openai/gpt-5.6-sol";
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Main model (any OpenRouter model id, including `:nitro`/`:floor` variants).
    pub model: String,
    /// Cheap/fast model for titles, web-page digestion and the `explore` subagent.
    pub small_model: String,
    /// Strong reasoning model used by the `consult` tool (second opinion).
    pub oracle_model: String,
    /// Model for the context-compaction summary; empty = main model.
    pub compact_model: String,
    /// Reasoning effort: none|minimal|low|medium|high|xhigh|max (or empty = model default).
    pub effort: String,
    /// Permission mode on start: default|accept-edits|auto|yolo|plan.
    pub mode: String,
    pub api_key: Option<String>,
    /// Shell command whose stdout is the API key (e.g. `pass show openrouter`).
    pub api_key_cmd: Option<String>,
    pub base_url: String,
    pub max_output_tokens: u32,
    /// Cap on the context window a session uses (0 = the model's full window).
    /// Long contexts cost more per step and degrade quality; 400k is a good default.
    pub context_limit: u64,
    /// Auto-compact when the context reaches this fraction of the window.
    pub compact_threshold: f64,
    pub temperature: Option<f32>,
    /// Show reasoning streams in the UI.
    pub show_reasoning: bool,
    /// Edit tool flavour: auto|replace|patch (`auto` picks patch for OpenAI models).
    pub edit_format: String,
    /// Commands run automatically when the agent finishes a turn that changed files.
    pub verify: Vec<String>,
    /// Max automatic verify→fix rounds per user turn.
    pub verify_rounds: u32,
    /// Sandbox bash in `auto` mode: auto|on|off.
    pub sandbox: String,
    /// Allow network inside the sandbox.
    pub sandbox_network: bool,
    /// Prompt caching for providers that need explicit breakpoints: auto|off.
    pub cache: String,
    /// Maximum agent steps (LLM calls) per user turn.
    pub max_steps: u32,
    /// Stop a turn once session cost exceeds this many dollars (0 = unlimited).
    pub max_cost: f64,
    /// Default bash timeout in seconds.
    pub bash_timeout: u64,
    /// Fallback models tried by OpenRouter when the main one fails.
    pub fallback_models: Vec<String>,
    /// Passed verbatim as OpenRouter's `provider` routing preferences.
    pub provider: Option<toml::Value>,
    /// Extra body fields merged into every request.
    pub extra_body: Option<toml::Value>,
    pub permissions: PermissionsConfig,
    pub hooks: Vec<HookConfig>,
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Enable/disable individual built-in tools by name.
    pub disabled_tools: Vec<String>,
    /// Extra instructions appended to the system prompt.
    pub instructions: Option<String>,
    /// Desktop notification (terminal bell / OSC 9) when a long turn finishes.
    pub notify: bool,
    pub theme: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    /// Rules like `bash(cargo test*)`, `edit(src/**)`, `webfetch(docs.rs)`, `mcp__github__*`.
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// session_start | user_prompt | pre_tool | post_tool | stop
    pub event: String,
    /// Regex over tool names (pre_tool/post_tool). Empty = all.
    #[serde(default)]
    pub matcher: String,
    pub command: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout: u64,
}

fn default_hook_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub enabled: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            small_model: DEFAULT_SMALL_MODEL.into(),
            oracle_model: DEFAULT_ORACLE_MODEL.into(),
            compact_model: String::new(),
            effort: String::new(),
            mode: "default".into(),
            api_key: None,
            api_key_cmd: None,
            base_url: DEFAULT_BASE_URL.into(),
            max_output_tokens: 32_000,
            context_limit: 400_000,
            compact_threshold: 0.8,
            temperature: None,
            show_reasoning: true,
            edit_format: "auto".into(),
            verify: vec![],
            verify_rounds: 3,
            sandbox: "auto".into(),
            sandbox_network: true,
            cache: "auto".into(),
            max_steps: 200,
            max_cost: 0.0,
            bash_timeout: 120,
            fallback_models: vec![],
            provider: None,
            extra_body: None,
            permissions: PermissionsConfig::default(),
            hooks: vec![],
            mcp: BTreeMap::new(),
            disabled_tools: vec![],
            instructions: None,
            notify: true,
            theme: "auto".into(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LEME_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("leme")
}

pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LEME_DATA_DIR") {
        return PathBuf::from(p);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("leme")
}

pub fn cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LEME_CACHE_DIR") {
        return PathBuf::from(p);
    }
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("leme")
}

/// Find the project root: nearest ancestor with `.git`, `.leme` or `AGENTS.md`;
/// falls back to `cwd`.
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() || d.join(".leme").is_dir() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    cwd.to_path_buf()
}

fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    // Arrays of permission rules / hooks accumulate across layers.
                    Some(existing @ toml::Value::Array(_)) if is_accumulating(&k) => {
                        if let (toml::Value::Array(a), toml::Value::Array(nv)) = (existing, v) {
                            a.extend(nv);
                        }
                    }
                    Some(existing) => merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

fn is_accumulating(key: &str) -> bool {
    matches!(key, "allow" | "deny" | "ask" | "hooks")
}

fn read_toml(path: &Path) -> Result<Option<toml::Value>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let v: toml::Value =
                toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(v))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn config_layers(root: &Path) -> Vec<PathBuf> {
    vec![
        config_dir().join("config.toml"),
        root.join(".leme").join("config.toml"),
        root.join(".leme").join("local.toml"),
    ]
}

impl Config {
    pub fn load(root: &Path) -> Result<Config> {
        let mut acc = toml::Value::try_from(Config::default())?;
        for path in config_layers(root) {
            if let Some(v) = read_toml(&path)? {
                merge(&mut acc, v);
            }
        }
        let mut cfg: Config = acc.try_into().context("invalid configuration")?;
        cfg.apply_env();
        cfg.load_mcp_json(root);
        Ok(cfg)
    }

    fn apply_env(&mut self) {
        if let Ok(m) = std::env::var("LEME_MODEL") {
            self.model = m;
        }
        if let Ok(u) = std::env::var("OPENROUTER_BASE_URL") {
            self.base_url = u;
        }
    }

    /// Claude-Code compatible `.mcp.json` in the project root.
    fn load_mcp_json(&mut self, root: &Path) {
        let path = root.join(".mcp.json");
        let Ok(s) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
            return;
        };
        let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object()) else {
            return;
        };
        for (name, spec) in servers {
            if self.mcp.contains_key(name) {
                continue;
            }
            let mut c = McpServerConfig {
                command: spec
                    .get("command")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                url: spec.get("url").and_then(|x| x.as_str()).map(String::from),
                ..Default::default()
            };
            if let Some(args) = spec.get("args").and_then(|a| a.as_array()) {
                c.args = args
                    .iter()
                    .filter_map(|a| a.as_str().map(String::from))
                    .collect();
            }
            for key in ["env", "headers"] {
                if let Some(m) = spec.get(key).and_then(|e| e.as_object()) {
                    let map: BTreeMap<String, String> = m
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect();
                    if key == "env" {
                        c.env = map;
                    } else {
                        c.headers = map;
                    }
                }
            }
            self.mcp.insert(name.clone(), c);
        }
    }

    /// Resolve the API key: config → `api_key_cmd` → env.
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(k) = &self.api_key
            && !k.trim().is_empty()
        {
            return Some(k.trim().to_string());
        }
        if let Some(cmd) = &self.api_key_cmd
            && let Ok(out) = std::process::Command::new("sh").arg("-c").arg(cmd).output()
        {
            let k = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !k.is_empty() {
                return Some(k);
            }
        }
        for var in ["OPENROUTER_API_KEY", "LEME_API_KEY"] {
            if let Ok(k) = std::env::var(var)
                && !k.trim().is_empty()
            {
                return Some(k.trim().to_string());
            }
        }
        None
    }

    pub fn compact_model(&self) -> &str {
        if self.compact_model.is_empty() {
            &self.model
        } else {
            &self.compact_model
        }
    }
}

/// Append a permission rule to `<root>/.leme/local.toml` so that "always
/// allow" answers persist for the project.
pub fn persist_allow_rule(root: &Path, rule: &str) -> Result<()> {
    let dir = root.join(".leme");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("local.toml");
    let mut v = read_toml(&path)?.unwrap_or(toml::Value::Table(Default::default()));
    let table = v.as_table_mut().context("local.toml is not a table")?;
    let perms = table
        .entry("permissions")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let perms = perms.as_table_mut().context("permissions is not a table")?;
    let allow = perms
        .entry("allow")
        .or_insert_with(|| toml::Value::Array(vec![]));
    if let toml::Value::Array(a) = allow
        && !a.iter().any(|x| x.as_str() == Some(rule))
    {
        a.push(toml::Value::String(rule.to_string()));
    }
    std::fs::write(&path, toml::to_string_pretty(&v)?)?;
    // Keep personal settings out of git by default.
    let gi = dir.join(".gitignore");
    if !gi.exists() {
        let _ = std::fs::write(&gi, "local.toml\n");
    }
    Ok(())
}

pub const TEMPLATE: &str = r#"# leme configuration — https://github.com/JaimeJunr/Leme-Agent
# Any OpenRouter model id works: https://openrouter.ai/models

model = "anthropic/claude-sonnet-5.5"
small_model = "google/gemini-3.5-flash-lite"   # titles, web digests, explore subagent
oracle_model = "openai/gpt-5.6-sol"           # `consult` tool: second opinion
# effort = "high"                              # reasoning effort
# mode = "default"                             # default | accept-edits | auto | yolo | plan
# api_key_cmd = "pass show openrouter"         # or set OPENROUTER_API_KEY
# verify = ["cargo check --quiet"]             # run after edits; failures are fed back
# max_cost = 5.0                               # per-session budget in USD
# context_limit = 400000                       # cap the usable window (0 = model's full window)
# fallback_models = ["openai/gpt-5.6-terra"]

# [provider]                                   # OpenRouter provider routing
# sort = "throughput"
# data_collection = "deny"

[permissions]
allow = [
  # "bash(cargo test*)",
  # "bash(npm run *)",
]
deny = [
  # "read(.env*)",
]

# [[hooks]]
# event = "post_tool"
# matcher = "edit|write|multi_edit|apply_patch"
# command = "cargo fmt"

# [mcp.github]
# command = "github-mcp-server"
# args = ["stdio"]
# env = { GITHUB_TOKEN = "..." }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_merge_and_accumulate() {
        let mut base = toml::Value::try_from(Config::default()).unwrap();
        let a: toml::Value = toml::from_str(
            "model='x'\n[permissions]\nallow=['bash(ls*)']\n[provider]\nsort='price'",
        )
        .unwrap();
        let b: toml::Value =
            toml::from_str("effort='high'\n[permissions]\nallow=['bash(cargo*)']").unwrap();
        merge(&mut base, a);
        merge(&mut base, b);
        let cfg: Config = base.try_into().unwrap();
        assert_eq!(cfg.model, "x");
        assert_eq!(cfg.effort, "high");
        assert_eq!(cfg.permissions.allow.len(), 2);
        assert!(cfg.provider.is_some());
        assert_eq!(cfg.max_steps, 200);
    }

    #[test]
    fn template_parses() {
        let v: toml::Value = toml::from_str(TEMPLATE).unwrap();
        let mut base = toml::Value::try_from(Config::default()).unwrap();
        merge(&mut base, v);
        let _: Config = base.try_into().unwrap();
    }
}

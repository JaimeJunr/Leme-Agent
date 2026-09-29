//! Markdown-defined extensions: skills, custom slash commands and subagent
//! definitions. Compatible with the `.claude/` layout so existing assets work.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub fields: BTreeMap<String, String>,
    pub body: String,
}

/// Minimal YAML-ish frontmatter parser (`key: value` lines).
pub fn parse_frontmatter(src: &str) -> Frontmatter {
    let src = src.trim_start_matches('\u{feff}');
    let mut fm = Frontmatter::default();
    let Some(rest) = src.strip_prefix("---") else {
        fm.body = src.to_string();
        return fm;
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    let (head, body) = match rest.find("\n---") {
        Some(i) => {
            let after = &rest[i + 4..];
            let after = after.split_once('\n').map(|(_, b)| b).unwrap_or("");
            (&rest[..i], after)
        }
        None => ("", rest),
    };
    let mut last_key: Option<String> = None;
    for line in head.lines() {
        if let Some(item) = line.trim_start().strip_prefix("- ") {
            // YAML list continuation: key:\n  - a\n  - b
            if let Some(k) = &last_key {
                let e = fm.fields.entry(k.clone()).or_default();
                if !e.is_empty() {
                    e.push_str(", ");
                }
                e.push_str(item.trim().trim_matches(['"', '\'']));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().trim_matches(['"', '\'']).to_string();
            last_key = Some(k.clone());
            fm.fields.insert(k, v);
        }
    }
    fm.body = body.trim_start_matches(['\r', '\n']).to_string();
    fm
}

fn list_field(v: Option<&String>) -> Option<Vec<String>> {
    let v = v?
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if v.is_empty() {
        return None;
    }
    Some(
        v.split(',')
            .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub model: Option<String>,
    pub body: String,
    pub source: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    /// Model id, or `small`/`main`/`oracle` aliases.
    pub model: Option<String>,
    pub prompt: String,
    pub builtin: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Extensions {
    pub skills: Vec<Skill>,
    pub commands: Vec<Command>,
    pub agents: Vec<AgentDef>,
}

/// Search roots, highest priority first.
fn roots(project: &Path, kind: &str) -> Vec<PathBuf> {
    let mut v = vec![
        project.join(".harness").join(kind),
        project.join(".claude").join(kind),
        project.join(".agents").join(kind),
        crate::config::config_dir().join(kind),
    ];
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".claude").join(kind));
        v.push(h.join(".agents").join(kind));
    }
    v
}

fn md_files(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            let sub = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}:{name}")
            };
            md_files(&p, &sub, out);
        } else if let Some(stem) = name.strip_suffix(".md") {
            let full = if prefix.is_empty() {
                stem.to_string()
            } else {
                format!("{prefix}:{stem}")
            };
            out.push((full, p));
        }
    }
}

pub fn builtin_agents() -> Vec<AgentDef> {
    vec![
        AgentDef {
            name: "explore".into(),
            description: "Fast, cheap read-only codebase exploration: locating files and symbols, tracing how something works, answering 'where/how is X done'. Give it a specific question; it returns a concise report with file paths and line numbers.".into(),
            tools: Some(vec!["read".into(), "ls".into(), "glob".into(), "grep".into(), "bash".into()]),
            model: Some("small".into()),
            prompt: "You are an exploration subagent. Investigate the codebase to answer the question precisely. Search broadly first (grep/glob), then read the relevant parts. Only run read-only shell commands. Your final message is your report to the main agent: lead with the direct answer, then the key file paths with line numbers and short relevant snippets. Be concise; do not suggest changes unless asked.".into(),
            builtin: true,
        },
        AgentDef {
            name: "general".into(),
            description: "General-purpose subagent with the same tools and model as you. Use for self-contained multi-step tasks (research + edits) whose details would clutter your context, or to run independent workstreams in parallel.".into(),
            tools: None,
            model: None,
            prompt: "You are a subagent working on a delegated task. Complete it fully and autonomously, verifying your work. Your final message is your report to the main agent: what you did, files changed, verification results, and anything left open. Be concise.".into(),
            builtin: true,
        },
    ]
}

impl Extensions {
    pub fn load(project: &Path) -> Extensions {
        let mut ext = Extensions::default();
        // Skills: <root>/skills/<name>/SKILL.md
        for dir in roots(project, "skills") {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let file = e.path().join("SKILL.md");
                let Ok(src) = std::fs::read_to_string(&file) else {
                    continue;
                };
                let fm = parse_frontmatter(&src);
                let name = fm
                    .fields
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| e.file_name().to_string_lossy().to_string());
                if ext.skills.iter().any(|s| s.name == name) {
                    continue;
                }
                let description = fm
                    .fields
                    .get("description")
                    .cloned()
                    .unwrap_or_else(|| crate::util::first_line(&fm.body, 120));
                ext.skills.push(Skill {
                    name,
                    description,
                    dir: e.path(),
                    file,
                });
            }
        }
        for dir in roots(project, "commands") {
            let mut files = vec![];
            md_files(&dir, "", &mut files);
            for (name, path) in files {
                if ext.commands.iter().any(|c| c.name == name) {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let fm = parse_frontmatter(&src);
                ext.commands.push(Command {
                    description: fm
                        .fields
                        .get("description")
                        .cloned()
                        .unwrap_or_else(|| crate::util::first_line(&fm.body, 80)),
                    argument_hint: fm.fields.get("argument-hint").cloned().unwrap_or_default(),
                    model: fm.fields.get("model").cloned().filter(|m| !m.is_empty()),
                    body: fm.body,
                    source: path,
                    name,
                });
            }
        }
        for dir in roots(project, "agents") {
            let mut files = vec![];
            md_files(&dir, "", &mut files);
            for (stem, path) in files {
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let fm = parse_frontmatter(&src);
                let name = fm.fields.get("name").cloned().unwrap_or(stem);
                if ext.agents.iter().any(|a| a.name == name) {
                    continue;
                }
                let model = fm
                    .fields
                    .get("model")
                    .cloned()
                    .filter(|m| !m.is_empty() && m != "inherit");
                // Claude Code aliases → our roles.
                let model = model.map(|m| match m.as_str() {
                    "haiku" => "small".to_string(),
                    "sonnet" => "main".to_string(),
                    "opus" => "oracle".to_string(),
                    _ => m,
                });
                ext.agents.push(AgentDef {
                    description: fm.fields.get("description").cloned().unwrap_or_default(),
                    tools: list_field(fm.fields.get("tools"))
                        .map(|t| t.into_iter().map(|x| map_tool_name(&x)).collect()),
                    model,
                    prompt: fm.body,
                    builtin: false,
                    name,
                });
            }
        }
        for b in builtin_agents() {
            if !ext.agents.iter().any(|a| a.name == b.name) {
                ext.agents.push(b);
            }
        }
        ext
    }

    pub fn agent(&self, name: &str) -> Option<&AgentDef> {
        self.agents
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
    }
}

/// Map Claude Code tool names to ours.
pub fn map_tool_name(n: &str) -> String {
    match n {
        "Read" => "read",
        "Write" => "write",
        "Edit" => "edit",
        "MultiEdit" => "multi_edit",
        "Bash" => "bash",
        "Grep" => "grep",
        "Glob" => "glob",
        "LS" => "ls",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        "TodoWrite" => "todo",
        "Task" => "task",
        other => return other.to_lowercase(),
    }
    .to_string()
}

/// Expand `$ARGUMENTS`, `$1..$9` and `!`cmd`` (shell output) in a command body.
pub async fn expand_command(body: &str, args: &str, cwd: &Path) -> String {
    let parts: Vec<String> =
        shlex::split(args).unwrap_or_else(|| args.split_whitespace().map(String::from).collect());
    let mut out = body.replace("$ARGUMENTS", args);
    for i in (1..=9).rev() {
        out = out.replace(
            &format!("${i}"),
            parts.get(i - 1).map(|s| s.as_str()).unwrap_or(""),
        );
    }
    if !body.contains("$ARGUMENTS") && !args.trim().is_empty() {
        out.push_str(&format!("\n\nArguments: {args}"));
    }
    // !`command` → output
    let re = regex::Regex::new(r"!`([^`]+)`").unwrap();
    let mut result = String::new();
    let mut last = 0;
    for cap in re.captures_iter(&out) {
        let m = cap.get(0).unwrap();
        result.push_str(&out[last..m.start()]);
        let cmd = &cap[1];
        let o = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .output()
            .await
            .map(|o| {
                let mut s = String::from_utf8_lossy(&o.stdout).to_string();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                s
            })
            .unwrap_or_else(|e| format!("(failed: {e})"));
        result.push_str(o.trim_end());
        last = m.end();
    }
    result.push_str(&out[last..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter() {
        let fm = parse_frontmatter(
            "---\nname: x\ndescription: \"does y\"\ntools: Read, Grep\n---\nBody here\n",
        );
        assert_eq!(fm.fields["name"], "x");
        assert_eq!(fm.fields["description"], "does y");
        assert_eq!(fm.body, "Body here\n");
        assert_eq!(
            list_field(fm.fields.get("tools")).unwrap(),
            vec!["Read", "Grep"]
        );
        let fm2 = parse_frontmatter("---\ntools:\n  - a\n  - b\n---\n");
        assert_eq!(fm2.fields["tools"], "a, b");
        let fm3 = parse_frontmatter("no frontmatter");
        assert_eq!(fm3.body, "no frontmatter");
    }

    #[tokio::test]
    async fn expands_args() {
        let s = expand_command("Fix $1 then $ARGUMENTS !`echo hi`", "a b", Path::new(".")).await;
        assert_eq!(s, "Fix a then a b hi");
    }
}

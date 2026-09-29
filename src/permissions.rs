//! Permission modes, rules and the shell command classifier.

use crate::config::PermissionsConfig;
use crate::tools::{Tool, ToolKind};
use crate::util::{display_path, resolve_path, wildcard_match};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ask before edits and non-read-only commands.
    Default,
    /// Edits inside the workspace are automatic; commands still ask.
    AcceptEdits,
    /// Everything automatic; shell commands run in the OS sandbox and
    /// destructive commands still ask.
    Auto,
    /// No questions, no sandbox.
    Yolo,
    /// Read-only investigation, ends with a plan for approval.
    Plan,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s.trim().to_lowercase().replace('_', "-").as_str() {
            "default" | "ask" | "normal" => Mode::Default,
            "accept-edits" | "acceptedits" | "edits" | "auto-edit" => Mode::AcceptEdits,
            "auto" | "full-auto" | "sandbox" => Mode::Auto,
            "yolo" | "bypass" | "bypass-permissions" | "danger" => Mode::Yolo,
            "plan" | "read-only" | "readonly" => Mode::Plan,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::AcceptEdits => "accept-edits",
            Mode::Auto => "auto",
            Mode::Yolo => "yolo",
            Mode::Plan => "plan",
        }
    }

    /// Shift+Tab cycle order.
    pub fn next(self) -> Mode {
        match self {
            Mode::Default => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Auto,
            Mode::Auto => Mode::Plan,
            Mode::Plan => Mode::Default,
            Mode::Yolo => Mode::Default,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Allow { sandbox: bool },
    Ask { reason: String, rule: String },
    Deny(String),
}

/// The permission "subject" of a call: the string rules are matched against.
fn subject(tool: &dyn Tool, args: &Value, root: &Path, cwd: &Path) -> (String, String) {
    let name = tool.name();
    let family = match name {
        "edit" | "multi_edit" | "write" | "apply_patch" => "edit",
        "read" | "ls" | "glob" | "grep" => "read",
        n => n,
    };
    let s = match name {
        "bash" => args
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        "web_fetch" => args
            .get("url")
            .and_then(|u| u.as_str())
            .and_then(|u| u.split("://").nth(1))
            .map(|r| r.split('/').next().unwrap_or("").to_string())
            .unwrap_or_default(),
        "apply_patch" => tool.summarize(args),
        _ => args
            .get("path")
            .and_then(|p| p.as_str())
            .map(|p| display_path(root, &resolve_path(cwd, p)))
            .unwrap_or_default(),
    };
    (family.to_string(), s)
}

/// Match `family(pattern)` / `family` / `mcp__server__*` rules.
pub fn rule_matches(rule: &str, family: &str, tool_name: &str, subj: &str) -> bool {
    let rule = rule.trim();
    if let Some(open) = rule.find('(') {
        if !rule.ends_with(')') {
            return false;
        }
        let rname = rule[..open].trim().to_lowercase();
        let pat = &rule[open + 1..rule.len() - 1];
        let rname = crate::extensions::map_tool_name(&rname);
        if rname != family && rname != tool_name {
            return false;
        }
        // `bash(npm run test:*)` (Claude Code syntax) == `bash(npm run test*)`
        let pat = pat.replace(":*", "*");
        if family == "bash" {
            // Every sub-command of a compound command must match.
            return split_commands(subj)
                .map(|parts| !parts.is_empty() && parts.iter().all(|p| wildcard_match(&pat, p)))
                .unwrap_or(false);
        }
        wildcard_match(&pat, subj) || wildcard_match(&pat.replace("**", "*"), subj)
    } else {
        let r = crate::extensions::map_tool_name(rule);
        r == family || wildcard_match(&r, tool_name)
    }
}

pub struct Checker<'a> {
    pub mode: Mode,
    pub cfg: &'a PermissionsConfig,
    pub session_rules: &'a [String],
    pub root: &'a Path,
    pub cwd: &'a Path,
    pub sandbox_available: bool,
    pub sandbox_setting: &'a str,
}

impl Checker<'_> {
    pub fn check(&self, tool: &dyn Tool, args: &Value) -> Verdict {
        let (family, subj) = subject(tool, args, self.root, self.cwd);
        let name = tool.name();
        let kind = tool.kind();
        let sandbox_on = self.sandbox_available
            && match self.sandbox_setting {
                "on" => true,
                "off" => false,
                _ => self.mode == Mode::Auto,
            };
        if let Some(r) = self
            .cfg
            .deny
            .iter()
            .find(|r| rule_matches(r, &family, name, &subj))
        {
            return Verdict::Deny(format!("blocked by deny rule `{r}`"));
        }
        if self.mode == Mode::Plan {
            match kind {
                ToolKind::Edit => return Verdict::Deny("plan mode is read-only; propose the plan first".into()),
                ToolKind::Exec if name == "bash" && !is_read_only_command(&subj) => {
                    return Verdict::Deny("plan mode only allows read-only shell commands (ls, git status/diff/log, …)".into())
                }
                ToolKind::External => return Verdict::Deny("plan mode does not allow external tools".into()),
                _ => {}
            }
        }
        let explicitly_allowed = self
            .cfg
            .allow
            .iter()
            .chain(self.session_rules.iter())
            .any(|r| rule_matches(r, &family, name, &subj));
        if explicitly_allowed {
            return Verdict::Allow {
                sandbox: sandbox_on && kind == ToolKind::Exec,
            };
        }
        if self
            .cfg
            .ask
            .iter()
            .any(|r| rule_matches(r, &family, name, &subj))
            && self.mode != Mode::Yolo
        {
            return Verdict::Ask {
                reason: "matches an ask rule".into(),
                rule: suggest_rule(&family, name, &subj),
            };
        }
        match kind {
            ToolKind::Read | ToolKind::Network => Verdict::Allow { sandbox: false },
            ToolKind::Edit => {
                let inside = edit_inside_workspace(tool, args, self.root, self.cwd);
                match self.mode {
                    Mode::Yolo => Verdict::Allow { sandbox: false },
                    Mode::Auto | Mode::AcceptEdits if inside => Verdict::Allow { sandbox: false },
                    _ if !inside => Verdict::Ask {
                        reason: "edits a file outside the workspace".into(),
                        rule: format!("edit({subj})"),
                    },
                    _ => Verdict::Ask {
                        reason: "file edit".into(),
                        rule: "edit".into(),
                    },
                }
            }
            ToolKind::Exec => {
                if self.mode == Mode::Yolo {
                    return Verdict::Allow { sandbox: false };
                }
                if name == "bash" && is_read_only_command(&subj) {
                    return Verdict::Allow { sandbox: false };
                }
                if self.mode == Mode::Auto {
                    if let Some(why) = dangerous(&subj) {
                        return Verdict::Ask {
                            reason: why,
                            rule: suggest_rule(&family, name, &subj),
                        };
                    }
                    if sandbox_on {
                        return Verdict::Allow { sandbox: true };
                    }
                    return Verdict::Allow { sandbox: false };
                }
                Verdict::Ask {
                    reason: "runs a shell command".into(),
                    rule: suggest_rule(&family, name, &subj),
                }
            }
            ToolKind::External => match self.mode {
                Mode::Yolo | Mode::Auto => Verdict::Allow { sandbox: false },
                _ => Verdict::Ask {
                    reason: "external (MCP) tool".into(),
                    rule: name.to_string(),
                },
            },
        }
    }
}

fn edit_inside_workspace(tool: &dyn Tool, args: &Value, root: &Path, cwd: &Path) -> bool {
    if tool.name() == "apply_patch" {
        let p = args
            .get("patch")
            .or_else(|| args.get("input"))
            .and_then(|p| p.as_str())
            .unwrap_or("");
        return p
            .lines()
            .filter_map(|l| {
                l.strip_prefix("*** Update File:")
                    .or_else(|| l.strip_prefix("*** Add File:"))
                    .or_else(|| l.strip_prefix("*** Delete File:"))
                    .or_else(|| l.strip_prefix("*** Move to:"))
            })
            .all(|f| crate::util::is_within(root, &resolve_path(cwd, f.trim())));
    }
    match args.get("path").and_then(|p| p.as_str()) {
        Some(p) => {
            let abs = resolve_path(cwd, p);
            crate::util::is_within(root, &abs) && !abs.components().any(|c| c.as_os_str() == ".git")
        }
        None => false,
    }
}

fn suggest_rule(family: &str, name: &str, subj: &str) -> String {
    if family == "bash" {
        let parts = split_commands(subj).unwrap_or_default();
        let first = parts.first().cloned().unwrap_or_default();
        let words: Vec<&str> = first.split_whitespace().collect();
        let prefix = match words.as_slice() {
            [] => String::new(),
            [a] => a.to_string(),
            [a, b, ..]
                if !a.contains('/')
                    && !b.starts_with('-')
                    && !b.contains('/')
                    && !b.contains('.') =>
            {
                format!("{a} {b}")
            }
            [a, ..] => a.to_string(),
        };
        return format!("bash({prefix}*)");
    }
    if family == "web_fetch" {
        return format!("web_fetch({subj})");
    }
    name.to_string()
}

/// Split a shell command on `&&`, `||`, `;`, `|` and newlines, respecting
/// quotes. Returns None if the command uses constructs we can't reason
/// about (command substitution, redirection to files, subshells…).
pub fn split_commands(cmd: &str) -> Option<Vec<String>> {
    let mut parts = vec![];
    let mut cur = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let (mut sq, mut dq) = (false, false);
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if sq {
            cur.push(c);
            if c == '\'' {
                sq = false;
            }
            i += 1;
            continue;
        }
        if c == '\\' {
            cur.push(c);
            if let Some(n) = next {
                cur.push(n);
            }
            i += 2;
            continue;
        }
        if dq {
            if c == '`' || (c == '$' && next == Some('(')) {
                return None;
            }
            cur.push(c);
            if c == '"' {
                dq = false;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                sq = true;
                cur.push(c);
            }
            '"' => {
                dq = true;
                cur.push(c);
            }
            '`' => return None,
            '$' if next == Some('(') => return None,
            '(' | ')' | '{' | '}' => return None,
            // Input redirection / heredocs / process substitution: too
            // opaque to classify.
            '<' => return None,
            '>' => {
                // Only harmless redirections: `2>&1`, `>&2`, `>/dev/null`.
                let rest: String = chars[i + 1..].iter().collect();
                let allowed = ["&1", "&2", "/dev/null", " /dev/null"];
                match allowed.iter().find(|a| rest.starts_with(**a)) {
                    Some(a) => {
                        if matches!(cur.chars().last(), Some('1') | Some('2')) {
                            cur.pop();
                        }
                        i += 1 + a.len();
                        continue;
                    }
                    None => return None,
                }
            }
            '&' if next == Some('>') => {
                let rest: String = chars[i + 2..].iter().collect();
                if rest.starts_with("/dev/null") || rest.starts_with(" /dev/null") {
                    i += 2 + if rest.starts_with(' ') { 10 } else { 9 };
                    continue;
                }
                return None;
            }
            '&' if next == Some('&') => {
                parts.push(std::mem::take(&mut cur));
                i += 2;
                continue;
            }
            '&' => return None, // background
            '|' if next == Some('|') => {
                parts.push(std::mem::take(&mut cur));
                i += 2;
                continue;
            }
            '|' | ';' | '\n' => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    if sq || dq {
        return None;
    }
    parts.push(cur);
    Some(
        parts
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
    )
}

const ALWAYS_SAFE: &[&str] = &[
    "ls",
    "pwd",
    "cat",
    "head",
    "tail",
    "wc",
    "echo",
    "printf",
    "which",
    "whereis",
    "type",
    "file",
    "stat",
    "du",
    "df",
    "tree",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ag",
    "fd",
    "sort",
    "uniq",
    "cut",
    "tr",
    "diff",
    "cmp",
    "comm",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "date",
    "whoami",
    "id",
    "uname",
    "hostname",
    "printenv",
    "true",
    "false",
    "test",
    "[",
    "nl",
    "column",
    "jq",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "shasum",
    "cksum",
    "od",
    "xxd",
    "hexdump",
    "strings",
    "ps",
    "free",
    "uptime",
    "nproc",
    "arch",
    "lscpu",
    "tac",
    "rev",
    "fold",
    "expand",
    "seq",
    "sleep",
    "cal",
    "locale",
    "tokei",
    "cloc",
    "scc",
    "wc",
    "zcat",
    "man",
    "info",
    "less",
    "more",
];

fn is_version_query(words: &[&str]) -> bool {
    words.len() == 2
        && matches!(
            words[1],
            "--version" | "-V" | "-v" | "version" | "--help" | "-h"
        )
}

fn segment_read_only(seg: &str) -> bool {
    let words: Vec<String> = match shlex::split(seg) {
        Some(w) => w,
        None => return false,
    };
    // Skip leading env assignments.
    let words: Vec<&str> = words
        .iter()
        .map(|s| s.as_str())
        .skip_while(|w| w.contains('=') && !w.starts_with('-'))
        .collect();
    let Some(&cmd) = words.first() else {
        return true;
    };
    let cmd = cmd.rsplit('/').next().unwrap_or(cmd);
    if is_version_query(&words) {
        return true;
    }
    if ALWAYS_SAFE.contains(&cmd) {
        return true;
    }
    let sub = words.get(1).copied().unwrap_or("");
    match cmd {
        "find" => !words.iter().any(|w| {
            matches!(
                *w,
                "-exec"
                    | "-execdir"
                    | "-delete"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprintf"
                    | "-fls"
            )
        }),
        "sed" => {
            words.contains(&"-n") && !words.iter().any(|w| w.starts_with("-i") || w.contains('w'))
        }
        "git" => {
            // Skip global options (and their arguments) to find the subcommand.
            let mut si = 1;
            while si < words.len() && words[si].starts_with('-') {
                if matches!(
                    words[si],
                    "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
                ) {
                    si += 1;
                }
                si += 1;
            }
            if si >= words.len() {
                return words.len() > 1;
            }
            let s = words[si];
            let rest = &words[si + 1..];
            match s {
                "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "ls-tree"
                | "blame" | "describe" | "shortlog" | "grep" | "cat-file" | "reflog"
                | "whatchanged" | "merge-base" | "rev-list" | "count-objects" | "check-ignore"
                | "name-rev" | "for-each-ref" | "show-ref" | "var" => true,
                "branch" => rest.iter().all(|w| {
                    matches!(
                        *w,
                        "-a" | "-r"
                            | "-v"
                            | "-vv"
                            | "--list"
                            | "--all"
                            | "--show-current"
                            | "--merged"
                            | "--no-merged"
                    )
                }),
                "remote" => {
                    rest.is_empty()
                        || rest == ["-v"]
                        || rest.first() == Some(&"show")
                        || rest.first() == Some(&"get-url")
                }
                "tag" => {
                    rest.is_empty() || rest.iter().all(|w| matches!(*w, "-l" | "--list" | "-n"))
                }
                "stash" => rest.first() == Some(&"list") || rest.first() == Some(&"show"),
                "worktree" => rest.first() == Some(&"list"),
                "config" => rest.iter().any(|w| {
                    matches!(*w, "--get" | "--list" | "-l" | "--get-all" | "--get-regexp")
                }),
                _ => false,
            }
        }
        "cargo" => matches!(
            sub,
            "metadata"
                | "tree"
                | "version"
                | "--version"
                | "search"
                | "locate-project"
                | "pkgid"
                | "verify-project"
        ),
        "go" => matches!(sub, "version" | "env" | "list" | "doc"),
        "npm" | "pnpm" | "yarn" | "bun" => {
            matches!(sub, "ls" | "list" | "view" | "info" | "why" | "outdated" | "--version" | "-v" | "config" if sub != "config" || words.get(2) == Some(&"get") || words.get(2) == Some(&"list"))
        }
        "pip" | "pip3" => matches!(sub, "list" | "show" | "freeze" | "--version"),
        "docker" | "podman" => matches!(
            sub,
            "ps" | "images" | "inspect" | "version" | "info" | "logs"
        ),
        "kubectl" => {
            matches!(
                sub,
                "get" | "describe" | "logs" | "version" | "explain" | "top"
            ) || (sub == "config" && words.get(2) == Some(&"view"))
        }
        "gh" => {
            matches!(sub, "pr" | "issue" | "run" | "repo" | "release")
                && matches!(
                    words.get(2).copied(),
                    Some("view" | "list" | "status" | "diff" | "checks")
                )
        }
        "rustc" | "node" | "python" | "python3" | "ruby" | "java" | "deno" | "go1" => {
            is_version_query(&words)
        }
        "command" => sub == "-v",
        _ => false,
    }
}

/// True if every sub-command is known to be read-only.
pub fn is_read_only_command(cmd: &str) -> bool {
    let Some(parts) = split_commands(cmd) else {
        return false;
    };
    !parts.is_empty() && parts.iter().all(|p| segment_read_only(p))
}

/// Commands that stay behind a prompt even in `auto` mode.
pub fn dangerous(cmd: &str) -> Option<String> {
    let c = format!(" {} ", cmd.replace('\n', " ; "));
    let checks: &[(&str, &str)] = &[
        (r"(^|[\s;&|])sudo\s", "uses sudo"),
        (
            r"\brm\s+(-\S+\s+)*-[a-zA-Z]*[rR][a-zA-Z]*\s+(\S+\s+)*(/\*?|~\S*|\$HOME\S*|\.\.(/\S*)?)(\s|$)",
            "recursive delete outside the workspace",
        ),
        (r"\bgit\s+push\b", "pushes to a remote"),
        (
            r"\bgit\s+reset\s+--hard\b",
            "discards changes (git reset --hard)",
        ),
        (
            r"\bgit\s+clean\s+-[a-zA-Z]*f",
            "deletes untracked files (git clean)",
        ),
        (
            r"\bgit\s+(checkout|restore)\s+(--\s+)?\.(\s|$)",
            "discards working-tree changes",
        ),
        (
            r"\bgit\s+(rebase|filter-branch|filter-repo)\b",
            "rewrites git history",
        ),
        (r"\bgit\s+commit\b.*--amend", "rewrites the last commit"),
        (r"\bgit\s+branch\s+-D\b", "force-deletes a branch"),
        (r"\b(mkfs|fdisk|parted)\b", "touches disks/partitions"),
        (r"\bdd\s+.*\bof=", "raw disk write (dd)"),
        (
            r"(curl|wget)[^|]*\|\s*(sudo\s+)?(ba|z)?sh\b",
            "pipes a download into a shell",
        ),
        (
            r"\b(shutdown|reboot|halt|poweroff)\b",
            "shuts down the machine",
        ),
        (r"\bchmod\s+(-R\s+)?777\b", "makes files world-writable"),
        (
            r"\b(npm|cargo|pnpm|yarn|twine|gem)\s+publish\b",
            "publishes a package",
        ),
        (
            r"\bdocker\s+(system|volume|image)\s+prune\b",
            "prunes docker data",
        ),
        (
            r"\bkubectl\s+(delete|apply|drain)\b",
            "changes a Kubernetes cluster",
        ),
        (r"\bterraform\s+(apply|destroy)\b", "changes infrastructure"),
        (
            r"(?i)\bdrop\s+(table|database)\b",
            "drops a database object",
        ),
        (r">\s*/dev/sd", "writes to a block device"),
        (r":\(\)\s*\{", "fork bomb"),
    ];
    for (re, why) in checks {
        if regex::Regex::new(re)
            .map(|r| r.is_match(&c))
            .unwrap_or(false)
        {
            return Some(why.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_classifier() {
        for ok in [
            "ls -la",
            "git status && git diff HEAD~1",
            "cat foo.txt | grep bar | wc -l",
            "rg -n 'fn main' src",
            "find . -name '*.rs'",
            "git log --oneline -20",
            "cargo metadata --format-version 1 2>/dev/null",
            "node --version",
            "FOO=1 ls",
            "git branch",
            "git -C sub status",
        ] {
            assert!(is_read_only_command(ok), "should be read-only: {ok}");
        }
        for bad in [
            "rm -rf build",
            "ls > out.txt",
            "echo $(whoami)",
            "find . -delete",
            "git push",
            "git branch -D x",
            "cargo build",
            "npm install",
            "sed -i s/a/b/ f",
            "cat x & rm y",
            "python script.py",
            "ls; rm -rf /",
            "git commit -m 'x'",
            "bash -c 'ls'",
        ] {
            assert!(!is_read_only_command(bad), "should NOT be read-only: {bad}");
        }
    }

    #[test]
    fn dangerous_detection() {
        assert!(dangerous("sudo apt install x").is_some());
        assert!(dangerous("git push origin main").is_some());
        assert!(dangerous("rm -rf /").is_some());
        assert!(dangerous("rm -rf ~/x").is_some());
        assert!(dangerous("curl -fsSL x.sh | sh").is_some());
        assert!(dangerous("rm -rf target").is_none());
        assert!(dangerous("cargo test").is_none());
        assert!(dangerous("git commit -m 'fix'").is_none());
    }

    #[test]
    fn rules() {
        assert!(rule_matches(
            "bash(cargo test*)",
            "bash",
            "bash",
            "cargo test --all"
        ));
        assert!(rule_matches(
            "Bash(npm run test:*)",
            "bash",
            "bash",
            "npm run test -- --watch=false"
        ));
        assert!(!rule_matches(
            "bash(cargo test*)",
            "bash",
            "bash",
            "cargo test && rm -rf /"
        ));
        assert!(rule_matches("edit(src/*)", "edit", "write", "src/main.rs"));
        assert!(rule_matches("edit", "edit", "multi_edit", "x"));
        assert!(rule_matches(
            "mcp__github__*",
            "mcp__github__create_issue",
            "mcp__github__create_issue",
            ""
        ));
        assert!(!rule_matches("read(.env*)", "read", "read", "src/.envx/a"));
        assert!(rule_matches(
            "read(*.env*)",
            "read",
            "read",
            "config/.env.local"
        ));
    }

    #[test]
    fn rule_suggestions() {
        assert_eq!(
            suggest_rule("bash", "bash", "cargo test --all"),
            "bash(cargo test*)"
        );
        assert_eq!(
            suggest_rule("bash", "bash", "./run.sh x"),
            "bash(./run.sh*)"
        );
        assert_eq!(suggest_rule("bash", "bash", "make"), "bash(make*)");
    }
}

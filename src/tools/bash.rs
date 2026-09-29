//! Shell execution: foreground commands with live output, timeouts, process
//! group cleanup, persistent working directory, optional OS sandbox; plus
//! background jobs for servers/watchers.

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_bool, arg_opt_str, arg_str, arg_u64};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

const MAX_TIMEOUT_SECS: u64 = 1800;
const OUTPUT_LINES: usize = 400;
const OUTPUT_BYTES: usize = 30_000;
const CAPTURE_LIMIT: usize = 16 * 1024 * 1024;

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('@'..='~').contains(&c2) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: until BEL or ST
                    while let Some(c2) = chars.next() {
                        if c2 == '\x07' {
                            break;
                        }
                        if c2 == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Collapse carriage-return progress bars to their final state per line.
fn collapse_cr(line: &str) -> &str {
    let l = line.trim_end_matches('\r');
    match l.rfind('\r') {
        Some(i) => &l[i + 1..],
        None => l,
    }
}

fn shell_env(cmd: &mut Command) {
    cmd.env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("MANPAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("HARNESS", "1");
}

pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid as i32, libc::SIGTERM);
    }
    let _ = pid;
}

fn kill_group_hard(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid as i32, libc::SIGKILL);
    }
    let _ = pid;
}

fn build_command(ctx: &ToolCtx, script: &str) -> Command {
    let cfg = ctx.shared.cfg();
    let argv: Vec<String> = if ctx.sandbox {
        crate::sandbox::wrap(ctx.root(), cfg.sandbox_network, script)
    } else {
        vec!["bash".into(), "-c".into(), script.into()]
    };
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(ctx.cwd());
    shell_env(&mut cmd);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.kill_on_drop(true);
    cmd
}

pub struct CommandResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub interrupted: bool,
    pub duration: Duration,
    pub new_cwd: Option<PathBuf>,
}

/// Run a script in the foreground, streaming lines to the UI.
pub async fn run_command(
    ctx: &ToolCtx,
    command: &str,
    timeout: Duration,
) -> std::io::Result<CommandResult> {
    let cwd_file = std::env::temp_dir().join(format!("harness-cwd-{}", crate::util::short_id()));
    let script = format!(
        "{{\n{command}\n}} 2>&1\n__harness_ec=$?\npwd -P > '{}' 2>/dev/null\nexit $__harness_ec",
        cwd_file.display()
    );
    let mut cmd = build_command(ctx, &script);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let (cap2, events, id) = (captured.clone(), ctx.events.clone(), ctx.call_id.clone());
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).split(b'\n');
        let mut n = 0usize;
        while let Ok(Some(raw)) = lines.next_segment().await {
            let s = String::from_utf8_lossy(&raw);
            let clean = strip_ansi(collapse_cr(&s));
            {
                let mut c = cap2.lock();
                if c.len() < CAPTURE_LIMIT {
                    c.push_str(&clean);
                    c.push('\n');
                }
            }
            n += 1;
            if n <= 20_000 {
                events.send(crate::agent::events::AgentEvent::ToolProgress {
                    id: id.clone(),
                    line: clean,
                });
            }
        }
    });
    // stderr of the wrapper itself (sandbox errors); the command's stderr is merged into stdout.
    let err_reader = tokio::spawn(async move {
        let mut s = String::new();
        let _ = BufReader::new(stderr)
            .take(64 * 1024)
            .read_to_string(&mut s)
            .await;
        s
    });

    let mut timed_out = false;
    let mut interrupted = false;
    let status = tokio::select! {
        s = child.wait() => s.ok(),
        _ = tokio::time::sleep(timeout) => { timed_out = true; None }
        _ = ctx.cancel.cancelled() => { interrupted = true; None }
    };
    let status = match status {
        Some(s) => Some(s),
        None => {
            kill_group(pid);
            let s = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            match s {
                Ok(Ok(s)) => Some(s),
                _ => {
                    kill_group_hard(pid);
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            }
        }
    };
    // Background processes may keep the pipe open; don't wait for them.
    let _ = tokio::time::timeout(Duration::from_millis(300), reader).await;
    let wrapper_err = tokio::time::timeout(Duration::from_millis(100), err_reader)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let mut output = std::mem::take(&mut *captured.lock());
    if !wrapper_err.trim().is_empty() {
        output.push_str(&wrapper_err);
    }
    let new_cwd = std::fs::read_to_string(&cwd_file)
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.is_dir());
    let _ = std::fs::remove_file(&cwd_file);
    Ok(CommandResult {
        output,
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        interrupted,
        duration: start.elapsed(),
        new_cwd,
    })
}

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> String {
        "Run a bash command. The working directory persists between calls (`cd` sticks); environment \
variables do not. stdout+stderr are combined; long output keeps the head and tail and the full log is saved \
to a file you can `read`. Default timeout 120s (max 1800s via `timeout`). Commands are non-interactive: \
never use editors, pagers or prompts (pass -y/--yes, `git --no-pager`, `git commit -m`). \
Set `background: true` for servers/watchers and check them with the `jobs` tool. \
Prefer the dedicated tools over shell equivalents: `read` not cat/head, `grep` not grep/rg, `glob` not find, \
`edit` not sed. Chain dependent commands with `&&`; issue independent read-only commands as parallel calls."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "description": {"type": "string", "description": "5-10 word summary of what it does"},
                "timeout": {"type": "integer", "description": "Timeout in seconds"},
                "background": {"type": "boolean", "description": "Run as a background job"}
            },
            "required": ["command"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Exec
    }
    fn summarize(&self, args: &Value) -> String {
        let c = arg_opt_str(args, "command").unwrap_or("?");
        let mut s = crate::util::first_line(c, 200);
        if arg_bool(args, "background") {
            s.push_str("  &");
        }
        s
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let command = match arg_str(&args, "command") {
            Ok(c) if !c.trim().is_empty() => c.to_string(),
            Ok(_) => return ToolOutput::err("command is empty"),
            Err(e) => return ToolOutput::err(e),
        };
        if arg_bool(&args, "background") {
            return start_job(ctx, &command).await;
        }
        let default = ctx.shared.cfg().bash_timeout;
        let mut secs = arg_u64(&args, "timeout").unwrap_or(default);
        // Some models pass milliseconds.
        if secs > MAX_TIMEOUT_SECS * 10 {
            secs /= 1000;
        }
        let timeout = Duration::from_secs(secs.clamp(1, MAX_TIMEOUT_SECS));
        let res = match run_command(ctx, &command, timeout).await {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("failed to start bash: {e}")),
        };
        if let Some(cwd) = &res.new_cwd {
            *ctx.shared.cwd.lock() = cwd.clone();
        }
        format_result(ctx, &res, timeout)
    }
}

fn format_result(ctx: &ToolCtx, res: &CommandResult, timeout: Duration) -> ToolOutput {
    let (body, truncated) =
        crate::util::head_tail(res.output.trim_end(), OUTPUT_LINES, OUTPUT_BYTES);
    let mut content = if body.trim().is_empty() {
        "(no output)".to_string()
    } else {
        body
    };
    if truncated {
        if let Some(p) = ctx.shared.spill("bash", &res.output) {
            content.push_str(&format!(
                "\n[full output ({} bytes) saved to {} — use read/grep on it]",
                res.output.len(),
                p.display()
            ));
        }
    }
    let dur = crate::util::fmt_duration(res.duration);
    let mut is_error = false;
    let summary;
    if res.interrupted {
        content.push_str("\n[interrupted by user]");
        is_error = true;
        summary = "interrupted".to_string();
    } else if res.timed_out {
        content.push_str(&format!(
            "\n[timed out after {}s and was killed. For long-running processes use background=true; otherwise raise `timeout`]",
            timeout.as_secs()
        ));
        is_error = true;
        summary = format!("timeout {}s", timeout.as_secs());
    } else {
        match res.exit_code {
            Some(0) => summary = format!("ok · {dur}"),
            Some(c) => {
                content.push_str(&format!("\n[exit code {c}]"));
                is_error = true;
                summary = format!("exit {c} · {dur}");
            }
            None => {
                content.push_str("\n[terminated by signal]");
                is_error = true;
                summary = "killed".into();
            }
        }
    }
    if res.exit_code == Some(126) && res.output.contains("harness sandbox") {
        content.push_str("\n[the sandbox blocked this command]");
    }
    if let Some(cwd) = &res.new_cwd {
        let root = ctx.root();
        if !cwd.starts_with(root) {
            content.push_str(&format!(
                "\n[cwd is now {} — outside the workspace]",
                cwd.display()
            ));
        }
    }
    ToolOutput {
        content,
        is_error,
        summary,
        ..Default::default()
    }
}

// ───────────────────────────── background jobs ─────────────────────────────

pub struct Job {
    pub id: String,
    pub command: String,
    pub pid: u32,
    pub log: PathBuf,
    pub started: Instant,
    child: Child,
    pub exit: Option<i32>,
}

#[derive(Default)]
pub struct Jobs {
    jobs: Mutex<Vec<Job>>,
}

impl Jobs {
    pub fn kill_all(&self) {
        for j in self.jobs.lock().iter_mut() {
            if j.exit.is_none() {
                kill_group(j.pid);
            }
        }
    }

    pub fn running(&self) -> usize {
        let mut jobs = self.jobs.lock();
        jobs.iter_mut()
            .filter_map(|j| if poll(j).is_none() { Some(()) } else { None })
            .count()
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.kill_all();
    }
}

fn poll(j: &mut Job) -> Option<i32> {
    if j.exit.is_none() {
        if let Ok(Some(st)) = j.child.try_wait() {
            j.exit = Some(st.code().unwrap_or(-1));
        }
    }
    j.exit
}

fn tail_file(p: &Path, n: usize) -> String {
    let s = std::fs::read(p)
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    strip_ansi(
        &lines[start..]
            .iter()
            .map(|l| collapse_cr(l))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

async fn start_job(ctx: &ToolCtx, command: &str) -> ToolOutput {
    let id = format!("job{}", &crate::util::short_id()[..4]);
    let _ = std::fs::create_dir_all(&ctx.shared.spill_dir);
    let log = ctx.shared.spill_dir.join(format!("{id}.log"));
    let script = format!("exec > '{}' 2>&1\n{command}", log.display());
    let mut cmd = build_command(ctx, &script);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.kill_on_drop(false);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("failed to start: {e}")),
    };
    let pid = child.id().unwrap_or(0);
    ctx.shared.jobs.jobs.lock().push(Job {
        id: id.clone(),
        command: command.to_string(),
        pid,
        log: log.clone(),
        started: Instant::now(),
        child,
        exit: None,
    });
    // Give it a moment so early failures (port in use, typo) are visible.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut jobs = ctx.shared.jobs.jobs.lock();
    let job = jobs.iter_mut().find(|j| j.id == id).unwrap();
    let status = match poll(job) {
        Some(c) => format!("exited with code {c}"),
        None => "running".into(),
    };
    let tail = tail_file(&log, 20);
    ToolOutput::ok(format!(
        "Started background job {id} (pid {pid}), status: {status}. Log: {}\nFirst output:\n{}\nUse the `jobs` tool to see more output or stop it.",
        log.display(),
        if tail.is_empty() { "(none yet)".into() } else { tail }
    ))
    .with_summary(format!("{id} {status}"))
}

pub struct JobsTool;

#[async_trait]
impl Tool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }
    fn description(&self) -> String {
        "Manage background jobs started with bash(background=true): `list` all jobs, `output` to see the latest \
lines of a job's log, `kill` to stop one."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "output", "kill"]},
                "id": {"type": "string"},
                "lines": {"type": "integer", "description": "Lines of output to show (default 60)"}
            },
            "required": ["action"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        format!(
            "{} {}",
            arg_opt_str(args, "action").unwrap_or("list"),
            arg_opt_str(args, "id").unwrap_or("")
        )
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let action = arg_opt_str(&args, "action").unwrap_or("list");
        let mut jobs = ctx.shared.jobs.jobs.lock();
        match action {
            "list" => {
                if jobs.is_empty() {
                    return ToolOutput::ok("No background jobs.");
                }
                let mut s = String::new();
                for j in jobs.iter_mut() {
                    let st = match poll(j) {
                        Some(c) => format!("exited({c})"),
                        None => "running".into(),
                    };
                    s.push_str(&format!(
                        "{}  {}  {}  {}\n",
                        j.id,
                        st,
                        crate::util::fmt_duration(j.started.elapsed()),
                        crate::util::first_line(&j.command, 80)
                    ));
                }
                ToolOutput::ok(s)
            }
            "output" | "kill" => {
                let Some(id) = arg_opt_str(&args, "id") else {
                    return ToolOutput::err("`id` is required");
                };
                let Some(j) = jobs.iter_mut().find(|j| j.id == id) else {
                    return ToolOutput::err(format!("no job `{id}`"));
                };
                if action == "kill" {
                    if poll(j).is_none() {
                        kill_group(j.pid);
                    }
                    return ToolOutput::ok(format!("Sent SIGTERM to job {id}."))
                        .with_summary("killed");
                }
                let n = arg_u64(&args, "lines").unwrap_or(60).clamp(1, 2000) as usize;
                let st = match poll(j) {
                    Some(c) => format!("exited with code {c}"),
                    None => "running".into(),
                };
                let tail = tail_file(&j.log, n);
                ToolOutput::ok(format!("Job {id}: {st}\n{tail}")).with_summary(st)
            }
            other => ToolOutput::err(format!("unknown action `{other}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_and_cr() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m ok"), "red ok");
        assert_eq!(strip_ansi("\x1b]0;title\x07x"), "x");
        assert_eq!(collapse_cr("10%\r50%\r100%"), "100%");
    }
}

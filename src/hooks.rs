//! User-configured lifecycle hooks (shell commands).
//!
//! The event payload is passed as JSON on stdin. Exit code 0 = ok (stdout is
//! added as context for `session_start`/`user_prompt`), exit code 2 = block
//! (stderr is fed back to the model), anything else = warning for the user.

use crate::config::HookConfig;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Default)]
pub struct HookOutcome {
    /// Set when a hook exited with code 2.
    pub block: Option<String>,
    /// stdout of successful hooks.
    pub context: Vec<String>,
    /// Non-fatal problems to show the user.
    pub warnings: Vec<String>,
}

pub async fn run(
    hooks: &[HookConfig],
    event: &str,
    tool: Option<&str>,
    payload: &Value,
    cwd: &Path,
) -> HookOutcome {
    let mut out = HookOutcome::default();
    for h in hooks.iter().filter(|h| h.event == event) {
        if let Some(t) = tool
            && !h.matcher.is_empty()
        {
            let re = regex::Regex::new(&format!("^(?:{})$", h.matcher));
            match re {
                Ok(re) if re.is_match(t) => {}
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(&h.command)
            .current_dir(cwd)
            .env("HARNESS_EVENT", event)
            .env("HARNESS_TOOL", tool.unwrap_or(""))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                out.warnings
                    .push(format!("hook `{}` failed to start: {e}", h.command));
                continue;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let data = payload.to_string();
            let _ = stdin.write_all(data.as_bytes()).await;
        }
        let res =
            tokio::time::timeout(Duration::from_secs(h.timeout), child.wait_with_output()).await;
        match res {
            Err(_) => out.warnings.push(format!(
                "hook `{}` timed out after {}s",
                h.command, h.timeout
            )),
            Ok(Err(e)) => out
                .warnings
                .push(format!("hook `{}` failed: {e}", h.command)),
            Ok(Ok(o)) => {
                let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                match o.status.code() {
                    Some(0) => {
                        if !stdout.is_empty() {
                            out.context.push(stdout);
                        }
                    }
                    Some(2) => {
                        let msg = if stderr.is_empty() { stdout } else { stderr };
                        out.block = Some(if msg.is_empty() {
                            format!("blocked by hook `{}`", h.command)
                        } else {
                            msg
                        });
                        return out;
                    }
                    code => out.warnings.push(format!(
                        "hook `{}` exited with {}: {}",
                        h.command,
                        code.map(|c| c.to_string()).unwrap_or("signal".into()),
                        crate::util::first_line(&stderr, 200)
                    )),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exit_codes() {
        let hooks = vec![
            HookConfig {
                event: "pre_tool".into(),
                matcher: "bash".into(),
                command: "cat >/dev/null; echo ctx".into(),
                timeout: 5,
            },
            HookConfig {
                event: "pre_tool".into(),
                matcher: "edit|write".into(),
                command: "echo nope >&2; exit 2".into(),
                timeout: 5,
            },
        ];
        let o = run(
            &hooks,
            "pre_tool",
            Some("bash"),
            &serde_json::json!({}),
            Path::new("."),
        )
        .await;
        assert!(o.block.is_none());
        assert_eq!(o.context, vec!["ctx".to_string()]);
        let o = run(
            &hooks,
            "pre_tool",
            Some("write"),
            &serde_json::json!({}),
            Path::new("."),
        )
        .await;
        assert_eq!(o.block.as_deref(), Some("nope"));
        let o = run(
            &hooks,
            "pre_tool",
            Some("read"),
            &serde_json::json!({}),
            Path::new("."),
        )
        .await;
        assert!(o.block.is_none() && o.context.is_empty());
    }
}

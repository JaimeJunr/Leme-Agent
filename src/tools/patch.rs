//! `apply_patch` — the envelope diff format OpenAI models are trained on:
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path/new.rs
//! +contents
//! *** Update File: path/old.rs
//! *** Move to: path/renamed.rs      (optional)
//! @@ fn context_header()            (optional)
//!  context
//! -removed
//! +added
//! *** Delete File: path/gone.rs
//! *** End Patch
//! ```

use super::fs::unified_diff;
use super::{Display, Tool, ToolCtx, ToolKind, ToolOutput, arg_str};
use crate::util::{display_path, resolve_path};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum Op {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, PartialEq, Default)]
pub struct Hunk {
    pub header: Option<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
    pub eof: bool,
}

pub fn parse(patch: &str) -> Result<Vec<Op>, String> {
    let text = patch.replace("\r\n", "\n");
    let mut lines: Vec<&str> = text.lines().collect();
    // Tolerate missing envelope / heredoc wrappers.
    while lines
        .first()
        .map(|l| l.trim().is_empty() || l.starts_with("apply_patch") || l.starts_with("<<"))
        .unwrap_or(false)
    {
        lines.remove(0);
    }
    let start = lines
        .iter()
        .position(|l| l.trim() == "*** Begin Patch")
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = lines
        .iter()
        .rposition(|l| l.trim() == "*** End Patch")
        .unwrap_or(lines.len());
    if end < start {
        return Err("malformed patch envelope".into());
    }
    let body = &lines[start..end];
    let mut ops = vec![];
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        if let Some(p) = line.strip_prefix("*** Add File:") {
            let path = p.trim().to_string();
            i += 1;
            let mut content = String::new();
            while i < body.len() && !body[i].starts_with("*** ") {
                let l = body[i];
                content.push_str(l.strip_prefix('+').unwrap_or(l));
                content.push('\n');
                i += 1;
            }
            ops.push(Op::Add { path, content });
        } else if let Some(p) = line.strip_prefix("*** Delete File:") {
            ops.push(Op::Delete {
                path: p.trim().to_string(),
            });
            i += 1;
        } else if let Some(p) = line.strip_prefix("*** Update File:") {
            let path = p.trim().to_string();
            i += 1;
            let mut move_to = None;
            if i < body.len()
                && let Some(m) = body[i].strip_prefix("*** Move to:")
            {
                move_to = Some(m.trim().to_string());
                i += 1;
            }
            let mut hunks: Vec<Hunk> = vec![];
            let mut cur: Option<Hunk> = None;
            while i < body.len() {
                let l = body[i];
                if l.starts_with("*** End of File") {
                    if let Some(h) = cur.as_mut() {
                        h.eof = true;
                    }
                    i += 1;
                    continue;
                }
                if l.starts_with("*** ") {
                    break;
                }
                if let Some(h) = l.strip_prefix("@@") {
                    if let Some(c) = cur.take()
                        && (!c.old.is_empty() || !c.new.is_empty())
                    {
                        hunks.push(c);
                    }
                    let h = h.trim().trim_end_matches("@@").trim();
                    cur = Some(Hunk {
                        header: if h.is_empty() {
                            None
                        } else {
                            Some(h.to_string())
                        },
                        ..Default::default()
                    });
                    i += 1;
                    continue;
                }
                let h = cur.get_or_insert_with(Hunk::default);
                if let Some(r) = l.strip_prefix('-') {
                    h.old.push(r.to_string());
                } else if let Some(a) = l.strip_prefix('+') {
                    h.new.push(a.to_string());
                } else {
                    let c = l.strip_prefix(' ').unwrap_or(l);
                    h.old.push(c.to_string());
                    h.new.push(c.to_string());
                }
                i += 1;
            }
            if let Some(c) = cur.take()
                && (!c.old.is_empty() || !c.new.is_empty())
            {
                hunks.push(c);
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("Update File {path}: no hunks"));
            }
            ops.push(Op::Update {
                path,
                move_to,
                hunks,
            });
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            return Err(format!(
                "unexpected line in patch: `{}`. Each file section must start with `*** Add File:`, `*** Update File:` or `*** Delete File:`",
                crate::util::ellipsize(line, 80)
            ));
        }
    }
    if ops.is_empty() {
        return Err("patch contains no operations".into());
    }
    Ok(ops)
}

fn find_seq(lines: &[String], pat: &[String], from: usize, eof: bool) -> Option<usize> {
    if pat.is_empty() {
        return Some(if eof {
            lines.len()
        } else {
            from.min(lines.len())
        });
    }
    if pat.len() > lines.len() {
        return None;
    }
    let cmps: [fn(&str, &str) -> bool; 3] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
    ];
    for cmp in cmps {
        let range: Box<dyn Iterator<Item = usize>> = if eof {
            Box::new(std::iter::once(lines.len() - pat.len()))
        } else {
            Box::new(from..=lines.len() - pat.len())
        };
        for s in range {
            if pat.iter().enumerate().all(|(k, p)| cmp(&lines[s + k], p)) {
                return Some(s);
            }
        }
        // Fall back to searching from the top (hunks out of order).
        if from > 0 && !eof {
            for s in 0..from.min(lines.len() - pat.len() + 1) {
                if pat.iter().enumerate().all(|(k, p)| cmp(&lines[s + k], p)) {
                    return Some(s);
                }
            }
        }
    }
    None
}

pub fn apply_hunks(content: &str, hunks: &[Hunk], path: &str) -> Result<String, String> {
    let trailing_nl = content.ends_with('\n') || content.is_empty();
    let mut lines: Vec<String> = content
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    let mut cursor = 0usize;
    for (n, h) in hunks.iter().enumerate() {
        let mut from = cursor;
        if let Some(hdr) = &h.header {
            let t = hdr.trim();
            if let Some(pos) = lines
                .iter()
                .skip(cursor)
                .position(|l| l.trim() == t || l.contains(t))
            {
                from = cursor + pos;
            } else if let Some(pos) = lines.iter().position(|l| l.trim() == t || l.contains(t)) {
                from = pos;
            }
        }
        let at = find_seq(&lines, &h.old, from, h.eof).ok_or_else(|| {
            let preview: Vec<String> = h.old.iter().take(6).map(|l| format!("  {l}")).collect();
            format!(
                "{path}: hunk #{} did not match the file. Expected lines:\n{}\nRe-read the file and regenerate the patch with exact context lines.",
                n + 1,
                preview.join("\n")
            )
        })?;
        lines.splice(at..at + h.old.len(), h.new.iter().cloned());
        cursor = at + h.new.len();
    }
    let mut out = lines.join("\n");
    if trailing_nl && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

enum Planned {
    Write {
        path: PathBuf,
        old: String,
        new: String,
    },
    Remove {
        path: PathBuf,
        old: String,
    },
}

fn plan(cwd: &Path, patch: &str) -> Result<Vec<Planned>, String> {
    let ops = parse(patch)?;
    let mut planned = vec![];
    for op in ops {
        match op {
            Op::Add { path, content } => {
                let p = resolve_path(cwd, &path);
                let old = std::fs::read_to_string(&p).unwrap_or_default();
                planned.push(Planned::Write {
                    path: p,
                    old,
                    new: content,
                });
            }
            Op::Delete { path } => {
                let p = resolve_path(cwd, &path);
                let old =
                    std::fs::read_to_string(&p).map_err(|e| format!("Delete File {path}: {e}"))?;
                planned.push(Planned::Remove { path: p, old });
            }
            Op::Update {
                path,
                move_to,
                hunks,
            } => {
                let p = resolve_path(cwd, &path);
                let old =
                    std::fs::read_to_string(&p).map_err(|e| format!("Update File {path}: {e}"))?;
                let new = apply_hunks(&old, &hunks, &path)?;
                match move_to {
                    Some(m) => {
                        let np = resolve_path(cwd, &m);
                        planned.push(Planned::Remove {
                            path: p,
                            old: old.clone(),
                        });
                        planned.push(Planned::Write {
                            path: np,
                            old: String::new(),
                            new,
                        });
                    }
                    None => planned.push(Planned::Write { path: p, old, new }),
                }
            }
        }
    }
    Ok(planned)
}

pub struct ApplyPatchTool;

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> String {
        "Edit files with a patch. Format:\n*** Begin Patch\n*** Update File: path/to/file\n@@ optional line to anchor the hunk (e.g. a function signature)\n context line (starts with a space)\n-removed line\n+added line\n context line\n*** Add File: path/new_file\n+line of new file\n*** Delete File: path/old_file\n*** End Patch\n\
Include ~3 lines of unchanged context around each change so hunks match unambiguously; use several @@ hunks for several changes. \
`*** Move to: new/path` right after an Update header renames the file. All file changes apply atomically."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"patch": {"type": "string", "description": "The full patch text"}},
            "required": ["patch"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn summarize(&self, args: &Value) -> String {
        let p = args
            .get("patch")
            .or_else(|| args.get("input"))
            .and_then(|p| p.as_str())
            .unwrap_or("");
        let files: Vec<&str> = p
            .lines()
            .filter_map(|l| {
                l.strip_prefix("*** Update File:")
                    .or_else(|| l.strip_prefix("*** Add File:"))
                    .or_else(|| l.strip_prefix("*** Delete File:"))
            })
            .map(|s| s.trim())
            .collect();
        files.join(", ")
    }
    fn preview(&self, ctx: &ToolCtx, args: &Value) -> Option<String> {
        let patch = args.get("patch").or_else(|| args.get("input"))?.as_str()?;
        let planned = plan(&ctx.cwd(), patch).ok()?;
        let mut s = String::new();
        for p in planned {
            match p {
                Planned::Write { path, old, new } => {
                    s.push_str(&unified_diff(&display_path(ctx.root(), &path), &old, &new))
                }
                Planned::Remove { path, old } => {
                    s.push_str(&unified_diff(&display_path(ctx.root(), &path), &old, ""))
                }
            }
        }
        Some(s)
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let patch = match arg_str(&args, "patch").or_else(|_| arg_str(&args, "input")) {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let planned = match plan(&ctx.cwd(), &patch) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        {
            let files = ctx.shared.files.lock();
            for p in &planned {
                let path = match p {
                    Planned::Write { path, .. } | Planned::Remove { path, .. } => path,
                };
                if let Err(e) = files.check_fresh(path) {
                    return ToolOutput::err(e);
                }
            }
        }
        let mut summary = vec![];
        let mut diffs = String::new();
        for p in &planned {
            match p {
                Planned::Write { path, old, new } => {
                    ctx.shared.checkpoint(path);
                    if let Some(d) = path.parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    if let Err(e) = std::fs::write(path, new) {
                        return ToolOutput::err(format!(
                            "writing {}: {e} (earlier files in this patch were already written)",
                            path.display()
                        ));
                    }
                    ctx.shared.note_modified(path);
                    let rel = display_path(ctx.root(), path);
                    diffs.push_str(&unified_diff(&rel, old, new));
                    summary.push(format!("{} {rel}", if old.is_empty() { "A" } else { "M" }));
                }
                Planned::Remove { path, old } => {
                    ctx.shared.checkpoint(path);
                    if let Err(e) = std::fs::remove_file(path) {
                        return ToolOutput::err(format!("deleting {}: {e}", path.display()));
                    }
                    ctx.shared.note_modified(path);
                    let rel = display_path(ctx.root(), path);
                    diffs.push_str(&unified_diff(&rel, old, ""));
                    summary.push(format!("D {rel}"));
                }
            }
        }
        ToolOutput::ok(format!("Patch applied:\n{}", summary.join("\n")))
            .with_summary(format!("{} files", summary.len()))
            .with_display(Display::Diff {
                path: summary.join(", "),
                diff: diffs,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_apply() {
        let patch = "*** Begin Patch\n*** Update File: a.py\n@@ def f():\n     x = 1\n-    return x\n+    return x + 1\n*** Add File: b.txt\n+hello\n*** Delete File: c.txt\n*** End Patch";
        let ops = parse(patch).unwrap();
        assert_eq!(ops.len(), 3);
        if let Op::Update { hunks, .. } = &ops[0] {
            let src = "import os\n\ndef f():\n    x = 1\n    return x\n";
            let out = apply_hunks(src, hunks, "a.py").unwrap();
            assert_eq!(out, "import os\n\ndef f():\n    x = 1\n    return x + 1\n");
        } else {
            panic!()
        }
        assert_eq!(
            ops[1],
            Op::Add {
                path: "b.txt".into(),
                content: "hello\n".into()
            }
        );
    }

    #[test]
    fn tolerant_whitespace_and_eof() {
        let hunks = vec![Hunk {
            header: None,
            old: vec!["b  ".into()],
            new: vec!["B".into()],
            eof: false,
        }];
        assert_eq!(apply_hunks("a\nb\nc\n", &hunks, "x").unwrap(), "a\nB\nc\n");
        let add_end = vec![Hunk {
            header: None,
            old: vec![],
            new: vec!["z".into()],
            eof: true,
        }];
        assert_eq!(apply_hunks("a\n", &add_end, "x").unwrap(), "a\nz\n");
    }

    #[test]
    fn mismatch_is_error() {
        let hunks = vec![Hunk {
            header: None,
            old: vec!["nope".into()],
            new: vec!["x".into()],
            eof: false,
        }];
        assert!(apply_hunks("a\n", &hunks, "x").is_err());
    }
}

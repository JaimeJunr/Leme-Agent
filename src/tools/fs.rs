//! File tools: read, ls, glob, write, edit, multi_edit.

use super::edit_match::{self, MatchError};
use super::{
    Display, Tool, ToolCtx, ToolKind, ToolOutput, arg_bool, arg_opt_str, arg_str, arg_u64,
};
use crate::conversation::Image;
use crate::util::{display_path, looks_binary, resolve_path};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const DEFAULT_READ_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_READ_BYTES: usize = 256 * 1024;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

fn image_mime(p: &Path) -> Option<&'static str> {
    match p.extension()?.to_str()?.to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

fn diff_stats(diff: &str) -> (usize, usize) {
    let mut add = 0;
    let mut del = 0;
    for l in diff.lines() {
        if l.starts_with('+') && !l.starts_with("+++") {
            add += 1;
        } else if l.starts_with('-') && !l.starts_with("---") {
            del += 1;
        }
    }
    (add, del)
}

/// Numbered excerpt around the changed region (helps the model verify an
/// edit without re-reading the file).
fn excerpt(content: &str, from_line: usize, to_line: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = from_line.saturating_sub(4);
    let end = (to_line + 3).min(lines.len());
    let mut s = String::new();
    for (i, l) in lines.iter().enumerate().take(end).skip(start) {
        s.push_str(&format!(
            "{:>6}\t{}\n",
            i + 1,
            crate::util::ellipsize(l, MAX_LINE_CHARS)
        ));
    }
    s
}

pub fn read_image(path: &Path) -> Result<Image, String> {
    let mime = image_mime(path).ok_or("not an image")?;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!("image too large ({} bytes)", meta.len()));
    }
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(Image {
        mime: mime.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(data),
        label: path.display().to_string(),
    })
}

// ───────────────────────────── read ─────────────────────────────

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> String {
        "Read a file from the filesystem. Returns lines prefixed with line numbers (cat -n format). \
By default reads up to 2000 lines from the start; use `offset` (1-based line) and `limit` for large files. \
Also reads images (png/jpg/gif/webp) so you can see them. Reading a directory lists it. \
Call this in parallel for several files you know you need. Always read a file before overwriting it with `write`."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute or workspace-relative path"},
                "offset": {"type": "integer", "description": "1-based line to start from"},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)"}
            },
            "required": ["path"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        let p = arg_opt_str(args, "path").unwrap_or("?").to_string();
        match (arg_u64(args, "offset"), arg_u64(args, "limit")) {
            (Some(o), Some(l)) => format!("{p}:{o}-{}", o + l),
            (Some(o), None) => format!("{p}:{o}-"),
            _ => p,
        }
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let p = match arg_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let path = resolve_path(&ctx.cwd(), p);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return ToolOutput::err(not_found_msg(&path, e, ctx.root())),
        };
        if meta.is_dir() {
            return list_dir(ctx, &path, 1).await;
        }
        if image_mime(&path).is_some() {
            return match read_image(&path) {
                Ok(img) => {
                    ctx.shared.files.lock().mark_read(&path);
                    ToolOutput {
                        content: format!(
                            "Image file {} ({} bytes). The image is attached.",
                            display_path(ctx.root(), &path),
                            meta.len()
                        ),
                        images: vec![img],
                        summary: "image".into(),
                        ..Default::default()
                    }
                }
                Err(e) => ToolOutput::err(e),
            };
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        if looks_binary(&bytes) {
            return ToolOutput::err(format!(
                "{} is a binary file ({} bytes); not shown. Use bash tools (e.g. `file`, `xxd | head`) if you need to inspect it.",
                path.display(),
                bytes.len()
            ));
        }
        let text = String::from_utf8_lossy(&bytes);
        ctx.shared.files.lock().mark_read(&path);
        let total = text.lines().count();
        if total == 0 {
            return ToolOutput::ok(format!("{} is empty.", display_path(ctx.root(), &path)))
                .with_summary("empty");
        }
        let offset = arg_u64(&args, "offset").unwrap_or(1).max(1) as usize;
        let limit = arg_u64(&args, "limit")
            .map(|l| l as usize)
            .unwrap_or(DEFAULT_READ_LINES)
            .max(1);
        if offset > total {
            return ToolOutput::err(format!(
                "offset {offset} is past the end of the file ({total} lines)"
            ));
        }
        let mut out = String::new();
        let mut shown = 0;
        let mut cut_bytes = false;
        for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let l = if line.chars().count() > MAX_LINE_CHARS {
                format!(
                    "{}… [line truncated]",
                    crate::util::ellipsize(line, MAX_LINE_CHARS)
                )
            } else {
                line.to_string()
            };
            out.push_str(&format!("{:>6}\t{}\n", i + 1, l));
            shown += 1;
            if out.len() > MAX_READ_BYTES {
                cut_bytes = true;
                break;
            }
        }
        let last = offset + shown - 1;
        if last < total || cut_bytes {
            out.push_str(&format!(
                "\n[showing lines {offset}-{last} of {total}. Use offset={} to continue.]\n",
                last + 1
            ));
        }
        if let Some(note) = crate::instructions::nested_for(&ctx.shared, &path) {
            out.push_str(&note);
        }
        let summary = if shown == total {
            format!("{total} lines")
        } else {
            format!("lines {offset}-{last} of {total}")
        };
        ToolOutput::ok(out).with_summary(summary)
    }
}

fn not_found_msg(path: &Path, e: std::io::Error, root: &Path) -> String {
    let mut msg = format!("cannot access {}: {e}", path.display());
    // Suggest similarly named files — saves a round trip.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let mut hits = vec![];
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .max_depth(Some(12))
            .build();
        for entry in walker.flatten().take(50_000) {
            if entry.file_name().to_str() == Some(name) {
                hits.push(display_path(root, entry.path()));
                if hits.len() >= 5 {
                    break;
                }
            }
        }
        if !hits.is_empty() {
            msg.push_str(&format!("\nDid you mean: {}", hits.join(", ")));
        }
    }
    msg
}

// ───────────────────────────── ls ─────────────────────────────

pub struct LsTool;

async fn list_dir(ctx: &ToolCtx, dir: &Path, depth: usize) -> ToolOutput {
    const LIMIT: usize = 500;
    let root = ctx.root().to_path_buf();
    let dir = dir.to_path_buf();
    let res = tokio::task::spawn_blocking(move || {
        let mut entries: Vec<(PathBuf, bool)> = vec![];
        let walker = ignore::WalkBuilder::new(&dir)
            .hidden(false)
            .max_depth(Some(depth))
            .filter_entry(|e| e.file_name() != ".git")
            .sort_by_file_path(|a, b| a.cmp(b))
            .build();
        let mut truncated = false;
        for e in walker.flatten() {
            if e.path() == dir {
                continue;
            }
            if entries.len() >= LIMIT {
                truncated = true;
                break;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push((e.path().to_path_buf(), is_dir));
        }
        let mut out = format!("{}/\n", display_path(&root, &dir));
        for (p, is_dir) in &entries {
            let rel = p.strip_prefix(&dir).unwrap_or(p);
            let d = rel.components().count().saturating_sub(1);
            let name = rel
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push_str(&"  ".repeat(d + 1));
            out.push_str(&name);
            if *is_dir {
                out.push('/');
            }
            out.push('\n');
        }
        if truncated {
            out.push_str(&format!(
                "[truncated at {LIMIT} entries; list a subdirectory or use glob]\n"
            ));
        }
        (out, entries.len())
    })
    .await;
    match res {
        Ok((out, n)) => ToolOutput::ok(out).with_summary(format!("{n} entries")),
        Err(e) => ToolOutput::err(e.to_string()),
    }
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> String {
        "List a directory as a tree (respects .gitignore). `depth` defaults to 2. Prefer `glob` to find files by pattern.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory (default: current directory)"},
                "depth": {"type": "integer", "description": "Recursion depth (default 2)"}
            }
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        arg_opt_str(args, "path").unwrap_or(".").to_string()
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let path = resolve_path(&ctx.cwd(), arg_opt_str(&args, "path").unwrap_or("."));
        if !path.is_dir() {
            return ToolOutput::err(format!("{} is not a directory", path.display()));
        }
        let depth = arg_u64(&args, "depth").unwrap_or(2).clamp(1, 10) as usize;
        list_dir(ctx, &path, depth).await
    }
}

// ───────────────────────────── glob ─────────────────────────────

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> String {
        "Find files by glob pattern (e.g. `**/*.rs`, `src/**/test_*.py`). Respects .gitignore. \
Results are sorted by modification time, newest first. Use this instead of `find`/`ls -R` in bash."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern relative to `path`"},
                "path": {"type": "string", "description": "Directory to search (default: current directory)"}
            },
            "required": ["pattern"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        let pat = arg_opt_str(args, "pattern").unwrap_or("?");
        match arg_opt_str(args, "path") {
            Some(p) => format!("{pat} in {p}"),
            None => pat.to_string(),
        }
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let pattern = match arg_str(&args, "pattern") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let base = resolve_path(&ctx.cwd(), arg_opt_str(&args, "path").unwrap_or("."));
        let root = ctx.root().to_path_buf();
        let res = tokio::task::spawn_blocking(move || -> Result<(String, usize), String> {
            let pat = if pattern.contains('/') || pattern.starts_with("**") {
                pattern.trim_start_matches("./").to_string()
            } else {
                format!("**/{pattern}")
            };
            let glob = globset::GlobBuilder::new(&pat)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("invalid glob: {e}"))?
                .compile_matcher();
            let mut hits: Vec<(std::time::SystemTime, PathBuf)> = vec![];
            let walker = ignore::WalkBuilder::new(&base)
                .hidden(false)
                .filter_entry(|e| e.file_name() != ".git")
                .build();
            for e in walker.flatten() {
                if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let rel = e.path().strip_prefix(&base).unwrap_or(e.path());
                if glob.is_match(rel) {
                    let mt = e
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    hits.push((mt, e.path().to_path_buf()));
                }
            }
            hits.sort_by(|a, b| b.0.cmp(&a.0));
            let n = hits.len();
            let mut out: Vec<String> = hits
                .iter()
                .take(300)
                .map(|(_, p)| display_path(&root, p))
                .collect();
            if n > 300 {
                out.push(format!(
                    "[{} more files not shown; narrow the pattern]",
                    n - 300
                ));
            }
            if n == 0 {
                return Ok(("No files matched.".into(), 0));
            }
            Ok((out.join("\n"), n))
        })
        .await;
        match res {
            Ok(Ok((out, n))) => ToolOutput::ok(out).with_summary(format!("{n} files")),
            Ok(Err(e)) => ToolOutput::err(e),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

// ───────────────────────────── write ─────────────────────────────

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> String {
        "Create a new file or completely overwrite an existing one. Parent directories are created. \
To change part of an existing file, use `edit` instead — it is cheaper and safer. \
You must `read` an existing file before overwriting it."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "description": "Full file content"}
            },
            "required": ["path", "content"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn summarize(&self, args: &Value) -> String {
        arg_opt_str(args, "path").unwrap_or("?").to_string()
    }
    fn preview(&self, ctx: &ToolCtx, args: &Value) -> Option<String> {
        let p = resolve_path(&ctx.cwd(), arg_opt_str(args, "path")?);
        let new = args.get("content")?.as_str()?;
        let old = std::fs::read_to_string(&p).unwrap_or_default();
        Some(unified_diff(&display_path(ctx.root(), &p), &old, new))
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let p = match arg_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let content = match args.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(v @ (Value::Object(_) | Value::Array(_))) => {
                serde_json::to_string_pretty(v).unwrap_or_default()
            }
            _ => return ToolOutput::err("missing required parameter `content`"),
        };
        let path = resolve_path(&ctx.cwd(), p);
        if path.is_dir() {
            return ToolOutput::err(format!("{} is a directory", path.display()));
        }
        let old = if path.exists() {
            {
                let files = ctx.shared.files.lock();
                if !files.was_read(&path) {
                    return ToolOutput::err(format!(
                        "{} already exists and you have not read it. Read it first (or use `edit` for partial changes).",
                        path.display()
                    ));
                }
                if let Err(e) = files.check_fresh(&path) {
                    return ToolOutput::err(e);
                }
            }
            std::fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };
        ctx.shared.checkpoint(&path);
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return ToolOutput::err(format!("cannot create {}: {e}", dir.display()));
        }
        if let Err(e) = std::fs::write(&path, &content) {
            return ToolOutput::err(format!("cannot write {}: {e}", path.display()));
        }
        ctx.shared.note_modified(&path);
        let rel = display_path(ctx.root(), &path);
        let diff = unified_diff(&rel, &old, &content);
        let (add, del) = diff_stats(&diff);
        let created = old.is_empty() && del == 0;
        let lines = content.lines().count();
        let mut msg = if created {
            format!("Created {rel} ({lines} lines).")
        } else {
            format!("Wrote {rel} ({lines} lines, +{add} -{del}).")
        };
        if let Some(note) = crate::instructions::nested_for(&ctx.shared, &path) {
            msg.push_str(&note);
        }
        ToolOutput::ok(msg)
            .with_summary(if created {
                format!("created, {lines} lines")
            } else {
                format!("+{add} -{del}")
            })
            .with_display(Display::Diff { path: rel, diff })
    }
}

// ───────────────────────────── edit ─────────────────────────────

pub struct EditTool;

struct EditSpec {
    old: String,
    new: String,
    replace_all: bool,
}

/// Apply a sequence of edits in memory. Returns new content and the line
/// span touched (for the excerpt), or an error message.
fn apply_edits(
    path: &Path,
    content: &str,
    edits: &[EditSpec],
) -> Result<(String, usize, usize, Vec<String>), String> {
    let mut cur = content.to_string();
    let mut lo = usize::MAX;
    let mut hi = 0;
    let mut notes = vec![];
    for (i, e) in edits.iter().enumerate() {
        let prefix = if edits.len() > 1 {
            format!("edit #{}: ", i + 1)
        } else {
            String::new()
        };
        if e.old == e.new {
            return Err(format!(
                "{prefix}old_string and new_string are identical; nothing to change"
            ));
        }
        if e.old.is_empty() {
            return Err(format!(
                "{prefix}old_string is empty. To create a file use `write`; to insert, include an anchor line in old_string."
            ));
        }
        match edit_match::find(&cur, &e.old, &e.new, e.replace_all) {
            Ok(found) => {
                if found.strategy != edit_match::Strategy::Exact {
                    notes.push(format!(
                        "{prefix}matched using {} comparison",
                        found.strategy.label()
                    ));
                }
                let first = found.ranges.first().map(|r| r.0).unwrap_or(0);
                let start_line = edit_match::line_of(&cur, first);
                let next = edit_match::apply(&cur, &found);
                let end_line = start_line + found.new_string.lines().count().max(1) - 1;
                lo = lo.min(start_line);
                hi = hi.max(end_line);
                cur = next;
            }
            Err(MatchError::Ambiguous { lines }) => {
                return Err(format!(
                    "{prefix}old_string matches {} places in {} (lines {}). Include more surrounding context to make it unique, or set replace_all=true to change every occurrence.",
                    lines.len(),
                    path.display(),
                    lines
                        .iter()
                        .map(|l| l.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            Err(MatchError::NotFound { hint }) => {
                let mut msg = format!("{prefix}old_string not found in {}.", path.display());
                if let Some(h) = hint {
                    msg.push_str(&format!(" The most similar region is:\n{h}\nCopy the exact text (without the line-number prefix) and retry."));
                } else {
                    msg.push_str(" Read the file again to get its exact current content.");
                }
                return Err(msg);
            }
        }
    }
    Ok((cur, if lo == usize::MAX { 1 } else { lo }, hi.max(1), notes))
}

fn parse_edit(v: &Value) -> Result<EditSpec, String> {
    let old = v
        .get("old_string")
        .and_then(|s| s.as_str())
        .ok_or("each edit needs `old_string`")?;
    let new = v
        .get("new_string")
        .and_then(|s| s.as_str())
        .ok_or("each edit needs `new_string`")?;
    Ok(EditSpec {
        old: old.to_string(),
        new: new.to_string(),
        replace_all: arg_bool(v, "replace_all"),
    })
}

async fn run_edits(ctx: &ToolCtx, path_arg: &str, edits: Vec<EditSpec>) -> ToolOutput {
    let path = resolve_path(&ctx.cwd(), path_arg);
    let content = match std::fs::read(&path) {
        Ok(b) => {
            if looks_binary(&b) {
                return ToolOutput::err(format!("{} is a binary file", path.display()));
            }
            String::from_utf8_lossy(&b).to_string()
        }
        Err(e) => {
            return ToolOutput::err(format!(
                "cannot read {}: {e}. Use `write` to create new files.",
                path.display()
            ));
        }
    };
    if let Err(e) = ctx.shared.files.lock().check_fresh(&path) {
        return ToolOutput::err(e);
    }
    let (new_content, lo, hi, notes) = match apply_edits(&path, &content, &edits) {
        Ok(r) => r,
        Err(e) => return ToolOutput::err(e),
    };
    ctx.shared.checkpoint(&path);
    if let Err(e) = std::fs::write(&path, &new_content) {
        return ToolOutput::err(format!("cannot write {}: {e}", path.display()));
    }
    ctx.shared.note_modified(&path);
    let rel = display_path(ctx.root(), &path);
    let diff = unified_diff(&rel, &content, &new_content);
    let (add, del) = diff_stats(&diff);
    let mut msg = format!("Edited {rel} (+{add} -{del}).");
    for n in &notes {
        msg.push_str(&format!(" Note: {n}."));
    }
    msg.push_str(&format!(
        "\nResult around the change:\n{}",
        excerpt(&new_content, lo, hi)
    ));
    if let Some(note) = crate::instructions::nested_for(&ctx.shared, &path) {
        msg.push_str(&note);
    }
    ToolOutput::ok(msg)
        .with_summary(format!("+{add} -{del}"))
        .with_display(Display::Diff { path: rel, diff })
}

fn preview_edits(ctx: &ToolCtx, path_arg: &str, edits: &[EditSpec]) -> Option<String> {
    let path = resolve_path(&ctx.cwd(), path_arg);
    let content = std::fs::read_to_string(&path).ok()?;
    let (new_content, ..) = apply_edits(&path, &content, edits).ok()?;
    Some(unified_diff(
        &display_path(ctx.root(), &path),
        &content,
        &new_content,
    ))
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> String {
        "Replace text in a file. `old_string` must match the file content (copy it from `read` output \
WITHOUT the line-number prefix) and must be unique unless `replace_all` is true — include enough surrounding \
lines to make it unique. Small whitespace/indentation differences are tolerated. Preserve the file's \
indentation style in `new_string`. For several changes to one file, prefer `multi_edit`."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to replace"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn summarize(&self, args: &Value) -> String {
        arg_opt_str(args, "path").unwrap_or("?").to_string()
    }
    fn preview(&self, ctx: &ToolCtx, args: &Value) -> Option<String> {
        let e = parse_edit(args).ok()?;
        preview_edits(ctx, arg_opt_str(args, "path")?, &[e])
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let p = match arg_str(&args, "path") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let e = match parse_edit(&args) {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(e),
        };
        run_edits(ctx, &p, vec![e]).await
    }
}

pub struct MultiEditTool;

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multi_edit"
    }
    fn description(&self) -> String {
        "Apply several edits to one file atomically (all succeed or none are applied). Edits run in order, \
each on the result of the previous one. Same matching rules as `edit`."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn summarize(&self, args: &Value) -> String {
        let n = args
            .get("edits")
            .and_then(|e| e.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        format!("{} ({n} edits)", arg_opt_str(args, "path").unwrap_or("?"))
    }
    fn preview(&self, ctx: &ToolCtx, args: &Value) -> Option<String> {
        let edits: Vec<EditSpec> = args
            .get("edits")?
            .as_array()?
            .iter()
            .filter_map(|e| parse_edit(e).ok())
            .collect();
        preview_edits(ctx, arg_opt_str(args, "path")?, &edits)
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let p = match arg_str(&args, "path") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let raw = match args.get("edits") {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(s)) => match serde_json::from_str::<Vec<Value>>(s) {
                Ok(a) => a,
                Err(_) => return ToolOutput::err("`edits` must be an array"),
            },
            _ => return ToolOutput::err("missing required parameter `edits`"),
        };
        let mut edits = vec![];
        for e in &raw {
            match parse_edit(e) {
                Ok(e) => edits.push(e),
                Err(e) => return ToolOutput::err(e),
            }
        }
        if edits.is_empty() {
            return ToolOutput::err("`edits` is empty");
        }
        run_edits(ctx, &p, edits).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edits_sequential_and_atomic() {
        let p = Path::new("x.rs");
        let content = "a\nb\nc\n";
        let edits = vec![
            EditSpec {
                old: "a".into(),
                new: "A".into(),
                replace_all: false,
            },
            EditSpec {
                old: "A\nb".into(),
                new: "AB".into(),
                replace_all: false,
            },
        ];
        let (out, lo, _, _) = apply_edits(p, content, &edits).ok().unwrap();
        assert_eq!(out, "AB\nc\n");
        assert_eq!(lo, 1);
        let bad = vec![
            EditSpec {
                old: "a".into(),
                new: "A".into(),
                replace_all: false,
            },
            EditSpec {
                old: "zzz".into(),
                new: "y".into(),
                replace_all: false,
            },
        ];
        let err = apply_edits(p, content, &bad).err().unwrap();
        assert!(err.starts_with("edit #2"));
    }

    #[test]
    fn diff_has_stats() {
        let d = unified_diff("f", "a\nb\n", "a\nc\n");
        assert_eq!(diff_stats(&d), (1, 1));
    }
}

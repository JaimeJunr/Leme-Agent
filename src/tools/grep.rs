//! In-process ripgrep: the same engine as `rg`, no subprocess, respects .gitignore.

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_bool, arg_opt_str, arg_str, arg_u64};
use crate::util::{display_path, resolve_path};
use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct GrepTool;

#[derive(Default)]
struct FileHits {
    lines: Vec<(u64, bool, String)>, // (line, is_match, text)
    count: usize,
}

struct Collector<'a> {
    hits: &'a mut FileHits,
    max_line: usize,
    counting_only: bool,
}

impl Sink for Collector<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.hits.count += 1;
        if self.counting_only {
            return Ok(true);
        }
        let start = m.line_number().unwrap_or(0);
        for (i, line) in String::from_utf8_lossy(m.bytes()).lines().enumerate() {
            self.hits.lines.push((
                start + i as u64,
                true,
                crate::util::ellipsize(line, self.max_line),
            ));
        }
        Ok(self.hits.lines.len() < 2000)
    }

    fn context(&mut self, _s: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if self.counting_only {
            return Ok(true);
        }
        let ln = c.line_number().unwrap_or(0);
        let text = String::from_utf8_lossy(c.bytes());
        let text = text.trim_end_matches(['\n', '\r']);
        self.hits
            .lines
            .push((ln, false, crate::util::ellipsize(text, self.max_line)));
        Ok(true)
    }

    fn context_break(&mut self, _s: &Searcher) -> Result<bool, Self::Error> {
        if !self.counting_only {
            self.hits.lines.push((0, false, "--".into()));
        }
        Ok(true)
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> String {
        "Search file contents with a regular expression (ripgrep engine, Rust regex syntax; respects .gitignore). \
Use this instead of grep/rg in bash. output_mode: `content` (matching lines with line numbers, default), \
`files` (only file paths, sorted by recency) or `count`. Filter files with `glob` (e.g. `*.ts`, `src/**/*.rs`) \
or `type` (rust, py, js, ts, go, java, c, cpp, …). Escape literal braces/parens, e.g. `fn main\\(`."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex to search for"},
                "path": {"type": "string", "description": "File or directory (default: current directory)"},
                "glob": {"type": "string", "description": "Only search files matching this glob"},
                "type": {"type": "string", "description": "Only search files of this type (ripgrep type names)"},
                "output_mode": {"type": "string", "enum": ["content", "files", "count"]},
                "case_insensitive": {"type": "boolean"},
                "context": {"type": "integer", "description": "Lines of context around matches (content mode)"},
                "multiline": {"type": "boolean", "description": "Allow patterns to span lines (`.` matches newline)"},
                "limit": {"type": "integer", "description": "Max output lines/files (default 250)"}
            },
            "required": ["pattern"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summarize(&self, args: &Value) -> String {
        let mut s = format!("\"{}\"", arg_opt_str(args, "pattern").unwrap_or("?"));
        if let Some(g) = arg_opt_str(args, "glob").or_else(|| arg_opt_str(args, "type")) {
            s.push_str(&format!(" ({g})"));
        }
        if let Some(p) = arg_opt_str(args, "path") {
            s.push_str(&format!(" in {p}"));
        }
        s
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> ToolOutput {
        let pattern = match arg_str(&args, "pattern") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let base = resolve_path(&ctx.cwd(), arg_opt_str(&args, "path").unwrap_or("."));
        if !base.exists() {
            return ToolOutput::err(format!("path {} does not exist", base.display()));
        }
        let mode = arg_opt_str(&args, "output_mode")
            .unwrap_or("content")
            .to_string();
        let mode = match mode.as_str() {
            "files_with_matches" | "files" | "list" => "files".to_string(),
            "count" => "count".to_string(),
            _ => "content".to_string(),
        };
        let ci = arg_bool(&args, "case_insensitive") || arg_bool(&args, "-i");
        let multiline = arg_bool(&args, "multiline");
        let context = arg_u64(&args, "context")
            .or_else(|| arg_u64(&args, "-C"))
            .unwrap_or(0)
            .min(20) as usize;
        let limit = arg_u64(&args, "limit")
            .or_else(|| arg_u64(&args, "head_limit"))
            .unwrap_or(250)
            .clamp(1, 5000) as usize;
        let glob = arg_opt_str(&args, "glob").map(String::from);
        let ftype = arg_opt_str(&args, "type").map(String::from);
        let root = ctx.root().to_path_buf();
        let cancel = ctx.cancel.clone();

        let res = tokio::task::spawn_blocking(move || -> Result<(String, usize, usize), String> {
            let matcher = RegexMatcherBuilder::new()
                .case_insensitive(ci)
                .multi_line(multiline)
                .dot_matches_new_line(multiline)
                .build(&pattern)
                .map_err(|e| format!("invalid regex: {e}"))?;
            let mut wb = ignore::WalkBuilder::new(&base);
            wb.hidden(false).filter_entry(|e| e.file_name() != ".git");
            if let Some(t) = &ftype {
                let mut tb = ignore::types::TypesBuilder::new();
                tb.add_defaults();
                tb.select(t);
                let types = tb.build().map_err(|e| format!("bad type: {e}"))?;
                wb.types(types);
            }
            if let Some(g) = &glob {
                let mut ob = ignore::overrides::OverrideBuilder::new(&base);
                for part in split_glob_list(g) {
                    let pat = if part.contains('/') { part } else { format!("**/{part}") };
                    ob.add(&pat).map_err(|e| format!("bad glob: {e}"))?;
                }
                wb.overrides(ob.build().map_err(|e| format!("bad glob: {e}"))?);
            }
            let results: Arc<Mutex<Vec<(PathBuf, FileHits)>>> = Arc::new(Mutex::new(vec![]));
            let counting_only = mode != "content";
            wb.build_parallel().run(|| {
                let matcher = matcher.clone();
                let results = results.clone();
                let cancel = cancel.clone();
                let mut searcher = SearcherBuilder::new()
                    .binary_detection(BinaryDetection::quit(b'\x00'))
                    .line_number(true)
                    .multi_line(multiline)
                    .before_context(context)
                    .after_context(context)
                    .build();
                Box::new(move |entry| {
                    if cancel.is_cancelled() {
                        return ignore::WalkState::Quit;
                    }
                    let Ok(entry) = entry else { return ignore::WalkState::Continue };
                    if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        return ignore::WalkState::Continue;
                    }
                    let mut hits = FileHits::default();
                    let r = searcher.search_path(
                        &matcher,
                        entry.path(),
                        Collector { hits: &mut hits, max_line: 400, counting_only },
                    );
                    if r.is_ok() && hits.count > 0 {
                        results.lock().push((entry.path().to_path_buf(), hits));
                    }
                    ignore::WalkState::Continue
                })
            });
            let mut results = std::mem::take(&mut *results.lock());
            let total_files = results.len();
            let total_matches: usize = results.iter().map(|(_, h)| h.count).sum();
            if total_files == 0 {
                return Ok(("No matches found.".into(), 0, 0));
            }
            let mut out = String::new();
            match mode.as_str() {
                "files" => {
                    results.sort_by_key(|(p, _)| std::cmp::Reverse(mtime(p)));
                    for (p, _) in results.iter().take(limit) {
                        out.push_str(&display_path(&root, p));
                        out.push('\n');
                    }
                    if total_files > limit {
                        out.push_str(&format!("[{} more files]\n", total_files - limit));
                    }
                }
                "count" => {
                    results.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(&b.0)));
                    for (p, h) in results.iter().take(limit) {
                        out.push_str(&format!("{}: {}\n", display_path(&root, p), h.count));
                    }
                    out.push_str(&format!("[{total_matches} matches in {total_files} files]\n"));
                }
                _ => {
                    results.sort_by(|a, b| a.0.cmp(&b.0));
                    let mut lines = 0;
                    'outer: for (p, h) in &results {
                        out.push_str(&display_path(&root, p));
                        out.push('\n');
                        for (ln, is_match, text) in &h.lines {
                            if lines >= limit {
                                out.push_str("…\n");
                                break 'outer;
                            }
                            if text == "--" && *ln == 0 {
                                out.push_str("  --\n");
                            } else {
                                let sep = if *is_match { ':' } else { '-' };
                                out.push_str(&format!("  {ln}{sep} {text}\n"));
                            }
                            lines += 1;
                        }
                    }
                    if lines >= limit {
                        out.push_str(&format!(
                            "[output limited to {limit} lines; {total_matches} matches in {total_files} files. Narrow the search or use output_mode=files]\n"
                        ));
                    }
                }
            }
            Ok((out, total_matches, total_files))
        })
        .await;
        match res {
            Ok(Ok((out, m, f))) => ToolOutput::ok(out).with_summary(if m == 0 {
                "no matches".to_string()
            } else {
                format!("{m} matches in {f} files")
            }),
            Ok(Err(e)) => ToolOutput::err(e),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Split `*.{ts,tsx}` style lists and comma/space separated globs.
fn split_glob_list(g: &str) -> Vec<String> {
    if g.contains('{') {
        return vec![g.to_string()];
    }
    g.split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

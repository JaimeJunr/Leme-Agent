//! Small helpers shared across modules.

use std::path::{Component, Path, PathBuf};

/// Rough token estimate. Real counts come back from the API after every call;
/// this only fills the gap for content added since the last response.
pub fn estimate_tokens(s: &str) -> usize {
    // ~3.6 chars/token is a good middle ground for code + English.
    (s.len() as f64 / 3.6).ceil() as usize
}

/// Truncate to at most `max` bytes on a char boundary.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncate to `max` chars, appending an ellipsis when cut.
pub fn ellipsize(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

/// First line of `s`, ellipsized.
pub fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    let more = s.lines().nth(1).is_some();
    let mut out = ellipsize(line, max);
    if more && !out.ends_with('…') {
        out.push_str(" …");
    }
    out
}

/// Keep the head and tail of a large output so the model sees both the
/// beginning (usually the command echo / first error) and the end (the
/// summary / final error). Returns (text, was_truncated).
pub fn head_tail(s: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let lines: Vec<&str> = s.lines().collect();
    let too_many_lines = lines.len() > max_lines;
    let too_many_bytes = s.len() > max_bytes;
    if !too_many_lines && !too_many_bytes {
        return (s.to_string(), false);
    }
    let head_n = max_lines / 4;
    let tail_n = max_lines - head_n;
    let (head, tail): (Vec<&str>, Vec<&str>) = if too_many_lines {
        (
            lines[..head_n].to_vec(),
            lines[lines.len() - tail_n..].to_vec(),
        )
    } else {
        (lines.clone(), vec![])
    };
    let omitted = lines.len().saturating_sub(head.len() + tail.len());
    let mut head_s = head.join("\n");
    let mut tail_s = tail.join("\n");
    // Enforce the byte budget, favouring the tail.
    let budget_head = max_bytes / 4;
    let budget_tail = max_bytes - budget_head;
    if head_s.len() > budget_head {
        head_s = truncate_bytes(&head_s, budget_head).to_string();
    }
    if tail_s.len() > budget_tail {
        let start = tail_s.len() - budget_tail;
        let mut st = start;
        while st < tail_s.len() && !tail_s.is_char_boundary(st) {
            st += 1;
        }
        tail_s = tail_s[st..].to_string();
    }
    let mut out = head_s;
    if tail.is_empty() {
        out.push_str(&format!("\n… [output truncated, {} bytes total]", s.len()));
    } else {
        out.push_str(&format!("\n… [{omitted} lines omitted] …\n"));
        out.push_str(&tail_s);
    }
    (out, true)
}

/// Normalise a path lexically (no filesystem access): resolves `.` and `..`.
pub fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Resolve a user/model supplied path against `cwd`, expanding `~`.
pub fn resolve_path(cwd: &Path, p: &str) -> PathBuf {
    let p = p.trim();
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else if p == "~" {
        dirs::home_dir().unwrap_or_default()
    } else {
        PathBuf::from(p)
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    normalize_path(&joined)
}

/// Display a path relative to `root` when inside it.
pub fn display_path(root: &Path, p: &Path) -> String {
    match p.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        Ok(_) => ".".to_string(),
        Err(_) => {
            if let Some(home) = dirs::home_dir()
                && let Ok(rel) = p.strip_prefix(&home)
            {
                return format!("~/{}", rel.display());
            }
            p.display().to_string()
        }
    }
}

pub fn is_within(root: &Path, p: &Path) -> bool {
    normalize_path(p).starts_with(normalize_path(root))
}

/// Heuristic binary detection: NUL byte in the first 8KB.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Format a dollar amount compactly.
pub fn fmt_cost(c: f64) -> String {
    if c == 0.0 {
        "$0".into()
    } else if c < 0.01 {
        format!("${c:.4}")
    } else if c < 10.0 {
        format!("${c:.3}")
    } else {
        format!("${c:.2}")
    }
}

pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

pub fn fmt_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if d.as_millis() >= 10_000 {
        format!("{s}s")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Try hard to parse tool-call arguments produced by weaker models:
/// strips code fences, trailing commas and closes unbalanced brackets.
pub fn parse_json_lenient(raw: &str) -> Result<serde_json::Value, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(serde_json::json!({}));
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        // Some models double-encode the arguments as a JSON string.
        if let serde_json::Value::String(inner) = &v
            && let Ok(iv) = serde_json::from_str::<serde_json::Value>(inner)
            && iv.is_object()
        {
            return Ok(iv);
        }
        return Ok(v);
    }
    let mut s = t.to_string();
    if s.starts_with("```") {
        s = s
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .to_string();
        if let Some(i) = s.rfind("```") {
            s.truncate(i);
        }
    }
    let s2 = strip_trailing_commas(&s);
    if let Ok(v) = serde_json::from_str(&s2) {
        return Ok(v);
    }
    let s3 = close_brackets(&s2);
    if let Ok(v) = serde_json::from_str(&s3) {
        return Ok(v);
    }
    Err(match serde_json::from_str::<serde_json::Value>(t) {
        Err(e) => e.to_string(),
        Ok(_) => "invalid JSON".into(),
    })
}

fn strip_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut in_str = false;
    let mut esc = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            continue;
        }
        if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn close_brackets(s: &str) -> String {
    let mut stack = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    for c in s.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = s.to_string();
    if in_str {
        out.push('"');
    } else {
        let trimmed = out.trim_end().trim_end_matches(',').to_string();
        out = trimmed;
    }
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    out
}

/// Glob-ish match used by permission rules: `*` matches anything.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti] || p[pi] == '?') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_json() {
        assert_eq!(parse_json_lenient(r#"{"a":1,}"#).unwrap()["a"], 1);
        assert_eq!(
            parse_json_lenient(r#"{"a":[1,2,"#).unwrap_or_default()["a"][1],
            2
        );
        assert_eq!(
            parse_json_lenient("```json\n{\"a\":2}\n```").unwrap()["a"],
            2
        );
        assert_eq!(parse_json_lenient(r#""{\"a\":3}""#).unwrap()["a"], 3);
        assert_eq!(parse_json_lenient(r#"{"s":"x,}"}"#).unwrap()["s"], "x,}");
        assert!(parse_json_lenient("").unwrap().is_object());
    }

    #[test]
    fn wildcard() {
        assert!(wildcard_match("cargo test*", "cargo test --all"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("git push*", "git status"));
        assert!(wildcard_match("src/*.rs", "src/main.rs"));
    }

    #[test]
    fn head_tail_truncates() {
        let s: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let (out, t) = head_tail(&s, 100, 100_000);
        assert!(t);
        assert!(out.contains("line 0"));
        assert!(out.contains("line 999"));
        assert!(out.contains("omitted"));
    }

    #[test]
    fn paths() {
        let cwd = Path::new("/a/b");
        assert_eq!(resolve_path(cwd, "../c"), PathBuf::from("/a/c"));
        assert_eq!(resolve_path(cwd, "./d/./e"), PathBuf::from("/a/b/d/e"));
        assert!(is_within(Path::new("/a"), Path::new("/a/b/../c")));
        assert!(!is_within(Path::new("/a/b"), Path::new("/a/b/../c")));
    }
}

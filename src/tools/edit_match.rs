//! Locating `old_string` inside a file, tolerating the small mistakes models
//! make (whitespace drift, wrong indentation, over-escaped strings) — without
//! ever silently picking one of several ambiguous matches.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Strategy {
    Exact,
    TrimmedLines,
    Whitespace,
    Indentation,
    Unescaped,
}

impl Strategy {
    pub fn label(self) -> &'static str {
        match self {
            Strategy::Exact => "exact",
            Strategy::TrimmedLines => "trimmed-lines",
            Strategy::Whitespace => "whitespace-normalized",
            Strategy::Indentation => "indentation-flexible",
            Strategy::Unescaped => "unescaped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Found {
    pub strategy: Strategy,
    /// Byte ranges in the original content.
    pub ranges: Vec<(usize, usize)>,
    /// Replacement adjusted for the strategy (e.g. re-indented).
    pub new_string: String,
}

pub enum MatchError {
    NotFound { hint: Option<String> },
    Ambiguous { lines: Vec<usize> },
}

/// Byte offset → 1-based line number.
pub fn line_of(content: &str, offset: usize) -> usize {
    content[..offset.min(content.len())].matches('\n').count() + 1
}

/// Byte offsets of the start of each line (plus content.len() sentinel).
fn line_starts(content: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    if *v.last().unwrap() != content.len() {
        v.push(content.len());
    }
    v
}

fn lines_of(s: &str) -> Vec<&str> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn indent_of(l: &str) -> &str {
    &l[..l.len() - l.trim_start().len()]
}

fn min_indent(lines: &[&str]) -> usize {
    lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_of(l).len())
        .min()
        .unwrap_or(0)
}

/// Line-window matching with a per-line comparison function.
fn window_match(
    content: &str,
    old: &str,
    eq: impl Fn(&[&str], &[&str]) -> bool,
) -> Vec<(usize, usize)> {
    if content.is_empty() {
        return vec![];
    }
    let starts = line_starts(content);
    let clines = lines_of(content);
    let olines = lines_of(old);
    if olines.is_empty() || olines.len() > clines.len() || starts.len() != clines.len() + 1 {
        return vec![];
    }
    let bytes = content.as_bytes();
    let mut out = vec![];
    for i in 0..=(clines.len() - olines.len()) {
        let window = &clines[i..i + olines.len()];
        if eq(window, &olines) {
            let start = starts[i];
            let last = i + olines.len() - 1;
            let next = starts[last + 1];
            // End of the last line, excluding its line terminator unless
            // `old` itself ended with a newline.
            let end = if old.ends_with('\n') {
                next
            } else {
                let mut e = next;
                if e > start && bytes[e - 1] == b'\n' {
                    e -= 1;
                    if e > start && bytes[e - 1] == b'\r' {
                        e -= 1;
                    }
                }
                e
            };
            out.push((start, end));
        }
    }
    out
}

fn exact_all(content: &str, old: &str) -> Vec<(usize, usize)> {
    if old.is_empty() {
        return vec![];
    }
    content
        .match_indices(old)
        .map(|(i, m)| (i, i + m.len()))
        .collect()
}

/// Translate `new`'s indentation from the model's style (as seen in `old`)
/// to the file's style (as seen in the matched `window`).
fn remap_indent(window: &[&str], old: &[&str], new: &str) -> String {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<usize, String> = BTreeMap::new();
    for (w, o) in window.iter().zip(old) {
        if !o.trim().is_empty() {
            map.entry(indent_of(o).len())
                .or_insert_with(|| indent_of(w).to_string());
        }
    }
    let identity = window
        .iter()
        .zip(old)
        .all(|(w, o)| o.trim().is_empty() || indent_of(w) == indent_of(o));
    if identity || map.is_empty() {
        return new.to_string();
    }
    let tabs = map.values().any(|v| v.contains('\t'));
    // Uniform offset (e.g. the whole block is nested one level deeper)?
    let offsets: Vec<i64> = map
        .iter()
        .map(|(k, v)| v.len() as i64 - *k as i64)
        .collect();
    let uniform = offsets.windows(2).all(|w| w[0] == w[1]);
    // Otherwise a scale factor (2-space vs 4-space indentation).
    let scale = map
        .iter()
        .filter(|(k, _)| **k > 0)
        .map(|(k, v)| v.len() as f64 / *k as f64)
        .next()
        .unwrap_or(1.0);
    let convert = |ind: usize| -> String {
        if let Some(v) = map.get(&ind) {
            return v.clone();
        }
        let n = if uniform {
            (ind as i64 + offsets[0]).max(0) as usize
        } else {
            (ind as f64 * scale).round() as usize
        };
        if tabs { "\t".repeat(n) } else { " ".repeat(n) }
    };
    new.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                return line.trim_end_matches([' ', '\t']).to_string();
            }
            let ind = indent_of(line).len();
            format!("{}{}", convert(ind), &line[ind..])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn unescape(s: &str) -> String {
    s.replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\'", "'")
        .replace("\\\\", "\\")
}

pub fn find(content: &str, old: &str, new: &str, replace_all: bool) -> Result<Found, MatchError> {
    let pick = |strategy: Strategy,
                ranges: Vec<(usize, usize)>,
                new_string: String|
     -> Option<Result<Found, MatchError>> {
        match ranges.len() {
            0 => None,
            1 => Some(Ok(Found {
                strategy,
                ranges,
                new_string,
            })),
            _ if replace_all && strategy == Strategy::Exact => Some(Ok(Found {
                strategy,
                ranges,
                new_string,
            })),
            _ => Some(Err(MatchError::Ambiguous {
                lines: ranges.iter().map(|(s, _)| line_of(content, *s)).collect(),
            })),
        }
    };

    if let Some(r) = pick(Strategy::Exact, exact_all(content, old), new.to_string()) {
        return r;
    }
    // CRLF files with LF old_string.
    if content.contains("\r\n") && !old.contains("\r\n") {
        let old_crlf = old.replace('\n', "\r\n");
        let new_crlf = new.replace('\n', "\r\n");
        if let Some(r) = pick(Strategy::Exact, exact_all(content, &old_crlf), new_crlf) {
            return r;
        }
    }
    if replace_all {
        return Err(MatchError::NotFound {
            hint: closest_hint(content, old),
        });
    }
    let trimmed = window_match(content, old, |w, o| {
        w.iter().zip(o).all(|(a, b)| a.trim() == b.trim())
    });
    // Keep the file's indentation when only leading whitespace differed.
    let adjusted = |ranges: &[(usize, usize)]| -> String {
        match ranges.first() {
            Some((st, e)) => remap_indent(&lines_of(&content[*st..*e]), &lines_of(old), new),
            None => new.to_string(),
        }
    };
    if !trimmed.is_empty() {
        let n = adjusted(&trimmed);
        if let Some(r) = pick(Strategy::TrimmedLines, trimmed, n) {
            return r;
        }
    }
    let ws = window_match(content, old, |w, o| {
        w.iter()
            .zip(o)
            .all(|(a, b)| collapse_ws(a) == collapse_ws(b))
    });
    if !ws.is_empty() {
        let n = adjusted(&ws);
        if let Some(r) = pick(Strategy::Whitespace, ws, n) {
            return r;
        }
    }
    let indent = window_match(content, old, |w, o| {
        let wi = min_indent(w);
        let oi = min_indent(o);
        w.iter().zip(o).all(|(a, b)| {
            let a2 = if a.trim().is_empty() {
                ""
            } else {
                &a[wi.min(a.len())..]
            };
            let b2 = if b.trim().is_empty() {
                ""
            } else {
                &b[oi.min(b.len())..]
            };
            a2.trim_end() == b2.trim_end()
        })
    });
    if !indent.is_empty() {
        let n = adjusted(&indent);
        if let Some(r) = pick(Strategy::Indentation, indent, n) {
            return r;
        }
    }
    if old.contains('\\') {
        let u = unescape(old);
        if u != old
            && let Some(r) = pick(Strategy::Unescaped, exact_all(content, &u), unescape(new))
        {
            return r;
        }
    }
    Err(MatchError::NotFound {
        hint: closest_hint(content, old),
    })
}

/// Find the region most similar to `old` to help the model correct itself.
pub fn closest_hint(content: &str, old: &str) -> Option<String> {
    let clines = lines_of(content);
    let olines: Vec<&str> = lines_of(old);
    if olines.is_empty() || clines.is_empty() || clines.len() > 20_000 || olines.len() > 200 {
        return None;
    }
    let n = olines.len().min(clines.len());
    let otrim: Vec<String> = olines.iter().map(|l| collapse_ws(l)).collect();
    let ctrim: Vec<String> = clines.iter().map(|l| collapse_ws(l)).collect();
    let mut best = (0usize, 0usize);
    for i in 0..=(clines.len() - n) {
        let mut score = 0;
        for j in 0..n {
            let (a, b) = (&ctrim[i + j], &otrim[j]);
            if a == b && !a.is_empty() {
                score += 3;
            } else if !a.is_empty()
                && !b.is_empty()
                && (a.contains(b.as_str()) || b.contains(a.as_str()))
            {
                score += 1;
            }
        }
        if score > best.0 {
            best = (score, i);
        }
    }
    if best.0 == 0 {
        return None;
    }
    let start = best.1.saturating_sub(2);
    let end = (best.1 + n + 2).min(clines.len());
    let mut s = String::new();
    for (k, l) in clines[start..end].iter().enumerate() {
        s.push_str(&format!("{:>6}\t{}\n", start + k + 1, l));
    }
    Some(s)
}

pub fn apply(content: &str, found: &Found) -> String {
    let mut out = String::with_capacity(content.len() + found.new_string.len());
    let mut last = 0;
    for (s, e) in &found.ranges {
        out.push_str(&content[last..*s]);
        out.push_str(&found.new_string);
        last = *e;
    }
    out.push_str(&content[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(content: &str, old: &str, new: &str) -> (String, Strategy) {
        match find(content, old, new, false) {
            Ok(f) => (apply(content, &f), f.strategy),
            Err(MatchError::NotFound { .. }) => panic!("not found"),
            Err(MatchError::Ambiguous { lines }) => panic!("ambiguous {lines:?}"),
        }
    }

    #[test]
    fn exact() {
        let (r, s) = run("a\nb\nc\n", "b", "B");
        assert_eq!(r, "a\nB\nc\n");
        assert_eq!(s, Strategy::Exact);
    }

    #[test]
    fn trailing_whitespace_drift() {
        let (r, s) = run(
            "fn x() {  \n    y();\n}\n",
            "fn x() {\n    y();\n}",
            "fn x() {\n    z();\n}",
        );
        assert_eq!(r, "fn x() {\n    z();\n}\n");
        assert_eq!(s, Strategy::TrimmedLines);
    }

    #[test]
    fn wrong_indentation_is_reindented() {
        let content = "impl A {\n        fn f() {\n            1\n        }\n}\n";
        let old = "fn f() {\n    1\n}";
        let new = "fn f() {\n    2\n}";
        let (r, _) = run(content, old, new);
        assert_eq!(
            r,
            "impl A {\n        fn f() {\n            2\n        }\n}\n"
        );
    }

    #[test]
    fn crlf() {
        let (r, _) = run("a\r\nb\r\nc\r\n", "a\nb", "x\ny");
        assert_eq!(r, "x\r\ny\r\nc\r\n");
    }

    #[test]
    fn ambiguous_reports_lines() {
        match find("x\ny\nx\n", "x", "z", false) {
            Err(MatchError::Ambiguous { lines }) => assert_eq!(lines, vec![1, 3]),
            _ => panic!(),
        }
        let f = find("x\ny\nx\n", "x", "z", true).ok().unwrap();
        assert_eq!(apply("x\ny\nx\n", &f), "z\ny\nz\n");
    }

    #[test]
    fn hint_on_miss() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        match find(content, "fn main() {\n    println!(\"bye\");\n}", "", false) {
            Err(MatchError::NotFound { hint: Some(h) }) => assert!(h.contains("fn main")),
            _ => panic!(),
        }
    }

    #[test]
    fn unescaped() {
        let (r, s) = run("say(\"hi\")\n", "say(\\\"hi\\\")", "say(\\\"yo\\\")");
        assert_eq!(r, "say(\"yo\")\n");
        assert_eq!(s, Strategy::Unescaped);
    }
}

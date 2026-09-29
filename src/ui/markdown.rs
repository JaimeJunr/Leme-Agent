//! Streaming Markdown → styled terminal lines. Lines are rendered as soon as
//! they are complete; tables are buffered until they end so columns align.

use super::style::{Line, Style, theme, wrap};
use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct Markdown {
    width: usize,
    in_code: bool,
    fence: String,
    lang: String,
    table: Vec<String>,
    last_blank: bool,
}

impl Markdown {
    pub fn new(width: usize) -> Markdown {
        Markdown {
            width,
            ..Default::default()
        }
    }

    pub fn set_width(&mut self, w: usize) {
        self.width = w;
    }

    pub fn reset(&mut self) {
        let w = self.width;
        *self = Markdown::new(w);
    }

    /// Render one complete source line.
    pub fn line(&mut self, raw: &str) -> Vec<Line> {
        let raw = raw.trim_end_matches('\r');
        let mut out = vec![];
        let trimmed = raw.trim_start();
        // Tables are buffered.
        if !self.in_code && trimmed.starts_with('|') {
            self.table.push(trimmed.to_string());
            return out;
        }
        if !self.table.is_empty() {
            out.extend(self.flush_table());
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence: String = trimmed
                .chars()
                .take_while(|c| *c == '`' || *c == '~')
                .collect();
            if !self.in_code {
                self.in_code = true;
                self.fence = fence;
                self.lang = trimmed[self.fence.len()..].trim().to_string();
                let label = if self.lang.is_empty() {
                    "code".to_string()
                } else {
                    self.lang.clone()
                };
                out.push(Line::styled(
                    format!("  ╭─ {label}"),
                    Style::fg(theme::MUTED),
                ));
                return out;
            } else if fence.len() >= self.fence.len() && trimmed[fence.len()..].trim().is_empty() {
                self.in_code = false;
                out.push(Line::styled("  ╰─", Style::fg(theme::MUTED)));
                return out;
            }
        }
        if self.in_code {
            let mut l = Line::styled("  │ ", Style::fg(theme::MUTED));
            for (t, s) in highlight(raw, &self.lang) {
                l.push(t, s);
            }
            // Code is not word-wrapped nicely; hard wrap keeps alignment readable.
            out.extend(wrap(&l, self.width, 4));
            self.last_blank = false;
            return out;
        }
        if trimmed.is_empty() {
            if !self.last_blank {
                out.push(Line::new());
            }
            self.last_blank = true;
            return out;
        }
        self.last_blank = false;
        // Headings
        if let Some(level) = heading_level(trimmed) {
            let text = trimmed[level..].trim().trim_end_matches('#').trim();
            let mut style = Style::fg(theme::HEADING).bold();
            if level == 1 {
                style = style.underline();
            }
            let mut l = Line::new();
            inline(text, style, &mut l);
            out.extend(wrap(&l, self.width, 0));
            return out;
        }
        // Horizontal rule
        if is_rule(trimmed) {
            out.push(Line::styled(
                "─".repeat(self.width.min(60)),
                Style::fg(theme::MUTED),
            ));
            return out;
        }
        // Blockquote
        if let Some(q) = trimmed.strip_prefix('>') {
            let mut l = Line::styled("▎ ", Style::fg(theme::MUTED));
            inline(q.trim_start(), Style::default().italic(), &mut l);
            out.extend(wrap(&l, self.width, 2));
            return out;
        }
        // Lists
        let indent = raw.len() - trimmed.len();
        if let Some((marker, rest)) = list_item(trimmed) {
            let pad = " ".repeat(indent.min(12));
            let mut l = Line::raw(pad.clone());
            let (bullet, rest) = if let Some(r) = rest.strip_prefix("[ ] ") {
                ("☐ ".to_string(), r)
            } else if let Some(r) = rest
                .strip_prefix("[x] ")
                .or_else(|| rest.strip_prefix("[X] "))
            {
                ("☑ ".to_string(), r)
            } else if marker.ends_with('.') || marker.ends_with(')') {
                (format!("{marker} "), rest)
            } else {
                ("• ".to_string(), rest)
            };
            l.push(bullet.clone(), Style::fg(theme::ACCENT));
            inline(rest, Style::default(), &mut l);
            out.extend(wrap(&l, self.width, pad.len() + bullet.width()));
            return out;
        }
        let mut l = Line::new();
        inline(raw, Style::default(), &mut l);
        out.extend(wrap(&l, self.width, indent.min(8)));
        out
    }

    /// Flush buffered state at the end of a message.
    pub fn finish(&mut self) -> Vec<Line> {
        let mut out = vec![];
        if !self.table.is_empty() {
            out.extend(self.flush_table());
        }
        if self.in_code {
            out.push(Line::styled("  ╰─", Style::fg(theme::MUTED)));
        }
        self.in_code = false;
        self.fence.clear();
        self.last_blank = false;
        out
    }

    fn flush_table(&mut self) -> Vec<Line> {
        let rows: Vec<Vec<String>> = std::mem::take(&mut self.table)
            .iter()
            .map(|r| {
                let r = r.trim().trim_start_matches('|').trim_end_matches('|');
                r.split('|').map(|c| c.trim().to_string()).collect()
            })
            .collect();
        let is_sep = |r: &Vec<String>| {
            r.iter()
                .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
        };
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut widths = vec![0usize; ncols];
        for r in rows.iter().filter(|r| !is_sep(r)) {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(strip_inline(c).width());
            }
        }
        // Shrink columns to fit the terminal.
        let total: usize = widths.iter().sum::<usize>() + ncols * 3 + 1;
        if total > self.width && ncols > 0 {
            let budget = self.width.saturating_sub(ncols * 3 + 1).max(ncols * 4);
            let per = budget / ncols;
            for w in widths.iter_mut() {
                if *w > per {
                    *w = per.max(4);
                }
            }
        }
        let mut out = vec![];
        for (ri, r) in rows.iter().enumerate() {
            if is_sep(r) {
                let mut l = Line::new();
                let parts: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                l.push(
                    format!("├─{}─┤", parts.join("─┼─")),
                    Style::fg(theme::MUTED),
                );
                out.push(l);
                continue;
            }
            let mut l = Line::styled("│ ", Style::fg(theme::MUTED));
            for (i, w) in widths.iter().enumerate() {
                let cell = r.get(i).map(|s| s.as_str()).unwrap_or("");
                let plain = strip_inline(cell);
                let shown = if plain.width() > *w {
                    crate::util::ellipsize(&plain, w.saturating_sub(1))
                } else {
                    plain
                };
                let style = if ri == 0 {
                    Style::default().bold()
                } else {
                    Style::default()
                };
                let mut cl = Line::new();
                if ri == 0 || shown.width() < strip_inline(cell).width() {
                    cl.push(shown.clone(), style);
                } else {
                    inline(cell, style, &mut cl);
                }
                let cw = cl.width();
                for s in cl.spans {
                    l.push(s.text, s.style);
                }
                l.push(" ".repeat(w.saturating_sub(cw)), Style::default());
                l.push(
                    if i + 1 == widths.len() {
                        " │"
                    } else {
                        " │ "
                    },
                    Style::fg(theme::MUTED),
                );
            }
            out.push(l);
        }
        out
    }
}

fn heading_level(s: &str) -> Option<usize> {
    let n = s.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&n) && s[n..].starts_with(' ') {
        Some(n)
    } else {
        None
    }
}

fn is_rule(s: &str) -> bool {
    let t: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
}

fn list_item(s: &str) -> Option<(String, &str)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(r) = s.strip_prefix(m) {
            return Some((m.trim().to_string(), r));
        }
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let rest = &s[digits.len()..];
        for d in [". ", ") "] {
            if let Some(r) = rest.strip_prefix(d) {
                return Some((format!("{digits}{}", d.trim()), r));
            }
        }
    }
    None
}

fn strip_inline(s: &str) -> String {
    let mut l = Line::new();
    inline(s, Style::default(), &mut l);
    l.text()
}

/// Inline Markdown: `code`, **bold**, *italic*, _italic_, ~~strike~~, [link](url).
pub fn inline(s: &str, base: Style, out: &mut Line) {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut buf = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut strike = false;
    let style_of = |bold: bool, italic: bool, strike: bool| {
        let mut st = base;
        st.bold |= bold;
        st.italic |= italic;
        st.strike |= strike;
        st
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        if c == '\\'
            && next
                .map(|n| "\\`*_{}[]()#+-.!|~".contains(n))
                .unwrap_or(false)
        {
            buf.push(next.unwrap());
            i += 2;
            continue;
        }
        if c == '`' {
            let ticks = chars[i..].iter().take_while(|c| **c == '`').count();
            let close = (i + ticks..chars.len().saturating_sub(ticks - 1)).find(|&j| {
                chars[j..j + ticks].iter().all(|c| *c == '`') && chars.get(j + ticks) != Some(&'`')
            });
            if let Some(j) = close {
                out.push(std::mem::take(&mut buf), style_of(bold, italic, strike));
                let code: String = chars[i + ticks..j].iter().collect();
                out.push(code.trim().to_string(), Style::fg(theme::CODE));
                i = j + ticks;
                continue;
            }
        }
        if c == '*' && next == Some('*') {
            out.push(std::mem::take(&mut buf), style_of(bold, italic, strike));
            bold = !bold;
            i += 2;
            continue;
        }
        if c == '~' && next == Some('~') {
            out.push(std::mem::take(&mut buf), style_of(bold, italic, strike));
            strike = !strike;
            i += 2;
            continue;
        }
        if c == '*' || c == '_' {
            let opening = !italic
                && next.map(|n| !n.is_whitespace()).unwrap_or(false)
                && prev.map(|p| !p.is_alphanumeric()).unwrap_or(true);
            let closing = italic
                && prev.map(|p| !p.is_whitespace()).unwrap_or(false)
                && next.map(|n| !n.is_alphanumeric()).unwrap_or(true);
            if opening || closing {
                out.push(std::mem::take(&mut buf), style_of(bold, italic, strike));
                italic = !italic;
                i += 1;
                continue;
            }
        }
        if c == '[' {
            if let Some(close) = chars[i..].iter().position(|c| *c == ']').map(|p| p + i) {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = chars[close..]
                        .iter()
                        .position(|c| *c == ')')
                        .map(|p| p + close)
                    {
                        out.push(std::mem::take(&mut buf), style_of(bold, italic, strike));
                        let text: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..end].iter().collect();
                        out.push(
                            text.clone(),
                            style_of(bold, italic, strike)
                                .with_fg(theme::ACCENT)
                                .underline(),
                        );
                        if url != text && !url.starts_with('#') {
                            out.push(format!(" ({url})"), Style::fg(theme::MUTED));
                        }
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    out.push(buf, style_of(bold, italic, strike));
}

const KEYWORDS: &[&str] = &[
    "fn",
    "let",
    "mut",
    "pub",
    "use",
    "mod",
    "impl",
    "struct",
    "enum",
    "trait",
    "match",
    "if",
    "else",
    "for",
    "while",
    "loop",
    "return",
    "async",
    "await",
    "const",
    "static",
    "type",
    "where",
    "self",
    "Self",
    "crate",
    "super",
    "in",
    "def",
    "class",
    "import",
    "from",
    "as",
    "with",
    "try",
    "except",
    "finally",
    "raise",
    "lambda",
    "yield",
    "pass",
    "function",
    "var",
    "export",
    "default",
    "new",
    "this",
    "extends",
    "interface",
    "implements",
    "package",
    "func",
    "go",
    "defer",
    "chan",
    "select",
    "case",
    "switch",
    "break",
    "continue",
    "struct",
    "public",
    "private",
    "protected",
    "void",
    "int",
    "string",
    "bool",
    "true",
    "false",
    "null",
    "None",
    "True",
    "False",
    "nil",
    "undefined",
    "and",
    "or",
    "not",
    "is",
    "unsafe",
    "dyn",
    "ref",
    "move",
    "throw",
    "throws",
    "catch",
    "then",
    "do",
    "done",
    "fi",
    "echo",
    "local",
    "readonly",
];

/// Tiny lexical highlighter: comments, strings, numbers, keywords.
pub fn highlight(line: &str, lang: &str) -> Vec<(String, Style)> {
    let lang = lang.to_lowercase();
    if matches!(
        lang.as_str(),
        "" | "text" | "txt" | "md" | "markdown" | "output" | "console" | "log"
    ) {
        return vec![(line.to_string(), Style::default())];
    }
    if matches!(lang.as_str(), "diff" | "patch") {
        let st = if line.starts_with('+') {
            Style::fg(theme::OK)
        } else if line.starts_with('-') {
            Style::fg(theme::ERR)
        } else if line.starts_with("@@") {
            Style::fg(theme::ACCENT)
        } else {
            Style::default()
        };
        return vec![(line.to_string(), st)];
    }
    let hash_comments = matches!(
        lang.as_str(),
        "py" | "python"
            | "sh"
            | "bash"
            | "zsh"
            | "shell"
            | "rb"
            | "ruby"
            | "yaml"
            | "yml"
            | "toml"
            | "r"
            | "perl"
            | "dockerfile"
            | "make"
            | "makefile"
            | "nix"
            | "conf"
            | "ini"
    );
    let dash_comments = matches!(lang.as_str(), "sql" | "lua" | "haskell" | "hs");
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<(String, Style)> = vec![];
    let mut i = 0;
    let mut word = String::new();
    let flush_word = |word: &mut String, out: &mut Vec<(String, Style)>| {
        if word.is_empty() {
            return;
        }
        let st = if KEYWORDS.contains(&word.as_str()) {
            Style::fg(Color::Magenta)
        } else if word
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            Style::fg(Color::Yellow)
        } else if word
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
            && word.len() > 1
        {
            Style::fg(Color::Cyan)
        } else {
            Style::default()
        };
        out.push((std::mem::take(word), st));
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let is_comment = (c == '/' && next == Some('/'))
            || (hash_comments && c == '#')
            || (dash_comments && c == '-' && next == Some('-'))
            || (c == '/' && next == Some('*'));
        if is_comment && !(hash_comments && c == '#' && lang == "css") {
            flush_word(&mut word, &mut out);
            let rest: String = chars[i..].iter().collect();
            out.push((rest, Style::fg(theme::MUTED).italic()));
            return out;
        }
        if c == '"' || c == '\'' || c == '`' {
            // Rust lifetimes / char literals heuristics: treat 'a as not a string when unterminated.
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == c && chars[j - 1] != '\\')
            {
                flush_word(&mut word, &mut out);
                let s: String = chars[i..=end].iter().collect();
                out.push((s, Style::fg(Color::Green)));
                i = end + 1;
                continue;
            }
        }
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush_word(&mut word, &mut out);
            out.push((c.to_string(), Style::default()));
        }
        i += 1;
    }
    flush_word(&mut word, &mut out);
    // merge adjacent default-styled chunks
    let mut merged: Vec<(String, Style)> = vec![];
    for (t, s) in out {
        if let Some(last) = merged.last_mut() {
            if last.1 == s {
                last.0.push_str(&t);
                continue;
            }
        }
        merged.push((t, s));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(md: &str) -> Vec<String> {
        let mut m = Markdown::new(80);
        let mut out = vec![];
        for l in md.lines() {
            out.extend(m.line(l));
        }
        out.extend(m.finish());
        out.iter().map(|l| l.text()).collect()
    }

    #[test]
    fn basic_blocks() {
        let r = render(
            "# Title\n\nSome **bold** and `code`.\n- item one\n1. first\n```rust\nfn main() {}\n```\n> quote",
        );
        assert_eq!(r[0], "Title");
        assert!(r.contains(&"Some bold and code.".to_string()));
        assert!(r.contains(&"• item one".to_string()));
        assert!(r.contains(&"1. first".to_string()));
        assert!(r.iter().any(|l| l.contains("╭─ rust")));
        assert!(r.iter().any(|l| l.contains("│ fn main() {}")));
        assert!(r.iter().any(|l| l.starts_with("▎ quote")));
    }

    #[test]
    fn table_aligned() {
        let r = render("| a | bb |\n|---|---|\n| ccc | d |\ntext");
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].chars().count(), r[2].chars().count());
    }

    #[test]
    fn snake_case_not_italic() {
        let mut l = Line::new();
        inline("use foo_bar_baz here", Style::default(), &mut l);
        assert_eq!(l.text(), "use foo_bar_baz here");
        assert!(l.spans.iter().all(|s| !s.style.italic));
    }

    #[test]
    fn links() {
        let mut l = Line::new();
        inline("see [docs](https://x.y)", Style::default(), &mut l);
        assert_eq!(l.text(), "see docs (https://x.y)");
    }
}

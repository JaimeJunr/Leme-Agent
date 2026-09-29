//! Multi-line input editor with history and paste placeholders.

use super::style::{Line, Style, theme};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct Composer {
    pub text: String,
    /// Byte offset of the cursor.
    pub cursor: usize,
    history: Vec<String>,
    hist_idx: Option<usize>,
    draft: String,
    pastes: Vec<String>,
    history_path: Option<std::path::PathBuf>,
}

fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i]
        .grapheme_indices(true)
        .next_back()
        .map(|(j, _)| j)
        .unwrap_or(0)
}

fn next_boundary(s: &str, i: usize) -> usize {
    s[i..]
        .graphemes(true)
        .next()
        .map(|g| i + g.len())
        .unwrap_or(s.len())
}

impl Composer {
    pub fn with_history(path: std::path::PathBuf) -> Composer {
        let history = std::fs::read_to_string(&path)
            .map(|s| {
                s.lines()
                    .filter_map(|l| serde_json::from_str::<String>(l).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Composer {
            history,
            history_path: Some(path),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.hist_idx = None;
    }

    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.hist_idx = None;
    }

    /// Large pastes are shown as a placeholder and expanded on submit.
    pub fn paste(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let lines = s.lines().count();
        if lines > 8 || s.len() > 1500 {
            self.pastes.push(s.clone());
            let tag = format!("[Pasted text #{} +{} lines]", self.pastes.len(), lines);
            self.insert(&tag);
        } else {
            self.insert(&s);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Delete a whole paste placeholder at once.
        if self.text[..self.cursor].ends_with(']')
            && let Some(start) = self.text[..self.cursor].rfind("[Pasted text #")
            && !self.text[start..self.cursor].contains('\n')
        {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
            return;
        }
        let p = prev_boundary(&self.text, self.cursor);
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let n = next_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..n, "");
    }

    pub fn left(&mut self) {
        self.cursor = prev_boundary(&self.text, self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = next_boundary(&self.text, self.cursor);
    }

    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        let idx = trimmed
            .rfind(|c: char| c.is_whitespace() || "/.,;:()[]{}".contains(c))
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = idx;
    }

    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skip_ws = after.len() - after.trim_start().len();
        let rest = &after[skip_ws..];
        let idx = rest
            .find(|c: char| c.is_whitespace() || "/.,;:()[]{}".contains(c))
            .unwrap_or(rest.len());
        self.cursor += skip_ws + idx.max(if rest.is_empty() { 0 } else { 1 }).min(rest.len());
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len())
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    pub fn kill_to_end(&mut self) {
        let e = self.line_end();
        if e == self.cursor && e < self.text.len() {
            self.text.remove(e);
        } else {
            self.text.replace_range(self.cursor..e, "");
        }
    }

    pub fn kill_to_start(&mut self) {
        let s = self.line_start();
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
    }

    pub fn delete_word(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }

    pub fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }

    /// Move up a line; returns false if already on the first line.
    pub fn up(&mut self) -> bool {
        if self.on_first_line() {
            return false;
        }
        let col = self.text[self.line_start()..self.cursor]
            .graphemes(true)
            .count();
        let prev_end = self.line_start() - 1;
        let prev_start = self.text[..prev_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = advance(&self.text, prev_start, prev_end, col);
        true
    }

    pub fn down(&mut self) -> bool {
        if self.on_last_line() {
            return false;
        }
        let col = self.text[self.line_start()..self.cursor]
            .graphemes(true)
            .count();
        let next_start = self.line_end() + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(self.text.len());
        self.cursor = advance(&self.text, next_start, next_end, col);
        true
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist_idx {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(idx);
        self.text = self.history[idx].clone();
        self.cursor = self.text.len();
    }

    pub fn history_next(&mut self) {
        match self.hist_idx {
            None => {}
            Some(i) if i + 1 >= self.history.len() => {
                self.hist_idx = None;
                self.text = std::mem::take(&mut self.draft);
                self.cursor = self.text.len();
            }
            Some(i) => {
                self.hist_idx = Some(i + 1);
                self.text = self.history[i + 1].clone();
                self.cursor = self.text.len();
            }
        }
    }

    /// Take the text (expanding paste placeholders) and record history.
    pub fn take(&mut self) -> String {
        let raw = std::mem::take(&mut self.text);
        let mut out = raw.clone();
        for (i, p) in self.pastes.iter().enumerate() {
            let lines = p.lines().count();
            out = out.replacen(&format!("[Pasted text #{} +{} lines]", i + 1, lines), p, 1);
        }
        if !raw.trim().is_empty() && self.history.last() != Some(&out) {
            self.history.push(out.clone());
            if let Some(path) = &self.history_path {
                if let Some(d) = path.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "{}", serde_json::to_string(&out).unwrap_or_default());
                }
            }
        }
        self.clear();
        out
    }

    /// The whitespace-delimited token under the cursor and its byte range.
    pub fn current_token(&self) -> (usize, usize, &str) {
        let start = self.text[..self.cursor]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = self.text[self.cursor..]
            .find(char::is_whitespace)
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len());
        (start, end, &self.text[start..end])
    }

    pub fn replace_range(&mut self, start: usize, end: usize, with: &str) {
        self.text.replace_range(start..end, with);
        self.cursor = start + with.len();
    }

    /// Render into lines with a prompt; returns (lines, (row, col)).
    pub fn render(
        &self,
        width: usize,
        prompt: &str,
        prompt_style: Style,
        placeholder: &str,
    ) -> (Vec<Line>, (u16, u16)) {
        let pw = prompt.width();
        let avail = width.saturating_sub(pw + 1).max(10);
        let mut lines = vec![];
        let mut cur_pos = (0u16, pw as u16);
        if self.text.is_empty() {
            let mut l = Line::styled(prompt, prompt_style);
            l.push(placeholder, Style::fg(theme::MUTED));
            return (vec![l], (0, pw as u16));
        }
        let mut offset = 0usize;
        for (li, logical) in self.text.split('\n').enumerate() {
            // Char-wrap each logical line.
            let mut row = String::new();
            let mut row_w = 0;
            let mut row_start = offset;
            let mut rows: Vec<(String, usize)> = vec![];
            for (gi, g) in logical.grapheme_indices(true) {
                let gw = g.width();
                if row_w + gw > avail {
                    rows.push((std::mem::take(&mut row), row_start));
                    row_w = 0;
                    row_start = offset + gi;
                }
                row.push_str(g);
                row_w += gw;
            }
            rows.push((row, row_start));
            let n = rows.len();
            for (ri, (r, start)) in rows.into_iter().enumerate() {
                let prefix = if li == 0 && ri == 0 {
                    prompt.to_string()
                } else {
                    " ".repeat(pw)
                };
                let pstyle = if li == 0 && ri == 0 {
                    prompt_style
                } else {
                    Style::default()
                };
                let mut l = Line::styled(prefix, pstyle);
                l.push(r.clone(), Style::default());
                let end = start + r.len();
                if self.cursor >= start
                    && (self.cursor < end || (self.cursor == end && (ri + 1 == n)))
                {
                    let col = self.text[start..self.cursor].width();
                    cur_pos = (lines.len() as u16, (pw + col) as u16);
                }
                lines.push(l);
            }
            offset += logical.len() + 1;
        }
        (lines, cur_pos)
    }
}

fn advance(s: &str, start: usize, end: usize, cols: usize) -> usize {
    let mut i = start;
    for (n, (gi, g)) in s[start..end].grapheme_indices(true).enumerate() {
        if n >= cols {
            return start + gi;
        }
        i = start + gi + g.len();
    }
    i.min(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing() {
        let mut c = Composer::default();
        c.insert("hello world");
        c.word_left();
        assert_eq!(c.cursor, 6);
        c.delete_word();
        assert_eq!(c.text, "world");
        c.end();
        c.insert("\nline2");
        assert!(c.up());
        assert!(c.on_first_line());
        assert!(c.down());
        c.backspace();
        assert_eq!(c.text, "world\nline");
        c.home();
        c.kill_to_end();
        assert_eq!(c.text, "world\n");
    }

    #[test]
    fn paste_placeholder() {
        let mut c = Composer::default();
        let big: String = (0..20).map(|i| format!("l{i}\n")).collect();
        c.paste(&big);
        assert!(c.text.starts_with("[Pasted text #1"));
        let out = c.take();
        assert!(out.contains("l19"));
    }

    #[test]
    fn render_cursor() {
        let mut c = Composer::default();
        c.insert("ab\ncd");
        let (lines, pos) = c.render(40, "› ", Style::default(), "");
        assert_eq!(lines.len(), 2);
        assert_eq!(pos, (1, 4));
    }
}

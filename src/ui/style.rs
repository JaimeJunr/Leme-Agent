//! Styled text primitives, ANSI rendering and word wrapping.

use crossterm::style::Color;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub reverse: bool,
}

impl Style {
    pub fn fg(c: Color) -> Style {
        Style {
            fg: Some(c),
            ..Default::default()
        }
    }
    pub fn bold(mut self) -> Style {
        self.bold = true;
        self
    }
    pub fn italic(mut self) -> Style {
        self.italic = true;
        self
    }
    pub fn dimmed(mut self) -> Style {
        self.dim = true;
        self
    }
    pub fn underline(mut self) -> Style {
        self.underline = true;
        self
    }
    pub fn with_fg(mut self, c: Color) -> Style {
        self.fg = Some(c);
        self
    }
}

pub mod theme {
    use crossterm::style::Color;
    pub const ACCENT: Color = Color::Cyan;
    pub const OK: Color = Color::Green;
    pub const ERR: Color = Color::Red;
    pub const WARN: Color = Color::Yellow;
    pub const MUTED: Color = Color::DarkGrey;
    pub const CODE: Color = Color::Yellow;
    pub const HEADING: Color = Color::Magenta;
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn new() -> Line {
        Line::default()
    }
    pub fn raw(text: impl Into<String>) -> Line {
        Line {
            spans: vec![Span {
                text: text.into(),
                style: Style::default(),
            }],
        }
    }
    pub fn styled(text: impl Into<String>, style: Style) -> Line {
        Line {
            spans: vec![Span {
                text: text.into(),
                style,
            }],
        }
    }
    pub fn push(&mut self, text: impl Into<String>, style: Style) -> &mut Line {
        let text = text.into();
        if text.is_empty() {
            return self;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
            return self;
        }
        self.spans.push(Span { text, style });
        self
    }
    pub fn with(mut self, text: impl Into<String>, style: Style) -> Line {
        self.push(text, style);
        self
    }
    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| s.text.width()).sum()
    }
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }
}

fn sgr(style: &Style) -> String {
    let mut codes: Vec<String> = vec![];
    if style.bold {
        codes.push("1".into());
    }
    if style.dim {
        codes.push("2".into());
    }
    if style.italic {
        codes.push("3".into());
    }
    if style.underline {
        codes.push("4".into());
    }
    if style.reverse {
        codes.push("7".into());
    }
    if style.strike {
        codes.push("9".into());
    }
    if let Some(c) = style.fg {
        codes.push(color_code(c, false));
    }
    if let Some(c) = style.bg {
        codes.push(color_code(c, true));
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn color_code(c: Color, bg: bool) -> String {
    let base = if bg { 40 } else { 30 };
    let bright = if bg { 100 } else { 90 };
    match c {
        Color::Black => format!("{}", base),
        Color::DarkRed => format!("{}", base + 1),
        Color::DarkGreen => format!("{}", base + 2),
        Color::DarkYellow => format!("{}", base + 3),
        Color::DarkBlue => format!("{}", base + 4),
        Color::DarkMagenta => format!("{}", base + 5),
        Color::DarkCyan => format!("{}", base + 6),
        Color::Grey => format!("{}", base + 7),
        Color::DarkGrey => format!("{}", bright),
        Color::Red => format!("{}", bright + 1),
        Color::Green => format!("{}", bright + 2),
        Color::Yellow => format!("{}", bright + 3),
        Color::Blue => format!("{}", bright + 4),
        Color::Magenta => format!("{}", bright + 5),
        Color::Cyan => format!("{}", bright + 6),
        Color::White => format!("{}", bright + 7),
        Color::Rgb { r, g, b } => format!("{};2;{r};{g};{b}", if bg { 48 } else { 38 }),
        Color::AnsiValue(v) => format!("{};5;{v}", if bg { 48 } else { 38 }),
        Color::Reset => {
            if bg {
                "49".into()
            } else {
                "39".into()
            }
        }
    }
}

/// Render a line to an ANSI string (always ends with a reset).
pub fn render_ansi(line: &Line) -> String {
    let mut s = String::new();
    for span in &line.spans {
        let code = sgr(&span.style);
        if code.is_empty() {
            s.push_str(&sanitize(&span.text));
        } else {
            s.push_str(&code);
            s.push_str(&sanitize(&span.text));
            s.push_str("\x1b[0m");
        }
    }
    s
}

/// Strip control characters that would corrupt the terminal layout.
pub fn sanitize(s: &str) -> String {
    if !s.chars().any(|c| c.is_control()) {
        return s.to_string();
    }
    s.chars()
        .map(|c| match c {
            '\t' => ' ',
            c if c.is_control() => '\u{FFFD}',
            c => c,
        })
        .filter(|c| *c != '\u{FFFD}')
        .collect()
}

/// Word-wrap a styled line to `width` columns. Continuation lines are
/// prefixed with `indent` spaces.
pub fn wrap(line: &Line, width: usize, indent: usize) -> Vec<Line> {
    let width = width.max(8);
    if line.width() <= width && !line.spans.iter().any(|s| s.text.contains('\t')) {
        return vec![line.clone()];
    }
    // Tokenize into words (keeping whitespace as separate tokens) per span.
    let mut tokens: Vec<(String, Style)> = vec![];
    for span in &line.spans {
        let text = span.text.replace('\t', "    ");
        for w in text.split_word_bounds() {
            tokens.push((w.to_string(), span.style));
        }
    }
    let mut out: Vec<Line> = vec![];
    let mut cur = Line::new();
    let mut cur_w = 0usize;
    let pad = " ".repeat(indent.min(width / 2));
    let pad_w = pad.len();
    let mut first = true;
    for (tok, style) in tokens {
        let tw = tok.width();
        let avail = width - if first { 0 } else { pad_w };
        if cur_w + tw <= avail {
            cur.push(tok, style);
            cur_w += tw;
            continue;
        }
        if tok.trim().is_empty() {
            // Break at whitespace: drop it.
            out.push(std::mem::take(&mut cur));
            first = false;
            cur.push(pad.clone(), Style::default());
            cur_w = 0;
            continue;
        }
        if tw <= avail && cur_w > 0 {
            out.push(std::mem::take(&mut cur));
            first = false;
            cur.push(pad.clone(), Style::default());
            cur_w = 0;
            cur.push(tok, style);
            cur_w += tw;
            continue;
        }
        // Hard-break a long token by graphemes.
        for g in tok.graphemes(true) {
            let gw = g.width();
            let avail = width - if first { 0 } else { pad_w };
            if cur_w + gw > avail && cur_w > 0 {
                out.push(std::mem::take(&mut cur));
                first = false;
                cur.push(pad.clone(), Style::default());
                cur_w = 0;
            }
            cur.push(g, style);
            cur_w += gw;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Truncate a line to `width` columns (adds … when cut).
pub fn truncate(line: &Line, width: usize) -> Line {
    if line.width() <= width {
        return line.clone();
    }
    let mut out = Line::new();
    let mut w = 0;
    for span in &line.spans {
        for g in span.text.graphemes(true) {
            let gw = g.width();
            if w + gw + 1 > width {
                out.push("…", span.style);
                return out;
            }
            out.push(g, span.style);
            w += gw;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words() {
        let l = Line::raw("hello world this is a long line of text");
        let w = wrap(&l, 12, 2);
        assert!(w.len() > 2);
        for x in &w {
            assert!(x.width() <= 12, "{:?}", x.text());
        }
        assert!(w[1].text().starts_with("  "));
    }

    #[test]
    fn wraps_long_token() {
        let l = Line::raw("a".repeat(30));
        let w = wrap(&l, 10, 0);
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn wide_chars() {
        let l = Line::raw("日本語のテキストです日本語のテキストです");
        for x in wrap(&l, 10, 0) {
            assert!(x.width() <= 10);
        }
    }

    #[test]
    fn ansi_render() {
        let l = Line::new()
            .with("a", Style::fg(Color::Red).bold())
            .with("b", Style::default());
        assert_eq!(render_ansi(&l), "\x1b[1;91ma\x1b[0mb");
    }
}

//! Inline terminal renderer: finished output is printed into the terminal's
//! native scrollback (so scrolling, selection and copy just work), while a
//! small "live" region at the bottom (streaming text, running tools, input
//! box, status) is redrawn in place using synchronized updates — no
//! full-screen takeover and no flicker.

use super::style::{Line, render_ansi, truncate, wrap};
use crossterm::{cursor, queue, terminal};
use std::io::{Stdout, Write};

pub struct Screen {
    out: Stdout,
    /// Rows occupied by the live region currently on screen.
    live_rows: u16,
    /// Row (within the live region) where the cursor was left.
    cursor_row: u16,
    pub width: u16,
    pub height: u16,
    pub active: bool,
}

impl Screen {
    pub fn new() -> std::io::Result<Screen> {
        let (w, h) = terminal::size().unwrap_or((100, 30));
        Ok(Screen {
            out: std::io::stdout(),
            live_rows: 0,
            cursor_row: 0,
            width: w.max(20),
            height: h.max(5),
            active: false,
        })
    }

    pub fn enter(&mut self) -> std::io::Result<()> {
        terminal::enable_raw_mode()?;
        queue!(self.out, crossterm::event::EnableBracketedPaste)?;
        if terminal::supports_keyboard_enhancement().unwrap_or(false) {
            let _ = queue!(
                self.out,
                crossterm::event::PushKeyboardEnhancementFlags(
                    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            );
        }
        self.out.flush()?;
        self.active = true;
        Ok(())
    }

    pub fn leave(&mut self) {
        if !self.active {
            return;
        }
        let _ = self.clear_live();
        if terminal::supports_keyboard_enhancement().unwrap_or(false) {
            let _ = queue!(self.out, crossterm::event::PopKeyboardEnhancementFlags);
        }
        let _ = queue!(
            self.out,
            crossterm::event::DisableBracketedPaste,
            cursor::Show
        );
        let _ = self.out.flush();
        let _ = terminal::disable_raw_mode();
        self.active = false;
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        self.width = w.max(20);
        self.height = h.max(5);
    }

    fn clear_live(&mut self) -> std::io::Result<()> {
        if self.cursor_row > 0 {
            queue!(self.out, cursor::MoveUp(self.cursor_row))?;
        }
        queue!(
            self.out,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::FromCursorDown)
        )?;
        self.live_rows = 0;
        self.cursor_row = 0;
        Ok(())
    }

    /// Print lines permanently (into scrollback) and redraw the live region.
    pub fn render(
        &mut self,
        commit: &[Line],
        live: &[Line],
        cursor_pos: Option<(u16, u16)>,
    ) -> std::io::Result<()> {
        let w = self.width as usize;
        queue!(self.out, terminal::BeginSynchronizedUpdate, cursor::Hide)?;
        self.clear_live()?;
        let mut buf = String::new();
        for l in commit {
            for wl in wrap(l, w, 0) {
                buf.push_str(&render_ansi(&wl));
                buf.push_str("\r\n");
            }
        }
        // Live region: never taller than the screen (keep the bottom part).
        let max_rows = (self.height as usize).saturating_sub(1).max(3);
        let mut rows: Vec<Line> = live.iter().map(|l| truncate(l, w)).collect();
        let mut cursor = cursor_pos;
        if rows.len() > max_rows {
            let cut = rows.len() - max_rows;
            rows.drain(..cut);
            cursor = cursor.map(|(r, c)| (r.saturating_sub(cut as u16), c));
        }
        for (i, l) in rows.iter().enumerate() {
            buf.push_str(&render_ansi(l));
            if i + 1 < rows.len() {
                buf.push_str("\r\n");
            }
        }
        self.out.write_all(buf.as_bytes())?;
        self.live_rows = rows.len() as u16;
        let last = self.live_rows.saturating_sub(1);
        match cursor {
            Some((r, c)) => {
                let r = r.min(last);
                if last > r {
                    queue!(self.out, cursor::MoveUp(last - r))?;
                }
                queue!(self.out, cursor::MoveToColumn(c), cursor::Show)?;
                self.cursor_row = r;
            }
            None => {
                queue!(self.out, cursor::MoveToColumn(0))?;
                self.cursor_row = last;
            }
        }
        queue!(self.out, terminal::EndSynchronizedUpdate)?;
        self.out.flush()
    }

    /// Emit a raw escape sequence (OSC 52 clipboard, notifications, title).
    pub fn raw(&mut self, s: &str) {
        let _ = self.out.write_all(s.as_bytes());
        let _ = self.out.flush();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.leave();
    }
}

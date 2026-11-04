//! Terminal state of one pane: a vt100 parser plus replies to terminal queries.
//!
//! Programs ask their terminal questions (cursor position, device attributes, colours).
//! A pane may have no client attached, so orchd answers them itself; otherwise TUIs such as
//! Codex stall on start-up waiting for the reply.

use std::num::NonZeroU16;

#[derive(Default)]
pub struct Callbacks {
    /// Bytes to write back to the program (query replies).
    pub replies: Vec<u8>,
    pub title: String,
    pub bells: u64,
}

impl vt100::Callbacks for Callbacks {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).chars().take(200).collect();
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, i2, c) {
            // DSR: status / cursor position.
            (None, None, 'n') if first == 5 => self.replies.extend_from_slice(b"\x1b[0n"),
            (None, None, 'n') if first == 6 => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (Some(b'?'), None, 'n') if first == 6 => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[?{};{}R", row + 1, col + 1).as_bytes());
            }
            // DA1 (VT220 with ANSI colour) and DA2.
            (None, None, 'c') if first == 0 => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            (Some(b'>'), None, 'c') if first == 0 => {
                self.replies.extend_from_slice(b"\x1b[>1;10;0c")
            }
            // XTVERSION.
            (Some(b'>'), None, 'q') => self.replies.extend_from_slice(b"\x1bP>|orchtool\x1b\\"),
            // DECRQM: report every queried private mode as "not recognised".
            (Some(b'?'), Some(b'$'), 'p') => self
                .replies
                .extend_from_slice(format!("\x1b[?{first};0$y").as_bytes()),
            // Kitty keyboard query (CSI ? u): no reply = unsupported; DA1 follows it anyway.
            _ => {}
        }
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            [b"10", b"?"] => self
                .replies
                .extend_from_slice(b"\x1b]10;rgb:d0d0/d0d0/d0d0\x1b\\"),
            [b"11", b"?"] => self
                .replies
                .extend_from_slice(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\"),
            _ => {}
        }
    }
}

pub struct Term {
    parser: vt100::Parser<Callbacks>,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap()
}

impl Term {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                nz(rows),
                nz(cols),
                scrollback,
                Callbacks::default(),
            ),
        }
    }

    /// Feed program output; returns query replies to write back to the program.
    pub fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.process(bytes);
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn size(&self) -> (u16, u16) {
        let (r, c) = self.parser.screen().size();
        (r.get(), c.get())
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(nz(rows), nz(cols));
    }

    pub fn title(&self) -> &str {
        &self.parser.callbacks().title
    }

    /// Visible screen only, trailing blank lines dropped.
    pub fn visible_text(&self) -> String {
        let (_, cols) = self.size();
        let rows: Vec<String> = self.screen().rows(0, cols).collect();
        join_trimmed(rows)
    }

    /// Scrollback + screen; the last `lines` lines (trailing blank lines dropped first).
    pub fn recent_text(&mut self, lines: usize) -> String {
        self.recent_text_mode(lines, false)
    }

    /// Logical lines for copying: only soft wraps are joined, hard newlines remain.
    pub fn recent_text_unwrapped(&mut self, lines: usize) -> String {
        self.recent_text_mode(lines, true)
    }

    fn recent_text_mode(&mut self, lines: usize, unwrapped: bool) -> String {
        let (rows, cols) = self.size();
        let rows = rows as usize;
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let total = screen.scrollback(); // rows of scrollback actually present
                                         // View at offset o shows combined lines [total - o, total - o + rows).
                                         // We need combined lines from max(0, total + rows - lines - slack); fetch a page more
                                         // than needed because trailing blank lines are trimmed afterwards.
        let want = lines.saturating_add(rows);
        // A logical line can span any number of rows. Start at the oldest retained row so
        // --lines never cuts a soft-wrapped line just because the pane is narrow.
        let mut offset = if unwrapped {
            total
        } else {
            total.min(want.saturating_sub(rows))
        };
        let mut out: Vec<String> = Vec::new();
        let mut continued = false;
        let mut next = total - offset;
        loop {
            screen.set_scrollback(offset);
            let start = total - offset;
            for (i, row) in screen.rows(0, cols).enumerate() {
                if start + i >= next {
                    if unwrapped && continued {
                        out.last_mut().expect("previous wrapped row").push_str(&row);
                    } else {
                        out.push(row);
                    }
                    continued = screen.row_wrapped(i as u16);
                    next = start + i + 1;
                }
            }
            if offset == 0 {
                break;
            }
            offset = offset.saturating_sub(rows);
        }
        screen.set_scrollback(0);
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        let skip = out.len().saturating_sub(lines);
        out.drain(..skip);
        out.join("\n")
    }
}

fn join_trimmed(mut rows: Vec<String>) -> String {
    while rows.last().is_some_and(|l| l.trim().is_empty()) {
        rows.pop();
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_cursor_position_and_device_attributes() {
        let mut t = Term::new(24, 80, 0);
        let r = t.process(b"ab\x1b[6n");
        assert_eq!(r, b"\x1b[1;3R");
        let r = t.process(b"\x1b[c\x1b[>c");
        assert_eq!(r, b"\x1b[?62;22c\x1b[>1;10;0c");
        // Fragmented query still answered once complete.
        assert!(t.process(b"\x1b[").is_empty());
        assert_eq!(t.process(b"6n"), b"\x1b[1;3R");
        assert_eq!(
            t.process(b"\x1b]11;?\x07"),
            b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\"
        );
    }

    #[test]
    fn title_is_captured() {
        let mut t = Term::new(5, 20, 0);
        t.process(b"\x1b]2;hello title\x07");
        assert_eq!(t.title(), "hello title");
    }

    #[test]
    fn unwrapped_joins_soft_rows_across_scrollback_pages_and_preserves_hard_lines() {
        let mut t = Term::new(3, 8, 100);
        let long = "abcdefghijklmnopqrstuvwxyz0123456789";
        t.process(format!("first\r\n{long}\r\n\r\nlast").as_bytes());
        assert_eq!(
            t.recent_text_unwrapped(100),
            format!("first\n{long}\n\nlast")
        );
        assert_eq!(t.recent_text_unwrapped(3), format!("{long}\n\nlast"));
        assert_eq!(t.recent_text_unwrapped(1), "last");
        assert_eq!(t.recent_text_unwrapped(0), "");
        assert_eq!(t.screen().scrollback(), 0);
        assert!(t.recent_text(100).contains("abcdefgh\nijklmnop"));
    }

    #[test]
    fn unwrapped_keeps_spaces_unicode_and_exact_width_hard_breaks() {
        let mut t = Term::new(6, 6, 100);
        t.process("ab  界cd\r\n123456\r\nZ".as_bytes());
        assert_eq!(t.recent_text_unwrapped(100), "ab  界cd\n123456\nZ");
        assert_eq!(t.screen().scrollback(), 0);
        let mut t = Term::new(4, 5, 10);
        t.process("abcd界e".as_bytes());
        // This vt100 version does not mark the early wrap caused by a wide character
        // that cannot fit in the final column. Preserve the break rather than guessing
        // and accidentally joining a real newline with the same screen contents.
        assert!(!t.screen().row_wrapped(0));
        assert_eq!(t.recent_text_unwrapped(100), "abcd\n界e");
    }

    #[test]
    fn recent_text_includes_scrollback_in_order() {
        let mut t = Term::new(5, 20, 1000);
        for i in 0..50 {
            t.process(format!("line {i}\r\n").as_bytes());
        }
        let all = t.recent_text(1000);
        let lines: Vec<&str> = all.lines().collect();
        assert_eq!(lines.len(), 50);
        assert_eq!(lines[0], "line 0");
        assert_eq!(lines[49], "line 49");
        let last3 = t.recent_text(3);
        assert_eq!(last3, "line 47\nline 48\nline 49");
        // The screen view is restored to the live screen afterwards.
        assert_eq!(t.screen().scrollback(), 0);
        assert!(t.visible_text().ends_with("line 49"));
    }
}

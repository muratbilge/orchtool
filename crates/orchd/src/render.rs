//! Per-client screen sync: turns the pane's current screen into terminal bytes for one client.
//!
//! The renderer remembers what that client was last sent (one formatted string per row, the
//! input modes and the cursor). Each frame only rewrites rows that changed. Nothing is ever
//! queued: when a client is slow, the next frame is computed against what it really has, so it
//! jumps straight to the latest state (mosh-style), and no diff depends on a skipped one.
//!
//! The view is clipped to the client's own terminal size, so a small observer (a phone) never
//! sees wrapped garbage and never shrinks the pane.

use std::fmt::Write as _;

pub struct Renderer {
    view_rows: u16,
    view_cols: u16,
    /// What the client has: one entry per rendered row. Empty = needs a full redraw.
    rows: Vec<String>,
    modes: String,
    cursor: Option<(u16, u16, bool)>,
    cursor_style: Option<vt100::CursorStyle>,
    gen: u64,
    need_full: bool,
}

pub struct Frame {
    pub full: bool,
    pub data: String,
}

impl Renderer {
    pub fn new(view_rows: u16, view_cols: u16) -> Self {
        Self {
            view_rows: view_rows.max(1),
            view_cols: view_cols.max(1),
            rows: Vec::new(),
            modes: String::new(),
            cursor: None,
            cursor_style: None,
            gen: 0,
            need_full: true,
        }
    }

    pub fn set_view(&mut self, rows: u16, cols: u16) {
        self.view_rows = rows.max(1);
        self.view_cols = cols.max(1);
        self.need_full = true;
    }

    pub fn view(&self) -> (u16, u16) {
        (self.view_rows, self.view_cols)
    }

    pub fn force_full(&mut self) {
        self.need_full = true;
    }

    /// `gen` changes whenever the pane is resized; that forces a full redraw.
    /// Returns `None` when the client is already up to date.
    pub fn render(&mut self, screen: &vt100::Screen, gen: u64) -> Option<Frame> {
        let (prows, pcols) = screen.size();
        let h = prows.get().min(self.view_rows);
        let w = pcols.get().min(self.view_cols);
        let full = self.need_full || gen != self.gen || self.rows.len() != h as usize;
        let mut out = String::new();
        if full {
            out.push_str("\x1b[m\x1b[H\x1b[2J");
            self.rows.clear();
            self.rows.resize(h as usize, String::new());
            self.modes.clear();
            self.cursor = None;
            self.cursor_style = None;
        }
        let mut wrote_rows = false;
        for (i, row) in screen.rows_formatted(0, w).take(h as usize).enumerate() {
            if !full && self.rows[i] == row {
                continue;
            }
            if !(full && row.is_empty()) {
                let _ = write!(out, "\x1b[{};1H\x1b[m\x1b[2K", i + 1);
                out.push_str(&row);
                wrote_rows = true;
            }
            self.rows[i] = row;
        }
        let modes = screen.input_mode_formatted();
        if modes != self.modes {
            out.push_str(&modes);
            self.modes = modes;
        }
        let style = screen.cursor_style();
        if self.cursor_style != Some(style) {
            let _ = write!(out, "\x1b[{} q", cursor_style_code(style));
            self.cursor_style = Some(style);
        }
        let (cr, cc) = screen.cursor_position();
        let inside = cr < h && cc < w;
        let visible = inside && !screen.hide_cursor();
        let cursor = (cr.min(h - 1), cc.min(w - 1), visible);
        if wrote_rows || full || self.cursor != Some(cursor) {
            out.push_str("\x1b[m");
            let _ = write!(out, "\x1b[{};{}H", cursor.0 + 1, cursor.1 + 1);
            out.push_str(if visible { "\x1b[?25h" } else { "\x1b[?25l" });
            self.cursor = Some(cursor);
        }
        self.gen = gen;
        self.need_full = false;
        if out.is_empty() {
            None
        } else {
            Some(Frame { full, data: out })
        }
    }
}

fn cursor_style_code(style: vt100::CursorStyle) -> u8 {
    use vt100::CursorStyle::*;
    match style {
        Default => 0,
        BlinkBlock => 1,
        SteadyBlock => 2,
        BlinkUnderline => 3,
        SteadyUnderline => 4,
        BlinkVerticalBar => 5,
        SteadyVerticalBar => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    fn parser(rows: u16, cols: u16) -> vt100::Parser {
        vt100::Parser::new(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
            0,
        )
    }

    /// The client's terminal (a second vt100) must show exactly the server's screen within
    /// the view, cell by cell, including colours and attributes.
    fn assert_same(server: &vt100::Screen, client: &vt100::Screen, h: u16, w: u16) {
        for r in 0..h {
            for c in 0..w {
                let a = server.cell(r, c).unwrap();
                let b = client.cell(r, c).unwrap();
                assert_eq!(
                    (
                        a.contents(),
                        a.fgcolor(),
                        a.bgcolor(),
                        a.bold(),
                        a.inverse(),
                        a.underline()
                    ),
                    (
                        b.contents(),
                        b.fgcolor(),
                        b.bgcolor(),
                        b.bold(),
                        b.inverse(),
                        b.underline()
                    ),
                    "cell {r},{c}"
                );
            }
        }
        let (cr, cc) = server.cursor_position();
        if cr < h && cc < w {
            assert_eq!(client.cursor_position(), (cr, cc));
        }
        assert_eq!(server.bracketed_paste(), client.bracketed_paste());
        assert_eq!(server.application_cursor(), client.application_cursor());
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_chunk(rng: &mut Rng, rows: u16, cols: u16) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..rng.below(12) + 1 {
            match rng.below(14) {
                0 => out.extend(
                    format!(
                        "\x1b[{};{}H",
                        rng.below(rows as u64) + 1,
                        rng.below(cols as u64) + 1
                    )
                    .bytes(),
                ),
                1 => out.extend(format!("\x1b[3{}m", rng.below(8)).bytes()),
                2 => out.extend(format!("\x1b[4{};1m", rng.below(8)).bytes()),
                3 => out.extend(b"\x1b[m"),
                4 => out.extend(b"\r\n"),
                5 => out.extend(b"\x1b[K"),
                6 => out.extend(b"\x1b[2J"),
                7 => out.extend("界字".as_bytes()),
                8 => out.extend(b"\x1b[?2004h"),
                9 => out.extend(b"\x1b[?1h\x1b[7m"),
                10 => out.extend(b"\x1b[3L"),
                11 => out.extend(b"\x1b[?1049h"),
                12 => out.extend(b"\x1b[?1049l"),
                _ => {
                    let n = rng.below(30) as usize;
                    out.extend((0..n).map(|i| b'a' + ((i as u8) % 26)));
                }
            }
        }
        out
    }

    #[test]
    fn random_diffs_reproduce_the_screen() {
        for seed in 1..40u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let (rows, cols) = (8, 30);
            let mut server = parser(rows, cols);
            let mut client = parser(rows, cols);
            let mut r = Renderer::new(rows, cols);
            for step in 0..60 {
                server.process(&random_chunk(&mut rng, rows, cols));
                // Sometimes skip a frame: the next one must still be correct.
                if step % 3 == 1 {
                    continue;
                }
                if let Some(f) = r.render(server.screen(), 0) {
                    client.process(f.data.as_bytes());
                }
                assert_same(server.screen(), client.screen(), rows, cols);
            }
        }
    }

    #[test]
    fn small_observer_view_is_clipped() {
        let mut server = parser(10, 40);
        server.process(b"0123456789012345678901234567890123456789\r\nsecond line\r\n");
        let mut client = parser(5, 20);
        let mut r = Renderer::new(5, 20);
        let f = r.render(server.screen(), 0).unwrap();
        client.process(f.data.as_bytes());
        assert_same(server.screen(), client.screen(), 5, 20);
        // Nothing wrapped into the client's second row.
        assert_eq!(client.screen().rows(0, 20).nth(1).unwrap(), "second line");
    }

    #[test]
    fn unchanged_screen_sends_nothing_and_resize_forces_full() {
        let mut server = parser(5, 20);
        server.process(b"hello");
        let mut r = Renderer::new(5, 20);
        assert!(r.render(server.screen(), 0).unwrap().full);
        assert!(r.render(server.screen(), 0).is_none());
        server.process(b"!");
        let f = r.render(server.screen(), 0).unwrap();
        assert!(!f.full);
        assert!(f.data.len() < 40, "one row + cursor, got {:?}", f.data);
        server.set_size(NonZeroU16::new(6).unwrap(), NonZeroU16::new(20).unwrap());
        assert!(r.render(server.screen(), 1).unwrap().full);
    }
}

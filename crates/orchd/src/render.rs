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
        let clipped = w < pcols.get();
        let rows: Vec<String> = screen.rows_formatted(0, w).take(h as usize).collect();
        for (i, mut row) in rows.into_iter().enumerate() {
            // A wide glyph starting in the last visible column would spill past the client's
            // edge (wrap, or scroll on the bottom row): show a blank there instead.
            if clipped
                && screen
                    .cell(i as u16, w - 1)
                    .is_some_and(|c| c.is_wide() && !c.is_wide_continuation())
            {
                row = screen.rows_formatted(0, w - 1).nth(i).unwrap_or_default();
                // Explicit position: the formatted row may end anywhere (blank runs are
                // emitted as erase sequences that do not move the cursor).
                let _ = write!(row, "\x1b[{};{}H\x1b[m ", i + 1, w);
            }
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
        let modes = input_modes(screen);
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

/// The full input-mode state as explicit set *and* reset sequences. (vt100's
/// `input_mode_formatted` only emits non-default mouse modes, so a mode switched off by the
/// program would stay on in the client's terminal.)
fn input_modes(screen: &vt100::Screen) -> String {
    use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
    let mut s = String::new();
    s.push_str(if screen.application_keypad() {
        "\x1b="
    } else {
        "\x1b>"
    });
    s.push_str(if screen.application_cursor() {
        "\x1b[?1h"
    } else {
        "\x1b[?1l"
    });
    s.push_str(if screen.bracketed_paste() {
        "\x1b[?2004h"
    } else {
        "\x1b[?2004l"
    });
    s.push_str("\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l");
    s.push_str(match screen.mouse_protocol_mode() {
        M::None => "",
        M::Press => "\x1b[?9h",
        M::PressRelease => "\x1b[?1000h",
        M::ButtonMotion => "\x1b[?1002h",
        M::AnyMotion => "\x1b[?1003h",
    });
    if screen.mouse_protocol_mode() != M::None {
        s.push_str(match screen.mouse_protocol_encoding() {
            E::Default => "",
            E::Utf8 => "\x1b[?1005h",
            E::Sgr => "\x1b[?1006h",
        });
    }
    s
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
    fn random_diffs_reproduce_a_clipped_view() {
        for seed in 1..30u64 {
            let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
            let (rows, cols) = (10, 36);
            let (vr, vc) = (6, 21);
            let mut server = parser(rows, cols);
            let mut client = parser(vr, vc);
            let mut r = Renderer::new(vr, vc);
            for step in 0..60 {
                server.process(&random_chunk(&mut rng, rows, cols));
                if step % 4 == 1 {
                    continue;
                }
                let mut last = String::new();
                if let Some(f) = r.render(server.screen(), 0) {
                    client.process(f.data.as_bytes());
                    last = f.data;
                }
                // Same cells, except a wide glyph cut by the edge is shown as a blank.
                for row in 0..vr {
                    for col in 0..vc {
                        let a = server.screen().cell(row, col).unwrap();
                        let b = client.screen().cell(row, col).unwrap();
                        if col == vc - 1 && a.is_wide() {
                            assert!(b.contents().trim().is_empty(), "edge blank at {row},{col}");
                            continue;
                        }
                        if a.contents() != b.contents() && std::env::var("RDEBUG").is_ok() {
                            eprintln!("step {step} server rows:");
                            for l in server.screen().rows(0, cols) {
                                eprintln!("|{l}|");
                            }
                            eprintln!("client rows:");
                            for l in client.screen().rows(0, vc) {
                                eprintln!("|{l}|");
                            }
                            eprintln!("last data: {:?}", last);
                        }
                        assert_eq!(a.contents(), b.contents(), "seed {seed} cell {row},{col}");
                        assert_eq!(a.bgcolor(), b.bgcolor(), "seed {seed} bg {row},{col}");
                    }
                }
                assert_eq!(client.screen().scrollback(), 0);
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
    fn mouse_modes_are_switched_off_again() {
        let mut server = parser(5, 20);
        let mut client = parser(5, 20);
        let mut r = Renderer::new(5, 20);
        server.process(b"\x1b[?1002h\x1b[?1006h");
        client.process(r.render(server.screen(), 0).unwrap().data.as_bytes());
        assert_eq!(
            client.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::ButtonMotion
        );
        assert_eq!(
            client.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );
        server.process(b"\x1b[?1002l\x1b[?1006l");
        client.process(r.render(server.screen(), 0).unwrap().data.as_bytes());
        assert_eq!(
            client.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        assert_eq!(
            client.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Default
        );
        // A full snapshot also states the modes explicitly.
        r.force_full();
        let f = r.render(server.screen(), 0).unwrap();
        assert!(f.data.contains("\x1b[?1000l"));
    }

    #[test]
    fn wide_glyph_on_the_clip_edge_does_not_spill() {
        let mut server = parser(6, 40);
        // 界 starts at column 19 (0-based) of every row, including the bottom one.
        for row in 1..=6 {
            server.process(format!("\x1b[{row};1H{}界x", "a".repeat(19)).as_bytes());
        }
        let mut client = parser(6, 20);
        let mut r = Renderer::new(6, 20);
        client.process(r.render(server.screen(), 0).unwrap().data.as_bytes());
        for (i, line) in client.screen().rows(0, 20).enumerate() {
            assert_eq!(line.trim_end(), "a".repeat(19), "row {i}");
        }
        // Nothing scrolled: the top row is still row 0 of the pane.
        assert_eq!(client.screen().scrollback(), 0);
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

//! Replay a raw PTY-output capture through vt100. This is not a PTY runtime.
use std::io::{self, Read};
use std::num::NonZeroU16;

fn main() -> io::Result<()> {
    let mut parser = vt100::Parser::new(
        NonZeroU16::new(24).unwrap(),
        NonZeroU16::new(80).unwrap(),
        1000,
    );
    let mut input = io::stdin().lock();
    let mut buffer = [0; 4096];
    let mut total = 0_u64;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        parser.process(&buffer[..count]);
    }
    let screen = parser.screen();
    eprintln!(
        "bytes={total} cursor={:?} alternate={} paste={} mouse={:?}/{:?}",
        screen.cursor_position(),
        screen.alternate_screen(),
        screen.bracketed_paste(),
        screen.mouse_protocol_mode(),
        screen.mouse_protocol_encoding()
    );
    println!("{}", screen.contents());
    Ok(())
}

#[cfg(test)]
mod tests {
    fn parser() -> vt100::Parser {
        vt100::Parser::new(24.try_into().unwrap(), 80.try_into().unwrap(), 10)
    }

    #[test]
    fn alternate_screen_restores_primary() {
        let mut p = parser();
        p.process(b"primary\x1b[?1049h\x1b[Halternate");
        assert!(p.screen().alternate_screen());
        assert!(p.screen().contents().contains("alternate"));
        p.process(b"\x1b[?1049l");
        assert!(!p.screen().alternate_screen());
        assert_eq!(p.screen().contents(), "primary");
    }

    #[test]
    fn unicode_and_fragmented_utf8_preserve_width() {
        let mut p = parser();
        for byte in "A界e\u{301}".as_bytes() {
            p.process(&[*byte]);
        }
        assert_eq!(p.screen().cursor_position(), (0, 4));
        assert_eq!(p.screen().contents(), "A界e\u{301}");
    }

    #[test]
    fn paste_mouse_and_resize_are_observable() {
        let mut p = parser();
        let initial_mode = format!("{:?}", p.screen().mouse_protocol_mode());
        let initial_encoding = format!("{:?}", p.screen().mouse_protocol_encoding());
        p.process(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h");
        assert!(p.screen().bracketed_paste());
        assert_ne!(
            initial_mode,
            format!("{:?}", p.screen().mouse_protocol_mode())
        );
        assert_ne!(
            initial_encoding,
            format!("{:?}", p.screen().mouse_protocol_encoding())
        );
        p.set_size(12.try_into().unwrap(), 40.try_into().unwrap());
        assert_eq!(
            p.screen().size(),
            (12.try_into().unwrap(), 40.try_into().unwrap())
        );
        p.process(b"\x1b[?2004l\x1b[?1000l\x1b[?1006l");
        assert!(!p.screen().bracketed_paste());
        assert_eq!(
            initial_mode,
            format!("{:?}", p.screen().mouse_protocol_mode())
        );
        assert_eq!(
            initial_encoding,
            format!("{:?}", p.screen().mouse_protocol_encoding())
        );
    }
}

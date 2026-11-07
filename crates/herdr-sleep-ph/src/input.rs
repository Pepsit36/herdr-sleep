//! Turns raw terminal bytes into the text worth replaying.

/// Strips terminal control sequences and control characters from `text`.
///
/// CSI, OSC and two-character escape sequences are dropped, `\r\n` and `\r`
/// become `\n`, tabs and newlines survive, every other control character and
/// DEL are dropped. Bracketed paste markers are CSI sequences, so a pasted
/// text keeps its content and loses its markers.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    // Parameter and intermediate bytes, then one final byte.
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC runs until BEL or ST (ESC \).
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                Some('O') => {
                    // SS3, e.g. arrows in application mode.
                    chars.next();
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() || c == '\u{fffd}' => {}
            c => out.push(c),
        }
    }
    out
}

/// The text a user typed, as it should be replayed to the agent.
pub fn printable(bytes: &[u8]) -> String {
    clean(&String::from_utf8_lossy(bytes))
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_line() {
        assert_eq!(printable(b"bonjour\r"), "bonjour");
    }

    #[test]
    fn control_keys_only_give_nothing() {
        assert_eq!(printable(b"\x03\x03\x04"), "");
    }

    #[test]
    fn focus_reports_are_dropped() {
        assert_eq!(printable(b"\x1b[I\x1b[Ohello\x1b[I"), "hello");
    }

    #[test]
    fn bracketed_paste_keeps_both_lines() {
        assert_eq!(
            printable(b"\x1b[200~first\r\nsecond\x1b[201~\r"),
            "first\nsecond"
        );
    }

    #[test]
    fn arrows_are_dropped() {
        assert_eq!(printable(b"\x1b[A\x1b[B\x1bOAok\x1b[1;5C"), "ok");
    }

    #[test]
    fn alt_key_and_lone_escape() {
        assert_eq!(printable(b"\x1bxabc\x1b"), "abc");
    }

    #[test]
    fn utf8_survives() {
        assert_eq!(printable("café\r".as_bytes()), "café");
    }

    #[test]
    fn invalid_utf8_is_dropped() {
        assert_eq!(printable(b"ab\xffcd\xc3"), "abcd");
    }

    #[test]
    fn del_and_tab_and_lone_cr() {
        assert_eq!(printable(b"a\x7fb\tc\rd\r"), "ab\tc\nd");
    }

    #[test]
    fn osc_is_dropped() {
        assert_eq!(clean("\x1b]0;title\x07x\x1b]2;t\x1b\\y"), "xy");
    }

    #[test]
    fn colours_are_stripped_but_newlines_stay() {
        assert_eq!(clean("\x1b[31mred\x1b[0m\nnext\r\n"), "red\nnext\n");
    }

    #[test]
    fn trailing_whitespace_is_trimmed() {
        assert_eq!(printable(b"hi  \r\r\n"), "hi");
        assert_eq!(printable(b"\r"), "");
    }
}

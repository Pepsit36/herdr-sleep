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
                // Alt with an arrow is `ESC ESC [ A`: the first ESC stands
                // alone, the rest is parsed as the sequence it is.
                Some('\x1b') => {
                    if chars.peek().is_some() {
                        // Put the second ESC back by parsing it again below.
                        let rest: String = std::iter::once('\x1b').chain(chars.by_ref()).collect();
                        out.push_str(&clean(&rest));
                        return out;
                    }
                }
                Some('[') => {
                    // Parameter and intermediate bytes, then one final byte.
                    let mut params = String::new();
                    let mut final_byte = None;
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7e').contains(&n) {
                            final_byte = Some(n);
                            break;
                        }
                        params.push(n);
                    }
                    // X10 mouse report: `ESC [ M` then three payload bytes.
                    if final_byte == Some('M') && params.is_empty() {
                        for _ in 0..3 {
                            chars.next();
                        }
                    }
                }
                // DCS, SOS, PM and APC strings run until ST (ESC \) or BEL.
                Some('P' | 'X' | '^' | '_') => {
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

/// What the user typed in the Placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// The text to replay, trailing whitespace removed.
    pub text: String,
    /// The user ended it with Enter, so the Replay presses Enter too. Without
    /// it the text is left in the prompt for the user to finish.
    pub submitted: bool,
}

/// The text a user typed and whether they pressed Enter after it.
pub fn captured(bytes: &[u8]) -> Captured {
    let cleaned = clean(&String::from_utf8_lossy(bytes));
    let submitted = cleaned.trim_end_matches([' ', '\t']).ends_with('\n');
    Captured {
        text: cleaned.trim_end().to_string(),
        submitted,
    }
}

/// The text a user typed, as it should be replayed to the agent.
#[cfg(test)]
pub fn printable(bytes: &[u8]) -> String {
    captured(bytes).text
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
    fn enter_at_the_end_means_submitted() {
        assert!(captured(b"fix the build\r").submitted);
        assert!(captured(b"line one\r\nline two\r").submitted);
        assert!(!captured(b"fix the").submitted);
        assert!(!captured(b"fix\r then more").submitted);
        assert!(!captured(b"\x03").submitted);
        // Enter alone is a wake, with nothing to type.
        let alone = captured(b"\r");
        assert!(alone.submitted && alone.text.is_empty());
        assert_eq!(captured(b"hi  \r").text, "hi");
    }

    #[test]
    fn alt_arrow_and_mouse_and_device_strings_leave_no_text() {
        assert_eq!(printable(b"\x1b\x1b[Aok"), "ok");
        assert_eq!(printable(b"a\x1b[M #!b"), "ab");
        assert_eq!(printable(b"x\x1bPq1;2\x1b\\y"), "xy");
        assert_eq!(printable(b"x\x1b_Gf=100;\x1b\\y"), "xy");
    }

    #[test]
    fn trailing_whitespace_is_trimmed() {
        assert_eq!(printable(b"hi  \r\r\n"), "hi");
        assert_eq!(printable(b"\r"), "");
    }
}

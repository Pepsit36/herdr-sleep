//! What the Placeholder shows in the pane.

use crate::input::clean;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

const MAX_BYTES: u64 = 64 * 1024;
const MAX_LINES: usize = 200;

const CLEAR: &str = "\x1b[2J\x1b[H";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Who is asleep, for the banner.
pub struct Banner<'a> {
    pub icon: &'a str,
    pub name: Option<&'a str>,
    pub kind: &'a str,
    pub session: &'a str,
}

/// Reads the tail of the Preview file, stripped of control sequences.
pub fn load_preview(path: &str) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > MAX_BYTES {
        file.seek(SeekFrom::Start(len - MAX_BYTES)).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES).read_to_end(&mut bytes).ok()?;
    Some(limit_lines(&clean(&String::from_utf8_lossy(&bytes))))
}

/// Keeps the last `MAX_LINES` lines.
fn limit_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(MAX_LINES);
    lines[skip..].join("\n")
}

/// The whole screen, with `\r\n` line ends because the terminal is raw.
pub fn render(preview: Option<&str>, banner: &Banner) -> String {
    let mut out = String::from(CLEAR);
    if let Some(preview) = preview.filter(|p| !p.trim().is_empty()) {
        out.push_str(DIM);
        for line in preview.lines() {
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.push_str(RESET);
        out.push_str("\r\n");
    }
    let who = match banner.name {
        Some(name) => format!("{name} ({})", banner.kind),
        None => banner.kind.to_string(),
    };
    out.push_str(&format!(
        "{}  {who} is asleep. Focus this pane, press any key or send a prompt to wake it.\r\n",
        banner.icon
    ));
    out.push_str(&format!("session: {}\r\n", banner.session));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn banner(name: Option<&str>) -> Banner<'_> {
        Banner {
            icon: "z",
            name,
            kind: "claude",
            session: "sess-1",
        }
    }

    #[test]
    fn banner_names_the_agent() {
        let s = render(None, &banner(Some("worker")));
        assert!(s.starts_with("\x1b[2J\x1b[H"));
        assert!(s.contains(
            "z  worker (claude) is asleep. Focus this pane, press any key or send a prompt to wake it."
        ));
        assert!(s.contains("session: sess-1"));
    }

    #[test]
    fn unnamed_agent_shows_its_kind() {
        let s = render(None, &banner(None));
        assert!(s.contains("z  claude is asleep."));
    }

    #[test]
    fn preview_is_dimmed_above_the_banner() {
        let s = render(Some("one\ntwo"), &banner(None));
        let dim = s.find("\x1b[2mone\r\ntwo\r\n\x1b[0m").unwrap();
        assert!(dim < s.find("is asleep").unwrap());
    }

    #[test]
    fn blank_preview_is_skipped() {
        assert!(!render(Some(" \n"), &banner(None)).contains("\x1b[2m"));
    }

    #[test]
    fn keeps_the_last_lines() {
        let text: String = (0..300).map(|i| format!("l{i}\n")).collect();
        let kept = limit_lines(&text);
        assert_eq!(kept.lines().count(), MAX_LINES);
        assert_eq!(kept.lines().next(), Some("l100"));
        assert_eq!(kept.lines().last(), Some("l299"));
    }

    #[test]
    fn load_preview_strips_escapes_and_tolerates_absence() {
        let dir = std::env::temp_dir().join(format!("hsph-screen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.txt");
        std::fs::write(&file, "\x1b[31mred\x1b[0m\n\x1b]0;t\x07ok\n").unwrap();
        assert_eq!(
            load_preview(file.to_str().unwrap()).as_deref(),
            Some("red\nok")
        );
        assert_eq!(load_preview(dir.join("none").to_str().unwrap()), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

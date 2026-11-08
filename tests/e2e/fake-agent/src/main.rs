//! A fake `claude` for the end-to-end suite.
//!
//! It reports itself to Herdr like a real integration (state, session, resume
//! command, release), draws a Claude-like input box with the prompt marker
//! `❯` followed by a no-break space, echoes `got: <line>` for every line and
//! quits on `/exit` or `/quit` after printing the resume hint.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const SOURCE: &str = "herdr:claude";
const AGENT: &str = "claude";
const RULE_WIDTH: usize = 60;

/// Puts the terminal in raw mode and restores it on drop.
struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    fn enter() -> Option<Self> {
        // SAFETY: plain termios calls on stdin with a zeroed, then filled, struct.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Self { saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `enter`.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
    }
}

/// Herdr reports; every failure is ignored, like a real integration does.
struct Reporter {
    herdr: Option<String>,
    pane: Option<String>,
    seq: u64,
}

impl Reporter {
    fn from_env() -> Self {
        let herdr = std::env::var("HERDR_BIN_PATH")
            .ok()
            .filter(|v| !v.is_empty());
        let pane = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|v| !v.is_empty());
        Self {
            herdr,
            pane,
            seq: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(1),
        }
    }

    fn call(&mut self, subcommand: &str, extra: &[&str]) {
        let (Some(herdr), Some(pane)) = (&self.herdr, &self.pane) else {
            return;
        };
        self.seq += 1;
        let _ = Command::new(herdr)
            .args([
                "pane", subcommand, pane, "--source", SOURCE, "--agent", AGENT,
            ])
            .args(extra)
            .arg("--seq")
            .arg(self.seq.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// Herdr refuses a resume command until the source holds the pane, so the
    /// first report carries the state only.
    fn report_idle(&mut self, sid: &str) {
        self.call(
            "report-agent",
            &["--state", "idle", "--agent-session-id", sid],
        );
    }

    fn report_session(&mut self, sid: &str) {
        self.call(
            "report-agent-session",
            &["--agent-session-id", sid, "--", AGENT, "--resume", sid],
        );
    }

    fn release(&mut self) {
        self.call("release-agent", &[]);
    }
}

fn session_id() -> String {
    let args: Vec<String> = std::env::args().collect();
    if let Some(at) = args.iter().position(|a| a == "--resume")
        && let Some(sid) = args.get(at + 1)
    {
        return sid.clone();
    }
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "00000000-0000-0000-0000-000000000000".to_string())
}

/// Saves the transcript a real Claude Code keeps, so that the session can be resumed.
fn save_conversation(sid: &str) {
    let config = std::env::var("CLAUDE_CONFIG_DIR").ok().or_else(|| {
        std::env::var("HOME")
            .ok()
            .map(|home| format!("{home}/.claude"))
    });
    if let Some(config) = config {
        let dir = format!("{config}/projects/e2e");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(format!("{dir}/{sid}.jsonl"), "{}\n");
    }
}

fn rule() -> String {
    "\u{2500}".repeat(RULE_WIDTH)
}

/// Draws the input box and leaves the cursor at the end of the prompt line.
fn draw_box(out: &mut impl Write, typed: &str) {
    let _ = write!(out, "{}\r\n\u{276F}\u{a0}{typed}\r\n{}", rule(), rule());
    let _ = write!(out, "\x1b[1A\r\x1b[2K\u{276F}\u{a0}{typed}");
    let _ = out.flush();
}

/// Redraws only the prompt line.
fn redraw(out: &mut impl Write, typed: &str) {
    let _ = write!(out, "\r\x1b[2K\u{276F}\u{a0}{typed}");
    let _ = out.flush();
}

/// Leaves the box: cursor below the bottom rule, at column 0.
fn leave_box(out: &mut impl Write) {
    let _ = write!(out, "\x1b[1B\r\n");
}

fn main() {
    let mut reporter = Reporter::from_env();
    let sid = session_id();
    save_conversation(&sid);
    reporter.report_idle(&sid);
    reporter.report_session(&sid);

    let _raw = RawMode::enter();
    let mut out = std::io::stdout();
    let _ = write!(out, "fake claude session {sid}\r\n");
    draw_box(&mut out, "");

    let mut typed = String::new();
    let mut input = std::io::stdin().lock();
    let mut pending: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    let mut escape = 0u8; // 0 none, 1 after ESC, 2 inside CSI
    loop {
        match input.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let b = byte[0];
        match escape {
            1 => {
                escape = if b == b'[' || b == b'O' { 2 } else { 0 };
                continue;
            }
            2 => {
                if (0x40..=0x7e).contains(&b) {
                    escape = 0;
                }
                continue;
            }
            _ => {}
        }
        match b {
            0x1b => escape = 1,
            0x03 => {
                typed.clear();
                redraw(&mut out, &typed);
            }
            0x04 if typed.is_empty() => {
                // Ctrl+D on an empty prompt does nothing, like a guarded REPL.
            }
            0x7f | 0x08 => {
                typed.pop();
                redraw(&mut out, &typed);
            }
            b'\r' | b'\n' => {
                let line = std::mem::take(&mut typed);
                leave_box(&mut out);
                let command = line.trim();
                if command == "/exit" || command == "/quit" {
                    let _ = write!(
                        out,
                        "Resume this session with:\r\nclaude --resume {sid}\r\n"
                    );
                    let _ = out.flush();
                    reporter.release();
                    return;
                }
                if !command.is_empty() {
                    let _ = write!(out, "got: {line}\r\n");
                }
                draw_box(&mut out, "");
            }
            0x20..=0x7e => {
                typed.push(b as char);
                redraw(&mut out, &typed);
            }
            0x80..=0xff => {
                pending.push(b);
                if let Ok(text) = std::str::from_utf8(&pending) {
                    typed.push_str(text);
                    pending.clear();
                    redraw(&mut out, &typed);
                } else if pending.len() >= 4 {
                    pending.clear();
                }
            }
            _ => {}
        }
    }
}

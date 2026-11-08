//! Declarations made to Herdr: the Sleeping agent identity, the Sleep marker
//! and their removal at hand-over.
//!
//! Herdr is reached through its CLI only and only exit codes are looked at.
//! A failed declaration is logged and ignored: the pane must stay wakeable.

use crate::args::Args;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Source under which every declaration is made.
pub const SOURCE: &str = "pepsit36.herdr-sleep";

const RENAME_TRIES: u32 = 15;
const RENAME_DELAY: Duration = Duration::from_millis(200);
/// Poll interval while waiting for Herdr to clear the name.
const NAME_POLL: Duration = Duration::from_millis(50);
/// Longest wait for Herdr to clear the name. It happens a fraction of a second
/// after the Sleeping agent appears; when it does not happen the name stays.
const NAME_WINDOW: Duration = Duration::from_millis(1500);

/// Strictly increasing sequence numbers, starting at the epoch milliseconds.
#[derive(Debug)]
pub struct Seq(u64);

impl Seq {
    pub fn starting_at_now() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Seq(ms)
    }

    pub fn next(&mut self) -> u64 {
        let n = self.0;
        self.0 += 1;
        n
    }
}

/// `pane <sub> <pane> --source S --seq N`. Herdr wants the pane before the
/// options: with the pane last it rejects the value of the first option
/// ("unknown option") on `report-metadata` and `release-agent`.
fn base(sub: &str, args: &Args, seq: u64) -> Vec<String> {
    ["pane", sub, &args.pane, "--source", SOURCE]
        .iter()
        .map(|s| s.to_string())
        .chain(["--seq".to_string(), seq.to_string()])
        .collect()
}

fn agent_name(args: &Args) -> String {
    format!("sleeping-{}", args.kind)
}

fn push(v: &mut Vec<String>, items: &[&str]) {
    v.extend(items.iter().map(|s| s.to_string()));
}

pub fn report_agent(args: &Args, seq: u64) -> Vec<String> {
    let mut v = base("report-agent", args, seq);
    push(&mut v, &["--agent", &agent_name(args)]);
    push(&mut v, &["--state", "idle", "--message", "sleeping"]);
    push(&mut v, &["--agent-session-id", &args.session]);
    v
}

pub fn report_agent_session(args: &Args, seq: u64) -> Vec<String> {
    let mut v = base("report-agent-session", args, seq);
    push(&mut v, &["--agent", &agent_name(args)]);
    push(&mut v, &["--agent-session-id", &args.session]);
    v
}

pub fn release_agent(args: &Args, seq: u64) -> Vec<String> {
    let mut v = base("release-agent", args, seq);
    push(&mut v, &["--agent", &agent_name(args)]);
    v
}

pub fn set_token(args: &Args, seq: u64) -> Vec<String> {
    let mut v = base("report-metadata", args, seq);
    push(
        &mut v,
        &["--token", &format!("{}={}", args.token, args.icon)],
    );
    v
}

pub fn clear_token(args: &Args, seq: u64) -> Vec<String> {
    let mut v = base("report-metadata", args, seq);
    push(&mut v, &["--clear-token", &args.token]);
    v
}

/// `agent get <pane>`.
pub fn get_agent(args: &Args) -> Vec<String> {
    ["agent", "get", &args.pane]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Whether the JSON Herdr printed for an agent carries a `name`. Herdr leaves
/// the key out when the agent has none; `display_agent` is a different key.
pub fn has_name(json: &str) -> bool {
    json.contains("\"name\":\"")
}

pub fn rename(args: &Args, name: &str) -> Vec<String> {
    ["agent", "rename", &args.pane, name]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Runs the declarations against the `herdr` binary.
pub struct Reporter<'a> {
    args: &'a Args,
    seq: Seq,
}

impl<'a> Reporter<'a> {
    pub fn new(args: &'a Args) -> Self {
        Reporter {
            args,
            seq: Seq::starting_at_now(),
        }
    }

    fn run(&self, argv: &[String]) -> bool {
        let status = Command::new(&self.args.herdr_bin)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => true,
            Ok(s) => {
                eprintln!(
                    "herdr-sleep-ph: `herdr {}` failed: {s}",
                    argv[..2].join(" ")
                );
                false
            }
            Err(e) => {
                eprintln!("herdr-sleep-ph: cannot run {}: {e}", self.args.herdr_bin);
                false
            }
        }
    }

    /// Standard output of `herdr <argv>`, `None` when it failed.
    fn output(&self, argv: &[String]) -> Option<String> {
        let out = Command::new(&self.args.herdr_bin)
            .args(argv)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The Sleeping agent identity: state, session and name.
    fn identity(&mut self) {
        let seq = self.seq.next();
        self.run(&report_agent(self.args, seq));
        let seq = self.seq.next();
        self.run(&report_agent_session(self.args, seq));
        self.rename();
    }

    /// Names the agent. Herdr needs a moment to register it first, so the
    /// call is retried.
    fn rename(&mut self) {
        let Some(name) = &self.args.name else {
            return;
        };
        let rename = rename(self.args, name);
        for attempt in 0..RENAME_TRIES {
            if attempt > 0 {
                sleep(RENAME_DELAY);
            }
            if self.run(&rename) {
                break;
            }
        }
    }

    /// Herdr's own process detection clears the name of the pane a fraction
    /// of a second after the Sleeping agent appears. Wait until the name is
    /// gone and give it again; no wait at all for a pane without a name.
    fn keep_name(&mut self) {
        if self.args.name.is_none() {
            return;
        }
        let deadline = Instant::now() + NAME_WINDOW;
        while Instant::now() < deadline {
            sleep(NAME_POLL);
            if self
                .output(&get_agent(self.args))
                .is_some_and(|json| !has_name(&json))
            {
                self.rename();
                return;
            }
        }
    }

    /// Announces the Sleeping agent and the Sleep marker.
    pub fn declare(&mut self) {
        self.identity();
        let seq = self.seq.next();
        self.run(&set_token(self.args, seq));
        self.keep_name();
    }

    /// Removes the Sleep marker and the Sleeping agent identity.
    pub fn release(&mut self) {
        let seq = self.seq.next();
        self.run(&clear_token(self.args, seq));
        let seq = self.seq.next();
        self.run(&release_agent(self.args, seq));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::sample;

    #[test]
    fn report_agent_declares_identity_without_a_resume_command() {
        let v = report_agent(&sample(), 7);
        assert_eq!(
            v.join(" "),
            "pane report-agent w1-2 --source pepsit36.herdr-sleep --seq 7 --agent sleeping-claude \
             --state idle --message sleeping --agent-session-id sess-1"
        );
    }

    #[test]
    fn session_release_and_tokens() {
        let a = sample();
        assert_eq!(
            report_agent_session(&a, 8).join(" "),
            "pane report-agent-session w1-2 --source pepsit36.herdr-sleep --seq 8 \
             --agent sleeping-claude --agent-session-id sess-1"
        );
        assert_eq!(
            release_agent(&a, 9).join(" "),
            "pane release-agent w1-2 --source pepsit36.herdr-sleep --seq 9 \
             --agent sleeping-claude"
        );
        assert_eq!(
            set_token(&a, 1).join(" "),
            "pane report-metadata w1-2 --source pepsit36.herdr-sleep --seq 1 --token sleep=z"
        );
        assert_eq!(
            clear_token(&a, 2).join(" "),
            "pane report-metadata w1-2 --source pepsit36.herdr-sleep --seq 2 --clear-token sleep"
        );
        assert_eq!(rename(&a, "worker").join(" "), "agent rename w1-2 worker");
    }

    #[test]
    fn a_name_is_told_from_a_display_name() {
        assert!(has_name(
            r#"{"agent":"sleeping-claude","name":"alpha","pane_id":"p"}"#
        ));
        assert!(!has_name(
            r#"{"agent":"sleeping-claude","display_agent":"alpha"}"#
        ));
        assert!(!has_name(r#"{"agent":"sleeping-claude","pane_id":"p"}"#));
        assert_eq!(get_agent(&sample()).join(" "), "agent get w1-2");
    }

    #[test]
    fn seq_is_strictly_increasing_from_now() {
        let mut seq = Seq::starting_at_now();
        let a = seq.next();
        let b = seq.next();
        assert_eq!(b, a + 1);
        assert!(a > 1_700_000_000_000);
    }
}

//! Declarations made to Herdr: the Sleeping agent identity, the Sleep marker
//! and their removal at hand-over.
//!
//! Herdr is reached through its CLI only and only exit codes are looked at.
//! A failed declaration is logged and ignored: the pane must stay wakeable.

use crate::args::Args;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Source under which every declaration is made.
pub const SOURCE: &str = "pepsit36.herdr-sleep";

const RENAME_TRIES: u32 = 15;
const RENAME_DELAY: Duration = Duration::from_millis(200);

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

/// The Placeholder's own command line, with the program replaced by the
/// absolute path of the running executable, so that Herdr relaunches exactly
/// this command after a restart.
pub fn own_argv(argv: &[String], exe: Option<PathBuf>) -> Vec<String> {
    let mut own = argv.to_vec();
    if let (Some(first), Some(exe)) = (own.first_mut(), exe) {
        *first = exe.to_string_lossy().into_owned();
    }
    own
}

fn base(sub: &str, seq: u64) -> Vec<String> {
    ["pane", sub, "--source", SOURCE]
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

pub fn report_agent(args: &Args, seq: u64, own: &[String]) -> Vec<String> {
    let mut v = base("report-agent", seq);
    push(&mut v, &["--agent", &agent_name(args)]);
    push(&mut v, &["--state", "idle", "--message", "sleeping"]);
    push(
        &mut v,
        &["--agent-session-id", &args.session, &args.pane, "--"],
    );
    v.extend(own.iter().cloned());
    v
}

pub fn report_agent_session(args: &Args, seq: u64, own: &[String]) -> Vec<String> {
    let mut v = base("report-agent-session", seq);
    push(&mut v, &["--agent", &agent_name(args)]);
    push(
        &mut v,
        &["--agent-session-id", &args.session, &args.pane, "--"],
    );
    v.extend(own.iter().cloned());
    v
}

pub fn release_agent(args: &Args, seq: u64) -> Vec<String> {
    let mut v = base("release-agent", seq);
    push(&mut v, &["--agent", &agent_name(args), &args.pane]);
    v
}

pub fn set_token(args: &Args, seq: u64) -> Vec<String> {
    let mut v = base("report-metadata", seq);
    push(
        &mut v,
        &["--token", &format!("{}={}", args.token, args.icon)],
    );
    v.push(args.pane.clone());
    v
}

pub fn clear_token(args: &Args, seq: u64) -> Vec<String> {
    let mut v = base("report-metadata", seq);
    push(&mut v, &["--clear-token", &args.token]);
    v.push(args.pane.clone());
    v
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

    /// Announces the Sleeping agent and the Sleep marker.
    pub fn declare(&mut self, own: &[String]) {
        let seq = self.seq.next();
        self.run(&report_agent(self.args, seq, own));
        let seq = self.seq.next();
        self.run(&report_agent_session(self.args, seq, own));
        if let Some(name) = &self.args.name {
            // Herdr needs a moment to register the agent before a rename.
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
        let seq = self.seq.next();
        self.run(&set_token(self.args, seq));
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

    fn own() -> Vec<String> {
        vec!["/bin/herdr-sleep-ph".into(), "--pane".into(), "w1-2".into()]
    }

    #[test]
    fn report_agent_declares_identity_and_own_command() {
        let v = report_agent(&sample(), 7, &own());
        assert_eq!(
            v.join(" "),
            "pane report-agent --source pepsit36.herdr-sleep --seq 7 --agent sleeping-claude \
             --state idle --message sleeping --agent-session-id sess-1 w1-2 -- \
             /bin/herdr-sleep-ph --pane w1-2"
        );
    }

    #[test]
    fn session_release_and_tokens() {
        let a = sample();
        assert_eq!(
            report_agent_session(&a, 8, &own()).join(" "),
            "pane report-agent-session --source pepsit36.herdr-sleep --seq 8 \
             --agent sleeping-claude --agent-session-id sess-1 w1-2 -- \
             /bin/herdr-sleep-ph --pane w1-2"
        );
        assert_eq!(
            release_agent(&a, 9).join(" "),
            "pane release-agent --source pepsit36.herdr-sleep --seq 9 \
             --agent sleeping-claude w1-2"
        );
        assert_eq!(
            set_token(&a, 1).join(" "),
            "pane report-metadata --source pepsit36.herdr-sleep --seq 1 --token sleep=z w1-2"
        );
        assert_eq!(
            clear_token(&a, 2).join(" "),
            "pane report-metadata --source pepsit36.herdr-sleep --seq 2 --clear-token sleep w1-2"
        );
        assert_eq!(rename(&a, "worker").join(" "), "agent rename w1-2 worker");
    }

    #[test]
    fn seq_is_strictly_increasing_from_now() {
        let mut seq = Seq::starting_at_now();
        let a = seq.next();
        let b = seq.next();
        assert_eq!(b, a + 1);
        assert!(a > 1_700_000_000_000);
    }

    #[test]
    fn own_argv_uses_the_absolute_executable() {
        let argv = vec!["herdr-sleep-ph".to_string(), "--pane".into(), "p".into()];
        let own = own_argv(&argv, Some(PathBuf::from("/opt/herdr-sleep-ph")));
        assert_eq!(own, vec!["/opt/herdr-sleep-ph", "--pane", "p"]);
        assert_eq!(own_argv(&argv, None), argv);
    }
}

//! The Restore operation: after a Herdr restart, put the Placeholder back in
//! every pane that went to sleep before it (ADR 0008).
//!
//! Herdr brings a restored pane back as a plain shell. The registry of
//! sleeping panes says which of them held a Placeholder; for each one that
//! still exists, runs no agent and sits at its shell prompt, the Placeholder
//! is started the way Sleep starts it. Panes are restored one after the
//! other. Nothing here fails loudly: a pane that does not qualify is skipped
//! and the reason goes to the journal.
//!
//! Journal events: `RESTORE <pane>`, `RESTORE-SKIP <pane> <reason>`,
//! `RESTORE-DONE restored=N skipped=M`, `WARN`.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::config::Config;
use crate::journal::Journal;

use super::Env;
use super::markers::sync_markers;
use super::registry::{Registry, Sleeper};
use super::sleep::{PlaceholderParams, is_typeable, placeholder_argv, shell_quote, wait_until};
use super::wake::sanitize_pane;

/// Poll interval while waiting for a pane to be ready for its Placeholder.
const SETTLE_POLL: Duration = Duration::from_millis(250);
/// Longest wait for a pane to sit at its shell prompt with the native
/// identity released.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Poll interval while waiting for the Placeholder to report.
const SLEEPING_POLL: Duration = Duration::from_millis(300);
/// Longest wait for the Placeholder to report.
const SLEEPING_TIMEOUT: Duration = Duration::from_secs(10);

/// What one restore pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RestoreReport {
    /// Panes that hold a Placeholder again, in the order they were restored.
    pub restored: Vec<String>,
    /// Panes left alone, with the reason.
    pub skipped: Vec<(String, String)>,
    /// The pass could not run (no state directory, Herdr unreachable).
    pub error: Option<String>,
}

/// What a pane looked like the last time it was examined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneState {
    /// At its shell prompt with no agent: ready for the Placeholder.
    Ready,
    /// The foreground process is not the shell, and Herdr tracks an agent.
    AgentAlive,
    /// The foreground process is not the shell.
    Busy,
    /// At its shell prompt, but Herdr still reports an agent.
    IdentityHeld,
}

impl PaneState {
    fn reason(self) -> &'static str {
        match self {
            PaneState::Ready => "ready",
            PaneState::AgentAlive => "agent-alive",
            PaneState::Busy => "pane-busy",
            PaneState::IdentityHeld => "identity-not-released",
        }
    }
}

fn pane_state(env: &dyn Env, pane: &str) -> PaneState {
    let at_shell = env.process_info(pane).is_ok_and(|info| info.at_shell());
    let held = env
        .pane(pane)
        .map_or(true, |p| p.has_agent() || p.agent_session.is_some());
    match (at_shell, held) {
        (true, false) => PaneState::Ready,
        (true, true) => PaneState::IdentityHeld,
        (false, true) => PaneState::AgentAlive,
        (false, false) => PaneState::Busy,
    }
}

/// Bring the Placeholder back in every registered pane that qualifies.
pub fn restore(env: &dyn Env, config: &Config) -> RestoreReport {
    let mut report = RestoreReport::default();
    let Some(state_dir) = env.state_dir() else {
        report.error = Some("no state directory".into());
        return report;
    };
    let registry = Registry::load(&state_dir);
    if registry.panes.is_empty() {
        return report;
    }
    let journal = Journal::new(&state_dir);
    let snapshot = match env.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let reason = format!("cannot read the session: {error}");
            journal.log("WARN", &format!("restore: {reason}"));
            report.error = Some(reason);
            return report;
        }
    };

    for (pane, sleeper) in &registry.panes {
        let outcome = if snapshot.panes.iter().any(|p| &p.pane_id == pane) {
            restore_pane(env, config, &journal, &state_dir, pane, sleeper)
        } else {
            Err("pane-gone".to_string())
        };
        match outcome {
            Ok(()) => {
                journal.log("RESTORE", pane);
                report.restored.push(pane.clone());
            }
            Err(reason) => {
                journal.log("RESTORE-SKIP", &format!("{pane} {reason}"));
                report.skipped.push((pane.clone(), reason));
            }
        }
    }
    journal.log(
        "RESTORE-DONE",
        &format!(
            "restored={} skipped={}",
            report.restored.len(),
            report.skipped.len()
        ),
    );
    if !report.restored.is_empty() {
        sync_markers(env, config);
    }
    report
}

/// Restore one pane; the reason when it was left alone.
fn restore_pane(
    env: &dyn Env,
    config: &Config,
    journal: &Journal,
    state_dir: &Path,
    pane: &str,
    sleeper: &Sleeper,
) -> Result<(), String> {
    let kind = sleeper.kind.as_str();
    let profile = config
        .profile(kind)
        .ok_or_else(|| format!("no profile for kind {kind}"))?;
    let placeholder = env
        .placeholder_bin()
        .map_err(|error| format!("placeholder unavailable: {error}"))?;

    // The pane's shell may still be starting (Herdr starts it when a client
    // attaches) and Herdr releases the native identity a moment after the
    // process is gone.
    let mut state = PaneState::Busy;
    let settled = wait_until(env, SETTLE_TIMEOUT, SETTLE_POLL, |_| {
        state = pane_state(env, pane);
        state == PaneState::Ready
    });
    if !settled {
        return Err(state.reason().to_string());
    }

    let shell = env
        .process_info(pane)
        .ok()
        .and_then(|info| env.shell_path(info.shell_pid))
        .or_else(|| {
            std::env::var_os("SHELL")
                .filter(|s| !s.is_empty())
                .map(Into::into)
        })
        .unwrap_or_else(|| "/bin/sh".into());
    let main_bin = env.main_bin();
    let herdr_bin = env.herdr_bin();
    let preview = preview_path(state_dir, pane);
    let argv = placeholder_argv(
        &placeholder,
        &PlaceholderParams {
            pane,
            name: sleeper.name.as_deref(),
            kind,
            session: &sleeper.session,
            state_dir,
            main_bin: &main_bin,
            herdr_bin: &herdr_bin,
            shell: &shell,
            icon: &config.sleep_icon,
            token: &config.token_name,
            cwd: &sleeper.cwd,
            preview: preview.as_deref(),
            resume: &profile.resume_argv(&sleeper.session),
        },
    );
    // `pane run` types the words into the pane's shell: the same rules as
    // Sleep apply.
    if let Some(word) = argv.iter().find(|word| !is_typeable(word)) {
        return Err(format!(
            "cannot type {word:?} safely into a shell (control character or backslash)"
        ));
    }
    let quoted: Vec<String> = argv.iter().map(|word| shell_quote(word)).collect();
    let mut command: Vec<&str> = vec!["exec"];
    command.extend(quoted.iter().map(String::as_str));
    env.run(pane, &command)
        .map_err(|error| format!("cannot start the placeholder: {error}"))?;

    let announced = wait_until(env, SLEEPING_TIMEOUT, SLEEPING_POLL, |_| {
        env.agent(pane)
            .is_ok_and(|a| a.is_sleeping() && a.kind() == Some(kind))
    });
    if !announced {
        journal.log(
            "WARN",
            &format!("{pane}: placeholder did not report sleeping-{kind}"),
        );
        return Err(format!("placeholder did not report sleeping-{kind}"));
    }
    Ok(())
}

/// The Preview file Sleep left for `pane`, when it still exists.
fn preview_path(state_dir: &Path, pane: &str) -> Option<String> {
    let path = state_dir.join(format!("preview-{}.txt", sanitize_pane(pane)));
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::{Refusal, SleepMode};
    use crate::herdr::{Agent, AgentSession, AgentStatus, Pane, ProcessInfo, Snapshot};
    use crate::ops::EnvResult;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    /// How one pane of the restarted session behaves, on the virtual clock.
    #[derive(Clone)]
    struct Spec {
        /// The shell is the foreground process from this moment on; `None`
        /// when something else runs forever.
        shell_at: Option<Duration>,
        /// Herdr still reports an agent until this moment; `None` forever.
        released_at: Option<Duration>,
        /// Delay from `exec` to the Placeholder's report; `None` never.
        announce_after: Option<Duration>,
        /// Whether `herdr pane run` fails.
        run_fails: bool,
    }

    impl Spec {
        fn ready() -> Self {
            Spec {
                shell_at: Some(Duration::ZERO),
                released_at: Some(Duration::ZERO),
                announce_after: Some(Duration::from_millis(600)),
                run_fails: false,
            }
        }
    }

    struct Fake {
        base: Instant,
        elapsed: Cell<Duration>,
        calls: RefCell<Vec<String>>,
        state_dir: PathBuf,
        panes: BTreeMap<String, Spec>,
        exec_at: RefCell<BTreeMap<String, Duration>>,
        placeholder: bool,
        snapshot_error: bool,
    }

    impl Fake {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-sleep-restore-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Fake {
                base: Instant::now(),
                elapsed: Cell::new(Duration::ZERO),
                calls: RefCell::new(Vec::new()),
                state_dir: dir,
                panes: BTreeMap::new(),
                exec_at: RefCell::new(BTreeMap::new()),
                placeholder: true,
                snapshot_error: false,
            }
        }

        /// A registered pane that exists in the session.
        fn pane(mut self, pane: &str, spec: Spec) -> Self {
            self.register(pane, "claude", "sess-1", Some("worker"), "/work");
            self.panes.insert(pane.to_string(), spec);
            self
        }

        fn register(&self, pane: &str, kind: &str, session: &str, name: Option<&str>, cwd: &str) {
            Registry::add(
                &self.state_dir,
                pane,
                Sleeper {
                    kind: kind.into(),
                    session: session.into(),
                    name: name.map(Into::into),
                    cwd: cwd.into(),
                    since_ms: 1,
                },
            )
            .unwrap();
        }

        fn clock(&self) -> Duration {
            self.elapsed.get()
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn runs(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| c.starts_with("run "))
                .collect()
        }

        fn journal(&self) -> Vec<String> {
            std::fs::read_to_string(self.state_dir.join(crate::journal::FILE_NAME))
                .unwrap_or_default()
                .lines()
                .map(|line| line.split_once(' ').unwrap().1.to_string())
                .collect()
        }

        fn released(&self, spec: &Spec) -> bool {
            spec.released_at.is_some_and(|at| self.clock() >= at)
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }

    impl Env for Fake {
        fn snapshot(&self) -> EnvResult<Snapshot> {
            if self.snapshot_error {
                return Err("socket gone".into());
            }
            Ok(Snapshot {
                panes: self
                    .panes
                    .keys()
                    .map(|pane| Pane {
                        pane_id: pane.clone(),
                        ..Pane::default()
                    })
                    .collect(),
                ..Snapshot::default()
            })
        }

        fn pane(&self, pane: &str) -> EnvResult<Pane> {
            let spec = &self.panes[pane];
            let held = !self.released(spec);
            Ok(Pane {
                pane_id: pane.into(),
                agent: held.then(|| "claude".to_string()),
                agent_session: held.then(|| AgentSession {
                    source: "herdr:claude".into(),
                    agent: None,
                    kind: Some("id".into()),
                    value: "sess-1".into(),
                }),
                ..Pane::default()
            })
        }

        fn agent(&self, target: &str) -> EnvResult<Agent> {
            self.log(format!("agent {target}"));
            let spec = &self.panes[target];
            let announced = match (self.exec_at.borrow().get(target), spec.announce_after) {
                (Some(at), Some(after)) => self.clock() >= *at + after,
                _ => false,
            };
            Ok(Agent {
                pane_id: target.into(),
                agent: announced.then(|| "sleeping-claude".to_string()),
                agent_status: AgentStatus::Idle,
                ..Agent::default()
            })
        }

        fn process_info(&self, pane: &str) -> EnvResult<ProcessInfo> {
            let spec = &self.panes[pane];
            let at_shell = spec.shell_at.is_some_and(|at| self.clock() >= at);
            Ok(ProcessInfo {
                pane_id: pane.into(),
                shell_pid: 100,
                foreground_process_group_id: if at_shell { 100 } else { 200 },
                foreground_processes: Vec::new(),
            })
        }

        fn screen_text(&self, _: &str, _: usize) -> EnvResult<String> {
            Ok(String::new())
        }

        fn screen_ansi(&self, _: &str, _: usize) -> EnvResult<String> {
            Ok(String::new())
        }

        fn rename_agent(&self, _: &str, _: &str) -> EnvResult<()> {
            Ok(())
        }

        fn send_text(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!("restore never types into an agent")
        }

        fn send_keys(&self, _: &str, _: &[&str]) -> EnvResult<()> {
            unreachable!("restore never presses keys")
        }

        fn run(&self, pane: &str, argv: &[&str]) -> EnvResult<()> {
            self.log(format!("run {pane} {}", argv.join(" ")));
            if self.panes[pane].run_fails {
                return Err("pane gone".into());
            }
            self.exec_at.borrow_mut().insert(pane.into(), self.clock());
            Ok(())
        }

        fn notify(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!("restore does not notify")
        }

        fn tab_rename(&self, _: &str, _: &str) -> EnvResult<()> {
            Ok(())
        }

        fn workspace_report_metadata(
            &self,
            _: &str,
            _: &str,
            _: &crate::herdr::MetadataChange,
            _: u64,
        ) -> EnvResult<()> {
            Ok(())
        }

        fn check_guards(&self, _: &Config, _: &Agent, _: SleepMode) -> Result<(), Refusal> {
            unreachable!("restore does not run guards")
        }

        fn agent_rss_kb(&self, _: &Agent, _: &str) -> Option<u64> {
            None
        }

        fn shell_path(&self, pid: i32) -> Option<PathBuf> {
            self.log(format!("shell-path {pid}"));
            Some("/bin/zsh".into())
        }

        fn state_dir(&self) -> Option<PathBuf> {
            Some(self.state_dir.clone())
        }

        fn placeholder_bin(&self) -> EnvResult<PathBuf> {
            if self.placeholder {
                Ok("/opt/bin/herdr-sleep-ph".into())
            } else {
                Err("missing".into())
            }
        }

        fn main_bin(&self) -> PathBuf {
            "/opt/bin/herdr-sleep".into()
        }

        fn herdr_bin(&self) -> PathBuf {
            "/usr/bin/herdr".into()
        }

        fn sleep(&self, duration: Duration) {
            self.elapsed.set(self.elapsed.get() + duration);
        }

        fn now(&self) -> Instant {
            self.base + self.elapsed.get()
        }

        fn now_millis(&self) -> u64 {
            1_000_000_000_000 + u64::try_from(self.clock().as_millis()).unwrap()
        }
    }

    impl Fake {
        fn log(&self, call: impl Into<String>) {
            self.calls.borrow_mut().push(call.into());
        }
    }

    fn run(fake: &Fake) -> RestoreReport {
        restore(fake, &Config::default())
    }

    #[test]
    fn restores_every_qualifying_pane_in_order() {
        let fake = Fake::new("order")
            .pane("w1-3", Spec::ready())
            .pane("w1-2", Spec::ready());
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2", "w1-3"]);
        assert!(report.skipped.is_empty());
        let runs = fake.runs();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].starts_with("run w1-2 exec /opt/bin/herdr-sleep-ph --pane w1-2 "));
        assert!(runs[1].starts_with("run w1-3 exec "));
        assert_eq!(
            fake.journal(),
            [
                "RESTORE w1-2",
                "RESTORE w1-3",
                "RESTORE-DONE restored=2 skipped=0"
            ]
        );
    }

    #[test]
    fn the_command_is_the_one_sleep_types() {
        let fake = Fake::new("command").pane("w1-2", Spec::ready());
        run(&fake);
        let command = &fake.runs()[0];
        assert!(
            command.starts_with(
                "run w1-2 exec /opt/bin/herdr-sleep-ph --pane w1-2 --name worker --kind claude \
                 --session sess-1 --state-dir "
            ),
            "{command}"
        );
        assert!(command.contains(
            " --main-bin /opt/bin/herdr-sleep --herdr-bin /usr/bin/herdr --shell /bin/zsh "
        ));
        assert!(command.contains(" --cwd /work"));
        assert!(command.ends_with(" -- claude --resume sess-1"), "{command}");
    }

    #[test]
    fn restores_are_sequential() {
        let fake = Fake::new("sequential")
            .pane("w1-2", Spec::ready())
            .pane("w1-3", Spec::ready());
        run(&fake);
        let calls = fake.calls();
        let second_run = calls
            .iter()
            .position(|c| c.starts_with("run w1-3"))
            .unwrap();
        let first_report = calls
            .iter()
            .rposition(|c| c == "agent w1-2")
            .expect("the first pane is awaited");
        assert!(first_report < second_run, "{calls:?}");
    }

    #[test]
    fn waits_for_the_identity_to_be_released() {
        let spec = Spec {
            released_at: Some(Duration::from_secs(3)),
            ..Spec::ready()
        };
        let fake = Fake::new("release").pane("w1-2", spec);
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2"]);
        let at = fake.exec_at.borrow()["w1-2"];
        assert!(at >= Duration::from_secs(3), "{at:?}");
    }

    #[test]
    fn waits_for_the_shell_to_start() {
        let spec = Spec {
            shell_at: Some(Duration::from_secs(8)),
            released_at: Some(Duration::from_secs(8)),
            ..Spec::ready()
        };
        let fake = Fake::new("shell").pane("w1-2", spec);
        assert_eq!(run(&fake).restored, ["w1-2"]);
    }

    #[test]
    fn skips_a_pane_whose_identity_is_never_released() {
        let spec = Spec {
            released_at: None,
            ..Spec::ready()
        };
        let fake = Fake::new("held").pane("w1-2", spec);
        let report = run(&fake);
        assert!(report.restored.is_empty());
        assert_eq!(
            report.skipped,
            [("w1-2".to_string(), "identity-not-released".to_string())]
        );
        assert!(fake.runs().is_empty());
        assert!(fake.clock() >= SETTLE_TIMEOUT);
        assert_eq!(
            fake.journal(),
            [
                "RESTORE-SKIP w1-2 identity-not-released",
                "RESTORE-DONE restored=0 skipped=1"
            ]
        );
    }

    #[test]
    fn skips_a_pane_whose_agent_is_alive() {
        let spec = Spec {
            shell_at: None,
            released_at: None,
            ..Spec::ready()
        };
        let fake = Fake::new("alive").pane("w1-2", spec);
        let report = run(&fake);
        assert_eq!(
            report.skipped,
            [("w1-2".to_string(), "agent-alive".to_string())]
        );
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn skips_a_busy_pane() {
        let spec = Spec {
            shell_at: None,
            ..Spec::ready()
        };
        let fake = Fake::new("busy").pane("w1-2", spec);
        let report = run(&fake);
        assert_eq!(
            report.skipped,
            [("w1-2".to_string(), "pane-busy".to_string())]
        );
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn a_skipped_pane_does_not_stop_the_next_one() {
        let busy = Spec {
            shell_at: None,
            ..Spec::ready()
        };
        let fake = Fake::new("continue")
            .pane("w1-2", busy)
            .pane("w1-3", Spec::ready());
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-3"]);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            fake.journal().last().unwrap(),
            "RESTORE-DONE restored=1 skipped=1"
        );
    }

    #[test]
    fn skips_a_pane_that_no_longer_exists() {
        let fake = Fake::new("gone").pane("w1-2", Spec::ready());
        fake.register("w9-9", "claude", "s", None, "/");
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2"]);
        assert_eq!(
            report.skipped,
            [("w9-9".to_string(), "pane-gone".to_string())]
        );
        // The entry is left for the Sweep to prune.
        assert!(Registry::load(&fake.state_dir).panes.contains_key("w9-9"));
    }

    #[test]
    fn skips_values_a_shell_could_misread() {
        let fake = Fake::new("untypeable").pane("w1-2", Spec::ready());
        fake.register("w1-2", "claude", "sess-1", Some("bad\nname"), "/work");
        let report = run(&fake);
        assert!(report.restored.is_empty());
        assert!(report.skipped[0].1.starts_with("cannot type"), "{report:?}");
        assert!(fake.runs().is_empty());

        fake.register("w1-2", "claude", "sess-1", Some("ok"), "/back\\slash");
        let report = run(&fake);
        assert!(report.skipped[0].1.starts_with("cannot type"), "{report:?}");
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn skips_a_kind_without_profile() {
        let fake = Fake::new("profile").pane("w1-2", Spec::ready());
        fake.register("w1-2", "mystery", "s", None, "/");
        let report = run(&fake);
        assert_eq!(
            report.skipped,
            [(
                "w1-2".to_string(),
                "no profile for kind mystery".to_string()
            )]
        );
    }

    #[test]
    fn skips_everything_when_the_placeholder_is_missing() {
        let mut fake = Fake::new("noph")
            .pane("w1-2", Spec::ready())
            .pane("w1-3", Spec::ready());
        fake.placeholder = false;
        let report = run(&fake);
        assert!(report.restored.is_empty());
        assert_eq!(report.skipped.len(), 2);
        assert!(report.skipped[0].1.starts_with("placeholder unavailable"));
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn skips_a_placeholder_that_never_reports() {
        let spec = Spec {
            announce_after: None,
            ..Spec::ready()
        };
        let fake = Fake::new("silent").pane("w1-2", spec);
        let report = run(&fake);
        assert_eq!(
            report.skipped,
            [(
                "w1-2".to_string(),
                "placeholder did not report sleeping-claude".to_string()
            )]
        );
        assert!(fake.journal().iter().any(|l| l.starts_with("WARN ")));
    }

    #[test]
    fn skips_a_pane_the_command_could_not_reach() {
        let spec = Spec {
            run_fails: true,
            ..Spec::ready()
        };
        let fake = Fake::new("runfail").pane("w1-2", spec);
        let report = run(&fake);
        assert!(
            report.skipped[0]
                .1
                .starts_with("cannot start the placeholder")
        );
    }

    #[test]
    fn an_empty_registry_does_nothing_and_logs_nothing() {
        let fake = Fake::new("empty");
        assert_eq!(run(&fake), RestoreReport::default());
        assert!(fake.journal().is_empty());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn an_unreachable_herdr_is_reported_and_touches_nothing() {
        let mut fake = Fake::new("socket").pane("w1-2", Spec::ready());
        fake.snapshot_error = true;
        let report = run(&fake);
        assert_eq!(
            report.error.as_deref(),
            Some("cannot read the session: socket gone")
        );
        assert!(fake.runs().is_empty());
        assert!(Registry::load(&fake.state_dir).panes.contains_key("w1-2"));
    }

    #[test]
    fn restored_panes_stay_registered() {
        let fake = Fake::new("keep").pane("w1-2", Spec::ready());
        run(&fake);
        assert!(Registry::load(&fake.state_dir).panes.contains_key("w1-2"));
    }

    #[test]
    fn the_preview_left_by_sleep_is_shown_again() {
        let fake = Fake::new("preview").pane("w1-2", Spec::ready());
        let file = fake.state_dir.join("preview-w1-2.txt");
        std::fs::write(&file, "last words").unwrap();
        run(&fake);
        assert!(
            fake.runs()[0].contains(&format!(" --preview {}", file.display())),
            "{}",
            fake.runs()[0]
        );
    }

    #[test]
    fn the_report_serialises_for_json() {
        let report = RestoreReport {
            restored: vec!["a".into()],
            skipped: vec![("b".into(), "pane-busy".into())],
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({"restored": ["a"], "skipped": [["b", "pane-busy"]], "error": null})
        );
    }
}

//! The Wake operation: press Enter in the Placeholder, which launches the
//! Agent again through the user's shell, and optionally wait until the Agent
//! is Ready.
//!
//! Herdr emits `tab.focused` and `pane.focused` together, so a Wake of a pane
//! is ignored when the previous one is less than [`DEDUP_WINDOW`] old. The
//! time of the last Wake lives in `<state_dir>/wake-<pane>.ts` as epoch
//! milliseconds.
//!
//! Journal events: `WAKE`, `READY <ms>`, `WAKE-TIMEOUT`, `WARN`.
//!
//! A Ready Agent is removed from the registry of sleeping panes.

use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::guards;
use crate::journal::Journal;

use super::Env;
use super::markers::sync_markers;
use super::registry::{self, Registry};
use super::sleep::wait_until;

/// Wakes of one pane closer together than this are one Wake.
pub const DEDUP_WINDOW: Duration = Duration::from_secs(15);
/// Poll interval while waiting for the Agent to be Ready.
pub const READY_POLL: Duration = Duration::from_millis(250);
/// Longest wait for the Agent to be Ready.
pub const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// How a Wake is carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WakeOptions {
    /// Wait until the Agent is Ready. Also bypasses the de-duplication, since
    /// the caller asked for this Wake explicitly.
    pub wait: bool,
}

/// How a Wake attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    /// Enter was pressed. `ms` is the time until the Agent was Ready, `0`
    /// when the caller did not wait.
    Woken { ms: u64 },
    /// No agent matches the target.
    NotFound,
    /// The pane does not hold a Placeholder.
    NotSleeping,
    /// A Wake of the same pane happened less than 15 s ago.
    Deduplicated,
    /// The Agent was not Ready after 120 s.
    TimedOut,
    /// The Wake could not be carried out.
    Failed(String),
}

/// Wake the Sleeping agent `target` (pane id or agent name).
pub fn wake_agent(
    env: &dyn Env,
    config: &Config,
    target: &str,
    options: WakeOptions,
) -> WakeOutcome {
    let Ok(agent) = env.agent(target) else {
        return WakeOutcome::NotFound;
    };
    if !agent.is_sleeping() {
        return WakeOutcome::NotSleeping;
    }
    let Some(state_dir) = env.state_dir() else {
        return WakeOutcome::Failed("no state directory".into());
    };
    if let Err(error) = std::fs::create_dir_all(&state_dir) {
        return WakeOutcome::Failed(format!(
            "cannot create the state directory {}: {error}",
            state_dir.display()
        ));
    }
    let pane = agent.pane_id.as_str();
    let label = agent.name.as_deref().unwrap_or(pane);
    let kind = agent.kind().unwrap_or_default();

    let stamp = state_dir.join(format!("wake-{}.ts", sanitize_pane(pane)));
    let now = env.now_millis();
    if !options.wait && woken_recently(&stamp, now) {
        return WakeOutcome::Deduplicated;
    }
    if let Err(error) = write_atomic(&stamp, &now.to_string()) {
        return WakeOutcome::Failed(format!("cannot write {}: {error}", stamp.display()));
    }

    let journal = Journal::new(&state_dir);
    journal.log("WAKE", &format!("{label} ({pane}) kind={kind}"));
    if let Err(error) = env.send_keys(pane, &["enter"]) {
        return WakeOutcome::Failed(format!("cannot press Enter: {error}"));
    }
    if !options.wait {
        return WakeOutcome::Woken { ms: 0 };
    }

    match await_ready(env, config, pane, kind) {
        Some(elapsed) => {
            restore_name(env, &journal, pane, agent.name.as_deref());
            forget(&journal, &state_dir, pane);
            let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            journal.log("READY", &format!("{label} ({pane}) {ms}"));
            sync_markers(env, config);
            WakeOutcome::Woken { ms }
        }
        None => {
            journal.log(
                "WAKE-TIMEOUT",
                &format!(
                    "{label} ({pane}) not ready after {}s",
                    READY_TIMEOUT.as_secs()
                ),
            );
            WakeOutcome::TimedOut
        }
    }
}

/// Whether the Agent in `pane` is Ready: Herdr recognises it again under
/// `kind` (not as a Sleeping agent), its status is one of the configured
/// `sleep_statuses`, and its screen shows the Profile's prompt.
pub fn is_ready(env: &dyn Env, pane: &str, kind: &str, config: &Config) -> bool {
    let Some(profile) = config.profile(kind) else {
        return false;
    };
    let Ok(agent) = env.agent(pane) else {
        return false;
    };
    if agent.is_sleeping() || agent.agent.as_deref() != Some(kind) {
        return false;
    }
    if !config.allows_status(agent.agent_status) {
        return false;
    }
    env.screen_ansi(pane, guards::SCREEN_LINES)
        .is_ok_and(|screen| guards::is_ready_screen(&screen, profile))
}

/// Wait up to [`READY_TIMEOUT`] for the Agent to be Ready; the time it took.
pub(super) fn await_ready(
    env: &dyn Env,
    config: &Config,
    pane: &str,
    kind: &str,
) -> Option<Duration> {
    let mut took = Duration::ZERO;
    wait_until(env, READY_TIMEOUT, READY_POLL, |elapsed| {
        let ready = is_ready(env, pane, kind, config);
        if ready {
            took = elapsed;
        }
        ready
    })
    .then_some(took)
}

/// Give the pane its original name back: Herdr forgets it while the pane
/// holds a Placeholder. A failure is journaled and does not stop the Wake.
pub(super) fn restore_name(env: &dyn Env, journal: &Journal, pane: &str, name: Option<&str>) {
    let Some(name) = name.filter(|name| !name.is_empty()) else {
        return;
    };
    if let Err(error) = env.rename_agent(pane, name) {
        journal.log("WARN", &format!("{pane}: cannot restore the name: {error}"));
    }
}

/// Remove the pane from the registry of sleeping panes: the Agent is back, so
/// a Herdr restart must not put a Placeholder in its place. A failure is
/// journaled and does not stop the Wake.
pub(super) fn forget(journal: &Journal, state_dir: &Path, pane: &str) {
    if let Err(error) = Registry::remove(state_dir, pane) {
        journal.log(
            "WARN",
            &format!("{pane}: cannot update {}: {error}", registry::FILE_NAME),
        );
    }
}

/// Keeps `[A-Za-z0-9_-]`, replaces anything else with `_`, so a pane id such
/// as `w1:p2` can be part of a file name.
pub fn sanitize_pane(pane: &str) -> String {
    pane.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn woken_recently(stamp: &Path, now: u64) -> bool {
    let Some(previous) = std::fs::read_to_string(stamp)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
    else {
        return false;
    };
    let window = u64::try_from(DEDUP_WINDOW.as_millis()).unwrap_or(u64::MAX);
    // A stamp from the future (clock change) does not silence anything.
    now.checked_sub(previous).is_some_and(|age| age < window)
}

/// Write `text` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("ts.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::guards::{Refusal, SleepMode};
    use crate::herdr::{Agent, AgentStatus, Pane, ProcessInfo};
    use crate::ops::EnvResult;
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    pub(in crate::ops) const EPOCH_MS: u64 = 1_000_000_000_000;
    const PROMPT: &str = "some output\n\u{276F} \n";

    /// Scripted environment with a virtual clock.
    pub(in crate::ops) struct Fake {
        pub(in crate::ops) base: Instant,
        pub(in crate::ops) elapsed: Cell<Duration>,
        pub(in crate::ops) calls: RefCell<Vec<String>>,
        pub(in crate::ops) state_dir: PathBuf,
        pub(in crate::ops) found: bool,
        pub(in crate::ops) label: String,
        pub(in crate::ops) name: Option<String>,
        pub(in crate::ops) status: AgentStatus,
        /// Time after the first Enter at which Herdr reports `claude`.
        pub(in crate::ops) agent_after: Option<Duration>,
        /// Time after the first Enter at which the prompt is on screen.
        pub(in crate::ops) prompt_after: Option<Duration>,
        pub(in crate::ops) enter_at: Cell<Option<Duration>>,
    }

    impl Fake {
        pub(in crate::ops) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-sleep-wake-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Fake {
                base: Instant::now(),
                elapsed: Cell::new(Duration::ZERO),
                calls: RefCell::new(Vec::new()),
                state_dir: dir,
                found: true,
                label: "sleeping-claude".into(),
                name: Some("worker".into()),
                status: AgentStatus::Idle,
                agent_after: Some(Duration::from_millis(2000)),
                prompt_after: Some(Duration::from_millis(3000)),
                enter_at: Cell::new(None),
            }
        }

        pub(in crate::ops) fn log(&self, call: impl Into<String>) {
            self.calls.borrow_mut().push(call.into());
        }

        pub(in crate::ops) fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        pub(in crate::ops) fn clock(&self) -> Duration {
            self.elapsed.get()
        }

        pub(in crate::ops) fn advance(&self, by: Duration) {
            self.elapsed.set(self.elapsed.get() + by);
        }

        pub(in crate::ops) fn journal(&self) -> String {
            std::fs::read_to_string(self.state_dir.join(crate::journal::FILE_NAME))
                .unwrap_or_default()
        }

        pub(in crate::ops) fn events(&self) -> Vec<String> {
            self.journal()
                .lines()
                .map(|l| l.split(' ').nth(1).unwrap().to_string())
                .collect()
        }

        /// Whether `after` has passed since the first Enter.
        pub(in crate::ops) fn passed(&self, after: Option<Duration>) -> bool {
            match (self.enter_at.get(), after) {
                (Some(at), Some(after)) => self.clock() >= at + after,
                _ => false,
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }

    impl Env for Fake {
        fn snapshot(&self) -> EnvResult<crate::herdr::Snapshot> {
            Ok(crate::herdr::Snapshot::default())
        }

        fn pane(&self, _: &str) -> EnvResult<Pane> {
            unreachable!("wake does not read panes")
        }

        fn agent(&self, target: &str) -> EnvResult<Agent> {
            self.log(format!("agent {target}"));
            if !self.found {
                return Err("not found".into());
            }
            let label = if self.passed(self.agent_after) {
                "claude"
            } else {
                self.label.as_str()
            };
            Ok(Agent {
                pane_id: "w1:p2".into(),
                name: self.name.clone(),
                agent: Some(label.into()),
                agent_status: self.status,
                ..Agent::default()
            })
        }

        fn process_info(&self, _: &str) -> EnvResult<ProcessInfo> {
            unreachable!()
        }

        fn screen_text(&self, _: &str, _: usize) -> EnvResult<String> {
            unreachable!()
        }

        fn screen_ansi(&self, pane: &str, lines: usize) -> EnvResult<String> {
            self.log(format!("read-ansi {pane} {lines}"));
            Ok(if self.passed(self.prompt_after) {
                PROMPT.into()
            } else {
                "loading...\n".into()
            })
        }

        fn rename_agent(&self, pane: &str, name: &str) -> EnvResult<()> {
            self.log(format!("rename {pane} {name}"));
            Ok(())
        }

        fn send_text(&self, pane: &str, text: &str) -> EnvResult<()> {
            self.log(format!("text {pane} {text}"));
            Ok(())
        }

        fn send_keys(&self, pane: &str, keys: &[&str]) -> EnvResult<()> {
            self.log(format!("keys {pane} {}", keys.join(" ")));
            if self.enter_at.get().is_none() {
                self.enter_at.set(Some(self.clock()));
            }
            Ok(())
        }

        fn run(&self, _: &str, _: &[&str]) -> EnvResult<()> {
            unreachable!()
        }

        fn notify(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!()
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
            unreachable!()
        }

        fn agent_rss_kb(&self, _: &Agent, _: &str) -> Option<u64> {
            None
        }

        fn shell_path(&self, _: i32) -> Option<PathBuf> {
            None
        }

        fn state_dir(&self) -> Option<PathBuf> {
            Some(self.state_dir.clone())
        }

        fn placeholder_bin(&self) -> EnvResult<PathBuf> {
            unreachable!()
        }

        fn main_bin(&self) -> PathBuf {
            "/opt/bin/herdr-sleep".into()
        }

        fn herdr_bin(&self) -> PathBuf {
            "/usr/bin/herdr".into()
        }

        fn sleep(&self, duration: Duration) {
            self.advance(duration);
        }

        fn now(&self) -> Instant {
            self.base + self.elapsed.get()
        }

        fn now_millis(&self) -> u64 {
            EPOCH_MS + u64::try_from(self.elapsed.get().as_millis()).unwrap()
        }
    }

    const NO_WAIT: WakeOptions = WakeOptions { wait: false };
    const WAIT: WakeOptions = WakeOptions { wait: true };

    fn wake(fake: &Fake, options: WakeOptions) -> WakeOutcome {
        wake_agent(fake, &Config::default(), "worker", options)
    }

    fn registered(fake: &Fake) {
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        for pane in ["w1:p2", "w1:p3"] {
            Registry::add(
                &fake.state_dir,
                pane,
                registry::Sleeper {
                    kind: "claude".into(),
                    session: "s".into(),
                    name: None,
                    cwd: "/".into(),
                    since_ms: 1,
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn a_ready_agent_leaves_the_registry() {
        let fake = Fake::new("registry-ready");
        registered(&fake);
        assert_eq!(wake(&fake, WAIT), WakeOutcome::Woken { ms: 3000 });
        let panes = Registry::load(&fake.state_dir).panes;
        assert_eq!(panes.keys().collect::<Vec<_>>(), ["w1:p3"]);
    }

    #[test]
    fn an_agent_that_is_not_ready_stays_registered() {
        let mut fake = Fake::new("registry-timeout");
        fake.prompt_after = None;
        registered(&fake);
        assert_eq!(wake(&fake, WAIT), WakeOutcome::TimedOut);
        assert!(Registry::load(&fake.state_dir).panes.contains_key("w1:p2"));
    }

    #[test]
    fn wakes_a_sleeping_agent_without_waiting() {
        let fake = Fake::new("nowait");
        assert_eq!(wake(&fake, NO_WAIT), WakeOutcome::Woken { ms: 0 });
        assert_eq!(fake.calls(), ["agent worker", "keys w1:p2 enter"]);
        assert_eq!(fake.events(), ["WAKE"]);
        assert!(fake.journal().contains("WAKE worker (w1:p2) kind=claude"));
        assert_eq!(fake.clock(), Duration::ZERO);
    }

    #[test]
    fn unknown_target_is_not_found() {
        let mut fake = Fake::new("missing");
        fake.found = false;
        assert_eq!(wake(&fake, NO_WAIT), WakeOutcome::NotFound);
        assert_eq!(fake.calls(), ["agent worker"]);
        assert!(!fake.state_dir.exists());
    }

    #[test]
    fn an_awake_agent_is_left_alone() {
        let mut fake = Fake::new("awake");
        fake.label = "claude".into();
        assert_eq!(wake(&fake, WAIT), WakeOutcome::NotSleeping);
        assert_eq!(fake.calls(), ["agent worker"]);
        assert!(!fake.state_dir.exists());
    }

    #[test]
    fn a_second_wake_within_15_seconds_is_deduplicated() {
        let mut fake = Fake::new("dedup");
        fake.agent_after = None;
        assert_eq!(wake(&fake, NO_WAIT), WakeOutcome::Woken { ms: 0 });
        fake.advance(Duration::from_millis(14_999));
        assert_eq!(wake(&fake, NO_WAIT), WakeOutcome::Deduplicated);
        let enters = fake.calls().iter().filter(|c| c.contains("keys")).count();
        assert_eq!(enters, 1);
        assert_eq!(fake.events(), ["WAKE"]);
    }

    #[test]
    fn a_wake_after_15_seconds_goes_through() {
        let mut fake = Fake::new("after");
        fake.agent_after = None;
        wake(&fake, NO_WAIT);
        fake.advance(Duration::from_secs(15));
        assert_eq!(wake(&fake, NO_WAIT), WakeOutcome::Woken { ms: 0 });
        assert_eq!(fake.events(), ["WAKE", "WAKE"]);
    }

    #[test]
    fn wait_bypasses_the_deduplication_and_restamps() {
        let mut fake = Fake::new("bypass");
        fake.agent_after = None;
        wake(&fake, NO_WAIT);
        fake.advance(Duration::from_secs(1));
        // The Agent comes up 2 s after the second Enter.
        fake.agent_after = Some(Duration::from_millis(2000));
        fake.enter_at.set(None);
        assert_eq!(wake(&fake, WAIT), WakeOutcome::Woken { ms: 3000 });
        let stamp = fake.state_dir.join("wake-w1_p2.ts");
        assert_eq!(
            std::fs::read_to_string(stamp).unwrap(),
            (EPOCH_MS + 1000).to_string()
        );
    }

    #[test]
    fn ready_agent_gets_its_name_back() {
        let fake = Fake::new("ready");
        assert_eq!(wake(&fake, WAIT), WakeOutcome::Woken { ms: 3000 });
        let calls = fake.calls();
        let rename = calls.iter().position(|c| c == "rename w1:p2 worker");
        assert!(rename.is_some(), "{calls:?}");
        assert_eq!(calls[1], "keys w1:p2 enter");
        assert_eq!(calls.iter().filter(|c| c.starts_with("rename")).count(), 1);
        assert_eq!(fake.events(), ["WAKE", "READY"]);
        assert!(
            fake.journal()
                .trim_end()
                .ends_with("READY worker (w1:p2) 3000")
        );
        assert!(calls.contains(&"read-ansi w1:p2 40".to_string()));
    }

    #[test]
    fn a_sleeping_label_is_not_ready() {
        let mut fake = Fake::new("label");
        fake.agent_after = None;
        assert_eq!(wake(&fake, WAIT), WakeOutcome::TimedOut);
        assert!(!fake.calls().iter().any(|c| c.starts_with("rename")));
        // The screen is not even read while Herdr still reports the Placeholder.
        assert!(!fake.calls().iter().any(|c| c.starts_with("read-ansi")));
    }

    #[test]
    fn a_ready_agent_without_prompt_is_not_ready() {
        let mut fake = Fake::new("noprompt");
        fake.prompt_after = None;
        assert_eq!(wake(&fake, WAIT), WakeOutcome::TimedOut);
        assert!(!fake.calls().iter().any(|c| c.starts_with("rename")));
        assert_eq!(fake.events(), ["WAKE", "WAKE-TIMEOUT"]);
    }

    #[test]
    fn a_working_agent_is_not_ready() {
        let mut fake = Fake::new("working");
        fake.status = AgentStatus::Working;
        assert_eq!(wake(&fake, WAIT), WakeOutcome::TimedOut);
    }

    #[test]
    fn times_out_after_120_seconds() {
        let mut fake = Fake::new("timeout");
        fake.agent_after = None;
        assert_eq!(wake(&fake, WAIT), WakeOutcome::TimedOut);
        assert_eq!(fake.clock(), Duration::from_secs(120));
        // One lookup of the target, then a check every 250 ms up to 120 s.
        let lookups = fake
            .calls()
            .iter()
            .filter(|c| c.starts_with("agent"))
            .count();
        assert_eq!(lookups, 1 + 481);
        assert!(fake.journal().contains("WAKE-TIMEOUT worker (w1:p2)"));
    }

    #[test]
    fn an_unnamed_pane_skips_the_rename() {
        let mut fake = Fake::new("unnamed");
        fake.name = None;
        assert_eq!(wake(&fake, WAIT), WakeOutcome::Woken { ms: 3000 });
        assert!(!fake.calls().iter().any(|c| c.starts_with("rename")));
        assert!(fake.journal().contains("READY w1:p2 (w1:p2) 3000"));
    }

    #[test]
    fn the_stamp_name_is_sanitised() {
        assert_eq!(sanitize_pane("w1:p2"), "w1_p2");
        assert_eq!(sanitize_pane("../a b"), "___a_b");
        assert_eq!(sanitize_pane("ok_name-1"), "ok_name-1");
        let fake = Fake::new("sanitise");
        wake(&fake, NO_WAIT);
        assert!(fake.state_dir.join("wake-w1_p2.ts").is_file());
    }

    #[test]
    fn the_stamp_is_written_atomically() {
        let fake = Fake::new("atomic");
        wake(&fake, NO_WAIT);
        let names: Vec<String> = std::fs::read_dir(&fake.state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"wake-w1_p2.ts".to_string()), "{names:?}");
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        let stamp = std::fs::read_to_string(fake.state_dir.join("wake-w1_p2.ts")).unwrap();
        assert_eq!(stamp, EPOCH_MS.to_string());
    }

    #[test]
    fn a_stamp_from_the_future_does_not_deduplicate() {
        let fake = Fake::new("future");
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        std::fs::write(
            fake.state_dir.join("wake-w1_p2.ts"),
            (EPOCH_MS + 60_000).to_string(),
        )
        .unwrap();
        assert_eq!(wake(&fake, NO_WAIT), WakeOutcome::Woken { ms: 0 });
    }

    #[test]
    fn a_garbled_stamp_does_not_deduplicate() {
        let fake = Fake::new("garbled");
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        std::fs::write(fake.state_dir.join("wake-w1_p2.ts"), "soon").unwrap();
        assert_eq!(wake(&fake, NO_WAIT), WakeOutcome::Woken { ms: 0 });
    }
}

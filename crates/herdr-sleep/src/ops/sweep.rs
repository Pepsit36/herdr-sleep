//! The Sweep: one pass over every managed, awake agent.
//!
//! Each agent has an idle clock. Its signature is `status:state_change_seq:focused`;
//! while the signature stays the same the clock keeps running, any change
//! restarts it. An agent that stays Settled and unfocused for
//! `sleep_after_minutes` is put to sleep. The clocks live in
//! `<state_dir>/sweep.json` so a restarted Watcher keeps counting.
//!
//! Journal events: the ones of [`sleep_agent`](super::sleep_agent), and
//! `PRUNE` when a pane of the registry of sleeping panes is gone.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::guards::SleepMode;
use crate::herdr::{Agent, Snapshot};
use crate::journal::Journal;

use super::registry::{self, Registry};
use super::{Env, SleepOutcome, sleep_agent, sync_markers};

/// Most agents put to sleep in one pass. A sleep takes seconds, so the rest
/// waits for the next pass instead of freezing the Watcher.
pub const MAX_SLEEPS_PER_SWEEP: usize = 3;

/// Name of the clocks file inside the state directory.
pub const STATE_FILE: &str = "sweep.json";

/// The idle clock of one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentClock {
    /// `status:state_change_seq:focused` when the clock was last restarted.
    pub signature: String,
    /// Wall-clock milliseconds since the Unix epoch at which it restarted.
    pub since_ms: u64,
    /// Code of the refusal already logged for this agent, if any.
    #[serde(default)]
    pub skip: Option<String>,
}

/// The idle clocks of every tracked agent, keyed by pane id.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SweepState {
    #[serde(default)]
    pub agents: BTreeMap<String, AgentClock>,
}

impl SweepState {
    /// The file holding the state inside `state_dir`.
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(STATE_FILE)
    }

    /// Read the state from `state_dir`. A missing, unreadable or garbled file
    /// gives an empty state: clocks restart, nothing else is lost.
    pub fn load(state_dir: &Path) -> Self {
        fs::read_to_string(Self::path(state_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write the state to `state_dir` atomically (temporary file, then rename).
    pub fn save(&self, state_dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(state_dir)?;
        let target = Self::path(state_dir);
        let tmp = state_dir.join(format!("{STATE_FILE}.tmp"));
        let text = serde_json::to_string(self).map_err(std::io::Error::other)?;
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &target)
    }
}

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct SweepReport {
    /// Managed, awake agents looked at.
    pub considered: usize,
    /// Panes now asleep.
    pub slept: Vec<String>,
    /// Panes a guard refused, with the refusal code.
    pub refused: Vec<(String, String)>,
    /// Panes whose sleep was abandoned or unconfirmed, with the reason.
    pub aborted: Vec<(String, String)>,
    /// Panes that would have slept (`dry_run`).
    pub dry_run: Vec<String>,
    /// Eligible panes left for the next pass by [`MAX_SLEEPS_PER_SWEEP`].
    pub deferred: usize,
    /// The pass could not run (Herdr unreachable).
    pub error: Option<String>,
}

impl SweepReport {
    /// Whether the pass changed or tried to change anything.
    pub fn is_eventful(&self) -> bool {
        !self.slept.is_empty() || !self.aborted.is_empty() || !self.dry_run.is_empty()
    }
}

/// Whether the agent has the focus, by its own flag or by the session focus.
pub fn is_focused(snapshot: &Snapshot, agent: &Agent) -> bool {
    agent.focused || snapshot.focused_pane_id.as_deref() == Some(agent.pane_id.as_str())
}

/// The signature an idle clock keeps: `status:state_change_seq:focused`.
pub fn signature(agent: &Agent, focused: bool) -> String {
    format!(
        "{}:{}:{focused}",
        agent.agent_status, agent.state_change_seq
    )
}

/// Forget the sleeping panes that no longer exist. A snapshot without any pane
/// is not trusted: it more likely means Herdr is still restoring its session
/// than that every pane was closed.
fn prune_registry(env: &dyn Env, snapshot: &Snapshot) {
    let Some(dir) = env.state_dir() else {
        return;
    };
    if snapshot.panes.is_empty() {
        return;
    }
    let alive: HashSet<&str> = snapshot.panes.iter().map(|p| p.pane_id.as_str()).collect();
    match Registry::prune(&dir, &alive) {
        Ok(gone) => {
            for pane in gone {
                Journal::new(&dir).log("PRUNE", &format!("{pane} no longer exists"));
            }
        }
        Err(error) => {
            Journal::new(&dir).log(
                "WARN",
                &format!("cannot prune {}: {error}", registry::FILE_NAME),
            );
        }
    }
}

/// Run one Sweep: update the idle clocks in `state` and put to sleep the
/// agents that are due.
pub fn sweep_once(env: &dyn Env, config: &Config, state: &mut SweepState) -> SweepReport {
    sweep_with_mode(env, config, state, SleepMode::Sweep)
}

/// [`sweep_once`] with the sleep mode spelled out.
pub fn sweep_with_mode(
    env: &dyn Env,
    config: &Config,
    state: &mut SweepState,
    mode: SleepMode,
) -> SweepReport {
    let mut report = SweepReport::default();
    let snapshot = match env.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report.error = Some(format!("cannot read the session: {error}"));
            return report;
        }
    };
    prune_registry(env, &snapshot);
    let now = env.now_millis();
    let sleep_after_ms = u64::try_from(config.sleep_after().as_millis()).unwrap_or(u64::MAX);

    let mut clocks: BTreeMap<String, AgentClock> = BTreeMap::new();
    let mut due = Vec::new();
    for agent in &snapshot.agents {
        if agent.is_sleeping() || !agent.kind().is_some_and(|kind| config.is_managed(kind)) {
            continue;
        }
        report.considered += 1;
        let focused = is_focused(&snapshot, agent);
        let signature = signature(agent, focused);
        let clock = match state.agents.remove(&agent.pane_id) {
            Some(clock) if clock.signature == signature => clock,
            _ => AgentClock {
                signature,
                since_ms: now,
                skip: None,
            },
        };
        let eligible = !focused
            && config.allows_status(agent.agent_status)
            && now.saturating_sub(clock.since_ms) >= sleep_after_ms;
        if eligible {
            due.push(agent);
        }
        clocks.insert(agent.pane_id.clone(), clock);
    }

    let mut attempts = 0;
    for agent in due {
        if attempts >= MAX_SLEEPS_PER_SWEEP {
            report.deferred += 1;
            continue;
        }
        let pane = agent.pane_id.clone();
        let last_skip = clocks.get(&pane).and_then(|c| c.skip.clone());
        match sleep_agent(env, config, agent, mode, last_skip.as_deref()) {
            SleepOutcome::Skipped(refusal) => {
                let code = refusal.code().to_string();
                if let Some(clock) = clocks.get_mut(&pane) {
                    clock.skip = Some(code.clone());
                }
                report.refused.push((pane, code));
                continue;
            }
            SleepOutcome::Asleep { .. } => {
                clocks.remove(&pane);
                report.slept.push(pane);
            }
            SleepOutcome::DryRun => report.dry_run.push(pane),
            SleepOutcome::Aborted(reason) | SleepOutcome::Warned(reason) => {
                report.aborted.push((pane, reason));
            }
        }
        attempts += 1;
    }

    state.agents = clocks;
    // Clocks are an optimisation of the next pass, never a reason to fail.
    if let Some(dir) = env.state_dir()
        && let Err(error) = state.save(&dir)
    {
        Journal::new(&dir).log("WARN", &format!("cannot save {STATE_FILE}: {error}"));
    }
    // One synchronisation per pass heals a marker the per-sleep ones missed.
    if !report.slept.is_empty() {
        sync_markers(env, config);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::Refusal;
    use crate::herdr::{Agent, AgentSession, AgentStatus, Pane, ProcessInfo, Snapshot};
    use crate::ops::EnvResult;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const EPOCH_MS: u64 = 1_000_000_000_000;
    const MINUTE: u64 = 60_000;

    struct Fake {
        base: Instant,
        clock_ms: Cell<u64>,
        state_dir: PathBuf,
        agents: RefCell<Vec<Agent>>,
        focused: RefCell<Option<String>>,
        refusals: RefCell<HashMap<String, Refusal>>,
        snapshot_error: Cell<bool>,
        panes: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-sleep-sweep-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Fake {
                base: Instant::now(),
                clock_ms: Cell::new(EPOCH_MS),
                state_dir: dir,
                agents: RefCell::new(Vec::new()),
                focused: RefCell::new(None),
                refusals: RefCell::new(HashMap::new()),
                snapshot_error: Cell::new(false),
                panes: RefCell::new(Vec::new()),
            }
        }

        fn add(&self, pane: &str, status: AgentStatus, seq: u64) {
            self.agents.borrow_mut().push(Agent {
                pane_id: pane.into(),
                agent: Some("claude".into()),
                agent_status: status,
                state_change_seq: seq,
                agent_session: Some(AgentSession {
                    source: "herdr:claude".into(),
                    agent: Some("claude".into()),
                    kind: Some("id".into()),
                    value: "sid".into(),
                }),
                ..Agent::default()
            });
        }

        fn update(&self, pane: &str, f: impl Fn(&mut Agent)) {
            for agent in self.agents.borrow_mut().iter_mut() {
                if agent.pane_id == pane {
                    f(agent);
                }
            }
        }

        fn advance(&self, ms: u64) {
            self.clock_ms.set(self.clock_ms.get() + ms);
        }

        fn journal(&self) -> String {
            fs::read_to_string(self.state_dir.join(crate::journal::FILE_NAME)).unwrap_or_default()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.state_dir);
        }
    }

    impl Env for Fake {
        fn snapshot(&self) -> EnvResult<Snapshot> {
            if self.snapshot_error.get() {
                return Err("socket closed".into());
            }
            Ok(Snapshot {
                panes: self
                    .panes
                    .borrow()
                    .iter()
                    .map(|pane| Pane {
                        pane_id: pane.clone(),
                        ..Pane::default()
                    })
                    .collect(),
                agents: self.agents.borrow().clone(),
                focused_pane_id: self.focused.borrow().clone(),
                ..Snapshot::default()
            })
        }
        fn pane(&self, _: &str) -> EnvResult<Pane> {
            unreachable!()
        }
        fn agent(&self, _: &str) -> EnvResult<Agent> {
            unreachable!()
        }
        fn process_info(&self, _: &str) -> EnvResult<ProcessInfo> {
            unreachable!()
        }
        fn screen_text(&self, _: &str, _: usize) -> EnvResult<String> {
            unreachable!()
        }
        fn screen_ansi(&self, _: &str, _: usize) -> EnvResult<String> {
            unreachable!()
        }
        fn rename_agent(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!()
        }
        fn send_text(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!()
        }
        fn send_keys(&self, _: &str, _: &[&str]) -> EnvResult<()> {
            unreachable!()
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
        fn check_guards(&self, _: &Config, agent: &Agent, _: SleepMode) -> Result<(), Refusal> {
            match self.refusals.borrow().get(&agent.pane_id) {
                Some(refusal) => Err(refusal.clone()),
                None => Ok(()),
            }
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
            self.advance(u64::try_from(duration.as_millis()).unwrap());
        }
        fn now(&self) -> Instant {
            self.base
        }
        fn now_millis(&self) -> u64 {
            self.clock_ms.get()
        }
    }

    /// A config that stops at the dry run, so no pane is ever touched.
    fn config(minutes: u64) -> Config {
        Config {
            sleep_after_minutes: minutes,
            dry_run: true,
            ..Config::default()
        }
    }

    fn pass(fake: &Fake, config: &Config, state: &mut SweepState) -> SweepReport {
        sweep_once(fake, config, state)
    }

    #[test]
    fn the_clock_starts_at_the_first_sight_and_the_boundary_is_inclusive() {
        let fake = Fake::new("boundary");
        fake.add("p1", AgentStatus::Idle, 1);
        let (config, mut state) = (config(15), SweepState::default());

        let report = pass(&fake, &config, &mut state);
        assert_eq!(report.considered, 1);
        assert!(report.dry_run.is_empty());
        assert_eq!(state.agents["p1"].since_ms, EPOCH_MS);

        fake.advance(15 * MINUTE - 1);
        assert!(pass(&fake, &config, &mut state).dry_run.is_empty());
        fake.advance(1);
        assert_eq!(pass(&fake, &config, &mut state).dry_run, ["p1"]);
    }

    #[test]
    fn zero_minutes_means_eligible_at_once() {
        let fake = Fake::new("zero");
        fake.add("p1", AgentStatus::Done, 1);
        let report = pass(&fake, &config(0), &mut SweepState::default());
        assert_eq!(report.dry_run, ["p1"]);
    }

    #[test]
    fn any_signature_change_restarts_the_clock() {
        let fake = Fake::new("reset");
        fake.add("p1", AgentStatus::Idle, 1);
        let (config, mut state) = (config(10), SweepState::default());
        pass(&fake, &config, &mut state);

        fake.advance(9 * MINUTE);
        fake.update("p1", |a| a.state_change_seq = 2);
        pass(&fake, &config, &mut state);
        assert_eq!(state.agents["p1"].since_ms, EPOCH_MS + 9 * MINUTE);

        fake.advance(9 * MINUTE);
        fake.update("p1", |a| a.agent_status = AgentStatus::Done);
        pass(&fake, &config, &mut state);
        assert_eq!(state.agents["p1"].since_ms, EPOCH_MS + 18 * MINUTE);

        fake.advance(9 * MINUTE);
        *fake.focused.borrow_mut() = Some("p1".into());
        pass(&fake, &config, &mut state);
        assert_eq!(state.agents["p1"].since_ms, EPOCH_MS + 27 * MINUTE);

        fake.advance(9 * MINUTE);
        *fake.focused.borrow_mut() = None;
        let report = pass(&fake, &config, &mut state);
        assert!(report.dry_run.is_empty());
        assert_eq!(state.agents["p1"].since_ms, EPOCH_MS + 36 * MINUTE);
    }

    #[test]
    fn a_focused_agent_never_sleeps() {
        let fake = Fake::new("focused");
        fake.add("p1", AgentStatus::Idle, 1);
        fake.add("p2", AgentStatus::Idle, 1);
        fake.update("p1", |a| a.focused = true);
        *fake.focused.borrow_mut() = Some("p2".into());
        let (config, mut state) = (config(0), SweepState::default());
        let report = pass(&fake, &config, &mut state);
        assert_eq!(report.considered, 2);
        assert!(report.dry_run.is_empty());
        fake.advance(60 * MINUTE);
        assert!(pass(&fake, &config, &mut state).dry_run.is_empty());
    }

    #[test]
    fn working_and_blocked_agents_never_accumulate_idle_time() {
        let fake = Fake::new("busy");
        fake.add("p1", AgentStatus::Working, 1);
        fake.add("p2", AgentStatus::Blocked, 1);
        let (config, mut state) = (config(0), SweepState::default());
        for _ in 0..3 {
            fake.advance(60 * MINUTE);
            assert!(pass(&fake, &config, &mut state).dry_run.is_empty());
        }
    }

    #[test]
    fn unmanaged_and_sleeping_agents_are_ignored() {
        let fake = Fake::new("ignored");
        fake.add("p1", AgentStatus::Idle, 1);
        fake.add("p2", AgentStatus::Idle, 1);
        fake.add("p3", AgentStatus::Idle, 1);
        fake.update("p1", |a| a.agent = Some("sleeping-claude".into()));
        fake.update("p2", |a| a.agent = Some("mystery".into()));
        let mut state = SweepState::default();
        let report = pass(&fake, &config(0), &mut state);
        assert_eq!(report.considered, 1);
        assert_eq!(report.dry_run, ["p3"]);
        assert_eq!(state.agents.keys().collect::<Vec<_>>(), ["p3"]);
    }

    #[test]
    fn a_refusal_is_logged_once_per_code_and_again_when_the_code_changes() {
        let fake = Fake::new("skip");
        fake.add("p1", AgentStatus::Idle, 1);
        fake.refusals
            .borrow_mut()
            .insert("p1".into(), Refusal::DraftInPrompt);
        let (config, mut state) = (config(0), SweepState::default());

        let report = pass(&fake, &config, &mut state);
        assert_eq!(report.refused, [("p1".into(), "draft-in-prompt".into())]);
        pass(&fake, &config, &mut state);
        pass(&fake, &config, &mut state);
        assert_eq!(fake.journal().matches("SKIP").count(), 1);
        assert_eq!(state.agents["p1"].skip.as_deref(), Some("draft-in-prompt"));

        fake.refusals
            .borrow_mut()
            .insert("p1".into(), Refusal::PromptNotDetected);
        pass(&fake, &config, &mut state);
        assert_eq!(fake.journal().matches("SKIP").count(), 2);
        pass(&fake, &config, &mut state);
        assert_eq!(fake.journal().matches("SKIP").count(), 2);

        // A change of state forgets the refusal: the next one is logged again.
        fake.update("p1", |a| a.state_change_seq = 9);
        pass(&fake, &config, &mut state);
        assert_eq!(fake.journal().matches("SKIP").count(), 3);
        pass(&fake, &config, &mut state);
        assert_eq!(fake.journal().matches("SKIP").count(), 3);
    }

    #[test]
    fn at_most_three_agents_are_handled_per_pass() {
        let fake = Fake::new("cap");
        for n in 1..=5 {
            fake.add(&format!("p{n}"), AgentStatus::Idle, 1);
        }
        let (config, mut state) = (config(0), SweepState::default());
        let report = pass(&fake, &config, &mut state);
        assert_eq!(report.dry_run.len(), MAX_SLEEPS_PER_SWEEP);
        assert_eq!(report.deferred, 2);
        // A dry run leaves the agents awake, so the same ones are due again.
        assert_eq!(pass(&fake, &config, &mut state).dry_run.len(), 3);
    }

    #[test]
    fn refused_agents_do_not_use_the_cap() {
        let fake = Fake::new("cap-refused");
        for n in 1..=5 {
            fake.add(&format!("p{n}"), AgentStatus::Idle, 1);
        }
        for n in 1..=3 {
            fake.refusals
                .borrow_mut()
                .insert(format!("p{n}"), Refusal::DraftInPrompt);
        }
        let report = pass(&fake, &config(0), &mut SweepState::default());
        assert_eq!(report.refused.len(), 3);
        assert_eq!(report.dry_run, ["p4", "p5"]);
        assert_eq!(report.deferred, 0);
    }

    #[test]
    fn disappeared_panes_are_dropped_from_the_state() {
        let fake = Fake::new("gone");
        fake.add("p1", AgentStatus::Idle, 1);
        fake.add("p2", AgentStatus::Idle, 1);
        let (config, mut state) = (config(30), SweepState::default());
        pass(&fake, &config, &mut state);
        assert_eq!(state.agents.len(), 2);
        fake.agents.borrow_mut().retain(|a| a.pane_id != "p1");
        pass(&fake, &config, &mut state);
        assert_eq!(state.agents.keys().collect::<Vec<_>>(), ["p2"]);
    }

    #[test]
    fn a_failed_snapshot_reports_an_error_and_keeps_the_state() {
        let fake = Fake::new("error");
        fake.add("p1", AgentStatus::Idle, 1);
        let (config, mut state) = (config(30), SweepState::default());
        pass(&fake, &config, &mut state);
        fake.snapshot_error.set(true);
        let report = pass(&fake, &config, &mut state);
        assert!(report.error.unwrap().contains("socket closed"));
        assert_eq!(state.agents.len(), 1);
    }

    #[test]
    fn the_state_is_saved_after_each_pass_and_reloaded() {
        let fake = Fake::new("persist");
        fake.add("p1", AgentStatus::Idle, 4);
        let config = config(30);
        let mut state = SweepState::default();
        pass(&fake, &config, &mut state);

        let text = fs::read_to_string(SweepState::path(&fake.state_dir)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["agents"]["p1"]["signature"], "idle:4:false");
        assert_eq!(json["agents"]["p1"]["since_ms"], EPOCH_MS);
        assert!(json["agents"]["p1"]["skip"].is_null());
        assert!(!fake.state_dir.join("sweep.json.tmp").exists());
        assert_eq!(SweepState::load(&fake.state_dir), state);
    }

    fn register(fake: &Fake, panes: &[&str]) {
        for pane in panes {
            let sleeper = registry::Sleeper {
                kind: "claude".into(),
                session: "s".into(),
                name: None,
                cwd: "/".into(),
                since_ms: 1,
            };
            Registry::add(&fake.state_dir, pane, sleeper).unwrap();
        }
    }

    #[test]
    fn the_sweep_prunes_the_sleepers_whose_pane_is_gone() {
        let fake = Fake::new("prune");
        register(&fake, &["p1", "p2", "p3"]);
        *fake.panes.borrow_mut() = vec!["p2".into(), "p9".into()];
        pass(&fake, &config(30), &mut SweepState::default());
        let panes = Registry::load(&fake.state_dir).panes;
        assert_eq!(panes.keys().collect::<Vec<_>>(), ["p2"]);
        let journal = fs::read_to_string(fake.state_dir.join("herdr-sleep.log")).unwrap();
        assert!(journal.contains("PRUNE p1 no longer exists"));
        assert!(journal.contains("PRUNE p3 no longer exists"));
    }

    #[test]
    fn a_snapshot_without_panes_prunes_nothing() {
        let fake = Fake::new("prune-empty");
        register(&fake, &["p1"]);
        pass(&fake, &config(30), &mut SweepState::default());
        assert!(Registry::load(&fake.state_dir).panes.contains_key("p1"));
    }

    #[test]
    fn a_garbled_or_missing_state_file_is_an_empty_state() {
        let fake = Fake::new("garbage");
        assert_eq!(SweepState::load(&fake.state_dir), SweepState::default());
        fs::create_dir_all(&fake.state_dir).unwrap();
        for text in [
            "",
            "not json",
            "[1]",
            r#"{"agents":3}"#,
            r#"{"agents":{"p":{}}}"#,
        ] {
            fs::write(SweepState::path(&fake.state_dir), text).unwrap();
            assert_eq!(SweepState::load(&fake.state_dir), SweepState::default());
        }
        fs::write(SweepState::path(&fake.state_dir), "{}").unwrap();
        assert_eq!(SweepState::load(&fake.state_dir), SweepState::default());
    }
}

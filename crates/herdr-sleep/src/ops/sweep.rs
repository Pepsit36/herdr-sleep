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

use super::live_agents::{self, LiveAgents};
use super::pane_lock::PaneLock;
use super::registry::{self, Registry};
use super::wake::{sanitize_pane, wake_stamp_path};
use super::{Env, SleepOutcome, sleep_agent, sync_markers};

/// Most agents put to sleep in one pass. A sleep takes seconds, so the rest
/// waits for the next pass instead of freezing the Watcher.
pub const MAX_SLEEPS_PER_SWEEP: usize = 3;

/// Wait before a Sleep that was abandoned is tried again, doubled by every
/// failure in a row up to [`RETRY_CEILING_MS`]. An exit command that does not
/// work would otherwise be typed into the agent at every pass.
const RETRY_BASE_MS: u64 = 5 * 60_000;
const RETRY_CEILING_MS: u64 = 60 * 60_000;

/// Name of the clocks file inside the state directory.
pub const STATE_FILE: &str = "sweep.json";

/// The idle clock of one agent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentClock {
    /// `status:state_change_seq:focused` when the clock was last restarted.
    pub signature: String,
    /// Wall-clock milliseconds since the Unix epoch at which it restarted.
    pub since_ms: u64,
    /// Code of the refusal already logged for this agent, if any.
    #[serde(default)]
    pub skip: Option<String>,
    /// Abandoned Sleeps in a row since the clock restarted.
    #[serde(default)]
    pub failures: u32,
    /// Wall-clock milliseconds before which a Sweep does not try again.
    #[serde(default)]
    pub retry_after_ms: u64,
}

impl AgentClock {
    /// Note an abandoned Sleep at `now` and push the next try away.
    fn failed(&mut self, now: u64) {
        self.failures = self.failures.saturating_add(1);
        let wait = RETRY_BASE_MS
            .saturating_mul(1 << self.failures.saturating_sub(1).min(8))
            .min(RETRY_CEILING_MS);
        self.retry_after_ms = now.saturating_add(wait);
    }
}

/// The idle clocks of every tracked agent, keyed by pane id.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SweepState {
    #[serde(default)]
    pub agents: BTreeMap<String, AgentClock>,
    /// Panes this process saw with an Agent: what tells an Agent that quit
    /// from one Herdr has not brought back yet. Never saved: after a restart
    /// the new Watcher has seen nothing.
    #[serde(skip)]
    pub alive: HashSet<String>,
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
        let text = serde_json::to_string(self).map_err(std::io::Error::other)?;
        super::atomic::write(&Self::path(state_dir), &text)
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
    let journal = Journal::new(&dir);
    match Registry::prune(&dir, &alive) {
        Ok(gone) => {
            for pane in gone {
                journal.log("PRUNE", &format!("{pane} no longer exists"));
            }
        }
        Err(error) => {
            journal.log(
                "WARN",
                &format!("cannot prune {}: {error}", registry::FILE_NAME),
            );
        }
    }
    prune_pane_files(&dir, snapshot, env.now_millis());
    for (pane, reason) in stale_entries(env, &dir, snapshot) {
        match Registry::remove(&dir, &pane) {
            Ok(()) => journal.log("PRUNE", &format!("{pane} {reason}")),
            Err(error) => journal.log(
                "WARN",
                &format!("cannot prune {}: {error}", registry::FILE_NAME),
            ),
        }
    }
}

/// How old the files of a pane that no longer exists must be before they go.
const PANE_FILE_GRACE_MS: u64 = 60 * 60_000;

/// Remove the files kept for panes that no longer exist (a saved draft, a
/// preview, a replay text, a wake stamp, the lock files). A draft left behind
/// would be typed into whatever agent a recycled pane id holds some day.
fn prune_pane_files(dir: &Path, snapshot: &Snapshot, now_ms: u64) {
    let alive: HashSet<String> = snapshot
        .panes
        .iter()
        .map(|pane| sanitize_pane(&pane.pane_id))
        .collect();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let owner = ["draft", "preview", "replay", "wake", "sleep"]
            .into_iter()
            .find_map(|family| {
                let rest = name.strip_prefix(family)?.strip_prefix('-')?;
                let (id, extension) = rest.rsplit_once('.')?;
                matches!(extension, "txt" | "ts" | "lock").then_some(id)
            });
        let Some(owner) = owner else {
            continue;
        };
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|age| {
                now_ms.saturating_sub(u64::try_from(age.as_millis()).unwrap_or(u64::MAX))
                    >= PANE_FILE_GRACE_MS
            });
        if !alive.contains(owner) && old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// How long after a Wake its entry may still be waiting for the agent to be
/// ready (the Wake itself gives up after two minutes).
const WAKE_GRACE_MS: u64 = 5 * 60_000;

/// The registered panes whose entry no longer describes a pane that sleeps,
/// with why: a restart would otherwise put the Placeholder back for a session
/// that is running again or whose resume failed.
///
/// A pane is left alone while a Sleep or a Wake holds it, and while it shows
/// a Sleeping agent. One that runs an agent is stale. A bare shell is stale
/// only when a Wake was asked for since the last Sleep (that stamp is removed
/// when a Sleep starts) and that was a while ago: after a Herdr restart the
/// shell is bare too, and the entry is what `restore` needs.
fn stale_entries(env: &dyn Env, dir: &Path, snapshot: &Snapshot) -> Vec<(String, &'static str)> {
    let registry = Registry::load(dir);
    let now = env.now_millis();
    let mut stale = Vec::new();
    for pane in registry.panes.keys() {
        let Some(info) = snapshot.pane(pane) else {
            continue;
        };
        if info.is_sleeping() {
            continue;
        }
        let busy = ["sleep", "wake"].into_iter().any(|purpose| {
            !matches!(
                PaneLock::try_acquire_for(dir, pane, purpose),
                Ok(Some(_)) | Err(_)
            )
        });
        if busy {
            continue;
        }
        if info.has_agent() {
            stale.push((pane.clone(), "runs an agent again"));
            continue;
        }
        let woken_at = fs::read_to_string(wake_stamp_path(dir, pane))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok());
        if woken_at.is_some_and(|at| now.saturating_sub(at) >= WAKE_GRACE_MS) {
            stale.push((pane.clone(), "was woken and is not asleep any more"));
        }
    }
    stale
}

/// Refresh the record of live Agents that `restore` falls back on.
fn record_live_agents(env: &dyn Env, config: &Config, state: &mut SweepState, snapshot: &Snapshot) {
    let Some(dir) = env.state_dir() else {
        return;
    };
    let mut live = LiveAgents::load(&dir);
    if live.refresh(
        snapshot,
        config,
        &mut state.alive,
        env.now_millis(),
        &|pane| env.pane_shell_started_ms(pane),
    ) && let Err(error) = live.save(&dir)
    {
        Journal::new(&dir).log(
            "WARN",
            &format!("cannot save {}: {error}", live_agents::FILE_NAME),
        );
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
    sweep_pass(env, config, state, mode, false)
}

/// A Sweep that does not wait for the Idle time: every Settled, unfocused
/// agent is due now, and the cap of [`MAX_SLEEPS_PER_SWEEP`] does not apply.
/// The other Guards still apply.
pub fn sweep_now(env: &dyn Env, config: &Config, state: &mut SweepState) -> SweepReport {
    sweep_pass(env, config, state, SleepMode::Sweep, true)
}

fn sweep_pass(
    env: &dyn Env,
    config: &Config,
    state: &mut SweepState,
    mode: SleepMode,
    immediate: bool,
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
    record_live_agents(env, config, state, &snapshot);
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
                failures: 0,
                retry_after_ms: 0,
            },
        };
        let eligible = !focused
            && config.allows_status(agent.agent_status)
            && (immediate
                || (now.saturating_sub(clock.since_ms) >= sleep_after_ms
                    && now >= clock.retry_after_ms));
        if eligible {
            due.push(agent);
        }
        clocks.insert(agent.pane_id.clone(), clock);
    }

    let mut attempts = 0;
    for agent in due {
        if !immediate && attempts >= MAX_SLEEPS_PER_SWEEP {
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
            // Instant and harmless: it must not use up the places of the agents
            // behind it.
            SleepOutcome::DryRun => {
                report.dry_run.push(pane);
                continue;
            }
            SleepOutcome::Aborted(reason) => {
                if let Some(clock) = clocks.get_mut(&pane) {
                    clock.failed(now);
                }
                report.aborted.push((pane, reason));
            }
            SleepOutcome::Warned(reason) => report.aborted.push((pane, reason)),
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
    // One synchronisation per pass heals a marker the per-sleep ones missed,
    // such as the prefix of a tab whose agent was woken and has quit since.
    sync_markers(env, config);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::Refusal;
    use crate::herdr::{Agent, AgentSession, AgentStatus, Pane, ProcessInfo, Snapshot};
    use crate::ops::EnvResult;
    use crate::ops::registry::Sleeper;
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
        snapshots: Cell<usize>,
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
                snapshots: Cell::new(0),
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
            self.snapshots.set(self.snapshots.get() + 1);
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
        fn agent(&self, pane: &str) -> EnvResult<Agent> {
            self.agents
                .borrow()
                .iter()
                .find(|agent| agent.pane_id == pane)
                .cloned()
                .ok_or_else(|| "no such agent".into())
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
    fn every_pass_records_the_live_agents_for_a_restart() {
        let fake = Fake::new("live");
        fake.panes.borrow_mut().push("p1".into());
        fake.add("p1", AgentStatus::Working, 1);
        fake.update("p1", |a| a.cwd = Some("/work".into()));
        let mut state = SweepState::default();
        pass(&fake, &config(600), &mut state);
        let live = LiveAgents::load(&fake.state_dir);
        assert_eq!(live.panes["p1"].session, "sid");
        assert_eq!(live.panes["p1"].cwd, "/work");

        // The agent quits while the Watcher looks: its record goes.
        fake.agents.borrow_mut().clear();
        pass(&fake, &config(600), &mut state);
        assert!(LiveAgents::load(&fake.state_dir).panes.is_empty());
    }

    #[test]
    fn a_new_watcher_keeps_the_record_of_agents_herdr_has_not_brought_back() {
        let fake = Fake::new("live-restart");
        fake.panes.borrow_mut().push("p1".into());
        fake.add("p1", AgentStatus::Working, 1);
        fake.update("p1", |a| a.cwd = Some("/work".into()));
        pass(&fake, &config(600), &mut SweepState::default());

        // Herdr restarted: the pane is a shell, the Watcher is new.
        fake.agents.borrow_mut().clear();
        pass(&fake, &config(600), &mut SweepState::default());
        assert!(LiveAgents::load(&fake.state_dir).panes.contains_key("p1"));
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
    fn a_dry_run_reports_every_agent_that_is_due() {
        // It touches nothing and takes no time: it must not hide the agents
        // behind the first three.
        let fake = Fake::new("cap");
        for n in 1..=5 {
            fake.add(&format!("p{n}"), AgentStatus::Idle, 1);
        }
        let (config, mut state) = (config(0), SweepState::default());
        let report = pass(&fake, &config, &mut state);
        assert_eq!(report.dry_run.len(), 5);
        assert_eq!(report.deferred, 0);
    }

    #[test]
    fn a_refusal_does_not_use_up_a_place_either() {
        let fake = Fake::new("cap-refused");
        for n in 1..=5 {
            fake.add(&format!("p{n}"), AgentStatus::Idle, 1);
        }
        for n in 1..=4 {
            fake.refusals
                .borrow_mut()
                .insert(format!("p{n}"), Refusal::Focused);
        }
        let (config, mut state) = (config(0), SweepState::default());
        let report = pass(&fake, &config, &mut state);
        assert_eq!(report.refused.len(), 4);
        assert_eq!(report.dry_run, ["p5"]);
    }

    #[test]
    fn an_abandoned_sleep_is_tried_again_later_and_later_each_time() {
        let mut clock = AgentClock::default();
        clock.failed(1_000);
        assert_eq!(clock.retry_after_ms, 1_000 + RETRY_BASE_MS);
        clock.failed(2_000);
        assert_eq!(clock.retry_after_ms, 2_000 + 2 * RETRY_BASE_MS);
        for _ in 0..20 {
            clock.failed(3_000);
        }
        assert_eq!(clock.retry_after_ms, 3_000 + RETRY_CEILING_MS);
    }

    #[test]
    fn an_agent_waiting_for_its_next_try_is_not_due_but_a_sweep_now_takes_it() {
        let fake = Fake::new("backoff");
        fake.add("p1", AgentStatus::Idle, 1);
        let (config, mut state) = (config(0), SweepState::default());
        pass(&fake, &config, &mut state);
        state.agents.get_mut("p1").unwrap().retry_after_ms = EPOCH_MS + 10 * MINUTE;
        assert!(pass(&fake, &config, &mut state).dry_run.is_empty());
        assert_eq!(sweep_now(&fake, &config, &mut state).dry_run, ["p1"]);
        fake.advance(10 * MINUTE);
        assert_eq!(pass(&fake, &config, &mut state).dry_run, ["p1"]);
    }

    fn registered(fake: &Fake, pane: &str) {
        let sleeper = Sleeper {
            kind: "claude".into(),
            session: "sid".into(),
            name: None,
            cwd: "/work".into(),
            since_ms: EPOCH_MS,
        };
        Registry::add(&fake.state_dir, pane, sleeper).unwrap();
    }

    fn snapshot_of(panes: &[(&str, Option<&str>)]) -> Snapshot {
        Snapshot {
            panes: panes
                .iter()
                .map(|(pane, agent)| Pane {
                    pane_id: (*pane).into(),
                    agent: agent.map(str::to_string),
                    ..Pane::default()
                })
                .collect(),
            ..Snapshot::default()
        }
    }

    fn stale(fake: &Fake, snapshot: &Snapshot) -> Vec<String> {
        stale_entries(fake, &fake.state_dir, snapshot)
            .into_iter()
            .map(|(pane, _)| pane)
            .collect()
    }

    #[test]
    fn the_files_of_a_pane_that_is_gone_are_removed_after_a_while() {
        let fake = Fake::new("pane-files");
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        let aged = |name: &str, age_ms: u64| {
            let path = fake.state_dir.join(name);
            let file = std::fs::File::create(&path).unwrap();
            file.set_modified(UNIX_EPOCH + Duration::from_millis(EPOCH_MS - age_ms))
                .unwrap();
            path
        };
        let hour = 60 * MINUTE;
        let gone_old = aged("draft-w9_p9.txt", 2 * hour);
        let gone_recent = aged("sleep-w9_p9.lock", MINUTE);
        let alive_old = aged("preview-w1_p1.txt", 2 * hour);
        let other = aged("sweep.json", 2 * hour);
        prune_pane_files(&fake.state_dir, &snapshot_of(&[("w1:p1", None)]), EPOCH_MS);
        assert!(!gone_old.exists());
        assert!(gone_recent.exists());
        assert!(alive_old.exists());
        assert!(other.exists());
    }

    #[test]
    fn an_entry_whose_pane_runs_an_agent_again_is_stale() {
        let fake = Fake::new("stale-agent");
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        registered(&fake, "p1");
        registered(&fake, "p2");
        let snapshot = snapshot_of(&[("p1", Some("claude")), ("p2", Some("sleeping-claude"))]);
        assert_eq!(stale(&fake, &snapshot), ["p1"]);
    }

    #[test]
    fn a_bare_shell_is_stale_only_after_a_wake_that_was_asked_for() {
        let fake = Fake::new("stale-shell");
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        registered(&fake, "p1");
        let snapshot = snapshot_of(&[("p1", None)]);
        // After a Herdr restart: no Wake since the Sleep, `restore` needs it.
        assert!(stale(&fake, &snapshot).is_empty());
        let stamp = wake_stamp_path(&fake.state_dir, "p1");
        std::fs::write(&stamp, (EPOCH_MS - WAKE_GRACE_MS / 2).to_string()).unwrap();
        assert!(stale(&fake, &snapshot).is_empty());
        std::fs::write(&stamp, (EPOCH_MS - WAKE_GRACE_MS).to_string()).unwrap();
        assert_eq!(stale(&fake, &snapshot), ["p1"]);
    }

    #[test]
    fn a_pane_a_sleep_holds_keeps_its_entry() {
        let fake = Fake::new("stale-locked");
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        registered(&fake, "p1");
        let _lock = PaneLock::try_acquire(&fake.state_dir, "p1").unwrap();
        assert!(stale(&fake, &snapshot_of(&[("p1", Some("claude"))])).is_empty());
    }

    #[test]
    fn every_pass_checks_the_markers_even_when_nothing_sleeps() {
        // The prefix of a tab whose woken agent has quit is only ever
        // cleared by a synchronisation: a pass that sleeps nothing does one.
        let fake = Fake::new("markers-every-pass");
        fake.add("w1:p1", AgentStatus::Working, 1);
        let mut state = SweepState::default();
        let report = sweep_once(&fake, &Config::default(), &mut state);
        assert!(report.slept.is_empty());
        // One read of the session for the sweep, one for the markers.
        assert_eq!(fake.snapshots.get(), 2);
    }

    #[test]
    fn a_sweep_now_does_not_wait_for_the_idle_time_nor_the_cap() {
        let fake = Fake::new("now");
        for n in 1..=5 {
            fake.add(&format!("p{n}"), AgentStatus::Idle, 1);
        }
        let (config, mut state) = (config(600), SweepState::default());
        assert!(pass(&fake, &config, &mut state).dry_run.is_empty());
        let report = sweep_now(&fake, &config, &mut state);
        assert_eq!(report.dry_run.len(), 5);
        assert_eq!(report.deferred, 0);
    }

    #[test]
    fn a_sweep_now_keeps_the_other_guards() {
        let fake = Fake::new("now-guards");
        fake.add("p1", AgentStatus::Idle, 1);
        fake.add("p2", AgentStatus::Working, 1);
        fake.add("p3", AgentStatus::Idle, 1);
        fake.refusals
            .borrow_mut()
            .insert("p3".into(), Refusal::DraftInPrompt);
        let report = sweep_now(&fake, &config(600), &mut SweepState::default());
        assert_eq!(report.dry_run, ["p1"]);
        assert_eq!(
            report.refused,
            [("p3".to_string(), "draft-in-prompt".to_string())]
        );
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

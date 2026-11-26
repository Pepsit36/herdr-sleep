//! The Restore operation: after a Herdr restart, put the Placeholder back in
//! every pane that was asleep or that Herdr still holds a Saved session for
//! (ADR 0008, 0009).
//!
//! Herdr brings a restored pane back as a plain shell. Two sources say which
//! of them must sleep: the registry of sleeping panes, and the Saved sessions
//! Herdr itself reports for panes with no live Agent (an Agent that never went
//! to sleep, stopped by the restart). For each pane that still exists, runs no
//! agent, sits at its shell prompt and whose working directory still exists:
//!
//! - a registered pane gets its Placeholder, started the way Sleep starts it;
//! - a Saved session pane first gets its Agent back with `herdr agent start`
//!   and its Session, then goes to sleep through the ordinary Sleep. Herdr
//!   keeps the Saved session as the pane's identity and ignores every report
//!   of another source while it does, so a Placeholder cannot take the pane
//!   over directly. One Agent at a time is ever running.
//!
//! Panes are restored one after the other, and a pane that is not ready yet
//! never holds the others back: it is looked at again after them, round after
//! round, until it has had twice the settle timeout. Nothing here fails
//! loudly: a pane that does not qualify is skipped and the reason goes to the
//! journal.
//!
//! Journal events: `RESTORE-DRY-RUN`, `RESTORE-FRESH`, `RESTORE-START panes=N registered=M saved=K recorded=R`,
//! `RESTORE <pane>`, `RESTORE-RETRY <pane> <reason>`,
//! `RESTORE-SKIP <pane> <reason>` (also for a pane Herdr shows asleep that the
//! registry does not know), `RESTORE-CWD <pane> <cwd>`,
//! `RESTORE-RESUME <pane>`, `RESTORE-DONE restored=N skipped=M missing_cwd=K`,
//! `WARN`.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::guards::{Refusal, SleepMode};
use crate::herdr::{Pane, Snapshot};
use crate::journal::Journal;

use super::Env;
use super::live_agents::LiveAgents;
use super::markers::sync_markers;
use super::pane_lock::PaneLock;
use super::registry::{Registry, Sleeper};
use super::sleep::{
    PlaceholderParams, SleepOutcome, is_typeable, placeholder_argv, shell_quote,
    sleep_agent_waiting, wait_until,
};
use super::wake::sanitize_pane;

/// Name of the file holding the last restore report inside the state
/// directory.
pub const REPORT_FILE: &str = "restore.json";

/// Poll interval while waiting for a pane to be ready for its Placeholder.
const SETTLE_POLL: Duration = Duration::from_millis(250);
/// Pause between two rounds over the panes that are not ready yet.
const ROUND_POLL: Duration = Duration::from_secs(1);
/// Poll interval while waiting for the Placeholder to report.
const SLEEPING_POLL: Duration = Duration::from_millis(300);
/// Longest wait for the Placeholder to report.
const SLEEPING_TIMEOUT: Duration = Duration::from_secs(10);

/// What one restore pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RestoreReport {
    /// Panes that hold a Placeholder again, in the order they were restored.
    pub restored: Vec<String>,
    /// Panes left alone, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Panes ignored because their working directory is gone, with it.
    #[serde(default)]
    pub missing_cwd: Vec<(String, String)>,
    /// The pass could not run (no state directory, Herdr unreachable).
    pub error: Option<String>,
}

impl RestoreReport {
    /// Read the report the last restore left in `state_dir`.
    pub fn load(state_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(state_dir.join(REPORT_FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save(&self, state_dir: &Path) {
        if let Ok(text) = serde_json::to_string(self) {
            let _ = std::fs::write(state_dir.join(REPORT_FILE), text);
        }
    }
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
    /// At its shell prompt with no agent label, but Herdr still holds a Saved
    /// session for the pane.
    SessionHeld,
}

impl PaneState {
    fn reason(self) -> &'static str {
        match self {
            PaneState::Ready => "ready",
            PaneState::AgentAlive => "agent-alive",
            PaneState::Busy => "pane-busy",
            PaneState::IdentityHeld => "identity-not-released",
            PaneState::SessionHeld => "session-held",
        }
    }
}

/// `saved` panes are chosen for the Session Herdr holds, so only a live
/// agent label keeps them from being ready.
fn pane_state(env: &dyn Env, pane: &str, saved: bool) -> PaneState {
    let at_shell = env.process_info(pane).is_ok_and(|info| info.at_shell());
    let (agent, session) = env.pane(pane).map_or((true, false), |p| {
        (p.has_agent(), p.agent_session.is_some())
    });
    let held = agent || (!saved && session);
    match (at_shell, held) {
        (true, false) => PaneState::Ready,
        (true, true) if !agent => PaneState::SessionHeld,
        (true, true) => PaneState::IdentityHeld,
        (false, true) => PaneState::AgentAlive,
        (false, false) => PaneState::Busy,
    }
}

/// Whether looking at a pane again may help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// No: the reason is final.
    Never,
    /// Yes, once: an attempt failed and another may work.
    Once,
    /// Yes, round after round until the pane has had its time: it is not
    /// ready yet.
    Poll,
}

/// Why a pane was left alone, and whether a second look may help.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Skip {
    reason: String,
    retry: Retry,
}

impl Skip {
    fn final_(reason: impl Into<String>) -> Self {
        Skip {
            reason: reason.into(),
            retry: Retry::Never,
        }
    }

    fn transient(reason: impl Into<String>) -> Self {
        Skip {
            reason: reason.into(),
            retry: Retry::Once,
        }
    }

    fn not_ready(state: PaneState) -> Self {
        Skip {
            reason: state.reason().into(),
            retry: Retry::Poll,
        }
    }
}

/// A pane that has to get its Placeholder back.
struct Candidate {
    pane: String,
    sleeper: Sleeper,
    /// The pane comes from a Saved session, not from the registry.
    saved: bool,
    /// Herdr holds no session for the pane: the record of live Agents does.
    recorded: bool,
}

/// Why a pane's Saved session cannot make a Sleeper.
enum Unfit {
    /// Not a pane for the plugin: leave it alone.
    Skip(String),
    /// The working directory is gone.
    MissingCwd(String),
}

/// The Sleeper a Saved session describes.
fn saved_sleeper(env: &dyn Env, config: &Config, pane: &Pane) -> Result<Sleeper, Unfit> {
    let Some(session) = &pane.agent_session else {
        return Err(Unfit::Skip("no saved session".into()));
    };
    let label = session.agent.as_deref().or(pane.agent.as_deref());
    let Some(kind) = label.map(|l| {
        l.strip_prefix(crate::herdr::model::SLEEPING_PREFIX)
            .unwrap_or(l)
    }) else {
        return Err(Unfit::Skip("saved session has no kind".into()));
    };
    if config.profile(kind).is_none() {
        return Err(Unfit::Skip(format!("no profile for kind {kind}")));
    }
    let cwd = pane.effective_cwd().unwrap_or("");
    if pane.restore_error.is_some() || cwd.is_empty() || !env.dir_exists(cwd) {
        return Err(Unfit::MissingCwd(cwd.to_string()));
    }
    Ok(Sleeper {
        kind: kind.to_string(),
        session: session.value.clone(),
        name: pane.name.clone().filter(|n| !n.is_empty()),
        cwd: cwd.to_string(),
        since_ms: env.now_millis(),
    })
}

/// Panes to restore, with the ones that are left out right away.
fn candidates(
    env: &dyn Env,
    config: &Config,
    registry: &Registry,
    live: &LiveAgents,
    snapshot: &Snapshot,
    report: &mut RestoreReport,
) -> Vec<Candidate> {
    let mut found = Vec::new();
    for (pane, sleeper) in &registry.panes {
        let Some(info) = snapshot.pane(pane) else {
            report.skipped.push((pane.clone(), "pane-gone".into()));
            continue;
        };
        // Already a Sleeping agent: a second restore, or a pane that kept it.
        if info.is_sleeping() {
            continue;
        }
        if !env.dir_exists(&sleeper.cwd) {
            report.missing_cwd.push((pane.clone(), sleeper.cwd.clone()));
        } else {
            found.push(Candidate {
                pane: pane.clone(),
                sleeper: sleeper.clone(),
                saved: false,
                recorded: false,
            });
        }
    }
    for pane in &snapshot.panes {
        if registry.panes.contains_key(&pane.pane_id) {
            continue;
        }
        // Herdr shows a sleeping agent the plugin has no record of: there is
        // nothing to restore it from, and it must not go unsaid.
        if pane.is_sleeping() {
            report
                .skipped
                .push((pane.pane_id.clone(), "sleeping-agent-not-registered".into()));
            continue;
        }
        // Herdr holds no session for an Agent it never saw report one (a
        // Cursor without its hook, an Agent started before its hook): the
        // record of live Agents knows it.
        if pane.agent_session.is_none() {
            let Some(record) = live.panes.get(&pane.pane_id) else {
                continue;
            };
            if pane.has_agent() || !config.is_managed(&record.kind) {
                continue;
            }
            // A shell older than the last sighting of the Agent was there all
            // along: the user quit the Agent, Herdr did not restart.
            if env
                .pane_shell_started_ms(&pane.pane_id)
                .is_some_and(|started| started < record.since_ms)
            {
                report
                    .skipped
                    .push((pane.pane_id.clone(), "agent-quit-before".into()));
                continue;
            }
            if !env.dir_exists(&record.cwd) {
                report
                    .missing_cwd
                    .push((pane.pane_id.clone(), record.cwd.clone()));
                continue;
            }
            found.push(Candidate {
                pane: pane.pane_id.clone(),
                sleeper: record.clone(),
                saved: true,
                recorded: true,
            });
            continue;
        }
        match saved_sleeper(env, config, pane) {
            Ok(mut sleeper) => {
                // Herdr forgets the name of an Agent that never slept.
                if sleeper.name.is_none() {
                    sleeper.name = live.panes.get(&pane.pane_id).and_then(|r| r.name.clone());
                }
                found.push(Candidate {
                    pane: pane.pane_id.clone(),
                    sleeper,
                    saved: true,
                    recorded: false,
                });
            }
            Err(Unfit::Skip(reason)) => report.skipped.push((pane.pane_id.clone(), reason)),
            Err(Unfit::MissingCwd(cwd)) => report.missing_cwd.push((pane.pane_id.clone(), cwd)),
        }
    }
    found
}

/// Bring the Placeholder back in every pane that qualifies.
pub fn restore(env: &dyn Env, config: &Config) -> RestoreReport {
    let mut report = RestoreReport::default();
    let Some(state_dir) = env.state_dir() else {
        report.error = Some("no state directory".into());
        return report;
    };
    let journal = Journal::new(&state_dir);
    // One restore at a time: a second one, typed by hand while the startup
    // one is still working, would type the Placeholder command into the same
    // panes again, and the copy would be replayed into the agent at its Wake.
    let _lock = match PaneLock::try_acquire_for(&state_dir, "all", "restore") {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            let reason = "another restore is running".to_string();
            journal.log("RESTORE-SKIP", &format!("all {reason}"));
            report.error = Some(reason);
            return report;
        }
        Err(error) => {
            let reason = format!("cannot lock the restore: {error}");
            journal.log("WARN", &format!("restore: {reason}"));
            report.error = Some(reason);
            return report;
        }
    };
    let snapshot = match env.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let reason = format!("cannot read the session: {error}");
            journal.log("WARN", &format!("restore: {reason}"));
            report.error = Some(reason);
            return report;
        }
    };
    let registry = Registry::load(&state_dir);
    let live = LiveAgents::load(&state_dir);
    let mut pending = candidates(env, config, &registry, &live, &snapshot, &mut report);
    journal.log(
        "RESTORE-START",
        &format!(
            "panes={} registered={} saved={} recorded={}",
            snapshot.panes.len(),
            pending.iter().filter(|c| !c.saved).count(),
            pending.iter().filter(|c| c.saved && !c.recorded).count(),
            pending.iter().filter(|c| c.recorded).count()
        ),
    );
    for (pane, reason) in &report.skipped {
        journal.log("RESTORE-SKIP", &format!("{pane} {reason}"));
    }
    for (pane, cwd) in &report.missing_cwd {
        journal.log("RESTORE-CWD", &format!("{pane} {cwd}"));
    }
    if pending.is_empty() && report.skipped.is_empty() && report.missing_cwd.is_empty() {
        return report;
    }

    // `dry_run` only says what would be done: nothing is typed or started.
    if config.dry_run {
        for candidate in pending.drain(..) {
            let source = if candidate.recorded {
                "record"
            } else if candidate.saved {
                "saved-session"
            } else {
                "registry"
            };
            journal.log(
                "RESTORE-DRY-RUN",
                &format!(
                    "{} kind={} source={source}",
                    candidate.pane, candidate.sleeper.kind
                ),
            );
            report
                .skipped
                .push((candidate.pane, "dry-run: nothing is restored".into()));
        }
    }

    let timeout = config.restore_settle_timeout();
    // A pane that is not ready is looked at again after the others, round
    // after round, until it has had two settle timeouts from the start.
    let deadline = timeout * 2;
    let start = env.now();
    let mut noted: HashSet<String> = HashSet::new();
    let mut tried_again: HashSet<String> = HashSet::new();
    while !pending.is_empty() {
        let expired = env.now().duration_since(start) >= deadline;
        let mut retry = Vec::new();
        for mut candidate in pending {
            let mut outcome = restore_pane(
                env,
                config,
                &journal,
                &state_dir,
                &candidate,
                Duration::ZERO,
                timeout,
            );
            // Herdr keeps a Saved session as the identity of a pane for good
            // when no Agent ever ran there: a Placeholder cannot take it
            // over, so the Agent is resumed and goes to sleep (ADR 0009).
            if expired
                && !candidate.saved
                && outcome
                    .as_ref()
                    .is_err_and(|skip| skip.reason == PaneState::SessionHeld.reason())
            {
                candidate.saved = true;
                // The session Herdr holds is the one to resume: the registry
                // may name an older one.
                if let Some(pane) = snapshot.pane(&candidate.pane)
                    && let Ok(mut held) = saved_sleeper(env, config, pane)
                {
                    if held.name.is_none() {
                        held.name = candidate.sleeper.name.clone();
                    }
                    candidate.sleeper = held;
                }
                outcome = restore_pane(
                    env,
                    config,
                    &journal,
                    &state_dir,
                    &candidate,
                    Duration::ZERO,
                    timeout,
                );
            }
            let pane = candidate.pane.clone();
            match outcome {
                Ok(()) => {
                    journal.log("RESTORE", &pane);
                    report.restored.push(pane);
                }
                Err(skip) => {
                    let again = match skip.retry {
                        Retry::Poll => !expired,
                        Retry::Once => tried_again.insert(pane.clone()),
                        Retry::Never => false,
                    };
                    if again {
                        if noted.insert(pane.clone()) {
                            journal.log("RESTORE-RETRY", &format!("{pane} {}", skip.reason));
                        }
                        retry.push(candidate);
                    } else {
                        journal.log("RESTORE-SKIP", &format!("{pane} {}", skip.reason));
                        report.skipped.push((pane, skip.reason));
                    }
                }
            }
        }
        pending = retry;
        if !pending.is_empty() {
            env.sleep(ROUND_POLL);
        }
    }
    journal.log(
        "RESTORE-DONE",
        &format!(
            "restored={} skipped={} missing_cwd={}",
            report.restored.len(),
            report.skipped.len(),
            report.missing_cwd.len()
        ),
    );
    report.save(&state_dir);
    if !report.restored.is_empty() {
        sync_markers(env, config);
    }
    report
}

/// Restore one pane: the Placeholder of a registered pane, or the resumed and
/// slept Agent of a Saved session.
fn restore_pane(
    env: &dyn Env,
    config: &Config,
    journal: &Journal,
    state_dir: &Path,
    candidate: &Candidate,
    settle: Duration,
    timeout: Duration,
) -> Result<(), Skip> {
    let pane = candidate.pane.as_str();
    if candidate.saved {
        resume_then_sleep(
            env,
            config,
            journal,
            pane,
            &candidate.sleeper,
            settle,
            timeout,
        )
    } else {
        put_placeholder(
            env,
            config,
            journal,
            state_dir,
            pane,
            &candidate.sleeper,
            settle,
        )
    }
}

/// Put the Placeholder of `sleeper` in `pane`, the pane's shell being ready
/// within `settle`.
fn put_placeholder(
    env: &dyn Env,
    config: &Config,
    journal: &Journal,
    state_dir: &Path,
    pane: &str,
    sleeper: &Sleeper,
    settle: Duration,
) -> Result<(), Skip> {
    let kind = sleeper.kind.as_str();
    let profile = config
        .profile(kind)
        .ok_or_else(|| Skip::final_(format!("no profile for kind {kind}")))?;
    let placeholder = env
        .placeholder_bin()
        .map_err(|error| Skip::final_(format!("placeholder unavailable: {error}")))?;

    // The pane's shell may still be starting (Herdr starts it when a client
    // attaches) and Herdr releases the native identity a moment after the
    // process is gone.
    let mut state = PaneState::Busy;
    let settled = wait_until(env, settle, SETTLE_POLL, |_| {
        state = pane_state(env, pane, false);
        state == PaneState::Ready
    });
    if !settled {
        return Err(Skip::not_ready(state));
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
        return Err(Skip::final_(format!(
            "cannot type {word:?} safely into a shell (control character or backslash)"
        )));
    }
    let quoted: Vec<String> = argv.iter().map(|word| shell_quote(word)).collect();
    let mut command: Vec<&str> = vec!["exec"];
    command.extend(quoted.iter().map(String::as_str));
    env.run(pane, &command)
        .map_err(|error| Skip::transient(format!("cannot start the placeholder: {error}")))?;

    let announced = wait_until(env, SLEEPING_TIMEOUT, SLEEPING_POLL, |_| {
        env.agent(pane)
            .is_ok_and(|a| a.is_sleeping() && a.kind() == Some(kind))
    });
    if !announced {
        journal.log(
            "WARN",
            &format!("{pane}: placeholder did not report sleeping-{kind}"),
        );
        // The Placeholder runs: typing the command again would nest it.
        return Err(Skip::final_(format!(
            "placeholder did not report sleeping-{kind}"
        )));
    }
    Ok(())
}

/// Longest agent name Herdr accepts.
const NAME_MAX: usize = 32;

/// Whether Herdr accepts `name` for an agent: 1 to 32 characters, a lowercase
/// letter first, then lowercase letters, digits, `-` and `_`.
fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= NAME_MAX
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The name of the resumed agent: the one that was kept when Herdr accepts
/// it, else `<kind>-<pane>` cut to what Herdr accepts (pane ids carry capital
/// letters).
fn resumed_name(sleeper: &Sleeper, pane: &str) -> String {
    if let Some(name) = sleeper.name.as_deref().filter(|n| valid_agent_name(n)) {
        return name.to_string();
    }
    let generated: String = format!("{}-{}", sleeper.kind, sanitize_pane(pane))
        .to_ascii_lowercase()
        .chars()
        .take(NAME_MAX)
        .collect();
    if valid_agent_name(&generated) {
        generated
    } else {
        "agent".to_string()
    }
}

/// Bring the Agent of a Saved session back in `pane` and put it to sleep.
///
/// The pane's shell must be ready within `settle`, the Agent within `timeout`
/// once started, and so must its Guards. An Agent that is back but cannot
/// sleep stays awake: the Sweep puts it to sleep when it has been idle long
/// enough. An Agent that started but whose start was reported as failed (its
/// name was refused, say) is slept all the same: it runs, and leaving it
/// awake would keep its memory for nothing.
fn resume_then_sleep(
    env: &dyn Env,
    config: &Config,
    journal: &Journal,
    pane: &str,
    sleeper: &Sleeper,
    settle: Duration,
    timeout: Duration,
) -> Result<(), Skip> {
    let kind = sleeper.kind.as_str();
    let profile = config
        .profile(kind)
        .ok_or_else(|| Skip::final_(format!("no profile for kind {kind}")))?;
    if config.dry_run {
        return Err(Skip::final_("dry-run: the agent is not resumed"));
    }
    // A Claude Code session that never saved a conversation cannot be
    // resumed (`--resume` fails): the Agent starts fresh in the same
    // directory, which loses nothing.
    let fresh = kind == "claude" && env.claude_conversation_saved(&sleeper.session) == Some(false);
    let resume = if fresh {
        vec![profile.bin.clone()]
    } else {
        profile.resume_argv(&sleeper.session)
    };
    if let Some(word) = resume.iter().find(|word| !is_typeable(word)) {
        return Err(Skip::final_(format!(
            "cannot type {word:?} safely into a shell (control character or backslash)"
        )));
    }
    let mut state = PaneState::Busy;
    let ready = wait_until(env, settle, SETTLE_POLL, |_| {
        state = pane_state(env, pane, true);
        state == PaneState::Ready
    });
    if !ready {
        return Err(Skip::not_ready(state));
    }

    let name = resumed_name(sleeper, pane);
    if fresh {
        journal.log(
            "RESTORE-FRESH",
            &format!("{pane} kind={kind} no saved conversation to resume"),
        );
    }
    journal.log("RESTORE-RESUME", &format!("{pane} kind={kind}"));
    // The profile's command starts with the binary, which `agent start`
    // already knows from the kind.
    let args = resume.get(1..).unwrap_or_default();
    if let Err(error) = env.start_agent(&name, kind, pane, args, timeout) {
        // An Agent that runs in the pane must not be left behind.
        if !env
            .agent(pane)
            .is_ok_and(|a| a.agent.is_some() && !a.is_sleeping())
        {
            return Err(Skip::transient(format!("cannot resume the agent: {error}")));
        }
        journal.log(
            "WARN",
            &format!("{pane}: the agent runs although its start failed: {error}"),
        );
    }
    let agent = env
        .agent(pane)
        .map_err(|error| Skip::final_(format!("cannot read the resumed agent: {error}")))?;
    match sleep_agent_waiting(env, config, &agent, SleepMode::Manual, timeout) {
        SleepOutcome::Asleep { .. } | SleepOutcome::Warned(_) => Ok(()),
        SleepOutcome::Skipped(refusal) => Err(Skip::final_(format!(
            "resumed but still awake ({refusal}); the sweep puts it to sleep once idle"
        ))),
        SleepOutcome::Aborted(reason) => Err(Skip::final_(format!(
            "resumed but still awake ({reason}); the sweep retries"
        ))),
        SleepOutcome::DryRun => Err(Skip::final_("dry-run")),
    }
}

/// Put the Saved session of `pane` to sleep: its Agent is resumed with its
/// Session, then goes to sleep like any other.
pub fn sleep_saved_session(env: &dyn Env, config: &Config, pane_id: &str) -> SleepOutcome {
    let Some(state_dir) = env.state_dir() else {
        return SleepOutcome::Aborted("no state directory".into());
    };
    let pane = match env.pane(pane_id) {
        Ok(pane) => pane,
        Err(error) => return SleepOutcome::Aborted(format!("cannot read pane {pane_id}: {error}")),
    };
    if pane.is_sleeping() {
        return SleepOutcome::Aborted("the pane already holds a sleeping agent".into());
    }
    if pane.has_agent() {
        return SleepOutcome::Aborted("the pane holds a live agent".into());
    }
    if pane.agent_session.is_none() {
        return SleepOutcome::Skipped(Refusal::NoSession);
    }
    let sleeper = match saved_sleeper(env, config, &pane) {
        Ok(sleeper) => sleeper,
        Err(Unfit::Skip(reason)) => return SleepOutcome::Aborted(reason),
        Err(Unfit::MissingCwd(cwd)) => {
            return SleepOutcome::Aborted(format!(
                "the working directory is gone ({}): nothing could resume the session there",
                if cwd.is_empty() { "unknown" } else { &cwd }
            ));
        }
    };
    let journal = Journal::new(&state_dir);
    match resume_then_sleep(
        env,
        config,
        &journal,
        pane_id,
        &sleeper,
        config.restore_settle_timeout(),
        config.restore_settle_timeout(),
    ) {
        Ok(()) => {
            journal.log(
                "ASLEEP",
                &format!("{pane_id} kind={} from the saved session", sleeper.kind),
            );
            SleepOutcome::Asleep { freed_kb: 0 }
        }
        Err(skip) => SleepOutcome::Aborted(skip.reason),
    }
}

/// The Preview file Sleep left for `pane`, when it still exists.
fn preview_path(state_dir: &Path, pane: &str) -> Option<String> {
    let path = state_dir.join(format!("preview-{}.txt", sanitize_pane(pane)));
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::SleepMode;
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
        /// The pane is known to Herdr by a Saved session only (no registry
        /// entry): its working directory, `None` for a plain registered pane.
        saved_cwd: Option<String>,
        /// Herdr keeps a Saved session for this registered pane, with no
        /// agent label, for good.
        session_only: bool,
        /// Herdr reports no session for this pane, with or without an Agent.
        no_session: bool,
    }

    impl Spec {
        fn ready() -> Self {
            Spec {
                shell_at: Some(Duration::ZERO),
                released_at: Some(Duration::ZERO),
                announce_after: Some(Duration::from_millis(600)),
                run_fails: false,
                saved_cwd: None,
                session_only: false,
                no_session: false,
            }
        }

        /// A pane Herdr holds a Saved session for, never registered.
        fn saved(cwd: &str) -> Self {
            Spec {
                saved_cwd: Some(cwd.into()),
                ..Spec::ready()
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
        /// Directories that do not exist.
        missing_dirs: Vec<String>,
        /// Panes Herdr could not restore (`restore_error`).
        restore_errors: Vec<String>,
        /// Kind Herdr reports for the Saved sessions.
        saved_kind: String,
        /// `herdr agent start` fails.
        start_fails: bool,
        /// `herdr agent start` reports a failure after the agent started.
        start_fails_but_runs: bool,
        /// Name Herdr reports for the Saved sessions.
        saved_name: Option<String>,
        /// Panes Herdr shows as `sleeping-claude` that the registry lacks.
        unknown_sleepers: Vec<String>,
        /// Sessions whose conversation Claude Code never saved.
        unsaved: Vec<String>,
        /// When the shell of every pane started, as milliseconds since the epoch.
        shell_started: Option<u64>,
        /// The Guards refuse the resumed agent.
        guard: Option<Refusal>,
        /// Saved session panes whose agent was started again.
        live: RefCell<BTreeMap<String, String>>,
        /// Panes whose agent has quit since.
        quit: RefCell<std::collections::BTreeSet<String>>,
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
                missing_dirs: Vec::new(),
                restore_errors: Vec::new(),
                saved_kind: "claude".into(),
                start_fails: false,
                start_fails_but_runs: false,
                saved_name: Some("worker".into()),
                unknown_sleepers: Vec::new(),
                unsaved: Vec::new(),
                shell_started: None,
                guard: None,
                live: RefCell::new(BTreeMap::new()),
                quit: RefCell::new(Default::default()),
            }
        }

        /// A registered pane that exists in the session, or, with a
        /// `saved_cwd`, a pane only Herdr's Saved session knows.
        fn pane(mut self, pane: &str, spec: Spec) -> Self {
            if spec.saved_cwd.is_none() {
                self.register(pane, "claude", "sess-1", Some("worker"), "/work");
            }
            self.panes.insert(pane.to_string(), spec);
            self
        }

        fn saved_pane(&self, pane: &str, spec: &Spec) -> Pane {
            let held = !self.released(spec);
            Pane {
                pane_id: pane.into(),
                name: self.saved_name.clone(),
                agent: held.then(|| self.saved_kind.clone()),
                agent_session: ((spec.saved_cwd.is_some() && !spec.no_session)
                    || spec.session_only)
                    .then(|| AgentSession {
                        source: "herdr:claude".into(),
                        agent: Some(self.saved_kind.clone()),
                        kind: Some("id".into()),
                        value: format!("sess-{pane}"),
                    }),
                cwd: spec.saved_cwd.clone(),
                restore_error: self
                    .restore_errors
                    .contains(&pane.to_string())
                    .then(|| "Saved directory is unavailable".to_string()),
                ..Pane::default()
            }
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
                    .iter()
                    .map(|(pane, spec)| match spec.saved_cwd {
                        // A Saved session pane shows its Session, with no
                        // live agent label.
                        Some(_) => Pane {
                            agent: None,
                            ..self.saved_pane(pane, spec)
                        },
                        None => Pane {
                            pane_id: pane.clone(),
                            ..Pane::default()
                        },
                    })
                    .chain(self.unknown_sleepers.iter().map(|pane| Pane {
                        pane_id: pane.clone(),
                        agent: Some("sleeping-claude".into()),
                        ..Pane::default()
                    }))
                    .collect(),
                ..Snapshot::default()
            })
        }

        fn pane(&self, pane: &str) -> EnvResult<Pane> {
            let spec = &self.panes[pane];
            if spec.saved_cwd.is_some() {
                if self.quit.borrow().contains(pane) {
                    return Ok(Pane {
                        pane_id: pane.into(),
                        ..Pane::default()
                    });
                }
                if self.live.borrow().contains_key(pane) {
                    return Ok(Pane {
                        agent: Some(self.saved_kind.clone()),
                        ..self.saved_pane(pane, spec)
                    });
                }
                return Ok(self.saved_pane(pane, spec));
            }
            if self.quit.borrow().contains(pane) {
                // The Agent ran and quit: Herdr lets go of the Saved session.
                return Ok(Pane {
                    pane_id: pane.into(),
                    ..Pane::default()
                });
            }
            let held = !self.released(spec);
            Ok(Pane {
                pane_id: pane.into(),
                agent: held.then(|| "claude".to_string()),
                agent_session: (held || spec.session_only).then(|| AgentSession {
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
            if let Some(name) = self.live.borrow().get(target)
                && !self.quit.borrow().contains(target)
            {
                let pane = self.saved_pane(target, spec);
                return Ok(Agent {
                    pane_id: target.into(),
                    name: Some(name.clone()),
                    agent: Some(self.saved_kind.clone()),
                    agent_status: AgentStatus::Idle,
                    cwd: pane.cwd,
                    // A running Agent reports its session itself.
                    agent_session: pane.agent_session.or_else(|| {
                        Some(AgentSession {
                            source: "herdr:claude".into(),
                            agent: Some(self.saved_kind.clone()),
                            kind: Some("id".into()),
                            value: format!("sess-{target}"),
                        })
                    }),
                    ..Agent::default()
                });
            }
            Ok(Agent {
                pane_id: target.into(),
                agent: announced.then(|| "sleeping-claude".to_string()),
                agent_status: AgentStatus::Idle,
                ..Agent::default()
            })
        }

        fn process_info(&self, pane: &str) -> EnvResult<ProcessInfo> {
            let spec = &self.panes[pane];
            let live = self.live.borrow().contains_key(pane) && !self.quit.borrow().contains(pane);
            let at_shell = !live && spec.shell_at.is_some_and(|at| self.clock() >= at);
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

        fn start_agent(
            &self,
            name: &str,
            kind: &str,
            pane: &str,
            args: &[String],
            _: Duration,
        ) -> EnvResult<()> {
            self.log(format!("start {name} {kind} {pane} {}", args.join(" ")));
            if self.start_fails {
                return Err("agent not ready".into());
            }
            self.live.borrow_mut().insert(pane.into(), name.into());
            if self.start_fails_but_runs {
                return Err("invalid_agent_name".into());
            }
            Ok(())
        }

        fn pane_shell_started_ms(&self, _: &str) -> Option<u64> {
            self.shell_started
        }

        fn claude_conversation_saved(&self, session: &str) -> Option<bool> {
            self.unsaved.iter().any(|s| s == session).then_some(false)
        }

        fn send_text(&self, pane: &str, text: &str) -> EnvResult<()> {
            self.log(format!("text {pane} {text}"));
            Ok(())
        }

        fn send_keys(&self, pane: &str, _: &[&str]) -> EnvResult<()> {
            // The exit command is typed, then Enter: the agent quits.
            self.quit.borrow_mut().insert(pane.into());
            Ok(())
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
            Ok(())
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
            match &self.guard {
                Some(refusal) => Err(refusal.clone()),
                None => Ok(()),
            }
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

        fn dir_exists(&self, path: &str) -> bool {
            !self.missing_dirs.iter().any(|dir| dir == path)
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
                "RESTORE-START panes=2 registered=2 saved=0 recorded=0",
                "RESTORE w1-2",
                "RESTORE w1-3",
                "RESTORE-DONE restored=2 skipped=0 missing_cwd=0"
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
        assert!(fake.clock() >= 2 * Config::default().restore_settle_timeout());
        assert_eq!(
            fake.journal(),
            [
                "RESTORE-START panes=1 registered=1 saved=0 recorded=0",
                "RESTORE-RETRY w1-2 identity-not-released",
                "RESTORE-SKIP w1-2 identity-not-released",
                "RESTORE-DONE restored=0 skipped=1 missing_cwd=0"
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
            "RESTORE-DONE restored=1 skipped=1 missing_cwd=0"
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
    fn an_empty_registry_does_nothing_and_says_it_ran() {
        let fake = Fake::new("empty");
        assert_eq!(run(&fake), RestoreReport::default());
        assert_eq!(
            fake.journal(),
            ["RESTORE-START panes=0 registered=0 saved=0 recorded=0"]
        );
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
            missing_cwd: vec![("c".into(), "/gone".into())],
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "restored": ["a"],
                "skipped": [["b", "pane-busy"]],
                "missing_cwd": [["c", "/gone"]],
                "error": null
            })
        );
    }

    fn config_with_timeout(seconds: u64) -> Config {
        Config {
            restore_settle_timeout_seconds: seconds,
            ..Config::default()
        }
    }

    #[test]
    fn a_saved_session_gets_a_placeholder_and_a_registry_entry() {
        let fake = Fake::new("saved").pane("w1-2", Spec::saved("/proj"));
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2"]);
        let command = &fake.runs()[0];
        assert!(
            command.contains(" --kind claude --session sess-w1-2 "),
            "{command}"
        );
        assert!(command.contains(" --cwd /proj"), "{command}");
        assert!(
            command.ends_with(" -- claude --resume sess-w1-2"),
            "{command}"
        );
        let registry = Registry::load(&fake.state_dir);
        assert_eq!(registry.panes["w1-2"].session, "sess-w1-2");
        assert_eq!(registry.panes["w1-2"].name.as_deref(), Some("worker"));
    }

    #[test]
    fn a_pane_with_a_live_agent_label_is_waited_for() {
        let spec = Spec {
            released_at: Some(Duration::from_secs(5)),
            ..Spec::saved("/proj")
        };
        let mut fake = Fake::new("saved-label").pane("w1-2", spec);
        // The snapshot shows no label, the pane itself still does until it
        // is released.
        assert_eq!(run(&fake).restored, ["w1-2"]);
        assert!(fake.exec_at.borrow()["w1-2"] >= Duration::from_secs(5));
        fake.missing_dirs.clear();
    }

    #[test]
    fn a_pane_whose_directory_is_gone_is_listed_apart() {
        let mut fake = Fake::new("cwd")
            .pane("w1-2", Spec::saved("/gone"))
            .pane("w1-3", Spec::ready());
        fake.missing_dirs = vec!["/gone".into(), "/work".into()];
        fake.register("w1-4", "claude", "s", None, "/work");
        fake.panes.insert("w1-4".into(), Spec::ready());
        let report = run(&fake);
        assert!(report.restored.is_empty());
        assert!(report.skipped.is_empty());
        assert_eq!(
            report.missing_cwd,
            [
                ("w1-3".to_string(), "/work".to_string()),
                ("w1-4".to_string(), "/work".to_string()),
                ("w1-2".to_string(), "/gone".to_string()),
            ]
        );
        assert!(fake.runs().is_empty());
        assert_eq!(
            fake.journal().last().unwrap(),
            "RESTORE-DONE restored=0 skipped=0 missing_cwd=3"
        );
    }

    #[test]
    fn herdr_restore_error_means_the_directory_is_gone() {
        let mut fake = Fake::new("restore-error").pane("w1-2", Spec::saved("/proj"));
        fake.restore_errors = vec!["w1-2".into()];
        let report = run(&fake);
        assert_eq!(
            report.missing_cwd,
            [("w1-2".to_string(), "/proj".to_string())]
        );
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn a_saved_session_of_an_unmanaged_kind_is_left_alone() {
        let mut fake = Fake::new("unmanaged").pane("w1-2", Spec::saved("/proj"));
        fake.saved_kind = "mystery".into();
        let report = run(&fake);
        assert_eq!(
            report.skipped,
            [(
                "w1-2".to_string(),
                "no profile for kind mystery".to_string()
            )]
        );
        assert!(fake.runs().is_empty());
        assert!(!Registry::load(&fake.state_dir).panes.contains_key("w1-2"));
    }

    #[test]
    fn a_pane_that_is_late_gets_a_second_pass() {
        // Not released within one wait, released within two.
        let late = Spec {
            released_at: Some(Duration::from_secs(90)),
            ..Spec::ready()
        };
        let fake = Fake::new("second-pass")
            .pane("w1-2", late)
            .pane("w1-3", Spec::ready());
        let report = restore(&fake, &config_with_timeout(60));
        assert_eq!(report.restored, ["w1-3", "w1-2"]);
        assert!(report.skipped.is_empty());
        assert!(
            fake.journal()
                .contains(&"RESTORE-RETRY w1-2 identity-not-released".to_string())
        );
    }

    #[test]
    fn the_wait_is_the_configured_one() {
        let held = Spec {
            released_at: None,
            ..Spec::ready()
        };
        let fake = Fake::new("timeout").pane("w1-2", held);
        restore(&fake, &config_with_timeout(7));
        assert!(fake.clock() >= Duration::from_secs(14));
        assert!(fake.clock() < Duration::from_secs(20));
    }

    #[test]
    fn a_pane_whose_placeholder_started_is_not_run_again() {
        let silent = Spec {
            announce_after: None,
            ..Spec::ready()
        };
        let fake = Fake::new("no-double-exec").pane("w1-2", silent);
        run(&fake);
        assert_eq!(fake.runs().len(), 1);
    }

    /// A Herdr restart with `resume_agents_on_restore = false`: agents that
    /// slept (registry), agents that were awake (Saved sessions only), slow
    /// identities, and a directory that disappeared. Every pane that can
    /// ends asleep and the rest is accounted for.
    #[test]
    fn a_restart_puts_every_pane_back_to_sleep() {
        let mut fake = Fake::new("restart");
        for i in 0..12u64 {
            let released_at = Some(Duration::from_secs(if i % 3 == 0 { 0 } else { 8 * i }));
            let registered = Spec {
                released_at,
                ..Spec::ready()
            };
            let saved = Spec {
                released_at,
                ..Spec::saved("/proj")
            };
            fake = fake
                .pane(&format!("w{i}A:p1"), registered)
                .pane(&format!("w{i}B:pC"), saved);
        }
        fake = fake.pane("g-00", Spec::saved("/gone"));
        fake.missing_dirs = vec!["/gone".into()];
        let report = restore(&fake, &config_with_timeout(60));
        assert_eq!(report.restored.len(), 24, "{report:?}");
        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(
            report.missing_cwd,
            [("g-00".to_string(), "/gone".to_string())]
        );
        // Every restored pane got exactly one Placeholder.
        assert_eq!(fake.runs().len(), 24);
        // Every awake agent was started again with its own session, under a
        // name Herdr accepts.
        let started = starts(&fake);
        assert_eq!(started.len(), 12, "{started:?}");
        for i in 0..12 {
            let pane = format!("w{i}B:pC");
            assert!(
                started
                    .iter()
                    .any(|c| c.ends_with(&format!(" {pane} --resume sess-{pane}"))),
                "{pane}: {started:?}"
            );
        }
        assert!(started.iter().all(|c| {
            let name = c.split(' ').nth(1).unwrap();
            valid_agent_name(name)
        }));
        // Every pane is in the registry for the next restart.
        assert_eq!(Registry::load(&fake.state_dir).panes.len(), 24);
        assert_eq!(
            fake.journal().last().unwrap(),
            "RESTORE-DONE restored=24 skipped=0 missing_cwd=1"
        );
        assert_eq!(RestoreReport::load(&fake.state_dir).as_ref(), Some(&report));
    }

    #[test]
    fn sleep_from_a_saved_session_puts_the_placeholder_in_place() {
        let fake = Fake::new("from-session").pane("w1-2", Spec::saved("/proj"));
        let outcome = sleep_saved_session(&fake, &Config::default(), "w1-2");
        assert_eq!(outcome, SleepOutcome::Asleep { freed_kb: 0 });
        assert_eq!(fake.runs().len(), 1);
        assert!(Registry::load(&fake.state_dir).panes.contains_key("w1-2"));
    }

    #[test]
    fn sleep_from_a_session_explains_each_refusal() {
        let mut fake = Fake::new("explain")
            .pane("w1-2", Spec::ready())
            .pane("w1-3", Spec::saved("/gone"));
        fake.missing_dirs = vec!["/gone".into()];
        // Released: no agent and no Saved session left.
        fake.elapsed.set(Duration::from_secs(1));
        assert_eq!(
            sleep_saved_session(&fake, &Config::default(), "w1-2"),
            SleepOutcome::Skipped(Refusal::NoSession)
        );
        let SleepOutcome::Aborted(reason) = sleep_saved_session(&fake, &Config::default(), "w1-3")
        else {
            panic!("expected an abort");
        };
        assert!(
            reason.contains("working directory is gone (/gone)"),
            "{reason}"
        );
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn sleep_from_a_session_reports_a_pane_that_never_gets_to_its_prompt() {
        let busy = Spec {
            shell_at: None,
            ..Spec::saved("/proj")
        };
        let fake = Fake::new("busy-session").pane("w1-2", busy);
        assert_eq!(
            sleep_saved_session(&fake, &Config::default(), "w1-2"),
            SleepOutcome::Aborted("pane-busy".into())
        );
    }

    fn record(fake: &Fake, pane: &str, session: &str, name: Option<&str>) {
        let mut live = LiveAgents::default();
        live.panes.insert(
            pane.into(),
            Sleeper {
                kind: "claude".into(),
                session: session.into(),
                name: name.map(Into::into),
                cwd: "/proj".into(),
                since_ms: 1,
            },
        );
        live.save(&fake.state_dir).unwrap();
    }

    #[test]
    fn an_agent_herdr_holds_no_session_for_comes_back_from_the_live_record() {
        let fake = Fake::new("recorded").pane(
            "w1-2",
            Spec {
                no_session: true,
                ..Spec::saved("/proj")
            },
        );
        record(&fake, "w1-2", "sess-rec", Some("cursor-orch"));
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2"]);
        let started = starts(&fake);
        assert_eq!(started.len(), 1);
        assert!(
            started[0].starts_with("start cursor-orch claude w1-2 "),
            "{started:?}"
        );
        assert!(started[0].ends_with("--resume sess-rec"), "{started:?}");
        assert!(
            fake.journal()[0].ends_with("registered=0 saved=0 recorded=1"),
            "{:?}",
            fake.journal()
        );
    }

    #[test]
    fn a_record_older_than_the_shell_is_resumed_and_a_newer_one_is_not() {
        for (started, restored) in [(Some(10_000), true), (Some(0), false), (None, true)] {
            let mut fake = Fake::new("record-age").pane(
                "w1-2",
                Spec {
                    no_session: true,
                    ..Spec::saved("/proj")
                },
            );
            fake.shell_started = started;
            record(&fake, "w1-2", "sess-rec", Some("n")); // seen at 1 ms
            fake.shell_started = started.map(|s| if s == 0 { 0 } else { s });
            let report = run(&fake);
            assert_eq!(report.restored.len() == 1, restored, "{started:?}");
        }
    }

    #[test]
    fn a_record_does_not_resume_a_pane_that_shows_an_agent_or_no_record_exists() {
        let fake = Fake::new("no-record").pane(
            "w1-2",
            Spec {
                no_session: true,
                ..Spec::saved("/proj")
            },
        );
        let report = run(&fake);
        assert!(report.restored.is_empty());
        assert!(starts(&fake).is_empty());
    }

    #[test]
    fn a_saved_session_gets_its_name_back_from_the_record() {
        let mut fake = Fake::new("name").pane("w1-2", Spec::saved("/proj"));
        fake.saved_name = None;
        record(&fake, "w1-2", "sess-w1-2", Some("kept-name"));
        run(&fake);
        assert!(starts(&fake)[0].starts_with("start kept-name claude w1-2 "));
    }

    #[test]
    fn a_session_that_never_saved_a_conversation_starts_fresh() {
        let mut fake = Fake::new("fresh").pane("w1-2", Spec::saved("/proj"));
        fake.unsaved.push("sess-w1-2".into());
        run(&fake);
        let started = starts(&fake);
        assert_eq!(started.len(), 1);
        assert!(!started[0].contains("--resume"), "{started:?}");
        assert!(
            fake.journal()
                .iter()
                .any(|l| l.contains("RESTORE-FRESH w1-2")),
            "{:?}",
            fake.journal()
        );
    }

    fn starts(fake: &Fake) -> Vec<String> {
        fake.calls()
            .into_iter()
            .filter(|c| c.starts_with("start "))
            .collect()
    }

    #[test]
    fn a_saved_session_is_resumed_with_its_session_then_put_to_sleep() {
        let fake = Fake::new("resume").pane("w1-2", Spec::saved("/proj"));
        run(&fake);
        assert_eq!(
            starts(&fake),
            ["start worker claude w1-2 --resume sess-w1-2"]
        );
        // Typed exit command, then the Placeholder.
        let calls = fake.calls();
        let start = calls.iter().position(|c| c.starts_with("start ")).unwrap();
        let exit = calls
            .iter()
            .position(|c| c.starts_with("text w1-2 "))
            .unwrap();
        let exec = calls
            .iter()
            .position(|c| c.starts_with("run w1-2 "))
            .unwrap();
        assert!(start < exit && exit < exec, "{calls:?}");
        assert!(
            fake.journal()
                .contains(&"RESTORE-RESUME w1-2 kind=claude".to_string())
        );
    }

    #[test]
    fn the_generated_name_is_one_herdr_accepts() {
        let mut fake = Fake::new("name").pane("w2A:p3", Spec::saved("/proj"));
        fake.saved_name = None;
        let report = run(&fake);
        assert_eq!(report.restored, ["w2A:p3"]);
        assert_eq!(
            starts(&fake),
            ["start claude-w2a_p3 claude w2A:p3 --resume sess-w2A:p3"]
        );
    }

    #[test]
    fn a_kept_name_herdr_would_refuse_is_replaced() {
        let mut fake = Fake::new("bad-name").pane("w1-2", Spec::saved("/proj"));
        fake.saved_name = Some("Worker One".into());
        run(&fake);
        assert_eq!(starts(&fake).len(), 1);
        assert!(starts(&fake)[0].starts_with("start claude-w1-2 "));
        assert!(valid_agent_name("worker_1-b"));
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name("1abc"));
        assert!(!valid_agent_name("Abc"));
        assert!(!valid_agent_name(&"a".repeat(33)));
    }

    #[test]
    fn an_agent_that_started_despite_a_failure_is_put_to_sleep() {
        let mut fake = Fake::new("started-anyway").pane("w1-2", Spec::saved("/proj"));
        fake.start_fails_but_runs = true;
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2"]);
        assert_eq!(starts(&fake).len(), 1);
        assert!(
            fake.journal()
                .iter()
                .any(|l| l.starts_with("WARN w1-2: the agent runs"))
        );
    }

    #[test]
    fn a_pane_that_is_not_ready_never_holds_the_others_back() {
        let stuck = Spec {
            released_at: None,
            ..Spec::ready()
        };
        let fake = Fake::new("no-blocking")
            .pane("a-stuck", stuck)
            .pane("b-ready", Spec::ready())
            .pane("c-saved", Spec::saved("/proj"));
        let report = restore(&fake, &config_with_timeout(60));
        assert_eq!(report.restored, ["b-ready", "c-saved"]);
        assert_eq!(report.skipped.len(), 1);
        // The ready panes were done within seconds, not after a full wait.
        let at = fake.exec_at.borrow();
        assert!(at["b-ready"] < Duration::from_secs(5), "{at:?}");
        assert!(at["c-saved"] < Duration::from_secs(10), "{at:?}");
    }

    #[test]
    fn a_registered_pane_stuck_on_a_saved_session_is_resumed_then_put_to_sleep() {
        let held = Spec {
            session_only: true,
            ..Spec::ready()
        };
        let fake = Fake::new("session-held").pane("w1-2", held);
        let report = restore(&fake, &config_with_timeout(10));
        assert_eq!(report.restored, ["w1-2"], "{report:?}");
        assert_eq!(starts(&fake), ["start worker claude w1-2 --resume sess-1"]);
        assert!(fake.runs().len() == 1);
        assert!(
            fake.journal()
                .contains(&"RESTORE-RETRY w1-2 session-held".to_string())
        );
    }

    #[test]
    fn a_sleeping_pane_the_registry_does_not_know_is_journaled() {
        // Herdr shows 'sleeping-claude' but nothing records its session: it
        // cannot be restored, and the journal says so instead of staying silent.
        let mut fake = Fake::new("unknown-sleeper").pane("w1-2", Spec::ready());
        fake.unknown_sleepers = vec!["w1-9".into()];
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2"]);
        assert!(report.skipped.contains(&(
            "w1-9".to_string(),
            "sleeping-agent-not-registered".to_string()
        )));
        assert!(
            fake.journal()
                .contains(&"RESTORE-SKIP w1-9 sleeping-agent-not-registered".to_string())
        );
    }

    #[test]
    fn a_registered_pane_that_already_sleeps_is_left_alone_without_a_word() {
        let mut fake = Fake::new("second-restore").pane("w1-2", Spec::ready());
        fake.register("w1-9", "claude", "sess-9", None, "/work");
        fake.unknown_sleepers = vec!["w1-9".into()];
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    }

    #[test]
    fn a_second_restore_waits_for_nothing_and_types_nothing() {
        let fake = Fake::new("two-restores").pane("w1-2", Spec::ready());
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        let _first = PaneLock::try_acquire_for(&fake.state_dir, "all", "restore")
            .unwrap()
            .unwrap();
        let report = run(&fake);
        assert_eq!(report.error.as_deref(), Some("another restore is running"));
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn every_pane_the_restore_could_not_use_is_in_the_journal() {
        let fake = Fake::new("journal-all").pane("w1-2", Spec::ready());
        fake.register("w9-9", "claude", "sess-9", None, "/work");
        let report = run(&fake);
        assert_eq!(report.restored, ["w1-2"]);
        let journal = fake.journal();
        assert!(
            journal.contains(&"RESTORE-SKIP w9-9 pane-gone".to_string()),
            "{journal:?}"
        );
    }

    #[test]
    fn a_resume_that_fails_is_retried_then_left_alone() {
        let mut fake = Fake::new("start-fails").pane("w1-2", Spec::saved("/proj"));
        fake.start_fails = true;
        let report = run(&fake);
        assert!(report.restored.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(
            report.skipped[0]
                .1
                .starts_with("cannot resume the agent: agent not ready"),
            "{report:?}"
        );
        assert_eq!(starts(&fake).len(), 2);
    }

    #[test]
    fn a_resumed_agent_a_guard_keeps_awake_is_reported_not_retried() {
        let mut fake = Fake::new("guard").pane("w1-2", Spec::saved("/proj"));
        fake.guard = Some(Refusal::DraftInPrompt);
        let report = run(&fake);
        assert!(report.restored.is_empty());
        assert!(
            report.skipped[0].1.starts_with("resumed but still awake"),
            "{report:?}"
        );
        assert_eq!(starts(&fake).len(), 1);
        assert!(fake.runs().is_empty());
    }

    #[test]
    fn exclusion_regexes_never_block_the_return_of_a_registered_pane() {
        let fake = Fake::new("excluded-registry").pane("w1-2", Spec::ready());
        let config = Config::from_toml_str(
            "exclude_name_regex = [\".\"]\nexclude_screen_regex = [\".\"]\nexclude_cmdline_regex = [\".\"]",
        )
        .unwrap();
        let report = restore(&fake, &config);
        assert_eq!(report.restored, ["w1-2"]);
        assert_eq!(fake.runs().len(), 1);
    }

    #[test]
    fn a_resumed_agent_with_an_excluded_name_stays_awake_and_says_why() {
        let mut fake = Fake::new("excluded-saved").pane("w1-2", Spec::saved("/proj"));
        fake.guard = Some(Refusal::NameExcluded("deploy".into()));
        let report = run(&fake);
        assert_eq!(starts(&fake).len(), 1);
        assert!(
            report.skipped[0].1.contains("matches exclude_name_regex"),
            "{report:?}"
        );
    }

    #[test]
    fn dry_run_never_resumes_an_agent() {
        let fake = Fake::new("dry").pane("w1-2", Spec::saved("/proj"));
        let config = Config {
            dry_run: true,
            ..Config::default()
        };
        let report = restore(&fake, &config);
        assert_eq!(report.skipped.len(), 1);
        assert!(starts(&fake).is_empty());
        assert!(fake.runs().is_empty());
        assert!(
            fake.journal()
                .iter()
                .any(|l| l.contains("RESTORE-DRY-RUN w1-2 kind=claude source=saved-session"))
        );
    }

    #[test]
    fn only_one_agent_runs_at_a_time() {
        let mut fake = Fake::new("one-at-a-time");
        for i in 0..4 {
            fake = fake.pane(&format!("s-{i}"), Spec::saved("/proj"));
        }
        run(&fake);
        let calls = fake.calls();
        let mut running = 0;
        for call in &calls {
            if call.starts_with("start ") {
                running += 1;
            } else if call.starts_with("run ") {
                running -= 1;
            }
            assert!(running <= 1, "{calls:?}");
        }
        assert_eq!(starts(&fake).len(), 4);
    }
}

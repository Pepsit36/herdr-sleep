//! The Sleep operation: quit an Agent through its own exit command, wait for
//! Herdr to forget the native agent, then replace the pane's shell with the
//! Placeholder (ADR 0002, 0003, 0005).
//!
//! The sequence is the riskiest step of the plugin, because a mistake leaves
//! a pane in a half state. Each stage therefore has a bounded wait, a stable
//! journal event and an outcome that leaves the pane untouched or
//! recoverable. No signal is ever sent: an agent that does not quit is left
//! running and the next Sweep retries.
//!
//! Journal events: `SKIP`, `DRY-RUN`, `SLEEP`, `ABORT`, `ASLEEP`, `WARN`.
//!
//! Once the Sleeping agent is announced (or the Placeholder started without
//! announcing), the pane is added to the registry so that `restore` can bring
//! it back after a Herdr restart (ADR 0008).
//!
//! Markers (sleep icon on the pane, tab and workspace) are synchronised by
//! the caller, not here.

use std::path::Path;
use std::time::Duration;

use crate::config::{Config, Profile};
use crate::guards::{self, Refusal, SleepMode};
use crate::herdr::Agent;
use crate::journal::Journal;

use super::Env;
use super::markers::sync_markers;
use super::pane_lock::PaneLock;
use super::registry::{self, Registry, Sleeper};

/// Pause between typing the exit command and pressing Enter, so an
/// autocomplete menu opened by the command can settle.
const KEY_DELAY: Duration = Duration::from_millis(500);
/// Poll interval while waiting for the agent to quit.
const QUIT_POLL: Duration = Duration::from_millis(500);
/// A second Enter is sent when the agent is still there after this long.
const SECOND_ENTER_AFTER: Duration = Duration::from_secs(5);
/// Longest wait for the agent to quit.
const QUIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Poll interval while waiting for Herdr to release the native identity.
const RELEASE_POLL: Duration = Duration::from_millis(250);
/// Longest wait for Herdr to release the native identity.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while waiting for the Placeholder to report.
const SLEEPING_POLL: Duration = Duration::from_millis(300);
/// Longest wait for the Placeholder to report.
const SLEEPING_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while another Sleep holds the pane.
const LOCK_POLL: Duration = Duration::from_millis(250);
/// Longest wait of a manual Sleep for the one holding the pane.
const LOCK_WAIT: Duration = Duration::from_secs(60);
/// Title of the notification shown when an agent went to sleep.
pub const NOTIFICATION_TITLE: &str = "Herdr Sleep";

/// How a Sleep attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SleepOutcome {
    /// The pane now holds a Placeholder. `freed_kb` is the resident memory
    /// of the agent process tree before it quit, `0` when unknown.
    Asleep { freed_kb: u64 },
    /// A Guard forbids sleep.
    Skipped(Refusal),
    /// `dry_run` is on: the agent would have been put to sleep.
    DryRun,
    /// The attempt was abandoned; the pane is untouched or at a plain shell.
    Aborted(String),
    /// The Placeholder was started but never announced itself.
    Warned(String),
}

/// Pause between two attempts of [`sleep_agent_waiting`].
const WAIT_POLL: Duration = Duration::from_secs(2);

/// Whether a refusal may go away by itself: the agent is still working, or
/// has just started and shows no prompt or session yet.
fn is_transient(refusal: &Refusal) -> bool {
    matches!(
        refusal,
        Refusal::NotSettled(_)
            | Refusal::NoSession
            | Refusal::NoConversation
            | Refusal::PromptNotDetected
            | Refusal::TabUnreadable
    )
}

/// [`sleep_agent`], tried again every few seconds for up to `timeout` while
/// the refusal is one that may go away by itself (see [`is_transient`]). The
/// outcome is the last one.
pub fn sleep_agent_waiting(
    env: &dyn Env,
    config: &Config,
    agent: &Agent,
    mode: SleepMode,
    timeout: Duration,
) -> SleepOutcome {
    let start = env.now();
    let mut current = agent.clone();
    loop {
        // Wait for the status first: the refusal of every attempt would be
        // journaled.
        while mode != SleepMode::Force
            && !config.allows_status(current.agent_status)
            && env.now().duration_since(start) < timeout
        {
            env.sleep(WAIT_POLL);
            if let Ok(fresh) = env.agent(&agent.pane_id) {
                current = fresh;
            }
        }
        let outcome = sleep_agent(env, config, &current, mode, None);
        let SleepOutcome::Skipped(refusal) = &outcome else {
            return outcome;
        };
        if !is_transient(refusal) || env.now().duration_since(start) >= timeout {
            return outcome;
        }
        env.sleep(WAIT_POLL);
        if let Ok(fresh) = env.agent(&agent.pane_id) {
            current = fresh;
        }
    }
}

/// Put `agent` to sleep.
///
/// `last_skip` is the code of the refusal the previous Sweep logged for this
/// agent, so a Sweep does not repeat the same line every minute.
pub fn sleep_agent(
    env: &dyn Env,
    config: &Config,
    agent: &Agent,
    mode: SleepMode,
    last_skip: Option<&str>,
) -> SleepOutcome {
    let Some(state_dir) = env.state_dir() else {
        return SleepOutcome::Aborted("no state directory".into());
    };
    if let Err(error) = std::fs::create_dir_all(&state_dir) {
        return SleepOutcome::Aborted(format!(
            "cannot create the state directory {}: {error}",
            state_dir.display()
        ));
    }
    let journal = Journal::new(&state_dir);
    let pane = agent.pane_id.as_str();
    let label = agent.name.as_deref().unwrap_or(pane);

    // 0. One Sleep at a time per pane, and only on the pane as it is now: the
    // caller read `agent` earlier, and another Sleep may have put it to sleep
    // since. Typing the Placeholder command a second time would hand it to the
    // Placeholder, which replays it into the Agent at the next Wake.
    // A manual Sleep waits for the one holding the pane, so that it reports
    // the real outcome (already asleep, refused by a guard) and not a bare
    // "superseded"; a Sweep just leaves the pane to it.
    let mut waited = Duration::ZERO;
    let _lock = loop {
        match PaneLock::try_acquire(&state_dir, pane) {
            Ok(Some(lock)) => break lock,
            Ok(None) if mode != SleepMode::Sweep && waited < LOCK_WAIT => {
                env.sleep(LOCK_POLL);
                waited += LOCK_POLL;
                continue;
            }
            Ok(None) => return SleepOutcome::Skipped(Refusal::Superseded),
            Err(error) => {
                journal.log(
                    "WARN",
                    &format!("{label} ({pane}): cannot lock the pane: {error}"),
                );
                return SleepOutcome::Aborted(format!("cannot lock the pane: {error}"));
            }
        }
    };
    // From here on the agent is the one Herdr reports now: the caller read
    // its copy earlier, and a Sweep sleeps its agents one after the other,
    // seconds apart, so its status and focus may have changed since.
    let current = match env.agent(pane) {
        Ok(current) if !current.is_sleeping() && current.session_id() == agent.session_id() => {
            current
        }
        _ => return SleepOutcome::Skipped(Refusal::Superseded),
    };
    let agent = &current;

    // 1. Guards.
    if let Err(refusal) = env.check_guards(config, agent, mode) {
        let repeated = mode == SleepMode::Sweep && last_skip == Some(refusal.code());
        if !refusal.is_silent() && !repeated {
            journal.log("SKIP", &format!("{label} ({pane}): {refusal}"));
        }
        return SleepOutcome::Skipped(refusal);
    }
    let abort = |reason: String| {
        journal.log("ABORT", &format!("{label} ({pane}): {reason}"));
        SleepOutcome::Aborted(reason)
    };

    let Some(kind) = agent.kind() else {
        return abort("agent has no kind".into());
    };
    let Some(profile) = config.profile(kind) else {
        return abort(format!("no profile for kind {kind}"));
    };
    let Some(session) = agent.session_id() else {
        return abort("no session id".into());
    };

    // 2. Dry run.
    if config.dry_run && mode != SleepMode::Force {
        journal.log("DRY-RUN", &format!("{label} ({pane}) would sleep"));
        return SleepOutcome::DryRun;
    }

    // The Placeholder must exist before anything is typed into the agent.
    let placeholder = match env.placeholder_bin() {
        Ok(path) => path,
        Err(error) => return abort(format!("placeholder unavailable: {error}")),
    };

    // The Placeholder command is typed into the pane's shell. Refuse values a
    // shell could read differently from a single word (control characters
    // split or end a command line, and fish treats a backslash as an escape
    // even between single quotes) before the agent is touched.
    let cwd_value = agent.effective_cwd().unwrap_or(".");
    let state_dir_text = state_dir.to_string_lossy();
    let main_bin_path = env.main_bin();
    let herdr_bin_path = env.herdr_bin();
    let main_bin_text = main_bin_path.to_string_lossy();
    let herdr_bin_text = herdr_bin_path.to_string_lossy();
    let placeholder_text = placeholder.to_string_lossy();
    let early_words = [
        pane,
        kind,
        session,
        cwd_value,
        config.sleep_icon.as_str(),
        config.token_name.as_str(),
        state_dir_text.as_ref(),
        main_bin_text.as_ref(),
        herdr_bin_text.as_ref(),
        placeholder_text.as_ref(),
    ];
    let named = agent.name.as_deref().into_iter();
    let kept_args = env
        .agent_command(agent, &profile.bin)
        .map(|command| profile.kept_args(&command))
        .unwrap_or_default();
    let resume = profile.resume_argv(session, &kept_args);
    if let Some(word) = early_words
        .iter()
        .copied()
        .chain(named)
        .chain(resume.iter().map(String::as_str))
        .find(|word| !is_typeable(word))
    {
        return abort(format!(
            "cannot type {word:?} safely into a shell (control character or backslash)"
        ));
    }

    // 3. Preview and memory figure, both best effort.
    let preview = write_preview(env, config, profile, pane, &state_dir);
    let freed_kb = env.agent_rss_kb(agent, &profile.bin).unwrap_or(0);

    // 3b. A draft in the prompt is saved and the prompt cleared, so the exit
    // command is typed into an empty prompt. The draft is typed back at Wake.
    let draft = if profile.sleep_with_draft {
        match take_draft(env, profile, pane, &state_dir) {
            Ok(draft) => draft,
            Err(reason) => return abort(reason),
        }
    } else {
        None
    };
    if let Some(text) = &draft {
        journal.log("DRAFT", &format!("{pane} saved bytes={}", text.len()));
    }
    let undo = |typed_exit: bool| {
        undo_typing(
            env,
            pane,
            &state_dir,
            typed_exit.then_some(profile.exit.as_str()),
            draft.as_deref(),
        );
    };

    // 4. Quit the agent through its exit command. The pane is registered
    // first: an agent that takes longer than the wait to quit leaves a bare
    // shell behind, and the entry is what lets `restore` put the Placeholder
    // there. It is withdrawn when the agent is still running after an abandon.
    let cwd = agent.effective_cwd().unwrap_or(".");
    let entry = Entry {
        kind,
        session,
        name: agent.name.as_deref(),
        cwd,
        args: &kept_args,
    };
    journal.log(
        "SLEEP",
        &format!("{label} ({pane}) kind={kind} session={session}"),
    );
    register(env, &journal, pane, &entry);
    // A Wake of the last fifteen seconds must not stop the next one, and a
    // stamp left from an earlier Wake would make the Sweep take this pane's
    // entry for a stale one.
    let _ = std::fs::remove_file(super::wake::wake_stamp_path(&state_dir, pane));
    let withdraw = || {
        if let Err(error) = Registry::remove(&state_dir, pane) {
            journal.log(
                "WARN",
                &format!("{pane}: cannot update {}: {error}", registry::FILE_NAME),
            );
        }
    };
    if let Err(error) = env.send_text(pane, &profile.exit) {
        undo(false);
        withdraw();
        return abort(format!("cannot type the exit command: {error}"));
    }
    env.sleep(KEY_DELAY);
    if let Err(error) = env.send_keys(pane, &["enter"]) {
        undo(true);
        withdraw();
        return abort(format!("cannot press Enter: {error}"));
    }
    let mut second_enter = false;
    let quit = wait_until(env, QUIT_TIMEOUT, QUIT_POLL, |elapsed| {
        if env.process_info(pane).is_ok_and(|info| info.at_shell()) {
            return true;
        }
        if !second_enter && elapsed >= SECOND_ENTER_AFTER {
            second_enter = true;
            let _ = env.send_keys(pane, &["enter"]);
        }
        false
    });
    if !quit {
        undo(true);
        // The agent may have quit while the keys were being undone: then the
        // entry stays, for the pane is a plain shell that only `restore` can
        // turn back into a Sleeping agent.
        if env.process_info(pane).is_ok_and(|info| !info.at_shell()) {
            withdraw();
        } else {
            journal.log(
                "WARN",
                &format!("{label} ({pane}): the agent quit after the sleep was abandoned"),
            );
        }
        return abort(format!(
            "agent still running after {}s",
            QUIT_TIMEOUT.as_secs()
        ));
    }

    // 5. Herdr must release its native identity, or the Placeholder's
    // reports are ignored.
    let released = wait_until(env, RELEASE_TIMEOUT, RELEASE_POLL, |_| {
        env.pane(pane)
            .is_ok_and(|p| !p.has_agent() && p.agent_session.is_none())
    });
    if !released {
        return abort("identity not released".into());
    }

    // 6 and 7. Replace the shell with the Placeholder.
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
    let argv = placeholder_argv(
        &placeholder,
        &PlaceholderParams {
            pane,
            name: agent.name.as_deref(),
            kind,
            session,
            state_dir: &state_dir,
            main_bin: &main_bin,
            herdr_bin: &herdr_bin,
            shell: &shell,
            icon: &config.sleep_icon,
            token: &config.token_name,
            cwd,
            preview: preview.as_deref(),
            resume: &resume,
        },
    );
    // `pane run` types the words into the pane's shell, so each one is quoted
    // for a POSIX shell (single quotes work in fish as well).
    if let Some(word) = argv.iter().find(|word| !is_typeable(word)) {
        return abort(format!(
            "cannot type {word:?} safely into a shell (control character or backslash)"
        ));
    }
    // Last look before typing: a Placeholder already in the pane would keep
    // the command as Captured input.
    if env.agent(pane).is_ok_and(|current| current.is_sleeping()) {
        return abort("the pane already holds a placeholder".into());
    }
    // Whatever the user typed while the agent was quitting would be glued to
    // the command line: clear the line first.
    let _ = env.send_keys(pane, &["ctrl+u"]);
    let quoted: Vec<String> = argv.iter().map(|word| shell_quote(word)).collect();
    let mut command: Vec<&str> = vec!["exec"];
    command.extend(quoted.iter().map(String::as_str));
    if let Err(error) = env.run(pane, &command) {
        return abort(format!("cannot start the placeholder: {error}"));
    }

    // 8. Wait for the Sleeping agent.
    let announced = wait_until(env, SLEEPING_TIMEOUT, SLEEPING_POLL, |_| {
        env.agent(pane)
            .is_ok_and(|a| a.is_sleeping() && a.kind() == Some(kind))
    });
    if !announced {
        let reason = format!("placeholder did not report sleeping-{kind}");
        journal.log("WARN", &format!("{label} ({pane}): {reason}"));
        // The Placeholder runs, only its report is missing: it may still be
        // restored after a restart.
        register(env, &journal, pane, &entry);
        sync_markers(env, config);
        return SleepOutcome::Warned(reason);
    }
    // The entry written before the exit is refreshed with the time of the
    // announce.
    register(env, &journal, pane, &entry);
    let freed_mb = freed_kb / 1024;
    journal.log(
        "ASLEEP",
        &format!("{label} ({pane}) kind={kind} freed={freed_mb}MB"),
    );
    if config.notify && mode == SleepMode::Sweep {
        let _ = env.notify(
            NOTIFICATION_TITLE,
            &notification_text(&config.sleep_icon, label, kind, freed_mb),
        );
    }
    sync_markers(env, config);
    SleepOutcome::Asleep { freed_kb }
}

/// Record the sleeping pane in the registry. A failure is journaled and does
/// not change the outcome of the Sleep.
/// What the registry records about the agent that goes to sleep.
struct Entry<'a> {
    kind: &'a str,
    session: &'a str,
    name: Option<&'a str>,
    cwd: &'a str,
    args: &'a [String],
}

fn register(env: &dyn Env, journal: &Journal, pane: &str, entry: &Entry<'_>) {
    let Some(state_dir) = env.state_dir() else {
        return;
    };
    let sleeper = Sleeper {
        kind: entry.kind.to_string(),
        session: entry.session.to_string(),
        name: entry.name.filter(|n| !n.is_empty()).map(str::to_string),
        cwd: entry.cwd.to_string(),
        args: entry.args.to_vec(),
        since_ms: env.now_millis(),
    };
    if let Err(error) = Registry::add(&state_dir, pane, sleeper) {
        journal.log(
            "WARN",
            &format!("{pane}: cannot save {}: {error}", registry::FILE_NAME),
        );
    }
}

/// `<icon> <name or pane> (<kind>) went to sleep, freed <N> MB`, without the
/// memory part when it is unknown.
fn notification_text(icon: &str, label: &str, kind: &str, freed_mb: u64) -> String {
    let mut text = format!("{icon} {label} ({kind}) went to sleep");
    if freed_mb > 0 {
        text.push_str(&format!(", freed {freed_mb} MB"));
    }
    text
}

/// The file holding the draft saved at Sleep, typed back at Wake.
pub(super) fn draft_path(state_dir: &Path, pane: &str) -> std::path::PathBuf {
    state_dir.join(format!("draft-{}.txt", super::wake::sanitize_pane(pane)))
}

/// Save the draft of the prompt in [`draft_path`] and clear the prompt (Ctrl+U),
/// checking on screen that it is empty. `Ok(None)` when there is no draft. An
/// error leaves the draft where it was: nothing is lost.
fn take_draft(
    env: &dyn Env,
    profile: &Profile,
    pane: &str,
    state_dir: &Path,
) -> Result<Option<String>, String> {
    let path = draft_path(state_dir, pane);
    let screen = env
        .screen_ansi(pane, guards::SCREEN_LINES)
        .map_err(|error| format!("cannot read the screen: {error}"))?;
    let text = match guards::read_draft(&screen, profile) {
        None => {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        Some(guards::DraftRead::Unreadable) => {
            return Err("the draft in the prompt cannot be read reliably, so it is kept".into());
        }
        Some(guards::DraftRead::Text(text)) => text,
    };
    std::fs::write(&path, &text).map_err(|error| format!("cannot save the draft: {error}"))?;
    if env.send_keys(pane, &["ctrl+u"]).is_err() {
        // Nothing was erased: the draft is still in the prompt.
        let _ = std::fs::remove_file(&path);
        return Err("cannot clear the draft from the prompt, so it is kept".into());
    }
    env.sleep(KEY_DELAY);
    match env.screen_ansi(pane, guards::SCREEN_LINES) {
        Ok(screen) if guards::prompt_state(&screen, profile) == guards::PromptState::Empty => {}
        Ok(_) => {
            // The prompt did not empty: the draft is still there.
            let _ = std::fs::remove_file(&path);
            return Err("cannot clear the draft from the prompt, so it is kept".into());
        }
        Err(error) => {
            // Ctrl+U was sent and the result cannot be seen: the draft may be
            // gone from the prompt, so the file stays.
            return Err(format!(
                "cannot tell whether the draft was cleared ({error}); it is saved in {}",
                path.display()
            ));
        }
    }
    Ok(Some(text))
}

/// After a Sleep that failed: erase the exit command typed in the prompt
/// (one Backspace per character) and type the saved draft back.
fn undo_typing(
    env: &dyn Env,
    pane: &str,
    state_dir: &Path,
    exit: Option<&str>,
    draft: Option<&str>,
) {
    if let Some(exit) = exit {
        let backspaces = vec!["backspace"; exit.chars().count()];
        let _ = env.send_keys(pane, &backspaces);
    }
    if let Some(text) = draft {
        // The file goes only once the text is back in the prompt.
        if env.send_text(pane, text).is_ok() {
            let _ = std::fs::remove_file(draft_path(state_dir, pane));
        }
    }
}

/// Capture the Preview into `<state_dir>/preview-<pane>.txt`: the last
/// `preview_lines` lines above the input box, or of the whole screen when no
/// frame is found. A failure only drops the preview.
fn write_preview(
    env: &dyn Env,
    config: &Config,
    profile: &Profile,
    pane: &str,
    state_dir: &Path,
) -> Option<String> {
    let wanted = usize::try_from(config.preview_lines).ok()?;
    if wanted == 0 {
        return None;
    }
    let screen = env
        .screen_text(pane, guards::SCREEN_LINES.max(wanted))
        .ok()?;
    let lines: Vec<&str> = match guards::above_frame(&screen, profile) {
        Some(above) => above,
        None => screen.lines().collect(),
    };
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    let lines = &lines[lines.len().saturating_sub(wanted)..];
    let first = lines.iter().position(|l| !l.is_empty())?;
    let last = lines.iter().rposition(|l| !l.is_empty())?;
    let text = lines[first..=last].join("\n");
    let safe = super::wake::sanitize_pane(pane);
    let path = state_dir.join(format!("preview-{safe}.txt"));
    std::fs::write(&path, text).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Call `done` with the time elapsed since the start until it returns true or
/// `timeout` has passed, pausing `poll` between calls. The condition is
/// checked once more at the deadline.
pub(super) fn wait_until(
    env: &dyn Env,
    timeout: Duration,
    poll: Duration,
    mut done: impl FnMut(Duration) -> bool,
) -> bool {
    let start = env.now();
    loop {
        let elapsed = env.now().duration_since(start);
        if done(elapsed) {
            return true;
        }
        if elapsed >= timeout {
            return false;
        }
        env.sleep(poll);
    }
}

/// What the Placeholder is told on its command line.
#[derive(Debug, Clone)]
pub struct PlaceholderParams<'a> {
    pub pane: &'a str,
    pub name: Option<&'a str>,
    pub kind: &'a str,
    pub session: &'a str,
    pub state_dir: &'a Path,
    pub main_bin: &'a Path,
    pub herdr_bin: &'a Path,
    pub shell: &'a Path,
    pub icon: &'a str,
    pub token: &'a str,
    pub cwd: &'a str,
    pub preview: Option<&'a str>,
    /// The agent's full resume command.
    pub resume: &'a [String],
}

/// Whether `word` can be typed into any common shell (POSIX shells and fish)
/// as one quoted word: no control character, which would end or split the
/// command line, and no backslash, which fish reads as an escape even between
/// single quotes.
pub fn is_typeable(word: &str) -> bool {
    !word.chars().any(|c| c.is_control() || c == '\\')
}

/// Quote `word` so that a shell reads it back as one word. Only meaningful for
/// words that pass [`is_typeable`].
///
/// Words made only of safe characters are left alone, which keeps the typed
/// command readable. The single-quote form `'\''` also works in fish.
pub fn shell_quote(word: &str) -> String {
    // A leading `=` is a command lookup in zsh.
    let safe = !word.is_empty()
        && !word.starts_with('=')
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Command line that starts the Placeholder, program first.
pub fn placeholder_argv(placeholder: &Path, p: &PlaceholderParams<'_>) -> Vec<String> {
    let path = |path: &Path| path.to_string_lossy().into_owned();
    let mut argv = vec![path(placeholder), "--pane".into(), p.pane.into()];
    if let Some(name) = p.name.filter(|n| !n.is_empty()) {
        argv.extend(["--name".into(), name.into()]);
    }
    argv.extend([
        "--kind".into(),
        p.kind.into(),
        "--session".into(),
        p.session.into(),
        "--state-dir".into(),
        path(p.state_dir),
        "--main-bin".into(),
        path(p.main_bin),
        "--herdr-bin".into(),
        path(p.herdr_bin),
        "--shell".into(),
        path(p.shell),
        "--icon".into(),
        p.icon.into(),
        "--token".into(),
        p.token.into(),
        "--cwd".into(),
        p.cwd.into(),
    ]);
    if let Some(preview) = p.preview {
        argv.extend(["--preview".into(), preview.into()]);
    }
    argv.push("--".into());
    argv.extend(p.resume.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{AgentSession, AgentStatus, Pane, ProcessInfo};
    use crate::ops::EnvResult;
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    /// Scripted environment with a virtual clock: `sleep` advances the clock
    /// without waiting.
    struct Fake {
        base: Instant,
        elapsed: Cell<Duration>,
        calls: RefCell<Vec<String>>,
        state_dir: Option<PathBuf>,
        guards: Result<(), Refusal>,
        /// Status of the agent each guard check was given.
        guarded: RefCell<Vec<AgentStatus>>,
        /// What `herdr agent get` reports when it differs from the caller's copy.
        fresh_status: Option<AgentStatus>,
        placeholder: bool,
        screen: Result<String, String>,
        rss_kb: Option<u64>,
        command: Option<String>,
        /// Enters needed before the agent starts quitting.
        enters_needed: usize,
        /// Time from the effective Enter to the shell being foreground.
        quit_after: Option<Duration>,
        /// Time from the quit to Herdr releasing the native identity.
        release_after: Option<Duration>,
        /// Time from `exec` to the Placeholder's report.
        sleeping_after: Option<Duration>,
        enters: Cell<usize>,
        quit_at: Cell<Option<Duration>>,
        exec_at: Cell<Option<Duration>>,
        agent: Agent,
        /// What the screen shows once Ctrl+U was pressed, when it matters.
        cleared_screen: Option<String>,
        cleared: Cell<bool>,
        /// Another Sleep's Placeholder shows up as soon as the agent quits.
        early_placeholder: bool,
        /// The screen cannot be read once Ctrl+U was pressed.
        screen_fails_after_clear: bool,
        /// Typing text into the pane fails.
        text_fails: bool,
    }

    impl Fake {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-sleep-ops-{name}-{}-{}",
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
                state_dir: Some(dir),
                guards: Ok(()),
                guarded: RefCell::new(Vec::new()),
                fresh_status: None,
                placeholder: true,
                screen: Ok("\n  last line\n> \n\n".into()),
                rss_kb: Some(512 * 1024),
                command: None,
                enters_needed: 1,
                quit_after: Some(Duration::from_millis(1500)),
                release_after: Some(Duration::from_millis(500)),
                sleeping_after: Some(Duration::from_millis(600)),
                enters: Cell::new(0),
                quit_at: Cell::new(None),
                exec_at: Cell::new(None),
                cleared_screen: None,
                cleared: Cell::new(false),
                early_placeholder: false,
                screen_fails_after_clear: false,
                text_fails: false,
                agent: Agent {
                    pane_id: "w1-2".into(),
                    name: Some("worker".into()),
                    agent: Some("claude".into()),
                    agent_status: AgentStatus::Idle,
                    workspace_id: "w1".into(),
                    tab_id: "w1:t1".into(),
                    cwd: Some("/work".into()),
                    agent_session: Some(AgentSession {
                        source: "herdr:claude".into(),
                        agent: None,
                        kind: Some("id".into()),
                        value: "sess-1".into(),
                    }),
                    ..Agent::default()
                },
            }
        }

        fn dir(&self) -> &Path {
            self.state_dir.as_ref().unwrap()
        }

        fn log(&self, call: impl Into<String>) {
            self.calls.borrow_mut().push(call.into());
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn clock(&self) -> Duration {
            self.elapsed.get()
        }

        fn journal(&self) -> String {
            std::fs::read_to_string(self.dir().join(crate::journal::FILE_NAME)).unwrap_or_default()
        }

        fn has_quit(&self) -> bool {
            match (self.quit_at.get(), self.quit_after) {
                (Some(at), Some(after)) => self.clock() >= at + after,
                _ => false,
            }
        }

        fn identity_released(&self) -> bool {
            match (self.quit_at.get(), self.release_after) {
                (Some(at), Some(after)) => self.clock() >= at + after,
                _ => false,
            }
        }

        fn is_sleeping(&self) -> bool {
            if self.early_placeholder && self.quit_at.get().is_some() {
                return true;
            }
            match (self.exec_at.get(), self.sleeping_after) {
                (Some(at), Some(after)) => self.clock() >= at + after,
                _ => false,
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            if let Some(dir) = &self.state_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    impl Env for Fake {
        fn snapshot(&self) -> EnvResult<crate::herdr::Snapshot> {
            Ok(crate::herdr::Snapshot::default())
        }

        fn pane(&self, pane: &str) -> EnvResult<Pane> {
            self.log(format!("pane {pane}"));
            let released = self.identity_released();
            Ok(Pane {
                pane_id: pane.into(),
                agent: (!released).then(|| "claude".to_string()),
                agent_session: (!released).then(|| self.agent.agent_session.clone().unwrap()),
                ..Pane::default()
            })
        }

        fn agent(&self, target: &str) -> EnvResult<Agent> {
            self.log(format!("agent {target}"));
            let mut agent = self.agent.clone();
            if let Some(status) = self.fresh_status {
                agent.agent_status = status;
            }
            if self.is_sleeping() {
                agent.agent = Some("sleeping-claude".into());
            }
            Ok(agent)
        }

        fn process_info(&self, pane: &str) -> EnvResult<ProcessInfo> {
            self.log(format!("process-info {pane}"));
            let group = if self.has_quit() { 100 } else { 200 };
            Ok(ProcessInfo {
                pane_id: pane.into(),
                shell_pid: 100,
                foreground_process_group_id: group,
                foreground_processes: Vec::new(),
            })
        }

        fn screen_text(&self, pane: &str, lines: usize) -> EnvResult<String> {
            self.log(format!("read {pane} {lines}"));
            self.screen.clone()
        }

        fn screen_ansi(&self, pane: &str, lines: usize) -> EnvResult<String> {
            self.log(format!("read-ansi {pane} {lines}"));
            if self.screen_fails_after_clear && self.cleared.get() {
                return Err("timeout".into());
            }
            match (&self.cleared_screen, self.cleared.get()) {
                (Some(screen), true) => Ok(screen.clone()),
                _ => self.screen.clone(),
            }
        }

        fn rename_agent(&self, pane: &str, name: &str) -> EnvResult<()> {
            self.log(format!("rename {pane} {name}"));
            Ok(())
        }

        fn send_text(&self, pane: &str, text: &str) -> EnvResult<()> {
            self.log(format!("text {pane} {text}"));
            if self.text_fails {
                return Err("pane gone".into());
            }
            Ok(())
        }

        fn send_keys(&self, pane: &str, keys: &[&str]) -> EnvResult<()> {
            self.log(format!("keys {pane} {}", keys.join(" ")));
            if keys.contains(&"ctrl+u") {
                self.cleared.set(true);
            }
            if keys.contains(&"enter") {
                let enters = self.enters.get() + 1;
                self.enters.set(enters);
                if enters == self.enters_needed {
                    self.quit_at.set(Some(self.clock()));
                }
            }
            Ok(())
        }

        fn run(&self, pane: &str, argv: &[&str]) -> EnvResult<()> {
            self.log(format!("run {pane} {}", argv.join(" ")));
            self.exec_at.set(Some(self.clock()));
            Ok(())
        }

        fn notify(&self, title: &str, body: &str) -> EnvResult<()> {
            self.log(format!("notify {title}: {body}"));
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
        fn check_guards(&self, _: &Config, agent: &Agent, mode: SleepMode) -> Result<(), Refusal> {
            self.log(format!("guards {mode:?}"));
            self.guarded.borrow_mut().push(agent.agent_status);
            self.guards.clone()
        }

        fn agent_rss_kb(&self, _: &Agent, bin: &str) -> Option<u64> {
            self.log(format!("rss {bin}"));
            self.rss_kb
        }

        fn agent_command(&self, _: &Agent, _: &str) -> Option<String> {
            self.command.clone()
        }

        fn shell_path(&self, pid: i32) -> Option<PathBuf> {
            self.log(format!("shell-path {pid}"));
            Some("/bin/zsh".into())
        }

        fn state_dir(&self) -> Option<PathBuf> {
            self.state_dir.clone()
        }

        fn placeholder_bin(&self) -> EnvResult<PathBuf> {
            self.log("placeholder");
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
            1_000_000_000_000 + u64::try_from(self.elapsed.get().as_millis()).unwrap()
        }
    }

    fn config() -> Config {
        Config::default()
    }

    fn run(fake: &Fake, mode: SleepMode) -> SleepOutcome {
        sleep_agent(fake, &config(), &fake.agent, mode, None)
    }

    #[test]
    fn the_guards_see_the_agent_as_it_is_now_not_as_the_caller_read_it() {
        let mut fake = Fake::new("fresh-guards");
        fake.fresh_status = Some(AgentStatus::Working);
        run(&fake, SleepMode::Sweep);
        assert_eq!(*fake.guarded.borrow(), [AgentStatus::Working]);
    }

    fn typed(fake: &Fake) -> bool {
        fake.calls()
            .iter()
            .any(|c| c.starts_with("text") || c.starts_with("keys") || c.starts_with("run "))
    }

    #[test]
    fn a_pane_another_sleep_holds_is_left_alone() {
        let fake = Fake::new("locked");
        let _other = PaneLock::try_acquire(fake.dir(), "w1-2").unwrap().unwrap();
        let outcome = run(&fake, SleepMode::Sweep);
        assert_eq!(outcome, SleepOutcome::Skipped(Refusal::Superseded));
        assert!(fake.calls().is_empty());
        assert_eq!(fake.journal(), "");
    }

    #[test]
    fn a_manual_sleep_waits_for_the_holder_then_gives_up() {
        let fake = Fake::new("locked-manual");
        let _other = PaneLock::try_acquire(fake.dir(), "w1-2").unwrap().unwrap();
        let outcome = run(&fake, SleepMode::Manual);
        assert_eq!(outcome, SleepOutcome::Skipped(Refusal::Superseded));
        assert!(fake.clock() >= LOCK_WAIT, "{:?}", fake.clock());
        assert!(!typed(&fake));
    }

    #[test]
    fn a_sweep_does_not_wait_for_the_holder() {
        let fake = Fake::new("locked-sweep");
        let _other = PaneLock::try_acquire(fake.dir(), "w1-2").unwrap().unwrap();
        run(&fake, SleepMode::Sweep);
        assert_eq!(fake.clock(), Duration::ZERO);
    }

    #[test]
    fn a_pane_that_already_sleeps_is_left_alone() {
        let mut fake = Fake::new("asleep");
        fake.sleeping_after = Some(Duration::ZERO);
        fake.exec_at.set(Some(Duration::ZERO));
        let outcome = run(&fake, SleepMode::Manual);
        assert_eq!(outcome, SleepOutcome::Skipped(Refusal::Superseded));
        assert!(!typed(&fake));
    }

    #[test]
    fn a_pane_whose_agent_changed_is_left_alone() {
        let fake = Fake::new("changed");
        let mut stale = fake.agent.clone();
        stale.agent_session.as_mut().unwrap().value = "an-older-session".into();
        let outcome = sleep_agent(&fake, &config(), &stale, SleepMode::Sweep, None);
        assert_eq!(outcome, SleepOutcome::Skipped(Refusal::Superseded));
        assert!(!typed(&fake));
    }

    #[test]
    fn the_lock_is_released_when_the_sleep_ends() {
        let fake = Fake::new("released");
        assert!(matches!(
            run(&fake, SleepMode::Sweep),
            SleepOutcome::Asleep { .. }
        ));
        assert!(PaneLock::try_acquire(fake.dir(), "w1-2").unwrap().is_some());
    }

    #[test]
    fn the_command_is_not_typed_into_a_placeholder_that_appeared_meanwhile() {
        let mut fake = Fake::new("early");
        fake.early_placeholder = true;
        let outcome = run(&fake, SleepMode::Sweep);
        assert!(matches!(outcome, SleepOutcome::Aborted(_)), "{outcome:?}");
        assert!(exec_call(&fake).is_none());
    }

    fn exec_call(fake: &Fake) -> Option<String> {
        fake.calls().into_iter().find(|c| c.starts_with("run "))
    }

    #[test]
    fn an_announced_sleeper_is_registered() {
        let fake = Fake::new("registry");
        run(&fake, SleepMode::Sweep);
        let registry = Registry::load(fake.dir());
        assert_eq!(
            registry.panes.get("w1-2"),
            Some(&Sleeper {
                kind: "claude".into(),
                session: "sess-1".into(),
                name: Some("worker".into()),
                cwd: "/work".into(),
                args: Vec::new(),
                since_ms: fake.now_millis(),
            })
        );
    }

    #[test]
    fn the_flags_of_the_agent_are_kept_for_its_wake() {
        let mut fake = Fake::new("kept-args");
        fake.command = Some("claude --model opus --api-key sk-secret".into());
        run(&fake, SleepMode::Sweep);
        let sleeper = Registry::load(fake.dir()).panes.remove("w1-2").unwrap();
        assert_eq!(sleeper.args, ["--model", "opus"]);
        let typed = exec_call(&fake).unwrap();
        assert!(
            typed.contains("claude --model opus --resume sess-1"),
            "{typed}"
        );
        assert!(!typed.contains("sk-secret"), "{typed}");
    }

    #[test]
    fn an_unannounced_placeholder_is_registered_too() {
        let mut fake = Fake::new("registry-warned");
        fake.sleeping_after = None;
        assert!(matches!(
            run(&fake, SleepMode::Sweep),
            SleepOutcome::Warned(_)
        ));
        assert!(Registry::load(fake.dir()).panes.contains_key("w1-2"));
    }

    #[test]
    fn a_refused_or_abandoned_sleep_of_a_running_agent_registers_nothing() {
        let mut fake = Fake::new("registry-refused");
        fake.guards = Err(Refusal::DraftInPrompt);
        run(&fake, SleepMode::Manual);
        let mut fake_abandoned = Fake::new("registry-abandoned");
        fake_abandoned.quit_after = None;
        run(&fake_abandoned, SleepMode::Manual);
        for fake in [&fake, &fake_abandoned] {
            assert!(Registry::load(fake.dir()).panes.is_empty());
        }
    }

    #[test]
    fn an_agent_that_quit_before_the_placeholder_started_stays_registered() {
        // The pane is a bare shell then: `restore` is the only way back.
        let mut fake = Fake::new("registry-aborted");
        fake.release_after = None;
        assert!(matches!(
            run(&fake, SleepMode::Manual),
            SleepOutcome::Aborted(_)
        ));
        assert!(Registry::load(fake.dir()).panes.contains_key("w1-2"));
    }

    #[test]
    fn happy_path_runs_in_order() {
        let fake = Fake::new("happy");
        let outcome = run(&fake, SleepMode::Sweep);
        assert_eq!(
            outcome,
            SleepOutcome::Asleep {
                freed_kb: 512 * 1024
            }
        );

        let preview = fake.dir().join("preview-w1-2.txt");
        assert_eq!(std::fs::read_to_string(&preview).unwrap(), "  last line\n>");
        let lines = guards::SCREEN_LINES;
        let expected_run = format!(
            "run w1-2 exec /opt/bin/herdr-sleep-ph --pane w1-2 --name worker --kind claude \
             --session sess-1 --state-dir {dir} --main-bin /opt/bin/herdr-sleep \
             --herdr-bin /usr/bin/herdr --shell /bin/zsh --icon {icon} --token {token} \
             --cwd /work --preview {preview} -- claude --resume sess-1",
            dir = fake.dir().display(),
            icon = shell_quote(&config().sleep_icon),
            token = config().token_name,
            preview = preview.display(),
        );
        let calls = fake.calls();
        let position = |prefix: &str| calls.iter().position(|c| c.starts_with(prefix)).unwrap();
        let ordered = [
            "agent w1-2".to_string(),
            "guards Sweep".to_string(),
            "placeholder".to_string(),
            format!("read w1-2 {lines}"),
            "rss claude".to_string(),
            "text w1-2 /exit".to_string(),
            "keys w1-2 enter".to_string(),
            "process-info w1-2".to_string(),
            "pane w1-2".to_string(),
            "shell-path 100".to_string(),
            expected_run.clone(),
            "notify".to_string(),
        ];
        let mut last = 0;
        for step in &ordered {
            let at = calls.iter().position(|c| c.starts_with(step.as_str()));
            let at = at.unwrap_or_else(|| panic!("missing {step} in {calls:?}"));
            assert!(at >= last, "{step} out of order in {calls:?}");
            last = at;
        }
        assert_eq!(calls[position("run ")], expected_run);
        // The Sleeping agent is awaited after the run.
        assert_eq!(calls[position("run ") + 1], "agent w1-2");
        // Enter after the exit command, then Ctrl+U just before the exec line.
        assert_eq!(calls.iter().filter(|c| c.starts_with("keys")).count(), 2);
        assert_eq!(calls[position("run ") - 1], "keys w1-2 ctrl+u");

        let journal = fake.journal();
        let events: Vec<&str> = journal
            .lines()
            .map(|l| l.split(' ').nth(1).unwrap())
            .collect();
        assert_eq!(events, ["SLEEP", "ASLEEP"]);
        assert!(journal.contains("freed=512MB"));
    }

    #[test]
    fn notification_only_on_a_sweep() {
        let text = |fake: &Fake| fake.calls().into_iter().find(|c| c.starts_with("notify"));
        let sweep = Fake::new("notify-sweep");
        run(&sweep, SleepMode::Sweep);
        let icon = config().sleep_icon;
        assert_eq!(
            text(&sweep).unwrap(),
            format!("notify Herdr Sleep: {icon} worker (claude) went to sleep, freed 512 MB")
        );
        for mode in [SleepMode::Manual, SleepMode::Force] {
            let fake = Fake::new("notify-other");
            run(&fake, mode);
            assert!(text(&fake).is_none());
        }
        let mut quiet = config();
        quiet.notify = false;
        let fake = Fake::new("notify-off");
        sleep_agent(&fake, &quiet, &fake.agent, SleepMode::Sweep, None);
        assert!(text(&fake).is_none());
    }

    #[test]
    fn unknown_memory_is_left_out_of_the_notification() {
        let mut fake = Fake::new("no-rss");
        fake.rss_kb = None;
        assert_eq!(
            run(&fake, SleepMode::Sweep),
            SleepOutcome::Asleep { freed_kb: 0 }
        );
        let icon = config().sleep_icon;
        assert!(fake.calls().contains(&format!(
            "notify Herdr Sleep: {icon} worker (claude) went to sleep"
        )));
    }

    #[test]
    fn unnamed_agent_is_labelled_by_its_pane() {
        let mut fake = Fake::new("unnamed");
        fake.agent.name = None;
        run(&fake, SleepMode::Sweep);
        let icon = config().sleep_icon;
        assert!(fake.calls().contains(&format!(
            "notify Herdr Sleep: {icon} w1-2 (claude) went to sleep, freed 512 MB"
        )));
        assert!(!exec_call(&fake).unwrap().contains("--name"));
    }

    #[test]
    fn refusal_is_journaled_unless_silent() {
        let mut fake = Fake::new("refused");
        fake.guards = Err(Refusal::DraftInPrompt);
        let outcome = run(&fake, SleepMode::Manual);
        assert_eq!(outcome, SleepOutcome::Skipped(Refusal::DraftInPrompt));
        assert!(
            fake.journal()
                .contains(" SKIP worker (w1-2): text is typed")
        );
        assert!(!fake.calls().iter().any(|c| c.starts_with("text")));

        let mut silent = Fake::new("silent");
        silent.guards = Err(Refusal::NotManaged);
        assert_eq!(
            run(&silent, SleepMode::Manual),
            SleepOutcome::Skipped(Refusal::NotManaged)
        );
        assert_eq!(silent.journal(), "");
    }

    #[test]
    fn a_sweep_does_not_repeat_the_same_refusal() {
        let mut fake = Fake::new("dedupe");
        fake.guards = Err(Refusal::Focused);
        let cfg = config();
        sleep_agent(&fake, &cfg, &fake.agent, SleepMode::Sweep, Some("focused"));
        assert_eq!(fake.journal(), "");
        sleep_agent(&fake, &cfg, &fake.agent, SleepMode::Sweep, Some("excluded"));
        assert_eq!(fake.journal().lines().count(), 1);
        // A manual request always says why.
        sleep_agent(&fake, &cfg, &fake.agent, SleepMode::Manual, Some("focused"));
        assert_eq!(fake.journal().lines().count(), 2);
    }

    #[test]
    fn dry_run_stops_before_touching_the_agent() {
        let fake = Fake::new("dry");
        let mut cfg = config();
        cfg.dry_run = true;
        let outcome = sleep_agent(&fake, &cfg, &fake.agent, SleepMode::Sweep, None);
        assert_eq!(outcome, SleepOutcome::DryRun);
        assert_eq!(fake.calls(), ["agent w1-2", "guards Sweep"]);
        assert!(
            fake.journal()
                .contains(" DRY-RUN worker (w1-2) would sleep")
        );

        // Force overrides the dry run.
        let outcome = sleep_agent(&fake, &cfg, &fake.agent, SleepMode::Force, None);
        assert!(matches!(outcome, SleepOutcome::Asleep { .. }));
    }

    #[test]
    fn agent_that_never_quits_is_abandoned_without_exec() {
        let mut fake = Fake::new("stuck");
        fake.quit_after = None;
        let outcome = run(&fake, SleepMode::Sweep);
        assert_eq!(
            outcome,
            SleepOutcome::Aborted("agent still running after 20s".into())
        );
        let keys: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("keys"))
            .collect();
        assert_eq!(
            keys,
            [
                "keys w1-2 enter",
                "keys w1-2 enter",
                "keys w1-2 backspace backspace backspace backspace backspace"
            ],
            "one Enter, one retry, then the typed /exit is erased"
        );
        assert!(exec_call(&fake).is_none());
        assert!(!fake.calls().iter().any(|c| c.starts_with("pane ")));
        assert!(fake.clock() >= QUIT_TIMEOUT);
        assert!(fake.journal().contains(" ABORT worker (w1-2): agent still"));
    }

    const RULE: &str =
        "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";

    fn framed(prompt_lines: &[&str]) -> String {
        format!(
            "  done\n\n{RULE}\n{}\n{RULE}\n  status\n",
            prompt_lines.join("\n")
        )
    }

    fn with_drafts() -> Config {
        Config::from_toml_str("sleep_with_draft = true").unwrap()
    }

    /// A Fake showing `draft` in the prompt until Ctrl+U clears it.
    fn drafting(name: &str, draft: &str) -> Fake {
        let mut fake = Fake::new(name);
        fake.screen = Ok(framed(&[&format!("\u{276F} {draft}")]));
        fake.cleared_screen = Some(framed(&["\u{276F} "]));
        fake
    }

    #[test]
    fn a_draft_is_saved_the_prompt_cleared_and_exit_typed_into_it() {
        let fake = drafting("draft", "half a thought");
        let outcome = sleep_agent(&fake, &with_drafts(), &fake.agent, SleepMode::Manual, None);
        assert!(
            matches!(outcome, SleepOutcome::Asleep { .. }),
            "{outcome:?}"
        );
        let calls = fake.calls();
        let at = |prefix: &str| calls.iter().position(|c| c.starts_with(prefix)).unwrap();
        assert!(at("keys w1-2 ctrl+u") < at("text w1-2 /exit"));
        assert_eq!(
            std::fs::read_to_string(fake.dir().join("draft-w1-2.txt")).unwrap(),
            "half a thought"
        );
        assert!(!fake.journal().contains("half a thought"));
    }

    #[test]
    fn a_failed_sleep_erases_the_exit_and_types_the_draft_back() {
        let mut fake = drafting("draft-undo", "half a thought");
        fake.quit_after = None;
        let outcome = sleep_agent(&fake, &with_drafts(), &fake.agent, SleepMode::Manual, None);
        assert!(matches!(outcome, SleepOutcome::Aborted(_)), "{outcome:?}");
        let calls = fake.calls();
        let last_keys = calls.iter().rposition(|c| c.starts_with("keys")).unwrap();
        assert_eq!(
            calls[last_keys],
            "keys w1-2 backspace backspace backspace backspace backspace"
        );
        assert_eq!(
            calls.iter().rfind(|c| c.starts_with("text")).unwrap(),
            "text w1-2 half a thought"
        );
        assert!(!fake.dir().join("draft-w1-2.txt").exists());
    }

    #[test]
    fn a_draft_that_cannot_be_cleared_is_kept_and_nothing_is_typed() {
        let mut fake = drafting("draft-stuck", "half a thought");
        fake.cleared_screen = None;
        let outcome = sleep_agent(&fake, &with_drafts(), &fake.agent, SleepMode::Manual, None);
        assert!(
            matches!(&outcome, SleepOutcome::Aborted(m) if m.contains("cannot clear")),
            "{outcome:?}"
        );
        assert!(!fake.calls().iter().any(|c| c.starts_with("text")));
        assert!(!fake.dir().join("draft-w1-2.txt").exists());
    }

    #[test]
    fn a_draft_is_kept_when_the_screen_cannot_be_read_after_ctrl_u() {
        let mut fake = drafting("draft-blind", "half a thought");
        fake.screen_fails_after_clear = true;
        let outcome = sleep_agent(&fake, &with_drafts(), &fake.agent, SleepMode::Manual, None);
        assert!(
            matches!(&outcome, SleepOutcome::Aborted(m) if m.contains("cannot tell")),
            "{outcome:?}"
        );
        // The prompt may be empty now: the text is on disk, nothing is lost.
        assert_eq!(
            std::fs::read_to_string(fake.dir().join("draft-w1-2.txt")).unwrap(),
            "half a thought"
        );
    }

    #[test]
    fn a_draft_that_cannot_be_typed_back_stays_on_disk() {
        let mut fake = drafting("draft-lost", "half a thought");
        fake.quit_after = None;
        fake.text_fails = true;
        let outcome = sleep_agent(&fake, &with_drafts(), &fake.agent, SleepMode::Manual, None);
        assert!(matches!(outcome, SleepOutcome::Aborted(_)), "{outcome:?}");
        assert!(fake.dir().join("draft-w1-2.txt").exists());
    }

    #[test]
    fn a_wrapped_draft_is_kept_untouched() {
        let mut fake = Fake::new("draft-wrapped");
        fake.screen = Ok(framed(&[
            "\u{276F} a long line that",
            "  the terminal wrapped",
        ]));
        let outcome = sleep_agent(&fake, &with_drafts(), &fake.agent, SleepMode::Manual, None);
        assert!(
            matches!(&outcome, SleepOutcome::Aborted(m) if m.contains("cannot be read reliably")),
            "{outcome:?}"
        );
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| c.starts_with("text") || c.starts_with("keys"))
        );
    }

    #[test]
    fn without_the_setting_the_draft_is_left_alone() {
        let fake = drafting("draft-off", "half a thought");
        run(&fake, SleepMode::Force);
        // The line is cleared once the agent is gone, never the prompt before.
        let calls = fake.calls();
        let exit = calls.iter().position(|c| c == "text w1-2 /exit").unwrap();
        assert!(!calls[..exit].iter().any(|c| c.contains("ctrl+u")));
        assert!(!fake.dir().join("draft-w1-2.txt").exists());
    }

    #[test]
    fn a_swallowed_first_enter_is_retried_after_five_seconds() {
        let mut fake = Fake::new("swallowed");
        fake.enters_needed = 2;
        let outcome = run(&fake, SleepMode::Sweep);
        assert!(matches!(outcome, SleepOutcome::Asleep { .. }));
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|c| c.starts_with("keys w1-2 enter"))
                .count(),
            2
        );
        // 0.5 s before the first Enter, 5 s until the second, then the quit.
        assert!(fake.clock() >= Duration::from_millis(500) + SECOND_ENTER_AFTER);
    }

    #[test]
    fn identity_that_is_never_released_blocks_the_placeholder() {
        let mut fake = Fake::new("limbo");
        fake.release_after = None;
        let outcome = run(&fake, SleepMode::Sweep);
        assert_eq!(
            outcome,
            SleepOutcome::Aborted("identity not released".into())
        );
        assert!(exec_call(&fake).is_none());
        assert!(
            fake.journal()
                .contains(" ABORT worker (w1-2): identity not released")
        );
    }

    #[test]
    fn missing_placeholder_aborts_before_typing() {
        let mut fake = Fake::new("no-ph");
        fake.placeholder = false;
        let outcome = run(&fake, SleepMode::Sweep);
        assert!(matches!(outcome, SleepOutcome::Aborted(m) if m.contains("placeholder")));
        let calls = fake.calls();
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("text") || c.starts_with("keys"))
        );
    }

    #[test]
    fn preview_leaves_out_the_input_box_and_the_status_bar() {
        let rule = "\u{2500}".repeat(20);
        let mut fake = Fake::new("preview-frame");
        fake.screen = Ok(format!(
            "old\n  answer\n\n{rule}\n> \n{rule}\n  [Sonnet] status\n  more status\n"
        ));
        run(&fake, SleepMode::Sweep);
        let preview = fake.dir().join("preview-w1-2.txt");
        assert_eq!(std::fs::read_to_string(preview).unwrap(), "old\n  answer");
    }

    #[test]
    fn preview_keeps_the_last_lines_above_the_frame() {
        let rule = "\u{2500}".repeat(20);
        let mut fake = Fake::new("preview-count");
        let many: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        fake.screen = Ok(format!("{many}{rule}\n> \n{rule}\nstatus\n"));
        run(&fake, SleepMode::Sweep);
        let text = std::fs::read_to_string(fake.dir().join("preview-w1-2.txt")).unwrap();
        let kept: Vec<&str> = text.lines().collect();
        assert_eq!(kept.len(), usize::try_from(config().preview_lines).unwrap());
        assert_eq!(kept.last(), Some(&"line 50"));
    }

    #[test]
    fn failing_preview_only_drops_the_preview() {
        let mut fake = Fake::new("no-preview");
        fake.screen = Err("boom".into());
        let outcome = run(&fake, SleepMode::Sweep);
        assert!(matches!(outcome, SleepOutcome::Asleep { .. }));
        assert!(!exec_call(&fake).unwrap().contains("--preview"));
        assert!(!fake.dir().join("preview-w1-2.txt").exists());
    }

    #[test]
    fn placeholder_that_never_reports_is_a_warning() {
        let mut fake = Fake::new("warned");
        fake.sleeping_after = None;
        let outcome = run(&fake, SleepMode::Sweep);
        assert_eq!(
            outcome,
            SleepOutcome::Warned("placeholder did not report sleeping-claude".into())
        );
        assert!(exec_call(&fake).is_some());
        assert!(fake.journal().contains(" WARN worker (w1-2)"));
        assert!(!fake.calls().iter().any(|c| c.starts_with("notify")));
    }

    #[test]
    fn waiting_gives_a_busy_agent_time_before_the_guards_run() {
        let mut fake = Fake::new("wait-busy");
        fake.agent.agent_status = AgentStatus::Working;
        fake.guards = Err(Refusal::NotSettled(AgentStatus::Working));
        let outcome = sleep_agent_waiting(
            &fake,
            &config(),
            &fake.agent,
            SleepMode::Manual,
            Duration::from_secs(10),
        );
        assert_eq!(
            outcome,
            SleepOutcome::Skipped(Refusal::NotSettled(AgentStatus::Working))
        );
        assert!(fake.clock() >= Duration::from_secs(10));
        // One attempt, not one per poll: the journal would repeat the refusal.
        let guards = fake
            .calls()
            .iter()
            .filter(|c| c.starts_with("guards"))
            .count();
        assert_eq!(guards, 1);
    }

    #[test]
    fn waiting_retries_a_transient_refusal_only() {
        let mut fake = Fake::new("wait-transient");
        fake.guards = Err(Refusal::PromptNotDetected);
        let outcome = sleep_agent_waiting(
            &fake,
            &config(),
            &fake.agent,
            SleepMode::Manual,
            Duration::from_secs(6),
        );
        assert_eq!(outcome, SleepOutcome::Skipped(Refusal::PromptNotDetected));
        let guards = |fake: &Fake| {
            fake.calls()
                .iter()
                .filter(|c| c.starts_with("guards"))
                .count()
        };
        assert!(guards(&fake) > 1);

        let mut fake = Fake::new("wait-final");
        fake.guards = Err(Refusal::DraftInPrompt);
        let outcome = sleep_agent_waiting(
            &fake,
            &config(),
            &fake.agent,
            SleepMode::Manual,
            Duration::from_secs(60),
        );
        assert_eq!(outcome, SleepOutcome::Skipped(Refusal::DraftInPrompt));
        assert_eq!(guards(&fake), 1);
        assert!(fake.clock() < Duration::from_secs(5));
    }

    #[test]
    fn the_mode_reaches_the_guards() {
        for mode in [SleepMode::Sweep, SleepMode::Manual, SleepMode::Force] {
            let fake = Fake::new("mode");
            run(&fake, mode);
            assert_eq!(fake.calls()[1], format!("guards {mode:?}"));
        }
    }

    #[test]
    fn forced_sleep_without_a_session_is_aborted() {
        let mut fake = Fake::new("no-session");
        fake.agent.agent_session = None;
        let outcome = run(&fake, SleepMode::Force);
        assert_eq!(outcome, SleepOutcome::Aborted("no session id".into()));
        assert!(!fake.calls().iter().any(|c| c.starts_with("text")));
    }

    #[test]
    fn missing_state_directory_aborts_first() {
        let mut fake = Fake::new("no-state");
        fake.state_dir = None;
        let outcome = run(&fake, SleepMode::Sweep);
        assert_eq!(outcome, SleepOutcome::Aborted("no state directory".into()));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn placeholder_argv_matches_the_placeholder_flags() {
        let resume = vec!["claude".to_string(), "--resume".into(), "s".into()];
        let params = PlaceholderParams {
            pane: "p",
            name: None,
            kind: "claude",
            session: "s",
            state_dir: Path::new("/state"),
            main_bin: Path::new("/bin/hs"),
            herdr_bin: Path::new("/bin/herdr"),
            shell: Path::new("/bin/zsh"),
            icon: "z",
            token: "sleep",
            cwd: "/w",
            preview: None,
            resume: &resume,
        };
        let argv = placeholder_argv(Path::new("/bin/ph"), &params);
        assert_eq!(
            argv,
            [
                "/bin/ph",
                "--pane",
                "p",
                "--kind",
                "claude",
                "--session",
                "s",
                "--state-dir",
                "/state",
                "--main-bin",
                "/bin/hs",
                "--herdr-bin",
                "/bin/herdr",
                "--shell",
                "/bin/zsh",
                "--icon",
                "z",
                "--token",
                "sleep",
                "--cwd",
                "/w",
                "--",
                "claude",
                "--resume",
                "s"
            ]
        );
        let named = PlaceholderParams {
            name: Some("w"),
            preview: Some("/state/p.txt"),
            ..params
        };
        let argv = placeholder_argv(Path::new("/bin/ph"), &named);
        assert_eq!(&argv[3..5], ["--name", "w"]);
        let at = argv.iter().position(|a| a == "--preview").unwrap();
        assert_eq!(argv[at + 1], "/state/p.txt");
        assert_eq!(argv[at + 2], "--");
    }

    #[test]
    fn shell_quote_keeps_safe_words_and_quotes_the_rest() {
        assert_eq!(shell_quote("--pane"), "--pane");
        assert_eq!(
            shell_quote("/opt/bin/herdr-sleep-ph"),
            "/opt/bin/herdr-sleep-ph"
        );
        assert_eq!(shell_quote("my dir"), "'my dir'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$HOME;rm"), "'$HOME;rm'");
        assert_eq!(shell_quote("\u{1f4a4}"), "'\u{1f4a4}'");
        assert_eq!(shell_quote("=ls"), "'=ls'");
        assert_eq!(shell_quote("a=b"), "a=b");
    }

    #[test]
    fn exec_command_quotes_unsafe_words() {
        let mut fake = Fake::new("quote");
        fake.agent.cwd = Some("/work/my project".into());
        let outcome = run(&fake, SleepMode::Sweep);
        assert!(
            matches!(outcome, SleepOutcome::Asleep { .. }),
            "{outcome:?}"
        );
        assert!(
            exec_call(&fake)
                .unwrap()
                .contains("--cwd '/work/my project'")
        );
    }

    #[test]
    fn typeable_words_exclude_control_characters_and_backslashes() {
        assert!(is_typeable("/work/my project"));
        assert!(is_typeable("\u{1f4a4}"));
        assert!(!is_typeable("a\nb"));
        assert!(!is_typeable("a\tb"));
        assert!(!is_typeable("c:\\dir"));
        assert!(!is_typeable("esc\u{1b}[0m"));
    }

    #[test]
    fn untypeable_agent_values_abort_before_anything_is_typed() {
        for hostile in ["bad\nname", "back\\slash"] {
            let mut fake = Fake::new("hostile-name");
            fake.agent.name = Some(hostile.into());
            let outcome = run(&fake, SleepMode::Sweep);
            assert!(matches!(outcome, SleepOutcome::Aborted(_)), "{outcome:?}");
            assert!(
                !fake
                    .calls()
                    .iter()
                    .any(|c| c.starts_with("text ") || c.starts_with("keys ")),
                "{:?}",
                fake.calls()
            );
            assert!(exec_call(&fake).is_none());
        }
        let mut fake = Fake::new("hostile-cwd");
        fake.agent.cwd = Some("/work/it's\nhere".into());
        assert!(matches!(
            run(&fake, SleepMode::Sweep),
            SleepOutcome::Aborted(_)
        ));
        assert!(exec_call(&fake).is_none());
    }
}

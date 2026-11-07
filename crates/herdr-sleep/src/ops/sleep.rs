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
//! Markers (sleep icon on the pane, tab and workspace) are synchronised by
//! the caller, not here.

use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::guards::{Refusal, SleepMode};
use crate::herdr::Agent;
use crate::journal::Journal;

use super::Env;

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

    // 3. Preview and memory figure, both best effort.
    let preview = write_preview(env, config, pane, &state_dir);
    let freed_kb = env.agent_rss_kb(agent, &profile.bin).unwrap_or(0);

    // 4. Quit the agent through its exit command.
    journal.log("SLEEP", &format!("{label} ({pane}) kind={kind}"));
    if let Err(error) = env.send_text(pane, &profile.exit) {
        return abort(format!("cannot type the exit command: {error}"));
    }
    env.sleep(KEY_DELAY);
    if let Err(error) = env.send_keys(pane, &["enter"]) {
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
    let cwd = agent.effective_cwd().unwrap_or(".");
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
            resume: &profile.resume_argv(session),
        },
    );
    // `pane run` types the words into the pane's shell, so each one is quoted
    // for a POSIX shell (single quotes work in fish as well).
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
        return SleepOutcome::Warned(reason);
    }
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
    SleepOutcome::Asleep { freed_kb }
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

/// Capture the Preview into `<state_dir>/preview-<pane>.txt`. A failure only
/// drops the preview.
fn write_preview(env: &dyn Env, config: &Config, pane: &str, state_dir: &Path) -> Option<String> {
    let lines = usize::try_from(config.preview_lines).ok()?;
    if lines == 0 {
        return None;
    }
    let screen = env.screen_text(pane, lines).ok()?;
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let first = lines.iter().position(|l| !l.is_empty())?;
    let last = lines.iter().rposition(|l| !l.is_empty())?;
    let text = lines[first..=last].join("\n");
    let safe: String = pane
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = state_dir.join(format!("preview-{safe}.txt"));
    std::fs::write(&path, text).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Call `done` with the time elapsed since the start until it returns true or
/// `timeout` has passed, pausing `poll` between calls. The condition is
/// checked once more at the deadline.
fn wait_until(
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

/// Quote `word` for a POSIX shell so that it is read back as one word.
///
/// Words made only of safe characters are left alone, which keeps the typed
/// command readable. The single-quote form `'\''` also works in fish.
pub fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
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
        placeholder: bool,
        screen: Result<String, String>,
        rss_kb: Option<u64>,
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
                placeholder: true,
                screen: Ok("\n  last line\n> \n\n".into()),
                rss_kb: Some(512 * 1024),
                enters_needed: 1,
                quit_after: Some(Duration::from_millis(1500)),
                release_after: Some(Duration::from_millis(500)),
                sleeping_after: Some(Duration::from_millis(600)),
                enters: Cell::new(0),
                quit_at: Cell::new(None),
                exec_at: Cell::new(None),
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

        fn send_text(&self, pane: &str, text: &str) -> EnvResult<()> {
            self.log(format!("text {pane} {text}"));
            Ok(())
        }

        fn send_keys(&self, pane: &str, keys: &[&str]) -> EnvResult<()> {
            self.log(format!("keys {pane} {}", keys.join(" ")));
            let enters = self.enters.get() + 1;
            self.enters.set(enters);
            if enters == self.enters_needed {
                self.quit_at.set(Some(self.clock()));
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

        fn check_guards(&self, _: &Config, _: &Agent, mode: SleepMode) -> Result<(), Refusal> {
            self.log(format!("guards {mode:?}"));
            self.guards.clone()
        }

        fn agent_rss_kb(&self, _: &Agent, bin: &str) -> Option<u64> {
            self.log(format!("rss {bin}"));
            self.rss_kb
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
    }

    fn config() -> Config {
        Config::default()
    }

    fn run(fake: &Fake, mode: SleepMode) -> SleepOutcome {
        sleep_agent(fake, &config(), &fake.agent, mode, None)
    }

    fn exec_call(fake: &Fake) -> Option<String> {
        fake.calls().into_iter().find(|c| c.starts_with("run "))
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
        let lines = config().preview_lines;
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
            "agent w1-2".to_string(),
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
        assert_eq!(calls.iter().filter(|c| c.starts_with("keys")).count(), 1);

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
        assert_eq!(fake.calls(), ["guards Sweep"]);
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
        let enters: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("keys"))
            .collect();
        assert_eq!(enters.len(), 2, "one Enter, then one retry");
        assert!(exec_call(&fake).is_none());
        assert!(!fake.calls().iter().any(|c| c.starts_with("pane ")));
        assert!(fake.clock() >= QUIT_TIMEOUT);
        assert!(fake.journal().contains(" ABORT worker (w1-2): agent still"));
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
                .filter(|c| c.starts_with("keys"))
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
    fn the_mode_reaches_the_guards() {
        for mode in [SleepMode::Sweep, SleepMode::Manual, SleepMode::Force] {
            let fake = Fake::new("mode");
            run(&fake, mode);
            assert_eq!(fake.calls()[0], format!("guards {mode:?}"));
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
}

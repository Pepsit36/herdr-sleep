//! The outside world of an operation: Herdr, processes, the clock.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{self, Config};
use crate::guards::{self, Refusal, SleepMode};
use crate::herdr::{Agent, HerdrClient, MetadataChange, Pane, ProcessInfo, ReadFormat, Snapshot};
use crate::procs::{self, ProcessTable};

/// File name of the Placeholder binary, installed next to `herdr-sleep`.
pub const PLACEHOLDER_BIN: &str = "herdr-sleep-ph";

/// Errors of the environment are plain messages: operations log them and
/// decide for themselves whether they matter.
pub type EnvResult<T> = Result<T, String>;

/// Everything an operation reads from or does to the world.
pub trait Env {
    /// Herdr's view of the whole session.
    fn snapshot(&self) -> EnvResult<Snapshot>;
    /// Herdr's view of one pane.
    fn pane(&self, pane: &str) -> EnvResult<Pane>;
    /// Herdr's view of the agent in a pane, by pane id or agent name.
    fn agent(&self, target: &str) -> EnvResult<Agent>;
    /// Foreground processes of a pane.
    fn process_info(&self, pane: &str) -> EnvResult<ProcessInfo>;
    /// The last `lines` rows of a pane as plain text.
    fn screen_text(&self, pane: &str, lines: usize) -> EnvResult<String>;
    /// The last `lines` rows of a pane with their escape sequences.
    fn screen_ansi(&self, pane: &str, lines: usize) -> EnvResult<String>;
    /// Give the agent in `pane` the name `name`.
    fn rename_agent(&self, pane: &str, name: &str) -> EnvResult<()>;
    /// Start the agent `kind` named `name` in `pane`, which sits at its shell
    /// prompt, with `args` after its binary, and wait until it is ready.
    fn start_agent(
        &self,
        _name: &str,
        _kind: &str,
        _pane: &str,
        _args: &[String],
        _timeout: Duration,
    ) -> EnvResult<()> {
        Err("starting an agent is not supported here".into())
    }
    /// Whether Claude Code saved the conversation of `session`, `None` when
    /// that cannot be told.
    fn claude_conversation_saved(&self, _session: &str) -> Option<bool> {
        None
    }
    /// Wall-clock milliseconds since the Unix epoch at which the shell of
    /// `pane` started, `None` when that cannot be told.
    fn pane_shell_started_ms(&self, _pane: &str) -> Option<u64> {
        None
    }
    /// Type text into a pane without pressing Enter.
    fn send_text(&self, pane: &str, text: &str) -> EnvResult<()>;
    /// Press logical keys in a pane.
    fn send_keys(&self, pane: &str, keys: &[&str]) -> EnvResult<()>;
    /// Type a command into a pane and press Enter.
    fn run(&self, pane: &str, argv: &[&str]) -> EnvResult<()>;
    /// Show a notification in Herdr.
    fn notify(&self, title: &str, body: &str) -> EnvResult<()>;
    /// Give the tab `tab` the label `label`.
    fn tab_rename(&self, tab: &str, label: &str) -> EnvResult<()>;
    /// Set or clear a metadata token of `workspace` on behalf of `source`.
    fn workspace_report_metadata(
        &self,
        workspace: &str,
        source: &str,
        change: &MetadataChange,
        seq: u64,
    ) -> EnvResult<()>;

    /// Run the guards for `agent`.
    fn check_guards(&self, config: &Config, agent: &Agent, mode: SleepMode) -> Result<(), Refusal>;
    /// Resident memory of the agent process `bin` and its descendants in
    /// kilobytes, `None` when it cannot be determined.
    fn agent_rss_kb(&self, agent: &Agent, bin: &str) -> Option<u64>;
    /// The executable behind the shell process `pid`.
    fn shell_path(&self, pid: i32) -> Option<PathBuf>;

    /// Directory of runtime state, `None` when it cannot be determined.
    fn state_dir(&self) -> Option<PathBuf>;
    /// The Placeholder binary, verified to exist and be executable.
    fn placeholder_bin(&self) -> EnvResult<PathBuf>;
    /// The `herdr-sleep` binary the Placeholder calls back.
    fn main_bin(&self) -> PathBuf;
    /// The `herdr` binary, as an absolute path when it can be resolved.
    fn herdr_bin(&self) -> PathBuf;

    /// Whether `path` is an existing directory.
    fn dir_exists(&self, path: &str) -> bool {
        Path::new(path).is_dir()
    }

    /// Pause for `duration`.
    fn sleep(&self, duration: Duration);
    /// The current instant of the environment's clock.
    fn now(&self) -> Instant;
    /// Wall-clock time in milliseconds since the Unix epoch.
    fn now_millis(&self) -> u64;
}

/// [`Env`] backed by the real Herdr, the real processes and the real clock.
pub struct LiveEnv {
    client: HerdrClient,
    state_dir: Option<PathBuf>,
    placeholder: Option<PathBuf>,
}

impl LiveEnv {
    /// Environment that talks to Herdr through `client`, with the state
    /// directory taken from the process environment.
    pub fn new(client: HerdrClient) -> Self {
        LiveEnv {
            client,
            state_dir: config::state_dir(config::process_env),
            placeholder: None,
        }
    }

    /// Use `dir` as the state directory.
    pub fn with_state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }

    /// Use the Placeholder at `path` instead of the one next to the running
    /// executable.
    pub fn with_placeholder(mut self, path: impl Into<PathBuf>) -> Self {
        self.placeholder = Some(path.into());
        self
    }

    /// The Herdr client behind this environment.
    pub fn client(&self) -> &HerdrClient {
        &self.client
    }
}

fn message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl Env for LiveEnv {
    fn snapshot(&self) -> EnvResult<Snapshot> {
        self.client.snapshot().map_err(message)
    }

    fn pane(&self, pane: &str) -> EnvResult<Pane> {
        self.client.pane_get(pane).map_err(message)
    }

    fn agent(&self, target: &str) -> EnvResult<Agent> {
        self.client.agent_get(target).map_err(message)
    }

    fn process_info(&self, pane: &str) -> EnvResult<ProcessInfo> {
        self.client.pane_process_info(pane).map_err(message)
    }

    fn screen_text(&self, pane: &str, lines: usize) -> EnvResult<String> {
        self.client
            .pane_read(pane, lines, ReadFormat::Text)
            .map_err(message)
    }

    fn screen_ansi(&self, pane: &str, lines: usize) -> EnvResult<String> {
        self.client
            .pane_read(pane, lines, ReadFormat::Ansi)
            .map_err(message)
    }

    fn rename_agent(&self, pane: &str, name: &str) -> EnvResult<()> {
        self.client.agent_rename(pane, name).map_err(message)
    }

    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane: &str,
        args: &[String],
        timeout: Duration,
    ) -> EnvResult<()> {
        self.client
            .agent_start(name, kind, pane, timeout, args)
            .map_err(message)
    }

    fn pane_shell_started_ms(&self, pane: &str) -> Option<u64> {
        let info = self.client.pane_process_info(pane).ok()?;
        procs::started_ms(info.shell_pid, self.now_millis())
    }

    fn claude_conversation_saved(&self, session: &str) -> Option<bool> {
        crate::guards::claude_session_saved(session)
    }

    fn send_text(&self, pane: &str, text: &str) -> EnvResult<()> {
        self.client.pane_send_text(pane, text).map_err(message)
    }

    fn send_keys(&self, pane: &str, keys: &[&str]) -> EnvResult<()> {
        self.client.pane_send_keys(pane, keys).map_err(message)
    }

    fn run(&self, pane: &str, argv: &[&str]) -> EnvResult<()> {
        self.client.pane_run(pane, argv).map_err(message)
    }

    fn notify(&self, title: &str, body: &str) -> EnvResult<()> {
        self.client.notify(title, body).map_err(message)
    }

    fn tab_rename(&self, tab: &str, label: &str) -> EnvResult<()> {
        self.client.tab_rename(tab, label).map_err(message)
    }

    fn workspace_report_metadata(
        &self,
        workspace: &str,
        source: &str,
        change: &MetadataChange,
        seq: u64,
    ) -> EnvResult<()> {
        self.client
            .workspace_report_metadata(workspace, source, change, seq)
            .map_err(message)
    }

    fn check_guards(&self, config: &Config, agent: &Agent, mode: SleepMode) -> Result<(), Refusal> {
        // An unreadable process table leaves the table empty, so the guards
        // that need it fail closed.
        let table = ProcessTable::load().unwrap_or_default();
        guards::check_live(&self.client, config, agent, &table, mode)
    }

    fn agent_rss_kb(&self, agent: &Agent, bin: &str) -> Option<u64> {
        let info = self.client.pane_process_info(&agent.pane_id).ok()?;
        let table = ProcessTable::load().ok()?;
        let pid = procs::find_agent_process(&info, &table, bin)?;
        Some(table.rss_of_tree(pid))
    }

    fn shell_path(&self, pid: i32) -> Option<PathBuf> {
        procs::shell_path_of(pid)
    }

    fn state_dir(&self) -> Option<PathBuf> {
        self.state_dir.clone()
    }

    fn placeholder_bin(&self) -> EnvResult<PathBuf> {
        let path = match &self.placeholder {
            Some(path) => path.clone(),
            None => std::env::current_exe()
                .map_err(message)?
                .with_file_name(PLACEHOLDER_BIN),
        };
        if is_executable_file(&path) {
            Ok(path)
        } else {
            Err(format!("{} is missing or not executable", path.display()))
        }
    }

    fn main_bin(&self) -> PathBuf {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("herdr-sleep"))
    }

    fn herdr_bin(&self) -> PathBuf {
        let binary = self.client.binary();
        if binary.is_absolute() {
            return binary.to_path_buf();
        }
        binary
            .to_str()
            .and_then(procs::resolve_program)
            .unwrap_or_else(|| binary.to_path_buf())
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

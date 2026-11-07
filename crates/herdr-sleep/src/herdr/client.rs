//! The `herdr` command line, driven as a JSON API.
//!
//! Every exchange with Herdr is one `herdr` process: arguments in, JSON out.
//! [`HerdrClient`] builds the arguments, a [`CommandRunner`] runs the process,
//! and the output is interpreted the same way whatever the command:
//!
//! - JSON on stdout carrying an `error` object is an [`HerdrError::Api`];
//! - JSON on stdout otherwise yields its `result` value;
//! - JSON on stderr carrying an `error` object is an [`HerdrError::Api`] too
//!   (server errors come that way, with exit status 1);
//! - anything else is an [`HerdrError::Exec`].
//!
//! Tests swap the runner for a recording one and check the argument vectors
//! without ever starting a process.

use std::env;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::model::{Agent, AgentStatus, Pane, ProcessInfo, Snapshot, Tab, Workspace};

/// Environment variable Herdr sets to the path of its own binary.
pub const HERDR_BIN_PATH: &str = "HERDR_BIN_PATH";

/// Binary name used when `HERDR_BIN_PATH` is not set.
pub const DEFAULT_BINARY: &str = "herdr";

/// Time a `herdr` call may take before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a `herdr` call did not produce a result.
#[derive(Debug)]
pub enum HerdrError {
    /// The `herdr` process could not be started.
    Spawn { binary: PathBuf, source: io::Error },
    /// The process ran longer than the timeout and was killed.
    Timeout { after: Duration },
    /// The process produced no JSON the client understands.
    Exec { status: Option<i32>, stderr: String },
    /// Herdr answered with an error object.
    Api { code: String, message: String },
    /// Herdr answered, but not with the shape the typed wrapper expects.
    Decode {
        context: &'static str,
        source: serde_json::Error,
    },
}

impl HerdrError {
    /// The Herdr error code, when the error is an API error.
    pub fn api_code(&self) -> Option<&str> {
        match self {
            HerdrError::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    fn decode(context: &'static str, source: serde_json::Error) -> Self {
        HerdrError::Decode { context, source }
    }

    fn missing(context: &'static str, what: &str) -> Self {
        HerdrError::decode(
            context,
            <serde_json::Error as serde::de::Error>::custom(format!("missing {what}")),
        )
    }
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HerdrError::Spawn { binary, source } => {
                write!(f, "cannot run {}: {source}", binary.display())
            }
            HerdrError::Timeout { after } => {
                write!(f, "herdr did not answer within {:.1}s", after.as_secs_f64())
            }
            HerdrError::Exec { status, stderr } => {
                match status {
                    Some(code) => write!(f, "herdr exited with status {code}")?,
                    None => write!(f, "herdr was killed by a signal")?,
                }
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            HerdrError::Api { code, message } => write!(f, "herdr: {code}: {message}"),
            HerdrError::Decode { context, source } => {
                write!(f, "unexpected herdr output for {context}: {source}")
            }
        }
    }
}

impl std::error::Error for HerdrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HerdrError::Spawn { source, .. } => Some(source),
            HerdrError::Decode { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// What a finished `herdr` process left behind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    /// Exit status code; `None` when the process died of a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs the `herdr` binary. The real implementation spawns a process; tests
/// substitute one that records the arguments.
pub trait CommandRunner: Send + Sync {
    /// Run `binary` with `args`, stdin closed, and capture both output streams.
    ///
    /// Returns [`HerdrError::Spawn`] when the process cannot start and
    /// [`HerdrError::Timeout`] when it outlives `timeout`.
    fn run(
        &self,
        binary: &Path,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, HerdrError>;
}

/// The runner that starts a real process.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRunner;

impl ProcessRunner {
    const POLL_INTERVAL: Duration = Duration::from_millis(10);
}

impl CommandRunner for ProcessRunner {
    fn run(
        &self,
        binary: &Path,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, HerdrError> {
        let mut child = Command::new(binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| HerdrError::Spawn {
                binary: binary.to_path_buf(),
                source,
            })?;

        // Both pipes are drained from their own thread so a chatty command
        // never blocks on a full pipe while we wait for it to exit.
        let stdout = spawn_reader(child.stdout.take());
        let stderr = spawn_reader(child.stderr.take());

        let status = match wait_with_timeout(&mut child, timeout) {
            Ok(status) => status,
            Err(err) => {
                // The reader threads are left to finish on their own: a
                // grandchild that inherited the pipes could hold them open
                // after the child is gone, and the caller must not wait for
                // that.
                let _ = child.kill();
                let _ = child.wait();
                drop(stdout);
                drop(stderr);
                return Err(err);
            }
        };

        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        Ok(CommandOutput {
            status: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

fn spawn_reader<R: Read + Send + 'static>(stream: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut buffer);
        }
        buffer
    })
}

fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, HerdrError> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(source) => {
                return Err(HerdrError::Spawn {
                    binary: PathBuf::new(),
                    source,
                });
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(HerdrError::Timeout { after: timeout });
        }
        thread::sleep(ProcessRunner::POLL_INTERVAL.min(deadline - now));
    }
}

/// Output flavour of `herdr pane read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFormat {
    /// Plain text, styling stripped.
    Text,
    /// Text with its escape sequences.
    Ansi,
}

impl ReadFormat {
    fn as_flag_value(self) -> &'static str {
        match self {
            ReadFormat::Text => "text",
            ReadFormat::Ansi => "ansi",
        }
    }
}

/// One change to the metadata tokens of a pane or a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataChange {
    /// Set `name` to `value`.
    Token { name: String, value: String },
    /// Remove `name`.
    ClearToken { name: String },
}

impl MetadataChange {
    /// A `Token` change.
    pub fn token(name: impl Into<String>, value: impl Into<String>) -> Self {
        MetadataChange::Token {
            name: name.into(),
            value: value.into(),
        }
    }

    /// A `ClearToken` change.
    pub fn clear_token(name: impl Into<String>) -> Self {
        MetadataChange::ClearToken { name: name.into() }
    }

    fn push_args(&self, args: &mut Vec<String>) {
        match self {
            MetadataChange::Token { name, value } => {
                args.push("--token".into());
                args.push(format!("{name}={value}"));
            }
            MetadataChange::ClearToken { name } => {
                args.push("--clear-token".into());
                args.push(name.clone());
            }
        }
    }
}

/// What `herdr pane report-agent` needs: the lifecycle state a source claims
/// for the agent in a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportAgent {
    pub pane: String,
    /// Reporter identifier (`--source`).
    pub source: String,
    /// Agent label (`--agent`), for instance `sleeping-claude`.
    pub agent: String,
    /// Reported state. Herdr accepts `idle`, `working`, `blocked` and
    /// `unknown` here, never `done`.
    pub state: AgentStatus,
    pub message: Option<String>,
    pub seq: u64,
    pub session_id: Option<String>,
    pub session_path: Option<String>,
    /// Command that resumes the session after a Herdr restart, passed after
    /// `--`. Empty means none.
    pub resume: Vec<String>,
}

/// What `herdr pane report-agent-session` needs: the Session identity a
/// source claims for the agent in a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportAgentSession {
    pub pane: String,
    pub source: String,
    pub agent: String,
    pub seq: u64,
    pub session_id: Option<String>,
    pub session_path: Option<String>,
    pub session_start_source: Option<String>,
    /// Command that resumes the session after a Herdr restart, passed after
    /// `--`. Empty means none.
    pub resume: Vec<String>,
}

/// Client of the `herdr` command line.
#[derive(Clone)]
pub struct HerdrClient {
    binary: PathBuf,
    timeout: Duration,
    runner: Arc<dyn CommandRunner>,
}

impl fmt::Debug for HerdrClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HerdrClient")
            .field("binary", &self.binary)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl HerdrClient {
    /// Client for the binary Herdr announces in `HERDR_BIN_PATH`, or `herdr`
    /// on `PATH` when the variable is absent or empty.
    pub fn from_env() -> Self {
        let binary = env::var_os(HERDR_BIN_PATH)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY));
        Self::with_binary(binary)
    }

    /// Client for a specific binary.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self::with_runner(binary, Arc::new(ProcessRunner))
    }

    /// Client whose processes are run by `runner`.
    pub fn with_runner(binary: impl Into<PathBuf>, runner: Arc<dyn CommandRunner>) -> Self {
        HerdrClient {
            binary: binary.into(),
            timeout: DEFAULT_TIMEOUT,
            runner,
        }
    }

    /// Same client with another per-call timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The binary this client runs.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The per-call timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Run `herdr <args>` and return the `result` value of its JSON answer.
    pub fn call(&self, args: &[&str]) -> Result<Value, HerdrError> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.call_owned(&args)
    }

    fn call_owned(&self, args: &[String]) -> Result<Value, HerdrError> {
        let output = self.runner.run(&self.binary, args, self.timeout)?;
        interpret_output(&output)
    }

    /// Run a call and deserialise `result.<key>`.
    fn fetch<T: DeserializeOwned>(
        &self,
        args: &[&str],
        key: &'static str,
    ) -> Result<T, HerdrError> {
        let result = self.call(args)?;
        let value = result
            .get(key)
            .cloned()
            .ok_or_else(|| HerdrError::missing(key, &format!("'{key}' in result")))?;
        serde_json::from_value(value).map_err(|e| HerdrError::decode(key, e))
    }

    /// Run a call whose result is not needed.
    fn act(&self, args: &[String]) -> Result<(), HerdrError> {
        self.call_owned(args).map(|_| ())
    }

    // Queries ------------------------------------------------------------

    /// `herdr api snapshot`
    pub fn snapshot(&self) -> Result<Snapshot, HerdrError> {
        self.fetch(&["api", "snapshot"], "snapshot")
    }

    /// `herdr agent list`
    pub fn agent_list(&self) -> Result<Vec<Agent>, HerdrError> {
        self.fetch(&["agent", "list"], "agents")
    }

    /// `herdr agent get <target>` (a pane id or a unique agent name).
    pub fn agent_get(&self, target: &str) -> Result<Agent, HerdrError> {
        self.fetch(&["agent", "get", target], "agent")
    }

    /// `herdr pane list [--workspace <id>]`
    pub fn pane_list(&self, workspace: Option<&str>) -> Result<Vec<Pane>, HerdrError> {
        let mut args = vec!["pane", "list"];
        if let Some(workspace) = workspace {
            args.extend(["--workspace", workspace]);
        }
        self.fetch(&args, "panes")
    }

    /// `herdr pane get <pane>`
    pub fn pane_get(&self, pane: &str) -> Result<Pane, HerdrError> {
        self.fetch(&["pane", "get", pane], "pane")
    }

    /// `herdr pane process-info --pane <pane>`
    pub fn pane_process_info(&self, pane: &str) -> Result<ProcessInfo, HerdrError> {
        self.fetch(&["pane", "process-info", "--pane", pane], "process_info")
    }

    /// `herdr tab list [--workspace <id>]`
    pub fn tab_list(&self, workspace: Option<&str>) -> Result<Vec<Tab>, HerdrError> {
        let mut args = vec!["tab", "list"];
        if let Some(workspace) = workspace {
            args.extend(["--workspace", workspace]);
        }
        self.fetch(&args, "tabs")
    }

    /// `herdr workspace list`
    pub fn workspace_list(&self) -> Result<Vec<Workspace>, HerdrError> {
        self.fetch(&["workspace", "list"], "workspaces")
    }

    /// `herdr pane read <pane> --lines <n> --format <format>`: the last
    /// `lines` rows of the pane's recent output.
    pub fn pane_read(
        &self,
        pane: &str,
        lines: usize,
        format: ReadFormat,
    ) -> Result<String, HerdrError> {
        let lines = lines.to_string();
        let result = self.call(&[
            "pane",
            "read",
            pane,
            "--lines",
            &lines,
            "--format",
            format.as_flag_value(),
        ])?;
        extract_read_text(&result).ok_or_else(|| {
            HerdrError::missing("pane read", "text in result (text, content or lines)")
        })
    }

    // Actions ------------------------------------------------------------

    /// `herdr pane run <pane> <argv...>`: type a command and press Enter.
    pub fn pane_run(&self, pane: &str, argv: &[&str]) -> Result<(), HerdrError> {
        let mut args = strings(&["pane", "run", pane]);
        push_positionals(&mut args, argv);
        self.act(&args)
    }

    /// `herdr pane send-text <pane> <text>`: type text without Enter.
    pub fn pane_send_text(&self, pane: &str, text: &str) -> Result<(), HerdrError> {
        let mut args = strings(&["pane", "send-text", pane]);
        push_positionals(&mut args, &[text]);
        self.act(&args)
    }

    /// `herdr pane send-keys <pane> <keys...>`: press logical keys such as
    /// `enter`, `esc` or `ctrl+c`.
    pub fn pane_send_keys(&self, pane: &str, keys: &[&str]) -> Result<(), HerdrError> {
        let mut args = strings(&["pane", "send-keys", pane]);
        push_positionals(&mut args, keys);
        self.act(&args)
    }

    /// `herdr pane report-agent ...`
    pub fn pane_report_agent(&self, report: &ReportAgent) -> Result<(), HerdrError> {
        let mut args = strings(&[
            "pane",
            "report-agent",
            "--source",
            &report.source,
            "--agent",
            &report.agent,
            "--state",
            report.state.as_str(),
        ]);
        if let Some(message) = &report.message {
            args.push("--message".into());
            args.push(message.clone());
        }
        args.push("--seq".into());
        args.push(report.seq.to_string());
        push_session_flags(
            &mut args,
            report.session_id.as_deref(),
            report.session_path.as_deref(),
        );
        args.push(report.pane.clone());
        push_resume(&mut args, &report.resume);
        self.act(&args)
    }

    /// `herdr pane report-agent-session ...`
    pub fn pane_report_agent_session(&self, report: &ReportAgentSession) -> Result<(), HerdrError> {
        let mut args = strings(&[
            "pane",
            "report-agent-session",
            "--source",
            &report.source,
            "--agent",
            &report.agent,
            "--seq",
            &report.seq.to_string(),
        ]);
        push_session_flags(
            &mut args,
            report.session_id.as_deref(),
            report.session_path.as_deref(),
        );
        if let Some(start) = &report.session_start_source {
            args.push("--session-start-source".into());
            args.push(start.clone());
        }
        args.push(report.pane.clone());
        push_resume(&mut args, &report.resume);
        self.act(&args)
    }

    /// `herdr pane release-agent --source <source> --agent <agent> --seq <seq> <pane>`
    pub fn pane_release_agent(
        &self,
        pane: &str,
        source: &str,
        agent: &str,
        seq: u64,
    ) -> Result<(), HerdrError> {
        let seq = seq.to_string();
        let args = strings(&[
            "pane",
            "release-agent",
            "--source",
            source,
            "--agent",
            agent,
            "--seq",
            &seq,
            pane,
        ]);
        self.act(&args)
    }

    /// `herdr pane report-metadata --source <source> (--token|--clear-token) ... --seq <seq> <pane>`
    pub fn pane_report_metadata(
        &self,
        pane: &str,
        source: &str,
        change: &MetadataChange,
        seq: u64,
    ) -> Result<(), HerdrError> {
        let mut args = strings(&["pane", "report-metadata", "--source", source]);
        change.push_args(&mut args);
        args.push("--seq".into());
        args.push(seq.to_string());
        args.push(pane.into());
        self.act(&args)
    }

    /// `herdr workspace report-metadata --source <source> (--token|--clear-token) ... --seq <seq> <workspace>`
    pub fn workspace_report_metadata(
        &self,
        workspace: &str,
        source: &str,
        change: &MetadataChange,
        seq: u64,
    ) -> Result<(), HerdrError> {
        let mut args = strings(&["workspace", "report-metadata", "--source", source]);
        change.push_args(&mut args);
        args.push("--seq".into());
        args.push(seq.to_string());
        args.push(workspace.into());
        self.act(&args)
    }

    /// `herdr tab rename <tab> <label>`
    pub fn tab_rename(&self, tab: &str, label: &str) -> Result<(), HerdrError> {
        let mut args = strings(&["tab", "rename", tab]);
        push_positionals(&mut args, &[label]);
        self.act(&args)
    }

    /// `herdr tab focus <tab>`
    pub fn tab_focus(&self, tab: &str) -> Result<(), HerdrError> {
        self.act(&strings(&["tab", "focus", tab]))
    }

    /// `herdr agent rename <pane> <name>`
    pub fn agent_rename(&self, pane: &str, name: &str) -> Result<(), HerdrError> {
        let mut args = strings(&["agent", "rename", pane]);
        push_positionals(&mut args, &[name]);
        self.act(&args)
    }

    /// `herdr agent focus <pane>`
    pub fn agent_focus(&self, pane: &str) -> Result<(), HerdrError> {
        self.act(&strings(&["agent", "focus", pane]))
    }

    /// `herdr notification show --body <body> <title>`
    pub fn notify(&self, title: &str, body: &str) -> Result<(), HerdrError> {
        let mut args = strings(&["notification", "show", "--body", body]);
        push_positionals(&mut args, &[title]);
        self.act(&args)
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Append free-form positionals, guarded by `--` when one of them could be
/// mistaken for an option.
fn push_positionals(args: &mut Vec<String>, values: &[&str]) {
    if values.iter().any(|v| v.starts_with('-')) {
        args.push("--".into());
    }
    args.extend(values.iter().map(|v| v.to_string()));
}

fn push_session_flags(args: &mut Vec<String>, id: Option<&str>, path: Option<&str>) {
    if let Some(id) = id {
        args.push("--agent-session-id".into());
        args.push(id.into());
    }
    if let Some(path) = path {
        args.push("--agent-session-path".into());
        args.push(path.into());
    }
}

fn push_resume(args: &mut Vec<String>, resume: &[String]) {
    if !resume.is_empty() {
        args.push("--".into());
        args.extend(resume.iter().cloned());
    }
}

/// Turn what a `herdr` process printed into a result or an error.
pub fn interpret_output(output: &CommandOutput) -> Result<Value, HerdrError> {
    if let Some(value) = parse_json(&output.stdout) {
        if let Some(error) = api_error(&value) {
            return Err(error);
        }
        return Ok(match value {
            Value::Object(mut object) => object.remove("result").unwrap_or(Value::Null),
            other => other,
        });
    }
    if let Some(error) = parse_json(&output.stderr).as_ref().and_then(api_error) {
        return Err(error);
    }
    Err(HerdrError::Exec {
        status: output.status,
        stderr: output.stderr.trim().to_string(),
    })
}

fn parse_json(text: &str) -> Option<Value> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    serde_json::from_str(text).ok()
}

fn api_error(value: &Value) -> Option<HerdrError> {
    let error = value.get("error")?.as_object()?;
    Some(HerdrError::Api {
        code: string_field(error, "code").unwrap_or_else(|| "unknown".into()),
        message: string_field(error, "message").unwrap_or_default(),
    })
}

fn string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Pull the captured text out of a `pane read` result.
///
/// The result is read defensively because its exact layout is not pinned
/// down: a string field named `text` or `content` is taken as is, a `lines`
/// array of strings is joined with newlines, a bare string result is taken
/// whole, and the same lookup is repeated one level down (for a result
/// wrapped in an object such as `output` or `read`).
pub fn extract_read_text(result: &Value) -> Option<String> {
    fn direct(value: &Value) -> Option<String> {
        if let Some(text) = value.as_str() {
            return Some(text.to_string());
        }
        let object = value.as_object()?;
        for key in ["text", "content"] {
            if let Some(text) = object.get(key).and_then(Value::as_str) {
                return Some(text.to_string());
            }
        }
        let lines = object.get("lines")?.as_array()?;
        let lines: Option<Vec<&str>> = lines.iter().map(Value::as_str).collect();
        lines.map(|lines| lines.join("\n"))
    }

    direct(result).or_else(|| {
        result
            .as_object()?
            .values()
            .filter(|v| v.is_object())
            .find_map(direct)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Records the argument vectors it receives and replays canned outputs.
    #[derive(Default)]
    struct RecordingRunner {
        calls: Mutex<Vec<(PathBuf, Vec<String>, Duration)>>,
        outputs: Mutex<VecDeque<CommandOutput>>,
    }

    impl RecordingRunner {
        fn with_stdout(stdout: &str) -> Arc<Self> {
            let runner = Arc::new(Self::default());
            runner.push_stdout(stdout);
            runner
        }

        fn push_stdout(&self, stdout: &str) {
            self.outputs.lock().unwrap().push_back(CommandOutput {
                status: Some(0),
                stdout: stdout.to_string(),
                stderr: String::new(),
            });
        }

        fn argv(&self) -> Vec<String> {
            let calls = self.calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "expected exactly one call");
            calls[0].1.clone()
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(
            &self,
            binary: &Path,
            args: &[String],
            timeout: Duration,
        ) -> Result<CommandOutput, HerdrError> {
            self.calls
                .lock()
                .unwrap()
                .push((binary.to_path_buf(), args.to_vec(), timeout));
            Ok(self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| CommandOutput {
                    status: Some(0),
                    stdout: r#"{"id":"t","result":{"ok":true}}"#.into(),
                    stderr: String::new(),
                }))
        }
    }

    const OK: &str = r#"{"id":"t","result":{"ok":true}}"#;

    fn client() -> (HerdrClient, Arc<RecordingRunner>) {
        let runner = RecordingRunner::with_stdout(OK);
        let client = HerdrClient::with_runner("/opt/herdr", runner.clone());
        (client, runner)
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn argv_of(f: impl FnOnce(&HerdrClient)) -> Vec<String> {
        let (client, runner) = client();
        f(&client);
        runner.argv()
    }

    fn v(args: &[&str]) -> Vec<String> {
        strings(args)
    }

    #[test]
    fn builder_records_binary_and_timeout() {
        let (client, runner) = client();
        let client = client.with_timeout(Duration::from_secs(3));
        assert_eq!(client.binary(), Path::new("/opt/herdr"));
        assert_eq!(client.timeout(), Duration::from_secs(3));
        client.call(&["agent", "list"]).unwrap();
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0].0, PathBuf::from("/opt/herdr"));
        assert_eq!(calls[0].2, Duration::from_secs(3));
    }

    #[test]
    fn default_timeout_is_fifteen_seconds() {
        assert_eq!(HerdrClient::with_binary("herdr").timeout(), DEFAULT_TIMEOUT);
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn call_returns_the_result_value() {
        let (client, _) = client();
        assert_eq!(
            client.call(&["x"]).unwrap(),
            serde_json::json!({"ok": true})
        );
    }

    #[test]
    fn interpret_stdout_error_object() {
        let output = CommandOutput {
            status: Some(0),
            stdout: fixture("pane-get-missing.json"),
            stderr: String::new(),
        };
        match interpret_output(&output) {
            Err(HerdrError::Api { code, message }) => {
                assert_eq!(code, "pane_not_found");
                assert_eq!(message, "pane nope:p9 not found");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn interpret_stderr_error_object() {
        let output = CommandOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: r#"{"error":{"code":"server_unavailable","message":"no server"}}"#.into(),
        };
        let err = interpret_output(&output).unwrap_err();
        assert_eq!(err.api_code(), Some("server_unavailable"));
        assert_eq!(err.to_string(), "herdr: server_unavailable: no server");
    }

    #[test]
    fn interpret_garbage_is_exec_error() {
        let output = CommandOutput {
            status: Some(2),
            stdout: "usage: herdr ...".into(),
            stderr: "error: unexpected argument\n".into(),
        };
        match interpret_output(&output) {
            Err(HerdrError::Exec { status, stderr }) => {
                assert_eq!(status, Some(2));
                assert_eq!(stderr, "error: unexpected argument");
            }
            other => panic!("unexpected: {other:?}"),
        }
        let killed = CommandOutput::default();
        assert_eq!(
            interpret_output(&killed).unwrap_err().to_string(),
            "herdr was killed by a signal"
        );
    }

    #[test]
    fn interpret_result_without_result_key_is_null() {
        let output = CommandOutput {
            status: Some(0),
            stdout: r#"{"id":"t"}"#.into(),
            stderr: String::new(),
        };
        assert_eq!(interpret_output(&output).unwrap(), Value::Null);
    }

    #[test]
    fn error_display_and_source() {
        let spawn = HerdrError::Spawn {
            binary: "/nope/herdr".into(),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert!(spawn.to_string().starts_with("cannot run /nope/herdr: "));
        assert!(std::error::Error::source(&spawn).is_some());
        let timeout = HerdrError::Timeout {
            after: Duration::from_millis(1500),
        };
        assert_eq!(timeout.to_string(), "herdr did not answer within 1.5s");
        assert!(std::error::Error::source(&timeout).is_none());
    }

    #[test]
    fn typed_queries_parse_fixtures_and_build_argv() {
        type Case = (&'static str, fn(&HerdrClient), &'static [&'static str]);
        let cases: [Case; 8] = [
            (
                "snapshot.json",
                |c| {
                    let s = c.snapshot().unwrap();
                    assert_eq!(s.panes.len(), 11);
                },
                &["api", "snapshot"],
            ),
            (
                "agent-list.json",
                |c| {
                    assert_eq!(c.agent_list().unwrap().len(), 6);
                },
                &["agent", "list"],
            ),
            (
                "agent-get.json",
                |c| {
                    assert_eq!(c.agent_get("wC:p1").unwrap().pane_id, "wC:p1");
                },
                &["agent", "get", "wC:p1"],
            ),
            (
                "pane-list.json",
                |c| {
                    assert_eq!(c.pane_list(Some("wC")).unwrap().len(), 11);
                },
                &["pane", "list", "--workspace", "wC"],
            ),
            (
                "pane-get-current.json",
                |c| {
                    assert_eq!(c.pane_get("wC:p1").unwrap().tab_id, "wC:t1");
                },
                &["pane", "get", "wC:p1"],
            ),
            (
                "process-info-current.json",
                |c| {
                    assert_eq!(c.pane_process_info("wC:p1").unwrap().shell_pid, 184186);
                },
                &["pane", "process-info", "--pane", "wC:p1"],
            ),
            (
                "tab-list.json",
                |c| {
                    assert_eq!(c.tab_list(None).unwrap().len(), 6);
                },
                &["tab", "list"],
            ),
            (
                "workspace-list.json",
                |c| {
                    assert_eq!(c.workspace_list().unwrap().len(), 6);
                },
                &["workspace", "list"],
            ),
        ];
        for (name, check, expected) in cases {
            let runner = RecordingRunner::with_stdout(&fixture(name));
            let client = HerdrClient::with_runner("herdr", runner.clone());
            check(&client);
            assert_eq!(runner.argv(), v(expected), "{name}");
        }
    }

    #[test]
    fn pane_list_without_workspace() {
        let runner = RecordingRunner::with_stdout(&fixture("pane-list.json"));
        let client = HerdrClient::with_runner("herdr", runner.clone());
        client.pane_list(None).unwrap();
        assert_eq!(runner.argv(), v(&["pane", "list"]));
    }

    #[test]
    fn tab_list_with_workspace() {
        let runner = RecordingRunner::with_stdout(&fixture("tab-list.json"));
        let client = HerdrClient::with_runner("herdr", runner.clone());
        client.tab_list(Some("w3")).unwrap();
        assert_eq!(runner.argv(), v(&["tab", "list", "--workspace", "w3"]));
    }

    #[test]
    fn typed_query_with_missing_key_is_decode_error() {
        let (client, _) = client();
        match client.agent_list() {
            Err(HerdrError::Decode { context, .. }) => assert_eq!(context, "agents"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn api_error_surfaces_from_typed_query() {
        let runner = RecordingRunner::with_stdout(&fixture("pane-get-missing.json"));
        let client = HerdrClient::with_runner("herdr", runner);
        assert_eq!(
            client.pane_get("nope:p9").unwrap_err().api_code(),
            Some("pane_not_found")
        );
    }

    #[test]
    fn pane_read_argv_and_text_shapes() {
        for (stdout, expected) in [
            (r#"{"result":{"text":"a\nb"}}"#, "a\nb"),
            (r#"{"result":{"content":"c"}}"#, "c"),
            (r#"{"result":{"lines":["l1","l2"]}}"#, "l1\nl2"),
            (r#"{"result":{"output":{"text":"nested"}}}"#, "nested"),
            (r#"{"result":"bare"}"#, "bare"),
        ] {
            let runner = RecordingRunner::with_stdout(stdout);
            let client = HerdrClient::with_runner("herdr", runner.clone());
            assert_eq!(
                client.pane_read("w1:p1", 40, ReadFormat::Text).unwrap(),
                expected
            );
            assert_eq!(
                runner.argv(),
                v(&["pane", "read", "w1:p1", "--lines", "40", "--format", "text"])
            );
        }
        let runner = RecordingRunner::with_stdout(r#"{"result":{"rows":3}}"#);
        let client = HerdrClient::with_runner("herdr", runner.clone());
        assert!(matches!(
            client.pane_read("w1:p1", 5, ReadFormat::Ansi),
            Err(HerdrError::Decode {
                context: "pane read",
                ..
            })
        ));
        assert_eq!(runner.argv()[6], "ansi");
    }

    #[test]
    fn pane_run_argv() {
        let argv = argv_of(|c| c.pane_run("w1:p1", &["cargo", "test"]).unwrap());
        assert_eq!(argv, v(&["pane", "run", "w1:p1", "cargo", "test"]));
        // A command carrying option-like words is fenced off with `--`.
        let argv = argv_of(|c| {
            c.pane_run("w1:p1", &["exec", "/opt/ph", "--pane", "w1:p1"])
                .unwrap()
        });
        assert_eq!(
            argv,
            v(&[
                "pane", "run", "w1:p1", "--", "exec", "/opt/ph", "--pane", "w1:p1"
            ])
        );
    }

    #[test]
    fn pane_send_text_argv() {
        let argv = argv_of(|c| c.pane_send_text("w1:p1", "/exit").unwrap());
        assert_eq!(argv, v(&["pane", "send-text", "w1:p1", "/exit"]));
        let argv = argv_of(|c| c.pane_send_text("w1:p1", "-x").unwrap());
        assert_eq!(argv, v(&["pane", "send-text", "w1:p1", "--", "-x"]));
    }

    #[test]
    fn pane_send_keys_argv() {
        let argv = argv_of(|c| c.pane_send_keys("w1:p1", &["esc", "enter"]).unwrap());
        assert_eq!(argv, v(&["pane", "send-keys", "w1:p1", "esc", "enter"]));
    }

    #[test]
    fn pane_report_agent_argv_full() {
        let report = ReportAgent {
            pane: "w1:p1".into(),
            source: "herdr-sleep".into(),
            agent: "sleeping-claude".into(),
            state: AgentStatus::Idle,
            message: Some("asleep".into()),
            seq: 7,
            session_id: Some("sid".into()),
            session_path: Some("/tmp/s.jsonl".into()),
            resume: v(&["herdr-sleep-ph", "--pane", "w1:p1"]),
        };
        let argv = argv_of(|c| c.pane_report_agent(&report).unwrap());
        assert_eq!(
            argv,
            v(&[
                "pane",
                "report-agent",
                "--source",
                "herdr-sleep",
                "--agent",
                "sleeping-claude",
                "--state",
                "idle",
                "--message",
                "asleep",
                "--seq",
                "7",
                "--agent-session-id",
                "sid",
                "--agent-session-path",
                "/tmp/s.jsonl",
                "w1:p1",
                "--",
                "herdr-sleep-ph",
                "--pane",
                "w1:p1",
            ])
        );
    }

    #[test]
    fn pane_report_agent_argv_minimal() {
        let report = ReportAgent {
            pane: "w1:p1".into(),
            source: "herdr-sleep".into(),
            agent: "sleeping-codex".into(),
            state: AgentStatus::Working,
            message: None,
            seq: 0,
            session_id: None,
            session_path: None,
            resume: Vec::new(),
        };
        let argv = argv_of(|c| c.pane_report_agent(&report).unwrap());
        assert_eq!(
            argv,
            v(&[
                "pane",
                "report-agent",
                "--source",
                "herdr-sleep",
                "--agent",
                "sleeping-codex",
                "--state",
                "working",
                "--seq",
                "0",
                "w1:p1",
            ])
        );
    }

    #[test]
    fn pane_report_agent_session_argv() {
        let report = ReportAgentSession {
            pane: "w1:p1".into(),
            source: "herdr-sleep".into(),
            agent: "sleeping-claude".into(),
            seq: 3,
            session_id: Some("sid".into()),
            session_path: None,
            session_start_source: Some("resume".into()),
            resume: v(&["claude", "--resume", "sid"]),
        };
        let argv = argv_of(|c| c.pane_report_agent_session(&report).unwrap());
        assert_eq!(
            argv,
            v(&[
                "pane",
                "report-agent-session",
                "--source",
                "herdr-sleep",
                "--agent",
                "sleeping-claude",
                "--seq",
                "3",
                "--agent-session-id",
                "sid",
                "--session-start-source",
                "resume",
                "w1:p1",
                "--",
                "claude",
                "--resume",
                "sid",
            ])
        );
    }

    #[test]
    fn pane_release_agent_argv() {
        let argv = argv_of(|c| {
            c.pane_release_agent("w1:p1", "herdr-sleep", "sleeping-claude", 9)
                .unwrap()
        });
        assert_eq!(
            argv,
            v(&[
                "pane",
                "release-agent",
                "--source",
                "herdr-sleep",
                "--agent",
                "sleeping-claude",
                "--seq",
                "9",
                "w1:p1",
            ])
        );
    }

    #[test]
    fn pane_report_metadata_argv() {
        let set = MetadataChange::token("sleep", "zz");
        let argv = argv_of(|c| {
            c.pane_report_metadata("w1:p1", "herdr-sleep", &set, 4)
                .unwrap()
        });
        assert_eq!(
            argv,
            v(&[
                "pane",
                "report-metadata",
                "--source",
                "herdr-sleep",
                "--token",
                "sleep=zz",
                "--seq",
                "4",
                "w1:p1"
            ])
        );
        let clear = MetadataChange::clear_token("sleep");
        let argv = argv_of(|c| {
            c.pane_report_metadata("w1:p1", "herdr-sleep", &clear, 5)
                .unwrap()
        });
        assert_eq!(
            argv,
            v(&[
                "pane",
                "report-metadata",
                "--source",
                "herdr-sleep",
                "--clear-token",
                "sleep",
                "--seq",
                "5",
                "w1:p1"
            ])
        );
    }

    #[test]
    fn workspace_report_metadata_argv() {
        let set = MetadataChange::token("sleep", "zz");
        let argv = argv_of(|c| {
            c.workspace_report_metadata("w1", "herdr-sleep", &set, 1)
                .unwrap()
        });
        assert_eq!(
            argv,
            v(&[
                "workspace",
                "report-metadata",
                "--source",
                "herdr-sleep",
                "--token",
                "sleep=zz",
                "--seq",
                "1",
                "w1"
            ])
        );
        let clear = MetadataChange::clear_token("sleep");
        let argv = argv_of(|c| {
            c.workspace_report_metadata("w1", "herdr-sleep", &clear, 2)
                .unwrap()
        });
        assert_eq!(
            argv,
            v(&[
                "workspace",
                "report-metadata",
                "--source",
                "herdr-sleep",
                "--clear-token",
                "sleep",
                "--seq",
                "2",
                "w1"
            ])
        );
    }

    #[test]
    fn tab_and_agent_argv() {
        assert_eq!(
            argv_of(|c| c.tab_rename("w1:t1", "zz build").unwrap()),
            v(&["tab", "rename", "w1:t1", "zz build"])
        );
        assert_eq!(
            argv_of(|c| c.tab_focus("w1:t1").unwrap()),
            v(&["tab", "focus", "w1:t1"])
        );
        assert_eq!(
            argv_of(|c| c.agent_rename("w1:p1", "reviewer").unwrap()),
            v(&["agent", "rename", "w1:p1", "reviewer"])
        );
        assert_eq!(
            argv_of(|c| c.agent_focus("w1:p1").unwrap()),
            v(&["agent", "focus", "w1:p1"])
        );
    }

    #[test]
    fn notify_argv() {
        assert_eq!(
            argv_of(|c| c.notify("Herdr Sleep", "claude is asleep").unwrap()),
            v(&[
                "notification",
                "show",
                "--body",
                "claude is asleep",
                "Herdr Sleep"
            ])
        );
        assert_eq!(
            argv_of(|c| c.notify("-title", "b").unwrap()),
            v(&["notification", "show", "--body", "b", "--", "-title"])
        );
    }

    #[test]
    fn extract_read_text_shapes() {
        use serde_json::json;
        assert_eq!(
            extract_read_text(&json!({"text": "t"})).as_deref(),
            Some("t")
        );
        assert_eq!(extract_read_text(&json!({"lines": ["a", 1]})), None);
        assert_eq!(
            extract_read_text(&json!({"lines": []})).as_deref(),
            Some("")
        );
        assert_eq!(extract_read_text(&json!(42)), None);
        assert_eq!(
            extract_read_text(&json!({"a": {"b": {"text": "deep"}}})),
            None
        );
    }
}

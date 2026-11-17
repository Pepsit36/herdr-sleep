//! Typed views of what the `herdr` CLI reports.
//!
//! Every struct ignores fields it does not know and treats missing optional
//! fields as `None` or their default, so a newer Herdr adding keys never breaks
//! parsing.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Prefix of the label a Placeholder reports for a Sleeping agent.
pub const SLEEPING_PREFIX: &str = "sleeping-";

/// Lifecycle status Herdr assigns to an agent, tab or workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    /// Herdr cannot classify the agent, or reported a status this build does
    /// not know.
    #[default]
    #[serde(other)]
    Unknown,
}

impl AgentStatus {
    /// Statuses that mean the agent has nothing in progress (Settled).
    pub const SETTLED: [AgentStatus; 2] = [AgentStatus::Idle, AgentStatus::Done];

    /// The lowercase name Herdr uses for this status.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string is not a known agent status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAgentStatus(pub String);

impl fmt::Display for UnknownAgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown agent status '{}'", self.0)
    }
}

impl std::error::Error for UnknownAgentStatus {}

impl FromStr for AgentStatus {
    type Err = UnknownAgentStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "idle" => Ok(AgentStatus::Idle),
            "working" => Ok(AgentStatus::Working),
            "blocked" => Ok(AgentStatus::Blocked),
            "done" => Ok(AgentStatus::Done),
            "unknown" => Ok(AgentStatus::Unknown),
            other => Err(UnknownAgentStatus(other.to_string())),
        }
    }
}

/// The Session an agent reported to Herdr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Who reported the session (`herdr:claude` for Herdr's own detection).
    pub source: String,
    /// Agent label the session belongs to.
    #[serde(default)]
    pub agent: Option<String>,
    /// How to read `value`: `id` for a session identifier, `path` for a file.
    #[serde(default)]
    pub kind: Option<String>,
    /// The session identifier or path.
    pub value: String,
}

/// Shared shape of an agent entry and a pane entry.
macro_rules! agent_fields_helpers {
    ($ty:ty) => {
        impl $ty {
            /// Directory the pane works in: the foreground process's cwd when
            /// Herdr knows it, the shell's cwd otherwise.
            pub fn effective_cwd(&self) -> Option<&str> {
                self.foreground_cwd.as_deref().or(self.cwd.as_deref())
            }

            /// The Session value reported for this pane, when there is one.
            pub fn session_id(&self) -> Option<&str> {
                self.agent_session.as_ref().map(|s| s.value.as_str())
            }

            /// Whether the pane holds a Placeholder (label prefixed with
            /// `sleeping-`).
            pub fn is_sleeping(&self) -> bool {
                self.agent
                    .as_deref()
                    .is_some_and(|label| label.starts_with(SLEEPING_PREFIX))
            }

            /// The Kind of the agent: the label without the `sleeping-` prefix.
            pub fn kind(&self) -> Option<&str> {
                self.agent
                    .as_deref()
                    .map(|label| label.strip_prefix(SLEEPING_PREFIX).unwrap_or(label))
            }

            /// Whether the agent's status is one of `statuses`.
            pub fn is_settled(&self, statuses: &[AgentStatus]) -> bool {
                statuses.contains(&self.agent_status)
            }
        }
    };
}

/// An agent as `herdr agent list`, `herdr agent get` and the snapshot report
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Agent {
    pub pane_id: String,
    /// User-given agent name, when Herdr reports one.
    #[serde(default)]
    pub name: Option<String>,
    /// Agent label (`claude`, `codex`, `sleeping-claude`, ...).
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub focused: bool,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub terminal_title: Option<String>,
    /// Metadata tokens reported on the pane, when Herdr includes them.
    #[serde(default)]
    pub tokens: Option<HashMap<String, String>>,
}

agent_fields_helpers!(Agent);

/// Scroll position of a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Scroll {
    #[serde(default)]
    pub max_offset_from_bottom: u64,
    #[serde(default)]
    pub offset_from_bottom: u64,
    #[serde(default)]
    pub viewport_rows: u64,
}

/// A pane as `herdr pane list`, `herdr pane get` and the snapshot report it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub focused: bool,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub terminal_title: Option<String>,
    #[serde(default)]
    pub tokens: Option<HashMap<String, String>>,
    #[serde(default)]
    pub scroll: Option<Scroll>,
    /// Why Herdr could not restore the pane as it was (for example
    /// `Saved directory is unavailable`).
    #[serde(default)]
    pub restore_error: Option<String>,
}

agent_fields_helpers!(Pane);

impl Pane {
    /// Whether a native agent (not a Placeholder) occupies the pane.
    pub fn has_agent(&self) -> bool {
        self.agent.is_some()
    }
}

/// A tab as `herdr tab list` and the snapshot report it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Tab {
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: u32,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub pane_count: u32,
    #[serde(default)]
    pub agent_status: AgentStatus,
}

/// A workspace as `herdr workspace list` and the snapshot report it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: u32,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub active_tab_id: Option<String>,
    #[serde(default)]
    pub pane_count: u32,
    #[serde(default)]
    pub tab_count: u32,
    #[serde(default)]
    pub agent_status: AgentStatus,
}

/// The whole session as `herdr api snapshot` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub panes: Vec<Pane>,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    #[serde(default)]
    pub focused_tab_id: Option<String>,
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
    /// Herdr version, as a string such as `0.9.5`.
    #[serde(default)]
    pub version: Option<String>,
    /// Protocol number of the snapshot format.
    #[serde(default)]
    pub protocol: Option<u64>,
}

impl Snapshot {
    /// The pane with this id, if the snapshot has it.
    pub fn pane(&self, pane_id: &str) -> Option<&Pane> {
        self.panes.iter().find(|p| p.pane_id == pane_id)
    }

    /// The agent entry of this pane, if an agent occupies it.
    pub fn agent(&self, pane_id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.pane_id == pane_id)
    }
}

/// One process in a pane's foreground process group.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForegroundProcess {
    pub pid: i32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// What `herdr pane process-info` reports.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pane_id: String,
    pub shell_pid: i32,
    #[serde(default)]
    pub foreground_process_group_id: i32,
    #[serde(default)]
    pub foreground_processes: Vec<ForegroundProcess>,
}

impl ProcessInfo {
    /// Whether the pane sits at its shell prompt: the foreground process group
    /// is the shell's own.
    pub fn at_shell(&self) -> bool {
        self.foreground_process_group_id == self.shell_pid
    }
}

/// The `error` object the CLI prints when a call fails.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const FAKE_UUID: &str = "11111111-2222-4333-8444-555555555555";

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn result(name: &str, key: &str) -> Value {
        fixture(name)["result"][key].clone()
    }

    #[test]
    fn status_round_trips_through_text() {
        for status in [
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Blocked,
            AgentStatus::Done,
            AgentStatus::Unknown,
        ] {
            assert_eq!(status.to_string().parse::<AgentStatus>(), Ok(status));
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{status}\""));
            assert_eq!(serde_json::from_str::<AgentStatus>(&json).unwrap(), status);
        }
        assert_eq!(
            "napping".parse::<AgentStatus>(),
            Err(UnknownAgentStatus("napping".into()))
        );
        assert_eq!(
            serde_json::from_str::<AgentStatus>("\"napping\"").unwrap(),
            AgentStatus::Unknown
        );
    }

    #[test]
    fn snapshot_fixture_parses() {
        let snapshot: Snapshot =
            serde_json::from_value(result("snapshot.json", "snapshot")).unwrap();
        assert_eq!(snapshot.agents.len(), 6);
        assert_eq!(snapshot.panes.len(), 11);
        assert_eq!(snapshot.tabs.len(), 6);
        assert_eq!(snapshot.workspaces.len(), 6);
        assert_eq!(snapshot.focused_pane_id.as_deref(), Some("wB:p1"));
        assert_eq!(snapshot.focused_tab_id.as_deref(), Some("wB:t1"));
        assert_eq!(snapshot.focused_workspace_id.as_deref(), Some("wB"));
        assert_eq!(snapshot.version.as_deref(), Some("0.9.5"));
        assert_eq!(snapshot.protocol, Some(22));

        let working = snapshot.agent("wC:p1").unwrap();
        assert_eq!(working.agent_status, AgentStatus::Working);
        assert_eq!(working.state_change_seq, 32);
        assert_eq!(working.session_id(), Some(FAKE_UUID));
        assert_eq!(working.kind(), Some("claude"));
        assert!(!working.is_sleeping());
        assert!(!working.is_settled(&AgentStatus::SETTLED));

        let focused = snapshot.agent("wB:p1").unwrap();
        assert!(focused.focused);
        assert_eq!(focused.agent_status, AgentStatus::Done);
        assert!(focused.is_settled(&AgentStatus::SETTLED));

        let shell = snapshot.pane("w5:p2").unwrap();
        assert!(!shell.has_agent());
        assert_eq!(shell.agent_status, AgentStatus::Unknown);
        assert_eq!(shell.kind(), None);
        assert_eq!(shell.session_id(), None);
        assert_eq!(shell.effective_cwd(), Some("/home/user/project-a/sub"));
        assert_eq!(shell.scroll.unwrap().viewport_rows, 76);
        assert!(snapshot.agent("w5:p2").is_none());
    }

    #[test]
    fn agent_list_fixture_parses() {
        let agents: Vec<Agent> =
            serde_json::from_value(result("agent-list.json", "agents")).unwrap();
        assert_eq!(agents.len(), 6);
        assert!(agents.iter().all(|a| a.agent.as_deref() == Some("claude")));
        assert!(agents.iter().all(|a| a.name.is_none()));
        assert!(agents.iter().all(|a| a.tokens.is_none()));
        let statuses: Vec<AgentStatus> = agents.iter().map(|a| a.agent_status).collect();
        assert_eq!(
            statuses,
            [
                AgentStatus::Idle,
                AgentStatus::Idle,
                AgentStatus::Idle,
                AgentStatus::Idle,
                AgentStatus::Done,
                AgentStatus::Working
            ]
        );
        let session = agents[0].agent_session.as_ref().unwrap();
        assert_eq!(session.source, "herdr:claude");
        assert_eq!(session.kind.as_deref(), Some("id"));
        assert_eq!(session.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn agent_get_fixture_parses() {
        let agent: Agent = serde_json::from_value(result("agent-get.json", "agent")).unwrap();
        assert_eq!(agent.pane_id, "wC:p1");
        assert_eq!(agent.workspace_id, "wC");
        assert_eq!(agent.tab_id, "wC:t1");
        assert_eq!(
            agent.terminal_title.as_deref(),
            Some("\u{25d0} Agent task 6")
        );
        assert_eq!(agent.effective_cwd(), Some("/home/user/herdr-sleep"));
    }

    #[test]
    fn pane_list_fixture_parses() {
        let panes: Vec<Pane> = serde_json::from_value(result("pane-list.json", "panes")).unwrap();
        assert_eq!(panes.len(), 11);
        assert_eq!(panes.iter().filter(|p| p.has_agent()).count(), 6);
        assert!(panes.iter().all(|p| p.scroll.is_some()));
        let current = panes.iter().find(|p| p.pane_id == "wC:p1").unwrap();
        assert_eq!(current.scroll.unwrap().max_offset_from_bottom, 858);
    }

    #[test]
    fn pane_get_fixture_parses() {
        let pane: Pane = serde_json::from_value(result("pane-get-current.json", "pane")).unwrap();
        assert_eq!(pane.pane_id, "wC:p1");
        assert_eq!(pane.agent_status, AgentStatus::Working);
        assert_eq!(pane.session_id(), Some(FAKE_UUID));
        assert_eq!(
            pane.state_change_seq, 0,
            "pane get carries no seq: default applies"
        );
    }

    #[test]
    fn pane_get_missing_is_an_api_error() {
        let value = fixture("pane-get-missing.json");
        let error: ApiError = serde_json::from_value(value["error"].clone()).unwrap();
        assert_eq!(error.code, "pane_not_found");
        assert_eq!(error.message, "pane nope:p9 not found");
        assert_eq!(error.to_string(), "pane_not_found: pane nope:p9 not found");
    }

    #[test]
    fn tab_list_fixture_parses() {
        let tabs: Vec<Tab> = serde_json::from_value(result("tab-list.json", "tabs")).unwrap();
        assert_eq!(tabs.len(), 6);
        let focused: Vec<&Tab> = tabs.iter().filter(|t| t.focused).collect();
        assert_eq!(focused.len(), 1);
        assert_eq!(focused[0].tab_id, "wB:t1");
        assert_eq!(focused[0].agent_status, AgentStatus::Done);
        assert_eq!(tabs[2].pane_count, 4);
        assert_eq!(tabs[2].label, "1");
        assert_eq!(tabs[2].number, 1);
    }

    #[test]
    fn workspace_list_fixture_parses() {
        let workspaces: Vec<Workspace> =
            serde_json::from_value(result("workspace-list.json", "workspaces")).unwrap();
        assert_eq!(workspaces.len(), 6);
        let last = &workspaces[5];
        assert_eq!(last.workspace_id, "wC");
        assert_eq!(last.label, "herdr-sleep");
        assert_eq!(last.number, 6);
        assert_eq!(last.active_tab_id.as_deref(), Some("wC:t1"));
        assert_eq!(last.pane_count, 2);
        assert_eq!(last.tab_count, 1);
        assert_eq!(last.agent_status, AgentStatus::Working);
    }

    #[test]
    fn process_info_fixture_parses() {
        let info: ProcessInfo =
            serde_json::from_value(result("process-info-current.json", "process_info")).unwrap();
        assert_eq!(info.pane_id, "wC:p1");
        assert_eq!(info.shell_pid, 184186);
        assert_eq!(info.foreground_process_group_id, 184214);
        assert_eq!(info.foreground_processes.len(), 1);
        assert_eq!(info.foreground_processes[0].pid, 184214);
        assert_eq!(info.foreground_processes[0].name, "claude");
        assert_eq!(
            info.foreground_processes[0].cwd.as_deref(),
            Some("/home/user/herdr-sleep")
        );
        assert!(!info.at_shell());

        let idle = ProcessInfo {
            foreground_process_group_id: info.shell_pid,
            foreground_processes: Vec::new(),
            ..info
        };
        assert!(idle.at_shell());
    }

    #[test]
    fn sleeping_label_helpers() {
        let agent = Agent {
            agent: Some("sleeping-codex".into()),
            agent_status: AgentStatus::Idle,
            ..Agent::default()
        };
        assert!(agent.is_sleeping());
        assert_eq!(agent.kind(), Some("codex"));
        assert_eq!(agent.effective_cwd(), None);
        assert!(agent.is_settled(&[AgentStatus::Idle]));
        assert!(!agent.is_settled(&[AgentStatus::Done]));
    }

    #[test]
    fn missing_optional_fields_take_defaults() {
        let json = r#"{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1"}"#;
        let pane: Pane = serde_json::from_str(json).unwrap();
        assert_eq!(pane.agent_status, AgentStatus::Unknown);
        assert!(!pane.focused);
        assert!(pane.scroll.is_none());
        let agent: Agent = serde_json::from_str(json).unwrap();
        assert_eq!(agent.state_change_seq, 0);
        let with_extra = r#"{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","brand_new_key":1,"tokens":{"sleep":"zz"}}"#;
        let agent: Agent = serde_json::from_str(with_extra).unwrap();
        assert_eq!(agent.tokens.unwrap()["sleep"], "zz");
    }
}

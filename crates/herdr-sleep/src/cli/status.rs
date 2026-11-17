//! The `status` command: every managed agent, one line each.
//!
//! [`build_report`] gathers the data from a snapshot, the idle clocks, the
//! process table and the watcher state without touching anything outside its
//! arguments; [`render`] turns the report into text.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

use herdr_sleep::config::Config;
use herdr_sleep::herdr::{AgentStatus, HerdrClient, ProcessInfo, Snapshot};
use herdr_sleep::ops::RestoreReport;
use herdr_sleep::ops::{Env, LiveEnv, SweepState, is_focused, signature};
use herdr_sleep::procs::{self, ProcessTable};
use herdr_sleep::watcher::{self, WatcherInfo};
use serde::Serialize;

use super::{emit, fail};

/// Longest name shown in the text output; `--json` keeps it whole.
const NAME_WIDTH: usize = 24;

/// One managed agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusRow {
    pub pane: String,
    pub name: Option<String>,
    pub kind: String,
    pub status: String,
    pub sleeping: bool,
    /// Seconds the awake agent has been settled and unchanged, from the idle
    /// clock; `None` when unknown.
    pub idle_seconds: Option<u64>,
    /// Memory of the awake agent's process tree; `None` when unreadable.
    pub rss_kb: Option<u64>,
    /// Code of the last refusal remembered for the awake agent.
    pub refusal: Option<String>,
}

/// What `status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub agents: Vec<StatusRow>,
    pub watcher: WatcherInfo,
    pub state_dir: Option<String>,
}

/// Everything [`build_report`] reads.
pub struct StatusInput<'a> {
    pub snapshot: &'a Snapshot,
    pub config: &'a Config,
    pub state: &'a SweepState,
    /// Wall-clock milliseconds since the Unix epoch.
    pub now_ms: u64,
    /// Process information of the awake agents' panes, by pane id.
    pub infos: &'a HashMap<String, ProcessInfo>,
    pub table: &'a ProcessTable,
    pub watcher: WatcherInfo,
    pub state_dir: Option<&'a Path>,
}

/// Build the report: one row per managed agent, in snapshot order.
pub fn build_report(input: &StatusInput) -> StatusReport {
    let agents = input
        .snapshot
        .agents
        .iter()
        .filter_map(|agent| {
            let kind = agent.kind().filter(|kind| input.config.is_managed(kind))?;
            let sleeping = agent.is_sleeping();
            let clock = input.state.agents.get(&agent.pane_id);
            let (idle_seconds, rss_kb, refusal) = if sleeping {
                (None, None, None)
            } else {
                let focused = is_focused(input.snapshot, agent);
                let idle = clock
                    .filter(|clock| clock.signature == signature(agent, focused))
                    .filter(|_| agent.is_settled(&AgentStatus::SETTLED))
                    .map(|clock| input.now_ms.saturating_sub(clock.since_ms) / 1000);
                let rss = input
                    .config
                    .profile(kind)
                    .and_then(|profile| rss_of(input, &agent.pane_id, &profile.bin));
                (idle, rss, clock.and_then(|clock| clock.skip.clone()))
            };
            Some(StatusRow {
                pane: agent.pane_id.clone(),
                name: agent.name.clone().filter(|name| !name.is_empty()),
                kind: kind.to_string(),
                status: agent.agent_status.to_string(),
                sleeping,
                idle_seconds,
                rss_kb,
                refusal,
            })
        })
        .collect();
    StatusReport {
        agents,
        watcher: input.watcher.clone(),
        state_dir: input.state_dir.map(|dir| dir.display().to_string()),
    }
}

/// Memory of the process tree of the agent binary `bin` running in `pane`.
fn rss_of(input: &StatusInput, pane: &str, bin: &str) -> Option<u64> {
    let info = input.infos.get(pane)?;
    let pid = procs::find_agent_process(info, input.table, bin)?;
    input.table.get(pid)?;
    Some(input.table.rss_of_tree(pid))
}

/// `0s`, `59s`, `1m`, `59m`, `1h0m`, `25h3m`.
pub fn format_idle(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
    }
}

fn shorten(name: &str) -> String {
    if name.chars().count() <= NAME_WIDTH {
        return name.to_string();
    }
    let kept: String = name.chars().take(NAME_WIDTH - 1).collect();
    format!("{kept}…")
}

/// The text output: a header, one line per agent, then the footer.
///
/// A sleeping agent starts with `sleep_icon`, an awake one with two spaces.
pub fn render(report: &StatusReport, sleep_icon: &str) -> String {
    let mut out = String::new();
    if report.agents.is_empty() {
        out.push_str("no managed agents\n");
    } else {
        let mut rows: Vec<[String; 7]> = vec![
            [
                "PANE", "NAME", "KIND", "STATUS", "IDLE", "RSS MB", "REFUSAL",
            ]
            .map(String::from),
        ];
        for agent in &report.agents {
            rows.push([
                agent.pane.clone(),
                agent.name.as_deref().map_or("-".into(), shorten),
                agent.kind.clone(),
                agent.status.clone(),
                agent.idle_seconds.map_or("-".into(), format_idle),
                agent
                    .rss_kb
                    .map_or("-".into(), |kb| ((kb + 512) / 1024).to_string()),
                agent.refusal.clone().unwrap_or_else(|| "-".into()),
            ]);
        }
        let mut widths = [0usize; 6];
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        for (index, row) in rows.iter().enumerate() {
            let marker = match index {
                0 => "  ",
                _ if report.agents[index - 1].sleeping => sleep_icon,
                _ => "  ",
            };
            let mut line = marker.to_string();
            for (column, (width, cell)) in widths.iter().zip(row).enumerate() {
                line.push_str(if column == 0 { " " } else { "  " });
                line.push_str(cell);
                line.push_str(&" ".repeat(width - cell.chars().count()));
            }
            line.push_str("  ");
            line.push_str(&row[6]);
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out.push('\n');
    match (report.watcher.running, report.watcher.pid) {
        (true, Some(pid)) => out.push_str(&format!("watcher: running (pid {pid})\n")),
        (true, None) => out.push_str("watcher: running\n"),
        (false, _) => out.push_str("watcher: not running\n"),
    }
    out.push_str(&format!(
        "state: {}\n",
        report.state_dir.as_deref().unwrap_or("-")
    ));
    out
}

/// Process information of the panes of the awake managed agents.
fn collect_infos(
    env: &dyn Env,
    config: &Config,
    snapshot: &Snapshot,
) -> HashMap<String, ProcessInfo> {
    snapshot
        .agents
        .iter()
        .filter(|agent| !agent.is_sleeping())
        .filter(|agent| agent.kind().is_some_and(|kind| config.is_managed(kind)))
        .filter_map(|agent| {
            let info = env.process_info(&agent.pane_id).ok()?;
            Some((agent.pane_id.clone(), info))
        })
        .collect()
}

/// Run `herdr-sleep status [--json]`.
pub fn run(json: bool) -> ExitCode {
    watcher::ensure_watcher_logged();
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let env = LiveEnv::new(HerdrClient::from_env());
    let state_dir = env.state_dir();
    let watcher = match &state_dir {
        Some(dir) => match watcher::watcher_info(dir) {
            Ok(info) => info,
            Err(error) => {
                return fail(format!(
                    "cannot use the state directory {}: {error}",
                    dir.display()
                ));
            }
        },
        None => WatcherInfo {
            running: false,
            pid: None,
        },
    };
    let snapshot = match env.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return fail(format!("cannot read the session: {error}")),
    };
    let state = state_dir
        .as_deref()
        .map(SweepState::load)
        .unwrap_or_default();
    let table = ProcessTable::load().unwrap_or_default();
    let infos = collect_infos(&env, &config, &snapshot);
    let report = build_report(&StatusInput {
        snapshot: &snapshot,
        config: &config,
        state: &state,
        now_ms: env.now_millis(),
        infos: &infos,
        table: &table,
        watcher,
        state_dir: state_dir.as_deref(),
    });
    // Panes the last restore could not put to sleep, when there are any.
    let restore = state_dir
        .as_deref()
        .and_then(RestoreReport::load)
        .filter(|r| !r.skipped.is_empty() || !r.missing_cwd.is_empty());
    if json {
        let mut value = match serde_json::to_value(&report) {
            Ok(value) => value,
            Err(error) => return fail(format!("cannot print the report: {error}")),
        };
        if let (Some(restore), Some(object)) = (&restore, value.as_object_mut()) {
            object.insert(
                "restore".into(),
                serde_json::json!({
                    "skipped": restore.skipped,
                    "missing_cwd": restore.missing_cwd,
                }),
            );
        }
        emit(&value.to_string());
    } else {
        // `render` ends every line with a newline.
        emit(render(&report, &config.sleep_icon).trim_end_matches('\n'));
        for line in restore.iter().flat_map(restore_lines) {
            emit(&line);
        }
    }
    ExitCode::SUCCESS
}

/// The lines `status` adds about the last restore: panes left alone, and
/// panes whose working directory is gone.
pub fn restore_lines(restore: &RestoreReport) -> Vec<String> {
    let mut lines = Vec::new();
    lines.extend(
        restore
            .skipped
            .iter()
            .map(|(pane, reason)| format!("restore: {pane} left alone: {reason}")),
    );
    lines.extend(
        restore
            .missing_cwd
            .iter()
            .map(|(pane, cwd)| format!("restore: {pane} ignored, working directory gone: {cwd}")),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_sleep::herdr::{Agent, ForegroundProcess};
    use herdr_sleep::ops::AgentClock;
    use herdr_sleep::procs::ProcessEntry;

    const NOW: u64 = 10_000_000;

    fn agent(pane: &str, name: Option<&str>, label: &str, status: AgentStatus, seq: u64) -> Agent {
        Agent {
            pane_id: pane.into(),
            name: name.map(str::to_string),
            agent: Some(label.into()),
            agent_status: status,
            state_change_seq: seq,
            ..Agent::default()
        }
    }

    fn clock(agent: &Agent, ago_s: u64, skip: Option<&str>) -> AgentClock {
        AgentClock {
            signature: signature(agent, false),
            since_ms: NOW - ago_s * 1000,
            skip: skip.map(str::to_string),
        }
    }

    fn table() -> ProcessTable {
        ProcessTable::from_entries(vec![
            ProcessEntry {
                pid: 100,
                ppid: 1,
                rss_kb: 300 * 1024,
                command: "claude --flag".into(),
            },
            ProcessEntry {
                pid: 101,
                ppid: 100,
                rss_kb: 100 * 1024,
                command: "node helper".into(),
            },
        ])
    }

    fn info(pane: &str, pid: i32) -> ProcessInfo {
        ProcessInfo {
            pane_id: pane.into(),
            shell_pid: 1,
            foreground_process_group_id: pid,
            foreground_processes: vec![ForegroundProcess {
                pid,
                name: "claude".into(),
                cwd: None,
            }],
        }
    }

    fn running() -> WatcherInfo {
        WatcherInfo {
            running: true,
            pid: Some(4242),
        }
    }

    struct Fixture {
        snapshot: Snapshot,
        config: Config,
        state: SweepState,
        infos: HashMap<String, ProcessInfo>,
        table: ProcessTable,
    }

    impl Fixture {
        fn report(&self, watcher: WatcherInfo) -> StatusReport {
            build_report(&StatusInput {
                snapshot: &self.snapshot,
                config: &self.config,
                state: &self.state,
                now_ms: NOW,
                infos: &self.infos,
                table: &self.table,
                watcher,
                state_dir: Some(Path::new("/state/herdr-sleep")),
            })
        }
    }

    fn fixture() -> Fixture {
        let worker = agent("w1:p1", Some("worker"), "claude", AgentStatus::Idle, 3);
        let napping = agent(
            "w1:p2",
            Some("napper"),
            "sleeping-claude",
            AgentStatus::Idle,
            1,
        );
        let unnamed = agent("w2:p1", None, "claude", AgentStatus::Done, 9);
        let busy = agent("w2:p2", Some("busy"), "claude", AgentStatus::Working, 4);
        let other = agent(
            "w3:p1",
            Some("other"),
            "unmanaged-tool",
            AgentStatus::Idle,
            1,
        );
        let mut state = SweepState::default();
        state.agents.insert(
            "w1:p1".into(),
            clock(&worker, 3 * 60 + 5, Some("busy-children")),
        );
        // The clock of `busy` belongs to an earlier state: unknown idle time.
        state.agents.insert(
            "w2:p2".into(),
            AgentClock {
                signature: "idle:1:false".into(),
                since_ms: NOW - 5000,
                skip: None,
            },
        );
        let mut infos = HashMap::new();
        infos.insert("w1:p1".into(), info("w1:p1", 100));
        infos.insert("w2:p1".into(), info("w2:p1", 999));
        Fixture {
            snapshot: Snapshot {
                agents: vec![worker, napping, unnamed, busy, other],
                ..Snapshot::default()
            },
            config: Config::default(),
            state,
            infos,
            table: table(),
        }
    }

    #[test]
    fn the_last_restore_is_listed() {
        let restore = RestoreReport {
            skipped: vec![("p1".into(), "identity-not-released".into())],
            missing_cwd: vec![("p2".into(), "/gone".into())],
            ..RestoreReport::default()
        };
        assert_eq!(
            restore_lines(&restore),
            [
                "restore: p1 left alone: identity-not-released",
                "restore: p2 ignored, working directory gone: /gone"
            ]
        );
        assert!(restore_lines(&RestoreReport::default()).is_empty());
    }

    #[test]
    fn idle_time_formatting() {
        for (seconds, text) in [
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (59 * 60 + 59, "59m"),
            (3600, "1h0m"),
            (3600 + 12 * 60, "1h12m"),
            (25 * 3600 + 3 * 60 + 59, "25h3m"),
        ] {
            assert_eq!(format_idle(seconds), text, "{seconds}");
        }
    }

    #[test]
    fn the_report_covers_managed_agents_only() {
        let report = fixture().report(running());
        let panes: Vec<_> = report.agents.iter().map(|a| a.pane.as_str()).collect();
        assert_eq!(panes, ["w1:p1", "w1:p2", "w2:p1", "w2:p2"]);
    }

    #[test]
    fn an_awake_agent_has_idle_time_memory_and_refusal() {
        let report = fixture().report(running());
        assert_eq!(
            report.agents[0],
            StatusRow {
                pane: "w1:p1".into(),
                name: Some("worker".into()),
                kind: "claude".into(),
                status: "idle".into(),
                sleeping: false,
                idle_seconds: Some(185),
                rss_kb: Some(400 * 1024),
                refusal: Some("busy-children".into()),
            }
        );
    }

    #[test]
    fn a_sleeping_agent_has_nothing_but_identity() {
        let row = &fixture().report(running()).agents[1];
        assert!(row.sleeping);
        assert_eq!(row.kind, "claude");
        assert_eq!(
            (row.idle_seconds, row.rss_kb, row.refusal.as_deref()),
            (None, None, None)
        );
    }

    #[test]
    fn unknown_idle_time_and_unreadable_memory_are_none() {
        let report = fixture().report(running());
        // No clock, and a pid the process table does not know.
        let unnamed = &report.agents[2];
        assert_eq!(unnamed.name, None);
        assert_eq!((unnamed.idle_seconds, unnamed.rss_kb), (None, None));
        // A stale clock and a working agent.
        let busy = &report.agents[3];
        assert_eq!((busy.idle_seconds, busy.rss_kb), (None, None));
    }

    #[test]
    fn text_output_snapshot() {
        let text = render(&fixture().report(running()), "zz");
        let expected = [
            "   PANE   NAME    KIND    STATUS   IDLE  RSS MB  REFUSAL",
            "   w1:p1  worker  claude  idle     3m    400     busy-children",
            "zz w1:p2  napper  claude  idle     -     -       -",
            "   w2:p1  -       claude  done     -     -       -",
            "   w2:p2  busy    claude  working  -     -       -",
            "",
            "watcher: running (pid 4242)",
            "state: /state/herdr-sleep",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    #[test]
    fn text_output_with_a_long_name_and_no_watcher() {
        let mut fixture = fixture();
        fixture.snapshot.agents.truncate(1);
        fixture.snapshot.agents[0].name = Some("a-very-long-agent-name-indeed-yes".into());
        let report = build_report(&StatusInput {
            snapshot: &fixture.snapshot,
            config: &fixture.config,
            state: &SweepState::default(),
            now_ms: NOW,
            infos: &HashMap::new(),
            table: &fixture.table,
            watcher: WatcherInfo {
                running: false,
                pid: None,
            },
            state_dir: None,
        });
        let expected = [
            "   PANE   NAME                      KIND    STATUS  IDLE  RSS MB  REFUSAL",
            "   w1:p1  a-very-long-agent-name-…  claude  idle    -     -       -",
            "",
            "watcher: not running",
            "state: -",
            "",
        ]
        .join("\n");
        assert_eq!(render(&report, "zz"), expected);
        // The JSON keeps the whole name.
        assert_eq!(
            report.agents[0].name.as_deref(),
            Some("a-very-long-agent-name-indeed-yes")
        );
    }

    #[test]
    fn an_empty_report() {
        let report = StatusReport {
            agents: vec![],
            watcher: running(),
            state_dir: Some("/s".into()),
        };
        assert_eq!(
            render(&report, "zz"),
            "no managed agents\n\nwatcher: running (pid 4242)\nstate: /s\n"
        );
    }

    #[test]
    fn json_shape() {
        let report = fixture().report(running());
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        let keys = |v: &serde_json::Value| {
            let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(keys(&value), ["agents", "state_dir", "watcher"]);
        assert_eq!(
            keys(&value["agents"][0]),
            [
                "idle_seconds",
                "kind",
                "name",
                "pane",
                "refusal",
                "rss_kb",
                "sleeping",
                "status"
            ]
        );
        assert_eq!(value["agents"][0]["idle_seconds"], 185);
        assert_eq!(value["agents"][2]["name"], serde_json::Value::Null);
        assert_eq!(value["watcher"]["pid"], 4242);
        assert_eq!(value["state_dir"], "/state/herdr-sleep");
    }
}

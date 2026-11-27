//! The record of every live Agent: what `restore` needs to bring an Agent
//! back after a Herdr restart even when it never went to sleep (ADR 0012).
//!
//! It lives in `<state_dir>/live-agents.json`, one entry per pane, in the shape
//! of a registry entry. The Sweep refreshes it on every pass from what Herdr
//! reports (kind, session, name, working directory of each managed Agent).
//! A record is dropped when its pane is gone, when the pane holds a sleeping
//! Agent (the registry takes over), when the Agent was seen quitting, or when
//! nobody saw the Agent for [`MAX_AGE`]. A pane that shows no Agent simply
//! because Herdr just restarted keeps its record. A quit is told from a
//! restart by what this very Watcher saw alive and then gone, and by the pane
//! shell: older than the last sighting means it was there all along.
//!
//! Writes are atomic and reads tolerant, like the registry: a missing or
//! garbled file is an empty record, and the worst consequence is a pane that
//! stays a plain shell after a restart.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::herdr::Snapshot;

use super::registry::Sleeper;

/// Name of the file inside the state directory.
pub const FILE_NAME: &str = "live-agents.json";

/// A record nobody refreshed for this long is forgotten: the Agent is gone
/// for good, and a restart a week later must not bring it back.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Every live Agent last seen, keyed by pane id. `since_ms` of each entry is
/// the last time the Agent was seen alive.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LiveAgents {
    #[serde(default)]
    pub panes: BTreeMap<String, Sleeper>,
}

impl LiveAgents {
    /// The file holding the record inside `state_dir`.
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(FILE_NAME)
    }

    /// Read the record. A missing, unreadable or garbled file gives an empty one.
    pub fn load(state_dir: &Path) -> Self {
        fs::read_to_string(Self::path(state_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write the record atomically.
    pub fn save(&self, state_dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(state_dir)?;
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        super::atomic::write(&Self::path(state_dir), &text)
    }

    /// Bring the record in line with `snapshot`. `alive` holds the panes this
    /// process saw with an Agent so far; it is what tells an Agent that quit
    /// from one that Herdr has not brought back yet; `shell_started` gives
    /// when the shell of a pane started: a shell older than the last sighting
    /// of the Agent was there all along, so the Agent quit (a restart makes a
    /// new shell). Returns whether anything changed.
    pub fn refresh(
        &mut self,
        snapshot: &Snapshot,
        config: &Config,
        alive: &mut HashSet<String>,
        now_ms: u64,
        shell_started: &dyn Fn(&str) -> Option<u64>,
    ) -> bool {
        // Herdr is starting: nothing it reports can be trusted yet.
        if snapshot.panes.is_empty() {
            return false;
        }
        let before = self.clone();
        let mut seen: HashSet<&str> = HashSet::new();
        for agent in &snapshot.agents {
            if agent.is_sleeping() {
                continue;
            }
            let pane = agent.pane_id.as_str();
            let managed = agent.kind().is_some_and(|kind| config.is_managed(kind));
            let (Some(kind), Some(session), Some(cwd)) =
                (agent.kind(), agent.session_id(), agent.effective_cwd())
            else {
                // Seen, but not restorable: keep what an earlier look found.
                if managed {
                    seen.insert(pane);
                }
                continue;
            };
            if !managed {
                continue;
            }
            seen.insert(pane);
            alive.insert(agent.pane_id.clone());
            // Herdr clears the name of an agent for a moment now and then: a
            // missing name never replaces the one already recorded.
            let name = agent
                .name
                .clone()
                .or_else(|| self.panes.get(&agent.pane_id).and_then(|r| r.name.clone()));
            self.panes.insert(
                agent.pane_id.clone(),
                Sleeper {
                    kind: kind.to_string(),
                    session: session.to_string(),
                    name,
                    cwd: cwd.to_string(),
                    args: Vec::new(),
                    since_ms: now_ms,
                },
            );
        }
        let max_age_ms = u64::try_from(MAX_AGE.as_millis()).unwrap_or(u64::MAX);
        self.panes.retain(|pane, record| {
            let Some(found) = snapshot.pane(pane) else {
                return false;
            };
            if found.is_sleeping() {
                return false;
            }
            if seen.contains(pane.as_str()) || found.has_agent() {
                return true;
            }
            // No Agent in the pane: it quit in front of us, or Herdr has not
            // brought it back yet.
            if alive.remove(pane) {
                return false;
            }
            if shell_started(pane).is_some_and(|started| started < record.since_ms) {
                return false;
            }
            now_ms.saturating_sub(record.since_ms) < max_age_ms
        });
        *self != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{Agent, AgentSession, AgentStatus, Pane};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-sleep-live-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn agent(pane: &str, label: &str, session: Option<&str>) -> Agent {
        Agent {
            pane_id: pane.into(),
            name: Some("worker".into()),
            agent: Some(label.into()),
            agent_status: AgentStatus::Idle,
            cwd: Some("/work".into()),
            agent_session: session.map(|value| AgentSession {
                source: "herdr:claude".into(),
                agent: None,
                kind: Some("id".into()),
                value: value.into(),
            }),
            ..Agent::default()
        }
    }

    fn snapshot(panes: &[&str], agents: Vec<Agent>) -> Snapshot {
        Snapshot {
            panes: panes
                .iter()
                .map(|pane| Pane {
                    pane_id: (*pane).into(),
                    agent: agents
                        .iter()
                        .find(|a| a.pane_id == *pane)
                        .and_then(|a| a.agent.clone()),
                    ..Pane::default()
                })
                .collect(),
            agents,
            ..Snapshot::default()
        }
    }

    fn refresh(live: &mut LiveAgents, snap: &Snapshot, alive: &mut HashSet<String>) -> bool {
        live.refresh(snap, &Config::default(), alive, 1_000, &|_| None)
    }

    #[test]
    fn a_live_agent_is_recorded_with_what_a_restore_needs() {
        let mut live = LiveAgents::default();
        let mut alive = HashSet::new();
        let snap = snapshot(&["w1:p1"], vec![agent("w1:p1", "claude", Some("sess-1"))]);
        assert!(refresh(&mut live, &snap, &mut alive));
        assert_eq!(
            live.panes.get("w1:p1"),
            Some(&Sleeper {
                kind: "claude".into(),
                session: "sess-1".into(),
                name: Some("worker".into()),
                cwd: "/work".into(),
                args: Vec::new(),
                since_ms: 1_000,
            })
        );
        assert!(!refresh(&mut live, &snap, &mut alive), "nothing changed");
    }

    #[test]
    fn a_name_herdr_stops_reporting_for_a_moment_is_kept() {
        let mut live = LiveAgents::default();
        let mut alive = HashSet::new();
        let named = snapshot(&["w1:p1"], vec![agent("w1:p1", "claude", Some("s"))]);
        refresh(&mut live, &named, &mut alive);
        let mut unnamed_agent = agent("w1:p1", "claude", Some("s"));
        unnamed_agent.name = None;
        let unnamed = snapshot(&["w1:p1"], vec![unnamed_agent]);
        refresh(&mut live, &unnamed, &mut alive);
        assert_eq!(live.panes["w1:p1"].name.as_deref(), Some("worker"));
    }

    #[test]
    fn an_agent_without_a_session_or_not_managed_is_not_recorded() {
        let mut live = LiveAgents::default();
        let mut alive = HashSet::new();
        let snap = snapshot(
            &["w1:p1", "w1:p2"],
            vec![
                agent("w1:p1", "cursor", None),
                agent("w1:p2", "someother", Some("s")),
            ],
        );
        refresh(&mut live, &snap, &mut alive);
        assert!(live.panes.is_empty());
    }

    #[test]
    fn a_restart_keeps_the_record_until_the_agent_is_seen_gone() {
        let mut live = LiveAgents::default();
        let mut alive = HashSet::new();
        let running = snapshot(&["w1:p1"], vec![agent("w1:p1", "claude", Some("s"))]);
        refresh(&mut live, &running, &mut alive);

        // Herdr restarted: a new Watcher has seen nothing yet, the pane is a shell.
        let shell = snapshot(&["w1:p1"], vec![]);
        let mut fresh = HashSet::new();
        assert!(!refresh(&mut live, &shell, &mut fresh));
        assert!(live.panes.contains_key("w1:p1"));

        // The same Watcher saw it alive, then gone: the user quit it.
        assert!(refresh(&mut live, &shell, &mut alive));
        assert!(live.panes.is_empty());
    }

    #[test]
    fn a_shell_older_than_the_last_sighting_means_the_agent_quit() {
        let mut live = LiveAgents::default();
        let mut alive = HashSet::new();
        let running = snapshot(&["w1:p1"], vec![agent("w1:p1", "claude", Some("s"))]);
        refresh(&mut live, &running, &mut alive);
        let shell = snapshot(&["w1:p1"], vec![]);
        let mut fresh = HashSet::new();
        // Started after the last sighting (1_000): Herdr restarted, keep it.
        assert!(
            !live.refresh(&shell, &Config::default(), &mut fresh, 2_000, &|_| Some(
                1_500
            ))
        );
        assert_eq!(live.panes.len(), 1);
        // Started before it: the pane was there all along, the agent quit.
        assert!(
            live.refresh(&shell, &Config::default(), &mut fresh, 2_000, &|_| Some(
                500
            ))
        );
        assert!(live.panes.is_empty());
    }

    #[test]
    fn a_gone_pane_or_a_sleeping_agent_drops_the_record() {
        let mut live = LiveAgents::default();
        let mut alive = HashSet::new();
        let running = snapshot(
            &["w1:p1", "w1:p2"],
            vec![
                agent("w1:p1", "claude", Some("a")),
                agent("w1:p2", "claude", Some("b")),
            ],
        );
        refresh(&mut live, &running, &mut alive);
        let later = snapshot(
            &["w1:p2"],
            vec![agent("w1:p2", "sleeping-claude", Some("b"))],
        );
        refresh(&mut live, &later, &mut alive);
        assert!(live.panes.is_empty());
    }

    #[test]
    fn an_empty_snapshot_changes_nothing_and_old_records_expire() {
        let mut live = LiveAgents::default();
        let mut alive = HashSet::new();
        let running = snapshot(&["w1:p1"], vec![agent("w1:p1", "claude", Some("s"))]);
        refresh(&mut live, &running, &mut alive);
        assert!(!refresh(&mut live, &Snapshot::default(), &mut alive));
        assert_eq!(live.panes.len(), 1);

        let shell = snapshot(&["w1:p1"], vec![]);
        let later = 1_000 + u64::try_from(MAX_AGE.as_millis()).unwrap() + 1;
        let mut fresh = HashSet::new();
        assert!(live.refresh(&shell, &Config::default(), &mut fresh, later, &|_| None));
        assert!(live.panes.is_empty());
    }

    #[test]
    fn the_file_round_trips_and_a_garbled_one_is_empty() {
        let dir = temp_dir("file");
        let mut live = LiveAgents::default();
        let mut alive = HashSet::new();
        let running = snapshot(&["w1:p1"], vec![agent("w1:p1", "claude", Some("s"))]);
        refresh(&mut live, &running, &mut alive);
        live.save(&dir).unwrap();
        assert_eq!(LiveAgents::load(&dir), live);
        fs::write(LiveAgents::path(&dir), "{nope").unwrap();
        assert_eq!(LiveAgents::load(&dir), LiveAgents::default());
        fs::remove_dir_all(&dir).unwrap();
    }
}

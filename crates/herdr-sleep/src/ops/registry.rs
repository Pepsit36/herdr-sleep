//! The registry of sleeping panes: what `restore` needs to bring the
//! Placeholders back after a Herdr restart (ADR 0008).
//!
//! It lives in `<state_dir>/sleepers.json`, one entry per pane. Sleep adds an
//! entry once the Sleeping agent is announced, Wake and Replay remove it when
//! the Agent is Ready, and the Sweep prunes the entries of panes that no
//! longer exist. Writes are atomic (temporary file, then rename) and reads are
//! tolerant: a missing or garbled file is an empty registry, because the
//! worst consequence is a pane that stays a plain shell after a restart.
//!
//! Updates are read-modify-write under a lock (`sleepers.lock`), so two
//! processes updating different panes at the same instant lose nothing.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Name of the registry file inside the state directory.
pub const FILE_NAME: &str = "sleepers.json";

/// Name of the lock file (`sleepers.lock`) that serialises the updates.
const LOCK_NAME: &str = "sleepers";

/// What is needed to put the Placeholder of one pane back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sleeper {
    /// The Kind of the Agent that sleeps (`claude`, not `sleeping-claude`).
    pub kind: String,
    /// The Session to resume on Wake.
    pub session: String,
    /// The name of the Agent, restored on Wake.
    #[serde(default)]
    pub name: Option<String>,
    /// Working directory of the pane.
    pub cwd: String,
    /// The flags of the agent's own command line that come back when it wakes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Wall-clock milliseconds since the Unix epoch at which it went to sleep.
    pub since_ms: u64,
}

/// Every sleeping pane, keyed by pane id.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub panes: BTreeMap<String, Sleeper>,
}

impl Registry {
    /// The file holding the registry inside `state_dir`.
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(FILE_NAME)
    }

    /// Read the registry. A missing, unreadable or garbled file gives an
    /// empty registry.
    pub fn load(state_dir: &Path) -> Self {
        fs::read_to_string(Self::path(state_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write the registry atomically.
    pub fn save(&self, state_dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(state_dir)?;
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        super::atomic::write(&Self::path(state_dir), &text)
    }

    /// Record `sleeper` for `pane`, replacing any previous entry.
    pub fn add(state_dir: &Path, pane: &str, sleeper: Sleeper) -> std::io::Result<()> {
        super::atomic::locked(state_dir, LOCK_NAME, || {
            let mut registry = Self::load(state_dir);
            registry.panes.insert(pane.to_string(), sleeper);
            registry.save(state_dir)
        })
    }

    /// Forget `pane`. Nothing is written when it was not registered.
    pub fn remove(state_dir: &Path, pane: &str) -> std::io::Result<()> {
        super::atomic::locked(state_dir, LOCK_NAME, || {
            let mut registry = Self::load(state_dir);
            if registry.panes.remove(pane).is_none() {
                return Ok(());
            }
            registry.save(state_dir)
        })
    }

    /// Forget every pane that is not in `alive`; the panes forgotten.
    pub fn prune(state_dir: &Path, alive: &HashSet<&str>) -> std::io::Result<Vec<String>> {
        super::atomic::locked(state_dir, LOCK_NAME, || {
            Self::prune_locked(state_dir, alive)
        })
    }

    fn prune_locked(state_dir: &Path, alive: &HashSet<&str>) -> std::io::Result<Vec<String>> {
        let mut registry = Self::load(state_dir);
        let gone: Vec<String> = registry
            .panes
            .keys()
            .filter(|pane| !alive.contains(pane.as_str()))
            .cloned()
            .collect();
        if gone.is_empty() {
            return Ok(gone);
        }
        for pane in &gone {
            registry.panes.remove(pane);
        }
        registry.save(state_dir)?;
        Ok(gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-sleep-registry-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sleeper(session: &str) -> Sleeper {
        Sleeper {
            kind: "claude".into(),
            session: session.into(),
            name: Some("worker".into()),
            cwd: "/work".into(),
            args: Vec::new(),
            since_ms: 42,
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_registry() {
        let dir = temp_dir("missing");
        assert_eq!(Registry::load(&dir), Registry::default());
        assert_eq!(Registry::load(&dir.join("absent")), Registry::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_garbled_file_is_an_empty_registry() {
        let dir = temp_dir("garbled");
        fs::write(Registry::path(&dir), "{ not json").unwrap();
        assert_eq!(Registry::load(&dir), Registry::default());
        fs::write(Registry::path(&dir), r#"{"panes": 3}"#).unwrap();
        assert_eq!(Registry::load(&dir), Registry::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let dir = temp_dir("tolerant");
        fs::write(
            Registry::path(&dir),
            r#"{"panes": {"p1": {"kind": "claude", "session": "s", "cwd": "/", "since_ms": 1, "future": true}}, "extra": 1}"#,
        )
        .unwrap();
        let registry = Registry::load(&dir);
        assert_eq!(registry.panes["p1"].name, None);
        assert_eq!(registry.panes["p1"].kind, "claude");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_replaces_and_keeps_the_other_panes() {
        let dir = temp_dir("add");
        Registry::add(&dir, "p1", sleeper("a")).unwrap();
        Registry::add(&dir, "p2", sleeper("b")).unwrap();
        Registry::add(&dir, "p1", sleeper("c")).unwrap();
        let registry = Registry::load(&dir);
        assert_eq!(registry.panes.len(), 2);
        assert_eq!(registry.panes["p1"].session, "c");
        assert_eq!(registry.panes["p2"].session, "b");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_creates_the_state_directory() {
        let dir = temp_dir("create").join("nested");
        Registry::add(&dir, "p1", sleeper("a")).unwrap();
        assert!(Registry::path(&dir).is_file());
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn remove_forgets_one_pane_and_ignores_an_unknown_one() {
        let dir = temp_dir("remove");
        Registry::add(&dir, "p1", sleeper("a")).unwrap();
        Registry::add(&dir, "p2", sleeper("b")).unwrap();
        Registry::remove(&dir, "p1").unwrap();
        Registry::remove(&dir, "nope").unwrap();
        let registry = Registry::load(&dir);
        assert_eq!(registry.panes.keys().collect::<Vec<_>>(), ["p2"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_on_an_empty_directory_writes_nothing() {
        let dir = temp_dir("remove-empty");
        Registry::remove(&dir, "p1").unwrap();
        assert!(!Registry::path(&dir).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_drops_the_panes_that_are_gone() {
        let dir = temp_dir("prune");
        for pane in ["p1", "p2", "p3"] {
            Registry::add(&dir, pane, sleeper(pane)).unwrap();
        }
        let alive: HashSet<&str> = ["p2", "other"].into();
        let gone = Registry::prune(&dir, &alive).unwrap();
        assert_eq!(gone, ["p1", "p3"]);
        assert_eq!(
            Registry::load(&dir).panes.keys().collect::<Vec<_>>(),
            ["p2"]
        );
        assert!(Registry::prune(&dir, &alive).unwrap().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_leaves_no_temporary_file() {
        let dir = temp_dir("atomic");
        Registry::add(&dir, "p1", sleeper("a")).unwrap();
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [FILE_NAME, "sleepers.lock"]);
        let text = fs::read_to_string(Registry::path(&dir)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["panes"]["p1"]["since_ms"], 42);
        fs::remove_dir_all(&dir).unwrap();
    }
}

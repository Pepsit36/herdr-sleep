//! Sleep markers beyond the pane: the workspace token and the tab prefix.
//!
//! The Placeholder sets the pane token itself. Herdr has pane and workspace
//! tokens but no tab tokens, so this module covers the rest:
//!
//! - a workspace gets the `$<token_name>` token while every Managed agent in
//!   it sleeps (at least one), and loses it otherwise;
//! - with `tab_prefix`, the label of a tab starts with `"<icon> "` while every
//!   Managed agent in it sleeps (at least one).
//!
//! Only the difference between the wanted and the observed state is sent. The
//! snapshot does not expose workspace tokens, so what was sent is remembered
//! in `<state_dir>/markers.json`, together with the tabs the plugin prefixed:
//! a prefix is only ever removed from a tab listed there, so a label that
//! merely starts with the icon is never touched.
//!
//! Nothing here fails the caller: errors end in the journal as `WARN`.
//!
//! Journal events: `MARKERS`, `WARN`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::herdr::{MetadataChange, Snapshot};
use crate::journal::Journal;

use super::Env;

/// The `--source` of every report the plugin makes.
pub const SOURCE: &str = "pepsit36.herdr-sleep";

/// Name of the remembered markers inside the state directory.
pub const STATE_FILE: &str = "markers.json";

/// Last sequence number handed out by this process.
static LAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// A sequence number above every earlier one of this process, based on the
/// wall clock.
fn next_seq(env: &dyn Env) -> u64 {
    let now = env.now_millis();
    let previous = LAST_SEQ
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| {
            Some(now.max(last.saturating_add(1)))
        })
        .unwrap_or(0);
    now.max(previous.saturating_add(1))
}

/// What the plugin remembers of the markers it set.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MarkersState {
    /// Ids of the tabs whose label the plugin prefixed.
    #[serde(default)]
    pub prefixed_tabs: BTreeSet<String>,
    /// Whether the workspace token is set, by workspace id.
    #[serde(default)]
    pub workspace_tokens: BTreeMap<String, bool>,
}

impl MarkersState {
    /// The file holding the state inside `state_dir`.
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(STATE_FILE)
    }

    /// Read the state from `state_dir`. A missing, unreadable or garbled file
    /// gives an empty state.
    pub fn load(state_dir: &Path) -> Self {
        fs::read_to_string(Self::path(state_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write the state to `state_dir` atomically (temporary file, then rename).
    pub fn save(&self, state_dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(state_dir)?;
        let tmp = state_dir.join(format!("{STATE_FILE}.tmp"));
        let text = serde_json::to_string(self).map_err(std::io::Error::other)?;
        fs::write(&tmp, text)?;
        fs::rename(&tmp, Self::path(state_dir))
    }
}

/// What one synchronisation changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkerReport {
    pub workspaces_set: usize,
    pub workspaces_cleared: usize,
    pub tabs_prefixed: usize,
    pub tabs_unprefixed: usize,
}

impl MarkerReport {
    /// Whether anything was sent to Herdr.
    pub fn is_empty(&self) -> bool {
        *self == MarkerReport::default()
    }
}

/// How many Managed agents a workspace or a tab holds, and how many sleep.
#[derive(Debug, Clone, Copy, Default)]
struct Census {
    managed: usize,
    sleeping: usize,
}

impl Census {
    fn add(&mut self, sleeping: bool) {
        self.managed += 1;
        self.sleeping += usize::from(sleeping);
    }

    /// At least one Managed agent, and all of them sleep.
    fn all_asleep(self) -> bool {
        self.managed > 0 && self.sleeping == self.managed
    }
}

/// Count the Managed agents of every workspace and tab of `snapshot`.
fn census(
    snapshot: &Snapshot,
    config: &Config,
) -> (BTreeMap<String, Census>, BTreeMap<String, Census>) {
    let mut workspaces: BTreeMap<String, Census> = BTreeMap::new();
    let mut tabs: BTreeMap<String, Census> = BTreeMap::new();
    for workspace in &snapshot.workspaces {
        workspaces
            .entry(workspace.workspace_id.clone())
            .or_default();
    }
    for tab in &snapshot.tabs {
        tabs.entry(tab.tab_id.clone()).or_default();
    }

    let mut count = |workspace: &str, tab: &str, kind: Option<&str>, sleeping: bool| {
        if !workspace.is_empty() {
            workspaces.entry(workspace.to_string()).or_default();
        }
        if !kind.is_some_and(|kind| config.is_managed(kind)) {
            return;
        }
        if !workspace.is_empty() {
            workspaces
                .entry(workspace.to_string())
                .or_default()
                .add(sleeping);
        }
        if !tab.is_empty() {
            tabs.entry(tab.to_string()).or_default().add(sleeping);
        }
    };
    for agent in &snapshot.agents {
        count(
            &agent.workspace_id,
            &agent.tab_id,
            agent.kind(),
            agent.is_sleeping(),
        );
    }
    // Panes the agent list does not mention.
    for pane in &snapshot.panes {
        if snapshot.agents.iter().any(|a| a.pane_id == pane.pane_id) {
            continue;
        }
        count(
            &pane.workspace_id,
            &pane.tab_id,
            pane.kind(),
            pane.is_sleeping(),
        );
    }
    (workspaces, tabs)
}

/// Bring the workspace tokens and the tab prefixes in line with the sleeping
/// agents, sending only what differs from what was observed or remembered.
///
/// Never fails: errors are journaled as `WARN`.
pub fn sync_markers(env: &dyn Env, config: &Config) -> MarkerReport {
    sync(env, config, false)
}

/// [`sync_markers`] for when Herdr just started and lost every token: the
/// remembered workspace tokens are ignored, so the wanted ones are sent again.
pub fn resync_markers(env: &dyn Env, config: &Config) -> MarkerReport {
    sync(env, config, true)
}

fn sync(env: &dyn Env, config: &Config, forget_tokens: bool) -> MarkerReport {
    let mut report = MarkerReport::default();
    let Some(dir) = env.state_dir() else {
        return report;
    };
    // Best effort: the journal never decides the outcome.
    let _ = fs::create_dir_all(&dir);
    let journal = Journal::new(&dir);
    let warn = |message: String| journal.log("WARN", &format!("markers: {message}"));

    let snapshot = match env.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            warn(format!("cannot read the session: {error}"));
            return report;
        }
    };
    let (workspaces, tabs) = census(&snapshot, config);
    let before = MarkersState::load(&dir);
    let mut state = before.clone();
    if forget_tokens {
        state.workspace_tokens.clear();
    }

    // Workspace tokens.
    state
        .workspace_tokens
        .retain(|id, _| workspaces.contains_key(id));
    for (id, count) in &workspaces {
        let wanted = count.all_asleep();
        let observed = state.workspace_tokens.get(id).copied().unwrap_or(false);
        if wanted == observed {
            continue;
        }
        let change = if wanted {
            MetadataChange::token(&config.token_name, &config.sleep_icon)
        } else {
            MetadataChange::clear_token(&config.token_name)
        };
        match env.workspace_report_metadata(id, SOURCE, &change, next_seq(env)) {
            Ok(()) => {
                if wanted {
                    state.workspace_tokens.insert(id.clone(), true);
                    report.workspaces_set += 1;
                } else {
                    state.workspace_tokens.remove(id);
                    report.workspaces_cleared += 1;
                }
            }
            Err(error) => warn(format!("workspace {id}: cannot update the token: {error}")),
        }
    }

    // Tab prefixes.
    let prefix = format!("{} ", config.sleep_icon);
    state.prefixed_tabs.retain(|id| tabs.contains_key(id));
    for tab in &snapshot.tabs {
        let ours = state.prefixed_tabs.contains(&tab.tab_id);
        let prefixed = tab.label.starts_with(&prefix);
        let wanted = config.tab_prefix
            && !config.sleep_icon.is_empty()
            && tabs.get(&tab.tab_id).is_some_and(|c| c.all_asleep());
        match (wanted, ours) {
            (true, false) if !prefixed => {
                let label = format!("{prefix}{}", tab.label);
                match env.tab_rename(&tab.tab_id, &label) {
                    Ok(()) => {
                        state.prefixed_tabs.insert(tab.tab_id.clone());
                        report.tabs_prefixed += 1;
                    }
                    Err(error) => warn(format!(
                        "tab {}: cannot add the prefix: {error}",
                        tab.tab_id
                    )),
                }
            }
            (false, true) if prefixed => {
                let label = tab.label[prefix.len()..].to_string();
                match env.tab_rename(&tab.tab_id, &label) {
                    Ok(()) => {
                        state.prefixed_tabs.remove(&tab.tab_id);
                        report.tabs_unprefixed += 1;
                    }
                    Err(error) => {
                        warn(format!(
                            "tab {}: cannot remove the prefix: {error}",
                            tab.tab_id
                        ));
                    }
                }
            }
            // Renamed by the user since: the label is theirs, forget it.
            (_, true) if !prefixed => {
                state.prefixed_tabs.remove(&tab.tab_id);
            }
            // Already in the wanted state, or a label the plugin did not
            // prefix: leave it alone.
            _ => {}
        }
    }

    if state != before
        && let Err(error) = state.save(&dir)
    {
        warn(format!("cannot save {STATE_FILE}: {error}"));
    }
    if !report.is_empty() {
        journal.log(
            "MARKERS",
            &format!(
                "workspaces +{} -{} tabs +{} -{}",
                report.workspaces_set,
                report.workspaces_cleared,
                report.tabs_prefixed,
                report.tabs_unprefixed
            ),
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::{Refusal, SleepMode};
    use crate::herdr::{Agent, Pane, ProcessInfo, Tab, Workspace};
    use crate::ops::EnvResult;
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    struct Fake {
        dir: PathBuf,
        base: Instant,
        agents: RefCell<Vec<Agent>>,
        tabs: RefCell<Vec<Tab>>,
        workspaces: RefCell<Vec<Workspace>>,
        /// `(workspace, change, seq)` of every token report.
        reports: RefCell<Vec<(String, MetadataChange, u64)>>,
        /// `(tab, label)` of every rename. Renames apply to the fake tabs.
        renames: RefCell<Vec<(String, String)>>,
        fail_writes: Cell<bool>,
        snapshot_error: Cell<bool>,
    }

    impl Fake {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-sleep-markers-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Fake {
                dir,
                base: Instant::now(),
                agents: RefCell::new(Vec::new()),
                tabs: RefCell::new(Vec::new()),
                workspaces: RefCell::new(Vec::new()),
                reports: RefCell::new(Vec::new()),
                renames: RefCell::new(Vec::new()),
                fail_writes: Cell::new(false),
                snapshot_error: Cell::new(false),
            }
        }

        /// A workspace `ws` with a tab `<ws>:t1` labelled `label`.
        fn tab(&self, ws: &str, tab: &str, label: &str) {
            if !self
                .workspaces
                .borrow()
                .iter()
                .any(|w| w.workspace_id == ws)
            {
                self.workspaces.borrow_mut().push(Workspace {
                    workspace_id: ws.into(),
                    ..Workspace::default()
                });
            }
            self.tabs.borrow_mut().push(Tab {
                tab_id: tab.into(),
                workspace_id: ws.into(),
                label: label.into(),
                ..Tab::default()
            });
        }

        fn agent(&self, pane: &str, ws: &str, tab: &str, label: &str) {
            self.agents.borrow_mut().push(Agent {
                pane_id: pane.into(),
                workspace_id: ws.into(),
                tab_id: tab.into(),
                agent: Some(label.into()),
                ..Agent::default()
            });
        }

        fn set_label(&self, pane: &str, label: &str) {
            for agent in self.agents.borrow_mut().iter_mut() {
                if agent.pane_id == pane {
                    agent.agent = Some(label.into());
                }
            }
        }

        fn label_of(&self, tab: &str) -> String {
            self.tabs
                .borrow()
                .iter()
                .find(|t| t.tab_id == tab)
                .unwrap()
                .label
                .clone()
        }

        fn relabel(&self, tab: &str, label: &str) {
            for t in self.tabs.borrow_mut().iter_mut() {
                if t.tab_id == tab {
                    t.label = label.into();
                }
            }
        }

        fn journal(&self) -> String {
            fs::read_to_string(self.dir.join(crate::journal::FILE_NAME)).unwrap_or_default()
        }

        fn reported(&self) -> Vec<(String, MetadataChange)> {
            self.reports
                .borrow()
                .iter()
                .map(|(w, c, _)| (w.clone(), c.clone()))
                .collect()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    impl Env for Fake {
        fn snapshot(&self) -> EnvResult<Snapshot> {
            if self.snapshot_error.get() {
                return Err("socket closed".into());
            }
            Ok(Snapshot {
                agents: self.agents.borrow().clone(),
                tabs: self.tabs.borrow().clone(),
                workspaces: self.workspaces.borrow().clone(),
                ..Snapshot::default()
            })
        }
        fn pane(&self, target: &str) -> EnvResult<Pane> {
            Err(format!("no pane {target}"))
        }
        fn agent(&self, target: &str) -> EnvResult<Agent> {
            self.agents
                .borrow()
                .iter()
                .find(|a| a.pane_id == target)
                .cloned()
                .ok_or_else(|| format!("no agent {target}"))
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
        fn tab_rename(&self, tab: &str, label: &str) -> EnvResult<()> {
            if self.fail_writes.get() {
                return Err("denied".into());
            }
            self.renames.borrow_mut().push((tab.into(), label.into()));
            self.relabel(tab, label);
            Ok(())
        }
        fn workspace_report_metadata(
            &self,
            workspace: &str,
            source: &str,
            change: &MetadataChange,
            seq: u64,
        ) -> EnvResult<()> {
            assert_eq!(source, SOURCE);
            if self.fail_writes.get() {
                return Err("denied".into());
            }
            self.reports
                .borrow_mut()
                .push((workspace.into(), change.clone(), seq));
            Ok(())
        }
        fn check_guards(&self, _: &Config, _: &Agent, _: SleepMode) -> Result<(), Refusal> {
            unreachable!()
        }
        fn agent_rss_kb(&self, _: &Agent, _: &str) -> Option<u64> {
            None
        }
        fn shell_path(&self, _: i32) -> Option<PathBuf> {
            None
        }
        fn state_dir(&self) -> Option<PathBuf> {
            Some(self.dir.clone())
        }
        fn placeholder_bin(&self) -> EnvResult<PathBuf> {
            unreachable!()
        }
        fn main_bin(&self) -> PathBuf {
            unreachable!()
        }
        fn herdr_bin(&self) -> PathBuf {
            unreachable!()
        }
        fn sleep(&self, _: Duration) {}
        fn now(&self) -> Instant {
            self.base
        }
        fn now_millis(&self) -> u64 {
            1_000
        }
    }

    fn set() -> MetadataChange {
        MetadataChange::token("sleep", "💤")
    }

    fn clear() -> MetadataChange {
        MetadataChange::clear_token("sleep")
    }

    /// Workspace `w1`, tab `w1:t1` labelled `build`, two Claude panes.
    fn two_agents(name: &str) -> Fake {
        let fake = Fake::new(name);
        fake.tab("w1", "w1:t1", "build");
        fake.agent("w1:p1", "w1", "w1:t1", "sleeping-claude");
        fake.agent("w1:p2", "w1", "w1:t1", "sleeping-claude");
        fake
    }

    #[test]
    fn keeping_a_tab_awake_toggles_the_icon_and_keeps_the_sleep_prefix_first() {
        use super::super::keep_awake::{KeepAwakeOutcome, keep_awake};
        let config = Config::default();
        let fake = two_agents("keep-awake");
        fake.set_label("w1:p2", "claude");
        // An awake tab gets the lock, and asking again with --on changes nothing.
        let outcome = keep_awake(&fake, &config, "w1:p1", None).unwrap();
        assert_eq!(
            outcome,
            KeepAwakeOutcome::Kept {
                label: "\u{1f512} build".into()
            }
        );
        assert_eq!(fake.label_of("w1:t1"), "\u{1f512} build");
        assert!(matches!(
            keep_awake(&fake, &config, "w1:p1", Some(true)).unwrap(),
            KeepAwakeOutcome::Unchanged { kept: true, .. }
        ));
        // Once every agent sleeps the sleep prefix goes in front of the lock,
        // and is removed alone at the next wake.
        fake.set_label("w1:p2", "sleeping-claude");
        sync_markers(&fake, &config);
        assert_eq!(fake.label_of("w1:t1"), "\u{1f4a4} \u{1f512} build");
        fake.set_label("w1:p2", "claude");
        sync_markers(&fake, &config);
        assert_eq!(fake.label_of("w1:t1"), "\u{1f512} build");
        // The same action lifts the protection.
        let outcome = keep_awake(&fake, &config, "w1:p1", None).unwrap();
        assert!(matches!(outcome, KeepAwakeOutcome::Released { .. }));
        assert_eq!(fake.label_of("w1:t1"), "build");
    }

    #[test]
    fn keeping_a_tab_awake_needs_an_icon_and_a_known_pane() {
        use super::super::keep_awake::keep_awake;
        let fake = two_agents("keep-awake-errors");
        let off = Config::from_toml_str("keep_awake_icon = \"\"").unwrap();
        assert!(keep_awake(&fake, &off, "w1:p1", None).is_err());
        assert!(keep_awake(&fake, &Config::default(), "w9:p9", None).is_err());
        assert!(fake.renames.borrow().is_empty());
    }

    #[test]
    fn a_fully_sleeping_workspace_gets_the_token() {
        let fake = two_agents("ws-set");
        let report = sync_markers(&fake, &Config::default());
        assert_eq!(report.workspaces_set, 1);
        assert_eq!(fake.reported(), [("w1".to_string(), set())]);
    }

    #[test]
    fn a_mixed_workspace_is_cleared_once_it_had_the_token() {
        let fake = two_agents("ws-mixed");
        sync_markers(&fake, &Config::default());
        fake.set_label("w1:p2", "claude");
        let report = sync_markers(&fake, &Config::default());
        assert_eq!(report.workspaces_cleared, 1);
        assert_eq!(fake.reported().last(), Some(&("w1".to_string(), clear())));
    }

    #[test]
    fn a_workspace_that_never_had_the_token_sends_nothing() {
        let fake = two_agents("ws-never");
        fake.set_label("w1:p2", "claude");
        assert!(sync_markers(&fake, &Config::default()).is_empty());
        assert!(fake.reported().is_empty());
    }

    #[test]
    fn unmanaged_agents_are_ignored() {
        let fake = two_agents("unmanaged");
        fake.agent("w1:p3", "w1", "w1:t1", "aider");
        fake.agent("w1:p4", "w1", "w1:t1", "sleeping-aider");
        sync_markers(&fake, &Config::default());
        assert_eq!(fake.reported(), [("w1".to_string(), set())]);
    }

    #[test]
    fn a_workspace_without_managed_agents_is_cleared() {
        let fake = two_agents("empty");
        sync_markers(&fake, &Config::default());
        fake.agents.borrow_mut().clear();
        sync_markers(&fake, &Config::default());
        assert_eq!(fake.reported().last(), Some(&("w1".to_string(), clear())));
    }

    #[test]
    fn a_fully_sleeping_tab_gets_the_prefix_and_loses_it_on_wake() {
        let fake = two_agents("tab");
        let report = sync_markers(&fake, &Config::default());
        assert_eq!(report.tabs_prefixed, 1);
        assert_eq!(fake.label_of("w1:t1"), "💤 build");

        fake.set_label("w1:p1", "claude");
        let report = sync_markers(&fake, &Config::default());
        assert_eq!(report.tabs_unprefixed, 1);
        assert_eq!(fake.label_of("w1:t1"), "build");
        assert!(MarkersState::load(&fake.dir).prefixed_tabs.is_empty());
    }

    #[test]
    fn a_second_pass_sends_nothing() {
        let fake = two_agents("idempotent");
        sync_markers(&fake, &Config::default());
        let (reports, renames) = (fake.reports.borrow().len(), fake.renames.borrow().len());
        assert!(sync_markers(&fake, &Config::default()).is_empty());
        assert_eq!(fake.reports.borrow().len(), reports);
        assert_eq!(fake.renames.borrow().len(), renames);
        assert_eq!(fake.label_of("w1:t1"), "💤 build");
    }

    #[test]
    fn a_label_the_user_marked_is_never_touched() {
        let fake = two_agents("user-icon");
        fake.relabel("w1:t1", "💤 mine");
        sync_markers(&fake, &Config::default());
        assert!(fake.renames.borrow().is_empty());

        fake.set_label("w1:p1", "claude");
        sync_markers(&fake, &Config::default());
        assert!(fake.renames.borrow().is_empty());
        assert_eq!(fake.label_of("w1:t1"), "💤 mine");
    }

    #[test]
    fn a_tab_the_user_renamed_is_forgotten() {
        let fake = two_agents("renamed");
        sync_markers(&fake, &Config::default());
        fake.relabel("w1:t1", "docs");
        fake.set_label("w1:p1", "claude");
        let sent = fake.renames.borrow().len();
        let report = sync_markers(&fake, &Config::default());
        assert_eq!((report.tabs_prefixed, report.tabs_unprefixed), (0, 0));
        assert_eq!(fake.renames.borrow().len(), sent);
        assert_eq!(fake.label_of("w1:t1"), "docs");
        assert!(MarkersState::load(&fake.dir).prefixed_tabs.is_empty());
    }

    #[test]
    fn disabling_tab_prefix_removes_only_ours() {
        let fake = two_agents("disable");
        fake.tab("w1", "w1:t2", "💤 theirs");
        fake.agent("w1:p3", "w1", "w1:t2", "sleeping-claude");
        sync_markers(&fake, &Config::default());
        assert_eq!(fake.label_of("w1:t1"), "💤 build");

        let config = Config {
            tab_prefix: false,
            ..Config::default()
        };
        let report = sync_markers(&fake, &config);
        assert_eq!(report.tabs_unprefixed, 1);
        assert_eq!(fake.label_of("w1:t1"), "build");
        assert_eq!(fake.label_of("w1:t2"), "💤 theirs");
        // And it stays off.
        assert!(sync_markers(&fake, &config).is_empty());
    }

    #[test]
    fn sequence_numbers_strictly_increase() {
        let fake = Fake::new("seq");
        for ws in ["w1", "w2", "w3"] {
            fake.tab(ws, &format!("{ws}:t1"), "x");
            fake.agent(
                &format!("{ws}:p1"),
                ws,
                &format!("{ws}:t1"),
                "sleeping-claude",
            );
        }
        sync_markers(&fake, &Config::default());
        let seqs: Vec<u64> = fake.reports.borrow().iter().map(|r| r.2).collect();
        assert_eq!(seqs.len(), 3);
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    }

    #[test]
    fn failures_are_journaled_and_retried() {
        let fake = two_agents("failures");
        fake.fail_writes.set(true);
        let report = sync_markers(&fake, &Config::default());
        assert!(report.is_empty());
        let journal = fake.journal();
        assert!(journal.contains("WARN markers: workspace w1"), "{journal}");
        assert!(journal.contains("WARN markers: tab w1:t1"), "{journal}");
        assert!(MarkersState::load(&fake.dir).prefixed_tabs.is_empty());

        fake.fail_writes.set(false);
        let report = sync_markers(&fake, &Config::default());
        assert_eq!((report.workspaces_set, report.tabs_prefixed), (1, 1));
    }

    #[test]
    fn an_unreadable_session_is_journaled() {
        let fake = two_agents("no-snapshot");
        fake.snapshot_error.set(true);
        assert!(sync_markers(&fake, &Config::default()).is_empty());
        assert!(fake.journal().contains("WARN markers: cannot read"));
    }

    #[test]
    fn a_restart_sends_the_tokens_again() {
        let fake = two_agents("resync");
        sync_markers(&fake, &Config::default());
        assert_eq!(fake.reports.borrow().len(), 1);
        resync_markers(&fake, &Config::default());
        assert_eq!(fake.reports.borrow().len(), 2);
    }

    #[test]
    fn closed_workspaces_and_tabs_are_forgotten() {
        let fake = two_agents("pruned");
        sync_markers(&fake, &Config::default());
        fake.agents.borrow_mut().clear();
        fake.tabs.borrow_mut().clear();
        fake.workspaces.borrow_mut().clear();
        sync_markers(&fake, &Config::default());
        assert_eq!(MarkersState::load(&fake.dir), MarkersState::default());
    }

    #[test]
    fn the_state_file_is_tolerant_and_atomic() {
        let dir =
            std::env::temp_dir().join(format!("herdr-sleep-markers-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(MarkersState::load(&dir), MarkersState::default());

        fs::create_dir_all(&dir).unwrap();
        fs::write(MarkersState::path(&dir), "not json {").unwrap();
        assert_eq!(MarkersState::load(&dir), MarkersState::default());
        fs::write(MarkersState::path(&dir), r#"{"prefixed_tabs":["w1:t1"]}"#).unwrap();
        assert!(MarkersState::load(&dir).prefixed_tabs.contains("w1:t1"));

        let state = MarkersState {
            prefixed_tabs: ["w2:t1".to_string()].into(),
            workspace_tokens: [("w2".to_string(), true)].into(),
        };
        state.save(&dir).unwrap();
        assert_eq!(MarkersState::load(&dir), state);
        assert!(!dir.join(format!("{STATE_FILE}.tmp")).exists());
        let text = fs::read_to_string(MarkersState::path(&dir)).unwrap();
        assert!(text.contains(r#""prefixed_tabs":["w2:t1"]"#), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }
}

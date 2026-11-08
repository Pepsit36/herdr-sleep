//! The focus hooks: `pane.focused`, `tab.focused` and `workspace.focused` all
//! run `herdr-sleep event`.
//!
//! A focus event wakes at most one pane, the one the user is looking at, and
//! never a whole tab or workspace: waking many agents at once spikes memory.
//! Herdr emits the three events together for one focus, so the command first
//! lets them converge, then resolves the pane from a single snapshot. The
//! per-pane de-duplication of [`wake_agent`] absorbs the twin runs.
//!
//! The command never fails loudly: whatever goes wrong ends in the journal.
//!
//! Journal events: `FOCUS-WAKE <pane> <event>`, `WARN`.

use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::herdr::Snapshot;
use crate::journal::Journal;

use super::Env;
use super::markers::sync_markers;
use super::sleep::wait_until;
use super::wake::{WakeOptions, WakeOutcome, wake_agent};

/// Pause before reading the snapshot, so the twin events converge.
pub const SETTLE_DELAY: Duration = Duration::from_millis(400);
/// Poll interval while waiting for Herdr to stop reporting the Placeholder.
pub const AWAKE_POLL: Duration = Duration::from_millis(250);
/// Longest wait for Herdr to stop reporting the Placeholder.
pub const AWAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Which focus changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusKind {
    Pane,
    Tab,
    Workspace,
}

impl FocusKind {
    /// `pane.focused`, `tab.focused` and `workspace.focused`, also spelled
    /// with underscores.
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().replace('_', ".").as_str() {
            "pane.focused" => Some(FocusKind::Pane),
            "tab.focused" => Some(FocusKind::Tab),
            "workspace.focused" => Some(FocusKind::Workspace),
            _ => None,
        }
    }
}

/// What a focus hook was told, from its environment variables.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FocusEvent {
    /// The event name as Herdr gave it, e.g. `pane.focused`.
    pub name: String,
    pub kind: Option<FocusKind>,
    /// Ids carried by the event payload.
    pub pane_id: Option<String>,
    pub tab_id: Option<String>,
    pub workspace_id: Option<String>,
    /// Ids of the context in which the hook runs.
    pub context_pane_id: Option<String>,
    pub context_tab_id: Option<String>,
    pub context_workspace_id: Option<String>,
}

/// The string `key` of the first JSON object that has it, at its top level or
/// under its `data` object. Empty strings count as absent.
fn field(roots: &[Value], key: &str) -> Option<String> {
    roots.iter().find_map(|root| {
        [root.get(key), root.get("data").and_then(|d| d.get(key))]
            .into_iter()
            .flatten()
            .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string))
    })
}

fn parse_object(text: Option<String>) -> Option<Value> {
    serde_json::from_str::<Value>(&text?)
        .ok()
        .filter(Value::is_object)
}

impl FocusEvent {
    /// Read the event from `lookup`, which answers environment variables.
    ///
    /// The payload is parsed defensively: its fields are accepted at the top
    /// level and under a `data` object, and garbage is ignored.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| lookup(key).filter(|v| !v.trim().is_empty());
        let payload: Vec<Value> = parse_object(get("HERDR_PLUGIN_EVENT_JSON"))
            .into_iter()
            .collect();
        let context: Vec<Value> = parse_object(get("HERDR_PLUGIN_CONTEXT_JSON"))
            .into_iter()
            .collect();

        let name = get("HERDR_PLUGIN_EVENT")
            .or_else(|| field(&payload, "event"))
            .or_else(|| field(&payload, "type"))
            .unwrap_or_default();
        FocusEvent {
            kind: FocusKind::parse(&name),
            name,
            pane_id: field(&payload, "pane_id"),
            tab_id: field(&payload, "tab_id"),
            workspace_id: field(&payload, "workspace_id"),
            context_pane_id: field(&context, "focused_pane_id")
                .or_else(|| field(&context, "pane_id"))
                .or_else(|| get("HERDR_PANE_ID")),
            context_tab_id: field(&context, "tab_id").or_else(|| get("HERDR_TAB_ID")),
            context_workspace_id: field(&context, "workspace_id")
                .or_else(|| get("HERDR_WORKSPACE_ID")),
        }
    }

    /// The single pane this event is about, `None` when nothing resolves.
    pub fn target(&self, snapshot: &Snapshot) -> Option<String> {
        if let Some(pane) = &self.pane_id {
            return Some(pane.clone());
        }
        let tab = || self.tab_id.as_deref().or(self.context_tab_id.as_deref());
        let workspace = || {
            self.workspace_id
                .as_deref()
                .or(self.context_workspace_id.as_deref())
        };
        let derived = match self.kind {
            Some(FocusKind::Tab) => tab().and_then(|t| focused_pane_of_tab(snapshot, t)),
            Some(FocusKind::Workspace) => {
                workspace().and_then(|w| focused_pane_of_workspace(snapshot, w))
            }
            Some(FocusKind::Pane) => None,
            None => self
                .tab_id
                .as_deref()
                .and_then(|t| focused_pane_of_tab(snapshot, t))
                .or_else(|| {
                    self.workspace_id
                        .as_deref()
                        .and_then(|w| focused_pane_of_workspace(snapshot, w))
                }),
        };
        derived.or_else(|| self.context_pane_id.clone())
    }
}

/// The pane of `tab` that has the focus inside it. A tab with a single pane
/// has no other candidate.
fn focused_pane_of_tab(snapshot: &Snapshot, tab: &str) -> Option<String> {
    let mut panes = snapshot.panes.iter().filter(|p| p.tab_id == tab);
    if let Some(current) = &snapshot.focused_pane_id
        && snapshot.pane(current).is_some_and(|p| p.tab_id == tab)
    {
        return Some(current.clone());
    }
    let first = panes.next()?;
    let rest: Vec<_> = panes.collect();
    if first.focused {
        return Some(first.pane_id.clone());
    }
    if let Some(pane) = rest.iter().find(|p| p.focused) {
        return Some(pane.pane_id.clone());
    }
    rest.is_empty().then(|| first.pane_id.clone())
}

/// The focused pane of the active tab of `workspace`.
fn focused_pane_of_workspace(snapshot: &Snapshot, workspace: &str) -> Option<String> {
    let active = snapshot
        .workspaces
        .iter()
        .find(|w| w.workspace_id == workspace)
        .and_then(|w| w.active_tab_id.clone())
        .or_else(|| {
            snapshot
                .tabs
                .iter()
                .find(|t| t.workspace_id == workspace && t.focused)
                .map(|t| t.tab_id.clone())
        })?;
    focused_pane_of_tab(snapshot, &active)
}

/// How a focus event ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventOutcome {
    /// Nothing to do: no target, or the target is not a sleeping agent.
    Ignored,
    /// Enter was pressed in the Sleeping pane.
    Woke { pane: String },
    /// Something went wrong; the journal has the details.
    Failed(String),
}

/// Handle one focus event: wake the pane it is about when that pane is a
/// sleeping agent.
pub fn handle_focus_event(env: &dyn Env, config: &Config, event: &FocusEvent) -> EventOutcome {
    let journal = env.state_dir().map(|dir| {
        // Best effort: the journal never decides the outcome.
        let _ = std::fs::create_dir_all(&dir);
        Journal::new(&dir)
    });
    let warn = |message: String| {
        if let Some(journal) = &journal {
            journal.log("WARN", &format!("{}: {message}", event_label(event)));
        }
        EventOutcome::Failed(message)
    };

    env.sleep(SETTLE_DELAY);
    let snapshot = match env.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return warn(format!("cannot read the session: {error}")),
    };
    let Some(pane) = event.target(&snapshot) else {
        return EventOutcome::Ignored;
    };
    if !snapshot.agent(&pane).is_some_and(|a| a.is_sleeping()) {
        return EventOutcome::Ignored;
    }

    match wake_agent(env, config, &pane, WakeOptions { wait: false }) {
        WakeOutcome::Woken { .. } => {}
        WakeOutcome::NotFound | WakeOutcome::NotSleeping | WakeOutcome::Deduplicated => {
            return EventOutcome::Ignored;
        }
        WakeOutcome::TimedOut => return warn(format!("{pane}: wake timed out")),
        WakeOutcome::Failed(reason) => return warn(format!("{pane}: {reason}")),
    }
    if let Some(journal) = &journal {
        journal.log("FOCUS-WAKE", &format!("{pane} {}", event_label(event)));
    }

    // Do not exit before the wake took effect.
    let mut lookup_failed = None;
    let awake = wait_until(env, AWAKE_TIMEOUT, AWAKE_POLL, |_| match env.agent(&pane) {
        Ok(agent) => !agent.is_sleeping(),
        Err(error) => {
            lookup_failed = Some(error);
            true
        }
    });
    if let Some(error) = lookup_failed {
        return warn(format!("{pane}: cannot follow the wake: {error}"));
    }
    if !awake {
        return warn(format!(
            "{pane}: still asleep after {} s",
            AWAKE_TIMEOUT.as_secs()
        ));
    }
    sync_markers(env, config);
    EventOutcome::Woke { pane }
}

fn event_label(event: &FocusEvent) -> &str {
    if event.name.is_empty() {
        "focus"
    } else {
        &event.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::{Refusal, SleepMode};
    use crate::herdr::{Agent, Pane, ProcessInfo, Tab, Workspace};
    use crate::ops::EnvResult;
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn reads_top_level_payload_fields() {
        let event = FocusEvent::from_env(lookup(&[
            ("HERDR_PLUGIN_EVENT", "pane.focused"),
            (
                "HERDR_PLUGIN_EVENT_JSON",
                r#"{"pane_id":"w1:p2","workspace_id":"w1"}"#,
            ),
        ]));
        assert_eq!(event.kind, Some(FocusKind::Pane));
        assert_eq!(event.pane_id.as_deref(), Some("w1:p2"));
        assert_eq!(event.workspace_id.as_deref(), Some("w1"));
        assert_eq!(event.tab_id, None);
    }

    #[test]
    fn reads_payload_fields_under_data() {
        let event = FocusEvent::from_env(lookup(&[
            ("HERDR_PLUGIN_EVENT", "tab.focused"),
            (
                "HERDR_PLUGIN_EVENT_JSON",
                r#"{"event":"tab_focused","data":{"tab_id":"w1:t2","workspace_id":"w1"}}"#,
            ),
        ]));
        assert_eq!(event.kind, Some(FocusKind::Tab));
        assert_eq!(event.tab_id.as_deref(), Some("w1:t2"));
        assert_eq!(event.pane_id, None);
    }

    #[test]
    fn the_event_name_falls_back_to_the_payload() {
        let event = FocusEvent::from_env(lookup(&[(
            "HERDR_PLUGIN_EVENT_JSON",
            r#"{"event":"workspace_focused","data":{"workspace_id":"w3"}}"#,
        )]));
        assert_eq!(event.kind, Some(FocusKind::Workspace));
        assert_eq!(event.name, "workspace_focused");
        assert_eq!(event.workspace_id.as_deref(), Some("w3"));
    }

    #[test]
    fn reads_only_the_context() {
        let event = FocusEvent::from_env(lookup(&[(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"data":{"workspace_id":"w1","tab_id":"w1:t1","focused_pane_id":"w1:p3"}}"#,
        )]));
        assert_eq!(event.kind, None);
        assert_eq!(event.pane_id, None);
        assert_eq!(event.context_pane_id.as_deref(), Some("w1:p3"));
        assert_eq!(event.context_tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(event.context_workspace_id.as_deref(), Some("w1"));
    }

    #[test]
    fn plain_variables_complete_the_context() {
        let event = FocusEvent::from_env(lookup(&[
            ("HERDR_WORKSPACE_ID", "w2"),
            ("HERDR_TAB_ID", "w2:t1"),
            ("HERDR_PANE_ID", "w2:p1"),
            ("HERDR_PLUGIN_CONTEXT_JSON", r#"{"tab_id":"w2:t9"}"#),
        ]));
        assert_eq!(event.context_workspace_id.as_deref(), Some("w2"));
        assert_eq!(event.context_tab_id.as_deref(), Some("w2:t9"));
        assert_eq!(event.context_pane_id.as_deref(), Some("w2:p1"));
    }

    #[test]
    fn garbage_and_empty_input_yield_an_empty_event() {
        for json in ["not json", "[1,2]", "\"pane_id\"", "{", ""] {
            let event = FocusEvent::from_env(lookup(&[
                ("HERDR_PLUGIN_EVENT_JSON", json),
                ("HERDR_PLUGIN_CONTEXT_JSON", json),
            ]));
            assert_eq!(event, FocusEvent::default(), "{json}");
        }
        assert_eq!(FocusEvent::from_env(lookup(&[])), FocusEvent::default());
        let blank =
            FocusEvent::from_env(lookup(&[("HERDR_PLUGIN_EVENT_JSON", r#"{"pane_id":""}"#)]));
        assert_eq!(blank.pane_id, None);
    }

    #[test]
    fn an_unknown_event_name_has_no_kind() {
        let event = FocusEvent::from_env(lookup(&[("HERDR_PLUGIN_EVENT", "pane.closed")]));
        assert_eq!(event.kind, None);
        assert_eq!(event.name, "pane.closed");
    }

    // Behaviour with a scripted environment ------------------------------

    const EPOCH_MS: u64 = 1_000_000_000_000;

    /// Session: workspace `w1` with active tab `w1:t1` (panes `p1` focused,
    /// `p2`), tab `w1:t2` (panes `p3`, `p4` focused). `p1`, `p2`, `p3` and `p4`
    /// are sleeping agents unless `awake` says otherwise.
    struct Fake {
        base: Instant,
        elapsed: Cell<Duration>,
        calls: RefCell<Vec<String>>,
        state_dir: PathBuf,
        awake: Vec<&'static str>,
        snapshot_error: bool,
        keys_error: bool,
        /// Time after the first Enter at which Herdr stops reporting the
        /// Placeholder; `None` for never.
        awake_after: Option<Duration>,
        enter_at: Cell<Option<Duration>>,
    }

    impl Fake {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-sleep-event-{name}-{}-{}",
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
                state_dir: dir,
                awake: Vec::new(),
                snapshot_error: false,
                keys_error: false,
                awake_after: Some(Duration::from_millis(1000)),
                enter_at: Cell::new(None),
            }
        }

        fn log(&self, call: impl Into<String>) {
            self.calls.borrow_mut().push(call.into());
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn enters(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| c.starts_with("keys"))
                .collect()
        }

        fn clock(&self) -> Duration {
            self.elapsed.get()
        }

        fn journal(&self) -> String {
            std::fs::read_to_string(self.state_dir.join(crate::journal::FILE_NAME))
                .unwrap_or_default()
        }

        fn events(&self) -> Vec<String> {
            self.journal()
                .lines()
                .map(|l| l.split(' ').nth(1).unwrap().to_string())
                .collect()
        }

        fn is_awake(&self, pane: &str) -> bool {
            self.awake.contains(&pane)
                || matches!(
                    (self.enter_at.get(), self.awake_after),
                    (Some(at), Some(after)) if self.clock() >= at + after
                )
        }

        fn agent_entry(&self, pane: &str, sleeping: bool) -> Agent {
            Agent {
                pane_id: pane.into(),
                agent: Some(
                    if sleeping {
                        "sleeping-claude"
                    } else {
                        "claude"
                    }
                    .into(),
                ),
                ..Agent::default()
            }
        }

        fn session(&self) -> Snapshot {
            let pane = |id: &str, tab: &str, focused: bool| Pane {
                pane_id: format!("w1:{id}"),
                tab_id: format!("w1:{tab}"),
                workspace_id: "w1".into(),
                focused,
                ..Pane::default()
            };
            Snapshot {
                agents: ["p1", "p2", "p3", "p4"]
                    .iter()
                    .map(|p| {
                        let id = format!("w1:{p}");
                        let sleeping = !self.awake.contains(&id.as_str());
                        self.agent_entry(&id, sleeping)
                    })
                    .collect(),
                panes: vec![
                    pane("p1", "t1", true),
                    pane("p2", "t1", false),
                    pane("p3", "t2", false),
                    pane("p4", "t2", true),
                ],
                tabs: vec![
                    Tab {
                        tab_id: "w1:t1".into(),
                        workspace_id: "w1".into(),
                        focused: true,
                        ..Tab::default()
                    },
                    Tab {
                        tab_id: "w1:t2".into(),
                        workspace_id: "w1".into(),
                        ..Tab::default()
                    },
                ],
                workspaces: vec![Workspace {
                    workspace_id: "w1".into(),
                    focused: true,
                    active_tab_id: Some("w1:t1".into()),
                    ..Workspace::default()
                }],
                focused_pane_id: Some("w1:p1".into()),
                focused_tab_id: Some("w1:t1".into()),
                focused_workspace_id: Some("w1".into()),
                ..Snapshot::default()
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }

    impl Env for Fake {
        fn snapshot(&self) -> EnvResult<Snapshot> {
            self.log(format!("snapshot at {}", self.clock().as_millis()));
            if self.snapshot_error {
                return Err("socket closed".into());
            }
            Ok(self.session())
        }

        fn pane(&self, _: &str) -> EnvResult<Pane> {
            unreachable!()
        }

        fn agent(&self, target: &str) -> EnvResult<Agent> {
            self.log(format!("agent {target}"));
            let known = self.session().agent(target).is_some();
            if !known {
                return Err("not found".into());
            }
            Ok(self.agent_entry(target, !self.is_awake(target)))
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

        fn send_keys(&self, pane: &str, keys: &[&str]) -> EnvResult<()> {
            self.log(format!("keys {pane} {}", keys.join(" ")));
            if self.keys_error {
                return Err("pane gone".into());
            }
            if self.enter_at.get().is_none() {
                self.enter_at.set(Some(self.clock()));
            }
            Ok(())
        }

        fn run(&self, _: &str, _: &[&str]) -> EnvResult<()> {
            unreachable!()
        }

        fn notify(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!()
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
            unreachable!()
        }

        fn agent_rss_kb(&self, _: &Agent, _: &str) -> Option<u64> {
            None
        }

        fn shell_path(&self, _: i32) -> Option<PathBuf> {
            None
        }

        fn state_dir(&self) -> Option<PathBuf> {
            Some(self.state_dir.clone())
        }

        fn placeholder_bin(&self) -> EnvResult<PathBuf> {
            unreachable!()
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
            EPOCH_MS + u64::try_from(self.elapsed.get().as_millis()).unwrap()
        }
    }

    fn event(name: &str, json: &str) -> FocusEvent {
        FocusEvent::from_env(lookup(&[
            ("HERDR_PLUGIN_EVENT", name),
            ("HERDR_PLUGIN_EVENT_JSON", json),
        ]))
    }

    fn handle(fake: &Fake, event: &FocusEvent) -> EventOutcome {
        handle_focus_event(fake, &Config::default(), event)
    }

    fn woke(pane: &str) -> EventOutcome {
        EventOutcome::Woke { pane: pane.into() }
    }

    #[test]
    fn focusing_a_sleeping_pane_wakes_exactly_that_pane() {
        let fake = Fake::new("pane");
        let outcome = handle(&fake, &event("pane.focused", r#"{"pane_id":"w1:p2"}"#));
        assert_eq!(outcome, woke("w1:p2"));
        assert_eq!(fake.enters(), ["keys w1:p2 enter"]);
        assert_eq!(fake.events(), ["WAKE", "FOCUS-WAKE"]);
        assert!(fake.journal().contains("FOCUS-WAKE w1:p2 pane.focused"));
    }

    #[test]
    fn focusing_an_awake_pane_does_nothing() {
        let mut fake = Fake::new("awake");
        fake.awake = vec!["w1:p2"];
        let outcome = handle(&fake, &event("pane.focused", r#"{"pane_id":"w1:p2"}"#));
        assert_eq!(outcome, EventOutcome::Ignored);
        assert!(fake.enters().is_empty());
        assert_eq!(fake.journal(), "");
    }

    #[test]
    fn a_pane_that_is_not_an_agent_is_ignored() {
        let fake = Fake::new("shell");
        let outcome = handle(&fake, &event("pane.focused", r#"{"pane_id":"w1:p9"}"#));
        assert_eq!(outcome, EventOutcome::Ignored);
        assert!(fake.enters().is_empty());
    }

    #[test]
    fn focusing_a_tab_wakes_only_its_focused_pane() {
        let fake = Fake::new("tab");
        let outcome = handle(&fake, &event("tab.focused", r#"{"tab_id":"w1:t2"}"#));
        assert_eq!(outcome, woke("w1:p4"));
        assert_eq!(fake.enters(), ["keys w1:p4 enter"]);
    }

    #[test]
    fn a_tab_payload_under_data_resolves_the_same_way() {
        let fake = Fake::new("tab-data");
        let outcome = handle(
            &fake,
            &event("tab.focused", r#"{"data":{"tab_id":"w1:t1"}}"#),
        );
        assert_eq!(outcome, woke("w1:p1"));
        assert_eq!(fake.enters(), ["keys w1:p1 enter"]);
    }

    #[test]
    fn focusing_a_workspace_wakes_only_the_pane_of_its_active_tab() {
        let fake = Fake::new("workspace");
        let outcome = handle(
            &fake,
            &event("workspace.focused", r#"{"workspace_id":"w1"}"#),
        );
        assert_eq!(outcome, woke("w1:p1"));
        assert_eq!(fake.enters(), ["keys w1:p1 enter"]);
    }

    #[test]
    fn the_context_names_the_pane_when_the_payload_does_not() {
        let fake = Fake::new("context");
        let event = FocusEvent::from_env(lookup(&[
            ("HERDR_PLUGIN_EVENT", "pane.focused"),
            (
                "HERDR_PLUGIN_CONTEXT_JSON",
                r#"{"focused_pane_id":"w1:p3"}"#,
            ),
        ]));
        assert_eq!(handle(&fake, &event), woke("w1:p3"));
        assert_eq!(fake.enters(), ["keys w1:p3 enter"]);
    }

    #[test]
    fn an_unresolvable_target_does_nothing() {
        let fake = Fake::new("none");
        for event in [
            event("pane.focused", "{}"),
            event("tab.focused", r#"{"tab_id":"w9:t9"}"#),
            event("workspace.focused", r#"{"workspace_id":"w9"}"#),
            FocusEvent::default(),
        ] {
            assert_eq!(handle(&fake, &event), EventOutcome::Ignored);
        }
        assert!(fake.enters().is_empty());
        assert_eq!(fake.journal(), "");
    }

    #[test]
    fn twin_events_wake_once() {
        let mut fake = Fake::new("twins");
        fake.awake_after = None;
        let pane = event("pane.focused", r#"{"pane_id":"w1:p2"}"#);
        let tab = event("tab.focused", r#"{"pane_id":"w1:p2"}"#);
        assert!(matches!(handle(&fake, &pane), EventOutcome::Failed(_)));
        // The twin ran alongside the first one, 5 s after its Enter.
        fake.elapsed.set(Duration::from_secs(5));
        assert_eq!(handle(&fake, &tab), EventOutcome::Ignored);
        assert_eq!(fake.enters(), ["keys w1:p2 enter"]);
        assert_eq!(fake.events(), ["WAKE", "FOCUS-WAKE", "WARN"]);
    }

    #[test]
    fn the_settle_delay_comes_before_the_snapshot() {
        let fake = Fake::new("settle");
        handle(&fake, &event("pane.focused", r#"{"pane_id":"w1:p1"}"#));
        assert_eq!(fake.calls()[0], "snapshot at 400");
    }

    #[test]
    fn waits_until_herdr_stops_reporting_the_placeholder() {
        let fake = Fake::new("wait");
        let outcome = handle(&fake, &event("pane.focused", r#"{"pane_id":"w1:p1"}"#));
        assert_eq!(outcome, woke("w1:p1"));
        assert_eq!(fake.clock(), SETTLE_DELAY + Duration::from_millis(1000));
    }

    #[test]
    fn gives_up_after_20_seconds() {
        let mut fake = Fake::new("giveup");
        fake.awake_after = None;
        let outcome = handle(&fake, &event("pane.focused", r#"{"pane_id":"w1:p1"}"#));
        assert!(matches!(outcome, EventOutcome::Failed(_)));
        assert_eq!(fake.clock(), SETTLE_DELAY + AWAKE_TIMEOUT);
        assert_eq!(fake.events(), ["WAKE", "FOCUS-WAKE", "WARN"]);
        assert!(fake.journal().contains("still asleep after 20 s"));
    }

    #[test]
    fn a_failing_snapshot_is_journaled() {
        let mut fake = Fake::new("snapshot-error");
        fake.snapshot_error = true;
        let outcome = handle(&fake, &event("pane.focused", r#"{"pane_id":"w1:p1"}"#));
        assert!(matches!(outcome, EventOutcome::Failed(_)));
        assert_eq!(fake.events(), ["WARN"]);
        assert!(
            fake.journal()
                .contains("pane.focused: cannot read the session")
        );
    }

    #[test]
    fn a_failing_enter_is_journaled_and_not_reported_as_a_wake() {
        let mut fake = Fake::new("keys-error");
        fake.keys_error = true;
        let outcome = handle(&fake, &event("pane.focused", r#"{"pane_id":"w1:p1"}"#));
        assert!(matches!(outcome, EventOutcome::Failed(_)));
        assert_eq!(fake.events(), ["WAKE", "WARN"]);
        assert!(fake.journal().contains("cannot press Enter"));
    }
}

//! `herdr-sleep event` end to end: the real binary against a scripted `herdr`
//! shell script, run the way the focus hooks run it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SNAPSHOT: &str = r#"{"id":"x","result":{"snapshot":{
"agents":[
 {"pane_id":"wC:p1","agent":"sleeping-claude","agent_status":"idle","tab_id":"wC:t1","workspace_id":"wC"},
 {"pane_id":"wC:p2","agent":"sleeping-claude","agent_status":"idle","tab_id":"wC:t1","workspace_id":"wC"}],
"panes":[
 {"pane_id":"wC:p1","tab_id":"wC:t1","workspace_id":"wC","focused":false},
 {"pane_id":"wC:p2","tab_id":"wC:t1","workspace_id":"wC","focused":true}],
"tabs":[{"tab_id":"wC:t1","workspace_id":"wC","focused":true}],
"workspaces":[{"workspace_id":"wC","focused":true,"active_tab_id":"wC:t1"}],
"focused_pane_id":"wC:p2","focused_tab_id":"wC:t1","focused_workspace_id":"wC"}}}"#;

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "herdr-sleep-event-flow-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let sandbox = Sandbox { dir };
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let agent = fs::read_to_string(fixtures.join("agent-get.json"))
            .unwrap()
            .replace(r#""agent_status":"working""#, r#""agent_status":"idle""#);
        fs::write(sandbox.path("agent-awake.json"), &agent).unwrap();
        fs::write(
            sandbox.path("agent-sleeping.json"),
            agent.replace(
                r#""agent":"claude","agent_session""#,
                r#""agent":"sleeping-claude","agent_session""#,
            ),
        )
        .unwrap();
        fs::write(sandbox.path("snapshot.json"), SNAPSHOT).unwrap();
        let script = format!(
            r#"#!/bin/sh
D="{dir}"
echo "$*" >> "$D/calls"
case "$1 $2" in
  "api snapshot") cat "$D/snapshot.json" ;;
  "agent get")
    # Answer for the pane that was asked about, whatever the fixture holds.
    if [ -e "$D/enter" ]; then F="$D/agent-awake.json"
    else F="$D/agent-sleeping.json"; fi
    sed "s/\"pane_id\":\"wC:p1\"/\"pane_id\":\"$3\"/" "$F" ;;
  "pane send-keys") touch "$D/enter"; echo '{{"id":"x","result":{{"type":"ok"}}}}' ;;
  *) echo '{{"id":"x","result":{{"type":"ok"}}}}' ;;
esac
"#,
            dir = sandbox.dir.display()
        );
        let path = sandbox.path("herdr");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        sandbox
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn event(&self, name: &str, payload: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-sleep"))
            .arg("event")
            .env("HERDR_BIN_PATH", self.path("herdr"))
            // No socket, no Watcher: these tests must not start one.
            .env_remove("HERDR_SOCKET_PATH")
            .env("HERDR_SLEEP_STATE_DIR", self.path("state"))
            .env("HERDR_SLEEP_CONFIG_DIR", self.path("config"))
            .env("HERDR_PLUGIN_EVENT", name)
            .env("HERDR_PLUGIN_EVENT_JSON", payload)
            .env("HERDR_PLUGIN_CONTEXT_JSON", "{}")
            .env("HERDR_WORKSPACE_ID", "wC")
            .env("HERDR_TAB_ID", "wC:t1")
            .env("HERDR_PANE_ID", "wC:p2")
            .output()
            .unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn presses(sandbox: &Sandbox) -> Vec<String> {
    sandbox
        .calls()
        .into_iter()
        .filter(|c| c.contains("send-keys"))
        .collect()
}

#[test]
fn a_pane_focus_wakes_that_pane() {
    let sandbox = Sandbox::new("pane");
    let output = sandbox.event("pane.focused", r#"{"pane_id":"wC:p1","workspace_id":"wC"}"#);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty());
    assert_eq!(presses(&sandbox), ["pane send-keys wC:p1 enter"]);
    let journal = fs::read_to_string(sandbox.path("state/herdr-sleep.log")).unwrap();
    assert!(
        journal.contains("FOCUS-WAKE wC:p1 pane.focused"),
        "{journal}"
    );
}

#[test]
fn a_tab_focus_wakes_the_focused_pane_only() {
    let sandbox = Sandbox::new("tab");
    let output = sandbox.event("tab.focused", r#"{"data":{"tab_id":"wC:t1"}}"#);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(presses(&sandbox), ["pane send-keys wC:p2 enter"]);
}

#[test]
fn an_unknown_target_exits_0_without_touching_a_pane() {
    let sandbox = Sandbox::new("unknown");
    fs::write(sandbox.path("snapshot.json"), "not json").unwrap();
    let output = sandbox.event("pane.focused", r#"{"pane_id":"wC:p1"}"#);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(presses(&sandbox).is_empty());
    let journal = fs::read_to_string(sandbox.path("state/herdr-sleep.log")).unwrap();
    assert!(journal.contains("WARN"), "{journal}");
}

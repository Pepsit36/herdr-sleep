//! Marker synchronisation through the real [`LiveEnv`] and a scripted `herdr`
//! shell script that answers `api snapshot` with a recorded session in which
//! one pane sleeps, and remembers every call.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use herdr_sleep::config::Config;
use herdr_sleep::herdr::HerdrClient;
use herdr_sleep::ops::{LiveEnv, MarkersState, sync_markers};

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    /// A sandbox whose snapshot is the recorded one with the agent in `w3:p1`
    /// replaced by its Placeholder.
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "herdr-sleep-markers-flow-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot.json");
        let mut document: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(fixture).unwrap()).unwrap();
        let snapshot = &mut document["result"]["snapshot"];
        for list in ["agents", "panes"] {
            for entry in snapshot[list].as_array_mut().unwrap() {
                if entry["pane_id"] == "w3:p1" {
                    entry["agent"] = "sleeping-claude".into();
                }
            }
        }
        fs::write(dir.join("snapshot.json"), document.to_string()).unwrap();

        let script = format!(
            r#"#!/bin/sh
D="{dir}"
echo "$*" >> "$D/calls"
case "$1 $2" in
  "api snapshot") cat "$D/snapshot.json" ;;
  *) echo '{{"id":"x","result":{{"type":"ok"}}}}' ;;
esac
"#,
            dir = dir.display()
        );
        let herdr = dir.join("herdr");
        fs::write(&herdr, script).unwrap();
        fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox { dir }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn env(&self) -> LiveEnv {
        LiveEnv::new(HerdrClient::with_binary(self.dir.join("herdr")))
            .with_state_dir(self.dir.join("state"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_sleeping_pane_marks_its_workspace_and_tab() {
    let sandbox = Sandbox::new("sleeping");
    let env = sandbox.env();

    let report = sync_markers(&env, &Config::default());
    assert_eq!((report.workspaces_set, report.tabs_prefixed), (1, 1));

    let calls = sandbox.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], "api snapshot");
    let prefix =
        "workspace report-metadata w3 --source pepsit36.herdr-sleep --token sleep=💤 --seq ";
    assert!(calls[1].starts_with(prefix), "{calls:?}");
    assert_eq!(calls[2], "tab rename w3:t1 💤 1");

    let state = MarkersState::load(&sandbox.dir.join("state"));
    assert!(state.prefixed_tabs.contains("w3:t1"));
    assert_eq!(state.workspace_tokens.get("w3"), Some(&true));
    assert!(
        !sandbox.dir.join("state/markers.json.tmp").exists(),
        "temporary file left behind"
    );

    // The recorded snapshot still shows the old label, but the workspace is
    // remembered: only the prefix is retried, never the token.
    let again = sync_markers(&env, &Config::default());
    assert_eq!((again.workspaces_set, again.tabs_prefixed), (0, 0));
}

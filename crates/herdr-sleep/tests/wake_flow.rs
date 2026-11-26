//! `herdr-sleep wake` end to end: the real binary against a scripted `herdr`
//! shell script that answers with the JSON of recorded Herdr output, turns the
//! Sleeping agent into a Ready agent once Enter was pressed, and remembers
//! every call.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
            "herdr-sleep-wake-flow-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let sandbox = Sandbox { dir };
        sandbox.install_fixtures();
        sandbox.write_executable("herdr", &sandbox.herdr_script());
        sandbox
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn write_executable(&self, name: &str, body: &str) {
        let path = self.path(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn install_fixtures(&self) {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let agent = fs::read_to_string(fixtures.join("agent-get.json"))
            .unwrap()
            .replace(r#""agent_status":"working""#, r#""agent_status":"idle""#);
        fs::write(self.path("agent-awake.json"), &agent).unwrap();
        fs::write(
            self.path("agent-sleeping.json"),
            agent.replace(
                r#""agent":"claude","agent_session""#,
                r#""agent":"sleeping-claude","agent_session""#,
            ),
        )
        .unwrap();
        fs::copy(
            fixtures.join("pane-get-missing.json"),
            self.path("missing.json"),
        )
        .unwrap();
    }

    /// A `herdr` whose answers follow the files it leaves in its directory:
    /// `enter` appears with the first Enter; the agent is reported awake from
    /// the second lookup after it. `agent get nope` is unknown.
    fn herdr_script(&self) -> String {
        format!(
            r#"#!/bin/sh
D="{dir}"
echo "$*" >> "$D/calls"
case "$1 $2" in
  "agent get")
    if [ "$3" = "nope" ]; then cat "$D/missing.json"; exit 1; fi
    if [ -e "$D/awake" ]; then cat "$D/agent-awake.json"
    elif [ -e "$D/enter" ]; then touch "$D/awake"; cat "$D/agent-sleeping.json"
    elif [ -e "$D/always-awake" ]; then cat "$D/agent-awake.json"
    else cat "$D/agent-sleeping.json"; fi ;;
  "api snapshot") echo '{{"id":"x","result":{{"snapshot":{{}}}}}}' ;;
  "pane read") printf 'output\n\342\235\257 \n' ;;
  "pane send-text") touch "$D/enter"; echo '{{"id":"x","result":{{"type":"ok"}}}}' ;;
  *) echo '{{"id":"x","result":{{"type":"ok"}}}}' ;;
esac
"#,
            dir = self.dir.display()
        )
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn wake(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-sleep"))
            .arg("wake")
            .args(args)
            .env("HERDR_BIN_PATH", self.path("herdr"))
            // No socket, no Watcher: these tests must not start one.
            .env_remove("HERDR_SOCKET_PATH")
            .env("HERDR_SLEEP_STATE_DIR", self.path("state"))
            .env("HERDR_SLEEP_CONFIG_DIR", self.path("config"))
            .output()
            .unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn wake_waits_for_the_agent_and_reports_json() {
    let sandbox = Sandbox::new("wait");
    let output = sandbox.wake(&["wC:p1", "--json"]);
    assert!(output.status.success(), "{output:?}");

    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["woke"], true);
    assert_eq!(report["pane"], "wC:p1");
    assert!(report["name"].is_null());
    assert!(report["ms"].as_u64().unwrap() >= 250, "{report}");
    assert!(report["reason"].is_null());

    let calls = sandbox.calls();
    let enter = calls
        .iter()
        .position(|c| c == "pane send-text wC:p1 \u{1b}[I")
        .unwrap_or_else(|| panic!("no Enter in {calls:?}"));
    assert_eq!(
        calls.iter().filter(|c| c.contains("send-text")).count(),
        1,
        "{calls:?}"
    );
    assert!(calls[..enter].iter().all(|c| !c.contains("pane read")));
    assert!(
        calls[enter..]
            .iter()
            .any(|c| c.starts_with("pane read wC:p1 --lines 40 --format ansi"))
    );

    let journal = fs::read_to_string(sandbox.path("state/herdr-sleep.log")).unwrap();
    let events: Vec<&str> = journal
        .lines()
        .map(|l| l.split(' ').nth(1).unwrap())
        .collect();
    assert_eq!(events, ["WAKE", "READY"]);
    assert!(sandbox.path("state/wake-wC_p1.ts").is_file());
}

#[test]
fn no_wait_presses_enter_and_returns() {
    let sandbox = Sandbox::new("nowait");
    let output = sandbox.wake(&["wC:p1", "--no-wait"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output), "wC:p1 is waking up");
    let calls = sandbox.calls();
    assert!(calls.contains(&"pane send-text wC:p1 \u{1b}[I".to_string()));
    assert!(calls.iter().all(|c| !c.contains("pane read")));

    // The focus hook fires twice for one focus: the second wake does nothing
    // (the Placeholder is still the one reported, as right after the first Enter).
    let _ = fs::remove_file(sandbox.path("enter"));
    let again = sandbox.wake(&["wC:p1", "--no-wait", "--json"]);
    assert!(again.status.success());
    let report: serde_json::Value = serde_json::from_str(&stdout(&again)).unwrap();
    assert_eq!(report["woke"], false);
    assert_eq!(report["reason"], "deduplicated");
    assert_eq!(
        sandbox
            .calls()
            .iter()
            .filter(|c| c.contains("send-text"))
            .count(),
        1
    );
}

#[test]
fn an_awake_agent_is_not_touched() {
    let sandbox = Sandbox::new("awake");
    fs::write(sandbox.path("always-awake"), "").unwrap();
    let output = sandbox.wake(&["wC:p1", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["woke"], false);
    assert_eq!(report["reason"], "not-sleeping");
    assert!(sandbox.calls().iter().all(|c| !c.contains("send-text")));
}

#[test]
fn an_unknown_agent_exits_1() {
    let sandbox = Sandbox::new("missing");
    let output = sandbox.wake(&["nope", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["woke"], false);
    assert_eq!(report["pane"], "nope");
    assert_eq!(report["reason"], "not-found");
}

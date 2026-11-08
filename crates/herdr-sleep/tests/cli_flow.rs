//! The command line end to end: the real binary against a scripted `herdr`
//! shell script, run the way Herdr runs plugin actions.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

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
            "herdr-sleep-cli-flow-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let sandbox = Sandbox { dir };
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for file in [
            "snapshot.json",
            "agent-get.json",
            "process-info-current.json",
        ] {
            fs::copy(fixtures.join(file), sandbox.path(file)).unwrap();
        }
        let script = format!(
            r#"#!/bin/sh
D="{dir}"
echo "$*" >> "$D/calls"
case "$1 $2" in
  "api snapshot") cat "$D/snapshot.json" ;;
  "pane process-info") cat "$D/process-info-current.json" ;;
  "agent get")
    # Answer for the pane that was asked about, whatever the fixture holds.
    sed "s/\"pane_id\":\"wC:p1\"/\"pane_id\":\"$3\"/" "$D/agent-get.json" ;;
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

    /// The binary with a clean, Herdr-free environment.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-sleep"));
        command
            .args(args)
            .env("HERDR_BIN_PATH", self.path("herdr"))
            // No socket, no Watcher: these tests must not start one.
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_PLUGIN_CONTEXT_JSON")
            .env("HERDR_SLEEP_STATE_DIR", self.path("state"))
            .env("HERDR_SLEEP_CONFIG_DIR", self.path("config"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn json(output: &Output) -> Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|e| panic!("{e}: {output:?}"))
}

#[test]
fn status_json_lists_the_managed_agents() {
    let sandbox = Sandbox::new("status-json");
    let output = sandbox.run(&["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value = json(&output);
    let agents = value["agents"].as_array().unwrap();
    let panes: Vec<_> = agents.iter().map(|a| a["pane"].as_str().unwrap()).collect();
    assert_eq!(
        panes,
        ["w3:p1", "w5:p1", "w9:p1", "wA:p1", "wB:p1", "wC:p1"]
    );
    assert_eq!(agents[0]["kind"], "claude");
    assert_eq!(agents[0]["status"], "idle");
    assert_eq!(agents[0]["sleeping"], false);
    assert_eq!(agents[0]["name"], Value::Null);
    assert_eq!(agents[0]["idle_seconds"], Value::Null);
    assert_eq!(agents[5]["status"], "working");
    assert_eq!(value["watcher"]["running"], false);
    assert_eq!(
        value["state_dir"].as_str().map(PathBuf::from),
        Some(sandbox.path("state"))
    );
}

#[test]
fn status_text_has_a_header_and_a_footer() {
    let sandbox = Sandbox::new("status-text");
    let output = sandbox.run(&["status"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let lines: Vec<_> = text.lines().collect();
    assert!(
        lines[0].contains("PANE") && lines[0].contains("REFUSAL"),
        "{text}"
    );
    assert!(
        lines[1].contains("w3:p1") && lines[1].contains("claude"),
        "{text}"
    );
    assert!(text.contains("watcher: not running\n"), "{text}");
    assert!(
        text.contains(&format!("state: {}\n", sandbox.path("state").display())),
        "{text}"
    );
}

#[test]
fn an_unusable_state_directory_is_a_clean_error() {
    let sandbox = Sandbox::new("bad-state");
    fs::write(sandbox.path("plain-file"), "x").unwrap();
    let bad = sandbox.path("plain-file/state");
    let output = sandbox
        .command(&["status"])
        .env("HERDR_SLEEP_STATE_DIR", &bad)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let message = stderr(&output);
    assert!(
        message.starts_with("herdr-sleep: cannot use the state directory"),
        "{message}"
    );
    assert!(!message.contains("panicked"), "{message}");
    assert!(output.stdout.is_empty());
}

#[test]
fn an_invalid_configuration_is_a_clean_error() {
    let sandbox = Sandbox::new("bad-config");
    fs::create_dir_all(sandbox.path("config")).unwrap();
    fs::write(
        sandbox.path("config/config.toml"),
        "sleep_after_minutes = 'x'",
    )
    .unwrap();
    for command in ["status", "sweep", "config"] {
        let output = sandbox.run(&[command]);
        assert_eq!(output.status.code(), Some(1), "{command}: {output:?}");
        assert!(
            stderr(&output).starts_with("herdr-sleep: invalid configuration"),
            "{command}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn sleep_uses_the_action_context_pane() {
    let sandbox = Sandbox::new("sleep-context");
    let output = sandbox
        .command(&["sleep", "--json"])
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"pane_id":"w9:p7","focused_pane_id":"w9:p8","workspace_id":"w9"}"#,
        )
        .env("HERDR_PANE_ID", "w9:p9")
        .output()
        .unwrap();
    let value = json(&output);
    assert_eq!(value["pane"], "w9:p7", "{output:?}");
    // The fixture agent is still working: a guard refuses, with exit code 1.
    assert_eq!(value["slept"], false);
    assert_eq!(value["outcome"], "refused");
    assert_eq!(output.status.code(), Some(1));
    assert!(sandbox.calls().contains(&"agent get w9:p7".to_string()));
}

#[test]
fn sleep_falls_back_to_the_focused_pane_then_the_environment() {
    let sandbox = Sandbox::new("sleep-fallback");
    let focused = sandbox
        .command(&["sleep"])
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"focused_pane_id":"w9:p8"}"#,
        )
        .env("HERDR_PANE_ID", "w9:p9")
        .output()
        .unwrap();
    assert!(
        stdout(&focused).starts_with("w9:p8 was not put to sleep"),
        "{focused:?}"
    );
    let env_only = sandbox
        .command(&["sleep"])
        .env("HERDR_PANE_ID", "w9:p9")
        .output()
        .unwrap();
    assert!(
        stdout(&env_only).starts_with("w9:p9 was not put to sleep"),
        "{env_only:?}"
    );
}

#[test]
fn wake_uses_the_action_context_pane() {
    let sandbox = Sandbox::new("wake-context");
    let output = sandbox
        .command(&["wake"])
        .env("HERDR_PLUGIN_CONTEXT_JSON", r#"{"pane_id":"w9:p7"}"#)
        .output()
        .unwrap();
    // The fixture agent is awake: nothing to wake, a single plain line.
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(stdout(&output), "w9:p7 is not asleep\n");
    assert!(sandbox.calls().contains(&"agent get w9:p7".to_string()));
}

#[test]
fn sleep_and_wake_without_any_target_are_usage_errors() {
    let sandbox = Sandbox::new("no-target");
    for command in ["sleep", "wake"] {
        let output = sandbox.run(&[command]);
        assert_eq!(output.status.code(), Some(2), "{command}: {output:?}");
        assert!(stderr(&output).starts_with(&format!("herdr-sleep: {command}: expects a pane")));
        assert!(output.stdout.is_empty());
    }
    assert!(sandbox.calls().is_empty());
}

#[test]
fn config_json_and_toml() {
    let sandbox = Sandbox::new("config");
    fs::create_dir_all(sandbox.path("config")).unwrap();
    fs::write(
        sandbox.path("config/config.toml"),
        "sleep_after_minutes = 7\nbogus = 1\n",
    )
    .unwrap();

    let output = sandbox.run(&["config", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value = json(&output);
    assert_eq!(value["sleep_after_minutes"], 7);
    assert_eq!(value["profiles"]["claude"]["bin"], "claude");
    let err = stderr(&output);
    assert!(err.contains(&format!(
        "config file: {}",
        sandbox.path("config/config.toml").display()
    )));
    assert!(
        err.contains("warning: unknown key `bogus` ignored"),
        "{err}"
    );

    let output = sandbox.run(&["config"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let table: toml::Table = stdout(&output).parse().unwrap();
    assert_eq!(table["sleep_after_minutes"].as_integer(), Some(7));
}

#[test]
fn sweep_json_reports_the_pass() {
    let sandbox = Sandbox::new("sweep");
    let output = sandbox.run(&["sweep", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value = json(&output);
    assert_eq!(value["considered"], 6);
    assert_eq!(value["slept"], serde_json::json!([]));
    assert_eq!(value["error"], Value::Null);
}

#[test]
fn ensure_watcher_json_without_a_socket_fails_cleanly() {
    let sandbox = Sandbox::new("ensure");
    let output = sandbox.run(&["ensure-watcher", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value = json(&output);
    assert_eq!(value["running"], false);
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("HERDR_SOCKET_PATH")
    );
}

#[test]
fn unknown_usage_exits_2_with_the_usage_on_stderr() {
    let sandbox = Sandbox::new("usage");
    for args in [
        &["nap"][..],
        &["status", "--bogus"],
        &["sleep", "a", "b"],
        &["watch", "now"],
    ] {
        let output = sandbox.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        let err = stderr(&output);
        assert!(err.starts_with("herdr-sleep: "), "{err}");
        assert!(err.contains("Usage: herdr-sleep <command>"), "{err}");
    }
    let output = sandbox.run(&["nap"]);
    assert!(stderr(&output).contains("unknown command 'nap'"));
}

#[test]
fn help_and_version_exit_0() {
    let sandbox = Sandbox::new("help");
    let output = sandbox.run(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).starts_with("Usage: herdr-sleep <command>"));
    assert!(!stdout(&output).contains("replay"));
    let output = sandbox.run(&["wake", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).starts_with("Usage: herdr-sleep wake"));
    let output = sandbox.run(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).starts_with("herdr-sleep "));
    assert!(sandbox.calls().is_empty());
}

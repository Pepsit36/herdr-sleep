//! The Watcher and the Sweep through the real binary, a scripted `herdr`
//! shell script and a socket path the test can delete.
//!
//! Every test owns a temporary state directory and kills the Watchers
//! started in it when it ends, however it ends.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use herdr_sleep::config::Config;
use herdr_sleep::guards::SleepMode;
use herdr_sleep::herdr::HerdrClient;
use herdr_sleep::ops::{LiveEnv, SweepState, sweep_with_mode};
use herdr_sleep::watcher::{WatcherLock, recorded_pid};

const SNAPSHOT: &str = r#"{"id":"x","result":{"snapshot":{
"agents":[{"pane_id":"wC:p1","agent":"claude","agent_status":"idle","focused":false,"state_change_seq":7,
 "agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"11111111-2222-4333-8444-555555555555"},
 "cwd":"/home/user/herdr-sleep","tab_id":"wC:t1","workspace_id":"wC"}],
"panes":[{"pane_id":"wC:p1","tab_id":"wC:t1","workspace_id":"wC","focused":false}],
"tabs":[{"tab_id":"wC:t1","workspace_id":"wC","focused":true}],
"workspaces":[{"workspace_id":"wC","focused":true,"active_tab_id":"wC:t1"}],
"focused_pane_id":null}}}"#;

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
            "herdr-sleep-watcher-flow-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("config")).unwrap();
        let sandbox = Sandbox { dir };
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let agent = fs::read_to_string(fixtures.join("agent-get.json"))
            .unwrap()
            .replace(r#""agent_status":"working""#, r#""agent_status":"idle""#);
        fs::write(sandbox.path("agent-get.json"), &agent).unwrap();
        fs::write(
            sandbox.path("agent-get-sleeping.json"),
            agent.replace(r#""agent":"claude""#, r#""agent":"sleeping-claude""#),
        )
        .unwrap();
        for name in ["pane-get-current.json", "process-info-current.json"] {
            fs::copy(fixtures.join(name), sandbox.path(name)).unwrap();
        }
        fs::write(
            sandbox.path("pane-get-quit.json"),
            r#"{"id":"cli:pane:get","result":{"pane":{"agent_status":"unknown","cwd":"/home/user/herdr-sleep","focused":false,"pane_id":"wC:p1","tab_id":"wC:t1","workspace_id":"wC"},"type":"pane_info"}}"#,
        )
        .unwrap();
        fs::write(
            sandbox.path("process-info-quit.json"),
            r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":184186,"foreground_processes":[],"pane_id":"wC:p1","shell_pid":184186},"type":"pane_process_info"}}"#,
        )
        .unwrap();
        fs::write(sandbox.path("snapshot.json"), SNAPSHOT).unwrap();
        fs::write(sandbox.path("socket"), "").unwrap();
        fs::write(
            sandbox.path("config/config.toml"),
            "sleep_after_minutes = 0\nsweep_interval_seconds = 5\n",
        )
        .unwrap();
        sandbox.write_executable("herdr", &sandbox.herdr_script());
        sandbox.write_executable("herdr-sleep-ph", "#!/bin/sh\nexit 0\n");
        sandbox
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn state(&self) -> PathBuf {
        self.path("state")
    }

    fn write_executable(&self, name: &str, body: &str) {
        let path = self.path(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `status` succeeds; the other answers follow the files left in the
    /// directory: `quit` appears with the first Enter, `exec` with `pane run`.
    fn herdr_script(&self) -> String {
        format!(
            r#"#!/bin/sh
D="{dir}"
echo "$*" >> "$D/calls"
case "$1 $2" in
  "status "*) exit 0 ;;
  "api snapshot") cat "$D/snapshot.json" ;;
  "agent get")
    if [ -e "$D/exec" ]; then cat "$D/agent-get-sleeping.json"; else cat "$D/agent-get.json"; fi ;;
  "pane get")
    if [ -e "$D/quit" ]; then cat "$D/pane-get-quit.json"; else cat "$D/pane-get-current.json"; fi ;;
  "pane process-info")
    if [ -e "$D/quit" ]; then cat "$D/process-info-quit.json"; else cat "$D/process-info-current.json"; fi ;;
  "pane read") printf 'first\n\nlast preview line  \n\n' ;;
  "pane send-keys") touch "$D/quit"; echo '{{"id":"x","result":{{"type":"ok"}}}}' ;;
  "pane run") touch "$D/exec"; echo '{{"id":"x","result":{{"type":"ok"}}}}' ;;
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

    fn journal(&self) -> String {
        fs::read_to_string(self.state().join("herdr-sleep.log")).unwrap_or_default()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-sleep"));
        command
            .args(args)
            .env("HERDR_BIN_PATH", self.path("herdr"))
            .env("HERDR_SOCKET_PATH", self.path("socket"))
            .env("HERDR_SLEEP_STATE_DIR", self.state())
            .env("HERDR_SLEEP_CONFIG_DIR", self.path("config"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        output
    }

    fn is_locked(&self) -> bool {
        WatcherLock::try_acquire(&self.state()).unwrap().is_none()
    }

    /// Wait until a Watcher holds the lock and has written its pid.
    fn wait_for_watcher(&self) -> u32 {
        wait_for("a watcher to take the lock", || {
            self.is_locked()
                .then(|| recorded_pid(&self.state()))
                .flatten()
        })
    }

    /// Pids of the Watchers running against this state directory.
    #[cfg(target_os = "linux")]
    fn watcher_pids(&self) -> Vec<u32> {
        let needle = format!("HERDR_SLEEP_STATE_DIR={}", self.state().display());
        let mut pids = Vec::new();
        for entry in fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let cmdline = fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let environ = fs::read(entry.path().join("environ")).unwrap_or_default();
            let is_watch = cmdline
                .split(|b| *b == 0)
                .collect::<Vec<_>>()
                .ends_with(&[b"watch".as_slice(), b"".as_slice()]);
            if is_watch
                && environ
                    .split(|b| *b == 0)
                    .any(|var| var == needle.as_bytes())
            {
                pids.push(pid);
            }
        }
        pids
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let mut pids: Vec<u32> = recorded_pid(&self.state()).into_iter().collect();
        #[cfg(target_os = "linux")]
        pids.extend(self.watcher_pids());
        for pid in pids {
            // SAFETY: plain `kill(2)` on a pid this test started.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn kill(pid: u32, signal: libc::c_int) {
    // SAFETY: plain `kill(2)` on a pid this test started.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

#[test]
fn two_detached_starts_leave_one_watcher_and_a_killed_one_is_replaced() {
    let sandbox = Sandbox::new("single");
    // Both invocations race for the lock; the loser exits at once.
    let (a, b) = (
        sandbox.command(&["watch", "--detach"]).output().unwrap(),
        sandbox.command(&["watch", "--detach"]).output().unwrap(),
    );
    assert!(a.status.success() && b.status.success());
    let first = sandbox.wait_for_watcher();
    std::thread::sleep(Duration::from_millis(500));
    #[cfg(target_os = "linux")]
    assert_eq!(sandbox.watcher_pids(), [first]);
    assert!(sandbox.journal().contains("WATCH-START"));

    let output = sandbox.run(&["ensure-watcher"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("already running"));

    // A crash: no cleanup, the kernel releases the lock.
    kill(first, libc::SIGKILL);
    wait_for("the lock to be released", || {
        (!sandbox.is_locked()).then_some(())
    });
    let output = sandbox.run(&["ensure-watcher"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("started"));
    let second = wait_for("the new watcher", || {
        sandbox
            .is_locked()
            .then(|| recorded_pid(&sandbox.state()))
            .flatten()
            .filter(|pid| *pid != first)
    });
    #[cfg(target_os = "linux")]
    assert_eq!(sandbox.watcher_pids(), [second]);
    assert_ne!(first, second);
}

#[test]
fn the_watcher_exits_when_the_socket_vanishes() {
    let sandbox = Sandbox::new("socket");
    sandbox.run(&["watch", "--detach"]);
    sandbox.wait_for_watcher();
    fs::remove_file(sandbox.path("socket")).unwrap();
    wait_for("the watcher to exit", || {
        (!sandbox.is_locked()).then_some(())
    });
    assert!(sandbox.journal().contains("WATCH-EXIT socket-gone"));
}

#[test]
fn a_sigterm_stops_the_watcher_cleanly() {
    let sandbox = Sandbox::new("term");
    sandbox.run(&["watch", "--detach"]);
    let pid = sandbox.wait_for_watcher();
    kill(pid, libc::SIGTERM);
    wait_for("the watcher to exit", || {
        (!sandbox.is_locked()).then_some(())
    });
    assert!(sandbox.journal().contains("WATCH-EXIT signal"));
}

#[test]
fn a_foreground_watch_beside_a_running_one_exits_silently() {
    let sandbox = Sandbox::new("second");
    sandbox.run(&["watch", "--detach"]);
    let pid = sandbox.wait_for_watcher();
    let output = sandbox.run(&["watch"]);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert_eq!(recorded_pid(&sandbox.state()), Some(pid));
}

#[test]
fn sweep_reports_what_it_considered_as_json() {
    let sandbox = Sandbox::new("sweep-json");
    let output = sandbox.run(&["sweep", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["considered"], 1);
    assert_eq!(report["error"], serde_json::Value::Null);
    // The real guards read the live process table, which a scripted Herdr
    // cannot fake: the agent is due and its sleep is decided by them.
    let decided = ["slept", "refused", "aborted", "dry_run"]
        .iter()
        .map(|key| report[key].as_array().unwrap().len())
        .sum::<usize>();
    assert_eq!(decided, 1, "{report}");
    assert!(sandbox.path("state/sweep.json").exists());
}

#[test]
fn a_due_agent_is_put_to_sleep() {
    let sandbox = Sandbox::new("sweep-sleep");
    let client = HerdrClient::with_binary(sandbox.path("herdr"));
    let env = LiveEnv::new(client)
        .with_state_dir(sandbox.state())
        .with_placeholder(sandbox.path("herdr-sleep-ph"));
    let config = Config::from_toml_str("sleep_after_minutes = 0").unwrap();
    let mut state = SweepState::default();

    // Forced, because the guards read the live process table.
    let report = sweep_with_mode(&env, &config, &mut state, SleepMode::Force);
    assert_eq!(report.considered, 1);
    assert_eq!(report.slept, ["wC:p1"], "{report:?}");
    assert!(state.agents.is_empty());
    let calls = sandbox.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("pane send-text wC:p1 /exit"))
    );
    assert!(calls.iter().any(|c| c.starts_with("pane run wC:p1")));
}

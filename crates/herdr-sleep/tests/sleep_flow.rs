//! The Sleep operation through the real [`LiveEnv`] and a scripted `herdr`
//! shell script that answers with the JSON of recorded Herdr output and
//! remembers every call.
//!
//! The guards read the live process table, which cannot be scripted, so the
//! flow runs in `Force` mode (every guard skipped). What the guards decide is
//! covered by the unit tests of `guards`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use herdr_sleep::config::Config;
use herdr_sleep::guards::SleepMode;
use herdr_sleep::herdr::HerdrClient;
use herdr_sleep::ops::{LiveEnv, SleepOutcome, sleep_agent};

const PANE: &str = "wC:p1";

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
            "herdr-sleep-flow-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let sandbox = Sandbox { dir };
        sandbox.install_fixtures();
        sandbox.write_executable("herdr", &sandbox.herdr_script());
        sandbox.write_executable("herdr-sleep-ph", "#!/bin/sh\nexit 0\n");
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
        for name in [
            "agent-get.json",
            "pane-get-current.json",
            "process-info-current.json",
        ] {
            fs::copy(fixtures.join(name), self.path(name)).unwrap();
        }
        // The same panes once the agent quit and once the Placeholder runs.
        let agent = fs::read_to_string(self.path("agent-get.json")).unwrap();
        fs::write(
            self.path("agent-get-sleeping.json"),
            agent.replace(r#""agent":"claude""#, r#""agent":"sleeping-claude""#),
        )
        .unwrap();
        fs::write(
            self.path("pane-get-quit.json"),
            r#"{"id":"cli:pane:get","result":{"pane":{"agent_status":"unknown","cwd":"/home/user/herdr-sleep","focused":false,"pane_id":"wC:p1","tab_id":"wC:t1","workspace_id":"wC"},"type":"pane_info"}}"#,
        )
        .unwrap();
        fs::write(
            self.path("process-info-quit.json"),
            r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":184186,"foreground_processes":[],"pane_id":"wC:p1","shell_pid":184186},"type":"pane_process_info"}}"#,
        )
        .unwrap();
    }

    /// A `herdr` whose answers follow the files it leaves in its directory:
    /// `quit` appears with the first Enter, `exec` with `pane run`.
    fn herdr_script(&self) -> String {
        format!(
            r#"#!/bin/sh
D="{dir}"
echo "$*" >> "$D/calls"
case "$1 $2" in
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

    fn env(&self) -> (HerdrClient, LiveEnv) {
        let client = HerdrClient::with_binary(self.path("herdr"));
        let env = LiveEnv::new(client.clone())
            .with_state_dir(self.path("state"))
            .with_placeholder(self.path("herdr-sleep-ph"));
        (client, env)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn forced_sleep_drives_herdr_through_the_whole_sequence() {
    let sandbox = Sandbox::new("full");
    let (client, env) = sandbox.env();
    let agent = client.agent_get(PANE).unwrap();
    let config = Config::default();

    let outcome = sleep_agent(&env, &config, &agent, SleepMode::Force, None);
    assert!(
        matches!(outcome, SleepOutcome::Asleep { .. }),
        "{outcome:?}"
    );

    let calls = sandbox.calls();
    let index = |prefix: &str| {
        calls
            .iter()
            .position(|c| c.starts_with(prefix))
            .unwrap_or_else(|| panic!("no call starting with '{prefix}' in {calls:?}"))
    };
    assert!(index("pane read wC:p1") < index("pane send-text wC:p1 /exit"));
    assert!(index("pane send-text wC:p1 /exit") < index("pane send-keys wC:p1 enter"));
    assert!(index("pane send-keys wC:p1 enter") < index("pane run wC:p1"));
    let run = &calls[index("pane run wC:p1")];
    assert!(run.contains(" exec "), "{run}");
    assert!(
        run.contains("herdr-sleep-ph --pane wC:p1 --kind claude"),
        "{run}"
    );
    assert!(
        run.contains("--session 11111111-2222-4333-8444-555555555555"),
        "{run}"
    );
    assert!(run.contains(&format!("--herdr-bin {}", sandbox.path("herdr").display())));
    assert!(run.contains("--preview "), "{run}");
    assert!(
        run.ends_with("-- claude --resume 11111111-2222-4333-8444-555555555555"),
        "{run}"
    );

    let preview = fs::read_to_string(sandbox.path("state/preview-wC_p1.txt")).unwrap();
    assert_eq!(preview, "first\n\nlast preview line");
    let journal = fs::read_to_string(sandbox.path("state/herdr-sleep.log")).unwrap();
    let events: Vec<&str> = journal
        .lines()
        .map(|l| l.split(' ').nth(1).unwrap())
        .collect();
    assert_eq!(events, ["SLEEP", "ASLEEP"]);
}

#[test]
fn missing_placeholder_leaves_the_agent_untouched() {
    let sandbox = Sandbox::new("no-ph");
    fs::remove_file(sandbox.path("herdr-sleep-ph")).unwrap();
    let (client, env) = sandbox.env();
    let agent = client.agent_get(PANE).unwrap();

    let outcome = sleep_agent(&env, &Config::default(), &agent, SleepMode::Force, None);
    assert!(matches!(outcome, SleepOutcome::Aborted(_)), "{outcome:?}");
    let calls = sandbox.calls();
    assert!(
        calls
            .iter()
            .all(|c| !c.contains("send-text") && !c.contains("send-keys")),
        "{calls:?}"
    );
    assert!(!sandbox.path("quit").exists());
}

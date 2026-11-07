//! The client against a real process: a fake `herdr` shell script answers
//! the way the real binary does.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use herdr_sleep::herdr::{HerdrClient, HerdrError};
use serde_json::json;

struct FakeBinary {
    dir: PathBuf,
}

impl FakeBinary {
    /// Write a `herdr` script whose body is `script` (after `#!/bin/sh`).
    fn new(name: &str, script: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("herdr-sleep-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("herdr");
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        FakeBinary { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("herdr")
    }

    fn client(&self) -> HerdrClient {
        HerdrClient::with_binary(self.path())
    }
}

impl Drop for FakeBinary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read_to_string(path).unwrap()
}

#[test]
fn json_result_on_stdout() {
    let fake = FakeBinary::new(
        "ok",
        r#"printf '%s\n' '{"id":"t","result":{"pane":{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent_status":"idle"}}}'"#,
    );
    let client = fake.client();
    let result = client.call(&["pane", "get", "w1:p1"]).unwrap();
    assert_eq!(result["pane"]["pane_id"], json!("w1:p1"));
    let pane = client.pane_get("w1:p1").unwrap();
    assert_eq!(pane.workspace_id, "w1");
}

#[test]
fn arguments_reach_the_binary() {
    let fake = FakeBinary::new(
        "argv",
        r#"eval "last=\${$#}"; printf '{"id":"t","result":{"argc":%d,"last":"%s"}}\n' "$#" "$last""#,
    );
    let result = fake
        .client()
        .call(&["pane", "send-text", "w1:p1", "hello world"])
        .unwrap();
    assert_eq!(result["argc"], json!(4));
    assert_eq!(result["last"], json!("hello world"));
}

#[test]
fn fixture_answer_feeds_typed_wrapper() {
    let script = format!(
        "cat <<'JSON'\n{}\nJSON",
        fixture("snapshot.json").trim_end()
    );
    let fake = FakeBinary::new("snapshot", &script);
    let snapshot = fake.client().snapshot().unwrap();
    assert_eq!(snapshot.agents.len(), 6);
    assert_eq!(snapshot.version.as_deref(), Some("0.9.5"));
}

#[test]
fn json_error_on_stdout() {
    let script = format!(
        "printf '%s\\n' '{}'",
        fixture("pane-get-missing.json").trim_end()
    );
    let fake = FakeBinary::new("stdout-error", &script);
    match fake.client().pane_get("nope:p9") {
        Err(HerdrError::Api { code, message }) => {
            assert_eq!(code, "pane_not_found");
            assert_eq!(message, "pane nope:p9 not found");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn json_error_on_stderr_with_exit_one() {
    let fake = FakeBinary::new(
        "stderr-error",
        r#"printf '%s\n' '{"error":{"code":"server_unavailable","message":"no server"}}' >&2; exit 1"#,
    );
    let err = fake.client().call(&["agent", "list"]).unwrap_err();
    assert_eq!(err.api_code(), Some("server_unavailable"));
}

#[test]
fn garbage_output_is_exec_error() {
    let fake = FakeBinary::new(
        "garbage",
        "echo 'herdr 0.9.5'; echo 'error: unexpected argument' >&2; exit 2",
    );
    match fake.client().call(&["--bogus"]) {
        Err(HerdrError::Exec { status, stderr }) => {
            assert_eq!(status, Some(2));
            assert_eq!(stderr, "error: unexpected argument");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn missing_binary_is_spawn_error() {
    let client = HerdrClient::with_binary("/definitely/not/here/herdr");
    match client.call(&["agent", "list"]) {
        Err(HerdrError::Spawn { binary, source }) => {
            assert_eq!(binary, PathBuf::from("/definitely/not/here/herdr"));
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn slow_binary_times_out_and_is_killed() {
    let fake = FakeBinary::new("slow", "sleep 20; echo '{\"result\":{}}'");
    let client = fake.client().with_timeout(Duration::from_millis(300));
    let start = Instant::now();
    match client.call(&["api", "snapshot"]) {
        Err(HerdrError::Timeout { after }) => assert_eq!(after, Duration::from_millis(300)),
        other => panic!("unexpected: {other:?}"),
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the process was not killed promptly: {:?}",
        start.elapsed()
    );
}

#[test]
fn large_output_does_not_deadlock() {
    let fake = FakeBinary::new(
        "large",
        r#"i=0; while [ $i -lt 4000 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=$((i+1)); done"#,
    );
    let text = fake
        .client()
        .pane_read("w1:p1", 10, herdr_sleep::herdr::ReadFormat::Text)
        .unwrap();
    assert_eq!(text.len(), 4000 * 80);
}

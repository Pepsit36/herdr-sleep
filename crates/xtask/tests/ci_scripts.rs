//! The shell files of `scripts/` and `scripts/ci/`: syntax, and the CI helpers
//! run against a fake `curl` that records its arguments and answers with a
//! status taken from the environment. Nothing reaches a network.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn script(name: &str) -> PathBuf {
    repo_root().join("scripts/ci").join(name)
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "herdr-sleep-ci-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(root.join("tools")).unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        // Answers GETs with $FAKE_GET_STATUS and writes with $FAKE_WRITE_STATUS.
        executable(
            &root.join("tools/curl"),
            "#!/bin/sh\n\
             echo \"$*\" >> \"$CURL_LOG\"\n\
             case \"$*\" in\n\
             *--request\\ POST* | *--upload-file*) printf '%s' \"${FAKE_WRITE_STATUS:-201}\" ;;\n\
             *) printf '%s' \"${FAKE_GET_STATUS:-404}\" ;;\n\
             esac\n",
        );
        Sandbox { root }
    }

    fn dist(&self, names: &[&str]) {
        for name in names {
            fs::write(self.root.join("dist").join(name), "x").unwrap();
        }
    }

    fn run(&self, script_name: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("tools").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new("sh");
        command
            .arg(script(script_name))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", path)
            .env("CURL_LOG", self.root.join("curl.log"));
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn curl_calls(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("curl.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn every_shell_file_parses() {
    let mut files = Vec::new();
    for dir in ["scripts", "scripts/ci"] {
        for entry in fs::read_dir(repo_root().join(dir)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "sh") {
                files.push(path);
            }
        }
    }
    assert!(files.len() >= 5, "{files:?}");
    for file in files {
        let out = Command::new("sh").arg("-n").arg(&file).output().unwrap();
        assert!(
            out.status.success(),
            "{}: {}",
            file.display(),
            text(&out.stderr)
        );
    }
}

const RELEASE_ENV: [(&str, &str); 4] = [
    ("GITLAB_TOKEN", "glpat-secret"),
    ("CI_API_V4_URL", "https://gitlab.example/api/v4"),
    ("CI_PROJECT_ID", "42"),
    ("VERSION", "1.2.3"),
];

#[test]
fn require_env_names_the_first_missing_variable() {
    let sb = Sandbox::new();
    let ok = sb.run("require-env.sh", &["A", "B"], &[("A", "1"), ("B", "2")]);
    assert!(ok.status.success());
    let out = sb.run(
        "require-env.sh",
        &["GITLAB_TOKEN", "GITHUB_TOKEN"],
        &[("GITHUB_TOKEN", "x")],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(
        err.contains("GITLAB_TOKEN is not set") && err.contains("api and write_repository"),
        "{err}"
    );
    let out = sb.run(
        "require-env.sh",
        &["GITLAB_TOKEN", "GITHUB_TOKEN"],
        &[("GITLAB_TOKEN", "x"), ("GITHUB_TOKEN", "")],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("GITHUB_TOKEN is not set"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn gitlab_release_creates_the_release_with_links_to_the_github_assets() {
    let sb = Sandbox::new();
    sb.dist(&[
        "herdr-sleep-1.2.3-linux-x86_64.tar.gz",
        "herdr-sleep-1.2.3-linux-x86_64.tar.gz.sha256",
        "herdr-sleep-9.9.9-linux-x86_64.tar.gz",
    ]);
    fs::write(sb.root.join("notes.md"), "### Added\n\n- A thing.\n").unwrap();
    let out = sb.run("gitlab-release.sh", &["notes.md", "dist"], &RELEASE_ENV);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let calls = sb.curl_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].contains("projects/42/releases/v1.2.3"),
        "{}",
        calls[0]
    );
    let post = &calls[1];
    assert!(
        post.contains("--request POST") && post.ends_with("projects/42/releases"),
        "{post}"
    );
    assert!(
        post.contains("tag_name=v1.2.3") && post.contains("description@notes.md"),
        "{post}"
    );
    let base = "https://github.com/Pepsit36/herdr-sleep/releases/download/v1.2.3";
    assert!(
        post.contains(&format!(
            "assets[links][][url]={base}/herdr-sleep-1.2.3-linux-x86_64.tar.gz"
        )),
        "{post}"
    );
    assert!(
        post.contains(&format!(
            "{base}/herdr-sleep-1.2.3-linux-x86_64.tar.gz.sha256"
        )),
        "{post}"
    );
    assert!(!post.contains("9.9.9"), "{post}");
}

#[test]
fn gitlab_release_leaves_an_existing_release_alone() {
    let sb = Sandbox::new();
    fs::write(sb.root.join("notes.md"), "x").unwrap();
    let mut env = RELEASE_ENV.to_vec();
    env.push(("FAKE_GET_STATUS", "200"));
    let out = sb.run("gitlab-release.sh", &["notes.md", "dist"], &env);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("already exists"));
    assert_eq!(sb.curl_calls().len(), 1);
}

#[test]
fn gitlab_release_fails_on_the_missing_token_before_any_call() {
    let sb = Sandbox::new();
    fs::write(sb.root.join("notes.md"), "x").unwrap();
    let env: Vec<_> = RELEASE_ENV
        .iter()
        .filter(|(k, _)| *k != "GITLAB_TOKEN")
        .copied()
        .collect();
    let out = sb.run("gitlab-release.sh", &["notes.md", "dist"], &env);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("GITLAB_TOKEN"),
        "{}",
        text(&out.stderr)
    );
    assert!(sb.curl_calls().is_empty());
}

#[test]
fn gitlab_release_fails_when_the_api_refuses() {
    let sb = Sandbox::new();
    fs::write(sb.root.join("notes.md"), "x").unwrap();
    let mut env = RELEASE_ENV.to_vec();
    env.push(("FAKE_WRITE_STATUS", "403"));
    let out = sb.run("gitlab-release.sh", &["notes.md", "dist"], &env);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("HTTP 403"));
}

const CANARY_ENV: [(&str, &str); 4] = [
    ("CI_JOB_TOKEN", "job-secret"),
    ("CI_API_V4_URL", "https://gitlab.example/api/v4"),
    ("CI_PROJECT_ID", "42"),
    ("VERSION", "0.2.0-pre.ab12cd3"),
];

#[test]
fn canary_upload_publishes_each_file_as_a_generic_package() {
    let sb = Sandbox::new();
    sb.dist(&[
        "herdr-sleep-0.2.0-pre.ab12cd3-linux-x86_64.tar.gz",
        "herdr-sleep-0.2.0-pre.ab12cd3-linux-x86_64.tar.gz.sha256",
        "VERSION",
    ]);
    let out = sb.run("canary-upload.sh", &["dist"], &CANARY_ENV);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let calls = sb.curl_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    let base =
        "https://gitlab.example/api/v4/projects/42/packages/generic/herdr-sleep/0.2.0-pre.ab12cd3";
    assert!(
        calls[0].contains("JOB-TOKEN: job-secret")
            && calls[0].ends_with(&format!(
                "{base}/herdr-sleep-0.2.0-pre.ab12cd3-linux-x86_64.tar.gz"
            )),
        "{}",
        calls[0]
    );
    assert!(calls.iter().all(|call| !call.contains("github")));
}

#[test]
fn canary_upload_fails_on_an_empty_dist_and_on_a_refused_upload() {
    let sb = Sandbox::new();
    let empty = sb.run("canary-upload.sh", &["dist"], &CANARY_ENV);
    assert_eq!(empty.status.code(), Some(1));
    assert!(text(&empty.stderr).contains("no archive"));
    sb.dist(&["herdr-sleep-0.2.0-pre.ab12cd3-linux-x86_64.tar.gz"]);
    let mut env = CANARY_ENV.to_vec();
    env.push(("FAKE_WRITE_STATUS", "401"));
    let refused = sb.run("canary-upload.sh", &["dist"], &env);
    assert_eq!(refused.status.code(), Some(1));
    assert!(text(&refused.stderr).contains("HTTP 401"));
}

/// A `target/<triple>/release` with two fake binaries and a `file` that describes them.
fn smoke_setup(sb: &Sandbox, triple: &str, described_as: &str) {
    let release = sb.root.join("target").join(triple).join("release");
    fs::create_dir_all(&release).unwrap();
    for name in ["herdr-sleep", "herdr-sleep-ph"] {
        executable(&release.join(name), "#!/bin/sh\necho herdr-sleep 1.2.3\n");
    }
    executable(
        &sb.root.join("tools/file"),
        &format!("#!/bin/sh\necho \"$1: {described_as}\"\n"),
    );
}

#[test]
fn smoke_checks_the_machine_the_linkage_and_the_version() {
    let linux = "ELF 64-bit LSB executable, x86-64, version 1 (SYSV), statically linked, stripped";
    let sb = Sandbox::new();
    smoke_setup(&sb, "x86_64-unknown-linux-musl", linux);
    assert!(
        sb.run("smoke.sh", &["x86_64-unknown-linux-musl", "1.2.3"], &[])
            .status
            .success()
    );
    let wrong_version = sb.run("smoke.sh", &["x86_64-unknown-linux-musl", "9.9.9"], &[]);
    assert_eq!(wrong_version.status.code(), Some(1));

    let dynamic = Sandbox::new();
    smoke_setup(
        &dynamic,
        "x86_64-unknown-linux-musl",
        "ELF 64-bit LSB executable, x86-64, dynamically linked",
    );
    let out = dynamic.run("smoke.sh", &["x86_64-unknown-linux-musl", "1.2.3"], &[]);
    assert!(
        text(&out.stderr).contains("not static"),
        "{}",
        text(&out.stderr)
    );

    let wrong_machine = Sandbox::new();
    smoke_setup(&wrong_machine, "aarch64-unknown-linux-musl", linux);
    assert_eq!(
        wrong_machine
            .run("smoke.sh", &["aarch64-unknown-linux-musl", "1.2.3"], &[])
            .status
            .code(),
        Some(1)
    );

    let mac = Sandbox::new();
    smoke_setup(
        &mac,
        "aarch64-apple-darwin",
        "Mach-O 64-bit arm64 executable, flags:<NOUNDEFS>",
    );
    assert!(
        mac.run("smoke.sh", &["aarch64-apple-darwin", "1.2.3"], &[])
            .status
            .success()
    );
    assert_eq!(
        mac.run("smoke.sh", &["riscv64-unknown-linux-musl", "1.2.3"], &[])
            .status
            .code(),
        Some(2)
    );
}

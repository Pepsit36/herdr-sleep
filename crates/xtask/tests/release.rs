//! End-to-end tests of `cargo xtask release` against throwaway repositories
//! (with a bare `origin`) and a stub GitHub API served on a local socket.
//! Nothing here reaches the network or a real remote.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

static COUNTER: AtomicU32 = AtomicU32::new(0);

const CARGO_TOML: &str =
    "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.0.0\"\nedition = \"2024\"\n";
const MANIFEST: &str = "id = \"x.y\"\nversion = \"0.0.0\"\n\n[[build]]\ncommand = [\"sh\"]\n";
const LOCK: &str = "version = 4\n\n[[package]]\nname = \"herdr-sleep\"\nversion = \"0.0.0\"\n\n[[package]]\nname = \"herdr-sleep-ph\"\nversion = \"0.0.0\"\n\n[[package]]\nname = \"libc\"\nversion = \"0.2.1\"\n\n[[package]]\nname = \"xtask\"\nversion = \"0.0.0\"\n";
const CHANGELOG: &str = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n\n<!-- stamped -->\n\n### Added\n\n- Thing \"quoted\".\n";

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

struct Repo {
    root: PathBuf,
    work: PathBuf,
    origin: PathBuf,
    /// The merge commit the pipeline runs on.
    merge: String,
}

impl Repo {
    /// A repository whose `main` holds a `feat:` commit and its stamped 0.1.0.
    fn new() -> Repo {
        let root = std::env::temp_dir().join(format!(
            "xtask-release-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let (work, origin) = (root.join("work"), root.join("origin.git"));
        fs::create_dir_all(&work).unwrap();
        git(&root, &["init", "-q", "--bare", "origin.git"]);
        git(&work, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Test Author"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
        ] {
            git(&work, &["config", key, value]);
        }
        git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        let mut repo = Repo {
            root,
            work,
            origin,
            merge: String::new(),
        };
        for (name, text) in [
            ("Cargo.toml", CARGO_TOML),
            ("herdr-plugin.toml", MANIFEST),
            ("Cargo.lock", LOCK),
        ] {
            repo.write(name, text);
        }
        repo.commit("chore: initial commit");
        repo.write("feature.txt", "feature\n");
        repo.commit("feat(thing): add a thing");
        repo.write("CHANGELOG.md", CHANGELOG);
        repo.commit("docs(changelog): stamp 0.1.0");
        git(&repo.work, &["push", "-q", "origin", "main:main"]);
        repo.merge = repo.head();
        repo
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.work.join(name), text).unwrap();
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.work.join(name)).unwrap()
    }

    fn commit(&self, message: &str) {
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", message]);
    }

    fn head(&self) -> String {
        git(&self.work, &["rev-parse", "HEAD"])
    }

    fn origin(&self, args: &[&str]) -> String {
        git(&self.origin, args)
    }

    /// A fresh checkout of the pipeline's commit: what a retried job starts from.
    fn reset_to_merge(&self) {
        git(&self.work, &["reset", "-q", "--hard", &self.merge]);
        git(&self.work, &["clean", "-q", "-fd"]);
    }

    fn xtask(&self, args: &[&str]) -> Run {
        self.xtask_env(args, &[])
    }

    fn xtask_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .args(args)
            .current_dir(&self.work)
            .env_remove("RESUME_RELEASE")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GITHUB_REPOSITORY")
            .env_remove("GITHUB_API_URL")
            .env_remove("GITHUB_UPLOAD_URL")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE");
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().expect("xtask runs");
        Run {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn prepare_applied(&self) {
        let run = self.xtask(&["release", "prepare", "--apply"]);
        assert_eq!(run.code, 0, "{}", run.err);
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Puts `v0.1.0` on a commit that is not in the history of `main`.
fn tag_elsewhere(repo: &Repo) {
    git(&repo.work, &["stash", "-q", "--include-untracked"]);
    git(&repo.work, &["checkout", "-q", "-b", "side", "HEAD~2"]);
    repo.write("side.txt", "x\n");
    repo.commit("chore: side");
    git(&repo.work, &["tag", "v0.1.0"]);
    git(&repo.work, &["checkout", "-q", "main"]);
    let _ = Command::new("git")
        .args(["stash", "pop", "-q"])
        .current_dir(&repo.work)
        .output();
}

// ------------------------------------------------------------------ prepare

#[test]
fn prepare_reports_the_version_and_changes_nothing_without_apply() {
    let repo = Repo::new();
    let run = repo.xtask(&["release", "prepare"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out, "RELEASE=1\nVERSION=0.1.0\n");
    assert_eq!(repo.read("Cargo.toml"), CARGO_TOML);
    assert_eq!(git(&repo.work, &["status", "--porcelain"]), "");
}

#[test]
fn prepare_apply_rewrites_the_three_files_and_is_idempotent() {
    let repo = Repo::new();
    repo.prepare_applied();
    assert_eq!(
        repo.read("Cargo.toml"),
        CARGO_TOML.replace("0.0.0", "0.1.0")
    );
    assert_eq!(
        repo.read("herdr-plugin.toml"),
        MANIFEST.replace("0.0.0", "0.1.0")
    );
    assert_eq!(repo.read("Cargo.lock"), LOCK.replace("0.0.0", "0.1.0"));
    let changed = git(&repo.work, &["status", "--porcelain"]);
    assert_eq!(changed.lines().count(), 3, "{changed}");
    repo.prepare_applied();
    assert_eq!(repo.read("Cargo.lock"), LOCK.replace("0.0.0", "0.1.0"));
}

#[test]
fn prepare_dry_run_writes_nothing() {
    let repo = Repo::new();
    let run = repo.xtask(&["release", "--dry-run", "prepare", "--apply"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.err.contains("would set the version"), "{}", run.err);
    assert_eq!(git(&repo.work, &["status", "--porcelain"]), "");
}

#[test]
fn prepare_has_nothing_to_release_after_a_chore() {
    let repo = Repo::new();
    git(&repo.work, &["tag", "v0.1.0"]);
    repo.write("note.txt", "x\n");
    repo.commit("chore: tidy");
    let run = repo.xtask(&["release", "prepare", "--apply"]);
    assert_eq!(run.code, 3, "{}", run.err);
    assert_eq!(run.out, "RELEASE=0\n");
    assert!(run.err.contains("nothing to release"), "{}", run.err);
}

#[test]
fn prepare_refuses_a_changelog_that_names_another_version() {
    let repo = Repo::new();
    repo.write("CHANGELOG.md", &CHANGELOG.replace("0.1.0", "0.2.0"));
    repo.commit("docs(changelog): wrong stamp");
    let run = repo.xtask(&["release", "prepare", "--apply"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("top section of CHANGELOG.md is 0.2.0"),
        "{}",
        run.err
    );
    assert_eq!(repo.read("Cargo.toml"), CARGO_TOML);
}

#[test]
fn prepare_refuses_a_tag_on_another_commit() {
    let repo = Repo::new();
    tag_elsewhere(&repo);
    let run = repo.xtask(&["release", "prepare", "--apply"]);
    assert_eq!(run.code, 1, "{}", run.out);
    assert!(run.err.contains("tag v0.1.0 already exists"), "{}", run.err);
    assert_eq!(repo.read("Cargo.toml"), CARGO_TOML);
}

#[test]
fn set_version_accepts_a_prerelease_and_refuses_junk() {
    let repo = Repo::new();
    let run = repo.xtask(&["release", "set-version", "0.2.0-pre.ab12cd3"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        repo.read("herdr-plugin.toml")
            .contains("version = \"0.2.0-pre.ab12cd3\"")
    );
    assert!(
        repo.read("Cargo.lock")
            .contains("name = \"xtask\"\nversion = \"0.2.0-pre.ab12cd3\"")
    );
    assert_eq!(repo.xtask(&["release", "set-version", "v1"]).code, 2);
}

// -------------------------------------------------------------------- notes

#[test]
fn notes_print_the_section_without_the_marker() {
    let repo = Repo::new();
    let run = repo.xtask(&["release", "notes", "--version", "0.1.0"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out, "### Added\n\n- Thing \"quoted\".\n");
    assert_eq!(
        repo.xtask(&["release", "notes", "--version", "9.9.9"]).code,
        1
    );
}

// ------------------------------------------------------------------- commit

const SUBJECT: &str = "chore(release): 0.1.0 [skip ci]";

fn remote_tag(repo: &Repo) -> String {
    repo.origin(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "refs/tags/v0.1.0^{commit}",
    ])
}

fn commit_count(repo: &Repo) -> String {
    repo.origin(&["rev-list", "--count", "main"])
}

fn release_commit(repo: &Repo) {
    let run = repo.xtask(&["release", "commit"]);
    assert_eq!(run.code, 0, "{}", run.err);
}

#[test]
fn commit_dry_run_touches_nothing() {
    let repo = Repo::new();
    repo.prepare_applied();
    let run = repo.xtask(&["release", "--dry-run", "commit"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("would tag it v0.1.0"), "{}", run.out);
    assert_eq!(repo.head(), repo.merge);
    assert_eq!(repo.origin(&["tag", "--list"]), "");
}

#[test]
fn commit_tags_and_pushes_exactly_the_three_files() {
    let repo = Repo::new();
    git(&repo.work, &["config", "advice.detachedHead", "false"]);
    repo.write("stray.txt", "left over\n");
    repo.prepare_applied();
    release_commit(&repo);
    let commit = repo.head();
    assert_eq!(remote_tag(&repo), commit);
    assert_eq!(repo.origin(&["rev-parse", "main"]), commit);
    assert_eq!(git(&repo.work, &["log", "-1", "--format=%s"]), SUBJECT);
    let files = git(&repo.work, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files, "Cargo.lock\nCargo.toml\nherdr-plugin.toml");
    assert_eq!(git(&repo.work, &["rev-parse", "HEAD~1"]), repo.merge);
}

#[test]
fn commit_demands_the_prepared_files() {
    let repo = Repo::new();
    let run = repo.xtask(&["release", "commit"]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("release prepare --apply"), "{}", run.err);
}

#[test]
fn commit_rerun_after_a_complete_release_changes_nothing() {
    let repo = Repo::new();
    repo.prepare_applied();
    release_commit(&repo);
    let (commit, count) = (repo.head(), commit_count(&repo));
    repo.reset_to_merge();
    git(&repo.work, &["tag", "-d", "v0.1.0"]);
    repo.prepare_applied();
    release_commit(&repo);
    assert_eq!(remote_tag(&repo), commit);
    assert_eq!(repo.origin(&["rev-parse", "main"]), commit);
    assert_eq!(commit_count(&repo), count);
}

/// A finished release whose publication stopped: commit and tag exist, and
/// `main` moved on with a merge commit on top, as after a merged request.
fn released(repo: &Repo) {
    repo.prepare_applied();
    release_commit(repo);
    git(&repo.work, &["checkout", "-q", "-b", "topic", "HEAD~1"]);
    repo.write("topic.txt", "x\n");
    repo.commit("chore: topic");
    git(&repo.work, &["checkout", "-q", "main"]);
    git(
        &repo.work,
        &["merge", "-q", "--no-ff", "-m", "Merge topic", "topic"],
    );
}

const RESUME: &[(&str, &str)] = &[("RESUME_RELEASE", "1")];

#[test]
fn prepare_resumes_a_release_when_asked() {
    let repo = Repo::new();
    released(&repo);
    let run = repo.xtask_env(&["release", "prepare", "--apply"], RESUME);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out, "RELEASE=1\nVERSION=0.1.0\n");
    assert!(run.err.contains("resuming v0.1.0"), "{}", run.err);
    assert_eq!(git(&repo.work, &["status", "--porcelain"]), "");
}

#[test]
fn prepare_has_nothing_to_release_after_a_release_without_the_variable() {
    let repo = Repo::new();
    released(&repo);
    let run = repo.xtask(&["release", "prepare", "--apply"]);
    assert_eq!(run.code, 3, "{}", run.err);
    assert_eq!(run.out, "RELEASE=0\n");
}

#[test]
fn prepare_refuses_to_resume_when_a_publishable_commit_follows() {
    let repo = Repo::new();
    released(&repo);
    repo.write("fix.txt", "x\n");
    repo.commit("fix(thing): repair a thing");
    let run = repo.xtask_env(&["release", "prepare"], RESUME);
    assert_eq!(run.code, 1, "{}", run.out);
    assert!(run.err.contains("publish a new version"), "{}", run.err);
    assert_eq!(run.out, "");
}

#[test]
fn prepare_refuses_to_resume_a_tag_that_is_not_a_release_commit() {
    let repo = Repo::new();
    git(&repo.work, &["tag", "v0.1.0"]);
    let run = repo.xtask_env(&["release", "prepare"], RESUME);
    assert_eq!(run.code, 1, "{}", run.out);
    assert!(run.err.contains("RESUME_RELEASE=1"), "{}", run.err);
}

#[test]
fn prepare_releases_normally_when_a_publishable_commit_follows_without_the_variable() {
    let repo = Repo::new();
    released(&repo);
    repo.write("fix.txt", "x\n");
    repo.commit("fix(thing): repair a thing");
    let run = repo.xtask(&["release", "prepare"]);
    // The changelog still names 0.1.0: the normal path is taken and refuses it.
    assert_eq!(run.code, 1, "{}", run.out);
    assert!(run.err.contains("publish 0.1.1"), "{}", run.err);
}

#[test]
fn commit_is_a_no_op_when_resuming() {
    let repo = Repo::new();
    released(&repo);
    let (head, count, tag) = (repo.head(), commit_count(&repo), remote_tag(&repo));
    let run = repo.xtask_env(&["release", "commit"], RESUME);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        run.out.contains("already committed and tagged"),
        "{}",
        run.out
    );
    assert_eq!(repo.head(), head);
    assert_eq!(commit_count(&repo), count);
    assert_eq!(remote_tag(&repo), tag);
}

#[test]
fn commit_finishes_a_release_whose_branch_was_pushed_without_its_tag() {
    let repo = Repo::new();
    repo.prepare_applied();
    release_commit(&repo);
    let (commit, count) = (repo.head(), commit_count(&repo));
    git(&repo.origin, &["tag", "-d", "v0.1.0"]);
    repo.reset_to_merge();
    git(&repo.work, &["tag", "-d", "v0.1.0"]);
    repo.prepare_applied();
    release_commit(&repo);
    assert_eq!(remote_tag(&repo), commit);
    assert_eq!(commit_count(&repo), count);
}

#[test]
fn commit_finishes_a_release_that_only_exists_locally() {
    let repo = Repo::new();
    repo.prepare_applied();
    git(&repo.work, &["commit", "-q", "-am", SUBJECT]);
    git(&repo.work, &["tag", "v0.1.0"]);
    let commit = repo.head();
    // Back on the merge commit with the prepared files, the commit is reused.
    git(&repo.work, &["reset", "-q", "--hard", &repo.merge]);
    repo.prepare_applied();
    release_commit(&repo);
    assert_eq!(remote_tag(&repo), commit);
    assert_eq!(repo.origin(&["rev-parse", "main"]), commit);
}

#[test]
fn commit_refuses_to_publish_out_of_order() {
    let repo = Repo::new();
    repo.prepare_applied();
    // Someone else merged meanwhile.
    let other = repo.root.join("other");
    git(
        &repo.root,
        &[
            "clone",
            "-q",
            "-b",
            "main",
            repo.origin.to_str().unwrap(),
            "other",
        ],
    );
    git(&other, &["config", "user.name", "Other"]);
    git(&other, &["config", "user.email", "other@example.invalid"]);
    fs::write(other.join("later.txt"), "later\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-q", "-m", "feat: later"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let run = repo.xtask(&["release", "commit"]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("out of order"), "{}", run.err);
    assert_eq!(repo.head(), repo.merge);
    assert_eq!(repo.origin(&["tag", "--list"]), "");
}

#[test]
fn commit_refuses_to_move_a_tag() {
    let repo = Repo::new();
    repo.prepare_applied();
    tag_elsewhere(&repo);
    let run = repo.xtask(&["release", "commit"]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("refusing to move it"), "{}", run.err);
    assert_eq!(repo.origin(&["tag", "--list"]), "");
}

// ----------------------------------------------------------- publish-github

struct Stub {
    port: u16,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct State {
    /// Answers to the tag lookup that are still 404.
    tag_misses: u32,
    release_body: Option<String>,
    assets: Vec<(u64, String, usize)>,
    next_id: u64,
    /// Upload attempt numbers (from 1) that fail with HTTP 500.
    failing_uploads: Vec<u32>,
    upload_attempts: u32,
    requests: Vec<String>,
    unauthorised: u32,
}

impl State {
    fn release_json(&self) -> String {
        let assets: Vec<String> = self
            .assets
            .iter()
            .map(|(id, name, size)| format!("{{\"id\":{id},\"name\":\"{name}\",\"size\":{size}}}"))
            .collect();
        format!(
            "{{\"id\":1,\"tag_name\":\"v0.1.0\",\"assets\":[{}]}}",
            assets.join(",")
        )
    }

    fn count(&self, prefix: &str) -> usize {
        self.requests
            .iter()
            .filter(|r| r.starts_with(prefix))
            .count()
    }
}

fn answer(state: &Mutex<State>, method: &str, target: &str, body: &[u8]) -> (u16, String) {
    let mut s = state.lock().unwrap();
    s.requests.push(format!("{method} {target}"));
    let path = target.split('?').next().unwrap();
    match (method, path) {
        ("GET", "/repos/o/r/git/ref/tags/v0.1.0") if s.tag_misses > 0 => {
            s.tag_misses -= 1;
            (404, "{}".into())
        }
        ("GET", "/repos/o/r/git/ref/tags/v0.1.0") => (200, "{}".into()),
        ("GET", "/repos/o/r/releases/tags/v0.1.0") if s.release_body.is_some() => {
            (200, s.release_json())
        }
        ("GET", "/repos/o/r/releases/tags/v0.1.0") => (404, "{}".into()),
        ("POST", "/repos/o/r/releases") => {
            s.release_body = Some(String::from_utf8_lossy(body).into_owned());
            (201, s.release_json())
        }
        ("POST", "/up/repos/o/r/releases/1/assets") => {
            s.upload_attempts += 1;
            if s.failing_uploads.contains(&s.upload_attempts) {
                return (500, "{\"message\":\"boom\"}".into());
            }
            let name = target.split("name=").nth(1).unwrap().to_string();
            if s.assets.iter().any(|(_, known, _)| *known == name) {
                return (422, "{\"message\":\"already_exists\"}".into());
            }
            s.next_id += 1;
            let id = s.next_id;
            s.assets.push((id, name, body.len()));
            (201, "{}".into())
        }
        ("DELETE", p) if p.starts_with("/repos/o/r/releases/assets/") => {
            let id: u64 = p.rsplit('/').next().unwrap().parse().unwrap();
            s.assets.retain(|(known, _, _)| *known != id);
            (204, String::new())
        }
        _ => (404, "{\"message\":\"no route\"}".into()),
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<State>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let (method, target) = (
        parts.next().unwrap().to_string(),
        parts.next().unwrap().to_string(),
    );
    let (mut length, mut authorised) = (0usize, false);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        if line == "Authorization: Bearer secret" {
            authorised = true;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let (status, text) = if authorised {
        answer(state, &method, &target, &body)
    } else {
        state.lock().unwrap().unauthorised += 1;
        (401, "{}".into())
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

impl Stub {
    fn start() -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, flag) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &shared),
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Stub {
            port,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("GITHUB_TOKEN", "secret".to_string()),
            ("GITHUB_REPOSITORY", "o/r".to_string()),
            ("GITHUB_API_URL", format!("http://127.0.0.1:{}", self.port)),
            (
                "GITHUB_UPLOAD_URL",
                format!("http://127.0.0.1:{}/up", self.port),
            ),
        ]
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn have_curl() -> bool {
    Command::new("curl").arg("--version").output().is_ok()
}

fn dist(repo: &Repo) -> PathBuf {
    let dir = repo.root.join("dist");
    fs::create_dir_all(&dir).unwrap();
    for (i, os) in ["linux-x86_64", "linux-aarch64"].iter().enumerate() {
        let name = format!("herdr-sleep-0.1.0-{os}.tar.gz");
        fs::write(dir.join(&name), vec![b'a'; 100 + i]).unwrap();
        fs::write(
            dir.join(format!("{name}.sha256")),
            format!("{}  {name}\n", "0".repeat(64)),
        )
        .unwrap();
    }
    fs::write(
        dir.join("herdr-sleep-0.2.0-linux-x86_64.tar.gz"),
        "other version",
    )
    .unwrap();
    dir
}

fn publish(repo: &Repo, stub: &Stub, extra: &[&str]) -> Run {
    let dist = dist(repo);
    let mut args = vec![
        "release",
        "publish-github",
        "--dist",
        dist.to_str().unwrap(),
        "--poll-seconds",
        "0",
    ];
    args.extend(extra);
    let env = stub.env();
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    repo.xtask_env(&args, &pairs)
}

#[test]
fn publish_creates_the_release_and_attaches_every_file_once() {
    if !have_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    let (repo, stub) = (Repo::new(), Stub::start());
    stub.with(|s| s.tag_misses = 2);
    repo.reset_to_merge();
    let run = publish(&repo, &stub, &["--version", "0.1.0"]);
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    stub.with(|s| {
        assert_eq!(s.unauthorised, 0);
        assert_eq!(s.assets.len(), 4);
        assert_eq!(s.count("GET /repos/o/r/git/ref/tags/v0.1.0"), 3);
        assert_eq!(s.count("POST /repos/o/r/releases"), 1);
        let body = s.release_body.clone().unwrap();
        assert!(body.contains("\"tag_name\":\"v0.1.0\""), "{body}");
        assert!(body.contains("Thing \\\"quoted\\\"."), "{body}");
    });
    // A second run finds everything in place.
    let again = publish(&repo, &stub, &["--version", "0.1.0"]);
    assert_eq!(again.code, 0, "{}{}", again.out, again.err);
    stub.with(|s| {
        assert_eq!(s.count("POST /repos/o/r/releases"), 1);
        assert_eq!(s.count("POST /up"), 4);
        assert_eq!(s.assets.len(), 4);
    });
}

#[test]
fn publish_resumes_after_a_failure_halfway_through_the_assets() {
    if !have_curl() {
        return;
    }
    let (repo, stub) = (Repo::new(), Stub::start());
    stub.with(|s| s.failing_uploads = vec![3]);
    let first = publish(&repo, &stub, &["--version", "0.1.0"]);
    assert_eq!(first.code, 1, "{}", first.out);
    assert!(first.err.contains("HTTP 500"), "{}", first.err);
    stub.with(|s| assert_eq!(s.assets.len(), 2));
    let second = publish(&repo, &stub, &["--version", "0.1.0"]);
    assert_eq!(second.code, 0, "{}{}", second.out, second.err);
    stub.with(|s| {
        assert_eq!(s.assets.len(), 4);
        assert_eq!(s.count("POST /repos/o/r/releases"), 1);
        assert_eq!(s.upload_attempts, 3 + 2);
    });
}

#[test]
fn publish_replaces_an_asset_attached_with_another_size() {
    if !have_curl() {
        return;
    }
    let (repo, stub) = (Repo::new(), Stub::start());
    stub.with(|s| {
        s.release_body = Some("{}".into());
        s.next_id = 10;
        s.assets
            .push((10, "herdr-sleep-0.1.0-linux-x86_64.tar.gz".into(), 7));
    });
    let run = publish(&repo, &stub, &["--version", "0.1.0"]);
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    stub.with(|s| {
        assert_eq!(s.assets.len(), 4);
        assert_eq!(s.count("DELETE /repos/o/r/releases/assets/10"), 1);
        assert!(!s.requests.contains(&"POST /repos/o/r/releases".to_string()));
    });
}

#[test]
fn publish_names_the_missing_token_and_calls_nothing() {
    if !have_curl() {
        return;
    }
    let (repo, stub) = (Repo::new(), Stub::start());
    let dist = dist(&repo);
    let run = repo.xtask(&[
        "release",
        "publish-github",
        "--dist",
        dist.to_str().unwrap(),
        "--version",
        "0.1.0",
    ]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("GITHUB_TOKEN"), "{}", run.err);
    stub.with(|s| assert!(s.requests.is_empty()));
}

#[test]
fn publish_gives_up_when_the_tag_never_arrives() {
    if !have_curl() {
        return;
    }
    let (repo, stub) = (Repo::new(), Stub::start());
    stub.with(|s| s.tag_misses = u32::MAX);
    let run = publish(&repo, &stub, &["--version", "0.1.0", "--wait-seconds", "0"]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("did not reach o/r"), "{}", run.err);
    stub.with(|s| assert!(s.release_body.is_none()));
}

#[test]
fn publish_dry_run_calls_nothing_and_needs_no_token() {
    let (repo, stub) = (Repo::new(), Stub::start());
    let dist = dist(&repo);
    let run = repo.xtask(&[
        "release",
        "--dry-run",
        "publish-github",
        "--dist",
        dist.to_str().unwrap(),
        "--version",
        "0.1.0",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out.matches("would attach").count(), 4, "{}", run.out);
    stub.with(|s| assert!(s.requests.is_empty()));
}

#[test]
fn publish_refuses_an_empty_dist() {
    let repo = Repo::new();
    let empty = repo.root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let run = repo.xtask(&[
        "release",
        "publish-github",
        "--dist",
        empty.to_str().unwrap(),
        "--version",
        "0.1.0",
    ]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("no herdr-sleep-0.1.0-"), "{}", run.err);
}

// ------------------------------------------------- the whole sequence, dry then real

#[test]
fn the_whole_sequence_runs_dry_then_for_real() {
    if !have_curl() {
        return;
    }
    let (repo, stub) = (Repo::new(), Stub::start());
    dist(&repo);
    // Dry run of every step: nothing moves anywhere.
    assert_eq!(
        repo.xtask(&["release", "--dry-run", "prepare", "--apply"])
            .code,
        0
    );
    repo.prepare_applied();
    assert_eq!(repo.xtask(&["release", "--dry-run", "commit"]).code, 0);
    assert_eq!(repo.head(), repo.merge);
    assert_eq!(repo.origin(&["tag", "--list"]), "");
    // The real thing against the stub.
    release_commit(&repo);
    let run = publish(&repo, &stub, &["--version", "0.1.0"]);
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert_eq!(remote_tag(&repo), repo.head());
    stub.with(|s| assert_eq!(s.assets.len(), 4));
}

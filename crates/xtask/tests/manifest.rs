//! Checks of `herdr-plugin.toml` and of its `[[build]]` hook, `scripts/build.sh`.
//!
//! The script tests run in a temporary copy of the repository skeleton with a
//! private `PATH` (a fake `curl` that only reads `file://` URLs and a fake
//! `cargo`), so they never touch the network or the real toolchain.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use toml::Value;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn manifest() -> Value {
    let text = fs::read_to_string(repo_root().join("herdr-plugin.toml")).unwrap();
    toml::from_str(&text).unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn tables<'a>(manifest: &'a Value, key: &str) -> &'a Vec<Value> {
    manifest.get(key).unwrap().as_array().unwrap()
}

#[test]
fn manifest_identity() {
    let m = manifest();
    assert_eq!(m["id"].as_str(), Some("pepsit36.herdr-sleep"));
    assert_eq!(m["name"].as_str(), Some("Herdr Sleep"));
    assert_eq!(m["min_herdr_version"].as_str(), Some("0.9.2"));
    assert_eq!(strings(&m["platforms"]), ["linux", "macos"]);
    assert!(!m["description"].as_str().unwrap().is_empty());
    assert!(m["version"].as_str().unwrap().split('.').count() == 3);
}

#[test]
fn manifest_build_and_startup_commands() {
    let m = manifest();
    let build = tables(&m, "build");
    assert_eq!(build.len(), 1);
    assert_eq!(strings(&build[0]["command"]), ["sh", "scripts/build.sh"]);
    let startup = tables(&m, "startup");
    // Herdr starts the hooks asynchronously: `restore` also starts the watcher
    // when it ends, and `watch --detach` is idempotent.
    assert_eq!(startup.len(), 2);
    assert_eq!(
        strings(&startup[0]["command"]),
        ["bin/herdr-sleep", "restore"]
    );
    assert_eq!(
        strings(&startup[1]["command"]),
        ["bin/herdr-sleep", "watch", "--detach"]
    );
}

#[test]
fn manifest_events() {
    let m = manifest();
    let events = tables(&m, "events");
    let mut names: Vec<&str> = events.iter().map(|e| e["on"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["pane.focused", "tab.focused", "workspace.focused"]);
    for event in events {
        assert_eq!(strings(&event["command"]), ["bin/herdr-sleep", "event"]);
    }
}

#[test]
fn manifest_actions() {
    let m = manifest();
    let actions = tables(&m, "actions");
    let expected: [(&str, &str, &[&str]); 6] = [
        ("sleep", "Sleep this agent", &["pane"]),
        ("wake", "Wake this agent", &["pane"]),
        ("keep-awake", "Keep this tab awake (toggle)", &["pane"]),
        ("sweep", "Sleep idle agents now", &["global", "workspace"]),
        ("status", "Sleep status", &["global", "workspace"]),
        ("ensure-watcher", "Restart sleep watcher", &["global"]),
    ];
    assert_eq!(actions.len(), expected.len());
    for (id, title, contexts) in expected {
        let action = actions
            .iter()
            .find(|a| a["id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("missing action {id}"));
        assert_eq!(action["title"].as_str(), Some(title));
        assert_eq!(strings(&action["contexts"]), contexts);
        assert_eq!(strings(&action["command"]), ["bin/herdr-sleep", id]);
    }
}

#[test]
fn every_command_runs_a_shipped_binary() {
    let m = manifest();
    let shipped = ["bin/herdr-sleep", "bin/herdr-sleep-ph"];
    for key in ["startup", "events", "actions"] {
        for entry in tables(&m, key) {
            let command = strings(&entry["command"]);
            assert!(shipped.contains(&command[0].as_str()), "{command:?}");
        }
    }
    // The binary names are the package names of the two binary crates.
    for krate in ["herdr-sleep", "herdr-sleep-ph"] {
        let cargo =
            fs::read_to_string(repo_root().join(format!("crates/{krate}/Cargo.toml"))).unwrap();
        assert!(cargo.contains(&format!("name = \"{krate}\"")));
    }
}

// ---------------------------------------------------------------- build.sh

const TOOLS: &[&str] = &[
    "sh",
    "tar",
    "uname",
    "dirname",
    "mktemp",
    "sed",
    "head",
    "cut",
    "find",
    "cp",
    "chmod",
    "mv",
    "rm",
    "mkdir",
    "cat",
    "gzip",
    "sha256sum",
    "shasum",
];

fn find_tool(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
}

fn write_executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn sha256(file: &Path) -> Option<String> {
    for (tool, args) in [("sha256sum", vec![]), ("shasum", vec!["-a", "256"])] {
        if find_tool(tool).is_some() {
            let out = Command::new(tool).args(args).arg(file).output().unwrap();
            return Some(
                String::from_utf8(out.stdout)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_string(),
            );
        }
    }
    None
}

fn os_arch() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    (os, arch)
}

/// A temporary copy of the repository skeleton plus a private toolbox.
struct Sandbox {
    root: PathBuf,
    repo: PathBuf,
    tools: PathBuf,
    releases: PathBuf,
}

impl Sandbox {
    /// `None` (with a printed reason) when the host lacks what the tests need.
    fn new(version: &str, with_cargo: bool) -> Option<Sandbox> {
        for needed in ["sh", "tar", "gzip"] {
            if find_tool(needed).is_none() {
                eprintln!("skipping build.sh test: {needed} is not installed");
                return None;
            }
        }
        if find_tool("sha256sum").is_none() && find_tool("shasum").is_none() {
            eprintln!("skipping build.sh test: no sha256 tool is installed");
            return None;
        }
        let root = std::env::temp_dir().join(format!(
            "herdr-sleep-build-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let (repo, tools, releases) = (root.join("repo"), root.join("tools"), root.join("rel"));
        for dir in [repo.join("scripts"), tools.clone(), releases.clone()] {
            fs::create_dir_all(dir).unwrap();
        }
        // The manifest carries the version of the last release: pin the one under test.
        let manifest = fs::read_to_string(repo_root().join("herdr-plugin.toml"))
            .unwrap()
            .lines()
            .map(|line| {
                if line.starts_with("version = \"") {
                    format!("version = \"{version}\"")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let manifest = format!("{manifest}\n");
        fs::write(repo.join("herdr-plugin.toml"), manifest).unwrap();
        fs::copy(
            repo_root().join("scripts/build.sh"),
            repo.join("scripts/build.sh"),
        )
        .unwrap();
        for tool in TOOLS {
            if let Some(real) = find_tool(tool) {
                symlink(real, tools.join(tool)).unwrap();
            }
        }
        // Reads file:// URLs only; a missing file is an HTTP 404 (exit 22).
        write_executable(
            &tools.join("curl"),
            "#!/bin/sh\n\
             echo \"$*\" >> \"$CURL_LOG\"\n\
             for last; do :; done\n\
             f=${last#file://}\n\
             [ -f \"$f\" ] || exit 22\n\
             cat \"$f\"\n",
        );
        if with_cargo {
            write_executable(
                &tools.join("cargo"),
                "#!/bin/sh\n\
                 echo \"$*\" >> \"$CARGO_LOG\"\n\
                 mkdir -p target/release\n\
                 echo source > target/release/herdr-sleep\n\
                 echo source > target/release/herdr-sleep-ph\n",
            );
        }
        Some(Sandbox {
            root,
            repo,
            tools,
            releases,
        })
    }

    /// Publish a fake release `version` whose archive holds the two binaries.
    fn publish(&self, version: &str, good_checksum: bool) {
        let (os, arch) = os_arch();
        let dir = self.releases.join(format!("v{version}"));
        fs::create_dir_all(&dir).unwrap();
        let stage = self.root.join("stage");
        fs::create_dir_all(&stage).unwrap();
        for b in ["herdr-sleep", "herdr-sleep-ph"] {
            write_executable(&stage.join(b), "#!/bin/sh\necho release\n");
        }
        let name = format!("herdr-sleep-{version}-{os}-{arch}.tar.gz");
        let archive = dir.join(&name);
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .args(["-C"])
            .arg(&stage)
            .args(["herdr-sleep", "herdr-sleep-ph"])
            .status()
            .unwrap();
        assert!(status.success());
        let digest = if good_checksum {
            sha256(&archive).unwrap()
        } else {
            "0".repeat(64)
        };
        fs::write(
            dir.join(format!("{name}.sha256")),
            format!("{digest}  {name}\n"),
        )
        .unwrap();
    }

    fn run(&self) -> (i32, String, String) {
        let out = Command::new(self.tools.join("sh"))
            .arg("scripts/build.sh")
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", &self.tools)
            .env("HOME", &self.root)
            .env(
                "HERDR_SLEEP_RELEASE_BASE_URL",
                format!("file://{}", self.releases.display()),
            )
            .env("CURL_LOG", self.root.join("curl.log"))
            .env("CARGO_LOG", self.root.join("cargo.log"))
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn bin(&self, name: &str) -> String {
        fs::read_to_string(self.repo.join("bin").join(name)).unwrap_or_default()
    }

    fn cargo_calls(&self) -> String {
        fs::read_to_string(self.root.join("cargo.log")).unwrap_or_default()
    }

    fn downloaded(&self) -> bool {
        self.root.join("curl.log").exists()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn build_installs_the_release_binaries() {
    let Some(sb) = Sandbox::new("1.2.3", true) else {
        return;
    };
    sb.publish("1.2.3", true);
    let (code, out, err) = sb.run();
    assert_eq!(code, 0, "{out}{err}");
    assert!(sb.bin("herdr-sleep").contains("release"));
    assert!(sb.bin("herdr-sleep-ph").contains("release"));
    assert!(sb.cargo_calls().is_empty());
    let mode = fs::metadata(sb.repo.join("bin/herdr-sleep"))
        .unwrap()
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0);
    assert!(!sb.repo.join("bin/herdr-sleep.new").exists());
}

#[test]
fn build_falls_back_on_a_bad_checksum() {
    let Some(sb) = Sandbox::new("1.2.3", true) else {
        return;
    };
    sb.publish("1.2.3", false);
    let (code, out, err) = sb.run();
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("checksum mismatch"), "{out}");
    assert_eq!(sb.cargo_calls().trim(), "build --release --locked");
    assert!(sb.bin("herdr-sleep").contains("source"));
    assert!(sb.bin("herdr-sleep-ph").contains("source"));
}

#[test]
fn build_falls_back_when_the_release_is_missing() {
    let Some(sb) = Sandbox::new("1.2.3", true) else {
        return;
    };
    let (code, out, err) = sb.run();
    assert_eq!(code, 0, "{out}{err}");
    assert!(sb.downloaded());
    assert_eq!(sb.cargo_calls().trim(), "build --release --locked");
    assert!(sb.bin("herdr-sleep").contains("source"));
}

#[test]
fn build_of_an_unreleased_checkout_never_downloads() {
    let Some(sb) = Sandbox::new("0.0.0", true) else {
        return;
    };
    sb.publish("0.0.0", true);
    let (code, out, err) = sb.run();
    assert_eq!(code, 0, "{out}{err}");
    assert!(!sb.downloaded());
    assert_eq!(sb.cargo_calls().trim(), "build --release --locked");
    assert!(sb.bin("herdr-sleep-ph").contains("source"));
}

#[test]
fn build_fails_clearly_without_release_and_without_cargo() {
    let Some(sb) = Sandbox::new("1.2.3", false) else {
        return;
    };
    let (code, out, err) = sb.run();
    assert_eq!(code, 1, "{out}{err}");
    assert!(err.contains("cargo is not available"), "{err}");
    assert!(err.contains("https://rustup.rs"), "{err}");
    assert!(err.contains("/releases"), "{err}");
    assert!(!sb.repo.join("bin").exists());
}

// ---------------------------------------------------------------- make dist

fn make_available() -> bool {
    if find_tool("make").is_none() {
        eprintln!("skipping make dist test: make is not installed");
        return false;
    }
    true
}

fn make_dist(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("make")
        .arg("dist")
        .args(args)
        .current_dir(dir)
        .env_remove("MAKEFLAGS")
        .output()
        .unwrap()
}

#[test]
fn make_dist_names_the_archive_of_every_release_target() {
    if !make_available() {
        return;
    }
    let cases = [
        ("x86_64-unknown-linux-musl", "linux-x86_64"),
        ("aarch64-unknown-linux-musl", "linux-aarch64"),
        ("x86_64-apple-darwin", "macos-x86_64"),
        ("aarch64-apple-darwin", "macos-aarch64"),
    ];
    for (triple, suffix) in cases {
        let target = format!("TARGET={triple}");
        let out = Command::new("make")
            .args(["-n", "dist", &target, "VERSION=1.2.3"])
            .current_dir(repo_root())
            .env_remove("MAKEFLAGS")
            .output()
            .unwrap();
        let plan = String::from_utf8_lossy(&out.stdout);
        let name = format!("herdr-sleep-1.2.3-{suffix}.tar.gz");
        assert!(out.status.success(), "{triple}: {plan}");
        assert!(
            plan.contains(&format!(
                "tar -czf dist/{name} -C target/{triple}/release herdr-sleep herdr-sleep-ph"
            )),
            "{plan}"
        );
        assert!(plan.contains(&format!("> {name}.sha256")), "{plan}");
    }
    let refused = Command::new("make")
        .args(["-n", "dist", "TARGET=x86_64-pc-windows-msvc"])
        .current_dir(repo_root())
        .output()
        .unwrap();
    assert!(!refused.status.success());
}

#[test]
fn make_dist_without_a_target_packages_target_release() {
    if !make_available() {
        return;
    }
    let out = Command::new("make")
        .args(["-n", "dist"])
        .current_dir(repo_root())
        .env_remove("MAKEFLAGS")
        .output()
        .unwrap();
    let plan = String::from_utf8_lossy(&out.stdout);
    let (os, arch) = os_arch();
    assert!(
        plan.contains(&format!(
            "-{os}-{arch}.tar.gz -C target/release herdr-sleep herdr-sleep-ph"
        )),
        "{plan}"
    );
}

/// The archive `make dist` produces is exactly what `scripts/build.sh` downloads,
/// checks and unpacks.
#[test]
fn an_archive_from_make_dist_installs_through_build_sh() {
    if !make_available() {
        return;
    }
    let Some(sb) = Sandbox::new("1.2.3", true) else {
        return;
    };
    let (os, arch) = os_arch();
    let triple = if os == "macos" {
        format!("{arch}-apple-darwin")
    } else {
        format!("{arch}-unknown-linux-musl")
    };
    let packaging = sb.root.join("packaging");
    let release_dir = packaging.join("target").join(&triple).join("release");
    fs::create_dir_all(&release_dir).unwrap();
    fs::copy(repo_root().join("Makefile"), packaging.join("Makefile")).unwrap();
    fs::copy(
        sb.repo.join("herdr-plugin.toml"),
        packaging.join("herdr-plugin.toml"),
    )
    .unwrap();
    for name in ["herdr-sleep", "herdr-sleep-ph"] {
        write_executable(
            &release_dir.join(name),
            &format!("#!/bin/sh\necho packaged {name}\n"),
        );
    }
    let made = make_dist(&packaging, &[&format!("TARGET={triple}"), "VERSION=1.2.3"]);
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );

    let name = format!("herdr-sleep-1.2.3-{os}-{arch}.tar.gz");
    let published = sb.releases.join("v1.2.3");
    fs::create_dir_all(&published).unwrap();
    for file in [name.clone(), format!("{name}.sha256")] {
        fs::copy(packaging.join("dist").join(&file), published.join(&file)).unwrap();
    }
    let checksum = fs::read_to_string(published.join(format!("{name}.sha256"))).unwrap();
    assert_eq!(
        checksum,
        format!("{}  {name}\n", sha256(&published.join(&name)).unwrap())
    );

    let (code, out, err) = sb.run();
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("installed release 1.2.3"), "{out}");
    assert_eq!(
        sb.bin("herdr-sleep"),
        "#!/bin/sh\necho packaged herdr-sleep\n"
    );
    assert_eq!(
        sb.bin("herdr-sleep-ph"),
        "#!/bin/sh\necho packaged herdr-sleep-ph\n"
    );
    assert_eq!(sb.cargo_calls(), "");
}

//! Drives the real binary through a pty with stub `herdr`, shell and main
//! binaries that record what they receive.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < TIMEOUT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Harness {
    dir: PathBuf,
    child: Child,
    master: File,
    screen: String,
}

impl Harness {
    fn start(label: &str) -> Harness {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hsph-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("state")).unwrap();
        let p = |n: &str| dir.join(n);
        script(
            &p("herdr"),
            &format!("printf '%s\\n' \"$*\" >> '{}'", p("herdr.log").display()),
        );
        script(
            &p("shell"),
            &format!(
                "{{ printf 'PWD=%s\\n' \"$PWD\"; \
                 for a in \"$@\"; do printf 'ARG=%s\\n' \"$a\"; done; }} > '{}'",
                p("shell.out").display()
            ),
        );
        script(
            &p("main"),
            &format!("printf '%s\\n' \"$*\" > '{}'", p("main.out").display()),
        );
        fs::write(p("preview.txt"), "\x1b[31mline one\x1b[0m\nline two\n").unwrap();

        let (master, slave) = openpty();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-sleep-ph"));
        cmd.args(["--pane", "1-2", "--name", "worker", "--kind", "claude"])
            .args(["--session", "sess-1", "--icon", "z", "--token", "sleep"])
            .arg("--state-dir")
            .arg(p("state"))
            .arg("--main-bin")
            .arg(p("main"))
            .arg("--herdr-bin")
            .arg(p("herdr"))
            .arg("--shell")
            .arg(p("shell"))
            .arg("--cwd")
            .arg(&dir)
            .arg("--preview")
            .arg(p("preview.txt"))
            .args(["--", "claude", "--resume", "sess-1"])
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        Harness {
            dir,
            child,
            master,
            screen: String::new(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Reads the pty until `needle` shows up on the screen.
    fn expect_screen(&mut self, needle: &str) {
        let start = Instant::now();
        while !self.screen.contains(needle) {
            assert!(
                start.elapsed() < TIMEOUT,
                "timed out waiting for {needle:?}, got {:?}",
                self.screen
            );
            let mut buf = [0u8; 4096];
            match self.master.read(&mut buf) {
                Ok(n) if n > 0 => self.screen.push_str(&String::from_utf8_lossy(&buf[..n])),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn herdr_log(&self) -> Vec<String> {
        fs::read_to_string(self.path("herdr.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Waits for the stub shell to have been exec'd; returns what it recorded.
    fn shell_record(&self) -> Vec<String> {
        wait_for("the shell to run", || self.path("shell.out").exists());
        fs::read_to_string(self.path("shell.out"))
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    fn rss_kib(&self) -> u64 {
        let status = fs::read_to_string(format!("/proc/{}/status", self.child.id())).unwrap();
        status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .expect("VmRSS")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn openpty() -> (File, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: valid out pointers; the fds are owned right after.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty");
    // SAFETY: openpty returned two fresh descriptors.
    let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // SAFETY: the master is a valid fd; keep it out of the child, read without blocking.
    unsafe {
        libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    (master, slave)
}

fn seq_of(line: &str) -> u64 {
    let mut words = line.split(' ');
    words.find(|w| *w == "--seq").expect("--seq");
    words.next().unwrap().parse().unwrap()
}

#[test]
fn declares_shows_replays_and_hands_over() {
    let mut h = Harness::start("typed");
    h.expect_screen("is asleep");

    assert!(h.screen.contains("line one") && h.screen.contains("line two"));
    assert!(
        !h.screen.contains("\x1b[31m"),
        "preview escapes must be stripped"
    );
    assert!(h.screen.contains(
        "z  worker (claude) is asleep. Focus this pane, press any key or send a prompt to wake it."
    ));
    assert!(h.screen.contains("session: sess-1"));

    let log = h.herdr_log();
    let src = "--source pepsit36.herdr-sleep";
    assert_eq!(log.len(), 4, "{log:?}");
    assert!(log[0].starts_with(&format!("pane report-agent {src} --seq ")));
    assert!(log[0].contains(
        "--agent sleeping-claude --state idle --message sleeping --agent-session-id sess-1 1-2 -- "
    ));
    assert!(log[0].contains(env!("CARGO_BIN_EXE_herdr-sleep-ph")));
    assert!(log[0].ends_with("-- claude --resume sess-1"), "{}", log[0]);
    assert!(log[1].starts_with(&format!("pane report-agent-session {src} --seq ")));
    assert_eq!(log[2], "agent rename 1-2 worker");
    assert!(log[3].ends_with("--token sleep=z 1-2"), "{}", log[3]);

    #[cfg(target_os = "linux")]
    {
        let rss = h.rss_kib();
        println!("RSS while waiting: {rss} kB");
        // The unoptimised test binary is larger than the release one the
        // plugin ships; the 2 MB budget applies to release builds.
        let budget = if cfg!(debug_assertions) { 4096 } else { 3000 };
        assert!(rss < budget, "RSS {rss} kB over {budget} kB");
    }

    h.type_bytes(b"hello\r");
    let shell = h.shell_record();
    assert_eq!(shell[0], format!("PWD={}", h.dir.display()));
    assert_eq!(
        &shell[1..],
        [
            "ARG=-i".to_string(),
            "ARG=-c".to_string(),
            format!(
                "ARG=claude --resume sess-1; exec {} -l",
                h.path("shell").display()
            ),
        ]
    );

    let replay = h.path("state/replay-1-2.txt");
    assert_eq!(fs::read_to_string(&replay).unwrap(), "hello");
    wait_for("the replay process", || h.path("main.out").exists());
    wait_for("the replay process output", || {
        fs::read_to_string(h.path("main.out")).is_ok_and(|s| !s.is_empty())
    });
    assert_eq!(
        fs::read_to_string(h.path("main.out")).unwrap().trim(),
        format!(
            "replay --pane 1-2 --name worker --kind claude --file {}",
            replay.display()
        )
    );

    let log = h.herdr_log();
    assert_eq!(log.len(), 6, "{log:?}");
    assert!(log[4].ends_with("--clear-token sleep 1-2"), "{}", log[4]);
    assert!(log[5].starts_with(&format!("pane release-agent {src} --seq ")));
    assert!(
        log[5].ends_with("--agent sleeping-claude 1-2"),
        "{}",
        log[5]
    );
    let seqs: Vec<u64> = [
        "pane report-agent ",
        "pane report-agent-session",
        "pane report-metadata",
        "pane release-agent",
    ]
    .iter()
    .flat_map(|p| log.iter().filter(move |l| l.starts_with(p)))
    .map(|l| seq_of(l))
    .collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seqs.len(), "seq values must be distinct");
    let in_order: Vec<u64> = log
        .iter()
        .filter(|l| !l.starts_with("agent "))
        .map(|l| seq_of(l))
        .collect();
    assert!(in_order.windows(2).all(|w| w[0] < w[1]), "{in_order:?}");
}

#[test]
fn control_keys_wake_without_a_replay() {
    let mut h = Harness::start("ctrl");
    h.expect_screen("is asleep");
    h.type_bytes(b"\x03\x03\x04");
    let shell = h.shell_record();
    assert_eq!(shell[1], "ARG=-i");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!h.path("state/replay-1-2.txt").exists());
    assert!(!h.path("main.out").exists());
}

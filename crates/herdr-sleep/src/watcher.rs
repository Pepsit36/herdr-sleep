//! The Watcher: the background process that runs a Sweep on a timer.
//!
//! Herdr has no timer for plugins and `[[startup]]` runs once, so the plugin
//! keeps its own process alive (ADR 0004): `herdr-sleep watch` is started by
//! the startup hook and relaunched by any other entry point that finds it
//! missing ([`ensure_watcher`]).
//!
//! Exactly one Watcher runs per state directory. It holds an exclusive
//! `flock` on `<state_dir>/watcher.lock` for its whole life; the kernel
//! releases the lock on any exit, so there is no stale pid to detect and a
//! recycled pid cannot fool anyone. The pid written in the file is only
//! informational.
//!
//! The Watcher exits when Herdr is gone (the socket path vanished, or
//! `herdr status` failed [`MAX_STATUS_FAILURES`] passes in a row), when it is
//! asked to stop (SIGTERM, SIGINT), or when its lock file was removed (a new
//! Watcher could otherwise lock a fresh file next to it).
//!
//! Journal events: `WATCH-START pid`, `WATCH-EXIT reason`,
//! `SWEEP considered=N slept=N refused=N` (only when something happened),
//! `WARN`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::{self, Config};
use crate::herdr::HerdrClient;
use crate::journal::Journal;
use crate::ops::{Env, LiveEnv, SweepState, resync_markers, sweep_once};

/// Name of the lock file inside the state directory.
pub const LOCK_FILE: &str = "watcher.lock";
/// Consecutive passes with a failing `herdr status` after which the Watcher
/// concludes that Herdr is gone.
pub const MAX_STATUS_FAILURES: u32 = 3;
/// Length of one wait step; the exit conditions are checked between steps.
pub const SLICE: Duration = Duration::from_secs(1);

/// Where the lock file of the Watcher of `state_dir` lives.
pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LOCK_FILE)
}

/// An exclusive lock on the lock file, released when dropped.
#[derive(Debug)]
pub struct WatcherLock {
    file: File,
}

impl WatcherLock {
    /// Try to take the lock of `state_dir` without waiting. `Ok(None)` means
    /// another process holds it, that is, a Watcher is alive.
    pub fn try_acquire(state_dir: &Path) -> std::io::Result<Option<WatcherLock>> {
        fs::create_dir_all(state_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path(state_dir))?;
        // SAFETY: `file` owns a valid descriptor for the whole call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(WatcherLock { file }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        }
    }

    /// Replace the content of the lock file with `pid`.
    pub fn write_pid(&mut self, pid: u32) -> std::io::Result<()> {
        self.file.set_len(0)?;
        // The descriptor was not opened in append mode and nothing was read
        // or written through it, so the offset is still 0.
        self.file.write_all(format!("{pid}\n").as_bytes())?;
        self.file.flush()
    }
}

/// The pid the Watcher of `state_dir` wrote in its lock file, if any.
pub fn recorded_pid(state_dir: &Path) -> Option<u32> {
    fs::read_to_string(lock_path(state_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether a Watcher runs, as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WatcherInfo {
    pub running: bool,
    /// The pid the Watcher wrote in its lock file, when it runs and wrote it.
    pub pid: Option<u32>,
}

/// Look at the lock of the Watcher of `state_dir` without keeping it.
///
/// Fails when the state directory cannot be created or the lock cannot be
/// opened.
pub fn watcher_info(state_dir: &Path) -> std::io::Result<WatcherInfo> {
    match WatcherLock::try_acquire(state_dir)? {
        Some(lock) => {
            drop(lock);
            Ok(WatcherInfo {
                running: false,
                pid: None,
            })
        }
        None => Ok(WatcherInfo {
            running: true,
            pid: recorded_pid(state_dir),
        }),
    }
}

/// What [`ensure_watcher`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensure {
    /// A Watcher holds the lock.
    AlreadyRunning,
    /// None was running; one was launched.
    Started,
    /// None was running and none could be launched.
    Failed(String),
}

/// Make sure a Watcher runs: cheap enough for every hook.
pub fn ensure_watcher() -> Ensure {
    if !socket_path_is_set() {
        return Ensure::Failed("HERDR_SOCKET_PATH is not set: no Herdr to watch".into());
    }
    let Some(state_dir) = config::state_dir(config::process_env) else {
        return Ensure::Failed("no state directory".into());
    };
    match WatcherLock::try_acquire(&state_dir) {
        Ok(Some(lock)) => drop(lock),
        Ok(None) => return Ensure::AlreadyRunning,
        Err(error) => return Ensure::Failed(format!("cannot open the lock: {error}")),
    }
    match spawn_detached() {
        Ok(()) => Ensure::Started,
        Err(error) => Ensure::Failed(format!("cannot start the watcher: {error}")),
    }
}

fn socket_path_is_set() -> bool {
    std::env::var_os("HERDR_SOCKET_PATH").is_some_and(|p| !p.is_empty())
}

/// [`ensure_watcher`] for the entry points: a failure is journaled, never
/// fatal.
pub fn ensure_watcher_logged() {
    // Without Herdr's socket there is nothing to watch (a command run outside
    // a Herdr session): stay silent.
    if !socket_path_is_set() {
        return;
    }
    if let Ensure::Failed(reason) = ensure_watcher()
        && let Some(dir) = config::state_dir(config::process_env)
    {
        let _ = fs::create_dir_all(&dir);
        Journal::new(&dir).log("WARN", &format!("watcher: {reason}"));
    }
}

/// Start `herdr-sleep watch` in its own session with its output discarded.
/// The environment is inherited, so `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH` and
/// the `HERDR_PLUGIN_*` variables reach it.
pub fn spawn_detached() -> std::io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("watch")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and touches no Rust state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The child is not waited for: it outlives this process.
    command.spawn().map(drop)
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// The live-environment seams of the loop, replaced by a scripted host in
/// tests.
pub trait Host {
    /// Whether Herdr's socket still exists.
    fn socket_exists(&self) -> bool;
    /// Whether `herdr status` succeeds.
    fn herdr_ok(&self) -> bool;
    /// Wait for `duration`.
    fn pause(&self, duration: Duration);
    /// Whether the Watcher was asked to stop.
    fn shutdown_requested(&self) -> bool;
}

/// Why the loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Signal,
    SocketGone,
    HerdrUnreachable,
    LockRemoved,
}

impl ExitReason {
    /// The word journaled after `WATCH-EXIT`.
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Signal => "signal",
            ExitReason::SocketGone => "socket-gone",
            ExitReason::HerdrUnreachable => "herdr-unreachable",
            ExitReason::LockRemoved => "lock-removed",
        }
    }
}

/// Run Sweeps until an exit condition holds.
///
/// `load_config` is called on every pass, so edits apply without a restart. A
/// config that fails to load is journaled once per distinct message and the
/// previous one stays in force.
pub fn run_loop(
    host: &dyn Host,
    env: &dyn Env,
    lock: &Path,
    load_config: &dyn Fn() -> Result<Config, String>,
) -> ExitReason {
    let state_dir = env.state_dir();
    let journal = state_dir.as_deref().map(Journal::new);
    let warn = |message: String| {
        if let Some(journal) = &journal {
            journal.log("WARN", &message);
        }
    };
    let mut state = state_dir
        .as_deref()
        .map(SweepState::load)
        .unwrap_or_default();
    let mut config = Config::default();
    let mut last_config_error: Option<String> = None;
    let mut status_failures = 0;

    let exit_condition = || {
        if host.shutdown_requested() {
            Some(ExitReason::Signal)
        } else if !lock.exists() {
            Some(ExitReason::LockRemoved)
        } else if !host.socket_exists() {
            Some(ExitReason::SocketGone)
        } else {
            None
        }
    };

    loop {
        if let Some(reason) = exit_condition() {
            return reason;
        }
        match load_config() {
            Ok(loaded) => {
                config = loaded;
                last_config_error = None;
            }
            Err(error) => {
                if last_config_error.as_ref() != Some(&error) {
                    warn(format!("config: {error}; keeping the previous one"));
                    last_config_error = Some(error);
                }
            }
        }

        if host.herdr_ok() {
            status_failures = 0;
            let report = sweep_once(env, &config, &mut state);
            if let Some(error) = &report.error {
                warn(format!("sweep: {error}"));
            } else if report.is_eventful()
                && let Some(journal) = &journal
            {
                journal.log(
                    "SWEEP",
                    &format!(
                        "considered={} slept={} refused={}",
                        report.considered,
                        report.slept.len(),
                        report.refused.len()
                    ),
                );
            }
        } else {
            status_failures += 1;
            warn(format!(
                "herdr status failed ({status_failures}/{MAX_STATUS_FAILURES})"
            ));
            if status_failures >= MAX_STATUS_FAILURES {
                return ExitReason::HerdrUnreachable;
            }
        }

        for _ in 0..config.sweep_interval_seconds.max(1) {
            host.pause(SLICE);
            if let Some(reason) = exit_condition() {
                return reason;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The live host
// ---------------------------------------------------------------------------

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Make SIGTERM and SIGINT set the shutdown flag instead of killing the
/// process.
pub fn install_signal_handlers() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic, which is
        // async-signal-safe; the zeroed `sigaction` is then fully set up.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// [`Host`] backed by the real file system, `herdr status` and the clock.
pub struct LiveHost {
    socket: PathBuf,
    client: HerdrClient,
}

impl LiveHost {
    pub fn new(socket: PathBuf, client: HerdrClient) -> Self {
        LiveHost { socket, client }
    }
}

impl Host for LiveHost {
    fn socket_exists(&self) -> bool {
        self.socket.exists()
    }

    fn herdr_ok(&self) -> bool {
        self.client.status_ok()
    }

    fn pause(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn shutdown_requested(&self) -> bool {
        SHUTDOWN.load(Ordering::SeqCst)
    }
}

/// Why `watch` did not run its loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEnd {
    /// Another Watcher holds the lock: nothing to do.
    AlreadyRunning,
    /// The loop ran and ended for this reason.
    Exited(ExitReason),
    /// The Watcher could not start; the message says why.
    Failed(String),
}

/// Run the Watcher in the foreground: `herdr-sleep watch`.
pub fn watch() -> WatchEnd {
    let Some(state_dir) = config::state_dir(config::process_env) else {
        return WatchEnd::Failed("no state directory".into());
    };
    let Some(socket) = std::env::var_os("HERDR_SOCKET_PATH")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
    else {
        return WatchEnd::Failed("HERDR_SOCKET_PATH is not set: no Herdr to watch".into());
    };
    let mut lock = match WatcherLock::try_acquire(&state_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => return WatchEnd::AlreadyRunning,
        Err(error) => return WatchEnd::Failed(format!("cannot take the lock: {error}")),
    };
    let pid = std::process::id();
    if let Err(error) = lock.write_pid(pid) {
        return WatchEnd::Failed(format!("cannot write the pid: {error}"));
    }
    install_signal_handlers();

    let journal = Journal::new(&state_dir);
    journal.log("WATCH-START", &pid.to_string());
    let client = HerdrClient::from_env();
    let env = LiveEnv::new(client.clone()).with_state_dir(&state_dir);
    // Herdr does not keep metadata tokens across a restart.
    if let Ok(config) = Config::load() {
        resync_markers(&env, &config);
    }
    let host = LiveHost::new(socket, client);
    let reason = run_loop(&host, &env, &lock_path(&state_dir), &|| {
        Config::load().map_err(|e| e.to_string())
    });
    journal.log("WATCH-EXIT", reason.as_str());
    drop(lock);
    WatchEnd::Exited(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guards::{Refusal, SleepMode};
    use crate::herdr::{Agent, AgentSession, AgentStatus, Pane, ProcessInfo, Snapshot};
    use crate::ops::EnvResult;
    use std::cell::{Cell, RefCell};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-sleep-watcher-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// One idle agent, a virtual clock advanced by the host's pauses.
    struct FakeEnv {
        dir: PathBuf,
        clock_ms: Cell<u64>,
        snapshot_failures: Cell<u32>,
        snapshots: Cell<u32>,
    }

    impl Env for FakeEnv {
        fn snapshot(&self) -> EnvResult<Snapshot> {
            self.snapshots.set(self.snapshots.get() + 1);
            if self.snapshot_failures.get() > 0 {
                self.snapshot_failures.set(self.snapshot_failures.get() - 1);
                return Err("boom".into());
            }
            Ok(Snapshot {
                agents: vec![Agent {
                    pane_id: "p1".into(),
                    agent: Some("claude".into()),
                    agent_status: AgentStatus::Idle,
                    agent_session: Some(AgentSession {
                        source: "herdr:claude".into(),
                        agent: None,
                        kind: None,
                        value: "sid".into(),
                    }),
                    ..Agent::default()
                }],
                ..Snapshot::default()
            })
        }
        fn pane(&self, _: &str) -> EnvResult<Pane> {
            unreachable!()
        }
        fn agent(&self, _: &str) -> EnvResult<Agent> {
            Ok(self.snapshot()?.agents.remove(0))
        }
        fn process_info(&self, _: &str) -> EnvResult<ProcessInfo> {
            unreachable!()
        }
        fn screen_text(&self, _: &str, _: usize) -> EnvResult<String> {
            unreachable!()
        }
        fn screen_ansi(&self, _: &str, _: usize) -> EnvResult<String> {
            unreachable!()
        }
        fn rename_agent(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!()
        }
        fn send_text(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!()
        }
        fn send_keys(&self, _: &str, _: &[&str]) -> EnvResult<()> {
            unreachable!()
        }
        fn run(&self, _: &str, _: &[&str]) -> EnvResult<()> {
            unreachable!()
        }
        fn notify(&self, _: &str, _: &str) -> EnvResult<()> {
            unreachable!()
        }
        fn tab_rename(&self, _: &str, _: &str) -> EnvResult<()> {
            Ok(())
        }
        fn workspace_report_metadata(
            &self,
            _: &str,
            _: &str,
            _: &crate::herdr::MetadataChange,
            _: u64,
        ) -> EnvResult<()> {
            Ok(())
        }
        fn check_guards(&self, _: &Config, _: &Agent, _: SleepMode) -> Result<(), Refusal> {
            Ok(())
        }
        fn agent_rss_kb(&self, _: &Agent, _: &str) -> Option<u64> {
            None
        }
        fn shell_path(&self, _: i32) -> Option<PathBuf> {
            None
        }
        fn state_dir(&self) -> Option<PathBuf> {
            Some(self.dir.clone())
        }
        fn placeholder_bin(&self) -> EnvResult<PathBuf> {
            unreachable!()
        }
        fn main_bin(&self) -> PathBuf {
            "/opt/bin/herdr-sleep".into()
        }
        fn herdr_bin(&self) -> PathBuf {
            "/usr/bin/herdr".into()
        }
        fn sleep(&self, _: Duration) {}
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn now_millis(&self) -> u64 {
            self.clock_ms.get()
        }
    }

    type Script = Box<dyn FnMut(u32, &FakeHost)>;

    /// A host whose `pause` advances the virtual clock and runs `script` with
    /// the number of the pause, so a test can break the world at a given time.
    struct FakeHost<'a> {
        env: &'a FakeEnv,
        pauses: Cell<u32>,
        socket: Cell<bool>,
        status_ok: Cell<bool>,
        shutdown: Cell<bool>,
        script: RefCell<Script>,
    }

    impl Host for FakeHost<'_> {
        fn socket_exists(&self) -> bool {
            self.socket.get()
        }
        fn herdr_ok(&self) -> bool {
            self.status_ok.get()
        }
        fn pause(&self, duration: Duration) {
            let n = self.pauses.get() + 1;
            self.pauses.set(n);
            self.env
                .clock_ms
                .set(self.env.clock_ms.get() + duration.as_millis() as u64);
            let mut script = self.script.borrow_mut();
            (*script)(n, self);
            assert!(n < 10_000, "the loop never ended");
        }
        fn shutdown_requested(&self) -> bool {
            self.shutdown.get()
        }
    }

    struct Rig {
        _dir: Dir,
        env: FakeEnv,
    }

    fn rig(name: &str) -> Rig {
        let dir = temp_dir(name);
        fs::write(lock_path(&dir), "").unwrap();
        Rig {
            env: FakeEnv {
                dir: dir.clone(),
                clock_ms: Cell::new(1_000_000),
                snapshot_failures: Cell::new(0),
                snapshots: Cell::new(0),
            },
            _dir: Dir(dir),
        }
    }

    impl Rig {
        fn host(&self, script: impl FnMut(u32, &FakeHost) + 'static) -> FakeHost<'_> {
            FakeHost {
                env: &self.env,
                pauses: Cell::new(0),
                socket: Cell::new(true),
                status_ok: Cell::new(true),
                shutdown: Cell::new(false),
                script: RefCell::new(Box::new(script)),
            }
        }

        fn run(&self, host: &FakeHost, config: impl Fn() -> Result<Config, String>) -> ExitReason {
            run_loop(host, &self.env, &lock_path(&self.env.dir), &config)
        }

        fn journal(&self) -> String {
            fs::read_to_string(self.env.dir.join(crate::journal::FILE_NAME)).unwrap_or_default()
        }
    }

    fn config(interval: u64) -> Config {
        Config {
            sweep_interval_seconds: interval,
            sleep_after_minutes: 1_000,
            dry_run: true,
            ..Config::default()
        }
    }

    // Lock ---------------------------------------------------------------

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let dir = Dir(temp_dir("lock"));
        let mut first = WatcherLock::try_acquire(&dir.0).unwrap().unwrap();
        first.write_pid(4242).unwrap();
        assert_eq!(recorded_pid(&dir.0), Some(4242));
        assert!(WatcherLock::try_acquire(&dir.0).unwrap().is_none());
        drop(first);
        // A process forked by a parallel test may hold the descriptor for a
        // moment, until it execs.
        let mut second = None;
        for _ in 0..200 {
            second = WatcherLock::try_acquire(&dir.0).unwrap();
            if second.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut second = second.expect("the lock is released once the first holder is gone");
        // The pid is overwritten, not appended to.
        second.write_pid(7).unwrap();
        assert_eq!(fs::read_to_string(lock_path(&dir.0)).unwrap(), "7\n");
    }

    #[test]
    fn watcher_info_tells_running_from_not_running() {
        let dir = Dir(temp_dir("info"));
        assert_eq!(
            watcher_info(&dir.0).unwrap(),
            WatcherInfo {
                running: false,
                pid: None
            }
        );
        let mut lock = WatcherLock::try_acquire(&dir.0).unwrap().unwrap();
        lock.write_pid(4242).unwrap();
        assert_eq!(
            watcher_info(&dir.0).unwrap(),
            WatcherInfo {
                running: true,
                pid: Some(4242)
            }
        );
    }

    #[test]
    fn watcher_info_fails_cleanly_on_an_unusable_state_dir() {
        let dir = Dir(temp_dir("info-bad"));
        let file = dir.0.join("plain-file");
        fs::write(&file, "x").unwrap();
        assert!(watcher_info(&file.join("state")).is_err());
    }

    // Loop ---------------------------------------------------------------

    #[test]
    fn a_pass_runs_every_interval() {
        let rig = rig("interval");
        let host = rig.host(|n, host| host.shutdown.set(n == 25));
        assert_eq!(rig.run(&host, || Ok(config(10))), ExitReason::Signal);
        // Passes at the start, after 10 s and after 20 s, each reading the
        // session for the sweep and again for the markers.
        assert_eq!(rig.env.snapshots.get(), 6);
    }

    #[test]
    fn the_loop_exits_when_the_socket_vanishes() {
        let rig = rig("socket");
        let host = rig.host(|n, host| host.socket.set(n < 4));
        assert_eq!(rig.run(&host, || Ok(config(60))), ExitReason::SocketGone);
        assert_eq!(host.pauses.get(), 4);
    }

    #[test]
    fn the_loop_exits_when_the_lock_file_is_removed() {
        let rig = rig("lock-removed");
        let lock = lock_path(&rig.env.dir);
        let host = rig.host(move |n, _| {
            if n == 2 {
                fs::remove_file(&lock).unwrap();
            }
        });
        assert_eq!(rig.run(&host, || Ok(config(60))), ExitReason::LockRemoved);
        assert_eq!(host.pauses.get(), 2);
    }

    #[test]
    fn a_signal_stops_the_loop_within_a_slice() {
        let rig = rig("signal");
        let host = rig.host(|n, host| host.shutdown.set(n == 1));
        assert_eq!(rig.run(&host, || Ok(config(60))), ExitReason::Signal);
        assert_eq!(host.pauses.get(), 1);
    }

    #[test]
    fn three_failed_statuses_in_a_row_end_the_loop() {
        let rig = rig("status");
        let host = rig.host(|_, _| {});
        host.status_ok.set(false);
        assert_eq!(
            rig.run(&host, || Ok(config(5))),
            ExitReason::HerdrUnreachable
        );
        assert_eq!(host.pauses.get(), 10);
        assert_eq!(rig.env.snapshots.get(), 0);
    }

    #[test]
    fn a_single_failed_status_is_tolerated() {
        let rig = rig("status-once");
        // Down for the second pass only, then Herdr goes away for real.
        let host = rig.host(|n, host| {
            host.status_ok.set(n != 5);
            host.socket.set(n < 20);
        });
        assert_eq!(rig.run(&host, || Ok(config(5))), ExitReason::SocketGone);
        assert!(rig.journal().contains("herdr status failed (1/3)"));
    }

    #[test]
    fn a_failed_pass_does_not_stop_the_loop() {
        let rig = rig("pass-error");
        rig.env.snapshot_failures.set(1);
        let host = rig.host(|n, host| host.shutdown.set(n == 12));
        assert_eq!(rig.run(&host, || Ok(config(5))), ExitReason::Signal);
        // The failed read ends its pass at once; the two others read twice.
        assert_eq!(rig.env.snapshots.get(), 5);
        assert!(
            rig.journal()
                .contains("WARN sweep: cannot read the session")
        );
    }

    #[test]
    fn the_config_is_reloaded_on_every_pass() {
        let rig = rig("reload");
        let calls = Cell::new(0);
        let host = rig.host(|n, host| host.shutdown.set(n == 7));
        let reason = rig.run(&host, || {
            calls.set(calls.get() + 1);
            // First pass: far from idle. Second pass: due at once.
            Ok(if calls.get() == 1 {
                config(3)
            } else {
                Config {
                    sleep_after_minutes: 0,
                    ..config(3)
                }
            })
        });
        assert_eq!(reason, ExitReason::Signal);
        assert_eq!(calls.get(), 3);
        let journal = rig.journal();
        assert_eq!(journal.matches(" SWEEP ").count(), 2, "{journal}");
        assert!(journal.contains("SWEEP considered=1 slept=0 refused=0"));
    }

    #[test]
    fn a_broken_config_is_journaled_once_and_the_previous_one_stays() {
        let rig = rig("broken");
        let calls = Cell::new(0);
        let host = rig.host(|n, host| host.shutdown.set(n == 12));
        rig.run(&host, || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(config(4))
            } else {
                Err("bad value".into())
            }
        });
        let journal = rig.journal();
        assert_eq!(journal.matches("config: bad value").count(), 1, "{journal}");
        // The interval of the first config kept applying: 3 passes in 12 s.
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn idle_passes_are_not_journaled() {
        let rig = rig("quiet");
        let host = rig.host(|n, host| host.shutdown.set(n == 12));
        rig.run(&host, || Ok(config(4)));
        assert_eq!(rig.journal(), "");
    }
}

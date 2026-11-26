//! One Sleep at a time per pane.
//!
//! A Sweep of the Watcher, the "Sleep idle agents now" action and a manual
//! Sleep run in different processes, and each works from a list of agents it
//! read earlier. Two of them reaching the same pane would type the Placeholder
//! command twice: the second copy lands in the Placeholder, which keeps what
//! is typed in it as Captured input, and the Replay types it into the Agent at
//! the next Wake.
//!
//! The lock is an `flock` on `<state_dir>/sleep-<pane>.lock`, held for the
//! whole Sleep. The kernel releases it when the process dies, so a crash never
//! leaves a pane locked.

use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use super::wake::sanitize_pane;

/// Holds the lock of a pane until dropped.
#[derive(Debug)]
pub struct PaneLock {
    _file: File,
}

fn lock_path(state_dir: &Path, pane: &str, purpose: &str) -> PathBuf {
    state_dir.join(format!("{purpose}-{}.lock", sanitize_pane(pane)))
}

impl PaneLock {
    /// Take the lock of `pane` without waiting. `Ok(None)` means another Sleep
    /// holds it.
    pub fn try_acquire(state_dir: &Path, pane: &str) -> std::io::Result<Option<PaneLock>> {
        Self::try_acquire_for(state_dir, pane, "sleep")
    }

    /// The lock of `pane` for another `purpose` (`wake`): locks of different
    /// purposes do not exclude each other.
    pub fn try_acquire_for(
        state_dir: &Path,
        pane: &str,
        purpose: &str,
    ) -> std::io::Result<Option<PaneLock>> {
        fs::create_dir_all(state_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path(state_dir, pane, purpose))?;
        // SAFETY: `file` owns a valid descriptor for the whole call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(PaneLock { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-sleep-pane-lock-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_pane_is_locked_once_at_a_time() {
        let dir = dir("once");
        let first = PaneLock::try_acquire(&dir, "w1:p2").unwrap();
        assert!(first.is_some());
        assert!(PaneLock::try_acquire(&dir, "w1:p2").unwrap().is_none());
        drop(first);
        assert!(PaneLock::try_acquire(&dir, "w1:p2").unwrap().is_some());
    }

    #[test]
    fn two_panes_do_not_wait_for_each_other() {
        let dir = dir("two");
        let _first = PaneLock::try_acquire(&dir, "w1:p2").unwrap().unwrap();
        assert!(PaneLock::try_acquire(&dir, "w1:p3").unwrap().is_some());
    }
}

//! Writing the state files: a temporary name of its own and a rename, so two
//! processes writing the same file never write into each other's temporary
//! file, and a reader never sees a half-written one.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace the content of `path` with `text`. The temporary file sits next to
/// `path` and carries the process and a counter in its name.
pub fn write(path: &Path, text: &str) -> io::Result<()> {
    write_bytes(path, text.as_bytes())
}

/// [`write`] for content that may not be UTF-8 (a journal cut mid-character).
pub fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Run `update` holding an exclusive lock on `<dir>/<name>.lock`, waiting for
/// whoever holds it. For a file several processes read, change and write back:
/// without it one of two simultaneous updates is lost. The kernel drops the
/// lock when the process ends. A lock that cannot be taken is not a reason to
/// lose the update: it runs unlocked.
pub fn locked<T>(dir: &Path, name: &str, update: impl FnOnce() -> T) -> T {
    use std::os::unix::io::AsRawFd;
    let file = fs::create_dir_all(dir).ok().and_then(|()| {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(format!("{name}.lock")))
            .ok()
    });
    if let Some(file) = &file {
        // SAFETY: `file` owns a valid descriptor for the whole call.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    }
    let result = update();
    drop(file);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_content_is_replaced_and_no_temporary_file_is_left() {
        let dir = std::env::temp_dir().join(format!("herdr-sleep-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("state.json");
        write(&file, "one").unwrap();
        write(&file, "two").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["state.json"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn simultaneous_locked_updates_are_not_lost() {
        let dir = std::env::temp_dir().join(format!("herdr-sleep-locked-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("count");
        fs::write(&file, "0").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (dir, file) = (dir.clone(), file.clone());
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        locked(&dir, "count", || {
                            let n: u32 = fs::read_to_string(&file).unwrap().parse().unwrap();
                            write(&file, &(n + 1).to_string()).unwrap();
                        });
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), "200");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_directory_is_an_error_without_a_leftover() {
        let file = std::env::temp_dir().join("herdr-sleep-atomic-absent/state.json");
        assert!(write(&file, "x").is_err());
    }
}

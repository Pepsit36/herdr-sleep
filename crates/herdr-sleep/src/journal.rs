//! The journal: an append-only log of what the plugin did and why.
//!
//! One line per event, `2026-11-07T10:15:30Z EVENT details`, in
//! `<state_dir>/herdr-sleep.log`. The file is bounded: when it grows past
//! [`MAX_BYTES`] the oldest half is dropped. Lines are mirrored to stderr only
//! when stderr is a terminal, so a command run by hand shows its progress and
//! a detached process stays quiet.

use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Name of the log file inside the state directory.
pub const FILE_NAME: &str = "herdr-sleep.log";
/// Size above which the oldest half of the file is dropped.
pub const MAX_BYTES: u64 = 1024 * 1024;

/// Append-only event log.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
    mirror: bool,
}

impl Journal {
    /// Journal in `state_dir`, mirrored to stderr when it is a terminal.
    pub fn new(state_dir: &Path) -> Self {
        Self::with_mirror(state_dir, std::io::stderr().is_terminal())
    }

    /// Journal in `state_dir` with the stderr mirror forced on or off.
    pub fn with_mirror(state_dir: &Path, mirror: bool) -> Self {
        Journal {
            path: state_dir.join(FILE_NAME),
            mirror,
        }
    }

    /// The log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record an event now. Failures to write are ignored: the journal must
    /// never be the reason an operation fails.
    pub fn log(&self, event: &str, details: &str) {
        self.log_at(SystemTime::now(), event, details);
    }

    fn log_at(&self, at: SystemTime, event: &str, details: &str) {
        let line = format_line(at, event, details);
        if self.mirror {
            eprintln!("{line}");
        }
        let _ = self.append(&line);
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        if file.metadata()?.len() > MAX_BYTES {
            drop(file);
            self.truncate_oldest_half()?;
        }
        Ok(())
    }

    fn truncate_oldest_half(&self) -> std::io::Result<()> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        // Under the journal lock and by rename, so a process trimming the file never
        // overwrites the lines another one appends meanwhile.
        crate::ops::atomic::locked(dir, "journal", || {
            let bytes = fs::read(&self.path)?;
            if bytes.len() as u64 <= MAX_BYTES {
                // Another process trimmed it first.
                return Ok(());
            }
            let mut start = bytes.len() / 2;
            if let Some(offset) = bytes[start..].iter().position(|&b| b == b'\n') {
                start += offset + 1;
            }
            crate::ops::atomic::write_bytes(&self.path, &bytes[start..])
        })
    }
}

/// `RFC3339Z EVENT details`, on one line.
pub fn format_line(at: SystemTime, event: &str, details: &str) -> String {
    let details = details.replace(['\n', '\r'], " ");
    let stamp = rfc3339_utc(at);
    if details.is_empty() {
        format!("{stamp} {event}")
    } else {
        format!("{stamp} {event} {details}")
    }
}

/// UTC timestamp such as `2026-11-07T10:15:30Z`.
pub fn rfc3339_utc(at: SystemTime) -> String {
    let secs = match at.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Proleptic Gregorian date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-sleep-journal-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(rfc3339_utc(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(at(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(at(1_762_510_530)), "2025-11-07T10:15:30Z");
        assert_eq!(rfc3339_utc(at(4_102_444_799)), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn line_format() {
        assert_eq!(
            format_line(at(0), "SLEEP", "pane=w1-2\nkind=claude"),
            "1970-01-01T00:00:00Z SLEEP pane=w1-2 kind=claude"
        );
        assert_eq!(format_line(at(0), "WARN", ""), "1970-01-01T00:00:00Z WARN");
    }

    #[test]
    fn appends_lines() {
        let dir = temp_dir("append");
        let journal = Journal::with_mirror(&dir, false);
        journal.log("SLEEP", "one");
        journal.log("ASLEEP", "two");
        let text = fs::read_to_string(journal.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("Z SLEEP one"));
        assert!(lines[1].ends_with("Z ASLEEP two"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn drops_the_oldest_half_past_the_bound() {
        let dir = temp_dir("bound");
        let journal = Journal::with_mirror(&dir, false);
        let filler = "x".repeat(1000);
        for i in 0..1100 {
            journal.log("EVENT", &format!("{i:05} {filler}"));
        }
        let text = fs::read_to_string(journal.path()).unwrap();
        assert!(text.len() as u64 <= MAX_BYTES);
        assert!(text.len() as u64 > MAX_BYTES / 4);
        assert!(text.lines().all(|l| l.contains(" EVENT ")));
        assert!(text.contains("01099"));
        assert!(!text.contains("00000 "));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unwritable_directory_is_ignored() {
        let journal = Journal::with_mirror(Path::new("/nonexistent/herdr-sleep"), false);
        journal.log("SKIP", "nothing happens");
    }
}

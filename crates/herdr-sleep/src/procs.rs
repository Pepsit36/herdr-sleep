//! Process inspection on Linux and macOS, with nothing but `ps` and `/proc`.
//!
//! The plugin needs to know what runs behind a pane: the agent process and
//! its descendants (to measure the memory a Sleep frees and to refuse to
//! sleep an agent with busy children), the agent's environment (to detect a
//! protected environment) and the shell behind the pane (to relaunch it on
//! Wake).

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::herdr::model::ProcessInfo;

/// One line of `ps -axo pid=,ppid=,rss=,command=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: i32,
    pub ppid: i32,
    /// Resident set size in kilobytes.
    pub rss_kb: u64,
    /// Command line as `ps` prints it, arguments included.
    pub command: String,
}

impl ProcessEntry {
    /// First word of the command line, the program as it was invoked.
    pub fn program(&self) -> &str {
        self.command.split_whitespace().next().unwrap_or("")
    }
}

/// The process table at one point in time.
#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    entries: Vec<ProcessEntry>,
    by_pid: HashMap<i32, usize>,
    children: HashMap<i32, Vec<usize>>,
}

impl ProcessTable {
    /// Read the live process table through `ps`.
    pub fn load() -> io::Result<Self> {
        let output = Command::new("ps")
            // `ww` keeps a long command line whole (`--remote-control` at its end),
            // whatever `COLUMNS` says.
            .args(["-axwwo", "pid=,ppid=,rss=,command="])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "ps exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(Self::parse(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Parse the output of `ps -axo pid=,ppid=,rss=,command=`.
    ///
    /// Lines that do not start with three numbers are skipped.
    pub fn parse(text: &str) -> Self {
        let entries: Vec<ProcessEntry> = text.lines().filter_map(parse_line).collect();
        Self::from_entries(entries)
    }

    /// Build a table from entries, for instance from a parsed sample.
    pub fn from_entries(entries: Vec<ProcessEntry>) -> Self {
        let mut by_pid = HashMap::with_capacity(entries.len());
        let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            by_pid.insert(entry.pid, index);
            children.entry(entry.ppid).or_default().push(index);
        }
        ProcessTable {
            entries,
            by_pid,
            children,
        }
    }

    /// Every entry, in `ps` order.
    pub fn entries(&self) -> &[ProcessEntry] {
        &self.entries
    }

    /// The entry of `pid`, if it was running.
    pub fn get(&self, pid: i32) -> Option<&ProcessEntry> {
        self.by_pid.get(&pid).map(|&i| &self.entries[i])
    }

    /// Direct children of `pid`.
    pub fn children(&self, pid: i32) -> Vec<&ProcessEntry> {
        self.children
            .get(&pid)
            .map(|indexes| indexes.iter().map(|&i| &self.entries[i]).collect())
            .unwrap_or_default()
    }

    /// Children, grandchildren and so on of `pid`, breadth first.
    pub fn descendants(&self, pid: i32) -> Vec<&ProcessEntry> {
        let mut seen: HashSet<i32> = HashSet::from([pid]);
        let mut queue: VecDeque<i32> = VecDeque::from([pid]);
        let mut found = Vec::new();
        while let Some(current) = queue.pop_front() {
            for child in self.children(current) {
                if seen.insert(child.pid) {
                    queue.push_back(child.pid);
                    found.push(child);
                }
            }
        }
        found
    }

    /// Resident memory of `pid` and all its descendants, in kilobytes.
    pub fn rss_of_tree(&self, pid: i32) -> u64 {
        let own = self.get(pid).map(|e| e.rss_kb).unwrap_or(0);
        own + self.descendants(pid).iter().map(|e| e.rss_kb).sum::<u64>()
    }
}

fn parse_line(line: &str) -> Option<ProcessEntry> {
    let mut rest = line.trim_start();
    let mut numbers = [0i64; 3];
    for slot in &mut numbers {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        *slot = rest[..end].parse().ok()?;
        rest = rest[end..].trim_start();
    }
    Some(ProcessEntry {
        pid: i32::try_from(numbers[0]).ok()?,
        ppid: i32::try_from(numbers[1]).ok()?,
        rss_kb: u64::try_from(numbers[2]).ok()?,
        command: rest.trim_end().to_string(),
    })
}

/// The environment of a running process.
///
/// Linux reads `/proc/<pid>/environ`; macOS asks `ps eww`, whose output
/// appends `KEY=value` words to the command line (a value containing spaces
/// is cut at the first space, which is enough to test presence and simple
/// values).
pub fn environment_of(pid: i32) -> io::Result<Vec<(String, String)>> {
    if cfg!(target_os = "macos") {
        let output = Command::new("ps")
            .args(["eww", "-o", "command=", "-p", &pid.to_string()])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("ps found no process {pid}"),
            ));
        }
        Ok(parse_ps_environment(&String::from_utf8_lossy(
            &output.stdout,
        )))
    } else {
        let bytes = std::fs::read(format!("/proc/{pid}/environ"))?;
        Ok(parse_environ_block(&bytes))
    }
}

/// Parse the NUL separated `KEY=value` block of `/proc/<pid>/environ`.
pub fn parse_environ_block(bytes: &[u8]) -> Vec<(String, String)> {
    bytes
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| split_assignment(&String::from_utf8_lossy(entry)))
        .collect()
}

/// Parse the `command args KEY=value ...` line printed by `ps eww`.
///
/// Words are taken as environment entries from the first word that looks
/// like an assignment, so a command argument containing `=` before the
/// environment would be taken as one too; a pane shell's command line
/// is short enough for that not to matter.
pub fn parse_ps_environment(line: &str) -> Vec<(String, String)> {
    line.split_whitespace()
        .skip_while(|word| !looks_like_assignment(word))
        .filter_map(split_assignment)
        .collect()
}

fn looks_like_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((key, _)) => {
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !key.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

fn split_assignment(entry: &str) -> Option<(String, String)> {
    let (key, value) = entry.split_once('=')?;
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), value.to_string()))
}

/// Whether the environment of `pid` defines any of `names`.
///
/// A process whose environment cannot be read (gone, or owned by another
/// user) counts as defining none.
pub fn has_any_env_var(pid: i32, names: &[&str]) -> bool {
    environment_of(pid)
        .map(|env| env.iter().any(|(key, _)| names.contains(&key.as_str())))
        .unwrap_or(false)
}

/// Seconds from `[[dd-]hh:]mm:ss`, the elapsed-time format of `ps`.
pub fn parse_etime(text: &str) -> Option<u64> {
    let text = text.trim();
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let parts: Vec<u64> = clock
        .split(':')
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    let seconds = match parts[..] {
        [m, s] => m * 60 + s,
        [h, m, s] => (h * 60 + m) * 60 + s,
        _ => return None,
    };
    Some(days * 86_400 + seconds)
}

/// Wall-clock milliseconds since the Unix epoch at which process `pid`
/// started, `None` when it cannot be told.
pub fn started_ms(pid: i32, now_ms: u64) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let elapsed = parse_etime(&String::from_utf8_lossy(&output.stdout))?;
    now_ms.checked_sub(elapsed * 1000)
}

/// The executable behind process `pid`, as an absolute existing path.
///
/// The first word of the command line is taken, stripped of the leading `-`
/// a login shell carries, and resolved through `PATH` when it is not already
/// a path to an existing file.
pub fn shell_path_of(pid: i32) -> Option<PathBuf> {
    let output = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    resolve_program(text.split_whitespace().next()?)
}

/// Resolve a program word (`-fish`, `/usr/bin/fish`, `fish`) to an existing
/// absolute path.
pub fn resolve_program(word: &str) -> Option<PathBuf> {
    let word = word.strip_prefix('-').unwrap_or(word);
    if word.is_empty() {
        return None;
    }
    let candidate = Path::new(word);
    if candidate.is_absolute() {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    if word.contains('/') {
        return std::fs::canonicalize(candidate)
            .ok()
            .filter(|p| p.is_file());
    }
    find_in_path(word)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The pid of the agent binary `bin` among the pane's foreground processes.
///
/// A process matches when Herdr names it `bin`, or when the first word of
/// its command line is `bin` or ends with `/bin`.
pub fn find_agent_process(info: &ProcessInfo, table: &ProcessTable, bin: &str) -> Option<i32> {
    info.foreground_processes
        .iter()
        .find(|process| {
            process.name == bin
                || table
                    .get(process.pid)
                    .is_some_and(|entry| program_is(entry.program(), bin))
        })
        .map(|process| process.pid)
}

fn program_is(program: &str, bin: &str) -> bool {
    program == bin
        || program
            .strip_suffix(bin)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

/// Available memory as a percent of the total, from the text of `/proc/meminfo`.
pub fn available_memory_percent(meminfo: &str) -> Option<u8> {
    let kb = |key: &str| -> Option<u64> {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let total = kb("MemTotal").filter(|total| *total > 0)?;
    let available = kb("MemAvailable")?;
    u8::try_from((available.min(total) * 100 / total).min(100)).ok()
}

/// Resident memory of one process in kilobytes, `None` when it is gone.
pub fn rss_kb(pid: i32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn available_memory_is_read_from_meminfo() {
        let text = "MemTotal:       16000000 kB\nMemFree:  100 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(super::available_memory_percent(text), Some(25));
        assert_eq!(super::available_memory_percent("MemTotal: 10 kB\n"), None);
        assert_eq!(
            super::available_memory_percent("MemAvailable: 1 kB\n"),
            None
        );
        assert_eq!(super::available_memory_percent(""), None);
    }

    use super::*;
    use crate::herdr::model::ForegroundProcess;

    const SHELL_PID: i32 = 184186;
    const AGENT_PID: i32 = 184214;

    fn linux_table() -> ProcessTable {
        let path = format!("{}/tests/fixtures/ps-linux.txt", env!("CARGO_MANIFEST_DIR"));
        ProcessTable::parse(&std::fs::read_to_string(path).unwrap())
    }

    const MACOS_SAMPLE: &str = "\
    1     0  18240 /sbin/launchd
  512     1  11200 /System/Library/CoreServices/loginwindow console
 4021   512  38912 /Applications/Herdr.app/Contents/MacOS/herdr
 4030  4021   9216 -zsh
 4101  4030 412032 node /opt/homebrew/bin/claude --resume 11111111-2222-4333-8444-555555555555
 4102  4101   2048 /bin/sh -c git status
 4112  4030   5120 /usr/local/bin/codex
";

    #[test]
    fn ps_elapsed_times_parse() {
        assert_eq!(parse_etime("05:09"), Some(309));
        assert_eq!(parse_etime("  1:02:03 "), Some(3723));
        assert_eq!(
            parse_etime("2-03:04:05"),
            Some(2 * 86_400 + 3 * 3600 + 4 * 60 + 5)
        );
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("abc"), None);
    }

    #[test]
    fn this_process_started_before_now() {
        let now = 1_900_000_000_000;
        let started = started_ms(i32::try_from(std::process::id()).unwrap(), now).unwrap();
        assert!(started <= now);
    }

    #[test]
    fn parses_linux_sample() {
        let table = linux_table();
        assert_eq!(table.entries().len(), 67);
        let init = table.get(1).unwrap();
        assert_eq!(
            (init.ppid, init.rss_kb, init.command.as_str()),
            (0, 12504, "/sbin/init")
        );
        let agent = table.get(AGENT_PID).unwrap();
        assert_eq!(agent.ppid, SHELL_PID);
        assert_eq!(agent.rss_kb, 458752);
        assert_eq!(agent.program(), "claude");
        assert_eq!(table.get(638).unwrap().command, "-fish");
        let ps = table.entries().last().unwrap();
        assert_eq!(ps.command, "ps -axo pid=,ppid=,rss=,command=");
        assert!(table.get(999_999).is_none());
    }

    #[test]
    fn parses_macos_shaped_sample() {
        let table = ProcessTable::parse(MACOS_SAMPLE);
        assert_eq!(table.entries().len(), 7);
        assert_eq!(table.get(4030).unwrap().command, "-zsh");
        assert_eq!(table.get(4101).unwrap().program(), "node");
        assert_eq!(table.get(4101).unwrap().rss_kb, 412032);
        assert_eq!(table.descendants(4030).len(), 3);
        assert_eq!(table.rss_of_tree(4030), 9216 + 412032 + 2048 + 5120);
    }

    #[test]
    fn skips_lines_that_are_not_processes() {
        let table = ProcessTable::parse("  PID  PPID RSS COMMAND\n 12 1 100 a b c\n\nnot a line\n");
        assert_eq!(table.entries().len(), 1);
        assert_eq!(table.get(12).unwrap().command, "a b c");
    }

    #[test]
    fn descendants_are_transitive_and_cycle_safe() {
        let table = linux_table();
        let pids: Vec<i32> = table.descendants(AGENT_PID).iter().map(|e| e.pid).collect();
        assert_eq!(
            pids,
            [
                203817, 204741, 204775, 204577, 204787, 204776, 204782, 204785
            ]
        );
        assert_eq!(table.children(AGENT_PID).len(), 3);
        assert!(table.descendants(204785).is_empty());
        assert!(table.descendants(999_999).is_empty());

        let looped = ProcessTable::from_entries(vec![
            ProcessEntry {
                pid: 1,
                ppid: 2,
                rss_kb: 1,
                command: "a".into(),
            },
            ProcessEntry {
                pid: 2,
                ppid: 1,
                rss_kb: 2,
                command: "b".into(),
            },
        ]);
        assert_eq!(looped.descendants(1).len(), 1);
        assert_eq!(looped.rss_of_tree(1), 3);
    }

    #[test]
    fn rss_of_tree_sums_the_subtree() {
        let table = linux_table();
        let agent_tree = 458752 + 6208 + 1916 + 6216 + 1720 + 3388 + 3340 + 8200 + 4796;
        assert_eq!(table.rss_of_tree(AGENT_PID), agent_tree);
        assert_eq!(table.rss_of_tree(SHELL_PID), 8132 + agent_tree);
        assert_eq!(table.rss_of_tree(999_999), 0);
    }

    #[test]
    fn environ_block_parsing() {
        let block = b"PATH=/usr/bin:/bin\0HERDR_ENV=1\0EMPTY=\0=odd\0WITH=a=b\0\0";
        let env = parse_environ_block(block);
        assert_eq!(
            env,
            [
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("HERDR_ENV".into(), "1".into()),
                ("EMPTY".into(), String::new()),
                ("WITH".into(), "a=b".into()),
            ]
        );
    }

    #[test]
    fn ps_environment_parsing() {
        let line = "/bin/zsh -c claude --resume x TERM=xterm-256color HOME=/Users/u HERDR_PANE_ID=w1:p1 PATH=/a:/b\n";
        let env = parse_ps_environment(line);
        assert_eq!(
            env,
            [
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("HOME".into(), "/Users/u".into()),
                ("HERDR_PANE_ID".into(), "w1:p1".into()),
                ("PATH".into(), "/a:/b".into()),
            ]
        );
        assert!(parse_ps_environment("/usr/bin/fish").is_empty());
        assert!(parse_ps_environment("").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_own_environment_from_proc() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let env = environment_of(pid).unwrap();
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        assert!(env.iter().all(|(k, _)| !k.is_empty()));
        assert!(has_any_env_var(
            pid,
            &["NO_SUCH_VARIABLE_HERDR_SLEEP", "PATH"]
        ));
        assert!(!has_any_env_var(pid, &["NO_SUCH_VARIABLE_HERDR_SLEEP"]));
        assert!(!has_any_env_var(i32::MAX, &["PATH"]));
    }

    #[test]
    fn resolves_program_words() {
        let sh = resolve_program("-/bin/sh").unwrap();
        assert_eq!(sh, PathBuf::from("/bin/sh"));
        assert!(resolve_program("/definitely/not/here").is_none());
        assert!(resolve_program("-").is_none());
        assert!(resolve_program("no-such-program-herdr-sleep").is_none());
        let found = resolve_program("sh").unwrap();
        assert!(found.is_absolute() && found.is_file());
    }

    #[test]
    fn shell_path_of_own_process_is_the_test_binary() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let path = shell_path_of(pid).unwrap();
        assert!(path.is_absolute() && path.is_file());
        let own = std::env::current_exe().unwrap();
        assert_eq!(path.file_name(), own.file_name());
    }

    #[test]
    fn rss_of_own_process_is_positive() {
        let pid = i32::try_from(std::process::id()).unwrap();
        assert!(rss_kb(pid).unwrap() > 0);
        assert!(rss_kb(i32::MAX).is_none());
    }

    #[test]
    fn finds_agent_process_by_name_or_command() {
        let table = linux_table();
        let info = ProcessInfo {
            pane_id: "wC:p1".into(),
            shell_pid: SHELL_PID,
            foreground_process_group_id: AGENT_PID,
            foreground_processes: vec![ForegroundProcess {
                pid: AGENT_PID,
                name: "claude".into(),
                cwd: None,
            }],
        };
        assert_eq!(find_agent_process(&info, &table, "claude"), Some(AGENT_PID));
        assert_eq!(find_agent_process(&info, &table, "codex"), None);

        // Herdr names the process differently, the command line decides.
        let renamed = ProcessInfo {
            foreground_processes: vec![ForegroundProcess {
                pid: AGENT_PID,
                name: "node".into(),
                cwd: None,
            }],
            ..info.clone()
        };
        assert_eq!(
            find_agent_process(&renamed, &table, "claude"),
            Some(AGENT_PID)
        );

        // A full path in the command line matches by its last component.
        let macos = ProcessTable::parse(MACOS_SAMPLE);
        let codex = ProcessInfo {
            foreground_processes: vec![
                ForegroundProcess {
                    pid: 4102,
                    name: "sh".into(),
                    cwd: None,
                },
                ForegroundProcess {
                    pid: 4112,
                    name: "codex".into(),
                    cwd: None,
                },
            ],
            ..info.clone()
        };
        assert_eq!(find_agent_process(&codex, &macos, "codex"), Some(4112));
        let by_path = ProcessInfo {
            foreground_processes: vec![ForegroundProcess {
                pid: 4112,
                name: "x".into(),
                cwd: None,
            }],
            ..info.clone()
        };
        assert_eq!(find_agent_process(&by_path, &macos, "codex"), Some(4112));
        assert_eq!(find_agent_process(&by_path, &macos, "dex"), None);

        let empty = ProcessInfo {
            foreground_processes: Vec::new(),
            ..info
        };
        assert_eq!(find_agent_process(&empty, &table, "claude"), None);
    }
}

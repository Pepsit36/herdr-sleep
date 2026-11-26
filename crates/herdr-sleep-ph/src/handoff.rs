//! Giving the pane back: save the Captured input, start the Replay, restore
//! the terminal and `exec` the user's shell to Wake the Agent.

use crate::args::Args;
use crate::input::printable;
use crate::tty::RawMode;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Quotes one word for a POSIX shell.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Seconds after the shell started within which a failing resume command is
/// reported: it did not get to run, as opposed to an agent the user quit.
const QUICK_FAILURE_SECS: u32 = 10;

/// POSIX `sh` helper run after the resume command with its exit status, the
/// command line and the wall-clock second the Wake started at. It says what
/// happened when the resume command failed within [`QUICK_FAILURE_SECS`] of
/// the Wake, so the pane is not silently left at a bare shell. The shell of
/// the pane is the same process as the Placeholder, so its own age would be
/// the age of the pane: the Wake time is given instead.
fn failure_report() -> String {
    format!(
        concat!(
            "[ \"$1\" = 0 ] && exit 0; ",
            "now=$(date +%s); ",
            "[ $((now - $3)) -lt {limit} ] 2>/dev/null || exit 0; ",
            "printf \"\\nherdr-sleep: the agent could not be resumed (exit status %s).\\n\" \"$1\"; ",
            "printf \"herdr-sleep: command tried: %s\\n\" \"$2\"; ",
            "printf \"herdr-sleep: the session may have no saved conversation. This is a plain shell.\\n\\n\""
        ),
        limit = QUICK_FAILURE_SECS
    )
}

/// How the pane's shell reads the wake script. The script is typed to that
/// shell (it runs the resume command with the user's aliases and `PATH`), so
/// it has to be written in its syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFamily {
    /// `sh`, bash, zsh, dash, ksh: the exit status of the last command is `$?`.
    Posix,
    /// fish rejects `$?` while parsing and would exit with status 127 before
    /// running anything: it spells the status `$status`.
    Fish,
    /// A shell whose syntax is unknown: no status is read, so nothing in the
    /// script can fail to parse. A failed resume is not reported.
    Other,
}

impl ShellFamily {
    fn of(shell: &str) -> Self {
        let name = Path::new(shell)
            .file_name()
            .map(|n| n.to_string_lossy().trim_start_matches('-').to_string())
            .unwrap_or_default();
        match name.as_str() {
            "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh" | "mksh" | "pdksh" | "busybox" => {
                Self::Posix
            }
            "fish" => Self::Fish,
            _ => Self::Other,
        }
    }
}

/// The script the shell runs: the resume command, a report when it failed at
/// once, then a login shell. Only the exit status of the resume command is
/// spelled in the shell's own syntax; the report itself is a POSIX `sh`
/// script, run by `sh`.
pub fn wake_script(resume: &[String], shell: &str, woke_at: u64) -> String {
    script_for(resume, shell, ShellFamily::of(shell), woke_at)
}

fn script_for(resume: &[String], shell: &str, family: ShellFamily, woke_at: u64) -> String {
    let command: Vec<String> = resume.iter().map(|w| shell_quote(w)).collect();
    let command = command.join(" ");
    let status = match family {
        ShellFamily::Posix => "$?",
        ShellFamily::Fish => "$status",
        ShellFamily::Other => {
            return format!("{command}; exec {} -l", shell_quote(shell));
        }
    };
    format!(
        "{command}; sh -c {} sh {status} {} {woke_at}; exec {} -l",
        shell_quote(&failure_report()),
        shell_quote(&command),
        shell_quote(shell)
    )
}

/// Keeps `[A-Za-z0-9_-]`, replaces anything else with `_`.
pub fn sanitize_pane(pane: &str) -> String {
    pane.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn replay_path(state_dir: &str, pane: &str) -> PathBuf {
    Path::new(state_dir).join(format!("replay-{}.txt", sanitize_pane(pane)))
}

/// Arguments of the detached replay process (after the program).
pub fn replay_args(args: &Args, file: &Path) -> Vec<String> {
    let mut v = vec!["replay".to_string(), "--pane".into(), args.pane.clone()];
    if let Some(name) = &args.name {
        v.extend(["--name".to_string(), name.clone()]);
    }
    v.extend([
        "--kind".to_string(),
        args.kind.clone(),
        "--file".into(),
        file.to_string_lossy().into_owned(),
    ]);
    v
}

/// Writes the file atomically: temporary file, then rename.
fn write_replay(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

fn spawn_replay(args: &Args, file: &Path) -> io::Result<()> {
    let mut cmd = Command::new(&args.main_bin);
    cmd.args(replay_args(args, file))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and allowed between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // Not waited for: it outlives the exec below.
    cmd.spawn().map(drop)
}

/// Seconds since the Unix epoch.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The first directory that still exists: the pane's, `$HOME`, `/`.
fn working_dir(cwd: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    [cwd, home.as_str(), "/"]
        .iter()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .find(|d| d.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Saves and replays the Captured input, restores the terminal and replaces
/// this process by the user's shell. Only returns when nothing could be
/// executed.
pub fn run(args: &Args, raw: Option<RawMode>, captured: &[u8]) -> ExitCode {
    let text = printable(captured);
    // Started even with nothing to type: the Replay also gives the pane its
    // name back, which Herdr forgot while the Placeholder held the pane.
    let file = replay_path(&args.state_dir, &args.pane);
    match write_replay(&file, &text) {
        Ok(()) => {
            if let Err(e) = spawn_replay(args, &file) {
                eprintln!("herdr-sleep-ph: cannot start {}: {e}", args.main_bin);
            }
        }
        Err(e) => eprintln!("herdr-sleep-ph: cannot write {}: {e}", file.display()),
    }

    drop(raw);
    let dir = working_dir(&args.cwd);
    if let Err(e) = std::env::set_current_dir(&dir) {
        eprintln!("herdr-sleep-ph: cannot enter {}: {e}", dir.display());
    }

    let base = Path::new(&args.shell)
        .file_name()
        .map_or_else(|| "sh".into(), |n| n.to_string_lossy().into_owned());
    let err = Command::new(&args.shell)
        .arg0(format!("-{base}"))
        .args([
            "-i",
            "-c",
            &wake_script(&args.resume, &args.shell, now_secs()),
        ])
        .env("PWD", &dir)
        .exec();
    eprintln!("herdr-sleep-ph: cannot run {}: {err}", args.shell);

    let err = Command::new(&args.resume[0])
        .args(&args.resume[1..])
        .env("PWD", &dir)
        .exec();
    eprintln!("herdr-sleep-ph: cannot run {}: {err}", args.resume[0]);
    ExitCode::from(127)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::sample;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("claude"), "claude");
        assert_eq!(shell_quote("--resume=a/b.c"), "--resume=a/b.c");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
    }

    #[test]
    fn script_resumes_reports_a_failure_then_falls_back_to_a_login_shell() {
        let resume = vec!["claude".to_string(), "--resume".into(), "s 1".into()];
        let script = wake_script(&resume, "/bin/zsh", 1700000000);
        assert!(script.starts_with("claude --resume 's 1'; sh -c '"));
        assert!(script.ends_with("; exec /bin/zsh -l"));
    }

    #[test]
    fn the_status_is_spelled_in_the_syntax_of_the_shell() {
        let resume = vec!["claude".to_string()];
        let posix = wake_script(&resume, "/bin/zsh", 1700000000);
        assert!(
            posix.contains(" sh $? ") && !posix.contains("$status"),
            "{posix}"
        );
        let login = wake_script(&resume, "-bash", 1700000000);
        assert!(login.contains(" sh $? "), "{login}");
        let fish = wake_script(&resume, "/usr/bin/fish", 1700000000);
        assert!(
            fish.contains(" sh $status ") && !fish.contains("$?"),
            "{fish}"
        );
        // An unknown shell gets a script with no status at all.
        let other = wake_script(&resume, "/usr/bin/nu", 1700000000);
        assert_eq!(other, "claude; exec /usr/bin/nu -l");
    }

    /// Path of `fish`, when it is installed.
    fn fish() -> Option<PathBuf> {
        [
            "/usr/bin/fish",
            "/usr/local/bin/fish",
            "/bin/fish",
            "/opt/homebrew/bin/fish",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    }

    #[test]
    fn the_script_parses_and_reaches_the_login_shell_in_fish() {
        let Some(fish) = fish() else {
            eprintln!("fish is not installed: skipped");
            return;
        };
        for (resume, report) in [(&["true"][..], false), (&["sh", "-c", "exit 3"][..], true)] {
            // Written for fish, with the login shell replaced by `echo` to see it start.
            let resume: Vec<String> = resume.iter().map(|w| w.to_string()).collect();
            let script = script_for(&resume, "/bin/echo", ShellFamily::Fish, now_secs());
            let out = Command::new(&fish)
                .args(["-i", "-c", &script])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let (stdout, stderr) = (
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
            assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
            assert!(!stderr.contains("$?"), "{stderr}");
            assert!(stdout.trim_end().ends_with("-l"), "{stdout}");
            assert_eq!(
                stdout.contains("could not be resumed (exit status 3)"),
                report,
                "{stdout}"
            );
        }
    }

    fn run_script(resume: &[&str]) -> String {
        run_script_woken_at(resume, now_secs())
    }

    fn run_script_woken_at(resume: &[&str], woke_at: u64) -> String {
        let resume: Vec<String> = resume.iter().map(|w| w.to_string()).collect();
        let out = Command::new("sh")
            .args([
                "-c",
                &script_for(&resume, "/bin/echo", ShellFamily::Posix, woke_at),
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn a_quick_failure_is_explained_and_the_shell_still_starts() {
        let stdout = run_script(&["sh", "-c", "exit 3"]);
        assert!(
            stdout.contains("could not be resumed (exit status 3)"),
            "{stdout}"
        );
        assert!(stdout.contains("command tried: sh -c 'exit 3'"), "{stdout}");
        assert!(
            stdout.ends_with("-l\n"),
            "the shell is still exec'd: {stdout}"
        );
    }

    #[test]
    fn a_failure_long_after_the_wake_is_an_agent_the_user_quit() {
        // The pane is old, the Wake is a minute old: nothing is reported.
        let stdout = run_script_woken_at(&["sh", "-c", "exit 3"], now_secs() - 60);
        assert_eq!(stdout, "-l\n");
    }

    #[test]
    fn a_failure_is_reported_whatever_the_age_of_the_pane() {
        // The script runs in a shell that is not new: only the Wake time counts.
        let out = Command::new("sh")
            .args([
                "-c",
                &format!(
                    "sleep 2; {}",
                    script_for(
                        &["sh".into(), "-c".into(), "exit 3".into()],
                        "/bin/echo",
                        ShellFamily::Posix,
                        now_secs()
                    )
                ),
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("could not be resumed"), "{stdout}");
    }

    #[test]
    fn a_successful_resume_prints_nothing() {
        assert_eq!(run_script(&["true"]), "-l\n");
    }

    #[test]
    fn pane_ids_are_sanitised() {
        assert_eq!(sanitize_pane("w1-2_a"), "w1-2_a");
        assert_eq!(sanitize_pane("../a b:c"), "___a_b_c");
        assert_eq!(
            replay_path("/state", "w:1"),
            PathBuf::from("/state/replay-w_1.txt")
        );
    }

    #[test]
    fn replay_command_omits_an_absent_name() {
        let mut a = sample();
        let file = Path::new("/state/replay-w1-2.txt");
        assert_eq!(
            replay_args(&a, file).join(" "),
            "replay --pane w1-2 --name worker --kind claude --file /state/replay-w1-2.txt"
        );
        a.name = None;
        assert_eq!(
            replay_args(&a, file).join(" "),
            "replay --pane w1-2 --kind claude --file /state/replay-w1-2.txt"
        );
    }

    #[test]
    fn replay_file_is_written_atomically() {
        let dir = std::env::temp_dir().join(format!("hsph-handoff-{}", std::process::id()));
        let file = dir.join("sub").join("replay-p.txt");
        write_replay(&file, "hello").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello");
        assert!(!file.with_extension("tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_cwd_falls_back() {
        assert_eq!(working_dir("/"), PathBuf::from("/"));
        let fallback = working_dir("/definitely/not/here");
        assert!(fallback.is_dir());
    }
}

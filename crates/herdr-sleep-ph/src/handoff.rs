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

/// The script the shell runs: the resume command, then a login shell.
pub fn wake_script(resume: &[String], shell: &str) -> String {
    let command: Vec<String> = resume.iter().map(|w| shell_quote(w)).collect();
    format!("{}; exec {} -l", command.join(" "), shell_quote(shell))
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
        .args(["-i", "-c", &wake_script(&args.resume, &args.shell)])
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
    fn script_resumes_then_falls_back_to_a_login_shell() {
        let resume = vec!["claude".to_string(), "--resume".into(), "s 1".into()];
        assert_eq!(
            wake_script(&resume, "/bin/zsh"),
            "claude --resume 's 1'; exec /bin/zsh -l"
        );
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

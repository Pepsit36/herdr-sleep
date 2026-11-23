//! Replay: once the woken Agent is Ready, type into it what the user typed
//! in the Placeholder meanwhile (the Captured input).
//!
//! The Placeholder saves the Captured input in a file and starts
//! `herdr-sleep replay` detached before it launches the Agent. The text comes
//! from the user's keyboard: it is never written to the journal, and it goes
//! through `send_text`, which types it into the Agent, never into a shell.
//!
//! Journal events: `REPLAY <bytes>`, `REPLAY-TIMEOUT`, `WARN`.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;
use crate::journal::Journal;

use super::Env;
use super::wake::{READY_TIMEOUT, await_ready, forget, restore_name};

/// Pause between typing the text and pressing Enter, so a menu opened by the
/// text can settle.
const ENTER_DELAY: Duration = Duration::from_millis(300);

/// What the Placeholder hands over to the Replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayArgs {
    pub pane: String,
    /// Original agent name, restored once the Agent is Ready.
    pub name: Option<String>,
    pub kind: String,
    /// File holding the Captured input.
    pub file: PathBuf,
}

/// How a Replay ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// The text was typed and Enter pressed.
    Replayed { bytes: usize },
    /// The Agent was not Ready in time; the file is kept.
    TimedOut,
    /// The Replay could not be carried out.
    Failed(String),
}

/// Wait for the Agent to be Ready, then type the Captured input into it.
pub fn replay(env: &dyn Env, config: &Config, args: &ReplayArgs) -> ReplayOutcome {
    // The Placeholder's state directory (where the replay file is) is the one
    // the Sleep wrote to. The environment this process inherited from the pane
    // may point elsewhere, so it comes second.
    let Some(state_dir) = args
        .file
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(PathBuf::from)
        .or_else(|| env.state_dir())
    else {
        return ReplayOutcome::Failed("no state directory".into());
    };
    if let Err(error) = std::fs::create_dir_all(&state_dir) {
        return ReplayOutcome::Failed(format!(
            "cannot create the state directory {}: {error}",
            state_dir.display()
        ));
    }
    let journal = Journal::new(&state_dir);
    let pane = args.pane.as_str();

    let text = match std::fs::read_to_string(&args.file) {
        Ok(text) => text,
        Err(error) => {
            return ReplayOutcome::Failed(format!("cannot read {}: {error}", args.file.display()));
        }
    };

    if await_ready(env, config, pane, &args.kind).is_none() {
        journal.log(
            "REPLAY-TIMEOUT",
            &format!("{pane} not ready after {}s", READY_TIMEOUT.as_secs()),
        );
        return ReplayOutcome::TimedOut;
    }
    restore_name(env, &journal, pane, args.name.as_deref());
    forget(&journal, &state_dir, pane);

    if let Err(error) = std::fs::remove_file(&args.file) {
        journal.log(
            "WARN",
            &format!("{pane}: cannot delete the replay file: {error}"),
        );
    }
    let bytes = text.len();
    if bytes > 0 {
        if let Err(error) = env.send_text(pane, &text) {
            return ReplayOutcome::Failed(format!("cannot type the text: {error}"));
        }
        env.sleep(ENTER_DELAY);
        if let Err(error) = env.send_keys(pane, &["enter"]) {
            return ReplayOutcome::Failed(format!("cannot press Enter: {error}"));
        }
    }
    // The draft saved at Sleep comes back last, after what was typed in the
    // Placeholder, and is never submitted.
    let draft_file = super::sleep::draft_path(&state_dir, pane);
    if let Ok(draft) = std::fs::read_to_string(&draft_file) {
        let _ = std::fs::remove_file(&draft_file);
        if !draft.is_empty() {
            if let Err(error) = env.send_text(pane, &draft) {
                return ReplayOutcome::Failed(format!("cannot type the draft back: {error}"));
            }
            journal.log("DRAFT", &format!("{pane} restored bytes={}", draft.len()));
        }
    }
    journal.log("REPLAY", &format!("{pane} bytes={bytes}"));
    ReplayOutcome::Replayed { bytes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::wake::tests::Fake;

    fn setup(name: &str, text: &str) -> (Fake, ReplayArgs) {
        let fake = Fake::new(name);
        std::fs::create_dir_all(&fake.state_dir).unwrap();
        let file = fake.state_dir.join("replay-w1_p2.txt");
        std::fs::write(&file, text).unwrap();
        // The Placeholder starts the Replay at the moment it launches the Agent.
        fake.enter_at.set(Some(Duration::ZERO));
        let args = ReplayArgs {
            pane: "w1:p2".into(),
            name: Some("worker".into()),
            kind: "claude".into(),
            file,
        };
        (fake, args)
    }

    #[test]
    fn types_the_text_once_the_agent_is_ready() {
        let (fake, args) = setup("happy", "fix the build");
        let outcome = replay(&fake, &Config::default(), &args);
        assert_eq!(outcome, ReplayOutcome::Replayed { bytes: 13 });
        let calls = fake.calls();
        let tail: Vec<&str> = calls
            .iter()
            .skip_while(|c| !c.starts_with("rename"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            [
                "rename w1:p2 worker",
                "text w1:p2 fix the build",
                "keys w1:p2 enter"
            ]
        );
        assert!(!args.file.exists());
        // Ready at 3 s, then the 300 ms pause before Enter.
        assert_eq!(fake.clock(), Duration::from_millis(3300));
        assert_eq!(fake.events(), ["REPLAY"]);
        assert!(fake.journal().trim_end().ends_with("REPLAY w1:p2 bytes=13"));
    }

    #[test]
    fn the_saved_draft_is_typed_back_after_the_text_and_never_submitted() {
        let (fake, args) = setup("draft", "fix the build");
        let draft = super::super::sleep::draft_path(&fake.state_dir, "w1:p2");
        std::fs::write(&draft, "half a thought").unwrap();
        replay(&fake, &Config::default(), &args);
        let calls = fake.calls();
        let tail: Vec<&str> = calls
            .iter()
            .skip_while(|c| !c.starts_with("rename"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            [
                "rename w1:p2 worker",
                "text w1:p2 fix the build",
                "keys w1:p2 enter",
                "text w1:p2 half a thought"
            ]
        );
        assert!(!draft.exists());
        assert!(!fake.journal().contains("half a thought"));
    }

    #[test]
    fn the_state_directory_is_the_one_of_the_replay_file() {
        use crate::ops::registry::{Registry, Sleeper};
        let (fake, mut args) = setup("other-dir", "hi");
        // The file lives in the Placeholder's state directory, which is not the
        // one this process's environment names.
        let placeholder_dir = fake.state_dir.join("placeholder");
        std::fs::create_dir_all(&placeholder_dir).unwrap();
        let moved = placeholder_dir.join("replay-w1_p2.txt");
        std::fs::rename(&args.file, &moved).unwrap();
        args.file = moved;
        let sleeper = Sleeper {
            kind: "claude".into(),
            session: "s".into(),
            name: None,
            cwd: "/".into(),
            since_ms: 1,
        };
        Registry::add(&placeholder_dir, "w1:p2", sleeper).unwrap();
        replay(&fake, &Config::default(), &args);
        assert!(Registry::load(&placeholder_dir).panes.is_empty());
        let journal = std::fs::read_to_string(placeholder_dir.join(crate::journal::FILE_NAME));
        assert!(journal.unwrap().contains("REPLAY w1:p2"));
    }

    #[test]
    fn a_ready_agent_leaves_the_registry() {
        use crate::ops::registry::{Registry, Sleeper};
        let (fake, args) = setup("registry", "hi");
        let sleeper = Sleeper {
            kind: "claude".into(),
            session: "s".into(),
            name: None,
            cwd: "/".into(),
            since_ms: 1,
        };
        Registry::add(&fake.state_dir, "w1:p2", sleeper).unwrap();
        replay(&fake, &Config::default(), &args);
        assert!(Registry::load(&fake.state_dir).panes.is_empty());
    }

    #[test]
    fn the_typed_text_is_never_journaled() {
        let (fake, args) = setup("secret", "hunter2 my password");
        replay(&fake, &Config::default(), &args);
        assert!(!fake.journal().contains("hunter2"));
        assert!(!fake.journal().contains("password"));
    }

    #[test]
    fn a_timeout_keeps_the_file_and_types_nothing() {
        let (mut fake, args) = setup("timeout", "hello");
        fake.prompt_after = None;
        let outcome = replay(&fake, &Config::default(), &args);
        assert_eq!(outcome, ReplayOutcome::TimedOut);
        assert_eq!(std::fs::read_to_string(&args.file).unwrap(), "hello");
        assert!(!fake.calls().iter().any(|c| c.starts_with("text")));
        assert!(!fake.calls().iter().any(|c| c.starts_with("keys")));
        assert_eq!(fake.events(), ["REPLAY-TIMEOUT"]);
        assert_eq!(fake.clock(), Duration::from_secs(120));
    }

    #[test]
    fn an_unnamed_pane_skips_the_rename() {
        let (fake, mut args) = setup("unnamed", "hi");
        args.name = None;
        assert_eq!(
            replay(&fake, &Config::default(), &args),
            ReplayOutcome::Replayed { bytes: 2 }
        );
        assert!(!fake.calls().iter().any(|c| c.starts_with("rename")));
    }

    #[test]
    fn a_missing_file_fails_before_waiting() {
        let (fake, args) = setup("nofile", "x");
        std::fs::remove_file(&args.file).unwrap();
        let outcome = replay(&fake, &Config::default(), &args);
        assert!(matches!(outcome, ReplayOutcome::Failed(_)), "{outcome:?}");
        assert!(fake.calls().is_empty());
    }
}

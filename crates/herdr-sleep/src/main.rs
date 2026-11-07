//! `herdr-sleep`: the command line and plugin entry points of Herdr Sleep.

mod cli;

use std::path::PathBuf;
use std::process::ExitCode;

use herdr_sleep::ops::ReplayArgs;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
Usage: herdr-sleep <command> [options]

Herdr Sleep puts idle coding agents to sleep to free memory and wakes them
on demand, keeping their pane, name and session.

Commands:
  sleep <pane|name> [--force]
                   Put an idle agent to sleep. Safety guards apply; --force
                   skips them (use with care)

  wake <pane|name> [--no-wait] [--json]
                   Wake a sleeping agent and wait until it is ready (its
                   prompt is back); --no-wait returns right away. --json
                   prints woke, pane, name, ms and reason

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit

Exit codes:
  0  success (asleep, or dry run)
  1  refused, aborted or failed
  2  usage error (wake: also an agent not ready after 120 s)

wake exits 0 when the agent was woken, is not asleep or was woken a moment
ago, and 1 when no such agent exists.
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Help,
    Version,
    Sleep {
        target: String,
        force: bool,
    },
    Wake {
        target: String,
        wait: bool,
        json: bool,
    },
    /// Internal: started by the Placeholder to type the Captured input into
    /// the woken Agent. Not part of the usage text.
    ///
    /// `herdr-sleep replay --pane P [--name N] --kind K --file F`
    Replay(ReplayArgs),
    Unknown(String),
    /// A known command used wrongly; the message says how.
    Usage(String),
}

fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Invocation {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        None | Some("-h") | Some("--help") | Some("help") => Invocation::Help,
        Some("-V") | Some("--version") | Some("version") => Invocation::Version,
        Some("sleep") => parse_sleep(args),
        Some("wake") => parse_wake(args),
        Some("replay") => parse_replay(args),
        Some(other) => Invocation::Unknown(other.to_string()),
    }
}

fn parse_sleep(args: impl Iterator<Item = String>) -> Invocation {
    let mut target = None;
    let mut force = false;
    for arg in args {
        if arg == "--force" {
            force = true;
        } else if arg.starts_with('-') {
            return Invocation::Usage(format!("sleep: unknown option '{arg}'"));
        } else if target.replace(arg).is_some() {
            return Invocation::Usage("sleep: expects a single pane or name".into());
        }
    }
    match target {
        Some(target) => Invocation::Sleep { target, force },
        None => Invocation::Usage("sleep: expects a pane or an agent name".into()),
    }
}

fn parse_wake(args: impl Iterator<Item = String>) -> Invocation {
    let mut target = None;
    let mut wait = true;
    let mut json = false;
    for arg in args {
        if arg == "--no-wait" {
            wait = false;
        } else if arg == "--json" {
            json = true;
        } else if arg.starts_with('-') {
            return Invocation::Usage(format!("wake: unknown option '{arg}'"));
        } else if target.replace(arg).is_some() {
            return Invocation::Usage("wake: expects a single pane or name".into());
        }
    }
    match target {
        Some(target) => Invocation::Wake { target, wait, json },
        None => Invocation::Usage("wake: expects a pane or an agent name".into()),
    }
}

fn parse_replay(args: impl Iterator<Item = String>) -> Invocation {
    let (mut pane, mut name, mut kind, mut file) = (None, None, None, None);
    let mut args = args;
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--pane" => &mut pane,
            "--name" => &mut name,
            "--kind" => &mut kind,
            "--file" => &mut file,
            other => return Invocation::Usage(format!("replay: unknown argument '{other}'")),
        };
        match args.next() {
            Some(value) => *slot = Some(value),
            None => return Invocation::Usage(format!("replay: {flag} expects a value")),
        }
    }
    match (pane, kind, file) {
        (Some(pane), Some(kind), Some(file)) => Invocation::Replay(ReplayArgs {
            pane,
            name,
            kind,
            file: PathBuf::from(file),
        }),
        _ => Invocation::Usage("replay: expects --pane, --kind and --file".into()),
    }
}

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)) {
        Invocation::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Invocation::Version => {
            println!("herdr-sleep {VERSION}");
            ExitCode::SUCCESS
        }
        Invocation::Sleep { target, force } => cli::sleep(&target, force),
        Invocation::Wake { target, wait, json } => cli::wake(&target, wait, json),
        Invocation::Replay(args) => cli::replay_command(&args),
        Invocation::Usage(message) => {
            eprintln!("herdr-sleep: {message}\n");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
        Invocation::Unknown(command) => {
            eprintln!("herdr-sleep: unknown command '{command}'\n");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Invocation {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn no_argument_prints_help() {
        assert_eq!(parse(&[]), Invocation::Help);
    }

    #[test]
    fn version_flags() {
        assert_eq!(parse(&["--version"]), Invocation::Version);
        assert_eq!(parse(&["-V"]), Invocation::Version);
    }

    #[test]
    fn sleep_takes_a_target_and_an_optional_force() {
        assert_eq!(
            parse(&["sleep", "w1-2"]),
            Invocation::Sleep {
                target: "w1-2".into(),
                force: false
            }
        );
        assert_eq!(
            parse(&["sleep", "--force", "worker"]),
            Invocation::Sleep {
                target: "worker".into(),
                force: true
            }
        );
        assert!(matches!(parse(&["sleep"]), Invocation::Usage(_)));
        assert!(matches!(parse(&["sleep", "a", "b"]), Invocation::Usage(_)));
        assert!(matches!(parse(&["sleep", "-x", "a"]), Invocation::Usage(_)));
    }

    #[test]
    fn wake_waits_unless_told_not_to() {
        assert_eq!(
            parse(&["wake", "worker"]),
            Invocation::Wake {
                target: "worker".into(),
                wait: true,
                json: false
            }
        );
        assert_eq!(
            parse(&["wake", "--no-wait", "--json", "w1:p2"]),
            Invocation::Wake {
                target: "w1:p2".into(),
                wait: false,
                json: true
            }
        );
        assert!(matches!(parse(&["wake"]), Invocation::Usage(_)));
        assert!(matches!(parse(&["wake", "a", "b"]), Invocation::Usage(_)));
        assert!(matches!(parse(&["wake", "-x", "a"]), Invocation::Usage(_)));
    }

    #[test]
    fn replay_takes_the_placeholder_arguments() {
        assert_eq!(
            parse(&[
                "replay", "--pane", "w1:p2", "--name", "worker", "--kind", "claude", "--file",
                "/s/r.txt"
            ]),
            Invocation::Replay(ReplayArgs {
                pane: "w1:p2".into(),
                name: Some("worker".into()),
                kind: "claude".into(),
                file: PathBuf::from("/s/r.txt"),
            })
        );
        assert!(matches!(
            parse(&["replay", "--pane", "p", "--kind", "claude", "--file", "f"]),
            Invocation::Replay(ReplayArgs { name: None, .. })
        ));
        assert!(matches!(
            parse(&["replay", "--pane", "p"]),
            Invocation::Usage(_)
        ));
        assert!(matches!(parse(&["replay", "--pane"]), Invocation::Usage(_)));
        assert!(matches!(
            parse(&["replay", "--x", "1"]),
            Invocation::Usage(_)
        ));
    }

    #[test]
    fn unknown_command_is_reported() {
        assert_eq!(parse(&["nap"]), Invocation::Unknown("nap".into()));
    }
}

//! Command line parsing: turns the arguments into an [`Invocation`].
//!
//! The parser never prints and never exits; `main` decides what to do with
//! the result. A known command used wrongly gives [`Invocation::Usage`], an
//! unknown one [`Invocation::Unknown`]. `--help` after a command wins over
//! any other mistake on the line, and `--` ends the options so a target may
//! start with a dash.

use std::path::PathBuf;

use herdr_sleep::ops::ReplayArgs;

/// Every command name, internal ones included.
pub const COMMANDS: [&str; 11] = [
    "watch",
    "sweep",
    "sleep",
    "wake",
    "keep-awake",
    "status",
    "config",
    "event",
    "replay",
    "ensure-watcher",
    "restore",
];

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// The main help (`None`) or the help of one command.
    Help(Option<&'static str>),
    Version,
    Sleep {
        /// `None` when the action context must provide the pane.
        target: Option<String>,
        force: bool,
        /// Put the Saved session of a pane with no live Agent to sleep.
        from_session: bool,
        /// Seconds to keep trying while the Agent is not yet Settled.
        wait: Option<u64>,
        json: bool,
    },
    Wake {
        target: Option<String>,
        wait: bool,
        json: bool,
    },
    /// Toggle (or set with `--on` / `--off`) the keep-awake icon of a tab.
    KeepAwake {
        target: Option<String>,
        state: Option<bool>,
    },
    Status {
        json: bool,
    },
    Sweep {
        /// Do not wait for the Idle time.
        now: bool,
        json: bool,
    },
    Config {
        json: bool,
    },
    Watch {
        detach: bool,
    },
    EnsureWatcher {
        json: bool,
    },
    Restore {
        json: bool,
    },
    /// The focus hooks.
    Event,
    /// Internal: started by the Placeholder to type the Captured input into
    /// the woken Agent. Not part of the usage text.
    ///
    /// `herdr-sleep replay --pane P [--name N] --kind K --file F`
    Replay(ReplayArgs),
    Unknown(String),
    /// A known command used wrongly; the message says how.
    Usage(String),
}

/// The canonical name of a command, `None` when there is no such command.
fn command_name(word: &str) -> Option<&'static str> {
    COMMANDS.into_iter().find(|name| *name == word)
}

/// The arguments of one command, split into flags and positionals.
struct Split {
    flags: Vec<&'static str>,
    positionals: Vec<String>,
    help: bool,
    /// The first mistake found, reported unless `--help` was asked.
    error: Option<String>,
}

impl Split {
    fn has(&self, flag: &str) -> bool {
        self.flags.contains(&flag)
    }
}

/// Split `args` for `command`, which accepts the flags in `allowed`.
fn split(command: &str, args: impl Iterator<Item = String>, allowed: &[&'static str]) -> Split {
    let mut out = Split {
        flags: Vec::new(),
        positionals: Vec::new(),
        help: false,
        error: None,
    };
    let mut options_ended = false;
    for arg in args {
        if options_ended || arg == "-" || !arg.starts_with('-') {
            out.positionals.push(arg);
        } else if arg == "--" {
            options_ended = true;
        } else if arg == "-h" || arg == "--help" {
            out.help = true;
        } else if let Some(flag) = allowed.iter().find(|flag| **flag == arg) {
            out.flags.push(flag);
        } else if out.error.is_none() {
            out.error = Some(format!("{command}: unknown option '{arg}'"));
        }
    }
    out
}

/// Resolve the help request and the mistakes of `split`, then hand the
/// positionals (at most `max`) to `build`.
fn finish(
    command: &'static str,
    mut parsed: Split,
    max: usize,
    build: impl FnOnce(&Split, Vec<String>) -> Invocation,
) -> Invocation {
    if parsed.help {
        return Invocation::Help(Some(command));
    }
    if let Some(error) = parsed.error {
        return Invocation::Usage(error);
    }
    if parsed.positionals.len() > max {
        let message = match max {
            0 => format!(
                "{command}: takes no argument, got '{}'",
                parsed.positionals[0]
            ),
            _ => format!("{command}: expects a single pane or name"),
        };
        return Invocation::Usage(message);
    }
    let positionals = std::mem::take(&mut parsed.positionals);
    build(&parsed, positionals)
}

/// Parse the arguments that follow the program name.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Invocation {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Invocation::Help(None);
    };
    match first.as_str() {
        "-h" | "--help" => return Invocation::Help(None),
        "-V" | "--version" | "version" => return Invocation::Version,
        "help" => return parse_help(args),
        _ => {}
    }
    let Some(command) = command_name(&first) else {
        return Invocation::Unknown(first);
    };
    match command {
        "sleep" => parse_sleep(args),
        "wake" => finish(
            command,
            split(command, args, &["--no-wait", "--json"]),
            1,
            |p, mut pos| Invocation::Wake {
                target: pos.pop(),
                wait: !p.has("--no-wait"),
                json: p.has("--json"),
            },
        ),
        "keep-awake" => {
            let parsed = split(command, args, &["--on", "--off"]);
            let parsed = match (
                parsed.error.is_none(),
                parsed.has("--on") && parsed.has("--off"),
            ) {
                (true, true) => Split {
                    error: Some("keep-awake: choose --on or --off, not both".into()),
                    ..parsed
                },
                _ => parsed,
            };
            finish(command, parsed, 1, |p, mut pos| Invocation::KeepAwake {
                target: pos.pop(),
                state: match (p.has("--on"), p.has("--off")) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                },
            })
        }
        "status" => finish(command, split(command, args, &["--json"]), 0, |p, _| {
            Invocation::Status {
                json: p.has("--json"),
            }
        }),
        "sweep" => finish(
            command,
            split(command, args, &["--now", "--json"]),
            0,
            |p, _| Invocation::Sweep {
                now: p.has("--now"),
                json: p.has("--json"),
            },
        ),
        "config" => finish(command, split(command, args, &["--json"]), 0, |p, _| {
            Invocation::Config {
                json: p.has("--json"),
            }
        }),
        "ensure-watcher" => finish(command, split(command, args, &["--json"]), 0, |p, _| {
            Invocation::EnsureWatcher {
                json: p.has("--json"),
            }
        }),
        "restore" => finish(command, split(command, args, &["--json"]), 0, |p, _| {
            Invocation::Restore {
                json: p.has("--json"),
            }
        }),
        "watch" => finish(command, split(command, args, &["--detach"]), 0, |p, _| {
            Invocation::Watch {
                detach: p.has("--detach"),
            }
        }),
        "event" => finish(command, split(command, args, &[]), 0, |_, _| {
            Invocation::Event
        }),
        _ => parse_replay(args),
    }
}

/// Seconds `sleep --wait` keeps trying when no value is given.
pub const DEFAULT_WAIT_SECONDS: u64 = 120;

/// `sleep [<pane|name>] [--force] [--from-session] [--wait[=SECONDS]] [--json]`.
fn parse_sleep(args: impl Iterator<Item = String>) -> Invocation {
    let mut wait = None;
    let mut wait_error = None;
    let mut rest = Vec::new();
    let mut options_ended = false;
    for arg in args {
        if options_ended || !arg.starts_with("--wait") {
            options_ended |= arg == "--";
            rest.push(arg);
        } else if arg == "--wait" {
            wait = Some(DEFAULT_WAIT_SECONDS);
        } else if let Some(value) = arg.strip_prefix("--wait=") {
            match value.parse::<u64>() {
                Ok(seconds) if seconds > 0 => wait = Some(seconds),
                _ => {
                    wait_error.get_or_insert(format!(
                        "sleep: --wait expects a number of seconds above 0, got '{value}'"
                    ));
                }
            }
        } else {
            rest.push(arg);
        }
    }
    let parsed = split(
        "sleep",
        rest.into_iter(),
        &["--force", "--from-session", "--json"],
    );
    let parsed = match (parsed.error.is_none(), wait_error) {
        (true, Some(error)) => Split {
            error: Some(error),
            ..parsed
        },
        _ => parsed,
    };
    finish("sleep", parsed, 1, |p, mut pos| Invocation::Sleep {
        target: pos.pop(),
        force: p.has("--force"),
        from_session: p.has("--from-session"),
        wait,
        json: p.has("--json"),
    })
}

/// `help` alone, or `help <command>`.
fn parse_help(mut args: impl Iterator<Item = String>) -> Invocation {
    let Some(word) = args.next() else {
        return Invocation::Help(None);
    };
    if args.next().is_some() {
        return Invocation::Usage("help: expects a single command".into());
    }
    match command_name(&word) {
        Some(command) => Invocation::Help(Some(command)),
        None => Invocation::Unknown(word),
    }
}

fn parse_replay(args: impl Iterator<Item = String>) -> Invocation {
    let (mut pane, mut name, mut kind, mut file) = (None, None, None, None);
    let mut args = args;
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "-h" | "--help" => return Invocation::Help(Some("replay")),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Invocation {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    fn usage(args: &[&str]) -> String {
        match parse(args) {
            Invocation::Usage(message) => message,
            other => panic!("expected a usage error for {args:?}, got {other:?}"),
        }
    }

    fn sleep(target: Option<&str>, force: bool, json: bool) -> Invocation {
        Invocation::Sleep {
            target: target.map(str::to_string),
            force,
            from_session: false,
            wait: None,
            json,
        }
    }

    fn wake(target: Option<&str>, wait: bool, json: bool) -> Invocation {
        Invocation::Wake {
            target: target.map(str::to_string),
            wait,
            json,
        }
    }

    #[test]
    fn keep_awake_parses_its_target_and_state() {
        let keep = |target: Option<&str>, state| Invocation::KeepAwake {
            target: target.map(str::to_string),
            state,
        };
        assert_eq!(parse(&["keep-awake"]), keep(None, None));
        assert_eq!(parse(&["keep-awake", "w1:p2"]), keep(Some("w1:p2"), None));
        assert_eq!(
            parse(&["keep-awake", "--on", "w"]),
            keep(Some("w"), Some(true))
        );
        assert_eq!(parse(&["keep-awake", "--off"]), keep(None, Some(false)));
        assert_eq!(
            usage(&["keep-awake", "--on", "--off"]),
            "keep-awake: choose --on or --off, not both"
        );
        assert_eq!(
            parse(&["keep-awake", "--help"]),
            Invocation::Help(Some("keep-awake"))
        );
    }

    #[test]
    fn no_argument_and_help_spellings() {
        for args in [&[][..], &["-h"], &["--help"], &["help"]] {
            assert_eq!(parse(args), Invocation::Help(None), "{args:?}");
        }
    }

    #[test]
    fn sleep_takes_from_session_and_an_optional_wait() {
        let full = |wait, from_session| Invocation::Sleep {
            target: Some("w1:p2".into()),
            force: false,
            from_session,
            wait,
            json: false,
        };
        assert_eq!(
            parse(&["sleep", "w1:p2", "--from-session"]),
            full(None, true)
        );
        assert_eq!(parse(&["sleep", "--wait", "w1:p2"]), full(Some(120), false));
        assert_eq!(
            parse(&["sleep", "w1:p2", "--wait=45"]),
            full(Some(45), false)
        );
        for bad in ["--wait=0", "--wait=x", "--wait="] {
            assert!(usage(&["sleep", bad]).contains("--wait expects"), "{bad}");
        }
        // After `--`, a target may look like the option.
        assert_eq!(
            parse(&["sleep", "--", "--wait"]),
            Invocation::Sleep {
                target: Some("--wait".into()),
                force: false,
                from_session: false,
                wait: None,
                json: false,
            }
        );
    }

    #[test]
    fn sweep_takes_now() {
        assert_eq!(
            parse(&["sweep", "--now", "--json"]),
            Invocation::Sweep {
                now: true,
                json: true
            }
        );
    }

    #[test]
    fn help_of_a_command() {
        assert_eq!(parse(&["help", "sleep"]), Invocation::Help(Some("sleep")));
        assert_eq!(parse(&["wake", "--help"]), Invocation::Help(Some("wake")));
        assert_eq!(parse(&["status", "-h"]), Invocation::Help(Some("status")));
        assert_eq!(
            parse(&["replay", "--help"]),
            Invocation::Help(Some("replay"))
        );
        assert_eq!(parse(&["help", "nap"]), Invocation::Unknown("nap".into()));
        assert!(usage(&["help", "a", "b"]).contains("single command"));
    }

    #[test]
    fn help_wins_over_other_mistakes() {
        assert_eq!(
            parse(&["sleep", "--bogus", "a", "b", "--help"]),
            Invocation::Help(Some("sleep"))
        );
    }

    #[test]
    fn every_command_answers_help() {
        for command in COMMANDS {
            assert_eq!(
                parse(&[command, "--help"]),
                Invocation::Help(Some(command)),
                "{command}"
            );
        }
    }

    #[test]
    fn version_flags() {
        for args in [&["--version"][..], &["-V"], &["version"]] {
            assert_eq!(parse(args), Invocation::Version, "{args:?}");
        }
    }

    #[test]
    fn unknown_command() {
        assert_eq!(parse(&["nap"]), Invocation::Unknown("nap".into()));
        assert_eq!(parse(&["--nap"]), Invocation::Unknown("--nap".into()));
    }

    #[test]
    fn sleep_target_and_flags() {
        assert_eq!(parse(&["sleep"]), sleep(None, false, false));
        assert_eq!(parse(&["sleep", "w1-2"]), sleep(Some("w1-2"), false, false));
        assert_eq!(
            parse(&["sleep", "--force", "worker"]),
            sleep(Some("worker"), true, false)
        );
        assert_eq!(
            parse(&["sleep", "worker", "--json", "--force"]),
            sleep(Some("worker"), true, true)
        );
        assert_eq!(parse(&["sleep", "--json"]), sleep(None, false, true));
    }

    #[test]
    fn sleep_mistakes() {
        assert_eq!(
            usage(&["sleep", "a", "b"]),
            "sleep: expects a single pane or name"
        );
        assert_eq!(usage(&["sleep", "-x", "a"]), "sleep: unknown option '-x'");
        assert_eq!(
            usage(&["sleep", "--no-wait"]),
            "sleep: unknown option '--no-wait'"
        );
    }

    #[test]
    fn wake_target_and_flags() {
        assert_eq!(parse(&["wake"]), wake(None, true, false));
        assert_eq!(
            parse(&["wake", "worker"]),
            wake(Some("worker"), true, false)
        );
        assert_eq!(
            parse(&["wake", "--no-wait", "--json", "w1:p2"]),
            wake(Some("w1:p2"), false, true)
        );
        assert_eq!(parse(&["wake", "--no-wait"]), wake(None, false, false));
    }

    #[test]
    fn wake_mistakes() {
        assert_eq!(
            usage(&["wake", "a", "b"]),
            "wake: expects a single pane or name"
        );
        assert_eq!(usage(&["wake", "-x"]), "wake: unknown option '-x'");
        assert_eq!(
            usage(&["wake", "--force"]),
            "wake: unknown option '--force'"
        );
    }

    #[test]
    fn double_dash_ends_the_options() {
        assert_eq!(
            parse(&["sleep", "--", "--odd-name"]),
            sleep(Some("--odd-name"), false, false)
        );
        assert_eq!(
            parse(&["wake", "--no-wait", "--", "-1"]),
            wake(Some("-1"), false, false)
        );
        // After `--` a flag spelling is a positional, so two of them are too many.
        assert!(matches!(
            parse(&["sleep", "--", "--force", "x"]),
            Invocation::Usage(_)
        ));
        assert_eq!(parse(&["sleep", "--"]), sleep(None, false, false));
        assert_eq!(
            usage(&["status", "--", "extra"]),
            "status: takes no argument, got 'extra'"
        );
    }

    #[test]
    fn a_lone_dash_is_a_positional() {
        assert_eq!(parse(&["sleep", "-"]), sleep(Some("-"), false, false));
    }

    #[test]
    fn status_sweep_config_ensure_watcher_take_json_only() {
        assert_eq!(parse(&["status"]), Invocation::Status { json: false });
        assert_eq!(
            parse(&["status", "--json"]),
            Invocation::Status { json: true }
        );
        assert_eq!(
            parse(&["sweep"]),
            Invocation::Sweep {
                now: false,
                json: false
            }
        );
        assert_eq!(
            parse(&["sweep", "--json"]),
            Invocation::Sweep {
                now: false,
                json: true
            }
        );
        assert_eq!(parse(&["config"]), Invocation::Config { json: false });
        assert_eq!(
            parse(&["config", "--json"]),
            Invocation::Config { json: true }
        );
        assert_eq!(
            parse(&["ensure-watcher"]),
            Invocation::EnsureWatcher { json: false }
        );
        assert_eq!(
            parse(&["ensure-watcher", "--json"]),
            Invocation::EnsureWatcher { json: true }
        );
        for command in ["status", "sweep", "config", "ensure-watcher"] {
            assert_eq!(
                usage(&[command, "x"]),
                format!("{command}: takes no argument, got 'x'")
            );
            assert_eq!(
                usage(&[command, "--detach"]),
                format!("{command}: unknown option '--detach'")
            );
        }
    }

    #[test]
    fn restore_options() {
        assert_eq!(parse(&["restore"]), Invocation::Restore { json: false });
        assert_eq!(
            parse(&["restore", "--json"]),
            Invocation::Restore { json: true }
        );
        assert_eq!(
            usage(&["restore", "x"]),
            "restore: takes no argument, got 'x'"
        );
        assert_eq!(
            usage(&["restore", "--force"]),
            "restore: unknown option '--force'"
        );
    }

    #[test]
    fn watch_options() {
        assert_eq!(parse(&["watch"]), Invocation::Watch { detach: false });
        assert_eq!(
            parse(&["watch", "--detach"]),
            Invocation::Watch { detach: true }
        );
        assert_eq!(usage(&["watch", "-x"]), "watch: unknown option '-x'");
        assert_eq!(
            usage(&["watch", "now"]),
            "watch: takes no argument, got 'now'"
        );
        assert_eq!(
            usage(&["watch", "--json"]),
            "watch: unknown option '--json'"
        );
    }

    #[test]
    fn event_takes_no_argument() {
        assert_eq!(parse(&["event"]), Invocation::Event);
        assert_eq!(usage(&["event", "x"]), "event: takes no argument, got 'x'");
        assert_eq!(
            usage(&["event", "--json"]),
            "event: unknown option '--json'"
        );
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
    }

    #[test]
    fn replay_mistakes() {
        assert_eq!(
            usage(&["replay", "--pane", "p"]),
            "replay: expects --pane, --kind and --file"
        );
        assert_eq!(
            usage(&["replay", "--pane"]),
            "replay: --pane expects a value"
        );
        assert_eq!(
            usage(&["replay", "--x", "1"]),
            "replay: unknown argument '--x'"
        );
        assert_eq!(
            usage(&["replay"]),
            "replay: expects --pane, --kind and --file"
        );
    }
}

//! `herdr-sleep`: the command line and plugin entry points of Herdr Sleep.

mod cli;

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
Usage: herdr-sleep <command> [options]

Herdr Sleep puts idle coding agents to sleep to free memory and wakes them
on demand, keeping their pane, name and session.

Commands:
  sleep <pane|name> [--force]
                   Put an idle agent to sleep. Safety guards apply; --force
                   skips them (use with care)

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit

Exit codes:
  0  success (asleep, or dry run)
  1  refused, aborted or failed
  2  usage error
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
    fn unknown_command_is_reported() {
        assert_eq!(parse(&["nap"]), Invocation::Unknown("nap".into()));
    }
}

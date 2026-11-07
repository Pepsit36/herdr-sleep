//! `herdr-sleep`: the command line and plugin entry points of Herdr Sleep.

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
Usage: herdr-sleep <command> [options]

Herdr Sleep puts idle coding agents to sleep to free memory and wakes them
on demand, keeping their pane, name and session.

Commands:
  (none yet: this build only knows --version and --help)

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit

Exit codes:
  0  success
  2  usage error
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Help,
    Version,
    Unknown(String),
}

fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Invocation {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        None | Some("-h") | Some("--help") | Some("help") => Invocation::Help,
        Some("-V") | Some("--version") | Some("version") => Invocation::Version,
        Some(other) => Invocation::Unknown(other.to_string()),
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
    fn unknown_command_is_reported() {
        assert_eq!(parse(&["nap"]), Invocation::Unknown("nap".into()));
    }
}

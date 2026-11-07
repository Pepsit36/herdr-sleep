//! `cargo xtask`: repository tooling. Run with `cargo xtask <command>`.

mod changelog;
mod cli;
mod commands;
mod commit;
mod date;
mod entries;
mod git;
mod plan;
mod version;

use std::process::ExitCode;

use cli::{Failure, Outcome};

const USAGE: &str = "\
Usage: cargo xtask <command> [options]

Commands:
  changelog <subcommand>      Versions and CHANGELOG.md: version, stamp, check,
                              coverage, unreleased (`changelog --help` for details)
  canary-version SHORT_SHA    Print the prerelease version of a canary build
  release-backlog             Fail while main stamps a version that has no tag

Options:
  -h, --help       Print this help and exit

Exit codes: 0 ok, 1 failure (explained on standard error), 2 usage,
3 nothing to publish (`changelog version` only).
";

fn dispatch(args: &[String]) -> Outcome {
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => {
            print!("{USAGE}");
            return Ok(0);
        }
    };
    match command {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "changelog" => commands::changelog(rest),
        "canary-version" if cli::wants_help(rest) => {
            print!("{}", commands::canary::USAGE);
            Ok(0)
        }
        "canary-version" => commands::canary::run(rest),
        "release-backlog" if cli::wants_help(rest) => {
            print!("{}", commands::backlog::USAGE);
            Ok(0)
        }
        "release-backlog" => commands::backlog::run(rest),
        other => Err(Failure::Usage(format!("unknown command '{other}'"))),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(code) => ExitCode::from(code),
        Err(Failure::Usage(message)) => {
            eprintln!("xtask: {message}\n");
            let usage = match args.first().map(String::as_str) {
                Some("changelog") => commands::CHANGELOG_USAGE,
                Some("canary-version") => commands::canary::USAGE,
                Some("release-backlog") => commands::backlog::USAGE,
                _ => USAGE,
            };
            eprint!("{usage}");
            ExitCode::from(2)
        }
        Err(Failure::Error(message)) => {
            eprintln!("{message}");
            ExitCode::from(1)
        }
    }
}

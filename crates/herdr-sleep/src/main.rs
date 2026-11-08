//! `herdr-sleep`: the command line and plugin entry points of Herdr Sleep.
//!
//! This file parses the arguments into an [`Invocation`] and dispatches it;
//! the parser lives in [`args`], the texts in [`help`], the commands in
//! [`cli`].

mod args;
mod cli;
mod help;

use std::process::ExitCode;

use args::{Invocation, parse_args};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)) {
        Invocation::Help(topic) => {
            print!("{}", help::text(topic));
            ExitCode::SUCCESS
        }
        Invocation::Version => {
            println!("herdr-sleep {VERSION}");
            ExitCode::SUCCESS
        }
        Invocation::Sleep {
            target,
            force,
            json,
        } => cli::sleep(target.as_deref(), force, json),
        Invocation::Wake { target, wait, json } => cli::wake(target.as_deref(), wait, json),
        Invocation::Status { json } => cli::status(json),
        Invocation::Sweep { json } => cli::sweep(json),
        Invocation::Config { json } => cli::config(json),
        Invocation::Watch { detach } => cli::watch(detach),
        Invocation::EnsureWatcher { json } => cli::ensure_watcher(json),
        Invocation::Event => cli::event(),
        Invocation::Replay(args) => cli::replay_command(&args),
        Invocation::Usage(message) => usage_failure(&message),
        Invocation::Unknown(command) => usage_failure(&format!("unknown command '{command}'")),
    }
}

/// The message, then the main help, on stderr; exit code 2.
fn usage_failure(message: &str) -> ExitCode {
    eprintln!("herdr-sleep: {message}\n");
    eprint!("{}", help::MAIN);
    ExitCode::from(2)
}

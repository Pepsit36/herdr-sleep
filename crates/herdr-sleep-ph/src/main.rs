//! `herdr-sleep-ph`: the Placeholder that occupies a sleeping pane.
//!
//! This crate depends on `libc` only. Everything it needs from the system
//! (raw terminal mode, `poll`, `exec`) is reached through `libc` directly.

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
Usage: herdr-sleep-ph [options] -- <resume command...>

The Placeholder is started by herdr-sleep inside a sleeping pane. It is not
meant to be run by hand.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("-V") | Some("--version") => {
            println!("herdr-sleep-ph {VERSION}");
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

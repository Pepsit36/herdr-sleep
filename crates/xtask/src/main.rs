//! `cargo xtask`: repository tooling. Run with `cargo xtask <command>`.

use std::process::ExitCode;

const USAGE: &str = "\
Usage: cargo xtask <command> [options]

Commands:
  (none yet)

Options:
  -h, --help       Print this help and exit
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("xtask: unknown command '{other}'\n");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

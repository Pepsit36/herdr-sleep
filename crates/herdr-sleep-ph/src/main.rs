//! `herdr-sleep-ph`: the Placeholder that occupies a sleeping pane.
//!
//! This crate depends on `libc` only. Everything it needs from the system
//! (raw terminal mode, `poll`, `exec`) is reached through `libc` directly.

mod args;
mod handoff;
mod input;
mod report;
mod screen;
mod tty;

use args::{Parsed, USAGE};
use std::io::Write;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let args = match args::parse(argv.get(1..).unwrap_or_default()) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("herdr-sleep-ph {VERSION}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(args)) => args,
        Err(message) => {
            eprintln!("herdr-sleep-ph: {message}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    tty::catch_stop_signals();
    // A terminal whose settings cannot be changed still gets its keys, in cooked mode.
    let is_tty = tty::is_tty();
    let raw = tty::RawMode::enter();
    let mut reporter = report::Reporter::new(&args);

    let preview = args.preview.as_deref().and_then(screen::load_preview);
    let banner = screen::Banner {
        icon: &args.icon,
        name: args.name.as_deref(),
        kind: &args.kind,
        session: &args.session,
    };
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(screen::render(preview.as_deref(), &banner).as_bytes());
    let _ = stdout.flush();

    // Keys typed while Herdr is told are kept by the terminal for `capture`.
    reporter.declare();
    let captured = tty::capture(is_tty);
    reporter.release();
    // The terminal went away without a key: there is nobody to wake it for.
    let Some(captured) = captured else {
        return ExitCode::from(1);
    };
    handoff::run(&args, raw, &captured)
}

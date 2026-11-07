//! `changelog unreleased`: the entry body the counted commits suggest.

use crate::cli::{Options, Outcome};
use crate::commands::version::counting;
use crate::entries::{self, NO_CHANGES};

pub fn run(args: &[String]) -> Outcome {
    let options = Options::parse(args, &["--since", "--base", "--default-branch"])?;
    options.no_positional()?;
    let counting = counting(&options)?;
    match entries::body(&counting.range.commits()) {
        Some(body) => print!("{body}"),
        None => println!("{NO_CHANGES}"),
    }
    Ok(0)
}

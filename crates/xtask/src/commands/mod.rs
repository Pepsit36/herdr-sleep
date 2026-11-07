//! The commands behind `cargo xtask`.

pub mod backlog;
pub mod canary;
pub mod check;
pub mod coverage;
pub mod stamp;
pub mod unreleased;
pub mod version;

use crate::cli::{Failure, Outcome, usage, wants_help};

pub const CHANGELOG_USAGE: &str = "\
Usage: cargo xtask changelog <subcommand> [options]

Versions are computed from the non-merge commits a branch adds on top of the
default branch: `feat:` publishes a minor, `fix:` a patch, a breaking marker
(`type!:` or a `BREAKING CHANGE:` body line) a major, every other type nothing.
While the repository has no `vX.Y.Z` tag at all, the first release is 0.1.0.

Subcommands:
  version [--since REF] [--base REF]
      Print the version the next publication will carry, and nothing else, on
      standard output. Exit 3 when nothing publishes. Where the counting starts
      and the base it uses are reported on standard error.
  stamp [--date YYYY-MM-DD] [--base REF]
      Turn `## [Unreleased]` into `## [X.Y.Z] - date` and reopen an empty one
      above. Run again after a rebase or a new commit: it corrects the number
      and the date and folds the new entries in.
  check [--base REF]
      Fail when the branch publishes a version CHANGELOG.md does not carry on
      top, when a section is stamped with no publishable commit behind it, or
      when a section inherited from the default branch was changed.
  coverage
      Fail when a tag reachable from HEAD has no stamped section describing it.
  unreleased [--since REF]
      Print the entry body the counted commits suggest, for authors.

Options:
  --base REF, --default-branch REF
      The ref the branch is measured against. Default: origin/main, then main.
  --since REF
      Count the commits since REF instead (what the release job reads).
  -h, --help
      Print this help and exit.

Exit codes: 0 ok, 1 failure (explained on standard error), 2 usage,
3 nothing to publish (`version` only).
";

pub fn changelog(args: &[String]) -> Outcome {
    let (subcommand, rest) = match args.split_first() {
        Some((subcommand, rest)) => (subcommand.as_str(), rest),
        None => return Err(usage("changelog needs a subcommand")),
    };
    if subcommand == "-h" || subcommand == "--help" || subcommand == "help" {
        print!("{CHANGELOG_USAGE}");
        return Ok(0);
    }
    if wants_help(rest) {
        print!("{CHANGELOG_USAGE}");
        return Ok(0);
    }
    match subcommand {
        "version" => version::run(rest),
        "stamp" => stamp::run(rest),
        "check" => check::run(rest),
        "coverage" => coverage::run(rest),
        "unreleased" => unreleased::run(rest),
        other => Err(Failure::Usage(format!(
            "unknown changelog subcommand '{other}'"
        ))),
    }
}

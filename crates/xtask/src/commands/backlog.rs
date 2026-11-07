//! `release-backlog`: fails while a version stamped in the default branch's
//! `CHANGELOG.md` has no tag, which is a release that did not complete.
//! Merging more on top would stack unpublished versions. Decided from the
//! file and the tags only: no API, no token.
//!
//! Right after a merge, until its `release` job has tagged, this check is red
//! too: retry it. The branches named `fix/release-*` are exempt, since they
//! are the ones that repair the release.

use crate::changelog::{Changelog, FILE_NAME};
use crate::cli::{Options, Outcome, error};
use crate::git::{self, Tags};
use crate::plan;

pub const USAGE: &str = "\
Usage: cargo xtask release-backlog [--base REF]

Fail while CHANGELOG.md on the default branch (origin/main, then main) stamps a
version that has no `v<version>` tag: a release that is still running, failed
or never ran. Passes before the first tag exists and on `fix/release-*`
branches. Fetch the default branch and the tags before running it.
";

pub fn run(args: &[String]) -> Outcome {
    let options = Options::parse(args, &["--base", "--default-branch"])?;
    options.no_positional()?;

    let branch = std::env::var("CI_COMMIT_BRANCH")
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(git::current_branch)
        .unwrap_or_default();
    if branch.starts_with("fix/release-") {
        println!("Branch {branch} repairs the release pipeline: the backlog check is skipped.");
        return Ok(0);
    }

    let tags = Tags::load();
    if tags.bootstrap() {
        println!("No tag yet: the repository has not published, so there is no backlog.");
        return Ok(0);
    }

    let default_ref = plan::default_ref(options.base_ref())
        .filter(|reference| git::ref_exists(reference))
        .ok_or_else(|| error("release-backlog: neither origin/main nor main exists; fetch the default branch first."))?;
    let Some(text) = git::show(&default_ref, FILE_NAME) else {
        println!("{default_ref} has no {FILE_NAME}: nothing is stamped there.");
        return Ok(0);
    };
    let missing: Vec<String> = Changelog::parse(&text)
        .headings()
        .into_iter()
        .filter(|heading| heading.stamped && !tags.exists(heading.version))
        .map(|heading| heading.version.to_string())
        .collect();

    if missing.is_empty() {
        println!("Every stamped version of {default_ref} has its tag.");
        return Ok(0);
    }
    let listed = missing.join(", ");
    eprintln!(
        "ERROR: {default_ref} stamps a version that has no tag (release failed, or still running): {listed}"
    );
    eprintln!();
    eprintln!(
        "{FILE_NAME} on {default_ref} stamps it, but no git tag v<version> exists. Either the release"
    );
    eprintln!(
        "job of the merge that brought it is still running (wait for it, then retry this job),"
    );
    eprintln!("or it failed or never ran, and the version was not published.");
    eprintln!("This branch is blocked so that nothing piles up on top of an unpublished version.");
    eprintln!();
    eprintln!("What to do:");
    eprintln!(
        "  1. open the pipeline of the main commit that stamped it, and read its `release` job;"
    );
    eprintln!(
        "  2. fix the cause (a token, a registry, the right to push a tag) and retry that job:"
    );
    eprintln!("     it tags the commit and publishes;");
    eprintln!("  3. retry this job.");
    eprintln!();
    eprintln!("A fix to the release itself goes on a branch named fix/release-<something>: that");
    eprintln!("prefix is exempt from this check.");
    Ok(1)
}

//! `canary-version`: the version of a prerelease built from a branch.
//!
//! A prerelease sorts before the release it is cut from, so it is built on the
//! version the branch is about to publish: it then sorts after the last
//! release and before the one the branch leads to. When the branch holds
//! nothing publishable, the patch of the highest known version (last tag, or a
//! version `CHANGELOG.md` carries) is incremented instead, which still sorts
//! after it. Any other failure stops the job: a canary is never worth a wrong
//! number.

use std::sync::LazyLock;

use regex::Regex;

use crate::cli::{Options, Outcome, read_changelog, usage};
use crate::commit::bump_level;
use crate::git::Tags;
use crate::plan;
use crate::version::Version;

pub const USAGE: &str = "\
Usage: cargo xtask canary-version SHORT_SHA [--base REF]

Print the SemVer prerelease version of a canary build: the version this branch
publishes followed by `-pre.<sha>`, e.g. `0.2.0-pre.ab12cd3`. When the branch
publishes nothing, the highest known version with its patch incremented.
";

static SHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{7,40}$").expect("valid regex"));

pub fn run(args: &[String]) -> Outcome {
    let options = Options::parse(args, &["--base", "--default-branch"])?;
    let sha = match options.positional() {
        [sha] if SHA.is_match(sha) => sha.as_str(),
        [sha] => return Err(usage(format!("'{sha}' is not a short commit sha"))),
        [] => return Err(usage("canary-version needs the short sha of the commit")),
        [_, extra, ..] => return Err(usage(format!("unexpected argument '{extra}'"))),
    };

    let tags = Tags::load();
    let changelog = read_changelog()?;
    let plan = plan::branch_plan(&changelog, options.base_ref(), &tags);
    let version = match bump_level(&plan.range.commits()) {
        Some(level) => plan::publish_version(&tags, plan.base, level),
        None => {
            let known = plan
                .headings
                .iter()
                .map(|heading| heading.version)
                .chain(tags.highest_reachable())
                .max()
                .unwrap_or(Version::ZERO);
            Version::new(known.major, known.minor, known.patch + 1)
        }
    };
    println!("{version}-pre.{sha}");
    Ok(0)
}

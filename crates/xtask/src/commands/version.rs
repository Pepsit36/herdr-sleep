//! `changelog version`: the version the next publication will carry.

use crate::cli::{EXIT_NOTHING_TO_PUBLISH, Options, Outcome, read_changelog};
use crate::commit::bump_level;
use crate::git::{self, Tags};
use crate::plan::{self, Range};
use crate::version::Version;

/// Where the counting starts, the base and the range, resolved once for the
/// commands that share them.
pub struct Counting {
    pub base: Version,
    pub range: Range,
    pub from: String,
    pub tags: Tags,
}

/// `--since REF` counts from REF; otherwise the branch plan decides.
pub fn counting(options: &Options) -> Result<Counting, crate::cli::Failure> {
    let tags = Tags::load();
    if let Some(since) = options.get("--since") {
        if !git::ref_exists(since) {
            return Err(crate::cli::error(format!("--since {since}: no such ref")));
        }
        let base = Version::parse(since)
            .or_else(|| tags.highest_reachable())
            .unwrap_or(Version::ZERO);
        return Ok(Counting {
            base,
            range: Range::Since(since.to_string()),
            from: since.to_string(),
            tags,
        });
    }
    let changelog = read_changelog()?;
    let plan = plan::branch_plan(&changelog, options.base_ref(), &tags);
    if plan.default_ref.is_none() {
        eprintln!(
            "changelog version: {} not found, counting from {}.",
            options.base_ref().unwrap_or("origin/main"),
            plan.counting_from()
        );
    }
    Ok(Counting {
        base: plan.base,
        range: plan.range.clone(),
        from: plan.counting_from(),
        tags,
    })
}

pub fn run(args: &[String]) -> Outcome {
    let options = Options::parse(args, &["--since", "--base", "--default-branch"])?;
    options.no_positional()?;
    let counting = counting(&options)?;

    let Some(level) = bump_level(&counting.range.commits()) else {
        eprintln!(
            "changelog version: no publishable commit in {}.",
            counting.range.label()
        );
        return Ok(EXIT_NOTHING_TO_PUBLISH);
    };
    let version = plan::publish_version(&counting.tags, counting.base, level);
    if counting.tags.bootstrap() {
        eprintln!(
            "Counting from {}: no tag yet, publishing {version} as the first release.",
            counting.from
        );
    } else {
        eprintln!(
            "Counting from {}: base {}, publishing {version}.",
            counting.from, counting.base
        );
    }
    println!("{version}");
    Ok(0)
}

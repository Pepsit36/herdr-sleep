//! `changelog check`: run on every branch pipeline. The branch has to carry,
//! in the top section of `CHANGELOG.md`, exactly the version its merge will
//! publish, and must leave what it inherited exactly as it was.

use crate::changelog::{Changelog, FILE_NAME, Heading};
use crate::cli::{Options, Outcome, error, read_changelog};
use crate::commit::bump_level;
use crate::entries::entry_lines;
use crate::git::{self, Tags};
use crate::plan::{self, Plan};
use crate::version::Version;

const FAILED: &str = "changelog check: FAILED.\n";

/// How `CHANGELOG.md` changed against the merge base, section by section.
struct Diff {
    base: Vec<(Version, String)>,
    changed: Vec<Version>,
    missing: Vec<Version>,
    reordered: bool,
    empty_base: bool,
}

impl Diff {
    fn touched(&self) -> bool {
        !self.empty_base && (!self.changed.is_empty() || !self.missing.is_empty() || self.reordered)
    }

    fn base_body(&self, version: Version) -> Option<&str> {
        self.base
            .iter()
            .find(|(found, _)| *found == version)
            .map(|(_, body)| body.as_str())
    }
}

pub fn run(args: &[String]) -> Outcome {
    let options = Options::parse(args, &["--base", "--default-branch"])?;
    options.no_positional()?;

    let base_ref = plan::default_ref(options.base_ref()).ok_or_else(|| {
        error(
            "changelog check: neither origin/main nor main exists; fetch the default branch first.",
        )
    })?;
    let Some(merge_base) = git::merge_base(&base_ref) else {
        let short = base_ref.strip_prefix("origin/").unwrap_or(&base_ref);
        return Err(error(format!(
            "changelog check: no merge base between {base_ref} and HEAD.\nFetch it first (git fetch origin {short}) and make sure the clone is not shallow (GIT_DEPTH: \"0\")."
        )));
    };

    let tags = Tags::load();
    let changelog = read_changelog()?;
    let plan = plan::branch_plan(&changelog, Some(&base_ref), &tags);
    let level = bump_level(&git::commits(Some(&format!("{merge_base}..HEAD"))));
    let stamped = plan.owned;

    if level.is_some() && on_release_repair_branch() {
        return Ok(release_repair_publishes());
    }

    let diff = section_diff(&merge_base, &changelog, &tags).map_err(error)?;
    let frozen: Vec<Version> = diff
        .changed
        .iter()
        .chain(&diff.missing)
        .copied()
        .filter(|version| tags.exists(*version))
        .collect();
    if !frozen.is_empty() {
        return Ok(frozen_failure(&frozen));
    }
    if diff.touched() {
        return Ok(match level {
            None => repair_verdict(&changelog, &diff, &tags),
            Some(_) => conflict_failure(&diff),
        });
    }

    let Some(level) = level else {
        // A stamped section the merge base already carries was inherited, not
        // written by this branch (before the first tag the release has not
        // happened yet). Only a stamp the branch wrote itself is superfluous.
        if let Some(stamped) = stamped.filter(|s| diff.base_body(s.version).is_none()) {
            return Ok(superfluous_stamp(stamped));
        }
        println!(
            "changelog check: no publishable commit between {} and HEAD, nothing to require.",
            &merge_base[..8]
        );
        return Ok(0);
    };

    let version = plan::publish_version(&tags, plan.base, level);
    match stamped {
        Some(stamped) if stamped.version == version => {
            println!(
                "changelog check: OK, this branch publishes {version} and {FILE_NAME} says so."
            );
            Ok(0)
        }
        _ => Ok(stamp_missing(version, stamped, &changelog, &plan, &tags)),
    }
}

/// Whether the branch is one of the `fix/release-*` branches, which repair the
/// release pipeline while a stamped version waits for its tag.
fn on_release_repair_branch() -> bool {
    std::env::var("CI_COMMIT_BRANCH")
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(git::current_branch)
        .is_some_and(|branch| branch.starts_with("fix/release-"))
}

/// A repair branch is exempt from the backlog check, so it can merge while
/// the last version is untagged; a publishable commit on it would then ask
/// the release job for a version on top of that one, which it refuses.
fn release_repair_publishes() -> u8 {
    eprintln!("ERROR: a fix/release-* branch must not publish a version.");
    eprintln!();
    eprintln!(
        "It repairs the release pipeline while a stamped version has no tag, and the release job"
    );
    eprintln!("refuses to publish a new one on top of it. Use commit types that publish nothing");
    eprintln!("(`ci:`, `build:`, `chore:`, `refactor:`, `docs:`, `test:`) for the repair, and");
    eprintln!("remove the stamped section this branch added, if any.");
    1
}

fn section_diff(merge_base: &str, changelog: &Changelog, tags: &Tags) -> Result<Diff, String> {
    let base =
        Changelog::parse(&git::show(merge_base, FILE_NAME).unwrap_or_default()).section_bodies()?;
    let head = changelog.section_bodies()?;
    let head_body = |version: Version| {
        head.iter()
            .find(|(found, _)| *found == version)
            .map(|(_, body)| body.as_str())
    };

    let changed = base
        .iter()
        .filter(|(version, body)| {
            let Some(now) = head_body(*version) else {
                return false;
            };
            if now.trim() == body.trim() {
                return false;
            }
            // Before the first release every branch folds its entries into
            // the inherited [0.1.0] section: gaining entries is not a change.
            !(tags.bootstrap() && *version == Version::FIRST && only_gained(body, now))
        })
        .map(|(version, _)| *version)
        .collect();
    let missing = base
        .iter()
        .map(|(version, _)| *version)
        .filter(|version| head_body(*version).is_none())
        .collect();
    let common_in_base: Vec<Version> = base
        .iter()
        .map(|(version, _)| *version)
        .filter(|version| head_body(*version).is_some())
        .collect();
    let common_in_head: Vec<Version> = head
        .iter()
        .map(|(version, _)| *version)
        .filter(|version| base.iter().any(|(found, _)| found == version))
        .collect();

    Ok(Diff {
        empty_base: base.is_empty(),
        base,
        changed,
        missing,
        reordered: common_in_base != common_in_head,
    })
}

/// Every entry of `before` is still in `after`.
fn only_gained(before: &str, after: &str) -> bool {
    let kept = entry_lines(after);
    entry_lines(before).iter().all(|line| kept.contains(line))
}

fn superfluous_stamp(stamped: Heading) -> u8 {
    eprintln!("{FAILED}");
    eprintln!(
        "This branch has no publishable commit, but {FILE_NAME} carries a stamped {} section.",
        stamped.version
    );
    eprintln!(
        "The `release` job would publish nothing, and the next branch would take that number as its base."
    );
    eprintln!("Remove the stamped section and put its entries back under `## [Unreleased]`.");
    1
}

fn stamp_missing(
    version: Version,
    stamped: Option<Heading>,
    changelog: &Changelog,
    plan: &Plan,
    tags: &Tags,
) -> u8 {
    eprintln!("{FAILED}");
    eprintln!(
        "{}",
        stamp_missing_reason(version, stamped, changelog, plan, tags)
    );
    eprintln!();
    eprintln!("Run `cargo xtask changelog stamp` and commit {FILE_NAME}.");
    eprintln!("It writes `## [{version}] - <today>` and reopens an empty [Unreleased] above it.");
    1
}

/// Three ways of not being stamped, and they do not read the same.
fn stamp_missing_reason(
    version: Version,
    stamped: Option<Heading>,
    changelog: &Changelog,
    plan: &Plan,
    tags: &Tags,
) -> String {
    if let Some(stamped) = stamped {
        return format!(
            "This branch publishes {version}, but the top section of {FILE_NAME} says {}.",
            stamped.version
        );
    }
    if changelog.unreleased_index().is_none() {
        return format!(
            "This branch publishes {version}, but {FILE_NAME} has no [Unreleased] section."
        );
    }
    match inherited_stamp(plan, tags) {
        Some(inherited) => format!(
            "This branch publishes {version}, but the top section of {FILE_NAME} is {}, stamped by another branch. Yours is missing.",
            inherited.version
        ),
        None => format!(
            "This branch publishes {version}, but [Unreleased] is still the top section of {FILE_NAME}."
        ),
    }
}

/// The top section when it was stamped by another branch and not published
/// yet. The marker stays once the version is tagged, so a tagged section is a
/// released one, not someone else's work in progress.
fn inherited_stamp(plan: &Plan, tags: &Tags) -> Option<Heading> {
    let top = *plan.headings.first()?;
    (top.stamped && !tags.exists(top.version)).then_some(top)
}

/// A released entry is frozen, typo included. That is the rule of the file,
/// and it has no exception: not a reword, not a reorder, not a removal.
fn frozen_failure(tagged: &[Version]) -> u8 {
    eprintln!("{FAILED}");
    for version in tagged {
        eprintln!("Section {version} carries a tag: a published entry is frozen, typo included.");
    }
    eprintln!();
    eprintln!(
        "That is the rule of {FILE_NAME}. If the wording is wrong, say so in the entry of a later"
    );
    eprintln!("version; the published one stays as it was published.");
    1
}

/// The branch brought its own commits and still moved what it inherited: that
/// is what a badly resolved conflict looks like.
fn conflict_failure(diff: &Diff) -> u8 {
    eprintln!("{FAILED}");
    for version in &diff.changed {
        eprintln!(
            "Section {version} was changed by this branch: it was stamped by another branch and merged before yours."
        );
    }
    for version in &diff.missing {
        eprintln!(
            "Section {version} is no longer in {FILE_NAME}, and it was stamped by another branch."
        );
    }
    if diff.reordered {
        eprintln!("The sections of {FILE_NAME} are no longer in the order they were merged in.");
    }
    eprintln!();
    eprintln!(
        "This is what a badly resolved conflict on {FILE_NAME} looks like. Put your own entries back"
    );
    eprintln!("under `## [Unreleased]`, leave every section below it exactly as it is, and run");
    eprintln!("`cargo xtask changelog stamp` again.");
    1
}

/// A branch that brings no publishable commit of its own may take out or
/// renumber the stamped sections that carry no tag: that is how the version
/// of an abandoned merge request leaves the file. What it may not do is leave
/// the file saying anything other than what the release job will publish.
fn repair_verdict(changelog: &Changelog, diff: &Diff, tags: &Tags) -> u8 {
    let expected = plan::release_version(tags);
    let untagged: Vec<Heading> = changelog
        .headings()
        .into_iter()
        .filter(|heading| !tags.exists(heading.version))
        .collect();
    let removed: Vec<String> = diff.missing.iter().map(Version::to_string).collect();

    let Some(expected) = expected else {
        if !untagged.is_empty() {
            return repair_failure(None, &untagged);
        }
        println!(
            "changelog check: {} removed from {FILE_NAME}, and nothing untagged is left. The release job will publish nothing after this merge, which agrees.",
            removed.join(", ")
        );
        return 0;
    };

    if untagged.len() != 1 || untagged[0].version != expected {
        return repair_failure(Some(expected), &untagged);
    }
    if let Some(first) = changelog.headings().first()
        && first.version != expected
    {
        return repair_position_failure(expected, first.version);
    }
    if diff.reordered {
        return reorder_failure();
    }
    let survivor = changelog.version_body(expected).unwrap_or_default();
    if let Some((problem, lines)) = repair_body_problem(diff, expected, &survivor, tags) {
        return repair_body_failure(expected, problem, &lines);
    }
    println!(
        "changelog check: OK, {FILE_NAME} was repaired: {expected} is the only untagged section left, and it is what the release job will publish."
    );
    0
}

fn repair_failure(expected: Option<Version>, untagged: &[Heading]) -> u8 {
    let left: Vec<String> = untagged
        .iter()
        .map(|heading| heading.version.to_string())
        .collect();
    eprintln!("{FAILED}");
    eprintln!(
        "This branch brings no publishable commit, so it may take out or renumber the stamped"
    );
    eprintln!("sections that carry no tag. The file does not line up with what will happen next.");
    eprintln!();
    match expected {
        None => {
            eprintln!(
                "After this merge the release job will publish nothing, so no untagged section should be"
            );
            eprintln!("left in {FILE_NAME}. Still there: {}.", left.join(", "));
        }
        Some(expected) => {
            eprintln!(
                "After this merge the release job will publish {expected}, so {FILE_NAME} must carry exactly"
            );
            eprintln!(
                "one untagged section, at the top, numbered {expected}. Untagged right now: {}.",
                if left.is_empty() {
                    "none".to_string()
                } else {
                    left.join(", ")
                }
            );
        }
    }
    eprintln!();
    eprintln!(
        "Renumber the section the merge will publish, and take the abandoned ones out. A section"
    );
    eprintln!("that carries a tag is never touched.");
    1
}

fn repair_position_failure(expected: Version, first: Version) -> u8 {
    eprintln!("{FAILED}");
    eprintln!(
        "{expected} is the version this merge will publish, so it has to come at the top of {FILE_NAME}."
    );
    eprintln!("The first version section is {first} instead.");
    eprintln!();
    eprintln!(
        "The repair takes the abandoned sections out and renumbers the one that stays; it does"
    );
    eprintln!("not move it.");
    1
}

/// The order of the sections is the order the versions were merged in, and it
/// is a record, not a layout choice. No branch reorders it, repair branch
/// included.
fn reorder_failure() -> u8 {
    eprintln!("{FAILED}");
    eprintln!("The sections of {FILE_NAME} are no longer in the order they were merged in.");
    eprintln!();
    eprintln!(
        "That order is a record of what was published when, and a published entry is frozen,"
    );
    eprintln!("its position included. Put the sections back in the order they had, newest first.");
    1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyProblem {
    Invented,
    Lost,
    Unknown,
}

/// On a repair branch the surviving section keeps the body it had. The one
/// latitude: it may take in the entries of the untagged sections this branch
/// removed. Those commits stay in the history and ship with the version that
/// publishes them, so their entry can legitimately join it.
fn repair_body_problem(
    diff: &Diff,
    version: Version,
    survivor: &str,
    tags: &Tags,
) -> Option<(BodyProblem, Vec<String>)> {
    let allowed: Vec<String> = diff
        .base
        .iter()
        .filter(|(other, _)| !tags.exists(*other))
        .flat_map(|(_, body)| entry_lines(body))
        .collect();
    let kept = entry_lines(survivor);

    let invented: Vec<String> = kept
        .iter()
        .filter(|line| !allowed.contains(line))
        .cloned()
        .collect();
    if !invented.is_empty() {
        return Some((BodyProblem::Invented, invented));
    }

    // The gesture renumbers the top pending section, and that number is often
    // the one an abandoned section had, so the number at HEAD says nothing
    // about which section survived. As soon as a pending section was removed,
    // the survivor descends from the highest of them.
    let pending = diff
        .missing
        .iter()
        .copied()
        .filter(|other| !tags.exists(*other))
        .max();
    let descent = match pending {
        Some(highest) => diff.base_body(highest),
        None if !tags.exists(version) => diff.base_body(version),
        None => None,
    };
    let Some(descent) = descent else {
        return Some((BodyProblem::Unknown, Vec::new()));
    };
    let lost: Vec<String> = entry_lines(descent)
        .into_iter()
        .filter(|line| !kept.contains(line))
        .collect();
    (!lost.is_empty()).then_some((BodyProblem::Lost, lost))
}

fn repair_body_failure(version: Version, problem: BodyProblem, lines: &[String]) -> u8 {
    eprintln!("{FAILED}");
    match problem {
        BodyProblem::Invented => {
            for line in lines {
                eprintln!("In section {version}: {line}");
            }
            eprintln!();
            eprintln!(
                "That entry was not in any of the sections waiting to be published, so this branch wrote"
            );
            eprintln!(
                "it. A repair branch does not write entries: it takes abandoned sections out and keeps"
            );
            eprintln!("what was already there.");
        }
        BodyProblem::Lost => {
            for line in lines {
                eprintln!("Gone from section {version}: {line}");
            }
            eprintln!();
            eprintln!(
                "That entry described the version this merge will publish and is no longer there."
            );
            eprintln!("A repair branch renumbers a section; it does not empty it.");
        }
        BodyProblem::Unknown => {
            eprintln!(
                "Section {version} of {FILE_NAME} does not descend from any section that was waiting to be"
            );
            eprintln!(
                "published. A repair branch renumbers one of those; it does not invent a section."
            );
        }
    }
    1
}

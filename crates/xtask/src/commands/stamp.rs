//! `changelog stamp`: the author's gesture once a branch is ready.
//!
//! Gives the top section the version this merge will publish, and reopens an
//! empty `[Unreleased]` above it. Run again after a rebase or new commits, it
//! corrects the number and the date and folds the new `[Unreleased]` entries
//! into the section it owns.

use std::fs;

use crate::changelog::{Changelog, FILE_NAME, UNRELEASED_HEADING, lines_of};
use crate::cli::{Options, Outcome, changelog_path, error, usage};
use crate::commit::bump_level;
use crate::date;
use crate::entries::{self, NO_CHANGES, STAMP_MARKER};
use crate::git::Tags;
use crate::plan::{self, Plan};
use crate::version::Version;

/// What a fresh `CHANGELOG.md` starts with.
pub const HEADER: &str = "\
# Changelog

All notable changes to Herdr Sleep are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Describe your change under `[Unreleased]`, for a user of the plugin. When your branch is ready,
run `cargo xtask changelog stamp`: it works out the version the merge will publish and turns
`[Unreleased]` into `[X.Y.Z] - <date>`, reopening an empty one above. Run it again after a rebase
or a new commit: it corrects the number and the date and folds the new entries in. A section
written by `stamp` carries `<!-- stamped -->`. Never edit a section that already carries a tag:
a published entry is frozen, typo included.
";

pub fn run(args: &[String]) -> Outcome {
    let options = Options::parse(args, &["--date", "--base", "--default-branch"])?;
    options.no_positional()?;
    let date = match options.get("--date") {
        Some(given) if date::looks_like_date(given) => given.to_string(),
        Some(given) => return Err(usage(format!("--date {given}: expected YYYY-MM-DD"))),
        None => date::today(),
    };

    let path = changelog_path()?;
    if !path.exists() {
        println!("{FILE_NAME} does not exist, creating it.");
        fs::write(&path, format!("{HEADER}\n{UNRELEASED_HEADING}\n"))
            .map_err(|cause| error(format!("cannot write {}: {cause}", path.display())))?;
    }
    let text = fs::read_to_string(&path)
        .map_err(|cause| error(format!("cannot read {}: {cause}", path.display())))?;
    let mut changelog = Changelog::parse(&text);

    let tags = Tags::load();
    let plan = plan::branch_plan(&changelog, options.base_ref(), &tags);
    let Some(level) = bump_level(&plan.range.commits()) else {
        println!(
            "No publishable commit in {}: nothing to stamp.",
            plan.range.label()
        );
        return Ok(0);
    };
    let version = plan::publish_version(&tags, plan.base, level);
    describe(&plan, &tags, version);

    match changelog.unreleased_index() {
        Some(index) => stamp_section(&mut changelog, index, &plan, version, &date)?,
        None => stamp_without_unreleased(&mut changelog, &plan, version, &date),
    }

    fs::write(&path, changelog.render())
        .map_err(|cause| error(format!("cannot write {}: {cause}", path.display())))?;
    println!(
        "{FILE_NAME} now carries `## [{version}] - {date}`, with an empty [Unreleased] above it."
    );
    Ok(0)
}

fn describe(plan: &Plan, tags: &Tags, version: Version) {
    if tags.bootstrap() {
        println!(
            "Counting from {}: no tag yet, publishing {version} as the first release.",
            plan.counting_from()
        );
    } else {
        println!(
            "Counting from {}: base {}, publishing {version}.",
            plan.counting_from(),
            plan.base
        );
    }
    if let Some(owned) = plan.owned {
        if owned.version != version {
            println!(
                "Correcting the section this branch already stamped: {} becomes {version}.",
                owned.version
            );
        }
    }
}

fn stamped_heading(version: Version, date: &str) -> Vec<String> {
    vec![
        UNRELEASED_HEADING.to_string(),
        String::new(),
        format!("## [{version}] - {date}"),
        String::new(),
        STAMP_MARKER.to_string(),
        String::new(),
    ]
}

/// `[Unreleased]` exists. Its entries are folded into the section this branch
/// owns, or start a new one, and `[Unreleased]` is reopened empty.
fn stamp_section(
    changelog: &mut Changelog,
    index: usize,
    plan: &Plan,
    version: Version,
    date: &str,
) -> Result<(), crate::cli::Failure> {
    let stop = changelog.section_end(index);
    let incoming = changelog.body_at(index);
    let mut block = stamped_heading(version, date);

    let Some(owned) = plan.owned else {
        block.extend(lines_of(&entry_body(&incoming, plan)));
        changelog.splice(index, stop, block);
        return Ok(());
    };
    if owned.index < index {
        return Err(error(format!(
            "[Unreleased] comes after the section this branch stamped ([{}]) in {FILE_NAME}; move it above the version sections.",
            owned.version
        )));
    }
    let owned_stop = changelog.section_end(owned.index);
    let existing = changelog.body_at(owned.index);
    block.extend(lines_of(&entries::merge_bodies(&existing, &incoming)));
    changelog.splice(index, owned_stop, block);
    Ok(())
}

/// No `[Unreleased]` section at all: one is inserted, and the version section
/// is generated from the commits.
fn stamp_without_unreleased(changelog: &mut Changelog, plan: &Plan, version: Version, date: &str) {
    println!("WARNING: {FILE_NAME} has no [Unreleased] section.");
    let mut block = stamped_heading(version, date);
    block.extend(lines_of(&entry_body("", plan)));
    let insert_at = changelog
        .first_section_index()
        .unwrap_or(changelog.lines.len());
    if insert_at == changelog.lines.len() {
        block.push(String::new());
    }
    changelog.splice(insert_at, insert_at, block);
}

/// What goes under the new heading: what the author wrote, or a starting
/// point taken from the commit subjects when nothing was written.
fn entry_body(incoming: &str, plan: &Plan) -> String {
    let written = incoming.trim();
    if !written.is_empty() {
        return format!("{written}\n");
    }
    let generated =
        entries::body(&plan.range.commits()).unwrap_or_else(|| format!("{NO_CHANGES}\n"));
    println!(
        "Nothing was written under [Unreleased], so the entry comes from the commit subjects."
    );
    println!("Read it and reword it for a user of the plugin:");
    print!("{generated}");
    generated
}

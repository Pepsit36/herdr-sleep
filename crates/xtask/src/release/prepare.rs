//! `release prepare` and `release set-version`: decide the version and write it.

use crate::changelog::FILE_NAME;
use crate::cli::{
    EXIT_NOTHING_TO_PUBLISH, Failure, Options, Outcome, error, read_changelog, usage,
};
use crate::git::{self, Tags};
use crate::plan;
use crate::version::Version;

use super::{finished_at_head, read, rewrite, take_flag, write};

/// Rewrites the three files to `version`; nothing is written unless all three
/// rewrite cleanly.
pub(super) fn write_version(version: &str, dry_run: bool) -> Result<(), Failure> {
    let mut rewritten = Vec::new();
    for name in rewrite::FILES {
        let text = rewrite::file(name, &read(name)?, version).map_err(error)?;
        rewritten.push((name, text));
    }
    for (name, text) in rewritten {
        if dry_run {
            eprintln!("release: would set the version of {name} to {version}");
        } else {
            write(name, &text)?;
            eprintln!("release: {name} is at {version}");
        }
    }
    Ok(())
}

pub(super) fn set_version(args: &[String], dry_run: bool) -> Outcome {
    let options = Options::parse(args, &[])?;
    match options.positional() {
        [version] if rewrite::is_version(version) => write_version(version, dry_run)?,
        [version] => return Err(usage(format!("'{version}' is not a version"))),
        _ => return Err(usage("set-version needs exactly one version")),
    }
    Ok(0)
}

/// `CHANGELOG.md` must say, on top and stamped, the version being released.
fn check_changelog(version: Version) -> Result<(), Failure> {
    let changelog = read_changelog()?;
    match changelog.headings().first() {
        Some(top) if top.version == version && top.stamped => Ok(()),
        Some(top) => Err(error(format!(
            "release prepare: the commits since the last tag publish {version}, but the top section of {FILE_NAME} is {}{}.\nStamp it with `cargo xtask changelog stamp` in a branch and merge that first.",
            top.version,
            if top.stamped { "" } else { " (not stamped)" }
        ))),
        None => Err(error(format!(
            "release prepare: {FILE_NAME} has no version section; the commits since the last tag publish {version}."
        ))),
    }
}

/// A tag that already exists is acceptable only as the leftover of an earlier
/// attempt at this very release: its commit is the child of HEAD.
fn check_tag(version: Version) -> Result<(), Failure> {
    let tag = version.tag();
    let Some(tagged) = git::rev_parse(&tag) else {
        return Ok(());
    };
    let head = git::rev_parse("HEAD").unwrap_or_default();
    if tagged == head {
        return Err(error(format!(
            "release prepare: tag {tag} already points at HEAD, which is a release commit, yet commits publish {version} again."
        )));
    }
    if git::rev_parse(&format!("{tag}^")).as_deref() == Some(head.as_str()) {
        eprintln!(
            "release prepare: {tag} already exists on the release commit of HEAD; resuming that release."
        );
        return Ok(());
    }
    Err(error(format!(
        "release prepare: tag {tag} already exists on {}, which is not the release commit of HEAD ({}).\nA version is never published twice: repair the tag, or the history, by hand.",
        &tagged[..tagged.len().min(8)],
        &head[..head.len().min(8)]
    )))
}

pub(super) fn run(args: &[String], dry_run: bool) -> Outcome {
    let (apply, rest) = take_flag(args, "--apply");
    Options::parse(&rest, &[])?.no_positional()?;

    let tags = Tags::load();
    let Some(version) = plan::release_version(&tags) else {
        if let Some(version) = finished_at_head(&tags) {
            eprintln!(
                "release prepare: HEAD is the release commit of {}; resuming that release.",
                version.tag()
            );
            if apply {
                write_version(&version.to_string(), dry_run)?;
            }
            println!("RELEASE=1");
            println!("VERSION={version}");
            return Ok(0);
        }
        eprintln!("release prepare: no publishable commit since the last tag; nothing to release.");
        println!("RELEASE=0");
        return Ok(EXIT_NOTHING_TO_PUBLISH);
    };
    check_changelog(version)?;
    check_tag(version)?;
    if apply {
        write_version(&version.to_string(), dry_run)?;
    }
    println!("RELEASE=1");
    println!("VERSION={version}");
    Ok(0)
}

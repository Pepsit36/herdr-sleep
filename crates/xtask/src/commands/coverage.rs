//! `changelog coverage`: every tag reachable from HEAD has a stamped section
//! that describes it.

use crate::changelog::FILE_NAME;
use crate::cli::{Options, Outcome, read_changelog};
use crate::entries::{self, NO_CHANGES, STAMP_MARKER};
use crate::git::Tags;

pub fn run(args: &[String]) -> Outcome {
    let options = Options::parse(args, &[])?;
    options.no_positional()?;

    let tags = Tags::load();
    let reachable: Vec<_> = tags.reachable().collect();
    if reachable.is_empty() {
        println!(
            "changelog coverage: no tag reachable from HEAD{}, nothing to cover yet.",
            if tags.bootstrap() {
                " (the repository has not published yet)"
            } else {
                ""
            }
        );
        return Ok(0);
    }

    let changelog = read_changelog()?;
    let mut problems: Vec<(String, String)> = Vec::new();
    for version in &reachable {
        let Some(body) = changelog.version_body(*version) else {
            problems.push((
                version.to_string(),
                format!("no `## [{version}]` section in {FILE_NAME}"),
            ));
            continue;
        };
        if !body.contains(STAMP_MARKER) {
            problems.push((
                version.to_string(),
                format!("the section does not carry `{STAMP_MARKER}`"),
            ));
        }
        if !entries::describes_something(&body) {
            problems.push((
                version.to_string(),
                format!("the section has no entry, not even `{NO_CHANGES}`"),
            ));
        }
    }

    if problems.is_empty() {
        println!(
            "changelog coverage: OK, {} tag(s) reachable from HEAD each have a stamped section in {FILE_NAME}.",
            reachable.len()
        );
        return Ok(0);
    }
    eprintln!("changelog coverage: FAILED.\n");
    for (version, message) in &problems {
        eprintln!("## [{version}]\n  {message}\n");
    }
    eprintln!(
        "Every tag reachable from HEAD needs a `## [version]` section written by `cargo xtask changelog stamp`,"
    );
    eprintln!("with at least one bullet or `{NO_CHANGES}`.");
    Ok(1)
}

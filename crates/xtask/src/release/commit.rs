//! `release commit`: the release commit, its tag, and the two pushes.
//!
//! Every step looks at what an earlier attempt left behind (a commit, a tag, a
//! pushed branch) and finishes the job instead of failing or duplicating.

use std::fs;

use crate::cli::{Failure, Options, Outcome, error};
use crate::git::{self, Tags};
use crate::plan;
use crate::version::Version;

use super::{capture, finished_at_head, manifest_version, read, rewrite, root};

fn git_in(args: &[&str]) -> Result<String, Failure> {
    capture("git", args).map_err(error)
}

fn subject(version: Version) -> String {
    format!("chore(release): {version} [skip ci]")
}

/// The working tree must already carry `version` in the three files, exactly
/// as `release prepare --apply` writes it.
fn check_files(version: Version) -> Result<(), Failure> {
    let wanted = version.to_string();
    for name in rewrite::FILES {
        let text = read(name)?;
        let expected = rewrite::file(name, &text, &wanted).map_err(error)?;
        if text != expected {
            return Err(error(format!(
                "release commit: {name} does not carry {wanted}; run `cargo xtask release prepare --apply` first."
            )));
        }
    }
    Ok(())
}

/// Whether `candidate` is the release commit of `version` on top of `head`:
/// same subject, `head` as only parent, and the three files as in the working tree.
fn is_release_commit(candidate: &str, head: &str, version: Version) -> bool {
    let Some(parents) = git::run(&["rev-list", "--parents", "-n", "1", candidate]) else {
        return false;
    };
    let fields: Vec<&str> = parents.split_whitespace().collect();
    if fields.len() != 2 || fields[1] != head {
        return false;
    }
    if git::run(&["log", "-1", "--format=%s", candidate]).map(|s| s.trim().to_string())
        != Some(subject(version))
    {
        return false;
    }
    let Ok(top) = root() else { return false };
    rewrite::FILES
        .iter()
        .all(|name| git::show(candidate, name) == fs::read_to_string(top.join(name)).ok())
}

/// Brings the remote tag and branch tip in, ignoring what does not exist yet,
/// and returns the local name of the branch tip.
fn fetch_state(remote: &str, branch: &str, tag: &str) -> String {
    let tracking = format!("refs/remotes/{remote}/{branch}");
    let _ = capture(
        "git",
        &[
            "fetch",
            "--quiet",
            remote,
            &format!("refs/tags/{tag}:refs/tags/{tag}"),
        ],
    );
    let _ = capture(
        "git",
        &[
            "fetch",
            "--quiet",
            remote,
            &format!("+refs/heads/{branch}:{tracking}"),
        ],
    );
    tracking
}

/// A release commit of an earlier attempt: the local tag or the remote branch tip.
fn earlier_attempt(tag: &str, tracking: &str, head: &str, version: Version) -> Option<String> {
    [tag, tracking]
        .iter()
        .filter_map(|reference| git::rev_parse(reference))
        .find(|candidate| is_release_commit(candidate, head, version))
}

pub(super) fn run(args: &[String], dry_run: bool) -> Outcome {
    let options = Options::parse(args, &["--remote", "--branch"])?;
    options.no_positional()?;
    let remote = options.get("--remote").unwrap_or("origin");
    let branch = options.get("--branch").unwrap_or("main");

    let tags = Tags::load();
    if let Some(version) = finished_at_head(&tags) {
        println!(
            "release commit: {} is already committed and tagged; nothing to do.",
            version.tag()
        );
        return Ok(0);
    }
    let version = plan::release_version(&tags)
        .ok_or_else(|| error("release commit: no publishable commit since the last tag."))?;
    let current = manifest_version()?;
    if current != version.to_string() {
        return Err(error(format!(
            "release commit: herdr-plugin.toml says {current}, the release is {version}; run `cargo xtask release prepare --apply` first."
        )));
    }
    check_files(version)?;
    let head = git::rev_parse("HEAD").ok_or_else(|| error("release commit: no commit yet"))?;
    let tag = version.tag();

    if dry_run {
        println!(
            "release commit: would commit {} files as \"{}\"",
            rewrite::FILES.len(),
            subject(version)
        );
        println!("release commit: would tag it {tag}");
        println!("release commit: would push the commit to {remote} {branch}, then {tag}");
        return Ok(0);
    }

    let tracking = fetch_state(remote, branch, &tag);
    let tip = git::rev_parse(&tracking);
    let earlier = earlier_attempt(&tag, &tracking, &head, version);
    if let Some(tip) = tip.filter(|tip| *tip != head && earlier.as_deref() != Some(tip.as_str())) {
        return Err(error(format!(
            "release commit: {remote}/{branch} is at {}, not at {}, the commit of this pipeline; refusing to publish out of order. Retry the release from the latest {branch} pipeline.",
            &tip[..8],
            &head[..8]
        )));
    }

    let release = match earlier {
        Some(found) => {
            println!(
                "release commit: reusing the release commit {} of an earlier attempt",
                &found[..8]
            );
            found
        }
        None => {
            let mut add = vec!["add", "--"];
            add.extend(rewrite::FILES);
            git_in(&add)?;
            let message = subject(version);
            let mut commit = vec!["commit", "--quiet", "-m", &message, "--only", "--"];
            commit.extend(rewrite::FILES);
            git_in(&commit)?;
            let created = git::rev_parse("HEAD")
                .ok_or_else(|| error("release commit: the commit vanished"))?;
            println!("release commit: committed {}", &created[..8]);
            created
        }
    };

    match git::rev_parse(&tag) {
        Some(tagged) if tagged == release => {}
        Some(other) => {
            return Err(error(format!(
                "release commit: tag {tag} is on {} and not on the release commit {}; refusing to move it.",
                &other[..8],
                &release[..8]
            )));
        }
        None => {
            git_in(&["tag", &tag, &release])?;
            println!("release commit: tagged {tag}");
        }
    }

    git_in(&[
        "push",
        "--quiet",
        remote,
        &format!("{release}:refs/heads/{branch}"),
    ])?;
    git_in(&["push", "--quiet", remote, &format!("refs/tags/{tag}")])?;
    println!("release commit: pushed {branch} and {tag} to {remote}");
    Ok(0)
}

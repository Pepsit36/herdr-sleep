//! Every access to the repository goes through the `git` command.
//!
//! Failures come back as `None`, never as an error: every caller has a sane
//! fallback (a missing file, an unknown ref, a repository without tags).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::commit::Commit;
use crate::version::Version;

/// Runs `git` with `args` and returns its standard output when it succeeds.
pub fn run(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The root of the working tree, where `CHANGELOG.md` lives.
pub fn top_level() -> Option<PathBuf> {
    let out = run(&["rev-parse", "--show-toplevel"])?;
    let path = out.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Non-merge commits of `range` (`A..B`), newest first; the whole history when
/// `range` is `None`.
pub fn commits(range: Option<&str>) -> Vec<Commit> {
    let mut args = vec!["log", "--format=%H%x1f%s%x1f%b%x1e", "--no-merges"];
    if let Some(range) = range {
        args.push(range);
    }
    run(&args)
        .unwrap_or_default()
        .split('\x1e')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            if record.is_empty() {
                return None;
            }
            let mut fields = record.splitn(3, '\x1f');
            let sha = fields.next()?.trim().to_string();
            let subject = fields.next().unwrap_or_default().to_string();
            let body = fields.next().unwrap_or_default().to_string();
            Some(Commit { sha, subject, body })
        })
        .collect()
}

fn version_tags(args: &[&str]) -> BTreeSet<Version> {
    run(args)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.starts_with('v')
                .then(|| Version::parse(line))
                .flatten()
        })
        .collect()
}

/// The `vX.Y.Z` tags of the repository, and the subset reachable from HEAD.
///
/// A branch pipeline fetches every tag, so a clone can hold tags published
/// after the branch was cut. Those versions have no section in the branch's
/// file and must not be asked for one: only the reachable ones are walked.
/// Whether the repository has any tag at all decides the bootstrap rule.
#[derive(Clone, Debug, Default)]
pub struct Tags {
    all: BTreeSet<Version>,
    reachable: BTreeSet<Version>,
}

impl Tags {
    pub fn load() -> Tags {
        Tags {
            all: version_tags(&["tag", "--list", "v[0-9]*"]),
            reachable: version_tags(&["tag", "--merged", "HEAD", "--list", "v[0-9]*"]),
        }
    }

    /// No `vX.Y.Z` tag anywhere: the repository has not published yet.
    pub fn bootstrap(&self) -> bool {
        self.all.is_empty()
    }

    pub fn exists(&self, version: Version) -> bool {
        self.all.contains(&version)
    }

    pub fn highest_reachable(&self) -> Option<Version> {
        self.reachable.iter().next_back().copied()
    }

    /// Reachable tags, lowest first.
    pub fn reachable(&self) -> impl Iterator<Item = Version> + '_ {
        self.reachable.iter().copied()
    }
}

pub fn rev_parse(reference: &str) -> Option<String> {
    let out = run(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{reference}^{{commit}}"),
    ])?;
    let sha = out.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

pub fn ref_exists(reference: &str) -> bool {
    rev_parse(reference).is_some()
}

pub fn merge_base(reference: &str) -> Option<String> {
    let out = run(&["merge-base", reference, "HEAD"])?;
    let sha = out.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

/// The content of `path` at `revision`, or `None` when it is not there.
pub fn show(revision: &str, path: &str) -> Option<String> {
    run(&["show", &format!("{revision}:{path}")])
}

/// The checked out branch, or `None` when HEAD is detached.
pub fn current_branch() -> Option<String> {
    let out = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let name = out.trim();
    (!name.is_empty() && name != "HEAD").then(|| name.to_string())
}

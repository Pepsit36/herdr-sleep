//! `cargo xtask release`: the steps of a release, each small enough to run
//! offline and to run twice. The CI pipeline only calls these commands.

mod commit;
mod github;
mod json;
mod notes;
mod prepare;
mod rewrite;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::cli::{Failure, Outcome, error, usage, wants_help};
use crate::git::{self, Tags};
use crate::plan;
use crate::version::Version;

pub const USAGE: &str = "\
Usage: cargo xtask release [--dry-run] <subcommand> [options]

Subcommands, in the order the pipeline runs them:
  prepare [--apply]
      Decide the release. Print `RELEASE=1` and `VERSION=X.Y.Z` (or `RELEASE=0`
      and exit 3 when nothing publishes). Refuses when CHANGELOG.md does not
      carry the computed version on top, or when its tag exists on a commit that
      is not this one's release commit. With RESUME_RELEASE=1, resume the last
      release (its commit and tag exist, nothing publishable follows): print the
      same lines, change nothing. With --apply, write the version into
      Cargo.toml, herdr-plugin.toml and Cargo.lock.
  set-version X.Y.Z
      Write a version, prerelease allowed, into the same three files (canary builds).
  notes [--version X.Y.Z]
      Print the CHANGELOG.md section of a version (default: the manifest's).
  commit [--remote NAME] [--branch NAME]
      Commit the three files as `chore(release): X.Y.Z [skip ci]`, tag it `vX.Y.Z`,
      push the branch and then the tag. Reuses a release commit, tag or pushed
      branch left by an earlier attempt instead of failing. Does nothing, and
      succeeds, with RESUME_RELEASE=1 (see prepare).
  publish-github [--dist DIR] [--version X.Y.Z] [--wait-seconds N] [--poll-seconds N]
      With GITHUB_TOKEN: wait for the tag on GitHub, create the release when it is
      missing, upload every archive and checksum of DIR that is not attached yet,
      then verify the uploaded sizes. GITHUB_REPOSITORY, GITHUB_API_URL and
      GITHUB_UPLOAD_URL override the defaults (Pepsit36/herdr-sleep, github.com).

Options:
  --dry-run   Say what would happen; change nothing and call no network.
  -h, --help  Print this help and exit.
";

/// Splits a boolean flag out of `args`.
pub(crate) fn take_flag(args: &[String], flag: &str) -> (bool, Vec<String>) {
    let rest: Vec<String> = args.iter().filter(|arg| *arg != flag).cloned().collect();
    (rest.len() != args.len(), rest)
}

/// The root of the working tree.
pub(crate) fn root() -> Result<PathBuf, Failure> {
    git::top_level().ok_or_else(|| error("not inside a git working tree"))
}

pub(crate) fn read(name: &str) -> Result<String, Failure> {
    let path = root()?.join(name);
    fs::read_to_string(&path)
        .map_err(|cause| error(format!("cannot read {}: {cause}", path.display())))
}

pub(crate) fn write(name: &str, text: &str) -> Result<(), Failure> {
    let path = root()?.join(name);
    fs::write(&path, text)
        .map_err(|cause| error(format!("cannot write {}: {cause}", path.display())))
}

/// The version `herdr-plugin.toml` carries now.
pub(crate) fn manifest_version() -> Result<String, Failure> {
    let text = read(rewrite::MANIFEST)?;
    text.lines()
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            let inner = line.strip_prefix("version = \"")?.strip_suffix('"')?;
            Some(inner.to_string())
        })
        .ok_or_else(|| error(format!("{} carries no version", rewrite::MANIFEST)))
}

/// The CI variable that asks to finish a release which stopped after its tag.
pub(crate) const RESUME_VARIABLE: &str = "RESUME_RELEASE";

/// With `RESUME_RELEASE=1`: the version of the last release, whose commit and
/// tag already exist, so that its publication can be finished. Without the
/// variable, `None`. Refuses when the last tag is not on a `chore(release)`
/// commit of this history, when the manifests do not carry its version, or
/// when a commit after it publishes something (that is a new release).
pub(crate) fn resume_version(tags: &Tags) -> Result<Option<Version>, Failure> {
    if std::env::var(RESUME_VARIABLE).as_deref() != Ok("1") {
        return Ok(None);
    }
    let fail = |why: String| error(format!("release: {RESUME_VARIABLE}=1 but {why}"));
    let version = tags
        .highest_reachable()
        .ok_or_else(|| fail("no release tag is reachable from HEAD.".to_string()))?;
    let tag = version.tag();
    let commit = git::rev_parse(&format!("{tag}^{{commit}}"))
        .ok_or_else(|| fail(format!("tag {tag} does not exist.")))?;
    let ancestor = git::run(&["merge-base", "--is-ancestor", &commit, "HEAD"]).is_some();
    let subject = git::run(&["log", "-1", "--format=%s", &commit]).unwrap_or_default();
    let prefix = format!("chore(release): {version}");
    let is_release = subject
        .trim()
        .strip_prefix(&prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '));
    if !ancestor || !is_release {
        return Err(fail(format!(
            "tag {tag} is not on a `{prefix}` commit in the history of HEAD."
        )));
    }
    if plan::release_version(tags).is_some() {
        return Err(fail(format!(
            "commits after {tag} publish a new version; that is a new release, not a resume."
        )));
    }
    let current = manifest_version()?;
    if current != version.to_string() {
        return Err(fail(format!(
            "{} says {current}, not {version}.",
            rewrite::MANIFEST
        )));
    }
    Ok(Some(version))
}

/// Runs `program` and returns its standard output, or its failure explained.
pub(crate) fn capture(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|cause| format!("cannot run {program}: {cause}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub fn run(args: &[String]) -> Outcome {
    if wants_help(args) {
        print!("{USAGE}");
        return Ok(0);
    }
    let (dry_run, args) = take_flag(args, "--dry-run");
    let Some((subcommand, rest)) = args.split_first() else {
        return Err(usage("release needs a subcommand"));
    };
    match subcommand.as_str() {
        "prepare" => prepare::run(rest, dry_run),
        "set-version" => prepare::set_version(rest, dry_run),
        "notes" => notes::run(rest),
        "commit" => commit::run(rest, dry_run),
        "publish-github" => github::run(rest, dry_run),
        other => Err(usage(format!("unknown release subcommand '{other}'"))),
    }
}

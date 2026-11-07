//! What a branch is about to publish: the base version, the commits that
//! decide the bump, and the section of `CHANGELOG.md` the branch owns.
//!
//! This is the single implementation of "which version does this publish".
//! The branch stamp, the branch check and the release job all go through it,
//! so the number an author stamps and the number published cannot differ.

use crate::changelog::{Changelog, FILE_NAME, Heading};
use crate::commit::Commit;
use crate::git::{self, Tags};
use crate::version::{Level, Version};

/// Refs tried, in order, when no `--base` is given: the fetched default
/// branch, then a local one.
pub const DEFAULT_REFS: [&str; 2] = ["origin/main", "main"];

/// The commits counted towards a version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Range {
    /// `<ref>..HEAD`.
    Since(String),
    /// Every commit reachable from HEAD.
    Whole,
}

impl Range {
    pub fn commits(&self) -> Vec<Commit> {
        match self {
            Range::Since(from) => git::commits(Some(&format!("{from}..HEAD"))),
            Range::Whole => git::commits(None),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Range::Since(from) => format!("{from}..HEAD"),
            Range::Whole => "the whole history".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub headings: Vec<Heading>,
    /// The section this branch stamped, if any: the top one, stamped, not
    /// tagged, and different from the top section at the merge base.
    pub owned: Option<Heading>,
    /// The highest of the last tag reachable from HEAD and the versions the
    /// file carries outside the owned section.
    pub base: Version,
    pub range: Range,
    pub merge_base: Option<String>,
    /// The ref the branch is measured against, when it exists.
    pub default_ref: Option<String>,
}

impl Plan {
    /// Where the counting starts, for a message.
    pub fn counting_from(&self) -> String {
        match (&self.merge_base, &self.range) {
            (Some(merge_base), _) => format!(
                "{} ({})",
                self.default_ref.as_deref().unwrap_or("the merge base"),
                &merge_base[..merge_base.len().min(8)]
            ),
            (None, Range::Since(tag)) => format!("the last tag {tag}"),
            (None, Range::Whole) => "the root commit".to_string(),
        }
    }
}

/// The ref a branch is measured against: the one requested, else the first of
/// `DEFAULT_REFS` that exists.
pub fn default_ref(requested: Option<&str>) -> Option<String> {
    match requested {
        Some(reference) => Some(reference.to_string()),
        None => DEFAULT_REFS
            .iter()
            .find(|reference| git::ref_exists(reference))
            .map(|reference| reference.to_string()),
    }
}

/// HEAD is the default branch: the branch of that name is checked out, or HEAD
/// is detached on the very commit the ref points to (a pipeline of `main`).
fn on_default_branch(reference: &str) -> bool {
    let short = reference.strip_prefix("origin/").unwrap_or(reference);
    match git::current_branch() {
        Some(branch) => branch == short,
        None => {
            git::rev_parse("HEAD").is_some() && git::rev_parse("HEAD") == git::rev_parse(reference)
        }
    }
}

/// What a branch is about to publish, measured against `requested` (or the
/// default branch).
pub fn branch_plan(changelog: &Changelog, requested: Option<&str>, tags: &Tags) -> Plan {
    let headings = changelog.headings();
    let default_ref = default_ref(requested).filter(|reference| git::ref_exists(reference));
    let on_default = default_ref.as_deref().is_some_and(on_default_branch);
    let merge_base = if on_default {
        None
    } else {
        default_ref.as_deref().and_then(git::merge_base)
    };
    let tag = tags.highest_reachable();

    // On the default branch the file is not consulted: the top section is the
    // one the release job is about to publish, and must not raise the base.
    let owned = if on_default {
        None
    } else {
        owned_section(&headings, merge_base.as_deref(), tags)
    };
    let base = if on_default {
        tag.unwrap_or(Version::ZERO)
    } else {
        headings
            .iter()
            .filter(|heading| Some(**heading) != owned)
            .map(|heading| heading.version)
            .chain(tag)
            .max()
            .unwrap_or(Version::ZERO)
    };
    let range = match (&merge_base, tag) {
        (Some(merge_base), _) => Range::Since(merge_base.clone()),
        (None, Some(tag)) => Range::Since(tag.tag()),
        (None, None) => Range::Whole,
    };

    Plan {
        headings,
        owned,
        base,
        range,
        merge_base,
        default_ref,
    }
}

/// The section the branch stamped itself. A stamped, untagged top section is
/// inherited rather than owned when the merge base already had it on top,
/// with one exception: before the first release every branch publishes
/// `0.1.0`, so a `[0.1.0]` section inherited from the default branch is the
/// branch's own to fold into.
pub fn owned_section(
    headings: &[Heading],
    merge_base: Option<&str>,
    tags: &Tags,
) -> Option<Heading> {
    let top = *headings.first()?;
    if !top.stamped || tags.exists(top.version) {
        return None;
    }
    let Some(merge_base) = merge_base else {
        return Some(top);
    };
    let before = git::show(merge_base, FILE_NAME)
        .map(|text| Changelog::parse(&text))
        .and_then(|changelog| changelog.headings().first().copied());
    match before {
        Some(before) if before.version == top.version => {
            (tags.bootstrap() && top.version == Version::FIRST).then_some(top)
        }
        _ => Some(top),
    }
}

/// The version `level` publishes on top of `base`. While the repository has
/// no tag at all, the first release is `0.1.0` whatever the file says.
pub fn publish_version(tags: &Tags, base: Version, level: Level) -> Version {
    if tags.bootstrap() {
        Version::FIRST
    } else {
        base.bump(level)
    }
}

/// What the release job publishes after a merge: the commits since the last
/// tag reachable from HEAD, merges left out. `None` when nothing publishes.
pub fn release_version(tags: &Tags) -> Option<Version> {
    let (base, range) = match tags.highest_reachable() {
        Some(tag) => (tag, Range::Since(tag.tag())),
        None => (Version::ZERO, Range::Whole),
    };
    let level = crate::commit::bump_level(&range.commits())?;
    Some(publish_version(tags, base, level))
}

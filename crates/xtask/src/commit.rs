//! Reading a commit as Conventional Commits defines it: which level it
//! publishes, and the parts of its subject an entry can be written from.
//!
//! The type is read from the subject line only. A `fix:` or `feat:` further
//! down a body (a quoted message, a note) is prose and must not publish a
//! version on its own, since a publication cannot be taken back. The body is
//! only read for the breaking footer, in both spellings the specification
//! allows: `BREAKING CHANGE:` and `BREAKING-CHANGE:`.

use std::sync::LazyLock;

use regex::Regex;

use crate::version::Level;

static SUBJECT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z]+)(?:\(([^)]*)\))?(!)?:\s+(.+)$").expect("valid regex"));
static BREAKING_FOOTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^BREAKING[ -]CHANGE:").expect("valid regex"));

/// A non-merge commit as `git log` hands it over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub body: String,
}

/// The parts of a conventional subject, `type(scope)!: description`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subject<'a> {
    pub kind: &'a str,
    pub scope: Option<&'a str>,
    pub bang: bool,
    pub description: &'a str,
}

/// Reads a subject line, or `None` when it does not follow the convention.
pub fn parse_subject(line: &str) -> Option<Subject<'_>> {
    let caps = SUBJECT.captures(line.trim())?;
    let scope = caps
        .get(2)
        .map(|found| found.as_str().trim())
        .filter(|scope| !scope.is_empty());
    Some(Subject {
        kind: caps.get(1)?.as_str(),
        scope,
        bang: caps.get(3).is_some(),
        description: caps.get(4)?.as_str(),
    })
}

impl Commit {
    pub fn subject(&self) -> Option<Subject<'_>> {
        parse_subject(&self.subject)
    }

    /// A breaking marker on any type publishes a major: `!` after the type or
    /// scope, or a breaking footer anywhere in the body.
    pub fn breaking(&self) -> bool {
        self.subject().is_some_and(|subject| subject.bang) || BREAKING_FOOTER.is_match(&self.body)
    }

    /// What this commit publishes on its own: `feat` a minor, `fix` a patch, a
    /// breaking marker a major. Every other type publishes nothing.
    pub fn level(&self) -> Option<Level> {
        if self.breaking() {
            return Some(Level::Major);
        }
        match self.subject()?.kind {
            "feat" => Some(Level::Minor),
            "fix" => Some(Level::Patch),
            _ => None,
        }
    }
}

/// The level a set of commits publishes together: the highest of them, or
/// `None` when none of them publishes anything.
pub fn bump_level(commits: &[Commit]) -> Option<Level> {
    commits.iter().filter_map(Commit::level).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, body: &str) -> Commit {
        Commit {
            sha: "0123456789abcdef".into(),
            subject: subject.into(),
            body: body.into(),
        }
    }

    #[test]
    fn reads_every_part_of_a_subject() {
        let subject = parse_subject("feat(watch)!: wake on demand").unwrap();
        assert_eq!(subject.kind, "feat");
        assert_eq!(subject.scope, Some("watch"));
        assert!(subject.bang);
        assert_eq!(subject.description, "wake on demand");

        let subject = parse_subject("fix: trim the name").unwrap();
        assert_eq!(subject.scope, None);
        assert!(!subject.bang);
        assert_eq!(parse_subject("docs(): empty scope").unwrap().scope, None);
    }

    #[test]
    fn rejects_subjects_outside_the_convention() {
        for line in [
            "Merge branch 'x'",
            "feat:no space",
            "Feat: capital",
            "feat (x): spaced",
            "",
        ] {
            assert!(parse_subject(line).is_none(), "{line:?}");
        }
    }

    #[test]
    fn maps_types_to_levels() {
        assert_eq!(commit("feat: a", "").level(), Some(Level::Minor));
        assert_eq!(commit("feat(scope): a", "").level(), Some(Level::Minor));
        assert_eq!(commit("fix: a", "").level(), Some(Level::Patch));
        for kind in [
            "perf", "build", "deps", "revert", "security", "refactor", "docs", "chore", "ci",
            "test", "style",
        ] {
            assert_eq!(commit(&format!("{kind}: a"), "").level(), None, "{kind}");
        }
        assert_eq!(commit("not conventional", "").level(), None);
    }

    #[test]
    fn breaking_markers_publish_a_major_on_any_type() {
        assert_eq!(commit("feat!: a", "").level(), Some(Level::Major));
        assert_eq!(commit("chore(ci)!: a", "").level(), Some(Level::Major));
        assert_eq!(
            commit("refactor: a", "Details.\n\nBREAKING CHANGE: gone").level(),
            Some(Level::Major)
        );
        assert_eq!(
            commit("fix: a", "BREAKING-CHANGE: gone").level(),
            Some(Level::Major)
        );
        assert_eq!(
            commit("fix: a", "not a BREAKING CHANGE: marker").level(),
            Some(Level::Patch)
        );
    }

    #[test]
    fn the_body_never_decides_the_type() {
        assert_eq!(commit("chore: a", "feat: hidden in the body").level(), None);
        assert_eq!(commit("docs: a", "fix: quoted").level(), None);
    }

    #[test]
    fn the_highest_level_wins() {
        let commits = [
            commit("fix: a", ""),
            commit("feat: b", ""),
            commit("chore: c", ""),
        ];
        assert_eq!(bump_level(&commits), Some(Level::Minor));
        assert_eq!(bump_level(&[commit("chore: c", "")]), None);
        assert_eq!(bump_level(&[]), None);
    }
}

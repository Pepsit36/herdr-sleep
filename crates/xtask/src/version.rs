//! Version numbers: parsing, ordering and the bump each commit level applies.

use std::fmt;

/// How much a commit publishes. Ordered so that the highest level wins when
/// several commits disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Patch,
    Minor,
    Major,
}

/// A released version, `X.Y.Z`, with its natural ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub const ZERO: Version = Version::new(0, 0, 0);

    /// The version of the first release. While the repository carries no tag at
    /// all, every publishable branch publishes this one.
    pub const FIRST: Version = Version::new(0, 1, 0);

    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Version {
            major,
            minor,
            patch,
        }
    }

    /// Reads `X.Y.Z` or `vX.Y.Z`, nothing else: a prerelease suffix or a
    /// missing part is not a version this tooling knows.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let (major, minor, patch) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        Some(Version::new(number(major)?, number(minor)?, number(patch)?))
    }

    pub fn bump(self, level: Level) -> Version {
        match level {
            Level::Major => Version::new(self.major + 1, 0, 0),
            Level::Minor => Version::new(self.major, self.minor + 1, 0),
            Level::Patch => Version::new(self.major, self.minor, self.patch + 1),
        }
    }

    /// The git tag that marks this version as released.
    pub fn tag(self) -> String {
        format!("v{self}")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_and_without_prefix() {
        assert_eq!(Version::parse("1.2.3"), Some(Version::new(1, 2, 3)));
        assert_eq!(Version::parse("v0.10.0"), Some(Version::new(0, 10, 0)));
        assert_eq!(Version::parse(" v1.0.0 "), Some(Version::new(1, 0, 0)));
    }

    #[test]
    fn rejects_what_is_not_a_version() {
        for text in [
            "",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "v1.2.3-pre.1",
            "1..3",
            "a.b.c",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn orders_numerically_not_lexically() {
        assert!(Version::new(0, 10, 0) > Version::new(0, 9, 9));
        assert!(Version::new(1, 0, 0) > Version::new(0, 99, 99));
        assert!(Level::Major > Level::Minor && Level::Minor > Level::Patch);
    }

    #[test]
    fn bumps_and_resets_the_lower_parts() {
        let version = Version::new(1, 2, 3);
        assert_eq!(version.bump(Level::Patch), Version::new(1, 2, 4));
        assert_eq!(version.bump(Level::Minor), Version::new(1, 3, 0));
        assert_eq!(version.bump(Level::Major), Version::new(2, 0, 0));
        assert_eq!(version.tag(), "v1.2.3");
    }
}

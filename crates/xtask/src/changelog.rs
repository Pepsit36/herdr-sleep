//! The sections of `CHANGELOG.md`: reading them and rewriting them in place.

use std::sync::LazyLock;

use regex::Regex;

use crate::entries::STAMP_MARKER;
use crate::version::Version;

pub const FILE_NAME: &str = "CHANGELOG.md";
pub const UNRELEASED_HEADING: &str = "## [Unreleased]";

static UNRELEASED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^##\s*\[unreleased\]").expect("valid regex"));
static ANY_SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^##\s*\[").expect("valid regex"));
static LEVEL_TWO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^##\s").expect("valid regex"));
static VERSION_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^##\s*\[(\d+\.\d+\.\d+)\]").expect("valid regex"));

/// A `## [X.Y.Z]` heading and what its section says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heading {
    /// Line index of the heading.
    pub index: usize,
    pub version: Version,
    /// Whether the section carries the stamp marker.
    pub stamped: bool,
}

/// The file as lines, split so that a trailing newline survives a round trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changelog {
    pub lines: Vec<String>,
}

impl Changelog {
    pub fn parse(text: &str) -> Changelog {
        Changelog {
            lines: text.split('\n').map(str::to_string).collect(),
        }
    }

    pub fn render(&self) -> String {
        self.lines.join("\n")
    }

    pub fn unreleased_index(&self) -> Option<usize> {
        self.lines.iter().position(|line| UNRELEASED.is_match(line))
    }

    /// The first `## [` heading of any kind, where a missing `[Unreleased]`
    /// section is inserted.
    pub fn first_section_index(&self) -> Option<usize> {
        self.lines
            .iter()
            .position(|line| ANY_SECTION.is_match(line))
    }

    /// Index of the heading that closes the section opened at `from`, or the
    /// number of lines when the section runs to the end of the file.
    pub fn section_end(&self, from: usize) -> usize {
        self.lines
            .iter()
            .enumerate()
            .skip(from + 1)
            .find(|(_, line)| LEVEL_TWO.is_match(line))
            .map_or(self.lines.len(), |(index, _)| index)
    }

    /// Text of the section opened at `from`, heading excluded.
    pub fn body_at(&self, from: usize) -> String {
        self.lines[from + 1..self.section_end(from)].join("\n")
    }

    /// Text of the `## [version]` section, heading excluded, or `None` when absent.
    pub fn version_body(&self, version: Version) -> Option<String> {
        let index = self
            .headings()
            .into_iter()
            .find(|heading| heading.version == version)?
            .index;
        Some(self.body_at(index))
    }

    /// Every `## [X.Y.Z]` section, in file order.
    pub fn headings(&self) -> Vec<Heading> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                let version = Version::parse(&VERSION_HEADING.captures(line)?[1])?;
                Some(Heading {
                    index,
                    version,
                    stamped: self.body_at(index).contains(STAMP_MARKER),
                })
            })
            .collect()
    }

    /// Version to section body, in file order (newest first). A version that
    /// appears twice is a badly resolved conflict and is reported as such.
    pub fn section_bodies(&self) -> Result<Vec<(Version, String)>, String> {
        let headings = self.headings();
        let mut duplicates: Vec<Version> = Vec::new();
        for heading in &headings {
            let repeated = headings
                .iter()
                .filter(|other| other.version == heading.version)
                .count();
            if repeated > 1 && !duplicates.contains(&heading.version) {
                duplicates.push(heading.version);
            }
        }
        if !duplicates.is_empty() {
            let listed: Vec<String> = duplicates.iter().map(Version::to_string).collect();
            return Err(format!(
                "{FILE_NAME} carries the same version twice: {}. Keep one section per version.",
                listed.join(", ")
            ));
        }
        Ok(headings
            .into_iter()
            .map(|heading| (heading.version, self.body_at(heading.index)))
            .collect())
    }

    /// Replaces the lines `from..to` with `replacement`.
    pub fn splice(&mut self, from: usize, to: usize, replacement: Vec<String>) {
        self.lines.splice(from..to, replacement);
    }
}

/// Splits a body into lines the way the file is split, so that a body ending
/// with a newline leaves a blank line before the next heading.
pub fn lines_of(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Changelog\n\nIntro.\n\n## [Unreleased]\n\n### Added\n\n- New.\n\n## [0.2.0] - 2026-01-02\n\n<!-- stamped -->\n\n### Fixed\n\n- Fixed.\n\n## [0.1.0] - 2026-01-01\n\n- Old.\n";

    #[test]
    fn round_trips_the_text() {
        assert_eq!(Changelog::parse(SAMPLE).render(), SAMPLE);
        assert_eq!(Changelog::parse("").render(), "");
    }

    #[test]
    fn finds_sections_and_their_bodies() {
        let changelog = Changelog::parse(SAMPLE);
        assert_eq!(changelog.unreleased_index(), Some(4));
        assert_eq!(changelog.first_section_index(), Some(4));
        assert_eq!(changelog.section_end(4), 10);
        assert_eq!(changelog.body_at(4), "\n### Added\n\n- New.\n");
        assert_eq!(
            changelog.version_body(Version::new(0, 1, 0)).unwrap(),
            "\n- Old.\n"
        );
        assert_eq!(changelog.version_body(Version::new(9, 9, 9)), None);
    }

    #[test]
    fn lists_version_headings_with_their_stamp() {
        let headings = Changelog::parse(SAMPLE).headings();
        assert_eq!(
            headings,
            vec![
                Heading {
                    index: 10,
                    version: Version::new(0, 2, 0),
                    stamped: true
                },
                Heading {
                    index: 18,
                    version: Version::new(0, 1, 0),
                    stamped: false
                },
            ]
        );
    }

    #[test]
    fn refuses_duplicate_versions() {
        let changelog = Changelog::parse("## [0.1.0]\n\n- A.\n\n## [0.1.0]\n\n- B.\n");
        let error = changelog.section_bodies().unwrap_err();
        assert!(error.contains("0.1.0"), "{error}");
        assert_eq!(Changelog::parse(SAMPLE).section_bodies().unwrap().len(), 2);
    }

    #[test]
    fn splices_lines_in_place() {
        let mut changelog = Changelog::parse("a\nb\nc\n");
        changelog.splice(1, 2, lines_of("x\ny"));
        assert_eq!(changelog.render(), "a\nx\ny\nc\n");
    }
}

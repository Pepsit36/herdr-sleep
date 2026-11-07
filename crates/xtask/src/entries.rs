//! Changelog entries: the bullets written from commit subjects when an author
//! wrote nothing, and the folding of two entry bodies into one.

use std::sync::LazyLock;

use regex::Regex;

use crate::commit::Commit;

/// Keep a Changelog categories, in the order the file lists them.
pub const CATEGORIES: [&str; 5] = ["Added", "Changed", "Removed", "Fixed", "Security"];

/// Written by `stamp` into the section it owns. It marks a section whose
/// version is not tagged yet, and tells `coverage` that the section describes
/// its whole tag interval.
pub const STAMP_MARKER: &str = "<!-- stamped -->";

/// The body of a section that publishes a version without a visible change.
pub const NO_CHANGES: &str = "_No notable changes._";

static CATEGORY_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^###\s+(\w+)").expect("valid regex"));

/// Conventional type to Keep a Changelog category. Types absent from this list
/// produce no bullet.
fn category(kind: &str) -> Option<&'static str> {
    match kind {
        "feat" => Some("Added"),
        "fix" => Some("Fixed"),
        "perf" | "refactor" | "docs" | "build" | "deps" => Some("Changed"),
        "revert" => Some("Removed"),
        "security" => Some("Security"),
        _ => None,
    }
}

/// The bullet a commit suggests, with its category, or `None` when the commit
/// is not worth an entry.
pub fn bullet(commit: &Commit) -> Option<(&'static str, String)> {
    let subject = commit.subject()?;
    let breaking = commit.breaking();
    // A breaking change publishes a major whatever its type, so it is listed
    // even when that type is normally silent. Keep a Changelog has no category
    // for it, and a breaking change is a change: Changed.
    let category = if breaking {
        "Changed"
    } else {
        category(subject.kind)?
    };

    let description = sentence(subject.description);
    let scope = subject
        .scope
        .and_then(pretty_scope)
        .or_else(|| (subject.kind == "docs").then(|| "Docs".to_string()));
    let mut text = match scope {
        Some(scope) => format!("**{scope}:** {description}"),
        None => description,
    };
    if breaking {
        text = format!("**Breaking:** {text}");
    }
    Some((category, format!("- {text}")))
}

/// A full sentence: first letter capitalised, trailing period.
pub fn sentence(description: &str) -> String {
    let text = description.trim();
    let mut chars = text.chars();
    let mut out: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// A scope as a reader would write it: known acronyms upper cased, dashes
/// turned into spaces, first letter capitalised.
pub fn pretty_scope(scope: &str) -> Option<String> {
    let scope = scope.trim();
    if scope.is_empty() {
        return None;
    }
    let known = match scope {
        "api" => "API",
        "ci" => "CI",
        "cli" => "CLI",
        "http" => "HTTP",
        "json" => "JSON",
        "ph" => "Placeholder",
        "tui" => "TUI",
        "ui" => "UI",
        "url" => "URL",
        _ => "",
    };
    if !known.is_empty() {
        return Some(known.to_string());
    }
    let spaced = scope.replace('-', " ");
    let mut chars = spaced.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}

/// The entry body for a set of commits, categories in Keep a Changelog order,
/// or `None` when no commit is worth a bullet.
pub fn body(commits: &[Commit]) -> Option<String> {
    let mut found: Vec<(&'static str, Vec<String>)> = Vec::new();
    for commit in commits {
        let Some((category, text)) = bullet(commit) else {
            continue;
        };
        let bullets = match found.iter_mut().find(|(name, _)| *name == category) {
            Some((_, bullets)) => bullets,
            None => {
                found.push((category, Vec::new()));
                &mut found.last_mut().expect("just pushed").1
            }
        };
        if !bullets.contains(&text) {
            bullets.push(text);
        }
    }
    let blocks: Vec<String> = CATEGORIES
        .iter()
        .filter_map(|name| {
            let (_, bullets) = found.iter().find(|(found, _)| found == name)?;
            Some(format!("### {name}\n\n{}\n", bullets.join("\n")))
        })
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

fn is_bullet(line: &str) -> bool {
    line.starts_with("- ") || line.starts_with("* ")
}

/// Bullet lines of a section body, which is what an entry is.
pub fn entry_lines(body: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|line| is_bullet(line))
        .map(str::to_string)
        .collect()
}

/// Whether a body describes something: a bullet, or the explicit statement that
/// there is nothing to describe.
pub fn describes_something(body: &str) -> bool {
    body.lines()
        .map(str::trim)
        .any(|line| is_bullet(line) || line == NO_CHANGES)
}

/// Folds two Keep a Changelog bodies into one: the section a branch already
/// stamped, and the new entries written under `[Unreleased]`. Categories keep
/// the Keep a Changelog order, entries keep their order of appearance, and
/// duplicates are dropped. Anything written outside a category is kept first.
pub fn merge_bodies(existing: &str, incoming: &str) -> String {
    let mut preamble: Vec<String> = Vec::new();
    let mut categories: Vec<(String, Vec<String>)> = Vec::new();

    for text in [existing, incoming] {
        let mut current: Option<usize> = None;
        for raw in text.lines() {
            let line = raw.trim_end();
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed == STAMP_MARKER || trimmed == NO_CHANGES {
                continue;
            }
            if let Some(caps) = CATEGORY_HEADING.captures(line) {
                let name = caps[1].to_string();
                current = Some(
                    match categories.iter().position(|(found, _)| *found == name) {
                        Some(index) => index,
                        None => {
                            categories.push((name, Vec::new()));
                            categories.len() - 1
                        }
                    },
                );
            } else if let Some(index) = current {
                let entries = &mut categories[index].1;
                match entries.last_mut() {
                    // A continuation line belongs to the entry above it.
                    Some(last) if !is_bullet(trimmed) => {
                        last.push('\n');
                        last.push_str(line);
                    }
                    _ => entries.push(line.to_string()),
                }
            } else if !preamble.iter().any(|kept| kept == line) {
                preamble.push(line.to_string());
            }
        }
    }

    let mut ordered: Vec<&str> = CATEGORIES.to_vec();
    ordered.extend(
        categories
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| !CATEGORIES.contains(name)),
    );
    let blocks: Vec<String> = ordered
        .iter()
        .filter_map(|name| {
            let (_, entries) = categories.iter().find(|(found, _)| found == name)?;
            let mut unique: Vec<&str> = Vec::new();
            for entry in entries {
                if !unique.contains(&entry.as_str()) {
                    unique.push(entry);
                }
            }
            (!unique.is_empty()).then(|| format!("### {name}\n\n{}\n", unique.join("\n")))
        })
        .collect();

    let mut out = String::new();
    if !preamble.is_empty() {
        out.push_str(&preamble.join("\n"));
        out.push('\n');
        if !blocks.is_empty() {
            out.push('\n');
        }
    }
    if blocks.is_empty() && preamble.is_empty() {
        out.push_str(NO_CHANGES);
        out.push('\n');
    } else {
        out.push_str(&blocks.join("\n"));
    }
    out
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
    fn sentences_are_capitalised_and_closed() {
        assert_eq!(
            sentence("wake the agent on demand"),
            "Wake the agent on demand."
        );
        assert_eq!(sentence("Already done."), "Already done.");
        assert_eq!(sentence("really?"), "Really?");
        assert_eq!(sentence("  "), "");
    }

    #[test]
    fn scopes_read_like_words() {
        assert_eq!(pretty_scope("watch"), Some("Watch".into()));
        assert_eq!(pretty_scope("sidebar-token"), Some("Sidebar token".into()));
        assert_eq!(pretty_scope("cli"), Some("CLI".into()));
        assert_eq!(pretty_scope("ph"), Some("Placeholder".into()));
        assert_eq!(pretty_scope(" "), None);
    }

    #[test]
    fn bullets_follow_the_type_and_scope() {
        assert_eq!(
            bullet(&commit("feat(watch): wake on demand", "")),
            Some(("Added", "- **Watch:** Wake on demand.".into()))
        );
        assert_eq!(
            bullet(&commit("fix: trim names", "")),
            Some(("Fixed", "- Trim names.".into()))
        );
        assert_eq!(
            bullet(&commit("docs: explain the token", "")),
            Some(("Changed", "- **Docs:** Explain the token.".into()))
        );
        assert_eq!(
            bullet(&commit("chore!: drop the old flag", "")),
            Some(("Changed", "- **Breaking:** Drop the old flag.".into()))
        );
        assert_eq!(
            bullet(&commit("revert: the change", "")),
            Some(("Removed", "- The change.".into()))
        );
        for silent in [
            "chore: a",
            "ci: a",
            "test: a",
            "style: a",
            "unknown: a",
            "no convention",
        ] {
            assert_eq!(bullet(&commit(silent, "")), None, "{silent}");
        }
    }

    #[test]
    fn body_groups_bullets_in_category_order() {
        let commits = [
            commit("fix(ph): free the memory", ""),
            commit("feat: sleep", ""),
            commit("feat: sleep", ""),
            commit("chore: nothing", ""),
        ];
        assert_eq!(
            body(&commits).unwrap(),
            "### Added\n\n- Sleep.\n\n### Fixed\n\n- **Placeholder:** Free the memory.\n"
        );
        assert_eq!(body(&[commit("chore: nothing", "")]), None);
    }

    #[test]
    fn merge_folds_categories_and_drops_duplicates() {
        let existing = "\n<!-- stamped -->\n\n### Fixed\n\n- One.\n\n### Added\n\n- Two.\n";
        let incoming = "### Added\n\n- Two.\n- Three.\n\n### Security\n\n- Four.\n";
        assert_eq!(
            merge_bodies(existing, incoming),
            "### Added\n\n- Two.\n- Three.\n\n### Fixed\n\n- One.\n\n### Security\n\n- Four.\n"
        );
    }

    #[test]
    fn merge_keeps_continuation_lines_with_their_bullet() {
        let existing = "### Added\n\n- A long entry\n  that continues.\n";
        let incoming = "### Added\n\n- A long entry\n  that continues.\n- Another.\n";
        assert_eq!(
            merge_bodies(existing, incoming),
            "### Added\n\n- A long entry\n  that continues.\n- Another.\n"
        );
    }

    #[test]
    fn merge_keeps_the_preamble_and_handles_emptiness() {
        assert_eq!(merge_bodies("", ""), "_No notable changes._\n");
        assert_eq!(
            merge_bodies("<!-- stamped -->\n\n_No notable changes._\n", ""),
            "_No notable changes._\n"
        );
        assert_eq!(
            merge_bodies("A note.\n\n### Added\n\n- One.\n", "A note.\n"),
            "A note.\n\n### Added\n\n- One.\n"
        );
        assert_eq!(
            merge_bodies("### Other\n\n- Odd.\n", "### Added\n\n- One.\n"),
            "### Added\n\n- One.\n\n### Other\n\n- Odd.\n"
        );
    }

    #[test]
    fn entry_lines_are_the_bullets() {
        let body = "<!-- stamped -->\n\n### Added\n\n- One.\n  continued\n* Two.\n";
        assert_eq!(
            entry_lines(body),
            vec!["- One.".to_string(), "* Two.".to_string()]
        );
        assert!(describes_something(body));
        assert!(describes_something("_No notable changes._"));
        assert!(!describes_something("<!-- stamped -->\n\n### Added\n"));
    }
}

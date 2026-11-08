//! End-to-end tests of `cargo xtask` against throwaway repositories.
//!
//! Every test builds its own repository in the temporary directory, with a
//! bare `origin` holding `main`, so that `origin/main` exists as it does in a
//! pipeline. Tags are created with `git tag`, releases are simulated by
//! writing the sections the release job would leave behind.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU32 = AtomicU32::new(0);

const HEADER: &str = "# Changelog\n\nIntro.\n\n";
const MARKER: &str = "<!-- stamped -->";

struct Run {
    code: i32,
    out: String,
    err: String,
}

struct Repo {
    root: PathBuf,
    dir: PathBuf,
    files: Cell<u32>,
}

impl Repo {
    fn new() -> Repo {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            "xtask-test-{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let root = std::env::temp_dir().join(name);
        let dir = root.join("work");
        let origin = root.join("origin.git");
        fs::create_dir_all(&dir).unwrap();
        git(&root, &["init", "-q", "--bare", "origin.git"]);
        git(&dir, &["init", "-q", "-b", "main"]);
        let repo = Repo {
            root,
            dir,
            files: Cell::new(0),
        };
        for (key, value) in [
            ("user.name", "Test Author"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
        ] {
            repo.git(&["config", key, value]);
        }
        repo.git(&["remote", "add", "origin", origin.to_str().unwrap()]);
        repo.commit("chore: initial commit");
        repo.push_main();
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.dir, args)
    }

    /// Adds a commit carrying `message` (the whole message, body included).
    fn commit(&self, message: &str) {
        let count = self.files.get() + 1;
        self.files.set(count);
        self.write(&format!("notes/{count}.txt"), &format!("note {count}\n"));
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn tag(&self, name: &str) {
        self.git(&["tag", name]);
    }

    fn branch(&self, name: &str) {
        self.git(&["checkout", "-q", "-b", name]);
    }

    fn checkout(&self, name: &str) {
        self.git(&["checkout", "-q", name]);
    }

    fn push_main(&self) {
        self.git(&["push", "-q", "origin", "main:main"]);
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn write_changelog(&self, sections: &str) {
        self.write("CHANGELOG.md", &format!("{HEADER}{sections}"));
    }

    /// Writes the changelog and commits it, so that the merge base carries it.
    fn commit_changelog(&self, sections: &str, message: &str) {
        self.write_changelog(sections);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn changelog(&self) -> String {
        fs::read_to_string(self.dir.join("CHANGELOG.md")).unwrap_or_default()
    }

    fn xtask(&self, args: &[&str]) -> Run {
        self.xtask_with_env(args, &[])
    }

    fn xtask_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .args(args)
            .current_dir(&self.dir)
            .env_remove("CI_COMMIT_BRANCH")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE");
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().expect("xtask runs");
        Run {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Simulates a published 0.1.0 on main: tag, stamped section, pushed.
    fn with_first_release(self) -> Repo {
        self.commit_changelog(
            &released_section("0.1.0", "- First."),
            "chore(release): 0.1.0",
        );
        self.tag("v0.1.0");
        self.push_main();
        self
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn released_section(version: &str, bullets: &str) -> String {
    format!(
        "## [Unreleased]\n\n## [{version}] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n{bullets}\n"
    )
}

fn version(repo: &Repo) -> Run {
    repo.xtask(&["changelog", "version", "--base", "origin/main"])
}

fn assert_version(repo: &Repo, expected: &str) {
    let run = version(repo);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out.trim(), expected, "{}", run.err);
}

fn assert_nothing_publishes(repo: &Repo) {
    let run = version(repo);
    assert_eq!(run.code, 3, "{}{}", run.out, run.err);
    assert!(run.out.is_empty(), "stdout must stay empty: {}", run.out);
    assert!(run.err.contains("no publishable commit"), "{}", run.err);
}

// Version rule.

#[test]
fn version_follows_the_commit_types() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/rule");
    repo.commit("chore: housekeeping");
    assert_nothing_publishes(&repo);
    repo.commit("docs: explain\n\nfix: this line is prose, not a type");
    assert_nothing_publishes(&repo);
    repo.commit("fix(watch): trim the name");
    assert_version(&repo, "0.1.1");
    repo.commit("feat: sleep on demand");
    assert_version(&repo, "0.2.0");
    repo.commit("refactor(cli)!: rename the flag");
    assert_version(&repo, "1.0.0");
    let run = version(&repo);
    assert!(
        run.err.contains("Counting from origin/main (")
            && run.err.contains("base 0.1.0, publishing 1.0.0."),
        "{}",
        run.err
    );
}

#[test]
fn breaking_footer_publishes_a_major_on_any_type() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/footer");
    repo.commit("chore: drop the old config\n\nBREAKING CHANGE: the old key is gone");
    assert_version(&repo, "1.0.0");
}

#[test]
fn merge_commits_are_not_read() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/merge");
    repo.branch("feature/inner");
    repo.commit("chore: nothing visible");
    repo.checkout("feature/merge");
    repo.git(&[
        "merge",
        "-q",
        "--no-ff",
        "-m",
        "feat: a title that must not publish",
        "feature/inner",
    ]);
    assert_nothing_publishes(&repo);

    repo.branch("feature/inner-feat");
    repo.commit("feat: the real change");
    repo.checkout("feature/merge");
    repo.git(&[
        "merge",
        "-q",
        "--no-ff",
        "-m",
        "chore: merge title",
        "feature/inner-feat",
    ]);
    assert_version(&repo, "0.2.0");
}

#[test]
fn version_counts_since_the_last_tag_on_the_default_branch() {
    let repo = Repo::new().with_first_release();
    repo.commit("fix: on main after the release");
    repo.commit("feat: also on main");
    repo.push_main();
    assert_version(&repo, "0.2.0");
    let run = repo.xtask(&["changelog", "version", "--since", "v0.1.0"]);
    assert_eq!(run.out.trim(), "0.2.0", "{}", run.err);
}

// Bootstrap rule.

#[test]
fn before_the_first_tag_everything_publishes_0_1_0() {
    let repo = Repo::new();
    repo.branch("feature/first");
    repo.commit("chore: still nothing");
    assert_nothing_publishes(&repo);
    repo.commit("feat: one");
    repo.commit("feat: two");
    repo.commit("feat!: three");
    assert_version(&repo, "0.1.0");
    let run = version(&repo);
    assert!(run.err.contains("no tag yet"), "{}", run.err);

    repo.write_changelog("## [Unreleased]\n\n## [0.3.0] - 2026-01-01\n\n- Hand written.\n");
    assert_version(&repo, "0.1.0");
}

#[test]
fn bootstrap_on_the_default_branch_counts_the_whole_history() {
    let repo = Repo::new();
    assert_nothing_publishes(&repo);
    repo.commit("fix: first publishable commit");
    assert_version(&repo, "0.1.0");
}

// Base version.

#[test]
fn base_is_the_highest_of_tags_and_untouched_sections() {
    let repo = Repo::new().with_first_release();
    repo.commit("feat: merged before, not released yet");
    repo.commit_changelog(
        &format!(
            "## [Unreleased]\n\n## [0.3.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Pending.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        ),
        "docs(changelog): stamp 0.3.0",
    );
    repo.push_main();
    repo.branch("feature/on-top");
    repo.commit("fix: small");
    assert_version(&repo, "0.3.1");

    let run = repo.xtask(&[
        "changelog",
        "stamp",
        "--date",
        "2026-02-01",
        "--base",
        "origin/main",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        repo.changelog().contains("## [0.3.1] - 2026-02-01"),
        "{}",
        repo.changelog()
    );
    // The owned section never raises the base of its own branch.
    assert_version(&repo, "0.3.1");
}

#[test]
fn base_comes_from_the_tag_when_the_file_says_less() {
    let repo = Repo::new().with_first_release();
    repo.tag("v0.4.2");
    repo.push_main();
    repo.branch("feature/tagged");
    repo.commit("feat: more");
    assert_version(&repo, "0.5.0");
}

// Stamp.

#[test]
fn stamp_then_restamp_folds_the_new_entries() {
    let repo = Repo::new().with_first_release();
    repo.branch("bugfix/fold");
    repo.commit("fix(watch): trim the name");
    repo.write_changelog(&format!(
        "## [Unreleased]\n\n### Fixed\n\n- Names are trimmed.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
    ));

    let run = repo.xtask(&[
        "changelog",
        "stamp",
        "--date",
        "2026-02-02",
        "--base",
        "origin/main",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        run.out
            .contains("CHANGELOG.md now carries `## [0.1.1] - 2026-02-02`"),
        "{}",
        run.out
    );
    assert_eq!(
        repo.changelog(),
        format!(
            "{HEADER}## [Unreleased]\n\n## [0.1.1] - 2026-02-02\n\n{MARKER}\n\n### Fixed\n\n- Names are trimmed.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        )
    );

    repo.commit("feat: sleep on demand");
    let text = repo.changelog().replace(
        "## [Unreleased]\n",
        "## [Unreleased]\n\n### Added\n\n- Agents sleep on demand.\n\n### Fixed\n\n- Names are trimmed.\n",
    );
    repo.write("CHANGELOG.md", &text);
    let run = repo.xtask(&[
        "changelog",
        "stamp",
        "--date",
        "2026-02-03",
        "--base",
        "origin/main",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("0.1.1 becomes 0.2.0"), "{}", run.out);
    assert_eq!(
        repo.changelog(),
        format!(
            "{HEADER}## [Unreleased]\n\n## [0.2.0] - 2026-02-03\n\n{MARKER}\n\n### Added\n\n- Agents sleep on demand.\n\n### Fixed\n\n- Names are trimmed.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        )
    );
    assert_eq!(repo.changelog().matches(MARKER).count(), 2);
}

#[test]
fn stamp_writes_the_entry_from_the_commits_when_nothing_was_written() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/generated");
    repo.commit("feat(watch): wake on demand");
    repo.commit("fix: trim names");
    repo.commit("chore: silent");
    let run = repo.xtask(&[
        "changelog",
        "stamp",
        "--date",
        "2026-03-01",
        "--base",
        "origin/main",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        run.out.contains("comes from the commit subjects"),
        "{}",
        run.out
    );
    let expected = format!(
        "## [0.2.0] - 2026-03-01\n\n{MARKER}\n\n### Added\n\n- **Watch:** Wake on demand.\n\n### Fixed\n\n- Trim names.\n"
    );
    assert!(repo.changelog().contains(&expected), "{}", repo.changelog());
}

#[test]
fn stamp_creates_the_file_and_does_nothing_without_a_publishable_commit() {
    let repo = Repo::new().with_first_release();
    repo.branch("chore/quiet");
    repo.commit("chore: quiet");
    let run = repo.xtask(&["changelog", "stamp", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("nothing to stamp"), "{}", run.out);

    fs::remove_file(repo.dir.join("CHANGELOG.md")).unwrap();
    repo.commit("fix: real");
    let run = repo.xtask(&[
        "changelog",
        "stamp",
        "--date",
        "2026-03-02",
        "--base",
        "origin/main",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    let text = repo.changelog();
    assert!(text.starts_with("# Changelog\n"), "{text}");
    assert!(text.contains("cargo xtask changelog stamp"), "{text}");
    assert!(text.contains("## [Unreleased]\n\n## [0.1.1] - 2026-03-02\n\n<!-- stamped -->\n\n### Fixed\n\n- Real.\n"), "{text}");
}

#[test]
fn stamp_rejects_a_malformed_date() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/date");
    repo.commit("feat: a");
    let run = repo.xtask(&["changelog", "stamp", "--date", "yesterday"]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("YYYY-MM-DD"), "{}", run.err);
}

#[test]
fn bootstrap_stamp_folds_into_the_inherited_0_1_0_section() {
    let repo = Repo::new();
    repo.commit("feat: the first feature");
    repo.commit_changelog(
        &released_section("0.1.0", "- First."),
        "docs(changelog): stamp 0.1.0",
    );
    repo.push_main();
    repo.branch("feature/second");
    repo.commit("feat: the second feature");
    repo.write_changelog(&format!(
        "## [Unreleased]\n\n### Added\n\n- Second.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
    ));
    let run = repo.xtask(&[
        "changelog",
        "stamp",
        "--date",
        "2026-01-05",
        "--base",
        "origin/main",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(
        repo.changelog(),
        format!(
            "{HEADER}## [Unreleased]\n\n## [0.1.0] - 2026-01-05\n\n{MARKER}\n\n### Added\n\n- First.\n- Second.\n"
        )
    );
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("publishes 0.1.0"), "{}", run.out);
}

// Check.

#[test]
fn check_fails_while_a_publishable_branch_is_unstamped() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/unstamped");
    repo.commit("feat: new");
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("This branch publishes 0.2.0, but [Unreleased] is still the top section of CHANGELOG.md."),
        "{}",
        run.err
    );
    assert!(
        run.err
            .contains("Run `cargo xtask changelog stamp` and commit CHANGELOG.md."),
        "{}",
        run.err
    );
}

#[test]
fn check_passes_after_stamp_and_fails_on_a_stale_number() {
    let repo = Repo::new().with_first_release();
    repo.branch("bugfix/stamped");
    repo.commit("fix: a");
    let run = repo.xtask(&["changelog", "stamp", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        run.out.contains("OK, this branch publishes 0.1.1"),
        "{}",
        run.out
    );

    repo.commit("feat: raises the bump");
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains(
            "This branch publishes 0.2.0, but the top section of CHANGELOG.md says 0.1.1."
        ),
        "{}",
        run.err
    );
}

#[test]
fn check_passes_a_quiet_branch_that_touched_nothing() {
    let repo = Repo::new().with_first_release();
    repo.branch("chore/quiet");
    repo.commit("chore: quiet");
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("nothing to require"), "{}", run.out);
}

#[test]
fn check_passes_a_quiet_branch_when_main_carries_the_stamp_before_the_first_tag() {
    // Before the first release `main` holds a stamped 0.1.0 that has no tag yet.
    // A branch that publishes nothing and leaves it alone has nothing to answer for.
    let repo = Repo::new();
    repo.commit("feat: first");
    repo.commit_changelog(
        &format!(
            "## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        ),
        "docs(changelog): stamp 0.1.0",
    );
    repo.push_main();
    repo.branch("docs/readme");
    repo.commit("docs: explain");
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("nothing to require"), "{}", run.out);
}

#[test]
fn check_refuses_a_change_to_a_tagged_section() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/typo");
    repo.commit("feat: new");
    repo.xtask(&["changelog", "stamp", "--base", "origin/main"]);
    let text = repo.changelog().replace("- First.", "- First, reworded.");
    repo.write("CHANGELOG.md", &text);
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err
            .contains("Section 0.1.0 carries a tag: a published entry is frozen"),
        "{}",
        run.err
    );
}

#[test]
fn check_refuses_a_superfluous_stamp() {
    let repo = Repo::new().with_first_release();
    repo.branch("chore/overstamped");
    repo.commit("chore: nothing to publish");
    repo.write_changelog(&format!(
        "## [Unreleased]\n\n## [0.1.1] - 2026-01-02\n\n{MARKER}\n\n### Fixed\n\n- Nothing really.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
    ));
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("This branch has no publishable commit, but CHANGELOG.md carries a stamped 0.1.1 section."),
        "{}",
        run.err
    );
}

#[test]
fn check_refuses_a_change_to_an_inherited_untagged_section() {
    let repo = Repo::new().with_first_release();
    repo.commit("feat: merged before");
    repo.commit_changelog(
        &format!(
            "## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Pending.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        ),
        "docs(changelog): stamp 0.2.0",
    );
    repo.push_main();
    repo.branch("feature/conflict");
    repo.commit("feat: mine");
    repo.xtask(&["changelog", "stamp", "--base", "origin/main"]);
    assert!(
        repo.changelog().contains("## [0.3.0]"),
        "{}",
        repo.changelog()
    );
    let text = repo
        .changelog()
        .replace("- Pending.", "- Pending, edited by the wrong branch.");
    repo.write("CHANGELOG.md", &text);
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("Section 0.2.0 was changed by this branch"),
        "{}",
        run.err
    );
    assert!(run.err.contains("badly resolved conflict"), "{}", run.err);
}

#[test]
fn check_names_an_inherited_stamp_when_the_branch_has_none() {
    let repo = Repo::new().with_first_release();
    repo.commit("feat: merged before");
    repo.commit_changelog(
        &format!(
            "## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Pending.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        ),
        "docs(changelog): stamp 0.2.0",
    );
    repo.push_main();
    repo.branch("feature/forgot");
    repo.commit("fix: mine");
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains(
            "the top section of CHANGELOG.md is 0.2.0, stamped by another branch. Yours is missing."
        ),
        "{}",
        run.err
    );
}

#[test]
fn check_lets_a_quiet_branch_repair_an_abandoned_stamp() {
    let repo = Repo::new().with_first_release();
    repo.commit_changelog(
        &format!(
            "## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Never published.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        ),
        "docs(changelog): stamp without a commit behind it",
    );
    repo.push_main();
    repo.branch("fix/release-cleanup");
    repo.commit("chore: repair the file");
    // An inherited stamp left untouched is not this branch's business.
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("nothing to require"), "{}", run.out);

    repo.write_changelog(&released_section("0.1.0", "- First."));
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        run.out.contains("0.2.0 removed from CHANGELOG.md"),
        "{}",
        run.out
    );
}

#[test]
fn check_requires_the_repair_to_match_what_will_publish() {
    let repo = Repo::new().with_first_release();
    repo.commit("feat: merged, release failed");
    repo.commit_changelog(
        &format!(
            "## [Unreleased]\n\n## [0.2.1] - 2026-01-03\n\n{MARKER}\n\n### Fixed\n\n- Wrong number.\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Pending.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        ),
        "docs(changelog): two pending sections",
    );
    repo.push_main();
    repo.branch("fix/release-renumber");
    repo.commit("chore: repair");
    repo.write_changelog(&format!(
        "## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Pending.\n\n### Fixed\n\n- Wrong number.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
    ));
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    assert!(
        run.out
            .contains("was repaired: 0.2.0 is the only untagged section left"),
        "{}",
        run.out
    );

    repo.write_changelog(&format!(
        "## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Invented here.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
    ));
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("In section 0.2.0: - Invented here."),
        "{}",
        run.err
    );

    repo.write_changelog(&format!(
        "## [Unreleased]\n\n## [0.5.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Pending.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
    ));
    let run = repo.xtask(&["changelog", "check", "--base", "origin/main"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err
            .contains("numbered 0.2.0. Untagged right now: 0.5.0."),
        "{}",
        run.err
    );
}

#[test]
fn check_needs_a_merge_base() {
    let repo = Repo::new().with_first_release();
    let run = repo.xtask(&["changelog", "check", "--base", "origin/nowhere"]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("no merge base"), "{}", run.err);
}

// Coverage.

#[test]
fn coverage_walks_every_reachable_tag() {
    let repo = Repo::new().with_first_release();
    repo.commit("feat: second");
    repo.commit_changelog(
        &format!(
            "## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n\n_No notable changes._\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        ),
        "chore(release): 0.2.0",
    );
    repo.tag("v0.2.0");
    let run = repo.xtask(&["changelog", "coverage"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("OK, 2 tag(s)"), "{}", run.out);

    let text = repo.changelog().replacen(MARKER, "", 1);
    repo.write("CHANGELOG.md", &text);
    let run = repo.xtask(&["changelog", "coverage"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("## [0.2.0]") && run.err.contains("does not carry"),
        "{}",
        run.err
    );

    repo.write_changelog(&format!(
        "## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n"
    ));
    let run = repo.xtask(&["changelog", "coverage"]);
    assert_eq!(run.code, 1);
    assert!(run.err.contains("no `## [0.1.0]` section"), "{}", run.err);
    assert!(
        run.err.contains("## [0.2.0]") && run.err.contains("has no entry"),
        "{}",
        run.err
    );
}

#[test]
fn coverage_has_nothing_to_walk_before_the_first_tag() {
    let repo = Repo::new();
    let run = repo.xtask(&["changelog", "coverage"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("nothing to cover yet"), "{}", run.out);
}

// Unreleased.

#[test]
fn unreleased_prints_the_suggested_entry() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/entry");
    repo.commit("feat(sidebar-token): show the sleeping count");
    repo.commit("perf: faster wake");
    let run = repo.xtask(&["changelog", "unreleased", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(
        run.out,
        "### Added\n\n- **Sidebar token:** Show the sleeping count.\n\n### Changed\n\n- Faster wake.\n"
    );
    let run = repo.xtask(&["changelog", "unreleased", "--since", "HEAD"]);
    assert_eq!(run.out.trim(), "_No notable changes._");
}

// Canary.

#[test]
fn canary_version_is_the_next_version_with_a_prerelease_suffix() {
    let repo = Repo::new().with_first_release();
    repo.branch("feature/canary");
    repo.commit("chore: quiet");
    let run = repo.xtask(&["canary-version", "ab12cd3", "--base", "origin/main"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.out.trim(), "0.1.1-pre.ab12cd3");

    repo.commit("feat: loud");
    let run = repo.xtask(&["canary-version", "ab12cd3", "--base", "origin/main"]);
    assert_eq!(run.out.trim(), "0.2.0-pre.ab12cd3");

    let run = repo.xtask(&["canary-version", "ZZZ"]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("not a short commit sha"), "{}", run.err);
    assert_eq!(repo.xtask(&["canary-version"]).code, 2);
}

#[test]
fn canary_version_in_bootstrap_takes_the_highest_known_version() {
    let repo = Repo::new();
    repo.branch("feature/canary");
    repo.commit("chore: quiet");
    assert_eq!(
        repo.xtask(&["canary-version", "0123456"]).out.trim(),
        "0.0.1-pre.0123456"
    );
    repo.commit("fix: loud");
    assert_eq!(
        repo.xtask(&["canary-version", "0123456"]).out.trim(),
        "0.1.0-pre.0123456"
    );
}

// Release backlog.

fn repo_with_unreleased_stamp_on_main() -> Repo {
    let repo = Repo::new().with_first_release();
    repo.commit("feat: merged");
    repo.commit_changelog(
        &format!(
            "## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n{MARKER}\n\n### Added\n\n- Pending.\n\n## [0.1.0] - 2026-01-01\n\n{MARKER}\n\n### Added\n\n- First.\n"
        ),
        "docs(changelog): stamp 0.2.0",
    );
    repo.push_main();
    repo.branch("feature/blocked");
    repo.commit("chore: anything");
    repo
}

#[test]
fn release_backlog_blocks_while_main_stamps_an_untagged_version() {
    let repo = repo_with_unreleased_stamp_on_main();
    let run = repo.xtask(&["release-backlog"]);
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("has no tag") && run.err.contains("0.2.0"),
        "{}",
        run.err
    );
    assert!(run.err.contains("retry"), "{}", run.err);
    assert!(run.err.contains("fix/release-"), "{}", run.err);

    repo.tag("v0.2.0");
    let run = repo.xtask(&["release-backlog"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        run.out
            .contains("Every stamped version of origin/main has its tag."),
        "{}",
        run.out
    );
}

#[test]
fn release_backlog_exempts_repair_branches() {
    let repo = repo_with_unreleased_stamp_on_main();
    let run = repo.xtask_with_env(
        &["release-backlog"],
        &[("CI_COMMIT_BRANCH", "fix/release-token")],
    );
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("skipped"), "{}", run.out);

    repo.branch("fix/release-local");
    let run = repo.xtask(&["release-backlog"]);
    assert_eq!(run.code, 0, "{}", run.err);
}

#[test]
fn release_backlog_passes_before_the_first_tag() {
    let repo = Repo::new();
    repo.commit("feat: first");
    repo.commit_changelog(
        &released_section("0.1.0", "- First."),
        "docs(changelog): stamp 0.1.0",
    );
    repo.push_main();
    repo.branch("feature/next");
    let run = repo.xtask(&["release-backlog"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.out.contains("No tag yet"), "{}", run.out);
}

// Help and usage.

#[test]
fn help_and_usage_errors() {
    let repo = Repo::new();
    let run = repo.xtask(&["--help"]);
    assert_eq!(run.code, 0);
    assert!(
        run.out.contains("changelog")
            && run.out.contains("canary-version")
            && run.out.contains("release-backlog")
    );
    let run = repo.xtask(&["changelog", "--help"]);
    assert_eq!(run.code, 0);
    for word in [
        "version",
        "stamp",
        "check",
        "coverage",
        "unreleased",
        "--since",
        "--base",
    ] {
        assert!(run.out.contains(word), "{word} missing from {}", run.out);
    }
    assert_eq!(repo.xtask(&["nope"]).code, 2);
    assert_eq!(repo.xtask(&["changelog"]).code, 2);
    assert_eq!(repo.xtask(&["changelog", "nope"]).code, 2);
    let run = repo.xtask(&["changelog", "version", "--bogus"]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("unknown option '--bogus'"), "{}", run.err);
}

# CLAUDE.md

Instructions for Claude, and any other agent, working in this repository.

Herdr Sleep is a Herdr plugin, written in Rust, that puts idle coding agents to sleep to free
memory and wakes them on demand. Read `GLOSSARY.md` for the vocabulary and use it in code,
comments, commits and docs. Decisions that shape the design live in `docs/adr/`; read them before
changing what they decide, and add one when a new decision meets the bar (hard to reverse,
surprising without context, a real trade-off).

## Language and style

- Everything is in English: code, comments, log and screen messages, docs, commits, merge
  requests, issues. The conversation with the maintainer may be in French; the product is not.
- No em dash anywhere: code, commits, CHANGELOG, docs, merge request descriptions.
- The project is presented on its own merits. Docs and comments never refer to an earlier
  implementation, prototype or "previous version" of the plugin.
- Code goes through `cargo fmt`; `cargo clippy --all-targets -- -D warnings` must be clean.

## Layout

- `crates/herdr-sleep`: the CLI and plugin entry points (`watch`, `event`, `sleep`, `wake`,
  `status`, ...). Talks to Herdr only through `herdr` JSON output (`HERDR_BIN_PATH`).
- `crates/herdr-sleep-ph`: the Placeholder. Depends on `libc` only; keep it that way, its RSS is
  the whole point of the project.
- `crates/xtask`: repository tooling (`cargo xtask changelog ...`, `cargo xtask release ...`).
  Never a runtime dependency of the plugin.
- `herdr-plugin.toml`: the manifest Herdr reads. `scripts/build.sh` is its `[[build]]` command.
- `tests/e2e`: Docker end-to-end suite against real Herdr binaries (lowest supported version and
  latest). `make e2e` runs it.

## Versions are computed, never chosen

- `Cargo.toml` and `herdr-plugin.toml` carry the version of the last release on `main`. Never
  edit a version by hand: the `release` CI job computes the next one, writes it in both files and
  `Cargo.lock`, commits `chore(release): X.Y.Z` on `main` and tags it. That commit is the only
  one ever pushed to `main` outside a merge request.
- The version comes from `cargo xtask changelog version`, the single implementation shared by the
  branch stamp, the branch check and the release job. On a branch, the base is the highest of the
  last tag reachable from HEAD and the versions `CHANGELOG.md` already carries outside your own
  section; the commits that decide the bump are the ones your branch adds on top of `main`.
- Only three things publish: `feat:` publishes a minor, `fix:` publishes a patch, and a breaking
  marker (`BREAKING CHANGE:` or `BREAKING-CHANGE:` in a body, or `type!:`) publishes a major. The
  type is read from the subject line only. Every other type publishes nothing: `perf`, `build`,
  `deps`, `revert`, `security`, `refactor`, `docs`, `chore`, `ci`, `test`, `style` included.
- Only non-merge commits count. A merge commit repeats the merge request title and titles are
  not read. Never squash: the project merges with a merge commit on a rebased branch.

## Every releasable branch stamps CHANGELOG.md

1. Describe the change under `## [Unreleased]` in `CHANGELOG.md`, in Keep a Changelog form
   (`### Added`, `### Changed`, `### Removed`, `### Fixed`, `### Security`), written for a user
   of the plugin: what changes for them, not which file moved.
2. When the branch is ready, run `cargo xtask changelog stamp`. It works out the version this
   merge will publish and turns `[Unreleased]` into `[X.Y.Z] - YYYY-MM-DD`, reopening an empty
   one above. Commit `CHANGELOG.md`.
3. After any rebase on `main`, or any new commit, run `cargo xtask changelog stamp` again: it
   corrects the number and the date and folds new `[Unreleased]` entries into the stamped section.
4. Never edit a section that already carries a tag. A published entry is frozen, typo included.

`changelog:check` runs in every branch pipeline. It fails when the branch has a publishable
commit and the top section of `CHANGELOG.md` does not carry exactly the version the merge will
publish, and when a section is stamped with no publishable commit behind it. Run the command it
names; do not work around the check. `release:backlog` fails every branch while `main` carries a
stamped version without its tag (a release that did not complete); `fix/release-*` branches are
exempt because they repair it.

## Merging

- Semi-linear history: a branch is rebased on `main` before it merges, which keeps the stamped
  version correct. After a rebase, stamp again.
- Merges go through the merge request and its pipeline. Never push to `main` yourself.
- The `release` resource group processes pipelines oldest first; the job refuses to publish rather
  than publish out of order.

## Commits and branches

- Conventional Commits with a scope when one applies: `fix(placeholder): ...`,
  `feat(watch): ...`. Atomic commits, one concern each.
- Branch names: `feature/...`, `bugfix/...`, `hotfix/...`, `fix/...`, `breaking/...` get a
  manual `canary` job that builds a prerelease when someone plays it. Any other prefix works too.
- No history rewrite on a shared branch: follow-up commits.
- No attribution lines, trailers or mentions of AI assistance in commits or merge requests.

## Tests

- `cargo test` must pass (unit and integration tests of every crate).
- `make e2e` runs the Docker suite against every Herdr version of the matrix (the lowest
  `min_herdr_version` and the latest release). A change to sleep, wake, the Placeholder, the
  watcher or the manifest is not done until the suite is green on both.
- Never run the e2e suite or any test against the maintainer's live Herdr session.

## Docs

- `README.md` is for users: install, configuration, sidebar token, keybinding, commands, and an
  honest "verified" section (which agents and platforms the suite actually covers).
- Keep `GLOSSARY.md` free of implementation details; update it the moment a term changes.
- A merge request that changes behaviour updates `CHANGELOG.md`, the README when users see the
  change, and the glossary or an ADR when the model or a decision changes.

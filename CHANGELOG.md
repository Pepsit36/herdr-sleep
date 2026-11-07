# Changelog

All notable changes to Herdr Sleep are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Describe your change under `[Unreleased]`, for a user of the plugin. When your branch is ready,
run `cargo xtask changelog stamp`: it works out the version the merge will publish and turns
`[Unreleased]` into `[X.Y.Z] - <date>`, reopening an empty one above. Run it again after a rebase
or a new commit: it corrects the number and the date and folds the new entries in. A section
written by `stamp` carries `<!-- stamped -->`. Never edit a section that already carries a tag:
a published entry is frozen, typo included.

## [Unreleased]

### Added

- The `herdr-sleep sleep <pane|name>` command puts an idle agent to sleep: it quits the agent with its own exit command (never a signal) and keeps the pane, its name and its session.

## [0.1.0] - 2026-11-07

<!-- stamped -->

### Added

- **Tooling:** `cargo xtask changelog` computes the version a branch publishes from its commits,
  stamps `CHANGELOG.md` with it and checks the file in every branch pipeline; `cargo xtask
  release-backlog` blocks branches while a stamped version waits for its tag, and `cargo xtask
  canary-version` names prerelease builds.
- **Configuration:** the plugin reads `config.toml` from its Herdr config directory (idle delay, eligible statuses, sleep icon and sidebar token, notifications, name, directory and environment filters, ignored child processes, watcher interval). Every key is optional, unknown keys only produce a warning, and a bad value is reported with the key and the file. Claude, Codex, Cursor and Gemini are built in as agent profiles, and any other agent can be added or a built-in tweaked in an `[agents.<kind>]` table.

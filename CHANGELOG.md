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

## [0.1.0] - 2026-11-08

<!-- stamped -->

### Added

- **Tooling:** `cargo xtask changelog` computes the version a branch publishes from its commits,
  stamps `CHANGELOG.md` with it and checks the file in every branch pipeline; `cargo xtask
  release-backlog` blocks branches while a stamped version waits for its tag, and `cargo xtask
  canary-version` names prerelease builds.
- **Configuration:** the plugin reads `config.toml` from its Herdr config directory (idle delay, eligible statuses, sleep icon and sidebar token, notifications, name, directory and environment filters, ignored child processes, watcher interval). Every key is optional, unknown keys only produce a warning, and a bad value is reported with the key and the file. Claude, Codex, Cursor and Gemini are built in as agent profiles, and any other agent can be added or a built-in tweaked in an `[agents.<kind>]` table.
- The `herdr-sleep sleep <pane|name>` command puts an idle agent to sleep: it quits the agent with its own exit command (never a signal) and keeps the pane, its name and its session.
- **Wake:** `herdr-sleep wake <pane|name>` wakes a sleeping agent: any focus, key or prompt wakes a
  sleeping pane, and what was typed meanwhile is replayed into the agent once it is ready.
- **Focus wake:** focusing a sleeping pane, or its tab or space, wakes that pane only.
- **Automatic sleep:** idle agents are put to sleep after `sleep_after_minutes`, by a background
  watcher that starts with Herdr and restarts itself when it is missing; `herdr-sleep sweep` runs
  one pass on demand.
- **Installation:** install Herdr Sleep as a Herdr plugin with `herdr plugin install Pepsit36/herdr-sleep`. The install downloads the prebuilt binaries of the release (checksum verified) and falls back to building from source with cargo when none matches your platform or you are offline. The plugin adds the actions `sleep`, `wake`, `sweep`, `status` and `ensure-watcher`, starts its watcher with Herdr and wakes the focused pane on focus.
- **Sidebar markers:** a sleeping pane or a fully sleeping space shows a `$sleep` sidebar token (name and icon configurable), and a fully sleeping tab gets the icon as a label prefix (`tab_prefix`, on by default).

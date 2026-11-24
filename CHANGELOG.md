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

## [0.6.1] - 2026-11-24

<!-- stamped -->

### Fixed

- **Restart:** a pane whose Herdr identifier has capital letters, which is most of them, is resumed
  again after a restart: the agent used to get a name Herdr refuses (`invalid_agent_name`), stayed
  awake and was given up. Names are now lowercase and at most 32 characters, and a kept name that
  Herdr would refuse is replaced. An agent that started although its start was reported as failed
  now goes to sleep like the others.
- **Restart:** a pane that is not ready no longer makes the others wait. Ready panes sleep within
  seconds, and a slow one is looked at again, round after round, for twice
  `restore_settle_timeout_seconds`. A registered pane that Herdr still holds a saved session for
  is resumed then put to sleep when its time is up, instead of being given up.
- **Restart:** every run of `restore` starts with a `RESTORE-START` line in the journal, and the
  panes it leaves aside before the rounds (`pane-gone`, no saved session) are logged as
  `RESTORE-SKIP`, so a restart that did nothing is no longer silent.

## [0.6.0] - 2026-11-23

<!-- stamped -->

### Added

- **Keep a tab awake:** the new `keep-awake` action (bind it to a key) puts `keep_awake_icon` (🔒)
  in front of the label of the tab of the focused pane, and none of its agents goes to sleep while
  the icon is there, by sweep or by `sleep` (`tab-kept-awake`; `sleep --force` still sleeps them).
  Run it again to let the tab sleep. `herdr-sleep keep-awake [<pane|name>] [--on|--off]` does the
  same from a terminal.

## [0.5.0] - 2026-11-23

<!-- stamped -->

### Added

- **Remote Control:** a Claude Code agent driven through Remote Control (started with
  `--remote-control` or `--rc`, or showing the `/rc active` badge) no longer goes to sleep, by sweep
  or by `sleep`. The refusal is `remote-control`; `sleep --force` still sleeps it.

## [0.4.0] - 2026-11-23

<!-- stamped -->

### Added

- **Drafts:** `sleep_with_draft = true` (global or per agent, off by default) puts an agent to sleep
  even when a one-row draft is typed in its prompt. The draft is saved, the prompt cleared, and the
  draft typed back, without Enter, when the agent wakes. A draft over several rows or a pasted text
  still keeps the agent awake.

### Fixed

- **Wake:** the replay after a wake now uses the state directory of the placeholder that started
  it, so the woken pane leaves the registry of sleeping panes and its journal lines land in the
  right place even when the pane's environment names another state directory.
- **Sleep:** a sleep that fails no longer leaves `/exit` typed in the prompt, which made the next
  attempt type `/exit/exit`. The text is erased, and a saved draft is typed back.

## [0.3.4] - 2026-11-19

<!-- stamped -->

### Fixed

- **Sleep action:** when the `sleep` action (keybinding or action picker) cannot put the pane to
  sleep, Herdr now shows a notification with the reason and what to try (for example a draft in the
  prompt, or `--force`). Before, the explanation went to a stderr nobody sees. It is shown whatever
  `notify` says.

## [0.3.3] - 2026-11-19

<!-- stamped -->

### Fixed

- **Sleeping pane:** the placeholder no longer shows the agent's empty input box and status line
  under its message. The preview stops above the input box (found with `prompt_frame_regex`) and
  keeps the last `preview_lines` lines before it. Without a frame on screen it is unchanged.

## [0.3.2] - 2026-11-18

<!-- stamped -->

### Fixed

- **Prompt detection:** an agent with a tall status bar under its prompt (about 16 rows with Claude
  Code) no longer makes `sleep` and the sweep refuse with `prompt-not-detected`. The prompt is now
  looked for between the last two horizontal rules of the screen, so the messages above it and the
  status bar below it do not matter; the last rows of the screen are searched when no such frame is
  drawn. `restore` and `wake` use the same detection. New optional settings `prompt_area_lines`
  (5 to 40, default 15) and `prompt_frame_regex` (empty turns the frame detection off) apply to
  every agent and can be set per agent. The refusal now names both settings.

## [0.3.1] - 2026-11-18

<!-- stamped -->

### Fixed

- **Wake in fish:** waking a sleeping agent in a pane whose shell is fish no longer closes the pane.
  fish rejected the wake script (`$?` is `$status` in fish) and exited before resuming anything; the
  script is now written in the syntax of the pane's shell, and a shell the plugin does not know gets
  a script with nothing shell-specific in it, at the price of no report when the resume fails.

## [0.3.0] - 2026-11-18

<!-- stamped -->

### Added

- **Exclusion patterns:** three new optional settings keep an agent awake. `exclude_name_regex`
  matches the agent's name, `exclude_screen_regex` matches the last lines of its screen (for
  example "waiting for approval") and `exclude_cmdline_regex` matches its command line or that of
  any of its child processes. All are case-insensitive and empty by default. A match blocks the
  sweep and `sleep`, which explains itself (`excluded-name-regex`, `excluded-screen-regex`,
  `excluded-cmdline-regex`) and suggests `--force`; `herdr-sleep status` shows the reason. After a
  Herdr restart the Placeholders of sleeping panes still come back; an agent resumed from a saved
  session stays awake when a pattern matches.

## [0.2.0] - 2026-11-17

<!-- stamped -->

### Added

- **Restart:** after a Herdr restart, agents that were awake (not only the ones that slept) are put to
  sleep again. Herdr keeps their saved session; the plugin starts each agent again with its
  session, one at a time, and puts it to sleep once it is ready, so no pane is left as a plain shell.
- **Restart:** `restore_settle_timeout_seconds` (60 s by default) sets how long `restore` waits for a
  pane's shell and for Herdr to release the agent's identity. Panes that were not ready get a second
  pass, and the end of `restore` reports how many were restored, left alone, or ignored because their
  working directory is gone (listed apart, also in `herdr-sleep status`).
- `herdr-sleep sleep <pane>` handles a pane with no live agent but a saved session (resume, then
  sleep), and `--from-session` forces that path.
- `herdr-sleep sleep --wait[=SECONDS]` keeps trying while the agent is not idle yet.
- `herdr-sleep sweep --now` puts every settled, unfocused agent to sleep without waiting for the idle
  time, and without the limit of three agents per pass. The other guards still apply.

### Changed

- A refused or failed `herdr-sleep sleep` now prints the reason on stderr, with what to try next
  (`--wait`, `--force`), instead of a line on stdout.

### Fixed

- Agents that were awake at a Herdr restart stayed plain shells with no sleep marker, and `sweep`,
  `sleep` and `restore` could not reach them.
- A pane that Herdr had not released within 20 seconds was skipped and never retried.

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
- **Commands:** `herdr-sleep status` lists every managed agent with its idle time, memory and last refusal, and the watcher state; `herdr-sleep config` prints the effective configuration; `status`, `sweep`, `sleep`, `wake`, `config` and `ensure-watcher` accept `--json`, and `sleep` and `wake` act on the pane of the Herdr action when no target is given.
- **Restart:** sleeping panes are restored after a Herdr restart: the plugin puts the Placeholder back in every pane that was asleep, without starting any agent. Set `[session] resume_agents_on_restore = false` in Herdr's config so that Herdr does not relaunch every agent at once.

### Fixed

- **Sleep:** a Claude Code agent whose conversation has not been saved yet (for instance before
  its first message) is no longer put to sleep, because it could not be resumed. `status` shows
  `no-conversation` and the agent is retried later.
- **Wake:** when the resume command fails right after waking, the pane now says what happened and
  which command was tried before leaving you in your shell, instead of dropping to a bare prompt.

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

- **Wake:** an agent comes back with the flags it was started with. `--model` is kept for every
  built-in kind, and `--permission-mode` and `--dangerously-skip-permissions` for claude. The list
  is the `keep_args` key of an agent kind; anything not on it, such as a token or a prompt on the
  command line, is neither stored nor typed again.

## [0.7.8] - 2026-11-27

<!-- stamped -->

### Fixed

- **Sleeping pane:** stopping a Placeholder (`kill`, closing the terminal) now gives the terminal
  its settings back and releases the sleeping agent, instead of leaving the pane in raw mode with
  a ghost agent.
- **Sleeping pane:** a terminal whose settings cannot be changed still wakes on a key, instead of
  waiting forever.
- **Sleeping pane:** the name check after a Sleep no longer depends on how Herdr spaces its JSON.

## [0.7.7] - 2026-11-26

<!-- stamped -->

### Fixed

- **Restart:** an agent keeps its name across a restart even when Herdr stopped reporting the name
  for a moment just before: the record of live agents no longer drops a name it knew.

## [0.7.6] - 2026-11-26

<!-- stamped -->

### Fixed

- The journal no longer loses lines when two processes write at the moment it is trimmed.
- The built-in list of ignored children matches `mcp` as a word, so a build of a project whose
  name merely contains it is no longer taken for an idle MCP server.

## [0.7.5] - 2026-11-26

<!-- stamped -->

### Changed

- **Action:** "Sleep idle agents now" runs `herdr-sleep sweep --now`, as its title says: every
  settled, unfocused agent sleeps now, without waiting for the idle time and without the limit of
  three per pass. The guards still apply.
- **Configuration:** an empty pattern, a `sleep_icon` equal to `keep_awake_icon`, a `sleep_icon`
  with a control character or a backslash, and an agent `bin` with arguments are refused with a
  message naming the key; they used to be accepted and to match everything, hide the sleep state
  or break every sleep. `sleep_statuses = []` is accepted with a warning.

### Fixed

- **Sleep:** a draft whose first line is empty is seen as a draft and keeps the agent awake. What
  you type while the agent quits is cleared before the Placeholder command is typed, and a word
  that starts with `=` is quoted for zsh.
- **Herdr calls:** a command that exited cleanly is a success even when it prints a warning or a
  plain `ok`, and one that leaves a background process holding its pipes no longer blocks the
  plugin for as long as that process lives.
- **State:** the state files are written through a temporary file of their own and the registry is
  updated under a lock, so two plugin processes no longer overwrite each other's update. Draft,
  preview and wake files of panes that were closed are removed after an hour.
- **Commands:** `sleep <pane> --wait 45` says to write `--wait=45` instead of looking for a pane
  named 45, `wake` writes its failures on stderr like `sleep`, and `--help` and `--version` do not
  panic when stdout is closed.

## [0.7.4] - 2026-11-26

<!-- stamped -->

### Fixed

- **Install:** the plugin installs the prebuilt binaries only when the checkout is the code of the
  release the manifest names, and builds from source otherwise. A checkout some commits ahead of its
  last release used to get the binaries of that release under a newer manifest. Downloads give up
  after a timeout and are retried, and a binary that does not run on the machine falls back to a
  source build instead of being installed.
- **Update:** a Watcher that was running when the plugin was updated leaves by itself within a few
  seconds, and the next hook starts the new one. The path typed into every Placeholder no longer
  ends with ` (deleted)` after an update, which made the replay at wake fail.
- **Release:** a resumed release replaces every asset it attaches, and a checksum always follows
  its archive, so nobody downloads an archive that does not match its `.sha256`.
- **Release:** a clone without tags of a repository that already published no longer restarts at
  `0.1.0`, and a `fix/release-*` branch with a publishable commit is refused by `changelog:check`
  with the reason instead of failing the release later.
- **CI:** the test job installs `curl` and `make`, so the publish and dist tests run instead of
  returning early. `make help` lists `e2e`, and the end-to-end suite fails on an unknown scenario.

## [0.7.3] - 2026-11-26

<!-- stamped -->

### Fixed

- **Sleep:** an agent started in a nested shell (`nix develop`, `sudo -i`, `bash`) or with `exec`
  stays awake and the refusal is `not-shell-job`. It used to be quit, the sleep was abandoned after
  twenty seconds and the keys typed to undo it landed in the nested shell, leaving a bare pane
  with no Placeholder.
- **Sleep:** the pane is written in `sleepers.json` just before the exit command is typed, and the
  entry is withdrawn when the agent is still running after an abandoned sleep. An agent that
  quits late, after the abandon, no longer leaves a bare shell that nothing restores. The session
  is now in the `SLEEP` journal line.
- **Sweep:** a sleep that is abandoned is not tried again at every pass: the agent is left alone
  for five minutes, then ten, up to an hour, until its state changes (`sweep --now` still tries).
  A dry run and a refused agent no longer use up the three places of a pass, so the agents behind
  them are no longer left out forever.
- **Wake:** a registry entry whose pane runs an agent again, or whose resume failed a few minutes
  after a wake, is dropped by the Sweep. A restart used to put the Placeholder back for a session
  that was already running or dead.
- **Restart:** two `restore` runs at once no longer type the Placeholder command twice in a pane
  (the second one ends at once), and a second `restore` leaves the panes that already sleep alone
  instead of listing them as skipped. A pane Herdr holds only as a Saved session is resumed with
  that session, not an older one from the registry.

## [0.7.2] - 2026-11-26

<!-- stamped -->

### Fixed

- **Wake:** text you type in the placeholder is submitted to the agent only if you pressed Enter
  after it. It used to be submitted always, so a half typed line, or a key pressed to wake the
  pane, reached the agent as a prompt. Without Enter the text waits in the prompt for you to finish.
- **Wake:** waking by focus no longer presses Enter in the pane: that key landed in the text you
  were typing, or in a startup dialog of the agent. It now sends a terminal focus report, which the
  placeholder ignores as text.
- **Wake:** a replay that fails (a typing error, a broken configuration) is written in the journal
  and keeps the typed text and your draft on disk instead of deleting them first. A configuration
  that does not load no longer stops the replay.
- **Wake:** the placeholder no longer lets Alt with an arrow, mouse reports or device strings
  leave stray characters in the text, does not take a closed terminal for a wake, and gives up
  on a `herdr` call that does not answer after ten seconds instead of waiting for it forever.
- **Wake:** the three focus hooks of a tab, a workspace and a pane no longer wake the pane
  together, and a failed wake no longer blocks the next focus for fifteen seconds.

## [0.7.1] - 2026-11-26

<!-- stamped -->

### Fixed

- **Sleep:** the guards now judge the agent as Herdr reports it at that moment. A sweep puts its
  agents to sleep one after the other, seconds apart, and used to check the second one with the
  status and focus it had when the sweep began: an agent that had started working, or that you had
  just focused, could still be sent `/exit`.
- **Sleep:** a colored background on the prompt line (`ESC[48;2;...m`) no longer makes the text look
  faint, which hid a draft and let the exit command be typed after it. Hyperlink and title
  sequences are no longer read as text.
- **Sleep:** when the draft cannot be checked after it was cleared from the prompt (a Herdr call
  timing out), the draft stays on disk and the message says so, instead of being deleted. A draft
  that could not be typed back after a failed sleep is kept too.
- **Wake:** a resume that fails right away is explained in the pane again, whatever the age of the
  pane. The check used the age of the pane's shell, so it never fired for a pane open for more than
  ten seconds.
- **Wake:** an agent that is back at its prompt counts as ready whatever `sleep_statuses` holds.
  With `sleep_statuses = ["done"]` a wake used to wait two minutes and replay nothing.
- **Watcher:** with a configuration that does not load at startup, the watcher puts nothing to sleep
  (it used the defaults, ignoring `dry_run` and exclusions), and `restore` starts no agent. Both say
  so in the journal.
- **Sleep:** a manual sleep that meets a sleep already running on the same pane waits for it and
  reports the real outcome, instead of answering `superseded`.
- **Restart:** an agent you quit just before stopping the server is not brought back, even when no
  sweep ran in between: the pane's shell, older than the last time the agent was seen, tells a quit
  from a restart.
- **Install:** the minimum Rust version is 1.88, which the code needs, and a signal during
  `scripts/build.sh` ends the script instead of continuing with a source build. Long command lines
  are no longer cut by `ps`, which hid `--remote-control` at their end.

## [0.7.0] - 2026-11-26

<!-- stamped -->

### Added

- **Restart:** every awake agent is recorded as the Watcher sweeps (`live-agents.json`), so an
  agent that Herdr holds no saved session for, such as one that never went to sleep or a Cursor
  agent, comes back after `herdr server stop` and a new start, resumed with its session and put to
  sleep, and keeps its name. An agent you quit before the restart stays closed.
- **Restart:** with `dry_run = true`, `restore` only journals what it would do (`RESTORE-DRY-RUN`)
  and touches no pane.

### Changed

- **Restart:** a Claude Code session that never saved a conversation, which cannot be resumed, is
  started fresh in the same directory instead of failing (`RESTORE-FRESH`).
- **Sleep:** the `no-session` refusal now says that Herdr reports no session for the agent and
  names the hook to install (for Cursor, `herdr integration install cursor`).

## [0.6.4] - 2026-11-25

<!-- stamped -->

### Fixed

- **Sleep marker:** the tab of a pane you woke keeps its 💤 prefix no longer. The prefix used to
  stay until another agent went to sleep somewhere, so a tab whose agent had quit since still
  looked asleep, and a restart then left it as a plain shell without any trace. The markers are now
  brought in line as soon as a woken agent is back, and on every sweep, even when nothing sleeps.
- **Restart:** a pane that Herdr shows as a sleeping agent while the plugin has no record of it
  (so nothing to restore it from) is now listed in the journal as `RESTORE-SKIP <pane>
  sleeping-agent-not-registered` instead of being passed over in silence.

## [0.6.3] - 2026-11-25

<!-- stamped -->

### Fixed

- **Keep awake:** an agent of a tab kept awake could still go to sleep when Herdr did not answer
  the question "what is the label of this tab" at that moment: the guard took the silence for
  "not kept awake". It now refuses (`tab-unreadable`), journals it and tries again at the next
  sweep.

## [0.6.2] - 2026-11-25

<!-- stamped -->

### Fixed

- **Sleep:** a command that looked like `exec .../herdr-sleep-ph --pane ...` no longer shows up in
  the chat of an agent after a wake. Two sleeps reaching the same pane together (the Watcher's
  sweep, the "Sleep idle agents now" action, a manual sleep) typed the Placeholder command twice:
  the second copy landed in the Placeholder, which kept it as Captured input, and the Replay typed
  it into the agent at the next wake. A pane now goes to sleep once at a time, and a sleep that
  finds the pane already asleep, or its agent changed, leaves it alone without a word in the
  journal.

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

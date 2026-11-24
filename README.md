# Herdr Sleep

A [Herdr](https://herdr.dev) plugin that puts idle coding agents to sleep to free memory, and wakes
them on demand with their pane, name and session intact.

A coding agent that has been idle for fifteen minutes still holds hundreds of megabytes. Herdr Sleep
quits it cleanly, keeps the pane open with a tiny placeholder process (about 2 MB), and brings the
agent back with its own `--resume` the moment you focus the pane, press a key or send it a prompt.
What you typed while it was asleep is replayed into the agent once it is ready.

- No signal is ever sent: the agent is asked to quit with its own exit command (`/exit`).
- Nothing is lost: pane, position, name and session are kept; an unsent one-row draft can be saved and typed back (`sleep_with_draft`), other drafts block the sleep.
- One pane wakes at a time, never a whole tab or space, so waking does not spike memory.
- Claude Code, Codex, Cursor and Gemini are built in; any other agent can be added in the config.

## Requirements

- Herdr 0.9.2 or later.
- Linux or macOS.
- Nothing else for the prebuilt binaries. Without a matching release the installer builds from
  source and needs a Rust toolchain.

## Install

```sh
herdr plugin install Pepsit36/herdr-sleep
```

The install hook downloads the prebuilt binaries of the release that matches the plugin version,
verifies their checksum, and falls back to `cargo build --release` when none matches your platform
or you are offline. Then add the sleep marker to your sidebar (below) and restart Herdr, or run the
`ensure-watcher` action once.

Update with `herdr plugin update`.

## How it works

1. **Sweep.** A background watcher, started by Herdr and relaunched by the plugin if it ever stops,
   checks every minute. An agent that stays idle (or done), unfocused and unchanged for
   `sleep_after_minutes` is put to sleep.
2. **Sleep.** Safety guards run first: the agent must have a session, be at an empty prompt, have no
   busy child process and not be excluded by your filters. Then the plugin types the agent's exit
   command, waits for Herdr to forget the agent, and replaces the pane's shell with the
   **placeholder**, which shows the last lines of the agent's screen above its input box (the box
   and the status bar under it are left out) and announces a
   `sleeping-<kind>` agent to Herdr under the same name.
3. **Wake.** Focusing the pane (or its tab or space), pressing any key in it, sending it a prompt
   with `herdr agent prompt`, or running `herdr-sleep wake` starts the agent again with its
   `--resume`. The pane name comes back, and what you typed meanwhile is typed into the agent when
   it shows its prompt. The agent is started by the shell the pane had, so its aliases and `PATH`
   apply; bash, zsh and fish are handled, and with another shell nothing is reported when the
   resume fails (see Troubleshooting).
4. **After a Herdr restart,** the plugin puts every pane back to sleep at startup, instead of letting
   Herdr relaunch all the agents at once. Set `resume_agents_on_restore = false` in the `[session]`
   section of Herdr's config so that Herdr does not also relaunch them. Panes that were asleep get
   their placeholder back without starting any agent. Agents that were awake (Herdr keeps their
   saved session) are started again with `--resume`, one at a time, and put to sleep as soon as they
   are ready. Only panes that still exist, have no agent, sit at a shell prompt and still have their
   working directory are handled. Herdr starts a restored pane's shell when a client attaches, so
   the plugin looks again, round after round, at the panes that are not ready yet, for up to twice
   `restore_settle_timeout_seconds`, without holding the others back. The others are left alone and
   logged as `RESTORE-SKIP` in the journal; the first line of a run is `RESTORE-START`, with the
   panes it found. Panes whose directory is gone are listed apart.

## Sidebar marker

A sleeping pane, and a space whose agents all sleep, report a `$sleep` token. Add it to the rows you
want in your Herdr config:

```toml
[ui.sidebar.agents]
rows = [["state_icon", "sleep", "workspace", "tab"], ["agent"]]

[ui.sidebar.spaces]
rows = [["state_icon", "sleep", "workspace"], ["branch", "git_status"]]
```

Herdr has no tab token, so a tab whose agents all sleep gets the icon as a label prefix instead
(`tab_prefix = false` turns that off). The token name and the icon are configurable.

## Keyboard shortcut

Bind the `sleep` action to put the focused agent to sleep right away, ignoring the idle delay and
focus but keeping every safety guard:

```toml
[[keys.command]]
key = "prefix+z"
type = "plugin_action"
command = "pepsit36.herdr-sleep.sleep"
description = "sleep this agent"
```

When a guard refuses (a draft in the prompt, a busy child, an excluded agent...), the action shows
a Herdr notification that says why and what to try, since nothing would be printed anywhere you
can see. It is always shown for the action, whatever `notify` says: `notify` only concerns the
sleep that happens by itself.

### Keep a tab awake

Bind the `keep-awake` action to stop a tab from ever sleeping, and again to let it sleep:

```toml
[[keys.command]]
key = "prefix+k"
type = "plugin_action"
command = "pepsit36.herdr-sleep.keep-awake"
description = "keep this tab awake (toggle)"
```

The label of the tab of the focused pane gets the `keep_awake_icon` (🔒) in front, and none of its
agents goes to sleep, by sweep or by `sleep` (`tab-kept-awake`; `sleep --force` ignores it). Agents
that already sleep stay asleep. The icon in the label is the whole state: it survives a Herdr
restart, and renaming the tab without it lifts the protection. From a terminal:
`herdr-sleep keep-awake [<pane|name>] [--on|--off]`.

The other actions are `wake`, `sweep` (sleep every idle agent now), `status` and `ensure-watcher`,
all available from Herdr's action picker.

## Configuration

`herdr plugin config-dir pepsit36.herdr-sleep` prints the directory; put a `config.toml` in it.
Every key is optional, unknown keys only produce a warning (see `herdr-sleep config`).

```toml
# Idle time before an agent is put to sleep, in minutes.
sleep_after_minutes = 15
# Statuses that count as idle: "idle", "done".
sleep_statuses = ["idle", "done"]
# Log what would happen, change nothing.
dry_run = false

# Appearance
sleep_icon = "💤"
token_name = "sleep"        # shown as $sleep in sidebar rows
tab_prefix = true           # prefix the label of a tab whose agents all sleep
keep_awake_icon = "🔒"      # prefix of a tab kept awake with the keep-awake action ("" turns it off)
preview_lines = 20          # last lines above the input box kept on the sleeping pane
notify = true               # Herdr notification when an agent falls asleep by itself

# Selection
include_names = []          # if set, only these agent names may sleep
exclude_names = []
exclude_cwd_regex = []      # never sleep an agent whose directory matches
protect_env_vars = []       # never sleep a process carrying one of these variables
# Regexes (case-insensitive) that keep an agent awake whenever they match:
exclude_name_regex = []     # the agent's name
exclude_screen_regex = []   # one of the last lines of its screen, e.g. "waiting for approval"
exclude_cmdline_regex = []  # its command line or that of any child, ignored children included
# Child processes that never block the sleep (case-insensitive regexes).
# Setting this replaces the built-in list.
ignore_child_regex = ["mcp", "npm exec", "language-server", "tsserver", "caffeinate",
  "<defunct>", "^\\(.*\\)$", "/cursor-agent/versions/"]

# Watcher
sweep_interval_seconds = 60

# After a Herdr restart: seconds a pane gets (twice that in all) to reach its shell
# prompt and for Herdr to release the agent's identity (5 to 600).
restore_settle_timeout_seconds = 60

# Prompt detection. The prompt is looked for between the last two rules of the screen (lines
# matching prompt_frame_regex), whatever sits above or below them, such as a tall status bar.
# Without such a frame the last prompt_area_lines rows are searched (5 to 40). An empty
# prompt_frame_regex turns the frame detection off, and with it the trimming of the sleeping pane
# preview. Both can be set per agent too.
prompt_area_lines = 15
prompt_frame_regex = '^\s*\u{2500}[\u{2500}\s]{9,}$'   # a line of at least ten "─"

# Sleep an agent even when a draft is typed in its prompt: the draft is saved, the prompt cleared
# (Ctrl+U), and the draft typed back, without Enter, when the agent wakes. Only a draft on one
# row is saved; a longer draft or a pasted text keeps the agent awake (draft-in-prompt). Off by
# default. Can be set per agent too.
sleep_with_draft = false

# Agents. Built in: claude, codex, cursor, gemini. A table overrides fields of a built-in
# or adds an agent; a new agent needs all four keys.
[agents.myagent]
bin = "myagent"             # process name
exit = "/quit"              # typed to make it quit
resume = ["--resume", "{sid}"]
prompt_regex = "^\\s*>"     # marks the input line, used to detect a draft and readiness
# prompt_area_lines = 20     # optional, overrides the global value for this agent
# prompt_frame_regex = ""    # optional, "" turns the frame detection off for this agent
```

The configuration is re-read on every sweep.

## Commands

```
herdr-sleep status [--json]                 list the managed agents and the watcher
herdr-sleep sleep [<pane|name>] [--force]   put one agent to sleep (--force skips the guards)
                  [--wait[=SECONDS]]        keep trying while the agent is not idle yet (default 120 s)
                  [--from-session]          resume the saved session of a pane with no agent, then sleep
herdr-sleep wake  [<pane|name>] [--no-wait] wake one agent
herdr-sleep sweep [--now] [--json]          sleep every agent that is idle long enough, now
                                            (--now: do not wait for the idle time)
herdr-sleep restore [--json]                put panes back to sleep after a Herdr restart (run by the
                                            startup hook, safe to run by hand)
herdr-sleep config [--json]                 print the effective configuration
herdr-sleep ensure-watcher                  start the watcher unless it is running
```

Without a pane or name, `sleep` and `wake` act on the pane of the Herdr action that started them.
A refused `sleep` prints the reason on stderr, exits 1 and says what to try (`--wait`, `--force`).
When the pane has no live agent but Herdr holds a saved session for it, `sleep <pane>` resumes the
agent and puts it to sleep.
`herdr-sleep <command> --help` documents the options, `--json` output and exit codes.

Sending a prompt with `herdr agent prompt` to a sleeping agent works: the text wakes the pane and
is replayed into the agent once it is ready.

## Troubleshooting

- **An agent never sleeps.** `herdr-sleep status` shows the last refusal per agent: `draft-in-prompt`
  (text typed at the prompt: set `sleep_with_draft = true` to sleep anyway and get a one-row draft
  back at wake), `busy-children` (a child process is running; add it to
  `ignore_child_regex` if it is harmless), `prompt-not-detected` (the screen shows no prompt that
  matches the agent's `prompt_regex`, which makes the plugin refuse rather than guess; the prompt
  is searched between the last two rules of the screen (`prompt_frame_regex`) and, without a frame,
  in the last `prompt_area_lines` rows: raise `prompt_area_lines` or adapt `prompt_frame_regex` when
  your agent draws its prompt differently),
  `no-conversation` (a Claude Code session whose conversation is not saved yet, typically before the
  first message: it could not be resumed, so the agent is left alone and retried later), and so on.
- **A tab never sleeps.** `tab-unreadable` means Herdr did not give the tab's label, so the agent is left alone until the next sweep. `tab-kept-awake` means its label starts with the `keep_awake_icon`: run
  the `keep-awake` action again, or rename the tab without the icon.
- **A Claude Code agent in Remote Control stays awake.** `remote-control` means the agent was
  started with `--remote-control` (or `--rc`), or its screen shows the `/rc active` badge: it is
  driven from another device, so putting it to sleep would cut that session. A terminal too narrow
  to draw the badge hides it, and only the flag protects then. `sleep --force` ignores it.
- **An agent stays awake on purpose.** `excluded-name-regex`, `excluded-screen-regex` and
  `excluded-cmdline-regex` mean one of your `exclude_name_regex`, `exclude_screen_regex` or
  `exclude_cmdline_regex` patterns matched; the refusal names the text that matched. These patterns
  are not the same as `ignore_child_regex`, which makes a child process stop counting as work.
  `herdr-sleep sleep --force` ignores them like every other guard. A pattern that does not compile
  is reported with its key and the Watcher keeps the previous valid configuration.
- **The pane shows a bare shell after a wake.** When the resume command fails within ten seconds,
  the pane says so, prints the command it tried and stays at a login shell. That report needs a
  shell the plugin knows (bash, zsh, fish and other POSIX shells); with any other shell the
  agent is still resumed, but a failure is silent.
- **Nothing happens at all.** Run `herdr-sleep ensure-watcher`, then check the journal,
  `herdr-sleep.log` in the plugin's state directory (printed by `herdr-sleep status`).
- **See what would happen** with `dry_run = true`.
- Sleeping panes after a Herdr restart appear once a Herdr client attaches; panes the plugin could
  not restore are listed as `RESTORE-SKIP <pane> <reason>` in the journal (and by `herdr-sleep
  status`), followed by a `RESTORE-DONE restored=N skipped=M missing_cwd=K` line. Panes whose
  working directory is gone are `RESTORE-CWD`; a pane Herdr shows asleep that the plugin has no
  record of is `sleeping-agent-not-registered`. Run `herdr-sleep restore` again to retry them; raise
  `restore_settle_timeout_seconds` when many panes are skipped as `identity-not-released`. The panes
  that were asleep are in `sleepers.json` in the state directory.

## What is verified

- **Claude Code**: sleep, wake by focus, key and prompt, replay, draft guard, tab and space markers
  and restart recovery (including agents that were awake at the restart) are covered by an end-to-end suite that runs the plugin against real Herdr
  binaries (0.9.2 and the latest release) in Docker, with a scripted stand-in for the agent that
  reproduces Claude Code's prompt. Prompt detection was checked against real Claude Code screens.
- **Codex, Cursor, Gemini**: built-in profiles are best effort and have not been run against the real
  agents. If one misbehaves, override its fields in `[agents.<kind>]` and please open an issue.
- **Shells**: the end-to-end suite runs the wake in bash; zsh is only covered by unit tests of the script, and fish is covered by a unit
  test that runs the wake script in a real fish when one is installed.
- **Linux**: tested. **macOS**: the binaries are cross-compiled and the code paths that differ
  (process environment lookup) are unit tested, but nothing has been run on a Mac yet.
- Herdr versions 0.9.3 and 0.9.4 were not exercised individually.

## Development

```sh
make check   # fmt, clippy, tests
make build   # release binaries in target/release
make link    # build and link the plugin into your Herdr for development
make e2e     # Docker end-to-end suite on Herdr 0.9.2 and the latest release
```

Before a branch merges, `cargo xtask changelog stamp` (or `make changelog`) gives the `[Unreleased]`
section of `CHANGELOG.md` the version the merge will publish; run it again after a rebase. See
`CONTRIBUTING.md` for the workflow, `CLAUDE.md` for the working conventions, `GLOSSARY.md` for the
vocabulary and `docs/adr/` for the decisions that shape the design.

## Licence

MIT, see `LICENSE`.

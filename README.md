# Herdr Sleep

A [Herdr](https://herdr.dev) plugin that puts idle coding agents to sleep to free memory, and wakes
them on demand with their pane, name and session intact.

A coding agent that has been idle for fifteen minutes still holds hundreds of megabytes. Herdr Sleep
quits it cleanly, keeps the pane open with a tiny placeholder process (about 2 MB), and brings the
agent back with its own `--resume` the moment you focus the pane, press a key or send it a prompt.
What you typed while it was asleep is replayed into the agent once it is ready.

- No signal is ever sent: the agent is asked to quit with its own exit command (`/exit`).
- Nothing is lost: pane, position, name and session are kept; unsent drafts block the sleep.
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
   **placeholder**, which shows the last lines of the agent's screen and announces a
   `sleeping-<kind>` agent to Herdr under the same name.
3. **Wake.** Focusing the pane (or its tab or space), pressing any key in it, sending it a prompt
   with `herdr agent prompt`, or running `herdr-sleep wake` starts the agent again with its
   `--resume`. The pane name comes back, and what you typed meanwhile is typed into the agent when
   it shows its prompt.
4. **After a Herdr restart,** the plugin restores sleeping panes as placeholders at startup, without
   starting any agent, instead of letting Herdr relaunch them all at once. Set
   `resume_agents_on_restore = false` in the `[session]` section of Herdr's config so that Herdr does
   not also relaunch them. Only panes that still exist, have no agent and sit at a shell prompt are
   restored (Herdr starts a restored pane's shell when a client attaches, so the plugin waits up to
   20 seconds per pane); the others are left alone and logged as `RESTORE-SKIP` in the journal.

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
preview_lines = 20          # last lines of the agent kept on the sleeping pane
notify = true               # Herdr notification when an agent falls asleep by itself

# Selection
include_names = []          # if set, only these agent names may sleep
exclude_names = []
exclude_cwd_regex = []      # never sleep an agent whose directory matches
protect_env_vars = []       # never sleep a process carrying one of these variables
# Child processes that never block the sleep (case-insensitive regexes).
# Setting this replaces the built-in list.
ignore_child_regex = ["mcp", "npm exec", "language-server", "tsserver", "caffeinate",
  "<defunct>", "^\\(.*\\)$", "/cursor-agent/versions/"]

# Watcher
sweep_interval_seconds = 60

# Agents. Built in: claude, codex, cursor, gemini. A table overrides fields of a built-in
# or adds an agent; a new agent needs all four keys.
[agents.myagent]
bin = "myagent"             # process name
exit = "/quit"              # typed to make it quit
resume = ["--resume", "{sid}"]
prompt_regex = "^\\s*>"     # marks the input line, used to detect a draft and readiness
```

The configuration is re-read on every sweep.

## Commands

```
herdr-sleep status [--json]                 list the managed agents and the watcher
herdr-sleep sleep [<pane|name>] [--force]   put one agent to sleep (--force skips the guards)
herdr-sleep wake  [<pane|name>] [--no-wait] wake one agent
herdr-sleep sweep [--json]                  sleep every agent that is idle long enough, now
herdr-sleep config [--json]                 print the effective configuration
herdr-sleep ensure-watcher                  start the watcher unless it is running
```

Without a pane or name, `sleep` and `wake` act on the pane of the Herdr action that started them.
`herdr-sleep <command> --help` documents the options, `--json` output and exit codes.

Sending a prompt with `herdr agent prompt` to a sleeping agent works: the text wakes the pane and
is replayed into the agent once it is ready.

## Troubleshooting

- **An agent never sleeps.** `herdr-sleep status` shows the last refusal per agent: `draft-in-prompt`
  (text typed at the prompt), `busy-children` (a child process is running; add it to
  `ignore_child_regex` if it is harmless), `prompt-not-detected` (the screen shows no prompt that
  matches the agent's `prompt_regex`, which makes the plugin refuse rather than guess),
  `no-conversation` (a Claude Code session whose conversation is not saved yet, typically before the
  first message: it could not be resumed, so the agent is left alone and retried later), and so on.
- **Nothing happens at all.** Run `herdr-sleep ensure-watcher`, then check the journal,
  `herdr-sleep.log` in the plugin's state directory (printed by `herdr-sleep status`).
- **See what would happen** with `dry_run = true`.
- Sleeping panes after a Herdr restart appear once a Herdr client attaches; panes the plugin could
  not restore are listed as `RESTORE-SKIP <pane> <reason>` in the journal, followed by a
  `RESTORE-DONE restored=N skipped=M` line. The panes it knows about are in `sleepers.json` in the
  state directory.

## What is verified

- **Claude Code**: sleep, wake by focus, key and prompt, replay, draft guard, tab and space markers
  and restart recovery are covered by an end-to-end suite that runs the plugin against real Herdr
  binaries (0.9.2 and the latest release) in Docker, with a scripted stand-in for the agent that
  reproduces Claude Code's prompt. Prompt detection was checked against real Claude Code screens.
- **Codex, Cursor, Gemini**: built-in profiles are best effort and have not been run against the real
  agents. If one misbehaves, override its fields in `[agents.<kind>]` and please open an issue.
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

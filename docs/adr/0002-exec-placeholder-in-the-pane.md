---
status: accepted
date: 2026-11-07
---

# Replace the pane's shell with the Placeholder via `exec`

Sleeping must keep the pane (position, cwd, name, Herdr identity) and leave as little as possible resident. Alternatives were a `SIGSTOP`-ed agent (frees no memory), closing and recreating the pane (loses layout, name and focus events), or keeping the shell alive and running the Placeholder as its child (5–10 MB of shell per sleeping pane). We instead run `exec <placeholder>` in the pane's shell: the shell process is replaced, nothing else stays resident, and on Wake the Placeholder `exec`s the user's shell again to launch the agent and fall back to a login shell afterwards.

## Consequences

- The plugin must wait for Herdr to release the native agent identity before the Placeholder reports its own, otherwise the pane shows a stale agent.
- The Placeholder must restore the terminal state (termios, cwd) before handing over to the shell.
- The wake script is parsed by the pane's own shell, so it is written in that shell's syntax: only the exit status of the resume command differs (`$?`, `$status` in fish), and a shell the plugin does not know gets a script without it. fish once rejected a POSIX script and closed the pane.

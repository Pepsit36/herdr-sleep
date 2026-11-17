---
status: accepted
date: 2026-11-08
---

# The plugin restores sleeping panes itself after a Herdr restart

After a Herdr restart a sleeping pane comes back as a plain shell, and the plugin puts the Placeholder back. Sleep records every sleeping pane in `<state_dir>/sleepers.json` (kind, session, name, working directory); Wake and Replay remove the entry when the Agent is Ready, and the Sweep prunes the entries of panes that no longer exist. The `[[startup]]` hook runs `herdr-sleep restore`, which, one pane after the other, takes every registered pane that still exists, runs no agent and sits at its shell prompt, waits until Herdr has released the native agent identity, and then runs `exec <placeholder>` with absolute paths exactly as Sleep does. A pane that does not qualify is left alone and journaled as `RESTORE-SKIP`. The Placeholder no longer reports a resume command, so Herdr's own restore does nothing for it; users set `[session] resume_agents_on_restore = false` so that Herdr does not relaunch every agent at once.

## Considered options

- Native resume, the Placeholder reporting its own command as the agent's resume command: Herdr refuses a path as a resume command (`invalid_resume_argv: resume_argv must start with a plain command name`), so the command would have to be the bare name `herdr-sleep-ph` and work only when that name is on the `PATH` of the pane's shell. Herdr also clears the agent name and the stored resume command about 60 ms after the Agent's process disappears, so the Placeholder had to declare its identity a second time after a timer. Both make the restore depend on the user's environment and on timing.
- A symlink of `herdr-sleep-ph` in a directory of the user's `PATH`: it needs an install step outside Herdr, and a wrong `PATH` in the pane's shell silently breaks it.

## Consequences

- Nothing depends on the `PATH` or on a timer: the restore uses absolute paths and the observable state of the pane.
- Herdr starts the pane's shell when a client attaches, so `restore` waits (20 s per pane at the time, `restore_settle_timeout_seconds` since ADR 0009) for the shell and for the identity release, and skips the pane after that. `restore` also starts the Watcher when it ends, because startup hooks run concurrently.
- The registry is a second source of truth next to Herdr; it is tolerant to loss (a pane missing from it stays a plain shell) and to staleness (an entry is only acted on when the pane has no agent and sits at a shell prompt).
- The Placeholder declares its identity once, when it starts. Herdr's own process detection still clears the agent name a fraction of a second after the Sleeping agent appears (observed on Herdr 0.9.2 and 0.9.5: about 200 ms later, `agent get` then has no `name`, while the agent label, session and tokens stay). Nothing signals it in advance, so the Placeholder polls `herdr agent get` every 50 ms, gives the name again as soon as it is gone, and stops after at most 1.5 s if it never goes. A name given after the clearing is kept.

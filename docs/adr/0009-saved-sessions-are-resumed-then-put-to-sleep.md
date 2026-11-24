---
status: accepted
date: 2026-11-17
---

# A Saved session is resumed, then put to sleep, never given a Placeholder directly

After a Herdr restart, `restore` also handles the panes that Herdr reports with a Saved session and no live Agent: Agents that were awake, never went through Sleep, and were stopped by the restart. They are not in `sleepers.json`, so ADR 0008 alone left them as plain shells. `restore` now reads them from Herdr (`agent_session` on a pane with no agent label), and for each one whose working directory exists it starts the Agent again with `herdr agent start --kind <kind> --pane <pane> -- <resume arguments>`, waits for the ordinary Sleep guards to pass, and runs the ordinary Sleep. One Agent runs at a time. `herdr-sleep sleep <pane>` takes the same path for such a pane (`--from-session` forces it), and `herdr-sleep restore` can be run by hand.

## Considered options

- A Placeholder directly in the pane, as for registered panes: Herdr keeps the Saved session as the pane's identity with its own source (`herdr:claude`) and silently ignores every report of another source while it does. `report-agent`, `report-agent-session` and `release-agent` all exit 0 and change nothing (observed on Herdr 0.9.2 and 0.9.5), so the Placeholder never appears as a Sleeping agent. The Saved session is only released when a real Agent runs in the pane and quits.
- Recording every live Agent in `sleepers.json` while it runs: it would also keep the Agent's name across a restart (Herdr forgets the name of an Agent that never slept), but it makes the registry a copy of Herdr's own state that the Sweep must keep fresh, and the Saved session already says what to resume.

## Consequences

- A restart costs one real Agent start per Saved session pane, one after the other, so memory peaks at one Agent. This is the manual procedure (`herdr agent start`, wait for idle, `herdr-sleep sleep`) done by the plugin.
- Herdr forgets the name of an Agent that never slept. The resumed Agent gets the name `<kind>-<pane>`; a name that went through Sleep is kept by the registry.
- An Agent that is back but cannot sleep (a Guard refuses) stays awake and the Sweep puts it to sleep once it has been idle long enough.
- `dry_run` never starts an Agent.
- Panes whose working directory is gone are not resumed; they are listed apart (`RESTORE-CWD`, `herdr-sleep status`).
- Herdr starts the pane's shell when a client attaches, so a pane that is not ready is looked at again after the others, round after round, for twice `restore_settle_timeout_seconds` (60 s by default) from the start of the run. A slow pane never holds the others back.
- The Agent gets a name Herdr accepts (lowercase letters, digits, `-` and `_`, 32 characters at most): a kept name that does not qualify is replaced by `<kind>-<pane>` in lowercase. An Agent that started although `herdr agent start` reported a failure is put to sleep all the same.
- A registered pane for which Herdr still holds a Saved session, with no agent label, when its time is up is resumed then put to sleep, because a Placeholder cannot take the identity over.

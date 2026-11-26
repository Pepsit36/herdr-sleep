---
status: accepted
date: 2026-11-26
---

# Every live Agent is recorded so that a restart can bring it back

The Sweep keeps `<state_dir>/live-agents.json` up to date on every pass: for each managed Agent that is awake, the Kind, the Session, the name and the working directory Herdr reports. After a Herdr restart `restore` uses it for a pane that shows no Agent, holds no Saved session, is not in the registry of sleeping panes and has a record: the Agent is resumed with its Session and put to sleep, as for a Saved session (ADR 0009). The record also gives back the name of an Agent that never slept, which Herdr forgets, so it keeps its name across a restart.

A record is dropped when its pane is gone, when its pane holds a sleeping Agent, when the Agent was seen alive and then gone (the user quit it), or when nobody saw the Agent for seven days. A pane that shows no Agent only because Herdr just restarted keeps its record. Two things tell a quit from a restart: the "seen alive" memory of the Watcher process, which is never saved, and the age of the pane's shell, which is older than the last sighting of the Agent when the pane was there all along and newer after a restart. The second one works for a quit that no Watcher pass saw, such as one just before a restart, and `restore` applies it too (`agent-quit-before`).

## Considered options

- Herdr's Saved session as the only source (ADR 0009 as it was): an Agent that Herdr never saw report a session (a Cursor without its hook, an Agent started before its hook was installed) is not a Saved session, and was silently left as a plain shell after a restart.
- Recording from Sleep only (the registry): an Agent that never slept is not in it, which is the case that loses panes.
- Reading Herdr's `session.json` for `agent_resume` or `agent_session`: a private file whose format Herdr may change, read while Herdr writes it.

## Consequences

- The record is a copy of what Herdr reports, refreshed every Sweep pass (five seconds to a minute) and tolerant to loss: a missing or garbled file is an empty record. The age of a shell is read with `ps`; when it cannot be told, the record is kept.
- An Agent for which Herdr reports no session is not recorded and cannot be restored. Cursor needs `herdr integration install cursor` for Herdr to report its session; the Sleep refusal `no-session` says so.
- A Claude Code session whose conversation was never saved cannot be resumed (`--resume` fails): `restore` starts the Agent fresh in the same directory (`RESTORE-FRESH`), which loses nothing, and it goes to sleep once it has a conversation.
- With `dry_run` on, `restore` only journals what it would do (`RESTORE-DRY-RUN`) and touches no pane.

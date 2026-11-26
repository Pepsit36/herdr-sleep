# Herdr Sleep

A Herdr plugin that puts idle coding agents to sleep to free memory and wakes them on demand, keeping their pane, name and session.

## Language

### Agents

**Agent**:
A coding-agent process (Claude Code, Codex, Cursor, Gemini…) running in a Herdr pane and tracked by Herdr.
_Avoid_: Bot, assistant, session

**Kind**:
The family an Agent belongs to, as Herdr labels it (`claude`, `codex`, `cursor`, `gemini`).
_Avoid_: Type, agent label

**Profile**:
The per-Kind knowledge the plugin needs to put an Agent to sleep and bring it back: binary, exit command, resume arguments, prompt marker.
_Avoid_: Agent config, adapter

**Session**:
The agent-side conversation identifier that lets an Agent be resumed where it left off.
_Avoid_: Conversation id, sid (outside code)

**Managed agent**:
An Agent whose Kind has a Profile; the only agents the plugin ever acts on.
_Avoid_: Supported agent, known agent

### Sleep lifecycle

**Settled**:
An Agent whose Herdr status is `idle` or `done`, i.e. it has nothing in progress.
_Avoid_: Inactive, finished

**Idle time**:
How long an Agent has been Settled, unfocused and unchanged, measured by the Watcher.
_Avoid_: Inactivity, age

**Guard**:
A condition that forbids sleep even when the Idle time is reached (working, focused, typed draft that is not saved, busy children, an agent that is not a job of the pane's own shell, Remote Control, a kept-awake tab, protected environment, unrecognised prompt, or a name, screen or command line the user excluded by pattern).
_Avoid_: Check, filter, rule

**Sleep**:
Quitting an Agent through its own exit command and replacing the pane's shell with a Placeholder, so the pane holds nothing but the Placeholder.
_Avoid_: Hibernate, park, suspend, kill

**Placeholder**:
The tiny process that occupies a sleeping pane: it announces the Sleeping agent to Herdr, shows the Preview, and waits for a reason to Wake.
_Avoid_: Stub, sleeper, daemon

**Sleeping agent**:
The identity a Placeholder reports to Herdr: Kind prefixed with `sleeping-`, the original name, and status `idle`.
_Avoid_: Ghost, parked agent

**Preview**:
The last lines of the Agent's screen above its input box, captured before Sleep and displayed by the Placeholder. The input box and the status bar below it are not part of it.
_Avoid_: Snapshot, screenshot

**Wake**:
Launching the Agent again with its Session, in place of the Placeholder, through the user's shell.
_Avoid_: Resume (reserved for the agent-side command), restore, unpark

**Restore**:
After a Herdr restart, putting every pane back to sleep that was asleep before it or that holds a Saved session. A pane that was asleep gets its Placeholder back without starting any Agent; a Saved session pane first gets its Agent back, one at a time, then sleeps.
_Avoid_: Resume (reserved for the agent-side command), relaunch

**Live record**:
What the plugin remembers of every awake Agent (Kind, Session, name, working directory), so that a restart can bring back an Agent that Herdr holds no Saved session for.
_Avoid_: Registry (that is the list of sleeping panes)

**Saved session**:
The Session Herdr keeps for a pane whose Agent a restart stopped: the pane is a plain shell with no live Agent, and the Session says which conversation it held.
_Avoid_: Orphan, dangling session

**Wake trigger**:
Any event that causes a Wake: the pane receiving focus, any key pressed in the Placeholder, a prompt sent to the pane, or an explicit `wake` command.
_Avoid_: Wake source, wake-up event

**Captured input**:
Keyboard or pasted text received by the Placeholder before the Agent is Ready, stripped of control and escape sequences.
_Avoid_: Buffered input, queued keys

**Kept-awake tab**:
A tab whose label carries the keep-awake icon: none of its Agents goes to sleep unless forced.
_Avoid_: Pinned tab, locked tab

**Remote Control**:
The mode where a Claude Code Agent is driven from another device. An Agent in Remote Control never sleeps unless forced.
_Avoid_: Remote session, rc

**Saved draft**:
The text typed in the Agent's prompt when it goes to sleep, kept while the pane sleeps and typed back, never submitted, once the Agent is Ready.
_Avoid_: Backup, stash

**Replay**:
Sending the Captured input to the Agent once it is Ready.
_Avoid_: Forwarding, playback

**Ready**:
A woken Agent that Herdr recognises again under its Kind and that shows its prompt line.
_Avoid_: Up, alive, available

### Scheduling and display

**Sweep**:
One pass over all Managed agents that puts to sleep every one whose Idle time is reached and no Guard applies.
_Avoid_: Scan, tick, cron

**Watcher**:
The detached process that runs a Sweep at a fixed interval while the Herdr server is alive.
_Avoid_: Daemon, scheduler, timer

**Manual sleep**:
A Sleep requested by the user (action, shortcut or CLI) that ignores Idle time and focus but keeps every safety Guard.
_Avoid_: Forced sleep (reserved for the CLI flag that skips Guards)

**Sleep marker**:
The visual sign of sleep: the sleep icon as a metadata token on a sleeping pane or a fully sleeping workspace, and as a prefix on a fully sleeping tab label.
_Avoid_: Badge, indicator, zz

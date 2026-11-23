---
status: accepted
date: 2026-11-23
---

# A tab kept awake is marked by the icon in its label, nothing else

The keep-awake action puts `keep_awake_icon` (🔒 by default) and a space in front of the label of the tab of a pane. The `tab-kept-awake` guard refuses to put any agent of a tab to sleep, by sweep or by `sleep`, while its label starts with that icon, after the `💤 ` prefix a tab whose agents all sleep already carries. Running the action again removes the icon, and renaming the tab without it lifts the protection. `sleep --force` ignores the guard. Agents already asleep stay asleep.

## Considered options

- A list of tab ids in the state directory, the icon being only a display: the state and what the user sees can drift, and Herdr may hand out other tab ids after a restart, which would silently protect the wrong tab or none.
- A Herdr tab token or metadata: Herdr has none for tabs (see the tab prefix of the sleep marker), and a token would not show in the tab name.

## Consequences

- No file to keep in sync: the protection survives a Herdr restart and is visible at a glance. It is also fragile on purpose: a user who renames the tab loses it, and sees that straight away.
- The sleep prefix is always outermost (`💤 🔒 build`), so the marker sync adds and removes it without touching the lock.
- Changing `keep_awake_icon` lifts the protection of the tabs that carry the old icon; an empty icon turns the feature off.
- The guard reads the tab label of the agent on every check, which is one more Herdr call per guarded agent.

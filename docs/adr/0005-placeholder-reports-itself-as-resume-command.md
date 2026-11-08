---
status: superseded by ADR 0008
date: 2026-11-07
---

# The Placeholder declares its own command as the agent's resume command

Since Herdr 0.9.2 an agent can report its resume command, and Herdr relaunches it after a server restart. The Placeholder reports *its own* command line (not the agent's) as the resume command of the Sleeping agent. After a restart Herdr therefore brings back the Placeholder, not the agent: sleeping panes stay asleep, no memory spike occurs, and the plugin needs no registry or restore logic of its own. The Placeholder still knows the real resume command and uses it on Wake. This is why `min_herdr_version` is `0.9.2`.

## Consequences

- Metadata tokens are not restored by Herdr after a restart; the relaunched Placeholder re-reports the Sleep marker itself.
- If a future Herdr release filters self-reported agents on restore, a plugin-side restore will have to be added back.

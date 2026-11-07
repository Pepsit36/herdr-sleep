---
status: accepted
date: 2026-11-07
---

# Run Sweeps from a self-healing detached Watcher

Herdr offers no timer for plugins and treats `[[startup]]` commands as one-shot initialisation, and `[[startup]]` does not run when a plugin is enabled after the server started. A system scheduler (launchd, systemd user timers) would work but needs an install step outside Herdr, two OS-specific implementations, and breaks the "install from the marketplace and it works" expectation. The Watcher is the plugin binary itself in `watch` mode, started detached by `[[startup]]`, guarded by a pid lock in the plugin state directory, and exiting on its own when the Herdr socket disappears. Every other entry point (event hooks, actions, CLI) checks the lock and relaunches the Watcher if it is missing, so no manual step is ever required; an explicit `ensure-watcher` action remains for discoverability.

## Consequences

- There is at most one Watcher per Herdr server; a stale lock is detected by pid liveness.
- The Watcher must survive its parent command exiting (`setsid`, detached stdio), which Herdr tolerates as long as the process stops when the server stops.

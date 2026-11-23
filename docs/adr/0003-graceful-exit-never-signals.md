---
status: accepted
date: 2026-11-07
---

# Quit agents through their own exit command, never with signals

Most sleep plugins send `SIGTERM`. That skips the agent's shutdown path and discards anything typed in its composer. We type the Profile's exit command (`/exit`, `/quit`…) and press Enter, and we never signal the process. The price is a set of Guards that must hold before typing anything: the prompt line must be recognised (fail-closed when the Profile's `prompt_regex` does not match), it must be empty, and the agent must be Settled. If the agent has not quit after a bounded wait, the attempt is abandoned and logged; the next Sweep retries.

## Consequences

- Agents whose prompt line is not recognised never sleep; the reason is visible in `status` and the log.
- Each Profile needs a reliable `prompt_regex`; unverified built-ins are documented as such.
- A draft keeps the agent awake unless the user opts into saving it (ADR 0010).

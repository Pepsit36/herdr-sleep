---
status: accepted
date: 2026-11-23
---

# A draft in the prompt is saved and typed back at Wake, never discarded

ADR 0003 refuses to sleep while text is typed in the prompt, because the exit command would be appended to it (`/exit/exit`, or `draft/exit`) and quitting would lose it. With `sleep_with_draft = true` (global or per agent, off by default) the plugin sleeps such an agent anyway: it reads the draft from the screen, writes it to `draft-<pane>.txt` in the state directory, clears the prompt with Ctrl+U, checks on screen that the prompt is empty, and then types the exit command as usual. At Wake the Replay types the draft back into the prompt once the agent is Ready, after the Captured input and without Enter.

Only a draft that fits on one row of the prompt is saved. A draft over several rows, a pasted text that the agent shows as a summary (`[Pasted text #1 +20 lines]`, `[Image #1]`) or a draft with control characters keeps the agent awake, with the `draft-in-prompt` refusal. If the prompt is not empty after Ctrl+U, the Sleep is abandoned and the draft stays where it was.

An abandoned Sleep also cleans up after itself, with or without this setting: the exit command typed into the prompt is erased with one Backspace per character, and a saved draft is typed back.

## Considered options

- Keep refusing every draft (ADR 0003 alone): safe, but an agent with a forgotten half-typed sentence never sleeps, and `sleep --force` still appends the exit command to the text.
- Save multi-row drafts too: the screen cannot tell a real line break from a line the terminal wrapped, so the restored text could differ from what the user typed. Refusing is the only faithful answer.
- Clear the prompt with Ctrl+C: it clears a draft in Claude Code but can quit the agent when the prompt is already empty. Ctrl+U has no effect on an empty prompt.
- On by default: it changes what a Sweep does to the user's text without being asked. It stays opt-in.

## Consequences

- The draft is user text. It is never written to the journal (only its length), and the file is deleted when it is typed back or when the Sleep is abandoned.
- Saving relies on what the screen shows, so it is fail-closed: any doubt keeps the agent awake.
- A Wake that never reaches Ready keeps the draft file; the next Sleep of the pane replaces it.

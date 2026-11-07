---
status: accepted
date: 2026-11-07
---

# Write the whole plugin in Rust

The plugin's value is measured in freed memory: a sleeping pane must cost about 1 MB of RSS for days. A Placeholder written for Node or Python would weigh 30–50 MB and cancel most of the gain, and splitting the project into a scripted CLI plus a native Placeholder would mean two toolchains for one small tool. We therefore write both binaries in Rust, with the Placeholder depending on `libc` only, release profile tuned for size (`opt-level = "z"`, LTO, `panic = "abort"`, stripped).

## Consequences

- Users without a Rust toolchain rely on prebuilt binaries (see ADR 0006).
- The author's default stack for other projects is TypeScript; this deviation is deliberate.

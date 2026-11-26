---
status: accepted
date: 2026-11-07
---

# Ship prebuilt binaries through the manifest's `[[build]]` hook

`herdr plugin install` clones the GitHub repository and runs `[[build]]`; Herdr installs no toolchain. Requiring `cargo` from every user would make a Rust plugin the hardest one to install on the marketplace, and committing binaries would bloat the repository with unreproducible artifacts. The build hook is a small POSIX shell script: it downloads the release archive matching the checked-out tag, OS and architecture from GitHub Releases, verifies its checksum, and falls back to `cargo build --release` when no matching asset exists (unreleased commit, unknown platform, offline). Release assets for Linux and macOS (x86_64 and aarch64) are produced by the GitLab CI pipeline, macOS targets being cross-compiled from Linux with `cargo-zigbuild`.

## Consequences

- The GitHub repository is a push mirror of the GitLab source of truth and must carry the tags, since the script resolves assets by tag.
- macOS binaries are neither signed nor notarised; they are command-line tools launched from a terminal, which Gatekeeper does not block.
- The script downloads only when the checkout is the code of the release the manifest names: the
  head is the `chore(release)` commit of that version, or no file the binaries come from changed
  since its tag. A checkout ahead of its release, a branch or a fork is built from source, and so is
  one whose downloaded binaries do not run on the machine. Without git there is nothing to compare
  and the manifest is trusted.

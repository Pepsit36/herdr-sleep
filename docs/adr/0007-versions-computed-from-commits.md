---
status: accepted
date: 2026-11-07
---

# Versions are computed from Conventional Commits and committed by the release job

Nobody chooses a version number by hand. `cargo xtask changelog version` derives it from the non-merge commits a branch adds on top of `main`: a `feat:` subject publishes a minor, a `fix:` subject a patch, a breaking marker (`type!:` or `BREAKING CHANGE:` in a body) a major, and every other type (`refactor`, `perf`, `docs`, `chore`, `ci`, `test`, `build`, `style`) publishes nothing and ships with the next release. The author stamps `CHANGELOG.md` in the branch (`cargo xtask changelog stamp`), the branch pipeline checks that the stamped version is the one the merge will publish, and the `release` job that runs on `main` after a merge writes that version into `herdr-plugin.toml`, `Cargo.toml` and `Cargo.lock`, commits it as `chore(release): X.Y.Z`, tags the commit `vX.Y.Z`, builds the binaries and publishes them. Merges are semi-linear merge commits, never squashes, so the commit types the author stamped against are the ones the release job reads.

## Considered options

- Keeping `0.0.0` on `main` and injecting the version at build time only: the Herdr marketplace and `herdr plugin install` read `herdr-plugin.toml` from the default branch, so every user would see `0.0.0`.
- Letting the author bump the version in the branch: two open merge requests would race for the same number, and the merge order, not the author, decides which version is next.

## Consequences

- `main` receives exactly one commit per release that no human wrote; it is the only commit ever pushed to `main` outside a merge request, and it carries `[skip ci]`.
- A release that fails after the merge leaves `main` with a stamped `CHANGELOG.md` and no tag; a `release:backlog` job blocks every other branch until that release is repaired or retried.

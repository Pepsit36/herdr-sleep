# Contributing

Thanks for helping. The project is developed on a self-hosted GitLab and mirrored to GitHub; issues
and merge requests are welcome on either, and maintainers port GitHub contributions to GitLab.

## Set up

You need a Rust toolchain (see `rust-version` in `Cargo.toml`), and Docker for the end-to-end suite.

```sh
make check    # fmt, clippy with warnings denied, tests
make build    # release binaries in target/release
make link     # build, install the binaries under bin/ and link the plugin into your Herdr
make unlink
make e2e      # Docker end-to-end suite against real Herdr binaries
```

`make link` points your running Herdr at your working tree, which is the quickest way to try a
change. Do not run experiments against a session you care about: the end-to-end suite never touches
your Herdr, it runs everything in containers.

## Layout

- `crates/herdr-sleep`: the plugin command line and its logic (guards, sleep, wake, watcher, markers).
- `crates/herdr-sleep-ph`: the placeholder process. It depends on `libc` only; keep it that way, its
  memory footprint is the point of the project.
- `crates/xtask`: repository tooling (`cargo xtask ...`): changelog, versions, release.
- `tests/e2e`: the Docker end-to-end suite and its stand-in agent.
- `docs/adr`: the decisions that shape the design. Read them before changing what they decide.
- `GLOSSARY.md`: the vocabulary used in code, docs and commits.

## Making a change

1. Open or pick an issue.
2. Branch from `main`: `feature/<n>-...`, `fix/<n>-...` and so on.
3. Write Conventional Commits in English (`feat(wake): ...`, `fix(sleep): ...`), atomic, with a scope.
   Only `feat:` (minor), `fix:` (patch) and breaking changes (`!` or a `BREAKING CHANGE:` footer)
   publish a release; the other types ship with the next one.
4. Describe what changes for a user under `## [Unreleased]` in `CHANGELOG.md`, then run
   `cargo xtask changelog stamp` and commit the result. Run it again after a rebase.
5. `make check` must pass, and `make e2e` for anything touching sleep, wake, the placeholder, the
   watcher or the manifest.
6. Open a merge request. The project merges with a merge commit after a rebase, never a squash.

Versions are computed from the commits, never edited by hand (ADR 0007). No em dash anywhere, and
code, comments and docs are in English.

## Reporting a problem

Include the output of `herdr-sleep status`, the relevant lines of the journal (`herdr-sleep.log` in
the state directory it prints), your Herdr version (`herdr --version`) and the agent involved.

# Herdr Sleep

A [Herdr](https://herdr.dev) plugin that puts idle coding agents to sleep to free memory, and
wakes them on demand with their pane, name and session intact.

**Work in progress.** The plugin is being built towards its first release; nothing is
installable yet. Follow the [issues](https://git.duplessy.eu/sebastien/herdr-sleep/-/issues) for
progress.

## Development

```sh
make check   # fmt, clippy, tests
make build   # release binaries in target/release
make e2e     # Docker end-to-end suite (not in place yet)
```

Before a branch merges, `cargo xtask changelog stamp` (or `make changelog`) gives the `[Unreleased]`
section of `CHANGELOG.md` the version the merge will publish; run it again after a rebase.

See `CLAUDE.md` for the working conventions, `GLOSSARY.md` for the vocabulary and `docs/adr/`
for the decisions that shape the design.

## Licence

MIT, see `LICENSE`.

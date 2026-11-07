//! Library side of Herdr Sleep: everything the `herdr-sleep` binary needs
//! that is not command line handling.
//!
//! - [`config`] loads and validates `config.toml`: thresholds, filters and the
//!   Profiles of the managed Agents.
//! - [`herdr`] talks to Herdr through the `herdr` command line and its JSON
//!   output, and holds the typed models of what Herdr reports.
//! - [`procs`] inspects the processes behind a pane (process tree, memory,
//!   environment) on Linux and macOS without any extra crate.

pub mod config;
pub mod herdr;
pub mod procs;

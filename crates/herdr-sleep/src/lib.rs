//! Library side of Herdr Sleep: everything the `herdr-sleep` binary needs
//! that is not command line handling.
//!
//! - [`config`] loads and validates `config.toml`: thresholds, filters and the
//!   Profiles of the managed Agents.
//! - [`guards`] decides whether an agent may sleep and analyses its prompt.
//! - [`herdr`] talks to Herdr through the `herdr` command line and its JSON
//!   output, and holds the typed models of what Herdr reports.
//! - [`journal`] is the append-only log of what the plugin did and why.
//! - [`ops`] holds the operations that change panes, starting with Sleep.
//! - [`procs`] inspects the processes behind a pane (process tree, memory,
//!   environment) on Linux and macOS without any extra crate.

pub mod config;
pub mod guards;
pub mod herdr;
pub mod journal;
pub mod ops;
pub mod procs;

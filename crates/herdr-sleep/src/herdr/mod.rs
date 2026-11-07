//! Access to Herdr: the CLI client and the models of its JSON output.

pub mod client;
pub mod model;

pub use client::{
    CommandOutput, CommandRunner, HerdrClient, HerdrError, MetadataChange, ProcessRunner,
    ReadFormat, ReportAgent, ReportAgentSession,
};
pub use model::{
    Agent, AgentSession, AgentStatus, ApiError, ForegroundProcess, Pane, ProcessInfo, Scroll,
    Snapshot, Tab, Workspace,
};

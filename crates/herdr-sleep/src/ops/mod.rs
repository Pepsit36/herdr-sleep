//! Operations that change the state of panes.
//!
//! Every operation talks to the outside world through [`Env`], so its
//! sequence of steps can be tested with a scripted fake and a virtual clock.

mod env;
pub mod event;
pub mod keep_awake;
pub mod markers;
pub mod pane_lock;
pub mod registry;
pub mod replay;
pub mod restore;
pub mod sleep;
pub mod sweep;
pub mod wake;

pub use env::{Env, EnvResult, LiveEnv, PLACEHOLDER_BIN};
pub use event::{ActionContext, EventOutcome, FocusEvent, FocusKind, handle_focus_event};
pub use keep_awake::{KeepAwakeOutcome, keep_awake};
pub use markers::{MarkerReport, MarkersState, resync_markers, sync_markers};
pub use registry::{Registry, Sleeper};
pub use replay::{ReplayArgs, ReplayOutcome, replay};
pub use restore::{RestoreReport, restore, sleep_saved_session};
pub use sleep::{
    PlaceholderParams, SleepOutcome, placeholder_argv, sleep_agent, sleep_agent_waiting,
};
pub use sweep::{
    AgentClock, MAX_SLEEPS_PER_SWEEP, SweepReport, SweepState, is_focused, signature, sweep_now,
    sweep_once, sweep_with_mode,
};
pub use wake::{WakeOptions, WakeOutcome, wake_agent};

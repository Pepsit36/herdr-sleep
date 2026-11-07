//! Operations that change the state of panes.
//!
//! Every operation talks to the outside world through [`Env`], so its
//! sequence of steps can be tested with a scripted fake and a virtual clock.

mod env;
pub mod replay;
pub mod sleep;
pub mod wake;

pub use env::{Env, EnvResult, LiveEnv, PLACEHOLDER_BIN};
pub use replay::{ReplayArgs, ReplayOutcome, replay};
pub use sleep::{PlaceholderParams, SleepOutcome, placeholder_argv, sleep_agent};
pub use wake::{WakeOptions, WakeOutcome, wake_agent};

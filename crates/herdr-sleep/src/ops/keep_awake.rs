//! Keep a tab awake: the keep-awake action puts `keep_awake_icon` in front
//! of the label of the tab of a pane, and no agent of that tab goes to sleep
//! while it is there (the `tab-kept-awake` guard). Running it again removes
//! the icon.
//!
//! The label is the only state, see [`crate::tab_label`].
//!
//! Journal events: `KEEP-AWAKE`.

use crate::config::Config;
use crate::journal::Journal;
use crate::tab_label;

use super::Env;

/// What the keep-awake action did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepAwakeOutcome {
    /// The tab is now kept awake.
    Kept { label: String },
    /// The tab can sleep again.
    Released { label: String },
    /// The tab was already in the asked state.
    Unchanged { kept: bool, label: String },
}

/// Toggle the keep-awake icon on the tab of `target` (a pane id or an agent
/// name), or set it to `want` when that is given.
pub fn keep_awake(
    env: &dyn Env,
    config: &Config,
    target: &str,
    want: Option<bool>,
) -> Result<KeepAwakeOutcome, String> {
    if config.keep_awake_icon.is_empty() {
        return Err("keep_awake_icon is empty, which turns the feature off".into());
    }
    let tab_id = env
        .agent(target)
        .map(|agent| agent.tab_id)
        .or_else(|_| env.pane(target).map(|pane| pane.tab_id))
        .map_err(|error| format!("no pane or agent '{target}': {error}"))?;
    let snapshot = env
        .snapshot()
        .map_err(|error| format!("cannot read the session: {error}"))?;
    let tab = snapshot
        .tabs
        .iter()
        .find(|tab| tab.tab_id == tab_id)
        .ok_or_else(|| format!("tab {tab_id} not found"))?;
    let (sleep_icon, keep_icon) = (&config.sleep_icon, &config.keep_awake_icon);
    let kept = tab_label::keeps_awake(&tab.label, sleep_icon, keep_icon);
    let shown = |label: &str| label.to_string();
    if want == Some(kept) {
        return Ok(KeepAwakeOutcome::Unchanged {
            kept,
            label: shown(&tab.label),
        });
    }
    let label = tab_label::toggled(&tab.label, sleep_icon, keep_icon);
    env.tab_rename(&tab_id, &label)
        .map_err(|error| format!("cannot rename tab {tab_id}: {error}"))?;
    if let Some(dir) = env.state_dir() {
        let _ = std::fs::create_dir_all(&dir);
        Journal::new(&dir).log(
            "KEEP-AWAKE",
            &format!("{tab_id} {}", if kept { "off" } else { "on" }),
        );
    }
    Ok(if kept {
        KeepAwakeOutcome::Released { label }
    } else {
        KeepAwakeOutcome::Kept { label }
    })
}

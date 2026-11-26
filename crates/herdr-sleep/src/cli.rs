//! The commands: `sleep`, `wake`, `status`, `sweep`, `config`, `watch`,
//! `ensure-watcher`, `restore`, `event` and `replay`.
//!
//! Every failure prints `herdr-sleep: <message>` on stderr and exits 1; a
//! usage error exits 2.

mod config_cmd;
mod status;

use std::io::Write;
use std::process::ExitCode;

use herdr_sleep::config::Config;
use herdr_sleep::guards::{Refusal, SleepMode};
use herdr_sleep::herdr::Agent;
use herdr_sleep::herdr::HerdrClient;
use herdr_sleep::ops::{
    ActionContext, Env, FocusEvent, KeepAwakeOutcome, LiveEnv, ReplayArgs, ReplayOutcome,
    RestoreReport, SleepOutcome, SweepReport, SweepState, WakeOptions, WakeOutcome,
    handle_focus_event, replay, sleep_agent, sleep_agent_waiting, sleep_saved_session, sweep_now,
    sweep_once, wake_agent,
};
use herdr_sleep::watcher::{self, Ensure, WatchEnd};
use serde_json::json;

pub use config_cmd::run as config;
pub use status::run as status;

/// Print `text` and a newline on stdout. A closed pipe (`| head`) is not an
/// error worth a panic.
pub fn emit(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
}

/// Print `herdr-sleep: <message>` on stderr and give the failure exit code.
pub fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("herdr-sleep: {message}");
    ExitCode::from(1)
}

/// Print a usage error and give the usage exit code.
pub fn usage_error(message: &str) -> ExitCode {
    eprintln!("herdr-sleep: {message}");
    ExitCode::from(2)
}

/// The pane an action is about: the explicit `target`, else the one of the
/// action context in `lookup`. `None` when neither gives one.
pub fn resolve_target(
    target: Option<&str>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    match target {
        Some(target) => Some(target.to_string()),
        None => ActionContext::from_env(lookup).target().map(str::to_string),
    }
}

fn target_or_usage(command: &str, target: Option<&str>) -> Result<String, ExitCode> {
    resolve_target(target, |key| std::env::var(key).ok()).ok_or_else(|| {
        usage_error(&format!(
            "{command}: expects a pane or an agent name (none given and no Herdr action context)"
        ))
    })
}

/// Run `herdr-sleep sleep [<pane|name>] [--force] [--from-session]
/// [--wait[=SECONDS]] [--json]`.
pub fn sleep(
    target: Option<&str>,
    force: bool,
    from_session: bool,
    wait: Option<u64>,
    json: bool,
) -> ExitCode {
    // Started from a Herdr action (no target given): nobody reads stderr, so
    // a Sleep that did not happen is explained in a notification as well.
    let from_action = target.is_none();
    let target = match target_or_usage("sleep", target) {
        Ok(target) => target,
        Err(code) => return code,
    };
    watcher::ensure_watcher_logged();
    let client = HerdrClient::from_env();
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let env = LiveEnv::new(client.clone());
    let agent = client.agent_get(&target);
    let (label, outcome) = match agent {
        Ok(agent) if from_session => {
            return fail(format!(
                "{target} holds a live {} agent: use `sleep` without --from-session",
                agent.kind().unwrap_or("unknown")
            ));
        }
        Ok(agent) => {
            let mode = if force {
                SleepMode::Force
            } else {
                SleepMode::Manual
            };
            let outcome = match wait {
                Some(seconds) => sleep_agent_waiting(
                    &env,
                    &config,
                    &agent,
                    mode,
                    std::time::Duration::from_secs(seconds),
                ),
                None => sleep_agent(&env, &config, &agent, mode, None),
            };
            let label = agent.name.clone().unwrap_or_else(|| agent.pane_id.clone());
            if json {
                emit(&sleep_json(&agent, &outcome).to_string());
                return exit_code(&outcome);
            }
            (label, outcome)
        }
        Err(agent_error) => match client.pane_get(&target) {
            Ok(pane) if pane.agent_session.is_some() && !pane.has_agent() => {
                eprintln!(
                    "herdr-sleep: {target} has no live agent: resuming its saved session, then putting it to sleep"
                );
                let outcome = sleep_saved_session(&env, &config, &pane.pane_id);
                if json {
                    let shell = Agent {
                        pane_id: pane.pane_id.clone(),
                        name: pane.name.clone(),
                        ..Agent::default()
                    };
                    emit(&sleep_json(&shell, &outcome).to_string());
                    return exit_code(&outcome);
                }
                let label = pane.name.unwrap_or_else(|| pane.pane_id.clone());
                (label, outcome)
            }
            Ok(pane) => {
                return fail(format!(
                    "{target} is pane {} with no agent and no saved session: nothing to put to sleep",
                    pane.pane_id
                ));
            }
            Err(_) => {
                return fail(format!(
                    "no agent or pane '{target}' ({agent_error}); give a pane id or the name of a live agent"
                ));
            }
        },
    };
    report_sleep(&label, &outcome, wait.is_some());
    if from_action && let Some(text) = refusal_notice(&label, &outcome, wait.is_some()) {
        let _ = env.notify("Herdr Sleep", &text);
    }
    exit_code(&outcome)
}

/// The notification text for a Sleep that did not happen: what stderr says,
/// with what to try next. `None` when the pane fell asleep (or would, in a
/// dry run).
pub fn refusal_notice(label: &str, outcome: &SleepOutcome, waited: bool) -> Option<String> {
    if matches!(outcome, SleepOutcome::Asleep { .. } | SleepOutcome::DryRun) {
        return None;
    }
    let line = describe(label, outcome);
    Some(match sleep_hint(outcome, waited) {
        Some(hint) => format!("{line} ({hint})"),
        None => line,
    })
}

/// Print the outcome: success on stdout, a refusal or failure on stderr
/// with what to try next.
fn report_sleep(label: &str, outcome: &SleepOutcome, waited: bool) {
    let line = describe(label, outcome);
    match outcome {
        SleepOutcome::Asleep { .. } | SleepOutcome::DryRun => emit(&line),
        _ => {
            eprintln!("herdr-sleep: {line}");
            if let Some(hint) = sleep_hint(outcome, waited) {
                eprintln!("herdr-sleep: {hint}");
            }
        }
    }
}

/// What to try after a Sleep that did not happen.
pub fn sleep_hint(outcome: &SleepOutcome, waited: bool) -> Option<&'static str> {
    let SleepOutcome::Skipped(refusal) = outcome else {
        return None;
    };
    match refusal {
        Refusal::NotManaged => None,
        Refusal::NotSettled(_) if !waited => Some(
            "the agent is not idle yet: retry with --wait to keep trying, or use --force to skip the guards",
        ),
        Refusal::NoSession | Refusal::NoConversation | Refusal::PromptNotDetected if !waited => {
            Some(
                "the agent may have just started: retry with --wait, or use --force to skip the guards",
            )
        }
        _ => Some("use --force to skip the guards"),
    }
}

/// Run `herdr-sleep wake [<pane|name>] [--no-wait] [--json]`.
pub fn wake(target: Option<&str>, wait: bool, json: bool) -> ExitCode {
    let target = match target_or_usage("wake", target) {
        Ok(target) => target,
        Err(code) => return code,
    };
    watcher::ensure_watcher_logged();
    let client = HerdrClient::from_env();
    let agent = client.agent_get(&target).ok();
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let pane = agent
        .as_ref()
        .map_or(target.as_str(), |a| a.pane_id.as_str());
    let outcome = wake_agent(&LiveEnv::new(client), &config, pane, WakeOptions { wait });
    if json {
        emit(&wake_json(&target, agent.as_ref(), &outcome));
    } else {
        let label = agent
            .as_ref()
            .map_or(target.as_str(), |a| a.name.as_deref().unwrap_or(&a.pane_id));
        emit(&describe_wake(label, &outcome, wait));
    }
    wake_exit_code(&outcome)
}

/// Run `herdr-sleep keep-awake [<pane|name>] [--on|--off]`. From a Herdr
/// action (no target given) the result is also a notification, since nobody
/// reads the output.
pub fn keep_awake(target: Option<&str>, state: Option<bool>) -> ExitCode {
    let from_action = target.is_none();
    let target = match target_or_usage("keep-awake", target) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let env = LiveEnv::new(HerdrClient::from_env());
    let line = match herdr_sleep::ops::keep_awake(&env, &config, &target, state) {
        Ok(outcome) => describe_keep_awake(&outcome),
        Err(error) => {
            if from_action {
                let _ = env.notify("Herdr Sleep", &error);
            }
            return fail(error);
        }
    };
    emit(&line);
    if from_action {
        let _ = env.notify("Herdr Sleep", &line);
    }
    ExitCode::SUCCESS
}

/// One plain-text line for what the keep-awake action did.
pub fn describe_keep_awake(outcome: &KeepAwakeOutcome) -> String {
    match outcome {
        KeepAwakeOutcome::Kept { label } => format!("tab {label} is kept awake"),
        KeepAwakeOutcome::Released { label } => format!("tab {label} can sleep again"),
        KeepAwakeOutcome::Unchanged { kept: true, label } => {
            format!("tab {label} was already kept awake")
        }
        KeepAwakeOutcome::Unchanged { kept: false, label } => {
            format!("tab {label} could already sleep")
        }
    }
}

/// Run `herdr-sleep event`, the command of the focus hooks. Silent and always
/// successful: a hook must never fail loudly, the journal holds the outcome.
pub fn event() -> ExitCode {
    watcher::ensure_watcher_logged();
    let event = FocusEvent::from_env(|key| std::env::var(key).ok());
    let config = Config::load().unwrap_or_else(|error| {
        eprintln!("herdr-sleep: {error}");
        Config::default()
    });
    let env = LiveEnv::new(HerdrClient::from_env());
    handle_focus_event(&env, &config, &event);
    ExitCode::SUCCESS
}

/// Run `herdr-sleep restore [--json]`, the command of the startup hook: put
/// the Placeholder back in the panes that slept before a Herdr restart, then
/// make sure the Watcher runs. Always successful: a hook must never fail
/// loudly, the journal holds the outcome.
pub fn restore(json: bool) -> ExitCode {
    let env = LiveEnv::new(HerdrClient::from_env());
    // Not the defaults: a broken configuration could hide `dry_run` or the
    // exclusions, and the restore starts agents.
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("herdr-sleep: {error}");
            if let Some(dir) = herdr_sleep::ops::Env::state_dir(&env) {
                let _ = std::fs::create_dir_all(&dir);
                herdr_sleep::journal::Journal::new(&dir).log(
                    "WARN",
                    &format!("restore: {error}; nothing is restored until the configuration loads"),
                );
            }
            watcher::ensure_watcher_logged();
            return ExitCode::SUCCESS;
        }
    };
    let report = herdr_sleep::ops::restore(&env, &config);
    watcher::ensure_watcher_logged();
    if json {
        match serde_json::to_string(&report) {
            Ok(text) => emit(&text),
            Err(error) => eprintln!("herdr-sleep: cannot print the report: {error}"),
        }
    } else {
        for line in describe_restore(&report) {
            emit(&line);
        }
    }
    ExitCode::SUCCESS
}

/// One line per restored or skipped pane, or a single line when there was
/// nothing to do.
pub fn describe_restore(report: &RestoreReport) -> Vec<String> {
    if let Some(error) = &report.error {
        return vec![format!("restore failed: {error}")];
    }
    let mut lines: Vec<String> = report
        .restored
        .iter()
        .map(|pane| format!("{pane} is asleep again"))
        .collect();
    lines.extend(
        report
            .skipped
            .iter()
            .map(|(pane, reason)| format!("{pane} left alone: {reason}")),
    );
    lines.extend(report.missing_cwd.iter().map(|(pane, cwd)| {
        let cwd = if cwd.is_empty() { "unknown" } else { cwd };
        format!("{pane} ignored: its working directory is gone ({cwd})")
    }));
    if lines.is_empty() {
        lines.push("no sleeping pane to restore".to_string());
    } else {
        lines.push(format!(
            "restore done: {} restored, {} left alone, {} with a missing working directory",
            report.restored.len(),
            report.skipped.len(),
            report.missing_cwd.len()
        ));
    }
    lines
}

/// Run `herdr-sleep sweep [--now] [--json]`: one Sweep in the foreground.
pub fn sweep(now: bool, json: bool) -> ExitCode {
    watcher::ensure_watcher_logged();
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let env = LiveEnv::new(HerdrClient::from_env());
    let mut state = herdr_sleep::ops::Env::state_dir(&env)
        .map(|dir| SweepState::load(&dir))
        .unwrap_or_default();
    let report = if now {
        sweep_now(&env, &config, &mut state)
    } else {
        sweep_once(&env, &config, &mut state)
    };
    if json {
        match serde_json::to_string(&report) {
            Ok(text) => emit(&text),
            Err(error) => return fail(format!("cannot print the report: {error}")),
        }
    } else {
        for line in describe_sweep(&report) {
            emit(&line);
        }
    }
    if report.error.is_some() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// One line per decision of a Sweep, or a single line when there was none.
pub fn describe_sweep(report: &SweepReport) -> Vec<String> {
    if let Some(error) = &report.error {
        return vec![format!("sweep failed: {error}")];
    }
    let mut lines = Vec::new();
    lines.extend(report.slept.iter().map(|pane| format!("{pane} is asleep")));
    lines.extend(
        report
            .dry_run
            .iter()
            .map(|pane| format!("{pane} would be put to sleep (dry run)")),
    );
    lines.extend(
        report
            .refused
            .iter()
            .map(|(pane, code)| format!("{pane} stays awake: {code}")),
    );
    lines.extend(
        report
            .aborted
            .iter()
            .map(|(pane, reason)| format!("{pane} stays awake: {reason}")),
    );
    if report.deferred > 0 {
        lines.push(format!(
            "{} more due, left for the next pass",
            report.deferred
        ));
    }
    if lines.is_empty() {
        lines.push(format!(
            "nothing to put to sleep ({} agents considered)",
            report.considered
        ));
    }
    lines
}

/// Run `herdr-sleep watch [--detach]`.
pub fn watch(detach: bool) -> ExitCode {
    if detach {
        return ensure_watcher(false);
    }
    match watcher::watch() {
        WatchEnd::AlreadyRunning | WatchEnd::Exited(_) => ExitCode::SUCCESS,
        WatchEnd::Failed(reason) => fail(format!("watcher: {reason}")),
    }
}

/// Run `herdr-sleep ensure-watcher [--json]`.
pub fn ensure_watcher(json: bool) -> ExitCode {
    let ensure = watcher::ensure_watcher();
    if json {
        emit(&ensure_json(&ensure).to_string());
    }
    match ensure {
        Ensure::AlreadyRunning => {
            if !json {
                emit("watcher is already running");
            }
            ExitCode::SUCCESS
        }
        Ensure::Started => {
            if !json {
                emit("watcher started");
            }
            ExitCode::SUCCESS
        }
        Ensure::Failed(_) if json => ExitCode::from(1),
        Ensure::Failed(reason) => fail(format!("watcher: {reason}")),
    }
}

/// The `--json` object of `ensure-watcher`: `running`, `started` and `error`
/// (`null` on success).
pub fn ensure_json(ensure: &Ensure) -> serde_json::Value {
    match ensure {
        Ensure::AlreadyRunning => json!({"running": true, "started": false, "error": null}),
        Ensure::Started => json!({"running": true, "started": true, "error": null}),
        Ensure::Failed(reason) => json!({"running": false, "started": false, "error": reason}),
    }
}

/// One plain-text line for a wake outcome.
pub fn describe_wake(label: &str, outcome: &WakeOutcome, waited: bool) -> String {
    match outcome {
        WakeOutcome::Woken { .. } if !waited => format!("{label} is waking up"),
        WakeOutcome::Woken { ms } => format!("{label} is awake and ready ({ms} ms)"),
        WakeOutcome::NotFound => format!("{label}: no such agent"),
        WakeOutcome::NotSleeping => format!("{label} is not asleep"),
        WakeOutcome::Deduplicated => format!("{label} was woken a moment ago"),
        WakeOutcome::TimedOut => format!("{label} was not ready after 120 s"),
        WakeOutcome::Failed(reason) => format!("{label} could not be woken: {reason}"),
    }
}

/// The `--json` object of a wake: `woke`, `pane`, `name`, `ms` and `reason`
/// (`null` when the agent was woken).
pub fn wake_json(target: &str, agent: Option<&Agent>, outcome: &WakeOutcome) -> String {
    let (woke, ms, reason) = match outcome {
        WakeOutcome::Woken { ms } => (true, Some(*ms), None),
        WakeOutcome::NotFound => (false, None, Some("not-found".to_string())),
        WakeOutcome::NotSleeping => (false, None, Some("not-sleeping".to_string())),
        WakeOutcome::Deduplicated => (false, None, Some("deduplicated".to_string())),
        WakeOutcome::TimedOut => (false, None, Some("timeout".to_string())),
        WakeOutcome::Failed(reason) => (false, None, Some(reason.clone())),
    };
    json!({
        "woke": woke,
        "pane": agent.map_or(target, |a| a.pane_id.as_str()),
        "name": agent.and_then(|a| a.name.as_deref()),
        "ms": ms,
        "reason": reason,
    })
    .to_string()
}

/// The `--json` object of a sleep: `pane`, `name`, `outcome` (`asleep`,
/// `refused`, `dry-run`, `aborted` or `warned`), `slept`, `freed_kb` and
/// `reason` (the refusal code or the explanation; `null` otherwise).
pub fn sleep_json(agent: &Agent, outcome: &SleepOutcome) -> serde_json::Value {
    let (code, freed_kb, reason) = match outcome {
        SleepOutcome::Asleep { freed_kb } => ("asleep", Some(*freed_kb), None),
        SleepOutcome::Skipped(refusal) => ("refused", None, Some(refusal.code().to_string())),
        SleepOutcome::DryRun => ("dry-run", None, None),
        SleepOutcome::Aborted(reason) => ("aborted", None, Some(reason.clone())),
        SleepOutcome::Warned(reason) => ("warned", None, Some(reason.clone())),
    };
    json!({
        "pane": agent.pane_id,
        "name": agent.name,
        "outcome": code,
        "slept": matches!(outcome, SleepOutcome::Asleep { .. }),
        "freed_kb": freed_kb,
        "reason": reason,
    })
}

/// 0 woken, not asleep or de-duplicated, 1 not found or failed, 2 timed out.
pub fn wake_exit_code(outcome: &WakeOutcome) -> ExitCode {
    match outcome {
        WakeOutcome::Woken { .. } | WakeOutcome::NotSleeping | WakeOutcome::Deduplicated => {
            ExitCode::SUCCESS
        }
        WakeOutcome::NotFound | WakeOutcome::Failed(_) => ExitCode::from(1),
        WakeOutcome::TimedOut => ExitCode::from(2),
    }
}

/// Run `herdr-sleep replay --pane P [--name N] --kind K --file F`, started by
/// the Placeholder. Silent: the journal holds the outcome.
pub fn replay_command(args: &ReplayArgs) -> ExitCode {
    let env = LiveEnv::new(HerdrClient::from_env());
    // The text the user typed is replayed whatever the configuration says: a
    // typo in it must not lose it. The defaults are harmless here, a Replay
    // only types into an Agent that just woke.
    let config = Config::load().unwrap_or_else(|error| {
        if let Some(dir) = herdr_sleep::ops::Env::state_dir(&env) {
            let _ = std::fs::create_dir_all(&dir);
            herdr_sleep::journal::Journal::new(&dir).log(
                "WARN",
                &format!("replay: {error}; using the default configuration"),
            );
        }
        Config::default()
    });
    match replay(&env, &config, args) {
        ReplayOutcome::Replayed { .. } => ExitCode::SUCCESS,
        ReplayOutcome::TimedOut => ExitCode::from(1),
        ReplayOutcome::Failed(reason) => fail(format!("replay failed: {reason}")),
    }
}

/// One plain-text line for an outcome.
pub fn describe(label: &str, outcome: &SleepOutcome) -> String {
    match outcome {
        SleepOutcome::Asleep { freed_kb: 0 } => format!("{label} is asleep"),
        SleepOutcome::Asleep { freed_kb } => {
            format!("{label} is asleep, freed {} MB", freed_kb / 1024)
        }
        SleepOutcome::Skipped(refusal) => format!("{label} was not put to sleep: {refusal}"),
        SleepOutcome::DryRun => format!("{label} would be put to sleep (dry run)"),
        SleepOutcome::Aborted(reason) => format!("{label} stays awake: {reason}"),
        SleepOutcome::Warned(reason) => format!("{label}: {reason}"),
    }
}

/// 0 asleep or dry run, 1 refused, aborted or warned.
pub fn exit_code(outcome: &SleepOutcome) -> ExitCode {
    match outcome {
        SleepOutcome::Asleep { .. } | SleepOutcome::DryRun => ExitCode::SUCCESS,
        SleepOutcome::Skipped(_) | SleepOutcome::Aborted(_) | SleepOutcome::Warned(_) => {
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_sleep::guards::Refusal;

    #[test]
    fn outcomes_are_one_line() {
        assert_eq!(
            describe("worker", &SleepOutcome::Asleep { freed_kb: 2048 }),
            "worker is asleep, freed 2 MB"
        );
        assert_eq!(
            describe("worker", &SleepOutcome::Asleep { freed_kb: 0 }),
            "worker is asleep"
        );
        assert_eq!(
            describe("worker", &SleepOutcome::Skipped(Refusal::Focused)),
            "worker was not put to sleep: agent is focused"
        );
        assert_eq!(
            describe("worker", &SleepOutcome::DryRun),
            "worker would be put to sleep (dry run)"
        );
    }

    #[test]
    fn a_refused_sleep_is_explained_in_a_notification() {
        let draft = SleepOutcome::Skipped(Refusal::DraftInPrompt);
        let text = refusal_notice("worker", &draft, false).unwrap();
        assert!(text.starts_with("worker was not put to sleep: "), "{text}");
        assert!(text.contains("use --force to skip the guards"), "{text}");
        let aborted = SleepOutcome::Aborted("no placeholder".into());
        assert!(
            refusal_notice("worker", &aborted, false)
                .unwrap()
                .starts_with("worker stays awake: no placeholder")
        );
        let unmanaged = SleepOutcome::Skipped(Refusal::NotManaged);
        assert_eq!(
            refusal_notice("worker", &unmanaged, false).unwrap(),
            "worker was not put to sleep: agent kind is not managed"
        );
    }

    #[test]
    fn a_successful_sleep_has_no_refusal_notice() {
        for outcome in [SleepOutcome::Asleep { freed_kb: 1 }, SleepOutcome::DryRun] {
            assert_eq!(refusal_notice("worker", &outcome, false), None);
        }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&SleepOutcome::DryRun), ExitCode::SUCCESS);
        assert_eq!(
            exit_code(&SleepOutcome::Asleep { freed_kb: 1 }),
            ExitCode::SUCCESS
        );
        assert_eq!(
            exit_code(&SleepOutcome::Aborted("x".into())),
            ExitCode::from(1)
        );
        assert_eq!(
            exit_code(&SleepOutcome::Warned("x".into())),
            ExitCode::from(1)
        );
        assert_eq!(
            exit_code(&SleepOutcome::Skipped(Refusal::Focused)),
            ExitCode::from(1)
        );
    }

    #[test]
    fn wake_lines_and_exit_codes() {
        assert_eq!(
            describe_wake("worker", &WakeOutcome::Woken { ms: 0 }, false),
            "worker is waking up"
        );
        assert_eq!(
            describe_wake("worker", &WakeOutcome::Woken { ms: 4200 }, true),
            "worker is awake and ready (4200 ms)"
        );
        assert_eq!(
            describe_wake("worker", &WakeOutcome::NotSleeping, true),
            "worker is not asleep"
        );
        assert_eq!(
            wake_exit_code(&WakeOutcome::Deduplicated),
            ExitCode::SUCCESS
        );
        assert_eq!(wake_exit_code(&WakeOutcome::NotSleeping), ExitCode::SUCCESS);
        assert_eq!(wake_exit_code(&WakeOutcome::NotFound), ExitCode::from(1));
        assert_eq!(wake_exit_code(&WakeOutcome::TimedOut), ExitCode::from(2));
    }

    #[test]
    fn hints_say_what_to_try_next() {
        let busy = SleepOutcome::Skipped(Refusal::NotSettled(
            herdr_sleep::herdr::AgentStatus::Working,
        ));
        assert!(sleep_hint(&busy, false).unwrap().contains("--wait"));
        assert!(!sleep_hint(&busy, true).unwrap().contains("--wait"));
        assert!(sleep_hint(&busy, true).unwrap().contains("--force"));
        let draft = SleepOutcome::Skipped(Refusal::DraftInPrompt);
        assert_eq!(
            sleep_hint(&draft, false),
            Some("use --force to skip the guards")
        );
        assert_eq!(
            sleep_hint(&SleepOutcome::Skipped(Refusal::NotManaged), false),
            None
        );
        assert_eq!(sleep_hint(&SleepOutcome::Aborted("x".into()), false), None);
    }

    #[test]
    fn restore_lines() {
        let report = RestoreReport {
            restored: vec!["p1".into()],
            skipped: vec![("p2".into(), "pane-busy".into())],
            missing_cwd: vec![("p3".into(), "/gone".into())],
            error: None,
        };
        assert_eq!(
            describe_restore(&report),
            [
                "p1 is asleep again",
                "p2 left alone: pane-busy",
                "p3 ignored: its working directory is gone (/gone)",
                "restore done: 1 restored, 1 left alone, 1 with a missing working directory"
            ]
        );
        assert_eq!(
            describe_restore(&RestoreReport::default()),
            ["no sleeping pane to restore"]
        );
        let failed = RestoreReport {
            error: Some("socket gone".into()),
            ..RestoreReport::default()
        };
        assert_eq!(describe_restore(&failed), ["restore failed: socket gone"]);
    }

    #[test]
    fn sweep_lines() {
        let report = SweepReport {
            considered: 3,
            slept: vec!["p1".into()],
            refused: vec![("p2".into(), "focused".into())],
            deferred: 2,
            ..SweepReport::default()
        };
        assert_eq!(
            describe_sweep(&report),
            [
                "p1 is asleep",
                "p2 stays awake: focused",
                "2 more due, left for the next pass"
            ]
        );
        assert_eq!(
            describe_sweep(&SweepReport {
                considered: 4,
                ..SweepReport::default()
            }),
            ["nothing to put to sleep (4 agents considered)"]
        );
    }

    #[test]
    fn wake_json_shape() {
        let agent = Agent {
            pane_id: "w1:p2".into(),
            name: Some("worker".into()),
            ..Agent::default()
        };
        let parse = |text: String| serde_json::from_str::<serde_json::Value>(&text).unwrap();
        assert_eq!(
            parse(wake_json(
                "worker",
                Some(&agent),
                &WakeOutcome::Woken { ms: 12 }
            )),
            json!({"woke": true, "pane": "w1:p2", "name": "worker", "ms": 12, "reason": null})
        );
        assert_eq!(
            parse(wake_json("nope", None, &WakeOutcome::NotFound)),
            json!({"woke": false, "pane": "nope", "name": null, "ms": null, "reason": "not-found"})
        );
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn an_explicit_target_wins_over_the_context() {
        let env = [("HERDR_PANE_ID", "w1:p9")];
        assert_eq!(
            resolve_target(Some("worker"), lookup(&env)).as_deref(),
            Some("worker")
        );
    }

    #[test]
    fn the_context_fallback_order() {
        let json = r#"{"pane_id":"w1:p1","focused_pane_id":"w1:p2"}"#;
        let all = [
            ("HERDR_PLUGIN_CONTEXT_JSON", json),
            ("HERDR_PANE_ID", "w1:p3"),
        ];
        assert_eq!(resolve_target(None, lookup(&all)).as_deref(), Some("w1:p1"));
        let focused = [
            (
                "HERDR_PLUGIN_CONTEXT_JSON",
                r#"{"focused_pane_id":"w1:p2"}"#,
            ),
            ("HERDR_PANE_ID", "w1:p3"),
        ];
        assert_eq!(
            resolve_target(None, lookup(&focused)).as_deref(),
            Some("w1:p2")
        );
        let env_only = [("HERDR_PANE_ID", "w1:p3")];
        assert_eq!(
            resolve_target(None, lookup(&env_only)).as_deref(),
            Some("w1:p3")
        );
        assert_eq!(resolve_target(None, lookup(&[])), None);
    }

    #[test]
    fn sleep_json_shape() {
        let agent = Agent {
            pane_id: "w1:p2".into(),
            name: Some("worker".into()),
            ..Agent::default()
        };
        assert_eq!(
            sleep_json(&agent, &SleepOutcome::Asleep { freed_kb: 2048 }),
            json!({"pane": "w1:p2", "name": "worker", "outcome": "asleep", "slept": true,
                   "freed_kb": 2048, "reason": null})
        );
        assert_eq!(
            sleep_json(&agent, &SleepOutcome::Skipped(Refusal::Focused)),
            json!({"pane": "w1:p2", "name": "worker", "outcome": "refused", "slept": false,
                   "freed_kb": null, "reason": "focused"})
        );
        assert_eq!(
            sleep_json(&agent, &SleepOutcome::DryRun)["outcome"],
            "dry-run"
        );
        assert_eq!(
            sleep_json(&agent, &SleepOutcome::Aborted("x".into()))["reason"],
            "x"
        );
    }

    #[test]
    fn ensure_json_shape() {
        assert_eq!(
            ensure_json(&Ensure::AlreadyRunning),
            json!({"running": true, "started": false, "error": null})
        );
        assert_eq!(
            ensure_json(&Ensure::Started),
            json!({"running": true, "started": true, "error": null})
        );
        assert_eq!(
            ensure_json(&Ensure::Failed("no".into())),
            json!({"running": false, "started": false, "error": "no"})
        );
    }

    #[test]
    fn sweep_json_keys() {
        let value = serde_json::to_value(SweepReport::default()).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "aborted",
                "considered",
                "deferred",
                "dry_run",
                "error",
                "refused",
                "slept"
            ]
        );
    }
}

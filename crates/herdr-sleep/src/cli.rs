//! The `sleep`, `wake` and `replay` commands.

use std::process::ExitCode;

use herdr_sleep::config::Config;
use herdr_sleep::guards::SleepMode;
use herdr_sleep::herdr::Agent;
use herdr_sleep::herdr::HerdrClient;
use herdr_sleep::ops::{
    LiveEnv, ReplayArgs, ReplayOutcome, SleepOutcome, WakeOptions, WakeOutcome, replay,
    sleep_agent, wake_agent,
};
use serde_json::json;

/// Run `herdr-sleep sleep <pane|name> [--force]`.
pub fn sleep(target: &str, force: bool) -> ExitCode {
    let client = HerdrClient::from_env();
    let agent = match client.agent_get(target) {
        Ok(agent) => agent,
        Err(error) => {
            eprintln!("herdr-sleep: cannot find agent '{target}': {error}");
            return ExitCode::from(1);
        }
    };
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("herdr-sleep: {error}");
            return ExitCode::from(1);
        }
    };
    let mode = if force {
        SleepMode::Force
    } else {
        SleepMode::Manual
    };
    let outcome = sleep_agent(&LiveEnv::new(client), &config, &agent, mode, None);
    let label = agent.name.as_deref().unwrap_or(&agent.pane_id);
    println!("{}", describe(label, &outcome));
    exit_code(&outcome)
}

/// Run `herdr-sleep wake <pane|name> [--no-wait] [--json]`.
pub fn wake(target: &str, wait: bool, json: bool) -> ExitCode {
    let client = HerdrClient::from_env();
    let agent = client.agent_get(target).ok();
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("herdr-sleep: {error}");
            return ExitCode::from(1);
        }
    };
    let pane = agent.as_ref().map_or(target, |a| a.pane_id.as_str());
    let outcome = wake_agent(&LiveEnv::new(client), &config, pane, WakeOptions { wait });
    if json {
        println!("{}", wake_json(target, agent.as_ref(), &outcome));
    } else {
        let label = agent
            .as_ref()
            .map_or(target, |a| a.name.as_deref().unwrap_or(&a.pane_id));
        println!("{}", describe_wake(label, &outcome, wait));
    }
    wake_exit_code(&outcome)
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
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("herdr-sleep: {error}");
            return ExitCode::from(1);
        }
    };
    let env = LiveEnv::new(HerdrClient::from_env());
    match replay(&env, &config, args) {
        ReplayOutcome::Replayed { .. } => ExitCode::SUCCESS,
        ReplayOutcome::TimedOut => ExitCode::from(1),
        ReplayOutcome::Failed(reason) => {
            eprintln!("herdr-sleep: replay failed: {reason}");
            ExitCode::from(1)
        }
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
}

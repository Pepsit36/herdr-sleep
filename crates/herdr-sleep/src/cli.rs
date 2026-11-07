//! The `sleep` command: put one agent to sleep from the command line.

use std::process::ExitCode;

use herdr_sleep::config::Config;
use herdr_sleep::guards::SleepMode;
use herdr_sleep::herdr::HerdrClient;
use herdr_sleep::ops::{LiveEnv, SleepOutcome, sleep_agent};

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
}

//! The `doctor` command: what keeps the plugin from working, one line each.
//!
//! [`gather`] reads the machine into [`Facts`]; [`evaluate`] turns the facts
//! into checks without touching anything, so the verdicts are testable.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use herdr_sleep::config::Config;
use herdr_sleep::herdr::HerdrClient;
use herdr_sleep::ops::{Env, LiveEnv};
use herdr_sleep::procs::{self, ProcessTable};
use herdr_sleep::watcher;
use serde::Serialize;

use super::{emit, fail};

/// The lowest Herdr the plugin supports, the one of the manifest.
const MIN_HERDR_VERSION: &str = "0.9.2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Ok,
    Warn,
    Fail,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

/// What [`gather`] found out about the machine.
#[derive(Debug)]
pub struct Facts {
    pub herdr_bin: PathBuf,
    /// Output of `herdr --version`, `None` when it could not run.
    pub herdr_version: Option<String>,
    /// Whether `herdr` answered a request for the session.
    pub herdr_answers: bool,
    pub config_error: Option<String>,
    pub config_warnings: Vec<String>,
    /// The state directory and whether a file can be written in it.
    pub state_dir: Option<(PathBuf, bool)>,
    pub watcher_running: bool,
    pub placeholder: Result<PathBuf, String>,
    pub main_bin: PathBuf,
    pub main_bin_exists: bool,
    pub process_table: bool,
    /// Programs of the profiles that are not on the `PATH`, as `kind (bin)`.
    pub missing_agents: Vec<String>,
}

impl Default for Facts {
    fn default() -> Self {
        Facts {
            herdr_bin: PathBuf::new(),
            herdr_version: None,
            herdr_answers: false,
            config_error: None,
            config_warnings: Vec::new(),
            state_dir: None,
            watcher_running: false,
            placeholder: Err("not found".into()),
            main_bin: PathBuf::new(),
            main_bin_exists: false,
            process_table: false,
            missing_agents: Vec::new(),
        }
    }
}

/// `0.9.2` from `herdr 0.9.2` or `0.9.2`: three numbers, missing ones are zero.
pub fn parse_version(text: &str) -> Option<[u64; 3]> {
    let word = text
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = word.split(['.', '-', '+']);
    let mut out = [0; 3];
    for slot in &mut out {
        match parts.next().map(str::parse) {
            Some(Ok(n)) => *slot = n,
            _ => break,
        }
    }
    Some(out)
}

fn check(name: &'static str, verdict: Verdict, detail: impl Into<String>) -> Check {
    Check {
        name,
        verdict,
        detail: detail.into(),
    }
}

/// The verdicts for `facts`.
pub fn evaluate(facts: &Facts) -> Vec<Check> {
    let mut out = Vec::new();

    out.push(match &facts.herdr_version {
        None => check(
            "herdr",
            Verdict::Fail,
            format!("cannot run {}", facts.herdr_bin.display()),
        ),
        Some(text) => match (parse_version(text), parse_version(MIN_HERDR_VERSION)) {
            (Some(found), Some(least)) if found < least => check(
                "herdr",
                Verdict::Fail,
                format!("{text} is older than {MIN_HERDR_VERSION}, the lowest supported"),
            ),
            _ => check("herdr", Verdict::Ok, text.trim()),
        },
    });
    if facts.herdr_version.is_some() {
        out.push(if facts.herdr_answers {
            check("session", Verdict::Ok, "herdr answers")
        } else {
            check(
                "session",
                Verdict::Fail,
                "herdr does not answer: is a server running (or HERDR_SOCKET_PATH set)?",
            )
        });
    }

    out.push(match &facts.config_error {
        Some(error) => check("config", Verdict::Fail, error.clone()),
        None if facts.config_warnings.is_empty() => check("config", Verdict::Ok, "valid"),
        None => check("config", Verdict::Warn, facts.config_warnings.join("; ")),
    });

    out.push(match &facts.state_dir {
        None => check(
            "state",
            Verdict::Fail,
            "no state directory (HOME is not set?)",
        ),
        Some((dir, true)) => check("state", Verdict::Ok, dir.display().to_string()),
        Some((dir, false)) => check(
            "state",
            Verdict::Fail,
            format!("cannot write in {}", dir.display()),
        ),
    });

    out.push(if facts.watcher_running {
        check("watcher", Verdict::Ok, "running")
    } else {
        check(
            "watcher",
            Verdict::Warn,
            "not running: nothing sleeps by itself (run `herdr-sleep ensure-watcher`)",
        )
    });

    out.push(match &facts.placeholder {
        Ok(path) => check("placeholder", Verdict::Ok, path.display().to_string()),
        Err(error) => check("placeholder", Verdict::Fail, error.clone()),
    });
    out.push(if facts.main_bin_exists {
        check("binary", Verdict::Ok, facts.main_bin.display().to_string())
    } else {
        check(
            "binary",
            Verdict::Fail,
            format!(
                "{} is gone: the plugin was replaced while running, restart the watcher",
                facts.main_bin.display()
            ),
        )
    });
    out.push(if facts.process_table {
        check("ps", Verdict::Ok, "the process table can be read")
    } else {
        check(
            "ps",
            Verdict::Fail,
            "`ps -axo pid=,ppid=,rss=,command=` does not work: no agent can be checked",
        )
    });
    out.push(if facts.missing_agents.is_empty() {
        check("agents", Verdict::Ok, "every agent program is on the PATH")
    } else {
        check(
            "agents",
            Verdict::Warn,
            format!(
                "not on the PATH of this shell: {} (fine when the pane's shell finds them)",
                facts.missing_agents.join(", ")
            ),
        )
    });
    out
}

/// Whether a file can be created in `dir`.
fn writable(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok() && {
        let probe = dir.join(format!("doctor-{}.tmp", std::process::id()));
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    }
}

fn gather() -> Facts {
    let client = HerdrClient::from_env();
    let env = LiveEnv::new(HerdrClient::from_env());
    let herdr_bin = client.binary().to_path_buf();
    let herdr_version = std::process::Command::new(&herdr_bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    let (config, config_error) = match Config::load() {
        Ok(config) => (Some(config), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let state_dir = env.state_dir();
    let main_bin = env.main_bin();
    Facts {
        herdr_answers: herdr_version.is_some() && client.snapshot().is_ok(),
        herdr_bin,
        herdr_version,
        config_warnings: config
            .as_ref()
            .map(|c| c.warnings.clone())
            .unwrap_or_default(),
        config_error,
        watcher_running: state_dir
            .as_deref()
            .and_then(|dir| watcher::watcher_info(dir).ok())
            .is_some_and(|info| info.running),
        state_dir: state_dir.map(|dir| {
            let ok = writable(&dir);
            (dir, ok)
        }),
        placeholder: env.placeholder_bin().map_err(|e| e.to_string()),
        main_bin_exists: main_bin.is_file(),
        main_bin,
        process_table: ProcessTable::load().is_ok_and(|table| !table.entries().is_empty()),
        missing_agents: config
            .map(|config| {
                config
                    .profiles()
                    .filter(|profile| procs::resolve_program(&profile.bin).is_none())
                    .map(|profile| format!("{} ({})", profile.kind, profile.bin))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The text output: one line per check.
pub fn render(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            let mark = match c.verdict {
                Verdict::Ok => "ok  ",
                Verdict::Warn => "warn",
                Verdict::Fail => "FAIL",
            };
            format!("{mark} {:<11} {}", c.name, c.detail)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run `herdr-sleep doctor [--json]`. Exit code 1 when a check fails.
pub fn run(json: bool) -> ExitCode {
    let checks = evaluate(&gather());
    let failed = checks.iter().any(|c| c.verdict == Verdict::Fail);
    if json {
        match serde_json::to_string(&serde_json::json!({ "ok": !failed, "checks": checks })) {
            Ok(text) => emit(&text),
            Err(error) => return fail(format!("cannot print the report: {error}")),
        }
    } else {
        emit(&render(&checks));
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Facts {
        Facts {
            herdr_bin: "/usr/bin/herdr".into(),
            herdr_version: Some("herdr 0.9.5".into()),
            herdr_answers: true,
            state_dir: Some(("/state".into(), true)),
            watcher_running: true,
            placeholder: Ok("/opt/herdr-sleep-ph".into()),
            main_bin: "/opt/herdr-sleep".into(),
            main_bin_exists: true,
            process_table: true,
            ..Facts::default()
        }
    }

    fn verdicts(facts: &Facts) -> Vec<(&'static str, Verdict)> {
        evaluate(facts)
            .iter()
            .map(|c| (c.name, c.verdict))
            .collect()
    }

    #[test]
    fn a_healthy_machine_has_no_warning_and_no_failure() {
        assert!(
            evaluate(&healthy())
                .iter()
                .all(|c| c.verdict == Verdict::Ok)
        );
    }

    #[test]
    fn versions_are_compared_by_number() {
        assert_eq!(parse_version("herdr 0.9.5"), Some([0, 9, 5]));
        assert_eq!(parse_version("0.10.0-rc1"), Some([0, 10, 0]));
        assert_eq!(parse_version("nothing"), None);
        let mut facts = healthy();
        facts.herdr_version = Some("herdr 0.9.1".into());
        assert!(verdicts(&facts).contains(&("herdr", Verdict::Fail)));
        facts.herdr_version = Some("herdr 0.10.0".into());
        assert!(verdicts(&facts).contains(&("herdr", Verdict::Ok)));
    }

    #[test]
    fn what_stops_the_plugin_fails_and_what_only_limits_it_warns() {
        let mut facts = healthy();
        facts.watcher_running = false;
        facts.missing_agents = vec!["cursor (cursor-agent)".into()];
        facts.config_warnings = vec!["agents.x is not managed".into()];
        let got = verdicts(&facts);
        assert!(got.contains(&("watcher", Verdict::Warn)));
        assert!(got.contains(&("agents", Verdict::Warn)));
        assert!(got.contains(&("config", Verdict::Warn)));
        assert!(!got.iter().any(|(_, v)| *v == Verdict::Fail));

        facts.main_bin_exists = false;
        facts.process_table = false;
        facts.placeholder = Err("missing".into());
        facts.state_dir = Some(("/state".into(), false));
        facts.config_error = Some("bad".into());
        let failed: Vec<_> = verdicts(&facts)
            .into_iter()
            .filter(|(_, v)| *v == Verdict::Fail)
            .map(|(n, _)| n)
            .collect();
        assert_eq!(failed, ["config", "state", "placeholder", "binary", "ps"]);
    }

    #[test]
    fn an_unreachable_herdr_skips_the_session_check() {
        let mut facts = healthy();
        facts.herdr_version = None;
        let got = verdicts(&facts);
        assert!(got.contains(&("herdr", Verdict::Fail)));
        assert!(!got.iter().any(|(n, _)| *n == "session"));
    }

    #[test]
    fn the_lowest_version_is_the_one_of_the_manifest() {
        let manifest = include_str!("../../../../herdr-plugin.toml");
        assert!(
            manifest.contains(&format!("min_herdr_version = \"{MIN_HERDR_VERSION}\"")),
            "update MIN_HERDR_VERSION"
        );
    }
}

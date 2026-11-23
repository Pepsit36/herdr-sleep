//! The help texts: the main help and one help per command.

/// The main help, printed by `herdr-sleep --help` and, on stderr, after a
/// usage error.
pub const MAIN: &str = "\
Usage: herdr-sleep <command> [options]

Herdr Sleep puts idle coding agents to sleep to free memory and wakes them
on demand, keeping their pane, name and session.

Commands:
  watch [--detach]            Run the watcher that sweeps on a timer
  sweep [--now] [--json]      Put to sleep every agent idle long enough, now
  sleep [<pane|name>] [--force] [--from-session] [--wait[=SECONDS]] [--json]
                              Put one agent to sleep
  wake [<pane|name>] [--no-wait] [--json]
                              Wake one sleeping agent
  keep-awake [<pane|name>] [--on|--off]
                              Keep the tab of a pane awake, or let it sleep again
  status [--json]             List the managed agents and the watcher
  config [--json]             Print the effective configuration
  event                       Command of the focus hooks (wakes the focused pane)
  ensure-watcher [--json]     Start the watcher unless it is running
  restore [--json]            Put panes back to sleep after a Herdr restart

Run 'herdr-sleep <command> --help' for the options of a command.
Without a pane or name, sleep and wake act on the pane of the Herdr action
that started them.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit

Exit codes:
  0  success
  1  failure or refusal (the reason is printed on stderr or in the output)
  2  usage error; wake: the agent was not ready after 120 s

Files:
  config      <config dir>/config.toml (see 'herdr-sleep config' for the path)
  state       <state dir>/ with sweep.json (idle clocks), sleepers.json
              (sleeping panes), watcher.lock and herdr-sleep.log (the journal)
";

const SLEEP: &str = "\
Usage: herdr-sleep sleep [<pane|name>] [--force] [--from-session] [--wait[=SECONDS]] [--json] [--]

Put an agent to sleep: it quits through its own exit command and its pane
keeps a Placeholder. The safety guards apply. A refusal is printed on stderr
with what to try next.

A pane with no live agent but a saved session (what a Herdr restart leaves
for an agent that was awake) is handled too: its agent is started again with
its session, then put to sleep. The pane must be given by id.

Without a pane or name the pane of the Herdr action context is used
(HERDR_PLUGIN_CONTEXT_JSON pane_id, then focused_pane_id, then HERDR_PANE_ID).

Options:
  --force     Skip the guards (use with care)
  --from-session
              The pane has no live agent: resume its saved session, then put
              it to sleep. Fails when an agent is running there
  --wait[=SECONDS]
              Keep trying while the agent is not idle yet or shows no prompt
              (default 120 seconds)
  --json      Print one JSON object instead of a line:
              pane, name, outcome (asleep, refused, dry-run, aborted, warned),
              slept (bool), freed_kb (number or null), reason (string or null)
  --          End of options; the next word is the target

Exit codes:
  0  asleep, or dry run
  1  refused, aborted, failed or no such agent
  2  usage error (no target and no action context)
";

const WAKE: &str = "\
Usage: herdr-sleep wake [<pane|name>] [--no-wait] [--json] [--]

Wake a sleeping agent and wait until it is ready (its prompt is back).

Without a pane or name the pane of the Herdr action context is used
(HERDR_PLUGIN_CONTEXT_JSON pane_id, then focused_pane_id, then HERDR_PANE_ID).

Options:
  --no-wait   Return right away
  --json      Print one JSON object instead of a line:
              woke (bool), pane, name (string or null), ms (number or null),
              reason (not-found, not-sleeping, deduplicated, timeout, or the
              failure text; null when woken)
  --          End of options; the next word is the target

Exit codes:
  0  woken, not asleep, or woken a moment ago
  1  no such agent, or the wake failed
  2  usage error, or not ready after 120 s
";

const KEEP_AWAKE: &str = "\
Usage: herdr-sleep keep-awake [<pane|name>] [--on|--off] [--]

Keep the tab of a pane awake: its label gets the keep_awake_icon in front, and
no agent of that tab goes to sleep, by sweep or by `sleep` (the tab-kept-awake
refusal; `sleep --force` ignores it). Agents already asleep stay asleep.
Running it again removes the icon, and so does renaming the tab without it.

Without a pane or name the pane of the Herdr action context is used
(HERDR_PLUGIN_CONTEXT_JSON pane_id, then focused_pane_id, then HERDR_PANE_ID).

Options:
  --on    Keep the tab awake (nothing changes when it already is)
  --off   Let the tab sleep (nothing changes when it already can)
  --      End of options; the next word is the target

Exit codes:
  0  done, or already in the asked state
  1  no such pane or agent, or the tab could not be renamed
  2  usage error
";

const STATUS: &str = "\
Usage: herdr-sleep status [--json]

List every managed agent, one line each: sleep marker, pane, name, kind,
status, idle time and memory (MB) of an awake agent, and the last refusal
that kept it awake. The footer gives the watcher state and the state
directory.

Options:
  --json      Print one JSON object instead:
              agents (array of pane, name, kind, status, sleeping, idle_seconds,
              rss_kb, refusal; null when unknown), watcher (running, pid),
              state_dir (string or null)

Exit codes:
  0  success
  1  Herdr unreachable or the state directory unusable
";

const SWEEP: &str = "\
Usage: herdr-sleep sweep [--now] [--json]

Run one sweep now: put to sleep every agent idle for sleep_after_minutes.

Options:
  --now       Do not wait for sleep_after_minutes, and do not stop at three
              agents: every settled, unfocused agent is considered. The other
              guards still apply
  --json      Print the report as one JSON object instead of lines:
              considered (number), slept (panes), refused (array of
              [pane, code]), aborted (array of [pane, reason]), dry_run
              (panes), deferred (number), error (string or null)

Exit codes:
  0  the sweep ran
  1  the sweep could not run (Herdr unreachable) or the configuration is invalid
";

const CONFIG: &str = "\
Usage: herdr-sleep config [--json]

Print the effective configuration (the file merged with the defaults and the
built-in agent profiles) as TOML. The path of the config file, the state
directory and any warnings go to stderr.

Options:
  --json      Print JSON instead of TOML. Keys are the configuration keys,
              plus profiles (by kind) and warnings

Exit codes:
  0  success
  1  the configuration is invalid
";

const WATCH: &str = "\
Usage: herdr-sleep watch [--detach]

Run the watcher, which sweeps every sweep_interval_seconds. Only one runs at
a time; a second one exits right away.

Options:
  --detach    Start the watcher in the background and return

Exit codes:
  0  the watcher ran and ended, or one was already running
  1  the watcher could not start
";

const EVENT: &str = "\
Usage: herdr-sleep event

Command of the pane.focused, tab.focused and workspace.focused hooks: wakes
the focused pane when it sleeps (that pane only). Reads the HERDR_PLUGIN_*
variables, prints nothing and always exits 0.
";

const ENSURE_WATCHER: &str = "\
Usage: herdr-sleep ensure-watcher [--json]

Start the watcher unless it is already running.

Options:
  --json      Print one JSON object instead of a line:
              running (bool), started (bool), error (string or null)

Exit codes:
  0  the watcher runs
  1  the watcher could not be started
";

const RESTORE: &str = "\
Usage: herdr-sleep restore [--json]

Command of the startup hook, also safe to run by hand. After a Herdr restart,
put every pane back to sleep that was asleep before it (it gets its
Placeholder) or that Herdr holds a saved session for (its agent is started
again with its session, one at a time, then put to sleep). The pane must
still exist, run no agent, sit at its shell prompt and have its working
directory. A pane that is not ready in time gets a second chance after the
others, waiting restore_settle_timeout_seconds each time. Panes whose working
directory is gone are listed apart. Then make sure the watcher runs. Always
exits 0 unless the command line is wrong.

Options:
  --json      Print one JSON object instead of lines:
              restored (panes), skipped (array of [pane, reason]),
              missing_cwd (array of [pane, directory]), error (string or null)

Exit codes:
  0  the pass ran, or could not run (the journal says why)
  2  usage error
";

const REPLAY: &str = "\
Usage: herdr-sleep replay --pane <id> [--name <name>] --kind <kind> --file <path>

Internal: started by the Placeholder to type the captured input into the
woken agent. Prints nothing; the journal holds the outcome.
";

/// The help of one command, `None` when there is no such command.
pub fn command(name: &str) -> Option<&'static str> {
    Some(match name {
        "sleep" => SLEEP,
        "wake" => WAKE,
        "keep-awake" => KEEP_AWAKE,
        "status" => STATUS,
        "sweep" => SWEEP,
        "config" => CONFIG,
        "watch" => WATCH,
        "event" => EVENT,
        "ensure-watcher" => ENSURE_WATCHER,
        "restore" => RESTORE,
        "replay" => REPLAY,
        _ => return None,
    })
}

/// The help of a topic: a command, or the main help for `None`.
pub fn text(topic: Option<&str>) -> &'static str {
    topic.and_then(command).unwrap_or(MAIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::COMMANDS;

    #[test]
    fn every_command_has_a_help() {
        for name in COMMANDS {
            let help = command(name).unwrap_or_else(|| panic!("no help for {name}"));
            assert!(
                help.starts_with(&format!("Usage: herdr-sleep {name}")),
                "{name}"
            );
        }
    }

    #[test]
    fn the_main_help_lists_every_public_command_but_not_replay() {
        for name in COMMANDS.into_iter().filter(|name| *name != "replay") {
            assert!(
                MAIN.lines().any(|l| l.trim_start().starts_with(name)),
                "{name}"
            );
        }
        assert!(!MAIN.contains("replay"));
    }

    #[test]
    fn no_em_dash_anywhere() {
        for name in COMMANDS {
            assert!(!command(name).unwrap().contains('\u{2014}'));
        }
        assert!(!MAIN.contains('\u{2014}'));
    }

    #[test]
    fn main_help_snapshot() {
        let expected = "\
Usage: herdr-sleep <command> [options]

Herdr Sleep puts idle coding agents to sleep to free memory and wakes them
on demand, keeping their pane, name and session.

Commands:
  watch [--detach]            Run the watcher that sweeps on a timer
  sweep [--now] [--json]      Put to sleep every agent idle long enough, now
  sleep [<pane|name>] [--force] [--from-session] [--wait[=SECONDS]] [--json]
                              Put one agent to sleep
  wake [<pane|name>] [--no-wait] [--json]
                              Wake one sleeping agent
  keep-awake [<pane|name>] [--on|--off]
                              Keep the tab of a pane awake, or let it sleep again
  status [--json]             List the managed agents and the watcher
  config [--json]             Print the effective configuration
  event                       Command of the focus hooks (wakes the focused pane)
  ensure-watcher [--json]     Start the watcher unless it is running
  restore [--json]            Put panes back to sleep after a Herdr restart

Run 'herdr-sleep <command> --help' for the options of a command.
Without a pane or name, sleep and wake act on the pane of the Herdr action
that started them.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit

Exit codes:
  0  success
  1  failure or refusal (the reason is printed on stderr or in the output)
  2  usage error; wake: the agent was not ready after 120 s

Files:
  config      <config dir>/config.toml (see 'herdr-sleep config' for the path)
  state       <state dir>/ with sweep.json (idle clocks), sleepers.json
              (sleeping panes), watcher.lock and herdr-sleep.log (the journal)
";
        assert_eq!(MAIN, expected);
    }

    #[test]
    fn status_help_snapshot() {
        let expected = "\
Usage: herdr-sleep status [--json]

List every managed agent, one line each: sleep marker, pane, name, kind,
status, idle time and memory (MB) of an awake agent, and the last refusal
that kept it awake. The footer gives the watcher state and the state
directory.

Options:
  --json      Print one JSON object instead:
              agents (array of pane, name, kind, status, sleeping, idle_seconds,
              rss_kb, refusal; null when unknown), watcher (running, pid),
              state_dir (string or null)

Exit codes:
  0  success
  1  Herdr unreachable or the state directory unusable
";
        assert_eq!(command("status"), Some(expected));
    }

    #[test]
    fn text_falls_back_to_the_main_help() {
        assert_eq!(text(None), MAIN);
        assert_eq!(text(Some("nap")), MAIN);
        assert_eq!(text(Some("wake")), WAKE);
    }
}

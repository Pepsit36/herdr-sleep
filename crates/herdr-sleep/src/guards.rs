//! Sleep guards: the checks that decide whether an agent may be put to sleep.
//!
//! [`check`] is pure: it works on a [`GuardContext`], which reads the outside
//! world (process info, environment, screen) lazily through a [`Probe`].
//! [`LiveProbe`] is the real implementation; tests inject their own. Guards
//! fail closed (see ADR 0003): when the prompt cannot be recognised, the agent
//! is not put to sleep.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::config::{Config, Profile};
use crate::herdr::client::{HerdrClient, ReadFormat};
use crate::herdr::model::{Agent, AgentStatus, ProcessInfo};
use crate::procs::{self, ProcessTable};
use crate::tab_label;

/// Number of screen rows read to analyse the prompt. It is at least the largest
/// `prompt_area_lines` the configuration accepts, and `wake` reads the same
/// number of rows.
pub const SCREEN_LINES: usize = 40;
/// Only this many rows at the bottom of the screen are searched for a text
/// that excludes the agent from sleep.
const EXCLUDE_SCREEN_LINES: usize = 15;
/// Longest command shown for a busy child.
const COMMAND_MAX_CHARS: usize = 80;
/// Busy children listed in a refusal.
const LISTED_CHILDREN: usize = 2;
/// Foreground colour Claude Code uses for placeholder and suggestion text.
const GREY_RGB: [u32; 3] = [153, 153, 153];

/// Why the guards are being run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepMode {
    /// Automatic sweep: every guard applies.
    Sweep,
    /// The user asked for this agent to sleep: every safety guard applies,
    /// except Focused (the user knows what they look at).
    Manual,
    /// The user insists: every guard is skipped except "kind not managed",
    /// because without a Profile there is nothing to do.
    Force,
}

/// Why an agent may not sleep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotManaged,
    NoSession,
    NoConversation,
    NotSettled(AgentStatus),
    Focused,
    NotIncluded,
    Excluded,
    NameExcluded(String),
    ScreenExcluded(String),
    CmdlineExcluded(String),
    RemoteControl,
    TabKeptAwake,
    /// The label of the agent's tab cannot be read, so whether the tab is
    /// kept awake is unknown.
    TabUnreadable,
    CwdExcluded(String),
    AgentProcessNotFound,
    /// The agent is not a job of the pane's own shell (it runs in a nested
    /// shell, or replaced the shell), so quitting it would not bring the
    /// shell back to the foreground.
    NotShellJob,
    ProtectedEnv(String),
    BusyChildren {
        count: usize,
        first: Vec<String>,
    },
    PromptNotDetected,
    DraftInPrompt,
    /// Another Sleep holds the pane, or the pane is no longer the one the
    /// caller read: it already sleeps, or its Agent changed.
    Superseded,
}

impl Refusal {
    /// Stable kebab-case identifier, safe to store and match on.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NotManaged => "not-managed",
            Refusal::NoSession => "no-session",
            Refusal::NoConversation => "no-conversation",
            Refusal::NotSettled(_) => "not-settled",
            Refusal::Focused => "focused",
            Refusal::NotIncluded => "not-included",
            Refusal::Excluded => "excluded",
            Refusal::NameExcluded(_) => "excluded-name-regex",
            Refusal::ScreenExcluded(_) => "excluded-screen-regex",
            Refusal::CmdlineExcluded(_) => "excluded-cmdline-regex",
            Refusal::RemoteControl => "remote-control",
            Refusal::TabKeptAwake => "tab-kept-awake",
            Refusal::TabUnreadable => "tab-unreadable",
            Refusal::CwdExcluded(_) => "cwd-excluded",
            Refusal::AgentProcessNotFound => "agent-process-not-found",
            Refusal::NotShellJob => "not-shell-job",
            Refusal::ProtectedEnv(_) => "protected-env",
            Refusal::BusyChildren { .. } => "busy-children",
            Refusal::PromptNotDetected => "prompt-not-detected",
            Refusal::DraftInPrompt => "draft-in-prompt",
            Refusal::Superseded => "superseded",
        }
    }

    /// Whether the refusal is too ordinary to log or notify (the pane simply
    /// does not hold a managed agent, or another Sleep already has it).
    pub fn is_silent(&self) -> bool {
        matches!(self, Refusal::NotManaged | Refusal::Superseded)
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NotManaged => f.write_str("agent kind is not managed"),
            Refusal::NoSession => f.write_str(
                "no session id (Herdr reports none for this agent: its hook may be missing, for example `herdr integration install cursor`)",
            ),
            Refusal::NoConversation => f.write_str("no saved conversation to resume yet"),
            Refusal::NotSettled(status) => write!(f, "agent is {status}, not settled"),
            Refusal::Focused => f.write_str("agent is focused"),
            Refusal::NotIncluded => f.write_str("name is not in include_names"),
            Refusal::Excluded => f.write_str("name is in exclude_names"),
            Refusal::NameExcluded(name) => {
                write!(
                    f,
                    "excluded-name-regex: name {name} matches exclude_name_regex"
                )
            }
            Refusal::ScreenExcluded(line) => {
                write!(
                    f,
                    "excluded-screen-regex: the screen matches exclude_screen_regex ({line})"
                )
            }
            Refusal::CmdlineExcluded(command) => {
                write!(
                    f,
                    "excluded-cmdline-regex: a command line matches exclude_cmdline_regex ({command})"
                )
            }
            Refusal::RemoteControl => f.write_str(
                "remote-control: Remote Control is active, so the agent is driven from another device",
            ),
            Refusal::TabKeptAwake => f.write_str(
                "tab-kept-awake: the tab is kept awake (toggle it with the keep-awake action)",
            ),
            Refusal::TabUnreadable => f.write_str(
                "tab-unreadable: the label of the tab cannot be read, so it may be kept awake",
            ),
            Refusal::CwdExcluded(cwd) => write!(f, "working directory {cwd} is excluded"),
            Refusal::AgentProcessNotFound => f.write_str("agent process not found in the pane"),
            Refusal::NotShellJob => f.write_str(
                "the agent is not a job of the pane's shell (nested shell or exec), so quitting it would not return to the shell",
            ),
            Refusal::ProtectedEnv(var) => write!(f, "protected environment variable {var}"),
            Refusal::BusyChildren { count, first } => {
                let noun = if *count == 1 { "child" } else { "children" };
                write!(f, "{count} busy {noun}")?;
                if !first.is_empty() {
                    write!(f, ": {}", first.join(", "))?;
                    if *count > first.len() {
                        f.write_str(", ...")?;
                    }
                }
                Ok(())
            }
            Refusal::PromptNotDetected => f.write_str(
                "prompt not detected on screen (see prompt_area_lines and prompt_frame_regex)",
            ),
            Refusal::DraftInPrompt => f.write_str("text is typed in the prompt"),
            Refusal::Superseded => f.write_str("another sleep holds the pane, or it changed"),
        }
    }
}

impl std::error::Error for Refusal {}

/// The outside world the guards look at, queried only when a guard needs it.
pub trait Probe {
    /// Foreground processes of the agent's pane.
    fn process_info(&self) -> Option<ProcessInfo>;
    /// Environment of process `pid`, `None` when unreadable.
    fn environment(&self, pid: i32) -> Option<Vec<(String, String)>>;
    /// The pane's screen as ANSI text, `None` when it cannot be read.
    fn screen(&self) -> Option<String>;
    /// Whether Claude Code has saved the conversation of `session_id`,
    /// `None` when that cannot be told.
    fn claude_conversation_saved(&self, session_id: &str) -> Option<bool>;
    /// The label of tab `tab_id`, `None` when it cannot be read.
    fn tab_label(&self, _tab_id: &str) -> Option<String> {
        None
    }
}

/// Claude Code's configuration directory: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
fn claude_config_dir(config_dir: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    match config_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".claude")),
    }
}

/// Whether `<config_dir>/projects/*/<session_id>.jsonl` exists. Claude Code
/// writes that transcript with the first message; without it `--resume` fails.
fn claude_transcript_exists(config_dir: &Path, session_id: &str) -> Option<bool> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id == ".." {
        return Some(false);
    }
    let file = format!("{session_id}.jsonl");
    let projects = fs::read_dir(config_dir.join("projects")).ok()?;
    Some(
        projects
            .flatten()
            .any(|project| project.path().join(&file).is_file()),
    )
}

/// Whether Claude Code saved the conversation of `session_id`, `None` when
/// that cannot be told.
pub fn claude_session_saved(session_id: &str) -> Option<bool> {
    let dir = claude_config_dir(
        std::env::var_os("CLAUDE_CONFIG_DIR"),
        std::env::var_os("HOME"),
    )?;
    claude_transcript_exists(&dir, session_id)
}

/// [`Probe`] backed by the real Herdr and the real processes.
pub struct LiveProbe<'a> {
    client: &'a HerdrClient,
    pane: &'a str,
}

impl<'a> LiveProbe<'a> {
    pub fn new(client: &'a HerdrClient, pane: &'a str) -> Self {
        Self { client, pane }
    }
}

impl Probe for LiveProbe<'_> {
    fn process_info(&self) -> Option<ProcessInfo> {
        self.client.pane_process_info(self.pane).ok()
    }

    fn environment(&self, pid: i32) -> Option<Vec<(String, String)>> {
        procs::environment_of(pid).ok()
    }

    fn screen(&self) -> Option<String> {
        self.client
            .pane_read(self.pane, SCREEN_LINES, ReadFormat::Ansi)
            .ok()
    }

    fn tab_label(&self, tab_id: &str) -> Option<String> {
        self.client
            .tab_list(None)
            .ok()?
            .into_iter()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.label)
    }

    fn claude_conversation_saved(&self, session_id: &str) -> Option<bool> {
        let dir = claude_config_dir(
            std::env::var_os("CLAUDE_CONFIG_DIR"),
            std::env::var_os("HOME"),
        )?;
        claude_transcript_exists(&dir, session_id)
    }
}

/// Everything [`check`] needs to know about one agent.
pub struct GuardContext<'a> {
    pub config: &'a Config,
    pub agent: &'a Agent,
    pub table: &'a ProcessTable,
    pub probe: &'a dyn Probe,
}

impl<'a> GuardContext<'a> {
    pub fn new(
        config: &'a Config,
        agent: &'a Agent,
        table: &'a ProcessTable,
        probe: &'a dyn Probe,
    ) -> Self {
        Self {
            config,
            agent,
            table,
            probe,
        }
    }
}

/// Run the guards against the real Herdr and process table.
pub fn check_live(
    client: &HerdrClient,
    config: &Config,
    agent: &Agent,
    table: &ProcessTable,
    mode: SleepMode,
) -> Result<(), Refusal> {
    let probe = LiveProbe::new(client, &agent.pane_id);
    check(&GuardContext::new(config, agent, table, &probe), mode)
}

/// Decide whether the agent may sleep. The first failing guard wins.
pub fn check(ctx: &GuardContext<'_>, mode: SleepMode) -> Result<(), Refusal> {
    let (config, agent) = (ctx.config, ctx.agent);

    let profile = agent
        .kind()
        .and_then(|kind| config.profile(kind))
        .ok_or(Refusal::NotManaged)?;
    if mode == SleepMode::Force {
        return Ok(());
    }

    let session_id = agent.session_id().ok_or(Refusal::NoSession)?;
    // Only Claude Code has a reliable check; a missing answer fails closed.
    if agent.kind() == Some("claude")
        && ctx.probe.claude_conversation_saved(session_id) != Some(true)
    {
        return Err(Refusal::NoConversation);
    }
    if !config.allows_status(agent.agent_status) {
        return Err(Refusal::NotSettled(agent.agent_status));
    }
    if mode == SleepMode::Sweep && agent.focused {
        return Err(Refusal::Focused);
    }
    check_filters(config, agent)?;
    // Fails closed like the conversation check: a label that cannot be read
    // may be the one of a tab kept awake.
    if !config.keep_awake_icon.is_empty() {
        match ctx.probe.tab_label(&agent.tab_id) {
            Some(label)
                if tab_label::keeps_awake(&label, &config.sleep_icon, &config.keep_awake_icon) =>
            {
                return Err(Refusal::TabKeptAwake);
            }
            Some(_) => {}
            None => return Err(Refusal::TabUnreadable),
        }
    }

    let info = ctx.probe.process_info();
    let pid = info
        .as_ref()
        .and_then(|info| procs::find_agent_process(info, ctx.table, &profile.bin))
        .ok_or(Refusal::AgentProcessNotFound)?;
    if !info
        .as_ref()
        .is_some_and(|info| is_job_of_shell(info, ctx.table))
    {
        return Err(Refusal::NotShellJob);
    }

    if let Some(command) = excluded_command(config, ctx.table, pid) {
        return Err(Refusal::CmdlineExcluded(shorten(command)));
    }
    if let Some(line) = excluded_screen_line(ctx) {
        return Err(Refusal::ScreenExcluded(shorten(&line)));
    }
    if agent.kind() == Some("claude") && remote_control_active(ctx, pid) {
        return Err(Refusal::RemoteControl);
    }

    if let Some(environment) = ctx.probe.environment(pid)
        && let Some(var) = config
            .protect_env_vars
            .iter()
            .find(|var| environment.iter().any(|(key, _)| key == *var))
    {
        return Err(Refusal::ProtectedEnv(var.clone()));
    }

    let busy: Vec<&str> = ctx
        .table
        .descendants(pid)
        .into_iter()
        .map(|entry| entry.command.as_str())
        .filter(|command| !config.child_ignored(command))
        .collect();
    if !busy.is_empty() {
        return Err(Refusal::BusyChildren {
            count: busy.len(),
            first: busy
                .iter()
                .take(LISTED_CHILDREN)
                .copied()
                .map(shorten)
                .collect(),
        });
    }

    let screen = ctx.probe.screen();
    match screen.as_deref().map(|s| prompt_state(s, profile)) {
        None | Some(PromptState::NotFound) => Err(Refusal::PromptNotDetected),
        Some(PromptState::Draft) => {
            let saved = profile.sleep_with_draft
                && screen
                    .as_deref()
                    .is_some_and(|s| matches!(read_draft(s, profile), Some(DraftRead::Text(_))));
            if saved {
                Ok(())
            } else {
                Err(Refusal::DraftInPrompt)
            }
        }
        Some(PromptState::Empty) => Ok(()),
    }
}

/// Whether the foreground group of the pane is a job its own shell started:
/// its leader is a direct child of the shell. Quitting the agent then hands
/// the foreground back to the shell, which is how a Sleep sees the agent gone.
/// An agent in a nested shell (`nix develop`, `sudo -i`) leaves that nested
/// shell in the foreground, and one started with `exec` is the shell itself.
fn is_job_of_shell(info: &ProcessInfo, table: &ProcessTable) -> bool {
    info.foreground_process_group_id != info.shell_pid
        && table
            .get(info.foreground_process_group_id)
            .is_some_and(|leader| leader.ppid == info.shell_pid)
}

/// The first command line of the agent process or of any of its descendants
/// (ignored children included) that matches `exclude_cmdline_regex`.
fn excluded_command<'t>(config: &Config, table: &'t ProcessTable, pid: i32) -> Option<&'t str> {
    if config.exclude_cmdline_regex.is_empty() {
        return None;
    }
    table
        .get(pid)
        .into_iter()
        .chain(table.descendants(pid))
        .map(|entry| entry.command.as_str())
        .find(|command| config.cmdline_excluded(command))
}

/// The badge Claude Code shows while Remote Control is connected.
const REMOTE_CONTROL_BADGE: &str = "/rc active";

/// Whether a Claude Code session is driven through Remote Control: it was
/// started with `--remote-control` (or `--rc`), or the screen shows the
/// `/rc active` badge in its last [`EXCLUDE_SCREEN_LINES`] lines. A terminal
/// too narrow to draw the badge hides it, so only the flag protects then.
fn remote_control_active(ctx: &GuardContext<'_>, pid: i32) -> bool {
    let flagged = ctx.table.get(pid).is_some_and(|entry| {
        entry.command.split_whitespace().any(|word| {
            let name = word.split('=').next().unwrap_or(word);
            name == "--remote-control" || name == "--rc"
        })
    });
    flagged
        || ctx.probe.screen().is_some_and(|screen| {
            let lines: Vec<&str> = screen.split('\n').collect();
            let start = lines.len().saturating_sub(EXCLUDE_SCREEN_LINES);
            lines[start..].iter().any(|line| {
                let plain: String = parse_cells(line).iter().map(|c| c.ch).collect();
                plain.to_lowercase().contains(REMOTE_CONTROL_BADGE)
            })
        })
}

/// The first of the last [`EXCLUDE_SCREEN_LINES`] screen lines, without colours,
/// that matches `exclude_screen_regex`. The screen is only read when the list
/// is not empty.
fn excluded_screen_line(ctx: &GuardContext<'_>) -> Option<String> {
    if ctx.config.exclude_screen_regex.is_empty() {
        return None;
    }
    let screen = ctx.probe.screen()?;
    let lines: Vec<&str> = screen.split('\n').collect();
    let start = lines.len().saturating_sub(EXCLUDE_SCREEN_LINES);
    lines[start..]
        .iter()
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let plain: String = parse_cells(line).iter().map(|c| c.ch).collect();
            plain.trim().to_string()
        })
        .find(|line| ctx.config.screen_excluded(line))
}

fn shorten(text: &str) -> String {
    text.chars().take(COMMAND_MAX_CHARS).collect()
}

fn check_filters(config: &Config, agent: &Agent) -> Result<(), Refusal> {
    let name = agent.name.as_deref();
    let listed = |list: &[String]| name.is_some_and(|n| list.iter().any(|x| x == n));
    if !config.include_names.is_empty() && !listed(&config.include_names) {
        return Err(Refusal::NotIncluded);
    }
    if listed(&config.exclude_names) {
        return Err(Refusal::Excluded);
    }
    if let Some(name) = name.filter(|n| config.name_excluded(n)) {
        return Err(Refusal::NameExcluded(name.to_string()));
    }
    if let Some(cwd) = agent.effective_cwd()
        && config.cwd_excluded(cwd)
    {
        return Err(Refusal::CwdExcluded(cwd.to_string()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt analysis
// ---------------------------------------------------------------------------

/// What the agent's input line holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptState {
    /// No line of the prompt zone matches the Profile's `prompt_regex`.
    NotFound,
    /// The prompt is there and nothing (or only a grey suggestion) is typed.
    Empty,
    /// The user typed something.
    Draft,
}

/// Whether the screen shows the agent at a recognisable prompt.
pub fn is_ready_screen(screen_ansi: &str, profile: &Profile) -> bool {
    prompt_state(screen_ansi, profile) != PromptState::NotFound
}

/// Analyse the prompt of an ANSI screen (see [`PromptState`]).
///
/// The prompt is looked for between the last two rules (`prompt_frame_regex`)
/// of the screen, so a tall status bar below it and the earlier messages above
/// it do not matter: a frame without a prompt inside is not a prompt. Without
/// a frame (two rules), the last `prompt_area_lines` rows are searched.
pub fn prompt_state(screen_ansi: &str, profile: &Profile) -> PromptState {
    let lines: Vec<&str> = screen_ansi
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let framed = framed_zone(&lines, profile);
    let area = match framed {
        Some(zone) => zone,
        None => &lines[lines.len().saturating_sub(profile.prompt_area_lines)..],
    };
    let Some((row, state)) = area
        .iter()
        .enumerate()
        .rev()
        .find_map(|(row, line)| Some((row, line_state(line, &profile.prompt_regex)?)))
    else {
        return PromptState::NotFound;
    };
    // Inside the frame, whatever is drawn under the prompt row belongs to the
    // draft: one that starts with a line break leaves the first row empty.
    if state == PromptState::Empty && framed.is_some() && has_text(&area[row + 1..]) {
        return PromptState::Draft;
    }
    state
}

/// Whether any of `lines` shows something besides blanks and box drawing.
fn has_text(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        parse_cells(line)
            .iter()
            .any(|c| !c.dim && !is_blank(c.ch) && !is_box(c.ch))
    })
}

/// The lines between the last two rules of the screen. A rule that a narrow
/// pane wraps onto several rows counts once.
fn framed_zone<'a, 'l>(lines: &'a [&'l str], profile: &Profile) -> Option<&'a [&'l str]> {
    let (upper, lower) = frame_bounds(lines, profile)?;
    Some(&lines[upper + 1..lower])
}

/// Row of the upper rule and of the first row of the lower rule.
fn frame_bounds(lines: &[&str], profile: &Profile) -> Option<(usize, usize)> {
    let frame = profile.prompt_frame_regex.as_ref()?;
    let is_rule = |line: &str| {
        let plain: String = parse_cells(line).iter().map(|c| c.ch).collect();
        frame.is_match(&plain)
    };
    // The first row of the lowest rule, then the last row of the one above.
    let mut lower = lines.iter().rposition(|line| is_rule(line))?;
    while lower > 0 && is_rule(lines[lower - 1]) {
        lower -= 1;
    }
    let upper = lines[..lower].iter().rposition(|line| is_rule(line))?;
    Some((upper, lower))
}

/// The screen above the input box: everything before the upper rule of the
/// frame, so the box and the status bar below it are left out. `None` without
/// a frame.
pub fn above_frame<'a>(screen: &'a str, profile: &Profile) -> Option<Vec<&'a str>> {
    let lines: Vec<&str> = screen
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let (upper, _) = frame_bounds(&lines, profile)?;
    Some(lines[..upper].to_vec())
}

/// One visible character and whether it is drawn dim or grey.
#[derive(Debug, Clone, Copy)]
struct Cell {
    ch: char,
    dim: bool,
}

fn is_blank(c: char) -> bool {
    c.is_whitespace() || c == '\u{00A0}'
}

fn is_box(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// What a line holds when it is the prompt line: the text typed after the
/// marker (grey suggestions left out). `None` when the line is not a prompt.
fn line_input(line: &str, prompt: &Regex) -> Option<String> {
    let cells = parse_cells(line);
    // Skip indentation and a left box border, so `│ ❯ text │` matches too.
    let lead = cells
        .iter()
        .take_while(|c| is_blank(c.ch) || is_box(c.ch))
        .count();
    let plain: String = cells[lead..].iter().map(|c| c.ch).collect();
    let end = prompt.find(&plain)?.end();
    let marker_chars = plain[..end].chars().count();
    let input: String = cells[lead + marker_chars..]
        .iter()
        .filter(|c| !c.dim)
        .map(|c| c.ch)
        .collect();
    Some(
        input
            .trim_end_matches(|c| is_blank(c) || is_box(c))
            .to_string(),
    )
}

/// The state of one line when it holds the prompt, `None` when it does not.
fn line_state(line: &str, prompt: &Regex) -> Option<PromptState> {
    let input = line_input(line, prompt)?;
    Some(if input.chars().any(|c| !is_blank(c)) {
        PromptState::Draft
    } else {
        PromptState::Empty
    })
}

/// A draft typed in the prompt, as far as the screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftRead {
    /// The whole draft, on one row, safe to type back.
    Text(String),
    /// A draft the screen cannot give back faithfully: several rows (a long
    /// line wrapped by the terminal looks like two lines), a pasted text the
    /// agent shows as a summary, or control characters.
    Unreadable,
}

/// The draft in the prompt of an ANSI screen. `None` when the prompt is empty
/// or not found.
pub fn read_draft(screen_ansi: &str, profile: &Profile) -> Option<DraftRead> {
    let lines: Vec<&str> = screen_ansi
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let area = match framed_zone(&lines, profile) {
        Some(zone) => zone,
        None => &lines[lines.len().saturating_sub(profile.prompt_area_lines)..],
    };
    let row = area
        .iter()
        .rposition(|line| line_input(line, &profile.prompt_regex).is_some())?;
    let input = line_input(area[row], &profile.prompt_regex)?;
    let input = input.trim_start_matches(is_blank).to_string();
    if input.is_empty() {
        return None;
    }
    // Anything visible under the prompt row may be a second row of the draft.
    let alone = area[row + 1..].iter().all(|line| {
        parse_cells(line)
            .iter()
            .all(|c| is_blank(c.ch) || is_box(c.ch))
    });
    let summarised = input.contains("[Pasted text") || input.contains("[Image");
    if !alone || summarised || input.chars().any(char::is_control) {
        return Some(DraftRead::Unreadable);
    }
    Some(DraftRead::Text(input))
}

/// Split a line into visible characters with their dim flag. Dim is SGR 2,
/// or the grey foreground; SGR 0, 22, 39 or another colour ends it.
fn parse_cells(line: &str) -> Vec<Cell> {
    let mut cells = Vec::new();
    let (mut faint, mut grey) = (false, false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            cells.push(Cell {
                ch: c,
                dim: faint || grey,
            });
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                let mut final_byte = None;
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        final_byte = Some(c);
                        break;
                    }
                    params.push(c);
                }
                if final_byte == Some('m') {
                    apply_sgr(&params, &mut faint, &mut grey);
                }
            }
            // OSC (a hyperlink, a title): up to BEL or `ESC \`, never text.
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                }
            }
            // Two-character escape such as `ESC ( B`: drop the next char.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    cells
}

fn apply_sgr(params: &str, faint: &mut bool, grey: &mut bool) {
    let codes: Vec<u32> = params
        .split([';', ':'])
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => (*faint, *grey) = (false, false),
            2 => *faint = true,
            22 => *faint = false,
            39 | 30..=37 | 90..=97 => *grey = false,
            // Foreground (38), background (48) and underline (58) colours take
            // their parameters along: skipping them keeps the `2` of
            // `48;2;R;G;B` from reading as faint. Only the foreground can be
            // the grey of a placeholder.
            38 | 48 | 58 => {
                let foreground = codes[i] == 38;
                match codes.get(i + 1) {
                    Some(2) => {
                        if foreground {
                            *grey = codes.get(i + 2..i + 5) == Some(&GREY_RGB[..]);
                        }
                        i += 4;
                    }
                    Some(5) => {
                        if foreground {
                            *grey = false;
                        }
                        i += 2;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_PROMPT_AREA_LINES;
    use crate::procs::ProcessTable;
    use regex::RegexBuilder;
    use std::collections::HashMap;

    /// Expand `\e` into a real escape byte.
    fn esc(s: &str) -> String {
        s.replace("\\e", "\u{1b}")
    }

    fn config() -> Config {
        Config::default()
    }

    fn profile(kind: &str) -> Profile {
        config().profile(kind).unwrap().clone()
    }

    fn state(screen: &str, kind: &str) -> PromptState {
        prompt_state(&esc(screen), &profile(kind))
    }

    // ---- prompt analysis ------------------------------------------------

    const RULE: &str =
        "\\e[0m\\e[38;2;136;136;136m\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\\e[0m";

    fn claude_screen(prompt_line: &str) -> String {
        format!(
            "\\e[0m  done\r\n\r\n{RULE}\r\n{prompt_line}\r\n{RULE}\r\n  \\e[0m\\e[38;5;5m[Model]\\e[0m\r\n  auto mode on"
        )
    }

    #[test]
    fn empty_claude_prompt() {
        let screen = claude_screen("\\e[0m\\e[38;2;153;153;153m\u{276F}\u{a0}\\e[0m");
        assert_eq!(state(&screen, "claude"), PromptState::Empty);
        let screen = claude_screen("\\e[0m\\e[38;2;153;153;153m\u{276F} \u{a0}\\e[0m");
        assert_eq!(state(&screen, "claude"), PromptState::Empty);
    }

    #[test]
    fn typed_text_is_a_draft() {
        let screen = claude_screen("\\e[0m\\e[38;2;153;153;153m\u{276F}\u{a0}\\e[0mhello\\e[0m");
        assert_eq!(state(&screen, "claude"), PromptState::Draft);
    }

    #[test]
    fn grey_suggestion_only_is_empty() {
        let screen = claude_screen(
            "\\e[0m\\e[38;2;153;153;153m\u{276F}\u{a0}\\e[0m\\e[38;2;153;153;153mrun the tests\\e[0m",
        );
        assert_eq!(state(&screen, "claude"), PromptState::Empty);
        let screen = claude_screen("\\e[0m\\e[1m\u{276F}\u{a0}\\e[2mrun the tests\\e[22m");
        assert_eq!(state(&screen, "claude"), PromptState::Empty);
        let screen = claude_screen("\\e[0m\u{276F}\u{a0}\\e[2mrun the tests");
        assert_eq!(state(&screen, "claude"), PromptState::Empty);
    }

    #[test]
    fn typed_text_then_dim_suggestion_is_a_draft() {
        let screen = claude_screen("\\e[0m\u{276F}\u{a0}fix\\e[2m the build\\e[0m");
        assert_eq!(state(&screen, "claude"), PromptState::Draft);
    }

    #[test]
    fn exit_typed_under_open_menu_is_a_draft() {
        let screen = esc(&format!(
            "{RULE}\r\n\\e[0m\u{276F}\u{a0}/exit\\e[0m\r\n{RULE}\r\n  \\e[38;2;177;185;249m/exit\\e[0m  Exit the REPL\r\n  /export  Export"
        ));
        assert_eq!(
            prompt_state(&screen, &profile("claude")),
            PromptState::Draft
        );
    }

    #[test]
    fn box_borders_are_tolerated() {
        for (line, expected) in [
            (
                "\u{2502} \u{276F}\u{a0}        \u{2502}",
                PromptState::Empty,
            ),
            ("\u{2502} \u{276F} hello    \u{2502}", PromptState::Draft),
            (
                "\\e[2m\u{2502}\\e[0m \u{276F} \\e[2mtry this\\e[0m \\e[2m\u{2502}\\e[0m",
                PromptState::Empty,
            ),
        ] {
            assert_eq!(state(line, "claude"), expected, "{line}");
        }
    }

    #[test]
    fn no_prompt_is_not_found() {
        let screen = "\\e[0mcompiling...\r\n\\e[38;2;1;2;3mworking\\e[0m\r\n";
        assert_eq!(state(screen, "claude"), PromptState::NotFound);
        assert!(!is_ready_screen(&esc(screen), &profile("claude")));
        assert_eq!(state("", "claude"), PromptState::NotFound);
    }

    #[test]
    fn prompt_above_the_bottom_area_is_ignored() {
        let mut screen = String::from("\u{276F}\u{a0}\n");
        screen.push_str(&"output\n".repeat(DEFAULT_PROMPT_AREA_LINES));
        assert_eq!(state(&screen, "claude"), PromptState::NotFound);
        let near = format!(
            "\u{276F}\u{a0}\n{}",
            "output\n".repeat(DEFAULT_PROMPT_AREA_LINES - 2)
        );
        assert_eq!(state(&near, "claude"), PromptState::Empty);
    }

    // ---- prompt frame ---------------------------------------------------

    const WIDE_RULE: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";

    /// A 40-row Claude Code screen: history, the framed prompt on row 25 and a
    /// 14-row status bar below it (16 rows from the bottom).
    fn tall_screen(prompt_line: &str, history_prompt: bool) -> String {
        let mut rows: Vec<String> = (1..=8).map(|n| format!("  output {n}")).collect();
        rows.push(if history_prompt {
            "\u{276F} yes check".into()
        } else {
            "  yes check".into()
        });
        rows.extend((10..=23).map(|n| format!("  reply {n}")));
        rows.push(format!("\\e[38;2;136;136;136m{WIDE_RULE}\\e[0m"));
        rows.push(prompt_line.to_string());
        rows.push(format!("\\e[38;2;136;136;136m{WIDE_RULE}\\e[0m"));
        rows.extend((27..=39).map(|n| format!("  status {n}")));
        rows.push("  \u{23F5}\u{23F5} auto mode on (shift+tab to cycle)".into());
        assert_eq!(rows.len(), 40);
        esc(&rows.join("\r\n"))
    }

    const EMPTY_PROMPT: &str = "\\e[0m\\e[38;2;153;153;153m\u{276F}\u{a0}\\e[0m";

    fn without_frame(kind: &str) -> Profile {
        Profile {
            prompt_frame_regex: None,
            sleep_with_draft: false,
            ..profile(kind)
        }
    }

    #[test]
    fn a_framed_prompt_is_found_below_the_default_window() {
        let screen = tall_screen(EMPTY_PROMPT, true);
        assert_eq!(
            prompt_state(&screen, &profile("claude")),
            PromptState::Empty
        );
        assert!(is_ready_screen(&screen, &profile("claude")));
        // Without the frame detection the prompt is 16 rows from the bottom.
        assert_eq!(
            prompt_state(&screen, &without_frame("claude")),
            PromptState::NotFound
        );
        // A wider window finds it too.
        let wide = Profile {
            prompt_area_lines: 20,
            ..without_frame("claude")
        };
        assert_eq!(prompt_state(&screen, &wide), PromptState::Empty);
    }

    #[test]
    fn a_framed_draft_is_a_draft() {
        let screen = tall_screen("\\e[0m\u{276F}\u{a0}texte", true);
        assert_eq!(
            prompt_state(&screen, &profile("claude")),
            PromptState::Draft
        );
    }

    #[test]
    fn prompts_in_the_history_above_the_frame_are_ignored() {
        // The framed prompt is empty although the history holds typed messages.
        let screen = tall_screen(EMPTY_PROMPT, true);
        assert_eq!(
            prompt_state(&screen, &profile("claude")),
            PromptState::Empty
        );
        // A frame with no prompt inside is not a prompt, whatever the history says.
        let screen = tall_screen("  thinking...", true);
        assert_eq!(
            prompt_state(&screen, &profile("claude")),
            PromptState::NotFound
        );
    }

    #[test]
    fn a_rule_wrapped_by_a_narrow_pane_counts_once() {
        let wrapped = format!(
            "{}\r\n{}\r\n\u{276F}\u{a0}\r\n{}\r\n{}\r\nstatus",
            "\u{2500}".repeat(40),
            "\u{2500}".repeat(20),
            "\u{2500}".repeat(40),
            "\u{2500}".repeat(20),
        );
        assert_eq!(
            prompt_state(&wrapped, &profile("claude")),
            PromptState::Empty
        );
        // Only one rule is left: the last rows are searched.
        let lone = format!("{0}\r\n{0}\r\n\u{276F}\u{a0}", "\u{2500}".repeat(40));
        assert_eq!(prompt_state(&lone, &profile("claude")), PromptState::Empty);
    }

    #[test]
    fn without_two_rules_the_last_rows_are_searched() {
        // Short rules (under ten characters) are not a frame.
        let screen = claude_screen("\\e[0m\\e[38;2;153;153;153m\u{276F}\u{a0}\\e[0m");
        assert_eq!(
            prompt_state(&esc(&screen), &profile("claude")),
            PromptState::Empty
        );
        // A single wide rule is not a frame either.
        let one = format!("old\r\n{WIDE_RULE}\r\n\u{276F}\u{a0}\r\nstatus");
        assert_eq!(prompt_state(&one, &profile("claude")), PromptState::Empty);
    }

    #[test]
    fn other_agents_fall_back_without_a_frame() {
        // Codex draws no rule: the last rows still decide.
        assert_eq!(state("\u{203A} \n  gpt-5", "codex"), PromptState::Empty);
        let framed = format!("{WIDE_RULE}\r\n\u{203A} fix bug\r\n{WIDE_RULE}\r\n  status");
        assert_eq!(prompt_state(&framed, &profile("codex")), PromptState::Draft);
    }

    #[test]
    fn the_prompt_area_of_a_profile_sets_the_fallback_window() {
        let narrow = Profile {
            prompt_area_lines: 5,
            ..without_frame("claude")
        };
        let near = format!("\u{276F}\u{a0}\n{}", "output\n".repeat(3));
        assert_eq!(prompt_state(&near, &narrow), PromptState::Empty);
        let far = format!("\u{276F}\u{a0}\n{}", "output\n".repeat(5));
        assert_eq!(prompt_state(&far, &narrow), PromptState::NotFound);
        assert_eq!(DEFAULT_PROMPT_AREA_LINES, 15);
    }

    #[test]
    fn lowest_prompt_line_wins() {
        let screen = "\u{276F} old message\n\nreply\n\u{276F}\u{a0}\n";
        assert_eq!(state(screen, "claude"), PromptState::Empty);
    }

    #[test]
    fn other_profiles() {
        assert_eq!(state("\u{203A} \n  gpt-5", "codex"), PromptState::Empty);
        assert_eq!(
            state("\u{203A} \\e[2mImprove docs\\e[0m\n", "codex"),
            PromptState::Empty
        );
        assert_eq!(state("\u{203A} fix bug\n", "codex"), PromptState::Draft);
        assert_eq!(state(" > \n", "gemini"), PromptState::Empty);
        assert_eq!(
            state("\u{2502} > ls \u{2502}\n", "gemini"),
            PromptState::Draft
        );
        assert_eq!(state("\u{203A} \n", "gemini"), PromptState::NotFound);
        assert_eq!(state("\u{2192} \n", "cursor"), PromptState::Empty);
        assert!(is_ready_screen("> \n", &profile("gemini")));
    }

    #[test]
    fn a_coloured_background_does_not_make_the_text_faint() {
        // `48;2;R;G;B` carries a `2` that is not SGR faint.
        for sgr in ["48;2;40;40;40", "48;5;236", "58;2;1;2;3", "48:2:40:40:40"] {
            let cells = parse_cells(&esc(&format!("\\e[{sgr}m\u{276F} hello")));
            assert!(cells.iter().all(|c| !c.dim), "{sgr}: {cells:?}");
        }
        // A real faint after it still counts.
        let cells = parse_cells(&esc("\\e[48;2;40;40;40m\\e[2mhint"));
        assert!(cells.iter().all(|c| c.dim));
    }

    #[test]
    fn osc_sequences_are_not_text() {
        let text = |line: &str| {
            parse_cells(&esc(line))
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
        };
        assert_eq!(text("\\e]8;;https://x.y\\e\\link\\e]8;;\\e\\"), "link");
        assert_eq!(text("\\e]0;title\u{7}abc"), "abc");
    }

    #[test]
    fn sgr_parsing_details() {
        let cells = parse_cells(&esc(
            "\\e[38;5;4ma\\e[38;2;153;153;153mb\\e[39mc\\e[2;22md\\e[?25lx",
        ));
        let dims: Vec<bool> = cells.iter().map(|c| c.dim).collect();
        assert_eq!(dims, [false, true, false, false, false]);
    }

    // ---- guards ---------------------------------------------------------

    struct FakeProbe {
        info: Option<ProcessInfo>,
        env: HashMap<i32, Vec<(String, String)>>,
        screen: Option<String>,
        saved: Option<bool>,
        /// The label of every tab, `None` when it cannot be read.
        tab: Option<String>,
    }

    impl Probe for FakeProbe {
        fn claude_conversation_saved(&self, _session_id: &str) -> Option<bool> {
            self.saved
        }
        fn tab_label(&self, _: &str) -> Option<String> {
            self.tab.clone()
        }
        fn process_info(&self) -> Option<ProcessInfo> {
            self.info.clone()
        }
        fn environment(&self, pid: i32) -> Option<Vec<(String, String)>> {
            self.env.get(&pid).cloned()
        }
        fn screen(&self) -> Option<String> {
            self.screen.clone()
        }
    }

    const AGENT_PID: i32 = 100;

    fn probe(screen: &str) -> FakeProbe {
        FakeProbe {
            info: Some(ProcessInfo {
                pane_id: "w1:p1".into(),
                shell_pid: 50,
                foreground_process_group_id: AGENT_PID,
                foreground_processes: vec![crate::herdr::model::ForegroundProcess {
                    pid: AGENT_PID,
                    name: "claude".into(),
                    cwd: None,
                }],
            }),
            env: HashMap::new(),
            screen: Some(esc(screen)),
            saved: Some(true),
            tab: Some(String::new()),
        }
    }

    fn empty_screen() -> String {
        claude_screen("\\e[0m\\e[38;2;153;153;153m\u{276F}\u{a0}\\e[0m")
    }

    fn agent() -> Agent {
        Agent {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            agent: Some("claude".into()),
            agent_status: AgentStatus::Idle,
            agent_session: Some(crate::herdr::model::AgentSession {
                source: "herdr:claude".into(),
                agent: Some("claude".into()),
                kind: Some("id".into()),
                value: "sid-1".into(),
            }),
            ..Agent::default()
        }
    }

    fn table(children: &[&str]) -> ProcessTable {
        let mut text = String::from("50 1 100 -bash\n100 50 5000 claude\n");
        for (i, command) in children.iter().enumerate() {
            text.push_str(&format!("{} 100 10 {command}\n", 200 + i));
        }
        ProcessTable::parse(&text)
    }

    fn run(
        config: &Config,
        agent: &Agent,
        table: &ProcessTable,
        probe: &FakeProbe,
        mode: SleepMode,
    ) -> Result<(), Refusal> {
        check(&GuardContext::new(config, agent, table, probe), mode)
    }

    fn sweep(agent: &Agent, children: &[&str], probe: &FakeProbe) -> Result<(), Refusal> {
        run(&config(), agent, &table(children), probe, SleepMode::Sweep)
    }

    #[test]
    fn clean_agent_may_sleep_in_every_mode() {
        let p = probe(&empty_screen());
        for mode in [SleepMode::Sweep, SleepMode::Manual, SleepMode::Force] {
            assert_eq!(run(&config(), &agent(), &table(&[]), &p, mode), Ok(()));
        }
    }

    #[test]
    fn not_managed() {
        let p = probe(&empty_screen());
        let mut a = agent();
        a.agent = Some("mystery".into());
        for mode in [SleepMode::Sweep, SleepMode::Manual, SleepMode::Force] {
            let r = run(&config(), &a, &table(&[]), &p, mode);
            assert_eq!(r, Err(Refusal::NotManaged));
        }
        a.agent = None;
        assert_eq!(sweep(&a, &[], &p), Err(Refusal::NotManaged));
        assert!(Refusal::NotManaged.is_silent());
    }

    #[test]
    fn no_session() {
        let mut a = agent();
        a.agent_session = None;
        assert_eq!(
            sweep(&a, &[], &probe(&empty_screen())),
            Err(Refusal::NoSession)
        );
    }

    #[test]
    fn claude_without_a_saved_conversation_is_refused() {
        let mut p = probe(&empty_screen());
        p.saved = Some(false);
        assert_eq!(sweep(&agent(), &[], &p), Err(Refusal::NoConversation));
        // Unknown fails closed.
        p.saved = None;
        assert_eq!(sweep(&agent(), &[], &p), Err(Refusal::NoConversation));
        let t = table(&[]);
        assert_eq!(
            run(&config(), &agent(), &t, &p, SleepMode::Manual),
            Err(Refusal::NoConversation)
        );
        // Force skips every guard.
        assert_eq!(run(&config(), &agent(), &t, &p, SleepMode::Force), Ok(()));
        // NoSession comes first.
        let mut a = agent();
        a.agent_session = None;
        assert_eq!(sweep(&a, &[], &p), Err(Refusal::NoSession));
    }

    #[test]
    fn other_kinds_are_not_checked_for_a_conversation() {
        let mut p = probe(&empty_screen());
        p.saved = Some(false);
        p.info.as_mut().unwrap().foreground_processes[0].name = "codex".into();
        let mut a = agent();
        a.agent = Some("codex".into());
        assert_ne!(sweep(&a, &[], &p), Err(Refusal::NoConversation));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-guards-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn transcript_lookup() {
        let dir = scratch("transcript");
        assert_eq!(claude_transcript_exists(&dir, "sid-1"), None);
        fs::create_dir_all(dir.join("projects/-home-me-app")).unwrap();
        fs::create_dir_all(dir.join("projects/-home-me-other")).unwrap();
        assert_eq!(claude_transcript_exists(&dir, "sid-1"), Some(false));
        fs::write(dir.join("projects/-home-me-other/sid-1.jsonl"), "{}").unwrap();
        assert_eq!(claude_transcript_exists(&dir, "sid-1"), Some(true));
        assert_eq!(claude_transcript_exists(&dir, "sid-2"), Some(false));
        assert_eq!(claude_transcript_exists(&dir, "../sid-1"), Some(false));
        assert_eq!(claude_transcript_exists(&dir, ""), Some(false));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_dir_resolution() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            claude_config_dir(os("/cfg"), os("/home/me")),
            Some(PathBuf::from("/cfg"))
        );
        assert_eq!(
            claude_config_dir(None, os("/home/me")),
            Some(PathBuf::from("/home/me/.claude"))
        );
        assert_eq!(
            claude_config_dir(os(""), os("/home/me")),
            Some(PathBuf::from("/home/me/.claude"))
        );
        assert_eq!(claude_config_dir(None, None), None);
    }

    #[test]
    fn not_settled() {
        for status in [
            AgentStatus::Working,
            AgentStatus::Blocked,
            AgentStatus::Unknown,
        ] {
            let mut a = agent();
            a.agent_status = status;
            assert_eq!(
                sweep(&a, &[], &probe(&empty_screen())),
                Err(Refusal::NotSettled(status))
            );
        }
        let mut a = agent();
        a.agent_status = AgentStatus::Done;
        assert_eq!(sweep(&a, &[], &probe(&empty_screen())), Ok(()));
    }

    #[test]
    fn focused_in_sweep_only() {
        let mut a = agent();
        a.focused = true;
        let p = probe(&empty_screen());
        assert_eq!(sweep(&a, &["node server.js"], &p), Err(Refusal::Focused));
        let t = table(&[]);
        assert_eq!(run(&config(), &a, &t, &p, SleepMode::Manual), Ok(()));
    }

    #[test]
    fn name_filters() {
        let p = probe(&empty_screen());
        let t = table(&[]);
        let mut a = agent();
        a.name = Some("worker".into());

        let mut c = config();
        c.include_names = vec!["other".into()];
        assert_eq!(
            run(&c, &a, &t, &p, SleepMode::Sweep),
            Err(Refusal::NotIncluded)
        );
        a.name = None;
        assert_eq!(
            run(&c, &a, &t, &p, SleepMode::Sweep),
            Err(Refusal::NotIncluded)
        );

        let mut c = config();
        c.exclude_names = vec!["worker".into()];
        a.name = Some("worker".into());
        assert_eq!(
            run(&c, &a, &t, &p, SleepMode::Sweep),
            Err(Refusal::Excluded)
        );
        assert_eq!(run(&c, &a, &t, &p, SleepMode::Force), Ok(()));
    }

    #[test]
    fn name_regex_excludes_case_insensitively() {
        let mut c = config();
        c.exclude_name_regex = vec![
            RegexBuilder::new("^deploy")
                .case_insensitive(true)
                .build()
                .unwrap(),
        ];
        let p = probe(&empty_screen());
        let t = table(&[]);
        let mut a = agent();
        a.name = Some("Deploy-prod".into());
        let r = run(&c, &a, &t, &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::NameExcluded("Deploy-prod".into())));
        assert_eq!(
            run(&c, &a, &t, &p, SleepMode::Manual),
            Err(Refusal::NameExcluded("Deploy-prod".into()))
        );
        assert_eq!(run(&c, &a, &t, &p, SleepMode::Force), Ok(()));
        a.name = Some("redeploy".into());
        assert_eq!(run(&c, &a, &t, &p, SleepMode::Sweep), Ok(()));
        a.name = None;
        assert_eq!(run(&c, &a, &t, &p, SleepMode::Sweep), Ok(()));
    }

    fn with_regex(set: impl FnOnce(&mut Config, Vec<Regex>), pattern: &str) -> Config {
        let mut c = config();
        set(
            &mut c,
            vec![
                RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .unwrap(),
            ],
        );
        c
    }

    #[test]
    fn screen_regex_matches_the_last_lines_without_colours() {
        let c = with_regex(|c, r| c.exclude_screen_regex = r, "waiting for approval");
        let t = table(&[]);
        let screen = format!(
            "\\e[1mWaiting \\e[31mfor\\e[0m approval\r\n{}",
            empty_screen()
        );
        let r = run(&c, &agent(), &t, &probe(&screen), SleepMode::Sweep);
        assert_eq!(
            r,
            Err(Refusal::ScreenExcluded("Waiting for approval".into()))
        );
        assert_eq!(r.unwrap_err().code(), "excluded-screen-regex");
        let p = probe(&empty_screen());
        assert_eq!(run(&c, &agent(), &t, &p, SleepMode::Sweep), Ok(()));
        assert_eq!(
            run(&c, &agent(), &t, &probe(&screen), SleepMode::Force),
            Ok(())
        );
        // A match above the last lines is stale text and is ignored.
        let old = format!(
            "waiting for approval\n{}{}",
            "output\n".repeat(EXCLUDE_SCREEN_LINES),
            empty_screen()
        );
        assert_eq!(
            run(&c, &agent(), &t, &probe(&old), SleepMode::Sweep),
            Ok(())
        );
    }

    #[test]
    fn screen_regex_wins_over_a_missing_prompt() {
        let c = with_regex(|c, r| c.exclude_screen_regex = r, "running tests");
        let p = probe("running tests\r\n");
        let r = run(&c, &agent(), &table(&[]), &p, SleepMode::Manual);
        assert_eq!(r.unwrap_err().code(), "excluded-screen-regex");
    }

    #[test]
    fn the_screen_is_not_read_without_a_screen_regex() {
        let mut p = probe(&empty_screen());
        p.screen = None;
        let c = with_regex(|c, r| c.exclude_cmdline_regex = r, "nomatch");
        let r = run(&c, &agent(), &table(&[]), &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::PromptNotDetected));
        let c = with_regex(|c, r| c.exclude_screen_regex = r, "x");
        let r = run(&c, &agent(), &table(&[]), &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::PromptNotDetected));
    }

    /// A probe that also knows the label of every tab.
    struct LabelProbe {
        inner: FakeProbe,
        label: &'static str,
    }

    impl Probe for LabelProbe {
        fn process_info(&self) -> Option<ProcessInfo> {
            self.inner.process_info()
        }
        fn environment(&self, pid: i32) -> Option<Vec<(String, String)>> {
            self.inner.environment(pid)
        }
        fn screen(&self) -> Option<String> {
            self.inner.screen()
        }
        fn claude_conversation_saved(&self, session_id: &str) -> Option<bool> {
            self.inner.claude_conversation_saved(session_id)
        }
        fn tab_label(&self, _: &str) -> Option<String> {
            Some(self.label.into())
        }
    }

    #[test]
    fn a_tab_kept_awake_refuses_every_sleep_but_force() {
        let t = table(&[]);
        let run_with = |label: &'static str, mode| {
            let p = LabelProbe {
                inner: probe(&empty_screen()),
                label,
            };
            check(&GuardContext::new(&config(), &agent(), &t, &p), mode)
        };
        for mode in [SleepMode::Sweep, SleepMode::Manual] {
            let r = run_with("\u{1f512} build", mode);
            assert_eq!(r, Err(Refusal::TabKeptAwake));
            assert_eq!(r.unwrap_err().code(), "tab-kept-awake");
        }
        assert_eq!(
            run_with("\u{1f4a4} \u{1f512} build", SleepMode::Sweep),
            Err(Refusal::TabKeptAwake)
        );
        assert_eq!(run_with("\u{1f512} build", SleepMode::Force), Ok(()));
        assert_eq!(run_with("build", SleepMode::Sweep), Ok(()));
    }

    #[test]
    fn an_unreadable_tab_label_refuses_but_force_and_a_disabled_feature_do_not() {
        let t = table(&[]);
        let mut p = probe(&empty_screen());
        p.tab = None;
        let r = run(&config(), &agent(), &t, &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::TabUnreadable));
        assert_eq!(
            run(&config(), &agent(), &t, &p, SleepMode::Manual),
            Err(Refusal::TabUnreadable)
        );
        assert_eq!(run(&config(), &agent(), &t, &p, SleepMode::Force), Ok(()));
        let mut off = config();
        off.keep_awake_icon = String::new();
        assert_eq!(run(&off, &agent(), &t, &p, SleepMode::Sweep), Ok(()));
    }

    #[test]
    fn remote_control_keeps_a_claude_awake() {
        let t = table(&[]);
        let badge = format!("  \\e[2m/rc active\\e[0m\r\n{}", empty_screen());
        let r = run(&config(), &agent(), &t, &probe(&badge), SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::RemoteControl));
        assert_eq!(r.unwrap_err().code(), "remote-control");
        // Manual sleep is refused too, a forced one is not.
        let p = probe(&badge);
        assert_eq!(
            run(&config(), &agent(), &t, &p, SleepMode::Manual),
            Err(Refusal::RemoteControl)
        );
        assert_eq!(run(&config(), &agent(), &t, &p, SleepMode::Force), Ok(()));
        // Without the badge the agent sleeps.
        let p = probe(&empty_screen());
        assert_eq!(run(&config(), &agent(), &t, &p, SleepMode::Sweep), Ok(()));
    }

    #[test]
    fn the_remote_control_flag_is_enough_when_the_badge_is_hidden() {
        let p = probe(&empty_screen());
        for flag in ["--remote-control", "--rc", "--remote-control=work"] {
            let text = format!("50 1 100 -bash\n100 50 5000 claude {flag}\n");
            let r = run(
                &config(),
                &agent(),
                &ProcessTable::parse(&text),
                &p,
                SleepMode::Sweep,
            );
            assert_eq!(r, Err(Refusal::RemoteControl), "{flag}");
        }
        let other = ProcessTable::parse("50 1 100 -bash\n100 50 5000 claude --rcfile x\n");
        assert_eq!(
            run(&config(), &agent(), &other, &p, SleepMode::Sweep),
            Ok(())
        );
    }

    #[test]
    fn cmdline_regex_covers_the_agent_and_every_child() {
        let c = with_regex(|c, r| c.exclude_cmdline_regex = r, "cargo (test|build)");
        let p = probe(&empty_screen());
        let r = run(
            &c,
            &agent(),
            &table(&["Cargo TEST --all"]),
            &p,
            SleepMode::Sweep,
        );
        assert_eq!(r, Err(Refusal::CmdlineExcluded("Cargo TEST --all".into())));
        assert_eq!(r.unwrap_err().code(), "excluded-cmdline-regex");
        let t = table(&[]);
        assert_eq!(run(&c, &agent(), &t, &p, SleepMode::Sweep), Ok(()));
        // An ignored child still counts for the exclusion.
        let ignored = table(&["node /opt/mcp-server/cargo build"]);
        assert_eq!(
            sweep(&agent(), &["node /opt/mcp-server/cargo build"], &p),
            Ok(())
        );
        let r = run(&c, &agent(), &ignored, &p, SleepMode::Manual);
        assert_eq!(r.unwrap_err().code(), "excluded-cmdline-regex");
        let r = run(&c, &agent(), &ignored, &p, SleepMode::Force);
        assert_eq!(r, Ok(()));
        // The agent's own command line counts too.
        let c = with_regex(|c, r| c.exclude_cmdline_regex = r, "--dangerously");
        let own = ProcessTable::parse("50 1 100 -bash\n100 50 5000 claude --dangerously-skip\n");
        let r = run(&c, &agent(), &own, &p, SleepMode::Sweep);
        assert_eq!(r.unwrap_err().code(), "excluded-cmdline-regex");
    }

    #[test]
    fn cwd_excluded() {
        let mut c = config();
        c.exclude_cwd_regex = vec![Regex::new("/secret").unwrap()];
        let mut a = agent();
        a.cwd = Some("/home/u/secret/app".into());
        let r = run(
            &c,
            &a,
            &table(&[]),
            &probe(&empty_screen()),
            SleepMode::Sweep,
        );
        assert_eq!(r, Err(Refusal::CwdExcluded("/home/u/secret/app".into())));
        assert_eq!(r.unwrap_err().code(), "cwd-excluded");
    }

    #[test]
    fn agent_process_not_found() {
        let mut p = probe(&empty_screen());
        p.info.as_mut().unwrap().foreground_processes[0].name = "vim".into();
        let t = ProcessTable::parse("50 1 100 -bash\n100 50 5000 vim\n");
        let r = run(&config(), &agent(), &t, &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::AgentProcessNotFound));
        p.info = None;
        assert_eq!(sweep(&agent(), &[], &p), Err(Refusal::AgentProcessNotFound));
    }

    #[test]
    fn an_agent_in_a_nested_shell_is_refused() {
        let mut p = probe(&empty_screen());
        // `bash` (50) started from the pane shell (10), then the agent.
        p.info.as_mut().unwrap().shell_pid = 10;
        let t = ProcessTable::parse("10 1 100 -bash\n50 10 100 bash\n100 50 5000 claude\n");
        let r = run(&config(), &agent(), &t, &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::NotShellJob));
    }

    #[test]
    fn an_agent_that_replaced_the_shell_is_refused() {
        let mut p = probe(&empty_screen());
        p.info.as_mut().unwrap().shell_pid = AGENT_PID;
        let t = ProcessTable::parse("100 1 5000 claude\n");
        let r = run(&config(), &agent(), &t, &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::NotShellJob));
    }

    #[test]
    fn protected_env() {
        let mut c = config();
        c.protect_env_vars = vec!["KEEP_AWAKE".into(), "OTHER".into()];
        let mut p = probe(&empty_screen());
        p.env.insert(
            AGENT_PID,
            vec![("HOME".into(), "/h".into()), ("OTHER".into(), "1".into())],
        );
        let t = table(&[]);
        let r = run(&c, &agent(), &t, &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::ProtectedEnv("OTHER".into())));
        // Unreadable environment counts as not protected.
        p.env.clear();
        assert_eq!(run(&c, &agent(), &t, &p, SleepMode::Sweep), Ok(()));
        assert_eq!(run(&c, &agent(), &t, &p, SleepMode::Manual), Ok(()));
    }

    #[test]
    fn ignored_children_do_not_count() {
        let p = probe(&empty_screen());
        let ignored = [
            "node /opt/mcp-server/index.js",
            "(grep)",
            "npm exec some-tool",
            "typescript-language-server --stdio",
        ];
        assert_eq!(sweep(&agent(), &ignored, &p), Ok(()));
    }

    #[test]
    fn busy_children_are_listed() {
        let p = probe(&empty_screen());
        let r = sweep(
            &agent(),
            &["node server.js", "(grep)", "sleep 600", "make"],
            &p,
        );
        let refusal = r.unwrap_err();
        assert_eq!(
            refusal,
            Refusal::BusyChildren {
                count: 3,
                first: vec!["node server.js".into(), "sleep 600".into()]
            }
        );
        assert_eq!(
            refusal.to_string(),
            "3 busy children: node server.js, sleep 600, ..."
        );
        let one = sweep(&agent(), &["make"], &p).unwrap_err();
        assert_eq!(one.to_string(), "1 busy child: make");
    }

    #[test]
    fn long_commands_are_truncated() {
        let long = format!("node {}", "x".repeat(200));
        let p = probe(&empty_screen());
        match sweep(&agent(), &[&long], &p).unwrap_err() {
            Refusal::BusyChildren { first, .. } => {
                assert_eq!(first[0].chars().count(), COMMAND_MAX_CHARS);
                assert!(long.starts_with(&first[0]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn busy_children_checked_before_prompt_and_in_manual() {
        let mut p = probe("no prompt here");
        let t = table(&["node server.js"]);
        let r = run(&config(), &agent(), &t, &p, SleepMode::Manual);
        assert_eq!(r.unwrap_err().code(), "busy-children");
        p.screen = None;
        let r = run(&config(), &agent(), &table(&[]), &p, SleepMode::Manual);
        assert_eq!(r, Err(Refusal::PromptNotDetected));
    }

    #[test]
    fn prompt_not_detected_and_draft() {
        let p = probe("compiling\r\nstill compiling");
        assert_eq!(sweep(&agent(), &[], &p), Err(Refusal::PromptNotDetected));
        let draft = claude_screen("\\e[0m\u{276F}\u{a0}half a thought");
        let p = probe(&draft);
        assert_eq!(sweep(&agent(), &[], &p), Err(Refusal::DraftInPrompt));
        // Manual does not skip the draft guard; Force does.
        let t = table(&[]);
        let r = run(&config(), &agent(), &t, &p, SleepMode::Manual);
        assert_eq!(r, Err(Refusal::DraftInPrompt));
        assert_eq!(run(&config(), &agent(), &t, &p, SleepMode::Force), Ok(()));
    }

    /// Like `claude_screen`, with a frame long enough to be found.
    fn framed_screen(prompt_line: &str) -> String {
        let rule = "\u{2500}".repeat(12);
        format!("  done\r\n\r\n{rule}\r\n{prompt_line}\r\n{rule}\r\n  auto mode on")
    }

    #[test]
    fn a_draft_that_starts_with_a_line_break_is_a_draft() {
        let rule = "\u{2500}".repeat(12);
        let screen = format!(
            "  done\r\n{rule}\r\n\u{276F}\u{a0}\r\n  second row\r\n{rule}\r\n  auto mode on"
        );
        let p = probe(&screen);
        let r = run(&config(), &agent(), &table(&[]), &p, SleepMode::Sweep);
        assert_eq!(r, Err(Refusal::DraftInPrompt));
        // An empty prompt with blank rows under it stays empty.
        let screen =
            format!("  done\r\n{rule}\r\n\u{276F}\u{a0}\r\n   \r\n{rule}\r\n  auto mode on");
        let p = probe(&screen);
        let r = run(&config(), &agent(), &table(&[]), &p, SleepMode::Sweep);
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn a_one_row_draft_passes_only_when_it_may_be_saved() {
        let p = probe(&framed_screen("\\e[0m\u{276F}\u{a0}half a thought"));
        let t = table(&[]);
        let mut c = config();
        assert_eq!(
            run(&c, &agent(), &t, &p, SleepMode::Manual),
            Err(Refusal::DraftInPrompt)
        );
        c.profiles.get_mut("claude").unwrap().sleep_with_draft = true;
        assert_eq!(run(&c, &agent(), &t, &p, SleepMode::Manual), Ok(()));
        // A pasted text the agent summarises cannot be given back: still refused.
        let pasted = probe(&framed_screen(
            "\\e[0m\u{276F}\u{a0}[Pasted text #1 +20 lines]",
        ));
        assert_eq!(
            run(&c, &agent(), &t, &pasted, SleepMode::Manual),
            Err(Refusal::DraftInPrompt)
        );
    }

    #[test]
    fn reading_a_draft() {
        let read = |screen: &str| read_draft(&esc(screen), &profile("claude"));
        assert_eq!(
            read(&framed_screen("\\e[0m\u{276F}\u{a0}fix the build")),
            Some(DraftRead::Text("fix the build".into()))
        );
        // Nothing typed, or only a grey suggestion: no draft.
        assert_eq!(read(&framed_screen("\\e[0m\u{276F}\u{a0}")), None);
        assert_eq!(
            read(&framed_screen(
                "\\e[0m\u{276F}\u{a0}\\e[38;2;153;153;153msuggestion\\e[0m"
            )),
            None
        );
        // A second row inside the frame may belong to the draft.
        let two_rows = framed_screen("\\e[0m\u{276F}\u{a0}first row\r\n  second row");
        assert_eq!(read(&two_rows), Some(DraftRead::Unreadable));
        assert_eq!(
            read(&framed_screen("\\e[0m\u{276F}\u{a0}[Image #1]")),
            Some(DraftRead::Unreadable)
        );
    }

    #[test]
    fn force_skips_everything_but_not_managed() {
        let mut a = agent();
        a.agent_session = None;
        a.agent_status = AgentStatus::Working;
        a.focused = true;
        let p = FakeProbe {
            info: None,
            env: HashMap::new(),
            screen: None,
            saved: Some(true),
            tab: None,
        };
        assert_eq!(
            run(&config(), &a, &table(&["make"]), &p, SleepMode::Force),
            Ok(())
        );
    }

    #[test]
    fn codes_are_stable() {
        let cases = [
            (Refusal::NotManaged, "not-managed"),
            (Refusal::NoSession, "no-session"),
            (Refusal::NoConversation, "no-conversation"),
            (Refusal::NotSettled(AgentStatus::Working), "not-settled"),
            (Refusal::Focused, "focused"),
            (Refusal::NotIncluded, "not-included"),
            (Refusal::Excluded, "excluded"),
            (Refusal::NameExcluded("n".into()), "excluded-name-regex"),
            (Refusal::ScreenExcluded("s".into()), "excluded-screen-regex"),
            (
                Refusal::CmdlineExcluded("c".into()),
                "excluded-cmdline-regex",
            ),
            (Refusal::CwdExcluded("/x".into()), "cwd-excluded"),
            (Refusal::AgentProcessNotFound, "agent-process-not-found"),
            (Refusal::NotShellJob, "not-shell-job"),
            (Refusal::ProtectedEnv("V".into()), "protected-env"),
            (
                Refusal::BusyChildren {
                    count: 1,
                    first: vec![],
                },
                "busy-children",
            ),
            (Refusal::PromptNotDetected, "prompt-not-detected"),
            (Refusal::DraftInPrompt, "draft-in-prompt"),
            (Refusal::TabUnreadable, "tab-unreadable"),
            (Refusal::Superseded, "superseded"),
        ];
        for (refusal, code) in cases {
            assert_eq!(refusal.code(), code);
            assert_eq!(
                refusal.is_silent(),
                matches!(code, "not-managed" | "superseded")
            );
            assert!(!refusal.to_string().is_empty());
        }
        assert_eq!(
            Refusal::NotSettled(AgentStatus::Blocked).to_string(),
            "agent is blocked, not settled"
        );
    }
}

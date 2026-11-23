//! Configuration: `config.toml`, its defaults, validation and the built-in
//! Profiles.
//!
//! Loading is three steps. A first pass over a [`toml::Table`] records unknown
//! keys as warnings, a second pass deserialises a permissive raw struct, and a
//! conversion step validates every value and compiles every regex. Anything
//! unusable is a [`ConfigError`] naming the key; anything merely suspicious
//! (an unknown key, an incomplete custom Profile) is a warning in
//! [`Config::warnings`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize, Serializer};

use crate::herdr::model::AgentStatus;

/// Name of the configuration file inside [`config_dir`].
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Placeholder replaced by the Session identifier in `resume` arguments.
pub const SESSION_PLACEHOLDER: &str = "{sid}";

const PLUGIN_ID: &str = "pepsit36.herdr-sleep";

/// Built-in `ignore_child_regex`: child processes that never count as work.
pub const DEFAULT_IGNORE_CHILD_REGEX: [&str; 8] = [
    "mcp",
    "npm exec",
    "language-server",
    "tsserver",
    "caffeinate",
    "<defunct>",
    r"^\(.*\)$",
    "/cursor-agent/versions/",
];

const DEFAULT_SLEEP_AFTER_MINUTES: u64 = 15;
const DEFAULT_SLEEP_ICON: &str = "💤";
const DEFAULT_KEEP_AWAKE_ICON: &str = "🔒";
const DEFAULT_TOKEN_NAME: &str = "sleep";
const DEFAULT_PREVIEW_LINES: u32 = 20;
const MAX_PREVIEW_LINES: i64 = 200;
const DEFAULT_SWEEP_INTERVAL_SECONDS: u64 = 60;
const SWEEP_INTERVAL_RANGE: std::ops::RangeInclusive<i64> = 5..=3600;
const DEFAULT_RESTORE_SETTLE_TIMEOUT_SECONDS: u64 = 60;
/// Default number of bottom screen rows searched for the prompt when no frame is found.
pub const DEFAULT_PROMPT_AREA_LINES: usize = 15;
const PROMPT_AREA_LINES_RANGE: std::ops::RangeInclusive<i64> = 5..=40;
/// Default `prompt_frame_regex`: a horizontal rule of at least ten box-drawing dashes.
pub const DEFAULT_PROMPT_FRAME_REGEX: &str = r"^\s*\u{2500}[\u{2500}\s]{9,}$";

const RESTORE_SETTLE_TIMEOUT_RANGE: std::ops::RangeInclusive<i64> = 5..=600;

const TOP_LEVEL_KEYS: [&str; 23] = [
    "sleep_after_minutes",
    "sleep_statuses",
    "dry_run",
    "sleep_icon",
    "keep_awake_icon",
    "token_name",
    "tab_prefix",
    "preview_lines",
    "notify",
    "include_names",
    "exclude_names",
    "exclude_cwd_regex",
    "protect_env_vars",
    "ignore_child_regex",
    "prompt_area_lines",
    "prompt_frame_regex",
    "sleep_with_draft",
    "exclude_name_regex",
    "exclude_screen_regex",
    "exclude_cmdline_regex",
    "sweep_interval_seconds",
    "restore_settle_timeout_seconds",
    "agents",
];

const AGENT_KEYS: [&str; 7] = [
    "bin",
    "exit",
    "resume",
    "prompt_regex",
    "prompt_area_lines",
    "prompt_frame_regex",
    "sleep_with_draft",
];

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn non_empty(env: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    env(key).filter(|v| !v.is_empty())
}

/// Directory holding `config.toml`.
///
/// Precedence: `HERDR_SLEEP_CONFIG_DIR`, `HERDR_PLUGIN_CONFIG_DIR`,
/// `$XDG_CONFIG_HOME/herdr/plugins/config/pepsit36.herdr-sleep`, then
/// `~/.config/herdr/plugins/config/pepsit36.herdr-sleep`. `None` when no
/// variable leads to a directory (no `HOME`).
pub fn config_dir(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = non_empty(&env, "HERDR_SLEEP_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = non_empty(&env, "HERDR_PLUGIN_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = match non_empty(&env, "XDG_CONFIG_HOME") {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(non_empty(&env, "HOME")?).join(".config"),
    };
    Some(base.join("herdr/plugins/config").join(PLUGIN_ID))
}

/// Directory holding runtime state (watcher state, logs).
///
/// Precedence: `HERDR_SLEEP_STATE_DIR`, `HERDR_PLUGIN_STATE_DIR`,
/// `$XDG_STATE_HOME/herdr-sleep`, then `~/.local/state/herdr-sleep`.
pub fn state_dir(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = non_empty(&env, "HERDR_SLEEP_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = non_empty(&env, "HERDR_PLUGIN_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = match non_empty(&env, "XDG_STATE_HOME") {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(non_empty(&env, "HOME")?).join(".local/state"),
    };
    Some(base.join("herdr-sleep"))
}

/// Environment lookup backed by the process environment.
pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A configuration file that cannot be read, parsed or validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// File the error comes from, when the configuration was read from disk.
    pub path: Option<PathBuf>,
    /// Offending key (`preview_lines`, `agents.codex.resume`), when known.
    pub key: Option<String>,
    pub message: String,
}

impl ConfigError {
    fn key(key: &str, message: impl Into<String>) -> Self {
        ConfigError {
            path: None,
            key: Some(key.to_string()),
            message: message.into(),
        }
    }

    fn with_path(mut self, path: Option<&Path>) -> Self {
        self.path = path.map(Path::to_path_buf);
        self
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid configuration")?;
        if let Some(path) = &self.path {
            write!(f, " in {}", path.display())?;
        }
        if let Some(key) = &self.key {
            write!(f, ": key `{key}`")?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

/// What the plugin needs to know to put one Kind of Agent to sleep and bring
/// it back.
#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    pub kind: String,
    /// Executable that starts the agent.
    pub bin: String,
    /// Command typed into the agent to make it quit gracefully.
    pub exit: String,
    /// Arguments that resume a Session; contains `{sid}`.
    pub resume: Vec<String>,
    /// Matches the agent's empty input line when it is at its prompt.
    #[serde(serialize_with = "ser_regex")]
    pub prompt_regex: Regex,
    /// Rows at the bottom of the screen searched for the prompt when no frame is found.
    pub prompt_area_lines: usize,
    /// Matches a rule drawn above and below the input line; `None` turns the frame
    /// detection off.
    #[serde(serialize_with = "ser_opt_regex")]
    pub prompt_frame_regex: Option<Regex>,
    /// Whether a one-row draft in the prompt is saved, the prompt cleared, and the draft typed
    /// back when the agent wakes, instead of refusing to sleep.
    pub sleep_with_draft: bool,
}

impl Profile {
    /// Full command line that resumes `sid`: `bin` then `resume` with `{sid}`
    /// replaced.
    pub fn resume_argv(&self, sid: &str) -> Vec<String> {
        std::iter::once(self.bin.clone())
            .chain(
                self.resume
                    .iter()
                    .map(|arg| arg.replace(SESSION_PLACEHOLDER, sid)),
            )
            .collect()
    }
}

struct BuiltinProfile {
    kind: &'static str,
    bin: &'static str,
    exit: &'static str,
    resume: [&'static str; 2],
    prompt_regex: &'static str,
}

/// Verified: this is the setup the end-to-end suite and the maintainer run.
const CLAUDE: BuiltinProfile = BuiltinProfile {
    kind: "claude",
    bin: "claude",
    exit: "/exit",
    resume: ["--resume", SESSION_PLACEHOLDER],
    prompt_regex: r"^\s*\u{276F}",
};

/// Best effort, not verified against the real agent.
const CODEX: BuiltinProfile = BuiltinProfile {
    kind: "codex",
    bin: "codex",
    exit: "/exit",
    resume: ["resume", SESSION_PLACEHOLDER],
    prompt_regex: r"^\s*\u{203A}",
};

/// Best effort, not verified against the real agent.
const CURSOR: BuiltinProfile = BuiltinProfile {
    kind: "cursor",
    bin: "cursor-agent",
    exit: "/quit",
    resume: ["--resume", SESSION_PLACEHOLDER],
    prompt_regex: r"^\s*[\u{2192}>]",
};

/// Best effort, not verified against the real agent.
const GEMINI: BuiltinProfile = BuiltinProfile {
    kind: "gemini",
    bin: "gemini",
    exit: "/quit",
    resume: ["--resume", SESSION_PLACEHOLDER],
    prompt_regex: r"^\s*>",
};

const BUILTINS: [&BuiltinProfile; 4] = [&CLAUDE, &CODEX, &CURSOR, &GEMINI];

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The validated configuration.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub sleep_after_minutes: u64,
    pub sleep_statuses: Vec<AgentStatus>,
    pub dry_run: bool,
    pub sleep_icon: String,
    /// Icon that prefixes the label of a tab kept awake; empty turns the feature off.
    pub keep_awake_icon: String,
    pub token_name: String,
    pub tab_prefix: bool,
    pub preview_lines: u32,
    pub notify: bool,
    pub include_names: Vec<String>,
    pub exclude_names: Vec<String>,
    #[serde(serialize_with = "ser_regexes")]
    pub exclude_cwd_regex: Vec<Regex>,
    pub protect_env_vars: Vec<String>,
    #[serde(serialize_with = "ser_regexes")]
    pub ignore_child_regex: Vec<Regex>,
    #[serde(serialize_with = "ser_regexes")]
    pub exclude_name_regex: Vec<Regex>,
    #[serde(serialize_with = "ser_regexes")]
    pub exclude_screen_regex: Vec<Regex>,
    #[serde(serialize_with = "ser_regexes")]
    pub exclude_cmdline_regex: Vec<Regex>,
    pub sweep_interval_seconds: u64,
    pub restore_settle_timeout_seconds: u64,
    /// Default of every Profile's `prompt_area_lines`.
    pub prompt_area_lines: usize,
    /// Default of every Profile's `prompt_frame_regex`.
    #[serde(serialize_with = "ser_opt_regex")]
    pub prompt_frame_regex: Option<Regex>,
    /// Default of every Profile's `sleep_with_draft`.
    pub sleep_with_draft: bool,
    /// Managed Profiles by Kind: built-ins with overrides plus custom kinds.
    pub profiles: BTreeMap<String, Profile>,
    /// Non-fatal findings (unknown keys, unmanaged custom kinds).
    pub warnings: Vec<String>,
}

fn ser_regex<S: Serializer>(re: &Regex, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(re.as_str())
}

fn ser_opt_regex<S: Serializer>(re: &Option<Regex>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(re.as_ref().map_or("", Regex::as_str))
}

fn ser_regexes<S: Serializer>(res: &[Regex], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(res.iter().map(Regex::as_str))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    sleep_after_minutes: Option<u64>,
    sleep_statuses: Option<Vec<String>>,
    dry_run: Option<bool>,
    sleep_icon: Option<String>,
    keep_awake_icon: Option<String>,
    token_name: Option<String>,
    tab_prefix: Option<bool>,
    preview_lines: Option<i64>,
    notify: Option<bool>,
    include_names: Option<Vec<String>>,
    exclude_names: Option<Vec<String>>,
    exclude_cwd_regex: Option<Vec<String>>,
    protect_env_vars: Option<Vec<String>>,
    ignore_child_regex: Option<Vec<String>>,
    exclude_name_regex: Option<Vec<String>>,
    exclude_screen_regex: Option<Vec<String>>,
    exclude_cmdline_regex: Option<Vec<String>>,
    prompt_area_lines: Option<i64>,
    prompt_frame_regex: Option<String>,
    sleep_with_draft: Option<bool>,
    sweep_interval_seconds: Option<i64>,
    restore_settle_timeout_seconds: Option<i64>,
    agents: BTreeMap<String, RawAgent>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawAgent {
    bin: Option<String>,
    exit: Option<String>,
    resume: Option<Vec<String>>,
    prompt_regex: Option<String>,
    prompt_area_lines: Option<i64>,
    prompt_frame_regex: Option<String>,
    sleep_with_draft: Option<bool>,
}

/// The prompt detection settings every Profile starts from.
struct PromptDefaults {
    area_lines: usize,
    frame_regex: Option<Regex>,
    sleep_with_draft: bool,
}

fn check_area_lines(key: &str, lines: i64) -> Result<usize, ConfigError> {
    if PROMPT_AREA_LINES_RANGE.contains(&lines) {
        Ok(lines as usize)
    } else {
        Err(ConfigError::key(
            key,
            format!(
                "{lines} is out of range {}..={}",
                PROMPT_AREA_LINES_RANGE.start(),
                PROMPT_AREA_LINES_RANGE.end()
            ),
        ))
    }
}

/// An empty pattern turns the frame detection off.
fn compile_frame(key: &str, pattern: &str) -> Result<Option<Regex>, ConfigError> {
    if pattern.is_empty() {
        Ok(None)
    } else {
        compile(key, pattern).map(Some)
    }
}

/// Compiles `pattern` case-insensitively, naming `key` on failure.
fn compile(key: &str, pattern: &str) -> Result<Regex, ConfigError> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|e| {
            ConfigError::key(
                key,
                format!("invalid regex `{pattern}`: {}", first_line(&e.to_string())),
            )
        })
}

fn first_line(s: &str) -> &str {
    s.lines().last().unwrap_or(s).trim()
}

fn compile_all(key: &str, patterns: &[String]) -> Result<Vec<Regex>, ConfigError> {
    patterns.iter().map(|p| compile(key, p)).collect()
}

fn is_identifier(s: &str, extra: &[char]) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

fn check_non_empty(key: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        Err(ConfigError::key(key, "must not be empty"))
    } else {
        Ok(())
    }
}

fn check_resume(key: &str, resume: &[String]) -> Result<(), ConfigError> {
    if resume.iter().any(|a| a.contains(SESSION_PLACEHOLDER)) {
        Ok(())
    } else {
        Err(ConfigError::key(
            key,
            format!("must contain an argument with `{SESSION_PLACEHOLDER}`"),
        ))
    }
}

fn parse_statuses(values: &[String]) -> Result<Vec<AgentStatus>, ConfigError> {
    let mut out = Vec::new();
    for value in values {
        let status = match value.as_str() {
            "idle" => AgentStatus::Idle,
            "done" => AgentStatus::Done,
            "working" | "blocked" | "unknown" => {
                return Err(ConfigError::key(
                    "sleep_statuses",
                    format!("`{value}` is not allowed, only `idle` and `done` can be put to sleep"),
                ));
            }
            other => {
                return Err(ConfigError::key(
                    "sleep_statuses",
                    format!("unknown status `{other}`, expected `idle` or `done`"),
                ));
            }
        };
        if !out.contains(&status) {
            out.push(status);
        }
    }
    Ok(out)
}

fn unknown_key_warnings(table: &toml::Table) -> Vec<String> {
    let mut warnings = Vec::new();
    for (key, value) in table {
        if !TOP_LEVEL_KEYS.contains(&key.as_str()) {
            warnings.push(format!("unknown key `{key}` ignored"));
        }
        if key == "agents"
            && let Some(agents) = value.as_table()
        {
            for (kind, agent) in agents {
                let Some(agent) = agent.as_table() else {
                    continue;
                };
                for field in agent.keys() {
                    if !AGENT_KEYS.contains(&field.as_str()) {
                        warnings.push(format!("unknown key `agents.{kind}.{field}` ignored"));
                    }
                }
            }
        }
    }
    warnings
}

impl Config {
    /// Loads `config.toml` from [`config_dir`] using the process environment.
    /// A missing file yields the defaults.
    pub fn load() -> Result<Config, ConfigError> {
        Self::load_with_env(process_env)
    }

    /// Like [`Config::load`] with an explicit environment lookup.
    pub fn load_with_env(env: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        match config_dir(env) {
            Some(dir) => Self::load_from(&dir.join(CONFIG_FILE_NAME)),
            None => Ok(Config::default()),
        }
    }

    /// Loads one file; a missing file yields the defaults.
    pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, Some(path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(ConfigError {
                path: Some(path.to_path_buf()),
                key: None,
                message: format!("cannot read the file: {e}"),
            }),
        }
    }

    /// Parses and validates configuration text.
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigError> {
        Self::parse(text, None)
    }

    fn parse(text: &str, path: Option<&Path>) -> Result<Config, ConfigError> {
        let wrap = |message: String| ConfigError {
            path: path.map(Path::to_path_buf),
            key: None,
            message,
        };
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| wrap(format!("invalid TOML: {e}")))?;
        let warnings = unknown_key_warnings(&table);
        let raw: RawConfig = table
            .try_into()
            .map_err(|e: toml::de::Error| wrap(format!("invalid value: {e}")))?;
        Self::from_raw(raw, warnings).map_err(|e| e.with_path(path))
    }

    fn from_raw(raw: RawConfig, mut warnings: Vec<String>) -> Result<Config, ConfigError> {
        let sleep_statuses = match &raw.sleep_statuses {
            Some(values) => parse_statuses(values)?,
            None => AgentStatus::SETTLED.to_vec(),
        };

        let token_name = raw.token_name.unwrap_or_else(|| DEFAULT_TOKEN_NAME.into());
        if !is_identifier(&token_name, &['_', '-']) {
            return Err(ConfigError::key(
                "token_name",
                format!("`{token_name}` must be non-empty and match [A-Za-z0-9_-]+"),
            ));
        }

        let preview_lines = raw.preview_lines.unwrap_or(DEFAULT_PREVIEW_LINES.into());
        if !(0..=MAX_PREVIEW_LINES).contains(&preview_lines) {
            return Err(ConfigError::key(
                "preview_lines",
                format!("{preview_lines} is out of range 0..={MAX_PREVIEW_LINES}"),
            ));
        }

        let sweep = raw
            .sweep_interval_seconds
            .unwrap_or(DEFAULT_SWEEP_INTERVAL_SECONDS as i64);
        if !SWEEP_INTERVAL_RANGE.contains(&sweep) {
            return Err(ConfigError::key(
                "sweep_interval_seconds",
                format!(
                    "{sweep} is out of range {}..={}",
                    SWEEP_INTERVAL_RANGE.start(),
                    SWEEP_INTERVAL_RANGE.end()
                ),
            ));
        }

        let settle = raw
            .restore_settle_timeout_seconds
            .unwrap_or(DEFAULT_RESTORE_SETTLE_TIMEOUT_SECONDS as i64);
        if !RESTORE_SETTLE_TIMEOUT_RANGE.contains(&settle) {
            return Err(ConfigError::key(
                "restore_settle_timeout_seconds",
                format!(
                    "{settle} is out of range {}..={}",
                    RESTORE_SETTLE_TIMEOUT_RANGE.start(),
                    RESTORE_SETTLE_TIMEOUT_RANGE.end()
                ),
            ));
        }

        let protect_env_vars = raw.protect_env_vars.unwrap_or_default();
        for name in &protect_env_vars {
            let valid = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && is_identifier(name, &['_']);
            if !valid {
                return Err(ConfigError::key(
                    "protect_env_vars",
                    format!("`{name}` is not a valid environment variable name"),
                ));
            }
        }

        let ignore_child_regex = match &raw.ignore_child_regex {
            Some(patterns) => compile_all("ignore_child_regex", patterns)?,
            None => default_ignore_child_regex(),
        };
        let exclude_cwd_regex = compile_all(
            "exclude_cwd_regex",
            &raw.exclude_cwd_regex.unwrap_or_default(),
        )?;
        let exclude_name_regex = compile_all(
            "exclude_name_regex",
            &raw.exclude_name_regex.unwrap_or_default(),
        )?;
        let exclude_screen_regex = compile_all(
            "exclude_screen_regex",
            &raw.exclude_screen_regex.unwrap_or_default(),
        )?;
        let exclude_cmdline_regex = compile_all(
            "exclude_cmdline_regex",
            &raw.exclude_cmdline_regex.unwrap_or_default(),
        )?;

        let prompt_area_lines = check_area_lines(
            "prompt_area_lines",
            raw.prompt_area_lines
                .unwrap_or(DEFAULT_PROMPT_AREA_LINES as i64),
        )?;
        let prompt_frame_regex = match &raw.prompt_frame_regex {
            Some(pattern) => compile_frame("prompt_frame_regex", pattern)?,
            None => Some(compile("prompt_frame_regex", DEFAULT_PROMPT_FRAME_REGEX)?),
        };
        let sleep_with_draft = raw.sleep_with_draft.unwrap_or(false);
        let defaults = PromptDefaults {
            area_lines: prompt_area_lines,
            frame_regex: prompt_frame_regex.clone(),
            sleep_with_draft,
        };

        let profiles = build_profiles(raw.agents, &defaults, &mut warnings)?;

        Ok(Config {
            sleep_after_minutes: raw
                .sleep_after_minutes
                .unwrap_or(DEFAULT_SLEEP_AFTER_MINUTES),
            sleep_statuses,
            dry_run: raw.dry_run.unwrap_or(false),
            sleep_icon: raw.sleep_icon.unwrap_or_else(|| DEFAULT_SLEEP_ICON.into()),
            keep_awake_icon: raw
                .keep_awake_icon
                .unwrap_or_else(|| DEFAULT_KEEP_AWAKE_ICON.into()),
            token_name,
            tab_prefix: raw.tab_prefix.unwrap_or(true),
            preview_lines: preview_lines as u32,
            notify: raw.notify.unwrap_or(true),
            include_names: raw.include_names.unwrap_or_default(),
            exclude_names: raw.exclude_names.unwrap_or_default(),
            exclude_cwd_regex,
            protect_env_vars,
            ignore_child_regex,
            exclude_name_regex,
            exclude_screen_regex,
            exclude_cmdline_regex,
            sweep_interval_seconds: sweep as u64,
            restore_settle_timeout_seconds: settle as u64,
            prompt_area_lines,
            prompt_frame_regex,
            sleep_with_draft,
            profiles,
            warnings,
        })
    }

    /// Idle time after which a Settled agent goes to sleep.
    pub fn sleep_after(&self) -> Duration {
        Duration::from_secs(self.sleep_after_minutes.saturating_mul(60))
    }

    /// Longest wait of `restore`, per pane and per pass, for a pane to be
    /// ready for its Placeholder.
    pub fn restore_settle_timeout(&self) -> Duration {
        Duration::from_secs(self.restore_settle_timeout_seconds)
    }

    /// Whether an agent in `status` may be put to sleep.
    pub fn allows_status(&self, status: AgentStatus) -> bool {
        self.sleep_statuses.contains(&status)
    }

    /// Applies `include_names` (when non-empty, the name must be listed) and
    /// `exclude_names`. An agent without a name only passes when
    /// `include_names` is empty.
    pub fn name_allowed(&self, name: Option<&str>) -> bool {
        let listed = |list: &[String]| name.is_some_and(|n| list.iter().any(|x| x == n));
        (self.include_names.is_empty() || listed(&self.include_names))
            && !listed(&self.exclude_names)
    }

    /// Whether `cwd` matches `exclude_cwd_regex`.
    pub fn cwd_excluded(&self, cwd: &str) -> bool {
        self.exclude_cwd_regex.iter().any(|re| re.is_match(cwd))
    }

    /// Whether a child process command line is ignored when looking for busy
    /// children.
    pub fn child_ignored(&self, command: &str) -> bool {
        self.ignore_child_regex
            .iter()
            .any(|re| re.is_match(command))
    }

    /// Whether `name` matches `exclude_name_regex`.
    pub fn name_excluded(&self, name: &str) -> bool {
        self.exclude_name_regex.iter().any(|re| re.is_match(name))
    }

    /// Whether `command` matches `exclude_cmdline_regex`.
    pub fn cmdline_excluded(&self, command: &str) -> bool {
        self.exclude_cmdline_regex
            .iter()
            .any(|re| re.is_match(command))
    }

    /// Whether a line of the agent's screen matches `exclude_screen_regex`.
    pub fn screen_excluded(&self, line: &str) -> bool {
        self.exclude_screen_regex.iter().any(|re| re.is_match(line))
    }

    /// The Profile of `kind`, when the Kind is managed.
    pub fn profile(&self, kind: &str) -> Option<&Profile> {
        self.profiles.get(kind)
    }

    /// Whether `kind` has a Profile (Managed agent).
    pub fn is_managed(&self, kind: &str) -> bool {
        self.profiles.contains_key(kind)
    }

    /// Annotated default `config.toml`. Default settings are lines starting
    /// with `#` directly followed by the key, so removing that `#` activates
    /// them; prose comments start with `# `.
    pub fn to_toml_commented_default() -> String {
        let children = DEFAULT_IGNORE_CHILD_REGEX
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"# Herdr Sleep configuration. Every key is optional; the values below are the defaults.

# Minutes an agent must stay settled, unfocused and unchanged before it sleeps.
#sleep_after_minutes = {DEFAULT_SLEEP_AFTER_MINUTES}

# Statuses that may be put to sleep. Only "idle" and "done" are accepted.
#sleep_statuses = ["idle", "done"]

# Report what would happen without putting anything to sleep.
#dry_run = false

# Icon shown in the sidebar for a sleeping agent.
#sleep_icon = "{DEFAULT_SLEEP_ICON}"

# Icon that prefixes the label of a tab kept awake with the keep-awake action. No agent of such a
# tab goes to sleep. Empty turns the feature off.
#keep_awake_icon = "{DEFAULT_KEEP_AWAKE_ICON}"

# Name of the sidebar token (letters, digits, "_" and "-").
#token_name = "{DEFAULT_TOKEN_NAME}"

# Prefix the tab title of a sleeping agent with the icon.
#tab_prefix = true

# Lines of the screen kept as the preview (0 to 200).
#preview_lines = {DEFAULT_PREVIEW_LINES}

# Send a Herdr notification when an agent goes to sleep.
#notify = true

# When non-empty, only agents with one of these names may sleep.
#include_names = []

# Agents with one of these names never sleep.
#exclude_names = []

# Agents whose working directory matches one of these regexes never sleep (case-insensitive).
#exclude_cwd_regex = []

# Agents whose process environment defines one of these variables never sleep.
#protect_env_vars = []

# Child processes matching one of these regexes do not count as work (case-insensitive).
# Setting this key replaces the built-in list.
#ignore_child_regex = [{children}]

# Seconds between two sweeps of the watcher (5 to 3600).
#sweep_interval_seconds = {DEFAULT_SWEEP_INTERVAL_SECONDS}

# Seconds `restore` waits, per pane and per pass, for the pane's shell and for Herdr to
# release the native agent identity after a Herdr restart (5 to 600).
#restore_settle_timeout_seconds = {DEFAULT_RESTORE_SETTLE_TIMEOUT_SECONDS}

# Agents whose name matches one of these regexes never sleep (case-insensitive).
#exclude_name_regex = []

# Agents whose last screen lines match one of these regexes never sleep (case-insensitive),
# for example "waiting for approval" or "running tests".
#exclude_screen_regex = []

# Agents whose command line, or the command line of one of their child processes (even an
# ignored one), matches one of these regexes never sleep (case-insensitive).
#exclude_cmdline_regex = []

# Bottom screen rows searched for the prompt when no frame is found (5 to 40). Also settable
# per agent kind: [agents.<kind>] prompt_area_lines.
#prompt_area_lines = {DEFAULT_PROMPT_AREA_LINES}

# A line matching this regex is a rule drawn above and below the prompt; the prompt is looked
# for between the last two rules of the screen. Empty turns the frame detection off. Also
# settable per agent kind.
#prompt_frame_regex = '{DEFAULT_PROMPT_FRAME_REGEX}'

# Put an agent to sleep even when a one-row draft is typed in its prompt: the draft is saved,
# the prompt cleared, and the draft typed back (without Enter) when the agent wakes. A draft
# over several rows, or a pasted text, still keeps the agent awake. Also settable per agent kind.
#sleep_with_draft = false

# Per-kind profiles. A table overrides single fields of a built-in kind (claude,
# codex, cursor, gemini) or defines a new kind, which needs all four fields.
# [agents.myagent]
# bin = "myagent"
# exit = "/quit"
# resume = ["--resume", "{{sid}}"]
# prompt_regex = '^\s*>'
"#
        )
    }
}

fn default_ignore_child_regex() -> Vec<Regex> {
    DEFAULT_IGNORE_CHILD_REGEX
        .iter()
        .map(|p| compile("ignore_child_regex", p).expect("built-in pattern compiles"))
        .collect()
}

fn build_profiles(
    agents: BTreeMap<String, RawAgent>,
    defaults: &PromptDefaults,
    warnings: &mut Vec<String>,
) -> Result<BTreeMap<String, Profile>, ConfigError> {
    let mut raws = agents;
    let mut profiles = BTreeMap::new();

    for b in BUILTINS {
        let over = raws.remove(b.kind).unwrap_or_default();
        let key = |field: &str| format!("agents.{}.{field}", b.kind);
        let profile = Profile {
            kind: b.kind.to_string(),
            bin: checked(&key("bin"), over.bin, || b.bin.to_string())?,
            exit: checked(&key("exit"), over.exit, || b.exit.to_string())?,
            resume: match over.resume {
                Some(resume) => {
                    check_resume(&key("resume"), &resume)?;
                    resume
                }
                None => b.resume.iter().map(|s| s.to_string()).collect(),
            },
            prompt_regex: compile(
                &key("prompt_regex"),
                over.prompt_regex.as_deref().unwrap_or(b.prompt_regex),
            )?,
            prompt_area_lines: prompt_area_lines(&key, over.prompt_area_lines, defaults)?,
            prompt_frame_regex: prompt_frame_regex(&key, over.prompt_frame_regex, defaults)?,
            sleep_with_draft: over.sleep_with_draft.unwrap_or(defaults.sleep_with_draft),
        };
        profiles.insert(profile.kind.clone(), profile);
    }

    for (kind, over) in raws {
        if kind.trim().is_empty() {
            return Err(ConfigError::key("agents", "kind name must not be empty"));
        }
        let key = |field: &str| format!("agents.{kind}.{field}");
        let bin = over
            .bin
            .map(|v| check_non_empty(&key("bin"), &v).map(|_| v))
            .transpose()?;
        let exit = over
            .exit
            .map(|v| check_non_empty(&key("exit"), &v).map(|_| v))
            .transpose()?;
        if let Some(resume) = &over.resume {
            check_resume(&key("resume"), resume)?;
        }
        let prompt_regex = over
            .prompt_regex
            .as_deref()
            .map(|p| compile(&key("prompt_regex"), p))
            .transpose()?;

        let area_lines = prompt_area_lines(&key, over.prompt_area_lines, defaults)?;
        let frame_regex = prompt_frame_regex(&key, over.prompt_frame_regex.clone(), defaults)?;

        let missing: Vec<&str> = [
            ("bin", bin.is_none()),
            ("exit", exit.is_none()),
            ("resume", over.resume.is_none()),
            ("prompt_regex", prompt_regex.is_none()),
        ]
        .into_iter()
        .filter_map(|(name, absent)| absent.then_some(name))
        .collect();
        if let (Some(bin), Some(exit), Some(resume), Some(prompt_regex)) =
            (bin, exit, over.resume, prompt_regex)
        {
            profiles.insert(
                kind.clone(),
                Profile {
                    kind,
                    bin,
                    exit,
                    resume,
                    prompt_regex,
                    prompt_area_lines: area_lines,
                    prompt_frame_regex: frame_regex,
                    sleep_with_draft: over.sleep_with_draft.unwrap_or(defaults.sleep_with_draft),
                },
            );
        } else {
            warnings.push(format!(
                "agents.{kind} is not managed: missing {}",
                missing.join(", ")
            ));
        }
    }
    Ok(profiles)
}

fn prompt_area_lines(
    key: &impl Fn(&str) -> String,
    own: Option<i64>,
    defaults: &PromptDefaults,
) -> Result<usize, ConfigError> {
    match own {
        Some(lines) => check_area_lines(&key("prompt_area_lines"), lines),
        None => Ok(defaults.area_lines),
    }
}

fn prompt_frame_regex(
    key: &impl Fn(&str) -> String,
    own: Option<String>,
    defaults: &PromptDefaults,
) -> Result<Option<Regex>, ConfigError> {
    match own {
        Some(pattern) => compile_frame(&key("prompt_frame_regex"), &pattern),
        None => Ok(defaults.frame_regex.clone()),
    }
}

fn checked(
    key: &str,
    value: Option<String>,
    default: impl FnOnce() -> String,
) -> Result<String, ConfigError> {
    match value {
        Some(v) => check_non_empty(key, &v).map(|_| v),
        None => Ok(default()),
    }
}

impl Default for Config {
    fn default() -> Self {
        Config::from_raw(RawConfig::default(), Vec::new()).expect("defaults are valid")
    }
}

/// Kinds that ship with a built-in Profile.
pub fn builtin_kinds() -> BTreeSet<&'static str> {
    BUILTINS.iter().map(|p| p.kind).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        Config::from_toml_str(text).unwrap()
    }

    fn err(text: &str) -> String {
        Config::from_toml_str(text).unwrap_err().to_string()
    }

    fn json(c: &Config) -> serde_json::Value {
        serde_json::to_value(c).unwrap()
    }

    #[test]
    fn defaults() {
        let c = Config::default();
        assert_eq!(c.sleep_after(), Duration::from_secs(900));
        assert_eq!(c.sleep_statuses, vec![AgentStatus::Idle, AgentStatus::Done]);
        assert!(!c.dry_run);
        assert_eq!(c.sleep_icon, "💤");
        assert_eq!(c.token_name, "sleep");
        assert!(c.tab_prefix && c.notify);
        assert_eq!(c.preview_lines, 20);
        assert_eq!(c.sweep_interval_seconds, 60);
        assert_eq!(c.restore_settle_timeout_seconds, 60);
        assert_eq!(c.restore_settle_timeout(), Duration::from_secs(60));
        assert!(c.include_names.is_empty() && c.exclude_names.is_empty());
        assert!(c.protect_env_vars.is_empty() && c.exclude_cwd_regex.is_empty());
        assert_eq!(c.ignore_child_regex.len(), 8);
        assert!(c.warnings.is_empty());
        for kind in ["claude", "codex", "cursor", "gemini"] {
            assert!(c.is_managed(kind));
        }
        assert!(!c.is_managed("aider"));
        assert_eq!(builtin_kinds().len(), 4);
        assert_eq!(cfg("").sleep_after_minutes, 15);
    }

    #[test]
    fn every_key_override() {
        let c = cfg(r#"
            sleep_after_minutes = 0
            sleep_statuses = ["done"]
            dry_run = true
            sleep_icon = "z"
            token_name = "zzz-1_a"
            tab_prefix = false
            preview_lines = 200
            notify = false
            include_names = ["a"]
            exclude_names = ["b"]
            exclude_cwd_regex = ["^/tmp"]
            protect_env_vars = ["_X1"]
            ignore_child_regex = ["foo"]
            sweep_interval_seconds = 3600
        "#);
        assert_eq!(c.sleep_after(), Duration::ZERO);
        assert!(c.allows_status(AgentStatus::Done));
        assert!(!c.allows_status(AgentStatus::Idle));
        assert!(c.dry_run && !c.tab_prefix && !c.notify);
        assert_eq!(c.sleep_icon, "z");
        assert_eq!(c.token_name, "zzz-1_a");
        assert_eq!(c.preview_lines, 200);
        assert_eq!(c.protect_env_vars, vec!["_X1"]);
        assert_eq!(c.sweep_interval_seconds, 3600);
        assert!(c.name_allowed(Some("a")));
        assert!(!c.name_allowed(Some("c")));
        assert!(!c.name_allowed(None));
        assert!(c.cwd_excluded("/TMP/x"));
        assert!(!c.cwd_excluded("/home"));
    }

    #[test]
    fn name_filters() {
        let c = cfg(r#"exclude_names = ["b"]"#);
        assert!(c.name_allowed(None));
        assert!(c.name_allowed(Some("a")));
        assert!(!c.name_allowed(Some("b")));
        let c = cfg(r#"include_names = ["b"]
exclude_names = ["b"]"#);
        assert!(!c.name_allowed(Some("b")));
    }

    #[test]
    fn exclusion_regexes_are_empty_by_default_and_case_insensitive() {
        let c = Config::default();
        assert!(c.exclude_name_regex.is_empty());
        assert!(c.exclude_screen_regex.is_empty());
        assert!(c.exclude_cmdline_regex.is_empty());
        assert!(!c.name_excluded("deploy") && !c.screen_excluded("x") && !c.cmdline_excluded("x"));
        let c = cfg(r#"
            exclude_name_regex = ["^deploy"]
            exclude_screen_regex = ["waiting for approval"]
            exclude_cmdline_regex = ["cargo (test|build)"]
        "#);
        assert!(c.warnings.is_empty());
        assert!(c.name_excluded("Deploy-prod") && !c.name_excluded("redeploy"));
        assert!(c.screen_excluded("  WAITING for Approval  ") && !c.screen_excluded("done"));
        assert!(c.cmdline_excluded("Cargo TEST --all") && !c.cmdline_excluded("cargo fmt"));
    }

    #[test]
    fn invalid_exclusion_regexes_name_their_key() {
        for key in [
            "exclude_name_regex",
            "exclude_screen_regex",
            "exclude_cmdline_regex",
        ] {
            let e = err(&format!(r#"{key} = ["ok", "("]"#));
            assert!(e.contains(key) && e.contains("`(`"), "{e}");
        }
    }

    #[test]
    fn prompt_detection_defaults() {
        let c = Config::default();
        assert_eq!(c.prompt_area_lines, 15);
        let frame = c.prompt_frame_regex.as_ref().unwrap();
        assert!(frame.is_match(&"\u{2500}".repeat(20)));
        assert!(frame.is_match(&format!("  {}  ", "\u{2500}".repeat(10))));
        assert!(!frame.is_match(&"\u{2500}".repeat(9)));
        assert!(!frame.is_match("\u{276F} \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}"));
        assert!(!frame.is_match("          "));
        let claude = c.profile("claude").unwrap();
        assert_eq!(claude.prompt_area_lines, 15);
        assert!(claude.prompt_frame_regex.is_some());
    }

    #[test]
    fn prompt_detection_keys_are_global_with_per_profile_overrides() {
        let c = cfg(r#"
            prompt_area_lines = 30
            prompt_frame_regex = "^=+$"
            [agents.codex]
            prompt_area_lines = 8
            prompt_frame_regex = ""
            [agents.mine]
            bin = "m"
            exit = "/q"
            resume = ["-r", "{sid}"]
            prompt_regex = "^>"
        "#);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.prompt_area_lines, 30);
        let claude = c.profile("claude").unwrap();
        assert_eq!(claude.prompt_area_lines, 30);
        assert_eq!(claude.prompt_frame_regex.as_ref().unwrap().as_str(), "^=+$");
        let codex = c.profile("codex").unwrap();
        assert_eq!(codex.prompt_area_lines, 8);
        assert!(codex.prompt_frame_regex.is_none());
        assert_eq!(c.profile("mine").unwrap().prompt_area_lines, 30);
        // An empty global pattern turns the detection off for every profile.
        let off = cfg(r#"prompt_frame_regex = """#);
        assert!(off.prompt_frame_regex.is_none());
        assert!(off.profile("claude").unwrap().prompt_frame_regex.is_none());
    }

    #[test]
    fn prompt_detection_keys_are_validated() {
        for text in ["prompt_area_lines = 4", "prompt_area_lines = 41"] {
            assert!(err(text).contains("prompt_area_lines"), "{text}");
        }
        assert_eq!(cfg("prompt_area_lines = 5").prompt_area_lines, 5);
        assert_eq!(cfg("prompt_area_lines = 40").prompt_area_lines, 40);
        assert!(err(r#"prompt_frame_regex = "(""#).contains("prompt_frame_regex"));
        let e = err("[agents.claude]\nprompt_frame_regex = \"(\"\n");
        assert!(e.contains("agents.claude.prompt_frame_regex"), "{e}");
        let e = err("[agents.claude]\nprompt_area_lines = 99\n");
        assert!(e.contains("agents.claude.prompt_area_lines"), "{e}");
    }

    #[test]
    fn rejects_non_sleepable_statuses() {
        let e = err(r#"sleep_statuses = ["idle", "working"]"#);
        assert!(e.contains("sleep_statuses") && e.contains("working"), "{e}");
        assert!(err(r#"sleep_statuses = ["blocked"]"#).contains("sleep_statuses"));
        assert!(err(r#"sleep_statuses = ["nope"]"#).contains("nope"));
    }

    #[test]
    fn bounds_and_names() {
        assert!(err("preview_lines = 201").contains("preview_lines"));
        assert!(err("preview_lines = -1").contains("preview_lines"));
        assert!(err("sweep_interval_seconds = 4").contains("sweep_interval_seconds"));
        assert!(
            err("restore_settle_timeout_seconds = 4").contains("restore_settle_timeout_seconds")
        );
        assert!(
            err("restore_settle_timeout_seconds = 601").contains("restore_settle_timeout_seconds")
        );
        assert!(err("sweep_interval_seconds = 3601").contains("sweep_interval_seconds"));
        assert!(err(r#"token_name = "a b""#).contains("token_name"));
        assert!(err(r#"token_name = """#).contains("token_name"));
        assert!(err(r#"protect_env_vars = ["1ABC"]"#).contains("protect_env_vars"));
        assert!(err(r#"protect_env_vars = ["A-B"]"#).contains("`A-B`"));
        assert_eq!(cfg("preview_lines = 0").preview_lines, 0);
    }

    #[test]
    fn invalid_toml_and_types_name_the_file() {
        let e = Config::parse(
            "sleep_after_minutes = \"x\"",
            Some(Path::new("/c/config.toml")),
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("/c/config.toml") && e.contains("sleep_after_minutes"),
            "{e}"
        );
        assert!(err("not toml [").contains("invalid TOML"));
    }

    #[test]
    fn regexes() {
        let e = err(r#"exclude_cwd_regex = ["("]"#);
        assert!(e.contains("exclude_cwd_regex") && e.contains("`(`"), "{e}");
        let e = err(r#"ignore_child_regex = ["ok", "[z"]"#);
        assert!(e.contains("ignore_child_regex") && e.contains("[z"), "{e}");
        let c = cfg(r#"exclude_cwd_regex = ["^/Work"]"#);
        assert!(c.cwd_excluded("/work/x"));
    }

    #[test]
    fn ignore_child_regex_replaces_builtins() {
        let c = cfg(r#"ignore_child_regex = ["^only$"]"#);
        assert!(c.child_ignored("ONLY"));
        assert!(!c.child_ignored("lazy-mcp"));
        assert!(!cfg("ignore_child_regex = []").child_ignored("lazy-mcp"));
    }

    #[test]
    fn default_child_regexes_compile_with_unicode_case() {
        let c = Config::default();
        assert!(c.child_ignored("/Users/x/.claude/mcp/bin/lazy-mcp --name chrome"));
        assert!(c.child_ignored("(grep)"));
        assert!(c.child_ignored("NPM EXEC foo"));
        assert!(!c.child_ignored("node server.js"));
    }

    #[test]
    fn unknown_keys_warn() {
        let c = cfg("bogus = 1\n[agents.claude]\nbin = \"c\"\nwhat = 2\n");
        assert_eq!(c.warnings.len(), 2, "{:?}", c.warnings);
        assert!(c.warnings.iter().any(|w| w.contains("`bogus`")));
        assert!(c.warnings.iter().any(|w| w.contains("agents.claude.what")));
    }

    #[test]
    fn builtin_profiles() {
        let c = Config::default();
        let claude = c.profile("claude").unwrap();
        assert_eq!(claude.resume_argv("S1"), ["claude", "--resume", "S1"]);
        assert!(claude.prompt_regex.is_match("  \u{276F} hi"));
        assert_eq!(
            c.profile("codex").unwrap().resume_argv("S"),
            ["codex", "resume", "S"]
        );
        assert_eq!(c.profile("cursor").unwrap().bin, "cursor-agent");
        assert!(
            c.profile("cursor")
                .unwrap()
                .prompt_regex
                .is_match("\u{2192} x")
        );
        assert_eq!(c.profile("gemini").unwrap().exit, "/quit");
        assert!(
            c.profile("codex")
                .unwrap()
                .prompt_regex
                .is_match("\u{203A}")
        );
    }

    #[test]
    fn builtin_override_keeps_other_fields() {
        let c = cfg("[agents.claude]\nbin = \"/opt/claude\"\n");
        let p = c.profile("claude").unwrap();
        assert_eq!(p.bin, "/opt/claude");
        assert_eq!(p.exit, "/exit");
        assert_eq!(p.resume, ["--resume", "{sid}"]);
        assert!(p.prompt_regex.is_match("\u{276F}"));
        assert!(c.warnings.is_empty());
    }

    #[test]
    fn new_kind_complete_and_incomplete() {
        let c = cfg(r#"
            [agents.aider]
            bin = "aider"
            exit = "/exit"
            resume = ["--restore", "{sid}"]
            prompt_regex = '^>'
        "#);
        assert!(c.is_managed("aider"));
        assert_eq!(
            c.profile("aider").unwrap().resume_argv("9"),
            ["aider", "--restore", "9"]
        );

        let c = cfg("[agents.aider]\nbin = \"aider\"\n");
        assert!(!c.is_managed("aider"));
        assert_eq!(c.warnings.len(), 1);
        assert!(c.warnings[0].contains("aider") && c.warnings[0].contains("exit"));
    }

    #[test]
    fn resume_requires_sid() {
        let e = err("[agents.claude]\nresume = [\"--resume\"]\n");
        assert!(
            e.contains("agents.claude.resume") && e.contains("{sid}"),
            "{e}"
        );
        assert!(err("[agents.x]\nresume = [\"a\"]\n").contains("agents.x.resume"));
        assert!(
            err("[agents.claude]\nprompt_regex = \"(\"\n").contains("agents.claude.prompt_regex")
        );
        assert!(err("[agents.claude]\nbin = \"\"\n").contains("agents.claude.bin"));
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn config_dir_precedence() {
        let all = [
            ("HERDR_SLEEP_CONFIG_DIR", "/a"),
            ("HERDR_PLUGIN_CONFIG_DIR", "/b"),
            ("XDG_CONFIG_HOME", "/x"),
            ("HOME", "/h"),
        ];
        assert_eq!(config_dir(env_of(&all)), Some("/a".into()));
        assert_eq!(config_dir(env_of(&all[1..])), Some("/b".into()));
        assert_eq!(
            config_dir(env_of(&all[2..])),
            Some("/x/herdr/plugins/config/pepsit36.herdr-sleep".into())
        );
        assert_eq!(
            config_dir(env_of(&all[3..])),
            Some("/h/.config/herdr/plugins/config/pepsit36.herdr-sleep".into())
        );
        assert_eq!(config_dir(env_of(&[("HERDR_SLEEP_CONFIG_DIR", "")])), None);
        assert_eq!(config_dir(env_of(&[])), None);
    }

    #[test]
    fn state_dir_precedence() {
        let all = [
            ("HERDR_SLEEP_STATE_DIR", "/a"),
            ("HERDR_PLUGIN_STATE_DIR", "/b"),
            ("XDG_STATE_HOME", "/x"),
            ("HOME", "/h"),
        ];
        assert_eq!(state_dir(env_of(&all)), Some("/a".into()));
        assert_eq!(state_dir(env_of(&all[1..])), Some("/b".into()));
        assert_eq!(state_dir(env_of(&all[2..])), Some("/x/herdr-sleep".into()));
        assert_eq!(
            state_dir(env_of(&all[3..])),
            Some("/h/.local/state/herdr-sleep".into())
        );
        assert_eq!(state_dir(env_of(&[])), None);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("herdr-sleep-cfg-{}", std::process::id()));
        let c = Config::load_from(&dir.join("config.toml")).unwrap();
        assert_eq!(json(&c), json(&Config::default()));
        let dir_s = dir.to_string_lossy().into_owned();
        let c = Config::load_with_env(|k| (k == "HERDR_SLEEP_CONFIG_DIR").then(|| dir_s.clone()))
            .unwrap();
        assert_eq!(c.sleep_after_minutes, 15);
    }

    #[test]
    fn sleep_with_draft_is_off_by_default_and_per_agent() {
        let c = Config::default();
        assert!(!c.sleep_with_draft && !c.profile("claude").unwrap().sleep_with_draft);
        let c = Config::from_toml_str(
            "sleep_with_draft = true\n[agents.codex]\nsleep_with_draft = false",
        )
        .unwrap();
        assert!(c.profile("claude").unwrap().sleep_with_draft);
        assert!(!c.profile("codex").unwrap().sleep_with_draft);
    }

    #[test]
    fn load_reads_file_and_names_it_on_error() {
        let dir = std::env::temp_dir().join(format!("herdr-sleep-cfg-rw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CONFIG_FILE_NAME);
        std::fs::write(&file, "sleep_after_minutes = 3\n").unwrap();
        assert_eq!(Config::load_from(&file).unwrap().sleep_after_minutes, 3);
        std::fs::write(&file, "preview_lines = 999\n").unwrap();
        let e = Config::load_from(&file).unwrap_err().to_string();
        assert!(
            e.contains("config.toml") && e.contains("preview_lines"),
            "{e}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn serializes_regexes_as_sources() {
        let v = json(&Config::default());
        assert_eq!(v["ignore_child_regex"][0], "mcp");
        assert_eq!(v["profiles"]["claude"]["prompt_regex"], r"^\s*\u{276F}");
        assert_eq!(v["sleep_statuses"], serde_json::json!(["idle", "done"]));
    }

    #[test]
    fn annotated_sample_is_valid_and_matches_defaults() {
        let sample = Config::to_toml_commented_default();
        // Nothing active: defaults.
        assert_eq!(json(&cfg(&sample)), json(&Config::default()));
        // Every default line activated: still the defaults.
        let active: String = sample
            .lines()
            .map(|l| match l.strip_prefix('#') {
                Some(rest) if !rest.starts_with(' ') && !rest.is_empty() => rest,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(active.contains("sleep_after_minutes = 15"));
        assert!(active.contains("ignore_child_regex = ["));
        let c = cfg(&active);
        assert!(c.warnings.is_empty());
        assert_eq!(json(&c), json(&Config::default()));
    }
}

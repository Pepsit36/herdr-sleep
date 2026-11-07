//! Command line of the Placeholder.
//!
//! Named flags keep the process self-describing in `ps` and in the resume
//! command Herdr stores, and make the order irrelevant. Everything after `--`
//! is the agent's resume command.

pub const USAGE: &str = "\
Usage: herdr-sleep-ph --pane P [--name N] --kind K --session SID --state-dir D
                      --main-bin B --herdr-bin H --shell SH --icon I --token T
                      --cwd C [--preview FILE] -- <agent resume command...>

The Placeholder is started by herdr-sleep inside a sleeping pane. It is not
meant to be run by hand.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit
";

/// Everything the Placeholder needs, received on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub pane: String,
    pub name: Option<String>,
    pub kind: String,
    pub session: String,
    pub state_dir: String,
    pub main_bin: String,
    pub herdr_bin: String,
    pub shell: String,
    pub icon: String,
    pub token: String,
    pub cwd: String,
    pub preview: Option<String>,
    pub resume: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Help,
    Version,
    Run(Box<Args>),
}

/// Parses the arguments that follow the program name.
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut pane = None;
    let mut name = None;
    let mut kind = None;
    let mut session = None;
    let mut state_dir = None;
    let mut main_bin = None;
    let mut herdr_bin = None;
    let mut shell = None;
    let mut icon = None;
    let mut token = None;
    let mut cwd = None;
    let mut preview = None;
    let mut resume = Vec::new();

    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        if flag == "--" {
            resume.extend(iter.by_ref().cloned());
            break;
        }
        let slot = match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--pane" => &mut pane,
            "--name" => &mut name,
            "--kind" => &mut kind,
            "--session" => &mut session,
            "--state-dir" => &mut state_dir,
            "--main-bin" => &mut main_bin,
            "--herdr-bin" => &mut herdr_bin,
            "--shell" => &mut shell,
            "--icon" => &mut icon,
            "--token" => &mut token,
            "--cwd" => &mut cwd,
            "--preview" => &mut preview,
            other => return Err(format!("unknown argument `{other}`")),
        };
        let value = iter
            .next()
            .ok_or_else(|| format!("`{flag}` needs a value"))?;
        *slot = Some(value.clone());
    }

    fn need(value: Option<String>, flag: &str) -> Result<String, String> {
        value.ok_or_else(|| format!("missing `{flag}`"))
    }

    if resume.is_empty() {
        return Err("missing the resume command after `--`".to_string());
    }
    Ok(Parsed::Run(Box::new(Args {
        pane: need(pane, "--pane")?,
        name: name.filter(|n| !n.is_empty()),
        kind: need(kind, "--kind")?,
        session: need(session, "--session")?,
        state_dir: need(state_dir, "--state-dir")?,
        main_bin: need(main_bin, "--main-bin")?,
        herdr_bin: need(herdr_bin, "--herdr-bin")?,
        shell: need(shell, "--shell")?,
        icon: need(icon, "--icon")?,
        token: need(token, "--token")?,
        cwd: need(cwd, "--cwd")?,
        preview,
        resume,
    })))
}

#[cfg(test)]
pub(crate) fn sample() -> Args {
    Args {
        pane: "w1-2".into(),
        name: Some("worker".into()),
        kind: "claude".into(),
        session: "sess-1".into(),
        state_dir: "/state".into(),
        main_bin: "/bin/herdr-sleep".into(),
        herdr_bin: "/bin/herdr".into(),
        shell: "/bin/zsh".into(),
        icon: "z".into(),
        token: "sleep".into(),
        cwd: "/work".into(),
        preview: None,
        resume: vec!["claude".into(), "--resume".into(), "sess-1".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn full() -> Vec<String> {
        v(&[
            "--pane",
            "w1-2",
            "--name",
            "worker",
            "--kind",
            "claude",
            "--session",
            "sess-1",
            "--state-dir",
            "/state",
            "--main-bin",
            "/bin/herdr-sleep",
            "--herdr-bin",
            "/bin/herdr",
            "--shell",
            "/bin/zsh",
            "--icon",
            "z",
            "--token",
            "sleep",
            "--cwd",
            "/work",
            "--",
            "claude",
            "--resume",
            "sess-1",
        ])
    }

    #[test]
    fn parses_every_flag_and_the_resume_command() {
        let Parsed::Run(args) = parse(&full()).unwrap() else {
            panic!("expected Run");
        };
        assert_eq!(*args, sample());
    }

    #[test]
    fn name_and_preview_are_optional() {
        let mut argv = full();
        argv.drain(2..4);
        let Parsed::Run(args) = parse(&argv).unwrap() else {
            panic!("expected Run");
        };
        assert_eq!(args.name, None);
        assert_eq!(args.preview, None);
    }

    #[test]
    fn preview_is_read_when_given() {
        let mut argv = full();
        argv.splice(2..2, v(&["--preview", "/tmp/p"]));
        let Parsed::Run(args) = parse(&argv).unwrap() else {
            panic!("expected Run");
        };
        assert_eq!(args.preview.as_deref(), Some("/tmp/p"));
    }

    #[test]
    fn resume_command_keeps_flag_lookalikes() {
        let mut argv = full();
        argv.extend(v(&["--pane", "--"]));
        let Parsed::Run(args) = parse(&argv).unwrap() else {
            panic!("expected Run");
        };
        assert_eq!(args.pane, "w1-2");
        assert_eq!(args.resume.last().map(String::as_str), Some("--"));
        assert_eq!(args.resume.len(), 5);
    }

    #[test]
    fn rejects_missing_unknown_and_valueless() {
        assert!(parse(&v(&["--pane", "p", "--", "x"])).is_err());
        assert!(
            parse(&v(&["--bogus", "1"]))
                .unwrap_err()
                .contains("--bogus")
        );
        assert!(
            parse(&v(&["--pane"]))
                .unwrap_err()
                .contains("needs a value")
        );
        let mut argv = full();
        argv.truncate(argv.len() - 3);
        assert!(parse(&argv).unwrap_err().contains("resume command"));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(&v(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse(&v(&["-V"])), Ok(Parsed::Version));
    }
}

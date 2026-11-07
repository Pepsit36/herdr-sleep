//! Command line plumbing shared by every command: options, failures, exit codes.

use std::fs;
use std::path::PathBuf;

use crate::changelog::{Changelog, FILE_NAME};
use crate::git;

/// Exit code of `changelog version` when nothing publishes. Not an error.
pub const EXIT_NOTHING_TO_PUBLISH: u8 = 3;

/// How a command ends when it does not end well.
#[derive(Debug)]
pub enum Failure {
    /// Wrong invocation: exit 2, with the usage.
    Usage(String),
    /// Anything else: exit 1, with the explanation.
    Error(String),
}

pub type Outcome = Result<u8, Failure>;

pub fn usage(message: impl Into<String>) -> Failure {
    Failure::Usage(message.into())
}

pub fn error(message: impl Into<String>) -> Failure {
    Failure::Error(message.into())
}

/// Options of a command, every one of them taking a value.
#[derive(Debug, Default)]
pub struct Options {
    values: Vec<(String, String)>,
    positional: Vec<String>,
}

impl Options {
    /// Reads `--name value` and `--name=value` for the names in `accepted`.
    /// Anything that starts with a dash and is not accepted is a usage error.
    pub fn parse(args: &[String], accepted: &[&str]) -> Result<Options, Failure> {
        let mut options = Options::default();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if !arg.starts_with('-') {
                options.positional.push(arg.clone());
                continue;
            }
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (arg.as_str(), None),
            };
            if !accepted.contains(&name) {
                return Err(usage(format!("unknown option '{name}'")));
            }
            let value = match inline {
                Some(value) => value,
                None => iter
                    .next()
                    .cloned()
                    .ok_or_else(|| usage(format!("option '{name}' needs a value")))?,
            };
            options.values.push((name.to_string(), value));
        }
        Ok(options)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(found, _)| found == name)
            .map(|(_, value)| value.as_str())
    }

    /// `--base` and `--default-branch` name the same thing: the ref the branch
    /// is measured against.
    pub fn base_ref(&self) -> Option<&str> {
        self.get("--base").or_else(|| self.get("--default-branch"))
    }

    pub fn positional(&self) -> &[String] {
        &self.positional
    }

    pub fn no_positional(&self) -> Result<(), Failure> {
        match self.positional.first() {
            Some(extra) => Err(usage(format!("unexpected argument '{extra}'"))),
            None => Ok(()),
        }
    }
}

pub fn wants_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "-h" || arg == "--help")
}

/// Where `CHANGELOG.md` lives: the root of the working tree.
pub fn changelog_path() -> Result<PathBuf, Failure> {
    git::top_level()
        .map(|root| root.join(FILE_NAME))
        .ok_or_else(|| error("not inside a git working tree"))
}

/// The file as it is, or an empty one when it does not exist yet.
pub fn read_changelog() -> Result<Changelog, Failure> {
    let path = changelog_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Changelog::parse(&text)),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => Ok(Changelog::parse("")),
        Err(cause) => Err(error(format!("cannot read {}: {cause}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn reads_options_in_both_forms() {
        let options = Options::parse(
            &args(&["--date", "2026-01-01", "--base=main", "x"]),
            &["--date", "--base"],
        )
        .unwrap();
        assert_eq!(options.get("--date"), Some("2026-01-01"));
        assert_eq!(options.base_ref(), Some("main"));
        assert_eq!(options.positional(), ["x".to_string()]);
        assert!(options.no_positional().is_err());
    }

    #[test]
    fn refuses_unknown_and_valueless_options() {
        assert!(matches!(
            Options::parse(&args(&["--nope"]), &["--date"]),
            Err(Failure::Usage(_))
        ));
        assert!(matches!(
            Options::parse(&args(&["--date"]), &["--date"]),
            Err(Failure::Usage(_))
        ));
        assert!(wants_help(&args(&["version", "-h"])));
    }
}

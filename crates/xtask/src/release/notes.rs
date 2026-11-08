//! `release notes`: the `CHANGELOG.md` section of a version, as release body.

use crate::cli::{Options, Outcome, error, read_changelog, usage};
use crate::entries::STAMP_MARKER;
use crate::version::Version;

use super::manifest_version;

/// The body of the section of `version`, marker and surrounding blank lines removed.
pub(super) fn section(version: Version) -> Result<String, crate::cli::Failure> {
    let body = read_changelog()?
        .version_body(version)
        .ok_or_else(|| error(format!("CHANGELOG.md has no section for {version}")))?;
    let lines: Vec<&str> = body
        .lines()
        .filter(|line| line.trim() != STAMP_MARKER)
        .collect();
    let text = lines.join("\n");
    let text = text.trim();
    if text.is_empty() {
        return Err(error(format!(
            "the CHANGELOG.md section of {version} is empty"
        )));
    }
    Ok(format!("{text}\n"))
}

pub(super) fn run(args: &[String]) -> Outcome {
    let options = Options::parse(args, &["--version"])?;
    options.no_positional()?;
    let text = match options.get("--version") {
        Some(text) => text.to_string(),
        None => manifest_version()?,
    };
    let version =
        Version::parse(&text).ok_or_else(|| usage(format!("'{text}' is not a version")))?;
    print!("{}", section(version)?);
    Ok(0)
}

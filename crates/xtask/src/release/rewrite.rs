//! The three places that carry the version, rewritten as plain text.
//!
//! Every function asserts the exact shape it expects and refuses anything
//! else: a format change must fail loudly in the release job, never publish
//! a half-rewritten version.

use std::sync::LazyLock;

use regex::Regex;

/// Files that carry the version, relative to the repository root.
pub const CARGO_TOML: &str = "Cargo.toml";
pub const MANIFEST: &str = "herdr-plugin.toml";
pub const CARGO_LOCK: &str = "Cargo.lock";
pub const FILES: [&str; 3] = [CARGO_TOML, MANIFEST, CARGO_LOCK];

/// The workspace packages whose `version` lines `Cargo.lock` records.
pub const LOCKED_PACKAGES: [&str; 3] = ["herdr-sleep", "herdr-sleep-ph", "xtask"];

static VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d+\.\d+\.\d+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$").expect("valid regex")
});

/// `X.Y.Z`, with an optional SemVer prerelease (canary builds).
pub fn is_version(text: &str) -> bool {
    VERSION.is_match(text)
}

/// The version in a `version = "X"` line, nothing else on the line.
fn version_in(line: &str) -> Option<&str> {
    let inner = line.strip_prefix("version = \"")?.strip_suffix('"')?;
    (!inner.contains('"')).then_some(inner)
}

fn line_of(version: &str) -> String {
    format!("version = \"{version}\"")
}

fn checked(version: &str) -> Result<(), String> {
    if is_version(version) {
        Ok(())
    } else {
        Err(format!("'{version}' is not a version"))
    }
}

/// `[workspace.package] version` of `Cargo.toml`.
pub fn cargo_toml(text: &str, version: &str) -> Result<String, String> {
    checked(version)?;
    let mut in_section = false;
    let mut seen = 0;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            if line.starts_with('[') {
                in_section = line.trim_end() == "[workspace.package]";
            } else if in_section && version_in(line).is_some() {
                seen += 1;
                return line_of(version);
            }
            line.to_string()
        })
        .collect();
    match seen {
        1 => Ok(lines.join("\n")),
        n => Err(format!(
            "{CARGO_TOML}: expected one `version = \"...\"` line in [workspace.package], found {n}"
        )),
    }
}

/// The top level `version` of `herdr-plugin.toml`.
pub fn manifest(text: &str, version: &str) -> Result<String, String> {
    checked(version)?;
    let mut in_table = false;
    let mut seen = 0;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            if line.starts_with('[') {
                in_table = true;
            } else if !in_table && version_in(line).is_some() {
                seen += 1;
                return line_of(version);
            }
            line.to_string()
        })
        .collect();
    match seen {
        1 => Ok(lines.join("\n")),
        n => Err(format!(
            "{MANIFEST}: expected one top level `version = \"...\"` line, found {n}"
        )),
    }
}

/// The `version` of each workspace package in `Cargo.lock`.
pub fn cargo_lock(text: &str, version: &str) -> Result<String, String> {
    checked(version)?;
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    for package in LOCKED_PACKAGES {
        let header = format!("name = \"{package}\"");
        let hits: Vec<usize> = (1..lines.len().saturating_sub(1))
            .filter(|&i| lines[i - 1] == "[[package]]" && lines[i] == header)
            .collect();
        let [at] = hits[..] else {
            return Err(format!(
                "{CARGO_LOCK}: expected one [[package]] block for {package}, found {}",
                hits.len()
            ));
        };
        if version_in(&lines[at + 1]).is_none() {
            return Err(format!(
                "{CARGO_LOCK}: the line after `{header}` is not a version line"
            ));
        }
        lines[at + 1] = line_of(version);
    }
    Ok(lines.join("\n"))
}

/// Rewrites `text` of the file called `name` (one of `FILES`).
pub fn file(name: &str, text: &str, version: &str) -> Result<String, String> {
    match name {
        CARGO_TOML => cargo_toml(text, version),
        MANIFEST => manifest(text, version),
        CARGO_LOCK => cargo_lock(text, version),
        other => Err(format!("{other} does not carry the version")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = "[workspace]\nversion = \"9.9.9\"\n\n[workspace.package]\n# note\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[profile.release]\nversion = \"7\"\n";
    const MANIFEST_TEXT: &str =
        "id = \"x\"\n# c\nversion = \"0.0.0\"\n\n[[build]]\nversion = \"keep\"\n";
    const LOCK: &str = "version = 4\n\n[[package]]\nname = \"herdr-sleep\"\nversion = \"0.0.0\"\ndependencies = [\n \"libc\",\n]\n\n[[package]]\nname = \"herdr-sleep-ph\"\nversion = \"0.0.0\"\n\n[[package]]\nname = \"libc\"\nversion = \"0.2.1\"\n\n[[package]]\nname = \"xtask\"\nversion = \"0.0.0\"\n";

    #[test]
    fn cargo_toml_changes_only_the_workspace_package_version() {
        let out = cargo_toml(TOML, "1.2.3").unwrap();
        assert_eq!(out, TOML.replace("0.0.0", "1.2.3"));
        assert_eq!(cargo_toml(&out, "1.2.3").unwrap(), out);
    }

    #[test]
    fn manifest_changes_only_the_top_level_version() {
        let out = manifest(MANIFEST_TEXT, "1.2.3").unwrap();
        assert_eq!(out, MANIFEST_TEXT.replace("0.0.0", "1.2.3"));
        assert_eq!(manifest(&out, "1.2.3").unwrap(), out);
    }

    #[test]
    fn lock_changes_only_the_workspace_packages() {
        let out = cargo_lock(LOCK, "1.2.3").unwrap();
        assert_eq!(out, LOCK.replace("0.0.0", "1.2.3"));
        assert!(out.contains("name = \"libc\"\nversion = \"0.2.1\""));
        assert_eq!(cargo_lock(&out, "1.2.3").unwrap(), out);
    }

    #[test]
    fn unexpected_shapes_are_refused() {
        assert!(cargo_toml("[workspace.package]\nname = \"x\"\n", "1.0.0").is_err());
        assert!(cargo_toml(&TOML.replace("0.0.0", "0.0.0\"\nversion = \"1"), "1.0.0").is_err());
        assert!(manifest("[[build]]\nversion = \"0.0.0\"\n", "1.0.0").is_err());
        assert!(manifest("version = '0.0.0'\n", "1.0.0").is_err());
        assert!(cargo_lock(&LOCK.replace("\"xtask\"", "\"tasks\""), "1.0.0").is_err());
        assert!(
            cargo_lock(
                &LOCK.replace(
                    "version = \"0.0.0\"\n\n[[package]]\nname = \"libc\"",
                    "\n\n[[package]]\nname = \"libc\""
                ),
                "1.0.0"
            )
            .is_err()
        );
        assert!(cargo_toml(TOML, "1.0").is_err());
        assert!(file("README.md", "", "1.0.0").is_err());
    }

    #[test]
    fn accepts_prereleases_only_in_semver_shape() {
        assert!(is_version("0.2.0-pre.ab12cd3"));
        assert!(is_version("10.0.1"));
        for bad in ["1.0", "v1.0.0", "1.0.0-", "1.0.0 ", "1.0.0-pre\n"] {
            assert!(!is_version(bad), "{bad:?}");
        }
    }
}

//! `release publish-github`: the GitHub release, its assets, and their check.
//!
//! HTTP goes through `curl`, configured on its standard input so that the
//! token never shows in a process listing. Every step first looks at what is
//! already there, so running it twice attaches each file once.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::cli::{Failure, Options, Outcome, error, usage};
use crate::version::Version;

use super::json::{Json, quote};
use super::{manifest_version, notes};

const DEFAULT_REPOSITORY: &str = "Pepsit36/herdr-sleep";
const DEFAULT_API: &str = "https://api.github.com";
const DEFAULT_UPLOADS: &str = "https://uploads.github.com";

struct Github {
    token: String,
    repository: String,
    api: String,
    uploads: String,
    scratch: PathBuf,
}

struct Reply {
    status: u16,
    body: String,
}

/// A value of a curl config file: quoted, with the two escapes it needs.
fn config_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

impl Github {
    fn request(
        &self,
        method: &str,
        url: &str,
        json_body: Option<&str>,
        file_body: Option<&Path>,
    ) -> Result<Reply, Failure> {
        let output = self.scratch.join("reply");
        let _ = fs::remove_file(&output);
        let mut config = vec![
            "silent".to_string(),
            "show-error".to_string(),
            "max-time = 900".to_string(),
            format!("request = {}", config_quote(method)),
            format!("url = {}", config_quote(url)),
            format!("output = {}", config_quote(&output.to_string_lossy())),
            "write-out = \"%{http_code}\"".to_string(),
            format!(
                "header = {}",
                config_quote(&format!("Authorization: Bearer {}", self.token))
            ),
            "header = \"Accept: application/vnd.github+json\"".to_string(),
            "header = \"X-GitHub-Api-Version: 2022-11-28\"".to_string(),
        ];
        if let Some(body) = json_body {
            let path = self.scratch.join("request.json");
            fs::write(&path, body)
                .map_err(|cause| error(format!("cannot write {}: {cause}", path.display())))?;
            config.push("header = \"Content-Type: application/json\"".to_string());
            config.push(format!(
                "data-binary = {}",
                config_quote(&format!("@{}", path.display()))
            ));
        }
        if let Some(path) = file_body {
            config.push("header = \"Content-Type: application/octet-stream\"".to_string());
            config.push(format!(
                "data-binary = {}",
                config_quote(&format!("@{}", path.display()))
            ));
        }
        let mut child = Command::new("curl")
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|cause| error(format!("cannot run curl: {cause}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "{}", config.join("\n"));
        }
        let finished = child
            .wait_with_output()
            .map_err(|cause| error(format!("curl did not finish: {cause}")))?;
        if !finished.status.success() {
            return Err(error(format!(
                "curl {method} {url} failed: {}",
                String::from_utf8_lossy(&finished.stderr).trim()
            )));
        }
        let status = String::from_utf8_lossy(&finished.stdout)
            .trim()
            .parse()
            .map_err(|_| error(format!("curl {method} {url}: no HTTP status")))?;
        let body = fs::read_to_string(&output).unwrap_or_default();
        Ok(Reply { status, body })
    }

    fn expect(&self, reply: &Reply, wanted: &[u16], what: &str) -> Result<(), Failure> {
        if wanted.contains(&reply.status) {
            return Ok(());
        }
        let excerpt: String = reply.body.chars().take(300).collect();
        Err(error(format!("{what}: HTTP {} {excerpt}", reply.status)))
    }

    fn repo_url(&self, path: &str) -> String {
        format!("{}/repos/{}/{path}", self.api, self.repository)
    }

    /// Polls until the tag is visible: the push mirror from GitLab lags.
    fn wait_for_tag(&self, tag: &str, wait: Duration, poll: Duration) -> Result<(), Failure> {
        let started = Instant::now();
        loop {
            let reply = self.request(
                "GET",
                &self.repo_url(&format!("git/ref/tags/{tag}")),
                None,
                None,
            )?;
            match reply.status {
                200 => return Ok(()),
                404 => {}
                _ => self.expect(&reply, &[200], "looking for the tag")?,
            }
            if started.elapsed() >= wait {
                return Err(error(format!(
                    "tag {tag} did not reach {} within {} seconds; is the push mirror running?",
                    self.repository,
                    wait.as_secs()
                )));
            }
            eprintln!(
                "publish-github: waiting for {tag} to reach {}",
                self.repository
            );
            sleep(poll);
        }
    }

    fn release_by_tag(&self, tag: &str) -> Result<Option<Json>, Failure> {
        let reply = self.request(
            "GET",
            &self.repo_url(&format!("releases/tags/{tag}")),
            None,
            None,
        )?;
        match reply.status {
            404 => Ok(None),
            _ => {
                self.expect(&reply, &[200], "reading the release")?;
                Json::parse(&reply.body).map(Some).map_err(error)
            }
        }
    }

    fn ensure_release(&self, tag: &str, notes: &str) -> Result<Json, Failure> {
        if let Some(found) = self.release_by_tag(tag)? {
            println!("publish-github: release {tag} exists");
            return Ok(found);
        }
        let body = format!(
            "{{\"tag_name\":{},\"name\":{},\"body\":{},\"draft\":false,\"prerelease\":false}}",
            quote(tag),
            quote(tag),
            quote(notes)
        );
        let reply = self.request("POST", &self.repo_url("releases"), Some(&body), None)?;
        self.expect(&reply, &[201], "creating the release")?;
        println!("publish-github: created release {tag}");
        Json::parse(&reply.body).map_err(error)
    }
}

/// `(name, size, id)` of the assets attached to a release.
fn attached(release: &Json) -> Vec<(String, u64, u64)> {
    release
        .get("assets")
        .map(Json::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|asset| {
            Some((
                asset.get("name")?.as_str()?.to_string(),
                asset.get("size")?.as_u64()?,
                asset.get("id")?.as_u64()?,
            ))
        })
        .collect()
}

/// The archives and checksums of `version` in `dist`, sorted by name.
pub(super) fn dist_files(
    dist: &Path,
    version: Version,
) -> Result<Vec<(String, PathBuf, u64)>, Failure> {
    let prefix = format!("herdr-sleep-{version}-");
    let entries = fs::read_dir(dist)
        .map_err(|cause| error(format!("cannot read {}: {cause}", dist.display())))?;
    let mut files: Vec<(String, PathBuf, u64)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let wanted = name.starts_with(&prefix)
                && (name.ends_with(".tar.gz") || name.ends_with(".tar.gz.sha256"));
            let size = entry.metadata().ok()?.len();
            wanted.then(|| (name, entry.path(), size))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(error(format!(
            "no {prefix}*.tar.gz archive in {}; run the build jobs first",
            dist.display()
        )));
    }
    Ok(files)
}

fn seconds(options: &Options, name: &str, default: u64) -> Result<Duration, Failure> {
    match options.get(name) {
        None => Ok(Duration::from_secs(default)),
        Some(text) => text
            .parse()
            .map(Duration::from_secs)
            .map_err(|_| usage(format!("{name} needs a number of seconds"))),
    }
}

pub(super) fn run(args: &[String], dry_run: bool) -> Outcome {
    let options = Options::parse(
        args,
        &["--dist", "--version", "--wait-seconds", "--poll-seconds"],
    )?;
    options.no_positional()?;
    let text = match options.get("--version") {
        Some(text) => text.to_string(),
        None => manifest_version()?,
    };
    let version =
        Version::parse(&text).ok_or_else(|| usage(format!("'{text}' is not a version")))?;
    let tag = version.tag();
    let dist = Path::new(options.get("--dist").unwrap_or("dist"));
    let files = dist_files(dist, version)?;
    let notes = notes::section(version)?;
    let repository =
        std::env::var("GITHUB_REPOSITORY").unwrap_or_else(|_| DEFAULT_REPOSITORY.to_string());

    if dry_run {
        println!("publish-github: would wait for {tag} on {repository}");
        println!(
            "publish-github: would create the release {tag} if missing, with {} lines of notes",
            notes.lines().count()
        );
        for (name, _, size) in &files {
            println!(
                "publish-github: would attach {name} ({size} bytes) unless it is attached with that size"
            );
        }
        return Ok(0);
    }

    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let token = token.ok_or_else(|| {
        error("publish-github: GITHUB_TOKEN is not set. Create a fine-grained token with Contents read and write on the repository and add it as a masked CI/CD variable.")
    })?;
    let scratch = std::env::temp_dir().join(format!("herdr-sleep-publish-{}", std::process::id()));
    fs::create_dir_all(&scratch)
        .map_err(|cause| error(format!("cannot create {}: {cause}", scratch.display())))?;
    let github = Github {
        token,
        repository,
        api: std::env::var("GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_API.to_string()),
        uploads: std::env::var("GITHUB_UPLOAD_URL").unwrap_or_else(|_| DEFAULT_UPLOADS.to_string()),
        scratch: scratch.clone(),
    };
    let result = publish(&github, &options, &tag, &notes, &files);
    let _ = fs::remove_dir_all(&scratch);
    result?;
    Ok(0)
}

fn publish(
    github: &Github,
    options: &Options,
    tag: &str,
    notes: &str,
    files: &[(String, PathBuf, u64)],
) -> Result<(), Failure> {
    github.wait_for_tag(
        tag,
        seconds(options, "--wait-seconds", 600)?,
        seconds(options, "--poll-seconds", 10)?,
    )?;
    let release = github.ensure_release(tag, notes)?;
    let id = release
        .get("id")
        .and_then(Json::as_u64)
        .ok_or_else(|| error("the release has no id"))?;
    let present = attached(&release);

    for (name, path, size) in files {
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            return Err(error(format!(
                "refusing to upload '{name}': unexpected characters in the name"
            )));
        }
        match present.iter().find(|(known, _, _)| known == name) {
            Some((_, attached_size, _)) if attached_size == size => {
                println!("publish-github: {name} is already attached");
                continue;
            }
            Some((_, _, asset_id)) => {
                let url = github.repo_url(&format!("releases/assets/{asset_id}"));
                let reply = github.request("DELETE", &url, None, None)?;
                github.expect(&reply, &[204], &format!("replacing {name}"))?;
                println!("publish-github: removed {name} attached with another size");
            }
            None => {}
        }
        let url = format!(
            "{}/repos/{}/releases/{id}/assets?name={name}",
            github.uploads, github.repository
        );
        let reply = github.request("POST", &url, None, Some(path))?;
        github.expect(&reply, &[201], &format!("uploading {name}"))?;
        println!("publish-github: attached {name}");
    }

    let verified = github
        .release_by_tag(tag)?
        .ok_or_else(|| error("the release disappeared after the upload"))?;
    let present = attached(&verified);
    for (name, _, size) in files {
        match present.iter().find(|(known, _, _)| known == name) {
            Some((_, found, _)) if found == size => {}
            Some((_, found, _)) => {
                return Err(error(format!(
                    "{name} was uploaded with {found} bytes, expected {size}"
                )));
            }
            None => return Err(error(format!("{name} is not attached to the release"))),
        }
    }
    println!(
        "publish-github: {} files verified on release {tag}",
        files.len()
    );
    Ok(())
}

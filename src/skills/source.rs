//! Where skills are installed from: an `https://` git URL (any host, with
//! an optional `#subdir` and `@ref`), `github:owner/repo[/subdir][@ref]`,
//! or a local folder. Nothing else: no ssh, `git@`, `file://`, `http://`
//! or git remote helpers.
//!
//! A git source is fetched into a fresh staging folder with every hook,
//! submodule, LFS filter, extra protocol and prompt turned off, and nothing
//! from the repository is ever run.

use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one git command may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// A parsed install source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Git {
        /// The clone URL, always `https://`.
        url: String,
        subdir: Option<String>,
        /// Branch, tag or commit; `None` is the default branch.
        reference: Option<String>,
    },
    Local {
        path: PathBuf,
        subdir: Option<String>,
    },
}

impl Source {
    /// Parses what the user typed. Local paths must exist.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if let Some(rest) = raw.strip_prefix("github:") {
            return github(rest);
        }
        if let Some(rest) = raw.strip_prefix("https://") {
            return https(rest);
        }
        if let Some(scheme) = refused_scheme(raw) {
            bail!(
                "{scheme} sources aren't supported; use an https:// git URL, github:owner/repo, or a local folder"
            );
        }
        local(raw)
    }

    /// The source as recorded in the lockfile (URL or absolute folder).
    pub fn location(&self) -> String {
        match self {
            Source::Git { url, .. } => url.clone(),
            Source::Local { path, .. } => path.display().to_string(),
        }
    }

    pub fn subdir(&self) -> Option<&str> {
        match self {
            Source::Git { subdir, .. } | Source::Local { subdir, .. } => subdir.as_deref(),
        }
    }

    pub fn reference(&self) -> Option<&str> {
        match self {
            Source::Git { reference, .. } => reference.as_deref(),
            Source::Local { .. } => None,
        }
    }

    /// The default bundle name: the repository or folder name.
    pub fn default_bundle(&self) -> String {
        let last = match self {
            Source::Git { url, .. } => url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .trim_end_matches(".git")
                .to_string(),
            Source::Local { path, .. } => path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        };
        slug(&last)
    }

    /// Rebuilds a source from a lockfile entry (for `gz skills update`).
    pub fn from_lock(
        location: &str,
        subdir: Option<&str>,
        reference: Option<&str>,
    ) -> Result<Self> {
        let mut raw = location.to_string();
        if let Some(subdir) = subdir {
            raw.push('#');
            raw.push_str(subdir);
        }
        if let Some(reference) = reference {
            raw.push('@');
            raw.push_str(reference);
        }
        Self::parse(&raw)
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.location())?;
        if let Some(subdir) = self.subdir() {
            write!(f, "#{subdir}")?;
        }
        if let Some(reference) = self.reference() {
            write!(f, "@{reference}")?;
        }
        Ok(())
    }
}

fn refused_scheme(raw: &str) -> Option<&'static str> {
    let lower = raw.to_ascii_lowercase();
    [
        ("http://", "http://"),
        ("ssh://", "ssh"),
        ("git@", "ssh (git@)"),
        ("git://", "git://"),
        ("file://", "file://"),
        ("ftp://", "ftp://"),
    ]
    .into_iter()
    .find(|(prefix, _)| lower.starts_with(prefix))
    .map(|(_, name)| name)
    .or_else(|| lower.contains("::").then_some("git remote-helper (::)"))
    .or_else(|| {
        // Any other `scheme://`.
        lower
            .split_once("://")
            .filter(|(scheme, _)| {
                scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+')
            })
            .map(|_| "other URL")
    })
}

fn github(rest: &str) -> Result<Source> {
    let (path, reference) = split_ref(rest);
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
        bail!("use github:owner/repo[/subdir][@ref]");
    };
    for part in [owner, repo] {
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            || part.starts_with(['-', '.'])
        {
            bail!("'{part}' is not a valid GitHub owner or repository name");
        }
    }
    let subdir: Vec<&str> = parts.collect();
    let subdir = (!subdir.is_empty()).then(|| subdir.join("/"));
    Ok(Source::Git {
        url: format!(
            "https://github.com/{owner}/{}.git",
            repo.trim_end_matches(".git")
        ),
        subdir: subdir.map(|s| check_subdir(&s)).transpose()?,
        reference: reference.map(check_ref).transpose()?,
    })
}

/// `host/path[#subdir][@ref]`, also accepting `@ref` before the `#`.
fn https(rest: &str) -> Result<Source> {
    let (address, fragment) = match rest.split_once('#') {
        Some((address, fragment)) => (address, Some(fragment)),
        None => (rest, None),
    };
    let (fragment, fragment_ref) = match fragment {
        Some(fragment) => {
            let (subdir, reference) = split_ref(fragment);
            (Some(subdir), reference)
        }
        None => (None, None),
    };
    let (host_and_path, address_ref) = match address.split_once('/') {
        Some((host, path)) => {
            let (path, reference) = split_ref(path);
            (format!("{host}/{path}"), reference)
        }
        None => (address.to_string(), None),
    };
    let host = host_and_path.split('/').next().unwrap_or_default();
    if host.is_empty() || host.contains('@') {
        bail!("put no user name or password in the URL; git asks your credential helper instead");
    }
    if address_ref.is_some() && fragment_ref.is_some() {
        bail!("give the ref once (url#subdir@ref)");
    }
    if host_and_path.contains(['?', ' ']) || !host_and_path.contains('/') {
        bail!("'https://{host_and_path}' is not a git repository URL");
    }
    let parsed = url::Url::parse(&format!("https://{host_and_path}"))
        .with_context(|| format!("'https://{host_and_path}' is not a valid URL"))?;
    if parsed.host_str().is_none() {
        bail!("'https://{host_and_path}' has no host");
    }
    Ok(Source::Git {
        url: format!("https://{host_and_path}"),
        subdir: fragment
            .filter(|s| !s.is_empty())
            .map(check_subdir)
            .transpose()?,
        reference: address_ref.or(fragment_ref).map(check_ref).transpose()?,
    })
}

fn local(raw: &str) -> Result<Source> {
    let (path, subdir) = match raw.split_once('#') {
        Some((path, subdir)) => (path, Some(check_subdir(subdir)?)),
        None => (raw, None),
    };
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => directories::BaseDirs::new()
            .context("cannot resolve the home folder")?
            .home_dir()
            .join(rest),
        None => PathBuf::from(path),
    };
    if !expanded.is_dir() {
        bail!(
            "'{raw}' is not a folder; give an https:// git URL, github:owner/repo, or a local folder"
        );
    }
    let path = expanded
        .canonicalize()
        .with_context(|| format!("cannot open {}", expanded.display()))?;
    Ok(Source::Local { path, subdir })
}

/// `path@ref` → (`path`, `Some(ref)`), at the last `@`.
fn split_ref(text: &str) -> (&str, Option<&str>) {
    match text.rsplit_once('@') {
        Some((path, reference)) => (path, Some(reference)),
        None => (text, None),
    }
}

/// Branch, tag or commit names: never an option, never a path escape.
fn check_ref(reference: &str) -> Result<String> {
    let ok = !reference.is_empty()
        && !reference.starts_with(['-', '/'])
        && !reference.contains("..")
        && reference
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
    if !ok {
        bail!("'{reference}' is not a valid branch, tag or commit");
    }
    Ok(reference.to_string())
}

/// A relative folder inside the source, without `..` or a leading `-`.
pub fn check_subdir(subdir: &str) -> Result<String> {
    let trimmed = subdir.trim_matches('/');
    let path = Path::new(trimmed);
    let escapes = path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)));
    if trimmed.is_empty() || escapes || trimmed.starts_with('-') || trimmed.contains('\\') {
        bail!("'{subdir}' must be a folder inside the source (no '..' or absolute paths)");
    }
    Ok(trimmed.to_string())
}

/// A name usable as a bundle id: lowercase letters, digits and hyphens.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "skills".to_string()
    } else {
        out
    }
}

/// One git invocation, as data so its hardening can be checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommand {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// `-c` settings on every git call: no hooks, https only.
fn hardened_config() -> Vec<String> {
    let hooks = if cfg!(windows) { "NUL" } else { "/dev/null" };
    [
        format!("core.hooksPath={hooks}"),
        "protocol.allow=never".to_string(),
        "protocol.https.allow=always".to_string(),
        "submodule.recurse=false".to_string(),
        "core.fsmonitor=false".to_string(),
        "filter.lfs.required=false".to_string(),
        "filter.lfs.smudge=".to_string(),
        "filter.lfs.process=".to_string(),
    ]
    .into_iter()
    .flat_map(|setting| ["-c".to_string(), setting])
    .collect()
}

fn hardened_env() -> Vec<(String, String)> {
    [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_LFS_SKIP_SMUDGE", "1"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_PROTOCOL_FROM_USER", "0"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn git(dir: Option<&Path>, args: &[&str]) -> GitCommand {
    let mut all = hardened_config();
    if let Some(dir) = dir {
        all.extend(["-C".to_string(), dir.display().to_string()]);
    }
    all.extend(args.iter().map(|a| a.to_string()));
    GitCommand {
        args: all,
        env: hardened_env(),
    }
}

/// The commands that fetch `url` at `reference` (default branch when
/// `None`) into the empty folder `dest`: an init without templates (so no
/// sample hooks), a shallow fetch of that one ref without submodules or
/// tags, a detached checkout, and reading the commit.
pub fn fetch_commands(url: &str, reference: Option<&str>, dest: &Path) -> Vec<GitCommand> {
    let dest_text = dest.display().to_string();
    vec![
        git(None, &["init", "--quiet", "--template=", &dest_text]),
        git(
            Some(dest),
            &[
                "fetch",
                "--quiet",
                "--depth",
                "1",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                url,
                reference.unwrap_or("HEAD"),
            ],
        ),
        git(
            Some(dest),
            &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
        ),
        git(Some(dest), &["rev-parse", "HEAD"]),
    ]
}

/// Fetches a git source into `dest` (which must not exist yet) and returns
/// the commit it resolved to.
pub fn fetch(url: &str, reference: Option<&str>, dest: &Path) -> Result<String> {
    let mut commit = String::new();
    for command in fetch_commands(url, reference, dest) {
        commit = run_git(&command)?;
    }
    let commit = commit.trim().to_string();
    if commit.len() < 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("git did not report the fetched commit");
    }
    Ok(commit)
}

/// Runs one git command with a timeout and no stdin; stdout on success.
fn run_git(command: &GitCommand) -> Result<String> {
    let mut process = Command::new("git");
    process
        .args(&command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_SSH_COMMAND");
    for (key, value) in &command.env {
        process.env(key, value);
    }
    let mut child = process
        .spawn()
        .context("cannot run git; install git to add skills from a URL")?;
    let started = Instant::now();
    loop {
        if child.try_wait().context("cannot wait for git")?.is_some() {
            break;
        }
        if started.elapsed() > GIT_TIMEOUT {
            child.kill().ok();
            child.wait().ok();
            bail!(
                "git took longer than {}s and was stopped",
                GIT_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child
        .wait_with_output()
        .context("cannot read git's output")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {} failed: {}",
            command
                .args
                .iter()
                .rev()
                .find(|a| !a.starts_with('-'))
                .map_or("", |a| a.as_str()),
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_source(raw: &str) -> (String, Option<String>, Option<String>) {
        match Source::parse(raw).unwrap() {
            Source::Git {
                url,
                subdir,
                reference,
            } => (url, subdir, reference),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_https_urls_with_subdir_and_ref_in_either_order() {
        assert_eq!(
            git_source("https://gitlab.example.com/team/skills.git"),
            (
                "https://gitlab.example.com/team/skills.git".into(),
                None,
                None
            )
        );
        assert_eq!(
            git_source("https://github.com/o/r#skills/pdf@v1.2"),
            (
                "https://github.com/o/r".into(),
                Some("skills/pdf".into()),
                Some("v1.2".into())
            )
        );
        assert_eq!(
            git_source("https://github.com/o/r@main#skills/pdf"),
            (
                "https://github.com/o/r".into(),
                Some("skills/pdf".into()),
                Some("main".into())
            )
        );
    }

    #[test]
    fn parses_the_github_shorthand() {
        assert_eq!(
            git_source("github:acme/agent-skills/skills/sql@0123abc"),
            (
                "https://github.com/acme/agent-skills.git".into(),
                Some("skills/sql".into()),
                Some("0123abc".into())
            )
        );
        assert!(Source::parse("github:onlyowner").is_err());
        assert!(Source::parse("github:-x/repo").is_err());
    }

    #[test]
    fn refuses_every_other_transport() {
        for raw in [
            "http://github.com/o/r",
            "ssh://git@github.com/o/r",
            "git@github.com:o/r.git",
            "git://github.com/o/r",
            "file:///tmp/repo",
            "ext::sh -c touch% /tmp/pwned",
            "s3://bucket/skills",
        ] {
            let err = Source::parse(raw).unwrap_err().to_string();
            assert!(err.contains("aren't supported"), "{raw}: {err}");
        }
    }

    #[test]
    fn refuses_credentials_option_like_refs_and_escaping_subdirs() {
        assert!(Source::parse("https://user:pw@github.com/o/r").is_err());
        assert!(Source::parse("https://github.com/o/r@--upload-pack=x").is_err());
        assert!(Source::parse("https://github.com/o/r@a..b").is_err());
        assert!(Source::parse("https://github.com/o/r#../../etc").is_err());
        assert!(
            Source::parse("https://github.com/o/r#/abs").is_ok(),
            "trimmed to 'abs'"
        );
        assert!(Source::parse("github:o/r/../x").is_err());
    }

    #[test]
    fn local_folders_must_exist() {
        let dir = std::env::temp_dir();
        let parsed = Source::parse(&format!("{}#skills/a", dir.display())).unwrap();
        assert!(matches!(parsed, Source::Local { subdir: Some(ref s), .. } if s == "skills/a"));
        assert!(Source::parse("/definitely/not/here").is_err());
    }

    #[test]
    fn bundle_names_come_from_the_repository_or_folder() {
        assert_eq!(
            Source::parse("github:o/Agent_Skills")
                .unwrap()
                .default_bundle(),
            "agent-skills"
        );
        assert_eq!(
            Source::parse("https://host.io/team/x.git")
                .unwrap()
                .default_bundle(),
            "x"
        );
    }

    #[test]
    fn git_runs_with_hooks_protocols_submodules_lfs_and_prompts_off() {
        let dest = Path::new("/tmp/stage/src");
        let commands = fetch_commands("https://github.com/o/r.git", Some("v1"), dest);
        assert_eq!(commands.len(), 4);
        for command in &commands {
            let args = command.args.join(" ");
            for setting in [
                "-c core.hooksPath=/dev/null",
                "-c protocol.allow=never",
                "-c protocol.https.allow=always",
                "-c submodule.recurse=false",
            ] {
                assert!(
                    cfg!(windows) || args.contains(setting),
                    "{setting} missing: {args}"
                );
            }
            for (key, value) in [
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GIT_LFS_SKIP_SMUDGE", "1"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
            ] {
                assert!(command.env.contains(&(key.into(), value.into())), "{key}");
            }
        }
        let init = commands[0].args.join(" ");
        assert!(
            init.ends_with("init --quiet --template= /tmp/stage/src"),
            "{init}"
        );
        let fetch = &commands[1].args;
        let tail: Vec<&str> = fetch
            .iter()
            .map(String::as_str)
            .skip_while(|a| *a != "fetch")
            .collect();
        assert_eq!(
            tail,
            vec![
                "fetch",
                "--quiet",
                "--depth",
                "1",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                "https://github.com/o/r.git",
                "v1"
            ],
            "the URL comes after `--`, so it can never be read as an option"
        );
        assert!(commands[2]
            .args
            .ends_with(&["--detach".into(), "FETCH_HEAD".into()]));
        let default = fetch_commands("https://github.com/o/r.git", None, dest);
        assert_eq!(default[1].args.last().map(String::as_str), Some("HEAD"));
    }
}

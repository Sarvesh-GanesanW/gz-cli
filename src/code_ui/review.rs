//! `/review` and `/security-review`: a canned request, sent as a normal
//! message, for the model to review a change with read-only tools: the
//! uncommitted changes, a branch against its base, or a pull request
//! (through `gh`).

use std::path::Path;

/// What a review looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Staged, unstaged and untracked changes.
    Uncommitted,
    /// A branch (or any ref) against the branch it came from.
    Branch(String),
    /// A pull request, by number or URL.
    PullRequest(String),
}

/// Which review to ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    General,
    Security,
}

impl Focus {
    pub fn command(self) -> &'static str {
        match self {
            Focus::General => "/review",
            Focus::Security => "/security-review",
        }
    }
}

impl Target {
    /// Reads `/review`'s argument: nothing, a PR (`123`, `#123`, a
    /// `…/pull/123` URL) or a branch name.
    pub fn parse(argument: &str) -> Result<Self, String> {
        let argument = argument.trim();
        if argument.is_empty() {
            return Ok(Target::Uncommitted);
        }
        if argument.contains(char::is_whitespace) {
            return Err(format!(
                "review one thing at a time: a branch, a PR number or URL, or nothing for the uncommitted changes (not `{argument}`)"
            ));
        }
        let number = argument.strip_prefix('#').unwrap_or(argument);
        if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
            return Ok(Target::PullRequest(number.to_string()));
        }
        if argument.starts_with("https://") {
            if !is_pull_request_url(argument) {
                return Err(format!(
                    "`{argument}` is not a pull request link gz can pass to gh; use https://<host>/<owner>/<repo>/pull/<number> or just the number"
                ));
            }
            return Ok(Target::PullRequest(argument.to_string()));
        }
        if !is_safe_ref(argument) {
            return Err(format!(
                "`{argument}` is not a branch name gz can pass to git; use letters, digits and / . _ -"
            ));
        }
        Ok(Target::Branch(argument.to_string()))
    }

    fn what(&self) -> String {
        match self {
            Target::Uncommitted => "the uncommitted changes in this repository".to_string(),
            Target::Branch(branch) => {
                format!("the branch `{branch}` against the branch it came from")
            }
            Target::PullRequest(pr) => format!("the pull request {pr}"),
        }
    }

    /// The read-only commands that show the change.
    fn steps(&self) -> String {
        match self {
            Target::Uncommitted => "- `git status --short` and `git diff HEAD` for tracked changes (`git diff --cached` too if HEAD does not exist yet)\n\
                 - read each untracked file `git status` lists"
                .to_string(),
            Target::Branch(branch) => format!(
                "- find its base: the remote's default branch (`git symbolic-ref --short refs/remotes/origin/HEAD`, else `main` or `master`), then `git merge-base <base> {branch}`\n\
                 - `git log --oneline <merge-base>..{branch}` and `git diff <merge-base>...{branch}`"
            ),
            Target::PullRequest(pr) => format!(
                "- `gh pr view {pr}` for its title, description and base\n\
                 - `gh pr diff {pr}` for the change"
            ),
        }
    }
}

/// Branch names safe to put in a shell command the model runs.
/// `https://<host>/<owner>/<repo>/pull/<number>`, with a trailing slash
/// at most: only characters that are inert in a shell command line.
fn is_pull_request_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    let [host, owner, repo, "pull", number] = parts.as_slice() else {
        return false;
    };
    let is_name = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    let is_host = host.split_once(':').map_or(is_name(host), |(name, port)| {
        is_name(name) && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
    });
    is_host
        && is_name(owner)
        && is_name(repo)
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

fn is_safe_ref(name: &str) -> bool {
    !name.starts_with('-')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
}

/// Whether `gh` is on `PATH`.
pub fn has_gh() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join("gh")))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.with_extension("exe").is_file() || path.is_file()
    }
}

/// The message a review sends. Refuses a PR when `gh` is missing.
pub fn prompt(focus: Focus, target: &Target, has_gh: bool) -> Result<String, String> {
    if matches!(target, Target::PullRequest(_)) && !has_gh {
        return Err(format!(
            "{} of a pull request needs the GitHub CLI (`gh`): install it and run `gh auth login`, or review the branch instead ({} <branch>)",
            focus.command(),
            focus.command()
        ));
    }
    let (asked, look_for) = match focus {
        Focus::General => ("Review", GENERAL_CHECKS),
        Focus::Security => ("Do a security review of", SECURITY_CHECKS),
    };
    Ok(format!(
        "{asked} {what}.\n\n\
         This is a read-only review: do not edit, write or delete files, do not commit, push, check out or stash, and run only commands that read. Get the change with:\n\
         {steps}\n\n\
         Read the surrounding code wherever the diff alone does not show whether something is right.\n\n\
         {look_for}\n\n\
         Report each finding with its file and line, the severity (high, medium or low), what goes wrong and in which situation, and the fix you suggest, most severe first. Leave out style nits and anything you are not reasonably sure of; if you find nothing worth fixing, say so plainly.",
        what = target.what(),
        steps = target.steps(),
    ))
}

const GENERAL_CHECKS: &str = "Look for: bugs and wrong behaviour (edge cases, error handling, off-by-one, concurrency, resource leaks), breaking changes to interfaces or data, missing or wrong tests for what changed, and security problems.";

const SECURITY_CHECKS: &str = "Focus only on security: injection (SQL, shell, path, template), authentication and authorization gaps, secrets or credentials in code, logs or errors, unsafe deserialization, SSRF and open redirects, path traversal, insecure defaults or crypto, missing input validation at trust boundaries, and dependencies added with known risks. Say how each issue could be exploited.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_read_from_the_argument() {
        assert_eq!(Target::parse(""), Ok(Target::Uncommitted));
        assert_eq!(Target::parse("#42"), Ok(Target::PullRequest("42".into())));
        assert_eq!(Target::parse("42"), Ok(Target::PullRequest("42".into())));
        let url = "https://github.com/o/r/pull/7";
        assert_eq!(Target::parse(url), Ok(Target::PullRequest(url.into())));
        let enterprise = "https://git.acme.io:8443/team/app/pull/12/";
        assert!(Target::parse(enterprise).is_ok());
        for bad in [
            "https://x/pull/1;id",
            "https://github.com/o/r/pull/7$(id)",
            "https://github.com/o/r/issues/7",
            "https://github.com/o/r/pull/",
            "https://github.com/o/r/pull/7/files",
        ] {
            assert!(
                Target::parse(bad)
                    .unwrap_err()
                    .contains("not a pull request link"),
                "{bad}"
            );
        }
        assert_eq!(
            Target::parse("feature/x-1"),
            Ok(Target::Branch("feature/x-1".into()))
        );
        assert!(Target::parse("a b")
            .unwrap_err()
            .contains("one thing at a time"));
        assert!(Target::parse("x;rm")
            .unwrap_err()
            .contains("not a branch name"));
        assert!(Target::parse("--force").is_err());
    }

    #[test]
    fn prompts_name_the_target_its_commands_and_the_focus() {
        let review = prompt(Focus::General, &Target::Uncommitted, false).unwrap();
        assert!(review.starts_with("Review the uncommitted changes"));
        assert!(review.contains("`git diff HEAD`"));
        assert!(review.contains("read-only review"));

        let branch = prompt(Focus::Security, &Target::Branch("dev".into()), false).unwrap();
        assert!(branch.starts_with("Do a security review of the branch `dev`"));
        assert!(branch.contains("git diff <merge-base>...dev"));
        assert!(branch.contains("Focus only on security"));

        let pr = prompt(Focus::General, &Target::PullRequest("9".into()), true).unwrap();
        assert!(pr.contains("`gh pr diff 9`"));
        let no_gh = prompt(Focus::General, &Target::PullRequest("9".into()), false);
        assert!(no_gh.unwrap_err().contains("needs the GitHub CLI"));
    }
}

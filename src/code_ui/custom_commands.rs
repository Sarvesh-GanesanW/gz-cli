//! Your own slash commands: Markdown files in `.gz/commands/` (project)
//! and `<gz config folder>/commands/` (user). `review.md` is `/review`;
//! a subfolder is a namespace, so `frontend/component.md` is
//! `/frontend:component`. Optional front matter:
//!
//! ```markdown
//! ---
//! description: Make a React component
//! argument-hint: <name> [props]
//! model: sonnet
//! ---
//! Create the component $1 with props $2. Follow @docs/components.md.
//! ```
//!
//! The body is sent as your message with `$ARGUMENTS` (everything after
//! the name) and `$1`…`$9` (one word each, quotes group words) filled in;
//! `@file` references are read like any `@` mention.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::code_ui::review::{self, Focus, Target};

/// The project's commands folder, from the open folder.
pub const PROJECT_DIR: &str = ".gz/commands";
/// The user's commands folder, inside the gz config folder.
pub const USER_DIR: &str = "commands";
/// How deep namespaces go (`a/b/c.md` is `/a:b:c`).
const MAX_DEPTH: usize = 4;
/// A command file larger than this is refused: it is a prompt, not data,
/// and the chat takes at most 200,000 characters per message
/// (gz-chat-eks api/routes/chat/field_types.py `MAX_USER_INPUT_CHARS`).
const MAX_FILE_BYTES: u64 = 100 * 1024;
/// A description made from the body is cut to this many characters.
const DESCRIPTION_CHARS: usize = 80;

/// Which folder a command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::User => "user",
        }
    }
}

/// One command file, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// `/frontend:component`.
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// The model it runs on, as `/models` names it; `None` keeps the
    /// current one.
    pub model: Option<String>,
    pub body: String,
    pub scope: Scope,
    pub path: PathBuf,
    /// What the `/` menu shows next to the name.
    pub menu_text: String,
}

/// Every command found, and what could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// The folders it was read from, to read them again.
    project_root: Option<PathBuf>,
    user_config: Option<PathBuf>,
    pub commands: Vec<CustomCommand>,
    /// Skills `/name` loads: `(/name, menu text)`.
    skills: Vec<(String, String)>,
    /// Commands a project command of the same name hides.
    pub hidden: Vec<CustomCommand>,
    pub problems: Vec<String>,
}

impl Catalog {
    /// Reads both folders; a project command wins over a user command of
    /// the same name. Missing folders have no commands.
    pub fn load(project_root: Option<&Path>, user_config: Option<&Path>) -> Self {
        let mut catalog = Catalog {
            project_root: project_root.map(Path::to_path_buf),
            user_config: user_config.map(Path::to_path_buf),
            ..Catalog::default()
        };
        let folders = [
            (
                project_root.map(|root| root.join(PROJECT_DIR)),
                Scope::Project,
            ),
            (user_config.map(|dir| dir.join(USER_DIR)), Scope::User),
        ];
        for (folder, scope) in folders {
            let Some(folder) = folder else { continue };
            for (path, name) in command_files(&folder, &mut catalog.problems) {
                match read_command(&path, name, scope) {
                    Ok(command) => catalog.add(command),
                    Err(err) => catalog
                        .problems
                        .push(format!("{}: {err:#}", path.display())),
                }
            }
        }
        catalog.commands.sort_by(|a, b| a.name.cmp(&b.name));
        catalog
    }

    /// Reads the same folders again, so files added or changed since show.
    pub fn reload(&mut self) {
        let skills = std::mem::take(&mut self.skills);
        *self = Catalog::load(self.project_root.as_deref(), self.user_config.as_deref());
        self.skills = skills;
    }

    /// The skills the assistant may use now, each runnable as `/name`.
    pub fn set_skills(&mut self, names: &[String]) {
        self.skills = names
            .iter()
            .map(|name| {
                (
                    format!("/{name}"),
                    "skill: load it and send what follows".to_string(),
                )
            })
            .collect();
    }

    /// The skill `/name` loads, when there is one and no command file
    /// has that name.
    pub fn skill(&self, name: &str) -> Option<&str> {
        if self.find(name).is_some() {
            return None;
        }
        self.skills
            .iter()
            .find(|(command, _)| command == name)
            .map(|(command, _)| &command[1..])
    }

    /// Skills for the `/` menu, after command files of the same name.
    pub fn skill_menu(&self) -> impl Iterator<Item = (&str, &str)> {
        self.skills
            .iter()
            .filter(|(command, _)| self.find(command).is_none())
            .map(|(command, text)| (command.as_str(), text.as_str()))
    }

    fn add(&mut self, command: CustomCommand) {
        if self.commands.iter().any(|c| c.name == command.name) {
            self.hidden.push(command);
            return;
        }
        self.commands.push(command);
    }

    /// The command named `name` (with its `/`).
    pub fn find(&self, name: &str) -> Option<&CustomCommand> {
        self.commands.iter().find(|c| c.name == name)
    }
}

/// Every `.md` file under `folder` with its command name, sorted.
fn command_files(folder: &Path, problems: &mut Vec<String>) -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    walk(folder, &mut Vec::new(), &mut found, problems);
    found.sort();
    found
}

fn walk(
    folder: &Path,
    namespace: &mut Vec<String>,
    found: &mut Vec<(PathBuf, String)>,
    problems: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => return problems.push(format!("cannot read {}: {err}", folder.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Symlinks are followed: a dotfiles setup links its commands in.
        let is_dir = path.is_dir();
        let stem = match is_dir {
            true => path.file_name(),
            false => path.file_stem(),
        };
        let Some(stem) = stem.and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        if stem.starts_with('.') {
            continue;
        }
        if is_dir {
            if namespace.len() + 1 >= MAX_DEPTH {
                problems.push(format!(
                    "{} is nested too deep for a command namespace (at most {MAX_DEPTH} levels)",
                    path.display()
                ));
                continue;
            }
            namespace.push(stem);
            walk(&path, namespace, found, problems);
            namespace.pop();
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let parts: Vec<&str> = namespace
            .iter()
            .map(String::as_str)
            .chain([stem.as_str()])
            .collect();
        if let Some(bad) = parts.iter().find(|part| !is_valid_part(part)) {
            problems.push(format!(
                "{}: '{bad}' cannot be part of a command name (use letters, digits, - and _)",
                path.display()
            ));
            continue;
        }
        found.push((path.clone(), format!("/{}", parts.join(":"))));
    }
}

fn is_valid_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct FrontMatter {
    description: Option<String>,
    argument_hint: Option<String>,
    model: Option<String>,
}

fn read_command(path: &Path, name: String, scope: Scope) -> Result<CustomCommand> {
    let size = std::fs::metadata(path).context("cannot read it")?.len();
    if size > MAX_FILE_BYTES {
        bail!("it is {size} bytes; a command file may have at most {MAX_FILE_BYTES}");
    }
    let text = std::fs::read_to_string(path).context("cannot read it as UTF-8 text")?;
    let (front, body) = split_front_matter(&text)?;
    let front: FrontMatter = match front {
        Some(yaml) if !yaml.trim().is_empty() => {
            serde_yaml::from_str(yaml).context("its front matter is not valid YAML")?
        }
        _ => FrontMatter::default(),
    };
    let body = body.trim().to_string();
    if body.is_empty() {
        bail!("it has no text to send");
    }
    let description = front
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| first_line(&body));
    let argument_hint = front
        .argument_hint
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty());
    let mut menu_text = description.clone();
    if let Some(hint) = &argument_hint {
        menu_text = format!("{menu_text} · {hint}");
    }
    menu_text = format!("{menu_text} ({})", scope.as_str());
    Ok(CustomCommand {
        name,
        description,
        argument_hint,
        model: front
            .model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty()),
        body,
        scope,
        path: path.to_path_buf(),
        menu_text,
    })
}

/// `(front matter, body)`; no front matter when the file doesn't start
/// with `---`.
fn split_front_matter(text: &str) -> Result<(Option<&str>, &str)> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok((None, text));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Ok((Some(&rest[..offset]), &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    bail!("its front matter is not closed with `---`")
}

fn first_line(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .trim_start_matches('#')
        .trim();
    if line.chars().count() <= DESCRIPTION_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(DESCRIPTION_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// A slash line typed outside the full-screen UI, expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    pub prompt: String,
    /// A command file's `model:`, as written.
    pub model: Option<String>,
    /// Where it came from, for messages: the command file, or the command.
    pub source: String,
    pub kind: ExpandedKind,
}

/// What a slash line turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandedKind {
    /// `/review` or `/security-review`, built into gz.
    Review,
    /// One of the project's or the user's command files.
    CommandFile,
    /// `/<skill>`: a request to load one of the user's skills.
    Skill,
}

/// What a slash line typed outside the full-screen UI (a one-shot
/// `gz code "/review"`, the plain line mode) sends, as the UI would:
/// `/review`, `/security-review`, your own command files and
/// `/<skill>` (for `skills`, the usable skill names). `Ok(None)`: none of
/// those, so the line goes as typed. `Err` says what is wrong with it.
pub fn expand_line(
    line: &str,
    project_root: &Path,
    user_config: Option<&Path>,
    skills: &[String],
) -> Result<Option<Expanded>, String> {
    let line = line.trim();
    if !line.starts_with('/') {
        return Ok(None);
    }
    let (name, arguments) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let focus = match name {
        "/review" => Some(Focus::General),
        "/security-review" => Some(Focus::Security),
        _ => None,
    };
    if let Some(focus) = focus {
        let prompt = Target::parse(arguments)
            .and_then(|target| review::prompt(focus, &target, review::has_gh()))?;
        return Ok(Some(Expanded {
            prompt,
            model: None,
            source: name.to_string(),
            kind: ExpandedKind::Review,
        }));
    }
    if crate::code_ui::state::is_built_in(name) {
        return Ok(None);
    }
    let mut catalog = Catalog::load(Some(project_root), user_config);
    catalog.set_skills(skills);
    if let Some(found) = catalog.find(name) {
        return Ok(Some(Expanded {
            prompt: expand(found, arguments),
            model: found.model.clone(),
            source: found.path.display().to_string(),
            kind: ExpandedKind::CommandFile,
        }));
    }
    Ok(catalog.skill(name).map(|skill| Expanded {
        prompt: skill_prompt(skill, arguments),
        model: None,
        source: name.to_string(),
        kind: ExpandedKind::Skill,
    }))
}

/// The message `/skill-name arguments` sends: the arguments, and a request
/// to load and follow the skill.
pub fn skill_prompt(skill: &str, arguments: &str) -> String {
    let arguments = arguments.trim();
    let text = match arguments.is_empty() {
        true => format!("Use the {skill} skill."),
        false => arguments.to_string(),
    };
    format!(
        "{text}{}",
        crate::assistant::attachments::skill_request(&[skill.to_string()])
    )
}

/// The message `command` sends for `arguments`: `$ARGUMENTS` and
/// `$1`…`$9` filled in. Arguments given to a body that uses neither are
/// added after it, so they are never lost.
pub fn expand(command: &CustomCommand, arguments: &str) -> String {
    let arguments = arguments.trim();
    let words = split_arguments(arguments);
    let body = &command.body;
    let uses_placeholders = body.contains("$ARGUMENTS") || (1..=9).any(|n| has_positional(body, n));
    let mut out = String::with_capacity(body.len() + arguments.len());
    let mut rest = body.as_str();
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(arguments);
            rest = tail;
            continue;
        }
        match positional(after) {
            Some(n) => {
                out.push_str(words.get(n - 1).map(String::as_str).unwrap_or_default());
                rest = &after[1..];
            }
            None => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    if !uses_placeholders && !arguments.is_empty() {
        out.push_str("\n\n");
        out.push_str(arguments);
    }
    out
}

/// `Some(n)` when `after` (the text after a `$`) starts with one digit
/// 1–9 not followed by another digit.
fn positional(after: &str) -> Option<usize> {
    let mut chars = after.chars();
    let digit = chars.next()?.to_digit(10).filter(|d| *d >= 1)?;
    if chars.next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(digit as usize)
}

fn has_positional(body: &str, n: usize) -> bool {
    body.match_indices('$')
        .any(|(at, _)| positional(&body[at + 1..]) == Some(n))
}

/// Splits arguments into words; `"…"` and `'…'` keep spaces inside one.
pub fn split_arguments(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_line_expands_like_the_ui_outside_it() {
        let dir = crate::assistant::sandbox::tests::temp_dir("custom-expand-line");
        write(
            &dir.join(".gz/commands/deploy.md"),
            "---\nmodel: opus\n---\nDeploy to $1",
        );
        write(&dir.join(".gz/commands/help.md"), "not the help");
        let expand = |line: &str| expand_line(line, &dir, None, &["pdf".to_string()]);

        let deploy = expand("/deploy staging").unwrap().unwrap();
        assert_eq!(deploy.prompt, "Deploy to staging");
        assert_eq!(deploy.model.as_deref(), Some("opus"));
        assert!(deploy.source.ends_with("deploy.md"), "{}", deploy.source);

        let review = expand("/review").unwrap().unwrap();
        assert!(review.prompt.starts_with("Review the uncommitted changes"));
        assert!(expand("/review x;y")
            .unwrap_err()
            .contains("not a branch name"));

        let skill = expand("/pdf fill it").unwrap().unwrap();
        assert!(skill.prompt.starts_with("fill it"), "{}", skill.prompt);

        assert_eq!(expand("/help"), Ok(None), "a built-in wins");
        assert_eq!(expand("/nothing here"), Ok(None));
        assert_eq!(expand("plain text"), Ok(None));
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn command(body: &str) -> CustomCommand {
        CustomCommand {
            name: "/x".into(),
            description: String::new(),
            argument_hint: None,
            model: None,
            body: body.into(),
            scope: Scope::User,
            path: PathBuf::new(),
            menu_text: String::new(),
        }
    }

    #[test]
    fn folders_become_namespaces_and_the_project_wins() {
        let dir = crate::assistant::sandbox::tests::temp_dir("custom-commands");
        let project = dir.join("project");
        let user = dir.join("config");
        write(
            &project.join(".gz/commands/frontend/component.md"),
            "---\ndescription: Make a component\nargument-hint: <name>\nmodel: fast\n---\nMake $1.\n",
        );
        write(
            &project.join(".gz/commands/deploy.md"),
            "# Deploy the app\nSteps…",
        );
        write(&user.join("commands/deploy.md"), "mine");
        write(&user.join("commands/notes.txt"), "not a command");
        write(&user.join("commands/bad name.md"), "x");
        let catalog = Catalog::load(Some(&project), Some(&user));
        let names: Vec<&str> = catalog.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/deploy", "/frontend:component"]);
        let component = catalog.find("/frontend:component").unwrap();
        assert_eq!(component.description, "Make a component");
        assert_eq!(component.argument_hint.as_deref(), Some("<name>"));
        assert_eq!(component.model.as_deref(), Some("fast"));
        assert_eq!(component.menu_text, "Make a component · <name> (project)");
        let deploy = catalog.find("/deploy").unwrap();
        assert_eq!(deploy.scope, Scope::Project);
        assert_eq!(deploy.description, "Deploy the app");
        assert_eq!(catalog.hidden.len(), 1, "the user's /deploy is hidden");
        assert_eq!(catalog.problems.len(), 1, "{:?}", catalog.problems);
        assert!(catalog.problems[0].contains("'bad name' cannot be part of a command name"));
    }

    #[test]
    fn broken_files_are_named_and_skipped() {
        let dir = crate::assistant::sandbox::tests::temp_dir("custom-commands-bad");
        write(
            &dir.join("commands/open.md"),
            "---\ndescription: x\nno close",
        );
        write(
            &dir.join("commands/empty.md"),
            "---\ndescription: x\n---\n\n",
        );
        write(
            &dir.join("commands/yaml.md"),
            "---\ndescription: [\n---\nbody",
        );
        let catalog = Catalog::load(None, Some(&dir));
        assert!(catalog.commands.is_empty());
        let problems = catalog.problems.join("\n");
        assert!(problems.contains("not closed with `---`"), "{problems}");
        assert!(problems.contains("has no text to send"), "{problems}");
        assert!(problems.contains("not valid YAML"), "{problems}");
    }

    #[test]
    fn arguments_fill_placeholders() {
        let both = command("Fix $1 in $2 ($ARGUMENTS) for $$ and $10 and $0");
        assert_eq!(
            expand(&both, "issue-12 \"src/a b.rs\""),
            "Fix issue-12 in src/a b.rs (issue-12 \"src/a b.rs\") for $$ and $10 and $0"
        );
        assert_eq!(expand(&both, ""), "Fix  in  () for $$ and $10 and $0");
        let plain = command("Review the diff.");
        assert_eq!(
            expand(&plain, "focus on tests"),
            "Review the diff.\n\nfocus on tests"
        );
        assert_eq!(expand(&plain, "  "), "Review the diff.");
    }

    #[test]
    fn quotes_group_words() {
        assert_eq!(
            split_arguments(r#"a "b c" 'd e' f"g h" """#),
            vec!["a", "b c", "d e", "fg h", ""]
        );
    }
}

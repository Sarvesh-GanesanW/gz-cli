//! One rule's pattern: `tool` alone (every use of it) or `tool(specifier)`.
//! Parsing is pure; [`Pattern::compile`] resolves paths against a folder
//! and the home folder so matching needs no further context.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobMatcher};

use crate::assistant::permissions::command_starts_with;
use crate::assistant::sandbox::{normalize, real_path};

use super::shell;

/// The tool that reads one file.
pub const READ_FILE: &str = "read_file";
/// Tools that read files, by path.
const READ_TOOLS: &[&str] = &[READ_FILE, "list_directory", "find_files", "search_files"];
/// Tools that change files, by path.
pub(super) const EDIT_TOOLS: &[&str] = &["edit_file", "write_file", "notebook_edit"];
const COMMAND_TOOL: &str = "run_command";
const SCRIPT_TOOL: &str = "run_script";
const FETCH_TOOL: &str = "web_fetch";
const MCP_TOOL: &str = "local_mcp_call";
/// The folder read that reads file contents, not just names.
const SEARCH_TOOL: &str = "search_files";
/// Reads that cover a whole folder rather than one file.
const FOLDER_READS: &[&str] = &["list_directory", "find_files", "search_files"];
/// The suffix that turns a command into "commands starting with".
const PREFIX_SUFFIX: &str = ":*";
const DOMAIN_PREFIX: &str = "domain:";

/// Every name a rule may start with, for error messages and `/help`.
pub const TOOL_NAMES: &str = "read, edit, read_file, list_directory, find_files, search_files, \
                              edit_file, write_file, notebook_edit, run_command, run_script, \
                              web_fetch, local_mcp_call (or mcp)";

/// What a rule's specifier says, before paths are resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Spec {
    /// The tool name alone: every use.
    Any,
    /// An exact command line.
    Command(String),
    /// Commands that start with these words (`git status:*`).
    CommandPrefix(String),
    /// A path or glob, as typed.
    Path(String),
    /// A host, or `*.host` for its subdomains.
    Domain(String),
    /// A local MCP server, or one of its tools.
    Mcp {
        server: String,
        tool: Option<String>,
    },
}

/// A parsed rule pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    tools: &'static [&'static str],
    spec: Spec,
}

impl Pattern {
    /// Parses `tool` or `tool(specifier)`; errors say what form fits.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (name, spec) = match text.find('(') {
            Some(open) => {
                let Some(inner) = text[open + 1..].strip_suffix(')') else {
                    bail!("'{text}' has no closing ')'; write it as tool(pattern)");
                };
                (text[..open].trim(), Some(inner.trim()))
            }
            None => (text, None),
        };
        let name = name.to_ascii_lowercase();
        let tools = tools_named(&name).with_context(|| {
            format!("'{name}' isn't a tool a rule can name; use one of: {TOOL_NAMES}")
        })?;
        let spec = match spec {
            None => Spec::Any,
            Some("") => bail!("'{text}' has an empty pattern; drop the () to mean every use"),
            Some(spec) => parse_spec(tools, spec)?,
        };
        Ok(Self { tools, spec })
    }

    /// Resolves path patterns against `folder` (relative ones) and `home`
    /// (`~/…`), and through symlinks, ready to match.
    pub fn compile(&self, folder: &Path, home: Option<&Path>) -> Result<Compiled> {
        let matcher = match &self.spec {
            Spec::Any => Matcher::Any,
            Spec::Command(line) => Matcher::Command(line.clone()),
            Spec::CommandPrefix(start) => Matcher::CommandPrefix(start.clone()),
            Spec::Domain(host) => Matcher::Domain(host.clone()),
            Spec::Mcp { server, tool } => Matcher::Mcp {
                server: server.clone(),
                tool: tool.clone(),
            },
            Spec::Path(text) => Matcher::Path(PathPattern::new(text, folder, home)?),
        };
        Ok(Compiled {
            tools: self.tools,
            matcher,
        })
    }
}

fn tools_named(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "read" => READ_TOOLS,
        "edit" => EDIT_TOOLS,
        "read_file" => &READ_TOOLS[0..1],
        "list_directory" => &READ_TOOLS[1..2],
        "find_files" => &READ_TOOLS[2..3],
        "search_files" => &READ_TOOLS[3..4],
        "edit_file" => &EDIT_TOOLS[0..1],
        "write_file" => &EDIT_TOOLS[1..2],
        "notebook_edit" => &EDIT_TOOLS[2..3],
        COMMAND_TOOL => &[COMMAND_TOOL],
        SCRIPT_TOOL => &[SCRIPT_TOOL],
        FETCH_TOOL => &[FETCH_TOOL],
        MCP_TOOL | "mcp" => &[MCP_TOOL],
        _ => return None,
    })
}

fn parse_spec(tools: &'static [&'static str], spec: &str) -> Result<Spec> {
    match tools.first().copied() {
        Some(COMMAND_TOOL) => command_spec(spec),
        Some(SCRIPT_TOOL) => {
            bail!("run_script takes no pattern; write run_script alone to mean every script")
        }
        Some(FETCH_TOOL) => domain_spec(spec),
        Some(MCP_TOOL) => mcp_spec(spec),
        _ => Ok(Spec::Path(spec.to_string())),
    }
}

fn command_spec(spec: &str) -> Result<Spec> {
    let Some(start) = spec.strip_suffix(PREFIX_SUFFIX) else {
        return Ok(Spec::Command(spec.to_string()));
    };
    let start = start.trim();
    if start.is_empty() {
        bail!("name the command before :*, e.g. run_command(git status:*)");
    }
    if crate::assistant::permissions::has_shell_syntax(start) {
        bail!(
            "'{start}' has shell syntax (; | & > $ …); a :* rule names one program and how its \
             arguments start"
        );
    }
    Ok(Spec::CommandPrefix(start.to_string()))
}

fn domain_spec(spec: &str) -> Result<Spec> {
    let Some(host) = spec.strip_prefix(DOMAIN_PREFIX) else {
        bail!("web_fetch rules name a host as domain:<host>, e.g. web_fetch(domain:docs.rs)");
    };
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let bare = host.strip_prefix("*.").unwrap_or(&host);
    let valid = !bare.is_empty()
        && bare
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if !valid {
        bail!("'{host}' isn't a host; use one like docs.rs or *.example.com");
    }
    Ok(Spec::Domain(host))
}

fn mcp_spec(spec: &str) -> Result<Spec> {
    let (server, tool) = match spec.split_once('/') {
        Some((server, "*")) => (server, None),
        Some((server, tool)) => (server, Some(tool.trim().to_string())),
        None => (spec, None),
    };
    let server = server.trim();
    if server.is_empty() || tool.as_deref() == Some("") {
        bail!("local MCP rules name a server or server/tool, e.g. mcp(github/create_issue)");
    }
    Ok(Spec::Mcp {
        server: server.to_string(),
        tool,
    })
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.tools {
            READ_TOOLS => "read",
            EDIT_TOOLS => "edit",
            [one] => one,
            _ => "?",
        };
        match &self.spec {
            Spec::Any => write!(f, "{name}"),
            Spec::Command(line) => write!(f, "{name}({line})"),
            Spec::CommandPrefix(start) => write!(f, "{name}({start}{PREFIX_SUFFIX})"),
            Spec::Path(path) => write!(f, "{name}({path})"),
            Spec::Domain(host) => write!(f, "{name}({DOMAIN_PREFIX}{host})"),
            Spec::Mcp { server, tool: None } => write!(f, "{name}({server})"),
            Spec::Mcp {
                server,
                tool: Some(tool),
            } => write!(f, "{name}({server}/{tool})"),
        }
    }
}

/// What one tool call is, as rules see it.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub tool: &'a str,
    /// The real (absolute, symlink-free) path a file tool works on.
    pub path: Option<&'a Path>,
    /// The command line, for `run_command`.
    pub command: Option<&'a str>,
    /// The host, for `web_fetch`.
    pub host: Option<&'a str>,
    /// `server/tool`, for `local_mcp_call`.
    pub local_tool: Option<&'a str>,
}

impl Call<'_> {
    /// A listing or search of a whole folder (not one file).
    fn reads_folder(&self) -> bool {
        FOLDER_READS.contains(&self.tool) && self.path.is_some_and(Path::is_dir)
    }
}

/// How strictly a pattern is matched: an allow rule must cover all of
/// what the call does; a deny or ask rule catches any part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    Covers,
    Touches,
}

/// A pattern ready to match calls.
#[derive(Debug, Clone)]
pub struct Compiled {
    tools: &'static [&'static str],
    matcher: Matcher,
}

#[derive(Debug, Clone)]
enum Matcher {
    Any,
    Command(String),
    CommandPrefix(String),
    Path(PathPattern),
    Domain(String),
    Mcp {
        server: String,
        tool: Option<String>,
    },
}

/// How a rule's file tools use the files they name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileUse {
    Read,
    Edit,
}

impl Compiled {
    /// Whether this rule names a tool that uses files as `used`.
    pub fn names_file_use(&self, used: FileUse) -> bool {
        let family = match used {
            FileUse::Read => READ_TOOLS,
            FileUse::Edit => EDIT_TOOLS,
        };
        self.tools.iter().any(|tool| family.contains(tool))
    }

    /// Whether a deny or ask command rule can't tell if `call` runs what
    /// it names, because the line's program is only known at run time
    /// (`$CMD x`, `curl … | sh`).
    pub fn is_unclear(&self, call: &Call) -> bool {
        let names_commands = matches!(
            self.matcher,
            Matcher::Command(_) | Matcher::CommandPrefix(_)
        );
        names_commands
            && self.tools.contains(&call.tool)
            && call.command.is_some_and(|line| shell::analyse(line).opaque)
    }

    pub fn matches(&self, call: &Call, reach: Reach) -> bool {
        if !self.tools.contains(&call.tool) {
            return false;
        }
        match &self.matcher {
            Matcher::Any => true,
            Matcher::Command(line) => call
                .command
                .is_some_and(|command| command_is(command, line, reach)),
            Matcher::CommandPrefix(start) => call
                .command
                .is_some_and(|command| command_starts(command, start, reach)),
            Matcher::Path(pattern) => call.path.is_some_and(|path| {
                if call.reads_folder() {
                    pattern.matches_folder(path, reach, call.tool == SEARCH_TOOL)
                } else {
                    pattern.matches(path, reach)
                }
            }),
            Matcher::Domain(host) => call.host.is_some_and(|h| domain_matches(host, h)),
            Matcher::Mcp { server, tool } => call
                .local_tool
                .is_some_and(|target| mcp_matches(server, tool.as_deref(), target)),
        }
    }
}

/// Exact command rules: an allow rule matches the whole line only; a deny
/// or ask rule also matches any one command the line runs.
fn command_is(command: &str, line: &str, reach: Reach) -> bool {
    let command = command.trim();
    match reach {
        Reach::Covers => command == line,
        Reach::Touches => {
            let rule = rule_form(line);
            command == line
                || shell::analyse(command)
                    .forms
                    .iter()
                    .any(|form| *form == line || *form == rule)
        }
    }
}

/// Prefix rules: an allow rule never matches a line with shell syntax
/// (`git status; rm -rf ~` starts with `git status`); a deny or ask rule
/// matches if any command the line runs starts that way, also behind
/// wrappers like `sudo -u root`, inside `sh -c '…'` and with the
/// program's full path or quotes.
fn command_starts(command: &str, start: &str, reach: Reach) -> bool {
    match reach {
        Reach::Covers => command_starts_with(command, start),
        Reach::Touches => {
            let rule = rule_form(start);
            shell::analyse(command)
                .forms
                .iter()
                .any(|form| starts_with_words(form, start) || starts_with_words(form, &rule))
        }
    }
}

/// `form` is `start` alone or `start` and more words.
fn starts_with_words(form: &str, start: &str) -> bool {
    form.strip_prefix(start)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// A rule's command the way [`shell::analyse`] writes commands: single
/// spaces and the program without its folder.
fn rule_form(text: &str) -> String {
    let mut words = text.split_whitespace();
    let program = words.next().map(shell::base_name).unwrap_or_default();
    std::iter::once(program)
        .chain(words)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `docs.rs.` (with the root's trailing dot) is the same host as
/// `docs.rs`.
fn domain_matches(rule: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    match rule.strip_prefix("*.") {
        Some(parent) => host
            .strip_suffix(parent)
            .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1),
        None => host == rule,
    }
}

fn mcp_matches(server: &str, tool: Option<&str>, target: &str) -> bool {
    let Some((call_server, call_tool)) = target.split_once('/') else {
        return false;
    };
    call_server == server && tool.is_none_or(|tool| tool == call_tool)
}

/// A path rule resolved to real paths: a plain path means that file or
/// folder and everything in it; a glob (`*`, `**`, `?`, `[…]`, `{…}`)
/// matches whole paths, with `*` staying inside one folder.
#[derive(Debug, Clone)]
struct PathPattern {
    /// The part before any glob character, through symlinks.
    base: PathBuf,
    glob: Option<GlobMatcher>,
    /// The glob's folder levels below `base`, one matcher each, to tell
    /// which folders could hold a matching file.
    levels: Vec<Level>,
}

#[derive(Debug, Clone)]
enum Level {
    Name(GlobMatcher),
    /// `**`, or a level too complex to split: any depth could match.
    AnyDepth,
}

/// A name no real file has, to ask whether a glob covers a whole folder.
const PROBE: &str = "\u{1}gz-probe";

impl PathPattern {
    fn new(text: &str, folder: &Path, home: Option<&Path>) -> Result<Self> {
        let absolute = absolute(text, folder, home)?;
        let normalized = normalize(&absolute);
        let (literal, rest) = split_literal(&normalized);
        let base = real_path(&literal).unwrap_or(literal);
        let levels = rest.as_deref().map(levels_of).unwrap_or_default();
        let glob = match rest {
            None => None,
            Some(rest) => {
                // Globs always separate folders with `/`, Windows too.
                let base_text = base.to_string_lossy();
                let base_text = if cfg!(windows) {
                    base_text.replace('\\', "/")
                } else {
                    base_text.into_owned()
                };
                let text = format!(
                    "{}/{rest}",
                    globset::escape(&base_text).trim_end_matches('/')
                );
                let glob = GlobBuilder::new(&text)
                    .literal_separator(true)
                    .build()
                    .with_context(|| format!("'{text}' isn't a valid glob"))?;
                Some(glob.compile_matcher())
            }
        };
        Ok(Self { base, glob, levels })
    }

    /// A deny or ask rule that matches a folder also catches what is in it.
    fn matches(&self, path: &Path, reach: Reach) -> bool {
        match (&self.glob, reach) {
            (None, _) => path.starts_with(&self.base),
            (Some(glob), Reach::Covers) => glob.is_match(path),
            (Some(glob), Reach::Touches) => path.ancestors().any(|a| glob.is_match(a)),
        }
    }

    /// For a call that reads a whole folder: an allow rule must cover
    /// everything in it. A deny or ask rule catches a listing of a folder
    /// whose every entry the rule covers, and a search (`reads_contents`)
    /// of any folder that could hold a file the rule names, since it reads
    /// them; a listing elsewhere only shows names, so it isn't caught.
    fn matches_folder(&self, folder: &Path, reach: Reach, reads_contents: bool) -> bool {
        if self.matches(folder, reach) {
            return true;
        }
        match reach {
            Reach::Covers => {
                let deep = folder.join(PROBE).join(PROBE);
                self.glob
                    .as_ref()
                    .is_some_and(|glob| glob.is_match(folder.join(PROBE)) && glob.is_match(deep))
            }
            Reach::Touches => match &self.glob {
                None => {
                    folder.starts_with(&self.base)
                        || (reads_contents && self.base.starts_with(folder))
                }
                Some(glob) => {
                    glob.is_match(folder.join(PROBE))
                        || (reads_contents && self.could_hold_match(folder))
                }
            },
        }
    }

    /// Whether some file at or below `folder` could match the glob.
    fn could_hold_match(&self, folder: &Path) -> bool {
        let Ok(below) = folder.strip_prefix(&self.base) else {
            return self.base.starts_with(folder);
        };
        let names: Vec<String> = below
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        for (depth, name) in names.iter().enumerate() {
            match self.levels.get(depth) {
                Some(Level::AnyDepth) => return true,
                Some(Level::Name(level)) if level.is_match(name) => {}
                // Deeper than the glob reaches, or a level that differs.
                _ => return false,
            }
        }
        self.levels.len() > names.len()
    }
}

fn absolute(text: &str, folder: &Path, home: Option<&Path>) -> Result<PathBuf> {
    if let Some(rest) = text.strip_prefix("~/").or((text == "~").then_some("")) {
        let home = home.context("a ~ path needs the home folder, which gz couldn't find")?;
        return Ok(home.join(rest));
    }
    if text.starts_with('~') {
        bail!("'{text}': only ~ and ~/… are supported");
    }
    let path = Path::new(text);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        folder.join(path)
    })
}

/// One matcher per folder level of the glob text after the literal base.
fn levels_of(rest: &str) -> Vec<Level> {
    rest.split('/')
        .map(|level| {
            if level.contains("**") {
                return Level::AnyDepth;
            }
            GlobBuilder::new(level)
                .literal_separator(true)
                .build()
                .map_or(Level::AnyDepth, |glob| Level::Name(glob.compile_matcher()))
        })
        .collect()
}

/// The leading components without glob characters, and the rest (if any)
/// as glob text.
fn split_literal(path: &Path) -> (PathBuf, Option<String>) {
    let mut literal = PathBuf::new();
    let mut components = path.components();
    for component in components.by_ref() {
        let text = component.as_os_str().to_string_lossy();
        if text.contains(['*', '?', '[', ']', '{', '}']) {
            let rest: Vec<String> = std::iter::once(text.into_owned())
                .chain(
                    components
                        .by_ref()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned()),
                )
                .collect();
            return (literal, Some(rest.join("/")));
        }
        literal.push(component);
    }
    (literal, None)
}

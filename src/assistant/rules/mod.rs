//! Permission rules: lists of `allow`, `ask` and `deny` patterns the user
//! (or a project) writes down, checked before gz asks about a tool call.
//!
//! Sources, highest first: command-line flags (`--allowedTools`,
//! `--disallowedTools`), the private project file
//! `.gz/permissions.local.toml`, the shared project file
//! `.gz/permissions.toml`, and the user's `permissions.toml` in the gz
//! config folder. Across all of them deny beats ask beats allow. A
//! project file is code from a repository, so its allow rules (and the
//! mode it picks) count only once the user trusted that version of it
//! (`/permissions trust`); its deny and ask rules always count.
//!
//! The one entry point is [`RuleSet::verdict`]; the session calls it once
//! per tool call, after hooks and before grants, modes and prompts.

mod files;
mod gate;
mod guarded;
mod pattern;
mod shell;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::Path;

use anyhow::{bail, Result};

use crate::assistant::permissions::Refusal;
use crate::assistant::sandbox::Sandbox;
use crate::assistant::tools::{Access, Planned};

pub use files::{add_rule, remove_rule, trust_project, Places, Scope, Settings};
pub use gate::Gated;
pub use guarded::holds_settings;
pub use pattern::{Call, FileUse, Pattern, Reach, TOOL_NAMES};

/// The permission modes `default_mode` and `--permission-mode` name
/// (each also has a camel-case spelling, and `ask` is also `default`).
pub const MODE_NAMES: [&str; 8] = [
    "ask",
    "default",
    "accept-edits",
    "acceptEdits",
    "plan",
    "auto",
    "dont-ask",
    "dontAsk",
];

/// What a rule does with the calls it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Deny,
    Ask,
    Allow,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Deny, Kind::Ask, Kind::Allow];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Allow => "allow",
            Kind::Ask => "ask",
            Kind::Deny => "deny",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        Kind::ALL.into_iter().find(|kind| kind.as_str() == word)
    }

    fn reach(self) -> Reach {
        match self {
            Kind::Allow => Reach::Covers,
            Kind::Ask | Kind::Deny => Reach::Touches,
        }
    }
}

/// Where a rule comes from, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `--allowedTools` / `--disallowedTools` for this session.
    CommandLine,
    /// `.gz/permissions.local.toml`: this user's, in this project.
    Local,
    /// `.gz/permissions.toml`: shared with everyone in the project.
    Project,
    /// `permissions.toml` in the gz config folder.
    User,
    /// gz's own: writes to the project files that hold rules, hooks and
    /// MCP servers always ask ([`guarded`]).
    BuiltIn,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::CommandLine => "command line",
            Source::Local => "local",
            Source::Project => "project",
            Source::User => "user",
            Source::BuiltIn => "built-in",
        }
    }

    /// Where the rule is written, told to the model (`to_user` false) or to
    /// the user.
    fn origin(self, to_user: bool) -> &'static str {
        match (self, to_user) {
            (Source::CommandLine, _) => "the command line",
            (Source::Local, _) => "the project's .gz/permissions.local.toml",
            (Source::Project, _) => "the project's .gz/permissions.toml",
            (Source::User, true) => "your permissions.toml",
            (Source::User, false) => "the user's permissions.toml",
            (Source::BuiltIn, _) => "gz itself",
        }
    }

    /// Whether this is a file from the project folder, trusted per version.
    fn is_project(self) -> bool {
        matches!(self, Source::Local | Source::Project)
    }
}

/// One rule as loaded.
#[derive(Debug, Clone)]
pub struct Rule {
    pub kind: Kind,
    pub source: Source,
    pub pattern: Pattern,
    /// False for a project allow rule the user hasn't trusted yet.
    pub active: bool,
    compiled: pattern::Compiled,
}

/// The rule that decided a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleVerdict {
    pub kind: Kind,
    pub source: Source,
    /// The rule as written, e.g. `run_command(rm:*)`.
    pub rule: String,
    /// A deny or ask command rule couldn't see what the line runs, so it
    /// asks (`kind` is then [`Kind::Ask`]).
    pub unclear: bool,
}

impl RuleVerdict {
    /// What the model is told for a deny rule: final, with the reason.
    pub fn refusal(&self) -> Refusal {
        Refusal::Blocked {
            by: "a deny rule",
            reason: format!(
                "the deny rule `{}` from {} forbids it.",
                self.rule,
                self.source.origin(false)
            ),
            allow_with: None,
        }
    }

    /// The line shown above the prompt an ask rule forces.
    pub fn ask_note(&self) -> String {
        let origin = self.source.origin(true);
        if self.unclear {
            return format!(
                "gz can't tell which program this line runs until it runs, so the rule `{}` \
                 from {origin} can't be checked; it asks instead.",
                self.rule
            );
        }
        format!(
            "The ask rule `{}` from {origin} says to always ask about this.",
            self.rule
        )
    }
}

/// Rules from `--allowedTools` and `--disallowedTools`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlagRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl FlagRules {
    /// Splits each flag value at commas and spaces outside parentheses,
    /// so `--allowedTools "read run_command(git status:*)"` is two rules.
    pub fn new(allow: &[String], deny: &[String]) -> Result<Self> {
        let split = |values: &[String]| -> Result<Vec<String>> {
            let rules: Vec<String> = values.iter().flat_map(|v| split_rules(v)).collect();
            for rule in &rules {
                Pattern::parse(rule)?;
            }
            Ok(rules)
        };
        Ok(Self {
            allow: split(allow)?,
            deny: split(deny)?,
        })
    }
}

/// `a, b c(d e)` → `a`, `b`, `c(d e)`.
fn split_rules(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && (c == ',' || c.is_whitespace()) {
            if !current.trim().is_empty() {
                out.push(current.trim().to_string());
            }
            current.clear();
        } else {
            current.push(c);
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Every rule that applies in one project folder.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    /// Sorted by source, highest first.
    rules: Vec<Rule>,
    /// Rules or files that couldn't be read, to show the user.
    pub problems: Vec<String>,
    /// Mode and cycle choices from the files.
    pub settings: Settings,
    /// Where the rules came from, to read them again.
    places: Option<Places>,
    flags: FlagRules,
}

impl RuleSet {
    /// Reads every source for the project folder `places` names.
    pub fn load(places: Places, flags: FlagRules) -> Self {
        let mut set = RuleSet {
            places: Some(places.clone()),
            flags,
            ..RuleSet::default()
        };
        set.add_flags();
        for (source, loaded) in files::read_all(&places) {
            match loaded {
                Ok(Some(file)) => set.add_file(source, file, &places),
                Ok(None) => {}
                Err(problem) => set.problems.push(problem),
            }
        }
        set.rules.sort_by_key(|rule| rule.source);
        set
    }

    /// The rules for the folder `sandbox` started in.
    pub fn for_sandbox(sandbox: &Sandbox) -> Self {
        let mut places = Places::for_folder(&sandbox.start(), sandbox.home());
        // Unit tests must never follow the developer's own rules.
        if cfg!(test) {
            places.user_file = None;
            places.trust = None;
        }
        Self::load(places, FlagRules::default())
    }

    /// The same sources read again (after an edit elsewhere).
    pub fn reloaded(&self) -> Self {
        match &self.places {
            Some(places) => Self::load(places.clone(), self.flags.clone()),
            None => self.clone(),
        }
    }

    /// This set with `flags` for the command-line source.
    pub fn with_flags(&self, flags: FlagRules) -> Self {
        match &self.places {
            Some(places) => Self::load(places.clone(), flags),
            None => {
                let mut set = RuleSet {
                    flags,
                    ..RuleSet::default()
                };
                set.add_flags();
                set
            }
        }
    }

    fn add_flags(&mut self) {
        let folder = self
            .places
            .as_ref()
            .map(|p| p.folder.clone())
            .unwrap_or_default();
        let home = self.places.as_ref().and_then(|p| p.home.clone());
        let flags = self.flags.clone();
        for (kind, texts) in [(Kind::Allow, &flags.allow), (Kind::Deny, &flags.deny)] {
            for text in texts {
                self.push(
                    Source::CommandLine,
                    kind,
                    text,
                    true,
                    &folder,
                    home.as_deref(),
                );
            }
        }
    }

    fn add_file(&mut self, source: Source, mut file: files::Loaded, places: &Places) {
        let trusted = !source.is_project() || file.trusted;
        if let Some(mode) = file
            .settings
            .default_mode
            .take_if(|mode| !MODE_NAMES.contains(&mode.as_str()))
        {
            self.problems.push(format!(
                "{} default_mode \"{mode}\" isn't a mode and is ignored; use one of: {}",
                source.label(),
                MODE_NAMES.join(", ")
            ));
        }
        if source == Source::User || trusted {
            self.settings.merge(&file.settings);
        }
        for kind in Kind::ALL {
            for text in file.lists.of(kind) {
                let active = kind != Kind::Allow || trusted;
                self.push(
                    source,
                    kind,
                    text,
                    active,
                    &places.folder,
                    places.home.as_deref(),
                );
            }
        }
    }

    fn push(
        &mut self,
        source: Source,
        kind: Kind,
        text: &str,
        active: bool,
        folder: &Path,
        home: Option<&Path>,
    ) {
        let parsed = Pattern::parse(text).and_then(|p| Ok((p.compile(folder, home)?, p)));
        match parsed {
            Ok((compiled, pattern)) => self.rules.push(Rule {
                kind,
                source,
                pattern,
                active,
                compiled,
            }),
            Err(err) => self.problems.push(format!(
                "{} {} rule `{text}` is ignored: {err:#}",
                source.label(),
                kind.as_str()
            )),
        }
    }

    /// Every rule, highest source first.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The project folder these rules were read for.
    pub fn places(&self) -> Option<&Places> {
        self.places.as_ref()
    }

    /// The rule that decides `planned`, if any: a deny rule anywhere wins,
    /// then an ask rule, then an allow rule.
    pub fn verdict(&self, planned: &Planned) -> Option<RuleVerdict> {
        let call = call_of(planned)?;
        self.verdict_for(&call)
    }

    /// The verdict for `call`: a matching deny rule, else a matching ask
    /// rule, else gz's own ask for an edit of its project settings, else an
    /// ask for a command line a deny or ask rule can't see into, else a
    /// matching allow rule.
    pub fn verdict_for(&self, call: &Call) -> Option<RuleVerdict> {
        let matching = |kind: Kind| {
            self.active(kind)
                .find(|rule| rule.compiled.matches(call, kind.reach()))
                .map(|rule| rule.verdict(kind, false))
        };
        let unclear = || {
            self.active(Kind::Deny)
                .chain(self.active(Kind::Ask))
                .find(|rule| rule.compiled.is_unclear(call))
                .map(|rule| rule.verdict(Kind::Ask, true))
        };
        matching(Kind::Deny)
            .or_else(|| matching(Kind::Ask))
            .or_else(|| guarded::verdict(call))
            .or_else(unclear)
            .or_else(|| matching(Kind::Allow))
    }

    /// The deny or ask rule that covers reading `path` (real and
    /// absolute), as written.
    pub fn read_refusal(&self, path: &Path) -> Option<String> {
        let call = Call {
            tool: pattern::READ_FILE,
            path: Some(path),
            command: None,
            host: None,
            local_tool: None,
        };
        self.verdict_for(&call)
            .filter(|verdict| verdict.kind != Kind::Allow)
            .map(|verdict| verdict.rule)
    }

    /// Whether an active deny or ask rule names files used as `used`.
    /// Commands and scripts can reach any file without naming it on the
    /// command line, so gz never runs one without asking while such a rule
    /// stands: only the prompt (or the rule, for the file tools) decides.
    pub fn guards_files(&self, used: FileUse) -> bool {
        self.active(Kind::Deny)
            .chain(self.active(Kind::Ask))
            .any(|rule| rule.compiled.names_file_use(used))
    }

    fn active(&self, kind: Kind) -> impl Iterator<Item = &Rule> {
        self.rules
            .iter()
            .filter(move |rule| rule.kind == kind && rule.active)
    }
}

impl Rule {
    fn verdict(&self, kind: Kind, unclear: bool) -> RuleVerdict {
        RuleVerdict {
            kind,
            source: self.source,
            rule: self.pattern.to_string(),
            unclear,
        }
    }
}

/// What rules see of a planned call; `None` for calls rules never cover
/// (folder changes, server actions, installs).
fn call_of(planned: &Planned) -> Option<Call<'_>> {
    let tool = planned.tool_name();
    let mut call = Call {
        tool,
        path: None,
        command: None,
        host: None,
        local_tool: None,
    };
    match planned.access {
        Access::Read | Access::Write => call.path = planned.path(),
        Access::Command => call.command = planned.command().map(|(line, _, _)| line),
        Access::Fetch => call.host = planned.target.as_deref(),
        Access::LocalTool => call.local_tool = planned.target.as_deref(),
        Access::Script => {}
        Access::Folder | Access::ServerAction | Access::Install | Access::Trust => return None,
    }
    Some(call)
}

/// A rule as typed in `/permissions`: `allow|ask|deny [user|project|local]
/// <pattern>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEdit {
    pub kind: Kind,
    pub scope: Scope,
    pub pattern: String,
}

impl RuleEdit {
    /// Parses what follows `/permissions <kind>`.
    pub fn parse(kind: Kind, text: &str) -> Result<Self> {
        let text = text.trim();
        let (first, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let (scope, pattern) = match Scope::parse(first) {
            Some(scope) => (scope, rest.trim()),
            None => (Scope::User, text),
        };
        if pattern.is_empty() {
            bail!(
                "name the rule, e.g. /permissions {} {} run_command(git push:*)",
                kind.as_str(),
                scope.as_str()
            );
        }
        Ok(Self {
            kind,
            scope,
            pattern: Pattern::parse(pattern)?.to_string(),
        })
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind.as_str(), self.pattern)
    }
}

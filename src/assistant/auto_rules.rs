//! What auto mode knows about the user's preferences, kept next to the gz
//! config and never in a project, so a repository cannot grant itself
//! permissions:
//!
//! - `auto_mode.toml`: the user's own rules in plain language, which the
//!   review applies before its defaults (a deny always blocks, an allow
//!   overrides a default block, environment names what the user trusts).
//!   Edited with `/auto` or by hand.
//! - `auto_mode_history.toml`: how the user answered permission prompts in
//!   each folder, which the review treats as precedent for similar actions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::assistant::private_file::write_private;

/// Most rules of one kind; the review endpoint accepts no more.
pub const MAX_RULES: usize = 50;
/// Longest rule or remembered action, in characters.
pub const MAX_LINE_CHARS: usize = 500;
/// Answers remembered per folder; older ones are forgotten first.
pub const MAX_DECISIONS: usize = 40;

/// The user's own auto mode rules. Field names are the review endpoint's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub environment: Vec<String>,
}

/// Which list a rule belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Allow,
    Deny,
    Environment,
}

impl RuleKind {
    /// `allow`, `deny` or `env`, as typed after `/auto`.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "env" | "environment" => Some(Self::Environment),
            _ => None,
        }
    }

    fn list(self, rules: &mut AutoRules) -> &mut Vec<String> {
        match self {
            Self::Allow => &mut rules.allow,
            Self::Deny => &mut rules.deny,
            Self::Environment => &mut rules.environment,
        }
    }
}

/// How the user answered one earlier prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PastDecision {
    pub action: String,
    pub decision: Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Allowed,
    Denied,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct History {
    /// Workspace folder → its answers, oldest first.
    #[serde(default)]
    folders: BTreeMap<PathBuf, Vec<PastDecision>>,
}

/// The two files, at the default location or (in tests) a given folder.
#[derive(Debug, Clone)]
pub struct AutoStore {
    rules: PathBuf,
    history: PathBuf,
}

impl AutoStore {
    /// The files in the gz config folder (honors `XDG_CONFIG_HOME`).
    pub fn open() -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Ok(Self::in_folder(dir))
    }

    pub fn in_folder(dir: &Path) -> Self {
        Self {
            rules: dir.join("auto_mode.toml"),
            history: dir.join("auto_mode_history.toml"),
        }
    }

    pub fn rules(&self) -> Result<AutoRules> {
        read_toml(&self.rules)
    }

    /// Adds a rule; an existing identical rule is not added twice.
    pub fn add_rule(&self, kind: RuleKind, text: &str) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            bail!("the rule is empty");
        }
        if text.chars().count() > MAX_LINE_CHARS {
            bail!("a rule can be at most {MAX_LINE_CHARS} characters");
        }
        let mut rules = self.rules()?;
        let list = kind.list(&mut rules);
        if list.iter().any(|rule| rule == text) {
            return Ok(());
        }
        if list.len() >= MAX_RULES {
            bail!("there are already {MAX_RULES} rules of this kind; remove one first");
        }
        list.push(text.to_string());
        self.write_rules(&rules)
    }

    /// Removes the `number`th rule (from 1, as `/auto` lists them).
    pub fn remove_rule(&self, kind: RuleKind, number: usize) -> Result<String> {
        let mut rules = self.rules()?;
        let list = kind.list(&mut rules);
        if number == 0 || number > list.len() {
            bail!("there is no rule {number}; /auto lists them");
        }
        let removed = list.remove(number - 1);
        self.write_rules(&rules)?;
        Ok(removed)
    }

    /// The user's answers in `root`, oldest first.
    pub fn decisions_for(&self, root: &Path) -> Result<Vec<PastDecision>> {
        let mut history: History = read_toml(&self.history)?;
        Ok(history.folders.remove(root).unwrap_or_default())
    }

    /// Remembers an answer in `root`, forgetting the oldest beyond
    /// [`MAX_DECISIONS`]. The same action keeps only its latest answer.
    pub fn record_decision(&self, root: &Path, action: &str, answer: Answer) -> Result<()> {
        let action: String = action.chars().take(MAX_LINE_CHARS).collect();
        let mut history: History = read_toml(&self.history)?;
        let decisions = history.folders.entry(root.to_path_buf()).or_default();
        decisions.retain(|past| past.action != action);
        decisions.push(PastDecision {
            action,
            decision: answer,
        });
        let excess = decisions.len().saturating_sub(MAX_DECISIONS);
        decisions.drain(..excess);
        let text = toml::to_string_pretty(&history).context("cannot encode the history")?;
        write_private(&self.history, &text)
    }

    fn write_rules(&self, rules: &AutoRules) -> Result<()> {
        let text = toml::to_string_pretty(rules).context("cannot encode the rules")?;
        write_private(&self.rules, &text)
    }
}

fn read_toml<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> AutoStore {
        let dir = std::env::temp_dir().join(format!("gz-auto-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AutoStore::in_folder(&dir)
    }

    #[test]
    fn rules_are_added_once_listed_and_removed_by_number() {
        let store = store("rules");
        store
            .add_rule(RuleKind::Allow, " make deploy-docs ")
            .unwrap();
        store.add_rule(RuleKind::Allow, "make deploy-docs").unwrap();
        store.add_rule(RuleKind::Deny, "Pushing to main").unwrap();
        let rules = store.rules().unwrap();
        assert_eq!(rules.allow, vec!["make deploy-docs"]);
        assert_eq!(rules.deny, vec!["Pushing to main"]);
        assert_eq!(
            store.remove_rule(RuleKind::Deny, 1).unwrap(),
            "Pushing to main"
        );
        assert!(store.remove_rule(RuleKind::Deny, 1).is_err());
        assert!(store.add_rule(RuleKind::Environment, "  ").is_err());
    }

    #[test]
    fn decisions_are_per_folder_latest_wins_and_capped() {
        let store = store("history");
        let (a, b) = (Path::new("/work/a"), Path::new("/work/b"));
        store
            .record_decision(a, "run_command git push", Answer::Allowed)
            .unwrap();
        store
            .record_decision(a, "run_command git push", Answer::Denied)
            .unwrap();
        store
            .record_decision(b, "run_command make", Answer::Allowed)
            .unwrap();
        assert_eq!(
            store.decisions_for(a).unwrap(),
            vec![PastDecision {
                action: "run_command git push".into(),
                decision: Answer::Denied
            }]
        );
        for i in 0..MAX_DECISIONS + 5 {
            store
                .record_decision(b, &format!("cmd {i}"), Answer::Allowed)
                .unwrap();
        }
        let kept = store.decisions_for(b).unwrap();
        assert_eq!(kept.len(), MAX_DECISIONS);
        assert_eq!(
            kept.last().unwrap().action,
            format!("cmd {}", MAX_DECISIONS + 4)
        );
    }

    #[test]
    fn decisions_serialize_as_the_review_endpoint_expects() {
        let past = PastDecision {
            action: "x".into(),
            decision: Answer::Denied,
        };
        assert_eq!(
            serde_json::to_value(&past).unwrap(),
            serde_json::json!({"action": "x", "decision": "denied"})
        );
    }
}

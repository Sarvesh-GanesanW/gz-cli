//! Swaps names for ids on the command line before clap reads it.
//!
//! [`plan`] is pure: it finds the values to look up. [`apply`] looks them
//! up (projects first, so a job is looked for in the project given) and
//! returns the words with ids in their place.

use std::collections::HashMap;

use anyhow::Result;
use clap::Command;

use super::lookup::{Candidate, Catalog};
use super::pick;
use super::roles::{project_before_job, role_of, Role};
use super::walk::{leaf, walk, Walk};
use super::Kind;
use crate::cmd::Runtime;

/// A value to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Index of the word in the command line (program name left out).
    pub word: usize,
    /// What stays in front of the id in the same word, e.g. `--project=`.
    pub prefix: String,
    pub value: String,
    pub kind: Kind,
    /// A job given alone where the command takes `<PROJECT> <JOB>`: its
    /// project is put in front of it.
    pub adds_project: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub words: Vec<String>,
    pub edits: Vec<Edit>,
    /// A project given as an id on the same line.
    pub project: Option<i64>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// The words with `0` for each id still to look up, so clap can read
    /// the rest of the line (the profile, output format, usage errors).
    pub fn placeholder(&self) -> Vec<String> {
        let mut replaced = HashMap::new();
        for edit in &self.edits {
            let mut words = vec![format!("{}0", edit.prefix)];
            if edit.adds_project {
                words.insert(0, "0".to_string());
            }
            replaced.insert(edit.word, words);
        }
        splice(&self.words, &replaced)
    }
}

/// The values on `words` (program name left out) that are names of ids.
pub fn plan(root: &Command, words: &[String]) -> Plan {
    let walked = walk(root, words);
    let command = leaf(root, &walked.path);
    let key = walked.key();
    let lone_job = lone_job_arg(&walked, command, &key);
    let mut plan = Plan {
        words: words.to_vec(),
        ..Plan::default()
    };
    for filled in &walked.filled {
        let Some(arg) = command.get_arguments().find(|a| a.get_id() == &filled.arg) else {
            continue;
        };
        let Some(mut kind) = kind_of(&walked, &key, command, arg, filled.nth) else {
            continue;
        };
        let adds_project = lone_job.as_deref() == Some(filled.arg.as_str());
        if adds_project {
            kind = Kind::Job;
        }
        if !adds_project && kind.is_id(&filled.value) {
            if kind == Kind::Project {
                plan.project = filled.value.parse().ok();
            }
            continue;
        }
        plan.edits.push(Edit {
            word: filled.word,
            prefix: filled.prefix.clone(),
            value: filled.value.clone(),
            kind,
            adds_project,
        });
    }
    plan
}

/// The kind of id the `nth` value of `arg` is, if it is one.
fn kind_of(
    walked: &Walk,
    key: &str,
    command: &Command,
    arg: &clap::Arg,
    nth: usize,
) -> Option<Kind> {
    match role_of(key, command, arg)? {
        Role::Id(kind) => Some(kind),
        Role::Ids(kinds) => {
            let count = walked.values_of(arg.get_id().as_str()).len();
            let start = kinds.len().checked_sub(count)?;
            kinds.get(start + nth).copied().flatten()
        }
        Role::DesignerId => match walked.first_value("kind")? {
            "dataset" => Some(Kind::Dataset),
            "dashboard" => Some(Kind::Dashboard),
            "visualization" => Some(Kind::Visualization),
            "filter" => Some(Kind::Filter),
            _ => None,
        },
        Role::Name(_) | Role::Text(_) => None,
    }
}

/// `jobs run DQC`: a command taking `<PROJECT> <JOB>` given one value,
/// which is then the job. Returns the argument that value filled.
fn lone_job_arg(walked: &Walk, command: &Command, key: &str) -> Option<String> {
    let project = project_before_job(key, command)?;
    let positional_values = walked
        .filled
        .iter()
        .filter(|f| {
            command
                .get_arguments()
                .any(|a| a.is_positional() && a.get_id() == &f.arg)
        })
        .count();
    let project = project.get_id().as_str();
    (positional_values == 1 && walked.values_of(project).len() == 1).then(|| project.to_string())
}

/// Looks up every name in `plan` and returns the words with ids.
pub async fn apply(rt: &Runtime, plan: &Plan) -> Result<Vec<String>> {
    let mut catalog = Catalog::new(rt);
    let mut replaced = HashMap::new();
    let mut project = plan.project;
    let (projects, others): (Vec<&Edit>, Vec<&Edit>) =
        plan.edits.iter().partition(|e| e.kind == Kind::Project);
    for edit in projects.into_iter().chain(others) {
        let scope = project.filter(|_| edit.kind.in_project());
        let candidates = match edit.kind {
            Kind::LineageAsset => catalog.lineage_assets(&edit.value).await?,
            kind => catalog.list(kind, scope).await?,
        };
        let lookup = pick::Lookup {
            candidates: &candidates,
            by_id: edit.adds_project,
            scope,
        };
        let chosen = pick::choose(rt, edit.kind, &edit.value, lookup, |several| {
            several_message(plan, edit, several)
        })?;
        if edit.kind == Kind::Project {
            project = Some(chosen.id_number()?);
        }
        replaced.insert(edit.word, replacement(edit, &chosen));
    }
    Ok(splice(&plan.words, &replaced))
}

/// The words that take the place of `edit` once `chosen`.
fn replacement(edit: &Edit, chosen: &Candidate) -> Vec<String> {
    let id = format!("{}{}", edit.prefix, chosen.id);
    match (&chosen.project, edit.adds_project) {
        (Some((project, _)), true) => vec![project.to_string(), id],
        _ => vec![id],
    }
}

/// Several matches with no terminal to ask on: each one's exact command.
fn several_message(plan: &Plan, edit: &Edit, several: &[Candidate]) -> String {
    let mut message = format!(
        "several {}s are named '{}'; run the one you mean:",
        edit.kind.noun(),
        edit.value
    );
    for candidate in several {
        let mut replaced = HashMap::new();
        replaced.insert(edit.word, replacement(edit, candidate));
        let words = splice(&plan.words, &replaced);
        let line: Vec<String> = words.iter().map(|w| quote(w)).collect();
        message.push_str(&format!(
            "\n  gz {}    # {}",
            line.join(" "),
            candidate.describe()
        ));
    }
    message
}

fn splice(words: &[String], replaced: &HashMap<usize, Vec<String>>) -> Vec<String> {
    words
        .iter()
        .enumerate()
        .flat_map(|(at, word)| {
            replaced
                .get(&at)
                .cloned()
                .unwrap_or_else(|| vec![word.clone()])
        })
        .collect()
}

/// A word as a POSIX shell would need it typed.
pub fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::walk::command_tree;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn edits(line: &str) -> Vec<(usize, String, Kind, bool)> {
        plan(&command_tree(), &words(line))
            .edits
            .into_iter()
            .map(|e| (e.word, e.value, e.kind, e.adds_project))
            .collect()
    }

    #[test]
    fn numeric_values_are_left_alone() {
        let plan = plan(
            &command_tree(),
            &words("jobs run 1999 3013 --compute 4 --rte x"),
        );
        assert!(plan.is_empty());
        assert_eq!(plan.project, Some(1999));
    }

    #[test]
    fn names_of_ids_are_planned() {
        assert_eq!(
            edits("jobs run Sales DQC --compute small --rte x"),
            vec![
                (2, "Sales".into(), Kind::Project, false),
                (3, "DQC".into(), Kind::Job, false),
                (5, "small".into(), Kind::Compute, false),
            ]
        );
        assert_eq!(
            edits("projects get Sales"),
            vec![(2, "Sales".into(), Kind::Project, false)]
        );
        assert_eq!(
            edits("designer delete dashboard Revenue"),
            vec![(3, "Revenue".into(), Kind::Dashboard, false)]
        );
        assert_eq!(
            edits("agents get support-bot"),
            vec![(2, "support-bot".into(), Kind::Agent, false)]
        );
    }

    #[test]
    fn a_lone_job_brings_its_project() {
        assert_eq!(
            edits("jobs run DQC --compute 4 --rte x"),
            vec![(2, "DQC".into(), Kind::Job, true)]
        );
        // A lone job id is looked up too, to find its project.
        assert_eq!(
            edits("jobs run 3013 --compute 4 --rte x"),
            vec![(2, "3013".into(), Kind::Job, true)]
        );
    }

    #[test]
    fn listed_values_align_to_the_right() {
        assert_eq!(
            edits("projects rename-job DQC NewName"),
            vec![(2, "DQC".into(), Kind::Job, false)]
        );
        assert_eq!(
            edits("projects rename-job Sales DQC NewName"),
            vec![
                (2, "Sales".into(), Kind::Project, false),
                (3, "DQC".into(), Kind::Job, false)
            ]
        );
    }

    #[test]
    fn names_a_command_takes_itself_are_not_planned() {
        assert!(edits("fix clean-orders").is_empty());
        assert!(edits("sql --warehouse Lineage --compute small").is_empty());
        assert!(edits("warehouse namespaces Lineage").is_empty());
    }

    #[test]
    fn placeholders_keep_the_line_parseable() {
        let plan = plan(
            &command_tree(),
            &words("jobs run DQC --compute=small --rte x"),
        );
        assert_eq!(
            plan.placeholder(),
            words("jobs run 0 0 --compute=0 --rte x")
        );
    }

    #[test]
    fn replacements_keep_prefixes_and_add_projects() {
        let job = Candidate {
            id: "3013".into(),
            name: "DQC".into(),
            project: Some((1999, "Sales".into())),
            detail: None,
        };
        let lone = Edit {
            word: 2,
            prefix: String::new(),
            value: "DQC".into(),
            kind: Kind::Job,
            adds_project: true,
        };
        assert_eq!(replacement(&lone, &job), vec!["1999", "3013"]);
        let flag = Edit {
            prefix: "--job=".into(),
            adds_project: false,
            ..lone
        };
        assert_eq!(replacement(&flag, &job), vec!["--job=3013"]);
    }

    #[test]
    fn several_matches_list_exact_commands() {
        let plan = plan(&command_tree(), &words("jobs run DQC --rte x"));
        let several = vec![
            Candidate {
                id: "3013".into(),
                name: "DQC".into(),
                project: Some((1999, "Sales".into())),
                detail: None,
            },
            Candidate {
                id: "3050".into(),
                name: "dqc".into(),
                project: Some((2001, "Ops Team".into())),
                detail: None,
            },
        ];
        let message = several_message(&plan, &plan.edits[0], &several);
        assert!(
            message.contains(
                "gz jobs run 1999 3013 --rte x    # DQC (id 3013) in project Sales (1999)"
            ),
            "{message}"
        );
        assert!(
            message.contains("gz jobs run 2001 3050 --rte x"),
            "{message}"
        );
    }

    #[test]
    fn quotes_only_what_needs_it() {
        assert_eq!(quote("Lineage.sales.orders"), "Lineage.sales.orders");
        assert_eq!(quote("Home Health"), "'Home Health'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}

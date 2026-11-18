//! Which candidate a name means: an exact match wins, compared without
//! regard to case (a match in the same case first). Several matches are
//! chosen from a numbered list in a terminal; elsewhere they are an error
//! that lists them.

use anyhow::{bail, Result};

use super::lookup::Candidate;
use super::Kind;
use crate::cmd::Runtime;

/// How a name matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matched {
    One(Candidate),
    Several(Vec<Candidate>),
    /// Nothing matched; these contain the name, as hints.
    None(Vec<Candidate>),
}

/// Most hints shown when nothing matches.
const HINTS: usize = 5;

/// Matches `wanted` against the candidates' names; with `by_id`, an id
/// equal to `wanted` matches first.
pub fn find(candidates: &[Candidate], wanted: &str, by_id: bool) -> Matched {
    if by_id {
        let ids: Vec<&Candidate> = candidates.iter().filter(|c| c.id == wanted).collect();
        if !ids.is_empty() {
            return several_or_one(ids);
        }
    }
    let same_case: Vec<&Candidate> = candidates.iter().filter(|c| c.name == wanted).collect();
    if !same_case.is_empty() {
        return several_or_one(same_case);
    }
    let any_case: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.name.eq_ignore_ascii_case(wanted))
        .collect();
    if !any_case.is_empty() {
        return several_or_one(any_case);
    }
    let needle = wanted.to_lowercase();
    Matched::None(
        candidates
            .iter()
            .filter(|c| c.name.to_lowercase().contains(&needle))
            .take(HINTS)
            .cloned()
            .collect(),
    )
}

fn several_or_one(found: Vec<&Candidate>) -> Matched {
    match found.as_slice() {
        [one] => Matched::One((*one).clone()),
        _ => Matched::Several(found.into_iter().cloned().collect()),
    }
}

/// "no job named 'x' that you can see", with hints and the list command.
pub fn not_found(
    noun: &str,
    called: &str,
    wanted: &str,
    scope: &str,
    hints: &[Candidate],
    list: &str,
) -> String {
    let mut message = format!("no {noun} {called} '{wanted}'{scope} that you can see");
    if hints.is_empty() {
        message.push('.');
    } else {
        let names: Vec<String> = hints.iter().map(Candidate::describe).collect();
        message.push_str(&format!("; did you mean {}?", names.join(", ")));
    }
    message.push_str(&format!(" See `{list}`."));
    message
}

/// Asks which of `candidates` on the terminal (stderr and stdin).
pub fn ask(noun: &str, wanted: &str, candidates: &[Candidate]) -> Result<Candidate> {
    eprintln!("Several {noun}s are named '{wanted}':");
    for (n, candidate) in candidates.iter().enumerate() {
        eprintln!("  {}) {}", n + 1, candidate.describe());
    }
    let answer = crate::cmd::prompt_line(&format!("Which one? [1-{}] ", candidates.len()))?;
    match answer.parse::<usize>() {
        Ok(n) if (1..=candidates.len()).contains(&n) => Ok(candidates[n - 1].clone()),
        _ => bail!("cancelled; no {noun} chosen"),
    }
}

/// Whether gz may ask: a terminal on both ends, and neither `-q` nor
/// `--no-input`.
pub fn may_ask(rt: &Runtime) -> bool {
    use std::io::IsTerminal;
    !rt.asked_quiet
        && !rt.no_input
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
}

/// The candidate `wanted` names, asking when several match and gz may ask.
/// `several` words the error when it may not; [`listed`] is the plain one.
/// `scope` is the project the candidates were read from, for messages.
pub fn choose(
    rt: &Runtime,
    kind: Kind,
    wanted: &str,
    lookup: Lookup<'_>,
    several: impl FnOnce(&[Candidate]) -> String,
) -> Result<Candidate> {
    match find(lookup.candidates, wanted, lookup.by_id) {
        Matched::One(one) => Ok(one),
        Matched::Several(list) if may_ask(rt) => ask(kind.noun(), wanted, &list),
        Matched::Several(list) => bail!("{}", several(&list)),
        Matched::None(hints) => {
            let list = kind.list_command();
            let (scope, list) = match lookup.scope {
                Some(p) => (
                    format!(" in project {p}"),
                    list.replace("<project>", &p.to_string()),
                ),
                None => (String::new(), list.to_string()),
            };
            let called = match lookup.by_id {
                true => "named or numbered",
                false => "named",
            };
            bail!(
                "{}",
                not_found(kind.noun(), called, wanted, &scope, &hints, &list)
            )
        }
    }
}

/// What a name is matched against.
pub struct Lookup<'c> {
    pub candidates: &'c [Candidate],
    /// Whether an id equal to the name matches too.
    pub by_id: bool,
    /// The project the candidates come from.
    pub scope: Option<i64>,
}

/// "several jobs are named 'x': …; pass the id".
pub fn listed(kind: Kind, wanted: &str, several: &[Candidate]) -> String {
    let names: Vec<String> = several.iter().map(Candidate::describe).collect();
    format!(
        "several {}s are named '{wanted}': {}; pass the id",
        kind.noun(),
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(id: &str, name: &str) -> Candidate {
        Candidate {
            id: id.into(),
            name: name.into(),
            project: None,
            detail: None,
        }
    }

    fn ids(matched: &Matched) -> Vec<String> {
        match matched {
            Matched::One(one) => vec![one.id.clone()],
            Matched::Several(list) | Matched::None(list) => {
                list.iter().map(|c| c.id.clone()).collect()
            }
        }
    }

    #[test]
    fn exact_case_wins_then_any_case() {
        let list = vec![named("1", "Sales"), named("2", "sales"), named("3", "Ops")];
        assert_eq!(find(&list, "sales", false), Matched::One(list[1].clone()));
        assert_eq!(find(&list, "OPS", false), Matched::One(list[2].clone()));
        let several = find(&list, "SALES", false);
        assert!(matches!(several, Matched::Several(_)));
        assert_eq!(ids(&several), vec!["1", "2"]);
    }

    #[test]
    fn ids_match_only_when_asked() {
        let list = vec![named("3", "7"), named("7", "Ops")];
        assert_eq!(ids(&find(&list, "7", true)), vec!["7"]);
        assert_eq!(ids(&find(&list, "7", false)), vec!["3"]);
    }

    #[test]
    fn no_match_hints_at_names_that_contain_it() {
        let list = vec![named("1", "Daily DQC"), named("2", "Ops")];
        let matched = find(&list, "dqc", false);
        assert_eq!(matched, Matched::None(vec![list[0].clone()]));
        let message = not_found(
            "job",
            "named",
            "dqc",
            "",
            &[list[0].clone()],
            "gz projects jobs 1",
        );
        assert!(
            message.contains("did you mean Daily DQC (id 1)?"),
            "{message}"
        );
        assert!(message.ends_with("See `gz projects jobs 1`."), "{message}");
        let bare = not_found(
            "job",
            "named",
            "x",
            " in project 1",
            &[],
            "gz projects jobs 1",
        );
        assert_eq!(
            bare,
            "no job named 'x' in project 1 that you can see. See `gz projects jobs 1`."
        );
    }
}

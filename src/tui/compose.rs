//! Screens built from more than one request: a join (lineage connections
//! named from `/connection`), a union (the data catalog's datasets, jobs and
//! warehouses) or a pair (a folder's subfolders and files). The level's own
//! source and the `with` sources go out together; an optional expansion
//! then asks once per row of one answer (each project's jobs); `combine`
//! builds the rows from everything. One failed source doesn't hide the
//! rest: `combine` sees each failure and can report it.

use serde_json::Value;

use crate::tui::nav::Source;
use crate::tui::shape;

pub type Answer = Result<Value, String>;

/// A follow-up request per row of one answer.
#[derive(Debug, Clone, Copy)]
pub struct Expand {
    /// Which answer's rows to expand: 0 is the level's own source, then
    /// `with` in order.
    pub part: usize,
    /// Filled from each row (then the drill trail).
    pub source: Source,
    /// Rows expanded at most, so a huge list can't fan out without bound.
    pub max: usize,
}

/// Everything `combine` builds rows from.
pub struct Parts<'a> {
    /// The level's own answer, then each `with` answer.
    pub answers: &'a [Answer],
    /// Each expanded row with its answer.
    pub expanded: &'a [(Value, Answer)],
    /// The row drilled into to reach this level (Null at the root).
    pub parent: &'a Value,
}

impl Parts<'_> {
    /// The rows of answer `index`; empty when it failed.
    pub fn rows(&self, index: usize) -> Vec<Value> {
        match self.answers.get(index) {
            Some(Ok(answer)) => shape::rows_of(answer),
            _ => Vec::new(),
        }
    }
}

/// The rows to show, and a line per source that failed.
#[derive(Debug, Default)]
pub struct Combined {
    pub rows: Vec<Value>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Compose {
    pub with: &'static [Source],
    pub expand: Option<Expand>,
    pub combine: fn(&Parts) -> Combined,
}

/// What an answer meant for a run in progress.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Not one of this run's requests (a stale answer).
    NotMine,
    Waiting,
    /// Every first answer is in; send the expansion for these rows.
    Expand(Vec<Value>),
    /// Everything is in; call [`ComposeRun::combine`].
    Done,
}

/// One load of a composed level, collecting its answers.
#[derive(Debug, Clone)]
pub struct ComposeRun {
    spec: &'static Compose,
    ids: Vec<u64>,
    answers: Vec<Option<Answer>>,
    expansion: Vec<(u64, Value, Option<Answer>)>,
    expanding: bool,
}

impl ComposeRun {
    /// `ids` are the requests sent: the level's own first, then `with`.
    pub fn new(spec: &'static Compose, ids: Vec<u64>) -> Self {
        let answers = ids.iter().map(|_| None).collect();
        Self {
            spec,
            ids,
            answers,
            expansion: Vec::new(),
            expanding: false,
        }
    }

    pub fn accept(&mut self, id: u64, answer: Answer) -> Step {
        if let Some(index) = self.ids.iter().position(|i| *i == id) {
            self.answers[index] = Some(answer);
            if self.answers.iter().any(Option::is_none) {
                return Step::Waiting;
            }
            return match self.spec.expand {
                Some(expand) if !self.expanding => {
                    self.expanding = true;
                    let rows = match &self.answers[expand.part] {
                        Some(Ok(value)) => shape::rows_of(value),
                        _ => Vec::new(),
                    };
                    Step::Expand(rows.into_iter().take(expand.max).collect())
                }
                _ => Step::Done,
            };
        }
        if let Some(slot) = self.expansion.iter_mut().find(|(i, _, _)| *i == id) {
            slot.2 = Some(answer);
            return if self.expansion.iter().all(|(_, _, a)| a.is_some()) {
                Step::Done
            } else {
                Step::Waiting
            };
        }
        Step::NotMine
    }

    /// Records the expansion requests sent; Done when there were none.
    pub fn expanding(&mut self, sent: Vec<(u64, Value)>) -> Step {
        self.expansion = sent.into_iter().map(|(id, row)| (id, row, None)).collect();
        if self.expansion.is_empty() {
            Step::Done
        } else {
            Step::Waiting
        }
    }

    /// Records an expansion row whose request couldn't be built.
    pub fn expansion_failed(&mut self, row: Value, reason: String) {
        self.expansion.push((0, row, Some(Err(reason))));
    }

    pub fn combine(&self, parent: &Value) -> Combined {
        let answers: Vec<Answer> = self
            .answers
            .iter()
            .map(|a| a.clone().unwrap_or_else(|| Err("no answer".into())))
            .collect();
        let expanded: Vec<(Value, Answer)> = self
            .expansion
            .iter()
            .map(|(_, row, a)| {
                let answer = a.clone().unwrap_or_else(|| Err("no answer".into()));
                (row.clone(), answer)
            })
            .collect();
        (self.spec.combine)(&Parts {
            answers: &answers,
            expanded: &expanded,
            parent,
        })
    }

    /// The level's own answer, for an error when nothing could be built.
    pub fn own_error(&self) -> Option<String> {
        match self.answers.first() {
            Some(Some(Err(err))) => Some(err.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn count_everything(parts: &Parts) -> Combined {
        let mut rows = parts.rows(0);
        rows.extend(parts.rows(1));
        for (project, answer) in parts.expanded {
            if let Ok(jobs) = answer {
                for job in shape::rows_of(jobs) {
                    rows.push(json!({"job": job, "project": project["Id"]}));
                }
            }
        }
        let failures = parts
            .answers
            .iter()
            .filter_map(|a| a.as_ref().err().cloned())
            .collect();
        Combined { rows, failures }
    }

    static PAIR: Compose = Compose {
        with: &[Source::get("x", "/b")],
        expand: None,
        combine: count_everything,
    };

    static FAN_OUT: Compose = Compose {
        with: &[Source::get("x", "/projects")],
        expand: Some(Expand {
            part: 1,
            source: Source::get("x", "/jobs/{Id}"),
            max: 2,
        }),
        combine: count_everything,
    };

    #[test]
    fn waits_for_every_part_and_ignores_stale_answers() {
        let mut run = ComposeRun::new(&PAIR, vec![1, 2]);
        assert_eq!(run.accept(9, Ok(json!([]))), Step::NotMine);
        assert_eq!(run.accept(2, Ok(json!([{"b": 1}]))), Step::Waiting);
        assert_eq!(run.accept(1, Err("HTTP 500".into())), Step::Done);
        let combined = run.combine(&Value::Null);
        assert_eq!(combined.rows, vec![json!({"b": 1})]);
        assert_eq!(combined.failures, vec!["HTTP 500".to_string()]);
        assert_eq!(run.own_error().as_deref(), Some("HTTP 500"));
    }

    #[test]
    fn expands_rows_of_one_part_up_to_its_limit() {
        let mut run = ComposeRun::new(&FAN_OUT, vec![1, 2]);
        run.accept(1, Ok(json!([])));
        let projects = json!([{"Id": 1}, {"Id": 2}, {"Id": 3}]);
        let Step::Expand(rows) = run.accept(2, Ok(projects)) else {
            panic!("the projects should expand");
        };
        assert_eq!(rows.len(), 2, "capped at max");
        assert_eq!(
            run.expanding(vec![(3, rows[0].clone()), (4, rows[1].clone())]),
            Step::Waiting
        );
        assert_eq!(run.accept(3, Ok(json!([{"Id": 10}]))), Step::Waiting);
        assert_eq!(run.accept(4, Err("HTTP 403".into())), Step::Done);
        let combined = run.combine(&Value::Null);
        assert_eq!(combined.rows.len(), 4, "3 projects and 1 job");
        assert_eq!(combined.rows[3], json!({"job": {"Id": 10}, "project": 1}));
    }

    #[test]
    fn an_empty_expansion_is_done_at_once() {
        let mut run = ComposeRun::new(&FAN_OUT, vec![1, 2]);
        run.accept(1, Ok(json!([])));
        assert_eq!(run.accept(2, Ok(json!([]))), Step::Expand(vec![]));
        assert_eq!(run.expanding(vec![]), Step::Done);
    }
}

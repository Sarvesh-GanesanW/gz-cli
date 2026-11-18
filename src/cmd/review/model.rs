//! What a review found for each job, and the verdict it leads to. Pure: the
//! steps that call the platform fill these in, the renderers read them.

use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::impact::{Kind, Report};

/// The verdict, worst last so `max` picks the worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Warn => "warn",
            Verdict::Fail => "fail",
        }
    }
}

/// How much one finding weighs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Warn,
    Fail,
    /// gz could not finish a step: the verdict can't be trusted fully.
    Error,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

impl Finding {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Self {
            level,
            text: text.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"level": self.level.word(), "text": self.text})
    }
}

/// One step of a job's review.
#[derive(Debug)]
pub enum Step<T> {
    Done(T),
    /// Not run, by choice or because nothing calls for it; no weight.
    NotAsked(String),
    /// Could not run for a reason the change itself has; warns.
    Skipped(String),
    /// gz could not finish it.
    Failed(String),
}

impl<T> Step<T> {
    pub fn done(&self) -> Option<&T> {
        match self {
            Step::Done(value) => Some(value),
            _ => None,
        }
    }

    /// `{"status": …, "reason": …}`, with `data` merged in when done.
    pub fn to_json(&self, data: impl FnOnce(&T) -> Value) -> Value {
        let (status, reason) = match self {
            Step::Done(value) => {
                let mut out = data(value);
                if !out.is_object() {
                    out = json!({"result": out});
                }
                out["status"] = json!("done");
                return out;
            }
            Step::NotAsked(reason) => ("notRun", reason),
            Step::Skipped(reason) => ("skipped", reason),
            Step::Failed(reason) => ("error", reason),
        };
        json!({"status": status, "reason": reason})
    }
}

/// The job's local code against the saved code.
#[derive(Debug)]
pub enum Code {
    Changed {
        diff: String,
    },
    /// The project has no job of that name.
    New,
    /// The file is gone; `gz sync` never deletes a job.
    Removed,
    Unchanged,
}

/// Lineage's answer for the tables the job writes.
#[derive(Debug)]
pub struct Impact {
    pub writes: Vec<String>,
    pub report: Report,
}

/// What `gz jobs try` reported.
#[derive(Debug)]
pub struct Tried {
    pub branch: String,
    pub run_id: String,
    pub status: String,
    /// Each table's entry from try's report.
    pub tables: Vec<Value>,
    pub quality_blocks: Vec<String>,
    /// What try said after the run, when it ended with an error.
    pub error: Option<String>,
    /// Try's whole report, so nothing it adds is lost.
    pub report: Value,
}

impl Tried {
    pub fn succeeded(&self) -> bool {
        self.status == crate::cmd::run_wait::SUCCESS
    }

    /// The tables the run wrote to: `(warehouse, database.table)`.
    pub fn changed_tables(&self) -> Vec<(String, String)> {
        self.tables
            .iter()
            .filter(|t| t["changed"] == json!(true))
            .filter_map(|t| {
                Some((
                    t["warehouse"].as_str()?.to_string(),
                    t["table"].as_str()?.to_string(),
                ))
            })
            .collect()
    }

    /// Tables try found the run left alone, as `warehouse.database.table`.
    pub fn unchanged_tables(&self) -> Vec<String> {
        self.tables
            .iter()
            .filter(|t| t["changed"] == json!(false))
            .filter_map(|t| t["table"].as_str().map(str::to_string))
            .collect()
    }
}

/// One table's rows and schema, branch against main.
#[derive(Debug)]
pub struct TableRows {
    pub text: String,
    pub json: Value,
    pub added_rows: u64,
    pub removed_rows: u64,
    pub added_columns: Vec<String>,
    pub removed_columns: Vec<String>,
    /// `name: from → to`.
    pub retyped_columns: Vec<String>,
}

/// Everything review found for one job.
#[derive(Debug)]
pub struct JobReview {
    pub file: String,
    pub job: String,
    pub job_id: Option<i64>,
    pub code: Code,
    pub impact: Step<Impact>,
    pub tried: Step<Tried>,
    pub rows: Vec<(String, Step<TableRows>)>,
    pub rows_note: Option<String>,
    pub cleanup: Step<String>,
}

impl JobReview {
    /// The findings, in step order.
    pub fn findings(&self, fail_on: &[Kind]) -> Vec<Finding> {
        let mut found = Vec::new();
        found.extend(code_findings(&self.code, &self.job));
        found.extend(impact_findings(&self.impact, fail_on));
        found.extend(try_findings(&self.tried));
        for (table, rows) in &self.rows {
            found.extend(rows_findings(table, rows));
        }
        if let Step::Failed(err) = &self.cleanup {
            found.push(Finding::new(Level::Error, format!("cleanup: {err}")));
        }
        found
    }
}

pub fn verdict_of(findings: &[Finding]) -> Verdict {
    findings
        .iter()
        .map(|f| match f.level {
            Level::Fail => Verdict::Fail,
            Level::Warn | Level::Error => Verdict::Warn,
        })
        .max()
        .unwrap_or(Verdict::Pass)
}

/// The exit code for the whole review: a fail beats a gz error, which beats
/// a strict warn.
pub fn exit_code(verdict: Verdict, had_error: bool, strict: bool) -> i32 {
    match verdict {
        Verdict::Fail => 2,
        _ if had_error => 1,
        Verdict::Warn if strict => 3,
        _ => 0,
    }
}

fn step_finding<T>(label: &str, step: &Step<T>) -> Option<Finding> {
    match step {
        Step::Skipped(reason) => Some(Finding::new(Level::Warn, format!("{label}: {reason}"))),
        Step::Failed(reason) => Some(Finding::new(Level::Error, format!("{label}: {reason}"))),
        Step::Done(_) | Step::NotAsked(_) => None,
    }
}

fn code_findings(code: &Code, job: &str) -> Vec<Finding> {
    match code {
        Code::New => vec![Finding::new(
            Level::Warn,
            format!("job {job} is not in the project yet: `gz sync` creates it, then review can try it"),
        )],
        Code::Removed => vec![Finding::new(
            Level::Warn,
            format!("the file was removed, but gz sync never deletes jobs: job {job} stays on the platform"),
        )],
        Code::Changed { .. } | Code::Unchanged => Vec::new(),
    }
}

fn impact_findings(impact: &Step<Impact>, fail_on: &[Kind]) -> Vec<Finding> {
    let Step::Done(impact) = impact else {
        return step_finding("impact", impact).into_iter().collect();
    };
    let protected = impact.report.protected(fail_on);
    if protected.is_empty() {
        return Vec::new();
    }
    vec![Finding::new(
        Level::Fail,
        format!(
            "affects protected dependents: {}",
            protected_text(&impact.report, &protected)
        ),
    )]
}

fn protected_text(report: &Report, protected: &BTreeMap<Kind, usize>) -> String {
    let parts: Vec<String> = protected
        .iter()
        .map(|(kind, n)| {
            let names: Vec<&str> = report
                .dependents
                .iter()
                .filter(|d| d.kind == *kind)
                .map(|d| d.name.as_str())
                .collect();
            format!(
                "{} ({})",
                crate::impact::count_text(*kind, *n),
                names.join(", ")
            )
        })
        .collect();
    parts.join("; ")
}

fn try_findings(tried: &Step<Tried>) -> Vec<Finding> {
    let Step::Done(tried) = tried else {
        return step_finding("try", tried).into_iter().collect();
    };
    let mut found = Vec::new();
    if !tried.succeeded() {
        let status = if tried.status.is_empty() {
            "unknown"
        } else {
            &tried.status
        };
        found.push(Finding::new(
            Level::Fail,
            format!(
                "try run {} ended with status {status} (`gz jobs logs --job {}`)",
                tried.run_id, tried.run_id
            ),
        ));
    }
    for block in &tried.quality_blocks {
        found.push(Finding::new(Level::Fail, format!("quality gate: {block}")));
    }
    // A run that passed but try still failed: a check after the run refused it.
    if let (true, Some(error)) = (tried.succeeded(), &tried.error) {
        found.push(Finding::new(Level::Fail, format!("try: {error}")));
    }
    found
}

fn rows_findings(table: &str, rows: &Step<TableRows>) -> Vec<Finding> {
    let Step::Done(rows) = rows else {
        return step_finding(&format!("rows of {table}"), rows)
            .into_iter()
            .collect();
    };
    let mut found = Vec::new();
    if !rows.removed_columns.is_empty() {
        found.push(Finding::new(
            Level::Warn,
            format!(
                "{table} drops column(s) {}",
                rows.removed_columns.join(", ")
            ),
        ));
    }
    if !rows.retyped_columns.is_empty() {
        found.push(Finding::new(
            Level::Warn,
            format!(
                "{table} changes the type of {}",
                rows.retyped_columns.join(", ")
            ),
        ));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tried(status: &str, blocks: &[&str], error: Option<&str>) -> Tried {
        Tried {
            branch: "review-1-x".into(),
            run_id: "run-1".into(),
            status: status.into(),
            tables: Vec::new(),
            quality_blocks: blocks.iter().map(|b| b.to_string()).collect(),
            error: error.map(str::to_string),
            report: Value::Null,
        }
    }

    fn review(code: Code, impact: Step<Impact>, tried: Step<Tried>) -> JobReview {
        JobReview {
            file: "a.py".into(),
            job: "a".into(),
            job_id: Some(1),
            code,
            impact,
            tried,
            rows: Vec::new(),
            rows_note: None,
            cleanup: Step::NotAsked("kept".into()),
        }
    }

    fn changed() -> Code {
        Code::Changed {
            diff: "+x\n".into(),
        }
    }

    fn no_impact() -> Step<Impact> {
        Step::Done(Impact {
            writes: vec!["db.t".into()],
            report: Report::default(),
        })
    }

    fn levels(review: &JobReview) -> Vec<Level> {
        review.findings(&[]).iter().map(|f| f.level).collect()
    }

    #[test]
    fn a_clean_try_passes() {
        let r = review(
            changed(),
            no_impact(),
            Step::Done(tried("Success", &[], None)),
        );
        assert_eq!(verdict_of(&r.findings(&[])), Verdict::Pass);
    }

    #[test]
    fn a_failed_run_or_a_quality_refusal_fails() {
        let failed = review(
            changed(),
            no_impact(),
            Step::Done(tried("Failed", &[], Some("run ended"))),
        );
        assert_eq!(
            levels(&failed),
            vec![Level::Fail],
            "the run's own error is not counted twice"
        );
        let gated = review(
            changed(),
            no_impact(),
            Step::Done(tried("Success", &["db.t: no passing verdict"], None)),
        );
        assert_eq!(verdict_of(&gated.findings(&[])), Verdict::Fail);
        let checked = review(
            changed(),
            no_impact(),
            Step::Done(tried("Success", &[], Some("quality.toml check failed"))),
        );
        assert_eq!(levels(&checked), vec![Level::Fail]);
    }

    #[test]
    fn a_job_that_cannot_be_tried_warns_with_the_reason() {
        let r = review(
            changed(),
            no_impact(),
            Step::Skipped("the job writes no lakehouse table".into()),
        );
        let findings = r.findings(&[]);
        assert_eq!(verdict_of(&findings), Verdict::Warn);
        assert!(findings[0].text.contains("no lakehouse table"));
    }

    #[test]
    fn a_step_gz_could_not_finish_is_an_error_that_warns() {
        let r = review(
            changed(),
            Step::Failed("HTTP 500".into()),
            Step::NotAsked("x".into()),
        );
        assert_eq!(levels(&r), vec![Level::Error]);
        assert_eq!(verdict_of(&r.findings(&[])), Verdict::Warn);
    }

    #[test]
    fn dropped_and_retyped_columns_warn() {
        let mut r = review(
            changed(),
            no_impact(),
            Step::Done(tried("Success", &[], None)),
        );
        r.rows.push((
            "wh.db.t".into(),
            Step::Done(TableRows {
                text: String::new(),
                json: Value::Null,
                added_rows: 1,
                removed_rows: 0,
                added_columns: vec!["c".into()],
                removed_columns: vec!["a".into()],
                retyped_columns: vec!["b: int → string".into()],
            }),
        ));
        assert_eq!(levels(&r), vec![Level::Warn, Level::Warn]);
    }

    #[test]
    fn new_and_removed_files_warn_and_unchanged_passes() {
        let of = |code| review(code, Step::NotAsked("-".into()), Step::NotAsked("-".into()));
        assert_eq!(levels(&of(Code::New)), vec![Level::Warn]);
        assert_eq!(levels(&of(Code::Removed)), vec![Level::Warn]);
        assert!(levels(&of(Code::Unchanged)).is_empty());
    }

    #[test]
    fn dependents_of_a_protected_kind_fail() {
        let dashboard = crate::impact::Dependent {
            kind: Kind::Dashboard,
            id: "7".into(),
            name: "Sales".into(),
            owner: None,
            depth: 1,
            root: "db.t".into(),
            via: crate::impact::Via::Direct,
            columns: crate::impact::ColumnUse::NotAsked,
            asset_type: None,
            written_by: Vec::new(),
            visualization_ids: Vec::new(),
        };
        let impact = || {
            Step::Done(Impact {
                writes: vec!["db.t".into()],
                report: Report {
                    dependents: vec![dashboard.clone()],
                    ..Report::default()
                },
            })
        };
        let r = review(changed(), impact(), Step::NotAsked("-".into()));
        assert!(
            r.findings(&[Kind::Agent]).is_empty(),
            "other kinds don't fail"
        );
        let findings = r.findings(&[Kind::Dashboard]);
        assert_eq!(findings[0].level, Level::Fail);
        assert!(findings[0].text.contains("(Sales)"), "{}", findings[0].text);
    }

    #[test]
    fn exit_codes_follow_the_verdict() {
        assert_eq!(exit_code(Verdict::Pass, false, true), 0);
        assert_eq!(exit_code(Verdict::Warn, false, false), 0);
        assert_eq!(exit_code(Verdict::Warn, false, true), 3);
        assert_eq!(exit_code(Verdict::Warn, true, true), 1);
        assert_eq!(exit_code(Verdict::Fail, true, false), 2);
    }
}

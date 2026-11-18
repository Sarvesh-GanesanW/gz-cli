//! The review as text for a terminal, markdown for a pull request comment,
//! and JSON for scripts.

use owo_colors::OwoColorize;
use serde_json::{json, Value};

use super::model::{verdict_of, Code, Finding, JobReview, Level, Step, Verdict};
use super::Review;
use crate::impact::summary_line;

/// Diff lines shown before the rest is cut (the JSON has all of it).
const DIFF_LINES: usize = 60;
/// GitHub refuses comments over 65,536 characters; leave room for the rest.
const MARKDOWN_DIFF_CHARS: usize = 20_000;

/// The overall verdict and whether any step hit a gz error.
pub fn totals(review: &Review) -> (Verdict, bool) {
    let findings: Vec<Finding> = review
        .jobs
        .iter()
        .flat_map(|job| job.findings(&review.fail_on))
        .collect();
    let had_error = findings.iter().any(|f| f.level == Level::Error);
    (verdict_of(&findings), had_error)
}

fn diff_stats(diff: &str) -> (usize, usize) {
    let counted = |sign: char, header: &str| {
        diff.lines()
            .filter(|l| l.starts_with(sign) && !l.starts_with(header))
            .count()
    };
    (counted('+', "+++"), counted('-', "---"))
}

fn code_line(code: &Code) -> String {
    match code {
        Code::Changed { diff } => {
            let (added, removed) = diff_stats(diff);
            format!("+{added} -{removed} lines against the saved code")
        }
        Code::New => "a new job: the project has no job of this name".into(),
        Code::Removed => "the file was removed".into(),
        Code::Unchanged => "the same as the saved code".into(),
    }
}

fn impact_line(job: &JobReview) -> String {
    match &job.impact {
        Step::Done(impact) => {
            let writes = if impact.writes.is_empty() {
                "writes no table lineage knows of".to_string()
            } else {
                format!("writes {}", impact.writes.join(", "))
            };
            format!("{writes}; {}", summary_line(&impact.report.counts()))
        }
        other => step_reason(other),
    }
}

fn step_reason<T>(step: &Step<T>) -> String {
    match step {
        Step::Done(_) => "done".into(),
        Step::NotAsked(reason) | Step::Skipped(reason) => reason.clone(),
        // The error itself is in the reasons, next to the verdict.
        Step::Failed(_) => "gz could not finish this step".into(),
    }
}

fn try_line(job: &JobReview) -> String {
    match &job.tried {
        Step::Done(tried) => {
            let mut line = format!(
                "run {} on branch {}: {}",
                tried.run_id,
                tried.branch,
                if tried.status.is_empty() {
                    "unknown"
                } else {
                    &tried.status
                }
            );
            let unchanged = tried.unchanged_tables();
            if !unchanged.is_empty() {
                line.push_str(&format!("; left alone: {}", unchanged.join(", ")));
            }
            line
        }
        other => step_reason(other),
    }
}

fn quality_line(job: &JobReview) -> Option<String> {
    let tried = job.tried.done()?;
    if tried.quality_blocks.is_empty() {
        return Some("no quality gate refused the branch".into());
    }
    Some(format!("refused: {}", tried.quality_blocks.join("; ")))
}

fn rows_summary(rows: &super::model::TableRows) -> String {
    let mut parts = vec![format!("+{} -{} rows", rows.added_rows, rows.removed_rows)];
    if !rows.added_columns.is_empty() {
        parts.push(format!("new columns {}", rows.added_columns.join(", ")));
    }
    if !rows.removed_columns.is_empty() {
        parts.push(format!(
            "dropped columns {}",
            rows.removed_columns.join(", ")
        ));
    }
    if !rows.retyped_columns.is_empty() {
        parts.push(format!("retyped {}", rows.retyped_columns.join(", ")));
    }
    if parts.len() == 1 {
        parts.push("same schema".into());
    }
    parts.join(", ")
}

fn verdict_word_colored(verdict: Verdict) -> String {
    let word = verdict.word().to_uppercase();
    match verdict {
        Verdict::Pass => word.green().bold().to_string(),
        Verdict::Warn => word.yellow().bold().to_string(),
        Verdict::Fail => word.red().bold().to_string(),
    }
}

fn job_title(job: &JobReview) -> String {
    match job.job_id {
        Some(id) => format!("{} → job {} ({id})", job.file, job.job),
        None => format!("{} → job {}", job.file, job.job),
    }
}

/// "for project 1999: 2 jobs", after the verdict word.
fn headline_tail(review: &Review) -> String {
    let count = review.jobs.len();
    let (_, had_error) = totals(review);
    format!(
        "for project {}: {count} job{}{}",
        review.project,
        if count == 1 { "" } else { "s" },
        if had_error {
            " (gz could not finish every step)"
        } else {
            ""
        }
    )
}

// ---------- text ----------

pub fn text(review: &Review) -> String {
    let mut out = Vec::new();
    let (verdict, _) = totals(review);
    if review.jobs.is_empty() {
        out.push("No changed job to review: every job file matches its saved code.".to_string());
    }
    for job in &review.jobs {
        out.extend(job_text(job, &review.fail_on));
        out.push(String::new());
    }
    if !review.unmapped.is_empty() {
        out.push(output_dim(&format!(
            "Not job files (no job in gz-sync.toml): {}",
            review.unmapped.join(", ")
        )));
    }
    out.push(format!(
        "{} {} {}",
        "Review:".bold(),
        verdict_word_colored(verdict),
        headline_tail(review)
    ));
    out.join("\n") + "\n"
}

fn output_dim(text: &str) -> String {
    crate::output::dim(text)
}

fn job_text(job: &JobReview, fail_on: &[crate::impact::Kind]) -> Vec<String> {
    let findings = job.findings(fail_on);
    let verdict = verdict_of(&findings);
    let mut lines = vec![format!(
        "{}  {}",
        job_title(job).bold(),
        verdict_word_colored(verdict)
    )];
    let row = |label: &str, value: String| format!("  {:<9} {value}", label.dimmed());
    lines.push(row("Code", code_line(&job.code)));
    if let Code::Changed { diff } = &job.code {
        lines.extend(diff_text(diff));
    }
    lines.push(row("Impact", impact_line(job)));
    lines.push(row("Try", try_line(job)));
    for (table, rows) in &job.rows {
        match rows {
            Step::Done(rows) => {
                lines.push(row("Rows", format!("{table}: {}", rows_summary(rows))));
                lines.extend(rows.text.lines().map(|l| format!("            {l}")));
            }
            other => lines.push(row("Rows", format!("{table}: {}", step_reason(other)))),
        }
    }
    if let Some(note) = &job.rows_note {
        lines.push(row("Rows", note.clone()));
    }
    if let Some(quality) = quality_line(job) {
        lines.push(row("Quality", quality));
    }
    if !matches!(job.cleanup, Step::NotAsked(_)) || job.tried.done().is_some() {
        lines.push(row("Cleanup", step_reason_or_done(&job.cleanup)));
    }
    for finding in &findings {
        let mark = match finding.level {
            Level::Fail => "fail".red().to_string(),
            Level::Warn => "warn".yellow().to_string(),
            Level::Error => "error".red().to_string(),
        };
        lines.push(format!("  {mark}  {}", finding.text));
    }
    lines
}

fn step_reason_or_done(step: &Step<String>) -> String {
    match step {
        Step::Done(text) => text.clone(),
        other => step_reason(other),
    }
}

fn diff_text(diff: &str) -> Vec<String> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut shown: Vec<String> = lines
        .iter()
        .take(DIFF_LINES)
        .map(|line| {
            let colored = if line.starts_with('+') && !line.starts_with("+++") {
                line.green().to_string()
            } else if line.starts_with('-') && !line.starts_with("---") {
                line.red().to_string()
            } else {
                line.dimmed().to_string()
            };
            format!("    {colored}")
        })
        .collect();
    if lines.len() > DIFF_LINES {
        shown.push(format!(
            "    {}",
            format!(
                "… {} more lines (-o json has them all)",
                lines.len() - DIFF_LINES
            )
            .dimmed()
        ));
    }
    shown
}

// ---------- markdown ----------

/// Marks the comment so the GitHub Action updates it instead of adding one.
pub const MARKDOWN_MARKER: &str = "<!-- gz-review -->";

pub fn markdown(review: &Review) -> String {
    let (verdict, _) = totals(review);
    let mut out = vec![
        MARKDOWN_MARKER.to_string(),
        format!(
            "## gz review: **{}** {}",
            verdict.word().to_uppercase(),
            headline_tail(review)
        ),
        String::new(),
    ];
    if review.jobs.is_empty() {
        out.push("No changed job to review: every job file matches its saved code.".to_string());
        out.push(String::new());
    } else {
        out.push("| Job | File | Verdict | Why |".to_string());
        out.push("|---|---|---|---|".to_string());
        for job in &review.jobs {
            let findings = job.findings(&review.fail_on);
            let why: Vec<String> = findings.iter().map(|f| md_cell(&f.text)).collect();
            out.push(format!(
                "| {} | `{}` | {} | {} |",
                md_cell(&job.job),
                md_cell(&job.file),
                verdict_of(&findings).word(),
                if why.is_empty() {
                    "-".to_string()
                } else {
                    why.join("<br>")
                }
            ));
        }
        out.push(String::new());
    }
    for job in &review.jobs {
        out.extend(job_markdown(job));
    }
    if !review.unmapped.is_empty() {
        out.push(format!(
            "Not job files (no job in gz-sync.toml): {}",
            review
                .unmapped
                .iter()
                .map(|f| format!("`{}`", md_cell(f)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out.join("\n") + "\n"
}

/// Text safe inside a table cell or inline code.
fn md_cell(text: &str) -> String {
    text.replace('|', "\\|")
        .replace('\n', " ")
        .replace('`', "'")
}

fn job_markdown(job: &JobReview) -> Vec<String> {
    let mut lines = vec![format!("### {}", md_cell(&job_title(job))), String::new()];
    lines.push(format!("- **Code:** {}", md_cell(&code_line(&job.code))));
    lines.push(format!("- **Impact:** {}", md_cell(&impact_line(job))));
    if let Step::Done(impact) = &job.impact {
        for dependent in impact.report.dependents.iter().take(20) {
            lines.push(format!(
                "  - {} {} ({})",
                dependent.kind.singular(),
                md_cell(&dependent.name),
                md_cell(&dependent.via.describe())
            ));
        }
        if impact.report.dependents.len() > 20 {
            lines.push(format!(
                "  - … {} more",
                impact.report.dependents.len() - 20
            ));
        }
    }
    lines.push(format!("- **Try:** {}", md_cell(&try_line(job))));
    for (table, rows) in &job.rows {
        let text = match rows {
            Step::Done(rows) => rows_summary(rows),
            other => step_reason(other),
        };
        lines.push(format!(
            "- **Rows** `{}`: {}",
            md_cell(table),
            md_cell(&text)
        ));
    }
    if let Some(note) = &job.rows_note {
        lines.push(format!("- **Rows:** {}", md_cell(note)));
    }
    if let Some(quality) = quality_line(job) {
        lines.push(format!("- **Quality:** {}", md_cell(&quality)));
    }
    if let Code::Changed { diff } = &job.code {
        let shown: String = diff.chars().take(MARKDOWN_DIFF_CHARS).collect();
        let cut = if shown.len() < diff.len() {
            "\n… (cut; `gz review -o json` has the whole diff)"
        } else {
            ""
        };
        lines.push(String::new());
        lines.push("<details><summary>Code diff</summary>".to_string());
        lines.push(String::new());
        lines.push("```diff".to_string());
        lines.push(format!("{}{cut}", shown.replace("```", "'''").trim_end()));
        lines.push("```".to_string());
        lines.push(String::new());
        lines.push("</details>".to_string());
    }
    lines.push(String::new());
    lines
}

// ---------- json ----------

pub fn json(review: &Review) -> Value {
    let (verdict, had_error) = totals(review);
    json!({
        "verdict": verdict.word(),
        "errors": had_error,
        "project": review.project,
        "failOn": review.fail_on.iter().map(|k| k.plural()).collect::<Vec<_>>(),
        "jobs": review.jobs.iter().map(|job| job_json(job, &review.fail_on)).collect::<Vec<_>>(),
        "unmapped": review.unmapped,
    })
}

fn job_json(job: &JobReview, fail_on: &[crate::impact::Kind]) -> Value {
    let findings = job.findings(fail_on);
    let code = match &job.code {
        Code::Changed { diff } => json!({"state": "changed", "diff": diff}),
        Code::New => json!({"state": "new"}),
        Code::Removed => json!({"state": "removed"}),
        Code::Unchanged => json!({"state": "unchanged"}),
    };
    json!({
        "file": job.file,
        "job": job.job,
        "jobId": job.job_id,
        "verdict": verdict_of(&findings).word(),
        "reasons": findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
        "code": code,
        "impact": job.impact.to_json(|impact| {
            let mut value = impact.report.to_json();
            value["writes"] = json!(impact.writes);
            value
        }),
        "try": job.tried.to_json(|tried| json!({
            "branch": tried.branch,
            "run": {"jobIdentifier": tried.run_id, "status": tried.status},
            "qualityBlocks": tried.quality_blocks,
            "error": tried.error,
            "report": tried.report,
        })),
        "rows": job.rows.iter().map(|(table, rows)| {
            let mut value = rows.to_json(|rows| rows.json.clone());
            value["table"] = json!(table);
            value
        }).collect::<Vec<_>>(),
        "rowsNote": job.rows_note,
        "cleanup": job.cleanup.to_json(|text| json!({"result": text})),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::review::model::{Impact, Tried};
    use crate::impact::Report;

    fn sample() -> Review {
        Review {
            project: 1999,
            jobs: vec![JobReview {
                file: "etl/a.py".into(),
                job: "load-a".into(),
                job_id: Some(3013),
                code: Code::Changed {
                    diff: "--- a/etl/a.py\n+++ b/etl/a.py\n@@ -1 +1 @@\n-x = 1\n+x = 2\n".into(),
                },
                impact: Step::Done(Impact {
                    writes: vec!["db.t".into()],
                    report: Report::default(),
                }),
                tried: Step::Done(Tried {
                    branch: "review-3013-x".into(),
                    run_id: "run-1".into(),
                    status: "Failed".into(),
                    tables: vec![],
                    quality_blocks: vec![],
                    error: Some("run run-1 ended with status Failed".into()),
                    report: json!({}),
                }),
                rows: vec![],
                rows_note: Some("rows not compared: the run failed".into()),
                cleanup: Step::Done("branch review-3013-x dropped from wh.db.t".into()),
            }],
            unmapped: vec!["README.md".into()],
            fail_on: vec![],
        }
    }

    #[test]
    fn counts_diff_lines_without_headers() {
        assert_eq!(diff_stats("--- a\n+++ b\n-x\n+y\n+z\n"), (2, 1));
    }

    #[test]
    fn markdown_has_the_marker_the_verdict_table_and_the_diff() {
        let md = markdown(&sample());
        assert!(md.starts_with(MARKDOWN_MARKER), "{md}");
        assert!(
            md.contains("## gz review: **FAIL** for project 1999: 1 job"),
            "{md}"
        );
        assert!(
            md.contains("| load-a | `etl/a.py` | fail | try run run-1 ended with status Failed"),
            "{md}"
        );
        assert!(md.contains("```diff\n--- a/etl/a.py"), "{md}");
        assert!(
            md.contains("- **Rows:** rows not compared: the run failed"),
            "{md}"
        );
        assert!(
            md.contains("Not job files (no job in gz-sync.toml): `README.md`"),
            "{md}"
        );
    }

    #[test]
    fn json_has_the_verdict_reasons_and_each_step() {
        let value = json(&sample());
        assert_eq!(value["verdict"], json!("fail"));
        let job = &value["jobs"][0];
        assert_eq!(job["reasons"][0]["level"], json!("fail"));
        assert_eq!(job["impact"]["status"], json!("done"));
        assert_eq!(job["impact"]["writes"], json!(["db.t"]));
        assert_eq!(job["try"]["run"]["status"], json!("Failed"));
        assert_eq!(job["cleanup"]["status"], json!("done"));
        assert_eq!(job["code"]["state"], json!("changed"));
    }

    #[test]
    fn a_step_gz_could_not_finish_shows_in_the_headline_and_the_reasons() {
        let mut review = sample();
        review.jobs[0].impact = Step::Failed("HTTP 500 from lineage".into());
        let md = markdown(&review);
        assert!(md.contains("1 job (gz could not finish every step)"), "{md}");
        assert!(md.contains("impact: HTTP 500 from lineage"), "{md}");
        assert!(md.contains("- **Impact:** gz could not finish this step"), "{md}");
    }

    #[test]
    fn text_names_each_step() {
        let text = crate::cmd::review::render::text(&sample());
        for label in ["Code", "Impact", "Try", "Rows", "Cleanup", "Review:"] {
            assert!(text.contains(label), "{label} missing from {text}");
        }
    }
}

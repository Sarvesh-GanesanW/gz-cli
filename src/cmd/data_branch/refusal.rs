//! Turning the ETL provider's refusal of a branch run into what to change.
//!
//! The provider (groundzero-etl-provider-api utils/branch_runs.py
//! `refuse_unbranched_writes`) refuses a run whose writes it can't keep on
//! the branch, with one text:
//! `Not running on branch B: the run would change data outside it — P1; P2.
//! Make the branch on every table the job writes (gz jobs try does), or run
//! the job normally.`
//! Each problem is one of `unbranched_writes`' kinds. Writes it can't follow
//! carry the analyzer's message (utils/astTreeGen.py `_add_unresolved`), not
//! its code, so the advice is keyed by that message.

const REFUSAL_START: &str = "Not running on branch ";
const LIST_START: &str = ": the run would change data outside it — ";
const LIST_END: &str = ". Make the branch on every table the job writes";
const HIDDEN_WRITE: &str = "a write gz cannot follow: ";

const READ_FORM: &str = r#"spark.executeQuery("<Conn>", "SELECT … FROM db.t")"#;
const WRITE_FORM: &str = r#"spark.writeGzLakehouse(df, "<Conn>", "t", database="db")"#;

/// What to change for each analyzer message, by the message's start (some
/// end in a number).
const HIDDEN_WRITE_ADVICE: &[(&str, &str)] = &[
    (
        "The session SQL text is not statically deterministic.",
        "the SQL given to spark.sql() is built at run time. Read with spark.executeQuery and write with spark.writeGzLakehouse, or make the SQL a literal string",
    ),
    (
        "The SQL text is not statically deterministic.",
        "the query given to the connector is built at run time. Make it a literal string",
    ),
    (
        "The connector connection name is not statically deterministic.",
        "the connection name is worked out at run time. Make it a literal string, e.g. \"<Conn>\"",
    ),
    (
        "The sink dataframe provenance could not be resolved statically.",
        "gz can't tell which tables the dataframe being written comes from. Build it from reads gz can follow, such as spark.executeQuery, in the same file",
    ),
    (
        "The connector asset name is not statically deterministic.",
        "the table name of a write is worked out at run time. Make it a literal string",
    ),
    (
        "The connector asset namespace is not statically deterministic.",
        "the database of a write is worked out at run time. Make it a literal string (database=\"db\")",
    ),
    (
        "Unqualified SQL assets were omitted because the namespace is dynamic.",
        "the SQL names tables without their database, and the database is worked out at run time. Write each table as db.t",
    ),
    (
        "The connector call does not match one unique audited signature.",
        "the write call's arguments don't match a known form. Pass them as the form below does, naming database=",
    ),
    (
        "The connector method is registered without deterministic asset semantics.",
        "the job writes with a method gz can't follow. Write lakehouse tables with spark.writeGzLakehouse",
    ),
    (
        "SQLGlot could not parse",
        "the SQL of a write could not be parsed. Check that it is valid Spark SQL",
    ),
    (
        "Resolved SQL exceeds",
        "the SQL of a write is too long to check. Split it into smaller statements",
    ),
    (
        "Wildcard projection requires a schema snapshot",
        "the SQL of a write selects *. Name the columns instead",
    ),
];

/// The provider's refusal, rewritten with what to change for each problem;
/// `None` when `text` is not that refusal.
pub fn explain(text: &str) -> Option<String> {
    let (branch, problems) = parse(text)?;
    let mut lines = vec![format!(
        "the ETL provider won't run the job on branch {branch}, because the run would change data outside the branch:"
    )];
    let mut needs_forms = false;
    for problem in problems {
        let advice = advise(problem, branch);
        needs_forms |= problem.starts_with(HIDDEN_WRITE) && advice.is_some();
        lines.push(format!("  - {}", tidy(problem)));
        if let Some(advice) = advice {
            lines.push(format!("    To fix: {advice}."));
        }
    }
    if needs_forms {
        lines.push(format!(
            "gz can follow these forms:\n  read:  {READ_FORM}\n  write: {WRITE_FORM}\nSQL, table and connection names must be literal strings, or built only from literals and a catalog from spark.getLakehouseCatalog(\"<Conn>\")."
        ));
    }
    Some(lines.join("\n"))
}

/// The branch and each problem the refusal lists.
fn parse(text: &str) -> Option<(&str, Vec<&str>)> {
    let rest = &text[text.find(REFUSAL_START)? + REFUSAL_START.len()..];
    let (branch, rest) = rest.split_once(LIST_START)?;
    let list = &rest[..rest.find(LIST_END)?];
    Some((branch, list.split("; ").collect()))
}

/// The provider ends the list with a period after messages that end in one.
fn tidy(problem: &str) -> &str {
    problem.strip_suffix('.').unwrap_or(problem)
}

fn advise(problem: &str, branch: &str) -> Option<String> {
    if let Some(message) = problem.strip_prefix(HIDDEN_WRITE) {
        return HIDDEN_WRITE_ADVICE
            .iter()
            .find(|(start, _)| message.starts_with(start))
            .map(|(_, advice)| advice.to_string());
    }
    if let Some(table) = problem.strip_suffix(&format!(" has no branch {branch}")) {
        return Some(format!(
            "the code writes {table}, which its saved lineage doesn't show. Add --table {table} to gz jobs try"
        ));
    }
    if problem.ends_with(" cannot be branched") {
        return Some(
            "only lakehouse tables can be tried on a branch. Run the job normally, or move that write to another job"
                .to_string(),
        );
    }
    if problem.ends_with("(its connection names no lakehouse warehouse)") {
        return Some("write through a lakehouse connection that names its warehouse".to_string());
    }
    if problem.ends_with("(the table has no database)") {
        return Some("name the table's database in the write (database=\"db\")".to_string());
    }
    if problem.starts_with("the job's code could not be read") {
        return Some("check that the job's file is valid Python".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(problems: &str) -> String {
        format!(
            "HTTP 400 Bad Request: Not running on branch dq_pass: the run would change data outside it — {problems}. Make the branch on every table the job writes (gz jobs try does), or run the job normally."
        )
    }

    #[test]
    fn dynamic_session_sql_gets_its_own_advice_and_the_forms() {
        let text = explain(&refusal(
            "a write gz cannot follow: The session SQL text is not statically deterministic.",
        ))
        .unwrap();
        assert!(
            text.starts_with("the ETL provider won't run the job on branch dq_pass"),
            "{text}"
        );
        assert!(
            text.contains("  - a write gz cannot follow: The session SQL text is not statically deterministic\n    To fix: the SQL given to spark.sql() is built at run time."),
            "{text}"
        );
        assert!(text.contains(READ_FORM), "{text}");
        assert!(text.contains(WRITE_FORM), "{text}");
        assert!(
            text.contains("spark.getLakehouseCatalog(\"<Conn>\")"),
            "{text}"
        );
        assert!(!text.contains(".."), "{text}");
    }

    #[test]
    fn unknown_sink_provenance_and_dynamic_connection_each_get_their_advice() {
        let text = explain(&refusal(
            "a write gz cannot follow: The connector connection name is not statically deterministic.; a write gz cannot follow: The sink dataframe provenance could not be resolved statically.",
        ))
        .unwrap();
        assert!(
            text.contains("statically deterministic\n    To fix: the connection name is worked out at run time"),
            "{text}"
        );
        assert!(
            text.contains("resolved statically\n    To fix: gz can't tell which tables the dataframe being written comes from"),
            "{text}"
        );
    }

    #[test]
    fn a_message_with_a_number_is_matched_by_its_start() {
        let text = explain(&refusal(
            "a write gz cannot follow: Resolved SQL exceeds 1048576 bytes.",
        ))
        .unwrap();
        assert!(
            text.contains("To fix: the SQL of a write is too long"),
            "{text}"
        );
    }

    #[test]
    fn an_unknown_message_is_kept_as_the_provider_wrote_it() {
        let text = explain(&refusal(
            "a write gz cannot follow: Something new happened.",
        ))
        .unwrap();
        assert!(
            text.contains("  - a write gz cannot follow: Something new happened"),
            "{text}"
        );
        assert!(!text.contains("To fix"), "{text}");
        assert!(!text.contains("gz can follow these forms"), "{text}");
    }

    #[test]
    fn a_table_without_the_branch_names_the_flag_that_adds_it() {
        let text = explain(&refusal(
            "manufacturing.mfg_ops.downtime has no branch dq_pass",
        ))
        .unwrap();
        assert!(
            text.contains("To fix: the code writes manufacturing.mfg_ops.downtime, which its saved lineage doesn't show. Add --table manufacturing.mfg_ops.downtime to gz jobs try."),
            "{text}"
        );
    }

    #[test]
    fn writes_outside_the_lakehouse_and_unnamed_tables_get_advice() {
        let text = explain(&refusal(
            "Lake:orders (the table has no database); Ops:stats (its connection names no lakehouse warehouse); postgres public.audit cannot be branched",
        ))
        .unwrap();
        for advice in [
            "To fix: name the table's database in the write",
            "To fix: write through a lakehouse connection",
            "To fix: only lakehouse tables can be tried on a branch",
        ] {
            assert!(text.contains(advice), "missing {advice:?} in {text}");
        }
    }

    #[test]
    fn other_errors_are_left_alone() {
        assert_eq!(
            explain("HTTP 400 Bad Request: This run can't write to branch try-1"),
            None
        );
    }
}

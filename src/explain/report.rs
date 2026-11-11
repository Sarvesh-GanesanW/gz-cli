//! The trace as text, top to bottom: figures, datasets, tables, gaps.
//! Plain text only, so it reads the same in a pipe; `-o json` has it all.

use std::fmt::Write as _;

use super::designer::{DatasetKind, Figure, FilterUse};
use super::runs::{CodeVersion, RunMatch};
use super::trace::{Changes, DatasetTrace, SnapshotView, TableTrace, Trace, Upstream};
use crate::table_history::when;
use crate::tui::format::group_digits;

/// Longer SQL is cut here, with a pointer to `-o json`.
const MAX_SQL_LINES: usize = 12;
/// Lineage edges shown per table.
const MAX_UPSTREAM: usize = 8;
/// Commits listed under "since".
const MAX_COMMITS: usize = 5;
/// Characters of a code hash shown.
const HASH_SHOWN: usize = 12;

pub fn text(trace: &Trace) -> String {
    let mut out = String::new();
    heading(&mut out, trace);
    figures(&mut out, trace);
    for dataset in &trace.datasets {
        dataset_block(&mut out, dataset);
    }
    tables(&mut out, trace);
    gaps(&mut out, &trace.gaps);
    out
}

fn grouped(number: u64) -> String {
    group_digits(&number.to_string())
}

fn heading(out: &mut String, trace: &Trace) {
    let subject = &trace.subject;
    let named = match (&subject.name, &subject.id) {
        (Some(name), Some(id)) if name != id => format!(" {name} ({id})"),
        (Some(name), _) => format!(" {name}"),
        (None, Some(id)) => format!(" {id}"),
        (None, None) => String::new(),
    };
    let moment = match &trace.at {
        Some(at) => format!("as of {}", when::display(at)),
        None => "as of now".to_string(),
    };
    let kind = match subject.kind {
        "sql" => "a SQL query",
        other => other,
    };
    let _ = writeln!(out, "Explaining {kind}{named}, {moment}");
}

fn dataset_name(trace: &Trace, id: &str) -> String {
    trace
        .datasets
        .iter()
        .find(|d| d.definition.id == id)
        .map_or_else(
            || id.to_string(),
            |d| format!("{} ({id})", d.definition.name),
        )
}

fn figures(out: &mut String, trace: &Trace) {
    if trace.figures.is_empty() {
        return;
    }
    let _ = writeln!(out, "\nFigures");
    for figure in &trace.figures {
        figure_block(out, trace, figure);
    }
}

fn figure_block(out: &mut String, trace: &Trace, figure: &Figure) {
    let dataset = figure
        .dataset_id
        .as_deref()
        .map_or("no dataset".to_string(), |id| {
            format!("dataset {}", dataset_name(trace, id))
        });
    let _ = writeln!(
        out,
        "  {} (visualization {}, {}) · {dataset}",
        figure.name, figure.id, figure.chart_type
    );
    for measure in &figure.measures {
        let mut line = match &measure.expression {
            Some(expression) => format!("{} = {}", measure.column, one_line(expression)),
            None => measure.column.clone(),
        };
        if let Some(aggregation) = &measure.chart_aggregation {
            let _ = write!(line, " ({aggregation} in the chart)");
        }
        let _ = writeln!(out, "    measure  {line}");
    }
    if figure.measures.is_empty() {
        let _ = writeln!(out, "    measure  none named in the chart's settings");
    }
    if !figure.dimensions.is_empty() {
        let _ = writeln!(out, "    by       {}", figure.dimensions.join(", "));
    }
    if let Some(note) = &figure.chart_note {
        let _ = writeln!(out, "    chart    {note}");
    }
    let used = figure
        .dataset_id
        .as_deref()
        .and_then(|id| trace.datasets.iter().find(|d| d.definition.id == id))
        .map(|d| filters_used(&d.definition.filters))
        .unwrap_or_default();
    if !used.is_empty() {
        let _ = writeln!(out, "    filters  {used}");
    }
}

fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `WC-YEAR (year): 2025, 2024 · WC-Region: APAC`, for filters the query uses.
fn filters_used(filters: &[FilterUse]) -> String {
    filters
        .iter()
        .filter(|f| !f.placeholders.is_empty())
        .map(|f| {
            let column = f
                .column
                .as_deref()
                .map_or(String::new(), |c| format!(" ({c})"));
            let values = if f.defaults.is_empty() {
                "no default".to_string()
            } else {
                f.defaults.join(", ")
            };
            format!("{}{column}: {values}", f.name)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn dataset_block(out: &mut String, dataset: &DatasetTrace) {
    let definition = &dataset.definition;
    let how = match definition.kind {
        DatasetKind::Sql => "SQL",
        DatasetKind::Code => "code",
    };
    let connections = dataset
        .connection_details
        .iter()
        .map(|c| match (&c.warehouse, &c.database) {
            (Some(warehouse), Some(database)) => {
                format!("{} → {warehouse} (default database {database})", c.name)
            }
            (Some(warehouse), None) => format!("{} → {warehouse}", c.name),
            _ => format!(
                "{} ({})",
                c.name,
                c.kind.as_deref().unwrap_or("unknown type")
            ),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(
        out,
        "\nDataset {} ({}) · {how} on {}",
        definition.name,
        definition.id,
        if connections.is_empty() {
            "no connection".to_string()
        } else {
            connections
        }
    );
    if let Some(runtime) = &definition.runtime {
        let _ = writeln!(out, "  runs in  {runtime}");
    }
    if let Some(sql) = &definition.sql {
        quoted(out, sql);
    }
    if let Some(code) = &definition.code {
        let label = match definition.kind {
            DatasetKind::Sql => "  then this prep code:",
            DatasetKind::Code => "  code:",
        };
        let _ = writeln!(out, "{label}");
        quoted(out, code);
    }
    let reads = if dataset.tables.is_empty() {
        "no lakehouse table found".to_string()
    } else {
        dataset.tables.join(", ")
    };
    let _ = writeln!(out, "  reads    {reads}");
    if !dataset.unresolved.is_empty() {
        let _ = writeln!(out, "  unsure   {}", dataset.unresolved.join(", "));
    }
}

/// The text indented under a bar, cut after [`MAX_SQL_LINES`].
fn quoted(out: &mut String, text: &str) {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .skip_while(|l| l.is_empty())
        .collect();
    let lines: Vec<&str> = {
        let end = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        lines[..end].to_vec()
    };
    for line in lines.iter().take(MAX_SQL_LINES) {
        let _ = writeln!(out, "  │ {line}");
    }
    if lines.len() > MAX_SQL_LINES {
        let _ = writeln!(
            out,
            "  │ … {} more lines (-o json shows all of it)",
            lines.len() - MAX_SQL_LINES
        );
    }
}

fn tables(out: &mut String, trace: &Trace) {
    for table in &trace.tables {
        table_block(out, table, trace.at.is_some());
    }
}

fn table_block(out: &mut String, table: &TableTrace, has_at: bool) {
    let _ = writeln!(
        out,
        "\nTable {} · read by {}",
        table.key,
        table.read_by.join(", ")
    );
    if let Some(snapshot) = &table.snapshot {
        let _ = writeln!(out, "  snapshot  {}", snapshot_line(snapshot));
        if let Some(writer) = writer(snapshot) {
            let _ = writeln!(out, "  written   by {writer}");
        }
        match &table.run {
            Some(found) => run_lines(out, found),
            None if table.problems.iter().all(|p| !p.starts_with("runs:")) => {
                let _ = writeln!(
                    out,
                    "  run       none found: no job or dataset run was going when it was committed"
                );
            }
            None => {}
        }
    }
    if let Some(version) = &table.code_version {
        let _ = writeln!(out, "  code      {}", version_line(version));
    }
    if let Some(edges) = &table.upstream {
        upstream_lines(out, edges);
    }
    if has_at {
        if let Some(changes) = &table.changes_since {
            changes_lines(out, changes);
        } else if table.snapshot.is_some() {
            let _ = writeln!(out, "  since     no commits since then");
        }
    }
    for problem in &table.problems {
        let _ = writeln!(out, "  unknown   {problem}");
    }
}

/// `6861193065538938533 · 2026-09-10 07:21 UTC · overwrite · 120,000 records (+120,000) · current`.
fn snapshot_line(snapshot: &SnapshotView) -> String {
    let mut parts = vec![snapshot.id.clone(), when::display(&snapshot.committed_at)];
    parts.extend(snapshot.operation.clone());
    match (snapshot.total_records, snapshot.added_records) {
        (Some(total), Some(added)) => {
            parts.push(format!("{} records (+{})", grouped(total), grouped(added)))
        }
        (Some(total), None) => parts.push(format!("{} records", grouped(total))),
        _ => {}
    }
    if !snapshot.names.is_empty() {
        parts.push(snapshot.names.join(", "));
    }
    parts.join(" · ")
}

fn writer(snapshot: &SnapshotView) -> Option<String> {
    match (&snapshot.app_name, &snapshot.spark_app_id) {
        (Some(name), Some(app)) => Some(format!("{name} (Spark application {app})")),
        (Some(name), None) => Some(name.clone()),
        (None, Some(app)) => Some(format!("Spark application {app}")),
        (None, None) => None,
    }
}

fn run_lines(out: &mut String, found: &RunMatch) {
    let run = &found.run;
    let what = match (&run.job_name, run.job_id, &run.dataset_name, run.dataset_id) {
        (Some(name), Some(id), _, _) => format!("job {name} ({id})"),
        (None, Some(id), _, _) => format!("job {id}"),
        (_, None, Some(name), Some(id)) => format!("dataset {name} ({id})"),
        (_, None, None, Some(id)) => format!("dataset {id}"),
        _ => "an unnamed workload".to_string(),
    };
    let project = run
        .project_name
        .as_deref()
        .map_or(String::new(), |p| format!(", project {p}"));
    let ended = run
        .completed_at
        .as_deref()
        .map_or("still going".to_string(), when::display);
    let _ = writeln!(
        out,
        "  run       {} of {what}{project} · {} · {} → {ended}",
        run.execution_id,
        run.status,
        when::display(&run.started_at)
    );
    let _ = writeln!(
        out,
        "  match     {}: {}",
        found.certainty.label(),
        found.reasons.join("; ")
    );
    if !found.alternatives.is_empty() {
        let _ = writeln!(out, "  or        {}", found.alternatives.join(", "));
    }
}

fn version_line(version: &CodeVersion) -> String {
    let mut parts = vec![match version.number {
        Some(number) => format!("version {number}"),
        None => format!("version id {}", version.version_id),
    }];
    if let Some(hash) = &version.code_hash {
        parts.push(hash.chars().take(HASH_SHOWN).collect());
    }
    if let Some(compiled) = &version.compiled_on {
        parts.push(format!("saved {}", when::display(compiled)));
    }
    if let Some(uri) = &version.source_uri {
        parts.push(uri.clone());
    }
    parts.join(" · ")
}

fn upstream_lines(out: &mut String, edges: &[Upstream]) {
    if edges.is_empty() {
        let _ = writeln!(out, "  upstream  none recorded");
        return;
    }
    for (i, edge) in edges.iter().take(MAX_UPSTREAM).enumerate() {
        let label = if i == 0 { "upstream" } else { "" };
        let mut detail: Vec<String> = edge.operation.iter().cloned().collect();
        detail.extend(edge.event_time.as_deref().map(when::display));
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", detail.join(", "))
        };
        let _ = writeln!(
            out,
            "  {label:<8}  {} → {}{detail}",
            edge.source, edge.target
        );
    }
    if edges.len() > MAX_UPSTREAM {
        let _ = writeln!(
            out,
            "            … {} more (-o json shows all)",
            edges.len() - MAX_UPSTREAM
        );
    }
}

fn changes_lines(out: &mut String, changes: &Changes) {
    let count = changes.commits.len();
    let records = match (changes.records_then, changes.records_now) {
        (Some(then), Some(now)) => {
            let delta = now as i128 - then as i128;
            format!(
                "; records {} → {} ({}{})",
                grouped(then),
                grouped(now),
                if delta >= 0 { "+" } else { "−" },
                grouped(delta.unsigned_abs() as u64)
            )
        }
        _ => String::new(),
    };
    let _ = writeln!(
        out,
        "  since     {count} commit{} after it{records}",
        if count == 1 { "" } else { "s" }
    );
    for commit in changes.commits.iter().take(MAX_COMMITS) {
        let _ = writeln!(out, "            {}", snapshot_line(commit));
    }
    if count > MAX_COMMITS {
        let _ = writeln!(out, "            … {} more", count - MAX_COMMITS);
    }
}

fn gaps(out: &mut String, gaps: &[String]) {
    if gaps.is_empty() {
        return;
    }
    let _ = writeln!(out, "\nWhat gz cannot tell");
    for gap in gaps {
        let _ = writeln!(out, "  - {gap}");
    }
}

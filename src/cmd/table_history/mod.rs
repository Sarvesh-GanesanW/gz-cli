//! `gz table versions`, `gz diff` and `gz bisect`: a lakehouse table's
//! history, what changed between two of its snapshots, and the snapshot
//! where it started failing a check. The history is one read of the
//! provider; comparing rows and running a check use a SQL session, the same
//! one `gz sql` picks, reuses and stops.

pub(crate) mod report;

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{bail, Result};

use crate::cli::{BisectArgs, DiffArgs, ShellArgs, VersionsArgs};
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};
use crate::sql::Runner;
use crate::table_history::bisect::{self, Check, Outcome};
use crate::table_history::diff::{self, Comparison, RowRequest};
use crate::table_history::{History, TableName};
use report::Subject;

/// Width used for sample tables when output is not a terminal.
const PIPE_WIDTH: usize = 200;

fn table_width() -> usize {
    crossterm::terminal::size()
        .map(|(columns, _)| columns as usize)
        .ok()
        .filter(|_| std::io::stdout().is_terminal())
        .unwrap_or(PIPE_WIDTH)
}

/// The session options with the warehouse already chosen, so the session
/// opens on the warehouse whose history was read.
fn on_warehouse(shell: &ShellArgs, warehouse: &str) -> ShellArgs {
    ShellArgs {
        warehouse: Some(warehouse.to_string()),
        ..shell.clone()
    }
}

pub async fn versions(rt: &Runtime, args: &VersionsArgs) -> Result<()> {
    let table = TableName::parse(&args.table)?;
    let shell = ShellArgs {
        warehouse: args.warehouse.clone(),
        ..ShellArgs::default()
    };
    let warehouse = crate::sql::warehouse(rt, &shell).await?;
    let reply = History::fetch(&rt.http, &warehouse.name, &table).await?;
    if rt.format != OutputFormat::Table {
        return rt.show(reply).await;
    }
    let history = History::parse(&reply)?;
    if history.snapshots.is_empty() {
        output::info(&format!("{table} has no snapshot yet"));
        return Ok(());
    }
    rt.show_columns(&report::versions_rows(&history), &report::VERSION_COLUMNS)
}

pub async fn diff(rt: &Runtime, args: &DiffArgs) -> Result<()> {
    let table = TableName::parse(&args.table)?;
    let warehouse = crate::sql::warehouse(rt, &args.shell).await?;
    let history = History::load(&rt.http, &warehouse.name, &table).await?;
    let comparison = Comparison::between(&history, &args.from, &args.to)?;
    let rows = if args.rows || !args.key.is_empty() {
        let session = Runner::open(rt, &on_warehouse(&args.shell, &warehouse.name)).await?;
        if !rt.quiet {
            output::info("Comparing rows…");
        }
        let request = RowRequest {
            table: &table,
            from: &comparison.from,
            to: &comparison.to,
            key: &args.key,
            sample: args.sample,
        };
        let compared = diff::compare_rows(&session, &request).await;
        session.close(rt, args.shell.keep_session).await;
        Some(compared?)
    } else {
        None
    };
    let subject = Subject {
        table: &table,
        warehouse: &warehouse.name,
        history: &history,
    };
    if rt.format != OutputFormat::Table {
        return rt
            .show(report::diff_json(&subject, &comparison, rows.as_ref()))
            .await;
    }
    print!(
        "{}",
        report::diff_text(&subject, &comparison, rows.as_ref(), table_width())
    );
    Ok(())
}

fn check_of(args: &BisectArgs) -> Result<Check> {
    match (&args.condition, &args.check) {
        (Some(condition), None) => Check::condition(condition),
        (None, Some(query)) => Check::query(&crate::cmd::read_text_arg(query)?),
        _ => bail!("give the check as --where CONDITION or --check SQL (one of them)"),
    }
}

pub async fn bisect(rt: &Runtime, args: &BisectArgs) -> Result<()> {
    let table = TableName::parse(&args.table)?;
    let check = check_of(args)?;
    let warehouse = crate::sql::warehouse(rt, &args.shell).await?;
    let history = History::load(&rt.http, &warehouse.name, &table).await?;
    let range = bisect::range(&history, args.good.as_deref(), &args.bad)?;

    let session = Runner::open(rt, &on_warehouse(&args.shell, &warehouse.name)).await?;
    let show_progress = !rt.quiet && rt.format == OutputFormat::Table;
    let searched = bisect::search(&session, &table, &check, &range, |probe| {
        if show_progress {
            output::info(&report::probe_line(probe));
        }
    })
    .await;
    session.close(rt, args.shell.keep_session).await;
    let search = searched?;

    let subject = Subject {
        table: &table,
        warehouse: &warehouse.name,
        history: &history,
    };
    if rt.format != OutputFormat::Table {
        return rt
            .show(report::bisect_json(&subject, &check, &search))
            .await;
    }
    print!("{}", report::bisect_text(&subject, &check, &search));
    let Outcome::Found {
        last_good,
        first_bad,
    } = &search.outcome
    else {
        return Ok(());
    };
    if !args.code {
        return Ok(());
    }
    let prompt = report::code_prompt(&subject, &check, last_good, first_bad);
    eprintln!(
        "{}",
        output::dim("── gz code starts with this message in its input box: review or edit it, then press Enter ──")
    );
    println!("{prompt}\n");
    crate::cmd::code::run_with_draft(rt, Path::new("."), prompt).await
}

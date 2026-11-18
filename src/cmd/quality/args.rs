//! Flags for `gz quality`.

use clap::{Args, Subcommand, ValueEnum};

use crate::cli::ScanArgs;

#[derive(Args, Debug)]
pub struct QualityArgs {
    #[command(subcommand)]
    pub action: QualityAction,
}

#[derive(Subcommand, Debug)]
pub enum QualityAction {
    #[command(
        about = "Each branch's quality verdict: passed or failed, for its current head or an older snapshot"
    )]
    Show {
        #[arg(value_name = "WAREHOUSE.DATABASE.TABLE")]
        table: String,
    },
    #[command(
        about = "Require a passing verdict on a branch's current head before it can be published"
    )]
    Require {
        #[arg(value_name = "WAREHOUSE.DATABASE.TABLE")]
        table: String,
        #[arg(long, help = "Turn the requirement off again")]
        off: bool,
    },
    #[command(
        about = "Past check results for a table, newest first, from the tables jobs saved them to (--limit, default 20)"
    )]
    History(HistoryArgs),
    #[command(
        about = "Run the checks on a table now, on Groundzero compute, and print the report",
        long_about = CHECK_ABOUT
    )]
    Check(CheckArgs),
}

const CHECK_ABOUT: &str = "Run the data quality checks on a lakehouse table now and print the report.

gz writes a small Spark (or DuckDB) job that calls runTableDataQualityChecks, \
runs it as a temporary job in --project with the lakehouse connection of the \
table's warehouse, reads the report from the run's log, and deletes the job. \
The run starts paid compute.

Each check shows its severity, whether it ran, and its key numbers; -o json \
prints the whole report (row values left out). The exit code is 2 when the \
gate fails: a check at --fail-on or worse, or a check that could not run.

--branch checks the head of a branch or tag. Checking a branch records its \
quality verdict on the table, as a job would, which needs permission to alter \
the table. --results appends one row per check to a results table, which \
`gz quality history` then finds (Spark runtimes only).

Examples:
  gz quality check Lineage.sales.orders --project 1999
  gz quality check Lineage.sales.orders --project 1999 --severity outliers=error --fail-on warn
  gz quality check Lineage.sales.orders --project 1999 --branch try-nightly --results Lineage.dq.results
  gz quality check Lineage.sales.orders --project 1999 --config @quality.yaml -o json";

/// The severity that fails `gz quality check`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum FailOn {
    Warn,
    Error,
}

impl FailOn {
    /// As qualipilot and the runtime spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            FailOn::Warn => "warn",
            FailOn::Error => "error",
        }
    }
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    #[arg(value_name = "WAREHOUSE.DATABASE.TABLE")]
    pub table: String,
    #[arg(
        long,
        help = "DE/ML project to run the temporary job in (see `gz projects list`)"
    )]
    pub project: i64,
    #[arg(
        long,
        value_enum,
        default_value = "error",
        help = "Fail (exit 2) when a check reaches this severity"
    )]
    pub fail_on: FailOn,
    #[arg(
        long,
        value_name = "ID",
        conflicts_with = "branch",
        help = "Check this snapshot instead of the current one"
    )]
    pub snapshot: Option<i64>,
    #[arg(
        long,
        value_name = "NAME",
        help = "Check the head of this branch or tag"
    )]
    pub branch: Option<String>,
    #[arg(
        long,
        value_name = "WAREHOUSE.DATABASE.TABLE",
        help = "Also append the results to this table (created when missing)"
    )]
    pub results: Option<String>,
    #[arg(
        long = "severity",
        value_name = "CHECK=LEVEL",
        help = "Severity a check gets when it finds something, e.g. outliers=error (ok, warn or error); repeatable"
    )]
    pub severities: Vec<String>,
    #[arg(
        long,
        value_name = "CONFIG",
        help = "Full qualipilot config, JSON or YAML (inline, or @file)"
    )]
    pub config: Option<String>,
    #[arg(
        long,
        value_name = "NAME|ID",
        default_value = "small",
        help = "Compute size (see `gz rte compute`)"
    )]
    pub compute: String,
    #[arg(
        long,
        default_value = "gz-rte-spark-sklearn:1.0.0",
        help = "Runtime image (see `gz rte list`)"
    )]
    pub rte: String,
    #[arg(
        long,
        value_name = "NAME",
        help = "Lakehouse connection to read the table through (default: one whose warehouse is the table's)"
    )]
    pub connection: Option<String>,
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    #[arg(value_name = "WAREHOUSE.DATABASE.TABLE", help = "The checked table")]
    pub table: String,
    #[arg(
        long,
        value_name = "WAREHOUSE.DATABASE.TABLE",
        help = "Read this results table (resultsTable=, saveDataQualityResults); default: every results table jobs linked to the table"
    )]
    pub results: Option<String>,
    #[arg(long, help = "Only warnings, errors and checks that failed to run")]
    pub failed: bool,
    #[arg(long, help = "Compute size name or id, e.g. small (remembered)")]
    pub compute: Option<String>,
    #[arg(
        long,
        help = "Leave the session running on exit; the next gz sql reuses it"
    )]
    pub keep_session: bool,
    #[command(flatten)]
    pub scan: ScanArgs,
}

//! Flags for `gz quality`.

use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::cli::ScanArgs;
use crate::notify::NotifyArgs;

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
    #[command(
        about = "Run a table's checks on a schedule on Groundzero, with an email when they fail",
        long_about = WATCH_ABOUT
    )]
    Watch(WatchArgs),
    #[command(
        about = "Check a quality.toml and list every problem with its line and column",
        long_about = LINT_ABOUT
    )]
    Lint {
        #[arg(
            value_name = "PATH",
            help = "The file, or a folder holding quality.toml (default: ./quality.toml)"
        )]
        path: Option<PathBuf>,
    },
}

const LINT_ABOUT: &str = "Check a quality.toml and list every problem with its line and column.

quality.toml sits next to the job files of a `gz sync` folder and declares \
checks per output table. `gz jobs try` runs them on the try's branch and \
turns on the table's publish gate, so the branch publishes (from gz or the \
web app) only with a passing verdict; `gz jobs publish` also refuses when \
the verdict is from other rules than the file's.

  [tables.\"Lineage.sales.orders\"]
  fail_on = \"error\"                      # or \"warn\"
  rows = { min = 1, max = 5_000_000 }
  not_null = [\"order_id\"]
  null_fraction = { coupon = 0.9 }       # at most 90% nulls
  unique = [\"order_id\", [\"customer_id\", \"ordered_at\"]]
  accepted_values = { status = [\"new\", \"paid\", \"shipped\"] }
  ranges = { amount = { min = 0, max = 100000 } }
  freshness = { column = \"ordered_at\", max_age = \"24h\" }
  max_commit_age = \"24h\"

  [[tables.\"Lineage.sales.orders\".sql]]
  name = \"no_negative_totals\"
  query = \"SELECT * FROM {table} WHERE total < 0\"   # fails when rows come back
  severity = \"error\"                     # or \"warn\"";

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
        help = "Fail (exit 2) when a check reaches this severity [default: error]"
    )]
    pub fail_on: Option<FailOn>,
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
    #[arg(
        long,
        value_name = "PATH",
        num_args = 0..=1,
        default_missing_value = "quality.toml",
        conflicts_with_all = ["config", "severities", "snapshot", "fail_on"],
        help = "Run the table's rules from a quality.toml (default ./quality.toml) instead of the default checks; Spark runtimes only"
    )]
    pub rules: Option<PathBuf>,
    #[command(flatten)]
    pub notify: NotifyArgs,
}

impl CheckArgs {
    pub fn fail_on(&self) -> FailOn {
        self.fail_on.unwrap_or(FailOn::Error)
    }
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

const WATCH_ABOUT: &str =
    "Run a table's checks on a schedule on Groundzero, with an email when they fail.

gz makes a job in --project that runs the checks (the table's rules from \
--rules, else the default checks of `gz quality check`) and a schedule that \
runs it --every interval, so the watch keeps running when this machine is \
off. --max-age also fails the check when the table's last commit is older. \
A failing check fails the run, which sends the schedule's failure email to \
--notify-email. Each run starts paid compute.

Watches are named gz-quality-watch-<table>-<id>; the web app's Scheduler \
shows them too.

Examples:
  gz quality watch Lineage.sales.orders --project 1999 --every 1h --max-age 24h
  gz quality watch Lineage.sales.orders --project 1999 --every 6h --rules --notify-email ops@acme.io
  gz quality watch list
  gz quality watch show Lineage.sales.orders --wait
  gz quality watch stop Lineage.sales.orders";

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct WatchArgs {
    #[command(subcommand)]
    pub action: Option<WatchAction>,
    #[command(flatten)]
    pub create: WatchCreateArgs,
}

#[derive(Subcommand, Debug)]
pub enum WatchAction {
    #[command(about = "Every watch, with its schedule and latest run")]
    List,
    #[command(
        about = "A watch's schedule, recent runs and the latest finished run's checks (exit 2 when they failed)"
    )]
    Show {
        #[arg(
            value_name = "TABLE|ID",
            help = "The watched table, or the watch's schedule id"
        )]
        watch: String,
        #[arg(long, help = "Wait for a run that is going to end first")]
        wait: bool,
    },
    #[command(about = "Stop a watch: delete its schedule, failure email and job")]
    Stop {
        #[arg(
            value_name = "TABLE|ID",
            help = "The watched table, or the watch's schedule id"
        )]
        watch: String,
        #[arg(short, long, help = "Stop without asking")]
        yes: bool,
    },
}

#[derive(Args, Debug)]
pub struct WatchCreateArgs {
    #[arg(value_name = "WAREHOUSE.DATABASE.TABLE", required = true)]
    pub table: Option<String>,
    #[arg(
        long,
        required = true,
        help = "DE/ML project the watch's job lives in (see `gz projects list`)"
    )]
    pub project: Option<i64>,
    #[arg(
        long,
        value_name = "INTERVAL",
        required = true,
        help = "How often: 5m, 10m, 15m, 20m, 30m, 1h, 2h, 3h, 4h, 6h, 8h, 12h, 1d or 7d"
    )]
    pub every: Option<String>,
    #[arg(
        long,
        value_name = "AGE",
        help = "Also fail when the table's last commit is older than this, e.g. 24h"
    )]
    pub max_age: Option<String>,
    #[arg(
        long,
        value_name = "PATH",
        num_args = 0..=1,
        default_missing_value = "quality.toml",
        help = "Check the table's rules from a quality.toml (default ./quality.toml) instead of the default checks"
    )]
    pub rules: Option<PathBuf>,
    #[arg(
        long,
        value_enum,
        conflicts_with = "rules",
        help = "Fail at this severity [default: error; with --rules, the file's fail_on]"
    )]
    pub fail_on: Option<FailOn>,
    #[arg(
        long = "notify-email",
        value_name = "ADDRESS",
        help = "Email this address when a run fails (repeatable; the first gets it, the rest are copied)"
    )]
    pub notify_email: Vec<String>,
    #[arg(
        long,
        value_name = "ADDRESS",
        requires = "notify_email",
        help = "Send the failure email from this address (default: the first --notify-email)"
    )]
    pub from_email: Option<String>,
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
        help = "Spark runtime image (see `gz rte list`)"
    )]
    pub rte: String,
    #[arg(
        long,
        value_name = "NAME",
        help = "Lakehouse connection to read the table through (default: one whose warehouse is the table's)"
    )]
    pub connection: Option<String>,
    #[arg(long, help = "Also run the checks once now")]
    pub run_now: bool,
}

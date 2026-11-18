//! Flags for `gz review`.

use clap::{Args, ValueEnum};
use std::path::PathBuf;

pub(crate) const REVIEW_ABOUT: &str = "Review job changes in a `gz sync` folder before they ship.

For each changed job (its local file against the code saved on the platform; \
with no FILEs, every job in the folder whose code differs) gz review:
  1. shows the code diff against the saved version;
  2. lists the tables the job writes and what depends on them downstream \
(the same lineage walk as `gz impact`);
  3. runs the local code with `gz jobs try` on a write branch of each table it \
writes, then compares each table's rows and schema, branch against main (the \
same comparison as `gz diff --rows`, on a SQL session gz stops afterwards \
unless it was already running);
  4. reports the quality results the try run recorded;
  5. gives a verdict: pass, warn or fail, with the reasons.

The branches review makes are dropped at the end, unless --keep-branches. \
Without --compute and --rte the jobs are not tried, which makes the verdict \
at best warn. A job that can't be tried (it writes no lakehouse table, or \
the ETL provider refuses the branch run) is reported with the reason, and \
its other steps still run.

The verdict fails on: a failed try run, a refusal by the quality gate, or \
dependents of a --fail-on kind (or the folder's `[impact] fail_on`). It \
warns on: a step that could not run, a dropped or retyped column, a job \
not in the project yet, a removed file.

Exit codes: 0 pass (or warn without --strict), 1 gz could not finish a \
step, 2 fail, 3 warn with --strict.

Examples:
  gz review --compute 1003 --rte gz-rte-spark-pytorch:1.0.0
  gz review etl/clean_orders.py --compute 1003 --rte gz-rte-spark-pytorch:1.0.0 --sql-compute small
  gz review etl/clean_orders.py --format markdown > review.md     for a PR comment
  gz review -o json --strict                                        for a script";

/// How the review is printed when `-o` is the default table view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum ReviewFormat {
    #[default]
    Text,
    Markdown,
}

#[derive(Args, Debug)]
pub struct ReviewArgs {
    #[arg(
        value_name = "FILE",
        help = "Changed job files (default: every job whose local code differs from the saved code)"
    )]
    pub paths: Vec<PathBuf>,
    #[arg(
        long,
        default_value = ".",
        help = "The gz sync folder holding the job files (and gz-sync.toml)"
    )]
    pub dir: PathBuf,
    #[arg(
        long,
        help = "Project of the jobs (default: the folder's [impact] project or last sync)"
    )]
    pub project: Option<i64>,
    #[arg(
        long,
        requires = "rte",
        help = "Compute size id for the try runs (see `gz rte compute`); without it jobs are not tried"
    )]
    pub compute: Option<i64>,
    #[arg(
        long,
        requires = "compute",
        help = "Runtime image for the try runs, e.g. gz-rte-spark-pytorch:1.0.0 (see `gz rte list`)"
    )]
    pub rte: Option<String>,
    #[arg(
        long,
        requires = "compute",
        help = "Override the jobs' minimum workers"
    )]
    pub workers: Option<u32>,
    #[arg(long, requires = "compute", help = "Override the jobs' GPU size")]
    pub gpu: Option<u32>,
    #[arg(long, help = "Don't compare rows (no SQL session is started)")]
    pub no_rows: bool,
    #[arg(
        long,
        value_name = "NAME",
        help = "Compute size for the SQL session that compares rows, e.g. small (default: the one gz sql remembers)"
    )]
    pub sql_compute: Option<String>,
    #[arg(
        long,
        default_value = "3",
        value_parser = clap::value_parser!(u32).range(1..=10),
        help = "Table hops to follow downstream (1-10)"
    )]
    pub depth: u32,
    #[arg(
        long,
        value_name = "KINDS",
        value_delimiter = ',',
        help = "Fail when dependents of these kinds exist: tables, jobs, datasets, visualizations, dashboards, agents"
    )]
    pub fail_on: Vec<String>,
    #[arg(long, help = "Exit 3 when the verdict is warn")]
    pub strict: bool,
    #[arg(long, help = "Keep the branches the try runs wrote, to look at them")]
    pub keep_branches: bool,
    #[arg(
        long,
        value_enum,
        default_value = "text",
        help = "text, or markdown for a pull request comment (-o json for scripts)"
    )]
    pub format: ReviewFormat,
}

//! Flags for `gz quality`.

use clap::{Args, Subcommand};

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
        about = "Past check results for a table, newest first, from a results table (--limit, default 20)"
    )]
    History(HistoryArgs),
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    #[arg(value_name = "WAREHOUSE.DATABASE.TABLE", help = "The checked table")]
    pub table: String,
    #[arg(
        long,
        value_name = "WAREHOUSE.DATABASE.TABLE",
        help = "Where the checks saved their results (runTableDataQualityChecks resultsTable=, saveDataQualityResults)"
    )]
    pub results: String,
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

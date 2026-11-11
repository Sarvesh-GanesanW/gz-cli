//! Flags for `gz jobs try`, `gz jobs publish` and `gz jobs discard`.

use clap::Args;
use std::path::PathBuf;

use crate::cli::RunChoiceArgs;

/// The job and the tables it writes, shared by all three commands.
#[derive(Args, Debug, Clone)]
pub struct BranchJobArgs {
    #[arg(help = "Job id or name")]
    pub job: String,
    #[arg(
        long,
        help = "Project id (default: the folder's gz sync project, else the one project with the job)"
    )]
    pub project: Option<i64>,
    #[arg(
        long = "table",
        value_name = "WAREHOUSE.DATABASE.TABLE",
        help = "A table the job writes that its lineage doesn't show (repeatable)"
    )]
    pub tables: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct TryArgs {
    #[command(flatten)]
    pub target: BranchJobArgs,
    #[arg(
        long,
        help = "Branch name (default: try-<job>-<UTC time>); letters, digits, '_' and '-'"
    )]
    pub branch: Option<String>,
    #[arg(
        long,
        default_value = ".",
        help = "The gz sync folder holding the job's code; without a mapped file the saved code runs"
    )]
    pub dir: PathBuf,
    #[command(flatten)]
    pub choice: RunChoiceArgs,
}

#[derive(Args, Debug, Clone)]
pub struct PublishArgs {
    #[command(flatten)]
    pub target: BranchJobArgs,
    #[arg(long, help = "The branch `gz jobs try` made")]
    pub branch: String,
    #[arg(short, long, help = "Publish without asking")]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DiscardArgs {
    #[command(flatten)]
    pub target: BranchJobArgs,
    #[arg(long, help = "The branch `gz jobs try` made")]
    pub branch: String,
}

mod apikey;
mod assistant;
mod cli;
mod cmd;
mod code_ui;
mod cognito;
mod complete;
mod config;
mod error_log;
mod explain;
mod http;
mod http_retry;
mod impact;
mod inline_term;
mod install_method;
mod notify;
mod oauth;
mod output;
mod resolve;
mod skills;
mod spend;
mod sql;
mod sse;
mod subagents;
mod table_history;
mod timing;
mod tui;
mod webapp;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, OutputArg};
use crate::cmd::Runtime;
use crate::output::OutputFormat;

#[tokio::main]
async fn main() {
    restore_sigpipe();
    if let Err(err) = run().await {
        // The run's report already went to stdout, error included.
        if let Some(exit) = err.downcast_ref::<cmd::code_report::ReportedExit>() {
            std::process::exit(exit.0);
        }
        // A spent cap gets its own code, so scripts can tell it apart.
        let code = if err.downcast_ref::<spend::caps::CapReached>().is_some() {
            spend::caps::CapReached::EXIT_CODE
        } else {
            1
        };
        report_error(err);
        std::process::exit(code);
    }
}

async fn run() -> Result<()> {
    let words: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if words.first().map(String::as_str) == Some(complete::COMMAND) {
        return complete::run(&words[1..]).await;
    }
    // Names of ids are swapped for ids before clap reads the line, so the
    // first parse reads a copy with placeholders in their place.
    let plan = resolve::rewrite::plan(&resolve::walk::command_tree(), &words);
    let first = if plan.is_empty() {
        words
    } else {
        plan.placeholder()
    };
    let mut cli = parse(first);
    error_log::remember_args(std::env::args_os().map(|arg| arg.to_string_lossy().into_owned()));
    let rt = runtime(&cli)?;
    if !plan.is_empty() {
        cli = parse(resolve::rewrite::apply(&rt, &plan).await?);
    }
    cmd::dispatch(&rt, &cli.command).await
}

fn parse(words: Vec<String>) -> Cli {
    Cli::parse_from(std::iter::once("gz".to_string()).chain(words))
}

fn runtime(cli: &Cli) -> Result<Runtime> {
    let format = output_format(cli);
    // Full-screen and conversational commands draw their own progress.
    let quiet = cli.quiet
        || matches!(
            cli.command,
            crate::cli::Command::Tui | crate::cli::Command::Code(_)
        )
        || is_sql_shell(&cli.command);
    let config = config::load_config()?;
    let resolved = config::resolve(
        &config,
        cli.profile.as_deref(),
        cli.host.clone(),
        cli.token.clone(),
    );
    let http = http::ApiClient::new(resolved, format, quiet, cli.verbose, cli.timeout)?;
    Ok(Runtime {
        http,
        format,
        limit: cli.limit,
        quiet,
        asked_quiet: cli.quiet,
        no_input: cli.no_input,
    })
}

/// The SQL shell draws its own progress; spinners would break its screen.
fn is_sql_shell(command: &crate::cli::Command) -> bool {
    matches!(
        command,
        crate::cli::Command::Sql(crate::cli::SqlArgs {
            action: None | Some(crate::cli::SqlAction::Shell(_)),
            ..
        })
    )
}

fn output_format(cli: &Cli) -> OutputFormat {
    if cli.json {
        return OutputFormat::Json;
    }
    match cli.output {
        OutputArg::Table => OutputFormat::Table,
        OutputArg::Json => OutputFormat::Json,
        OutputArg::Yaml => OutputFormat::Yaml,
        OutputArg::Csv => OutputFormat::Csv,
    }
}

#[cfg(unix)]
fn restore_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn report_error(err: anyhow::Error) {
    let wants_json = std::env::args().any(|arg| arg == "--json" || arg == "-o")
        || std::env::args().any(|arg| arg == "json");
    if wants_json {
        let value = serde_json::json!({"error": format!("{err:#}")});
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        eprintln!("gz: error: {err:#}");
    }
}

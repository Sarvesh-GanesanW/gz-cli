mod cli;
mod cmd;
mod config;
mod http;
mod oauth;
mod output;
mod tui;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, OutputArg};
use crate::cmd::Runtime;
use crate::output::OutputFormat;

#[tokio::main]
async fn main() {
    restore_sigpipe();
    if let Err(err) = run().await {
        report_error(err);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let format = output_format(&cli);
    let quiet = cli.quiet || matches!(cli.command, crate::cli::Command::Tui);
    let config = config::load_config()?;
    let resolved = config::resolve(&config, cli.profile.as_deref(), cli.host, cli.token);
    let http = http::ApiClient::new(resolved, format, quiet, cli.verbose, cli.timeout)?;
    let rt = Runtime {
        http,
        format,
        limit: cli.limit,
        quiet,
    };
    cmd::dispatch(&rt, &cli.command).await
}

fn output_format(cli: &Cli) -> OutputFormat {
    if cli.json {
        return OutputFormat::Json;
    }
    match cli.output {
        OutputArg::Table => OutputFormat::Table,
        OutputArg::Json => OutputFormat::Json,
        OutputArg::Yaml => OutputFormat::Yaml,
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

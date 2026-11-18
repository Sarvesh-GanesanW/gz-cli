//! Flags for `gz audit`.

use clap::{Args, Subcommand};

use super::findings::{FindingKind, Severity};

#[derive(Args, Debug)]
pub struct AuditArgs {
    #[command(subcommand)]
    pub action: AuditAction,
}

#[derive(Subcommand, Debug)]
pub enum AuditAction {
    #[command(
        about = "Every agent on the site: owner, model, tools, connections, approvals, limits, recent runs, and findings (admins only)",
        long_about = AGENTS_ABOUT
    )]
    Agents(AgentsAuditArgs),
}

#[derive(Args, Debug)]
pub struct AgentsAuditArgs {
    #[arg(long, value_name = "USER", help = "Only agents this user owns")]
    pub owner: Option<String>,
    #[arg(
        long,
        value_enum,
        value_name = "FINDING",
        help = "Only these findings, and the agents that have them (repeatable)"
    )]
    pub finding: Vec<FindingKind>,
    #[arg(
        long,
        value_enum,
        value_name = "SEVERITY",
        help = "Only findings at this severity or above, and the agents that have them"
    )]
    pub severity: Option<Severity>,
    #[arg(
        long,
        value_enum,
        value_name = "SEVERITY",
        help = "Exit with code 2 when a shown finding is at this severity or above"
    )]
    pub fail_on: Option<Severity>,
    #[arg(
        long,
        default_value_t = 30,
        value_parser = clap::value_parser!(u32).range(1..=365),
        help = "Days of runs to count; also how long without a run makes an agent unused"
    )]
    pub days: u32,
}

const AGENTS_ABOUT: &str = "Every agent on the site, for administrators.

For each agent: owner and whether that user still exists, when it was created \
and last changed, its model, its tools (built-in, and the client tool \
parse_pdf; machine-local tools belong to gz code, not to saved agents), its MCP \
connections and the tools they expose, the connections it names (its data \
tools also reach whatever the person running it can reach), sub-agents and \
team, which tools ask for approval, its limits and quota, and its runs, \
failures and cost over the last --days days.

Findings, each with a severity and a reason:
  write-without-approval  a tool that changes data runs without asking (high
                          for elevated tools); low when only ETL jobs skip it
  mcp-unreviewed          MCP tools, which can't be put behind approval
  missing-connection      an MCP connection it names is gone or isn't MCP
  sensitive-outbound      reads site data, or is classified confidential or
                          restricted (high), and can send data out unreviewed
                          (web_fetch, MCP)
  no-owner, owner-gone    no owner, or the owner left the user pool or is
                          disabled
  unused                  no run in --days days
  failing-runs            20% of finished runs failed (high at 50% and 3+)
  spend-outlier           spent over 3x the median spending agent and $1
  sub-agent-broader       a sub-agent or team member can use tools or MCP
                          connections its parent can't
  missing-sub-agent       a sub-agent points to a deleted agent

-o json prints every record with its findings; -o csv prints one row per \
finding. --fail-on exits with code 2 when a shown finding is at that severity \
or above, for CI.

The agent builder decides who is an administrator (the site's Admin group); \
anyone else is refused.";

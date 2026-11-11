use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum OutputArg {
    #[default]
    Table,
    Json,
    Yaml,
    Csv,
}

#[derive(Parser, Debug)]
#[command(
    name = "gz",
    version,
    about = "Groundzero CLI - notebooks, lakehouse, ETL, MLOps and agents from one fast binary"
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        env = "GZ_PROFILE",
        help = "Config profile to use"
    )]
    pub profile: Option<String>,
    #[arg(
        long,
        global = true,
        env = "GZ_HOST",
        help = "Override ALL derived service URLs (local dev/stubs only)"
    )]
    pub host: Option<String>,
    #[arg(
        long,
        global = true,
        env = "GZ_TOKEN",
        help = "Cognito ID token for one-off calls (prefer `gz auth login`)"
    )]
    pub token: Option<String>,
    #[arg(
        short,
        long,
        global = true,
        value_enum,
        default_value = "table",
        help = "Output format"
    )]
    pub output: OutputArg,
    #[arg(long, global = true, help = "Shortcut for --output json")]
    pub json: bool,
    #[arg(short, long, global = true, help = "Suppress spinners and hints")]
    pub quiet: bool,
    #[arg(short, long, global = true, action = clap::ArgAction::Count, help = "Verbose output (-v, -vv)")]
    pub verbose: u8,
    #[arg(long, global = true, help = "Max items to fetch and show")]
    pub limit: Option<usize>,
    #[arg(
        long,
        global = true,
        default_value = "60",
        help = "HTTP timeout in seconds"
    )]
    pub timeout: u64,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone)]
pub struct BodyArgs {
    #[arg(
        long,
        value_name = "JSON|@FILE|@-",
        help = "Extra JSON body merged into the request (inline, @file, or @- for stdin)"
    )]
    pub data: Option<String>,
    #[arg(long, value_name = "K=V", help = "Query parameter (repeatable)")]
    pub query: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "Verify config, auth, and endpoint reachability")]
    Doctor {
        #[arg(long, help = "Only check local config; skip network probes")]
        offline: bool,
    },
    #[command(about = "Set client, site, domain (prompts when no flags given)")]
    Configure(ConfigureArgs),
    #[command(about = "Manage config profiles")]
    Profile(ProfileArgs),
    #[command(about = "Sign in, sign out, and check status")]
    Auth(AuthArgs),
    #[command(about = "Lakehouse warehouses and namespaces")]
    Warehouse(WarehouseArgs),
    #[command(about = "SQL sessions, queries, and saved queries")]
    Sql(SqlArgs),
    #[command(about = "Tables: schema, snapshots, and maintenance")]
    Table(TableArgs),
    #[command(about = "Backups and backup schedules")]
    Backup(BackupArgs),
    #[command(about = "Governance: table policies, permissions, lineage")]
    Govern(GovernArgs),
    #[command(about = "Lakehouse tags and search")]
    Tags(TagsArgs),
    #[command(about = "Files and folders on storage connections")]
    Fs(FsArgs),
    #[command(about = "Data connections: schemas, tests, reads (datasets: `gz datasets`)")]
    Data(DataArgs),
    #[command(about = "ETL jobs: submit, status, logs, observability")]
    Jobs(JobsArgs),
    #[command(about = "MLOps notebooks (Jupyter)")]
    Notebooks(NotebooksArgs),
    #[command(about = "MLflow models and experiments")]
    Models(ModelsArgs),
    #[command(about = "Agent builder: agents, runs, approvals")]
    Agents(AgentsArgs),
    #[command(about = "AI chat runs and conversations")]
    Chat(ChatArgs),
    #[command(about = "Log exports and schedules")]
    Logs(LogsArgs),
    #[command(about = "Runtime environments (DuckDB / Spark)")]
    Rte(RteArgs),
    #[command(about = "Site settings")]
    Sites(SitesArgs),
    #[command(about = "Raw escape hatch: call any service endpoint")]
    Request(RequestArgs),
    #[command(about = "Interactive terminal UI")]
    Tui,
    #[command(about = "DE/ML projects and jobs registry")]
    Projects(ProjectsArgs),
    #[command(about = "Designer: datasets, dashboards, visualizations, filters")]
    Designer(DesignerArgs),
    #[command(about = "Workspaces")]
    Workspaces(WorkspacesArgs),
    #[command(about = "Schedules")]
    Schedules(SchedulesArgs),
    #[command(about = "Permissions")]
    Permissions(PermissionsArgs),
    #[command(about = "Connections registry")]
    Connections(ConnectionsArgs),
    #[command(about = "Print shell completions")]
    Completion(CompletionArgs),
    #[command(about = "Update gz to the latest release")]
    Update(UpdateArgs),
    #[command(about = "Admin: users, groups, chat budgets, sites, email, billing")]
    Admin(AdminArgs),
    #[command(about = "Search everything you can access")]
    Search(SearchArgs),
    #[command(
        about = "AI assistant for this folder: reads, edits, and runs with your permission",
        alias = "assist"
    )]
    Code(CodeArgs),
    #[command(
        about = "Connect AI tools (Claude Code, Codex, Cursor, VS Code) to Groundzero's MCP server"
    )]
    Mcp(McpArgs),
    #[command(about = "Run a local Python file on Groundzero compute and stream its logs")]
    Run(RunArgs),
    #[command(about = "Sync a folder of job files with a DE/ML project (push, --pull, --watch)")]
    Sync(SyncArgs),
    #[command(about = "Your account's AI usage: tokens, cost, and budget")]
    Usage(UsageArgs),
    // Hidden: `/v1/flows` is not on the deployed agent builder yet, and teams
    // are what ships. Kept because which model wins is still undecided.
    #[command(
        hide = true,
        about = "Flow Builder flows: define in YAML, validate, submit, run live"
    )]
    Flows(FlowsArgs),
    #[command(about = "What gz may do without asking: saved local rules and chat approvals")]
    Approvals(ApprovalsArgs),
    #[command(about = "Datasets: list them or get one (same as `gz designer datasets`)")]
    Datasets(DatasetsArgs),
    #[command(
        about = "Skills for gz code: list, install (git URL or folder), switch on/off, verify"
    )]
    Skills(SkillsArgs),
    #[command(about = "Plugins: bundles of skills installed together (skills only, nothing runs)")]
    Plugins(PluginsArgs),
    #[command(
        about = "Fix a failed job run with gz code (same as `gz jobs fix`)",
        long_about = FIX_ABOUT
    )]
    Fix(FixArgs),
    #[command(
        about = "What changed in a table between two snapshots: commits, schema, rows",
        long_about = DIFF_ABOUT
    )]
    Diff(DiffArgs),
    #[command(
        about = "Find the snapshot where a table started failing a check",
        long_about = BISECT_ABOUT
    )]
    Bisect(BisectArgs),
    #[command(
        about = "Where a number comes from: dataset, tables, snapshots, the runs that wrote them",
        long_about = EXPLAIN_ABOUT
    )]
    Explain(ExplainArgs),
}

const EXPLAIN_ABOUT: &str = "Where a number on a dashboard comes from.

gz follows a figure back through the platform: the visualization's measure \
and the filters it applies, the dataset's definition, the lakehouse tables \
it reads, the snapshot each table was at, the job run that most likely wrote \
that snapshot (and how sure that is), the run's code version, and the \
tables upstream. With --at it uses the snapshots current at that time and \
lists the commits each input has had since, so you can see whether the \
inputs moved.

It only reads. The platform does not record the run id on table commits, so \
runs are matched by commit time and Spark application; the trace says how.

Examples:
  gz explain dashboard Executive-Working-Capital-Overview
  gz explain dashboard 1114 --viz WC-YearChart --at 2026-09-16
  gz explain viz 1717
  gz explain dataset WC-YeartoQuarter --depth 2
  gz explain sql \"SELECT sum(amount) FROM sales.orders\" --warehouse Lineage
  gz explain dashboard 1114 -o json          the whole trace, for a script";

/// Options every `gz explain` subject takes.
#[derive(Args, Debug, Clone)]
pub struct ExplainOptions {
    #[arg(
        long,
        value_name = "TIME",
        help = "Trace as of a past UTC date or time (2026-09-16, 2026-09-16T08:00): the snapshots current then, and the commits since"
    )]
    pub at: Option<String>,
    #[arg(
        long,
        default_value = "1",
        value_parser = clap::value_parser!(u8).range(1..=10),
        help = "Levels of upstream lineage to follow per table (1-10)"
    )]
    pub depth: u8,
}

#[derive(Args, Debug)]
pub struct ExplainArgs {
    #[command(subcommand)]
    pub target: ExplainTarget,
}

#[derive(Subcommand, Debug)]
pub enum ExplainTarget {
    #[command(about = "Every figure on a dashboard, or one with --viz")]
    Dashboard {
        #[arg(help = "Dashboard id or name")]
        dashboard: String,
        #[arg(long, help = "Only this visualization (id or name)")]
        viz: Option<String>,
        #[command(flatten)]
        options: ExplainOptions,
    },
    #[command(about = "One visualization", alias = "visualization")]
    Viz {
        #[arg(help = "Visualization id or name")]
        visualization: String,
        #[command(flatten)]
        options: ExplainOptions,
    },
    #[command(about = "One dataset")]
    Dataset {
        #[arg(help = "Dataset id or name")]
        dataset: String,
        #[command(flatten)]
        options: ExplainOptions,
    },
    #[command(about = "The tables a SQL query reads (text or @file.sql)")]
    Sql {
        #[arg(help = "The query, or @file.sql")]
        query: String,
        #[arg(
            long,
            help = "Warehouse the query runs on (default: the one gz sql uses)"
        )]
        warehouse: Option<String>,
        #[arg(long, help = "Database for table names written without one")]
        database: Option<String>,
        #[command(flatten)]
        options: ExplainOptions,
    },
}

/// How `gz diff` and `gz bisect` name a point in a table's history.
const REF_HELP: &str = "a branch, a tag, a snapshot id, `current`, `previous`, or a UTC date or time (2026-09-16, 2026-09-16T08:00)";

const DIFF_ABOUT: &str = "What changed in a lakehouse table between two snapshots.

Without --rows gz reads the table's history only: the commits between the two \
snapshots and the record and file counts each one recorded. That is instant \
and starts nothing.

--rows compares the rows themselves on a SQL session (the one `gz sql` uses; \
it is stopped afterwards unless --keep-session): schema changes, rows added \
and removed, and a few sample rows. With --key gz matches rows by those \
columns, so it also tells which rows changed and in which columns.

Examples:
  gz diff sales.orders                         the last commit, from the history
  gz diff sales.orders --rows                  the last commit, row by row
  gz diff sales.orders --from 2026-09-01 --key order_id
  gz diff sales.orders --from release --to audit --rows --sample 20
  gz diff sales.orders --rows -o json          everything, for a script";

#[derive(Args, Debug)]
pub struct DiffArgs {
    #[arg(help = "Table as database.table")]
    pub table: String,
    #[arg(long, value_name = "REF", default_value = "previous", help = format!("The older side: {REF_HELP}"))]
    pub from: String,
    #[arg(
        long,
        value_name = "REF",
        default_value = "current",
        help = "The newer side, named the same way"
    )]
    pub to: String,
    #[arg(long, help = "Compare the rows themselves (runs SQL on a session)")]
    pub rows: bool,
    #[arg(
        long,
        value_name = "COLUMNS",
        value_delimiter = ',',
        help = "Columns that identify a row, to tell changed rows from added and removed ones (implies --rows)"
    )]
    pub key: Vec<String>,
    #[arg(
        long,
        value_name = "N",
        default_value = "5",
        value_parser = clap::value_parser!(u32).range(0..=100),
        help = "Sample rows shown for each kind of change (0 for none)"
    )]
    pub sample: u32,
    #[command(flatten)]
    pub shell: ShellArgs,
}

const BISECT_ABOUT: &str = "Find the snapshot where a lakehouse table started failing a check.

A check is SQL that returns the bad rows: a snapshot fails when the check \
returns any. Give it as a condition (--where \"amount < 0\") or as a whole \
query that reads the table as {table} (--check, or --check @file.sql).

gz tests the newest snapshot (--bad) and the oldest one kept (or --good), then \
halves the commits in between, so a long history takes a handful of queries. \
It reports the first failing snapshot, what that commit changed, and the \
Spark application that wrote it. The queries run on a SQL session (the one \
`gz sql` uses; it is stopped afterwards unless --keep-session).

Examples:
  gz bisect sales.orders --where \"amount < 0\"
  gz bisect sales.orders --check \"SELECT order_id FROM {table} GROUP BY order_id HAVING count(*) > 1\"
  gz bisect sales.orders --where \"country IS NULL\" --good 2026-09-01
  gz bisect sales.orders --check @checks/orders.sql --code     then hand it to gz code";

#[derive(Args, Debug)]
pub struct BisectArgs {
    #[arg(help = "Table as database.table")]
    pub table: String,
    #[arg(
        long = "where",
        value_name = "CONDITION",
        conflicts_with = "check",
        required_unless_present = "check",
        help = "Rows matching this condition are bad"
    )]
    pub condition: Option<String>,
    #[arg(
        long,
        value_name = "SQL|@FILE",
        help = "A query that returns the bad rows, reading the table as {table}"
    )]
    pub check: Option<String>,
    #[arg(long, value_name = "REF", help = format!("A snapshot known to pass (default: the oldest one kept): {REF_HELP}"))]
    pub good: Option<String>,
    #[arg(
        long,
        value_name = "REF",
        default_value = "current",
        help = "A snapshot known to fail, named the same way"
    )]
    pub bad: String,
    #[arg(long, help = "Open gz code with what was found as a prepared message")]
    pub code: bool,
    #[command(flatten)]
    pub shell: ShellArgs,
}

const FIX_ABOUT: &str = "Fix a failed job run with gz code.

gz finds the job's newest failed run (or --run), reads its status, log \
excerpt and config, puts the job's code in the folder (like `gz sync --pull`, \
asking before it replaces local edits), and opens gz code there with a \
prepared message in its input box: find the root cause, make the smallest \
fix, show the diff, then — with your approval — `gz sync` the file and rerun \
the job. Nothing is sent until you press Enter, and gz code asks before \
every change and command.

A run that is still going is never touched: gz reads only the logs of runs \
that are over.

Examples:
  gz fix clean-orders                    newest failed run, project from this folder
  gz jobs fix 1006 --project 1000 --dir fixes
  gz fix clean-orders --run 0123…cdef --compute 1003 --rte gz-rte-spark-pytorch:1.0.0
  gz fix clean-orders --print | less     the evidence and the prompt only";

#[derive(Args, Debug)]
pub struct FixArgs {
    #[arg(help = "Job id or name")]
    pub job: String,
    #[arg(
        long,
        help = "Project id (default: the folder's `gz sync` project, else the project that has the job)"
    )]
    pub project: Option<i64>,
    #[arg(
        long,
        value_name = "RUN_ID",
        help = "Fix this run instead of the newest failed one"
    )]
    pub run: Option<String>,
    #[arg(
        long,
        default_value = ".",
        help = "Folder for the job file and gz code (created if missing)"
    )]
    pub dir: String,
    #[arg(
        long,
        help = "Print the evidence and the prompt instead of starting gz code"
    )]
    pub print: bool,
    #[arg(long, help = "Compute size id for the rerun (see `gz rte compute`)")]
    pub compute: Option<i64>,
    #[arg(long, help = "Runtime image for the rerun (see `gz rte list`)")]
    pub rte: Option<String>,
}

#[derive(Args, Debug)]
pub struct SkillsArgs {
    #[command(subcommand)]
    pub action: SkillsAction,
}

/// How an install is confirmed.
#[derive(Args, Debug, Clone, Copy)]
pub struct ConsentArgs {
    #[arg(long, short = 'y', help = "Don't ask (needed when not interactive)")]
    pub yes: bool,
    #[arg(
        long,
        help = "With --yes, install even with MEDIUM warnings (HIGH findings always refuse)"
    )]
    pub accept_warnings: bool,
}

#[derive(Subcommand, Debug)]
pub enum SkillsAction {
    #[command(about = "Every skill with its status, source and bundle")]
    List,
    #[command(about = "One skill: description, files, source, commit and findings")]
    Show { name: String },
    #[command(
        about = "Install from an https:// git URL (#subdir, @ref), github:owner/repo[/subdir][@ref], or a local folder"
    )]
    Install {
        source: String,
        #[arg(long, value_delimiter = ',', help = "Install only these skills (a,b)")]
        skills: Option<Vec<String>>,
        #[arg(long, help = "Replace skills that are already installed")]
        replace: bool,
        #[command(flatten)]
        consent: ConsentArgs,
    },
    #[command(about = "Reinstall installed skills from their source (all, or one)")]
    Update {
        name: Option<String>,
        #[command(flatten)]
        consent: ConsentArgs,
    },
    #[command(about = "Delete a skill from the personal skills folder")]
    Remove {
        name: String,
        #[arg(long, short = 'y', help = "Don't ask")]
        yes: bool,
    },
    #[command(about = "Switch a skill on")]
    Enable { name: String },
    #[command(about = "Switch a skill off")]
    Disable { name: String },
    #[command(
        about = "Check installed skills against the lockfile; --accept trusts a changed one"
    )]
    Verify {
        #[arg(
            long,
            value_name = "NAME",
            help = "Re-scan a changed skill and trust it as it is now"
        )]
        accept: Option<String>,
        #[arg(long, short = 'y', help = "Don't ask")]
        yes: bool,
    },
    #[command(about = "Extra skill folders (each a parent of skill folders)")]
    Folders {
        #[command(subcommand)]
        action: Option<SkillFoldersAction>,
    },
    #[command(about = "The site's team skills: list them or sync them now")]
    Team {
        #[command(subcommand)]
        action: TeamSkillsAction,
    },
    #[command(about = "Turn on a team skill with MEDIUM warnings, for its current version only")]
    Trust {
        name: String,
        #[arg(long, short = 'y', help = "Don't ask")]
        yes: bool,
    },
    #[command(
        about = "Publish a personal skill as a team skill for everyone on the site (site admins)"
    )]
    Publish {
        name: String,
        #[command(flatten)]
        consent: ConsentArgs,
    },
    #[command(about = "Remove a team skill for everyone on the site (site admins)")]
    Unpublish {
        name: String,
        #[arg(long, short = 'y', help = "Don't ask")]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum TeamSkillsAction {
    #[command(about = "Team skills on the server: who updated them, when, and their local status")]
    List,
    #[command(about = "Download new and changed team skills now and remove deleted ones")]
    Sync,
}

#[derive(Subcommand, Debug)]
pub enum SkillFoldersAction {
    #[command(about = "Register a folder of skills")]
    Add { path: std::path::PathBuf },
    #[command(about = "Unregister a folder (its skills stay on disk)")]
    Remove { path: std::path::PathBuf },
}

#[derive(Args, Debug)]
pub struct PluginsArgs {
    #[command(subcommand)]
    pub action: PluginsAction,
}

#[derive(Subcommand, Debug)]
pub enum PluginsAction {
    #[command(about = "Installed bundles and their skills")]
    List,
    #[command(about = "Install every skill of a bundle (same sources as `gz skills install`)")]
    Install {
        source: String,
        #[arg(long, help = "Replace skills that are already installed")]
        replace: bool,
        #[command(flatten)]
        consent: ConsentArgs,
    },
    #[command(about = "Remove every skill a bundle installed")]
    Remove {
        bundle: String,
        #[arg(long, short = 'y', help = "Don't ask")]
        yes: bool,
    },
}

#[derive(Args, Debug)]
pub struct ApprovalsArgs {
    #[command(subcommand)]
    pub action: ApprovalsAction,
}

#[derive(Subcommand, Debug)]
pub enum ApprovalsAction {
    #[command(about = "List saved local rules and chat approvals, with ids to revoke")]
    List {
        #[arg(
            long,
            help = "Also list this conversation's \"for this chat\" approvals"
        )]
        conversation: Option<String>,
        #[arg(long, help = "Local rules for every folder, not just this one")]
        all: bool,
    },
    #[command(about = "Revoke one entry by its id from `gz approvals list` (e.g. L1, C1, A1)")]
    Revoke {
        id: String,
        #[arg(long, help = "Conversation the C ids belong to (same as for list)")]
        conversation: Option<String>,
        #[arg(long, help = "Ids refer to the --all listing")]
        all: bool,
    },
}

#[derive(Args, Debug)]
pub struct FlowsArgs {
    #[command(subcommand)]
    pub action: FlowsAction,
}

#[derive(Subcommand, Debug)]
pub enum FlowsAction {
    #[command(about = "List flows you can see")]
    List {
        #[arg(long, help = "Only flows in this DE/ML project")]
        project: Option<i64>,
        #[arg(long, help = "Match names and descriptions")]
        search: Option<String>,
        #[arg(long, help = "Approval status, e.g. approved or pending")]
        status: Option<String>,
    },
    #[command(about = "Show a flow's draft definition")]
    Get { id: String },
    #[command(about = "Step and trigger types a flow can use")]
    NodeTypes,
    #[command(about = "Write a commented example flow to start from")]
    Init {
        #[arg(default_value = "flow.yaml")]
        path: String,
        #[arg(long, help = "Overwrite the file if it exists")]
        force: bool,
    },
    #[command(about = "Create a flow from a YAML or JSON file")]
    Create {
        #[arg(short, long, help = "Flow definition (.yaml, .yml, or .json)")]
        file: String,
        #[arg(long, help = "DE/ML project id (overrides projectId in the file)")]
        project: Option<i64>,
    },
    #[command(about = "Replace a flow's draft from a YAML or JSON file")]
    Update {
        id: String,
        #[arg(short, long)]
        file: String,
    },
    #[command(about = "Delete a flow and its job")]
    Delete {
        id: String,
        #[arg(long, short = 'y', help = "Don't ask for confirmation")]
        yes: bool,
    },
    #[command(about = "Check a flow (by id) or a local file and list every problem")]
    Validate {
        #[arg(help = "Flow id, or a .yaml/.json file to check before creating it")]
        target: String,
    },
    #[command(about = "Submitted versions and their approval decisions")]
    Versions { id: String },
    #[command(about = "Submit the current draft for approval")]
    Submit {
        id: String,
        #[arg(long, help = "Note for the reviewer")]
        comment: Option<String>,
    },
    #[command(about = "Approve (default) or reject a submitted version")]
    Decide {
        id: String,
        #[arg(help = "Approval id (from `gz flows versions`)")]
        approval: i64,
        #[arg(long, help = "Reject instead of approve")]
        reject: bool,
        #[arg(long, help = "Reason; required when rejecting")]
        comment: Option<String>,
    },
    #[command(about = "Run a flow and follow it live; approval steps ask here")]
    Run {
        id: String,
        #[arg(
            long = "param",
            value_name = "NAME=VALUE",
            help = "Run parameter (repeatable; values may be JSON)"
        )]
        params: Vec<String>,
        #[arg(
            long = "params",
            value_name = "JSON|@FILE",
            help = "All parameters as a JSON object"
        )]
        params_file: Option<String>,
        #[arg(long, help = "Run the latest draft instead of the approved version")]
        latest: bool,
        #[arg(long, help = "Start the run and print its id without following it")]
        background: bool,
    },
    #[command(about = "Answer a paused run's approvals and continue it")]
    Resume {
        id: String,
        run: String,
        #[arg(
            long,
            value_name = "INTERRUPT_ID",
            help = "Approve this pending approval (repeatable)"
        )]
        approve: Vec<String>,
        #[arg(
            long,
            value_name = "INTERRUPT_ID",
            help = "Reject this pending approval (repeatable)"
        )]
        reject: Vec<String>,
        #[arg(long, help = "Note sent with rejections")]
        comment: Option<String>,
        #[arg(long, help = "Don't follow the continued run")]
        background: bool,
    },
    #[command(about = "A flow's runs")]
    Runs { id: String },
    #[command(about = "One run: each step's status, time, tokens, and cost")]
    Show { id: String, run: String },
    #[command(about = "Cancel a running or paused run")]
    Cancel { id: String, run: String },
    #[command(about = "A flow's triggers and schedules")]
    Triggers { id: String },
}

#[derive(Args, Debug)]
pub struct McpArgs {
    #[command(subcommand)]
    pub action: McpAction,
}

#[derive(Subcommand, Debug)]
pub enum McpAction {
    #[command(about = "Add the groundzero MCP server to an AI tool's config")]
    Setup {
        #[arg(value_enum)]
        client: McpClient,
    },
    #[command(about = "Print this site's MCP server URL")]
    Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum McpClient {
    /// Claude Code (`claude mcp add`, or a project .mcp.json)
    Claude,
    /// Codex (`codex mcp add`, or ~/.codex/config.toml)
    Codex,
    /// Cursor (~/.cursor/mcp.json)
    Cursor,
    /// VS Code (.vscode/mcp.json in this folder)
    Vscode,
    /// Print every client's snippet without writing anything
    Print,
}

/// Where and how a job runs: shared by `gz jobs run` and `gz run`.
#[derive(Args, Debug, Clone)]
pub struct RunChoiceArgs {
    #[arg(long, help = "Compute size id (see `gz rte compute`)")]
    pub compute: i64,
    #[arg(
        long,
        help = "Runtime image, e.g. gz-rte-spark-pytorch:1.0.0 (see `gz rte list`)"
    )]
    pub rte: String,
    #[arg(long, help = "Override the job's minimum workers")]
    pub workers: Option<u32>,
    #[arg(long, help = "Override the job's GPU size")]
    pub gpu: Option<u32>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(help = "Local Python file to run")]
    pub file: String,
    #[arg(long, help = "Project id (see `gz projects list`)")]
    pub project: i64,
    #[arg(
        long,
        help = "Job id or name to run it as (default: a scratch job gz-run-<file>)"
    )]
    pub job: Option<String>,
    #[command(flatten)]
    pub choice: RunChoiceArgs,
    #[arg(short, long, help = "Stream the run's logs until it finishes")]
    pub follow: bool,
    #[arg(
        long,
        short = 'y',
        help = "Don't ask before creating a job or replacing its code"
    )]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    #[arg(default_value = ".", help = "Folder of job files (Python)")]
    pub dir: String,
    #[arg(long, help = "Project id (see `gz projects list`)")]
    pub project: i64,
    #[arg(long, help = "Download the project's job code into the folder instead")]
    pub pull: bool,
    #[arg(long, help = "Show what would change without changing anything")]
    pub dry_run: bool,
    #[arg(long, help = "Keep syncing whenever a file is saved (Ctrl+C to stop)")]
    pub watch: bool,
    #[arg(
        long,
        help = "Overwrite even when the other side changed since the last sync"
    )]
    pub force: bool,
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with = "watch",
        help = "Sync only this file (path inside the folder, e.g. etl/clean_orders.py); repeatable"
    )]
    pub only: Vec<String>,
}

#[derive(Args, Debug)]
pub struct CodeArgs {
    #[arg(
        help = "Ask once and exit (or @file); omit for an interactive session. With -o json|yaml \
                and no prompt, piped input is the prompt"
    )]
    pub prompt: Option<String>,
    #[arg(long, help = "Allow reading any file in this folder without asking")]
    pub yes_reads: bool,
    #[arg(
        long,
        help = "Auto mode: a review runs routine actions and blocks risky ones (one-shot runs too)"
    )]
    pub auto: bool,
    #[arg(
        long,
        short,
        help = "Continue this conversation (id from `gz chat list`)"
    )]
    pub conversation: Option<String>,
    #[arg(
        long = "continue",
        conflicts_with_all = ["conversation", "resume"],
        help = "Continue the last conversation started in this folder"
    )]
    pub continue_last: bool,
    #[arg(
        long,
        conflicts_with = "conversation",
        help = "Pick an earlier conversation to continue"
    )]
    pub resume: bool,
    #[arg(
        long,
        help = "Model: a Groundzero mode (e.g. opus) or one of your LLM connections; remembered"
    )]
    pub model: Option<String>,
    #[arg(
        long,
        help = "Reasoning effort for the model (low, medium, high, …); remembered"
    )]
    pub effort: Option<String>,
    #[arg(
        long,
        value_name = "TOKENS",
        help = "Summarize older messages once the conversation reaches this many tokens \
                (100k steps: 200k, 300k, … or `default`); remembered"
    )]
    pub auto_compact: Option<String>,
}

// ---------- configure / profile ----------

#[derive(Args, Debug)]
pub struct ConfigureArgs {
    #[arg(long, help = "Client name, e.g. dev (service URLs derive from this)")]
    pub client: Option<String>,
    #[arg(long, help = "Site / subdomain, e.g. acme")]
    pub site: Option<String>,
    #[arg(long, help = "Domain, e.g. groundzero.cloud")]
    pub domain: Option<String>,
    #[arg(long, help = "Override ALL derived URLs (local dev/stubs only)")]
    pub host: Option<String>,
    #[arg(long, help = "Override the derived OAuth server host")]
    pub auth_host: Option<String>,
    #[arg(
        long,
        value_name = "URL",
        help = "The web app's address when it isn't https://{site}.{client}.{domain}, e.g. a local dev server (http://dev.dev.local.localhost:3000); \"\" clears it"
    )]
    pub web_url: Option<String>,
    #[arg(
        long,
        value_name = "SERVICE=URL",
        help = "Per-service base URL for gateway services without derived URLs (repeatable)"
    )]
    pub service: Vec<String>,
    #[arg(long, help = "Clear all per-service URLs")]
    pub clear_services: bool,
    #[arg(long, help = "Only print what would change")]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct ProfileArgs {
    #[command(subcommand)]
    pub action: ProfileAction,
}

#[derive(Subcommand, Debug)]
pub enum ProfileAction {
    #[command(about = "List profiles")]
    List,
    #[command(about = "Show the active profile (token redacted)")]
    Show,
    #[command(about = "Switch the active profile")]
    Use { name: String },
    #[command(about = "Delete a profile")]
    Delete { name: String },
}

// ---------- auth ----------

#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub action: AuthAction,
}

#[derive(Subcommand, Debug)]
pub enum AuthAction {
    #[command(about = "Sign in with your Groundzero username, password, and MFA code")]
    Login {
        #[arg(
            long,
            help = "Store a platform API key (sk_…) for automation instead; read from a prompt or stdin"
        )]
        api_key: bool,
        #[arg(long, help = "Username or email (prompted if missing)")]
        username: Option<String>,
        #[arg(long, help = "Read the password from stdin instead of prompting")]
        password_stdin: bool,
        #[arg(
            long,
            value_name = "CODE",
            help = "Authenticator code (prompted if needed)"
        )]
        mfa_code: Option<String>,
    },
    #[command(about = "Show sign-in status (token redacted)")]
    Status,
    #[command(about = "Sign out: revoke the session and clear it locally")]
    Logout {
        #[arg(long, help = "Clear locally without revoking server-side")]
        local_only: bool,
    },
    #[command(about = "Get an MCP token for AI tools (browser sign-in); prints it")]
    McpToken {
        #[arg(long, help = "Request write scope as well as read")]
        write: bool,
        #[arg(long, help = "Print the authorize URL instead of opening a browser")]
        no_open: bool,
        #[arg(
            long,
            default_value = "0",
            help = "Loopback callback port (0 = random)"
        )]
        port: u16,
    },
}

// ---------- warehouse ----------

#[derive(Args, Debug)]
pub struct WarehouseArgs {
    #[command(subcommand)]
    pub action: WarehouseAction,
}

#[derive(Subcommand, Debug)]
pub enum WarehouseAction {
    #[command(about = "List warehouses")]
    List,
    #[command(about = "Create a warehouse")]
    Create {
        #[arg(long, help = "Warehouse name")]
        name: String,
        #[arg(long, help = "S3 bucket that stores the warehouse")]
        bucket: String,
        #[arg(long)]
        description: Option<String>,
    },
    #[command(about = "Delete a warehouse by id")]
    Delete { id: String },
    #[command(about = "Update a warehouse description")]
    Describe {
        id: String,
        #[arg(long)]
        description: String,
    },
    #[command(about = "List namespaces in a warehouse")]
    Namespaces { warehouse: String },
    #[command(about = "Show client name")]
    Client,
    #[command(about = "Show account id")]
    Account,
}

// ---------- sql ----------

#[derive(Args, Debug)]
pub struct UsageArgs {
    #[command(subcommand)]
    pub action: Option<UsageAction>,
}

#[derive(Subcommand, Debug)]
pub enum UsageAction {
    #[command(
        about = "Your Groundzero budget, gz code's spend on this machine today and this month, and your caps"
    )]
    Budget,
}

#[derive(Args, Debug)]
#[command(about = "SQL on the lakehouse: an interactive shell with no subcommand")]
pub struct SqlArgs {
    #[command(subcommand)]
    pub action: Option<SqlAction>,
    #[command(flatten)]
    pub shell: ShellArgs,
}

/// Where `gz sql`'s shell and scripts run.
#[derive(Args, Debug, Clone, Default)]
pub struct ShellArgs {
    #[arg(long, help = "Warehouse name or id (remembered for next time)")]
    pub warehouse: Option<String>,
    #[arg(long, help = "Compute size name or id, e.g. small (remembered)")]
    pub compute: Option<String>,
    #[arg(
        long,
        help = "Leave the session running on exit; the next gz sql reuses it"
    )]
    pub keep_session: bool,
}

#[derive(Subcommand, Debug)]
pub enum SqlAction {
    #[command(about = "Interactive SQL shell (same as bare `gz sql` in a terminal)")]
    Shell(ShellArgs),
    #[command(about = "Run every statement in a .sql file and print the results")]
    RunFile {
        file: String,
        #[command(flatten)]
        shell: ShellArgs,
    },
    #[command(about = "Run a SQL query (connectionConfig via --data)")]
    Run {
        #[arg(long, help = "SQL text (or @file.sql)")]
        sql: Option<String>,
        #[arg(long, help = "Wait for output")]
        wait: bool,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Submit a query on a running session")]
    Submit {
        #[arg(long, help = "SQL text (or @file.sql)")]
        sql: String,
        #[arg(long, help = "Session id (from `gz sql start-session`)")]
        session: String,
        #[arg(long, help = "Warehouse name the query runs against")]
        warehouse: String,
        #[arg(long, help = "Rows per result page")]
        page_size: Option<u32>,
        #[arg(long = "async", help = "Return immediately; poll with `gz sql status`")]
        background: bool,
    },
    #[command(about = "Status of a submitted query")]
    Status { session: String, execution: String },
    #[command(about = "Cancel a running query")]
    Cancel { session: String, execution: String },
    #[command(about = "Fetch a page of a finished query's results")]
    Results {
        session: String,
        execution: String,
        #[arg(long, help = "Warehouse name the query ran against")]
        warehouse: String,
        #[arg(long, default_value = "1")]
        page: u32,
        #[arg(long, default_value = "100")]
        page_size: u32,
    },
    #[command(about = "Session state: Starting, Running, Stopping, Stopped")]
    SessionState { session: String },
    #[command(about = "Export a finished query's full result (download links, valid 1h)")]
    Export {
        session: String,
        execution: String,
        #[arg(long, help = "Warehouse name the query ran against")]
        warehouse: String,
        #[arg(long, value_parser = ["csv", "parquet"], default_value = "csv")]
        format: String,
        #[arg(long, value_name = "DIR", help = "Download the files into this folder")]
        out: Option<String>,
    },
    #[command(about = "Show where each statement of a script starts and ends")]
    Split {
        #[arg(help = "SQL text (or @file.sql)")]
        sql: String,
    },
    #[command(about = "Show query execution time")]
    QueryTime {
        session_id: String,
        execution_id: String,
    },
    #[command(about = "Start a lakehouse session")]
    StartSession {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Stop the session `gz sql` left running (or the one given)")]
    Stop {
        #[arg(help = "Session id (default: the one gz sql kept for this profile)")]
        session: Option<String>,
    },
    #[command(about = "Stop a lakehouse session (raw body)")]
    StopSession {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Check session status")]
    SessionStatus { session_id: String },
    #[command(about = "Shut down the session backend")]
    Shutdown {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Authorize a query or table action")]
    Authorize {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Execute a v1 SQL statement")]
    Statement {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Manage saved queries")]
    Saved {
        #[command(subcommand)]
        action: SavedQueryAction,
    },
    #[command(about = "Test a lakehouse connection")]
    TestConnection {
        #[command(flatten)]
        body: BodyArgs,
    },
}

#[derive(Subcommand, Debug)]
pub enum SavedQueryAction {
    #[command(about = "List saved queries")]
    List,
    #[command(about = "Get a saved query by id")]
    Get { id: i64 },
    #[command(about = "Save a query")]
    Create {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a saved query (name, warehouseId, warehouseName via --data)")]
    Update {
        id: i64,
        #[arg(
            long,
            help = "New SQL text (or @file.sql); required so the query isn't blanked"
        )]
        sql: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete a saved query")]
    Delete { id: i64 },
    #[command(about = "Rename a saved query")]
    Rename {
        id: i64,
        #[arg(long)]
        name: String,
    },
    #[command(about = "List saved queries for a warehouse")]
    ByWarehouse { id: i64 },
}

// ---------- table ----------

#[derive(Args, Debug)]
pub struct TableArgs {
    #[command(subcommand)]
    pub action: TableAction,
}

#[derive(Subcommand, Debug)]
pub enum TableAction {
    #[command(about = "Show schema of every namespace in a warehouse")]
    Schemas { warehouse: String },
    #[command(about = "Show schema of one namespace")]
    Namespace {
        warehouse: String,
        namespace: String,
    },
    #[command(about = "Describe one table")]
    Describe {
        warehouse: String,
        namespace: String,
        table: String,
    },
    #[command(about = "Show table properties")]
    Props {
        warehouse: String,
        namespace: String,
        table: String,
    },
    #[command(about = "Sample records from a table")]
    Sample {
        warehouse: String,
        namespace: String,
        table: String,
    },
    #[command(about = "Count tables in a namespace")]
    Count {
        warehouse: String,
        namespace: String,
    },
    #[command(about = "Database size on disk")]
    DbSize {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Export a database schema")]
    ExportSchema {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Import a database schema")]
    ImportSchema {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Connection schema for a named connection")]
    ConnectionSchema {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Set table description")]
    SetDescription {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Change table settings")]
    Settings {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Manage compaction")]
    Compact {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Configure storage optimization")]
    Optimize {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Expire old snapshots")]
    ExpireSnapshots {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Commit a catalog mutation")]
    Commit {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete a database (destructive)")]
    DeleteDb {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Audit logs for a warehouse")]
    AuditLogs {
        warehouse: String,
        #[arg(long, help = "Continue from a cursor returned earlier")]
        cursor: Option<String>,
        #[arg(long)]
        lookback_days: Option<u32>,
    },
    #[command(about = "Prune audit logs (defaults: older than 30 days, keep last 20)")]
    MaintainAuditLogs {
        warehouse: String,
        #[arg(long)]
        older_than_days: Option<u32>,
        #[arg(long)]
        retain_last: Option<u32>,
    },
    #[command(about = "Issue short-lived Iceberg credentials")]
    Credentials {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "A table's snapshots, newest first, with its branches and tags")]
    Versions(VersionsArgs),
}

#[derive(Args, Debug)]
pub struct VersionsArgs {
    #[arg(help = "Table as database.table")]
    pub table: String,
    #[arg(
        long,
        help = "Warehouse name or id (default: the one `gz sql` remembers)"
    )]
    pub warehouse: Option<String>,
}

// ---------- backup ----------

#[derive(Args, Debug)]
pub struct BackupArgs {
    #[command(subcommand)]
    pub action: BackupAction,
}

#[derive(Subcommand, Debug)]
pub enum BackupAction {
    #[command(about = "Back up a database now")]
    Create {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Restore a database from backup")]
    Restore {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Export a dataset")]
    Export {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List backups for a warehouse")]
    List { warehouse: String },
    #[command(about = "Delete a backup")]
    Delete { name: String },
    #[command(about = "Backup status for a warehouse")]
    Status { warehouse: String },
    #[command(about = "All backup statuses for a warehouse")]
    Statuses { warehouse: String },
    #[command(about = "Update a backup status record")]
    UpdateStatus {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Details of one backup")]
    Properties {
        warehouse: String,
        database: String,
        backup: String,
    },
    #[command(about = "Backup status by backup name")]
    StatusByName {
        #[arg(long)]
        backup_name: String,
    },
    #[command(about = "Mark stale backup statuses")]
    MarkStale {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List backup schedules")]
    Schedules {
        warehouse: String,
        database: Option<String>,
        #[arg(long, help = "Only schedules in this status")]
        status: Option<String>,
    },
    #[command(about = "Create a backup schedule")]
    Schedule {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete a backup schedule")]
    Unschedule {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Pause a schedule")]
    Pause {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Resume a schedule")]
    Resume {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Trigger a scheduled backup now")]
    Start {
        #[command(flatten)]
        body: BodyArgs,
    },
}

// ---------- govern ----------

#[derive(Args, Debug)]
pub struct GovernArgs {
    #[command(subcommand)]
    pub action: GovernAction,
}

/// Identifies one lakehouse table.
#[derive(Args, Debug, Clone)]
pub struct TableRef {
    #[arg(long)]
    pub warehouse: String,
    #[arg(long)]
    pub database: String,
    #[arg(long)]
    pub table: String,
}

#[derive(Subcommand, Debug)]
pub enum GovernAction {
    #[command(about = "Get the table data policy")]
    GetPolicy {
        #[command(flatten)]
        table: TableRef,
    },
    #[command(about = "Save the table data policy")]
    SetPolicy {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Disable the table data policy")]
    DeletePolicy {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Preview a policy against a user context")]
    PreviewPolicy {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Policy change history")]
    PolicyHistory {
        #[command(flatten)]
        table: TableRef,
    },
    #[command(about = "Roll back a policy version")]
    RollbackPolicy {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Get table permissions")]
    GetPermissions {
        #[command(flatten)]
        table: TableRef,
    },
    #[command(about = "Save table permissions")]
    SetPermissions {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Preview table permissions")]
    PreviewPermissions {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Table lineage graph")]
    Lineage {
        #[command(flatten)]
        table: TableRef,
        #[arg(long, value_parser = ["upstream", "downstream"], default_value = "upstream")]
        direction: String,
        #[arg(long, help = "Hops to follow (1-10)")]
        depth: Option<u32>,
    },
    #[command(about = "Tables whose policy blocks raw external access")]
    GovernedTables { warehouse: String },
}

// ---------- tags ----------

#[derive(Args, Debug)]
pub struct TagsArgs {
    #[command(subcommand)]
    pub action: TagsAction,
}

#[derive(Subcommand, Debug)]
pub enum TagsAction {
    #[command(about = "Column inventory for tagging")]
    Inventory {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List tags on a warehouse")]
    List { warehouse: String },
    #[command(about = "Tag a warehouse")]
    Tag {
        warehouse: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Remove a warehouse tag")]
    Untag { warehouse: String, key: String },
    #[command(about = "List tags on a table")]
    TableList {
        warehouse: String,
        namespace: String,
        table: String,
    },
    #[command(about = "Tag a table")]
    TableTag {
        warehouse: String,
        namespace: String,
        table: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Remove a table tag")]
    TableUntag {
        warehouse: String,
        namespace: String,
        table: String,
        key: String,
    },
    #[command(about = "List tags on a database")]
    DbList { warehouse: String, database: String },
    #[command(about = "Tag a database")]
    DbTag {
        warehouse: String,
        database: String,
        #[arg(
            long = "tag",
            value_name = "KEY=VALUE",
            required = true,
            help = "Repeatable"
        )]
        tags: Vec<String>,
    },
    #[command(about = "Remove a database tag")]
    DbUntag {
        warehouse: String,
        database: String,
        key: String,
    },
    #[command(about = "Tags on one column (columnId from `gz table describe`)")]
    ColumnList {
        warehouse: String,
        namespace: String,
        table: String,
        column_id: u64,
    },
    #[command(about = "Replace all tags on one column")]
    ColumnSet {
        warehouse: String,
        namespace: String,
        table: String,
        column_id: u64,
        #[arg(
            long = "tag",
            value_name = "KEY=VALUE",
            help = "Repeatable; none clears the tags"
        )]
        tags: Vec<String>,
        #[arg(
            long,
            help = "Version you read (default: read it now); guards against lost updates"
        )]
        version: Option<u64>,
    },
    #[command(about = "Reindex warehouse tags")]
    Reindex { warehouse: String },
    #[command(about = "Find tables by tag")]
    Search {
        warehouse: String,
        #[arg(long)]
        tag_key: String,
        #[arg(long)]
        tag_value: Option<String>,
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        offset: Option<u32>,
    },
    #[command(about = "Get table retention")]
    Retention {
        warehouse: String,
        namespace: String,
        table: String,
    },
    #[command(about = "Set table retention")]
    SetRetention {
        warehouse: String,
        namespace: String,
        table: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Clear table retention")]
    ClearRetention {
        warehouse: String,
        namespace: String,
        table: String,
    },
}

// ---------- fs ----------

#[derive(Args, Debug)]
pub struct FsArgs {
    #[command(subcommand)]
    pub action: FsAction,
}

#[derive(Subcommand, Debug)]
pub enum FsAction {
    #[command(about = "List storage connections you can browse")]
    Connections,
    #[command(about = "List files under a prefix")]
    Ls {
        #[arg(long)]
        connection: String,
        #[arg(long, default_value = "")]
        prefix: String,
    },
    #[command(about = "List folders under a prefix")]
    Lsd {
        #[arg(long)]
        connection: String,
        #[arg(long, default_value = "")]
        prefix: String,
    },
    #[command(about = "List every file in a connection")]
    ListAll {
        #[arg(long)]
        connection: String,
    },
    #[command(about = "Download a file")]
    Get {
        #[arg(long)]
        connection: String,
        #[arg(long, help = "File key, e.g. raw/2024/orders.csv")]
        path: String,
        #[arg(long, help = "Local destination (defaults to the file name)")]
        out: Option<String>,
    },
    #[command(about = "Upload a file into a folder (keeps the local file name)")]
    Put {
        #[arg(long)]
        connection: String,
        #[arg(long, default_value = "", help = "Destination folder, e.g. raw/2024")]
        dir: String,
        #[arg(help = "Local file to upload")]
        file: String,
    },
    #[command(about = "Create a folder")]
    Mkdir {
        #[arg(long)]
        connection: String,
        #[arg(long, help = "Folder path, e.g. raw/2024")]
        path: String,
    },
    #[command(about = "Delete files")]
    Rm {
        #[arg(long)]
        connection: String,
        #[arg(required = true, help = "File keys to delete")]
        keys: Vec<String>,
    },
    #[command(about = "Delete folders and everything in them")]
    Rmdir {
        #[arg(long)]
        connection: String,
        #[arg(required = true, help = "Folder paths to delete")]
        keys: Vec<String>,
    },
    #[command(about = "Custom RTE build status")]
    RteStatus,
    #[command(about = "Upload a custom RTE artifact")]
    RtePut {
        #[arg(help = "Local file to upload")]
        file: String,
        #[arg(long, help = "Base image repo, e.g. gz-rte-duckdb-sklearn")]
        base_repo: String,
        #[arg(long, help = "Tag for the new image")]
        tag: String,
    },
}

// ---------- data ----------

#[derive(Args, Debug)]
pub struct DataArgs {
    #[command(subcommand)]
    pub action: DataAction,
}

#[derive(Subcommand, Debug)]
pub enum DataAction {
    #[command(about = "Get a connection schema")]
    Schema {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Get an Iceberg connection schema")]
    IcebergSchema {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Test a connection")]
    Test {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Read data")]
    Read {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Read an execution result")]
    Execution {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Policy fingerprint for a read")]
    Fingerprint {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "OData read")]
    OData {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "OData metadata")]
    ODataMeta {
        #[command(flatten)]
        body: BodyArgs,
    },
}

// ---------- jobs ----------

#[derive(Args, Debug)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub action: JobsAction,
}

#[derive(Subcommand, Debug)]
pub enum JobsAction {
    #[command(about = "Submit an ETL job (full spec via --data); works with an API key")]
    Submit {
        #[command(flatten)]
        body: BodyArgs,
        #[arg(
            short,
            long,
            help = "Stream logs until the run ends; exit non-zero if it fails"
        )]
        follow: bool,
    },
    #[command(about = "Run a saved job (like Run in the web app's job editor)")]
    Run {
        project: i64,
        job: i64,
        #[command(flatten)]
        choice: RunChoiceArgs,
        #[arg(short, long, help = "Stream the run's logs until it finishes")]
        follow: bool,
        #[arg(
            long,
            conflicts_with = "follow",
            help = "Print the submit spec instead of running it (for `jobs submit --data` in CI)"
        )]
        spec_only: bool,
    },
    #[command(about = "Abort a running job session")]
    Abort {
        #[arg(long, help = "Session id of the run")]
        session: String,
        #[arg(long, help = "Execution (job identifier), if known")]
        execution: Option<String>,
        #[arg(long)]
        job_id: Option<i64>,
    },
    #[command(about = "Job status by job identifier")]
    Status {
        #[arg(long)]
        job: String,
    },
    #[command(about = "Show job logs (--follow streams until the run ends)")]
    Logs {
        #[arg(long, help = "Job identifier (32 hex chars)")]
        job: String,
        #[arg(long, help = "Resume from a cursor returned earlier")]
        cursor: Option<String>,
        #[arg(short, long, help = "Keep polling until the run finishes")]
        follow: bool,
    },
    #[command(about = "The active run of a job, if any")]
    CurrentRun { job_id: i64 },
    #[command(about = "Fetch EKS pod logs for a job")]
    EksLogs {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Submit a scheduled job")]
    Schedule {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Read ETL data")]
    Read {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Consumption metrics")]
    Metrics {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Consumption metrics for one execution")]
    ExecutionMetrics { execution: String },
    #[command(about = "Project-level metric aggregates")]
    ProjectMetrics,
    #[command(about = "Insights for a job")]
    Insights { job_id: i64 },
    #[command(about = "Collect observability for an execution")]
    Collect {
        execution: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Lineage health")]
    LineageHealth,
    #[command(about = "Lineage for a job")]
    LineageJob { job_id: i64 },
    #[command(about = "Lineage for a dataset")]
    LineageDataset { dataset_id: i64 },
    #[command(about = "Lineage assets of a connection")]
    LineageAssets {
        #[arg(long, help = "Connection id")]
        connection: i64,
    },
    #[command(about = "Explore lineage: asset detail, impact, movement, versions, export")]
    Lineage {
        #[command(subcommand)]
        action: LineageAction,
    },
    #[command(about = "Ontology models")]
    Ontology {
        #[command(subcommand)]
        action: OntologyAction,
    },
    #[command(about = "External job submission status/logs")]
    External {
        #[command(subcommand)]
        action: ExternalAction,
    },
    #[command(
        about = "Fix the newest failed run with gz code: evidence, local code, a prepared task",
        long_about = FIX_ABOUT
    )]
    Fix(FixArgs),
}

#[derive(Subcommand, Debug)]
pub enum OntologyAction {
    #[command(about = "List ontology models")]
    List,
    #[command(about = "Get an ontology model")]
    Get { id: i64 },
    #[command(about = "Delete an ontology model")]
    Delete { id: i64 },
    #[command(about = "Model usage")]
    Usage { id: i64 },
    #[command(about = "Calculate models")]
    Calculate {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List model releases")]
    Releases { id: i64 },
    #[command(about = "Create a model release")]
    Release {
        id: i64,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Record citation events")]
    Citations {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Resolve ontology context")]
    Resolve {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Create a model definition")]
    Define {
        id: i64,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Patch a model definition")]
    PatchDefinition {
        id: i64,
        definition: i64,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Batch-patch model definitions")]
    PatchDefinitions {
        id: i64,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Read a model release")]
    ReadRelease { id: i64, release: String },
    #[command(about = "How the model has drifted from its sources")]
    Drift { id: i64 },
    #[command(about = "Past imports into a model")]
    Imports { id: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LineageExportFormat {
    Json,
    Csv,
    Openlineage,
}

#[derive(Subcommand, Debug)]
pub enum LineageAction {
    #[command(about = "One asset with its upstream and downstream neighbours")]
    Asset {
        id: u64,
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=5), help = "Hops each way (0-5)")]
        depth: Option<u8>,
        #[arg(long, help = "Lineage as of this run")]
        run: Option<u64>,
    },
    #[command(about = "How data moved into and out of an asset")]
    Movement { id: u64 },
    #[command(about = "What breaks downstream if this asset changes")]
    Impact {
        id: u64,
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5), help = "Hops (1-5)")]
        depth: Option<u8>,
    },
    #[command(about = "Schema and lineage versions of an asset")]
    Versions { id: u64 },
    #[command(about = "Connections that have lineage")]
    Connections,
    #[command(about = "Export lineage edges (or OpenLineage events)")]
    Export {
        #[arg(long, value_parser = ["table", "column"], default_value = "table")]
        kind: String,
        #[arg(long, value_enum, default_value = "json")]
        format: LineageExportFormat,
        #[arg(long, help = "Only this connection")]
        connection: Option<u64>,
        #[arg(long)]
        offset: Option<u64>,
    },
}

impl LineageExportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            LineageExportFormat::Json => "json",
            LineageExportFormat::Csv => "csv",
            LineageExportFormat::Openlineage => "openlineage",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ExternalAction {
    #[command(about = "Submit an external job")]
    Submit {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "External job status")]
    Status {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "External job logs")]
    Logs {
        #[command(flatten)]
        body: BodyArgs,
    },
}

// ---------- notebooks ----------

#[derive(Args, Debug)]
pub struct NotebooksArgs {
    #[command(subcommand)]
    pub action: NotebooksAction,
}

#[derive(Subcommand, Debug)]
pub enum NotebooksAction {
    #[command(about = "List notebooks in a project")]
    List { project_id: i64 },
    #[command(about = "Create a notebook")]
    Create {
        #[arg(long)]
        project_id: i64,
        #[arg(long)]
        name: String,
        #[arg(long)]
        template: Option<String>,
    },
    #[command(about = "Publish notebook content (PUT)")]
    Update {
        project_id: i64,
        notebook_id: i64,
        #[arg(long)]
        path: String,
    },
    #[command(about = "Patch notebook metadata")]
    Patch {
        notebook_id: i64,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete a notebook")]
    Delete { project_id: i64, notebook_id: i64 },
    #[command(about = "Check a notebook name exists")]
    Exists { project_id: i64, name: String },
    #[command(about = "Start a notebook server")]
    Start {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Stop a notebook server")]
    Stop {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List running notebooks")]
    Running {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Publish a notebook")]
    Publish {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List published notebooks")]
    Published,
    #[command(about = "Get a published notebook")]
    GetPublished { name: String },
    #[command(about = "Unpublish a notebook")]
    Unpublish { notebook_id: i64, name: String },
}

// ---------- models ----------

#[derive(Args, Debug)]
pub struct ModelsArgs {
    #[command(subcommand)]
    pub action: ModelsAction,
}

#[derive(Subcommand, Debug)]
pub enum ModelsAction {
    #[command(about = "Start MLflow")]
    Start {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Stop MLflow")]
    Stop {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List models")]
    List {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List all models")]
    ListAll,
    #[command(about = "List all experiments")]
    Experiments,
    #[command(about = "Publish a model")]
    Publish {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List published models")]
    Published,
    #[command(about = "Import a model")]
    Import {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Unpublish a model")]
    Unpublish { id: i64, name: String },
    #[command(about = "Running MLflow pods")]
    Running,
    #[command(about = "Delete a model from a project")]
    Delete {
        project: i64,
        #[arg(help = "Model name")]
        name: String,
    },
}

// ---------- agents ----------

#[derive(Args, Debug)]
pub struct AgentsArgs {
    #[command(subcommand)]
    pub action: AgentsAction,
}

#[derive(Subcommand, Debug)]
pub enum AgentsAction {
    #[command(about = "List agents")]
    List {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Get an agent")]
    Get { id: String },
    #[command(about = "Create an agent")]
    Create {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update an agent")]
    Update {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete an agent")]
    Delete { id: String },
    #[command(about = "Test an agent without saving")]
    Test {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List agent runs")]
    Runs {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Get a run, with the child runs its sub-agents started")]
    Run { id: String, run: String },
    #[command(about = "List an agent's sub-agents (agents it can call as tools)")]
    SubAgents { id: String },
    #[command(about = "Let an agent call another agent as a tool (call_<alias>)")]
    AddSubAgent {
        id: String,
        #[arg(help = "The agent to call")]
        child: String,
        #[arg(long, help = "Short name; the tool becomes call_<alias>")]
        alias: String,
        #[arg(long, help = "When the parent should use it")]
        description: Option<String>,
    },
    #[command(about = "Stop an agent calling a sub-agent")]
    RemoveSubAgent {
        id: String,
        #[arg(help = "Alias or agent id of the sub-agent")]
        alias: String,
    },
    #[command(about = "Stream a run's events until it finishes")]
    Events { id: String, run: String },
    #[command(about = "Cancel a run")]
    Cancel { id: String, run: String },
    #[command(about = "Run observability")]
    Observability { id: String, run: String },
    #[command(about = "Ask an agent and stream its answer")]
    Invoke {
        id: String,
        #[arg(help = "Message (or @file)")]
        message: String,
        #[arg(
            long,
            help = "Run the unpublished draft (what the builder's preview uses)"
        )]
        draft: bool,
        #[arg(long, help = "Continue an existing session")]
        session: Option<String>,
    },
    #[command(about = "Resume a run")]
    Resume {
        id: String,
        run: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List sessions")]
    Sessions { id: String },
    #[command(about = "Session messages")]
    Messages { id: String, session: String },
    #[command(about = "Approval history of an agent")]
    Approvals { id: String },
    #[command(about = "Publish an approved agent")]
    Publish { id: String },
    #[command(about = "All approval requests")]
    ApprovalRequests {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Decide an approval request")]
    Decide {
        approval: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List publications")]
    Publications,
    #[command(about = "Agent quota")]
    Quota { id: String },
    #[command(about = "Set agent quota")]
    SetQuota {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Agent tags")]
    Tags { id: String },
    #[command(about = "Tag an agent")]
    Tag {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List tools")]
    Tools,
    #[command(about = "List models")]
    Models,
    #[command(about = "Agent metrics")]
    Metrics {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Metrics for one agent")]
    AgentMetrics { id: String },
    #[command(about = "Agent metrics over time")]
    Timeseries {
        #[arg(long, help = "Start, ISO 8601, e.g. 2026-11-01T00:00:00Z")]
        from: String,
        #[arg(long, help = "End, ISO 8601")]
        to: String,
        #[arg(long, value_parser = ["hour", "day", "week"], default_value = "day")]
        bucket: String,
    },
    #[command(about = "Submit the current version for admin approval")]
    SubmitApproval {
        id: String,
        #[arg(long, help = "Note for the reviewer")]
        comment: Option<String>,
    },
    #[command(about = "Agents published to the company catalog")]
    Catalog {
        #[arg(long)]
        offset: Option<u32>,
    },
    #[command(about = "Install a catalog agent into a project")]
    ImportPublication {
        publication: String,
        #[arg(long, help = "Target project id")]
        project: u64,
        #[arg(long, value_parser = ["fail", "replace", "keep_both"], default_value = "fail", help = "If an agent with that name exists")]
        on_conflict: String,
        #[arg(long, help = "Install under this name")]
        name: Option<String>,
    },
    #[command(about = "Governance checks of a run")]
    RunGovernance { id: String, run: String },
    #[command(about = "Token, cost, and latency metrics of a run")]
    RunMetrics { id: String, run: String },
    #[command(about = "Quality evaluations of a run")]
    Evaluations { id: String, run: String },
    #[command(about = "Feedback left on a run")]
    Feedback { id: String, run: String },
    #[command(about = "Audit trail of an agent")]
    Audit {
        id: String,
        #[arg(long, help = "Only this run")]
        run: Option<String>,
    },
    #[command(about = "Remove an agent's spending limit")]
    ClearQuota { id: String },
    #[command(about = "Remove a tag from an agent")]
    Untag { id: String, key: String },
}

// ---------- chat ----------

#[derive(Args, Debug)]
pub struct ChatArgs {
    #[command(subcommand)]
    pub action: ChatAction,
}

#[derive(Subcommand, Debug)]
pub enum ChatAction {
    #[command(about = "Ask Groundzero AI and stream the answer")]
    Ask {
        #[arg(help = "Your question (or @file)")]
        prompt: String,
        #[arg(
            long,
            short,
            help = "Continue this conversation (id from `gz chat list`)"
        )]
        conversation: Option<String>,
        #[arg(long, help = "Title for a new conversation")]
        title: Option<String>,
    },
    #[command(about = "List your conversations", alias = "conversations")]
    List {
        #[arg(long, help = "Match titles and messages")]
        search: Option<String>,
    },
    #[command(about = "Show a conversation", alias = "conversation")]
    Show { id: String },
    #[command(
        about = "Delete one or more conversations",
        alias = "delete-conversation"
    )]
    Delete {
        #[arg(required = true, num_args = 1..=100)]
        ids: Vec<String>,
    },
    #[command(about = "Copy a conversation (optionally up to one message) into a new one")]
    Fork {
        id: String,
        #[arg(long, help = "Copy history through the message at this timestamp")]
        through: Option<String>,
        #[arg(long, requires = "through", help = "Stop just before that message")]
        exclude_selected: bool,
        #[arg(long, help = "Name of the new conversation")]
        name: Option<String>,
    },
    #[command(about = "Redirect a reply that is still running")]
    Steer {
        run: String,
        #[arg(help = "What the assistant should do instead (or @file)")]
        message: String,
    },
    #[command(about = "Status of a chat run")]
    Status { run: String },
    #[command(about = "Tool actions you've approved ahead of time")]
    Grants {
        #[arg(long, help = "Also list grants for this conversation")]
        conversation: Option<String>,
    },
    #[command(about = "Withdraw a standing approval so the tool asks again")]
    Revoke {
        #[arg(help = "Tool name, as shown by `gz chat grants`")]
        tool: String,
        #[arg(long, value_enum, default_value = "always")]
        scope: GrantScope,
        #[arg(long, required_if_eq("scope", "conversation"))]
        conversation: Option<String>,
    },
    #[command(about = "Models and features available to you")]
    Capabilities,
    #[command(about = "Rename a conversation")]
    Rename { id: String, name: String },
    #[command(about = "Approve or deny a paused tool action, then keep streaming")]
    Approve {
        run: String,
        #[arg(long, help = "Interrupt id from the paused run")]
        interrupt: String,
        #[arg(long, help = "Deny instead of approve")]
        deny: bool,
        #[arg(
            long,
            value_enum,
            default_value = "once",
            help = "Remember the approval: once, for this chat, or always"
        )]
        remember: Remember,
    },
    #[command(about = "Start a chat run with a raw body (advanced)")]
    Run {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Budget status")]
    Budget,
    #[command(about = "Chat metrics")]
    Metrics,
    #[command(about = "Upload a document for chat")]
    Upload {
        #[arg(help = "Local file to upload")]
        file: String,
    },
    #[command(about = "Cancel a chat run")]
    Cancel { run: String },
    #[command(about = "Stop the active reply in a conversation")]
    Stop { conversation: String },
    #[command(about = "Send feedback on a reply")]
    Feedback {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Improve a prompt before sending it")]
    Enhance { prompt: String },
    #[command(about = "Analyse logs with chat")]
    AnalyseLogs {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Data lineage insight")]
    Lineage {
        #[command(flatten)]
        body: BodyArgs,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum GrantScope {
    Conversation,
    Always,
}

/// How long `gz chat approve` remembers an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Remember {
    Once,
    Chat,
    Always,
}

impl Remember {
    pub fn decision(self) -> crate::assistant::Decision {
        use crate::assistant::Decision;
        match self {
            Remember::Once => Decision::AllowOnce,
            Remember::Chat => Decision::AllowChat,
            Remember::Always => Decision::AllowAlways,
        }
    }
}

impl GrantScope {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantScope::Conversation => "conversation",
            GrantScope::Always => "always",
        }
    }
}

// ---------- logs ----------

#[derive(Args, Debug)]
pub struct LogsArgs {
    #[command(subcommand)]
    pub action: LogsAction,
}

#[derive(Subcommand, Debug)]
pub enum LogsAction {
    #[command(about = "Run a log export once (spec via --data)")]
    Export {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List export runs")]
    Runs {
        #[arg(long, help = "Only runs of this schedule")]
        schedule: Option<String>,
        #[arg(long, value_parser = ["manual", "scheduled"])]
        trigger: Option<String>,
        #[arg(long, help = "Runs per page (server default 50)")]
        page_size: Option<u32>,
        #[arg(long)]
        offset: Option<u32>,
    },
    #[command(about = "Get an export run")]
    Run { id: String },
    #[command(about = "Delete a run-once export")]
    DeleteRun { id: String },
    #[command(about = "List export schedules")]
    Schedules,
    #[command(about = "Get an export schedule")]
    GetSchedule { name: String },
    #[command(about = "Create an export schedule (spec via --data)")]
    CreateSchedule {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update an export schedule (changed fields via --data)")]
    UpdateSchedule {
        name: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Stop (disable) an export schedule")]
    StopSchedule { name: String },
    #[command(about = "Delete an export schedule")]
    DeleteSchedule { name: String },
}

// ---------- rte ----------

#[derive(Args, Debug)]
pub struct RteArgs {
    #[command(subcommand)]
    pub action: RteAction,
}

#[derive(Subcommand, Debug)]
pub enum RteAction {
    #[command(about = "List runtime environments")]
    List,
    #[command(about = "Register a runtime image")]
    Register {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List runtime image names")]
    Images,
    #[command(about = "List all runtime images, including hidden ones")]
    All,
    #[command(about = "Compute sizes (no subcommand lists them)")]
    Compute {
        #[command(subcommand)]
        action: Option<ComputeAction>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ComputeAction {
    #[command(about = "Get a compute size")]
    Get { id: i64 },
    #[command(about = "Create a compute size")]
    Create { name: String },
    #[command(about = "Replace a compute size's config")]
    Update {
        id: i64,
        #[arg(
            long,
            value_name = "JSON|@FILE",
            help = "Config, e.g. {\"cpu\":\"2\",\"memory\":\"4Gi\"}"
        )]
        config: String,
    },
    #[command(about = "Rename a compute size")]
    Rename { id: i64, name: String },
    #[command(about = "Delete a compute size")]
    Delete { id: i64 },
}

// ---------- sites ----------

#[derive(Args, Debug)]
pub struct SitesArgs {
    #[command(subcommand)]
    pub action: SitesAction,
}

#[derive(Subcommand, Debug)]
pub enum SitesAction {
    #[command(about = "Save a site setting, e.g. theme")]
    Set {
        name: String,
        #[arg(long, value_name = "JSON|@FILE", help = "Setting value (JSON object)")]
        value: String,
    },
    #[command(about = "Get a site setting")]
    Get { name: String },
}

// ---------- request / completion ----------

#[derive(Args, Debug)]
pub struct RequestArgs {
    #[arg(help = "Service key (see `gz doctor`), e.g. lakehouse, etl, chat, datasets")]
    pub service: String,
    #[arg(help = "HTTP method")]
    pub method: String,
    #[arg(help = "Path, e.g. /lakehouse/warehouse")]
    pub path: String,
    #[command(flatten)]
    pub body: BodyArgs,
}

#[derive(Args, Debug)]
pub struct CompletionArgs {
    #[arg(help = "Shell: bash, zsh, fish, powershell, elvish")]
    pub shell: String,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[arg(long, help = "Only check; don't download or replace anything")]
    pub check: bool,
    #[arg(
        long,
        value_name = "TAG",
        help = "Install a specific release tag (e.g. v0.2.0)"
    )]
    pub version: Option<String>,
}

// ---------- projects ----------

#[derive(Args, Debug)]
pub struct ProjectsArgs {
    #[command(subcommand)]
    pub action: ProjectsAction,
}

#[derive(Subcommand, Debug)]
pub enum ProjectsAction {
    #[command(about = "List DE/ML projects")]
    List,
    #[command(about = "Get a project")]
    Get { id: String },
    #[command(about = "Create a project")]
    Create {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Set a project's description")]
    Update {
        id: String,
        #[arg(long)]
        description: String,
    },
    #[command(about = "Rename a project")]
    Rename { id: String, name: String },
    #[command(about = "Transfer a project to another owner (admins)")]
    SetOwner { id: String, owner: String },
    #[command(about = "Delete a project")]
    Delete { id: String },
    #[command(about = "List jobs in a project")]
    Jobs { project: String },
    #[command(about = "Get a job")]
    Job { project: String, job: String },
    #[command(about = "Create a job in a project")]
    CreateJob {
        project: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a job's code and/or config (unchanged parts are kept)")]
    UpdateJob {
        project: String,
        job: String,
        #[arg(long, value_name = "@FILE|TEXT", help = "New job source")]
        code: Option<String>,
        #[arg(long, value_name = "JSON|@FILE", help = "Config fields to change")]
        config: Option<String>,
    },
    #[command(about = "Rename a job")]
    RenameJob { job: String, name: String },
    #[command(about = "Delete a job")]
    DeleteJob { job: String },
    #[command(about = "Run history of a job")]
    JobRuns { job: String },
    #[command(about = "List published jobs")]
    Published,
    #[command(about = "Get a published job by name")]
    PublishedJob { name: String },
    #[command(about = "Publish a job")]
    Publish {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Unpublish a job")]
    Unpublish { job: String, name: String },
    #[command(about = "Check a job name exists")]
    JobExists { project: String, name: String },
}

// ---------- datasets ----------

/// Datasets are Designer objects; this top-level command exists because people (and gz code)
/// look for them by name, not under `designer`.
#[derive(Args, Debug)]
pub struct DatasetsArgs {
    #[command(subcommand)]
    pub action: Option<DatasetsAction>,
}

#[derive(Subcommand, Debug)]
pub enum DatasetsAction {
    #[command(about = "List datasets (the default)")]
    List,
    #[command(about = "Get a dataset")]
    Get { id: String },
}

impl DatasetsArgs {
    /// The Designer action this shortcut stands for.
    pub fn designer_action(&self) -> DesignerAction {
        match &self.action {
            None | Some(DatasetsAction::List) => DesignerAction::Datasets,
            Some(DatasetsAction::Get { id }) => DesignerAction::Dataset { id: id.clone() },
        }
    }
}

// ---------- designer ----------

#[derive(Args, Debug)]
pub struct DesignerArgs {
    #[command(subcommand)]
    pub action: DesignerAction,
}

#[derive(Subcommand, Debug)]
pub enum DesignerAction {
    #[command(about = "List datasets")]
    Datasets,
    #[command(about = "Get a dataset")]
    Dataset { id: String },
    #[command(about = "Create a dataset")]
    CreateDataset {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a dataset")]
    UpdateDataset {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List dashboards")]
    Dashboards,
    #[command(about = "Get a dashboard")]
    Dashboard { id: String },
    #[command(about = "Create a dashboard")]
    CreateDashboard {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a dashboard")]
    UpdateDashboard {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List published dashboards")]
    Published,
    #[command(about = "Publish a dashboard")]
    Publish {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Unpublish a dashboard")]
    Unpublish { dashboard: String, name: String },
    #[command(about = "List visualizations")]
    Visualizations,
    #[command(about = "Get a visualization")]
    Visualization { id: String },
    #[command(about = "Create a visualization")]
    CreateVisualization {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a visualization")]
    UpdateVisualization {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List filters")]
    Filters,
    #[command(about = "Get a filter")]
    Filter { id: String },
    #[command(about = "Create a filter")]
    CreateFilter {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a filter")]
    UpdateFilter {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Rename a dataset, dashboard, visualization, or filter")]
    Rename {
        kind: DesignerKind,
        id: String,
        name: String,
    },
    #[command(about = "Delete a dataset, dashboard, visualization, or filter")]
    Delete { kind: DesignerKind, id: String },
    #[command(about = "Check whether a dashboard name is taken")]
    DashboardExists { name: String },
    #[command(about = "Get a published dashboard by name")]
    PublishedDashboard { name: String },
}

/// Designer resources that share the same CRUD routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DesignerKind {
    Dataset,
    Dashboard,
    Visualization,
    Filter,
}

// ---------- workspaces ----------

#[derive(Args, Debug)]
pub struct WorkspacesArgs {
    #[command(subcommand)]
    pub action: WorkspacesAction,
}

#[derive(Subcommand, Debug)]
pub enum WorkspacesAction {
    #[command(about = "List workspaces")]
    List,
    #[command(about = "List a workspace folder's contents")]
    Contents { id: String },
    #[command(about = "Create a workspace")]
    Create {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Create inside a workspace folder")]
    CreateIn {
        folder: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Get a workspace")]
    Get { id: String },
    #[command(about = "Rename a workspace")]
    Rename { id: String, name: String },
    #[command(about = "Move a workspace under another folder")]
    Move {
        id: String,
        #[arg(long, help = "Destination folder id")]
        parent: i64,
    },
    #[command(about = "Transfer a workspace to another owner (admins)")]
    SetOwner { id: String, owner: String },
    #[command(about = "Delete a workspace")]
    Delete { id: String },
    #[command(about = "Remove an item from a workspace")]
    Remove { workspace: String, id: String },
    #[command(about = "Check a dashboard exists in a workspace")]
    Exists { parent: String, name: String },
    #[command(about = "Add a dashboard to a workspace")]
    AddDashboard {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Get a workspace dashboard")]
    Dashboard { parent: String, dashboard: String },
}

// ---------- schedules ----------

#[derive(Args, Debug)]
pub struct SchedulesArgs {
    #[command(subcommand)]
    pub action: SchedulesAction,
}

#[derive(Subcommand, Debug)]
pub enum SchedulesAction {
    #[command(about = "List schedules")]
    List,
    #[command(about = "Get a schedule")]
    Get { id: String },
    #[command(about = "Create a schedule")]
    Create {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a schedule")]
    Update {
        id: String,
        name: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete a schedule")]
    Delete { id: String, name: String },
    #[command(about = "Start a schedule")]
    Start { id: String, name: String },
    #[command(about = "Stop a schedule")]
    Stop { id: String, name: String },
    #[command(about = "Run a schedule once now (cron + jobDetails via --data)")]
    Run {
        id: String,
        name: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "A schedule's runs")]
    Runs { id: String },
    #[command(about = "Schedule executions")]
    Executions,
    #[command(about = "Email notifications of a schedule")]
    Email {
        #[command(subcommand)]
        action: ScheduleEmailAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ScheduleEmailAction {
    #[command(about = "Notification types (success, failure, …)")]
    Types,
    #[command(about = "Placeholders usable in subject and body")]
    Parameters,
    #[command(about = "List a schedule's email notifications")]
    List { schedule: String },
    #[command(about = "Add an email notification (fields via --data)")]
    Add {
        schedule: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Replace an email notification (fields via --data)")]
    Update {
        schedule: String,
        notification: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete an email notification")]
    Delete {
        schedule: String,
        notification: String,
    },
}

// ---------- permissions ----------

#[derive(Args, Debug)]
pub struct PermissionsArgs {
    #[command(subcommand)]
    pub action: PermissionsAction,
}

#[derive(Subcommand, Debug)]
pub enum PermissionsAction {
    #[command(about = "List resource types")]
    List,
    #[command(about = "List resources of a type")]
    Resources { resource_type: String },
    #[command(about = "Grant permissions")]
    Grant {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List effects (allow / deny)")]
    Effects,
    #[command(about = "Access types for a resource type")]
    AccessTypes { resource_type: String },
    #[command(about = "Get a policy")]
    Policy {
        resource_type: String,
        effect: String,
        permission: String,
    },
    #[command(about = "Resource policy")]
    ResourcePolicy {
        resource_type: String,
        resource: String,
        effect: String,
        permission: String,
    },
    #[command(about = "A user's cached permissions for a resource type")]
    UserCache {
        username: String,
        resource_type: String,
    },
    #[command(about = "Remove access")]
    RemoveAccess {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "API keys")]
    ApiKey {
        #[command(subcommand)]
        action: ApiKeyAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ApiKeyAction {
    #[command(about = "List a user's API keys")]
    List { username: String },
    #[command(about = "Create an API key for a user")]
    Create {
        username: String,
        #[arg(long, help = "What the key is for")]
        use_case: String,
    },
    #[command(about = "Enable an API key")]
    Enable { id: String },
    #[command(about = "Disable an API key")]
    Disable { id: String },
}

// ---------- connections ----------

#[derive(Args, Debug)]
pub struct ConnectionsArgs {
    #[command(subcommand)]
    pub action: ConnectionsAction,
}

#[derive(Subcommand, Debug)]
pub enum ConnectionsAction {
    #[command(about = "List connections")]
    List,
    #[command(about = "Get a connection")]
    Get { id: String },
    #[command(about = "Create a connection")]
    Create {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a connection")]
    Update {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Rename a connection")]
    Rename { id: String, name: String },
    #[command(about = "Delete a connection")]
    Delete { id: String },
}

// ---------- admin ----------

#[derive(Args, Debug)]
pub struct AdminArgs {
    #[command(subcommand)]
    pub action: AdminAction,
}

/// Confirmation for actions that change or remove other people's access.
#[derive(Args, Debug, Clone, Copy)]
pub struct Confirm {
    #[arg(long, short = 'y', help = "Don't ask for confirmation")]
    pub yes: bool,
}

#[derive(Subcommand, Debug)]
pub enum AdminAction {
    #[command(about = "Users in the Cognito user pool")]
    Users {
        #[command(subcommand)]
        action: AdminUsersAction,
    },
    #[command(about = "User groups")]
    Groups {
        #[command(subcommand)]
        action: AdminGroupsAction,
    },
    #[command(about = "Chat spending limits per user or group")]
    Budgets {
        #[command(subcommand)]
        action: AdminBudgetsAction,
    },
    #[command(about = "List sites")]
    Sites,
    #[command(about = "Verified sender emails for notifications")]
    Emails {
        #[command(subcommand)]
        action: AdminEmailsAction,
    },
    #[command(about = "Account details (client, plan)")]
    Client,
    #[command(about = "Monthly bill")]
    Billing {
        #[arg(long, help = "e.g. 2026")]
        year: i32,
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=12), help = "1-12")]
        month: u8,
        #[arg(long, help = "Only this site (default: all sites)")]
        site: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum AdminUsersAction {
    #[command(about = "List users")]
    List {
        #[arg(
            long,
            value_name = "TOKEN",
            help = "Next page (token printed by the previous list)"
        )]
        next: Option<String>,
    },
    #[command(about = "Show one user")]
    Get { username: String },
    #[command(about = "Invite a user (emails a temporary password)")]
    Create {
        username: String,
        #[arg(long)]
        email: String,
        #[arg(long, help = "Site the user belongs to")]
        site: Option<String>,
    },
    #[command(about = "Delete a user")]
    Delete {
        username: String,
        #[command(flatten)]
        confirm: Confirm,
    },
    #[command(about = "Block a user from signing in")]
    Disable {
        username: String,
        #[command(flatten)]
        confirm: Confirm,
    },
    #[command(about = "Let a disabled user sign in again")]
    Enable { username: String },
    #[command(about = "Groups a user belongs to")]
    Groups { username: String },
    #[command(about = "Add a user to a group")]
    AddGroup { username: String, group: String },
    #[command(about = "Remove a user from a group")]
    RemoveGroup {
        username: String,
        group: String,
        #[command(flatten)]
        confirm: Confirm,
    },
}

#[derive(Subcommand, Debug)]
pub enum AdminGroupsAction {
    #[command(about = "List groups")]
    List {
        #[arg(
            long,
            value_name = "TOKEN",
            help = "Next page (token printed by the previous list)"
        )]
        next: Option<String>,
    },
    #[command(about = "Users in a group")]
    Members {
        group: String,
        #[arg(
            long,
            value_name = "TOKEN",
            help = "Next page (token printed by the previous list)"
        )]
        next: Option<String>,
    },
    #[command(about = "Create a group")]
    Create {
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, help = "Lower wins when a user is in several groups")]
        precedence: Option<u32>,
    },
    #[command(about = "Delete a group")]
    Delete {
        name: String,
        #[command(flatten)]
        confirm: Confirm,
    },
    #[command(about = "Built-in system groups")]
    System,
    #[command(about = "Groups used for workspace sharing")]
    Workspace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BudgetSubject {
    User,
    Group,
}

impl BudgetSubject {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetSubject::User => "user",
            BudgetSubject::Group => "group",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum AdminBudgetsAction {
    #[command(about = "All budgets for users or groups")]
    List {
        #[arg(long = "for", value_enum, default_value = "user")]
        subject: BudgetSubject,
        #[arg(long, help = "Site (default: your profile's site)")]
        site: Option<String>,
    },
    #[command(about = "Budget of one user or group")]
    Get {
        name: String,
        #[arg(long = "for", value_enum, default_value = "user")]
        subject: BudgetSubject,
        #[arg(long)]
        site: Option<String>,
    },
    #[command(about = "Set a monthly limit")]
    Set {
        name: String,
        #[arg(long, help = "Limit in US cents, e.g. 5000 = $50")]
        limit_cents: u64,
        #[arg(long = "for", value_enum, default_value = "user")]
        subject: BudgetSubject,
        #[arg(long)]
        site: Option<String>,
    },
    #[command(about = "Remove a limit")]
    Remove {
        name: String,
        #[arg(long = "for", value_enum, default_value = "user")]
        subject: BudgetSubject,
        #[arg(long)]
        site: Option<String>,
        #[command(flatten)]
        confirm: Confirm,
    },
}

#[derive(Subcommand, Debug)]
pub enum AdminEmailsAction {
    #[command(about = "List sender emails and their verification state")]
    List,
    #[command(about = "Add a sender email (AWS sends a verification mail)")]
    Add { email: String },
    #[command(about = "Remove a sender email")]
    Remove {
        email: String,
        #[command(flatten)]
        confirm: Confirm,
    },
}

// ---------- search ----------

#[derive(Args, Debug)]
pub struct SearchArgs {
    #[arg(help = "What to look for (typos are fine)")]
    pub query: String,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Only these kinds, e.g. dashboard,dataset"
    )]
    pub kinds: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn gz_datasets_lists_by_default_and_gets_by_id() {
        let action = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Datasets(datasets) => datasets.designer_action(),
            other => panic!("parsed as {other:?}"),
        };
        assert!(matches!(
            action(&["gz", "datasets"]),
            DesignerAction::Datasets
        ));
        assert!(matches!(
            action(&["gz", "datasets", "list"]),
            DesignerAction::Datasets
        ));
        assert!(matches!(
            action(&["gz", "datasets", "get", "1888"]),
            DesignerAction::Dataset { id } if id == "1888"
        ));
    }

    /// Same name *and* type as the global flag, so clap reads it safely:
    /// `configure --host` is how the global override gets saved.
    const SAFE_REUSE: &[(&str, &str)] = &[("configure", "host")];

    /// A subcommand flag named like a global one with a different type (e.g.
    /// `--limit: u32` vs the global `usize`) compiles but panics when parsed.
    #[test]
    fn no_subcommand_reuses_a_global_flag_name() {
        let root = Cli::command();
        let globals: Vec<String> = root
            .get_arguments()
            .filter(|a| a.is_global_set())
            .map(|a| a.get_id().to_string())
            .collect();
        let mut stack: Vec<(String, clap::Command)> = root
            .get_subcommands()
            .map(|c| (c.get_name().to_string(), c.clone()))
            .collect();
        while let Some((path, command)) = stack.pop() {
            for arg in command.get_arguments() {
                let id = arg.get_id().to_string();
                let allowed = SAFE_REUSE.contains(&(path.as_str(), id.as_str()));
                assert!(
                    !globals.contains(&id) || arg.is_global_set() || allowed,
                    "`gz {path}` redefines global --{id}"
                );
            }
            for sub in command.get_subcommands() {
                stack.push((format!("{path} {}", sub.get_name()), sub.clone()));
            }
        }
    }
}

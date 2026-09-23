use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum OutputArg {
    #[default]
    Table,
    Json,
    Yaml,
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
        help = "Bearer token for one-off calls (prefer `gz auth login`)"
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
    #[arg(long, global = true, help = "Truncate listed arrays to N items")]
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
    Doctor,
    #[command(about = "Set client, site, domain (prompts when no flags given)")]
    Configure(ConfigureArgs),
    #[command(about = "Manage config profiles")]
    Profile(ProfileArgs),
    #[command(about = "OAuth login, status, and logout (MCP-style)")]
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
    #[command(about = "Data connections: schemas, tests, reads")]
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
    #[command(about = "Print shell completions")]
    Completion(CompletionArgs),
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
    #[command(about = "Log in via OAuth (browser) or --manual (user/pass/TOTP)")]
    Login {
        #[arg(
            long,
            help = "Headless login with username/password/TOTP instead of browser"
        )]
        manual: bool,
        #[arg(long, help = "Request write scope as well as read")]
        write: bool,
        #[arg(long, help = "Site for manual login (defaults to profile site)")]
        site: Option<String>,
        #[arg(long, help = "Username for manual login (prompted if missing)")]
        username: Option<String>,
        #[arg(long, help = "Auth domain for manual login")]
        domain: Option<String>,
        #[arg(long, help = "Print the authorize URL instead of opening a browser")]
        no_open: bool,
        #[arg(
            long,
            default_value = "0",
            help = "Loopback callback port (0 = random)"
        )]
        port: u16,
    },
    #[command(about = "Show login status (token redacted)")]
    Status,
    #[command(about = "Revoke the token server-side and clear it locally")]
    Logout {
        #[arg(long, help = "Clear locally without calling the revoke endpoint")]
        local_only: bool,
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
        #[command(flatten)]
        body: BodyArgs,
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
pub struct SqlArgs {
    #[command(subcommand)]
    pub action: SqlAction,
}

#[derive(Subcommand, Debug)]
pub enum SqlAction {
    #[command(about = "Run a SQL query (connectionConfig via --data)")]
    Run {
        #[arg(long, help = "SQL text (or @file.sql)")]
        sql: Option<String>,
        #[arg(long, help = "Wait for output")]
        wait: bool,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Submit a query asynchronously")]
    Submit {
        #[arg(long, help = "SQL text (or @file.sql)")]
        sql: Option<String>,
        #[command(flatten)]
        body: BodyArgs,
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
    #[command(about = "Stop a lakehouse session")]
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
    #[command(about = "Update a saved query")]
    Update {
        id: i64,
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
    #[command(about = "List table snapshots")]
    Snapshots {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Roll back a table to a snapshot")]
    Rollback {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Expire old snapshots")]
    ExpireSnapshots {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Remove orphan files")]
    RemoveOrphans {
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
    AuditLogs { warehouse: String },
    #[command(about = "Maintain (prune) audit logs")]
    MaintainAuditLogs { warehouse: String },
    #[command(about = "Issue short-lived Iceberg credentials")]
    Credentials {
        #[command(flatten)]
        body: BodyArgs,
    },
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
    #[command(about = "Record a backup status")]
    RecordStatus {
        #[command(flatten)]
        body: BodyArgs,
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

#[derive(Subcommand, Debug)]
pub enum GovernAction {
    #[command(about = "Get the table data policy")]
    GetPolicy {
        #[command(flatten)]
        body: BodyArgs,
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
        body: BodyArgs,
    },
    #[command(about = "Roll back a policy version")]
    RollbackPolicy {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Get table permissions")]
    GetPermissions {
        #[command(flatten)]
        body: BodyArgs,
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
        body: BodyArgs,
    },
    #[command(about = "Ingest lineage events")]
    IngestLineage {
        #[command(flatten)]
        body: BodyArgs,
    },
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
    #[command(about = "Reindex warehouse tags")]
    Reindex { warehouse: String },
    #[command(about = "Search tagged objects")]
    Search {
        warehouse: String,
        #[arg(long)]
        q: Option<String>,
        #[command(flatten)]
        body: BodyArgs,
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
    #[command(about = "List files (detailed)")]
    List {
        #[arg(long)]
        connection: String,
        #[arg(long, default_value = "")]
        prefix: String,
    },
    #[command(about = "Download a file")]
    Get {
        #[arg(long)]
        connection: String,
        #[arg(long)]
        path: String,
        #[arg(long, help = "Local destination (defaults to the file name)")]
        out: Option<String>,
    },
    #[command(about = "Upload a file")]
    Put {
        #[arg(long)]
        connection: String,
        #[arg(long)]
        path: String,
        #[arg(help = "Local file to upload")]
        file: String,
    },
    #[command(about = "Create a folder")]
    Mkdir {
        #[arg(long)]
        connection: String,
        #[arg(long)]
        path: String,
    },
    #[command(about = "Delete files")]
    Rm {
        #[arg(long)]
        connection: String,
        #[arg(help = "File keys to delete")]
        keys: Vec<String>,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete folders")]
    Rmdir {
        #[arg(long)]
        connection: String,
        #[arg(help = "Folder keys to delete")]
        keys: Vec<String>,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Custom RTE build status")]
    RteStatus,
    #[command(about = "Upload a custom RTE artifact")]
    RtePut {
        #[arg(help = "Local file to upload")]
        file: String,
        #[command(flatten)]
        body: BodyArgs,
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
    #[command(about = "Submit an ETL job (full spec via --data)")]
    Submit {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Abort a running execution")]
    Abort {
        #[arg(long)]
        execution: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        job_id: Option<i64>,
    },
    #[command(about = "Job status by job identifier")]
    Status {
        #[arg(long)]
        job: String,
    },
    #[command(about = "Fetch job logs")]
    Logs {
        #[arg(long)]
        job: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        next_token: Option<String>,
        #[command(flatten)]
        body: BodyArgs,
    },
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
    #[command(about = "Runtime observability snapshot")]
    Runtime {
        #[command(flatten)]
        body: BodyArgs,
    },
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
    #[command(about = "Lineage assets")]
    LineageAssets {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Ingest catalog lineage events")]
    IngestLineage {
        #[command(flatten)]
        body: BodyArgs,
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
    #[command(about = "Admin: users, permissions, deploy")]
    Admin {
        #[command(subcommand)]
        action: ModelsAdminAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ModelsAdminAction {
    #[command(about = "Create a user")]
    CreateUser {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete a user")]
    DeleteUser {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Grant admin")]
    SetAdmin {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Update a password")]
    UpdatePass {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Set experiment permission")]
    ExperimentPerm {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Set model permission")]
    ModelPerm {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List experiments")]
    Experiments {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List experiment runs")]
    Runs {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List models")]
    List {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Deploy MLOps")]
    Deploy {
        #[command(flatten)]
        body: BodyArgs,
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
    #[command(about = "Get a run")]
    Run { id: String, run: String },
    #[command(about = "Run events")]
    Events { id: String, run: String },
    #[command(about = "Run observability")]
    Observability { id: String, run: String },
    #[command(about = "Start a run")]
    Start {
        id: String,
        #[command(flatten)]
        body: BodyArgs,
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
    #[command(about = "Pending approvals")]
    Approvals { id: String },
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
}

// ---------- chat ----------

#[derive(Args, Debug)]
pub struct ChatArgs {
    #[command(subcommand)]
    pub action: ChatAction,
}

#[derive(Subcommand, Debug)]
pub enum ChatAction {
    #[command(about = "Start a chat run (body via --data)")]
    Run {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Cancel a chat run")]
    Cancel {
        run: String,
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Stop chat streaming")]
    Stop {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Create a conversation")]
    NewConversation {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Send feedback")]
    Feedback {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Enhance a prompt")]
    Enhance {
        #[command(flatten)]
        body: BodyArgs,
    },
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

// ---------- logs ----------

#[derive(Args, Debug)]
pub struct LogsArgs {
    #[command(subcommand)]
    pub action: LogsAction,
}

#[derive(Subcommand, Debug)]
pub enum LogsAction {
    #[command(about = "Validate a log export request")]
    Validate {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Create a log export")]
    Export {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "List export schedules")]
    Schedules,
    #[command(about = "Get an export schedule")]
    GetSchedule { name: String },
    #[command(about = "Create or update an export schedule")]
    CreateSchedule {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Delete an export schedule")]
    DeleteSchedule { name: String },
    #[command(about = "Pause an export schedule")]
    PauseSchedule { name: String },
    #[command(about = "Resume an export schedule")]
    ResumeSchedule { name: String },
}

// ---------- rte ----------

#[derive(Args, Debug)]
pub struct RteArgs {
    #[command(subcommand)]
    pub action: RteAction,
}

#[derive(Subcommand, Debug)]
pub enum RteAction {
    #[command(about = "List known runtime environments (offline)")]
    List,
}

// ---------- sites ----------

#[derive(Args, Debug)]
pub struct SitesArgs {
    #[command(subcommand)]
    pub action: SitesAction,
}

#[derive(Subcommand, Debug)]
pub enum SitesAction {
    #[command(about = "Save a site setting")]
    Set {
        #[command(flatten)]
        body: BodyArgs,
    },
    #[command(about = "Get a site setting")]
    Get { name: String },
}

// ---------- request / completion ----------

#[derive(Args, Debug)]
pub struct RequestArgs {
    #[arg(
        help = "Service key: lakehouse, iceberg, etl, data, files, mlops, mlopsadmin, agents, chat, logs, auth"
    )]
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

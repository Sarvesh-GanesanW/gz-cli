//! The agent builder's `GET /v1/admin/agents/audit` records, as far as the
//! findings read them. `-o json` prints the server's records whole.

use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub created_by: Option<String>,
    pub owner_status: OwnerStatus,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub data_classification: Option<String>,
    pub model: Model,
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(default)]
    pub mcp_connections: Vec<McpConnection>,
    #[serde(default)]
    pub sub_agents: Vec<Delegate>,
    #[serde(default)]
    pub team: Option<Team>,
    pub limits: Limits,
    pub runs: Runs,
}

impl AgentRecord {
    /// Sub-agents and team members: every agent this one can hand work to.
    pub fn delegates(&self) -> impl Iterator<Item = &Delegate> {
        self.sub_agents
            .iter()
            .chain(self.team.iter().flat_map(|team| team.members.iter()))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.name == name)
    }

    pub fn has_mcp_connection(&self, name: &str) -> bool {
        self.mcp_connections.iter().any(|mcp| mcp.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OwnerStatus {
    Active,
    Disabled,
    /// The user pool has no such user any more.
    Missing,
    /// The agent records no owner.
    None,
    /// The user pool could not be asked.
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    #[serde(default)]
    pub model_id: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub connection_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub kind: ToolKind,
    #[serde(default)]
    pub category: Option<String>,
    pub is_mutating: bool,
    pub is_elevated: bool,
    pub requires_approval: bool,
    pub skips_approval_in_etl_jobs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolKind {
    BuiltIn,
    /// Runs in the caller's runtime (`parse_pdf`).
    Client,
}

/// Native tool categories that read site data.
const DATA_READ_CATEGORIES: [&str; 4] = ["data_provider", "datasets", "connections", "documents"];
/// The native tool category that reaches outside the site.
const OUTBOUND_CATEGORY: &str = "web";

impl Tool {
    pub fn reads_site_data(&self) -> bool {
        self.category
            .as_deref()
            .is_some_and(|category| DATA_READ_CATEGORIES.contains(&category))
    }

    pub fn is_outbound(&self) -> bool {
        self.category.as_deref() == Some(OUTBOUND_CATEGORY)
    }

    pub fn writes_unreviewed(&self) -> bool {
        self.is_mutating && !self.requires_approval
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConnection {
    pub name: String,
    pub status: McpStatus,
    #[serde(default)]
    pub server_host: Option<String>,
    /// `None`: every tool the server offers.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpStatus {
    Ok,
    Missing,
    NotMcp,
    Invalid,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Delegate {
    pub agent_id: String,
    pub alias: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Team {
    pub mode: String,
    #[serde(default)]
    pub members: Vec<Delegate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    #[serde(default)]
    pub max_run_tree_spend_usd: Option<f64>,
    #[serde(default)]
    pub quota_limit_cents: Option<i64>,
    #[serde(default, deserialize_with = "optional_decimal_text")]
    pub quota_spend_usd: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Runs {
    pub count: u64,
    pub child_run_count: u64,
    pub completed_count: u64,
    pub failed_count: u64,
    pub cancelled_count: u64,
    #[serde(deserialize_with = "decimal_text")]
    pub cost_usd: f64,
    pub unpriced_run_count: u64,
    #[serde(default)]
    pub last_run_at: Option<String>,
}

impl Runs {
    /// Root runs that ended: completed, failed or cancelled.
    pub fn finished(&self) -> u64 {
        self.completed_count + self.failed_count + self.cancelled_count
    }
}

/// Costs come as exact decimal text (`"0.120000"`).
fn decimal_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let text = String::deserialize(deserializer)?;
    text.parse()
        .map_err(|_| serde::de::Error::custom(format!("not a decimal amount: {text:?}")))
}

fn optional_decimal_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(None),
        Some(text) => text
            .parse()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("not a decimal amount: {text:?}"))),
    }
}

//! Which model answers: a Groundzero mode (from chat `/chat/capabilities`)
//! or one of the user's own LLM connections, chosen the way the web app's
//! model picker does (ground-zero/src/pages/home/chatModels.js), and how a
//! choice goes into the run body (gz-chat-eks ChatRequest: `groundzeroMode`
//! with `reasoningEffort`, or `modelProvider`, `modelConnectionName`, and
//! `modelId`).

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::assistant::subagent_settings::{self, Limits};
use crate::cmd::seg;
use crate::http::ApiClient;

/// Connection types that can answer chat (chatModels.js LLM_MODEL_PROVIDERS).
pub const LLM_PROVIDERS: &[&str] = &[
    "ANTHROPIC",
    "AWSBEDROCK",
    "AZUREOPENAI",
    "COHERE",
    "CUSTOMLLM",
    "FIREWORKSAI",
    "GEMINI",
    "HUGGINGFACE",
    "OPENAI",
    "TOGETHERAI",
    "XAI",
];

/// Saved-choice prefix for connections: `conn:OPENAI:my-gpt`.
const CONNECTION_KEY: &str = "conn:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundzeroMode {
    pub value: String,
    pub label: String,
    pub efforts: Vec<String>,
    /// The model's conversation budget in tokens, when the server says.
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConnection {
    pub provider: String,
    pub name: String,
    /// Empty when the connection has no model saved (it can't be used).
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Model {
    Groundzero(GroundzeroMode),
    Connection(LlmConnection),
}

impl Model {
    pub fn label(&self) -> String {
        match self {
            Model::Groundzero(mode) => mode.label.clone(),
            Model::Connection(c) => format!("{} ({})", c.name, c.provider.to_lowercase()),
        }
    }

    /// Stable id for config and `--model`.
    pub fn key(&self) -> String {
        match self {
            Model::Groundzero(mode) => mode.value.clone(),
            Model::Connection(c) => format!("{CONNECTION_KEY}{}:{}", c.provider, c.name),
        }
    }

    pub fn efforts(&self) -> &[String] {
        match self {
            Model::Groundzero(mode) => &mode.efforts,
            Model::Connection(_) => &[],
        }
    }

    /// The conversation budget in tokens; unknown for connections until a
    /// run reports it.
    pub fn context_window(&self) -> Option<u64> {
        match self {
            Model::Groundzero(mode) => mode.context_window,
            Model::Connection(_) => None,
        }
    }
}

/// A model plus the reasoning effort picked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub model: Model,
    pub effort: Option<String>,
}

impl Selection {
    /// `Claude Opus 5 · high`.
    pub fn label(&self) -> String {
        match &self.effort {
            Some(effort) => format!("{} · {effort}", self.model.label()),
            None => self.model.label(),
        }
    }

    /// Adds this choice to a `POST /chat/runs` body.
    pub fn apply_to(&self, body: &mut Value) {
        match &self.model {
            Model::Groundzero(mode) => {
                body["groundzeroMode"] = json!(mode.value);
            }
            Model::Connection(c) => {
                body["modelProvider"] = json!(c.provider);
                body["modelConnectionName"] = json!(c.name);
                if !c.model_id.is_empty() {
                    body["modelId"] = json!(c.model_id);
                }
            }
        }
        if let Some(effort) = &self.effort {
            body["reasoningEffort"] = json!(effort);
        }
    }
}

/// Everything the picker can offer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub modes: Vec<GroundzeroMode>,
    pub default_mode: Option<String>,
    pub default_effort: Option<String>,
    pub connections: Vec<LlmConnection>,
    /// Helper-agent kinds the chat's `task` tool can start.
    pub subagent_profiles: Vec<SubagentProfile>,
    /// The server reviews actions for auto mode (`features.actionReview`).
    pub action_review: bool,
    /// The review reads recent blocks, so the user can authorize one
    /// (`features.actionReviewAuthorizations`).
    pub action_review_authorizations: bool,
    /// The server lets gz run subagents in the background
    /// (`features.backgroundSubagents`).
    pub background_subagents: bool,
    /// The server takes notes for a running turn (`features.liveSteering`).
    pub live_steering: bool,
    /// Every client tool the server's catalog has (`clientTools`); `None`
    /// from a server too old to list them.
    pub client_tools: Option<Vec<String>>,
    /// What the server accepts in `subagentSettings`; `None` when it doesn't
    /// take them (`features.subagentSettings`).
    pub subagent_settings: Option<Limits>,
    /// What the server takes as `clientContext`; `None` when it doesn't
    /// (`features.localContext`).
    pub local_context: Option<crate::assistant::local_context::Limits>,
    /// What the server takes for team skills; `None` when it has none
    /// (`features.teamSkills`).
    pub team_skills: Option<crate::skills::team::TeamLimits>,
}

/// One subagent profile from `/chat/capabilities` `subagentProfiles`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentProfile {
    pub id: String,
    pub label: String,
    pub description: String,
}

/// Subagent profiles from a `/chat/capabilities` reply (empty when the
/// server predates subagents).
pub fn subagent_profiles_from(capabilities: &Value) -> Vec<SubagentProfile> {
    capabilities["subagentProfiles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = row["id"].as_str()?.to_string();
            Some(SubagentProfile {
                label: row["label"].as_str().unwrap_or(&id).to_string(),
                description: row["description"].as_str().unwrap_or_default().to_string(),
                id,
            })
        })
        .collect()
}

impl Catalog {
    pub fn models(&self) -> Vec<Model> {
        self.modes
            .iter()
            .cloned()
            .map(Model::Groundzero)
            .chain(self.connections.iter().cloned().map(Model::Connection))
            .collect()
    }

    /// The server's default, shown when nothing was chosen.
    pub fn default_selection(&self) -> Option<Selection> {
        let value = self.default_mode.as_deref()?;
        let mode = self.modes.iter().find(|m| m.value == value)?.clone();
        Some(Selection {
            model: Model::Groundzero(mode),
            effort: self.default_effort.clone(),
        })
    }

    /// Finds a model by saved key, mode value or label, or connection name
    /// (case-insensitive). The effort is kept only if the model offers it.
    pub fn resolve(&self, wanted: &str, effort: Option<&str>) -> Option<Selection> {
        let needle = wanted.trim().to_lowercase();
        let model = self.models().into_iter().find(|m| {
            m.key().to_lowercase() == needle
                || m.label().to_lowercase() == needle
                || matches!(m, Model::Groundzero(g) if g.label.to_lowercase() == needle)
                || matches!(m, Model::Connection(c) if c.name.to_lowercase() == needle)
        })?;
        let effort = effort
            .filter(|e| model.efforts().iter().any(|x| x == e))
            .map(str::to_string);
        Some(Selection { model, effort })
    }
}

/// Groundzero modes from a `/chat/capabilities` reply.
pub fn modes_from(capabilities: &Value) -> Vec<GroundzeroMode> {
    capabilities["groundzeroModes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(GroundzeroMode {
                value: m["value"].as_str()?.to_string(),
                label: m["label"]
                    .as_str()
                    .unwrap_or(m["value"].as_str()?)
                    .to_string(),
                efforts: m["reasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect(),
                context_window: m["contextWindowTokens"].as_u64(),
            })
        })
        .collect()
}

/// The model id saved on a connection row (connectionConfig.js
/// getConnectionModelId): top-level fields first, then its `config`.
pub fn connection_model_id(row: &Value) -> String {
    let config = match &row["config"] {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    };
    ["modelId", "model_id", "model", "deploymentName"]
        .iter()
        .find_map(|key| row[key].as_str().or(config[key].as_str()))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// LLM connections from `/connection` rows: filtered to chat-capable types,
/// deduped by provider + name, sorted by name then provider.
pub fn connections_from(rows: &[Value]) -> Vec<LlmConnection> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<LlmConnection> = rows
        .iter()
        .filter_map(|row| {
            let provider = row["type"].as_str()?.to_uppercase();
            let name = row["Name"]
                .as_str()
                .or(row["name"].as_str())?
                .trim()
                .to_string();
            (LLM_PROVIDERS.contains(&provider.as_str()) && !name.is_empty()).then(|| {
                LlmConnection {
                    model_id: connection_model_id(row),
                    provider,
                    name,
                }
            })
        })
        .filter(|c| seen.insert(format!("{}:{}", c.provider, c.name)))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.provider.cmp(&b.provider)));
    out
}

/// Loads the picker's lists. Connections without a model id in the list are
/// looked up one by one, as the web app does.
pub async fn load(http: &ApiClient) -> Result<Catalog> {
    let capabilities = http
        .request_json("chat", Method::GET, "/chat/capabilities", &[], None)
        .await
        .context("cannot load the model list")?;
    let rows = http
        .request_json("connections", Method::GET, "/connection", &[], None)
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let mut connections = connections_from(&rows);
    for connection in connections.iter_mut().filter(|c| c.model_id.is_empty()) {
        let Some(id) = rows
            .iter()
            .find(|r| r["Name"].as_str().or(r["name"].as_str()) == Some(connection.name.as_str()))
            .and_then(|r| r["Id"].as_i64().or(r["id"].as_i64()))
        else {
            continue;
        };
        let path = format!("/connection/{}", seg(&id.to_string()));
        if let Ok(detail) = http
            .request_json("connections", Method::GET, &path, &[], None)
            .await
        {
            connection.model_id = connection_model_id(&detail);
        }
    }
    Ok(Catalog {
        modes: modes_from(&capabilities),
        default_mode: capabilities["defaultGroundzeroMode"]
            .as_str()
            .map(str::to_string),
        default_effort: capabilities["defaultReasoningEffort"]
            .as_str()
            .map(str::to_string),
        connections,
        subagent_profiles: subagent_profiles_from(&capabilities),
        action_review: capabilities["features"]["actionReview"].as_bool() == Some(true),
        action_review_authorizations: capabilities["features"]["actionReviewAuthorizations"]
            .as_bool()
            == Some(true),
        background_subagents: capabilities["features"]["backgroundSubagents"].as_bool()
            == Some(true),
        live_steering: capabilities["features"]["liveSteering"].as_bool() == Some(true),
        client_tools: capabilities["clientTools"].as_array().map(|names| {
            names
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        }),
        subagent_settings: subagent_settings::limits_from(&capabilities),
        local_context: crate::assistant::local_context::limits_from(&capabilities),
        team_skills: crate::skills::team::limits_from(&capabilities),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog {
            modes: modes_from(&json!({"groundzeroModes": [
                {"value": "opus", "label": "Claude Opus 5", "reasoningEfforts": ["low", "high"], "contextWindowTokens": 872000},
                {"value": "sonnet55", "label": "Claude Sonnet 5.5", "reasoningEfforts": ["low"]},
            ]})),
            default_mode: Some("sonnet55".into()),
            default_effort: Some("low".into()),
            connections: connections_from(&[
                json!({"Name": "zeta-gpt", "type": "OPENAI", "config": "{\"model\":\"gpt-5\"}"}),
                json!({"Name": "alpha", "type": "anthropic", "modelId": "claude-x"}),
                json!({"Name": "alpha", "type": "ANTHROPIC", "modelId": "dup"}),
                json!({"Name": "chat-store", "type": "AICHATSTORAGE"}),
                json!({"Name": "s3", "type": "AWSS3"}),
            ]),
            subagent_profiles: Vec::new(),
            action_review: false,
            action_review_authorizations: false,
            background_subagents: false,
            live_steering: false,
            client_tools: None,
            subagent_settings: None,
            local_context: None,
            team_skills: None,
        }
    }

    #[test]
    fn reads_the_context_window_when_listed() {
        let c = catalog();
        assert_eq!(c.modes[0].context_window, Some(872_000));
        assert_eq!(c.modes[1].context_window, None);
        assert_eq!(
            Model::Connection(c.connections[0].clone()).context_window(),
            None
        );
    }

    #[test]
    fn reads_subagent_profiles() {
        let profiles = subagent_profiles_from(&json!({"subagentProfiles": [
            {"id": "analyst", "label": "Analyst", "description": "lakehouse reads"},
            {"label": "no id"},
        ]}));
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].label, "Analyst");
        assert!(subagent_profiles_from(&json!({})).is_empty());
    }

    #[test]
    fn builds_connections_like_the_web_app() {
        let c = catalog().connections;
        assert_eq!(c.len(), 2, "filtered to LLM types and deduped: {c:?}");
        assert_eq!(
            (c[0].name.as_str(), c[0].model_id.as_str()),
            ("alpha", "claude-x")
        );
        assert_eq!(
            (c[1].provider.as_str(), c[1].model_id.as_str()),
            ("OPENAI", "gpt-5")
        );
    }

    #[test]
    fn groundzero_choices_send_mode_and_effort() {
        let selection = catalog().resolve("Claude Opus 5", Some("high")).unwrap();
        let mut body = json!({});
        selection.apply_to(&mut body);
        assert_eq!(
            body,
            json!({"groundzeroMode": "opus", "reasoningEffort": "high"})
        );
        assert_eq!(selection.label(), "Claude Opus 5 · high");
    }

    #[test]
    fn connection_choices_send_provider_name_and_model() {
        let selection = catalog().resolve("zeta-gpt", Some("high")).unwrap();
        assert_eq!(selection.effort, None, "connections have no effort levels");
        let mut body = json!({"groundzeroMode": "x"});
        body.as_object_mut().unwrap().remove("groundzeroMode");
        selection.apply_to(&mut body);
        assert_eq!(
            body,
            json!({"modelProvider": "OPENAI", "modelConnectionName": "zeta-gpt", "modelId": "gpt-5"})
        );
        assert_eq!(
            catalog()
                .resolve(&selection.model.key(), None)
                .unwrap()
                .model,
            selection.model
        );
    }

    #[test]
    fn resolves_by_key_value_or_label_and_reports_the_default() {
        let c = catalog();
        assert_eq!(c.resolve("OPUS", None).unwrap().model.key(), "opus");
        assert!(c.resolve("nope", None).is_none());
        assert_eq!(
            c.default_selection().unwrap().label(),
            "Claude Sonnet 5.5 · low"
        );
        assert_eq!(
            c.resolve("opus", Some("max")).unwrap().effort,
            None,
            "unsupported effort dropped"
        );
    }
}

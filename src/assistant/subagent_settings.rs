//! The user's subagent settings (`/subagents model|effort|context|max|parallel`):
//! which model the chat's helper agents run on, how large their context
//! grows before they summarize, how many a run may start, and how many run
//! at once. They go out as `subagentSettings` on every chat run when the
//! server advertises `features.subagentSettings` (gz-chat-eks ChatRequest),
//! and are remembered per profile (`code_subagent_settings`).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::assistant::compaction;
use crate::assistant::models::{Catalog, GroundzeroMode, Model, Selection};

/// Clears the subagent model, so subagents run on the chat's model.
pub const MAIN_WORD: &str = "main";
/// Clears a count limit.
pub const OFF_WORD: &str = "off";

/// What the server accepts (`subagentSettingsLimits`); the defaults are the
/// contract's, for servers that advertise the feature without the limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub context_step: u64,
    pub min_context: u64,
    pub max_subagents: u32,
    pub max_concurrent: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            context_step: 100_000,
            min_context: 100_000,
            max_subagents: 50,
            max_concurrent: 20,
        }
    }
}

/// The limits from a `/chat/capabilities` reply; `None` when the server
/// doesn't take subagent settings.
pub fn limits_from(capabilities: &Value) -> Option<Limits> {
    if capabilities["features"]["subagentSettings"].as_bool() != Some(true) {
        return None;
    }
    let advertised = &capabilities["subagentSettingsLimits"];
    let fallback = Limits::default();
    let positive = |key: &str| advertised[key].as_u64().filter(|n| *n > 0);
    let count = |key: &str| positive(key).and_then(|n| u32::try_from(n).ok());
    Some(Limits {
        context_step: positive("contextTokensStep").unwrap_or(fallback.context_step),
        min_context: positive("minContextTokens").unwrap_or(fallback.min_context),
        max_subagents: count("maxSubagents").unwrap_or(fallback.max_subagents),
        max_concurrent: count("maxConcurrent").unwrap_or(fallback.max_concurrent),
    })
}

/// What the user chose; every field is optional and unset means the
/// server's behavior (the main chat's model, effort and context; no caps).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSettings {
    /// A Groundzero mode offered in chat (`groundzeroMode` values).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Where each subagent summarizes older messages, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// Subagents one run may start, nested ones included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_subagents: Option<u32>,
    /// Subagents running at the same time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
}

impl SubagentSettings {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The run body's `subagentSettings`, or `None` when nothing is set.
    /// `main` is the chat's own model: on a connection the server refuses
    /// `model` and `reasoningEffort`, and with no subagent model the effort
    /// must be one the main model offers, so neither is sent then.
    pub fn request_body(&self, main: Option<&Selection>) -> Option<Value> {
        let on_connection = matches!(
            main,
            Some(Selection {
                model: Model::Connection(_),
                ..
            })
        );
        let mut body = Map::new();
        if !on_connection {
            if let Some(model) = &self.model {
                body.insert("model".into(), json!(model));
            }
            let effort_fits = |effort: &String| match (&self.model, main) {
                (None, Some(main)) => main.model.efforts().contains(effort),
                _ => true,
            };
            if let Some(effort) = self.reasoning_effort.as_ref().filter(|e| effort_fits(e)) {
                body.insert("reasoningEffort".into(), json!(effort));
            }
        }
        if let Some(tokens) = self.context_tokens {
            body.insert("contextTokens".into(), json!(tokens));
        }
        if let Some(count) = self.max_subagents {
            body.insert("maxSubagents".into(), json!(count));
        }
        if let Some(count) = self.max_concurrent {
            body.insert("maxConcurrent".into(), json!(count));
        }
        (!body.is_empty()).then_some(Value::Object(body))
    }

    /// Forgets a saved model the server no longer offers (and an effort
    /// that model no longer has), which the server would refuse; returns
    /// what was dropped, to tell the user.
    pub fn drop_unavailable(&mut self, catalog: &Catalog) -> Option<String> {
        let wanted = self.model.clone()?;
        match find_mode(catalog, &wanted) {
            None => {
                self.model = None;
                self.reasoning_effort = None;
                Some(format!(
                    "The subagent model `{wanted}` is no longer offered; subagents use the chat's model"
                ))
            }
            Some(mode) => {
                let effort = self.reasoning_effort.clone()?;
                if mode.efforts.contains(&effort) {
                    return None;
                }
                self.reasoning_effort = None;
                Some(format!(
                    "{} no longer offers effort `{effort}`; subagents use its default effort",
                    mode.label
                ))
            }
        }
    }

    /// `(setting, value)` rows for the `/subagents` panel.
    pub fn rows(&self, catalog: Option<&Catalog>) -> Vec<(&'static str, String)> {
        let model = match &self.model {
            Some(value) => catalog
                .and_then(|c| find_mode(c, value))
                .map(|m| m.label.clone())
                .unwrap_or_else(|| value.clone()),
            None => "the chat's model".into(),
        };
        let limit = |count: Option<u32>| count.map_or("no limit".to_string(), |n| n.to_string());
        vec![
            ("model", model),
            (
                "effort",
                self.reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "default".into()),
            ),
            (
                "context",
                self.context_tokens
                    .map_or("the chat's".to_string(), crate::subagents::tokens_label),
            ),
            ("max per run", limit(self.max_subagents)),
            ("parallel", limit(self.max_concurrent)),
        ]
    }
}

/// The setting a `/subagents <setting> <value>` changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Model,
    Effort,
    Context,
    Max,
    Parallel,
}

impl Setting {
    pub fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "model" => Self::Model,
            "effort" => Self::Effort,
            "context" => Self::Context,
            "max" => Self::Max,
            "parallel" => Self::Parallel,
            _ => return None,
        })
    }
}

/// What a value is checked against: the model list (absent while loading),
/// the chat's own model, and the server's limits.
pub struct Choices<'a> {
    pub catalog: Option<&'a Catalog>,
    pub main: Option<&'a Selection>,
    pub limits: Limits,
}

/// New settings after a change, plus a note when another setting had to
/// change with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated {
    pub settings: SubagentSettings,
    pub note: Option<String>,
}

/// Applies `/subagents <setting> <value>` to `current`, or says what the
/// valid choices are.
pub fn update(
    current: &SubagentSettings,
    setting: Setting,
    value: &str,
    choices: &Choices,
) -> Result<Updated> {
    let value = value.trim();
    let mut settings = current.clone();
    let mut note = None;
    match setting {
        Setting::Model => note = set_model(&mut settings, value, choices)?,
        Setting::Effort => settings.reasoning_effort = effort(&settings, value, choices)?,
        Setting::Context => settings.context_tokens = context(value, &choices.limits)?,
        Setting::Max => {
            settings.max_subagents = count(value, choices.limits.max_subagents, "max")?;
        }
        Setting::Parallel => {
            settings.max_concurrent = count(value, choices.limits.max_concurrent, "parallel")?;
        }
    }
    Ok(Updated { settings, note })
}

/// A mode offered in chat, by value or label (case-insensitive).
fn find_mode<'a>(catalog: &'a Catalog, wanted: &str) -> Option<&'a GroundzeroMode> {
    let wanted = wanted.trim().to_lowercase();
    catalog
        .modes
        .iter()
        .find(|m| m.value.to_lowercase() == wanted || m.label.to_lowercase() == wanted)
}

fn loaded<'a>(choices: &Choices<'a>) -> Result<&'a Catalog> {
    choices
        .catalog
        .context("the model list is still loading (or couldn't load); try again in a moment")
}

fn set_model(
    settings: &mut SubagentSettings,
    value: &str,
    choices: &Choices,
) -> Result<Option<String>> {
    if value.eq_ignore_ascii_case(MAIN_WORD) {
        settings.model = None;
        return Ok(None);
    }
    let catalog = loaded(choices)?;
    let Some(mode) = find_mode(catalog, value) else {
        let offered: Vec<&str> = catalog.modes.iter().map(|m| m.value.as_str()).collect();
        bail!(
            "`{value}` isn't a model offered in chat; choose {}, or `{MAIN_WORD}` for the chat's model",
            offered.join(", ")
        );
    };
    settings.model = Some(mode.value.clone());
    let Some(effort) = settings.reasoning_effort.clone() else {
        return Ok(None);
    };
    if mode.efforts.contains(&effort) {
        return Ok(None);
    }
    settings.reasoning_effort = None;
    Ok(Some(format!(
        "{} doesn't offer effort `{effort}`, so subagents use its default effort",
        mode.label
    )))
}

/// The model whose efforts apply: the subagent model, else the chat's.
fn effort_model(settings: &SubagentSettings, choices: &Choices) -> Result<Model> {
    let catalog = loaded(choices)?;
    if let Some(value) = &settings.model {
        return find_mode(catalog, value)
            .cloned()
            .map(Model::Groundzero)
            .with_context(|| format!("the subagent model `{value}` is no longer offered; choose another with /subagents model"));
    }
    choices
        .main
        .cloned()
        .or_else(|| catalog.default_selection())
        .map(|selection| selection.model)
        .context(
            "the chat's model isn't known yet; choose a subagent model first with /subagents model",
        )
}

fn effort(settings: &SubagentSettings, value: &str, choices: &Choices) -> Result<Option<String>> {
    if value.eq_ignore_ascii_case(compaction::DEFAULT_WORD) {
        return Ok(None);
    }
    let model = effort_model(settings, choices)?;
    if let Model::Connection(_) = model {
        bail!("the chat runs on your connection, which has no effort levels; choose a subagent model first with /subagents model");
    }
    let efforts = model.efforts();
    if efforts.is_empty() {
        bail!(
            "{} has no reasoning-effort levels; use `{}`",
            model.label(),
            compaction::DEFAULT_WORD
        );
    }
    match efforts.iter().find(|e| e.eq_ignore_ascii_case(value)) {
        Some(effort) => Ok(Some(effort.clone())),
        None => bail!(
            "{} offers effort {} or `{}`, not `{value}`",
            model.label(),
            efforts.join(", "),
            compaction::DEFAULT_WORD
        ),
    }
}

fn context(value: &str, limits: &Limits) -> Result<Option<u64>> {
    if compaction::is_default(value) {
        return Ok(None);
    }
    let step = limits.context_step;
    let accepted = compaction::parse_tokens(value)
        .filter(|tokens| *tokens >= limits.min_context && tokens % step == 0);
    match accepted {
        Some(tokens) => Ok(Some(tokens)),
        None => {
            let (step, min) = (
                crate::subagents::tokens_label(step),
                crate::subagents::tokens_label(limits.min_context),
            );
            bail!(
                "context takes tokens in {step} steps from {min} (e.g. 200K, 300K), or `{}`, not `{value}`",
                compaction::DEFAULT_WORD
            )
        }
    }
}

fn count(value: &str, top: u32, setting: &str) -> Result<Option<u32>> {
    if value.eq_ignore_ascii_case(OFF_WORD) {
        return Ok(None);
    }
    match value.parse::<u32>().ok().filter(|n| (1..=top).contains(n)) {
        Some(n) => Ok(Some(n)),
        None => bail!("{setting} takes a number from 1 to {top}, or `{OFF_WORD}`, not `{value}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::models::{modes_from, LlmConnection};

    fn catalog() -> Catalog {
        Catalog {
            modes: modes_from(&json!({"groundzeroModes": [
                {"value": "opus", "label": "Claude Opus 5", "reasoningEfforts": ["low", "high"]},
                {"value": "haiku55", "label": "Claude Haiku 5.5", "reasoningEfforts": ["low"]},
                {"value": "flash", "label": "Flash"},
            ]})),
            default_mode: Some("opus".into()),
            ..Catalog::default()
        }
    }

    fn connection() -> Selection {
        Selection {
            model: Model::Connection(LlmConnection {
                provider: "OPENAI".into(),
                name: "my-gpt".into(),
                model_id: "gpt-5".into(),
            }),
            effort: None,
        }
    }

    fn apply(current: &SubagentSettings, setting: Setting, value: &str) -> Result<Updated> {
        let catalog = catalog();
        let choices = Choices {
            catalog: Some(&catalog),
            main: None,
            limits: Limits::default(),
        };
        update(current, setting, value, &choices)
    }

    fn error(current: &SubagentSettings, setting: Setting, value: &str) -> String {
        apply(current, setting, value).unwrap_err().to_string()
    }

    #[test]
    fn model_must_be_a_chat_mode_and_names_the_choices() {
        let set = apply(
            &SubagentSettings::default(),
            Setting::Model,
            "Claude Haiku 5.5",
        )
        .unwrap();
        assert_eq!(set.settings.model.as_deref(), Some("haiku55"));
        let err = error(&SubagentSettings::default(), Setting::Model, "gpt-9");
        assert!(
            err.contains("opus, haiku55, flash") && err.contains("`main`"),
            "{err}"
        );
        let cleared = apply(&set.settings, Setting::Model, "MAIN").unwrap();
        assert_eq!(cleared.settings.model, None);
    }

    #[test]
    fn switching_model_drops_an_effort_it_lacks() {
        let current = SubagentSettings {
            reasoning_effort: Some("high".into()),
            ..SubagentSettings::default()
        };
        let set = apply(&current, Setting::Model, "haiku55").unwrap();
        assert_eq!(set.settings.reasoning_effort, None);
        assert!(set.note.unwrap().contains("doesn't offer effort `high`"));
        let kept = apply(&current, Setting::Model, "opus").unwrap();
        assert_eq!(kept.settings.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(kept.note, None);
    }

    #[test]
    fn effort_is_checked_against_the_subagent_model_else_the_chats() {
        let haiku = SubagentSettings {
            model: Some("haiku55".into()),
            ..SubagentSettings::default()
        };
        let err = error(&haiku, Setting::Effort, "high");
        assert!(err.contains("Claude Haiku 5.5 offers effort low"), "{err}");
        // No subagent model: the chat's model (the default, opus) decides.
        let set = apply(&SubagentSettings::default(), Setting::Effort, "HIGH").unwrap();
        assert_eq!(set.settings.reasoning_effort.as_deref(), Some("high"));
        let flash = SubagentSettings {
            model: Some("flash".into()),
            ..SubagentSettings::default()
        };
        assert!(error(&flash, Setting::Effort, "low").contains("no reasoning-effort levels"));
        let catalog = catalog();
        let main = connection();
        let on_connection = Choices {
            catalog: Some(&catalog),
            main: Some(&main),
            limits: Limits::default(),
        };
        let err = update(
            &SubagentSettings::default(),
            Setting::Effort,
            "low",
            &on_connection,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("choose a subagent model first"), "{err}");
    }

    #[test]
    fn model_and_effort_wait_for_the_model_list() {
        let loading = Choices {
            catalog: None,
            main: None,
            limits: Limits::default(),
        };
        let err = update(
            &SubagentSettings::default(),
            Setting::Model,
            "opus",
            &loading,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("still loading"), "{err}");
        let counts = update(&SubagentSettings::default(), Setting::Max, "6", &loading).unwrap();
        assert_eq!(
            counts.settings.max_subagents,
            Some(6),
            "limits need no list"
        );
    }

    #[test]
    fn context_takes_steps_from_the_minimum() {
        let none = SubagentSettings::default();
        assert_eq!(
            apply(&none, Setting::Context, "200k")
                .unwrap()
                .settings
                .context_tokens,
            Some(200_000)
        );
        assert_eq!(
            apply(&none, Setting::Context, "300000")
                .unwrap()
                .settings
                .context_tokens,
            Some(300_000)
        );
        assert_eq!(
            apply(&none, Setting::Context, "default")
                .unwrap()
                .settings
                .context_tokens,
            None
        );
        for bad in ["150k", "0", "50000", "lots", "-100k"] {
            let err = error(&none, Setting::Context, bad);
            assert!(err.contains("100K steps from 100K"), "{bad}: {err}");
        }
        let catalog = catalog();
        let coarse = Choices {
            catalog: Some(&catalog),
            main: None,
            limits: Limits {
                context_step: 200_000,
                min_context: 400_000,
                ..Limits::default()
            },
        };
        assert!(update(&none, Setting::Context, "200k", &coarse).is_err());
        assert!(update(&none, Setting::Context, "300k", &coarse).is_err());
        assert!(update(&none, Setting::Context, "600k", &coarse).is_ok());
    }

    #[test]
    fn counts_stay_within_the_servers_limits() {
        let none = SubagentSettings::default();
        assert_eq!(
            apply(&none, Setting::Max, "50")
                .unwrap()
                .settings
                .max_subagents,
            Some(50)
        );
        assert_eq!(
            apply(&none, Setting::Parallel, "3")
                .unwrap()
                .settings
                .max_concurrent,
            Some(3)
        );
        assert!(error(&none, Setting::Max, "51").contains("from 1 to 50"));
        assert!(error(&none, Setting::Parallel, "21").contains("from 1 to 20"));
        assert!(error(&none, Setting::Parallel, "0").contains("`off`"));
        let capped = SubagentSettings {
            max_concurrent: Some(3),
            ..SubagentSettings::default()
        };
        assert_eq!(
            apply(&capped, Setting::Parallel, "off")
                .unwrap()
                .settings
                .max_concurrent,
            None
        );
    }

    #[test]
    fn limits_come_from_capabilities_only_when_advertised() {
        assert_eq!(
            limits_from(&json!({"features": {"backgroundSubagents": true}})),
            None
        );
        let limits = limits_from(&json!({
            "features": {"subagentSettings": true},
            "subagentSettingsLimits": {"contextTokensStep": 50000, "minContextTokens": 100000, "maxSubagents": 10, "maxConcurrent": 4}
        }))
        .unwrap();
        assert_eq!(
            (
                limits.context_step,
                limits.max_subagents,
                limits.max_concurrent
            ),
            (50_000, 10, 4)
        );
        assert_eq!(
            limits_from(&json!({"features": {"subagentSettings": true}})),
            Some(Limits::default())
        );
    }

    #[test]
    fn the_body_carries_only_what_is_set() {
        assert_eq!(SubagentSettings::default().request_body(None), None);
        let all = SubagentSettings {
            model: Some("haiku55".into()),
            reasoning_effort: Some("low".into()),
            context_tokens: Some(200_000),
            max_subagents: Some(6),
            max_concurrent: Some(3),
        };
        assert_eq!(
            all.request_body(None),
            Some(
                json!({"model": "haiku55", "reasoningEffort": "low", "contextTokens": 200000, "maxSubagents": 6, "maxConcurrent": 3})
            )
        );
        assert_eq!(
            all.request_body(Some(&connection())),
            Some(json!({"contextTokens": 200000, "maxSubagents": 6, "maxConcurrent": 3})),
            "a connection chat refuses model and effort"
        );
        let effort_only = SubagentSettings {
            reasoning_effort: Some("high".into()),
            ..SubagentSettings::default()
        };
        let haiku = catalog().resolve("haiku55", None);
        assert_eq!(
            effort_only.request_body(haiku.as_ref()),
            None,
            "an effort the chat's model lacks isn't sent"
        );
        let opus = catalog().resolve("opus", None);
        assert_eq!(
            effort_only.request_body(opus.as_ref()),
            Some(json!({"reasoningEffort": "high"}))
        );
    }

    #[test]
    fn saved_choices_the_server_dropped_are_forgotten() {
        let mut gone = SubagentSettings {
            model: Some("retired".into()),
            reasoning_effort: Some("low".into()),
            max_concurrent: Some(2),
            ..SubagentSettings::default()
        };
        assert!(gone
            .drop_unavailable(&catalog())
            .unwrap()
            .contains("`retired`"));
        assert_eq!(
            (gone.model, gone.reasoning_effort, gone.max_concurrent),
            (None, None, Some(2))
        );
        let mut effort = SubagentSettings {
            model: Some("haiku55".into()),
            reasoning_effort: Some("high".into()),
            ..SubagentSettings::default()
        };
        assert!(effort.drop_unavailable(&catalog()).is_some());
        assert_eq!(effort.model.as_deref(), Some("haiku55"));
        assert_eq!(effort.reasoning_effort, None);
        let mut fine = SubagentSettings {
            model: Some("opus".into()),
            reasoning_effort: Some("high".into()),
            ..SubagentSettings::default()
        };
        assert_eq!(fine.drop_unavailable(&catalog()), None);
    }
}

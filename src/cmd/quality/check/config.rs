//! The qualipilot config `gz quality check` sends: `--config` as given, with
//! each `--severity CHECK=LEVEL` set in `checks.severity_overrides`
//! (qualipilot models/config.py `CheckConfig.severity_overrides`). An
//! override applies when the check finds something; a clean check stays ok.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

use crate::cmd::read_text_arg;

/// qualipilot's `BuiltInCheckName`, the checks an override may name.
const CHECK_NAMES: [&str; 11] = [
    "dataset_contract",
    "missing_values",
    "duplicates",
    "data_types",
    "outliers",
    "ranges",
    "cardinality",
    "freshness",
    "linkage",
    "quality_contract",
    "iceberg_table",
];
const LEVELS: [&str; 3] = ["ok", "warn", "error"];

/// `None` when neither flag was given: the runtime's defaults apply.
pub fn qualipilot_config(config: Option<&str>, severities: &[String]) -> Result<Option<Value>> {
    let mut config = match config {
        Some(raw) => parse_config(&read_text_arg(raw)?)?,
        None if severities.is_empty() => return Ok(None),
        None => Value::Object(Map::new()),
    };
    for severity in severities {
        let (check, level) = parse_severity(severity)?;
        overrides(&mut config)?.insert(check.to_string(), Value::from(level));
    }
    Ok(Some(config))
}

/// JSON or YAML (YAML reads JSON too), and an object at the top.
fn parse_config(text: &str) -> Result<Value> {
    let value: Value = serde_yaml::from_str(text)
        .context("--config is neither JSON nor YAML; pass a qualipilot config")?;
    if !value.is_object() {
        bail!("--config must be a qualipilot config object, e.g. {{\"checks\": {{\"freshness\": true}}}}");
    }
    Ok(value)
}

fn parse_severity(text: &str) -> Result<(&str, &str)> {
    let Some((check, level)) = text.split_once('=') else {
        bail!("write --severity as CHECK=LEVEL, e.g. outliers=error (got '{text}')");
    };
    let (check, level) = (check.trim(), level.trim());
    if !CHECK_NAMES.contains(&check) {
        bail!(
            "--severity: no check '{check}'; checks are {}",
            CHECK_NAMES.join(", ")
        );
    }
    if !LEVELS.contains(&level) {
        bail!("--severity {check}: the level must be ok, warn or error (got '{level}')");
    }
    Ok((check, level))
}

/// `checks.severity_overrides`, made when missing.
fn overrides(config: &mut Value) -> Result<&mut Map<String, Value>> {
    let mut node = config;
    for key in ["checks", "severity_overrides"] {
        let object = node
            .as_object_mut()
            .context("--config: `checks` and `checks.severity_overrides` must be objects")?;
        node = object
            .entry(key)
            .or_insert_with(|| Value::Object(Map::new()));
    }
    node.as_object_mut()
        .context("--config: `checks.severity_overrides` must be an object")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_flags_leave_the_runtime_defaults() {
        assert_eq!(qualipilot_config(None, &[]).unwrap(), None);
    }

    #[test]
    fn severities_become_overrides() {
        let config =
            qualipilot_config(None, &["outliers=error".into(), " ranges = warn".into()]).unwrap();
        assert_eq!(
            config,
            Some(
                json!({"checks": {"severity_overrides": {"outliers": "error", "ranges": "warn"}}})
            )
        );
    }

    #[test]
    fn severities_override_the_config_file_and_keep_the_rest() {
        let yaml = "checks:\n  freshness: true\n  severity_overrides:\n    outliers: warn\n    duplicates: error\nengine: spark\n";
        let config = qualipilot_config(Some(yaml), &["outliers=error".into()]).unwrap();
        assert_eq!(
            config,
            Some(json!({
                "checks": {"freshness": true, "severity_overrides": {"outliers": "error", "duplicates": "error"}},
                "engine": "spark",
            }))
        );
        let from_json = qualipilot_config(Some(r#"{"checks": {"ranges": false}}"#), &[]).unwrap();
        assert_eq!(from_json, Some(json!({"checks": {"ranges": false}})));
    }

    #[test]
    fn unknown_checks_levels_and_shapes_are_refused() {
        let refused = |config: Option<&str>, severity: &str| {
            qualipilot_config(config, &[severity.to_string()])
                .unwrap_err()
                .to_string()
        };
        assert!(refused(None, "outlier=error").contains("no check 'outlier'"));
        assert!(refused(None, "outliers=fatal").contains("ok, warn or error"));
        assert!(refused(None, "outliers").contains("CHECK=LEVEL"));
        assert!(refused(Some("[1, 2]"), "outliers=error").contains("config object"));
        assert!(refused(Some(r#"{"checks": []}"#), "outliers=error").contains("must be objects"));
    }
}

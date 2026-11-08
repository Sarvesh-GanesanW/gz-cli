//! Request templates filled from the rows on screen.
//!
//! A placeholder is `{…}` holding one of:
//! - a position, `{0}`, `{1}`: the ids picked along the drill trail, then
//!   the selected row's id (the original form);
//! - a field path, `{Id}`, `{config.siteName}`: looked up in the current
//!   row first, then its parent rows, nearest first;
//! - a variable, `{$site}`: values the app knows (the signed-in site);
//! - alternatives, `{config.siteName|$site}`: the first that resolves.
//!
//! Values are percent-encoded in paths and JSON-escaped in bodies. JSON
//! braces (`{"a": 1}`) are never placeholders: a placeholder holds only
//! letters, digits, `_`, `.`, `$` and `|`.

use std::fmt;

use serde_json::Value;

use crate::cmd::seg;
use crate::tui::fields;

/// Everything a template can read.
#[derive(Debug, Default, Clone)]
pub struct Scope<'a> {
    /// Positional ids for `{0}`, `{1}`, ….
    pub ids: Vec<String>,
    /// Rows searched for field placeholders, nearest first.
    pub rows: Vec<&'a Value>,
    /// `$name` variables.
    pub vars: Vec<(&'static str, String)>,
}

impl<'a> Scope<'a> {
    pub fn with_ids(ids: &[String]) -> Self {
        Self {
            ids: ids.to_vec(),
            ..Self::default()
        }
    }

    /// Searches `row` before every row already in scope.
    pub fn nearest(mut self, row: &'a Value) -> Self {
        self.rows.insert(0, row);
        self
    }

    pub fn var(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.vars.push((name, value.into()));
        self
    }

    fn resolve(&self, token: &str) -> Option<Value> {
        token.split('|').find_map(|choice| self.resolve_one(choice))
    }

    fn resolve_one(&self, token: &str) -> Option<Value> {
        if let Some(name) = token.strip_prefix('$') {
            return self
                .vars
                .iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| Value::String(value.clone()));
        }
        if let Ok(index) = token.parse::<usize>() {
            return self.ids.get(index).cloned().map(Value::String);
        }
        self.rows
            .iter()
            .filter_map(|row| fields::lookup(row, token))
            .find(|v| !v.is_null())
            .cloned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// A placeholder resolved to nothing.
    Missing(String),
    /// The filled body is not valid JSON.
    BadBody(String),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::Missing(field) => write!(f, "this row has no `{field}`"),
            TemplateError::BadBody(err) => write!(f, "the request body is not valid JSON ({err})"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Percent-encoded, for URL paths and query values.
    Path,
    /// JSON-escaped (without quotes), for request bodies.
    Json,
}

/// Fills every placeholder in `template`.
pub fn fill(template: &str, scope: &Scope, encoding: Encoding) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match placeholder(after) {
            Some(token) => {
                let value = scope
                    .resolve(token)
                    .ok_or_else(|| TemplateError::Missing(token.to_string()))?;
                out.push_str(&encode(&value, encoding));
                rest = &after[token.len() + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Fills a JSON body template and parses it.
pub fn fill_json(template: &str, scope: &Scope) -> Result<Value, TemplateError> {
    let text = fill(template, scope, Encoding::Json)?;
    serde_json::from_str(&text).map_err(|e| TemplateError::BadBody(e.to_string()))
}

/// The placeholder token at the start of `text` (just after a `{`).
fn placeholder(text: &str) -> Option<&str> {
    let end = text.find('}')?;
    let token = &text[..end];
    let is_token_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '|');
    (!token.is_empty() && token.chars().all(is_token_char)).then_some(token)
}

fn encode(value: &Value, encoding: Encoding) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match encoding {
        Encoding::Path => seg(&text),
        Encoding::Json if value.is_string() => {
            let quoted = serde_json::to_string(&text).unwrap_or_default();
            quoted[1..quoted.len() - 1].to_string()
        }
        Encoding::Json => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn positional_ids_still_work() {
        let scope = Scope::with_ids(&["wh".into(), "my db".into()]);
        let path = fill("/lakehouse/schema/{0}/{1}", &scope, Encoding::Path).unwrap();
        assert_eq!(path, "/lakehouse/schema/wh/my%20db");
    }

    #[test]
    fn named_fields_come_from_the_row_then_its_parents() {
        let parent = json!({"projectId": "p 1", "name": "parent"});
        let row = json!({"id": 7, "name": "job"});
        let scope = Scope::default().nearest(&parent).nearest(&row);
        let path = fill(
            "/etl/{projectId}/jobs/{id}?n={name}",
            &scope,
            Encoding::Path,
        )
        .unwrap();
        assert_eq!(path, "/etl/p%201/jobs/7?n=job");
    }

    #[test]
    fn nested_paths_alternatives_and_variables() {
        let row = json!({"Name": "LT", "config": {}});
        let scope = Scope::default().nearest(&row).var("site", "preview");
        let body = fill_json(
            r#"{"connectionName":"{Name}","site":"{config.siteName|$site}"}"#,
            &scope,
        )
        .unwrap();
        assert_eq!(body, json!({"connectionName": "LT", "site": "preview"}));
    }

    #[test]
    fn bodies_escape_strings_and_keep_numbers_bare() {
        let row = json!({"name": "a\"b", "limit": 10, "on": true});
        let scope = Scope::default().nearest(&row);
        let body = fill_json(r#"{"n":"{name}","limit":{limit},"on":{on}}"#, &scope).unwrap();
        assert_eq!(body, json!({"n": "a\"b", "limit": 10, "on": true}));
    }

    #[test]
    fn missing_fields_are_reported_by_name() {
        let row = json!({"Id": 1});
        let scope = Scope::default().nearest(&row);
        let err = fill("/x/{assetId}", &scope, Encoding::Path).unwrap_err();
        assert_eq!(err, TemplateError::Missing("assetId".into()));
        assert_eq!(err.to_string(), "this row has no `assetId`");
    }

    #[test]
    fn json_braces_and_empty_braces_are_left_alone() {
        let scope = Scope::with_ids(&["c1".into()]);
        let body = fill_json(r#"{"a":{"b":"{0}"},"c":{}}"#, &scope).unwrap();
        assert_eq!(body, json!({"a": {"b": "c1"}, "c": {}}));
    }
}

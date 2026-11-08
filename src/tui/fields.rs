//! Reading fields out of API rows by name: dotted paths into nested objects
//! (`latestVersion.columnCount`), array indexes (`bindings.0.connectionName`)
//! and alternatives tried in order (`["Name", "name"]`).

use serde_json::Value;

/// The value at a dotted `path`, or None when any step is missing.
pub fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }
    if let Some(found) = value.get(path) {
        // A key that itself contains dots ("usage.cost") wins over a walk.
        return Some(found);
    }
    path.split('.')
        .try_fold(value, |current, step| match current {
            Value::Object(map) => map.get(step),
            Value::Array(items) => step.parse::<usize>().ok().and_then(|i| items.get(i)),
            _ => None,
        })
}

/// The first alternative present and not null.
pub fn lookup_any<'a>(value: &'a Value, alternatives: &[&str]) -> Option<&'a Value> {
    alternatives
        .iter()
        .filter_map(|path| lookup(value, path))
        .find(|v| !v.is_null())
}

/// A field as an identifier string: strings and numbers only, never empty.
pub fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn walks_dotted_paths_and_indexes() {
        let row = json!({"latestVersion": {"columnCount": 14}, "bindings": [{"name": "c"}]});
        assert_eq!(lookup(&row, "latestVersion.columnCount"), Some(&json!(14)));
        assert_eq!(lookup(&row, "bindings.0.name"), Some(&json!("c")));
        assert_eq!(lookup(&row, "latestVersion.missing"), None);
        assert_eq!(lookup(&json!({"a.b": 1}), "a.b"), Some(&json!(1)));
    }

    #[test]
    fn alternatives_skip_missing_and_null() {
        let row = json!({"Name": null, "name": "GZS3"});
        assert_eq!(lookup_any(&row, &["Name", "name"]), Some(&json!("GZS3")));
        assert_eq!(lookup_any(&row, &["x"]), None);
    }

    #[test]
    fn ids_are_non_empty_strings_or_numbers() {
        assert_eq!(id_text(&json!(1000)).as_deref(), Some("1000"));
        assert_eq!(id_text(&json!("")), None);
        assert_eq!(id_text(&json!({"a": 1})), None);
    }
}

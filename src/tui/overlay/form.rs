//! A form filled in before an action runs: text, multiline, number, select
//! and checkbox fields, validated on submit with messages under each field.
//! Tab/Shift-Tab move, Enter submits (Ctrl+S inside a multiline field),
//! Esc cancels.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::Regex;
use serde_json::{Map, Value};

use crate::tui::fields;
use crate::tui::format::text_of;
use crate::tui::screen::{Choices, FieldKind, FormField, FormSpec};

#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Text(String),
    Checked(bool),
    /// Index into the field's choices.
    Choice(Option<usize>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceState {
    Ready(Vec<(String, String)>),
    /// Waiting for the request with this id.
    Loading(u64),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Editing,
    /// The values, keyed by field name, ready to fill a request template.
    Submitted(Value),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct FormState {
    pub spec: &'static FormSpec,
    pub values: Vec<FieldValue>,
    pub errors: Vec<Option<String>>,
    pub choices: Vec<ChoiceState>,
    pub focus: usize,
}

impl FormState {
    /// `default_of` fills a field's default template from the row.
    pub fn new(spec: &'static FormSpec, default_of: impl Fn(&str) -> Option<String>) -> Self {
        let defaults: Vec<Option<String>> = spec
            .fields
            .iter()
            .map(|f| f.default.and_then(&default_of))
            .collect();
        let choices: Vec<ChoiceState> = spec
            .fields
            .iter()
            .map(|f| match f.kind {
                FieldKind::Select(Choices::Static(options)) => ChoiceState::Ready(
                    options
                        .iter()
                        .map(|(v, l)| (v.to_string(), l.to_string()))
                        .collect(),
                ),
                _ => ChoiceState::Ready(Vec::new()),
            })
            .collect();
        let values = spec
            .fields
            .iter()
            .zip(&defaults)
            .zip(&choices)
            .map(|((field, default), choices)| initial_value(field, default.as_deref(), choices))
            .collect();
        Self {
            spec,
            values,
            errors: vec![None; spec.fields.len()],
            choices,
            focus: 0,
        }
    }

    /// Select fields whose choices come from an endpoint, by index.
    pub fn endpoint_fields(&self) -> Vec<usize> {
        self.spec
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(f.kind, FieldKind::Select(Choices::Endpoint { .. })))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn set_loading(&mut self, field: usize, request: u64) {
        self.choices[field] = ChoiceState::Loading(request);
    }

    pub fn set_failed(&mut self, field: usize, err: String) {
        self.choices[field] = ChoiceState::Failed(err);
    }

    /// Lands the answer for request `id`, if a field waits for it.
    pub fn choices_loaded(&mut self, id: u64, result: Result<Value, String>) -> bool {
        let Some(field) = self
            .choices
            .iter()
            .position(|c| *c == ChoiceState::Loading(id))
        else {
            return false;
        };
        let FieldKind::Select(Choices::Endpoint { value, label, .. }) =
            self.spec.fields[field].kind
        else {
            return false;
        };
        self.choices[field] = match result {
            Ok(answer) => {
                let rows = match crate::output::tabular(&answer) {
                    Value::Array(rows) => rows.clone(),
                    _ => Vec::new(),
                };
                let options = rows
                    .iter()
                    .filter_map(|row| {
                        let value = fields::lookup_any(row, value).map(text_of)?;
                        let label = fields::lookup_any(row, label)
                            .map(text_of)
                            .unwrap_or_else(|| value.clone());
                        Some((value, label))
                    })
                    .collect::<Vec<_>>();
                if self.values[field] == FieldValue::Choice(None) && !options.is_empty() {
                    self.values[field] = FieldValue::Choice(Some(0));
                }
                ChoiceState::Ready(options)
            }
            Err(err) => ChoiceState::Failed(err),
        };
        true
    }

    pub fn field(&self, index: usize) -> &'static FormField {
        &self.spec.fields[index]
    }

    /// The text shown for a field's current value.
    pub fn display(&self, index: usize) -> String {
        match (&self.values[index], &self.choices[index]) {
            (FieldValue::Text(text), _) => text.clone(),
            (FieldValue::Checked(on), _) => if *on { "[x]" } else { "[ ]" }.to_string(),
            (FieldValue::Choice(Some(i)), ChoiceState::Ready(options)) => options
                .get(*i)
                .map(|(_, label)| format!("‹ {label} ›"))
                .unwrap_or_default(),
            (FieldValue::Choice(_), ChoiceState::Loading(_)) => "loading…".to_string(),
            (FieldValue::Choice(_), ChoiceState::Failed(err)) => format!("couldn't load: {err}"),
            (FieldValue::Choice(None), ChoiceState::Ready(_)) => "(none)".to_string(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let multiline = matches!(self.field(self.focus).kind, FieldKind::Multiline);
        match key.code {
            KeyCode::Esc => return Outcome::Cancelled,
            KeyCode::Char('s') if ctrl => return self.submit(),
            KeyCode::Enter if !multiline => return self.submit(),
            KeyCode::Tab | KeyCode::Down => self.move_focus(1),
            KeyCode::BackTab | KeyCode::Up => self.move_focus(-1),
            _ => self.edit(key),
        }
        Outcome::Editing
    }

    fn move_focus(&mut self, delta: isize) {
        let last = self.values.len().saturating_sub(1) as isize;
        self.focus = (self.focus as isize + delta).clamp(0, last) as usize;
    }

    fn edit(&mut self, key: KeyEvent) {
        let index = self.focus;
        let kind = self.field(index).kind;
        let option_count = match &self.choices[index] {
            ChoiceState::Ready(options) => options.len(),
            _ => 0,
        };
        match (&mut self.values[index], key.code) {
            (FieldValue::Text(text), KeyCode::Enter) => text.push('\n'),
            (FieldValue::Text(text), KeyCode::Backspace) => {
                text.pop();
            }
            (FieldValue::Text(text), KeyCode::Char('u'))
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                text.clear()
            }
            (FieldValue::Text(text), KeyCode::Char(c)) => {
                let accepted = match kind {
                    FieldKind::Number { .. } => c.is_ascii_digit() || matches!(c, '.' | '-'),
                    _ => true,
                };
                if accepted {
                    text.push(c);
                }
            }
            (FieldValue::Checked(on), KeyCode::Char(' ')) => *on = !*on,
            (FieldValue::Choice(choice), KeyCode::Left | KeyCode::Right | KeyCode::Char(' '))
                if option_count > 0 =>
            {
                let step = if key.code == KeyCode::Left {
                    option_count - 1
                } else {
                    1
                };
                *choice = Some(choice.map_or(0, |i| (i + step) % option_count));
            }
            _ => return,
        }
        self.errors[index] = None;
    }

    fn submit(&mut self) -> Outcome {
        self.errors = (0..self.values.len()).map(|i| self.check(i)).collect();
        if let Some(first_bad) = self.errors.iter().position(Option::is_some) {
            self.focus = first_bad;
            return Outcome::Editing;
        }
        Outcome::Submitted(self.values_json())
    }

    /// The first problem with field `index`, if any.
    fn check(&self, index: usize) -> Option<String> {
        let field = self.field(index);
        let text = match &self.values[index] {
            FieldValue::Text(text) => text.trim().to_string(),
            FieldValue::Checked(_) => return None,
            FieldValue::Choice(choice) => {
                let missing =
                    choice.is_none() || matches!(self.choices[index], ChoiceState::Failed(_));
                return (field.required && missing)
                    .then(|| format!("Pick a {}", field.label.to_lowercase()));
            }
        };
        if text.is_empty() {
            return field
                .required
                .then(|| format!("{} is required", field.label));
        }
        if let FieldKind::Number { min, max } = field.kind {
            let Ok(number) = text.parse::<f64>() else {
                return Some("Enter a number".to_string());
            };
            if let Some(min) = min.filter(|min| number < *min) {
                return Some(format!("Must be at least {min}"));
            }
            if let Some(max) = max.filter(|max| number > *max) {
                return Some(format!("Must be at most {max}"));
            }
        }
        if let Some(max) = field
            .validation
            .max_len
            .filter(|max| text.chars().count() > *max)
        {
            return Some(format!("At most {max} characters"));
        }
        if let Some((pattern, message)) = field.validation.pattern {
            let anchored = format!("^(?:{pattern})$");
            let matches = Regex::new(&anchored).is_ok_and(|re| re.is_match(&text));
            if !matches {
                return Some(message.to_string());
            }
        }
        None
    }

    /// Values keyed by field name: text as strings, numbers as numbers,
    /// checkboxes as booleans, selects as the chosen value.
    pub fn values_json(&self) -> Value {
        let mut out = Map::new();
        for (index, field) in self.spec.fields.iter().enumerate() {
            let value = match (&self.values[index], &self.choices[index]) {
                (FieldValue::Text(text), _) => match field.kind {
                    FieldKind::Number { .. } => text
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                        .map_or(Value::Null, |n| number_value(n, text)),
                    _ => Value::String(text.trim().to_string()),
                },
                (FieldValue::Checked(on), _) => Value::Bool(*on),
                (FieldValue::Choice(Some(i)), ChoiceState::Ready(options)) => options
                    .get(*i)
                    .map_or(Value::Null, |(value, _)| Value::String(value.clone())),
                (FieldValue::Choice(_), _) => Value::Null,
            };
            out.insert(field.name.to_string(), value);
        }
        Value::Object(out)
    }
}

/// Whole numbers stay integers in the request body ("10", not "10.0").
fn number_value(number: serde_json::Number, text: &str) -> Value {
    match text.trim().parse::<i64>() {
        Ok(int) => Value::from(int),
        Err(_) => Value::Number(number),
    }
}

fn initial_value(field: &FormField, default: Option<&str>, choices: &ChoiceState) -> FieldValue {
    match field.kind {
        FieldKind::Checkbox => FieldValue::Checked(default.is_some_and(|d| d == "true")),
        FieldKind::Select(_) => {
            let ChoiceState::Ready(options) = choices else {
                return FieldValue::Choice(None);
            };
            let by_default = default.and_then(|d| options.iter().position(|(v, _)| v == d));
            FieldValue::Choice(by_default.or((!options.is_empty()).then_some(0)))
        }
        _ => FieldValue::Text(default.unwrap_or_default().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::nav::Source;
    use serde_json::json;

    const NAME_RULE: &str = "[A-Za-z][A-Za-z0-9_]*";

    static FORM: FormSpec = FormSpec {
        title: "New warehouse",
        submit: "Create",
        fields: &[
            FormField::new("name", "Name", FieldKind::Text)
                .required()
                .pattern(NAME_RULE, "Start with a letter; letters, digits and _ only")
                .default("{suggested}"),
            FormField::new(
                "limit",
                "Row limit",
                FieldKind::Number {
                    min: Some(1.0),
                    max: Some(100.0),
                },
            ),
            FormField::new(
                "format",
                "Format",
                FieldKind::Select(Choices::Static(&[("csv", "CSV"), ("json", "JSON")])),
            ),
            FormField::new("notes", "Notes", FieldKind::Multiline),
            FormField::new("public", "Public", FieldKind::Checkbox),
            FormField::new(
                "owner",
                "Owner",
                FieldKind::Select(Choices::Endpoint {
                    source: Source::get("x", "/users"),
                    value: &["id"],
                    label: &["name"],
                }),
            ),
        ],
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(form: &mut FormState, text: &str) {
        for c in text.chars() {
            form.on_key(key(KeyCode::Char(c)));
        }
    }

    fn blank() -> FormState {
        FormState::new(&FORM, |_| None)
    }

    #[test]
    fn defaults_come_from_the_row() {
        let form = FormState::new(&FORM, |t| (t == "{suggested}").then(|| "sales".to_string()));
        assert_eq!(form.values[0], FieldValue::Text("sales".into()));
        assert_eq!(form.values[2], FieldValue::Choice(Some(0)));
    }

    #[test]
    fn required_and_pattern_errors_block_submit_and_focus_the_field() {
        let mut form = blank();
        form.focus = 1;
        assert_eq!(form.on_key(key(KeyCode::Enter)), Outcome::Editing);
        assert_eq!(form.focus, 0);
        assert_eq!(form.errors[0].as_deref(), Some("Name is required"));
        typed(&mut form, "9lives");
        assert!(form.errors[0].is_none(), "typing clears the message");
        form.on_key(key(KeyCode::Enter));
        assert_eq!(
            form.errors[0].as_deref(),
            Some("Start with a letter; letters, digits and _ only")
        );
    }

    #[test]
    fn numbers_are_range_checked_and_filtered_while_typing() {
        let mut form = blank();
        typed(&mut form, "ok");
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, "5x00");
        assert_eq!(form.values[1], FieldValue::Text("500".into()));
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.errors[1].as_deref(), Some("Must be at most 100"));
    }

    #[test]
    fn submits_typed_values_by_name() {
        let mut form = blank();
        typed(&mut form, "sales");
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, "10");
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Right));
        form.on_key(key(KeyCode::Tab));
        typed(&mut form, "a");
        form.on_key(key(KeyCode::Enter));
        typed(&mut form, "b");
        form.on_key(key(KeyCode::Tab));
        form.on_key(key(KeyCode::Char(' ')));
        let Outcome::Submitted(values) =
            form.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        else {
            panic!("form should submit");
        };
        assert_eq!(
            values,
            json!({"name": "sales", "limit": 10, "format": "json", "notes": "a\nb", "public": true, "owner": null})
        );
    }

    #[test]
    fn endpoint_choices_load_by_request_id() {
        let mut form = blank();
        assert_eq!(form.endpoint_fields(), vec![5]);
        form.set_loading(5, 42);
        assert_eq!(form.display(5), "loading…");
        assert!(!form.choices_loaded(7, Ok(json!([]))));
        assert!(form.choices_loaded(42, Ok(json!({"items": [{"id": "u1", "name": "Ana"}]}))));
        assert_eq!(form.display(5), "‹ Ana ›");
        assert_eq!(form.values_json()["owner"], "u1");
    }

    #[test]
    fn escape_cancels() {
        assert_eq!(blank().on_key(key(KeyCode::Esc)), Outcome::Cancelled);
    }
}

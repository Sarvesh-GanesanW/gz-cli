//! The task list the model keeps with `todo_write`. The chat server runs
//! the tool and streams each new list as a
//! `todo_update` event; front ends show it while the work goes on.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// One item of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
    /// What it reads as while in progress ("Running the tests").
    pub active_form: Option<String>,
}

impl Todo {
    /// What to show: the in-progress wording while it runs.
    pub fn label(&self) -> &str {
        match (&self.active_form, self.status) {
            (Some(active), TodoStatus::InProgress) => active,
            _ => &self.content,
        }
    }
}

/// The list in a `todo_update` event; items it can't read are left out.
pub fn parse(data: &Value) -> Vec<Todo> {
    data["todos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let status = match item["status"].as_str()? {
                "pending" => TodoStatus::Pending,
                "in_progress" => TodoStatus::InProgress,
                "completed" => TodoStatus::Completed,
                _ => return None,
            };
            Some(Todo {
                content: item["content"].as_str()?.to_string(),
                status,
                active_form: item["active_form"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Whether the list still has work left to show.
pub fn has_open_items(todos: &[Todo]) -> bool {
    todos.iter().any(|t| t.status != TodoStatus::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_list_and_skips_bad_items() {
        let todos = parse(&json!({"type": "todo_update", "todos": [
            {"content": "Run tests", "status": "in_progress", "active_form": "Running tests"},
            {"content": "Fix lint", "status": "pending"},
            {"content": "broken", "status": "maybe"},
            {"content": "Write docs", "status": "completed"}
        ]}));
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[0].label(), "Running tests");
        assert_eq!(todos[1].label(), "Fix lint");
        assert!(has_open_items(&todos));
        assert!(!has_open_items(&todos[2..]));
    }
}

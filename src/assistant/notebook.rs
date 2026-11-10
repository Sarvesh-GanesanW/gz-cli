//! Jupyter notebooks: `read_file` on a `.ipynb` shows
//! its cells (with ids to target) and their text outputs instead of raw
//! JSON, and `notebook_edit` replaces, inserts or deletes one cell.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

/// Characters of one cell's outputs shown when reading.
const MAX_OUTPUT_CHARS: usize = 2_000;

/// How `notebook_edit` changes the notebook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    Replace,
    Insert,
    Delete,
}

/// A validated `notebook_edit` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellEdit {
    pub mode: EditMode,
    /// The cell to change; for an insert, the cell to insert after
    /// (`None`: at the start).
    pub cell_id: Option<String>,
    pub cell_type: Option<String>,
    pub source: Option<String>,
}

impl CellEdit {
    pub fn parse(input: &Value) -> Result<Self> {
        let text = |key: &str| input[key].as_str().map(str::to_string);
        let mode = match input["edit_mode"].as_str() {
            None | Some("replace") => EditMode::Replace,
            Some("insert") => EditMode::Insert,
            Some("delete") => EditMode::Delete,
            Some(other) => bail!("unknown edit_mode '{other}' (replace, insert, or delete)"),
        };
        let edit = Self {
            mode,
            cell_id: text("cell_id"),
            cell_type: text("cell_type"),
            source: text("new_source"),
        };
        if let Some(kind) = &edit.cell_type {
            if kind != "code" && kind != "markdown" {
                bail!("cell_type must be code or markdown, not '{kind}'");
            }
        }
        match mode {
            EditMode::Replace if edit.cell_id.is_none() || edit.source.is_none() => {
                bail!("replace needs cell_id and new_source")
            }
            EditMode::Insert if edit.source.is_none() || edit.cell_type.is_none() => {
                bail!("insert needs new_source and cell_type")
            }
            EditMode::Delete if edit.cell_id.is_none() => bail!("delete needs cell_id"),
            _ => Ok(edit),
        }
    }

    /// One line for prompts: `replace cell 3a1f`.
    pub fn describe(&self) -> String {
        let verb = match self.mode {
            EditMode::Replace => "replace cell",
            EditMode::Insert => "insert a cell after",
            EditMode::Delete => "delete cell",
        };
        match (&self.cell_id, self.mode) {
            (None, EditMode::Insert) => "insert a cell at the start".to_string(),
            (Some(id), _) => format!("{verb} {id}"),
            (None, _) => verb.to_string(),
        }
    }

    /// The notebook `text` with this edit applied, as Jupyter writes it.
    pub fn apply(&self, text: &str) -> Result<String> {
        let mut notebook: Value =
            serde_json::from_str(text).context("this is not a notebook (bad JSON)")?;
        let cells = notebook["cells"]
            .as_array_mut()
            .context("this notebook has no cells list")?;
        let position = match &self.cell_id {
            Some(id) => Some(find(cells, id)?),
            None => None,
        };
        match self.mode {
            EditMode::Replace => {
                let cell = &mut cells[position.context("replace needs cell_id")?];
                cell["source"] = source_lines(self.source.as_deref().unwrap_or_default());
                if let Some(kind) = &self.cell_type {
                    cell["cell_type"] = json!(kind);
                }
                reset_outputs(cell);
            }
            EditMode::Insert => {
                let mut cell = json!({
                    "cell_type": self.cell_type.as_deref().unwrap_or("code"),
                    "id": new_id(),
                    "metadata": {},
                    "source": source_lines(self.source.as_deref().unwrap_or_default()),
                });
                reset_outputs(&mut cell);
                let at = position.map(|i| i + 1).unwrap_or(0);
                cells.insert(at, cell);
            }
            EditMode::Delete => {
                cells.remove(position.context("delete needs cell_id")?);
            }
        }
        write(&notebook)
    }
}

/// The notebook as text for the model: each cell's id, type, source and
/// text outputs.
pub fn render(text: &str) -> Result<String> {
    let notebook: Value =
        serde_json::from_str(text).context("this is not a notebook (bad JSON)")?;
    let cells = notebook["cells"]
        .as_array()
        .context("this notebook has no cells list")?;
    if cells.is_empty() {
        return Ok("(a notebook with no cells)".to_string());
    }
    let mut out = String::new();
    for (index, cell) in cells.iter().enumerate() {
        let kind = cell["cell_type"].as_str().unwrap_or("code");
        out.push_str(&format!(
            "<cell id=\"{}\" type=\"{kind}\">\n",
            cell_id(cell, index)
        ));
        out.push_str(joined(&cell["source"]).trim_end());
        out.push_str("\n</cell>\n");
        let outputs = outputs_text(cell);
        if !outputs.trim().is_empty() {
            let shown: String = outputs.chars().take(MAX_OUTPUT_CHARS).collect();
            out.push_str(&format!("<output>\n{}\n</output>\n", shown.trim_end()));
        }
    }
    Ok(out)
}

/// A cell's id, or `cell-N` for notebooks older than ids (nbformat < 4.5).
fn cell_id(cell: &Value, index: usize) -> String {
    cell["id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("cell-{index}"))
}

fn find(cells: &[Value], id: &str) -> Result<usize> {
    cells
        .iter()
        .enumerate()
        .position(|(index, cell)| cell_id(cell, index) == id)
        .with_context(|| format!("there is no cell '{id}'; read the notebook to see the cell ids"))
}

/// Jupyter keeps source as a list of lines, each ending in `\n` but the last.
fn source_lines(source: &str) -> Value {
    let lines: Vec<String> = source.split_inclusive('\n').map(str::to_string).collect();
    json!(lines)
}

fn joined(source: &Value) -> String {
    match source {
        Value::String(text) => text.clone(),
        Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// A code cell's old outputs no longer match its source.
fn reset_outputs(cell: &mut Value) {
    if cell["cell_type"] == "code" {
        cell["outputs"] = json!([]);
        cell["execution_count"] = Value::Null;
    } else if let Some(fields) = cell.as_object_mut() {
        fields.remove("outputs");
        fields.remove("execution_count");
    }
}

fn outputs_text(cell: &Value) -> String {
    let Some(outputs) = cell["outputs"].as_array() else {
        return String::new();
    };
    outputs
        .iter()
        .map(|output| match output["output_type"].as_str() {
            Some("stream") => joined(&output["text"]),
            Some("error") => format!(
                "{}: {}",
                output["ename"].as_str().unwrap_or("Error"),
                output["evalue"].as_str().unwrap_or_default()
            ),
            _ => joined(&output["data"]["text/plain"]),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// New cell ids: random, 8 hex characters, as Jupyter makes them.
fn new_id() -> String {
    crate::cmd::new_uuid().replace('-', "")[..8].to_string()
}

/// JSON as Jupyter writes notebooks: one-space indent, trailing newline.
fn write(notebook: &Value) -> Result<String> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    notebook.serialize(&mut serializer)?;
    out.push(b'\n');
    Ok(String::from_utf8(out)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTEBOOK: &str = r##"{"cells": [
        {"cell_type": "markdown", "id": "intro", "metadata": {}, "source": ["# Title\n", "Notes"]},
        {"cell_type": "code", "id": "load", "metadata": {}, "execution_count": 3,
         "source": "import pandas as pd", "outputs": [{"output_type": "stream", "name": "stdout", "text": ["ok\n"]}]}
    ], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"##;

    fn edit(input: Value) -> Value {
        let out = CellEdit::parse(&input).unwrap().apply(NOTEBOOK).unwrap();
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn reading_shows_cells_with_ids_and_outputs() {
        let text = render(NOTEBOOK).unwrap();
        assert_eq!(
            text,
            "<cell id=\"intro\" type=\"markdown\">\n# Title\nNotes\n</cell>\n\
             <cell id=\"load\" type=\"code\">\nimport pandas as pd\n</cell>\n<output>\nok\n</output>\n"
        );
    }

    #[test]
    fn replace_insert_and_delete_target_cells_by_id() {
        let replaced =
            edit(json!({"cell_id": "load", "new_source": "import polars as pl\nprint(1)"}));
        let cell = &replaced["cells"][1];
        assert_eq!(cell["source"], json!(["import polars as pl\n", "print(1)"]));
        assert_eq!(cell["outputs"], json!([]), "stale outputs are cleared");
        assert_eq!(cell["execution_count"], Value::Null);

        let inserted = edit(json!({"edit_mode": "insert", "cell_id": "intro",
                                   "cell_type": "code", "new_source": "x = 1"}));
        assert_eq!(inserted["cells"].as_array().unwrap().len(), 3);
        assert_eq!(inserted["cells"][1]["source"], json!(["x = 1"]));
        assert_eq!(inserted["cells"][1]["id"].as_str().unwrap().len(), 8);

        let deleted = edit(json!({"edit_mode": "delete", "cell_id": "intro"}));
        assert_eq!(deleted["cells"][0]["id"], "load");
    }

    #[test]
    fn edits_say_what_is_missing_or_wrong() {
        let err = |input: Value| format!("{:#}", CellEdit::parse(&input).unwrap_err());
        assert!(err(json!({"new_source": "x"})).contains("replace needs cell_id"));
        assert!(err(json!({"edit_mode": "insert", "new_source": "x"})).contains("cell_type"));
        assert!(err(json!({"edit_mode": "delete"})).contains("delete needs cell_id"));
        let missing = CellEdit::parse(&json!({"cell_id": "nope", "new_source": "x"}))
            .unwrap()
            .apply(NOTEBOOK)
            .unwrap_err();
        assert!(format!("{missing:#}").contains("no cell 'nope'"));
    }

    #[test]
    fn cells_without_ids_are_named_by_position() {
        let old = r#"{"cells": [{"cell_type": "code", "source": "1", "outputs": []}], "nbformat": 4, "nbformat_minor": 4}"#;
        assert!(render(old).unwrap().starts_with("<cell id=\"cell-0\""));
        let out = CellEdit::parse(&json!({"cell_id": "cell-0", "new_source": "2"}))
            .unwrap()
            .apply(old)
            .unwrap();
        assert!(out.contains("\"2\"") && out.ends_with("}\n"));
    }
}

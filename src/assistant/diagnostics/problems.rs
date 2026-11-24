//! Reading checker output: `path:line[:col]: message` lines (ruff concise,
//! shellcheck gcc, go vet, eslint unix, mypy, cargo short), ruff's
//! notebook form `path:cell N:line:col: message`, and `path(line,col):
//! message` lines (tsc), keeping only the edited files, and telling new
//! problems from ones the file already had.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One problem a checker reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Absolute, as resolved from the checker's folder.
    pub path: PathBuf,
    /// The notebook cell (1-based, markdown cells counted) `line` is in.
    pub cell: Option<u32>,
    pub line: u32,
    pub column: Option<u32>,
    pub message: String,
    /// The text of the line it points at, trimmed; set by [`with_source`].
    pub source: String,
}

impl Problem {
    /// `shown_path:line[:col]: message`, or for a notebook
    /// `shown_path cell N line L[ col C]: message`.
    pub fn describe(&self, shown_path: &str) -> String {
        if let Some(cell) = self.cell {
            let column = self.column.map(|c| format!(" col {c}")).unwrap_or_default();
            return format!(
                "{shown_path} cell {cell} line {}{column}: {}",
                self.line, self.message
            );
        }
        match self.column {
            Some(column) => format!("{shown_path}:{}:{column}: {}", self.line, self.message),
            None => format!("{shown_path}:{}: {}", self.line, self.message),
        }
    }
}

/// Every problem in `output` (run in `cwd`) that is about one of `files`.
pub fn about(output: &str, cwd: &Path, files: &[PathBuf]) -> Vec<Problem> {
    let wanted: Vec<PathBuf> = files.iter().map(|file| comparable(file)).collect();
    output
        .lines()
        .filter_map(|line| parse_line(line, cwd))
        .filter(|problem| wanted.contains(&comparable(&problem.path)))
        .collect()
}

/// How many problem lines `output` has, about any file.
pub fn about_any(output: &str, cwd: &Path) -> usize {
    output
        .lines()
        .filter(|line| parse_line(line, cwd).is_some())
        .count()
}

/// `problems` with the text of the line each points at, from `text` (the
/// file as it was when checked; a notebook's line is read in its cell).
pub fn with_source(mut problems: Vec<Problem>, text: &str) -> Vec<Problem> {
    let lines: Vec<&str> = text.lines().collect();
    let cells = problems
        .iter()
        .any(|problem| problem.cell.is_some())
        .then(|| notebook_cells(text));
    for problem in &mut problems {
        let line = match (problem.cell, &cells) {
            (Some(cell), Some(cells)) => (cell as usize)
                .checked_sub(1)
                .and_then(|at| cells.get(at))
                .and_then(|source| nth_line(source.lines(), problem.line)),
            _ => nth_line(lines.iter().copied(), problem.line),
        };
        problem.source = line.map(|line| line.trim().to_string()).unwrap_or_default();
    }
    problems
}

/// Line `number` (1-based) of `lines`.
fn nth_line<'a>(mut lines: impl Iterator<Item = &'a str>, number: u32) -> Option<&'a str> {
    lines.nth((number as usize).checked_sub(1)?)
}

/// Each cell's source in a notebook's JSON, in order; empty when `text`
/// is not a notebook.
fn notebook_cells(text: &str) -> Vec<String> {
    let Ok(notebook) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(cells) = notebook["cells"].as_array() else {
        return Vec::new();
    };
    cells
        .iter()
        .map(|cell| match &cell["source"] {
            serde_json::Value::String(source) => source.clone(),
            serde_json::Value::Array(parts) => parts.iter().filter_map(|p| p.as_str()).collect(),
            _ => String::new(),
        })
        .collect()
}

/// The problems in `after` that `before` did not have. Lines move when a
/// file is edited, so a problem is matched by file, message and the text
/// of its line, never by line number, and each earlier problem accounts
/// for one later one.
pub fn new_since(before: &[Problem], after: Vec<Problem>) -> Vec<Problem> {
    let mut known: HashMap<(PathBuf, String, String), usize> = HashMap::new();
    for problem in before {
        *known.entry(match_key(problem)).or_default() += 1;
    }
    after
        .into_iter()
        .filter(|problem| {
            let key = match_key(problem);
            match known.get_mut(&key) {
                Some(left) if *left > 0 => {
                    *left -= 1;
                    false
                }
                _ => true,
            }
        })
        .collect()
}

fn match_key(problem: &Problem) -> (PathBuf, String, String) {
    (
        comparable(&problem.path),
        problem.message.clone(),
        problem.source.clone(),
    )
}

fn parse_line(line: &str, cwd: &Path) -> Option<Problem> {
    let line = line.trim_end();
    let (path, line, column, message) =
        parse_colon_form(line).or_else(|| parse_paren_form(line))?;
    let (path, cell) = split_cell(path);
    Some(Problem {
        path: resolve(path, cwd),
        cell,
        line,
        column,
        message: message.to_string(),
        source: String::new(),
    })
}

/// `n.ipynb:cell 3` → (`n.ipynb`, 3): ruff names the notebook cell after
/// the path.
fn split_cell(path: &str) -> (&str, Option<u32>) {
    let Some((file, cell)) = path.rsplit_once(":cell ") else {
        return (path, None);
    };
    match cell.parse() {
        Ok(cell) => (file, Some(cell)),
        Err(_) => (path, None),
    }
}

type Parsed<'a> = (&'a str, u32, Option<u32>, &'a str);

/// `path:line:col: message` or `path:line: message`. The path ends at the
/// first `:<digits>:`, so Windows drive letters (`C:\`) stay in it.
fn parse_colon_form(line: &str) -> Option<Parsed<'_>> {
    for (at, _) in line.match_indices(':') {
        let path = &line[..at];
        let rest = &line[at + 1..];
        let Some((number, after)) = leading_number(rest) else {
            continue;
        };
        let Some(after) = after.strip_prefix(':') else {
            continue;
        };
        if path.trim().is_empty() {
            return None;
        }
        let (column, message) = match leading_number(after) {
            Some((column, tail)) if tail.starts_with(':') => (Some(column), &tail[1..]),
            _ => (None, after),
        };
        let message = message.trim();
        return (!message.is_empty()).then_some((path, number, column, message));
    }
    None
}

/// `path(line,col): message`, as tsc prints without `--pretty`.
fn parse_paren_form(line: &str) -> Option<Parsed<'_>> {
    let (head, message) = line.split_once("): ")?;
    let (path, position) = head.rsplit_once('(')?;
    let (row, column) = position.split_once(',')?;
    let message = message.trim();
    if path.trim().is_empty() || message.is_empty() {
        return None;
    }
    Some((path, row.parse().ok()?, Some(column.parse().ok()?), message))
}

fn leading_number(text: &str) -> Option<(u32, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let number = text[..digits].parse().ok()?;
    Some((number, &text[digits..]))
}

fn resolve(path: &str, cwd: &Path) -> PathBuf {
    // Some tools name themselves first: `vet: ./main.go:3:2: …`.
    let path = path.rsplit(": ").next().unwrap_or(path);
    let path = Path::new(path.trim());
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The same file compares equal however it was spelled (`/tmp` and
/// `/private/tmp` on macOS, `a/../b`).
fn comparable(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(path: &str, line: u32, message: &str) -> Problem {
        Problem {
            path: PathBuf::from(path),
            cell: None,
            line,
            column: None,
            message: message.into(),
            source: String::new(),
        }
    }

    #[test]
    fn reads_every_common_checker_format() {
        let cwd = Path::new("/w");
        let lines = [
            "app.py:3:8: F401 [*] `os` imported but unused",
            "/w/run.sh:2:6: warning: Double quote to prevent globbing. [SC2086]",
            "./main.go:7:2: fmt.Printf format %d has arg x of wrong type string",
            "src/lib.rs:12:5: error[E0425]: cannot find value `y` in this scope",
            "app.py:9: error: Incompatible return value type  [return-value]",
            "src/a.ts(4,7): error TS2322: Type 'string' is not assignable to type 'number'.",
        ];
        let parsed: Vec<Problem> = lines
            .iter()
            .map(|line| parse_line(line, cwd).unwrap())
            .collect();
        assert_eq!(parsed[0].path, Path::new("/w/app.py"));
        assert_eq!((parsed[0].line, parsed[0].column), (3, Some(8)));
        assert_eq!(parsed[0].message, "F401 [*] `os` imported but unused");
        assert_eq!(parsed[1].path, Path::new("/w/run.sh"));
        assert_eq!(parsed[2].path, Path::new("/w/main.go"));
        assert_eq!(
            parsed[3].message,
            "error[E0425]: cannot find value `y` in this scope"
        );
        assert_eq!((parsed[4].line, parsed[4].column), (9, None));
        assert_eq!(parsed[5].path, Path::new("/w/src/a.ts"));
        assert_eq!((parsed[5].line, parsed[5].column), (4, Some(7)));
    }

    #[test]
    fn a_tool_name_before_the_path_is_not_part_of_it() {
        let parsed = parse_line("vet: ./main.go:3:2: undefined: y", Path::new("/w")).unwrap();
        assert_eq!(parsed.path, Path::new("/w/main.go"));
        assert_eq!(parsed.message, "undefined: y");
    }

    #[test]
    fn skips_lines_that_are_not_problems() {
        let cwd = Path::new("/w");
        for line in [
            "",
            "Found 3 errors.",
            "# example.com/pkg",
            "    |",
            "warning: unused import: `x`",
            ":3: no path",
        ] {
            assert_eq!(parse_line(line, cwd), None, "{line}");
        }
    }

    #[test]
    fn keeps_a_windows_drive_in_the_path() {
        let parsed = parse_colon_form(r"C:\w\app.py:3:8: E501 line too long").unwrap();
        assert_eq!(parsed, (r"C:\w\app.py", 3, Some(8), "E501 line too long"));
    }

    #[test]
    fn only_problems_in_the_edited_files_count() {
        let output = "a.py:1:1: F401 one\nb.py:2:1: F401 two\nc.py:5:1: E1 three\n";
        let found = about(output, Path::new("/w"), &[PathBuf::from("/w/a.py")]);
        let messages: Vec<&str> = found.iter().map(|p| p.message.as_str()).collect();
        assert_eq!(messages, ["F401 one"]);
    }

    #[test]
    fn a_problem_that_only_moved_lines_is_not_new() {
        let before = vec![
            problem("/w/a.py", 3, "F401 os unused"),
            problem("/w/a.py", 9, "E1 dup"),
        ];
        let after = vec![
            problem("/w/a.py", 5, "F401 os unused"),
            problem("/w/a.py", 11, "E1 dup"),
            problem("/w/a.py", 12, "E1 dup"),
            problem("/w/a.py", 1, "F821 undefined name"),
        ];
        let fresh = new_since(&before, after);
        let shown: Vec<(u32, &str)> = fresh.iter().map(|p| (p.line, p.message.as_str())).collect();
        assert_eq!(shown, [(12, "E1 dup"), (1, "F821 undefined name")]);
    }

    #[test]
    fn the_same_message_on_a_different_line_text_is_new() {
        let before = with_source(vec![problem("/w/a.py", 1, "E1 bad")], "BAD old\n");
        let after = with_source(
            vec![
                problem("/w/a.py", 1, "E1 bad"),
                problem("/w/a.py", 3, "E1 bad"),
            ],
            "BAD new\nx\n  BAD old\n",
        );
        let fresh = new_since(&before, after);
        assert_eq!(fresh.len(), 1);
        assert_eq!((fresh[0].line, fresh[0].source.as_str()), (1, "BAD new"));
    }

    /// Real `ruff check --output-format concise` (0.17.6) output for a
    /// notebook whose first cell is markdown.
    const RUFF_NOTEBOOK: &str = "n.ipynb:cell 2:2:8: F401 [*] `os` imported but unused\n\
                                 n.ipynb:cell 3:1:8: F401 [*] `sys` imported but unused\n";
    const NOTEBOOK: &str = r##"{"cells":[{"cell_type":"markdown","metadata":{},"source":["# hi"]},{"cell_type":"code","metadata":{},"execution_count":null,"outputs":[],"source":["x = 1\n","import os"]},{"cell_type":"code","metadata":{},"execution_count":null,"outputs":[],"source":"import sys\n"}],"metadata":{},"nbformat":4,"nbformat_minor":5}"##;

    #[test]
    fn reads_ruffs_notebook_cells_and_their_source() {
        let found = about(
            RUFF_NOTEBOOK,
            Path::new("/w"),
            &[PathBuf::from("/w/n.ipynb")],
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].path, Path::new("/w/n.ipynb"));
        assert_eq!(
            (found[0].cell, found[0].line, found[0].column),
            (Some(2), 2, Some(8))
        );
        let found = with_source(found, NOTEBOOK);
        assert_eq!(found[0].source, "import os");
        assert_eq!(found[1].source, "import sys");
        assert_eq!(
            found[0].describe("n.ipynb"),
            "n.ipynb cell 2 line 2 col 8: F401 [*] `os` imported but unused"
        );
    }

    #[test]
    fn describes_with_and_without_a_column() {
        let mut found = problem("/w/a.py", 3, "bad");
        assert_eq!(found.describe("a.py"), "a.py:3: bad");
        found.column = Some(2);
        assert_eq!(found.describe("a.py"), "a.py:3:2: bad");
    }
}

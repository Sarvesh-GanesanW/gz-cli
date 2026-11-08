//! How tool calls read on screen: `Read(src/main.rs)` with a `⎿` result
//! line, the way coding CLIs present them. Built from the session's
//! `tool_name target` summaries and one-line result details.

use crate::assistant::local_context::ContextTool;
use crate::assistant::tools::ToolName;

/// The display form of one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    /// `Read`, `Search`, `Update`, `Bash`, `Script`, … (shown bold).
    pub verb: String,
    /// What it acts on, shown in parentheses.
    pub target: String,
    /// Read-only calls may be grouped into one summary line.
    pub read_only: bool,
}

impl Label {
    #[cfg(test)]
    pub fn title(&self) -> String {
        if self.target.is_empty() {
            self.verb.clone()
        } else {
            format!("{}({})", self.verb, self.target)
        }
    }
}

/// `read_file src/main.rs` → `Read(src/main.rs)`; server tools keep their
/// own display name.
pub fn label(name: &str, summary: &str) -> Label {
    let rest = summary
        .split_once(' ')
        .map(|(_, rest)| rest)
        .unwrap_or_default()
        .trim();
    if let Some(tool) = ContextTool::parse(name) {
        let verb = match tool {
            ContextTool::ChangeDirectory => "Folder",
            ContextTool::Memory => "Memory",
        };
        return Label {
            verb: verb.to_string(),
            target: rest.to_string(),
            read_only: false,
        };
    }
    let Some(tool) = ToolName::parse(name) else {
        return Label {
            verb: name.to_string(),
            target: String::new(),
            read_only: false,
        };
    };
    let (verb, target) = match tool {
        ToolName::ReadFile => ("Read", rest.to_string()),
        ToolName::ListDirectory => ("List", rest.to_string()),
        ToolName::FindFiles => ("Search", format!("pattern: \"{}\"", before_in(rest))),
        ToolName::SearchFiles => (
            "Search",
            format!("pattern: \"{}\"", before_in(rest).trim_matches('/')),
        ),
        ToolName::EditFile => ("Update", rest.to_string()),
        ToolName::WriteFile => ("Write", rest.to_string()),
        ToolName::RunCommand => ("Bash", rest.to_string()),
        ToolName::RunScript => ("Script", rest.replace(" (", " · ").replace(')', "")),
    };
    Label {
        verb: verb.to_string(),
        target,
        read_only: tool.access() == crate::assistant::tools::Access::Read,
    }
}

/// `"*.rs in src"` → `"*.rs"`.
fn before_in(rest: &str) -> &str {
    rest.rsplit_once(" in ").map(|(p, _)| p).unwrap_or(rest)
}

/// The `⎿` line under a finished call, phrased per tool.
pub fn result_line(name: &str, summary: &str, detail: &str, diff: Option<&str>) -> String {
    let count = leading_number(detail);
    let target = label(name, summary).target;
    match (ToolName::parse(name), count) {
        (Some(ToolName::ReadFile), Some(n)) => format!("Read {n} {}", plural(n, "line")),
        (Some(ToolName::ListDirectory), Some(n)) => format!("Listed {n} {}", plural(n, "entry")),
        (Some(ToolName::FindFiles), Some(n)) => format!("Found {n} {}", plural(n, "file")),
        (Some(ToolName::SearchFiles), Some(n)) => format!("Found {n} {}", plural(n, "match")),
        (Some(ToolName::EditFile | ToolName::WriteFile), _) => match diff.map(diff_counts) {
            Some((added, removed)) => format!(
                "Updated {target} with {added} {} and {removed} {}",
                plural(added, "addition"),
                plural(removed, "removal")
            ),
            None => detail.to_string(),
        },
        _ => detail.to_string(),
    }
}

/// A one-line summary for several read-only calls: `Read 3 files`.
pub fn group_summary(labels: &[Label]) -> String {
    let count = |verb: &str| labels.iter().filter(|l| l.verb == verb).count();
    let parts: Vec<String> = [("Read", "file"), ("Search", "pattern"), ("List", "folder")]
        .iter()
        .filter_map(|(verb, noun)| {
            let n = count(verb);
            let past = match *verb {
                "Read" => "Read",
                "Search" => "Searched",
                _ => "Listed",
            };
            (n > 0).then(|| format!("{past} {n} {}", plural(n, noun)))
        })
        .collect();
    parts.join(", ")
}

fn leading_number(detail: &str) -> Option<usize> {
    detail.split_whitespace().next()?.parse().ok()
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        noun.to_string()
    } else if noun.ends_with("ch") || noun.ends_with('s') {
        format!("{noun}es")
    } else if let Some(stem) = noun.strip_suffix('y') {
        format!("{stem}ies")
    } else {
        format!("{noun}s")
    }
}

/// (additions, removals) in a unified diff, headers excluded.
pub fn diff_counts(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(a, r), line| {
        if line.starts_with("+++") || line.starts_with("---") {
            (a, r)
        } else if line.starts_with('+') {
            (a + 1, r)
        } else if line.starts_with('-') {
            (a, r + 1)
        } else {
            (a, r)
        }
    })
}

/// One row of a rendered diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    Context {
        line: usize,
        text: String,
    },
    Added {
        line: usize,
        text: String,
    },
    Removed {
        line: usize,
        text: String,
    },
    /// A gap between hunks.
    Gap,
}

/// Unified diff → numbered rows (new-file numbers for context and
/// additions, old-file numbers for removals).
pub fn diff_rows(diff: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let (mut old_line, mut new_line) = (0usize, 0usize);
    let mut seen_hunk = false;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(header) = line.strip_prefix("@@") {
            let (old_start, new_start) = hunk_starts(header);
            if seen_hunk {
                rows.push(DiffRow::Gap);
            }
            seen_hunk = true;
            old_line = old_start;
            new_line = new_start;
            continue;
        }
        let text = line.get(1..).unwrap_or_default().to_string();
        match line.chars().next() {
            Some('+') => {
                rows.push(DiffRow::Added {
                    line: new_line,
                    text,
                });
                new_line += 1;
            }
            Some('-') => {
                rows.push(DiffRow::Removed {
                    line: old_line,
                    text,
                });
                old_line += 1;
            }
            Some('\\') => {}
            _ => {
                rows.push(DiffRow::Context {
                    line: new_line,
                    text,
                });
                old_line += 1;
                new_line += 1;
            }
        }
    }
    rows
}

/// ` -16,7 +16,7 @@` → (16, 16).
fn hunk_starts(header: &str) -> (usize, usize) {
    let mut old = 1;
    let mut new = 1;
    for part in header.split_whitespace() {
        let number = |p: &str| {
            p[1..]
                .split(',')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1)
        };
        if part.starts_with('-') {
            old = number(part);
        } else if part.starts_with('+') {
            new = number(part);
        }
    }
    (old, new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_local_tools_like_a_coding_cli() {
        assert_eq!(
            label("read_file", "read_file src/a.rs").title(),
            "Read(src/a.rs)"
        );
        assert_eq!(
            label("search_files", "search_files /TODO/ in .").title(),
            "Search(pattern: \"TODO\")"
        );
        assert_eq!(
            label("find_files", "find_files *.rs in src").title(),
            "Search(pattern: \"*.rs\")"
        );
        assert_eq!(
            label("edit_file", "edit_file src/a.rs").title(),
            "Update(src/a.rs)"
        );
        assert_eq!(
            label("run_command", "run_command cargo test").title(),
            "Bash(cargo test)"
        );
        assert_eq!(
            label("run_script", "run_script python (11 lines)").title(),
            "Script(python · 11 lines)"
        );
        assert_eq!(
            label("Query lakehouse", "Query lakehouse").title(),
            "Query lakehouse"
        );
        assert!(label("read_file", "read_file x").read_only);
        assert!(!label("edit_file", "edit_file x").read_only);
    }

    #[test]
    fn phrases_results_per_tool() {
        assert_eq!(
            result_line("read_file", "read_file a", "7 lines", None),
            "Read 7 lines"
        );
        assert_eq!(
            result_line("search_files", "search_files /x/ in .", "12 lines", None),
            "Found 12 matches"
        );
        assert_eq!(
            result_line("find_files", "find_files *.rs in .", "1 lines", None),
            "Found 1 file"
        );
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n a\n-b\n+c\n+d\n";
        assert_eq!(
            result_line("edit_file", "edit_file x", "edited", Some(diff)),
            "Updated x with 2 additions and 1 removal"
        );
        assert_eq!(
            result_line("run_command", "run_command ls", "3 lines", None),
            "3 lines"
        );
    }

    #[test]
    fn numbers_diff_rows_from_hunk_headers() {
        let diff = "--- a/x\n+++ b/x\n@@ -18,3 +18,3 @@\n keep\n-old\n+new\n tail\n@@ -40,1 +40,2 @@\n z\n+added\n";
        let rows = diff_rows(diff);
        assert_eq!(
            rows[0],
            DiffRow::Context {
                line: 18,
                text: "keep".into()
            }
        );
        assert_eq!(
            rows[1],
            DiffRow::Removed {
                line: 19,
                text: "old".into()
            }
        );
        assert_eq!(
            rows[2],
            DiffRow::Added {
                line: 19,
                text: "new".into()
            }
        );
        assert_eq!(
            rows[3],
            DiffRow::Context {
                line: 20,
                text: "tail".into()
            }
        );
        assert_eq!(rows[4], DiffRow::Gap);
        assert_eq!(
            rows[6],
            DiffRow::Added {
                line: 41,
                text: "added".into()
            }
        );
    }

    #[test]
    fn summarizes_groups_of_reads() {
        let labels = vec![
            label("read_file", "read_file a"),
            label("read_file", "read_file b"),
            label("search_files", "search_files /x/ in ."),
        ];
        assert_eq!(group_summary(&labels), "Read 2 files, Searched 1 pattern");
    }
}

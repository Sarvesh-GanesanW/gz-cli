//! Splits SQL text into statements on `;`, ignoring semicolons inside
//! quotes ('…', "…", `…`) and comments (`-- …`, `/* … */`).

/// Complete statements (without their `;`) and the unfinished rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub statements: Vec<String>,
    pub rest: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    Code,
    Quoted(char),
    LineComment,
    BlockComment,
}

pub fn split(text: &str) -> Split {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut scan = Scan::Code;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match scan {
            Scan::Code => match ch {
                ';' => {
                    push_statement(&mut statements, &current);
                    current.clear();
                    continue;
                }
                '\'' | '"' | '`' => scan = Scan::Quoted(ch),
                '-' if chars.peek() == Some(&'-') => scan = Scan::LineComment,
                '/' if chars.peek() == Some(&'*') => scan = Scan::BlockComment,
                _ => {}
            },
            Scan::Quoted(quote) => {
                // A doubled quote ('it''s') stays inside the string.
                if ch == quote && chars.peek() == Some(&quote) {
                    current.push(ch);
                    current.push(chars.next().unwrap_or(quote));
                    continue;
                }
                if ch == quote {
                    scan = Scan::Code;
                }
            }
            Scan::LineComment if ch == '\n' => scan = Scan::Code,
            Scan::BlockComment if ch == '*' && chars.peek() == Some(&'/') => {
                current.push(ch);
                current.push(chars.next().unwrap_or('/'));
                scan = Scan::Code;
                continue;
            }
            Scan::LineComment | Scan::BlockComment => {}
        }
        current.push(ch);
    }
    Split {
        statements,
        rest: current.trim().to_string(),
    }
}

fn push_statement(statements: &mut Vec<String>, text: &str) {
    if has_code(text) {
        statements.push(text.trim().to_string());
    }
}

/// True when `text` holds something other than whitespace and comments.
pub fn has_code(text: &str) -> bool {
    !strip_comments(text).trim().is_empty()
}

fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    let mut in_block = false;
    while let Some(line) = lines.next() {
        let mut rest = line;
        loop {
            if in_block {
                match rest.find("*/") {
                    Some(end) => {
                        rest = &rest[end + 2..];
                        in_block = false;
                    }
                    None => break,
                }
            } else if let Some(start) = rest.find("/*") {
                out.push_str(&rest[..start]);
                rest = &rest[start + 2..];
                in_block = true;
            } else {
                out.push_str(rest.split("--").next().unwrap_or(""));
                break;
            }
        }
        if lines.peek().is_some() {
            out.push('\n');
        }
    }
    out
}

/// True when Enter should run the input: it ends a statement with `;`
/// (outside quotes and comments) or is a `\` meta command.
pub fn is_ready(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.starts_with('\\') {
        return true;
    }
    let parts = split(text);
    !parts.statements.is_empty() && !has_code(&parts.rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_semicolons() {
        let parts = split("select 1; select 2;\n select 3");
        assert_eq!(parts.statements, vec!["select 1", "select 2"]);
        assert_eq!(parts.rest, "select 3");
    }

    #[test]
    fn ignores_semicolons_in_strings_and_identifiers() {
        let parts = split("select 'a;b', \"x;y\", `c;d`, 'it''s;ok' from t;");
        assert_eq!(parts.statements.len(), 1);
        assert!(parts.statements[0].ends_with("from t"));
    }

    #[test]
    fn ignores_semicolons_in_comments() {
        let parts = split("select 1 -- not; here\n, 2; /* nor; here */ select 3;");
        assert_eq!(parts.statements.len(), 2, "{parts:?}");
        assert!(parts.statements[0].contains(", 2"));
        assert!(parts.statements[1].ends_with("select 3"));
    }

    #[test]
    fn drops_empty_and_comment_only_statements() {
        let parts = split(";; -- only a comment\n;select 1;");
        assert_eq!(parts.statements, vec!["select 1"]);
    }

    #[test]
    fn readiness_needs_a_closing_semicolon_or_a_meta_command() {
        assert!(!is_ready("select 1"));
        assert!(!is_ready("select ';'"));
        assert!(is_ready("select 1;"));
        assert!(is_ready("select 1; -- trailing comment"));
        assert!(!is_ready("select 1; select"));
        assert!(is_ready("\\dt"));
        assert!(!is_ready("/* open; comment"));
    }
}

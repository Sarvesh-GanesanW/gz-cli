//! What `gz explain` reads from a dataset's text besides its tables: the
//! expression behind a selected column, and the SQL written inside Python.

use crate::sql::tokens::{spanned_tokens, tokenize, Spanned, Token};

/// The expression the query's outermost `SELECT` names `column`
/// (`ROUND(SUM(revenue27), 0)` for `... AS Revenue`), compared without
/// case. A bare column selected under its own name is returned as is.
/// `None` when the column is not named in that list (`SELECT *`, or a
/// name built at run time).
pub fn select_expression(sql: &str, column: &str) -> Option<String> {
    let chars: Vec<char> = sql.chars().collect();
    let tokens = spanned_tokens(sql);
    let items = outer_select_items(&tokens)?;
    let wanted = column.to_ascii_lowercase();
    items.into_iter().find_map(|item| {
        let (expression, alias) = split_alias(item)?;
        if alias.to_ascii_lowercase() != wanted {
            return None;
        }
        let range = expression.first()?.1.start..expression.last()?.1.end;
        Some(chars[range].iter().collect::<String>())
    })
}

/// The comma-separated items of the last `SELECT` at the top level (the
/// one after any CTEs), up to its `FROM`.
fn outer_select_items(tokens: &[Spanned]) -> Option<Vec<&[Spanned]>> {
    let mut depth = 0usize;
    let mut select = None;
    for (i, (token, _)) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth == 0 && token.is_keyword("SELECT") => select = Some(i),
            _ => {}
        }
    }
    let start = select? + 1;
    let mut items = Vec::new();
    let mut item_start = start;
    depth = 0;
    for (i, (token, _)) in tokens.iter().enumerate().skip(start) {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Punct(',') if depth == 0 => {
                items.push(&tokens[item_start..i]);
                item_start = i + 1;
            }
            _ if depth == 0 && (token.is_keyword("FROM") || *token == Token::Punct(';')) => {
                items.push(&tokens[item_start..i]);
                return Some(items);
            }
            _ => {}
        }
    }
    items.push(&tokens[item_start..]);
    Some(items)
}

/// An item's expression and the name it goes by: `expr AS name`,
/// `expr name`, or a column `t.name` by itself.
fn split_alias(item: &[Spanned]) -> Option<(&[Spanned], String)> {
    let skip_distinct = usize::from(
        item.first()
            .is_some_and(|(t, _)| t.is_keyword("DISTINCT") || t.is_keyword("ALL")),
    );
    let item = &item[skip_distinct..];
    let (last, _) = item.last()?;
    let name = match last {
        Token::Word(word) => word.clone(),
        Token::Quoted(name) => name.clone(),
        _ => return None,
    };
    if item.len() == 1 {
        return Some((item, name));
    }
    let before = &item[item.len() - 2].0;
    if before.is_keyword("AS") {
        return Some((&item[..item.len() - 2], name));
    }
    if *before == Token::Punct('.') {
        return Some((item, name));
    }
    let ends_expression = matches!(
        before,
        Token::Punct(')') | Token::Word(_) | Token::Quoted(_) | Token::Literal
    );
    ends_expression.then(|| (&item[..item.len() - 1], name))
}

/// SQL written inside a Python dataset (`readGzLakehouse(query="...")`, a
/// `sql = """..."""` block): every string literal that reads like a query.
/// Best effort, since the code may build its SQL at run time.
pub fn sql_in_python(code: &str) -> Vec<String> {
    python_strings(code)
        .into_iter()
        .filter(|text| looks_like_query(text))
        .collect()
}

fn looks_like_query(text: &str) -> bool {
    let tokens = tokenize(text);
    let has = |word: &str| tokens.iter().any(|t| t.is_keyword(word));
    has("SELECT") && has("FROM")
}

/// The string literals in Python code, triple-quoted ones included; an
/// f-string's `{...}` parts are kept as written.
fn python_strings(code: &str) -> Vec<String> {
    let chars: Vec<char> = code.chars().collect();
    let mut strings = Vec::new();
    let mut at = 0;
    while let Some(&c) = chars.get(at) {
        if c == '#' {
            at = find(&chars, at, &['\n']).unwrap_or(chars.len());
            continue;
        }
        if c != '"' && c != '\'' {
            at += 1;
            continue;
        }
        let triple = [c, c, c];
        let (end, text) = if chars[at..].starts_with(&triple) {
            let end = find(&chars, at + 3, &triple).unwrap_or(chars.len());
            (
                (end + 3).min(chars.len()),
                chars[at + 3..end].iter().collect(),
            )
        } else {
            python_short_string(&chars, at, c)
        };
        strings.push(text);
        at = end;
    }
    strings
}

/// Where `seq` next starts, searching from `start`.
fn find(chars: &[char], start: usize, seq: &[char]) -> Option<usize> {
    (start..chars.len()).find(|&i| chars[i..].starts_with(seq))
}

fn python_short_string(chars: &[char], start: usize, quote: char) -> (usize, String) {
    let mut text = String::new();
    let mut at = start + 1;
    while at < chars.len() && chars[at] != quote && chars[at] != '\n' {
        if chars[at] == '\\' && at + 1 < chars.len() {
            text.push(chars[at + 1]);
            at += 2;
            continue;
        }
        text.push(chars[at]);
        at += 1;
    }
    ((at + 1).min(chars.len()), text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::tables::tables_read;

    fn names(sql: &str) -> Vec<String> {
        tables_read(sql)
            .tables
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn finds_the_expression_behind_a_selected_column() {
        let sql = "WITH t AS (SELECT a AS Revenue FROM x.y)\r\nSELECT\r\n    \"Year\" AS Year,\r\n    ROUND(SUM(revenue27), 0) AS Revenue,\r\n    COUNT(DISTINCT n.\"ncr_id\") \"NCR_Count\", o.region\r\nFROM DemoDB.bi_working_capital o GROUP BY \"Year\"";
        assert_eq!(
            select_expression(sql, "revenue").as_deref(),
            Some("ROUND(SUM(revenue27), 0)"),
            "the outer SELECT, not the CTE's"
        );
        assert_eq!(
            select_expression(sql, "NCR_Count").as_deref(),
            Some("COUNT(DISTINCT n.\"ncr_id\")")
        );
        assert_eq!(
            select_expression(sql, "region").as_deref(),
            Some("o.region")
        );
        assert_eq!(select_expression(sql, "Year").as_deref(), Some("\"Year\""));
        assert_eq!(select_expression("SELECT * FROM x.y", "Revenue"), None);
    }

    #[test]
    fn finds_the_sql_written_inside_python_code() {
        let code = r#"
from GZ.DuckDBUtils import duckSession
# query = "SELECT * FROM commented.out"
duck = duckSession(siteName="dev")
sql = """
SELECT "order_date", SUM("quantity") FROM "demo_sales"."orders" GROUP BY 1
"""
other = duck.readGzLakehouse(connName="Conn", query='SELECT * FROM demo_sales.fake_names LIMIT 10')
"#;
        let found = sql_in_python(code);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(names(&found[0]), ["demo_sales.orders"]);
        assert_eq!(names(&found[1]), ["demo_sales.fake_names"]);
    }
}

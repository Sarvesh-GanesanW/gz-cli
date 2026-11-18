//! Tab completion for the SQL shell: keywords, meta commands, and the
//! warehouse's names from the session's [`Catalog`]. Which names fit
//! depends on where the cursor is: tables after `FROM`/`JOIN`, databases
//! after `SHOW TABLES IN`, and the columns of the statement's own tables
//! after `SELECT`, `WHERE`, `ON`, `BY` and the like. Pure: text and a
//! cursor go in, the word to replace and its candidates come out.

use super::catalog::{Catalog, Table};
use super::tables::{name_of, tables_read};
use super::tokens::{spanned_tokens, Token};

/// What may replace the word before the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Char index where the replaced text starts; it ends at the cursor.
    pub start: usize,
    pub candidates: Vec<String>,
}

/// What kind of name the cursor's position takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Table,
    Database,
    Column,
    Keyword,
}

/// Keywords and common functions offered as words (Spark SQL).
const KEYWORDS: &[&str] = &[
    "ALL",
    "AND",
    "ANTI",
    "ARRAY",
    "AS",
    "ASC",
    "AVG",
    "BETWEEN",
    "BIGINT",
    "BOOLEAN",
    "BY",
    "CASE",
    "CAST",
    "COALESCE",
    "COLUMNS",
    "CONCAT",
    "COUNT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DATABASES",
    "DATE",
    "DATE_ADD",
    "DATE_FORMAT",
    "DATE_SUB",
    "DATE_TRUNC",
    "DATEDIFF",
    "DECIMAL",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DOUBLE",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "EXTENDED",
    "FALSE",
    "FILTER",
    "FIRST",
    "FLOAT",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INNER",
    "INT",
    "INTERSECT",
    "INTERVAL",
    "IS",
    "JOIN",
    "LAST",
    "LATERAL",
    "LEFT",
    "LENGTH",
    "LIKE",
    "LIMIT",
    "LOWER",
    "MAX",
    "MIN",
    "NOT",
    "NULL",
    "NULLS",
    "NULLIF",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PARTITIONS",
    "QUALIFY",
    "RANGE",
    "RANK",
    "RIGHT",
    "RLIKE",
    "ROUND",
    "ROW_NUMBER",
    "ROWS",
    "SELECT",
    "SEMI",
    "SHOW",
    "STRING",
    "SUBSTRING",
    "SUM",
    "TABLE",
    "TABLES",
    "TABLESAMPLE",
    "THEN",
    "TIMESTAMP",
    "TO_DATE",
    "TRIM",
    "TRUE",
    "UNION",
    "UPPER",
    "USING",
    "VALUES",
    "VIEWS",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "YEAR",
];

/// Words after which a table name goes.
const TABLE_WORDS: &[&str] = &["FROM", "JOIN", "INTO", "UPDATE", "TABLE"];
/// Words after which a column or expression goes.
const COLUMN_WORDS: &[&str] = &[
    "SELECT", "WHERE", "AND", "OR", "ON", "BY", "HAVING", "SET", "WHEN", "THEN", "ELSE", "NOT",
    "DISTINCT", "CASE", "BETWEEN", "IN", "LIKE", "ILIKE", "RLIKE", "IS", "QUALIFY",
];
/// Clause words that decide what a `,` continues.
const CLAUSE_WORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "ON", "BY", "HAVING", "SET", "VALUES", "USING", "QUALIFY", "LIMIT",
];

/// Completes the word before `cursor` (a char index) in `text`: the
/// shell's whole input, maybe several statements or a `\` command.
pub fn complete(text: &str, cursor: usize, catalog: Option<&Catalog>) -> Option<Completion> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let word_start = (0..cursor)
        .rev()
        .take_while(|&at| is_word_char(chars[at]))
        .last()
        .unwrap_or(cursor);
    let word: String = chars[word_start..cursor].iter().collect();
    let (qualifiers, last) = split_word(&word);
    let start = cursor - last.chars().count();
    let before: String = chars[..word_start].iter().collect();

    let candidates = if before.trim_start().starts_with('\\') || word.starts_with('\\') {
        meta_candidates(&before, &word, &qualifiers, catalog)
    } else {
        let after: String = chars[cursor..].iter().collect();
        sql_candidates(&before, &after, &qualifiers, &last, catalog)
    };
    let wanted = last.trim_start_matches('`').to_ascii_lowercase();
    let mut matching: Vec<String> = Vec::new();
    for candidate in candidates {
        let bare = candidate.trim_start_matches('`').to_ascii_lowercase();
        if bare.starts_with(&wanted) && !matching.contains(&candidate) {
            matching.push(candidate);
        }
    }
    if matching.is_empty() {
        return None;
    }
    Some(Completion {
        start,
        candidates: matching,
    })
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '$' | '.' | '`' | '\\')
}

/// `db.ta` → (["db"], "ta"); backticks come off the qualifiers.
fn split_word(word: &str) -> (Vec<String>, String) {
    let mut parts: Vec<&str> = word.split('.').collect();
    let last = parts.pop().unwrap_or_default().to_string();
    let qualifiers = parts
        .into_iter()
        .map(|p| p.trim_matches('`').to_string())
        .collect();
    (qualifiers, last)
}

/// A name as SQL needs it: backticked unless it is a plain identifier.
fn quoted(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

/// Keywords in the case the user is typing.
fn keywords(typed: &str) -> Vec<String> {
    let lower = typed.chars().any(|c| c.is_ascii_lowercase());
    KEYWORDS
        .iter()
        .map(|k| {
            if lower {
                k.to_ascii_lowercase()
            } else {
                k.to_string()
            }
        })
        .collect()
}

fn meta_candidates(
    before: &str,
    word: &str,
    qualifiers: &[String],
    catalog: Option<&Catalog>,
) -> Vec<String> {
    let command = before.split_whitespace().next();
    let Some(command) = command else {
        return super::meta::COMMANDS
            .iter()
            .filter(|c| c.starts_with(word))
            .map(|c| c.to_string())
            .collect();
    };
    let Some(catalog) = catalog else {
        return Vec::new();
    };
    match command {
        "\\d" if !qualifiers.is_empty() => qualified_names(catalog, qualifiers, &[]),
        "\\d" | "\\dt" => database_names(catalog),
        _ => Vec::new(),
    }
}

fn sql_candidates(
    before: &str,
    after: &str,
    qualifiers: &[String],
    last: &str,
    catalog: Option<&Catalog>,
) -> Vec<String> {
    let head = current_statement_head(before);
    let statement = format!("{head}{}", next_statement_tail(after));
    let context = context(&head);
    let Some(catalog) = catalog else {
        return match context {
            Context::Keyword | Context::Column if !last.is_empty() => keywords(last),
            _ => Vec::new(),
        };
    };
    let sources = statement_sources(catalog, &statement);
    if !qualifiers.is_empty() {
        return qualified_names(catalog, qualifiers, &sources);
    }
    match context {
        Context::Table => {
            let mut names = database_names(catalog);
            names.push(quoted(&catalog.warehouse));
            names.extend(catalog.warehouses.iter().map(|w| quoted(w)));
            names
        }
        Context::Database => database_names(catalog),
        Context::Column => {
            let mut names: Vec<String> = sources
                .iter()
                .flat_map(|s| s.table.columns.iter().map(|c| quoted(&c.name)))
                .collect();
            names.extend(sources.iter().map(|s| quoted(&s.name)));
            if !last.is_empty() {
                names.extend(keywords(last));
            }
            names
        }
        Context::Keyword if !last.is_empty() => keywords(last),
        Context::Keyword => Vec::new(),
    }
}

fn database_names(catalog: &Catalog) -> Vec<String> {
    catalog.databases.iter().map(|d| quoted(&d.name)).collect()
}

/// Names after `qualifier.`: a source's columns, a database's tables, or
/// the warehouse's databases.
fn qualified_names(catalog: &Catalog, qualifiers: &[String], sources: &[Source]) -> Vec<String> {
    let columns = |table: &Table| table.columns.iter().map(|c| quoted(&c.name)).collect();
    if let [single] = qualifiers {
        if let Some(source) = sources.iter().find(|s| s.name.eq_ignore_ascii_case(single)) {
            return columns(source.table);
        }
        if let Some(database) = catalog.database(single) {
            return database.tables.iter().map(|t| quoted(&t.name)).collect();
        }
        if catalog.is_current_warehouse(single) {
            return database_names(catalog);
        }
        return Vec::new();
    }
    if let [warehouse, db] = qualifiers {
        if catalog.is_current_warehouse(warehouse) {
            return catalog
                .database(db)
                .map(|d| d.tables.iter().map(|t| quoted(&t.name)).collect())
                .unwrap_or_default();
        }
    }
    catalog
        .table(qualifiers)
        .map(|(_, table)| columns(table))
        .unwrap_or_default()
}

/// The statement the cursor is in, up to the word being typed.
fn current_statement_head(before: &str) -> String {
    let start = spanned_tokens(before)
        .iter()
        .rev()
        .find(|(token, _)| *token == Token::Punct(';'))
        .map_or(0, |(_, range)| range.end);
    before.chars().skip(start).collect()
}

/// The rest of the cursor's statement, up to its `;`.
fn next_statement_tail(after: &str) -> String {
    let end = spanned_tokens(after)
        .iter()
        .find(|(token, _)| *token == Token::Punct(';'))
        .map_or(after.chars().count(), |(_, range)| range.start);
    after.chars().take(end).collect()
}

fn context(head: &str) -> Context {
    let tokens: Vec<Token> = spanned_tokens(head).into_iter().map(|(t, _)| t).collect();
    let Some(last) = tokens.last() else {
        return Context::Keyword;
    };
    let previous = tokens.len().checked_sub(2).map(|at| &tokens[at]);
    if let Some(word) = last.keyword() {
        if TABLE_WORDS.contains(&word.as_str())
            || (matches!(word.as_str(), "DESCRIBE" | "DESC") && tokens.len() == 1)
        {
            return Context::Table;
        }
        if matches!(word.as_str(), "DATABASE" | "SCHEMA" | "NAMESPACE")
            || (word == "IN" && previous.is_some_and(|p| p.is_keyword("TABLES")))
        {
            return Context::Database;
        }
        if COLUMN_WORDS.contains(&word.as_str()) {
            return Context::Column;
        }
    }
    match last {
        Token::Punct(',') => match clause(&tokens) {
            Some(word) if word == "FROM" => Context::Table,
            Some(_) => Context::Column,
            None => Context::Keyword,
        },
        Token::Punct(')') => Context::Keyword,
        Token::Punct(_) => Context::Column,
        _ => Context::Keyword,
    }
}

/// The clause word governing the end of `tokens`, at its own bracket level.
fn clause(tokens: &[Token]) -> Option<String> {
    let mut depth = 0usize;
    for token in tokens.iter().rev() {
        match token {
            Token::Punct(')') => depth += 1,
            Token::Punct('(') if depth == 0 => return Some("SELECT".into()),
            Token::Punct('(') => depth -= 1,
            _ if depth > 0 => {}
            _ => {
                if let Some(word) = token.keyword() {
                    if CLAUSE_WORDS.contains(&word.as_str()) || word == "JOIN" {
                        return Some(if word == "JOIN" { "FROM".into() } else { word });
                    }
                }
            }
        }
    }
    None
}

/// A table the statement reads, under the name its columns are
/// qualified with (an alias, or the table's own name).
struct Source<'a> {
    name: String,
    table: &'a Table,
}

fn statement_sources<'a>(catalog: &'a Catalog, statement: &str) -> Vec<Source<'a>> {
    let refs = tables_read(statement).tables;
    let runs = name_runs(statement);
    let mut sources = Vec::new();
    for reference in refs {
        let table = match reference.parts.as_slice() {
            [only] => match catalog.tables_named(only).as_slice() {
                [(_, table)] => *table,
                _ => continue,
            },
            parts => match catalog.table(parts) {
                Some((_, table)) => table,
                None => continue,
            },
        };
        sources.push(Source {
            name: table.name.clone(),
            table,
        });
        for run in runs
            .iter()
            .filter(|r| same_parts(&r.parts, &reference.parts))
        {
            if let Some(alias) = &run.alias {
                sources.push(Source {
                    name: alias.clone(),
                    table,
                });
            }
        }
    }
    sources
}

fn same_parts(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// A dotted name in the text and the alias written after it, if any.
struct NameRun {
    parts: Vec<String>,
    alias: Option<String>,
}

fn name_runs(statement: &str) -> Vec<NameRun> {
    let tokens: Vec<Token> = spanned_tokens(statement)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    let mut runs = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        let Some(first) = name_of(&tokens[at]) else {
            at += 1;
            continue;
        };
        let mut parts = vec![first.to_string()];
        at += 1;
        while tokens.get(at) == Some(&Token::Punct('.')) {
            match tokens.get(at + 1).and_then(name_of) {
                Some(part) => parts.push(part.to_string()),
                None => break,
            }
            at += 2;
        }
        let alias_at = if tokens.get(at).is_some_and(|t| t.is_keyword("AS")) {
            at + 1
        } else {
            at
        };
        let alias = tokens.get(alias_at).and_then(name_of).map(str::to_string);
        runs.push(NameRun { parts, alias });
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::catalog::tests::sample;

    fn at_end(text: &str) -> Vec<String> {
        complete(text, text.chars().count(), Some(&sample()))
            .map(|c| c.candidates)
            .unwrap_or_default()
    }

    #[test]
    fn tables_follow_from_and_join_as_database_then_table() {
        assert_eq!(
            at_end("select * from "),
            vec!["DemoDB", "sales", "empty_db", "WH", "Other"]
        );
        assert_eq!(at_end("select * from De"), vec!["DemoDB"]);
        assert_eq!(
            at_end("select * from demodb.Pro"),
            vec!["ProductionMetrics", "product"]
        );
        assert_eq!(at_end("select * from WH.sales."), vec!["orders"]);
        assert_eq!(
            at_end("select * from a.b x join WH."),
            vec!["DemoDB", "sales", "empty_db"]
        );
    }

    #[test]
    fn columns_come_from_the_statements_own_tables() {
        let text = "select  from DemoDB.product";
        let completion = complete(text, 7, Some(&sample())).unwrap();
        assert_eq!(completion.start, 7);
        assert_eq!(completion.candidates, vec!["plant_id", "price", "product"]);
        assert_eq!(
            at_end("select * from DemoDB.ProductionMetrics where un"),
            vec!["units_produced", "union"]
        );
    }

    #[test]
    fn aliases_qualify_columns() {
        let text = "select p.pr from DemoDB.product p join sales.orders AS o on o.";
        let completion = complete(text, 11, Some(&sample())).unwrap();
        assert_eq!(
            (completion.start, completion.candidates),
            (9, vec!["price".to_string()])
        );
        assert_eq!(
            at_end(text),
            vec!["`order id`"],
            "names that need it are quoted"
        );
    }

    #[test]
    fn commas_continue_the_clause_they_are_in() {
        let text = "select plant_id,  from DemoDB.product";
        let candidates = complete(text, 17, Some(&sample())).unwrap().candidates;
        assert_eq!(candidates[0], "plant_id");
        assert_eq!(at_end("select 1 from DemoDB.product, s"), vec!["sales"]);
    }

    #[test]
    fn keywords_follow_the_case_being_typed() {
        assert_eq!(at_end("sel"), vec!["select"]);
        assert_eq!(at_end("SEL"), vec!["SELECT"]);
        assert_eq!(at_end("select a fr"), vec!["from"]);
        assert!(at_end("select a ").is_empty(), "no word, no keyword list");
    }

    #[test]
    fn databases_follow_show_tables_in() {
        assert_eq!(at_end("show tables in s"), vec!["sales"]);
    }

    #[test]
    fn only_the_cursors_statement_counts() {
        let text = "select * from sales.orders; select pl from DemoDB.product;";
        let cursor = "select * from sales.orders; select pl".chars().count();
        let completion = complete(text, cursor, Some(&sample())).unwrap();
        assert_eq!(completion.candidates, vec!["plant_id"]);
    }

    #[test]
    fn meta_commands_and_their_arguments() {
        assert_eq!(at_end("\\ti"), vec!["\\timing"]);
        assert_eq!(at_end("\\dt s"), vec!["sales"]);
        assert_eq!(
            at_end("\\d DemoDB.prod"),
            vec!["ProductionMetrics", "product"]
        );
        assert!(at_end("\\o fi").is_empty());
    }

    #[test]
    fn without_a_catalog_only_keywords_complete() {
        let completion = complete("sel", 3, None).unwrap();
        assert_eq!(completion.candidates, vec!["select"]);
        assert!(complete("select * from De", 16, None).is_none());
    }
}

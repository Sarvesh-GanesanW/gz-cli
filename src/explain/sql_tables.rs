//! The tables a SQL query reads, found without running it.
//!
//! Datasets are written for the lakehouse (Spark SQL, or DuckDB through
//! `readGzLakehouse`), with names quoted as `"x"` or `` `x` ``, Designer
//! filter placeholders (`{{Region.value}}`), CTEs, subqueries and aliases.
//! No endpoint returns the tables of a query (the provider's own extractor,
//! `utils/table_references.py`, is used only inside its authorizer), so gz
//! reads the text itself: a tokenizer plus the few rules that decide which
//! names after `FROM` and `JOIN` are tables. Whatever it cannot decide (a
//! table function, a placeholder where a table goes) is reported, not
//! guessed.

/// A table name as the query writes it: one to three dotted parts
/// (`table`, `db.table`, `catalog.db.table`), unquoted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableRef {
    pub parts: Vec<String>,
}

impl std::fmt::Display for TableRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.parts.join("."))
    }
}

/// What a query reads, and what in it could not be read as a table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryTables {
    /// In the order first seen, without duplicates.
    pub tables: Vec<TableRef>,
    /// Sources gz could not resolve to a table, as written
    /// (`read_parquet(...)`, `{{source.value}}`).
    pub unresolved: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A bare word: keyword or unquoted name.
    Word(String),
    /// A `"quoted"` or `` `quoted` `` name, unquoted.
    Quoted(String),
    /// A string or number literal; its text does not matter here.
    Literal,
    /// A `{{...}}` Designer placeholder, as written.
    Placeholder(String),
    Punct(char),
}

impl Token {
    fn keyword(&self) -> Option<String> {
        match self {
            Token::Word(word) => Some(word.to_ascii_uppercase()),
            _ => None,
        }
    }

    fn is_keyword(&self, wanted: &str) -> bool {
        self.keyword().as_deref() == Some(wanted)
    }

    fn name(&self) -> Option<&str> {
        match self {
            Token::Word(word) if !is_reserved(word) => Some(word),
            Token::Quoted(name) => Some(name),
            _ => None,
        }
    }
}

/// Words that end a table reference or start a clause, so they are never
/// taken as a table name or an alias.
const RESERVED: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "MINUS",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "NATURAL",
    "SEMI",
    "ANTI",
    "ON",
    "USING",
    "AS",
    "WITH",
    "LATERAL",
    "VIEW",
    "WINDOW",
    "QUALIFY",
    "VALUES",
    "SET",
    "INTO",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "BY",
    "TABLESAMPLE",
    "VERSION",
    "TIMESTAMP",
    "FOR",
    "PIVOT",
    "UNPIVOT",
    "CLUSTER",
    "DISTRIBUTE",
    "SORT",
    "FETCH",
    "RETURNING",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "CASE",
    "ALL",
    "DISTINCT",
    "POSITIONAL",
    "ASOF",
];

fn is_reserved(word: &str) -> bool {
    RESERVED.contains(&word.to_ascii_uppercase().as_str())
}

fn tokenize(sql: &str) -> Vec<Token> {
    spanned_tokens(sql)
        .into_iter()
        .map(|(token, _)| token)
        .collect()
}

/// Tokens with the char range each one covers in the text.
fn spanned_tokens(sql: &str) -> Vec<(Token, std::ops::Range<usize>)> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut spans = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let start = at;
        let count = tokens.len();
        let c = chars[at];
        let next = chars.get(at + 1).copied();
        if c.is_whitespace() {
            at += 1;
        } else if c == '-' && next == Some('-') {
            at = skip_until(&chars, at, |chars, i| chars[i] == '\n');
        } else if c == '/' && next == Some('*') {
            at = skip_block_comment(&chars, at);
        } else if c == '{' && next == Some('{') {
            let end = find_seq(&chars, at + 2, &['}', '}']).map_or(chars.len(), |e| e + 2);
            tokens.push(Token::Placeholder(chars[at..end].iter().collect()));
            at = end;
        } else if c == '\'' {
            at = skip_quoted(&chars, at, '\'').0;
            tokens.push(Token::Literal);
        } else if c == '"' || c == '`' {
            let (end, text) = skip_quoted(&chars, at, c);
            tokens.push(Token::Quoted(text));
            at = end;
        } else if c.is_ascii_digit() {
            at = skip_until(&chars, at, |chars, i| {
                !(chars[i].is_ascii_alphanumeric() || chars[i] == '.')
            });
            tokens.push(Token::Literal);
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let end = skip_until(&chars, at, |chars, i| {
                !(chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            });
            tokens.push(Token::Word(chars[at..end].iter().collect()));
            at = end;
        } else {
            tokens.push(Token::Punct(c));
            at += 1;
        }
        if tokens.len() > count {
            spans.push(start..at);
        }
    }
    tokens.into_iter().zip(spans).collect()
}

/// The index of the first char from `start` that `stop` accepts, or the end.
fn skip_until(chars: &[char], start: usize, stop: impl Fn(&[char], usize) -> bool) -> usize {
    (start..chars.len())
        .find(|&i| stop(chars, i))
        .unwrap_or(chars.len())
}

fn find_seq(chars: &[char], start: usize, seq: &[char]) -> Option<usize> {
    (start..chars.len()).find(|&i| chars[i..].starts_with(seq))
}

fn skip_block_comment(chars: &[char], start: usize) -> usize {
    find_seq(chars, start + 2, &['*', '/']).map_or(chars.len(), |end| end + 2)
}

/// Skips a quoted run starting at `start` (a doubled quote is an escaped
/// one) and returns the index after it with the text inside.
fn skip_quoted(chars: &[char], start: usize, quote: char) -> (usize, String) {
    let mut text = String::new();
    let mut at = start + 1;
    while at < chars.len() {
        if chars[at] == quote {
            if chars.get(at + 1) == Some(&quote) {
                text.push(quote);
                at += 2;
                continue;
            }
            return (at + 1, text);
        }
        text.push(chars[at]);
        at += 1;
    }
    (at, text)
}

/// One level of parentheses: whether a `SELECT` has started in it, which
/// is what makes a `FROM` there a clause and not part of `EXTRACT(x FROM y)`.
#[derive(Default)]
struct Level {
    has_select: bool,
    /// Inside a `FROM` list, where a comma starts another table.
    in_from: bool,
}

struct Walker {
    tokens: Vec<Token>,
    at: usize,
    levels: Vec<Level>,
    ctes: Vec<String>,
    found: QueryTables,
}

/// The tables `sql` reads. Names of CTEs defined in the query are not
/// tables and are left out.
pub fn tables_read(sql: &str) -> QueryTables {
    let tokens = tokenize(sql);
    let ctes = cte_names(&tokens);
    let mut walker = Walker {
        tokens,
        at: 0,
        levels: vec![Level::default()],
        ctes,
        found: QueryTables::default(),
    };
    walker.walk();
    walker.found
}

/// Every `name AS (` that follows `WITH` or a CTE's closing `),`.
fn cte_names(tokens: &[Token]) -> Vec<String> {
    let mut names = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let starts_cte = token.is_keyword("WITH") || token.is_keyword("RECURSIVE");
        let follows_cte = *token == Token::Punct(',')
            && i > 0
            && tokens[i - 1] == Token::Punct(')')
            && !names.is_empty();
        if !(starts_cte || follows_cte) {
            continue;
        }
        let Some(name) = tokens.get(i + 1).and_then(Token::name) else {
            continue;
        };
        // `name (col, ...) AS (` lists the CTE's columns first.
        let mut after = i + 2;
        if tokens.get(after) == Some(&Token::Punct('(')) {
            after = matching_paren(tokens, after) + 1;
        }
        let is_cte = tokens.get(after).is_some_and(|t| t.is_keyword("AS"))
            && tokens.get(after + 1) == Some(&Token::Punct('('));
        if is_cte {
            names.push(name.to_ascii_lowercase());
        }
    }
    names
}

fn matching_paren(tokens: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

impl Walker {
    fn level(&mut self) -> &mut Level {
        // The outermost level is never popped, so there is always one.
        if self.levels.is_empty() {
            self.levels.push(Level::default());
        }
        let last = self.levels.len() - 1;
        &mut self.levels[last]
    }

    fn peek(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.at + offset)
    }

    fn walk(&mut self) {
        while self.at < self.tokens.len() {
            let token = self.tokens[self.at].clone();
            self.at += 1;
            match token {
                Token::Punct('(') => self.levels.push(Level::default()),
                Token::Punct(')') => {
                    if self.levels.len() > 1 {
                        self.levels.pop();
                    }
                }
                Token::Punct(',') if self.level().in_from => self.read_source(),
                Token::Punct(';') => *self.level() = Level::default(),
                Token::Word(word) => self.on_word(&word.to_ascii_uppercase()),
                _ => {}
            }
        }
    }

    fn on_word(&mut self, keyword: &str) {
        match keyword {
            "SELECT" => {
                let level = self.level();
                level.has_select = true;
                level.in_from = false;
            }
            "FROM" if self.is_clause_from() => {
                self.level().in_from = true;
                self.read_source();
            }
            "JOIN" => {
                self.level().in_from = true;
                self.read_source();
            }
            // `ON` and `USING` stay in the FROM list: a comma after a join
            // condition starts another table.
            "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "UNION" | "INTERSECT" | "EXCEPT"
            | "WINDOW" | "QUALIFY" => self.level().in_from = false,
            _ => {}
        }
    }

    /// A `FROM` that starts a clause: after a `SELECT` at this level, and
    /// not `IS [NOT] DISTINCT FROM`.
    fn is_clause_from(&mut self) -> bool {
        let before = self.at.checked_sub(2).and_then(|i| self.tokens.get(i));
        let after_distinct = before.is_some_and(|t| t.is_keyword("DISTINCT"));
        self.level().has_select && !after_distinct
    }

    /// Reads the source after `FROM`, `JOIN` or a comma in a `FROM` list.
    fn read_source(&mut self) {
        while self.peek(0).is_some_and(|t| t.is_keyword("LATERAL")) {
            self.at += 1;
        }
        match self.peek(0).cloned() {
            // A subquery: the walk goes on inside it.
            Some(Token::Punct('(')) => {}
            Some(Token::Placeholder(text)) => {
                self.found.unresolved.push(text);
                self.at += 1;
            }
            Some(token) if token.name().is_some() => self.read_name(),
            _ => {}
        }
    }

    fn read_name(&mut self) {
        let mut parts = Vec::new();
        while let Some(part) = self.peek(0).and_then(Token::name).map(str::to_string) {
            parts.push(part);
            self.at += 1;
            if self.peek(0) != Some(&Token::Punct('.')) {
                break;
            }
            self.at += 1;
        }
        // A table function; the walk goes on into its arguments, which may
        // hold a subquery.
        if self.peek(0) == Some(&Token::Punct('(')) {
            self.found
                .unresolved
                .push(format!("{}(...)", parts.join(".")));
            return;
        }
        let is_cte = parts.len() == 1 && self.ctes.contains(&parts[0].to_ascii_lowercase());
        if parts.len() > 3 {
            self.found.unresolved.push(parts.join("."));
            return;
        }
        if parts.is_empty() || is_cte {
            return;
        }
        let table = TableRef { parts };
        if !self.found.tables.contains(&table) {
            self.found.tables.push(table);
        }
    }
}

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

type Spanned = (Token, std::ops::Range<usize>);

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
    while at < chars.len() {
        let c = chars[at];
        if c == '#' {
            at = skip_until(&chars, at, |chars, i| chars[i] == '\n');
            continue;
        }
        if c != '"' && c != '\'' {
            at += 1;
            continue;
        }
        let triple = [c, c, c];
        let (end, text) = if chars[at..].starts_with(&triple) {
            let end = find_seq(&chars, at + 3, &triple).unwrap_or(chars.len());
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

    fn names(sql: &str) -> Vec<String> {
        tables_read(sql)
            .tables
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn reads_a_plain_table_and_its_alias_is_not_a_table() {
        assert_eq!(
            names("SELECT o.id FROM sales.orders o WHERE o.amount > 0"),
            ["sales.orders"]
        );
        assert_eq!(names("select * from orders as o"), ["orders"]);
    }

    #[test]
    fn unquotes_double_quoted_and_backticked_names() {
        assert_eq!(
            names(r#"SELECT * FROM "Manufacturing_Performance_Analytics"."fact_defect" AS p"#),
            ["Manufacturing_Performance_Analytics.fact_defect"]
        );
        assert_eq!(
            names("SELECT * FROM `cat`.`my db`.`odd``name`"),
            ["cat.my db.odd`name"]
        );
    }

    #[test]
    fn finds_every_join_and_comma_join_once() {
        let sql = r#"
            SELECT * FROM "mfg_ops"."nonconformance_reports" n
            LEFT JOIN "mfg_ops"."material_lots" m ON n.lot = m.lot_id
            LEFT OUTER JOIN mfg_ops.suppliers s USING (supplier_id), mfg_ops.plants p, mfg_ops.lines
            JOIN mfg_ops.suppliers s2 ON 1 = 1"#;
        assert_eq!(
            names(sql),
            [
                "mfg_ops.nonconformance_reports",
                "mfg_ops.material_lots",
                "mfg_ops.suppliers",
                "mfg_ops.plants",
                "mfg_ops.lines",
            ]
        );
    }

    #[test]
    fn leaves_out_ctes_but_reads_what_they_read() {
        let sql = r#"
            WITH weekly AS (SELECT * FROM "mfg_ops"."oee_daily"),
                 ranked (line, n) AS (SELECT line, count(*) FROM weekly GROUP BY line)
            SELECT * FROM weekly JOIN ranked ON TRUE"#;
        assert_eq!(names(sql), ["mfg_ops.oee_daily"]);
    }

    #[test]
    fn reads_subqueries_in_from_and_where() {
        let sql = "SELECT * FROM (SELECT * FROM a.x) t \
                   WHERE id IN (SELECT id FROM a.y) AND EXISTS (SELECT 1 FROM a.z)";
        assert_eq!(names(sql), ["a.x", "a.y", "a.z"]);
    }

    #[test]
    fn from_inside_a_function_or_a_comparison_is_not_a_table() {
        let sql = "SELECT EXTRACT(YEAR FROM order_date) AS y, \
                   SUBSTRING(name FROM 1 FOR 3), TRIM(BOTH ' ' FROM code) \
                   FROM sales.orders WHERE a IS DISTINCT FROM b AND c IS NOT DISTINCT FROM d";
        assert_eq!(names(sql), ["sales.orders"]);
    }

    #[test]
    fn skips_strings_comments_and_placeholders() {
        let sql = "-- FROM commented.out\n\
                   SELECT 'FROM fake.table' AS s /* JOIN nope.no */ \
                   FROM DemoDB.bi_working_capital \
                   WHERE supplier_region IN {{WC-Region.value}} AND \"Year\" IN {{WC-YEAR.value}}";
        assert_eq!(names(sql), ["DemoDB.bi_working_capital"]);
    }

    #[test]
    fn says_what_it_cannot_resolve() {
        let found = tables_read(
            "SELECT * FROM read_parquet('s3://x/*.parquet') JOIN {{source.value}} s ON TRUE",
        );
        assert!(found.tables.is_empty());
        assert_eq!(found.unresolved, ["read_parquet(...)", "{{source.value}}"]);
    }

    #[test]
    fn keeps_catalog_qualified_names_and_time_travel() {
        assert_eq!(
            names("SELECT * FROM gzlakehouse__conn.sales.orders VERSION AS OF 104"),
            ["gzlakehouse__conn.sales.orders"]
        );
    }

    #[test]
    fn handles_set_operations_and_several_statements() {
        let sql = "SELECT a FROM s.one UNION ALL SELECT a FROM s.two; SELECT * FROM s.three";
        assert_eq!(names(sql), ["s.one", "s.two", "s.three"]);
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

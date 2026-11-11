//! The lakehouse tables a Spark SQL statement reads, found without a full
//! parser: every name after `FROM`, `JOIN`, `UPDATE`, `MERGE INTO` and a
//! `MERGE`'s `USING`, and each further name in a comma-separated `FROM`.
//!
//! Quotes and comments are skipped (`'…'` and `"…"` are strings in Spark,
//! `` `…` `` an identifier), a `FROM` inside a function call
//! (`EXTRACT(day FROM ts)`) is not a table, and names a `WITH` defines are
//! CTEs. A name the lakehouse would read as a table must be qualified
//! (`database.table`, optionally after the warehouse); a bare name that is
//! no CTE is reported apart, since its size cannot be looked up.

/// A table as `database.table`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableRef {
    pub database: String,
    pub table: String,
}

impl std::fmt::Display for TableRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.database, self.table)
    }
}

/// What a statement reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reads {
    /// Each table once.
    pub tables: Vec<TableRef>,
    /// Bare names read as tables that no `WITH` defines.
    pub unqualified: Vec<String>,
}

/// Iceberg metadata tables, read as `db.table.files` and the like; they
/// read the table's metadata, not its data.
const METADATA_TABLES: &[&str] = &[
    "all_data_files",
    "all_delete_files",
    "all_entries",
    "all_files",
    "all_manifests",
    "data_files",
    "delete_files",
    "entries",
    "files",
    "history",
    "manifests",
    "metadata_log_entries",
    "partitions",
    "position_deletes",
    "refs",
    "snapshots",
];
/// Iceberg refs read as `db.table.branch_x`; they read the table's data.
const REF_PREFIXES: &[&str] = &["branch_", "tag_", "snapshot_id_", "at_timestamp_"];

/// Words before `(` that open a query, a list or a clause rather than a
/// function call, so a `FROM` inside still names a table.
const NOT_FUNCTIONS: &[&str] = &[
    "ALL",
    "AND",
    "ANY",
    "AS",
    "EXISTS",
    "FROM",
    "IN",
    "INTERSECT",
    "JOIN",
    "LATERAL",
    "MINUS",
    "NOT",
    "ON",
    "OR",
    "SELECT",
    "SOME",
    "TABLE",
    "UNION",
    "USING",
    "VALUES",
    "WHERE",
    "WITH",
    "EXCEPT",
    "INTO",
];
/// Words that end a `FROM` item's alias and options.
const CLAUSE_WORDS: &[&str] = &[
    "WHERE",
    "GROUP",
    "HAVING",
    "ORDER",
    "LIMIT",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "MINUS",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "ANTI",
    "SEMI",
    "ON",
    "USING",
    "WINDOW",
    "LATERAL",
    "PIVOT",
    "UNPIVOT",
    "CLUSTER",
    "DISTRIBUTE",
    "SORT",
    "QUALIFY",
    "SET",
    "WHEN",
    "SELECT",
    "INSERT",
    "OFFSET",
];

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// A name or keyword; `quoted` for a `` `name` ``.
    Word {
        text: String,
        quoted: bool,
    },
    Dot,
    Comma,
    Open,
    Close,
    /// A string, number or operator: never part of a name.
    Other,
}

impl Token {
    fn keyword(&self) -> Option<String> {
        match self {
            Token::Word {
                text,
                quoted: false,
            } => Some(text.to_ascii_uppercase()),
            _ => None,
        }
    }

    fn is_keyword(&self, word: &str) -> bool {
        self.keyword().as_deref() == Some(word)
    }
}

/// The tables `sql` reads. `warehouse` is the catalog a three-part name
/// may start with.
pub fn reads(sql: &str, warehouse: &str) -> Reads {
    let tokens = tokenize(sql);
    let ctes = cte_names(&tokens);
    let mut found = Reads::default();
    let mut parens: Vec<bool> = Vec::new();
    let mut is_merge = false;
    for (at, token) in tokens.iter().enumerate() {
        match token {
            Token::Open => parens.push(opens_function(&tokens, at)),
            Token::Close => {
                parens.pop();
            }
            _ => {}
        }
        let Some(word) = token.keyword() else {
            continue;
        };
        if word == "MERGE" {
            is_merge = true;
        }
        let inside_function = parens.last().copied().unwrap_or(false);
        let starts_list = match word.as_str() {
            "FROM" => !inside_function,
            "JOIN" | "UPDATE" => true,
            "INTO" => at > 0 && tokens[at - 1].is_keyword("MERGE"),
            "USING" => is_merge,
            _ => false,
        };
        if starts_list {
            read_items(
                &tokens,
                at + 1,
                word == "FROM",
                warehouse,
                &ctes,
                &mut found,
            );
        }
    }
    found
}

/// Whether the `(` at `at` opens a function call's arguments.
fn opens_function(tokens: &[Token], at: usize) -> bool {
    let Some(Token::Word { text, quoted }) = at.checked_sub(1).map(|i| &tokens[i]) else {
        return false;
    };
    *quoted || !NOT_FUNCTIONS.contains(&text.to_ascii_uppercase().as_str())
}

/// Reads the table named at `start`, and when `is_list`, each one after a
/// comma at the same depth.
fn read_items(
    tokens: &[Token],
    start: usize,
    is_list: bool,
    warehouse: &str,
    ctes: &[String],
    found: &mut Reads,
) {
    let mut at = start;
    loop {
        let (parts, next) = dotted_name(tokens, at);
        let is_call = matches!(tokens.get(next), Some(Token::Open));
        if !parts.is_empty() && !is_call {
            record(&parts, warehouse, ctes, found);
        }
        if !is_list {
            return;
        }
        match next_item(tokens, next) {
            Some(item) => at = item,
            None => return,
        }
    }
}

/// The position after the comma that starts the next item of a `FROM`
/// list, or `None` when the list ends first.
fn next_item(tokens: &[Token], from: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, token) in tokens.iter().enumerate().skip(from) {
        match token {
            Token::Open => depth += 1,
            Token::Close if depth == 0 => return None,
            Token::Close => depth -= 1,
            Token::Comma if depth == 0 => return Some(at + 1),
            Token::Word { .. } if depth == 0 => {
                let word = token.keyword().unwrap_or_default();
                if CLAUSE_WORDS.contains(&word.as_str()) {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

/// The parts of a dotted name starting at `at`, and the position after it.
fn dotted_name(tokens: &[Token], mut at: usize) -> (Vec<String>, usize) {
    let mut parts = Vec::new();
    while let Some(Token::Word { text, .. }) = tokens.get(at) {
        parts.push(text.clone());
        at += 1;
        if !matches!(tokens.get(at), Some(Token::Dot)) {
            break;
        }
        at += 1;
    }
    (parts, at)
}

fn record(parts: &[String], warehouse: &str, ctes: &[String], found: &mut Reads) {
    if let [name] = parts {
        // `UPDATE SET` in a MERGE's `WHEN MATCHED` names no table.
        if CLAUSE_WORDS.contains(&name.to_ascii_uppercase().as_str()) {
            return;
        }
        let is_cte = ctes.iter().any(|cte| cte.eq_ignore_ascii_case(name));
        if !is_cte && !found.unqualified.contains(name) {
            found.unqualified.push(name.clone());
        }
        return;
    }
    let Some(table) = base_table(parts, warehouse) else {
        return;
    };
    if !found.tables.contains(&table) {
        found.tables.push(table);
    }
}

/// The table a qualified name reads; `None` for a metadata table, which
/// reads no data.
fn base_table(parts: &[String], warehouse: &str) -> Option<TableRef> {
    let mut names: Vec<&str> = parts.iter().map(String::as_str).collect();
    if names.len() > 2 && names[0] == warehouse {
        names.remove(0);
    }
    let mut is_metadata = false;
    while names.len() > 2 {
        let last = names[names.len() - 1].to_ascii_lowercase();
        if METADATA_TABLES.contains(&last.as_str()) {
            is_metadata = true;
        } else if !REF_PREFIXES.iter().any(|p| last.starts_with(p)) {
            break;
        }
        names.pop();
    }
    if is_metadata {
        return None;
    }
    // Without a matching warehouse, a third leading part is taken for a
    // catalog, as the lakehouse does.
    let [.., database, table] = names[..] else {
        return None;
    };
    Some(TableRef {
        database: database.to_string(),
        table: table.to_string(),
    })
}

/// The names `WITH` clauses define.
fn cte_names(tokens: &[Token]) -> Vec<String> {
    let mut names = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        if !token.is_keyword("WITH") {
            continue;
        }
        let mut next = at + 1;
        if tokens.get(next).is_some_and(|t| t.is_keyword("RECURSIVE")) {
            next += 1;
        }
        while let Some(Token::Word { text, .. }) = tokens.get(next) {
            let mut after = next + 1;
            if matches!(tokens.get(after), Some(Token::Open)) {
                after = past_group(tokens, after);
            }
            if !tokens.get(after).is_some_and(|t| t.is_keyword("AS")) {
                break;
            }
            names.push(text.clone());
            after = past_group(tokens, after + 1);
            if !matches!(tokens.get(after), Some(Token::Comma)) {
                break;
            }
            next = after + 1;
        }
    }
    names
}

/// The position after the parenthesised group opening at `at` (or `at`
/// itself when no group opens there).
fn past_group(tokens: &[Token], at: usize) -> usize {
    if !matches!(tokens.get(at), Some(Token::Open)) {
        return at;
    }
    let mut depth = 0usize;
    for (position, token) in tokens.iter().enumerate().skip(at) {
        match token {
            Token::Open => depth += 1,
            Token::Close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return position + 1;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

fn tokenize(sql: &str) -> Vec<Token> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    while let Some(&ch) = chars.get(at) {
        let next = chars.get(at + 1).copied();
        at = match ch {
            _ if ch.is_whitespace() => at + 1,
            '-' if next == Some('-') => skip_until(&chars, at, "\n"),
            '/' if next == Some('*') => skip_until(&chars, at + 2, "*/"),
            '\'' | '"' => {
                tokens.push(Token::Other);
                past_quote(&chars, at, ch).0
            }
            '`' => {
                let (end, text) = past_quote(&chars, at, '`');
                tokens.push(Token::Word { text, quoted: true });
                end
            }
            '.' => {
                tokens.push(Token::Dot);
                at + 1
            }
            ',' => {
                tokens.push(Token::Comma);
                at + 1
            }
            '(' => {
                tokens.push(Token::Open);
                at + 1
            }
            ')' => {
                tokens.push(Token::Close);
                at + 1
            }
            _ if ch.is_alphabetic() || ch == '_' => {
                let end = word_end(&chars, at);
                tokens.push(Token::Word {
                    text: chars[at..end].iter().collect(),
                    quoted: false,
                });
                end
            }
            _ if ch.is_ascii_digit() => {
                tokens.push(Token::Other);
                // A number such as 1.5e3 is one token, so its dot is no name's.
                let mut end = at;
                while chars
                    .get(end)
                    .is_some_and(|c| c.is_alphanumeric() || *c == '.' || *c == '_')
                {
                    end += 1;
                }
                end
            }
            _ => {
                tokens.push(Token::Other);
                at + 1
            }
        };
    }
    tokens
}

fn word_end(chars: &[char], start: usize) -> usize {
    let mut end = start;
    while chars
        .get(end)
        .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
    {
        end += 1;
    }
    end
}

/// The position after `marker` (or the end), searching from `from`.
fn skip_until(chars: &[char], from: usize, marker: &str) -> usize {
    let marker: Vec<char> = marker.chars().collect();
    let mut at = from;
    while at < chars.len() {
        if chars[at..].starts_with(&marker) {
            return at + marker.len();
        }
        at += 1;
    }
    chars.len()
}

/// The position after the quoted text opening at `start`, and its text. A
/// doubled quote stays inside; in strings a backslash escapes the next
/// character, as in Spark.
fn past_quote(chars: &[char], start: usize, quote: char) -> (usize, String) {
    let mut text = String::new();
    let mut at = start + 1;
    while let Some(&ch) = chars.get(at) {
        if ch == '\\' && quote != '`' {
            if let Some(&escaped) = chars.get(at + 1) {
                text.push(escaped);
            }
            at += 2;
            continue;
        }
        if ch == quote {
            if chars.get(at + 1) == Some(&quote) {
                text.push(quote);
                at += 2;
                continue;
            }
            return (at + 1, text);
        }
        text.push(ch);
        at += 1;
    }
    (chars.len(), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str) -> Vec<String> {
        reads(sql, "wh")
            .tables
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_join_with_aliases_reads_both_tables() {
        let sql = "SELECT o.id FROM sales.orders AS o JOIN sales.customers c ON o.cid = c.id";
        assert_eq!(names(sql), ["sales.orders", "sales.customers"]);
    }

    #[test]
    fn ctes_are_not_tables_but_the_tables_inside_them_are() {
        let sql = "WITH recent (id) AS (SELECT id FROM sales.orders WHERE day > '2026-01-01'), \
                   big AS (SELECT * FROM recent JOIN ops.events e ON e.id = recent.id) \
                   SELECT * FROM big, recent";
        let found = reads(sql, "wh");
        assert_eq!(names(sql), ["sales.orders", "ops.events"]);
        assert!(found.unqualified.is_empty(), "{:?}", found.unqualified);
    }

    #[test]
    fn subqueries_and_comma_lists_are_read() {
        let sql = "SELECT * FROM a.x, (SELECT * FROM b.y WHERE k IN (SELECT k FROM c.z)) s, d.w \
                   WHERE EXISTS (SELECT 1 FROM e.v)";
        let mut found = names(sql);
        found.sort();
        assert_eq!(found, ["a.x", "b.y", "c.z", "d.w", "e.v"]);
    }

    #[test]
    fn quotes_comments_and_function_froms_hide_nothing_else() {
        let sql = "SELECT 'FROM fake.one', \"from fake.two\", EXTRACT(day FROM o.ts), \
                   TRIM(BOTH 'x' FROM o.name) -- FROM fake.three\n\
                   /* JOIN fake.four */ FROM `sales`.`order items` o";
        assert_eq!(names(sql), ["sales.order items"]);
    }

    #[test]
    fn the_warehouse_prefix_and_refs_are_the_base_table_but_metadata_tables_read_no_data() {
        let sql = "SELECT * FROM wh.sales.orders JOIN sales.orders.branch_dev d \
                   JOIN sales.items.files f JOIN other.sales.returns r";
        assert_eq!(names(sql), ["sales.orders", "sales.returns"]);
    }

    #[test]
    fn writes_are_not_reads_but_update_merge_and_delete_are() {
        assert_eq!(
            names("INSERT INTO sales.archive SELECT * FROM sales.orders"),
            ["sales.orders"]
        );
        let merge = "MERGE INTO sales.orders t USING staging.orders s ON t.id = s.id \
                     WHEN MATCHED THEN UPDATE SET * WHEN NOT MATCHED THEN INSERT *";
        assert_eq!(names(merge), ["sales.orders", "staging.orders"]);
        assert!(reads(merge, "wh").unqualified.is_empty());
        assert_eq!(
            names("UPDATE sales.orders SET x = 1 WHERE id IN (SELECT id FROM sales.bad)"),
            ["sales.orders", "sales.bad"]
        );
        assert_eq!(
            names("DELETE FROM sales.orders WHERE id = 1"),
            ["sales.orders"]
        );
    }

    #[test]
    fn a_bare_name_that_is_no_cte_is_reported_apart() {
        let found = reads("SELECT * FROM orders JOIN sales.items USING (id)", "wh");
        assert_eq!(found.unqualified, ["orders"]);
        assert_eq!(found.tables.len(), 1);
    }

    #[test]
    fn table_functions_and_time_travel_are_handled() {
        let sql = "SELECT * FROM range(10) r JOIN sales.orders VERSION AS OF 123 o ON o.id = r.id";
        let found = reads(sql, "wh");
        assert_eq!(names(sql), ["sales.orders"]);
        assert!(found.unqualified.is_empty());
    }
}

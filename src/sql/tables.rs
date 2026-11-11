//! The tables a SQL statement reads, found without running it or fully
//! parsing it: the names after `FROM`, `JOIN`, `UPDATE`, `MERGE INTO` and
//! a `MERGE`'s `USING`, and each further name in a `FROM` list. No
//! endpoint returns the tables of a query (the lakehouse's own extractor,
//! groundzero-iceberg-provider-api `utils/table_references.py`, runs only
//! inside its authorizer), so gz reads the text itself.
//!
//! Names are reported as the query writes them; what a name means (the
//! warehouse prefix, an Iceberg ref or metadata table) depends on where the
//! query runs, so each caller reads that from [`TableRef::base`] or
//! [`QueryTables::lakehouse_reads`]. An `INSERT` or `CREATE TABLE … AS`
//! target is written, not read, and is left out. What cannot be decided (a
//! table function, a placeholder where a table goes) is reported apart, not
//! guessed.

use super::tokens::{tokenize, Token};
use crate::table_history::TableName;

/// A table name as the query writes it, unquoted: its dotted parts
/// (`table`, `db.table`, `catalog.db.table`, maybe with a ref or metadata
/// table after).
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
    /// In the order first seen, without duplicates; no CTE names.
    pub tables: Vec<TableRef>,
    /// Sources that are no table name, as written (`read_parquet(...)`,
    /// `{{source.value}}`).
    pub unresolved: Vec<String>,
}

/// The table a name reads once the catalog and any Iceberg suffix are
/// taken off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseTable {
    pub table: TableRef,
    /// False for a metadata table (`db.t.files`), which reads `db.t`'s
    /// metadata and none of its data.
    pub reads_data: bool,
}

/// The lakehouse tables a query reads data from, for sizing a scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LakehouseReads {
    /// Each table once.
    pub tables: Vec<TableName>,
    /// Bare names, whose database (so whose size) is unknown.
    pub unqualified: Vec<String>,
}

/// Iceberg metadata tables, read as `db.table.files` and the like.
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

/// Words that end a table reference or start a clause, so they are never
/// taken as a table name.
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

/// Words that end a `FROM` list, so a comma after them separates columns
/// or values, not tables. `ON` and `USING` do not: a comma after a join
/// condition starts another table.
const LIST_ENDS: &[&str] = &[
    "WHERE",
    "GROUP",
    "HAVING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "MINUS",
    "WINDOW",
    "QUALIFY",
    "LATERAL",
    "PIVOT",
    "UNPIVOT",
    "CLUSTER",
    "DISTRIBUTE",
    "SORT",
    "SET",
    "WHEN",
];

/// Words before `(` that open a query, a list or a clause rather than a
/// function call, so a `FROM` inside still names a table.
const NOT_FUNCTIONS: &[&str] = &[
    "ALL",
    "AND",
    "ANY",
    "AS",
    "EXCEPT",
    "EXISTS",
    "FROM",
    "IN",
    "INTERSECT",
    "INTO",
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
];

impl TableRef {
    /// The table this name reads: without a leading catalog `is_catalog`
    /// accepts, then without Iceberg ref and metadata-table suffixes
    /// (`db.t.branch_dev` and `db.t.files` both read `db.t`). A third
    /// leading part that is no known catalog stays, as the lakehouse takes
    /// it for one.
    pub fn base(&self, is_catalog: impl Fn(&str) -> bool) -> BaseTable {
        let mut parts = self.parts.clone();
        if parts.len() > 2 && is_catalog(&parts[0]) {
            parts.remove(0);
        }
        let mut reads_data = true;
        while let [_, _, .., last] = parts.as_slice() {
            let last = last.to_ascii_lowercase();
            if METADATA_TABLES.contains(&last.as_str()) {
                reads_data = false;
            } else if !REF_PREFIXES.iter().any(|p| last.starts_with(p)) {
                break;
            }
            parts.pop();
        }
        BaseTable {
            table: TableRef { parts },
            reads_data,
        }
    }
}

impl QueryTables {
    /// The tables whose data the query reads on `warehouse`'s lakehouse,
    /// as `database.table`. Unresolved sources are left out: a table
    /// function or placeholder has no size gz can look up.
    pub fn lakehouse_reads(&self, warehouse: &str) -> LakehouseReads {
        let mut reads = LakehouseReads::default();
        for name in &self.tables {
            if let [bare] = name.parts.as_slice() {
                push_once(&mut reads.unqualified, bare.clone());
                continue;
            }
            let base = name.base(|catalog| catalog == warehouse);
            if !base.reads_data {
                continue;
            }
            if let [.., database, table] = base.table.parts.as_slice() {
                let table = TableName {
                    database: database.clone(),
                    table: table.clone(),
                };
                push_once(&mut reads.tables, table);
            }
        }
        reads
    }
}

fn push_once<T: PartialEq>(list: &mut Vec<T>, item: T) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// The tables `sql` reads (one statement or several).
pub fn tables_read(sql: &str) -> QueryTables {
    let tokens = tokenize(sql);
    let ctes = cte_names(&tokens);
    let mut walker = Walker {
        tokens,
        at: 0,
        levels: vec![Level::default()],
        is_merge: false,
        ctes,
        found: QueryTables::default(),
    };
    walker.walk();
    walker.found
}

/// A name in the position of a table, CTE or alias: an unquoted word that
/// is no keyword, or a quoted name.
fn name_of(token: &Token) -> Option<&str> {
    match token {
        Token::Word(word) if !RESERVED.contains(&word.to_ascii_uppercase().as_str()) => Some(word),
        Token::Quoted(name) => Some(name),
        _ => None,
    }
}

/// The names `WITH` clauses define, lower-cased: `name [(columns)] AS
/// (...)`, and each one after a comma.
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
        while let Some(name) = tokens.get(next).and_then(name_of) {
            let after = past_group(tokens, next + 1);
            if !tokens.get(after).is_some_and(|t| t.is_keyword("AS")) {
                break;
            }
            names.push(name.to_ascii_lowercase());
            let after = past_group(tokens, after + 1);
            if tokens.get(after) != Some(&Token::Punct(',')) {
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
    if tokens.get(at) != Some(&Token::Punct('(')) {
        return at;
    }
    let mut depth = 0usize;
    for (position, token) in tokens.iter().enumerate().skip(at) {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => {
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

/// Whether the `(` at `open` starts a function call's arguments.
fn opens_function(tokens: &[Token], open: usize) -> bool {
    match open.checked_sub(1).and_then(|i| tokens.get(i)) {
        Some(Token::Word(word)) => !NOT_FUNCTIONS.contains(&word.to_ascii_uppercase().as_str()),
        Some(Token::Quoted(_)) => true,
        _ => false,
    }
}

/// One level of parentheses.
#[derive(Default)]
struct Level {
    /// The arguments of a function call, where `FROM` is part of the call
    /// (`EXTRACT(day FROM ts)`) unless a `SELECT` started inside.
    is_function: bool,
    has_select: bool,
    /// Inside a `FROM` list, where a comma starts another table.
    in_from: bool,
}

struct Walker {
    tokens: Vec<Token>,
    at: usize,
    levels: Vec<Level>,
    /// The statement is a `MERGE`, whose `USING` names its source.
    is_merge: bool,
    ctes: Vec<String>,
    found: QueryTables,
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

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    /// The keyword `back` tokens before the one just taken.
    fn is_keyword_before(&self, back: usize, word: &str) -> bool {
        self.at
            .checked_sub(back + 1)
            .and_then(|i| self.tokens.get(i))
            .is_some_and(|t| t.is_keyword(word))
    }

    fn walk(&mut self) {
        while let Some(token) = self.tokens.get(self.at).cloned() {
            self.at += 1;
            match token {
                Token::Punct('(') => {
                    let is_function = opens_function(&self.tokens, self.at - 1);
                    self.levels.push(Level {
                        is_function,
                        ..Level::default()
                    });
                }
                Token::Punct(')') => {
                    if self.levels.len() > 1 {
                        self.levels.pop();
                    }
                }
                Token::Punct(',') if self.level().in_from => self.read_source(),
                Token::Punct(';') => {
                    self.levels = vec![Level::default()];
                    self.is_merge = false;
                }
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
            "MERGE" => self.is_merge = true,
            "UPDATE" => self.read_source(),
            "INTO" if self.is_keyword_before(1, "MERGE") => self.read_source(),
            "USING" if self.is_merge => self.read_source(),
            _ if LIST_ENDS.contains(&keyword) => self.level().in_from = false,
            _ => {}
        }
    }

    /// A `FROM` that starts a clause: not one inside a function call's
    /// arguments, and not `IS [NOT] DISTINCT FROM`.
    fn is_clause_from(&mut self) -> bool {
        let after_distinct = self.is_keyword_before(1, "DISTINCT");
        let level = self.level();
        !after_distinct && (level.has_select || !level.is_function)
    }

    /// Reads the source after `FROM`, `JOIN`, a comma in a `FROM` list,
    /// `UPDATE`, `MERGE INTO` or a `MERGE`'s `USING`.
    fn read_source(&mut self) {
        while self.peek().is_some_and(|t| t.is_keyword("LATERAL")) {
            self.at += 1;
        }
        match self.peek().cloned() {
            // A subquery: the walk goes on inside it.
            Some(Token::Punct('(')) => {}
            Some(Token::Placeholder(text)) => {
                self.found.unresolved.push(text);
                self.at += 1;
            }
            Some(token) if name_of(&token).is_some() => self.read_name(),
            _ => {}
        }
    }

    fn read_name(&mut self) {
        let mut parts = Vec::new();
        // Only the first part can be mistaken for a keyword; after a dot
        // any word is a name (`db.version`).
        while let Some(part) = self.peek().and_then(|token| match token {
            Token::Word(word) if !parts.is_empty() => Some(word.clone()),
            _ => name_of(token).map(str::to_string),
        }) {
            parts.push(part);
            self.at += 1;
            if self.peek() != Some(&Token::Punct('.')) {
                break;
            }
            self.at += 1;
        }
        // A table function; the walk goes on into its arguments, which may
        // hold a subquery.
        if self.peek() == Some(&Token::Punct('(')) {
            self.found
                .unresolved
                .push(format!("{}(...)", parts.join(".")));
            return;
        }
        let is_cte =
            matches!(parts.as_slice(), [name] if self.ctes.contains(&name.to_ascii_lowercase()));
        if parts.is_empty() || is_cte {
            return;
        }
        push_once(&mut self.found.tables, TableRef { parts });
    }
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

    /// The `database.table`s whose data a query reads on warehouse `wh`.
    fn lakehouse_names(sql: &str) -> Vec<String> {
        lakehouse_reads(sql)
            .tables
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn lakehouse_reads(sql: &str) -> LakehouseReads {
        tables_read(sql).lakehouse_reads("wh")
    }

    // ---------- every name read ----------

    #[test]
    fn reads_a_plain_table_and_its_alias_is_not_a_table() {
        assert_eq!(
            names("SELECT o.id FROM sales.orders o WHERE o.amount > 0"),
            ["sales.orders"]
        );
        assert_eq!(names("select * from orders as o"), ["orders"]);
    }

    #[test]
    fn a_join_with_aliases_reads_both_tables() {
        let sql = "SELECT o.id FROM sales.orders AS o JOIN sales.customers c ON o.cid = c.id";
        assert_eq!(lakehouse_names(sql), ["sales.orders", "sales.customers"]);
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
    fn ctes_are_not_tables_but_the_tables_inside_them_are() {
        let sql = "WITH recent (id) AS (SELECT id FROM sales.orders WHERE day > '2026-01-01'), \
                   big AS (SELECT * FROM recent JOIN ops.events e ON e.id = recent.id) \
                   SELECT * FROM big, recent";
        let found = lakehouse_reads(sql);
        assert_eq!(lakehouse_names(sql), ["sales.orders", "ops.events"]);
        assert!(found.unqualified.is_empty(), "{:?}", found.unqualified);
    }

    #[test]
    fn reads_subqueries_in_from_and_where() {
        let sql = "SELECT * FROM (SELECT * FROM a.x) t \
                   WHERE id IN (SELECT id FROM a.y) AND EXISTS (SELECT 1 FROM a.z)";
        assert_eq!(names(sql), ["a.x", "a.y", "a.z"]);
    }

    #[test]
    fn subqueries_and_comma_lists_are_read() {
        let sql = "SELECT * FROM a.x, (SELECT * FROM b.y WHERE k IN (SELECT k FROM c.z)) s, d.w \
                   WHERE EXISTS (SELECT 1 FROM e.v)";
        let mut found = lakehouse_names(sql);
        found.sort();
        assert_eq!(found, ["a.x", "b.y", "c.z", "d.w", "e.v"]);
    }

    #[test]
    fn from_inside_a_function_or_a_comparison_is_not_a_table() {
        let sql = "SELECT EXTRACT(YEAR FROM order_date) AS y, \
                   SUBSTRING(name FROM 1 FOR 3), TRIM(BOTH ' ' FROM code) \
                   FROM sales.orders WHERE a IS DISTINCT FROM b AND c IS NOT DISTINCT FROM d";
        assert_eq!(names(sql), ["sales.orders"]);
    }

    #[test]
    fn a_query_inside_a_function_call_is_still_read() {
        // Of the two extractors only the one gz explain used saw this.
        assert_eq!(
            names("SELECT ARRAY(SELECT id FROM sales.bad) FROM sales.orders"),
            ["sales.bad", "sales.orders"]
        );
    }

    #[test]
    fn quotes_comments_and_function_froms_hide_nothing_else() {
        let sql = "SELECT 'FROM fake.one', \"from fake.two\", EXTRACT(day FROM o.ts), \
                   TRIM(BOTH 'x' FROM o.name) -- FROM fake.three\n\
                   /* JOIN fake.four */ FROM `sales`.`order items` o";
        assert_eq!(lakehouse_names(sql), ["sales.order items"]);
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
    fn an_escaped_quote_does_not_end_a_string() {
        // Spark escapes with a backslash; the explain extractor did not,
        // and took `FROM fake.one` here for a table.
        let sql = r#"SELECT 'it\'s FROM fake.one', "say \"FROM fake.two\"" FROM sales.orders"#;
        assert_eq!(names(sql), ["sales.orders"]);
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
    fn a_comma_after_lateral_view_or_cluster_by_starts_no_table() {
        // The explain extractor read `b` and `d` here as tables.
        let sql = "SELECT * FROM s.t LATERAL VIEW explode(arr) e AS a, b CLUSTER BY c, d";
        let found = tables_read(sql);
        assert_eq!(names(sql), ["s.t"]);
        assert_eq!(found.unresolved, Vec::<String>::new());
    }

    #[test]
    fn a_name_after_a_dot_may_be_a_keyword() {
        assert_eq!(names("SELECT * FROM logs.version v"), ["logs.version"]);
    }

    // ---------- reads against writes ----------

    #[test]
    fn writes_are_not_reads_but_update_merge_and_delete_are() {
        assert_eq!(
            lakehouse_names("INSERT INTO sales.archive SELECT * FROM sales.orders"),
            ["sales.orders"]
        );
        assert_eq!(
            lakehouse_names("CREATE TABLE sales.archive AS SELECT * FROM sales.orders"),
            ["sales.orders"]
        );
        let merge = "MERGE INTO sales.orders t USING staging.orders s ON t.id = s.id \
                     WHEN MATCHED THEN UPDATE SET * WHEN NOT MATCHED THEN INSERT *";
        assert_eq!(lakehouse_names(merge), ["sales.orders", "staging.orders"]);
        assert!(lakehouse_reads(merge).unqualified.is_empty());
        assert_eq!(
            lakehouse_names("UPDATE sales.orders SET x = 1 WHERE id IN (SELECT id FROM sales.bad)"),
            ["sales.orders", "sales.bad"]
        );
        assert_eq!(
            lakehouse_names("DELETE FROM sales.orders WHERE id = 1"),
            ["sales.orders"]
        );
    }

    #[test]
    fn a_from_without_a_select_still_names_a_table() {
        // Only the scan estimate's extractor saw these: the explain one
        // took a `FROM` for a clause only after a `SELECT`.
        assert_eq!(names("DELETE FROM sales.orders"), ["sales.orders"]);
        assert_eq!(names("FROM sales.orders SELECT id"), ["sales.orders"]);
    }

    // ---------- the lakehouse view ----------

    #[test]
    fn the_warehouse_prefix_and_refs_are_the_base_table_but_metadata_tables_read_no_data() {
        let sql = "SELECT * FROM wh.sales.orders JOIN sales.orders.branch_dev d \
                   JOIN sales.items.files f JOIN other.sales.returns r";
        assert_eq!(lakehouse_names(sql), ["sales.orders", "sales.returns"]);
    }

    #[test]
    fn double_quoted_names_are_sized_too() {
        // The scan estimate's extractor read `"…"` as a string, as Spark
        // does outside a name, and missed these tables.
        assert_eq!(
            lakehouse_names(r#"SELECT * FROM "sales"."orders""#),
            ["sales.orders"]
        );
    }

    #[test]
    fn a_bare_name_that_is_no_cte_is_reported_apart() {
        let found = lakehouse_reads("SELECT * FROM orders JOIN sales.items USING (id)");
        assert_eq!(found.unqualified, ["orders"]);
        assert_eq!(found.tables.len(), 1);
    }

    #[test]
    fn table_functions_and_time_travel_are_handled() {
        let sql = "SELECT * FROM range(10) r JOIN sales.orders VERSION AS OF 123 o ON o.id = r.id";
        let found = lakehouse_reads(sql);
        assert_eq!(lakehouse_names(sql), ["sales.orders"]);
        assert!(found.unqualified.is_empty());
        assert_eq!(tables_read(sql).unresolved, ["range(...)"]);
    }

    #[test]
    fn the_base_table_drops_a_known_catalog_and_iceberg_suffixes() {
        let base = |text: &str| {
            let name = TableRef {
                parts: text.split('.').map(String::from).collect(),
            };
            let base = name.base(|catalog| catalog == "wh");
            (base.table.to_string(), base.reads_data)
        };
        assert_eq!(base("wh.db.t"), ("db.t".to_string(), true));
        assert_eq!(base("wh.db.t.tag_v1"), ("db.t".to_string(), true));
        assert_eq!(base("db.t.snapshots"), ("db.t".to_string(), false));
        assert_eq!(base("other.db.t"), ("other.db.t".to_string(), true));
        assert_eq!(base("t"), ("t".to_string(), true));
    }
}

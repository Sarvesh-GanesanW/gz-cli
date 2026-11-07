//! `\` meta commands of the SQL shell, psql-style.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Meta {
    /// `\d`: databases (namespaces) in the warehouse.
    Databases,
    /// `\dt [db]`: tables, in `db` or the current database.
    Tables(Option<String>),
    /// `\d table`: columns of a table.
    Describe(String),
    /// `\use db`: explains how to address another database (the lakehouse
    /// provider refuses `USE` statements).
    Use(String),
    /// `\x`: toggle one-column-per-line rows.
    Expanded,
    /// `\o file.csv`: write the current (or next) result to a CSV file.
    Output(PathBuf),
    /// `\timing`: toggle the elapsed time under results.
    Timing,
    /// `\more`: next page of the last result.
    More,
    Quit,
    Help,
}

/// The SQL a lookup meta command runs on the session.
impl Meta {
    pub fn sql(&self) -> Option<String> {
        match self {
            Meta::Databases => Some("SHOW DATABASES".to_string()),
            Meta::Tables(Some(db)) => Some(format!("SHOW TABLES IN {db}")),
            Meta::Tables(None) => Some("SHOW TABLES".to_string()),
            Meta::Describe(table) => Some(format!("DESCRIBE TABLE {table}")),
            _ => None,
        }
    }
}

pub const HELP: &[&str] = &[
    "Statements end with ;  ·  enter on an unfinished statement adds a line",
    "\\d (\\l)         databases          \\dt [db]    tables",
    "\\d <table>      describe a table   \\use <db>   how to query a db",
    "\\x              expanded rows      \\timing     toggle elapsed time",
    "\\o <file.csv>   save result as CSV \\more       next page (or space)",
    "\\q              quit (stops the session)  \\help  this help",
    "esc / ctrl+c cancels a running query  ·  ctrl+d quits",
];

pub fn parse(input: &str) -> Result<Meta, String> {
    let trimmed = input.trim().trim_end_matches(';');
    let mut words = trimmed.split_whitespace();
    let command = words.next().unwrap_or_default();
    let argument = words.next().map(str::to_string);
    if words.next().is_some() {
        return Err(format!("{command} takes at most one argument"));
    }
    let need = |what: &str| {
        argument
            .clone()
            .ok_or_else(|| format!("{command} needs {what}"))
    };
    match command {
        "\\d" => Ok(argument.map(Meta::Describe).unwrap_or(Meta::Databases)),
        "\\l" | "\\dn" => Ok(Meta::Databases),
        "\\dt" => Ok(Meta::Tables(argument)),
        "\\use" | "\\c" => need("a database name").map(Meta::Use),
        "\\x" => Ok(Meta::Expanded),
        "\\o" => need("a file name, e.g. \\o result.csv").map(|f| Meta::Output(PathBuf::from(f))),
        "\\timing" => Ok(Meta::Timing),
        "\\more" | "\\n" => Ok(Meta::More),
        "\\q" | "\\quit" | "\\exit" => Ok(Meta::Quit),
        "\\help" | "\\h" | "\\?" => Ok(Meta::Help),
        other => Err(format!("Unknown command {other} · \\help lists them")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lookups() {
        assert_eq!(parse("\\d"), Ok(Meta::Databases));
        assert_eq!(parse("\\d orders"), Ok(Meta::Describe("orders".into())));
        assert_eq!(parse("\\dt"), Ok(Meta::Tables(None)));
        assert_eq!(parse("\\dt sales;"), Ok(Meta::Tables(Some("sales".into()))));
        assert_eq!(parse("\\use sales"), Ok(Meta::Use("sales".into())));
    }

    #[test]
    fn parses_toggles_and_output() {
        assert_eq!(parse("\\x"), Ok(Meta::Expanded));
        assert_eq!(parse("\\timing"), Ok(Meta::Timing));
        assert_eq!(parse("\\o out.csv"), Ok(Meta::Output("out.csv".into())));
        assert_eq!(parse("\\q"), Ok(Meta::Quit));
    }

    #[test]
    fn rejects_unknown_and_missing_arguments() {
        assert!(parse("\\zz").unwrap_err().contains("Unknown command"));
        assert!(parse("\\use").unwrap_err().contains("needs a database"));
        assert!(parse("\\o").unwrap_err().contains("needs a file"));
        assert!(parse("\\d a b").unwrap_err().contains("at most one"));
    }

    #[test]
    fn lookups_map_to_spark_sql() {
        assert_eq!(Meta::Databases.sql().unwrap(), "SHOW DATABASES");
        assert_eq!(
            Meta::Tables(Some("sales".into())).sql().unwrap(),
            "SHOW TABLES IN sales"
        );
        assert_eq!(
            Meta::Describe("sales.orders".into()).sql().unwrap(),
            "DESCRIBE TABLE sales.orders"
        );
        assert!(Meta::Expanded.sql().is_none());
        assert!(Meta::Use("sales".into()).sql().is_none());
    }
}

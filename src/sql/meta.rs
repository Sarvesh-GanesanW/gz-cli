//! `\` meta commands of the SQL shell, psql-style.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Meta {
    /// `\dn` (`\l`, bare `\d`): databases (namespaces) in the warehouse.
    Databases,
    /// `\dt [db]`: tables, in `db` or in every database.
    Tables(Option<String>),
    /// `\d table`: columns, types, partitioning and properties.
    Describe(String),
    /// `\use db`: explains how to address another database (the lakehouse
    /// provider refuses `USE` statements).
    Use(String),
    /// `\x`: toggle one-column-per-line rows.
    Expanded,
    /// `\o file`: save every following result to `file`; `\o off` (or a
    /// bare `\o`) stops.
    Output(Option<PathBuf>),
    /// `\save file`: save the last result, every page of it.
    Save(PathBuf),
    /// `\timing`: toggle the elapsed time under results.
    Timing,
    /// `\more`: next page of the last result.
    More,
    /// `\refresh`: read the warehouse's names again for completion.
    Refresh,
    /// `\restart`: start a new session after the old one stopped.
    Restart,
    Quit,
    Help,
}

/// Every command, for completion.
pub const COMMANDS: &[&str] = &[
    "\\d",
    "\\dn",
    "\\dt",
    "\\l",
    "\\use",
    "\\x",
    "\\timing",
    "\\o",
    "\\save",
    "\\more",
    "\\refresh",
    "\\restart",
    "\\q",
    "\\help",
    "\\h",
    "\\?",
];

impl Meta {
    /// The SQL a lookup command runs on the session.
    pub fn sql(&self) -> Option<String> {
        match self {
            Meta::Databases => Some("SHOW DATABASES".to_string()),
            Meta::Tables(Some(db)) => Some(format!("SHOW TABLES IN {db}")),
            // Spark's DESCRIBE EXTENDED adds the partitioning and the
            // table's properties to the columns.
            Meta::Describe(table) => Some(format!("DESCRIBE TABLE EXTENDED {table}")),
            _ => None,
        }
    }
}

pub const HELP: &[&str] = &[
    "Statements end with ;  ·  enter on an unfinished statement adds a line  ·  tab completes",
    "\\dn (\\l, \\d)       databases            \\dt [db]        tables (all, or in db)",
    "\\d <table>        columns, partitioning and properties of a table",
    "\\x                expanded rows        \\timing         toggle elapsed time",
    "\\o <file>         save the next results to a .csv, .jsonl or .parquet file",
    "\\o off            stop saving          \\save <file>    save the last result",
    "\\more             next page (or space) \\refresh        reload names for tab completion",
    "\\use <db>         how to query a db    \\restart        start a new session",
    "\\q                quit (stops the session)            \\help (\\h, \\?)  this help",
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
        "\\o" => Ok(Meta::Output(
            argument
                .filter(|a| !a.eq_ignore_ascii_case("off"))
                .map(PathBuf::from),
        )),
        "\\save" => need("a file name, e.g. \\save result.csv").map(|f| Meta::Save(f.into())),
        "\\timing" => Ok(Meta::Timing),
        "\\more" | "\\n" => Ok(Meta::More),
        "\\refresh" => Ok(Meta::Refresh),
        "\\restart" => Ok(Meta::Restart),
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
        assert_eq!(parse("\\dn"), Ok(Meta::Databases));
        assert_eq!(parse("\\d orders"), Ok(Meta::Describe("orders".into())));
        assert_eq!(parse("\\dt"), Ok(Meta::Tables(None)));
        assert_eq!(parse("\\dt sales;"), Ok(Meta::Tables(Some("sales".into()))));
        assert_eq!(parse("\\use sales"), Ok(Meta::Use("sales".into())));
        assert_eq!(parse("\\refresh"), Ok(Meta::Refresh));
        assert_eq!(parse("\\restart"), Ok(Meta::Restart));
    }

    #[test]
    fn parses_toggles_and_output() {
        assert_eq!(parse("\\x"), Ok(Meta::Expanded));
        assert_eq!(parse("\\timing"), Ok(Meta::Timing));
        assert_eq!(
            parse("\\o out.parquet"),
            Ok(Meta::Output(Some("out.parquet".into())))
        );
        assert_eq!(parse("\\o off"), Ok(Meta::Output(None)));
        assert_eq!(parse("\\o"), Ok(Meta::Output(None)));
        assert_eq!(parse("\\save a.jsonl"), Ok(Meta::Save("a.jsonl".into())));
        assert_eq!(parse("\\h"), Ok(Meta::Help));
        assert_eq!(parse("\\q"), Ok(Meta::Quit));
    }

    #[test]
    fn rejects_unknown_and_missing_arguments() {
        assert!(parse("\\zz").unwrap_err().contains("Unknown command"));
        assert!(parse("\\use").unwrap_err().contains("needs a database"));
        assert!(parse("\\save").unwrap_err().contains("needs a file"));
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
            "DESCRIBE TABLE EXTENDED sales.orders"
        );
        assert!(
            Meta::Tables(None).sql().is_none(),
            "all tables come from the catalog"
        );
        assert!(Meta::Expanded.sql().is_none());
        assert!(Meta::Use("sales".into()).sql().is_none());
    }

    #[test]
    fn help_names_every_command() {
        let help = HELP.join("\n");
        for command in COMMANDS {
            assert!(help.contains(command), "{command} is missing from \\help");
        }
    }
}

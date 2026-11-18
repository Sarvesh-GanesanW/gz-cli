//! Saving query results to local files: `\save` (the last result) and
//! `\o` (every following result). The format comes from the file's
//! extension: `.csv`, `.jsonl` (one JSON object per row) or `.parquet`.
//! Rows are written page by page as they arrive, so a large result never
//! sits in memory whole.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::cells::Temporal;
use super::parquet_out::ParquetSink;
use crate::output;

/// Most rows one save writes; past it the save stops and says so.
pub const ROW_CAP: usize = 1_000_000;
/// Rows asked for per page while saving (the shell shows 100 at a time).
pub const PAGE_SIZE: u32 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Csv,
    Jsonl,
    Parquet,
}

impl FileFormat {
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("csv") => Ok(Self::Csv),
            Some("jsonl") | Some("ndjson") => Ok(Self::Jsonl),
            Some("parquet") => Ok(Self::Parquet),
            _ => Err(format!(
                "{} needs a .csv, .jsonl or .parquet extension",
                path.display()
            )),
        }
    }

    /// Whether a later result with the same columns can go on the end.
    fn appends(self) -> bool {
        !matches!(self, Self::Parquet)
    }
}

/// Where one result goes: a file, created afresh or appended to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub path: PathBuf,
    pub format: FileFormat,
    pub append: bool,
}

/// `\o`'s target: the next results' files. CSV and JSON Lines results
/// with the same columns share one file; a CSV result with other columns,
/// and every Parquet result, gets its own numbered file
/// (`out.csv`, `out-2.csv`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTarget {
    pub path: PathBuf,
    format: FileFormat,
    files: Vec<(PathBuf, Vec<String>)>,
}

impl OutputTarget {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        let format = FileFormat::from_path(&path)?;
        Ok(Self {
            path,
            format,
            files: Vec::new(),
        })
    }

    pub fn destination(&mut self, columns: &[String]) -> Destination {
        if let Some((path, previous)) = self.files.last() {
            let same_shape = self.format == FileFormat::Jsonl || previous == columns;
            if self.format.appends() && same_shape {
                return Destination {
                    path: path.clone(),
                    format: self.format,
                    append: true,
                };
            }
        }
        let path = numbered(&self.path, self.files.len() + 1);
        self.files.push((path.clone(), columns.to_vec()));
        Destination {
            path,
            format: self.format,
            append: false,
        }
    }
}

/// `out.csv` for the first file, `out-2.csv` for the second, ….
fn numbered(path: &Path, number: usize) -> PathBuf {
    if number == 1 {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}-{number}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{number}"),
    };
    path.with_file_name(name)
}

/// A path as typed, with a leading `~/` meaning the home folder.
pub fn expand_home(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => path.to_path_buf(),
    }
}

/// What a save wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: PathBuf,
    pub rows: usize,
    /// The result had more rows than [`ROW_CAP`].
    pub capped: bool,
    /// Rows the result has that the session could not page out.
    pub missing: Option<u64>,
    /// Columns written other than as their type (Parquet).
    pub notes: Vec<String>,
}

impl Saved {
    pub fn message(&self) -> String {
        let plural = if self.rows == 1 { "" } else { "s" };
        let mut text = format!("Saved {} row{plural} to {}", self.rows, self.path.display());
        if self.capped {
            text.push_str(&format!(
                ". Stopped at the {ROW_CAP}-row cap; the result has more rows \
                 (narrow the query, or `gz sql export` for the full result)"
            ));
        }
        if let Some(missing) = self.missing {
            text.push_str(&format!(
                ". The session paged out only part of the result: {missing} more row(s) \
                 are not in the file (`gz sql export` writes the full result)"
            ));
        }
        for note in &self.notes {
            text.push_str(&format!(". {note}"));
        }
        text
    }
}

impl Layout {
    /// `row` with its date and timestamp cells in ISO 8601.
    fn iso_dates(&self, row: &Value) -> Value {
        let mut row = row.clone();
        for (column, temporal) in self.columns.iter().zip(&self.temporal) {
            let (Some(temporal), Some(cell)) = (temporal, row.get_mut(column)) else {
                continue;
            };
            if let Some(iso) = cell.as_str().and_then(|text| temporal.iso(text)) {
                *cell = Value::String(iso);
            }
        }
        row
    }

    /// One JSON object with its keys in the result's column order (a
    /// serde_json map would sort them).
    fn json_line(&self, row: &Value) -> Result<String> {
        let mut line = String::from("{");
        for (at, column) in self.columns.iter().enumerate() {
            if at > 0 {
                line.push(',');
            }
            line.push_str(&serde_json::to_string(column)?);
            line.push(':');
            line.push_str(&serde_json::to_string(
                row.get(column).unwrap_or(&Value::Null),
            )?);
        }
        line.push_str("}\n");
        Ok(line)
    }
}

/// One result being written.
pub struct ResultFile {
    layout: Layout,
    sink: Sink,
}

/// The result's columns, and which hold dates or timestamps to rewrite
/// as ISO 8601 in text files.
struct Layout {
    columns: Vec<String>,
    temporal: Vec<Option<Temporal>>,
}

enum Sink {
    Csv(BufWriter<File>),
    Jsonl(BufWriter<File>),
    Parquet(Box<ParquetSink>),
}

impl ResultFile {
    /// Opens `to` for a result with these columns (types as the server
    /// names them, maybe empty); a new CSV file gets the header row.
    pub fn create(to: &Destination, columns: &[String], types: &[String]) -> Result<Self> {
        if let Some(dir) = to.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if !dir.is_dir() {
                bail!("no folder {}", dir.display());
            }
        }
        let open = || -> Result<BufWriter<File>> {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .append(to.append)
                .truncate(!to.append)
                .open(&to.path)
                .with_context(|| format!("cannot write {}", to.path.display()))?;
            Ok(BufWriter::new(file))
        };
        let sink = match to.format {
            FileFormat::Csv => {
                let mut file = open()?;
                if !to.append {
                    file.write_all(output::csv_text(columns, &[]).as_bytes())?;
                }
                Sink::Csv(file)
            }
            FileFormat::Jsonl => Sink::Jsonl(open()?),
            FileFormat::Parquet => {
                let file = File::create(&to.path)
                    .with_context(|| format!("cannot write {}", to.path.display()))?;
                Sink::Parquet(Box::new(ParquetSink::new(file, columns, types)))
            }
        };
        let layout = Layout {
            columns: columns.to_vec(),
            temporal: (0..columns.len())
                .map(|at| types.get(at).and_then(|t| Temporal::of(t)))
                .collect(),
        };
        Ok(Self { layout, sink })
    }

    pub fn write_rows(&mut self, rows: &[Value]) -> Result<()> {
        let layout = &self.layout;
        match &mut self.sink {
            Sink::Csv(file) => {
                let rows: Vec<Value> = rows.iter().map(|row| layout.iso_dates(row)).collect();
                let text = output::csv_text(&layout.columns, &rows);
                // csv_text always starts with the header; it is written once.
                let body = text.split_once('\n').map_or("", |(_, rest)| rest);
                file.write_all(body.as_bytes())?;
            }
            Sink::Jsonl(file) => {
                for row in rows {
                    let line = layout.json_line(&layout.iso_dates(row))?;
                    file.write_all(line.as_bytes())?;
                }
            }
            Sink::Parquet(sink) => sink.write(rows)?,
        }
        Ok(())
    }

    /// Flushes the file; returns notes about how columns were written.
    pub fn finish(self) -> Result<Vec<String>> {
        match self.sink {
            Sink::Csv(mut file) | Sink::Jsonl(mut file) => {
                file.flush()?;
                Ok(Vec::new())
            }
            Sink::Parquet(sink) => sink.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gz-sqlsave-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn columns(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn the_extension_picks_the_format() {
        assert_eq!(
            FileFormat::from_path(Path::new("a.CSV")),
            Ok(FileFormat::Csv)
        );
        assert_eq!(
            FileFormat::from_path(Path::new("a.jsonl")),
            Ok(FileFormat::Jsonl)
        );
        assert_eq!(
            FileFormat::from_path(Path::new("a.parquet")),
            Ok(FileFormat::Parquet)
        );
        assert!(FileFormat::from_path(Path::new("a.txt"))
            .unwrap_err()
            .contains(".parquet"));
    }

    #[test]
    fn output_target_appends_same_shaped_results_and_numbers_the_rest() {
        let mut csv = OutputTarget::new("out/r.csv".into()).unwrap();
        let first = csv.destination(&columns(&["a"]));
        assert_eq!(
            (first.path.clone(), first.append),
            ("out/r.csv".into(), false)
        );
        assert!(csv.destination(&columns(&["a"])).append);
        let other = csv.destination(&columns(&["b"]));
        assert_eq!((other.path, other.append), ("out/r-2.csv".into(), false));

        let mut jsonl = OutputTarget::new("r.jsonl".into()).unwrap();
        jsonl.destination(&columns(&["a"]));
        assert!(
            jsonl.destination(&columns(&["b"])).append,
            "rows name their own keys"
        );

        let mut parquet = OutputTarget::new("r.parquet".into()).unwrap();
        parquet.destination(&columns(&["a"]));
        assert_eq!(
            parquet.destination(&columns(&["a"])).path,
            PathBuf::from("r-2.parquet")
        );
    }

    #[test]
    fn csv_and_jsonl_write_page_by_page_in_column_order() {
        let dir = temp_dir("text");
        let rows = [
            json!({"b": "x,y", "a": 1, "d": "Sun, 01 Jan 2023 00:00:00 GMT"}),
            json!({"a": null, "b": "z", "d": null}),
        ];
        let names = columns(&["b", "a", "d"]);
        let types = columns(&["string", "integer", "date"]);
        for (name, format) in [("r.csv", FileFormat::Csv), ("r.jsonl", FileFormat::Jsonl)] {
            let to = Destination {
                path: dir.join(name),
                format,
                append: false,
            };
            let mut file = ResultFile::create(&to, &names, &types).unwrap();
            file.write_rows(&rows[..1]).unwrap();
            file.write_rows(&rows[1..]).unwrap();
            file.finish().unwrap();
        }
        let csv = std::fs::read_to_string(dir.join("r.csv")).unwrap();
        assert_eq!(csv, "b,a,d\n\"x,y\",1,2023-01-01\nz,,\n");
        let jsonl = std::fs::read_to_string(dir.join("r.jsonl")).unwrap();
        assert_eq!(
            jsonl,
            "{\"b\":\"x,y\",\"a\":1,\"d\":\"2023-01-01\"}\n{\"b\":\"z\",\"a\":null,\"d\":null}\n",
            "keys in column order, dates in ISO form"
        );

        let again = Destination {
            path: dir.join("r.csv"),
            format: FileFormat::Csv,
            append: true,
        };
        let mut file = ResultFile::create(&again, &names, &types).unwrap();
        file.write_rows(&rows[1..]).unwrap();
        file.finish().unwrap();
        let csv = std::fs::read_to_string(dir.join("r.csv")).unwrap();
        assert_eq!(
            csv, "b,a,d\n\"x,y\",1,2023-01-01\nz,,\nz,,\n",
            "an append adds no second header"
        );
    }

    #[test]
    fn a_missing_folder_is_named() {
        let to = Destination {
            path: "/no/such/folder/r.csv".into(),
            format: FileFormat::Csv,
            append: false,
        };
        let error = ResultFile::create(&to, &[], &[]).err().unwrap();
        assert!(error.to_string().contains("no folder /no/such/folder"));
    }

    #[test]
    fn the_message_says_when_rows_were_left_out() {
        let mut saved = Saved {
            path: "r.csv".into(),
            rows: 1,
            capped: false,
            missing: None,
            notes: Vec::new(),
        };
        assert_eq!(saved.message(), "Saved 1 row to r.csv");
        saved.capped = true;
        saved.notes.push("x (struct) written as text".into());
        let text = saved.message();
        assert!(text.contains("Stopped at the 1000000-row cap"), "{text}");
        assert!(text.ends_with(". x (struct) written as text"), "{text}");
    }

    #[test]
    fn home_is_expanded() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expand_home(Path::new("~/a.csv")),
            Path::new(&home).join("a.csv")
        );
        assert_eq!(expand_home(Path::new("a.csv")), PathBuf::from("a.csv"));
    }
}

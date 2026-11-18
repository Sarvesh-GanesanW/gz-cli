//! Writing result rows (JSON, as the lakehouse pages them) to a Parquet
//! file with typed columns.
//!
//! Each column's type comes from the page's schema, named as the session
//! names Spark types (groundzero-iceberg `iceberghelper/query_results.py`
//! `spark_type_name`: `integer`, `long`, `decimal`, `date`, `timestamp`,
//! …). Values arrive as JSON: decimals as text (`"1.50"`), dates and
//! timestamps as HTTP dates (`"Wed, 18 Nov 2026 11:07:43 GMT"`), arrays,
//! maps and structs as JSON. Nested and binary columns are written as JSON
//! text. A column whose first page does not read as its type is written
//! as text, and the save says so.

use std::fs::File;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arrow_array::{
    ArrayRef, BooleanArray, Date32Array, Decimal128Array, Float32Array, Float64Array, Int32Array,
    Int64Array, RecordBatch, StringArray, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde_json::Value;

use super::cells::{parse_instant, MICROS_PER_DAY};

/// Widest decimal Parquet holds in 128 bits.
const DECIMAL_PRECISION: u8 = 38;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Boolean,
    Int32,
    Int64,
    Float32,
    Float64,
    Decimal {
        scale: i8,
    },
    Date,
    /// `utc` for Spark's `timestamp`; `timestamp_ntz` has no zone.
    Timestamp {
        utc: bool,
    },
    Text,
}

impl Kind {
    fn data_type(self) -> DataType {
        match self {
            Kind::Boolean => DataType::Boolean,
            Kind::Int32 => DataType::Int32,
            Kind::Int64 => DataType::Int64,
            Kind::Float32 => DataType::Float32,
            Kind::Float64 => DataType::Float64,
            Kind::Decimal { scale } => DataType::Decimal128(DECIMAL_PRECISION, scale),
            Kind::Date => DataType::Date32,
            Kind::Timestamp { utc } => {
                DataType::Timestamp(TimeUnit::Microsecond, utc.then(|| "UTC".into()))
            }
            Kind::Text => DataType::Utf8,
        }
    }
}

pub struct ParquetSink {
    file: Option<File>,
    columns: Vec<String>,
    types: Vec<String>,
    kinds: Vec<Kind>,
    writer: Option<ArrowWriter<File>>,
    notes: Vec<String>,
}

impl ParquetSink {
    pub fn new(file: File, columns: &[String], types: &[String]) -> Self {
        Self {
            file: Some(file),
            columns: columns.to_vec(),
            types: types.to_vec(),
            kinds: Vec::new(),
            writer: None,
            notes: Vec::new(),
        }
    }

    pub fn write(&mut self, rows: &[Value]) -> Result<()> {
        if self.writer.is_none() {
            self.start(rows)?;
        }
        let arrays = self
            .columns
            .iter()
            .zip(&self.kinds)
            .map(|(column, kind)| array(*kind, rows, column))
            .collect::<Result<Vec<_>>>()?;
        let schema = self.schema();
        let batch = RecordBatch::try_new(schema, arrays).context("cannot build a Parquet batch")?;
        let writer = self
            .writer
            .as_mut()
            .context("the Parquet writer is not open")?;
        writer.write(&batch).context("cannot write Parquet rows")
    }

    /// Settles each column's type from the first rows and opens the file.
    fn start(&mut self, rows: &[Value]) -> Result<()> {
        let kinds: Vec<Kind> = self
            .columns
            .iter()
            .enumerate()
            .map(|(at, column)| {
                let named = self.types.get(at).map(String::as_str).unwrap_or_default();
                let kind = kind_for(named, rows, column);
                if kind == Kind::Text && !is_text_type(named) {
                    self.notes
                        .push(format!("{column} ({}) written as text", type_label(named)));
                }
                match array(kind, rows, column) {
                    Ok(_) => kind,
                    Err(err) => {
                        self.notes
                            .push(format!("{column} written as text: {err:#}"));
                        Kind::Text
                    }
                }
            })
            .collect();
        self.kinds = kinds;
        let file = self
            .file
            .take()
            .context("the Parquet file is already open")?;
        let properties = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        self.writer = Some(
            ArrowWriter::try_new(file, self.schema(), Some(properties))
                .context("cannot start the Parquet file")?,
        );
        Ok(())
    }

    fn schema(&self) -> Arc<Schema> {
        let fields: Vec<Field> = self
            .columns
            .iter()
            .zip(&self.kinds)
            .map(|(name, kind)| Field::new(name, kind.data_type(), true))
            .collect();
        Arc::new(Schema::new(fields))
    }

    /// Closes the file; returns notes on columns written as text.
    pub fn finish(mut self) -> Result<Vec<String>> {
        if self.writer.is_none() {
            self.start(&[])?;
        }
        if let Some(writer) = self.writer.take() {
            writer.close().context("cannot finish the Parquet file")?;
        }
        Ok(self.notes)
    }
}

fn type_label(named: &str) -> &str {
    if named.is_empty() {
        "no type"
    } else {
        named
    }
}

fn is_text_type(named: &str) -> bool {
    matches!(named, "string" | "char" | "varchar")
}

/// The Parquet column for a type the session names, or for the values
/// themselves when the page came without a schema.
fn kind_for(named: &str, rows: &[Value], column: &str) -> Kind {
    let base = named.split('(').next().unwrap_or_default().trim();
    match base {
        "boolean" => Kind::Boolean,
        "byte" | "tinyint" | "short" | "smallint" | "integer" | "int" => Kind::Int32,
        "long" | "bigint" => Kind::Int64,
        "float" => Kind::Float32,
        "double" => Kind::Float64,
        "decimal" => Kind::Decimal {
            scale: declared_scale(named).unwrap_or_else(|| observed_scale(rows, column)),
        },
        "date" => Kind::Date,
        "timestamp" => Kind::Timestamp { utc: true },
        "timestamp_ntz" => Kind::Timestamp { utc: false },
        "" => inferred(rows, column),
        _ => Kind::Text,
    }
}

/// `decimal(10,2)` → 2.
fn declared_scale(named: &str) -> Option<i8> {
    let inside = named.split_once('(')?.1.trim_end_matches(')');
    inside.split(',').nth(1)?.trim().parse().ok()
}

/// The most digits after the point among the first rows' values.
fn observed_scale(rows: &[Value], column: &str) -> i8 {
    let widest = rows
        .iter()
        .filter_map(|row| decimal_text(row.get(column)?))
        .filter_map(|text| text.split_once('.').map(|(_, frac)| frac.len()))
        .max()
        .unwrap_or(0);
    widest.min(DECIMAL_PRECISION as usize) as i8
}

fn inferred(rows: &[Value], column: &str) -> Kind {
    let first = rows
        .iter()
        .filter_map(|r| r.get(column))
        .find(|v| !v.is_null());
    match first {
        Some(Value::Bool(_)) => Kind::Boolean,
        Some(Value::Number(n)) if n.is_i64() => Kind::Int64,
        Some(Value::Number(_)) => Kind::Float64,
        _ => Kind::Text,
    }
}

fn cells<'a>(rows: &'a [Value], column: &'a str) -> impl Iterator<Item = Option<&'a Value>> {
    rows.iter()
        .map(move |row| row.get(column).filter(|v| !v.is_null()))
}

/// Reads every row's `column` as `kind`; an error names the first value
/// that does not fit.
fn array(kind: Kind, rows: &[Value], column: &str) -> Result<ArrayRef> {
    fn read<T>(
        rows: &[Value],
        column: &str,
        what: &str,
        convert: impl Fn(&Value) -> Option<T>,
    ) -> Result<Vec<Option<T>>> {
        cells(rows, column)
            .map(|cell| match cell {
                None => Ok(None),
                Some(value) => match convert(value) {
                    Some(converted) => Ok(Some(converted)),
                    None => bail!("{value} is not a {what}"),
                },
            })
            .collect()
    }
    let array: ArrayRef = match kind {
        Kind::Boolean => Arc::new(BooleanArray::from(read(
            rows,
            column,
            "boolean",
            Value::as_bool,
        )?)),
        Kind::Int32 => Arc::new(Int32Array::from(read(
            rows,
            column,
            "32-bit integer",
            |v| v.as_i64().and_then(|n| i32::try_from(n).ok()),
        )?)),
        Kind::Int64 => Arc::new(Int64Array::from(read(
            rows,
            column,
            "64-bit integer",
            Value::as_i64,
        )?)),
        Kind::Float32 => Arc::new(Float32Array::from(read(rows, column, "number", |v| {
            v.as_f64().map(|n| n as f32)
        })?)),
        Kind::Float64 => Arc::new(Float64Array::from(read(
            rows,
            column,
            "number",
            Value::as_f64,
        )?)),
        Kind::Decimal { scale } => Arc::new(
            Decimal128Array::from(read(rows, column, "decimal", |v| {
                decimal_text(v).and_then(|t| scaled_decimal(&t, scale))
            })?)
            .with_precision_and_scale(DECIMAL_PRECISION, scale)
            .context("cannot size a decimal column")?,
        ),
        Kind::Date => Arc::new(Date32Array::from(read(rows, column, "date", |v| {
            parse_instant(v.as_str()?).map(|(days, _)| days as i32)
        })?)),
        Kind::Timestamp { utc } => {
            let values = TimestampMicrosecondArray::from(read(rows, column, "timestamp", |v| {
                parse_instant(v.as_str()?).map(|(days, micros)| days * MICROS_PER_DAY + micros)
            })?);
            Arc::new(if utc {
                values.with_timezone("UTC")
            } else {
                values
            })
        }
        Kind::Text => Arc::new(StringArray::from(
            cells(rows, column)
                .map(|cell| cell.map(text_of))
                .collect::<Vec<_>>(),
        )),
    };
    Ok(array)
}

/// A text cell: strings as they are, anything else as JSON.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn decimal_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// `"-12.5"` at scale 2 → -1250; `None` when it has more digits after the
/// point than the scale keeps, or is no decimal.
fn scaled_decimal(text: &str, scale: i8) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let scale = usize::try_from(scale).ok()?;
    let all_digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() && fraction.is_empty()
        || !all_digits(whole)
        || !all_digits(fraction)
        || fraction.len() > scale
    {
        return None;
    }
    let padded = format!("{whole}{fraction}{}", "0".repeat(scale - fraction.len()));
    let magnitude: i128 = if padded.is_empty() {
        0
    } else {
        padded.parse().ok()?
    };
    Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use serde_json::json;

    #[test]
    fn scales_decimals_exactly() {
        assert_eq!(scaled_decimal("12.5", 2), Some(1250));
        assert_eq!(scaled_decimal("-0.07", 2), Some(-7));
        assert_eq!(scaled_decimal("3", 0), Some(3));
        assert_eq!(scaled_decimal("1.234", 2), None, "would lose a digit");
        assert_eq!(scaled_decimal("abc", 2), None);
        assert_eq!(
            observed_scale(&[json!({"d": "1.50"}), json!({"d": "2.125"})], "d"),
            3
        );
        assert_eq!(declared_scale("decimal(10,2)"), Some(2));
    }

    #[test]
    fn writes_typed_columns_and_notes_text_ones() {
        let path = std::env::temp_dir().join(format!("gz-parquet-{}.parquet", std::process::id()));
        let columns: Vec<String> = ["i", "l", "d", "day", "ts", "s", "nested", "odd"]
            .iter()
            .map(|c| c.to_string())
            .collect();
        let types: Vec<String> = [
            "integer",
            "long",
            "decimal",
            "date",
            "timestamp",
            "string",
            "struct",
            "integer",
        ]
        .iter()
        .map(|t| t.to_string())
        .collect();
        let rows = [
            json!({"i": 1, "l": 5_000_000_000_i64, "d": "1.50", "day": "Wed, 18 Nov 2026 00:00:00 GMT",
                   "ts": "Wed, 18 Nov 2026 11:07:43 GMT", "s": "x", "nested": {"a": 1}, "odd": "n/a"}),
            json!({"i": null, "l": 2, "d": "-2.25", "day": null, "ts": null, "s": null, "nested": null, "odd": 3}),
        ];
        let mut sink = ParquetSink::new(File::create(&path).unwrap(), &columns, &types);
        sink.write(&rows[..1]).unwrap();
        sink.write(&rows[1..]).unwrap();
        let notes = sink.finish().unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].starts_with("nested (struct) written as text"));
        assert!(notes[1].contains("odd written as text: \"n/a\" is not a 32-bit integer"));

        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&path).unwrap())
            .unwrap()
            .build()
            .unwrap();
        let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
        assert_eq!(batches.len(), 1, "both pages land in one row group");
        let batch = &batches[0];
        let schema = batch.schema();
        let types: Vec<DataType> = schema
            .fields()
            .iter()
            .map(|f| f.data_type().clone())
            .collect();
        assert_eq!(
            types,
            vec![
                DataType::Int32,
                DataType::Int64,
                DataType::Decimal128(38, 2),
                DataType::Date32,
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                DataType::Utf8,
                DataType::Utf8,
                DataType::Utf8,
            ]
        );
        let decimals = batch
            .column(2)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(decimals.value(1), -225);
        assert!(batch.column(0).is_null(1));
        let nested = batch
            .column(6)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(nested.value(0), "{\"a\":1}");
    }
}

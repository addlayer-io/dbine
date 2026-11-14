//! `executeDaxQueries` (spec §4.2, Arrow). Owned by part B.
//!
//! - Body: `{"query":…,"queryTimeout":…,"resultSetRowCountLimit":…,
//!   "effectiveUsername":…,"roles":[…],"customData":…}`; whole-result
//!   paths always send a limit of their own (F4).
//! - The answer: concatenated Arrow IPC streams (LZ4_FRAME), decoded
//!   incrementally from the byte stream ([`Decoder`]: a new
//!   `StreamDecoder` at each end-of-stream) so a large answer never sits
//!   whole in memory; dictionary batches and `dense_union` (Variant) taken;
//!   decoding behind `catch_unwind` (S7).
//! - Cells: `int64`, `double`, `decimal128(19,4)` (as text), `date64` (as
//!   `YYYY-MM-DD HH:MM:SS[.fff]`), dictionary `utf8`, `bool`, `binary`
//!   (`json_bytes`), `dense_union`, nulls (ported from
//!   `flightsql/src/cells.rs`).
//! - An error rowset (schema metadata `IsError=true`, `FaultCode`,
//!   `FaultString`, `ErrorCode`/`ErrorMessage`/location columns) with
//!   HTTP 200 → `Error::Statement`.

use super::Rowset;
use arrow_array::cast::AsArray;
use arrow_array::types::*;
use arrow_array::{Array, ArrayRef, RecordBatch, UnionArray};
use arrow_buffer::{Buffer, MutableBuffer};
use arrow_ipc::reader::StreamDecoder;
use arrow_schema::{ArrowError, DataType, SchemaRef};
use dbine_driver::transfer::Cell;
use dbine_driver::{json_bytes, json_f64, json_i64, json_u64, Error, Result, ScriptError};
use serde_json::{json, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// The request body for one unit.
pub fn body(
    query: &str,
    row_limit: Option<u64>,
    timeout_secs: u64,
    effective_username: Option<&str>,
    roles: &[String],
    custom_data: Option<&str>,
) -> Value {
    let mut b = json!({ "query": query, "queryTimeout": timeout_secs });
    if let Some(n) = row_limit {
        b["resultSetRowCountLimit"] = json!(n);
    }
    if let Some(u) = effective_username {
        b["effectiveUsername"] = json!(u);
    }
    if !roles.is_empty() {
        b["roles"] = json!(roles);
    }
    if let Some(c) = custom_data {
        b["customData"] = json!(c);
    }
    b
}

/// What the stream brought.
#[derive(Debug)]
pub enum Event {
    /// A result set starts (its schema).
    Begin {
        schema: SchemaRef,
        /// An error rowset (`IsError=true`).
        error: bool,
    },
    Batch(RecordBatch),
    /// The result set ended.
    End,
}

/// Decodes concatenated IPC streams as their bytes arrive.
///
/// The decoder slices message bodies out of the buffers it's given, and
/// some arrays (a dense union's offsets) are taken from those slices as
/// they are, so every buffer it gets is an aligned copy that starts at an
/// 8-byte boundary of the answer (IPC pads each message to 8 bytes).
#[derive(Debug, Default)]
pub struct Decoder {
    dec: StreamDecoder,
    begun: bool,
    /// Bytes past the last 8-byte boundary, waiting for the next chunk.
    pending: Vec<u8>,
}

fn bad_stream(e: impl std::fmt::Display) -> Error {
    Error::Query(format!(
        "Power BI respondió un resultado Arrow que no se pudo leer: {e}"
    ))
}

/// `bytes` in a new, aligned allocation.
fn aligned(bytes: &[u8]) -> Buffer {
    let mut m = MutableBuffer::new(bytes.len());
    m.extend_from_slice(bytes);
    m.into()
}

fn meta_true(schema: &SchemaRef, key: &str) -> bool {
    schema
        .metadata()
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(key) && v.trim().eq_ignore_ascii_case("true"))
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    fn begin(&mut self, out: &mut Vec<Event>) {
        if self.begun {
            return;
        }
        if let Some(schema) = self.dec.schema() {
            self.begun = true;
            let error = meta_true(&schema, "IsError");
            out.push(Event::Begin { schema, error });
        }
    }

    /// Feeds the next bytes of the answer; what they completed goes to
    /// `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<Event>) -> Result<()> {
        self.pending.extend_from_slice(chunk);
        let n = self.pending.len() / 8 * 8;
        if n == 0 {
            return Ok(());
        }
        let mut buf = aligned(&self.pending[..n]);
        self.pending.drain(..n);
        self.feed(&mut buf, out)
    }

    fn feed(&mut self, buf: &mut Buffer, out: &mut Vec<Event>) -> Result<()> {
        while !buf.is_empty() {
            let dec = &mut self.dec;
            let r = catch_unwind(AssertUnwindSafe(|| dec.decode(buf)))
                .map_err(|_| bad_stream("datos inconsistentes"))?;
            match r {
                Ok(Some(batch)) => {
                    self.begin(out);
                    out.push(Event::Batch(batch));
                }
                Ok(None) => self.begin(out),
                // The stream ended and the next one starts (one per
                // EVALUATE): a new decoder takes the rest.
                Err(ArrowError::IpcError(m))
                    if m == "Unexpected EOS" && self.dec.finish().is_ok() =>
                {
                    self.begin(out);
                    if self.begun {
                        out.push(Event::End);
                    }
                    self.dec = StreamDecoder::new();
                    self.begun = false;
                }
                Err(e) => return Err(bad_stream(e)),
            }
        }
        Ok(())
    }

    /// The answer ended: closes the last result set.
    pub fn finish(&mut self, out: &mut Vec<Event>) -> Result<()> {
        if !self.pending.is_empty() {
            let mut buf = aligned(&std::mem::take(&mut self.pending));
            self.feed(&mut buf, out)?;
        }
        self.dec.finish().map_err(bad_stream)?;
        self.begin(out);
        if self.begun {
            out.push(Event::End);
            self.begun = false;
        }
        Ok(())
    }
}

/// The model type a column's Arrow type stands for (what `ColumnInfo`
/// and transfers name): `Int64`, `Double`, `Decimal`, `DateTime`,
/// `String`, `Boolean`, `Binary`, `Variant`.
pub fn model_type(dt: &DataType) -> String {
    match dt {
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => "Int64".into(),
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => "Int64".into(),
        DataType::Float16 | DataType::Float32 | DataType::Float64 => "Double".into(),
        DataType::Decimal32(..)
        | DataType::Decimal64(..)
        | DataType::Decimal128(..)
        | DataType::Decimal256(..) => "Decimal".into(),
        DataType::Date32 | DataType::Date64 | DataType::Timestamp(..) => "DateTime".into(),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "String".into(),
        DataType::Boolean => "Boolean".into(),
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => "Binary".into(),
        DataType::Union(..) => "Variant".into(),
        DataType::Dictionary(_, v) => model_type(v),
        other => other.to_string(),
    }
}

/// A dictionary column as its plain values (strings come dictionary-
/// encoded by default).
pub fn plain(a: &ArrayRef) -> Result<ArrayRef> {
    match a.data_type() {
        DataType::Dictionary(_, v) => arrow_cast::cast(a, v).map_err(bad_stream),
        _ => Ok(a.clone()),
    }
}

/// `date64` milliseconds as `YYYY-MM-DD HH:MM:SS[.fff]`.
fn datetime_ms(ms: i64) -> Option<String> {
    let t = chrono::DateTime::from_timestamp_millis(ms)?.naive_utc();
    Some(if ms.rem_euclid(1000) == 0 {
        t.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
    })
}

fn display(a: &dyn Array, row: usize) -> Value {
    arrow_cast::display::array_value_to_string(a, row)
        .map(Value::String)
        .unwrap_or(Value::Null)
}

fn binary_at(a: &dyn Array, row: usize) -> Option<Vec<u8>> {
    Some(match a.data_type() {
        DataType::Binary => a.as_binary::<i32>().value(row).to_vec(),
        DataType::LargeBinary => a.as_binary::<i64>().value(row).to_vec(),
        DataType::BinaryView => a.as_binary_view().value(row).to_vec(),
        DataType::FixedSizeBinary(_) => a.as_fixed_size_binary().value(row).to_vec(),
        _ => return None,
    })
}

/// One cell as the grid shows it.
pub fn cell(a: &dyn Array, row: usize) -> Value {
    if a.is_null(row) {
        return Value::Null;
    }
    match a.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(a.as_boolean().value(row)),
        DataType::Int8 => json_i64(a.as_primitive::<Int8Type>().value(row) as i64),
        DataType::Int16 => json_i64(a.as_primitive::<Int16Type>().value(row) as i64),
        DataType::Int32 => json_i64(a.as_primitive::<Int32Type>().value(row) as i64),
        DataType::Int64 => json_i64(a.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => json_u64(a.as_primitive::<UInt8Type>().value(row) as u64),
        DataType::UInt16 => json_u64(a.as_primitive::<UInt16Type>().value(row) as u64),
        DataType::UInt32 => json_u64(a.as_primitive::<UInt32Type>().value(row) as u64),
        DataType::UInt64 => json_u64(a.as_primitive::<UInt64Type>().value(row)),
        DataType::Float32 => json_f64(a.as_primitive::<Float32Type>().value(row) as f64),
        DataType::Float64 => json_f64(a.as_primitive::<Float64Type>().value(row)),
        DataType::Utf8 => Value::String(a.as_string::<i32>().value(row).to_string()),
        DataType::LargeUtf8 => Value::String(a.as_string::<i64>().value(row).to_string()),
        DataType::Utf8View => Value::String(a.as_string_view().value(row).to_string()),
        DataType::Date64 => datetime_ms(a.as_primitive::<Date64Type>().value(row))
            .map_or(Value::Null, Value::String),
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => binary_at(a, row)
            .map(|b| json_bytes(&b))
            .unwrap_or(Value::Null),
        DataType::Timestamp(..) => match display(a, row) {
            Value::String(s) => Value::String(s.replacen('T', " ", 1)),
            v => v,
        },
        DataType::Dictionary(..) => match a.as_any_dictionary_opt() {
            Some(d) => {
                let keys = d.normalized_keys();
                match keys.get(row) {
                    Some(&k) => cell(d.values().as_ref(), k),
                    None => Value::Null,
                }
            }
            None => display(a, row),
        },
        DataType::Union(..) => match a.as_any().downcast_ref::<UnionArray>() {
            Some(u) => {
                let child = u.child(u.type_id(row));
                cell(child.as_ref(), u.value_offset(row))
            }
            None => Value::Null,
        },
        _ => display(a, row),
    }
}

/// One cell, typed without loss, for transfers.
pub fn typed_cell(a: &dyn Array, row: usize) -> Cell {
    if a.is_null(row) {
        return Cell::Null;
    }
    match a.data_type() {
        DataType::Boolean => Cell::Bool(a.as_boolean().value(row)),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            match cell(a, row) {
                Value::Number(n) => n.as_i64().map_or(Cell::Null, Cell::Int),
                Value::String(s) => s.parse().map_or(Cell::Text(s), Cell::Int),
                _ => Cell::Null,
            }
        }
        DataType::Float32 | DataType::Float64 => match a.data_type() {
            DataType::Float32 => Cell::Float(a.as_primitive::<Float32Type>().value(row) as f64),
            _ => Cell::Float(a.as_primitive::<Float64Type>().value(row)),
        },
        DataType::Decimal128(..)
        | DataType::Decimal256(..)
        | DataType::Decimal64(..)
        | DataType::Decimal32(..) => match display(a, row) {
            Value::String(s) => Cell::Decimal(s),
            _ => Cell::Null,
        },
        DataType::Date64 => datetime_ms(a.as_primitive::<Date64Type>().value(row))
            .map_or(Cell::Null, Cell::DateTime),
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => binary_at(a, row).map_or(Cell::Null, Cell::Bytes),
        DataType::Dictionary(..) => match a.as_any_dictionary_opt() {
            Some(d) => match d.normalized_keys().get(row) {
                Some(&k) => typed_cell(d.values().as_ref(), k),
                None => Cell::Null,
            },
            None => Cell::Null,
        },
        DataType::Union(..) => match a.as_any().downcast_ref::<UnionArray>() {
            Some(u) => typed_cell(u.child(u.type_id(row)).as_ref(), u.value_offset(row)),
            None => Cell::Null,
        },
        DataType::Timestamp(..) => match cell(a, row) {
            Value::String(s) => Cell::DateTime(s),
            _ => Cell::Null,
        },
        _ => match cell(a, row) {
            Value::String(s) => Cell::Text(s),
            v => Cell::from_json(&v),
        },
    }
}

/// A batch's columns, dictionaries decoded once (not per cell).
fn plain_columns(batch: &RecordBatch) -> Result<Vec<ArrayRef>> {
    batch.columns().iter().map(plain).collect()
}

/// The rows of a batch as grid cells (decoding errors, not panics).
pub fn rows_of(batch: &RecordBatch) -> Result<Vec<Vec<Value>>> {
    let cols = plain_columns(batch)?;
    catch_unwind(AssertUnwindSafe(|| {
        (0..batch.num_rows())
            .map(|r| cols.iter().map(|c| cell(c.as_ref(), r)).collect())
            .collect()
    }))
    .map_err(|_| bad_stream("una celda inconsistente"))
}

/// The rows of a batch as typed cells.
pub fn typed_rows_of(batch: &RecordBatch) -> Result<Vec<Vec<Cell>>> {
    let cols = plain_columns(batch)?;
    catch_unwind(AssertUnwindSafe(|| {
        (0..batch.num_rows())
            .map(|r| cols.iter().map(|c| typed_cell(c.as_ref(), r)).collect())
            .collect()
    }))
    .map_err(|_| bad_stream("una celda inconsistente"))
}

/// An error rowset as the statement error, with its 1-based line and
/// column when the rowset says where.
pub fn error_of(schema: &SchemaRef, rows: &[Vec<Value>]) -> (ScriptError, Option<(u32, u32)>) {
    let meta = |k: &str| {
        schema
            .metadata()
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
    };
    let names: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| f.name().to_ascii_lowercase())
        .collect();
    let first = rows.first();
    let text_at = |pred: &dyn Fn(&str) -> bool| -> Option<String> {
        let i = names.iter().position(|n| pred(n))?;
        match first?.get(i)? {
            Value::Null => None,
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    };
    let message = text_at(&|n| n.ends_with("errormessage"))
        .or_else(|| meta("FaultString"))
        .or_else(|| text_at(&|n| n.ends_with("errordescription")))
        .unwrap_or_else(|| "Power BI rechazó la consulta".to_string());
    let code = text_at(&|n| n.ends_with("errorcode")).or_else(|| meta("FaultCode"));
    let num =
        |pred: &dyn Fn(&str) -> bool| text_at(pred).and_then(|s| s.trim().parse::<u32>().ok());
    let line = num(&|n| n.contains("line"));
    let column = num(&|n| (n.contains("column") || n.contains("position")) && !n.contains("line"));
    let mut e = ScriptError::new(crate::auth::short(&message, 2000));
    if let Some(c) = code {
        e = e.with_code(c);
    }
    (e, line.map(|l| (l, column.unwrap_or(1))))
}

/// The result sets of a whole answer (tests and small reads; the query
/// path decodes the stream as it arrives).
pub fn parse(body: &[u8], max_rows: usize) -> Result<Vec<Rowset>> {
    let mut dec = Decoder::new();
    let mut events = Vec::new();
    dec.push(body, &mut events)?;
    dec.finish(&mut events)?;
    let mut sets: Vec<Rowset> = Vec::new();
    let mut error: Option<(SchemaRef, Vec<Vec<Value>>)> = None;
    for ev in events {
        match ev {
            Event::Begin {
                schema,
                error: true,
            } => error = Some((schema, Vec::new())),
            Event::Begin { schema, .. } => sets.push(Rowset {
                columns: schema.fields().iter().map(|f| f.name().clone()).collect(),
                types: schema
                    .fields()
                    .iter()
                    .map(|f| f.data_type().to_string())
                    .collect(),
                rows: Vec::new(),
                truncated: false,
            }),
            Event::Batch(b) => {
                let rows = rows_of(&b)?;
                if let Some((_, e)) = &mut error {
                    e.extend(rows);
                } else if let Some(s) = sets.last_mut() {
                    for r in rows {
                        if s.rows.len() < max_rows {
                            s.rows.push(r);
                        } else {
                            s.truncated = true;
                        }
                    }
                }
            }
            Event::End => {
                if let Some((schema, rows)) = error.take() {
                    return Err(Error::Statement(Box::new(error_of(&schema, &rows).0)));
                }
            }
        }
    }
    Ok(sets)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use arrow_array::builder::StringDictionaryBuilder;
    use arrow_array::{
        BinaryArray, BooleanArray, Date64Array, Decimal128Array, Float64Array, Int64Array,
        StringArray,
    };
    use arrow_ipc::writer::{IpcWriteOptions, StreamWriter};
    use arrow_ipc::CompressionType;
    use arrow_schema::{Field, Schema, UnionFields, UnionMode};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// One IPC stream, LZ4_FRAME-compressed as the service sends it.
    pub(crate) fn stream(batch: &RecordBatch) -> Vec<u8> {
        let opts = IpcWriteOptions::default()
            .try_with_compression(Some(CompressionType::LZ4_FRAME))
            .unwrap();
        let mut w = StreamWriter::try_new_with_options(Vec::new(), &batch.schema(), opts).unwrap();
        w.write(batch).unwrap();
        w.finish().unwrap();
        w.into_inner().unwrap()
    }

    /// A result set of every type the service sends (spec F2).
    pub(crate) fn every_type() -> RecordBatch {
        let mut dict = StringDictionaryBuilder::<Int32Type>::new();
        dict.append_value("Norte");
        dict.append_value("Sur");
        dict.append_null();
        let union_fields = UnionFields::try_new(
            vec![0, 1],
            vec![
                Field::new("int64", DataType::Int64, true),
                Field::new("utf8", DataType::Utf8, true),
            ],
        )
        .unwrap();
        let variant = UnionArray::try_new(
            union_fields.clone(),
            vec![0i8, 1, 0].into(),
            Some(vec![0i32, 0, 1].into()),
            vec![
                Arc::new(Int64Array::from(vec![7, 9])) as ArrayRef,
                Arc::new(StringArray::from(vec!["siete"])) as ArrayRef,
            ],
        )
        .unwrap();
        let schema = Schema::new(vec![
            Field::new("Sales[Qty]", DataType::Int64, true),
            Field::new("Sales[Ratio]", DataType::Float64, true),
            Field::new("Sales[Amount]", DataType::Decimal128(19, 4), true),
            Field::new("Date[Date]", DataType::Date64, true),
            Field::new(
                "Geo[Region]",
                DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
                true,
            ),
            Field::new("Sales[Paid]", DataType::Boolean, true),
            Field::new("Sales[Blob]", DataType::Binary, true),
            Field::new(
                "[Variant]",
                DataType::Union(union_fields, UnionMode::Dense),
                true,
            ),
        ]);
        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Int64Array::from(vec![
                    Some(9007199254740993),
                    Some(1),
                    None,
                ])),
                Arc::new(Float64Array::from(vec![Some(0.5), None, Some(-1.25)])),
                Arc::new(
                    Decimal128Array::from(vec![Some(123_450_000), Some(-1), None])
                        .with_precision_and_scale(19, 4)
                        .unwrap(),
                ),
                Arc::new(Date64Array::from(vec![
                    Some(1_706_708_700_123),
                    Some(1_706_659_200_000),
                    None,
                ])),
                Arc::new(dict.finish()),
                Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
                Arc::new(BinaryArray::from(vec![
                    Some(&b"\xca\xfe"[..]),
                    None,
                    Some(&b""[..]),
                ])),
                Arc::new(variant),
            ],
        )
        .unwrap()
    }

    /// The error rowset of a DAX error (spec §4.2).
    pub(crate) fn error_rowset(message: &str) -> RecordBatch {
        let mut meta = HashMap::new();
        meta.insert("IsError".to_string(), "true".to_string());
        meta.insert("FaultCode".to_string(), "Client".to_string());
        meta.insert("FaultString".to_string(), message.to_string());
        let schema = Schema::new_with_metadata(
            vec![
                Field::new("ErrorCode", DataType::Utf8, true),
                Field::new("ErrorMessage", DataType::Utf8, true),
                Field::new("ErrorDescription", DataType::Utf8, true),
                Field::new("LineNumber", DataType::Int64, true),
                Field::new("ColumnNumber", DataType::Int64, true),
            ],
            meta,
        );
        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(vec!["-2147467259"])),
                Arc::new(StringArray::from(vec![message])),
                Arc::new(StringArray::from(vec!["DAX"])),
                Arc::new(Int64Array::from(vec![2])),
                Arc::new(Int64Array::from(vec![5])),
            ],
        )
        .unwrap()
    }

    pub(crate) fn ints(name: &str, values: &[i64]) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(name, DataType::Int64, true)])),
            vec![Arc::new(Int64Array::from(values.to_vec()))],
        )
        .unwrap()
    }

    #[test]
    fn every_dax_type_as_the_grid_shows_it() {
        let body = stream(&every_type());
        let sets = parse(&body, 100).unwrap();
        assert_eq!(sets.len(), 1);
        let s = &sets[0];
        assert_eq!(s.columns[0], "Sales[Qty]");
        assert_eq!(
            s.rows[0],
            vec![
                json!("9007199254740993"),
                json!(0.5),
                json!("12345.0000"),
                json!("2024-01-31 13:45:00.123"),
                json!("Norte"),
                json!(true),
                json!("0xCAFE"),
                json!(7),
            ]
        );
        assert_eq!(
            s.rows[1],
            vec![
                json!(1),
                Value::Null,
                json!("-0.0001"),
                json!("2024-01-31 00:00:00"),
                json!("Sur"),
                json!(false),
                Value::Null,
                json!("siete"),
            ]
        );
        assert_eq!(s.rows[2][0], Value::Null);
        assert_eq!(s.rows[2][4], Value::Null);
        assert_eq!(s.rows[2][7], json!(9));
        let types: Vec<String> = every_type()
            .schema()
            .fields()
            .iter()
            .map(|f| model_type(f.data_type()))
            .collect();
        assert_eq!(
            types,
            ["Int64", "Double", "Decimal", "DateTime", "String", "Boolean", "Binary", "Variant"]
        );
        // Typed cells for transfers: decimals exact, dates as text, bytes whole.
        let typed = typed_rows_of(&every_type()).unwrap();
        assert_eq!(typed[0][0], Cell::Int(9007199254740993));
        assert_eq!(typed[0][2], Cell::Decimal("12345.0000".into()));
        assert_eq!(
            typed[0][3],
            Cell::DateTime("2024-01-31 13:45:00.123".into())
        );
        assert_eq!(typed[0][4], Cell::Text("Norte".into()));
        assert_eq!(typed[0][6], Cell::Bytes(vec![0xca, 0xfe]));
        assert_eq!(typed[2][7], Cell::Int(9));
    }

    #[test]
    fn two_evaluates_two_streams_and_max_rows() {
        let mut body = stream(&ints("[a]", &[1, 2, 3]));
        body.extend(stream(&ints("[b]", &[4])));
        let sets = parse(&body, 2).unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].rows, vec![vec![json!(1)], vec![json!(2)]]);
        assert!(sets[0].truncated);
        assert_eq!(sets[1].columns, ["[b]"]);
        assert!(!sets[1].truncated);
        assert_eq!(sets[1].types, ["Int64"]);
    }

    #[test]
    fn a_schema_only_stream_is_an_empty_result() {
        let empty = RecordBatch::new_empty(ints("[a]", &[]).schema());
        let mut body = stream(&empty);
        body.extend(stream(&ints("[b]", &[1])));
        let sets = parse(&body, 10).unwrap();
        assert_eq!(sets.len(), 2);
        assert!(sets[0].rows.is_empty() && sets[0].columns == ["[a]"]);
    }

    #[test]
    fn decoding_byte_by_byte_gives_the_same() {
        let mut body = stream(&every_type());
        body.extend(stream(&ints("[b]", &[4, 5])));
        let mut dec = Decoder::new();
        let mut events = Vec::new();
        for b in &body {
            dec.push(std::slice::from_ref(b), &mut events).unwrap();
        }
        dec.finish(&mut events).unwrap();
        let begins = events
            .iter()
            .filter(|e| matches!(e, Event::Begin { .. }))
            .count();
        let ends = events.iter().filter(|e| matches!(e, Event::End)).count();
        let rows: usize = events
            .iter()
            .map(|e| match e {
                Event::Batch(b) => b.num_rows(),
                _ => 0,
            })
            .sum();
        assert_eq!((begins, ends, rows), (2, 2, 5));
    }

    #[test]
    fn an_error_rowset_is_a_statement_error() {
        let mut body = stream(&ints("[ok]", &[1]));
        body.extend(stream(&error_rowset("The syntax for 'x' is incorrect.")));
        let Err(Error::Statement(e)) = parse(&body, 10) else {
            panic!("expected a statement error");
        };
        assert_eq!(e.message, "The syntax for 'x' is incorrect.");
        assert_eq!(e.code.as_deref(), Some("-2147467259"));
        let schema = error_rowset("m").schema();
        let (_, pos) = error_of(&schema, &rows_of(&error_rowset("m")).unwrap());
        assert_eq!(pos, Some((2, 5)));
    }

    #[test]
    fn a_cut_or_garbage_stream_is_an_error_not_a_panic() {
        let body = stream(&every_type());
        for cut in [1, 7, body.len() / 2, body.len() - 1] {
            assert!(parse(&body[..cut], 10).is_err(), "{cut}");
        }
        for junk in [
            &b"\xff\xff\xff\xff\x10\x00\x00\x00garbage!garbage!"[..],
            b"not arrow at all",
        ] {
            assert!(parse(junk, 10).is_err());
        }
        assert!(parse(b"", 10).unwrap().is_empty());
    }

    #[test]
    fn bodies() {
        let b = body(
            "EVALUATE 'T'",
            Some(1001),
            600,
            Some("ana@contoso.com"),
            &["Norte".into()],
            Some("x"),
        );
        assert_eq!(
            b,
            json!({"query": "EVALUATE 'T'", "queryTimeout": 600, "resultSetRowCountLimit": 1001,
                   "effectiveUsername": "ana@contoso.com", "roles": ["Norte"], "customData": "x"})
        );
        let b = body("q", Some(1), 30, None, &[], None);
        assert!(b.get("roles").is_none() && b.get("effectiveUsername").is_none());
    }
}

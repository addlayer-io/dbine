//! Moving rows in bulk between databases (the migration's data copy).
//!
//! Rows travel in typed, lossless batches ([`RowBatch`] of [`Cell`]s), not
//! as the grid's `serde_json::Value`s: binaries whole, decimals and money
//! exact, dates as the engine gave them. A batch closes at [`CHUNK_ROWS`]
//! rows or [`CHUNK_BYTES`] bytes, whichever comes first, so memory stays
//! bounded even with wide rows, and batches (not rows) are what crosses the
//! channel between a driver host and the app.
//!
//! A driver takes part through three optional pieces:
//! - [`Session::read_batches`](crate::Session::read_batches): read a table
//!   in batches. Every driver has it (the default adapts `execute`); drivers
//!   override it to read typed values.
//! - [`Session::bulk_load`](crate::Session::bulk_load): the engine's native
//!   bulk load (`INSERT BULK`, `COPY`, `LOAD DATA`, an appender…), with
//!   [`Driver::supports_bulk_load`](crate::Driver::supports_bulk_load).
//!   Without it, the migration writes the driver's `insert_script`.
//! - [`Driver::copy_native`](crate::Driver::copy_native): source and target
//!   are the same driver: copy inside it, rows never decoded (SQL Server's
//!   raw TDS rows, PostgreSQL's binary `COPY`).

use crate::ObjectRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use std::sync::{Arc, Mutex};

/// A batch closes at this many rows…
pub const CHUNK_ROWS: usize = 1_000;
/// …or at this many bytes (estimated), whichever comes first.
pub const CHUNK_BYTES: usize = 2 * 1024 * 1024;

/// One value, without loss. Temporal values and decimals keep the engine's
/// own text (ISO 8601 / plain digits) so no precision or range is lost on
/// the way; each target parses what it needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// Exact decimal (`numeric`, `money`…): digits with an optional sign and
    /// point, no exponent.
    Decimal(String),
    Text(String),
    Bytes(#[serde(with = "bytes")] Vec<u8>),
    /// `YYYY-MM-DD`.
    Date(String),
    /// `HH:MM:SS[.fffffffff]`.
    Time(String),
    /// `YYYY-MM-DD HH:MM:SS[.fffffffff]`, no zone.
    DateTime(String),
    /// `YYYY-MM-DD HH:MM:SS[.fffffffff]±HH:MM`.
    DateTimeTz(String),
    /// Canonical `8-4-4-4-12` hex.
    Uuid(String),
    /// A JSON document (JSON columns, nested document fields, arrays).
    Json(String),
}

impl Cell {
    /// Estimated bytes it takes in memory (for the batch's byte bound).
    pub fn size(&self) -> usize {
        16 + match self {
            Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => s.len(),
            Cell::Bytes(b) => b.len(),
            _ => 0,
        }
    }

    /// From a grid value (drivers that only give `serde_json::Value`s).
    /// Strings stay text: without the column's type, nothing better is known.
    pub fn from_json(v: &Value) -> Cell {
        match v {
            Value::Null => Cell::Null,
            Value::Bool(b) => Cell::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Cell::Int(i)
                } else if let Some(u) = n.as_u64() {
                    Cell::UInt(u)
                } else {
                    Cell::Float(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            Value::String(s) => Cell::Text(s.clone()),
            other => Cell::Json(other.to_string()),
        }
    }

    /// As a grid value (for the `insert_script` path). Binaries become the
    /// `0x…` hex text drivers' literals recognise, whole.
    pub fn to_json(&self) -> Value {
        match self {
            Cell::Null => Value::Null,
            Cell::Bool(b) => Value::Bool(*b),
            Cell::Int(i) => crate::json_i64(*i),
            Cell::UInt(u) => crate::json_u64(*u),
            Cell::Float(f) => crate::json_f64(*f),
            Cell::Bytes(b) => {
                let mut s = String::with_capacity(2 + b.len() * 2);
                s.push_str("0x");
                for x in b {
                    s.push_str(&format!("{x:02X}"));
                }
                Value::String(s)
            }
            Cell::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())),
            Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => Value::String(s.clone()),
        }
    }
}

/// A column of the rows being moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferColumn {
    pub name: String,
    /// The engine's type, as its catalog spells it (`nvarchar(50)`, `int4`…).
    pub type_name: String,
    pub nullable: bool,
}

/// Rows in the order of the read's columns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RowBatch {
    pub rows: Vec<Vec<Cell>>,
    /// Estimated size (sum of [`Cell::size`]).
    #[serde(default)]
    pub bytes: usize,
}

impl RowBatch {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

/// What to read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadSpec {
    pub table: ObjectRef,
    /// Only these columns, in this order (`None`: all of them).
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// A condition in the driver's language (the rows to sync); `None`:
    /// every row. Drivers that can't filter answer `Unsupported`.
    #[serde(default)]
    pub filter: Option<String>,
}

/// Where and how to bulk load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSpec {
    pub table: ObjectRef,
    /// Target columns, in the order of the batches' cells.
    pub columns: Vec<String>,
    /// Lock the whole table while loading (SQL Server `TABLOCK`): faster,
    /// minimal logging where the engine allows it.
    #[serde(default)]
    pub table_lock: bool,
    /// Write the given values into identity / auto-increment columns.
    #[serde(default)]
    pub keep_identity: bool,
    /// Commit every this many rows…
    pub commit_rows: u64,
    /// …or this many bytes, whichever comes first.
    pub commit_bytes: u64,
}

impl LoadSpec {
    pub const DEFAULT_COMMIT_ROWS: u64 = 100_000;
    pub const DEFAULT_COMMIT_BYTES: u64 = 512 * 1024 * 1024;
}

/// A table copied inside one driver ([`Driver::copy_native`](crate::Driver::copy_native)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopySpec {
    pub source: ReadSpec,
    pub target: LoadSpec,
}

/// A same-engine clone ("clonar"): what makes the target identical to the
/// source beyond columns and key (partitions, storage, temporal tables,
/// every index option, constraints, code, descriptions…), from
/// [`Driver::clone_script`](crate::Driver::clone_script). Every statement
/// is idempotent (safe to run again on resume).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloneScript {
    /// Before any table: schemas, partition functions and schemes,
    /// filegroups, user types…
    pub before: Vec<String>,
    pub tables: Vec<CloneTable>,
    /// After all the data: foreign keys, checks, sequences, code objects
    /// in dependency order, descriptions, temporal versioning back on…
    pub after: Vec<String>,
    /// What couldn't be kept identical on this target, and what was done
    /// instead (Spanish, for the run's log).
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneTable {
    pub table: ObjectRef,
    /// The table with its columns, key and storage (no other index).
    pub create: String,
    /// Right before its data (e.g. versioning suspended).
    #[serde(default)]
    pub before_data: Vec<String>,
    /// Right after its data: its indexes, identity reseed…
    #[serde(default)]
    pub after_data: Vec<String>,
}

/// Sync by rows ("sincronizar solo lo que cambió"): rows are grouped by key
/// into buckets; each side sums its buckets and only the buckets that
/// differ are copied and merged. Source and target must be the same
/// engine (the row hashes have to match).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaSpec {
    pub table: ObjectRef,
    /// The key (primary key or a unique, not-null one).
    pub key: Vec<String>,
    /// Columns compared and copied, in order.
    pub columns: Vec<String>,
    pub buckets: Buckets,
    pub depth: DeltaDepth,
    /// Cores the engine may use for the summary (0: the server decides).
    #[serde(default)]
    pub max_cores: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Buckets {
    /// First key column is an integer: `width`-wide ranges from `lo`
    /// (rows outside `lo..=hi` fall in the edge buckets `-1` and `n`).
    Range { column: String, lo: i64, hi: i64, width: i64, n: u64 },
    /// A hash of the key modulo `n` (a prime).
    Hash { n: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaDepth {
    /// Every byte of every column.
    Full,
    /// Large columns by length only (reads no off-row pages).
    Sizes,
    /// Keys only: finds inserted and deleted rows, not updated ones.
    Keys,
}

/// One bucket's summary on one side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketSum {
    pub bucket: i64,
    pub rows: u64,
    /// Sum of the rows' hashes, as decimal text (it exceeds 64 bits).
    pub sum: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaResult {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    /// What the user should know about this sync (Spanish, for the run's
    /// log): a constraint left untrusted, a limitation…
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Buckets present on one side only, or whose count or sum differ.
pub fn changed_buckets(source: &[BucketSum], target: &[BucketSum]) -> Vec<i64> {
    use std::collections::BTreeMap;
    let a: BTreeMap<i64, &BucketSum> = source.iter().map(|b| (b.bucket, b)).collect();
    let b: BTreeMap<i64, &BucketSum> = target.iter().map(|b| (b.bucket, b)).collect();
    let mut keys: Vec<i64> = a.keys().chain(b.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter().filter(|k| a.get(k) != b.get(k)).collect()
}

/// Receives a read's batches. It may block while the consumer is behind:
/// that's the backpressure that bounds memory.
pub trait BatchSink: Send {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()>;
    fn batch(&mut self, batch: RowBatch) -> io::Result<()>;
}

pub type BatchSinkRef = Arc<Mutex<dyn BatchSink>>;

/// Gives a bulk load its batches, `None` at the end.
#[async_trait::async_trait]
pub trait BatchSource: Send {
    async fn next(&mut self) -> Option<RowBatch>;
}

/// Committed rows so far (called after each commit).
pub type Progress<'a> = &'a (dyn Fn(u64) + Send + Sync);

/// Groups rows into batches by [`CHUNK_ROWS`] / [`CHUNK_BYTES`] and hands
/// them to a sink.
pub struct BatchBuilder {
    batch: RowBatch,
    pub rows: u64,
}

impl Default for BatchBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchBuilder {
    pub fn new() -> Self {
        BatchBuilder { batch: RowBatch { rows: Vec::with_capacity(CHUNK_ROWS), bytes: 0 }, rows: 0 }
    }

    /// Add a row; a full batch goes to `sink`.
    pub fn push(&mut self, row: Vec<Cell>, sink: &mut dyn BatchSink) -> io::Result<()> {
        self.batch.bytes += row.iter().map(Cell::size).sum::<usize>();
        self.batch.rows.push(row);
        self.rows += 1;
        if self.batch.rows.len() >= CHUNK_ROWS || self.batch.bytes >= CHUNK_BYTES {
            self.flush(sink)?;
        }
        Ok(())
    }

    /// Hand over what's pending.
    pub fn flush(&mut self, sink: &mut dyn BatchSink) -> io::Result<()> {
        if self.batch.rows.is_empty() {
            return Ok(());
        }
        // The next batch isn't sized after one huge row.
        let next = RowBatch { rows: Vec::with_capacity(self.batch.rows.len().min(CHUNK_ROWS)), bytes: 0 };
        sink.batch(std::mem::replace(&mut self.batch, next))
    }
}

/// Grid rows of a run turned into batches (the default
/// [`Session::read_batches`](crate::Session::read_batches)). Only the run's
/// first result set is read.
pub struct JsonBatches {
    sink: BatchSinkRef,
    builder: BatchBuilder,
    /// Only these columns, in this order (by name, case-insensitive).
    wanted: Option<Vec<String>>,
    /// Positions in the result of the columns handed over.
    pick: Vec<usize>,
}

impl JsonBatches {
    pub fn new(sink: BatchSinkRef) -> Self {
        JsonBatches { sink, builder: BatchBuilder::new(), wanted: None, pick: Vec::new() }
    }

    /// Hand over only `columns`, in that order (a read's `ReadSpec::columns`).
    pub fn with_columns(mut self, columns: Option<Vec<String>>) -> Self {
        self.wanted = columns;
        self
    }

    /// Hand over the last batch; the rows read.
    pub fn finish(&mut self) -> crate::Result<u64> {
        let mut sink = self.sink.lock().map_err(|_| crate::Error::State("destino de lotes".into()))?;
        self.builder.flush(&mut *sink)?;
        Ok(self.builder.rows)
    }
}

impl crate::RowSink for JsonBatches {
    fn begin(&mut self, index: usize, columns: &[crate::ResultColumn]) -> io::Result<()> {
        if index != 0 {
            return Ok(());
        }
        self.pick = match &self.wanted {
            None => (0..columns.len()).collect(),
            Some(names) => names
                .iter()
                .map(|n| {
                    columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(n))
                        .ok_or_else(|| io::Error::other(format!("la lectura no trae la columna «{n}»")))
                })
                .collect::<io::Result<_>>()?,
        };
        let cols: Vec<TransferColumn> = self
            .pick
            .iter()
            .map(|&i| TransferColumn { name: columns[i].name.clone(), type_name: columns[i].type_name.clone(), nullable: true })
            .collect();
        self.sink.lock().map_err(|_| io::Error::other("destino de lotes"))?.begin(&cols)
    }

    fn row(&mut self, index: usize, row: &[Value]) -> io::Result<()> {
        if index != 0 {
            return Ok(());
        }
        let mut sink = self.sink.lock().map_err(|_| io::Error::other("destino de lotes"))?;
        let cells = self.pick.iter().map(|&i| row.get(i).map(Cell::from_json).unwrap_or(Cell::Null)).collect();
        self.builder.push(cells, &mut *sink)
    }
}

/// Read a table through `execute` + `browse_query`, turning its grid
/// values into cells: the default [`Session::read_batches`](crate::Session::read_batches),
/// and the fallback for drivers published before it existed.
pub async fn read_via_execute<S: crate::Session + ?Sized>(session: &mut S, spec: &ReadSpec, sink: BatchSinkRef) -> crate::Result<u64> {
    if spec.filter.is_some() {
        return Err(crate::Error::Unsupported("este motor no filtra la lectura por lotes".into()));
    }
    let query = session.browse_query(&spec.table, u32::MAX);
    let adapter = Arc::new(Mutex::new(JsonBatches::new(sink).with_columns(spec.columns.clone())));
    let mut out = crate::QueryOutcome { sink: Some(crate::RowSinkRef(adapter.clone())), ..Default::default() };
    let r = session.execute(&query, usize::MAX, &mut out).await;
    out.sink = None;
    let rows = adapter.lock().map_err(|_| crate::Error::State("lector de lotes".into()))?.finish()?;
    r?;
    if let Some(e) = out.sink_error.or(out.error) {
        return Err(crate::Error::Query(e));
    }
    Ok(rows)
}

/// `Vec<u8>` as MessagePack binary, not an array of numbers.
mod bytes {
    use serde::de::{Deserializer, SeqAccess, Visitor};
    use serde::Serializer;
    use std::fmt;

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(b) = seq.next_element::<u8>()? {
                    out.push(b);
                }
                Ok(out)
            }
        }
        d.deserialize_byte_buf(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Collect(Vec<RowBatch>);
    impl BatchSink for Collect {
        fn begin(&mut self, _: &[TransferColumn]) -> io::Result<()> {
            Ok(())
        }
        fn batch(&mut self, b: RowBatch) -> io::Result<()> {
            self.0.push(b);
            Ok(())
        }
    }

    #[test]
    fn batches_close_by_rows_or_bytes() {
        let mut sink = Collect(Vec::new());
        let mut b = BatchBuilder::new();
        for i in 0..2_500 {
            b.push(vec![Cell::Int(i)], &mut sink).unwrap();
        }
        b.flush(&mut sink).unwrap();
        assert_eq!(sink.0.iter().map(RowBatch::len).collect::<Vec<_>>(), vec![1000, 1000, 500]);

        // Wide rows: the byte bound closes the batch first.
        let mut sink = Collect(Vec::new());
        let mut b = BatchBuilder::new();
        for _ in 0..10 {
            b.push(vec![Cell::Bytes(vec![0; 1024 * 1024])], &mut sink).unwrap();
        }
        b.flush(&mut sink).unwrap();
        assert!(sink.0.iter().all(|x| x.len() <= 2));
        assert_eq!(sink.0.iter().map(RowBatch::len).sum::<usize>(), 10);
    }

    #[test]
    fn json_round_trip_keeps_binaries_whole() {
        let big = vec![0xABu8; 4096];
        let v = Cell::Bytes(big.clone()).to_json();
        let s = v.as_str().unwrap();
        assert_eq!(s.len(), 2 + 4096 * 2);
        assert!(!s.ends_with('…'));
        assert_eq!(Cell::from_json(&serde_json::json!(5)), Cell::Int(5));
        assert_eq!(Cell::from_json(&serde_json::json!(u64::MAX)), Cell::UInt(u64::MAX));
        assert_eq!(Cell::from_json(&serde_json::json!({"a": 1})), Cell::Json("{\"a\":1}".into()));
    }

    #[test]
    fn grid_reads_keep_only_the_requested_columns() {
        use crate::RowSink;
        let sink = Arc::new(Mutex::new(Collect(Vec::new())));
        let mut j = JsonBatches::new(sink.clone()).with_columns(Some(vec!["C".into(), "a".into()]));
        let col = |n: &str| crate::ResultColumn { name: n.into(), type_name: String::new() };
        j.begin(0, &[col("a"), col("b"), col("c")]).unwrap();
        j.row(0, &[serde_json::json!(1), serde_json::json!(2), serde_json::json!(3)]).unwrap();
        assert_eq!(j.finish().unwrap(), 1);
        assert_eq!(sink.lock().unwrap().0[0].rows, vec![vec![Cell::Int(3), Cell::Int(1)]]);
        // A column the read doesn't bring is an error, not a silent shift.
        let mut j = JsonBatches::new(sink).with_columns(Some(vec!["z".into()]));
        assert!(j.begin(0, &[col("a")]).is_err());
    }

    #[test]
    fn changed_buckets_are_the_differences() {
        let b = |bucket, rows, sum: &str| BucketSum { bucket, rows, sum: sum.into() };
        let src = vec![b(0, 10, "5"), b(1, 10, "6"), b(2, 3, "1")];
        let dst = vec![b(0, 10, "5"), b(1, 10, "7"), b(3, 1, "9")];
        assert_eq!(changed_buckets(&src, &dst), vec![1, 2, 3]);
    }

    #[test]
    fn batches_serialize_bytes_compactly() {
        let batch = RowBatch { rows: vec![vec![Cell::Bytes(vec![1, 2, 3]), Cell::Decimal("12.3400".into()), Cell::Null]], bytes: 0 };
        let json = serde_json::to_string(&batch).unwrap();
        let back: RowBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rows, batch.rows);
    }
}

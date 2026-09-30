//! Bulk transfer (see `dbine_driver::transfer`) over Arrow Flight SQL.
//!
//! Reading: one `SELECT` streamed as Arrow record batches, each column
//! turned straight into cells from its Arrow type (no JSON on the way):
//! integers, floats, decimals as exact digits (from the raw integer, so a
//! DuckDB `HUGEINT` keeps its 39 digits), dates, times and timestamps
//! formatted from their raw values (any year, `24:00:00`, `infinity`),
//! zoned timestamps with their offset (UTC for named zones: same instant),
//! binaries whole, UUIDs (the `arrow.uuid` extension) and nested values
//! (unions as `{"member": value}`) as JSON. DuckDB types whose Arrow form
//! isn't faithful (`UHUGEINT`, `BIT`, `BIGNUM`, `TIMETZ`, `INTERVAL`) are
//! read and loaded as their text, cast on the server (inside a nested type
//! they are refused). A value that can't be
//! represented is an error, never a null or an error text passed off as
//! data.
//!
//! Loading: the cells become Arrow arrays of the target table's own types
//! (nested columns from their JSON) and go up with `CommandStatementIngest`
//! (Flight SQL bulk ingest, the ADBC one): one `DoPut` stream per commit
//! window, appending to the existing table. Servers without ingest (or a
//! load into only some of the columns) get a prepared `INSERT` with small
//! slices of each batch bound as its parameter sets, which Flight SQL runs
//! once per row, on the server.
//!
//! Windows are atomic through Flight SQL transactions (`BeginTransaction`
//! / `EndTransaction`): a window that fails or is cancelled is rolled back,
//! and a call in flight is never dropped (dropping a `DoPut` ends its
//! stream as a normal finish, and the server keeps running what it got):
//! the load waits for it and only then rolls back, so nothing commits after
//! `bulk_load` returns. Servers without transactions commit each batch
//! (ingest) or each slice (prepared) by itself, reported as it lands.

use crate::{cells, flight_error, FlightSession};
use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float64Builder, Int64Builder, LargeListBuilder, ListBuilder, NullBuilder, StringBuilder, UInt64Builder,
};
use arrow_array::cast::AsArray;
use arrow_array::types::*;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, Date64Array, Decimal128Array, FixedSizeListArray, LargeListArray, ListArray, MapArray, RecordBatch, StructArray,
    Time32MillisecondArray, Time32SecondArray, Time64MicrosecondArray, Time64NanosecondArray, TimestampMicrosecondArray, TimestampMillisecondArray,
    TimestampNanosecondArray, TimestampSecondArray, UnionArray,
};
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_cast::{cast_with_options, CastOptions};
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::sql::{
    ActionClosePreparedStatementRequest, ActionCreatePreparedStatementRequest, ActionCreatePreparedStatementResult, ActionEndTransactionRequest, Any,
    CommandPreparedStatementUpdate, CommandStatementIngest, DoPutUpdateResult, EndTransaction, ProstMessageExt, TableDefinitionOptions, TableExistsOption,
    TableNotExistOption,
};
use arrow_flight::{Action, FlightDescriptor};
use arrow_schema::{DataType, Field, FieldRef, Schema, SchemaRef, TimeUnit, UnionMode};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{Error, Result};
use futures::{SinkExt, StreamExt};
use prost::bytes::Bytes;
use prost::Message;
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const EXTENSION: &str = "ARROW:extension:name";
const SQL_TYPE: &str = "ARROW:FLIGHT:SQL:TYPE_NAME";
/// Rows per prepared `INSERT` call: the server runs them one by one, and a
/// cancel waits for the call in flight (it is never dropped).
const PREPARED_ROWS: usize = 256;

/// How a window's rows go up.
enum Put {
    /// `CommandStatementIngest` into the table (all its columns, in order).
    Ingest { table: String, schema: Option<String> },
    /// A prepared `INSERT` of these columns.
    Prepared { query: String },
}

/// Where a window closes: rows or bytes, whichever comes first.
#[derive(Clone, Copy)]
struct Window {
    rows: u64,
    bytes: u64,
}

impl Window {
    fn full(&self, rows: u64, bytes: u64) -> bool {
        rows >= self.rows || bytes >= self.bytes
    }
}

fn batch_bytes(b: &RowBatch) -> u64 {
    if b.bytes > 0 {
        b.bytes as u64
    } else {
        b.rows.iter().flatten().map(|c| c.size() as u64).sum()
    }
}

/// The source's rows put in the table's column order (`order[k]`: the
/// source cell of the table's column `k`).
struct Reordered<'a> {
    source: &'a mut dyn BatchSource,
    order: Vec<usize>,
}

#[dbine_driver::async_trait]
impl BatchSource for Reordered<'_> {
    async fn next(&mut self) -> Option<RowBatch> {
        let mut b = self.source.next().await?;
        for row in &mut b.rows {
            // A row of another length stays as is: the conversion says so.
            if row.len() == self.order.len() {
                let mut cells: Vec<Option<Cell>> = std::mem::take(row).into_iter().map(Some).collect();
                *row = self.order.iter().map(|&i| cells[i].take().unwrap_or(Cell::Null)).collect();
            }
        }
        Some(b)
    }
}

fn is_nested(dt: &DataType) -> bool {
    matches!(dt, DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) | DataType::ListView(_) | DataType::LargeListView(_) | DataType::Struct(_) | DataType::Map(..) | DataType::Union(..))
}

fn is_hugeint(f: &Field) -> bool {
    f.metadata().get(SQL_TYPE).is_some_and(|t| t.eq_ignore_ascii_case("HUGEINT"))
}

/// GizmoSQL (DuckDB) binds a prepared statement's parameters one scalar at
/// a time through their text: a `HUGEINT` past 38 digits fails there.
fn check_duckdb_params(schema: &SchemaRef, b: &RowBatch) -> Result<()> {
    for (i, f) in schema.fields().iter().enumerate().filter(|(_, f)| is_hugeint(f)) {
        for row in &b.rows {
            let digits = row.get(i).and_then(text_of).map(|t| t.trim().trim_start_matches(['-', '+']).trim_start_matches('0').len()).unwrap_or(0);
            if digits > 38 {
                return Err(Error::Unsupported(format!(
                    "GizmoSQL (DuckDB) no acepta un HUGEINT de más de 38 dígitos como parámetro de un INSERT, y cargar solo algunas columnas de la tabla usa un INSERT preparado; cargá todas las columnas para usar la carga masiva (columna {})",
                    f.name()
                )));
            }
        }
    }
    Ok(())
}

/// DuckDB types its Arrow export doesn't carry faithfully: `UHUGEINT`
/// comes as the raw bits of a DECIMAL(38,0) (past i128::MAX it reads as a
/// negative number), `BIT` and `BIGNUM` as their internal bytes, and
/// `TIMETZ` as a TIME without its offset, and `INTERVAL` as a
/// MonthDayNano whose nanoseconds are its microseconds times 1000 (a time
/// part past about 292 years wraps around to another value). They travel
/// as their text, cast on the server both ways.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AsText {
    /// Read back as exact digits.
    Number,
    Text,
}

const DUCKDB_AS_TEXT: [&str; 7] = ["UHUGEINT", "BIGNUM", "VARINT", "BIT", "BITSTRING", "TIMETZ", "INTERVAL"];

fn duckdb_as_text(t: &str) -> Option<AsText> {
    match t.trim().to_ascii_uppercase().as_str() {
        "UHUGEINT" | "BIGNUM" | "VARINT" => Some(AsText::Number),
        "BIT" | "BITSTRING" | "TIMETZ" | "TIME WITH TIME ZONE" | "INTERVAL" => Some(AsText::Text),
        _ => None,
    }
}

/// One of those types inside a nested one (`UHUGEINT[]`, `STRUCT(a BIT)`):
/// a type name in type position (followed by the end, `)`, `,` or `[`), so
/// a field called `bit` or an enum label `'BIT'` doesn't count.
fn duckdb_nested_as_text(t: &str) -> bool {
    let u = t.to_ascii_uppercase();
    if duckdb_as_text(&u).is_some() {
        return false;
    }
    if u.contains("TIME WITH TIME ZONE") {
        return true;
    }
    let b = u.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i < b.len() {
        if !word(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && word(b[i]) {
            i += 1;
        }
        let quoted = start > 0 && matches!(b[start - 1], b'"' | b'\'');
        let next = u[i..].trim_start().as_bytes().first().copied();
        if !quoted && DUCKDB_AS_TEXT.contains(&&u[start..i]) && matches!(next, None | Some(b')' | b',' | b'[')) {
            return true;
        }
    }
    false
}

/// A column's declared type (by exact name, else ignoring case).
fn type_of<'a>(types: &'a [(String, String)], column: &str) -> Option<&'a str> {
    types.iter().find(|(n, _)| n == column).or_else(|| types.iter().find(|(n, _)| n.eq_ignore_ascii_case(column))).map(|(_, t)| t.as_str())
}

/// `load`: the refusal is for loading the column, else for reading it (two
/// whole sentences, so each one translates on its own).
fn nested_as_text_unsupported(column: &str, t: &str, load: bool) -> Error {
    Error::Unsupported(if load {
        format!(
            "GizmoSQL (DuckDB) no recibe por Arrow el valor exacto de UHUGEINT, BIT, BIGNUM, TIMETZ ni INTERVAL dentro de un tipo anidado: la columna {column} ({t}) no se puede cargar sin perder datos"
        )
    } else {
        format!(
            "GizmoSQL (DuckDB) no manda por Arrow el valor exacto de UHUGEINT, BIT, BIGNUM, TIMETZ ni INTERVAL dentro de un tipo anidado: la columna {column} ({t}) no se puede leer sin perder datos"
        )
    })
}

impl FlightSession {
    /// A DuckDB table's declared column types, from `duckdb_columns()` (or
    /// the `GetTables` type names when that isn't there: GizmoSQL's SQLite
    /// backend). Empty for other engines: their Arrow types are their own.
    async fn duckdb_types(&self, schema: Option<&str>, table: &str) -> Result<Vec<(String, String)>> {
        if self.server.engine() != crate::Engine::DuckDb {
            return Ok(Vec::new());
        }
        let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let db = self.catalog.as_deref().filter(|c| !c.is_empty()).map(lit).unwrap_or_else(|| "current_database()".into());
        let sch = schema.filter(|s| !s.is_empty()).map(lit).unwrap_or_else(|| "current_schema()".into());
        let q = format!(
            "SELECT column_name, data_type FROM duckdb_columns() WHERE database_name = {db} AND lower(schema_name) = lower({sch}) AND lower(table_name) = lower({}) ORDER BY column_index",
            lit(table)
        );
        match self.records(&q).await {
            Ok(rows) if !rows.is_empty() => {
                let get = |r: &serde_json::Map<String, Value>, k: &str| r.get(k).map(crate::text).unwrap_or_default();
                return Ok(rows.iter().map(|r| (get(r, "column_name"), get(r, "data_type"))).collect());
            }
            Ok(_) => {}
            Err(e @ (Error::Connect(_) | Error::AuthFailed(_) | Error::Cancelled)) => return Err(e),
            Err(e) => tracing::debug!("flightsql: sin duckdb_columns(), se usan los tipos de GetTables: {e}"),
        }
        let found = self.tables(schema, Some(table), true).await?.into_iter().find(|t| t.1 == table).and_then(|t| t.3);
        Ok(found.map(|s| s.fields().iter().filter_map(|f| f.metadata().get(SQL_TYPE).map(|t| (f.name().clone(), t.clone()))).collect()).unwrap_or_default())
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let types = self.duckdb_types(spec.table.schema(), &spec.table.name).await?;
        let wanted: Option<Vec<String>> = match &spec.columns {
            Some(c) if !c.is_empty() => Some(c.clone()),
            // Every column, listed when one of them needs a cast.
            _ if types.iter().any(|(_, t)| duckdb_as_text(t).is_some() || duckdb_nested_as_text(t)) => Some(types.iter().map(|(n, _)| n.clone()).collect()),
            _ => None,
        };
        // Per result column: read as text on the server, and its type.
        let mut as_text: Vec<Option<(AsText, String)>> = Vec::new();
        let list = match &wanted {
            Some(cols) => {
                let mut items = Vec::with_capacity(cols.len());
                for c in cols {
                    let q = quote_ident(Quote::Double, c);
                    let t = type_of(&types, c);
                    if let Some(t) = t.filter(|t| duckdb_nested_as_text(t)) {
                        return Err(nested_as_text_unsupported(c, t, false));
                    }
                    match t.and_then(|t| duckdb_as_text(t).map(|k| (k, t.to_string()))) {
                        Some(k) => {
                            items.push(format!("CAST({q} AS VARCHAR) AS {q}"));
                            as_text.push(Some(k));
                        }
                        None => {
                            items.push(q);
                            as_text.push(None);
                        }
                    }
                }
                items.join(", ")
            }
            None => "*".to_string(),
        };
        let columns_of = |schema: &Schema| {
            let mut cols = transfer_columns(schema);
            for (c, k) in cols.iter_mut().zip(&as_text) {
                if let Some((_, t)) = k {
                    c.type_name = t.clone();
                }
            }
            cols
        };
        let numbers = as_text.iter().any(|k| matches!(k, Some((AsText::Number, _))));
        let mut q = format!("SELECT {list} FROM {}", self.qualified(spec.table.schema(), &spec.table.name));
        if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            q.push_str(&format!(" WHERE {f}"));
        }
        let mut c = self.conn.client();
        let info = self.cancel.run(async { c.execute(q, None).await.map_err(flight_error) }).await?;
        let mut begun = false;
        if let Ok(schema) = info.clone().try_decode_schema() {
            sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&columns_of(&schema))?;
            begun = true;
        }
        let mut builder = BatchBuilder::new();
        self.stream(info, |b| {
            let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
            if !begun {
                s.begin(&columns_of(&b.schema()))?;
                begun = true;
            }
            for mut row in batch_rows(&b)? {
                if numbers {
                    for (c, k) in row.iter_mut().zip(&as_text) {
                        if let (Some((AsText::Number, _)), Cell::Text(t)) = (k, &mut *c) {
                            *c = Cell::Decimal(std::mem::take(t));
                        }
                    }
                }
                builder.push(row, &mut *s)?;
            }
            Ok(())
        })
        .await?;
        let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
        if !begun {
            s.begin(&[])?;
        }
        builder.flush(&mut *s)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(&mut self, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
        if self.server.read_only == Some(true) {
            return Err(Error::Query("El servidor Flight SQL es de solo lectura: no se pueden cargar datos.".into()));
        }
        let (schema, name) = (spec.table.schema().map(str::to_string), spec.table.name.clone());
        let found = self.tables(schema.as_deref(), Some(&name), true).await?.into_iter().find(|t| t.1 == name).and_then(|t| t.3);
        let target = match found {
            Some(s) => s,
            None => {
                // Without `GetTables` schemas: the table's first (empty) result.
                let q = format!("SELECT * FROM {} WHERE 1 = 0", self.qualified(schema.as_deref(), &name));
                let mut c = self.conn.client();
                let info = self.cancel.run(async { c.execute(q, None).await.map_err(flight_error) }).await?;
                info.try_decode_schema().map_err(flight_error)?
            }
        };
        let types = self.duckdb_types(schema.as_deref(), &name).await?;
        let mut fields = Vec::with_capacity(spec.columns.len());
        for c in &spec.columns {
            let f = target.fields().iter().find(|f| f.name() == c).or_else(|| target.fields().iter().find(|f| f.name().eq_ignore_ascii_case(c)));
            let Some(f) = f else { return Err(Error::Query(format!("La tabla {name} no tiene la columna {c}."))) };
            match type_of(&types, f.name()) {
                Some(t) if duckdb_nested_as_text(t) => return Err(nested_as_text_unsupported(c, t, true)),
                // Its text, cast by the server (see `AsText`).
                Some(t) if duckdb_as_text(t).is_some() => fields.push(Field::new(f.name(), DataType::Utf8, true)),
                _ => fields.push(f.as_ref().clone()),
            }
        }
        let batch_schema: SchemaRef = Arc::new(Schema::new(fields));
        let window = Window { rows: spec.commit_rows.max(1), bytes: spec.commit_bytes.max(1) };
        // Every column of the table, in any order: ingest, with the cells
        // put in the table's order.
        let order: Option<Vec<usize>> = (target.fields().len() == batch_schema.fields().len())
            .then(|| target.fields().iter().map(|t| batch_schema.fields().iter().position(|b| b.name() == t.name())).collect::<Option<Vec<_>>>())
            .flatten()
            .filter(|o| {
                let mut seen = o.clone();
                seen.sort_unstable();
                seen.dedup();
                seen.len() == o.len()
            });

        let transactions = self.transactions().await?;
        if let Some(order) = order {
            let table_schema: SchemaRef = Arc::new(Schema::new(order.iter().map(|&i| batch_schema.field(i).clone()).collect::<Vec<_>>()));
            if self.ingest_works(&table_schema, &name, schema.clone(), transactions).await? {
                let put = Put::Ingest { table: name, schema };
                if order.iter().enumerate().all(|(k, &i)| k == i) {
                    return self.put_windows(&put, &table_schema, source, window, transactions, progress).await;
                }
                let mut source = Reordered { source, order };
                return self.put_windows(&put, &table_schema, &mut source, window, transactions, progress).await;
            }
        }
        if self.server.engine() == crate::Engine::DuckDb {
            if let Some(f) = batch_schema.fields().iter().find(|f| is_nested(f.data_type())) {
                return Err(Error::Unsupported(format!(
                    "GizmoSQL (DuckDB) no acepta valores anidados (STRUCT, LIST, MAP, UNION) como parámetros de un INSERT, y cargar solo algunas columnas de la tabla usa un INSERT preparado; cargá todas las columnas para usar la carga masiva (columna {})",
                    f.name()
                )));
            }
        }
        // A prepared INSERT: parameters are positional, plain names and
        // nullable, with the column's own metadata (its engine type).
        let cols = spec.columns.iter().map(|c| quote_ident(Quote::Double, c)).collect::<Vec<_>>().join(", ");
        let marks = vec!["?"; spec.columns.len()].join(", ");
        let query = format!("INSERT INTO {} ({cols}) VALUES ({marks})", self.qualified(spec.table.schema(), &spec.table.name));
        let params: SchemaRef = Arc::new(Schema::new(
            batch_schema
                .fields()
                .iter()
                .enumerate()
                .map(|(i, f)| Field::new(format!("${}", i + 1), f.data_type().clone(), true).with_metadata(f.metadata().clone()))
                .collect::<Vec<_>>(),
        ));
        self.put_windows(&Put::Prepared { query }, &params, source, window, transactions, progress).await
    }

    fn ingest_command(&self, table: &str, schema: Option<String>, transaction_id: Option<Bytes>) -> CommandStatementIngest {
        CommandStatementIngest {
            table_definition_options: Some(TableDefinitionOptions { if_not_exist: TableNotExistOption::Fail as i32, if_exists: TableExistsOption::Append as i32 }),
            table: table.to_string(),
            schema,
            catalog: self.catalog.clone(),
            temporary: false,
            transaction_id,
            options: Default::default(),
        }
    }

    /// Does the server take `BeginTransaction`? (Tried and rolled back.)
    async fn transactions(&self) -> Result<bool> {
        match self.conn.client().begin_transaction().await.map_err(flight_error) {
            Ok(id) => {
                self.end_transaction(id, EndTransaction::Rollback).await?;
                Ok(true)
            }
            Err(e @ (Error::Connect(_) | Error::AuthFailed(_) | Error::Cancelled)) => Err(e),
            Err(e) => {
                tracing::debug!("flightsql: sin transacciones, cada lote se confirma solo: {e}");
                Ok(false)
            }
        }
    }

    async fn end_transaction(&self, id: Bytes, action: EndTransaction) -> Result<()> {
        let body = ActionEndTransactionRequest { transaction_id: id, action: action as i32 }.as_any().encode_to_vec();
        let mut s = self.conn.client().do_action(Action { r#type: "EndTransaction".into(), body: body.into() }).await.map_err(flight_error)?;
        while s.message().await.map_err(flight_error)?.is_some() {}
        Ok(())
    }

    /// An ingest of no rows (inside a transaction that is rolled back when
    /// the server has them): does the server take `CommandStatementIngest`?
    async fn ingest_works(&self, schema: &SchemaRef, table: &str, db_schema: Option<String>, transactions: bool) -> Result<bool> {
        let tx = if transactions { Some(self.conn.client().begin_transaction().await.map_err(flight_error)?) } else { None };
        let mut c = self.conn.client();
        let probe = futures::stream::iter(vec![Ok(RecordBatch::new_empty(schema.clone()))]);
        let r = c.execute_ingest(self.ingest_command(table, db_schema, tx.clone()), probe).await;
        if let Some(tx) = tx {
            self.end_transaction(tx, EndTransaction::Rollback).await?;
        }
        if let Err(e) = &r {
            tracing::debug!("flightsql: sin carga masiva (ingest), se usa INSERT preparado: {e}");
        }
        Ok(r.is_ok())
    }

    /// `CreatePreparedStatement` (in `transaction_id` when given): the
    /// handle (the client's prepared statement binds parameters as a query,
    /// which runs them only once).
    async fn prepare(&self, query: String, transaction_id: Option<Bytes>) -> Result<Bytes> {
        let action = Action {
            r#type: "CreatePreparedStatement".into(),
            body: ActionCreatePreparedStatementRequest { query, transaction_id }.as_any().encode_to_vec().into(),
        };
        let mut c = self.conn.client();
        let mut stream = c.do_action(action).await.map_err(flight_error)?;
        let msg = stream.message().await.map_err(flight_error)?.ok_or_else(|| Error::Query("el servidor no preparó el INSERT".into()))?;
        let any = Any::decode(&*msg.body).map_err(Error::query)?;
        let r: ActionCreatePreparedStatementResult =
            any.unpack().map_err(flight_error)?.ok_or_else(|| Error::Query("respuesta inesperada al preparar el INSERT".into()))?;
        while stream.message().await.map_err(flight_error)?.is_some() {}
        Ok(r.prepared_statement_handle)
    }

    async fn close_prepared(&self, handle: Bytes) {
        let close = Action {
            r#type: "ClosePreparedStatement".into(),
            body: ActionClosePreparedStatementRequest { prepared_statement_handle: handle }.as_any().encode_to_vec().into(),
        };
        if let Err(e) = self.conn.client().do_action(close).await {
            tracing::debug!("flightsql: no se cerró el INSERT preparado: {e}");
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.flag.load(Ordering::SeqCst)
    }

    /// The source's next batch, or `Cancelled`.
    async fn next_batch(&self, source: &mut dyn BatchSource) -> Result<Option<RowBatch>> {
        self.cancel.run(async { Ok(source.next().await) }).await
    }

    /// Commit windows: each one in its own transaction, committed when its
    /// last call succeeded and reported only then. Without transactions a
    /// window is a single call (a batch, or a prepared slice), converted
    /// whole before it goes up, so a value that doesn't convert never
    /// leaves part of it behind.
    async fn put_windows(&self, put: &Put, schema: &SchemaRef, source: &mut dyn BatchSource, window: Window, transactions: bool, progress: Progress<'_>) -> Result<u64> {
        // Without transactions any call fills the window.
        let window = if transactions { window } else { Window { rows: u64::MAX, bytes: 1 } };
        let mut done = 0u64;
        let mut next = self.next_batch(source).await?;
        while let Some(first) = next.take() {
            if self.cancelled() {
                return Err(Error::Cancelled);
            }
            let tx = if transactions { Some(self.conn.client().begin_transaction().await.map_err(flight_error)?) } else { None };
            let r = match put {
                Put::Ingest { table, schema: db_schema } => {
                    let cmd = self.ingest_command(table, db_schema.clone(), tx.clone()).as_any().encode_to_vec();
                    self.ingest_window(&cmd, schema, first, source, window).await.map(|n| (n, None))
                }
                Put::Prepared { query } => self.prepared_window(query, tx.clone(), schema, first, source, window).await,
            };
            // Cancelled while the window's last call was running: it
            // finished (it's never dropped), but the window doesn't commit.
            let r = match r {
                Ok(_) if tx.is_some() && self.cancelled() => Err(Error::Cancelled),
                r => r,
            };
            let (n, rest) = match (r, tx) {
                (Ok(r), Some(tx)) => {
                    self.end_transaction(tx, EndTransaction::Commit).await?;
                    r
                }
                (Ok(r), None) => r,
                (Err(e), Some(tx)) => {
                    if let Err(re) = self.end_transaction(tx, EndTransaction::Rollback).await {
                        tracing::warn!("flightsql: no se deshizo la ventana: {re}");
                    }
                    return Err(e);
                }
                (Err(e), None) if matches!(e, Error::Cancelled) => return Err(e),
                (Err(e), None) => {
                    let why = match put {
                        Put::Prepared { .. } => " El servidor no tiene transacciones y corre cada fila por separado: las filas de esa tanda anteriores al error pueden haber quedado guardadas.",
                        Put::Ingest { .. } => "",
                    };
                    return Err(match e {
                        Error::Query(m) if !why.is_empty() => Error::Query(format!("{m}{why}")),
                        e => e,
                    });
                }
            };
            done += n;
            progress(done);
            next = match rest {
                Some(b) => Some(b),
                None => self.next_batch(source).await?,
            };
        }
        Ok(done)
    }

    /// One `DoPut` stream of ingest: batches are converted and sent while
    /// the server appends them. A value that doesn't convert, or a cancel,
    /// stops the sending; the call always runs to its end (the caller rolls
    /// the window back).
    async fn ingest_window(&self, cmd: &[u8], schema: &SchemaRef, first: RowBatch, source: &mut dyn BatchSource, window: Window) -> Result<u64> {
        let (tx, rx) = futures::channel::mpsc::channel::<std::result::Result<RecordBatch, FlightError>>(2);
        let data = FlightDataEncoderBuilder::new()
            .with_schema(schema.clone())
            .with_flight_descriptor(Some(FlightDescriptor::new_cmd(cmd.to_vec())))
            .build(rx)
            .filter_map(|r| async move { r.inspect_err(|e| tracing::warn!("flightsql: lote sin codificar: {e}")).ok() });
        let mut client = self.conn.client();
        let call = async {
            let mut resp = client.do_put(data).await.map_err(flight_error)?;
            let msg = resp.message().await.map_err(flight_error)?;
            while resp.message().await.map_err(flight_error)?.is_some() {}
            Ok::<i64, Error>(msg.and_then(|m| DoPutUpdateResult::decode(&*m.app_metadata).ok()).map_or(-1, |r| r.record_count))
        };
        let pump = async move {
            let mut tx = tx;
            let (mut n, mut bytes) = (0u64, 0u64);
            let mut batch = Some(first);
            while let Some(b) = batch.take() {
                let rb = record_batch(schema, &b).map_err(Error::Query)?;
                if self.cancel.run(async { Ok(tx.send(Ok(rb)).await.is_ok()) }).await? {
                    n += b.len() as u64;
                    bytes += batch_bytes(&b);
                } else {
                    break; // the call ended: its result says why
                }
                if !window.full(n, bytes) {
                    batch = self.next_batch(source).await?;
                }
            }
            Ok::<u64, Error>(n)
            // `tx` dropped here: the stream ends and the call finishes.
        };
        let (sent, stored) = futures::join!(pump, call);
        let stored = stored?;
        let sent = sent?;
        if stored >= 0 && stored as u64 != sent {
            return Err(Error::Query(format!("el servidor guardó {stored} filas de {sent}")));
        }
        Ok(sent)
    }

    /// Prepared `INSERT` calls of at most [`PREPARED_ROWS`] rows each,
    /// awaited one by one (a cancel is seen between them). The window
    /// closes on its exact row count; the rows left of the last batch come
    /// back for the next window.
    async fn prepared_window(
        &self,
        query: &str,
        tx: Option<Bytes>,
        schema: &SchemaRef,
        first: RowBatch,
        source: &mut dyn BatchSource,
        window: Window,
    ) -> Result<(u64, Option<RowBatch>)> {
        let handle = self.prepare(query.to_string(), tx).await?;
        let cmd = CommandPreparedStatementUpdate { prepared_statement_handle: handle.clone() }.as_any().encode_to_vec();
        let mut pending = first.rows.into_iter();
        let r = async {
            let (mut n, mut bytes) = (0u64, 0u64);
            while !window.full(n, bytes) {
                if pending.len() == 0 {
                    match self.next_batch(source).await? {
                        Some(b) => pending = b.rows.into_iter(),
                        None => break,
                    }
                    continue;
                }
                let take = (window.rows - n).min(PREPARED_ROWS as u64) as usize;
                let b = RowBatch { rows: pending.by_ref().take(take).collect(), bytes: 0 };
                if self.server.engine() == crate::Engine::DuckDb {
                    check_duckdb_params(schema, &b)?;
                }
                let rb = record_batch(schema, &b).map_err(Error::Query)?;
                let stored = self.put_one(&cmd, schema, rb).await?;
                if stored >= 0 && stored as u64 != b.len() as u64 {
                    return Err(Error::Query(format!("el servidor guardó {stored} filas de {}", b.len())));
                }
                n += b.len() as u64;
                bytes += batch_bytes(&b);
                if self.cancelled() {
                    return Err(Error::Cancelled);
                }
            }
            Ok(n)
        }
        .await;
        self.close_prepared(handle).await;
        let rest = (pending.len() > 0).then(|| RowBatch { rows: pending.collect(), bytes: 0 });
        Ok((r?, rest))
    }

    /// One `DoPut` with one batch, run to its end.
    async fn put_one(&self, cmd: &[u8], schema: &SchemaRef, rb: RecordBatch) -> Result<i64> {
        let data = FlightDataEncoderBuilder::new()
            .with_schema(schema.clone())
            .with_flight_descriptor(Some(FlightDescriptor::new_cmd(cmd.to_vec())))
            .build(futures::stream::iter(vec![Ok(rb)]))
            .filter_map(|r| async move { r.inspect_err(|e| tracing::warn!("flightsql: lote sin codificar: {e}")).ok() });
        let mut resp = self.conn.client().do_put(data).await.map_err(flight_error)?;
        let msg = resp.message().await.map_err(flight_error)?;
        while resp.message().await.map_err(flight_error)?.is_some() {}
        Ok(msg.and_then(|m| DoPutUpdateResult::decode(&*m.app_metadata).ok()).map_or(-1, |r| r.record_count))
    }
}

// ---- Arrow → cells ----

fn transfer_columns(schema: &Schema) -> Vec<TransferColumn> {
    schema
        .fields()
        .iter()
        .map(|f| TransferColumn {
            name: f.name().clone(),
            type_name: f.metadata().get(SQL_TYPE).cloned().unwrap_or_else(|| f.data_type().to_string()),
            nullable: f.is_nullable(),
        })
        .collect()
}

/// A record batch as rows of cells.
pub(crate) fn batch_rows(b: &RecordBatch) -> Result<Vec<Vec<Cell>>> {
    let schema = b.schema();
    let mut rows: Vec<Vec<Cell>> = (0..b.num_rows()).map(|_| Vec::with_capacity(b.num_columns())).collect();
    for (f, a) in schema.fields().iter().zip(b.columns()) {
        // Dictionaries (and run-end encoded columns) by their values.
        let a: ArrayRef = match a.data_type() {
            DataType::Dictionary(_, v) => arrow_cast::cast(a, v).map_err(Error::query)?,
            DataType::RunEndEncoded(_, v) => arrow_cast::cast(a, v.data_type()).map_err(Error::query)?,
            _ => a.clone(),
        };
        let uuid = f.metadata().get(EXTENSION).is_some_and(|e| e == "arrow.uuid");
        for (r, row) in rows.iter_mut().enumerate() {
            row.push(to_cell(a.as_ref(), r, uuid).map_err(|e| Error::Query(format!("columna {}: {e}", f.name())))?);
        }
    }
    Ok(rows)
}

/// Arrow's own text, for the types without a mapping of their own; a value
/// it can't format is an error (not its error message as text).
fn display(a: &dyn Array, row: usize) -> std::result::Result<String, String> {
    let f = ArrayFormatter::try_new(a, &FormatOptions::default()).map_err(|e| e.to_string())?;
    f.value(row).try_to_string().map_err(|e| e.to_string())
}

fn uuid_text(b: &[u8]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

// Dates, times and timestamps are formatted from their raw values (proleptic
// Gregorian, any year), not through chrono: its range ends at ±262,143
// years, it has no `24:00:00`, and without `chrono-tz` it can't place a
// named zone.

const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DD`; years outside 0000–9999 signed (`-0043`, `+12000`), as
/// chrono and ISO 8601 write them. Year 0 is 1 BC.
fn date_text(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    if (0..=9999).contains(&y) {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        format!("{y:+05}-{m:02}-{d:02}")
    }
}

/// `HH:MM:SS[.fff|.ffffff|.fffffffff]` (up to `24:00:00`).
fn time_text(nanos: i64) -> String {
    let (s, f) = (nanos.div_euclid(1_000_000_000), nanos.rem_euclid(1_000_000_000));
    let frac = if f == 0 {
        String::new()
    } else if f % 1_000_000 == 0 {
        format!(".{:03}", f / 1_000_000)
    } else if f % 1000 == 0 {
        format!(".{:06}", f / 1000)
    } else {
        format!(".{f:09}")
    };
    format!("{:02}:{:02}:{:02}{frac}", s / 3600, s / 60 % 60, s % 60)
}

fn unit_nanos(unit: &TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => 1_000_000_000,
        TimeUnit::Millisecond => 1_000_000,
        TimeUnit::Microsecond => 1000,
        TimeUnit::Nanosecond => 1,
    }
}

/// `v` units since the epoch as `YYYY-MM-DD HH:MM:SS[.f]`.
fn timestamp_text(v: i64, unit: &TimeUnit) -> String {
    let per_day = NANOS_PER_DAY / unit_nanos(unit);
    format!("{} {}", date_text(v.div_euclid(per_day)), time_text(v.rem_euclid(per_day) * unit_nanos(unit)))
}

/// A zone's fixed offset in seconds: `+HH:MM`, `+HHMM`, `+HH`, `Z`, UTC.
/// Named zones (`America/Argentina/Buenos_Aires`) give `None`.
fn zone_offset(tz: &str) -> Option<i64> {
    let t = tz.trim();
    if matches!(t.to_ascii_uppercase().as_str(), "Z" | "UTC" | "GMT" | "ETC/UTC" | "ETC/GMT" | "ZULU" | "UNIVERSAL") {
        return Some(0);
    }
    parse_offset(t)
}

fn offset_text(secs: i64) -> String {
    let (sign, a) = if secs < 0 { ('-', -secs) } else { ('+', secs) };
    if a % 60 == 0 {
        format!("{sign}{:02}:{:02}", a / 3600, a / 60 % 60)
    } else {
        format!("{sign}{:02}:{:02}:{:02}", a / 3600, a / 60 % 60, a % 60)
    }
}

/// DuckDB's (and PostgreSQL's) infinite dates and timestamps: the type's
/// largest value and its negation (or its smallest).
fn infinite(v: i64, max: i64) -> Option<Cell> {
    if v == max {
        Some(Cell::Text("infinity".into()))
    } else if v == -max || v == -max - 1 {
        Some(Cell::Text("-infinity".into()))
    } else {
        None
    }
}

/// A timestamp: its wall-clock text, with the zone's offset when the type
/// has one (a named zone is written in UTC: the same instant).
fn timestamp_cell(v: i64, unit: &TimeUnit, tz: Option<&str>) -> Cell {
    if let Some(c) = infinite(v, i64::MAX) {
        return c;
    }
    let Some(tz) = tz else { return Cell::DateTime(timestamp_text(v, unit)) };
    let off = zone_offset(tz).unwrap_or(0);
    let local = i128::from(v) + i128::from(off) * i128::from(1_000_000_000 / unit_nanos(unit));
    match i64::try_from(local) {
        Ok(local) => Cell::DateTimeTz(format!("{}{}", timestamp_text(local, unit), offset_text(off))),
        Err(_) => Cell::DateTimeTz(format!("{}+00:00", timestamp_text(v, unit))),
    }
}

/// A decimal's exact digits from its unscaled integer text.
fn decimal_text(unscaled: String, scale: i8) -> String {
    let (sign, digits) = match unscaled.strip_prefix('-') {
        Some(d) => ("-", d.to_string()),
        None => ("", unscaled),
    };
    if scale <= 0 {
        return if digits == "0" { digits } else { format!("{sign}{digits}{}", "0".repeat(scale.unsigned_abs() as usize)) };
    }
    let scale = scale as usize;
    let digits = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale + 1 - digits.len())) } else { digits };
    let (int, frac) = digits.split_at(digits.len() - scale);
    format!("{sign}{int}.{frac}")
}

pub(crate) fn to_cell(a: &dyn Array, row: usize, uuid: bool) -> std::result::Result<Cell, String> {
    if a.is_null(row) {
        return Ok(Cell::Null);
    }
    Ok(match a.data_type() {
        DataType::Null => Cell::Null,
        DataType::Boolean => Cell::Bool(a.as_boolean().value(row)),
        DataType::Int8 => Cell::Int(a.as_primitive::<Int8Type>().value(row).into()),
        DataType::Int16 => Cell::Int(a.as_primitive::<Int16Type>().value(row).into()),
        DataType::Int32 => Cell::Int(a.as_primitive::<Int32Type>().value(row).into()),
        DataType::Int64 => Cell::Int(a.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => Cell::Int(a.as_primitive::<UInt8Type>().value(row).into()),
        DataType::UInt16 => Cell::Int(a.as_primitive::<UInt16Type>().value(row).into()),
        DataType::UInt32 => Cell::Int(a.as_primitive::<UInt32Type>().value(row).into()),
        DataType::UInt64 => Cell::UInt(a.as_primitive::<UInt64Type>().value(row)),
        DataType::Float16 => Cell::Float(a.as_primitive::<Float16Type>().value(row).to_f64()),
        // Through its shortest text, so 1.1f stays 1.1.
        DataType::Float32 => {
            let f = a.as_primitive::<Float32Type>().value(row);
            Cell::Float(f.to_string().parse().unwrap_or(f64::from(f)))
        }
        DataType::Float64 => Cell::Float(a.as_primitive::<Float64Type>().value(row)),
        // From the unscaled integer: Arrow's text stops at the type's
        // precision, and DuckDB sends `HUGEINT` (39 digits) as (38, 0).
        DataType::Decimal32(_, s) => Cell::Decimal(decimal_text(a.as_primitive::<Decimal32Type>().value(row).to_string(), *s)),
        DataType::Decimal64(_, s) => Cell::Decimal(decimal_text(a.as_primitive::<Decimal64Type>().value(row).to_string(), *s)),
        DataType::Decimal128(_, s) => Cell::Decimal(decimal_text(a.as_primitive::<Decimal128Type>().value(row).to_string(), *s)),
        DataType::Decimal256(_, s) => Cell::Decimal(decimal_text(a.as_primitive::<Decimal256Type>().value(row).to_string(), *s)),
        DataType::Utf8 => Cell::Text(a.as_string::<i32>().value(row).to_string()),
        DataType::LargeUtf8 => Cell::Text(a.as_string::<i64>().value(row).to_string()),
        DataType::Utf8View => Cell::Text(a.as_string_view().value(row).to_string()),
        DataType::FixedSizeBinary(16) if uuid => Cell::Uuid(uuid_text(a.as_fixed_size_binary().value(row))),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView | DataType::FixedSizeBinary(_) => {
            cells::binary_at(a, row).map(Cell::Bytes).ok_or_else(|| format!("binario sin leer ({})", a.data_type()))?
        }
        DataType::Date32 => {
            let v = a.as_primitive::<Date32Type>().value(row);
            infinite(v.into(), i32::MAX.into()).unwrap_or_else(|| Cell::Date(date_text(v.into())))
        }
        DataType::Date64 => {
            let v = a.as_primitive::<Date64Type>().value(row);
            infinite(v, i64::MAX).unwrap_or_else(|| Cell::Date(date_text(v.div_euclid(86_400_000))))
        }
        DataType::Time32(TimeUnit::Second) => Cell::Time(time_text(i64::from(a.as_primitive::<Time32SecondType>().value(row)) * 1_000_000_000)),
        DataType::Time32(_) => Cell::Time(time_text(i64::from(a.as_primitive::<Time32MillisecondType>().value(row)) * 1_000_000)),
        DataType::Time64(TimeUnit::Microsecond) => Cell::Time(time_text(a.as_primitive::<Time64MicrosecondType>().value(row).saturating_mul(1000))),
        DataType::Time64(_) => Cell::Time(time_text(a.as_primitive::<Time64NanosecondType>().value(row))),
        DataType::Timestamp(unit, tz) => {
            let v = match unit {
                TimeUnit::Second => a.as_primitive::<TimestampSecondType>().value(row),
                TimeUnit::Millisecond => a.as_primitive::<TimestampMillisecondType>().value(row),
                TimeUnit::Microsecond => a.as_primitive::<TimestampMicrosecondType>().value(row),
                TimeUnit::Nanosecond => a.as_primitive::<TimestampNanosecondType>().value(row),
            };
            timestamp_cell(v, unit, tz.as_deref())
        }
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) | DataType::ListView(_) | DataType::LargeListView(_) | DataType::Struct(_) | DataType::Map(..) => {
            Cell::Json(json(a, row)?.to_string())
        }
        // `{"member": value}`; a union has no nulls of its own: it is null
        // when its member's value is (DuckDB's NULL union).
        DataType::Union(fields, _) => {
            let u = a.as_union();
            let id = u.type_id(row);
            let (child, at) = (u.child(id), u.value_offset(row));
            if child.is_null(at) {
                Cell::Null
            } else {
                let name = fields.iter().find(|(i, _)| *i == id).map(|(_, f)| f.name().clone()).ok_or_else(|| format!("UNION sin el miembro {id}"))?;
                let mut o = serde_json::Map::new();
                o.insert(name, json(child.as_ref(), at)?);
                Cell::Json(Value::Object(o).to_string())
            }
        }
        _ => Cell::Text(display(a, row)?),
    })
}

/// A nested value as JSON (binaries whole, as `0x…`).
fn json(a: &dyn Array, row: usize) -> std::result::Result<Value, String> {
    if a.is_null(row) {
        return Ok(Value::Null);
    }
    let list = |values: ArrayRef| -> std::result::Result<Value, String> { Ok(Value::Array((0..values.len()).map(|i| json(values.as_ref(), i)).collect::<std::result::Result<_, _>>()?)) };
    Ok(match a.data_type() {
        DataType::List(_) => list(a.as_list::<i32>().value(row))?,
        DataType::LargeList(_) => list(a.as_list::<i64>().value(row))?,
        DataType::FixedSizeList(..) => list(a.as_fixed_size_list().value(row))?,
        DataType::Struct(fields) => {
            let s = a.as_struct();
            Value::Object(fields.iter().zip(s.columns()).map(|(f, c)| Ok((f.name().clone(), json(c.as_ref(), row)?))).collect::<std::result::Result<_, String>>()?)
        }
        DataType::Map(..) => {
            let m = a.as_map();
            let entries = m.value(row);
            let (keys, values) = (entries.column(0), entries.column(1));
            let ks: Vec<Value> = (0..entries.len()).map(|i| json(keys.as_ref(), i)).collect::<std::result::Result<_, _>>()?;
            if ks.iter().all(Value::is_string) {
                Value::Object(ks.into_iter().enumerate().map(|(i, k)| Ok((k.as_str().unwrap_or_default().to_string(), json(values.as_ref(), i)?))).collect::<std::result::Result<_, String>>()?)
            } else {
                Value::Array(ks.into_iter().enumerate().map(|(i, k)| Ok(Value::Array(vec![k, json(values.as_ref(), i)?]))).collect::<std::result::Result<_, String>>()?)
            }
        }
        _ => match to_cell(a, row, false)? {
            Cell::Int(i) => Value::from(i),
            Cell::UInt(u) => Value::from(u),
            Cell::Json(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
            c => c.to_json(),
        },
    })
}

// ---- cells → Arrow ----

/// A batch of rows as a record batch of `schema`'s types.
pub(crate) fn record_batch(schema: &SchemaRef, b: &RowBatch) -> std::result::Result<RecordBatch, String> {
    if let Some(bad) = b.rows.iter().find(|r| r.len() != schema.fields().len()) {
        return Err(format!("la fila tiene {} valores y la carga {} columnas", bad.len(), schema.fields().len()));
    }
    let mut arrays = Vec::with_capacity(schema.fields().len());
    for (i, f) in schema.fields().iter().enumerate() {
        let a = column(b.rows.iter().map(|r| &r[i]), b.len(), f).map_err(|e| format!("columna {}: {e}", f.name()))?;
        arrays.push(a);
    }
    RecordBatch::try_new(schema.clone(), arrays).map_err(|e| e.to_string())
}

fn text_of(c: &Cell) -> Option<String> {
    Some(match c {
        Cell::Null => return None,
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Bytes(b) => String::from_utf8(b.clone()).unwrap_or_else(|_| b.iter().map(|x| format!("{x:02X}")).collect()),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => s.clone(),
    })
}

fn int_of(c: &Cell) -> std::result::Result<Option<i64>, String> {
    Ok(match c {
        Cell::Null => None,
        Cell::Bool(b) => Some(*b as i64),
        Cell::Int(i) => Some(*i),
        Cell::UInt(u) => Some(i64::try_from(*u).map_err(|_| format!("{u} no entra en un entero de 64 bits"))?),
        Cell::Float(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => Some(*f as i64),
        other => {
            let t = text_of(other).unwrap_or_default();
            let t = t.trim();
            Some(t.parse::<i64>().or_else(|_| t.strip_suffix(".0").unwrap_or(t).parse()).map_err(|_| format!("«{t}» no es un entero"))?)
        }
    })
}

fn float_of(c: &Cell) -> std::result::Result<Option<f64>, String> {
    Ok(match c {
        Cell::Null => None,
        Cell::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Cell::Int(i) => Some(*i as f64),
        Cell::UInt(u) => Some(*u as f64),
        Cell::Float(f) => Some(*f),
        other => {
            let t = text_of(other).unwrap_or_default();
            Some(t.trim().parse().map_err(|_| format!("«{t}» no es un número"))?)
        }
    })
}

fn uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let h: String = s.chars().filter(|c| *c != '-').collect();
    if h.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(h.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// `0x…` hex (how binaries travel inside JSON).
fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if !h.len().is_multiple_of(2) {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok())).collect()
}

fn strict() -> CastOptions<'static> {
    CastOptions { safe: false, ..Default::default() }
}

// Temporal text → raw values. The forms read here and other engines give:
// `[±]Y…-MM-DD[( |T)HH:MM[:SS[.f]]][Z|±HH[:MM[:SS]]][ BC]`, `infinity`
// and `-infinity`.

struct Moment {
    days: i64,
    /// Nanoseconds into the day (`24:00:00` included).
    nanos: i64,
    /// Seconds east of UTC, when the text has an offset.
    offset: Option<i64>,
}

fn digits(s: &str, min: usize, max: usize) -> Option<(i64, &str)> {
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    if n < min || n > max {
        return None;
    }
    Some((s[..n].parse().ok()?, &s[n..]))
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `±HH[:MM[:SS]]` or `±HHMM` (the whole text).
fn parse_offset(s: &str) -> Option<i64> {
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let s = &s[1..];
    let (h, rest) = digits(s, 2, 2)?;
    let (m, rest) = match rest.strip_prefix(':') {
        Some(r) => digits(r, 2, 2)?,
        None if rest.is_empty() => (0, rest),
        None => digits(rest, 2, 2)?,
    };
    let (sec, rest) = match rest.strip_prefix(':') {
        Some(r) => digits(r, 2, 2)?,
        None => (0, rest),
    };
    (rest.is_empty() && h <= 18 && m < 60 && sec < 60).then_some(sign * (h * 3600 + m * 60 + sec))
}

/// `HH:MM[:SS[.f]]` into nanoseconds, and what follows it.
fn parse_clock(s: &str) -> Option<(i64, &str)> {
    let (h, rest) = digits(s, 1, 2)?;
    let (m, rest) = digits(rest.strip_prefix(':')?, 2, 2)?;
    let (sec, mut rest) = match rest.strip_prefix(':') {
        Some(r) => digits(r, 2, 2)?,
        None => (0, rest),
    };
    let mut frac = 0i64;
    if let Some(r) = rest.strip_prefix('.') {
        let n = r.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        // Beyond nanoseconds: dropped (no Arrow type holds them).
        let f = &r[..n.min(9)];
        frac = f.parse::<i64>().ok()? * 10i64.pow(9 - f.len() as u32);
        rest = &r[n..];
    }
    let nanos = ((h * 60 + m) * 60 + sec) * 1_000_000_000 + frac;
    (m < 60 && sec < 60 && nanos <= NANOS_PER_DAY).then_some((nanos, rest))
}

fn parse_moment(text: &str) -> std::result::Result<Moment, String> {
    let bad = || format!("«{text}» no es una fecha u hora válida");
    let mut s = text.trim();
    let mut bc = false;
    for era in [" BC", " bc", " AD", " ad"] {
        if let Some(r) = s.strip_suffix(era) {
            bc = era.trim().eq_ignore_ascii_case("bc");
            s = r.trim_end();
        }
    }
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (y, rest) = digits(s, 1, 9).ok_or_else(bad)?;
    let (m, rest) = digits(rest.strip_prefix('-').ok_or_else(bad)?, 1, 2).ok_or_else(bad)?;
    let (d, rest) = digits(rest.strip_prefix('-').ok_or_else(bad)?, 1, 2).ok_or_else(bad)?;
    let y = if neg { -y } else { y };
    let y = if bc { 1 - y } else { y };
    let (m, d) = (m as u32, d as u32);
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(bad());
    }
    let mut out = Moment { days: days_from_civil(y, m, d), nanos: 0, offset: None };
    let rest = match rest.strip_prefix(['T', 't', ' ']) {
        Some(r) => {
            let (nanos, r) = parse_clock(r.trim_start()).ok_or_else(bad)?;
            out.nanos = nanos;
            r
        }
        None => rest,
    };
    let rest = rest.trim();
    if !rest.is_empty() {
        out.offset = Some(if rest.eq_ignore_ascii_case("z") || rest.eq_ignore_ascii_case("utc") { 0 } else { parse_offset(rest).ok_or_else(bad)? });
    }
    Ok(out)
}

/// `infinity` / `-infinity` as DuckDB stores them (the type's largest
/// value and its negation; PostgreSQL's smallest is read back as well).
fn infinity(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "infinity" | "+infinity" => Some(true),
        "-infinity" => Some(false),
        _ => None,
    }
}

fn date_value(c: &Cell) -> std::result::Result<Option<i64>, String> {
    let Some(t) = text_of(c) else { return Ok(None) };
    if let Some(up) = infinity(&t) {
        return Ok(Some(if up { i32::MAX.into() } else { (-i32::MAX).into() }));
    }
    let m = parse_moment(&t)?;
    if m.nanos != 0 || m.offset.is_some_and(|o| o != 0) {
        return Err(format!("«{t}» tiene hora: no entra en una fecha"));
    }
    Ok(Some(m.days))
}

/// Units of `unit` since the epoch, UTC. Text without an offset is in
/// `zone` (the column's fixed offset, or UTC).
fn timestamp_value(c: &Cell, unit: &TimeUnit, zone: i64) -> std::result::Result<Option<i64>, String> {
    let Some(t) = text_of(c) else { return Ok(None) };
    if let Some(up) = infinity(&t) {
        return Ok(Some(if up { i64::MAX } else { -i64::MAX }));
    }
    let m = parse_moment(&t)?;
    let secs = i128::from(m.days) * 86_400 - i128::from(m.offset.unwrap_or(zone));
    let per = unit_nanos(unit);
    let v = (secs * 1_000_000_000 + i128::from(m.nanos)).div_euclid(i128::from(per));
    i64::try_from(v).ok().filter(|v| v.unsigned_abs() < i64::MAX as u64).map(Some).ok_or_else(|| format!("«{t}» está fuera del rango de la columna"))
}

fn time_value(c: &Cell) -> std::result::Result<Option<i64>, String> {
    let Some(t) = text_of(c) else { return Ok(None) };
    match parse_clock(t.trim()) {
        Some((n, "")) => Ok(Some(n)),
        _ => Err(format!("«{t}» no es una hora válida")),
    }
}

/// A decimal's text as its unscaled integer at `scale` (extra digits
/// rounded half away from zero, as SQL casts do), checked against
/// `precision` unless `any` (DuckDB's `HUGEINT` travels as (38, 0)).
fn decimal_value(text: &str, precision: u8, scale: i8, any: bool) -> std::result::Result<i128, String> {
    let bad = || format!("«{text}» no es un número decimal");
    let t = text.trim();
    let (neg, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let (mantissa, exp) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], t[i + 1..].parse::<i32>().map_err(|_| bad())?),
        None => (t, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() && frac.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let digits = format!("{int}{frac}");
    // value = digits × 10^(exp − len(frac)); unscaled = value × 10^scale.
    let shift = exp - frac.len() as i32 + i32::from(scale);
    let digits = digits.trim_start_matches('0');
    let too_big = || format!("«{text}» no entra en DECIMAL({precision},{scale})");
    // Accumulated negative: i128::MIN has no positive counterpart.
    let mut v: i128 = 0;
    let keep = if shift >= 0 { digits.len() } else { digits.len().saturating_sub(shift.unsigned_abs() as usize) };
    for b in digits[..keep].bytes() {
        v = v.checked_mul(10).and_then(|v| v.checked_sub(i128::from(b - b'0'))).ok_or_else(too_big)?;
    }
    if shift > 0 {
        for _ in 0..shift {
            v = v.checked_mul(10).ok_or_else(too_big)?;
        }
    } else if keep < digits.len() && digits.as_bytes()[keep] >= b'5' {
        v = v.checked_sub(1).ok_or_else(too_big)?;
    }
    if !any && precision < 39 && v.unsigned_abs() >= 10u128.pow(u32::from(precision)) {
        return Err(too_big());
    }
    if neg {
        Ok(v)
    } else {
        v.checked_neg().ok_or_else(too_big)
    }
}

/// One column of cells as an Arrow array of `field`'s type.
fn column<'a>(cells: impl Iterator<Item = &'a Cell>, len: usize, field: &Field) -> std::result::Result<ArrayRef, String> {
    let dt = field.data_type();
    let cast = |a: ArrayRef| -> std::result::Result<ArrayRef, String> {
        if a.data_type() == dt {
            Ok(a)
        } else {
            cast_with_options(&a, dt, &strict()).map_err(|e| e.to_string())
        }
    };
    match dt {
        DataType::Null => Ok(arrow_array::new_null_array(dt, len)),
        DataType::Boolean => {
            let mut b = BooleanBuilder::with_capacity(len);
            for c in cells {
                match c {
                    Cell::Null => b.append_null(),
                    Cell::Bool(v) => b.append_value(*v),
                    other => match text_of(other).unwrap_or_default().trim().to_ascii_lowercase().as_str() {
                        "1" | "true" | "t" | "yes" | "y" => b.append_value(true),
                        "0" | "false" | "f" | "no" | "n" => b.append_value(false),
                        t => return Err(format!("«{t}» no es un booleano")),
                    },
                }
            }
            Ok(Arc::new(b.finish()))
        }
        DataType::UInt64 => {
            let mut b = UInt64Builder::with_capacity(len);
            for c in cells {
                match c {
                    Cell::Null => b.append_null(),
                    Cell::UInt(u) => b.append_value(*u),
                    other => {
                        let t = text_of(other).unwrap_or_default();
                        b.append_value(t.trim().parse().map_err(|_| format!("«{t}» no es un entero sin signo"))?)
                    }
                }
            }
            Ok(Arc::new(b.finish()))
        }
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::UInt8 | DataType::UInt16 | DataType::UInt32 => {
            let mut b = Int64Builder::with_capacity(len);
            for c in cells {
                b.append_option(int_of(c)?);
            }
            cast(Arc::new(b.finish()))
        }
        DataType::Float16 | DataType::Float32 | DataType::Float64 => {
            let mut b = Float64Builder::with_capacity(len);
            for c in cells {
                b.append_option(float_of(c)?);
            }
            cast(Arc::new(b.finish()))
        }
        DataType::Decimal32(p, s) | DataType::Decimal64(p, s) | DataType::Decimal128(p, s) => {
            let any = is_hugeint(field);
            let values = cells.map(|c| text_of(c).map(|t| decimal_value(&t, *p, *s, any)).transpose()).collect::<std::result::Result<Vec<_>, _>>()?;
            // Unchecked: `HUGEINT` values past 38 digits are valid there.
            let a = Decimal128Array::from(values).with_data_type(DataType::Decimal128(*p, *s));
            cast(Arc::new(a))
        }
        DataType::Date32 => {
            let v = cells.map(|c| date_value(c)?.map(|d| i32::try_from(d).map_err(|_| format!("{d} días está fuera del rango de la columna"))).transpose()).collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(Arc::new(Date32Array::from(v)))
        }
        DataType::Date64 => {
            let v = cells
                .map(|c| {
                    Ok(match date_value(c)? {
                        Some(d) if d == i64::from(i32::MAX) => Some(i64::MAX),
                        Some(d) if d == -i64::from(i32::MAX) => Some(-i64::MAX),
                        d => d.map(|d| d * 86_400_000),
                    })
                })
                .collect::<std::result::Result<Vec<_>, String>>()?;
            Ok(Arc::new(Date64Array::from(v)))
        }
        DataType::Time32(unit) | DataType::Time64(unit) => {
            let per = unit_nanos(unit);
            let v = cells.map(|c| Ok(time_value(c)?.map(|n| n / per))).collect::<std::result::Result<Vec<_>, String>>()?;
            Ok(match dt {
                DataType::Time32(TimeUnit::Second) => Arc::new(Time32SecondArray::from(v.into_iter().map(|x| x.map(|x| x as i32)).collect::<Vec<_>>())),
                DataType::Time32(_) => Arc::new(Time32MillisecondArray::from(v.into_iter().map(|x| x.map(|x| x as i32)).collect::<Vec<_>>())),
                DataType::Time64(TimeUnit::Microsecond) => Arc::new(Time64MicrosecondArray::from(v)),
                _ => Arc::new(Time64NanosecondArray::from(v)),
            })
        }
        DataType::Timestamp(unit, tz) => {
            // Named zones only label the (UTC) values: nothing to convert.
            let zone = tz.as_deref().and_then(zone_offset).unwrap_or(0);
            let v = cells.map(|c| timestamp_value(c, unit, zone)).collect::<std::result::Result<Vec<_>, _>>()?;
            let tz = tz.clone();
            Ok(match unit {
                TimeUnit::Second => Arc::new(TimestampSecondArray::from(v).with_timezone_opt(tz)),
                TimeUnit::Millisecond => Arc::new(TimestampMillisecondArray::from(v).with_timezone_opt(tz)),
                TimeUnit::Microsecond => Arc::new(TimestampMicrosecondArray::from(v).with_timezone_opt(tz)),
                TimeUnit::Nanosecond => Arc::new(TimestampNanosecondArray::from(v).with_timezone_opt(tz)),
            })
        }
        DataType::FixedSizeBinary(16) => {
            let mut b = FixedSizeBinaryBuilder::with_capacity(len, 16);
            for c in cells {
                match c {
                    Cell::Null => b.append_null(),
                    Cell::Bytes(v) => b.append_value(v).map_err(|e| e.to_string())?,
                    other => {
                        let t = text_of(other).unwrap_or_default();
                        let v = uuid_bytes(&t).map(|u| u.to_vec()).or_else(|| hex_bytes(&t).filter(|h| h.len() == 16)).ok_or_else(|| format!("«{t}» no es un UUID"))?;
                        b.append_value(v).map_err(|e| e.to_string())?
                    }
                }
            }
            Ok(Arc::new(b.finish()))
        }
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView | DataType::FixedSizeBinary(_) => {
            let mut b = BinaryBuilder::with_capacity(len, len * 16);
            for c in cells {
                match c {
                    Cell::Null => b.append_null(),
                    Cell::Bytes(v) => b.append_value(v),
                    Cell::Uuid(u) => b.append_value(uuid_bytes(u).ok_or_else(|| format!("«{u}» no es un UUID"))?),
                    other => b.append_value(text_of(other).unwrap_or_default().as_bytes()),
                }
            }
            cast(Arc::new(b.finish()))
        }
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) | DataType::Struct(_) | DataType::Map(..) | DataType::Union(..) => {
            let values = cells
                .map(|c| match c {
                    Cell::Null => Ok(Value::Null),
                    Cell::Json(s) | Cell::Text(s) => serde_json::from_str(s).map_err(|_| format!("«{s}» no es JSON")),
                    other => Err(format!("«{}» no es JSON", text_of(other).unwrap_or_default())),
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            nested(&values, field)
        }
        _ => {
            // Text and the rest: Arrow parses the cells' exact text into
            // the target type (strict: a value that doesn't fit is an
            // error, not a null).
            let mut b = StringBuilder::with_capacity(len, len * 16);
            for c in cells {
                b.append_option(text_of(c));
            }
            cast(Arc::new(b.finish()))
        }
    }
}

/// A JSON leaf as the cell it stands for (binaries come as `0x…`).
fn leaf_cell(v: &Value, dt: &DataType) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => Cell::Int(i),
            (None, Some(u)) => Cell::UInt(u),
            _ => match dt {
                DataType::Float16 | DataType::Float32 | DataType::Float64 => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
                _ => Cell::Text(n.to_string()),
            },
        },
        Value::String(s) => match dt {
            DataType::Binary | DataType::LargeBinary | DataType::BinaryView | DataType::FixedSizeBinary(_) => hex_bytes(s).map(Cell::Bytes).unwrap_or_else(|| Cell::Text(s.clone())),
            _ => Cell::Text(s.clone()),
        },
        other => Cell::Json(other.to_string()),
    }
}

/// The offsets and nulls of lists of these lengths (`None`: a null list),
/// taken from a list of nulls (arrow-buffer isn't a dependency of ours).
fn shape(lens: &[Option<usize>]) -> ListArray {
    let mut b = ListBuilder::new(NullBuilder::new());
    for l in lens {
        b.values().append_nulls(l.unwrap_or(0));
        b.append(l.is_some());
    }
    b.finish()
}

fn large_shape(lens: &[Option<usize>]) -> LargeListArray {
    let mut b = LargeListBuilder::new(NullBuilder::new());
    for l in lens {
        b.values().append_nulls(l.unwrap_or(0));
        b.append(l.is_some());
    }
    b.finish()
}

/// Nested JSON values (null for a null value) as an array of `field`'s
/// nested type: structs from objects, lists from arrays, maps from objects
/// or `[key, value]` pairs.
fn nested(values: &[Value], field: &Field) -> std::result::Result<ArrayRef, String> {
    let valid = || BooleanArray::from(values.iter().map(|v| (!v.is_null()).then_some(true)).collect::<Vec<_>>());
    let items = |v: &Value| -> std::result::Result<Option<Vec<Value>>, String> {
        match v {
            Value::Null => Ok(None),
            Value::Array(a) => Ok(Some(a.clone())),
            other => Err(format!("«{other}» no es una lista")),
        }
    };
    match field.data_type() {
        DataType::Struct(fields) => {
            let mut children = Vec::with_capacity(fields.len());
            for f in fields.iter() {
                let child = values
                    .iter()
                    .map(|v| match v {
                        Value::Null => Ok(Value::Null),
                        Value::Object(o) => Ok(o.get(f.name()).cloned().unwrap_or(Value::Null)),
                        other => Err(format!("«{other}» no es un objeto")),
                    })
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                children.push(nested(&child, f)?);
            }
            let nulls = valid().nulls().cloned();
            Ok(Arc::new(StructArray::try_new(fields.clone(), children, nulls).map_err(|e| e.to_string())?))
        }
        DataType::List(f) | DataType::LargeList(f) => {
            let mut lens = Vec::with_capacity(values.len());
            let mut flat = Vec::new();
            for v in values {
                let it = items(v)?;
                lens.push(it.as_ref().map(Vec::len));
                flat.extend(it.unwrap_or_default());
            }
            let child = nested(&flat, f)?;
            Ok(match field.data_type() {
                DataType::List(_) => {
                    let (_, offsets, _, nulls) = shape(&lens).into_parts();
                    Arc::new(ListArray::try_new(f.clone(), offsets, child, nulls).map_err(|e| e.to_string())?)
                }
                _ => {
                    let (_, offsets, _, nulls) = large_shape(&lens).into_parts();
                    Arc::new(LargeListArray::try_new(f.clone(), offsets, child, nulls).map_err(|e| e.to_string())?)
                }
            })
        }
        DataType::FixedSizeList(f, n) => {
            let mut flat = Vec::new();
            for v in values {
                match items(v)? {
                    None => flat.extend(std::iter::repeat_n(Value::Null, *n as usize)),
                    Some(it) if it.len() == *n as usize => flat.extend(it),
                    Some(_) => return Err(format!("«{v}» no tiene {n} elementos")),
                }
            }
            let child = nested(&flat, f)?;
            let nulls = valid().nulls().cloned();
            Ok(Arc::new(FixedSizeListArray::try_new(f.clone(), *n, child, nulls).map_err(|e| e.to_string())?))
        }
        DataType::Map(entries, sorted) => {
            let DataType::Struct(kv) = entries.data_type() else { return Err(format!("mapa sin entradas: {}", field.data_type())) };
            if kv.len() != 2 {
                return Err(format!("mapa sin clave y valor: {}", field.data_type()));
            }
            let (mut keys, mut vals, mut lens) = (Vec::new(), Vec::new(), Vec::with_capacity(values.len()));
            for v in values {
                match v {
                    Value::Null => lens.push(None),
                    Value::Object(o) => {
                        lens.push(Some(o.len()));
                        for (k, x) in o {
                            keys.push(Value::String(k.clone()));
                            vals.push(x.clone());
                        }
                    }
                    Value::Array(pairs) => {
                        lens.push(Some(pairs.len()));
                        for p in pairs {
                            match p.as_array().map(Vec::as_slice) {
                                Some([k, x]) => {
                                    keys.push(k.clone());
                                    vals.push(x.clone());
                                }
                                _ => return Err(format!("«{p}» no es un par [clave, valor]")),
                            }
                        }
                    }
                    other => return Err(format!("«{other}» no es un mapa")),
                }
            }
            let children: Vec<ArrayRef> = vec![nested(&keys, &kv[0])?, nested(&vals, &kv[1])?];
            let entries_array = StructArray::try_new(kv.clone(), children, None).map_err(|e| e.to_string())?;
            let (_, offsets, _, nulls) = shape(&lens).into_parts();
            let f: FieldRef = entries.clone();
            Ok(Arc::new(MapArray::try_new(f, offsets, entries_array, nulls, *sorted).map_err(|e| e.to_string())?))
        }
        // From `{"member": value}` (as read); a null is the first member's
        // null, as DuckDB writes a NULL union.
        DataType::Union(fields, mode) => {
            let ids: Vec<i8> = fields.iter().map(|(i, _)| i).collect();
            if ids.is_empty() {
                return Err(format!("UNION sin miembros: {}", field.data_type()));
            }
            let names = || fields.iter().map(|(_, f)| f.name().as_str()).collect::<Vec<_>>().join(", ");
            let sparse = *mode == UnionMode::Sparse;
            let mut children: Vec<Vec<Value>> = vec![if sparse { vec![Value::Null; values.len()] } else { Vec::new() }; ids.len()];
            let (mut type_ids, mut offsets) = (Vec::with_capacity(values.len()), Vec::with_capacity(values.len()));
            for (row, v) in values.iter().enumerate() {
                let (k, x) = match v {
                    Value::Null => (0, Value::Null),
                    Value::Object(o) if o.len() == 1 => {
                        let (key, x) = o.iter().next().ok_or_else(|| format!("«{v}» no es un valor de UNION"))?;
                        let k = fields.iter().position(|(_, f)| f.name() == key).ok_or_else(|| format!("«{v}»: el UNION no tiene el miembro {key} (tiene {})", names()))?;
                        (k, x.clone())
                    }
                    other => return Err(format!("«{other}» no es un valor de UNION: va un objeto con uno de sus miembros ({})", names())),
                };
                type_ids.push(ids[k]);
                if sparse {
                    children[k][row] = x;
                } else {
                    offsets.push(i32::try_from(children[k].len()).map_err(|e| e.to_string())?);
                    children[k].push(x);
                }
            }
            let arrays = fields.iter().zip(children).map(|((_, f), vals)| nested(&vals, f)).collect::<std::result::Result<Vec<ArrayRef>, String>>()?;
            let offsets = (!sparse).then(|| offsets.into());
            Ok(Arc::new(UnionArray::try_new(fields.clone(), type_ids.into(), offsets, arrays).map_err(|e| e.to_string())?))
        }
        dt => {
            let cells: Vec<Cell> = values.iter().map(|v| leaf_cell(v, dt)).collect();
            column(cells.iter(), cells.len(), field)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{BinaryArray, FixedSizeBinaryArray, Float32Array, Int64Array, StringArray};
    use std::collections::HashMap;

    #[test]
    fn arrow_to_cells() {
        let uuid_field = Field::new("u", DataType::FixedSizeBinary(16), true).with_metadata(HashMap::from([(EXTENSION.to_string(), "arrow.uuid".to_string())]));
        let schema = Arc::new(Schema::new(vec![
            Field::new("i", DataType::Int64, true),
            Field::new("f", DataType::Float32, true),
            Field::new("d", DataType::Decimal128(10, 2), true),
            Field::new("s", DataType::Utf8, true),
            Field::new("b", DataType::Binary, true),
            Field::new("dt", DataType::Date32, true),
            Field::new("t", DataType::Time64(TimeUnit::Microsecond), true),
            Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
            Field::new("tz", DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into())), true),
            uuid_field,
            Field::new("l", DataType::List(Arc::new(Field::new("item", DataType::Int64, true))), true),
        ]));
        let big = vec![7u8; 5000];
        let b = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![Some(9007199254740993), None])),
                Arc::new(Float32Array::from(vec![Some(1.1), None])),
                Arc::new(Decimal128Array::from(vec![Some(-12345), None]).with_precision_and_scale(10, 2).unwrap()),
                Arc::new(StringArray::from(vec![Some("ñ"), None])),
                Arc::new(BinaryArray::from(vec![Some(&big[..]), None])),
                Arc::new(Date32Array::from(vec![Some(19753), None])),
                Arc::new(Time64MicrosecondArray::from(vec![Some(49_500_123_456), None])),
                Arc::new(TimestampMicrosecondArray::from(vec![Some(1_706_708_700_123_000), None])),
                Arc::new(TimestampMicrosecondArray::from(vec![Some(1_706_708_700_000_000), None]).with_timezone("+00:00")),
                Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(vec![Some(vec![0x12u8; 16]), None].into_iter(), 16).unwrap()),
                Arc::new(ListArray::from_iter_primitive::<Int64Type, _, _>(vec![Some(vec![Some(1), None]), None])),
            ],
        )
        .unwrap();
        let rows = batch_rows(&b).unwrap();
        assert_eq!(
            rows[0],
            vec![
                Cell::Int(9007199254740993),
                Cell::Float(1.1),
                Cell::Decimal("-123.45".into()),
                Cell::Text("ñ".into()),
                Cell::Bytes(big.clone()),
                Cell::Date("2024-01-31".into()),
                Cell::Time("13:45:00.123456".into()),
                Cell::DateTime("2024-01-31 13:45:00.123".into()),
                Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into()),
                Cell::Uuid("12121212-1212-1212-1212-121212121212".into()),
                Cell::Json("[1,null]".into()),
            ]
        );
        assert!(rows[1].iter().all(|c| *c == Cell::Null));
    }

    #[test]
    fn cells_to_arrow_round_trip() {
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("i", DataType::Int32, true),
            Field::new("d", DataType::Decimal128(38, 4), true),
            Field::new("s", DataType::Utf8, true),
            Field::new("b", DataType::Binary, true),
            Field::new("dt", DataType::Date32, true),
            Field::new("t", DataType::Time64(TimeUnit::Microsecond), true),
            Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
            Field::new("tz", DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into())), true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("f", DataType::Float64, true),
        ]));
        let row = vec![
            Cell::Int(42),
            Cell::Decimal("12345678901234567890.1234".into()),
            Cell::Text("O'Brien".into()),
            Cell::Bytes(vec![0, 255, 7]),
            Cell::Date("2024-01-31".into()),
            Cell::Time("13:45:00.5".into()),
            Cell::DateTime("2024-01-31 13:45:00.123456".into()),
            Cell::DateTimeTz("2024-01-31 10:45:00-03:00".into()),
            Cell::Bool(true),
            Cell::Float(0.1),
        ];
        let batch = RowBatch { rows: vec![row, vec![Cell::Null; 10]], bytes: 0 };
        let rb = record_batch(&schema, &batch).unwrap();
        let back = batch_rows(&rb).unwrap();
        assert_eq!(
            back[0],
            vec![
                Cell::Int(42),
                Cell::Decimal("12345678901234567890.1234".into()),
                Cell::Text("O'Brien".into()),
                Cell::Bytes(vec![0, 255, 7]),
                Cell::Date("2024-01-31".into()),
                Cell::Time("13:45:00.500".into()),
                Cell::DateTime("2024-01-31 13:45:00.123456".into()),
                Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into()),
                Cell::Bool(true),
                Cell::Float(0.1),
            ]
        );
        assert!(back[1].iter().all(|c| *c == Cell::Null));

        // Strict: overflow is an error, not a null.
        let small: SchemaRef = Arc::new(Schema::new(vec![Field::new("i", DataType::Int8, true)]));
        assert!(record_batch(&small, &RowBatch { rows: vec![vec![Cell::Int(300)]], bytes: 0 }).is_err());
        assert_eq!(uuid_bytes("12121212-1212-1212-1212-121212121212"), Some([0x12; 16]));
    }

    fn one(f: Field, a: ArrayRef) -> Vec<Vec<Cell>> {
        batch_rows(&RecordBatch::try_new(Arc::new(Schema::new(vec![f])), vec![a]).unwrap()).unwrap()
    }

    /// Cells into a one-column batch of `f` and back.
    fn round_trip(f: Field, cells: Vec<Cell>) -> std::result::Result<Vec<Cell>, String> {
        let schema: SchemaRef = Arc::new(Schema::new(vec![f]));
        let rb = record_batch(&schema, &RowBatch { rows: cells.into_iter().map(|c| vec![c]).collect(), bytes: 0 })?;
        Ok(batch_rows(&rb).map_err(|e| e.to_string())?.into_iter().map(|mut r| r.remove(0)).collect())
    }

    #[test]
    fn named_zones_read_and_load() {
        // DuckDB's TIMESTAMPTZ: µs with a named zone (no chrono-tz here).
        for zone in ["Etc/UTC", "America/Argentina/Buenos_Aires"] {
            let f = Field::new("tz", DataType::Timestamp(TimeUnit::Microsecond, Some(zone.into())), true);
            let a = TimestampMicrosecondArray::from(vec![Some(-62_135_596_800_000_000), Some(1_717_254_000_123_456), None]).with_timezone(zone);
            let rows = one(f.clone(), Arc::new(a));
            assert_eq!(rows[0][0], Cell::DateTimeTz("0001-01-01 00:00:00+00:00".into()), "{zone}");
            assert_eq!(rows[1][0], Cell::DateTimeTz("2024-06-01 15:00:00.123456+00:00".into()), "{zone}");
            assert_eq!(rows[2][0], Cell::Null);
            // …and loads back into the same type, NULLs included.
            let back = round_trip(f, vec![Cell::DateTimeTz("2024-06-01 12:00:00.123456-03:00".into()), Cell::Null, Cell::DateTime("0001-01-01 00:00:00".into())]).unwrap();
            assert_eq!(back, vec![rows[1][0].clone(), Cell::Null, rows[0][0].clone()]);
        }
        // A fixed offset keeps its wall clock.
        let f = Field::new("tz", DataType::Timestamp(TimeUnit::Second, Some("-03:00".into())), true);
        let back = round_trip(f, vec![Cell::DateTimeTz("2024-06-01 12:00:00-03:00".into()), Cell::DateTime("2024-06-01 12:00:00".into())]).unwrap();
        assert_eq!(back, vec![Cell::DateTimeTz("2024-06-01 12:00:00-03:00".into()); 2]);
    }

    #[test]
    fn hugeint_keeps_39_digits() {
        let f = Field::new("h", DataType::Decimal128(38, 0), true).with_metadata(HashMap::from([(SQL_TYPE.to_string(), "HUGEINT".to_string())]));
        let a = Decimal128Array::from(vec![i128::MIN, i128::MAX, -5]).with_precision_and_scale(38, 0).unwrap();
        let rows = one(f.clone(), Arc::new(a));
        let want = ["-170141183460469231731687303715884105728", "170141183460469231731687303715884105727", "-5"];
        assert_eq!(rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), want.iter().map(|w| Cell::Decimal(w.to_string())).collect::<Vec<_>>());
        let back = round_trip(f, want.iter().map(|w| Cell::Decimal(w.to_string())).collect()).unwrap();
        assert_eq!(back, want.iter().map(|w| Cell::Decimal(w.to_string())).collect::<Vec<_>>());
        // A true DECIMAL(38,0) (or smaller) still checks its precision.
        let plain = Field::new("d", DataType::Decimal128(38, 0), true);
        assert!(round_trip(plain, vec![Cell::Decimal(want[1].into())]).is_err());
        let small = Field::new("d", DataType::Decimal128(5, 2), true);
        assert!(round_trip(small.clone(), vec![Cell::Decimal("1234.5".into())]).is_err());
        assert_eq!(round_trip(small, vec![Cell::Decimal("-0.005".into()), Cell::Float(12.5), Cell::Text("1e2".into())]).unwrap(), vec![
            Cell::Decimal("-0.01".into()),
            Cell::Decimal("12.50".into()),
            Cell::Decimal("100.00".into())
        ]);
        assert_eq!(decimal_text("5".into(), 3), "0.005");
        assert_eq!(decimal_text("-12".into(), -2), "-1200");
    }

    #[test]
    fn values_chrono_cannot_hold() {
        // TIME '24:00:00', DATE / TIMESTAMP ±infinity: values, not error text.
        let t = one(Field::new("t", DataType::Time64(TimeUnit::Microsecond), true), Arc::new(Time64MicrosecondArray::from(vec![86_400_000_000])));
        assert_eq!(t[0][0], Cell::Time("24:00:00".into()));
        let d = one(Field::new("d", DataType::Date32, true), Arc::new(Date32Array::from(vec![i32::MAX, -i32::MAX, i32::MIN])));
        assert_eq!(d.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), vec![Cell::Text("infinity".into()), Cell::Text("-infinity".into()), Cell::Text("-infinity".into())]);
        let ts = one(Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true), Arc::new(TimestampMicrosecondArray::from(vec![i64::MAX, -i64::MAX])));
        assert_eq!(ts.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), vec![Cell::Text("infinity".into()), Cell::Text("-infinity".into())]);
        // They load back as DuckDB stores them.
        let f = Field::new("t", DataType::Time64(TimeUnit::Microsecond), true);
        assert_eq!(round_trip(f.clone(), vec![Cell::Time("24:00:00".into())]).unwrap(), vec![Cell::Time("24:00:00".into())]);
        assert!(round_trip(f, vec![Cell::Time("24:00:01".into())]).is_err());
        let f = Field::new("d", DataType::Date32, true);
        assert_eq!(round_trip(f, vec![Cell::Text("infinity".into()), Cell::Text("-infinity".into())]).unwrap(), vec![Cell::Text("infinity".into()), Cell::Text("-infinity".into())]);
        let f = Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true);
        assert_eq!(round_trip(f, vec![Cell::Text("-infinity".into())]).unwrap(), vec![Cell::Text("-infinity".into())]);
    }

    #[test]
    fn years_outside_0000_9999() {
        // '0044-03-15 (BC) 12:00:00' and year 12000, read and loaded back.
        let f = Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true);
        let bc = days_from_civil(-43, 3, 15) * 86_400_000_000 + 12 * 3_600_000_000;
        let far = days_from_civil(12000, 1, 1) * 86_400_000_000;
        let rows = one(f.clone(), Arc::new(TimestampMicrosecondArray::from(vec![bc, far])));
        let cells: Vec<Cell> = rows.iter().map(|r| r[0].clone()).collect();
        assert_eq!(cells, vec![Cell::DateTime("-0043-03-15 12:00:00".into()), Cell::DateTime("+12000-01-01 00:00:00".into())]);
        assert_eq!(round_trip(f.clone(), cells.clone()).unwrap(), cells);
        // PostgreSQL's spelling of the same instant.
        assert_eq!(round_trip(f, vec![Cell::DateTime("0044-03-15 12:00:00 BC".into())]).unwrap(), vec![cells[0].clone()]);
        let f = Field::new("d", DataType::Date32, true);
        let dates = vec![Cell::Date("-0043-03-15".into()), Cell::Date("+12000-01-01".into()), Cell::Date("2024-02-29".into())];
        assert_eq!(round_trip(f.clone(), dates.clone()).unwrap(), dates);
        assert!(round_trip(f, vec![Cell::Date("2023-02-29".into())]).is_err());
        for d in [-800_000, -1, 0, 59, 19_753, 3_000_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }

    #[test]
    fn nested_values_load_back() {
        let st = Field::new("st", DataType::Struct(vec![Field::new("a", DataType::Int32, true), Field::new("b", DataType::Utf8, true)].into()), true);
        let li = Field::new("l", DataType::List(Arc::new(Field::new("l", DataType::Int32, true))), true);
        let entries = Field::new("entries", DataType::Struct(vec![Field::new("key", DataType::Utf8, false), Field::new("value", DataType::Int32, true)].into()), false);
        let map = Field::new("m", DataType::Map(Arc::new(entries), false), true);
        let bin = Field::new("lb", DataType::List(Arc::new(Field::new("x", DataType::Binary, true))), true);
        let schema: SchemaRef = Arc::new(Schema::new(vec![st, li, map, bin]));
        let rows = vec![
            vec![Cell::Json(r#"{"a":1,"b":"x"}"#.into()), Cell::Json("[1,null,3]".into()), Cell::Json(r#"{"k":1}"#.into()), Cell::Json(r#"["0x00FF",null]"#.into())],
            vec![Cell::Null, Cell::Null, Cell::Null, Cell::Null],
            vec![Cell::Json(r#"{"a":null,"b":null}"#.into()), Cell::Json("[]".into()), Cell::Json("{}".into()), Cell::Json("[]".into())],
        ];
        let rb = record_batch(&schema, &RowBatch { rows: rows.clone(), bytes: 0 }).unwrap();
        assert_eq!(batch_rows(&rb).unwrap(), rows);
        let bad = RowBatch { rows: vec![vec![Cell::Json("[1]".into()), Cell::Null, Cell::Null, Cell::Null]], bytes: 0 };
        assert!(record_batch(&schema, &bad).is_err());
    }

    #[test]
    fn unions_as_json() {
        use arrow_schema::UnionFields;
        let uf = UnionFields::try_new(vec![0i8, 1], vec![Field::new("n", DataType::Int32, true), Field::new("s", DataType::Utf8, true)]).unwrap();
        // DuckDB: sparse, and a NULL union is the first member's null.
        let children = || -> Vec<ArrayRef> { vec![Arc::new(arrow_array::Int32Array::from(vec![None, Some(7), None])), Arc::new(StringArray::from(vec![Some("hola"), None, None]))] };
        let a = UnionArray::try_new(uf.clone(), vec![1i8, 0, 0].into(), None, children()).unwrap();
        let f = Field::new("u", DataType::Union(uf.clone(), UnionMode::Sparse), true);
        let rows = one(f.clone(), Arc::new(a));
        let want = vec![Cell::Json(r#"{"s":"hola"}"#.into()), Cell::Json(r#"{"n":7}"#.into()), Cell::Null];
        assert_eq!(rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), want);
        assert_eq!(round_trip(f, want.clone()).unwrap(), want);
        let dense = Field::new("u", DataType::Union(uf.clone(), UnionMode::Dense), true);
        assert_eq!(round_trip(dense.clone(), want.clone()).unwrap(), want);
        // Inside a list, and what isn't one of its members.
        let li = Field::new("l", DataType::List(Arc::new(Field::new("x", DataType::Union(uf, UnionMode::Sparse), true))), true);
        let l = vec![Cell::Json(r#"[{"n":1},null,{"s":"x"}]"#.into())];
        assert_eq!(round_trip(li, l.clone()).unwrap(), l);
        assert!(round_trip(dense.clone(), vec![Cell::Json(r#"{"z":1}"#.into())]).is_err());
        assert!(round_trip(dense.clone(), vec![Cell::Json(r#"{"n":1,"s":"x"}"#.into())]).is_err());
        assert!(round_trip(dense, vec![Cell::Text("hola".into())]).is_err());
    }

    #[test]
    fn duckdb_types_read_as_text() {
        for (t, k) in [("UHUGEINT", Some(AsText::Number)), ("BIGNUM", Some(AsText::Number)), ("bit", Some(AsText::Text)), ("TIME WITH TIME ZONE", Some(AsText::Text)), ("interval", Some(AsText::Text)), ("HUGEINT", None), ("TIME", None)] {
            assert_eq!(duckdb_as_text(t), k, "{t}");
        }
        for t in ["UHUGEINT[]", "STRUCT(a BIT)", "MAP(VARCHAR, BIGNUM)", "STRUCT(a TIME WITH TIME ZONE, b INTEGER)", "UNION(n INTEGER, b BIT)", "BIT[3]", "INTERVAL[]", "STRUCT(a INTERVAL)"] {
            assert!(duckdb_nested_as_text(t), "{t}");
        }
        for t in ["BIT", "INTERVAL", "STRUCT(\"interval\" INTEGER)", "STRUCT(bit INTEGER)", "STRUCT(\"BIT\" INTEGER)", "ENUM('BIT', 'x')", "INTEGER[]", "STRUCT(bits BLOB)", "TIME"] {
            assert!(!duckdb_nested_as_text(t), "{t}");
        }
        let types = vec![("Id".to_string(), "INTEGER".to_string()), ("u".to_string(), "UHUGEINT".to_string())];
        assert_eq!((type_of(&types, "id"), type_of(&types, "u"), type_of(&types, "x")), (Some("INTEGER"), Some("UHUGEINT"), None));
    }

    #[test]
    fn windows_close_by_rows_or_bytes() {
        let w = Window { rows: 100_000, bytes: 512 << 20 };
        assert!(!w.full(99_999, 1) && w.full(100_000, 0) && w.full(1, 512 << 20));
        let wide = RowBatch { rows: vec![vec![Cell::Bytes(vec![0; 1 << 20])]], bytes: 0 };
        assert!(batch_bytes(&wide) >= 1 << 20);
        assert_eq!(batch_bytes(&RowBatch { rows: vec![], bytes: 7 }), 7);
    }
}

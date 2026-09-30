//! Bulk transfer (see `dbine_driver::transfer`) for Cloud Spanner.
//!
//! Reading: one read-only transaction (a consistent snapshot) split with
//! `partitionQuery`; each partition is read with `executeStreamingSql`,
//! whose `PartialResultSet`s are merged (chunked values) and typed from the
//! result's row type: INT64 as integers, NUMERIC exact, BYTES whole,
//! TIMESTAMP with its nanoseconds, ARRAY / STRUCT / JSON as JSON. A query
//! that can't be partitioned (a view…) is read as a single stream in the
//! same transaction. A filter goes to the `WHERE`.
//!
//! Loading: `insert` mutations (never `insertOrUpdate`: existing rows are
//! not overwritten), committed in windows within Spanner's limits: 80,000
//! mutations per commit (a row counts its columns plus its secondary index
//! entries) and 100 MB, and within `LoadSpec`'s own. A commit refused for
//! its size (some emulator versions have lower limits) is split in halves
//! and the window shrinks for the rest of the load. Failures are told apart
//! by their gRPC status, not their wording: only those where nothing was
//! written (`ABORTED`, `RESOURCE_EXHAUSTED`, a lost session) are retried.
//! Rows wait for their commit serialized, so a window costs its encoded
//! bytes. A read cut short is resumed from its last resume token.

use crate::{bq, bq_qualified, create_session, gcp, SpannerSession};
use base64::Engine;
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result};
use serde_json::{json, Map, Value as Json};
use std::collections::VecDeque;
use std::time::Duration;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Mutations per commit (Spanner's limit).
const MAX_MUTATIONS: u64 = 80_000;
/// Encoded bytes per commit. Far under Spanner's 100 MB: the serialized
/// window and the request built from it are both alive while it is sent,
/// so this keeps a table within ~32 MiB in flight. A smaller window only
/// costs more commits.
const MAX_COMMIT_BYTES: u64 = 16 * 1024 * 1024;
/// Attempts of a commit that Spanner aborted (lock conflicts).
const ABORT_RETRIES: u32 = 5;

/// A column as the catalog describes it.
pub(crate) struct CatalogColumn {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
}

impl SpannerSession {
    /// The table's visible columns in order; empty if it doesn't exist.
    async fn catalog_columns(&mut self, t: &ObjectRef) -> Result<Vec<CatalogColumn>> {
        let rows = self
            .text_rows(
                "SELECT COLUMN_NAME, SPANNER_TYPE, IS_NULLABLE FROM INFORMATION_SCHEMA.COLUMNS
                 WHERE TABLE_SCHEMA = @p0 AND TABLE_NAME = @p1 AND NOT IS_HIDDEN ORDER BY ORDINAL_POSITION",
                &[t.schema().unwrap_or(""), &t.name],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
                CatalogColumn { name: s(0), type_name: s(1), nullable: s(2) != "NO" }
            })
            .collect())
    }

    /// Mutations a written row adds through the table's other indexes
    /// (secondary, foreign-key backing, search, vector…): each entry counts
    /// its listed columns plus the table's key, which every index entry
    /// carries implicitly. It errs on the high side: a window that's too
    /// small only costs a commit more; one too big is refused.
    async fn index_columns(&mut self, t: &ObjectRef) -> Result<u64> {
        let rows = self
            .text_rows(
                "SELECT
                   (SELECT COUNT(*) FROM INFORMATION_SCHEMA.INDEX_COLUMNS
                     WHERE TABLE_SCHEMA = @p0 AND TABLE_NAME = @p1 AND INDEX_TYPE != 'PRIMARY_KEY'),
                   (SELECT COUNT(*) FROM INFORMATION_SCHEMA.INDEXES
                     WHERE TABLE_SCHEMA = @p0 AND TABLE_NAME = @p1 AND INDEX_TYPE != 'PRIMARY_KEY'),
                   (SELECT COUNT(*) FROM INFORMATION_SCHEMA.INDEX_COLUMNS
                     WHERE TABLE_SCHEMA = @p0 AND TABLE_NAME = @p1 AND INDEX_TYPE = 'PRIMARY_KEY')",
                &[t.schema().unwrap_or(""), &t.name],
            )
            .await?;
        let n = |i: usize| rows.first().and_then(|r| r.get(i).cloned().flatten()).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        Ok(index_mutations(n(0), n(1), n(2)))
    }

    /// `POST {session}:{verb}`, recreating the session once if Spanner
    /// dropped it (idle sessions expire after an hour).
    async fn session_call(&mut self, verb: &str, body: &Json) -> Result<Json> {
        match self.api.post(&format!("{}:{verb}", self.session), body).await {
            Err(Error::Query(m)) if m.contains("Session not found") => {
                self.session = create_session(&self.api, &self.database).await?;
                self.api.post(&format!("{}:{verb}", self.session), body).await
            }
            other => other,
        }
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let catalog = self.catalog_columns(&spec.table).await?;
        if catalog.is_empty() {
            return Err(Error::Query(format!("No existe la tabla {}.", bq_qualified(spec.table.schema(), &spec.table.name))));
        }
        let cols = read_columns(&catalog, spec.columns.as_deref())?;
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let sql = select_sql(&spec.table, &names, spec.filter.as_deref());
        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;

        let tx = self.session_call("beginTransaction", &json!({ "options": { "readOnly": { "strong": true } } })).await?;
        let tx = tx.get("id").and_then(Json::as_str).map(str::to_string).ok_or_else(|| Error::Query("Spanner no abrió la transacción de lectura.".into()))?;
        let parts: Vec<Option<String>> =
            match self.session_call("partitionQuery", &json!({ "transaction": { "id": tx }, "sql": sql })).await {
                Ok(r) => r
                    .get("partitions")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.get("partitionToken").and_then(Json::as_str).map(|t| Some(t.to_string())))
                    .collect(),
                // Not root-partitionable (views, some filters): one stream.
                Err(Error::Query(m)) => {
                    tracing::debug!("spanner: lectura sin particiones: {m}");
                    vec![None]
                }
                Err(e) => return Err(e),
            };

        let mut builder = BatchBuilder::new();
        for part in parts {
            let mut body = json!({ "transaction": { "id": tx }, "sql": sql });
            if let Some(t) = part {
                body["partitionToken"] = json!(t);
            }
            self.stream(&body, &sink, &mut builder).await?;
        }
        builder.flush(&mut *sink.lock().map_err(|_| Error::State("destino de lotes".into()))?)?;
        Ok(builder.rows)
    }

    /// One `executeStreamingSql`, its rows handed to `builder` as they
    /// come. A stream cut short (network, `UNAVAILABLE`) is resumed from its
    /// last resume token, rows after it dropped so none arrives twice.
    async fn stream(&mut self, body: &Json, sink: &BatchSinkRef, builder: &mut BatchBuilder) -> Result<()> {
        let mut resume = Resume::new();
        let mut retries = 0;
        loop {
            match self.stream_once(body, &mut resume, sink, builder).await {
                Ok(()) => {
                    let rest = resume.finish()?;
                    return hand_over(rest, sink, builder);
                }
                Err(Error::Connect(m)) if resume.resumable && retries < STREAM_RETRIES => {
                    retries += 1;
                    tracing::debug!("spanner: lectura cortada ({m}); se retoma");
                    tokio::time::sleep(Duration::from_millis(200 << retries)).await;
                    resume.rewind();
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn stream_once(&mut self, body: &Json, resume: &mut Resume, sink: &BatchSinkRef, builder: &mut BatchBuilder) -> Result<()> {
        let url = format!("{}/v1/{}:executeStreamingSql", self.api.base, self.session);
        let mut body = body.clone();
        if let Some(t) = &resume.token {
            body["resumeToken"] = json!(t);
        }
        let mut req = self.api.http.post(url).json(&body);
        if let Some(b) = self.api.tokens.bearer().await? {
            req = req.header("Authorization", b);
        }
        let mut resp = req.send().await.map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            return Err(http_error(status, &text));
        }
        let mut split = JsonSplitter::default();
        while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Connect(e.to_string()))? {
            for msg in split.feed(&chunk)? {
                hand_over(resume.push(result_set(msg)?)?, sink, builder)?;
            }
        }
        split.finish()
    }

    pub(crate) async fn transfer_load(&mut self, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden cargar datos.".into()));
        }
        if spec.columns.is_empty() {
            return Err(Error::Query("La carga no tiene columnas.".into()));
        }
        let catalog = self.catalog_columns(&spec.table).await?;
        if catalog.is_empty() {
            return Err(Error::Query(format!("No existe la tabla {}.", bq_qualified(spec.table.schema(), &spec.table.name))));
        }
        let types: Vec<Ty> = spec
            .columns
            .iter()
            .map(|n| {
                catalog
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(n))
                    .map(|c| Ty::parse(&c.type_name))
                    .ok_or_else(|| Error::Query(format!("La tabla no tiene la columna «{n}».")))
            })
            .collect::<Result<_>>()?;
        let per_row = spec.columns.len() as u64 + self.index_columns(&spec.table).await?;
        let mut window = Window {
            rows: (MAX_MUTATIONS / per_row).clamp(1, spec.commit_rows.max(1)),
            bytes: spec.commit_bytes.clamp(1, MAX_COMMIT_BYTES),
        };
        let target = Target { table: mutation_table(&spec.table), columns: &spec.columns };

        let (mut pending, mut done) = (Pending::default(), 0u64);
        while let Some(batch) = source.next().await {
            for row in batch.rows {
                if row.len() != types.len() {
                    return Err(Error::Query(format!("La fila tiene {} valores y la carga {} columnas.", row.len(), types.len())));
                }
                let values = row
                    .iter()
                    .zip(&types)
                    .enumerate()
                    .map(|(i, (c, t))| encode(c, t).map_err(|e| Error::Query(format!("Columna «{}»: {e}", spec.columns[i]))))
                    .collect::<Result<Vec<Json>>>()?;
                // Kept serialized: a row costs its encoded bytes and no more.
                let row = serde_json::to_string(&values)?;
                // The rows before it go first if it would push the window
                // over its bytes: a window never holds more than its
                // bytes, or one row alone.
                if let Some(rows) = pending.make_room(row.len(), &window) {
                    self.commit_window(&target, rows, &mut window, &mut done, progress).await?;
                }
                pending.push(row);
                if pending.full(&window) {
                    self.commit_window(&target, pending.take(), &mut window, &mut done, progress).await?;
                }
            }
        }
        if !pending.rows.is_empty() {
            self.commit_window(&target, pending.take(), &mut window, &mut done, progress).await?;
        }
        Ok(done)
    }

    /// Commit `rows`; a commit refused for its size is split in halves
    /// (nothing of it was written) and the window shrinks.
    async fn commit_window(&mut self, target: &Target<'_>, rows: Vec<String>, window: &mut Window, done: &mut u64, progress: Progress<'_>) -> Result<()> {
        let mut queue = VecDeque::from([rows]);
        while let Some(mut part) = queue.pop_front() {
            match self.commit(target, &part).await {
                Ok(()) => {
                    *done += part.len() as u64;
                    progress(*done);
                }
                Err(CommitError::TooBig(_)) if part.len() > 1 => {
                    let half = part.len() / 2;
                    window.rows = window.rows.min(half as u64).max(1);
                    let bytes: u64 = part[..half].iter().map(|r| r.len() as u64 + 1).sum();
                    window.bytes = window.bytes.min(bytes).max(1);
                    let rest = part.split_off(half);
                    queue.push_front(rest);
                    queue.push_front(part);
                }
                Err(CommitError::TooBig(e) | CommitError::Failed(e)) => return Err(e),
            }
        }
        Ok(())
    }

    /// One single-use read-write transaction with the rows' `insert`.
    /// Only failures where Spanner says nothing was written are retried
    /// (aborted, the session gone, throttled); a lost answer (network,
    /// `UNAVAILABLE`, `DEADLINE_EXCEEDED`) may have committed, so it is
    /// returned, never sent again.
    async fn commit(&mut self, target: &Target<'_>, rows: &[String]) -> std::result::Result<(), CommitError> {
        let mut delay = Duration::from_millis(100);
        let mut attempt = 0;
        let mut renewed = false;
        loop {
            attempt += 1;
            // Built for every attempt (instead of cloned): the request owns
            // it and no second copy of the window stays in memory.
            let body = commit_body(target, rows).map_err(CommitError::Failed)?;
            let url = format!("{}/v1/{}:commit", self.api.base, self.session);
            let mut req = self.api.http.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(body);
            if let Some(b) = self.api.tokens.bearer().await.map_err(CommitError::Failed)? {
                req = req.header("Authorization", b);
            }
            let resp = req.send().await.map_err(|e| CommitError::Failed(Error::Connect(e.to_string())))?;
            let status = resp.status().as_u16();
            let text = resp.text().await.map_err(|e| CommitError::Failed(Error::Connect(e.to_string())))?;
            if (200..300).contains(&status) {
                return Ok(());
            }
            let err = http_error(status, &text);
            match commit_failure(status, &text) {
                Failure::SessionGone if !renewed => {
                    renewed = true;
                    self.session = create_session(&self.api, &self.database).await.map_err(CommitError::Failed)?;
                }
                // Out of resources for this commit: smaller ones help, and
                // halving a window nothing of which was written is safe.
                Failure::Exhausted if rows.len() > 1 => return Err(CommitError::TooBig(err)),
                Failure::Retry | Failure::Exhausted if attempt < ABORT_RETRIES => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Failure::TooBig => return Err(CommitError::TooBig(err)),
                _ => return Err(CommitError::Failed(err)),
            }
        }
    }
}

enum CommitError {
    /// Refused for its size: nothing was written.
    TooBig(Error),
    Failed(Error),
}

#[derive(Debug, PartialEq)]
enum Failure {
    /// `ABORTED`: nothing was written and trying again can work.
    Retry,
    /// `RESOURCE_EXHAUSTED` without a size reason (throttling, memory):
    /// nothing was written.
    Exhausted,
    /// Refused for its size (mutations or bytes): nothing was written.
    TooBig,
    SessionGone,
    Other,
}

/// The gRPC status of an error answer: the REST API's
/// `{"error": {"status": "ABORTED"…}}`, or the emulator gateway's flat
/// `{"code": 10…}` (a gRPC code).
fn grpc_status(body: &str) -> Option<String> {
    let j: Json = serde_json::from_str(body).ok()?;
    if let Some(s) = j.pointer("/error/status").and_then(Json::as_str) {
        return Some(s.to_string());
    }
    let code = j.get("code").and_then(Json::as_u64).filter(|_| j.get("error").is_none())?;
    let name = match code {
        1 => "CANCELLED",
        3 => "INVALID_ARGUMENT",
        4 => "DEADLINE_EXCEEDED",
        5 => "NOT_FOUND",
        6 => "ALREADY_EXISTS",
        8 => "RESOURCE_EXHAUSTED",
        9 => "FAILED_PRECONDITION",
        10 => "ABORTED",
        14 => "UNAVAILABLE",
        _ => return None,
    };
    Some(name.into())
}

/// What a failed commit means, from its gRPC status first and its text
/// only where the status alone doesn't tell (a size limit is reported as
/// `INVALID_ARGUMENT` or `RESOURCE_EXHAUSTED`; the emulator may lose the
/// status).
fn commit_failure(http: u16, body: &str) -> Failure {
    let message = http_error(http, body).to_string();
    let status = grpc_status(body);
    // The size wording counts only under a status a size limit uses (or
    // none): a duplicate key or a bad value whose text happens to say
    // "too large" must fail the window whole, never split it.
    let size_status = matches!(status.as_deref(), None | Some("INVALID_ARGUMENT" | "RESOURCE_EXHAUSTED"));
    if http == 413 || (size_status && too_big(&message)) {
        return Failure::TooBig;
    }
    match status.as_deref() {
        Some("ABORTED") => Failure::Retry,
        Some("RESOURCE_EXHAUSTED") => Failure::Exhausted,
        Some("NOT_FOUND") if message.contains("Session not found") => Failure::SessionGone,
        Some(_) => Failure::Other,
        // No status (a gateway that lost it): the text is all there is.
        None if message.contains("Session not found") => Failure::SessionGone,
        None if message.to_ascii_lowercase().contains("transaction was aborted") => Failure::Retry,
        None => Failure::Other,
    }
}

struct Window {
    rows: u64,
    bytes: u64,
}

/// The serialized rows waiting for their commit and their bytes.
#[derive(Default)]
struct Pending {
    rows: Vec<String>,
    bytes: u64,
}

impl Pending {
    /// The rows to commit before a row of `len` bytes joins them, when it
    /// would take the window over its bytes (peak memory: the window and
    /// its request body, so twice the larger of the window and one row).
    fn make_room(&mut self, len: usize, window: &Window) -> Option<Vec<String>> {
        (!self.rows.is_empty() && self.bytes + len as u64 + 1 > window.bytes).then(|| self.take())
    }

    fn push(&mut self, row: String) {
        self.bytes += row.len() as u64 + 1;
        self.rows.push(row);
    }

    fn full(&self, window: &Window) -> bool {
        self.rows.len() as u64 >= window.rows || self.bytes >= window.bytes
    }

    fn take(&mut self) -> Vec<String> {
        self.bytes = 0;
        std::mem::take(&mut self.rows)
    }
}

struct Target<'a> {
    table: String,
    columns: &'a [String],
}

/// The commit request: one `insert` with the rows, already serialized.
fn commit_body(target: &Target<'_>, rows: &[String]) -> Result<Vec<u8>> {
    let table = serde_json::to_string(&target.table)?;
    let columns = serde_json::to_string(target.columns)?;
    let size: usize = rows.iter().map(|r| r.len() + 1).sum();
    let mut b = Vec::with_capacity(size + table.len() + columns.len() + 96);
    b.extend_from_slice(br#"{"singleUseTransaction":{"readWrite":{}},"mutations":[{"insert":{"table":"#);
    b.extend_from_slice(table.as_bytes());
    b.extend_from_slice(br#","columns":"#);
    b.extend_from_slice(columns.as_bytes());
    b.extend_from_slice(br#","values":["#);
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            b.push(b',');
        }
        b.extend_from_slice(r.as_bytes());
    }
    b.extend_from_slice(b"]}}]}");
    Ok(b)
}

/// Mutations per row from the other indexes: their listed columns, plus
/// the table's key once per index.
fn index_mutations(index_columns: u64, indexes: u64, key_columns: u64) -> u64 {
    index_columns + indexes * key_columns
}

/// A mutation names the table unquoted, with its schema.
fn mutation_table(t: &ObjectRef) -> String {
    match t.schema() {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

/// Spanner refused the commit for its size (mutations or bytes). Splitting
/// commits the halves one by one, so it is decided only when the text
/// names the commit as a whole (the transaction, its mutations, the
/// request or message) and nothing about one value: one value over its
/// column's length is a data error, and splitting would commit the rows
/// around it. Unknown wordings fail the window whole.
fn too_big(m: &str) -> bool {
    let m = m.to_ascii_lowercase();
    let one_value = ["column", "cell", "value", "string of length", "bytes of length", "key", "row "];
    // "too many mutations" explains itself counting columns: that one
    // speaks of the whole commit whatever else it says.
    if m.contains("too many mutations") || m.contains("mutation limit") {
        return true;
    }
    if one_value.iter().any(|k| m.contains(k)) {
        return false;
    }
    let whole = ["transaction", "mutation", "commit", "request", "payload", "message", "entity"];
    let size = ["too large", "larger than max", "exceeds the maximum", "exceeds the limit", "size limit", "exceeds limit"];
    whole.iter().any(|k| m.contains(k)) && size.iter().any(|k| m.contains(k))
}

/// The read's columns: the requested ones in their order, or all.
fn read_columns(catalog: &[CatalogColumn], wanted: Option<&[String]>) -> Result<Vec<TransferColumn>> {
    let col = |c: &CatalogColumn| TransferColumn { name: c.name.clone(), type_name: c.type_name.clone(), nullable: c.nullable };
    let cols: Vec<TransferColumn> = match wanted {
        None => catalog.iter().map(col).collect(),
        Some(names) => names
            .iter()
            .map(|n| {
                catalog
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(n))
                    .map(col)
                    .ok_or_else(|| Error::Query(format!("La tabla no tiene la columna «{n}».")))
            })
            .collect::<Result<_>>()?,
    };
    if cols.is_empty() {
        return Err(Error::Query("La lectura no tiene columnas.".into()));
    }
    Ok(cols)
}

fn select_sql(t: &ObjectRef, columns: &[String], filter: Option<&str>) -> String {
    let list = columns.iter().map(|c| bq(c)).collect::<Vec<_>>().join(", ");
    let mut sql = format!("SELECT {list} FROM {}", bq_qualified(t.schema(), &t.name));
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        sql.push_str(&format!(" WHERE ({f})"));
    }
    sql
}

/// Times a cut stream is resumed.
const STREAM_RETRIES: u32 = 3;
/// Rows held until the next resume token; past this they are handed over
/// and the stream can no longer be resumed until the next token.
const HOLD_BYTES: usize = 8 * 1024 * 1024;

fn hand_over(rows: Vec<Vec<Cell>>, sink: &BatchSinkRef, builder: &mut BatchBuilder) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
    for r in rows {
        builder.push(r, &mut *s)?;
    }
    Ok(())
}

/// A stream's rows, held back until a resume token covers them: only then
/// are they handed over, so a stream resumed from that token never repeats
/// one.
#[derive(Default)]
struct Resume {
    rows: RowAssembler,
    held: Vec<Vec<Cell>>,
    held_bytes: usize,
    /// The last resume token and the assembler's partial row at it.
    token: Option<String>,
    mark: (Vec<Json>, bool),
    /// Nothing handed over past `token` (or past the start, without one).
    resumable: bool,
}

impl Resume {
    /// Nothing handed over yet: a cut before the first token starts over.
    fn new() -> Resume {
        Resume { resumable: true, ..Default::default() }
    }

    fn push(&mut self, prs: Json) -> Result<Vec<Vec<Cell>>> {
        let token = prs.get("resumeToken").and_then(Json::as_str).filter(|t| !t.is_empty()).map(str::to_string);
        let rows = self.rows.push(prs)?;
        self.held_bytes += rows.iter().flatten().map(Cell::size).sum::<usize>();
        self.held.extend(rows);
        if let Some(t) = token {
            self.token = Some(t);
            self.mark = (self.rows.pending.clone(), self.rows.chunked);
            self.resumable = true;
        } else if self.held_bytes > HOLD_BYTES {
            self.resumable = false;
        } else {
            return Ok(Vec::new());
        }
        self.held_bytes = 0;
        Ok(std::mem::take(&mut self.held))
    }

    /// Back to the last token: what came after it will come again.
    fn rewind(&mut self) {
        self.held.clear();
        self.held_bytes = 0;
        self.rows.pending = self.mark.0.clone();
        self.rows.chunked = self.mark.1;
    }

    fn finish(&mut self) -> Result<Vec<Vec<Cell>>> {
        self.rows.finish()?;
        Ok(std::mem::take(&mut self.held))
    }
}

/// An error answer, as `Api::send` reads it.
fn http_error(status: u16, body: &str) -> Error {
    let flat: Option<Json> = serde_json::from_str(body).ok();
    if let Some(m) = flat.as_ref().and_then(|j| j.get("message")).and_then(Json::as_str) {
        return if status == 401 { Error::AuthFailed(m.into()) } else { Error::Query(m.into()) };
    }
    gcp::api_error(status, body)
}

/// A stream element as a `PartialResultSet`: the REST API sends them as a
/// JSON array, the emulator's gateway as `{"result": …}` lines; an error
/// comes as `{"error": …}` in both.
fn result_set(mut msg: Json) -> Result<Json> {
    if let Some(e) = msg.get("error") {
        let m = e.get("message").and_then(Json::as_str).map(str::to_string).unwrap_or_else(|| e.to_string());
        // The server dropped the stream: it can be resumed.
        let unavailable = e.get("status").and_then(Json::as_str) == Some("UNAVAILABLE") || e.get("code").and_then(Json::as_u64) == Some(14);
        return Err(if unavailable { Error::Connect(m) } else { Error::Query(m) });
    }
    Ok(match msg.get_mut("result") {
        Some(r) => r.take(),
        None => msg,
    })
}

/// Splits a stream of bytes into its top-level JSON objects, whether they
/// come inside an array (`[{…},{…}]`) or one per line.
#[derive(Default)]
struct JsonSplitter {
    buf: Vec<u8>,
    /// Where the object being read starts.
    start: Option<usize>,
    /// Bytes of `buf` already scanned.
    pos: usize,
    depth: usize,
    in_str: bool,
    esc: bool,
}

impl JsonSplitter {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Json>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut i = self.pos;
        while i < self.buf.len() {
            let c = self.buf[i];
            match self.start {
                None => match c {
                    b'{' => {
                        self.start = Some(i);
                        self.depth = 1;
                    }
                    b'[' | b']' | b',' | b' ' | b'\n' | b'\r' | b'\t' => {}
                    _ => return Err(Error::Query("Spanner devolvió una respuesta inesperada al leer.".into())),
                },
                Some(s) => {
                    if self.in_str {
                        if self.esc {
                            self.esc = false;
                        } else if c == b'\\' {
                            self.esc = true;
                        } else if c == b'"' {
                            self.in_str = false;
                        }
                    } else {
                        match c {
                            b'"' => self.in_str = true,
                            b'{' | b'[' => self.depth += 1,
                            b'}' | b']' => {
                                self.depth -= 1;
                                if self.depth == 0 {
                                    out.push(serde_json::from_slice(&self.buf[s..=i])?);
                                    self.start = None;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            i += 1;
        }
        match self.start {
            None => {
                self.buf.clear();
                self.pos = 0;
            }
            Some(s) => {
                self.buf.drain(..s);
                self.start = Some(0);
                self.pos = i - s;
            }
        }
        Ok(out)
    }

    fn finish(&self) -> Result<()> {
        if self.start.is_some() {
            return Err(Error::Connect("la lectura se cortó antes de terminar".into()));
        }
        Ok(())
    }
}

/// Turns `PartialResultSet`s into rows of cells: values come flat (rows ×
/// columns) and the last one of a set may continue in the next
/// (`chunkedValue`).
#[derive(Default)]
struct RowAssembler {
    types: Vec<Json>,
    pending: Vec<Json>,
    chunked: bool,
}

impl RowAssembler {
    fn push(&mut self, mut prs: Json) -> Result<Vec<Vec<Cell>>> {
        if self.types.is_empty() {
            if let Some(f) = prs.pointer("/metadata/rowType/fields").and_then(Json::as_array) {
                self.types = f.iter().map(|f| f.get("type").cloned().unwrap_or(Json::Null)).collect();
            }
        }
        let mut values = match prs.get_mut("values").map(Json::take) {
            Some(Json::Array(v)) => v,
            _ => Vec::new(),
        };
        if values.is_empty() {
            return Ok(Vec::new());
        }
        if self.types.is_empty() {
            return Err(Error::Query("Spanner devolvió filas sin sus columnas.".into()));
        }
        if self.chunked {
            let first = values.remove(0);
            let last = self.pending.pop().ok_or_else(|| Error::Query("Spanner devolvió un valor partido sin su comienzo.".into()))?;
            self.pending.push(merge(last, first)?);
        }
        self.pending.extend(values);
        self.chunked = prs.get("chunkedValue").and_then(Json::as_bool).unwrap_or(false);

        let n = self.types.len();
        let usable = self.pending.len() - usize::from(self.chunked);
        let take = usable / n * n;
        let rows = self
            .pending
            .drain(..take)
            .collect::<Vec<_>>()
            .chunks(n)
            .map(|r| r.iter().zip(&self.types).map(|(v, t)| to_cell(v, t)).collect())
            .collect();
        Ok(rows)
    }

    fn finish(&self) -> Result<()> {
        if self.chunked || !self.pending.is_empty() {
            return Err(Error::Query("La lectura terminó con una fila incompleta.".into()));
        }
        Ok(())
    }
}

/// A chunked value with its continuation: strings are concatenated; in
/// lists, a trailing string or list is merged with the next's first.
fn merge(a: Json, b: Json) -> Result<Json> {
    match (a, b) {
        (Json::String(mut x), Json::String(y)) => {
            x.push_str(&y);
            Ok(Json::String(x))
        }
        (Json::Array(mut x), Json::Array(y)) => {
            let mut y = y.into_iter();
            if let Some(first) = y.next() {
                match x.pop() {
                    Some(last) if matches!((&last, &first), (Json::String(_), Json::String(_)) | (Json::Array(_), Json::Array(_))) => {
                        x.push(merge(last, first)?)
                    }
                    Some(last) => {
                        x.push(last);
                        x.push(first);
                    }
                    None => x.push(first),
                }
            }
            x.extend(y);
            Ok(Json::Array(x))
        }
        _ => Err(Error::Query("Spanner devolvió un valor partido que no se puede unir.".into())),
    }
}

// ---- Spanner value → cell ----

fn code(t: &Json) -> &str {
    t.get("code").and_then(Json::as_str).unwrap_or("")
}

fn float_of(s: &str) -> f64 {
    match s {
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        other => other.parse().unwrap_or(f64::NAN),
    }
}

/// Plain decimal digits (a NUMERIC that isn't PostgreSQL's `NaN`).
fn is_decimal(s: &str) -> bool {
    let d = s.strip_prefix('-').unwrap_or(s);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit() || b == b'.') && d.bytes().filter(|&b| b == b'.').count() <= 1
}

/// `2024-01-31T13:45:00.123456789Z` → `2024-01-31 13:45:00.123456789+00:00`.
fn timestamp_text(s: &str) -> String {
    let t = s.replacen('T', " ", 1);
    match t.strip_suffix('Z') {
        Some(u) => format!("{u}+00:00"),
        None => t,
    }
}

pub(crate) fn to_cell(v: &Json, t: &Json) -> Cell {
    match (code(t), v) {
        (_, Json::Null) => Cell::Null,
        ("BOOL", Json::Bool(b)) => Cell::Bool(*b),
        ("INT64" | "ENUM", Json::String(s)) => s.parse().map_or_else(|_| Cell::Text(s.clone()), Cell::Int),
        ("FLOAT64" | "FLOAT32", Json::Number(n)) => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
        ("FLOAT64" | "FLOAT32", Json::String(s)) => Cell::Float(float_of(s)),
        ("NUMERIC", Json::String(s)) if is_decimal(s) => Cell::Decimal(s.clone()),
        ("BYTES" | "PROTO", Json::String(s)) => B64.decode(s).map_or_else(|_| Cell::Text(s.clone()), Cell::Bytes),
        ("DATE", Json::String(s)) => Cell::Date(s.clone()),
        ("TIMESTAMP", Json::String(s)) => Cell::DateTimeTz(timestamp_text(s)),
        ("UUID", Json::String(s)) => Cell::Uuid(s.to_ascii_lowercase()),
        ("JSON", Json::String(s)) => Cell::Json(s.clone()),
        ("ARRAY" | "STRUCT", _) => Cell::Json(whole_json(v, t).to_string()),
        (_, Json::String(s)) => Cell::Text(s.clone()),
        (_, Json::Bool(b)) => Cell::Bool(*b),
        (_, other) => Cell::Json(other.to_string()),
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for x in b {
        s.push_str(&format!("{x:02X}"));
    }
    s
}

/// A value inside an ARRAY / STRUCT as JSON: integers as numbers, NUMERIC
/// as exact text, binaries whole as `0x…` hex, JSON parsed, structs as
/// objects.
fn whole_json(v: &Json, t: &Json) -> Json {
    match (code(t), v) {
        (_, Json::Null) => Json::Null,
        ("ARRAY", Json::Array(a)) => {
            let et = t.get("arrayElementType").unwrap_or(&Json::Null);
            Json::Array(a.iter().map(|e| whole_json(e, et)).collect())
        }
        ("STRUCT", Json::Array(a)) => {
            let fields = t.pointer("/structType/fields").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
            let m: Map<String, Json> = fields
                .iter()
                .zip(a)
                .enumerate()
                .map(|(i, (f, e))| {
                    let name = f.get("name").and_then(Json::as_str).filter(|n| !n.is_empty()).map_or_else(|| format!("_{i}"), str::to_string);
                    (name, whole_json(e, f.get("type").unwrap_or(&Json::Null)))
                })
                .collect();
            Json::Object(m)
        }
        ("INT64" | "ENUM", Json::String(s)) => s.parse::<i64>().map_or_else(|_| v.clone(), Json::from),
        ("FLOAT64" | "FLOAT32", Json::Number(_)) => v.clone(),
        ("BYTES" | "PROTO", Json::String(s)) => B64.decode(s).map_or_else(|_| v.clone(), |b| Json::String(hex(&b))),
        ("JSON", Json::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| v.clone()),
        _ => v.clone(),
    }
}

// ---- cell → Spanner value ----

/// A column's type, from the catalog's `SPANNER_TYPE`.
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    Bool,
    Int,
    Float,
    Numeric,
    Str,
    Bytes,
    Date,
    Timestamp,
    Json,
    Uuid,
    /// Sent as text (INTERVAL…).
    Other,
    Array(Box<Ty>),
}

impl Ty {
    fn parse(s: &str) -> Ty {
        let up = s.trim().to_ascii_uppercase();
        if let Some(rest) = up.strip_prefix("ARRAY<") {
            // `ARRAY<STRING(MAX)>`, `ARRAY<FLOAT32>(vector_length=>3)`.
            let mut depth = 1;
            for (i, c) in rest.char_indices() {
                match c {
                    '<' => depth += 1,
                    '>' => {
                        depth -= 1;
                        if depth == 0 {
                            return Ty::Array(Box::new(Ty::parse(&rest[..i])));
                        }
                    }
                    _ => {}
                }
            }
            return Ty::Other;
        }
        let base = up.split(['(', '<']).next().unwrap_or("").trim();
        match base {
            "BOOL" => Ty::Bool,
            "INT64" | "ENUM" => Ty::Int,
            "FLOAT64" | "FLOAT32" => Ty::Float,
            "NUMERIC" => Ty::Numeric,
            "STRING" => Ty::Str,
            "BYTES" | "PROTO" => Ty::Bytes,
            "DATE" => Ty::Date,
            "TIMESTAMP" => Ty::Timestamp,
            "JSON" => Ty::Json,
            "UUID" => Ty::Uuid,
            _ => Ty::Other,
        }
    }
}

type Enc = std::result::Result<Json, String>;

fn cannot(c: &Cell, what: &str) -> String {
    let v = c.to_json();
    let mut s = v.as_str().map_or_else(|| v.to_string(), str::to_string);
    if s.chars().count() > 40 {
        s = s.chars().take(40).collect::<String>() + "…";
    }
    format!("no se puede guardar «{s}» como {what}")
}

fn float_json(f: f64) -> Json {
    if f.is_nan() {
        json!("NaN")
    } else if f == f64::INFINITY {
        json!("Infinity")
    } else if f == f64::NEG_INFINITY {
        json!("-Infinity")
    } else {
        json!(f)
    }
}

/// `0x…` hex (how binaries travel as text) decoded; `None` if it isn't.
fn unhex(s: &str) -> Option<Vec<u8>> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if h.len() % 2 != 0 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok()).collect()
}

/// A text form of any cell (for STRING and the like).
fn text_of(c: &Cell) -> std::result::Result<String, String> {
    Ok(match c {
        Cell::Text(s) | Cell::Decimal(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => s.clone(),
        Cell::Bytes(b) => String::from_utf8(b.clone()).map_err(|_| "un binario que no es UTF-8 no entra en un STRING".to_string())?,
        Cell::Null => String::new(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) => f.to_string(),
    })
}

fn encode(c: &Cell, t: &Ty) -> Enc {
    if matches!(c, Cell::Null) {
        return Ok(Json::Null);
    }
    Ok(match t {
        Ty::Array(et) => {
            let list = match c {
                Cell::Json(s) | Cell::Text(s) => serde_json::from_str::<Json>(s).map_err(|_| cannot(c, "lista"))?,
                _ => return Err(cannot(c, "lista")),
            };
            let Json::Array(items) = list else { return Err(cannot(c, "lista")) };
            let enc = items
                .iter()
                .map(|e| {
                    let cell = match (&**et, e) {
                        (Ty::Json, Json::Null) => Cell::Null,
                        (Ty::Json, e) => Cell::Json(e.to_string()),
                        // Inside JSON a binary is `0x…` hex (`Cell::to_json`);
                        // any other string is its UTF-8 bytes.
                        (Ty::Bytes, Json::String(s)) => Cell::Bytes(unhex(s).unwrap_or_else(|| s.as_bytes().to_vec())),
                        (_, e) => Cell::from_json(e),
                    };
                    encode(&cell, et)
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Json::Array(enc)
        }
        Ty::Bool => Json::Bool(match c {
            Cell::Bool(b) => *b,
            Cell::Int(i) => *i != 0,
            Cell::UInt(u) => *u != 0,
            Cell::Text(s) | Cell::Decimal(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => true,
                "false" | "f" | "0" => false,
                _ => return Err(cannot(c, "BOOL")),
            },
            _ => return Err(cannot(c, "BOOL")),
        }),
        Ty::Int => Json::String(match c {
            Cell::Int(i) => i.to_string(),
            Cell::UInt(u) => i64::try_from(*u).map_err(|_| cannot(c, "INT64"))?.to_string(),
            Cell::Bool(b) => i64::from(*b).to_string(),
            Cell::Float(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => (*f as i64).to_string(),
            Cell::Text(s) | Cell::Decimal(s) => {
                let s = s.trim();
                // `12` or `12.000` (an exact decimal without fraction).
                let int = match s.split_once('.') {
                    Some((i, f)) if f.bytes().all(|b| b == b'0') => i,
                    _ => s,
                };
                int.parse::<i64>().map_err(|_| cannot(c, "INT64"))?.to_string()
            }
            _ => return Err(cannot(c, "INT64")),
        }),
        Ty::Float => match c {
            Cell::Float(f) => float_json(*f),
            Cell::Int(i) => float_json(*i as f64),
            Cell::UInt(u) => float_json(*u as f64),
            Cell::Bool(b) => float_json(f64::from(u8::from(*b))),
            Cell::Text(s) | Cell::Decimal(s) => {
                let s = s.trim();
                float_json(match s {
                    "NaN" | "Infinity" | "-Infinity" => float_of(s),
                    _ => s.parse().map_err(|_| cannot(c, "FLOAT64"))?,
                })
            }
            _ => return Err(cannot(c, "FLOAT64")),
        },
        Ty::Numeric => Json::String(match c {
            Cell::Decimal(s) | Cell::Text(s) => match plain_numeric(s) {
                Some(r) => r.map_err(|why| format!("{}: {why}", cannot(c, "NUMERIC")))?,
                // Not plain digits (an exponent…): Spanner judges it.
                None => s.trim().to_string(),
            },
            Cell::Int(i) => i.to_string(),
            Cell::UInt(u) => u.to_string(),
            // Display never uses an exponent; a binary fraction that needs
            // more than 9 decimals is refused, never rounded.
            Cell::Float(f) if f.is_finite() => {
                plain_numeric(&f.to_string()).unwrap_or_else(|| Ok(f.to_string())).map_err(|why| format!("{}: {why}", cannot(c, "NUMERIC")))?
            }
            _ => return Err(cannot(c, "NUMERIC")),
        }),
        Ty::Str | Ty::Other => Json::String(text_of(c)?),
        // Text is its UTF-8 bytes, always: `0x…` hex is only how binaries
        // travel inside JSON (array elements, decoded above).
        Ty::Bytes => Json::String(B64.encode(match c {
            Cell::Bytes(b) => b.clone(),
            Cell::Text(s) | Cell::Json(s) => s.as_bytes().to_vec(),
            _ => return Err(cannot(c, "BYTES")),
        })),
        Ty::Date => Json::String(match c {
            Cell::Date(s) | Cell::Text(s) => s.trim().to_string(),
            Cell::DateTime(s) | Cell::DateTimeTz(s) if s.len() >= 10 && s.is_char_boundary(10) => s[..10].to_string(),
            _ => return Err(cannot(c, "DATE")),
        }),
        Ty::Timestamp => Json::String(match c {
            Cell::DateTimeTz(s) | Cell::DateTime(s) | Cell::Text(s) => rfc3339_utc(s).ok_or_else(|| cannot(c, "TIMESTAMP"))?,
            Cell::Date(s) => rfc3339_utc(&format!("{} 00:00:00", s.trim())).ok_or_else(|| cannot(c, "TIMESTAMP"))?,
            _ => return Err(cannot(c, "TIMESTAMP")),
        }),
        Ty::Json => Json::String(match c {
            Cell::Json(s) | Cell::Text(s) => s.clone(),
            other => other.to_json().to_string(),
        }),
        Ty::Uuid => Json::String(match c {
            Cell::Uuid(s) | Cell::Text(s) => s.trim().to_string(),
            Cell::Bytes(b) if b.len() == 16 => {
                let h = hex(b);
                let h = &h[2..];
                format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]).to_ascii_lowercase()
            }
            _ => return Err(cannot(c, "UUID")),
        }),
    })
}

/// Spanner's NUMERIC: up to 29 integer digits and 9 decimals.
const NUMERIC_INT: usize = 29;
const NUMERIC_SCALE: usize = 9;

/// A plain decimal (`[+-]digits[.digits]`) normalized for NUMERIC: no
/// leading zeros, no trailing fractional zeros (so `1.500000000000000000`
/// from a `decimal(38,18)` fits). `None` if it isn't plain digits; `Err`
/// (the reason, in Spanish) if it has more digits than NUMERIC keeps:
/// refused, never rounded.
fn plain_numeric(s: &str) -> Option<std::result::Result<String, String>> {
    let s = s.trim();
    let (neg, body) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    if int.len() > NUMERIC_INT {
        return Some(Err(format!("NUMERIC admite hasta {NUMERIC_INT} dígitos enteros")));
    }
    if frac.len() > NUMERIC_SCALE {
        return Some(Err(format!("NUMERIC admite hasta {NUMERIC_SCALE} decimales y el valor no se redondea")));
    }
    let int = if int.is_empty() { "0" } else { int };
    let sign = if neg && (int != "0" || !frac.is_empty()) { "-" } else { "" };
    Some(Ok(if frac.is_empty() { format!("{sign}{int}") } else { format!("{sign}{int}.{frac}") }))
}

/// The widest real UTC offset, in minutes (±18:00).
const MAX_OFFSET_MINUTES: i64 = 18 * 60;

fn num(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// `YYYY-MM-DD[ T]HH:MM:SS[.f…][Z| UTC|±HH[:MM]]` (no zone: UTC) → the
/// RFC 3339 in UTC that Spanner takes (`…Z`), nanoseconds kept.
fn rfc3339_utc(s: &str) -> Option<String> {
    let s = s.trim();
    // Byte offsets below: anything but ASCII isn't a timestamp (and would
    // cut a character in half).
    if s.len() < 19 || !s.is_ascii() {
        return None;
    }
    let (date, rest) = s.split_at(10);
    let (sep, rest) = rest.split_at(1);
    if !matches!(sep, " " | "T" | "t") {
        return None;
    }
    let (hms, rest) = rest.split_at(8);
    let (frac, zone) = match rest.strip_prefix('.') {
        Some(r) => {
            let n = r.bytes().take_while(u8::is_ascii_digit).count();
            (&r[..n], &r[n..])
        }
        None => ("", rest),
    };
    let d: Vec<&str> = date.split('-').collect();
    let t: Vec<&str> = hms.split(':').collect();
    // Fixed widths (`YYYY-MM-DD HH:MM:SS`): `2024-1-011` is not a date.
    if d.len() != 3 || t.len() != 3 || d[0].len() != 4 || d[1].len() != 2 || t.iter().any(|p| p.len() != 2) {
        return None;
    }
    let (y, mo, da) = (num(d[0])?, num(d[1])?, num(d[2])?);
    let (h, mi, se) = (num(t[0])?, num(t[1])?, num(t[2])?);
    // The day against its month's real length, and no leap second:
    // Spanner has none, and the offset arithmetic below would otherwise
    // move an impossible date or second to another one instead of refusing it.
    if !(1..=12).contains(&mo) || da < 1 || da > month_days(y, mo) || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let offset = match zone.trim() {
        "" | "Z" | "z" | "UTC" => 0,
        z => {
            let (sign, z) = match z.split_at(1) {
                ("+", r) => (1, r),
                ("-", r) => (-1, r),
                _ => return None,
            };
            let (hh, mm) = match (z.len(), z.as_bytes().get(2)) {
                (2, _) => (num(z)?, 0),
                (4, _) => (num(&z[..2])?, num(&z[2..])?),
                (5, Some(b':')) => (num(&z[..2])?, num(&z[3..])?),
                _ => return None,
            };
            // A real zone is within ±18:00 (ISO 8601 / java.time); any
            // other offset would move the value instead of refusing it.
            if mm > 59 || hh * 60 + mm > MAX_OFFSET_MINUTES {
                return None;
            }
            sign * (hh * 60 + mm)
        }
    };
    let frac = if frac.is_empty() { String::new() } else { format!(".{}", &frac[..frac.len().min(9)]) };
    // Always rebuilt from the numbers (offset 0 too), so both paths
    // normalize alike and the result is checked against TIMESTAMP's range.
    let secs = days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se - offset * 60;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, mo, da) = civil_from_days(days);
    // Spanner's TIMESTAMP: 0001-01-01 to 9999-12-31 (UTC).
    if !(1..=9999).contains(&y) {
        return None;
    }
    Some(format!("{y:04}-{mo:02}-{da:02}T{:02}:{:02}:{:02}{frac}Z", rem / 3600, rem % 3600 / 60, rem % 60))
}

fn month_days(y: i64, m: i64) -> i64 {
    match m {
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::kinds;

    fn t(code: &str) -> Json {
        json!({ "code": code })
    }

    #[test]
    fn cells_are_typed_from_the_row_type() {
        assert_eq!(to_cell(&json!("9007199254740993"), &t("INT64")), Cell::Int(9_007_199_254_740_993));
        assert_eq!(to_cell(&json!(1.5), &t("FLOAT64")), Cell::Float(1.5));
        assert!(matches!(to_cell(&json!("NaN"), &t("FLOAT64")), Cell::Float(f) if f.is_nan()));
        assert_eq!(to_cell(&json!("-Infinity"), &t("FLOAT32")), Cell::Float(f64::NEG_INFINITY));
        assert_eq!(
            to_cell(&json!("99999999999999999999999999999.999999999"), &t("NUMERIC")),
            Cell::Decimal("99999999999999999999999999999.999999999".into())
        );
        assert_eq!(to_cell(&json!("NaN"), &t("NUMERIC")), Cell::Text("NaN".into()));
        assert_eq!(to_cell(&json!(true), &t("BOOL")), Cell::Bool(true));
        assert_eq!(to_cell(&json!("ñ"), &t("STRING")), Cell::Text("ñ".into()));
        assert_eq!(to_cell(&json!("YWI="), &t("BYTES")), Cell::Bytes(b"ab".to_vec()));
        assert_eq!(to_cell(&json!("2024-02-29"), &t("DATE")), Cell::Date("2024-02-29".into()));
        assert_eq!(
            to_cell(&json!("2024-01-31T13:45:00.123456789Z"), &t("TIMESTAMP")),
            Cell::DateTimeTz("2024-01-31 13:45:00.123456789+00:00".into())
        );
        assert_eq!(to_cell(&json!("{\"a\": 1}"), &t("JSON")), Cell::Json("{\"a\": 1}".into()));
        assert_eq!(to_cell(&json!("ABC-DEF"), &t("UUID")), Cell::Uuid("abc-def".into()));
        assert_eq!(to_cell(&Json::Null, &t("INT64")), Cell::Null);
        let arr = json!({ "code": "ARRAY", "arrayElementType": { "code": "BYTES" } });
        assert_eq!(to_cell(&json!(["AAE=", null]), &arr), Cell::Json("[\"0x0001\",null]".into()));
        let st = json!({ "code": "ARRAY", "arrayElementType": { "code": "STRUCT", "structType": { "fields": [
            { "name": "x", "type": { "code": "INT64" } }, { "name": "", "type": { "code": "JSON" } }, { "name": "n", "type": { "code": "NUMERIC" } }
        ] } } });
        // Compared as values: key order depends on serde_json features.
        match to_cell(&json!([["1", "{\"k\":[1]}", "1.50"]]), &st) {
            Cell::Json(s) => assert_eq!(serde_json::from_str::<Json>(&s).unwrap(), json!([{ "_1": { "k": [1] }, "n": "1.50", "x": 1 }])),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stream_objects_split_at_any_boundary() {
        let array = br#"[{"metadata":{"rowType":{"fields":[]}},"values":["a\"}]"]},
{"values":["{"],"chunkedValue":true}]"#;
        let lines = b"{\"result\":{\"values\":[\"1\"]}}\n{\"result\":{\"values\":[\"2\"]}}\n";
        for input in [&array[..], &lines[..]] {
            let whole = JsonSplitter::default().feed(input).unwrap();
            assert_eq!(whole.len(), 2);
            for cut in 0..input.len() {
                let mut s = JsonSplitter::default();
                let mut got = s.feed(&input[..cut]).unwrap();
                got.extend(s.feed(&input[cut..]).unwrap());
                s.finish().unwrap();
                assert_eq!(got, whole, "cut at {cut}");
            }
        }
        let mut s = JsonSplitter::default();
        s.feed(b"[{\"values\":[").unwrap();
        assert!(s.finish().is_err());
        assert!(JsonSplitter::default().feed(b"<html>").is_err());
    }

    #[test]
    fn stream_elements_and_errors() {
        assert_eq!(result_set(json!({ "result": { "values": [] } })).unwrap(), json!({ "values": [] }));
        assert_eq!(result_set(json!({ "values": [] })).unwrap(), json!({ "values": [] }));
        let e = result_set(json!({ "error": { "code": 3, "message": "Table not found: x" } })).unwrap_err();
        assert!(e.to_string().contains("Table not found"), "{e}");
    }

    #[test]
    fn chunked_values_are_merged_into_rows() {
        let mut a = RowAssembler::default();
        let meta = json!({ "rowType": { "fields": [
            { "name": "id", "type": { "code": "INT64" } },
            { "name": "s", "type": { "code": "STRING" } },
            { "name": "l", "type": { "code": "ARRAY", "arrayElementType": { "code": "STRING" } } }
        ] } });
        // Row 1 whole, row 2's string split in two, its list in three.
        let rows = a.push(json!({ "metadata": meta, "values": ["1", "uno", ["a"], "2", "do"], "chunkedValue": true })).unwrap();
        assert_eq!(rows, vec![vec![Cell::Int(1), Cell::Text("uno".into()), Cell::Json("[\"a\"]".into())]]);
        assert!(a.push(json!({ "values": ["s", ["x", "y"]], "chunkedValue": true })).unwrap().is_empty());
        let rows = a.push(json!({ "values": [["z", "w"], "3"], "chunkedValue": false })).unwrap();
        assert_eq!(rows, vec![vec![Cell::Int(2), Cell::Text("dos".into()), Cell::Json("[\"x\",\"yz\",\"w\"]".into())]]);
        assert!(a.finish().is_err(), "row 3 is incomplete");
        let rows = a.push(json!({ "values": [Json::Null, Json::Null] })).unwrap();
        assert_eq!(rows, vec![vec![Cell::Int(3), Cell::Null, Cell::Null]]);
        a.finish().unwrap();
        // Lists of non-strings are concatenated.
        assert_eq!(merge(json!([1, 2]), json!([3])).unwrap(), json!([1, 2, 3]));
        assert_eq!(merge(json!([["a"]]), json!([["b"], ["c"]])).unwrap(), json!([["ab"], ["c"]]));
        assert!(merge(json!(1), json!("x")).is_err());
        // Values before the columns are an error, not a silent shift.
        assert!(RowAssembler::default().push(json!({ "values": ["1"] })).is_err());
    }

    #[test]
    fn read_columns_follow_the_requested_order() {
        let cat = |n: &str, t: &str, null: bool| CatalogColumn { name: n.into(), type_name: t.into(), nullable: null };
        let catalog = vec![cat("Id", "INT64", false), cat("Nombre", "STRING(20)", true), cat("Foto", "BYTES(MAX)", true)];
        let all = read_columns(&catalog, None).unwrap();
        assert_eq!(all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Id", "Nombre", "Foto"]);
        assert!(!all[0].nullable && all[1].type_name == "STRING(20)");
        let some = read_columns(&catalog, Some(&["foto".into(), "id".into()])).unwrap();
        assert_eq!(some.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Foto", "Id"]);
        assert!(read_columns(&catalog, Some(&["nope".into()])).is_err());
        assert!(read_columns(&catalog, Some(&[])).is_err());
    }

    #[test]
    fn select_statements() {
        let tb = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "t x".into() };
        assert_eq!(select_sql(&tb, &["a".into(), "b c".into()], None), "SELECT `a`, `b c` FROM `t x`");
        let tb = ObjectRef { kind: kinds::TABLE.into(), schema: Some("ventas".into()), name: "t".into() };
        assert_eq!(select_sql(&tb, &["a".into()], Some(" a > 1 OR a IS NULL ")), "SELECT `a` FROM `ventas`.`t` WHERE (a > 1 OR a IS NULL)");
        assert_eq!(select_sql(&tb, &["a".into()], Some("  ")), "SELECT `a` FROM `ventas`.`t`");
        assert_eq!(mutation_table(&tb), "ventas.t");
    }

    #[test]
    fn column_types_from_the_catalog() {
        assert_eq!(Ty::parse("STRING(MAX)"), Ty::Str);
        assert_eq!(Ty::parse("bytes(10)"), Ty::Bytes);
        assert_eq!(Ty::parse("ARRAY<STRING(MAX)>"), Ty::Array(Box::new(Ty::Str)));
        assert_eq!(Ty::parse("ARRAY<FLOAT32>(vector_length=>3)"), Ty::Array(Box::new(Ty::Float)));
        assert_eq!(Ty::parse("PROTO<a.b.C>"), Ty::Bytes);
        assert_eq!(Ty::parse("ENUM<a.b.E>"), Ty::Int);
        assert_eq!(Ty::parse("NUMERIC"), Ty::Numeric);
        assert_eq!(Ty::parse("TIMESTAMP"), Ty::Timestamp);
        assert_eq!(Ty::parse("INTERVAL"), Ty::Other);
        assert_eq!(Ty::parse("ARRAY<INT64"), Ty::Other);
    }

    #[test]
    fn cells_are_encoded_for_mutations() {
        let e = |c: Cell, t: Ty| encode(&c, &t).unwrap();
        assert_eq!(e(Cell::Null, Ty::Int), Json::Null);
        assert_eq!(e(Cell::Int(-5), Ty::Int), json!("-5"));
        assert_eq!(e(Cell::UInt(7), Ty::Int), json!("7"));
        assert!(encode(&Cell::UInt(u64::MAX), &Ty::Int).is_err());
        assert_eq!(e(Cell::Decimal("12.000".into()), Ty::Int), json!("12"));
        assert!(encode(&Cell::Decimal("12.5".into()), &Ty::Int).is_err());
        assert_eq!(e(Cell::Float(3.0), Ty::Int), json!("3"));
        assert_eq!(e(Cell::Bool(true), Ty::Int), json!("1"));
        assert_eq!(e(Cell::Float(1.25), Ty::Float), json!(1.25));
        assert_eq!(e(Cell::Float(f64::NAN), Ty::Float), json!("NaN"));
        assert_eq!(e(Cell::Float(f64::INFINITY), Ty::Float), json!("Infinity"));
        assert_eq!(e(Cell::Text("-Infinity".into()), Ty::Float), json!("-Infinity"));
        assert_eq!(e(Cell::Int(2), Ty::Float), json!(2.0));
        assert_eq!(e(Cell::Decimal(" 123.450 ".into()), Ty::Numeric), json!("123.45"));
        // A decimal(38,18) value that fits: trailing zeros are dropped.
        assert_eq!(e(Cell::Decimal("1.500000000000000000".into()), Ty::Numeric), json!("1.5"));
        assert_eq!(e(Cell::Decimal("-000.000000000000".into()), Ty::Numeric), json!("0"));
        assert_eq!(e(Cell::Decimal("+007".into()), Ty::Numeric), json!("7"));
        assert_eq!(e(Cell::Decimal(".25".into()), Ty::Numeric), json!("0.25"));
        assert_eq!(e(Cell::Decimal("99999999999999999999999999999.999999999".into()), Ty::Numeric), json!("99999999999999999999999999999.999999999"));
        // More than NUMERIC keeps is refused, never rounded.
        let err = encode(&Cell::Decimal("1.0000000001".into()), &Ty::Numeric).unwrap_err();
        assert!(err.contains("9 decimales"), "{err}");
        assert!(encode(&Cell::Decimal("100000000000000000000000000000".into()), &Ty::Numeric).unwrap_err().contains("29 dígitos"));
        assert_eq!(e(Cell::Float(0.5), Ty::Numeric), json!("0.5"));
        assert_eq!(e(Cell::Float(-2.0), Ty::Numeric), json!("-2"));
        assert!(encode(&Cell::Float(0.1 + 0.2), &Ty::Numeric).is_err());
        // Not plain digits: sent as is for Spanner to judge.
        assert_eq!(e(Cell::Text("1e3".into()), Ty::Numeric), json!("1e3"));
        assert_eq!(e(Cell::Int(9), Ty::Numeric), json!("9"));
        assert!(encode(&Cell::Float(f64::NAN), &Ty::Numeric).is_err());
        assert_eq!(e(Cell::Text("T".into()), Ty::Bool), json!(true));
        assert_eq!(e(Cell::Int(0), Ty::Bool), json!(false));
        assert!(encode(&Cell::Text("quizás".into()), &Ty::Bool).is_err());
        assert_eq!(e(Cell::Text("O'Brien".into()), Ty::Str), json!("O'Brien"));
        assert_eq!(e(Cell::Int(4), Ty::Str), json!("4"));
        assert_eq!(e(Cell::Uuid("a-b".into()), Ty::Str), json!("a-b"));
        assert!(encode(&Cell::Bytes(vec![0xFF]), &Ty::Str).is_err());
        assert_eq!(e(Cell::Bytes(b"ab".to_vec()), Ty::Bytes), json!("YWI="));
        // Text is its UTF-8 bytes, even when it looks like hex.
        assert_eq!(e(Cell::Text("0xCAFE".into()), Ty::Bytes), json!(B64.encode("0xCAFE")));
        assert_eq!(e(Cell::Text("ab".into()), Ty::Bytes), json!("YWI="));
        // Inside an array (JSON) `0x…` is a binary; other strings are text.
        assert_eq!(e(Cell::Json("[\"0x6162\", \"ab\"]".into()), Ty::Array(Box::new(Ty::Bytes))), json!(["YWI=", "YWI="]));
        assert_eq!(e(Cell::Date("2024-02-29".into()), Ty::Date), json!("2024-02-29"));
        assert_eq!(e(Cell::DateTime("2024-02-29 10:00:00".into()), Ty::Date), json!("2024-02-29"));
        assert_eq!(e(Cell::DateTimeTz("2024-01-31 13:45:00.123456789+00:00".into()), Ty::Timestamp), json!("2024-01-31T13:45:00.123456789Z"));
        assert_eq!(e(Cell::DateTime("2024-01-31 13:45:00".into()), Ty::Timestamp), json!("2024-01-31T13:45:00Z"));
        assert_eq!(e(Cell::Date("2024-01-31".into()), Ty::Timestamp), json!("2024-01-31T00:00:00Z"));
        assert!(encode(&Cell::Int(1), &Ty::Timestamp).is_err());
        assert_eq!(e(Cell::Json("{\"a\": 1}".into()), Ty::Json), json!("{\"a\": 1}"));
        assert_eq!(e(Cell::Int(1), Ty::Json), json!("1"));
        assert_eq!(e(Cell::Bytes((0..16).collect()), Ty::Uuid), json!("00010203-0405-0607-0809-0a0b0c0d0e0f"));
        assert_eq!(e(Cell::Json("[1, null, 3]".into()), Ty::Array(Box::new(Ty::Int))), json!(["1", null, "3"]));
        assert_eq!(e(Cell::Json("[\"0x00FF\"]".into()), Ty::Array(Box::new(Ty::Bytes))), json!(["AP8="]));
        assert_eq!(e(Cell::Json("[{\"a\":1}, null]".into()), Ty::Array(Box::new(Ty::Json))), json!(["{\"a\":1}", null]));
        assert_eq!(
            e(Cell::Json("[\"2024-01-01 00:00:00+03:00\"]".into()), Ty::Array(Box::new(Ty::Timestamp))),
            json!(["2023-12-31T21:00:00Z"])
        );
        assert!(encode(&Cell::Json("{\"a\":1}".into()), &Ty::Array(Box::new(Ty::Int))).is_err());
        assert!(encode(&Cell::Int(1), &Ty::Array(Box::new(Ty::Int))).is_err());
        // The error names the value, cut short.
        let long = encode(&Cell::Text("x".repeat(100)), &Ty::Int).unwrap_err();
        assert!(long.ends_with("…» como INT64"), "{long}");
    }

    #[test]
    fn timestamps_go_to_utc_with_their_nanoseconds() {
        assert_eq!(rfc3339_utc("2024-01-31T13:45:00Z").as_deref(), Some("2024-01-31T13:45:00Z"));
        assert_eq!(rfc3339_utc("2024-01-31 13:45:00 UTC").as_deref(), Some("2024-01-31T13:45:00Z"));
        assert_eq!(rfc3339_utc("2024-01-31 23:30:00.5-03:00").as_deref(), Some("2024-02-01T02:30:00.5Z"));
        assert_eq!(rfc3339_utc("2024-03-01 01:00:00.123456789+0530").as_deref(), Some("2024-02-29T19:30:00.123456789Z"));
        assert_eq!(rfc3339_utc("2000-01-01 00:00:00.1234567891+01").as_deref(), Some("1999-12-31T23:00:00.123456789Z"));
        assert_eq!(rfc3339_utc("0001-01-01 00:00:00+00:00").as_deref(), Some("0001-01-01T00:00:00Z"));
        assert_eq!(rfc3339_utc("9999-12-31 23:59:59.999999999").as_deref(), Some("9999-12-31T23:59:59.999999999Z"));
        assert_eq!(rfc3339_utc("2024-01-31"), None);
        assert_eq!(rfc3339_utc("2024-13-01 00:00:00"), None);
        assert_eq!(rfc3339_utc("2024-01-01 00:00:00+3"), None);
        assert_eq!(rfc3339_utc("2024-01-01X00:00:00"), None);
        // Non-ASCII text is refused, not a panic at a char boundary.
        for bad in ["2024-01-3ñ 13:45:00", "2024-01-01 00:00:00€", "2024-01-01 00:00:00+a€", "2024-01-01 00:00:0€", "€€€€€€€€€€€€€€€€€€€€"] {
            assert_eq!(rfc3339_utc(bad), None, "{bad}");
            assert!(encode(&Cell::Text(bad.into()), &Ty::Timestamp).unwrap_err().contains("TIMESTAMP"));
        }
        assert!(encode(&Cell::Json("[\"2024-01-3ñ 13:45:00\"]".into()), &Ty::Array(Box::new(Ty::Timestamp))).is_err());
        for days in [-719_162, -1, 0, 1, 11_016, 19_782, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn impossible_dates_are_refused_whatever_the_offset() {
        for bad in [
            "2024-02-30 10:00:00+01:00",
            "2023-02-29 12:00:00-03:00",
            "2024-02-30 10:00:00+00:00",
            "2024-04-31 00:00:00",
            "1900-02-29 00:00:00+05:00",
            "2024-06-30 23:59:60+00:30",
            "2024-06-30 23:59:60Z",
            "2024-01-00 00:00:00",
            // Offsets no zone has: refused, never applied.
            "2024-01-01 00:00:00+99:99",
            "2024-01-01 00:00:00-00:60",
            "2024-01-01 00:00:00+24:00",
            "2024-01-01 00:00:00+1999",
            "2024-01-01 00:00:00+18:01",
            "2024-01-01 00:00:00+0:130",
            "2024-01-01 00:00:00+01:3",
            // Malformed parts, with or without an offset.
            "2024-1-011 00:00:00",
            "2024-1-011 00:00:00+01:00",
            "2024-01-01 0:00:000",
            // Out of TIMESTAMP's range, before or after the offset.
            "0000-01-01 00:00:00",
            "0000-12-31 23:00:00Z",
            "9999-12-31 23:00:00-02:00",
            "0001-01-01 00:30:00+01:00",
        ] {
            assert_eq!(rfc3339_utc(bad), None, "{bad}");
        }
        assert_eq!(rfc3339_utc("2024-02-29 23:00:00-02:00").as_deref(), Some("2024-03-01T01:00:00Z"));
        assert_eq!(rfc3339_utc("2000-02-29 00:00:00+01:00").as_deref(), Some("2000-02-28T23:00:00Z"));
        assert_eq!(rfc3339_utc("2024-04-30 00:00:00").as_deref(), Some("2024-04-30T00:00:00Z"));
        assert_eq!(rfc3339_utc("2024-01-01 00:00:00+18:00").as_deref(), Some("2023-12-31T06:00:00Z"));
        assert_eq!(rfc3339_utc("2024-01-01 00:00:00-1800").as_deref(), Some("2024-01-01T18:00:00Z"));
        assert_eq!(rfc3339_utc("2024-01-01 00:00:00-00:00").as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(rfc3339_utc("2024-01-01t00:00:00.5z").as_deref(), Some("2024-01-01T00:00:00.5Z"));
    }

    #[test]
    fn a_commit_window_stays_within_the_memory_bound() {
        // The serialized window and its request body live together.
        const { assert!(2 * MAX_COMMIT_BYTES <= 32 * 1024 * 1024) };
        // A big row after an almost full window: the window goes first,
        // so no commit holds more than its bytes (or one row alone).
        let window = Window { rows: 1000, bytes: 100 };
        let mut pending = Pending::default();
        let mut commits: Vec<Vec<String>> = Vec::new();
        for len in [30, 30, 30, 90, 5, 250, 10, 60, 60] {
            let row = "x".repeat(len);
            commits.extend(pending.make_room(row.len(), &window));
            pending.push(row);
            if pending.full(&window) {
                commits.push(pending.take());
            }
        }
        commits.push(pending.take());
        let sizes: Vec<Vec<usize>> = commits.iter().map(|c| c.iter().map(String::len).collect()).collect();
        assert_eq!(sizes, [vec![30, 30, 30], vec![90, 5], vec![250], vec![10, 60], vec![60]]);
        for c in &commits {
            let bytes: u64 = c.iter().map(|r| r.len() as u64 + 1).sum();
            assert!(bytes <= window.bytes || c.len() == 1, "{bytes}");
        }
    }

    #[test]
    fn data_errors_never_split_a_window() {
        let rest = |status: &str, msg: &str| json!({ "error": { "code": 400, "message": msg, "status": status } }).to_string();
        let flat = |code: u64, msg: &str| json!({ "code": code, "message": msg }).to_string();
        // A duplicate key whose text says "too large".
        assert_eq!(commit_failure(409, &rest("ALREADY_EXISTS", "Row [too large] in table t already exists")), Failure::Other);
        assert_eq!(commit_failure(409, &flat(6, "Row [exceeds the maximum] in table t already exists")), Failure::Other);
        assert_eq!(commit_failure(400, &rest("FAILED_PRECONDITION", "Transaction is too large")), Failure::Other);
        // One value over its column's length.
        let col = "New value exceeds the maximum size limit for this column: t.s";
        assert_eq!(commit_failure(400, &rest("INVALID_ARGUMENT", col)), Failure::Other);
        assert_eq!(commit_failure(400, &flat(3, col)), Failure::Other);
        assert_eq!(commit_failure(400, col), Failure::Other);
        // One value over its limit, in wordings that don't name the column
        // the same way: still one value, never the commit's size.
        for msg in [
            "String of length 9 exceeds the maximum length 4 for t.s",
            "Cell value of column bin in table t exceeds the maximum size limit",
            "Key size exceeds the maximum size limit",
            "Value too large",
            "Row too large",
            "exceeds the maximum size limit",
        ] {
            assert_eq!(commit_failure(400, &rest("INVALID_ARGUMENT", msg)), Failure::Other, "{msg}");
            assert_eq!(commit_failure(400, &flat(3, msg)), Failure::Other, "{msg}");
        }
        // The commit's own size still splits.
        assert_eq!(commit_failure(400, &rest("INVALID_ARGUMENT", "Request payload size exceeds the limit: 104857600 bytes.")), Failure::TooBig);
        assert_eq!(commit_failure(400, &rest("RESOURCE_EXHAUSTED", "grpc: received message larger than max")), Failure::TooBig);
        assert_eq!(commit_failure(500, "Transaction is too large"), Failure::TooBig);
    }

    #[test]
    fn commits_carry_one_insert_and_respect_the_limits() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let target = Target { table: "ventas.t".into(), columns: &cols };
        let rows = vec![json!(["1", "x"]).to_string(), json!(["2", null]).to_string()];
        let body: Json = serde_json::from_slice(&commit_body(&target, &rows).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({ "singleUseTransaction": { "readWrite": {} }, "mutations": [
                { "insert": { "table": "ventas.t", "columns": ["a", "b"], "values": [["1", "x"], ["2", null]] } }
            ] })
        );
        assert!(too_big("The transaction contains too many mutations. Insert and update operations count with the multiplicity"));
        assert!(too_big("grpc: received message larger than max (5000000 vs. 4194304)"));
        assert!(!too_big("Row [1] in table t already exists"));
        let quoted = Target { table: "t\"x".into(), columns: &cols };
        let body: Json = serde_json::from_slice(&commit_body(&quoted, &[]).unwrap()).unwrap();
        assert_eq!(body.pointer("/mutations/0/insert/table"), Some(&json!("t\"x")));
        assert_eq!(body.pointer("/mutations/0/insert/values"), Some(&json!([])));
        assert_eq!(http_error(404, r#"{"code":5,"message":"Table not found"}"#).to_string(), Error::Query("Table not found".into()).to_string());
        assert!(matches!(http_error(401, r#"{"error":{"message":"no"}}"#), Error::AuthFailed(_)));
    }

    #[test]
    fn commit_failures_are_read_from_the_grpc_status() {
        let rest = |status: &str, msg: &str| json!({ "error": { "code": 400, "message": msg, "status": status } }).to_string();
        let flat = |code: u64, msg: &str| json!({ "code": code, "message": msg }).to_string();
        // Aborted, with any wording (or none the emulator could marshal).
        assert_eq!(commit_failure(409, &rest("ABORTED", "Transaction was aborted.")), Failure::Retry);
        assert_eq!(commit_failure(409, &flat(10, "failed to marshal error message")), Failure::Retry);
        // A duplicate key is also HTTP 409, and is not retried.
        assert_eq!(commit_failure(409, &rest("ALREADY_EXISTS", "Row [1] in table t already exists")), Failure::Other);
        assert_eq!(commit_failure(409, &flat(6, "Row [1] in table t already exists")), Failure::Other);
        // Size: by status 413, or its text under a size status (or none).
        assert_eq!(commit_failure(413, "<html>Request Entity Too Large</html>"), Failure::TooBig);
        assert_eq!(commit_failure(400, &rest("INVALID_ARGUMENT", "The transaction contains too many mutations.")), Failure::TooBig);
        assert_eq!(commit_failure(400, &flat(3, "Transaction is too large")), Failure::TooBig);
        // Out of resources without a size reason: halved (or retried).
        assert_eq!(commit_failure(429, &rest("RESOURCE_EXHAUSTED", "Quota exceeded")), Failure::Exhausted);
        assert_eq!(commit_failure(400, &flat(8, "failed to marshal error message")), Failure::Exhausted);
        assert_eq!(commit_failure(404, &rest("NOT_FOUND", "Session not found: projects/p/…")), Failure::SessionGone);
        assert_eq!(commit_failure(404, &rest("NOT_FOUND", "Table not found: t")), Failure::Other);
        // A lost answer may have committed: never retried.
        assert_eq!(commit_failure(503, &rest("UNAVAILABLE", "try again")), Failure::Other);
        assert_eq!(commit_failure(504, &rest("DEADLINE_EXCEEDED", "Transaction was aborted?")), Failure::Other);
        // No status at all: only the text.
        assert_eq!(commit_failure(500, "Transaction was aborted"), Failure::Retry);
        assert_eq!(commit_failure(500, "boom"), Failure::Other);
        assert_eq!(grpc_status(&flat(14, "x")).as_deref(), Some("UNAVAILABLE"));
        assert_eq!(grpc_status("nope"), None);
    }

    #[test]
    fn index_entries_count_the_key_too() {
        // Two indexes (3 listed columns) on a table with a 2-column key.
        assert_eq!(index_mutations(3, 2, 2), 7);
        assert_eq!(index_mutations(0, 0, 1), 0);
    }

    fn meta2() -> Json {
        json!({ "rowType": { "fields": [
            { "name": "id", "type": { "code": "INT64" } }, { "name": "s", "type": { "code": "STRING" } }
        ] } })
    }

    #[test]
    fn a_cut_stream_resumes_without_repeating_rows() {
        let mut r = Resume::new();
        // Rows are held until a token covers them.
        assert!(r.push(json!({ "metadata": meta2(), "values": ["1", "a"] })).unwrap().is_empty());
        let got = r.push(json!({ "values": ["2", "b", "3", "c"], "chunkedValue": true, "resumeToken": "t1" })).unwrap();
        assert_eq!(got.len(), 2);
        assert!(r.push(json!({ "values": ["cc", "4", "d"] })).unwrap().is_empty());
        assert!(r.resumable);
        // Cut: back to t1, with row 3's partial value restored.
        r.rewind();
        assert_eq!(r.token.as_deref(), Some("t1"));
        assert!(r.push(json!({ "metadata": meta2(), "values": ["cc", "4", "d"], "resumeToken": "t2" })).unwrap().len() == 2);
        assert!(r.finish().unwrap().is_empty());
        // Without a token, rows are handed over at the end.
        let mut r = Resume::new();
        assert!(r.push(json!({ "metadata": meta2(), "values": ["1", "a"] })).unwrap().is_empty());
        assert_eq!(r.finish().unwrap(), vec![vec![Cell::Int(1), Cell::Text("a".into())]]);
        // Too much without a token: handed over, and no longer resumable.
        let mut r = Resume::new();
        let big = "x".repeat(HOLD_BYTES);
        assert_eq!(r.push(json!({ "metadata": meta2(), "values": ["1", big] })).unwrap().len(), 1);
        assert!(!r.resumable);
        // An in-stream UNAVAILABLE can be resumed; other errors can't.
        assert!(matches!(result_set(json!({ "error": { "code": 14, "message": "gone" } })), Err(Error::Connect(_))));
        assert!(matches!(result_set(json!({ "error": { "status": "UNAVAILABLE", "message": "gone" } })), Err(Error::Connect(_))));
    }
}

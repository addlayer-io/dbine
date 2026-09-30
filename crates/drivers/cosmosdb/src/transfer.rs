//! Bulk transfer (see `dbine_driver::transfer`) for Azure Cosmos DB (NoSQL).
//!
//! Reading: `SELECT * FROM c [WHERE <filter>]`, one query per partition key
//! range, [`READ_RANGES`] ranges at once, each paged by its continuation
//! token with [`PAGE`] items per page (`x-ms-max-item-count`); pages are
//! handed over as they arrive. What a read holds at once (pages being
//! fetched, waiting in the channel or being turned into rows) is bounded by
//! bytes: each page reserves [`PAGE_MAX`] of a [`READ_BUDGET`] before its
//! request and keeps its real size until the consumer is done with it.
//! Without asked-for columns a first pass reads every item to learn every
//! top-level key (Cosmos DB has no schema, and a sample would drop keys
//! that only appear later); the columns are `id`, the other keys sorted and
//! the system properties last, the same on every run. A key that shows up
//! only in the second pass (the container changed meanwhile) fails the
//! read instead of being dropped. Values keep their JSON type; nested
//! objects and arrays become JSON cells. A key present with `null` and a
//! missing key are both a null cell: that is what every other target reads
//! as NULL (a JSON `null` cell would land as the text `null` or fail a
//! typed column). Rows can't tell them apart, so Cosmos DB to Cosmos DB
//! goes through [`copy_native`], which moves the items themselves and
//! keeps explicit nulls (an explicit `null` partition key is not the
//! "undefined" one). The filter is a Cosmos SQL condition over the item `c`.
//!
//! Loading: rows are grouped by their partition key value, at most
//! [`WINDOW`] rows or [`WINDOW_BYTES`] of items at once; a group goes as one
//! **transactional batch** (up to [`BATCH_OPS`] creates of one partition
//! key value in one request, all or nothing) and a lone item as a point
//! create (`POST …/docs`), up to [`IN_FLIGHT`] requests and
//! [`MAX_INFLIGHT_BYTES`] of items at once. Batches cut the requests (the
//! emulator does ~400 point creates/s but a 100-item batch in ~0.1 s);
//! point creates keep items whose partition key value doesn't repeat nearby
//! from waiting. Throttling (429, for the request or for any operation of a
//! batch, which then applied nothing) is retried after the server's
//! `x-ms-retry-after-ms`, as the SDKs do, so the load goes at the
//! container's provisioned RU/s; "retry with" (449, a write that lost a
//! race and applied nothing) too. On an error, or when the load is
//! cancelled (its future dropped), no request is started or retried again
//! and the ones already sent are waited for before returning: nothing is
//! written after the load ends. A request left without a final answer (no
//! reply, a dropped connection, or the service's own timeout or
//! unavailability, 408 / 5xx, which Cosmos DB documents as "may or may not
//! have been applied") may still be applied by the server, and the error
//! says so.
//!
//! [`copy_native`] (Cosmos DB to Cosmos DB) feeds the source's items to
//! the same writer, only renamed or projected when the copy asks for
//! columns, with the read's budget cut so the whole stays within ~32 MiB.
//!
//! A row means what it means in the insert script: null cells and system
//! properties left out, JSON `null` cells kept as `null`, `id` always sent
//! as text (Cosmos DB requires it; a row without one fails), JSON cells
//! nested, whole numbers as numbers, binaries as `0x…` hex. An item over
//! Cosmos DB's 2 MB is refused before sending, naming its `id`. A create
//! never replaces an item: an existing `id` in its partition fails the load
//! (409).

use crate::{auth_header, ddl, enc, error_message, http_date, CosmosSession, API_VERSION, SYSTEM_PROPS};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, CopySpec, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{kinds, Error, Result, Session};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

/// Items per read page.
const PAGE: usize = 1_000;
/// Partition key ranges read at once.
const READ_RANGES: usize = 8;
/// The largest query page Cosmos DB answers.
const PAGE_MAX: usize = 4 << 20;
/// Page text a read holds at once (parsed, roughly twice that).
const READ_BUDGET: usize = 16 << 20;
/// A native copy's read budget: it also holds the writer's items.
const COPY_READ_BUDGET: usize = 8 << 20;
/// Requests (batches or point creates) in flight at once…
pub(crate) const IN_FLIGHT: usize = 32;
/// …and item bytes in them. The body and the request's copy of it double
/// it: with [`WINDOW_BYTES`] waiting and the batch at hand, ~32 MiB per
/// table.
const MAX_INFLIGHT_BYTES: usize = 8 << 20;
/// Operations per transactional batch (the service's limit).
const BATCH_OPS: usize = 100;
/// A batch's body stays under the 2 MB request limit.
const BATCH_BYTES: usize = 1_800_000;
/// Rows grouped by partition key value before the groups are sent…
const WINDOW: usize = 5_000;
/// …or item bytes, whichever comes first.
const WINDOW_BYTES: usize = 8 << 20;
/// Cosmos DB's item size limit.
const MAX_ITEM: usize = 2 * 1024 * 1024;
/// Retries of a throttled (429) request before giving up.
const MAX_THROTTLED: u32 = 60;
/// One request (the SDKs' gateway timeout).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(65);
/// Longest wait for the requests already sent when a load fails or is
/// cancelled (one request's timeout and some margin). Waited once: what
/// is still unanswered then is abandoned and reported.
const DRAIN: Duration = Duration::from_secs(70);
/// Added to a load error when a request may still be applied.
const UNSURE: &str = "quedaron pedidos a Cosmos DB sin respuesta: el servidor podría escribir ítems después";

/// What a request needs, cheap to clone into tasks.
#[derive(Clone)]
struct Rest {
    http: reqwest::Client,
    base: String,
    key: String,
    /// Set when a load fails or is cancelled: no request starts or is
    /// retried again.
    stop: Arc<AtomicBool>,
}

struct Reply {
    body: Value,
    continuation: Option<String>,
    /// The reply's size as received.
    bytes: usize,
}

/// A request that failed; `unsure` when it may have been applied anyway
/// (no answer: a timeout, a connection dropped after sending).
#[derive(Debug)]
struct Failed {
    err: Error,
    unsure: bool,
}

fn sure(err: Error) -> Failed {
    Failed { err, unsure: false }
}

/// A status the service may answer while the write still goes through:
/// its own timeout (408) or unavailability (503, any 5xx).
fn maybe_applied(status: u16) -> bool {
    status == 408 || status >= 500
}

/// "Retry with": a write that lost a race and applied nothing; safe to send again.
const RETRY_WITH: u16 = 449;

impl Reply {
    /// A query page's items.
    fn items(self) -> Vec<Value> {
        match self.body {
            Value::Object(mut o) => match o.remove("Documents") {
                Some(Value::Array(d)) => d,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

/// What a transactional batch's reply says.
#[derive(Debug, PartialEq)]
enum BatchOutcome {
    Done,
    /// An operation was throttled (429) or told to retry (449): nothing
    /// was applied, retry.
    Throttled(Option<f64>),
    /// The error, and whether the batch may still be applied (an
    /// operation timed out or found the service unavailable).
    Failed(String, bool),
}

/// Each operation's `statusCode`: all 2xx is done; the failing operation
/// (the others say 424, "failed dependency") names the error.
fn batch_outcome(body: &Value, ids: &[String]) -> BatchOutcome {
    let Some(ops) = body.as_array() else { return BatchOutcome::Failed(format!("respuesta inesperada del lote: {body}"), false) };
    let code = |op: &Value| op.get("statusCode").and_then(Value::as_u64).unwrap_or(0);
    let unsure = ops.iter().any(|op| maybe_applied(code(op) as u16));
    if !unsure {
        if let Some(t) = ops.iter().find(|op| code(op) == 429 || code(op) == RETRY_WITH as u64) {
            return BatchOutcome::Throttled(t.get("retryAfterMilliseconds").and_then(Value::as_f64));
        }
    }
    match ops.iter().enumerate().find(|(_, op)| !(200..300).contains(&code(op)) && code(op) != 424) {
        None if ops.iter().all(|op| (200..300).contains(&code(op))) => BatchOutcome::Done,
        None => BatchOutcome::Failed("el lote no se aplicó".into(), unsure),
        Some((i, op)) => {
            let id = ids.get(i).map(String::as_str).unwrap_or("?");
            let msg = match code(op) {
                409 => format!("ya existe un ítem con el id «{id}» en su partición"),
                c => format!("el ítem «{id}» falló con el estado {c}"),
            };
            BatchOutcome::Failed(msg, unsure)
        }
    }
}

impl Rest {
    /// One signed request, retried while throttled (429). Returns the
    /// status (a success) and the parsed reply with its continuation.
    async fn send(&self, method: Method, rtype: &str, link: &str, path: &str, body: Option<&str>, headers: &[(&str, String)]) -> std::result::Result<Reply, Failed> {
        let (status, reply) = self.send_raw(method, rtype, link, path, body, headers).await?;
        if status.is_success() {
            return Ok(reply);
        }
        let msg = error_message(&reply.body);
        let err = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::AuthFailed(msg),
            StatusCode::CONFLICT => Error::Query(format!("ya existe un ítem con ese id en su partición: {msg}")),
            s => Error::Query(format!("Cosmos DB respondió {}: {msg}", s.as_u16())),
        };
        Err(Failed { err, unsure: maybe_applied(status.as_u16()) })
    }

    /// [`Self::send`] without turning a failure status into an error. Once
    /// [`Rest::stop`] is set it sends nothing (not even a retry).
    async fn send_raw(
        &self,
        method: Method,
        rtype: &str,
        link: &str,
        path: &str,
        body: Option<&str>,
        headers: &[(&str, String)],
    ) -> std::result::Result<(StatusCode, Reply), Failed> {
        let mut throttled = 0;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Err(sure(Error::Cancelled));
            }
            let date = http_date();
            let mut rq = self
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .header("Authorization", auth_header(&self.key, method.as_str(), rtype, link, &date).map_err(sure)?)
                .header("x-ms-date", date)
                .header("x-ms-version", API_VERSION)
                .header("Accept", "application/json")
                .timeout(REQUEST_TIMEOUT);
            for (k, v) in headers {
                rq = rq.header(*k, v);
            }
            if let Some(b) = body {
                rq = rq.body(b.to_string());
            }
            // Without an answer the server may have taken the request
            // (unless it never connected).
            let resp = rq.send().await.map_err(|e| Failed { unsure: !e.is_connect(), err: Error::Connect(e.to_string()) })?;
            let status = resp.status();
            let h = resp.headers();
            let continuation = h.get("x-ms-continuation").and_then(|v| v.to_str().ok()).filter(|c| !c.is_empty()).map(str::to_string);
            let retry_ms = h.get("x-ms-retry-after-ms").and_then(|v| v.to_str().ok()?.trim().parse::<f64>().ok());
            let text = resp.text().await.map_err(|e| Failed { unsure: true, err: Error::Connect(e.to_string()) })?;
            if (status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == RETRY_WITH) && throttled < MAX_THROTTLED {
                throttled += 1;
                tokio::time::sleep(retry_delay(retry_ms, throttled)).await;
                continue;
            }
            let bytes = text.len();
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
            return Ok((status, Reply { body, continuation, bytes }));
        }
    }

    /// Create one item.
    async fn create(self, link: String, path: String, pk: String, item: String) -> std::result::Result<u64, Failed> {
        let h = [("Content-Type", "application/json".to_string()), ("x-ms-documentdb-partitionkey", pk)];
        self.send(Method::POST, "docs", &link, &path, Some(&item), &h).await?;
        Ok(1)
    }

    /// Create items of one partition key value in a transactional batch.
    async fn batch(self, link: String, path: String, pk: String, group: Group) -> std::result::Result<u64, Failed> {
        let Group { ids, items, bytes } = group;
        let n = items.len() as u64;
        let mut body = String::with_capacity(bytes + items.len() * 48 + 2);
        body.push('[');
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            body.push_str("{\"operationType\":\"Create\",\"resourceBody\":");
            body.push_str(&item);
            body.push('}');
        }
        body.push(']');
        let h = [
            ("Content-Type", "application/json".to_string()),
            ("x-ms-documentdb-partitionkey", pk),
            ("x-ms-cosmos-is-batch-request", "True".to_string()),
            ("x-ms-cosmos-batch-atomic", "True".to_string()),
            ("x-ms-cosmos-batch-continue-on-error", "False".to_string()),
        ];
        let mut throttled = 0;
        loop {
            let (status, reply) = self.send_raw(Method::POST, "docs", &link, &path, Some(&body), &h).await?;
            if !reply.body.is_array() {
                // Refused as a whole (auth, size, the container…), or the
                // service timed out / was unavailable (maybe applied).
                let err = match status {
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::AuthFailed(error_message(&reply.body)),
                    s => Error::Query(format!("lote transaccional ({}): {}", s.as_u16(), error_message(&reply.body))),
                };
                return Err(Failed { err, unsure: maybe_applied(status.as_u16()) });
            }
            match batch_outcome(&reply.body, &ids) {
                BatchOutcome::Done => return Ok(n),
                BatchOutcome::Throttled(ms) if throttled < MAX_THROTTLED => {
                    if self.stop.load(Ordering::SeqCst) {
                        return Err(sure(Error::Cancelled));
                    }
                    throttled += 1;
                    tokio::time::sleep(retry_delay(ms, throttled)).await;
                }
                BatchOutcome::Throttled(_) => return Err(sure(Error::Query("Cosmos DB siguió limitando el lote (429)".into()))),
                BatchOutcome::Failed(m, unsure) => return Err(Failed { err: Error::Query(m), unsure: unsure || maybe_applied(status.as_u16()) }),
            }
        }
    }

    /// Every page of a query on one partition key range, into `tx`. Each
    /// page reserves [`PAGE_MAX`] of `budget` before its request and keeps
    /// its size until the consumer drops it.
    async fn read_range(self, link: String, path: String, sql: String, range: Option<String>, budget: Arc<Semaphore>, tx: mpsc::Sender<Result<Page>>) {
        let body = json!({ "query": sql, "parameters": [] }).to_string();
        let mut cont: Option<String> = None;
        loop {
            let Ok(mut permit) = budget.clone().acquire_many_owned(PAGE_MAX as u32).await else { return };
            let mut h = vec![
                ("x-ms-documentdb-isquery", "True".to_string()),
                ("Content-Type", "application/query+json".to_string()),
                ("x-ms-documentdb-query-enablecrosspartition", "True".to_string()),
                ("x-ms-max-item-count", PAGE.to_string()),
            ];
            if let Some(r) = &range {
                h.push(("x-ms-documentdb-partitionkeyrangeid", r.clone()));
            }
            if let Some(c) = &cont {
                h.push(("x-ms-continuation", c.clone()));
            }
            match self.send(Method::POST, "docs", &link, &path, Some(&body), &h).await {
                Ok(p) => {
                    let next = p.continuation.clone();
                    // Keep what the page takes (its text; a page over the
                    // maximum keeps the whole reservation).
                    let keep = p.bytes.clamp(1, PAGE_MAX);
                    drop(permit.split(PAGE_MAX - keep));
                    if tx.send(Ok(Page { items: p.items(), _permit: permit })).await.is_err() {
                        return;
                    }
                    match next {
                        Some(c) => cont = Some(c),
                        None => return,
                    }
                }
                Err(f) => {
                    let _ = tx.send(Err(f.err)).await;
                    return;
                }
            }
        }
    }
}

/// A read page and its share of the read's budget.
struct Page {
    items: Vec<Value>,
    _permit: OwnedSemaphorePermit,
}

/// What a read queries: the container's link and docs path, the query,
/// and its partition key ranges.
struct ReadPlan {
    link: String,
    path: String,
    sql: String,
    ranges: Vec<Option<String>>,
}

/// Every page of `sql` (one reader per partition key range,
/// [`READ_RANGES`] at once) to `each`, as they arrive.
async fn scan(rest: &Rest, plan: &ReadPlan, mut each: impl FnMut(Vec<Value>) -> Result<()>) -> Result<()> {
    let (mut rx, _guard) = pages(rest, plan, READ_BUDGET);
    while let Some(page) = rx.recv().await {
        each(page?.items)?;
    }
    Ok(())
}

/// The pages of a read as they arrive, `budget` bytes of page text at
/// most. Dropping the guard (or the receiver) stops the readers.
fn pages(rest: &Rest, plan: &ReadPlan, budget: usize) -> (mpsc::Receiver<Result<Page>>, AbortOnDrop) {
    let budget = Arc::new(Semaphore::new(budget));
    let (tx, rx) = mpsc::channel::<Result<Page>>(READ_RANGES * 2);
    let (rest, link, path, sql, ranges) = (rest.clone(), plan.link.clone(), plan.path.clone(), plan.sql.clone(), plan.ranges.clone());
    let feeder = tokio::spawn(async move {
        let mut readers = JoinSet::new();
        let failed = |r: Option<std::result::Result<(), tokio::task::JoinError>>| match r {
            Some(Err(e)) => Some(Error::State(format!("lectura interrumpida: {e}"))),
            _ => None,
        };
        for r in ranges {
            if readers.len() >= READ_RANGES {
                if let Some(e) = failed(readers.join_next().await) {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
            readers.spawn(rest.clone().read_range(link.clone(), path.clone(), sql.clone(), r, budget.clone(), tx.clone()));
        }
        while let Some(r) = readers.join_next().await {
            // A reader that panicked would leave its range's pages out.
            if let Some(e) = failed(Some(r)) {
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    });
    // Returning (an error included) drops the receiver and aborts the
    // readers.
    (rx, AbortOnDrop(feeder))
}

/// A read's column names: `id`, the other keys sorted, the system
/// properties last.
#[derive(Default)]
struct Keys {
    user: BTreeSet<String>,
    system: BTreeSet<String>,
}

impl Keys {
    fn add(&mut self, item: &Value) {
        let Some(o) = item.as_object() else { return };
        for k in o.keys() {
            let set = if SYSTEM_PROPS.contains(&k.as_str()) { &mut self.system } else { &mut self.user };
            if k != "id" && !set.contains(k) {
                set.insert(k.clone());
            }
        }
    }

    /// `id` even for an empty container: every item has one.
    fn names(self) -> Vec<String> {
        std::iter::once("id".to_string()).chain(self.user).chain(self.system).collect()
    }
}

/// How long to wait before retrying a throttled request: the server's
/// hint, else a growing backoff.
fn retry_delay(hint_ms: Option<f64>, attempt: u32) -> Duration {
    match hint_ms.filter(|m| m.is_finite() && *m >= 0.0) {
        Some(ms) => Duration::from_millis(ms.ceil() as u64 + 5),
        None => Duration::from_millis((50u64 << attempt.min(6)).min(5_000)),
    }
}

/// A JSON value as a cell: nested values as JSON.
pub(crate) fn to_cell(v: &Value) -> Cell {
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

/// An item's row: a missing key and a key present with `null` are both a
/// null cell, the NULL every target understands (Cosmos DB to Cosmos DB,
/// where they differ, goes through [`copy_native`]).
fn item_row(item: &Value, names: &[String]) -> Vec<Cell> {
    names.iter().map(|n| item.get(n).map_or(Cell::Null, to_cell)).collect()
}

/// A source item as the native copy writes it: system properties left
/// out, or only the asked-for keys (`(source, target)`), renamed; explicit
/// nulls kept, missing keys left missing. `id` as text, as [`row_item`].
fn copy_item(item: Value, pairs: Option<&[(String, String)]>) -> Result<Value> {
    let Value::Object(mut o) = item else { return Err(Error::Query(format!("Cosmos DB devolvió un ítem que no es un objeto: {item}"))) };
    let mut m = match pairs {
        None => {
            o.retain(|k, _| !SYSTEM_PROPS.contains(&k.as_str()));
            o
        }
        Some(p) => p
            .iter()
            .filter(|(_, to)| !SYSTEM_PROPS.contains(&to.as_str()))
            .filter_map(|(from, to)| Some((to.clone(), o.get(from)?.clone())))
            .collect(),
    };
    match m.get("id") {
        Some(Value::String(_)) => {}
        Some(v) if !v.is_null() => {
            let s = v.to_string();
            m.insert("id".into(), Value::String(s));
        }
        _ => {
            return Err(Error::Query("Cada ítem necesita un campo «id» de texto: la copia no tiene la columna id o el ítem la tiene vacía.".into()))
        }
    }
    Ok(Value::Object(m))
}

/// A row as the item the insert script would write.
pub(crate) fn row_item(names: &[String], row: &[Cell]) -> Result<Value> {
    let m: serde_json::Map<String, Value> = names
        .iter()
        .zip(row)
        .filter(|(n, c)| !matches!(c, Cell::Null) && !SYSTEM_PROPS.contains(&n.as_str()))
        .map(|(n, c)| {
            let v = match (n.as_str(), c) {
                ("id", _) => match c.to_json() {
                    Value::String(s) => Value::String(s),
                    // A JSON `null` id is no id (the check below).
                    Value::Null => Value::Null,
                    other => Value::String(other.to_string()),
                },
                // Whole numbers as numbers, even past 2^53 (the grid shows those as text).
                (_, Cell::Int(i)) => Value::from(*i),
                (_, Cell::UInt(u)) => Value::from(*u),
                _ => c.to_json(),
            };
            (n.clone(), v)
        })
        .collect();
    if !m.get("id").is_some_and(Value::is_string) {
        return Err(Error::Query("Cada ítem necesita un campo «id» de texto: la carga no tiene la columna id o la fila la tiene vacía.".into()));
    }
    Ok(Value::Object(m))
}

/// Items of one partition key value waiting to be sent, as JSON text.
#[derive(Default)]
struct Group {
    ids: Vec<String>,
    items: Vec<String>,
    bytes: usize,
}

impl Group {
    fn push(&mut self, id: String, item: String) {
        self.bytes += item.len();
        self.ids.push(id);
        self.items.push(item);
    }
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

impl CosmosSession {
    fn rest(&self) -> Rest {
        Rest { http: self.http.clone(), base: self.base.clone(), key: self.key.clone(), stop: Arc::new(AtomicBool::new(false)) }
    }

    /// What reading a container with `filter` queries.
    async fn read_plan(&self, coll: &str, filter: Option<&str>) -> Result<ReadPlan> {
        let link = format!("{}/colls/{coll}", self.db_link()?);
        let path = format!("{}/colls/{}/docs", self.db_path()?, enc(coll));
        let sql = match filter.map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) => format!("SELECT * FROM c WHERE {f}"),
            None => "SELECT * FROM c".to_string(),
        };
        // One reader per partition key range; the gateway's cross-partition
        // query when they can't be listed.
        let ranges_path = format!("{}/colls/{}/pkranges", self.db_path()?, enc(coll));
        let ranges: Vec<Option<String>> = match self.list(&ranges_path, "pkranges", &link, "PartitionKeyRanges").await {
            Ok(r) if !r.is_empty() => r.iter().filter_map(|r| r.get("id")?.as_str().map(|s| Some(s.to_string()))).collect(),
            _ => vec![None],
        };
        Ok(ReadPlan { link, path, sql, ranges })
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        if spec.table.kind != kinds::COLLECTION {
            return dbine_driver::transfer::read_via_execute(self, spec, sink).await;
        }
        let plan = self.read_plan(&spec.table.name, spec.filter.as_deref()).await?;
        let rest = self.rest();

        // Without asked-for columns, a first pass learns every key.
        let (names, found) = match &spec.columns {
            Some(n) => (n.clone(), None),
            None => {
                let mut keys = Keys::default();
                scan(&rest, &plan, |items| {
                    items.iter().for_each(|it| keys.add(it));
                    Ok(())
                })
                .await?;
                let names = keys.names();
                let found: HashSet<String> = names.iter().cloned().collect();
                (names, Some(found))
            }
        };
        let cols: Vec<TransferColumn> = names
            .iter()
            .map(|n| TransferColumn { name: n.clone(), type_name: if n == "id" { "string".into() } else { String::new() }, nullable: n != "id" })
            .collect();
        sink.lock().map_err(lock_err)?.begin(&cols)?;
        let mut builder = BatchBuilder::new();
        scan(&rest, &plan, |items| {
            let mut s = sink.lock().map_err(lock_err)?;
            for it in items {
                if let (Some(found), Some(o)) = (&found, it.as_object()) {
                    if let Some(k) = o.keys().find(|k| !found.contains(*k)) {
                        let id = o.get("id").and_then(Value::as_str).unwrap_or("?");
                        return Err(Error::Query(format!(
                            "El ítem «{id}» tiene el campo «{k}», que no estaba al leer las columnas: el contenedor cambió durante la copia. Volvé a copiarlo."
                        )));
                    }
                }
                builder.push(item_row(&it, &names), &mut *s)?;
            }
            Ok(())
        })
        .await?;
        builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(
        &mut self,
        spec: &LoadSpec,
        columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<u64> {
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        self.write(spec, Feed::Rows { names, source }, progress).await
    }

    /// Create `feed`'s items in `spec.table`; returns the items written.
    async fn write(&mut self, spec: &LoadSpec, mut feed: Feed<'_>, progress: Progress<'_>) -> Result<u64> {
        self.check_writable()?;
        if spec.table.kind != kinds::COLLECTION {
            return Err(Error::Unsupported("en Cosmos DB solo se cargan ítems en un contenedor".into()));
        }
        let coll = spec.table.name.clone();
        let pk_paths = self.partition_paths(&coll).await?;
        let link = format!("{}/colls/{coll}", self.db_link()?);
        let path = format!("{}/colls/{}/docs", self.db_path()?, enc(&coll));

        // Progress every `commit_rows` rows or `commit_bytes` bytes written.
        let (every_rows, every_bytes) = (spec.commit_rows.max(1), spec.commit_bytes.max(1));
        let (mut done, mut reported, mut bytes) = (0u64, 0u64, 0u64);
        let mut tick = |rows: u64, b: u64, force: bool| {
            done += rows;
            bytes += b;
            if done > reported && (force || done - reported >= every_rows || bytes >= every_bytes) {
                reported = done;
                bytes = 0;
                progress(done);
            }
        };
        let mut w = Writer { rest: self.rest(), link, path, coll, set: JoinSet::new(), bytes: 0 };
        // Dropping the load (cancelled) drops `w`, which waits for what
        // was sent.
        if let Err(f) = load(&mut w, &pk_paths, &mut feed, &mut tick).await {
            // Nothing may be written once the load has returned; what the
            // requests still out committed counts as progress.
            let d = w.drain().await;
            tick(d.rows, 0, true);
            if f.unsure || d.unsure || !d.answered {
                return Err(Error::Query(format!("{} ({UNSURE})", f.err)));
            }
            return Err(f.err);
        }
        tick(0, 0, true);
        Ok(done)
    }
}

fn cosmos(s: &mut dyn Session) -> Option<&mut CosmosSession> {
    s.as_any()?.downcast_mut::<CosmosSession>()
}

/// Between two Cosmos DB sessions (other accounts or databases included):
/// the source's items go to the target's writer as they are, so explicit
/// nulls, missing keys and every JSON type survive (rows would merge the
/// first two). `source` is only queried. Asked-for columns are picked (and
/// renamed to the target's) from each item.
pub(crate) async fn copy_native(source: &mut dyn Session, target: &mut dyn Session, spec: &CopySpec, progress: Progress<'_>) -> Result<u64> {
    let unsupported = |why: &str| Err(Error::Unsupported(format!("copia directa no disponible: {why}")));
    if spec.source.table.kind != kinds::COLLECTION || spec.target.table.kind != kinds::COLLECTION {
        return unsupported("solo entre contenedores");
    }
    let target_cols = &spec.target.columns;
    let pairs: Option<Vec<(String, String)>> = match spec.source.columns.as_ref().filter(|c| !c.is_empty()) {
        Some(from) if target_cols.is_empty() || target_cols.len() == from.len() => {
            let to = if target_cols.is_empty() { from } else { target_cols };
            Some(from.iter().cloned().zip(to.iter().cloned()).collect())
        }
        Some(from) => return Err(Error::State(format!("la copia lee {} columnas y carga {}", from.len(), target_cols.len()))),
        None if !target_cols.is_empty() => Some(target_cols.iter().map(|c| (c.clone(), c.clone())).collect()),
        None => None,
    };
    if let Some(p) = &pairs {
        let mut seen = HashSet::new();
        if let Some((_, t)) = p.iter().find(|(_, t)| !seen.insert(t.as_str())) {
            return Err(Error::State(format!("la columna de destino «{t}» está repetida")));
        }
    }
    let Some(src) = cosmos(source) else { return unsupported("el origen no es una sesión de Cosmos DB") };
    let plan = src.read_plan(&spec.source.table.name, spec.source.filter.as_deref()).await?;
    let (pages, reader) = pages(&src.rest(), &plan, COPY_READ_BUDGET);
    let Some(dst) = cosmos(target) else { return unsupported("el destino no es una sesión de Cosmos DB") };
    dst.write(&spec.target, Feed::Items { pages, pairs, _reader: reader }, progress).await
}

/// A load's requests in flight.
struct Writer {
    rest: Rest,
    link: String,
    path: String,
    coll: String,
    /// Each request's rows and item bytes.
    set: JoinSet<std::result::Result<(u64, usize), Failed>>,
    /// Item bytes in flight.
    bytes: usize,
}

type Tick<'a> = &'a mut (dyn FnMut(u64, u64, bool) + Send);

impl Writer {
    /// A finished request's rows and item bytes.
    fn done(&mut self, r: std::result::Result<std::result::Result<(u64, usize), Failed>, tokio::task::JoinError>) -> std::result::Result<(u64, u64), Failed> {
        // A task that panicked may have sent its request.
        let (rows, bytes) = r.map_err(|e| Failed { err: Error::State(format!("creación de ítems interrumpida: {e}")), unsure: true })??;
        self.bytes -= bytes;
        Ok((rows, bytes as u64))
    }

    /// Send a group once it fits in flight; `tick` gets the rows written
    /// meanwhile.
    async fn send(&mut self, pk: String, g: Group, tick: Tick<'_>) -> std::result::Result<(), Failed> {
        let cost = g.bytes;
        while !self.set.is_empty() && (self.set.len() >= IN_FLIGHT || self.bytes + cost > MAX_INFLIGHT_BYTES) {
            if let Some(r) = self.set.join_next().await {
                let (rows, bytes) = self.done(r)?;
                tick(rows, bytes, false);
            }
        }
        self.bytes += cost;
        let (r, link, path) = (self.rest.clone(), self.link.clone(), self.path.clone());
        if g.items.len() == 1 {
            let item = g.items.into_iter().next().unwrap_or_default();
            self.set.spawn(async move { r.create(link, path, pk, item).await.map(|n| (n, cost)) });
        } else {
            self.set.spawn(async move { r.batch(link, path, pk, g).await.map(|n| (n, cost)) });
        }
        Ok(())
    }

    /// What finished, without waiting.
    fn reap(&mut self, tick: Tick<'_>) -> std::result::Result<(), Failed> {
        while let Some(r) = self.set.try_join_next() {
            let (rows, bytes) = self.done(r)?;
            tick(rows, bytes, false);
        }
        Ok(())
    }

    /// Stop starting or retrying requests and wait for the ones sent, at
    /// most [`DRAIN`]. Whatever is still out then is abandoned (reported,
    /// never waited for again, not even by the drop).
    async fn drain(&mut self) -> Drained {
        self.drain_within(DRAIN).await
    }

    async fn drain_within(&mut self, limit: Duration) -> Drained {
        self.rest.stop.store(true, Ordering::SeqCst);
        let set = &mut self.set;
        let (mut unsure, mut rows) = (false, 0u64);
        let answered = tokio::time::timeout(limit, async {
            while let Some(r) = set.join_next().await {
                match r {
                    Ok(Ok((n, _))) => rows += n,
                    Ok(Err(f)) => unsure |= f.unsure,
                    Err(_) => unsure = true,
                }
            }
        })
        .await
        .is_ok();
        if !answered {
            self.set = JoinSet::new();
        }
        self.bytes = 0;
        Drained { answered, unsure, rows }
    }
}

/// What [`Writer::drain`] found.
struct Drained {
    /// Every request sent answered within [`DRAIN`].
    answered: bool,
    /// One may have been applied without saying so.
    unsure: bool,
    /// Rows the requests committed meanwhile.
    rows: u64,
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.set.is_empty() || std::thread::panicking() {
            return;
        }
        use tokio::runtime::{Handle, RuntimeFlavor};
        match Handle::try_current() {
            Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => {
                let d = tokio::task::block_in_place(|| h.block_on(self.drain()));
                if !d.answered || d.unsure {
                    tracing::warn!(container = %self.coll, "Cosmos DB load cancelled with requests left unanswered: items may still be written");
                }
            }
            // No worker to wait on: the tasks are aborted, and requests
            // already sent may still be written.
            _ => {
                self.rest.stop.store(true, Ordering::SeqCst);
                tracing::warn!(container = %self.coll, "Cosmos DB load cancelled outside a multi-thread runtime; sent requests not awaited");
            }
        }
    }
}

/// What a load writes: rows (as the insert script would), or a native
/// copy's source items.
enum Feed<'a> {
    Rows {
        names: Vec<String>,
        source: &'a mut dyn BatchSource,
    },
    Items {
        pages: mpsc::Receiver<Result<Page>>,
        /// `(source, target)` keys; `None`: the whole item.
        pairs: Option<Vec<(String, String)>>,
        /// Stops the source's readers when the load ends.
        _reader: AbortOnDrop,
    },
}

/// Items grouped by partition key value until they are sent.
#[derive(Default)]
struct Queue {
    groups: HashMap<String, Group>,
    /// Rows and item bytes waiting in `groups`.
    rows: usize,
    bytes: usize,
}

impl Queue {
    /// Queue one item, sending what is full.
    async fn push(&mut self, w: &mut Writer, pk_paths: &[String], item: Value, tick: Tick<'_>) -> std::result::Result<(), Failed> {
        let pk = ddl::partition_key_header(&item, pk_paths);
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let text = item.to_string();
        drop(item);
        if text.len() > MAX_ITEM {
            return Err(sure(Error::Query(format!("El ítem «{id}» ocupa {} bytes y Cosmos DB admite hasta {MAX_ITEM} por ítem.", text.len()))));
        }
        let g = self.groups.entry(pk.clone()).or_default();
        if !g.items.is_empty() && g.bytes + text.len() > BATCH_BYTES {
            let full = std::mem::take(g);
            self.rows -= full.items.len();
            self.bytes -= full.bytes;
            w.send(pk.clone(), full, tick).await?;
        }
        let g = self.groups.entry(pk.clone()).or_default();
        self.rows += 1;
        self.bytes += text.len();
        g.push(id, text);
        if g.items.len() >= BATCH_OPS {
            let full = self.groups.remove(&pk).unwrap_or_default();
            self.rows -= full.items.len();
            self.bytes -= full.bytes;
            w.send(pk, full, tick).await?;
        }
        if self.rows >= WINDOW || self.bytes >= WINDOW_BYTES {
            self.flush(w, tick).await?;
        }
        Ok(())
    }

    /// Send every group.
    async fn flush(&mut self, w: &mut Writer, tick: Tick<'_>) -> std::result::Result<(), Failed> {
        for (pk, g) in self.groups.drain() {
            w.send(pk, g, tick).await?;
        }
        (self.rows, self.bytes) = (0, 0);
        Ok(())
    }
}

/// `feed`'s items as creates; `tick` gets the rows written.
async fn load(w: &mut Writer, pk_paths: &[String], feed: &mut Feed<'_>, tick: Tick<'_>) -> std::result::Result<(), Failed> {
    let mut q = Queue::default();
    match feed {
        Feed::Rows { names, source } => {
            while let Some(batch) = source.next().await {
                for row in batch.rows {
                    if row.len() != names.len() {
                        return Err(sure(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len()))));
                    }
                    let item = row_item(names, &row).map_err(sure)?;
                    drop(row);
                    q.push(w, pk_paths, item, tick).await?;
                }
                w.reap(tick)?;
            }
        }
        Feed::Items { pages, pairs, .. } => {
            while let Some(page) = pages.recv().await {
                // The page keeps its share of the read's budget until its
                // items are queued as text.
                let page = page.map_err(sure)?;
                for it in page.items {
                    q.push(w, pk_paths, copy_item(it, pairs.as_deref()).map_err(sure)?, tick).await?;
                }
                drop(page._permit);
                w.reap(tick)?;
            }
        }
    }
    q.flush(w, tick).await?;
    while let Some(r) = w.set.join_next().await {
        let (rows, bytes) = w.done(r)?;
        tick(rows, bytes, false);
    }
    Ok(())
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rows_become_items() {
        let row = vec![
            Cell::Int(7),
            Cell::Text("etag".into()),
            Cell::Text("Dune".into()),
            Cell::Json("{\"x\":[1,2]}".into()),
            Cell::Null,
            Cell::Bytes(vec![1, 255]),
            Cell::Decimal("1.50".into()),
            Cell::UInt(u64::MAX),
        ];
        let item = row_item(&names(&["id", "_etag", "title", "meta", "gone", "blob", "price", "big"]), &row).unwrap();
        assert_eq!(item, json!({"id": "7", "title": "Dune", "meta": {"x": [1, 2]}, "blob": "0x01FF", "price": "1.50", "big": u64::MAX}));
        assert!(row_item(&names(&["name"]), &[Cell::Text("x".into())]).is_err());
        assert!(row_item(&names(&["id", "a"]), &[Cell::Null, Cell::Int(1)]).is_err());
        assert_eq!(ddl::partition_key_header(&item, &["/meta/x".into()]), "[[1,2]]");
        assert_eq!(ddl::partition_key_header(&item, &["/title".into()]), "[\"Dune\"]");
    }

    #[test]
    fn items_become_rows() {
        let item = json!({"id": "a", "_ts": 5, "n": 5, "f": 1.5, "big": u64::MAX, "ok": false, "tags": ["p"], "o": {"k": null}});
        let cols = names(&["id", "n", "f", "big", "ok", "tags", "o", "_ts", "missing"]);
        assert_eq!(
            item_row(&item, &cols),
            vec![
                Cell::Text("a".into()),
                Cell::Int(5),
                Cell::Float(1.5),
                Cell::UInt(u64::MAX),
                Cell::Bool(false),
                Cell::Json("[\"p\"]".into()),
                Cell::Json("{\"k\":null}".into()),
                Cell::Int(5),
                Cell::Null,
            ]
        );
        // And back: the same item, less the system properties.
        assert_eq!(
            row_item(&cols, &item_row(&item, &cols)).unwrap(),
            json!({"id": "a", "n": 5, "f": 1.5, "big": u64::MAX, "ok": false, "tags": ["p"], "o": {"k": null}})
        );
    }

    #[test]
    fn explicit_nulls_read_as_null_for_every_target() {
        // `cat` (the partition key) and `x` present with null, `y` missing:
        // all a null cell, the NULL a SQL target writes (a JSON `null` cell
        // would be the text 'null' or fail an integer column).
        let item = json!({"id": "a", "cat": null, "x": null});
        let cols = names(&["id", "cat", "x", "y"]);
        let row = item_row(&item, &cols);
        assert_eq!(row, vec![Cell::Text("a".into()), Cell::Null, Cell::Null, Cell::Null]);
        // A JSON null id is no id.
        assert!(row_item(&names(&["id"]), &[Cell::Json("null".into())]).is_err());
    }

    #[test]
    fn native_copy_keeps_explicit_nulls() {
        let item = json!({"id": "a", "cat": null, "x": null, "n": 1, "_ts": 5, "_etag": "e"});
        // The whole item, less the system properties.
        let whole = copy_item(item.clone(), None).unwrap();
        assert_eq!(whole, json!({"id": "a", "cat": null, "x": null, "n": 1}));
        // The null partition, not the "undefined" one (`[{}]`).
        assert_eq!(ddl::partition_key_header(&whole, &["/cat".into()]), "[null]");
        assert_eq!(ddl::partition_key_header(&whole, &["/y".into()]), "[{}]");
        // Asked-for keys, renamed; a missing one stays missing.
        let p = vec![("id".to_string(), "id".to_string()), ("x".into(), "equis".into()), ("y".into(), "ye".into()), ("_ts".into(), "_ts".into())];
        assert_eq!(copy_item(item.clone(), Some(&p)).unwrap(), json!({"id": "a", "equis": null}));
        // A numeric id as text; none, or null, is refused.
        assert_eq!(copy_item(json!({"id": 7}), None).unwrap(), json!({"id": "7"}));
        assert!(copy_item(json!({"id": null}), None).is_err());
        assert!(copy_item(json!({"n": 1}), None).is_err());
        assert!(copy_item(json!([1]), None).is_err());
    }

    #[test]
    fn keys_are_every_items_in_a_fixed_order() {
        let mut k = Keys::default();
        for it in [json!({"n": 1, "id": "a", "_ts": 1, "_etag": "e"}), json!({"id": "b", "late": 2, "cat": null})] {
            k.add(&it);
        }
        assert_eq!(k.names(), names(&["id", "cat", "late", "n", "_etag", "_ts"]));
        assert_eq!(Keys::default().names(), names(&["id"]));
    }

    #[test]
    fn batch_replies() {
        let ids: Vec<String> = vec!["a".into(), "b".into()];
        assert_eq!(batch_outcome(&json!([{"statusCode": 201}, {"statusCode": 201}]), &ids), BatchOutcome::Done);
        assert_eq!(
            batch_outcome(&json!([{"statusCode": 424}, {"statusCode": 409}]), &ids),
            BatchOutcome::Failed("ya existe un ítem con el id «b» en su partición".into(), false)
        );
        // "Retry with": nothing applied, sent again.
        assert_eq!(batch_outcome(&json!([{"statusCode": 449}, {"statusCode": 424}]), &ids), BatchOutcome::Throttled(None));
        // The service timed out or was unavailable: may be applied.
        assert!(matches!(batch_outcome(&json!([{"statusCode": 408}, {"statusCode": 424}]), &ids), BatchOutcome::Failed(_, true)));
        assert!(matches!(batch_outcome(&json!([{"statusCode": 424}, {"statusCode": 503}]), &ids), BatchOutcome::Failed(_, true)));
        assert_eq!(
            batch_outcome(&json!([{"statusCode": 429, "retryAfterMilliseconds": 30}, {"statusCode": 424}]), &ids),
            BatchOutcome::Throttled(Some(30.0))
        );
        assert!(matches!(batch_outcome(&json!({"code": "x"}), &ids), BatchOutcome::Failed(_, false)));
        assert!(matches!(batch_outcome(&json!([{"statusCode": 424}]), &ids), BatchOutcome::Failed(_, false)));
    }

    #[test]
    fn throttling_waits_as_told() {
        assert_eq!(retry_delay(Some(120.0), 1), Duration::from_millis(125));
        assert_eq!(retry_delay(None, 1), Duration::from_millis(100));
        assert_eq!(retry_delay(None, 30), Duration::from_millis(3_200));
        assert_eq!(retry_delay(Some(f64::NAN), 2), Duration::from_millis(200));
    }

    /// A local HTTP server answering each request with the next reply
    /// (`None`: close the connection without answering); counts requests.
    async fn fake(replies: Vec<Option<(u16, &'static str, &'static str)>>) -> (Rest, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let replies = Arc::new(std::sync::Mutex::new(replies.into_iter()));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (replies, h) = (replies.clone(), h.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        // One request: headers, then Content-Length bytes.
                        let mut chunk = [0u8; 8192];
                        let end = loop {
                            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break i + 4;
                            }
                            match sock.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                        let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                        while buf.len() < end + len {
                            match sock.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        buf.drain(..end + len);
                        h.fetch_add(1, Ordering::SeqCst);
                        let next = replies.lock().unwrap().next().flatten();
                        let Some((code, extra, body)) = next else { return };
                        let r = format!("HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}\r\n{body}", body.len());
                        if sock.write_all(r.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let rest = Rest {
            http: reqwest::Client::new(),
            base: format!("http://{addr}"),
            key: "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==".into(),
            stop: Arc::new(AtomicBool::new(false)),
        };
        (rest, hits)
    }

    fn group(ids: &[&str]) -> Group {
        let mut g = Group::default();
        for id in ids {
            g.push(id.to_string(), json!({ "id": id }).to_string());
        }
        g
    }

    const THROTTLED: (u16, &str, &str) = (429, "x-ms-retry-after-ms: 5\r\n", "{}");

    #[tokio::test]
    async fn throttled_requests_are_retried() {
        let (rest, hits) = fake(vec![Some(THROTTLED), Some(THROTTLED), Some((201, "", "{}"))]).await;
        assert_eq!(rest.create("l".into(), "/p".into(), "[\"k\"]".into(), "{\"id\":\"a\"}".into()).await.unwrap(), 1);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        // A batch with a throttled operation applied nothing: sent again.
        let (rest, hits) = fake(vec![
            Some((207, "", r#"[{"statusCode":429,"retryAfterMilliseconds":5},{"statusCode":424}]"#)),
            Some((200, "", r#"[{"statusCode":201},{"statusCode":201}]"#)),
        ])
        .await;
        assert_eq!(rest.batch("l".into(), "/p".into(), "[\"k\"]".into(), group(&["a", "b"])).await.unwrap(), 2);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_stopped_load_sends_and_retries_nothing() {
        let (rest, hits) = fake(vec![Some((429, "x-ms-retry-after-ms: 300\r\n", "{}")); 10]).await;
        let r = rest.clone();
        let task = tokio::spawn(async move { r.create("l".into(), "/p".into(), "[\"k\"]".into(), "{\"id\":\"a\"}".into()).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        rest.stop.store(true, Ordering::SeqCst);
        let f = task.await.unwrap().unwrap_err();
        assert!(matches!(f.err, Error::Cancelled) && !f.unsure, "{f:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(matches!(rest.batch("l".into(), "/p".into(), "[]".into(), group(&["a", "b"])).await, Err(Failed { err: Error::Cancelled, unsure: false })));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_request_left_unanswered_may_have_been_applied() {
        let (rest, _) = fake(vec![None]).await;
        let f = rest.create("l".into(), "/p".into(), "[\"k\"]".into(), "{\"id\":\"a\"}".into()).await.unwrap_err();
        assert!(f.unsure, "{f:?}");
        // Refused outright: nothing was applied.
        let (rest, _) = fake(vec![Some((409, "", r#"{"message":"conflict"}"#))]).await;
        let f = rest.create("l".into(), "/p".into(), "[\"k\"]".into(), "{\"id\":\"a\"}".into()).await.unwrap_err();
        assert!(!f.unsure && f.err.to_string().contains("ya existe"), "{f:?}");
    }

    /// The service's own timeout or unavailability answers, but the write
    /// may still go through: unsure, for point creates and batches.
    #[tokio::test]
    async fn server_timeouts_may_have_been_applied() {
        for code in [408u16, 503, 500] {
            let (rest, hits) = fake(vec![Some((code, "", r#"{"message":"timeout"}"#))]).await;
            let f = rest.create("l".into(), "/p".into(), "[\"k\"]".into(), "{\"id\":\"a\"}".into()).await.unwrap_err();
            assert!(f.unsure && f.err.to_string().contains(&code.to_string()), "{code}: {f:?}");
            assert_eq!(hits.load(Ordering::SeqCst), 1, "not retried");
            let (rest, _) = fake(vec![Some((code, "", r#"{"message":"timeout"}"#))]).await;
            let f = rest.batch("l".into(), "/p".into(), "[\"k\"]".into(), group(&["a", "b"])).await.unwrap_err();
            assert!(f.unsure, "{code}: {f:?}");
        }
        // A batch answered with an operation that timed out.
        let (rest, _) = fake(vec![Some((207, "", r#"[{"statusCode":408},{"statusCode":424}]"#))]).await;
        let f = rest.batch("l".into(), "/p".into(), "[\"k\"]".into(), group(&["a", "b"])).await.unwrap_err();
        assert!(f.unsure, "{f:?}");
        // A plain refusal is sure.
        let (rest, _) = fake(vec![Some((400, "", r#"{"message":"bad"}"#))]).await;
        let f = rest.batch("l".into(), "/p".into(), "[\"k\"]".into(), group(&["a", "b"])).await.unwrap_err();
        assert!(!f.unsure, "{f:?}");
    }

    /// "Retry with" (449) applied nothing: sent again, like a 429.
    #[tokio::test]
    async fn retry_with_is_retried() {
        let (rest, hits) = fake(vec![Some((449, "", "{}")), Some((201, "", "{}"))]).await;
        assert_eq!(rest.create("l".into(), "/p".into(), "[\"k\"]".into(), "{\"id\":\"a\"}".into()).await.unwrap(), 1);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let (rest, hits) = fake(vec![
            Some((207, "", r#"[{"statusCode":449},{"statusCode":424}]"#)),
            Some((200, "", r#"[{"statusCode":201},{"statusCode":201}]"#)),
        ])
        .await;
        assert_eq!(rest.batch("l".into(), "/p".into(), "[\"k\"]".into(), group(&["a", "b"])).await.unwrap(), 2);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// A failed load waits for the requests it had sent before returning,
    /// and a cancelled one (its future dropped) too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_or_cancelled_loads_wait_for_sent_requests() {
        use std::sync::atomic::AtomicUsize;
        // Requests answer after 300 ms; count the answers given.
        async fn slow() -> (Rest, Arc<AtomicUsize>) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let answered = Arc::new(AtomicUsize::new(0));
            let a = answered.clone();
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let a = a.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 8192];
                        loop {
                            let end = loop {
                                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                    break i + 4;
                                }
                                match sock.read(&mut chunk).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                                }
                            };
                            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                            let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                            while buf.len() < end + len {
                                match sock.read(&mut chunk).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                                }
                            }
                            let failing = String::from_utf8_lossy(&buf[end..end + len]).contains("\"bad\"");
                            buf.drain(..end + len);
                            // The bad item fails at once, the rest answer late.
                            let (code, body) = if failing { (409, r#"{"message":"conflict"}"#) } else {
                                tokio::time::sleep(Duration::from_millis(300)).await;
                                (201, "{}")
                            };
                            a.fetch_add(1, Ordering::SeqCst);
                            let r = format!("HTTP/1.1 {code} X\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                            if sock.write_all(r.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            let rest = Rest {
                http: reqwest::Client::new(),
                base: format!("http://{addr}"),
                key: "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==".into(),
                stop: Arc::new(AtomicBool::new(false)),
            };
            (rest, answered)
        }
        fn writer(rest: Rest) -> Writer {
            Writer { rest, link: "l".into(), path: "/p".into(), coll: "c".into(), set: JoinSet::new(), bytes: 0 }
        }
        struct Rows(std::vec::IntoIter<dbine_driver::transfer::RowBatch>);
        #[dbine_driver::async_trait]
        impl BatchSource for Rows {
            async fn next(&mut self) -> Option<dbine_driver::transfer::RowBatch> {
                self.0.next()
            }
        }
        // Distinct partition keys: point creates, all in flight at once.
        let rows = |ids: Vec<String>| {
            Rows(vec![dbine_driver::transfer::RowBatch { rows: ids.into_iter().map(|i| vec![Cell::Text(i.clone()), Cell::Text(i)]).collect(), bytes: 0 }].into_iter())
        };
        let cols = names(&["id", "cat"]);
        let pk = vec!["/cat".to_string()];

        // Failure: 10 slow creates sent, then one refused.
        let (rest, answered) = slow().await;
        let mut w = writer(rest);
        let mut ids: Vec<String> = (0..10).map(|i| format!("r{i}")).collect();
        ids.push("bad".into());
        let mut src = rows(ids);
        let mut ticked = 0;
        let mut tick = |n: u64, _: u64, _: bool| ticked += n;
        let f = load(&mut w, &pk, &mut Feed::Rows { names: cols.clone(), source: &mut src }, &mut tick).await.unwrap_err();
        assert!(f.err.to_string().contains("ya existe"), "{f:?}");
        let d = w.drain().await;
        assert!(d.answered && !d.unsure);
        assert_eq!(answered.load(Ordering::SeqCst), 11, "every sent request answered before returning");
        // What the drain saw committed is progress too.
        assert_eq!(ticked + d.rows, 10, "committed rows reported");
        assert!(w.set.is_empty());

        // Cancellation: the load's future dropped mid-flight.
        let (rest, answered) = slow().await;
        let cancelled = async move {
            let mut w = writer(rest);
            let mut src = rows((0..10).map(|i| format!("r{i}")).collect());
            let mut tick = |_: u64, _: u64, _: bool| {};
            let _ = load(&mut w, &pk, &mut Feed::Rows { names: cols, source: &mut src }, &mut tick).await;
        };
        assert!(tokio::time::timeout(Duration::from_millis(100), cancelled).await.is_err());
        assert_eq!(answered.load(Ordering::SeqCst), 10, "the drop waited for the sent requests");
    }

    /// A drain that runs out of time abandons what is still out (reported
    /// as unsure) and the drop doesn't wait for it again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_expired_drain_is_not_waited_again() {
        // Takes requests, never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let rest = Rest {
            http: reqwest::Client::new(),
            base: format!("http://{addr}"),
            key: "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==".into(),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let mut w = Writer { rest, link: "l".into(), path: "/p".into(), coll: "c".into(), set: JoinSet::new(), bytes: 0 };
        let mut tick = |_: u64, _: u64, _: bool| {};
        w.send("[\"k\"]".into(), group(&["a"]), &mut tick).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let d = w.drain_within(Duration::from_millis(200)).await;
        assert!(!d.answered && d.rows == 0);
        assert!(w.set.is_empty());
        let t = std::time::Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_secs(1), "the drop waited again");
    }
}

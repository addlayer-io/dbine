//! DAX queries (spec §4.2). Owned by part B.
//!
//! - API choice per model and account (`query_api = auto`):
//!   `executeDaxQueries` first, on Pro too (G3: Learn contradicts itself, so
//!   the service is asked). A 404, 405 or 501 on its path, the XMLA
//!   tenant-setting error or a shared-capacity refusal moves that model to
//!   JSON for that account for the host's life; another "not available"
//!   refusal with no cause DBine knows, for [`RETRY_ARROW`] (then Arrow is
//!   asked again). A paused or inactive capacity, and every other refusal,
//!   fail that query only and change nothing. The fallback is said once per
//!   session through `out.warning` (an `out.info` on shared capacity, where
//!   it's expected). `query_api = arrow` fails with the reason instead, and
//!   the driver's own reads that force an API never change the memory.
//! - A unit (`DEFINE` + its `EVALUATE`s, or one `EVALUATE`) must start with
//!   `DEFINE` or `EVALUATE` after comments
//!   ([`dbine_driver::read_only::dax_is_query`]); anything else is refused
//!   with `Error::Query` before any request.
//! - Arrow sends the unit whole and maps result set *i* to the unit's
//!   *i*-th `EVALUATE`; JSON sends one request per `EVALUATE`, each with the
//!   unit's `DEFINE` in front, stopping at the first error. With several
//!   `EVALUATE`s each result's `offset`/`line` is where its `EVALUATE`
//!   starts in the unit (the app moves it to the script: `place_result` in
//!   `commands/query.rs`), so the comment above it names it.
//! - Truncation: `truncated = true` and the warning (JSON's caps, Arrow's
//!   `resultSetRowCountLimit = max_rows + 1`, always sent). A cut past
//!   `max_rows` is the caller's own cap (the grid's, or a chart's or a
//!   notebook load's, whose sink knows it). In sink mode a server-side cut
//!   at or under `max_rows` is `Error::Query`, never a silent partial file
//!   (G2).
//! - `rls_user` / `rls_roles` / `custom_data` on user queries only, never on
//!   metadata reads (G13), and results say "Consultando como …".
//! - Errors as `Error::Statement` with the code and the line/column folded
//!   into an offset relative to the unit.
//! - The driver's own browses show the model's column names (`Amount`, not
//!   `Sales[Amount]`); an empty JSON browse takes its columns from the
//!   metadata.

pub mod arrow;
pub mod json;

use crate::catalog::Model;
use crate::dax::browse::parse_browse;
use crate::dax::split::{self, line_of, Parts};
use crate::http::{error_for, Failure, Priority, QueryReply};
use crate::Ctx;
use dbine_driver::sql::StatementKind;
use dbine_driver::{Error, QueryOutcome, Result, ResultColumn, ScriptError};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// The two query APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    /// `executeQueries`: any license, one table per call, 100,000 rows.
    Json,
    /// `executeDaxQueries`: Arrow, several `EVALUATE`s, INFO functions.
    Arrow,
}

/// What the session remembers about its queries (the host remembers each
/// model's fallback to JSON).
#[derive(Debug, Default)]
pub struct QueryState {
    /// The API the last query went to.
    pub last_api: Option<ApiKind>,
    /// Notes already given in this session.
    told_fallback: bool,
    told_rls: bool,
    told_json_roles: bool,
}

/// The model a query runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `None`: the group-less endpoints.
    pub group: Option<String>,
    pub dataset: String,
}

impl Target {
    pub fn of(m: &Model) -> Target {
        Target {
            group: m.workspace.id.clone(),
            dataset: m.id.clone(),
        }
    }

    pub(crate) fn path(&self, api: ApiKind) -> String {
        let op = match api {
            ApiKind::Json => "executeQueries",
            ApiKind::Arrow => "executeDaxQueries",
        };
        match &self.group {
            Some(g) => format!("/groups/{g}/datasets/{}/{op}", self.dataset),
            None => format!("/datasets/{}/{op}", self.dataset),
        }
    }
}

/// How [`run`] runs a query for the driver's own reads.
#[derive(Debug, Clone)]
pub struct QueryOpts {
    /// Rows kept per result set (the rest is cut: `truncated`).
    pub max_rows: usize,
    pub priority: Priority,
    /// Apply the connection's `rls_user` / `rls_roles` / `custom_data`
    /// (never for metadata).
    pub impersonate: bool,
    /// Force one API (INFO functions need Arrow); `None`: the model's.
    pub api: Option<ApiKind>,
}

impl QueryOpts {
    /// A metadata read: background, no impersonation, whatever API the
    /// model takes.
    pub fn metadata() -> QueryOpts {
        QueryOpts {
            max_rows: 1_000_000,
            priority: Priority::Background,
            impersonate: false,
            api: None,
        }
    }
}

/// One result set of a query, for the driver's own use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rowset {
    /// As the service names them (`Table[Column]`, `[Measure]`).
    pub columns: Vec<String>,
    /// The Arrow type of each column (`Int64`, `Utf8`, `Date64`…); empty on
    /// JSON (no types).
    pub types: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Cut by `max_rows` or by the API's cap.
    pub truncated: bool,
}

const NOT_A_QUERY: &str =
    "Power BI solo ejecuta consultas DAX: cada consulta empieza con DEFINE o EVALUATE.";
const SINK_CUT: &str = "Power BI cortó el resultado (la API devuelve una cantidad limitada de filas por consulta): no se escribe un resultado parcial. Limitá la consulta o usá un área de trabajo con capacidad (executeDaxQueries).";

/// Arrow's `resultSetRowCountLimit` for `max_rows` (one more, to know
/// there were more; at most what the API takes).
fn arrow_limit(max_rows: usize) -> u64 {
    (max_rows as u64).saturating_add(1).min(i32::MAX as u64)
}

// ------------------------------------------------- the Arrow memory

#[derive(Debug, Clone, PartialEq, Eq)]
enum Memo {
    Works,
    /// Why not (Spanish, for the warning), and until when it holds
    /// (`None`: the host's life).
    Unavailable(String, Option<Instant>),
}

/// How long a refusal with no cause DBine knows keeps a model on JSON
/// before Arrow is asked again.
pub(crate) const RETRY_ARROW: Duration = Duration::from_secs(10 * 60);

static ARROW: LazyLock<Mutex<HashMap<String, Memo>>> = LazyLock::new(Mutex::default);

/// Per cloud, account and model: a refusal for one account (outside the
/// security group the XMLA tenant setting applies to, say) never moves
/// another account's queries.
fn memo_key(cx: &Ctx, target: &Target) -> String {
    let a = cx.api.account();
    format!(
        "{:?}|{}|{}|{}",
        cx.settings.cloud,
        a.as_ref().map_or("", |a| a.tenant_id.as_str()),
        a.as_ref().map_or("", |a| a.object_id.as_str()),
        target.dataset.to_ascii_lowercase()
    )
}

fn memo(cx: &Ctx, target: &Target) -> Option<Memo> {
    let key = memo_key(cx, target);
    let mut all = ARROW.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(Memo::Unavailable(_, Some(until))) = all.get(&key) {
        if *until <= Instant::now() {
            all.remove(&key);
            return None;
        }
    }
    all.get(&key).cloned()
}

fn remember(cx: &Ctx, target: &Target, m: Memo) {
    ARROW
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(memo_key(cx, target), m);
}

/// What a refusal is remembered as.
fn unavailable(reason: String, retry: Option<Duration>) -> Memo {
    Memo::Unavailable(reason, retry.map(|d| Instant::now() + d))
}

fn choose(cx: &Ctx, target: &Target, forced: Option<ApiKind>) -> ApiKind {
    if let Some(f) = forced {
        return f;
    }
    match cx.settings.query_api {
        crate::config::QueryApi::Json => ApiKind::Json,
        crate::config::QueryApi::Arrow => ApiKind::Arrow,
        crate::config::QueryApi::Auto => match memo(cx, target) {
            Some(Memo::Unavailable(..)) => ApiKind::Json,
            _ => ApiKind::Arrow,
        },
    }
}

/// What a refused `executeDaxQueries` says about the API (rather than the
/// query being wrong).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    /// The API doesn't serve this model to this account: its path isn't
    /// there (404, 405, 501), the XMLA tenant setting is off for the
    /// account, or the workspace is on shared capacity. The reason, in
    /// Spanish; remembered for the host's life.
    Lasting(String),
    /// It says the API isn't available, with no cause DBine knows:
    /// remembered for [`RETRY_ARROW`].
    ForNow(String),
    /// The capacity is paused or not active: this query fails, nothing is
    /// remembered (the capacity may be resumed in a minute).
    Inactive,
}

fn refusal(f: &Failure, shared: bool) -> Option<Refusal> {
    let (code, message) = crate::http::error_parts(&f.body);
    // A DAX error ("Query (L, C) … is not supported in this context") is
    // the query's, whatever words it uses.
    if message
        .as_deref()
        .is_some_and(|m| m.trim_start().starts_with("Query ("))
    {
        return None;
    }
    let text = format!(
        "{} {}",
        code.unwrap_or_default(),
        message.unwrap_or_default()
    )
    .to_ascii_lowercase();
    let says = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if !matches!(f.status, 401 | 429)
        && says(&["paus", "suspend", "not active", "notactive", "inactive"])
    {
        return Some(Refusal::Inactive);
    }
    let reason = if shared {
        "executeDaxQueries no está disponible en esta área de trabajo (capacidad compartida): las consultas van por executeQueries, que devuelve como máximo 100.000 filas por consulta."
    } else {
        "La API Arrow (executeDaxQueries) no está disponible para este modelo: activá «Allow XMLA endpoints and Analyze in Excel with on-premises semantic models» en el portal de administración, o las consultas siguen por executeQueries, con sus límites (100.000 filas por consulta)."
    }
    .to_string();
    if matches!(f.status, 404 | 405 | 501) {
        return Some(Refusal::Lasting(reason));
    }
    if !matches!(f.status, 400 | 403 | 409) {
        return None;
    }
    if says(&["xmla", "sharedcapacity"]) || (says(&["shared"]) && says(&["capacity"])) {
        return Some(Refusal::Lasting(reason));
    }
    says(&[
        "notsupported",
        "not supported",
        "notavailable",
        "not available",
        "unsupported",
        "premium",
        "capacity",
    ])
    .then_some(Refusal::ForNow(reason))
}

/// A paused or inactive capacity, said as such.
fn inactive(f: &Failure) -> Error {
    let (code, _) = crate::http::error_parts(&f.body);
    let tag = match code {
        Some(c) => format!("HTTP {}, {c}", f.status),
        None => format!("HTTP {}", f.status),
    };
    Error::Query(format!(
        "La capacidad del área de trabajo está en pausa o no está activa ({tag}): Power BI no puede consultar el modelo hasta que se reanude."
    ))
}

// ------------------------------------------------- where rows go

/// Receives the result sets of a run.
pub(crate) trait Sink: Send {
    /// A result set starts: `(name, model type)` per column.
    fn begin(&mut self, columns: Vec<(String, String)>) -> Result<()>;
    fn rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()>;
    /// An Arrow batch of the current result set (grid cells by default).
    fn batch(&mut self, b: &arrow_array::RecordBatch) -> Result<()> {
        self.rows(arrow::rows_of(b)?)
    }
    /// It ended with `rows` rows; `cut`: the service may have had more.
    fn end(&mut self, rows: u64, cut: bool) -> Result<()>;
    /// Waiting for a slot of the query window (`limit` per minute).
    fn waiting(&mut self, limit: u32);
}

/// The editor's outcome (or its sink in export / chart / notebook runs).
struct ToOutcome<'o> {
    out: &'o mut QueryOutcome,
    max_rows: usize,
    unit: &'o str,
    /// Where each `EVALUATE` starts, when the unit has several.
    places: Vec<usize>,
    /// The driver's own browse of this table: its prefix goes.
    strip: Option<String>,
    index: usize,
}

impl ToOutcome<'_> {
    fn name(&self, c: &str) -> String {
        if let Some(t) = &self.strip {
            if let Some(rest) = c.strip_prefix(t.as_str()) {
                if rest.len() > 2 && rest.starts_with('[') && rest.ends_with(']') {
                    return rest[1..rest.len() - 1].to_string();
                }
            }
        }
        c.to_string()
    }
}

impl Sink for ToOutcome<'_> {
    fn begin(&mut self, columns: Vec<(String, String)>) -> Result<()> {
        let cols = columns
            .into_iter()
            .map(|(n, t)| ResultColumn {
                name: self.name(&n),
                type_name: t,
            })
            .collect();
        self.out.begin_result(cols);
        if self.places.len() > 1 {
            if let (Some(r), Some(&at)) = (self.out.results.last_mut(), self.places.get(self.index))
            {
                r.offset = Some(at);
                r.line = Some(line_of(self.unit, at));
            }
        }
        self.index += 1;
        Ok(())
    }

    fn rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        for r in rows {
            self.out.push_row(r, self.max_rows);
        }
        if let Some(e) = &self.out.sink_error {
            return Err(Error::Query(e.clone()));
        }
        Ok(())
    }

    fn end(&mut self, rows: u64, cut: bool) -> Result<()> {
        if !cut {
            return Ok(());
        }
        if usize::try_from(rows).map_or(true, |r| r > self.max_rows) {
            // More rows came than the caller asked for: its own cap cut the
            // result. The grid's `push_row` marked it; a sink (a chart, a
            // notebook load) streamed them and knows its own limit.
            return Ok(());
        }
        if self.out.sink.is_some() {
            // The service cut at or under the caller's cap: a partial
            // result would be written as if whole.
            return Err(Error::Query(SINK_CUT.into()));
        }
        if let Some(r) = self.out.results.last_mut() {
            r.truncated = true;
        }
        self.out.warning(json::CUT);
        Ok(())
    }

    fn waiting(&mut self, limit: u32) {
        self.out.info(format!(
            "Esperando el límite de consultas de Power BI ({limit} por minuto)…"
        ));
    }
}

/// The driver's own reads.
struct ToRowsets {
    sets: Vec<Rowset>,
    max_rows: usize,
}

impl Sink for ToRowsets {
    fn begin(&mut self, columns: Vec<(String, String)>) -> Result<()> {
        let (columns, types) = columns.into_iter().unzip();
        self.sets.push(Rowset {
            columns,
            types,
            rows: Vec::new(),
            truncated: false,
        });
        Ok(())
    }

    fn rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        if let Some(s) = self.sets.last_mut() {
            for r in rows {
                if s.rows.len() < self.max_rows {
                    s.rows.push(r);
                } else {
                    s.truncated = true;
                }
            }
        }
        Ok(())
    }

    fn end(&mut self, _rows: u64, cut: bool) -> Result<()> {
        if cut {
            if let Some(s) = self.sets.last_mut() {
                s.truncated = true;
            }
        }
        Ok(())
    }

    fn waiting(&mut self, _limit: u32) {}
}

// ------------------------------------------------- positions

/// The byte offset of 1-based `line` and character `column` in `text`.
fn offset_at(text: &str, line: u32, column: u32) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line {
        start += text[start..].find('\n')? + 1;
    }
    let row = &text[start..];
    let row = &row[..row.find('\n').unwrap_or(row.len())];
    let col = column.max(1) as usize - 1;
    Some(start + row.char_indices().nth(col).map_or(row.len(), |(i, _)| i))
}

/// A statement error placed in the unit: `pos` is in the text sent, `map`
/// turns its offsets into the unit's.
fn placed(
    mut e: ScriptError,
    pos: Option<(u32, u32)>,
    sent: &str,
    unit: &str,
    map: &dyn Fn(usize) -> usize,
) -> Error {
    if let Some(at) = pos.and_then(|(l, c)| offset_at(sent, l, c)) {
        let at = map(at).min(unit.len());
        e = e.at_offset(at).at_line(line_of(unit, at));
    }
    Error::Statement(Box::new(e))
}

// ------------------------------------------------- the two APIs

pub(crate) enum Sent {
    Done,
    /// The API doesn't serve the model; nothing was received. `retry`: how
    /// long to remember it (`None`: the host's life).
    Unavailable {
        reason: String,
        retry: Option<Duration>,
    },
}

/// What the user's query is run as (G13).
struct Imp<'a> {
    user: Option<&'a str>,
    roles: &'a [String],
    custom: Option<&'a str>,
}

fn imp(cx: &Ctx, on: bool) -> Imp<'_> {
    if on {
        Imp {
            user: cx.settings.rls_user.as_deref(),
            roles: &cx.settings.rls_roles,
            custom: cx.settings.custom_data.as_deref(),
        }
    } else {
        Imp {
            user: None,
            roles: &[],
            custom: None,
        }
    }
}

fn shared_capacity(cx: &Ctx, target: &Target) -> bool {
    cx.model
        .as_ref()
        .filter(|m| m.id.eq_ignore_ascii_case(&target.dataset))
        .is_some_and(|m| !m.workspace.on_dedicated_capacity)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn arrow_run(
    cx: &Ctx,
    target: &Target,
    text: &str,
    row_limit: u64,
    impersonate: bool,
    priority: Priority,
    sink: &mut (dyn Sink + Send),
) -> Result<Sent> {
    let who = imp(cx, impersonate);
    let body = arrow::body(
        text,
        Some(row_limit),
        cx.settings.query_timeout.as_secs(),
        who.user,
        who.roles,
        who.custom,
    );
    let rate = cx.api.query_rate(true);
    cx.api
        .query_slot_notify(true, priority, &mut |_| sink.waiting(rate))
        .await?;
    let mut stream = match cx
        .api
        .post_query(&target.path(ApiKind::Arrow), &body, None)
        .await?
    {
        QueryReply::Failed(f) => {
            match refusal(&f, shared_capacity(cx, target)) {
                Some(Refusal::Lasting(reason)) => {
                    return Ok(Sent::Unavailable {
                        reason,
                        retry: None,
                    })
                }
                Some(Refusal::ForNow(reason)) => {
                    return Ok(Sent::Unavailable {
                        reason,
                        retry: Some(RETRY_ARROW),
                    })
                }
                Some(Refusal::Inactive) => return Err(inactive(&f)),
                None => {}
            }
            if f.status == 400 {
                let (se, pos) = json::statement_error(&f.body);
                if crate::http::error_parts(&f.body).1.is_some() {
                    return Err(placed(se, pos, text, text, &|o| o));
                }
            }
            return Err(error_for(&f, true));
        }
        QueryReply::Stream(s) => s,
    };
    if stream
        .content_type
        .as_deref()
        .is_some_and(|t| t.to_ascii_lowercase().contains("json"))
    {
        return Err(Error::Query(
            "Power BI respondió JSON donde se esperaba Arrow".into(),
        ));
    }
    let mut dec = arrow::Decoder::new();
    let mut events = Vec::new();
    let mut error: Option<(arrow_schema::SchemaRef, Vec<Vec<Value>>)> = None;
    let mut rows_in_set = 0u64;
    loop {
        let chunk = stream.chunk().await?;
        let done = chunk.is_none();
        match chunk {
            Some(b) => dec.push(&b, &mut events)?,
            None => dec.finish(&mut events)?,
        }
        for ev in events.drain(..) {
            match ev {
                arrow::Event::Begin {
                    schema,
                    error: true,
                } => error = Some((schema, Vec::new())),
                arrow::Event::Begin { schema, .. } => {
                    rows_in_set = 0;
                    sink.begin(
                        schema
                            .fields()
                            .iter()
                            .map(|f| (f.name().clone(), arrow::model_type(f.data_type())))
                            .collect(),
                    )?;
                }
                arrow::Event::Batch(b) => match &mut error {
                    Some((_, e)) => e.extend(arrow::rows_of(&b)?),
                    None => {
                        rows_in_set += b.num_rows() as u64;
                        sink.batch(&b)?;
                    }
                },
                arrow::Event::End => match error.take() {
                    Some((schema, rows)) => {
                        let (se, pos) = arrow::error_of(&schema, &rows);
                        return Err(placed(se, pos, text, text, &|o| o));
                    }
                    None => {
                        // At the limit the service may have had more; past a
                        // limit it doesn't take, its default (1,000,000)
                        // may have cut silently (F4, a live check).
                        let cut = rows_in_set >= row_limit
                            || (row_limit > 1_000_000 && rows_in_set == 1_000_000);
                        sink.end(rows_in_set, cut)?;
                    }
                },
            }
        }
        if done {
            break;
        }
    }
    Ok(Sent::Done)
}

/// Each `EVALUATE` of the unit through `executeQueries`, with the unit's
/// `DEFINE` in front; the sensitivity label, if any.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn json_run(
    cx: &Ctx,
    target: &Target,
    text: &str,
    parts: &Parts,
    impersonate: bool,
    priority: Priority,
    sink: &mut (dyn Sink + Send),
    type_of: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<Option<String>> {
    let who = imp(cx, impersonate);
    let first = parts.evaluates.first().copied().unwrap_or(0);
    let define = parts.define.map(|d| &text[d..first]).unwrap_or("");
    let d_start = parts.define.unwrap_or(0);
    let mut label = None;
    for (i, &e) in parts.evaluates.iter().enumerate() {
        let e_end = parts.evaluates.get(i + 1).copied().unwrap_or(text.len());
        let query = format!("{define}{}", &text[e..e_end]);
        let d_len = define.len();
        let map = move |p: usize| {
            if p < d_len {
                d_start + p
            } else {
                e + (p - d_len)
            }
        };
        let body = json::body(&query, who.user);
        let rate = cx.api.query_rate(false);
        cx.api
            .query_slot_notify(false, priority, &mut |_| sink.waiting(rate))
            .await?;
        let timeout = Some(cx.settings.query_timeout + Duration::from_secs(30));
        let mut stream = match cx
            .api
            .post_query(&target.path(ApiKind::Json), &body, timeout)
            .await?
        {
            QueryReply::Failed(f) => {
                let (code, message) = crate::http::error_parts(&f.body);
                if f.status == 400 && (code.is_some() || message.is_some()) {
                    let (se, pos) = json::statement_error(&f.body);
                    return Err(placed(se, pos, &query, text, &map));
                }
                return Err(error_for(&f, true));
            }
            QueryReply::Stream(s) => s,
        };
        let (bytes, over) = stream.bytes(json::MAX_BYTES + 4 * 1024 * 1024).await?;
        if over {
            return Err(Error::Query(
                "Power BI respondió más de lo que su API devuelve por consulta".into(),
            ));
        }
        let parsed = json::parse_typed(&bytes, type_of)?;
        if let Some((se, pos)) = parsed.failure {
            return Err(placed(se, pos, &query, text, &map));
        }
        label = label.or(parsed.label);
        if parsed.tables.is_empty() {
            sink.begin(Vec::new())?;
            sink.end(0, false)?;
        }
        for t in parsed.tables {
            let rows = t.rows.len() as u64;
            sink.begin(
                t.columns
                    .iter()
                    .map(|c| (c.clone(), type_of(c).unwrap_or_default()))
                    .collect(),
            )?;
            sink.rows(t.rows)?;
            sink.end(rows, t.cut)?;
        }
    }
    Ok(label)
}

/// The model types the metadata cache knows, by result column name
/// (`Table[Column]`, or a plain name of `table`'s browse).
fn type_lookup(cx: &Ctx, table: Option<String>) -> impl Fn(&str) -> Option<String> + Sync {
    let meta = cx.meta.cached.clone();
    move |c: &str| {
        let meta = meta.as_ref()?;
        let (t, col) = match c.find('[') {
            Some(i) if c.ends_with(']') && i > 0 => (c[..i].to_string(), &c[i + 1..c.len() - 1]),
            Some(_) => return None,
            None => (table.clone()?, c),
        };
        let table = meta.tables.iter().find(|x| x.name == t)?;
        let column = table.columns.iter().find(|x| x.name == col)?;
        Some(column.data_type.name().to_string())
    }
}

// ------------------------------------------------- user queries

/// What the run's notes say about testing RLS (G13).
fn rls_notes(cx: &mut Ctx, api: ApiKind, out: &mut QueryOutcome) {
    let s = &cx.settings;
    let who = match (&s.rls_user, s.rls_roles.is_empty()) {
        (Some(u), true) => u.clone(),
        (Some(u), false) => format!("{u} (roles: {})", s.rls_roles.join(", ")),
        (None, false) => format!("los roles {}", s.rls_roles.join(", ")),
        (None, true) => return,
    };
    out.info(format!(
        "Consultando como {who}: los resultados se filtran con su seguridad por filas."
    ));
    if api == ApiKind::Json
        && (!s.rls_roles.is_empty() || s.custom_data.is_some())
        && !cx.query.told_json_roles
    {
        cx.query.told_json_roles = true;
        out.warning("Los roles y CUSTOMDATA de prueba solo se aplican con executeDaxQueries; executeQueries prueba solo el usuario.");
    }
    let no_roles = cx
        .meta
        .cached
        .as_ref()
        .is_some_and(|m| m.roles.is_empty() && m.tier != crate::meta::Tier::Stats);
    let required = cx
        .model
        .as_ref()
        .and_then(|m| m.effective_identity_required);
    if no_roles && required == Some(false) && !cx.query.told_rls {
        cx.query.told_rls = true;
        out.warning("El modelo no tiene roles de seguridad por filas: «Probar como usuario» no filtra nada y los totales son los de todo el modelo.");
    }
}

fn impersonating(cx: &Ctx) -> bool {
    cx.settings.rls_user.is_some()
        || !cx.settings.rls_roles.is_empty()
        || cx.settings.custom_data.is_some()
}

async fn run_user(
    cx: &mut Ctx,
    text: &str,
    max_rows: usize,
    out: &mut QueryOutcome,
    priority: Priority,
) -> Result<()> {
    if !dbine_driver::read_only::dax_is_query(text) {
        return Err(Error::Query(NOT_A_QUERY.into()));
    }
    let model = crate::catalog::model(cx)?.clone();
    let target = Target::of(&model);
    let parts = split::parts(text);
    if parts.evaluates.is_empty() {
        return Err(Error::Statement(Box::new(ScriptError::new(
            split::DEFINE_WITHOUT_EVALUATE,
        ))));
    }
    let browse = parse_browse(text).map(|b| b.table);
    let mut api = choose(cx, &target, None);
    if impersonating(cx) {
        rls_notes(cx, api, out);
    }
    let impersonate = impersonating(cx);
    let first_result = out.results.len();
    let places = parts.evaluates.clone();
    if api == ApiKind::Arrow {
        let mut sink = ToOutcome {
            out: &mut *out,
            max_rows,
            unit: text,
            places: places.clone(),
            strip: browse.clone(),
            index: 0,
        };
        match arrow_run(
            cx,
            &target,
            text,
            arrow_limit(max_rows),
            impersonate,
            priority,
            &mut sink,
        )
        .await?
        {
            Sent::Done => {
                remember(cx, &target, Memo::Works);
                cx.query.last_api = Some(ApiKind::Arrow);
            }
            Sent::Unavailable { reason, retry } => {
                remember(cx, &target, unavailable(reason.clone(), retry));
                if cx.settings.query_api == crate::config::QueryApi::Arrow {
                    return Err(Error::Query(reason));
                }
                api = ApiKind::Json;
            }
        }
    }
    if api == ApiKind::Json {
        if let Some(Memo::Unavailable(reason, _)) = memo(cx, &target) {
            if !cx.query.told_fallback && cx.settings.query_api == crate::config::QueryApi::Auto {
                cx.query.told_fallback = true;
                if shared_capacity(cx, &target) {
                    out.info(reason);
                } else {
                    out.warning(reason);
                }
            }
        }
        let type_of = type_lookup(cx, browse.clone());
        let label = {
            let mut sink = ToOutcome {
                out: &mut *out,
                max_rows,
                unit: text,
                places,
                strip: browse.clone(),
                index: 0,
            };
            json_run(
                cx,
                &target,
                text,
                &parts,
                impersonate,
                priority,
                &mut sink,
                &type_of,
            )
            .await?
        };
        cx.query.last_api = Some(ApiKind::Json);
        if let Some(l) = label {
            out.info(format!(
                "El modelo tiene la etiqueta de confidencialidad «{l}»."
            ));
        }
        // An empty browse: the table's columns from the metadata.
        if let Some(table) = &browse {
            if out.results[first_result..]
                .iter()
                .any(|r| r.columns.is_empty())
            {
                let meta = match cx.meta.cached.clone() {
                    Some(m) => Some(m),
                    None => crate::meta::model(cx).await.ok(),
                };
                if let Some(meta) = meta {
                    if let Some(t) = meta.tables.iter().find(|t| &t.name == table) {
                        let cols: Vec<ResultColumn> = t
                            .columns
                            .iter()
                            .map(|c| ResultColumn {
                                name: c.name.clone(),
                                type_name: c.data_type.name().to_string(),
                            })
                            .collect();
                        for r in &mut out.results[first_result..] {
                            if r.columns.is_empty() {
                                r.columns = cols.clone();
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// One unit of the editor's script (or a whole script on a `Whole`
/// caller): results, messages and errors into `out`.
pub async fn execute(
    cx: &mut Ctx,
    text: &str,
    max_rows: usize,
    out: &mut QueryOutcome,
) -> Result<()> {
    run_user(cx, text, max_rows, out, Priority::Editor).await
}

/// MCP and the assistant (decision D3): one unit with one `EVALUATE`
/// (else `Unsupported` before running), background priority, the query
/// timeout and a row limit always sent (S5).
pub async fn run_read_only(
    cx: &mut Ctx,
    statement: &str,
    max_rows: usize,
    out: &mut QueryOutcome,
) -> Result<()> {
    let units = split::split(statement);
    let one =
        match units.as_slice() {
            [u] if u.error.is_none()
                && u.kind == StatementKind::Sql
                && dbine_driver::read_only::dax_is_query(&u.text)
                && split::parts(&u.text).evaluates.len() == 1 =>
            {
                u
            }
            _ => return Err(Error::Unsupported(
                "una lectura de solo lectura en Power BI es una consulta DAX con un solo EVALUATE"
                    .into(),
            )),
        };
    match run_user(cx, &one.text, max_rows, out, Priority::Background).await {
        // Errors' places are in the unit; the caller's text may have
        // comments before it.
        Err(Error::Statement(mut e)) => {
            if let Some(o) = e.offset {
                let at = one.start + o;
                e.offset = Some(at);
                e.line = Some(line_of(statement, at));
            }
            Err(Error::Statement(e))
        }
        r => r,
    }
}

/// A query for the driver's own reads (metadata, health, monitor, jobs):
/// every result set, in order. A read that forces an API (INFO functions
/// on Arrow) never changes what the host remembers about the model: a
/// refused metadata read doesn't move the user's queries.
pub async fn run(
    cx: &mut Ctx,
    target: &Target,
    dax: &str,
    opts: &QueryOpts,
) -> Result<Vec<Rowset>> {
    let parts = split::parts(dax);
    if parts.evaluates.is_empty() {
        return Err(Error::Query(split::DEFINE_WITHOUT_EVALUATE.into()));
    }
    let mut sink = ToRowsets {
        sets: Vec::new(),
        max_rows: opts.max_rows,
    };
    let forced = opts.api.is_some();
    let mut api = choose(cx, target, opts.api);
    if api == ApiKind::Arrow {
        match arrow_run(
            cx,
            target,
            dax,
            arrow_limit(opts.max_rows),
            opts.impersonate,
            opts.priority,
            &mut sink,
        )
        .await?
        {
            Sent::Done => {
                if !forced {
                    remember(cx, target, Memo::Works);
                }
            }
            Sent::Unavailable { reason, retry } => {
                if !forced {
                    remember(cx, target, unavailable(reason.clone(), retry));
                }
                if forced || cx.settings.query_api == crate::config::QueryApi::Arrow {
                    return Err(Error::Unsupported(reason));
                }
                api = ApiKind::Json;
            }
        }
    }
    if api == ApiKind::Json {
        let none = |_: &str| -> Option<String> { None };
        json_run(
            cx,
            target,
            dax,
            &parts,
            opts.impersonate,
            opts.priority,
            &mut sink,
            &none,
        )
        .await?;
    }
    cx.query.last_api.get_or_insert(api);
    Ok(sink.sets)
}

/// Whether `executeDaxQueries` serves the model to this account (probed
/// once and remembered as [`refusal`] says; a paused capacity is an error,
/// not an answer).
pub async fn arrow_available(cx: &mut Ctx, target: &Target) -> Result<bool> {
    match cx.settings.query_api {
        crate::config::QueryApi::Json => return Ok(false),
        crate::config::QueryApi::Arrow => return Ok(true),
        crate::config::QueryApi::Auto => {}
    }
    if let Some(m) = memo(cx, target) {
        return Ok(m == Memo::Works);
    }
    let mut sink = ToRowsets {
        sets: Vec::new(),
        max_rows: 1,
    };
    match arrow_run(
        cx,
        target,
        "EVALUATE {1}",
        2,
        false,
        Priority::Background,
        &mut sink,
    )
    .await?
    {
        Sent::Done => {
            remember(cx, target, Memo::Works);
            Ok(true)
        }
        Sent::Unavailable { reason, retry } => {
            remember(cx, target, unavailable(reason, retry));
            Ok(false)
        }
    }
}

/// The API the session's queries go to, once known.
pub fn api_in_use(cx: &Ctx) -> Option<ApiKind> {
    cx.query.last_api
}

/// The API the session's queries will go to as far as it's known now
/// (no request): the forced one, the remembered one, else `None`.
pub fn api_expected(cx: &Ctx) -> Option<ApiKind> {
    if let Some(a) = cx.query.last_api {
        return Some(a);
    }
    match cx.settings.query_api {
        crate::config::QueryApi::Json => Some(ApiKind::Json),
        crate::config::QueryApi::Arrow => Some(ApiKind::Arrow),
        crate::config::QueryApi::Auto => {
            let target = Target::of(cx.model.as_ref()?);
            match memo(cx, &target)? {
                Memo::Works => Some(ApiKind::Arrow),
                Memo::Unavailable(..) => Some(ApiKind::Json),
            }
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

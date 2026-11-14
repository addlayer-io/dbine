//! Data compare of two queries ("Comparar con el origen…", docs/data-compare.md,
//! "Comparing two queries"). Each side is any connection (and database) and a
//! query in its engine's language: SQL, DAX, a MongoDB find… Both results are
//! read whole, each on a read-only session of its own, and compared on the
//! whole-table compare's machinery ([`whole::compare_with`]): in memory while
//! they're small, past that in the scratch DuckDB, encrypted with a key made
//! for that compare (under the vault folder's `compare/`, purged at start).
//!
//! The user pairs the columns (a DAX `Date[Year]` against a SQL `year`) and
//! picks the key among them; values compare by what they are, with the
//! options of [`QueryRules`]: a tolerance on numbers (a measure against a SUM
//! can differ by rounding), NULL as BLANK, case and spaces.
//!
//! Read-only: there is no sync script, since a query isn't a table
//! (`data_compare_script` refuses one). The difference is kept where the
//! table compare keeps its own ([`data_compare::diffs`]), so sorting and
//! filtering it (`data_compare_rows`) and the AI's explanation work the same.

use super::data_compare::{
    self, canon, whole, ChangedRow, DataCompareResult, Diff, Listed, ProgressEvent, Running,
    Sessions,
};
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::transfer::Cell;
use dbine_driver::{ColumnInfo, ObjectRef, QueryOutcome, ResultColumn, RowSink, RowSinkRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, State};
use tokio::sync::Notify;

#[cfg(test)]
#[path = "data_compare_query_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "data_compare_query_live_tests.rs"]
mod live_tests;

/// One side: where its query runs, and the query.
#[derive(Deserialize, Clone, Debug)]
pub struct QuerySide {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    pub query: String,
}

/// A compared column: its name in each result, and whether it is part of
/// the key that pairs the rows.
#[derive(Deserialize, Clone, Debug)]
pub struct ColumnPair {
    pub left: String,
    pub right: String,
    #[serde(default)]
    pub key: bool,
}

/// How values compare beyond their type.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct QueryOptions {
    /// Two numbers at most this far apart are the same (0: exactly)…
    #[serde(default)]
    pub absolute: f64,
    /// …or this far relative to the larger of the two (0.001 is 0.1 %).
    #[serde(default)]
    pub relative: f64,
    /// NULL equals a blank value (0, "" or false), as DAX's BLANK does.
    #[serde(default)]
    pub null_as_blank: bool,
    /// Text compares without case (keys too).
    #[serde(default)]
    pub ignore_case: bool,
    /// Text compares without its leading and trailing spaces (keys too).
    #[serde(default)]
    pub trim: bool,
}

#[derive(Deserialize)]
pub struct QueryCompareArgs {
    pub left: QuerySide,
    pub right: QuerySide,
    /// The compared columns; those marked `key` pair the rows.
    pub columns: Vec<ColumnPair>,
    #[serde(default)]
    pub options: QueryOptions,
    /// The UI's id for this compare: its progress events
    /// (`data-compare-progress`) carry it and `data_compare_cancel` stops
    /// it. Empty: one is made up.
    #[serde(default)]
    pub run: String,
}

/// The table compare's result, plus each compared column's name and type on
/// the right (`columns` and `types` are the left's).
#[derive(Serialize)]
pub struct QueryCompareResult {
    #[serde(flatten)]
    pub result: DataCompareResult,
    pub right_columns: Vec<String>,
    pub right_types: Vec<String>,
}

// ---------------------------------------------------------------------------
// How values compare

/// A query compare's [`whole::Rules`]: values by what they are (`1`, `1.0`
/// and `"1.00"` alike, as in the table compare; a date and a timestamp at
/// its midnight alike, since a SQL `date` against a DAX DateTime is the
/// same day; PostgreSQL's `t` and a `true` alike, [`bool_text`]), with the
/// [`QueryOptions`]. In the scratch DuckDB each
/// compared column keeps its text as compared (`c`), its number (`x`) and
/// whether it is blank (`b`), and [`QueryRules::same_sql`] says there what
/// [`QueryRules::same`] says here.
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueryRules {
    opts: QueryOptions,
}

impl QueryRules {
    pub(crate) fn new(opts: QueryOptions) -> CommandResult<QueryRules> {
        let ok = |x: f64| x.is_finite() && x >= 0.0;
        if !ok(opts.absolute) || !ok(opts.relative) {
            return Err(CommandError::BadRequest(
                "la tolerancia tiene que ser un número mayor o igual a cero".into(),
            ));
        }
        Ok(QueryRules { opts })
    }

    /// A value as text for comparing: its canonical form, a timestamp at
    /// midnight as its date, then spaces and case as the options say.
    /// `None`: NULL.
    fn text(&self, v: &Value) -> Option<String> {
        if v.is_null() {
            return None;
        }
        let c = match bool_text(v) {
            Some(b) => canon(&Value::Bool(b)),
            None => canon(v),
        };
        let mut s = midnight_as_date(c);
        if self.opts.trim {
            let t = s.trim();
            if t.len() != s.len() {
                s = t.to_string();
            }
        }
        if self.opts.ignore_case {
            s = s.to_lowercase();
        }
        Some(s)
    }

    /// A tolerance was asked for. Without one, numbers are the same only
    /// when their canonical text is ([`canon`] keeps every digit of an
    /// integer or a decimal): `f64` would take two int64 ids past 2^53 for
    /// one.
    fn tolerant(&self) -> bool {
        self.opts.absolute > 0.0 || self.opts.relative > 0.0
    }

    /// Two numbers within the tolerance: `|x - y| <= max(absolute,
    /// relative × max(|x|, |y|))`.
    fn within(&self, x: f64, y: f64) -> bool {
        self.tolerant()
            && (x - y).abs()
                <= self
                    .opts
                    .absolute
                    .max(self.opts.relative * x.abs().max(y.abs()))
    }

    /// A blank value, as DAX's BLANK equals: NULL, 0, "" or false.
    fn blank(&self, v: &Value) -> bool {
        match v {
            Value::Null | Value::Bool(false) => true,
            _ if bool_text(v) == Some(false) => true,
            _ => number(v) == Some(0.0) || self.text(v).is_some_and(|t| t.is_empty()),
        }
    }

    /// A tolerance as a DuckDB double, read the way Rust prints it.
    fn sql_number(x: f64) -> String {
        format!("CAST('{x}' AS DOUBLE)")
    }
}

impl whole::Rules for QueryRules {
    fn key(&self, v: &Value) -> String {
        self.text(v).unwrap_or_else(|| canon(v))
    }

    fn same(&self, l: &Value, r: &Value) -> bool {
        match (self.text(l), self.text(r)) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a == b
                    || match (number(l), number(r)) {
                        (Some(x), Some(y)) => self.within(x, y),
                        _ => false,
                    }
            }
            _ => self.opts.null_as_blank && self.blank(l) && self.blank(r),
        }
    }

    fn columns(&self) -> &'static [(&'static str, &'static str)] {
        &[("c", "VARCHAR"), ("x", "DOUBLE"), ("b", "BOOLEAN")]
    }

    fn cells(&self, v: &Value) -> Vec<Cell> {
        vec![
            self.text(v).map_or(Cell::Null, Cell::Text),
            number(v).map_or(Cell::Null, Cell::Float),
            Cell::Bool(self.blank(v)),
        ]
    }

    fn same_sql(&self, i: usize) -> String {
        let (a, r) = (
            Self::sql_number(self.opts.absolute),
            Self::sql_number(self.opts.relative),
        );
        let tolerance = if self.tolerant() {
            format!(
                " OR (l.x{i} IS NOT NULL AND r.x{i} IS NOT NULL \
                 AND abs(l.x{i} - r.x{i}) <= greatest({a}, {r} * greatest(abs(l.x{i}), abs(r.x{i}))))"
            )
        } else {
            String::new()
        };
        let blank = if self.opts.null_as_blank {
            format!(" OR ((l.c{i} IS NULL OR r.c{i} IS NULL) AND l.b{i} AND r.b{i})")
        } else {
            String::new()
        };
        format!(
            "coalesce((l.c{i} IS NULL AND r.c{i} IS NULL) \
             OR l.c{i} = r.c{i}{tolerance}{blank}, false)"
        )
    }

    /// Under NULL as BLANK, a row on one side only whose compared values
    /// are all blank is the row the other side left out: DAX's
    /// SUMMARIZECOLUMNS leaves out the rows whose measures are all BLANK,
    /// where SQL over a dimension gives them with NULL or 0.
    fn lone_same(&self, values: &[Value]) -> bool {
        self.opts.null_as_blank && !values.is_empty() && values.iter().all(|v| self.blank(v))
    }

    fn lone_same_sql(&self, view: &str, i: usize) -> Option<String> {
        self.opts.null_as_blank.then(|| format!("{view}.b{i}"))
    }
}

/// A boolean written as text: PostgreSQL's `t` and `f` (its driver streams
/// them before it says the column is boolean, which the table compare
/// learns from the catalog), or `true` and `false`. As booleans they meet
/// another engine's `true` or `1`, as keys too.
fn bool_text(v: &Value) -> Option<bool> {
    match v.as_str()? {
        "t" | "true" => Some(true),
        "f" | "false" => Some(false),
        _ => None,
    }
}

/// A number as the table compare reads one ([`canon`]): a JSON number, or
/// text that is only a number (not a code with leading zeros, "007").
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            let leading_zero = t.len() > 1 && t.starts_with('0') && !t.starts_with("0.");
            let plain = !t.is_empty()
                && !leading_zero
                && t.chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'));
            if plain {
                t.parse::<f64>().ok().filter(|f| f.is_finite())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A canonical timestamp at midnight (`2024-03-01 00:00:00`, as [`canon`]
/// writes them) as its date.
fn midnight_as_date(s: String) -> String {
    let b = s.as_bytes();
    let dated = b.len() == 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..10]
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if dated && s.ends_with(" 00:00:00") {
        s[..10].to_string()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// The compare

/// The compared columns, the key's first, and how many are the key.
fn pairs_of(columns: &[ColumnPair]) -> CommandResult<whole::Pairs> {
    let key: Vec<(String, String)> = columns
        .iter()
        .filter(|c| c.key)
        .map(|c| (c.left.clone(), c.right.clone()))
        .collect();
    if key.is_empty() {
        return Err(CommandError::BadRequest(
            "elegí las columnas que identifican cada fila (la clave)".into(),
        ));
    }
    let nk = key.len();
    let mut out = key;
    out.extend(
        columns
            .iter()
            .filter(|c| !c.key)
            .map(|c| (c.left.clone(), c.right.clone())),
    );
    Ok(whole::Pairs { columns: out, nk })
}

/// A side's compared columns, as the whole compare takes a side's columns
/// before reading (the query's result says their types as it begins).
fn named(pairs: &whole::Pairs, side: usize) -> Vec<ColumnInfo> {
    pairs
        .columns
        .iter()
        .map(|(l, r)| ColumnInfo {
            name: if side == 0 { l.clone() } else { r.clone() },
            data_type: String::new(),
            nullable: true,
            primary_key: false,
            auto_increment: false,
            default_value: None,
        })
        .collect()
}

fn check_side(side: &QuerySide, name: &str) -> CommandResult<()> {
    if side.connection_id.is_empty() {
        return Err(CommandError::BadRequest(format!(
            "elegí la conexión de {name}"
        )));
    }
    if side.query.trim().is_empty() {
        return Err(CommandError::BadRequest(format!(
            "escribí la consulta de {name}"
        )));
    }
    Ok(())
}

/// The UI's id for a compare (or a new one).
fn run_id(run: &str) -> CommandResult<String> {
    if run.is_empty() {
        Ok(uuid::Uuid::new_v4().to_string())
    } else if data_compare::valid_run(run) {
        Ok(run.to_string())
    } else {
        Err(CommandError::BadRequest(
            "id de comparación no válido".into(),
        ))
    }
}

/// The drivers the scratch DuckDB's host serves as well (the DuckDB crate's,
/// `local_duckdb::config`): a host's reader thread brings both their rows
/// and the scratch database's replies.
const SCRATCH_HOST_DRIVERS: &[&str] = &["duckdb", "duckdb_files"];

/// Whether a side's rows may be held where they arrive ([`whole::Source::hold`]):
/// not on the scratch DuckDB's host, nor when the driver isn't known.
fn holds_its_reader(driver: Option<&str>) -> bool {
    driver.is_some_and(|d| !SCRATCH_HOST_DRIVERS.contains(&d))
}

/// A query side as the difference keeps it: no table to write to.
fn kept_side(s: &QuerySide) -> data_compare::Side {
    data_compare::Side {
        connection_id: s.connection_id.clone(),
        database: s.database.clone(),
        object: ObjectRef {
            kind: data_compare::QUERY_KIND.into(),
            schema: None,
            name: String::new(),
        },
    }
}

/// Compare the whole results of two queries.
#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_query(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    args: QueryCompareArgs,
) -> CommandResult<QueryCompareResult> {
    let run = run_id(&args.run)?;
    let ctl = Arc::new(whole::Ctl::default());
    let _running = Running::register(&run, ctl.clone())?;
    let progress = |p: &whole::Progress| {
        for (i, side) in ["left", "right"].into_iter().enumerate() {
            let _ = app.emit(
                "data-compare-progress",
                ProgressEvent {
                    run: &run,
                    side,
                    rows: p.rows[i],
                    phase: p.phase.as_str(),
                    differences: p.differences,
                },
            );
        }
    };
    let opts = whole::Options {
        in_memory_rows: data_compare::IN_MEMORY_ROWS,
        in_memory_bytes: data_compare::IN_MEMORY_BYTES,
        max_diff_rows: data_compare::MAX_DIFF_ROWS,
        scratch: data_compare::scratch_root(),
        duckdb: None,
    };
    compare_queries(&state, args, &run, &ctl, &opts, &progress).await
}

/// Both queries on read-only sessions of their own (`dcmp:<run>:left|right`,
/// stopped by `cancel_session`), compared whole; the result, and the
/// difference kept for sorting, filtering and the AI.
async fn compare_queries(
    state: &AppState,
    args: QueryCompareArgs,
    run: &str,
    ctl: &Arc<whole::Ctl>,
    opts: &whole::Options,
    progress: &(dyn Fn(&whole::Progress) + Send + Sync),
) -> CommandResult<QueryCompareResult> {
    check_side(&args.left, "la izquierda")?;
    check_side(&args.right, "la derecha")?;
    let pairs = pairs_of(&args.columns)?;
    let rules = QueryRules::new(args.options)?;
    let sessions = Sessions {
        state,
        keys: [format!("dcmp:{run}:left"), format!("dcmp:{run}:right")],
    };
    for k in &sessions.keys {
        let (st, k) = (state.clone(), k.clone());
        ctl.add_stopper(Arc::new(move || {
            crate::commands::query::cancel_session(&st, &k)
        }));
    }
    let open = async {
        tokio::try_join!(
            state.dedicated_session(
                &sessions.keys[0],
                &args.left.connection_id,
                &args.left.database,
                true
            ),
            state.dedicated_session(
                &sessions.keys[1],
                &args.right.connection_id,
                &args.right.database,
                true
            )
        )
    };
    let (le, re) = tokio::select! {
        r = open => r?,
        () = ctl.stopped() => return Err(ctl.error()),
    };
    let hold = |side: &QuerySide| {
        let driver = state
            .store
            .get_connection(&side.connection_id)
            .ok()
            .flatten()
            .map(|c| c.config.driver);
        holds_its_reader(driver.as_deref())
    };
    let (lhold, rhold) = (hold(&args.left), hold(&args.right));
    let mut ls = le.session.lock().await;
    let mut rs = re.session.lock().await;
    let input = whole::Input {
        key: Vec::new(),
        wanted: Vec::new(),
        cols: [named(&pairs, 0), named(&pairs, 1)],
    };
    let how = whole::How {
        pairs: Some(pairs),
        rules: Arc::new(rules),
    };
    let c = whole::compare_with(
        whole::Source {
            session: &mut **ls,
            read: whole::Read::Query(&args.left.query),
            hold: lhold,
        },
        whole::Source {
            session: &mut **rs,
            read: whole::Read::Query(&args.right.query),
            hold: rhold,
        },
        input,
        how,
        opts,
        ctl,
        progress,
    )
    .await?;
    drop((ls, rs));
    drop(sessions);
    tracing::info!(
        left = c.counts.left,
        right = c.counts.right,
        on_disk = c.spilled,
        truncated = c.diff_truncated,
        "data compare (queries): done"
    );
    Ok(keep(args.left, args.right, c, opts.max_diff_rows))
}

/// The result for the UI, and the difference kept (as the table compare
/// keeps its own) for `data_compare_rows` and the AI.
fn keep(
    left: QuerySide,
    right: QuerySide,
    c: whole::Compared,
    max_diff_rows: usize,
) -> QueryCompareResult {
    let p = &c.projection;
    let type_of = |cols: &[ResultColumn], name: &str| {
        cols.iter()
            .find(|x| x.name == name)
            .or_else(|| cols.iter().find(|x| x.name.eq_ignore_ascii_case(name)))
            .map_or_else(String::new, |x| x.type_name.clone())
    };
    let left_types: Vec<String> = p
        .columns
        .iter()
        .map(|x| type_of(&c.query_columns[0], &x.0))
        .collect();
    let right_types: Vec<String> = p
        .columns
        .iter()
        .map(|x| type_of(&c.query_columns[1], &x.1))
        .collect();
    // What each result has that isn't compared.
    let unpaired = |side: usize| -> Vec<String> {
        c.query_columns[side]
            .iter()
            .filter(|col| {
                !p.columns.iter().any(|x| {
                    let n = if side == 0 { &x.0 } else { &x.1 };
                    n.eq_ignore_ascii_case(&col.name)
                })
            })
            .map(|col| col.name.clone())
            .collect()
    };
    let shown = |rows: &[Vec<Value>]| -> Vec<Vec<Value>> {
        rows.iter().take(data_compare::SHOWN).cloned().collect()
    };
    let id = uuid::Uuid::new_v4().to_string();
    let result = DataCompareResult {
        id: id.clone(),
        key: p.columns[..p.nk].iter().map(|x| x.0.clone()).collect(),
        columns: p.columns.iter().map(|x| x.0.clone()).collect(),
        types: left_types.clone(),
        only_left_columns: unpaired(0),
        only_right_columns: unpaired(1),
        counts: c.counts.clone(),
        only_left: shown(&c.only_left),
        only_right: shown(&c.only_right),
        changed: c
            .changed
            .iter()
            .take(data_compare::SHOWN)
            .cloned()
            .collect::<Vec<ChangedRow>>(),
        truncated_left: false,
        truncated_right: false,
        ordered: true,
        duplicate_keys: c.duplicate_keys,
        diff_truncated: c.diff_truncated,
        diff_limit: max_diff_rows as u64,
        listed: Listed {
            changed: c.changed.len() as u64,
            only_left: c.only_left.len() as u64,
            only_right: c.only_right.len() as u64,
        },
    };
    let out = QueryCompareResult {
        right_columns: p.columns.iter().map(|x| x.1.clone()).collect(),
        right_types: right_types.clone(),
        result,
    };
    let mut all = data_compare::diffs()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Keep the last few comparisons only.
    if all.len() > 8 {
        all.clear();
    }
    all.insert(
        id,
        Diff {
            left: kept_side(&left),
            right: kept_side(&right),
            key: out.result.key.clone(),
            columns: p.columns.clone(),
            left_identity: Vec::new(),
            right_identity: Vec::new(),
            only_left: c.only_left,
            only_right: c.only_right,
            changed: c.changed,
            left_types,
            right_types,
            partial: false,
            diff_truncated: c.diff_truncated,
            counts: c.counts,
            last_keys: c.last_keys,
        },
    );
    out
}

// ---------------------------------------------------------------------------
// A query's columns, before comparing

#[derive(Deserialize)]
pub struct QueryColumnsArgs {
    pub side: QuerySide,
    /// The UI's id for this read: `data_compare_cancel` stops it.
    #[serde(default)]
    pub run: String,
}

#[derive(Serialize, Debug)]
pub struct QueryColumns {
    /// The columns of the query's first result with columns.
    pub columns: Vec<ResultColumn>,
    /// Its first row, when it has one (what the suggested key looks at).
    pub sample: Option<Vec<Value>>,
}

/// The first result's columns and first row; the run stops at that row.
#[derive(Default)]
struct Probe {
    index: Option<usize>,
    columns: Vec<ResultColumn>,
    sample: Option<Vec<Value>>,
    /// A second result with columns came before the first had a row.
    more: bool,
    first: Arc<Notify>,
}

impl RowSink for Probe {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        if columns.is_empty() {
            return Ok(());
        }
        if self.index.is_none() {
            self.index = Some(index);
            self.columns = columns.to_vec();
        } else if self.index != Some(index) {
            self.more = true;
            self.first.notify_one();
        }
        Ok(())
    }

    fn row(&mut self, index: usize, row: &[Value]) -> std::io::Result<()> {
        if self.index == Some(index) && self.sample.is_none() {
            self.sample = Some(row.to_vec());
            self.first.notify_one();
        }
        Ok(())
    }
}

/// Removes a session when dropped, however the command ends.
struct Close<'a>(&'a AppState, String);

impl Drop for Close<'_> {
    fn drop(&mut self) {
        self.0.sessions.remove(&self.1);
    }
}

/// The columns of a query's result (and its first row), to pair them and
/// choose the key before comparing. The query runs on a read-only session
/// of its own and is stopped at its first row.
#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_query_columns(
    state: State<'_, AppState>,
    args: QueryColumnsArgs,
) -> CommandResult<QueryColumns> {
    let run = run_id(&args.run)?;
    check_side(&args.side, "la consulta")?;
    let ctl = Arc::new(whole::Ctl::default());
    let _running = Running::register(&run, ctl.clone())?;
    let key = format!("dcmp:{run}:columns");
    let _close = Close(state.inner(), key.clone());
    {
        let (st, k) = (state.inner().clone(), key.clone());
        ctl.add_stopper(Arc::new(move || {
            crate::commands::query::cancel_session(&st, &k)
        }));
    }
    let entry = tokio::select! {
        r = state.dedicated_session(&key, &args.side.connection_id, &args.side.database, true) => r?,
        () = ctl.stopped() => return Err(ctl.error()),
    };
    let mut s = entry.session.lock().await;
    probe(&mut **s, &args.side.query, &ctl, || {
        crate::commands::query::cancel_session(state.inner(), &key)
    })
    .await
}

/// Run `query` until its first row (or its end), keeping its first
/// result's columns; `stop` interrupts the run once that row came.
///
/// A run that fails once that row came still gives them: an engine that
/// cuts a result at the rows asked for may answer the cut as an error in
/// the same poll as the row (in process) or right after it (a host's row,
/// then its reply), and the compare runs the query again anyway, where a
/// real error shows.
async fn probe(
    s: &mut dyn dbine_driver::Session,
    query: &str,
    ctl: &whole::Ctl,
    stop: impl FnOnce(),
) -> CommandResult<QueryColumns> {
    let first = Arc::new(Notify::new());
    let sink = Arc::new(Mutex::new(Probe {
        first: first.clone(),
        ..Default::default()
    }));
    let mut out = QueryOutcome {
        sink: Some(RowSinkRef(sink.clone())),
        ..Default::default()
    };
    let ran = tokio::select! {
        r = s.execute(query, 1, &mut out) => Some(r),
        () = first.notified() => None,
        () = ctl.stopped() => return Err(ctl.error()),
    };
    out.sink = None;
    let sampled = || {
        let p = sink.lock().unwrap_or_else(|e| e.into_inner());
        p.index.is_some() && p.sample.is_some() && !p.more
    };
    match ran {
        // Enough: the rest of the run is of no use here.
        None => stop(),
        Some(r) => {
            let failed = r.err().map(|e| e.to_string()).or_else(|| out.error.take());
            if let Some(e) = failed {
                if !sampled() {
                    return Err(CommandError::Sql(e));
                }
                tracing::debug!(
                    "data compare: the probe's run failed after its first row; its columns stand"
                );
            }
        }
    }
    let mut p = sink.lock().unwrap_or_else(|e| e.into_inner());
    if p.more {
        return Err(CommandError::BadRequest(
            "la consulta devuelve más de un resultado: dejá una sola consulta".into(),
        ));
    }
    if p.index.is_none() {
        return Err(CommandError::BadRequest(
            "la consulta no devolvió un resultado con columnas".into(),
        ));
    }
    if let Some(name) = whole::repeated_column(&p.columns) {
        return Err(whole::repeated_column_error(None, name));
    }
    Ok(QueryColumns {
        columns: std::mem::take(&mut p.columns),
        sample: p.sample.take(),
    })
}

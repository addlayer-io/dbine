//! Data compare (docs/data-compare.md): the rows of two tables —
//! same or different connections, even different engines — matched by key:
//! rows only on one side, only on the other, and rows whose values differ.
//! The sync script is written by the target's driver (`insert_script`,
//! `update_script`, `delete_script`), so it's in the target's language; the
//! UI shows it and runs it only when the user says so.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::filter::{ColumnFilter, FilterOp};
use dbine_driver::{ColumnInfo, ObjectRef, RowChange, SortKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{Emitter, State};

#[path = "data_compare_whole.rs"]
pub(crate) mod whole;

#[cfg(test)]
#[path = "data_compare_live_tests.rs"]
mod live_tests;

/// Rows of a side kept in memory…
const IN_MEMORY_ROWS: usize = 250_000;
/// …and their estimated bytes: past either, both sides go to a scratch
/// DuckDB on disk (`whole`), which starts opening at half of them.
const IN_MEMORY_BYTES: usize = 256 << 20;
/// Differing rows the screen and the script get, all kinds together (the
/// counts are exact past it).
const MAX_DIFF_ROWS: usize = 1_000_000;
/// Rows of each kind sent to the UI (the counts and the script use them all).
const SHOWN: usize = 2_000;
/// Longest compare id the UI may give.
const MAX_RUN: usize = 128;

#[derive(Deserialize, Clone)]
pub struct Side {
    pub connection_id: String,
    pub database: String,
    pub object: ObjectRef,
}

#[derive(Deserialize)]
pub struct DataCompareArgs {
    pub left: Side,
    pub right: Side,
    /// Key columns; empty: the left table's primary key.
    #[serde(default)]
    pub key: Vec<String>,
    /// Columns to compare; empty: every column both sides have. (Older UIs
    /// also send a row `limit`: ignored, whole tables are compared.)
    #[serde(default)]
    pub columns: Vec<String>,
    /// The UI's id for this compare: its progress events carry it and
    /// `data_compare_cancel` stops it. Empty (older UIs): one is made up.
    #[serde(default)]
    pub run: String,
}

#[derive(Serialize, Clone)]
pub struct ChangedRow {
    pub left: Vec<Value>,
    pub right: Vec<Value>,
    /// Indexes (in `columns`) of the values that differ.
    pub diff: Vec<usize>,
}

#[derive(Serialize, Clone, Default, Debug, PartialEq, Eq)]
pub struct Counts {
    pub left: u64,
    pub right: u64,
    pub same: u64,
    pub only_left: u64,
    pub only_right: u64,
    pub changed: u64,
}

#[derive(Serialize, Clone)]
pub struct DataCompareResult {
    /// Id to ask for the sync script.
    pub id: String,
    pub key: Vec<String>,
    /// Compared columns (the key first), in the left table's names.
    pub columns: Vec<String>,
    /// Their types, on the left table (what a filter offers for each).
    pub types: Vec<String>,
    /// Columns only one side has (not compared).
    pub only_left_columns: Vec<String>,
    pub only_right_columns: Vec<String>,
    /// Over the whole tables: `same + changed + only_left` plus the left's
    /// repeated keys is `left`; likewise on the right.
    pub counts: Counts,
    pub only_left: Vec<Vec<Value>>,
    pub only_right: Vec<Vec<Value>>,
    pub changed: Vec<ChangedRow>,
    /// Always false: whole tables are compared (kept for older UIs).
    pub truncated_left: bool,
    pub truncated_right: bool,
    /// Always true: the lists come in key order (kept for older UIs).
    pub ordered: bool,
    /// Rows whose key was repeated on a side (compared once).
    pub duplicate_keys: u64,
    /// More rows differ than `diff_limit`: the screen and the script cover
    /// the first ones in key order (the counts are exact).
    pub diff_truncated: bool,
    pub diff_limit: u64,
    /// Rows of each kind in the lists the screen filters and the script
    /// syncs: `counts` unless `diff_truncated`.
    pub listed: Listed,
}

#[derive(Serialize, Clone, Default)]
pub struct Listed {
    pub changed: u64,
    pub only_left: u64,
    pub only_right: u64,
}

/// The whole difference, kept for the script (and for the AI's profile in
/// `data_compare_ai`).
pub(crate) struct Diff {
    pub(crate) left: Side,
    pub(crate) right: Side,
    pub(crate) key: Vec<String>,
    /// (left name, right name) of each compared column.
    pub(crate) columns: Vec<(String, String)>,
    /// Each side's identity / auto-increment columns: inserting their values
    /// needs the engine's wrap (SQL Server's IDENTITY_INSERT).
    pub(crate) left_identity: Vec<String>,
    pub(crate) right_identity: Vec<String>,
    pub(crate) only_left: Vec<Vec<Value>>,
    pub(crate) only_right: Vec<Vec<Value>>,
    pub(crate) changed: Vec<ChangedRow>,
    /// Each side's type of each compared column ("": not known), so a script
    /// writes a value the way its target column takes it.
    pub(crate) left_types: Vec<String>,
    pub(crate) right_types: Vec<String>,
    /// A side had more rows than were read: a row seen on one side may exist
    /// on the other past what was read, so it's never inserted or deleted.
    /// Whole tables are compared now, so it's false; it stays for the
    /// script's and the AI's checks.
    pub(crate) partial: bool,
    /// The lists hold the first differences only (`MAX_DIFF_ROWS`). Every
    /// row in them is confirmed on both tables, so all can be synced.
    pub(crate) diff_truncated: bool,
    /// The exact counts over the whole tables (the lists may hold fewer:
    /// `diff_truncated`).
    pub(crate) counts: Counts,
    /// A one-column key's last value on each side (NULL keys aside): whether
    /// the rows one side lacks come after the other's last.
    pub(crate) last_keys: Option<(Option<Value>, Option<Value>)>,
}

pub(crate) fn diffs() -> &'static Mutex<HashMap<String, Diff>> {
    static D: OnceLock<Mutex<HashMap<String, Diff>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

/// Whether a column holds booleans. PostgreSQL and its family answer them as
/// `t` and `f` in text, which no other engine's value equals, and which isn't
/// a value another engine's boolean column takes in a sync script.
pub(crate) fn is_bool_type(data_type: &str) -> bool {
    matches!(
        data_type.trim().to_ascii_lowercase().as_str(),
        "bool" | "boolean"
    )
}

/// A boolean column's value as a boolean; anything else stays as it came.
fn as_bool(v: Value) -> Value {
    let parsed = match &v {
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "1" => Some(true),
            "f" | "false" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    };
    parsed.map_or(v, Value::Bool)
}

/// Whether a column holds numbers, by the names engines give them: `int`,
/// `numeric(12,2)`, ClickHouse's `Nullable(Int32)`, BigQuery's `INT64`, or a
/// sampled document field's most common type (`int|string`).
fn is_number_type(data_type: &str) -> bool {
    let t = data_type.trim().to_ascii_lowercase();
    let t = t.split('|').next().unwrap_or_default();
    let t = t.strip_prefix("nullable(").unwrap_or(t);
    let head: String = t
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if matches!(
        head.as_str(),
        "int"
            | "integer"
            | "bigint"
            | "smallint"
            | "tinyint"
            | "mediumint"
            | "hugeint"
            | "long"
            | "short"
            | "byte"
            | "serial"
            | "bigserial"
            | "smallserial"
            | "varint"
            | "counter"
            | "number"
            | "numeric"
            | "decimal"
            | "dec"
            | "bignumeric"
            | "bigdecimal"
            | "real"
            | "float"
            | "double"
            | "half_float"
            | "scaled_float"
    ) {
        return true;
    }
    // Sized names: int32, uint64, float64, decimal128.
    ["int", "uint", "float", "decimal", "numeric"]
        .iter()
        .any(|p| {
            head.strip_prefix(p)
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// A number that came as text, as a number: only when that keeps it exact.
/// Past 2^53 or 15 significant digits it stays as text, which SQL engines
/// convert without losing anything.
fn as_number(v: Value) -> Value {
    let parsed = match &v {
        Value::String(s) => {
            let t = s.trim();
            let digits = t.trim_start_matches(['-', '+']);
            let plain = !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && digits.bytes().filter(|b| *b == b'.').count() <= 1
                && digits.bytes().any(|b| b.is_ascii_digit());
            let significant = digits
                .trim_start_matches(['0', '.'])
                .trim_end_matches(|c| c == '0' && digits.contains('.'))
                .bytes()
                .filter(u8::is_ascii_digit)
                .count();
            match t.parse::<f64>() {
                Ok(f) if plain && significant <= 15 => {
                    if f.fract() == 0.0 {
                        Some(Value::from(f as i64))
                    } else {
                        Some(Value::from(f))
                    }
                }
                _ => None,
            }
        }
        _ => None,
    };
    parsed.unwrap_or(v)
}

/// How a script writes one compared column's values: by the target column's
/// type, or the source's where the target doesn't say (an empty collection).
/// PostgreSQL answers numbers as text, and engines that keep types would
/// store them so: MongoDB's `"9"` isn't `9`, and CQL refuses it.
fn write_as(target_type: &str, source_type: &str, v: Value) -> Value {
    let known = |t: &str| !matches!(t.trim(), "" | "null");
    let t = if known(target_type) {
        target_type
    } else {
        source_type
    };
    if is_bool_type(t) {
        as_bool(v)
    } else if is_number_type(t) {
        as_number(v)
    } else {
        v
    }
}

/// A value as compared: numbers by value (`1` = `1.0` = `"1.00"`), dates
/// with `T` or a space alike, the rest as text.
pub(crate) fn canon(v: &Value) -> String {
    match v {
        Value::Null => "\u{0}null".into(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Number(n) => n.as_f64().map(num).unwrap_or_else(|| n.to_string()),
        Value::String(s) => {
            let t = s.trim();
            if let Ok(f) = t.parse::<f64>() {
                // A code with leading zeros ("007") is text, not a number.
                let leading_zero = t.len() > 1 && t.starts_with('0') && !t.starts_with("0.");
                if !t.is_empty()
                    && !leading_zero
                    && t.chars()
                        .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
                {
                    return num(f);
                }
            }
            // 2026-09-28T10:00:00.000Z ≈ 2026-09-28 10:00:00 ≈
            // 2026-09-28 10:00:00.0000000+00:00 (UTC, as `Cell::DateTimeTz`
            // spells it); another offset stays as written.
            if t.len() >= 19
                && t.as_bytes()[4] == b'-'
                && t.as_bytes()[7] == b'-'
                && matches!(t.as_bytes()[10], b'T' | b' ')
            {
                let mut d = t.replacen('T', " ", 1);
                for utc in ["Z", "+00:00"] {
                    if let Some(rest) = d.strip_suffix(utc) {
                        d = rest.to_string();
                        break;
                    }
                }
                if let Some(dot) = d.find('.') {
                    // Only the fraction's digits: an offset may follow them.
                    let end = d[dot + 1..]
                        .find(|c: char| !c.is_ascii_digit())
                        .map_or(d.len(), |i| dot + 1 + i);
                    let frac = d[dot + 1..end].trim_end_matches('0');
                    let sep = if frac.is_empty() { "" } else { "." };
                    d = format!("{}{sep}{frac}{}", &d[..dot], &d[end..]);
                }
                return d;
            }
            s.clone()
        }
        other => other.to_string(),
    }
}

fn num(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// The compares running now, by the UI's id: what `data_compare_cancel`
/// stops.
fn running() -> &'static Mutex<HashMap<String, Arc<whole::Ctl>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<whole::Ctl>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// A compare's place in [`running`], left when it ends.
struct Running(String);

impl Running {
    fn register(run: &str, ctl: Arc<whole::Ctl>) -> CommandResult<Running> {
        let mut all = running().lock().unwrap_or_else(|p| p.into_inner());
        if all.contains_key(run) {
            return Err(CommandError::BadRequest(
                "ya hay una comparación en curso con ese id".into(),
            ));
        }
        all.insert(run.to_string(), ctl);
        Ok(Running(run.to_string()))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        running()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

/// The sessions a compare opened (`state.sessions`), closed when it ends
/// however it ends.
struct Sessions<'a> {
    state: &'a AppState,
    keys: [String; 2],
}

impl Drop for Sessions<'_> {
    fn drop(&mut self) {
        for k in &self.keys {
            self.state.sessions.remove(k);
        }
    }
}

fn valid_run(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_RUN && s.chars().all(|c| !c.is_control())
}

/// Where the scratch DuckDBs of big compares go: a folder of their own next
/// to the vault, never the results store's (`results_store::clear` deletes
/// that one).
fn scratch_root() -> Option<std::path::PathBuf> {
    dbine_core::secrets::vault_dir().map(|d| d.join("compare"))
}

/// At startup: what a compare cut short by a crash left on disk.
pub(crate) fn purge_scratch() {
    if let Some(d) = scratch_root() {
        if d.exists() {
            if let Err(e) = std::fs::remove_dir_all(&d) {
                tracing::warn!(%e, "data compare: leftovers not deleted");
            }
        }
    }
}

/// What the UI gets while a compare runs (`data-compare-progress`), about
/// four times a second per side.
#[derive(Serialize, Clone)]
struct ProgressEvent<'a> {
    run: &'a str,
    side: &'static str,
    /// Rows read on that side.
    rows: u64,
    /// "reading", "preparing" (a read waits for the scratch DuckDB),
    /// "comparing" or "loading_differences".
    phase: &'static str,
    /// Differing rows brought back so far (while loading them).
    differences: u64,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    args: DataCompareArgs,
) -> CommandResult<DataCompareResult> {
    let run = if args.run.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else if valid_run(&args.run) {
        args.run.clone()
    } else {
        return Err(CommandError::BadRequest(
            "id de comparación no válido".into(),
        ));
    };
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
        in_memory_rows: IN_MEMORY_ROWS,
        in_memory_bytes: IN_MEMORY_BYTES,
        max_diff_rows: MAX_DIFF_ROWS,
        scratch: scratch_root(),
        duckdb: None,
    };
    compare_sides(&state, args, &run, &ctl, &opts, &progress).await
}

/// Stop a running compare: both reads and the DuckDB work; the compare
/// returns `Cancelled` and deletes what it wrote.
#[derive(Deserialize)]
pub struct DataCompareCancelArgs {
    pub run: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_cancel(args: DataCompareCancelArgs) -> CommandResult<()> {
    let ctl = running()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&args.run)
        .cloned();
    if let Some(ctl) = ctl {
        ctl.cancel();
    }
    Ok(())
}

/// Both sides on read-only sessions of their own (`dcmp:<run>:left|right`,
/// stopped by `cancel_session`), compared whole; the result, and the
/// difference kept for the script.
async fn compare_sides(
    state: &AppState,
    args: DataCompareArgs,
    run: &str,
    ctl: &Arc<whole::Ctl>,
    opts: &whole::Options,
    progress: &(dyn Fn(&whole::Progress) + Send + Sync),
) -> CommandResult<DataCompareResult> {
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
    let mut ls = le.session.lock().await;
    let mut rs = re.session.lock().await;
    let cols = async {
        tokio::try_join!(
            ls.columns(&args.left.object),
            rs.columns(&args.right.object)
        )
    };
    let (lcols, rcols) = tokio::select! {
        r = cols => r?,
        () = ctl.stopped() => return Err(ctl.error()),
    };
    // No key given: the left table's primary key.
    let key: Vec<String> = if args.key.is_empty() {
        lcols
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect()
    } else {
        args.key.clone()
    };
    if key.is_empty() {
        return Err(CommandError::BadRequest(
            "la tabla no tiene clave primaria: elegí las columnas que identifican cada fila".into(),
        ));
    }
    let identity = |cols: &[ColumnInfo]| -> Vec<String> {
        cols.iter()
            .filter(|c| c.auto_increment)
            .map(|c| c.name.clone())
            .collect()
    };
    let (left_identity, right_identity) = (identity(&lcols), identity(&rcols));
    let type_of = |cols: &[ColumnInfo], name: &str| {
        cols.iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .map_or_else(String::new, |c| c.data_type.clone())
    };
    let input = whole::Input {
        key,
        wanted: args.columns.clone(),
        cols: [lcols.clone(), rcols.clone()],
    };
    let c = whole::compare(
        whole::Side {
            session: &mut **ls,
            object: &args.left.object,
        },
        whole::Side {
            session: &mut **rs,
            object: &args.right.object,
        },
        input,
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
        "data compare: done"
    );
    let p = &c.projection;
    let left_types: Vec<String> = p.columns.iter().map(|x| type_of(&lcols, &x.0)).collect();
    let right_types: Vec<String> = p.columns.iter().map(|x| type_of(&rcols, &x.1)).collect();
    let id = uuid::Uuid::new_v4().to_string();
    let result = DataCompareResult {
        id: id.clone(),
        key: p.columns[..p.nk].iter().map(|x| x.0.clone()).collect(),
        columns: p.columns.iter().map(|x| x.0.clone()).collect(),
        types: left_types.clone(),
        only_left_columns: p.only_left_columns.clone(),
        only_right_columns: p.only_right_columns.clone(),
        counts: c.counts.clone(),
        only_left: c.only_left.iter().take(SHOWN).cloned().collect(),
        only_right: c.only_right.iter().take(SHOWN).cloned().collect(),
        changed: c.changed.iter().take(SHOWN).cloned().collect(),
        truncated_left: false,
        truncated_right: false,
        ordered: true,
        duplicate_keys: c.duplicate_keys,
        diff_truncated: c.diff_truncated,
        diff_limit: opts.max_diff_rows as u64,
        listed: Listed {
            changed: c.changed.len() as u64,
            only_left: c.only_left.len() as u64,
            only_right: c.only_right.len() as u64,
        },
    };
    let mut all = diffs().lock().unwrap();
    // Keep the last few comparisons only.
    if all.len() > 8 {
        all.clear();
    }
    all.insert(
        id,
        Diff {
            left: args.left,
            right: args.right,
            key: result.key.clone(),
            columns: p.columns.clone(),
            left_identity,
            right_identity,
            only_left: c.only_left,
            only_right: c.only_right,
            changed: c.changed,
            left_types,
            right_types,
            partial: false,
            diff_truncated: c.diff_truncated,
            counts: c.counts.clone(),
            last_keys: c.last_keys,
        },
    );
    Ok(result)
}

/// One kind of difference.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum RowKind {
    Changed,
    OnlyLeft,
    OnlyRight,
}

#[derive(Deserialize)]
pub struct DataRowsArgs {
    pub id: String,
    pub kind: RowKind,
    #[serde(default)]
    pub sort: Vec<SortKey>,
    #[serde(default)]
    pub filters: Vec<ColumnFilter>,
    /// Changed rows only: columns that have to differ in the row.
    #[serde(default)]
    pub changed_in: Vec<String>,
}

/// A row of a kind, with its place in that kind's whole list: what `Pick`
/// counts by.
#[derive(Serialize, Debug)]
pub struct DataRow {
    pub index: usize,
    /// The row's values; on a changed row, the left side's.
    pub left: Vec<Value>,
    /// A changed row's right side.
    pub right: Option<Vec<Value>>,
    /// A changed row's differing columns.
    pub diff: Vec<usize>,
}

#[derive(Serialize, Debug)]
pub struct DataRows {
    /// The first rows that pass, in the order asked for.
    pub rows: Vec<DataRow>,
    /// How many pass, among all the kind's rows (not only those shown).
    pub total: usize,
    /// With a filter, every row that passes: "all" marks those.
    pub indices: Vec<usize>,
}

/// A value to sort by: numbers by value, the rest as text without case, and
/// NULL last whichever way it goes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SortValue {
    Number(f64),
    Text(String),
    Null,
}

impl SortValue {
    pub(crate) fn cmp_with(&self, other: &Self, descending: bool) -> std::cmp::Ordering {
        use std::cmp::Ordering::{Greater, Less};
        let order = match (self, other) {
            (SortValue::Null, SortValue::Null) => std::cmp::Ordering::Equal,
            (SortValue::Null, _) => return Greater,
            (_, SortValue::Null) => return Less,
            (SortValue::Number(a), SortValue::Number(b)) => a.total_cmp(b),
            (SortValue::Number(_), SortValue::Text(_)) => Less,
            (SortValue::Text(_), SortValue::Number(_)) => Greater,
            (SortValue::Text(a), SortValue::Text(b)) => a.cmp(b),
        };
        if descending {
            order.reverse()
        } else {
            order
        }
    }
}

/// A value as a number, if it reads as one (PostgreSQL's come as text).
pub(crate) fn number_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = s.trim();
            let plain = !t.is_empty()
                && t.bytes()
                    .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+' | b'e' | b'E'));
            if plain {
                t.parse::<f64>().ok().filter(|f| f.is_finite())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A value as text, without case (dates with `T` or a space alike).
pub(crate) fn text_of(v: &Value) -> String {
    canon(v).to_lowercase()
}

pub(crate) fn sort_value(v: &Value) -> SortValue {
    match v {
        Value::Null => SortValue::Null,
        _ => number_of(v).map_or_else(|| SortValue::Text(text_of(v)), SortValue::Number),
    }
}

/// `v` against what a filter compares with: by number if both are, else as
/// text without case.
fn compare(v: &Value, w: &Value) -> std::cmp::Ordering {
    match (number_of(v), number_of(w)) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        _ => text_of(v).cmp(&text_of(w)),
    }
}

fn same_value(v: &Value, w: &Value) -> bool {
    if compare(v, w).is_eq() {
        return true;
    }
    // A date against a whole day: `2024-03-11` takes all of its hours.
    let day = text_of(w);
    let is_day = day.len() == 10
        && day.as_bytes()[4] == b'-'
        && day.as_bytes()[7] == b'-'
        && day.bytes().filter(u8::is_ascii_digit).count() == 8;
    is_day && text_of(v).starts_with(&format!("{day} "))
}

/// A value against a filter (its SQL kinds aren't offered here: they pass).
fn passes(v: &Value, f: &ColumnFilter) -> bool {
    use std::cmp::Ordering::{Greater, Less};
    let null = v.is_null();
    let text = text_of(v);
    let empty = Value::Null;
    let w = f.values.first().unwrap_or(&empty);
    let word = text_of(w);
    let truthy = matches!(as_bool(v.clone()), Value::Bool(true));
    let falsy = matches!(as_bool(v.clone()), Value::Bool(false));
    match f.op {
        FilterOp::Eq => !null && same_value(v, w),
        FilterOp::Ne => !null && !same_value(v, w),
        FilterOp::Gt => !null && compare(v, w) == Greater,
        FilterOp::Ge => !null && compare(v, w) != Less,
        FilterOp::Lt => !null && compare(v, w) == Less,
        FilterOp::Le => !null && compare(v, w) != Greater,
        FilterOp::Contains => !null && text.contains(&word),
        FilterOp::NotContains => !null && !text.contains(&word),
        FilterOp::StartsWith => !null && text.starts_with(&word),
        FilterOp::EndsWith => !null && text.ends_with(&word),
        FilterOp::IsNull => null,
        FilterOp::NotNull => !null,
        FilterOp::IsEmpty => v.as_str().is_some_and(str::is_empty),
        FilterOp::NotEmpty => !null && !v.as_str().is_some_and(str::is_empty),
        FilterOp::In => !null && f.values.iter().any(|x| same_value(v, x)),
        FilterOp::NotIn => !null && !f.values.iter().any(|x| same_value(v, x)),
        FilterOp::IsTrue => truthy,
        FilterOp::IsFalse => falsy,
        FilterOp::TrueOrNull => null || truthy,
        FilterOp::FalseOrNull => null || falsy,
        _ => true,
    }
}

/// A row of a kind: its place in the list, left, right, differing columns.
type RowRef<'a> = (usize, &'a [Value], Option<&'a [Value]>, &'a [usize]);

/// One kind of difference, filtered and sorted: over all its rows, not only
/// those the first answer brought. On a changed row a filter passes if either
/// side does, and it sorts by the left side, then the right.
fn rows_of(d: &Diff, args: &DataRowsArgs) -> DataRows {
    let index_of = |name: &str| {
        d.columns
            .iter()
            .position(|(l, _)| l.eq_ignore_ascii_case(name))
    };
    let filters: Vec<(usize, &ColumnFilter)> = args
        .filters
        .iter()
        .filter_map(|f| index_of(&f.column).map(|i| (i, f)))
        .collect();
    let changed_in: Vec<usize> = args.changed_in.iter().filter_map(|c| index_of(c)).collect();
    let sort: Vec<(usize, bool)> = args
        .sort
        .iter()
        .filter_map(|k| index_of(&k.column).map(|i| (i, k.descending)))
        .collect();
    let empty = Value::Null;
    let cell = |r: &[Value], i: usize| r.get(i).cloned().unwrap_or_else(|| empty.clone());
    let all: Vec<RowRef> = match args.kind {
        RowKind::Changed => d
            .changed
            .iter()
            .enumerate()
            .map(|(i, r)| (i, &r.left[..], Some(&r.right[..]), &r.diff[..]))
            .collect(),
        RowKind::OnlyLeft => d
            .only_left
            .iter()
            .enumerate()
            .map(|(i, r)| (i, &r[..], None, &[][..]))
            .collect(),
        RowKind::OnlyRight => d
            .only_right
            .iter()
            .enumerate()
            .map(|(i, r)| (i, &r[..], None, &[][..]))
            .collect(),
    };
    let mut passing: Vec<_> = all
        .into_iter()
        .filter(|(_, left, right, diff)| {
            changed_in.iter().all(|c| diff.contains(c))
                && filters.iter().all(|(i, f)| {
                    passes(&cell(left, *i), f) || right.is_some_and(|r| passes(&cell(r, *i), f))
                })
        })
        .collect();
    if !sort.is_empty() {
        // The keys once per row: reading a value as a number isn't free.
        let mut keyed: Vec<_> = passing
            .drain(..)
            .map(|row| {
                let keys: Vec<(SortValue, Option<SortValue>)> = sort
                    .iter()
                    .map(|(i, _)| {
                        (
                            sort_value(&cell(row.1, *i)),
                            row.2.map(|r| sort_value(&cell(r, *i))),
                        )
                    })
                    .collect();
                (keys, row)
            })
            .collect();
        keyed.sort_by(|(a, _), (b, _)| {
            for (n, (_, descending)) in sort.iter().enumerate() {
                let by_left = a[n].0.cmp_with(&b[n].0, *descending);
                if by_left.is_ne() {
                    return by_left;
                }
                if let (Some(x), Some(y)) = (&a[n].1, &b[n].1) {
                    let by_right = x.cmp_with(y, *descending);
                    if by_right.is_ne() {
                        return by_right;
                    }
                }
            }
            std::cmp::Ordering::Equal
        });
        passing = keyed.into_iter().map(|(_, row)| row).collect();
    }
    let total = passing.len();
    let indices = if filters.is_empty() && changed_in.is_empty() {
        Vec::new()
    } else {
        passing.iter().map(|r| r.0).collect()
    };
    let rows = passing
        .into_iter()
        .take(SHOWN)
        .map(|(index, left, right, diff)| DataRow {
            index,
            left: left.to_vec(),
            right: right.map(<[Value]>::to_vec),
            diff: diff.to_vec(),
        })
        .collect();
    DataRows {
        rows,
        total,
        indices,
    }
}

/// The rows of a comparison kept in memory, filtered and sorted.
#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_rows(args: DataRowsArgs) -> CommandResult<DataRows> {
    tokio::task::spawn_blocking(move || {
        let all = diffs().lock().unwrap();
        let d = all.get(&args.id).ok_or_else(|| {
            CommandError::NotFound("la comparación ya no está: volvé a comparar".into())
        })?;
        Ok(rows_of(d, &args))
    })
    .await
    .map_err(|e| CommandError::Internal(e.to_string()))?
}

#[derive(Deserialize)]
pub struct DataScriptArgs {
    pub id: String,
    /// What to do with each row that differs.
    pub choices: Choices,
}

/// Where a row goes: `Right` makes the right table like the left for that
/// row (update it, insert it there, or delete it there when only the right
/// has it); `Left` the other way; `None` leaves it alone.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    #[default]
    None,
    Right,
    Left,
}

/// The choice for one kind of difference: `all` for every row, `rows`
/// overriding some (index into that kind's full list, as the UI shows it).
#[derive(Deserialize, Default, Debug)]
pub struct Pick {
    #[serde(default)]
    pub all: Dir,
    #[serde(default)]
    pub rows: Vec<(usize, Dir)>,
}

impl Pick {
    /// Each row's direction, in order.
    fn resolve(&self, n: usize) -> Vec<Dir> {
        let mut dirs = vec![self.all; n];
        for &(i, d) in &self.rows {
            if let Some(slot) = dirs.get_mut(i) {
                *slot = d;
            }
        }
        dirs
    }
}

#[derive(Deserialize, Default, Debug)]
pub struct Choices {
    #[serde(default)]
    pub changed: Pick,
    #[serde(default)]
    pub only_left: Pick,
    #[serde(default)]
    pub only_right: Pick,
}

#[derive(Serialize)]
pub struct DataScript {
    /// Where it runs.
    pub connection_id: String,
    pub database: String,
    /// "left" or "right": the side it changes.
    pub side: &'static str,
    pub script: String,
    pub inserts: u64,
    pub updates: u64,
    pub deletes: u64,
    /// Its statements as the editor cuts them ([`dbine_driver::Driver::split_script`]):
    /// the units the run reports as they end (`query-progress`), so the
    /// task knows its total.
    pub statements: u64,
    /// The target's sessions take manual transactions
    /// ([`dbine_driver::Driver::supports_manual_transactions`]): the run
    /// goes statement by statement inside one transaction, committed only
    /// when every statement ran, so a failure leaves the side as it was.
    /// Without them the script goes to the driver whole, in one call.
    pub atomic: bool,
}

/// The rows each side gets, from the choices.
#[derive(Default)]
struct Plan<'a> {
    /// Rows to delete (whole rows of that side).
    delete: Vec<&'a Vec<Value>>,
    /// (source row, target row, differing columns).
    update: Vec<(&'a Vec<Value>, &'a Vec<Value>, &'a [usize])>,
    /// Rows to insert (the other side's).
    insert: Vec<Vec<Value>>,
}

/// What the choices do to each side: `(right, left)`. A partial comparison
/// only updates: a row seen on one side may exist on the other past what was
/// read, so inserting it could collide and deleting it would lose it.
fn plans<'a>(d: &'a Diff, c: &Choices) -> CommandResult<(Plan<'a>, Plan<'a>)> {
    let chosen = |p: &Pick, n: usize| p.resolve(n).iter().any(|d| *d != Dir::None);
    if d.partial
        && (chosen(&c.only_left, d.only_left.len()) || chosen(&c.only_right, d.only_right.len()))
    {
        return Err(CommandError::BadRequest(
            "la comparación es parcial: una fila de un solo lado puede existir del otro más allá de lo leído, así que no se inserta ni se borra".into(),
        ));
    }
    let (mut right, mut left) = (Plan::default(), Plan::default());
    for (row, dir) in d.changed.iter().zip(c.changed.resolve(d.changed.len())) {
        match dir {
            Dir::Right => right.update.push((&row.left, &row.right, &row.diff)),
            Dir::Left => left.update.push((&row.right, &row.left, &row.diff)),
            Dir::None => {}
        }
    }
    for (row, dir) in d
        .only_left
        .iter()
        .zip(c.only_left.resolve(d.only_left.len()))
    {
        match dir {
            Dir::Right => right.insert.push(row.clone()),
            Dir::Left => left.delete.push(row),
            Dir::None => {}
        }
    }
    for (row, dir) in d
        .only_right
        .iter()
        .zip(c.only_right.resolve(d.only_right.len()))
    {
        match dir {
            Dir::Left => left.insert.push(row.clone()),
            Dir::Right => right.delete.push(row),
            Dir::None => {}
        }
    }
    Ok((right, left))
}

/// The scripts that apply the chosen rows, one per side that changes, each
/// in that side's language. Nothing runs here.
#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_script(
    state: State<'_, AppState>,
    args: DataScriptArgs,
) -> CommandResult<Vec<DataScript>> {
    let all = diffs().lock().unwrap();
    let d = all.get(&args.id).ok_or_else(|| {
        CommandError::NotFound("la comparación ya no está: volvé a comparar".into())
    })?;
    let (right, left) = plans(d, &args.choices)?;
    let mut out = Vec::new();
    for (to_right, plan) in [(false, left), (true, right)] {
        if plan.delete.is_empty() && plan.update.is_empty() && plan.insert.is_empty() {
            continue;
        }
        out.push(side_script(&state, d, to_right, plan)?);
    }
    Ok(out)
}

/// One side's script: deletes, then updates, then inserts.
fn side_script(
    state: &AppState,
    d: &Diff,
    to_right: bool,
    plan: Plan<'_>,
) -> CommandResult<DataScript> {
    let target = if to_right { &d.right } else { &d.left };
    let cfg = state
        .store
        .get_connection(&target.connection_id)?
        .ok_or_else(|| CommandError::NotFound("la conexión destino ya no existe".into()))?
        .config;
    let driver = dbine_drivers::find(&cfg.driver)
        .ok_or_else(|| CommandError::BadRequest(format!("no hay driver '{}'", cfg.driver)))?;
    // The target's column names.
    let names: Vec<String> = d
        .columns
        .iter()
        .map(|(l, r)| if to_right { r.clone() } else { l.clone() })
        .collect();
    let nk = d.key.len();
    let (target_types, source_types) = if to_right {
        (&d.right_types, &d.left_types)
    } else {
        (&d.left_types, &d.right_types)
    };
    // Every value as the target column takes it.
    let typed = |i: usize, v: &Value| write_as(&target_types[i], &source_types[i], v.clone());
    let typed_row =
        |r: &[Value]| -> Vec<Value> { r.iter().enumerate().map(|(i, v)| typed(i, v)).collect() };
    let key_of = |r: &Vec<Value>| -> Vec<(String, Value)> {
        names[..nk]
            .iter()
            .cloned()
            .zip(typed_row(&r[..nk]))
            .collect()
    };
    let mut parts = Vec::new();
    if !plan.delete.is_empty() {
        let keys: Vec<Vec<(String, Value)>> = plan.delete.iter().map(|r| key_of(r)).collect();
        parts.push(driver.delete_script(&target.object, &keys)?);
    }
    if !plan.update.is_empty() {
        let changes: Vec<RowChange> = plan
            .update
            .iter()
            .map(|(src, dst, diff)| RowChange {
                key: key_of(dst),
                set: diff
                    .iter()
                    .map(|&i| (names[i].clone(), typed(i, &src[i])))
                    .collect(),
                row: names.iter().cloned().zip(typed_row(dst)).collect(),
            })
            .collect();
        parts.push(driver.update_script(&target.object, &changes)?);
    }
    if !plan.insert.is_empty() {
        let identity = if to_right {
            &d.right_identity
        } else {
            &d.left_identity
        };
        parts.extend(insert_parts(
            driver.as_ref(),
            &target.object,
            &names,
            identity,
            &plan.insert.iter().map(|r| typed_row(r)).collect::<Vec<_>>(),
        )?);
    }
    let (script, statements) = assemble(driver.as_ref(), parts);
    Ok(DataScript {
        connection_id: target.connection_id.clone(),
        database: target.database.clone(),
        side: if to_right { "right" } else { "left" },
        inserts: plan.insert.len() as u64,
        updates: plan.update.len() as u64,
        deletes: plan.delete.len() as u64,
        statements,
        atomic: driver.supports_manual_transactions(),
        script,
    })
}

/// The parts joined by the engine's separator, and how many statements the
/// editor cuts the result into.
/// The inserts of a sync. Rows copied with their identity values need the
/// engine's wrap: SQL Server refuses them unless IDENTITY_INSERT is on
/// (error 544); other engines wrap nothing.
fn insert_parts(
    driver: &dyn dbine_driver::Driver,
    target: &ObjectRef,
    names: &[String],
    identity: &[String],
    rows: &[Vec<Value>],
) -> CommandResult<Vec<String>> {
    let inserts = driver.insert_script(target, names, rows)?;
    let (before, after) = if names
        .iter()
        .any(|n| identity.iter().any(|i| i.eq_ignore_ascii_case(n)))
    {
        driver.data_load_wrap(&identity_table(target, identity))
    } else {
        (String::new(), String::new())
    };
    Ok([before, inserts, after]
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect())
}

/// New rows from the results grid ("Agregar fila", "Agregar documento") as
/// the engine's insert code, in the order they were added: consecutive rows
/// that set the same columns go in one `insert_script` call, and a batch
/// that sets an identity column gets the engine's wrap. No rows: it only
/// asks whether the engine writes inserts (with the result's `columns`, as
/// a row of NULLs: IoTDB needs its Time column), and only its
/// `Unsupported` is the reason the UI shows.
pub(crate) fn new_rows_parts(
    driver: &dyn dbine_driver::Driver,
    target: &ObjectRef,
    rows: &[Vec<(String, Value)>],
    identity: &[String],
    columns: &[String],
) -> CommandResult<Vec<String>> {
    if rows.is_empty() {
        let probe: Vec<Vec<Value>> = if columns.is_empty() {
            Vec::new()
        } else {
            vec![vec![Value::Null; columns.len()]]
        };
        return match driver.insert_script(target, columns, &probe) {
            Err(e @ dbine_driver::Error::Unsupported(_)) => Err(e.into()),
            _ => Ok(Vec::new()),
        };
    }
    let rows: Vec<&Vec<(String, Value)>> = rows.iter().filter(|r| !r.is_empty()).collect();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let names: Vec<String> = rows[i].iter().map(|(n, _)| n.clone()).collect();
        let mut batch = Vec::new();
        while i < rows.len() && rows[i].iter().map(|(n, _)| n).eq(names.iter()) {
            batch.push(rows[i].iter().map(|(_, v)| v.clone()).collect());
            i += 1;
        }
        parts.extend(insert_parts(driver, target, &names, identity, &batch)?);
    }
    Ok(parts)
}

/// The target as `data_load_wrap` takes it: only which columns are identity.
fn identity_table(object: &ObjectRef, identity: &[String]) -> dbine_driver::TableSchema {
    dbine_driver::TableSchema {
        schema: object.schema.clone(),
        name: object.name.clone(),
        columns: identity
            .iter()
            .map(|n| dbine_driver::ColumnDef {
                name: n.clone(),
                auto_increment: true,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn assemble(driver: &dyn dbine_driver::Driver, parts: Vec<String>) -> (String, u64) {
    let sep = driver.script_separator();
    let script = parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join(&format!("\n{sep}\n"));
    let statements = driver.split_script(&script).len() as u64;
    (script, statements)
}

#[cfg(test)]
mod tests {
    use super::{
        as_bool, as_number, assemble, canon, insert_parts, is_bool_type, is_number_type,
        new_rows_parts, plans, rows_of, write_as, ChangedRow, Choices, DataRowsArgs, Diff, Dir,
        Pick, RowKind, Side, SHOWN,
    };
    use dbine_driver::filter::{ColumnFilter, FilterOp};
    use dbine_driver::SortKey;
    use dbine_driver::{ObjectRef, RowChange};

    /// The statement count a sync run reports against: joining the parts
    /// neither merges nor adds statements, on every SQL engine.
    #[test]
    fn inserts_into_an_identity_column_are_wrapped() {
        let target = ObjectRef {
            kind: "table".into(),
            schema: Some("dbo".into()),
            name: "destino".into(),
        };
        let names = vec!["id".to_string(), "codigo".to_string()];
        let rows = vec![vec![json!(5), json!("a")]];
        let mssql = dbine_drivers::find("sqlserver").unwrap();
        let parts = insert_parts(mssql.as_ref(), &target, &names, &["id".into()], &rows).unwrap();
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0], "SET IDENTITY_INSERT [dbo].[destino] ON;");
        assert!(parts[1].starts_with("INSERT INTO [dbo].[destino]"));
        assert_eq!(parts[2], "SET IDENTITY_INSERT [dbo].[destino] OFF;");
        // No identity column among the inserted ones: nothing to wrap.
        assert_eq!(
            insert_parts(mssql.as_ref(), &target, &names, &[], &rows)
                .unwrap()
                .len(),
            1
        );
        // PostgreSQL takes the values as they come, then moves the sequence
        // past them, so the next insert doesn't collide with a copied id.
        let pg = dbine_drivers::find("postgres").unwrap();
        let parts = insert_parts(pg.as_ref(), &target, &names, &["id".into()], &rows).unwrap();
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(
            parts[0].starts_with("INSERT INTO")
                && parts[1].contains("setval(pg_get_serial_sequence"),
            "{parts:?}"
        );
    }

    #[test]
    fn grid_new_rows_batch_by_columns_and_wrap_identity() {
        let target = ObjectRef {
            kind: "table".into(),
            schema: Some("dbo".into()),
            name: "clientes".into(),
        };
        let mssql = dbine_drivers::find("sqlserver").unwrap();
        let rows = vec![
            vec![("nombre".to_string(), json!("Ana"))],
            vec![("nombre".to_string(), json!("Luis"))],
            vec![
                ("id".to_string(), json!(9)),
                ("nombre".to_string(), json!("Eva")),
            ],
            vec![],
        ];
        let parts = new_rows_parts(mssql.as_ref(), &target, &rows, &["id".into()], &[]).unwrap();
        // The first two share their columns (one INSERT batch, no identity);
        // the third sets the identity column: wrapped.
        assert_eq!(parts.len(), 4, "{parts:?}");
        assert!(
            parts[0].contains("N'Ana'") && parts[0].contains("N'Luis'"),
            "{parts:?}"
        );
        assert_eq!(parts[1], "SET IDENTITY_INSERT [dbo].[clientes] ON;");
        assert!(
            parts[2].contains("[id]") && parts[2].contains("N'Eva'"),
            "{parts:?}"
        );
        assert_eq!(parts[3], "SET IDENTITY_INSERT [dbo].[clientes] OFF;");
        // A document with nested fields goes to MongoDB as it is.
        let mongo = dbine_drivers::find("mongodb").unwrap();
        let coll = ObjectRef {
            kind: "collection".into(),
            schema: None,
            name: "pedidos".into(),
        };
        let doc = vec![vec![
            ("cliente".to_string(), json!({"nombre": "Ana"})),
            ("items".to_string(), json!([1, 2])),
        ]];
        let parts = new_rows_parts(mongo.as_ref(), &coll, &doc, &[], &[]).unwrap();
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert!(
            parts[0].contains("pedidos") && parts[0].contains("\"nombre\""),
            "{parts:?}"
        );
        // No rows: only whether the engine writes inserts.
        assert!(new_rows_parts(mssql.as_ref(), &target, &[], &[], &[])
            .unwrap()
            .is_empty());
        assert!(new_rows_parts(mongo.as_ref(), &coll, &[], &[], &[])
            .unwrap()
            .is_empty());
    }

    /// Every engine either writes a new row's insert code or says why not
    /// (`Unsupported`, the reason "Agregar fila" shows); none fails the
    /// empty probe the UI sends to find out.
    #[test]
    fn every_engine_writes_new_rows_or_says_why() {
        let mut unsupported = Vec::new();
        let mut failed = Vec::new();
        for d in dbine_drivers::all() {
            let id = d.info().id;
            // IoTDB (and TimechoDB) write to a device under a database path,
            // with a Time column.
            let tsdb = matches!(id, "iotdb" | "timechodb");
            let schema = tsdb.then(|| "root.ventas".to_string());
            let columns: Vec<String> = if tsdb {
                vec!["Time".into(), "nombre".into()]
            } else {
                vec!["id".into(), "nombre".into()]
            };
            let row = vec![columns
                .iter()
                .cloned()
                .zip([json!(1), json!("Ana")])
                .collect::<Vec<_>>()];
            let target = ObjectRef {
                kind: "table".into(),
                schema,
                name: "clientes".into(),
            };
            match new_rows_parts(d.as_ref(), &target, &[], &[], &columns) {
                Ok(p) => assert!(p.is_empty(), "{id}: {p:?}"),
                Err(e) => {
                    unsupported.push(id);
                    // An engine that can't insert says so for real rows too.
                    assert!(
                        new_rows_parts(d.as_ref(), &target, &row, &[], &[]).is_err(),
                        "{id}: probe failed ({e:?}) but rows worked"
                    );
                    continue;
                }
            }
            match new_rows_parts(d.as_ref(), &target, &row, &[], &[]) {
                Ok(parts) => assert!(parts.iter().any(|p| p.contains("Ana")), "{id}: {parts:?}"),
                Err(e) => failed.push(format!("{id}: {e:?}")),
            }
        }
        eprintln!("engines without inserts: {unsupported:?}");
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// A nested field edited in the grid or the JSON tree reaches document
    /// engines as an object (the UI parses its JSON text): the update code
    /// writes it as a document, not as a quoted string.
    #[test]
    fn document_engines_write_an_edited_nested_field_as_an_object() {
        let coll = ObjectRef {
            kind: "collection".into(),
            schema: None,
            name: "pedidos".into(),
        };
        let change = RowChange {
            key: vec![("_id".into(), json!("65a1b2c3d4e5f60718293a4b"))],
            set: vec![(
                "cliente".into(),
                json!({"nombre": "Ana", "contacto": {"email": "ana@ejemplo.com"}}),
            )],
            row: vec![],
        };
        for id in ["mongodb", "couchdb", "elasticsearch"] {
            let d = dbine_drivers::find(id).unwrap();
            let script = d
                .update_script(&coll, std::slice::from_ref(&change))
                .unwrap();
            assert!(script.contains("ana@ejemplo.com"), "{id}: {script}");
            assert!(
                !script.contains("\\\"email\\\"") && !script.contains("\"{\\\""),
                "{id}: nested field went as a string: {script}"
            );
        }
    }

    #[test]
    fn statement_count_is_the_sum_of_the_parts() {
        let target = ObjectRef {
            kind: "table".into(),
            schema: Some("s".into()),
            name: "t".into(),
        };
        let cols = vec!["id".to_string(), "name".to_string()];
        let rows: Vec<Vec<serde_json::Value>> = (1..=3)
            .map(|i| vec![json!(i), json!(format!("n{i}"))])
            .collect();
        let changes: Vec<RowChange> = (1..=2)
            .map(|i| RowChange {
                key: vec![("id".into(), json!(i))],
                set: vec![("name".into(), json!("x"))],
                row: vec![("id".into(), json!(i)), ("name".into(), json!("y"))],
            })
            .collect();
        let keys = vec![vec![("id".to_string(), json!(9))]];
        let mut checked = 0;
        for d in dbine_drivers::all() {
            let parts: Vec<String> = [
                d.delete_script(&target, &keys),
                d.update_script(&target, &changes),
                d.insert_script(&target, &cols, &rows),
            ]
            .into_iter()
            .filter_map(Result::ok)
            .collect();
            if parts.is_empty() {
                continue;
            }
            let each: u64 = parts.iter().map(|p| d.split_script(p).len() as u64).sum();
            let (script, statements) = assemble(d.as_ref(), parts);
            // Engines that split by their own steps (MongoDB, Redis, Solr…)
            // don't cut by `split_script`: their count is a floor and the
            // run's progress is clamped to it.
            if d.info().language == dbine_driver::Language::Sql {
                assert_eq!(statements, each, "{}: {script}", d.info().id);
            }
            assert!(statements > 0, "{}", d.info().id);
            checked += 1;
        }
        assert!(checked > 5, "only {checked} drivers write scripts");
    }

    fn diff(partial: bool) -> Diff {
        let side = || Side {
            connection_id: "c".into(),
            database: "d".into(),
            object: ObjectRef {
                kind: "table".into(),
                schema: None,
                name: "t".into(),
            },
        };
        Diff {
            left: side(),
            right: side(),
            key: vec!["id".into()],
            columns: vec![("id".into(), "id".into()), ("a".into(), "a".into())],
            left_identity: vec![],
            right_identity: vec![],
            only_left: vec![vec![json!(1), json!("x")]],
            only_right: vec![vec![json!(2), json!("y")]],
            changed: vec![ChangedRow {
                left: vec![json!(3), json!("l")],
                right: vec![json!(3), json!("r")],
                diff: vec![1],
            }],
            left_types: vec!["int".into(), "text".into()],
            right_types: vec!["int".into(), "text".into()],
            partial,
            diff_truncated: false,
            counts: super::Counts::default(),
            last_keys: None,
        }
    }

    fn pick(dir: Dir) -> Pick {
        Pick {
            all: dir,
            rows: vec![],
        }
    }

    #[test]
    fn a_boolean_is_worth_the_same_on_every_engine() {
        // PostgreSQL's text, SQL Server's boolean, MySQL's tinyint.
        for (pg, other) in [
            (json!("t"), json!(true)),
            (json!("f"), json!(false)),
            (json!("t"), json!(1)),
            (json!("f"), json!(0)),
            (json!("true"), json!(true)),
        ] {
            assert_eq!(canon(&as_bool(pg.clone())), canon(&other), "{pg} {other}");
        }
        assert_ne!(canon(&as_bool(json!("t"))), canon(&json!(false)));
        // Text that isn't a boolean, and NULL, stay as they came.
        assert_eq!(as_bool(json!("tal vez")), json!("tal vez"));
        assert_eq!(as_bool(json!(null)), json!(null));
        // Only boolean columns: a text column can hold a `t` that means `t`.
        assert!(is_bool_type("boolean") && is_bool_type("BOOL"));
        assert!(!is_bool_type("character varying(10)") && !is_bool_type("bit"));
    }

    #[test]
    fn a_number_as_text_becomes_a_number_only_when_exact() {
        for (text, number) in [
            ("9", json!(9)),
            ("-12", json!(-12)),
            ("007", json!(7)),
            ("45000.00", json!(45000)),
            ("220000.50", json!(220000.5)),
            ("0.25", json!(0.25)),
        ] {
            assert_eq!(as_number(json!(text)), number, "{text}");
        }
        // More than a double keeps: stays as text.
        for kept in [
            "9007199254740991",
            "12345678901234567890",
            "1234567890.123456789",
            "$1,000.00",
            "1e5",
            "",
            ".",
        ] {
            assert_eq!(as_number(json!(kept)), json!(kept), "{kept}");
        }
        // Not text: as it came.
        assert_eq!(as_number(json!(5)), json!(5));
        assert_eq!(as_number(json!(null)), json!(null));
    }

    #[test]
    fn number_types_by_the_names_engines_give_them() {
        for t in [
            "int",
            "integer",
            "bigint",
            "INT64",
            "UInt64",
            "Float64",
            "numeric(12,2)",
            "decimal(12,2)",
            "NUMBER(10,2)",
            "Nullable(Int32)",
            "double precision",
            "tinyint(1)",
            "int|double",
            "double",
            "scaled_float",
        ] {
            assert!(is_number_type(t), "{t}");
        }
        for t in [
            "text",
            "character varying(80)",
            "interval",
            "string|int",
            "date",
            "uuid",
            "",
            "null",
            "bit",
        ] {
            assert!(!is_number_type(t), "{t}");
        }
    }

    #[test]
    fn a_script_writes_a_value_the_way_its_target_takes_it() {
        // PostgreSQL's text into MongoDB's numbers, and into CQL's.
        assert_eq!(write_as("int", "integer", json!("9")), json!(9));
        assert_eq!(
            write_as("double", "numeric(12,2)", json!("45000.00")),
            json!(45000)
        );
        // A text column keeps its text, even if it looks like a number.
        assert_eq!(
            write_as("varchar(10)", "integer", json!("007")),
            json!("007")
        );
        // A target that doesn't say (an empty collection): by the source.
        assert_eq!(write_as("null", "integer", json!("9")), json!(9));
        assert_eq!(write_as("", "integer", json!("9")), json!(9));
        assert_eq!(write_as("", "text", json!("9")), json!("9"));
        // MySQL's tinyint into PostgreSQL's boolean.
        assert_eq!(write_as("boolean", "tinyint(1)", json!(1)), json!(true));
        assert_eq!(write_as("bool", "int", json!("0")), json!(false));
        assert_eq!(write_as("int", "text", json!(null)), json!(null));
    }

    fn people(n: usize) -> Diff {
        let mut d = diff(false);
        d.columns = vec![
            ("id".into(), "id".into()),
            ("nombre".into(), "nombre".into()),
            ("saldo".into(), "saldo".into()),
            ("alta".into(), "alta".into()),
        ];
        d.only_left = (0..n)
            .map(|i| {
                vec![
                    json!(i.to_string()),
                    if i == 7 {
                        json!(null)
                    } else {
                        json!(format!("Nombre {i}"))
                    },
                    json!(format!("{}.50", i % 10)),
                    json!(format!("2024-03-{:02} 10:00:00", i % 28 + 1)),
                ]
            })
            .collect();
        d.only_right = vec![];
        d.changed = vec![
            ChangedRow {
                left: vec![json!(1), json!("Ana"), json!(100), json!("a")],
                right: vec![json!(1), json!("Ana"), json!(250), json!("a")],
                diff: vec![2],
            },
            ChangedRow {
                left: vec![json!(2), json!("Beto"), json!(300), json!("a")],
                right: vec![json!(2), json!("Roberto"), json!(300), json!("a")],
                diff: vec![1],
            },
        ];
        d
    }

    fn ask(kind: RowKind) -> DataRowsArgs {
        DataRowsArgs {
            id: String::new(),
            kind,
            sort: vec![],
            filters: vec![],
            changed_in: vec![],
        }
    }

    fn where_(column: &str, op: FilterOp, value: serde_json::Value) -> ColumnFilter {
        ColumnFilter {
            column: column.into(),
            op,
            values: vec![value],
            sql: None,
        }
    }

    #[test]
    fn a_filter_covers_all_the_rows_not_only_those_shown() {
        let d = people(5000);
        // Past the 2,000 the first answer brought.
        let mut a = ask(RowKind::OnlyLeft);
        a.filters = vec![where_("id", FilterOp::Eq, json!(4321))];
        let r = rows_of(&d, &a);
        assert_eq!(r.total, 1);
        assert_eq!(r.indices, vec![4321]);
        assert_eq!(
            r.rows[0].index, 4321,
            "the place in the whole list, for Pick"
        );
        assert_eq!(r.rows[0].left[0], json!("4321"));
        // More pass than are shown: all counted, and all there for "all".
        let mut a = ask(RowKind::OnlyLeft);
        a.filters = vec![where_("saldo", FilterOp::Ge, json!(5))];
        let r = rows_of(&d, &a);
        assert_eq!(r.total, 2500);
        assert_eq!(r.indices.len(), 2500);
        assert_eq!(r.rows.len(), SHOWN.min(2500));
        // No filter: nothing to mark apart.
        assert!(rows_of(&d, &ask(RowKind::OnlyLeft)).indices.is_empty());
    }

    #[test]
    fn filters_compare_by_value() {
        let d = people(100);
        let count = |f: ColumnFilter| {
            let mut a = ask(RowKind::OnlyLeft);
            a.filters = vec![f];
            rows_of(&d, &a).total
        };
        // Numbers that came as text, by value and not as text: "9.50" < "10".
        assert_eq!(count(where_("saldo", FilterOp::Lt, json!(10))), 100);
        assert_eq!(count(where_("saldo", FilterOp::Eq, json!("3.5"))), 10);
        assert_eq!(count(where_("saldo", FilterOp::Ne, json!(3.5))), 90);
        // Text, without case; NULL matches neither way.
        assert_eq!(
            count(where_("nombre", FilterOp::Contains, json!("NOMBRE 9"))),
            11
        );
        assert_eq!(
            count(where_("nombre", FilterOp::NotContains, json!("nombre"))),
            0
        );
        assert_eq!(count(where_("nombre", FilterOp::IsNull, json!(null))), 1);
        assert_eq!(
            count(where_("nombre", FilterOp::StartsWith, json!("nombre 1"))),
            11
        );
        // A date against a whole day, and against a range.
        assert_eq!(count(where_("alta", FilterOp::Eq, json!("2024-03-05"))), 4);
        assert_eq!(count(where_("alta", FilterOp::Ge, json!("2024-03-27"))), 6);
        // A column that isn't compared filters nothing.
        assert_eq!(count(where_("otra", FilterOp::Eq, json!(1))), 100);
    }

    #[test]
    fn a_changed_row_passes_by_either_side_and_by_what_changed() {
        let d = people(0);
        let mut a = ask(RowKind::Changed);
        a.filters = vec![where_("saldo", FilterOp::Eq, json!(250))];
        assert_eq!(rows_of(&d, &a).rows[0].index, 0, "the right side's value");
        a.filters = vec![where_("nombre", FilterOp::Eq, json!("Roberto"))];
        assert_eq!(rows_of(&d, &a).rows[0].index, 1);
        // Only rows where this column differs.
        let mut a = ask(RowKind::Changed);
        a.changed_in = vec!["nombre".into()];
        let r = rows_of(&d, &a);
        assert_eq!((r.total, r.rows[0].index), (1, 1));
        assert_eq!(r.rows[0].right.as_ref().unwrap()[1], json!("Roberto"));
        assert_eq!(r.rows[0].diff, vec![1]);
        assert_eq!(r.indices, vec![1]);
    }

    #[test]
    fn sorting_covers_all_the_rows_by_value_with_null_last() {
        let d = people(5000);
        let by = |column: &str, descending: bool| {
            let mut a = ask(RowKind::OnlyLeft);
            a.sort = vec![SortKey {
                column: column.into(),
                descending,
            }];
            rows_of(&d, &a)
        };
        // By number, not as text ("4999" is past "999"), from all 5,000.
        assert_eq!(by("id", true).rows[0].left[0], json!("4999"));
        let up = by("id", false);
        assert_eq!(up.rows[10].left[0], json!("10"));
        assert_eq!(up.rows[10].index, 10);
        assert_eq!(up.total, 5000);
        assert_eq!(up.rows.len(), SHOWN);
        // NULL goes last whichever way.
        let d2 = people(10);
        let mut a = ask(RowKind::OnlyLeft);
        a.sort = vec![SortKey {
            column: "nombre".into(),
            descending: true,
        }];
        let r = rows_of(&d2, &a);
        assert_eq!(r.rows.last().unwrap().left[1], json!(null));
        assert_eq!(r.rows[0].left[1], json!("Nombre 9"));
        // A second key breaks the ties of the first, stable otherwise.
        let mut a = ask(RowKind::OnlyLeft);
        a.sort = vec![
            SortKey {
                column: "saldo".into(),
                descending: true,
            },
            SortKey {
                column: "id".into(),
                descending: true,
            },
        ];
        assert_eq!(rows_of(&d, &a).rows[0].left[0], json!("4999"));
    }

    #[test]
    fn a_partial_comparison_only_updates() {
        let d = diff(true);
        // Rows on both sides are confirmed: they can go either way.
        let c = Choices {
            changed: pick(Dir::Right),
            ..Default::default()
        };
        let (right, left) = plans(&d, &c).unwrap();
        assert_eq!(right.update.len(), 1);
        assert!(left.update.is_empty());
        // A row of one side, by "all" or one by one, in either direction.
        for c in [
            Choices {
                only_left: pick(Dir::Right),
                ..Default::default()
            },
            Choices {
                only_left: pick(Dir::Left),
                ..Default::default()
            },
            Choices {
                only_right: pick(Dir::Right),
                ..Default::default()
            },
            Choices {
                only_right: Pick {
                    all: Dir::None,
                    rows: vec![(0, Dir::Left)],
                },
                ..Default::default()
            },
        ] {
            let e = plans(&d, &c).err().expect("refused").to_string();
            assert!(e.contains("parcial"), "{e}");
        }
        // Past the end of the list nothing is chosen.
        let c = Choices {
            only_left: Pick {
                all: Dir::None,
                rows: vec![(5, Dir::Right)],
            },
            ..Default::default()
        };
        assert!(plans(&d, &c).is_ok());
    }

    #[test]
    fn a_full_comparison_still_inserts_and_deletes() {
        let d = diff(false);
        let c = Choices {
            changed: pick(Dir::Right),
            only_left: pick(Dir::Right),
            only_right: pick(Dir::Right),
        };
        let (right, left) = plans(&d, &c).unwrap();
        assert_eq!(right.insert.len(), 1);
        assert_eq!(right.delete.len(), 1);
        assert_eq!(right.update.len(), 1);
        assert!(left.insert.is_empty() && left.delete.is_empty() && left.update.is_empty());
    }

    #[test]
    fn row_choices_override_the_default() {
        let p = Pick {
            all: Dir::Right,
            rows: vec![(1, Dir::None), (2, Dir::Left), (9, Dir::Left)],
        };
        assert_eq!(
            p.resolve(4),
            vec![Dir::Right, Dir::None, Dir::Left, Dir::Right]
        );
        let none: Pick = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(none.resolve(2), vec![Dir::None, Dir::None]);
        let p: Pick =
            serde_json::from_value(serde_json::json!({ "all": "left", "rows": [[0, "none"]] }))
                .unwrap();
        assert_eq!(p.resolve(2), vec![Dir::None, Dir::Left]);
    }
    use serde_json::json;

    #[test]
    fn compares_values_across_engines() {
        assert_eq!(canon(&json!(1)), canon(&json!("1.00")));
        assert_eq!(canon(&json!(2.5)), canon(&json!("2.50")));
        assert_ne!(canon(&json!(1)), canon(&json!(2)));
        assert_eq!(canon(&json!(true)), canon(&json!(1)));
        assert_eq!(
            canon(&json!("2026-09-28T10:00:00.000Z")),
            canon(&json!("2026-09-28 10:00:00"))
        );
        assert_ne!(canon(&json!(null)), canon(&json!("")));
        assert_ne!(
            canon(&json!("007")),
            canon(&json!("7")),
            "a code with leading zeros is text"
        );
        // A fraction followed by an offset: only its digits lose their
        // trailing zeros, and UTC is UTC however it's written.
        assert_eq!(
            canon(&json!("2026-09-28 10:00:00.1230000+02:00")),
            canon(&json!("2026-09-28T10:00:00.123+02:00"))
        );
        assert_eq!(
            canon(&json!("2026-09-28 10:00:00.0000000+00:00")),
            canon(&json!("2026-09-28T10:00:00Z"))
        );
        assert_ne!(
            canon(&json!("2026-09-28 10:00:00+02:00")),
            canon(&json!("2026-09-28 10:00:00+03:00"))
        );
        assert_ne!(
            canon(&json!("2026-09-28 10:00:00.5+02:00")),
            canon(&json!("2026-09-28 10:00:00+02:00"))
        );
    }
}

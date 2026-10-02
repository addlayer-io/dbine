//! Data compare (docs/comparacion-de-datos.md): the rows of two tables —
//! same or different connections, even different engines — matched by key:
//! rows only on one side, only on the other, and rows whose values differ.
//! The sync script is written by the target's driver (`insert_script`,
//! `update_script`, `delete_script`), so it's in the target's language; the
//! UI shows it and runs it only when the user says so.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::{ObjectRef, QueryOutcome, RowChange};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tauri::State;

/// Rows read from each side at most (a bigger table is compared in part,
/// and the result says so).
const DEFAULT_LIMIT: u64 = 200_000;
/// Rows of each kind sent to the UI (the counts and the script use them all).
const SHOWN: usize = 2_000;

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
    /// Columns to compare; empty: every column both sides have.
    #[serde(default)]
    pub columns: Vec<String>,
    pub limit: Option<u64>,
}

#[derive(Serialize, Clone)]
pub struct ChangedRow {
    pub left: Vec<Value>,
    pub right: Vec<Value>,
    /// Indexes (in `columns`) of the values that differ.
    pub diff: Vec<usize>,
}

#[derive(Serialize, Clone, Default)]
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
    /// Columns only one side has (not compared).
    pub only_left_columns: Vec<String>,
    pub only_right_columns: Vec<String>,
    pub counts: Counts,
    pub only_left: Vec<Vec<Value>>,
    pub only_right: Vec<Vec<Value>>,
    pub changed: Vec<ChangedRow>,
    /// A side had more rows than the limit: the comparison is partial.
    pub truncated_left: bool,
    pub truncated_right: bool,
    /// Rows whose key was repeated on a side (compared once).
    pub duplicate_keys: u64,
}

/// The whole difference, kept for the script.
struct Diff {
    left: Side,
    right: Side,
    key: Vec<String>,
    /// (left name, right name) of each compared column.
    columns: Vec<(String, String)>,
    only_left: Vec<Vec<Value>>,
    only_right: Vec<Vec<Value>>,
    changed: Vec<ChangedRow>,
}

fn diffs() -> &'static Mutex<HashMap<String, Diff>> {
    static D: OnceLock<Mutex<HashMap<String, Diff>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

/// Read a side: its columns and its rows (up to `limit`, on its own
/// read-only session).
async fn read(state: &AppState, side: &Side, limit: u64) -> CommandResult<(Vec<dbine_driver::ColumnInfo>, Vec<String>, Vec<Vec<Value>>, bool)> {
    let key = format!("dcmp:{}", uuid::Uuid::new_v4());
    let entry = state.dedicated_session(&key, &side.connection_id, &side.database, true).await?;
    let result = async {
        let mut s = entry.session.lock().await;
        let cols = s.columns(&side.object).await?;
        let sql = s.browse_query(&side.object, u32::try_from(limit + 1).unwrap_or(u32::MAX));
        let mut out = QueryOutcome::default();
        s.execute(&sql, (limit + 1) as usize, &mut out).await?;
        if let Some(e) = out.error {
            return Err(dbine_driver::Error::Query(e));
        }
        let r = out.results.into_iter().find(|r| !r.columns.is_empty()).unwrap_or_default();
        let names: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
        let truncated = r.rows.len() as u64 > limit || r.truncated;
        let mut rows = r.rows;
        rows.truncate(limit as usize);
        Ok((cols, names, rows, truncated))
    }
    .await;
    state.sessions.remove(&key);
    Ok(result?)
}

/// A value as compared: numbers by value (`1` = `1.0` = `"1.00"`), dates
/// with `T` or a space alike, the rest as text.
fn canon(v: &Value) -> String {
    match v {
        Value::Null => "\u{0}null".into(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Number(n) => n.as_f64().map(num).unwrap_or_else(|| n.to_string()),
        Value::String(s) => {
            let t = s.trim();
            if let Ok(f) = t.parse::<f64>() {
                // A code with leading zeros ("007") is text, not a number.
                let leading_zero = t.len() > 1 && t.starts_with('0') && !t.starts_with("0.");
                if !t.is_empty() && !leading_zero && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')) {
                    return num(f);
                }
            }
            // 2026-09-28T10:00:00.000Z ≈ 2026-09-28 10:00:00
            if t.len() >= 19 && t.as_bytes()[4] == b'-' && t.as_bytes()[7] == b'-' && matches!(t.as_bytes()[10], b'T' | b' ') {
                let mut d = t.replacen('T', " ", 1);
                if d.ends_with('Z') {
                    d.pop();
                }
                if let Some(dot) = d.find('.') {
                    let frac = d[dot + 1..].trim_end_matches('0');
                    d = if frac.is_empty() { d[..dot].to_string() } else { format!("{}.{frac}", &d[..dot]) };
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

#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare(state: State<'_, AppState>, args: DataCompareArgs) -> CommandResult<DataCompareResult> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 5_000_000);
    let (l, r) = tokio::try_join!(read(&state, &args.left, limit), read(&state, &args.right, limit))?;
    let (lcols, lnames, lrows, ltrunc) = l;
    let (_rcols, rnames, rrows, rtrunc) = r;

    // Columns both have (by name, ignoring case), in the left's order.
    let find = |names: &[String], n: &str| names.iter().position(|x| x.eq_ignore_ascii_case(n));
    let key: Vec<String> = if args.key.is_empty() { lcols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect() } else { args.key.clone() };
    if key.is_empty() {
        return Err(CommandError::BadRequest("la tabla no tiene clave primaria: elegí las columnas que identifican cada fila".into()));
    }
    let wanted: Vec<&String> = if args.columns.is_empty() { lnames.iter().collect() } else { args.columns.iter().collect() };
    let mut columns: Vec<(String, usize, String, usize)> = Vec::new();
    for k in &key {
        let (Some(li), Some(ri)) = (find(&lnames, k), find(&rnames, k)) else {
            return Err(CommandError::BadRequest(format!("la columna clave «{k}» no está en las dos tablas")));
        };
        columns.push((lnames[li].clone(), li, rnames[ri].clone(), ri));
    }
    for n in wanted {
        if key.iter().any(|k| k.eq_ignore_ascii_case(n)) {
            continue;
        }
        if let (Some(li), Some(ri)) = (find(&lnames, n), find(&rnames, n)) {
            columns.push((lnames[li].clone(), li, rnames[ri].clone(), ri));
        }
    }
    let only_left_columns = lnames.iter().filter(|n| find(&rnames, n).is_none()).cloned().collect();
    let only_right_columns = rnames.iter().filter(|n| find(&lnames, n).is_none()).cloned().collect();
    let nk = key.len();

    let project = |row: &[Value], left: bool| -> Vec<Value> { columns.iter().map(|c| row.get(if left { c.1 } else { c.3 }).cloned().unwrap_or(Value::Null)).collect() };
    let key_of = |p: &[Value]| -> String { p[..nk].iter().map(canon).collect::<Vec<_>>().join("\u{1}") };

    let mut duplicate_keys = 0;
    let mut right: HashMap<String, Vec<Value>> = HashMap::with_capacity(rrows.len());
    for row in &rrows {
        let p = project(row, false);
        if right.insert(key_of(&p), p).is_some() {
            duplicate_keys += 1;
        }
    }
    let mut counts = Counts { left: lrows.len() as u64, right: rrows.len() as u64, ..Default::default() };
    let (mut only_left, mut changed) = (Vec::new(), Vec::new());
    let mut seen = std::collections::HashSet::new();
    for row in &lrows {
        let p = project(row, true);
        let k = key_of(&p);
        if !seen.insert(k.clone()) {
            duplicate_keys += 1;
            continue;
        }
        match right.remove(&k) {
            None => only_left.push(p),
            Some(rp) => {
                let diff: Vec<usize> = (nk..p.len()).filter(|&i| canon(&p[i]) != canon(&rp[i])).collect();
                if diff.is_empty() {
                    counts.same += 1;
                } else {
                    changed.push(ChangedRow { left: p, right: rp, diff });
                }
            }
        }
    }
    let only_right: Vec<Vec<Value>> = right.into_values().collect();
    counts.only_left = only_left.len() as u64;
    counts.only_right = only_right.len() as u64;
    counts.changed = changed.len() as u64;

    let id = uuid::Uuid::new_v4().to_string();
    let result = DataCompareResult {
        id: id.clone(),
        key: columns[..nk].iter().map(|c| c.0.clone()).collect(),
        columns: columns.iter().map(|c| c.0.clone()).collect(),
        only_left_columns,
        only_right_columns,
        counts,
        only_left: only_left.iter().take(SHOWN).cloned().collect(),
        only_right: only_right.iter().take(SHOWN).cloned().collect(),
        changed: changed.iter().take(SHOWN).cloned().collect(),
        truncated_left: ltrunc,
        truncated_right: rtrunc,
        duplicate_keys,
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
            columns: columns.into_iter().map(|c| (c.0, c.2)).collect(),
            only_left,
            only_right,
            changed,
        },
    );
    Ok(result)
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

/// The scripts that apply the chosen rows, one per side that changes, each
/// in that side's language. Nothing runs here.
#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_script(state: State<'_, AppState>, args: DataScriptArgs) -> CommandResult<Vec<DataScript>> {
    let all = diffs().lock().unwrap();
    let d = all.get(&args.id).ok_or_else(|| CommandError::NotFound("la comparación ya no está: volvé a comparar".into()))?;
    let c = &args.choices;
    let (mut right, mut left) = (Plan::default(), Plan::default());
    for (row, dir) in d.changed.iter().zip(c.changed.resolve(d.changed.len())) {
        match dir {
            Dir::Right => right.update.push((&row.left, &row.right, &row.diff)),
            Dir::Left => left.update.push((&row.right, &row.left, &row.diff)),
            Dir::None => {}
        }
    }
    for (row, dir) in d.only_left.iter().zip(c.only_left.resolve(d.only_left.len())) {
        match dir {
            Dir::Right => right.insert.push(row.clone()),
            Dir::Left => left.delete.push(row),
            Dir::None => {}
        }
    }
    for (row, dir) in d.only_right.iter().zip(c.only_right.resolve(d.only_right.len())) {
        match dir {
            Dir::Left => left.insert.push(row.clone()),
            Dir::Right => right.delete.push(row),
            Dir::None => {}
        }
    }
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
fn side_script(state: &AppState, d: &Diff, to_right: bool, plan: Plan<'_>) -> CommandResult<DataScript> {
    let target = if to_right { &d.right } else { &d.left };
    let cfg = state.store.get_connection(&target.connection_id)?.ok_or_else(|| CommandError::NotFound("la conexión destino ya no existe".into()))?.config;
    let driver = dbine_drivers::find(&cfg.driver).ok_or_else(|| CommandError::BadRequest(format!("no hay driver '{}'", cfg.driver)))?;
    // The target's column names.
    let names: Vec<String> = d.columns.iter().map(|(l, r)| if to_right { r.clone() } else { l.clone() }).collect();
    let nk = d.key.len();
    let key_of = |r: &Vec<Value>| -> Vec<(String, Value)> { names[..nk].iter().cloned().zip(r[..nk].iter().cloned()).collect() };
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
                set: diff.iter().map(|&i| (names[i].clone(), src[i].clone())).collect(),
                row: names.iter().cloned().zip(dst.iter().cloned()).collect(),
            })
            .collect();
        parts.push(driver.update_script(&target.object, &changes)?);
    }
    if !plan.insert.is_empty() {
        parts.push(driver.insert_script(&target.object, &names, &plan.insert)?);
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
fn assemble(driver: &dyn dbine_driver::Driver, parts: Vec<String>) -> (String, u64) {
    let sep = driver.script_separator();
    let script = parts.into_iter().filter(|p| !p.trim().is_empty()).collect::<Vec<_>>().join(&format!("\n{sep}\n"));
    let statements = driver.split_script(&script).len() as u64;
    (script, statements)
}

#[cfg(test)]
mod tests {
    use super::{assemble, canon, Dir, Pick};
    use dbine_driver::{ObjectRef, RowChange};

    /// The statement count a sync run reports against: joining the parts
    /// neither merges nor adds statements, on every SQL engine.
    #[test]
    fn statement_count_is_the_sum_of_the_parts() {
        let target = ObjectRef { kind: "table".into(), schema: Some("s".into()), name: "t".into() };
        let cols = vec!["id".to_string(), "name".to_string()];
        let rows: Vec<Vec<serde_json::Value>> = (1..=3).map(|i| vec![json!(i), json!(format!("n{i}"))]).collect();
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
            let parts: Vec<String> = [d.delete_script(&target, &keys), d.update_script(&target, &changes), d.insert_script(&target, &cols, &rows)]
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

    #[test]
    fn row_choices_override_the_default() {
        let p = Pick { all: Dir::Right, rows: vec![(1, Dir::None), (2, Dir::Left), (9, Dir::Left)] };
        assert_eq!(p.resolve(4), vec![Dir::Right, Dir::None, Dir::Left, Dir::Right]);
        let none: Pick = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(none.resolve(2), vec![Dir::None, Dir::None]);
        let p: Pick = serde_json::from_value(serde_json::json!({ "all": "left", "rows": [[0, "none"]] })).unwrap();
        assert_eq!(p.resolve(2), vec![Dir::None, Dir::Left]);
    }
    use serde_json::json;

    #[test]
    fn compares_values_across_engines() {
        assert_eq!(canon(&json!(1)), canon(&json!("1.00")));
        assert_eq!(canon(&json!(2.5)), canon(&json!("2.50")));
        assert_ne!(canon(&json!(1)), canon(&json!(2)));
        assert_eq!(canon(&json!(true)), canon(&json!(1)));
        assert_eq!(canon(&json!("2026-09-28T10:00:00.000Z")), canon(&json!("2026-09-28 10:00:00")));
        assert_ne!(canon(&json!(null)), canon(&json!("")));
        assert_ne!(canon(&json!("007")), canon(&json!("7")), "a code with leading zeros is text");
    }
}

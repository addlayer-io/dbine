//! What the drivers return: explorer objects and query results. Serialized
//! as-is to the UI (snake_case fields, mirrored by hand in
//! `web/src/api/types.ts`).

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// An object in the explorer. `kind` is one of the driver's
/// [`crate::ObjectKindInfo::id`]s (see [`crate::kinds`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbObject {
    pub kind: String,
    /// `None` on engines without schemas.
    pub schema: Option<String>,
    pub name: String,
    /// The owner of a dependent object (a trigger's table…).
    #[serde(default)]
    pub parent: Option<String>,
}

/// Which object a call is about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRef {
    pub kind: String,
    pub schema: Option<String>,
    pub name: String,
}

impl ObjectRef {
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref().filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub auto_increment: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultColumn {
    pub name: String,
    /// Engine type name when the driver knows it ("" otherwise).
    pub type_name: String,
}

/// One statement's outcome: a result set (`columns` not empty) and/or a
/// count of affected rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatementResult {
    pub columns: Vec<ResultColumn>,
    /// Cells as JSON: null, bool, number (only when it fits a JS number
    /// exactly), or string (decimals, dates, binary as 0x…).
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Rows the statement returned, including those dropped past the limit.
    pub total_rows: u64,
    /// More rows than the limit came back; only the first ones are in `rows`.
    pub truncated: bool,
    pub rows_affected: Option<u64>,
}

/// Everything a script produced. A failing statement stops the script:
/// the results before it are kept and `error` says what failed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryOutcome {
    pub results: Vec<StatementResult>,
    /// Server messages (PRINT, notices, warnings…).
    pub messages: Vec<String>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    /// Execution plans, when the run asked for them ([`crate::Session::explain`]).
    #[serde(default)]
    pub plans: Vec<Plan>,
    /// Where rows go instead of `rows` (exports): set by the app, never by
    /// drivers. Drivers don't notice: `begin_result` / `push_row` route
    /// through it, so every engine streams to a file without buffering.
    #[serde(skip)]
    pub sink: Option<RowSinkRef>,
    /// First error the sink returned; rows after it are dropped.
    #[serde(skip)]
    pub sink_error: Option<String>,
    /// Result sets before this outcome's first one (see [`Self::fork`]).
    #[serde(skip)]
    pub sink_base: usize,
}

/// Receives the rows of a run as they arrive (see [`QueryOutcome::sink`]).
pub trait RowSink: Send {
    /// Result set `index` (0-based, in this run) starts.
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()>;
    fn row(&mut self, index: usize, row: &[serde_json::Value]) -> std::io::Result<()>;
}

/// A shared [`RowSink`] (the outcome is `Clone`/`Debug`; sinks aren't).
#[derive(Clone)]
pub struct RowSinkRef(pub Arc<Mutex<dyn RowSink>>);

impl std::fmt::Debug for RowSinkRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RowSink")
    }
}

/// One statement's execution plan as a tree of operators, the way SSMS
/// draws it: the root is the statement's last operator (the one returning
/// rows), children feed it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    /// The statement the plan is for.
    pub statement: String,
    pub root: PlanNode,
    /// Actual figures (the statement ran) or estimates only.
    pub actual: bool,
    /// The engine's own plan, as it came: "showplan_xml", "json", "text"…
    pub raw_format: String,
    pub raw: String,
}

/// An operator. Every figure is optional: engines report different ones.
/// Costs are in the engine's own units; the UI turns `self_cost` into a
/// share of the root's `total_cost`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanNode {
    /// Operator ("Clustered Index Seek", "Hash Join", "Seq Scan"…).
    pub op: String,
    /// Refinement of the operator (logical op, join type, access type…).
    #[serde(default)]
    pub detail: String,
    /// Table / index it reads, if any.
    #[serde(default)]
    pub object: Option<String>,
    /// Cost of this operator and everything below it.
    #[serde(default)]
    pub total_cost: Option<f64>,
    /// Cost of this operator alone; when `None` the UI derives it from
    /// `total_cost` minus the children's.
    #[serde(default)]
    pub self_cost: Option<f64>,
    /// Estimated rows **per execution** (as engines report them).
    #[serde(default)]
    pub est_rows: Option<f64>,
    /// Actual rows **in total**, over all executions.
    #[serde(default)]
    pub actual_rows: Option<f64>,
    /// How many times it ran (loops / executions).
    #[serde(default)]
    pub executions: Option<f64>,
    #[serde(default)]
    pub actual_ms: Option<f64>,
    /// Things worth a warning icon (missing index, spill, implicit
    /// conversion, no join predicate…).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Everything else, for the properties panel.
    #[serde(default)]
    pub props: Vec<(String, String)>,
    #[serde(default)]
    pub children: Vec<PlanNode>,
}

impl QueryOutcome {
    /// An empty outcome that continues this one: drivers that fill a local
    /// outcome (on a blocking thread, per statement…) must start it with
    /// `fork` and hand it back with [`Self::merge`], so rows still reach the
    /// sink with the right result-set numbers.
    pub fn fork(&self) -> QueryOutcome {
        QueryOutcome { sink: self.sink.clone(), sink_base: self.sink_base + self.results.len(), ..Default::default() }
    }

    /// Take back what a [`Self::fork`] collected.
    pub fn merge(&mut self, child: QueryOutcome) {
        self.results.extend(child.results);
        self.messages.extend(child.messages);
        self.plans.extend(child.plans);
        if self.sink_error.is_none() {
            self.sink_error = child.sink_error;
        }
    }

    /// Start a new result set with these columns.
    pub fn begin_result(&mut self, columns: Vec<ResultColumn>) {
        if let Some(sink) = &self.sink {
            let index = self.sink_base + self.results.len();
            if self.sink_error.is_none() {
                if let Err(e) = sink.0.lock().expect("sink").begin(index, &columns) {
                    self.sink_error = Some(e.to_string());
                }
            }
        }
        self.results.push(StatementResult { columns, ..Default::default() });
    }

    /// Add a row to the current result set, keeping at most `max_rows`.
    pub fn push_row(&mut self, row: Vec<serde_json::Value>, max_rows: usize) {
        if self.results.is_empty() {
            self.begin_result(Vec::new());
        }
        let index = self.sink_base + self.results.len() - 1;
        let r = self.results.last_mut().expect("a result set");
        r.total_rows += 1;
        if let Some(sink) = &self.sink {
            if self.sink_error.is_none() {
                if let Err(e) = sink.0.lock().expect("sink").row(index, &row) {
                    self.sink_error = Some(e.to_string());
                }
            }
            return;
        }
        if r.rows.len() < max_rows {
            r.rows.push(row);
        } else {
            r.truncated = true;
        }
    }

    /// Record a statement that returned no rows.
    pub fn push_affected(&mut self, n: u64) {
        self.results.push(StatementResult { rows_affected: Some(n), ..Default::default() });
    }
}

/// Integers past ±2^53 go as strings so JS doesn't round them.
pub fn json_i64(v: i64) -> serde_json::Value {
    const MAX_SAFE: i64 = (1 << 53) - 1;
    if (-MAX_SAFE..=MAX_SAFE).contains(&v) {
        v.into()
    } else {
        v.to_string().into()
    }
}

pub fn json_u64(v: u64) -> serde_json::Value {
    match i64::try_from(v) {
        Ok(i) => json_i64(i),
        Err(_) => v.to_string().into(),
    }
}

pub fn json_f64(v: f64) -> serde_json::Value {
    serde_json::Number::from_f64(v).map_or_else(|| v.to_string().into(), serde_json::Value::Number)
}

/// Binary shown as `0x…` hex, cut at 1 KiB.
pub fn json_bytes(b: &[u8]) -> serde_json::Value {
    const MAX: usize = 1024;
    let mut s = String::with_capacity(2 + b.len().min(MAX) * 2);
    s.push_str("0x");
    for byte in b.iter().take(MAX) {
        s.push_str(&format!("{byte:02X}"));
    }
    if b.len() > MAX {
        s.push('…');
    }
    s.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_past_the_limit_are_counted_not_kept() {
        let mut o = QueryOutcome::default();
        o.begin_result(vec![]);
        for i in 0..5 {
            o.push_row(vec![i.into()], 3);
        }
        assert_eq!(o.results[0].rows.len(), 3);
        assert_eq!(o.results[0].total_rows, 5);
        assert!(o.results[0].truncated);
    }

    #[test]
    fn a_sink_takes_the_rows_instead_of_memory() {
        struct Count(Vec<(usize, usize)>);
        impl RowSink for Count {
            fn begin(&mut self, index: usize, cols: &[ResultColumn]) -> std::io::Result<()> {
                self.0.push((index, cols.len()));
                Ok(())
            }
            fn row(&mut self, index: usize, _: &[serde_json::Value]) -> std::io::Result<()> {
                self.0.push((index, 99));
                Ok(())
            }
        }
        let sink = Arc::new(Mutex::new(Count(Vec::new())));
        let mut o = QueryOutcome { sink: Some(RowSinkRef(sink.clone())), ..Default::default() };
        o.begin_result(vec![ResultColumn { name: "a".into(), type_name: String::new() }]);
        o.push_row(vec![1.into()], 1);
        o.push_row(vec![2.into()], 1);
        assert!(o.results[0].rows.is_empty(), "nothing buffered");
        assert_eq!(o.results[0].total_rows, 2);
        assert!(!o.results[0].truncated);
        assert_eq!(sink.lock().unwrap().0, vec![(0, 1), (0, 99), (0, 99)]);

        // A fork (a driver's local outcome) numbers its result sets after
        // the parent's, and merging brings them back.
        let mut child = o.fork();
        child.begin_result(vec![]);
        child.push_row(vec![3.into()], 1);
        o.merge(child);
        assert_eq!(o.results.len(), 2);
        assert_eq!(sink.lock().unwrap().0[3..], [(1, 0), (1, 99)]);
    }

    #[test]
    fn big_integers_become_strings() {
        assert_eq!(json_i64(42), serde_json::json!(42));
        assert_eq!(json_i64(i64::MAX), serde_json::json!("9223372036854775807"));
        assert_eq!(json_u64(u64::MAX), serde_json::json!("18446744073709551615"));
    }

    #[test]
    fn binary_is_hex() {
        assert_eq!(json_bytes(&[0xde, 0xad]), serde_json::json!("0xDEAD"));
    }
}

/// One row changed in the results grid: which row (`key`: its primary key
/// values, or every original value when there's no key) and the new values
/// (`set`). [`crate::Driver::update_script`] turns a list of them into code.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RowChange {
    pub key: Vec<(String, serde_json::Value)>,
    pub set: Vec<(String, serde_json::Value)>,
    /// The whole row as it was (every column of the result), for engines
    /// that rewrite a row whole (ksqlDB tables, TDengine…): the new row is
    /// this with `set` applied. May be empty.
    #[serde(default)]
    pub row: Vec<(String, serde_json::Value)>,
}

impl RowChange {
    /// The row after the change: `row` with `set` applied (columns only in
    /// `set` go at the end). Empty when `row` is.
    pub fn new_row(&self) -> Vec<(String, serde_json::Value)> {
        if self.row.is_empty() {
            return Vec::new();
        }
        let mut out = self.row.clone();
        for (k, v) in &self.set {
            match out.iter_mut().find(|(c, _)| c == k) {
                Some(slot) => slot.1 = v.clone(),
                None => out.push((k.clone(), v.clone())),
            }
        }
        out
    }
}

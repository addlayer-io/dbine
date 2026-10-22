//! Charts over a query's whole result, not just the rows the grid or the
//! notebook cell fetched. Two ways:
//!
//! - **Server**: SQL engines wrap the query in a `GROUP BY`
//!   ([`crate::Driver::aggregate_query`], built by [`grouped_sql`]) and only
//!   the groups come back.
//! - **Client**: everything else (other languages, dialects whose queries
//!   aren't plain SQL over rows, histograms and box plots) runs the query
//!   again and streams its rows through an [`Aggregator`] (a
//!   [`crate::RowSink`]), up to a row cap.
//!
//! Either way the answer is a [`ChartAggregate`]. Category labels follow
//! the results grid's chart: a cell's text as JavaScript's `String(v)`
//! gives it, and `None` for NULL (the UI shows its own "(null)" label).

use crate::model::ResultColumn;
use crate::read_only::first_write_in;
use crate::sql::{leading_keyword, quote_ident, Quote, ScriptDialect, ScriptStatement};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Most groups (rows of the grouped result) a chart asks for.
pub const MAX_GROUPS: u32 = 10_000;
/// Most measures in one chart.
pub const MAX_MEASURES: usize = 16;
/// Most histogram bins.
pub const MAX_BINS: u32 = 200;
/// Distinct groups the client path keeps in memory; past it, new groups
/// are dropped and the answer says `groups_truncated`.
pub const GROUPS_IN_MEMORY: usize = 200_000;

/// The aggregate of a measure, as the chart's "Agregación" names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFn {
    fn sql(self) -> &'static str {
        match self {
            AggFn::Count => "COUNT",
            AggFn::Sum => "SUM",
            AggFn::Avg => "AVG",
            AggFn::Min => "MIN",
            AggFn::Max => "MAX",
        }
    }
}

/// One value per group: `func` over `column`; `column: None` is
/// `COUNT(*)` (only with [`AggFn::Count`]). `COUNT(column)` counts its
/// non-null cells, as SQL does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measure {
    #[serde(default)]
    pub column: Option<String>,
    pub func: AggFn,
}

/// A grouped chart: one category per value of `x` (and one series per
/// value of `split`, when given), each measure aggregated per group.
/// Without `x`, the measures over the whole result: one category (`None`)
/// and no split (a KPI). Columns are named as the query's result names
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateSpec {
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub split: Option<String>,
    pub measures: Vec<Measure>,
    /// Most groups (distinct `x`, or `x` × `split` pairs) in the answer;
    /// past it the answer says `groups_truncated`. Clamped to
    /// 1..=[`MAX_GROUPS`].
    #[serde(default = "default_max_groups")]
    pub max_groups: u32,
}

fn default_max_groups() -> u32 {
    1_000
}

fn default_bins() -> u32 {
    20
}

fn default_boxes() -> u32 {
    50
}

/// What a chart asks for over the whole result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChartKind {
    /// Bars, lines, areas, pie: groups and measures (server where it can).
    Group(AggregateSpec),
    /// Counts of a numeric column in `bins` equal-width bins between its
    /// minimum and maximum. Always computed in DBine.
    Histogram {
        column: String,
        #[serde(default = "default_bins")]
        bins: u32,
    },
    /// Min, quartiles and max of a numeric column, per value of `by` (one
    /// box without it). Exact, always computed in DBine.
    Box {
        column: String,
        #[serde(default)]
        by: Option<String>,
        #[serde(default = "default_boxes")]
        max_groups: u32,
    },
}

/// Where the aggregation ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateMethod {
    /// The engine grouped the rows (`GROUP BY` around the query).
    #[default]
    Server,
    /// DBine ran the query again and aggregated the rows it read.
    Client,
}

/// One series of a grouped chart: measure `measure` (index in
/// [`AggregateSpec::measures`]) for the split value `split` (`None`
/// without a split, or for the NULL split value), one value per category.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartSeries {
    pub measure: usize,
    #[serde(default)]
    pub split: Option<String>,
    /// `None`: no value in that group (SUM/AVG/MIN/MAX over no numbers).
    pub values: Vec<Option<f64>>,
}

/// A histogram: `counts[i]` values fall in `[edges[i], edges[i + 1])`, the
/// last bin closed. `nulls` and `non_numeric` cells are left out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Histogram {
    pub edges: Vec<f64>,
    pub counts: Vec<u64>,
    pub nulls: u64,
    pub non_numeric: u64,
}

/// Outlier values a box carries at most (the most extreme ones);
/// [`BoxStats::outlier_count`] counts them all.
pub const MAX_BOX_OUTLIERS: usize = 100;

/// One box of a box plot (quartiles by linear interpolation, as NumPy's
/// default and R's type 7; whiskers as Tukey's, like the chart over the
/// loaded rows).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxStats {
    /// The `by` value (`None`: NULL, or no `by`).
    #[serde(default)]
    pub category: Option<String>,
    pub count: u64,
    pub min: f64,
    pub q1: f64,
    pub median: f64,
    pub q3: f64,
    pub max: f64,
    /// Tukey's whiskers: the lowest and highest values within 1.5 IQR of
    /// the box.
    pub whisker_low: f64,
    pub whisker_high: f64,
    /// Values past the whiskers.
    pub outlier_count: u64,
    /// The most extreme of them, at most [`MAX_BOX_OUTLIERS`], ascending.
    #[serde(default)]
    pub outliers: Vec<f64>,
}

/// A chart's data over the whole result of its query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChartAggregate {
    /// Grouped charts: the `x` values, in order (`None`: NULL).
    pub categories: Vec<Option<String>>,
    pub series: Vec<ChartSeries>,
    #[serde(default)]
    pub boxes: Option<Vec<BoxStats>>,
    #[serde(default)]
    pub histogram: Option<Histogram>,
    pub method: AggregateMethod,
    /// Rows the aggregation covered, when known.
    #[serde(default)]
    pub rows_scanned: Option<u64>,
    /// The client path stopped at its row cap: the chart covers only the
    /// first rows.
    #[serde(default)]
    pub rows_capped: bool,
    /// More groups than asked for: the last ones were left out.
    #[serde(default)]
    pub groups_truncated: bool,
}

// -- cells ------------------------------------------------------------------------

/// A cell as the chart labels it: JavaScript's `String(v)` for scalars,
/// `None` for NULL.
pub fn label(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (_, Some(u), _) => u.to_string(),
            (_, _, Some(f)) => js_number(f),
            _ => n.to_string(),
        }),
        other => Some(other.to_string()),
    }
}

fn js_number(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        format!("{f}")
    }
}

/// A cell as a number, as the chart reads it: numbers, booleans (1 / 0)
/// and strings that are a plain decimal number (`-12.5`, `3e4`).
pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) if numeric_text(s.trim()) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `^-?\d+(\.\d+)?(e[+-]?\d+)?$`, case-insensitive.
fn numeric_text(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    if !digits(&mut i) {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == b.len()
}

/// How labels are ordered on the client: numbers by value, then text,
/// NULL last.
fn order_labels(a: &Option<String>, b: &Option<String>) -> Ordering {
    fn key(l: &Option<String>) -> Option<(Option<f64>, &str)> {
        l.as_deref().map(|s| {
            (
                numeric_text(s.trim())
                    .then(|| s.trim().parse::<f64>().ok())
                    .flatten(),
                s,
            )
        })
    }
    match (key(a), key(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some((Some(x), _)), Some((Some(y), _))) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Some((Some(_), _)), Some((None, _))) => Ordering::Less,
        (Some((None, _)), Some((Some(_), _))) => Ordering::Greater,
        (Some((None, x)), Some((None, y))) => x.cmp(y),
    }
}

// -- validation ---------------------------------------------------------------------

/// The spec with its limits applied, or why it can't be charted.
pub fn validate(spec: &AggregateSpec) -> Result<AggregateSpec> {
    if spec.x.as_deref().is_some_and(|x| x.trim().is_empty()) {
        return Err(Error::Query("falta la columna del eje X".into()));
    }
    if spec.measures.is_empty() || spec.measures.len() > MAX_MEASURES {
        return Err(Error::Query(format!(
            "el gráfico necesita entre 1 y {MAX_MEASURES} medidas"
        )));
    }
    for m in &spec.measures {
        match (&m.column, m.func) {
            (None, AggFn::Count) => {}
            (None, _) => return Err(Error::Query("solo «Cantidad» puede ir sin columna".into())),
            (Some(c), _) if c.trim().is_empty() => {
                return Err(Error::Query("falta la columna de una medida".into()))
            }
            _ => {}
        }
    }
    let mut s = spec.clone();
    s.max_groups = s.max_groups.clamp(1, MAX_GROUPS);
    s.split = s.split.filter(|c| !c.trim().is_empty());
    if s.x.is_none() && s.split.is_some() {
        return Err(Error::Query(
            "un gráfico sin eje X no se separa en series".into(),
        ));
    }
    Ok(s)
}

// -- server: GROUP BY around the query ------------------------------------------------

/// How a dialect limits the grouped result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLimit {
    /// `… LIMIT n`
    Limit,
    /// `SELECT TOP n …`
    Top,
    /// `… FETCH FIRST n ROWS ONLY`
    FetchFirst,
    /// `SELECT FIRST n …` (Informix, Firebird)
    First,
}

/// What the grouped statement needs to know about the dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlShape {
    pub quote: Quote,
    pub limit: RowLimit,
    /// `FROM (…) AS q`; Oracle takes no `AS` before a table alias.
    pub alias_as: bool,
    /// `AVG` of an integer column is an integer (SQL Server, Sybase, DB2,
    /// Firebird): the column goes through `* 1.0E0` first.
    pub float_avg: bool,
    /// A derived table may have an `ORDER BY` (T-SQL refuses it without
    /// `TOP` / `OFFSET`: it is dropped there, it doesn't change the groups).
    pub derived_order_by: bool,
    /// A derived table may start with `WITH` (T-SQL can't nest a CTE).
    pub derived_with: bool,
}

impl SqlShape {
    /// The usual shape: `"x"`, `LIMIT n`, `AS q`.
    pub const fn ansi() -> Self {
        Self {
            quote: Quote::Double,
            limit: RowLimit::Limit,
            alias_as: true,
            float_avg: false,
            derived_order_by: true,
            derived_with: true,
        }
    }

    /// The shape of a [`crate::DriverInfo::dialect`]; `None` for dialects
    /// whose queries aren't plain SQL over rows (Cosmos DB's `c`, N1QL,
    /// PartiQL, ksqlDB push queries, InfluxQL, IoTDB paths) or whose SQL
    /// subqueries differ (OrientDB, TDengine): they aggregate in DBine.
    pub fn for_dialect(dialect: &str) -> Option<Self> {
        let base = Self::ansi();
        Some(match dialect {
            "cosmos" | "n1ql" | "partiql" | "ksql" | "influxql" | "iotdb" | "orientdb"
            | "tdengine" => return None,
            "mssql" | "sybase" => Self {
                quote: Quote::Bracket,
                limit: RowLimit::Top,
                float_avg: true,
                derived_order_by: false,
                derived_with: false,
                ..base
            },
            "access" => Self {
                quote: Quote::Bracket,
                limit: RowLimit::Top,
                derived_order_by: false,
                derived_with: false,
                ..base
            },
            "teradata" => Self {
                limit: RowLimit::Top,
                derived_order_by: false,
                derived_with: false,
                ..base
            },
            "oracle" => Self {
                limit: RowLimit::FetchFirst,
                alias_as: false,
                ..base
            },
            "db2" => Self {
                limit: RowLimit::FetchFirst,
                float_avg: true,
                ..base
            },
            "informix" => Self {
                limit: RowLimit::First,
                ..base
            },
            "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks"
            | "spanner" | "drill" => Self {
                quote: Quote::Backtick,
                ..base
            },
            _ => base,
        })
    }
}

/// The one statement of `statements` (the query split as its driver
/// does) if it is a read that starts with `SELECT` (or `WITH`, where
/// `with` allows it): what goes inside the grouped statement.
pub fn single_read(
    statements: &[ScriptStatement],
    dialect: &ScriptDialect,
    with: bool,
) -> Result<String> {
    let [one] = statements else {
        return Err(Error::Unsupported(
            "el gráfico agrupa en el servidor una sola consulta".into(),
        ));
    };
    if one.error.is_some() || one.repeat > 1 {
        return Err(Error::Unsupported(
            "el gráfico agrupa en el servidor una sola consulta".into(),
        ));
    }
    let lead = leading_keyword(&one.text, dialect);
    let ok = match lead.as_deref() {
        Some("select") => true,
        Some("with") => with,
        _ => false,
    };
    if !ok || first_write_in(&one.text, dialect).is_some() {
        return Err(Error::Unsupported(
            "solo un SELECT se agrupa en el servidor".into(),
        ));
    }
    Ok(one.text.trim().trim_end_matches(';').trim_end().to_string())
}

/// Whether a T-SQL `SELECT` limits its rows itself (`SELECT [ALL |
/// DISTINCT] TOP …`): its `ORDER BY` is then valid in a derived table.
fn has_top(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    let mut words = lower
        .split(|c: char| c.is_whitespace() || c == '(')
        .filter(|w| !w.is_empty());
    if words.next() != Some("select") {
        return false;
    }
    match words.next() {
        Some("all" | "distinct") => words.next() == Some("top"),
        w => w == Some("top"),
    }
}

/// `query` (one `SELECT` / `WITH`, see [`single_read`]) wrapped in a
/// `GROUP BY` of `spec`: columns `dbine_g0` (x), `dbine_g1` (split),
/// `dbine_n` (rows of the group), then `dbine_m0`… (the measures), ordered
/// by the groups and limited to `max_groups + 1` rows (one more tells the
/// result was cut). Without `x`: `dbine_n` and the measures over the whole
/// result, one row, no `GROUP BY`.
pub fn grouped_sql(shape: &SqlShape, query: &str, spec: &AggregateSpec) -> Result<String> {
    let spec = validate(spec)?;
    let q = |c: &str| quote_ident(shape.quote, c);
    let lead = query
        .trim_start()
        .get(..4)
        .map(|s| s.eq_ignore_ascii_case("with"))
        .unwrap_or(false);
    if lead && !shape.derived_with {
        return Err(Error::Unsupported(
            "este motor no agrupa en el servidor una consulta con WITH".into(),
        ));
    }
    let mut inner = query.trim().to_string();
    if !shape.derived_order_by && !has_top(&inner) {
        let c = crate::sort::clauses(&inner, false);
        if let (Some(at), None) = (c.order_by, c.tail) {
            inner = inner[..at].trim_end().to_string();
        }
    }
    let mut cols = Vec::new();
    let mut keys = Vec::new();
    if let Some(x) = &spec.x {
        cols.push(format!("{} AS {}", q(x), q("dbine_g0")));
        keys.push(q(x));
    }
    if let Some(s) = &spec.split {
        cols.push(format!("{} AS {}", q(s), q("dbine_g1")));
        keys.push(q(s));
    }
    cols.push(format!("COUNT(*) AS {}", q("dbine_n")));
    for (i, m) in spec.measures.iter().enumerate() {
        let e = match (&m.column, m.func) {
            (None, _) => "COUNT(*)".to_string(),
            (Some(c), AggFn::Avg) if shape.float_avg => format!("AVG({} * 1.0E0)", q(c)),
            (Some(c), f) => format!("{}({})", f.sql(), q(c)),
        };
        cols.push(format!("{e} AS {}", q(&format!("dbine_m{i}"))));
    }
    let alias = if shape.alias_as {
        "AS dbine_q"
    } else {
        "dbine_q"
    };
    if keys.is_empty() {
        return Ok(format!(
            "SELECT {}\nFROM (\n{inner}\n) {alias}",
            cols.join(", ")
        ));
    }
    let n = u64::from(spec.max_groups) + 1;
    let head = match shape.limit {
        RowLimit::Top => format!("SELECT TOP {n} "),
        RowLimit::First => format!("SELECT FIRST {n} "),
        _ => "SELECT ".to_string(),
    };
    let tail = match shape.limit {
        RowLimit::Limit => format!("\nLIMIT {n}"),
        RowLimit::FetchFirst => format!("\nFETCH FIRST {n} ROWS ONLY"),
        _ => String::new(),
    };
    let keys = keys.join(", ");
    Ok(format!(
        "{head}{}\nFROM (\n{inner}\n) {alias}\nGROUP BY {keys}\nORDER BY {keys}{tail}",
        cols.join(", ")
    ))
}

// -- shaping groups into categories and series --------------------------------------------

/// One group: its `x` and `split` labels, its rows, its measures.
#[derive(Debug, Clone, PartialEq)]
struct GroupRow {
    x: Option<String>,
    split: Option<String>,
    n: u64,
    values: Vec<Option<f64>>,
}

/// Rows (sorted by x, then split) into the answer, keeping `max_groups`
/// of them. With a split, the last category is left out whole when the
/// cut fell inside it (its series would be partial).
fn shape(
    mut rows: Vec<GroupRow>,
    spec: &AggregateSpec,
    method: AggregateMethod,
    mut truncated: bool,
) -> ChartAggregate {
    let max = spec.max_groups as usize;
    if rows.len() > max {
        truncated = true;
        let next = rows[max].x.clone();
        rows.truncate(max);
        if spec.split.is_some() && rows.first().is_some_and(|r| r.x != next) {
            while rows.last().is_some_and(|r| r.x == next) {
                rows.pop();
            }
        }
    }
    let mut categories: Vec<Option<String>> = Vec::new();
    for r in &rows {
        if categories.last() != Some(&r.x) || categories.is_empty() {
            categories.push(r.x.clone());
        }
    }
    let at: HashMap<&Option<String>, usize> =
        categories.iter().enumerate().map(|(i, c)| (c, i)).collect();
    let mut series = Vec::new();
    if spec.split.is_some() {
        let mut splits: Vec<Option<String>> = Vec::new();
        for r in &rows {
            if !splits.contains(&r.split) {
                splits.push(r.split.clone());
            }
        }
        splits.sort_by(order_labels);
        for s in &splits {
            for m in 0..spec.measures.len() {
                let mut values = vec![None; categories.len()];
                for r in rows.iter().filter(|r| &r.split == s) {
                    values[at[&r.x]] = r.values[m];
                }
                series.push(ChartSeries {
                    measure: m,
                    split: s.clone(),
                    values,
                });
            }
        }
    } else {
        for m in 0..spec.measures.len() {
            let mut values = vec![None; categories.len()];
            for r in &rows {
                values[at[&r.x]] = r.values[m];
            }
            series.push(ChartSeries {
                measure: m,
                split: None,
                values,
            });
        }
    }
    let rows_scanned = (!truncated).then(|| rows.iter().map(|r| r.n).sum());
    ChartAggregate {
        categories,
        series,
        method,
        rows_scanned,
        groups_truncated: truncated,
        ..Default::default()
    }
}

/// The answer from the grouped statement's rows ([`grouped_sql`]'s
/// columns, by position).
pub fn from_grouped(spec: &AggregateSpec, rows: &[Vec<Value>]) -> Result<ChartAggregate> {
    let spec = validate(spec)?;
    let at_split = usize::from(spec.x.is_some());
    let base = at_split + usize::from(spec.split.is_some());
    let width = base + 1 + spec.measures.len();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if r.len() < width {
            return Err(Error::Query(
                "el resultado agrupado no tiene las columnas esperadas".into(),
            ));
        }
        out.push(GroupRow {
            x: spec.x.as_ref().and_then(|_| label(&r[0])),
            split: spec.split.as_ref().and_then(|_| label(&r[at_split])),
            n: num(&r[base]).map_or(0, |n| n.max(0.0) as u64),
            values: (0..spec.measures.len())
                .map(|i| num(&r[base + 1 + i]))
                .collect(),
        });
    }
    if spec.x.is_none() {
        out.truncate(1);
        if out.is_empty() {
            out.push(empty_total(&spec));
        }
    }
    Ok(shape(out, &spec, AggregateMethod::Server, false))
}

/// The total of no rows, as SQL gives it: counts 0, the rest none.
fn empty_total(spec: &AggregateSpec) -> GroupRow {
    GroupRow {
        x: None,
        split: None,
        n: 0,
        values: spec
            .measures
            .iter()
            .map(|m| Acc::default().value(m.func))
            .collect(),
    }
}

// -- client: aggregate the rows as they stream in -------------------------------------------

#[derive(Debug, Clone, Default)]
struct Acc {
    count: u64,
    sum: f64,
    nums: u64,
    min: f64,
    max: f64,
}

impl Acc {
    fn add(&mut self, v: &Value) {
        if v.is_null() {
            return;
        }
        self.count += 1;
        if let Some(n) = num(v).filter(|n| n.is_finite()) {
            if self.nums == 0 {
                self.min = n;
                self.max = n;
            } else {
                self.min = self.min.min(n);
                self.max = self.max.max(n);
            }
            self.nums += 1;
            self.sum += n;
        }
    }

    fn value(&self, f: AggFn) -> Option<f64> {
        let some = self.nums > 0;
        match f {
            AggFn::Count => Some(self.count as f64),
            AggFn::Sum => some.then_some(self.sum),
            AggFn::Avg => some.then(|| self.sum / self.nums as f64),
            AggFn::Min => some.then_some(self.min),
            AggFn::Max => some.then_some(self.max),
        }
    }
}

/// Column indices of the result set being aggregated.
#[derive(Debug, Clone)]
enum Cols {
    Group {
        x: Option<usize>,
        split: Option<usize>,
        measures: Vec<Option<usize>>,
    },
    Histogram {
        column: usize,
    },
    Box {
        column: usize,
        by: Option<usize>,
    },
}

type GroupKey = (Option<String>, Option<String>);

/// The client path: a [`crate::RowSink`] that aggregates the first result
/// set that has the chart's columns, row by row, keeping at most `cap`
/// rows. `on_cap` runs once when a row past the cap arrives (the caller
/// stops the query there).
pub struct Aggregator {
    kind: ChartKind,
    cap: u64,
    on_cap: Option<Box<dyn FnMut() + Send>>,
    target: Option<(usize, Cols)>,
    rows: u64,
    capped: bool,
    missing: Option<String>,
    groups: HashMap<GroupKey, usize>,
    group_rows: Vec<(GroupKey, u64, Vec<Acc>)>,
    groups_dropped: bool,
    values: Vec<f64>,
    nulls: u64,
    non_numeric: u64,
    boxes: HashMap<Option<String>, Vec<f64>>,
}

impl Aggregator {
    pub fn new(kind: ChartKind, cap: u64) -> Result<Self> {
        let kind = match kind {
            ChartKind::Group(spec) => ChartKind::Group(validate(&spec)?),
            ChartKind::Histogram { column, bins } => {
                if column.trim().is_empty() {
                    return Err(Error::Query("falta la columna del histograma".into()));
                }
                ChartKind::Histogram {
                    column,
                    bins: bins.clamp(1, MAX_BINS),
                }
            }
            ChartKind::Box {
                column,
                by,
                max_groups,
            } => {
                if column.trim().is_empty() {
                    return Err(Error::Query("falta la columna del diagrama de caja".into()));
                }
                ChartKind::Box {
                    column,
                    by: by.filter(|b| !b.trim().is_empty()),
                    max_groups: max_groups.clamp(1, MAX_GROUPS),
                }
            }
        };
        Ok(Self {
            kind,
            cap: cap.max(1),
            on_cap: None,
            target: None,
            rows: 0,
            capped: false,
            missing: None,
            groups: HashMap::new(),
            group_rows: Vec::new(),
            groups_dropped: false,
            values: Vec::new(),
            nulls: 0,
            non_numeric: 0,
            boxes: HashMap::new(),
        })
    }

    /// Called once, when the first row past the cap arrives.
    pub fn on_cap(mut self, f: impl FnMut() + Send + 'static) -> Self {
        self.on_cap = Some(Box::new(f));
        self
    }

    /// Whether the cap was hit.
    pub fn capped(&self) -> bool {
        self.capped
    }

    fn resolve(&self, columns: &[ResultColumn]) -> std::result::Result<Cols, String> {
        let find = |name: &str| {
            columns
                .iter()
                .position(|c| c.name == name)
                .ok_or_else(|| name.to_string())
        };
        Ok(match &self.kind {
            ChartKind::Group(spec) => Cols::Group {
                x: spec.x.as_deref().map(find).transpose()?,
                split: spec.split.as_deref().map(find).transpose()?,
                measures: spec
                    .measures
                    .iter()
                    .map(|m| m.column.as_deref().map(find).transpose())
                    .collect::<std::result::Result<_, _>>()?,
            },
            ChartKind::Histogram { column, .. } => Cols::Histogram {
                column: find(column)?,
            },
            ChartKind::Box { column, by, .. } => Cols::Box {
                column: find(column)?,
                by: by.as_deref().map(find).transpose()?,
            },
        })
    }

    fn add(&mut self, cols: &Cols, row: &[Value]) {
        let cell = |i: usize| row.get(i).unwrap_or(&Value::Null);
        match cols {
            Cols::Group { x, split, measures } => {
                let key = (
                    x.and_then(|x| label(cell(x))),
                    split.and_then(|s| label(cell(s))),
                );
                let i = match self.groups.get(&key) {
                    Some(i) => *i,
                    None if self.group_rows.len() >= GROUPS_IN_MEMORY => {
                        self.groups_dropped = true;
                        return;
                    }
                    None => {
                        self.group_rows.push((
                            key.clone(),
                            0,
                            vec![Acc::default(); measures.len()],
                        ));
                        self.groups.insert(key, self.group_rows.len() - 1);
                        self.group_rows.len() - 1
                    }
                };
                let g = &mut self.group_rows[i];
                g.1 += 1;
                for (acc, col) in g.2.iter_mut().zip(measures) {
                    match col {
                        // COUNT(*): every row.
                        None => acc.add(&Value::Bool(true)),
                        Some(c) => acc.add(cell(*c)),
                    }
                }
            }
            Cols::Histogram { column } => match cell(*column) {
                Value::Null => self.nulls += 1,
                v => match num(v).filter(|n| n.is_finite()) {
                    Some(n) => self.values.push(n),
                    None => self.non_numeric += 1,
                },
            },
            Cols::Box { column, by } => {
                if let Some(n) = num(cell(*column)).filter(|n| n.is_finite()) {
                    let key = by.and_then(|b| label(cell(b)));
                    if !self.boxes.contains_key(&key) && self.boxes.len() >= GROUPS_IN_MEMORY {
                        self.groups_dropped = true;
                        return;
                    }
                    self.boxes.entry(key).or_default().push(n);
                }
            }
        }
    }

    /// The answer, once the query ended (or was stopped at the cap).
    pub fn finish(&mut self) -> Result<ChartAggregate> {
        if self.target.is_none() {
            let col = self.missing.take();
            return Err(Error::Query(match col {
                Some(c) => format!("la consulta no devolvió la columna «{c}»"),
                None => "la consulta no devolvió filas para el gráfico".into(),
            }));
        }
        let mut out = match &self.kind {
            ChartKind::Group(spec) => {
                let mut rows: Vec<GroupRow> = std::mem::take(&mut self.group_rows)
                    .into_iter()
                    .map(|((x, split), n, accs)| GroupRow {
                        x,
                        split,
                        n,
                        values: accs
                            .iter()
                            .zip(&spec.measures)
                            .map(|(a, m)| a.value(m.func))
                            .collect(),
                    })
                    .collect();
                rows.sort_by(|a, b| {
                    order_labels(&a.x, &b.x).then_with(|| order_labels(&a.split, &b.split))
                });
                if spec.x.is_none() && rows.is_empty() {
                    rows.push(empty_total(spec));
                }
                shape(rows, spec, AggregateMethod::Client, self.groups_dropped)
            }
            ChartKind::Histogram { bins, .. } => ChartAggregate {
                histogram: Some(histogram(&self.values, *bins, self.nulls, self.non_numeric)),
                ..Default::default()
            },
            ChartKind::Box { max_groups, .. } => {
                let mut keys: Vec<Option<String>> = self.boxes.keys().cloned().collect();
                keys.sort_by(order_labels);
                let truncated = self.groups_dropped || keys.len() > *max_groups as usize;
                keys.truncate(*max_groups as usize);
                let boxes = keys
                    .into_iter()
                    .filter_map(|k| {
                        let mut v = self.boxes.remove(&k)?;
                        box_stats(k, &mut v)
                    })
                    .collect();
                ChartAggregate {
                    boxes: Some(boxes),
                    groups_truncated: truncated,
                    ..Default::default()
                }
            }
        };
        out.method = AggregateMethod::Client;
        out.rows_scanned = Some(self.rows);
        out.rows_capped = self.capped;
        Ok(out)
    }
}

impl crate::model::RowSink for Aggregator {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        if self.target.is_none() && !columns.is_empty() {
            match self.resolve(columns) {
                Ok(cols) => self.target = Some((index, cols)),
                Err(missing) => {
                    self.missing.get_or_insert(missing);
                }
            }
        }
        Ok(())
    }

    fn row(&mut self, index: usize, row: &[Value]) -> std::io::Result<()> {
        if self.capped || self.target.as_ref().is_none_or(|(at, _)| *at != index) {
            return Ok(());
        }
        if self.rows >= self.cap {
            self.capped = true;
            if let Some(f) = self.on_cap.as_mut() {
                f();
            }
            return Ok(());
        }
        self.rows += 1;
        let target = self.target.take();
        if let Some((_, cols)) = &target {
            self.add(cols, row);
        }
        self.target = target;
        Ok(())
    }
}

/// Equal-width bins between the minimum and the maximum.
pub fn histogram(values: &[f64], bins: u32, nulls: u64, non_numeric: u64) -> Histogram {
    let mut h = Histogram {
        nulls,
        non_numeric,
        ..Default::default()
    };
    let Some((lo, hi)) = values.iter().fold(None, |acc: Option<(f64, f64)>, &v| {
        Some(acc.map_or((v, v), |(a, b)| (a.min(v), b.max(v))))
    }) else {
        return h;
    };
    let bins = if lo == hi {
        1
    } else {
        bins.clamp(1, MAX_BINS) as usize
    };
    let width = (hi - lo) / bins as f64;
    h.edges = (0..=bins)
        .map(|i| if i == bins { hi } else { lo + width * i as f64 })
        .collect();
    h.counts = vec![0; bins];
    for &v in values {
        let i = if width > 0.0 {
            (((v - lo) / width) as usize).min(bins - 1)
        } else {
            0
        };
        h.counts[i] += 1;
    }
    h
}

/// Linear-interpolation quantile of sorted values.
fn quantile(sorted: &[f64], p: f64) -> f64 {
    let h = (sorted.len() - 1) as f64 * p;
    let lo = h.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let (a, b, f) = (sorted[lo], sorted[hi], h - lo as f64);
    if f == 0.0 {
        return a;
    }
    // `b - a` overflows to infinity when the values are near ±f64::MAX;
    // the weighted form can't overflow between two finite values.
    let d = b - a;
    if d.is_finite() {
        a + f * d
    } else {
        a * (1.0 - f) + b * f
    }
}

fn box_stats(category: Option<String>, values: &mut [f64]) -> Option<BoxStats> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let (q1, q3) = (quantile(values, 0.25), quantile(values, 0.75));
    let iqr = q3 - q1;
    let (lo_fence, hi_fence) = (q1 - 1.5 * iqr, q3 + 1.5 * iqr);
    // Sorted: the outliers are a prefix and a suffix; the whiskers the
    // values next to them (the box's quartiles are always within).
    let (mut lo, mut hi) = (0, values.len());
    // Non-finite fences (infinite values, or an IQR that overflows) mark
    // no outliers: comparing against NaN would empty the box.
    if lo_fence.is_finite() && hi_fence.is_finite() {
        lo = values.partition_point(|v| *v < lo_fence);
        hi = values.partition_point(|v| *v <= hi_fence);
        if hi <= lo {
            (lo, hi) = (0, values.len());
        }
    }
    let (low, high) = (&values[..lo], &values[hi..]);
    // The most extreme ones when there are too many: from both ends, the
    // one further from its quartile first.
    let (mut i, mut j) = (0, high.len());
    while i + (high.len() - j) < MAX_BOX_OUTLIERS && (i < low.len() || j > 0) {
        let take_low = j == 0 || (i < low.len() && q1 - low[i] >= high[j - 1] - q3);
        if take_low {
            i += 1;
        } else {
            j -= 1;
        }
    }
    let mut outliers = low[..i].to_vec();
    outliers.extend_from_slice(&high[j..]);
    Some(BoxStats {
        category,
        count: values.len() as u64,
        min: values[0],
        q1,
        median: quantile(values, 0.5),
        q3,
        max: values[values.len() - 1],
        whisker_low: values[lo],
        whisker_high: values[hi - 1],
        outlier_count: (low.len() + high.len()) as u64,
        outliers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RowSink;
    use serde_json::json;

    fn m(column: Option<&str>, func: AggFn) -> Measure {
        Measure {
            column: column.map(Into::into),
            func,
        }
    }

    fn spec(
        x: &str,
        split: Option<&str>,
        measures: Vec<Measure>,
        max_groups: u32,
    ) -> AggregateSpec {
        AggregateSpec {
            x: Some(x.into()),
            split: split.map(Into::into),
            measures,
            max_groups,
        }
    }

    fn cols(names: &[&str]) -> Vec<ResultColumn> {
        names
            .iter()
            .map(|n| ResultColumn {
                name: n.to_string(),
                type_name: String::new(),
            })
            .collect()
    }

    #[test]
    fn labels_and_numbers_read_as_the_chart_does() {
        assert_eq!(label(&json!(null)), None);
        assert_eq!(label(&json!(3)), Some("3".into()));
        assert_eq!(label(&json!(3.0)), Some("3".into()));
        assert_eq!(label(&json!(2.5)), Some("2.5".into()));
        assert_eq!(label(&json!(true)), Some("true".into()));
        assert_eq!(label(&json!("a")), Some("a".into()));
        assert_eq!(num(&json!(" -12.5 ")), Some(-12.5));
        assert_eq!(num(&json!("3e4")), Some(30000.0));
        assert_eq!(num(&json!("1.")), None);
        assert_eq!(num(&json!("2024-01-01")), None);
        assert_eq!(num(&json!(false)), Some(0.0));
        assert_eq!(num(&json!(null)), None);
    }

    #[test]
    fn the_spec_is_checked() {
        assert!(validate(&spec("", None, vec![m(None, AggFn::Count)], 10)).is_err());
        assert!(validate(&spec("x", None, vec![], 10)).is_err());
        assert!(validate(&spec("x", None, vec![m(None, AggFn::Sum)], 10)).is_err());
        assert_eq!(
            validate(&spec("x", Some(""), vec![m(None, AggFn::Count)], 0))
                .unwrap()
                .max_groups,
            1
        );
        assert_eq!(
            validate(&spec("x", None, vec![m(None, AggFn::Count)], 1 << 30))
                .unwrap()
                .max_groups,
            MAX_GROUPS
        );
        // The UI's JSON (snake_case, like the other contract types).
        let s: ChartKind = serde_json::from_value(json!({
            "kind": "group", "x": "region", "split": null,
            "measures": [{ "column": null, "func": "count" }, { "column": "total", "func": "avg" }], "max_groups": 50
        }))
        .unwrap();
        assert_eq!(
            s,
            ChartKind::Group(spec(
                "region",
                None,
                vec![m(None, AggFn::Count), m(Some("total"), AggFn::Avg)],
                50
            ))
        );
        let h: ChartKind =
            serde_json::from_value(json!({ "kind": "histogram", "column": "v" })).unwrap();
        assert_eq!(
            h,
            ChartKind::Histogram {
                column: "v".into(),
                bins: 20
            }
        );
        let b: ChartKind =
            serde_json::from_value(json!({ "kind": "box", "column": "v", "by": "g" })).unwrap();
        assert_eq!(
            b,
            ChartKind::Box {
                column: "v".into(),
                by: Some("g".into()),
                max_groups: 50
            }
        );
    }

    fn stmt(text: &str) -> Vec<ScriptStatement> {
        crate::sql::split_script(text, &ScriptDialect::generic())
    }

    #[test]
    fn only_one_select_goes_to_the_server() {
        let d = ScriptDialect::generic();
        assert_eq!(
            single_read(&stmt("SELECT * FROM t;"), &d, true).unwrap(),
            "SELECT * FROM t"
        );
        assert_eq!(
            single_read(
                &stmt("-- c\nWITH a AS (SELECT 1 x) SELECT * FROM a"),
                &d,
                true
            )
            .unwrap(),
            "WITH a AS (SELECT 1 x) SELECT * FROM a"
        );
        assert!(single_read(&stmt("WITH a AS (SELECT 1 x) SELECT * FROM a"), &d, false).is_err());
        assert!(single_read(&stmt("SELECT 1; SELECT 2"), &d, true).is_err());
        assert!(single_read(&stmt("SHOW TABLES"), &d, true).is_err());
        assert!(single_read(
            &stmt("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d"),
            &d,
            true
        )
        .is_err());
        assert!(single_read(&stmt("SELECT * INTO t2 FROM t"), &d, true).is_err());
        assert!(single_read(&stmt("UPDATE t SET a = 1"), &d, true).is_err());
    }

    #[test]
    fn grouped_sql_per_quote_and_limit_family() {
        let s = spec(
            "region",
            None,
            vec![m(None, AggFn::Count), m(Some("total"), AggFn::Sum)],
            100,
        );
        // postgres, sqlite, duckdb, trino, snowflake… : "x", LIMIT, AS.
        assert_eq!(
            grouped_sql(&SqlShape::for_dialect("postgres").unwrap(), "SELECT * FROM t", &s).unwrap(),
            "SELECT \"region\" AS \"dbine_g0\", COUNT(*) AS \"dbine_n\", COUNT(*) AS \"dbine_m0\", SUM(\"total\") AS \"dbine_m1\"\n\
             FROM (\nSELECT * FROM t\n) AS dbine_q\nGROUP BY \"region\"\nORDER BY \"region\"\nLIMIT 101"
        );
        // mysql, clickhouse, bigquery, spark…: backticks.
        let s2 = spec("a`b", Some("c"), vec![m(Some("v"), AggFn::Max)], 10);
        assert_eq!(
            grouped_sql(&SqlShape::for_dialect("mysql").unwrap(), "SELECT * FROM t", &s2).unwrap(),
            "SELECT `a``b` AS `dbine_g0`, `c` AS `dbine_g1`, COUNT(*) AS `dbine_n`, MAX(`v`) AS `dbine_m0`\n\
             FROM (\nSELECT * FROM t\n) AS dbine_q\nGROUP BY `a``b`, `c`\nORDER BY `a``b`, `c`\nLIMIT 11"
        );
        // sqlserver: TOP, brackets, AVG in float, the ORDER BY dropped.
        let s3 = spec("r", None, vec![m(Some("v"), AggFn::Avg)], 5);
        let ms = SqlShape::for_dialect("mssql").unwrap();
        assert_eq!(
            grouped_sql(&ms, "SELECT r, v FROM t ORDER BY v DESC", &s3).unwrap(),
            "SELECT TOP 6 [r] AS [dbine_g0], COUNT(*) AS [dbine_n], AVG([v] * 1.0E0) AS [dbine_m0]\n\
             FROM (\nSELECT r, v FROM t\n) AS dbine_q\nGROUP BY [r]\nORDER BY [r]"
        );
        // …kept with TOP or OFFSET, which make it valid there.
        assert!(
            grouped_sql(&ms, "SELECT TOP 10 r, v FROM t ORDER BY v DESC", &s3)
                .unwrap()
                .contains("ORDER BY v DESC\n)")
        );
        assert!(
            grouped_sql(&ms, "SELECT r, v FROM t ORDER BY v OFFSET 5 ROWS", &s3)
                .unwrap()
                .contains("OFFSET 5 ROWS\n)")
        );
        assert!(matches!(
            grouped_sql(&ms, "WITH a AS (SELECT 1 r, 2 v) SELECT * FROM a", &s3),
            Err(Error::Unsupported(_))
        ));
        // oracle: FETCH FIRST, no AS before the alias.
        assert_eq!(
            grouped_sql(&SqlShape::for_dialect("oracle").unwrap(), "SELECT * FROM emp ORDER BY sal", &s3).unwrap(),
            "SELECT \"r\" AS \"dbine_g0\", COUNT(*) AS \"dbine_n\", AVG(\"v\") AS \"dbine_m0\"\n\
             FROM (\nSELECT * FROM emp ORDER BY sal\n) dbine_q\nGROUP BY \"r\"\nORDER BY \"r\"\nFETCH FIRST 6 ROWS ONLY"
        );
        // db2: FETCH FIRST and AVG in float; informix / firebird: FIRST n.
        let db2 = grouped_sql(
            &SqlShape::for_dialect("db2").unwrap(),
            "SELECT * FROM t",
            &s3,
        )
        .unwrap();
        assert!(
            db2.contains("AVG(\"v\" * 1.0E0)")
                && db2.ends_with("FETCH FIRST 6 ROWS ONLY")
                && db2.contains(") AS dbine_q")
        );
        assert!(grouped_sql(
            &SqlShape::for_dialect("informix").unwrap(),
            "SELECT * FROM t",
            &s3
        )
        .unwrap()
        .starts_with("SELECT FIRST 6 \"r\""));
        // A trailing line comment can't swallow the closing parenthesis.
        assert!(
            grouped_sql(&SqlShape::ansi(), "SELECT * FROM t -- all", &s3)
                .unwrap()
                .contains("-- all\n) AS dbine_q")
        );
        // Dialects that aggregate in DBine.
        for d in [
            "cosmos", "n1ql", "partiql", "ksql", "influxql", "iotdb", "orientdb", "tdengine",
        ] {
            assert!(SqlShape::for_dialect(d).is_none(), "{d}");
        }
    }

    #[test]
    fn grouped_rows_become_categories_and_series() {
        let s = spec(
            "r",
            None,
            vec![m(None, AggFn::Count), m(Some("v"), AggFn::Sum)],
            10,
        );
        let rows = vec![
            vec![json!("a"), json!(2), json!(2), json!("10.5")],
            vec![json!(null), json!(1), json!(1), json!(null)],
        ];
        let out = from_grouped(&s, &rows).unwrap();
        assert_eq!(out.categories, vec![Some("a".into()), None]);
        assert_eq!(out.series[0].values, vec![Some(2.0), Some(1.0)]);
        assert_eq!(out.series[1].values, vec![Some(10.5), None]);
        assert_eq!(out.method, AggregateMethod::Server);
        assert_eq!(out.rows_scanned, Some(3));
        assert!(!out.groups_truncated);
        // Split: one series per split value; a category cut in half is dropped.
        let s = spec("r", Some("k"), vec![m(None, AggFn::Count)], 3);
        let row = |r: &str, k: &str, n: i64| vec![json!(r), json!(k), json!(n), json!(n)];
        let rows = vec![
            row("a", "x", 1),
            row("a", "y", 2),
            row("b", "x", 3),
            row("b", "y", 4),
        ];
        let out = from_grouped(&s, &rows).unwrap();
        assert!(out.groups_truncated);
        assert_eq!(out.rows_scanned, None);
        assert_eq!(out.categories, vec![Some("a".into())]);
        assert_eq!(out.series.len(), 2);
        assert_eq!(
            (out.series[1].split.as_deref(), &out.series[1].values),
            (Some("y"), &vec![Some(2.0)])
        );
    }

    fn feed(kind: ChartKind, cap: u64, columns: &[&str], rows: &[Vec<Value>]) -> ChartAggregate {
        let mut a = Aggregator::new(kind, cap).unwrap();
        a.begin(0, &[]).unwrap();
        a.begin(1, &cols(columns)).unwrap();
        for r in rows {
            a.row(1, r).unwrap();
        }
        a.finish().unwrap()
    }

    #[test]
    fn the_client_groups_like_sql() {
        let s = spec(
            "r",
            None,
            vec![
                m(None, AggFn::Count),
                m(Some("v"), AggFn::Count),
                m(Some("v"), AggFn::Avg),
                m(Some("v"), AggFn::Min),
                m(Some("v"), AggFn::Max),
            ],
            10,
        );
        let rows = vec![
            vec![json!("b"), json!(1)],
            vec![json!("a"), json!("4")],
            vec![json!("b"), json!(null)],
            vec![json!(10), json!(2)],
            vec![json!(9), json!("x")],
            vec![json!(null), json!(5)],
            vec![json!("b"), json!(3)],
        ];
        let out = feed(ChartKind::Group(s), 1_000, &["r", "v"], &rows);
        // Numbers by value, then text, NULL last.
        assert_eq!(
            out.categories,
            vec![
                Some("9".into()),
                Some("10".into()),
                Some("a".into()),
                Some("b".into()),
                None
            ]
        );
        assert_eq!(
            out.series[0].values,
            vec![Some(1.0), Some(1.0), Some(1.0), Some(3.0), Some(1.0)]
        );
        // COUNT(v): non-null cells, numeric or not.
        assert_eq!(
            out.series[1].values,
            vec![Some(1.0), Some(1.0), Some(1.0), Some(2.0), Some(1.0)]
        );
        assert_eq!(
            out.series[2].values,
            vec![None, Some(2.0), Some(4.0), Some(2.0), Some(5.0)]
        );
        assert_eq!(out.series[3].values[3], Some(1.0));
        assert_eq!(out.series[4].values[3], Some(3.0));
        assert_eq!(
            (out.method, out.rows_scanned, out.rows_capped),
            (AggregateMethod::Client, Some(7), false)
        );
    }

    #[test]
    fn the_client_stops_at_the_cap() {
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let f = fired.clone();
        let s = spec("r", None, vec![m(None, AggFn::Count)], 10);
        let mut a = Aggregator::new(ChartKind::Group(s), 3)
            .unwrap()
            .on_cap(move || {
                f.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        a.begin(0, &cols(&["r"])).unwrap();
        for _ in 0..5 {
            a.row(0, &[json!("a")]).unwrap();
        }
        let out = a.finish().unwrap();
        assert!(out.rows_capped);
        assert_eq!(out.rows_scanned, Some(3));
        assert_eq!(out.series[0].values, vec![Some(3.0)]);
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_missing_column_is_an_error() {
        let mut a = Aggregator::new(
            ChartKind::Histogram {
                column: "v".into(),
                bins: 4,
            },
            10,
        )
        .unwrap();
        a.begin(0, &cols(&["w"])).unwrap();
        let e = a.finish().unwrap_err().to_string();
        assert!(e.contains("«v»"), "{e}");
    }

    #[test]
    fn histogram_bins() {
        let rows: Vec<Vec<Value>> = [
            json!(0),
            json!(1),
            json!(2.5),
            json!(10),
            json!(null),
            json!("x"),
            json!("7"),
        ]
        .into_iter()
        .map(|v| vec![v])
        .collect();
        let out = feed(
            ChartKind::Histogram {
                column: "v".into(),
                bins: 4,
            },
            100,
            &["v"],
            &rows,
        );
        let h = out.histogram.unwrap();
        assert_eq!(h.edges, vec![0.0, 2.5, 5.0, 7.5, 10.0]);
        assert_eq!(h.counts, vec![2, 1, 1, 1]);
        assert_eq!((h.nulls, h.non_numeric), (1, 1));
        // One distinct value: one bin.
        assert_eq!(
            histogram(&[3.0, 3.0], 10, 0, 0),
            Histogram {
                edges: vec![3.0, 3.0],
                counts: vec![2],
                nulls: 0,
                non_numeric: 0
            }
        );
        assert_eq!(histogram(&[], 10, 2, 0).counts, Vec::<u64>::new());
    }

    #[test]
    fn box_plot_quartiles() {
        let rows: Vec<Vec<Value>> = [(1, "a"), (2, "a"), (3, "a"), (4, "a"), (10, "b")]
            .into_iter()
            .map(|(v, g)| vec![json!(v), json!(g)])
            .collect();
        let out = feed(
            ChartKind::Box {
                column: "v".into(),
                by: Some("g".into()),
                max_groups: 1,
            },
            100,
            &["v", "g"],
            &rows,
        );
        let b = out.boxes.unwrap();
        assert!(out.groups_truncated);
        assert_eq!(b.len(), 1);
        assert_eq!(
            b[0],
            BoxStats {
                category: Some("a".into()),
                count: 4,
                min: 1.0,
                q1: 1.75,
                median: 2.5,
                q3: 3.25,
                max: 4.0,
                whisker_low: 1.0,
                whisker_high: 4.0,
                outlier_count: 0,
                outliers: vec![],
            }
        );
        // One box over everything: 10 is past the upper fence (4 + 1.5 × 2).
        let out = feed(
            ChartKind::Box {
                column: "v".into(),
                by: None,
                max_groups: 10,
            },
            100,
            &["v", "g"],
            &rows,
        );
        let b = &out.boxes.unwrap()[0];
        assert_eq!((b.median, b.min, b.max), (3.0, 1.0, 10.0));
        assert_eq!(
            (
                b.whisker_low,
                b.whisker_high,
                b.outlier_count,
                b.outliers.clone()
            ),
            (1.0, 4.0, 1, vec![10.0])
        );
    }

    #[test]
    fn box_whiskers_are_tukeys_and_outliers_are_capped() {
        // 0..1000 and outliers on both sides: 20 far below, 200 above.
        let mut v: Vec<f64> = (0..1000).map(f64::from).collect();
        v.extend((0..20).map(|i| -10_000.0 - f64::from(i)));
        v.extend((0..200).map(|i| 10_000.0 + f64::from(i)));
        let b = box_stats(None, &mut v).unwrap();
        let (q1, q3) = (b.q1, b.q3);
        let (lo, hi) = (q1 - 1.5 * (q3 - q1), q3 + 1.5 * (q3 - q1));
        let inside: Vec<f64> = v.iter().copied().filter(|x| *x >= lo && *x <= hi).collect();
        assert_eq!(
            b.whisker_low,
            inside.iter().copied().fold(f64::INFINITY, f64::min)
        );
        assert_eq!(
            b.whisker_high,
            inside.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        );
        assert_eq!(b.outlier_count as usize, v.len() - inside.len());
        assert_eq!(b.outliers.len(), MAX_BOX_OUTLIERS);
        // Ascending, and the most extreme of each side.
        assert!(b.outliers.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(b.outlier_count, 220);
        assert_eq!((b.whisker_low, b.whisker_high), (0.0, 999.0));
        // The lows are further from the box: all 20, then the top 80 highs.
        assert_eq!(b.outliers.first(), Some(&-10_019.0));
        assert_eq!(b.outliers[20], 10_120.0);
        assert_eq!(b.outliers.last(), Some(&10_199.0));
        // A constant column: no outliers, the whiskers at the value.
        let b = box_stats(None, &mut [5.0, 5.0, 5.0]).unwrap();
        assert_eq!(
            (b.whisker_low, b.whisker_high, b.outlier_count),
            (5.0, 5.0, 0)
        );
    }

    #[test]
    fn box_stats_near_f64_max_do_not_panic() {
        // `1e308 - -1e308` overflows: the quartiles stay finite and the
        // whiskers are the extremes, with no outliers.
        let b = box_stats(None, &mut [-1e308, 1e308]).unwrap();
        assert!(b.q1.is_finite() && b.q3.is_finite() && b.median.is_finite());
        assert_eq!(
            (b.whisker_low, b.whisker_high, b.outlier_count),
            (-1e308, 1e308, 0)
        );
        let b = box_stats(None, &mut [-1e308, -1e308, 1e308, 1e308, 0.0]).unwrap();
        assert_eq!((b.whisker_low, b.whisker_high), (-1e308, 1e308));
        let b = box_stats(None, &mut [f64::NEG_INFINITY, 1.0, f64::INFINITY]).unwrap();
        assert_eq!(b.outlier_count, 0);
        assert_eq!(
            (b.whisker_low, b.whisker_high),
            (f64::NEG_INFINITY, f64::INFINITY)
        );
    }

    fn total(measures: Vec<Measure>) -> AggregateSpec {
        AggregateSpec {
            x: None,
            split: None,
            measures,
            max_groups: 1000,
        }
    }

    #[test]
    fn a_total_has_no_group_by() {
        let s = total(vec![m(None, AggFn::Count), m(Some("v"), AggFn::Avg)]);
        assert_eq!(
            grouped_sql(&SqlShape::for_dialect("postgres").unwrap(), "SELECT * FROM t", &s).unwrap(),
            "SELECT COUNT(*) AS \"dbine_n\", COUNT(*) AS \"dbine_m0\", AVG(\"v\") AS \"dbine_m1\"\nFROM (\nSELECT * FROM t\n) AS dbine_q"
        );
        // No TOP / FETCH FIRST / FIRST: one row anyway; T-SQL still drops the ORDER BY.
        let ms = grouped_sql(
            &SqlShape::for_dialect("mssql").unwrap(),
            "SELECT v FROM t ORDER BY v",
            &s,
        )
        .unwrap();
        assert_eq!(ms, "SELECT COUNT(*) AS [dbine_n], COUNT(*) AS [dbine_m0], AVG([v] * 1.0E0) AS [dbine_m1]\nFROM (\nSELECT v FROM t\n) AS dbine_q");
        let ora = grouped_sql(
            &SqlShape::for_dialect("oracle").unwrap(),
            "SELECT v FROM t",
            &s,
        )
        .unwrap();
        assert!(
            ora.ends_with(") dbine_q") && !ora.contains("FETCH"),
            "{ora}"
        );
        assert!(!grouped_sql(
            &SqlShape::for_dialect("informix").unwrap(),
            "SELECT v FROM t",
            &s
        )
        .unwrap()
        .contains("FIRST"));
        // A split needs an X.
        assert!(validate(&AggregateSpec {
            split: Some("k".into()),
            ..s.clone()
        })
        .is_err());
        // The UI's JSON: `x` null (or left out).
        let k: ChartKind = serde_json::from_value(json!({ "kind": "group", "x": null, "split": null, "measures": [{ "column": null, "func": "count" }] })).unwrap();
        assert_eq!(k, ChartKind::Group(total(vec![m(None, AggFn::Count)])));
        // The server's one row.
        let out = from_grouped(&s, &[vec![json!(7), json!(7), json!("2.5")]]).unwrap();
        assert_eq!(out.categories, vec![None]);
        assert_eq!(
            (out.series[0].values.clone(), out.series[1].values.clone()),
            (vec![Some(7.0)], vec![Some(2.5)])
        );
        assert_eq!(out.rows_scanned, Some(7));
    }

    #[test]
    fn the_client_totals_like_sql() {
        let s = total(vec![
            m(None, AggFn::Count),
            m(Some("v"), AggFn::Count),
            m(Some("v"), AggFn::Sum),
            m(Some("v"), AggFn::Avg),
            m(Some("v"), AggFn::Min),
            m(Some("v"), AggFn::Max),
        ]);
        let rows: Vec<Vec<Value>> = [json!(1), json!(null), json!("4"), json!("x"), json!(-2)]
            .into_iter()
            .map(|v| vec![json!("g"), v])
            .collect();
        let out = feed(ChartKind::Group(s.clone()), 100, &["g", "v"], &rows);
        assert_eq!(out.categories, vec![None]);
        let got: Vec<Option<f64>> = out.series.iter().map(|x| x.values[0]).collect();
        assert_eq!(
            got,
            vec![
                Some(5.0),
                Some(4.0),
                Some(3.0),
                Some(1.0),
                Some(-2.0),
                Some(4.0)
            ]
        );
        // No rows: COUNT 0, the rest NULL (as SQL without GROUP BY).
        let out = feed(ChartKind::Group(s), 100, &["g", "v"], &[]);
        let got: Vec<Option<f64>> = out.series.iter().map(|x| x.values[0]).collect();
        assert_eq!(got, vec![Some(0.0), Some(0.0), None, None, None, None]);
        assert_eq!(out.rows_scanned, Some(0));
    }
}

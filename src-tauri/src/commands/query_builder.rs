//! "Diseñar consulta" (docs/constructor-de-consultas.md): a `SELECT` built
//! from a spec the visual builder edits (tables on a canvas, the joins
//! between them, a grid of columns with aggregates, sorting and filters).
//! The spec lives in the tab; the SQL is generated here and is never parsed
//! back into a spec.
//!
//! How each engine names a table, quotes and limits rows comes from its
//! driver: a table's browse query (`Session::browse_query`, what "Ver datos"
//! runs) has the table as the engine wants it after `FROM` (a BigQuery
//! dataset, an IoTDB device path, a Couchbase keyspace, a Cosmos DB
//! container…) and the engine's own TOP / FIRST / FETCH FIRST / LIMIT.
//! What it can't tell (which joins, grouping, HAVING…) is per dialect, in
//! [`features`].

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{DriverInfo, Language, ObjectRef, QueryOutcome, ResultColumn, Session};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tauri::State;

/// The row count asked of `browse_query` to find the engine's limit clause.
const SENTINEL: u32 = 7919;
/// Rows "Ejecutar vista previa" brings at most.
const PREVIEW_ROWS: u32 = 100;
/// How long reading the driver's names may wait for the metadata session.
const READ_LIMIT: Duration = Duration::from_secs(20);

// -- spec ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

impl JoinKind {
    fn sql(self) -> &'static str {
        match self {
            Self::Inner => "INNER JOIN",
            Self::Left => "LEFT JOIN",
            Self::Right => "RIGHT JOIN",
            Self::Full => "FULL OUTER JOIN",
        }
    }

    /// The same join seen from the other table.
    fn flipped(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            k => k,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    #[default]
    None,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    CountDistinct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    #[default]
    None,
    Asc,
    Desc,
}

/// A table (or view) on the canvas. Its position is the UI's business.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SpecTable {
    pub id: String,
    pub kind: String,
    pub schema: Option<String>,
    pub name: String,
    /// Empty: the table's name.
    pub alias: String,
}

/// `left.<left> = right.<right>`, columns of the join's two tables.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JoinPair {
    pub left: String,
    pub right: String,
}

/// `left <kind> JOIN right ON …`; `left` and `right` are table ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecJoin {
    #[serde(default)]
    pub id: String,
    pub kind: JoinKind,
    pub left: String,
    pub right: String,
    #[serde(default)]
    pub on: Vec<JoinPair>,
}

/// A filter cell: `<column> <op> <value>`. `raw`: the value is an SQL
/// expression written by the user, not a literal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Condition {
    /// `eq`, `ne`, `lt`, `le`, `gt`, `ge`, `like`, `not_like`, `in`,
    /// `not_in`, `between`, `is_null`, `is_not_null`.
    pub op: String,
    pub value: String,
    /// The upper end of `between`.
    pub value2: String,
    pub raw: bool,
}

/// A row of the grid: a column of a table on the canvas (`*` for all).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpecColumn {
    pub id: String,
    /// The table's id in the spec.
    pub table: String,
    pub column: String,
    /// The column's type as the engine reports it: a numeric column takes
    /// numbers as they are, any other one quoted text.
    pub data_type: String,
    pub alias: String,
    /// In the SELECT list (a hidden row only filters or sorts).
    pub show: bool,
    pub sort: SortDir,
    /// Position among the sorted columns (1 first); `None`: grid order.
    pub sort_order: Option<u32>,
    pub aggregate: Aggregate,
    /// One cell per filter group: the cells of a group are ANDed, the
    /// groups ORed. Conditions on aggregates go to HAVING.
    pub filters: Vec<Option<Condition>>,
}

impl Default for SpecColumn {
    fn default() -> Self {
        Self {
            id: String::new(),
            table: String::new(),
            column: String::new(),
            data_type: String::new(),
            alias: String::new(),
            show: true,
            sort: SortDir::None,
            sort_order: None,
            aggregate: Aggregate::None,
            filters: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuerySpec {
    pub database: String,
    pub tables: Vec<SpecTable>,
    pub joins: Vec<SpecJoin>,
    pub columns: Vec<SpecColumn>,
    pub distinct: bool,
    pub limit: Option<u32>,
}

// -- per engine ---------------------------------------------------------------------------------

/// What the builder offers on an engine (the UI hides the rest).
#[derive(Debug, Clone, Serialize)]
pub struct Features {
    /// Empty: one table per query.
    pub joins: Vec<JoinKind>,
    pub group_by: bool,
    pub having: bool,
    pub aggregates: Vec<Aggregate>,
    pub distinct: bool,
    pub order_by: bool,
    pub limit: bool,
    /// More than one filter group (`OR`).
    pub or_groups: bool,
    pub operators: Vec<&'static str>,
}

const ALL_OPS: &[&str] = &["eq", "ne", "lt", "le", "gt", "ge", "like", "not_like", "in", "not_in", "between", "is_null", "is_not_null"];
const ALL_AGGREGATES: &[Aggregate] =
    &[Aggregate::Count, Aggregate::Sum, Aggregate::Avg, Aggregate::Min, Aggregate::Max, Aggregate::CountDistinct];
const ALL_JOINS: &[JoinKind] = &[JoinKind::Inner, JoinKind::Left, JoinKind::Right, JoinKind::Full];

/// How the engine limits a `SELECT`, as its browse query does it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLimit {
    /// `… LIMIT n`
    Limit,
    /// `SELECT TOP (n) …` (SQL Server)
    TopParen,
    /// `SELECT TOP n …` (Sybase, Teradata, Access, Cosmos DB…)
    Top,
    /// `… FETCH FIRST n ROWS ONLY` (Oracle, Db2)
    FetchFirst,
    /// `SELECT FIRST n …` (Firebird, Informix)
    First,
    /// The engine's query has no row limit (DynamoDB PartiQL).
    None,
}

#[derive(Debug, Clone)]
pub struct Dialect {
    pub quote: Quote,
    pub limit: RowLimit,
    pub features: Features,
    /// MS Access: each join nests the ones before it in parentheses.
    pub nested_joins: bool,
    /// `N'…'` text literals (SQL Server, Sybase).
    pub unicode_strings: bool,
    /// Booleans as 1 / 0.
    pub bit_booleans: bool,
    /// Cosmos DB: a field is `c["name"]` of the container's alias.
    pub cosmos: bool,
    /// InfluxQL: `MEAN`, `COUNT(DISTINCT(x))`.
    pub influxql: bool,
    /// IoTDB: `MIN_VALUE` / `MAX_VALUE`.
    pub iotdb: bool,
    /// CQL: a WHERE outside the key needs `ALLOW FILTERING`.
    pub cql: bool,
    /// ksqlDB push queries: `EMIT CHANGES` before the limit.
    pub emit_changes: bool,
    /// The statement ends with `;` (the engine's browse query does).
    pub terminator: bool,
}

impl Dialect {
    /// From the driver and one browse query of its session (`sample`, asked
    /// with [`SENTINEL`] rows). `sqlite_version`: the server's, on SQLite
    /// (RIGHT and FULL joins arrived in 3.39).
    pub fn resolve(info: &DriverInfo, sample: &str, sqlite_version: Option<&str>) -> Self {
        let target = from_target(sample).map(|t| t.1).unwrap_or_default();
        let quote = if target.contains('[') {
            Quote::Bracket
        } else if target.contains('`') {
            Quote::Backtick
        } else if target.contains('"') {
            Quote::Double
        } else {
            default_quote(info.dialect)
        };
        let limit = limit_style(sample);
        let mut features = features(info, sqlite_version);
        features.limit = limit != RowLimit::None;
        Self {
            quote,
            limit,
            features,
            nested_joins: info.dialect == "access",
            unicode_strings: matches!(info.dialect, "mssql" | "sybase"),
            bit_booleans: matches!(info.dialect, "mssql" | "sybase" | "oracle" | "db2" | "informix"),
            cosmos: info.dialect == "cosmos",
            influxql: info.dialect == "influxql",
            iotdb: info.dialect == "iotdb",
            cql: info.language == Language::Cql,
            emit_changes: sample.to_ascii_uppercase().contains("EMIT CHANGES"),
            terminator: sample.trim_end().ends_with(';'),
        }
    }
}

/// The quote of identifiers when the browse query didn't quote its table.
fn default_quote(dialect: &str) -> Quote {
    match dialect {
        "mssql" | "sybase" | "access" => Quote::Bracket,
        "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks" | "spanner" | "n1ql" | "ksql" | "orientdb"
        | "iotdb" | "tdengine" | "drill" => Quote::Backtick,
        _ => Quote::Double,
    }
}

fn limit_style(sample: &str) -> RowLimit {
    let u = sample.to_ascii_uppercase();
    let has = |s: String| u.contains(&s);
    if has(format!("TOP ({SENTINEL})")) {
        RowLimit::TopParen
    } else if has(format!("TOP {SENTINEL}")) {
        RowLimit::Top
    } else if has(format!("FETCH FIRST {SENTINEL}")) || has(format!("FETCH NEXT {SENTINEL}")) {
        RowLimit::FetchFirst
    } else if has(format!("FIRST {SENTINEL}")) {
        RowLimit::First
    } else if has(format!("LIMIT {SENTINEL}")) {
        RowLimit::Limit
    } else {
        RowLimit::None
    }
}

/// `SQLite 3.45.1`, `libSQL (…) · SQLite 3.43.0`: 3.39 or later.
fn sqlite_outer_joins(version: Option<&str>) -> bool {
    let Some(v) = version.and_then(|v| v.rsplit_once("SQLite ").map(|(_, v)| v)) else {
        return false;
    };
    let mut n = v.split(|c: char| !c.is_ascii_digit()).map(|p| p.parse::<u32>().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0)) >= (3, 39)
}

/// Joins, grouping and filters per engine (the ones it doesn't have stay
/// out of the UI). See docs/soporte-por-motor.md.
pub fn features(info: &DriverInfo, sqlite_version: Option<&str>) -> Features {
    use JoinKind::*;
    let three = vec![Inner, Left, Right];
    let joins = if info.language == Language::Cql {
        vec![]
    } else {
        match (info.id, info.dialect) {
            // One table (container, measurement, device, stream, class) per query.
            (_, "cosmos" | "partiql" | "influxql" | "iotdb" | "tdengine" | "ksql" | "orientdb") => vec![],
            (_, "mysql" | "access") => three,
            (_, "sqlite") if !sqlite_outer_joins(sqlite_version) => vec![Inner, Left],
            (_, "n1ql") => vec![Inner, Left],
            ("heavydb", _) => vec![Inner, Left],
            ("sybase" | "cubrid" | "ignite" | "nuodb" | "openedge" | "zen" | "machbase" | "netsuite", _) => three,
            _ => ALL_JOINS.to_vec(),
        }
    };
    let mut f = Features {
        joins,
        group_by: true,
        having: true,
        aggregates: ALL_AGGREGATES.to_vec(),
        distinct: true,
        order_by: true,
        limit: true,
        or_groups: true,
        operators: ALL_OPS.to_vec(),
    };
    let no_count_distinct = |f: &mut Features| f.aggregates.retain(|a| *a != Aggregate::CountDistinct);
    match info.dialect {
        "cosmos" => {
            f.having = false;
            no_count_distinct(&mut f);
            f.operators.retain(|o| !matches!(*o, "is_null" | "is_not_null"));
        }
        "partiql" => {
            f.group_by = false;
            f.having = false;
            f.aggregates.clear();
            f.distinct = false;
            f.order_by = false;
            f.operators.retain(|o| !matches!(*o, "like" | "not_like"));
        }
        "influxql" => {
            f.having = false;
            f.distinct = false;
            f.operators = vec!["eq", "ne", "lt", "le", "gt", "ge"];
        }
        "iotdb" => {
            f.group_by = false;
            f.having = false;
            f.distinct = false;
            no_count_distinct(&mut f);
        }
        "ksql" => {
            f.distinct = false;
            f.order_by = false;
            no_count_distinct(&mut f);
        }
        "orientdb" => {
            f.having = false;
            no_count_distinct(&mut f);
        }
        "tdengine" | "access" => no_count_distinct(&mut f),
        _ => {}
    }
    if info.language == Language::Cql {
        f.group_by = false;
        f.having = false;
        f.distinct = false;
        f.or_groups = false;
        no_count_distinct(&mut f);
        f.operators = vec!["eq", "lt", "le", "gt", "ge", "in"];
    }
    f
}

/// The engines the builder is for: SQL and CQL.
pub fn builds(info: &DriverInfo) -> bool {
    matches!(info.language, Language::Sql | Language::Cql)
}

// -- names from the browse query ---------------------------------------------------------------

/// How the query names a table: what goes after `FROM`, and lines that
/// must come before the `SELECT` (Cosmos DB's `-- container: …`).
#[derive(Debug, Clone, Default)]
pub struct TableRef {
    pub target: String,
    pub prefix: String,
}

/// `(prefix, target)` of a browse query: its leading comment lines, and the
/// first `FROM`'s target up to unquoted whitespace or `;`.
fn from_target(browse: &str) -> Option<(String, String)> {
    let prefix: Vec<&str> = browse.lines().take_while(|l| l.trim_start().starts_with("--")).collect();
    let chars: Vec<char> = browse.chars().collect();
    let mut i = 0;
    let mut close: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = close {
            if c == q {
                close = None;
            }
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if matches!(c, '"' | '`' | '\'') {
            close = Some(c);
        } else if c == '[' {
            close = Some(']');
        } else if (i == 0 || chars[i - 1].is_whitespace())
            && chars.len() > i + 5
            && chars[i..i + 4].iter().collect::<String>().eq_ignore_ascii_case("from")
            && chars[i + 4].is_whitespace()
        {
            let mut j = i + 4;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let start = j;
            let mut close: Option<char> = None;
            while j < chars.len() {
                let c = chars[j];
                match close {
                    Some(q) if c == q => close = None,
                    Some(_) => {}
                    None if c.is_whitespace() || c == ';' => break,
                    None if matches!(c, '"' | '`') => close = Some(c),
                    None if c == '[' => close = Some(']'),
                    None => {}
                }
                j += 1;
            }
            let target: String = chars[start..j].iter().collect();
            return (!target.is_empty()).then(|| (prefix.join("\n"), target));
        }
        i += 1;
    }
    None
}

impl TableRef {
    fn of(browse: &str, quote: Quote, table: &SpecTable) -> Self {
        match from_target(browse) {
            Some((prefix, target)) => Self { target, prefix },
            None => Self::plain(quote, table),
        }
    }

    fn plain(quote: Quote, table: &SpecTable) -> Self {
        Self { target: qualified_name(quote, table.schema.as_deref(), &table.name), prefix: String::new() }
    }
}

// -- generation ------------------------------------------------------------------------------

/// The SQL of `spec` and what the user should know about it (parts the
/// engine doesn't have, tables left unjoined…). `limit` is the row limit
/// to apply (the spec's, or the preview's). Tables missing from `refs` are
/// named with the dialect's quoting.
pub fn generate(spec: &QuerySpec, d: &Dialect, refs: &HashMap<String, TableRef>, limit: Option<u32>) -> (String, Vec<String>) {
    let mut warn: Vec<String> = Vec::new();
    let f = &d.features;
    let mut tables: Vec<&SpecTable> = spec.tables.iter().collect();
    if tables.is_empty() {
        return (String::new(), warn);
    }
    if f.joins.is_empty() && tables.len() > 1 {
        warn.push(format!("este motor consulta una tabla a la vez: se usa solo «{}»", tables[0].name));
        tables.truncate(1);
    }
    let multi = tables.len() > 1;
    let q = |s: &str| quote_ident(d.quote, s);

    // Aliases: the user's, else the table's name; repeated ones get a number.
    let mut aliases: HashMap<&str, String> = HashMap::new();
    let mut taken: Vec<String> = Vec::new();
    for t in &tables {
        let base = if t.alias.trim().is_empty() { t.name.clone() } else { t.alias.trim().to_string() };
        let mut alias = base.clone();
        let mut n = 2;
        while taken.iter().any(|a| a.eq_ignore_ascii_case(&alias)) {
            alias = format!("{base}{n}");
            n += 1;
        }
        taken.push(alias.clone());
        aliases.insert(t.id.as_str(), alias);
    }
    let reference = |t: &SpecTable| refs.get(&t.id).cloned().unwrap_or_else(|| TableRef::plain(d.quote, t));

    // A column of a table on the canvas, qualified when there are several.
    let column = |table: &str, name: &str| -> Option<String> {
        let alias = aliases.get(table)?;
        Some(if d.cosmos {
            if name == "*" { "*".to_string() } else { format!("c[\"{}\"]", name.replace('\\', "\\\\").replace('"', "\\\"")) }
        } else if name == "*" {
            if multi { format!("{}.*", q(alias)) } else { "*".to_string() }
        } else if multi {
            format!("{}.{}", q(alias), q(name))
        } else {
            q(name)
        })
    };
    let rows: Vec<(&SpecColumn, String)> = spec.columns.iter().filter_map(|c| column(&c.table, &c.column).map(|e| (c, e))).collect();

    let mut unsupported_agg: Vec<&'static str> = Vec::new();
    let value = |c: &SpecColumn, expr: &str, unsupported: &mut Vec<&'static str>| -> String {
        if c.aggregate != Aggregate::None && !f.aggregates.contains(&c.aggregate) {
            unsupported.push(agg_label(c.aggregate));
        }
        aggregate(d, c.aggregate, expr)
    };

    // SELECT list.
    let mut select: Vec<String> = Vec::new();
    for (c, e) in rows.iter().filter(|(c, _)| c.show) {
        let v = value(c, e, &mut unsupported_agg);
        select.push(if c.alias.trim().is_empty() || (c.column == "*" && c.aggregate == Aggregate::None) {
            v
        } else if d.cosmos {
            format!("{v} AS {}", c.alias.trim())
        } else {
            format!("{v} AS {}", q(c.alias.trim()))
        });
    }
    if select.is_empty() {
        select.push("*".into());
    }

    // GROUP BY: with an aggregate, every shown or sorted plain column.
    let grouped = rows.iter().any(|(c, _)| c.aggregate != Aggregate::None);
    let mut group: Vec<String> = Vec::new();
    if grouped {
        for (c, e) in rows.iter().filter(|(c, _)| c.aggregate == Aggregate::None && (c.show || c.sort != SortDir::None)) {
            if c.column == "*" {
                warn.push("con funciones de agregado no se puede mostrar «*»: elegí las columnas".into());
                continue;
            }
            if !group.contains(e) {
                group.push(e.clone());
            }
        }
        if !group.is_empty() && !f.group_by {
            warn.push("este motor no agrupa por columnas: la consulta queda sin GROUP BY".into());
            group.clear();
        }
    }

    // WHERE / HAVING, one group of ANDed cells per filter column.
    let groups = rows.iter().map(|(c, _)| c.filters.len()).max().unwrap_or(0);
    let mut where_groups: Vec<Vec<String>> = Vec::new();
    let mut having_groups: Vec<Vec<String>> = Vec::new();
    let mut mixed = false;
    for g in 0..groups {
        let (mut w, mut h) = (Vec::new(), Vec::new());
        for (c, e) in &rows {
            let Some(Some(cond)) = c.filters.get(g) else { continue };
            let agg = c.aggregate != Aggregate::None;
            let expr = if agg { value(c, e, &mut unsupported_agg) } else { e.clone() };
            if !f.operators.contains(&cond.op.as_str()) {
                warn.push(format!("este motor no ofrece el operador «{}»", op_label(&cond.op)));
            }
            let Some(text) = condition(d, &expr, cond, if agg { "" } else { &c.data_type }) else { continue };
            if agg { h.push(text) } else { w.push(text) }
        }
        mixed |= !w.is_empty() && !h.is_empty();
        if !w.is_empty() {
            where_groups.push(w);
        }
        if !h.is_empty() {
            having_groups.push(h);
        }
    }
    if mixed && where_groups.len() + having_groups.len() > 2 {
        warn.push("las condiciones sobre agregados van a HAVING y las demás a WHERE: cada parte combina sus grupos O por separado".into());
    }
    if !f.or_groups && (where_groups.len() > 1 || having_groups.len() > 1) {
        warn.push("este motor no combina condiciones con OR: se usa solo el primer grupo de filtros".into());
        where_groups.truncate(1);
        having_groups.truncate(1);
    }
    if !having_groups.is_empty() && !f.having {
        warn.push("este motor no filtra agregados (HAVING): esas condiciones quedan fuera".into());
        having_groups.clear();
    }

    // ORDER BY, in the grid's sort order.
    let mut sorted: Vec<(usize, &SpecColumn, &String)> =
        rows.iter().enumerate().filter(|(_, (c, _))| c.sort != SortDir::None && c.column != "*").map(|(i, (c, e))| (i, *c, e)).collect();
    sorted.sort_by_key(|(i, c, _)| (c.sort_order.unwrap_or(u32::MAX), *i));
    let mut order: Vec<String> = Vec::new();
    if !sorted.is_empty() && !f.order_by {
        warn.push("este motor no ordena los resultados: la consulta queda sin ORDER BY".into());
    } else {
        for (_, c, e) in sorted {
            if d.influxql && !c.column.eq_ignore_ascii_case("time") {
                warn.push(format!("InfluxQL ordena solo por time: «{}» no puede ir en ORDER BY", c.column));
                continue;
            }
            let v = if c.aggregate == Aggregate::None { e.clone() } else { aggregate(d, c.aggregate, e) };
            order.push(format!("{v}{}", if c.sort == SortDir::Desc { " DESC" } else { "" }));
        }
    }
    unsupported_agg.sort();
    unsupported_agg.dedup();
    for a in unsupported_agg {
        warn.push(format!("este motor no ofrece {a}"));
    }

    // FROM with the joins: from the first table, each next table joined to
    // the ones already in by every join between them.
    let first = reference(tables[0]);
    let target = |t: &SpecTable| {
        let r = reference(t);
        let alias = &aliases[t.id.as_str()];
        if *alias == t.name { r.target } else { format!("{} {}", r.target, q(alias)) }
    };
    let mut from = target(tables[0]);
    let mut placed: Vec<&str> = vec![tables[0].id.as_str()];
    let usable: Vec<&SpecJoin> = spec
        .joins
        .iter()
        .filter(|j| j.left != j.right && aliases.contains_key(j.left.as_str()) && aliases.contains_key(j.right.as_str()))
        .collect();
    let mut joined = 0;
    while placed.len() < tables.len() {
        let next = tables.iter().find(|t| {
            !placed.contains(&t.id.as_str())
                && usable.iter().any(|j| {
                    !j.on.is_empty()
                        && ((j.left == t.id && placed.contains(&j.right.as_str())) || (j.right == t.id && placed.contains(&j.left.as_str())))
                })
        });
        let Some(t) = next.or_else(|| tables.iter().find(|t| !placed.contains(&t.id.as_str()))) else { break };
        let between: Vec<&&SpecJoin> = usable
            .iter()
            .filter(|j| {
                !j.on.is_empty()
                    && ((j.left == t.id && placed.contains(&j.right.as_str())) || (j.right == t.id && placed.contains(&j.left.as_str())))
            })
            .collect();
        if d.nested_joins && joined > 0 {
            from = format!("({from})");
        }
        if between.is_empty() {
            warn.push(format!("«{}» no está unida a las demás tablas: se combina con todas sus filas (CROSS JOIN)", t.name));
            from.push_str(&if d.nested_joins { format!(", {}", target(t)) } else { format!("\nCROSS JOIN {}", target(t)) });
        } else {
            let kind_of = |j: &SpecJoin| if j.right == t.id { j.kind } else { j.kind.flipped() };
            let kind = kind_of(between[0]);
            if between.iter().any(|j| kind_of(j) != kind) {
                warn.push(format!("«{}» tiene uniones de distinto tipo con las demás tablas: se usa {}", t.name, kind.sql()));
            }
            if !f.joins.contains(&kind) {
                warn.push(format!("este motor no tiene {}", kind.sql()));
            }
            let mut on: Vec<String> = Vec::new();
            for j in &between {
                for p in &j.on {
                    if let (Some(l), Some(r)) = (column(&j.left, &p.left), column(&j.right, &p.right)) {
                        on.push(format!("{l} = {r}"));
                    }
                }
            }
            let on = if d.nested_joins && on.len() > 1 { format!("({})", on.join(" AND ")) } else { on.join(" AND ") };
            from.push_str(&format!("\n{} {} ON {on}", kind.sql(), target(t)));
        }
        joined += 1;
        placed.push(t.id.as_str());
    }
    for j in &spec.joins {
        if j.on.is_empty() && aliases.contains_key(j.left.as_str()) && aliases.contains_key(j.right.as_str()) {
            warn.push("una unión no tiene columnas: arrastrá una columna sobre otra para unirlas".into());
        }
    }

    // Assemble.
    let mut sql = String::new();
    if !first.prefix.is_empty() {
        sql.push_str(&first.prefix);
        sql.push('\n');
    }
    let mut head = String::from("SELECT");
    let distinct = spec.distinct && f.distinct;
    if spec.distinct && !f.distinct {
        warn.push("este motor no ofrece DISTINCT en esta consulta".into());
    }
    let limit = limit.filter(|_| f.limit);
    if spec.limit.is_some() && !f.limit {
        warn.push("este motor no limita las filas en la consulta".into());
    }
    match (d.limit, limit) {
        (RowLimit::First, Some(n)) => head.push_str(&format!(" FIRST {n}")),
        _ => {}
    }
    if distinct {
        head.push_str(" DISTINCT");
    }
    match (d.limit, limit) {
        (RowLimit::TopParen, Some(n)) => head.push_str(&format!(" TOP ({n})")),
        (RowLimit::Top, Some(n)) => head.push_str(&format!(" TOP {n}")),
        _ => {}
    }
    let one_line = select.iter().map(|s| s.len() + 2).sum::<usize>() + head.len() < 90;
    if one_line {
        sql.push_str(&format!("{head} {}", select.join(", ")));
    } else {
        sql.push_str(&format!("{head}\n    {}", select.join(",\n    ")));
    }
    sql.push_str(&format!("\nFROM {from}"));
    let combine = |groups: &[Vec<String>]| -> String {
        if groups.len() == 1 {
            groups[0].join("\n  AND ")
        } else {
            groups.iter().map(|g| if g.len() > 1 { format!("({})", g.join(" AND ")) } else { g[0].clone() }).collect::<Vec<_>>().join("\n   OR ")
        }
    };
    if !where_groups.is_empty() {
        sql.push_str(&format!("\nWHERE {}", combine(&where_groups)));
    }
    if !group.is_empty() {
        sql.push_str(&format!("\nGROUP BY {}", group.join(", ")));
    }
    if !having_groups.is_empty() {
        sql.push_str(&format!("\nHAVING {}", combine(&having_groups)));
    }
    if !order.is_empty() {
        sql.push_str(&format!("\nORDER BY {}", order.join(", ")));
    }
    if d.emit_changes {
        sql.push_str("\nEMIT CHANGES");
    }
    match (d.limit, limit) {
        (RowLimit::Limit, Some(n)) => sql.push_str(&format!("\nLIMIT {n}")),
        (RowLimit::FetchFirst, Some(n)) => sql.push_str(&format!("\nFETCH FIRST {n} ROWS ONLY")),
        _ => {}
    }
    if d.cql && !where_groups.is_empty() {
        sql.push_str("\nALLOW FILTERING");
        warn.push("CQL: un filtro fuera de la clave primaria necesita ALLOW FILTERING, que recorre la tabla".into());
    }
    if d.terminator {
        sql.push(';');
    }
    warn.dedup();
    (sql, warn)
}

fn agg_label(a: Aggregate) -> &'static str {
    match a {
        Aggregate::None => "",
        Aggregate::Count => "COUNT",
        Aggregate::Sum => "SUM",
        Aggregate::Avg => "AVG",
        Aggregate::Min => "MIN",
        Aggregate::Max => "MAX",
        Aggregate::CountDistinct => "COUNT DISTINCT",
    }
}

fn op_label(op: &str) -> &str {
    match op {
        "eq" => "=",
        "ne" => "<>",
        "lt" => "<",
        "le" => "<=",
        "gt" => ">",
        "ge" => ">=",
        "like" => "LIKE",
        "not_like" => "NOT LIKE",
        "in" => "IN",
        "not_in" => "NOT IN",
        "between" => "BETWEEN",
        "is_null" => "IS NULL",
        "is_not_null" => "IS NOT NULL",
        other => other,
    }
}

fn aggregate(d: &Dialect, a: Aggregate, expr: &str) -> String {
    let star = expr == "*" || expr.ends_with(".*");
    let arg = if star { if d.cosmos { "1" } else { "*" } } else { expr };
    match a {
        Aggregate::None => expr.to_string(),
        Aggregate::Count => format!("COUNT({arg})"),
        Aggregate::Sum => format!("SUM({expr})"),
        Aggregate::Avg if d.influxql => format!("MEAN({expr})"),
        Aggregate::Avg => format!("AVG({expr})"),
        Aggregate::Min if d.iotdb => format!("MIN_VALUE({expr})"),
        Aggregate::Min => format!("MIN({expr})"),
        Aggregate::Max if d.iotdb => format!("MAX_VALUE({expr})"),
        Aggregate::Max => format!("MAX({expr})"),
        Aggregate::CountDistinct if d.influxql => format!("COUNT(DISTINCT({expr}))"),
        Aggregate::CountDistinct => format!("COUNT(DISTINCT {expr})"),
    }
}

/// A value typed in a filter cell as a literal of the column's type.
fn literal(d: &Dialect, value: &str, data_type: &str, text: bool) -> String {
    let v = value.trim();
    if v.eq_ignore_ascii_case("null") {
        return "NULL".into();
    }
    let ty = data_type.to_ascii_lowercase();
    if !text {
        if ty.contains("bool") && (v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false")) {
            let on = v.eq_ignore_ascii_case("true");
            return if d.bit_booleans { (if on { "1" } else { "0" }).into() } else { (if on { "TRUE" } else { "FALSE" }).into() };
        }
        let numeric_type = ty.is_empty()
            || ["int", "dec", "num", "float", "double", "real", "money", "serial", "bit"].iter().any(|n| ty.contains(n));
        let number = !v.is_empty()
            && v.parse::<f64>().is_ok()
            && v.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E'));
        if numeric_type && number {
            return v.to_string();
        }
    }
    if d.cosmos {
        return format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"));
    }
    let quoted = format!("'{}'", v.replace('\'', "''"));
    if d.unicode_strings { format!("N{quoted}") } else { quoted }
}

/// A filter cell as SQL; `None` while its value is still empty.
fn condition(d: &Dialect, expr: &str, c: &Condition, data_type: &str) -> Option<String> {
    let lit = |s: &str, text: bool| if c.raw { s.trim().to_string() } else { literal(d, s, data_type, text) };
    let v = c.value.trim();
    let needs_value = !matches!(c.op.as_str(), "is_null" | "is_not_null");
    if needs_value && v.is_empty() {
        return None;
    }
    let cmp = |op: &str| Some(format!("{expr} {op} {}", lit(v, false)));
    match c.op.as_str() {
        "is_null" => Some(format!("{expr} IS NULL")),
        "is_not_null" => Some(format!("{expr} IS NOT NULL")),
        "eq" => cmp("="),
        "ne" => cmp("<>"),
        "lt" => cmp("<"),
        "le" => cmp("<="),
        "gt" => cmp(">"),
        "ge" => cmp(">="),
        "like" => Some(format!("{expr} LIKE {}", lit(v, true))),
        "not_like" => Some(format!("{expr} NOT LIKE {}", lit(v, true))),
        "in" | "not_in" => {
            let list = if c.raw {
                v.to_string()
            } else {
                v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| literal(d, s, data_type, false)).collect::<Vec<_>>().join(", ")
            };
            Some(format!("{expr} {}IN ({list})", if c.op == "not_in" { "NOT " } else { "" }))
        }
        "between" => {
            let hi = c.value2.trim();
            (!hi.is_empty()).then(|| format!("{expr} BETWEEN {} AND {}", lit(v, false), lit(hi, false)))
        }
        _ => None,
    }
}

// -- commands ------------------------------------------------------------------------------------

/// What the driver says about the spec's tables (see the module docs).
struct Probe {
    browses: Vec<String>,
    sample: String,
    version: Option<String>,
}

async fn probe(s: &mut Box<dyn Session>, objects: Vec<ObjectRef>, sqlite: bool) -> dbine_driver::Result<Probe> {
    let browses = objects.iter().map(|o| s.browse_query(o, SENTINEL)).collect();
    let sentinel = ObjectRef { kind: dbine_driver::kinds::TABLE.into(), schema: None, name: "dbine_t".into() };
    let sample = s.browse_query(objects.first().unwrap_or(&sentinel), SENTINEL);
    let version = if sqlite { s.server_version().await.ok() } else { None };
    Ok(Probe { browses, sample, version })
}

/// The engine's dialect and the names of the spec's tables, from the
/// builder tab's session (`session_id`) or else the database's metadata one.
async fn context(
    state: &AppState,
    connection_id: &str,
    session_id: Option<&str>,
    spec: &QuerySpec,
) -> CommandResult<(Dialect, HashMap<String, TableRef>)> {
    let info = driver_of(state, connection_id)?.info();
    if !builds(info) {
        return Err(CommandError::BadRequest("el constructor de consultas es para motores SQL y CQL".into()));
    }
    let objects: Vec<ObjectRef> = spec
        .tables
        .iter()
        .map(|t| ObjectRef {
            kind: if t.kind.is_empty() { dbine_driver::kinds::TABLE.into() } else { t.kind.clone() },
            schema: t.schema.clone(),
            name: t.name.clone(),
        })
        .collect();
    let sqlite = info.dialect == "sqlite";
    let p = match session_id {
        Some(key) => {
            let entry = state.session(key, connection_id, &spec.database).await?;
            let mut s = entry.session.lock().await;
            probe(&mut s, objects, sqlite).await?
        }
        None => state.meta_read(connection_id, &spec.database, READ_LIMIT, move |s| Box::pin(probe(s, objects, sqlite))).await?,
    };
    let d = Dialect::resolve(info, &p.sample, p.version.as_deref());
    let refs = spec.tables.iter().zip(&p.browses).map(|(t, b)| (t.id.clone(), TableRef::of(b, d.quote, t))).collect();
    Ok((d, refs))
}

#[derive(Deserialize)]
pub struct BuildQueryArgs {
    pub connection_id: String,
    pub spec: QuerySpec,
    /// The builder tab's id: its own session, which closing the tab
    /// releases. Without it, the database's metadata session.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Serialize)]
pub struct BuiltQuery {
    pub sql: String,
    pub warnings: Vec<String>,
    pub features: Features,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn build_query(state: State<'_, AppState>, args: BuildQueryArgs) -> CommandResult<BuiltQuery> {
    let (d, refs) = context(&state, &args.connection_id, args.session_id.as_deref(), &args.spec).await?;
    let (sql, warnings) = generate(&args.spec, &d, &refs, args.spec.limit);
    Ok(BuiltQuery { sql, warnings, features: d.features })
}

#[derive(Deserialize)]
pub struct PreviewArgs {
    pub connection_id: String,
    pub spec: QuerySpec,
    /// The builder tab's id; the preview runs on `qb-preview:<id>`, which
    /// `cancel_query` takes.
    pub session_id: String,
}

#[derive(Serialize)]
pub struct BuiltPreview {
    /// What ran (the query with the preview's row limit).
    pub sql: String,
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
    /// More rows than the preview shows.
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// "Ejecutar vista previa": the query with at most [`PREVIEW_ROWS`] rows,
/// on a read-only session of its own.
#[tauri::command(rename_all = "camelCase")]
pub async fn preview_built_query(state: State<'_, AppState>, args: PreviewArgs) -> CommandResult<BuiltPreview> {
    let (d, refs) = context(&state, &args.connection_id, Some(&args.session_id), &args.spec).await?;
    let limit = Some(args.spec.limit.map_or(PREVIEW_ROWS, |n| n.min(PREVIEW_ROWS)));
    let (sql, _) = generate(&args.spec, &d, &refs, limit);
    if sql.is_empty() {
        return Err(CommandError::BadRequest("agregá una tabla para consultar".into()));
    }
    let key = format!("qb-preview:{}", args.session_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.spec.database, true).await?;
    let started = Instant::now();
    let mut out = QueryOutcome::default();
    let run = {
        let mut s = entry.session.lock().await;
        s.execute(&sql, PREVIEW_ROWS as usize, &mut out).await
    };
    state.sessions.remove(&key);
    run?;
    if let Some(e) = out.error.take() {
        return Err(CommandError::Sql(e));
    }
    let r = out.results.into_iter().rev().find(|r| !r.columns.is_empty());
    Ok(match r {
        Some(r) => BuiltPreview {
            sql,
            truncated: r.truncated,
            columns: r.columns,
            rows: r.rows,
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
        None => BuiltPreview { sql, columns: Vec::new(), rows: Vec::new(), truncated: false, elapsed_ms: started.elapsed().as_millis() as u64 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{Family, ObjectKindInfo};

    fn info(id: &'static str, language: Language, dialect: &'static str) -> DriverInfo {
        DriverInfo {
            id,
            name: id,
            family: Family::Relational,
            language,
            dialect,
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: true,
            object_kinds: vec![ObjectKindInfo::tables()],
        }
    }

    fn dialect(id: &'static str, d: &'static str, sample: &str) -> Dialect {
        Dialect::resolve(&info(id, Language::Sql, d), sample, None)
    }
    fn mssql() -> Dialect {
        dialect("sqlserver", "mssql", "SELECT TOP (7919) *\nFROM [dbo].[t]")
    }
    fn postgres() -> Dialect {
        dialect("postgres", "postgres", "SELECT *\nFROM \"public\".\"t\"\nLIMIT 7919")
    }
    fn oracle() -> Dialect {
        dialect("oracle", "oracle", "SELECT *\nFROM \"HR\".\"T\"\nFETCH FIRST 7919 ROWS ONLY")
    }
    fn mysql() -> Dialect {
        dialect("mysql", "mysql", "SELECT *\nFROM `db`.`t`\nLIMIT 7919")
    }

    fn table(id: &str, schema: Option<&str>, name: &str) -> SpecTable {
        SpecTable { id: id.into(), kind: "table".into(), schema: schema.map(Into::into), name: name.into(), alias: String::new() }
    }
    fn col(table: &str, name: &str) -> SpecColumn {
        SpecColumn { id: format!("{table}.{name}"), table: table.into(), column: name.into(), ..Default::default() }
    }
    fn cond(op: &str, value: &str) -> Option<Condition> {
        Some(Condition { op: op.into(), value: value.into(), ..Default::default() })
    }
    fn join(kind: JoinKind, left: &str, right: &str, l: &str, r: &str) -> SpecJoin {
        SpecJoin { id: format!("{left}-{right}"), kind, left: left.into(), right: right.into(), on: vec![JoinPair { left: l.into(), right: r.into() }] }
    }
    fn gen(spec: &QuerySpec, d: &Dialect) -> (String, Vec<String>) {
        generate(spec, d, &HashMap::new(), spec.limit)
    }

    fn one_table(schema: &str) -> QuerySpec {
        QuerySpec {
            tables: vec![table("a", Some(schema), "orders")],
            columns: vec![col("a", "id"), SpecColumn { alias: "Cliente".into(), ..col("a", "customer") }],
            limit: Some(10),
            ..Default::default()
        }
    }

    #[test]
    fn sql_server_uses_top_and_brackets() {
        let (sql, w) = gen(&one_table("dbo"), &mssql());
        assert_eq!(sql, "SELECT TOP (10) [id], [customer] AS [Cliente]\nFROM [dbo].[orders]");
        assert!(w.is_empty());
    }

    #[test]
    fn postgres_uses_limit_and_double_quotes() {
        let (sql, _) = gen(&one_table("public"), &postgres());
        assert_eq!(sql, "SELECT \"id\", \"customer\" AS \"Cliente\"\nFROM \"public\".\"orders\"\nLIMIT 10");
    }

    #[test]
    fn oracle_uses_fetch_first() {
        let (sql, _) = gen(&one_table("HR"), &oracle());
        assert_eq!(sql, "SELECT \"id\", \"customer\" AS \"Cliente\"\nFROM \"HR\".\"orders\"\nFETCH FIRST 10 ROWS ONLY");
    }

    #[test]
    fn mysql_uses_backticks() {
        let (sql, _) = gen(&one_table("shop"), &mysql());
        assert_eq!(sql, "SELECT `id`, `customer` AS `Cliente`\nFROM `shop`.`orders`\nLIMIT 10");
    }

    #[test]
    fn quote_and_limit_come_from_the_browse_query() {
        let d = mssql();
        assert_eq!((d.quote, d.limit), (Quote::Bracket, RowLimit::TopParen));
        let fb = dialect("firebird", "standard", "SELECT FIRST 7919 *\nFROM \"T\"");
        assert_eq!((fb.quote, fb.limit), (Quote::Double, RowLimit::First));
        let ase = dialect("sybase", "sybase", "SELECT TOP 7919 *\nFROM [dbo].[t]");
        assert_eq!(ase.limit, RowLimit::Top);
        let ddb = dialect("dynamodb", "partiql", "SELECT * FROM \"t\"");
        assert_eq!(ddb.limit, RowLimit::None);
        assert!(!ddb.features.limit);
        // An unquoted name: the dialect's quote.
        let orient = dialect("orientdb", "orientdb", "SELECT FROM V LIMIT 7919");
        assert_eq!(orient.quote, Quote::Backtick);
    }

    #[test]
    fn distinct_goes_where_each_engine_wants_it() {
        let mut s = one_table("dbo");
        s.distinct = true;
        assert!(gen(&s, &mssql()).0.starts_with("SELECT DISTINCT TOP (10) "));
        let fb = dialect("firebird", "standard", "SELECT FIRST 7919 *\nFROM \"T\"");
        assert!(gen(&s, &fb).0.starts_with("SELECT FIRST 10 DISTINCT "));
    }

    fn two_tables(kind: JoinKind) -> QuerySpec {
        QuerySpec {
            tables: vec![table("c", Some("public"), "customers"), table("o", Some("public"), "orders")],
            joins: vec![join(kind, "c", "o", "id", "customer_id")],
            columns: vec![col("c", "name"), col("o", "total")],
            ..Default::default()
        }
    }

    #[test]
    fn joins_are_qualified_with_the_aliases() {
        let (sql, w) = gen(&two_tables(JoinKind::Left), &postgres());
        assert_eq!(
            sql,
            "SELECT \"customers\".\"name\", \"orders\".\"total\"\nFROM \"public\".\"customers\"\nLEFT JOIN \"public\".\"orders\" ON \"customers\".\"id\" = \"orders\".\"customer_id\""
        );
        assert!(w.is_empty());
    }

    #[test]
    fn a_join_seen_from_the_other_table_flips() {
        let mut s = two_tables(JoinKind::Left);
        s.tables.reverse();
        let (sql, _) = gen(&s, &postgres());
        assert!(sql.contains("FROM \"public\".\"orders\"\nRIGHT JOIN \"public\".\"customers\" ON"), "{sql}");
    }

    #[test]
    fn full_join_is_flagged_where_missing() {
        let (sql, w) = gen(&two_tables(JoinKind::Full), &mysql());
        assert!(sql.contains("FULL OUTER JOIN"));
        assert_eq!(w, vec!["este motor no tiene FULL OUTER JOIN".to_string()]);
        assert!(!mysql().features.joins.contains(&JoinKind::Full));
        assert!(postgres().features.joins.contains(&JoinKind::Full));
    }

    #[test]
    fn self_join_gets_a_second_alias() {
        let s = QuerySpec {
            tables: vec![table("e", Some("dbo"), "emp"), table("m", Some("dbo"), "emp")],
            joins: vec![join(JoinKind::Inner, "e", "m", "boss_id", "id")],
            columns: vec![col("e", "name"), SpecColumn { alias: "boss".into(), ..col("m", "name") }],
            ..Default::default()
        };
        let (sql, _) = gen(&s, &mssql());
        assert_eq!(
            sql,
            "SELECT [emp].[name], [emp2].[name] AS [boss]\nFROM [dbo].[emp]\nINNER JOIN [dbo].[emp] [emp2] ON [emp].[boss_id] = [emp2].[id]"
        );
    }

    #[test]
    fn unjoined_tables_cross_join_with_a_warning() {
        let mut s = two_tables(JoinKind::Inner);
        s.joins.clear();
        let (sql, w) = gen(&s, &postgres());
        assert!(sql.contains("\nCROSS JOIN \"public\".\"orders\""));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn aggregates_group_by_the_plain_columns_and_filter_with_having() {
        let s = QuerySpec {
            tables: vec![table("c", Some("public"), "customers"), table("o", Some("public"), "orders")],
            joins: vec![join(JoinKind::Inner, "c", "o", "id", "customer_id")],
            columns: vec![
                SpecColumn { sort: SortDir::Asc, ..col("c", "name") },
                SpecColumn {
                    aggregate: Aggregate::Sum,
                    alias: "total".into(),
                    sort: SortDir::Desc,
                    sort_order: Some(1),
                    filters: vec![cond("gt", "100")],
                    ..col("o", "total")
                },
                SpecColumn { show: false, filters: vec![cond("eq", "AR")], data_type: "varchar(2)".into(), ..col("c", "country") },
                SpecColumn { aggregate: Aggregate::CountDistinct, ..col("o", "id") },
            ],
            limit: Some(5),
            ..Default::default()
        };
        let (sql, w) = gen(&s, &postgres());
        assert_eq!(
            sql,
            "SELECT\n    \"customers\".\"name\",\n    SUM(\"orders\".\"total\") AS \"total\",\n    COUNT(DISTINCT \"orders\".\"id\")\n\
             FROM \"public\".\"customers\"\nINNER JOIN \"public\".\"orders\" ON \"customers\".\"id\" = \"orders\".\"customer_id\"\n\
             WHERE \"customers\".\"country\" = 'AR'\nGROUP BY \"customers\".\"name\"\nHAVING SUM(\"orders\".\"total\") > 100\n\
             ORDER BY SUM(\"orders\".\"total\") DESC, \"customers\".\"name\"\nLIMIT 5"
        );
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn filter_groups_are_ored() {
        let s = QuerySpec {
            tables: vec![table("a", Some("dbo"), "t")],
            columns: vec![
                SpecColumn { filters: vec![cond("eq", "x"), cond("like", "y%")], data_type: "nvarchar(10)".into(), ..col("a", "name") },
                SpecColumn { filters: vec![cond("ge", "3"), None], data_type: "int".into(), ..col("a", "n") },
                SpecColumn { show: false, filters: vec![None, cond("is_null", "")], ..col("a", "d") },
            ],
            ..Default::default()
        };
        let (sql, _) = gen(&s, &mssql());
        assert_eq!(sql, "SELECT [name], [n]\nFROM [dbo].[t]\nWHERE ([name] = N'x' AND [n] >= 3)\n   OR ([name] LIKE N'y%' AND [d] IS NULL)");
    }

    #[test]
    fn literals_follow_the_column_type() {
        let d = postgres();
        let c = |op: &str, v: &str, ty: &str| condition(&d, "x", &Condition { op: op.into(), value: v.into(), ..Default::default() }, ty).unwrap();
        assert_eq!(c("eq", "12", "integer"), "x = 12");
        assert_eq!(c("eq", "12", "text"), "x = '12'");
        assert_eq!(c("eq", "O'Brien", "text"), "x = 'O''Brien'");
        assert_eq!(c("eq", "true", "boolean"), "x = TRUE");
        assert_eq!(c("in", "1, 2,3", "int"), "x IN (1, 2, 3)");
        assert_eq!(c("not_in", "a,b", "text"), "x NOT IN ('a', 'b')");
        assert_eq!(c("like", "1%", "int"), "x LIKE '1%'");
        let raw = Condition { op: "ge".into(), value: "CURRENT_DATE - 7".into(), raw: true, ..Default::default() };
        assert_eq!(condition(&d, "x", &raw, "date").unwrap(), "x >= CURRENT_DATE - 7");
        let between = Condition { op: "between".into(), value: "1".into(), value2: "9".into(), ..Default::default() };
        assert_eq!(condition(&d, "x", &between, "int").unwrap(), "x BETWEEN 1 AND 9");
        assert_eq!(condition(&mssql(), "x", &Condition { op: "eq".into(), value: "true".into(), ..Default::default() }, "boolean").unwrap(), "x = 1");
    }

    #[test]
    fn access_nests_joins_and_has_no_full() {
        let d = dialect("access", "access", "SELECT TOP 7919 *\nFROM [t]");
        let s = QuerySpec {
            tables: vec![table("a", None, "a"), table("b", None, "b"), table("c", None, "c")],
            joins: vec![join(JoinKind::Inner, "a", "b", "id", "a_id"), join(JoinKind::Left, "b", "c", "id", "b_id")],
            ..Default::default()
        };
        let (sql, _) = gen(&s, &d);
        assert_eq!(sql, "SELECT *\nFROM ([a]\nINNER JOIN [b] ON [a].[id] = [b].[a_id])\nLEFT JOIN [c] ON [b].[id] = [c].[b_id]");
        assert!(!d.features.joins.contains(&JoinKind::Full));
    }

    #[test]
    fn browse_targets_are_reused() {
        assert_eq!(from_target("SELECT *\nFROM `p`.`ds`.`t`\nLIMIT 5").unwrap().1, "`p`.`ds`.`t`");
        assert_eq!(from_target("SELECT META(d).id AS _id, d.*\nFROM `b`.`s`.`c` AS d\nLIMIT 5").unwrap().1, "`b`.`s`.`c`");
        assert_eq!(from_target("SELECT *\nFROM root.sg.d1\nORDER BY time DESC\nLIMIT 5").unwrap().1, "root.sg.d1");
        assert_eq!(from_target("SELECT * FROM \"my from\" LIMIT 5").unwrap().1, "\"my from\"");
        let (prefix, target) = from_target("-- container: people\nSELECT TOP 5 * FROM c").unwrap();
        assert_eq!((prefix.as_str(), target.as_str()), ("-- container: people", "c"));
    }

    #[test]
    fn cosmos_names_fields_on_the_container() {
        let d = dialect("cosmosdb", "cosmos", "-- container: people\nSELECT TOP 7919 * FROM c");
        let mut refs = HashMap::new();
        refs.insert("p".to_string(), TableRef { target: "c".into(), prefix: "-- container: people".into() });
        let s = QuerySpec {
            tables: vec![table("p", None, "people")],
            columns: vec![SpecColumn { filters: vec![cond("eq", "Ana")], data_type: "string".into(), ..col("p", "name") }],
            limit: Some(3),
            ..Default::default()
        };
        let (sql, _) = generate(&s, &d, &refs, s.limit);
        assert_eq!(sql, "-- container: people\nSELECT TOP 3 c[\"name\"]\nFROM c\nWHERE c[\"name\"] = 'Ana'");
        assert!(d.features.joins.is_empty());
    }

    #[test]
    fn cql_is_single_table_with_allow_filtering() {
        let d = Dialect::resolve(&info("cassandra", Language::Cql, ""), "SELECT *\nFROM \"ks\".\"t\"\nLIMIT 7919", None);
        let s = QuerySpec {
            tables: vec![table("a", Some("ks"), "t"), table("b", Some("ks"), "u")],
            columns: vec![SpecColumn { filters: vec![cond("eq", "1")], data_type: "int".into(), ..col("a", "k") }],
            limit: Some(20),
            ..Default::default()
        };
        let (sql, w) = gen(&s, &d);
        assert_eq!(sql, "SELECT \"k\"\nFROM \"ks\".\"t\"\nWHERE \"k\" = 1\nLIMIT 20\nALLOW FILTERING");
        assert_eq!(w.len(), 2, "{w:?}");
    }

    #[test]
    fn ksql_keeps_emit_changes_and_the_semicolon() {
        let d = dialect("ksqldb", "ksql", "SELECT * FROM `s` EMIT CHANGES LIMIT 7919;");
        let s = QuerySpec { tables: vec![table("a", None, "s")], limit: Some(4), ..Default::default() };
        assert_eq!(gen(&s, &d).0, "SELECT *\nFROM `s`\nEMIT CHANGES\nLIMIT 4;");
    }

    #[test]
    fn sqlite_outer_joins_need_3_39() {
        let sqlite = info("sqlite", Language::Sql, "sqlite");
        assert!(features(&sqlite, Some("SQLite 3.45.1")).joins.contains(&JoinKind::Full));
        assert!(features(&sqlite, Some("libSQL (0.24) · SQLite 3.39.0")).joins.contains(&JoinKind::Right));
        assert!(!features(&sqlite, Some("SQLite 3.31.1")).joins.contains(&JoinKind::Right));
    }

    #[test]
    fn spec_round_trips_with_defaults() {
        let s: QuerySpec = serde_json::from_value(serde_json::json!({
            "database": "main",
            "tables": [{ "id": "a", "name": "t", "x": 10, "y": 20 }],
            "columns": [{ "table": "a", "column": "id" }],
        }))
        .unwrap();
        assert!(s.columns[0].show);
        assert_eq!(s.columns[0].aggregate, Aggregate::None);
    }

    /// A join on a real SQLite file, through the driver: what the builder
    /// writes runs and joins.
    #[tokio::test]
    async fn live_sqlite_join() {
        let dir = std::env::temp_dir().join(format!("dbine-qb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qb.sqlite");
        let _ = std::fs::remove_file(&path);
        let cfg = dbine_driver::ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into(), ..Default::default() };
        let driver = dbine_drivers::find("sqlite").expect("sqlite driver");
        let mut s = driver.connect(&cfg, None).await.unwrap();
        for stmt in [
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id), total REAL)",
            "INSERT INTO customers VALUES (1, 'Ana'), (2, 'Beto'), (3, 'Caro')",
            "INSERT INTO orders VALUES (1, 1, 10), (2, 1, 15), (3, 2, 7)",
        ] {
            s.execute(stmt, 10, &mut QueryOutcome::default()).await.unwrap();
        }

        let spec = QuerySpec {
            database: "main".into(),
            tables: vec![table("c", None, "customers"), table("o", None, "orders")],
            joins: vec![join(JoinKind::Left, "c", "o", "id", "customer_id")],
            columns: vec![
                SpecColumn { sort: SortDir::Asc, ..col("c", "name") },
                SpecColumn { aggregate: Aggregate::Sum, alias: "total".into(), ..col("o", "total") },
                SpecColumn { aggregate: Aggregate::Count, alias: "n".into(), ..col("o", "id") },
            ],
            limit: Some(10),
            ..Default::default()
        };
        let objects = spec.tables.iter().map(|t| ObjectRef { kind: "table".into(), schema: None, name: t.name.clone() }).collect();
        let p = probe(&mut s, objects, true).await.unwrap();
        let d = Dialect::resolve(driver.info(), &p.sample, p.version.as_deref());
        let refs = spec.tables.iter().zip(&p.browses).map(|(t, b)| (t.id.clone(), TableRef::of(b, d.quote, t))).collect();
        let (sql, warnings) = generate(&spec, &d, &refs, spec.limit);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(sql.contains("LEFT JOIN"), "{sql}");

        let mut out = QueryOutcome::default();
        s.execute(&sql, 100, &mut out).await.unwrap();
        let r = out.results.iter().find(|r| !r.columns.is_empty()).expect("a result");
        let rows: Vec<(String, Option<f64>, i64)> = r
            .rows
            .iter()
            .map(|row| (row[0].as_str().unwrap().to_string(), row[1].as_f64(), row[2].as_i64().unwrap()))
            .collect();
        assert_eq!(rows, vec![("Ana".into(), Some(25.0), 2), ("Beto".into(), Some(7.0), 1), ("Caro".into(), None, 0)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

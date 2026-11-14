//! A table read whole and typed (spec §4.6 `read_batches`, G1). Owned by
//! part B. Transfers, data compare, whole-table compare and DataFrame SQL
//! loads read through here; a read is never silently partial.
//!
//! - Arrow (`executeDaxQueries`): one streamed read of the whole table
//!   with an explicit `resultSetRowCountLimit` above its row count (taken
//!   first with `COUNTROWS`); rows = limit → error, not data, and fewer
//!   rows than counted (a stream the service ended early, a table that
//!   shrank) → the same error as a table that changed.
//! - JSON pages project the wanted columns (and the key) only, each under
//!   a name of its own (`SELECTCOLUMNS(…, "__dbine_0", 'T'[c0], …)`), so a
//!   wide table costs what the wanted columns cost, and no measure of the
//!   model can be taken for one of them.
//! - JSON (`executeQueries`, 100,000 rows / 1,000,000 values / 15 MB per
//!   query), with a unique column (`IsKey`/`IsUnique`, a relationship's one
//!   side, or `COLUMNSTATISTICS` cardinality = rows): keyset pages
//!   `EVALUATE TOPN(N, SELECTCOLUMNS(FILTER('T', NOT ISBLANK('T'[k]) &&
//!   'T'[k] > <last>), …), [__dbine_k], ASC) ORDER BY [__dbine_k] ASC`, the
//!   BLANK key read on its own first (BLANK compares as 0 in DAX); DateTime
//!   keys aren't paged on (a literal would lose the milliseconds).
//! - JSON without a unique column: `TOPNSKIP(N, skip, SELECTCOLUMNS('T',
//!   …), <every projected column> ASC…)`, `skip` advanced by the rows each
//!   page returned (ties at the N-th row come back whole, and ordering by
//!   every column only ties identical rows), then the count is checked;
//!   past a size, or with nothing to order by, `Error::Query` with the
//!   reason.
//! - Pages: `N = min(99,999, 1,000,000 / columns - 1)` over the projected
//!   columns (one more row would pass the value cap), then sized to ~10 MB
//!   from the first page; a page the service still cuts is asked again
//!   with half the rows. `ReadSpec::filter` is a DAX condition. Values
//!   typed from the metadata (Arrow types on Arrow).

use crate::catalog;
use crate::dax::quote::{column_ref, literal, quote_column, quote_table};
use crate::http::{error_for, Priority, QueryReply};
use crate::meta::{DataType, ModelMeta, Table};
use crate::query::{self, json, ApiKind, QueryOpts, Sent, Sink, Target};
use crate::Ctx;
use dbine_driver::transfer::{BatchBuilder, Cell, TransferColumn};
use dbine_driver::{BatchSinkRef, Error, ReadSpec, Result};
use serde_json::Value;
use std::time::Duration;

/// Per-query caps of `executeQueries` (a page stays under each: the API, or
/// [`json::capped`], marks a query that reaches them).
#[derive(Debug, Clone, Copy)]
struct Caps {
    rows: usize,
    /// Values per query; a page leaves room for one more row.
    values: usize,
    /// What a page aims at, in bytes (the API cuts at 15 MB).
    bytes: usize,
    /// Rows a read without a unique column may have (`TOPNSKIP` asks the
    /// service to skip the rows before each page).
    topnskip: u64,
}

const CAPS: Caps = Caps {
    rows: json::MAX_ROWS - 1,
    values: json::MAX_VALUES,
    bytes: 10 * 1024 * 1024,
    topnskip: 2_000_000,
};

/// A wanted column: its model name and type.
#[derive(Debug, Clone, PartialEq)]
struct Col {
    name: String,
    ty: DataType,
}

/// What to read, from the spec and the metadata.
#[derive(Debug, Clone)]
struct Plan {
    table: String,
    /// `None`: every column of the result (no metadata).
    columns: Option<Vec<Col>>,
    filter: Option<String>,
    /// The unique column to page on (JSON).
    key: Option<Col>,
    nullable: Vec<bool>,
}

/// `'T'`, or `FILTER('T', (<filter>) && …extra)`.
fn base(p: &Plan, extra: &[String]) -> String {
    let t = quote_table(&p.table);
    let mut conds: Vec<String> = p.filter.iter().map(|f| format!("({f})")).collect();
    conds.extend(extra.iter().cloned());
    if conds.is_empty() {
        t
    } else {
        format!("FILTER({t}, {})", conds.join(" && "))
    }
}

/// The unique column of `t` to page on, as G1 picks it: a key, a unique
/// column, a relationship's one side, or a column whose distinct count is
/// the table's rows; never a DateTime, Binary or Variant one.
fn key_of(t: &Table, meta: &ModelMeta) -> Option<Col> {
    let usable = |ty: DataType| {
        matches!(
            ty,
            DataType::Int64 | DataType::String | DataType::Decimal | DataType::Double
        )
    };
    let one_side = |c: &str| {
        meta.relationships
            .iter()
            .any(|r| r.to_table == t.name && r.to_column == c)
    };
    let pick = |pred: &dyn Fn(&crate::meta::Column) -> bool| {
        t.columns
            .iter()
            .filter(|c| usable(c.data_type))
            .find(|c| pred(c))
            .map(|c| Col {
                name: c.name.clone(),
                ty: c.data_type,
            })
    };
    pick(&|c| c.key)
        .or_else(|| pick(&|c| c.unique))
        .or_else(|| pick(&|c| one_side(&c.name)))
        .or_else(|| {
            let rows = t.rows?;
            pick(&|c| rows > 0 && c.cardinality == Some(rows))
        })
}

fn plan_of(spec: &ReadSpec, meta: Option<&ModelMeta>) -> Result<Plan> {
    let table = spec.table.name.clone();
    let t = meta.and_then(|m| m.tables.iter().find(|t| t.name == table));
    let all: Option<Vec<(Col, bool)>> = t.map(|t| {
        t.columns
            .iter()
            .map(|c| {
                (
                    Col {
                        name: c.name.clone(),
                        ty: c.data_type,
                    },
                    c.nullable || !c.key,
                )
            })
            .collect()
    });
    let picked: Option<Vec<(Col, bool)>> = match (&spec.columns, all) {
        (None, all) => all,
        (Some(names), Some(all)) => Some(
            names
                .iter()
                .map(|n| {
                    all.iter()
                        .find(|(c, _)| c.name.eq_ignore_ascii_case(n))
                        .cloned()
                        .ok_or_else(|| {
                            Error::Query(format!("la tabla «{table}» no tiene la columna «{n}»"))
                        })
                })
                .collect::<Result<_>>()?,
        ),
        (Some(names), None) => Some(
            names
                .iter()
                .map(|n| {
                    (
                        Col {
                            name: n.clone(),
                            ty: DataType::Unknown,
                        },
                        true,
                    )
                })
                .collect(),
        ),
    };
    let key = match (t, meta) {
        (Some(t), Some(m)) => key_of(t, m),
        _ => None,
    };
    let (columns, nullable) = match picked {
        Some(p) => {
            let (c, n): (Vec<Col>, Vec<bool>) = p.into_iter().unzip();
            (Some(c), n)
        }
        None => (None, Vec::new()),
    };
    Ok(Plan {
        table,
        columns,
        filter: spec.filter.clone().filter(|f| !f.trim().is_empty()),
        key,
        nullable,
    })
}

/// A result column's model name (`Sales[Amount]` → `Amount` of `Sales`).
fn model_name<'a>(table: &str, c: &'a str) -> &'a str {
    if let Some(rest) = c.strip_prefix(table) {
        if rest.len() > 2 && rest.starts_with('[') && rest.ends_with(']') {
            return &rest[1..rest.len() - 1];
        }
    }
    if c.starts_with('[') && c.ends_with(']') && c.len() > 2 {
        return &c[1..c.len() - 1];
    }
    c
}

/// A JSON cell typed by the column's model type.
fn typed(v: &Value, ty: DataType) -> Cell {
    use base64::Engine;
    match (ty, v) {
        (_, Value::Null) => Cell::Null,
        (DataType::Int64, Value::Number(n)) => {
            n.as_i64().map_or_else(|| Cell::from_json(v), Cell::Int)
        }
        (DataType::Int64, Value::String(s)) => {
            s.parse().map_or_else(|_| Cell::Text(s.clone()), Cell::Int)
        }
        (DataType::Decimal, Value::Number(n)) => Cell::Decimal(n.to_string()),
        (DataType::Decimal, Value::String(s)) => Cell::Decimal(s.clone()),
        (DataType::Double, Value::Number(n)) => n.as_f64().map_or(Cell::Null, Cell::Float),
        (DataType::DateTime, Value::String(s)) => Cell::DateTime(s.replacen('T', " ", 1)),
        (DataType::Boolean, Value::Bool(b)) => Cell::Bool(*b),
        (DataType::Binary, Value::String(s)) => base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_or_else(|_| Cell::Text(s.clone()), Cell::Bytes),
        (DataType::String, Value::String(s)) => Cell::Text(s.clone()),
        _ => Cell::from_json(v),
    }
}

/// Where a read's batches go.
struct Out {
    sink: BatchSinkRef,
    builder: BatchBuilder,
    begun: bool,
}

impl Out {
    fn begin(&mut self, cols: &[TransferColumn]) -> Result<()> {
        if self.begun {
            return Err(Error::Query(
                "la lectura de la tabla devolvió más de un resultado".into(),
            ));
        }
        self.begun = true;
        self.sink
            .lock()
            .map_err(|_| Error::State("destino de lotes".into()))?
            .begin(cols)?;
        Ok(())
    }

    fn push(&mut self, row: Vec<Cell>) -> Result<()> {
        let mut sink = self
            .sink
            .lock()
            .map_err(|_| Error::State("destino de lotes".into()))?;
        self.builder.push(row, &mut *sink)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<u64> {
        let mut sink = self
            .sink
            .lock()
            .map_err(|_| Error::State("destino de lotes".into()))?;
        self.builder.flush(&mut *sink)?;
        Ok(self.builder.rows)
    }
}

fn transfer_columns(p: &Plan, names: &[(String, String)]) -> Vec<TransferColumn> {
    match &p.columns {
        Some(cols) => cols
            .iter()
            .enumerate()
            .map(|(i, c)| TransferColumn {
                name: c.name.clone(),
                type_name: if c.ty == DataType::Unknown {
                    names.get(i).map(|n| n.1.clone()).unwrap_or_default()
                } else {
                    c.ty.name().to_string()
                },
                nullable: p.nullable.get(i).copied().unwrap_or(true),
            })
            .collect(),
        None => names
            .iter()
            .map(|(n, t)| TransferColumn {
                name: model_name(&p.table, n).to_string(),
                type_name: t.clone(),
                nullable: true,
            })
            .collect(),
    }
}

pub async fn read_batches(cx: &mut Ctx, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    if !matches!(
        spec.table.kind.as_str(),
        crate::TABLE | crate::CALCULATION_GROUP
    ) {
        return Err(Error::Unsupported(
            "en Power BI se leen por lotes las tablas del modelo".into(),
        ));
    }
    let target = Target::of(catalog::model(cx)?);
    let meta = match cx.meta.cached.clone() {
        Some(m) => Some(m),
        None => crate::meta::model(cx).await.ok(),
    };
    let plan = plan_of(spec, meta.as_deref())?;
    let mut out = Out {
        sink,
        builder: BatchBuilder::new(),
        begun: false,
    };
    if query::arrow_available(cx, &target).await? {
        read_arrow(cx, &target, &plan, &mut out).await
    } else {
        read_json(cx, &target, &plan, &mut out, CAPS).await
    }
}

/// `EVALUATE { COUNTROWS(<base>) }`.
async fn count(cx: &mut Ctx, target: &Target, p: &Plan) -> Result<u64> {
    let opts = QueryOpts {
        max_rows: 1,
        priority: Priority::Editor,
        impersonate: false,
        api: None,
    };
    let sets = query::run(
        cx,
        target,
        &format!("EVALUATE {{ COUNTROWS({}) }}", base(p, &[])),
        &opts,
    )
    .await?;
    let v = sets
        .first()
        .and_then(|s| s.rows.first())
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or(Value::Null);
    Ok(match v {
        Value::Null => 0,
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    })
}

/// The projection of the wanted columns (`SELECTCOLUMNS`), or the table.
fn projected(p: &Plan, inner: &str) -> String {
    match &p.columns {
        Some(cols) if !cols.is_empty() => {
            let parts: Vec<String> = cols
                .iter()
                .map(|c| {
                    format!(
                        "{}, {}",
                        crate::dax::quote::string(&c.name),
                        column_ref(&p.table, &c.name)
                    )
                })
                .collect();
            format!("SELECTCOLUMNS({inner}, {})", parts.join(", "))
        }
        _ => inner.to_string(),
    }
}

/// The name of a JSON page's `i`-th projected column.
fn slot(i: usize) -> String {
    format!("__dbine_{i}")
}

/// `SELECTCOLUMNS(<inner>, "__dbine_0", 'T'[c0], …)`: only `cols`, each
/// under its slot's name (`inner` itself when there are none).
fn slots(table: &str, cols: &[Col], inner: &str) -> String {
    if cols.is_empty() {
        return inner.to_string();
    }
    let parts: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "{}, {}",
                crate::dax::quote::string(&slot(i)),
                column_ref(table, &c.name)
            )
        })
        .collect();
    format!("SELECTCOLUMNS({inner}, {})", parts.join(", "))
}

/// The slot a result column names (`[__dbine_3]` → 3).
fn slot_of(c: &str) -> Option<usize> {
    let c = c.strip_prefix('[').unwrap_or(c);
    let c = c.strip_suffix(']').unwrap_or(c);
    c.strip_prefix("__dbine_")?.parse().ok()
}

/// Arrow batches typed into the read's batches.
struct ArrowTyped<'a> {
    plan: &'a Plan,
    out: &'a mut Out,
    cut: bool,
}

impl Sink for ArrowTyped<'_> {
    fn begin(&mut self, columns: Vec<(String, String)>) -> Result<()> {
        let cols = transfer_columns(self.plan, &columns);
        self.out.begin(&cols)
    }

    fn rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        for r in rows {
            self.out.push(r.iter().map(Cell::from_json).collect())?;
        }
        Ok(())
    }

    fn batch(&mut self, b: &arrow_array::RecordBatch) -> Result<()> {
        for r in query::arrow::typed_rows_of(b)? {
            self.out.push(r)?;
        }
        Ok(())
    }

    fn end(&mut self, _rows: u64, cut: bool) -> Result<()> {
        self.cut |= cut;
        Ok(())
    }

    fn waiting(&mut self, _limit: u32) {}
}

async fn read_arrow(cx: &mut Ctx, target: &Target, p: &Plan, out: &mut Out) -> Result<u64> {
    let n = count(cx, target, p).await?;
    // Room for rows added since the count; reaching it is an error.
    let limit = n.saturating_add((n / 10).max(1000)).min(i32::MAX as u64);
    let text = format!("EVALUATE {}", projected(p, &base(p, &[])));
    let mut sink = ArrowTyped {
        plan: p,
        out: &mut *out,
        cut: false,
    };
    match query::arrow_run(cx, target, &text, limit, false, Priority::Editor, &mut sink).await? {
        Sent::Done => {}
        Sent::Unavailable { reason, .. } => return Err(Error::Query(reason)),
    }
    if sink.cut {
        return Err(Error::Query(format!(
            "La lectura de «{}» llegó al límite de {limit} filas que pidió: la tabla creció durante la lectura; volvé a intentar.",
            p.table
        )));
    }
    // A stream that ended early (the decoder takes an end at a message
    // boundary without its end-of-stream marker) or a table that shrank:
    // fewer rows than counted is never a whole read.
    let rows = out.builder.rows;
    if rows < n {
        return Err(changed(&p.table, n, rows));
    }
    if !out.begun {
        out.begin(&transfer_columns(p, &[]))?;
    }
    out.finish()
}

/// One JSON page: its rows (in the projected columns' order), the answer's
/// bytes, and whether the service cut it.
struct Page {
    rows: Vec<Vec<Value>>,
    names: Vec<String>,
    bytes: usize,
    cut: bool,
}

/// A page of `table`. `cols`: the columns the query projects, in slot
/// order ([`slots`]); `None`: the probe of a table without metadata (its
/// columns as the service names them).
async fn json_page(
    cx: &Ctx,
    target: &Target,
    table: &str,
    cols: Option<&[Col]>,
    dax: &str,
) -> Result<Page> {
    cx.api.query_slot(false, Priority::Editor).await?;
    let body = json::body(dax, None);
    let timeout = Some(cx.settings.query_timeout + Duration::from_secs(30));
    let mut stream = match cx
        .api
        .post_query(&target.path(ApiKind::Json), &body, timeout)
        .await?
    {
        QueryReply::Failed(f) => {
            if f.status == 400 {
                let (se, _) = json::statement_error(&f.body);
                return Err(Error::Statement(Box::new(se)));
            }
            return Err(error_for(&f, true));
        }
        QueryReply::Stream(s) => s,
    };
    let (bytes, over) = stream.bytes(json::MAX_BYTES + 4 * 1024 * 1024).await?;
    if over {
        return Ok(Page {
            rows: Vec::new(),
            names: Vec::new(),
            bytes: bytes.len(),
            cut: true,
        });
    }
    let types: Vec<DataType> = cols.unwrap_or_default().iter().map(|c| c.ty).collect();
    let type_of = move |c: &str| {
        slot_of(c)
            .and_then(|i| types.get(i))
            .map(|t| t.name().to_string())
    };
    let parsed = json::parse_typed(&bytes, &type_of)?;
    if let Some((se, _)) = parsed.failure {
        return Err(Error::Statement(Box::new(se)));
    }
    let Some(t) = parsed.tables.into_iter().next() else {
        return Ok(Page {
            rows: Vec::new(),
            names: Vec::new(),
            bytes: bytes.len(),
            cut: false,
        });
    };
    if t.cut {
        // Asked again smaller (or refused): its rows aren't used.
        return Ok(Page {
            rows: Vec::new(),
            names: Vec::new(),
            bytes: bytes.len(),
            cut: true,
        });
    }
    let names: Vec<String> = t
        .columns
        .iter()
        .map(|c| model_name(table, c).to_string())
        .collect();
    let rows = match cols {
        None => t.rows,
        Some(cols) => {
            let at: Vec<Option<usize>> = (0..cols.len())
                .map(|i| t.columns.iter().position(|c| slot_of(c) == Some(i)))
                .collect();
            // Never a column of blanks for one the service named otherwise.
            if !t.rows.is_empty() && at.iter().any(Option::is_none) {
                return Err(Error::Query(format!(
                    "Power BI devolvió columnas inesperadas al leer «{table}» por partes."
                )));
            }
            t.rows
                .into_iter()
                .map(|r| {
                    at.iter()
                        .map(|i| i.and_then(|i| r.get(i).cloned()).unwrap_or(Value::Null))
                        .collect()
                })
                .collect()
        }
    };
    Ok(Page {
        rows,
        names,
        bytes: bytes.len(),
        cut: false,
    })
}

/// The first page's size: the API's caps over the projected columns, with
/// room for one more row under the value cap (so a whole page is never
/// taken for a cut one).
fn first_size(columns: usize, caps: Caps) -> usize {
    caps.rows
        .min((caps.values / columns.max(1)).saturating_sub(1))
        .max(1)
}

/// The next pages' size, from the bytes per row of a page.
fn sized(n: usize, page: &Page, caps: Caps) -> usize {
    if page.rows.is_empty() {
        return n;
    }
    let per_row = (page.bytes / page.rows.len()).max(1);
    n.min((caps.bytes / per_row).max(1))
}

fn too_wide(table: &str) -> Error {
    Error::Query(format!(
        "Power BI corta cada consulta de executeQueries y una sola fila de «{table}» ya pasa ese límite: usá un área de trabajo con capacidad (executeDaxQueries)."
    ))
}

/// A page the service cut: half the rows, or the error when one row is
/// too much.
fn halve(n: usize, table: &str) -> Result<usize> {
    if n <= 1 {
        return Err(too_wide(table));
    }
    Ok(n / 2)
}

async fn read_json(
    cx: &mut Ctx,
    target: &Target,
    p: &Plan,
    out: &mut Out,
    caps: Caps,
) -> Result<u64> {
    let total = count(cx, target, p).await?;
    // Without the metadata the columns come from the first row.
    let mut p = p.clone();
    if p.columns.is_none() {
        let probe = json_page(
            cx,
            target,
            &p.table,
            None,
            &format!("EVALUATE TOPN(1, {})", base(&p, &[])),
        )
        .await?;
        if probe.cut {
            return Err(too_wide(&p.table));
        }
        p.columns = Some(
            probe
                .names
                .iter()
                .map(|n| Col {
                    name: n.clone(),
                    ty: DataType::Unknown,
                })
                .collect(),
        );
    }
    let cols = p.columns.clone().unwrap_or_default();
    out.begin(&transfer_columns(&p, &[]))?;
    let push = |out: &mut Out, rows: Vec<Vec<Value>>| -> Result<()> {
        for r in rows {
            out.push(r.iter().zip(&cols).map(|(v, c)| typed(v, c.ty)).collect())?;
        }
        Ok(())
    };
    if let Some(k) = p.key.clone() {
        let kref = column_ref(&p.table, &k.name);
        let ki = cols.iter().position(|c| c.name == k.name);
        // The key isn't among the wanted columns: read it too.
        let mut wide = cols.clone();
        if ki.is_none() {
            wide.push(k.clone());
        }
        let ki = ki.unwrap_or(cols.len());
        let korder = quote_column(&slot(ki));
        let mut n = first_size(wide.len(), caps);
        let strip = |mut rows: Vec<Vec<Value>>| {
            for r in &mut rows {
                r.truncate(cols.len());
            }
            rows
        };
        // BLANK compares as 0: the blank key on its own, first.
        let blank = json_page(
            cx,
            target,
            &p.table,
            Some(&wide),
            &format!(
                "EVALUATE {}",
                slots(&p.table, &wide, &base(&p, &[format!("ISBLANK({kref})")]))
            ),
        )
        .await?;
        if blank.cut {
            return Err(too_wide(&p.table));
        }
        let mut read = blank.rows.len() as u64;
        push(out, strip(blank.rows))?;
        let mut last: Option<Value> = None;
        loop {
            let mut conds = vec![format!("NOT ISBLANK({kref})")];
            if let Some(v) = &last {
                conds.push(format!("{kref} > {}", literal(v, Some(k.ty.name()))?));
            }
            let dax = format!(
                "EVALUATE TOPN({n}, {}, {korder}, ASC)\nORDER BY {korder} ASC",
                slots(&p.table, &wide, &base(&p, &conds))
            );
            let page = json_page(cx, target, &p.table, Some(&wide), &dax).await?;
            if page.cut {
                n = halve(n, &p.table)?;
                continue;
            }
            let (asked, got) = (n, page.rows.len());
            n = sized(n, &page, caps);
            last = page.rows.last().and_then(|r| r.get(ki)).cloned();
            read += got as u64;
            push(out, strip(page.rows))?;
            // A unique key has no ties: fewer rows than asked is the end.
            if got < asked || last.as_ref().is_none_or(Value::is_null) {
                break;
            }
        }
        if read != total {
            return Err(changed(&p.table, total, read));
        }
    } else {
        if total > caps.topnskip {
            return Err(Error::Query(format!(
                "«{}» no tiene una columna única y la API JSON corta en 100.000 filas por consulta: sus {total} filas no se pueden leer enteras de forma confiable. Usá un área de trabajo con capacidad (executeDaxQueries).",
                p.table
            )));
        }
        let mut n = first_size(cols.len(), caps);
        let order: Vec<String> = cols
            .iter()
            .enumerate()
            .filter(|(_, c)| !matches!(c.ty, DataType::Binary | DataType::Variant))
            .map(|(i, _)| format!("{}, ASC", quote_column(&slot(i))))
            .collect();
        if order.is_empty() && total > 0 {
            return Err(Error::Query(format!(
                "«{}» no tiene columnas por las que ordenar para leerla por partes con executeQueries.",
                p.table
            )));
        }
        let mut skip = 0u64;
        while skip < total {
            let dax = format!(
                "EVALUATE TOPNSKIP({n}, {skip}, {}, {})",
                slots(&p.table, &cols, &base(&p, &[])),
                order.join(", ")
            );
            let page = json_page(cx, target, &p.table, Some(&cols), &dax).await?;
            if page.cut {
                n = halve(n, &p.table)?;
                continue;
            }
            let got = page.rows.len() as u64;
            n = sized(n, &page, caps);
            push(out, page.rows)?;
            if got == 0 {
                break;
            }
            skip += got;
        }
        if skip != total {
            return Err(changed(&p.table, total, skip));
        }
    }
    // The pages aren't one snapshot: a refresh in between shows here.
    let after = count(cx, target, &p).await?;
    if after != total {
        return Err(changed(&p.table, total, after));
    }
    out.finish()
}

/// The table changed while it was read by pages: `total` rows at the
/// start, `seen` read or counted at the end.
fn changed(table: &str, total: u64, seen: u64) -> Error {
    Error::Query(format!(
        "«{table}» cambió durante la lectura (se esperaban {total} filas y hubo {seen}): volvé a intentar."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Model, Workspace};
    use crate::http::testing::{ctx_at, serve, settings, Reply, Request};
    use crate::meta::{Column, Relationship};
    use crate::query::arrow::tests::stream;
    use dbine_driver::transfer::{BatchSink, RowBatch};
    use dbine_driver::ObjectRef;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    #[derive(Default)]
    struct Collect {
        columns: Vec<TransferColumn>,
        rows: Vec<Vec<Cell>>,
    }

    impl BatchSink for Collect {
        fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
            self.columns = columns.to_vec();
            Ok(())
        }
        fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
            self.rows.extend(batch.rows);
            Ok(())
        }
    }

    fn cx_on(base: &str, opts: &[(&str, &str)]) -> Ctx {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut cx = ctx_at(base, settings(opts, true));
        cx.model = Some(Model {
            id: format!("bbbbbbbb-0000-0000-0000-{n:012}"),
            name: "M".into(),
            workspace: Workspace {
                id: Some("a2f89923-421a-464e-bf4c-25eab39bb09f".into()),
                name: "w".into(),
                on_dedicated_capacity: true,
                ..Default::default()
            },
            ..Default::default()
        });
        cx
    }

    fn meta(key: bool) -> Arc<ModelMeta> {
        Arc::new(ModelMeta {
            tables: vec![Table {
                name: "Sales".into(),
                columns: vec![
                    Column {
                        name: "Id".into(),
                        data_type: DataType::Int64,
                        key,
                        ..Default::default()
                    },
                    Column {
                        name: "Amount".into(),
                        data_type: DataType::Decimal,
                        nullable: true,
                        ..Default::default()
                    },
                    Column {
                        name: "Day".into(),
                        data_type: DataType::DateTime,
                        nullable: true,
                        ..Default::default()
                    },
                ],
                rows: Some(25),
                ..Default::default()
            }],
            ..Default::default()
        })
    }

    fn spec(filter: Option<&str>) -> ReadSpec {
        ReadSpec {
            table: ObjectRef {
                kind: crate::TABLE.into(),
                schema: None,
                name: "Sales".into(),
            },
            columns: None,
            filter: filter.map(str::to_string),
        }
    }

    /// The columns a page query projects, in slot order.
    fn slots_in(q: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = q;
        while let Some(i) = rest.find("\"__dbine_") {
            let r = &rest[i..];
            let open = r.find("'Sales'[").expect("a column") + "'Sales'[".len();
            let close = r[open..].find(']').expect("a column") + open;
            out.push(r[open..close].to_string());
            rest = &r[close..];
        }
        out
    }

    /// The JSON rows of `Sales` (a blank id for `None`) as the page query
    /// `q` projects them (`[__dbine_<slot>]` keys, as the service names
    /// SELECTCOLUMNS' columns).
    fn sales(q: &str, ids: impl Iterator<Item = Option<i64>>) -> String {
        let slots = slots_in(q);
        let rows: Vec<String> = ids
            .map(|i| {
                let cells: Vec<String> = slots
                    .iter()
                    .enumerate()
                    .map(|(s, c)| {
                        let v = match c.as_str() {
                            "Id" => i.map_or("null".to_string(), |i| i.to_string()),
                            "Amount" => format!("{}.5000", i.unwrap_or(0)),
                            "Day" => format!(
                                "\"2024-01-0{}T00:00:00\"",
                                1 + i.unwrap_or(0).rem_euclid(9)
                            ),
                            other => panic!("no column {other}"),
                        };
                        format!("\"[__dbine_{s}]\":{v}")
                    })
                    .collect();
                format!("{{{}}}", cells.join(","))
            })
            .collect();
        format!(
            r#"{{"results":[{{"tables":[{{"rows":[{}]}}]}}]}}"#,
            rows.join(",")
        )
    }

    fn count_reply(n: u64) -> Reply {
        Reply::json(
            200,
            &format!(r#"{{"results":[{{"tables":[{{"rows":[{{"[Value]":{n}}}]}}]}}]}}"#),
        )
    }

    type Log = Arc<Mutex<Vec<String>>>;

    async fn fake(
        arrow: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static,
        json_reply: impl Fn(&str) -> Reply + Send + Sync + 'static,
    ) -> (String, Log) {
        let log: Log = Arc::default();
        let l = log.clone();
        let base = serve(Arc::new(move |req: &Request| {
            let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
            if req.path.ends_with("/executeDaxQueries") {
                let q = body["query"].as_str().unwrap_or("").to_string();
                l.lock().unwrap().push(format!("arrow {q}"));
                arrow(&q, &body)
            } else {
                let q = body["queries"][0]["query"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                l.lock().unwrap().push(format!("json {q}"));
                json_reply(&q)
            }
        }))
        .await;
        (base, log)
    }

    #[tokio::test]
    async fn arrow_reads_the_table_in_one_stream_with_its_limit() {
        let (base, log) = fake(
            |q, body| {
                if q.starts_with("EVALUATE {1}") || q.contains("COUNTROWS") {
                    return Reply::arrow(stream(&crate::query::arrow::tests::ints(
                        "[Value]",
                        &[3],
                    )));
                }
                assert_eq!(body["resultSetRowCountLimit"], 1003);
                Reply::arrow(stream(&crate::query::arrow::tests::every_type()))
            },
            |_| Reply::json(500, "{}"),
        )
        .await;
        let mut cx = cx_on(&base, &[]);
        // No metadata for this table: the Arrow types name the columns.
        cx.meta.cached = Some(Arc::new(ModelMeta::default()));
        let sink = Arc::new(Mutex::new(Collect::default()));
        let n = read_batches(&mut cx, &spec(Some("'Sales'[Qty] > 0")), sink.clone())
            .await
            .unwrap();
        assert_eq!(n, 3);
        let got = sink.lock().unwrap();
        let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names[..3], ["Qty", "Ratio", "Amount"]);
        assert_eq!(got.columns[2].type_name, "Decimal");
        assert_eq!(got.rows[0][2], Cell::Decimal("12345.0000".into()));
        assert_eq!(
            got.rows[0][3],
            Cell::DateTime("2024-01-31 13:45:00.123".into())
        );
        let log = log.lock().unwrap();
        assert!(
            log.iter()
                .any(|q| q == "arrow EVALUATE { COUNTROWS(FILTER('Sales', ('Sales'[Qty] > 0))) }"),
            "{log:?}"
        );
        assert!(
            log.iter()
                .any(|q| q == "arrow EVALUATE FILTER('Sales', ('Sales'[Qty] > 0))"),
            "{log:?}"
        );
    }

    #[tokio::test]
    async fn arrow_at_the_limit_is_an_error_not_data() {
        let (base, _) = fake(
            |q, body| {
                if q.starts_with("EVALUATE {1}") || q.contains("COUNTROWS") {
                    return Reply::arrow(stream(&crate::query::arrow::tests::ints(
                        "[Value]",
                        &[0],
                    )));
                }
                // Grew past the room the count left.
                let n = body["resultSetRowCountLimit"].as_i64().unwrap();
                let v: Vec<i64> = (0..n).collect();
                Reply::arrow(stream(&crate::query::arrow::tests::ints("Sales[Id]", &v)))
            },
            |_| Reply::json(500, "{}"),
        )
        .await;
        let mut cx = cx_on(&base, &[]);
        // No metadata for this table: the Arrow types name the columns.
        cx.meta.cached = Some(Arc::new(ModelMeta::default()));
        let sink = Arc::new(Mutex::new(Collect::default()));
        let e = read_batches(&mut cx, &spec(None), sink).await.unwrap_err();
        assert!(e.to_string().contains("creció durante la lectura"), "{e}");
    }

    /// The fake for JSON pages over ids 1..=25 plus a blank id.
    fn json_sales(q: &str) -> Reply {
        if q.contains("COUNTROWS") {
            return count_reply(26);
        }
        if q.contains("ISBLANK('Sales'[Id])") && !q.contains("NOT ISBLANK") {
            return Reply::json(200, &sales(q, [None].into_iter()));
        }
        let n: i64 = q
            .split("TOPN(")
            .nth(1)
            .and_then(|r| r.split(',').next())
            .and_then(|x| x.trim().parse().ok())
            .unwrap_or(0);
        let after: i64 = q
            .split("'Sales'[Id] > ")
            .nth(1)
            .and_then(|r| r.split(')').next())
            .and_then(|x| x.trim().parse().ok())
            .unwrap_or(0);
        Reply::json(
            200,
            &sales(q, ((after + 1)..=(after + n).min(25)).map(Some)),
        )
    }

    #[tokio::test]
    async fn json_pages_on_the_key_blank_first() {
        let (base, log) = fake(|_, _| Reply::json(404, "{}"), json_sales).await;
        let mut cx = cx_on(&base, &[("query_api", "json")]);
        cx.meta.cached = Some(meta(true));
        let target = Target::of(cx.model.as_ref().unwrap());
        let plan = plan_of(&spec(None), Some(&meta(true))).unwrap();
        assert_eq!(plan.key.as_ref().map(|k| k.name.as_str()), Some("Id"));
        let sink = Arc::new(Mutex::new(Collect::default()));
        let mut out = Out {
            sink: sink.clone(),
            builder: BatchBuilder::new(),
            begun: false,
        };
        let caps = Caps {
            rows: 10,
            values: 1000,
            bytes: 1 << 20,
            topnskip: 1000,
        };
        let n = read_json(&mut cx, &target, &plan, &mut out, caps)
            .await
            .unwrap();
        assert_eq!(n, 26);
        let got = sink.lock().unwrap();
        assert_eq!(got.rows[0][0], Cell::Null, "the blank key first");
        let ids: Vec<Cell> = got.rows[1..].iter().map(|r| r[0].clone()).collect();
        assert_eq!(ids, (1..=25).map(Cell::Int).collect::<Vec<_>>());
        assert_eq!(got.rows[1][1], Cell::Decimal("1.5000".into()));
        assert_eq!(got.rows[1][2], Cell::DateTime("2024-01-02 00:00:00".into()));
        let types: Vec<&str> = got.columns.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(types, ["Int64", "Decimal", "DateTime"]);
        let log = log.lock().unwrap();
        assert!(log.iter().any(|q| q.contains("EVALUATE TOPN(10, SELECTCOLUMNS(FILTER('Sales', NOT ISBLANK('Sales'[Id]) && 'Sales'[Id] > 10), \"__dbine_0\", 'Sales'[Id], \"__dbine_1\", 'Sales'[Amount], \"__dbine_2\", 'Sales'[Day]), [__dbine_0], ASC)\nORDER BY [__dbine_0] ASC")), "{log:?}");
    }

    #[tokio::test]
    async fn json_pages_project_only_the_wanted_columns() {
        let (base, log) = fake(|_, _| Reply::json(404, "{}"), json_sales).await;
        let mut cx = cx_on(&base, &[("query_api", "json")]);
        let target = Target::of(cx.model.as_ref().unwrap());
        // One of three columns: the key goes along, Day never does.
        let mut s = spec(None);
        s.columns = Some(vec!["Amount".into()]);
        let plan = plan_of(&s, Some(&meta(true))).unwrap();
        // 9 values a page: 2 projected columns take 3 rows (one more would
        // pass the cap); all 3 columns would take only 2.
        let caps = Caps {
            rows: 10,
            values: 9,
            bytes: 1 << 20,
            topnskip: 1000,
        };
        let sink = Arc::new(Mutex::new(Collect::default()));
        let mut out = Out {
            sink: sink.clone(),
            builder: BatchBuilder::new(),
            begun: false,
        };
        assert_eq!(
            read_json(&mut cx, &target, &plan, &mut out, caps)
                .await
                .unwrap(),
            26
        );
        let got = sink.lock().unwrap();
        let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Amount"]);
        assert_eq!(got.rows[0], vec![Cell::Decimal("0.5000".into())]);
        assert_eq!(got.rows[25], vec![Cell::Decimal("25.5000".into())]);
        let log = log.lock().unwrap();
        assert!(log.iter().any(|q| q.contains("EVALUATE TOPN(3, SELECTCOLUMNS(FILTER('Sales', NOT ISBLANK('Sales'[Id])), \"__dbine_0\", 'Sales'[Amount], \"__dbine_1\", 'Sales'[Id]), [__dbine_1], ASC)\nORDER BY [__dbine_1] ASC")), "{log:?}");
        assert!(log.iter().all(|q| !q.contains("'Sales'[Day]")), "{log:?}");
    }

    #[tokio::test]
    async fn a_column_named_otherwise_is_an_error_not_blanks() {
        let (base, _) = fake(
            |_, _| Reply::json(404, "{}"),
            |q| {
                if q.contains("COUNTROWS") {
                    return count_reply(1);
                }
                Reply::json(
                    200,
                    r#"{"results":[{"tables":[{"rows":[{"Sales[Id]":1,"Sales[Amount]":1.5,"Sales[Day]":null}]}]}]}"#,
                )
            },
        )
        .await;
        let mut cx = cx_on(&base, &[("query_api", "json")]);
        let target = Target::of(cx.model.as_ref().unwrap());
        let plan = plan_of(&spec(None), Some(&meta(false))).unwrap();
        let mut out = Out {
            sink: Arc::new(Mutex::new(Collect::default())),
            builder: BatchBuilder::new(),
            begun: false,
        };
        let e = read_json(&mut cx, &target, &plan, &mut out, CAPS)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("columnas inesperadas"), "{e}");
    }

    #[tokio::test]
    async fn arrow_with_fewer_rows_than_counted_is_an_error() {
        let (base, _) = fake(
            |q, _| {
                if q.starts_with("EVALUATE {1}") || q.contains("COUNTROWS") {
                    return Reply::arrow(stream(&crate::query::arrow::tests::ints(
                        "[Value]",
                        &[5],
                    )));
                }
                // The stream ends after 3 of the 5 rows counted.
                Reply::arrow(stream(&crate::query::arrow::tests::ints(
                    "Sales[Id]",
                    &[1, 2, 3],
                )))
            },
            |_| Reply::json(500, "{}"),
        )
        .await;
        let mut cx = cx_on(&base, &[]);
        cx.meta.cached = Some(Arc::new(ModelMeta::default()));
        let sink = Arc::new(Mutex::new(Collect::default()));
        let e = read_batches(&mut cx, &spec(None), sink).await.unwrap_err();
        assert!(
            e.to_string().contains("se esperaban 5 filas y hubo 3"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn json_without_a_key_pages_with_topnskip_and_checks_the_count() {
        let rows = Arc::new(Mutex::new(25u64));
        let r = rows.clone();
        let (base, log) = fake(
            |_, _| Reply::json(404, "{}"),
            move |q| {
                if q.contains("COUNTROWS") {
                    return count_reply(*r.lock().unwrap());
                }
                let parts: Vec<i64> = q
                    .split("TOPNSKIP(")
                    .nth(1)
                    .unwrap()
                    .split(',')
                    .take(2)
                    .map(|x| x.trim().parse().unwrap())
                    .collect();
                let (n, skip) = (parts[0], parts[1]);
                Reply::json(200, &sales(q, ((skip + 1)..=(skip + n).min(25)).map(Some)))
            },
        )
        .await;
        let mut cx = cx_on(&base, &[("query_api", "json")]);
        let target = Target::of(cx.model.as_ref().unwrap());
        let plan = plan_of(&spec(None), Some(&meta(false))).unwrap();
        assert!(plan.key.is_none());
        let caps = Caps {
            rows: 10,
            values: 1000,
            bytes: 1 << 20,
            topnskip: 1000,
        };
        let sink = Arc::new(Mutex::new(Collect::default()));
        let mut out = Out {
            sink: sink.clone(),
            builder: BatchBuilder::new(),
            begun: false,
        };
        assert_eq!(
            read_json(&mut cx, &target, &plan, &mut out, caps)
                .await
                .unwrap(),
            25
        );
        assert!(log.lock().unwrap().iter().any(|q| q.contains(
            "TOPNSKIP(10, 20, SELECTCOLUMNS('Sales', \"__dbine_0\", 'Sales'[Id], \"__dbine_1\", 'Sales'[Amount], \"__dbine_2\", 'Sales'[Day]), [__dbine_0], ASC, [__dbine_1], ASC, [__dbine_2], ASC)"
        )));
        // The table changed under the read: an error, not a partial read.
        *rows.lock().unwrap() = 30;
        let mut out = Out {
            sink: Arc::new(Mutex::new(Collect::default())),
            builder: BatchBuilder::new(),
            begun: false,
        };
        let e = read_json(&mut cx, &target, &plan, &mut out, caps)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("cambió durante la lectura"), "{e}");
        // Too big to read by skipping.
        *rows.lock().unwrap() = 5000;
        let mut out = Out {
            sink: Arc::new(Mutex::new(Collect::default())),
            builder: BatchBuilder::new(),
            begun: false,
        };
        let e = read_json(&mut cx, &target, &plan, &mut out, caps)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no tiene una columna única"), "{e}");
    }

    #[tokio::test]
    async fn a_page_the_service_cuts_is_asked_again_smaller() {
        let (base, log) = fake(|_, _| Reply::json(404, "{}"), |q| {
            if q.contains("TOPN(10,") {
                return Reply::json(
                    200,
                    r#"{"results":[{"tables":[{"rows":[{"Sales[Id]":1}],"error":{"code":"x","message":"More than 15 MB"}}]}]}"#,
                );
            }
            json_sales(q)
        })
        .await;
        let mut cx = cx_on(&base, &[("query_api", "json")]);
        let target = Target::of(cx.model.as_ref().unwrap());
        let plan = plan_of(&spec(None), Some(&meta(true))).unwrap();
        let caps = Caps {
            rows: 10,
            values: 1000,
            bytes: 1 << 20,
            topnskip: 1000,
        };
        let sink = Arc::new(Mutex::new(Collect::default()));
        let mut out = Out {
            sink: sink.clone(),
            builder: BatchBuilder::new(),
            begun: false,
        };
        assert_eq!(
            read_json(&mut cx, &target, &plan, &mut out, caps)
                .await
                .unwrap(),
            26
        );
        assert!(log.lock().unwrap().iter().any(|q| q.contains("TOPN(5,")));
    }

    #[test]
    fn keys_as_g1_picks_them() {
        let mut m = (*meta(false)).clone();
        assert!(key_of(&m.tables[0], &m).is_none());
        m.relationships.push(Relationship {
            from_table: "Fact".into(),
            from_column: "SaleId".into(),
            to_table: "Sales".into(),
            to_column: "Id".into(),
            active: true,
            ..Default::default()
        });
        assert_eq!(key_of(&m.tables[0], &m).unwrap().name, "Id");
        // Never a DateTime: its literal would lose the milliseconds.
        let mut m = (*meta(false)).clone();
        m.tables[0].columns[2].unique = true;
        assert!(key_of(&m.tables[0], &m).is_none());
        m.tables[0].columns[0].cardinality = Some(25);
        assert_eq!(key_of(&m.tables[0], &m).unwrap().name, "Id");
        // Wanted columns, by name, in their order.
        let mut s = spec(None);
        s.columns = Some(vec!["day".into(), "Id".into()]);
        let p = plan_of(&s, Some(&m)).unwrap();
        let names: Vec<&str> = p
            .columns
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["Day", "Id"]);
        s.columns = Some(vec!["nope".into()]);
        assert!(plan_of(&s, Some(&m)).is_err());
        assert_eq!(
            projected(&p, "'Sales'"),
            "SELECTCOLUMNS('Sales', \"Day\", 'Sales'[Day], \"Id\", 'Sales'[Id])"
        );
    }

    #[test]
    fn cells_typed_from_json() {
        assert_eq!(typed(&json!(5), DataType::Int64), Cell::Int(5));
        assert_eq!(
            typed(&json!("9007199254740993"), DataType::Int64),
            Cell::Int(9007199254740993)
        );
        assert_eq!(
            typed(&json!("1.2300"), DataType::Decimal),
            Cell::Decimal("1.2300".into())
        );
        assert_eq!(typed(&json!(1.5), DataType::Double), Cell::Float(1.5));
        assert_eq!(
            typed(&json!("2024-01-31T00:00:00"), DataType::DateTime),
            Cell::DateTime("2024-01-31 00:00:00".into())
        );
        assert_eq!(
            typed(&json!("yv4="), DataType::Binary),
            Cell::Bytes(vec![0xca, 0xfe])
        );
        assert_eq!(typed(&Value::Null, DataType::String), Cell::Null);
        assert_eq!(model_name("Sales", "Sales[Id]"), "Id");
        assert_eq!(model_name("Sales", "[Id]"), "Id");
        assert_eq!(model_name("Sales", "Other[Id]"), "Other[Id]");
        assert_eq!(slot_of("[__dbine_3]"), Some(3));
        assert_eq!(slot_of("__dbine_0"), Some(0));
        assert_eq!(slot_of("Sales[Id]"), None);
        assert_eq!(slot_of("[__dbine_x]"), None);
        // One more row must still fit under the value cap.
        assert_eq!(first_size(13, CAPS), 76_922);
        assert!(!json::capped(first_size(13, CAPS), 13, 0));
        assert_eq!(first_size(1, CAPS), 99_999);
    }
}

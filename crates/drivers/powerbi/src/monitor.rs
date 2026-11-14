//! A light Monitor (spec §4.6 `monitor`). Owned by part C.
//!
//! Metrics: DBine's query calls in the last minute and the limit
//! ([`crate::http::Api::window_usage`]), model size in memory
//! (`INFO.STORAGETABLECOLUMNSEGMENTS` + `INFO.STORAGETABLECOLUMNS`, admin
//! and capacity), tables. Tables: size by table, rows by table, last
//! refreshes. Info: capacity SKU and state. Notes: no CPU, sessions or
//! queries over REST (the Fabric Capacity Metrics app has them). No table
//! data in it (the phone sees it, S6).
//!
//! The dashboard polls every few seconds: the memory read is kept five
//! minutes and the refresh history a minute, so polling spends almost
//! nothing of the user's 120 queries a minute.

use crate::catalog::Model;
use crate::http::Priority;
use crate::meta::rows::Rows;
use crate::meta::service::Service;
use crate::query::{ApiKind, QueryOpts, Target};
use crate::refresh::{runs_from, Run};
use crate::Ctx;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What the model takes in memory (VertiPaq), from the storage DMVs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Storage {
    /// `(table, bytes, rows)`, largest first.
    pub tables: Vec<(String, u64, u64)>,
    /// `(table, column, bytes)`.
    pub columns: Vec<(String, String, u64)>,
    pub total: u64,
}

const STORAGE_TTL: Duration = Duration::from_secs(300);
const HISTORY_TTL: Duration = Duration::from_secs(60);

static STORAGE: Mutex<BTreeMap<String, (Instant, Option<Storage>)>> = Mutex::new(BTreeMap::new());
static HISTORY: Mutex<BTreeMap<String, (Instant, Vec<Run>)>> = Mutex::new(BTreeMap::new());

fn key<S: Service>(s: &S, m: &Model) -> String {
    let who = s
        .account()
        .map(|a| format!("{}:{}", a.tenant_id, a.object_id))
        .unwrap_or_default();
    format!("{who}|{}", m.id)
}

/// The model's size in memory; `None` when it can't be read (no Arrow, not
/// a model admin). Cached five minutes.
pub(crate) async fn storage<S: Service>(s: &mut S, m: &Model) -> Result<Option<Storage>> {
    let k = key(s, m);
    let hit = STORAGE.lock().ok().and_then(|c| {
        c.get(&k)
            .filter(|e| e.0.elapsed() < STORAGE_TTL)
            .map(|e| e.1.clone())
    });
    if let Some(v) = hit {
        return Ok(v);
    }
    let target = Target::of(m);
    let read = if s.arrow_available(&target).await.unwrap_or(false) {
        let opts = QueryOpts {
            api: Some(ApiKind::Arrow),
            ..QueryOpts::metadata()
        };
        match s
            .dax(
                &target,
                "EVALUATE INFO.STORAGETABLECOLUMNSEGMENTS()\nEVALUATE INFO.STORAGETABLECOLUMNS()",
                &opts,
            )
            .await
        {
            Ok(sets) if sets.len() == 2 => Some(sizes(&sets[0], &sets[1])),
            Err(e) if crate::meta::is_fatal(&e) => return Err(e),
            _ => None,
        }
    } else {
        None
    };
    if let Ok(mut c) = STORAGE.lock() {
        c.retain(|_, e| e.0.elapsed() < STORAGE_TTL);
        c.insert(k, (Instant::now(), read.clone()));
    }
    Ok(read)
}

/// Which storage table a storage-DMV row belongs to.
///
/// Besides each table's own storage table (`TABLE_ID` `Sales (12)`, one
/// `COLUMN_ID` per column), the DMVs list the attribute hierarchies
/// (`H$Sales (12)$Amount (102)`), relationships (`R$…`) and user
/// hierarchies (`U$…`), whose `COLUMN_ID`s are generic (`POS_TO_ID`,
/// `ID_TO_POS`, `INDEX`…) and shared by every column of the table.
#[derive(Debug, PartialEq)]
enum Part {
    /// The table's data: the column's own `COLUMN_ID`.
    Data(String),
    /// A column's attribute hierarchy: that column's `COLUMN_ID`, from the
    /// `TABLE_ID`, as VertiPaq Analyzer reads it.
    Hierarchy(String),
    /// Relationships and user hierarchies: the table's, no column's.
    Table,
}

fn part_of(table: &str, table_id: Option<&str>, column_id: String) -> Part {
    let Some(tid) = table_id else {
        return Part::Data(column_id);
    };
    if let Some(rest) = tid.strip_prefix("H$") {
        // `H$<table> (<id>)$<column> (<id>)`; the table is DIMENSION_NAME.
        return rest
            .strip_prefix(table)
            .and_then(|r| r.strip_prefix(" ("))
            .and_then(|r| r.split_once(")$"))
            .map(|(_, c)| c.to_string())
            .filter(|c| !c.is_empty())
            .map_or(Part::Table, Part::Hierarchy);
    }
    if tid.starts_with("R$") || tid.starts_with("U$") {
        return Part::Table;
    }
    Part::Data(column_id)
}

/// Segment sizes plus dictionary sizes, by table and by column. A column's
/// size is its data, its dictionary and its attribute hierarchy;
/// relationships and user hierarchies count in their table only. Rows come
/// from the table's data segments only (a hierarchy's records are the
/// column's distinct values, not the table's rows).
fn sizes(segments: &crate::query::Rowset, columns: &crate::query::Rowset) -> Storage {
    let mut by_column: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut records: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut names: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut table_only: BTreeMap<String, u64> = BTreeMap::new();
    for r in Rows::new(columns).iter() {
        let (Some(t), Some(c)) = (r.str("DIMENSION_NAME"), r.str("COLUMN_ID")) else {
            continue;
        };
        let bytes = r.u64("DICTIONARY_SIZE").unwrap_or(0);
        match part_of(&t, r.str("TABLE_ID").as_deref(), c) {
            Part::Data(c) => {
                if let Some(a) = r.str("ATTRIBUTE_NAME") {
                    names.insert((t.clone(), c.clone()), a);
                }
                *by_column.entry((t, c)).or_default() += bytes;
            }
            Part::Hierarchy(c) => *by_column.entry((t, c)).or_default() += bytes,
            Part::Table => *table_only.entry(t).or_default() += bytes,
        }
    }
    for r in Rows::new(segments).iter() {
        let (Some(t), Some(c)) = (r.str("DIMENSION_NAME"), r.str("COLUMN_ID")) else {
            continue;
        };
        let bytes = r.u64("USED_SIZE").unwrap_or(0);
        match part_of(&t, r.str("TABLE_ID").as_deref(), c) {
            Part::Data(c) => {
                *records.entry((t.clone(), c.clone())).or_default() +=
                    r.u64("RECORDS_COUNT").unwrap_or(0);
                *by_column.entry((t, c)).or_default() += bytes;
            }
            Part::Hierarchy(c) => *by_column.entry((t, c)).or_default() += bytes,
            Part::Table => *table_only.entry(t).or_default() += bytes,
        }
    }
    let mut out = Storage::default();
    let mut tables: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for ((t, c), bytes) in by_column {
        let rows = records.get(&(t.clone(), c.clone())).copied().unwrap_or(0);
        let e = tables.entry(t.clone()).or_default();
        e.0 += bytes;
        // Every column of a table holds its rows: the largest count is it.
        e.1 = e.1.max(rows);
        out.total += bytes;
        let name = names.get(&(t.clone(), c.clone())).cloned().unwrap_or(c);
        if !name.starts_with("RowNumber-") {
            out.columns.push((t, name, bytes));
        }
    }
    for (t, bytes) in table_only {
        tables.entry(t).or_default().0 += bytes;
        out.total += bytes;
    }
    out.tables = tables.into_iter().map(|(t, (b, r))| (t, b, r)).collect();
    out.tables.sort_by_key(|a| std::cmp::Reverse(a.1));
    out.columns.sort_by_key(|a| std::cmp::Reverse(a.2));
    out
}

/// The newest refreshes (write permission); cached a minute.
pub(crate) async fn history<S: Service>(s: &S, m: &Model) -> Option<Vec<Run>> {
    if !m.write_permission {
        return None;
    }
    let k = key(s, m);
    let hit = HISTORY.lock().ok().and_then(|c| {
        c.get(&k)
            .filter(|e| e.0.elapsed() < HISTORY_TTL)
            .map(|e| e.1.clone())
    });
    if hit.is_some() {
        return hit;
    }
    let runs = runs_from(
        &s.get(
            &format!("{}/refreshes?$top=10", m.path()),
            Priority::Background,
        )
        .await
        .ok()?,
    );
    if let Ok(mut c) = HISTORY.lock() {
        c.retain(|_, e| e.0.elapsed() < HISTORY_TTL);
        c.insert(k, (Instant::now(), runs.clone()));
    }
    Some(runs)
}

/// The most memory a model may take on a capacity SKU, in GB (Learn,
/// "What is Power BI Premium?" / Fabric capacity SKUs); `None` unknown.
pub fn sku_memory_gb(sku: &str) -> Option<u64> {
    Some(match sku.to_ascii_uppercase().as_str() {
        "F2" | "F4" | "F8" | "A1" | "EM1" => 3,
        "F16" | "A2" | "EM2" => 5,
        "F32" | "A3" | "EM3" => 10,
        "F64" | "P1" | "A4" => 25,
        "F128" | "P2" | "A5" => 50,
        "F256" | "P3" | "A6" => 100,
        "F512" | "P4" => 200,
        "F1024" | "P5" | "F2048" => 400,
        s if s.starts_with("PP") => 100,
        _ => return None,
    })
}

/// The capacity as properties and the monitor say it (G4: an unknown SKU
/// says so, never a guess).
pub(crate) async fn capacity_text<S: Service>(s: &mut S, m: &Model) -> (String, Option<String>) {
    if !m.workspace.on_dedicated_capacity {
        return ("compartida (Pro)".into(), None);
    }
    let cap = match &m.workspace.capacity_id {
        Some(id) => s.capacity(id).await.ok().flatten(),
        None => None,
    };
    match cap.and_then(|c| c.sku.map(|sku| (sku, c.state))) {
        Some((sku, state)) => {
            let kind = if sku.to_ascii_uppercase().starts_with("PP") {
                "Premium por usuario".to_string()
            } else if sku.to_ascii_uppercase().starts_with('F') {
                format!("Fabric {sku}")
            } else {
                format!("Premium {sku}")
            };
            let text = match state {
                Some(st) => format!("{kind} ({st})"),
                None => kind,
            };
            (text, Some(sku))
        }
        None => ("capacidad (sin permiso para ver el SKU)".into(), None),
    }
}

fn status_text(r: &Run) -> &'static str {
    if r.running() {
        return "en curso";
    }
    match r.status.as_str() {
        "Completed" => "correcta",
        "Failed" => "fallida",
        "Cancelled" => "cancelada",
        "Disabled" => "desactivada",
        _ => "desconocida",
    }
}

pub async fn monitor(cx: &mut Ctx) -> Result<MonitorSnapshot> {
    monitor_in(cx).await
}

pub(crate) async fn monitor_in<S: Service>(s: &mut S) -> Result<MonitorSnapshot> {
    let mut snap = MonitorSnapshot::default();
    let w = s.window_usage();
    snap.metrics.push(
        Metric::new(
            "dbine_queries",
            "Consultas de DBine en el último minuto",
            "Actividad",
            MetricUnit::Count,
            Some(f64::from(w.used)),
        )
        .max((w.limit > 0).then_some(f64::from(w.limit)))
        .help("Consultas DAX que DBine mandó con esta cuenta en los últimos 60 segundos. Power BI admite 120 por minuto por usuario (40 con executeDaxQueries en Pro y Premium por usuario); cerca del máximo, las consultas esperan.")
        .source("ventana de consultas de DBine (executeQueries / executeDaxQueries)"),
    );
    snap.notes.push(
        "La API REST de Power BI no informa CPU, sesiones ni consultas en curso: la app Fabric Capacity Metrics los muestra.".into(),
    );
    let Some(m) = s.session_model().cloned() else {
        snap.notes.push(
            "Elegí un modelo semántico en la conexión para ver su memoria y sus actualizaciones."
                .into(),
        );
        return Ok(snap);
    };
    snap.info
        .push(("Área de trabajo".into(), m.workspace.name.clone()));
    snap.info.push(("Modelo".into(), m.name.clone()));
    let (cap, sku) = capacity_text(s, &m).await;
    snap.info.push(("Capacidad".into(), cap));
    let limit = sku
        .as_deref()
        .and_then(sku_memory_gb)
        .map(|gb| (gb * 1024 * 1024 * 1024) as f64);
    match storage(s, &m).await? {
        Some(st) => {
            snap.metrics.push(
                Metric::new("model_memory", "Memoria del modelo", "Memoria", MetricUnit::Bytes, Some(st.total as f64))
                    .max(limit)
                    .help("Lo que el modelo ocupa en memoria (diccionarios y segmentos de VertiPaq). Cerca del máximo de la capacidad, las actualizaciones y las consultas fallan.")
                    .source("INFO.STORAGETABLECOLUMNSEGMENTS + INFO.STORAGETABLECOLUMNS"),
            );
            snap.metrics.push(Metric::new(
                "tables",
                "Tablas",
                "Modelo",
                MetricUnit::Count,
                Some(st.tables.len() as f64),
            ));
            let mut t = MonitorTable::new("table_sizes", "Tamaño por tabla", &["Tabla", "Filas", "Memoria (bytes)"]);
            for (name, bytes, rows) in &st.tables {
                t.rows.push(vec![json!(name), json!(rows), json!(bytes)]);
            }
            snap.tables.push(t);
        }
        None => snap.notes.push(
            "La memoria del modelo se lee con funciones INFO de administrador del modelo, en capacidad Premium o Fabric.".into(),
        ),
    }
    match history(s, &m).await {
        Some(runs) => {
            let mut t = MonitorTable::new(
                "refreshes",
                "Últimas actualizaciones",
                &["Inicio", "Estado", "Origen", "Duración (s)"],
            );
            for r in runs {
                t.rows.push(vec![
                    json!(r.start.as_deref().and_then(crate::refresh::utc)),
                    json!(status_text(&r)),
                    json!(r.trigger()),
                    json!(r.duration_ms().map(|d| d / 1000)),
                ]);
            }
            snap.tables.push(t);
        }
        None => snap.notes.push(
            "El historial de actualizaciones necesita permiso de escritura en el modelo.".into(),
        ),
    }
    Ok(snap)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::Capacity;
    use crate::http::Method;
    use crate::meta::fake::{rowset, Fake};
    use serde_json::json;

    pub(crate) fn storage_answers(f: &mut Fake) {
        f.dax_answer(
            "INFO.STORAGETABLECOLUMNSEGMENTS()",
            vec![rowset(
                &[
                    "[DATABASE_NAME]",
                    "[CUBE_NAME]",
                    "[MEASURE_GROUP_NAME]",
                    "[DIMENSION_NAME]",
                    "[TABLE_ID]",
                    "[COLUMN_ID]",
                    "[SEGMENT_NUMBER]",
                    "[TABLE_PARTITION_NUMBER]",
                    "[RECORDS_COUNT]",
                    "[ALLOCATED_SIZE]",
                    "[USED_SIZE]",
                ],
                vec![
                    json!([
                        "db",
                        "Model",
                        "Sales",
                        "Sales",
                        "Sales (12)",
                        "Amount (102)",
                        0,
                        0,
                        60398,
                        262144,
                        120000
                    ]),
                    json!([
                        "db",
                        "Model",
                        "Sales",
                        "Sales",
                        "Sales (12)",
                        "Comment (104)",
                        0,
                        0,
                        60398,
                        9000000,
                        8000000
                    ]),
                    json!([
                        "db",
                        "Model",
                        "Customer",
                        "Customer",
                        "Customer (13)",
                        "City (111)",
                        0,
                        0,
                        18484,
                        65536,
                        30000
                    ]),
                    // Attribute hierarchies: one storage table per column,
                    // the same generic COLUMN_ID in each, records = the
                    // column's distinct values (together, more than the
                    // table's rows).
                    json!([
                        "db",
                        "Model",
                        "Sales",
                        "Sales",
                        "H$Sales (12)$Amount (102)",
                        "POS_TO_ID",
                        0,
                        0,
                        50000,
                        8192,
                        4000
                    ]),
                    json!([
                        "db",
                        "Model",
                        "Sales",
                        "Sales",
                        "H$Sales (12)$Comment (104)",
                        "POS_TO_ID",
                        0,
                        0,
                        40000,
                        8192,
                        3000
                    ]),
                    // A relationship's index: the table's, no column's.
                    json!([
                        "db",
                        "Model",
                        "Sales",
                        "Sales",
                        "R$Sales (12)$5b1e7c3a-3c3e-4e5d-9a8f-2b7a6b1c0d9e",
                        "INDEX",
                        0,
                        0,
                        60398,
                        8192,
                        2000
                    ]),
                ],
            )],
        );
        f.dax_answer(
            "INFO.STORAGETABLECOLUMNS()",
            vec![rowset(
                &[
                    "[DATABASE_NAME]",
                    "[CUBE_NAME]",
                    "[MEASURE_GROUP_NAME]",
                    "[DIMENSION_NAME]",
                    "[ATTRIBUTE_NAME]",
                    "[TABLE_ID]",
                    "[COLUMN_ID]",
                    "[COLUMN_TYPE]",
                    "[COLUMN_ENCODING]",
                    "[DATATYPE]",
                    "[ISKEY]",
                    "[ISUNIQUE]",
                    "[ISNULLABLE]",
                    "[ISROWNUMBER]",
                    "[DICTIONARY_SIZE]",
                ],
                vec![
                    json!([
                        "db",
                        "Model",
                        "Sales",
                        "Sales",
                        "Amount",
                        "Sales (12)",
                        "Amount (102)",
                        "BASIC_DATA",
                        1,
                        "DBTYPE_CY",
                        false,
                        false,
                        true,
                        false,
                        1000
                    ]),
                    json!([
                        "db",
                        "Model",
                        "Sales",
                        "Sales",
                        "Comment",
                        "Sales (12)",
                        "Comment (104)",
                        "BASIC_DATA",
                        2,
                        "DBTYPE_WSTR",
                        false,
                        false,
                        true,
                        false,
                        500000
                    ]),
                    json!([
                        "db",
                        "Model",
                        "Customer",
                        "Customer",
                        "City",
                        "Customer (13)",
                        "City (111)",
                        "BASIC_DATA",
                        2,
                        "DBTYPE_WSTR",
                        false,
                        false,
                        true,
                        false,
                        4000
                    ]),
                    json!([
                        "db",
                        "Model",
                        "Sales",
                        "Sales",
                        "POS_TO_ID",
                        "H$Sales (12)$Amount (102)",
                        "POS_TO_ID",
                        "BASIC_DATA",
                        2,
                        "DBTYPE_I8",
                        false,
                        false,
                        true,
                        false,
                        120
                    ]),
                ],
            )],
        );
    }

    #[tokio::test]
    async fn the_snapshot_reads_window_memory_and_refreshes() {
        let mut f = Fake::writer();
        f.arrow = true;
        f.model.as_mut().unwrap().workspace.on_dedicated_capacity = true;
        f.model.as_mut().unwrap().workspace.capacity_id =
            Some("0f084df7-c13d-451b-af5f-ed0c466403b2".into());
        f.capacities.insert(
            "0f084df7-c13d-451b-af5f-ed0c466403b2".into(),
            Capacity {
                id: "0f084df7-c13d-451b-af5f-ed0c466403b2".into(),
                sku: Some("F64".into()),
                state: Some("Active".into()),
                region: Some("Brazil South".into()),
            },
        );
        storage_answers(&mut f);
        let m = f.model.clone().unwrap();
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=10", m.path()),
            200,
            serde_json::from_str(crate::refresh::tests::HISTORY_FAILED).unwrap(),
        );
        let snap = monitor_in(&mut f).await.unwrap();
        let q = snap
            .metrics
            .iter()
            .find(|m| m.key == "dbine_queries")
            .unwrap();
        assert_eq!((q.value, q.max), (Some(3.0), Some(120.0)));
        let mem = snap
            .metrics
            .iter()
            .find(|m| m.key == "model_memory")
            .unwrap();
        // Data, dictionaries, hierarchies and the relationship's index.
        assert_eq!(mem.value, Some(8_664_120.0));
        assert_eq!(mem.max, Some(25.0 * 1024.0 * 1024.0 * 1024.0));
        assert!(snap
            .info
            .contains(&("Capacidad".into(), "Fabric F64 (Active)".into())));
        let sizes = snap.tables.iter().find(|t| t.key == "table_sizes").unwrap();
        // The rows are the data's, not the hierarchies' summed records.
        assert_eq!(
            sizes.rows[0],
            vec![json!("Sales"), json!(60398), json!(8_630_120)]
        );
        let refreshes = snap.tables.iter().find(|t| t.key == "refreshes").unwrap();
        assert_eq!(refreshes.rows[0][1], json!("fallida"));
        // Polled again: nothing new is asked.
        let calls = (f.dax_calls(), f.requests().len());
        monitor_in(&mut f).await.unwrap();
        assert_eq!((f.dax_calls(), f.requests().len()), calls);
    }

    #[tokio::test]
    async fn without_a_model_or_rights_it_says_why() {
        let mut f = Fake::new();
        f.model = None;
        let snap = monitor_in(&mut f).await.unwrap();
        assert_eq!(snap.metrics.len(), 1);
        assert!(snap.notes.iter().any(|n| n.contains("Elegí un modelo")));
        let mut f = Fake::new();
        f.model.as_mut().unwrap().workspace.on_dedicated_capacity = true;
        let snap = monitor_in(&mut f).await.unwrap();
        assert!(snap.info.contains(&(
            "Capacidad".into(),
            "capacidad (sin permiso para ver el SKU)".into()
        )));
        assert!(snap.notes.iter().any(|n| n.contains("administrador")));
        assert!(snap
            .notes
            .iter()
            .any(|n| n.contains("permiso de escritura")));
    }

    #[tokio::test]
    async fn hierarchies_go_to_their_column_and_relationships_to_the_table() {
        let mut f = Fake::writer();
        f.arrow = true;
        storage_answers(&mut f);
        let m = f.model.clone().unwrap();
        let st = storage(&mut f, &m).await.unwrap().unwrap();
        assert_eq!(
            st.columns,
            [
                ("Sales".into(), "Comment".into(), 8_503_000),
                ("Sales".into(), "Amount".into(), 125_120),
                ("Customer".into(), "City".into(), 34_000),
            ]
        );
        assert_eq!(st.total, 8_664_120);
        assert_eq!(
            part_of(
                "Sales",
                Some("H$Sales (12)$Amount (102)"),
                "POS_TO_ID".into()
            ),
            Part::Hierarchy("Amount (102)".into())
        );
        assert_eq!(
            part_of("Sales", Some("U$Sales (12)$Geo (400)"), "POS_TO_ID".into()),
            Part::Table
        );
        assert_eq!(
            part_of("Sales", Some("Sales (12)"), "Amount (102)".into()),
            Part::Data("Amount (102)".into())
        );
        // An H$ id that doesn't read as one stays with the table.
        assert_eq!(
            part_of("Sales", Some("H$Other (1)$X (2)"), "POS_TO_ID".into()),
            Part::Table
        );
    }

    #[test]
    fn sku_limits() {
        assert_eq!(sku_memory_gb("F64"), Some(25));
        assert_eq!(sku_memory_gb("p1"), Some(25));
        assert_eq!(sku_memory_gb("PP3"), Some(100));
        assert_eq!(sku_memory_gb("X9"), None);
    }
}

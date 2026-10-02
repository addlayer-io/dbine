//! A table's indexes and how they're used (`Session::index_usage`).
//!
//! BigQuery's only indexes are search indexes (one per table at most) and
//! vector indexes (one per column), read from the dataset's
//! `SEARCH_INDEXES` / `VECTOR_INDEXES` (definition from the `ddl` column,
//! as `database_schema` reads them, so the schema sync can drop them; size
//! `total_storage_bytes`; last write `last_refresh_time`). Primary and
//! foreign keys are unenforced constraints, not indexes: the primary key
//! isn't listed (the explorer marks its columns from `columns`), the
//! foreign keys come from `tables.get` (`tableConstraints.foreignKeys`).
//!
//! Counters: BigQuery records per query job whether it used the table's
//! search or vector index (`search_statistics.index_usage_mode` /
//! `vector_search_statistics.index_usage_mode`, `FULLY_USED` or
//! `PARTIALLY_USED`) in the region's `INFORMATION_SCHEMA.JOBS`, kept for 180
//! days. Reads (`seeks`) = jobs that used the index in that window; `since`
//! = its start; last read = the last such job. One counter (no seeks
//! against scans: `seek_scan_split` false) and no write counter
//! (`writes_counted` false), so no index is "sin uso". With more than one vector index the job doesn't say
//! which one it used: their counters stay at zero and the note says so.
//! `JOBS` needs `bigquery.jobs.listAll`; without it `JOBS_BY_USER` (the
//! login's own jobs) and a note; without either, no counters.
//!
//! The statistics are per job, not per table: a job that joins this table
//! with another one whose index it used would count here too. A
//! `PARTIALLY_USED` job lists the tables whose index it didn't use
//! (`index_unused_reasons[].base_table`), so those are left out; a
//! `FULLY_USED` job that searched only the other table still counts (the
//! job doesn't say which tables' indexes it used), so the number is an
//! upper bound and the note says so. If the region's `JOBS` doesn't take
//! the `base_table` filter, the plain count is used.
//!
//! Reading `JOBS` is a billed query (the bytes of 180 days of the region's
//! jobs), run every time the folder of a table with indexes is opened; the
//! note says so.

use crate::ddl::{ident, lit, Row};
use crate::indexes::{self, SEARCH, VECTOR};
use crate::monitor::region_qualifier;
use crate::BigQuerySession;
use dbine_driver::{ForeignKeyDef, IndexUsage, IndexUsageReport, ObjectRef, Result};
use serde_json::Value as Json;

/// `SEARCH_INDEXES` / `VECTOR_INDEXES` rows of one table.
pub fn indexes_sql(project: &str, dataset: &str, view: &str, table: &str) -> String {
    format!(
        "SELECT index_name, ddl, index_status, CAST(total_storage_bytes AS STRING) AS total_storage_bytes,
                FORMAT_TIMESTAMP('%F %T', last_refresh_time) AS last_refresh_time
         FROM {}.{}.INFORMATION_SCHEMA.{view} WHERE table_name = {}",
        ident(project),
        ident(dataset),
        lit(table)
    )
}

/// The jobs of the last 180 days that used the table's search or vector
/// index. `view`: `JOBS` or `JOBS_BY_USER`. `precise`: a `PARTIALLY_USED`
/// job that says it didn't use this table's index doesn't count.
pub fn usage_sql(location: Option<&str>, view: &str, project: &str, dataset: &str, table: &str, precise: bool) -> String {
    let this = format!("u.base_table.project_id = {} AND u.base_table.dataset_id = {} AND u.base_table.table_id = {}", lit(project), lit(dataset), lit(table));
    let used = |stats: &str| {
        if precise {
            format!(
                "(j.{stats}.index_usage_mode = 'FULLY_USED' OR (j.{stats}.index_usage_mode = 'PARTIALLY_USED'
                  AND NOT EXISTS (SELECT 1 FROM UNNEST(j.{stats}.index_unused_reasons) AS u WHERE {this})))"
            )
        } else {
            format!("j.{stats}.index_usage_mode IN ('FULLY_USED', 'PARTIALLY_USED')")
        }
    };
    let (s, v) = (used("search_statistics"), used("vector_search_statistics"));
    format!(
        "SELECT CAST(COUNT(DISTINCT IF({s}, j.job_id, NULL)) AS STRING) AS search_used,
                FORMAT_TIMESTAMP('%F %T', MAX(IF({s}, j.creation_time, NULL))) AS search_last,
                CAST(COUNT(DISTINCT IF({v}, j.job_id, NULL)) AS STRING) AS vector_used,
                FORMAT_TIMESTAMP('%F %T', MAX(IF({v}, j.creation_time, NULL))) AS vector_last,
                FORMAT_TIMESTAMP('%F %T', TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL 180 DAY)) AS since
         FROM {}.INFORMATION_SCHEMA.{view} AS j, UNNEST(j.referenced_tables) AS rt
         WHERE j.creation_time > TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL 180 DAY)
           AND rt.project_id = {} AND rt.dataset_id = {} AND rt.table_id = {}",
        region_qualifier(location),
        lit(project),
        lit(dataset),
        lit(table)
    )
}

/// Index rows of one kind (`SEARCH` / `VECTOR`) as indexes.
pub fn indexes(kind: &str, rows: &[Row]) -> Vec<IndexUsage> {
    rows.iter()
        .filter_map(|r| {
            let name = r.get("index_name")?;
            let def = indexes::parse(name, kind, r.get("ddl")?)?;
            let status = r.get("index_status").map(String::as_str).unwrap_or("ACTIVE");
            Some(IndexUsage {
                name: def.name,
                kind: if status == "ACTIVE" { format!("{kind} INDEX") } else { format!("{kind} INDEX ({status})") },
                key_columns: def.columns,
                included_columns: def.include,
                size_kb: r.get("total_storage_bytes").and_then(|b| b.parse::<u64>().ok()).map(|b| b.div_ceil(1024)),
                last_write: r.get("last_refresh_time").cloned(),
                ..Default::default()
            })
        })
        .collect()
}

/// The usage row onto the indexes; true when every index got its counter.
pub fn apply(ixs: &mut [IndexUsage], usage: &Row) -> bool {
    let n = |k: &str| usage.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let vectors = ixs.iter().filter(|i| i.kind.starts_with(VECTOR)).count();
    for i in ixs.iter_mut() {
        if i.kind.starts_with(SEARCH) {
            i.seeks = n("search_used");
            i.last_read = usage.get("search_last").cloned();
        } else if vectors == 1 {
            i.seeks = n("vector_used");
            i.last_read = usage.get("vector_last").cloned();
        }
    }
    vectors <= 1
}

/// `tableConstraints.foreignKeys` of a `tables.get` answer.
pub fn foreign_keys(table: &Json, project: &str) -> Vec<ForeignKeyDef> {
    let s = |v: &Json, k: &str| v.get(k).and_then(Json::as_str).unwrap_or_default().to_string();
    table
        .pointer("/tableConstraints/foreignKeys")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .map(|fk| {
            let rt = fk.get("referencedTable").cloned().unwrap_or_default();
            let refs = fk.get("columnReferences").and_then(Json::as_array).cloned().unwrap_or_default();
            let (p, d) = (s(&rt, "projectId"), s(&rt, "datasetId"));
            ForeignKeyDef {
                name: fk.get("name").and_then(Json::as_str).map(str::to_string),
                columns: refs.iter().map(|c| s(c, "referencingColumn")).collect(),
                ref_schema: Some(if p.is_empty() || p == project { d } else { format!("{p}.{d}") }),
                ref_table: s(&rt, "tableId"),
                ref_columns: refs.iter().map(|c| s(c, "referencedColumn")).collect(),
                on_delete: None,
                on_update: None,
            }
        })
        .collect()
}

pub(crate) async fn report(s: &mut BigQuerySession, table: &ObjectRef) -> Result<IndexUsageReport> {
    let ds = s.dataset(table)?;
    let project = s.api.project.clone();
    let meta = s.api.get(&["datasets", &ds, "tables", &table.name], &[]).await?;
    let mut r = IndexUsageReport { foreign_keys: foreign_keys(&meta, &project), seek_scan_split: false, writes_counted: false, ..Default::default() };
    // Emulators don't have these views.
    for kind in [SEARCH, VECTOR] {
        let view = if kind == SEARCH { "SEARCH_INDEXES" } else { "VECTOR_INDEXES" };
        match s.named_rows(&indexes_sql(&project, &ds, view, &table.name)).await {
            Ok(rows) => r.indexes.extend(indexes(kind, &rows)),
            Err(e) => tracing::debug!("bigquery: {view} not read: {e}"),
        }
    }
    if r.indexes.is_empty() {
        r.note = Some("BigQuery solo tiene índices de búsqueda y vectoriales, y esta tabla no tiene ninguno. Las claves primaria y foráneas no son índices (BigQuery no las aplica).".into());
        return Ok(r);
    }
    if s.api.emulator {
        r.note = Some("El emulador no tiene INFORMATION_SCHEMA.JOBS: se listan los índices sin contadores.".into());
        return Ok(r);
    }
    let loc = s.api.location.clone();
    let mut notes = Vec::new();
    let mut usage = None;
    for (view, precise) in [("JOBS", true), ("JOBS", false), ("JOBS_BY_USER", true), ("JOBS_BY_USER", false)] {
        match s.named_rows(&usage_sql(loc.as_deref(), view, &project, &ds, &table.name, precise)).await {
            Ok(rows) => {
                if view == "JOBS_BY_USER" {
                    notes.push("Solo cuentan tus consultas: para ver las de todos hace falta el permiso bigquery.jobs.listAll en el proyecto.".to_string());
                }
                usage = Some(rows.into_iter().next().unwrap_or_default());
                break;
            }
            Err(e) => tracing::debug!("bigquery: INFORMATION_SCHEMA.{view} (precise: {precise}) not read: {e}"),
        }
    }
    let Some(usage) = usage else {
        r.note = Some("No se pudo leer INFORMATION_SCHEMA.JOBS (hace falta el permiso bigquery.jobs.listAll, o bigquery.jobs.list para las consultas propias): se listan los índices sin contadores.".into());
        return Ok(r);
    };
    r.stats_available = true;
    r.since = usage.get("since").cloned();
    if !apply(&mut r.indexes, &usage) {
        notes.push("Con más de un índice vectorial en la tabla, BigQuery no dice cuál usó cada consulta: esos índices quedan sin contadores.".into());
    }
    notes.insert(0, "Lecturas = consultas de los últimos 180 días que usaron el índice (INFORMATION_SCHEMA.JOBS). BigQuery registra el uso por consulta, no por tabla: una consulta que une esta tabla con otra y usa solo el índice de la otra puede contar acá, así que el número es un máximo. BigQuery no cuenta escrituras. Leer JOBS es una consulta facturada (180 días de trabajos de la región) y se repite cada vez que se abre esta carpeta.".into());
    r.note = Some(notes.join(" "));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(kv: &[(&str, &str)]) -> Row {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn queries() {
        let q = indexes_sql("p", "d", "SEARCH_INDEXES", "o'k");
        assert!(q.contains("FROM `p`.`d`.INFORMATION_SCHEMA.SEARCH_INDEXES WHERE table_name = 'o\\'k'"), "{q}");
        let q = usage_sql(Some("EU"), "JOBS", "p", "d", "t", false);
        assert!(q.contains("FROM `region-eu`.INFORMATION_SCHEMA.JOBS AS j, UNNEST(j.referenced_tables) AS rt"), "{q}");
        assert!(q.contains("rt.project_id = 'p' AND rt.dataset_id = 'd' AND rt.table_id = 't'"));
        assert!(q.contains("j.search_statistics.index_usage_mode IN ('FULLY_USED', 'PARTIALLY_USED')"));
        assert!(q.contains("j.vector_search_statistics.index_usage_mode"));
        assert!(!q.contains("index_unused_reasons"));
        let q = usage_sql(None, "JOBS", "p", "d", "t", true);
        assert!(q.contains("j.search_statistics.index_usage_mode = 'FULLY_USED' OR (j.search_statistics.index_usage_mode = 'PARTIALLY_USED'"), "{q}");
        assert!(q.contains("NOT EXISTS (SELECT 1 FROM UNNEST(j.vector_search_statistics.index_unused_reasons) AS u WHERE u.base_table.project_id = 'p' AND u.base_table.dataset_id = 'd' AND u.base_table.table_id = 't')"), "{q}");
    }

    #[test]
    fn indexes_and_counters() {
        let search = indexes(
            SEARCH,
            &[row(&[
                ("index_name", "sx"),
                ("ddl", "CREATE SEARCH INDEX sx ON `p.d.t`(title, body) STORING (id) OPTIONS (analyzer = 'LOG_ANALYZER')"),
                ("index_status", "ACTIVE"),
                ("total_storage_bytes", "2048"),
                ("last_refresh_time", "2026-09-30 10:00:00"),
            ])],
        );
        let vector = indexes(VECTOR, &[row(&[("index_name", "vx"), ("ddl", "CREATE VECTOR INDEX vx ON `p.d.t`(emb) OPTIONS (index_type = 'IVF')"), ("index_status", "PENDING DISABLEMENT")])]);
        let mut ixs = [search, vector].concat();
        assert_eq!((ixs[0].kind.as_str(), ixs[0].key_columns.clone(), ixs[0].included_columns.clone()), ("SEARCH INDEX", vec!["title".to_string(), "body".to_string()], vec!["id".to_string()]));
        assert_eq!((ixs[0].size_kb, ixs[0].last_write.as_deref()), (Some(2), Some("2026-09-30 10:00:00")));
        assert_eq!(ixs[1].kind, "VECTOR INDEX (PENDING DISABLEMENT)");
        let usage = row(&[("search_used", "5"), ("search_last", "2026-10-01 09:00:00"), ("vector_used", "0"), ("since", "2026-04-04 00:00:00")]);
        assert!(apply(&mut ixs, &usage));
        let r = IndexUsageReport { stats_available: true, seek_scan_split: false, writes_counted: false, indexes: ixs.clone(), ..Default::default() }.derived();
        assert_eq!((r.indexes[0].seeks, r.indexes[0].read_share, r.indexes[0].seek_health, r.indexes[0].writes_per_read), (5, Some(1.0), None, None));
        assert_eq!((r.indexes[1].seeks, r.indexes[1].read_share, r.indexes[1].unused), (0, Some(0.0), false));
        // Two vector indexes: the job doesn't say which one.
        ixs.push(ixs[1].clone());
        let usage = row(&[("vector_used", "3")]);
        assert!(!apply(&mut ixs, &usage));
        assert!(ixs[1].seeks == 0 && ixs[2].seeks == 0);
    }

    #[test]
    fn foreign_keys_from_the_table() {
        let t = json!({"tableConstraints": {"primaryKey": {"columns": ["id"]}, "foreignKeys": [
            {"name": "fk_c", "referencedTable": {"projectId": "p", "datasetId": "d", "tableId": "c"},
             "columnReferences": [{"referencingColumn": "c1", "referencedColumn": "id1"}, {"referencingColumn": "c2", "referencedColumn": "id2"}]},
            {"referencedTable": {"projectId": "other", "datasetId": "x", "tableId": "y"}, "columnReferences": [{"referencingColumn": "y_id", "referencedColumn": "id"}]}
        ]}});
        let fks = foreign_keys(&t, "p");
        assert_eq!(fks.len(), 2);
        assert_eq!((fks[0].name.as_deref(), fks[0].columns.clone(), fks[0].ref_columns.clone()), (Some("fk_c"), vec!["c1".to_string(), "c2".to_string()], vec!["id1".to_string(), "id2".to_string()]));
        assert_eq!((fks[0].ref_schema.as_deref(), fks[0].ref_table.as_str()), (Some("d"), "c"));
        assert_eq!((fks[1].name.clone(), fks[1].ref_schema.as_deref()), (None, Some("other.x")));
        assert!(foreign_keys(&json!({}), "p").is_empty());
    }
}

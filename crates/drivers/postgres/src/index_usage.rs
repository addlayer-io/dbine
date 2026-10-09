//! A table's indexes and how they're used (`Session::index_usage`).
//!
//! PostgreSQL and the variants that keep its catalog:
//!
//! - The indexes (primary key included) and their columns come from
//!   `pg_index`, one row per column: keys in key order (descending ones from
//!   `indoption`), then the INCLUDE ones; the filter is `indpred`; the kind
//!   is the access method (`BTREE`, `GIN`, `BRIN`…).
//! - PostgreSQL counts index scans but doesn't tell a targeted lookup from a
//!   range or full scan: `pg_stat_all_indexes.idx_scan` goes into `seeks`
//!   and the report says `seek_scan_split: false` (neutral color, no seek
//!   ratio). `last_idx_scan` (PostgreSQL 16+) is the last read.
//! - It doesn't count writes per index either. Every inserted row and every
//!   update that isn't HOT writes to each of the table's indexes, so
//!   `updates` = `n_tup_ins + n_tup_upd - n_tup_hot_upd` of the table, the
//!   same for all its indexes (deletes don't touch them until VACUUM).
//!   A partial index (`WHERE …`) only gets the rows its filter keeps, and
//!   nothing says how many: it shows no writes (`updates` 0), so it's never
//!   judged unused, and the note says so.
//! - The table's sequential scans (`seq_scan`) aren't any index's reads;
//!   when there are, the note says how many.
//! - Partitioned tables: the parent's indexes have no counters of their own;
//!   each one adds up its partitions' (`pg_partition_tree`, PostgreSQL 12+).
//!   TimescaleDB hypertables do the same with their chunks' indexes
//!   (the chunks inherit the hypertable; a chunk index pairs with the
//!   hypertable's by definition).
//! - Size: `pg_relation_size` of the index (and its partitions or chunks).
//! - Since when: `stats_reset` of `pg_stat_database` (`None` when they were
//!   never reset: the counters run since the database was created).
//! - Greenplum and its forks count on each segment: the counters come from
//!   `gp_stat_all_indexes_summary` / `gp_stat_all_tables_summary`
//!   (Greenplum 7) or from `pg_stat_*`, which Cloudberry already adds up
//!   over the segments. Greenplum 6 only has the coordinator's, which never
//!   scans: the indexes are listed without counters.
//! - YugabyteDB keeps the counters of the node the session is connected to,
//!   and counts no table writes nor index sizes (DocDB stores them):
//!   `writes_counted` false. The same when the table's counters can't be
//!   read.
//!
//! CockroachDB: `crdb_internal.index_usage_statistics` (`total_reads`,
//! `last_read`, cluster-wide) by `crdb_internal.table_indexes` (which needs
//! `allow_unsafe_internals` since v25, set for the read only). No writes
//! per index (`writes_counted` false), no size, no reset time. If the server
//! refuses the read, the indexes are listed without counters. Cockroach's
//! `prefix` access method (26.x) is its ordinary ordered index: `BTREE`;
//! `inverted` is `GIN`.
//!
//! Disabling an index ("Deshabilitar índice"): only CockroachDB (22.2+)
//! has a supported way, `ALTER INDEX t@ix NOT VISIBLE` (the optimizer stops
//! using it; it's still maintained and still enforces uniqueness). Its state
//! comes from `information_schema.statistics.is_visible`, which needs no
//! `allow_unsafe_internals`. PostgreSQL and the rest of the family have
//! none: flipping `pg_index.indisvalid` by hand isn't something to offer.
//!
//! The engines without `pg_index` usage counters list their indexes (and
//! foreign keys) with `stats_available: false` and a note: Materialize,
//! RisingWave, CrateDB, H2 (from `information_schema`), Redshift (no
//! indexes: sort and distribution keys). Denodo has no indexes at all.

use crate::catalog::{cell, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{Error, ForeignKeyDef, IndexUsage, IndexUsageReport, ObjectRef, Result, SyncScript, TableSchema};
use std::collections::HashMap;
use tokio_postgres::SimpleQueryRow;

/// Every variant but Denodo, which has no indexes.
pub(crate) fn supported(v: Variant) -> bool {
    v != Variant::Denodo
}

/// Variants read through `pg_index` (the rest come from the structure read).
fn pg_path(v: Variant) -> bool {
    v.has_pg_catalog() && !matches!(v, Variant::CrateDb | Variant::RisingWave)
}

/// The engine has usage counters to read.
fn has_counters(v: Variant) -> bool {
    pg_path(v) && !matches!(v, Variant::Materialize | Variant::Yellowbrick)
}

/// The table's oid, as a subquery (`pg_table_is_visible` without a schema).
pub(crate) fn rel(v: Variant, schema: Option<&str>, name: &str) -> String {
    let schema_match = match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("n.nspname = {}", lit(v, s)),
        None => "pg_table_is_visible(c.oid)".to_string(),
    };
    format!(
        "(SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname = {} AND {schema_match} LIMIT 1)",
        lit(v, name)
    )
}

/// One row per index column: keys, then INCLUDE; the primary key first.
pub(crate) fn indexes_sql(v: Variant, version: i32, rel: &str) -> String {
    // openGauss reports 9.2 but has INCLUDE; Cockroach has STORING.
    let key_atts = if version >= 110000 || matches!(v, Variant::Cockroach | Variant::OpenGauss) {
        "COALESCE(x.indnkeyatts, x.indnatts)"
    } else {
        "x.indnatts"
    };
    format!(
        "SELECT i.relname AS idx, am.amname AS am, x.indisunique AS uniq, x.indisprimary AS pk,
                pg_get_expr(x.indpred, x.indrelid) AS pred, a.attname AS att,
                pg_get_indexdef(x.indexrelid, k.ord::int, true) AS expr, k.ord > {key_atts} AS inc,
                (x.indoption[k.ord - 1]::int & 1) = 1 AS dsc
         FROM pg_index x
         JOIN pg_class i ON i.oid = x.indexrelid
         LEFT JOIN pg_am am ON am.oid = i.relam
         CROSS JOIN LATERAL generate_series(1, x.indnatts::int) AS k(ord)
         LEFT JOIN pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = x.indkey[k.ord - 1]
         WHERE x.indrelid = {rel}
         ORDER BY x.indisprimary DESC, i.relname, k.ord"
    )
}

/// The table's foreign keys, one row per column pair.
pub(crate) fn foreign_keys_sql(rel: &str) -> String {
    format!(
        "SELECT con.conname AS con, a.attname AS col, rn.nspname AS rsch, rc.relname AS rtbl, ra.attname AS rcol,
                con.confdeltype::text AS del, con.confupdtype::text AS upd
         FROM pg_constraint con
         CROSS JOIN LATERAL unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
         JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum
         JOIN pg_class rc ON rc.oid = con.confrelid
         JOIN pg_namespace rn ON rn.oid = rc.relnamespace
         LEFT JOIN pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = con.confkey[k.ord::int]
         WHERE con.contype = 'f' AND con.conrelid = {rel}
         ORDER BY con.conname, k.ord"
    )
}

/// Where the counters are, the first that answers wins: (indexes, tables).
/// Greenplum and its forks count on each segment: the summary views of
/// Greenplum 7, or `pg_stat_*` where they already add up the segments
/// (Cloudberry). Greenplum 6 has only the coordinator's: none.
pub(crate) fn stat_views(v: Variant, version: i32) -> &'static [(&'static str, &'static str)] {
    if !v.mpp() {
        return &[("pg_stat_all_indexes", "pg_stat_all_tables")];
    }
    if version < 120000 {
        return &[];
    }
    &[
        ("gp_stat_all_indexes_summary", "gp_stat_all_tables_summary"),
        ("gp_toolkit.gp_stat_all_indexes_summary", "gp_toolkit.gp_stat_all_tables_summary"),
        ("pg_stat_all_indexes", "pg_stat_all_tables"),
    ]
}

/// Relations whose counters add up into `of` (an index, `x.indexrelid` of
/// table `rel`, or the table itself): itself, its partitions, TimescaleDB
/// chunks. A chunk's copy of an index is the chunk's index with the same
/// definition (`USING …` onwards).
fn members(v: Variant, version: i32, of: &str, rel: &str, index: bool) -> String {
    let mut parts = vec![format!("SELECT {of} AS m")];
    if version >= 120000 && v != Variant::Cockroach {
        parts.push(format!("SELECT relid FROM pg_partition_tree({of})"));
    }
    if v == Variant::Timescale {
        parts.push(if index {
            format!(
                "SELECT cx.indexrelid FROM pg_inherits ch JOIN pg_index cx ON cx.indrelid = ch.inhrelid
                 WHERE ch.inhparent = {rel}
                   AND substring(pg_get_indexdef(cx.indexrelid) FROM ' USING .*$') = substring(pg_get_indexdef({of}) FROM ' USING .*$')"
            )
        } else {
            format!("SELECT ch.inhrelid FROM pg_inherits ch WHERE ch.inhparent = {rel}")
        });
    }
    parts.join(" UNION ")
}

/// Per index: scans, last scan, bytes (its partitions and chunks added up).
pub(crate) fn index_stats_sql(v: Variant, version: i32, rel: &str, ix_view: &str) -> String {
    let last = if version >= 160000 && !v.mpp() { "max(s.last_idx_scan)::text" } else { "NULL::text" };
    let members = members(v, version, "x.indexrelid", rel, true);
    format!(
        "SELECT i.relname AS idx, COALESCE(sum(s.idx_scan), 0)::text AS scans, {last} AS last
         FROM pg_index x
         JOIN pg_class i ON i.oid = x.indexrelid
         CROSS JOIN LATERAL ({members}) AS mm
         LEFT JOIN {ix_view} s ON s.indexrelid = mm.m
         WHERE x.indrelid = {rel}
         GROUP BY i.relname"
    )
}

/// Per index: its size in bytes (partitions and chunks added up).
pub(crate) fn size_sql(v: Variant, version: i32, rel: &str) -> String {
    let members = members(v, version, "x.indexrelid", rel, true);
    format!(
        "SELECT i.relname AS idx, COALESCE(sum(pg_relation_size(mm.m)), 0)::text AS bytes
         FROM pg_index x
         JOIN pg_class i ON i.oid = x.indexrelid
         CROSS JOIN LATERAL ({members}) AS mm
         WHERE x.indrelid = {rel} AND mm.m IS NOT NULL
         GROUP BY i.relname"
    )
}

/// The table's index writes (inserts + non-HOT updates) and sequential scans.
pub(crate) fn table_stats_sql(v: Variant, version: i32, rel: &str, tbl_view: &str) -> String {
    let members = members(v, version, rel, rel, false);
    format!(
        "SELECT COALESCE(sum(s.n_tup_ins + s.n_tup_upd - s.n_tup_hot_upd), 0)::text AS writes,
                COALESCE(sum(s.seq_scan), 0)::text AS seq
         FROM {tbl_view} s WHERE s.relid IN (SELECT m FROM ({members}) AS mm)"
    )
}

pub(crate) const SINCE_SQL: &str = "SELECT stats_reset::text AS since FROM pg_stat_database WHERE datname = current_database()";

/// CockroachDB: reads per index, cluster-wide.
pub(crate) fn cockroach_sql(rel: &str) -> String {
    format!(
        "SELECT ti.index_name AS idx, COALESCE(u.total_reads, 0)::STRING AS scans, u.last_read::STRING AS last
         FROM crdb_internal.table_indexes ti
         LEFT JOIN crdb_internal.index_usage_statistics u ON u.table_id = ti.descriptor_id AND u.index_id = ti.index_id
         WHERE ti.descriptor_id = {rel}::INT8"
    )
}

/// CockroachDB: the table's invisible indexes ("deshabilitados").
pub(crate) fn cockroach_invisible_sql(v: Variant, schema: Option<&str>, name: &str) -> String {
    let schema = match schema.filter(|s| !s.is_empty()) {
        Some(s) => lit(v, s),
        None => "current_schema()".into(),
    };
    format!(
        "SELECT DISTINCT index_name AS idx FROM information_schema.statistics
         WHERE table_schema = {schema} AND table_name = {} AND is_visible = 'NO'",
        lit(v, name)
    )
}

/// Whether the variant can disable an index: CockroachDB only.
pub(crate) fn toggle_supported(v: Variant) -> bool {
    v == Variant::Cockroach
}

/// "Deshabilitar / Habilitar índice" on CockroachDB: `NOT VISIBLE` /
/// `VISIBLE`. The primary index can't be invisible.
pub(crate) fn toggle_script(v: Variant, table: &ObjectRef, index: &IndexUsage, enable: bool) -> Result<SyncScript> {
    if !toggle_supported(v) {
        return Err(Error::Unsupported("este motor no deshabilita índices".into()));
    }
    if index.primary_key {
        return Err(Error::Unsupported("CockroachDB no permite deshabilitar (ocultar) la clave primaria.".into()));
    }
    let owner = qualified_name(Quote::Double, table.schema(), &table.name);
    let mut warnings = Vec::new();
    if !enable {
        warnings.push("El índice se sigue manteniendo en cada escritura; el optimizador deja de usarlo.".to_string());
        if index.unique {
            warnings.push("Al ser único, sigue impidiendo valores repetidos aunque esté deshabilitado.".to_string());
        }
    }
    let state = if enable { "VISIBLE" } else { "NOT VISIBLE" };
    Ok(SyncScript { statements: vec![format!("ALTER INDEX {owner}@{} {state}", quote_ident(Quote::Double, &index.name))], warnings })
}

/// One `indexes_sql` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ColumnRow {
    pub index: String,
    pub am: Option<String>,
    pub unique: bool,
    pub primary_key: bool,
    pub filter: Option<String>,
    pub att: Option<String>,
    pub expr: String,
    pub included: bool,
    pub descending: bool,
}

/// One index's counters.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct UsageRow {
    pub scans: u64,
    pub last_read: Option<String>,
}

/// A column as the index names it: the column, or `(expression)`.
fn column_name(att: Option<&str>, expr: &str) -> String {
    match att {
        Some(a) if a == expr || quote_ident(Quote::Double, a) == expr => a.to_string(),
        // CockroachDB already wraps expressions.
        _ if expr.starts_with("((") && expr.ends_with("))") => expr[1..expr.len() - 1].to_string(),
        _ if expr.starts_with('(') && expr.ends_with(')') => expr.to_string(),
        _ => format!("({expr})"),
    }
}

/// `2026-10-01 10:00:00.123+00` → `2026-10-01 10:00:00`.
pub(crate) fn timestamp(s: Option<String>) -> Option<String> {
    s.filter(|s| s.len() >= 19).map(|s| s[..19].to_string())
}

/// The engine's access method as the report names it. CockroachDB 26.x
/// calls its ordinary ordered index `prefix` (a B-tree) and its `inverted`
/// index is a GIN: named as the explorer and PostgreSQL name them.
fn kind(am: Option<&str>) -> String {
    match am.filter(|a| !a.is_empty()) {
        Some(a) => crate::compare::crdb_index_method(a).to_ascii_uppercase(),
        None => "INDEX".into(),
    }
}

/// The rows put together, in `columns`' order. `usage` `None`: no counters.
/// `writes` is the table's (every index pays it), except a partial index's:
/// it only gets the rows its filter keeps, unknown, so it shows none and is
/// never judged unused.
pub(crate) fn assemble(columns: &[ColumnRow], usage: Option<&HashMap<String, UsageRow>>, sizes: Option<&HashMap<String, u64>>, writes: u64) -> Vec<IndexUsage> {
    let mut out: Vec<IndexUsage> = Vec::new();
    for c in columns {
        if out.last().is_none_or(|i| i.name != c.index) {
            let u = usage.and_then(|m| m.get(&c.index)).cloned().unwrap_or_default();
            out.push(IndexUsage {
                name: c.index.clone(),
                kind: kind(c.am.as_deref()),
                unique: c.unique || c.primary_key,
                primary_key: c.primary_key,
                filter: c.filter.clone(),
                size_kb: sizes.and_then(|m| m.get(&c.index)).map(|b| b.div_ceil(1024)),
                seeks: u.scans,
                updates: if usage.is_some() && c.filter.is_none() { writes } else { 0 },
                last_read: u.last_read,
                ..Default::default()
            });
        }
        let ix = out.last_mut().expect("pushed above");
        if c.included {
            ix.included_columns.push(c.att.clone().unwrap_or_else(|| c.expr.clone()));
        } else {
            let name = column_name(c.att.as_deref(), &c.expr);
            ix.key_columns.push(if c.descending { format!("{name} DESC") } else { name });
        }
    }
    out
}

/// The indexes and keys of the structure read (engines without `pg_index`).
pub(crate) fn from_structure(v: Variant, t: &TableSchema) -> Vec<IndexUsage> {
    let mut out = Vec::new();
    // Redshift's primary key is informational: there's no index behind it.
    if let Some(pk) = t.primary_key.as_ref().filter(|_| v != Variant::Redshift) {
        out.push(IndexUsage {
            name: pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into()),
            kind: "PRIMARY KEY".into(),
            unique: true,
            primary_key: true,
            key_columns: pk.columns.clone(),
            ..Default::default()
        });
    }
    for ix in &t.indexes {
        out.push(IndexUsage {
            name: ix.name.clone(),
            kind: ix.kind.clone().unwrap_or_else(|| "INDEX".into()).to_ascii_uppercase(),
            unique: ix.unique,
            key_columns: ix.columns.clone(),
            included_columns: ix.include.clone(),
            filter: ix.filter.clone(),
            ..Default::default()
        });
    }
    out
}

/// Why there are no counters, by engine.
pub(crate) fn no_counters_note(v: Variant) -> &'static str {
    match v {
        Variant::Redshift => "Redshift no tiene índices: ordena y reparte las filas con SORTKEY y DISTKEY. Se muestran las claves foráneas.",
        Variant::Materialize => "Materialize no cuenta cuántas veces se usa cada índice. Se listan los índices sin estadísticas de uso.",
        Variant::RisingWave => "RisingWave no cuenta cuántas veces se usa cada índice. Se listan los índices sin estadísticas de uso.",
        Variant::CrateDb => "CrateDB indexa cada columna por su cuenta y no cuenta el uso de los índices. Se listan la clave primaria y los índices de texto completo, sin estadísticas de uso.",
        Variant::H2 => "H2 no cuenta cuántas veces se usa cada índice. Se listan los índices sin estadísticas de uso.",
        Variant::Yellowbrick => "Yellowbrick no tiene índices secundarios ni cuenta su uso.",
        _ => "El servidor no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.",
    }
}

/// Why the counters couldn't be read (the server's message goes to the log).
pub(crate) fn refused_note(v: Variant) -> &'static str {
    match v {
        Variant::Cockroach => "El servidor no permitió leer las estadísticas de uso de CockroachDB (crdb_internal.index_usage_statistics). Se listan los índices sin estadísticas de uso.",
        Variant::Greenplum | Variant::Cloudberry | Variant::Greengage => "Este servidor solo tiene las estadísticas del coordinador, que no cuentan las lecturas de los segmentos (las suman Greenplum 7 y Cloudberry). Se listan los índices sin estadísticas de uso.",
        _ => "No se pudieron leer las estadísticas de uso (pg_stat_all_indexes): el usuario necesita poder leerlas, por ejemplo con el rol pg_read_all_stats. Se listan los índices sin estadísticas de uso.",
    }
}

/// What the note adds about the table's full scans.
pub(crate) fn seq_scan_note(seq: u64) -> Option<String> {
    (seq > 0).then(|| {
        format!(
            "Además, la tabla se recorrió completa {seq} {} sin usar ningún índice (lecturas secuenciales).",
            if seq == 1 { "vez" } else { "veces" }
        )
    })
}

/// The YugabyteDB counters are the node's, not the cluster's.
const YUGABYTE_NOTE: &str = "YugabyteDB cuenta el uso en cada nodo: los números son los del nodo al que está conectada la sesión. No cuenta las escrituras: un índice sin lecturas muestra 0 % en vez de «sin uso».";

/// `stats_reset` is empty: the counters were never reset in this database.
const NEVER_RESET_NOTE: &str = "Las estadísticas de esta base nunca se reiniciaron (pg_stat_reset): los contadores corren desde que se creó, o desde la última caída del servidor.";

/// Partial indexes show no writes: the table's aren't all theirs.
const PARTIAL_NOTE: &str = "Los índices parciales (con filtro WHERE) no muestran escrituras: PostgreSQL cuenta las de la tabla, no cuántas filas cumplen el filtro de cada índice, así que nunca se marcan «sin uso».";

/// The CockroachDB note: reads only.
const COCKROACH_NOTE: &str = "CockroachDB cuenta las lecturas de cada índice pero no las escrituras: un índice sin lecturas muestra 0 % en vez de «sin uso».";

fn flag(r: &SimpleQueryRow, name: &str) -> bool {
    cell(r, name).is_some_and(|v| v == "t" || v == "true")
}

fn num(r: &SimpleQueryRow, name: &str) -> u64 {
    cell(r, name).and_then(|s| s.split('.').next().and_then(|n| n.parse::<i64>().ok())).unwrap_or(0).max(0) as u64
}

/// Foreign key rows grouped by constraint.
fn group_foreign_keys(rows: &[SimpleQueryRow]) -> Vec<ForeignKeyDef> {
    let mut out: Vec<ForeignKeyDef> = Vec::new();
    for r in rows {
        let name = cell(r, "con");
        let col = cell(r, "col").unwrap_or_default();
        let rcol = cell(r, "rcol").unwrap_or_default();
        match out.last_mut() {
            Some(fk) if fk.name == name => {
                fk.columns.push(col);
                fk.ref_columns.push(rcol);
            }
            _ => out.push(ForeignKeyDef {
                name,
                columns: vec![col],
                ref_schema: cell(r, "rsch"),
                ref_table: cell(r, "rtbl").unwrap_or_default(),
                ref_columns: vec![rcol],
                on_delete: crate::structure::fk_action(cell(r, "del").as_deref()),
                on_update: crate::structure::fk_action(cell(r, "upd").as_deref()),
            }),
        }
    }
    out
}

impl PgSession {
    /// A query that may fail (missing view, privilege): `None` and a log line.
    async fn optional(&self, what: &str, sql: &str) -> Option<Vec<SimpleQueryRow>> {
        match self.text(sql).await {
            Ok(rows) => Some(rows),
            Err(e) => {
                tracing::debug!("{:?}: index usage, {what} unavailable: {e}", self.variant);
                None
            }
        }
    }

    pub(crate) async fn index_usage_report(&mut self, table: &ObjectRef) -> Result<IndexUsageReport> {
        let v = self.variant;
        if !pg_path(v) {
            return self.structure_report(table).await;
        }
        let schema = table.schema();
        let rel = rel(v, schema, &table.name);
        let columns: Vec<ColumnRow> = self
            .text(&indexes_sql(v, self.version, &rel))
            .await?
            .iter()
            .map(|r| ColumnRow {
                index: cell(r, "idx").unwrap_or_default(),
                am: cell(r, "am"),
                unique: flag(r, "uniq"),
                primary_key: flag(r, "pk"),
                filter: cell(r, "pred"),
                att: cell(r, "att"),
                expr: cell(r, "expr").unwrap_or_default(),
                included: flag(r, "inc"),
                descending: flag(r, "dsc"),
            })
            .collect();
        // Materialize has no foreign keys (nor pg_constraint rows).
        let foreign_keys = match self.optional("foreign keys", &foreign_keys_sql(&rel)).await {
            Some(rows) => group_foreign_keys(&rows),
            None => Vec::new(),
        };
        let mut report = IndexUsageReport { foreign_keys, seek_scan_split: false, writes_counted: false, ..Default::default() };
        let mut notes: Vec<String> = Vec::new();
        if !has_counters(v) {
            report.indexes = assemble(&columns, None, None, 0);
            report.note = Some(no_counters_note(v).into());
            return Ok(report);
        }

        let mut table_view = "pg_stat_all_tables";
        let usage_rows = if v == Variant::Cockroach {
            // Since v25 crdb_internal is closed unless the session opts in.
            let _ = self.client.batch_execute("SET allow_unsafe_internals = true").await;
            let rows = self.optional("counters", &cockroach_sql(&rel)).await;
            let _ = self.client.batch_execute("RESET allow_unsafe_internals").await;
            rows
        } else {
            let mut found = None;
            for &(ix_view, tbl_view) in stat_views(v, self.version) {
                if let Some(rows) = self.optional("counters", &index_stats_sql(v, self.version, &rel, ix_view)).await {
                    found = Some((rows, tbl_view));
                    break;
                }
            }
            found.map(|(rows, tbl_view)| {
                table_view = tbl_view;
                rows
            })
        };
        let usage: Option<HashMap<String, UsageRow>> = usage_rows.map(|rows| {
            rows.iter()
                .map(|r| (cell(r, "idx").unwrap_or_default(), UsageRow { scans: num(r, "scans"), last_read: timestamp(cell(r, "last")) }))
                .collect()
        });
        let (mut writes, mut sizes) = (0, None);
        // The writes come from the table's counters: unknown until read.
        let mut writes_counted = false;
        if usage.is_none() {
            notes.push(refused_note(v).into());
        } else if v == Variant::Cockroach {
            notes.push(COCKROACH_NOTE.into());
        } else {
            // YugabyteDB counts neither the table's writes nor its scans.
            if v == Variant::Yugabyte {
                notes.push(YUGABYTE_NOTE.into());
            } else if let Some(rows) = self.optional("table counters", &table_stats_sql(v, self.version, &rel, table_view)).await {
                if let Some(r) = rows.first() {
                    writes = num(r, "writes");
                    writes_counted = true;
                    if columns.iter().any(|c| c.filter.is_some()) {
                        notes.push(PARTIAL_NOTE.into());
                    }
                    notes.extend(seq_scan_note(num(r, "seq")));
                }
            }
            report.since = self.optional("stats reset", SINCE_SQL).await.and_then(|rows| timestamp(rows.first().and_then(|r| cell(r, "since"))));
            // YugabyteDB keeps them in memory: a restart clears them.
            if report.since.is_none() && v != Variant::Yugabyte {
                notes.push(NEVER_RESET_NOTE.into());
            }
        }
        // Neither reports an index's size through pg_relation_size.
        if !matches!(v, Variant::Cockroach | Variant::Yugabyte) {
            sizes = self
                .optional("sizes", &size_sql(v, self.version, &rel))
                .await
                .map(|rows| rows.iter().map(|r| (cell(r, "idx").unwrap_or_default(), num(r, "bytes"))).collect::<HashMap<_, _>>());
        }
        report.stats_available = usage.is_some();
        report.writes_counted = writes_counted;
        report.indexes = assemble(&columns, usage.as_ref(), sizes.as_ref(), writes);
        if toggle_supported(v) {
            if let Some(rows) = self.optional("visibility", &cockroach_invisible_sql(v, schema, &table.name)).await {
                let off: Vec<String> = rows.iter().filter_map(|r| cell(r, "idx")).collect();
                for ix in &mut report.indexes {
                    ix.disabled = off.contains(&ix.name);
                }
            }
        }
        report.note = (!notes.is_empty()).then(|| notes.join(" "));
        Ok(report)
    }

    /// Engines read through `information_schema`: the table's structure.
    async fn structure_report(&mut self, table: &ObjectRef) -> Result<IndexUsageReport> {
        let v = self.variant;
        let tables = self.schema_of_database().await?;
        let t = tables.iter().find(|t| t.name == table.name && (table.schema().is_none() || t.schema.as_deref() == table.schema()));
        Ok(IndexUsageReport {
            indexes: t.map(|t| from_structure(v, t)).unwrap_or_default(),
            foreign_keys: t.map(|t| t.foreign_keys.clone()).unwrap_or_default(),
            note: Some(no_counters_note(v).into()),
            seek_scan_split: false,
            writes_counted: false,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{IndexDef, KeyDef};

    fn col(index: &str, att: &str, included: bool, descending: bool) -> ColumnRow {
        ColumnRow { index: index.into(), am: Some("btree".into()), att: Some(att.into()), expr: att.into(), included, descending, ..Default::default() }
    }

    #[test]
    fn cockroach_toggles_by_visibility() {
        let t = ObjectRef { kind: "table".into(), schema: Some("ven tas".into()), name: "Pedidos".into() };
        let ix = IndexUsage { name: "ix_fecha".into(), ..Default::default() };
        let off = toggle_script(Variant::Cockroach, &t, &ix, false).unwrap();
        assert_eq!(off.statements, [r#"ALTER INDEX "ven tas"."Pedidos"@"ix_fecha" NOT VISIBLE"#]);
        assert_eq!(off.warnings, ["El índice se sigue manteniendo en cada escritura; el optimizador deja de usarlo."]);
        let on = toggle_script(Variant::Cockroach, &t, &ix, true).unwrap();
        assert_eq!(on.statements, [r#"ALTER INDEX "ven tas"."Pedidos"@"ix_fecha" VISIBLE"#]);
        assert!(on.warnings.is_empty());
        let bare = ObjectRef { schema: None, ..t.clone() };
        assert_eq!(toggle_script(Variant::Cockroach, &bare, &ix, true).unwrap().statements, [r#"ALTER INDEX "Pedidos"@"ix_fecha" VISIBLE"#]);
        let uq = IndexUsage { unique: true, ..ix.clone() };
        assert_eq!(toggle_script(Variant::Cockroach, &t, &uq, false).unwrap().warnings.len(), 2);
        let pk = IndexUsage { name: "pedidos_pkey".into(), primary_key: true, unique: true, ..Default::default() };
        assert!(matches!(toggle_script(Variant::Cockroach, &t, &pk, false), Err(Error::Unsupported(m)) if m.contains("clave primaria")));
        for v in Variant::ALL.iter().filter(|&&v| v != Variant::Cockroach) {
            assert!(!toggle_supported(*v) && toggle_script(*v, &t, &ix, false).is_err(), "{v:?}");
        }
        let sql = cockroach_invisible_sql(Variant::Cockroach, Some("s"), "t");
        assert!(sql.contains("is_visible = 'NO'") && sql.contains("table_schema = E's'") && sql.contains("table_name = E't'"), "{sql}");
        assert!(cockroach_invisible_sql(Variant::Cockroach, None, "t").contains("current_schema()"));
    }

    #[test]
    fn the_table_is_found_by_schema_or_search_path() {
        let r = rel(Variant::Postgres, Some("ven'tas"), "t");
        assert!(r.contains("c.relname = E't'") && r.contains("n.nspname = E'ven''tas'"), "{r}");
        assert!(rel(Variant::Postgres, None, "t").contains("pg_table_is_visible(c.oid)"));
        assert!(rel(Variant::Postgres, Some(""), "t").contains("pg_table_is_visible(c.oid)"));
    }

    #[test]
    fn index_columns_by_version() {
        let r = rel(Variant::Postgres, Some("s"), "t");
        let new = indexes_sql(Variant::Postgres, 160000, &r);
        assert!(new.contains("COALESCE(x.indnkeyatts, x.indnatts)") && new.contains(&format!("x.indrelid = {r}")));
        assert!(new.contains("x.indoption[k.ord - 1]::int & 1"), "descending keys");
        assert!(!new.contains("NOT x.indisprimary"), "the primary key is listed too");
        assert!(indexes_sql(Variant::Greenplum, 90426, &r).contains("k.ord > x.indnatts"));
        assert!(indexes_sql(Variant::OpenGauss, 90204, &r).contains("indnkeyatts"));
        assert!(foreign_keys_sql(&r).contains("con.contype = 'f' AND con.conrelid = "));
    }

    #[test]
    fn counters_add_partitions_and_chunks() {
        let r = rel(Variant::Postgres, Some("s"), "t");
        let pg16 = index_stats_sql(Variant::Postgres, 160000, &r, "pg_stat_all_indexes");
        assert!(pg16.contains("pg_partition_tree(x.indexrelid)") && pg16.contains("max(s.last_idx_scan)"), "{pg16}");
        assert!(pg16.contains("LEFT JOIN pg_stat_all_indexes s"), "never-scanned indexes still have a row");
        let pg11 = index_stats_sql(Variant::Postgres, 110000, &r, "pg_stat_all_indexes");
        assert!(!pg11.contains("pg_partition_tree") && pg11.contains("NULL::text AS last"));
        let ts = index_stats_sql(Variant::Timescale, 160000, &r, "pg_stat_all_indexes");
        assert!(ts.contains("JOIN pg_index cx ON cx.indrelid = ch.inhrelid") && ts.contains("' USING .*$'"), "{ts}");
        let ts_table = table_stats_sql(Variant::Timescale, 160000, &r, "pg_stat_all_tables");
        assert!(ts_table.contains("SELECT ch.inhrelid FROM pg_inherits ch") && !ts_table.contains("pg_index cx"), "{ts_table}");
        assert!(ts_table.contains("n_tup_ins + s.n_tup_upd - s.n_tup_hot_upd") && ts_table.contains("seq_scan"));
        assert!(index_stats_sql(Variant::Greenplum, 160000, &r, "gp_stat_all_indexes_summary").contains("NULL::text AS last"));
        assert!(table_stats_sql(Variant::Cloudberry, 140000, &r, "x_tables").contains("FROM x_tables s"));
        assert_eq!(stat_views(Variant::Postgres, 90600), [("pg_stat_all_indexes", "pg_stat_all_tables")]);
        assert!(stat_views(Variant::Greenplum, 90426).is_empty(), "Greenplum 6: the coordinator's only");
        assert_eq!(stat_views(Variant::Cloudberry, 140000).last(), Some(&("pg_stat_all_indexes", "pg_stat_all_tables")));
        assert!(size_sql(Variant::Postgres, 160000, &r).contains("sum(pg_relation_size(mm.m))"));
        let crdb = cockroach_sql(&r);
        assert!(crdb.contains("LEFT JOIN crdb_internal.index_usage_statistics u") && crdb.contains("::INT8"));
        assert!(!index_stats_sql(Variant::Cockroach, 130000, &r, "pg_stat_all_indexes").contains("pg_partition_tree"));
    }

    #[test]
    fn indexes_get_their_columns_counters_and_size() {
        let mut pk = col("t_pkey", "id", false, false);
        pk.primary_key = true;
        let mut a = col("ix_a", "a", false, true);
        a.filter = Some("(a > 0)".into());
        let expr = ColumnRow { index: "ix_a".into(), att: None, expr: "lower(b)".into(), ..Default::default() };
        let columns = [pk, a, expr, col("ix_a", "c", true, false), col("ix_never", "b", false, false)];
        let usage = HashMap::from([
            ("t_pkey".to_string(), UsageRow { scans: 9, last_read: Some("2026-10-01 10:00:00".into()) }),
            ("ix_a".to_string(), UsageRow { scans: 4, last_read: None }),
            ("ix_never".to_string(), UsageRow::default()),
        ]);
        let sizes = HashMap::from([("t_pkey".to_string(), 16384u64), ("ix_a".to_string(), 8193)]);
        let out = assemble(&columns, Some(&usage), Some(&sizes), 7);
        assert_eq!(out.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["t_pkey", "ix_a", "ix_never"]);
        assert!(out[0].primary_key && out[0].unique && out[0].kind == "BTREE");
        assert_eq!((out[0].seeks, out[0].scans, out[0].updates, out[0].size_kb), (9, 0, 7, Some(16)));
        assert_eq!(out[1].key_columns, ["a DESC", "(lower(b))"]);
        assert_eq!(out[1].included_columns, ["c"]);
        assert_eq!((out[1].filter.as_deref(), out[1].size_kb), (Some("(a > 0)"), Some(9)));
        assert_eq!(out[1].updates, 0, "a partial index isn't charged the table's writes");
        assert_eq!((out[2].seeks, out[2].updates, out[2].size_kb), (0, 7, None));

        let r = IndexUsageReport { stats_available: true, seek_scan_split: false, indexes: out, ..Default::default() }.derived();
        assert!(r.indexes[2].unused, "written, never read");
        assert!(r.indexes.iter().all(|i| i.seek_health.is_none()), "no seek/scan split: neutral");
        assert_eq!(r.indexes[0].read_share.map(|s| (s * 100.0).round()), Some(69.0));

        // No counters: listed, all zeros, nothing derived.
        let bare = assemble(&columns, None, None, 7);
        assert!(bare.iter().all(|i| i.seeks == 0 && i.updates == 0 && i.size_kb.is_none()));
    }

    #[test]
    fn expressions_and_quoted_columns() {
        assert_eq!(column_name(Some("a"), "a"), "a");
        assert_eq!(column_name(Some("Mixed"), "\"Mixed\""), "Mixed");
        assert_eq!(column_name(None, "lower(a)"), "(lower(a))");
        assert_eq!(column_name(None, "((a + 1))"), "(a + 1)");
        assert_eq!(timestamp(Some("2026-10-01 10:00:00.123456+00".into())).as_deref(), Some("2026-10-01 10:00:00"));
        assert_eq!(timestamp(Some("".into())), None);
    }

    #[test]
    fn structure_engines_list_keys_and_indexes() {
        let t = TableSchema {
            name: "t".into(),
            primary_key: Some(KeyDef { name: Some("t_pk".into()), columns: vec!["id".into()] }),
            indexes: vec![IndexDef { name: "ix".into(), columns: vec!["a".into()], unique: true, ..Default::default() }],
            ..Default::default()
        };
        let out = from_structure(Variant::H2, &t);
        assert_eq!(out.iter().map(|i| (i.name.as_str(), i.primary_key, i.unique)).collect::<Vec<_>>(), [("t_pk", true, true), ("ix", false, true)]);
        assert_eq!(out[1].kind, "INDEX");
        // Redshift's keys aren't indexes.
        assert!(from_structure(Variant::Redshift, &t).iter().all(|i| !i.primary_key));
    }

    #[test]
    fn which_engines_have_what() {
        assert!(!supported(Variant::Denodo));
        assert!(Variant::ALL.iter().filter(|v| **v != Variant::Denodo).all(|v| supported(*v)));
        for v in [Variant::Postgres, Variant::Timescale, Variant::Yugabyte, Variant::Cockroach, Variant::Greenplum, Variant::OpenGauss, Variant::Aurora] {
            assert!(pg_path(v) && has_counters(v), "{v:?}");
        }
        for v in [Variant::Redshift, Variant::CrateDb, Variant::RisingWave, Variant::H2] {
            assert!(!pg_path(v), "{v:?}");
        }
        assert!(pg_path(Variant::Materialize) && !has_counters(Variant::Materialize));
        assert_eq!(seq_scan_note(0), None);
        assert!(seq_scan_note(1).unwrap().contains("1 vez"));
        assert!(seq_scan_note(3).unwrap().contains("3 veces"));
        assert!(refused_note(Variant::Cockroach).contains("crdb_internal.index_usage_statistics"));
        assert!(!refused_note(Variant::Cockroach).contains("VIEWACTIVITY"));
        assert_eq!(kind(Some("prefix")), "BTREE");
        assert_eq!(kind(Some("inverted")), "GIN");
        assert_eq!(kind(Some("btree")), "BTREE");
        assert_eq!(kind(Some("gin")), "GIN");
        assert_eq!(kind(None), "INDEX");
        assert!(refused_note(Variant::Postgres).contains("pg_read_all_stats"));
    }
}

//! A table's indexes and how they're used (`Session::index_usage`).
//!
//! - The indexes, the primary key and the foreign keys come from the same
//!   ODBC catalog calls as the schema compare, for that table only
//!   (`SQLPrimaryKeys`, `SQLForeignKeys`, `SQLStatistics`, plus Db2's
//!   INCLUDE columns), so an index has the same name here as in the drop
//!   script "Eliminar índice" generates.
//! - The counters, where the engine keeps them per index:
//!   - Db2 (LUW): `MON_GET_INDEX` (LEFT JOIN from `SYSCAT.INDEXES`, so an
//!     index never used since the database was activated shows zeros).
//!     `INDEX_SCANS` counts every access through the index without telling
//!     point lookups from range scans: it goes in `seeks` and the report
//!     says the engine doesn't split them (`seek_scan_split` false).
//!     Updates = `KEY_UPDATES` + `INCLUDE_COL_UPDATES`. The last read is
//!     `SYSCAT.INDEXES.LASTUSED` (a date). Since: the database's activation
//!     (`MON_GET_DATABASE.DB_CONN_TIME`). Without EXECUTE on the monitor
//!     functions the indexes are listed without counters, with a note.
//!   - Db2 for i: `QSYS2.SYSINDEXSTAT.QUERY_USE_COUNT` (queries that used
//!     the index; no write counter) and `LAST_QUERY_USE`.
//!   - Sybase ASE: `master..monOpenObjectActivity.UsedCount` (plans that used
//!     the index while its descriptor was open), writes = rows inserted +
//!     deleted + updated through it, `LastUsedDate`. Needs mon_role and the
//!     monitoring options; without them, no counters and a note.
//!   - Everyone else (Db2 for z/OS, Informix, Teradata, SQL Anywhere,
//!     Altibase, CUBRID, Dameng, IRIS…): no per-index counters reachable
//!     over SQL; the indexes are listed with a note.
//! - Vertica, Exasol and Netezza have no user indexes (projections, automatic
//!   indexes, zone maps): only the key and the foreign keys. Engines with
//!   neither indexes nor foreign keys (Hive, Impala, Spark…) don't offer it.

use crate::design::{self, Eng};
use crate::presets::Preset;
use dbine_driver::{IndexUsage, IndexUsageReport, TableSchema};

/// The "Índices" folder is offered: the engine has indexes or reports keys.
pub fn supported(p: &Preset) -> bool {
    design::has_indexes(p) || design::reports_foreign_keys(p)
}

/// One index's counters, by catalog name. `primary`: the primary key's
/// own index (its catalog name may differ from the constraint's).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageRow {
    pub name: String,
    pub primary: bool,
    pub seeks: u64,
    pub updates: u64,
    pub last_read: Option<String>,
}

/// The counter query and how its parameters are built from (schema,
/// table): Db2 takes them twice, ASE the qualified name.
pub fn counters_sql(e: Eng) -> Option<&'static str> {
    match e {
        Eng::Db2 => Some(
            "SELECT i.INDNAME, i.UNIQUERULE, COALESCE(SUM(m.INDEX_SCANS), 0),
                    COALESCE(SUM(m.KEY_UPDATES), 0) + COALESCE(SUM(m.INCLUDE_COL_UPDATES), 0),
                    CHAR(NULLIF(i.LASTUSED, '0001-01-01'), ISO)
               FROM SYSCAT.INDEXES i
               LEFT JOIN TABLE(MON_GET_INDEX(CAST(? AS VARCHAR(128)), CAST(? AS VARCHAR(128)), -2)) m
                 ON m.TABSCHEMA = i.TABSCHEMA AND m.TABNAME = i.TABNAME AND m.IID = i.IID
              WHERE i.TABSCHEMA = ? AND i.TABNAME = ?
              GROUP BY i.INDNAME, i.UNIQUERULE, i.LASTUSED",
        ),
        Eng::Db2i => Some(
            "SELECT INDEX_NAME, '', COALESCE(QUERY_USE_COUNT, 0), 0, VARCHAR_FORMAT(LAST_QUERY_USE, 'YYYY-MM-DD HH24:MI:SS')
               FROM QSYS2.SYSINDEXSTAT WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        ),
        Eng::Ase => Some(
            "SELECT i.name, CASE WHEN i.status & 2048 = 2048 THEN 'P' ELSE '' END, COALESCE(a.UsedCount, 0),
                    COALESCE(a.RowsInserted, 0) + COALESCE(a.RowsDeleted, 0) + COALESCE(a.RowsUpdated, 0),
                    CONVERT(varchar(19), a.LastUsedDate, 23)
               FROM sysindexes i
               LEFT JOIN master..monOpenObjectActivity a ON a.DBID = db_id() AND a.ObjectID = i.id AND a.IndexID = i.indid
              WHERE i.id = object_id(?) AND i.indid BETWEEN 1 AND 254",
        ),
        _ => None,
    }
}

pub fn counters_params(e: Eng, schema: Option<&str>, table: &str) -> Vec<String> {
    let s = schema.unwrap_or_default().to_string();
    match e {
        Eng::Db2 => vec![s.clone(), table.into(), s, table.into()],
        Eng::Ase => vec![if s.is_empty() { table.into() } else { format!("{s}.{table}") }],
        _ => vec![s, table.into()],
    }
}

/// When the counters started.
pub fn since_sql(e: Eng) -> Option<&'static str> {
    match e {
        Eng::Db2 => Some("SELECT VARCHAR_FORMAT(MIN(DB_CONN_TIME), 'YYYY-MM-DD HH24:MI:SS') FROM TABLE(MON_GET_DATABASE(-2))"),
        _ => None,
    }
}

fn num(v: Option<&String>) -> u64 {
    v.and_then(|s| s.trim().split('.').next()?.parse().ok()).unwrap_or(0)
}

/// `counters_sql`'s rows: name, primary flag (`P`), reads, writes, last read.
pub fn parse_counters(rows: &[Vec<Option<String>>]) -> Vec<UsageRow> {
    rows.iter()
        .filter_map(|r| {
            let name = r.first()?.as_ref()?.trim().to_string();
            Some(UsageRow {
                name,
                primary: r.get(1).and_then(|v| v.as_deref()).is_some_and(|v| v.trim() == "P"),
                seeks: num(r.get(2).and_then(Option::as_ref)),
                updates: num(r.get(3).and_then(Option::as_ref)),
                last_read: r.get(4).cloned().flatten().map(|d| d.trim().replace('T', " ")).filter(|d| !d.is_empty()),
            })
        })
        .collect()
}

/// What the UI says: the mapping where there are counters, why not where
/// there aren't.
pub fn note(p: &Preset, stats: bool) -> String {
    let e = design::eng(p);
    match (e, stats) {
        (Eng::Db2, true) => "Db2 cuenta los accesos a cada índice (INDEX_SCANS) sin separar búsquedas puntuales de recorridos por rango: se muestran como lecturas. Escrituras: KEY_UPDATES + INCLUDE_COL_UPDATES. Último uso: SYSCAT.INDEXES.LASTUSED.".into(),
        (Eng::Db2, false) => "Para ver los contadores de uso el usuario necesita EXECUTE sobre MON_GET_INDEX (o la autoridad SQLADM, DBADM o DATAACCESS): se listan los índices sin contadores.".into(),
        (Eng::Db2i, true) => "Db2 for i cuenta las consultas que usaron cada índice (QUERY_USE_COUNT) sin separar búsquedas de recorridos ni contar escrituras.".into(),
        (Eng::Db2i, false) => "No se pudo leer QSYS2.SYSINDEXSTAT: se listan los índices sin contadores.".into(),
        (Eng::Ase, true) => "ASE cuenta los planes que usaron cada índice (UsedCount) mientras su descriptor estuvo abierto, sin separar búsquedas de recorridos. Escrituras: filas insertadas, borradas y actualizadas.".into(),
        (Eng::Ase, false) => "Los contadores de uso salen de monOpenObjectActivity: necesitan el rol mon_role y las opciones «enable monitoring» y «per object statistics active». Se listan los índices sin contadores.".into(),
        _ if !design::has_indexes(p) => format!("{} no tiene índices definidos por el usuario: se muestran la clave primaria y las claves foráneas.", p.name),
        _ => format!("{} no expone cuántas veces se usa cada índice: se listan los índices con sus columnas, sin contadores. Para saber si una consulta usa un índice, mirá su plan de ejecución.", p.name),
    }
}

/// The report: the primary key first, then the indexes in the schema's
/// order, with `usage`'s counters where there are (`None`: no counters).
pub fn assemble(p: &Preset, t: Option<&TableSchema>, usage: Option<&[UsageRow]>, since: Option<String>) -> IndexUsageReport {
    let stats = usage.is_some();
    let mut r = IndexUsageReport { since, stats_available: stats, note: Some(note(p, stats)), seek_scan_split: false, ..Default::default() };
    let Some(t) = t else { return r.derived() };
    let fill = |mut i: IndexUsage, u: Option<&UsageRow>| {
        if let Some(u) = u {
            (i.seeks, i.updates, i.last_read) = (u.seeks, u.updates, u.last_read.clone());
        }
        i
    };
    let rows = usage.unwrap_or_default();
    if let Some(pk) = &t.primary_key {
        let name = pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into());
        let u = rows.iter().find(|u| u.primary).or_else(|| rows.iter().find(|u| u.name == name));
        let kind = if design::has_indexes(p) { "PRIMARY KEY" } else { "CONSTRAINT" };
        r.indexes.push(fill(
            IndexUsage { name, kind: kind.into(), unique: true, primary_key: true, key_columns: pk.columns.clone(), ..Default::default() },
            u,
        ));
    }
    for ix in &t.indexes {
        let u = rows.iter().find(|u| u.name == ix.name);
        r.indexes.push(fill(
            IndexUsage {
                name: ix.name.clone(),
                kind: ix.kind.clone().unwrap_or_else(|| "INDEX".into()),
                unique: ix.unique,
                key_columns: ix.columns.clone(),
                included_columns: ix.include.clone(),
                filter: ix.filter.clone(),
                ..Default::default()
            },
            u,
        ));
    }
    r.foreign_keys = t.foreign_keys.clone();
    r.derived()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;
    use dbine_driver::{ForeignKeyDef, IndexDef, KeyDef};

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn table() -> TableSchema {
        TableSchema {
            schema: Some("APP".into()),
            name: "T".into(),
            primary_key: Some(KeyDef { name: Some("PK_T".into()), columns: vec!["ID".into()] }),
            indexes: vec![
                IndexDef { name: "IX_A".into(), columns: vec!["A".into()], include: vec!["C".into()], ..Default::default() },
                IndexDef { name: "IX_B".into(), columns: vec!["B".into()], unique: true, kind: Some("CLUSTERED".into()), ..Default::default() },
            ],
            foreign_keys: vec![ForeignKeyDef { columns: vec!["P_ID".into()], ref_table: "P".into(), ref_columns: vec!["ID".into()], ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn which_presets_offer_it() {
        for (id, on) in [("db2", true), ("informix", true), ("teradata", true), ("vertica", true), ("exasol", true), ("netezza", true), ("hive", false), ("spark", false), ("impala", false)] {
            assert_eq!(supported(preset(id)), on, "{id}");
        }
    }

    #[test]
    fn counter_queries_and_parameters() {
        assert!(counters_sql(Eng::Db2).unwrap().contains("MON_GET_INDEX(CAST(? AS VARCHAR(128))"));
        assert_eq!(counters_params(Eng::Db2, Some("APP"), "T"), ["APP", "T", "APP", "T"]);
        assert_eq!(counters_params(Eng::Ase, Some("dbo"), "t"), ["dbo.t"]);
        assert_eq!(counters_params(Eng::Ase, None, "t"), ["t"]);
        assert_eq!(counters_params(Eng::Db2i, Some("LIB"), "T"), ["LIB", "T"]);
        assert!(counters_sql(Eng::Teradata).is_none() && counters_sql(Eng::Informix).is_none());
        assert!(since_sql(Eng::Db2).is_some() && since_sql(Eng::Ase).is_none());
    }

    #[test]
    fn db2_counters() {
        let s = |v: &str| Some(v.to_string());
        let rows = parse_counters(&[
            vec![s("SQL230101"), s("P"), s("12"), s("3"), s("2026-09-30")],
            vec![s("IX_A"), s("D"), s("5"), s("2"), None],
            vec![s("IX_B"), s("U"), s("0"), s("7"), None],
        ]);
        let r = assemble(preset("db2"), Some(&table()), Some(&rows), Some("2026-09-01 08:00:00".into()));
        assert!(r.stats_available && !r.seek_scan_split);
        assert_eq!(r.since.as_deref(), Some("2026-09-01 08:00:00"));
        let got: Vec<(&str, u64, u64)> = r.indexes.iter().map(|i| (i.name.as_str(), i.seeks, i.updates)).collect();
        assert_eq!(got, [("PK_T", 12, 3), ("IX_A", 5, 2), ("IX_B", 0, 7)]);
        assert_eq!(r.indexes[0].last_read.as_deref(), Some("2026-09-30"));
        assert!(r.indexes[2].unused && !r.indexes[1].unused);
        assert_eq!(r.indexes[1].included_columns, ["C"]);
        assert_eq!(r.indexes[2].kind, "CLUSTERED");
        // One counter for every access: no seek ratio nor health.
        assert!(r.indexes.iter().all(|i| i.seek_ratio.is_none() && i.seek_health.is_none()));
        assert_eq!(r.indexes[0].read_share.map(|v| (v * 100.0).round()), Some(71.0));
        assert_eq!(r.foreign_keys.len(), 1);
        assert!(r.note.unwrap().contains("INDEX_SCANS"));
    }

    #[test]
    fn ase_dates_and_refusals() {
        let s = |v: &str| Some(v.to_string());
        let rows = parse_counters(&[vec![s("PK_T"), s("P"), s("4"), s("0"), s("2026-10-01T10:20:30")]]);
        assert_eq!(rows[0].last_read.as_deref(), Some("2026-10-01 10:20:30"));
        let r = assemble(preset("sybase"), Some(&table()), None, None);
        assert!(!r.stats_available && r.note.unwrap().contains("mon_role"));
        assert!(r.indexes.iter().all(|i| i.reads == 0 && !i.unused));
    }

    #[test]
    fn listing_only_engines() {
        let r = assemble(preset("teradata"), Some(&table()), None, None);
        assert!(!r.stats_available && r.note.as_deref().unwrap().starts_with("Teradata"));
        assert_eq!(r.indexes.len(), 3);
        let t = TableSchema { indexes: vec![], ..table() };
        let r = assemble(preset("vertica"), Some(&t), None, None);
        assert_eq!((r.indexes[0].kind.as_str(), r.indexes.len()), ("CONSTRAINT", 1));
        assert!(r.note.unwrap().contains("no tiene índices"));
        assert!(assemble(preset("db2"), None, None, None).indexes.is_empty());
    }
}

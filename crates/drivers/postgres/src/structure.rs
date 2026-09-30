//! The session's side of the table structure: every table with columns,
//! keys and indexes in a handful of catalog queries, and CREATE / DROP
//! DATABASE.

use crate::catalog::{cell, lit};
use crate::compare;
use crate::design;
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, CheckDef, ColumnDef, Error, ForeignKeyDef, IndexDef, KeyDef, Result, TableSchema};
use std::collections::BTreeMap;
use tokio_postgres::SimpleQueryRow;

type Key = (String, String);

fn key(r: &SimpleQueryRow) -> Key {
    (cell(r, "sch").unwrap_or_default(), cell(r, "tbl").unwrap_or_default())
}

fn flag(r: &SimpleQueryRow, name: &str) -> bool {
    cell(r, name).is_some_and(|v| v == "t" || v == "true")
}

/// `pg_constraint.confdeltype` / `confupdtype`; NO ACTION is the default.
pub(crate) fn fk_action(code: Option<&str>) -> Option<String> {
    match code? {
        "c" => Some("CASCADE".into()),
        "n" => Some("SET NULL".into()),
        "d" => Some("SET DEFAULT".into()),
        "r" => Some("RESTRICT".into()),
        _ => None,
    }
}

/// `information_schema.referential_constraints` rules; NO ACTION is the default.
fn info_action(rule: Option<String>) -> Option<String> {
    rule.filter(|r| !r.eq_ignore_ascii_case("NO ACTION"))
}

/// Whether a default makes the column a counter: `nextval(…)` (serial).
fn is_serial_default(d: &str) -> bool {
    d.starts_with("nextval(")
}

impl PgSession {
    pub(crate) async fn schema_of_database(&mut self) -> Result<Vec<TableSchema>> {
        match self.variant {
            Variant::Redshift | Variant::Denodo | Variant::CrateDb | Variant::RisingWave | Variant::H2 => {
                self.info_schema_structure().await
            }
            _ => self.pg_structure().await,
        }
    }

    async fn pg_structure(&self) -> Result<Vec<TableSchema>> {
        let v = self.variant;
        let filter = self.filter("n.nspname");
        let partition = if self.version >= 100000 && v != Variant::Cockroach { " AND NOT c.relispartition" } else { "" };
        let tables = format!(
            "SELECT n.nspname AS sch, c.relname AS tbl, obj_description(c.oid, 'pg_class') AS cmt
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'p') AND {filter}{partition}
             ORDER BY 1, 2"
        );
        let mut order: Vec<Key> = Vec::new();
        let mut map: BTreeMap<Key, TableSchema> = BTreeMap::new();
        for r in self.text(&tables).await? {
            let k = key(&r);
            order.push(k.clone());
            map.insert(
                k.clone(),
                TableSchema {
                    kind: kinds::TABLE.into(),
                    schema: Some(k.0),
                    name: k.1,
                    comment: cell(&r, "cmt"),
                    ..Default::default()
                },
            );
        }
        if map.is_empty() {
            return Ok(Vec::new());
        }

        let identity = if self.version >= 100000 || v == Variant::Cockroach { "a.attidentity" } else { "''" };
        let generated = if self.version >= 120000 && v != Variant::Cockroach { "a.attgenerated" } else { "''" };
        let hidden = if v == Variant::Cockroach {
            " AND NOT EXISTS (SELECT 1 FROM information_schema.columns ic
                              WHERE ic.table_schema = n.nspname AND ic.table_name = c.relname
                                AND ic.column_name = a.attname AND ic.is_hidden = 'YES')"
        } else {
            ""
        };
        let columns = format!(
            "SELECT n.nspname AS sch, c.relname AS tbl, a.attname AS col, format_type(a.atttypid, a.atttypmod) AS typ,
                    a.attnotnull AS notnull, pg_get_expr(d.adbin, d.adrelid) AS def, {identity}::text AS ident,
                    {generated}::text AS gen, col_description(c.oid, a.attnum) AS cmt
             FROM pg_attribute a
             JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
             WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped AND {filter}{hidden}
             ORDER BY 1, 2, a.attnum"
        );
        for r in self.text(&columns).await? {
            let Some(t) = map.get_mut(&key(&r)) else { continue };
            let mut data_type = cell(&r, "typ").unwrap_or_default();
            let mut default_value = cell(&r, "def");
            let ident = cell(&r, "ident").is_some_and(|i| !i.is_empty());
            let serial = default_value.as_deref().is_some_and(is_serial_default);
            if cell(&r, "gen").as_deref() == Some("s") {
                if let Some(expr) = default_value.take() {
                    data_type = format!("{data_type} GENERATED ALWAYS AS ({expr}) STORED");
                }
            }
            if serial {
                // Written back as an identity column.
                default_value = None;
            }
            t.columns.push(ColumnDef {
                name: cell(&r, "col").unwrap_or_default(),
                data_type,
                nullable: !flag(&r, "notnull"),
                default_value,
                auto_increment: ident || serial,
                comment: cell(&r, "cmt"),
                ..Default::default()
            });
        }

        // Identity (and serial) sequences: ALWAYS and the options that
        // aren't the defaults, so the DDL makes the same counter.
        if self.version >= 100000 && v != Variant::Cockroach && compare::has_sequences(v) {
            let identities = format!(
                "SELECT n.nspname AS sch, c.relname AS tbl, a.attname AS col, a.attidentity::text = 'a' AS always,
                        format_type(q.seqtypid, NULL) AS typ, q.seqstart::text AS st, q.seqincrement::text AS inc,
                        q.seqmin::text AS mn, q.seqmax::text AS mx, q.seqcache::text AS cache, q.seqcycle AS cyc
                 FROM pg_attribute a
                 JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_depend d ON d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass
                      AND d.refobjid = c.oid AND d.refobjsubid = a.attnum AND d.deptype IN ('i', 'a')
                 JOIN pg_sequence q ON q.seqrelid = d.objid
                 WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped AND {filter}"
            );
            match self.text(&identities).await {
                Ok(rows) => {
                    for r in rows {
                        let Some(t) = map.get_mut(&key(&r)) else { continue };
                        let col = cell(&r, "col").unwrap_or_default();
                        let Some(c) = t.columns.iter_mut().find(|c| c.name == col && c.auto_increment) else { continue };
                        let n = |k: &str| cell(&r, k).and_then(|s| s.parse::<i128>().ok());
                        let (Some(st), Some(inc), Some(mn), Some(mx), Some(cache)) = (n("st"), n("inc"), n("mn"), n("mx"), n("cache")) else {
                            continue;
                        };
                        let typ = cell(&r, "typ").unwrap_or_default();
                        if let Some(o) = design::identity_option(flag(&r, "always"), &typ, st, inc, mn, mx, cache, flag(&r, "cyc")) {
                            c.options.insert(design::IDENTITY_OPTION.into(), o);
                        }
                    }
                }
                Err(e) => tracing::debug!("{v:?}: identity options unavailable: {e}"),
            }
        }

        // Primary and foreign keys, one row per key column in key order.
        let constraints = format!(
            "SELECT n.nspname AS sch, c.relname AS tbl, con.conname AS con, con.contype::text AS typ,
                    a.attname AS col, rn.nspname AS rsch, rc.relname AS rtbl, ra.attname AS rcol,
                    con.confdeltype::text AS del, con.confupdtype::text AS upd
             FROM pg_constraint con
             JOIN pg_class c ON c.oid = con.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace
             CROSS JOIN LATERAL unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum
             LEFT JOIN pg_class rc ON rc.oid = con.confrelid
             LEFT JOIN pg_namespace rn ON rn.oid = rc.relnamespace
             LEFT JOIN pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = con.confkey[k.ord::int]
             WHERE con.contype IN ('p', 'f') AND c.relkind IN ('r', 'p') AND {filter}
             ORDER BY 1, 2, 3, k.ord"
        );
        for r in self.text(&constraints).await? {
            let Some(t) = map.get_mut(&key(&r)) else { continue };
            let name = cell(&r, "con");
            let col = cell(&r, "col").unwrap_or_default();
            if cell(&r, "typ").as_deref() == Some("p") {
                t.primary_key.get_or_insert_with(|| KeyDef { name: name.clone(), columns: Vec::new() }).columns.push(col);
                continue;
            }
            match t.foreign_keys.last_mut() {
                Some(fk) if fk.name == name => {
                    fk.columns.push(col);
                    fk.ref_columns.push(cell(&r, "rcol").unwrap_or_default());
                }
                _ => t.foreign_keys.push(ForeignKeyDef {
                    name,
                    columns: vec![col],
                    ref_schema: cell(&r, "rsch"),
                    ref_table: cell(&r, "rtbl").unwrap_or_default(),
                    ref_columns: vec![cell(&r, "rcol").unwrap_or_default()],
                    on_delete: fk_action(cell(&r, "del").as_deref()),
                    on_update: fk_action(cell(&r, "upd").as_deref()),
                }),
            }
        }

        // Indexes other than the primary key (unique and exclusion
        // constraints included), one row per column (keys, then INCLUDE /
        // STORING); expressions come back in parentheses. The first row
        // carries the whole definition, storage parameters and constraint.
        // openGauss reports 9.2 but has INCLUDE (on ubtree indexes).
        let key_atts = if self.version >= 110000 || matches!(v, Variant::Cockroach | Variant::OpenGauss) {
            "COALESCE(ix.indnkeyatts, ix.indnatts)"
        } else {
            "ix.indnatts"
        };
        let nnd = if self.version >= 150000 && v != Variant::Cockroach { "ix.indnullsnotdistinct" } else { "false" };
        // CockroachDB's unique indexes are constraints too; Materialize has
        // neither constraints nor pg_get_constraintdef.
        let constraint = if matches!(v, Variant::Cockroach | Variant::Materialize) {
            "NULL::text AS ctype, NULL::text AS cdef"
        } else {
            "(SELECT con.contype::text FROM pg_constraint con WHERE con.conindid = ix.indexrelid AND con.conrelid = ix.indrelid
                AND con.contype IN ('u', 'x') LIMIT 1) AS ctype,
             CASE WHEN k.ord = 1 THEN (SELECT pg_get_constraintdef(con.oid) FROM pg_constraint con WHERE con.conindid = ix.indexrelid
                AND con.conrelid = ix.indrelid AND con.contype IN ('u', 'x') LIMIT 1) END AS cdef"
        };
        let indexes = format!(
            "SELECT n.nspname AS sch, c.relname AS tbl, i.relname AS idx, ix.indisunique AS uniq, am.amname AS am,
                    pg_get_expr(ix.indpred, ix.indrelid) AS pred, a.attname AS att,
                    pg_get_indexdef(ix.indexrelid, k.ord, true) AS expr, k.ord > {key_atts} AS inc,
                    CASE WHEN k.ord = 1 THEN pg_get_indexdef(ix.indexrelid) END AS def,
                    CASE WHEN k.ord = 1 THEN array_to_string(i.reloptions, E'\\n') END AS opts,
                    {nnd} AS nnd, {constraint}
             FROM pg_index ix
             JOIN pg_class i ON i.oid = ix.indexrelid
             JOIN pg_class c ON c.oid = ix.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace
             LEFT JOIN pg_am am ON am.oid = i.relam
             CROSS JOIN LATERAL generate_series(1, ix.indnatts::int) AS k(ord)
             LEFT JOIN pg_attribute a ON a.attrelid = ix.indrelid AND a.attnum = ix.indkey[k.ord - 1]
             WHERE NOT ix.indisprimary AND c.relkind IN ('r', 'p') AND {filter}
             ORDER BY 1, 2, 3, k.ord"
        );
        match self.text(&indexes).await {
            Ok(rows) => {
                // Per index: its whole definition and its keys alone.
                let mut defs: BTreeMap<(Key, String), (String, Vec<String>)> = BTreeMap::new();
                for r in rows {
                    let Some(t) = map.get_mut(&key(&r)) else { continue };
                    let name = cell(&r, "idx").unwrap_or_default();
                    let expr = cell(&r, "expr").unwrap_or_default();
                    if t.indexes.last().is_none_or(|ix| ix.name != name) {
                        let mut ix = IndexDef {
                            name: name.clone(),
                            unique: flag(&r, "uniq"),
                            kind: cell(&r, "am"),
                            filter: cell(&r, "pred"),
                            ..Default::default()
                        };
                        let opts = cell(&r, "opts").unwrap_or_default();
                        for (k, val) in compare::reloptions(&opts) {
                            ix.options.insert(k, val);
                        }
                        if flag(&r, "nnd") {
                            ix.options.insert(compare::NULLS_NOT_DISTINCT.into(), "on".into());
                        }
                        if let (Some(ct), Some(cdef)) = (cell(&r, "ctype"), cell(&r, "cdef")) {
                            let with = opts.lines().collect::<Vec<_>>().join(", ");
                            ix.options.insert(compare::CONSTRAINT.into(), crate::clone::constraint_def(&cdef, &ct, Some(&with)));
                            if ct == "x" {
                                ix.kind = Some(compare::EXCLUDE.into());
                            }
                        }
                        t.indexes.push(ix);
                        defs.insert((key(&r), name.clone()), (cell(&r, "def").unwrap_or_default(), Vec::new()));
                    }
                    let ix = t.indexes.last_mut().expect("pushed above");
                    if flag(&r, "inc") {
                        ix.include.push(cell(&r, "att").unwrap_or(expr));
                        continue;
                    }
                    let col = match cell(&r, "att") {
                        Some(a) if a == expr || quote_ident(Quote::Double, &a) == expr => a,
                        // CockroachDB already wraps expressions.
                        _ if expr.starts_with("((") && expr.ends_with("))") => expr[1..expr.len() - 1].to_string(),
                        _ if expr.starts_with('(') && expr.ends_with(')') => expr.clone(),
                        _ => format!("({expr})"),
                    };
                    ix.columns.push(col);
                    if let Some((_, keys)) = defs.get_mut(&(key(&r), name)) {
                        keys.push(expr);
                    }
                }
                for ((k, name), (def, keys)) in defs {
                    let Some(ix) = map.get_mut(&k).and_then(|t| t.indexes.iter_mut().find(|i| i.name == name)) else { continue };
                    if ix.options.contains_key(compare::CONSTRAINT) {
                        continue;
                    }
                    if let Some(list) = compare::decorated_keys(&def, &keys) {
                        ix.options.insert(compare::KEYS.into(), list);
                    }
                }
            }
            Err(e) => tracing::debug!("{v:?}: indexes unavailable: {e}"),
        }

        // CHECK constraints (CockroachDB's hash-sharded indexes add their own).
        let checks = format!(
            "SELECT n.nspname AS sch, c.relname AS tbl, con.conname AS con, pg_get_constraintdef(con.oid) AS def
             FROM pg_constraint con
             JOIN pg_class c ON c.oid = con.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE con.contype = 'c' AND c.relkind IN ('r', 'p') AND {filter}
             ORDER BY 1, 2, 3"
        );
        // Materialize has no CHECK constraints.
        let checks = if v == Variant::Materialize { Ok(Vec::new()) } else { self.text(&checks).await };
        match checks {
            Ok(rows) => {
                for r in rows {
                    let Some(t) = map.get_mut(&key(&r)) else { continue };
                    let name = cell(&r, "con");
                    if v == Variant::Cockroach && name.as_deref().is_some_and(|n| n.starts_with("check_crdb_internal")) {
                        continue;
                    }
                    t.checks.push(CheckDef { name, expression: compare::check_expression(&cell(&r, "def").unwrap_or_default()) });
                }
            }
            Err(e) => tracing::debug!("{v:?}: checks unavailable: {e}"),
        }

        let mut out: Vec<TableSchema> = order.iter().filter_map(|k| map.remove(k)).collect();
        for t in &mut out {
            // CockroachDB's implicit key is on the hidden `rowid`.
            if t.primary_key.as_ref().is_some_and(|pk| pk.columns.iter().any(|c| !t.columns.iter().any(|x| &x.name == c))) {
                t.primary_key = None;
            }
        }
        Ok(out)
    }

    /// Redshift, Denodo, CrateDB and RisingWave, whose `pg_constraint` is
    /// missing or partial: `information_schema` (Redshift's `svv_columns`),
    /// keys when the engine reports them.
    async fn info_schema_structure(&self) -> Result<Vec<TableSchema>> {
        let v = self.variant;
        let (columns_view, remarks) = if v == Variant::Redshift { ("svv_columns", "remarks") } else { ("information_schema.columns", "NULL") };
        let db_filter = if v == Variant::Denodo { String::new() } else { format!(" AND {}", self.filter("c.table_schema")) };
        let columns = format!(
            "SELECT c.table_schema AS sch, c.table_name AS tbl, c.column_name AS col, c.data_type AS typ,
                    c.character_maximum_length AS len, c.numeric_precision AS prec, c.numeric_scale AS scale,
                    c.is_nullable AS nullable, c.column_default AS def, {remarks} AS cmt
             FROM {columns_view} c
             JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name
             WHERE t.table_type = 'BASE TABLE'{db_filter}
             ORDER BY 1, 2, c.ordinal_position"
        );
        let mut order: Vec<Key> = Vec::new();
        let mut map: BTreeMap<Key, TableSchema> = BTreeMap::new();
        for r in self.text(&columns).await? {
            let k = key(&r);
            let t = map.entry(k.clone()).or_insert_with(|| {
                order.push(k.clone());
                TableSchema {
                    kind: kinds::TABLE.into(),
                    schema: (v != Variant::Denodo).then(|| k.0.clone()),
                    name: k.1.clone(),
                    ..Default::default()
                }
            });
            let mut default_value = cell(&r, "def");
            let auto = default_value.as_deref().is_some_and(|d| is_serial_default(d) || d.contains("identity"));
            if auto {
                default_value = None;
            }
            t.columns.push(ColumnDef {
                name: cell(&r, "col").unwrap_or_default(),
                data_type: crate::catalog::info_type(
                    &cell(&r, "typ").unwrap_or_default(),
                    cell(&r, "len").as_deref(),
                    cell(&r, "prec").as_deref(),
                    cell(&r, "scale").as_deref(),
                ),
                nullable: cell(&r, "nullable").is_none_or(|n| n.eq_ignore_ascii_case("YES")),
                default_value,
                auto_increment: auto,
                comment: cell(&r, "cmt"),
                ..Default::default()
            });
        }
        if v != Variant::Denodo {
            let keys = format!(
                "SELECT tc.table_schema AS sch, tc.table_name AS tbl, tc.constraint_name AS con, tc.constraint_type AS typ,
                        kcu.column_name AS col, ccu.table_schema AS rsch, ccu.table_name AS rtbl, ccu.column_name AS rcol,
                        rc.delete_rule AS del, rc.update_rule AS upd
                 FROM information_schema.table_constraints tc
                 JOIN information_schema.key_column_usage kcu
                   ON kcu.constraint_schema = tc.constraint_schema AND kcu.constraint_name = tc.constraint_name
                  AND kcu.table_name = tc.table_name
                 LEFT JOIN information_schema.referential_constraints rc
                   ON rc.constraint_schema = tc.constraint_schema AND rc.constraint_name = tc.constraint_name
                 LEFT JOIN information_schema.constraint_column_usage ccu
                   ON ccu.constraint_schema = tc.constraint_schema AND ccu.constraint_name = tc.constraint_name
                  AND tc.constraint_type = 'FOREIGN KEY'
                 WHERE tc.constraint_type IN ('PRIMARY KEY', 'FOREIGN KEY') AND {}
                 ORDER BY 1, 2, 3, kcu.ordinal_position",
                self.filter("tc.table_schema")
            );
            match self.text(&keys).await {
                Ok(rows) => {
                    for r in rows {
                        let Some(t) = map.get_mut(&key(&r)) else { continue };
                        let name = cell(&r, "con");
                        let col = cell(&r, "col").unwrap_or_default();
                        if cell(&r, "typ").as_deref() == Some("PRIMARY KEY") {
                            let pk = t.primary_key.get_or_insert_with(|| KeyDef { name: name.clone(), columns: Vec::new() });
                            if !pk.columns.contains(&col) {
                                pk.columns.push(col);
                            }
                            continue;
                        }
                        match t.foreign_keys.last_mut() {
                            Some(fk) if fk.name == name => {
                                if !fk.columns.contains(&col) {
                                    fk.columns.push(col);
                                    fk.ref_columns.push(cell(&r, "rcol").unwrap_or_default());
                                }
                            }
                            _ => t.foreign_keys.push(ForeignKeyDef {
                                name,
                                columns: vec![col],
                                ref_schema: cell(&r, "rsch"),
                                ref_table: cell(&r, "rtbl").unwrap_or_default(),
                                ref_columns: vec![cell(&r, "rcol").unwrap_or_default()],
                                on_delete: info_action(cell(&r, "del")),
                                on_update: info_action(cell(&r, "upd")),
                            }),
                        }
                    }
                }
                Err(e) => tracing::debug!("redshift: keys unavailable: {e}"),
            }
            let comments = format!(
                "SELECT n.nspname AS sch, c.relname AS tbl, d.description AS cmt
                 FROM pg_description d JOIN pg_class c ON c.oid = d.objoid JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE d.objsubid = 0 AND c.relkind = 'r' AND {}",
                self.filter("n.nspname")
            );
            if let Ok(rows) = self.text(&comments).await {
                for r in rows {
                    if let Some(t) = map.get_mut(&key(&r)) {
                        t.comment = cell(&r, "cmt");
                    }
                }
            }
        }
        if v == Variant::CrateDb {
            for (k, ix, ck) in self.crate_extras(&order).await {
                if let Some(t) = map.get_mut(&k) {
                    t.indexes = ix;
                    t.checks = ck;
                }
            }
        }
        if v == Variant::H2 {
            for (k, c) in self.info_schema_checks().await {
                if let Some(t) = map.get_mut(&k) {
                    t.checks.push(c);
                }
            }
        }
        if v == Variant::RisingWave {
            for (k, ix) in self.rw_indexes().await {
                if let Some(t) = map.get_mut(&k) {
                    t.indexes.push(ix);
                }
            }
        }
        if v == Variant::H2 {
            for (k, ix) in self.h2_indexes().await {
                if let Some(t) = map.get_mut(&k) {
                    t.indexes.push(ix);
                }
            }
        }
        Ok(order.iter().filter_map(|k| map.remove(k)).collect())
    }

    pub(crate) async fn create_db(&mut self, name: &str) -> Result<()> {
        let sql = format!("CREATE DATABASE {}", quote_ident(Quote::Double, name));
        self.client.batch_execute(&sql).await.map_err(crate::err)
    }

    /// Other sessions on it are ended first (`WITH (FORCE)` on PostgreSQL 13+,
    /// `pg_terminate_backend` before that), as a manager is expected to.
    pub(crate) async fn drop_db(&mut self, name: &str) -> Result<()> {
        if self.database == name {
            return Err(Error::Query(format!(
                "No se puede borrar «{name}»: es la base de esta conexión. Conectate a otra (por ejemplo postgres)."
            )));
        }
        let db = quote_ident(Quote::Double, name);
        let v = self.variant;
        let sql = match v {
            Variant::Cockroach => format!("DROP DATABASE {db} CASCADE"),
            // RisingWave reports PostgreSQL 13 but has no WITH (FORCE).
            Variant::Redshift | Variant::RisingWave => format!("DROP DATABASE {db}"),
            _ if self.version >= 130000 => format!("DROP DATABASE {db} WITH (FORCE)"),
            _ => {
                let kill = format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = {} AND pid <> pg_backend_pid()",
                    lit(v, name)
                );
                if let Err(e) = self.client.simple_query(&kill).await {
                    tracing::debug!("{v:?}: could not end the other sessions: {e}");
                }
                format!("DROP DATABASE {db}")
            }
        };
        self.client.batch_execute(&sql).await.map_err(crate::err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fk_actions_map_to_sql() {
        assert_eq!(fk_action(Some("c")).as_deref(), Some("CASCADE"));
        assert_eq!(fk_action(Some("n")).as_deref(), Some("SET NULL"));
        assert_eq!(fk_action(Some("a")), None);
        assert_eq!(info_action(Some("NO ACTION".into())), None);
        assert_eq!(info_action(Some("CASCADE".into())).as_deref(), Some("CASCADE"));
    }
}

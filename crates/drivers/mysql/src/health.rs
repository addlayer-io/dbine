//! "Chequeo de salud" findings of MySQL, MariaDB, TiDB and OceanBase
//! ([`dbine_driver::Session::health_checks`]). The app already reports
//! connections, long queries, blocking and backups; these are the
//! engine's own, for the tables of `database`:
//!
//! - Tables without a primary key (every engine here).
//! - MyISAM tables on a server whose default engine is InnoDB, with the
//!   `ALTER TABLE … ENGINE=InnoDB` (MySQL, MariaDB).
//! - Unused indexes, with the window since the server started ("No
//!   concluyente" under 14 days): performance_schema's
//!   `table_io_waits_summary_by_index_usage` (what
//!   `sys.schema_unused_indexes` reads), MariaDB's `INDEX_STATISTICS` when
//!   `userstat` is on, TiDB's `TIDB_INDEX_USAGE` (8.0+). Without counters
//!   (performance_schema off, userstat off) the check is skipped.
//!   OceanBase keeps its counts across restarts with no start date: skipped.
//! - Redundant indexes: one whose columns repeat the start of another's,
//!   from `information_schema.STATISTICS` (the rule of
//!   `sys.schema_redundant_indexes`, on every engine here, sys or not).
//! - Fragmented tables: `DATA_FREE` large against the data, with
//!   `OPTIMIZE TABLE` (MySQL, MariaDB; not Aurora, whose storage reports
//!   no such space).
//! - Foreign keys without an index (OceanBase: InnoDB and TiDB create one
//!   by themselves).
//! - Tables whose collation differs from the database's.
//!
//! Each check is its own query: one that fails (an older version, no
//! permission) is skipped. Fix scripts are only shown; DBine never runs
//! them.

use crate::session::{at, lit, MySqlSession};
use crate::Variant;
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::Result;
use mysql_async::Row;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;
/// Days the index-usage counters must cover before "unused" means anything.
const MIN_WINDOW_DAYS: i64 = 14;
/// Free space inside a table before it counts as fragmented.
const MIN_FREE_BYTES: i64 = 64 * 1024 * 1024;

fn get(r: &Row, i: usize) -> String {
    at(r, i).unwrap_or_default()
}

fn num(r: &Row, i: usize) -> i64 {
    at(r, i).and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v as i64).unwrap_or(0)
}

fn mib(bytes: i64) -> String {
    format!("{} MB", bytes / (1024 * 1024))
}

/// Which checks an engine can answer.
struct Applies {
    no_pk: bool,
    myisam: bool,
    unused: bool,
    redundant: bool,
    fragmentation: bool,
    fk: bool,
    collations: bool,
}

fn applies(engine: Variant, product: Variant) -> Applies {
    let server = matches!(engine, Variant::MySql | Variant::MariaDb);
    let any = server || matches!(engine, Variant::TiDb | Variant::OceanBase);
    Applies {
        no_pk: any,
        myisam: server,
        unused: server || engine == Variant::TiDb,
        redundant: any,
        fragmentation: server && product != Variant::AuroraMySql,
        fk: engine == Variant::OceanBase,
        collations: any,
    }
}

/// An index as `information_schema.STATISTICS` lists it.
#[derive(Debug, Clone, PartialEq)]
struct Index {
    name: String,
    unique: bool,
    kind: String,
    /// Each key part: column (or the index's expression), prefix length, order.
    parts: Vec<String>,
}

/// Redundant indexes of one table: `(redundant, kept)`. An index is
/// redundant when its key parts are the first ones of another index of the
/// same kind and it isn't unique (or both are unique with the same parts).
/// Of two equal indexes only one is reported; the primary key is never
/// the redundant one.
fn redundant(indexes: &[Index]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for a in indexes {
        if a.name == "PRIMARY" || matches!(a.kind.as_str(), "FULLTEXT" | "SPATIAL") {
            continue;
        }
        let dominant = indexes.iter().find(|b| {
            if b.name == a.name || b.kind != a.kind || b.parts.len() < a.parts.len() || b.parts[..a.parts.len()] != a.parts[..] {
                return false;
            }
            let equal = b.parts.len() == a.parts.len();
            if a.unique {
                // A unique index only repeats one that's unique on the same parts.
                return b.unique && equal && (b.name == "PRIMARY" || b.name < a.name);
            }
            // Two equal non-unique ones: the first one by name stays.
            !equal || b.unique || b.name < a.name
        });
        if let Some(b) = dominant {
            out.push((a.name.clone(), b.name.clone()));
        }
    }
    out
}

impl MySqlSession {
    /// A check's rows; `None` (and a debug line) when the query fails.
    async fn check_rows(&mut self, what: &str, sql: &str) -> Option<Vec<Row>> {
        match self.rows(sql).await {
            Ok(rows) => Some(rows),
            Err(e) => {
                tracing::debug!("{:?}: health check {what} skipped: {e}", self.variant);
                None
            }
        }
    }

    async fn check_value(&mut self, what: &str, sql: &str, col: usize) -> Option<String> {
        self.check_rows(what, sql).await?.first().and_then(|r| at(r, col))
    }

    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let a = applies(self.variant, self.product);
        let mut out = Vec::new();
        if database.is_empty() {
            return Ok(out);
        }
        if a.no_pk {
            self.tables_without_pk(database, &mut out).await;
        }
        if a.myisam {
            self.myisam_tables(database, &mut out).await;
        }
        if a.unused {
            self.unused_indexes(database, &mut out).await;
        }
        if a.redundant {
            self.redundant_indexes(database, &mut out).await;
        }
        if a.fragmentation {
            self.fragmented_tables(database, &mut out).await;
        }
        if a.fk {
            self.fk_without_index(database, &mut out).await;
        }
        if a.collations {
            self.mixed_collations(database, &mut out).await;
        }
        for c in &mut out {
            c.objects.truncate(MAX_OBJECTS);
        }
        Ok(out)
    }

    async fn tables_without_pk(&mut self, db: &str, out: &mut Vec<HealthCheck>) {
        let sql = format!(
            "SELECT t.TABLE_NAME FROM information_schema.TABLES t
             WHERE t.TABLE_SCHEMA = {db} AND t.TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED')
               AND NOT EXISTS (SELECT 1 FROM information_schema.TABLE_CONSTRAINTS c
                               WHERE c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME AND c.CONSTRAINT_TYPE = 'PRIMARY KEY')
             ORDER BY 1 LIMIT {MAX_OBJECTS}",
            db = lit(db)
        );
        let Some(rows) = self.check_rows("tables without pk", &sql).await else { return };
        let objects: Vec<String> = rows.iter().map(|r| get(r, 0)).collect();
        out.push(
            HealthCheck::new(
                "no_primary_key",
                "Diseño",
                if objects.is_empty() { "Todas las tablas tienen clave primaria".to_string() } else { format!("{} tablas sin clave primaria", objects.len()) },
                if objects.is_empty() { Severity::Ok } else { Severity::Warning },
            )
            .detail(
                "Sin clave primaria el motor usa una clave interna oculta, nada impide filas duplicadas y, con replicación por filas, \
                 cada UPDATE o DELETE recorre la tabla entera en las réplicas. Group Replication y sql_require_primary_key la exigen.",
            )
            .objects(objects),
        );
    }

    async fn myisam_tables(&mut self, db: &str, out: &mut Vec<HealthCheck>) {
        let default = self.check_value("default engine", "SELECT @@default_storage_engine", 0).await;
        if !default.is_some_and(|e| e.eq_ignore_ascii_case("InnoDB")) {
            return;
        }
        let sql = format!(
            "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_TYPE = 'BASE TABLE' AND ENGINE = 'MyISAM'
             ORDER BY 1 LIMIT {MAX_OBJECTS}",
            lit(db)
        );
        let Some(rows) = self.check_rows("myisam", &sql).await else { return };
        let objects: Vec<String> = rows.iter().map(|r| get(r, 0)).collect();
        let fixes: Vec<String> =
            objects.iter().map(|t| format!("ALTER TABLE {} ENGINE=InnoDB;", qualified_name(Quote::Backtick, Some(db), t))).collect();
        let mut c = HealthCheck::new(
            "myisam_tables",
            "Diseño",
            if objects.is_empty() { "Ninguna tabla usa MyISAM".to_string() } else { format!("{} tablas MyISAM", objects.len()) },
            if objects.is_empty() { Severity::Ok } else { Severity::Warning },
        )
        .detail(
            "MyISAM no tiene transacciones ni recuperación ante caídas, bloquea la tabla entera en cada escritura e ignora las claves foráneas. \
             Convertirla a InnoDB reescribe la tabla: en tablas grandes, hacelo fuera de horario.",
        )
        .objects(objects);
        if !fixes.is_empty() {
            c = c.fix(fixes.join("\n"));
        }
        out.push(c);
    }

    /// Indexes with no reads since the server started, from the counters
    /// the engine has on.
    async fn unused_indexes(&mut self, db: &str, out: &mut Vec<HealthCheck>) {
        let db_lit = lit(db);
        let not_unique = |schema: &str, table: &str, index: &str| {
            format!(
                "NOT EXISTS (SELECT 1 FROM information_schema.STATISTICS st
                             WHERE st.TABLE_SCHEMA = {schema} AND st.TABLE_NAME = {table} AND st.INDEX_NAME = {index} AND st.NON_UNIQUE = 0)"
            )
        };
        let mut queries = Vec::new();
        if self.variant == Variant::TiDb {
            queries.push(format!(
                "SELECT u.TABLE_NAME, u.INDEX_NAME FROM information_schema.CLUSTER_TIDB_INDEX_USAGE u
                 WHERE u.TABLE_SCHEMA = {db_lit} AND u.INDEX_NAME <> 'PRIMARY' AND {}
                 GROUP BY u.TABLE_NAME, u.INDEX_NAME HAVING SUM(u.QUERY_TOTAL) = 0 ORDER BY 1, 2 LIMIT {MAX_OBJECTS}",
                not_unique("u.TABLE_SCHEMA", "u.TABLE_NAME", "u.INDEX_NAME")
            ));
            queries.push(format!(
                "SELECT u.TABLE_NAME, u.INDEX_NAME FROM information_schema.TIDB_INDEX_USAGE u
                 WHERE u.TABLE_SCHEMA = {db_lit} AND u.INDEX_NAME <> 'PRIMARY' AND u.QUERY_TOTAL = 0 AND {}
                 ORDER BY 1, 2 LIMIT {MAX_OBJECTS}",
                not_unique("u.TABLE_SCHEMA", "u.TABLE_NAME", "u.INDEX_NAME")
            ));
        } else {
            let on = |v: Option<String>| v.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("ON") || v.eq_ignore_ascii_case("YES"));
            if self.variant == Variant::MariaDb && on(self.check_value("userstat", "SELECT @@userstat", 0).await) {
                // INDEX_STATISTICS only lists the indexes that were read.
                queries.push(format!(
                    "SELECT s.TABLE_NAME, s.INDEX_NAME FROM information_schema.STATISTICS s
                     WHERE s.TABLE_SCHEMA = {db_lit} AND s.SEQ_IN_INDEX = 1 AND s.NON_UNIQUE = 1 AND s.INDEX_NAME <> 'PRIMARY'
                       AND NOT EXISTS (SELECT 1 FROM information_schema.INDEX_STATISTICS x
                                       WHERE x.TABLE_SCHEMA = s.TABLE_SCHEMA AND x.TABLE_NAME = s.TABLE_NAME AND x.INDEX_NAME = s.INDEX_NAME AND x.ROWS_READ > 0)
                     ORDER BY 1, 2 LIMIT {MAX_OBJECTS}"
                ));
            } else if on(self.check_value("performance_schema", "SELECT @@performance_schema", 0).await)
                && on(self.check_value("instrument", crate::index_usage::PERF_INSTRUMENT_SQL, 0).await)
            {
                // What sys.schema_unused_indexes reads, without the unique ones.
                queries.push(format!(
                    "SELECT u.OBJECT_NAME, u.INDEX_NAME FROM performance_schema.table_io_waits_summary_by_index_usage u
                     WHERE u.OBJECT_TYPE = 'TABLE' AND u.OBJECT_SCHEMA = {db_lit} AND u.INDEX_NAME IS NOT NULL AND u.INDEX_NAME <> 'PRIMARY'
                       AND u.COUNT_STAR = 0 AND {}
                     ORDER BY 1, 2 LIMIT {MAX_OBJECTS}",
                    not_unique("u.OBJECT_SCHEMA", "u.OBJECT_NAME", "u.INDEX_NAME")
                ));
            }
        }
        let mut rows = None;
        for q in &queries {
            if let Some(r) = self.check_rows("unused indexes", q).await {
                rows = Some(r);
                break;
            }
        }
        let Some(rows) = rows else { return };
        let window = self.check_rows("uptime", "SHOW GLOBAL STATUS LIKE 'Uptime'").await.and_then(|r| r.first().map(|r| num(r, 1) / 86400));
        let objects: Vec<String> = rows.iter().map(|r| format!("{} · {}", get(r, 0), get(r, 1))).collect();
        let drops: Vec<String> = rows
            .iter()
            .map(|r| drop_comment(db, &get(r, 0), &get(r, 1)))
            .collect();
        out.push(unused_check(objects, drops, window));
    }

    async fn redundant_indexes(&mut self, db: &str, out: &mut Vec<HealthCheck>) {
        let sql = format!(
            "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, INDEX_TYPE, COLUMN_NAME, SUB_PART, COLLATION
             FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {}
             ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
            lit(db)
        );
        let Some(rows) = self.check_rows("redundant indexes", &sql).await else { return };
        let mut tables: BTreeMap<String, Vec<Index>> = BTreeMap::new();
        for r in &rows {
            let (table, name) = (get(r, 0), get(r, 1));
            let part = format!(
                "{}({}){}",
                at(r, 4).unwrap_or_else(|| format!("\u{0}expr {name}")),
                get(r, 5),
                get(r, 6)
            );
            let list = tables.entry(table).or_default();
            match list.last_mut().filter(|i| i.name == name) {
                Some(i) => i.parts.push(part),
                None => list.push(Index { name, unique: num(r, 2) == 0, kind: get(r, 3).to_ascii_uppercase(), parts: vec![part] }),
            }
        }
        let mut objects = Vec::new();
        let mut fixes = Vec::new();
        for (table, indexes) in &tables {
            for (ix, kept) in redundant(indexes) {
                objects.push(format!("{table} · {ix} (lo cubre {kept})"));
                fixes.push(format!("ALTER TABLE {} DROP INDEX {};", qualified_name(Quote::Backtick, Some(db), table), quote_ident(Quote::Backtick, &ix)));
            }
        }
        let mut c = HealthCheck::new(
            "redundant_indexes",
            "Rendimiento",
            if objects.is_empty() { "No hay índices redundantes".to_string() } else { format!("{} índices redundantes", objects.len()) },
            if objects.is_empty() { Severity::Ok } else { Severity::Warning },
        )
        .detail(
            "Sus columnas son las primeras de otro índice de la tabla, que ya sirve para las mismas búsquedas: cuesta espacio y tiempo en cada escritura. \
             Antes de borrarlo, revisá que ninguna consulta lo nombre en un FORCE INDEX o USE INDEX.",
        )
        .objects(objects);
        if !fixes.is_empty() {
            c = c.fix(fixes.join("\n"));
        }
        out.push(c);
    }

    async fn fragmented_tables(&mut self, db: &str, out: &mut Vec<HealthCheck>) {
        // MySQL 8 caches these columns for a day unless asked otherwise.
        if self.variant == Variant::MySql {
            let _ = self.rows("SET SESSION information_schema_stats_expiry = 0").await;
        }
        let sql = format!(
            "SELECT TABLE_NAME, DATA_LENGTH + INDEX_LENGTH, DATA_FREE FROM information_schema.TABLES
             WHERE TABLE_SCHEMA = {} AND TABLE_TYPE = 'BASE TABLE' AND ENGINE IN ('InnoDB', 'MyISAM', 'Aria')
               AND DATA_FREE >= {MIN_FREE_BYTES} AND DATA_FREE > 0.2 * (DATA_LENGTH + INDEX_LENGTH)
             ORDER BY DATA_FREE DESC LIMIT {MAX_OBJECTS}",
            lit(db)
        );
        let Some(rows) = self.check_rows("fragmentation", &sql).await else { return };
        let objects: Vec<String> = rows.iter().map(|r| format!("{} ({} libres, {} de datos)", get(r, 0), mib(num(r, 2)), mib(num(r, 1)))).collect();
        let fixes: Vec<String> =
            rows.iter().map(|r| format!("OPTIMIZE TABLE {};", qualified_name(Quote::Backtick, Some(db), &get(r, 0)))).collect();
        let mut c = HealthCheck::new(
            "fragmented_tables",
            "Espacio",
            if objects.is_empty() { "Sin tablas con mucho espacio libre sin usar".to_string() } else { format!("{} tablas fragmentadas", objects.len()) },
            if objects.is_empty() { Severity::Ok } else { Severity::Info },
        )
        .detail(
            "Espacio libre (DATA_FREE) de más de 64 MB y más del 20 % de la tabla, que quedó de filas borradas. OPTIMIZE TABLE reconstruye la tabla y lo devuelve; \
             en InnoDB se hace en línea pero lleva tiempo y espacio en disco. Si la tabla está en un tablespace compartido, DATA_FREE es el del tablespace.",
        )
        .objects(objects);
        if !fixes.is_empty() {
            c = c.fix(fixes.join("\n"));
        }
        out.push(c);
    }

    async fn fk_without_index(&mut self, db: &str, out: &mut Vec<HealthCheck>) {
        let fk_sql = format!(
            "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE
             WHERE TABLE_SCHEMA = {} AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
            lit(db)
        );
        let lead_sql = format!("SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {} AND SEQ_IN_INDEX = 1", lit(db));
        let (Some(fks), Some(leads)) = (self.check_rows("foreign keys", &fk_sql).await, self.check_rows("index columns", &lead_sql).await) else {
            return;
        };
        let leading: HashSet<(String, String)> = leads.iter().map(|r| (get(r, 0).to_lowercase(), get(r, 1).to_lowercase())).collect();
        let mut keys: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for r in &fks {
            keys.entry((get(r, 0), get(r, 1))).or_default().push(get(r, 2));
        }
        let missing: Vec<(&(String, String), &Vec<String>)> =
            keys.iter().filter(|((t, _), cols)| !leading.contains(&(t.to_lowercase(), cols[0].to_lowercase()))).collect();
        let objects: Vec<String> = missing.iter().map(|((t, name), cols)| format!("{t} ({}) · {name}", cols.join(", "))).collect();
        let fixes: Vec<String> = missing
            .iter()
            .map(|((t, _), cols)| {
                let list = cols.iter().map(|c| quote_ident(Quote::Backtick, c)).collect::<Vec<_>>().join(", ");
                let name = quote_ident(Quote::Backtick, &format!("ix_{t}_{}", cols.join("_")));
                format!("CREATE INDEX {name} ON {} ({list});", qualified_name(Quote::Backtick, Some(db), t))
            })
            .collect();
        let mut c = HealthCheck::new(
            "fk_without_index",
            "Rendimiento",
            if objects.is_empty() { "Todas las claves foráneas tienen índice".to_string() } else { format!("{} claves foráneas sin índice", objects.len()) },
            if objects.is_empty() { Severity::Ok } else { Severity::Info },
        )
        .detail("Sin índice, borrar o actualizar en la tabla padre recorre la tabla hija entera, y los joins por esa columna son más lentos.")
        .objects(objects);
        if !fixes.is_empty() {
            c = c.fix(fixes.join("\n"));
        }
        out.push(c);
    }

    async fn mixed_collations(&mut self, db: &str, out: &mut Vec<HealthCheck>) {
        let Some(default) = self
            .check_rows(
                "database collation",
                &format!("SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {}", lit(db)),
            )
            .await
            .and_then(|r| r.first().map(|r| (get(r, 0), get(r, 1))))
        else {
            return;
        };
        let Some(rows) = self
            .check_rows(
                "table collations",
                &format!(
                    "SELECT TABLE_NAME, TABLE_COLLATION FROM information_schema.TABLES
                     WHERE TABLE_SCHEMA = {} AND TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED') AND TABLE_COLLATION IS NOT NULL ORDER BY 1",
                    lit(db)
                ),
            )
            .await
        else {
            return;
        };
        let (charset, collation) = default;
        let distinct: BTreeSet<String> = rows.iter().map(|r| get(r, 1)).collect();
        let odd: Vec<&Row> = rows.iter().filter(|r| !get(r, 1).eq_ignore_ascii_case(&collation)).collect();
        let objects: Vec<String> = odd.iter().take(MAX_OBJECTS).map(|r| format!("{} ({})", get(r, 0), get(r, 1))).collect();
        let (title, sev) = if odd.is_empty() {
            (format!("Todas las tablas usan la collation de la base ({collation})"), Severity::Ok)
        } else if distinct.len() > 1 {
            (format!("{} tablas con una collation distinta de la base ({collation})", odd.len()), Severity::Warning)
        } else {
            (format!("Las tablas usan {} y la base, {collation}", distinct.iter().next().cloned().unwrap_or_default()), Severity::Info)
        };
        let mut c = HealthCheck::new("mixed_collations", "Diseño", title, sev)
            .detail(
                "Comparar o unir columnas de texto con collations distintas falla («Illegal mix of collations») o impide usar los índices. \
                 Convertir una tabla reescribe sus datos y puede cambiar el largo máximo de sus índices: probalo antes en otro ambiente.",
            )
            .objects(objects);
        let safe = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !odd.is_empty() && self.variant.is_mysql_server() && safe(&charset) && safe(&collation) {
            let fixes: Vec<String> = odd
                .iter()
                .take(MAX_OBJECTS)
                .map(|r| format!("ALTER TABLE {} CONVERT TO CHARACTER SET {charset} COLLATE {collation};", qualified_name(Quote::Backtick, Some(db), &get(r, 0))))
                .collect();
            c = c.fix(fixes.join("\n"));
        }
        out.push(c);
    }
}

/// The unused-indexes finding: conclusive (and with a commented DROP per
/// index) only when the server has been up [`MIN_WINDOW_DAYS`].
/// Text for a `--` comment line: a server-controlled name can't end the
/// comment and turn the rest of the line into a statement. Line breaks
/// (CR, LF, NEL, U+2028/U+2029) and other control characters become `?`.
fn comment_text(s: &str) -> String {
    s.chars().map(|c| if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') { '?' } else { c }).collect()
}

/// A commented `ALTER TABLE … DROP INDEX` for the unused-indexes fix.
fn drop_comment(db: &str, table: &str, index: &str) -> String {
    let stmt = format!("ALTER TABLE {} DROP INDEX {};", qualified_name(Quote::Backtick, Some(db), table), quote_ident(Quote::Backtick, index));
    format!("-- {}", comment_text(&stmt))
}

fn unused_check(objects: Vec<String>, drops: Vec<String>, window: Option<i64>) -> HealthCheck {
    let conclusive = window.is_some_and(|d| d >= MIN_WINDOW_DAYS);
    let since = match window {
        Some(d) => format!("los contadores cubren {d} días, desde el arranque del servidor"),
        None => "el servidor no dice desde cuándo cuenta".to_string(),
    };
    let (title, sev) = if objects.is_empty() {
        ("Todos los índices se usaron".to_string(), Severity::Ok)
    } else if conclusive {
        (format!("{} índices sin lecturas ({since})", objects.len()), Severity::Warning)
    } else {
        (format!("No concluyente: {} índices sin lecturas, pero {since}", objects.len()), Severity::Info)
    };
    let mut c = HealthCheck::new("unused_indexes", "Rendimiento", title, sev)
        .detail(
            "Un índice que nunca se lee solo cuesta en cada escritura. Los contadores empiezan de nuevo al reiniciar el servidor (o al activarlos). \
             Antes de borrarlo, tené en cuenta procesos de fin de mes, reportes ocasionales y que cada réplica cuenta por separado.",
        )
        .objects(objects);
    if conclusive && !drops.is_empty() {
        c = c.fix(format!("-- Revisá cada uno antes de borrarlo:\n{}", drops.join("\n")));
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ix(name: &str, unique: bool, parts: &[&str]) -> Index {
        Index { name: name.into(), unique, kind: "BTREE".into(), parts: parts.iter().map(|p| p.to_string()).collect() }
    }

    #[test]
    fn redundant_indexes_follow_the_prefix_rule() {
        let r = redundant(&[
            ix("PRIMARY", true, &["id"]),
            ix("ix_id", false, &["id"]),
            ix("ix_a", false, &["a"]),
            ix("ix_ab", false, &["a", "b"]),
            ix("ix_b", false, &["b"]),
            ix("ix_b2", false, &["b"]),
            ix("ux_c", true, &["c"]),
            ix("ux_c2", true, &["c"]),
            ix("ux_cd", true, &["c", "d"]),
        ]);
        assert_eq!(
            r,
            vec![
                ("ix_id".to_string(), "PRIMARY".to_string()),
                ("ix_a".to_string(), "ix_ab".to_string()),
                ("ix_b2".to_string(), "ix_b".to_string()),
                ("ux_c2".to_string(), "ux_c".to_string()),
            ]
        );
        // A unique index isn't redundant to a wider one; FULLTEXT never is.
        let mut ft = ix("ft", false, &["a"]);
        ft.kind = "FULLTEXT".into();
        assert!(redundant(&[ix("ux_a", true, &["a"]), ix("ix_ab", false, &["a", "b"]), ft]).is_empty());
    }

    #[test]
    fn checks_by_engine() {
        let my = applies(Variant::MySql, Variant::MySql);
        assert!(my.no_pk && my.myisam && my.unused && my.redundant && my.fragmentation && !my.fk && my.collations);
        assert!(!applies(Variant::MySql, Variant::AuroraMySql).fragmentation);
        let ti = applies(Variant::TiDb, Variant::TiDb);
        assert!(ti.no_pk && !ti.myisam && ti.unused && !ti.fragmentation && !ti.fk);
        let ob = applies(Variant::OceanBase, Variant::OceanBase);
        assert!(ob.fk && !ob.unused && ob.redundant);
        for v in [Variant::SingleStore, Variant::StarRocks, Variant::Doris, Variant::Databend, Variant::Manticore, Variant::GreptimeDb] {
            let a = applies(v, v);
            assert!(!a.no_pk && !a.redundant && !a.unused && !a.collations, "{v:?}");
        }
    }

    #[test]
    fn a_name_cannot_end_the_commented_drop() {
        let line = drop_comment("d", "t\r\nDROP TABLE x;", "i`x\u{2028}\u{85}");
        assert_eq!(line, "-- ALTER TABLE `d`.`t??DROP TABLE x;` DROP INDEX `i``x??`;");
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn unused_is_conclusive_only_with_a_long_uptime() {
        let objs = || vec!["t · ix".to_string()];
        let drops = || vec!["-- ALTER TABLE `d`.`t` DROP INDEX `ix`;".to_string()];
        let short = unused_check(objs(), drops(), Some(2));
        assert!(short.title.starts_with("No concluyente") && short.fix.is_none() && short.severity == Severity::Info);
        let long = unused_check(objs(), drops(), Some(20));
        assert_eq!(long.severity, Severity::Warning);
        assert!(long.fix.unwrap().contains("DROP INDEX `ix`"));
    }
}

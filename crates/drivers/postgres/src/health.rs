//! "Chequeo de salud" findings of PostgreSQL and its family
//! ([`dbine_driver::Session::health_checks`]). The app already reports
//! connections, cache hits, long queries, blocking, idle transactions and
//! backups; these are the engine's own:
//!
//! - Maintenance (PostgreSQL's MVCC: not CockroachDB, YugabyteDB, whose
//!   storage has no VACUUM, nor the MPP forks, whose coordinator counters
//!   don't see the segments): autovacuum off globally or per table, tables
//!   with many dead tuples, tables with rows that were never analyzed.
//! - Transaction ID wraparound: `age(datfrozenxid)` against the 2^31 limit
//!   and `autovacuum_freeze_max_age` (not openGauss, whose XIDs are 64-bit).
//! - Unused indexes (`idx_scan = 0`) with the window the counters cover
//!   (`stats_reset`, or the server's start), "No concluyente" under 14
//!   days. CockroachDB: `crdb_internal.index_usage_statistics`, since the
//!   last node start.
//! - Invalid indexes (`indisvalid`), foreign keys no index starts with,
//!   tables without a primary key (CockroachDB: its hidden `rowid`),
//!   sequences near their limit (`pg_sequences`, PostgreSQL 10+) and
//!   duplicate indexes.
//! - CockroachDB: automatic statistics collection off.
//! - Redshift: `svv_table_info`, stale statistics and unsorted rows.
//!
//! Each check is its own query: one that fails (an older version, no
//! permission, a catalog the variant emulates) is skipped. Fix scripts are
//! only shown; DBine never runs them.

use crate::catalog::lit;
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::Result;
use std::time::Duration;
use tokio_postgres::SimpleQueryRow;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;
/// Days the index-usage counters must cover before "unused" means anything.
const MIN_WINDOW_DAYS: i64 = 14;
/// A check's query is cancelled after this long.
const QUERY_LIMIT: Duration = Duration::from_secs(30);
/// Transaction IDs before wraparound (2^31).
const XID_LIMIT: f64 = 2_147_483_648.0;
/// Separator of the column lists the queries return.
const SEP: char = '\u{1f}';

fn q(schema: &str, name: &str) -> String {
    qualified_name(Quote::Double, Some(schema), name)
}

fn get(r: &SimpleQueryRow, i: usize) -> String {
    r.get(i).unwrap_or_default().to_string()
}

fn num(r: &SimpleQueryRow, i: usize) -> i64 {
    r.get(i).and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v as i64).unwrap_or(0)
}

fn truthy(r: &SimpleQueryRow, i: usize) -> bool {
    matches!(r.get(i), Some("t" | "true" | "1"))
}

/// Which checks a variant's catalog can answer.
struct Applies {
    /// PostgreSQL's MVCC and its statistics views.
    maintenance: bool,
    wraparound: bool,
    unused: bool,
    invalid: bool,
    fk: bool,
    no_pk: bool,
    sequences: bool,
    duplicates: bool,
}

fn applies(v: Variant, version: i32) -> Applies {
    let pg = v.has_pg_catalog() && !matches!(v, Variant::CrateDb | Variant::RisingWave | Variant::Materialize | Variant::Yellowbrick);
    let crdb = v == Variant::Cockroach;
    let mvcc = pg && !crdb && v != Variant::Yugabyte && !v.mpp();
    Applies {
        maintenance: mvcc,
        wraparound: pg && !crdb && v != Variant::Yugabyte && v != Variant::OpenGauss,
        unused: mvcc,
        invalid: pg && !crdb,
        fk: pg && !v.mpp(),
        no_pk: pg && !v.mpp(),
        sequences: pg && !crdb && (version == 0 || version >= 100000),
        duplicates: pg,
    }
}

impl PgSession {
    /// A check's rows; `None` (and a debug line) when the query fails.
    async fn check_rows(&self, what: &str, sql: &str) -> Option<Vec<SimpleQueryRow>> {
        match self.text_within(sql, QUERY_LIMIT).await {
            Ok(rows) => Some(rows),
            Err(e) => {
                tracing::debug!("{:?}: health check {what} skipped: {e}", self.variant);
                None
            }
        }
    }

    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let v = self.variant;
        let mut out = Vec::new();
        if v == Variant::Redshift {
            self.redshift_checks(&mut out).await;
        } else {
            let a = applies(v, self.version);
            if a.maintenance {
                self.maintenance_checks(&mut out).await;
            }
            if a.wraparound {
                self.wraparound_check(database, &mut out).await;
            }
            if a.unused {
                self.unused_indexes(&mut out).await;
            }
            if v == Variant::Cockroach {
                self.cockroach_checks(&mut out).await;
            }
            if a.invalid {
                self.invalid_indexes(&mut out).await;
            }
            if a.fk {
                self.fk_without_index(&mut out).await;
            }
            if a.no_pk {
                self.tables_without_pk(&mut out).await;
            }
            if a.sequences {
                self.sequences_near_limit(&mut out).await;
            }
            if a.duplicates {
                self.duplicate_indexes(&mut out).await;
            }
        }
        for c in &mut out {
            c.objects.truncate(MAX_OBJECTS);
        }
        Ok(out)
    }

    /// Autovacuum (globally and per table), dead tuples, never analyzed.
    async fn maintenance_checks(&self, out: &mut Vec<HealthCheck>) {
        if let Some(rows) = self.check_rows("autovacuum", "SELECT current_setting('autovacuum'), current_setting('track_counts')").await {
            if let Some(r) = rows.first() {
                let on = get(r, 0) == "on" && get(r, 1) == "on";
                let mut c = HealthCheck::new(
                    "autovacuum",
                    "Configuración",
                    if on { "Autovacuum está activo" } else { "Autovacuum está apagado" },
                    if on { Severity::Ok } else { Severity::Critical },
                )
                .detail(
                    "Sin autovacuum las filas muertas no se limpian, las estadísticas envejecen y la base se acerca al límite de transacciones (wraparound). \
                     Necesita también track_counts. En un servicio administrado se cambia en los parámetros de la instancia.",
                );
                if !on {
                    c = c.fix("ALTER SYSTEM SET autovacuum = on;\nALTER SYSTEM SET track_counts = on;\nSELECT pg_reload_conf();");
                }
                out.push(c);
            }
        }

        let filter = self.filter("n.nspname");
        if let Some(rows) = self
            .check_rows(
                "autovacuum per table",
                &format!(
                    "SELECT n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                     WHERE c.relkind IN ('r', 'm') AND {filter}
                       AND EXISTS (SELECT 1 FROM unnest(c.reloptions) o
                                   WHERE lower(o) ~ '^autovacuum_enabled=(f|fa|fal|fals|false|n|no|of|off|0)$')
                     ORDER BY 1, 2 LIMIT {MAX_OBJECTS}"
                ),
            )
            .await
        {
            if !rows.is_empty() {
                let objects: Vec<String> = rows.iter().map(|r| format!("{}.{}", get(r, 0), get(r, 1))).collect();
                let fixes: Vec<String> = rows.iter().map(|r| format!("ALTER TABLE {} RESET (autovacuum_enabled);", q(&get(r, 0), &get(r, 1)))).collect();
                out.push(
                    HealthCheck::new("autovacuum_tables", "Configuración", format!("{} tablas con autovacuum desactivado", objects.len()), Severity::Warning)
                        .detail("Estas tablas tienen autovacuum_enabled = off: nadie limpia sus filas muertas ni actualiza sus estadísticas, salvo un VACUUM manual.")
                        .objects(objects)
                        .fix(fixes.join("\n")),
                );
            }
        }

        let filter = self.filter("schemaname");
        if let Some(rows) = self
            .check_rows(
                "dead tuples",
                &format!(
                    "SELECT schemaname, relname, n_dead_tup, n_live_tup FROM pg_stat_user_tables
                     WHERE n_dead_tup >= 1000 AND n_dead_tup > 0.2 * n_live_tup AND {filter}
                     ORDER BY n_dead_tup DESC LIMIT {MAX_OBJECTS}"
                ),
            )
            .await
        {
            let objects: Vec<String> =
                rows.iter().map(|r| format!("{}.{} ({} filas muertas, {} vivas)", get(r, 0), get(r, 1), get(r, 2), get(r, 3))).collect();
            let fixes: Vec<String> = rows.iter().map(|r| format!("VACUUM ANALYZE {};", q(&get(r, 0), &get(r, 1)))).collect();
            let mut c = HealthCheck::new(
                "dead_tuples",
                "Espacio",
                if objects.is_empty() { "Sin tablas con muchas filas muertas".to_string() } else { format!("{} tablas con muchas filas muertas", objects.len()) },
                if objects.is_empty() { Severity::Ok } else { Severity::Warning },
            )
            .detail(
                "Más de un 20 % de filas muertas: la tabla ocupa más de lo que necesita y las lecturas recorren filas que ya no existen. \
                 Si se repite, autovacuum no alcanza: revisá transacciones largas o bajá autovacuum_vacuum_scale_factor de esas tablas.",
            )
            .objects(objects);
            if !fixes.is_empty() {
                c = c.fix(fixes.join("\n"));
            }
            out.push(c);
        }

        if let Some(rows) = self
            .check_rows(
                "never analyzed",
                &format!(
                    "SELECT schemaname, relname, n_live_tup FROM pg_stat_user_tables
                     WHERE last_analyze IS NULL AND last_autoanalyze IS NULL AND n_live_tup >= 1000 AND {filter}
                     ORDER BY n_live_tup DESC LIMIT {MAX_OBJECTS}"
                ),
            )
            .await
        {
            if !rows.is_empty() {
                let objects: Vec<String> = rows.iter().map(|r| format!("{}.{} ({} filas)", get(r, 0), get(r, 1), get(r, 2))).collect();
                let fixes: Vec<String> = rows.iter().map(|r| format!("VACUUM ANALYZE {};", q(&get(r, 0), &get(r, 1)))).collect();
                out.push(
                    HealthCheck::new("never_analyzed", "Rendimiento", format!("{} tablas con filas que nunca se analizaron", objects.len()), Severity::Warning)
                        .detail("Sin estadísticas el optimizador supone cantidades de filas y puede elegir planes lentos.")
                        .objects(objects)
                        .fix(fixes.join("\n")),
                );
            }
        }
    }

    /// Transaction ID age of the database against the wraparound limit.
    async fn wraparound_check(&self, database: &str, out: &mut Vec<HealthCheck>) {
        let Some(rows) = self
            .check_rows(
                "wraparound",
                &format!(
                    "SELECT age(datfrozenxid)::text, current_setting('autovacuum_freeze_max_age') FROM pg_database WHERE datname = {}",
                    lit(self.variant, database)
                ),
            )
            .await
        else {
            return;
        };
        let Some(r) = rows.first() else { return };
        let age = num(r, 0);
        let freeze_max = num(r, 1).max(100_000);
        let pct = age as f64 / XID_LIMIT * 100.0;
        let sev = if age >= 1_500_000_000 {
            Severity::Critical
        } else if age > freeze_max * 2 {
            Severity::Warning
        } else {
            Severity::Ok
        };
        let mut c = HealthCheck::new(
            "xid_wraparound",
            "Mantenimiento",
            format!("Edad de transacciones: {} millones ({pct:.0} % del límite)", age / 1_000_000),
            sev,
        )
        .detail(format!(
            "Al llegar a unos 2.100 millones PostgreSQL deja de aceptar escrituras para no perder datos. Autovacuum congela las filas a partir de \
             autovacuum_freeze_max_age ({} millones): si la edad pasa del doble, no está alcanzando (transacciones largas, slots de replicación o \
             transacciones preparadas viejas lo frenan).",
            freeze_max / 1_000_000
        ));
        if sev != Severity::Ok {
            let filter = self.filter("n.nspname");
            if let Some(rows) = self
                .check_rows(
                    "oldest tables",
                    &format!(
                        "SELECT n.nspname, c.relname, age(c.relfrozenxid)::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                         WHERE c.relkind IN ('r', 'm') AND {filter} ORDER BY age(c.relfrozenxid) DESC LIMIT 20"
                    ),
                )
                .await
            {
                c = c.objects(rows.iter().map(|r| format!("{}.{} (edad {} millones)", get(r, 0), get(r, 1), num(r, 2) / 1_000_000)).collect());
                let fixes: Vec<String> = rows.iter().map(|r| format!("VACUUM FREEZE {};", q(&get(r, 0), &get(r, 1)))).collect();
                if !fixes.is_empty() {
                    c = c.fix(format!("-- Las tablas más viejas primero; puede tardar en tablas grandes.\n{}", fixes.join("\n")));
                }
            }
        }
        out.push(c);
    }

    /// Indexes never scanned, with the window the counters cover.
    async fn unused_indexes(&self, out: &mut Vec<HealthCheck>) {
        let window = self
            .check_rows(
                "stats window",
                "SELECT (EXTRACT(EPOCH FROM now() - COALESCE(stats_reset, pg_postmaster_start_time())) / 86400)::bigint::text,
                        (stats_reset IS NOT NULL)::text
                 FROM pg_stat_database WHERE datname = current_database()",
            )
            .await
            .and_then(|r| r.first().map(|r| (num(r, 0), truthy(r, 1))));
        let filter = self.filter("s.schemaname");
        let Some(rows) = self
            .check_rows(
                "unused indexes",
                &format!(
                    "SELECT s.schemaname, s.relname, s.indexrelname, pg_size_pretty(pg_relation_size(s.indexrelid))
                     FROM pg_stat_user_indexes s JOIN pg_index i ON i.indexrelid = s.indexrelid
                     WHERE s.idx_scan = 0 AND NOT i.indisunique AND NOT i.indisprimary AND {filter}
                       AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = s.indexrelid)
                       AND NOT EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhparent = s.relid)
                     ORDER BY pg_relation_size(s.indexrelid) DESC LIMIT {MAX_OBJECTS}"
                ),
            )
            .await
        else {
            return;
        };
        let objects = rows.iter().map(|r| format!("{}.{} · {} ({})", get(r, 0), get(r, 1), get(r, 2), get(r, 3))).collect();
        let drops = rows.iter().map(|r| format!("-- DROP INDEX {};", q(&get(r, 0), &get(r, 2)))).collect();
        let since = match window {
            Some((d, true)) => Some(format!("los contadores cubren {d} días, desde que se reiniciaron las estadísticas")),
            Some((d, false)) => Some(format!("los contadores cubren al menos {d} días, desde el arranque del servidor")),
            None => None,
        };
        out.push(unused_check(objects, drops, window.map(|w| w.0), since));
    }

    /// CockroachDB: automatic statistics and unused indexes.
    async fn cockroach_checks(&self, out: &mut Vec<HealthCheck>) {
        if let Some(rows) = self.check_rows("auto stats", "SHOW CLUSTER SETTING sql.stats.automatic_collection.enabled").await {
            if let Some(r) = rows.first() {
                let on = truthy(r, 0);
                let mut c = HealthCheck::new(
                    "auto_stats",
                    "Configuración",
                    if on { "La recolección automática de estadísticas está activa" } else { "La recolección automática de estadísticas está apagada" },
                    if on { Severity::Ok } else { Severity::Warning },
                )
                .detail("Sin estadísticas al día el optimizador estima mal las filas y elige planes lentos.");
                if !on {
                    c = c.fix("SET CLUSTER SETTING sql.stats.automatic_collection.enabled = true;");
                }
                out.push(c);
            }
        }

        // Since v25 crdb_internal is closed unless the session opts in.
        let _ = self.client.batch_execute("SET allow_unsafe_internals = true").await;
        let window = self
            .check_rows("node start", "SELECT (EXTRACT(EPOCH FROM now() - max(started_at)) / 86400)::INT8::STRING FROM crdb_internal.gossip_nodes")
            .await
            .and_then(|r| r.first().and_then(|r| r.get(0).map(|_| num(r, 0))));
        let rows = self
            .check_rows(
                "unused indexes",
                &format!(
                    "SELECT t.schema_name, t.name, ti.index_name
                     FROM crdb_internal.table_indexes ti
                     JOIN crdb_internal.tables t ON t.table_id = ti.descriptor_id AND t.database_name = current_database() AND t.drop_time IS NULL
                     LEFT JOIN crdb_internal.index_usage_statistics u ON u.table_id = ti.descriptor_id AND u.index_id = ti.index_id
                     WHERE ti.index_type = 'secondary' AND NOT ti.is_unique AND COALESCE(u.total_reads, 0) = 0
                     ORDER BY 1, 2, 3 LIMIT {MAX_OBJECTS}"
                ),
            )
            .await;
        let _ = self.client.batch_execute("RESET allow_unsafe_internals").await;
        let Some(rows) = rows else { return };
        let objects = rows.iter().map(|r| format!("{}.{} · {}", get(r, 0), get(r, 1), get(r, 2))).collect();
        let drops = rows.iter().map(|r| format!("-- DROP INDEX {}@{};", q(&get(r, 0), &get(r, 1)), quote_ident(Quote::Double, &get(r, 2)))).collect();
        let since = window.map(|d| format!("los contadores cubren {d} días, desde el último arranque de un nodo"));
        out.push(unused_check(objects, drops, window, since));
    }

    /// Indexes a failed CREATE INDEX CONCURRENTLY (or REINDEX) left invalid.
    async fn invalid_indexes(&self, out: &mut Vec<HealthCheck>) {
        let filter = self.filter("n.nspname");
        let Some(rows) = self
            .check_rows(
                "invalid indexes",
                &format!(
                    "SELECT n.nspname, t.relname, ic.relname FROM pg_index i
                     JOIN pg_class ic ON ic.oid = i.indexrelid JOIN pg_class t ON t.oid = i.indrelid
                     JOIN pg_namespace n ON n.oid = ic.relnamespace
                     WHERE NOT i.indisvalid AND {filter} ORDER BY 1, 2, 3 LIMIT {MAX_OBJECTS}"
                ),
            )
            .await
        else {
            return;
        };
        let objects: Vec<String> = rows.iter().map(|r| format!("{}.{} · {}", get(r, 0), get(r, 1), get(r, 2))).collect();
        let fixes: Vec<String> = rows.iter().map(|r| format!("REINDEX INDEX {};", q(&get(r, 0), &get(r, 2)))).collect();
        let mut c = HealthCheck::new(
            "invalid_indexes",
            "Rendimiento",
            if objects.is_empty() { "No hay índices inválidos".to_string() } else { format!("{} índices inválidos", objects.len()) },
            if objects.is_empty() { Severity::Ok } else { Severity::Warning },
        )
        .detail(
            "Un CREATE INDEX CONCURRENTLY o REINDEX que falló deja el índice inválido: no se usa en las consultas pero se sigue actualizando en cada escritura. \
             Si es un índice único, corregí antes los duplicados. Un índice que se está creando en este momento también aparece como inválido.",
        )
        .objects(objects);
        if !fixes.is_empty() {
            c = c.fix(fixes.join("\n"));
        }
        out.push(c);
    }

    /// Foreign keys whose leading column no index starts with.
    async fn fk_without_index(&self, out: &mut Vec<HealthCheck>) {
        let filter = self.filter("n.nspname");
        // Partitions repeat their parent's foreign key (conparentid, 11+).
        let own = if self.version >= 110000 && self.variant != Variant::Cockroach { "AND c.conparentid = 0" } else { "" };
        let Some(rows) = self
            .check_rows(
                "fk without index",
                &format!(
                    "SELECT n.nspname, t.relname, c.conname,
                            array_to_string(ARRAY(SELECT a.attname::text FROM generate_subscripts(c.conkey, 1) k
                                                  JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[k] ORDER BY k), chr(31))
                     FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace
                     WHERE c.contype = 'f' AND t.relkind IN ('r', 'p') {own} AND {filter}
                       AND NOT EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.conrelid AND i.indkey[0] = c.conkey[1])
                     ORDER BY 1, 2, 3 LIMIT {MAX_OBJECTS}"
                ),
            )
            .await
        else {
            return;
        };
        let cols = |r: &SimpleQueryRow| -> Vec<String> { get(r, 3).split(SEP).filter(|c| !c.is_empty()).map(str::to_string).collect() };
        let objects: Vec<String> = rows.iter().map(|r| format!("{}.{} ({}) · {}", get(r, 0), get(r, 1), cols(r).join(", "), get(r, 2))).collect();
        let fixes: Vec<String> = rows
            .iter()
            .map(|r| {
                let list = cols(r).iter().map(|c| quote_ident(Quote::Double, c)).collect::<Vec<_>>().join(", ");
                format!("CREATE INDEX ON {} ({list});", q(&get(r, 0), &get(r, 1)))
            })
            .collect();
        let mut c = HealthCheck::new(
            "fk_without_index",
            "Rendimiento",
            if objects.is_empty() { "Todas las claves foráneas tienen índice".to_string() } else { format!("{} claves foráneas sin índice", objects.len()) },
            if objects.is_empty() { Severity::Ok } else { Severity::Info },
        )
        .detail("Sin índice, borrar o actualizar en la tabla padre recorre la tabla hija entera (y la bloquea más tiempo), y los joins por esa columna son más lentos.")
        .objects(objects);
        if !fixes.is_empty() {
            c = c.fix(fixes.join("\n"));
        }
        out.push(c);
    }

    /// Tables without a primary key.
    async fn tables_without_pk(&self, out: &mut Vec<HealthCheck>) {
        let sql = if self.variant == Variant::Cockroach {
            // Without one, CockroachDB keys the table on a hidden rowid.
            format!(
                "SELECT table_schema, table_name FROM information_schema.columns
                 WHERE table_catalog = current_database() AND column_name = 'rowid' AND is_hidden = 'YES' AND {}
                 ORDER BY 1, 2 LIMIT {MAX_OBJECTS}",
                self.filter("table_schema")
            )
        } else {
            let (kinds, partitions) = if self.version >= 100000 { ("'r', 'p'", "AND NOT c.relispartition") } else { ("'r'", "") };
            format!(
                "SELECT n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE c.relkind IN ({kinds}) {partitions} AND {}
                   AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid = c.oid AND k.contype = 'p')
                 ORDER BY 1, 2 LIMIT {MAX_OBJECTS}",
                self.filter("n.nspname")
            )
        };
        let Some(rows) = self.check_rows("tables without pk", &sql).await else { return };
        let objects: Vec<String> = rows.iter().map(|r| format!("{}.{}", get(r, 0), get(r, 1))).collect();
        out.push(
            HealthCheck::new(
                "no_primary_key",
                "Diseño",
                if objects.is_empty() { "Todas las tablas tienen clave primaria".to_string() } else { format!("{} tablas sin clave primaria", objects.len()) },
                if objects.is_empty() { Severity::Ok } else { Severity::Info },
            )
            .detail(
                "Sin clave primaria nada impide filas duplicadas, la replicación lógica no puede publicar sus UPDATE y DELETE, \
                 y las herramientas (incluida la edición de datos de DBine) no pueden identificar una fila.",
            )
            .objects(objects),
        );
    }

    /// Sequences that used most of their range (and don't cycle).
    async fn sequences_near_limit(&self, out: &mut Vec<HealthCheck>) {
        let filter = self.filter("s.schemaname");
        let Some(rows) = self
            .check_rows(
                "sequences",
                &format!(
                    "SELECT s.schemaname, s.sequencename, s.last_value::text, s.min_value::text, s.max_value::text, s.increment_by::text,
                            s.data_type::text, tn.nspname, t.relname, a.attname, format_type(a.atttypid, a.atttypmod)
                     FROM pg_sequences s
                     JOIN pg_namespace sn ON sn.nspname = s.schemaname
                     JOIN pg_class sc ON sc.relnamespace = sn.oid AND sc.relname = s.sequencename
                     LEFT JOIN pg_depend d ON d.objid = sc.oid AND d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass
                                          AND d.deptype IN ('a', 'i') AND d.refobjsubid > 0
                     LEFT JOIN pg_class t ON t.oid = d.refobjid
                     LEFT JOIN pg_namespace tn ON tn.oid = t.relnamespace
                     LEFT JOIN pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid
                     WHERE NOT s.cycle AND s.last_value IS NOT NULL AND {filter}"
                ),
            )
            .await
        else {
            return;
        };
        let mut found: Vec<(f64, String, Vec<String>)> = Vec::new();
        for r in &rows {
            let parse = |i: usize| r.get(i).and_then(|v| v.parse::<f64>().ok());
            let (Some(last), Some(min), Some(max), Some(inc)) = (parse(2), parse(3), parse(4), parse(5)) else { continue };
            let used = if inc >= 0.0 { (last - min) / (max - min) } else { (max - last) / (max - min) };
            if !used.is_finite() || used < 0.75 {
                continue;
            }
            let (schema, name) = (get(r, 0), get(r, 1));
            let mut fix = Vec::new();
            let col_type = get(r, 10);
            if r.get(9).is_some() && matches!(col_type.as_str(), "integer" | "smallint") {
                fix.push(format!("ALTER TABLE {} ALTER COLUMN {} TYPE bigint;", q(&get(r, 7), &get(r, 8)), quote_ident(Quote::Double, &get(r, 9))));
            }
            if matches!(get(r, 6).as_str(), "integer" | "smallint") {
                fix.push(format!("ALTER SEQUENCE {} AS bigint;", q(&schema, &name)));
            }
            let owner = r.get(9).map(|c| format!(", de {}.{}.{c}", get(r, 7), get(r, 8))).unwrap_or_default();
            found.push((used, format!("{schema}.{name} ({:.0} % usado{owner})", used * 100.0), fix));
        }
        found.sort_by(|a, b| b.0.total_cmp(&a.0));
        let worst = found.first().map_or(0.0, |f| f.0);
        let objects: Vec<String> = found.iter().map(|f| f.1.clone()).collect();
        let fixes: Vec<String> = found.iter().flat_map(|f| f.2.clone()).collect();
        let mut c = HealthCheck::new(
            "sequences_near_limit",
            "Integridad",
            if objects.is_empty() { "Ninguna secuencia está cerca de su límite".to_string() } else { format!("{} secuencias usaron más del 75 % de su rango", objects.len()) },
            if objects.is_empty() {
                Severity::Ok
            } else if worst >= 0.9 {
                Severity::Critical
            } else {
                Severity::Warning
            },
        )
        .detail(
            "Cuando una secuencia llega a su máximo, cada INSERT que la usa falla. Pasar la columna y la secuencia a bigint lo resuelve, \
             pero cambiar el tipo de la columna reescribe la tabla y la bloquea mientras tanto: planificalo.",
        )
        .objects(objects);
        if !fixes.is_empty() {
            c = c.fix(fixes.join("\n"));
        }
        out.push(c);
    }

    /// Indexes with the same definition as another one of the same table.
    async fn duplicate_indexes(&self, out: &mut Vec<HealthCheck>) {
        let filter = self.filter("n.nspname");
        let key_atts = if self.version >= 110000 && self.variant != Variant::OpenGauss { "AND a.indnkeyatts = b.indnkeyatts" } else { "" };
        let keeps = |x: &str| format!("({x}.indisprimary OR {x}.indisunique OR EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = {x}.indexrelid))::text");
        let Some(rows) = self
            .check_rows(
                "duplicate indexes",
                &format!(
                    "SELECT n.nspname, t.relname, ia.relname, ib.relname, {}, {}
                     FROM pg_index a
                     JOIN pg_index b ON b.indrelid = a.indrelid AND b.indexrelid > a.indexrelid
                          AND a.indkey::text = b.indkey::text AND a.indclass::text = b.indclass::text
                          AND a.indcollation::text = b.indcollation::text AND a.indnatts = b.indnatts {key_atts}
                          AND COALESCE(pg_get_expr(a.indexprs, a.indrelid), '') = COALESCE(pg_get_expr(b.indexprs, b.indrelid), '')
                          AND COALESCE(pg_get_expr(a.indpred, a.indrelid), '') = COALESCE(pg_get_expr(b.indpred, b.indrelid), '')
                     JOIN pg_class ia ON ia.oid = a.indexrelid
                     JOIN pg_class ib ON ib.oid = b.indexrelid AND ib.relam = ia.relam
                     JOIN pg_class t ON t.oid = a.indrelid AND t.relkind IN ('r', 'p', 'm')
                     JOIN pg_namespace n ON n.oid = t.relnamespace
                     WHERE {filter} ORDER BY 1, 2, 3 LIMIT {MAX_OBJECTS}",
                    keeps("a"),
                    keeps("b")
                ),
            )
            .await
        else {
            return;
        };
        let crdb = self.variant == Variant::Cockroach;
        let mut objects = Vec::new();
        let mut fixes = Vec::new();
        for r in &rows {
            let (schema, table, ia, ib) = (get(r, 0), get(r, 1), get(r, 2), get(r, 3));
            objects.push(format!("{schema}.{table} · {ia} = {ib}"));
            // Drop the one no constraint needs; both needed: nothing to drop.
            let drop = match (truthy(r, 4), truthy(r, 5)) {
                (_, false) => Some(&ib),
                (false, true) => Some(&ia),
                (true, true) => None,
            };
            if let Some(ix) = drop {
                fixes.push(if crdb {
                    format!("DROP INDEX {}@{};", q(&schema, &table), quote_ident(Quote::Double, ix))
                } else {
                    format!("DROP INDEX {};", q(&schema, ix))
                });
            }
        }
        let mut c = HealthCheck::new(
            "duplicate_indexes",
            "Rendimiento",
            if objects.is_empty() { "No hay índices duplicados".to_string() } else { format!("{} pares de índices duplicados", objects.len()) },
            if objects.is_empty() { Severity::Ok } else { Severity::Warning },
        )
        .detail("Dos índices con la misma definición: el segundo no acelera nada y cuesta espacio y tiempo en cada escritura.")
        .objects(objects);
        if !fixes.is_empty() {
            c = c.fix(format!("-- Se conserva el índice de la clave primaria o restricción, si lo hay.\n{}", fixes.join("\n")));
        }
        out.push(c);
    }

    /// Redshift: stale statistics and unsorted rows, from `svv_table_info`.
    async fn redshift_checks(&self, out: &mut Vec<HealthCheck>) {
        let Some(rows) = self
            .check_rows(
                "svv_table_info",
                "SELECT \"schema\", \"table\", COALESCE(stats_off, 0)::varchar, COALESCE(unsorted, 0)::varchar, COALESCE(tbl_rows, 0)::varchar
                 FROM svv_table_info WHERE \"database\" = current_database()
                 ORDER BY size DESC",
            )
            .await
        else {
            return;
        };
        let pct = |r: &SimpleQueryRow, i: usize| r.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let stale: Vec<&SimpleQueryRow> = rows.iter().filter(|r| pct(r, 2) > 10.0).take(MAX_OBJECTS).collect();
        let mut c = HealthCheck::new(
            "stale_stats",
            "Rendimiento",
            if stale.is_empty() { "Estadísticas al día".to_string() } else { format!("{} tablas con estadísticas desactualizadas", stale.len()) },
            if stale.is_empty() { Severity::Ok } else { Severity::Warning },
        )
        .detail("stats_off mayor a 10 %: el planificador estima mal las filas y puede elegir distribuciones o joins lentos.")
        .objects(stale.iter().map(|r| format!("{}.{} ({:.0} % desactualizadas)", get(r, 0), get(r, 1), pct(r, 2))).collect());
        if !stale.is_empty() {
            c = c.fix(stale.iter().map(|r| format!("ANALYZE {};", q(&get(r, 0), &get(r, 1)))).collect::<Vec<_>>().join("\n"));
        }
        out.push(c);

        let unsorted: Vec<&SimpleQueryRow> = rows.iter().filter(|r| pct(r, 3) > 20.0 && num(r, 4) >= 100_000).take(MAX_OBJECTS).collect();
        let mut c = HealthCheck::new(
            "unsorted_rows",
            "Espacio",
            if unsorted.is_empty() { "Sin tablas con muchas filas desordenadas".to_string() } else { format!("{} tablas con más de 20 % de filas sin ordenar", unsorted.len()) },
            if unsorted.is_empty() { Severity::Ok } else { Severity::Warning },
        )
        .detail("Las filas fuera del orden de la sort key obligan a leer más bloques. VACUUM las reordena y recupera el espacio de las borradas.")
        .objects(unsorted.iter().map(|r| format!("{}.{} ({:.0} % sin ordenar)", get(r, 0), get(r, 1), pct(r, 3))).collect());
        if !unsorted.is_empty() {
            c = c.fix(unsorted.iter().map(|r| format!("VACUUM {};", q(&get(r, 0), &get(r, 1)))).collect::<Vec<_>>().join("\n"));
        }
        out.push(c);
    }
}

/// The unused-indexes finding: conclusive (and with a commented DROP per
/// index) only when the counters cover [`MIN_WINDOW_DAYS`].
fn unused_check(objects: Vec<String>, drops: Vec<String>, window: Option<i64>, since: Option<String>) -> HealthCheck {
    let conclusive = window.is_some_and(|d| d >= MIN_WINDOW_DAYS);
    let since = since.unwrap_or_else(|| "el servidor no dice desde cuándo cuenta".to_string());
    let (title, sev) = if objects.is_empty() {
        ("Todos los índices se usaron".to_string(), Severity::Ok)
    } else if conclusive {
        (format!("{} índices sin lecturas ({since})", objects.len()), Severity::Warning)
    } else {
        (format!("No concluyente: {} índices sin lecturas, pero {since}", objects.len()), Severity::Info)
    };
    let mut c = HealthCheck::new("unused_indexes", "Rendimiento", title, sev)
        .detail(
            "Un índice que nunca se lee solo cuesta en cada escritura. Antes de borrarlo, tené en cuenta procesos de fin de mes, reportes ocasionales, \
             índices creados hace poco y que cada réplica cuenta sus lecturas por separado.",
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

    #[test]
    fn checks_by_variant() {
        let pg = applies(Variant::Postgres, 160000);
        assert!(pg.maintenance && pg.wraparound && pg.unused && pg.invalid && pg.fk && pg.no_pk && pg.sequences && pg.duplicates);
        let crdb = applies(Variant::Cockroach, 130000);
        assert!(!crdb.maintenance && !crdb.wraparound && !crdb.unused && !crdb.invalid && !crdb.sequences);
        assert!(crdb.fk && crdb.no_pk && crdb.duplicates);
        let gp = applies(Variant::Greenplum, 90426);
        assert!(!gp.maintenance && gp.wraparound && !gp.fk && !gp.no_pk && !gp.sequences && gp.invalid);
        assert!(!applies(Variant::OpenGauss, 90204).wraparound && !applies(Variant::OpenGauss, 90204).sequences);
        assert!(!applies(Variant::Yugabyte, 110002).maintenance && applies(Variant::Yugabyte, 110002).invalid);
        for v in [Variant::Denodo, Variant::H2, Variant::CrateDb, Variant::RisingWave, Variant::Materialize, Variant::Yellowbrick, Variant::Redshift] {
            let a = applies(v, 0);
            assert!(!a.maintenance && !a.duplicates && !a.no_pk && !a.invalid, "{v:?}");
        }
    }

    #[test]
    fn unused_is_conclusive_only_with_a_long_window() {
        let objs = || vec!["public.t · ix".to_string()];
        let drops = || vec!["-- DROP INDEX \"public\".\"ix\";".to_string()];
        let short = unused_check(objs(), drops(), Some(3), Some("3 días".into()));
        assert_eq!(short.severity, Severity::Info);
        assert!(short.title.starts_with("No concluyente") && short.fix.is_none());
        let long = unused_check(objs(), drops(), Some(30), Some("30 días".into()));
        assert_eq!(long.severity, Severity::Warning);
        assert!(long.fix.unwrap().contains("-- DROP INDEX"));
        assert_eq!(unused_check(objs(), drops(), None, None).severity, Severity::Info);
        assert_eq!(unused_check(vec![], vec![], Some(30), None).severity, Severity::Ok);
    }
}

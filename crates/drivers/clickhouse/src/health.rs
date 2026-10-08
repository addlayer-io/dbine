//! "Chequeo de salud" findings of ClickHouse ([`dbine_driver::Session::health_checks`]):
//! partitions with too many active parts, detached or broken parts,
//! replicas that lag or went read-only (`system.replicas`), mutations that
//! fail or never finish, and large tables with dates but no TTL (only as
//! information). Each check is its own query on the `system` tables (no
//! data is read): one that fails (an older server, Timeplus, no access) is
//! skipped.

use crate::{text, ClickHouseSession};
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::Result;
use serde_json::Value;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;

/// The server defaults when `system.merge_tree_settings` can't be read.
const DELAY_INSERT: f64 = 1000.0;
const THROW_INSERT: f64 = 3000.0;

/// Hours a mutation may run before it counts as stuck.
const STUCK_HOURS: i64 = 1;

/// Tables from this size up are worth a TTL thought.
const TTL_BYTES: u64 = 10 * 1024 * 1024 * 1024;

fn s(r: &[Value], i: usize) -> String {
    r.get(i).map(text).unwrap_or_default()
}

fn n(r: &[Value], i: usize) -> f64 {
    s(r, i).trim().parse().unwrap_or(0.0)
}

fn qn(db: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(Quote::Backtick, db), quote_ident(Quote::Backtick, table))
}

/// A ClickHouse string literal.
fn lit(v: &str) -> String {
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Severity of a partition with `parts` active parts, against the
/// table-level limits where inserts slow down and fail.
pub(crate) fn parts_severity(parts: f64, delay: f64, throw: f64) -> Severity {
    if parts >= delay || parts >= throw * 0.8 {
        Severity::Critical
    } else if parts >= delay / 2.0 {
        Severity::Warning
    } else {
        Severity::Ok
    }
}

/// Detached parts the server set aside on its own (not by DETACH).
pub(crate) fn is_broken(reason: &str) -> bool {
    reason.starts_with("broken") || matches!(reason, "unexpected" | "noquorum" | "ignored" | "covered-by-broken" | "tmp-fetch")
}

const PARTS: &str = "SELECT table, partition, count() AS c FROM system.parts
  WHERE database = {db:String} AND active
  GROUP BY table, partition HAVING c >= 50
  ORDER BY c DESC LIMIT 200";

const DETACHED: &str = "SELECT table, name, ifNull(reason, '') FROM system.detached_parts
  WHERE database = {db:String} ORDER BY table, name LIMIT 1000";

const REPLICAS: &str = "SELECT table, is_readonly, is_session_expired, absolute_delay, queue_size, inserts_in_queue,
       active_replicas, total_replicas
  FROM system.replicas WHERE database = {db:String} ORDER BY table";

const MUTATIONS: &str = "SELECT table, mutation_id, command, toString(create_time), latest_fail_reason,
       dateDiff('hour', create_time, now()), parts_to_do
  FROM system.mutations
  WHERE database = {db:String} AND NOT is_done
    AND (latest_fail_reason != '' OR create_time < now() - toIntervalHour({hours:UInt32}))
  ORDER BY create_time LIMIT 200";

const NO_TTL: &str = "SELECT name, formatReadableSize(total_bytes) FROM system.tables
  WHERE database = {db:String} AND engine LIKE '%MergeTree' AND total_bytes >= {bytes:UInt64}
    AND position(engine_full, ' TTL ') = 0
    AND name IN (SELECT table FROM system.columns WHERE database = {db:String} AND type LIKE '%Date%')
  ORDER BY total_bytes DESC LIMIT 200";

impl ClickHouseSession {
    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let db = if database.is_empty() { self.database.clone() } else { database.to_string() };
        let p = [("db", db.as_str())];
        let mut out = Vec::new();

        // Too many active parts per partition: inserts slow down, then fail.
        let (mut delay, mut throw) = (DELAY_INSERT, THROW_INSERT);
        if let Ok(rows) = self
            .rows("SELECT name, value FROM system.merge_tree_settings WHERE name IN ('parts_to_delay_insert', 'parts_to_throw_insert')", &[])
            .await
        {
            for r in rows {
                match s(&r, 0).as_str() {
                    "parts_to_delay_insert" => delay = n(&r, 1).max(1.0),
                    "parts_to_throw_insert" => throw = n(&r, 1).max(1.0),
                    _ => {}
                }
            }
        }
        if let Ok(rows) = self.rows(PARTS, &p).await {
            let bad: Vec<&Vec<Value>> = rows.iter().filter(|r| parts_severity(n(r, 2), delay, throw) != Severity::Ok).collect();
            let sev = bad.iter().map(|r| parts_severity(n(r, 2), delay, throw)).max().unwrap_or(Severity::Ok);
            let objects: Vec<String> = bad
                .iter()
                .map(|r| {
                    let part = s(r, 1);
                    if part.is_empty() || part == "tuple()" {
                        format!("{} ({} partes)", s(r, 0), s(r, 2))
                    } else {
                        format!("{} · partición {part} ({} partes)", s(r, 0), s(r, 2))
                    }
                })
                .collect();
            let mut tables: Vec<String> = bad.iter().map(|r| s(r, 0)).collect();
            tables.dedup();
            let mut check = HealthCheck::new(
                "too_many_parts",
                "Rendimiento",
                if objects.is_empty() { "Ninguna partición tiene demasiadas partes".to_string() } else { format!("{} particiones con demasiadas partes activas", objects.len()) },
                sev,
            )
            .detail(format!(
                "Con {delay:.0} partes activas en una partición los INSERT se demoran y con {throw:.0} fallan (Too many parts). Suele venir de inserciones \
                 chicas y frecuentes, o de una clave de partición demasiado fina: agrupá las filas en lotes grandes (o usá async_insert) y revisá la partición. \
                 OPTIMIZE fuerza la fusión, pero reescribe la partición entera."
            ))
            .objects(objects);
            if !tables.is_empty() {
                check = check.fix(format!(
                    "-- Fusiona las partes; reescribe los datos, mejor fuera de horario pico:\n{}",
                    tables.iter().map(|t| format!("OPTIMIZE TABLE {};", qn(&db, t))).collect::<Vec<_>>().join("\n")
                ));
            }
            out.push(check);
        }

        // Detached parts: set aside by DETACH, or by the server because they were broken.
        if let Ok(rows) = self.rows(DETACHED, &p).await {
            let broken: Vec<&Vec<Value>> = rows.iter().filter(|r| is_broken(&s(r, 2))).collect();
            let objects: Vec<String> = rows
                .iter()
                .map(|r| {
                    let reason = s(r, 2);
                    format!("{} · {} ({})", s(r, 0), s(r, 1), if reason.is_empty() { "separada a mano".to_string() } else { reason })
                })
                .collect();
            let (title, sev) = match (rows.len(), broken.len()) {
                (0, _) => ("No hay partes separadas (detached)".to_string(), Severity::Ok),
                (t, 0) => (format!("{t} partes separadas a mano (DETACH)"), Severity::Info),
                (t, b) => (format!("{t} partes separadas, {b} rotas o inesperadas"), Severity::Warning),
            };
            let mut check = HealthCheck::new("detached_parts", "Integridad", title, sev)
                .detail(
                    "Las partes separadas no se consultan pero siguen ocupando disco en detached/. Las que el servidor apartó (broken, unexpected…) \
                     pueden ser datos que faltan en la tabla: revisalas antes de borrarlas. Las separadas a mano se vuelven a sumar con ATTACH PART.",
                )
                .objects(objects);
            if !rows.is_empty() {
                let lines: Vec<String> = rows
                    .iter()
                    .take(MAX_OBJECTS)
                    .map(|r| format!("-- ALTER TABLE {} DROP DETACHED PART {} SETTINGS allow_drop_detached = 1;", qn(&db, &s(r, 0)), lit(&s(r, 1))))
                    .collect();
                check = check.fix(format!("-- Borrar una parte separada es definitivo: revisá cada una antes.\n{}", lines.join("\n")));
            }
            out.push(check);
        }

        // Replicated tables: read-only replicas, lag and queues.
        if let Ok(rows) = self.rows(REPLICAS, &p).await {
            if !rows.is_empty() {
                let readonly: Vec<String> = rows
                    .iter()
                    .filter(|r| n(r, 1) > 0.0 || n(r, 2) > 0.0)
                    .map(|r| format!("{} (solo lectura{})", s(r, 0), if n(r, 2) > 0.0 { ", sesión de Keeper vencida" } else { "" }))
                    .collect();
                let lagging: Vec<String> = rows
                    .iter()
                    .filter(|r| n(r, 1) == 0.0 && (n(r, 3) >= 300.0 || n(r, 4) >= 100.0))
                    .map(|r| format!("{} (atraso {} s, cola {})", s(r, 0), s(r, 3), s(r, 4)))
                    .collect();
                let degraded: Vec<String> = rows
                    .iter()
                    .filter(|r| n(r, 7) > 0.0 && n(r, 6) < n(r, 7))
                    .map(|r| format!("{} ({} de {} réplicas activas)", s(r, 0), s(r, 6), s(r, 7)))
                    .collect();
                let mut objects = readonly.clone();
                objects.extend(lagging.iter().cloned());
                objects.extend(degraded.iter().cloned());
                let (title, sev) = if !readonly.is_empty() {
                    (format!("{} tablas replicadas en solo lectura", readonly.len()), Severity::Critical)
                } else if !lagging.is_empty() {
                    (format!("{} tablas replicadas atrasadas", lagging.len()), Severity::Warning)
                } else if !degraded.is_empty() {
                    (format!("{} tablas replicadas con réplicas caídas", degraded.len()), Severity::Warning)
                } else {
                    (format!("Las {} tablas replicadas están al día", rows.len()), Severity::Ok)
                };
                out.push(
                    HealthCheck::new("replication", "Replicación", title, sev)
                        .detail(
                            "Una réplica en solo lectura perdió la conexión con ClickHouse Keeper (o ZooKeeper) y rechaza los INSERT; una atrasada \
                             devuelve datos viejos. Revisá Keeper y el log del servidor; SYSTEM RESTART REPLICA reconecta la tabla.",
                        )
                        .objects(objects),
                );
            }
        }

        // Mutations that fail or never finish.
        let hours = STUCK_HOURS.to_string();
        if let Ok(rows) = self.rows(MUTATIONS, &[("db", db.as_str()), ("hours", hours.as_str())]).await {
            let failing = rows.iter().filter(|r| !s(r, 4).is_empty()).count();
            let objects: Vec<String> = rows
                .iter()
                .map(|r| {
                    let fail = s(r, 4);
                    let fail = if fail.is_empty() { String::new() } else { format!(": {}", fail.chars().take(160).collect::<String>()) };
                    let cmd = s(r, 2);
                    let cmd = cmd.strip_prefix('(').and_then(|c| c.strip_suffix(')')).unwrap_or(&cmd);
                    let age = if n(r, 5) < 1.0 { "hace menos de 1 h".to_string() } else { format!("hace {} h", s(r, 5)) };
                    format!("{} · {} ({cmd}, {age}, {} partes pendientes){fail}", s(r, 0), s(r, 1), s(r, 6))
                })
                .collect();
            let fixes: Vec<String> = rows
                .iter()
                .map(|r| format!("KILL MUTATION WHERE database = {} AND table = {} AND mutation_id = {};", lit(&db), lit(&s(r, 0)), lit(&s(r, 1))))
                .collect();
            let (title, sev) = match (rows.len(), failing) {
                (0, _) => ("No hay mutaciones trabadas".to_string(), Severity::Ok),
                (t, 0) => (format!("{t} mutaciones sin terminar hace más de {STUCK_HOURS} h"), Severity::Warning),
                (t, f) => (format!("{t} mutaciones trabadas, {f} con error"), Severity::Warning),
            };
            let mut check = HealthCheck::new("stuck_mutations", "Mantenimiento", title, sev)
                .detail(
                    "Un ALTER … UPDATE/DELETE que falla se reintenta sin fin y frena las fusiones de la tabla. Si el error es del propio comando, \
                     cancelalo con KILL MUTATION y corregilo; las partes ya cambiadas no vuelven atrás.",
                )
                .objects(objects);
            if !fixes.is_empty() {
                check = check.fix(fixes.join("\n"));
            }
            out.push(check);
        }

        // Large tables with dates and no TTL: only as information.
        let bytes = TTL_BYTES.to_string();
        if let Ok(rows) = self.rows(NO_TTL, &[("db", db.as_str()), ("bytes", bytes.as_str())]).await {
            if !rows.is_empty() {
                out.push(
                    HealthCheck::new("no_ttl", "Espacio", format!("{} tablas grandes con fechas y sin TTL", rows.len()), Severity::Info)
                        .detail(
                            "Si los datos viejos dejan de servir pasado un tiempo, un TTL los borra (o los mueve a un disco más barato) solo, \
                             en vez de crecer sin límite. Ignoralo si la tabla tiene que guardar todo el historial.",
                        )
                        .objects(rows.iter().map(|r| format!("{} ({})", s(r, 0), s(r, 1))).collect()),
                );
            }
        }

        for c in &mut out {
            c.objects.truncate(MAX_OBJECTS);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_against_the_insert_limits() {
        assert_eq!(parts_severity(100.0, 1000.0, 3000.0), Severity::Ok);
        assert_eq!(parts_severity(600.0, 1000.0, 3000.0), Severity::Warning);
        assert_eq!(parts_severity(1000.0, 1000.0, 3000.0), Severity::Critical);
        // Older servers: 150 / 300.
        assert_eq!(parts_severity(250.0, 150.0, 300.0), Severity::Critical);
    }

    #[test]
    fn broken_reasons_and_literals() {
        assert!(is_broken("broken-on-start") && is_broken("unexpected"));
        assert!(!is_broken("") && !is_broken("attaching"));
        assert_eq!(lit("a'b\\c"), "'a\\'b\\\\c'");
        assert_eq!(qn("d", "t`x"), "`d`.`t``x`");
    }
}

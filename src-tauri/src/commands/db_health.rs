//! "Chequeo de salud" of a database (docs/health-check.md): the
//! driver's own findings (`Session::health_checks`) plus the ones every
//! engine can answer from what DBine already reads: connections against
//! their limit, the cache, long queries, blocking, transactions left open,
//! and the age of the last backup. On a read-only session of its own.
//! Fix scripts only travel to the UI, which opens them in a query.

use crate::commands::schema::driver_of;
use crate::error::CommandResult;
use crate::state::AppState;
use dbine_driver::health::{HealthCheck, Severity};
use serde::{Deserialize, Serialize};
use tauri::State;

/// A statement running longer than this is "long".
const LONG_QUERY_MS: u64 = 5 * 60 * 1000;
/// Idle with a transaction open longer than this.
const IDLE_TXN_MS: u64 = 10 * 60 * 1000;
/// Days without a backup before it's a warning.
const BACKUP_DAYS: i64 = 7;

#[derive(Deserialize)]
pub struct HealthArgs {
    pub connection_id: String,
    pub database: String,
    /// The UI's id (its `cancel_query` target is `health:<id>`).
    pub run_id: String,
}

#[derive(Serialize)]
pub struct HealthReport {
    pub checks: Vec<HealthCheck>,
    /// When it ran (local time, for the header).
    pub checked_at: String,
    /// Checks that couldn't run, with why (no permission, not supported…).
    pub skipped: Vec<String>,
}

fn short(sql: &Option<String>) -> String {
    sql.as_deref().map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).map(|s| s.chars().take(120).collect()).unwrap_or_default()
}

#[tauri::command(rename_all = "camelCase")]
pub async fn database_health(state: State<'_, AppState>, args: HealthArgs) -> CommandResult<HealthReport> {
    let driver = driver_of(&state, &args.connection_id)?;
    let caps = driver.capabilities();
    let key = format!("health:{}", args.run_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;
    let mut checks = Vec::new();
    let mut skipped = Vec::new();
    let run = async {
        let mut s = entry.session.lock().await;

        // The engine's own.
        match s.health_checks(&args.database).await {
            Ok(c) => checks.extend(c),
            Err(e) => skipped.push(format!("Chequeos del motor: {e}")),
        }

        // Connections and cache, from the monitor.
        if caps.monitor {
            match s.monitor().await {
                Ok(snap) => {
                    if let Some(m) = snap.metrics.iter().find(|m| m.key == "connections") {
                        if let (Some(v), Some(max)) = (m.value, m.max.filter(|x| *x > 0.0)) {
                            let share = v / max;
                            let sev = if share >= 0.9 { Severity::Critical } else if share >= 0.75 { Severity::Warning } else { Severity::Ok };
                            checks.push(
                                HealthCheck::new("connections", "Conexiones", format!("{} de {} conexiones en uso ({:.0} %)", v as i64, max as i64, share * 100.0), sev)
                                    .detail("Cerca del máximo, las conexiones nuevas se rechazan. Revisá si hay aplicaciones que no cierran sus conexiones o subí el límite."),
                            );
                        }
                    }
                    if let Some(v) = snap.metrics.iter().find(|m| m.key == "cache_hit").and_then(|m| m.value) {
                        let sev = if v < 80.0 { Severity::Warning } else if v < 90.0 { Severity::Info } else { Severity::Ok };
                        checks.push(
                            HealthCheck::new("cache_hit", "Rendimiento", format!("Aciertos de caché: {v:.1} %"), sev)
                                .detail("Un porcentaje bajo indica que muchas lecturas van a disco: puede faltar memoria para la caché o haber consultas que recorren tablas enteras."),
                        );
                    }
                }
                Err(e) => skipped.push(format!("Monitor: {e}")),
            }
        }

        // Long queries, blocking and open transactions, from the processes.
        if caps.processes {
            match s.processes().await {
                Ok(list) => {
                    let mine = |p: &dbine_driver::ServerProcess| p.database.as_deref().is_none_or(|d| d.eq_ignore_ascii_case(&args.database));
                    let long: Vec<String> = list
                        .iter()
                        .filter(|p| p.active && !p.own && !p.system && mine(p) && p.elapsed_ms.unwrap_or(0) >= LONG_QUERY_MS)
                        .map(|p| format!("{} · {} min · {}", p.id, p.elapsed_ms.unwrap_or(0) / 60_000, short(&p.sql)))
                        .collect();
                    checks.push(
                        HealthCheck::new(
                            "long_queries",
                            "Actividad",
                            if long.is_empty() { "Sin consultas de más de 5 minutos".to_string() } else { format!("{} consultas llevan más de 5 minutos", long.len()) },
                            if long.is_empty() { Severity::Ok } else { Severity::Warning },
                        )
                        .detail("Las consultas largas retienen recursos y bloqueos. Revisalas en Monitor › Procesos.")
                        .objects(long),
                    );
                    let blocked: Vec<String> = list
                        .iter()
                        .filter(|p| p.blocked_by.is_some() && mine(p))
                        .map(|p| format!("{} espera a {}", p.id, p.blocked_by.clone().unwrap_or_default()))
                        .collect();
                    checks.push(
                        HealthCheck::new(
                            "blocking",
                            "Actividad",
                            if blocked.is_empty() { "Sin sesiones bloqueadas".to_string() } else { format!("{} sesiones están bloqueadas", blocked.len()) },
                            match blocked.len() {
                                0 => Severity::Ok,
                                1..=4 => Severity::Warning,
                                _ => Severity::Critical,
                            },
                        )
                        .detail("Una sesión bloqueada espera a que otra libere lo que necesita. Abrí Monitor › Procesos para ver la cadena.")
                        .objects(blocked),
                    );
                    let idle: Vec<String> = list
                        .iter()
                        .filter(|p| {
                            let st = p.status.as_deref().unwrap_or("").to_lowercase();
                            !p.active && mine(p) && (st.contains("idle in transaction") || st.contains("transacción abierta")) && p.elapsed_ms.unwrap_or(0) >= IDLE_TXN_MS
                        })
                        .map(|p| format!("{} · {} · {} min", p.id, p.user.clone().unwrap_or_default(), p.elapsed_ms.unwrap_or(0) / 60_000))
                        .collect();
                    if !idle.is_empty() {
                        checks.push(
                            HealthCheck::new("idle_in_transaction", "Actividad", format!("{} sesiones tienen una transacción abierta sin actividad", idle.len()), Severity::Warning)
                                .detail("Una transacción abierta mantiene bloqueos y versiones viejas de las filas. Suele ser una aplicación que no hizo COMMIT.")
                                .objects(idle),
                        );
                    }
                }
                Err(e) => skipped.push(format!("Procesos: {e}")),
            }
        }

        // The last backup, where the engine keeps their history.
        if driver.backup().is_some() {
            match s.backups(Some(&args.database)).await {
                Ok(list) => {
                    let last = list
                        .iter()
                        .filter(|b| b.database.as_deref().is_none_or(|d| d.eq_ignore_ascii_case(&args.database)))
                        .filter_map(|b| b.finished.clone().or(b.started.clone()))
                        .filter_map(|t| chrono::DateTime::parse_from_rfc3339(&t).ok().map(|d| d.with_timezone(&chrono::Utc)).or_else(|| {
                            chrono::NaiveDateTime::parse_from_str(&t.replace('T', " ")[..19.min(t.len())], "%Y-%m-%d %H:%M:%S").ok().map(|n| n.and_utc())
                        }))
                        .max();
                    let (title, sev) = match last {
                        None => ("No hay backups registrados de esta base".to_string(), Severity::Warning),
                        Some(t) => {
                            let days = (chrono::Utc::now() - t).num_days();
                            (format!("Último backup: hace {days} días ({})", t.format("%Y-%m-%d")), if days > BACKUP_DAYS { Severity::Warning } else { Severity::Ok })
                        }
                    };
                    checks.push(HealthCheck::new("last_backup", "Backups", title, sev).detail("Según el historial que guarda el motor. Los backups hechos con otras herramientas fuera del servidor no aparecen."));
                }
                Err(e) => skipped.push(format!("Backups: {e}")),
            }
        }
    };
    run.await;
    state.sessions.remove(&key);
    // Worst first, then by category.
    checks.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.category.cmp(&b.category)));
    Ok(HealthReport { checks, checked_at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(), skipped })
}

//! The monitor dashboard for Aurora DSQL. The service is serverless: no
//! host CPU, memory, disk or replicas to ask about by SQL (those, and the
//! DPUs it bills, are in CloudWatch). What the PostgreSQL side does report
//! (sessions, running statements, the asynchronous index jobs) is shown;
//! every part is optional and a refusal becomes a note.

use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use serde_json::Value;
use tokio_postgres::{Client, SimpleQueryMessage, SimpleQueryRow};

const MAX_ROWS: usize = 200;
const MAX_TEXT: usize = 2000;

pub(crate) async fn snapshot(client: &Client) -> MonitorSnapshot {
    let mut snap = MonitorSnapshot::default();
    match rows(client, "SELECT version() AS version, current_database() AS db, current_setting('TimeZone') AS tz").await {
        Ok(r) => {
            if let Some(r) = r.first() {
                for (label, col) in [("Versión", "version"), ("Base", "db"), ("Zona horaria", "tz")] {
                    if let Some(v) = text(r, col) {
                        snap.info.push((label.into(), v));
                    }
                }
            }
        }
        Err(e) => snap.notes.push(format!("No se pudo leer la versión: {e}")),
    }

    let activity = format!(
        "SELECT pid, usename, coalesce(host(client_addr), '') AS client, application_name, state,
                round(extract(epoch FROM now() - backend_start)::numeric) AS connected,
                CASE WHEN state = 'active' THEN round(extract(epoch FROM now() - query_start)::numeric, 1) END AS running,
                left(query, {MAX_TEXT}) AS query
         FROM pg_stat_activity WHERE pid <> pg_backend_pid()
         ORDER BY (state = 'active') DESC, backend_start LIMIT {MAX_ROWS}"
    );
    match rows(client, &activity).await {
        Ok(r) => {
            // This session isn't listed: count it.
            let active = r.iter().filter(|r| text(r, "state").as_deref() == Some("active")).count() as f64;
            snap.metrics.push(Metric::new("connections", "Conexiones", "Conexiones", U::Count, Some(r.len() as f64 + 1.0)));
            snap.metrics.push(Metric::new("active_sessions", "Sesiones activas", "Conexiones", U::Count, Some(active)));
            let longest = r.iter().filter_map(|r| text(r, "running").and_then(|v| num(&v))).fold(0.0, f64::max);
            snap.metrics.push(Metric::new("longest_query", "Consulta más larga", "Actividad", U::Seconds, Some(longest)));
            snap.tables.push(table(
                "sessions",
                "Sesiones",
                &["PID", "Usuario", "Cliente", "Aplicación", "Estado", "Conectada (s)", "En curso (s)", "Consulta"],
                &r,
            ));
            let running: Vec<SimpleQueryRow> = r.into_iter().filter(|r| text(r, "state").as_deref() == Some("active")).collect();
            let mut t = table("queries", "Consultas en curso", &["PID", "Usuario", "Duración (s)", "Consulta"], &[]);
            t.rows = running
                .iter()
                .map(|r| ["pid", "usename", "running", "query"].iter().map(|c| cell(text(r, c).as_deref())).collect())
                .collect();
            snap.tables.push(t);
        }
        Err(e) => snap.notes.push(format!("No se pudieron leer las sesiones (pg_stat_activity): {e}")),
    }

    // Asynchronous index builds (CREATE INDEX ASYNC).
    match rows(client, &format!("SELECT * FROM sys.jobs LIMIT {MAX_ROWS}")).await {
        Ok(r) => {
            let cols: Vec<String> = r.first().map(|f| f.columns().iter().map(|c| c.name().to_string()).collect()).unwrap_or_default();
            let refs: Vec<&str> = cols.iter().map(String::as_str).collect();
            snap.metrics.push(Metric::new("jobs", "Jobs asíncronos", "Actividad", U::Count, Some(r.len() as f64)));
            snap.tables.push(table("jobs", "Jobs asíncronos (índices)", &refs, &r));
        }
        Err(e) => tracing::debug!("dsql: sys.jobs unavailable: {e}"),
    }

    snap.notes.push(
        "Aurora DSQL es serverless: el uso de cómputo (DPU), el almacenamiento del clúster y la latencia están en CloudWatch, no por SQL."
            .into(),
    );
    snap.notes.push("Aurora DSQL no usa bloqueos (control de concurrencia optimista): no hay bloqueos en espera que mostrar.".into());
    snap
}

pub(crate) async fn rows(client: &Client, sql: &str) -> Result<Vec<SimpleQueryRow>, String> {
    let msgs = client.simple_query(sql).await.map_err(|e| match e.as_db_error() {
        Some(db) => db.message().to_string(),
        None => e.to_string(),
    })?;
    Ok(msgs
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(r),
            _ => None,
        })
        .collect())
}

fn text(r: &SimpleQueryRow, col: &str) -> Option<String> {
    r.try_get(col).ok().flatten().map(str::to_string)
}

fn table(key: &str, title: &str, cols: &[&str], rows: &[SimpleQueryRow]) -> MonitorTable {
    let mut t = MonitorTable::new(key, title, cols);
    t.rows = rows.iter().take(MAX_ROWS).map(|r| (0..cols.len()).map(|i| cell(r.get(i))).collect()).collect();
    t
}

/// Plain numbers as numbers, long texts cut.
fn cell(v: Option<&str>) -> Value {
    let Some(t) = v else { return Value::Null };
    let plain = !t.is_empty() && t.len() <= 16 && t.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-');
    if plain && !(t.len() > 1 && t.starts_with('0') && !t.starts_with("0.")) {
        if let Ok(i) = t.parse::<i64>() {
            return Value::from(i);
        }
        if let Some(n) = t.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
            return Value::Number(n);
        }
    }
    match t.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => Value::String(format!("{}…", &t[..i])),
        None => Value::String(t.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells() {
        assert_eq!(cell(Some("12")), Value::from(12));
        assert_eq!(cell(Some("1.5")), serde_json::json!(1.5));
        assert_eq!(cell(Some("0012")), Value::String("0012".into()));
        assert_eq!(cell(Some("idle")), Value::String("idle".into()));
        assert_eq!(cell(None), Value::Null);
    }
}

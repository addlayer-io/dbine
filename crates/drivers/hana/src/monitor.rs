//! Server monitor from HANA's monitoring views (`M_*`): host memory and
//! CPU time, services, connections, running statements, blocked
//! transactions, disk usage, workload counters and the largest column
//! tables. Every view needs the MONITORING role or CATALOG READ; a query
//! that fails is skipped with a note.

use crate::{err, read_lob, text};
use dbine_driver::monitor::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use dbine_driver::Result;
use hdbconnect_async::Connection;
use serde_json::Value;

const MAX_ROWS: usize = 200;
const MAX_TEXT: usize = 2000;

/// A result set with its column names, every value as text.
#[derive(Debug, Default, Clone)]
pub(crate) struct Set {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Set {
    fn idx(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.eq_ignore_ascii_case(name))
    }
    pub(crate) fn get(&self, row: usize, name: &str) -> Option<&str> {
        let i = self.idx(name)?;
        self.rows.get(row)?.get(i)?.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    fn num(&self, row: usize, name: &str) -> Option<f64> {
        self.get(row, name).and_then(dbine_driver::monitor::num)
    }
    fn first(&self, name: &str) -> Option<f64> {
        self.num(0, name)
    }
    fn text(&self, name: &str) -> Option<String> {
        self.get(0, name).map(str::to_string)
    }
    fn sum(&self, name: &str) -> Option<f64> {
        let v: Vec<f64> = (0..self.rows.len()).filter_map(|r| self.num(r, name)).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    }
    fn sum_where(&self, name: &str, col: &str, pred: impl Fn(&str) -> bool) -> Option<f64> {
        let v: Vec<f64> =
            (0..self.rows.len()).filter(|&r| self.get(r, col).is_some_and(&pred)).filter_map(|r| self.num(r, name)).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    }
    fn count_where(&self, name: &str, pred: impl Fn(&str) -> bool) -> f64 {
        (0..self.rows.len()).filter(|&r| self.get(r, name).is_some_and(&pred)).count() as f64
    }
}

pub(crate) async fn query(conn: &Connection, sql: &str) -> Result<Set> {
    let rs = conn.query(sql).await.map_err(err)?;
    let cols = rs.metadata().iter().map(|f| f.displayname().to_string()).collect();
    let mut rows = Vec::new();
    for row in rs.into_rows().await.map_err(err)? {
        let mut r = Vec::new();
        for v in row {
            r.push(text(&read_lob(v).await));
        }
        rows.push(r);
    }
    Ok(Set { cols, rows })
}

fn cell(v: Option<&str>) -> Value {
    let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) else { return Value::Null };
    if let Ok(i) = s.parse::<i64>() {
        return i.into();
    }
    if s.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') {
        if let Some(n) = s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
            return Value::Number(n);
        }
    }
    if s.chars().count() > MAX_TEXT {
        format!("{}…", s.chars().take(MAX_TEXT).collect::<String>()).into()
    } else {
        s.to_string().into()
    }
}

fn table(key: &str, title: &str, cols: &[(&str, &str)], set: &Set) -> Option<MonitorTable> {
    let present: Vec<&(&str, &str)> = cols.iter().filter(|(c, _)| set.idx(c).is_some()).collect();
    if present.is_empty() {
        return None;
    }
    let labels: Vec<&str> = present.iter().map(|(_, l)| *l).collect();
    let mut t = MonitorTable::new(key, title, &labels);
    for r in 0..set.rows.len().min(MAX_ROWS) {
        t.rows.push(present.iter().map(|(c, _)| cell(set.get(r, c))).collect());
    }
    Some(t)
}

fn m(key: &str, label: &str, group: &str, unit: U, v: Option<f64>) -> Metric {
    Metric::new(key, label, group, unit, v)
}

struct Snap {
    s: MonitorSnapshot,
    denied: bool,
}

impl Snap {
    fn failed(&mut self, what: &str, e: dbine_driver::Error) {
        let msg = e.to_string();
        // 258: insufficient privilege.
        if msg.contains("[258]") || msg.to_ascii_lowercase().contains("privilege") {
            self.denied = true;
        } else {
            self.s.notes.push(format!("{what}: no disponible ({}).", msg.lines().next().unwrap_or("").trim()));
        }
    }
    fn info(&mut self, label: &str, v: Option<String>) {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            self.s.info.push((label.into(), v));
        }
    }
    fn table(&mut self, t: Option<MonitorTable>) {
        if let Some(t) = t {
            self.s.tables.push(t);
        }
    }
}

pub(crate) async fn snapshot(conn: &Connection) -> MonitorSnapshot {
    let mut s = Snap { s: MonitorSnapshot::default(), denied: false };

    match query(
        conn,
        "SELECT SYSTEM_ID, DATABASE_NAME, HOST, VERSION, USAGE, SECONDS_BETWEEN(START_TIME, CURRENT_TIMESTAMP) AS UPTIME_S
           FROM SYS.M_DATABASE",
    )
    .await
    {
        Ok(d) => {
            s.s.metrics.push(m("uptime", "Tiempo activo", "Servidor", U::Seconds, d.first("UPTIME_S")));
            s.info("Sistema", d.text("SYSTEM_ID"));
            s.info("Base de datos", d.text("DATABASE_NAME"));
            s.info("Servidor", d.text("HOST"));
            s.info("Versión", d.text("VERSION"));
            s.info("Uso", d.text("USAGE"));
        }
        Err(e) => s.failed("Base de datos (M_DATABASE)", e),
    }

    // Current CPU (%) comes from the load history (HANA 2 SPS03+), sampled every 10 s.
    if let Ok(l) = query(conn, "SELECT TOP 1 CPU FROM SYS.M_LOAD_HISTORY_HOST ORDER BY TIME DESC").await {
        s.s.metrics.push(m("cpu", "CPU del servidor", "CPU", U::Percent, l.first("CPU")));
    }
    match query(conn, "SELECT * FROM SYS.M_HOST_RESOURCE_UTILIZATION").await {
        Ok(h) => {
            let busy = match (h.sum("TOTAL_CPU_USER_TIME"), h.sum("TOTAL_CPU_SYSTEM_TIME")) {
                (Some(u), Some(sy)) => Some((u + sy) / 1000.0 * 100.0),
                _ => None,
            };
            s.s.metrics.push(m("cpu_time", "CPU del host (tiempo)", "CPU", U::Percent, busy).counter());
            s.s.metrics.push(
                m("mem_used", "Memoria usada por HANA", "Memoria", U::Bytes, h.sum("INSTANCE_TOTAL_MEMORY_USED_SIZE"))
                    .max(h.sum("ALLOCATION_LIMIT")),
            );
            let phys = match (h.sum("USED_PHYSICAL_MEMORY"), h.sum("FREE_PHYSICAL_MEMORY")) {
                (Some(u), Some(f)) => Some(u + f),
                _ => None,
            };
            s.s.metrics.push(
                m("host_mem_used", "Memoria física del host", "Memoria", U::Bytes, h.sum("USED_PHYSICAL_MEMORY")).max(phys),
            );
            s.info("Hosts", Some(h.rows.len().to_string()));
        }
        Err(e) => s.failed("Recursos del host (M_HOST_RESOURCE_UTILIZATION)", e),
    }
    match query(
        conn,
        "SELECT HOST, PORT, SERVICE_NAME, TOTAL_MEMORY_USED_SIZE, HEAP_MEMORY_USED_SIZE, SHARED_MEMORY_USED_SIZE,
                EFFECTIVE_ALLOCATION_LIMIT
           FROM SYS.M_SERVICE_MEMORY ORDER BY TOTAL_MEMORY_USED_SIZE DESC",
    )
    .await
    {
        Ok(t) => s.table(table(
            "services",
            "Servicios y memoria",
            &[
                ("HOST", "Host"),
                ("PORT", "Puerto"),
                ("SERVICE_NAME", "Servicio"),
                ("TOTAL_MEMORY_USED_SIZE", "Memoria usada (bytes)"),
                ("HEAP_MEMORY_USED_SIZE", "Heap (bytes)"),
                ("SHARED_MEMORY_USED_SIZE", "Compartida (bytes)"),
                ("EFFECTIVE_ALLOCATION_LIMIT", "Límite (bytes)"),
            ],
            &t,
        )),
        Err(e) => s.failed("Memoria de los servicios (M_SERVICE_MEMORY)", e),
    }
    match query(conn, "SELECT SUM(EXECUTION_COUNT) AS EXECS, SUM(TRANSACTION_COUNT) AS TXS FROM SYS.M_WORKLOAD").await {
        Ok(w) => {
            s.s.metrics.push(m("queries", "Sentencias", "Actividad", U::Count, w.first("EXECS")).counter());
            s.s.metrics.push(m("transactions", "Transacciones", "Actividad", U::Count, w.first("TXS")).counter());
        }
        Err(e) => s.failed("Carga (M_WORKLOAD)", e),
    }
    match query(
        conn,
        "SELECT TOP 200 CONNECTION_ID, USER_NAME, CURRENT_SCHEMA_NAME, CLIENT_HOST, CONNECTION_STATUS,
                SECONDS_BETWEEN(START_TIME, CURRENT_TIMESTAMP) AS AGE_S, CONNECTION_TYPE
           FROM SYS.M_CONNECTIONS WHERE CONNECTION_ID > 0 AND IS_ACTIVE = 'TRUE'
          ORDER BY CASE WHEN CONNECTION_STATUS = 'RUNNING' THEN 0 ELSE 1 END, CONNECTION_ID",
    )
    .await
    {
        Ok(c) => {
            s.s.metrics.push(m("connections", "Conexiones", "Conexiones", U::Count, Some(c.rows.len() as f64)));
            s.s.metrics.push(m(
                "active_sessions",
                "Sesiones activas",
                "Conexiones",
                U::Count,
                Some(c.count_where("CONNECTION_STATUS", |v| v == "RUNNING")),
            ));
            s.table(table(
                "sessions",
                "Sesiones",
                &[
                    ("CONNECTION_ID", "ID"),
                    ("USER_NAME", "Usuario"),
                    ("CURRENT_SCHEMA_NAME", "Esquema"),
                    ("CLIENT_HOST", "Cliente"),
                    ("CONNECTION_STATUS", "Estado"),
                    ("AGE_S", "Conectada hace (s)"),
                    ("CONNECTION_TYPE", "Tipo"),
                ],
                &c,
            ));
        }
        Err(e) => s.failed("Conexiones (M_CONNECTIONS)", e),
    }
    match query(
        conn,
        "SELECT TOP 200 CONNECTION_ID, STATEMENT_STATUS, SECONDS_BETWEEN(LAST_EXECUTED_TIME, CURRENT_TIMESTAMP) AS SECS,
                USED_MEMORY_SIZE, TO_NVARCHAR(SUBSTR(STATEMENT_STRING, 1, 2000)) AS SQL_TEXT
           FROM SYS.M_ACTIVE_STATEMENTS WHERE STATEMENT_STATUS IN ('ACTIVE', 'SUSPENDED')
          ORDER BY LAST_EXECUTED_TIME",
    )
    .await
    {
        Ok(q) => s.table(table(
            "queries",
            "Consultas en curso",
            &[
                ("CONNECTION_ID", "Conexión"),
                ("STATEMENT_STATUS", "Estado"),
                ("SECS", "Duración (s)"),
                ("USED_MEMORY_SIZE", "Memoria (bytes)"),
                ("SQL_TEXT", "Consulta"),
            ],
            &q,
        )),
        Err(e) => s.failed("Sentencias en curso (M_ACTIVE_STATEMENTS)", e),
    }
    match query(conn, "SELECT TOP 200 * FROM SYS.M_BLOCKED_TRANSACTIONS").await {
        Ok(b) => {
            s.s.metrics.push(m("locks_waiting", "Bloqueos en espera", "Bloqueos", U::Count, Some(b.rows.len() as f64)));
            s.table(table(
                "locks",
                "Bloqueos / esperas",
                &[
                    ("BLOCKED_TRANSACTION_ID", "Transacción bloqueada"),
                    ("LOCK_OWNER_TRANSACTION_ID", "Bloqueada por"),
                    ("WAITING_SCHEMA_NAME", "Esquema"),
                    ("WAITING_OBJECT_NAME", "Objeto"),
                    ("LOCK_TYPE", "Tipo"),
                    ("LOCK_MODE", "Modo"),
                    ("BLOCKED_TIME", "Desde"),
                ],
                &b,
            ));
        }
        Err(e) => s.failed("Transacciones bloqueadas (M_BLOCKED_TRANSACTIONS)", e),
    }
    match query(conn, "SELECT HOST, USAGE_TYPE, USED_SIZE FROM SYS.M_DISK_USAGE WHERE USED_SIZE >= 0 ORDER BY USED_SIZE DESC").await {
        Ok(d) => {
            let data = d.sum_where("USED_SIZE", "USAGE_TYPE", |t| t == "DATA" || t == "LOG");
            let total = query(conn, "SELECT SUM(TOTAL_SIZE) AS TOTAL FROM SYS.M_DISKS WHERE USAGE_TYPE IN ('DATA', 'LOG')")
                .await
                .ok()
                .and_then(|t| t.first("TOTAL"));
            s.s.metrics.push(m("storage_used", "Espacio usado (datos y log)", "Almacenamiento", U::Bytes, data).max(total));
            s.table(table("storage", "Uso de disco", &[("HOST", "Host"), ("USAGE_TYPE", "Tipo"), ("USED_SIZE", "Usado (bytes)")], &d));
        }
        Err(e) => s.failed("Uso de disco (M_DISK_USAGE)", e),
    }
    if let Ok(r) = query(
        conn,
        "SELECT HOST, PORT, SECONDARY_HOST, REPLICATION_MODE, REPLICATION_STATUS,
                SECONDS_BETWEEN(SHIPPED_LOG_POSITION_TIME, LAST_LOG_POSITION_TIME) AS LAG_S
           FROM SYS.M_SERVICE_REPLICATION",
    )
    .await
    {
        if !r.rows.is_empty() {
            let lag = (0..r.rows.len()).filter_map(|i| r.num(i, "LAG_S")).fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))));
            s.s.metrics.push(m("replication_lag", "Retraso de réplica", "Replicación", U::Seconds, lag.map(|v| v.max(0.0))));
            s.table(table(
                "replication",
                "Réplicas (System Replication)",
                &[
                    ("HOST", "Host"),
                    ("PORT", "Puerto"),
                    ("SECONDARY_HOST", "Secundario"),
                    ("REPLICATION_MODE", "Modo"),
                    ("REPLICATION_STATUS", "Estado"),
                    ("LAG_S", "Retraso (s)"),
                ],
                &r,
            ));
        }
    }
    match query(
        conn,
        "SELECT TOP 20 SCHEMA_NAME, TABLE_NAME, SUM(RECORD_COUNT) AS ROWS_, SUM(MEMORY_SIZE_IN_TOTAL) AS BYTES
           FROM SYS.M_CS_TABLES GROUP BY SCHEMA_NAME, TABLE_NAME ORDER BY 4 DESC",
    )
    .await
    {
        Ok(t) => s.table(table(
            "top_objects",
            "Objetos más grandes (en memoria)",
            &[("SCHEMA_NAME", "Esquema"), ("TABLE_NAME", "Tabla"), ("ROWS_", "Filas"), ("BYTES", "Memoria (bytes)")],
            &t,
        )),
        Err(e) => s.failed("Tablas columnares (M_CS_TABLES)", e),
    }

    if s.denied {
        s.s.notes.insert(
            0,
            "Faltan permisos para parte de las vistas de monitoreo: asigná el rol MONITORING (o CATALOG READ) al usuario.".into(),
        );
    }
    if !s.s.metrics.iter().any(|x| x.key == "cpu") {
        s.s.notes.push(
            "El porcentaje de CPU sale de M_LOAD_HISTORY_HOST (HANA 2 SPS03 o posterior); sin esa vista se grafica el tiempo de CPU del host."
                .into(),
        );
    }
    s.s.notes.push("M_CS_TABLES solo cuenta las tablas columnares cargadas en memoria.".into());
    s.s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_pick_existing_columns_and_cut_text() {
        let set = Set {
            cols: vec!["CONNECTION_ID".into(), "SQL_TEXT".into()],
            rows: vec![vec![Some("42".into()), Some("x".repeat(3000))]],
        };
        let t = table("queries", "Q", &[("CONNECTION_ID", "Conexión"), ("MISSING", "No"), ("SQL_TEXT", "Consulta")], &set).unwrap();
        assert_eq!(t.columns, vec!["Conexión", "Consulta"]);
        assert_eq!(t.rows[0][0], Value::from(42));
        assert_eq!(t.rows[0][1].as_str().unwrap().chars().count(), MAX_TEXT + 1);
        assert!(table("x", "X", &[("NOPE", "n")], &set).is_none());
    }

    #[test]
    fn sums_and_filters() {
        let set = Set {
            cols: vec!["USAGE_TYPE".into(), "USED_SIZE".into()],
            rows: vec![
                vec![Some("DATA".into()), Some("100".into())],
                vec![Some("LOG".into()), Some("20".into())],
                vec![Some("TRACE".into()), Some("5".into())],
            ],
        };
        assert_eq!(set.sum("used_size"), Some(125.0));
        assert_eq!(set.sum_where("USED_SIZE", "USAGE_TYPE", |t| t == "DATA" || t == "LOG"), Some(120.0));
        assert_eq!(set.count_where("USAGE_TYPE", |t| t == "LOG"), 1.0);
    }
}

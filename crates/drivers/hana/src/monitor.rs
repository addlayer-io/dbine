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
        self.rows
            .get(row)?
            .get(i)?
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
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
        let v: Vec<f64> = (0..self.rows.len())
            .filter_map(|r| self.num(r, name))
            .collect();
        (!v.is_empty()).then(|| v.iter().sum())
    }
    fn sum_where(&self, name: &str, col: &str, pred: impl Fn(&str) -> bool) -> Option<f64> {
        let v: Vec<f64> = (0..self.rows.len())
            .filter(|&r| self.get(r, col).is_some_and(&pred))
            .filter_map(|r| self.num(r, name))
            .collect();
        (!v.is_empty()).then(|| v.iter().sum())
    }
    fn count_where(&self, name: &str, pred: impl Fn(&str) -> bool) -> f64 {
        (0..self.rows.len())
            .filter(|&r| self.get(r, name).is_some_and(&pred))
            .count() as f64
    }
}

pub(crate) async fn query(conn: &Connection, sql: &str) -> Result<Set> {
    let rs = conn.query(sql).await.map_err(err)?;
    let cols = rs
        .metadata()
        .iter()
        .map(|f| f.displayname().to_string())
        .collect();
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
    let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) else {
        return Value::Null;
    };
    if let Ok(i) = s.parse::<i64>() {
        return i.into();
    }
    if s.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
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
        t.rows
            .push(present.iter().map(|(c, _)| cell(set.get(r, c))).collect());
    }
    Some(t)
}

fn m(key: &str, label: &str, group: &str, unit: U, v: Option<f64>) -> Metric {
    Metric::new(key, label, group, unit, v)
}

/// What the metric queries answered (`None`: the query failed, or the
/// view isn't there).
#[derive(Debug, Default)]
struct Readings {
    /// M_DATABASE.
    database: Option<Set>,
    /// M_LOAD_HISTORY_HOST, the latest sample (HANA 2 SPS03+).
    load: Option<Set>,
    /// M_HOST_RESOURCE_UTILIZATION.
    host: Option<Set>,
    /// The hosts' logical CPUs (M_HOST_INFORMATION 'cpu_threads').
    cpus: Option<f64>,
    /// M_WORKLOAD.
    workload: Option<Set>,
    /// Connections and the running ones, counted by the server.
    connections: Option<(f64, f64)>,
    /// M_BLOCKED_TRANSACTIONS: how many, and whether the server counted
    /// them (else they are the rows of the list, which stops at 200).
    blocked: Option<(f64, bool)>,
    /// M_DISK_USAGE, and the disks' size (M_DISKS).
    disk: Option<Set>,
    disk_total: Option<f64>,
    /// M_SERVICE_REPLICATION, when there are secondaries.
    replication: Option<Set>,
}

/// The figures, in display order, each with what it measures and where it
/// comes from; one per query that answered.
fn metrics(r: &Readings) -> Vec<Metric> {
    let mut out = Vec::new();
    if let Some(d) = &r.database {
        out.push(
            m(
                "uptime",
                "Tiempo activo",
                "Servidor",
                U::Seconds,
                d.first("UPTIME_S"),
            )
            .help("Tiempo desde que arrancó la base de datos.")
            .source("M_DATABASE: SECONDS_BETWEEN(START_TIME, CURRENT_TIMESTAMP)"),
        );
    }
    if let Some(l) = &r.load {
        out.push(
            m("cpu", "CPU del servidor", "CPU", U::Percent, l.first("CPU"))
                .help("Uso de CPU del host en la última muestra del historial de carga, que HANA toma cada 10 segundos: 100 % equivale a todos los núcleos ocupados. Incluye todo lo que corre en el host, no solo HANA; con varios hosts, es el de la muestra más reciente.")
                .source("M_LOAD_HISTORY_HOST: CPU (latest TIME)"),
        );
    }
    if let Some(h) = &r.host {
        // Milliseconds of CPU of every logical CPU of the hosts: over their
        // count, the rate is the share of all of them (100 = all busy).
        let busy = match (
            h.sum("TOTAL_CPU_USER_TIME"),
            h.sum("TOTAL_CPU_SYSTEM_TIME"),
            r.cpus,
        ) {
            (Some(u), Some(sy), Some(n)) => Some((u + sy) / 1000.0 * 100.0 / n),
            _ => None,
        };
        out.push(
            m("cpu_time", "CPU del host (tiempo)", "CPU", U::Percent, busy)
                .max(Some(100.0))
                .counter()
                .help("Tiempo de CPU de los hosts (usuario y sistema) como parte de todos sus núcleos: 100 % equivale a todos ocupados. Lo informa el sistema operativo, así que incluye todo lo que corre en los hosts, no solo HANA.")
                .source("M_HOST_RESOURCE_UTILIZATION: TOTAL_CPU_USER_TIME + TOTAL_CPU_SYSTEM_TIME (ms), ÷ M_HOST_INFORMATION 'cpu_threads', sum of hosts"),
        );
        out.push(
            m(
                "mem_used",
                "Memoria usada por HANA",
                "Memoria",
                U::Bytes,
                h.sum("INSTANCE_TOTAL_MEMORY_USED_SIZE"),
            )
            .max(h.sum("ALLOCATION_LIMIT"))
            .help("Memoria que usan los procesos de la instancia: tablas en memoria, cálculos de las consultas, código y memoria compartida. El máximo es el límite de asignación (global_allocation_limit); cerca de él, HANA descarga tablas de la memoria y las sentencias pueden fallar por falta de memoria.")
            .source("M_HOST_RESOURCE_UTILIZATION: INSTANCE_TOTAL_MEMORY_USED_SIZE (max: ALLOCATION_LIMIT), sum of hosts"),
        );
        let phys = match (h.sum("USED_PHYSICAL_MEMORY"), h.sum("FREE_PHYSICAL_MEMORY")) {
            (Some(u), Some(f)) => Some(u + f),
            _ => None,
        };
        out.push(
            m(
                "host_mem_used",
                "Memoria física del host",
                "Memoria",
                U::Bytes,
                h.sum("USED_PHYSICAL_MEMORY"),
            )
            .max(phys)
            .help("Memoria física en uso en los hosts, de todos los procesos (HANA incluido); el máximo es la memoria física total.")
            .source("M_HOST_RESOURCE_UTILIZATION: USED_PHYSICAL_MEMORY (max: USED_PHYSICAL_MEMORY + FREE_PHYSICAL_MEMORY), sum of hosts"),
        );
    }
    if let Some(w) = &r.workload {
        out.push(
            m(
                "queries",
                "Sentencias",
                "Actividad",
                U::Count,
                w.first("EXECS"),
            )
            .counter()
            .help("Sentencias SQL que ejecutó la base, de cualquier tipo.")
            .source("M_WORKLOAD: SUM(EXECUTION_COUNT)"),
        );
        out.push(
            m(
                "transactions",
                "Transacciones",
                "Actividad",
                U::Count,
                w.first("TXS"),
            )
            .counter()
            .help("Transacciones que procesó la base, de lectura y de escritura.")
            .source("M_WORKLOAD: SUM(TRANSACTION_COUNT)"),
        );
    }
    if let Some((total, running)) = r.connections {
        out.push(
            m("connections", "Conexiones", "Conexiones", U::Count, Some(total))
                .help("Conexiones abiertas a la base, incluida la del monitor; no cuenta las internas de HANA.")
                .source("M_CONNECTIONS: COUNT(*) WHERE CONNECTION_ID > 0 AND IS_ACTIVE = 'TRUE'"),
        );
        out.push(
            m("active_sessions", "Sesiones activas", "Conexiones", U::Count, Some(running))
                .help("Conexiones ejecutando una sentencia ahora, incluida la del monitor. Si se mantiene por encima de los núcleos del host, las sentencias compiten por CPU.")
                .source("M_CONNECTIONS: COUNT(*) WHERE CONNECTION_STATUS = 'RUNNING'"),
        );
    }
    if let Some((n, counted)) = r.blocked {
        out.push(
            m("locks_waiting", "Bloqueos en espera", "Bloqueos", U::Count, Some(n))
                .help("Transacciones esperando un bloqueo que tiene otra, de fila, de tabla o de metadatos. Si no baja, mirá en la tabla de bloqueos quién lo retiene.")
                .source(if counted {
                    "M_BLOCKED_TRANSACTIONS: COUNT(*)"
                } else {
                    "M_BLOCKED_TRANSACTIONS: number of rows (up to 200)"
                }),
        );
    }
    if let Some(d) = &r.disk {
        let data = d.sum_where("USED_SIZE", "USAGE_TYPE", |t| t == "DATA" || t == "LOG");
        out.push(
            m("storage_used", "Espacio usado (datos y log)", "Almacenamiento", U::Bytes, data)
                .max(r.disk_total)
                .help("Espacio que ocupan en disco los volúmenes de datos y de log. El máximo es el tamaño de los discos donde están, que pueden tener también otros archivos.")
                .source("M_DISK_USAGE: SUM(USED_SIZE) WHERE USAGE_TYPE IN ('DATA', 'LOG') (max: M_DISKS SUM(TOTAL_SIZE))"),
        );
    }
    if let Some(rep) = r.replication.as_ref().filter(|r| !r.rows.is_empty()) {
        let lag = (0..rep.rows.len())
            .filter_map(|i| rep.num(i, "LAG_S"))
            .fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))));
        out.push(
            m("replication_lag", "Retraso de réplica", "Replicación", U::Seconds, lag.map(|v| v.max(0.0)))
                .help("Tiempo entre el último log que escribió el primario y el último que envió al secundario, en el servicio más atrasado. Si crece, el secundario se atrasa, y en una toma de control faltarían esos cambios.")
                .source("M_SERVICE_REPLICATION: MAX(SECONDS_BETWEEN(SHIPPED_LOG_POSITION_TIME, LAST_LOG_POSITION_TIME))"),
        );
    }
    out
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
            self.s.notes.push(format!(
                "{what}: no disponible ({}).",
                msg.lines().next().unwrap_or("").trim()
            ));
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
    let mut s = Snap {
        s: MonitorSnapshot::default(),
        denied: false,
    };
    let mut r = Readings::default();

    match query(
        conn,
        "SELECT SYSTEM_ID, DATABASE_NAME, HOST, VERSION, USAGE, SECONDS_BETWEEN(START_TIME, CURRENT_TIMESTAMP) AS UPTIME_S
           FROM SYS.M_DATABASE",
    )
    .await
    {
        Ok(d) => {
            s.info("Sistema", d.text("SYSTEM_ID"));
            s.info("Base de datos", d.text("DATABASE_NAME"));
            s.info("Servidor", d.text("HOST"));
            s.info("Versión", d.text("VERSION"));
            s.info("Uso", d.text("USAGE"));
            r.database = Some(d);
        }
        Err(e) => s.failed("Base de datos (M_DATABASE)", e),
    }

    // Current CPU (%) comes from the load history (HANA 2 SPS03+), sampled every 10 s.
    r.load = query(
        conn,
        "SELECT TOP 1 CPU FROM SYS.M_LOAD_HISTORY_HOST ORDER BY TIME DESC",
    )
    .await
    .ok();
    match query(conn, "SELECT * FROM SYS.M_HOST_RESOURCE_UTILIZATION").await {
        Ok(h) => {
            s.info("Hosts", Some(h.rows.len().to_string()));
            r.cpus = query(
                conn,
                "SELECT HOST, VALUE FROM SYS.M_HOST_INFORMATION WHERE KEY = 'cpu_threads'",
            )
            .await
            .ok()
            .and_then(|c| c.sum("VALUE"))
            .filter(|n| *n > 0.0);
            if r.cpus.is_none() {
                s.s.notes.push(
                    "No se pudo leer cuántos núcleos tienen los hosts (M_HOST_INFORMATION): sin ese dato no se muestra el tiempo de CPU del host."
                        .into(),
                );
            }
            r.host = Some(h);
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
    match query(
        conn,
        "SELECT SUM(EXECUTION_COUNT) AS EXECS, SUM(TRANSACTION_COUNT) AS TXS FROM SYS.M_WORKLOAD",
    )
    .await
    {
        Ok(w) => r.workload = Some(w),
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
            // The list stops at 200; the counts don't.
            let counted = query(
                conn,
                "SELECT COUNT(*) AS N, SUM(CASE WHEN CONNECTION_STATUS = 'RUNNING' THEN 1 ELSE 0 END) AS RUNNING
                   FROM SYS.M_CONNECTIONS WHERE CONNECTION_ID > 0 AND IS_ACTIVE = 'TRUE'",
            )
            .await
            .ok()
            .and_then(|n| Some((n.first("N")?, n.first("RUNNING").unwrap_or(0.0))));
            r.connections = Some(counted.unwrap_or((
                c.rows.len() as f64,
                c.count_where("CONNECTION_STATUS", |v| v == "RUNNING"),
            )));
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
            // The list stops at 200; the count doesn't.
            let counted = query(conn, "SELECT COUNT(*) AS N FROM SYS.M_BLOCKED_TRANSACTIONS")
                .await
                .ok()
                .and_then(|n| n.first("N"));
            r.blocked = Some(match counted {
                Some(n) => (n, true),
                None => (b.rows.len() as f64, false),
            });
        }
        Err(e) => s.failed("Transacciones bloqueadas (M_BLOCKED_TRANSACTIONS)", e),
    }
    match query(conn, "SELECT HOST, USAGE_TYPE, USED_SIZE FROM SYS.M_DISK_USAGE WHERE USED_SIZE >= 0 ORDER BY USED_SIZE DESC").await {
        Ok(d) => {
            r.disk_total = query(conn, "SELECT SUM(TOTAL_SIZE) AS TOTAL FROM SYS.M_DISKS WHERE USAGE_TYPE IN ('DATA', 'LOG')")
                .await
                .ok()
                .and_then(|t| t.first("TOTAL"));
            s.table(table("storage", "Uso de disco", &[("HOST", "Host"), ("USAGE_TYPE", "Tipo"), ("USED_SIZE", "Usado (bytes)")], &d));
            r.disk = Some(d);
        }
        Err(e) => s.failed("Uso de disco (M_DISK_USAGE)", e),
    }
    if let Ok(rep) = query(
        conn,
        "SELECT HOST, PORT, SECONDARY_HOST, REPLICATION_MODE, REPLICATION_STATUS,
                SECONDS_BETWEEN(SHIPPED_LOG_POSITION_TIME, LAST_LOG_POSITION_TIME) AS LAG_S
           FROM SYS.M_SERVICE_REPLICATION",
    )
    .await
    {
        if !rep.rows.is_empty() {
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
                &rep,
            ));
            r.replication = Some(rep);
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

    s.s.metrics = metrics(&r);
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
    s.s.notes
        .push("M_CS_TABLES solo cuenta las tablas columnares cargadas en memoria.".into());
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
        let t = table(
            "queries",
            "Q",
            &[
                ("CONNECTION_ID", "Conexión"),
                ("MISSING", "No"),
                ("SQL_TEXT", "Consulta"),
            ],
            &set,
        )
        .unwrap();
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
        assert_eq!(
            set.sum_where("USED_SIZE", "USAGE_TYPE", |t| t == "DATA" || t == "LOG"),
            Some(120.0)
        );
        assert_eq!(set.count_where("USAGE_TYPE", |t| t == "LOG"), 1.0);
    }

    fn one(cols: &[&str], v: &str) -> Option<Set> {
        Some(Set {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: vec![cols.iter().map(|_| Some(v.to_string())).collect()],
        })
    }

    /// Every query answering.
    fn full() -> Readings {
        Readings {
            database: one(&["UPTIME_S"], "60"),
            load: one(&["CPU"], "12"),
            host: one(
                &[
                    "TOTAL_CPU_USER_TIME",
                    "TOTAL_CPU_SYSTEM_TIME",
                    "INSTANCE_TOTAL_MEMORY_USED_SIZE",
                    "ALLOCATION_LIMIT",
                    "USED_PHYSICAL_MEMORY",
                    "FREE_PHYSICAL_MEMORY",
                ],
                "2000",
            ),
            cpus: Some(4.0),
            workload: one(&["EXECS", "TXS"], "10"),
            connections: Some((5.0, 1.0)),
            blocked: Some((7.0, true)),
            disk: one(&["USAGE_TYPE", "USED_SIZE"], "DATA"),
            disk_total: Some(100.0),
            replication: one(&["LAG_S"], "3"),
        }
    }

    /// Every figure says what it measures and where it comes from.
    #[test]
    fn every_metric_has_help_and_source() {
        let m = metrics(&full());
        assert_eq!(m.len(), 12);
        let mut keys: Vec<&str> = m.iter().map(|m| m.key.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), m.len(), "duplicate keys");
        for x in &m {
            assert!(
                x.help.as_deref().is_some_and(|h| !h.trim().is_empty()),
                "{} has no help",
                x.key
            );
            assert!(
                x.source.as_deref().is_some_and(|s| !s.trim().is_empty()),
                "{} has no source",
                x.key
            );
        }
        // A query that failed leaves its figures out.
        assert!(metrics(&Readings::default()).is_empty());
    }

    /// The host CPU time is a share of every logical CPU (100 = all busy),
    /// never per core; without the CPU count it has no value.
    #[test]
    fn host_cpu_time_is_a_share_of_every_cpu() {
        let cpu = |r: &Readings| {
            metrics(r)
                .into_iter()
                .find(|m| m.key == "cpu_time")
                .unwrap()
        };
        // 4 s of CPU over 4 CPUs: a second of all of them is 100.
        let mut r = full();
        let m = cpu(&r);
        assert_eq!(m.value, Some(100.0));
        assert_eq!(m.max, Some(100.0));
        assert!(m.counter);
        r.cpus = None;
        assert_eq!(cpu(&r).value, None);
    }
}

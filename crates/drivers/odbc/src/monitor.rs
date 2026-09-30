//! Server monitor per preset ([`dbine_driver::Session::monitor`]): each
//! engine's own system views (Db2 `MON_GET_*`, Teradata `DBC`, Vertica
//! `v_monitor`, Informix `sysmaster`, Sybase ASE `master..sysprocesses` and
//! MDA tables, Exasol `EXA_STATISTICS`…). The generic preset goes by the
//! DBMS name the driver reports.
//!
//! Every query is cheap (catalog and monitor views, no user tables) and
//! optional: one that fails (permissions, older version, feature off) is
//! skipped with a note, the rest of the snapshot still comes back. Queries
//! read columns by name where versions differ, so a missing column is just
//! a missing figure.

use crate::design::{eng, Eng};
use crate::presets::Preset;
use dbine_driver::monitor::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use dbine_driver::Result;
use serde_json::Value;
use std::collections::HashMap;

pub const MAX_ROWS: usize = 200;
pub const MAX_TEXT: usize = 2000;
const KB: f64 = 1024.0;
const MB: f64 = 1024.0 * 1024.0;
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// What the monitor needs from the session besides SQL.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    /// SQL_DBMS_NAME.
    pub dbms: String,
    /// "<dbms> <version>".
    pub version: String,
    pub database: String,
    /// The database file or folder of file-based presets (Access, dBase).
    pub file: Option<String>,
}

/// A result set with its column names.
#[derive(Debug, Clone, Default)]
pub struct Set {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Set {
    #[cfg(test)]
    pub fn new(cols: &[&str], rows: Vec<Vec<Option<&str>>>) -> Set {
        Set {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: rows.into_iter().map(|r| r.into_iter().map(|c| c.map(str::to_string)).collect()).collect(),
        }
    }

    /// Column index by name, case-insensitive.
    fn idx(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name))
    }

    pub fn has(&self, name: &str) -> bool {
        self.idx(name).is_some()
    }

    pub fn get(&self, row: usize, name: &str) -> Option<&str> {
        let i = self.idx(name)?;
        self.rows.get(row)?.get(i)?.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    pub fn at(&self, row: usize, col: usize) -> Option<&str> {
        self.rows.get(row)?.get(col)?.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    pub fn num(&self, row: usize, name: &str) -> Option<f64> {
        self.get(row, name).and_then(parse)
    }

    /// First row's value.
    pub fn text(&self, name: &str) -> Option<String> {
        self.get(0, name).map(str::to_string)
    }

    pub fn first(&self, name: &str) -> Option<f64> {
        self.num(0, name)
    }

    /// Sum over the rows (`None` when no row has a number there).
    pub fn sum(&self, name: &str) -> Option<f64> {
        let v: Vec<f64> = (0..self.rows.len()).filter_map(|r| self.num(r, name)).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    }

    pub fn avg(&self, name: &str) -> Option<f64> {
        let v: Vec<f64> = (0..self.rows.len()).filter_map(|r| self.num(r, name)).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    }

    /// Rows where `pred` holds for the text of column `name`.
    pub fn count_where(&self, name: &str, pred: impl Fn(&str) -> bool) -> f64 {
        (0..self.rows.len()).filter(|&r| self.get(r, name).is_some_and(&pred)).count() as f64
    }

    /// Two-column (name, value) sets as a map with lowercase, trimmed keys.
    pub fn kv(&self) -> HashMap<String, String> {
        self.rows
            .iter()
            .filter_map(|r| {
                let k = r.first()?.as_deref()?.trim().to_ascii_lowercase();
                let v = r.get(1)?.as_deref()?.trim().to_string();
                Some((k, v))
            })
            .collect()
    }
}

/// A number from server text ("12.5", "1,024", " 3 ").
pub fn parse(s: &str) -> Option<f64> {
    let t = s.trim();
    dbine_driver::monitor::num(t).or_else(|| t.replace(',', "").parse().ok()).filter(|v: &f64| v.is_finite())
}

/// Where the SQL goes: the ODBC connection, or canned sets in tests.
pub trait Source {
    fn query(&mut self, sql: &str) -> Result<Set>;
}

/// The snapshot being built.
pub struct Mon<'a> {
    src: &'a mut dyn Source,
    pub snap: MonitorSnapshot,
}

/// A standard metric by key (see the monitor contract).
pub fn std_metric(key: &str, value: Option<f64>) -> Metric {
    let (label, group, unit, counter) = match key {
        "cpu" => ("CPU del servidor", "CPU", U::Percent, false),
        "cpu_time" => ("CPU del proceso", "CPU", U::Percent, true),
        "mem_used" => ("Memoria usada", "Memoria", U::Bytes, false),
        "mem_cache" => ("Caché / buffer pool", "Memoria", U::Bytes, false),
        "connections" => ("Conexiones", "Conexiones", U::Count, false),
        "active_sessions" => ("Sesiones activas", "Conexiones", U::Count, false),
        "queries" => ("Consultas", "Actividad", U::Count, true),
        "transactions" => ("Transacciones", "Actividad", U::Count, true),
        "rows_read" => ("Filas leídas", "Actividad", U::Count, true),
        "rows_written" => ("Filas escritas", "Actividad", U::Count, true),
        "net_in" => ("Red entrante", "Red", U::Bytes, true),
        "net_out" => ("Red saliente", "Red", U::Bytes, true),
        "disk_read" => ("Lectura de disco", "Disco", U::Bytes, true),
        "disk_write" => ("Escritura en disco", "Disco", U::Bytes, true),
        "cache_hit" => ("Aciertos de caché", "Caché", U::Percent, false),
        "storage_used" => ("Espacio usado", "Almacenamiento", U::Bytes, false),
        "locks_waiting" => ("Bloqueos en espera", "Bloqueos", U::Count, false),
        "deadlocks" => ("Deadlocks", "Bloqueos", U::Count, true),
        "replication_lag" => ("Retraso de réplica", "Replicación", U::Seconds, false),
        "uptime" => ("Tiempo activo", "Servidor", U::Seconds, false),
        _ => (key, "Otros", U::Count, false),
    };
    let m = Metric::new(key, label, group, unit, value);
    if counter {
        m.counter()
    } else {
        m
    }
}

/// `(1 − misses / accesses) × 100`, when there were accesses.
pub fn hit_ratio(misses: Option<f64>, accesses: Option<f64>) -> Option<f64> {
    match (misses, accesses) {
        (Some(m), Some(a)) if a > 0.0 => Some(((1.0 - m / a) * 100.0).clamp(0.0, 100.0)),
        _ => None,
    }
}

fn add(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

fn times(v: Option<f64>, f: f64) -> Option<f64> {
    v.map(|x| x * f)
}

/// A ratio that may come as 0–1 or 0–100, as a percentage.
fn pct(v: Option<f64>) -> Option<f64> {
    v.map(|x| if x <= 1.0 { x * 100.0 } else { x })
}

fn short_err(e: &dbine_driver::Error) -> String {
    let s = e.to_string();
    let s = s.lines().next().unwrap_or("").trim();
    if s.chars().count() > 160 {
        format!("{}…", s.chars().take(160).collect::<String>())
    } else {
        s.to_string()
    }
}

/// A cell for a monitor table: numbers as numbers, long text cut.
fn cell(v: Option<&str>) -> Value {
    let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) else { return Value::Null };
    let numeric = !(s.len() > 1 && s.starts_with('0') && !s.starts_with("0."));
    if numeric {
        if let Ok(i) = s.parse::<i64>() {
            return i.into();
        }
        if let Ok(f) = s.parse::<f64>() {
            if f.is_finite() && s.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+')) {
                return serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number);
            }
        }
    }
    if s.chars().count() > MAX_TEXT {
        format!("{}…", s.chars().take(MAX_TEXT).collect::<String>()).into()
    } else {
        s.to_string().into()
    }
}

impl<'a> Mon<'a> {
    pub fn new(src: &'a mut dyn Source) -> Self {
        Mon { src, snap: MonitorSnapshot::default() }
    }

    /// Runs `sql`; on failure notes "`what`: …" and gives `None`.
    pub fn q(&mut self, what: &str, sql: &str) -> Option<Set> {
        match self.src.query(sql) {
            Ok(s) => Some(s),
            Err(e) => {
                self.note(format!("{what}: no disponible ({}).", short_err(&e)));
                None
            }
        }
    }

    /// Runs `sql`; a failure is silent (optional features).
    pub fn q_quiet(&mut self, sql: &str) -> Option<Set> {
        self.src.query(sql).ok()
    }

    pub fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        if !self.snap.notes.contains(&s) {
            self.snap.notes.push(s);
        }
    }

    pub fn metric(&mut self, m: Metric) {
        if let Some(old) = self.snap.metrics.iter_mut().find(|x| x.key == m.key) {
            if old.value.is_none() {
                *old = m;
            }
            return;
        }
        self.snap.metrics.push(m);
    }

    pub fn std(&mut self, key: &str, v: Option<f64>) {
        self.metric(std_metric(key, v));
    }

    pub fn std_max(&mut self, key: &str, v: Option<f64>, max: Option<f64>) {
        self.metric(std_metric(key, v).max(max.filter(|m| *m > 0.0)));
    }

    pub fn info(&mut self, label: &str, v: Option<impl ToString>) {
        let Some(v) = v.map(|v| v.to_string()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else { return };
        if !self.snap.info.iter().any(|(l, _)| l == label) {
            self.snap.info.push((label.to_string(), v));
        }
    }

    /// A table from a set, column by column in order, with these labels.
    pub fn table(&mut self, key: &str, title: &str, labels: &[&str], set: &Set) {
        let mut t = MonitorTable::new(key, title, labels);
        for r in 0..set.rows.len().min(MAX_ROWS) {
            t.rows.push((0..labels.len()).map(|c| cell(set.at(r, c))).collect());
        }
        self.snap.tables.push(t);
    }

    /// A table picking columns by name (`(column, label)`); columns the set
    /// doesn't have are left out. Nothing when none is there.
    pub fn table_by(&mut self, key: &str, title: &str, cols: &[(&str, &str)], set: &Set) {
        let present: Vec<&(&str, &str)> = cols.iter().filter(|(c, _)| set.has(c)).collect();
        if present.is_empty() {
            return;
        }
        let labels: Vec<&str> = present.iter().map(|(_, l)| *l).collect();
        let mut t = MonitorTable::new(key, title, &labels);
        for r in 0..set.rows.len().min(MAX_ROWS) {
            t.rows.push(present.iter().map(|(c, _)| cell(set.get(r, c))).collect());
        }
        self.snap.tables.push(t);
    }

    /// A table with the set's own column names.
    pub fn table_raw(&mut self, key: &str, title: &str, set: &Set) {
        let labels: Vec<&str> = set.cols.iter().map(String::as_str).collect();
        self.table(key, title, &labels, set);
    }

    /// A table from rows built in code.
    pub fn table_rows(&mut self, key: &str, title: &str, labels: &[&str], rows: Vec<Vec<Value>>) {
        let mut t = MonitorTable::new(key, title, labels);
        t.rows = rows.into_iter().take(MAX_ROWS).collect();
        self.snap.tables.push(t);
    }
}

/// Presets with a monitor. The others say why in [`unsupported_reason`].
pub fn supports(p: &Preset) -> bool {
    unsupported_reason(p).is_none()
}

pub fn unsupported_reason(p: &Preset) -> Option<&'static str> {
    match eng(p) {
        Eng::Spark => Some(
            "Spark Thrift Server y Kyuubi no exponen métricas por SQL: se ven en la interfaz web de Spark (puerto 4040) o en la API REST y las métricas de Kyuubi.",
        ),
        Eng::Zen => Some(
            "Actian Zen no expone sesiones, bloqueos ni estadísticas por SQL: se ven en Zen Monitor o con su API de administración (DTI/DTO).",
        ),
        Eng::NetSuite => Some(
            "SuiteAnalytics Connect es un servicio de solo lectura de NetSuite: no informa carga, sesiones ni almacenamiento.",
        ),
        Eng::Mimer => Some(
            "Mimer SQL no expone sesiones, sentencias en curso ni bloqueos por SQL: se ven con el programa sqlmonitor.",
        ),
        _ => None,
    }
}

/// One snapshot of the server behind `preset`.
pub fn collect(p: &Preset, ctx: &Ctx, src: &mut dyn Source) -> MonitorSnapshot {
    let mut m = Mon::new(src);
    let e = match eng(p) {
        Eng::Generic => match eng_from_dbms(&ctx.dbms) {
            Some(e) => e,
            None => {
                m.info("Motor", Some(&ctx.dbms));
                m.info("Versión", Some(&ctx.version));
                m.info("Base de datos", Some(&ctx.database));
                m.note(format!(
                    "El preset ODBC genérico no conoce las vistas de monitoreo de «{}»: solo muestra lo que informa el driver.",
                    if ctx.dbms.is_empty() { "este motor" } else { &ctx.dbms }
                ));
                return m.snap;
            }
        },
        e => e,
    };
    run(e, &mut m, ctx);
    m.info("Versión", Some(&ctx.version));
    m.info("Base de datos", Some(&ctx.database).filter(|d| !d.is_empty() && *d != "default"));
    m.snap
}

/// The engine behind the generic preset, from SQL_DBMS_NAME.
pub fn eng_from_dbms(dbms: &str) -> Option<Eng> {
    let d = dbms.to_ascii_lowercase();
    let has = |s: &str| d.contains(s);
    Some(if has("microsoft sql server") || has("azure sql") {
        Eng::SqlServer
    } else if has("adaptive server") || has("sybase") {
        Eng::Ase
    } else if has("sql anywhere") {
        Eng::Sqla
    } else if d.starts_with("dsn") || has("db2 for z") {
        Eng::Db2zos
    } else if (d.starts_with("as") && has("400")) || has("db2 for i") {
        Eng::Db2i
    } else if has("db2") {
        Eng::Db2
    } else if has("impala") {
        Eng::Impala
    } else if has("spark") || has("kyuubi") {
        Eng::Spark
    } else if has("hive") {
        Eng::Hive
    } else if has("teradata") {
        Eng::Teradata
    } else if has("vertica") {
        Eng::Vertica
    } else if has("netezza") {
        Eng::Netezza
    } else if has("exa") && has("sol") {
        Eng::Exasol
    } else if has("cubrid") {
        Eng::Cubrid
    } else if has("informix") || has("gbase") {
        Eng::Informix
    } else if has("altibase") {
        Eng::Altibase
    } else if d == "dm" || has("dameng") || has("dm8") {
        Eng::Dameng
    } else if has("monetdb") {
        Eng::MonetDb
    } else if has("virtuoso") {
        Eng::Virtuoso
    } else if has("ingres") || has("vector") {
        Eng::Ingres
    } else if has("mimer") {
        Eng::Mimer
    } else if has("iris") || has("cache") || has("caché") {
        Eng::Iris
    } else if has("openedge") || has("progress") {
        Eng::OpenEdge
    } else if has("sqream") {
        Eng::Sqream
    } else if has("maxdb") || has("sap db") {
        Eng::MaxDb
    } else if has("nuodb") {
        Eng::NuoDb
    } else if has("heavy") || has("omnisci") || has("mapd") {
        Eng::HeavyDb
    } else if has("machbase") {
        Eng::Machbase
    } else if has("ignite") {
        Eng::Ignite
    } else {
        return None;
    })
}

fn run(e: Eng, m: &mut Mon, ctx: &Ctx) {
    match e {
        Eng::Generic => {}
        Eng::SqlServer => sqlserver(m),
        Eng::Db2 => db2(m),
        Eng::Db2i => db2i(m),
        Eng::Db2zos => db2zos(m),
        Eng::Ase => ase(m),
        Eng::Sqla => sqla(m),
        Eng::Hive => hive(m),
        Eng::Impala => impala(m),
        Eng::Informix => informix(m, ctx),
        Eng::Teradata => teradata(m),
        Eng::Vertica => vertica(m),
        Eng::Exasol => exasol(m),
        Eng::Netezza => netezza(m),
        Eng::Altibase => altibase(m),
        Eng::Cubrid => cubrid(m),
        Eng::Dameng => dameng(m),
        Eng::Ocient => ocient(m),
        Eng::MonetDb => monetdb(m),
        Eng::Virtuoso => virtuoso(m),
        Eng::Ingres => ingres(m, ctx),
        Eng::Iris => iris(m),
        Eng::OpenEdge => openedge(m),
        Eng::Sqream => sqream(m),
        Eng::MaxDb => maxdb(m),
        Eng::NuoDb => nuodb(m),
        Eng::HeavyDb => heavydb(m, ctx),
        Eng::Machbase => machbase(m),
        Eng::Ignite => ignite(m),
        Eng::Ignite3 => ignite3(m),
        Eng::Access | Eng::DBase => files(e, m, ctx),
        Eng::Spark | Eng::Zen | Eng::NetSuite | Eng::Mimer => {
            if let Some(why) = unsupported_reason_eng(e) {
                m.note(why);
            }
        }
    }
}

fn unsupported_reason_eng(e: Eng) -> Option<&'static str> {
    match e {
        Eng::Spark => Some("Spark y Kyuubi no exponen métricas del servidor por SQL (se ven en la interfaz web de Spark)."),
        Eng::Zen => Some("Actian Zen no expone métricas por SQL (se ven en Zen Monitor)."),
        Eng::NetSuite => Some("SuiteAnalytics Connect no informa métricas del servicio."),
        Eng::Mimer => Some("Mimer SQL no expone métricas por SQL (se ven con sqlmonitor)."),
        _ => None,
    }
}

// ------------------------------------------------------------ SQL Server

/// SQL Server reached with the generic preset (the dedicated driver has
/// its own, fuller monitor).
fn sqlserver(m: &mut Mon) {
    const NEEDS: &str = "Sin el permiso VIEW SERVER STATE solo se ven la sesión propia y parte de las cifras.";
    if let Some(s) = m.q(
        "Información del sistema",
        "SELECT cpu_count, physical_memory_kb, committed_kb, committed_target_kb,
                DATEDIFF(SECOND, sqlserver_start_time, SYSDATETIME()) AS uptime_s
           FROM sys.dm_os_sys_info",
    ) {
        m.std_max("mem_used", times(s.first("committed_kb"), KB), times(s.first("committed_target_kb"), KB));
        m.std("uptime", s.first("uptime_s"));
        m.info("CPU lógicas", s.text("cpu_count"));
        m.info("Memoria física", s.first("physical_memory_kb").map(|k| format!("{:.1} GB", k * KB / GB)));
    } else {
        m.note(NEEDS);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 1
                x.value('(./Record/SchedulerMonitorEvent/SystemHealth/ProcessUtilization)[1]', 'int') AS sql_cpu,
                100 - x.value('(./Record/SchedulerMonitorEvent/SystemHealth/SystemIdle)[1]', 'int') AS total_cpu
           FROM (SELECT CONVERT(xml, record) AS x, [timestamp] AS ts FROM sys.dm_os_ring_buffers
                  WHERE ring_buffer_type = N'RING_BUFFER_SCHEDULER_MONITOR' AND record LIKE N'%<SystemHealth>%') r
          ORDER BY ts DESC",
    ) {
        m.std("cpu", s.first("total_cpu"));
        m.metric(Metric::new("sql_cpu", "CPU de SQL Server", "CPU", U::Percent, s.first("sql_cpu")));
    }
    if let Some(s) = m.q(
        "Contadores de rendimiento",
        "SELECT RTRIM(counter_name) AS name, cntr_value AS value FROM sys.dm_os_performance_counters
          WHERE (counter_name IN ('Batch Requests/sec', 'User Connections', 'Processes blocked', 'Database pages',
                                  'Buffer cache hit ratio', 'Buffer cache hit ratio base')
                 AND object_name NOT LIKE '%Partition%')
             OR (counter_name IN ('Transactions/sec', 'Number of Deadlocks/sec') AND instance_name = '_Total')",
    ) {
        let kv = s.kv();
        let g = |k: &str| kv.get(k).and_then(|v| parse(v));
        m.std("connections", g("user connections"));
        m.std("queries", g("batch requests/sec"));
        m.std("transactions", g("transactions/sec"));
        m.std("deadlocks", g("number of deadlocks/sec"));
        m.std("locks_waiting", g("processes blocked"));
        m.std("mem_cache", times(g("database pages"), 8192.0));
        match (g("buffer cache hit ratio"), g("buffer cache hit ratio base")) {
            (Some(h), Some(b)) if b > 0.0 => m.std("cache_hit", Some((h / b * 100.0).min(100.0))),
            _ => {}
        }
    }
    if let Some(s) = m.q_quiet("SELECT @@SERVERNAME AS srv, SERVERPROPERTY('Edition') AS ed, @@MAX_CONNECTIONS AS maxc") {
        m.info("Servidor", s.text("srv"));
        m.info("Edición", s.text("ed"));
        m.info("Máximo de conexiones", s.text("maxc"));
    }
    if let Some(s) = m.q(
        "Sesiones",
        "SELECT TOP 200 s.session_id, s.login_name, DB_NAME(s.database_id) AS db, s.host_name, COALESCE(r.status, s.status) AS status,
                DATEDIFF(SECOND, COALESCE(r.start_time, s.last_request_start_time), GETDATE()) AS secs,
                LEFT(t.text, 2000) AS sql_text
           FROM sys.dm_exec_sessions s
           LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
           OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
          WHERE s.is_user_process = 1
          ORDER BY CASE WHEN r.session_id IS NULL THEN 1 ELSE 0 END, s.session_id",
    ) {
        m.std("active_sessions", Some(s.count_where("status", |v| !v.eq_ignore_ascii_case("sleeping"))));
        m.table("sessions", "Sesiones", &["ID", "Usuario", "Base", "Cliente", "Estado", "Duración (s)", "Consulta actual"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 200 session_id, blocking_session_id, wait_type, wait_time, DB_NAME(database_id) AS db
           FROM sys.dm_exec_requests WHERE blocking_session_id <> 0",
    ) {
        m.table("locks", "Bloqueos / esperas", &["Sesión", "Bloqueada por", "Espera", "Tiempo (ms)", "Base"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 10 wait_type, waiting_tasks_count, wait_time_ms FROM sys.dm_os_wait_stats
          WHERE wait_type NOT LIKE 'SLEEP%' AND wait_type NOT LIKE '%QUEUE%' AND wait_type NOT LIKE 'XE%'
            AND wait_type NOT LIKE 'BROKER%' AND wait_type NOT LIKE 'SQLTRACE%' AND wait_type NOT LIKE 'HADR%'
            AND wait_type NOT IN ('CHECKPOINT_QUEUE', 'WAITFOR', 'DIRTY_PAGE_POLL', 'SOS_WORK_DISPATCHER', 'CLR_AUTO_EVENT', 'CLR_MANUAL_EVENT')
          ORDER BY wait_time_ms DESC",
    ) {
        m.table("waits", "Esperas principales", &["Tipo", "Esperas", "Tiempo total (ms)"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT d.name, d.state_desc, SUM(CAST(f.size AS bigint)) * 8192 AS bytes
           FROM sys.databases d JOIN sys.master_files f ON f.database_id = d.database_id
          GROUP BY d.name, d.state_desc ORDER BY d.name",
    ) {
        m.std("storage_used", s.sum("bytes"));
        m.table("databases", "Bases y tamaños", &["Base", "Estado", "Tamaño (bytes)"], &s);
    }
}

// ------------------------------------------------------------------- Db2

fn db2(m: &mut Mon) {
    let host = m.q("Recursos del sistema (ENV_GET_SYSTEM_RESOURCES)", "SELECT * FROM TABLE(SYSPROC.ENV_GET_SYSTEM_RESOURCES()) AS T");
    let host_mem = host.as_ref().and_then(|s| times(s.sum("MEMORY_TOTAL"), MB));
    if let Some(s) = &host {
        m.std("cpu", s.avg("CPU_USAGE_TOTAL"));
        m.info("Servidor", s.text("HOST_NAME"));
        m.info("Sistema operativo", s.text("OS_NAME").map(|o| format!("{o} {}", s.text("OS_VERSION").unwrap_or_default())));
        m.info("CPU en línea", s.sum("CPU_ONLINE"));
        m.info("Memoria del servidor", host_mem.map(|b| format!("{:.1} GB", b / GB)));
    }
    if let Some(s) = m.q(
        "Memoria (MON_GET_MEMORY_POOL)",
        "SELECT SUM(MEMORY_POOL_USED) AS USED_KB,
                SUM(CASE WHEN MEMORY_POOL_TYPE = 'BP' THEN MEMORY_POOL_USED ELSE 0 END) AS BP_KB
           FROM TABLE(MON_GET_MEMORY_POOL(NULL, NULL, -2)) AS T",
    ) {
        m.std_max("mem_used", times(s.first("USED_KB"), KB), host_mem);
        m.std("mem_cache", times(s.first("BP_KB"), KB));
    }
    if let Some(s) = m.q("Métricas de la base (MON_GET_DATABASE)", "SELECT * FROM TABLE(MON_GET_DATABASE(-2)) AS T") {
        let g = |c: &str| s.sum(c);
        m.std("cpu_time", times(g("TOTAL_CPU_TIME"), 100.0 / 1e6));
        m.std("connections", g("APPLS_CUR_CONS"));
        m.std("active_sessions", g("APPLS_IN_DB2"));
        m.std("queries", g("ACT_COMPLETED_TOTAL"));
        m.std("transactions", add(g("TOTAL_APP_COMMITS"), g("TOTAL_APP_ROLLBACKS")));
        m.std("rows_read", g("ROWS_READ"));
        m.std("rows_written", add(add(g("ROWS_INSERTED"), g("ROWS_UPDATED")), g("ROWS_DELETED")));
        m.std("net_in", g("TCPIP_RECV_VOLUME"));
        m.std("net_out", g("TCPIP_SEND_VOLUME"));
        let logical = add(g("POOL_DATA_L_READS"), g("POOL_INDEX_L_READS"));
        let physical = add(g("POOL_DATA_P_READS"), g("POOL_INDEX_P_READS"));
        m.std("cache_hit", hit_ratio(physical, logical));
        m.metric(Metric::new("physical_reads", "Páginas leídas de disco", "Disco", U::Count, physical).counter());
        m.std("locks_waiting", g("NUM_LOCKS_WAITING"));
        m.std("deadlocks", g("DEADLOCKS"));
        m.metric(Metric::new("lock_timeouts", "Timeouts de bloqueo", "Bloqueos", U::Count, g("LOCK_TIMEOUTS")).counter());
        let waits: Vec<Vec<Value>> = [
            ("Bloqueos", "LOCK_WAIT_TIME"),
            ("Escritura del log", "LOG_DISK_WAIT_TIME"),
            ("Lectura de buffer pool", "POOL_READ_TIME"),
            ("Escritura de buffer pool", "POOL_WRITE_TIME"),
            ("Lectura directa", "DIRECT_READ_TIME"),
            ("Escritura directa", "DIRECT_WRITE_TIME"),
            ("Red (recepción)", "TCPIP_RECV_WAIT_TIME"),
            ("Red (envío)", "TCPIP_SEND_WAIT_TIME"),
            ("Total", "TOTAL_WAIT_TIME"),
        ]
        .iter()
        .filter_map(|(l, c)| Some(vec![Value::from(*l), serde_json::json!(g(c)?)]))
        .collect();
        if !waits.is_empty() {
            m.table_rows("waits", "Esperas principales", &["Tipo", "Tiempo acumulado (ms)"], waits);
        }
    }
    if let Some(s) = m.q_quiet(
        "SELECT TIMESTAMPDIFF(2, CHAR(CURRENT TIMESTAMP - MIN(DB_CONN_TIME))) AS UP FROM TABLE(MON_GET_DATABASE(-2)) AS T",
    ) {
        m.std("uptime", s.first("UP"));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYSIBMADM.ENV_INST_INFO") {
        m.info("Instancia", s.text("INST_NAME"));
        m.info("Nivel de servicio", s.text("SERVICE_LEVEL"));
    }
    if let Some(s) = m.q_quiet("SELECT NAME, VALUE FROM SYSIBMADM.DBMCFG WHERE NAME IN ('max_connections', 'instance_memory')") {
        let kv = s.kv();
        m.info("max_connections", kv.get("max_connections"));
        m.info("instance_memory", kv.get("instance_memory"));
        if let Some(mx) = kv.get("max_connections").and_then(|v| parse(v)).filter(|v| *v > 0.0) {
            if let Some(c) = m.snap.metrics.iter_mut().find(|x| x.key == "connections") {
                c.max = Some(mx);
            }
        }
    }
    if let Some(s) = m.q(
        "Sesiones (MON_GET_CONNECTION)",
        "SELECT C.APPLICATION_HANDLE, C.SESSION_AUTH_ID, CURRENT SERVER AS DB, C.CLIENT_HOSTNAME, C.APPLICATION_NAME,
                COALESCE(Q.ACTIVITY_STATE, 'IDLE') AS STATE, Q.ELAPSED_TIME_SEC,
                CAST(SUBSTR(Q.STMT_TEXT, 1, 2000) AS VARCHAR(2000)) AS STMT
           FROM TABLE(MON_GET_CONNECTION(NULL, -1)) AS C
           LEFT JOIN SYSIBMADM.MON_CURRENT_SQL Q ON Q.APPLICATION_HANDLE = C.APPLICATION_HANDLE
          ORDER BY Q.ELAPSED_TIME_SEC DESC NULLS LAST
          FETCH FIRST 200 ROWS ONLY",
    ) {
        m.table("sessions", "Sesiones", &["ID", "Usuario", "Base", "Cliente", "Aplicación", "Estado", "Duración (s)", "Consulta actual"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT REQ_APPLICATION_HANDLE, HLD_APPLICATION_HANDLE, LOCK_OBJECT_TYPE, LOCK_MODE, LOCK_MODE_REQUESTED,
                TABSCHEMA, TABNAME, LOCK_WAIT_ELAPSED_TIME
           FROM SYSIBMADM.MON_LOCKWAITS FETCH FIRST 200 ROWS ONLY",
    ) {
        m.table(
            "locks",
            "Bloqueos / esperas",
            &["Sesión", "Bloqueada por", "Objeto", "Modo retenido", "Modo pedido", "Esquema", "Tabla", "Espera (s)"],
            &s,
        );
    }
    if let Some(s) = m.q(
        "Espacios de tablas (MON_GET_TABLESPACE)",
        "SELECT TBSP_NAME, TBSP_TYPE, SUM(TBSP_USED_PAGES * TBSP_PAGE_SIZE) AS USED, SUM(TBSP_TOTAL_PAGES * TBSP_PAGE_SIZE) AS TOTAL
           FROM TABLE(MON_GET_TABLESPACE(NULL, -2)) AS T GROUP BY TBSP_NAME, TBSP_TYPE ORDER BY 3 DESC",
    ) {
        m.std_max("storage_used", s.sum("USED"), s.sum("TOTAL"));
        m.table("tablespaces", "Espacios de tablas", &["Nombre", "Tipo", "Usado (bytes)", "Total (bytes)"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT T.TABSCHEMA, T.TABNAME, T.CARD, T.FPAGES * S.PAGESIZE AS BYTES
           FROM SYSCAT.TABLES T JOIN SYSCAT.TABLESPACES S ON S.TBSPACEID = T.TBSPACEID
          WHERE T.TYPE = 'T' AND T.FPAGES > 0 ORDER BY 4 DESC FETCH FIRST 20 ROWS ONLY",
    ) {
        m.table("top_objects", "Objetos más grandes", &["Esquema", "Tabla", "Filas", "Tamaño (bytes)"], &s);
        m.note("Los tamaños de tablas salen de las estadísticas (RUNSTATS): pueden estar desactualizados.");
    }
    if let Some(s) = m.q_quiet(
        "SELECT HADR_ROLE, HADR_STATE, STANDBY_ID, HADR_LOG_GAP,
                TIMESTAMPDIFF(2, CHAR(PRIMARY_LOG_TIME - STANDBY_REPLAY_LOG_TIME)) AS LAG_S
           FROM TABLE(MON_GET_HADR(NULL)) AS T",
    ) {
        if !s.rows.is_empty() {
            m.std("replication_lag", s.avg("LAG_S").map(|v| v.max(0.0)));
            m.info("Rol HADR", s.text("HADR_ROLE"));
            m.table("replication", "Réplicas (HADR)", &["Rol", "Estado", "Standby", "Brecha del log (bytes)", "Retraso (s)"], &s);
        }
    }
}

fn db2zos(m: &mut Mon) {
    if let Some(s) = m.q_quiet(
        "SELECT CURRENT SERVER AS LOC, GETVARIABLE('SYSIBM.SSID') AS SSID,
                GETVARIABLE('SYSIBM.DATA_SHARING_GROUP_NAME') AS DSG FROM SYSIBM.SYSDUMMY1",
    ) {
        m.info("Ubicación", s.text("LOC"));
        m.info("Subsistema", s.text("SSID"));
        m.info("Grupo de data sharing", s.text("DSG"));
    }
    let ts = m.q("Estadísticas en tiempo real", "SELECT SUM(SPACE) AS KB FROM SYSIBM.SYSTABLESPACESTATS");
    let ix = m.q_quiet("SELECT SUM(SPACE) AS KB FROM SYSIBM.SYSINDEXSPACESTATS");
    if ts.is_some() {
        let kb = add(ts.and_then(|s| s.first("KB")), ix.and_then(|s| s.first("KB")));
        m.std("storage_used", times(kb, KB));
    }
    if let Some(s) = m.q_quiet(
        "SELECT DBNAME, NAME, TOTALROWS, SPACE * 1024 AS BYTES FROM SYSIBM.SYSTABLESPACESTATS
          ORDER BY SPACE DESC FETCH FIRST 20 ROWS ONLY",
    ) {
        m.table("top_objects", "Objetos más grandes", &["Base", "Espacio de tablas", "Filas", "Tamaño (bytes)"], &s);
    }
    m.note("Db2 for z/OS no expone por SQL los hilos, la CPU ni la memoria del subsistema: se ven con -DISPLAY THREAD, IFI u OMEGAMON.");
}

fn db2i(m: &mut Mon) {
    if let Some(s) = m.q("Estado del sistema (QSYS2.SYSTEM_STATUS_INFO)", "SELECT * FROM QSYS2.SYSTEM_STATUS_INFO") {
        m.std("cpu", s.first("AVERAGE_CPU_UTILIZATION"));
        m.std_max("storage_used", times(s.first("SYSTEM_ASP_STORAGE"), MB).zip(s.first("SYSTEM_ASP_USED")).map(|(t, p)| t * p / 100.0), times(s.first("SYSTEM_ASP_STORAGE"), MB));
        m.metric(Metric::new("jobs", "Trabajos activos", "Conexiones", U::Count, s.first("ACTIVE_JOBS_IN_SYSTEM")).max(s.first("MAXIMUM_JOBS_IN_SYSTEM")));
        m.metric(Metric::new("temp_storage", "Almacenamiento temporal", "Memoria", U::Bytes, times(s.first("CURRENT_TEMPORARY_STORAGE"), MB)));
        m.info("Servidor", s.text("HOST_NAME"));
        m.info("Partición", s.text("PARTITION_NAME"));
        m.info("CPU configuradas", s.text("CONFIGURED_CPUS"));
        m.info("Memoria principal", s.first("MAIN_STORAGE_SIZE").map(|k| format!("{:.1} GB", k * KB / GB)));
    }
    if let Some(s) = m.q(
        "Trabajos de base de datos (QSYS2.ACTIVE_JOB_INFO)",
        "SELECT JOB_NAME, AUTHORIZATION_NAME, CLIENT_IP_ADDRESS, JOB_STATUS, SQL_STATEMENT_STATUS,
                CAST(SUBSTR(SQL_STATEMENT_TEXT, 1, 2000) AS VARCHAR(2000)) AS SQL_TEXT
           FROM TABLE(QSYS2.ACTIVE_JOB_INFO(JOB_NAME_FILTER => 'QZDASOINIT', DETAILED_INFO => 'ALL')) X
          WHERE JOB_STATUS <> 'PSRW' FETCH FIRST 200 ROWS ONLY",
    ) {
        m.std("connections", Some(s.rows.len() as f64));
        m.std("active_sessions", Some(s.count_where("SQL_STATEMENT_STATUS", |v| v.eq_ignore_ascii_case("ACTIVE"))));
        m.table("sessions", "Sesiones", &["Trabajo", "Usuario", "Cliente", "Estado", "Sentencia", "Consulta actual"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TABLE_SCHEMA, TABLE_NAME, NUMBER_ROWS, DATA_SIZE FROM QSYS2.SYSTABLESTAT
          WHERE TABLE_SCHEMA = CURRENT SCHEMA ORDER BY DATA_SIZE DESC FETCH FIRST 20 ROWS ONLY",
    ) {
        m.table("top_objects", "Objetos más grandes (biblioteca actual)", &["Biblioteca", "Tabla", "Filas", "Tamaño (bytes)"], &s);
    }
    m.note("Los bloqueos de filas en Db2 for i se consultan por tabla (QSYS2.RECORD_LOCK_INFO): no hay una vista global barata.");
}

// ------------------------------------------------------------ Sybase ASE

fn ase(m: &mut Mon) {
    if let Some(s) = m.q(
        "Variables globales",
        "SELECT @@servername AS srv, @@max_connections AS maxc, @@maxpagesize AS pg, @@cpu_busy AS busy, @@timeticks AS tt,
                @@total_read AS rd, @@total_write AS wr",
    ) {
        m.info("Servidor", s.text("srv"));
        m.info("Tamaño de página", s.text("pg"));
        // Ticks of CPU busy × µs per tick.
        if let (Some(b), Some(tt)) = (s.first("busy"), s.first("tt")) {
            m.std("cpu_time", Some(b * tt / 1e6 * 100.0));
        }
        m.metric(Metric::new("disk_reads", "Lecturas de disco", "Disco", U::Count, s.first("rd")).counter());
        m.metric(Metric::new("disk_writes", "Escrituras en disco", "Disco", U::Count, s.first("wr")).counter());
    }
    let mut maxc = None;
    if let Some(s) = m.q_quiet(
        "SELECT f.name, c.value FROM master..sysconfigures f, master..syscurconfigs c
          WHERE f.config = c.config AND f.name IN ('total logical memory', 'max memory', 'number of user connections')",
    ) {
        let kv = s.kv();
        let g = |k: &str| kv.get(k).and_then(|v| parse(v));
        // Memory settings are in 2 KB pages.
        m.std_max("mem_used", times(g("total logical memory"), 2048.0), times(g("max memory"), 2048.0));
        maxc = g("number of user connections");
        m.info("number of user connections", maxc);
    }
    if let Some(s) = m.q(
        "Procesos (sysprocesses)",
        "SELECT COUNT(*) AS n, SUM(CASE WHEN cmd <> 'AWAITING COMMAND' THEN 1 ELSE 0 END) AS act,
                SUM(CASE WHEN blocked > 0 THEN 1 ELSE 0 END) AS blk
           FROM master..sysprocesses WHERE suid > 0",
    ) {
        m.std_max("connections", s.first("n"), maxc);
        m.std("active_sessions", s.first("act"));
        m.std("locks_waiting", s.first("blk"));
    }
    if let Some(s) = m.q_quiet("SELECT datediff(ss, crdate, getdate()) AS up FROM master..sysdatabases WHERE name = 'tempdb'") {
        m.std("uptime", s.first("up"));
    }
    match m.q_quiet("SELECT * FROM master..monState") {
        Some(s) => {
            m.std("transactions", s.first("Transactions"));
            m.std("deadlocks", s.first("NumDeadlocks"));
            m.metric(Metric::new("lock_waits", "Esperas de bloqueo", "Bloqueos", U::Count, s.first("LockWaits")).counter());
        }
        None => m.note(
            "Las tablas MDA (monState, monDataCache, monEngine) no están disponibles: hace falta «enable monitoring» y el rol mon_role para ver transacciones, deadlocks y aciertos de caché.",
        ),
    }
    if let Some(s) = m.q_quiet("SELECT SUM(LogicalReads) AS lr, SUM(PhysicalReads) AS pr FROM master..monDataCache") {
        m.std("cache_hit", hit_ratio(s.first("pr"), s.first("lr")));
    }
    if let Some(s) = m.q_quiet("SELECT SUM(UserCPUTime + SystemCPUTime) AS busy FROM master..monEngine") {
        if let Some(b) = s.first("busy") {
            if let Some(c) = m.snap.metrics.iter_mut().find(|x| x.key == "cpu_time") {
                c.value = Some(b * 100.0);
            }
        }
    }
    if let Some(s) = m.q(
        "Sesiones",
        "SELECT TOP 200 spid, suser_name(suid) AS usr, db_name(dbid) AS db, hostname, program_name, status, cmd, blocked
           FROM master..sysprocesses WHERE suid > 0 ORDER BY spid",
    ) {
        m.table("sessions", "Sesiones", &["ID", "Usuario", "Base", "Cliente", "Programa", "Estado", "Comando", "Bloqueada por"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 200 spid, blocked, db_name(dbid) AS db, cmd, time_blocked FROM master..sysprocesses WHERE blocked > 0",
    ) {
        m.table("locks", "Bloqueos / esperas", &["Sesión", "Bloqueada por", "Base", "Comando", "Espera (s)"], &s);
    }
    if let Some(s) = m.q(
        "Bases de datos",
        "SELECT d.name, SUM(u.size) * @@maxpagesize AS total,
                SUM(curunreservedpgs(u.dbid, u.lstart, u.unreservedpgs)) * @@maxpagesize AS free
           FROM master..sysdatabases d, master..sysusages u WHERE u.dbid = d.dbid GROUP BY d.name ORDER BY d.name",
    ) {
        let used = match (s.sum("total"), s.sum("free")) {
            (Some(t), Some(f)) => Some(t - f),
            _ => None,
        };
        m.std_max("storage_used", used, s.sum("total"));
        m.table("databases", "Bases y tamaños", &["Base", "Tamaño (bytes)", "Libre (bytes)"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 20 user_name(uid) AS sch, name, row_count(db_id(), id) AS nrows, reserved_pages(db_id(), id) * @@maxpagesize AS bytes
           FROM sysobjects WHERE type = 'U' ORDER BY 4 DESC",
    ) {
        m.table("top_objects", "Objetos más grandes (base actual)", &["Dueño", "Tabla", "Filas", "Reservado (bytes)"], &s);
    }
    m.note("El texto de la consulta de cada sesión está en monProcessSQLText (MDA) y no se muestra aquí.");
}

// ---------------------------------------------------------- SQL Anywhere

fn sqla(m: &mut Mon) {
    if let Some(s) = m.q(
        "Propiedades del servidor",
        "SELECT PROPERTY('Name') AS srv, PROPERTY('MachineName') AS host, PROPERTY('ProductVersion') AS ver,
                DATEDIFF(second, PROPERTY('StartTime'), NOW()) AS up, PROPERTY('ProcessCPU') AS cpu,
                PROPERTY('CurrentCacheSize') AS cache_kb, PROPERTY('MaxCacheSize') AS max_cache_kb,
                PROPERTY('BytesReceived') AS net_in, PROPERTY('BytesSent') AS net_out, PROPERTY('ActiveReq') AS active,
                PROPERTY('NumLogicalProcessorsUsed') AS cpus,
                DB_PROPERTY('ConnCount') AS conns, DB_PROPERTY('CacheRead') AS cache_reads, DB_PROPERTY('CacheHits') AS cache_hits,
                DB_PROPERTY('DiskRead') AS disk_read, DB_PROPERTY('DiskWrite') AS disk_write, DB_PROPERTY('PageSize') AS page,
                DB_PROPERTY('FileSize') AS file_pages, DB_PROPERTY('FreePages') AS free_pages, DB_PROPERTY('Commit') AS commits",
    ) {
        let page = s.first("page");
        m.info("Servidor", s.text("srv"));
        m.info("Equipo", s.text("host"));
        m.info("CPU en uso", s.text("cpus"));
        m.std("uptime", s.first("up"));
        m.std("cpu_time", times(s.first("cpu"), 100.0));
        m.std_max("mem_cache", times(s.first("cache_kb"), KB), times(s.first("max_cache_kb"), KB));
        m.std("net_in", s.first("net_in"));
        m.std("net_out", s.first("net_out"));
        m.std("connections", s.first("conns"));
        m.std("active_sessions", s.first("active"));
        m.std("transactions", s.first("commits"));
        m.std("cache_hit", match (s.first("cache_hits"), s.first("cache_reads")) {
            (Some(h), Some(r)) if r > 0.0 => Some((h / r * 100.0).min(100.0)),
            _ => None,
        });
        if let Some(p) = page {
            m.std("disk_read", times(s.first("disk_read"), p));
            m.std("disk_write", times(s.first("disk_write"), p));
            if let Some(f) = s.first("file_pages") {
                m.std_max("storage_used", Some((f - s.first("free_pages").unwrap_or(0.0)) * p), Some(f * p));
            }
        }
    }
    if let Some(s) = m.q(
        "Conexiones (sa_conn_info)",
        "SELECT TOP 200 Number, Userid, DB_NAME(DBNumber) AS db, NodeAddr, CONNECTION_PROPERTY('ReqStatus', Number) AS st,
                LastReqTime, BlockedOn, CAST(LEFT(CONNECTION_PROPERTY('LastStatement', Number), 2000) AS LONG VARCHAR) AS stmt
           FROM sa_conn_info() ORDER BY Number",
    ) {
        m.std("locks_waiting", Some(s.count_where("BlockedOn", |v| v != "0")));
        m.table("sessions", "Sesiones", &["ID", "Usuario", "Base", "Cliente", "Estado", "Último pedido", "Bloqueada por", "Última sentencia"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 200 Number, BlockedOn, LockTable, Userid FROM sa_conn_info() WHERE BlockedOn <> 0",
    ) {
        m.table("locks", "Bloqueos / esperas", &["Sesión", "Bloqueada por", "Tabla", "Usuario"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 20 u.user_name, t.table_name, t.count, (t.table_page_count + t.ext_page_count) * DB_PROPERTY('PageSize') AS bytes
           FROM SYS.SYSTAB t JOIN SYS.SYSUSER u ON u.user_id = t.creator WHERE t.table_type = 1 ORDER BY 4 DESC",
    ) {
        m.table("top_objects", "Objetos más grandes", &["Dueño", "Tabla", "Filas", "Tamaño (bytes)"], &s);
    }
    m.note("SQL Anywhere guarda la última sentencia de cada conexión solo con el servidor iniciado con -zl (RememberLastStatement).");
}

// ----------------------------------------------------------------- Hive

fn hive(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT version() AS v") {
        m.info("Versión de Hive", s.at(0, 0));
    }
    if let Some(s) = m.q("Bloqueos (SHOW LOCKS)", "SHOW LOCKS") {
        m.std("locks_waiting", Some(s.count_where("lock_state", |v| v.eq_ignore_ascii_case("WAITING"))));
        m.table_raw("locks", "Bloqueos", &s);
    }
    if let Some(s) = m.q_quiet("SHOW TRANSACTIONS") {
        m.metric(Metric::new(
            "open_transactions",
            "Transacciones abiertas",
            "Actividad",
            U::Count,
            Some(s.count_where("state", |v| v.eq_ignore_ascii_case("OPEN"))),
        ));
    }
    if let Some(s) = m.q_quiet("SHOW COMPACTIONS") {
        m.metric(Metric::new(
            "compactions",
            "Compactaciones en curso o en cola",
            "Almacenamiento",
            U::Count,
            Some(s.count_where("state", |v| {
                let v = v.to_ascii_lowercase();
                v.contains("initiated") || v.contains("working")
            })),
        ));
        m.table_raw("compactions", "Compactaciones", &s);
    }
    m.note("HiveServer2 no informa CPU, memoria ni sesiones por SQL: se ven en su interfaz web (puerto 10002) o por JMX.");
}

fn impala(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT version() AS v") {
        m.info("Versión de Impala", s.at(0, 0));
    }
    match m.q_quiet(
        "SELECT query_id, db_user, db_name, query_state, total_time_ms, impala_coordinator, substr(sql, 1, 2000) AS sql_text
           FROM sys.impala_query_live ORDER BY total_time_ms DESC LIMIT 200",
    ) {
        Some(s) => {
            m.std("active_sessions", Some(s.rows.len() as f64));
            m.table("queries", "Consultas en curso", &["ID", "Usuario", "Base", "Estado", "Duración (ms)", "Coordinador", "Consulta"], &s);
        }
        None => m.note(
            "Las consultas en curso están en sys.impala_query_live solo desde Impala 4.4 con enable_workload_mgmt; si no, se ven en la interfaz web del impalad (puerto 25000).",
        ),
    }
    m.note("Impala no informa CPU ni memoria por SQL: se ven en la interfaz web de cada impalad y del statestore.");
}

// ------------------------------------------------------ Informix / GBase

fn informix(m: &mut Mon, _ctx: &Ctx) {
    if let Some(s) = m.q_quiet("SELECT DBINFO('dbhostname') AS host, DBINFO('version', 'full') AS ver FROM systables WHERE tabid = 1") {
        m.info("Servidor", s.text("host"));
        m.info("Versión del motor", s.text("ver"));
    }
    if let Some(s) = m.q("Memoria compartida (sysshmvals)", "SELECT sh_curtime - sh_boottime AS up FROM sysmaster:sysshmvals") {
        m.std("uptime", s.first("up"));
    }
    if let Some(s) = m.q_quiet("SELECT SUM(usecs_user + usecs_sys) AS cpu FROM sysmaster:sysvplst") {
        m.std("cpu_time", times(s.first("cpu"), 100.0));
    }
    if let Some(s) = m.q_quiet("SELECT SUM(seg_size) AS sz FROM sysmaster:sysseglst") {
        m.std("mem_used", s.first("sz"));
    }
    if let Some(s) = m.q("Perfil del servidor (sysprofile)", "SELECT name, value FROM sysmaster:sysprofile") {
        let kv = s.kv();
        let g = |k: &str| kv.get(k).and_then(|v| parse(v));
        m.std("transactions", add(g("commits"), g("rollbacks")));
        m.std("deadlocks", g("deadlks"));
        m.std("cache_hit", hit_ratio(g("dskreads"), g("bufreads")));
        m.metric(Metric::new("isam_calls", "Operaciones ISAM", "Actividad", U::Count, g("isamtot")).counter());
        m.metric(Metric::new("disk_reads", "Lecturas de disco", "Disco", U::Count, g("dskreads")).counter());
        m.metric(Metric::new("disk_writes", "Escrituras en disco", "Disco", U::Count, g("dskwrites")).counter());
        m.metric(Metric::new("lock_waits", "Esperas de bloqueo", "Bloqueos", U::Count, g("lockwts")).counter());
        m.metric(Metric::new("seq_scans", "Recorridos secuenciales", "Actividad", U::Count, g("seqscans")).counter());
    }
    if let Some(s) = m.q(
        "Sesiones (syssessions)",
        "SELECT FIRST 200 sid, username, hostname, DBINFO('utc_current') - connected AS secs FROM sysmaster:syssessions ORDER BY sid",
    ) {
        m.std("connections", Some(s.rows.len() as f64));
        m.table("sessions", "Sesiones", &["ID", "Usuario", "Cliente", "Conectada hace (s)"], &s);
    }
    if let Some(s) = m.q_quiet("SELECT FIRST 200 * FROM sysmaster:syssqlcurses") {
        m.table_by("queries", "Consultas en curso", &[("scs_sessionid", "Sesión"), ("scs_sqlstatement", "Consulta")], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT FIRST 200 owner, waiter, dbsname, tabname, type FROM sysmaster:syslocks WHERE waiter IS NOT NULL",
    ) {
        m.std("locks_waiting", Some(s.rows.len() as f64));
        m.table("locks", "Bloqueos / esperas", &["Sesión dueña", "Sesión en espera", "Base", "Tabla", "Tipo"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT d.name, SUM(c.chksize * c.pagesize) AS total, SUM(c.nfree * c.pagesize) AS free
           FROM sysmaster:sysdbspaces d, sysmaster:syschunks c WHERE c.dbsnum = d.dbsnum GROUP BY d.name ORDER BY d.name",
    ) {
        let used = s.sum("total").map(|t| t - s.sum("free").unwrap_or(0.0));
        m.std_max("storage_used", used, s.sum("total"));
        m.table("dbspaces", "Dbspaces", &["Dbspace", "Tamaño (bytes)", "Libre (bytes)"], &s);
    }
}

// --------------------------------------------------------------- Teradata

fn teradata(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT InfoKey, InfoData FROM DBC.DBCInfoV") {
        for (k, v) in s.kv() {
            m.info(&k.to_ascii_uppercase(), Some(v));
        }
    }
    match m.q_quiet("SELECT * FROM TABLE (MonitorPhysicalSummary()) AS t") {
        Some(s) => {
            m.std("cpu", s.first("AvgCPU"));
            m.metric(Metric::new("disk_busy", "Uso de disco", "Disco", U::Percent, s.first("AvgDisk")));
        }
        None => m.note("La CPU del sistema necesita el permiso MONITOR RESOURCE (MonitorPhysicalSummary)."),
    }
    match m.q_quiet("SELECT * FROM TABLE (MonitorSession(-1, '*', 0)) AS t") {
        Some(s) => {
            m.std("connections", Some(s.rows.len() as f64));
            m.std("active_sessions", Some(s.count_where("PEState", |v| !v.to_ascii_uppercase().starts_with("IDLE"))));
            m.std("locks_waiting", Some(s.count_where("Blk_1_SessNo", |v| parse(v).is_some_and(|n| n > 0.0))));
            m.table_by(
                "sessions",
                "Sesiones",
                &[
                    ("SessionNo", "ID"),
                    ("UserName", "Usuario"),
                    ("DefaultDataBase", "Base"),
                    ("LogonTime", "Inicio"),
                    ("PEState", "Estado PE"),
                    ("AMPState", "Estado AMP"),
                    ("AMPCPUSec", "CPU AMP (s)"),
                    ("Blk_1_SessNo", "Bloqueada por"),
                ],
                &s,
            );
        }
        None => {
            m.note("El estado de las sesiones necesita el permiso MONITOR SESSION: se muestran solo las sesiones abiertas.");
            if let Some(s) = m.q(
                "Sesiones (DBC.SessionInfoV)",
                "SELECT TOP 200 SessionNo, UserName, DefaultDataBase, LogonSource FROM DBC.SessionInfoV",
            ) {
                m.std("connections", Some(s.rows.len() as f64));
                m.table("sessions", "Sesiones", &["ID", "Usuario", "Base", "Origen"], &s);
            }
        }
    }
    if let Some(s) = m.q("Espacio (DBC.DiskSpaceV)", "SELECT SUM(CurrentPerm) AS used, SUM(MaxPerm) AS mx FROM DBC.DiskSpaceV") {
        m.std_max("storage_used", s.first("used"), s.first("mx"));
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 200 DatabaseName, SUM(MaxPerm) AS mx, SUM(CurrentPerm) AS cur, SUM(PeakPerm) AS peak
           FROM DBC.DiskSpaceV GROUP BY 1 ORDER BY 3 DESC",
    ) {
        m.table("databases", "Bases y tamaños", &["Base", "Cuota (bytes)", "Usado (bytes)", "Pico (bytes)"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 20 DataBaseName, TableName, SUM(CurrentPerm) AS bytes FROM DBC.TableSizeV
          WHERE DataBaseName = DATABASE GROUP BY 1, 2 ORDER BY 3 DESC",
    ) {
        m.table("top_objects", "Objetos más grandes (base actual)", &["Base", "Tabla", "Tamaño (bytes)"], &s);
    }
}

// ---------------------------------------------------------------- Vertica

fn vertica(m: &mut Mon) {
    if let Some(s) = m.q("Recursos de los nodos (host_resources)", "SELECT * FROM v_monitor.host_resources") {
        let total = s.sum("total_memory_bytes");
        let free = s.sum("total_memory_free_bytes");
        m.std_max("mem_used", total.zip(free).map(|(t, f)| t - f), total);
        m.std("mem_cache", add(s.sum("total_buffer_memory_bytes"), s.sum("total_memory_cache_bytes")));
        m.std_max("storage_used", times(s.sum("disk_space_used_mb"), MB), times(s.sum("disk_space_total_mb"), MB));
        m.info("Nodos (hosts)", Some(s.rows.len()));
        m.info("Procesadores", s.sum("processor_core_count").or(s.sum("processor_count")));
    }
    if let Some(s) = m.q_quiet(
        "SELECT * FROM v_monitor.system_resource_usage
          WHERE end_time = (SELECT MAX(end_time) FROM v_monitor.system_resource_usage)",
    ) {
        m.std("cpu", s.avg("average_cpu_usage_percent"));
        m.metric(Metric::new("net_rx_rate", "Red entrante (por s)", "Red", U::Bytes, times(s.sum("net_rx_kbytes_per_second"), KB)));
        m.metric(Metric::new("net_tx_rate", "Red saliente (por s)", "Red", U::Bytes, times(s.sum("net_tx_kbytes_per_second"), KB)));
        m.metric(Metric::new("io_read_rate", "Lectura de disco (por s)", "Disco", U::Bytes, times(s.sum("io_read_kbytes_per_second"), KB)));
        m.metric(Metric::new("io_write_rate", "Escritura en disco (por s)", "Disco", U::Bytes, times(s.sum("io_written_kbytes_per_second"), KB)));
        m.note("Vertica promedia CPU, red y disco por minuto (system_resource_usage): las cifras cambian una vez por minuto.");
    }
    let maxc = m
        .q_quiet("SELECT MAX(current_value) AS v FROM v_monitor.configuration_parameters WHERE parameter_name = 'MaxClientSessions'")
        .and_then(|s| s.first("v"));
    if let Some(s) = m.q_quiet("SELECT SUM(running_query_count) AS running, SUM(executed_query_count) AS executed FROM v_monitor.query_metrics") {
        m.std("queries", s.first("executed"));
    }
    if let Some(s) = m.q(
        "Sesiones (v_monitor.sessions)",
        "SELECT session_id, user_name, node_name, client_hostname,
                CASE WHEN current_statement <> '' THEN 'activa' ELSE 'inactiva' END AS state,
                DATEDIFF('second', statement_start, NOW()) AS secs, LEFT(current_statement, 2000) AS stmt
           FROM v_monitor.sessions ORDER BY statement_start LIMIT 200",
    ) {
        m.std_max("connections", Some(s.rows.len() as f64), maxc);
        m.std("active_sessions", Some(s.count_where("state", |v| v == "activa")));
        m.table("sessions", "Sesiones", &["ID", "Usuario", "Nodo", "Cliente", "Estado", "Duración (s)", "Consulta actual"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT node_names, object_name, lock_mode, lock_scope, request_timestamp, grant_timestamp, transaction_description
           FROM v_monitor.locks LIMIT 200",
    ) {
        m.std("locks_waiting", Some(s.rows.len() as f64 - s.count_where("grant_timestamp", |_| true)));
        m.table("locks", "Bloqueos", &["Nodos", "Objeto", "Modo", "Alcance", "Pedido", "Otorgado", "Transacción"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT anchor_table_schema, anchor_table_name, SUM(row_count) AS nrows, SUM(used_bytes) AS bytes
           FROM v_monitor.projection_storage GROUP BY 1, 2 ORDER BY 4 DESC LIMIT 20",
    ) {
        m.table("top_objects", "Objetos más grandes", &["Esquema", "Tabla", "Filas", "Tamaño (bytes)"], &s);
    }
    if let Some(s) = m.q_quiet("SELECT node_name, node_state, node_address, catalog_path FROM v_catalog.nodes ORDER BY node_name") {
        m.table("nodes", "Nodos del cluster", &["Nodo", "Estado", "Dirección", "Catálogo"], &s);
    }
}

// ----------------------------------------------------------------- Exasol

fn exasol(m: &mut Mon) {
    if let Some(s) = m.q(
        "Monitor (EXA_MONITOR_LAST_DAY)",
        "SELECT * FROM EXA_STATISTICS.EXA_MONITOR_LAST_DAY ORDER BY MEASURE_TIME DESC LIMIT 1",
    ) {
        m.std("cpu", s.first("CPU"));
        m.metric(Metric::new("load", "Carga", "CPU", U::Count, s.first("LOAD")));
        m.metric(Metric::new("temp_db_ram", "RAM temporal", "Memoria", U::Bytes, times(s.first("TEMP_DB_RAM"), MB)));
        m.metric(Metric::new("hdd_read_rate", "Lectura de disco (por s)", "Disco", U::Bytes, times(s.first("HDD_READ"), MB)));
        m.metric(Metric::new("hdd_write_rate", "Escritura en disco (por s)", "Disco", U::Bytes, times(s.first("HDD_WRITE"), MB)));
        m.metric(Metric::new("net_rate", "Red (por s)", "Red", U::Bytes, times(s.first("NET"), MB)));
        m.note("Exasol toma estas medidas cada 30 segundos (EXA_MONITOR_LAST_DAY).");
    }
    if let Some(s) = m.q_quiet("SELECT * FROM EXA_STATISTICS.EXA_DB_SIZE_LAST_DAY ORDER BY MEASURE_TIME DESC LIMIT 1") {
        m.std("storage_used", times(s.first("STORAGE_SIZE"), GB));
        m.std("mem_used", times(s.first("MEM_OBJECT_SIZE"), GB));
        m.info("RAM recomendada", s.first("RECOMMENDED_DB_RAM_SIZE").map(|g| format!("{g} GiB")));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM EXA_STATISTICS.EXA_SYSTEM_EVENTS ORDER BY MEASURE_TIME DESC LIMIT 1") {
        m.info("RAM de la base", s.first("DB_RAM_SIZE").map(|g| format!("{g} GiB")));
        m.info("Nodos", s.text("NODES"));
    }
    if let Some(s) = m.q_quiet(
        "SELECT SECONDS_BETWEEN(SYSTIMESTAMP, MAX(MEASURE_TIME)) AS UP FROM EXA_STATISTICS.EXA_SYSTEM_EVENTS WHERE EVENT_TYPE = 'STARTUP'",
    ) {
        m.std("uptime", s.first("UP"));
    }
    let sessions = m.q_quiet("SELECT * FROM EXA_DBA_SESSIONS").or_else(|| {
        m.note("Sin permisos de DBA se ven solo las sesiones propias (EXA_ALL_SESSIONS).");
        m.q("Sesiones", "SELECT * FROM EXA_ALL_SESSIONS")
    });
    if let Some(s) = sessions {
        m.std("connections", Some(s.rows.len() as f64));
        m.std("active_sessions", Some(s.count_where("STATUS", |v| !v.eq_ignore_ascii_case("IDLE"))));
        m.std("locks_waiting", Some(s.count_where("STATUS", |v| v.to_ascii_uppercase().contains("WAIT"))));
        m.table_by(
            "sessions",
            "Sesiones",
            &[
                ("SESSION_ID", "ID"),
                ("USER_NAME", "Usuario"),
                ("HOST", "Cliente"),
                ("CLIENT", "Programa"),
                ("STATUS", "Estado"),
                ("DURATION", "Duración"),
                ("ACTIVITY", "Actividad"),
                ("TEMP_DB_RAM", "RAM temporal (MiB)"),
                ("SQL_TEXT", "Consulta actual"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet(
        "SELECT ROOT_NAME, OBJECT_NAME, RAW_OBJECT_SIZE, MEM_OBJECT_SIZE FROM EXA_ALL_OBJECT_SIZES
          WHERE OBJECT_TYPE = 'TABLE' ORDER BY MEM_OBJECT_SIZE DESC LIMIT 20",
    ) {
        m.table("top_objects", "Objetos más grandes", &["Esquema", "Tabla", "Tamaño sin comprimir (bytes)", "Tamaño comprimido (bytes)"], &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT OBJECT_NAME, RAW_OBJECT_SIZE, MEM_OBJECT_SIZE FROM EXA_ALL_OBJECT_SIZES
          WHERE OBJECT_TYPE = 'SCHEMA' ORDER BY MEM_OBJECT_SIZE DESC LIMIT 200",
    ) {
        m.table("databases", "Esquemas y tamaños", &["Esquema", "Tamaño sin comprimir (bytes)", "Tamaño comprimido (bytes)"], &s);
    }
}

// ---------------------------------------------------------------- Netezza

fn netezza(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT * FROM _V_SYSTEM_UTIL ORDER BY ENTRY_TIME DESC LIMIT 1") {
        m.std("cpu", pct(s.first("HOST_CPU")));
        m.metric(Metric::new("spu_cpu", "CPU de los SPU", "CPU", U::Percent, pct(s.first("SPU_CPU"))));
        m.metric(Metric::new("host_memory", "Memoria del host", "Memoria", U::Percent, pct(s.first("HOST_MEMORY"))));
        m.metric(Metric::new("spu_disk", "Uso de disco de los SPU", "Disco", U::Percent, pct(s.first("SPU_DISK"))));
    }
    if let Some(s) = m.q(
        "Sesiones (_V_SESSION)",
        "SELECT ID, USERNAME, DBNAME, IPADDR, STATUS, CONNTIME, SUBSTR(COMMAND, 1, 2000) AS CMD FROM _V_SESSION ORDER BY ID LIMIT 200",
    ) {
        m.std("connections", Some(s.rows.len() as f64));
        m.std("active_sessions", Some(s.count_where("STATUS", |v| v.eq_ignore_ascii_case("active"))));
        m.table("sessions", "Sesiones", &["ID", "Usuario", "Base", "Cliente", "Estado", "Conexión", "Consulta actual"], &s);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM _V_QRYSTAT LIMIT 200") {
        m.table_by(
            "queries",
            "Consultas en curso",
            &[
                ("QS_SESSIONID", "Sesión"),
                ("QS_PLANID", "Plan"),
                ("QS_STATE", "Estado"),
                ("QS_TSUBMIT", "Enviada"),
                ("QS_TSTART", "Inicio"),
                ("QS_ESTCOST", "Costo estimado"),
                ("QS_RESROWS", "Filas"),
                ("QS_SQL", "Consulta"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT * FROM _V_LOCK LIMIT 200") {
        m.table_raw("locks", "Bloqueos", &s);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM _V_TABLE_STORAGE_STAT ORDER BY USED_BYTES DESC LIMIT 20") {
        m.table_by(
            "top_objects",
            "Objetos más grandes",
            &[("DATABASE", "Base"), ("SCHEMA", "Esquema"), ("TABLENAME", "Tabla"), ("USED_BYTES", "Tamaño (bytes)"), ("SKEW", "Sesgo")],
            &s,
        );
    }
}

// --------------------------------------------------------------- Altibase

fn altibase(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT * FROM V$INSTANCE") {
        m.std("uptime", s.first("WORKING_TIME_SEC"));
        m.info("Estado", s.text("STARTUP_PHASE"));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM V$VERSION") {
        m.info("Producto", s.text("PRODUCT_SIGNATURE"));
    }
    if let Some(s) = m.q("Memoria (V$MEMSTAT)", "SELECT SUM(ALLOC_SIZE) AS used, SUM(MAX_TOTAL_SIZE) AS peak FROM V$MEMSTAT") {
        m.std("mem_used", s.first("used"));
        m.metric(Metric::new("mem_peak", "Pico de memoria", "Memoria", U::Bytes, s.first("peak")));
    }
    if let Some(s) = m.q_quiet("SELECT NAME, VALUE FROM V$SYSSTAT") {
        let kv = s.kv();
        let g = |k: &str| kv.get(k).and_then(|v| parse(v));
        m.std("queries", g("execute success count"));
        m.metric(Metric::new("disk_reads", "Páginas leídas de disco", "Disco", U::Count, g("data page read")).counter());
        m.metric(Metric::new("disk_writes", "Páginas escritas en disco", "Disco", U::Count, g("data page write")).counter());
        m.std("cache_hit", hit_ratio(g("data page read"), g("data page gets")));
    }
    if let Some(s) = m.q("Sesiones (V$SESSION)", "SELECT * FROM V$SESSION") {
        m.std("connections", Some(s.rows.len() as f64));
        m.table_by(
            "sessions",
            "Sesiones",
            &[
                ("ID", "ID"),
                ("DB_USERNAME", "Usuario"),
                ("COMM_NAME", "Cliente"),
                ("CLIENT_APP_INFO", "Aplicación"),
                ("SESSION_STATE", "Estado"),
                ("LOGIN_TIME", "Inicio"),
                ("CURRENT_STMT_ID", "Sentencia actual"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT * FROM V$STATEMENT WHERE EXECUTE_FLAG = 1") {
        m.std("active_sessions", Some(s.rows.len() as f64));
        m.table_by("queries", "Consultas en curso", &[("SESSION_ID", "Sesión"), ("ID", "Sentencia"), ("TOTAL_TIME", "Tiempo"), ("QUERY", "Consulta")], &s);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM V$LOCK_WAIT") {
        m.std("locks_waiting", Some(s.rows.len() as f64));
        m.table_raw("locks", "Bloqueos / esperas", &s);
    }
    if let Some(s) = m.q_quiet(
        "SELECT NAME, TOTAL_PAGE_COUNT * PAGE_SIZE AS total, ALLOCATED_PAGE_COUNT * PAGE_SIZE AS used FROM V$TABLESPACES ORDER BY NAME",
    ) {
        m.std_max("storage_used", s.sum("used"), s.sum("total"));
        m.table("tablespaces", "Espacios de tablas", &["Nombre", "Total (bytes)", "Asignado (bytes)"], &s);
    }
}

// ----------------------------------------------------------------- CUBRID

fn cubrid(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT VERSION() AS v") {
        m.info("Versión del motor", s.at(0, 0));
    }
    match m.q_quiet("SHOW TRAN TABLES") {
        Some(s) => {
            m.std("connections", Some(s.rows.len() as f64));
            m.std("active_sessions", Some(s.count_where("Tran_status", |v| v.to_ascii_uppercase().contains("ACTIVE"))));
            m.table_by(
                "sessions",
                "Sesiones",
                &[
                    ("Tran_index", "ID"),
                    ("User_name", "Usuario"),
                    ("Host_name", "Cliente"),
                    ("Program_name", "Programa"),
                    ("Tran_status", "Estado"),
                    ("Query_time", "Tiempo de consulta"),
                    ("Wait_for_lock_holder", "Esperando a"),
                    ("SQL_text", "Consulta actual"),
                ],
                &s,
            );
            m.std("locks_waiting", Some(s.count_where("Wait_for_lock_holder", |v| v != "-1" && !v.is_empty())));
        }
        None => m.note("Las sesiones (SHOW TRAN TABLES) necesitan un usuario DBA y CUBRID 10 o posterior."),
    }
    m.note("CUBRID publica sus estadísticas de servidor (buffer, bloqueos, E/S) con «cubrid statdump», no por SQL.");
}

// ----------------------------------------------------------------- Dameng

fn dameng(m: &mut Mon) {
    if let Some(s) = m.q("Instancia (V$INSTANCE)", "SELECT I.*, DATEDIFF(SS, I.START_TIME, SYSDATE) AS UP_S FROM V$INSTANCE I") {
        m.std("uptime", s.first("UP_S"));
        m.info("Instancia", s.text("INSTANCE_NAME"));
        m.info("Servidor", s.text("HOST_NAME"));
        m.info("Estado", s.text("STATUS$"));
        m.info("Modo", s.text("MODE$"));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM V$SYSTEMINFO") {
        let total = s.first("TOTAL_PHY_SIZE");
        m.metric(Metric::new("host_mem_used", "Memoria del servidor", "Memoria", U::Bytes, total.zip(s.first("FREE_PHY_SIZE")).map(|(t, f)| t - f)).max(total));
        m.info("CPU", s.text("N_CPU"));
    }
    if let Some(s) = m.q_quiet("SELECT SUM(TOTAL_SIZE) AS total FROM V$MEM_POOL") {
        m.std("mem_used", s.first("total"));
    }
    if let Some(s) = m.q_quiet("SELECT SUM(N_PAGES * PAGE_SIZE) AS bytes, AVG(RAT_HIT) AS hit FROM V$BUFFERPOOL") {
        m.std("mem_cache", s.first("bytes"));
        m.std("cache_hit", pct(s.first("hit")));
    }
    if let Some(s) = m.q_quiet("SELECT NAME, STAT_VAL FROM V$SYSSTAT") {
        let kv = s.kv();
        let g = |k: &str| kv.get(k).and_then(|v| parse(v));
        m.std("transactions", g("transaction total count"));
        m.std("queries", g("select statements"));
        m.metric(Metric::new("physical_reads", "Lecturas físicas", "Disco", U::Count, g("physical read count")).counter());
    }
    let maxc = m.q_quiet("SELECT PARA_VALUE FROM V$DM_INI WHERE PARA_NAME = 'MAX_SESSIONS'").and_then(|s| s.first("PARA_VALUE"));
    if let Some(s) = m.q(
        "Sesiones (V$SESSIONS)",
        "SELECT SESS_ID, USER_NAME, CLNT_IP, APPNAME, STATE, CREATE_TIME, SUBSTR(SQL_TEXT, 1, 2000) AS SQL_TEXT FROM V$SESSIONS LIMIT 200",
    ) {
        m.std_max("connections", Some(s.rows.len() as f64), maxc);
        m.std("active_sessions", Some(s.count_where("STATE", |v| v.eq_ignore_ascii_case("ACTIVE"))));
        m.table("sessions", "Sesiones", &["ID", "Usuario", "Cliente", "Aplicación", "Estado", "Inicio", "Consulta actual"], &s);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM V$LOCK WHERE BLOCKED = 1") {
        m.std("locks_waiting", Some(s.rows.len() as f64));
        m.table_by("locks", "Bloqueos / esperas", &[("TRX_ID", "Transacción"), ("LTYPE", "Tipo"), ("LMODE", "Modo"), ("TABLE_ID", "Tabla"), ("ROW_IDX", "Fila")], &s);
    }
    let files = m.q_quiet("SELECT TABLESPACE_NAME, SUM(BYTES) AS total FROM DBA_DATA_FILES GROUP BY TABLESPACE_NAME ORDER BY 1");
    let free = m.q_quiet("SELECT TABLESPACE_NAME, SUM(BYTES) AS free FROM DBA_FREE_SPACE GROUP BY TABLESPACE_NAME").map(|s| s.kv());
    if let Some(s) = files {
        let total = s.sum("total");
        let free_total: Option<f64> = free.as_ref().map(|f| f.values().filter_map(|v| parse(v)).sum());
        m.std_max("storage_used", total.map(|t| t - free_total.unwrap_or(0.0)), total);
        let rows = (0..s.rows.len())
            .map(|r| {
                let name = s.at(r, 0).unwrap_or_default().to_string();
                let f = free.as_ref().and_then(|f| f.get(&name.to_ascii_lowercase())).and_then(|v| parse(v));
                vec![Value::from(name), serde_json::json!(s.num(r, "total")), serde_json::json!(f)]
            })
            .collect();
        m.table_rows("tablespaces", "Espacios de tablas", &["Nombre", "Tamaño (bytes)", "Libre (bytes)"], rows);
    }
    if let Some(s) = m.q_quiet(
        "SELECT OWNER, SEGMENT_NAME, SUM(BYTES) AS bytes FROM DBA_SEGMENTS GROUP BY OWNER, SEGMENT_NAME ORDER BY 3 DESC LIMIT 20",
    ) {
        m.table("top_objects", "Objetos más grandes", &["Dueño", "Objeto", "Tamaño (bytes)"], &s);
    }
}

// ----------------------------------------------------------------- Ocient

fn ocient(m: &mut Mon) {
    if let Some(s) = m.q("Nodos (sys.nodes)", "SELECT * FROM sys.nodes") {
        m.info("Nodos", Some(s.rows.len()));
        m.table_raw("nodes", "Nodos del cluster", &s);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM sys.queries LIMIT 200") {
        m.std("active_sessions", Some(s.rows.len() as f64));
        m.table_raw("queries", "Consultas en curso", &s);
    }
    m.note("Ocient no expone CPU ni memoria por SQL: se ven en su monitoreo (Prometheus / Grafana).");
}

// ---------------------------------------------------------------- MonetDB

const MONET_SYS: &str = "('sys', 'tmp', 'json', 'profiler', 'logging', 'information_schema', 'wlc', 'wlr')";

fn monetdb(m: &mut Mon) {
    let mut max_clients = None;
    if let Some(s) = m.q_quiet(
        "SELECT name, value FROM sys.environment
          WHERE name IN ('monet_version', 'monet_release', 'gdk_nr_threads', 'max_clients', 'gdk_dbpath', 'mapi_port')",
    ) {
        let kv = s.kv();
        m.info("Versión del motor", kv.get("monet_version").map(|v| match kv.get("monet_release") {
            Some(r) => format!("{v} ({r})"),
            None => v.clone(),
        }));
        m.info("Hilos", kv.get("gdk_nr_threads"));
        m.info("Ruta de la base", kv.get("gdk_dbpath"));
        max_clients = kv.get("max_clients").and_then(|v| parse(v));
    }
    if let Some(s) = m.q("Sesiones (sys.sessions)", "SELECT * FROM sys.sessions") {
        m.std_max("connections", Some(s.rows.len() as f64), max_clients);
        m.table_by(
            "sessions",
            "Sesiones",
            &[
                ("sessionid", "ID"),
                ("username", "Usuario"),
                ("hostname", "Cliente"),
                ("application", "Aplicación"),
                ("login", "Inicio"),
                ("idle", "Inactiva desde"),
                ("memorylimit", "Límite de memoria"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT * FROM sys.queue()") {
        m.std("active_sessions", Some(s.count_where("status", |v| v.eq_ignore_ascii_case("running"))));
        m.table_by(
            "queries",
            "Consultas en curso",
            &[("tag", "Etiqueta"), ("sessionid", "Sesión"), ("username", "Usuario"), ("started", "Inicio"), ("status", "Estado"), ("query", "Consulta")],
            &s,
        );
    }
    if let Some(s) = m.q_quiet(
        "SELECT SUM(columnsize + heapsize + hashsize + imprintsize + orderidxsize) AS bytes FROM sys.tablestorage",
    ) {
        m.std("storage_used", s.first("bytes"));
    }
    if let Some(s) = m.q_quiet(&format!(
        "SELECT \"schema\", \"table\", MAX(rowcount) AS nrows,
                SUM(columnsize + heapsize + hashsize + imprintsize + orderidxsize) AS bytes
           FROM sys.tablestorage WHERE \"schema\" NOT IN {MONET_SYS}
          GROUP BY \"schema\", \"table\" ORDER BY bytes DESC LIMIT 20"
    )) {
        m.table("top_objects", "Objetos más grandes", &["Esquema", "Tabla", "Filas", "Tamaño (bytes)"], &s);
    }
    m.note("MonetDB no expone CPU, memoria del proceso ni bloqueos por SQL (usa control de concurrencia optimista).");
}

// --------------------------------------------------------------- Virtuoso

/// Virtuoso's database page.
const VIRTUOSO_PAGE: f64 = 8192.0;

fn virtuoso(m: &mut Mon) {
    if let Some(s) = m.q(
        "Estadísticas (sys_stat)",
        "SELECT sys_stat('st_dbms_ver') AS ver, sys_stat('st_build_date') AS build,
                sys_stat('st_cli_n_current_connections') AS conns, sys_stat('st_cli_max_connected') AS peak,
                sys_stat('st_db_buffers') AS bufs, sys_stat('st_db_used_buffers') AS used_bufs,
                sys_stat('st_db_dirty_buffers') AS dirty, sys_stat('st_db_pages') AS pages,
                sys_stat('st_db_free_pages') AS free_pages, sys_stat('st_proc_running') AS running,
                sys_stat('st_proc_queued_req') AS queued",
    ) {
        m.info("Versión del motor", s.text("ver"));
        m.info("Compilación", s.text("build"));
        m.info("Máximo de conexiones simultáneas alcanzado", s.text("peak"));
        m.std("connections", s.first("conns"));
        m.std("active_sessions", s.first("running"));
        m.metric(Metric::new("queued", "Pedidos en cola", "Conexiones", U::Count, s.first("queued")));
        m.std_max("mem_cache", times(s.first("used_bufs"), VIRTUOSO_PAGE), times(s.first("bufs"), VIRTUOSO_PAGE));
        m.metric(Metric::new("dirty_buffers", "Buffers modificados", "Caché", U::Bytes, times(s.first("dirty"), VIRTUOSO_PAGE)));
        if let Some(p) = s.first("pages") {
            let free = s.first("free_pages").unwrap_or(0.0);
            m.std_max("storage_used", Some((p - free) * VIRTUOSO_PAGE), Some(p * VIRTUOSO_PAGE));
        }
    }
    if let Some(s) = m.q_quiet("SELECT SUM(WAITS) AS waits, SUM(DEADLOCKS) AS dl FROM DB.DBA.SYS_L_STAT") {
        m.std("deadlocks", s.first("dl"));
        m.metric(Metric::new("lock_waits", "Esperas de bloqueo", "Bloqueos", U::Count, s.first("waits")).counter());
    }
    if let Some(s) = m.q_quiet("SELECT SUM(TOUCHES) AS touches, SUM(READS) AS reads FROM DB.DBA.SYS_D_STAT") {
        m.std("cache_hit", hit_ratio(s.first("reads"), s.first("touches")));
        m.metric(Metric::new("disk_reads", "Páginas leídas de disco", "Disco", U::Count, s.first("reads")).counter());
    }
    if let Some(s) = m.q_quiet("SELECT TOP 20 KEY_TABLE, INDEX_NAME, LOCKS, WAITS, DEADLOCKS FROM DB.DBA.SYS_L_STAT ORDER BY WAITS DESC") {
        m.table("locks", "Índices con más esperas de bloqueo", &["Tabla", "Índice", "Bloqueos", "Esperas", "Deadlocks"], &s);
    }
    if let Some(s) = m.q_quiet("status('')") {
        m.table_raw("status", "Estado del servidor (status)", &s);
    }
    m.note("Virtuoso no tiene una vista de sesiones: el resumen de clientes está en el informe de status().");
}

// ----------------------------------------------------------------- Ingres

fn ingres(m: &mut Mon, ctx: &Ctx) {
    if let Some(s) = m.q_quiet("SELECT dbmsinfo('_version') AS ver, dbmsinfo('database') AS db") {
        m.info("Versión del motor", s.text("ver"));
    }
    if let Some(s) = m.q_quiet(
        "SELECT SUM(number_pages * table_pagesize) AS bytes FROM iitables WHERE table_type = 'T'",
    ) {
        m.std("storage_used", s.first("bytes"));
    }
    if let Some(s) = m.q(
        "Tablas (iitables)",
        "SELECT FIRST 20 table_owner, table_name, num_rows, number_pages * table_pagesize AS bytes
           FROM iitables WHERE table_type = 'T' AND system_use <> 'S' ORDER BY 4 DESC",
    ) {
        m.table("top_objects", "Objetos más grandes", &["Dueño", "Tabla", "Filas", "Tamaño (bytes)"], &s);
    }
    if ctx.database.eq_ignore_ascii_case("imadb") {
        if let Some(s) = m.q("Sesiones (ima_server_sessions)", "SELECT FIRST 200 * FROM ima_server_sessions") {
            m.std("connections", Some(s.rows.len() as f64));
            m.table_by(
                "sessions",
                "Sesiones",
                &[
                    ("session_id", "ID"),
                    ("effective_user", "Usuario"),
                    ("db_name", "Base"),
                    ("client_host", "Cliente"),
                    ("session_state", "Estado"),
                    ("session_activity", "Actividad"),
                    ("session_query", "Consulta actual"),
                ],
                &s,
            );
        }
        if let Some(s) = m.q_quiet("SELECT FIRST 200 * FROM ima_locklists") {
            m.table_raw("locks", "Listas de bloqueos", &s);
        }
    } else {
        m.note("Las sesiones, bloqueos y la caché de Ingres están en la IMA: conectate a la base imadb (con privilegios de administrador) para verlas.");
    }
    m.note("Ingres no expone CPU ni memoria del servidor por SQL.");
}

// ---------------------------------------------------- InterSystems IRIS

fn iris(m: &mut Mon) {
    if let Some(s) = m.q("Procesos (%SYS.ProcessQuery)", "SELECT * FROM %SYS.ProcessQuery") {
        let clients = s.count_where("ClientIPAddress", |_| true);
        m.std("connections", Some(clients));
        m.metric(Metric::new("processes", "Procesos", "Conexiones", U::Count, Some(s.rows.len() as f64)));
        m.std("mem_used", times(s.sum("MemoryUsed"), KB));
        m.metric(Metric::new("global_refs", "Referencias a globales", "Actividad", U::Count, s.sum("GlobalReferences")));
        let mut rows: Vec<usize> = (0..s.rows.len()).collect();
        // Client connections first.
        rows.sort_by_key(|&r| s.get(r, "ClientIPAddress").is_none());
        let cols = [
            ("Pid", "ID"),
            ("UserName", "Usuario"),
            ("NameSpace", "Namespace"),
            ("ClientIPAddress", "Cliente"),
            ("ClientExecutableName", "Programa"),
            ("State", "Estado"),
            ("CurrentLineAndRoutine", "Rutina actual"),
            ("CPUTime", "CPU (ms)"),
            ("MemoryUsed", "Memoria (KB)"),
        ];
        let present: Vec<&(&str, &str)> = cols.iter().filter(|(c, _)| s.has(c)).collect();
        let labels: Vec<&str> = present.iter().map(|(_, l)| *l).collect();
        let data = rows.iter().map(|&r| present.iter().map(|(c, _)| cell(s.get(r, c))).collect()).collect();
        m.table_rows("sessions", "Procesos", &labels, data);
    } else {
        m.note("Para ver los procesos, el usuario necesita SELECT sobre %SYS.ProcessQuery en este namespace.");
    }
    if let Some(s) = m.q_quiet("SELECT * FROM INFORMATION_SCHEMA.CURRENT_STATEMENTS") {
        m.std("active_sessions", Some(s.rows.len() as f64));
        m.table_by(
            "queries",
            "Consultas en curso",
            &[
                ("ProcessID", "Proceso"),
                ("UserName", "Usuario"),
                ("Namespace", "Namespace"),
                ("ExecutionStart", "Inicio"),
                ("ExecutionDuration", "Duración"),
                ("Status", "Estado"),
                ("StatementIndexHash", "Sentencia (hash)"),
            ],
            &s,
        );
    }
    m.note("IRIS y Caché no exponen la CPU ni la memoria de la instancia por SQL: se ven en el Portal de administración o en /api/monitor/metrics.");
}

// ------------------------------------------------------ Progress OpenEdge

fn openedge(m: &mut Mon) {
    if let Some(s) = m.q("Actividad (_ActSummary)", "SELECT * FROM PUB.\"_ActSummary\"") {
        let g = |c: &str| s.first(c);
        m.std("uptime", g("_Summary-Uptime"));
        m.std("transactions", add(g("_Summary-Commits"), g("_Summary-Undos")));
        m.std("rows_read", g("_Summary-RecReads"));
        m.std("rows_written", add(add(g("_Summary-RecUpd"), g("_Summary-RecCreat")), g("_Summary-RecDel")));
        m.std("cache_hit", hit_ratio(g("_Summary-DbReads"), g("_Summary-DbAccesses")));
        m.metric(Metric::new("disk_reads", "Bloques leídos de disco", "Disco", U::Count, g("_Summary-DbReads")).counter());
        m.metric(Metric::new("disk_writes", "Bloques escritos en disco", "Disco", U::Count, g("_Summary-DbWrites")).counter());
        m.metric(Metric::new("lock_waits", "Esperas de bloqueo de registros", "Bloqueos", U::Count, g("_Summary-RecWait")).counter());
    }
    if let Some(s) = m.q_quiet("SELECT * FROM PUB.\"_DbStatus\"") {
        let block = s.first("_DbStatus-DbBlkSize");
        if let Some(b) = block {
            m.std_max("storage_used", times(s.first("_DbStatus-HiWater"), b), times(s.first("_DbStatus-TotalBlks"), b));
        }
        m.metric(Metric::new("locks_held", "Bloqueos retenidos", "Bloqueos", U::Count, s.first("_DbStatus-NumLocks")));
        m.info("Iniciada", s.text("_DbStatus-Starttime"));
        m.info("Versión de la base", s.text("_DbStatus-DbVers"));
        m.info("Tamaño de bloque", block);
    }
    if let Some(s) = m.q("Conexiones (_Connect)", "SELECT * FROM PUB.\"_Connect\" WHERE \"_Connect-Usr\" IS NOT NULL") {
        m.std("connections", Some(s.rows.len() as f64));
        m.std("locks_waiting", Some(s.count_where("_Connect-Wait", |v| !v.contains("--"))));
        m.table_by(
            "sessions",
            "Sesiones",
            &[
                ("_Connect-Usr", "ID"),
                ("_Connect-Name", "Usuario"),
                ("_Connect-Type", "Tipo"),
                ("_Connect-Device", "Dispositivo"),
                ("_Connect-IPAddress", "Cliente"),
                ("_Connect-Time", "Inicio"),
                ("_Connect-Wait", "Espera"),
                ("_Connect-TransId", "Transacción"),
                ("_Connect-CacheInfo", "Consulta actual"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT * FROM PUB.\"_AreaStatus\"") {
        m.table_by(
            "areas",
            "Áreas de almacenamiento (bloques)",
            &[
                ("_AreaStatus-Areaname", "Área"),
                ("_AreaStatus-Totblocks", "Total"),
                ("_AreaStatus-Hiwater", "Usados (high water)"),
                ("_AreaStatus-Freenum", "Libres"),
                ("_AreaStatus-Extents", "Extents"),
            ],
            &s,
        );
    }
    m.note("La tabla de bloqueos (_Lock) no se lee porque recorrerla es costoso en bases grandes; se muestran las esperas por conexión.");
    m.note("La consulta actual de cada conexión (_Connect-CacheInfo) solo aparece con el caché de sentencias activado (-SQLStmtCache / promon).");
}

// ------------------------------------------------------------------ SQream

fn sqream(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT SHOW_VERSION()") {
        m.info("Versión del motor", s.at(0, 0));
    }
    if let Some(s) = m.q("Estado del servidor (SHOW_SERVER_STATUS)", "SELECT SHOW_SERVER_STATUS()") {
        let mut ids: Vec<&str> = (0..s.rows.len()).filter_map(|r| s.get(r, "connection_id")).collect();
        ids.sort();
        ids.dedup();
        m.std("connections", Some(ids.len() as f64));
        m.std("active_sessions", Some(s.count_where("statementstatus", |v| !v.eq_ignore_ascii_case("idle"))));
        m.table_by(
            "queries",
            "Sentencias",
            &[
                ("connection_id", "Conexión"),
                ("user_name", "Usuario"),
                ("database_name", "Base"),
                ("clientip", "Cliente"),
                ("instance", "Worker"),
                ("statementid", "Sentencia"),
                ("statementstatus", "Estado"),
                ("statementstarttime", "Inicio"),
                ("statement", "Consulta"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT SHOW_LOCKS()") {
        m.std("locks_waiting", Some(s.rows.len() as f64));
        m.table_by(
            "locks",
            "Bloqueos",
            &[
                ("stmt_id", "Sentencia"),
                ("username", "Usuario"),
                ("locked_object", "Objeto"),
                ("lockmode", "Modo"),
                ("lock_start_time", "Desde"),
                ("stmt_string", "Consulta"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT SUM(size) AS mb FROM sqream_catalog.extents") {
        m.std("storage_used", times(s.first("mb"), MB));
    }
    if let Some(s) = m.q_quiet(
        "SELECT schema_name, table_name, row_count FROM sqream_catalog.tables ORDER BY row_count DESC LIMIT 20",
    ) {
        m.table("top_objects", "Tablas con más filas", &["Esquema", "Tabla", "Filas"], &s);
    }
    m.note("SQream no expone CPU, GPU ni memoria por SQL: se ven con sus herramientas de monitoreo (Studio / métricas del worker).");
}

// ------------------------------------------------------------------ MaxDB

fn maxdb(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT * FROM SYSINFO.VERSION") {
        let v: Vec<String> = ["MAJORVERSION", "MINORVERSION", "CORRECTIONLEVEL", "BUILD"].iter().filter_map(|c| s.text(c)).collect();
        m.info("Versión del kernel", (!v.is_empty()).then(|| v.join(".")));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYSINFO.INSTANCE") {
        m.info("En línea desde", s.text("ONLINESTATEDATE"));
        m.info("Nodo", s.text("NODE"));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYSINFO.MACHINEUTILIZATION") {
        m.std("cpu", s.first("CPULOAD"));
    }
    if let Some(s) = m.q("Datos (SYSINFO.DATASTATISTICS)", "SELECT * FROM SYSINFO.DATASTATISTICS") {
        m.std_max("storage_used", times(s.first("USEDSIZE"), KB), times(s.first("USABLESIZE"), KB));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYSINFO.CACHESTATISTICS") {
        let data = (0..s.rows.len()).find(|&r| s.get(r, "NAME").is_some_and(|n| n.to_ascii_lowercase().contains("data")));
        if let Some(r) = data {
            m.std("cache_hit", s.num(r, "HITRATE"));
        }
        m.table_by(
            "caches",
            "Cachés",
            &[("NAME", "Caché"), ("ACCESSCOUNT", "Accesos"), ("SUCCESSFULACCESSCOUNT", "Aciertos"), ("HITRATE", "Aciertos (%)")],
            &s,
        );
    }
    if let Some(s) = m.q("Sesiones (SYSINFO.SESSIONS)", "SELECT * FROM SYSINFO.SESSIONS") {
        m.std("connections", Some(s.rows.len() as f64));
        m.table_by(
            "sessions",
            "Sesiones",
            &[
                ("SESSIONID", "ID"),
                ("USERNAME", "Usuario"),
                ("CURRENTSCHEMANAME", "Esquema"),
                ("APPLICATIONNODE", "Cliente"),
                ("APPLICATIONPROCESS", "Proceso"),
                ("CONNECTSTATE", "Estado"),
                ("STARTDATE", "Inicio"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT * FROM DOMAIN.LOCKS WHERE ROWNO <= 200") {
        m.table_by(
            "locks",
            "Bloqueos",
            &[
                ("SESSION", "Sesión"),
                ("USERNAME", "Usuario"),
                ("LOCKMODE", "Modo"),
                ("LOCKSTATE", "Estado"),
                ("SCHEMANAME", "Esquema"),
                ("TABLENAME", "Tabla"),
            ],
            &s,
        );
    }
    if let Some(s) = m.q_quiet(
        "SELECT TOP 20 SCHEMANAME, TABLENAME, ROWCOUNT, USEDSIZE * 1024 AS BYTES FROM SYSINFO.TABLESIZE ORDER BY USEDSIZE DESC",
    ) {
        m.table("top_objects", "Objetos más grandes", &["Esquema", "Tabla", "Filas", "Tamaño (bytes)"], &s);
    }
}

// ------------------------------------------------------------------ NuoDB

fn nuodb(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT GETRELEASEVERSION() AS v FROM DUAL") {
        m.info("Versión del motor", s.text("v"));
    }
    if let Some(s) = m.q("Conexiones (SYSTEM.CONNECTIONS)", "SELECT * FROM SYSTEM.CONNECTIONS") {
        m.std("connections", Some(s.rows.len() as f64));
        m.std("active_sessions", Some(s.count_where("SQLSTRING", |_| true)));
        m.metric(Metric::new("conn_memory", "Memoria de las conexiones", "Memoria", U::Bytes, s.sum("MEMUSAGE")));
        m.table_by(
            "sessions",
            "Sesiones",
            &[
                ("CONNID", "ID"),
                ("USER", "Usuario"),
                ("SCHEMA", "Esquema"),
                ("CLIENTHOST", "Cliente"),
                ("CLIENTINFO", "Programa"),
                ("NODEID", "Nodo"),
                ("RUNTIME", "Duración (µs)"),
                ("SQLSTRING", "Consulta actual"),
            ],
            &s,
        );
        m.note("Sin el rol SYSTEM.DBA, SYSTEM.CONNECTIONS muestra solo las conexiones propias.");
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYSTEM.TRANSACTIONS") {
        let blocked = s.count_where("BLOCKEDBY", |v| parse(v).is_some_and(|n| n > 0.0));
        m.std("locks_waiting", Some(blocked));
        m.metric(Metric::new("open_transactions", "Transacciones abiertas", "Actividad", U::Count, Some(s.rows.len() as f64)));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYSTEM.NODES") {
        m.table_by(
            "nodes",
            "Nodos (TE / SM)",
            &[
                ("ID", "ID"),
                ("TYPE", "Tipo"),
                ("STATE", "Estado"),
                ("HOSTNAME", "Host"),
                ("ADDRESS", "Dirección"),
                ("PORT", "Puerto"),
                ("TRIPTIME", "Latencia"),
                ("RELEASE_VER", "Versión"),
            ],
            &s,
        );
    }
    m.note("NuoDB no expone CPU, memoria del proceso ni tamaños de tablas por SQL: se ven con nuocmd y sus métricas.");
}

// ---------------------------------------------------------------- HeavyDB

fn heavydb(m: &mut Mon, ctx: &Ctx) {
    match m.q_quiet("SHOW USER SESSIONS") {
        Some(s) => {
            m.std("connections", Some(s.rows.len() as f64));
            m.table_by(
                "sessions",
                "Sesiones",
                &[("session_id", "ID"), ("login_name", "Usuario"), ("db_name", "Base"), ("client_address", "Cliente")],
                &s,
            );
        }
        None => m.note("SHOW USER SESSIONS solo lo puede usar un superusuario."),
    }
    match m.q_quiet("SHOW QUERIES") {
        Some(s) => {
            m.std("active_sessions", Some(s.count_where("current_status", |v| v.to_ascii_lowercase().contains("running"))));
            m.table_by(
                "queries",
                "Consultas en curso",
                &[
                    ("query_session_id", "ID"),
                    ("login_name", "Usuario"),
                    ("db_name", "Base"),
                    ("current_status", "Estado"),
                    ("submitted", "Enviada"),
                    ("exec_device_type", "Dispositivo"),
                    ("query_str", "Consulta"),
                ],
                &s,
            );
        }
        None => m.note("SHOW QUERIES necesita el servidor con enable-runtime-query-interrupt."),
    }
    if let Some(s) = m.q_quiet("SHOW TABLE DETAILS") {
        let size = |r: usize| add(s.num(r, "total_data_file_size"), s.num(r, "total_metadata_file_size"));
        let total: f64 = (0..s.rows.len()).filter_map(size).sum();
        m.std("storage_used", Some(total));
        let mut idx: Vec<usize> = (0..s.rows.len()).collect();
        idx.sort_by(|a, b| size(*b).unwrap_or(0.0).total_cmp(&size(*a).unwrap_or(0.0)));
        let rows = idx
            .into_iter()
            .take(20)
            .map(|r| vec![cell(s.get(r, "table_name")), cell(s.get(r, "max_rows")), serde_json::json!(size(r))])
            .collect();
        m.table_rows("top_objects", "Objetos más grandes (base actual)", &["Tabla", "Máximo de filas", "Tamaño en disco (bytes)"], rows);
    }
    if ctx.database.eq_ignore_ascii_case("information_schema") {
        if let Some(s) = m.q_quiet("SELECT * FROM memory_summary") {
            let bytes = |dev: &str, col: &str| -> Option<f64> {
                let v: Vec<f64> = (0..s.rows.len())
                    .filter(|&r| s.get(r, "device_type").is_some_and(|d| d.eq_ignore_ascii_case(dev)))
                    .filter_map(|r| Some(s.num(r, col)? * s.num(r, "page_size")?))
                    .collect();
                (!v.is_empty()).then(|| v.iter().sum())
            };
            m.std_max("mem_used", bytes("CPU", "used_page_count"), bytes("CPU", "max_page_count"));
            m.metric(
                Metric::new("gpu_mem_used", "Memoria GPU usada", "Memoria", U::Bytes, bytes("GPU", "used_page_count"))
                    .max(bytes("GPU", "max_page_count")),
            );
        }
    } else {
        m.note("La memoria de CPU y GPU está en information_schema.memory_summary: conectate a la base information_schema para verla.");
    }
}

// --------------------------------------------------------------- Machbase

fn machbase(m: &mut Mon) {
    if let Some(s) = m.q_quiet("SELECT * FROM V$VERSION") {
        m.info("Versión del motor", s.text("BINARY_SIGNATURE"));
    }
    if let Some(s) = m.q("Sesiones (V$SESSION)", "SELECT * FROM V$SESSION") {
        let open: Vec<usize> = (0..s.rows.len()).filter(|&r| s.num(r, "CLOSED") != Some(1.0)).collect();
        m.std("connections", Some(open.len() as f64));
        let cols = [
            ("ID", "ID"),
            ("USER_NAME", "Usuario"),
            ("CURRENT_DB_NAME", "Base"),
            ("USER_IP", "Cliente"),
            ("CLIENT_TYPE", "Tipo"),
            ("LOGIN_TIME", "Inicio"),
        ];
        let present: Vec<&(&str, &str)> = cols.iter().filter(|(c, _)| s.has(c)).collect();
        let labels: Vec<&str> = present.iter().map(|(_, l)| *l).collect();
        let rows = open.iter().map(|&r| present.iter().map(|(c, _)| cell(s.get(r, c))).collect()).collect();
        m.table_rows("sessions", "Sesiones", &labels, rows);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM V$STMT") {
        m.std("active_sessions", Some(s.rows.len() as f64));
        m.table_by("queries", "Sentencias", &[("ID", "ID"), ("SESS_ID", "Sesión"), ("STATE", "Estado"), ("QUERY", "Consulta")], &s);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM V$SYSMEM") {
        m.std("mem_used", s.sum("USAGE"));
        m.metric(Metric::new("mem_peak", "Pico de memoria", "Memoria", U::Bytes, s.sum("MAX_USAGE")));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM V$STORAGE_USAGE") {
        m.std_max("storage_used", s.first("USED_SPACE"), s.first("TOTAL_SPACE"));
    }
    m.note("Machbase no expone CPU ni tiempo activo por SQL.");
}

// ----------------------------------------------------------------- Ignite

fn ignite(m: &mut Mon) {
    if let Some(s) = m.q("Métricas de los nodos (SYS.NODE_METRICS)", "SELECT * FROM SYS.NODE_METRICS") {
        m.std("cpu", pct(s.avg("CUR_CPU_LOAD")));
        m.std_max("mem_used", s.sum("HEAP_MEMORY_USED"), s.sum("HEAP_MEMORY_MAX"));
        m.metric(Metric::new("nonheap_used", "Memoria fuera del heap", "Memoria", U::Bytes, s.sum("NONHEAP_MEMORY_USED")));
        let up = (0..s.rows.len()).filter_map(|r| s.num(r, "UPTIME")).fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))));
        m.std("uptime", up.map(|ms| ms / 1000.0));
        m.metric(Metric::new("threads", "Hilos", "CPU", U::Count, s.sum("CUR_THREAD_COUNT")));
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYS.NODES") {
        m.info("Nodos", Some(s.rows.len()));
        m.info("Versión del motor", s.text("VERSION"));
        m.table_by(
            "nodes",
            "Nodos del cluster",
            &[("NODE_ID", "ID"), ("CONSISTENT_ID", "ID consistente"), ("IS_CLIENT", "Cliente"), ("HOSTNAMES", "Hosts"), ("ADDRESSES", "Direcciones"), ("VERSION", "Versión")],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYS.CLIENT_CONNECTIONS") {
        m.std("connections", Some(s.rows.len() as f64));
        m.table_by(
            "sessions",
            "Conexiones de clientes",
            &[("CONNECTION_ID", "ID"), ("USER", "Usuario"), ("REMOTE_ADDRESS", "Cliente"), ("TYPE", "Tipo"), ("VERSION", "Versión")],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYS.SQL_QUERIES") {
        m.std("active_sessions", Some(s.rows.len() as f64));
        m.table_by(
            "queries",
            "Consultas en curso",
            &[("QUERY_ID", "ID"), ("SCHEMA_NAME", "Esquema"), ("START_TIME", "Inicio"), ("DURATION", "Duración (ms)"), ("SQL", "Consulta")],
            &s,
        );
    }
    if let Some(s) = m.q_quiet("SELECT COUNT(*) AS n FROM SYS.TRANSACTIONS") {
        m.metric(Metric::new("open_transactions", "Transacciones abiertas", "Actividad", U::Count, s.first("n")));
    }
    m.note("Ignite no informa el tamaño de las tablas por SQL: se ve con las métricas de las regiones de datos (JMX / control.sh).");
}

fn ignite3(m: &mut Mon) {
    if let Some(s) = m.q("Consultas (SYSTEM.SQL_QUERIES)", "SELECT * FROM SYSTEM.SQL_QUERIES") {
        m.std("active_sessions", Some(s.rows.len() as f64));
        m.table_raw("queries", "Consultas en curso", &s);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYSTEM.LOCKS") {
        m.metric(Metric::new("locks_held", "Bloqueos", "Bloqueos", U::Count, Some(s.rows.len() as f64)));
        m.table_raw("locks", "Bloqueos", &s);
    }
    if let Some(s) = m.q_quiet("SELECT * FROM SYSTEM.TRANSACTIONS") {
        m.metric(Metric::new("open_transactions", "Transacciones abiertas", "Actividad", U::Count, Some(s.rows.len() as f64)));
    }
    m.note("Ignite 3 no expone CPU ni memoria por SQL: se ven con sus métricas (JMX, REST u OpenTelemetry).");
}

// ------------------------------------------------------- Access / dBase

fn files(e: Eng, m: &mut Mon, ctx: &Ctx) {
    use std::path::Path;
    let Some(path) = ctx.file.as_deref() else {
        m.note("No se indicó el archivo de la base.");
        return;
    };
    let p = Path::new(path);
    if e == Eng::Access {
        match std::fs::metadata(p) {
            Ok(md) => {
                m.std("storage_used", Some(md.len() as f64));
                m.info("Archivo", Some(path));
                if let Ok(t) = md.modified() {
                    let t: chrono_lite::Time = t.into();
                    m.info("Modificado", Some(t.0));
                }
            }
            Err(e) => m.note(format!("No se pudo leer el archivo «{path}»: {e}.")),
        }
        m.note("Access es un archivo: no hay un servidor que informe CPU, memoria, sesiones ni bloqueos.");
        return;
    }
    let dir = if p.is_file() { p.parent().unwrap_or(p) } else { p };
    let entries = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            m.note(format!("No se pudo leer la carpeta «{}»: {e}.", dir.display()));
            return;
        }
    };
    let mut tables: HashMap<String, (f64, f64)> = HashMap::new();
    let mut total = 0.0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((stem, ext)) = name.rsplit_once('.') else { continue };
        let ext = ext.to_ascii_lowercase();
        if !["dbf", "mdx", "ndx", "dbt", "fpt", "cdx"].contains(&ext.as_str()) {
            continue;
        }
        let size = entry.metadata().map(|md| md.len() as f64).unwrap_or(0.0);
        total += size;
        let t = tables.entry(stem.to_string()).or_default();
        if ext == "dbf" {
            t.0 += size;
        } else {
            t.1 += size;
        }
    }
    tables.retain(|_, (d, _)| *d > 0.0);
    m.std("storage_used", Some(total));
    m.info("Carpeta", Some(dir.display()));
    m.info("Tablas (.dbf)", Some(tables.len()));
    let mut list: Vec<(String, (f64, f64))> = tables.into_iter().collect();
    list.sort_by(|a, b| (b.1 .0 + b.1 .1).total_cmp(&(a.1 .0 + a.1 .1)));
    let rows = list
        .into_iter()
        .take(20)
        .map(|(n, (d, x))| vec![Value::from(n), serde_json::json!(d), serde_json::json!(x)])
        .collect();
    m.table_rows("top_objects", "Tablas más grandes", &["Tabla", "Datos (bytes)", "Índices y memos (bytes)"], rows);
    m.note("Los archivos dBase no tienen servidor: no hay CPU, memoria, sesiones ni bloqueos que informar.");
}

/// `SystemTime` as local-agnostic UTC text, without a date crate.
mod chrono_lite {
    pub struct Time(pub String);

    impl From<std::time::SystemTime> for Time {
        fn from(t: std::time::SystemTime) -> Self {
            let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
            // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z.rem_euclid(146_097);
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let mo = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = yoe + era * 400 + i64::from(mo <= 2);
            Time(format!("{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem % 3600 / 60, rem % 60))
        }
    }
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;

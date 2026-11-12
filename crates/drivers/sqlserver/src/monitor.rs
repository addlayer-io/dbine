//! Server monitor from the dynamic management views: CPU (scheduler ring
//! buffer, or `sys.dm_db_resource_stats` in Azure SQL Database), memory,
//! performance counters, sessions, requests, locks, waits, file sizes and
//! Always On replicas. Every section is its own query: one refused for
//! lack of VIEW SERVER STATE (or missing in this edition) is left out with
//! a note instead of failing the snapshot.

use crate::variant::Variant;
use crate::{cell, text, SqlServerSession};
use dbine_driver::monitor::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use dbine_driver::Result;
use tiberius::Row;

/// Which set of views the server has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// SQL Server on a machine, or Azure SQL Managed Instance.
    Server,
    /// Azure SQL Database (EngineEdition 5): database-scoped DMVs.
    AzureDb,
    /// Fabric Data Warehouse / Synapse (EngineEdition 6, 11, 12…): only the
    /// exec_* views and Query Insights.
    Warehouse,
}

/// A number from a column of any numeric (or numeric text) type.
pub(crate) fn f(r: &Row, i: usize) -> Option<f64> {
    if let Ok(Some(v)) = r.try_get::<f64, _>(i) {
        return Some(v);
    }
    if let Ok(Some(v)) = r.try_get::<i64, _>(i) {
        return Some(v as f64);
    }
    if let Ok(Some(v)) = r.try_get::<i32, _>(i) {
        return Some(v as f64);
    }
    if let Ok(Some(v)) = r.try_get::<i16, _>(i) {
        return Some(v as f64);
    }
    if let Ok(Some(v)) = r.try_get::<u8, _>(i) {
        return Some(v as f64);
    }
    if let Ok(Some(v)) = r.try_get::<f32, _>(i) {
        return Some(v as f64);
    }
    if let Ok(Some(v)) = r.try_get::<tiberius::numeric::Numeric, _>(i) {
        return Some(f64::from(v));
    }
    if let Ok(Some(v)) = r.try_get::<bool, _>(i) {
        return Some(if v { 1.0 } else { 0.0 });
    }
    r.try_get::<&str, _>(i)
        .ok()
        .flatten()
        .and_then(dbine_driver::monitor::num)
}

/// What the probes couldn't read, folded into notes at the end.
#[derive(Default)]
pub(crate) struct Gaps {
    /// Parts refused for lack of permission.
    denied: Vec<&'static str>,
    /// Other failures, as notes.
    notes: Vec<String>,
}

impl Gaps {
    pub(crate) fn into_notes(self, permission: &str) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.denied.is_empty() {
            notes.push(format!(
                "Sin el permiso {permission} no se ven: {}.",
                self.denied.join(", ")
            ));
        }
        notes.extend(self.notes);
        notes
    }
}

fn denied(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("permission")
        || m.contains("permiso")
        || m.contains("view server")
        || m.contains("view database")
}

impl SqlServerSession {
    /// Rows of a monitor query, or `None` (and a gap) when it fails.
    pub(crate) async fn probe(
        &mut self,
        what: &'static str,
        sql: &str,
        gaps: &mut Gaps,
    ) -> Option<Vec<Row>> {
        let res = async {
            self.client
                .simple_query(sql)
                .await?
                .into_first_result()
                .await
        }
        .await;
        match res {
            Ok(rows) => Some(rows),
            Err(e) => {
                let msg = match &e {
                    tiberius::error::Error::Server(t) => t.message().to_string(),
                    other => other.to_string(),
                };
                tracing::debug!("sqlserver monitor: {what}: {msg}");
                if denied(&msg) {
                    if !gaps.denied.contains(&what) {
                        gaps.denied.push(what);
                    }
                } else {
                    gaps.notes.push(format!("No se pudo leer {what}: {msg}"));
                }
                None
            }
        }
    }
}

pub(crate) fn table(
    key: &str,
    title: &str,
    cols: &[&str],
    rows: Option<Vec<Row>>,
) -> Option<MonitorTable> {
    let rows = rows?;
    let mut t = MonitorTable::new(key, title, cols);
    t.rows = rows
        .into_iter()
        .map(|r| r.into_iter().map(crate::cell).collect())
        .collect();
    Some(t)
}

const INFO: &str = "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)),
       CAST(SERVERPROPERTY('Edition') AS nvarchar(128)),
       CAST(SERVERPROPERTY('EngineEdition') AS int),
       CAST(@@SERVERNAME AS nvarchar(256)),
       CAST(SERVERPROPERTY('ProductLevel') AS nvarchar(128)),
       CAST(SERVERPROPERTY('Collation') AS nvarchar(128)),
       CAST(SERVERPROPERTY('IsHadrEnabled') AS int),
       CAST(@@MAX_CONNECTIONS AS float),
       CAST(SERVERPROPERTY('Babelfish') AS int),
       DB_NAME()";

const TIMEZONE: &str = "SELECT CAST(CURRENT_TIMEZONE() AS nvarchar(128))";

const SYS_INFO: &str =
    "SELECT CAST(DATEDIFF(SECOND, sqlserver_start_time, SYSDATETIME()) AS float),
       CAST(cpu_count AS float), CAST(physical_memory_kb AS float) * 1024
  FROM sys.dm_os_sys_info";

const CPU_RING: &str = "SELECT TOP (1)
       CAST(record.value('(./Record/SchedulerMonitorEvent/SystemHealth/ProcessUtilization)[1]', 'int') AS float),
       CAST(record.value('(./Record/SchedulerMonitorEvent/SystemHealth/SystemIdle)[1]', 'int') AS float)
  FROM (SELECT CONVERT(xml, record) AS record, [timestamp] FROM sys.dm_os_ring_buffers
         WHERE ring_buffer_type = N'RING_BUFFER_SCHEDULER_MONITOR' AND record LIKE N'%<SystemHealth>%') AS x
 ORDER BY [timestamp] DESC";

/// The CPU time of the schedulers SQL Server runs queries on, and how many
/// there are (one per CPU it may use).
const CPU_TIME: &str = "SELECT CAST(SUM(total_cpu_usage_ms) AS float), CAST(COUNT(*) AS float)
  FROM sys.dm_os_schedulers WHERE status = N'VISIBLE ONLINE'";

const MEMORY: &str = "SELECT CAST(p.physical_memory_in_use_kb AS float) * 1024,
       CAST(m.total_physical_memory_kb AS float) * 1024, CAST(m.available_physical_memory_kb AS float) * 1024
  FROM sys.dm_os_process_memory p CROSS JOIN sys.dm_os_sys_memory m";

const COUNTERS: &str = "SELECT RTRIM(object_name), RTRIM(counter_name), RTRIM(instance_name), CAST(cntr_value AS float)
  FROM sys.dm_os_performance_counters
 WHERE counter_name IN (N'Batch Requests/sec', N'Transactions/sec', N'Page life expectancy',
         N'Buffer cache hit ratio', N'Buffer cache hit ratio base', N'Number of Deadlocks/sec',
         N'Lock Waits/sec', N'User Connections', N'Processes blocked', N'SQL Compilations/sec',
         N'SQL Re-Compilations/sec', N'Total Server Memory (KB)', N'Target Server Memory (KB)',
         N'Database Cache Memory (KB)', N'Errors/sec', N'Log Flushes/sec', N'Page reads/sec', N'Page writes/sec')
   AND RTRIM(instance_name) IN (N'', N'_Total')";

const FILE_IO: &str = "SELECT CAST(SUM(num_of_bytes_read) AS float), CAST(SUM(num_of_bytes_written) AS float),
       CAST(SUM(num_of_reads) AS float), CAST(SUM(num_of_writes) AS float), CAST(SUM(io_stall) AS float)
  FROM sys.dm_io_virtual_file_stats(NULL, NULL)";

const PACKETS: &str = "SELECT CAST(@@PACK_RECEIVED AS float), CAST(@@PACK_SENT AS float)";

/// Distinct sessions: under MARS a session has a request row per active
/// request, and must still count once.
const SESSION_COUNTS: &str = "SELECT CAST(COUNT(DISTINCT s.session_id) AS float),
       CAST(COUNT(DISTINCT CASE WHEN r.session_id IS NOT NULL AND r.session_id <> @@SPID THEN s.session_id END) AS float)
  FROM sys.dm_exec_sessions s
  LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
 WHERE s.is_user_process = 1";

const LOCK_COUNTS: &str = "SELECT CAST(COUNT(*) AS float),
       CAST(SUM(CASE WHEN request_status = N'WAIT' THEN 1 ELSE 0 END) AS float)
  FROM sys.dm_tran_locks";

/// sys.master_files keeps tempdb's startup size: its current size is in
/// tempdb's own sys.database_files.
const STORAGE: &str = "SELECT CAST(((SELECT SUM(CAST(size AS bigint)) FROM sys.master_files WHERE database_id <> 2)
       + ISNULL((SELECT SUM(CAST(size AS bigint)) FROM tempdb.sys.database_files), 0)) * 8192 AS float)";

const SESSIONS: &str = "SELECT TOP (200) s.session_id, s.login_name, DB_NAME(s.database_id), s.host_name, s.program_name,
       c.client_net_address, COALESCE(r.status, s.status),
       CAST(DATEDIFF(SECOND, COALESCE(r.start_time, s.last_request_start_time), SYSDATETIME()) AS float),
       s.cpu_time, CAST(s.memory_usage AS bigint) * 8, s.logical_reads, s.open_transaction_count,
       LEFT(t.text, 2000)
  FROM sys.dm_exec_sessions s
  LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
  LEFT JOIN sys.dm_exec_connections c ON c.session_id = s.session_id AND c.parent_connection_id IS NULL
  OUTER APPLY sys.dm_exec_sql_text(COALESCE(r.sql_handle, c.most_recent_sql_handle)) t
 WHERE s.is_user_process = 1 AND s.session_id <> @@SPID
 ORDER BY CASE WHEN r.session_id IS NULL THEN 1 ELSE 0 END, s.session_id";

const SESSION_COLS: &[&str] = &[
    "ID",
    "Usuario",
    "Base",
    "Equipo",
    "Programa",
    "Dirección",
    "Estado",
    "Duración (s)",
    "CPU (ms)",
    "Memoria (KB)",
    "Lecturas lógicas",
    "Transacciones abiertas",
    "Consulta",
];

const REQUESTS: &str = "SELECT TOP (200) r.session_id, r.status, r.command, DB_NAME(r.database_id), s.login_name,
       CAST(r.total_elapsed_time AS float) / 1000, r.cpu_time, r.logical_reads, r.writes,
       r.wait_type, r.wait_time, NULLIF(r.blocking_session_id, 0), CAST(r.percent_complete AS float),
       LEFT(SUBSTRING(t.text, r.statement_start_offset / 2 + 1,
            (CASE WHEN r.statement_end_offset <= 0 THEN DATALENGTH(t.text) ELSE r.statement_end_offset END
             - r.statement_start_offset) / 2 + 1), 2000)
  FROM sys.dm_exec_requests r
  JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id
  OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
 WHERE s.is_user_process = 1 AND r.session_id <> @@SPID
 ORDER BY r.total_elapsed_time DESC";

const REQUEST_COLS: &[&str] = &[
    "ID",
    "Estado",
    "Comando",
    "Base",
    "Usuario",
    "Duración (s)",
    "CPU (ms)",
    "Lecturas lógicas",
    "Escrituras",
    "Espera",
    "Espera (ms)",
    "Bloqueada por",
    "% completado",
    "Sentencia",
];

const BLOCKING: &str =
    "SELECT TOP (200) wt.session_id, wt.blocking_session_id, DB_NAME(r.database_id), wt.wait_type,
       CAST(wt.wait_duration_ms AS float), LEFT(wt.resource_description, 400), LEFT(t.text, 2000)
  FROM sys.dm_os_waiting_tasks wt
  LEFT JOIN sys.dm_exec_requests r ON r.session_id = wt.session_id
  OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
 WHERE wt.blocking_session_id IS NOT NULL AND wt.blocking_session_id <> wt.session_id
 ORDER BY wt.wait_duration_ms DESC";

const BLOCKING_COLS: &[&str] = &[
    "Sesión",
    "Bloqueada por",
    "Base",
    "Espera",
    "Espera (ms)",
    "Recurso",
    "Consulta",
];

/// Waits that are idle time, not work waiting on something.
const BENIGN_WAITS: &str = "N'BROKER_EVENTHANDLER', N'BROKER_RECEIVE_WAITFOR', N'BROKER_TASK_STOP',
  N'BROKER_TO_FLUSH', N'BROKER_TRANSMITTER', N'CHECKPOINT_QUEUE', N'CHKPT', N'CLR_AUTO_EVENT',
  N'CLR_MANUAL_EVENT', N'CLR_SEMAPHORE', N'DBMIRROR_DBM_EVENT', N'DBMIRROR_EVENTS_QUEUE',
  N'DBMIRROR_WORKER_QUEUE', N'DBMIRRORING_CMD', N'DIRTY_PAGE_POLL', N'DISPATCHER_QUEUE_SEMAPHORE',
  N'EXECSYNC', N'FSAGENT', N'FT_IFTS_SCHEDULER_IDLE_WAIT', N'FT_IFTSHC_MUTEX', N'HADR_CLUSAPI_CALL',
  N'HADR_FILESTREAM_IOMGR_IOCOMPLETION', N'HADR_LOGCAPTURE_WAIT', N'HADR_NOTIFICATION_DEQUEUE',
  N'HADR_TIMER_TASK', N'HADR_WORK_QUEUE', N'KSOURCE_WAKEUP', N'LAZYWRITER_SLEEP', N'LOGMGR_QUEUE',
  N'MEMORY_ALLOCATION_EXT', N'ONDEMAND_TASK_QUEUE', N'PARALLEL_REDO_DRAIN_WORKER',
  N'PARALLEL_REDO_LOG_CACHE', N'PARALLEL_REDO_TRAN_LIST', N'PARALLEL_REDO_WORKER_SYNC',
  N'PARALLEL_REDO_WORKER_WAIT_WORK', N'PREEMPTIVE_OS_FLUSHFILEBUFFERS', N'PREEMPTIVE_XE_GETTARGETSTATE',
  N'PWAIT_ALL_COMPONENTS_INITIALIZED', N'PWAIT_DIRECTLOGCONSUMER_GETNEXT',
  N'PWAIT_EXTENSIBILITY_CLEANUP_TASK', N'QDS_PERSIST_TASK_MAIN_LOOP_SLEEP', N'QDS_ASYNC_QUEUE',
  N'QDS_CLEANUP_STALE_QUERIES_TASK_MAIN_LOOP_SLEEP', N'QDS_SHUTDOWN_QUEUE', N'REDO_THREAD_PENDING_WORK',
  N'REQUEST_FOR_DEADLOCK_SEARCH', N'RESOURCE_QUEUE', N'SERVER_IDLE_CHECK', N'SNI_HTTP_ACCEPT',
  N'SOS_WORK_DISPATCHER', N'SP_SERVER_DIAGNOSTICS_SLEEP', N'SQLTRACE_BUFFER_FLUSH',
  N'SQLTRACE_INCREMENTAL_FLUSH_SLEEP', N'SQLTRACE_WAIT_ENTRIES', N'UCS_SESSION_REGISTRATION',
  N'VDI_CLIENT_OTHER', N'WAIT_FOR_RESULTS', N'WAITFOR', N'WAITFOR_TASKSHUTDOWN', N'WAIT_XTP_RECOVERY',
  N'WAIT_XTP_HOST_WAIT', N'WAIT_XTP_OFFLINE_CKPT_NEW_LOG', N'WAIT_XTP_CKPT_CLOSE', N'XE_DISPATCHER_JOIN',
  N'XE_DISPATCHER_WAIT', N'XE_TIMER_EVENT', N'XE_LIVE_TARGET_TVF', N'SOS_WORKER_MIGRATION',
  N'STARTUP_DEPENDENCY_MANAGER', N'CXCONSUMER', N'BROKER_DISPATCHER', N'AZURE_IMDS_VERSIONS',
  N'PVS_PREALLOCATE', N'HADR_FABRIC_CALLBACK', N'XE_BUFFERMGR_ALLPROCESSED_EVENT', N'SOS_WORKER_MIGRATION'";

fn waits_sql(view: &str) -> String {
    format!(
        "SELECT TOP (20) wait_type, waiting_tasks_count, CAST(wait_time_ms AS float), CAST(signal_wait_time_ms AS float),
                CAST(max_wait_time_ms AS float),
                CAST(CASE WHEN waiting_tasks_count > 0 THEN wait_time_ms * 1.0 / waiting_tasks_count END AS float)
           FROM {view}
          WHERE wait_time_ms > 0 AND wait_type NOT LIKE N'SLEEP[_]%' AND wait_type NOT IN ({BENIGN_WAITS})
          ORDER BY wait_time_ms DESC"
    )
}

const WAIT_COLS: &[&str] = &[
    "Espera",
    "Tareas",
    "Tiempo (ms)",
    "Señal (ms)",
    "Máximo (ms)",
    "Promedio (ms)",
];

const DATABASES: &str = "SELECT d.name, d.state_desc, d.recovery_model_desc,
       CAST(ROUND(SUM(CASE WHEN f.type = 0 THEN CAST(f.size AS bigint) ELSE 0 END) * 8 / 1024.0, 1) AS float),
       CAST(ROUND(SUM(CASE WHEN f.type = 1 THEN CAST(f.size AS bigint) ELSE 0 END) * 8 / 1024.0, 1) AS float),
       d.compatibility_level, d.log_reuse_wait_desc, d.user_access_desc
  FROM sys.databases d
  LEFT JOIN sys.master_files f ON f.database_id = d.database_id
 GROUP BY d.database_id, d.name, d.state_desc, d.recovery_model_desc, d.compatibility_level,
          d.log_reuse_wait_desc, d.user_access_desc
 ORDER BY d.database_id";

const DATABASE_COLS: &[&str] = &[
    "Base",
    "Estado",
    "Recuperación",
    "Datos (MB)",
    "Log (MB)",
    "Compatibilidad",
    "Espera del log",
    "Acceso",
];

const TOP_OBJECTS: &str = "SELECT TOP (20) s.name + N'.' + o.name, o.type_desc,
       CAST(SUM(CASE WHEN ps.index_id IN (0, 1) THEN ps.row_count ELSE 0 END) AS float),
       CAST(ROUND(SUM(ps.reserved_page_count) * 8 / 1024.0, 2) AS float),
       CAST(ROUND(SUM(ps.used_page_count) * 8 / 1024.0, 2) AS float)
  FROM sys.dm_db_partition_stats ps
  JOIN sys.objects o ON o.object_id = ps.object_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
 WHERE o.is_ms_shipped = 0
 GROUP BY s.name, o.name, o.type_desc
 ORDER BY SUM(ps.reserved_page_count) DESC";

const TOP_OBJECT_COLS: &[&str] = &["Objeto", "Tipo", "Filas", "Reservado (MB)", "Usado (MB)"];

const REPLICAS: &str =
    "SELECT ag.name, ar.replica_server_name, ars.role_desc, ars.synchronization_health_desc,
       DB_NAME(drs.database_id), drs.synchronization_state_desc,
       CAST(drs.log_send_queue_size AS float), CAST(drs.redo_queue_size AS float),
       CAST(drs.secondary_lag_seconds AS float), CAST(ars.is_local AS int)
  FROM sys.dm_hadr_database_replica_states drs
  JOIN sys.availability_replicas ar ON ar.replica_id = drs.replica_id
  JOIN sys.availability_groups ag ON ag.group_id = drs.group_id
  LEFT JOIN sys.dm_hadr_availability_replica_states ars ON ars.replica_id = drs.replica_id
 ORDER BY ag.name, ar.replica_server_name";

const REPLICA_COLS: &[&str] = &[
    "Grupo",
    "Réplica",
    "Rol",
    "Salud",
    "Base",
    "Sincronización",
    "Cola de envío (KB)",
    "Cola de rehacer (KB)",
    "Retraso (s)",
];

// Azure SQL Database: database-scoped views.

const AZ_RESOURCES: &str =
    "SELECT TOP (1) CAST(avg_cpu_percent AS float), CAST(avg_data_io_percent AS float),
       CAST(avg_log_write_percent AS float), CAST(avg_memory_usage_percent AS float),
       CAST(max_worker_percent AS float), CAST(max_session_percent AS float)
  FROM sys.dm_db_resource_stats ORDER BY end_time DESC";

const AZ_STORAGE: &str = "SELECT CAST(SUM(CAST(size AS bigint)) * 8192 AS float),
       CAST(SUM(CASE WHEN type = 0 THEN CAST(FILEPROPERTY(name, 'SpaceUsed') AS bigint) ELSE 0 END) * 8192 AS float),
       CAST(DATABASEPROPERTYEX(DB_NAME(), 'MaxSizeInBytes') AS float),
       CAST(DATABASEPROPERTYEX(DB_NAME(), 'ServiceObjective') AS nvarchar(128)),
       CAST(DATABASEPROPERTYEX(DB_NAME(), 'Edition') AS nvarchar(128))
  FROM sys.database_files";

const AZ_DATABASES: &str = "SELECT d.name, d.state_desc, d.compatibility_level,
       CAST(DATABASEPROPERTYEX(d.name, 'ServiceObjective') AS nvarchar(128)),
       CAST(DATABASEPROPERTYEX(d.name, 'Edition') AS nvarchar(128))
  FROM sys.databases d ORDER BY d.database_id";

// Fabric / Synapse: Query Insights keeps the finished requests.

const WH_HISTORY: &str = "SELECT TOP (50) distributed_statement_id, login_name, status,
       CONVERT(varchar(19), start_time, 120), CAST(total_elapsed_time_ms AS float), CAST(row_count AS float),
       CAST(data_scanned_disk_mb AS float), LEFT(command, 2000)
  FROM queryinsights.exec_requests_history
 WHERE start_time >= DATEADD(HOUR, -1, SYSUTCDATETIME())
 ORDER BY start_time DESC";

const WH_HISTORY_COLS: &[&str] = &[
    "ID",
    "Usuario",
    "Estado",
    "Inicio (UTC)",
    "Duración (ms)",
    "Filas",
    "Leído de disco (MB)",
    "Consulta",
];

const WH_SESSIONS: &str = "SELECT TOP (200) s.session_id, s.login_name, DB_NAME(s.database_id), s.host_name, s.program_name,
       COALESCE(r.status, s.status),
       CAST(DATEDIFF(SECOND, COALESCE(r.start_time, s.last_request_start_time), SYSDATETIME()) AS float),
       r.command
  FROM sys.dm_exec_sessions s
  LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
 WHERE s.is_user_process = 1
 ORDER BY CASE WHEN r.session_id IS NULL THEN 1 ELSE 0 END, s.session_id";

const WH_SESSION_COLS: &[&str] = &[
    "ID",
    "Usuario",
    "Base",
    "Equipo",
    "Programa",
    "Estado",
    "Duración (s)",
    "Comando",
];

const WH_REQUESTS: &str = "SELECT TOP (200) r.session_id, r.status, r.command, s.login_name,
       CAST(r.total_elapsed_time AS float) / 1000, r.cpu_time, r.wait_type,
       NULLIF(r.blocking_session_id, 0), r.distributed_statement_id
  FROM sys.dm_exec_requests r
  JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id
 WHERE s.is_user_process = 1 AND r.session_id <> @@SPID
 ORDER BY r.total_elapsed_time DESC";

const WH_REQUEST_COLS: &[&str] = &[
    "ID",
    "Estado",
    "Comando",
    "Usuario",
    "Duración (s)",
    "CPU (ms)",
    "Espera",
    "Bloqueada por",
    "ID distribuido",
];

/// The performance counters that matter, by counter name (the instance
/// is "" or "_Total"; the object's prefix names the instance).
#[derive(Default, Debug)]
struct Counters(Vec<(String, String, f64)>);

impl Counters {
    fn get(&self, object: &str, counter: &str) -> Option<f64> {
        self.0
            .iter()
            .find(|(o, c, _)| o.ends_with(object) && c == counter)
            .map(|(_, _, v)| *v)
    }
}

pub async fn snapshot(s: &mut SqlServerSession) -> Result<MonitorSnapshot> {
    let mut snap = MonitorSnapshot::default();
    let mut gaps = Gaps::default();
    let info = s
        .probe("la versión", INFO, &mut gaps)
        .await
        .and_then(|r| r.into_iter().next());
    let engine_edition = info.as_ref().and_then(|r| f(r, 2)).unwrap_or(0.0) as i64;
    if info.as_ref().and_then(|r| f(r, 8)) == Some(1.0) {
        // Babelfish answers on the same port: PostgreSQL's views instead.
        return crate::babelfish::monitor(s).await;
    }
    // EngineEdition: 5 Azure SQL Database, 12 SQL database in Fabric,
    // 6 Synapse dedicated pool, 11 Synapse serverless / Fabric warehouse.
    let flavor = match engine_edition {
        _ if s.variant == Variant::Fabric => Flavor::Warehouse,
        5 | 12 => Flavor::AzureDb,
        6 | 11 => Flavor::Warehouse,
        _ => Flavor::Server,
    };
    let mut max_connections = None;
    let mut hadr = false;
    if let Some(r) = &info {
        let edition = text(r, 1).unwrap_or_default();
        snap.info.push((
            "Versión".into(),
            format!(
                "{} {}",
                text(r, 0).unwrap_or_default(),
                text(r, 4).unwrap_or_default()
            )
            .trim()
            .into(),
        ));
        snap.info.push(("Edición".into(), edition));
        if let Some(n) = text(r, 3) {
            snap.info.push(("Servidor".into(), n));
        }
        if let Some(c) = text(r, 5) {
            snap.info.push(("Intercalación".into(), c));
        }
        if let Some(db) = text(r, 9) {
            snap.info.push(("Base actual".into(), db));
        }
        hadr = f(r, 6) == Some(1.0);
        // In Azure SQL Database and Fabric, @@MAX_CONNECTIONS (32767) isn't
        // the database's limit: shown only for a server.
        max_connections = f(r, 7).filter(|m| *m > 0.0 && flavor == Flavor::Server);
        if let Some(m) = max_connections {
            snap.info
                .push(("Conexiones máximas".into(), format!("{m}")));
        }
    }
    if let Some(tz) = s
        .probe("la zona horaria", TIMEZONE, &mut gaps)
        .await
        .and_then(|r| r.into_iter().next())
        .and_then(|r| text(&r, 0))
    {
        snap.info.push(("Zona horaria".into(), tz));
    }
    match flavor {
        Flavor::Server => server(s, &mut snap, &mut gaps, max_connections, hadr).await,
        Flavor::AzureDb => azure_db(s, &mut snap, &mut gaps, engine_edition == 12).await,
        Flavor::Warehouse => warehouse(s, &mut snap, &mut gaps).await,
    }
    let permission = match flavor {
        Flavor::Server => "VIEW SERVER STATE (VIEW SERVER PERFORMANCE STATE desde 2022)",
        _ => "VIEW DATABASE STATE",
    };
    snap.notes.extend(gaps.into_notes(permission));
    Ok(snap)
}

fn first(rows: Option<Vec<Row>>) -> Option<Row> {
    rows.and_then(|r| r.into_iter().next())
}

/// A probe's first row as plain numbers, one per column (`None` for a
/// NULL or a text): the metrics are built from these, so they can be
/// checked without a server.
pub(crate) type Nums = Option<Vec<Option<f64>>>;

pub(crate) fn row_nums(r: &Row) -> Vec<Option<f64>> {
    (0..r.len()).map(|i| f(r, i)).collect()
}

pub(crate) fn nums(rows: Option<Vec<Row>>) -> Nums {
    first(rows).map(|r| row_nums(&r))
}

/// Column `i` of [`Nums`].
pub(crate) fn at(r: &Nums, i: usize) -> Option<f64> {
    r.as_ref().and_then(|r| r.get(i).copied().flatten())
}

fn counters(rows: Option<Vec<Row>>) -> Counters {
    Counters(
        rows.unwrap_or_default()
            .iter()
            .filter_map(|r| Some((text(r, 0)?, text(r, 1)?, f(r, 3)?)))
            .collect(),
    )
}

/// What [`server`] reads.
#[derive(Default)]
struct ServerReadings {
    /// sys.dm_os_sys_info: uptime (s), cpu_count, host memory (bytes).
    sys: Nums,
    /// Scheduler monitor ring buffer: ProcessUtilization, SystemIdle (%).
    ring: Nums,
    /// sys.dm_os_schedulers: CPU ms, VISIBLE ONLINE schedulers.
    sched: Nums,
    counters: Counters,
    /// Process memory in use, host total, host available (bytes).
    mem: Nums,
    /// User sessions, ones running a request.
    sessions: Nums,
    /// File I/O: bytes read, bytes written, reads, writes, stall (ms).
    io: Nums,
    /// @@PACK_RECEIVED, @@PACK_SENT.
    packets: Nums,
    /// Data and log files (bytes).
    storage: Nums,
    /// Lock requests, waiting ones.
    locks: Nums,
    /// @@MAX_CONNECTIONS.
    max_conn: Option<f64>,
}

/// Both ring buffer figures at 0: the scheduler monitor hasn't sampled yet
/// (or can't read the host, as under emulation): unknown, not 100 %.
fn unsampled(ring: &Nums) -> bool {
    at(ring, 0) == Some(0.0) && at(ring, 1) == Some(0.0)
}

const OS_COUNTERS: &str = "sys.dm_os_performance_counters";

async fn server(
    s: &mut SqlServerSession,
    snap: &mut MonitorSnapshot,
    gaps: &mut Gaps,
    max_conn: Option<f64>,
    hadr: bool,
) {
    let r = ServerReadings {
        sys: nums(s.probe("el tiempo activo", SYS_INFO, gaps).await),
        ring: nums(s.probe("el uso de CPU", CPU_RING, gaps).await),
        sched: nums(s.probe("el tiempo de CPU", CPU_TIME, gaps).await),
        counters: counters(
            s.probe("los contadores de rendimiento", COUNTERS, gaps)
                .await,
        ),
        mem: nums(s.probe("la memoria", MEMORY, gaps).await),
        sessions: nums(s.probe("las sesiones", SESSION_COUNTS, gaps).await),
        io: nums(s.probe("la E/S de archivos", FILE_IO, gaps).await),
        packets: nums(s.probe("los paquetes de red", PACKETS, gaps).await),
        storage: nums(s.probe("el tamaño de los archivos", STORAGE, gaps).await),
        locks: nums(s.probe("los bloqueos", LOCK_COUNTS, gaps).await),
        max_conn,
    };
    snap.metrics.extend(server_metrics(&r));
    if r.ring.is_some() && unsampled(&r.ring) {
        snap.notes.push("El monitor de CPU de SQL Server todavía no tiene muestras (se toman cada minuto) o no puede leer el uso del equipo.".into());
    }
    if let Some(n) = at(&r.sys, 1) {
        snap.info.push(("Núcleos".into(), format!("{n}")));
    }
    if let Some(b) = at(&r.sys, 2) {
        snap.info.push((
            "Memoria del equipo".into(),
            format!("{:.1} GB", b / 1_073_741_824.0),
        ));
    }

    // Tables.
    let tables = [
        table(
            "sessions",
            "Sesiones",
            SESSION_COLS,
            s.probe("las sesiones", SESSIONS, gaps).await,
        ),
        table(
            "queries",
            "Consultas en curso",
            REQUEST_COLS,
            s.probe("las consultas en curso", REQUESTS, gaps).await,
        ),
        table(
            "locks",
            "Bloqueos / esperas",
            BLOCKING_COLS,
            s.probe("los bloqueos", BLOCKING, gaps).await,
        ),
        table(
            "waits",
            "Esperas principales (desde el inicio)",
            WAIT_COLS,
            s.probe("las esperas", &waits_sql("sys.dm_os_wait_stats"), gaps)
                .await,
        ),
        table(
            "databases",
            "Bases y tamaños",
            DATABASE_COLS,
            s.probe("los tamaños de las bases", DATABASES, gaps).await,
        ),
        table(
            "top_objects",
            "Objetos más grandes (base actual)",
            TOP_OBJECT_COLS,
            s.probe("los objetos más grandes", TOP_OBJECTS, gaps).await,
        ),
    ];
    snap.tables.extend(tables.into_iter().flatten());
    if hadr {
        replicas(s, snap, gaps).await;
    } else {
        snap.metrics.push(replication_lag(None));
        snap.info.push(("Always On".into(), "No habilitado".into()));
    }
}

/// SQL Server on a machine or Managed Instance: the headline figures, in
/// display order (the replica lag goes last, from [`replication_lag`]).
fn server_metrics(r: &ServerReadings) -> Vec<Metric> {
    let c = &r.counters;
    let pc = |object: &str, counter: &str| format!("{OS_COUNTERS}: {object} '{counter}'");
    let mut m = Vec::new();
    // CPU.
    let (sql_cpu, idle) = (at(&r.ring, 0), at(&r.ring, 1));
    let host = idle
        .filter(|_| !unsampled(&r.ring))
        .map(|i| (100.0 - i).clamp(0.0, 100.0));
    m.push(
        Metric::new("cpu", "CPU del servidor", "CPU", U::Percent, host)
            .max(Some(100.0))
            .help("Uso de CPU de todo el equipo, de SQL Server y de los demás procesos, en la última muestra del monitor de schedulers (toma una por minuto). Cerca de 100 % sostenido, las consultas esperan CPU.")
            .source("sys.dm_os_ring_buffers, RING_BUFFER_SCHEDULER_MONITOR: 100 - SystemHealth/SystemIdle (newest record)"),
    );
    m.push(
        Metric::new("cpu_sql", "CPU de SQL Server", "CPU", U::Percent, sql_cpu)
            .max(Some(100.0))
            .help("Parte de la CPU de todo el equipo que usó el proceso de SQL Server, en la última muestra del monitor de schedulers (una por minuto). Lo que falta para llegar a CPU del servidor lo usan otros procesos.")
            .source("sys.dm_os_ring_buffers, RING_BUFFER_SCHEDULER_MONITOR: SystemHealth/ProcessUtilization (newest record)"),
    );
    // ms of CPU → seconds × 100, spread over its schedulers: the rate is the
    // % of all the CPUs SQL Server may use (100 % is all of them busy), not
    // of one core, which read "500 %" with five busy cores.
    let cpu_ms = at(&r.sched, 0);
    let schedulers = at(&r.sched, 1).filter(|n| *n > 0.0);
    m.push(
        Metric::new(
            "cpu_time",
            "CPU del proceso",
            "CPU",
            U::Percent,
            cpu_ms.zip(schedulers).map(|(ms, n)| ms / 10.0 / n),
        )
        .max(Some(100.0))
        .counter()
        .help("Tiempo de CPU que usaron las tareas de SQL Server, repartido entre sus schedulers visibles (uno por CPU que puede usar): 100 % equivale a todas esas CPU ocupadas con SQL Server. A diferencia de CPU de SQL Server, no espera la muestra por minuto.")
        .source("sys.dm_os_schedulers: SUM(total_cpu_usage_ms) / 10 / COUNT(*) WHERE status = 'VISIBLE ONLINE'"),
    );

    // Memory.
    let target = c
        .get("Memory Manager", "Target Server Memory (KB)")
        .map(|k| k * 1024.0);
    let process = at(&r.mem, 0);
    let total = at(&r.mem, 1);
    let used = c
        .get("Memory Manager", "Total Server Memory (KB)")
        .map(|k| k * 1024.0)
        .or(process);
    let ceiling = match (target, total) {
        (Some(t), Some(p)) => Some(t.min(p)),
        (t, p) => t.or(p),
    };
    m.push(
        Metric::new(
            "mem_used",
            "Memoria de SQL Server",
            "Memoria",
            U::Bytes,
            used,
        )
        .max(ceiling)
        .help("Memoria que tiene comprometida el administrador de memoria de SQL Server. El máximo es Target Server Memory (lo que quiere usar, acotado por max server memory) o la del equipo si es menor; que llegue y se quede ahí es normal, porque SQL Server usa como caché todo lo que puede.")
        .source(&format!(
            "{OS_COUNTERS}: Memory Manager 'Total Server Memory (KB)' × 1024 (else sys.dm_os_process_memory.physical_memory_in_use_kb); max 'Target Server Memory (KB)'"
        )),
    );
    m.push(
        Metric::new(
            "mem_process",
            "Memoria del proceso",
            "Memoria",
            U::Bytes,
            process,
        )
        .max(total)
        .help("Memoria física que ocupa el proceso de SQL Server (working set, páginas grandes y AWE incluidas), con lo que reserva fuera del administrador de memoria. El máximo es la memoria del equipo.")
        .source("sys.dm_os_process_memory: physical_memory_in_use_kb × 1024"),
    );
    m.push(
        Metric::new(
            "mem_cache",
            "Caché de datos (buffer pool)",
            "Memoria",
            U::Bytes,
            c.get("Memory Manager", "Database Cache Memory (KB)")
                .map(|k| k * 1024.0),
        )
        .help("Memoria de SQL Server que guarda páginas de datos e índices (el buffer pool). Si se achica de golpe, hubo presión de memoria.")
        .source(&format!(
            "{} × 1024",
            pc("Memory Manager", "Database Cache Memory (KB)")
        )),
    );
    m.push(
        Metric::new(
            "mem_host",
            "Memoria del equipo en uso",
            "Memoria",
            U::Bytes,
            total.zip(at(&r.mem, 2)).map(|(t, a)| t - a),
        )
        .max(total)
        .help("Memoria física del equipo en uso por todos los procesos, según el sistema operativo. Si queda poca disponible, el sistema le pide a SQL Server que libere memoria.")
        .source("sys.dm_os_sys_memory: total_physical_memory_kb - available_physical_memory_kb"),
    );

    // Connections.
    let conns = c
        .get("General Statistics", "User Connections")
        .or(at(&r.sessions, 0));
    m.push(
        Metric::new("connections", "Conexiones", "Conexiones", U::Count, conns)
            .max(r.max_conn)
            .help("Conexiones de clientes abiertas. El máximo es @@MAX_CONNECTIONS, 32767 salvo que se haya limitado; un número que crece sin parar suele ser una aplicación que no cierra sus conexiones.")
            .source(&format!(
                "{} (else sys.dm_exec_sessions: COUNT(DISTINCT session_id) WHERE is_user_process = 1)",
                pc("General Statistics", "User Connections")
            )),
    );
    m.push(active_sessions(&r.sessions));

    // Activity.
    m.push(batch_requests(c, "Lotes T-SQL que recibe el servidor: cada envío de un cliente, con una o varias sentencias. Es la medida habitual de carga."));
    m.push(transactions(
        c,
        "Transacciones iniciadas en todas las bases, tempdb y las del sistema incluidas.",
    ));
    m.push(
        Metric::new(
            "compilations",
            "Compilaciones",
            "Actividad",
            U::Count,
            c.get("SQL Statistics", "SQL Compilations/sec"),
        )
        .counter()
        .help("Planes de ejecución que SQL Server compiló. Si se acercan a los lotes por segundo, casi nada reutiliza planes: suele ser SQL con literales en vez de parámetros.")
        .source(&total_of(pc("SQL Statistics", "SQL Compilations/sec"))),
    );
    m.push(
        Metric::new(
            "recompilations",
            "Recompilaciones",
            "Actividad",
            U::Count,
            c.get("SQL Statistics", "SQL Re-Compilations/sec"),
        )
        .counter()
        .help("Planes que hubo que volver a compilar por cambios de esquema o de estadísticas, opciones SET o RECOMPILE. Deberían ser una parte chica de las compilaciones.")
        .source(&total_of(pc("SQL Statistics", "SQL Re-Compilations/sec"))),
    );
    m.push(
        Metric::new(
            "errors",
            "Errores",
            "Actividad",
            U::Count,
            c.get("SQL Errors", "Errors/sec"),
        )
        .counter()
        .help("Errores que devolvió SQL Server, de todo tipo: de usuario, conexiones cortadas y también mensajes informativos, así que una tasa baja y constante puede ser normal.")
        .source(&total_of(format!("{}, _Total", pc("SQL Errors", "Errors/sec")))),
    );

    // Cache.
    let hit = match (
        c.get("Buffer Manager", "Buffer cache hit ratio"),
        c.get("Buffer Manager", "Buffer cache hit ratio base"),
    ) {
        (Some(v), Some(b)) if b > 0.0 => Some((v / b * 100.0).min(100.0)),
        _ => None,
    };
    m.push(
        Metric::new("cache_hit", "Aciertos de caché", "Caché", U::Percent, hit)
            .max(Some(100.0))
            .help("Porcentaje de páginas encontradas en el buffer pool sin leer de disco, sobre los últimos miles de accesos. Suele estar cerca de 100 %; para ver presión de memoria sirve más la esperanza de vida de página.")
            .source(&format!(
                "{} / 'Buffer cache hit ratio base' × 100",
                pc("Buffer Manager", "Buffer cache hit ratio")
            )),
    );
    m.push(
        Metric::new(
            "page_life",
            "Esperanza de vida de página",
            "Caché",
            U::Seconds,
            c.get("Buffer Manager", "Page life expectancy"),
        )
        .help("Segundos que una página se queda en el buffer pool sin usarse antes de que la desalojen. Si cae de golpe o se mantiene baja, falta memoria y las consultas van más al disco.")
        .source(&pc("Buffer Manager", "Page life expectancy")),
    );

    // Disk and network.
    m.extend(disk_io(
        &r.io,
        "Bytes leídos de los archivos de datos y de log de todas las bases, tempdb incluida.",
        "Bytes escritos en los archivos de datos y de log de todas las bases, tempdb incluida.",
    ));
    m.push(
        Metric::new(
            "io_stall",
            "Espera de E/S",
            "Disco",
            U::Millis,
            at(&r.io, 4),
        )
        .counter()
        .help("Tiempo que las lecturas y escrituras de archivos esperaron al disco, sumado entre todas: con varias a la vez puede pasar de 1000 ms por segundo. Si sube sin que suba la E/S, el disco responde más lento.")
        .source("sys.dm_io_virtual_file_stats(NULL, NULL): SUM(io_stall)"),
    );
    m.push(
        Metric::new(
            "log_flushes",
            "Vaciados del log",
            "Disco",
            U::Count,
            c.get("Databases", "Log Flushes/sec"),
        )
        .counter()
        .help("Escrituras del log de transacciones a disco, en todas las bases. Cada commit espera que su parte del log llegue al disco: muchas y chicas suelen ser commits fila por fila.")
        .source(&total_of(format!("{}, _Total", pc("Databases", "Log Flushes/sec")))),
    );
    m.push(
        Metric::new(
            "packets_in",
            "Paquetes recibidos",
            "Red",
            U::Count,
            at(&r.packets, 0),
        )
        .counter()
        .help("Paquetes de red (TDS) que SQL Server recibió de los clientes.")
        .source("@@PACK_RECEIVED"),
    );
    m.push(
        Metric::new(
            "packets_out",
            "Paquetes enviados",
            "Red",
            U::Count,
            at(&r.packets, 1),
        )
        .counter()
        .help("Paquetes de red (TDS) que SQL Server envió a los clientes; crece con los resultados grandes.")
        .source("@@PACK_SENT"),
    );

    // Storage.
    m.push(
        Metric::new(
            "storage_used",
            "Espacio de datos y logs",
            "Almacenamiento",
            U::Bytes,
            at(&r.storage, 0),
        )
        .help(
            "Tamaño en disco de los archivos de datos y de log de todas las bases, ocupados o no.",
        )
        .source("sys.master_files: SUM(size) × 8 KB, without tempdb; + tempdb.sys.database_files: SUM(size) × 8 KB (its current size)"),
    );

    // Locks.
    let blocked = c.get("General Statistics", "Processes blocked");
    m.push(
        Metric::new(
            "locks_waiting",
            "Bloqueos en espera",
            "Bloqueos",
            U::Count,
            at(&r.locks, 1).or(blocked),
        )
        .help(LOCKS_WAITING_HELP)
        .source(&format!(
            "sys.dm_tran_locks: COUNT(*) WHERE request_status = 'WAIT' (else {})",
            pc("General Statistics", "Processes blocked")
        )),
    );
    m.push(locks_held(&r.locks, "Pedidos de bloqueo vivos en todas las bases, concedidos o en espera. Miles es normal con mucha concurrencia; preocupa que crezcan sin bajar."));
    m.push(
        Metric::new(
            "blocked_sessions",
            "Procesos bloqueados",
            "Bloqueos",
            U::Count,
            blocked,
        )
        .help("Sesiones que esperan un bloqueo que tiene otra, según el contador de SQL Server. Si el valor se mantiene, mirá la tabla de bloqueos.")
        .source(&pc("General Statistics", "Processes blocked")),
    );
    m.push(deadlocks(c));
    m.push(
        Metric::new(
            "lock_waits",
            "Esperas de bloqueo",
            "Bloqueos",
            U::Count,
            c.get("Locks", "Lock Waits/sec"),
        )
        .counter()
        .help("Pedidos de bloqueo que tuvieron que esperar porque otra sesión tenía el recurso. Una tasa alta indica contención entre transacciones.")
        .source(&total_of(format!("{}, _Total", pc("Locks", "Lock Waits/sec")))),
    );

    m.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            U::Seconds,
            at(&r.sys, 0),
        )
        .help("Tiempo desde que arrancó el servicio de SQL Server.")
        .source("sys.dm_os_sys_info: DATEDIFF(SECOND, sqlserver_start_time, SYSDATETIME())"),
    );
    m
}

/// A performance counter's source: the DMV keeps the running total.
fn total_of(source: String) -> String {
    format!("{source} (running total)")
}

const LOCKS_WAITING_HELP: &str = "Pedidos de bloqueo esperando que otra sesión libere el recurso. Si se mantienen, la tabla de bloqueos muestra quién encabeza la cadena.";

fn active_sessions(sessions: &Nums) -> Metric {
    Metric::new(
        "active_sessions",
        "Sesiones activas",
        "Conexiones",
        U::Count,
        at(sessions, 1),
    )
    .help("Sesiones de usuario con una solicitud en curso (ejecutando, esperando o en cola), sin contar la de DBine.")
    .source("sys.dm_exec_sessions JOIN sys.dm_exec_requests: COUNT(DISTINCT session_id) WHERE is_user_process = 1, without DBine's own")
}

fn batch_requests(c: &Counters, help: &str) -> Metric {
    Metric::new(
        "queries",
        "Lotes (batch requests)",
        "Actividad",
        U::Count,
        c.get("SQL Statistics", "Batch Requests/sec"),
    )
    .counter()
    .help(help)
    .source(&total_of(format!(
        "{OS_COUNTERS}: SQL Statistics 'Batch Requests/sec'"
    )))
}

fn transactions(c: &Counters, help: &str) -> Metric {
    Metric::new(
        "transactions",
        "Transacciones",
        "Actividad",
        U::Count,
        c.get("Databases", "Transactions/sec"),
    )
    .counter()
    .help(help)
    .source(&total_of(format!(
        "{OS_COUNTERS}: Databases 'Transactions/sec', _Total"
    )))
}

fn deadlocks(c: &Counters) -> Metric {
    Metric::new(
        "deadlocks",
        "Deadlocks",
        "Bloqueos",
        U::Count,
        c.get("Locks", "Number of Deadlocks/sec"),
    )
    .counter()
    .help("Deadlocks que SQL Server detectó y resolvió cancelando una de las transacciones (error 1205). Si aparecen seguido, hay transacciones que toman los mismos recursos en distinto orden.")
    .source(&total_of(format!(
        "{OS_COUNTERS}: Locks 'Number of Deadlocks/sec', _Total"
    )))
}

/// Bytes read and written, from the file stats. Each help is a whole
/// sentence (the UI translates it as one message, never from fragments).
fn disk_io(io: &Nums, read_help: &str, write_help: &str) -> [Metric; 2] {
    [
        Metric::new(
            "disk_read",
            "Lectura en disco",
            "Disco",
            U::Bytes,
            at(io, 0),
        )
        .counter()
        .help(read_help)
        .source("sys.dm_io_virtual_file_stats(NULL, NULL): SUM(num_of_bytes_read)"),
        Metric::new(
            "disk_write",
            "Escritura en disco",
            "Disco",
            U::Bytes,
            at(io, 1),
        )
        .counter()
        .help(write_help)
        .source("sys.dm_io_virtual_file_stats(NULL, NULL): SUM(num_of_bytes_written)"),
    ]
}

fn locks_held(locks: &Nums, help: &str) -> Metric {
    Metric::new(
        "locks_held",
        "Bloqueos retenidos",
        "Bloqueos",
        U::Count,
        at(locks, 0),
    )
    .help(help)
    .source("sys.dm_tran_locks: COUNT(*)")
}

/// The Always On replica lag (`None` without Always On).
fn replication_lag(lag: Option<f64>) -> Metric {
    Metric::new(
        "replication_lag",
        "Retraso de réplica",
        "Replicación",
        U::Seconds,
        lag,
    )
    .help("Mayor retraso de una réplica secundaria de Always On respecto de la primaria. Se ve desde la réplica primaria; vacío sin Always On.")
    .source("sys.dm_hadr_database_replica_states: MAX(secondary_lag_seconds)")
}

async fn replicas(s: &mut SqlServerSession, snap: &mut MonitorSnapshot, gaps: &mut Gaps) {
    let Some(rows) = s.probe("las réplicas de Always On", REPLICAS, gaps).await else {
        return;
    };
    let lag = rows
        .iter()
        .filter_map(|r| f(r, 8))
        .fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))));
    if let Some(role) = rows
        .iter()
        .find(|r| f(r, 9) == Some(1.0))
        .and_then(|r| text(r, 2))
    {
        snap.info.push(("Rol (Always On)".into(), role));
    }
    snap.metrics.push(replication_lag(lag));
    let mut t = MonitorTable::new("replication", "Réplicas (Always On)", REPLICA_COLS);
    t.rows = rows
        .into_iter()
        .map(|r| {
            let mut v: Vec<serde_json::Value> = r.into_iter().map(cell).collect();
            v.truncate(REPLICA_COLS.len());
            v
        })
        .collect();
    snap.tables.push(t);
}

/// What [`azure_db`] reads.
#[derive(Default)]
struct AzureReadings {
    /// sys.dm_db_resource_stats, newest row: CPU, data I/O, log write,
    /// memory, workers, sessions (% of the service tier's limit).
    res: Nums,
    counters: Counters,
    /// User sessions, ones running a request.
    sessions: Nums,
    /// File I/O: bytes read, bytes written…
    io: Nums,
    /// sys.database_files: allocated, data used, MaxSizeInBytes (bytes).
    storage: Nums,
    /// Lock requests, waiting ones.
    locks: Nums,
    /// SQL database in Fabric (EngineEdition 12): the same views, but its
    /// limits are the ones Fabric sets, not a service tier.
    fabric: bool,
}

async fn azure_db(
    s: &mut SqlServerSession,
    snap: &mut MonitorSnapshot,
    gaps: &mut Gaps,
    fabric: bool,
) {
    let res = nums(
        s.probe(
            "el uso de recursos (sys.dm_db_resource_stats)",
            AZ_RESOURCES,
            gaps,
        )
        .await,
    );
    let counters = counters(
        s.probe("los contadores de rendimiento", COUNTERS, gaps)
            .await,
    );
    let sessions = nums(s.probe("las sesiones", SESSION_COUNTS, gaps).await);
    let io = nums(s.probe("la E/S de archivos", FILE_IO, gaps).await);
    let st = first(s.probe("el tamaño de la base", AZ_STORAGE, gaps).await);
    let locks = nums(s.probe("los bloqueos", LOCK_COUNTS, gaps).await);
    let r = AzureReadings {
        res,
        counters,
        sessions,
        io,
        storage: st.as_ref().map(row_nums),
        locks,
        fabric,
    };
    snap.metrics.extend(azure_metrics(&r));
    if r.res.is_some() && at(&r.res, 0).is_none() {
        snap.notes.push("sys.dm_db_resource_stats está vacío en master: conectate a una base de usuario para ver CPU, memoria y E/S.".into());
    }
    if let Some(row) = &st {
        if let Some(so) = text(row, 3) {
            snap.info.push(("Objetivo de servicio".into(), so));
        }
        if let Some(e) = text(row, 4) {
            snap.info.push(("Nivel".into(), e));
        }
    }

    let tables = [
        table(
            "sessions",
            "Sesiones",
            SESSION_COLS,
            s.probe("las sesiones", SESSIONS, gaps).await,
        ),
        table(
            "queries",
            "Consultas en curso",
            REQUEST_COLS,
            s.probe("las consultas en curso", REQUESTS, gaps).await,
        ),
        table(
            "locks",
            "Bloqueos / esperas",
            BLOCKING_COLS,
            s.probe("los bloqueos", BLOCKING, gaps).await,
        ),
        table(
            "waits",
            "Esperas principales de la base",
            WAIT_COLS,
            s.probe("las esperas", &waits_sql("sys.dm_db_wait_stats"), gaps)
                .await,
        ),
        table(
            "databases",
            "Bases",
            &[
                "Base",
                "Estado",
                "Compatibilidad",
                "Objetivo de servicio",
                "Nivel",
            ],
            s.probe("las bases", AZ_DATABASES, gaps).await,
        ),
        table(
            "top_objects",
            "Objetos más grandes",
            TOP_OBJECT_COLS,
            s.probe("los objetos más grandes", TOP_OBJECTS, gaps).await,
        ),
    ];
    snap.tables.extend(tables.into_iter().flatten());
    if fabric {
        snap.notes.push("La base de datos SQL de Fabric informa CPU, memoria y E/S como porcentaje del límite que Fabric le fija, promediado cada 15 s.".into());
    } else {
        snap.notes.push("Azure SQL Database informa CPU, memoria y E/S como porcentaje del límite del nivel de servicio, promediado cada 15 s.".into());
        snap.notes.push("Las réplicas de geo-replicación se ven desde la base primaria en sys.dm_geo_replication_link_status (no incluido).".into());
    }
}

/// The figures of `sys.dm_db_resource_stats`, each a share of the
/// database's limit: key, label, group, column index and name, and the help
/// in Azure SQL Database (a service tier) and in Fabric (limits Fabric sets).
const TIER: [(&str, &str, &str, usize, &str, &str, &str); 6] = [
    (
        "cpu",
        "CPU de la base",
        "CPU",
        0,
        "avg_cpu_percent",
        "CPU que usó la base como porcentaje del límite de su nivel de servicio (vCores o DTU), promedio de los últimos 15 s. Cerca de 100 % sostenido, las consultas esperan CPU: optimizalas o subí de nivel.",
        "CPU que usó la base como porcentaje del límite que Fabric le fija, promedio de los últimos 15 s. Cerca de 100 % sostenido, las consultas esperan CPU: optimizalas.",
    ),
    (
        "mem_percent",
        "Memoria usada",
        "Memoria",
        3,
        "avg_memory_usage_percent",
        "Memoria que usa la base como porcentaje del límite de su nivel de servicio, promedio de los últimos 15 s. Incluye la caché de datos, así que un valor alto es normal.",
        "Memoria que usa la base como porcentaje del límite que Fabric le fija, promedio de los últimos 15 s. Incluye la caché de datos, así que un valor alto es normal.",
    ),
    (
        "data_io",
        "E/S de datos (del límite)",
        "Disco",
        1,
        "avg_data_io_percent",
        "Lecturas y escrituras de datos como porcentaje del límite de E/S del nivel de servicio, promedio de los últimos 15 s. En 100 %, las consultas esperan al disco.",
        "Lecturas y escrituras de datos como porcentaje del límite de E/S que Fabric le fija a la base, promedio de los últimos 15 s. En 100 %, las consultas esperan al disco.",
    ),
    (
        "log_write",
        "Escritura del log (del límite)",
        "Disco",
        2,
        "avg_log_write_percent",
        "Escritura del log de transacciones como porcentaje del límite de velocidad de log del nivel de servicio, promedio de los últimos 15 s. En 100 %, las transacciones esperan para escribir (LOG_RATE_GOVERNOR).",
        "Escritura del log de transacciones como porcentaje del límite de velocidad de log que Fabric le fija a la base, promedio de los últimos 15 s. En 100 %, las transacciones esperan para escribir (LOG_RATE_GOVERNOR).",
    ),
    (
        "workers",
        "Workers (del límite)",
        "Conexiones",
        4,
        "max_worker_percent",
        "Máximo de workers (solicitudes simultáneas) de los últimos 15 s, como porcentaje del límite del nivel de servicio. Al llegar a 100 %, las solicitudes nuevas fallan.",
        "Máximo de workers (solicitudes simultáneas) de los últimos 15 s, como porcentaje del límite que Fabric le fija a la base. Al llegar a 100 %, las solicitudes nuevas fallan.",
    ),
    (
        "sessions_pct",
        "Sesiones (del límite)",
        "Conexiones",
        5,
        "max_session_percent",
        "Máximo de sesiones simultáneas de los últimos 15 s, como porcentaje del límite del nivel de servicio. Al llegar a 100 %, se rechazan las conexiones nuevas.",
        "Máximo de sesiones simultáneas de los últimos 15 s, como porcentaje del límite que Fabric le fija a la base. Al llegar a 100 %, se rechazan las conexiones nuevas.",
    ),
];

/// Azure SQL Database (and SQL database in Fabric): the headline figures,
/// in display order.
fn azure_metrics(r: &AzureReadings) -> Vec<Metric> {
    // A share of the database's limit, from the newest 15 s row.
    let mut m: Vec<Metric> = TIER
        .iter()
        .map(|&(key, label, group, i, column, azure, fabric)| {
            Metric::new(key, label, group, U::Percent, at(&r.res, i))
                .max(Some(100.0))
                .help(if r.fabric { fabric } else { azure })
                .source(&format!(
                    "sys.dm_db_resource_stats: {column} (newest 15 s row)"
                ))
        })
        .collect();
    let c = &r.counters;
    // No max: @@MAX_CONNECTIONS (32767) isn't the database's session limit,
    // which Sesiones (del límite) already shows as a share.
    m.push(
        Metric::new(
            "connections",
            "Conexiones",
            "Conexiones",
            U::Count,
            at(&r.sessions, 0),
        )
        .help("Sesiones de usuario abiertas en la base. Cuánto del límite usan se ve en Sesiones (del límite).")
        .source("sys.dm_exec_sessions: COUNT(DISTINCT session_id) WHERE is_user_process = 1"),
    );
    m.push(active_sessions(&r.sessions));
    m.push(batch_requests(c, "Lotes T-SQL que recibe la base: cada envío de un cliente, con una o varias sentencias. Es la medida habitual de carga."));
    m.push(transactions(
        c,
        "Transacciones iniciadas, sumando todas las bases que informa el contador.",
    ));
    m.push(deadlocks(c));
    m.extend(disk_io(
        &r.io,
        "Bytes leídos de los archivos de datos y de log de la base.",
        "Bytes escritos en los archivos de datos y de log de la base.",
    ));

    let used = at(&r.storage, 1);
    let max = at(&r.storage, 2).filter(|m| *m > 0.0);
    m.push(
        Metric::new(
            "storage_used",
            "Espacio usado",
            "Almacenamiento",
            U::Bytes,
            used,
        )
        .max(max)
        .help("Espacio ocupado dentro de los archivos de datos de la base, sin el log. El máximo es el tamaño máximo de la base: al llegar, las inserciones y actualizaciones fallan.")
        .source("sys.database_files: SUM(FILEPROPERTY(name, 'SpaceUsed')) × 8 KB of the data files; max DATABASEPROPERTYEX(DB_NAME(), 'MaxSizeInBytes')"),
    );
    m.push(
        Metric::new(
            "storage_allocated",
            "Espacio asignado",
            "Almacenamiento",
            U::Bytes,
            at(&r.storage, 0),
        )
        .max(max)
        .help("Tamaño de los archivos de la base, datos y log, ocupados o no. El máximo es el de los datos (MaxSizeInBytes).")
        .source("sys.database_files: SUM(size) × 8 KB"),
    );
    m.push(
        Metric::new(
            "locks_waiting",
            "Bloqueos en espera",
            "Bloqueos",
            U::Count,
            at(&r.locks, 1),
        )
        .help(LOCKS_WAITING_HELP)
        .source("sys.dm_tran_locks: COUNT(*) WHERE request_status = 'WAIT'"),
    );
    m.push(locks_held(&r.locks, "Pedidos de bloqueo vivos, concedidos o en espera. Miles es normal con mucha concurrencia; preocupa que crezcan sin bajar."));
    m
}

async fn warehouse(s: &mut SqlServerSession, snap: &mut MonitorSnapshot, gaps: &mut Gaps) {
    let sessions = nums(s.probe("las sesiones", SESSION_COUNTS, gaps).await);
    let history = s
        .probe("el historial de Query Insights", WH_HISTORY, gaps)
        .await;
    let summary = history
        .as_ref()
        .map(|h| (h.len() as f64, h.iter().filter_map(|r| f(r, 6)).sum()));
    snap.metrics.extend(warehouse_metrics(&sessions, summary));
    let tables = [
        table(
            "sessions",
            "Sesiones",
            WH_SESSION_COLS,
            s.probe("las sesiones", WH_SESSIONS, gaps).await,
        ),
        table(
            "queries",
            "Consultas en curso",
            WH_REQUEST_COLS,
            s.probe("las consultas en curso", WH_REQUESTS, gaps).await,
        ),
        table(
            "history",
            "Consultas recientes (última hora)",
            WH_HISTORY_COLS,
            history,
        ),
    ];
    snap.tables.extend(tables.into_iter().flatten());
    snap.notes.push(
        "Fabric no expone CPU, memoria ni disco del almacén por T-SQL: el consumo de capacidad está en la app Microsoft Fabric Capacity Metrics."
            .into(),
    );
    snap.notes
        .push("Query Insights guarda las consultas terminadas con unos minutos de demora.".into());
}

/// Fabric Data Warehouse / Synapse: sessions, and from Query Insights (when
/// it answers) how many queries ended in the last hour and the MB they read
/// from disk.
fn warehouse_metrics(sessions: &Nums, history: Option<(f64, f64)>) -> Vec<Metric> {
    let mut m = vec![
        Metric::new(
            "connections",
            "Conexiones",
            "Conexiones",
            U::Count,
            at(sessions, 0),
        )
        .help("Sesiones de usuario abiertas en el almacén.")
        .source("sys.dm_exec_sessions: COUNT(DISTINCT session_id) WHERE is_user_process = 1"),
        active_sessions(sessions),
    ];
    if let Some((n, scanned_mb)) = history {
        m.push(
            Metric::new(
                "queries_last_hour",
                "Consultas en la última hora (hasta 50)",
                "Actividad",
                U::Count,
                Some(n),
            )
            .help("Consultas que empezaron en la última hora y ya terminaron, según Query Insights, contando hasta 50. Query Insights las registra con unos minutos de demora.")
            .source("queryinsights.exec_requests_history: rows with start_time in the last hour (TOP 50)"),
        );
        m.push(
            Metric::new(
                "scanned_last_hour",
                "Leído de disco en esas consultas",
                "Actividad",
                U::Bytes,
                Some(scanned_mb * 1_048_576.0),
            )
            .help("Datos que leyeron del disco local del almacén las consultas que empezaron en la última hora y ya terminaron (hasta 50), sin contar lo que salió de memoria o del almacenamiento remoto.")
            .source("queryinsights.exec_requests_history: SUM(data_scanned_disk_mb) × 1048576 over the same rows"),
        );
    }
    m
}

/// The sessions in blocking chains: every session waiting on another
/// (`blocking_session_id`) and the ones they wait for, even idle ones holding
/// an open transaction (the usual culprit). The text is what the client
/// sent (the input buffer), not the server's parameterized form.
const BLOCKING_CHAINS: &str = "
WITH w AS (SELECT session_id, blocking_session_id FROM sys.dm_exec_requests WHERE blocking_session_id <> 0),
ids AS (SELECT session_id AS id FROM w UNION SELECT blocking_session_id FROM w)
SELECT CAST(s.session_id AS nvarchar(20)),
       CAST(NULLIF(r.blocking_session_id, 0) AS nvarchar(20)),
       s.login_name,
       CONCAT(s.host_name, CASE WHEN s.program_name IS NULL THEN N'' ELSE N' · ' + s.program_name END),
       DB_NAME(COALESCE(r.database_id, s.database_id)),
       COALESCE(r.wait_type, CASE WHEN r.session_id IS NULL AND s.open_transaction_count > 0 THEN N'inactiva con transacción abierta' END, r.status, s.status),
       CAST(COALESCE(r.wait_time, r.total_elapsed_time, DATEDIFF_BIG(ms, s.last_request_end_time, SYSDATETIME())) AS bigint),
       r.wait_resource,
       CAST(COALESCE(ib.event_info, t.text) AS nvarchar(4000))
FROM ids
JOIN sys.dm_exec_sessions s ON s.session_id = ids.id
LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
LEFT JOIN sys.dm_exec_connections c ON c.session_id = s.session_id
OUTER APPLY sys.dm_exec_sql_text(COALESCE(r.sql_handle, c.most_recent_sql_handle)) t
OUTER APPLY sys.dm_exec_input_buffer(s.session_id, NULL) ib";

pub async fn blocking(s: &mut SqlServerSession) -> Result<Vec<dbine_driver::BlockedSession>> {
    let rows = s.rows(BLOCKING_CHAINS, &[]).await?;
    let text = |r: &Row, i: usize| {
        r.try_get::<&str, _>(i)
            .ok()
            .flatten()
            .map(str::to_string)
            .filter(|v| !v.is_empty())
    };
    Ok(rows
        .iter()
        .map(|r| dbine_driver::BlockedSession {
            id: text(r, 0).unwrap_or_default(),
            blocked_by: text(r, 1),
            user: text(r, 2),
            client: text(r, 3),
            database: text(r, 4),
            wait: text(r, 5),
            waited_ms: r
                .try_get::<i64, _>(6)
                .ok()
                .flatten()
                .map(|v| v.max(0) as u64),
            object: text(r, 7),
            sql: text(r, 8).map(|q| q.trim().to_string()),
        })
        .collect())
}

pub async fn kill(s: &mut SqlServerSession, id: &str) -> Result<()> {
    let id: u32 = id.trim().parse().map_err(|_| {
        dbine_driver::Error::Query(format!("«{id}» no es un id de sesión de SQL Server"))
    })?;
    s.rows(&format!("KILL {id}"), &[]).await.map(|_| ())
}

/// Every metric has a non-empty help and source, and keys don't repeat.
#[cfg(test)]
pub(crate) fn assert_described(metrics: &[Metric]) {
    let mut keys: Vec<&str> = metrics.iter().map(|m| m.key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), metrics.len(), "duplicate keys");
    for m in metrics {
        assert!(
            m.help.as_deref().is_some_and(|h| !h.trim().is_empty()),
            "{} has no help",
            m.key
        );
        // A whole sentence: the UI translates it as one message.
        let help = m.help.as_deref().unwrap();
        assert!(
            help.starts_with(|c: char| c.is_uppercase()) && help.ends_with('.'),
            "{}: help is not a whole sentence: {help}",
            m.key
        );
        assert!(
            m.source.as_deref().is_some_and(|s| !s.trim().is_empty()),
            "{} has no source",
            m.key
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_errors_fold_into_one_note() {
        let mut g = Gaps::default();
        assert!(denied(
            "VIEW SERVER STATE permission was denied on object 'server'"
        ));
        assert!(!denied("Invalid object name 'sys.dm_os_ring_buffers'."));
        g.denied.extend(["el uso de CPU", "la memoria"]);
        g.notes.push("No se pudo leer x: y".into());
        let n = g.into_notes("VIEW SERVER STATE");
        assert_eq!(
            n[0],
            "Sin el permiso VIEW SERVER STATE no se ven: el uso de CPU, la memoria."
        );
        assert_eq!(n.len(), 2);
    }

    #[test]
    fn counters_match_any_instance_prefix() {
        let c = Counters(vec![(
            "MSSQL$SQLEXPRESS:Buffer Manager".into(),
            "Page life expectancy".into(),
            300.0,
        )]);
        assert_eq!(c.get("Buffer Manager", "Page life expectancy"), Some(300.0));
        assert_eq!(c.get("Buffer Manager", "Other"), None);
    }

    /// Every flavor's figures say what they measure and where they come
    /// from, even when the server reported nothing (they are always there).
    #[test]
    fn every_metric_has_help_and_source() {
        let mut server = server_metrics(&ServerReadings::default());
        server.push(replication_lag(None));
        assert_eq!(server.len(), 30);
        assert_described(&server);
        let azure = azure_metrics(&AzureReadings::default());
        assert_eq!(azure.len(), 17);
        assert_described(&azure);
        // @@MAX_CONNECTIONS isn't an Azure database's session limit.
        let conns = azure.iter().find(|m| m.key == "connections").unwrap();
        assert_eq!(conns.max, None);
        // SQL database in Fabric: same figures, but no service tier.
        let fabric = azure_metrics(&AzureReadings {
            fabric: true,
            ..Default::default()
        });
        assert_eq!(fabric.len(), 17);
        assert_described(&fabric);
        for (a, f) in azure.iter().zip(&fabric) {
            assert_eq!(a.key, f.key);
            let help = f.help.as_deref().unwrap();
            assert!(!help.contains("nivel de servicio"), "{}: {help}", f.key);
        }
        let warehouse = warehouse_metrics(&None, Some((3.0, 2.0)));
        assert_eq!(warehouse.len(), 4);
        assert_described(&warehouse);
        assert_eq!(warehouse_metrics(&None, None).len(), 2);
    }

    /// A help built with format! reaches the UI as a pattern plus a
    /// fragment translated on its own ("database-level"): never do it.
    #[test]
    fn help_is_never_formatted() {
        let pattern = concat!(".help(&", "format!");
        for src in [include_str!("monitor.rs"), include_str!("babelfish.rs")] {
            assert!(!src.contains(pattern));
        }
    }

    /// The process CPU is the schedulers' ms over how many there are: its
    /// rate is the share of all the CPUs SQL Server may use (100 = all busy).
    #[test]
    fn process_cpu_is_a_share_of_every_scheduler() {
        let cpu = |sched: Nums| {
            server_metrics(&ServerReadings {
                sched,
                ..Default::default()
            })
            .into_iter()
            .find(|m| m.key == "cpu_time")
            .unwrap()
        };
        // Four schedulers busy for a second: 4000 ms → +100.
        let a = cpu(Some(vec![Some(50_000.0), Some(4.0)]));
        let b = cpu(Some(vec![Some(54_000.0), Some(4.0)]));
        assert_eq!(b.value.unwrap() - a.value.unwrap(), 100.0);
        assert!(a.counter && a.max == Some(100.0) && a.unit == U::Percent);
        assert_eq!(cpu(Some(vec![Some(50_000.0), Some(0.0)])).value, None);
    }

    #[test]
    fn host_cpu_is_unknown_until_the_ring_buffer_samples() {
        let host = |ring: Nums| {
            server_metrics(&ServerReadings {
                ring,
                ..Default::default()
            })[0]
                .value
        };
        assert_eq!(host(Some(vec![Some(0.0), Some(0.0)])), None);
        assert_eq!(host(Some(vec![Some(20.0), Some(70.0)])), Some(30.0));
    }
}

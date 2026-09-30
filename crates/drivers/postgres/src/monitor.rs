//! The monitor dashboard ([`dbine_driver::Session::monitor`]) per variant.
//! Everything is read with cheap catalog and statistics queries (this runs
//! every few seconds): no table scans. Each part is optional: a view the
//! variant doesn't have, or that the user may not read, leaves a note and
//! the rest of the snapshot still comes back.
//!
//! - PostgreSQL and the engines that keep its statistics views (the managed
//!   services, EDB, Fujitsu, KingbaseES, openGauss, TimescaleDB,
//!   YugabyteDB, Greenplum and its forks): `pg_stat_activity`,
//!   `pg_stat_database`, `pg_locks`, `pg_settings`, `pg_database_size`,
//!   the bgwriter / checkpointer, `pg_stat_io`, replication, plus what each
//!   one adds (hypertables, segments, tablet servers, Aurora replicas…).
//! - CockroachDB: `crdb_internal` (node metrics, sessions, queries, nodes).
//! - Redshift: `stv_*` / `stl_*` / `svv_*` (and `sys_*` on Serverless).
//! - Materialize, RisingWave, CrateDB, Yellowbrick, Denodo: their own
//!   system catalogs.

use crate::catalog::cell;
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use serde_json::Value;
use tokio_postgres::SimpleQueryRow;

/// Rows kept per table.
const MAX_ROWS: usize = 200;
/// Characters kept per text cell (query texts).
const MAX_TEXT: usize = 2000;
/// Longest a monitoring query may run before it's cancelled.
const QUERY_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) async fn snapshot(s: &PgSession) -> MonitorSnapshot {
    let mut m = Mon { s, snap: MonitorSnapshot::default() };
    match s.variant {
        Variant::Cockroach => cockroach(&mut m).await,
        Variant::Redshift => redshift(&mut m).await,
        Variant::Denodo => denodo(&mut m).await,
        Variant::Materialize => materialize(&mut m).await,
        Variant::RisingWave => risingwave(&mut m).await,
        Variant::CrateDb => cratedb(&mut m).await,
        Variant::Yellowbrick => yellowbrick(&mut m).await,
        Variant::H2 => h2(&mut m).await,
        _ => postgres(&mut m).await,
    }
    m.snap
}

struct Mon<'a> {
    s: &'a PgSession,
    snap: MonitorSnapshot,
}

impl Mon<'_> {
    /// The rows of a monitoring query. When it fails, `what` (a sentence
    /// without the final period) goes to the notes with the server's reason.
    async fn rows(&mut self, sql: &str, what: &str) -> Option<Vec<SimpleQueryRow>> {
        match self.s.text_within(sql, QUERY_LIMIT).await {
            Ok(r) => Some(r),
            Err(e) => {
                let reason = reason(&e.to_string());
                self.note(if reason.is_empty() { format!("{what}.") } else { format!("{what}: {reason}") });
                None
            }
        }
    }

    /// Like [`Self::rows`], for the parts that are only there sometimes:
    /// a failure is no news.
    async fn quiet(&self, sql: &str) -> Option<Vec<SimpleQueryRow>> {
        match self.s.text_within(sql, QUERY_LIMIT).await {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::debug!("{:?}: monitor query failed: {e}", self.s.variant);
                None
            }
        }
    }

    async fn row(&mut self, sql: &str, what: &str) -> Option<SimpleQueryRow> {
        self.rows(sql, what).await?.into_iter().next()
    }

    fn note(&mut self, n: impl Into<String>) {
        let n = n.into();
        if !self.snap.notes.contains(&n) {
            self.snap.notes.push(n);
        }
    }

    fn info(&mut self, label: &str, value: impl Into<String>) {
        let value = value.into();
        if !value.trim().is_empty() {
            self.snap.info.push((label.to_string(), value));
        }
    }

    fn metric(&mut self, m: Metric) {
        self.snap.metrics.push(m);
    }

    /// A table with the first `cols.len()` cells of each row; the columns in
    /// `bytes` are byte counts shown as sizes.
    fn table(&mut self, key: &str, title: &str, cols: &[&str], rows: &[SimpleQueryRow], bytes: &[usize]) {
        let mut t = MonitorTable::new(key, title, cols);
        t.rows = rows
            .iter()
            .take(MAX_ROWS)
            .map(|r| {
                (0..cols.len())
                    .map(|i| {
                        let v = r.get(i);
                        match v.and_then(num) {
                            Some(b) if bytes.contains(&i) => Value::String(human_bytes(b)),
                            _ => json_cell(v),
                        }
                    })
                    .collect()
            })
            .collect();
        self.snap.tables.push(t);
    }

    /// A table with whatever columns the server returned (engines whose
    /// system views we only know by name).
    fn table_as_is(&mut self, key: &str, title: &str, rows: &[SimpleQueryRow]) {
        let Some(first) = rows.first() else {
            self.snap.tables.push(MonitorTable::new(key, title, &[]));
            return;
        };
        let cols: Vec<String> = first.columns().iter().map(|c| c.name().to_string()).collect();
        let refs: Vec<&str> = cols.iter().map(String::as_str).collect();
        self.table(key, title, &refs, rows, &[]);
    }
}

/// A numeric column by name.
fn f(r: &SimpleQueryRow, name: &str) -> Option<f64> {
    cell(r, name).and_then(|v| num(&v))
}

/// The sum of a numeric column over the rows; `None` when none has it.
fn sum(rows: &[SimpleQueryRow], name: &str) -> Option<f64> {
    rows.iter().filter_map(|r| f(r, name)).fold(None, |acc, v| Some(acc.unwrap_or(0.0) + v))
}

fn max_of(rows: &[SimpleQueryRow], name: &str) -> Option<f64> {
    rows.iter().filter_map(|r| f(r, name)).fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))))
}

/// A text cell as JSON: plain numbers become numbers (they sort as such),
/// long texts are cut.
fn json_cell(v: Option<&str>) -> Value {
    let Some(t) = v else { return Value::Null };
    let numeric = !t.is_empty()
        && t.len() <= 16
        && t.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-')
        && !(t.len() > 1 && t.starts_with('0') && !t.starts_with("0."));
    if numeric {
        if let Ok(i) = t.parse::<i64>() {
            return Value::from(i);
        }
        if let Some(n) = t.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
            return Value::Number(n);
        }
    }
    Value::String(truncate(t))
}

fn truncate(s: &str) -> String {
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// The server's reason, one line, without the severity prefix.
fn reason(e: &str) -> String {
    let line = e.lines().next().unwrap_or("").trim();
    for p in ["ERROR: ", "ERROR:  ", "FATAL: "] {
        if let Some(r) = line.strip_prefix(p) {
            return r.trim().to_string();
        }
    }
    line.to_string()
}

/// `pg_settings` value and unit ("16384", "8kB") in bytes; `None` for
/// settings that aren't sizes.
fn setting_bytes(setting: &str, unit: Option<&str>) -> Option<f64> {
    let v = num(setting)?;
    let factor = match unit? {
        "B" => 1.0,
        "kB" => 1024.0,
        "8kB" => 8192.0,
        "16kB" => 16384.0,
        "32kB" => 32768.0,
        "MB" => 1024.0 * 1024.0,
        "16MB" => 16.0 * 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(v * factor)
}

/// `1536` → `1.5 KB`.
fn human_bytes(b: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b;
    let mut i = 0;
    while v.abs() >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Percentage of `part` in `part + rest`.
fn ratio(part: Option<f64>, rest: Option<f64>) -> Option<f64> {
    let (p, r) = (part?, rest?);
    (p + r > 0.0).then(|| 100.0 * p / (p + r))
}

fn add(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (x, None) => x,
        (None, y) => y,
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL and the engines that keep its statistics views.

async fn postgres(m: &mut Mon<'_>) {
    let v = m.s.variant;
    let ver = m.s.version;

    let mut in_recovery = false;
    if let Some(r) = m
        .row(
            "SELECT version() AS version, current_database() AS db,
                    extract(epoch FROM now() - pg_postmaster_start_time()) AS uptime,
                    pg_postmaster_start_time()::text AS started,
                    CASE WHEN pg_is_in_recovery() THEN 'r' ELSE 'p' END AS role,
                    coalesce(host(inet_server_addr()) || ':' || inet_server_port(), '') AS addr",
            "No se pudo leer el estado del servidor",
        )
        .await
    {
        m.info("Versión", cell(&r, "version").unwrap_or_default());
        if v == Variant::Aurora {
            if let Some(a) = m.quiet("SELECT aurora_version() AS v").await.and_then(|r| r.first().and_then(|r| cell(r, "v"))) {
                m.info("Versión de Aurora", a);
            }
        }
        in_recovery = cell(&r, "role").as_deref() == Some("r");
        m.info("Rol", if in_recovery { "Réplica (en recuperación)" } else { "Primario" });
        m.info("Servidor", cell(&r, "addr").unwrap_or_default());
        m.info("Base actual", cell(&r, "db").unwrap_or_default());
        m.info("Iniciado", cell(&r, "started").unwrap_or_default());
        m.metric(Metric::new("uptime", "Tiempo activo", "Servidor", U::Seconds, f(&r, "uptime")));
    }

    let mut block = 8192.0;
    let mut max_conn = None;
    if let Some(rows) = m
        .rows(
            "SELECT name, setting, unit FROM pg_settings
             WHERE name IN ('max_connections', 'shared_buffers', 'effective_cache_size', 'work_mem',
                            'maintenance_work_mem', 'max_wal_size', 'wal_level', 'TimeZone', 'server_encoding',
                            'block_size', 'max_worker_processes', 'max_parallel_workers')
             ORDER BY name",
            "No se pudieron leer los parámetros (pg_settings)",
        )
        .await
    {
        for r in &rows {
            let (name, setting, unit) = (cell(r, "name").unwrap_or_default(), cell(r, "setting").unwrap_or_default(), cell(r, "unit"));
            match name.as_str() {
                "block_size" => block = num(&setting).unwrap_or(8192.0),
                "max_connections" => max_conn = num(&setting),
                _ => {}
            }
            if name != "block_size" {
                let shown = setting_bytes(&setting, unit.as_deref()).map(human_bytes).unwrap_or(setting);
                let label = if name == "TimeZone" { "Zona horaria".to_string() } else { name };
                m.info(&label, shown);
            }
        }
    }

    let extensions = m
        .quiet("SELECT extname || ' ' || extversion AS ext FROM pg_extension ORDER BY extname")
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| cell(r, "ext"))
        .collect::<Vec<_>>();
    m.info("Extensiones", extensions.join(", "));
    let has_ext = |name: &str| extensions.iter().any(|e| e.split(' ').next() == Some(name));

    // Sessions: client backends only (PostgreSQL 10+ lists the background
    // workers too).
    // Before 10 only client backends show up, except in openGauss, whose
    // background threads have no client port.
    let clients = if ver >= 100000 { "backend_type = 'client backend'" } else { "client_port IS NOT NULL" };
    let waiting = if ver >= 90600 { "wait_event_type = 'Lock'" } else { "waiting" };
    if let Some(r) = m
        .row(
            &format!(
                "SELECT count(*) AS total,
                        sum(CASE WHEN state = 'active' AND pid <> pg_backend_pid() THEN 1 ELSE 0 END) AS active,
                        sum(CASE WHEN state LIKE 'idle in transaction%' THEN 1 ELSE 0 END) AS idle_tx,
                        sum(CASE WHEN {waiting} THEN 1 ELSE 0 END) AS waiting,
                        max(CASE WHEN state = 'active' AND pid <> pg_backend_pid()
                                 THEN extract(epoch FROM now() - query_start) END) AS longest
                 FROM pg_stat_activity WHERE {clients}"
            ),
            "No se pudieron leer las sesiones (pg_stat_activity)",
        )
        .await
    {
        m.metric(Metric::new("connections", "Conexiones", "Conexiones", U::Count, f(&r, "total")).max(max_conn));
        m.metric(Metric::new("active_sessions", "Sesiones activas", "Conexiones", U::Count, f(&r, "active")));
        m.metric(Metric::new("idle_in_transaction", "Inactivas en transacción", "Conexiones", U::Count, f(&r, "idle_tx")));
        m.metric(Metric::new("longest_query", "Consulta más larga", "Actividad", U::Seconds, f(&r, "longest").or(Some(0.0))));
        m.metric(Metric::new("locks_waiting", "Bloqueos en espera", "Bloqueos", U::Count, f(&r, "waiting")));
    }

    // Throughput since the statistics were reset, summed over databases.
    let mut blks_read = None;
    if let Some(r) = m
        .row(
            "SELECT sum(xact_commit) AS commits, sum(xact_rollback) AS rollbacks,
                    sum(blks_read) AS blks_read, sum(blks_hit) AS blks_hit,
                    sum(tup_returned) AS tup_returned,
                    sum(tup_inserted) + sum(tup_updated) + sum(tup_deleted) AS tup_written,
                    sum(deadlocks) AS deadlocks, sum(temp_bytes) AS temp_bytes
             FROM pg_stat_database",
            "No se pudieron leer las estadísticas de las bases (pg_stat_database)",
        )
        .await
    {
        blks_read = f(&r, "blks_read");
        let tx = add(f(&r, "commits"), f(&r, "rollbacks"));
        m.metric(Metric::new("transactions", "Transacciones", "Actividad", U::Count, tx).counter());
        m.metric(Metric::new("rollbacks", "Rollbacks", "Actividad", U::Count, f(&r, "rollbacks")).counter());
        m.metric(Metric::new("rows_read", "Filas leídas", "Actividad", U::Count, f(&r, "tup_returned")).counter());
        m.metric(Metric::new("rows_written", "Filas escritas", "Actividad", U::Count, f(&r, "tup_written")).counter());
        m.metric(Metric::new("cache_hit", "Aciertos de caché", "Caché", U::Percent, ratio(f(&r, "blks_hit"), blks_read)));
        m.metric(Metric::new("deadlocks", "Deadlocks", "Bloqueos", U::Count, f(&r, "deadlocks")).counter());
        m.metric(Metric::new("temp_bytes", "Archivos temporales", "Disco", U::Bytes, f(&r, "temp_bytes")).counter());
    }

    // Disk I/O: pg_stat_io on 16+; before that, blocks read into
    // shared_buffers and blocks the checkpointer / bgwriter / backends wrote.
    let no_checkpoints = v == Variant::Yugabyte || v == Variant::Aurora;
    let mut written = None;
    if ver >= 160000 && !no_checkpoints {
        let sql = if ver >= 180000 {
            "SELECT sum(read_bytes) AS r, sum(write_bytes) AS w FROM pg_stat_io"
        } else {
            "SELECT sum(reads * op_bytes) AS r, sum(writes * op_bytes) AS w FROM pg_stat_io"
        };
        if let Some(r) = m.row(sql, "No se pudo leer la E/S (pg_stat_io)").await {
            m.metric(Metric::new("disk_read", "Lectura de disco", "Disco", U::Bytes, f(&r, "r")).counter());
            m.metric(Metric::new("disk_write", "Escritura en disco", "Disco", U::Bytes, f(&r, "w")).counter());
        }
    } else if !no_checkpoints {
        m.metric(Metric::new("disk_read", "Lectura de disco", "Disco", U::Bytes, blks_read.map(|b| b * block)).counter());
    }
    if !no_checkpoints {
        let sql = if ver >= 170000 {
            "SELECT num_timed + num_requested AS checkpoints, buffers_written AS written FROM pg_stat_checkpointer"
        } else {
            "SELECT checkpoints_timed + checkpoints_req AS checkpoints,
                    buffers_checkpoint + buffers_clean + buffers_backend AS written
             FROM pg_stat_bgwriter"
        };
        if let Some(r) = m.row(sql, "No se pudieron leer los checkpoints").await {
            m.metric(Metric::new("checkpoints", "Checkpoints", "Disco", U::Count, f(&r, "checkpoints")).counter());
            written = f(&r, "written").map(|b| b * block);
        }
        if ver < 160000 {
            m.metric(Metric::new("disk_write", "Escritura en disco", "Disco", U::Bytes, written).counter());
        }
        if !in_recovery {
            let wal = if ver >= 100000 {
                "SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), '0/0') AS wal"
            } else {
                "SELECT pg_xlog_location_diff(pg_current_xlog_location(), '0/0') AS wal"
            };
            if let Some(r) = m.row(wal, "No se pudo leer la posición del WAL").await {
                m.metric(Metric::new("wal_bytes", "WAL generado", "Disco", U::Bytes, f(&r, "wal")).counter());
            }
        }
    }

    // Statement counts: pg_stat_statements, or openGauss's own counters.
    if v == Variant::OpenGauss {
        opengauss(m).await;
    } else if has_ext("pg_stat_statements") {
        let (total, mean) = if ver >= 130000 { ("total_exec_time", "mean_exec_time") } else { ("total_time", "mean_time") };
        if let Some(r) = m.row("SELECT sum(calls) AS calls FROM pg_stat_statements", "No se pudo leer pg_stat_statements").await {
            m.metric(Metric::new("queries", "Consultas", "Actividad", U::Count, f(&r, "calls")).counter());
        }
        if let Some(rows) = m
            .quiet(&format!(
                "SELECT left(query, {MAX_TEXT}) AS query, calls, round({total}::numeric, 1) AS total_ms,
                        round({mean}::numeric, 2) AS mean_ms, rows,
                        round(100.0 * shared_blks_hit / nullif(shared_blks_hit + shared_blks_read, 0), 1) AS hit
                 FROM pg_stat_statements ORDER BY {total} DESC LIMIT 20"
            ))
            .await
        {
            m.table(
                "top_statements",
                "Consultas más costosas (pg_stat_statements)",
                &["Consulta", "Llamadas", "Tiempo total (ms)", "Promedio (ms)", "Filas", "Aciertos de caché (%)"],
                &rows,
                &[],
            );
        }
    } else {
        m.note(
            "Para ver consultas por segundo y las más costosas, instalá pg_stat_statements en esta base \
             (shared_preload_libraries y CREATE EXTENSION pg_stat_statements).",
        );
    }

    replication(m, in_recovery).await;

    // Tables.
    let wait_col = if ver >= 90600 {
        "coalesce(wait_event_type || ': ' || wait_event, '')"
    } else {
        "CASE WHEN waiting THEN 'Lock' ELSE '' END"
    };
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT pid, usename, datname,
                    coalesce(host(client_addr), CASE WHEN client_port = -1 THEN 'socket local' ELSE '' END) AS client,
                    application_name, state, {wait_col} AS wait,
                    round(extract(epoch FROM now() - backend_start)::numeric) AS connected,
                    CASE WHEN state = 'active' THEN round(extract(epoch FROM now() - query_start)::numeric, 1) END AS running,
                    left(query, {MAX_TEXT}) AS query
             FROM pg_stat_activity WHERE {clients} AND pid <> pg_backend_pid()
             ORDER BY (state = 'active') DESC, backend_start LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.table(
            "sessions",
            "Sesiones",
            &["PID", "Usuario", "Base", "Cliente", "Aplicación", "Estado", "Espera", "Conectada (s)", "En curso (s)", "Consulta"],
            &rows,
            &[],
        );
    }
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT pid, usename, datname, round(extract(epoch FROM now() - query_start)::numeric, 1) AS secs,
                    {wait_col} AS wait, left(query, {MAX_TEXT}) AS query
             FROM pg_stat_activity
             WHERE state = 'active' AND pid <> pg_backend_pid() AND {clients}
             ORDER BY query_start LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.table("queries", "Consultas en curso", &["PID", "Usuario", "Base", "Duración (s)", "Espera", "Consulta"], &rows, &[]);
    }
    let blocked_by = if ver >= 90600 { "array_to_string(pg_blocking_pids(a.pid), ', ')" } else { "''" };
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT a.pid, a.usename, a.datname, l.locktype, l.mode,
                    coalesce(l.relation::regclass::text, '') AS object, {blocked_by} AS blocked_by,
                    round(extract(epoch FROM now() - a.query_start)::numeric, 1) AS secs, left(a.query, {MAX_TEXT}) AS query
             FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
             WHERE NOT l.granted ORDER BY a.query_start LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.table(
            "locks",
            "Bloqueos en espera",
            &["PID", "Usuario", "Base", "Tipo", "Modo", "Objeto", "Bloqueado por", "Espera (s)", "Consulta"],
            &rows,
            &[],
        );
    }
    if ver >= 90600 {
        if let Some(rows) = m
            .quiet(
                "SELECT wait_event_type, wait_event, count(*) AS n FROM pg_stat_activity
                 WHERE state = 'active' AND wait_event IS NOT NULL AND pid <> pg_backend_pid()
                 GROUP BY 1, 2 ORDER BY 3 DESC LIMIT 50",
            )
            .await
        {
            m.table("waits", "Esperas principales", &["Tipo", "Evento", "Sesiones"], &rows, &[]);
        }
    }

    // Databases and their sizes (YugabyteDB doesn't size them).
    let size = if v == Variant::Yugabyte {
        "NULL::bigint"
    } else {
        "CASE WHEN has_database_privilege(d.oid, 'CONNECT') THEN pg_database_size(d.oid) END"
    };
    if let Some(rows) = m
        .rows(
            &format!(
                "SELECT datname, pg_size_pretty(bytes) AS size, numbackends, xact_commit, xact_rollback, hit, deadlocks,
                        pg_size_pretty(temp_bytes) AS temp, bytes
                 FROM (SELECT d.datname, {size} AS bytes, s.numbackends, s.xact_commit, s.xact_rollback,
                              round(100.0 * s.blks_hit / nullif(s.blks_hit + s.blks_read, 0), 2) AS hit,
                              s.deadlocks, s.temp_bytes
                       FROM pg_database d LEFT JOIN pg_stat_database s ON s.datid = d.oid
                       WHERE NOT d.datistemplate) x
                 ORDER BY bytes DESC NULLS LAST, datname LIMIT {MAX_ROWS}"
            ),
            "No se pudieron leer las bases y sus tamaños",
        )
        .await
    {
        if v != Variant::Yugabyte {
            m.metric(Metric::new("storage_used", "Espacio usado (todas las bases)", "Almacenamiento", U::Bytes, sum(&rows, "bytes")));
        }
        m.table(
            "databases",
            "Bases y tamaños",
            &["Base", "Tamaño", "Conexiones", "Commits", "Rollbacks", "Aciertos de caché (%)", "Deadlocks", "Temporales"],
            &rows,
            &[],
        );
    }

    if v != Variant::Yugabyte {
        // The biggest relations by the pages the planner knows of: sizing
        // them for real (pg_total_relation_size) would wait on any lock
        // held on them.
        let user = crate::catalog::user_schema(v, "n.nspname");
        if let Some(rows) = m
            .quiet(&format!(
                "SELECT x.nspname, x.relname, x.kind, pg_size_pretty(x.total * {block}::bigint) AS total,
                        pg_size_pretty(x.relpages::bigint * {block}::bigint) AS heap, x.rows, s.n_dead_tup,
                        coalesce(greatest(s.last_vacuum, s.last_autovacuum)::text, '') AS vacuum
                 FROM (SELECT c.oid, n.nspname, c.relname, c.relpages, c.reltuples::bigint AS rows,
                              CASE c.relkind WHEN 'm' THEN 'vista materializada' ELSE 'tabla' END AS kind,
                              c.relpages::bigint + coalesce(t.relpages, 0)
                                + coalesce((SELECT sum(i.relpages) FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid
                                            WHERE x.indrelid = c.oid), 0) AS total
                       FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                       LEFT JOIN pg_class t ON t.oid = c.reltoastrelid
                       WHERE c.relkind IN ('r', 'm') AND {user}) x
                 LEFT JOIN pg_stat_all_tables s ON s.relid = x.oid
                 ORDER BY x.total DESC, x.relname LIMIT 20",
                block = block as i64
            ))
            .await
        {
            m.table(
                "top_objects",
                "Objetos más grandes (esta base, según el último VACUUM / ANALYZE)",
                &["Esquema", "Objeto", "Tipo", "Tamaño total", "Sin índices", "Filas (estim.)", "Tuplas muertas", "Último vacuum"],
                &rows,
                &[],
            );
        }
    }

    if let Some(rows) = m
        .quiet(&format!(
            "SELECT name, setting, coalesce(unit, '') AS unit, source FROM pg_settings
             WHERE source NOT IN ('default', 'override', 'client', 'session') ORDER BY name LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.table("settings", "Parámetros modificados", &["Parámetro", "Valor", "Unidad", "Origen"], &rows, &[]);
    }

    // What each variant adds.
    if has_ext("timescaledb") {
        timescale(m).await;
    } else if v == Variant::Timescale {
        m.note("La extensión timescaledb no está instalada en esta base: no hay hypertables que mostrar.");
    }
    if v.mpp() {
        segments(m).await;
    }
    if v == Variant::Yugabyte {
        if let Some(rows) = m
            .rows(
                "SELECT host, port, num_connections, node_type, cloud, region, zone, public_ip FROM yb_servers() ORDER BY host",
                "No se pudieron leer los nodos (yb_servers)",
            )
            .await
        {
            m.metric(Metric::new("nodes", "Nodos", "Cluster", U::Count, Some(rows.len() as f64)));
            m.table("nodes", "Nodos del clúster", &["Host", "Puerto", "Conexiones", "Tipo", "Nube", "Región", "Zona", "IP pública"], &rows, &[]);
        }
        // CPU and memory of each tablet server (2.21+).
        if let Some(rows) = m
            .rows(
                "SELECT ((metrics->>'cpu_usage_user')::float8 + (metrics->>'cpu_usage_system')::float8) * 100 AS cpu,
                        (metrics->>'tserver_root_memory_consumption')::float8 AS ts_mem,
                        (metrics->>'tserver_root_memory_limit')::float8 AS ts_limit,
                        (metrics->>'memory_total')::float8 - (metrics->>'memory_available')::float8 AS host_used,
                        (metrics->>'memory_total')::float8 AS host_total
                 FROM yb_servers_metrics() WHERE status = 'OK'",
                "No se pudo leer el CPU ni la memoria de los tservers (yb_servers_metrics)",
            )
            .await
        {
            let n = rows.len().max(1) as f64;
            m.metric(Metric::new("cpu", "CPU (promedio de tservers)", "CPU", U::Percent, sum(&rows, "cpu").map(|c| c / n)));
            m.metric(Metric::new("mem_used", "Memoria de los tservers", "Memoria", U::Bytes, sum(&rows, "ts_mem")).max(sum(&rows, "ts_limit")));
            m.metric(Metric::new("host_mem_used", "Memoria de los hosts", "Memoria", U::Bytes, sum(&rows, "host_used")).max(sum(&rows, "host_total")));
        }
        m.note("YugabyteDB guarda los datos en DocDB: el tamaño de las bases, la E/S y los checkpoints se ven en la interfaz de yb-master / yb-tserver.");
    }

    match v {
        Variant::OpenGauss | Variant::Yugabyte => {}
        Variant::Aurora => m.note(
            "Aurora no expone por SQL el uso de CPU ni de memoria de la instancia, ni la E/S de su almacenamiento: están en CloudWatch y Performance Insights.",
        ),
        Variant::CloudSql | Variant::AlloyDb => m.note(format!(
            "{} no expone por SQL el uso de CPU ni de memoria de la instancia: están en Cloud Monitoring.",
            v.info().name
        )),
        _ if v.mpp() => m.note(format!(
            "{} no expone por SQL el uso de CPU ni de memoria de los hosts (con grupos de recursos: gp_toolkit.gp_resgroup_status_per_host).",
            v.info().name
        )),
        _ => m.note(
            "PostgreSQL no expone por SQL el uso de CPU ni de memoria del servidor: se ven en el sistema operativo o con la extensión pg_proctab.",
        ),
    }
}

/// Replicas: the lag on a standby, or the standbys a primary feeds.
async fn replication(m: &mut Mon<'_>, in_recovery: bool) {
    let v = m.s.variant;
    let ver = m.s.version;
    if v == Variant::Yugabyte || v.mpp() {
        return;
    }
    if v == Variant::Aurora {
        if let Some(rows) = m
            .rows(
                "SELECT server_id, CASE WHEN session_id = 'MASTER_SESSION_ID' THEN 'escritor' ELSE 'lector' END AS role,
                        replica_lag_in_msec, last_update_timestamp::text AS updated
                 FROM aurora_replica_status() ORDER BY 2, 1",
                "No se pudieron leer las instancias del clúster (aurora_replica_status)",
            )
            .await
        {
            let lag = max_of(&rows, "replica_lag_in_msec").map(|ms| ms / 1000.0);
            m.metric(Metric::new("replication_lag", "Retraso de réplica", "Replicación", U::Seconds, lag));
            m.table("replication", "Instancias del clúster", &["Instancia", "Rol", "Retraso (ms)", "Actualizado"], &rows, &[]);
        }
        return;
    }
    if in_recovery {
        if let Some(r) = m
            .row(
                "SELECT extract(epoch FROM now() - pg_last_xact_replay_timestamp()) AS lag",
                "No se pudo leer el retraso de la réplica",
            )
            .await
        {
            m.metric(Metric::new("replication_lag", "Retraso de réplica", "Replicación", U::Seconds, f(&r, "lag")));
        }
        if ver >= 90600 {
            if let Some(rows) = m
                .quiet("SELECT status, coalesce(latest_end_time::text, '') AS latest, conninfo FROM pg_stat_wal_receiver")
                .await
            {
                m.table("replication", "Origen de la replicación", &["Estado", "Último WAL recibido", "Conexión"], &rows, &[]);
            }
        }
        return;
    }
    let sql = if v == Variant::OpenGauss {
        "SELECT application_name, coalesce(host(client_addr), '') AS client, state, sync_state, NULL AS lag_s,
                pg_xlog_location_diff(sender_sent_location, receiver_replay_location) AS lag_bytes
         FROM pg_stat_replication ORDER BY 1"
            .to_string()
    } else if ver >= 100000 {
        "SELECT application_name, coalesce(host(client_addr), '') AS client, state, sync_state,
                extract(epoch FROM replay_lag) AS lag_s, pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn) AS lag_bytes
         FROM pg_stat_replication ORDER BY 1"
            .to_string()
    } else {
        "SELECT application_name, coalesce(host(client_addr), '') AS client, state, sync_state, NULL AS lag_s,
                pg_xlog_location_diff(pg_current_xlog_location(), replay_location) AS lag_bytes
         FROM pg_stat_replication ORDER BY 1"
            .to_string()
    };
    if let Some(rows) = m.rows(&sql, "No se pudieron leer las réplicas (pg_stat_replication)").await {
        m.info("Réplicas conectadas", rows.len().to_string());
        if !rows.is_empty() {
            m.metric(Metric::new("replication_lag", "Retraso de réplica", "Replicación", U::Seconds, max_of(&rows, "lag_s")));
            m.table(
                "replication",
                "Réplicas",
                &["Aplicación", "Cliente", "Estado", "Sincronía", "Retraso (s)", "Retraso"],
                &rows,
                &[5],
            );
        }
    }
    if ver >= 100000 && v != Variant::OpenGauss {
        if let Some(rows) = m
            .quiet(
                "SELECT slot_name, slot_type, CASE WHEN active THEN 'sí' ELSE 'no' END AS active,
                        pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn) AS retained
                 FROM pg_replication_slots ORDER BY 1",
            )
            .await
            .filter(|r| !r.is_empty())
        {
            m.table("slots", "Slots de replicación", &["Slot", "Tipo", "Activo", "WAL retenido"], &rows, &[3]);
        }
    }
}

async fn timescale(m: &mut Mon<'_>) {
    if let Some(rows) = m
        .rows(
            "SELECT h.hypertable_schema, h.hypertable_name, h.num_chunks,
                    CASE WHEN h.compression_enabled THEN 'sí' ELSE 'no' END AS compression,
                    (SELECT sum(c.relpages)::bigint * current_setting('block_size')::bigint
                     FROM timescaledb_information.chunks ch
                     JOIN pg_class c ON c.oid = format('%I.%I', ch.chunk_schema, ch.chunk_name)::regclass
                     WHERE ch.hypertable_schema = h.hypertable_schema AND ch.hypertable_name = h.hypertable_name) AS bytes
             FROM timescaledb_information.hypertables h ORDER BY h.num_chunks DESC LIMIT 50",
            "No se pudieron leer las hypertables",
        )
        .await
    {
        m.metric(Metric::new("hypertables", "Hypertables", "TimescaleDB", U::Count, Some(rows.len() as f64)));
        m.metric(Metric::new("chunks", "Chunks", "TimescaleDB", U::Count, sum(&rows, "num_chunks").or(Some(0.0))));
        m.table("hypertables", "Hypertables", &["Esquema", "Hypertable", "Chunks", "Compresión", "Tamaño (estim., sin índices)"], &rows, &[4]);
    }
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT j.job_id, j.application_name,
                    coalesce(j.hypertable_schema || '.' || j.hypertable_name, '') AS hypertable,
                    j.schedule_interval::text AS every, coalesce(s.last_run_status, '') AS last_status,
                    coalesce(s.last_run_started_at::text, '') AS last_run, coalesce(s.next_start::text, '') AS next_start,
                    coalesce(s.total_failures, 0) AS failures
             FROM timescaledb_information.jobs j
             LEFT JOIN timescaledb_information.job_stats s ON s.job_id = j.job_id
             ORDER BY j.job_id LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.table(
            "jobs",
            "Jobs en segundo plano",
            &["Job", "Aplicación", "Hypertable", "Cada", "Último estado", "Última ejecución", "Próxima", "Fallas"],
            &rows,
            &[],
        );
    }
}

/// Greenplum and its forks: the coordinator and its segments.
async fn segments(m: &mut Mon<'_>) {
    if let Some(rows) = m
        .rows(
            &format!(
                "SELECT content, dbid, CASE role WHEN 'p' THEN 'primario' ELSE 'espejo' END AS role,
                        CASE WHEN role = preferred_role THEN 'sí' ELSE 'no' END AS preferred,
                        CASE mode WHEN 's' THEN 'sincronizado' WHEN 'n' THEN 'sin sincronizar' ELSE mode::text END AS mode,
                        CASE status WHEN 'u' THEN 'activo' ELSE 'caído' END AS status, hostname, port,
                        CASE WHEN status = 'u' THEN 0 ELSE 1 END AS down
                 FROM gp_segment_configuration ORDER BY content, role DESC LIMIT {MAX_ROWS}"
            ),
            "No se pudieron leer los segmentos (gp_segment_configuration)",
        )
        .await
    {
        let segs = rows.iter().filter(|r| f(r, "content").is_some_and(|c| c >= 0.0)).count();
        m.metric(Metric::new("segments", "Segmentos", "Cluster", U::Count, Some(segs as f64)));
        m.metric(Metric::new("segments_down", "Segmentos caídos", "Cluster", U::Count, sum(&rows, "down")));
        m.table(
            "nodes",
            "Coordinador y segmentos",
            &["Contenido", "dbid", "Rol", "Rol preferido", "Modo", "Estado", "Host", "Puerto"],
            &rows,
            &[],
        );
    }
}

/// openGauss: host CPU and memory from `dbe_perf`, statement counts.
async fn opengauss(m: &mut Mon<'_>) {
    const NO_PERF: &str = "Sin el rol monadmin o sysadmin no se ven el CPU ni la memoria (dbe_perf)";
    if let Some(rows) = m
        .rows("SELECT name, value FROM dbe_perf.os_runtime WHERE name IN ('NUM_CPUS', 'BUSY_TIME', 'LOAD', 'PHYSICAL_MEMORY_BYTES')", NO_PERF)
        .await
    {
        let get = |n: &str| rows.iter().find(|r| cell(r, "name").as_deref() == Some(n)).and_then(|r| f(r, "value"));
        // Hundredths of a second busy, over every CPU: the rate is the
        // percentage of one core.
        m.metric(Metric::new("cpu_time", "CPU del host (% de un núcleo)", "CPU", U::Percent, get("BUSY_TIME")).counter());
        m.metric(Metric::new("load", "Carga (1 min)", "CPU", U::Count, get("LOAD")));
        if let Some(n) = get("NUM_CPUS") {
            m.info("CPUs", format!("{n}"));
        }
        if let Some(b) = get("PHYSICAL_MEMORY_BYTES") {
            m.info("Memoria física", human_bytes(b));
        }
    }
    if let Some(rows) = m
        .rows(
            "SELECT memorytype, memorymbytes FROM dbe_perf.memory_node_detail
             WHERE memorytype IN ('max_process_memory', 'process_used_memory', 'max_shared_memory', 'shared_used_memory')",
            NO_PERF,
        )
        .await
    {
        let mb = |n: &str| {
            rows.iter().find(|r| cell(r, "memorytype").as_deref() == Some(n)).and_then(|r| f(r, "memorymbytes")).map(|v| v * 1048576.0)
        };
        m.metric(Metric::new("mem_used", "Memoria usada", "Memoria", U::Bytes, mb("process_used_memory")).max(mb("max_process_memory")));
        m.metric(Metric::new("mem_cache", "Memoria compartida", "Memoria", U::Bytes, mb("shared_used_memory")).max(mb("max_shared_memory")));
    }
    if let Some(r) = m
        .row(
            "SELECT sum(select_count + update_count + insert_count + delete_count) AS q FROM gs_sql_count",
            "No se pudo leer la cantidad de sentencias (gs_sql_count)",
        )
        .await
    {
        m.metric(Metric::new("queries", "Consultas", "Actividad", U::Count, f(&r, "q")).counter());
    }
}

// ---------------------------------------------------------------------------
// CockroachDB

/// `crdb_internal.node_metrics` names: of the node this session is on.
const CRDB_METRICS: &[&str] = &[
    "sys.cpu.combined.percent-normalized",
    "sys.cpu.user.ns",
    "sys.cpu.sys.ns",
    "sys.rss",
    "sql.conns",
    "sql.statements.active",
    "sql.query.count",
    "sql.txn.commit.count",
    "sql.txn.rollback.count",
    "sys.host.net.recv.bytes",
    "sys.host.net.send.bytes",
    "sys.host.disk.read.bytes",
    "sys.host.disk.write.bytes",
    "capacity",
    "capacity.used",
    "sys.uptime",
    "txnwaitqueue.pusher.waiting",
    "rocksdb.block.cache.hits",
    "rocksdb.block.cache.misses",
    "ranges",
    "ranges.unavailable",
    "ranges.underreplicated",
    "liveness.livenodes",
];

async fn cockroach(m: &mut Mon<'_>) {
    // Since v25, crdb_internal is closed unless the session opts in. The
    // monitor has a session of its own, so the setting stays there; older
    // versions don't know it.
    let _ = m.quiet("SET allow_unsafe_internals = true").await;
    if let Some(r) = m
        .row(
            "SELECT version() AS version, crdb_internal.node_id()::STRING AS node,
                    crdb_internal.cluster_id()::STRING AS cluster, current_setting('TimeZone') AS tz",
            "No se pudo leer la versión",
        )
        .await
    {
        m.info("Versión", cell(&r, "version").unwrap_or_default());
        m.info("Nodo de esta sesión", cell(&r, "node").unwrap_or_default());
        m.info("ID del clúster", cell(&r, "cluster").unwrap_or_default());
        m.info("Zona horaria", cell(&r, "tz").unwrap_or_default());
    }
    let list = CRDB_METRICS.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ");
    if let Some(rows) = m
        .rows(
            &format!("SELECT name, value FROM crdb_internal.node_metrics WHERE name IN ({list})"),
            "Sin el privilegio VIEWCLUSTERMETADATA (o admin) no se ven las métricas del nodo (crdb_internal.node_metrics)",
        )
        .await
    {
        let g = |n: &str| rows.iter().find(|r| cell(r, "name").as_deref() == Some(n)).and_then(|r| f(r, "value"));
        m.metric(Metric::new("cpu", "CPU del servidor", "CPU", U::Percent, g("sys.cpu.combined.percent-normalized").map(|v| v * 100.0)));
        // Nanoseconds of CPU → seconds × 100 (rate = % of one core).
        let cpu_ns = add(g("sys.cpu.user.ns"), g("sys.cpu.sys.ns"));
        m.metric(Metric::new("cpu_time", "CPU del proceso", "CPU", U::Percent, cpu_ns.map(|ns| ns / 1e7)).counter());
        m.metric(Metric::new("mem_used", "Memoria (RSS)", "Memoria", U::Bytes, g("sys.rss")));
        m.metric(Metric::new("connections", "Conexiones", "Conexiones", U::Count, g("sql.conns")));
        m.metric(Metric::new("active_sessions", "Sentencias en curso", "Conexiones", U::Count, g("sql.statements.active")));
        m.metric(Metric::new("queries", "Consultas", "Actividad", U::Count, g("sql.query.count")).counter());
        let tx = add(g("sql.txn.commit.count"), g("sql.txn.rollback.count"));
        m.metric(Metric::new("transactions", "Transacciones", "Actividad", U::Count, tx).counter());
        m.metric(Metric::new("net_in", "Red entrante", "Red", U::Bytes, g("sys.host.net.recv.bytes")).counter());
        m.metric(Metric::new("net_out", "Red saliente", "Red", U::Bytes, g("sys.host.net.send.bytes")).counter());
        m.metric(Metric::new("disk_read", "Lectura de disco", "Disco", U::Bytes, g("sys.host.disk.read.bytes")).counter());
        m.metric(Metric::new("disk_write", "Escritura en disco", "Disco", U::Bytes, g("sys.host.disk.write.bytes")).counter());
        m.metric(Metric::new("cache_hit", "Aciertos de caché de bloques", "Caché", U::Percent, ratio(g("rocksdb.block.cache.hits"), g("rocksdb.block.cache.misses"))));
        m.metric(Metric::new("storage_used", "Espacio usado (este nodo)", "Almacenamiento", U::Bytes, g("capacity.used")).max(g("capacity")));
        m.metric(Metric::new("locks_waiting", "Transacciones en espera", "Bloqueos", U::Count, g("txnwaitqueue.pusher.waiting")));
        m.metric(Metric::new("ranges", "Rangos", "Rangos", U::Count, g("ranges")));
        m.metric(Metric::new("ranges_unavailable", "Rangos no disponibles", "Rangos", U::Count, g("ranges.unavailable")));
        m.metric(Metric::new("ranges_underreplicated", "Rangos sub-replicados", "Rangos", U::Count, g("ranges.underreplicated")));
        m.metric(Metric::new("live_nodes", "Nodos vivos", "Cluster", U::Count, g("liveness.livenodes")));
        m.metric(Metric::new("uptime", "Tiempo activo", "Servidor", U::Seconds, g("sys.uptime")));
        m.note("Las métricas de CPU, memoria, red y disco son del nodo al que está conectada esta sesión.");
    }
    // If crdb_internal is closed to this user, SHOW CLUSTER SESSIONS /
    // STATEMENTS are the supported equivalents (same columns, same
    // VIEWACTIVITY rules); admins stay on crdb_internal.
    let sessions_sql = format!(
        "SELECT node_id, session_id, user_name, client_address, application_name,
                CASE WHEN active_queries <> '' THEN 'activa' ELSE 'inactiva' END AS state,
                extract(epoch FROM now() - session_start)::INT8 AS connected,
                substring(active_queries, 1, {MAX_TEXT}) AS query
         FROM crdb_internal.cluster_sessions ORDER BY session_start LIMIT {MAX_ROWS}"
    );
    let sessions = match m.quiet(&sessions_sql).await {
        Some(rows) => Some(rows),
        None => {
            m.rows(
                &sessions_sql.replace("crdb_internal.cluster_sessions", "[SHOW CLUSTER SESSIONS]"),
                "No se pudieron leer las sesiones (hace falta el privilegio VIEWACTIVITY o admin para ver las de otros usuarios)",
            )
            .await
        }
    };
    if let Some(rows) = sessions {
        m.table("sessions", "Sesiones", &["Nodo", "Sesión", "Usuario", "Cliente", "Aplicación", "Estado", "Conectada (s)", "Consulta"], &rows, &[]);
    }
    let queries_sql = format!(
        "SELECT query_id, node_id, user_name, application_name,
                round(extract(epoch FROM now() - start)::DECIMAL, 1) AS secs, phase,
                substring(query, 1, {MAX_TEXT}) AS query
         FROM crdb_internal.cluster_queries
         WHERE session_id <> (SELECT session_id FROM [SHOW session_id])
         ORDER BY start LIMIT {MAX_ROWS}"
    );
    let queries = match m.quiet(&queries_sql).await {
        Some(rows) => Some(rows),
        None => m.quiet(&queries_sql.replace("crdb_internal.cluster_queries", "[SHOW CLUSTER STATEMENTS]")).await,
    };
    if let Some(rows) = queries {
        m.table("queries", "Consultas en curso", &["Consulta", "Nodo", "Usuario", "Aplicación", "Duración (s)", "Fase", "Texto"], &rows, &[]);
    }
    if let Some(rows) = m
        .quiet(
            "SELECT node_id, address, locality, server_version, CASE WHEN is_live THEN 'sí' ELSE 'no' END AS live,
                    ranges, leases, started_at::STRING AS started
             FROM crdb_internal.gossip_nodes ORDER BY node_id",
        )
        .await
    {
        m.metric(Metric::new("nodes", "Nodos", "Cluster", U::Count, Some(rows.len() as f64)));
        m.table("nodes", "Nodos del clúster", &["Nodo", "Dirección", "Localidad", "Versión", "Vivo", "Rangos", "Leases", "Iniciado"], &rows, &[]);
    }
    if let Some(rows) = m
        .quiet(
            "SELECT d.name, d.owner, count(t.table_id) AS tables
             FROM crdb_internal.databases d
             LEFT JOIN crdb_internal.tables t ON t.database_name = d.name AND t.drop_time IS NULL
             GROUP BY d.name, d.owner ORDER BY d.name",
        )
        .await
    {
        m.table("databases", "Bases", &["Base", "Dueño", "Tablas"], &rows, &[]);
    }
    m.note(
        "CockroachDB solo calcula el tamaño de bases y tablas recorriendo sus rangos, y los bloqueos con crdb_internal.cluster_locks \
         en todo el clúster: no se consultan en cada refresco (ver la DB Console).",
    );
}

// ---------------------------------------------------------------------------
// Redshift (provisioned: stv / stl / svv; Serverless: sys_*).

async fn redshift(m: &mut Mon<'_>) {
    if let Some(r) = m.row("SELECT version() AS version, current_database() AS db", "No se pudo leer la versión").await {
        m.info("Versión", cell(&r, "version").unwrap_or_default());
        m.info("Base actual", cell(&r, "db").unwrap_or_default());
    }
    let provisioned = m.quiet("SELECT count(DISTINCT node) AS nodes, count(*) AS slices FROM stv_slices").await;
    let Some(slices) = provisioned else {
        return redshift_serverless(m).await;
    };
    if let Some(r) = slices.first() {
        m.info("Nodos de cómputo", cell(r, "nodes").unwrap_or_default());
        m.info("Slices", cell(r, "slices").unwrap_or_default());
    }
    if let Some(r) = m.row("SELECT count(*) AS n FROM stv_sessions", "No se pudieron leer las sesiones (stv_sessions)").await {
        m.metric(Metric::new("connections", "Conexiones", "Conexiones", U::Count, f(&r, "n")));
    }
    if let Some(r) = m
        .row(
            "SELECT sum(CASE WHEN state LIKE 'Running%' THEN 1 ELSE 0 END) AS running,
                    sum(CASE WHEN state LIKE 'Queued%' THEN 1 ELSE 0 END) AS queued
             FROM stv_wlm_query_state",
            "No se pudo leer la cola de WLM (stv_wlm_query_state)",
        )
        .await
    {
        m.metric(Metric::new("active_sessions", "Consultas ejecutándose", "Conexiones", U::Count, f(&r, "running").or(Some(0.0))));
        m.metric(Metric::new("wlm_queued", "Consultas en cola (WLM)", "Conexiones", U::Count, f(&r, "queued").or(Some(0.0))));
    }
    if let Some(r) = m
        .row(
            "SELECT count(*) AS n FROM stl_query WHERE starttime > dateadd(minute, -1, getdate())",
            "No se pudo leer el historial de consultas (stl_query)",
        )
        .await
    {
        m.metric(Metric::new("queries_per_min", "Consultas (último minuto)", "Actividad", U::Count, f(&r, "n")));
    }
    if let Some(r) = m
        .row("SELECT count(*) AS n FROM svv_transactions WHERE NOT granted", "No se pudieron leer los bloqueos (svv_transactions)")
        .await
    {
        m.metric(Metric::new("locks_waiting", "Bloqueos en espera", "Bloqueos", U::Count, f(&r, "n")));
    }
    if let Some(rows) = m
        .rows(
            "SELECT owner AS node, sum(used)::bigint * 1048576 AS used, sum(capacity)::bigint * 1048576 AS capacity
             FROM stv_partitions WHERE part_begin = 0 GROUP BY owner ORDER BY owner",
            "Sin permiso de superusuario no se ve el uso de disco de los nodos (stv_partitions)",
        )
        .await
    {
        m.metric(Metric::new("storage_used", "Disco usado", "Almacenamiento", U::Bytes, sum(&rows, "used")).max(sum(&rows, "capacity")));
        m.table("nodes", "Nodos y disco", &["Nodo", "Usado", "Capacidad"], &rows, &[1, 2]);
    }
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT process, trim(user_name) AS user_name, trim(db_name) AS db_name, starttime::varchar AS started,
                    datediff(second, starttime, getdate()) AS secs
             FROM stv_sessions ORDER BY starttime LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.table("sessions", "Sesiones", &["PID", "Usuario", "Base", "Inicio", "Conectada (s)"], &rows, &[]);
    }
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT pid, trim(user_name) AS user_name, starttime::varchar AS started, round(duration / 1000000.0, 1) AS secs,
                    substring(query, 1, {MAX_TEXT}) AS query
             FROM stv_recents WHERE status = 'Running' ORDER BY starttime LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.table("queries", "Consultas en curso", &["PID", "Usuario", "Inicio", "Duración (s)", "Consulta"], &rows, &[]);
    }
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT xid, pid, trim(txn_owner) AS owner, trim(txn_db) AS db, lock_mode, relation,
                    CASE WHEN granted THEN 'sí' ELSE 'no' END AS granted, txn_start::varchar AS started
             FROM svv_transactions WHERE lockable_object_type = 'relation' ORDER BY granted, txn_start LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.table("locks", "Bloqueos", &["Transacción", "PID", "Dueño", "Base", "Modo", "Relación", "Otorgado", "Inicio"], &rows, &[]);
    }
    redshift_tables(m).await;
    m.note("Redshift no expone por SQL el uso de CPU ni de memoria de los nodos: están en CloudWatch y en la consola de Redshift.");
}

/// Table sizes (current database only: `svv_table_info` doesn't cross databases).
async fn redshift_tables(m: &mut Mon<'_>) {
    if let Some(rows) = m
        .quiet(
            "SELECT \"database\", sum(size)::bigint * 1048576 AS bytes, count(*) AS tables
             FROM svv_table_info GROUP BY 1 ORDER BY 2 DESC",
        )
        .await
    {
        m.table("databases", "Bases y tamaños", &["Base", "Tamaño", "Tablas"], &rows, &[1]);
    }
    if let Some(rows) = m
        .quiet(
            "SELECT \"schema\", \"table\", size::bigint * 1048576 AS bytes, tbl_rows, diststyle, unsorted, stats_off
             FROM svv_table_info ORDER BY size DESC LIMIT 20",
        )
        .await
    {
        m.table(
            "top_objects",
            "Tablas más grandes (esta base)",
            &["Esquema", "Tabla", "Tamaño", "Filas", "Distribución", "Sin ordenar (%)", "Estadísticas desactualizadas (%)"],
            &rows,
            &[2],
        );
    }
}

async fn redshift_serverless(m: &mut Mon<'_>) {
    m.info("Modalidad", "Serverless");
    if let Some(rows) = m
        .rows(
            &format!(
                "SELECT query_id, user_id, start_time::varchar AS started, round(elapsed_time / 1000000.0, 1) AS secs,
                        substring(query_text, 1, {MAX_TEXT}) AS query
                 FROM sys_query_history WHERE status = 'running' ORDER BY start_time LIMIT {MAX_ROWS}"
            ),
            "No se pudieron leer las consultas en curso (sys_query_history)",
        )
        .await
    {
        m.metric(Metric::new("active_sessions", "Consultas ejecutándose", "Conexiones", U::Count, Some(rows.len() as f64)));
        m.table("queries", "Consultas en curso", &["Consulta", "Usuario", "Inicio", "Duración (s)", "Texto"], &rows, &[]);
    }
    if let Some(r) = m
        .quiet("SELECT compute_capacity, compute_seconds FROM sys_serverless_usage ORDER BY start_time DESC LIMIT 1")
        .await
        .and_then(|r| r.into_iter().next())
    {
        m.metric(Metric::new("rpu", "Capacidad (RPU)", "CPU", U::Count, f(&r, "compute_capacity")));
    }
    if let Some(r) = m
        .quiet("SELECT count(*) AS n FROM sys_query_history WHERE start_time > dateadd(minute, -1, getdate())")
        .await
        .and_then(|r| r.into_iter().next())
    {
        m.metric(Metric::new("queries_per_min", "Consultas (último minuto)", "Actividad", U::Count, f(&r, "n")));
    }
    redshift_tables(m).await;
    m.note("Redshift Serverless no expone por SQL el CPU ni la memoria: la capacidad (RPU) y el consumo están en CloudWatch.");
}

// ---------------------------------------------------------------------------
// Denodo, Yellowbrick: system views shown as the server returns them.

async fn denodo(m: &mut Mon<'_>) {
    if let Some(v) = m.quiet("SELECT version() AS v").await.and_then(|r| r.first().and_then(|r| r.get(0).map(str::to_string))) {
        m.info("Versión", v);
    }
    if let Some(rows) = m
        .rows("SELECT * FROM GET_SESSIONS()", "No se pudieron leer las sesiones (GET_SESSIONS, requiere un usuario administrador)")
        .await
    {
        m.metric(Metric::new("connections", "Sesiones", "Conexiones", U::Count, Some(rows.len() as f64)));
        m.table_as_is("sessions", "Sesiones", &rows);
    }
    m.note("Denodo no expone por VQL el CPU, la memoria ni las consultas en curso: están en Denodo Monitor y por JMX.");
}

async fn yellowbrick(m: &mut Mon<'_>) {
    if let Some(r) = m.row("SELECT version() AS version, current_database() AS db", "No se pudo leer la versión").await {
        m.info("Versión", cell(&r, "version").unwrap_or_default());
        m.info("Base actual", cell(&r, "db").unwrap_or_default());
    }
    if let Some(rows) = m.rows(&format!("SELECT * FROM sys.session LIMIT {MAX_ROWS}"), "No se pudieron leer las sesiones (sys.session)").await {
        m.metric(Metric::new("connections", "Conexiones", "Conexiones", U::Count, Some(rows.len() as f64)));
        m.table_as_is("sessions", "Sesiones", &rows);
    }
    if let Some(rows) = m.rows(&format!("SELECT * FROM sys.query LIMIT {MAX_ROWS}"), "No se pudieron leer las consultas en curso (sys.query)").await {
        m.metric(Metric::new("active_sessions", "Consultas en curso", "Conexiones", U::Count, Some(rows.len() as f64)));
        m.table_as_is("queries", "Consultas en curso", &rows);
    }
    if let Some(rows) = m.quiet(&format!("SELECT * FROM sys.database LIMIT {MAX_ROWS}")).await {
        m.table_as_is("databases", "Bases", &rows);
    }
    if let Some(rows) = m.quiet(&format!("SELECT * FROM sys.worker LIMIT {MAX_ROWS}")).await {
        m.metric(Metric::new("nodes", "Workers", "Cluster", U::Count, Some(rows.len() as f64)));
        m.table_as_is("nodes", "Workers", &rows);
    }
    m.note("Yellowbrick no expone por SQL el CPU ni la memoria de los workers: están en Yellowbrick Manager.");
}

// ---------------------------------------------------------------------------
// Materialize

async fn materialize(m: &mut Mon<'_>) {
    if let Some(r) = m
        .row(
            "SELECT mz_version() AS version, extract(epoch FROM mz_uptime()) AS uptime, current_setting('cluster') AS cluster",
            "No se pudo leer la versión",
        )
        .await
    {
        m.info("Versión", cell(&r, "version").unwrap_or_default());
        m.info("Clúster de esta sesión", cell(&r, "cluster").unwrap_or_default());
        m.metric(Metric::new("uptime", "Tiempo activo", "Servidor", U::Seconds, f(&r, "uptime")));
    }
    let max_conn = m.quiet("SHOW max_connections").await.and_then(|r| r.first().and_then(|r| r.get(0).and_then(num)));
    if let Some(rows) = m
        .rows(
            &format!(
                "SELECT s.connection_id, coalesce(r.name, s.role_id) AS usr, coalesce(s.client_ip::text, '') AS client,
                        s.connected_at::text AS since, round(extract(epoch FROM now() - s.connected_at)) AS secs
                 FROM mz_internal.mz_sessions s LEFT JOIN mz_catalog.mz_roles r ON r.id = s.role_id
                 ORDER BY s.connected_at LIMIT {MAX_ROWS}"
            ),
            "No se pudieron leer las sesiones (mz_internal.mz_sessions)",
        )
        .await
    {
        m.metric(Metric::new("connections", "Conexiones", "Conexiones", U::Count, Some(rows.len() as f64)).max(max_conn));
        m.table("sessions", "Sesiones", &["Conexión", "Usuario", "Cliente", "Desde", "Conectada (s)"], &rows, &[]);
    }
    if let Some(rows) = m
        .rows(
            "SELECT c.name AS cluster, r.name AS replica, r.size, coalesce(st.status, '') AS status,
                    round(u.cpu_percent::numeric, 1) AS cpu, round(u.memory_percent::numeric, 1) AS mem,
                    round(u.disk_percent::numeric, 1) AS disk, mt.memory_bytes, sz.memory_bytes AS mem_limit
             FROM mz_catalog.mz_cluster_replicas r
             JOIN mz_catalog.mz_clusters c ON c.id = r.cluster_id
             LEFT JOIN mz_internal.mz_cluster_replica_utilization u ON u.replica_id = r.id
             LEFT JOIN mz_internal.mz_cluster_replica_metrics mt ON mt.replica_id = r.id AND mt.process_id = u.process_id
             LEFT JOIN mz_internal.mz_cluster_replica_statuses st ON st.replica_id = r.id AND st.process_id = u.process_id
             LEFT JOIN mz_catalog.mz_cluster_replica_sizes sz ON sz.size = r.size
             ORDER BY 1, 2",
            "No se pudo leer el uso de los clústeres (mz_internal.mz_cluster_replica_utilization)",
        )
        .await
    {
        m.metric(Metric::new("cpu", "CPU (réplica más cargada)", "CPU", U::Percent, max_of(&rows, "cpu")));
        m.metric(Metric::new("mem_used", "Memoria de las réplicas", "Memoria", U::Bytes, sum(&rows, "memory_bytes")).max(sum(&rows, "mem_limit")));
        m.metric(Metric::new("mem_percent", "Memoria (réplica más cargada)", "Memoria", U::Percent, max_of(&rows, "mem")));
        m.metric(Metric::new("replicas", "Réplicas de clúster", "Cluster", U::Count, Some(rows.len() as f64)));
        m.table(
            "nodes",
            "Clústeres y réplicas",
            &["Clúster", "Réplica", "Tamaño", "Estado", "CPU (%)", "Memoria (%)", "Disco (%)", "Memoria usada"],
            &rows,
            &[7],
        );
    }
    if let Some(rows) = m
        .quiet(
            "SELECT o.name, o.type, u.size_bytes
             FROM mz_catalog.mz_recent_storage_usage u JOIN mz_catalog.mz_objects o ON o.id = u.object_id
             WHERE u.size_bytes IS NOT NULL ORDER BY u.size_bytes DESC LIMIT 20",
        )
        .await
    {
        m.table("top_objects", "Objetos más grandes (almacenamiento)", &["Objeto", "Tipo", "Tamaño"], &rows, &[2]);
    }
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT name, type, status, coalesce(error, '') AS error FROM mz_internal.mz_source_statuses
             UNION ALL
             SELECT name, 'sink', status, coalesce(error, '') FROM mz_internal.mz_sink_statuses
             ORDER BY 1 LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.metric(Metric::new(
            "sources_failing",
            "Fuentes y sinks con error",
            "Streaming",
            U::Count,
            Some(rows.iter().filter(|r| matches!(cell(r, "status").as_deref(), Some("stalled" | "failed"))).count() as f64),
        ));
        m.table("sources", "Fuentes y sinks", &["Nombre", "Tipo", "Estado", "Error"], &rows, &[]);
    }
    if let Some(rows) = m
        .quiet(
            "SELECT d.name, count(o.id) AS objects FROM mz_catalog.mz_databases d
             LEFT JOIN mz_catalog.mz_schemas s ON s.database_id = d.id
             LEFT JOIN mz_catalog.mz_objects o ON o.schema_id = s.id
             GROUP BY d.name ORDER BY d.name",
        )
        .await
    {
        m.table("databases", "Bases", &["Base", "Objetos"], &rows, &[]);
    }
    m.note(
        "Materialize no tiene bloqueos ni lee de disco por consulta: no hay bloqueos, esperas ni E/S que mostrar; \
         las sentencias en curso solo quedan registradas con statement logging (mz_internal.mz_recent_activity_log).",
    );
}

// ---------------------------------------------------------------------------
// RisingWave

async fn risingwave(m: &mut Mon<'_>) {
    if let Some(r) = m
        .row(
            "SELECT version() AS version, extract(epoch FROM now() - pg_postmaster_start_time()) AS uptime",
            "No se pudo leer la versión",
        )
        .await
    {
        m.info("Versión", cell(&r, "version").unwrap_or_default());
        m.metric(Metric::new("uptime", "Tiempo activo", "Servidor", U::Seconds, f(&r, "uptime")));
    }
    if let Some(rows) = m
        .rows(
            "SELECT id, host, port, type, state, parallelism, rw_version, system_total_cpu_cores,
                    system_total_memory_bytes, started_at::varchar AS started
             FROM rw_catalog.rw_worker_nodes ORDER BY id",
            "No se pudieron leer los nodos (rw_catalog.rw_worker_nodes)",
        )
        .await
    {
        let compute = rows.iter().filter(|r| cell(r, "type").as_deref() == Some("WORKER_TYPE_COMPUTE_NODE")).count();
        m.metric(Metric::new("nodes", "Nodos", "Cluster", U::Count, Some(rows.len() as f64)));
        m.metric(Metric::new("compute_nodes", "Nodos de cómputo", "Cluster", U::Count, Some(compute as f64)));
        m.table(
            "nodes",
            "Nodos del clúster",
            &["ID", "Host", "Puerto", "Tipo", "Estado", "Paralelismo", "Versión", "CPUs", "Memoria del host", "Iniciado"],
            &rows,
            &[8],
        );
    }
    if let Some(rows) = m.rows("SHOW PROCESSLIST", "No se pudieron leer las sesiones (SHOW PROCESSLIST)").await {
        m.metric(Metric::new("connections", "Conexiones", "Conexiones", U::Count, Some(rows.len() as f64)));
        m.table_as_is("sessions", "Sesiones", &rows);
    }
    if let Some(rows) = m
        .rows(
            "SELECT sc.name AS schema, r.name, r.relation_type, s.total_key_count,
                    s.total_key_size + s.total_value_size AS bytes
             FROM rw_catalog.rw_table_stats s
             JOIN rw_catalog.rw_relations r ON r.id = s.id
             JOIN rw_catalog.rw_schemas sc ON sc.id = r.schema_id
             ORDER BY bytes DESC LIMIT 20",
            "No se pudo leer el tamaño de las tablas (rw_catalog.rw_table_stats)",
        )
        .await
    {
        m.table("top_objects", "Objetos más grandes (estado)", &["Esquema", "Objeto", "Tipo", "Claves", "Tamaño"], &rows, &[4]);
    }
    if let Some(r) = m
        .quiet("SELECT sum(total_key_size + total_value_size) AS bytes FROM rw_catalog.rw_table_stats")
        .await
        .and_then(|r| r.into_iter().next())
    {
        m.metric(Metric::new("storage_used", "Estado almacenado", "Almacenamiento", U::Bytes, f(&r, "bytes").or(Some(0.0))));
    }
    if let Some(rows) = m
        .quiet(&format!("SELECT id, name, status, parallelism FROM rw_catalog.rw_streaming_jobs ORDER BY id LIMIT {MAX_ROWS}"))
        .await
    {
        m.metric(Metric::new("streaming_jobs", "Jobs de streaming", "Streaming", U::Count, Some(rows.len() as f64)));
        m.table("jobs", "Jobs de streaming", &["ID", "Nombre", "Estado", "Paralelismo"], &rows, &[]);
    }
    m.note("RisingWave no expone por SQL el uso de CPU ni de memoria de los nodos, ni el throughput: están en su dashboard y en Prometheus.");
}

// ---------------------------------------------------------------------------
// CrateDB

async fn cratedb(m: &mut Mon<'_>) {
    if let Some(r) = m
        .quiet("SELECT c.name AS cluster, n.name AS master FROM sys.cluster c LEFT JOIN sys.nodes n ON n.id = c.master_node")
        .await
        .and_then(|r| r.into_iter().next())
    {
        m.info("Clúster", cell(&r, "cluster").unwrap_or_default());
        m.info("Nodo maestro", cell(&r, "master").unwrap_or_default());
    }
    if let Some(rows) = m
        .rows(
            "SELECT name, hostname, version['number'] AS version,
                    process['cpu']['percent'] AS cpu, os['cpu']['used'] AS os_cpu, load['1'] AS load1,
                    heap['used'] AS heap_used, heap['max'] AS heap_max,
                    mem['used'] AS mem_used, mem['free'] AS mem_free,
                    fs['total']['used'] AS fs_used, fs['total']['size'] AS fs_size,
                    fs['total']['bytes_read'] AS disk_read, fs['total']['bytes_written'] AS disk_write,
                    connections['psql']['open'] + connections['http']['open'] AS conns
             FROM sys.nodes ORDER BY name",
            "No se pudieron leer los nodos (sys.nodes)",
        )
        .await
    {
        if let Some(v) = rows.first().and_then(|r| cell(r, "version")) {
            m.info("Versión", format!("CrateDB {v}"));
        }
        m.info("Nodos", rows.len().to_string());
        let n = rows.len().max(1) as f64;
        // The OS figure when the node reports it (-1 in containers), else
        // the CrateDB process.
        let os_cpu: Vec<f64> = rows.iter().filter_map(|r| f(r, "os_cpu")).filter(|v| *v >= 0.0).collect();
        let cpu = if os_cpu.is_empty() {
            sum(&rows, "cpu").map(|c| c / n)
        } else {
            Some(os_cpu.iter().sum::<f64>() / os_cpu.len() as f64)
        };
        m.metric(Metric::new("cpu", "CPU (promedio de nodos)", "CPU", U::Percent, cpu));
        m.metric(Metric::new("load", "Carga (1 min, promedio)", "CPU", U::Count, sum(&rows, "load1").map(|l| l / n)));
        m.metric(Metric::new("heap_used", "Heap de la JVM", "Memoria", U::Bytes, sum(&rows, "heap_used")).max(sum(&rows, "heap_max")));
        let total = add(sum(&rows, "mem_used"), sum(&rows, "mem_free"));
        m.metric(Metric::new("mem_used", "Memoria de los hosts", "Memoria", U::Bytes, sum(&rows, "mem_used")).max(total));
        m.metric(Metric::new("connections", "Conexiones", "Conexiones", U::Count, sum(&rows, "conns")));
        m.metric(Metric::new("disk_read", "Lectura de disco", "Disco", U::Bytes, sum(&rows, "disk_read")).counter());
        m.metric(Metric::new("disk_write", "Escritura en disco", "Disco", U::Bytes, sum(&rows, "disk_write")).counter());
        m.metric(Metric::new("storage_used", "Disco usado (sistema de archivos)", "Almacenamiento", U::Bytes, sum(&rows, "fs_used")).max(sum(&rows, "fs_size")));
        m.table(
            "nodes",
            "Nodos del clúster",
            &["Nodo", "Host", "Versión", "CPU del proceso (%)", "CPU del SO (%)", "Carga", "Heap usado", "Heap máximo", "Memoria usada", "Memoria libre", "Disco usado", "Disco total"],
            &rows,
            &[6, 7, 8, 9, 10, 11],
        );
        if os_cpu.is_empty() {
            m.note("El sistema operativo de los nodos no informa el CPU total (os['cpu']['used'] = -1): se muestra el del proceso de CrateDB.");
        }
    }
    if let Some(r) = m
        .row(
            "SELECT sum(total_count) AS total, sum(failed_count) AS failed FROM sys.jobs_metrics",
            "No se pudieron leer las métricas de consultas (sys.jobs_metrics)",
        )
        .await
    {
        m.metric(Metric::new("queries", "Consultas", "Actividad", U::Count, f(&r, "total")).counter());
        m.metric(Metric::new("failed_queries", "Consultas con error", "Actividad", U::Count, f(&r, "failed")).counter());
    }
    if let Some(rows) = m
        .rows(
            &format!("SELECT id, username, node['name'] AS node, started, stmt FROM sys.jobs ORDER BY started LIMIT {MAX_ROWS}"),
            "No se pudieron leer las consultas en curso (sys.jobs)",
        )
        .await
    {
        // This very query is one of them.
        m.metric(Metric::new("active_sessions", "Consultas en curso", "Conexiones", U::Count, Some(rows.len().saturating_sub(1) as f64)));
        m.table("queries", "Consultas en curso", &["Job", "Usuario", "Nodo", "Inicio", "Consulta"], &rows, &[]);
    }
    if let Some(rows) = m
        .rows(
            "SELECT schema_name, table_name, sum(num_docs) AS docs, sum(size) AS bytes, count(*) AS shards
             FROM sys.shards WHERE \"primary\" GROUP BY schema_name, table_name ORDER BY bytes DESC LIMIT 20",
            "No se pudo leer el tamaño de las tablas (sys.shards)",
        )
        .await
    {
        m.table("top_objects", "Tablas más grandes (shards primarios)", &["Esquema", "Tabla", "Documentos", "Tamaño", "Shards"], &rows, &[3]);
    }
    if let Some(r) = m
        .quiet("SELECT sum(size) AS bytes FROM sys.shards WHERE \"primary\"")
        .await
        .and_then(|r| r.into_iter().next())
    {
        m.metric(Metric::new("data_size", "Datos (shards primarios)", "Almacenamiento", U::Bytes, f(&r, "bytes").or(Some(0.0))));
    }
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT table_schema, table_name, health, missing_shards, underreplicated_shards
             FROM sys.health ORDER BY severity DESC, table_schema, table_name LIMIT {MAX_ROWS}"
        ))
        .await
    {
        m.metric(Metric::new("underreplicated_shards", "Shards sub-replicados", "Cluster", U::Count, sum(&rows, "underreplicated_shards").or(Some(0.0))));
        m.metric(Metric::new("missing_shards", "Shards faltantes", "Cluster", U::Count, sum(&rows, "missing_shards").or(Some(0.0))));
        m.table("health", "Salud de las tablas", &["Esquema", "Tabla", "Salud", "Shards faltantes", "Shards sub-replicados"], &rows, &[]);
    }
    if let Some(rows) = m.quiet("SELECT id, severity, description FROM sys.checks WHERE NOT passed ORDER BY severity DESC").await {
        for r in &rows {
            if let Some(d) = cell(r, "description") {
                m.note(format!("Chequeo del clúster: {}", d.lines().next().unwrap_or("").trim()));
            }
        }
    }
    m.note("CrateDB no bloquea filas ni tiene transacciones: no hay bloqueos ni esperas que mostrar.");
}

// ---------------------------------------------------------------------------
// H2 (PostgreSQL server mode): INFORMATION_SCHEMA and the JVM.

async fn h2(m: &mut Mon<'_>) {
    if let Some(v) = m.quiet("SELECT H2VERSION() AS v").await.and_then(|r| r.first().and_then(|r| cell(r, "v"))) {
        m.info("Versión", format!("H2 {v}"));
    }
    if let Some(p) = m.quiet("SELECT DATABASE_PATH() AS p").await.and_then(|r| r.first().and_then(|r| cell(r, "p"))) {
        m.info("Archivo de la base", p);
    }
    if let Some(r) = m
        .row("SELECT MEMORY_USED() AS used, MEMORY_FREE() AS free", "No se pudo leer la memoria de la JVM")
        .await
    {
        // Kilobytes.
        let used = f(&r, "used").map(|k| k * 1024.0);
        m.metric(Metric::new("mem_used", "Memoria de la JVM", "Memoria", U::Bytes, used).max(add(used, f(&r, "free").map(|k| k * 1024.0))));
    }
    if let Some(rows) = m
        .rows(
            "SELECT setting_name, setting_value FROM information_schema.settings
             WHERE setting_name IN ('info.PAGE_COUNT', 'info.PAGE_SIZE', 'info.CACHE_SIZE', 'info.CACHE_MAX_SIZE',
                                    'info.FILE_READ', 'info.FILE_WRITE', 'MODE', 'CACHE_SIZE')",
            "No se pudieron leer los parámetros (information_schema.settings)",
        )
        .await
    {
        let g = |n: &str| rows.iter().find(|r| cell(r, "setting_name").as_deref() == Some(n)).and_then(|r| f(r, "setting_value"));
        let pages = g("info.PAGE_COUNT").zip(g("info.PAGE_SIZE")).map(|(c, s)| c * s);
        m.metric(Metric::new("storage_used", "Tamaño de la base", "Almacenamiento", U::Bytes, pages));
        // CACHE_SIZE / CACHE_MAX_SIZE are in KB.
        m.metric(Metric::new("mem_cache", "Caché de páginas", "Memoria", U::Bytes, g("info.CACHE_SIZE").map(|k| k * 1024.0)).max(g("info.CACHE_MAX_SIZE").map(|k| k * 1024.0)));
        m.metric(Metric::new("disk_read", "Lecturas del archivo", "Disco", U::Count, g("info.FILE_READ")).counter());
        m.metric(Metric::new("disk_write", "Escrituras del archivo", "Disco", U::Count, g("info.FILE_WRITE")).counter());
        if let Some(mode) = rows.iter().find(|r| cell(r, "setting_name").as_deref() == Some("MODE")).and_then(|r| cell(r, "setting_value")) {
            m.info("Modo de compatibilidad", mode);
        }
    }
    if let Some(rows) = m
        .rows(
            &format!(
                "SELECT session_id, user_name, client_addr, CAST(session_start AS VARCHAR) AS started,
                        CASE WHEN executing_statement IS NULL THEN 'inactiva' ELSE 'activa' END AS state,
                        CAST(executing_statement_start AS VARCHAR) AS since, executing_statement, blocker_id
                 FROM information_schema.sessions ORDER BY session_start LIMIT {MAX_ROWS}"
            ),
            "No se pudieron leer las sesiones (information_schema.sessions, requiere ADMIN)",
        )
        .await
    {
        let active = rows.iter().filter(|r| cell(r, "state").as_deref() == Some("activa")).count();
        let blocked = rows.iter().filter(|r| cell(r, "blocker_id").is_some()).count();
        m.metric(Metric::new("connections", "Sesiones", "Conexiones", U::Count, Some(rows.len() as f64)));
        // This very query is one of them.
        m.metric(Metric::new("active_sessions", "Sesiones activas", "Conexiones", U::Count, Some(active.saturating_sub(1) as f64)));
        m.metric(Metric::new("locks_waiting", "Sesiones bloqueadas", "Bloqueos", U::Count, Some(blocked as f64)));
        m.table(
            "sessions",
            "Sesiones",
            &["Sesión", "Usuario", "Cliente", "Inicio", "Estado", "Sentencia desde", "Sentencia", "Bloqueada por"],
            &rows,
            &[],
        );
    }
    if let Some(rows) = m
        .quiet(&format!("SELECT table_schema, table_name, session_id, lock_type FROM information_schema.locks LIMIT {MAX_ROWS}"))
        .await
    {
        m.table("locks", "Bloqueos", &["Esquema", "Tabla", "Sesión", "Tipo"], &rows, &[]);
    }
    if let Some(rows) = m
        .quiet(&format!(
            "SELECT table_schema, table_name, row_count_estimate FROM information_schema.tables
             WHERE table_type = 'BASE TABLE' AND table_schema NOT IN ('INFORMATION_SCHEMA', 'pg_catalog')
             ORDER BY row_count_estimate DESC LIMIT 20"
        ))
        .await
    {
        m.table("top_objects", "Tablas con más filas", &["Esquema", "Tabla", "Filas (estim.)"], &rows, &[]);
    }
    m.note("H2 corre dentro de una JVM: el CPU del proceso no se expone por SQL; la memoria es la de la JVM.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_become_numbers_only_when_plain() {
        assert_eq!(json_cell(Some("42")), Value::from(42));
        assert_eq!(json_cell(Some("-1.5")), serde_json::json!(-1.5));
        assert_eq!(json_cell(Some("0.25")), serde_json::json!(0.25));
        assert_eq!(json_cell(Some("007")), Value::String("007".into()));
        assert_eq!(json_cell(Some("10.0.0.1")), Value::String("10.0.0.1".into()));
        assert_eq!(json_cell(Some("2024-01-31")), Value::String("2024-01-31".into()));
        assert_eq!(json_cell(None), Value::Null);
        let long = "x".repeat(MAX_TEXT + 10);
        let Value::String(cut) = json_cell(Some(&long)) else { panic!() };
        assert_eq!(cut.chars().count(), MAX_TEXT + 1);
    }

    #[test]
    fn settings_in_bytes() {
        assert_eq!(setting_bytes("16384", Some("8kB")), Some(16384.0 * 8192.0));
        assert_eq!(setting_bytes("4096", Some("kB")), Some(4096.0 * 1024.0));
        assert_eq!(setting_bytes("1024", Some("MB")), Some(1024.0 * 1048576.0));
        assert_eq!(setting_bytes("100", None), None);
        assert_eq!(setting_bytes("200", Some("ms")), None);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(512.0), "512 B");
        assert_eq!(human_bytes(1536.0), "1.5 KB");
        assert_eq!(human_bytes(128.0 * 1048576.0), "128.0 MB");
    }

    #[test]
    fn server_reasons_lose_the_severity() {
        assert_eq!(reason("ERROR: permission denied for view x\nDetalle: y"), "permission denied for view x");
        assert_eq!(reason("se cerró la conexión"), "se cerró la conexión");
    }

    #[test]
    fn ratios_and_sums() {
        assert_eq!(ratio(Some(90.0), Some(10.0)), Some(90.0));
        assert_eq!(ratio(Some(0.0), Some(0.0)), None);
        assert_eq!(add(Some(1.0), None), Some(1.0));
        assert_eq!(add(None, None), None);
    }
}

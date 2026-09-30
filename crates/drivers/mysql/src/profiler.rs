//! The profiler ([`dbine_driver::profiler`]) per variant.
//!
//! - MySQL, MariaDB and the managed MySQL services, with Performance
//!   Schema on: its statement history, complete.
//!   `events_statements_history_long` keeps the last statements of the whole
//!   server; its consumer is off by default, so the profiler switches it on
//!   (when allowed) and back off at stop. Otherwise it reads
//!   `events_statements_history` (the last 10 of each connection, on by
//!   default), and without Performance Schema (MariaDB's default) it samples
//!   `information_schema.PROCESSLIST`.
//! - TiDB: the slow query log (`CLUSTER_SLOW_QUERY`) with its threshold at 0
//!   logs every statement, complete; the profiler lowers it on the instance
//!   it's connected to and puts it back at stop. Otherwise it samples
//!   `CLUSTER_PROCESSLIST`.
//! - OceanBase: `GV$OB_SQL_AUDIT` (on by default), complete; else the
//!   processlist.
//! - SingleStore: `information_schema.PROCESSLIST`; StarRocks, Doris and
//!   VeloDB: `SHOW FULL PROCESSLIST` (their information_schema view only
//!   lists this session); Databend: `system.processes`; Manticore:
//!   `SHOW THREADS`; GreptimeDB: `information_schema.process_list`. All
//!   sampled: they only show what runs now.

use crate::session::{at, lit, named, MySqlSession};
use crate::Variant;
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use mysql_async::Row;
use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Rows read from a history per poll.
const BATCH: usize = 2000;
/// Consumers `events_statements_history_long` needs, in dependency order.
const LONG: [&str; 4] =
    ["global_instrumentation", "thread_instrumentation", "events_statements_current", "events_statements_history_long"];
const SHORT: [&str; 4] =
    ["global_instrumentation", "thread_instrumentation", "events_statements_current", "events_statements_history"];

pub(crate) struct State {
    kind: Kind,
    /// The database watched (empty: all).
    database: String,
    /// Statements that put back what `start` changed.
    restore: Vec<String>,
}

enum Kind {
    History(History),
    Sampled(Sampling),
}

#[derive(Clone, Copy)]
enum Source {
    /// Performance Schema: `table`, this connection's thread, and a
    /// timer (picoseconds) at a known epoch time (µs); `cpu`: the table has
    /// `CPU_TIME` (MySQL 8.0.28+).
    Ps { table: &'static str, own: u64, timer: u64, epoch_us: i64, cpu: bool },
    /// TiDB's slow query log.
    SlowLog,
    /// OceanBase's SQL audit.
    SqlAudit,
}

/// A history read by its end (a statement is written there when it
/// finishes): rows that ended after `after - MARGIN`, less the ids seen.
struct History {
    source: Source,
    after: i64,
    seen: HashMap<String, i64>,
}

struct Sampling {
    sampler: Sampler,
    sql: String,
    clock: Clock,
    /// This connection's id, to leave it out.
    own: String,
    /// What each connection was seen running, to keep its start fixed
    /// when the engine only gives the elapsed time.
    seen: HashMap<String, Seen>,
}

struct Seen {
    key: String,
    started: i64,
    /// The earliest start the looks suggest (the elapsed time is rounded
    /// down).
    earliest: i64,
}

/// The server's time (epoch µs), from a reading at start.
#[derive(Clone, Copy)]
struct Clock {
    epoch_us: i64,
    at: Instant,
}

impl Clock {
    fn now(&self) -> i64 {
        self.epoch_us + self.at.elapsed().as_micros() as i64
    }
}

pub(crate) async fn start(s: &mut MySqlSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let v = s.variant;
    let mut restore = Vec::new();
    let clock = clock(s).await;
    let history = match v {
        Variant::MySql | Variant::MariaDb => performance_schema(s, opts, &mut restore).await,
        Variant::TiDb => slow_log(s, opts, &mut restore, &clock).await,
        Variant::OceanBase => sql_audit(s, &clock).await,
        _ => Err(String::new()),
    };
    let (kind, started) = match history {
        Ok((h, started)) => (Kind::History(h), started),
        Err(why) => {
            let (sql, source) = sample_sql(v);
            let mut started = ProfilerStarted::new(ProfilerMode::Sampled, source);
            if v == Variant::Databend {
                started = started.units(Some("bytes"), Some("bytes"));
            }
            let mut notes: Vec<String> = Vec::new();
            if !why.is_empty() {
                notes.push(why);
            }
            match v {
                Variant::StarRocks | Variant::Doris => {
                    notes.push("Solo se ven las conexiones del frontend al que está conectado DBine.".into())
                }
                Variant::Manticore => {
                    notes.push("Manticore responde casi todas las búsquedas en milisegundos: solo se ven las que tardan más.".into())
                }
                _ => {}
            }
            // Fail now rather than at every poll.
            s.rows(&sql).await.map_err(|e| Error::Query(format!("no se pudo leer {source} ({e})")))?;
            if !notes.is_empty() {
                started = started.note(notes.join(" "));
            }
            let sampling = Sampling {
                sampler: Sampler::new(utc(clock.now())),
                sql,
                clock,
                own: s.conn.id().to_string(),
                seen: HashMap::new(),
            };
            (Kind::Sampled(sampling), started)
        }
    };
    Ok((State { kind, database: opts.database.clone(), restore }, started))
}

pub(crate) async fn poll(s: &mut MySqlSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let db = state.database.clone();
    let mut out = match &mut state.kind {
        Kind::History(h) => {
            let rows = s.rows(&history_sql(h.source, h.after - margin(h.source), &db)).await?;
            h.read(&rows)
        }
        Kind::Sampled(p) => {
            let mut out = Vec::new();
            let until = Instant::now() + SAMPLE_FOR;
            loop {
                let rows = s.rows(&p.sql).await?;
                let now = p.clock.now();
                let samples = p.look(&rows, now, &db);
                out.extend(p.sampler.feed(samples));
                if Instant::now() + SAMPLE_EVERY > until {
                    break;
                }
                tokio::time::sleep(SAMPLE_EVERY).await;
            }
            out
        }
    };
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

/// Put back what `start` changed.
pub(crate) async fn stop(s: &mut MySqlSession, state: State) -> Result<()> {
    for sql in state.restore {
        s.rows(&sql).await?;
    }
    Ok(())
}

/// The server's clock now; the local one where the engine has no
/// `UNIX_TIMESTAMP()`.
async fn clock(s: &mut MySqlSession) -> Clock {
    for sql in ["SELECT UNIX_TIMESTAMP(NOW(6))", "SELECT UNIX_TIMESTAMP()"] {
        let t = s.optional_rows(sql).await.first().and_then(|r| at(r, 0)).and_then(|t| t.parse::<f64>().ok());
        if let Some(secs) = t {
            return Clock { epoch_us: (secs * 1e6) as i64, at: Instant::now() };
        }
    }
    let local = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64);
    Clock { epoch_us: local, at: Instant::now() }
}

// ------------------------------------------------------------ complete

/// How far back each poll reads again, for statements written to the
/// history a little after they ended.
fn margin(source: Source) -> i64 {
    match source {
        // Picoseconds.
        Source::Ps { .. } => 500_000_000_000,
        // Microseconds.
        Source::SlowLog | Source::SqlAudit => 1_000_000,
    }
}

impl History {
    fn new(source: Source, after: i64) -> Self {
        Self { source, after, seen: HashMap::new() }
    }

    fn read(&mut self, rows: &[Row]) -> Vec<ProfiledStatement> {
        let get = |r: &Row, n: &str| named(r, &[n]).filter(|v| !v.is_empty());
        let num = |r: &Row, n: &str| get(r, n).and_then(|v| v.parse::<f64>().ok());
        let mut out = Vec::new();
        for r in rows {
            let (Some(id), Some(end)) = (get(r, "id"), num(r, "end")) else { continue };
            let end = end as i64;
            if self.seen.contains_key(&id) {
                continue;
            }
            self.seen.insert(id, end);
            self.after = self.after.max(end);
            let text = get(r, "text").unwrap_or_default();
            let text = text.trim().trim_end_matches(';').trim_end().to_string();
            if text.is_empty() {
                continue;
            }
            // Start: epoch µs, or a Performance Schema timer.
            let start = num(r, "start").unwrap_or(0.0) as i64;
            let start = match self.source {
                Source::Ps { timer, epoch_us, .. } => epoch_us + (start - timer as i64) / 1_000_000,
                _ => start,
            };
            let affected = num(r, "affected").unwrap_or(0.0) as u64;
            let rows = if affected > 0 { Some(affected) } else { num(r, "sent").map(|n| n as u64) };
            out.push(ProfiledStatement {
                time: utc(start),
                duration_ms: num(r, "ms"),
                text,
                database: get(r, "db"),
                user: get(r, "usr"),
                client: get(r, "client"),
                rows,
                error: get(r, "error"),
                detail: get(r, "detail"),
                application: get(r, "app"),
                cpu_ms: num(r, "cpu_ms"),
                reads: num(r, "read_n").map(|n| n as u64),
                writes: num(r, "write_n").map(|n| n as u64),
            });
        }
        let floor = self.after - margin(self.source);
        self.seen.retain(|_, end| *end >= floor);
        out
    }
}

/// Statements that ended at `from` or later, oldest first.
fn history_sql(source: Source, from: i64, db: &str) -> String {
    let db = if db.is_empty() { None } else { Some(lit(db)) };
    match source {
        Source::Ps { table, own, cpu, .. } => format!(
            "SELECT CONCAT(e.THREAD_ID, '/', e.EVENT_ID) AS id, e.TIMER_END AS end, e.TIMER_START AS start, \
                    e.TIMER_WAIT / 1000000000 AS ms, e.SQL_TEXT AS text, e.CURRENT_SCHEMA AS db, \
                    t.PROCESSLIST_USER AS usr, \
                    (SELECT a.ATTR_VALUE FROM performance_schema.session_connect_attrs a \
                     WHERE a.PROCESSLIST_ID = t.PROCESSLIST_ID AND a.ATTR_NAME = 'program_name' LIMIT 1) AS app, \
                    t.PROCESSLIST_HOST AS client, \
                    e.ROWS_SENT AS sent, e.ROWS_AFFECTED AS affected, \
                    CASE WHEN e.ERRORS > 0 THEN COALESCE(e.MESSAGE_TEXT, CONCAT('error ', e.MYSQL_ERRNO)) END AS error, \
                    e.ROWS_EXAMINED AS read_n, {} AS cpu_ms, \
                    IF(e.NO_INDEX_USED > 0, 'sin índice', NULL) AS detail \
             FROM performance_schema.{table} e \
             LEFT JOIN performance_schema.threads t ON t.THREAD_ID = e.THREAD_ID \
             WHERE e.TIMER_END >= {from} AND e.THREAD_ID <> {own} AND e.SQL_TEXT IS NOT NULL \
               AND e.NESTING_EVENT_TYPE IS NULL{} \
             ORDER BY e.TIMER_END LIMIT {BATCH}",
            // Picoseconds.
            if cpu { "e.CPU_TIME / 1000000000" } else { "NULL" },
            db.map(|d| format!(" AND e.CURRENT_SCHEMA = {d}")).unwrap_or_default()
        ),
        // `Time` is when it ended; `Query` ends with ";".
        Source::SlowLog => format!(
            "SELECT CONCAT(INSTANCE, '/', Conn_ID, '/', UNIX_TIMESTAMP(Time)) AS id, \
                    UNIX_TIMESTAMP(Time) * 1000000 AS end, (UNIX_TIMESTAMP(Time) - Query_time) * 1000000 AS start, \
                    Query_time * 1000 AS ms, Query AS text, DB AS db, User AS usr, Host AS client, \
                    Result_rows AS sent, IF(Succ, NULL, 'falló (el registro de TiDB no guarda el mensaje)') AS error, \
                    Process_keys AS read_n, Write_keys AS write_n, CONCAT('memoria máx.: ', Mem_max, ' B') AS detail \
             FROM information_schema.CLUSTER_SLOW_QUERY \
             WHERE Time >= FROM_UNIXTIME({}) AND Is_internal = 0 AND Conn_ID <> CONNECTION_ID(){} \
             ORDER BY Time LIMIT {BATCH}",
            from as f64 / 1e6,
            db.map(|d| format!(" AND DB = {d}")).unwrap_or_default()
        ),
        Source::SqlAudit => format!(
            "SELECT CONCAT(SVR_IP, ':', SVR_PORT, '/', REQUEST_ID) AS id, REQUEST_TIME + ELAPSED_TIME AS end, \
                    REQUEST_TIME AS start, ELAPSED_TIME / 1000 AS ms, QUERY_SQL AS text, DB_NAME AS db, \
                    USER_NAME AS usr, CLIENT_IP AS client, RETURN_ROWS AS sent, AFFECTED_ROWS AS affected, \
                    IF(RET_CODE = 0, NULL, CONCAT('error ', RET_CODE)) AS error \
             FROM oceanbase.GV$OB_SQL_AUDIT \
             WHERE REQUEST_TIME + ELAPSED_TIME >= {from} AND IS_INNER_SQL = 0 AND IS_EXECUTOR_RPC = 0 \
               AND SID <> CONNECTION_ID(){} \
             ORDER BY REQUEST_TIME + ELAPSED_TIME LIMIT {BATCH}",
            db.map(|d| format!(" AND DB_NAME = {d}")).unwrap_or_default()
        ),
    }
}

/// MySQL and MariaDB: Performance Schema's statement history, switching
/// its consumers on when needed and allowed. `Err` says why not.
async fn performance_schema(
    s: &mut MySqlSession,
    opts: &ProfilerOptions,
    restore: &mut Vec<String>,
) -> std::result::Result<(History, ProfilerStarted), String> {
    let on = s.optional_rows("SELECT @@performance_schema").await;
    if on.first().and_then(|r| at(r, 0)).as_deref() != Some("1") {
        return Err("Performance Schema está desactivado (performance_schema = OFF, se cambia solo al reiniciar): \
                    se muestrean las consultas en curso, y las muy rápidas pueden no verse."
            .into());
    }
    // This connection's thread and its timer against the clock, read by
    // the same statement.
    let calib = s
        .rows(
            "SELECT e.THREAD_ID, e.TIMER_START, UNIX_TIMESTAMP(NOW(6)) \
             FROM performance_schema.events_statements_current e \
             JOIN performance_schema.threads t ON t.THREAD_ID = e.THREAD_ID \
             WHERE t.PROCESSLIST_ID = CONNECTION_ID()",
        )
        .await
        .map_err(|e| format!("No se puede leer Performance Schema ({e}): se muestrean las consultas en curso."))?;
    let (own, timer, epoch_us) = calib
        .first()
        .and_then(|r| {
            let at = |i| at(r, i);
            Some((at(0)?.parse::<u64>().ok()?, at(1)?.parse::<u64>().ok()?, (at(2)?.parse::<f64>().ok()? * 1e6) as i64))
        })
        .ok_or_else(|| "Performance Schema no registra las sentencias de esta conexión: se muestrean las consultas en curso.".to_string())?;
    let consumers: HashMap<String, bool> = s
        .optional_rows("SELECT NAME, ENABLED FROM performance_schema.setup_consumers")
        .await
        .iter()
        .filter_map(|r| Some((at(r, 0)?, at(r, 1)?.eq_ignore_ascii_case("YES"))))
        .collect();
    let off = |names: &[&str]| -> Vec<String> {
        names.iter().filter(|n| !consumers.get(**n).copied().unwrap_or(false)).map(|n| n.to_string()).collect()
    };
    // CPU_TIME (MySQL 8.0.28+) is measured only while the
    // events_statements_cpu consumer is on; otherwise it reads 0.
    const CPU: &str = "events_statements_cpu";
    let has_cpu = s
        .optional_rows(
            "SELECT 1 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = 'performance_schema' \
             AND TABLE_NAME = 'events_statements_current' AND COLUMN_NAME = 'CPU_TIME'",
        )
        .await
        .first()
        .is_some();
    let cpu_off = has_cpu && !off(&[CPU]).is_empty();
    let history = |table, cpu| History::new(Source::Ps { table, own, timer, epoch_us, cpu }, timer as i64);
    let complete = |source: &str| ProfilerStarted::new(ProfilerMode::Complete, source).units(Some("filas"), None);
    let source = "performance_schema.events_statements_history_long";
    let mut missing = off(&LONG);
    let long_on = missing.is_empty();
    if cpu_off && opts.change_server {
        missing.push(CPU.to_string());
    }
    if missing.is_empty() {
        let mut started = complete(source);
        if cpu_off {
            started = started.note(format!("El consumidor {CPU} de Performance Schema está desactivado: no se ve la CPU."));
        }
        return Ok((history("events_statements_history_long", has_cpu && !cpu_off), started));
    }
    let list = missing.iter().map(|n| lit(n)).collect::<Vec<_>>().join(", ");
    let mut refused = None;
    if opts.change_server {
        match s.rows(&format!("UPDATE performance_schema.setup_consumers SET ENABLED = 'YES' WHERE NAME IN ({list})")).await {
            Ok(_) => {
                restore.push(format!("UPDATE performance_schema.setup_consumers SET ENABLED = 'NO' WHERE NAME IN ({list})"));
                let mut started = complete(source);
                for n in &missing {
                    started = started.change(format!("consumidor {n} de Performance Schema (estaba desactivado)"));
                }
                return Ok((history("events_statements_history_long", has_cpu), started));
            }
            Err(e) if long_on => {
                // Only the CPU consumer was missing.
                return Ok((
                    history("events_statements_history_long", false),
                    complete(source).note(format!("No se pudo activar {CPU} ({e}): no se ve la CPU.")),
                ));
            }
            Err(e) => refused = Some(e),
        }
    }
    let why = match refused {
        Some(e) => format!("No se pudo activar events_statements_history_long ({e})"),
        None => "events_statements_history_long está desactivado y la conexión es de solo lectura".into(),
    };
    if off(&SHORT).is_empty() {
        return Ok((
            history("events_statements_history", has_cpu && !cpu_off),
            complete("performance_schema.events_statements_history").note(format!(
                "{why}: se leen las últimas 10 sentencias de cada conexión, así que una conexión muy activa, \
                 o que se cierra enseguida, puede perder algunas."
            )),
        ));
    }
    Err(format!("{why}: se muestrean las consultas en curso."))
}

/// TiDB: the slow query log with its threshold at 0.
async fn slow_log(
    s: &mut MySqlSession,
    opts: &ProfilerOptions,
    restore: &mut Vec<String>,
    clock: &Clock,
) -> std::result::Result<(History, ProfilerStarted), String> {
    let was = s
        .optional_rows("SELECT @@tidb_slow_log_threshold")
        .await
        .first()
        .and_then(|r| at(r, 0))
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(String::new)?;
    let mut started =
        ProfilerStarted::new(ProfilerMode::Complete, "information_schema.CLUSTER_SLOW_QUERY").units(Some("claves"), Some("claves"));
    if was > 0 {
        if !opts.change_server {
            return Err(format!(
                "El registro de consultas lentas de TiDB solo guarda las de más de {was} ms y la conexión es de solo lectura: \
                 se muestrean las consultas en curso."
            ));
        }
        if let Err(e) = s.rows("SET GLOBAL tidb_slow_log_threshold = 0").await {
            return Err(format!("No se pudo bajar tidb_slow_log_threshold a 0 ({e}): se muestrean las consultas en curso."));
        }
        restore.push(format!("SET GLOBAL tidb_slow_log_threshold = {was}"));
        started = started
            .change(format!("tidb_slow_log_threshold = 0 (estaba en {was} ms)"))
            .note("El umbral es de cada instancia de TiDB: en las demás solo se registran las consultas lentas.");
    }
    let history = History::new(Source::SlowLog, clock.now());
    if let Err(e) = s.rows(&history_sql(Source::SlowLog, clock.now(), "")).await {
        for sql in restore.drain(..) {
            let _ = s.rows(&sql).await;
        }
        return Err(format!("No se puede leer CLUSTER_SLOW_QUERY ({e}): se muestrean las consultas en curso."));
    }
    Ok((history, started))
}

/// OceanBase: its SQL audit, on by default (`ob_enable_sql_audit`).
async fn sql_audit(s: &mut MySqlSession, clock: &Clock) -> std::result::Result<(History, ProfilerStarted), String> {
    let history = History::new(Source::SqlAudit, clock.now());
    match s.rows(&history_sql(Source::SqlAudit, clock.now(), "")).await {
        Ok(_) => Ok((history, ProfilerStarted::new(ProfilerMode::Complete, "GV$OB_SQL_AUDIT"))),
        Err(e) => Err(format!(
            "No se puede leer GV$OB_SQL_AUDIT ({e}; ob_enable_sql_audit debe estar activado): se muestrean las consultas en curso."
        )),
    }
}

// ------------------------------------------------------------- sampled

/// What each connection runs now, and the source's name.
fn sample_sql(v: Variant) -> (String, &'static str) {
    let (sql, source) = match v {
        Variant::TiDb => (
            "SELECT * FROM information_schema.CLUSTER_PROCESSLIST WHERE ID <> CONNECTION_ID()",
            "information_schema.CLUSTER_PROCESSLIST",
        ),
        Variant::StarRocks | Variant::Doris => ("SHOW FULL PROCESSLIST", "SHOW FULL PROCESSLIST"),
        Variant::Databend => ("SELECT * FROM system.processes", "system.processes"),
        Variant::Manticore => ("SHOW THREADS OPTION format=sphinxql", "SHOW THREADS"),
        Variant::GreptimeDb => (
            "SELECT id, schemas AS db, query AS info, client AS host, CAST(start_timestamp AS BIGINT) AS start_ms, \
                    CAST(elapsed_time AS BIGINT) AS elapsed_ms \
             FROM information_schema.process_list",
            "information_schema.process_list",
        ),
        _ => ("SELECT * FROM information_schema.PROCESSLIST WHERE ID <> CONNECTION_ID()", "information_schema.PROCESSLIST"),
    };
    (sql.to_string(), source)
}

impl Sampling {
    /// One look at the processlist.
    fn look(&mut self, rows: &[Row], now: i64, db: &str) -> Vec<Sample> {
        let mut samples: Vec<Sample> = Vec::new();
        for r in rows {
            // Manticore lists each worker thread of a search: one per
            // connection.
            if let Some(s) = self.sample(r, now, db).filter(|s| samples.iter().all(|o| o.session != s.session)) {
                samples.push(s);
            }
        }
        // A connection that went idle starts afresh next time.
        self.seen.retain(|k, _| samples.iter().any(|s| s.session == *k));
        samples
    }

    /// One processlist row as a sample; `None` for idle connections, this
    /// one, and other databases.
    fn sample(&mut self, r: &Row, now: i64, db: &str) -> Option<Sample> {
        let get = |names: &[&str]| named(r, names).filter(|v| !v.is_empty());
        let mut session = get(&["ID", "ConnID"])?;
        if let Some(instance) = get(&["INSTANCE"]) {
            session = format!("{instance}/{session}");
        }
        let text = manticore_text(&get(&["INFO", "extra_info"])?);
        if session == self.own || text.trim() == self.sql.trim() {
            return None;
        }
        let command = get(&["COMMAND"]).unwrap_or_default().to_ascii_lowercase();
        if matches!(command.as_str(), "sleep" | "daemon" | "connect" | "killed" | "binlog dump" | "binlog dump gtid") {
            return None;
        }
        let database = get(&["DB", "database"]);
        if !db.is_empty() {
            // Old Doris prefixes the cluster: "default_cluster:db".
            let d = database.as_deref().map(|d| d.rsplit(':').next().unwrap_or(d));
            if !d.is_some_and(|d| d.eq_ignore_ascii_case(db)) {
                return None;
            }
        }
        // The start, exact when the engine gives it; else from the elapsed
        // time, whole seconds on most engines.
        let num = |names: &[&str]| get(names).and_then(|v| v.parse::<f64>().ok());
        let (exact, elapsed_ms, coarse) = if let Some(ms) = num(&["start_ms"]) {
            (Some(ms as i64 * 1000), num(&["elapsed_ms"]).unwrap_or(0.0), false)
        } else if let Some(ms) = num(&["TIME_MS"]) {
            (None, ms, false)
        } else if get(&["This/prev job time"]).is_some() {
            // Manticore's is the thread's current job, not the search: the
            // search started when first seen.
            (None, 0.0, false)
        } else {
            (None, num(&["TIME", "time"]).unwrap_or(0.0) * 1000.0, true)
        };
        let computed = exact.unwrap_or(now - (elapsed_ms * 1000.0) as i64);
        // One statement from the next on the same connection: the query id
        // where there is one, else the text.
        let key = format!("{}\n{text}", get(&["QUERY_ID", "QueryId", "query_id", "TxnStart"]).unwrap_or_default());
        let tolerance = match (exact, coarse) {
            (Some(_), _) => 0,
            _ if self.sql.starts_with("SHOW THREADS") => i64::MAX,
            (None, true) => 1_200_000,
            (None, false) => 50_000,
        };
        let same = self.seen.get(&session).is_some_and(|s| s.key == key && (computed - s.started).abs() <= tolerance);
        if !same {
            self.seen.insert(session.clone(), Seen { key, started: computed, earliest: computed });
        }
        let seen = self.seen.get_mut(&session)?;
        seen.earliest = seen.earliest.min(computed);
        let duration_ms = if exact.is_some() { elapsed_ms } else { ((now - seen.earliest) as f64 / 1000.0).max(elapsed_ms) };
        let state = get(&["STATE", "State"]).filter(|s| !s.eq_ignore_ascii_case("ok"));
        Some(Sample {
            session,
            started: utc(seen.started),
            text,
            running: true,
            duration_ms: Some(duration_ms),
            database,
            user: get(&["USER"]),
            client: get(&["HOST", "Connection from"]),
            rows: num(&["SENT_ROWS"]).map(|n| n as u64),
            error: None,
            detail: state.map(|s| format!("estado: {s}")),
            application: None,
            // Databend's system.processes: what the query read and wrote so far.
            reads: num(&["data_read_bytes"]).map(|n| n as u64),
            writes: num(&["data_write_bytes"]).map(|n| n as u64),
            ..Default::default()
        })
    }
}

/// The statement in a Manticore worker thread's info: `6 ch 0: api-search
/// query="" comment="" index="t" SELECT …` → `SELECT …`.
fn manticore_text(info: &str) -> String {
    let Some(i) = info.find("api-search ") else { return info.to_string() };
    let rest = &info[i..];
    match rest.find("index=\"").and_then(|j| rest[j + 7..].find('"').map(|k| j + 7 + k + 1)) {
        Some(end) => rest[end..].trim_start().to_string(),
        None => info.to_string(),
    }
}

/// Epoch microseconds as `YYYY-MM-DD HH:MM:SS.mmm`, UTC.
fn utc(us: i64) -> String {
    let ms = us.div_euclid(1000);
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}", rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_to_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00:00.000");
        assert_eq!(utc(1_700_000_000_123_456), "2023-11-14 22:13:20.123");
        assert_eq!(utc(951_782_400_000_000), "2000-02-29 00:00:00.000");
    }

    #[test]
    fn history_reads_cpu_where_the_table_has_it() {
        let ps = |cpu| Source::Ps { table: "events_statements_history_long", own: 1, timer: 0, epoch_us: 0, cpu };
        let with = history_sql(ps(true), 0, "");
        assert!(with.contains("e.CPU_TIME / 1000000000 AS cpu_ms") && with.contains("e.ROWS_EXAMINED AS read_n"));
        let without = history_sql(ps(false), 0, "");
        assert!(without.contains("NULL AS cpu_ms") && !without.contains("CPU_TIME"));
        let tidb = history_sql(Source::SlowLog, 0, "");
        assert!(tidb.contains("Process_keys AS read_n, Write_keys AS write_n"));
    }

    #[test]
    fn manticore_worker_info() {
        assert_eq!(manticore_text(r#"6 ch 0: api-search query="" comment="" index="t" SELECT 1 FROM t"#), "SELECT 1 FROM t");
        assert_eq!(manticore_text("SELECT 1"), "SELECT 1");
    }
}

//! The profiler ([`dbine_driver::profiler`]) for ClickHouse and Timeplus
//! Proton: `system.query_log`, complete. The server writes it in batches
//! (every 7.5 s by default); each poll asks for `SYSTEM FLUSH LOGS` first,
//! and without that privilege reads back far enough to catch a late batch.
//! Where the log can't be read, `system.processes` is sampled.
//!
//! The profiler's own requests carry a `query_id` with [`OWN`] as prefix,
//! which leaves them out of both.

use crate::{body_exception, server_error, ClickHouseSession};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::time::Instant;

const OWN: &str = "dbine-profiler-";
/// Rows read from the log per poll.
const BATCH: usize = 2000;
/// How far back each poll reads again (µs): a statement reaches the log a
/// little after it ends, or up to a flush interval later without flushing.
const MARGIN_FLUSHED: i64 = 2_000_000;
const MARGIN_UNFLUSHED: i64 = 10_000_000;
/// CPU time (user + system), ms, from a row's `ProfileEvents`.
const CPU_MS: &str =
    "(ProfileEvents['UserTimeMicroseconds'] + ProfileEvents['SystemTimeMicroseconds']) / 1000 AS cpu_ms";

pub(crate) enum State {
    Log(Log),
    Sampled(Sampled),
}

pub(crate) struct Log {
    database: String,
    /// Ends (epoch µs) read up to, and the rows read within the margin.
    after: i64,
    seen: HashMap<String, i64>,
    /// `SYSTEM FLUSH LOGS` works.
    flush: bool,
}

pub(crate) struct Sampled {
    database: String,
    sampler: Sampler,
    /// Each running query's start, fixed at first sight.
    starts: HashMap<String, i64>,
}

type Record = Map<String, Value>;

impl ClickHouseSession {
    /// One profiler request: rows as JSON objects.
    async fn profiler_rows(&self, sql: &str) -> Result<Vec<Record>> {
        let query_id = format!("{OWN}{}", uuid::Uuid::new_v4());
        let mut q = vec![("database", self.database.as_str()), ("query_id", query_id.as_str())];
        if self.sends_readonly() {
            q.push(("readonly", "1"));
        }
        let resp = self.conn.post().query(&q).body(sql.to_string()).send().await.map_err(|e| Error::Query(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Error::Query(e.to_string()))?;
        if !status.is_success() {
            return Err(server_error(status.as_u16(), text.trim()));
        }
        let mut out = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<Value>(line) {
                Ok(Value::Object(m)) => out.push(m),
                _ => return Err(body_exception(line).unwrap_or_else(|| Error::Query(line.to_string()))),
            }
        }
        Ok(out)
    }

    /// The server's clock, epoch µs.
    async fn profiler_now(&self) -> Result<i64> {
        let rows = self.profiler_rows("SELECT toUnixTimestamp64Micro(now64(6)) AS t FORMAT JSONEachRow").await?;
        rows.first().and_then(|r| num(r, "t")).map(|t| t as i64).ok_or_else(|| Error::Query("sin hora del servidor".into()))
    }

    pub(crate) async fn profiler_begin(&self, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
        let now = self.profiler_now().await?;
        let mut log = Log { database: opts.database.clone(), after: now, seen: HashMap::new(), flush: true };
        self.flush_logs(&mut log).await;
        match self.profiler_rows(&log_sql(&log)).await {
            Ok(_) => {
                let mut started =
                    ProfilerStarted::new(ProfilerMode::Complete, "system.query_log").units(Some("filas"), Some("filas"));
                if !log.flush {
                    started = started.note(
                        "Sin permiso para SYSTEM FLUSH LOGS: las consultas aparecen cuando el servidor vuelca el registro \
                         (cada 7,5 s por defecto).",
                    );
                }
                let logged = self.profiler_rows("SELECT getSetting('log_queries') AS v FORMAT JSONEachRow").await;
                if logged.ok().and_then(|r| r.first().and_then(|r| num(r, "v"))) == Some(0.0) {
                    let note = started.note.take().map(|n| format!("{n} ")).unwrap_or_default();
                    started = started.note(format!(
                        "{note}log_queries está desactivado para este usuario: las consultas de los usuarios con ese perfil no se registran."
                    ));
                }
                Ok((State::Log(log), started))
            }
            Err(e) => {
                let sampled = Sampled { database: opts.database.clone(), sampler: Sampler::new(utc(now)), starts: HashMap::new() };
                self.profiler_rows(&processes_sql(&sampled.database)).await?;
                Ok((
                    State::Sampled(sampled),
                    ProfilerStarted::new(ProfilerMode::Sampled, "system.processes")
                        .units(Some("filas"), Some("filas"))
                        .note(format!("No se puede leer system.query_log ({e}): se muestrean las consultas en curso.")),
                ))
            }
        }
    }

    pub(crate) async fn profiler_next(&self, state: &mut State) -> Result<Vec<ProfiledStatement>> {
        let mut out = match state {
            State::Log(log) => {
                self.flush_logs(log).await;
                let rows = self.profiler_rows(&log_sql(log)).await?;
                log.read(&rows)
            }
            State::Sampled(p) => {
                let mut out = Vec::new();
                let until = Instant::now() + SAMPLE_FOR;
                loop {
                    let rows = self.profiler_rows(&processes_sql(&p.database)).await?;
                    let samples = p.look(&rows);
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

    /// Flush the query log, while it works (it takes the SYSTEM FLUSH LOGS
    /// privilege).
    async fn flush_logs(&self, log: &mut Log) {
        if !log.flush {
            return;
        }
        // Only the query log where the server allows naming it.
        for sql in ["SYSTEM FLUSH LOGS query_log", "SYSTEM FLUSH LOGS"] {
            if self.profiler_rows(sql).await.is_ok() {
                return;
            }
        }
        log.flush = false;
    }
}

impl Log {
    fn margin(&self) -> i64 {
        if self.flush {
            MARGIN_FLUSHED
        } else {
            MARGIN_UNFLUSHED
        }
    }

    fn read(&mut self, rows: &[Record]) -> Vec<ProfiledStatement> {
        let mut out = Vec::new();
        for r in rows {
            let (Some(id), Some(end)) = (text(r, "id"), num(r, "end")) else { continue };
            let end = end as i64;
            if self.seen.insert(id, end).is_some() {
                continue;
            }
            self.after = self.after.max(end);
            let query = text(r, "query").unwrap_or_default();
            if query.trim().is_empty() {
                continue;
            }
            let written = num(r, "written").unwrap_or(0.0);
            let rows = if written > 0.0 { Some(written as u64) } else { num(r, "result").map(|n| n as u64) };
            out.push(ProfiledStatement {
                time: utc(num(r, "start").unwrap_or(0.0) as i64),
                duration_ms: num(r, "ms"),
                text: query.trim().to_string(),
                database: text(r, "db"),
                user: text(r, "usr"),
                client: client(r),
                rows,
                error: text(r, "error"),
                detail: text(r, "detail"),
                application: application(r),
                cpu_ms: num(r, "cpu_ms"),
                reads: count(r, "read"),
                writes: count(r, "written"),
            });
        }
        let floor = self.after - self.margin();
        self.seen.retain(|_, end| *end >= floor);
        out
    }
}

/// Finished statements (and failed ones) that ended after the margin,
/// oldest first.
fn log_sql(log: &Log) -> String {
    let from = log.after - log.margin();
    format!(
        "SELECT concat(query_id, '/', toString(type)) AS id, toUnixTimestamp64Micro(event_time_microseconds) AS end, \
                toUnixTimestamp64Micro(query_start_time_microseconds) AS start, query_duration_ms AS ms, query, \
                current_database AS db, user AS usr, toString(address) AS address, client_name, http_user_agent, \
                result_rows AS result, read_rows AS read, written_rows AS written, {CPU_MS}, exception AS error, \
                concat('leído: ', formatReadableSize(read_bytes), '; memoria: ', formatReadableSize(memory_usage)) AS detail \
         FROM system.query_log \
         WHERE event_date >= toDate(fromUnixTimestamp64Micro({from})) - 1 \
           AND event_time_microseconds >= fromUnixTimestamp64Micro({from}) \
           AND type != 'QueryStart' AND is_initial_query AND NOT startsWith(query_id, '{OWN}'){} \
         ORDER BY event_time_microseconds LIMIT {BATCH} FORMAT JSONEachRow",
        in_database(&log.database, true)
    )
}

fn processes_sql(database: &str) -> String {
    format!(
        "SELECT query_id AS id, elapsed, toUnixTimestamp64Micro(now64(6)) AS now, query, current_database AS db, \
                user AS usr, toString(address) AS address, client_name, http_user_agent, \
                read_rows AS read, written_rows AS written, {CPU_MS}, \
                concat('leído: ', formatReadableSize(read_bytes), '; memoria: ', formatReadableSize(memory_usage)) AS detail \
         FROM system.processes \
         WHERE is_initial_query AND NOT startsWith(query_id, '{OWN}'){} FORMAT JSONEachRow",
        in_database(database, false)
    )
}

/// Statements run from the database (or, in the log, touching it).
fn in_database(database: &str, touched: bool) -> String {
    if database.is_empty() {
        return String::new();
    }
    let db = format!("'{}'", database.replace('\\', "\\\\").replace('\'', "\\'"));
    if touched {
        format!(" AND (current_database = {db} OR has(databases, {db}))")
    } else {
        format!(" AND current_database = {db}")
    }
}

impl Sampled {
    fn look(&mut self, rows: &[Record]) -> Vec<Sample> {
        let mut out = Vec::new();
        for r in rows {
            let Some(id) = text(r, "id") else { continue };
            let now = num(r, "now").unwrap_or(0.0) as i64;
            let elapsed = num(r, "elapsed").unwrap_or(0.0);
            let start = *self.starts.entry(id.clone()).or_insert(now - (elapsed * 1e6) as i64);
            out.push(Sample {
                session: id,
                started: utc(start),
                text: text(r, "query").unwrap_or_default(),
                running: true,
                duration_ms: Some(elapsed * 1000.0),
                database: text(r, "db"),
                user: text(r, "usr"),
                client: client(r),
                application: application(r),
                detail: text(r, "detail"),
                cpu_ms: num(r, "cpu_ms"),
                reads: count(r, "read"),
                writes: count(r, "written"),
                ..Default::default()
            });
        }
        self.starts.retain(|id, _| out.iter().any(|s| s.session == *id));
        out
    }
}

/// The client's address.
fn client(r: &Record) -> Option<String> {
    text(r, "address").map(|a| a.trim_start_matches("::ffff:").to_string())
}

/// `client_name` (native clients) or the HTTP user agent.
fn application(r: &Record) -> Option<String> {
    text(r, "client_name").or_else(|| text(r, "http_user_agent"))
}

fn text(r: &Record, k: &str) -> Option<String> {
    match r.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Numbers come quoted when they're 64-bit.
fn num(r: &Record, k: &str) -> Option<f64> {
    match r.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn count(r: &Record, k: &str) -> Option<u64> {
    num(r, k).map(|n| n.max(0.0) as u64)
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
        assert_eq!(utc(1_700_000_000_123_456), "2023-11-14 22:13:20.123");
    }

    #[test]
    fn log_rows_carry_cpu_reads_and_writes() {
        let row: Record = serde_json::from_str(
            r#"{"id":"q/2","end":"1700000000223456","start":"1700000000123456","ms":"100","query":"INSERT INTO t SELECT 1",
                "result":"0","read":"1","written":"5","cpu_ms":2.5,"detail":"leído: 1.00 B; memoria: 4.00 KiB"}"#,
        )
        .unwrap();
        let mut log = Log { database: String::new(), after: 0, seen: HashMap::new(), flush: true };
        let st = log.read(&[row]).remove(0);
        assert_eq!((st.cpu_ms, st.reads, st.writes, st.rows), (Some(2.5), Some(1), Some(5), Some(5)));
        assert_eq!(st.time, "2023-11-14 22:13:20.123");
        assert!(log_sql(&log).contains("ProfileEvents['UserTimeMicroseconds']"));
        assert!(processes_sql("").contains("written_rows AS written"));
    }

    #[test]
    fn database_filter_escapes() {
        assert_eq!(in_database("", true), "");
        assert_eq!(in_database("a'b", true), " AND (current_database = 'a\\'b' OR has(databases, 'a\\'b'))");
        assert_eq!(in_database("x", false), " AND current_database = 'x'");
    }
}

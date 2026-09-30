//! The profiler ([`dbine_driver::profiler`]) for Snowflake: complete, from
//! the `INFORMATION_SCHEMA.QUERY_HISTORY` table function (near real time,
//! the last 7 days). Every read asks for the statements that finished since
//! the newest one seen, going back [`OVERLAP_MS`] because the history can
//! show a statement a little late; ids already reported are dropped.
//!
//! Nothing is switched on. The history shows the current user's statements,
//! and everyone's on the warehouses where the role has MONITOR or OPERATE.
//! The table function needs a running warehouse: while profiling, reading
//! it every [`EVERY`] keeps the session's warehouse awake.
//!
//! The profiler's own statements carry the QUERY_TAG [`TAG`] and are left
//! out.

use crate::monitor::Set;
use crate::SnowflakeSession;
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// QUERY_TAG of the profiler's own statements.
pub const TAG: &str = "dbine-profiler";
/// How far back each read goes before the newest statement seen.
const OVERLAP_MS: i64 = 30_000;
/// How often the history is read (each read is a query on the warehouse).
const EVERY: Duration = Duration::from_secs(3);
/// The table function's most rows per call.
const LIMIT: usize = 10_000;

pub struct State {
    /// The database watched (empty: all).
    database: String,
    seen: Seen,
    next: Instant,
}

/// Which statements were already reported, by query id, with their end
/// time (epoch ms).
#[derive(Debug)]
struct Seen {
    /// Statements that finished before this were there before profiling.
    since: i64,
    /// The newest end time seen.
    mark: i64,
    ids: HashMap<String, i64>,
}

impl Seen {
    fn new(now: i64) -> Self {
        Self { since: now, mark: now, ids: HashMap::new() }
    }

    /// Where the next read starts.
    fn from(&self) -> i64 {
        self.mark - OVERLAP_MS
    }

    /// True the first time `id` is seen (and it finished after profiling
    /// began).
    fn first(&mut self, id: &str, at: i64) -> bool {
        if at < self.since || self.ids.contains_key(id) {
            return false;
        }
        self.ids.insert(id.to_string(), at);
        self.mark = self.mark.max(at);
        true
    }

    /// Forget what the next read can't return any more.
    fn prune(&mut self) {
        let from = self.from();
        self.ids.retain(|_, at| *at >= from);
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// The statements that finished from `from` (epoch ms) on, oldest first.
fn history_sql(database: &str, from: i64) -> String {
    let (schema, filter) = if database.is_empty() {
        ("SNOWFLAKE.INFORMATION_SCHEMA".to_string(), String::new())
    } else {
        (format!("{}.INFORMATION_SCHEMA", quote(database)), format!(" AND database_name = {}", lit(database)))
    };
    format!(
        "SELECT query_id, TO_CHAR(CONVERT_TIMEZONE('UTC', start_time), 'YYYY-MM-DD HH24:MI:SS.FF3') AS time, \
                DATE_PART(EPOCH_MILLISECOND, end_time) AS end_ms, total_elapsed_time AS ms, query_text, \
                database_name, user_name, role_name, warehouse_name, query_type, execution_status, error_message, \
                rows_produced, bytes_scanned, bytes_written \
         FROM TABLE({schema}.QUERY_HISTORY(END_TIME_RANGE_START => TO_TIMESTAMP_LTZ({from}, 3), \
                                           END_TIME_RANGE_END => CURRENT_TIMESTAMP(), RESULT_LIMIT => {LIMIT})) \
         WHERE COALESCE(query_tag, '') <> '{TAG}'{filter} \
         ORDER BY end_time"
    )
}

fn get<'a>(set: &'a Set, row: &'a [Option<String>], name: &str) -> Option<&'a str> {
    let i = set.cols.iter().position(|c| c == name)?;
    row.get(i)?.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// New statements of a history read.
fn statements(set: &Set, seen: &mut Seen) -> Vec<ProfiledStatement> {
    let mut out = Vec::new();
    for row in &set.rows {
        let g = |name: &str| get(set, row, name);
        let (Some(id), Some(time), Some(end)) = (g("query_id"), g("time"), g("end_ms").and_then(|v| v.parse::<f64>().ok())) else {
            continue;
        };
        if !seen.first(id, end as i64) {
            continue;
        }
        let Some(text) = g("query_text") else { continue };
        let mut detail: Vec<String> = [g("query_type"), g("warehouse_name")].into_iter().flatten().map(str::to_string).collect();
        let bytes = |name: &str| g(name).and_then(|v| v.parse::<f64>().ok()).map(|b| b.max(0.0) as u64);
        if let Some(r) = g("role_name") {
            detail.push(format!("rol {r}"));
        }
        let failed = g("execution_status").is_some_and(|s| s.to_ascii_uppercase().starts_with("FAIL"));
        out.push(ProfiledStatement {
            time: time.to_string(),
            duration_ms: g("ms").and_then(|v| v.parse().ok()),
            text: text.to_string(),
            database: g("database_name").map(str::to_string),
            user: g("user_name").map(str::to_string),
            client: None,
            rows: g("rows_produced").and_then(|v| v.parse::<f64>().ok()).map(|v| v as u64),
            error: g("error_message").map(str::to_string).or_else(|| failed.then(|| "falló".to_string())),
            detail: (!detail.is_empty()).then(|| detail.join(" · ")),
            application: None,
            // Snowflake reports no CPU time per query.
            cpu_ms: None,
            reads: bytes("bytes_scanned"),
            writes: bytes("bytes_written"),
        });
    }
    seen.prune();
    out.sort_by(|a, b| a.time.cmp(&b.time));
    out
}

pub async fn start(s: &SnowflakeSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let database = if opts.database.is_empty() { s.ctx.database.clone().unwrap_or_default() } else { opts.database.clone() };
    let now = s.tagged_rows("SELECT DATE_PART(EPOCH_MILLISECOND, CURRENT_TIMESTAMP()) AS now", TAG).await?;
    let now = now
        .rows
        .first()
        .and_then(|r| get(&now, r, "now"))
        .and_then(|v| v.parse::<f64>().ok())
        .ok_or_else(|| Error::Query("Snowflake no devolvió la hora del servidor".into()))? as i64;
    let seen = Seen::new(now);
    // A first read proves the warehouse and the privileges.
    s.tagged_rows(&history_sql(&database, seen.from()), TAG).await.map_err(|e| {
        Error::Query(format!("no se pudo leer INFORMATION_SCHEMA.QUERY_HISTORY (necesita un warehouse en marcha): {e}"))
    })?;
    let started = ProfilerStarted::new(ProfilerMode::Complete, "INFORMATION_SCHEMA.QUERY_HISTORY")
        .units(Some("bytes"), Some("bytes"))
        .note(
            "Se ven las consultas del usuario actual, y las de todos en los warehouses donde el rol tiene MONITOR u OPERATE. \
             Aparecen al terminar, con unos segundos de demora; leer el historial mantiene encendido el warehouse de la sesión.",
        );
    Ok((State { database, seen, next: Instant::now() }, started))
}

pub async fn poll(s: &SnowflakeSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    if Instant::now() < state.next {
        return Ok(Vec::new());
    }
    state.next = Instant::now() + EVERY;
    let set = s.tagged_rows(&history_sql(&state.database, state.seen.from()), TAG).await?;
    Ok(statements(&set, &mut state.seen))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(rows: &[(&str, &str, &str)]) -> Set {
        Set {
            cols: ["query_id", "time", "end_ms", "ms", "query_text", "database_name", "user_name", "query_type", "execution_status", "error_message", "rows_produced", "bytes_scanned", "bytes_written"]
                .map(String::from)
                .to_vec(),
            rows: rows
                .iter()
                .map(|(id, end, status)| {
                    [
                        Some(id.to_string()),
                        Some("2024-01-31 10:00:00.123".into()),
                        Some(end.to_string()),
                        Some("42".into()),
                        Some(format!("SELECT {id}")),
                        Some("DB".into()),
                        Some("JDOE".into()),
                        Some("SELECT".into()),
                        Some(status.to_string()),
                        (*status == "FAILED_WITH_ERROR").then(|| "boom".to_string()),
                        Some("7".into()),
                        Some("2048".into()),
                        Some("0".into()),
                    ]
                    .to_vec()
                })
                .collect(),
        }
    }

    #[test]
    fn sql_scopes_and_skips_its_own() {
        let sql = history_sql("My\"Db", 1_700_000_000_000);
        assert!(sql.contains("FROM TABLE(\"My\"\"Db\".INFORMATION_SCHEMA.QUERY_HISTORY("));
        assert!(sql.contains("TO_TIMESTAMP_LTZ(1700000000000, 3)"));
        assert!(sql.contains("database_name = 'My\"Db'"));
        assert!(sql.contains("<> 'dbine-profiler'"));
        assert!(history_sql("", 0).contains("SNOWFLAKE.INFORMATION_SCHEMA.QUERY_HISTORY"));
    }

    #[test]
    fn history_rows_are_reported_once() {
        let mut seen = Seen::new(1_000_000);
        // Finished before profiling began: not reported.
        assert!(statements(&set(&[("old", "999000", "SUCCESS")]), &mut seen).is_empty());
        let out = statements(&set(&[("a", "1001000", "SUCCESS"), ("b", "1002000", "FAILED_WITH_ERROR")]), &mut seen);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].duration_ms, out[0].rows, out[0].user.as_deref()), (Some(42.0), Some(7), Some("JDOE")));
        assert_eq!((out[0].reads, out[0].writes, out[0].cpu_ms), (Some(2048), Some(0), None));
        assert!(!out[0].detail.as_deref().unwrap_or_default().contains("bytes"));
        assert_eq!(out[1].error.as_deref(), Some("boom"));
        assert_eq!(seen.from(), 1_002_000 - OVERLAP_MS);
        // The overlap brings them again, with a late one: only that is new.
        let out = statements(&set(&[("a", "1001000", "SUCCESS"), ("c", "1001500", "SUCCESS")]), &mut seen);
        assert_eq!(out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["SELECT c"]);
    }
}

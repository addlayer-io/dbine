//! The profiler ([`dbine_driver::profiler`]): `MONITOR`, complete, on
//! Redis, Valkey, Dragonfly and the servers that keep the command (KeyDB,
//! most managed services; some, like ElastiCache Serverless, disable it and
//! then the start fails with the server's answer).
//!
//! A dedicated connection runs `MONITOR` in a thread of its own: each line
//! (`<unix.micros> [<db> <addr>] "CMD" "arg"…`) is parsed and kept until the
//! next poll. `MONITOR` changes no setting (so it runs on read-only
//! connections too) but it has a cost on a busy server, and it says nothing
//! of durations, results, errors, CPU time or keys read and written. The profiler's own session is left out
//! by its address; at stop, `CLIENT KILL ID` closes the monitoring
//! connection.

use crate::command::{parse_line, quote_arg};
use crate::{db_index, err, shape, RedisSession};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use redis::aio::MultiplexedConnection;
use redis::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Statements kept between two polls, at most (the oldest go first).
const KEEP: usize = 100_000;

#[derive(Default)]
struct Shared {
    seen: VecDeque<ProfiledStatement>,
    /// Why the monitoring connection ended, if it did.
    error: Option<String>,
}

pub(crate) struct State {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    /// The monitoring connection's `CLIENT ID`, to close it at stop.
    id: Option<i64>,
    conn: MultiplexedConnection,
}

impl Drop for State {
    /// Dropped without `stop` (the session went away): close the
    /// monitoring connection all the same.
    fn drop(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        if let (Some(id), Ok(rt)) = (self.id, tokio::runtime::Handle::try_current()) {
            let mut conn = self.conn.clone();
            rt.spawn(async move {
                let _: redis::RedisResult<Value> =
                    redis::cmd("CLIENT").arg("KILL").arg("ID").arg(id).query_async(&mut conn).await;
            });
        }
    }
}

pub(crate) async fn start(s: &mut RedisSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let db = if opts.database.trim().is_empty() { None } else { Some(db_index(&opts.database)?) };
    // This session's address, to leave out its own commands.
    let own = match s.run(&[b"CLIENT", b"INFO"]).await {
        Ok(v) => field(&shape::text_of(&v), "addr"),
        Err(_) => None,
    };
    let client = s.client.clone();
    let (mut conn, id) = tokio::task::spawn_blocking(move || -> redis::RedisResult<_> {
        let mut c = client.get_connection_with_timeout(Duration::from_secs(15))?;
        let id: Option<i64> = redis::cmd("CLIENT").arg("ID").query(&mut c).ok();
        c.send_packed_command(&redis::cmd("MONITOR").get_packed_command())?;
        c.recv_response()?.extract_error()?;
        Ok((c, id))
    })
    .await
    .map_err(|e| Error::Query(e.to_string()))?
    .map_err(|e| match err(e) {
        Error::Query(m) => Error::Query(format!("el servidor no permite MONITOR: {m}")),
        other => other,
    })?;
    let shared = Arc::new(Mutex::new(Shared::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (sh, st) = (shared.clone(), stop.clone());
    std::thread::spawn(move || {
        let error = loop {
            let v = conn.recv_response();
            if st.load(Ordering::SeqCst) {
                break None;
            }
            let line = match v {
                Ok(Value::SimpleString(l)) => l,
                Ok(Value::BulkString(b)) => String::from_utf8_lossy(&b).into_owned(),
                Ok(_) => continue,
                Err(e) => break Some(e.to_string()),
            };
            let Some(stmt) = parse(&line, db, own.as_deref()) else { continue };
            let mut sh = sh.lock().unwrap_or_else(|e| e.into_inner());
            if sh.seen.len() == KEEP {
                sh.seen.pop_front();
            }
            sh.seen.push_back(stmt);
        };
        sh.lock().unwrap_or_else(|e| e.into_inner()).error = error;
    });
    let state = State { shared, stop, id, conn: s.conn.clone() };
    let started = ProfilerStarted::new(ProfilerMode::Complete, "MONITOR").note(
        "MONITOR tiene un costo en servidores con mucho tráfico y no informa la duración ni el resultado de cada comando.",
    );
    Ok((state, started))
}

pub(crate) fn poll(state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let mut sh = state.shared.lock().unwrap_or_else(|e| e.into_inner());
    if sh.seen.is_empty() {
        if let Some(e) = &sh.error {
            return Err(Error::Connect(format!("se cortó la conexión de MONITOR: {e}")));
        }
    }
    Ok(sh.seen.drain(..).collect())
}

/// Close the monitoring connection.
pub(crate) async fn stop(mut state: State) -> Result<()> {
    state.stop.store(true, Ordering::SeqCst);
    if let Some(id) = state.id {
        // Without CLIENT KILL (a restricted ACL), the thread ends with the
        // next command the server sees.
        let _: redis::RedisResult<Value> =
            redis::cmd("CLIENT").arg("KILL").arg("ID").arg(id).query_async(&mut state.conn).await;
    }
    Ok(())
}

/// `key=value` out of a `CLIENT INFO` line.
fn field(info: &str, key: &str) -> Option<String> {
    info.split_whitespace().find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')).map(str::to_string)
}

/// One `MONITOR` line, unless it's from another database or from `own`.
fn parse(line: &str, db: Option<i64>, own: Option<&str>) -> Option<ProfiledStatement> {
    let (time, rest) = line.split_once(' ')?;
    let rest = rest.strip_prefix('[')?;
    let (source, args) = rest.split_once("] ")?;
    let (in_db, addr) = source.split_once(' ')?;
    let in_db: i64 = in_db.parse().ok()?;
    if db.is_some_and(|d| d != in_db) || own == Some(addr) {
        return None;
    }
    let args = parse_line(args).ok()?;
    if args.is_empty() {
        return None;
    }
    let text = args.iter().map(|a| quote_arg(&String::from_utf8_lossy(a))).collect::<Vec<_>>().join(" ");
    Some(ProfiledStatement {
        time: stamp(time)?,
        text,
        database: Some(format!("db{in_db}")),
        // Commands a Lua script runs show `lua` as the client.
        client: Some(if addr == "lua" { "lua (script)".to_string() } else { addr.to_string() }),
        ..Default::default()
    })
}

/// `1700000000.123456` (Unix seconds) as `YYYY-MM-DD HH:MM:SS.mmm` in UTC.
pub(crate) fn stamp(unix: &str) -> Option<String> {
    let (secs, frac) = unix.split_once('.').unwrap_or((unix, "0"));
    let secs: i64 = secs.parse().ok()?;
    let ms: String = frac.chars().chain(std::iter::repeat('0')).take(3).collect();
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some(format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{ms}", rem / 3600, rem % 3600 / 60, rem % 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_lines() {
        let s = parse(r#"1706708700.601523 [2 127.0.0.1:34520] "set" "a b" "x\"y z""#, None, None).unwrap();
        assert_eq!(s.time, "2024-01-31 13:45:00.601");
        assert_eq!(s.text, r#"set "a b" "x\"y z""#);
        assert_eq!((s.database.as_deref(), s.client.as_deref()), (Some("db2"), Some("127.0.0.1:34520")));
        // Dragonfly gives seven decimals.
        assert_eq!(parse(r#"1706708700.7797346 [0 127.0.0.1:1] "GET" "k""#, None, None).unwrap().time, "2024-01-31 13:45:00.779");
        assert!(parse(r#"1706708700.1 [2 127.0.0.1:1] "get" "k""#, Some(0), None).is_none());
        assert!(parse(r#"1706708700.1 [0 127.0.0.1:1] "get" "k""#, None, Some("127.0.0.1:1")).is_none());
        assert_eq!(parse(r#"1706708700.1 [0 lua] "get" "k""#, None, None).unwrap().client.as_deref(), Some("lua (script)"));
        assert_eq!(stamp("951782400.5").as_deref(), Some("2000-02-29 00:00:00.500"));
    }

    #[test]
    fn client_info_fields() {
        assert_eq!(field("id=6 addr=127.0.0.1:34548 laddr=127.0.0.1:6379", "addr").as_deref(), Some("127.0.0.1:34548"));
    }
}

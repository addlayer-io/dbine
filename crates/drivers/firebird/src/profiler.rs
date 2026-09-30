//! The profiler ([`dbine_driver::profiler`]), sampled from
//! `MON$STATEMENTS` / `MON$ATTACHMENTS`.
//!
//! The monitoring tables cover the attached database only, and show a
//! statement's start (`MON$TIMESTAMP`) only while it runs, so what finishes
//! between two looks goes unseen. Firebird's trace API would see every
//! statement, but it's a Services API session the wire client here doesn't
//! speak. Nothing is switched on.
//!
//! The profiler has an attachment of its own (the MON$ snapshot is taken
//! once per transaction, and the tab's transaction is left alone), with the
//! session time zone set to UTC (Firebird 4+). Users without SYSDBA,
//! RDB$ADMIN or MONITOR_ANY_ATTACHMENT see only their own user's
//! attachments.

use crate::monitor::in_snapshot;
use crate::{int, join_err, message, poisoned, text, Conn, FirebirdSession};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use rsfbclient_core::{Column, Dialect, FirebirdClientSqlOps};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(crate) struct State {
    conn: Arc<Mutex<Conn>>,
    sampler: Sampler,
    database: String,
}

/// The running statements of the other attachments.
const RUNNING: &str = "
SELECT CAST(s.MON$STATEMENT_ID AS VARCHAR(20)), CAST(CAST(s.MON$TIMESTAMP AS TIMESTAMP) AS VARCHAR(30)),
       CAST(SUBSTRING(s.MON$SQL_TEXT FROM 1 FOR 8000) AS VARCHAR(8000)),
       CAST(DATEDIFF(MILLISECOND FROM s.MON$TIMESTAMP TO CURRENT_TIMESTAMP) AS DOUBLE PRECISION),
       TRIM(a.MON$USER), TRIM(a.MON$REMOTE_PROCESS), TRIM(a.MON$REMOTE_ADDRESS),
       r.MON$RECORD_SEQ_READS + r.MON$RECORD_IDX_READS, io.MON$PAGE_FETCHES, io.MON$PAGE_MARKS
  FROM MON$STATEMENTS s
  JOIN MON$ATTACHMENTS a ON a.MON$ATTACHMENT_ID = s.MON$ATTACHMENT_ID
  LEFT JOIN MON$RECORD_STATS r ON r.MON$STAT_ID = s.MON$STAT_ID
  LEFT JOIN MON$IO_STATS io ON io.MON$STAT_ID = s.MON$STAT_ID
 WHERE s.MON$STATE = 1 AND s.MON$ATTACHMENT_ID <> CURRENT_CONNECTION AND a.MON$SYSTEM_FLAG = 0";

pub(crate) async fn start(s: &FirebirdSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let mut target = s.target.clone();
    if !opts.database.trim().is_empty() {
        target.attach.db_name = opts.database.trim().to_string();
    }
    let database = target.attach.db_name.clone();
    let (conn, since, utc, all) = tokio::task::spawn_blocking(move || -> Result<(Conn, String, bool, bool)> {
        let mut c = Conn::open(&target)?;
        let utc = c.client.exec_immediate(&mut c.db, &mut c.tr, Dialect::D3, "SET TIME ZONE 'UTC'").is_ok();
        let now = c.rows("SELECT CAST(CAST(CURRENT_TIMESTAMP AS TIMESTAMP) AS VARCHAR(30)) FROM RDB$DATABASE", vec![])?;
        let since = now.first().and_then(|r| r.first()).and_then(text).map(|t| millis(&t)).unwrap_or_default();
        // Firebird 4+ names the privilege; 3 has only SYSDBA and RDB$ADMIN.
        let all = c
            .rows("SELECT IIF(RDB$SYSTEM_PRIVILEGE(MONITOR_ANY_ATTACHMENT), 1, 0) FROM RDB$DATABASE", vec![])
            .or_else(|_| {
                c.rows(
                    "SELECT IIF(CURRENT_USER = 'SYSDBA' OR RDB$ROLE_IN_USE('RDB$ADMIN'), 1, 0) FROM RDB$DATABASE",
                    vec![],
                )
            })
            .ok()
            .and_then(|r| r.first().and_then(|r| r.first()).and_then(int))
            .is_none_or(|v| v == 1);
        Ok((c, since, utc, all))
    })
    .await
    .map_err(join_err)??;
    let mut notes = vec!["Firebird solo muestra las sentencias mientras se ejecutan: las muy rápidas pueden no verse.".to_string()];
    if !all {
        notes.push("Sin SYSDBA, RDB$ADMIN ni MONITOR_ANY_ATTACHMENT solo se ven las conexiones del mismo usuario.".into());
    }
    if !utc {
        notes.push("Las horas son las del servidor (Firebird 3 no convierte a UTC).".into());
    }
    // Reads are page fetches (logical reads) and writes page marks (pages
    // changed), as far as the statement got when last seen.
    let started = ProfilerStarted::new(ProfilerMode::Sampled, "MON$STATEMENTS")
        .units(Some("páginas"), Some("páginas"))
        .note(notes.join(" "));
    Ok((State { conn: Arc::new(Mutex::new(conn)), sampler: Sampler::new(since), database }, started))
}

pub(crate) async fn poll(state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let mut out = Vec::new();
    let until = Instant::now() + SAMPLE_FOR;
    loop {
        let conn = state.conn.clone();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<Column>>> {
            let mut c = conn.lock().map_err(|_| poisoned())?;
            let mut res = in_snapshot(&mut c, &[RUNNING]).map_err(|e| Error::Query(message(&e)))?;
            res.pop().unwrap_or_else(|| Ok(Vec::new())).map_err(Error::Query)
        })
        .await
        .map_err(join_err)??;
        out.extend(state.sampler.feed(rows.iter().map(|r| sample(r, &state.database)).collect()));
        if Instant::now() + SAMPLE_EVERY > until {
            break;
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

fn sample(r: &[Column], database: &str) -> Sample {
    let t = |i: usize| r.get(i).and_then(text).filter(|v| !v.is_empty());
    let n = |i: usize| r.get(i).and_then(int);
    let count = |i: usize| n(i).map(|v| v.max(0) as u64);
    Sample {
        session: t(0).unwrap_or_default(),
        started: t(1).map(|v| millis(&v)).unwrap_or_default(),
        text: t(2).unwrap_or_default(),
        running: true,
        duration_ms: r.get(3).and_then(|c| match c.value {
            rsfbclient_core::SqlType::Floating(v) => Some(v.max(0.0)),
            _ => None,
        }),
        database: Some(database.to_string()),
        user: t(4),
        client: t(6),
        application: t(5),
        detail: n(7).filter(|v| *v > 0).map(|v| format!("registros leídos {v}")),
        reads: count(8),
        writes: count(9),
        ..Default::default()
    }
}

/// Firebird's timestamps carry tenths of a millisecond.
fn millis(t: &str) -> String {
    match t.find('.') {
        Some(i) if t.len() > i + 4 => t[..i + 4].to_string(),
        _ => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_keep_milliseconds() {
        assert_eq!(millis("2026-09-27 21:13:46.1120"), "2026-09-27 21:13:46.112");
        assert_eq!(millis("2026-09-27 21:13:46"), "2026-09-27 21:13:46");
    }

    #[test]
    fn page_fetches_and_marks_are_reads_and_writes() {
        use rsfbclient_core::SqlType::{Floating, Integer, Text};
        let row: Vec<Column> = [
            Text("7".into()),
            Text("2026-09-27 21:13:46.1120".into()),
            Text("SELECT 1".into()),
            Floating(12.0),
            Text("SYSDBA".into()),
            Text("isql".into()),
            Text("127.0.0.1".into()),
            Integer(40),
            Integer(1200),
            Integer(3),
        ]
        .into_iter()
        .map(|value| Column { value, raw_type: 0, name: String::new() })
        .collect();
        let s = sample(&row, "db");
        assert_eq!((s.reads, s.writes, s.cpu_ms), (Some(1200), Some(3), None));
        assert_eq!(s.detail.as_deref(), Some("registros leídos 40"));
        assert_eq!(s.started, "2026-09-27 21:13:46.112");
    }
}

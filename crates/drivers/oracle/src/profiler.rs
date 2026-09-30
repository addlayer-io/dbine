//! The profiler ([`dbine_driver::profiler`]), sampled from `V$SESSION`
//! (Oracle Database and Autonomous Database alike).
//!
//! Each session shows the statement it runs (`SQL_ID`, `SQL_EXEC_ID`,
//! `SQL_EXEC_START`) and the one it ran before (`PREV_*`), so a look
//! yields up to two samples per session, keyed by the execution rather than
//! the session: a statement is reported when it shows up as a previous one
//! or when it's gone. Oracle keeps no per-execution history outside ASH
//! (Diagnostics Pack license), so nothing is switched on. Start times come
//! in whole seconds (`SQL_EXEC_START` is a DATE). The duration is the
//! cursor's average from `V$SQLSTATS` (exact for a statement run once, but
//! without idle waits such as a sleep), or the time the profiler saw it
//! running when that's longer.
//!
//! The "database" is a schema: sessions whose current schema it is.
//! Reading the V$ views needs SELECT_CATALOG_ROLE (or SELECT ANY
//! DICTIONARY).

use crate::{db_code, err, OracleSession};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use oracledb::Connection;
use std::collections::HashMap;
use std::time::Instant;

pub(crate) struct State {
    sampler: Sampler,
    sql: String,
    /// When each execution (the samples' key) was first seen running.
    running: HashMap<String, Instant>,
}

/// A DATE in the server's local time as UTC text, milliseconds included.
fn utc(col: &str) -> String {
    format!(
        "TO_CHAR(SYS_EXTRACT_UTC(FROM_TZ(CAST({col} AS TIMESTAMP), TO_CHAR(SYSTIMESTAMP, 'TZH:TZM'))), 'YYYY-MM-DD HH24:MI:SS.FF3')"
    )
}

/// Every column as text.
fn texts(c: &Connection, sql: &str) -> std::result::Result<Vec<Vec<Option<String>>>, oracledb::Error> {
    // Out of the statement cache: a statement whose parse failed stays
    // cached and the next run reports ORA-01003 instead of the real error.
    let stmt = c.statement(sql).map(|b| b.exclude_from_cache()).and_then(|b| b.build())?;
    let cursor = stmt.query(&[])?;
    let n = cursor.columns().len();
    let mut out = Vec::new();
    for row in cursor {
        let row = row?;
        out.push((0..n).map(|i| row.get::<Option<String>>(i).ok().flatten()).collect());
    }
    Ok(out)
}

pub(crate) async fn start(s: &mut OracleSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let sql = sample_sql(&opts.database);
    // Whole seconds, as the statements' start times.
    let since = s
        .run(move |c| {
            match texts(c, "SELECT 1 FROM v$session s, v$sqlstats q WHERE ROWNUM = 1") {
                Err(e) if matches!(db_code(&e), Some(942 | 1031 | 1039)) => {
                    return Err(Error::Query(
                        "Para ver las consultas de las demás sesiones el usuario necesita leer V$SESSION y V$SQLSTATS \
                         (SELECT_CATALOG_ROLE o SELECT ANY DICTIONARY)."
                            .into(),
                    ))
                }
                r => r.map_err(err)?,
            };
            let rows =
                texts(c, "SELECT TO_CHAR(SYS_EXTRACT_UTC(SYSTIMESTAMP), 'YYYY-MM-DD HH24:MI:SS') || '.000' FROM dual")
                    .map_err(err)?;
            Ok(rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()).unwrap_or_default())
        })
        .await?;
    let started = ProfilerStarted::new(ProfilerMode::Sampled, "V$SESSION / V$SQLSTATS").note(
        "Oracle solo muestra la sentencia en curso y la anterior de cada sesión: las muy rápidas pueden no verse. \
         La hora de inicio es al segundo; la duración, la CPU, las lecturas (buffer gets) y las escrituras \
         (escrituras directas) son la media del cursor en V$SQLSTATS.",
    )
    .units(Some("bloques"), Some("bloques"));
    Ok((State { sampler: Sampler::new(since), sql, running: HashMap::new() }, started))
}

pub(crate) async fn poll(s: &mut OracleSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let mut out = Vec::new();
    let until = Instant::now() + SAMPLE_FOR;
    loop {
        let sql = state.sql.clone();
        let rows = s.run(move |c| texts(c, &sql).map_err(err)).await?;
        let now = Instant::now();
        let samples = rows
            .into_iter()
            .map(sample)
            .map(|mut smp| {
                if smp.running {
                    let first = *state.running.entry(smp.session.clone()).or_insert(now);
                    smp.duration_ms = Some(now.duration_since(first).as_secs_f64() * 1000.0);
                }
                // The key rides in `detail` until `cursor_figures`.
                smp.detail = Some(smp.session.clone());
                smp
            })
            .collect();
        out.extend(state.sampler.feed(samples));
        if Instant::now() + SAMPLE_EVERY > until {
            break;
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
    for st in &mut out {
        // Seen running: the time it was seen, else unknown.
        let key = st.detail.take().unwrap_or_default();
        st.duration_ms = state.running.remove(&key).map(|_| st.duration_ms.unwrap_or(0.0));
        st.detail = key_sql_id(&key).map(|id| format!("sql_id {id}"));
    }
    if !out.is_empty() {
        cursor_figures(s, &mut out).await?;
    }
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

/// The duration and rows of each statement's cursor (per execution), from
/// `V$SQLSTATS`; the sample only knows the time seen running, in seconds.
async fn cursor_figures(s: &mut OracleSession, out: &mut [ProfiledStatement]) -> Result<()> {
    let ids: Vec<String> = out.iter().filter_map(|st| st.detail.as_deref().and_then(sql_id)).map(str::to_string).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let list = ids.iter().map(|i| format!("'{}'", i.replace('\'', ""))).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT sql_id, TO_CHAR(elapsed_time / 1000 / executions, 'TM9', 'NLS_NUMERIC_CHARACTERS=''.,'''), \
                TO_CHAR(ROUND(rows_processed / executions)), TO_CHAR(executions), \
                TO_CHAR(cpu_time / 1000 / executions, 'TM9', 'NLS_NUMERIC_CHARACTERS=''.,'''), \
                TO_CHAR(ROUND(buffer_gets / executions)), TO_CHAR(ROUND(direct_writes / executions)) \
           FROM v$sqlstats WHERE sql_id IN ({list}) AND executions > 0"
    );
    let rows = s.run(move |c| texts(c, &sql).map_err(err)).await?;
    let figures: HashMap<String, Figures> = rows.into_iter().filter_map(figures).collect();
    for st in out.iter_mut() {
        let Some(id) = st.detail.as_deref().and_then(sql_id).map(str::to_string) else { continue };
        let Some(f) = figures.get(&id) else { continue };
        apply(st, &id, f);
    }
    Ok(())
}

/// One cursor's figures per execution (CPU in ms; reads are buffer gets and
/// writes direct writes, in blocks).
#[derive(Debug, Default)]
struct Figures {
    ms: Option<f64>,
    rows: Option<u64>,
    execs: u64,
    cpu_ms: Option<f64>,
    reads: Option<u64>,
    writes: Option<u64>,
}

/// A `cursor_figures` row, as text: sql_id, ms, rows, executions, cpu ms,
/// buffer gets, direct writes.
fn figures(r: Vec<Option<String>>) -> Option<(String, Figures)> {
    let id = r.first()?.clone()?;
    let n = |i: usize| r.get(i).cloned().flatten();
    let f = Figures {
        ms: n(1).and_then(|v| v.parse().ok()),
        rows: n(2).and_then(|v| v.parse().ok()),
        execs: n(3)?.parse().ok()?,
        cpu_ms: n(4).and_then(|v| v.parse().ok()),
        reads: n(5).and_then(|v| v.parse().ok()),
        writes: n(6).and_then(|v| v.parse().ok()),
    };
    Some((id, f))
}

fn apply(st: &mut ProfiledStatement, id: &str, f: &Figures) {
    if let Some(ms) = f.ms {
        st.duration_ms = Some(st.duration_ms.map_or(ms, |seen| seen.max(ms)));
    }
    st.cpu_ms = f.cpu_ms;
    st.reads = f.reads;
    st.writes = f.writes;
    if f.execs == 1 {
        st.rows = f.rows;
    } else {
        st.detail = Some(format!("sql_id {id}, media de {} ejecuciones", f.execs));
    }
}

/// `sid,serial#:sql_id:exec_id` → `sql_id`.
fn key_sql_id(key: &str) -> Option<&str> {
    key.split(':').nth(1).filter(|id| !id.is_empty())
}

fn sql_id(detail: &str) -> Option<&str> {
    detail.strip_prefix("sql_id ").map(|d| d.split(',').next().unwrap_or(d))
}

/// Per session other than this one, the running statement and the previous
/// one: key, start, text, running, ms, schema, user, client, application.
fn sample_sql(schema: &str) -> String {
    let scope = if schema.is_empty() {
        String::new()
    } else {
        format!(" AND s.schemaname = '{}'", schema.replace('\'', "''"))
    };
    let common = "s.schemaname, s.username, \
                  LTRIM(s.machine || NVL2(s.osuser, ' (' || s.osuser || ')', '')), s.program";
    let text = |id: &str, child: &str| {
        format!(
            "(SELECT DBMS_LOB.SUBSTR(q.sql_fulltext, 1000, 1) FROM v$sql q \
               WHERE q.sql_id = s.{id} AND q.child_number = s.{child} AND ROWNUM = 1)"
        )
    };
    format!(
        "SELECT s.sid || ',' || s.serial# || ':' || s.sql_id || ':' || s.sql_exec_id, {cur_start}, {cur_text}, '1', \
                NULL, {common} \
           FROM v$session s \
          WHERE s.type = 'USER' AND s.sid <> SYS_CONTEXT('USERENV', 'SID') AND s.status = 'ACTIVE' \
            AND s.sql_id IS NOT NULL AND s.sql_exec_start IS NOT NULL{scope} \
         UNION ALL \
         SELECT s.sid || ',' || s.serial# || ':' || s.prev_sql_id || ':' || s.prev_exec_id, {prev_start}, {prev_text}, '0', \
                NULL, {common} \
           FROM v$session s \
          WHERE s.type = 'USER' AND s.sid <> SYS_CONTEXT('USERENV', 'SID') \
            AND s.prev_sql_id IS NOT NULL AND s.prev_exec_start IS NOT NULL{scope}",
        cur_start = utc("s.sql_exec_start"),
        cur_text = text("sql_id", "sql_child_number"),
        prev_start = utc("s.prev_exec_start"),
        prev_text = text("prev_sql_id", "prev_child_number"),
    )
}

fn sample(r: Vec<Option<String>>) -> Sample {
    let mut r = r.into_iter();
    let mut next = || r.next().flatten();
    Sample {
        session: next().unwrap_or_default(),
        started: next().unwrap_or_default(),
        text: next().unwrap_or_default(),
        running: next().as_deref() == Some("1"),
        duration_ms: next().and_then(|v| v.parse::<f64>().ok()).map(|ms| ms.max(0.0)),
        database: next(),
        user: next(),
        client: next(),
        application: next(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_id_comes_back_from_the_detail() {
        assert_eq!(sql_id("sql_id 90mktz2skq1hq"), Some("90mktz2skq1hq"));
        assert_eq!(sql_id("sql_id 90mktz2skq1hq, media de 3 ejecuciones"), Some("90mktz2skq1hq"));
        assert_eq!(sql_id("otra cosa"), None);
        assert_eq!(key_sql_id("53,1234:90mktz2skq1hq:16777216"), Some("90mktz2skq1hq"));
        assert_eq!(key_sql_id("53,1234::"), None);
    }

    #[test]
    fn cursor_figures_per_execution() {
        let row = |v: &[&str]| v.iter().map(|s| Some(s.to_string())).collect::<Vec<_>>();
        let (id, f) = figures(row(&["abc", "12.5", "3", "1", "4.25", "120", "0"])).unwrap();
        let mut st = ProfiledStatement { detail: Some("sql_id abc".into()), ..Default::default() };
        apply(&mut st, &id, &f);
        assert_eq!((st.duration_ms, st.rows, st.cpu_ms, st.reads, st.writes), (Some(12.5), Some(3), Some(4.25), Some(120), Some(0)));
        assert_eq!(st.detail.as_deref(), Some("sql_id abc"));

        let (id, f) = figures(row(&["abc", "2", "3", "4", ".5", "7", "1"])).unwrap();
        let mut st = ProfiledStatement::default();
        apply(&mut st, &id, &f);
        assert_eq!((st.rows, st.cpu_ms, st.reads, st.writes), (None, Some(0.5), Some(7), Some(1)));
        assert_eq!(st.detail.as_deref(), Some("sql_id abc, media de 4 ejecuciones"));
    }
}

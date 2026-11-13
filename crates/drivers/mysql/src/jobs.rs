//! Server jobs ([`dbine_driver::Session::jobs`] and the calls around it)
//! per variant:
//!
//! - MySQL and MariaDB (Aurora, Cloud SQL, Azure, Percona): the Event
//!   Scheduler's events (see [`events`]). No history and no "run now": the
//!   server keeps neither.
//! - OceanBase (MySQL mode, 4.x): the same events, which it runs as
//!   DBMS_SCHEDULER jobs: its scheduler's views add each run's outcome and
//!   the runs going on. No "run now" either.
//! - StarRocks: tasks (`SUBMIT TASK`, scheduled or not) and the refresh
//!   tasks of asynchronous materialized views, with their runs from
//!   `information_schema.task_runs` (see [`starrocks`]).
//! - Doris and VeloDB (2.1+): `CREATE JOB` jobs and the jobs that refresh
//!   asynchronous materialized views, with their tasks (see [`doris`]).
//! - TiDB (no `CREATE EVENT`), SingleStore (pipelines are continuous loads,
//!   not scheduled jobs), Manticore and GreptimeDB have no scheduler.
//!   Databend has tasks only in Databend Cloud, with no server to test
//!   against ([`unsupported`] says it to the user).
//!
//! Ids are the engine's names, backtick-quoted as identifiers
//! (`` `db`.`event` ``, `` task:`name` ``, `` mv:`name` ``): they round-trip
//! whatever the name holds, and an id that doesn't read back exactly is
//! refused before anything reaches the server. Names go into statements
//! quoted again as identifiers or as string literals, never as they came.

pub(crate) mod clock;
pub(crate) mod doris;
pub(crate) mod events;
pub(crate) mod starrocks;

use crate::session::{lit, named, MySqlSession};
use crate::Variant;
use clock::Civil;
use dbine_driver::jobs::{JobDetail, JobList, JobOutcome, JobRun, MAX_JOB_RUNS};
use dbine_driver::{Error, PropertyInfo, Result};
use std::collections::HashMap;

/// The variants whose sessions list jobs.
pub(crate) fn supported(v: Variant) -> bool {
    matches!(
        v.base(),
        Variant::MySql
            | Variant::MariaDb
            | Variant::OceanBase
            | Variant::StarRocks
            | Variant::Doris
    )
}

/// Why a variant has no jobs.
pub(crate) fn unsupported(v: Variant) -> Error {
    Error::Unsupported(
        match v.base() {
            Variant::TiDb => "TiDB no tiene eventos ni otro programador de jobs",
            Variant::SingleStore => "SingleStore no tiene jobs programados por SQL",
            Variant::Databend => {
                "Databend tiene tareas solo en Databend Cloud, y DBine todavía no las lista"
            }
            Variant::Manticore => "Manticore Search no tiene jobs del servidor",
            Variant::GreptimeDb => "GreptimeDB no tiene jobs del servidor",
            _ => "este motor no tiene jobs del servidor",
        }
        .into(),
    )
}

pub(crate) async fn list(s: &mut MySqlSession) -> Result<JobList> {
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::list(s).await,
        Variant::StarRocks => starrocks::list(s).await,
        Variant::Doris => doris::list(s).await,
        v => Err(unsupported(v)),
    }
}

pub(crate) async fn detail(s: &mut MySqlSession, id: &str) -> Result<JobDetail> {
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::detail(s, id).await,
        Variant::StarRocks => starrocks::detail(s, id).await,
        Variant::Doris => doris::detail(s, id).await,
        v => Err(unsupported(v)),
    }
}

pub(crate) async fn runs(s: &mut MySqlSession, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    // At most `limit` (none for 0) and never past MAX_JOB_RUNS.
    let limit = limit.min(MAX_JOB_RUNS) as usize;
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::runs(s, id, limit).await,
        Variant::StarRocks => starrocks::runs(s, id, limit).await,
        Variant::Doris => doris::runs(s, id, limit).await,
        v => Err(unsupported(v)),
    }
}

pub(crate) async fn run(s: &mut MySqlSession, id: &str) -> Result<()> {
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => {
            events::event_id(id)?;
            Err(Error::Unsupported(
                "el servidor no ejecuta un evento a pedido: solo según su programación".into(),
            ))
        }
        Variant::StarRocks => starrocks::run(s, id).await,
        Variant::Doris => doris::run(s, id).await,
        v => Err(unsupported(v)),
    }
}

pub(crate) async fn stop(s: &mut MySqlSession, id: &str) -> Result<()> {
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::stop(s, id).await,
        Variant::StarRocks => starrocks::stop(s, id).await,
        Variant::Doris => doris::stop(s, id).await,
        v => Err(unsupported(v)),
    }
}

pub(crate) async fn set_enabled(s: &mut MySqlSession, id: &str, enabled: bool) -> Result<()> {
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => {
            events::set_enabled(s, id, enabled).await
        }
        Variant::StarRocks => starrocks::set_enabled(s, id, enabled).await,
        Variant::Doris => doris::set_enabled(s, id, enabled).await,
        v => Err(unsupported(v)),
    }
}

pub(crate) async fn script(s: &mut MySqlSession, id: &str) -> Result<String> {
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::script(s, id).await,
        Variant::StarRocks => starrocks::script(s, id).await,
        Variant::Doris => doris::script(s, id).await,
        v => Err(unsupported(v)),
    }
}

// ---- ids ----

/// Longest name an id may carry (engines allow 64 to 256 characters).
pub(crate) const MAX_NAME: usize = 256;

/// A backtick-quoted name at the start of `s` (backticks inside doubled):
/// the name and what follows the closing backtick.
pub(crate) fn unquote(s: &str) -> Option<(String, &str)> {
    let rest = s.strip_prefix('`')?;
    let mut name = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '`' {
            name.push(c);
        } else if chars.peek().is_some_and(|&(_, n)| n == '`') {
            name.push('`');
            chars.next();
        } else {
            let ok = !name.is_empty()
                && name.chars().count() <= MAX_NAME
                && !name.chars().any(char::is_control);
            return ok.then(|| (name, &rest[i + 1..]));
        }
    }
    None
}

/// `` `name` `` as an id part.
pub(crate) fn quoted(name: &str) -> String {
    dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Backtick, name)
}

/// `prefix` + one quoted name, nothing else.
pub(crate) fn one_name(id: &str, prefix: &str) -> Option<String> {
    match unquote(id.strip_prefix(prefix)?)? {
        (name, "") => Some(name),
        _ => None,
    }
}

/// A `SHOW CREATE` statement that names its object bare (`CREATE …
/// MATERIALIZED VIEW mv1 …`), with the name qualified by its database; after
/// a `USE` when the name isn't where it's expected.
fn qualify(create: &str, keyword: &str, schema: &str, name: &str) -> String {
    let create = create.trim_end().trim_end_matches(';').trim_end();
    for bare in [quoted(name), name.to_string()] {
        let pattern = format!("{keyword} {bare}");
        let Some(i) = create.find(&pattern) else {
            continue;
        };
        let after = &create[i + pattern.len()..];
        if after.is_empty() || after.starts_with(|c: char| c.is_whitespace() || c == '(') {
            return format!(
                "{}{keyword} {}.{}{after};\n",
                &create[..i],
                quoted(schema),
                quoted(name)
            );
        }
    }
    format!("USE {};\n{create};\n", quoted(schema))
}

/// Whether a failed catalog query failed for lack of privileges.
pub(crate) fn denied(e: &Error) -> bool {
    let m = e.to_string().to_ascii_lowercase();
    m.contains("denied") || m.contains("privilege")
}

/// The rows of a query on a catalog table or table function that older
/// versions lack: `None` when the server says it has no such table or
/// function, the error for any other failure.
pub(crate) async fn rows_if_present(
    s: &mut MySqlSession,
    sql: &str,
) -> Result<Option<Vec<mysql_async::Row>>> {
    use mysql_async::prelude::Queryable;
    match s.conn.query::<mysql_async::Row, _>(sql).await {
        Ok(rows) => Ok(Some(rows)),
        Err(mysql_async::Error::Server(e)) if missing(e.code, &e.message) => {
            tracing::debug!("{:?}: {sql}: {}", s.variant, e.message);
            Ok(None)
        }
        Err(e) => Err(crate::err(e)),
    }
}

/// A server error that says the table or table function read isn't there:
/// ER_UNKNOWN_TABLE, ER_NO_SUCH_TABLE, or StarRocks' and Doris' analyzer
/// messages (they report it under a generic number).
fn missing(code: u16, message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    let function = m.contains("function")
        && [
            "unknown",
            "not found",
            "can not found",
            "can not find",
            "cannot find",
            "could not find",
            "does not exist",
        ]
        .iter()
        .any(|w| m.contains(w));
    matches!(code, 1109 | 1146) || m.contains("unknown table") || function
}

/// The note for a user who may not see the server's jobs.
const DENIED_NOTE: &str = "Este usuario no tiene permiso para ver los jobs del servidor.";

/// The error for an id that isn't one of ours.
pub(crate) fn bad_id(id: &str, what: &str) -> Error {
    let shown: String = id.chars().take(120).collect();
    Error::Query(format!("«{shown}» no es un id de {what}"))
}

// ---- rows ----

/// A cell by column name, trimmed; `None` for NULL, empty and Doris's `\N`.
pub(crate) fn text(r: &mysql_async::Row, names: &[&str]) -> Option<String> {
    named(r, names)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "\\N")
}

fn number(r: &mysql_async::Row, names: &[&str]) -> Option<u64> {
    text(r, names)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u64)
}

fn prop(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: group.into(),
        label: label.into(),
        value: value.into(),
    }
}

/// How long between two wall-clock times of the same zone, in ms.
fn duration_ms(started: Option<Civil>, finished: Option<Civil>) -> Option<u64> {
    let d = finished?.seconds() - started?.seconds();
    (d >= 0).then(|| d as u64 * 1000)
}

/// A run state as StarRocks and Doris spell it.
fn outcome(state: &str) -> JobOutcome {
    match state.trim().to_ascii_uppercase().as_str() {
        "SUCCESS" | "SUCCEEDED" | "FINISHED" => JobOutcome::Succeeded,
        "FAILED" | "ERROR" => JobOutcome::Failed,
        "CANCELED" | "CANCELLED" | "KILLED" => JobOutcome::Canceled,
        "RUNNING" | "PENDING" => JobOutcome::Running,
        "MERGED" | "SKIPPED" => JobOutcome::Skipped,
        _ => JobOutcome::Unknown,
    }
}

// ---- time zones ----

/// The UTC times of wall-clock times read in some zones: fixed offsets are
/// worked out here, named zones (and MySQL's `SYSTEM`) by the server's
/// `CONVERT_TZ`. `mysql` keeps named zones to the range MySQL converts
/// right (1970 to 2038; it returns other values unconverted or without
/// daylight saving). A time the server can't convert (its zone tables not
/// loaded) is left out.
async fn to_utc(
    s: &mut MySqlSession,
    items: &[(Civil, String)],
    mysql: bool,
) -> HashMap<(Civil, String), Civil> {
    let mut out = HashMap::new();
    let mut named_zone = Vec::new();
    for (t, tz) in items {
        if out.contains_key(&(*t, tz.clone())) {
            continue;
        }
        match clock::fixed_offset(tz) {
            Some(off) => {
                out.insert((*t, tz.clone()), clock::to_utc(*t, off));
            }
            None if (!mysql || t.in_timestamp_range())
                && !named_zone.contains(&(*t, tz.clone())) =>
            {
                named_zone.push((*t, tz.clone()));
            }
            None => {}
        }
    }
    for chunk in named_zone.chunks(100) {
        let cols: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(i, (t, tz))| {
                format!(
                    "CONVERT_TZ({}, {}, '+00:00') AS c{i}",
                    lit(&t.sql()),
                    lit(tz)
                )
            })
            .collect();
        let sql = format!("SELECT {}", cols.join(", "));
        let Some(row) = s.optional_rows(&sql).await.into_iter().next() else {
            continue;
        };
        for (i, key) in chunk.iter().enumerate() {
            if let Some(utc) = crate::session::at(&row, i).and_then(|v| Civil::parse(&v)) {
                out.insert(key.clone(), utc);
            }
        }
    }
    out
}

/// The session's time zone and the server's clock in UTC.
async fn session_clock(s: &mut MySqlSession) -> Result<(String, Option<Civil>)> {
    let rows = s.rows("SELECT @@time_zone, UTC_TIMESTAMP()").await?;
    let row = rows.first();
    let tz = row
        .and_then(|r| crate::session::at(r, 0))
        .unwrap_or_else(|| "SYSTEM".into());
    let now = row
        .and_then(|r| crate::session::at(r, 1))
        .and_then(|v| Civil::parse(&v));
    Ok((tz, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_names_round_trip() {
        for name in ["plain", "with`tick", "dots.and spaces", "ñandú", "``"] {
            let q = quoted(name);
            assert_eq!(unquote(&q), Some((name.to_string(), "")), "{name}");
            assert_eq!(
                one_name(&format!("task:{q}"), "task:").as_deref(),
                Some(name)
            );
        }
        assert_eq!(unquote("`a`.`b`"), Some(("a".to_string(), ".`b`")));
    }

    #[test]
    fn foreign_ids_are_refused() {
        for bad in [
            "",
            "plain",
            "`unclosed",
            "`a`b",
            "``",
            "`a`; DROP TABLE t; --",
            "`a\nb`",
            "task:`a` OR 1=1",
            "task:x",
        ] {
            assert!(one_name(bad, "task:").is_none(), "{bad:?}");
        }
        let long = format!("task:`{}`", "x".repeat(MAX_NAME + 1));
        assert!(one_name(&long, "task:").is_none());
        let e = bad_id("`x'; --", "evento").to_string();
        assert!(e.contains("no es un id de evento"), "{e}");
    }

    #[test]
    fn engine_states_read_as_outcomes() {
        assert_eq!(outcome("SUCCESS"), JobOutcome::Succeeded);
        assert_eq!(outcome("failed"), JobOutcome::Failed);
        assert_eq!(outcome("CANCELED"), JobOutcome::Canceled);
        assert_eq!(outcome("PENDING"), JobOutcome::Running);
        assert_eq!(outcome("MERGED"), JobOutcome::Skipped);
        assert_eq!(outcome("??"), JobOutcome::Unknown);
    }

    #[test]
    fn only_engines_with_a_scheduler_list_jobs() {
        for v in Variant::ALL {
            let expected = matches!(
                v.base(),
                Variant::MySql
                    | Variant::MariaDb
                    | Variant::OceanBase
                    | Variant::StarRocks
                    | Variant::Doris
            );
            assert_eq!(supported(v), expected, "{v:?}");
            if !expected {
                assert!(matches!(unsupported(v), Error::Unsupported(_)));
            }
        }
    }

    #[test]
    fn only_a_missing_catalog_reads_as_an_older_version() {
        assert!(missing(
            1146,
            "Table 'information_schema.tasks' doesn't exist"
        ));
        assert!(missing(
            1064,
            "Getting analyzing error. Detail message: Unknown table 'information_schema.tasks'."
        ));
        assert!(missing(
            1105,
            "errCode = 2, detailMessage = Can not found function 'jobs'"
        ));
        assert!(missing(1105, "Unknown table function 'jobs'"));
        for real in [
            "Backend node not found. Check if any backend node is down.",
            "Access denied; you need (at least one of) the ADMIN privilege(s) for this operation",
            "Query timeout",
            "Unknown column 'Name' in 'table list'",
        ] {
            assert!(!missing(1105, real), "{real}");
        }
    }

    #[test]
    fn durations_between_wall_clock_times() {
        let a = Civil::parse("2026-11-12 03:00:00");
        let b = Civil::parse("2026-11-12 03:01:30");
        assert_eq!(duration_ms(a, b), Some(90_000));
        assert_eq!(duration_ms(b, a), None);
        assert_eq!(duration_ms(a, None), None);
    }
}

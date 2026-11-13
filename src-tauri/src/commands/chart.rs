//! A chart over the whole result of its query (the results grid's and the
//! notebook cell's chart), not just the rows they fetched. See
//! `dbine_driver::aggregate`:
//!
//! - grouped charts on SQL engines: the query wrapped in a `GROUP BY`
//!   (`Driver::aggregate_query`), only the groups come back;
//! - everything else, and a grouped statement the server rejects: the query
//!   runs again and its rows stream through an `Aggregator`, up to
//!   [`CLIENT_ROW_CAP`] rows.
//!
//! Reads only, on a read-only session of its own (`chart:<chart_id>`, which
//! `cancel_query` stops): a statement runs with `Session::run_read_only`
//! where the engine enforces the read; where it can't (`Unsupported`), with
//! `execute` only after the read-only guard finds no write in the user's
//! query nor in the statement DBine built (SQL and CQL; the other engines
//! enforce the session's read-only mode in their drivers). Not offered to
//! MCP or the AI assistant.
//!
//! A notebook's DataFrame SQL cell aggregates on its tab's DuckDB session
//! instead (`notebook_tab`, see `notebook::local::aggregate_local`).

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::aggregate::{self, Aggregator, ChartAggregate, ChartKind};
use dbine_driver::{
    sql, Driver, Error, Language, QueryOutcome, RowSinkRef, ScriptStatement, Session,
};
use serde::Deserialize;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::State;
use tokio::sync::Notify;

/// Rows the client path reads at most; past it the answer says
/// `rows_capped` and covers the first ones.
pub const CLIENT_ROW_CAP: u64 = 1_000_000;

#[derive(Deserialize)]
pub struct ChartAggregateArgs {
    /// Names this run's session (`chart:<chart_id>`): `cancel_query` with
    /// that id stops it.
    pub chart_id: String,
    #[serde(default)]
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    /// A DataFrame SQL cell's chart: the notebook tab whose DuckDB session
    /// (`nbx:<tab>`) aggregates it, instead of a connection
    /// ([`crate::notebook::local::aggregate_local`]; `cancel_query` on
    /// that session stops it).
    #[serde(default)]
    pub notebook_tab: Option<String>,
    /// The query the chart is drawn from (the cell's or the editor's).
    pub query: String,
    /// What to aggregate (`kind`: `group`, `histogram` or `box`).
    pub chart: ChartKind,
}

/// The chart's data over the whole result of `query`.
#[tauri::command(rename_all = "camelCase")]
pub async fn chart_aggregate(
    state: State<'_, AppState>,
    args: ChartAggregateArgs,
) -> CommandResult<ChartAggregate> {
    if let Some(tab) = args.notebook_tab.as_deref().filter(|t| !t.is_empty()) {
        return crate::notebook::local::aggregate_local(&state, tab, &args.query, &args.chart)
            .await;
    }
    let driver = driver_of(&state, &args.connection_id)?;
    let key = format!("chart:{}", args.chart_id);
    let entry = state
        .dedicated_session(&key, &args.connection_id, &args.database, true)
        .await?;
    let result = {
        let mut s = entry.session.lock().await;
        let stop = Stop {
            cancel: &entry.cancel,
            cancelled: &entry.cancelled,
            interrupter: entry.interrupter.as_ref(),
        };
        aggregate_on(
            driver.as_ref(),
            &mut **s,
            &args.query,
            &args.chart,
            CLIENT_ROW_CAP,
            &stop,
        )
        .await
    };
    // The session was only for this chart (and a stopped statement leaves
    // it in an unknown state).
    state
        .sessions
        .remove_if(&key, |_, e| Arc::ptr_eq(e, &entry));
    result
}

/// How a run is stopped from outside: `cancel_query` sets `cancelled`,
/// fires the interrupter and wakes `cancel`.
pub(crate) struct Stop<'a> {
    pub cancel: &'a Notify,
    pub cancelled: &'a AtomicBool,
    pub interrupter: Option<&'a Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) enum Fail {
    Cancelled,
    /// The guard found a write: nothing ran.
    Refused(String),
    Failed(String),
}

impl From<Fail> for CommandError {
    fn from(f: Fail) -> Self {
        match f {
            Fail::Cancelled => CommandError::Cancelled,
            Fail::Refused(m) => CommandError::BadRequest(m),
            Fail::Failed(m) => CommandError::Sql(m),
        }
    }
}

/// `query` cut for the checks that want a single query (the chart over the
/// whole result, the export's row estimate): the driver's units, or its
/// dialect's when those are fewer. A driver's own units may set apart a
/// line that only sets the context of the statement under it (Cosmos DB's
/// `-- container: x`, InfluxQL's `USE db`, a cqlsh command such as
/// `CONSISTENCY ONE`, a psql meta-command), which its `execute` runs with
/// that statement as one query; the dialect only cuts at the terminator
/// and at its batch lines, so it keeps them together.
pub(crate) fn query_units(driver: &dyn Driver, query: &str) -> Vec<ScriptStatement> {
    let own = driver.split_script(query);
    if own.len() < 2 {
        return own;
    }
    let plain = sql::split_script(query, &driver.script_dialect());
    if plain.len() < own.len() {
        plain
    } else {
        own
    }
}

/// The aggregation on `s` (a read-only session): the server's `GROUP BY`
/// where the driver builds one and the server runs it, else the client
/// path over at most `cap` rows.
pub(crate) async fn aggregate_on(
    driver: &dyn Driver,
    s: &mut dyn Session,
    query: &str,
    chart: &ChartKind,
    cap: u64,
    stop: &Stop<'_>,
) -> CommandResult<ChartAggregate> {
    let query = query.trim();
    if query.is_empty() {
        return Err(CommandError::BadRequest(
            "no hay consulta para el gráfico".into(),
        ));
    }
    let language = driver.info().language;
    if matches!(language, Language::Sql | Language::Cql) && query_units(driver, query).len() > 1 {
        return Err(CommandError::BadRequest(
            "el gráfico sobre todo el resultado lee una sola consulta: dejá solo la consulta del gráfico".into(),
        ));
    }
    if let ChartKind::Group(spec) = chart {
        let spec =
            aggregate::validate(spec).map_err(|e| CommandError::BadRequest(e.to_string()))?;
        if let Ok(statement) = driver.aggregate_query(query, &spec) {
            let max = spec.max_groups as usize + 1;
            match read(driver, s, query, &statement, max, None, None, stop).await {
                Ok((out, _)) => {
                    let rows = out
                        .results
                        .iter()
                        .find(|r| !r.columns.is_empty())
                        .map(|r| r.rows.as_slice())
                        .unwrap_or_default();
                    return Ok(aggregate::from_grouped(&spec, rows)?);
                }
                Err(Fail::Failed(e)) => {
                    // The query wrapped in a GROUP BY isn't valid there (a
                    // column type that can't be grouped, a construct a
                    // derived table refuses…): the rows are read instead.
                    tracing::debug!(%e, "chart: the grouped statement failed; aggregating in DBine");
                }
                Err(f) => return Err(f.into()),
            }
        }
    }
    let capped = Arc::new(Notify::new());
    let wake = capped.clone();
    let agg = Arc::new(Mutex::new(
        Aggregator::new(chart.clone(), cap)
            .map_err(|e| CommandError::BadRequest(e.to_string()))?
            .on_cap(move || wake.notify_one()),
    ));
    let sink = RowSinkRef(agg.clone());
    let max = usize::try_from(cap.saturating_add(1)).unwrap_or(usize::MAX);
    read(
        driver,
        s,
        query,
        query,
        max,
        Some(sink),
        Some(&capped),
        stop,
    )
    .await?;
    let mut a = agg
        .lock()
        .map_err(|_| CommandError::Internal("agregador del gráfico".into()))?;
    Ok(a.finish()?)
}

/// The first statement of `text` the read-only guard sees as a write, on
/// languages it reads (SQL, CQL).
fn write_in(driver: &dyn Driver, text: &str) -> Option<String> {
    matches!(driver.info().language, Language::Sql | Language::Cql)
        .then(|| dbine_driver::read_only::first_write_in(text, &driver.script_dialect()))
        .flatten()
}

/// Run `statement` as a read (see the module docs). `query` is the user's,
/// which the guard checks too when the engine can't enforce the read.
/// Returns the outcome and whether it stopped at the row cap.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read(
    driver: &dyn Driver,
    s: &mut dyn Session,
    query: &str,
    statement: &str,
    max_rows: usize,
    sink: Option<RowSinkRef>,
    capped: Option<&Notify>,
    stop: &Stop<'_>,
) -> Result<(QueryOutcome, bool), Fail> {
    if stop.cancelled.load(Ordering::SeqCst) {
        return Err(Fail::Cancelled);
    }
    let mut out = QueryOutcome {
        sink: sink.clone(),
        ..Default::default()
    };
    match until(stop, capped, s.run_read_only(statement, max_rows, &mut out)).await {
        Until::Done(Err(Error::Unsupported(_))) => {}
        other => return settle(other, out),
    }
    if let Some(kw) = write_in(driver, query).or_else(|| write_in(driver, statement)) {
        return Err(Fail::Refused(format!(
            "el gráfico solo lee: la consulta tiene una sentencia {kw}"
        )));
    }
    let mut out = QueryOutcome {
        sink,
        ..Default::default()
    };
    let ran = until(stop, capped, s.execute(statement, max_rows, &mut out)).await;
    settle(ran, out)
}

fn settle(ran: Until, mut out: QueryOutcome) -> Result<(QueryOutcome, bool), Fail> {
    out.sink = None;
    match ran {
        Until::Cancelled => Err(Fail::Cancelled),
        Until::Capped => Ok((out, true)),
        Until::Done(Err(Error::Cancelled)) => Err(Fail::Cancelled),
        Until::Done(Err(e)) => Err(Fail::Failed(e.to_string())),
        Until::Done(Ok(())) => match out.error.take().or_else(|| out.sink_error.take()) {
            Some(e) => Err(Fail::Failed(e)),
            None => Ok((out, false)),
        },
    }
}

enum Until {
    Done(dbine_driver::Result<()>),
    Cancelled,
    /// The aggregator got a row past the cap: the statement was stopped.
    Capped,
}

/// Await `run` until it ends, is cancelled, or hits the row cap (then the
/// statement is interrupted on the server and dropped).
async fn until<F: Future<Output = dbine_driver::Result<()>>>(
    stop: &Stop<'_>,
    capped: Option<&Notify>,
    run: F,
) -> Until {
    tokio::pin!(run);
    let cap = async {
        match capped {
            Some(n) => n.notified().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        r = &mut run => if stop.cancelled.load(Ordering::SeqCst) { Until::Cancelled } else { Until::Done(r) },
        _ = stop.cancel.notified() => Until::Cancelled,
        _ = cap => {
            if let Some(i) = stop.interrupter {
                i();
            }
            Until::Capped
        }
    }
}

#[cfg(test)]
#[path = "chart_live_tests.rs"]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn driver(id: &str) -> &'static dyn Driver {
        dbine_drivers::find(id)
            .unwrap_or_else(|| panic!("driver {id} not built in"))
            .as_ref()
    }

    /// A line that only sets the context of the query under it is a unit
    /// of its own for the driver (its run lens, its number), but not a
    /// second query for the chart; two queries still are.
    #[test]
    fn a_context_line_and_its_query_are_one_query() {
        for (id, context, first, second) in [
            (
                "cosmosdb",
                "-- container: items\n",
                "SELECT c.cat, c.n FROM c",
                "SELECT VALUE COUNT(1) FROM c",
            ),
            (
                "influxdb1",
                "USE mydb\n",
                "SELECT mean(v) FROM cpu WHERE time > now() - 1h GROUP BY time(1m)",
                "SELECT * FROM mem",
            ),
            (
                "cassandra",
                "CONSISTENCY ONE\n",
                "SELECT k, v FROM ks.t",
                "SELECT k FROM ks.u",
            ),
            (
                "scylladb",
                "PAGING OFF\n",
                "SELECT k, v FROM ks.t",
                "SELECT k FROM ks.u",
            ),
        ] {
            let d = driver(id);
            let one = format!("{context}{first}");
            assert_eq!(
                d.split_script(&one).len(),
                2,
                "{id}: the driver's own units"
            );
            assert_eq!(query_units(d, &one).len(), 1, "{id}");
            let one = format!("{context}{first};\n");
            assert_eq!(query_units(d, &one).len(), 1, "{id}");
            let two = format!("{context}{first};\n{second};");
            assert_eq!(query_units(d, &two).len(), 2, "{id}");
        }
        // A context line after `;` is still cut there by the dialect too.
        let influx = driver("influxdb1");
        assert_eq!(query_units(influx, "USE mydb;\nSELECT * FROM cpu").len(), 2);
        let pg = driver("postgres");
        assert_eq!(query_units(pg, "SELECT 1; SELECT 2").len(), 2);
        assert_eq!(query_units(pg, "SELECT 1").len(), 1);
    }
}

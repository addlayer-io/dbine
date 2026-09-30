//! The profiler ([`dbine_driver::profiler`]) per flavor. None of them keeps
//! a history an API can read (Neo4j's query log and Neptune's audit log go
//! to files), so all are sampled from what runs now:
//!
//! - Neo4j: `SHOW TRANSACTIONS YIELD *` (4.4+), each transaction's current
//!   query, scoped to the chosen database.
//! - Memgraph: `SHOW TRANSACTIONS`, the queries of each open transaction
//!   (the whole server: it has no database column).
//! - Neptune: the status APIs (`/openCypher/status`, `/gremlin/status`,
//!   `/sparql/status`), the queries running in the cluster.
//!
//! Nothing is switched on.
//!
//! Only Neo4j reports what a query costs: its page cache hits and faults
//! (reads, in pages) and its CPU time (only with `db.track_query_cpu_time`
//! on). Nothing reports what a query wrote, and Memgraph and Neptune report
//! neither CPU nor reads.

use crate::monitor::duration_ms;
use crate::{as_text, Flavor, GraphSession};
use chrono::{DateTime, NaiveDateTime, Utc};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::time::Instant;

pub(crate) struct State {
    sampler: Sampler,
    /// Neo4j: only this database's transactions ("" = all).
    database: String,
    /// When each query was first seen, for engines that don't say when it
    /// started (Neptune, older Memgraph).
    first_seen: HashMap<String, String>,
}

fn now() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

pub(crate) async fn start(s: &mut GraphSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let started = match s.flavor {
        Flavor::Neo4j => {
            s.records("SHOW TRANSACTIONS").await.map_err(|e| show_refused(e, "SHOW TRANSACTIONS"))?;
            ProfilerStarted::new(ProfilerMode::Sampled, "SHOW TRANSACTIONS").note(
                "Neo4j solo muestra las consultas en curso: las que duran menos de una décima de segundo pueden no verse \
                 (el registro de consultas va a un archivo del servidor). Sin el privilegio SHOW TRANSACTION, un \
                 usuario solo ve sus propias consultas.",
            )
            .units(Some("páginas"), None)
        }
        Flavor::Memgraph => {
            s.records("SHOW TRANSACTIONS").await.map_err(|e| show_refused(e, "SHOW TRANSACTIONS"))?;
            ProfilerStarted::new(ProfilerMode::Sampled, "SHOW TRANSACTIONS").note(
                "Memgraph solo muestra las transacciones en curso, de todo el servidor: las consultas que duran menos \
                 de una décima de segundo pueden no verse. Sin el privilegio TRANSACTION_MANAGEMENT, un usuario solo \
                 ve las suyas.",
            )
        }
        Flavor::Neptune => {
            let c = s.neptune_client().ok_or_else(|| Error::State("sin cliente de Neptune".into()))?;
            c.json("/openCypher/status").await?;
            ProfilerStarted::new(ProfilerMode::Sampled, "/openCypher/status, /gremlin/status, /sparql/status").note(
                "Neptune solo muestra las consultas en curso del cluster, sin usuario ni cliente: las que duran menos \
                 de una décima de segundo pueden no verse. El registro de auditoría va a CloudWatch, no a esta API.",
            )
        }
    };
    let database = if s.flavor == Flavor::Neo4j { opts.database.clone() } else { String::new() };
    Ok((State { sampler: Sampler::new(now()), database, first_seen: HashMap::new() }, started))
}

fn show_refused(e: Error, what: &str) -> Error {
    match e {
        Error::Query(m) => Error::Query(format!("El servidor no permite listar las transacciones ({what}): {m}")),
        e => e,
    }
}

pub(crate) async fn poll(s: &mut GraphSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let mut out = Vec::new();
    let until = Instant::now() + SAMPLE_FOR;
    loop {
        let samples = match s.flavor {
            Flavor::Neo4j => {
                let rows = s.records("SHOW TRANSACTIONS YIELD *").await?;
                rows.iter().filter_map(|r| neo4j_sample(r, &s.tag, &state.database)).collect()
            }
            Flavor::Memgraph => {
                let rows = s.records("SHOW TRANSACTIONS").await?;
                rows.iter().filter_map(|r| memgraph_sample(r, &s.tag, &mut state.first_seen)).collect()
            }
            Flavor::Neptune => {
                let c = s.neptune_client().ok_or_else(|| Error::State("sin cliente de Neptune".into()))?;
                neptune_samples(&c, &mut state.first_seen).await?
            }
        };
        // Forget the first sightings of queries that are gone.
        let live: std::collections::HashSet<&str> = samples.iter().map(|x: &Sample| x.session.as_str()).collect();
        state.first_seen.retain(|k, _| live.contains(k.as_str()));
        out.extend(state.sampler.feed(samples));
        if Instant::now() + SAMPLE_EVERY > until {
            break;
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

fn text(r: &Map<String, Value>, k: &str) -> Option<String> {
    r.get(k).map(as_text).filter(|t| !t.is_empty())
}

fn is_own(meta: Option<&Value>, tag: &str) -> bool {
    meta.and_then(|m| m.get("dbine")).and_then(Value::as_str) == Some(tag)
}

/// A Neo4j transaction running a query, except ours and other databases'.
fn neo4j_sample(r: &Map<String, Value>, tag: &str, database: &str) -> Option<Sample> {
    let query = text(r, "currentQuery")?;
    let db = text(r, "database");
    if is_own(r.get("metaData"), tag) || (!database.is_empty() && db.as_deref() != Some(database)) {
        return None;
    }
    let started = r.get("currentQueryStartTime").or_else(|| r.get("startTime")).map(as_text).and_then(|t| utc(&t))?;
    let application = r.get("metaData").and_then(|m| m.get("app")).and_then(Value::as_str).filter(|a| !a.is_empty());
    let mut detail = Vec::new();
    // Only worth showing when it isn't plainly running (blocked, waiting…).
    if let Some(st) = text(r, "currentQueryStatus").or_else(|| text(r, "status")).filter(|s| !s.eq_ignore_ascii_case("running")) {
        detail.push(st);
    }
    // Pages read from the page cache, whether they were there or not.
    let pages = |k: &str| r.get(k).and_then(Value::as_u64);
    let reads = match (pages("currentQueryPageHits"), pages("currentQueryPageFaults")) {
        (None, None) => None,
        (h, f) => Some(h.unwrap_or(0) + f.unwrap_or(0)),
    };
    Some(Sample {
        session: text(r, "transactionId")?,
        started,
        text: query,
        running: true,
        duration_ms: r.get("currentQueryElapsedTime").or_else(|| r.get("elapsedTime")).and_then(duration_ms),
        database: db,
        user: text(r, "username"),
        client: text(r, "clientAddress"),
        application: application.map(str::to_string),
        detail: (!detail.is_empty()).then(|| detail.join("; ")),
        // Null unless `db.track_query_cpu_time` is on.
        cpu_ms: r.get("currentQueryCpuTime").and_then(duration_ms),
        reads,
        ..Default::default()
    })
}

/// A Memgraph transaction with its queries, except ours.
fn memgraph_sample(r: &Map<String, Value>, tag: &str, first_seen: &mut HashMap<String, String>) -> Option<Sample> {
    if is_own(r.get("metadata"), tag) {
        return None;
    }
    let query = match r.get("query")? {
        Value::Array(a) => a.iter().map(as_text).collect::<Vec<_>>().join(";\n"),
        other => as_text(other),
    };
    if query.trim().is_empty() {
        return None;
    }
    let id = text(r, "transaction_id")?;
    let elapsed = r.get("elapsed_ms").and_then(|v| v.as_f64().or_else(|| as_text(v).parse().ok()));
    let started = r
        .get("start_time")
        .map(as_text)
        .and_then(|t| utc(&t))
        .unwrap_or_else(|| first_seen.entry(id.clone()).or_insert_with(|| since(elapsed)).clone());
    Some(Sample {
        session: id,
        started,
        text: query,
        running: true,
        duration_ms: elapsed,
        user: text(r, "username"),
        detail: text(r, "status").filter(|s| !s.eq_ignore_ascii_case("running")),
        ..Default::default()
    })
}

/// When a query that has run for `elapsed` ms started.
fn since(elapsed: Option<f64>) -> String {
    let back = chrono::Duration::milliseconds(elapsed.unwrap_or(0.0) as i64);
    (Utc::now() - back).format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

async fn neptune_samples(c: &crate::neptune::Client, first_seen: &mut HashMap<String, String>) -> Result<Vec<Sample>> {
    let (mut out, mut failed) = (Vec::new(), Vec::new());
    for (path, language) in [("/openCypher/status", "openCypher"), ("/gremlin/status", "Gremlin"), ("/sparql/status", "SPARQL")] {
        match c.json(path).await {
            Ok(v) => out.extend(neptune_queries(&v, language, first_seen)),
            // An endpoint may be off (SPARQL on a property-graph cluster…).
            Err(e) => failed.push(e),
        }
    }
    match failed.pop() {
        Some(e) if failed.len() == 2 => Err(e),
        _ => Ok(out),
    }
}

fn neptune_queries(v: &Value, language: &str, first_seen: &mut HashMap<String, String>) -> Vec<Sample> {
    let mut out = Vec::new();
    for q in v.get("queries").and_then(Value::as_array).into_iter().flatten() {
        let (Some(id), Some(text)) = (q.get("queryId").map(as_text), q.get("queryString").map(as_text)) else { continue };
        let elapsed = q.pointer("/queryEvalStats/elapsed").and_then(Value::as_f64);
        let session = format!("{language}:{id}");
        let started = first_seen.entry(session.clone()).or_insert_with(|| since(elapsed)).clone();
        let waited = q.pointer("/queryEvalStats/waited").and_then(Value::as_f64);
        out.push(Sample {
            session,
            started,
            text,
            running: true,
            duration_ms: elapsed,
            detail: Some(match waited {
                Some(w) => format!("{language}; espera {w:.0} ms"),
                None => language.to_string(),
            }),
            ..Default::default()
        });
    }
    out
}

/// A Bolt date-time as text (`…T…Z`, `…+02:00`, `…Z[Etc/UTC]`, or local)
/// in the profiler's format, in UTC.
fn utc(t: &str) -> Option<String> {
    let t = t.split('[').next()?.trim();
    let fmt = |d: DateTime<Utc>| d.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return Some(fmt(d.with_timezone(&Utc)));
    }
    let local = NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f"))
        .ok()?;
    Some(fmt(local.and_utc()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn times_become_utc() {
        assert_eq!(utc("2026-09-27T21:04:54.508Z").as_deref(), Some("2026-09-27 21:04:54.508"));
        assert_eq!(utc("2026-09-27T23:04:54.5+02:00").as_deref(), Some("2026-09-27 21:04:54.500"));
        assert_eq!(utc("2026-09-27T21:04:54.972958Z[Etc/UTC]").as_deref(), Some("2026-09-27 21:04:54.972"));
        assert_eq!(utc("2026-09-27T21:04:54").as_deref(), Some("2026-09-27 21:04:54.000"));
        assert_eq!(utc("nope"), None);
    }

    #[test]
    fn neo4j_rows() {
        let row = |db: &str, meta: Value| -> Map<String, Value> {
            json!({
                "transactionId": "neo4j-transaction-7", "currentQuery": "MATCH (n) RETURN n", "database": db,
                "currentQueryStartTime": "2026-09-27T21:04:54.508Z", "currentQueryElapsedTime": "PT0.25S",
                "username": "neo4j", "clientAddress": "127.0.0.1:1234", "metaData": meta, "currentQueryStatus": "running"
            })
            .as_object()
            .cloned()
            .unwrap()
        };
        let s = neo4j_sample(&row("neo4j", json!({"app": "cypher-shell"})), "me", "neo4j").unwrap();
        assert_eq!(s.duration_ms, Some(250.0));
        assert_eq!(s.client.as_deref(), Some("127.0.0.1:1234"));
        assert_eq!(s.application.as_deref(), Some("cypher-shell"));
        assert_eq!((s.cpu_ms, s.reads, s.detail.as_deref()), (None, None, None));
        let mut costed = row("neo4j", json!({}));
        costed.insert("currentQueryPageHits".into(), json!(40));
        costed.insert("currentQueryPageFaults".into(), json!(2));
        costed.insert("currentQueryCpuTime".into(), json!("PT0.12S"));
        let s = neo4j_sample(&costed, "me", "neo4j").unwrap();
        assert_eq!((s.cpu_ms, s.reads, s.writes), (Some(120.0), Some(42), None));
        assert!(neo4j_sample(&row("other", json!({})), "me", "neo4j").is_none());
        assert!(neo4j_sample(&row("neo4j", json!({"dbine": "me"})), "me", "").is_none());
    }

    #[test]
    fn neptune_status() {
        let v = json!({"queries": [{"queryId": "a1", "queryString": "MATCH (n) RETURN n", "queryEvalStats": {"elapsed": 300, "waited": 2}}]});
        let mut seen = HashMap::new();
        let first = neptune_queries(&v, "openCypher", &mut seen);
        let again = neptune_queries(&v, "openCypher", &mut seen);
        assert_eq!(first[0].started, again[0].started);
        assert_eq!((first[0].session.as_str(), first[0].duration_ms), ("openCypher:a1", Some(300.0)));
    }
}

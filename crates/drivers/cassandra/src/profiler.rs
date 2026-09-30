//! The profiler ([`dbine_driver::profiler`]) per flavor.
//!
//! - Cassandra 4.0+: `system_views.queries` (the statements each node is
//!   running now), sampled on every node. The view has no client, user or
//!   keyspace: every keyspace's statements are seen. Cassandra's own audit
//!   and full query logs are switched on by `nodetool` (JMX) and written to
//!   files, out of reach of CQL.
//! - ScyllaDB: its audit log in the `audit.audit_log` table, complete.
//!   Scylla must run with `audit: table` (a startup setting); when allowed,
//!   the profiler adds the QUERY, DML and DDL categories and the keyspace
//!   (`audit_categories`, `audit_keyspaces` in `system.config`, live) and
//!   puts both back at stop. The log has no durations or row counts.
//! - Amazon Keyspaces: no view of other clients' statements (CloudTrail
//!   logs them outside CQL): not supported.
//!
//! Neither source says what a statement cost (CPU, rows or partitions read
//! or written): the query view has only the queue and running times, and
//! the audit log not even that.

use crate::{boolean, int, text, CassandraSession, Flavor};
use chrono::{DateTime, Utc};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use scylla::cluster::Node;
use scylla::policies::load_balancing::{NodeIdentifier, SingleTargetLoadBalancingPolicy};
use scylla::statement::{Consistency, Statement};
use scylla::value::Row;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// In the profiler's own statements, to leave them out.
const OWN: &str = "/* dbine profiler */";
/// Audit rows are written by each node on its own: a poll looks this far
/// behind the newest one it saw, so late ones aren't missed.
const LAG_MS: i64 = 5000;
/// The audit categories that hold the statements.
const CATEGORIES: [&str; 3] = ["QUERY", "DML", "DDL"];

pub(crate) fn supported(f: Flavor) -> bool {
    f != Flavor::Keyspaces
}

pub(crate) enum State {
    /// `system_views.queries` on every node. `skew` is the server's clock
    /// minus ours (ms); `first` has, per node and thread, the statement it
    /// ran at the last look, how long it had been running and when it began.
    Queries { sampler: Sampler, skew: i64, first: HashMap<String, (String, i64, String)> },
    /// `audit.audit_log` of `ks`, one partition per day and node. `after`
    /// is the newest entry read (ms); `seen`, the entries read within the
    /// lag. `restore` has `system.config`'s settings as they were.
    Audit { ks: String, nodes: Vec<String>, after: i64, seen: HashMap<String, i64>, restore: Vec<(String, String)> },
}

async fn rows(s: &CassandraSession, cql: &str) -> Result<Vec<Row>> {
    s.rows(cql, ()).await
}

/// The server's clock, in ms.
async fn server_now(s: &CassandraSession) -> Result<i64> {
    let r = rows(s, "SELECT toUnixTimestamp(now()) FROM system.local").await?;
    Ok(r.first().map(|r| int(r, 0)).filter(|n| *n > 0).unwrap_or_else(|| Utc::now().timestamp_millis()))
}

pub(crate) async fn start(s: &CassandraSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    match s.flavor {
        Flavor::Keyspaces => Err(Error::Unsupported(
            "Amazon Keyspaces no muestra las consultas de otros clientes por CQL (quedan en CloudTrail)".into(),
        )),
        Flavor::Cassandra => {
            let now = server_now(s).await?;
            if let Err(e) = rows(s, &format!("SELECT thread_id FROM system_views.queries {OWN}")).await {
                return Err(Error::Unsupported(format!(
                    "este Cassandra no tiene system_views.queries (llegó en la 4.0): {e}"
                )));
            }
            let state = State::Queries {
                sampler: Sampler::new(stamp(now)),
                skew: now - Utc::now().timestamp_millis(),
                first: HashMap::new(),
            };
            let started = ProfilerStarted::new(ProfilerMode::Sampled, "system_views.queries").note(
                "Cassandra solo muestra las consultas en curso de cada nodo, sin el cliente, el usuario ni el keyspace: se ven las de todos los keyspaces.",
            );
            Ok((state, started))
        }
        Flavor::Scylla => audit_start(s, opts).await,
    }
}

async fn audit_start(s: &CassandraSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let ks = Some(opts.database.trim())
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .or_else(|| s.keyspace.clone())
        .ok_or_else(|| Error::Query("Elegí un keyspace para ver sus consultas.".into()))?;
    let setting = |name: &'static str| async move {
        let r = rows(s, &format!("SELECT value FROM system.config WHERE name = '{name}' {OWN}")).await?;
        Ok::<_, Error>(r.first().map(|r| unquote(&text(r, 0))))
    };
    let mode = setting("audit").await?.ok_or_else(|| {
        Error::Unsupported("esta versión de ScyllaDB no tiene auditoría (llegó a la edición abierta en la 2025.1)".into())
    })?;
    if !mode.split(',').any(|m| m.trim() == "table") {
        return Err(Error::Query(format!(
            "La auditoría de ScyllaDB está en «{mode}»: para ver las consultas hace falta «audit: table» en scylla.yaml (requiere reiniciar el nodo)."
        )));
    }
    let categories = setting("audit_categories").await?.unwrap_or_default();
    let keyspaces = setting("audit_keyspaces").await?.unwrap_or_default();
    let has = |list: &str, item: &str| list.split(',').any(|x| x.trim().eq_ignore_ascii_case(item));
    let missing: Vec<&str> = CATEGORIES.iter().copied().filter(|c| !has(&categories, c)).collect();
    let audited = has(&keyspaces, &ks);
    let mut started = ProfilerStarted::new(ProfilerMode::Complete, "audit.audit_log");
    let mut restore: Vec<(String, String)> = Vec::new();
    if !opts.change_server {
        if !audited || missing.len() == CATEGORIES.len() {
            return Err(Error::Query(format!(
                "La auditoría de ScyllaDB no registra las consultas de «{ks}» y la conexión es de solo lectura."
            )));
        }
        if !missing.is_empty() {
            started = started.note(format!("La auditoría no registra las categorías {}.", missing.join(", ")));
        }
    } else {
        let join = |list: &str, add: &[&str]| {
            let mut v: Vec<&str> = list.split(',').map(str::trim).filter(|x| !x.is_empty()).collect();
            v.extend(add);
            v.join(",")
        };
        let mut changes = Vec::new();
        if !missing.is_empty() {
            changes.push(("audit_categories", categories.clone(), join(&categories, &missing)));
        }
        if !audited {
            changes.push(("audit_keyspaces", keyspaces.clone(), join(&keyspaces, &[ks.as_str()])));
        }
        for (name, was, now) in changes {
            if let Err(e) = set(s, name, &now).await {
                for (name, was) in &restore {
                    let _ = set(s, name, was).await;
                }
                return Err(Error::Query(format!("No se pudo cambiar {name} de ScyllaDB: {e}")));
            }
            started = started.change(format!("{name} = {now} (estaba en «{was}»)"));
            restore.push((name.to_string(), was));
        }
    }
    // The audit log's partitions are per node, by the address it listens on.
    let mut nodes = Vec::new();
    for cql in ["SELECT listen_address, broadcast_address FROM system.local", "SELECT peer, peer FROM system.peers"] {
        for r in rows(s, &format!("{cql} {OWN}")).await? {
            for i in 0..2 {
                let a = text(&r, i);
                if !a.is_empty() && !nodes.contains(&a) {
                    nodes.push(a);
                }
            }
        }
    }
    let after = server_now(s).await?;
    let state = State::Audit { ks, nodes, after, seen: HashMap::new(), restore };
    Ok((state, started.note("La auditoría de ScyllaDB no registra la duración ni las filas de cada sentencia.")))
}

async fn set(s: &CassandraSession, name: &str, value: &str) -> Result<()> {
    let cql = format!("UPDATE system.config SET value = '{}' WHERE name = '{name}' {OWN}", value.replace('\'', "''"));
    s.session.query_unpaged(cql, ()).await.map(|_| ()).map_err(|e| Error::Query(e.to_string()))
}

pub(crate) async fn poll(s: &CassandraSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    match state {
        State::Queries { sampler, skew, first } => {
            let mut out = Vec::new();
            let until = Instant::now() + SAMPLE_FOR;
            loop {
                let mut samples = Vec::new();
                for node in s.session.get_cluster_state().get_nodes_info() {
                    samples.extend(node_queries(s, node, *skew, first).await?);
                }
                let live: std::collections::HashSet<&str> = samples.iter().map(|x| x.session.as_str()).collect();
                first.retain(|k, _| live.contains(k.as_str()));
                out.extend(sampler.feed(samples));
                if Instant::now() + SAMPLE_EVERY > until {
                    break;
                }
                tokio::time::sleep(SAMPLE_EVERY).await;
            }
            out.sort_by(|a, b| a.time.cmp(&b.time));
            Ok(out)
        }
        State::Audit { ks, nodes, after, seen, .. } => {
            let from = *after - LAG_MS;
            let today = Utc::now().timestamp_millis().max(*after).div_euclid(86_400_000);
            let days: Vec<String> = (from.div_euclid(86_400_000)..=today)
                .map(|d| format!("'{}+0000'", stamp(d * 86_400_000)))
                .collect();
            let nodes: Vec<String> = nodes.iter().map(|n| format!("'{n}'")).collect();
            let cql = format!(
                "SELECT event_time, toUnixTimestamp(event_time), category, consistency, error, keyspace_name, operation, \
                        source, username, table_name \
                 FROM audit.audit_log WHERE date IN ({}) AND node IN ({}) AND event_time > minTimeuuid('{}+0000') {OWN}",
                days.join(", "),
                nodes.join(", "),
                stamp(from)
            );
            // The audit keyspace has a replication factor of 3: a single
            // node or a node down would fail the default LOCAL_QUORUM.
            let mut st = Statement::new(cql);
            st.set_consistency(Consistency::LocalOne);
            let res = s.session.query_unpaged(st, ()).await.map_err(|e| Error::Query(e.to_string()))?;
            let mut out = Vec::new();
            for r in all_rows(res)? {
                let (id, ms) = (text(&r, 0), int(&r, 1));
                if text(&r, 5) != *ks || seen.contains_key(&id) {
                    continue;
                }
                seen.insert(id, ms);
                *after = (*after).max(ms);
                let (category, consistency, table) = (text(&r, 2), text(&r, 3), text(&r, 9));
                let mut detail = format!("{category} · {consistency}");
                if !table.is_empty() {
                    detail += &format!(" · {table}");
                }
                out.push(ProfiledStatement {
                    time: stamp(ms),
                    text: text(&r, 6),
                    database: Some(text(&r, 5)),
                    user: Some(text(&r, 8)).filter(|u| !u.is_empty()),
                    client: Some(text(&r, 7)).filter(|c| !c.is_empty()),
                    error: boolean(&r, 4).then(|| "falló".to_string()),
                    detail: Some(detail),
                    ..Default::default()
                });
            }
            let keep = *after - 2 * LAG_MS;
            seen.retain(|_, ms| *ms >= keep);
            out.sort_by(|a, b| a.time.cmp(&b.time));
            Ok(out)
        }
    }
}

/// Put back the settings `start` changed.
pub(crate) async fn stop(s: &CassandraSession, state: State) -> Result<()> {
    if let State::Audit { restore, .. } = state {
        for (name, was) in restore.iter().rev() {
            set(s, name, was).await?;
        }
    }
    Ok(())
}

/// What one node runs now, except the profiler's own look.
async fn node_queries(
    s: &CassandraSession,
    node: &Arc<Node>,
    skew: i64,
    first: &mut HashMap<String, (String, i64, String)>,
) -> Result<Vec<Sample>> {
    let mut st = Statement::new(format!("SELECT thread_id, queued_micros, running_micros, task FROM system_views.queries {OWN}"));
    st.set_load_balancing_policy(Some(SingleTargetLoadBalancingPolicy::new(NodeIdentifier::Node(node.clone()), None)));
    let res = s.session.query_unpaged(st, ()).await.map_err(|e| Error::Query(e.to_string()))?;
    let list = all_rows(res)?;
    let now = Utc::now().timestamp_millis() + skew;
    let addr = node.address.ip().to_string();
    let mut out = Vec::new();
    for r in list {
        let task = text(&r, 3);
        if task.contains(OWN) {
            continue;
        }
        let (queued, running) = (int(&r, 1), int(&r, 2));
        let (cql, consistency) = parse_task(&task);
        // Without a consistency level it's a node's internal read (a page of
        // an aggregate), not a client's request.
        if consistency.is_none() {
            continue;
        }
        let key = format!("{addr}/{}", text(&r, 0));
        // The same thread running the same text for less time: a new run.
        let started = match first.get(&key) {
            Some((t, was, started)) if *t == task && *was <= running => started.clone(),
            _ => stamp(now - (queued + running) / 1000),
        };
        first.insert(key.clone(), (task.clone(), running, started.clone()));
        let mut detail = format!("nodo {addr}");
        if let Some(c) = consistency {
            detail += &format!(" · consistencia {c}");
        }
        if queued >= 1000 {
            detail += &format!(" · en cola {} ms", queued / 1000);
        }
        out.push(Sample {
            session: key,
            started,
            text: cql,
            running: true,
            duration_ms: Some(running as f64 / 1000.0),
            detail: Some(detail),
            ..Default::default()
        });
    }
    Ok(out)
}

fn all_rows(res: scylla::response::query_result::QueryResult) -> Result<Vec<Row>> {
    let res = res.into_rows_result().map_err(Error::query)?;
    let list = res.rows::<Row>().map_err(Error::query)?.collect::<std::result::Result<Vec<_>, _>>();
    list.map_err(Error::query)
}

/// `QUERY <cql> [pageSize = 100] at consistency ONE` → the CQL and the
/// consistency.
fn parse_task(task: &str) -> (String, Option<String>) {
    let (rest, consistency) = match task.rsplit_once(" at consistency ") {
        Some((r, c)) => (r, Some(c.trim().to_string())),
        None => (task, None),
    };
    let rest = match rest.rfind(" [pageSize") {
        Some(i) if rest.ends_with(']') => &rest[..i],
        _ => rest,
    };
    let rest = rest.strip_prefix("QUERY ").or_else(|| rest.strip_prefix("EXECUTE ")).unwrap_or(rest);
    (rest.trim().to_string(), consistency)
}

/// `system.config` shows text values quoted (`"DCL,DDL"`).
fn unquote(v: &str) -> String {
    serde_json::from_str::<String>(v).unwrap_or_else(|_| v.to_string())
}

/// `YYYY-MM-DD HH:MM:SS.mmm` (UTC).
fn stamp(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks() {
        assert_eq!(
            parse_task("QUERY SELECT * FROM ks.t WHERE a = 1; [pageSize = 100] at consistency ONE"),
            ("SELECT * FROM ks.t WHERE a = 1;".to_string(), Some("ONE".to_string()))
        );
        assert_eq!(parse_task("something else").0, "something else");
    }

    #[test]
    fn config_values() {
        assert_eq!(unquote("\"DCL,DDL\""), "DCL,DDL");
        assert_eq!(unquote(""), "");
        assert_eq!(stamp(1_706_708_700_000), "2024-01-31 13:45:00.000");
    }
}

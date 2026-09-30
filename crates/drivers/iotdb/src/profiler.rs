//! The profiler ([`dbine_driver::profiler`]): IoTDB (and TimechoDB) only
//! show the queries running now (`SHOW QUERIES`: id, DataNode, seconds
//! elapsed, statement), so it's sampled. The answer has no start time over
//! REST and no database, user or client: the start is worked out when a
//! query is first seen, and the database from the paths the statement
//! names. Nothing is switched on.

use crate::IotDbSession;
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::Value as J;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// The profiler's own look, told apart from a user's `SHOW QUERIES` by its
/// limit. (A `WHERE` would do too, but IoTDB 1.3 then answers the columns
/// out of order over REST.)
const LOOK: &str = "SHOW QUERIES LIMIT 100000";

pub(crate) struct State {
    sampler: Sampler,
    /// Only statements that name this database (`root.x`), when set.
    database: String,
    /// When each query was first seen running.
    first_seen: HashMap<String, String>,
}

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

pub(crate) async fn start(s: &IotDbSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    s.query(LOOK, 10).await.map_err(|e| match e {
        Error::Query(m) => Error::Query(format!("El servidor no permite listar las consultas (SHOW QUERIES): {m}")),
        e => e,
    })?;
    let mut note = String::from(
        "IoTDB solo muestra las consultas en curso, sin usuario ni cliente: las que duran menos de una décima de \
         segundo pueden no verse.",
    );
    if !opts.database.is_empty() {
        note.push_str(&format!(" Se muestran las que nombran {} en sus rutas.", opts.database));
    }
    let state = State { sampler: Sampler::new(now()), database: opts.database.clone(), first_seen: HashMap::new() };
    Ok((state, ProfilerStarted::new(ProfilerMode::Sampled, "SHOW QUERIES").note(note)))
}

pub(crate) async fn poll(s: &IotDbSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let mut out = Vec::new();
    let until = Instant::now() + SAMPLE_FOR;
    loop {
        let t = s.query(LOOK, 10_000).await?;
        let col = |name: &str| t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name));
        let (ids, nodes, elapsed, statements) = (col("QueryId"), col("DataNodeId"), col("ElapsedTime"), col("Statement"));
        let get = |r: &[J], i: Option<usize>| i.and_then(|i| r.get(i)).cloned().unwrap_or(J::Null);
        let mut samples = Vec::new();
        for r in &t.rows {
            let text = get(r, statements).as_str().unwrap_or_default().to_string();
            let id = match get(r, ids) {
                J::String(id) => id,
                _ => continue,
            };
            if let Some(x) = sample(id, text, get(r, elapsed).as_f64(), get(r, nodes), state) {
                samples.push(x);
            }
        }
        let live: HashSet<&str> = samples.iter().map(|x| x.session.as_str()).collect();
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

/// One `SHOW QUERIES` row, except the profiler's own look and statements
/// that don't name the database.
fn sample(id: String, text: String, elapsed_s: Option<f64>, node: J, state: &mut State) -> Option<Sample> {
    if text.trim() == LOOK || text.trim().is_empty() {
        return None;
    }
    let database = (!state.database.is_empty()).then(|| state.database.clone());
    if let Some(db) = &database {
        if !names_database(&text, db) {
            return None;
        }
    }
    let ms = elapsed_s.map(|s| s * 1000.0);
    let started = state
        .first_seen
        .entry(id.clone())
        .or_insert_with(|| {
            let back = chrono::Duration::milliseconds(ms.unwrap_or(0.0) as i64);
            (chrono::Utc::now() - back).format("%Y-%m-%d %H:%M:%S%.3f").to_string()
        })
        .clone();
    let node = match node {
        J::Null => None,
        J::String(n) => Some(n),
        n => Some(n.to_string()),
    };
    Some(Sample {
        session: id,
        started,
        text,
        running: true,
        duration_ms: ms,
        database,
        detail: node.map(|n| format!("DataNode {n}")),
        ..Default::default()
    })
}

/// Whether a statement names `db` (`root.x`) as a whole path level:
/// `root.x.d1`, `root.x`, not `root.xy`.
fn names_database(text: &str, db: &str) -> bool {
    let (t, db) = (text.to_ascii_lowercase(), db.to_ascii_lowercase());
    t.match_indices(&db).any(|(i, _)| {
        let before = t[..i].chars().next_back();
        let after = t[i + db.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_name_databases() {
        assert!(names_database("SELECT s1 FROM root.sg.d1", "root.sg"));
        assert!(names_database("select count(*) from root.SG", "root.sg"));
        assert!(!names_database("SELECT s1 FROM root.sg2.d1", "root.sg"));
    }

    #[test]
    fn rows_become_samples() {
        let mut st = State { sampler: Sampler::new(""), database: "root.sg".into(), first_seen: HashMap::new() };
        let a = sample("q1".into(), "SELECT * FROM root.sg.d".into(), Some(0.25), J::from(1), &mut st).unwrap();
        assert_eq!((a.duration_ms, a.detail.as_deref()), (Some(250.0), Some("DataNode 1")));
        let b = sample("q1".into(), "SELECT * FROM root.sg.d".into(), Some(0.5), J::from(1), &mut st).unwrap();
        assert_eq!(a.started, b.started);
        assert!(sample("q2".into(), LOOK.into(), Some(0.0), J::Null, &mut st).is_none());
        assert!(sample("q3".into(), "SELECT * FROM root.other.d".into(), Some(0.0), J::Null, &mut st).is_none());
    }
}

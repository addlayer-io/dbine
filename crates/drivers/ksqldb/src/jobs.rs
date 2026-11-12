//! Server jobs ([`dbine_driver::Session::jobs`]): ksqlDB's persistent
//! queries (`CREATE … AS SELECT`, `INSERT INTO … SELECT`), which run on the
//! server until they're terminated, from `SHOW QUERIES EXTENDED`. Push
//! queries are clients' reads, not jobs: they stay in the process list.
//!
//! - Enabling and disabling is `RESUME` / `PAUSE` (ksqlDB 0.26 on, so
//!   Confluent Platform 7.3 on): a paused query keeps its state and its
//!   place in the input and goes on from there.
//! - Stopping is `TERMINATE`, which deletes the query for good: the stream
//!   or table it fed stays, with nothing writing to it. The job's tab says
//!   so.
//! - There's no "run now" (a persistent query is always running) and no
//!   history of runs; `queryErrors` (the latest errors the server kept) goes
//!   to the detail.
//! - The script is the statement the query was created with.
//!
//! Ids are query ids (`CTAS_ORDERS_7`, `INSERTQUERY_12`): one that comes
//! back has to be letters, digits, `_` and `-` (the shape of
//! [`crate::processes::valid_id`]) and name a persistent query the server
//! lists, before it goes into a statement.

use crate::processes::valid_id;
use crate::{text, KsqlSession};
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobStep, PropertyInfo, Result, ServerJob,
};
use serde_json::Value;
use std::time::Duration;

const KIND: &str = "Persistent query";
/// Longest a listing may take.
const LIST_LIMIT: Duration = Duration::from_secs(30);
/// Characters kept of a message.
const MAX_MESSAGE: usize = 4000;
/// Jobs listed at most.
const MAX_JOBS: usize = 5000;

const TERMINATE_NOTE: &str = "Detener ejecuta TERMINATE: la consulta se borra para siempre y el stream o la tabla que alimenta quedan sin quien escriba en ellos. Para frenarla un tiempo, deshabilitala (PAUSE).";
pub(crate) const NO_PAUSE_NOTE: &str = "Esta versión de ksqlDB no tiene PAUSE ni RESUME (llegaron en ksqlDB 0.26, Confluent Platform 7.3): las consultas no se pueden deshabilitar.";

/// The server can `PAUSE` / `RESUME` a query: ksqlDB 0.26 on, or
/// Confluent Platform 7.3 on (its versions are 5.x and up). An unknown
/// version is taken as able (the statement says if it isn't).
pub(crate) fn can_pause(version: &str) -> bool {
    let mut parts = version
        .trim()
        .trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit())
        .map(|p| p.parse::<u32>().ok());
    let (Some(Some(major)), Some(Some(minor))) = (parts.next(), parts.next()) else {
        return true;
    };
    match major {
        0 => minor >= 26,
        1..=4 => true,
        _ => (major, minor) >= (7, 3),
    }
}

/// What every host says of a query: the states in `ksqlHostQueryStatus`
/// (`state` only when it's missing, as it lags behind a pause).
fn states(q: &Value) -> Vec<String> {
    let hosts: Vec<String> = q
        .get("ksqlHostQueryStatus")
        .and_then(Value::as_object)
        .map(|m| m.values().map(text).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if !hosts.is_empty() {
        return hosts;
    }
    q.get("state")
        .map(text)
        .filter(|s| !s.is_empty())
        .into_iter()
        .collect()
}

fn list_of(q: &Value, key: &str) -> Vec<String> {
    q.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().map(text).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// The persistent queries of a `SHOW QUERIES EXTENDED` (or `EXPLAIN`)
/// reply.
pub(crate) fn descriptions(ents: &[Value]) -> Vec<&Value> {
    ents.iter()
        .flat_map(|e| {
            e.get("queryDescriptions")
                .and_then(Value::as_array)
                .map(|a| a.iter().collect::<Vec<_>>())
                .or_else(|| e.get("queryDescription").map(|d| vec![d]))
                .unwrap_or_default()
        })
        .filter(|q| q.get("queryType").map(text).as_deref() == Some("PERSISTENT"))
        .collect()
}

fn query_id(q: &Value) -> Option<String> {
    q.get("id")
        .map(|i| i.get("id").map(text).unwrap_or_else(|| text(i)))
        .filter(|s| valid_id(s))
}

/// One persistent query as a job.
pub(crate) fn job_of(q: &Value, pause: bool) -> Option<ServerJob> {
    let id = query_id(q)?;
    let states = states(q);
    let any = |s: &str| states.iter().any(|x| x == s);
    let paused = !states.is_empty() && states.iter().all(|s| s == "PAUSED");
    let running = any("RUNNING");
    let failed = any("ERROR");
    let sinks = list_of(q, "sinks");
    Some(ServerJob {
        name: id.clone(),
        kind: KIND.into(),
        group: (!sinks.is_empty()).then(|| sinks.join(", ")),
        description: None,
        owner: None,
        enabled: !paused,
        // It runs all the time, on no schedule.
        schedule: None,
        running,
        last_outcome: if failed {
            Some(JobOutcome::Failed)
        } else if running {
            Some(JobOutcome::Running)
        } else {
            None
        },
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: true,
        can_enable: pause,
        has_history: false,
        has_script: q
            .get("statementText")
            .map(text)
            .is_some_and(|s| !s.trim().is_empty()),
        system: false,
        id,
    })
}

fn cut(s: &str) -> String {
    s.chars().take(MAX_MESSAGE).collect()
}

fn prop(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: group.into(),
        label: label.into(),
        value: value.into(),
    }
}

/// A query's detail from its description.
pub(crate) fn detail_of(q: &Value, pause: bool) -> Option<JobDetail> {
    let job = job_of(q, pause)?;
    let mut properties = vec![prop("", "Id", job.id.clone())];
    let sources = list_of(q, "sources");
    if !sources.is_empty() {
        properties.push(prop("", "Lee de", sources.join(", ")));
    }
    let sinks = list_of(q, "sinks");
    if !sinks.is_empty() {
        properties.push(prop("", "Escribe en", sinks.join(", ")));
    }
    if let Some(g) = q.get("consumerGroupId").map(text).filter(|s| !s.is_empty()) {
        properties.push(prop("", "Grupo de consumidores", g));
    }
    if let Some(hosts) = q.get("ksqlHostQueryStatus").and_then(Value::as_object) {
        for (host, st) in hosts {
            properties.push(prop("Servidores", host, text(st)));
        }
    }
    if let Some(over) = q.get("overriddenProperties").and_then(Value::as_object) {
        for (k, v) in over {
            properties.push(prop("Propiedades", k, text(v)));
        }
    }
    let mut notes = vec![TERMINATE_NOTE.to_string()];
    if !pause {
        notes.push(NO_PAUSE_NOTE.into());
    }
    // The newest error the server kept, if any.
    let errors = q
        .get("queryErrors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(last) = errors.iter().max_by_key(|e| {
        e.get("timestamp")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MIN)
    }) {
        if let Some(m) = last.get("errorMessage").map(text).filter(|s| !s.is_empty()) {
            properties.push(prop("", "Último error", cut(&m)));
        }
        if let Some(ms) = last.get("timestamp").and_then(Value::as_i64) {
            properties.push(prop("", "Fecha del último error", utc_ms(ms)));
        }
    }
    Some(JobDetail {
        steps: vec![JobStep {
            name: "Consulta".into(),
            kind: Some("ksqlDB".into()),
            target: None,
            command: q.get("statementText").map(text).filter(|s| !s.is_empty()),
        }],
        schedules: Vec::new(),
        properties,
        notes,
        job,
    })
}

/// Milliseconds since 1970 as RFC 3339 in UTC.
pub(crate) fn utc_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn bad_id(id: &str) -> Error {
    Error::Query(format!(
        "«{}» no es un id de consulta persistente de ksqlDB",
        id.chars().take(200).collect::<String>()
    ))
}

fn not_found(id: &str) -> Error {
    Error::Query(format!("no existe la consulta persistente {id}"))
}

impl KsqlSession {
    pub(crate) async fn version(&self) -> String {
        match self.get_json("/info").await {
            Ok(v) => v
                .pointer("/KsqlServerInfo/version")
                .map(text)
                .unwrap_or_default(),
            Err(e) => {
                tracing::debug!("ksqlDB /info: {e}");
                String::new()
            }
        }
    }

    async fn descriptions(&mut self) -> Result<Vec<Value>> {
        let ents = tokio::time::timeout(LIST_LIMIT, self.ksql("SHOW QUERIES EXTENDED"))
            .await
            .map_err(|_| Error::Query("SHOW QUERIES EXTENDED no respondió a tiempo".into()))??;
        Ok(descriptions(&ents).into_iter().cloned().collect())
    }

    /// The description of the persistent query `id`, checked first.
    async fn description(&mut self, id: &str) -> Result<Value> {
        if !valid_id(id) {
            return Err(bad_id(id));
        }
        self.descriptions()
            .await?
            .into_iter()
            .find(|q| query_id(q).as_deref() == Some(id))
            .ok_or_else(|| not_found(id))
    }

    pub(crate) async fn server_jobs(&mut self) -> Result<JobList> {
        let pause = can_pause(&self.version().await);
        let mut jobs: Vec<ServerJob> = self
            .descriptions()
            .await?
            .iter()
            .filter_map(|q| job_of(q, pause))
            .collect();
        let mut notes = Vec::new();
        if !pause && !jobs.is_empty() {
            notes.push(NO_PAUSE_NOTE.to_string());
        }
        if jobs.len() > MAX_JOBS {
            jobs.truncate(MAX_JOBS);
            notes.push(format!(
                "Se muestran las primeras {MAX_JOBS} consultas persistentes."
            ));
        }
        Ok(JobList { jobs, notes })
    }

    pub(crate) async fn server_job_detail(&mut self, id: &str) -> Result<JobDetail> {
        let pause = can_pause(&self.version().await);
        let q = self.description(id).await?;
        detail_of(&q, pause).ok_or_else(|| not_found(id))
    }

    pub(crate) async fn stop_server_job(&mut self, id: &str) -> Result<()> {
        self.description(id).await?;
        // The id has the shape `valid_id` checked: letters, digits, `_`, `-`.
        self.ksql(&format!("TERMINATE {id}"))
            .await
            .map(|_| ())
            .map_err(|e| match e {
                Error::Query(m) => {
                    Error::Query(format!("no se pudo terminar la consulta {id}: {m}"))
                }
                e => e,
            })
    }

    pub(crate) async fn set_server_job_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        if !can_pause(&self.version().await) {
            return Err(Error::Unsupported(NO_PAUSE_NOTE.into()));
        }
        self.description(id).await?;
        let stmt = if enabled { "RESUME" } else { "PAUSE" };
        self.ksql(&format!("{stmt} {id}"))
            .await
            .map(|_| ())
            .map_err(|e| match e {
                Error::Query(m) if enabled => {
                    Error::Query(format!("no se pudo reanudar la consulta {id}: {m}"))
                }
                Error::Query(m) => Error::Query(format!("no se pudo pausar la consulta {id}: {m}")),
                e => e,
            })
    }

    pub(crate) async fn server_job_script(&mut self, id: &str) -> Result<String> {
        let q = self.description(id).await?;
        let sql = q
            .get("statementText")
            .map(text)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::Query(format!("ksqlDB no guardó la sentencia de la consulta {id}"))
            })?;
        let sql = sql.trim_end();
        Ok(if sql.ends_with(';') {
            format!("{sql}\n")
        } else {
            format!("{sql};\n")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply() -> Vec<Value> {
        vec![json!({"@type": "query_descriptions", "queryDescriptions": [
            {"id": "CTAS_T_7", "queryType": "PERSISTENT",
             "statementText": "CREATE TABLE T AS SELECT V, COUNT(*) C FROM S GROUP BY V EMIT CHANGES;",
             "sources": ["S"], "sinks": ["T"], "consumerGroupId": "_confluent-ksql-default_query_CTAS_T_7",
             "overriddenProperties": {"auto.offset.reset": "earliest"},
             "ksqlHostQueryStatus": {"h1:8088": "RUNNING"}, "state": "RUNNING", "queryErrors": []},
            // Paused: `state` still says RUNNING on 7.x.
            {"id": {"id": "CSAS_Y_4"}, "queryType": "PERSISTENT", "statementText": "CREATE STREAM Y AS SELECT * FROM S",
             "sinks": ["Y"], "ksqlHostQueryStatus": {"h1:8088": "PAUSED"}, "state": "RUNNING"},
            {"id": "CSAS_X_3", "queryType": "PERSISTENT", "statementText": "CREATE STREAM X AS SELECT * FROM S;",
             "sinks": ["X"], "state": "ERROR",
             "queryErrors": [
                {"errorMessage": "old", "timestamp": 1000, "type": "USER"},
                {"errorMessage": "deserialization failed", "timestamp": 1794499800000_i64, "type": "USER"}
             ]},
            {"id": "transient_S_42", "queryType": "PUSH", "statementText": "SELECT * FROM S EMIT CHANGES;"},
            {"id": "X; DROP STREAM S", "queryType": "PERSISTENT", "statementText": "CREATE STREAM Z AS SELECT * FROM S;"}
        ]})]
    }

    #[test]
    fn persistent_queries_become_jobs() {
        let ents = reply();
        let jobs: Vec<ServerJob> = descriptions(&ents)
            .into_iter()
            .filter_map(|q| job_of(q, true))
            .collect();
        // The push query and the one with a malformed id are left out.
        assert_eq!(
            jobs.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
            ["CTAS_T_7", "CSAS_Y_4", "CSAS_X_3"]
        );
        let (t, y, x) = (&jobs[0], &jobs[1], &jobs[2]);
        assert!(t.enabled && t.running && t.can_stop && t.can_enable && t.has_script);
        assert_eq!(t.last_outcome, Some(JobOutcome::Running));
        assert_eq!(t.group.as_deref(), Some("T"));
        assert!(!t.can_run && !t.has_history && t.schedule.is_none());
        assert!(!y.enabled && !y.running);
        assert_eq!(x.last_outcome, Some(JobOutcome::Failed));
        assert!(!jobs.iter().any(|j| j.system));
        // Without PAUSE, nothing can be disabled.
        assert!(!job_of(descriptions(&ents)[0], false).unwrap().can_enable);
    }

    #[test]
    fn detail_has_the_statement_the_errors_and_the_terminate_note() {
        let ents = reply();
        let d = detail_of(descriptions(&ents)[2], true).unwrap();
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some("CREATE STREAM X AS SELECT * FROM S;")
        );
        let p = |l: &str| {
            d.properties
                .iter()
                .find(|p| p.label == l)
                .map(|p| p.value.clone())
        };
        assert_eq!(p("Último error").as_deref(), Some("deserialization failed"));
        assert_eq!(
            p("Fecha del último error").as_deref(),
            Some("2026-11-12T16:10:00Z")
        );
        assert!(d.notes[0].contains("TERMINATE"));
        let d = detail_of(descriptions(&ents)[0], false).unwrap();
        assert!(d.notes.iter().any(|n| n.contains("PAUSE")));
        assert_eq!(p("Escribe en").as_deref(), Some("X"));
    }

    #[test]
    fn pause_needs_0_26_or_confluent_7_3() {
        assert!(can_pause("0.29.0"));
        assert!(can_pause("0.26.0"));
        assert!(!can_pause("0.23.1"));
        assert!(can_pause("7.7.1"));
        assert!(can_pause("7.3.0-ccs"));
        assert!(!can_pause("7.2.2"));
        assert!(!can_pause("6.2.0"));
        assert!(can_pause(""));
    }

    #[test]
    fn ids_have_one_shape() {
        assert!(valid_id("CTAS_T_7") && valid_id("INSERTQUERY_12"));
        for bad in ["", "X; DROP STREAM S", "CTAS T", "a'b", "q\n"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        assert!(bad_id("x; y").to_string().contains("no es un id"));
    }
}

//! What the phone asks and what DBine answers, as JSON over a [`Channel`].
//! Kept apart from the driver contract on purpose: the phone app doesn't
//! compile any of DBine's database code, and these types only carry what
//! the phone shows.
//!
//! [`Channel`]: crate::Channel

use serde::{Deserialize, Serialize};

/// After a pairing handshake, DBine asks its user and answers with this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairResult {
    pub accepted: bool,
    /// The computer's name.
    pub desktop: String,
    /// Why not, when refused ("rechazado en la computadora", "venció").
    #[serde(default)]
    pub reason: Option<String>,
}

/// A phone's request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum Call {
    /// What this phone may do, and the computer's name.
    Hello,
    /// The connections it can see.
    Connections,
    /// The databases of a connection.
    Databases { connection_id: String },
    /// Who waits for whom.
    Locks {
        connection_id: String,
        database: String,
    },
    /// The sessions running something.
    Processes {
        connection_id: String,
        database: String,
    },
    /// The server's figures: CPU, memory, connections… (the Monitor).
    Monitor { connection_id: String },
    /// End a session. The phone asks its user first (biometrics or PIN).
    Kill {
        connection_id: String,
        database: String,
        session_id: String,
    },
    /// What happened since the phone's last look: long queries that ended,
    /// task runs. `after: None` only asks where the list is; with another
    /// run's `boot`, all of this run's come back.
    Events {
        after: Option<u64>,
        boot: Option<String>,
    },
    /// The scheduled tasks, with their last run.
    Tasks,
    /// A task's last runs.
    TaskRuns { task_id: String },
    /// "Ejecutar ahora". The phone asks its user first.
    RunTask { task_id: String },
    /// A database's health check.
    Health {
        connection_id: String,
        database: String,
    },
    /// The editor's queries running now and the ones that ended lately.
    Queries,
    /// Cancel a running query (the phone asks its user first).
    CancelQuery { query_id: String },
    /// Cancel a running task run (the phone asks its user first).
    CancelTaskRun { run_id: String },
}

/// A query from DBine's editor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryRow {
    pub id: String,
    /// "Connection › database".
    pub place: String,
    /// Its first lines, trimmed.
    pub sql: String,
    pub started_at: String,
    /// How long it ran (or has been running).
    pub elapsed_ms: u64,
    /// Running, or how it ended.
    pub running: bool,
    pub ok: bool,
    /// "1240 filas", the error…
    pub summary: String,
    /// More of the statement, for the detail (trimmed to 2000 characters).
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueriesView {
    pub running: Vec<QueryRow>,
    pub recent: Vec<QueryRow>,
}

/// DBine's answer: `ok` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok<T: Serialize>(id: u64, v: &T) -> Response {
        match serde_json::to_value(v) {
            Ok(v) => Response {
                id,
                ok: Some(v),
                error: None,
            },
            Err(e) => Response::error(id, e.to_string()),
        }
    }

    pub fn error(id: u64, e: impl Into<String>) -> Response {
        Response {
            id,
            ok: None,
            error: Some(e.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub desktop: String,
    pub version: String,
    /// It may end sessions (Configuración › Dispositivos).
    pub can_kill: bool,
    /// It may run scheduled tasks (absent from a DBine before it).
    #[serde(default)]
    pub can_run_tasks: bool,
    /// How to reach DBine from outside the local network now: the phone
    /// keeps it up to date (its relays may have changed since pairing).
    #[serde(default)]
    pub remote: Option<crate::Remote>,
}

/// Something the phone notifies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRow {
    pub seq: u64,
    /// "query" or "task".
    pub kind: String,
    pub title: String,
    pub body: String,
    /// It ended well (no error, no alert).
    pub ok: bool,
    pub at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Events {
    pub events: Vec<EventRow>,
    /// The newest `seq` (0: none yet).
    pub last: u64,
    /// This DBine run: when it changes, `seq` started over.
    pub boot: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskItem {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub next_run: Option<String>,
    pub last: Option<RunRow>,
    /// It changes data and those steps changed since they were approved:
    /// it only runs after approving it on the computer.
    pub needs_approval: bool,
    /// Its steps' names, in order (to show where a running one is).
    #[serde(default)]
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRow {
    pub id: String,
    /// "running", "ok", "partial" or "failed".
    pub status: String,
    pub trigger: String,
    pub started_at: String,
    pub finished_at: String,
    pub error: Option<String>,
    pub steps: Vec<StepRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRow {
    pub kind: String,
    pub status: String,
    pub summary: String,
    pub alert: Option<String>,
}

/// One finding of a health check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthRow {
    pub category: String,
    pub title: String,
    /// "ok", "info", "warning" or "critical".
    pub severity: String,
    pub detail: Option<String>,
    pub objects: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthView {
    pub checks: Vec<HealthRow>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionItem {
    pub id: String,
    pub name: String,
    /// The engine's display name ("PostgreSQL").
    pub engine: String,
    pub color: Option<String>,
    /// The engine reports locks / processes / can end sessions.
    pub locks: bool,
    pub processes: bool,
    pub kill: bool,
    /// The engine has a Monitor (absent from a DBine before it).
    #[serde(default)]
    pub monitor: bool,
}

/// One figure of the Monitor. Labels and groups come in Spanish, as the
/// engine writes them: the phone translates them with its catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricRow {
    pub key: String,
    pub label: String,
    pub group: String,
    /// "percent", "bytes", "count", "millis" or "seconds".
    pub unit: String,
    pub value: Option<f64>,
    pub max: Option<f64>,
    /// A running total: the phone shows its rate per second.
    pub counter: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MonitorView {
    pub metrics: Vec<MetricRow>,
    /// (label, value): version, uptime, host…
    pub info: Vec<(String, String)>,
    /// What the engine can't report and why.
    pub notes: Vec<String>,
}

/// One session in a blocking chain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LockRow {
    pub id: String,
    pub blocked_by: Option<String>,
    pub user: Option<String>,
    pub client: Option<String>,
    pub database: Option<String>,
    pub wait: Option<String>,
    pub waited_ms: Option<u64>,
    pub object: Option<String>,
    pub sql: Option<String>,
}

/// One running session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessRow {
    pub id: String,
    pub status: Option<String>,
    pub user: Option<String>,
    pub host: Option<String>,
    pub database: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub blocked_by: Option<String>,
    pub sql: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r = Request {
            id: 3,
            call: Call::Kill {
                connection_id: "c".into(),
                database: "".into(),
                session_id: "53".into(),
            },
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(
            j,
            serde_json::json!({"id": 3, "call": "kill", "connection_id": "c", "database": "", "session_id": "53"})
        );
        assert_eq!(serde_json::from_value::<Request>(j).unwrap(), r);
        let hello: Request = serde_json::from_str(r#"{"id":1,"call":"hello"}"#).unwrap();
        assert_eq!(hello.call, Call::Hello);
        assert_eq!(
            serde_json::to_string(&Response::error(1, "no")).unwrap(),
            r#"{"id":1,"error":"no"}"#
        );
        // A call's fields sit next to the request's `id`: none may be called that.
        let c = Request {
            id: 9,
            call: Call::CancelQuery {
                query_id: "q".into(),
            },
        };
        assert_eq!(
            serde_json::from_str::<Request>(&serde_json::to_string(&c).unwrap()).unwrap(),
            c
        );
    }

    #[test]
    fn answers_carry_a_value_or_an_error() {
        let r = Response::ok(4, &QueriesView::default());
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({"id": 4, "ok": {"running": [], "recent": []}})
        );
        // A value JSON can't hold becomes the error.
        let mut bad = std::collections::HashMap::new();
        bad.insert(vec![1u8], 1);
        let r = Response::ok(5, &bad);
        assert_eq!((r.ok, r.error.is_some()), (None, true));
        // Neither: an empty answer still reads.
        let r: Response = serde_json::from_str(r#"{"id":6}"#).unwrap();
        assert_eq!((r.ok, r.error), (None, None));
    }
}

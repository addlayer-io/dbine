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
    Locks { connection_id: String, database: String },
    /// The sessions running something.
    Processes { connection_id: String, database: String },
    /// End a session. The phone asks its user first (biometrics or PIN).
    Kill { connection_id: String, database: String, session_id: String },
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
            Ok(v) => Response { id, ok: Some(v), error: None },
            Err(e) => Response::error(id, e.to_string()),
        }
    }

    pub fn error(id: u64, e: impl Into<String>) -> Response {
        Response { id, ok: None, error: Some(e.into()) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub desktop: String,
    pub version: String,
    /// It may end sessions (Configuración › Dispositivos).
    pub can_kill: bool,
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
        let r = Request { id: 3, call: Call::Kill { connection_id: "c".into(), database: "".into(), session_id: "53".into() } };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j, serde_json::json!({"id": 3, "call": "kill", "connection_id": "c", "database": "", "session_id": "53"}));
        assert_eq!(serde_json::from_value::<Request>(j).unwrap(), r);
        let hello: Request = serde_json::from_str(r#"{"id":1,"call":"hello"}"#).unwrap();
        assert_eq!(hello.call, Call::Hello);
        assert_eq!(serde_json::to_string(&Response::error(1, "no")).unwrap(), r#"{"id":1,"error":"no"}"#);
    }
}

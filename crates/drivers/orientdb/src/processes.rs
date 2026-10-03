//! The process list ([`dbine_driver::Session::processes`]), interrupting a
//! connection's command ([`dbine_driver::Session::cancel_query`]) and
//! closing a connection ([`dbine_driver::Session::kill_session`]).
//!
//! The monitor's `GET /server` lists every connection (HTTP and binary)
//! with its current command; the id is its `connectionId`.
//! `POST /connection/interrupt/<id>` interrupts the running command (as
//! Studio's "interrupt") and `POST /connection/kill/<id>` closes the
//! connection. Both need a server user (root). The connection reading
//! `/server` shows "Server status": that's DBine's own.

use crate::monitor::busy;
use crate::{as_text, OrientSession};
use dbine_driver::{Error, Result, ServerProcess};
use reqwest::Method;
use serde_json::Value;

/// Characters kept of a command.
const MAX_TEXT: usize = 20000;
const MAX_ROWS: usize = 2000;
/// `commandInfo` of the request reading `/server`.
const LISTING: &str = "Server status";

fn text(c: &Value, k: &str) -> Option<String> {
    c.get(k).map(as_text).map(|v| v.trim().to_string()).filter(|v| !v.is_empty() && v != "-")
}

fn ms(c: &Value, k: &str) -> Option<u64> {
    text(c, k).and_then(|v| v.parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

pub(crate) fn rows(server: &Value) -> Vec<ServerProcess> {
    let conns = server.get("connections").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut out: Vec<ServerProcess> = conns
        .iter()
        .filter_map(|c| {
            let id = text(c, "connectionId")?;
            let active = busy(c);
            let info = text(c, "commandInfo");
            let own = info.as_deref() == Some(LISTING);
            Some(ServerProcess {
                id,
                own,
                status: Some(info.clone().filter(|_| active || own).unwrap_or_else(|| "inactiva".into())),
                active,
                user: text(c, "user"),
                host: text(c, "remoteAddress").map(|a| a.trim_start_matches('/').to_string()),
                program: text(c, "driver").or_else(|| text(c, "protocol")),
                database: text(c, "db"),
                command: info.filter(|_| active),
                // The running command's time isn't reported; this is the last one's.
                elapsed_ms: if active { None } else { ms(c, "lastExecutionTime") },
                sql: text(c, "commandDetail").filter(|_| active).map(|d| d.chars().take(MAX_TEXT).collect()),
                ..Default::default()
            })
        })
        .collect();
    out.sort_by_key(|p| !p.active);
    out.truncate(MAX_ROWS);
    out
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 19 && id.chars().all(|c| c.is_ascii_digit())
}

impl OrientSession {
    pub(crate) async fn processes(&self) -> Result<Vec<ServerProcess>> {
        Ok(rows(&self.server().await?))
    }

    /// Interrupts the command (`interrupt`) or closes the connection (`kill`).
    pub(crate) async fn end_connection(&self, id: &str, action: &str) -> Result<()> {
        let kill = action == "kill";
        if self.read_only {
            let what = if kill { "terminar conexiones" } else { "cancelar comandos" };
            return Err(Error::Query(format!("Conexión de solo lectura: no se puede {what}.")));
        }
        let id = id.trim();
        if !valid_id(id) {
            return Err(Error::Query(format!("«{id}» no es un id de conexión (connectionId)")));
        }
        let server = self.server().await?;
        let conns = server.get("connections").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        let Some(c) = conns.iter().find(|c| text(c, "connectionId").as_deref() == Some(id)) else {
            return Err(Error::Query(format!("la conexión {id} ya se cerró o no existe")));
        };
        if text(c, "commandInfo").as_deref() == Some(LISTING) {
            return Err(Error::Query("esa es la conexión con la que DBine está consultando: no se puede cancelar desde acá".into()));
        }
        if !kill && !busy(c) {
            return Err(Error::Query(format!("la conexión {id} no está ejecutando nada")));
        }
        self.call(Method::POST, &format!("/connection/{action}/{id}"), None).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_from_server() {
        let s = json!({"connections": [
            {"connectionId": "1", "remoteAddress": "/10.0.0.1:5", "db": "-", "user": "-", "commandInfo": "Server status", "commandDetail": "-", "protocol": "http", "driver": ""},
            {"connectionId": "6", "remoteAddress": "/10.0.0.2:6", "db": "demo", "user": "root", "commandInfo": "Query", "commandDetail": "select from V", "protocol": "binary", "driver": "OrientDB Java"},
            {"connectionId": "7", "db": "demo", "commandInfo": "-", "lastExecutionTime": "12", "protocol": "http"}
        ]});
        let r = rows(&s);
        assert_eq!(r.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["6", "1", "7"]);
        assert!(r[0].active && !r[0].own);
        assert_eq!((r[0].sql.as_deref(), r[0].host.as_deref(), r[0].program.as_deref()), (Some("select from V"), Some("10.0.0.2:6"), Some("OrientDB Java")));
        assert!(r[1].own && r[1].user.is_none() && r[1].database.is_none());
        assert_eq!(r[1].status.as_deref(), Some("Server status"));
        assert_eq!((r[2].active, r[2].elapsed_ms, r[2].status.as_deref(), r[2].program.as_deref()), (false, Some(12), Some("inactiva"), Some("http")));
        assert!(valid_id("12") && !valid_id("1/../x") && !valid_id(""));
    }
}

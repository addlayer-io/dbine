//! What MCP clients did: one row per tool call, in a small SQLite file next
//! to the state (the newest ~10k are kept). Never holds secrets: the
//! summary is the tool's arguments without credentials (query text cut).

use rusqlite::{params, Connection};
use serde::Serialize;
use std::sync::Mutex;

/// How many calls the log keeps (the oldest go first).
const KEEP: i64 = 10_000;

#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    pub id: i64,
    pub at: String,
    pub client: String,
    /// The connection's name then ("" for calls about no connection).
    pub connection: String,
    pub tool: String,
    pub summary: String,
    pub ok: bool,
    pub rows: Option<u64>,
    pub error: Option<String>,
}

pub struct ActivityLog {
    conn: Mutex<Connection>,
}

impl ActivityLog {
    pub fn open(path: &std::path::Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> Self {
        Self::init(Connection::open_in_memory().expect("in-memory sqlite")).expect("activity schema")
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS activity (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 at         TEXT NOT NULL,
                 client     TEXT NOT NULL,
                 connection TEXT NOT NULL,
                 tool       TEXT NOT NULL,
                 summary    TEXT NOT NULL,
                 ok         INTEGER NOT NULL,
                 rows       INTEGER,
                 error      TEXT
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn record(&self, e: &ActivityEntry) {
        let c = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let r = c
            .execute(
                "INSERT INTO activity (at, client, connection, tool, summary, ok, rows, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![e.at, e.client, e.connection, e.tool, e.summary, e.ok, e.rows.map(|r| r as i64), e.error],
            )
            .and_then(|_| c.execute("DELETE FROM activity WHERE id <= (SELECT MAX(id) FROM activity) - ?1", [KEEP]));
        if let Err(e) = r {
            tracing::warn!(%e, "mcp: could not record activity");
        }
    }

    /// The newest calls first, optionally only a client's or a connection's.
    pub fn list(&self, client: Option<&str>, connection: Option<&str>, limit: u32) -> rusqlite::Result<Vec<ActivityEntry>> {
        let c = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = c.prepare(
            "SELECT id, at, client, connection, tool, summary, ok, rows, error FROM activity
              WHERE (?1 IS NULL OR client = ?1) AND (?2 IS NULL OR connection = ?2)
              ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![client, connection, limit.clamp(1, 1000)], |r| {
            Ok(ActivityEntry {
                id: r.get(0)?,
                at: r.get(1)?,
                client: r.get(2)?,
                connection: r.get(3)?,
                tool: r.get(4)?,
                summary: r.get(5)?,
                ok: r.get(6)?,
                rows: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                error: r.get(8)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_activity_filters_and_caps() {
        let log = ActivityLog::in_memory();
        for i in 0..5 {
            log.record(&ActivityEntry {
                id: 0,
                at: format!("t{i}"),
                client: if i % 2 == 0 { "Claude Code".into() } else { "Codex".into() },
                connection: "local".into(),
                tool: "list_objects".into(),
                summary: String::new(),
                ok: true,
                rows: Some(i),
                error: None,
            });
        }
        assert_eq!(log.list(None, None, 100).unwrap().len(), 5);
        assert_eq!(log.list(Some("Codex"), None, 100).unwrap().len(), 2);
        assert_eq!(log.list(None, Some("other"), 100).unwrap().len(), 0);
        assert_eq!(log.list(None, None, 100).unwrap()[0].at, "t4");
    }
}

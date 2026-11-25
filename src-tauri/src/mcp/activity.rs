//! What MCP clients did: one row per tool call, in a small SQLite file next
//! to the state (the newest ~10k are kept). Never holds secrets: the
//! summary is the tool's arguments without credentials (query text cut),
//! and `code` the query or code the call ran or asked to run, whole (SQL
//! text isn't sensitive; no rows are kept).
//!
//! Sessions: MCP has none (each call comes on its own), so a client's calls
//! one after the other, with no pause longer than [`SESSION_GAP`], are one
//! session: what an agent did, which the UI opens as a notebook.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::sync::Mutex;

/// How many calls the log keeps (the oldest go first).
const KEEP: i64 = 10_000;
/// A query or code longer than this (bytes) isn't kept whole: the summary
/// says what it was.
pub const CODE_MAX: usize = 64 * 1024;
/// A pause longer than this ends a client's session.
pub const SESSION_GAP: chrono::Duration = chrono::Duration::minutes(30);

#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    pub id: i64,
    pub at: String,
    pub client: String,
    /// The connection's name then ("" for calls about no connection).
    pub connection: String,
    /// The database the call named ("" when none).
    pub database: String,
    pub tool: String,
    pub summary: String,
    /// The query (`run_query`, `explain`) or code (`execute`), whole; `None`
    /// for other tools or past [`CODE_MAX`].
    pub code: Option<String>,
    pub ok: bool,
    pub rows: Option<u64>,
    pub error: Option<String>,
}

/// A client's calls with no pause longer than [`SESSION_GAP`]: what an
/// agent did, from `first_id` to `last_id`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Session {
    pub client: String,
    pub first_id: i64,
    pub last_id: i64,
    pub start: String,
    pub end: String,
    /// The connections it used, in the order it first did.
    pub connections: Vec<String>,
    pub calls: u32,
    /// Reads of rows that ran (`run_query`, `explain`, `sample_rows`).
    pub reads: u32,
    /// Writes that ran (`execute`, approved).
    pub writes: u32,
    /// Reads and writes the user rejected or left unanswered.
    pub rejected: u32,
    /// Calls that failed.
    pub errors: u32,
}

/// The step part of a logged tool (`run_query:enforced` → `enforced`).
fn phase(tool: &str) -> Option<&str> {
    tool.split_once(':').map(|(_, p)| p)
}

fn base(tool: &str) -> &str {
    tool.split_once(':').map_or(tool, |(b, _)| b)
}

pub struct ActivityLog {
    conn: Mutex<Connection>,
}

impl ActivityLog {
    pub fn open(path: &std::path::Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> Self {
        Self::init(Connection::open_in_memory().expect("in-memory sqlite"))
            .expect("activity schema")
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
        // Logs from before sessions: the database and the code, added.
        let have: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('activity')")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if !have.iter().any(|c| c == "database") {
            conn.execute_batch(
                "ALTER TABLE activity ADD COLUMN database TEXT NOT NULL DEFAULT ''",
            )?;
        }
        if !have.iter().any(|c| c == "code") {
            conn.execute_batch("ALTER TABLE activity ADD COLUMN code TEXT")?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn record(&self, e: &ActivityEntry) {
        let c = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let r = c
            .execute(
                "INSERT INTO activity (at, client, connection, database, tool, summary, code, ok, rows, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    e.at,
                    e.client,
                    e.connection,
                    e.database,
                    e.tool,
                    e.summary,
                    e.code.as_deref().filter(|c| c.len() <= CODE_MAX),
                    e.ok,
                    e.rows.map(|r| r as i64),
                    e.error
                ],
            )
            .and_then(|_| c.execute("DELETE FROM activity WHERE id <= (SELECT MAX(id) FROM activity) - ?1", [KEEP]));
        if let Err(e) = r {
            tracing::warn!(%e, "mcp: could not record activity");
        }
    }

    /// The newest calls first, optionally only a client's or a connection's.
    pub fn list(
        &self,
        client: Option<&str>,
        connection: Option<&str>,
        limit: u32,
    ) -> rusqlite::Result<Vec<ActivityEntry>> {
        let c = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = c.prepare(&format!(
            "SELECT {COLUMNS} FROM activity
              WHERE (?1 IS NULL OR client = ?1) AND (?2 IS NULL OR connection = ?2)
              ORDER BY id DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![client, connection, limit.clamp(1, 1000)], entry)?;
        rows.collect()
    }

    /// A client's calls from `first_id` to `last_id`, oldest first: a
    /// session's, for its notebook.
    pub fn calls(
        &self,
        client: &str,
        first_id: i64,
        last_id: i64,
    ) -> rusqlite::Result<Vec<ActivityEntry>> {
        let c = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = c.prepare(&format!(
            "SELECT {COLUMNS} FROM activity
              WHERE client = ?1 AND id BETWEEN ?2 AND ?3 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![client, first_id, last_id], entry)?;
        rows.collect()
    }

    /// The sessions, newest first: every client's (or `client`'s), the ones
    /// that used `connection` when given.
    pub fn sessions(
        &self,
        client: Option<&str>,
        connection: Option<&str>,
        limit: u32,
    ) -> rusqlite::Result<Vec<Session>> {
        let all = {
            let c = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            let mut stmt = c.prepare(&format!(
                "SELECT {COLUMNS} FROM activity WHERE (?1 IS NULL OR client = ?1) ORDER BY id"
            ))?;
            let rows = stmt.query_map(params![client], entry)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out: Vec<Session> = group(&all, SESSION_GAP)
            .into_iter()
            .filter(|s| connection.is_none_or(|c| s.connections.iter().any(|x| x == c)))
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.last_id));
        out.truncate(limit.clamp(1, 1000) as usize);
        Ok(out)
    }
}

const COLUMNS: &str = "id, at, client, connection, database, tool, summary, code, ok, rows, error";

fn entry(r: &rusqlite::Row) -> rusqlite::Result<ActivityEntry> {
    Ok(ActivityEntry {
        id: r.get(0)?,
        at: r.get(1)?,
        client: r.get(2)?,
        connection: r.get(3)?,
        database: r.get(4)?,
        tool: r.get(5)?,
        summary: r.get(6)?,
        code: r.get(7)?,
        ok: r.get(8)?,
        rows: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        error: r.get(10)?,
    })
}

fn at(e: &ActivityEntry) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(&e.at).ok()
}

/// `entries` (oldest first) as sessions: per client, a new one after a
/// pause longer than `gap` (a time that can't be read doesn't end one).
pub fn group(entries: &[ActivityEntry], gap: chrono::Duration) -> Vec<Session> {
    let mut open: std::collections::HashMap<&str, usize> = Default::default();
    let mut out: Vec<Session> = Vec::new();
    let mut last_at: Vec<Option<chrono::DateTime<chrono::FixedOffset>>> = Vec::new();
    for e in entries {
        let now = at(e);
        let current = open
            .get(e.client.as_str())
            .copied()
            .filter(|&i| match (last_at[i], now) {
                (Some(prev), Some(now)) => now - prev <= gap,
                _ => true,
            });
        let i = match current {
            Some(i) => i,
            None => {
                out.push(Session {
                    client: e.client.clone(),
                    first_id: e.id,
                    last_id: e.id,
                    start: e.at.clone(),
                    end: e.at.clone(),
                    connections: Vec::new(),
                    calls: 0,
                    reads: 0,
                    writes: 0,
                    rejected: 0,
                    errors: 0,
                });
                last_at.push(None);
                open.insert(e.client.as_str(), out.len() - 1);
                out.len() - 1
            }
        };
        let s = &mut out[i];
        s.last_id = e.id;
        s.end = e.at.clone();
        if now.is_some() {
            last_at[i] = now;
        }
        if !e.connection.is_empty() && !s.connections.contains(&e.connection) {
            s.connections.push(e.connection.clone());
        }
        let step = phase(&e.tool);
        if step == Some("request") {
            continue;
        }
        s.calls += 1;
        match (base(&e.tool), step) {
            (_, Some("rejected" | "timeout")) => s.rejected += 1,
            _ if !e.ok => s.errors += 1,
            ("execute", _) => s.writes += 1,
            ("run_query" | "explain" | "sample_rows", _) => s.reads += 1,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(at: &str, client: &str, connection: &str, tool: &str, ok: bool) -> ActivityEntry {
        ActivityEntry {
            id: 0,
            at: at.into(),
            client: client.into(),
            connection: connection.into(),
            database: "ventas".into(),
            tool: tool.into(),
            summary: String::new(),
            code: None,
            ok,
            rows: None,
            error: (!ok).then(|| "boom".into()),
        }
    }

    #[test]
    fn mcp_activity_filters_and_caps() {
        let log = ActivityLog::in_memory();
        for i in 0..5 {
            let mut e = call(
                &format!("t{i}"),
                if i % 2 == 0 { "Claude Code" } else { "Codex" },
                "local",
                "list_objects",
                true,
            );
            e.rows = Some(i);
            log.record(&e);
        }
        assert_eq!(log.list(None, None, 100).unwrap().len(), 5);
        assert_eq!(log.list(Some("Codex"), None, 100).unwrap().len(), 2);
        assert_eq!(log.list(None, Some("other"), 100).unwrap().len(), 0);
        assert_eq!(log.list(None, None, 100).unwrap()[0].at, "t4");
    }

    #[test]
    fn the_whole_query_and_its_database_are_kept_up_to_the_cap() {
        let log = ActivityLog::in_memory();
        let mut e = call(
            "2026-11-25T10:00:00Z",
            "Codex",
            "local",
            "run_query:enforced",
            true,
        );
        let long = format!("SELECT '{}'", "x".repeat(300));
        e.code = Some(long.clone());
        log.record(&e);
        e.code = Some("y".repeat(CODE_MAX + 1));
        log.record(&e);
        let got = log.list(None, None, 10).unwrap();
        assert_eq!(
            got[1].code.as_deref(),
            Some(long.as_str()),
            "longer than the summary's 200"
        );
        assert_eq!(got[1].database, "ventas");
        assert_eq!(got[0].code, None, "past the cap: not kept");
    }

    #[test]
    fn a_log_from_before_sessions_gets_the_new_columns() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE activity (id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT NOT NULL, client TEXT NOT NULL,
               connection TEXT NOT NULL, tool TEXT NOT NULL, summary TEXT NOT NULL, ok INTEGER NOT NULL,
               rows INTEGER, error TEXT);
             INSERT INTO activity (at, client, connection, tool, summary, ok) VALUES ('t0', 'Codex', 'local', 'list_objects', 'ventas', 1);",
        )
        .unwrap();
        let log = ActivityLog::init(c).unwrap();
        let old = log.list(None, None, 10).unwrap();
        assert_eq!(
            (old[0].database.as_str(), old[0].code.as_deref()),
            ("", None)
        );
        let mut e = call("t1", "Codex", "local", "run_query:enforced", true);
        e.code = Some("SELECT 1".into());
        log.record(&e);
        assert_eq!(
            log.list(None, None, 10).unwrap()[0].code.as_deref(),
            Some("SELECT 1")
        );
    }

    #[test]
    fn a_pause_longer_than_the_gap_starts_another_session_per_client() {
        let log = ActivityLog::in_memory();
        for e in [
            call(
                "2026-11-25T10:00:00Z",
                "Claude Code",
                "ventas",
                "list_objects",
                true,
            ),
            call(
                "2026-11-25T10:05:00Z",
                "Codex",
                "otra",
                "run_query:enforced",
                true,
            ),
            call(
                "2026-11-25T10:20:00Z",
                "Claude Code",
                "ventas",
                "run_query:enforced",
                true,
            ),
            call(
                "2026-11-25T10:21:00Z",
                "Claude Code",
                "ventas",
                "execute:request",
                true,
            ),
            call(
                "2026-11-25T10:22:00Z",
                "Claude Code",
                "ventas",
                "execute:approved",
                true,
            ),
            call(
                "2026-11-25T10:23:00Z",
                "Claude Code",
                "dw",
                "run_query:rejected",
                false,
            ),
            call(
                "2026-11-25T10:24:00Z",
                "Claude Code",
                "dw",
                "run_query",
                false,
            ),
            // 31 minutes later: another session.
            call(
                "2026-11-25T10:55:00Z",
                "Claude Code",
                "ventas",
                "describe_object",
                true,
            ),
        ] {
            log.record(&e);
        }
        let s = log.sessions(None, None, 10).unwrap();
        assert_eq!(s.len(), 3, "{s:#?}");
        // Newest first.
        assert_eq!(
            (s[0].client.as_str(), s[0].first_id, s[0].last_id),
            ("Claude Code", 8, 8)
        );
        let first = s
            .iter()
            .find(|x| x.client == "Claude Code" && x.first_id == 1)
            .unwrap();
        assert_eq!(first.last_id, 7);
        assert_eq!(first.connections, ["ventas", "dw"]);
        assert_eq!(
            (
                first.calls,
                first.reads,
                first.writes,
                first.rejected,
                first.errors
            ),
            (5, 1, 1, 1, 1),
            "the request isn't a call; the rest counted once"
        );
        assert_eq!(first.start, "2026-11-25T10:00:00Z");
        assert_eq!(first.end, "2026-11-25T10:24:00Z");
        let codex = s.iter().find(|x| x.client == "Codex").unwrap();
        assert_eq!(
            (codex.first_id, codex.last_id, codex.reads),
            (2, 2, 1),
            "another client's call between them doesn't cut"
        );
        // Filters.
        assert_eq!(log.sessions(Some("Codex"), None, 10).unwrap().len(), 1);
        assert_eq!(log.sessions(None, Some("dw"), 10).unwrap().len(), 1);
        assert_eq!(log.sessions(None, None, 1).unwrap().len(), 1);
        // A session's calls, oldest first, only that client's.
        let calls = log.calls("Claude Code", 1, 7).unwrap();
        assert_eq!(
            calls.iter().map(|c| c.id).collect::<Vec<_>>(),
            [1, 3, 4, 5, 6, 7]
        );
    }

    #[test]
    fn a_time_that_cant_be_read_doesnt_end_a_session() {
        let entries = [
            ActivityEntry {
                id: 1,
                ..call("2026-11-25T10:00:00Z", "A", "", "list_connections", true)
            },
            ActivityEntry {
                id: 2,
                ..call("garbage", "A", "", "list_connections", true)
            },
            ActivityEntry {
                id: 3,
                ..call("2026-11-25T10:29:00Z", "A", "", "list_connections", true)
            },
        ];
        let s = group(&entries, SESSION_GAP);
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].calls, s[0].connections.len()), (3, 0));
    }
}

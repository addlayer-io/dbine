//! The explorer's cache (`cache.db`, apart from the state store): the last
//! databases, objects and columns each connection showed, so reopening one
//! shows its tree at once while the server is asked again. The server's
//! answer always replaces what's here. Only names and structure: no rows,
//! no secrets. Losing the file loses nothing: it fills again.

use crate::{Error, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

/// What a cached entry holds.
pub mod kinds {
    /// The connection's databases (`database` is "").
    pub const DATABASES: &str = "databases";
    /// A database's objects.
    pub const OBJECTS: &str = "objects";
    /// One object's columns (`item` names the object).
    pub const COLUMNS: &str = "columns";
}

pub struct ExplorerCache {
    conn: Mutex<Connection>,
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::State(e.to_string())
}

impl ExplorerCache {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path).map_err(db_err)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(db_err)?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS explorer (
               connection_id TEXT NOT NULL,
               database TEXT NOT NULL,
               kind TEXT NOT NULL,
               item TEXT NOT NULL,
               payload TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               PRIMARY KEY (connection_id, database, kind, item)
             );",
        )
        .map_err(db_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// The last answer stored for this entry (JSON), if any.
    pub fn get(&self, connection_id: &str, database: &str, kind: &str, item: &str) -> Result<Option<String>> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT payload FROM explorer WHERE connection_id = ?1 AND database = ?2 AND kind = ?3 AND item = ?4",
                params![connection_id, database, kind, item],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)
    }

    /// Store the server's answer, replacing the previous one.
    pub fn put(&self, connection_id: &str, database: &str, kind: &str, item: &str, payload: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO explorer (connection_id, database, kind, item, payload, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (connection_id, database, kind, item)
                 DO UPDATE SET payload = excluded.payload, updated_at = excluded.updated_at",
                params![connection_id, database, kind, item, payload, chrono::Utc::now().to_rfc3339()],
            )
            .map(|_| ())
            .map_err(db_err)
    }

    /// Forget one entry (an object that no longer exists).
    pub fn remove(&self, connection_id: &str, database: &str, kind: &str, item: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM explorer WHERE connection_id = ?1 AND database = ?2 AND kind = ?3 AND item = ?4",
                params![connection_id, database, kind, item],
            )
            .map(|_| ())
            .map_err(db_err)
    }

    /// Forget everything about a connection (deleted, or pointed elsewhere).
    pub fn forget_connection(&self, connection_id: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM explorer WHERE connection_id = ?1", params![connection_id])
            .map(|_| ())
            .map_err(db_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_answer_per_entry() {
        let c = ExplorerCache::open_in_memory().unwrap();
        assert_eq!(c.get("c1", "", kinds::DATABASES, "").unwrap(), None);
        c.put("c1", "", kinds::DATABASES, "", r#"["a"]"#).unwrap();
        c.put("c1", "", kinds::DATABASES, "", r#"["a","b"]"#).unwrap();
        c.put("c1", "a", kinds::OBJECTS, "", "[]").unwrap();
        c.put("c2", "", kinds::DATABASES, "", r#"["x"]"#).unwrap();
        assert_eq!(c.get("c1", "", kinds::DATABASES, "").unwrap().as_deref(), Some(r#"["a","b"]"#));
        c.remove("c1", "a", kinds::OBJECTS, "").unwrap();
        assert_eq!(c.get("c1", "a", kinds::OBJECTS, "").unwrap(), None);
        c.forget_connection("c1").unwrap();
        assert_eq!(c.get("c1", "", kinds::DATABASES, "").unwrap(), None);
        assert_eq!(c.get("c2", "", kinds::DATABASES, "").unwrap().as_deref(), Some(r#"["x"]"#));
    }
}

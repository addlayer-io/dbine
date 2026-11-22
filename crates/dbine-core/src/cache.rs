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
    /// A database's lineage graph for a scope (`item` is the scope's canonical key).
    pub const LINEAGE: &str = "lineage";
    /// A Power BI model's external sources (`database` is "", `item` names the model).
    pub const LINEAGE_SOURCES: &str = "lineage_sources";
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
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The last answer stored for this entry (JSON), if any.
    pub fn get(
        &self,
        connection_id: &str,
        database: &str,
        kind: &str,
        item: &str,
    ) -> Result<Option<String>> {
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

    /// The last answer stored for this entry (JSON) with the moment it was stored (RFC 3339).
    pub fn get_with_time(
        &self,
        connection_id: &str,
        database: &str,
        kind: &str,
        item: &str,
    ) -> Result<Option<(String, String)>> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT payload, updated_at FROM explorer WHERE connection_id = ?1 AND database = ?2 AND kind = ?3 AND item = ?4",
                params![connection_id, database, kind, item],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_err)
    }

    /// Every entry of these kinds stored for a database (JSON), newest
    /// first: the explorer's filter searches them by name.
    pub fn payloads(
        &self,
        connection_id: &str,
        database: &str,
        kinds: &[&str],
    ) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM explorer WHERE connection_id = ?1 AND database = ?2 AND kind = ?3
                 ORDER BY updated_at DESC",
            )
            .map_err(db_err)?;
        for kind in kinds {
            let rows = stmt
                .query_map(params![connection_id, database, kind], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(db_err)?;
            for row in rows {
                out.push(row.map_err(db_err)?);
            }
        }
        Ok(out)
    }

    /// Store the server's answer, replacing the previous one.
    pub fn put(
        &self,
        connection_id: &str,
        database: &str,
        kind: &str,
        item: &str,
        payload: &str,
    ) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO explorer (connection_id, database, kind, item, payload, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (connection_id, database, kind, item)
                 DO UPDATE SET payload = excluded.payload, updated_at = excluded.updated_at",
                params![
                    connection_id,
                    database,
                    kind,
                    item,
                    payload,
                    chrono::Utc::now().to_rfc3339()
                ],
            )
            .map(|_| ())
            .map_err(db_err)
    }

    /// Forget one entry (an object that no longer exists).
    pub fn remove(
        &self,
        connection_id: &str,
        database: &str,
        kind: &str,
        item: &str,
    ) -> Result<()> {
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

    /// Forget the entries of these kinds only (the explorer's own entries stay): of one connection
    /// (`None`: every connection) and, when given, of one database.
    pub fn remove_kinds(
        &self,
        connection_id: Option<&str>,
        database: Option<&str>,
        kinds: &[&str],
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for kind in kinds {
            conn.execute(
                "DELETE FROM explorer WHERE kind = ?1
                 AND (?2 IS NULL OR connection_id = ?2) AND (?3 IS NULL OR database = ?3)",
                params![kind, connection_id, database],
            )
            .map_err(db_err)?;
        }
        Ok(())
    }

    /// Forget everything about a connection (deleted, or pointed elsewhere).
    pub fn forget_connection(&self, connection_id: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM explorer WHERE connection_id = ?1",
                params![connection_id],
            )
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
        c.put("c1", "", kinds::DATABASES, "", r#"["a","b"]"#)
            .unwrap();
        c.put("c1", "a", kinds::OBJECTS, "", "[]").unwrap();
        c.put("c2", "", kinds::DATABASES, "", r#"["x"]"#).unwrap();
        assert_eq!(
            c.get("c1", "", kinds::DATABASES, "").unwrap().as_deref(),
            Some(r#"["a","b"]"#)
        );
        c.remove("c1", "a", kinds::OBJECTS, "").unwrap();
        assert_eq!(c.get("c1", "a", kinds::OBJECTS, "").unwrap(), None);
        c.forget_connection("c1").unwrap();
        assert_eq!(c.get("c1", "", kinds::DATABASES, "").unwrap(), None);
        assert_eq!(
            c.get("c2", "", kinds::DATABASES, "").unwrap().as_deref(),
            Some(r#"["x"]"#)
        );
    }

    #[test]
    fn reads_an_entry_with_its_time() {
        let c = ExplorerCache::open_in_memory().unwrap();
        assert_eq!(
            c.get_with_time("c1", "a", kinds::LINEAGE, "k").unwrap(),
            None
        );
        c.put("c1", "a", kinds::LINEAGE, "k", "{}").unwrap();
        let (payload, at) = c
            .get_with_time("c1", "a", kinds::LINEAGE, "k")
            .unwrap()
            .unwrap();
        assert_eq!(payload, "{}");
        assert!(chrono::DateTime::parse_from_rfc3339(&at).is_ok());
    }

    #[test]
    fn clears_lineage_kinds_without_touching_the_explorer() {
        let c = ExplorerCache::open_in_memory().unwrap();
        for conn in ["c1", "c2"] {
            c.put(conn, "a", kinds::OBJECTS, "", "[1]").unwrap();
            c.put(conn, "a", kinds::LINEAGE, "k", "{}").unwrap();
            c.put(conn, "b", kinds::LINEAGE, "k", "{}").unwrap();
            c.put(conn, "", kinds::LINEAGE_SOURCES, "m", "[]").unwrap();
        }
        let both = [kinds::LINEAGE, kinds::LINEAGE_SOURCES];
        // One database of one connection.
        c.remove_kinds(Some("c1"), Some("a"), &[kinds::LINEAGE])
            .unwrap();
        assert_eq!(c.get("c1", "a", kinds::LINEAGE, "k").unwrap(), None);
        assert!(c.get("c1", "b", kinds::LINEAGE, "k").unwrap().is_some());
        // One connection.
        c.remove_kinds(Some("c1"), None, &both).unwrap();
        assert_eq!(c.get("c1", "b", kinds::LINEAGE, "k").unwrap(), None);
        assert_eq!(c.get("c1", "", kinds::LINEAGE_SOURCES, "m").unwrap(), None);
        assert!(c.get("c2", "a", kinds::LINEAGE, "k").unwrap().is_some());
        // Every connection.
        c.remove_kinds(None, None, &both).unwrap();
        assert_eq!(c.get("c2", "a", kinds::LINEAGE, "k").unwrap(), None);
        // The explorer's own entries survive all of it.
        assert!(c.get("c1", "a", kinds::OBJECTS, "").unwrap().is_some());
        assert!(c.get("c2", "a", kinds::OBJECTS, "").unwrap().is_some());
    }

    #[test]
    fn lists_a_databases_entries_of_some_kinds() {
        let c = ExplorerCache::open_in_memory().unwrap();
        c.put("c1", "a", "page", "tables", "[1]").unwrap();
        c.put("c1", "a", "page", "views", "[2]").unwrap();
        c.put("c1", "a", kinds::OBJECTS, "", "[3]").unwrap();
        c.put("c1", "a", kinds::COLUMNS, "t", "[4]").unwrap();
        c.put("c1", "b", "page", "tables", "[5]").unwrap();
        let mut got = c.payloads("c1", "a", &["page", kinds::OBJECTS]).unwrap();
        got.sort();
        assert_eq!(got, ["[1]", "[2]", "[3]"]);
        assert!(c.payloads("c2", "a", &["page"]).unwrap().is_empty());
    }
}

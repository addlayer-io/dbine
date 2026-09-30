//! Local state: connection folders, saved connections and saved queries, in
//! one SQLite file in the app's config directory. Synchronous (rusqlite behind a mutex): every
//! call is a small indexed read or write.

use dbine_driver::{ConnectionConfig, Error, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedConnection {
    pub id: String,
    pub name: String,
    /// Accent color shown next to the connection (CSS color), if any.
    #[serde(default)]
    pub color: Option<String>,
    /// Password stripped: it's in the keychain when `save_password`.
    pub config: ConnectionConfig,
    #[serde(default)]
    pub save_password: bool,
    /// The explorer folder it's in; `None` = top level.
    #[serde(default)]
    pub folder_id: Option<String>,
    /// Free labels to manage many connections (`prod`, `dev`, `qa`…),
    /// shown in the explorer and usable to filter it.
    #[serde(default)]
    pub tags: Vec<String>,
    /// What MCP clients may do with it (`disabled`, `schema`, `read`,
    /// `write`); `None` = the global default (docs/mcp.md).
    #[serde(default)]
    pub mcp_level: Option<String>,
    #[serde(default)]
    pub updated_at: String,
}

/// An explorer folder grouping connections (per client, per environment…).
/// Folders nest; a folder's color applies to connections without their own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionFolder {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

/// A query kept under a database in the explorer.
/// A statement run from the editor (the history sidebar). Local to this
/// machine: not in the cloud backup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub connection_id: String,
    /// The connection's name then (it may be renamed or deleted since).
    pub connection_name: String,
    pub driver: String,
    /// Where it ran: the server's host (a file's name for embedded engines).
    pub host: String,
    pub database: String,
    pub sql: String,
    pub started_at: String,
    pub duration_ms: u64,
    /// Rows returned or affected, when the engine says.
    pub rows: Option<u64>,
    pub error: Option<String>,
}

/// How many statements the history keeps (the oldest go first).
const HISTORY_MAX: i64 = 20_000;

/// A copy DBine made of a database: a script with its structure (and its
/// data) in a local file (the Backups tab). Local to this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupCopy {
    pub id: String,
    pub connection_id: String,
    pub database: String,
    pub path: String,
    pub created_at: String,
    pub size: u64,
    pub objects: u64,
    pub rows: u64,
    /// It carries the data, not only the structure.
    pub data: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedQuery {
    pub id: String,
    pub connection_id: String,
    pub database: String,
    pub name: String,
    pub sql: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub last_run_at: Option<String>,
}

/// A migration kept under its source database in the explorer ("Migraciones"): the Migrate
/// screen's configuration (drafts are auto-saved) and the runs started from it. The config is
/// the UI's own document (opaque here); the runs live in the migration records
/// (`dbine-transfer.sqlite` and the runs' files), this only keeps their ids.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SavedMigration {
    pub id: String,
    pub connection_id: String,
    pub database: String,
    pub name: String,
    #[serde(default)]
    pub config: serde_json::Value,
    /// Runs started from it, oldest first (the last one is the current one). Machine-local ids:
    /// on another machine (after a sync) they may not exist.
    #[serde(default)]
    pub run_ids: Vec<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

/// A reusable script of the Library: not tied to a database but to the
/// engines it's written for (a DBA's "reindex a table", "blocking
/// sessions"…). Opening it copies it into a query of the active database.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LibraryScript {
    pub id: String,
    pub name: String,
    /// Folder path in the Library ("Mantenimiento/Índices"); empty = root.
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub description: String,
    /// Driver ids it's for; `*sql` = any SQL engine.
    #[serde(default)]
    pub engines: Vec<String>,
    pub text: String,
    #[serde(default)]
    pub updated_at: String,
}

/// Everything the user keeps in the IDE, as one document: what a cloud
/// backup carries (secrets aside, which live in the keychain).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub connections: Vec<SavedConnection>,
    pub folders: Vec<ConnectionFolder>,
    pub queries: Vec<SavedQuery>,
    /// User preferences (`list_settings`), synced across machines.
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
    /// The script Library.
    #[serde(default)]
    pub library: Vec<LibraryScript>,
    /// Saved migrations (their configuration; the runs they link stay on the machine that ran them).
    #[serde(default)]
    pub migrations: Vec<SavedMigration>,
}

pub struct StateStore {
    conn: Mutex<Connection>,
}

/// Settings under this prefix belong to this machine (the sync engine's
/// own bookkeeping): they don't count as changes and never leave it.
pub const LOCAL_PREFIX: &str = "local.";

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::State(e.to_string())
}

impl StateStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path).map_err(db_err)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(db_err)?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS connections (
                 id            TEXT PRIMARY KEY,
                 name          TEXT NOT NULL,
                 color         TEXT,
                 config_json   TEXT NOT NULL,
                 save_password INTEGER NOT NULL DEFAULT 0,
                 sort_order    INTEGER NOT NULL DEFAULT 0,
                 created_at    TEXT NOT NULL,
                 updated_at    TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS queries (
                 id            TEXT PRIMARY KEY,
                 connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
                 database      TEXT NOT NULL,
                 name          TEXT NOT NULL,
                 sql           TEXT NOT NULL DEFAULT '',
                 created_at    TEXT NOT NULL,
                 updated_at    TEXT NOT NULL,
                 last_run_at   TEXT
             );
             CREATE INDEX IF NOT EXISTS queries_by_db ON queries(connection_id, database);
             CREATE TABLE IF NOT EXISTS migrations (
                 id            TEXT PRIMARY KEY,
                 connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
                 database      TEXT NOT NULL,
                 name          TEXT NOT NULL,
                 config_json   TEXT NOT NULL DEFAULT '{}',
                 run_ids_json  TEXT NOT NULL DEFAULT '[]',
                 created_at    TEXT NOT NULL,
                 updated_at    TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS migrations_by_db ON migrations(connection_id, database);
             CREATE TABLE IF NOT EXISTS folders (
                 id        TEXT PRIMARY KEY,
                 name      TEXT NOT NULL,
                 parent_id TEXT,
                 color     TEXT
             );
             CREATE TABLE IF NOT EXISTS library (
                 id          TEXT PRIMARY KEY,
                 name        TEXT NOT NULL,
                 folder      TEXT NOT NULL DEFAULT '',
                 description TEXT NOT NULL DEFAULT '',
                 engines     TEXT NOT NULL DEFAULT '[]',
                 text        TEXT NOT NULL DEFAULT '',
                 updated_at  TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS query_history (
                 id              INTEGER PRIMARY KEY AUTOINCREMENT,
                 connection_id   TEXT NOT NULL,
                 connection_name TEXT NOT NULL,
                 driver          TEXT NOT NULL,
                 host            TEXT NOT NULL,
                 database        TEXT NOT NULL,
                 sql             TEXT NOT NULL,
                 started_at      TEXT NOT NULL,
                 duration_ms     INTEGER NOT NULL,
                 rows            INTEGER,
                 error           TEXT
             );
             CREATE TABLE IF NOT EXISTS backups (
                 id            TEXT PRIMARY KEY,
                 connection_id TEXT NOT NULL,
                 database      TEXT NOT NULL,
                 path          TEXT NOT NULL,
                 created_at    TEXT NOT NULL,
                 size          INTEGER NOT NULL,
                 objects       INTEGER NOT NULL,
                 rows          INTEGER NOT NULL,
                 data          INTEGER NOT NULL,
                 duration_ms   INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- Bumped by every change to what a backup carries.
             INSERT OR IGNORE INTO settings (key, value) VALUES ('local.revision', '0');",
        )
        .map_err(db_err)?;
        // Added after the first release: state files from then lack it.
        let has_folder: bool = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('connections') WHERE name = 'folder_id'", [], |r| r.get(0))
            .map_err(db_err)?;
        if !has_folder {
            conn.execute_batch("ALTER TABLE connections ADD COLUMN folder_id TEXT").map_err(db_err)?;
        }
        let has_tags: bool = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('connections') WHERE name = 'tags_json'", [], |r| r.get(0))
            .map_err(db_err)?;
        if !has_tags {
            conn.execute_batch("ALTER TABLE connections ADD COLUMN tags_json TEXT").map_err(db_err)?;
        }
        let has_mcp: bool = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('connections') WHERE name = 'mcp_level'", [], |r| r.get(0))
            .map_err(db_err)?;
        if !has_mcp {
            conn.execute_batch("ALTER TABLE connections ADD COLUMN mcp_level TEXT").map_err(db_err)?;
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| Error::State("state store poisoned".into()))
    }

    // -- connections --------------------------------------------------------

    pub fn list_connections(&self) -> Result<Vec<SavedConnection>> {
        let c = self.lock()?;
        let mut stmt = c.prepare(
            "SELECT id, name, color, config_json, save_password, updated_at, folder_id, tags_json, mcp_level
               FROM connections ORDER BY sort_order, name COLLATE NOCASE",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })
        .map_err(db_err)?;
        rows.map(|row| {
            let (id, name, color, json, save_password, updated_at, folder_id, tags, mcp_level) = row.map_err(db_err)?;
            let tags = tags.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            Ok(SavedConnection { id, name, color, config: serde_json::from_str(&json)?, save_password, folder_id, tags, mcp_level, updated_at })
        })
        .collect()
    }

    pub fn get_connection(&self, id: &str) -> Result<Option<SavedConnection>> {
        Ok(self.list_connections()?.into_iter().find(|c| c.id == id))
    }

    /// Insert or update; the password in `config` is dropped (store it with
    /// [`crate::secrets`]).
    pub fn save_connection(&self, conn: &SavedConnection) -> Result<SavedConnection> {
        let mut config = conn.config.clone();
        config.password = None;
        let json = serde_json::to_string(&config)?;
        let ts = now();
        self.lock()?.execute(
            "INSERT INTO connections (id, name, color, config_json, save_password, created_at, updated_at, folder_id, tags_json, mcp_level)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET name = ?2, color = ?3, config_json = ?4,
                 save_password = ?5, updated_at = ?6, folder_id = ?7, tags_json = ?8, mcp_level = ?9",
            params![conn.id, conn.name, conn.color, json, conn.save_password, ts, conn.folder_id, tags_json(&conn.tags)?, conn.mcp_level],
        )
        .map_err(db_err)?;
        self.touch()?;
        Ok(SavedConnection { config, updated_at: ts, ..conn.clone() })
    }

    pub fn delete_connection(&self, id: &str) -> Result<()> {
        self.lock()?.execute("DELETE FROM connections WHERE id = ?1", [id]).map_err(db_err)?;
        self.touch()
    }

    /// Put a connection in a folder (`None` = top level).
    pub fn move_connection(&self, id: &str, folder_id: Option<&str>) -> Result<()> {
        self.lock()?
            .execute("UPDATE connections SET folder_id = ?2 WHERE id = ?1", params![id, folder_id])
            .map_err(db_err)?;
        self.touch()
    }

    // -- folders ------------------------------------------------------------

    pub fn list_folders(&self) -> Result<Vec<ConnectionFolder>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT id, name, parent_id, color FROM folders ORDER BY name COLLATE NOCASE")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| Ok(ConnectionFolder { id: r.get(0)?, name: r.get(1)?, parent_id: r.get(2)?, color: r.get(3)? }))
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_err)
    }

    /// Insert or update. Refuses to put a folder inside itself or one of its
    /// descendants.
    pub fn save_folder(&self, f: &ConnectionFolder) -> Result<ConnectionFolder> {
        if let Some(parent) = &f.parent_id {
            let folders = self.list_folders()?;
            let mut at = Some(parent.clone());
            while let Some(id) = at {
                if id == f.id {
                    return Err(Error::State("una carpeta no puede ir dentro de sí misma".into()));
                }
                at = folders.iter().find(|x| x.id == id).and_then(|x| x.parent_id.clone());
            }
        }
        self.lock()?
            .execute(
                "INSERT INTO folders (id, name, parent_id, color) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET name = ?2, parent_id = ?3, color = ?4",
                params![f.id, f.name, f.parent_id, f.color],
            )
            .map_err(db_err)?;
        self.touch()?;
        Ok(f.clone())
    }

    /// Delete a folder; what was in it (connections and subfolders) moves up
    /// to its parent, so deleting a folder never deletes connections.
    pub fn delete_folder(&self, id: &str) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(db_err)?;
        let parent: Option<String> = tx
            .query_row("SELECT parent_id FROM folders WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .map_err(db_err)?
            .flatten();
        tx.execute("UPDATE connections SET folder_id = ?2 WHERE folder_id = ?1", params![id, parent]).map_err(db_err)?;
        tx.execute("UPDATE folders SET parent_id = ?2 WHERE parent_id = ?1", params![id, parent]).map_err(db_err)?;
        tx.execute("DELETE FROM folders WHERE id = ?1", [id]).map_err(db_err)?;
        bump(&tx)?;
        tx.commit().map_err(db_err)
    }

    // -- queries ------------------------------------------------------------

    pub fn list_queries(&self, connection_id: &str, database: &str) -> Result<Vec<SavedQuery>> {
        let c = self.lock()?;
        let mut stmt = c.prepare(
            "SELECT id, connection_id, database, name, sql, updated_at, last_run_at
               FROM queries WHERE connection_id = ?1 AND database = ?2
              ORDER BY name COLLATE NOCASE",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map([connection_id, database], row_to_query).map_err(db_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_err)
    }

    pub fn get_query(&self, id: &str) -> Result<Option<SavedQuery>> {
        Ok(self
            .lock()?
            .query_row(
                "SELECT id, connection_id, database, name, sql, updated_at, last_run_at FROM queries WHERE id = ?1",
                [id],
                row_to_query,
            )
            .optional()
            .map_err(db_err)?)
    }

    pub fn save_query(&self, q: &SavedQuery) -> Result<SavedQuery> {
        let ts = now();
        self.lock()?.execute(
            "INSERT INTO queries (id, connection_id, database, name, sql, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET connection_id = ?2, database = ?3, name = ?4, sql = ?5, updated_at = ?6",
            params![q.id, q.connection_id, q.database, q.name, q.sql, ts],
        )
        .map_err(db_err)?;
        self.touch()?;
        Ok(SavedQuery { updated_at: ts, ..q.clone() })
    }

    pub fn mark_query_run(&self, id: &str) -> Result<()> {
        self.lock()?.execute("UPDATE queries SET last_run_at = ?2 WHERE id = ?1", params![id, now()]).map_err(db_err)?;
        Ok(())
    }

    pub fn delete_query(&self, id: &str) -> Result<()> {
        self.lock()?.execute("DELETE FROM queries WHERE id = ?1", [id]).map_err(db_err)?;
        self.touch()
    }

    // -- saved migrations ---------------------------------------------------

    /// A database's saved migrations, newest first.
    pub fn list_migrations(&self, connection_id: &str, database: &str) -> Result<Vec<SavedMigration>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare(
                "SELECT id, connection_id, database, name, config_json, run_ids_json, created_at, updated_at
                   FROM migrations WHERE connection_id = ?1 AND database = ?2
                  ORDER BY created_at DESC, id",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map([connection_id, database], row_to_migration).map_err(db_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_err)
    }

    pub fn get_migration(&self, id: &str) -> Result<Option<SavedMigration>> {
        self.lock()?
            .query_row(
                "SELECT id, connection_id, database, name, config_json, run_ids_json, created_at, updated_at FROM migrations WHERE id = ?1",
                [id],
                row_to_migration,
            )
            .optional()
            .map_err(db_err)
    }

    /// Create or update one (its creation date stays).
    pub fn save_migration(&self, m: &SavedMigration) -> Result<SavedMigration> {
        let ts = now();
        let config = serde_json::to_string(&m.config)?;
        let runs = serde_json::to_string(&m.run_ids)?;
        self.lock()?
            .execute(
                "INSERT INTO migrations (id, connection_id, database, name, config_json, run_ids_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(id) DO UPDATE SET connection_id = ?2, database = ?3, name = ?4, config_json = ?5,
                     run_ids_json = ?6, updated_at = ?7",
                params![m.id, m.connection_id, m.database, m.name, config, runs, ts],
            )
            .map_err(db_err)?;
        self.touch()?;
        self.get_migration(&m.id)?.ok_or_else(|| Error::State("saved migration vanished".into()))
    }

    pub fn rename_migration(&self, id: &str, name: &str) -> Result<SavedMigration> {
        let n = self
            .lock()?
            .execute("UPDATE migrations SET name = ?2, updated_at = ?3 WHERE id = ?1", params![id, name, now()])
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::State("la migración ya no existe".into()));
        }
        self.touch()?;
        self.get_migration(id)?.ok_or_else(|| Error::State("la migración ya no existe".into()))
    }

    /// Link a run to it: it becomes its current run (a run already linked, resumed or retried,
    /// stays where it is).
    pub fn link_migration_run(&self, id: &str, run_id: &str) -> Result<SavedMigration> {
        let mut m = self.get_migration(id)?.ok_or_else(|| Error::State("la migración ya no existe".into()))?;
        if !m.run_ids.iter().any(|r| r == run_id) {
            m.run_ids.push(run_id.to_string());
        }
        self.save_migration(&m)
    }

    /// A copy as a new draft: same configuration, no runs.
    pub fn duplicate_migration(&self, id: &str, new_id: &str, name: &str) -> Result<SavedMigration> {
        let m = self.get_migration(id)?.ok_or_else(|| Error::State("la migración ya no existe".into()))?;
        self.save_migration(&SavedMigration { id: new_id.into(), name: name.into(), run_ids: vec![], ..m })
    }

    /// Only the entry: neither the source, the target nor the runs' records are touched.
    pub fn delete_migration(&self, id: &str) -> Result<()> {
        self.lock()?.execute("DELETE FROM migrations WHERE id = ?1", [id]).map_err(db_err)?;
        self.touch()
    }

    fn all_migrations(&self) -> Result<Vec<SavedMigration>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT id, connection_id, database, name, config_json, run_ids_json, created_at, updated_at FROM migrations ORDER BY id")
            .map_err(db_err)?;
        let rows = stmt.query_map([], row_to_migration).map_err(db_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_err)
    }

    // -- history --------------------------------------------------------
    // Not a change to sync: no `touch()`.

    pub fn add_history(&self, e: &HistoryEntry) -> Result<()> {
        let c = self.lock()?;
        c.execute(
            "INSERT INTO query_history (connection_id, connection_name, driver, host, database, sql, started_at, duration_ms, rows, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![e.connection_id, e.connection_name, e.driver, e.host, e.database, e.sql, e.started_at, e.duration_ms as i64, e.rows.map(|r| r as i64), e.error],
        )
        .map_err(db_err)?;
        let id = c.last_insert_rowid();
        if id % 200 == 0 {
            c.execute("DELETE FROM query_history WHERE id <= ?1", [id - HISTORY_MAX]).map_err(db_err)?;
        }
        Ok(())
    }

    /// Newest first; `search` matches the text, the host, the database or the
    /// connection; `before` pages (an id from the previous page).
    pub fn list_history(&self, search: Option<&str>, before: Option<i64>, limit: u32) -> Result<Vec<HistoryEntry>> {
        let c = self.lock()?;
        let like = search.filter(|s| !s.trim().is_empty()).map(|s| format!("%{}%", s.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let mut stmt = c
            .prepare(
                "SELECT id, connection_id, connection_name, driver, host, database, sql, started_at, duration_ms, rows, error FROM query_history
                 WHERE (?1 IS NULL OR sql LIKE ?1 ESCAPE '\\' OR host LIKE ?1 ESCAPE '\\' OR database LIKE ?1 ESCAPE '\\' OR connection_name LIKE ?1 ESCAPE '\\')
                   AND (?2 IS NULL OR id < ?2)
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![like, before, limit], |r| {
                Ok(HistoryEntry {
                    id: r.get(0)?,
                    connection_id: r.get(1)?,
                    connection_name: r.get(2)?,
                    driver: r.get(3)?,
                    host: r.get(4)?,
                    database: r.get(5)?,
                    sql: r.get(6)?,
                    started_at: r.get(7)?,
                    duration_ms: r.get::<_, i64>(8)? as u64,
                    rows: r.get::<_, Option<i64>>(9)?.map(|n| n as u64),
                    error: r.get(10)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(db_err)
    }

    /// Delete some entries, or all of them (`None`).
    pub fn delete_history(&self, ids: Option<&[i64]>) -> Result<()> {
        let c = self.lock()?;
        match ids {
            None => c.execute("DELETE FROM query_history", []).map_err(db_err)?,
            Some(ids) => {
                let mut n = 0;
                for id in ids {
                    n += c.execute("DELETE FROM query_history WHERE id = ?1", [id]).map_err(db_err)?;
                }
                n
            }
        };
        Ok(())
    }

    // -- backup copies --------------------------------------------------
    // Local files: not a change to sync.

    pub fn add_backup(&self, b: &BackupCopy) -> Result<()> {
        let c = self.lock()?;
        c.execute(
            "INSERT INTO backups (id, connection_id, database, path, created_at, size, objects, rows, data, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![b.id, b.connection_id, b.database, b.path, b.created_at, b.size as i64, b.objects as i64, b.rows as i64, b.data, b.duration_ms as i64],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// A connection's copies (of one database, or all when `None`), newest first.
    pub fn list_backups(&self, connection_id: &str, database: Option<&str>) -> Result<Vec<BackupCopy>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare(
                "SELECT id, connection_id, database, path, created_at, size, objects, rows, data, duration_ms FROM backups
                 WHERE connection_id = ?1 AND (?2 IS NULL OR database = ?2) ORDER BY created_at DESC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![connection_id, database], |r| {
                Ok(BackupCopy {
                    id: r.get(0)?,
                    connection_id: r.get(1)?,
                    database: r.get(2)?,
                    path: r.get(3)?,
                    created_at: r.get(4)?,
                    size: r.get::<_, i64>(5)? as u64,
                    objects: r.get::<_, i64>(6)? as u64,
                    rows: r.get::<_, i64>(7)? as u64,
                    data: r.get(8)?,
                    duration_ms: r.get::<_, i64>(9)? as u64,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(db_err)
    }

    pub fn get_backup(&self, id: &str) -> Result<Option<BackupCopy>> {
        let conn: Option<String> = self
            .lock()?
            .query_row("SELECT connection_id FROM backups WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        Ok(match conn {
            Some(conn) => self.list_backups(&conn, None)?.into_iter().find(|b| b.id == id),
            None => None,
        })
    }

    pub fn delete_backup(&self, id: &str) -> Result<()> {
        self.lock()?.execute("DELETE FROM backups WHERE id = ?1", [id]).map_err(db_err)?;
        Ok(())
    }

    // -- library --------------------------------------------------------

    pub fn list_library(&self) -> Result<Vec<LibraryScript>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT id, name, folder, description, engines, text, updated_at FROM library ORDER BY folder COLLATE NOCASE, name COLLATE NOCASE")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(db_err)?;
        rows.map(|row| {
            let (id, name, folder, description, engines, text, updated_at) = row.map_err(db_err)?;
            Ok(LibraryScript { id, name, folder, description, engines: serde_json::from_str(&engines).unwrap_or_default(), text, updated_at })
        })
        .collect()
    }

    /// Insert or update.
    pub fn save_library_script(&self, s: &LibraryScript) -> Result<LibraryScript> {
        let ts = now();
        self.lock()?
            .execute(
                "INSERT INTO library (id, name, folder, description, engines, text, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET name = ?2, folder = ?3, description = ?4, engines = ?5, text = ?6, updated_at = ?7",
                params![s.id, s.name, s.folder, s.description, serde_json::to_string(&s.engines)?, s.text, ts],
            )
            .map_err(db_err)?;
        self.touch()?;
        Ok(LibraryScript { updated_at: ts, ..s.clone() })
    }

    pub fn delete_library_script(&self, id: &str) -> Result<()> {
        self.lock()?.execute("DELETE FROM library WHERE id = ?1", [id]).map_err(db_err)?;
        self.touch()
    }

    // -- settings & snapshot ----------------------------------------------

    /// Count a change to what a backup carries.
    fn touch(&self) -> Result<()> {
        let c = self.lock()?;
        bump(&c)
    }

    /// How many changes the state has seen (`local.revision`): the sync
    /// engine compares it with the one it last uploaded.
    pub fn revision(&self) -> Result<u64> {
        let v = self.get_setting("local.revision")?.unwrap_or_default();
        Ok(v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()).unwrap_or(0))
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = self
            .lock()?
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        Ok(raw.map(|r| serde_json::from_str(&r).unwrap_or(serde_json::Value::String(r))))
    }

    /// Set (or, with `None`, remove) a setting. Keys under
    /// [`LOCAL_PREFIX`] are this machine's; the rest are user preferences
    /// and count as a change.
    pub fn set_setting(&self, key: &str, value: Option<&serde_json::Value>) -> Result<()> {
        let c = self.lock()?;
        match value {
            Some(v) => c.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
                params![key, serde_json::to_string(v)?],
            ),
            None => c.execute("DELETE FROM settings WHERE key = ?1", [key]),
        }
        .map_err(db_err)?;
        if !key.starts_with(LOCAL_PREFIX) {
            bump(&c)?;
        }
        Ok(())
    }

    /// The user preferences (settings outside [`LOCAL_PREFIX`]).
    pub fn list_settings(&self) -> Result<BTreeMap<String, serde_json::Value>> {
        let c = self.lock()?;
        let mut stmt = c.prepare("SELECT key, value FROM settings WHERE key NOT LIKE 'local.%'").map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(db_err)?;
        rows.map(|row| {
            let (k, v) = row.map_err(db_err)?;
            Ok((k, serde_json::from_str(&v)?))
        })
        .collect()
    }

    /// Everything a backup carries.
    pub fn snapshot(&self) -> Result<StateSnapshot> {
        let queries = {
            let c = self.lock()?;
            let mut stmt = c
                .prepare("SELECT id, connection_id, database, name, sql, updated_at, last_run_at FROM queries ORDER BY id")
                .map_err(db_err)?;
            let rows = stmt.query_map([], row_to_query).map_err(db_err)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)?
        };
        Ok(StateSnapshot {
            connections: self.list_connections()?,
            folders: self.list_folders()?,
            queries,
            settings: self.list_settings()?,
            library: self.list_library()?,
            migrations: self.all_migrations()?,
        })
    }

    /// Replace the whole state with a snapshot (a restore), in one
    /// transaction: either all of it lands or nothing changes. This
    /// machine's own settings stay.
    pub fn replace_all(&self, snap: &StateSnapshot) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(db_err)?;
        tx.execute_batch(
            "DELETE FROM queries; DELETE FROM migrations; DELETE FROM connections; DELETE FROM folders;
             DELETE FROM settings WHERE key NOT LIKE 'local.%'; DELETE FROM library;",
        )
        .map_err(db_err)?;
        for (i, conn) in snap.connections.iter().enumerate() {
            let mut config = conn.config.clone();
            config.password = None;
            let ts = if conn.updated_at.is_empty() { now() } else { conn.updated_at.clone() };
            tx.execute(
                "INSERT INTO connections (id, name, color, config_json, save_password, sort_order, created_at, updated_at, folder_id, tags_json, mcp_level)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10)",
                params![
                    conn.id,
                    conn.name,
                    conn.color,
                    serde_json::to_string(&config)?,
                    conn.save_password,
                    i as i64,
                    ts,
                    conn.folder_id,
                    tags_json(&conn.tags)?,
                    conn.mcp_level
                ],
            )
            .map_err(db_err)?;
        }
        for f in &snap.folders {
            tx.execute(
                "INSERT INTO folders (id, name, parent_id, color) VALUES (?1, ?2, ?3, ?4)",
                params![f.id, f.name, f.parent_id, f.color],
            )
            .map_err(db_err)?;
        }
        let known: std::collections::HashSet<&str> = snap.connections.iter().map(|c| c.id.as_str()).collect();
        for q in snap.queries.iter().filter(|q| known.contains(q.connection_id.as_str())) {
            let ts = if q.updated_at.is_empty() { now() } else { q.updated_at.clone() };
            tx.execute(
                "INSERT INTO queries (id, connection_id, database, name, sql, created_at, updated_at, last_run_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
                params![q.id, q.connection_id, q.database, q.name, q.sql, ts, q.last_run_at],
            )
            .map_err(db_err)?;
        }
        for m in snap.migrations.iter().filter(|m| known.contains(m.connection_id.as_str())) {
            let ts = if m.updated_at.is_empty() { now() } else { m.updated_at.clone() };
            let created = if m.created_at.is_empty() { ts.clone() } else { m.created_at.clone() };
            tx.execute(
                "INSERT INTO migrations (id, connection_id, database, name, config_json, run_ids_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    m.id,
                    m.connection_id,
                    m.database,
                    m.name,
                    serde_json::to_string(&m.config)?,
                    serde_json::to_string(&m.run_ids)?,
                    created,
                    ts
                ],
            )
            .map_err(db_err)?;
        }
        for (k, v) in snap.settings.iter().filter(|(k, _)| !k.starts_with(LOCAL_PREFIX)) {
            tx.execute("INSERT INTO settings (key, value) VALUES (?1, ?2)", params![k, serde_json::to_string(v)?])
                .map_err(db_err)?;
        }
        for l in &snap.library {
            let ts = if l.updated_at.is_empty() { now() } else { l.updated_at.clone() };
            tx.execute(
                "INSERT INTO library (id, name, folder, description, engines, text, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![l.id, l.name, l.folder, l.description, serde_json::to_string(&l.engines)?, l.text, ts],
            )
            .map_err(db_err)?;
        }
        bump(&tx)?;
        tx.commit().map_err(db_err)
    }
}

fn bump(c: &Connection) -> Result<()> {
    c.execute(
        "UPDATE settings SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT) WHERE key = 'local.revision'",
        [],
    )
    .map_err(db_err)?;
    // When: a sync conflict keeps the newest side.
    c.execute(
        "INSERT INTO settings (key, value) VALUES ('local.changed_at', ?1) ON CONFLICT(key) DO UPDATE SET value = ?1",
        [serde_json::to_string(&now())?],
    )
    .map_err(db_err)?;
    Ok(())
}

fn row_to_query(r: &rusqlite::Row<'_>) -> rusqlite::Result<SavedQuery> {
    Ok(SavedQuery {
        id: r.get(0)?,
        connection_id: r.get(1)?,
        database: r.get(2)?,
        name: r.get(3)?,
        sql: r.get(4)?,
        updated_at: r.get(5)?,
        last_run_at: r.get(6)?,
    })
}

fn row_to_migration(r: &rusqlite::Row<'_>) -> rusqlite::Result<SavedMigration> {
    let config: String = r.get(4)?;
    let runs: String = r.get(5)?;
    Ok(SavedMigration {
        id: r.get(0)?,
        connection_id: r.get(1)?,
        database: r.get(2)?,
        name: r.get(3)?,
        // A damaged document reads as an empty draft rather than hiding the entry.
        config: serde_json::from_str(&config).unwrap_or(serde_json::Value::Null),
        run_ids: serde_json::from_str(&runs).unwrap_or_default(),
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

/// Tags as stored (`NULL` when there are none), trimmed and without repeats.
fn tags_json(tags: &[String]) -> Result<Option<String>> {
    let mut clean: Vec<&str> = Vec::new();
    for t in tags.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
        if !clean.iter().any(|c| c.eq_ignore_ascii_case(t)) {
            clean.push(t);
        }
    }
    Ok(if clean.is_empty() { None } else { Some(serde_json::to_string(&clean)?) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            name: "local".into(),
            color: None,
            config: ConnectionConfig {
                driver: "postgres".into(),
                host: "localhost".into(),
                username: Some("u".into()),
                password: Some("secret".into()),
                ..Default::default()
            },
            save_password: true,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        }
    }

    fn folder(id: &str, parent: Option<&str>) -> ConnectionFolder {
        ConnectionFolder { id: id.into(), name: id.into(), parent_id: parent.map(Into::into), color: None }
    }

    #[test]
    fn tags_are_kept_trimmed_and_without_repeats() {
        let s = StateStore::open_in_memory().unwrap();
        let mut c = conn("c1");
        c.tags = vec![" prod".into(), "Prod".into(), "".into(), "dev".into()];
        s.save_connection(&c).unwrap();
        assert_eq!(s.get_connection("c1").unwrap().unwrap().tags, vec!["prod".to_string(), "dev".to_string()]);
        c.tags = vec![];
        s.save_connection(&c).unwrap();
        assert!(s.get_connection("c1").unwrap().unwrap().tags.is_empty());
    }

    #[test]
    fn deleting_a_folder_moves_its_content_up() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_folder(&folder("cliente", None)).unwrap();
        s.save_folder(&folder("prod", Some("cliente"))).unwrap();
        s.save_folder(&folder("qa", Some("prod"))).unwrap();
        s.save_connection(&conn("c1")).unwrap();
        s.move_connection("c1", Some("prod")).unwrap();
        s.delete_folder("prod").unwrap();
        assert_eq!(s.get_connection("c1").unwrap().unwrap().folder_id.as_deref(), Some("cliente"));
        let qa = s.list_folders().unwrap().into_iter().find(|f| f.id == "qa").unwrap();
        assert_eq!(qa.parent_id.as_deref(), Some("cliente"));
    }

    #[test]
    fn a_folder_cannot_go_inside_itself() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_folder(&folder("a", None)).unwrap();
        s.save_folder(&folder("b", Some("a"))).unwrap();
        assert!(s.save_folder(&folder("a", Some("b"))).is_err());
    }

    #[test]
    fn old_state_files_gain_the_folder_column() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE connections (id TEXT PRIMARY KEY, name TEXT NOT NULL, color TEXT, config_json TEXT NOT NULL,
             save_password INTEGER NOT NULL DEFAULT 0, sort_order INTEGER NOT NULL DEFAULT 0,
             created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
        )
        .unwrap();
        let s = StateStore::init(c).unwrap();
        s.save_connection(&conn("c1")).unwrap();
        s.move_connection("c1", Some("x")).unwrap();
        assert_eq!(s.get_connection("c1").unwrap().unwrap().folder_id.as_deref(), Some("x"));
    }

    #[test]
    fn library_scripts_roundtrip_and_travel_in_the_snapshot() {
        let s = StateStore::open_in_memory().unwrap();
        let rev = s.revision().unwrap();
        let script = LibraryScript {
            id: "l1".into(),
            name: "Reindexar tabla".into(),
            folder: "Mantenimiento".into(),
            engines: vec!["sqlserver".into()],
            text: "ALTER INDEX ALL ON {{tabla}} REBUILD;".into(),
            ..Default::default()
        };
        s.save_library_script(&script).unwrap();
        assert!(s.revision().unwrap() > rev, "a library change is a change to sync");
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.library[0].engines, ["sqlserver"]);
        let other = StateStore::open_in_memory().unwrap();
        other.replace_all(&snap).unwrap();
        assert_eq!(other.list_library().unwrap()[0].text, script.text);
        other.delete_library_script("l1").unwrap();
        assert!(other.list_library().unwrap().is_empty());
    }

    #[test]
    fn passwords_never_reach_the_file() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_connection(&conn("c1")).unwrap();
        assert_eq!(s.get_connection("c1").unwrap().unwrap().config.password, None);
    }

    #[test]
    fn queries_belong_to_a_database_and_go_with_their_connection() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_connection(&conn("c1")).unwrap();
        let q = SavedQuery {
            id: "q1".into(),
            connection_id: "c1".into(),
            database: "app".into(),
            name: "Clientes".into(),
            sql: "select 1".into(),
            updated_at: String::new(),
            last_run_at: None,
        };
        s.save_query(&q).unwrap();
        assert_eq!(s.list_queries("c1", "app").unwrap().len(), 1);
        assert!(s.list_queries("c1", "other").unwrap().is_empty());
        s.delete_connection("c1").unwrap();
        assert!(s.get_query("q1").unwrap().is_none());
    }

    #[test]
    fn saved_migrations_keep_their_config_and_runs() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_connection(&conn("c1")).unwrap();
        let rev = s.revision().unwrap();
        let m = SavedMigration {
            id: "m1".into(),
            connection_id: "c1".into(),
            database: "app".into(),
            name: "→ pg · app2 · Convertir".into(),
            config: serde_json::json!({ "target_driver": "postgres", "tables": ["public.a"] }),
            ..Default::default()
        };
        let saved = s.save_migration(&m).unwrap();
        assert!(s.revision().unwrap() > rev, "a saved migration is a change to sync");
        assert!(!saved.created_at.is_empty());
        assert_eq!(s.list_migrations("c1", "app").unwrap(), vec![saved.clone()]);
        assert!(s.list_migrations("c1", "other").unwrap().is_empty());

        // Editing keeps the creation date; runs link in order, a resumed one isn't repeated.
        s.save_migration(&SavedMigration { config: serde_json::json!({ "mode": "clone" }), ..saved.clone() }).unwrap();
        s.link_migration_run("m1", "r1").unwrap();
        s.link_migration_run("m1", "r2").unwrap();
        let got = s.link_migration_run("m1", "r1").unwrap();
        assert_eq!(got.run_ids, ["r1", "r2"]);
        assert_eq!(got.config["mode"], "clone");
        assert_eq!(got.created_at, saved.created_at);

        assert_eq!(s.rename_migration("m1", "Copia nocturna").unwrap().name, "Copia nocturna");
        assert!(s.rename_migration("nope", "x").is_err());

        let dup = s.duplicate_migration("m1", "m2", "Copia nocturna (copia)").unwrap();
        assert!(dup.run_ids.is_empty(), "a duplicate is a new draft");
        assert_eq!(dup.config, got.config);
        assert_eq!(s.list_migrations("c1", "app").unwrap().len(), 2);

        // It travels in the snapshot (only with its connection).
        let snap = s.snapshot().unwrap();
        let other = StateStore::open_in_memory().unwrap();
        other.replace_all(&snap).unwrap();
        assert_eq!(other.get_migration("m1").unwrap().unwrap().run_ids, ["r1", "r2"]);
        let mut orphan = snap.clone();
        orphan.connections.clear();
        other.replace_all(&orphan).unwrap();
        assert!(other.get_migration("m1").unwrap().is_none());

        s.delete_migration("m2").unwrap();
        assert!(s.get_migration("m2").unwrap().is_none());
        s.delete_connection("c1").unwrap();
        assert!(s.get_migration("m1").unwrap().is_none(), "deleting the connection deletes its migrations");
    }

    #[test]
    fn backup_copies_are_listed_per_database_and_not_synced() {
        let s = StateStore::open_in_memory().unwrap();
        let rev = s.revision().unwrap();
        let copy = |id: &str, db: &str, at: &str| BackupCopy {
            id: id.into(), connection_id: "c1".into(), database: db.into(), path: format!("/tmp/{id}.sql"), created_at: at.into(),
            size: 10, objects: 2, rows: 5, data: true, duration_ms: 7,
        };
        s.add_backup(&copy("a", "ventas", "2026-01-01T00:00:00Z")).unwrap();
        s.add_backup(&copy("b", "ventas", "2026-02-01T00:00:00Z")).unwrap();
        s.add_backup(&copy("c", "rrhh", "2026-03-01T00:00:00Z")).unwrap();
        let ids = |v: Vec<BackupCopy>| v.into_iter().map(|b| b.id).collect::<Vec<_>>();
        assert_eq!(ids(s.list_backups("c1", Some("ventas")).unwrap()), ["b", "a"]);
        assert_eq!(ids(s.list_backups("c1", None).unwrap()), ["c", "b", "a"]);
        assert!(s.list_backups("c2", None).unwrap().is_empty());
        assert_eq!(s.get_backup("c").unwrap().unwrap().database, "rrhh");
        s.delete_backup("a").unwrap();
        assert!(s.get_backup("a").unwrap().is_none());
        assert_eq!(s.revision().unwrap(), rev);
    }

    #[test]
    fn history_is_kept_searched_and_not_synced() {
        let s = StateStore::open_in_memory().unwrap();
        let rev = s.revision().unwrap();
        let entry = |sql: &str, host: &str| HistoryEntry {
            id: 0, connection_id: "c1".into(), connection_name: "Prod".into(), driver: "postgres".into(), host: host.into(),
            database: "app".into(), sql: sql.into(), started_at: now(), duration_ms: 12, rows: Some(3), error: None,
        };
        s.add_history(&entry("SELECT * FROM clientes", "pg1")).unwrap();
        s.add_history(&entry("UPDATE stock SET n = 0 -- 100%", "pg2")).unwrap();
        s.add_history(&entry("SELECT 1", "pg1")).unwrap();
        assert_eq!(s.revision().unwrap(), rev, "the history isn't a change to sync");
        let all = s.list_history(None, None, 10).unwrap();
        assert_eq!(all.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["SELECT 1", "UPDATE stock SET n = 0 -- 100%", "SELECT * FROM clientes"]);
        assert_eq!(s.list_history(Some("pg2"), None, 10).unwrap().len(), 1);
        assert_eq!(s.list_history(Some("100%"), None, 10).unwrap().len(), 1, "a % in the search is literal");
        assert_eq!(s.list_history(Some("%"), None, 10).unwrap().len(), 1);
        let page2 = s.list_history(None, Some(all[1].id), 10).unwrap();
        assert_eq!(page2.len(), 1);
        s.delete_history(Some(&[all[0].id])).unwrap();
        assert_eq!(s.list_history(None, None, 10).unwrap().len(), 2);
        s.delete_history(None).unwrap();
        assert!(s.list_history(None, None, 10).unwrap().is_empty());
    }

}

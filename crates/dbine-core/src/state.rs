//! Local state: connection folders, saved connections and saved queries, in
//! one SQLite file in the app's config directory. Synchronous (rusqlite behind a mutex): every
//! call is a small indexed read or write.

use dbine_driver::{ConnectionConfig, Error, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::tasks::{ScheduledTask, TaskRun};

/// Runs kept per scheduled task.
pub const RUNS_KEPT: u32 = 200;

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

/// What [`StateStore::reorder_explorer`] orders: a level's connections or its folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExplorerItem {
    Connection,
    Folder,
}

/// A query kept under a database in the explorer.
/// A statement run from the editor (the history sidebar). Local to this
/// machine: not in the cloud backup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// The saved query it was run from (its tab's timeline).
    #[serde(default)]
    pub query_id: Option<String>,
    /// The project file it was run from (`project_id` + `file_path`).
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
}

/// How many statements the history keeps (the oldest go first).
const HISTORY_MAX: i64 = 20_000;

/// A saved query's text at one moment: its timeline in the history
/// sidebar (docs/historial.md). Local to this machine, like the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryVersion {
    pub id: i64,
    pub query_id: String,
    pub saved_at: String,
    /// Lines added and removed against the version before it.
    pub added: u32,
    pub removed: u32,
    /// `None` in a list; the text comes with [`StateStore::get_query_version`].
    pub sql: Option<String>,
}

/// While typing, at most one version of a query per this many seconds
/// (an explicit save, a run or closing the tab always records one).
pub const VERSION_THROTTLE_SECS: i64 = 60;
/// Versions kept per query, at most.
const VERSIONS_MAX: usize = 300;
/// Every version of the last days stays; then one a day (the day's last)
/// up to `VERSIONS_DAILY_DAYS`; older ones go.
const VERSIONS_ALL_DAYS: i64 = 7;
const VERSIONS_DAILY_DAYS: i64 = 90;

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

/// A connection and one of its databases: where a project's scripts run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectTarget {
    pub connection_id: String,
    pub database: String,
}

/// Which database a project's scripts run on. Local only: the repo names its
/// environments (`.dbine.json`), this machine maps them to its connections.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectBinding {
    /// Used when the repo has no environments, or none is active.
    #[serde(default)]
    pub direct: Option<ProjectTarget>,
    /// Alias (from `.dbine.json`) → the user's connection and database.
    #[serde(default)]
    pub environments: BTreeMap<String, ProjectTarget>,
    /// Alias in use; `None` = `direct`.
    #[serde(default)]
    pub active_environment: Option<String>,
}

/// A git working copy linked as a project ("Proyectos"). Machine-local: the
/// path only makes sense here, so it's neither synced nor backed up.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// The repo's top level, canonical.
    pub path: String,
    #[serde(default)]
    pub binding: ProjectBinding,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default)]
    pub created_at: String,
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

/// What a successful write changed, for whoever listens (the app relays it
/// to every window). `kind` is one of `connection`, `folder`, `explorer`
/// (an order or folder move), `query`, `migration`, `setting`, `library`,
/// `history`, `backup`, `project` (a linked git folder; local only) or
/// `restore` (the whole state replaced).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateChange {
    pub kind: String,
    pub id: Option<String>,
    pub connection_id: Option<String>,
    pub database: Option<String>,
}

impl StateChange {
    fn new(kind: &str, id: Option<&str>) -> Self {
        Self { kind: kind.into(), id: id.map(Into::into), connection_id: None, database: None }
    }

    fn scoped(mut self, connection_id: &str, database: Option<&str>) -> Self {
        self.connection_id = Some(connection_id.into());
        self.database = database.map(Into::into);
        self
    }
}

type ChangeHook = Arc<dyn Fn(StateChange) + Send + Sync>;

pub struct StateStore {
    conn: Mutex<Connection>,
    hook: RwLock<Option<ChangeHook>>,
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
             -- A scheduled task (`dbine --run-task`) can write while the app is open.
             PRAGMA busy_timeout = 5000;
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
             -- A saved query's versions (its timeline): local only, not in the snapshot.
             CREATE TABLE IF NOT EXISTS query_versions (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 query_id TEXT NOT NULL,
                 saved_at TEXT NOT NULL,
                 sql      TEXT NOT NULL,
                 hash     TEXT NOT NULL,
                 added    INTEGER NOT NULL DEFAULT 0,
                 removed  INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS query_versions_by_query ON query_versions(query_id, id);
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
             CREATE TABLE IF NOT EXISTS projects (
                 id           TEXT PRIMARY KEY,
                 name         TEXT NOT NULL,
                 path         TEXT NOT NULL UNIQUE,
                 binding_json TEXT NOT NULL DEFAULT '{}',
                 sort_order   INTEGER NOT NULL DEFAULT 0,
                 created_at   TEXT NOT NULL,
                 updated_at   TEXT NOT NULL
             );
             -- Scheduled tasks and their runs (tasks.rs): this machine's only,
             -- not in the snapshot.
             CREATE TABLE IF NOT EXISTS scheduled_tasks (
                 id         TEXT PRIMARY KEY,
                 task_json  TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS task_runs (
                 id         TEXT PRIMARY KEY,
                 task_id    TEXT NOT NULL,
                 started_at TEXT NOT NULL,
                 status     TEXT NOT NULL,
                 run_json   TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS task_runs_by_task ON task_runs(task_id, started_at);
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
        // Folders were listed by name; existing ones start equal (0) and keep that order.
        let has_folder_order: bool = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('folders') WHERE name = 'sort_order'", [], |r| r.get(0))
            .map_err(db_err)?;
        if !has_folder_order {
            conn.execute_batch("ALTER TABLE folders ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0").map_err(db_err)?;
        }
        // Where a run came from (a saved query, a project's file): added with the timeline.
        let has_origin: bool = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('query_history') WHERE name = 'query_id'", [], |r| r.get(0))
            .map_err(db_err)?;
        if !has_origin {
            conn.execute_batch(
                "ALTER TABLE query_history ADD COLUMN query_id TEXT;
                 ALTER TABLE query_history ADD COLUMN project_id TEXT;
                 ALTER TABLE query_history ADD COLUMN file_path TEXT;",
            )
            .map_err(db_err)?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS query_history_by_query ON query_history(query_id) WHERE query_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS query_history_by_file ON query_history(project_id, file_path) WHERE project_id IS NOT NULL;
             -- Versions of queries deleted meanwhile (with their connection, or by a restore).
             DELETE FROM query_versions WHERE query_id NOT IN (SELECT id FROM queries);",
        )
        .map_err(db_err)?;
        Ok(Self { conn: Mutex::new(conn), hook: RwLock::new(None) })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| Error::State("state store poisoned".into()))
    }

    /// Called after every successful write, with no lock held (the hook may
    /// read the store). Replaces any previous hook.
    pub fn set_change_hook(&self, hook: Box<dyn Fn(StateChange) + Send + Sync>) {
        if let Ok(mut h) = self.hook.write() {
            *h = Some(Arc::from(hook));
        }
    }

    fn notify(&self, change: StateChange) {
        let hook = self.hook.read().ok().and_then(|h| h.clone());
        if let Some(hook) = hook {
            hook(change);
        }
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
    /// [`crate::secrets`]). A new connection, or one that changes folder,
    /// goes last in its level.
    pub fn save_connection(&self, conn: &SavedConnection) -> Result<SavedConnection> {
        let mut config = conn.config.clone();
        config.password = None;
        let json = serde_json::to_string(&config)?;
        let ts = now();
        self.lock()?.execute(
            "INSERT INTO connections (id, name, color, config_json, save_password, created_at, updated_at, folder_id, tags_json, mcp_level, sort_order)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9,
                     (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM connections WHERE folder_id IS ?7))
             ON CONFLICT(id) DO UPDATE SET name = ?2, color = ?3, config_json = ?4,
                 save_password = ?5, updated_at = ?6, folder_id = ?7, tags_json = ?8, mcp_level = ?9,
                 sort_order = CASE WHEN folder_id IS ?7 THEN sort_order ELSE excluded.sort_order END",
            params![conn.id, conn.name, conn.color, json, conn.save_password, ts, conn.folder_id, tags_json(&conn.tags)?, conn.mcp_level],
        )
        .map_err(db_err)?;
        self.touch()?;
        self.notify(StateChange::new("connection", Some(&conn.id)));
        Ok(SavedConnection { config, updated_at: ts, ..conn.clone() })
    }

    pub fn delete_connection(&self, id: &str) -> Result<()> {
        self.lock()?.execute("DELETE FROM connections WHERE id = ?1", [id]).map_err(db_err)?;
        self.touch()?;
        self.notify(StateChange::new("connection", Some(id)));
        Ok(())
    }

    /// Put a connection in a folder (`None` = top level), last in it.
    pub fn move_connection(&self, id: &str, folder_id: Option<&str>) -> Result<()> {
        self.lock()?
            .execute(
                "UPDATE connections SET folder_id = ?2,
                     sort_order = (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM connections WHERE folder_id IS ?2 AND id <> ?1)
                  WHERE id = ?1 AND folder_id IS NOT ?2",
                params![id, folder_id],
            )
            .map_err(db_err)?;
        self.touch()?;
        self.notify(StateChange::new("explorer", Some(id)));
        Ok(())
    }

    /// Reorder one level of the explorer: `ids` (all connections, or all
    /// folders, of the level under `parent`; `None` = top level) end up in it,
    /// in that order. One transaction. Refuses to put a folder inside itself
    /// or one of its descendants.
    pub fn reorder_explorer(&self, kind: ExplorerItem, parent: Option<&str>, ids: &[String]) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(db_err)?;
        if kind == ExplorerItem::Folder {
            let mut at = parent.map(str::to_string);
            while let Some(id) = at {
                if ids.contains(&id) {
                    return Err(Error::State("una carpeta no puede ir dentro de sí misma".into()));
                }
                at = tx
                    .query_row("SELECT parent_id FROM folders WHERE id = ?1", [&id], |r| r.get(0))
                    .optional()
                    .map_err(db_err)?
                    .flatten();
            }
        }
        let sql = match kind {
            ExplorerItem::Connection => "UPDATE connections SET folder_id = ?2, sort_order = ?3 WHERE id = ?1",
            ExplorerItem::Folder => "UPDATE folders SET parent_id = ?2, sort_order = ?3 WHERE id = ?1",
        };
        for (i, id) in ids.iter().enumerate() {
            tx.execute(sql, params![id, parent, i as i64]).map_err(db_err)?;
        }
        bump(&tx)?;
        tx.commit().map_err(db_err)?;
        drop(c);
        self.notify(StateChange::new("explorer", parent));
        Ok(())
    }

    // -- folders ------------------------------------------------------------

    pub fn list_folders(&self) -> Result<Vec<ConnectionFolder>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT id, name, parent_id, color FROM folders ORDER BY sort_order, name COLLATE NOCASE")
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
        // New, or moved to another folder: last in its level.
        self.lock()?
            .execute(
                "INSERT INTO folders (id, name, parent_id, color, sort_order)
                 VALUES (?1, ?2, ?3, ?4, (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM folders WHERE parent_id IS ?3))
                 ON CONFLICT(id) DO UPDATE SET name = ?2, parent_id = ?3, color = ?4,
                     sort_order = CASE WHEN parent_id IS ?3 THEN sort_order ELSE excluded.sort_order END",
                params![f.id, f.name, f.parent_id, f.color],
            )
            .map_err(db_err)?;
        self.touch()?;
        self.notify(StateChange::new("folder", Some(&f.id)));
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
        tx.commit().map_err(db_err)?;
        drop(c);
        self.notify(StateChange::new("folder", Some(id)));
        Ok(())
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
        self.notify(StateChange::new("query", Some(&q.id)).scoped(&q.connection_id, Some(&q.database)));
        Ok(SavedQuery { updated_at: ts, ..q.clone() })
    }

    pub fn mark_query_run(&self, id: &str) -> Result<()> {
        self.lock()?.execute("UPDATE queries SET last_run_at = ?2 WHERE id = ?1", params![id, now()]).map_err(db_err)?;
        Ok(())
    }

    pub fn delete_query(&self, id: &str) -> Result<()> {
        {
            let c = self.lock()?;
            c.execute("DELETE FROM queries WHERE id = ?1", [id]).map_err(db_err)?;
            c.execute("DELETE FROM query_versions WHERE query_id = ?1", [id]).map_err(db_err)?;
        }
        self.touch()?;
        self.notify(StateChange::new("query", Some(id)));
        Ok(())
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
        let saved = self.get_migration(&m.id)?.ok_or_else(|| Error::State("saved migration vanished".into()))?;
        self.notify(StateChange::new("migration", Some(&m.id)).scoped(&m.connection_id, Some(&m.database)));
        Ok(saved)
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
        let saved = self.get_migration(id)?.ok_or_else(|| Error::State("la migración ya no existe".into()))?;
        self.notify(StateChange::new("migration", Some(id)).scoped(&saved.connection_id, Some(&saved.database)));
        Ok(saved)
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
        self.touch()?;
        self.notify(StateChange::new("migration", Some(id)));
        Ok(())
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
            "INSERT INTO query_history (connection_id, connection_name, driver, host, database, sql, started_at, duration_ms, rows, error, query_id, project_id, file_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                e.connection_id,
                e.connection_name,
                e.driver,
                e.host,
                e.database,
                e.sql,
                e.started_at,
                e.duration_ms as i64,
                e.rows.map(|r| r as i64),
                e.error,
                e.query_id,
                e.project_id,
                e.file_path
            ],
        )
        .map_err(db_err)?;
        let id = c.last_insert_rowid();
        if id % 200 == 0 {
            c.execute("DELETE FROM query_history WHERE id <= ?1", [id - HISTORY_MAX]).map_err(db_err)?;
        }
        drop(c);
        self.notify(StateChange::new("history", Some(&id.to_string())).scoped(&e.connection_id, Some(&e.database)));
        Ok(())
    }

    /// Newest first; `search` matches the text, the host, the database or the
    /// connection; `before` pages (an id from the previous page).
    pub fn list_history(&self, search: Option<&str>, before: Option<i64>, limit: u32) -> Result<Vec<HistoryEntry>> {
        let c = self.lock()?;
        let like = search.filter(|s| !s.trim().is_empty()).map(|s| format!("%{}%", s.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let mut stmt = c
            .prepare(
                "SELECT id, connection_id, connection_name, driver, host, database, sql, started_at, duration_ms, rows, error, query_id, project_id, file_path
                 FROM query_history
                 WHERE (?1 IS NULL OR sql LIKE ?1 ESCAPE '\\' OR host LIKE ?1 ESCAPE '\\' OR database LIKE ?1 ESCAPE '\\' OR connection_name LIKE ?1 ESCAPE '\\')
                   AND (?2 IS NULL OR id < ?2)
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map(params![like, before, limit], row_to_history).map_err(db_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(db_err)
    }

    /// The runs of one saved query, or of one project file, newest first
    /// (a tab's timeline).
    pub fn list_history_of(&self, query_id: Option<&str>, file: Option<(&str, &str)>, limit: u32) -> Result<Vec<HistoryEntry>> {
        let c = self.lock()?;
        let (project, path) = file.unzip();
        let mut stmt = c
            .prepare(
                "SELECT id, connection_id, connection_name, driver, host, database, sql, started_at, duration_ms, rows, error, query_id, project_id, file_path
                 FROM query_history
                 WHERE (?1 IS NOT NULL AND query_id = ?1) OR (?2 IS NOT NULL AND project_id = ?2 AND file_path = ?3)
                 ORDER BY id DESC LIMIT ?4",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map(params![query_id, project, path, limit], row_to_history).map_err(db_err)?;
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
        drop(c);
        self.notify(StateChange::new("history", None));
        Ok(())
    }

    // -- query versions (a saved query's timeline) ---------------------------
    // Local, like the history: no `touch()`, not in the snapshot.

    /// Record `sql` as the query's newest version, unless it's blank, equals
    /// the newest one, or (with `throttle`) the newest is younger than that.
    /// `saved_at`: when the text was saved (RFC 3339). Prunes the query's old
    /// versions afterwards. `None` when nothing was recorded.
    pub fn add_query_version(&self, query_id: &str, sql: &str, saved_at: &str, throttle: Option<chrono::Duration>) -> Result<Option<QueryVersion>> {
        if sql.trim().is_empty() {
            return Ok(None);
        }
        let hash = text_hash(sql);
        let c = self.lock()?;
        let last: Option<(String, String, String)> = c
            .query_row(
                "SELECT saved_at, hash, sql FROM query_versions WHERE query_id = ?1 ORDER BY id DESC LIMIT 1",
                [query_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(db_err)?;
        if let Some((at, h, _)) = &last {
            if *h == hash {
                return Ok(None);
            }
            if let (Some(min), Some(prev), Some(this)) = (throttle, parse_ts(at), parse_ts(saved_at)) {
                if this - prev < min {
                    return Ok(None);
                }
            }
        }
        let (added, removed) = match &last {
            Some((_, _, prev)) => line_changes(prev, sql),
            None => (0, 0),
        };
        c.execute(
            "INSERT INTO query_versions (query_id, saved_at, sql, hash, added, removed) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![query_id, saved_at, sql, hash, added, removed],
        )
        .map_err(db_err)?;
        let id = c.last_insert_rowid();
        prune_versions(&c, query_id, chrono::Utc::now())?;
        Ok(Some(QueryVersion { id, query_id: query_id.to_string(), saved_at: saved_at.to_string(), added, removed, sql: None }))
    }

    /// The versions around a save of a query's text (`before`: the query as
    /// it was; `None` for a new one). The text it had is kept when no
    /// version has it yet and it stood for a while (or there are no versions
    /// at all, a query from before the timeline), so the state before an
    /// edit can always come back. Then the new text, throttled unless `force`
    /// (an explicit save, a run, closing the tab).
    pub fn version_query_save(&self, query_id: &str, before: Option<&SavedQuery>, sql: &str, force: bool) -> Result<()> {
        let min = chrono::Duration::seconds(VERSION_THROTTLE_SECS);
        let now = chrono::Utc::now();
        if let Some(b) = before.filter(|b| b.sql != sql && !b.sql.trim().is_empty()) {
            let newest: Option<String> = self
                .lock()?
                .query_row("SELECT hash FROM query_versions WHERE query_id = ?1 ORDER BY id DESC LIMIT 1", [query_id], |r| r.get(0))
                .optional()
                .map_err(db_err)?;
            let stood = parse_ts(&b.updated_at).is_none_or(|at| now - at >= min);
            let at = if parse_ts(&b.updated_at).is_some() { b.updated_at.clone() } else { now.to_rfc3339() };
            match newest {
                None => {
                    self.add_query_version(query_id, &b.sql, &at, None)?;
                }
                Some(h) if h != text_hash(&b.sql) && stood => {
                    self.add_query_version(query_id, &b.sql, &at, None)?;
                }
                _ => {}
            }
        }
        self.add_query_version(query_id, sql, &now.to_rfc3339(), (!force).then_some(min))?;
        Ok(())
    }

    /// A query's versions, newest first (without their text).
    pub fn list_query_versions(&self, query_id: &str) -> Result<Vec<QueryVersion>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT id, query_id, saved_at, added, removed FROM query_versions WHERE query_id = ?1 ORDER BY id DESC")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([query_id], |r| {
                Ok(QueryVersion { id: r.get(0)?, query_id: r.get(1)?, saved_at: r.get(2)?, added: r.get(3)?, removed: r.get(4)?, sql: None })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(db_err)
    }

    /// One version, with its text.
    pub fn get_query_version(&self, id: i64) -> Result<Option<QueryVersion>> {
        self.lock()?
            .query_row("SELECT id, query_id, saved_at, added, removed, sql FROM query_versions WHERE id = ?1", [id], |r| {
                Ok(QueryVersion { id: r.get(0)?, query_id: r.get(1)?, saved_at: r.get(2)?, added: r.get(3)?, removed: r.get(4)?, sql: r.get(5)? })
            })
            .optional()
            .map_err(db_err)
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
        drop(c);
        self.notify(StateChange::new("backup", Some(&b.id)).scoped(&b.connection_id, Some(&b.database)));
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
        self.notify(StateChange::new("backup", Some(id)));
        Ok(())
    }

    // -- scheduled tasks --------------------------------------------------
    // This machine's: they point at its connections and folders, so they
    // aren't a change to sync (no bump).

    pub fn list_tasks(&self) -> Result<Vec<ScheduledTask>> {
        let c = self.lock()?;
        let mut stmt = c.prepare("SELECT task_json FROM scheduled_tasks").map_err(db_err)?;
        let rows: Vec<String> = stmt.query_map([], |r| r.get(0)).map_err(db_err)?.collect::<std::result::Result<_, _>>().map_err(db_err)?;
        let mut tasks: Vec<ScheduledTask> = rows.iter().filter_map(|j| serde_json::from_str(j).ok()).collect();
        tasks.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(tasks)
    }

    pub fn get_task(&self, id: &str) -> Result<Option<ScheduledTask>> {
        let j: Option<String> = self
            .lock()?
            .query_row("SELECT task_json FROM scheduled_tasks WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        Ok(j.and_then(|j| serde_json::from_str(&j).ok()))
    }

    pub fn save_task(&self, task: &ScheduledTask) -> Result<ScheduledTask> {
        let mut t = task.clone();
        let stamp = now();
        if t.created_at.is_empty() {
            t.created_at = stamp.clone();
        }
        t.updated_at = stamp;
        self.lock()?
            .execute(
                "INSERT INTO scheduled_tasks (id, task_json, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET task_json = ?2, updated_at = ?3",
                params![t.id, serde_json::to_string(&t)?, t.updated_at],
            )
            .map_err(db_err)?;
        self.notify(StateChange::new("scheduled_task", Some(&t.id)));
        Ok(t)
    }

    pub fn delete_task(&self, id: &str) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM task_runs WHERE task_id = ?1", [id]).map_err(db_err)?;
        tx.execute("DELETE FROM scheduled_tasks WHERE id = ?1", [id]).map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        drop(c);
        self.notify(StateChange::new("scheduled_task", Some(id)));
        Ok(())
    }

    /// Writes a run (new or updated: the runner saves it as it goes). Keeps
    /// the last [`RUNS_KEPT`] of each task.
    pub fn save_task_run(&self, run: &TaskRun) -> Result<()> {
        let c = self.lock()?;
        c.execute(
            "INSERT INTO task_runs (id, task_id, started_at, status, run_json) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET status = ?4, run_json = ?5",
            params![run.id, run.task_id, run.started_at, serde_json::to_value(run.status)?.as_str().unwrap_or(""), serde_json::to_string(run)?],
        )
        .map_err(db_err)?;
        c.execute(
            "DELETE FROM task_runs WHERE task_id = ?1 AND id NOT IN
               (SELECT id FROM task_runs WHERE task_id = ?1 ORDER BY started_at DESC LIMIT ?2)",
            params![run.task_id, RUNS_KEPT],
        )
        .map_err(db_err)?;
        drop(c);
        self.notify(StateChange::new("task_run", Some(&run.task_id)));
        Ok(())
    }

    /// A task's runs (all tasks' when `None`), newest first.
    pub fn list_task_runs(&self, task_id: Option<&str>, limit: u32) -> Result<Vec<TaskRun>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT run_json FROM task_runs WHERE (?1 IS NULL OR task_id = ?1) ORDER BY started_at DESC LIMIT ?2")
            .map_err(db_err)?;
        let rows: Vec<String> =
            stmt.query_map(params![task_id, limit], |r| r.get(0)).map_err(db_err)?.collect::<std::result::Result<_, _>>().map_err(db_err)?;
        Ok(rows.iter().filter_map(|j| serde_json::from_str(j).ok()).collect())
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
        self.notify(StateChange::new("library", Some(&s.id)));
        Ok(LibraryScript { updated_at: ts, ..s.clone() })
    }

    pub fn delete_library_script(&self, id: &str) -> Result<()> {
        self.lock()?.execute("DELETE FROM library WHERE id = ?1", [id]).map_err(db_err)?;
        self.touch()?;
        self.notify(StateChange::new("library", Some(id)));
        Ok(())
    }

    // -- projects -------------------------------------------------------
    // Folders on this machine: not a change to sync, not in a backup.

    /// Every project, by `sort_order` then name.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare(
                "SELECT id, name, path, binding_json, sort_order, created_at, updated_at FROM projects
                 ORDER BY sort_order, name COLLATE NOCASE",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map([], row_to_project).map_err(db_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_err)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        self.lock()?
            .query_row(
                "SELECT id, name, path, binding_json, sort_order, created_at, updated_at FROM projects WHERE id = ?1",
                [id],
                row_to_project,
            )
            .optional()
            .map_err(db_err)
    }

    /// Insert or update. A path already linked by another project is refused.
    pub fn save_project(&self, p: &Project) -> Result<Project> {
        let ts = now();
        let c = self.lock()?;
        let taken: Option<String> = c
            .query_row("SELECT id FROM projects WHERE path = ?1 AND id <> ?2", params![p.path, p.id], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        if taken.is_some() {
            return Err(Error::State("esa carpeta ya está vinculada como proyecto".into()));
        }
        let created = if p.created_at.is_empty() { ts.clone() } else { p.created_at.clone() };
        c.execute(
            "INSERT INTO projects (id, name, path, binding_json, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET name = ?2, path = ?3, binding_json = ?4, sort_order = ?5, updated_at = ?7",
            params![p.id, p.name, p.path, serde_json::to_string(&p.binding)?, p.sort_order, created, ts],
        )
        .map_err(db_err)?;
        let saved = c
            .query_row(
                "SELECT id, name, path, binding_json, sort_order, created_at, updated_at FROM projects WHERE id = ?1",
                [&p.id],
                row_to_project,
            )
            .map_err(db_err)?;
        drop(c);
        self.notify(StateChange::new("project", Some(&p.id)));
        Ok(saved)
    }

    pub fn set_project_binding(&self, id: &str, binding: &ProjectBinding) -> Result<Project> {
        let n = self
            .lock()?
            .execute(
                "UPDATE projects SET binding_json = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, serde_json::to_string(binding)?, now()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::State("el proyecto no existe".into()));
        }
        self.notify(StateChange::new("project", Some(id)));
        self.get_project(id)?.ok_or_else(|| Error::State("el proyecto no existe".into()))
    }

    /// Removes the row only: the folder is the user's and stays as it is.
    pub fn delete_project(&self, id: &str) -> Result<()> {
        self.lock()?.execute("DELETE FROM projects WHERE id = ?1", [id]).map_err(db_err)?;
        self.notify(StateChange::new("project", Some(id)));
        Ok(())
    }

    /// The order given; projects left out keep theirs, after these.
    pub fn reorder_projects(&self, ids: &[String]) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(db_err)?;
        let n = ids.len() as i64;
        tx.execute("UPDATE projects SET sort_order = sort_order + ?1", [n]).map_err(db_err)?;
        for (i, id) in ids.iter().enumerate() {
            tx.execute("UPDATE projects SET sort_order = ?2 WHERE id = ?1", params![id, i as i64]).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        drop(c);
        self.notify(StateChange::new("project", None));
        Ok(())
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
        drop(c);
        self.notify(StateChange::new("setting", Some(key)));
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
        // Like connections, the order is the snapshot's (older ones list folders by name).
        for (i, f) in snap.folders.iter().enumerate() {
            tx.execute(
                "INSERT INTO folders (id, name, parent_id, color, sort_order) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![f.id, f.name, f.parent_id, f.color, i as i64],
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
        tx.commit().map_err(db_err)?;
        drop(c);
        self.notify(StateChange::new("restore", None));
        Ok(())
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

fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let binding: String = r.get(3)?;
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        path: r.get(2)?,
        binding: serde_json::from_str(&binding).unwrap_or_default(),
        sort_order: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

fn text_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn parse_ts(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&chrono::Utc))
}

/// Lines added and removed going from `a` to `b` (a changed line counts as
/// both, as in git). Past a size it counts the differing middle as replaced.
pub fn line_changes(a: &str, b: &str) -> (u32, u32) {
    let x: Vec<&str> = a.lines().collect();
    let y: Vec<&str> = b.lines().collect();
    let pre = x.iter().zip(&y).take_while(|(p, q)| p == q).count();
    let (x, y) = (&x[pre..], &y[pre..]);
    let suf = x.iter().rev().zip(y.iter().rev()).take_while(|(p, q)| p == q).count();
    let (x, y) = (&x[..x.len() - suf], &y[..y.len() - suf]);
    let (n, m) = (x.len(), y.len());
    if n == 0 || m == 0 || n.saturating_mul(m) > 4_000_000 {
        return (m as u32, n as u32);
    }
    // The longest common subsequence's length, one row at a time.
    let mut prev = vec![0u32; m + 1];
    let mut cur = vec![0u32; m + 1];
    for xi in x {
        for (j, yj) in y.iter().enumerate() {
            cur[j + 1] = if xi == yj { prev[j] + 1 } else { cur[j].max(prev[j + 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let common = prev[m] as usize;
    ((m - common) as u32, (n - common) as u32)
}

/// Which versions go (`rows`: id and when, newest first): every one of the
/// last `VERSIONS_ALL_DAYS`, then the newest of each day up to
/// `VERSIONS_DAILY_DAYS`, and never more than `VERSIONS_MAX`.
fn versions_to_drop(rows: &[(i64, chrono::DateTime<chrono::Utc>)], now: chrono::DateTime<chrono::Utc>) -> Vec<i64> {
    let mut keep = 0usize;
    let mut last_day = None;
    let mut drop = Vec::new();
    for (id, at) in rows {
        let age = now - *at;
        let day = at.date_naive();
        let kept = if keep >= VERSIONS_MAX || age > chrono::Duration::days(VERSIONS_DAILY_DAYS) {
            false
        } else if age <= chrono::Duration::days(VERSIONS_ALL_DAYS) {
            true
        } else {
            last_day != Some(day)
        };
        if kept {
            keep += 1;
            last_day = Some(day);
        } else {
            drop.push(*id);
        }
    }
    drop
}

fn prune_versions(c: &Connection, query_id: &str, now: chrono::DateTime<chrono::Utc>) -> Result<()> {
    let mut stmt = c.prepare("SELECT id, saved_at FROM query_versions WHERE query_id = ?1 ORDER BY id DESC").map_err(db_err)?;
    let rows: Vec<(i64, String)> =
        stmt.query_map([query_id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(db_err)?.collect::<rusqlite::Result<_>>().map_err(db_err)?;
    // An unreadable date counts as now (kept while recent ones are).
    let rows: Vec<_> = rows.into_iter().map(|(id, at)| (id, parse_ts(&at).unwrap_or(now))).collect();
    for id in versions_to_drop(&rows, now) {
        c.execute("DELETE FROM query_versions WHERE id = ?1", [id]).map_err(db_err)?;
    }
    Ok(())
}

fn row_to_history(r: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
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
        query_id: r.get(11)?,
        project_id: r.get(12)?,
        file_path: r.get(13)?,
    })
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

    fn recording(s: &StateStore) -> Arc<Mutex<Vec<StateChange>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        s.set_change_hook(Box::new(move |c| sink.lock().unwrap().push(c)));
        seen
    }

    fn kinds(seen: &Arc<Mutex<Vec<StateChange>>>) -> Vec<String> {
        seen.lock().unwrap().drain(..).map(|c| c.kind).collect()
    }

    #[test]
    fn change_hook_fires_once_per_successful_write() {
        let s = StateStore::open_in_memory().unwrap();
        let seen = recording(&s);

        s.save_connection(&conn("c1")).unwrap();
        s.save_folder(&folder("f", None)).unwrap();
        s.move_connection("c1", Some("f")).unwrap();
        s.reorder_explorer(ExplorerItem::Connection, Some("f"), &["c1".into()]).unwrap();
        assert_eq!(kinds(&seen), ["connection", "folder", "explorer", "explorer"]);

        let q = SavedQuery { id: "q1".into(), connection_id: "c1".into(), database: "db".into(), name: "n".into(), sql: "select 1".into(), updated_at: String::new(), last_run_at: None };
        s.save_query(&q).unwrap();
        {
            let got = seen.lock().unwrap();
            assert_eq!(got[0], StateChange { kind: "query".into(), id: Some("q1".into()), connection_id: Some("c1".into()), database: Some("db".into()) });
        }
        s.delete_query("q1").unwrap();
        assert_eq!(kinds(&seen), ["query", "query"]);

        let m = SavedMigration { id: "m1".into(), connection_id: "c1".into(), database: "db".into(), name: "m".into(), config: serde_json::json!({}), run_ids: vec![], created_at: String::new(), updated_at: String::new() };
        s.save_migration(&m).unwrap();
        s.rename_migration("m1", "otra").unwrap();
        s.link_migration_run("m1", "r1").unwrap();
        s.delete_migration("m1").unwrap();
        assert_eq!(kinds(&seen), ["migration", "migration", "migration", "migration"]);

        s.set_setting("ui.theme", Some(&serde_json::json!("dark"))).unwrap();
        let lib = LibraryScript { id: "l1".into(), name: "n".into(), folder: String::new(), description: String::new(), engines: vec![], text: "x".into(), updated_at: String::new() };
        s.save_library_script(&lib).unwrap();
        s.delete_library_script("l1").unwrap();
        assert_eq!(kinds(&seen), ["setting", "library", "library"]);

        let h = HistoryEntry { id: 0, connection_id: "c1".into(), connection_name: "local".into(), driver: "postgres".into(), host: "h".into(), database: "db".into(), sql: "select 1".into(), started_at: now(), duration_ms: 1, rows: None, error: None, ..Default::default() };
        s.add_history(&h).unwrap();
        s.delete_history(None).unwrap();
        assert_eq!(kinds(&seen), ["history", "history"]);

        let snap = s.snapshot().unwrap();
        s.replace_all(&snap).unwrap();
        s.delete_folder("f").unwrap();
        s.delete_connection("c1").unwrap();
        assert_eq!(kinds(&seen), ["restore", "folder", "connection"]);

        // Reads never fire.
        s.list_connections().unwrap();
        s.get_setting("ui.theme").unwrap();
        assert!(kinds(&seen).is_empty());
    }

    #[test]
    fn change_hook_does_not_fire_on_a_failed_write() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_folder(&folder("a", None)).unwrap();
        s.save_folder(&folder("b", Some("a"))).unwrap();
        let seen = recording(&s);
        assert!(s.save_folder(&folder("a", Some("b"))).is_err());
        assert!(s.reorder_explorer(ExplorerItem::Folder, Some("b"), &["a".into()]).is_err());
        assert!(s.rename_migration("missing", "x").is_err());
        assert!(s.link_migration_run("missing", "r").is_err());
        assert!(kinds(&seen).is_empty());
    }

    #[test]
    fn change_hook_can_read_the_store() {
        // The hook runs with no lock held: reading back must not deadlock.
        let s = Arc::new(StateStore::open_in_memory().unwrap());
        let seen = Arc::new(Mutex::new(0usize));
        let (store, sink) = (Arc::downgrade(&s), seen.clone());
        s.set_change_hook(Box::new(move |_| {
            if let Some(store) = store.upgrade() {
                *sink.lock().unwrap() += store.list_connections().unwrap().len() + store.list_settings().unwrap().len();
            }
        }));
        s.save_connection(&conn("c1")).unwrap();
        s.set_setting("ui.theme", Some(&serde_json::json!("dark"))).unwrap();
        s.add_history(&HistoryEntry { id: 0, connection_id: "c1".into(), connection_name: "l".into(), driver: "postgres".into(), host: "h".into(), database: "db".into(), sql: "x".into(), started_at: now(), duration_ms: 0, rows: None, error: None, ..Default::default() }).unwrap();
        assert_eq!(*seen.lock().unwrap(), 1 + 2 + 2);
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

    fn folder_ids(s: &StateStore, parent: Option<&str>) -> Vec<String> {
        s.list_folders().unwrap().into_iter().filter(|f| f.parent_id.as_deref() == parent).map(|f| f.id).collect()
    }

    fn conn_ids(s: &StateStore, folder: Option<&str>) -> Vec<String> {
        s.list_connections().unwrap().into_iter().filter(|c| c.folder_id.as_deref() == folder).map(|c| c.id).collect()
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn old_state_files_gain_the_folder_order_and_keep_name_order() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE folders (id TEXT PRIMARY KEY, name TEXT NOT NULL, parent_id TEXT, color TEXT);
             INSERT INTO folders (id, name) VALUES ('z', 'Zeta'), ('a', 'alfa'), ('m', 'Medio');",
        )
        .unwrap();
        let s = StateStore::init(c).unwrap();
        assert_eq!(folder_ids(&s, None), ["a", "m", "z"]);
        s.save_folder(&folder("b", None)).unwrap();
        assert_eq!(folder_ids(&s, None), ["a", "m", "z", "b"], "a new folder goes last");
        s.reorder_explorer(ExplorerItem::Folder, None, &ids(&["z", "b", "a", "m"])).unwrap();
        assert_eq!(folder_ids(&s, None), ["z", "b", "a", "m"]);
    }

    #[test]
    fn reorder_sets_the_level_and_order_in_one_go() {
        let s = StateStore::open_in_memory().unwrap();
        for id in ["c1", "c2", "c3"] {
            s.save_connection(&conn(id)).unwrap();
        }
        assert_eq!(conn_ids(&s, None), ["c1", "c2", "c3"], "new connections go last, not by name");
        s.save_folder(&folder("f", None)).unwrap();
        s.save_connection(&SavedConnection { folder_id: Some("f".into()), ..conn("c4") }).unwrap();

        s.reorder_explorer(ExplorerItem::Connection, None, &ids(&["c3", "c1", "c2"])).unwrap();
        assert_eq!(conn_ids(&s, None), ["c3", "c1", "c2"]);
        // Into another level: it leaves the top level, its new siblings are renumbered.
        s.reorder_explorer(ExplorerItem::Connection, Some("f"), &ids(&["c1", "c4"])).unwrap();
        assert_eq!(conn_ids(&s, Some("f")), ["c1", "c4"]);
        assert_eq!(conn_ids(&s, None), ["c3", "c2"]);
        // Editing keeps its place; changing folder by editing puts it last there.
        s.save_connection(&SavedConnection { folder_id: Some("f".into()), name: "otro".into(), ..conn("c1") }).unwrap();
        assert_eq!(conn_ids(&s, Some("f")), ["c1", "c4"]);
        s.save_connection(&conn("c4")).unwrap();
        assert_eq!(conn_ids(&s, None), ["c3", "c2", "c4"]);
        s.move_connection("c3", Some("f")).unwrap();
        assert_eq!(conn_ids(&s, Some("f")), ["c1", "c3"]);
    }

    #[test]
    fn reorder_refuses_a_folder_inside_its_own_subtree() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_folder(&folder("a", None)).unwrap();
        s.save_folder(&folder("b", Some("a"))).unwrap();
        s.save_folder(&folder("c", Some("b"))).unwrap();
        let rev = s.revision().unwrap();
        assert!(s.reorder_explorer(ExplorerItem::Folder, Some("a"), &ids(&["a"])).is_err());
        assert!(s.reorder_explorer(ExplorerItem::Folder, Some("c"), &ids(&["a"])).is_err());
        assert_eq!(s.revision().unwrap(), rev, "nothing changed");
        assert_eq!(folder_ids(&s, None), ["a"]);
        // Up a level is fine.
        s.reorder_explorer(ExplorerItem::Folder, None, &ids(&["c", "a"])).unwrap();
        assert_eq!(folder_ids(&s, None), ["c", "a"]);
        assert_eq!(folder_ids(&s, Some("b")), Vec::<String>::new());
    }

    #[test]
    fn the_explorer_order_travels_in_the_snapshot() {
        let s = StateStore::open_in_memory().unwrap();
        for id in ["x", "y", "z"] {
            s.save_folder(&folder(id, None)).unwrap();
        }
        s.save_folder(&folder("y1", Some("y"))).unwrap();
        s.save_folder(&folder("y2", Some("y"))).unwrap();
        for id in ["c1", "c2"] {
            s.save_connection(&conn(id)).unwrap();
        }
        s.reorder_explorer(ExplorerItem::Folder, None, &ids(&["z", "x", "y"])).unwrap();
        s.reorder_explorer(ExplorerItem::Folder, Some("y"), &ids(&["y2", "y1"])).unwrap();
        s.reorder_explorer(ExplorerItem::Connection, None, &ids(&["c2", "c1"])).unwrap();
        let json = serde_json::to_string(&s.snapshot().unwrap()).unwrap();
        let other = StateStore::open_in_memory().unwrap();
        other.replace_all(&serde_json::from_str(&json).unwrap()).unwrap();
        assert_eq!(folder_ids(&other, None), ["z", "x", "y"]);
        assert_eq!(folder_ids(&other, Some("y")), ["y2", "y1"]);
        assert_eq!(conn_ids(&other, None), ["c2", "c1"]);

        // An older snapshot (folders by name, no order of their own) restores as it was listed.
        let old = r#"{"connections": [], "queries": [], "folders": [
            {"id": "b", "name": "Beta"}, {"id": "a", "name": "Alfa", "parent_id": "b"}, {"id": "c", "name": "Gamma"}]}"#;
        other.replace_all(&serde_json::from_str(old).unwrap()).unwrap();
        assert_eq!(folder_ids(&other, None), ["b", "c"]);
        assert_eq!(folder_ids(&other, Some("b")), ["a"]);
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
            ..Default::default()
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

    fn project(id: &str, name: &str, path: &str) -> Project {
        Project { id: id.into(), name: name.into(), path: path.into(), ..Default::default() }
    }

    #[test]
    fn projects_are_local_ordered_and_unique_per_folder() {
        let s = StateStore::open_in_memory().unwrap();
        let seen = recording(&s);
        let rev = s.revision().unwrap();

        let a = s.save_project(&project("p1", "Ventas", "/repos/ventas")).unwrap();
        assert!(!a.created_at.is_empty());
        s.save_project(&project("p2", "Analytics", "/repos/analytics")).unwrap();
        assert_eq!(kinds(&seen), ["project", "project"]);
        // Same folder twice: refused, and nothing fires.
        let err = s.save_project(&project("p3", "Otro", "/repos/ventas")).unwrap_err();
        assert!(err.to_string().contains("ya está vinculada"));
        assert!(kinds(&seen).is_empty());
        // Updating the same project keeps its path.
        s.save_project(&Project { name: "Ventas 2".into(), ..a.clone() }).unwrap();
        assert_eq!(s.get_project("p1").unwrap().unwrap().name, "Ventas 2");
        assert_eq!(s.get_project("p1").unwrap().unwrap().created_at, a.created_at);

        // Same order: by name.
        let names = |s: &StateStore| s.list_projects().unwrap().into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(names(&s), ["p2", "p1"]);
        s.reorder_projects(&["p1".into(), "p2".into()]).unwrap();
        assert_eq!(names(&s), ["p1", "p2"]);

        let binding = ProjectBinding {
            direct: Some(ProjectTarget { connection_id: "c1".into(), database: "ventas".into() }),
            environments: [("prod".to_string(), ProjectTarget { connection_id: "c2".into(), database: "v".into() })].into(),
            active_environment: Some("prod".into()),
        };
        let p = s.set_project_binding("p1", &binding).unwrap();
        assert_eq!(p.binding, binding);
        assert!(s.set_project_binding("nope", &binding).is_err());
        kinds(&seen);

        // Local only: no revision bump, not in a snapshot, kept by a restore.
        assert_eq!(s.revision().unwrap(), rev);
        let snap = s.snapshot().unwrap();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("/repos/ventas"));
        s.replace_all(&snap).unwrap();
        assert_eq!(names(&s), ["p1", "p2"]);
        assert_eq!(s.get_project("p1").unwrap().unwrap().binding, binding);
        kinds(&seen);

        s.delete_project("p2").unwrap();
        assert_eq!(kinds(&seen), ["project"]);
        assert_eq!(names(&s), ["p1"]);
        // A freed folder can be linked again.
        s.save_project(&project("p4", "Analytics", "/repos/analytics")).unwrap();
    }

    #[test]
    fn scheduled_tasks_and_runs() {
        use crate::tasks::{RunStatus, ScheduledTask, Step, TaskRun};
        let s = StateStore::open_in_memory().unwrap();
        let t = ScheduledTask { id: "t1".into(), name: "Reporte".into(), steps: vec![Step::default()], ..Default::default() };
        let saved = s.save_task(&t).unwrap();
        assert!(!saved.created_at.is_empty());
        assert_eq!(s.get_task("t1").unwrap().unwrap().name, "Reporte");
        // Not a change to sync.
        assert_eq!(s.revision().unwrap(), 0);
        for i in 0..(RUNS_KEPT + 5) {
            let run = TaskRun { id: format!("r{i}"), task_id: "t1".into(), started_at: format!("2026-10-08 10:{i:04}"), status: RunStatus::Ok, ..Default::default() };
            s.save_task_run(&run).unwrap();
        }
        let runs = s.list_task_runs(Some("t1"), 1000).unwrap();
        assert_eq!(runs.len(), RUNS_KEPT as usize);
        assert_eq!(runs[0].id, format!("r{}", RUNS_KEPT + 4));
        // The snapshot doesn't carry them.
        assert!(!serde_json::to_string(&s.snapshot().unwrap()).unwrap().contains("Reporte"));
        s.delete_task("t1").unwrap();
        assert!(s.list_tasks().unwrap().is_empty());
        assert!(s.list_task_runs(None, 10).unwrap().is_empty());
    }

    fn saved(s: &StateStore, id: &str, sql: &str, updated_at: &str) -> SavedQuery {
        let q = SavedQuery { id: id.into(), connection_id: "c1".into(), database: "db".into(), name: "q".into(), sql: sql.into(), updated_at: String::new(), last_run_at: None };
        s.save_query(&q).unwrap();
        SavedQuery { updated_at: updated_at.into(), ..q }
    }

    fn texts(s: &StateStore, q: &str) -> Vec<String> {
        s.list_query_versions(q).unwrap().iter().map(|v| s.get_query_version(v.id).unwrap().unwrap().sql.unwrap()).collect()
    }

    #[test]
    fn query_versions_dedupe_throttle_and_stay_local() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_connection(&conn("c1")).unwrap();
        saved(&s, "q1", "", "");
        let rev = s.revision().unwrap();
        let min = chrono::Duration::seconds(VERSION_THROTTLE_SECS);
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(10);
        let at = |secs: i64| (t0 + chrono::Duration::seconds(secs)).to_rfc3339();
        assert!(s.add_query_version("q1", "  \n", &at(0), Some(min)).unwrap().is_none(), "a blank text isn't a version");
        let v1 = s.add_query_version("q1", "select 1\nfrom a", &at(0), Some(min)).unwrap().unwrap();
        assert_eq!((v1.added, v1.removed), (0, 0));
        assert!(s.add_query_version("q1", "select 1\nfrom a", &at(120), None).unwrap().is_none(), "same text as the newest");
        assert!(s.add_query_version("q1", "select 2\nfrom a", &at(30), Some(min)).unwrap().is_none(), "throttled");
        let v2 = s.add_query_version("q1", "select 2\nfrom a\nwhere x", &at(30), None).unwrap().unwrap();
        assert_eq!((v2.added, v2.removed), (2, 1));
        assert!(s.add_query_version("q1", "select 3", &at(95), Some(min)).unwrap().unwrap().id > v2.id);
        assert_eq!(texts(&s, "q1"), ["select 3", "select 2\nfrom a\nwhere x", "select 1\nfrom a"]);
        assert_eq!(s.revision().unwrap(), rev, "versions aren't a change to sync");
        assert!(!serde_json::to_string(&s.snapshot().unwrap()).unwrap().contains("where x"));
        // Deleting the query takes its versions.
        s.delete_query("q1").unwrap();
        assert!(s.list_query_versions("q1").unwrap().is_empty());
    }

    #[test]
    fn a_save_keeps_the_text_before_it() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_connection(&conn("c1")).unwrap();
        // A query from before the timeline: its text becomes the first version.
        let old = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
        let before = saved(&s, "q1", "select viejo", &old);
        s.version_query_save("q1", Some(&before), "select nuevo", false).unwrap();
        assert_eq!(texts(&s, "q1"), ["select nuevo", "select viejo"]);
        // Typing on: throttled, and the text a moment ago isn't kept either.
        let recent = chrono::Utc::now().to_rfc3339();
        let before = saved(&s, "q1", "select nuevo 2", &recent);
        s.version_query_save("q1", Some(&before), "select nuevo 3", false).unwrap();
        assert_eq!(texts(&s, "q1").len(), 2);
        // An explicit save always records.
        s.version_query_save("q1", Some(&before), "select nuevo 3", true).unwrap();
        assert_eq!(texts(&s, "q1")[0], "select nuevo 3");
        // A text that stood for a while (say the app closed before a version)
        // is kept before the next edit replaces it.
        let before = saved(&s, "q1", "select quieto", &(chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339());
        s.version_query_save("q1", Some(&before), "select editado", false).unwrap();
        assert_eq!(texts(&s, "q1")[..2], ["select editado".to_string(), "select quieto".to_string()]);
    }

    #[test]
    fn old_versions_thin_out() {
        let now = chrono::Utc::now();
        let ago = |h: i64| now - chrono::Duration::hours(h);
        // Newest first: three today, two on day 10 (one goes), one on day 20, one past 90 days.
        let rows = vec![(9, ago(1)), (8, ago(2)), (7, ago(3)), (6, ago(240)), (5, ago(240) - chrono::Duration::minutes(5)), (4, ago(480)), (3, ago(24 * 100))];
        let dropped = versions_to_drop(&rows, now);
        assert!(dropped.contains(&3) && !dropped.contains(&6) && !dropped.contains(&4) && !dropped.contains(&9));
        // Same calendar day as id 6 (5 minutes before), unless it crossed midnight.
        assert_eq!(dropped.contains(&5), rows[3].1.date_naive() == rows[4].1.date_naive());
        // A cap per query, newest kept.
        let n = VERSIONS_MAX as i64 + 10;
        let many: Vec<_> = (0..n).map(|i| (n - i, now - chrono::Duration::seconds(i))).collect();
        let dropped = versions_to_drop(&many, now);
        assert_eq!(dropped, (1..=10).rev().collect::<Vec<i64>>());
    }

    #[test]
    fn line_changes_count_like_git() {
        assert_eq!(line_changes("a\nb\nc", "a\nb\nc"), (0, 0));
        assert_eq!(line_changes("a\nb\nc", "a\nB\nc\nd"), (2, 1));
        assert_eq!(line_changes("", "a\nb"), (2, 0));
        assert_eq!(line_changes("a\nb", ""), (0, 2));
        assert_eq!(line_changes("x\na\ny\nb", "a\nb\nz"), (1, 2));
    }

    #[test]
    fn runs_are_listed_by_query_and_by_file() {
        let s = StateStore::open_in_memory().unwrap();
        let run = |sql: &str, q: Option<&str>, f: Option<(&str, &str)>| HistoryEntry {
            connection_id: "c1".into(), sql: sql.into(), started_at: now(),
            query_id: q.map(Into::into), project_id: f.map(|f| f.0.into()), file_path: f.map(|f| f.1.into()),
            ..Default::default()
        };
        s.add_history(&run("select 1", Some("q1"), None)).unwrap();
        s.add_history(&run("select 2", None, Some(("p1", "a.sql")))).unwrap();
        s.add_history(&run("select 3", None, Some(("p1", "b.sql")))).unwrap();
        s.add_history(&run("select 4", Some("q1"), None)).unwrap();
        s.add_history(&run("select 5", None, None)).unwrap();
        let sqls = |v: Vec<HistoryEntry>| v.into_iter().map(|e| e.sql).collect::<Vec<_>>();
        assert_eq!(sqls(s.list_history_of(Some("q1"), None, 10).unwrap()), ["select 4", "select 1"]);
        assert_eq!(sqls(s.list_history_of(None, Some(("p1", "a.sql")), 10).unwrap()), ["select 2"]);
        assert!(s.list_history_of(None, None, 10).unwrap().is_empty());
        assert_eq!(s.list_history(None, None, 10).unwrap()[3].project_id.as_deref(), Some("p1"));
    }

    #[test]
    fn old_history_tables_gain_the_origin_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE query_history (id INTEGER PRIMARY KEY AUTOINCREMENT, connection_id TEXT NOT NULL, connection_name TEXT NOT NULL,
                 driver TEXT NOT NULL, host TEXT NOT NULL, database TEXT NOT NULL, sql TEXT NOT NULL, started_at TEXT NOT NULL,
                 duration_ms INTEGER NOT NULL, rows INTEGER, error TEXT);
                 INSERT INTO query_history (connection_id, connection_name, driver, host, database, sql, started_at, duration_ms)
                 VALUES ('c1', 'l', 'postgres', 'h', 'db', 'select viejo', '2026-01-01T00:00:00Z', 1);",
            )
            .unwrap();
        let s = StateStore::open(&path).unwrap();
        let all = s.list_history(None, None, 10).unwrap();
        assert_eq!((all[0].sql.as_str(), all[0].query_id.as_deref()), ("select viejo", None));
        drop(s);
        assert!(StateStore::open(&path).is_ok(), "opening it again changes nothing");
    }
}

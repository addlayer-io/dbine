//! The server's own backups (the "Backups" tab, docs/backups.md): what the
//! engine has made (`Session::backups`) and the code that makes, restores or
//! deletes one (`Driver::backup_script`), which DBine shows and runs only
//! when the user says so. DBine's own copies (a script with the structure
//! and the data, for every engine) don't go through the driver.

use crate::info::Field;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What the tab offers for an engine's native backups (`Driver::backup`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupSpec {
    /// Options of a new backup (type, destination, compression…), shown as
    /// a form; their values reach `BackupAction::Backup::options`.
    #[serde(default)]
    pub backup_options: Vec<Field>,
    /// It can restore (from an entry of the history or a location).
    pub restore: bool,
    #[serde(default)]
    pub restore_options: Vec<Field>,
    /// A backup of the history can be deleted.
    pub delete: bool,
    /// `Session::backups` lists what the server has.
    pub history: bool,
    /// A backup covers the whole server (Redis, etcd, a snapshot of every
    /// index…), not one database: the tab opens from the connection.
    pub server_wide: bool,
    /// Where the scripts must run (SQL Server's `master` for a restore);
    /// "": the tab's database.
    #[serde(deserialize_with = "crate::serde_static::str", default)]
    pub script_database: &'static str,
    /// A short note for the tab, in Spanish (where the files end up, what
    /// the server needs…).
    #[serde(deserialize_with = "crate::serde_static::str", default)]
    pub note: &'static str,
}

/// One backup the server knows about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackupEntry {
    /// What restoring or deleting it takes (a backup set id, a snapshot
    /// name, a path, an ARN…).
    pub id: String,
    /// The database / table / index it's of; `None`: the whole server.
    pub database: Option<String>,
    /// Full, differential, log, snapshot, incremental…
    pub kind: Option<String>,
    /// ISO 8601, as the server reports it.
    pub started: Option<String>,
    pub finished: Option<String>,
    pub size: Option<u64>,
    /// Where it is (a file on the server, a repository, a bucket…).
    pub location: Option<String>,
    /// As the server says it: completed, failed, in progress…
    pub status: Option<String>,
    /// Other facts, Spanish labels.
    pub details: Vec<(String, String)>,
    /// It can be restored from DBine.
    pub restorable: bool,
}

/// A change `Driver::backup_script` writes the code for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BackupAction {
    /// Back up `database` (`None`: the server, when `server_wide`).
    Backup { database: Option<String>, options: BTreeMap<String, String> },
    /// Restore `source` (a `BackupEntry::id`, or a location the user typed)
    /// into `database`, which may be a new name.
    Restore { source: String, database: Option<String>, options: BTreeMap<String, String> },
    Delete { source: String },
}

//! A run's state in SQLite, so it survives a cut (even `kill -9`).
//!
//! Tables `transfer_runs` and `transfer_tables`, created on open. A table's
//! `rows_done` are committed rows, written at most every
//! [`crate::PROGRESS_EVERY`]; what decides whether a table must be emptied
//! before copying is `attempts` (a copy started), written before any row.
//! A table synced by rows is flagged `delta` and is never emptied.

use crate::event::CopyStats;
use crate::job::{RunOptions, TransferJob};
use crate::lock;
use dbine_driver::{Error, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// A run as it was asked for (what `resume` runs again).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    pub jobs: Vec<TransferJob>,
    pub options: RunOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Running,
    /// The process ended while it ran; it can be resumed.
    Interrupted,
    Finished,
    /// Stopped at the first failure (`fail_fast`).
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableStatus {
    #[default]
    Pending,
    Running,
    /// Rows copied; its `post` statements are missing.
    Copied,
    Done,
    Failed,
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "interrupted" => RunStatus::Interrupted,
            "finished" => RunStatus::Finished,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            _ => RunStatus::Running,
        }
    }
}

impl TableStatus {
    fn as_str(self) -> &'static str {
        match self {
            TableStatus::Pending => "pending",
            TableStatus::Running => "running",
            TableStatus::Copied => "copied",
            TableStatus::Done => "done",
            TableStatus::Failed => "failed",
            TableStatus::Cancelled => "cancelled",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "running" => TableStatus::Running,
            "copied" => TableStatus::Copied,
            "done" => TableStatus::Done,
            "failed" => TableStatus::Failed,
            "cancelled" => TableStatus::Cancelled,
            _ => TableStatus::Pending,
        }
    }
}

/// A run's row.
#[derive(Debug, Clone, Serialize)]
pub struct RunState {
    pub id: String,
    pub status: RunStatus,
    pub created_at: String,
    pub updated_at: String,
    /// Time it spent running, over all its segments.
    pub active_ms: u64,
}

/// A table's row.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TableState {
    pub name: String,
    pub status: TableStatus,
    pub rows_total: Option<u64>,
    /// Committed rows.
    pub rows_done: u64,
    /// Every row is in: never emptied again.
    pub copied: bool,
    /// Copies started, over every resume.
    pub attempts: u32,
    /// Synced by rows: never emptied (a failed sync rolled back on its own).
    pub delta: bool,
    pub error: Option<String>,
    pub stats: Option<CopyStats>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// The state file.
pub struct Store {
    conn: Mutex<Connection>,
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::State(format!("estado de la transferencia: {e}"))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS transfer_runs (
    id TEXT PRIMARY KEY,
    spec_json TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    active_ms INTEGER NOT NULL DEFAULT 0,
    segment_start_ms INTEGER,
    updated_ms INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS transfer_tables (
    run_id TEXT NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    rows_total INTEGER,
    rows_done INTEGER NOT NULL DEFAULT 0,
    copied INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    stats_json TEXT,
    started_at TEXT,
    finished_at TEXT,
    delta INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, name)
);
";

const TABLE_COLUMNS: &str = "name, status, rows_total, rows_done, copied, attempts, error, stats_json, started_at, finished_at, delta";

fn table_row(r: &rusqlite::Row) -> rusqlite::Result<TableState> {
    let stats: Option<String> = r.get(7)?;
    Ok(TableState {
        name: r.get(0)?,
        status: TableStatus::parse(&r.get::<_, String>(1)?),
        rows_total: r.get::<_, Option<i64>>(2)?.map(|n| n as u64),
        rows_done: r.get::<_, i64>(3)? as u64,
        copied: r.get::<_, i64>(4)? != 0,
        attempts: r.get::<_, i64>(5)? as u32,
        error: r.get(6)?,
        stats: stats.and_then(|s| serde_json::from_str(&s).ok()),
        started_at: r.get(8)?,
        finished_at: r.get(9)?,
        delta: r.get::<_, i64>(10)? != 0,
    })
}

impl Store {
    /// Open (or create) the state file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(err)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(err)?;
        // FULL: a commit survives a power cut too, not only a killed
        // process (a lost `begin_copy` would resume on top of rows).
        conn.pragma_update(None, "synchronous", "FULL").map_err(err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(err)?;
        conn.execute_batch(SCHEMA).map_err(err)?;
        // State files from before sync by rows.
        if conn.prepare("SELECT delta FROM transfer_tables LIMIT 0").is_err() {
            conn.execute_batch("ALTER TABLE transfer_tables ADD COLUMN delta INTEGER NOT NULL DEFAULT 0").map_err(err)?;
        }
        Ok(Store { conn: Mutex::new(conn) })
    }

    /// At startup: runs left `running` by a process that ended become
    /// `interrupted` (their active time closes at their last activity), and
    /// their running tables go back to pending (or copied). Returns how many
    /// runs. Call it before any run starts in this process.
    pub fn mark_running_as_interrupted(&self) -> Result<usize> {
        let mut c = lock(&self.conn);
        let tx = c.transaction().map_err(err)?;
        tx.execute(
            "UPDATE transfer_tables SET status = CASE copied WHEN 1 THEN 'copied' ELSE 'pending' END
             WHERE status = 'running' AND run_id IN (SELECT id FROM transfer_runs WHERE status = 'running')",
            [],
        )
        .map_err(err)?;
        let n = tx
            .execute(
                "UPDATE transfer_runs SET status = 'interrupted',
                    active_ms = active_ms + MAX(0, updated_ms - COALESCE(segment_start_ms, updated_ms)),
                    segment_start_ms = NULL
                 WHERE status = 'running'",
                [],
            )
            .map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(n)
    }

    pub fn run(&self, id: &str) -> Result<Option<RunState>> {
        lock(&self.conn)
            .query_row("SELECT id, status, created_at, updated_at, active_ms FROM transfer_runs WHERE id = ?1", [id], |r| {
                Ok(RunState {
                    id: r.get(0)?,
                    status: RunStatus::parse(&r.get::<_, String>(1)?),
                    created_at: r.get(2)?,
                    updated_at: r.get(3)?,
                    active_ms: r.get::<_, i64>(4)? as u64,
                })
            })
            .optional()
            .map_err(err)
    }

    pub fn run_spec(&self, id: &str) -> Result<Option<RunSpec>> {
        let json: Option<String> = lock(&self.conn)
            .query_row("SELECT spec_json FROM transfer_runs WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .map_err(err)?;
        json.map(|j| serde_json::from_str(&j).map_err(err)).transpose()
    }

    pub fn tables(&self, run_id: &str) -> Result<Vec<TableState>> {
        let c = lock(&self.conn);
        let mut st = c
            .prepare(&format!("SELECT {TABLE_COLUMNS} FROM transfer_tables WHERE run_id = ?1 ORDER BY rowid"))
            .map_err(err)?;
        let rows = st.query_map([run_id], table_row).map_err(err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(err)
    }

    pub fn table(&self, run_id: &str, name: &str) -> Result<Option<TableState>> {
        lock(&self.conn)
            .query_row(
                &format!("SELECT {TABLE_COLUMNS} FROM transfer_tables WHERE run_id = ?1 AND name = ?2"),
                params![run_id, name],
                table_row,
            )
            .optional()
            .map_err(err)
    }

    /// "Reintentar las que fallaron": failed and cancelled tables go back to
    /// pending (keeping `copied`, so a copied one only finishes its `post`).
    pub fn requeue_failed(&self, run_id: &str) -> Result<usize> {
        lock(&self.conn)
            .execute(
                "UPDATE transfer_tables SET status = CASE copied WHEN 1 THEN 'copied' ELSE 'pending' END, error = NULL
                 WHERE run_id = ?1 AND status IN ('failed', 'cancelled')",
                [run_id],
            )
            .map_err(err)
    }

    // -- written by the engine -----------------------------------------------------------------

    /// A new run (or the same id again: its spec is replaced).
    pub(crate) fn begin_run(&self, id: &str, spec: &RunSpec) -> Result<()> {
        let json = serde_json::to_string(spec).map_err(err)?;
        let (t, ms) = (now(), now_ms());
        lock(&self.conn)
            .execute(
                "INSERT INTO transfer_runs (id, spec_json, status, created_at, updated_at, segment_start_ms, updated_ms)
                 VALUES (?1, ?2, 'running', ?3, ?3, ?4, ?4)
                 ON CONFLICT(id) DO UPDATE SET spec_json = excluded.spec_json, status = 'running',
                    updated_at = excluded.updated_at, segment_start_ms = excluded.segment_start_ms, updated_ms = excluded.updated_ms",
                params![id, json, t, ms],
            )
            .map_err(err)?;
        Ok(())
    }

    /// A run goes on (resume, retry).
    pub(crate) fn resume_run(&self, id: &str) -> Result<()> {
        let n = lock(&self.conn)
            .execute(
                "UPDATE transfer_runs SET status = 'running', updated_at = ?2, segment_start_ms = ?3, updated_ms = ?3 WHERE id = ?1",
                params![id, now(), now_ms()],
            )
            .map_err(err)?;
        if n == 0 {
            return Err(Error::State("la transferencia no existe".into()));
        }
        Ok(())
    }

    /// Add a job to the run's spec (the live queue), so a resume knows it.
    pub(crate) fn add_job(&self, id: &str, job: &TransferJob) -> Result<()> {
        let mut spec = self.run_spec(id)?.ok_or_else(|| Error::State("la transferencia no existe".into()))?;
        if spec.jobs.iter().any(|j| j.name == job.name) {
            return Ok(());
        }
        spec.jobs.push(job.clone());
        let json = serde_json::to_string(&spec).map_err(err)?;
        lock(&self.conn).execute("UPDATE transfer_runs SET spec_json = ?2 WHERE id = ?1", params![id, json]).map_err(err)?;
        Ok(())
    }

    /// Register a table as pending. One already there keeps its flags; only
    /// a finished (failed, cancelled) one goes back to pending.
    pub(crate) fn add_table(&self, run_id: &str, name: &str, rows_total: Option<u64>) -> Result<()> {
        lock(&self.conn)
            .execute(
                "INSERT INTO transfer_tables (run_id, name, status, rows_total) VALUES (?1, ?2, 'pending', ?3)
                 ON CONFLICT(run_id, name) DO UPDATE SET
                    rows_total = COALESCE(excluded.rows_total, rows_total),
                    status = CASE WHEN status IN ('failed', 'cancelled') THEN 'pending' ELSE status END",
                params![run_id, name, rows_total.map(|n| n as i64)],
            )
            .map_err(err)?;
        Ok(())
    }

    pub(crate) fn set_status(&self, run_id: &str, name: &str, status: TableStatus) -> Result<()> {
        lock(&self.conn)
            .execute(
                "UPDATE transfer_tables SET status = ?3, error = NULL, started_at = COALESCE(started_at, ?4) WHERE run_id = ?1 AND name = ?2",
                params![run_id, name, status.as_str(), now()],
            )
            .map_err(err)?;
        Ok(())
    }

    /// A copy starts: from here on the table may hold rows of this run.
    pub(crate) fn begin_copy(&self, run_id: &str, name: &str) -> Result<()> {
        lock(&self.conn)
            .execute("UPDATE transfer_tables SET attempts = attempts + 1 WHERE run_id = ?1 AND name = ?2", params![run_id, name])
            .map_err(err)?;
        Ok(())
    }

    /// A sync by rows starts: the table is flagged `delta` (never emptied).
    pub(crate) fn begin_delta(&self, run_id: &str, name: &str) -> Result<()> {
        lock(&self.conn)
            .execute("UPDATE transfer_tables SET attempts = attempts + 1, delta = 1 WHERE run_id = ?1 AND name = ?2", params![run_id, name])
            .map_err(err)?;
        Ok(())
    }

    pub(crate) fn set_rows_done(&self, run_id: &str, name: &str, rows: u64) -> Result<()> {
        let c = lock(&self.conn);
        c.execute("UPDATE transfer_tables SET rows_done = ?3 WHERE run_id = ?1 AND name = ?2", params![run_id, name, rows as i64])
            .map_err(err)?;
        c.execute("UPDATE transfer_runs SET updated_at = ?2, updated_ms = ?3 WHERE id = ?1", params![run_id, now(), now_ms()])
            .map_err(err)?;
        Ok(())
    }

    /// Every row is in: from now on the table is never emptied.
    pub(crate) fn set_copied(&self, run_id: &str, name: &str, rows: u64) -> Result<()> {
        lock(&self.conn)
            .execute(
                "UPDATE transfer_tables SET copied = 1, rows_done = ?3, status = 'copied' WHERE run_id = ?1 AND name = ?2",
                params![run_id, name, rows as i64],
            )
            .map_err(err)?;
        Ok(())
    }

    pub(crate) fn finish_table(&self, run_id: &str, name: &str, status: TableStatus, error: Option<&str>, stats: Option<&CopyStats>) -> Result<()> {
        let stats = stats.map(serde_json::to_string).transpose().map_err(err)?;
        lock(&self.conn)
            .execute(
                "UPDATE transfer_tables SET status = ?3, error = ?4, stats_json = COALESCE(?5, stats_json), finished_at = ?6
                 WHERE run_id = ?1 AND name = ?2",
                params![run_id, name, status.as_str(), error, stats, now()],
            )
            .map_err(err)?;
        Ok(())
    }

    pub(crate) fn finish_run(&self, id: &str, status: RunStatus) -> Result<()> {
        let ms = now_ms();
        lock(&self.conn)
            .execute(
                "UPDATE transfer_runs SET status = ?2, updated_at = ?3, updated_ms = ?4,
                    active_ms = active_ms + MAX(0, ?4 - COALESCE(segment_start_ms, ?4)), segment_start_ms = NULL
                 WHERE id = ?1",
                params![id, status.as_str(), now(), ms],
            )
            .map_err(err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_are_durable() {
        let dir = std::env::temp_dir().join(format!("dbine-transfer-sync-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let store = Store::open(&dir).unwrap();
        let sync: i64 = lock(&store.conn).query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
        assert_eq!(sync, 2, "synchronous=FULL");
        drop(store);
        let _ = std::fs::remove_file(&dir);
    }
}

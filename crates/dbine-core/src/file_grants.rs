//! Local files a notebook may read (docs/notebooks.md › Reading local files ›
//! Approval), and what each file was like the last time a notebook read it
//! (for the "changed since the last run" notice).
//!
//! - A grant is the pair (notebook file, file) or (notebook file, pattern),
//!   both canonical paths. A pattern is a glob resolved to its folder
//!   (`/Users/ana/exports/ventas_*.csv`).
//! - Both tables are this machine's only, like `secret_accounts`: they
//!   aren't in the snapshot, so no cloud backup, export or restore carries
//!   them, and writing them doesn't bump the revision. Nothing goes into
//!   the `.ipynb`.
//! - Grants are keyed by the notebook's path: a notebook moved or renamed
//!   asks again.

use crate::state::StateStore;
use dbine_driver::{Error, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

/// What a notebook may read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileGrant {
    /// The notebook file (canonical path).
    pub notebook: String,
    /// The file, or the pattern (canonical folder + glob).
    pub target: String,
    pub pattern: bool,
    /// RFC 3339.
    pub granted_at: String,
}

/// A file as a notebook last read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub size: u64,
    pub mtime_ms: i64,
    pub sha256: String,
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::State(e.to_string())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl StateStore {
    /// The grants of `notebook`, or every one (`None`), by notebook then target.
    pub fn list_file_grants(&self, notebook: Option<&str>) -> Result<Vec<FileGrant>> {
        let c = self.lock()?;
        let mut st = c
            .prepare(
                "SELECT notebook, target, pattern, granted_at FROM notebook_file_grants
                 WHERE ?1 IS NULL OR notebook = ?1 ORDER BY notebook, target",
            )
            .map_err(db_err)?;
        let rows = st
            .query_map([notebook], |r| Ok(FileGrant { notebook: r.get(0)?, target: r.get(1)?, pattern: r.get::<_, i64>(2)? != 0, granted_at: r.get(3)? }))
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db_err)
    }

    /// Grant `notebook` each `(target, is_pattern)`; one already there keeps
    /// its date.
    pub fn add_file_grants(&self, notebook: &str, targets: &[(String, bool)]) -> Result<()> {
        if notebook.trim().is_empty() {
            return Err(Error::State("falta la ruta del notebook".into()));
        }
        let ts = now();
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(db_err)?;
        for (target, pattern) in targets {
            tx.execute(
                "INSERT OR IGNORE INTO notebook_file_grants (notebook, target, pattern, granted_at) VALUES (?1, ?2, ?3, ?4)",
                params![notebook, target, *pattern as i64, ts],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    /// Whether it was there.
    pub fn remove_file_grant(&self, notebook: &str, target: &str) -> Result<bool> {
        Ok(self.lock()?.execute("DELETE FROM notebook_file_grants WHERE notebook = ?1 AND target = ?2", [notebook, target]).map_err(db_err)? > 0)
    }

    /// Every grant of `notebook`, or every one (`None`). How many went.
    pub fn clear_file_grants(&self, notebook: Option<&str>) -> Result<usize> {
        self.lock()?.execute("DELETE FROM notebook_file_grants WHERE ?1 IS NULL OR notebook = ?1", [notebook]).map_err(db_err)
    }

    pub fn file_record(&self, notebook: &str, path: &str) -> Result<Option<FileRecord>> {
        self.lock()?
            .query_row("SELECT size, mtime_ms, sha256 FROM notebook_file_reads WHERE notebook = ?1 AND path = ?2", [notebook, path], |r| {
                Ok(FileRecord { size: r.get::<_, i64>(0)? as u64, mtime_ms: r.get(1)?, sha256: r.get(2)? })
            })
            .optional()
            .map_err(db_err)
    }

    pub fn put_file_record(&self, notebook: &str, path: &str, rec: &FileRecord) -> Result<()> {
        self.lock()?
            .execute(
                "INSERT INTO notebook_file_reads (notebook, path, size, mtime_ms, sha256, read_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(notebook, path) DO UPDATE SET size = ?3, mtime_ms = ?4, sha256 = ?5, read_at = ?6",
                params![notebook, path, rec.size as i64, rec.mtime_ms, rec.sha256, now()],
            )
            .map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_and_records_are_local_and_per_notebook() {
        let s = StateStore::open_in_memory().unwrap();
        let rev = s.get_setting("local.revision").unwrap();
        s.add_file_grants("/nb/a.ipynb", &[("/d/x.csv".into(), false), ("/d/v_*.csv".into(), true)]).unwrap();
        s.add_file_grants("/nb/b.ipynb", &[("/d/x.csv".into(), false)]).unwrap();
        let first = s.list_file_grants(Some("/nb/a.ipynb")).unwrap();
        assert_eq!(first.iter().map(|g| (g.target.as_str(), g.pattern)).collect::<Vec<_>>(), vec![("/d/v_*.csv", true), ("/d/x.csv", false)]);
        // Granting again keeps the date.
        s.add_file_grants("/nb/a.ipynb", &[("/d/x.csv".into(), false)]).unwrap();
        assert_eq!(s.list_file_grants(Some("/nb/a.ipynb")).unwrap(), first);
        assert_eq!(s.list_file_grants(None).unwrap().len(), 3);
        assert!(s.remove_file_grant("/nb/a.ipynb", "/d/x.csv").unwrap());
        assert!(!s.remove_file_grant("/nb/a.ipynb", "/d/x.csv").unwrap());
        assert_eq!(s.clear_file_grants(Some("/nb/b.ipynb")).unwrap(), 1);
        assert_eq!(s.clear_file_grants(None).unwrap(), 1);
        assert!(s.list_file_grants(None).unwrap().is_empty());

        assert!(s.file_record("/nb/a.ipynb", "/d/x.csv").unwrap().is_none());
        let r = FileRecord { size: 3, mtime_ms: 10, sha256: "ab".into() };
        s.put_file_record("/nb/a.ipynb", "/d/x.csv", &r).unwrap();
        assert_eq!(s.file_record("/nb/a.ipynb", "/d/x.csv").unwrap(), Some(r));
        assert!(s.file_record("/nb/b.ipynb", "/d/x.csv").unwrap().is_none());
        let r2 = FileRecord { size: 4, mtime_ms: 11, sha256: "cd".into() };
        s.put_file_record("/nb/a.ipynb", "/d/x.csv", &r2).unwrap();
        assert_eq!(s.file_record("/nb/a.ipynb", "/d/x.csv").unwrap(), Some(r2));

        // Local only: no revision bump, nothing in the snapshot.
        assert_eq!(s.get_setting("local.revision").unwrap(), rev);
        let snap = serde_json::to_string(&s.snapshot().unwrap()).unwrap();
        assert!(!snap.contains("/d/x.csv") && !snap.contains("/nb/a.ipynb"));
    }
}

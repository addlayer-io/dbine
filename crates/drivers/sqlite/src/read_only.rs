//! `Session::run_read_only`: one statement as a read SQLite itself
//! enforces, not the text.
//!
//! - **One statement**: prepared with sqlite3's own tail semantics
//!   (`Batch`); anything after it other than whitespace and comments is
//!   refused before the first one runs.
//! - **Nothing that writes or changes the connection**: an authorizer
//!   (`sqlite3_set_authorizer`) installed while the statement is prepared
//!   and stepped allows only reading (SELECT, reading columns, functions,
//!   recursive CTEs and the PRAGMAs that only report). ATTACH, DETACH,
//!   transaction control, `load_extension`, PRAGMAs that set something
//!   (`writable_schema`, `query_only = OFF`…) are denied there. Then
//!   `sqlite3_stmt_readonly` has to say the statement doesn't write (it
//!   catches VACUUM, which the authorizer doesn't see).
//! - **A connection that can't write**: for a database file, a second
//!   connection opened with `SQLITE_OPEN_READ_ONLY`, `query_only` on, the
//!   defensive flag on and extension loading off. An in-memory database
//!   can't be reopened, so the session's connection is used with
//!   `query_only` on for the statement (and restored afterwards); the
//!   authorizer keeps the statement from turning it off.

use crate::{err, run_stmt, stmt_err};
use dbine_driver::{Error, QueryOutcome, Result};
use rusqlite::config::DbConfig;
use rusqlite::{ffi, Batch, Connection, InterruptHandle, OpenFlags};
use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Mutex;
use std::time::Duration;

/// PRAGMAs that only report, with or without an argument (a table or index
/// name, a row limit for the checks).
const REPORTING: &[&str] = &[
    "table_info", "table_xinfo", "table_list", "index_info", "index_xinfo", "index_list", "foreign_key_list", "foreign_key_check",
    "integrity_check", "quick_check", "database_list", "collation_list", "function_list", "module_list", "pragma_list", "compile_options",
];

/// PRAGMAs that report a setting when they have no argument (with one,
/// they change it).
const SETTINGS: &[&str] = &[
    "application_id", "auto_vacuum", "cache_size", "data_version", "encoding", "foreign_keys", "freelist_count", "journal_mode",
    "page_count", "page_size", "query_only", "schema_version", "synchronous", "temp_store", "user_version",
];

fn lock<T>(m: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    m.lock().map_err(|_| Error::State("conexión SQLite envenenada".into()))
}

/// Run `sql` as a read: on a read-only connection to the session's file
/// (opened once, kept in `ro`, its interrupt handle in `ro_interrupt`), or
/// on the session's connection with `query_only` when the database is in
/// memory or the file can't be opened read-only.
pub(crate) fn run(
    main: &Mutex<Connection>,
    ro: &Mutex<Option<Connection>>,
    ro_interrupt: &Mutex<Option<InterruptHandle>>,
    sql: &str,
    max_rows: usize,
    out: &mut QueryOutcome,
) -> Result<()> {
    let file = lock(main)?.path().filter(|p| !p.is_empty()).map(str::to_string);
    if let Some(file) = file {
        let mut slot = lock(ro)?;
        if slot.is_none() {
            if let Ok(c) = open_read_only(&file) {
                *lock(ro_interrupt)? = Some(c.get_interrupt_handle());
                *slot = Some(c);
            }
        }
        if let Some(c) = slot.as_ref() {
            return guarded(c, sql, max_rows, out);
        }
    }
    let c = lock(main)?;
    let was_on: bool = c.query_row("PRAGMA query_only", [], |r| r.get(0)).map_err(err)?;
    if !was_on {
        c.execute_batch("PRAGMA query_only = ON").map_err(err)?;
    }
    let r = guarded(&c, sql, max_rows, out);
    let restored = if was_on { Ok(()) } else { c.execute_batch("PRAGMA query_only = OFF").map_err(err) };
    r.and(restored)
}

/// A connection to `file` that can't write it.
fn open_read_only(file: &str) -> rusqlite::Result<Connection> {
    let c = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    c.busy_timeout(Duration::from_secs(5))?;
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    // SAFETY: a live handle; ENABLE_LOAD_EXTENSION takes (int, int*).
    unsafe {
        ffi::sqlite3_db_config(c.handle(), ffi::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION, 0 as c_int, std::ptr::null_mut::<c_int>());
    }
    c.execute_batch("PRAGMA query_only = ON")?;
    Ok(c)
}

/// What the authorizer denied first, for the error.
#[derive(Default)]
struct Denied(Cell<Option<String>>);

/// The statement, prepared and run with the authorizer installed (it is
/// called again if SQLite re-prepares the statement while stepping it).
fn guarded(c: &Connection, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    let denied = Denied::default();
    // SAFETY: `denied` outlives the authorizer, which is removed below
    // before it goes out of scope.
    unsafe {
        ffi::sqlite3_set_authorizer(c.handle(), Some(authorize), &denied as *const Denied as *mut c_void);
    }
    let r = prepare_and_run(c, sql, max_rows, out, &denied);
    unsafe {
        ffi::sqlite3_set_authorizer(c.handle(), None, std::ptr::null_mut());
    }
    r
}

fn prepare_and_run(c: &Connection, sql: &str, max_rows: usize, out: &mut QueryOutcome, denied: &Denied) -> Result<()> {
    let refused = |e: rusqlite::Error| match denied.0.take() {
        Some(what) => Error::Query(format!("Una lectura no admite {what}.")),
        None => stmt_err(e, sql),
    };
    let mut batch = Batch::new(c, sql);
    let Some(mut stmt) = batch.next().map_err(refused)? else {
        return Err(Error::Query("No hay ninguna sentencia que ejecutar.".into()));
    };
    // Whatever follows, valid or not, is a second statement.
    if !matches!(batch.next(), Ok(None)) {
        return Err(Error::Query("Una lectura admite una sola sentencia.".into()));
    }
    if !stmt.readonly() {
        return Err(Error::Query("La sentencia escribe en la base: una lectura no la admite.".into()));
    }
    let r = run_stmt(c, &mut stmt, sql, max_rows, out);
    match (r, denied.0.take()) {
        // Denied while stepping (the statement was re-prepared).
        (Err(_), Some(what)) => Err(Error::Query(format!("Una lectura no admite {what}."))),
        (r, _) => r,
    }
}

/// `sqlite3_set_authorizer`'s callback: only reading is allowed.
unsafe extern "C" fn authorize(
    data: *mut c_void,
    action: c_int,
    arg1: *const c_char,
    arg2: *const c_char,
    _db: *const c_char,
    _trigger: *const c_char,
) -> c_int {
    let text = |p: *const c_char| if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().to_ascii_lowercase() };
    let verdict = match action {
        ffi::SQLITE_SELECT | ffi::SQLITE_READ | ffi::SQLITE_RECURSIVE => None,
        // arg2: the function's name.
        ffi::SQLITE_FUNCTION => {
            let f = text(arg2);
            (f == "load_extension").then(|| format!("{f}()"))
        }
        // arg1: the PRAGMA, arg2: its argument (NULL when it has none).
        ffi::SQLITE_PRAGMA => {
            let name = text(arg1);
            let reports = REPORTING.contains(&name.as_str()) || (arg2.is_null() && SETTINGS.contains(&name.as_str()));
            (!reports).then(|| format!("PRAGMA {name}"))
        }
        other => Some(action_name(other).to_string()),
    };
    match verdict {
        None => ffi::SQLITE_OK,
        Some(what) => {
            if let Some(d) = (data as *const Denied).as_ref() {
                let first = d.0.take();
                d.0.set(first.or(Some(what)));
            }
            ffi::SQLITE_DENY
        }
    }
}

fn action_name(action: c_int) -> &'static str {
    match action {
        ffi::SQLITE_INSERT => "INSERT",
        ffi::SQLITE_UPDATE => "UPDATE",
        ffi::SQLITE_DELETE => "DELETE",
        ffi::SQLITE_ATTACH => "ATTACH",
        ffi::SQLITE_DETACH => "DETACH",
        ffi::SQLITE_TRANSACTION | ffi::SQLITE_SAVEPOINT => "control de transacciones",
        ffi::SQLITE_ALTER_TABLE => "ALTER TABLE",
        ffi::SQLITE_REINDEX => "REINDEX",
        ffi::SQLITE_ANALYZE => "ANALYZE",
        ffi::SQLITE_CREATE_INDEX
        | ffi::SQLITE_CREATE_TABLE
        | ffi::SQLITE_CREATE_TEMP_INDEX
        | ffi::SQLITE_CREATE_TEMP_TABLE
        | ffi::SQLITE_CREATE_TEMP_TRIGGER
        | ffi::SQLITE_CREATE_TEMP_VIEW
        | ffi::SQLITE_CREATE_TRIGGER
        | ffi::SQLITE_CREATE_VIEW
        | ffi::SQLITE_CREATE_VTABLE => "CREATE",
        ffi::SQLITE_DROP_INDEX
        | ffi::SQLITE_DROP_TABLE
        | ffi::SQLITE_DROP_TEMP_INDEX
        | ffi::SQLITE_DROP_TEMP_TABLE
        | ffi::SQLITE_DROP_TEMP_TRIGGER
        | ffi::SQLITE_DROP_TEMP_VIEW
        | ffi::SQLITE_DROP_TRIGGER
        | ffi::SQLITE_DROP_VIEW
        | ffi::SQLITE_DROP_VTABLE => "DROP",
        _ => "esa operación",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(c: &Connection, sql: &str) -> Result<QueryOutcome> {
        let mut out = QueryOutcome::default();
        guarded(c, sql, 100, &mut out).map(|_| out)
    }

    #[test]
    fn reads_pass_and_everything_else_is_denied() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (a); CREATE INDEX i ON t (a); INSERT INTO t VALUES (1), (2); CREATE VIEW v AS SELECT a FROM t").unwrap();
        for ok in [
            "SELECT count(*) FROM t",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3) SELECT * FROM n, v",
            "SELECT * FROM pragma_table_info('t')",
            "SELECT value FROM json_each('[1,2]')",
            "PRAGMA table_info(t)",
            "PRAGMA user_version",
            "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 1",
            "VALUES (1, 2)",
        ] {
            read(&c, ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "INSERT INTO t VALUES (3)",
            "UPDATE t SET a = 0",
            "DELETE FROM t",
            "CREATE TABLE u (a)",
            "CREATE TEMP TABLE u (a)",
            "DROP TABLE t",
            "ATTACH DATABASE ':memory:' AS x",
            "DETACH DATABASE main",
            "BEGIN",
            "SAVEPOINT s",
            "PRAGMA writable_schema = 1",
            "PRAGMA query_only = OFF",
            "PRAGMA user_version = 5",
            "SELECT load_extension('x')",
            "VACUUM",
            "REINDEX",
            "ANALYZE",
            "WITH x AS (SELECT 1) DELETE FROM t",
            "SELECT 1; DELETE FROM t",
            "SELECT 1; SELECT 2",
            "-- nothing",
        ] {
            assert!(read(&c, bad).is_err(), "{bad} should be refused");
        }
        let n: i64 = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        assert!(read(&c, "SELECT 1; -- trailing comment\n").is_ok());
        // The authorizer is gone afterwards.
        c.execute_batch("INSERT INTO t VALUES (3)").unwrap();
    }

    #[test]
    fn errors_say_why() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (a)").unwrap();
        let msg = |sql| read(&c, sql).unwrap_err().to_string();
        assert!(msg("ATTACH DATABASE ':memory:' AS x").contains("ATTACH"), "{}", msg("ATTACH DATABASE ':memory:' AS x"));
        assert!(msg("PRAGMA writable_schema = 1").contains("writable_schema"));
        assert!(msg("SELECT 1; DELETE FROM t").contains("una sola sentencia"));
    }
}

//! Read-only connections: a decorator that refuses any statement that
//! isn't a read before it reaches the server. A statement passes only when
//! its first keyword is a read and no word inside it writes or changes the
//! session: a T-SQL batch needs no `;` between statements (`SELECT 1 DELETE
//! FROM t`), a CTE can modify data (`WITH … DELETE`), `SELECT … INTO`
//! creates a table, and functions like `set_config` or `xp_cmdshell` act
//! from inside a SELECT. Drivers also set a server-side read-only mode where
//! the engine has one; the statements that would switch it off (`SET`,
//! `set_config`) are refused here. Only SQL drivers are wrapped; the others
//! enforce read-only themselves (see `ConnectionConfig::read_only`).

use crate::error::{Error, Result};
use crate::model::{ColumnInfo, DbObject, ObjectRef, QueryOutcome, TxState};
use crate::sql::{expose_versioned, name_tokens, split_script, strip_comments, BatchLine, ScriptDialect, StatementKind, TokenKind};
use crate::Session;
use async_trait::async_trait;
use std::sync::Arc;

const READ_KEYWORDS: &[&str] = &["select", "with", "show", "explain", "describe", "desc", "values", "table", "pragma", "use", "list", "count", "print", "match", "traverse"];

/// Leads whose second word is part of the read, not a write: `SHOW CREATE
/// TABLE t` (MySQL). Every other word is still checked: a T-SQL batch needs
/// no `;`, so whatever follows the lead runs too.
const SHOW_CREATE: &[&str] = &["show"];

/// T-SQL's reads. Any other word that starts a batch runs as a procedure
/// call (`SHOW`, `DESC`… aren't statements there).
const TSQL_READS: &[&str] = &["select", "with", "use", "print"];

/// Words that write, change structure or run other code wherever they
/// appear in a statement.
const WRITE_WORDS: &[&str] = &[
    "insert", "update", "delete", "merge", "upsert", "replace", "into", "drop", "create", "alter", "truncate", "rename", "grant", "revoke", "deny",
    "exec", "execute", "call", "dbcc", "reconfigure", "shutdown", "kill", "bulk", "openrowset", "opendatasource", "openquery", "setuser", "revert",
    // T-SQL statements that change state after a read in the same batch.
    "backup", "restore", "dump", "writetext", "updatetext", "receive", "commit", "rollback", "checkpoint",
];

/// Write words only when the next word says so (`ENABLE TRIGGER`, `SEND ON
/// CONVERSATION`, `SAVE TRANSACTION`), so columns named `enable` or `save`
/// still read.
const WRITE_PAIRS: &[(&str, &[&str])] =
    &[("enable", &["trigger"]), ("disable", &["trigger"]), ("send", &["on"]), ("save", &["tran", "transaction"]), ("end", &["conversation"])];

/// Write words that are also read-only functions (`REPLACE(s, a, b)`,
/// MySQL's `INSERT(s, pos, len, new)`).
const READ_FUNCTIONS: &[&str] = &["replace", "insert"];

/// Functions with side effects, called from a read: session settings,
/// other sessions, files, sequences, dynamic SQL and other servers.
const WRITE_FUNCTIONS: &[&str] = &[
    "set_config", "pg_terminate_backend", "pg_cancel_backend", "pg_reload_conf", "pg_rotate_logfile", "pg_promote", "pg_switch_wal",
    "pg_create_restore_point", "pg_file_write", "pg_file_rename", "pg_file_unlink", "pg_file_sync", "pg_logical_emit_message",
    "pg_create_logical_replication_slot", "pg_create_physical_replication_slot", "pg_drop_replication_slot", "pg_stat_reset",
    "pg_stat_reset_shared", "pg_stat_reset_single_table_counters", "pg_stat_reset_single_function_counters", "pg_stat_statements_reset",
    "lo_import", "lo_export", "lo_unlink", "lo_create", "lo_creat", "lo_from_bytea", "lo_put", "lo_truncate", "setval", "nextval", "dblink",
    "dblink_exec", "dblink_open", "dblink_send_query", "dblink_connect", "query_to_xml", "query_to_xmlschema", "query_to_xml_and_xmlschema",
    "cursor_to_xml", "load_file", "sys_exec", "sys_eval",
];

/// Prefixes of procedures and packages that act outside the query: SQL
/// Server's `sp_` / `xp_`, Oracle's `UTL_` (files, network) and `DBMS_`
/// (except the read-only packages in [`READ_PACKAGES`]).
const WRITE_PREFIXES: &[&str] = &["sp_", "xp_", "utl_", "dbms_"];
const READ_PACKAGES: &[&str] = &["dbms_metadata", "dbms_xplan", "dbms_lob", "dbms_random", "dbms_utility", "dbms_assert"];

/// SQLite pragmas that read and take an argument (`PRAGMA table_info(t)`).
const READ_PRAGMAS: &[&str] = &[
    "table_info", "table_xinfo", "table_list", "index_list", "index_info", "index_xinfo", "foreign_key_list", "foreign_key_check", "integrity_check",
    "quick_check", "database_list", "collation_list", "function_list", "module_list", "pragma_list", "compile_options",
];
const WRITE_PRAGMAS: &[&str] = &["optimize", "wal_checkpoint", "incremental_vacuum", "shrink_memory"];

/// Words that switch an estimated plan into running the script (SQL
/// Server's `SET SHOWPLAN_XML OFF`, Sybase's `SET NOEXEC OFF`).
const PLAN_MODE_WORDS: &[&str] = &["showplan_xml", "showplan_all", "showplan_text", "noexec", "fmtonly", "parseonly"];

pub struct ReadOnlySession {
    inner: Box<dyn Session>,
    dialect: ScriptDialect,
    /// T-SQL: the engine has no read-only mode the session can't undo, so
    /// each run also goes in a transaction that is always rolled back.
    roll_back: bool,
}

impl ReadOnlySession {
    /// Checks statements split on `;` and `GO` lines (any dialect's quotes
    /// and comments). Prefer [`Self::with_dialect`].
    pub fn new(inner: Box<dyn Session>) -> Self {
        Self::with_dialect(inner, ScriptDialect { batch: BatchLine::Go, ..ScriptDialect::generic() })
    }

    /// Checks statements split as the driver's dialect says
    /// ([`crate::Driver::script_dialect`]), statement by statement.
    pub fn with_dialect(inner: Box<dyn Session>, dialect: ScriptDialect) -> Self {
        let roll_back = dialect.tsql_blocks;
        let mut dialect = dialect.statements();
        if dialect.batch == BatchLine::None {
            dialect.batch = BatchLine::Go;
        }
        Self { inner, dialect, roll_back }
    }

    fn first_write(&self, sql: &str) -> Option<String> {
        first_write_in(sql, &self.dialect)
    }
}

/// The first statement that isn't a read, if any (`;` and `GO` lines).
pub fn first_write(sql: &str) -> Option<String> {
    first_write_in(sql, &ScriptDialect { batch: BatchLine::Go, ..ScriptDialect::generic() })
}

/// The first statement of `sql` (split as `dialect` says) that isn't a read.
/// MySQL's versioned comments (`/*!50000 delete … */`) are read as the code
/// they are. MySQL's `--` rule (a comment only with a space after it, so
/// `select 1--1; delete …` is two statements) comes with the MySQL dialect
/// ([`ScriptDialect::dash_comment_space`]); elsewhere `--` is a comment, as
/// the server reads it.
pub fn first_write_in(sql: &str, dialect: &ScriptDialect) -> Option<String> {
    writes_in(&expose_versioned(sql, dialect), dialect)
}

fn writes_in(sql: &str, dialect: &ScriptDialect) -> Option<String> {
    statements(sql, dialect).into_iter().find_map(|stmt| statement_write(&stmt, dialect))
}

fn statements(sql: &str, dialect: &ScriptDialect) -> Vec<String> {
    split_script(sql, &dialect.statements()).into_iter().filter(|s| s.kind != StatementKind::ClientCommand).map(|s| s.text).collect()
}

/// What makes `stmt` a write: its first keyword, or a word inside it.
fn statement_write(stmt: &str, dialect: &ScriptDialect) -> Option<String> {
    let kw = match leading(stmt, dialect) {
        Leading::Nothing => return None,
        // `[dbo].[proc]` or `"proc"` alone runs the procedure (T-SQL), and
        // anything else that isn't a word can't be told apart: refused.
        Leading::Other(c) => return Some(c.to_string()),
        Leading::Word(kw) => kw,
    };
    if dialect.tsql_blocks && !TSQL_READS.contains(&kw.as_str()) {
        return Some(kw.to_uppercase());
    }
    if !READ_KEYWORDS.contains(&kw.as_str()) {
        return Some(kw.to_uppercase());
    }
    hidden_write(stmt, &kw, dialect)
}

/// A word of `stmt` (not its first) that writes, changes the session or
/// runs code: see [`WRITE_WORDS`], [`WRITE_FUNCTIONS`], [`WRITE_PREFIXES`].
fn hidden_write(stmt: &str, first: &str, dialect: &ScriptDialect) -> Option<String> {
    let toks = name_tokens(stmt, dialect);
    let bytes = stmt.as_bytes();
    let bare = |i: usize| toks[i].kind == TokenKind::Name && !matches!(bytes[toks[i].start], b'"' | b'`' | b'[');
    let is_call = |i: usize| toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Punct && t.text == "(");
    let lower = |i: usize| toks[i].text.to_ascii_lowercase();
    if first == "pragma" {
        // `PRAGMA [schema.]x = y` and `PRAGMA x(y)` set x, except the reads.
        let Some(mut i) = toks.iter().position(|t| t.kind == TokenKind::Name && !t.text.eq_ignore_ascii_case("pragma")) else { return None };
        if toks.get(i + 1).is_some_and(|t| t.text == ".") && toks.get(i + 2).is_some_and(|t| t.kind == TokenKind::Name) {
            i += 2;
        }
        let name = lower(i);
        let assigns = toks.iter().any(|t| t.kind == TokenKind::Punct && t.text == "=");
        let with_arg = is_call(i) && !READ_PRAGMAS.contains(&name.as_str());
        if assigns || with_arg || WRITE_PRAGMAS.contains(&name.as_str()) {
            return Some("PRAGMA".into());
        }
    }
    for i in 0..toks.len() {
        if toks[i].kind != TokenKind::Name {
            continue;
        }
        let w = lower(i);
        if i == 1 && w == "create" && SHOW_CREATE.contains(&first) {
            continue;
        }
        // PostgreSQL's `U&"…"` names spell a function with escapes
        // (`U&"\0073et_config"`): what it names can't be checked here.
        if bare(i) && w == "u" && toks.get(i + 1).is_some_and(|t| t.text == "&" && t.start == toks[i].end) {
            if toks.get(i + 2).is_some_and(|t| t.start == toks[i + 1].end && matches!(bytes[t.start], b'"' | b'\'')) {
                return Some("U&".into());
            }
        }
        if bare(i) {
            if let Some((_, next)) = WRITE_PAIRS.iter().find(|(word, _)| *word == w) {
                if toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Name && next.contains(&t.text.to_ascii_lowercase().as_str())) {
                    return Some(w.to_uppercase());
                }
            }
        }
        // MySQL's `SELECT … INTO @var` only sets a session variable.
        let into_variable = w == "into" && toks.get(i + 1).is_some_and(|t| t.text == "@");
        if bare(i) && WRITE_WORDS.contains(&w.as_str()) && !(READ_FUNCTIONS.contains(&w.as_str()) && is_call(i)) && !into_variable {
            return Some(w.to_uppercase());
        }
        // `SET` outside `CHARACTER SET` changes the session (T-SQL batches).
        if bare(i) && w == "set" && !(i > 0 && matches!(lower(i - 1).as_str(), "character" | "char")) {
            return Some("SET".into());
        }
        if is_call(i) && WRITE_FUNCTIONS.contains(&w.as_str()) {
            return Some(w.to_uppercase());
        }
        // A call or a package member (`xp_cmdshell(…)`, `utl_http.request`),
        // not a column that happens to start the same way.
        let called = is_call(i) || toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".");
        if called && WRITE_PREFIXES.iter().any(|p| w.starts_with(p)) && !READ_PACKAGES.contains(&w.as_str()) {
            return Some(w.to_uppercase());
        }
    }
    None
}

/// Why an estimated plan of `sql` could run something: a statement that
/// changes the plan mode or the session, or a write next to other
/// statements (a single write is only planned, not run).
fn unsafe_to_plan(sql: &str, dialect: &ScriptDialect) -> Option<String> {
    let sql = expose_versioned(sql, dialect);
    let stmts = statements(&sql, dialect);
    for stmt in &stmts {
        let kw = first_keyword(stmt, dialect);
        if matches!(kw.as_deref(), Some("set" | "reset" | "begin" | "commit" | "rollback" | "start")) {
            return kw.map(|k| k.to_uppercase());
        }
        let toks = name_tokens(stmt, dialect);
        if let Some(t) = toks.iter().find(|t| t.kind == TokenKind::Name && PLAN_MODE_WORDS.contains(&t.text.to_ascii_lowercase().as_str())) {
            return Some(t.text.to_uppercase());
        }
        if toks.iter().enumerate().any(|(i, t)| {
            t.kind == TokenKind::Name && WRITE_FUNCTIONS.contains(&t.text.to_ascii_lowercase().as_str()) && toks.get(i + 1).is_some_and(|n| n.text == "(")
        }) {
            return Some("FUNCTION".into());
        }
    }
    if stmts.len() > 1 {
        return stmts.iter().find_map(|s| statement_write(s, dialect));
    }
    None
}

fn first_keyword(stmt: &str, dialect: &ScriptDialect) -> Option<String> {
    match leading(stmt, dialect) {
        Leading::Word(w) => Some(w),
        _ => None,
    }
}

/// How a statement starts, comments and opening parentheses (`(SELECT …)
/// UNION …`) aside.
enum Leading {
    /// Only comments or spaces: no statement.
    Nothing,
    Word(String),
    /// A quote, a bracket or any other character.
    Other(char),
}

fn leading(stmt: &str, dialect: &ScriptDialect) -> Leading {
    let s = strip_comments(stmt, dialect, false);
    let s = s.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    let word: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    match s.chars().next() {
        None => Leading::Nothing,
        Some(_) if !word.is_empty() => Leading::Word(word.to_ascii_lowercase()),
        Some(c) => Leading::Other(c),
    }
}

#[async_trait]
impl Session for ReadOnlySession {
    async fn server_version(&mut self) -> Result<String> {
        self.inner.server_version().await
    }
    async fn list_databases(&mut self) -> Result<Vec<String>> {
        self.inner.list_databases().await
    }
    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        self.inner.list_objects().await
    }
    async fn list_schemas(&mut self) -> Result<Option<Vec<crate::SchemaInfo>>> {
        self.inner.list_schemas().await
    }
    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        self.inner.columns(obj).await
    }
    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        self.inner.definition(obj).await
    }
    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        self.inner.browse_query(obj, limit)
    }
    async fn execute(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if let Some(kw) = self.first_write(sql) {
            return Err(Error::Query(format!(
                "Conexión de solo lectura: se bloqueó una sentencia {kw}. Solo se permiten lecturas (SELECT, WITH, SHOW, EXPLAIN…)."
            )));
        }
        if !self.roll_back {
            return self.inner.execute(sql, max_rows, out).await;
        }
        // Implicit transactions: whatever the batch changes is undone.
        match self.inner.set_autocommit(false).await {
            Ok(()) => {}
            Err(Error::Unsupported(_)) => return self.inner.execute(sql, max_rows, out).await,
            Err(e) => return Err(e),
        }
        let ran = self.inner.execute(sql, max_rows, out).await;
        let undone = self.inner.rollback().await;
        let restored = self.inner.set_autocommit(true).await;
        ran?;
        undone?;
        restored
    }
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        // An actual plan runs the script. An estimated one runs nothing,
        // unless the script turns plan mode off or holds more statements.
        if analyze {
            if let Some(kw) = self.first_write(sql) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: el plan real ejecutaría una sentencia {kw}. Pedí el plan estimado."
                )));
            }
        } else if let Some(kw) = unsafe_to_plan(sql, &self.dialect) {
            return Err(Error::Query(format!(
                "Conexión de solo lectura: el plan estimado no admite {kw} ni varias sentencias que escriban. Pedí el plan de una sola consulta."
            )));
        }
        self.inner.explain(sql, analyze, max_rows, out).await
    }
    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.inner.interrupter()
    }
    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        self.inner.transaction_state().await
    }
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        self.inner.set_autocommit(on).await
    }
    async fn commit(&mut self) -> Result<()> {
        self.inner.commit().await
    }
    async fn rollback(&mut self) -> Result<()> {
        self.inner.rollback().await
    }
    async fn database_schema(&mut self) -> Result<Vec<crate::TableSchema>> {
        self.inner.database_schema().await
    }
    async fn create_database(&mut self, _name: &str) -> Result<()> {
        Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()))
    }
    async fn create_database_with(&mut self, _name: &str, _options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()))
    }
    async fn database_properties(&mut self, database: &str) -> Result<crate::DatabaseProperties> {
        self.inner.database_properties(database).await
    }
    async fn alter_database(&mut self, _database: &str, _changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        Err(Error::Query("Conexión de solo lectura: no se pueden modificar las propiedades de una base.".into()))
    }
    async fn create_database_choices(&mut self) -> Result<Vec<crate::FieldChoices>> {
        self.inner.create_database_choices().await
    }
    async fn drop_database(&mut self, _name: &str) -> Result<()> {
        Err(Error::Query("Conexión de solo lectura: no se pueden borrar bases.".into()))
    }
    async fn monitor(&mut self) -> Result<crate::MonitorSnapshot> {
        self.inner.monitor().await
    }
    async fn blocking(&mut self) -> Result<Vec<crate::BlockedSession>> {
        self.inner.blocking().await
    }
    async fn principals(&mut self) -> Result<Vec<crate::Principal>> {
        self.inner.principals().await
    }
    async fn grants(&mut self, principal: &str) -> Result<Vec<crate::Grant>> {
        self.inner.grants(principal).await
    }
    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<crate::BackupEntry>> {
        self.inner.backups(database).await
    }
    async fn kill_session(&mut self, _id: &str) -> Result<()> {
        Err(Error::Query("Conexión de solo lectura: no se pueden terminar sesiones.".into()))
    }
    async fn processes(&mut self) -> Result<Vec<crate::ServerProcess>> {
        self.inner.processes().await
    }
    async fn cancel_query(&mut self, _id: &str) -> Result<()> {
        Err(Error::Query("Conexión de solo lectura: no se pueden cancelar consultas de otras sesiones.".into()))
    }
    /// Profiles without changing server settings.
    async fn profiler_start(&mut self, opts: &crate::ProfilerOptions) -> Result<crate::ProfilerStarted> {
        let opts = crate::ProfilerOptions { change_server: false, ..opts.clone() };
        self.inner.profiler_start(&opts).await
    }
    async fn profiler_poll(&mut self) -> Result<Vec<crate::ProfiledStatement>> {
        self.inner.profiler_poll().await
    }
    async fn profiler_stop(&mut self) -> Result<()> {
        self.inner.profiler_stop().await
    }
    async fn read_batches(&mut self, spec: &crate::ReadSpec, sink: crate::BatchSinkRef) -> Result<u64> {
        self.inner.read_batches(spec, sink).await
    }
    async fn bulk_load(
        &mut self,
        _spec: &crate::LoadSpec,
        _columns: &[crate::TransferColumn],
        _source: &mut dyn crate::BatchSource,
        _progress: crate::transfer::Progress<'_>,
    ) -> Result<u64> {
        Err(Error::Query("Conexión de solo lectura: no se pueden cargar datos.".into()))
    }
    /// The wrapped session: a driver's native copy only reads from it.
    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        self.inner.as_any()
    }
    async fn key_range(&mut self, table: &ObjectRef, column: &str) -> Result<Option<(i64, i64, u64)>> {
        self.inner.key_range(table, column).await
    }
    async fn delta_summary(&mut self, spec: &crate::DeltaSpec) -> Result<Vec<crate::BucketSum>> {
        self.inner.delta_summary(spec).await
    }
    async fn delta_apply(
        &mut self,
        _spec: &crate::DeltaSpec,
        _buckets: &[i64],
        _columns: &[crate::TransferColumn],
        _source: &mut dyn crate::BatchSource,
        _progress: crate::transfer::Progress<'_>,
    ) -> Result<crate::DeltaResult> {
        Err(Error::Query("Conexión de solo lectura: no se pueden cargar datos.".into()))
    }
    /// The server's answer; the UI hides writes of read-only connections itself.
    async fn permissions(&mut self, database: Option<&str>) -> Result<crate::Permissions> {
        self.inner.permissions(database).await
    }
    async fn health_checks(&mut self, database: &str) -> Result<Vec<crate::health::HealthCheck>> {
        self.inner.health_checks(database).await
    }
    async fn row_estimates(&mut self) -> Result<Vec<crate::stats::RowEstimate>> {
        self.inner.row_estimates().await
    }
    async fn object_comments(&mut self) -> Result<Vec<crate::stats::ObjectComment>> {
        self.inner.object_comments().await
    }
    async fn search_code(&mut self, query: &crate::search::CodeSearch) -> Result<Option<crate::search::CodeSearchReport>> {
        self.inner.search_code(query).await
    }
    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<crate::IndexUsageReport>> {
        self.inner.index_usage(table).await
    }
    async fn dependents(&mut self, target: &crate::DependencyTarget, scan: &crate::DependencyScan) -> Result<crate::DependencyReport> {
        self.inner.dependents(target, scan).await
    }
}

#[cfg(test)]
mod tests {
    use super::first_write;

    #[test]
    fn reads_pass() {
        assert_eq!(first_write("select 1; -- delete\n with x as (select 1) select * from x"), None);
        assert_eq!(first_write("/* update */ SELECT 'a;delete'"), None);
    }

    #[test]
    fn writes_are_caught() {
        assert_eq!(first_write("select 1; delete from t").as_deref(), Some("DELETE"));
        assert_eq!(first_write("select 1\nGO\ndrop table t").as_deref(), Some("DROP"));
        // GO inside a comment doesn't split; GO 2 and GO -- x do.
        assert_eq!(first_write("select 1 /*\nGO\n*/\nGO 2\nupdate t set a = 1").as_deref(), Some("UPDATE"));
        assert_eq!(first_write("select 1\nGO -- next\ninsert into t values (1)").as_deref(), Some("INSERT"));
    }

    #[test]
    fn writes_hidden_inside_a_read_are_caught() {
        use crate::sql::ScriptDialect;
        let t = ScriptDialect::tsql();
        // A T-SQL batch needs no `;` between statements.
        assert_eq!(super::first_write_in("SELECT 1 DELETE FROM dbo.t", &t).as_deref(), Some("DELETE"));
        assert_eq!(super::first_write_in("SELECT 1 DROP TABLE dbo.Orders", &t).as_deref(), Some("DROP"));
        assert_eq!(super::first_write_in("SELECT 1 EXEC xp_cmdshell 'whoami'", &t).as_deref(), Some("EXEC"));
        assert_eq!(super::first_write_in("SELECT 1 SET IMPLICIT_TRANSACTIONS ON", &t).as_deref(), Some("SET"));
        assert_eq!(super::first_write_in("SELECT * INTO dbo.copy FROM dbo.t", &t).as_deref(), Some("INTO"));
        assert_eq!(super::first_write_in("SELECT * FROM OPENQUERY(srv, 'delete from t')", &t).as_deref(), Some("OPENQUERY"));
        for d in [ScriptDialect::generic(), ScriptDialect::postgres(), t] {
            // Data-modifying CTEs.
            assert_eq!(super::first_write_in("WITH c AS (SELECT 1 x) DELETE FROM t", &d).as_deref(), Some("DELETE"), "{d:?}");
            assert_eq!(super::first_write_in("with d as (delete from t returning 1) select count(*) from d", &d).as_deref(), Some("DELETE"), "{d:?}");
            assert_eq!(super::first_write_in("select 1 from t for update", &d).as_deref(), Some("UPDATE"), "{d:?}");
        }
        let pg = ScriptDialect::postgres();
        // Functions that switch the session's read-only mode off or act elsewhere.
        assert_eq!(super::first_write_in("SELECT set_config('default_transaction_read_only','off',false)", &pg).as_deref(), Some("SET_CONFIG"));
        assert_eq!(super::first_write_in("select pg_catalog.set_config('x','y',false)", &pg).as_deref(), Some("SET_CONFIG"));
        assert_eq!(super::first_write_in("select dblink_exec('dbname=x', 'drop table t')", &pg).as_deref(), Some("DBLINK_EXEC"));
        assert_eq!(super::first_write_in("select pg_terminate_backend(123)", &pg).as_deref(), Some("PG_TERMINATE_BACKEND"));
        assert_eq!(super::first_write_in("select query_to_xml('delete from t', true, false, '')", &pg).as_deref(), Some("QUERY_TO_XML"));
        let ora = ScriptDialect::oracle();
        assert_eq!(super::first_write_in("select utl_http.request('http://x') from dual", &ora).as_deref(), Some("UTL_HTTP"));
        // MySQL writes files from a SELECT.
        assert_eq!(super::first_write_in("select * from t into outfile '/tmp/x'", &ScriptDialect::mysql()).as_deref(), Some("INTO"));
    }

    #[test]
    fn ordinary_reads_still_pass() {
        use crate::sql::ScriptDialect;
        for d in [ScriptDialect::generic(), ScriptDialect::postgres(), ScriptDialect::tsql(), ScriptDialect::mysql(), ScriptDialect::oracle()] {
            assert_eq!(super::first_write_in("select replace(name, 'a', 'b'), update_date, sp_id, deleted from t where note = 'delete me'", &d), None, "{d:?}");
            assert_eq!(super::first_write_in("with x as (select 1 as a) select * from x order by a desc", &d), None, "{d:?}");
            assert_eq!(super::first_write_in("select \"update\" from t", &d), None, "{d:?}");
            if !d.tsql_blocks {
                assert_eq!(super::first_write_in("show create table t", &d), None, "{d:?}");
            }
        }
        assert_eq!(super::first_write_in("select cast(x as char character set utf8mb4) from t", &ScriptDialect::mysql()), None);
        assert_eq!(super::first_write_in("select dbms_metadata.get_ddl('TABLE', 'T') from dual", &ScriptDialect::oracle()), None);
        assert_eq!(super::first_write_in("select * from t order by dbms_random.value", &ScriptDialect::oracle()), None);
        assert_eq!(super::first_write_in("select t.\"set\", \"call\" as \"exec\" from t", &ScriptDialect::oracle()), None);
        assert_eq!(super::first_write_in("select [into], [set] from dbo.t", &ScriptDialect::tsql()), None);
        assert_eq!(super::first_write_in("select * from sys.dm_exec_sessions", &ScriptDialect::tsql()), None);
        assert_eq!(super::first_write_in("select count(*) into @n from t", &ScriptDialect::mysql()), None);
        let lite = ScriptDialect::generic();
        assert_eq!(super::first_write_in("pragma table_info(t)", &lite), None);
        assert_eq!(super::first_write_in("pragma main.table_info(t)", &lite), None);
        assert_eq!(super::first_write_in("pragma journal_mode", &lite), None);
        assert_eq!(super::first_write_in("pragma journal_mode = wal", &lite).as_deref(), Some("PRAGMA"));
        assert_eq!(super::first_write_in("pragma writable_schema(1)", &lite).as_deref(), Some("PRAGMA"));
    }

    #[test]
    fn tsql_state_changes_and_escaped_names_are_caught() {
        use crate::sql::ScriptDialect;
        let t = ScriptDialect::tsql();
        for (sql, kw) in [
            ("SELECT 1 DISABLE TRIGGER audit ON dbo.t", "DISABLE"),
            ("SELECT 1 ENABLE TRIGGER ALL ON DATABASE", "ENABLE"),
            ("SELECT 1 BACKUP LOG db TO DISK = 'nul'", "BACKUP"),
            ("SELECT 1 RESTORE DATABASE db FROM DISK = 'x'", "RESTORE"),
            ("SELECT 1 WRITETEXT t.c @p 'x'", "WRITETEXT"),
            ("SELECT 1 UPDATETEXT t.c @p 0 NULL 'x'", "UPDATETEXT"),
            ("SELECT 1 SEND ON CONVERSATION @h (0x01)", "SEND"),
            ("SELECT 1 RECEIVE TOP(1) * FROM q", "RECEIVE"),
            ("SELECT 1 END CONVERSATION @h", "END"),
            ("SELECT 1 SAVE TRANSACTION s", "SAVE"),
            ("SELECT 1 COMMIT", "COMMIT"),
            ("SELECT 1 CHECKPOINT", "CHECKPOINT"),
        ] {
            assert_eq!(super::first_write_in(sql, &t).as_deref(), Some(kw), "{sql}");
        }
        // Columns named like the paired words still read.
        assert_eq!(super::first_write_in("select enable, disable, save, send, case when a then 1 end from t", &t), None);
        // PostgreSQL's escaped names can spell a refused function.
        let pg = ScriptDialect::postgres();
        assert_eq!(super::first_write_in("select U&\"\\0073et_config\"('default_transaction_read_only','off',false)", &pg).as_deref(), Some("U&"));
        assert_eq!(super::first_write_in("select u&\"!0073et_config\" UESCAPE '!' ('a','b',false)", &pg).as_deref(), Some("U&"));
    }

    /// Records what reaches the driver.
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    #[async_trait::async_trait]
    impl crate::Session for Recorder {
        async fn server_version(&mut self) -> crate::error::Result<String> {
            Ok(String::new())
        }
        async fn list_databases(&mut self) -> crate::error::Result<Vec<String>> {
            Ok(vec![])
        }
        async fn list_objects(&mut self) -> crate::error::Result<Vec<crate::model::DbObject>> {
            Ok(vec![])
        }
        async fn columns(&mut self, _: &crate::model::ObjectRef) -> crate::error::Result<Vec<crate::model::ColumnInfo>> {
            Ok(vec![])
        }
        async fn definition(&mut self, _: &crate::model::ObjectRef) -> crate::error::Result<Option<String>> {
            Ok(None)
        }
        fn browse_query(&self, _: &crate::model::ObjectRef, _: u32) -> String {
            String::new()
        }
        async fn execute(&mut self, sql: &str, _: usize, _: &mut crate::model::QueryOutcome) -> crate::error::Result<()> {
            self.0.lock().unwrap().push(format!("execute {sql}"));
            Ok(())
        }
        async fn set_autocommit(&mut self, on: bool) -> crate::error::Result<()> {
            self.0.lock().unwrap().push(format!("autocommit {on}"));
            Ok(())
        }
        async fn rollback(&mut self) -> crate::error::Result<()> {
            self.0.lock().unwrap().push("rollback".into());
            Ok(())
        }
    }

    fn ready<T>(f: impl std::future::Future<Output = T>) -> T {
        let mut f = std::pin::pin!(f);
        match f.as_mut().poll(&mut std::task::Context::from_waker(std::task::Waker::noop())) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("pending"),
        }
    }

    #[test]
    fn tsql_reads_run_in_a_transaction_that_is_rolled_back() {
        use crate::sql::ScriptDialect;
        use crate::Session;
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ro = super::ReadOnlySession::with_dialect(Box::new(Recorder(log.clone())), ScriptDialect::tsql());
        ready(ro.execute("SELECT 1", 10, &mut Default::default())).unwrap();
        assert_eq!(*log.lock().unwrap(), ["autocommit false", "execute SELECT 1", "rollback", "autocommit true"]);
        // Other dialects keep their own read-only mode: no extra round trips.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ro = super::ReadOnlySession::with_dialect(Box::new(Recorder(log.clone())), ScriptDialect::postgres());
        ready(ro.execute("SELECT 1", 10, &mut Default::default())).unwrap();
        assert_eq!(*log.lock().unwrap(), ["execute SELECT 1"]);
    }

    #[test]
    fn read_leads_dont_exempt_what_follows() {
        use crate::sql::ScriptDialect;
        let t = ScriptDialect::tsql();
        // In T-SQL these leads would run as procedure calls.
        for sql in ["SHOW x DELETE FROM t COMMIT", "DESCRIBE t", "DESC t UPDATE t SET a = 1", "PRAGMA x", "VALUES (1)"] {
            assert!(super::first_write_in(sql, &t).is_some(), "{sql}");
        }
        assert_eq!(super::first_write_in("SELECT 1; USE db; PRINT 'x'", &t), None);
        // Elsewhere they read, but what follows is still checked.
        let g = ScriptDialect::generic();
        assert_eq!(super::first_write("SHOW x DELETE FROM t").as_deref(), Some("DELETE"));
        assert_eq!(super::first_write("DESCRIBE t COMMIT").as_deref(), Some("COMMIT"));
        assert_eq!(super::first_write_in("pragma table_info(t) delete from t", &g).as_deref(), Some("DELETE"));
        assert_eq!(super::first_write_in("show create table t", &ScriptDialect::mysql()), None);
        assert_eq!(super::first_write_in("describe t", &ScriptDialect::mysql()), None);
    }

    #[test]
    fn statements_that_dont_start_with_a_word_fail_closed() {
        use crate::sql::ScriptDialect;
        let pg = ScriptDialect::postgres();
        let t = ScriptDialect::tsql();
        // Parenthesized reads still read, and their words are checked.
        assert_eq!(super::first_write_in("(SELECT 1) UNION (SELECT 2)", &pg), None);
        assert_eq!(super::first_write_in("((select a from t))", &pg), None);
        assert_eq!(super::first_write_in("(SELECT set_config('default_transaction_read_only','off',false))", &pg).as_deref(), Some("SET_CONFIG"));
        assert_eq!(super::first_write_in("(SELECT 1) DELETE FROM t", &t).as_deref(), Some("DELETE"));
        // A quoted or bracketed name alone runs a procedure in T-SQL.
        assert_eq!(super::first_write_in("[dbo].[purge_all]", &t).as_deref(), Some("["));
        assert_eq!(super::first_write_in("\"purge_all\"", &t).as_deref(), Some("\""));
        assert_eq!(super::first_write_in("{call purge_all}", &pg).as_deref(), Some("{"));
        // Comments alone are no statement.
        assert_eq!(super::first_write_in("select 1; -- the end", &pg), None);
        assert_eq!(super::first_write_in("/* nothing */", &t), None);
        // The MCP pre-check uses the same rule.
        assert_eq!(super::first_write("[dbo].[purge_all]").as_deref(), Some("["));
    }

    #[test]
    fn estimated_plans_refuse_what_could_run() {
        use crate::sql::ScriptDialect;
        let t = ScriptDialect::tsql();
        // SHOWPLAN turned off by a later batch would run the rest.
        assert!(super::unsafe_to_plan("SELECT 1\nGO\nSET SHOWPLAN_XML OFF\nGO\nDROP TABLE dbo.orders", &t).is_some());
        assert!(super::unsafe_to_plan("SET NOEXEC OFF", &t).is_some());
        assert!(super::unsafe_to_plan("select 1; delete from t", &ScriptDialect::postgres()).is_some());
        assert!(super::unsafe_to_plan("select set_config('a','b',false)", &ScriptDialect::postgres()).is_some());
        // One statement, even a write, is only planned.
        assert_eq!(super::unsafe_to_plan("DELETE FROM dbo.t WHERE id = 1", &t), None);
        assert_eq!(super::unsafe_to_plan("select * from t; select * from u", &ScriptDialect::postgres()), None);
    }

    #[test]
    fn the_driver_dialect_keeps_blocks_and_quotes_whole() {
        use crate::sql::ScriptDialect;
        // A dollar-quoted body is one statement (a DO block is a write).
        let pg = ScriptDialect::postgres();
        assert_eq!(super::first_write_in("select $$; delete from t; $$", &pg), None);
        assert_eq!(super::first_write_in("DO $$ begin delete from t; end $$", &pg).as_deref(), Some("DO"));
        // DELIMITER is the client's: not a write.
        let my = ScriptDialect::mysql();
        assert_eq!(super::first_write_in("DELIMITER //\nselect 1//\nDELIMITER ;\nselect 2;", &my), None);
    }

    #[test]
    fn mysql_comment_quirks_dont_hide_writes() {
        use crate::sql::ScriptDialect;
        for d in [ScriptDialect::mysql(), ScriptDialect::generic(), ScriptDialect::postgres()] {
            // MySQL runs a versioned comment's content.
            assert_eq!(super::first_write_in("select 1; /*!50000 delete from t */;", &d).as_deref(), Some("DELETE"), "{d:?}");
            assert_eq!(super::first_write_in("select 1 /*!; delete from t */", &d).as_deref(), Some("DELETE"), "{d:?}");
            assert_eq!(super::first_write_in("select 1; /*M!100101 drop table t */", &d).as_deref(), Some("DROP"), "{d:?}");
            // Real comments still hide nothing and block nothing.
            assert_eq!(super::first_write_in("select 1 -- ; delete from t\n", &d), None, "{d:?}");
            assert_eq!(super::first_write_in("select /*+ hint */ 1; /* delete */ select 2", &d), None, "{d:?}");
        }
        // A plain comment that hides a write in PostgreSQL: still caught.
        assert_eq!(super::first_write_in("select 1; --x\ndelete from t", &ScriptDialect::postgres()).as_deref(), Some("DELETE"));
        // `--1` isn't a comment in MySQL: the delete after it runs.
        assert_eq!(super::first_write_in("select 1--1; delete from t;", &ScriptDialect::mysql()).as_deref(), Some("DELETE"));
        assert_eq!(super::first_write_in("select 1 --note; drop\n", &ScriptDialect::mysql()).as_deref(), Some("DROP"));
    }

    #[test]
    fn mysql_dash_rule_only_applies_to_mysql() {
        use crate::sql::ScriptDialect;
        // Elsewhere `--note` is a comment to the end of the line: a read.
        for d in [ScriptDialect::postgres(), ScriptDialect::generic(), ScriptDialect::tsql(), ScriptDialect::oracle()] {
            assert_eq!(super::first_write_in("select 1 --note; drop\n", &d), None, "{d:?}");
            assert_eq!(super::first_write_in("select 1--1; delete from t;", &d), None, "{d:?}");
            // A write on the next line is still caught.
            assert_eq!(super::first_write_in("select 1 --note\n; drop table t", &d).as_deref(), Some("DROP"), "{d:?}");
        }
    }
}

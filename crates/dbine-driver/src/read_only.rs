//! Read-only connections: a decorator that refuses any statement that
//! isn't a read before it reaches the server. It checks the first keyword
//! of each SQL statement, so it's a guard against mistakes, not a security
//! boundary — use a read-only login for that. Only SQL drivers are wrapped;
//! the others enforce read-only themselves (see `ConnectionConfig::read_only`).

use crate::error::{Error, Result};
use crate::model::{ColumnInfo, DbObject, ObjectRef, QueryOutcome, TxState};
use crate::sql::{expose_versioned, split_script, strip_comments, BatchLine, ScriptDialect, StatementKind};
use crate::Session;
use async_trait::async_trait;
use std::sync::Arc;

const READ_KEYWORDS: &[&str] = &["select", "with", "show", "explain", "describe", "desc", "values", "table", "pragma", "use", "list", "count", "print", "match", "traverse"];

pub struct ReadOnlySession {
    inner: Box<dyn Session>,
    dialect: ScriptDialect,
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
        let mut dialect = dialect.statements();
        if dialect.batch == BatchLine::None {
            dialect.batch = BatchLine::Go;
        }
        Self { inner, dialect }
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
    split_script(sql, &dialect.statements()).into_iter().filter(|s| s.kind != StatementKind::ClientCommand).find_map(|stmt| {
        let kw = first_keyword(&stmt.text, dialect)?;
        (!READ_KEYWORDS.contains(&kw.as_str())).then(|| kw.to_uppercase())
    })
}

fn first_keyword(stmt: &str, dialect: &ScriptDialect) -> Option<String> {
    let s = strip_comments(stmt, dialect, false);
    let word: String = s.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
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
        self.inner.execute(sql, max_rows, out).await
    }
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        // An estimated plan runs nothing; an actual one runs the script.
        if analyze {
            if let Some(kw) = self.first_write(sql) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: el plan real ejecutaría una sentencia {kw}. Pedí el plan estimado."
                )));
            }
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

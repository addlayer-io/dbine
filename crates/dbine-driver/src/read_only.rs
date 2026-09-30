//! Read-only connections: a decorator that refuses any statement that
//! isn't a read before it reaches the server. It checks the first keyword
//! of each SQL statement, so it's a guard against mistakes, not a security
//! boundary — use a read-only login for that. Only SQL drivers are wrapped;
//! the others enforce read-only themselves (see `ConnectionConfig::read_only`).

use crate::error::{Error, Result};
use crate::model::{ColumnInfo, DbObject, ObjectRef, QueryOutcome};
use crate::Session;
use async_trait::async_trait;
use std::sync::Arc;

const READ_KEYWORDS: &[&str] = &["select", "with", "show", "explain", "describe", "desc", "values", "table", "pragma", "use", "list", "count", "print", "match", "traverse"];

pub struct ReadOnlySession {
    inner: Box<dyn Session>,
}

impl ReadOnlySession {
    pub fn new(inner: Box<dyn Session>) -> Self {
        Self { inner }
    }
}

/// The first statement that isn't a read, if any.
pub fn first_write(sql: &str) -> Option<String> {
    split_statements(sql).into_iter().find_map(|stmt| {
        let kw = first_keyword(&stmt)?;
        (!READ_KEYWORDS.contains(&kw.as_str())).then(|| kw.to_uppercase())
    })
}

fn first_keyword(stmt: &str) -> Option<String> {
    let s = strip_comments(stmt);
    let word: String = s.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
}

fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'-') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                out.push('\n');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Statements split on `;` and `GO` lines, ignoring those inside quotes.
fn split_statements(sql: &str) -> Vec<String> {
    let sql = strip_comments(sql);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in sql.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' || c == '`' => quote = Some(c),
            None if c == ';' => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            None => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter()
        .flat_map(|s| {
            s.split('\n')
                .fold(vec![String::new()], |mut acc, line| {
                    if line.trim().eq_ignore_ascii_case("go") {
                        acc.push(String::new());
                    } else {
                        let last = acc.last_mut().expect("non-empty");
                        last.push_str(line);
                        last.push('\n');
                    }
                    acc
                })
        })
        .filter(|s| !s.trim().is_empty())
        .collect()
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
        if let Some(kw) = first_write(sql) {
            return Err(Error::Query(format!(
                "Conexión de solo lectura: se bloqueó una sentencia {kw}. Solo se permiten lecturas (SELECT, WITH, SHOW, EXPLAIN…)."
            )));
        }
        self.inner.execute(sql, max_rows, out).await
    }
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        // An estimated plan runs nothing; an actual one runs the script.
        if analyze {
            if let Some(kw) = first_write(sql) {
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
    async fn database_schema(&mut self) -> Result<Vec<crate::TableSchema>> {
        self.inner.database_schema().await
    }
    async fn create_database(&mut self, _name: &str) -> Result<()> {
        Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()))
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
    }
}

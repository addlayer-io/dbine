//! libSQL / Turso over HTTP (Hrana, `POST /v2/pipeline`), with no native
//! client: libSQL is SQLite's SQL, so the designer, the DDL, the catalog
//! reader, the plans and the monitor are the SQLite driver's, run over the
//! network. A local libSQL database is a SQLite file: the SQLite driver
//! opens it.

mod hrana;
mod transfer;

use dbine_driver::sql::{leading_keyword, quote_ident, select_top, Limit, Quote, ScriptDefaults, ScriptDialect, ScriptMode};
use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec,
    Driver, DriverInfo, Error, Family, Field, FieldKind, Language, MonitorSnapshot, ObjectKindInfo, ObjectRef,
    QueryOutcome, Result, ResultColumn, ScriptError, Session, TableSchema, TxState,
};
use dbine_driver_sqlite::schema::Rows;
use dbine_driver_sqlite::{monitor as sqlite_monitor, plan, schema};
use hrana::{Client, StmtResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(LibsqlDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "libsql",
        name: "libSQL / Turso",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "sqlite",
        default_port: 8080,
        fields: vec![
            Field::new("host", "URL de la base", FieldKind::Text)
                .required()
                .placeholder("libsql://mibase-miorg.turso.io o http://localhost:8080")
                .help("Turso: turso db show <base> --url. Para un archivo local usá la conexión SQLite."),
            Field::new("auth_token", "Token", FieldKind::Password)
                .secret()
                .help("Turso: turso db tokens create <base>. Vacío si el servidor no pide autenticación."),
            Field::read_only(),
        ],
        databases_label: "",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::tables(), schema::virtual_tables(), ObjectKindInfo::views(), ObjectKindInfo::triggers()],
    }
}

struct LibsqlDriver {
    info: DriverInfo,
}

fn sync_script(changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
    schema::sync_script(changes)
}

#[async_trait]
impl Driver for LibsqlDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// SQLite's (see the SQLite driver): `[name]` too, trigger bodies whole.
    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect { bracket_idents: true, ..ScriptDialect::generic() }
    }

    /// One statement per request on the session's Hrana stream, which keeps
    /// its state (temp tables, PRAGMAs, an open transaction).
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// As sqlite3 (and `turso db shell`): go on after an error.
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    /// On Hrana 3 servers (Turso, current sqld); older ones say so when
    /// Manual is chosen.
    fn supports_manual_transactions(&self) -> bool {
        true
    }

    /// Databases are created and dropped through Turso's platform API (or
    /// sqld's admin API), not with SQL.
    fn capabilities(&self) -> Capabilities {
        Capabilities { create_database: false, drop_database: false, foreign_keys: true, monitor: true, ..Default::default() }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(schema::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        schema::create_templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(schema::table_ddl(table, parts))
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    /// Prepared multi-row INSERTs over Hrana, a transaction per commit
    /// window (see [`transfer`]).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// SQLite's: columns are added in place; any other change rebuilds the table.
    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync_script(changes)
    }

    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let token = cfg.option("auth_token").or(cfg.password.as_deref().filter(|p| !p.is_empty())).map(str::to_string);
        let mut client = Client::new(&cfg.host, token)?;
        // Re-applied whenever the server starts a new stream.
        client.init.push("PRAGMA foreign_keys = ON".into());
        if cfg.read_only {
            // Not every server honours it (sqld doesn't): statements are
            // checked here too.
            client.init.push("PRAGMA query_only = ON".into());
        }
        client.execute("SELECT 1").await.map_err(|e| match e {
            Error::Query(m) => Error::Connect(m),
            other => other,
        })?;
        client.detect_v3().await;
        Ok(Box::new(LibsqlSession { client, read_only: cfg.read_only, manual: false }))
    }
}

struct LibsqlSession {
    client: Client,
    read_only: bool,
    /// Manual transactions: a statement that writes opens one when none is open.
    manual: bool,
}

/// Whether, in manual mode, `stmt` opens a transaction: anything but reads,
/// transaction control and what SQLite refuses inside one (VACUUM).
fn opens_transaction(stmt: &str) -> bool {
    const NO: &[&str] = &[
        "select", "with", "values", "explain", "pragma", "begin", "commit", "end", "rollback", "savepoint", "release", "vacuum", "attach",
        "detach",
    ];
    leading_keyword(stmt, &ScriptDialect::generic()).is_some_and(|k| !NO.contains(&k.as_str()))
}

/// A failed statement: Hrana's code, and the line when sqld's parser says
/// where ("syntax error around L2:7").
fn step_error(e: hrana::StepError, single: bool) -> Error {
    let mut se = ScriptError::new(e.message.clone());
    if let Some(c) = e.code {
        se = se.with_code(c);
    }
    if single {
        if let Some(line) = e.message.split("around L").nth(1).and_then(|r| r.split(':').next()).and_then(|l| l.parse::<u32>().ok()) {
            se = se.at_line(line.max(1));
        }
    }
    se.into()
}

/// Statements of a script, split where SQLite would (`sqlite3_complete`:
/// a trigger's `BEGIN … END` stays whole, `;` in strings and comments
/// doesn't split).
fn split_script(sql: &str) -> Vec<String> {
    fn complete(s: &str) -> bool {
        match std::ffi::CString::new(s) {
            // SAFETY: a NUL-terminated string that outlives the call.
            Ok(c) => unsafe { rusqlite::ffi::sqlite3_complete(c.as_ptr()) != 0 },
            Err(_) => false,
        }
    }
    fn has_code(s: &str) -> bool {
        let mut rest = s;
        loop {
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix("--") {
                rest = r.split_once('\n').map_or("", |x| x.1);
            } else if let Some(r) = rest.strip_prefix("/*") {
                rest = r.split_once("*/").map_or("", |x| x.1);
            } else {
                return !rest.trim_matches(|c: char| c == ';' || c.is_whitespace()).is_empty();
            }
        }
    }
    let mut out = Vec::new();
    let mut start = 0;
    for (i, ch) in sql.char_indices() {
        if ch == ';' && complete(&sql[start..=i]) {
            let piece = sql[start..i].trim();
            if has_code(piece) {
                out.push(piece.to_string());
            }
            start = i + 1;
        }
    }
    let rest = sql[start..].trim();
    if has_code(rest) {
        out.push(rest.to_string());
    }
    out
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn int(v: &Value) -> i64 {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0)
}

fn push_result(r: StmtResult, max_rows: usize, out: &mut QueryOutcome) {
    if r.cols.is_empty() {
        out.push_affected(r.affected);
        return;
    }
    out.begin_result(r.cols.into_iter().map(|(name, type_name)| ResultColumn { name, type_name }).collect());
    for row in r.rows {
        out.push_row(row, max_rows);
    }
}

impl LibsqlSession {
    fn notices(&mut self, out: &mut QueryOutcome) {
        for n in std::mem::take(&mut self.client.notices) {
            out.warning(n);
        }
    }

    /// Run `f` (a sync function that asks for query results) against the
    /// server: each round runs the queries it asked for and weren't known
    /// yet in one pipeline, until it asks for nothing new. This is how the
    /// SQLite driver's catalog reader and monitor run over HTTP.
    async fn replay<T>(&mut self, f: impl Fn(&mut dyn FnMut(&str) -> std::result::Result<Rows, String>) -> T) -> Result<T> {
        let mut known: HashMap<String, std::result::Result<Rows, String>> = HashMap::new();
        for _ in 0..6 {
            let mut missing: Vec<String> = Vec::new();
            let out = f(&mut |sql: &str| match known.get(sql) {
                Some(r) => r.clone(),
                None => {
                    if !missing.iter().any(|m| m == sql) {
                        missing.push(sql.to_string());
                    }
                    Ok(Vec::new())
                }
            });
            if missing.is_empty() {
                return Ok(out);
            }
            let results = self.client.execute_each(&missing).await?;
            for (sql, r) in missing.into_iter().zip(results) {
                known.insert(sql, r.map(|r| r.rows));
            }
        }
        Err(Error::State("la lectura del catálogo no terminó".into()))
    }
}

const LIST_OBJECTS: &str = "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_master
 WHERE type IN ('table', 'view', 'trigger') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
   AND name NOT LIKE '\\_litestream\\_%' ESCAPE '\\' AND name NOT LIKE 'libsql\\_%' ESCAPE '\\'
 ORDER BY name";

#[async_trait]
impl Session for LibsqlSession {
    async fn server_version(&mut self) -> Result<String> {
        let sqlite = self.client.execute("SELECT sqlite_version()").await?;
        let sqlite = sqlite.rows.first().and_then(|r| r.first()).and_then(text).unwrap_or_default();
        Ok(match self.client.version().await {
            Some(v) => format!("libSQL ({v}) · SQLite {sqlite}"),
            None => format!("libSQL · SQLite {sqlite}"),
        })
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["main".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let r = self.client.execute(LIST_OBJECTS).await?;
        Ok(r.rows
            .iter()
            .filter_map(|row| {
                let (ty, name, tbl) = (text(row.first()?)?, text(row.get(1)?)?, text(row.get(2)?));
                let sql = row.get(3).and_then(text).unwrap_or_default();
                let (kind, parent) = match ty.as_str() {
                    "table" if schema::is_virtual(&sql) => (schema::VIRTUAL_TABLE, None),
                    "table" => (kinds::TABLE, None),
                    "view" => (kinds::VIEW, None),
                    _ => (kinds::TRIGGER, tbl),
                };
                Some(DbObject { kind: kind.into(), schema: None, name, parent })
            })
            .collect())
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let sql = format!(
            "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info({}, {}) ORDER BY cid",
            lit(&obj.name),
            lit(obj.schema().unwrap_or("main"))
        );
        let r = self.client.execute(&sql).await?;
        let pk_count = r.rows.iter().filter(|row| int(&row[4]) > 0).count();
        Ok(r.rows
            .iter()
            .map(|row| {
                let data_type = text(&row[1]).unwrap_or_default();
                let pk = int(&row[4]);
                ColumnInfo {
                    name: text(&row[0]).unwrap_or_default(),
                    // A lone INTEGER PRIMARY KEY is the rowid alias.
                    auto_increment: pk > 0 && pk_count == 1 && data_type.eq_ignore_ascii_case("INTEGER"),
                    data_type,
                    nullable: int(&row[2]) == 0 && pk == 0,
                    default_value: text(&row[3]),
                    primary_key: pk > 0,
                }
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let ty = match obj.kind.as_str() {
            kinds::TABLE | schema::VIRTUAL_TABLE => "table",
            kinds::VIEW => "view",
            kinds::TRIGGER => "trigger",
            _ => return Ok(None),
        };
        let master = match obj.schema() {
            Some(s) => format!("{}.sqlite_master", quote_ident(Quote::Double, s)),
            None => "sqlite_master".to_string(),
        };
        let sql = format!("SELECT sql FROM {master} WHERE type = '{ty}' AND name = '{}'", obj.name.replace('\'', "''"));
        let r = self.client.execute(&sql).await?;
        Ok(r.rows.first().and_then(|row| row.first()).and_then(text))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    /// The whole script in one round trip (a Hrana batch that stops at the
    /// first error). The server sends every row; `max_rows` only limits
    /// what's kept.
    async fn execute(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if self.read_only {
            if let Some(kw) = dbine_driver::read_only::first_write(sql) {
                return Err(Error::Query(format!("La conexión es de solo lectura: no se permite {kw}.")));
            }
        }
        let stmts = split_script(sql);
        if stmts.is_empty() {
            return Ok(());
        }
        let begin: Vec<bool> = stmts.iter().map(|s| self.manual && opens_transaction(s)).collect();
        let res = self.client.script(&stmts, &begin).await;
        self.notices(out);
        let (results, error) = res?;
        for r in results {
            push_result(r, max_rows, out);
        }
        match error {
            Some((_, e)) => Err(step_error(e, stmts.len() == 1)),
            None => Ok(()),
        }
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        Ok(self.client.autocommit.map(|on| if on { TxState::Idle } else { TxState::Open }))
    }

    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if !on && !self.client.v3 {
            return Err(Error::Unsupported("este servidor libSQL no informa transacciones (necesita Hrana 3): usá BEGIN y COMMIT".into()));
        }
        // On: a transaction still open is committed (the UI asks Commit /
        // Rollback first), so later statements don't join it.
        if on && self.client.v3 && self.client.autocommit == Some(false) {
            self.client.end_transaction("COMMIT").await?;
        }
        self.manual = !on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.client.end_transaction("COMMIT").await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.client.end_transaction("ROLLBACK").await
    }

    /// `EXPLAIN QUERY PLAN` per statement, as in SQLite: no costs nor row
    /// counts; with `analyze` the script runs and the plans are still the
    /// estimated ones.
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if analyze {
            out.messages.push("libSQL no mide cifras reales: se muestran los planes estimados.".into());
        }
        if analyze && self.read_only {
            if let Some(kw) = dbine_driver::read_only::first_write(sql) {
                return Err(Error::Query(format!("La conexión es de solo lectura: no se permite {kw}.")));
            }
        }
        for stmt in split_script(sql) {
            if plan::explainable(&stmt) {
                let r = self.client.execute(&format!("EXPLAIN QUERY PLAN {stmt}")).await?;
                let rows: Vec<(i64, i64, String)> = r
                    .rows
                    .iter()
                    .map(|row| (int(&row[0]), int(&row[1]), row.get(3).and_then(text).unwrap_or_default()))
                    .collect();
                out.plans.push(plan::query_plan(&stmt, &rows));
            } else if !analyze {
                out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt)));
            }
            if analyze {
                let r = self.client.execute(&stmt).await;
                self.notices(out);
                push_result(r?, max_rows, out);
            }
        }
        Ok(())
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        self.replay(|q| schema::read_schema_with(q)).await?.map_err(Error::Query)
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = self.replay(|q| sqlite_monitor::pragma_snapshot(q)).await?;
        let version = self.client.version().await;
        snap.info.insert(0, ("URL".into(), self.client.base().to_string()));
        if let Some(v) = version {
            snap.info.insert(1, ("Servidor".into(), v));
        }
        // "no es un servidor" is the local file's note; this one is remote.
        snap.notes.retain(|n| !n.starts_with("SQLite no es un servidor"));
        snap.notes.insert(
            0,
            "libSQL no expone CPU, memoria ni sesiones por SQL: en Turso, el uso (filas leídas y escritas, \
             almacenamiento) está en la consola o en la API de la plataforma."
                .into(),
        );
        Ok(snap)
    }

    /// Typed cells through a streaming Hrana 3 cursor (see [`transfer`]).
    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        _columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, source, progress).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_split_like_sqlite() {
        let s = split_script(
            "CREATE TABLE t (a TEXT DEFAULT ';');
             -- a comment; with a semicolon
             CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a = 'x;y'; SELECT 1; END;
             /* only a comment */;
             SELECT * FROM t",
        );
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(s[1].starts_with("-- a comment") && s[1].ends_with("END"), "{:?}", s[1]);
        assert_eq!(s[2], "SELECT * FROM t");
        assert!(split_script("  ;; -- nada\n").is_empty());
    }

    #[test]
    fn writes_open_transactions_and_reads_dont() {
        assert!(opens_transaction("insert into t values (1)") && opens_transaction("/* x */ create table t (a)"));
        assert!(!opens_transaction("select 1") && !opens_transaction("BEGIN") && !opens_transaction("commit") && !opens_transaction("vacuum"));
    }

    #[test]
    fn errors_carry_code_and_line() {
        let e = step_error(hrana::StepError { message: "syntax error around L2:7: `selec`".into(), code: Some("SQL_PARSE_ERROR".into()) }, true);
        let e = e.to_script_error();
        assert_eq!((e.code.as_deref(), e.line), (Some("SQL_PARSE_ERROR"), Some(2)));
    }

    #[test]
    fn info_is_sqlite_flavoured() {
        let i = info();
        assert_eq!((i.id, i.dialect), ("libsql", "sqlite"));
        assert!(i.fields.iter().any(|f| f.key == "auth_token" && f.secret));
    }

    #[test]
    fn sync_adds_in_place_and_rebuilds_the_rest() {
        use dbine_driver::{ColumnDef, KeyDef, TableChange};
        let col = |n: &str, t: &str, null: bool| ColumnDef { name: n.into(), data_type: t.into(), nullable: null, ..Default::default() };
        let old = TableSchema { name: "t".into(), columns: vec![col("id", "INTEGER", false), col("n", "TEXT", true)], primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }), ..Default::default() };
        let mut new = old.clone();
        new.columns.push(col("e", "TEXT", true));
        let s = sync_script(&[TableChange::Alter { old: old.clone(), new }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE \"t\" ADD COLUMN \"e\" TEXT NULL;"]);
        let mut new = old.clone();
        new.columns[1].nullable = false;
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements.len(), 1);
        assert!(s.statements[0].contains("INSERT INTO \"t__dbine_new\" (\"id\", \"n\") SELECT \"id\", \"n\" FROM \"t\";\nDROP TABLE \"t\";\nALTER TABLE \"t__dbine_new\" RENAME TO \"t\";"), "{}", s.statements[0]);
    }
}

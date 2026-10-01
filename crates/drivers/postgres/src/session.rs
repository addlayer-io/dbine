//! One live connection. PostgreSQL-catalog variants use typed catalog
//! queries with parameters; Redshift and Denodo, whose catalogs differ,
//! use text-protocol queries against their own views. Every secondary
//! catalog query is optional: when a variant doesn't have it, the explorer
//! shows what did work instead of failing.

use crate::catalog::{cell, first_cell, info_type, lit, rows, system_schema, user_schema};
use crate::{err, Variant};
use crate::plan::{self, StmtKind};
use dbine_driver::sql::{qualified_name, select_top, split_statements, Limit, Quote};
use crate::script;
use dbine_driver::{
    async_trait, kinds, ColumnDef, ColumnInfo, DbObject, Error, KeyDef, ObjectRef, Plan, QueryOutcome, ResultColumn,
    Result, Session, StatementResult, TableSchema, TxState,
};
use futures::{pin_mut, StreamExt};
use postgres_native_tls::MakeTlsConnector;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_postgres::error::{DbError, SqlState};
use tokio_postgres::{Client, SimpleQueryMessage, SimpleQueryRow};

pub struct PgSession {
    pub(crate) client: Client,
    pub(crate) tls: MakeTlsConnector,
    pub(crate) variant: Variant,
    /// Server version as `server_version_num` (e.g. 160002); 0 if unknown.
    pub(crate) version: i32,
    pub(crate) database: String,
    /// Notices from the connection task, in the order the server sent them.
    notices: mpsc::UnboundedReceiver<DbError>,
    /// The running profiler, if any.
    profiler: Option<crate::profiler::State>,
    /// `false`: manual transactions, each statement joins the transaction
    /// the first one opened (see [`Session::set_autocommit`]).
    autocommit: bool,
    /// The transaction as the statements run here left it (see
    /// [`script::next_state`]); asked to the server when it may be off.
    tx: TxState,
    /// A text of several statements controlled the transaction: the
    /// tracked state can't be trusted until the server is asked.
    tx_unsure: bool,
}

/// `$1`/`$2` (schema, name) resolved to a regclass; an empty schema means
/// "whatever the search_path finds".
const REGCLASS: &str = "to_regclass(CASE WHEN $1::text = '' THEN quote_ident($2::text) \
     ELSE quote_ident($1::text) || '.' || quote_ident($2::text) END)";

/// The same for servers before 9.6 (openGauss, Greenplum 6), whose
/// `to_regclass` is missing or only takes `cstring`.
const REGCLASS_OLD: &str = "(SELECT rc.oid FROM pg_class rc JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
     WHERE rc.relname = $2::text AND (rn.nspname = $1::text OR ($1::text = '' AND pg_table_is_visible(rc.oid))) LIMIT 1)";

impl PgSession {
    pub(crate) fn new(
        client: Client,
        tls: MakeTlsConnector,
        variant: Variant,
        version: i32,
        database: String,
        notices: mpsc::UnboundedReceiver<DbError>,
    ) -> Self {
        Self { client, tls, variant, version, database, notices, profiler: None, autocommit: true, tx: TxState::Idle, tx_unsure: false }
    }

    /// `pg_proc` filter for plain functions and procedures (no aggregates
    /// or window functions); `prokind` only exists since PostgreSQL 11.
    fn routine_filter(&self) -> &'static str {
        if self.version >= 110000 {
            "p.prokind IN ('f', 'p')"
        } else {
            "NOT p.proisagg AND NOT p.proiswindow"
        }
    }

    fn routine_kind(&self) -> &'static str {
        if self.version >= 110000 {
            "CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END"
        } else {
            "'function'"
        }
    }

    /// `$1`/`$2` (schema, name) as a relation oid.
    fn regclass(&self) -> &'static str {
        if self.version > 0 && self.version < 90600 && self.variant != Variant::Materialize {
            REGCLASS_OLD
        } else {
            REGCLASS
        }
    }

    pub(crate) fn filter(&self, col: &str) -> String {
        user_schema(self.variant, col)
    }

    /// Rows of a text-protocol query.
    pub(crate) async fn text(&self, sql: &str) -> Result<Vec<SimpleQueryRow>> {
        Ok(rows(self.client.simple_query(sql).await.map_err(err)?))
    }

    /// Like [`Self::text`], but a statement still running after `limit`
    /// is cancelled on the server (the monitor must never hang).
    pub(crate) async fn text_within(&self, sql: &str, limit: std::time::Duration) -> Result<Vec<SimpleQueryRow>> {
        let query = self.client.simple_query(sql);
        futures::pin_mut!(query);
        match tokio::time::timeout(limit, &mut query).await {
            Ok(r) => Ok(rows(r.map_err(err)?)),
            Err(_) => {
                if let Err(e) = self.client.cancel_token().cancel_query(self.tls.clone()).await {
                    tracing::debug!("{:?}: cancel failed: {e}", self.variant);
                }
                // Let the cancelled statement finish so the connection is free.
                let _ = query.await;
                Err(Error::Query(format!("sin respuesta en {} s", limit.as_secs())))
            }
        }
    }

    /// The rows of the first query that works; the last error otherwise.
    async fn first_working(&self, queries: &[String]) -> Result<Vec<SimpleQueryRow>> {
        let mut last = None;
        for q in queries {
            match self.text(q).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    tracing::debug!("{:?}: catalog query failed: {e}", self.variant);
                    last = Some(e);
                }
            }
        }
        Err(last.expect("at least one query"))
    }

    /// Tables and views from `information_schema`, the one catalog every
    /// variant is most likely to have.
    fn info_schema_relations(&self) -> String {
        format!(
            "SELECT CASE WHEN table_type = 'VIEW' THEN 'view' ELSE 'table' END AS kind,
                    table_schema AS schema, table_name AS name
             FROM information_schema.tables WHERE {}",
            self.filter("table_schema")
        )
    }

    /// Tables, views and materialized views from `pg_class`.
    async fn pg_relations(&self) -> Result<Vec<DbObject>> {
        let sql = format!(
            "SELECT CASE WHEN c.relkind IN ('r', 'p') THEN 'table'
                         WHEN c.relkind = 'm' THEN 'materialized_view' ELSE 'view' END,
                    n.nspname::text, c.relname::text
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'p', 'v', 'm') AND {}",
            self.filter("n.nspname")
        );
        let rows = self.client.query(&sql, &[]).await.map_err(err)?;
        Ok(rows
            .iter()
            .map(|r| DbObject { kind: r.get(0), schema: Some(r.get(1)), name: r.get(2), parent: None })
            .collect())
    }

    /// Functions, procedures and triggers from `pg_proc` / `pg_trigger`.
    async fn pg_routines_and_triggers(&self) -> Vec<DbObject> {
        let routines = format!(
            "SELECT {kind}, n.nspname::text, p.proname::text, NULL::text
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE {routines} AND {filter}
               AND NOT EXISTS (SELECT 1 FROM pg_depend d
                               WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype IN ('e', 'i'))",
            kind = self.routine_kind(),
            routines = self.routine_filter(),
            filter = self.filter("n.nspname"),
        );
        let triggers = format!(
            "SELECT 'trigger', n.nspname::text, t.tgname::text, c.relname::text
             FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE NOT t.tgisinternal AND {}",
            self.filter("n.nspname")
        );
        let mut out = Vec::new();
        for sql in [routines, triggers] {
            match self.client.query(&sql, &[]).await {
                Ok(rows) => out.extend(rows.iter().map(|r| DbObject {
                    kind: r.get(0),
                    schema: Some(r.get(1)),
                    name: r.get(2),
                    parent: r.get(3),
                })),
                Err(e) => tracing::debug!("{:?}: routines/triggers unavailable: {e}", self.variant),
            }
        }
        out
    }

    async fn redshift_objects(&self) -> Result<Vec<DbObject>> {
        let svv = format!(
            "SELECT CASE WHEN table_type = 'VIEW' THEN 'view' ELSE 'table' END AS kind,
                    table_schema AS schema, table_name AS name
             FROM svv_tables WHERE table_catalog = current_database() AND {}",
            self.filter("table_schema")
        );
        let pg_class = format!(
            "SELECT CASE WHEN c.relkind = 'r' THEN 'table' ELSE 'view' END AS kind,
                    n.nspname AS schema, c.relname AS name
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'v') AND {}",
            self.filter("n.nspname")
        );
        let mut out = text_objects(&self.first_working(&[svv, pg_class, self.info_schema_relations()]).await?, true);
        // Stored procedures and Python/SQL UDFs; pg_proc_info has prokind.
        let routines = format!(
            "SELECT CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END AS kind,
                    n.nspname AS schema, p.proname AS name
             FROM pg_proc_info p JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE {}",
            self.filter("n.nspname")
        );
        match self.text(&routines).await {
            Ok(r) => out.extend(text_objects(&r, true)),
            Err(e) => tracing::debug!("redshift: routines unavailable: {e}"),
        }
        Ok(out)
    }

    /// Sources and sinks of the streaming engines (tables fed by a
    /// connector are listed as tables, not as sources).
    async fn streaming_objects(&self) -> Vec<DbObject> {
        let queries: [String; 2] = match self.variant {
            Variant::RisingWave => [
                "SELECT 'source' AS kind, sc.name AS schema, s.name AS name
                 FROM rw_catalog.rw_sources s JOIN rw_catalog.rw_schemas sc ON sc.id = s.schema_id
                 WHERE s.associated_table_id IS NULL"
                    .into(),
                "SELECT 'sink' AS kind, sc.name AS schema, s.name AS name
                 FROM rw_catalog.rw_sinks s JOIN rw_catalog.rw_schemas sc ON sc.id = s.schema_id"
                    .into(),
            ],
            _ => [
                format!(
                    "SELECT 'source' AS kind, sc.name AS schema, s.name AS name
                     FROM mz_catalog.mz_sources s JOIN mz_catalog.mz_schemas sc ON sc.id = s.schema_id
                     JOIN mz_catalog.mz_databases d ON d.id = sc.database_id
                     WHERE s.id LIKE 'u%' AND s.type NOT IN ('progress', 'subsource', 'table') AND d.name = {}",
                    lit(self.variant, &self.database)
                ),
                format!(
                    "SELECT 'sink' AS kind, sc.name AS schema, s.name AS name
                     FROM mz_catalog.mz_sinks s JOIN mz_catalog.mz_schemas sc ON sc.id = s.schema_id
                     JOIN mz_catalog.mz_databases d ON d.id = sc.database_id
                     WHERE s.id LIKE 'u%' AND d.name = {}",
                    lit(self.variant, &self.database)
                ),
            ],
        };
        let mut out = Vec::new();
        for q in queries {
            match self.text(&q).await {
                Ok(rows) => out.extend(text_objects(&rows, true)),
                Err(e) => tracing::debug!("{:?}: sources/sinks unavailable: {e}", self.variant),
            }
        }
        out
    }

    /// `SHOW CREATE …` of RisingWave, Materialize and CrateDB (CrateDB only
    /// has it for tables; its views come from `information_schema`).
    async fn show_create(&self, obj: &ObjectRef) -> Result<Option<String>> {
        let q = qualified_name(Quote::Double, obj.schema(), &obj.name);
        let what = match obj.kind.as_str() {
            // H2 writes its DDL with SCRIPT, not SHOW CREATE.
            kinds::TABLE if self.variant == Variant::H2 => {
                let rows = self.text(&format!("SCRIPT NODATA NOPASSWORDS NOSETTINGS TABLE {q}")).await?;
                let ddl: Vec<String> = rows
                    .iter()
                    .filter_map(|r| r.get(0).map(str::to_string))
                    .filter(|l| !l.starts_with("--") && !l.starts_with("CREATE USER") && !l.starts_with("CREATE SCHEMA"))
                    .collect();
                return Ok((!ddl.is_empty()).then(|| ddl.join("\n")));
            }
            kinds::TABLE => "TABLE",
            kinds::VIEW if matches!(self.variant, Variant::CrateDb | Variant::H2) => {
                let sql = format!(
                    "SELECT view_definition FROM information_schema.views WHERE table_schema = {} AND table_name = {}",
                    lit(self.variant, obj.schema().unwrap_or("doc")),
                    lit(self.variant, &obj.name)
                );
                let def = self.text(&sql).await?.first().and_then(|r| r.get(0).map(str::to_string));
                return Ok(def.map(|d| format!("CREATE OR REPLACE VIEW {q} AS\n{d}")));
            }
            kinds::VIEW => "VIEW",
            kinds::MATERIALIZED_VIEW => "MATERIALIZED VIEW",
            crate::SOURCE => "SOURCE",
            crate::SINK => "SINK",
            _ => return Ok(None),
        };
        Ok(self.show_statement(&format!("SHOW CREATE {what} {q}"), "create_sql").await)
    }

    async fn denodo_objects(&self) -> Result<Vec<DbObject>> {
        let get_views = format!(
            "SELECT CASE WHEN view_type = 0 THEN 'table' ELSE 'view' END AS kind, name
             FROM GET_VIEWS() WHERE input_database_name = {}",
            lit(self.variant, &self.database)
        );
        let rows = self.first_working(&[self.info_schema_relations(), get_views]).await?;
        Ok(text_objects(&rows, false))
    }

    /// Columns from `information_schema.columns` or a view shaped like it
    /// (Redshift's `svv_columns`).
    async fn info_schema_columns(&self, view: &str, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let v = self.variant;
        let schema = match obj.schema() {
            Some(s) => format!(" AND table_schema = {}", lit(v, s)),
            None if v == Variant::Denodo => String::new(),
            None => " AND table_schema = current_schema()".into(),
        };
        let sql = format!(
            "SELECT column_name, data_type, character_maximum_length, numeric_precision, numeric_scale,
                    is_nullable, column_default
             FROM {view} WHERE table_name = {}{schema} ORDER BY ordinal_position",
            lit(v, &obj.name)
        );
        let rows = self.text(&sql).await?;
        Ok(rows
            .iter()
            .map(|r| {
                let data_type = cell(r, "data_type").unwrap_or_default();
                let default_value = cell(r, "column_default");
                let auto_increment = default_value
                    .as_deref()
                    .is_some_and(|d| d.starts_with("nextval(") || d.contains("identity"));
                ColumnInfo {
                    name: cell(r, "column_name").unwrap_or_default(),
                    data_type: info_type(
                        &data_type,
                        cell(r, "character_maximum_length").as_deref(),
                        cell(r, "numeric_precision").as_deref(),
                        cell(r, "numeric_scale").as_deref(),
                    ),
                    nullable: cell(r, "is_nullable").is_none_or(|n| n.eq_ignore_ascii_case("YES")),
                    primary_key: false,
                    auto_increment,
                    default_value,
                }
            })
            .collect())
    }

    async fn denodo_columns(&self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        match self.info_schema_columns("information_schema.columns", obj).await {
            Ok(c) if !c.is_empty() => return Ok(c),
            Ok(_) => {}
            Err(e) => tracing::debug!("denodo: information_schema.columns unavailable: {e}"),
        }
        let sql = format!(
            "SELECT column_name, column_vdp_type, column_is_nullable
             FROM GET_VIEW_COLUMNS() WHERE input_database_name = {} AND input_view_name = {}",
            lit(self.variant, &self.database),
            lit(self.variant, &obj.name)
        );
        let rows = self.text(&sql).await?;
        Ok(rows
            .iter()
            .map(|r| ColumnInfo {
                name: cell(r, "column_name").unwrap_or_default(),
                data_type: cell(r, "column_vdp_type").unwrap_or_default(),
                nullable: cell(r, "column_is_nullable").is_none_or(|n| n == "t" || n.eq_ignore_ascii_case("true")),
                primary_key: false,
                auto_increment: false,
                default_value: None,
            })
            .collect())
    }

    async fn pg_columns(&self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let identity = if self.version >= 100000 { "a.attidentity <> ''" } else { "false" };
        let hidden = if self.variant == Variant::Cockroach {
            // CockroachDB's implicit `rowid` key is a hidden column.
            " AND NOT EXISTS (SELECT 1 FROM information_schema.columns ic
                              WHERE ic.table_schema = n.nspname AND ic.table_name = c.relname
                                AND ic.column_name = a.attname AND ic.is_hidden = 'YES')"
        } else {
            ""
        };
        let sql = format!(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                    pg_get_expr(d.adbin, d.adrelid),
                    EXISTS (SELECT 1 FROM pg_index i
                            WHERE i.indrelid = a.attrelid AND i.indisprimary AND a.attnum = ANY (i.indkey)),
                    {identity}
             FROM pg_attribute a
             JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
             WHERE a.attrelid = {regclass} AND a.attnum > 0 AND NOT a.attisdropped{hidden}
             ORDER BY a.attnum",
            regclass = self.regclass()
        );
        let rows = self.client.query(&sql, &[&obj.schema().unwrap_or(""), &obj.name]).await.map_err(err)?;
        Ok(rows
            .iter()
            .map(|r| {
                let default_value: Option<String> = r.get(3);
                let auto_increment = r.get::<_, bool>(5)
                    || default_value.as_deref().is_some_and(|d| d.starts_with("nextval(") || d == "unique_rowid()");
                ColumnInfo {
                    name: r.get(0),
                    data_type: r.get(1),
                    nullable: r.get(2),
                    primary_key: r.get(4),
                    auto_increment,
                    default_value,
                }
            })
            .collect())
    }

    async fn pg_view_definition(&self, obj: &ObjectRef) -> Result<Option<String>> {
        let sql = format!(
            "SELECT c.relkind::text, n.nspname::text, pg_get_viewdef(c.oid, true)
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.oid = {regclass} AND c.relkind IN ('v', 'm')",
            regclass = self.regclass()
        );
        let Some(r) =
            self.client.query_opt(&sql, &[&obj.schema().unwrap_or(""), &obj.name]).await.map_err(err)?
        else {
            return Ok(None);
        };
        let (relkind, nsp, def): (String, String, String) = (r.get(0), r.get(1), r.get(2));
        let q = qualified_name(Quote::Double, Some(&nsp), &obj.name);
        let head = if relkind == "m" {
            format!("CREATE MATERIALIZED VIEW {q} AS\n")
        } else {
            format!("CREATE OR REPLACE VIEW {q} AS\n")
        };
        Ok(Some(head + &def))
    }

    async fn pg_routine_definition(&self, obj: &ObjectRef) -> Result<Option<String>> {
        // openGauss returns a record (headerlines, definition).
        let def = if self.variant == Variant::OpenGauss { "(pg_get_functiondef(p.oid)).definition" } else { "pg_get_functiondef(p.oid)" };
        let sql = format!(
            "SELECT {def}
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE p.proname = $2::text AND {}
               AND (n.nspname = $1::text OR ($1::text = '' AND pg_function_is_visible(p.oid)))
             ORDER BY p.oid",
            self.routine_filter()
        );
        joined(self.client.query(&sql, &[&obj.schema().unwrap_or(""), &obj.name]).await.map_err(err)?)
    }

    async fn pg_trigger_definition(&self, obj: &ObjectRef) -> Result<Option<String>> {
        let sql = "SELECT pg_get_triggerdef(t.oid, true)
             FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE t.tgname = $2::text AND NOT t.tgisinternal
               AND (n.nspname = $1::text OR ($1::text = '' AND pg_table_is_visible(c.oid)))
             ORDER BY c.relname";
        joined(self.client.query(sql, &[&obj.schema().unwrap_or(""), &obj.name]).await.map_err(err)?)
    }

    /// The first cell of a `SHOW CREATE …`-like statement, `None` if the
    /// engine refuses it.
    async fn show_statement(&self, sql: &str, column: &str) -> Option<String> {
        match self.text(sql).await {
            Ok(rows) => rows.first().and_then(|r| cell(r, column).or_else(|| r.get(r.len().saturating_sub(1)).map(str::to_string))),
            Err(e) => {
                tracing::debug!("{:?}: {sql}: {e}", self.variant);
                None
            }
        }
    }

    async fn redshift_definition(&self, obj: &ObjectRef) -> Result<Option<String>> {
        let v = self.variant;
        let schema = obj.schema().unwrap_or("public");
        let q = qualified_name(Quote::Double, Some(schema), &obj.name);
        match obj.kind.as_str() {
            // `SHOW TABLE` / `SHOW VIEW` return the DDL (Redshift, 2022+).
            kinds::TABLE => Ok(self.show_statement(&format!("SHOW TABLE {q}"), "").await),
            kinds::VIEW => {
                if let Some(d) = self.show_statement(&format!("SHOW VIEW {q}"), "").await {
                    return Ok(Some(d));
                }
                let sql = format!(
                    "SELECT pg_get_viewdef(c.oid, true) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                     WHERE n.nspname = {} AND c.relname = {}",
                    lit(v, schema),
                    lit(v, &obj.name)
                );
                let def = first_cell(&self.client.simple_query(&sql).await.map_err(err)?);
                Ok(def.map(|d| format!("CREATE OR REPLACE VIEW {q} AS\n{d}")))
            }
            kinds::PROCEDURE | kinds::FUNCTION => {
                let sql = format!(
                    "SELECT p.prosrc FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                     WHERE n.nspname = {} AND p.proname = {}",
                    lit(v, schema),
                    lit(v, &obj.name)
                );
                let parts: Vec<String> =
                    self.text(&sql).await?.iter().filter_map(|r| r.get(0).map(str::to_string)).collect();
                Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
            }
            _ => Ok(None),
        }
    }
}

impl PgSession {
    async fn explain_script(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let v = self.variant;
        let can_analyze = v.can_analyze();
        if analyze && !can_analyze {
            out.messages.push(format!(
                "{} no da cifras reales en EXPLAIN: se muestran los planes estimados.",
                v.info().name
            ));
        }
        for stmt in dbine_driver::sql::split_script(sql, &script::DIALECT).into_iter().map(|u| u.text) {
            let mut kind = plan::classify(&stmt);
            if kind == StmtKind::Write && !v.explains_writes() {
                if !analyze {
                    out.messages.push(format!("Sin plan ({} no explica escrituras): {}", v.info().name, plan::short(&stmt)));
                    continue;
                }
                kind = StmtKind::Other;
            }
            match (analyze, kind) {
                (false, StmtKind::Other) => {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt)));
                }
                (false, _) => {
                    let p = self.plan_of(&stmt, false).await?;
                    out.plans.push(p);
                }
                (true, StmtKind::Read) if can_analyze => {
                    self.run_statement(&stmt, max_rows, out).await?;
                    let p = self.plan_of(&stmt, true).await?;
                    out.plans.push(p);
                }
                (true, StmtKind::Other) => self.run_statement(&stmt, max_rows, out).await?,
                (true, _) => {
                    let p = self.plan_of(&stmt, false).await?;
                    out.plans.push(p);
                    self.run_statement(&stmt, max_rows, out).await?;
                }
            }
        }
        Ok(())
    }

    /// One statement, its result tagged. In manual mode, with no
    /// transaction open, a `BEGIN` goes first (unless the statement can't
    /// run in a transaction block, see [`script::no_begin`]).
    ///
    /// The simple protocol sends cells as text and the client drops each
    /// column's type, so a query is also described (Parse/Describe, nothing
    /// runs) with [`script::DESCRIBE`] in front. It goes out in the same
    /// pipeline, before the statement: no extra round trip, the statement
    /// stays the session's last activity, and a describe that fails ends
    /// with its own Sync. Only outside a transaction block, where a failure
    /// can't abort the user's transaction.
    async fn run_statement(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let v = self.variant;
        let head = script::head(sql);
        let begin = !self.autocommit && self.tx == TxState::Idle && !script::no_begin(v, &head);
        let types = self.tx == TxState::Idle
            && v.has_pg_catalog()
            && matches!(script::verb(sql, &head).as_str(), "select" | "values" | "table");
        let first = out.results.len();
        let client = &self.client;
        let notices = &mut self.notices;
        let describe = async {
            if !types {
                return None;
            }
            match client.prepare(&format!("{}{sql}", script::DESCRIBE)).await {
                Ok(stmt) => Some(stmt),
                Err(e) => {
                    tracing::debug!("{v:?}: column types unavailable: {e}");
                    None
                }
            }
        };
        let run = async {
            if begin {
                if let Err(e) = client.batch_execute("BEGIN").await {
                    return (false, Err(script::statement_error(e, "")));
                }
            }
            (begin, run_script(client, notices, sql, Some(&head), max_rows, out).await)
        };
        // Polled in this order: the describe is sent first.
        let (stmt, (began, res)) = futures::join!(describe, run);
        if began {
            self.tx = TxState::Open;
        }
        self.tx = script::next_state(v, self.tx, &head, res.is_ok());
        self.drain_notices(out);
        if let (Ok(()), Some(stmt), [r]) = (&res, stmt, &mut out.results[first..]) {
            if stmt.columns().len() == r.columns.len() {
                for (c, t) in r.columns.iter_mut().zip(stmt.columns()) {
                    c.type_name = t.type_().name().to_string();
                }
            }
        }
        res
    }

    /// Notices still queued once the statement ended.
    fn drain_notices(&mut self, out: &mut QueryOutcome) {
        while let Ok(n) = self.notices.try_recv() {
            script::notice(out, &n);
        }
    }

    /// One statement's plan; `actual` runs it under EXPLAIN ANALYZE.
    async fn plan_of(&self, stmt: &str, actual: bool) -> Result<Plan> {
        match self.variant {
            Variant::Cockroach => {
                let q = if actual { format!("EXPLAIN ANALYZE {stmt}") } else { format!("EXPLAIN (VERBOSE) {stmt}") };
                Ok(plan::cockroach_text(stmt, &self.lines(&q).await?, actual))
            }
            Variant::Redshift => Ok(plan::pg_text(stmt, &self.lines(&format!("EXPLAIN {stmt}")).await?, false)),
            // The query as H2 will run it, with the access path in comments.
            Variant::H2 => Ok(plan::h2_text(stmt, &self.lines(&format!("EXPLAIN {stmt}")).await?)),
            // Their own operator trees, as text.
            Variant::RisingWave | Variant::Materialize | Variant::CrateDb => {
                Ok(plan::tree_text(stmt, &self.lines(&format!("EXPLAIN {stmt}")).await?))
            }
            _ => {
                let q = if actual {
                    format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {stmt}")
                } else {
                    format!("EXPLAIN (FORMAT JSON) {stmt}")
                };
                match self.lines(&q).await {
                    Ok(raw) => plan::pg_json(stmt, &raw, actual).map_err(Error::Query),
                    Err(e) => {
                        tracing::debug!("{:?}: EXPLAIN FORMAT JSON refused, trying text: {e}", self.variant);
                        let q = if actual { format!("EXPLAIN ANALYZE {stmt}") } else { format!("EXPLAIN {stmt}") };
                        Ok(plan::pg_text(stmt, &self.lines(&q).await?, actual))
                    }
                }
            }
        }
    }

    /// The first cell of every row, one per line (EXPLAIN's output).
    async fn lines(&self, sql: &str) -> Result<String> {
        let rows = self.text(sql).await?;
        Ok(rows.iter().filter_map(|r| r.get(0)).collect::<Vec<_>>().join("\n"))
    }
}

/// Objects from a text-protocol catalog query with `kind`, `name` and,
/// when `with_schema`, `schema` columns.
fn text_objects(rows: &[SimpleQueryRow], with_schema: bool) -> Vec<DbObject> {
    rows.iter()
        .filter_map(|r| {
            Some(DbObject {
                kind: cell(r, "kind")?,
                schema: if with_schema { cell(r, "schema") } else { None },
                name: cell(r, "name")?,
                parent: None,
            })
        })
        .collect()
}

#[async_trait]
impl Session for PgSession {
    async fn server_version(&mut self) -> Result<String> {
        match self.client.simple_query("SELECT version()").await {
            Ok(msgs) => Ok(first_cell(&msgs).unwrap_or_default()),
            Err(e) if self.variant == Variant::Denodo => {
                tracing::debug!("denodo: version() unavailable: {e}");
                Ok("Denodo".into())
            }
            Err(e) => Err(err(e)),
        }
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut queries =
            vec!["SELECT datname FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY 1".to_string()];
        match self.variant {
            Variant::Redshift => queries.push("SELECT datname FROM pg_database ORDER BY 1".into()),
            Variant::Denodo => queries.push("SELECT db_name FROM GET_DATABASES()".into()),
            _ => {}
        }
        match self.first_working(&queries).await {
            Ok(rows) => Ok(rows
                .iter()
                .filter_map(|r| r.get(0).map(str::to_string))
                .filter(|d| !matches!(d.as_str(), "template0" | "template1" | "padb_harvest"))
                .collect()),
            Err(e) if self.variant.info_schema_only() => {
                tracing::debug!("{:?}: no database list: {e}", self.variant);
                Ok(vec![self.database.clone()])
            }
            Err(e) => Err(e),
        }
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let mut out = match self.variant {
            Variant::Redshift => self.redshift_objects().await?,
            Variant::Denodo => self.denodo_objects().await?,
            Variant::H2 => {
                let mut out = text_objects(&self.text(&self.info_schema_relations()).await?, true);
                out.extend(self.compare_objects().await);
                out
            }
            _ => {
                let mut out = match self.pg_relations().await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!("{:?}: pg_class unavailable, using information_schema: {e}", self.variant);
                        text_objects(&self.text(&self.info_schema_relations()).await?, true)
                    }
                };
                let v = self.variant;
                if v.streaming() {
                    out.extend(self.streaming_objects().await);
                } else if v != Variant::CrateDb {
                    out.extend(self.pg_routines_and_triggers().await);
                }
                out.extend(self.compare_objects().await);
                out
            }
        };
        out.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
        out.dedup_by(|a, b| a.kind == b.kind && a.schema == b.schema && a.name == b.name);
        Ok(out)
    }

    /// Every schema of the session's database, the empty ones too:
    /// `pg_namespace` (Materialize scopes it to the database and its
    /// ambient `mz_*` schemas), `information_schema.schemata` where there
    /// is no `pg_catalog` worth asking (H2, CrateDB) or it fails. Denodo has
    /// no schemas.
    async fn list_schemas(&mut self) -> Result<Option<Vec<dbine_driver::SchemaInfo>>> {
        let v = self.variant;
        if v == Variant::Denodo {
            return Ok(None);
        }
        let mut queries = Vec::new();
        if !matches!(v, Variant::H2 | Variant::CrateDb) {
            queries.push("SELECT nspname FROM pg_namespace ORDER BY 1".to_string());
        }
        queries.push("SELECT schema_name FROM information_schema.schemata ORDER BY 1".to_string());
        let rows = self.first_working(&queries).await?;
        let mut out: Vec<dbine_driver::SchemaInfo> = rows
            .iter()
            .filter_map(|r| r.get(0).map(str::to_string))
            .map(|name| dbine_driver::SchemaInfo { system: system_schema(v, &name), name })
            .collect();
        out.dedup_by(|a, b| a.name == b.name);
        Ok(Some(out))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        match self.variant {
            Variant::Redshift => self.info_schema_columns("svv_columns", obj).await,
            Variant::Denodo => self.denodo_columns(obj).await,
            _ => match self.pg_columns(obj).await {
                Ok(c) => Ok(c),
                Err(e) => {
                    tracing::debug!("{:?}: pg_attribute unavailable, using information_schema: {e}", self.variant);
                    self.info_schema_columns("information_schema.columns", obj).await
                }
            },
        }
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        if crate::compare::KINDS.contains(&obj.kind.as_str()) {
            return self.compare_definition(obj).await;
        }
        match self.variant {
            Variant::Redshift => return self.redshift_definition(obj).await,
            Variant::Denodo => {
                let q = format!("DESC VQL VIEW {}", dbine_driver::sql::quote_ident(Quote::Double, &obj.name));
                return Ok(self.show_statement(&q, "result").await);
            }
            Variant::RisingWave | Variant::Materialize | Variant::CrateDb | Variant::H2 => return self.show_create(obj).await,
            Variant::Cockroach
                if matches!(obj.kind.as_str(), kinds::TABLE | kinds::VIEW | kinds::MATERIALIZED_VIEW) =>
            {
                let q = qualified_name(Quote::Double, obj.schema(), &obj.name);
                if let Some(d) = self.show_statement(&format!("SHOW CREATE {q}"), "create_statement").await {
                    return Ok(Some(d));
                }
            }
            _ => {}
        }
        match obj.kind.as_str() {
            kinds::VIEW | kinds::MATERIALIZED_VIEW => self.pg_view_definition(obj).await,
            kinds::PROCEDURE | kinds::FUNCTION => self.pg_routine_definition(obj).await,
            kinds::TRIGGER => self.pg_trigger_definition(obj).await,
            // No CREATE TABLE from PostgreSQL itself: the app builds one.
            _ => Ok(None),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    /// The editor hands it one statement at a time (`PerStatement`): it gets
    /// its tag, its column types and a positioned error, and in manual mode
    /// a `BEGIN` first when no transaction is open. Other callers may send
    /// several statements, which the server runs as one implicit
    /// transaction (Materialize's go one by one, see [`one_by_one`]).
    async fn execute(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        // Notices of earlier requests (catalog reads, the state probe).
        while self.notices.try_recv().is_ok() {}
        let units = dbine_driver::sql::split_script(sql, &script::DIALECT);
        if units.len() != 1 {
            let res = match one_by_one(self.variant, sql) {
                Some(stmts) => {
                    let mut res = Ok(());
                    for stmt in stmts {
                        res = run_script(&self.client, &mut self.notices, &stmt, None, max_rows, out).await;
                        if res.is_err() {
                            break;
                        }
                    }
                    res
                }
                None => run_script(&self.client, &mut self.notices, sql, None, max_rows, out).await,
            };
            self.drain_notices(out);
            if units.iter().any(|u| script::controls_transaction(&script::head(&u.text))) {
                self.tx_unsure = true;
            } else if res.is_err() {
                self.tx = script::next_state(self.variant, self.tx, &[], false);
            }
            return res;
        }
        let head = script::head(&units[0].text);
        let rolls_back = self.tx == TxState::Failed && script::is_commit(&head);
        let first = out.results.len();
        let res = self.run_statement(sql, max_rows, out).await;
        if res.is_ok() && rolls_back {
            if let Some(r) = out.results[first..].last_mut() {
                r.tag = Some("ROLLBACK".into());
            }
            out.warning("La transacción tenía un error: COMMIT la deshizo (ROLLBACK) y no se confirmó ningún cambio.");
        }
        res
    }

    /// Plans per statement (the script is split with `split_statements`).
    ///
    /// Estimated: `EXPLAIN (FORMAT JSON)` for each statement EXPLAIN takes
    /// (reads and DML); nothing runs, and other statements (DDL, SET…) are
    /// skipped with a message.
    ///
    /// Actual: each statement runs as with `execute`. A read then runs a
    /// second time under `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` to get
    /// its figures (so reads execute twice); a write gets its estimated plan
    /// before it runs, never EXPLAIN ANALYZE, which would apply it twice.
    ///
    /// Variants: CockroachDB answers with its `•` text tree (`EXPLAIN
    /// (VERBOSE)` / `EXPLAIN ANALYZE`); Redshift only with the estimated
    /// text plan; a variant that refuses `FORMAT JSON` falls back to the
    /// text format. Denodo has no EXPLAIN.
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if self.variant == Variant::Denodo {
            return Err(Error::Unsupported("Denodo no ofrece planes de ejecución (EXPLAIN) por esta conexión".into()));
        }
        while self.notices.try_recv().is_ok() {}
        let res = self.explain_script(sql, analyze, max_rows, out).await;
        self.drain_notices(out);
        res
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        if !self.variant.manual_transactions() {
            return Ok(None);
        }
        if self.variant.probes_transaction() && (self.tx != TxState::Idle || self.tx_unsure) {
            // CockroachDB says it; elsewhere a transaction block's now() is
            // its start, not this statement's.
            let cockroach = self.variant == Variant::Cockroach;
            let probe = if cockroach { "SHOW TRANSACTION STATUS" } else { "SELECT now() <> statement_timestamp()" };
            match self.client.simple_query(probe).await {
                Ok(msgs) => {
                    let cell = crate::catalog::first_cell(&msgs).unwrap_or_default();
                    self.tx = match cell.as_str() {
                        "Aborted" => TxState::Failed,
                        "t" | "true" => TxState::Open,
                        "NoTxn" | "f" | "false" => TxState::Idle,
                        _ if cockroach => TxState::Open,
                        _ => TxState::Idle,
                    };
                }
                Err(e) if e.code() == Some(&SqlState::IN_FAILED_SQL_TRANSACTION) => self.tx = TxState::Failed,
                Err(e) => tracing::debug!("{:?}: transaction state unknown: {e}", self.variant),
            }
            self.tx_unsure = false;
        }
        Ok(Some(self.tx))
    }

    /// Off: the next statement opens a transaction (psql's `AUTOCOMMIT
    /// off`). On: an open transaction is committed first, as JDBC does.
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if !on && !self.variant.manual_transactions() {
            return Err(Error::Unsupported(format!(
                "{} no admite transacciones manuales desde DBine",
                self.variant.info().name
            )));
        }
        if on && !self.autocommit {
            self.commit().await?;
        }
        self.autocommit = on;
        Ok(())
    }

    /// COMMIT; an aborted transaction can only roll back, and that's an
    /// error: nothing was committed.
    async fn commit(&mut self) -> Result<()> {
        let state = self.transaction_state().await?.unwrap_or(self.tx);
        if state == TxState::Idle {
            return Ok(());
        }
        let res = self.client.batch_execute("COMMIT").await;
        self.tx = TxState::Idle;
        while self.notices.try_recv().is_ok() {}
        res.map_err(crate::err)?;
        if state == TxState::Failed {
            return Err(Error::State(
                "La transacción tenía un error y se deshizo (ROLLBACK): no se confirmó ningún cambio.".into(),
            ));
        }
        Ok(())
    }

    async fn rollback(&mut self) -> Result<()> {
        if self.transaction_state().await?.unwrap_or(self.tx) == TxState::Idle {
            return Ok(());
        }
        let res = self.client.batch_execute("ROLLBACK").await;
        self.tx = TxState::Idle;
        while self.notices.try_recv().is_ok() {}
        res.map_err(crate::err)
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        match self.schema_of_database().await {
            Ok(t) => Ok(t),
            // Denodo may lack information_schema: its own column list per view.
            Err(e) if self.variant == Variant::Denodo => {
                tracing::debug!("denodo: information_schema unavailable: {e}");
                let mut out = Vec::new();
                for o in self.list_objects().await?.into_iter().filter(|o| o.kind == kinds::TABLE) {
                    let obj = ObjectRef { kind: o.kind.clone(), schema: None, name: o.name.clone() };
                    let cols = self.denodo_columns(&obj).await?;
                    let pk: Vec<String> = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
                    out.push(TableSchema {
                        kind: o.kind,
                        name: o.name,
                        primary_key: (!pk.is_empty()).then(|| KeyDef { name: None, columns: pk }),
                        columns: cols
                            .into_iter()
                            .map(|c| ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, ..Default::default() })
                            .collect(),
                        ..Default::default()
                    });
                }
                Ok(out)
            }
            Err(e) => Err(e),
        }
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        if self.variant == Variant::Denodo {
            return Err(Error::Unsupported("las bases de Denodo se crean desde Denodo".into()));
        }
        if self.variant == Variant::CrateDb {
            return Err(Error::Unsupported("CrateDB tiene una sola base por clúster: se organizan en esquemas".into()));
        }
        if self.variant == Variant::H2 {
            return Err(Error::Unsupported("H2 crea la base al conectarse a un nombre nuevo (con -ifNotExists)".into()));
        }
        self.create_db(name).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.variant == Variant::Denodo {
            return Err(Error::Unsupported("las bases de Denodo se borran desde Denodo".into()));
        }
        if self.variant == Variant::CrateDb {
            return Err(Error::Unsupported("CrateDB tiene una sola base por clúster: se organizan en esquemas".into()));
        }
        if self.variant == Variant::H2 {
            return Err(Error::Unsupported("H2 no borra bases por SQL: se borran sus archivos (o DROP ALL OBJECTS)".into()));
        }
        self.drop_db(name).await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        Ok(crate::monitor::snapshot(self).await)
    }

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        crate::blocking::blocking(self).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        crate::blocking::kill(self, id).await
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        crate::backup::history(self, database).await
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        crate::security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        crate::security::grants(self, principal).await
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = crate::profiler::start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = crate::profiler::poll(self, &mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        match self.profiler.take() {
            Some(state) => crate::profiler::stop(self, state).await,
            None => Ok(()),
        }
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        crate::transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        _columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        crate::transfer::bulk_load(self, spec, source, progress).await
    }

    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    async fn key_range(&mut self, table: &ObjectRef, column: &str) -> Result<Option<(i64, i64, u64)>> {
        crate::delta::key_range(self, table, column).await
    }

    async fn delta_summary(&mut self, spec: &dbine_driver::DeltaSpec) -> Result<Vec<dbine_driver::BucketSum>> {
        crate::delta::summary(self, spec).await
    }

    async fn delta_apply(
        &mut self,
        spec: &dbine_driver::DeltaSpec,
        buckets: &[i64],
        columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<dbine_driver::DeltaResult> {
        crate::delta::apply(self, spec, buckets, columns, source, progress).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        // Dropping the client doesn't stop the query on the server; a
        // cancel request does. It may be called off the runtime's threads.
        let token = self.client.cancel_token();
        let tls = self.tls.clone();
        let rt = tokio::runtime::Handle::try_current().ok()?;
        Some(Arc::new(move || {
            let (token, tls) = (token.clone(), tls.clone());
            rt.spawn(async move {
                if let Err(e) = token.cancel_query(tls).await {
                    tracing::debug!("postgres cancel failed: {e}");
                }
            });
        }))
    }

    /// One query per variant: role attributes, predefined roles, system
    /// privileges (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        crate::permissions::check(self, database).await
    }
}

/// Every first column of `rows`, one after the other; `None` when empty.
fn joined(rows: Vec<tokio_postgres::Row>) -> Result<Option<String>> {
    let parts: Vec<String> = rows.iter().filter_map(|r| r.try_get::<_, Option<String>>(0).ok().flatten()).collect();
    Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
}

/// Materialize runs a multi-statement query as one implicit transaction,
/// where DDL isn't allowed ("cannot be run inside a transaction block"):
/// its scripts go one statement at a time. `None`: send the script as is
/// (other engines, a single statement, or dollar quotes the splitter
/// doesn't know).
fn one_by_one(v: Variant, sql: &str) -> Option<Vec<String>> {
    if v != Variant::Materialize || sql.contains('$') {
        return None;
    }
    let stmts = split_statements(sql);
    (stmts.len() > 1).then_some(stmts)
}

/// Run `sql` as one simple query. Notices go to `out` as they arrive, in
/// order with the results. `head`: the words of a single statement, to tag
/// its result like psql ("INSERT 0 3", "CREATE TABLE"); without it (a
/// text of several statements) each completion counts affected rows.
async fn run_script(
    client: &Client,
    notices: &mut mpsc::UnboundedReceiver<DbError>,
    sql: &str,
    head: Option<&[String]>,
    max_rows: usize,
    out: &mut QueryOutcome,
) -> Result<()> {
    let verb = head.map(|h| script::verb(sql, h));
    let stream = client.simple_query_raw(sql).await.map_err(|e| script::statement_error(e, sql))?;
    pin_mut!(stream);
    // Whether the statement in progress returned a row description.
    let mut has_rows = false;
    loop {
        // A long statement's notices (RAISE in a loop) show while it runs.
        let msg = tokio::select! {
            biased;
            Some(n) = notices.recv() => {
                script::notice(out, &n);
                continue;
            }
            msg = stream.next() => msg,
        };
        let Some(msg) = msg else { break };
        while let Ok(n) = notices.try_recv() {
            script::notice(out, &n);
        }
        match msg.map_err(|e| script::statement_error(e, sql))? {
            SimpleQueryMessage::RowDescription(cols) => {
                has_rows = true;
                out.begin_result(
                    cols.iter().map(|c| ResultColumn { name: c.name().to_string(), type_name: String::new() }).collect(),
                );
            }
            SimpleQueryMessage::Row(row) => {
                let cells = (0..row.len()).map(|i| row.get(i).map_or(serde_json::Value::Null, Into::into)).collect();
                out.push_row(cells, max_rows);
            }
            SimpleQueryMessage::CommandComplete(n) => {
                match (head, verb.as_deref()) {
                    (Some(h), Some(v)) => {
                        let tag = script::tag(h, v, n);
                        if has_rows {
                            if let Some(r) = out.results.last_mut() {
                                r.tag = tag;
                            }
                        } else {
                            out.results.push(StatementResult { rows_affected: script::affected(v, n), tag, ..Default::default() });
                        }
                    }
                    _ if !has_rows => out.push_affected(n),
                    _ => {}
                }
                has_rows = false;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_materialize_scripts_go_one_by_one() {
        let script = "CREATE SCHEMA s;\nGRANT USAGE ON SCHEMA s TO a;";
        assert_eq!(one_by_one(Variant::Materialize, script).unwrap().len(), 2);
        assert!(one_by_one(Variant::Postgres, script).is_none());
        assert!(one_by_one(Variant::Materialize, "SELECT 1;").is_none());
        assert!(one_by_one(Variant::Materialize, "SELECT $$a;b$$; SELECT 2").is_none());
    }
}

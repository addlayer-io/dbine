//! Amazon Athena through the AWS SDK. Each statement is a query execution:
//! start it, poll until it finishes, then page through GetQueryResults up to
//! `max_rows`. Databases are those of the Glue (or another) data catalog.

#[path = "../../dynamodb/src/aws.rs"]
mod aws;
mod ddl;
#[path = "../../trino/src/literal.rs"]
mod literal;
mod monitor;
mod permissions;
mod plan;
mod profiler;
#[path = "../../trino/src/script.rs"]
mod script;
mod sync;
mod transfer;

use aws_sdk_athena::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_athena::types::{
    QueryExecutionContext, QueryExecutionState, QueryRuntimeStatistics, QueryStage, QueryStagePlanNode, ResultConfiguration, Row,
    StatementType, TableMetadata,
};
use aws_sdk_athena::Client;
use dbine_driver::sql::{qualified_name, quote_ident, select_top, Limit, Quote, ScriptDialect};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject,
    DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef,
    QueryOutcome, ResultColumn, Result, RowChange, Session, TableSchema,
};
use serde_json::Value as Json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_CATALOG: &str = "AwsDataCatalog";
const DEFAULT_WORKGROUP: &str = "primary";
/// GetQueryResults' page size ceiling.
const PAGE: i32 = 1000;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(AthenaDriver { info: info() })]
}

fn info() -> DriverInfo {
    let mut fields = aws::fields_by_auth();
    fields.extend([
        Field::new("workgroup", "Workgroup", FieldKind::Text).default_value(DEFAULT_WORKGROUP),
        Field::new("output_location", "Ubicación de resultados", FieldKind::Text)
            .placeholder("s3://mi-bucket/athena-results/")
            .help("Necesaria si el workgroup no define una.")
            .advanced(),
        Field::new("catalog", "Catálogo de datos", FieldKind::Text).default_value(DEFAULT_CATALOG),
        Field::database().placeholder("default"),
        Field::read_only(),
    ]);
    DriverInfo {
        id: "athena",
        name: "Amazon Athena",
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "trino",
        default_port: 0,
        fields,
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::views()],
    }
}

pub struct AthenaDriver {
    info: DriverInfo,
}

pub struct AthenaSession {
    client: Client,
    region: String,
    catalog: String,
    workgroup: String,
    output: Option<String>,
    database: Option<String>,
    running: Arc<Mutex<Option<String>>>,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

fn err<E, R>(e: SdkError<E, R>) -> Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug,
{
    let unreachable = matches!(e, SdkError::DispatchFailure(_) | SdkError::TimeoutError(_));
    aws::classify(e.code(), e.message(), unreachable, DisplayErrorContext(&e).to_string())
}

#[async_trait]
impl Driver for AthenaDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// Each statement is a query execution of its own; the database (the
    /// execution context) is kept by the session, so `USE` carries over.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::PerStatement
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { create_database: true, drop_database: true, foreign_keys: false, monitor: true, ..Default::default() }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    /// Multi-row `INSERT … VALUES` up to Athena's query length (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(table, parts)
    }

    /// DML runs on Trino: double quotes, typed date / timestamp literals.
    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Json>]) -> Result<String> {
        Ok(literal::insert_script(target.schema(), &target.name, columns, rows))
    }

    /// Same as INSERT: runs on Trino; UPDATE works on Iceberg tables.
    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        Ok(literal::update_script(target.schema(), &target.name, changes))
    }

    /// Same as UPDATE: runs on Trino; DELETE by key works on Iceberg tables
    /// (Hive-format tables reject it when it runs).
    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Json)>]) -> Result<String> {
        Ok(literal::delete_script(target.schema(), &target.name, keys))
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        tls_provider();
        let conf = aws::sdk_config(cfg).await?;
        let region = conf.region().map(|r| r.to_string()).unwrap_or_default();
        let client = Client::new(&conf);
        let workgroup = cfg.option("workgroup").unwrap_or(DEFAULT_WORKGROUP).trim().to_string();
        // Proves the endpoint and the credentials; a login without
        // athena:GetWorkGroup (a Query error) may still run queries.
        match tokio::time::timeout(Duration::from_secs(20), client.get_work_group().work_group(&workgroup).send())
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(err)
        {
            Ok(_) | Err(Error::Query(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(Box::new(AthenaSession {
            client,
            region,
            catalog: cfg.option("catalog").unwrap_or(DEFAULT_CATALOG).trim().to_string(),
            workgroup,
            output: cfg.option("output_location").map(|o| o.trim().to_string()),
            database: database.or(Some(cfg.database.as_str())).map(str::trim).filter(|d| !d.is_empty()).map(Into::into),
            running: Arc::new(Mutex::new(None)),
            profiler: None,
        }))
    }
}

/// Rows, columns and counts of one finished execution.
#[derive(Default)]
struct Execution {
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Option<String>>>,
    update_count: Option<i64>,
    more: bool,
    has_result_set: bool,
}

impl AthenaSession {
    fn set_running(&self, id: Option<String>) {
        if let Ok(mut r) = self.running.lock() {
            *r = id;
        }
    }

    async fn run(&self, sql: &str, max_rows: usize) -> Result<Execution> {
        self.run_id(sql, max_rows).await.map(|(_, ex)| ex)
    }

    /// Runs one statement; also gives its query execution id.
    async fn run_id(&self, sql: &str, max_rows: usize) -> Result<(String, Execution)> {
        let mut ctx = QueryExecutionContext::builder().catalog(&self.catalog);
        if let Some(d) = &self.database {
            ctx = ctx.database(d);
        }
        let mut req = self.client.start_query_execution().query_string(sql).query_execution_context(ctx.build()).work_group(&self.workgroup);
        if let Some(o) = &self.output {
            req = req.result_configuration(ResultConfiguration::builder().output_location(o).build());
        }
        let id = req.send().await.map_err(err)?.query_execution_id.unwrap_or_default();
        self.set_running(Some(id.clone()));
        let result = self.wait_and_fetch(&id, max_rows).await;
        self.set_running(None);
        result.map(|ex| (id, ex)).map_err(|e| match e {
            Error::Statement(se) => Error::Statement(Box::new(placed_error(*se, sql))),
            other => other,
        })
    }

    /// `EXPLAIN (FORMAT JSON)`: the statement is only planned. Athena runs
    /// it as a query execution of its own (no data scanned).
    async fn estimated_plan(&self, stmt: &str) -> Result<dbine_driver::Plan> {
        let ex = self.run(&format!("EXPLAIN (FORMAT JSON) {stmt}"), 100_000).await?;
        plan::plan_json(stmt, &plan::explain_text(&ex.rows)).map_err(Error::Query)
    }

    /// The measured plan of an execution that already finished.
    async fn runtime_plan(&self, id: &str, stmt: &str) -> Result<Option<dbine_driver::Plan>> {
        let stats = self.client.get_query_runtime_statistics().query_execution_id(id).send().await.map_err(err)?;
        let Some(rs) = stats.query_runtime_statistics() else { return Ok(None) };
        let mut props = Vec::new();
        if let Ok(q) = self.client.get_query_execution().query_execution_id(id).send().await {
            if let Some(st) = q.query_execution().and_then(|q| q.statistics()) {
                if let Some(b) = st.data_scanned_in_bytes() {
                    props.push(("Datos escaneados (bytes)".to_string(), b.to_string()));
                }
                if let Some(d) = st.dpu_count() {
                    props.push(("DPU".to_string(), d.to_string()));
                }
            }
        }
        props.push(("Id de ejecución".to_string(), id.to_string()));
        Ok(plan::runtime_plan(stmt, &runtime_json(rs), props))
    }

    fn push_execution(ex: &Execution, max_rows: usize, out: &mut QueryOutcome) {
        if !ex.has_result_set {
            out.push_affected(ex.update_count.unwrap_or(0).max(0) as u64);
            return;
        }
        out.begin_result(ex.columns.iter().map(|(n, t)| ResultColumn { name: n.clone(), type_name: t.clone() }).collect());
        for r in &ex.rows {
            let cells = ex.columns.iter().enumerate().map(|(i, (_, t))| cell(t, r.get(i).cloned().flatten())).collect();
            out.push_row(cells, max_rows);
        }
        if ex.more {
            if let Some(last) = out.results.last_mut() {
                last.truncated = true;
            }
        }
    }

    async fn wait_and_fetch(&self, id: &str, max_rows: usize) -> Result<Execution> {
        let mut delay = Duration::from_millis(200);
        let statement_type = loop {
            let out = self.client.get_query_execution().query_execution_id(id).send().await.map_err(err)?;
            let qe = out.query_execution();
            let status = qe.and_then(|q| q.status());
            match status.and_then(|s| s.state()) {
                Some(QueryExecutionState::Succeeded) => break qe.and_then(|q| q.statement_type()).cloned(),
                Some(QueryExecutionState::Failed) => {
                    let reason = status.and_then(|s| s.state_change_reason()).unwrap_or("la consulta falló");
                    let kind = status.and_then(|s| s.athena_error()).and_then(|a| a.error_type());
                    return Err(failed_error(reason, kind).into());
                }
                Some(QueryExecutionState::Cancelled) => return Err(Error::Cancelled),
                _ => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
            }
        };
        let mut ex = Execution::default();
        let mut token: Option<String> = None;
        let mut first = true;
        loop {
            let page = self
                .client
                .get_query_results()
                .query_execution_id(id)
                .max_results(PAGE)
                .set_next_token(token.take())
                .send()
                .await
                .map_err(err)?;
            ex.update_count = ex.update_count.or(page.update_count());
            if let Some(rs) = page.result_set() {
                if first {
                    ex.columns = rs
                        .result_set_metadata()
                        .map(|m| m.column_info().iter().map(|c| (c.name().to_string(), c.r#type().to_string())).collect())
                        .unwrap_or_default();
                    ex.has_result_set = !ex.columns.is_empty();
                }
                let mut rows = rs.rows().iter().map(row_values);
                // SELECT results repeat the column names as their first row.
                if first && statement_type == Some(StatementType::Dml) {
                    let mut peek = rows.clone();
                    if let Some(h) = peek.next() {
                        if is_header(&h, &ex.columns) {
                            rows.next();
                        }
                    }
                }
                for r in rows {
                    if ex.rows.len() < max_rows {
                        ex.rows.push(r);
                    } else {
                        ex.more = true;
                    }
                }
            }
            first = false;
            match page.next_token() {
                Some(t) if ex.rows.len() < max_rows => token = Some(t.to_string()),
                Some(_) => {
                    ex.more = true;
                    break;
                }
                None => break,
            }
        }
        Ok(ex)
    }

    /// One statement of a script. `USE db` (or `USE catalog.db`) has no
    /// query execution of its own in Athena: it changes the session's
    /// execution context, as the Trino CLI does, after checking the
    /// database exists.
    async fn run_statement(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if let Some((catalog, db)) = use_target(stmt) {
            let catalog = catalog.unwrap_or_else(|| self.catalog.clone());
            let previous = std::mem::replace(&mut self.catalog, catalog);
            let known = match self.list_databases().await {
                Ok(dbs) => dbs.iter().find(|d| d.eq_ignore_ascii_case(&db)).cloned(),
                Err(e) => {
                    self.catalog = previous;
                    return Err(e);
                }
            };
            let Some(db) = known else {
                let msg = format!("La base de datos «{db}» no existe en el catálogo «{}».", self.catalog);
                self.catalog = previous;
                return Err(dbine_driver::ScriptError::new(msg).with_code("SCHEMA_NOT_FOUND").at_offset(0).at_line(1).into());
            };
            self.database = Some(db.clone());
            out.info(format!("Base de datos: {}.{db}", self.catalog));
            out.push_affected(0);
            if let Some(last) = out.results.last_mut() {
                last.tag = Some("USE".into());
            }
            out.database = Some(db);
            return Ok(());
        }
        let ex = self.run(stmt, max_rows).await?;
        Self::push_execution(&ex, max_rows, out);
        Ok(())
    }

    /// Every table and view of the session's database, as the catalog
    /// describes them.
    async fn table_metadata(&self) -> Result<Vec<TableMetadata>> {
        let Some(db) = self.database.clone() else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let resp = self
                .client
                .list_table_metadata()
                .catalog_name(&self.catalog)
                .database_name(&db)
                .set_next_token(token.take())
                .send()
                .await
                .map_err(err)?;
            out.extend(resp.table_metadata_list().iter().cloned());
            match resp.next_token() {
                Some(t) => token = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    /// Text of a `SHOW CREATE …`, one line per row.
    async fn show_create(&self, what: &str, name: &str) -> Result<Option<String>> {
        let db = self.database.as_deref();
        let ex = self.run(&format!("SHOW CREATE {what} {}", qualified_name(Quote::Backtick, db, name)), 10_000).await?;
        let lines: Vec<String> = ex.rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        Ok((!lines.is_empty()).then(|| lines.join("\n")))
    }
}

/// A failed execution's reason with its code: the error name Athena puts
/// before the message (`COLUMN_NOT_FOUND: line 1:8: …`), else its numeric
/// error type.
fn failed_error(reason: &str, error_type: Option<i32>) -> dbine_driver::ScriptError {
    let name = reason.split_once(": ").map(|(n, _)| n).filter(|n| {
        !n.is_empty() && n.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && n.contains(|c: char| c.is_ascii_uppercase())
    });
    let se = dbine_driver::ScriptError::new(reason);
    match (name, error_type) {
        (Some(n), _) => se.with_code(n),
        (None, Some(t)) => se.with_code(t.to_string()),
        (None, None) => se,
    }
}

/// The error placed in `sql` (the text sent) by the `line L:C` its
/// message gives.
fn placed_error(se: dbine_driver::ScriptError, sql: &str) -> dbine_driver::ScriptError {
    match script::line_col(&se.message) {
        Some((l, c)) => script::placed(se, sql, Some(l), Some(c)),
        None => se,
    }
}

/// `USE db` / `USE catalog.db` (identifiers bare, "quoted" or `quoted`):
/// the catalog when given and the database.
fn use_target(stmt: &str) -> Option<(Option<String>, String)> {
    let text = dbine_driver::sql::strip_comments(stmt, &ScriptDialect::generic(), false);
    let text = text.trim().trim_end_matches(';').trim();
    if script::head(text).0 != "USE" {
        return None;
    }
    let (_, rest) = text.split_once(char::is_whitespace)?;
    let mut parts = Vec::new();
    let mut chars = rest.trim().chars().peekable();
    loop {
        let mut part = String::new();
        match chars.peek().copied() {
            Some(q @ ('"' | '`')) => {
                chars.next();
                loop {
                    match chars.next()? {
                        c if c == q && chars.peek() == Some(&q) => {
                            chars.next();
                            part.push(q);
                        }
                        c if c == q => break,
                        c => part.push(c),
                    }
                }
            }
            _ => {
                while let Some(&c) = chars.peek() {
                    if c == '.' || c.is_whitespace() {
                        break;
                    }
                    part.push(c);
                    chars.next();
                }
                part = part.to_ascii_lowercase();
            }
        }
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        match chars.next() {
            Some('.') => continue,
            None => break,
            Some(_) => return None,
        }
    }
    match parts.len() {
        1 => Some((None, parts.pop()?)),
        2 => {
            let db = parts.pop()?;
            Some((parts.pop(), db))
        }
        _ => None,
    }
}

fn row_values(r: &Row) -> Vec<Option<String>> {
    r.data().iter().map(|d| d.var_char_value().map(str::to_string)).collect()
}

fn is_header(row: &[Option<String>], cols: &[(String, String)]) -> bool {
    row.len() == cols.len() && row.iter().zip(cols).all(|(v, (name, _))| v.as_deref() == Some(name.as_str()))
}

#[async_trait]
impl Session for AthenaSession {
    async fn server_version(&mut self) -> Result<String> {
        Ok(format!("Amazon Athena ({}, workgroup {})", self.region, self.workgroup))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let resp = self
                .client
                .list_databases()
                .catalog_name(&self.catalog)
                .set_next_token(token.take())
                .send()
                .await
                .map_err(err)?;
            out.extend(resp.database_list().iter().map(|d| d.name().to_string()));
            match resp.next_token() {
                Some(t) => token = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        Ok(self
            .table_metadata()
            .await?
            .iter()
            .map(|t| {
                let kind = if t.table_type() == Some("VIRTUAL_VIEW") { kinds::VIEW } else { kinds::TABLE };
                DbObject { kind: kind.into(), schema: None, name: t.name().to_string(), parent: None }
            })
            .collect())
    }

    /// From the catalog's table metadata: columns (partition keys last),
    /// comments, table type (Iceberg or the files' format), location and
    /// partitioning.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let mut out: Vec<TableSchema> = self.table_metadata().await?.iter().filter_map(ddl::table_schema).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.snapshot().await
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = profiler::start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = profiler::poll(self, &mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        self.profiler = None;
        Ok(())
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.run(&format!("CREATE DATABASE {}", quote_ident(Quote::Backtick, name)), 1).await.map(|_| ())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.database.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(name)) {
            return Err(Error::Query(format!("No se puede borrar «{name}»: es la base de datos de esta sesión.")));
        }
        self.run(&format!("DROP DATABASE {} CASCADE", quote_ident(Quote::Backtick, name)), 1).await.map(|_| ())
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let db = obj
            .schema()
            .map(str::to_string)
            .or_else(|| self.database.clone())
            .ok_or_else(|| Error::Query("no hay una base de datos seleccionada".into()))?;
        let resp = self
            .client
            .get_table_metadata()
            .catalog_name(&self.catalog)
            .database_name(&db)
            .table_name(&obj.name)
            .send()
            .await
            .map_err(err)?;
        let Some(t) = resp.table_metadata() else { return Ok(Vec::new()) };
        Ok(t.columns()
            .iter()
            .chain(t.partition_keys())
            .map(|c| ColumnInfo {
                name: c.name().to_string(),
                data_type: c.r#type().unwrap_or("").to_string(),
                nullable: true,
                primary_key: false,
                auto_increment: false,
                default_value: None,
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let what = if obj.kind == kinds::VIEW { "VIEW" } else { "TABLE" };
        self.show_create(what, &obj.name).await
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        let db = obj.schema().map(str::to_string).or_else(|| self.database.clone());
        select_top(Quote::Double, Limit::Limit, db.as_deref(), &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for unit in script::units(text, &ScriptDialect::generic()) {
            self.run_statement(&unit.text, max_rows, out).await.map_err(|e| script::shift(e, &unit))?;
        }
        Ok(())
    }

    /// Estimated: `EXPLAIN (FORMAT JSON)`, nothing runs. Actual: each
    /// statement runs once and `GetQueryRuntimeStatistics` gives its stages
    /// with measured rows, bytes and time.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        use plan::StmtKind;
        for unit in script::units(text, &ScriptDialect::generic()) {
            let stmt = unit.text;
            let kind = plan::classify(&stmt);
            if !analyze {
                if kind == StmtKind::Other {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt)));
                } else {
                    out.plans.push(self.estimated_plan(&stmt).await?);
                }
                continue;
            }
            let (id, ex) = self.run_id(&stmt, max_rows).await?;
            Self::push_execution(&ex, max_rows, out);
            if kind == StmtKind::Other {
                continue;
            }
            match self.runtime_plan(&id, &stmt).await {
                Ok(Some(p)) => out.plans.push(p),
                Ok(None) => out.messages.push(format!("Athena no dio estadísticas de ejecución para: {}", plan::short(&stmt))),
                Err(e) => out.messages.push(format!("No se pudieron leer las estadísticas de ejecución: {e}")),
            }
        }
        Ok(())
    }

    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, columns, source, progress).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let (client, running) = (self.client.clone(), self.running.clone());
        let rt = tokio::runtime::Handle::try_current().ok()?;
        Some(Arc::new(move || {
            // Taken, not cloned: a transfer that keeps its finished query
            // here while it pages (or between INSERTs) sees it gone and stops.
            let Some(id) = running.lock().ok().and_then(|mut r| r.take()) else { return };
            let client = client.clone();
            rt.spawn(async move {
                if let Err(e) = client.stop_query_execution().query_execution_id(id).send().await {
                    tracing::debug!("athena cancel failed: {}", DisplayErrorContext(&e));
                }
            });
        }))
    }

    /// Only the profiler's call can be tested (see `permissions`).
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        Ok(permissions::check(self).await)
    }
}

/// `GetQueryRuntimeStatistics` back into the API's JSON shape, which the
/// plan parser (and its recorded fixtures) read.
fn runtime_json(rs: &QueryRuntimeStatistics) -> Json {
    let mut v = serde_json::json!({});
    if let Some(t) = rs.timeline() {
        v["Timeline"] = serde_json::json!({
            "QueryQueueTimeInMillis": t.query_queue_time_in_millis(),
            "QueryPlanningTimeInMillis": t.query_planning_time_in_millis(),
            "EngineExecutionTimeInMillis": t.engine_execution_time_in_millis(),
            "ServiceProcessingTimeInMillis": t.service_processing_time_in_millis(),
            "TotalExecutionTimeInMillis": t.total_execution_time_in_millis(),
        });
    }
    if let Some(r) = rs.rows() {
        v["Rows"] = serde_json::json!({
            "InputRows": r.input_rows(), "InputBytes": r.input_bytes(),
            "OutputBytes": r.output_bytes(), "OutputRows": r.output_rows(),
        });
    }
    if let Some(s) = rs.output_stage() {
        v["OutputStage"] = stage_json(s);
    }
    v
}

fn stage_json(s: &QueryStage) -> Json {
    fn node(n: &QueryStagePlanNode) -> Json {
        serde_json::json!({
            "Name": n.name(), "Identifier": n.identifier(),
            "Children": n.children().iter().map(node).collect::<Vec<_>>(),
            "RemoteSources": n.remote_sources(),
        })
    }
    serde_json::json!({
        "StageId": s.stage_id(), "State": s.state(), "OutputBytes": s.output_bytes(), "OutputRows": s.output_rows(),
        "InputBytes": s.input_bytes(), "InputRows": s.input_rows(), "ExecutionTime": s.execution_time(),
        "QueryStagePlan": s.query_stage_plan().map(node),
        "SubStages": s.sub_stages().iter().map(stage_json).collect::<Vec<_>>(),
    })
}

/// A text cell as JSON by its Athena (Trino) type.
fn cell(ty: &str, v: Option<String>) -> Json {
    let Some(v) = v else { return Json::Null };
    match ty.to_ascii_lowercase().as_str() {
        "boolean" => Json::Bool(v.eq_ignore_ascii_case("true")),
        "tinyint" | "smallint" | "integer" | "int" | "bigint" => v.parse::<i64>().map_or(Json::String(v), json_i64),
        "double" | "float" | "real" => match v.parse::<f64>() {
            Ok(f) if f.is_finite() => json_f64(f),
            _ => Json::String(v),
        },
        // "61 62 63": bytes as spaced hex.
        "varbinary" => {
            let bytes: Option<Vec<u8>> = v.split_whitespace().map(|b| u8::from_str_radix(b, 16).ok()).collect();
            bytes.map_or(Json::String(v), |b| json_bytes(&b))
        }
        _ => Json::String(v),
    }
}

/// Install the TLS crypto provider the AWS client uses, once per process
/// (a second install is a harmless no-op error).
pub(crate) fn tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A plain-HTTP client for unit tests: the default HTTPS one loads the OS
/// root certificates, and debug builds panic where none can be read.
#[cfg(test)]
pub(crate) fn plain_http_client() -> aws_sdk_athena::config::SharedHttpClient {
    aws_smithy_http_client::Builder::new().build_http()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cells_by_type() {
        assert_eq!(cell("boolean", Some("true".into())), json!(true));
        assert_eq!(cell("bigint", Some("9007199254740993".into())), json!("9007199254740993"));
        assert_eq!(cell("integer", Some("-4".into())), json!(-4));
        assert_eq!(cell("double", Some("2.5".into())), json!(2.5));
        assert_eq!(cell("double", Some("NaN".into())), json!("NaN"));
        assert_eq!(cell("decimal", Some("1.10".into())), json!("1.10"));
        assert_eq!(cell("varbinary", Some("ca fe".into())), json!("0xCAFE"));
        assert_eq!(cell("timestamp", Some("2024-01-31 13:45:00.000".into())), json!("2024-01-31 13:45:00.000"));
        assert_eq!(cell("varchar", None), Json::Null);
    }

    #[test]
    fn header_row_detection() {
        let cols = vec![("a".to_string(), "varchar".to_string()), ("b".to_string(), "integer".to_string())];
        assert!(is_header(&[Some("a".into()), Some("b".into())], &cols));
        assert!(!is_header(&[Some("a".into()), Some("1".into())], &cols));
    }

    #[tokio::test]
    async fn never_drops_its_own_database() {
        tls_provider();
        let conf = aws_sdk_athena::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_athena::config::Region::new("us-east-1"))
            .http_client(plain_http_client())
            .build();
        let mut s = AthenaSession {
            client: Client::from_conf(conf),
            region: "us-east-1".into(),
            catalog: DEFAULT_CATALOG.into(),
            workgroup: DEFAULT_WORKGROUP.into(),
            output: None,
            database: Some("ventas".into()),
            running: Arc::new(Mutex::new(None)),
            profiler: None,
        };
        assert!(matches!(s.drop_database("Ventas").await, Err(Error::Query(m)) if m.contains("sesión")));
        let d = AthenaDriver { info: info() };
        assert!(d.capabilities().create_database && d.capabilities().drop_database && !d.capabilities().foreign_keys && d.capabilities().monitor);
        let target = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "t".into() };
        let ins = d.insert_script(&target, &["d".into()], &[vec![serde_json::json!("2024-01-31")]]).unwrap();
        assert_eq!(ins, "INSERT INTO \"t\" (\"d\") VALUES\n  (DATE '2024-01-31');");
    }

    #[test]
    fn use_and_errors() {
        assert_eq!(use_target("USE ventas;"), Some((None, "ventas".into())));
        assert_eq!(use_target("-- x\nuse AwsDataCatalog.Ventas"), Some((Some("awsdatacatalog".into()), "ventas".into())));
        assert_eq!(use_target("USE \"Mi Cat\".`db`"), Some((Some("Mi Cat".into()), "db".into())));
        assert_eq!(use_target("USE a.b.c"), None);
        assert_eq!(use_target("SELECT 1"), None);
        assert_eq!(use_target("USE"), None);
        let e = failed_error("COLUMN_NOT_FOUND: line 2:8: Column 'x' cannot be resolved", Some(1006));
        assert_eq!(e.code.as_deref(), Some("COLUMN_NOT_FOUND"));
        let e = placed_error(e, "select 1,\nselect x");
        assert_eq!((e.line, e.offset), (Some(2), Some(17)));
        let e = failed_error("Insufficient permissions to execute the query.", Some(1301));
        assert_eq!(e.code.as_deref(), Some("1301"));
        assert_eq!(placed_error(e, "select 1").line, None);
        assert_eq!(failed_error("x", None).code, None);
    }

    #[test]
    fn form_and_browse() {
        let i = info();
        assert_eq!(i.id, "athena");
        assert!(i.fields.iter().any(|f| f.key == "secret_access_key" && f.secret));
        assert_eq!(
            select_top(Quote::Double, Limit::Limit, Some("db"), "t", 5),
            "SELECT *\nFROM \"db\".\"t\"\nLIMIT 5"
        );
    }
}

//! Google BigQuery over its REST API (v2). A script runs as one query job
//! (`jobs.query`, then `jobs.getQueryResults` until it finishes and until
//! `max_rows` rows are in); BigQuery runs multi-statement scripts itself and
//! returns the last statement's result. Datasets are the databases.

mod blocks;
mod create_db;
mod ddl;
mod gcp;
mod index_usage;
mod indexes;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod script;
mod security;
mod sync;
mod backup;
mod transfer;

use dbine_driver::sql::{quote_ident, select_top, split_statements, Limit, Quote};
use ddl::ident;
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo,
    ObjectRef, QueryOutcome, ResultColumn, Result, RowChange, Session, TableSchema,
};
use serde_json::{json, Map, Value as Json};
use std::sync::{Arc, Mutex};

const API: &str = "https://bigquery.googleapis.com";
/// How long each request waits for the job before polling again.
const WAIT_MS: u64 = 10_000;
/// Rows per results page.
const PAGE: usize = 10_000;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(BigQueryDriver { info: info() })]
}

fn info() -> DriverInfo {
    let mut fields = gcp::fields();
    fields.push(
        Field::new("database", "Dataset predeterminado", FieldKind::Text)
            .placeholder("(ninguno)")
            .help("Las tablas sin calificar de las consultas se buscan en este dataset."),
    );
    fields.push(
        Field::new("location", "Ubicación", FieldKind::Text)
            .placeholder("US")
            .help("Región de los jobs (US, EU, southamerica-east1…). Vacía = la del dataset."),
    );
    fields.push(gcp::endpoint_field("http://localhost:9050"));
    fields.push(Field::read_only());
    DriverInfo {
        id: "bigquery",
        name: "Google BigQuery",
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "bigquery",
        default_port: 0,
        fields,
        databases_label: "Datasets",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::materialized_views(),
            ObjectKindInfo::functions(),
            ObjectKindInfo::procedures(),
        ],
    }
}

pub struct BigQueryDriver {
    info: DriverInfo,
}

/// A job in flight, for cancelling.
#[derive(Clone, Debug)]
struct JobRef {
    id: String,
    location: Option<String>,
}

#[derive(Clone)]
struct Api {
    http: reqwest::Client,
    tokens: gcp::Tokens,
    base: String,
    project: String,
    location: Option<String>,
    emulator: bool,
}

impl Api {
    /// `…/bigquery/v2/projects/<project>/<segments…>`, each segment escaped.
    fn url(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.base).map_err(|e| Error::Connect(format!("endpoint inválido: {e}")))?;
        url.path_segments_mut()
            .map_err(|_| Error::Connect("endpoint inválido".into()))?
            .pop_if_empty()
            .extend(["bigquery", "v2", "projects", self.project.as_str()])
            .extend(segments);
        Ok(url)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Json> {
        let req = match self.tokens.bearer().await? {
            Some(b) => req.header("Authorization", b),
            None => req,
        };
        let resp = req.send().await.map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
        if !status.is_success() {
            return Err(gcp::api_error(status.as_u16(), &body));
        }
        if body.is_empty() {
            return Ok(Json::Null);
        }
        Ok(serde_json::from_str(&body)?)
    }

    async fn get(&self, segments: &[&str], query: &[(&str, String)]) -> Result<Json> {
        let url = self.url(segments)?;
        self.send(self.http.get(url).query(query)).await
    }

    async fn post(&self, segments: &[&str], query: &[(&str, String)], body: &Json) -> Result<Json> {
        let url = self.url(segments)?;
        self.send(self.http.post(url).query(query).json(body)).await
    }

    /// Every item of a paged list (`items_key`), following `nextPageToken`.
    async fn list_all(&self, segments: &[&str], items_key: &str) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("maxResults", "1000".to_string())];
            if let Some(t) = token.take() {
                q.push(("pageToken", t));
            }
            let resp = self.get(segments, &q).await?;
            if let Some(items) = resp.get(items_key).and_then(Json::as_array) {
                out.extend(items.iter().cloned());
            }
            match resp.get("nextPageToken").and_then(Json::as_str) {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn cancel(&self, job: &JobRef) {
        let mut q = Vec::new();
        if let Some(l) = &job.location {
            q.push(("location", l.clone()));
        }
        if let Err(e) = self.post(&["jobs", &job.id, "cancel"], &q, &Json::Null).await {
            tracing::debug!("bigquery cancel failed: {e}");
        }
    }
}

/// Whether a job runs in a BigQuery session.
#[derive(Clone, Copy)]
enum InSession<'a> {
    No,
    /// Opens one (its id comes back in `sessionInfo`).
    Create,
    Use(&'a str),
}

/// Child jobs of a script fetched one by one, at most.
const MAX_CHILDREN: usize = 50;

/// A finished query: its schema, the rows kept and what the job reported.
#[derive(Debug, Default)]
struct QueryResult {
    fields: Vec<Json>,
    has_schema: bool,
    rows: Vec<Json>,
    total_rows: Option<u64>,
    dml_rows: Option<u64>,
    more: bool,
}

pub struct BigQuerySession {
    api: Api,
    dataset: Option<String>,
    /// The BigQuery session editor runs share once one needed it (temp
    /// tables, variables, transactions last between runs).
    session_id: Option<String>,
    job: Arc<Mutex<Option<JobRef>>>,
    /// The monitor's storage figures, refreshed every few minutes.
    storage: monitor::StorageCache,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

#[async_trait]
impl Driver for BigQueryDriver {
    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<Field> {
        create_db::fields()
    }

    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        create_db::script(name, options)
    }

    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        script::dialect()
    }

    /// Scripting blocks (`BEGIN … END`, `LOOP`, `IF`…) go whole.
    fn split_script(&self, text: &str) -> Vec<dbine_driver::ScriptStatement> {
        script::units(text)
    }

    /// One job with the whole script: BigQuery scripting runs it (variables
    /// and blocks span its statements), as the console and `bq` do.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::Whole
    }

    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: true,
            monitor: true,
            processes: true,
            cancel_query: true,
            ..Default::default()
        }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Load jobs from newline-delimited JSON (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    /// Search and vector indexes, with the jobs that used them (see
    /// `index_usage`), and the foreign keys.
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    /// IAM roles on datasets, tables and views, through DCL.
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// A dataset's tables as table snapshots, all at the same moment.
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(ddl::table_ddl(table, parts))
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Json>]) -> Result<String> {
        Ok(ddl::insert_script(target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        Ok(ddl::update_script(target.schema(), &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        Ok(ddl::delete_script(target.schema(), &target.name, keys))
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let project = cfg
            .option("project_id")
            .map(|p| p.trim().to_string())
            .ok_or_else(|| Error::Connect("falta el ID del proyecto".into()))?;
        let http = gcp::http_client()?;
        let tokens = gcp::Tokens::from_config(cfg, http.clone())?;
        let base = cfg.option("endpoint_url").unwrap_or(API).trim_end_matches('/').to_string();
        let api = Api {
            http,
            tokens,
            emulator: cfg.option("endpoint_url").is_some(),
            base,
            project,
            location: cfg.option("location").map(|l| l.trim().to_string()),
        };
        // Proves the endpoint, the credentials and the project.
        tokio::time::timeout(std::time::Duration::from_secs(20), api.get(&["datasets"], &[("maxResults", "1".into())]))
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(|e| match e {
                Error::Query(m) => Error::Connect(m),
                other => other,
            })?;
        let dataset = database
            .or(Some(cfg.database.as_str()))
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        Ok(Box::new(BigQuerySession {
            api,
            dataset,
            session_id: None,
            job: Arc::new(Mutex::new(None)),
            storage: Default::default(),
            profiler: None,
        }))
    }
}

impl BigQuerySession {
    fn set_job(&self, job: Option<JobRef>) {
        if let Ok(mut j) = self.job.lock() {
            *j = job;
        }
    }

    /// Runs `sql` as one job and keeps up to `max_rows` rows.
    async fn query(&self, sql: &str, max_rows: usize, params: Option<Json>) -> Result<QueryResult> {
        self.query_job(sql, max_rows, params).await.map(|(_, r)| r)
    }

    /// Runs `sql` as one job; also gives the job.
    async fn query_job(&self, sql: &str, max_rows: usize, params: Option<Json>) -> Result<(JobRef, QueryResult)> {
        self.query_job_in(sql, max_rows, params, InSession::No).await.map(|(j, r, _)| (j, r))
    }

    /// Runs `sql` as one job, in a BigQuery session or not; also gives the
    /// job and the session it ran in.
    async fn query_job_in(
        &self,
        sql: &str,
        max_rows: usize,
        params: Option<Json>,
        session: InSession<'_>,
    ) -> Result<(JobRef, QueryResult, Option<String>)> {
        let page = max_rows.clamp(1, PAGE);
        let mut body = json!({
            "query": sql,
            "useLegacySql": false,
            "timeoutMs": WAIT_MS,
            "maxResults": page,
            "formatOptions": { "useInt64Timestamp": true },
        });
        if let Some(d) = &self.dataset {
            body["defaultDataset"] = json!({ "projectId": self.api.project, "datasetId": d });
        }
        if let Some(l) = &self.api.location {
            body["location"] = json!(l);
        }
        if let Some(p) = params {
            body["parameterMode"] = json!("NAMED");
            body["queryParameters"] = p;
        }
        match session {
            InSession::No => {}
            InSession::Create => body["createSession"] = json!(true),
            InSession::Use(id) => body["connectionProperties"] = json!([{ "key": "session_id", "value": id }]),
        }
        let mut resp = self.api.post(&["queries"], &[], &body).await?;
        let session_id = resp.pointer("/sessionInfo/sessionId").and_then(Json::as_str).map(str::to_string);
        let job = JobRef {
            id: resp.pointer("/jobReference/jobId").and_then(Json::as_str).unwrap_or_default().to_string(),
            location: resp
                .pointer("/jobReference/location")
                .and_then(Json::as_str)
                .map(str::to_string)
                .or_else(|| self.api.location.clone()),
        };
        self.set_job(Some(job.clone()));
        let result = self.collect(&job, &mut resp, max_rows, page).await;
        self.set_job(None);
        result.map(|r| (job, r, session_id))
    }

    /// An editor run: in the tab's BigQuery session when it has one or the
    /// script needs one (`want`).
    async fn run_editor(&mut self, text: &str, max_rows: usize, want: bool) -> Result<(JobRef, QueryResult)> {
        let id = self.session_id.clone();
        let mode = match (&id, want) {
            (Some(id), _) => InSession::Use(id),
            (None, true) => InSession::Create,
            (None, false) => InSession::No,
        };
        let (job, r, sid) = self.query_job_in(text, max_rows, None, mode).await?;
        if self.session_id.is_none() {
            self.session_id = sid;
        }
        Ok((job, r))
    }

    /// One result per statement of a script, as the console lists them:
    /// its child jobs in order, each with its statement type as the tag.
    /// `false` (nothing pushed) when there are none or too many to fetch
    /// (a long loop): the script's last result is shown then.
    async fn push_children(&self, job: &JobRef, max_rows: usize, out: &mut QueryOutcome) -> Result<bool> {
        let mut q = vec![("parentJobId", job.id.clone())];
        if let Some(l) = &job.location {
            q.push(("location", l.clone()));
        }
        let list = self.api.get(&["jobs"], &q).await?;
        let mut kids: Vec<(f64, String, Option<String>)> = list
            .get("jobs")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let id = c.pointer("/jobReference/jobId").and_then(Json::as_str)?.to_string();
                let at = c.pointer("/statistics/creationTime").and_then(|v| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0);
                let loc = c.pointer("/jobReference/location").and_then(Json::as_str).map(str::to_string);
                Some((at, id, loc))
            })
            .collect();
        if kids.is_empty() || kids.len() > MAX_CHILDREN || list.get("nextPageToken").and_then(Json::as_str).is_some_and(|t| !t.is_empty()) {
            if kids.len() > MAX_CHILDREN {
                out.info(format!("El script ejecutó más de {MAX_CHILDREN} sentencias: se muestra el resultado de la última."));
            }
            return Ok(false);
        }
        kids.sort_by(|a, b| a.0.total_cmp(&b.0));
        let page = max_rows.clamp(1, PAGE);
        for (_, id, loc) in kids {
            let c = self.get_job(&id, loc.as_deref()).await?;
            let st = c.pointer("/statistics/query/statementType").and_then(Json::as_str).unwrap_or("").to_string();
            if let Some(m) = c.pointer("/status/errorResult/message").and_then(Json::as_str) {
                // Failed and handled by the script (EXCEPTION).
                out.warning(format!("{st}: {m}"));
                continue;
            }
            let child = JobRef { id: id.clone(), location: loc.clone().or_else(|| job.location.clone()) };
            if st == "SELECT" {
                let mut rq = vec![
                    ("maxResults", page.to_string()),
                    ("formatOptions.useInt64Timestamp", "true".to_string()),
                ];
                if let Some(l) = &child.location {
                    rq.push(("location", l.clone()));
                }
                let mut resp = self.api.get(&["queries", &id], &rq).await?;
                let r = self.collect(&child, &mut resp, max_rows, page).await?;
                push_result(&r, max_rows, out);
            } else {
                let n = c.pointer("/statistics/query/numDmlAffectedRows").and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64()));
                out.push_affected(n.unwrap_or(0));
            }
            if let Some(last) = out.results.last_mut() {
                last.tag = (!st.is_empty()).then(|| st.clone());
            }
        }
        Ok(true)
    }

    /// A dry run of one statement (`jobs.insert` with `dryRun`): validated
    /// and priced, not run, not billed.
    async fn dry_run(&self, sql: &str) -> Result<Json> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut job_ref = json!({ "projectId": self.api.project, "jobId": format!("dbine_dry_{nanos}_{seq}") });
        if let Some(l) = &self.api.location {
            job_ref["location"] = json!(l);
        }
        let mut query = json!({ "query": sql, "useLegacySql": false });
        if let Some(d) = &self.dataset {
            query["defaultDataset"] = json!({ "projectId": self.api.project, "datasetId": d });
        }
        let body = json!({ "jobReference": job_ref, "configuration": { "dryRun": true, "query": query } });
        self.api.post(&["jobs"], &[], &body).await
    }

    async fn get_job(&self, id: &str, location: Option<&str>) -> Result<Json> {
        let q: Vec<(&str, String)> = location.map(|l| ("location", l.to_string())).into_iter().collect();
        self.api.get(&["jobs", id], &q).await
    }

    /// The estimated plan of one statement, `None` for DDL.
    async fn estimated_plan(&self, stmt: &str) -> Result<Option<dbine_driver::Plan>> {
        if plan::no_plan_text(stmt) {
            return Ok(None);
        }
        let job = self.dry_run(stmt).await?;
        let st = job.pointer("/statistics/query/statementType").and_then(Json::as_str).unwrap_or("");
        if plan::is_ddl(st) {
            return Ok(None);
        }
        let mut tables = Vec::new();
        for t in job.pointer("/statistics/query/referencedTables").and_then(Json::as_array).into_iter().flatten() {
            let name = plan::table_name(t);
            let meta = match (t.get("projectId").and_then(Json::as_str), t.get("datasetId").and_then(Json::as_str), t.get("tableId").and_then(Json::as_str)) {
                (Some(p), Some(d), Some(id)) if p == self.api.project => self.api.get(&["datasets", d, "tables", id], &[]).await.ok(),
                _ => None,
            };
            tables.push((name, meta));
        }
        Ok(Some(plan::dry_run(stmt, &job, &tables)))
    }

    /// The measured plans of a finished job: its own, or one per child job
    /// when it ran a multi-statement script.
    async fn actual_plans(&self, job: &JobRef, text: &str, out: &mut QueryOutcome) -> Result<()> {
        let j = self.get_job(&job.id, job.location.as_deref()).await?;
        let children = j.pointer("/statistics/numChildJobs").and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or(v.as_u64()));
        let is_script = j.pointer("/statistics/query/statementType").and_then(Json::as_str) == Some("SCRIPT");
        if children.unwrap_or(0) == 0 && !is_script {
            out.plans.push(plan::job_plan(text.trim(), &j));
            return Ok(());
        }
        let mut q = vec![("parentJobId", job.id.clone())];
        if let Some(l) = &job.location {
            q.push(("location", l.clone()));
        }
        let list = self.api.get(&["jobs"], &q).await?;
        let mut kids: Vec<(f64, String, Option<String>)> = list
            .get("jobs")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let id = c.pointer("/jobReference/jobId").and_then(Json::as_str)?.to_string();
                let at = c.pointer("/statistics/creationTime").and_then(|v| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0);
                let loc = c.pointer("/jobReference/location").and_then(Json::as_str).map(str::to_string);
                Some((at, id, loc))
            })
            .collect();
        kids.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, id, loc) in kids {
            let c = self.get_job(&id, loc.as_deref()).await?;
            let st = c.pointer("/statistics/query/statementType").and_then(Json::as_str).unwrap_or("");
            if plan::is_ddl(st) {
                continue;
            }
            let stmt = c.pointer("/configuration/query/query").and_then(Json::as_str).unwrap_or("").to_string();
            out.plans.push(plan::job_plan(&stmt, &c));
        }
        Ok(())
    }

    async fn collect(&self, job: &JobRef, resp: &mut Json, max_rows: usize, page: usize) -> Result<QueryResult> {
        let results = |token: Option<String>| {
            let mut q = vec![
                ("timeoutMs", WAIT_MS.to_string()),
                ("maxResults", page.to_string()),
                ("formatOptions.useInt64Timestamp", "true".to_string()),
            ];
            if let Some(l) = &job.location {
                q.push(("location", l.clone()));
            }
            if let Some(t) = token {
                q.push(("pageToken", t));
            }
            q
        };
        while resp.get("jobComplete").and_then(Json::as_bool) == Some(false) {
            *resp = self.api.get(&["queries", &job.id], &results(None)).await?;
        }
        let mut out = QueryResult {
            fields: resp.pointer("/schema/fields").and_then(Json::as_array).cloned().unwrap_or_default(),
            has_schema: resp.get("schema").is_some(),
            total_rows: u64_field(resp, "totalRows"),
            dml_rows: u64_field(resp, "numDmlAffectedRows"),
            ..Default::default()
        };
        loop {
            for row in resp.get("rows").and_then(Json::as_array).into_iter().flatten() {
                if out.rows.len() < max_rows {
                    out.rows.push(row.clone());
                } else {
                    out.more = true;
                }
            }
            match resp.get("pageToken").and_then(Json::as_str) {
                Some(t) if !t.is_empty() && out.rows.len() < max_rows => {
                    *resp = self.api.get(&["queries", &job.id], &results(Some(t.to_string()))).await?;
                }
                Some(t) if !t.is_empty() => {
                    out.more = true;
                    break;
                }
                _ => break,
            }
        }
        Ok(out)
    }

    fn dataset(&self, obj: &ObjectRef) -> Result<String> {
        obj.schema()
            .map(str::to_string)
            .or_else(|| self.dataset.clone())
            .ok_or_else(|| Error::Query("no hay un dataset seleccionado".into()))
    }

    /// Rows of a catalog query by lowercase column name (NULLs left out).
    async fn named_rows(&self, sql: &str) -> Result<Vec<ddl::Row>> {
        let r = self.query(sql, 1_000_000, None).await?;
        let names: Vec<String> = r.fields.iter().map(|f| str_of(f, "name").to_ascii_lowercase()).collect();
        Ok(r.rows
            .iter()
            .map(|row| {
                let cells = row.get("f").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
                names
                    .iter()
                    .zip(cells)
                    .filter_map(|(n, c)| Some((n.clone(), c.get("v").and_then(Json::as_str)?.to_string())))
                    .collect()
            })
            .collect())
    }

    /// `ddl` from the dataset's INFORMATION_SCHEMA view, if it has one.
    async fn ddl(&self, dataset: &str, view: &str, name_col: &str, name: &str) -> Option<String> {
        let sql = format!(
            "SELECT ddl FROM {}.{}.INFORMATION_SCHEMA.{view} WHERE {name_col} = @name",
            quote_ident(Quote::Backtick, &self.api.project),
            quote_ident(Quote::Backtick, dataset)
        );
        let params = json!([{ "name": "name", "parameterType": { "type": "STRING" }, "parameterValue": { "value": name } }]);
        let r = self.query(&sql, 1, Some(params)).await.ok()?;
        r.rows.first().and_then(|row| row.pointer("/f/0/v")).and_then(Json::as_str).map(str::to_string)
    }
}

/// A finished query's rows (or affected count) into `out`.
fn push_result(r: &QueryResult, max_rows: usize, out: &mut QueryOutcome) {
    if !r.has_schema || r.fields.is_empty() {
        out.push_affected(r.dml_rows.unwrap_or(0));
        return;
    }
    out.begin_result(
        r.fields.iter().map(|f| ResultColumn { name: str_of(f, "name").to_string(), type_name: type_label(f) }).collect(),
    );
    for row in &r.rows {
        let cells = row.get("f").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
        out.push_row(
            r.fields
                .iter()
                .enumerate()
                .map(|(i, f)| cell(cells.get(i).and_then(|c| c.get("v")).unwrap_or(&Json::Null), f))
                .collect(),
            max_rows,
        );
    }
    if let Some(last) = out.results.last_mut() {
        if let Some(total) = r.total_rows {
            last.total_rows = last.total_rows.max(total);
        }
        last.truncated |= r.more || last.total_rows > last.rows.len() as u64;
    }
}

fn u64_field(v: &Json, key: &str) -> Option<u64> {
    match v.get(key)? {
        Json::String(s) => s.parse().ok(),
        n => n.as_u64(),
    }
}

#[async_trait]
impl Session for BigQuerySession {
    async fn server_version(&mut self) -> Result<String> {
        Ok(if self.api.emulator {
            format!("BigQuery (emulador, proyecto {})", self.api.project)
        } else {
            format!("Google BigQuery (proyecto {})", self.api.project)
        })
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let items = self.api.list_all(&["datasets"], "datasets").await?;
        let mut out: Vec<String> = items
            .iter()
            .filter_map(|d| d.pointer("/datasetReference/datasetId").and_then(Json::as_str).map(str::to_string))
            .collect();
        out.sort();
        Ok(out)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let Some(ds) = self.dataset.clone() else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for t in self.api.list_all(&["datasets", &ds, "tables"], "tables").await? {
            let Some(name) = t.pointer("/tableReference/tableId").and_then(Json::as_str) else { continue };
            let kind = match t.get("type").and_then(Json::as_str) {
                Some("VIEW") => kinds::VIEW,
                Some("MATERIALIZED_VIEW") => kinds::MATERIALIZED_VIEW,
                _ => kinds::TABLE,
            };
            out.push(DbObject { kind: kind.into(), schema: None, name: name.into(), parent: None });
        }
        // Emulators may not implement routines; they're optional.
        if let Ok(routines) = self.api.list_all(&["datasets", &ds, "routines"], "routines").await {
            for r in routines {
                let Some(name) = r.pointer("/routineReference/routineId").and_then(Json::as_str) else { continue };
                let kind =
                    if r.get("routineType").and_then(Json::as_str) == Some("PROCEDURE") { kinds::PROCEDURE } else { kinds::FUNCTION };
                out.push(DbObject { kind: kind.into(), schema: None, name: name.into(), parent: None });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let ds = self.dataset(obj)?;
        let t = self.api.get(&["datasets", &ds, "tables", &obj.name], &[]).await?;
        let pk: Vec<String> = t
            .pointer("/tableConstraints/primaryKey/columns")
            .and_then(Json::as_array)
            .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        flatten_fields(t.pointer("/schema/fields").and_then(Json::as_array).map_or(&[][..], Vec::as_slice), "", &mut out);
        for c in &mut out {
            c.primary_key = pk.contains(&c.name);
        }
        Ok(out)
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let ds = self.dataset(obj)?;
        match obj.kind.as_str() {
            kinds::FUNCTION | kinds::PROCEDURE => {
                if let Some(d) = self.ddl(&ds, "ROUTINES", "routine_name", &obj.name).await {
                    return Ok(Some(d));
                }
                let r = self.api.get(&["datasets", &ds, "routines", &obj.name], &[]).await?;
                Ok(r.get("definitionBody").and_then(Json::as_str).map(str::to_string))
            }
            _ => {
                if let Some(d) = self.ddl(&ds, "TABLES", "table_name", &obj.name).await {
                    return Ok(Some(d));
                }
                let t = self.api.get(&["datasets", &ds, "tables", &obj.name], &[]).await?;
                let name = format!("{}.{}", quote_ident(Quote::Backtick, &ds), quote_ident(Quote::Backtick, &obj.name));
                Ok(if let Some(q) = t.pointer("/view/query").and_then(Json::as_str) {
                    Some(format!("CREATE VIEW {name} AS\n{q}"))
                } else {
                    t.pointer("/materializedView/query")
                        .and_then(Json::as_str)
                        .map(|q| format!("CREATE MATERIALIZED VIEW {name} AS\n{q}"))
                })
            }
        }
    }

    /// The dataset's INFORMATION_SCHEMA, all tables at once. Keys and
    /// descriptions are optional: emulators don't have those views.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let Some(ds) = self.dataset.clone() else { return Ok(Vec::new()) };
        let is = format!("{}.{}.INFORMATION_SCHEMA", ident(&self.api.project), ident(&ds));
        let this = &*self;
        let q = |sql: String| async move { this.named_rows(&sql).await };
        let (tables, columns, options, descriptions, keys, refs) = tokio::join!(
            q(format!("SELECT * FROM {is}.TABLES WHERE table_type = 'BASE TABLE'")),
            q(format!("SELECT * FROM {is}.COLUMNS")),
            q(format!("SELECT table_name, option_value FROM {is}.TABLE_OPTIONS WHERE option_name = 'description'")),
            q(format!(
                "SELECT table_name, column_name, description FROM {is}.COLUMN_FIELD_PATHS
                 WHERE field_path = column_name AND description IS NOT NULL"
            )),
            q(format!(
                "SELECT k.table_name, k.constraint_name, c.constraint_type, k.column_name, k.ordinal_position,
                        k.position_in_unique_constraint
                 FROM {is}.KEY_COLUMN_USAGE k
                 JOIN {is}.TABLE_CONSTRAINTS c ON c.constraint_name = k.constraint_name AND c.table_name = k.table_name"
            )),
            q(format!("SELECT constraint_name, table_schema, table_name, column_name FROM {is}.CONSTRAINT_COLUMN_USAGE")),
        );
        let (keys, refs) = match (keys, refs) {
            (Ok(k), Ok(r)) => (k, r),
            _ => (Vec::new(), Vec::new()),
        };
        let mut out = ddl::assemble(&ds, &tables?, &columns?, &options.unwrap_or_default(), &descriptions.unwrap_or_default(), &keys, &refs);
        // Search and vector indexes, BigQuery's only ones; emulators don't
        // have these views.
        let (search, vector) = tokio::join!(
            q(format!("SELECT table_name, index_name, ddl FROM {is}.SEARCH_INDEXES")),
            q(format!("SELECT table_name, index_name, ddl FROM {is}.VECTOR_INDEXES")),
        );
        indexes::attach(&mut out, indexes::SEARCH, &search.unwrap_or_default());
        indexes::attach(&mut out, indexes::VECTOR, &vector.unwrap_or_default());
        Ok(out)
    }

    /// `datasets.insert` in the connection's location (DDL `CREATE SCHEMA`
    /// needs a region qualifier to land in one).
    async fn create_database(&mut self, name: &str) -> Result<()> {
        let mut body = json!({ "datasetReference": { "projectId": self.api.project, "datasetId": name } });
        if let Some(l) = &self.api.location {
            body["location"] = json!(l);
        }
        self.api.post(&["datasets"], &[], &body).await.map(|_| ())
    }

    async fn create_database_choices(&mut self) -> Result<Vec<dbine_driver::FieldChoices>> {
        self.create_database_choices_impl().await
    }

    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.create_database_with_impl(name, options).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.dataset.as_deref() == Some(name) {
            return Err(Error::Query(format!("no se puede borrar el dataset «{name}»: es el de la sesión actual")));
        }
        let url = self.api.url(&["datasets", name])?;
        self.api.send(self.api.http.delete(url).query(&[("deleteContents", "true")])).await.map(|_| ())
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        let ds = obj.schema().map(str::to_string).or_else(|| self.dataset.clone());
        select_top(Quote::Backtick, Limit::Limit, ds.as_deref(), &obj.name, limit)
    }

    /// The whole script as one job (BigQuery scripting), as the console
    /// runs it, then each statement's result from its child job. Scripts
    /// that leave temp tables, variables or a transaction open a BigQuery
    /// session, which the tab's later runs share.
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let units = script::units(text);
        let want = !self.api.emulator && (self.session_id.is_some() || script::needs_session(&units));
        let ran = match self.run_editor(text, max_rows, want).await {
            Err(Error::Query(m)) if self.session_id.is_some() && script::session_gone(&m) => {
                self.session_id = None;
                out.warning(
                    "La sesión de BigQuery terminó (venció por inactividad) y se abrió una nueva: sus tablas temporales y variables ya no están.",
                );
                self.run_editor(text, max_rows, true).await
            }
            other => other,
        };
        let (job, r) = ran.map_err(|e| match e {
            Error::Query(m) => script::error(&m, text).into(),
            other => other,
        })?;
        if script::is_script(&units) {
            match self.push_children(&job, max_rows, out).await {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => tracing::debug!("bigquery script children: {e}"),
            }
        }
        push_result(&r, max_rows, out);
        Ok(())
    }

    /// Estimated: a dry run per statement (nothing runs or is billed); the
    /// plan is the bytes it would process and the tables it reads, since
    /// BigQuery only builds stages when it runs. Actual: the script runs
    /// once, as with `execute`, and each job's `queryPlan` (stages with
    /// rows, slot time, shuffle, spill) is the plan.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        if analyze {
            let (job, r) = self.query_job(text, max_rows, None).await?;
            push_result(&r, max_rows, out);
            if let Err(e) = self.actual_plans(&job, text, out).await {
                out.messages.push(format!("No se pudo leer el plan del trabajo {}: {e}", job.id));
            }
            return Ok(());
        }
        let stmts = split_statements(text);
        let single = stmts.len() == 1;
        for stmt in stmts {
            let one: String = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
            match self.estimated_plan(&stmt).await {
                Ok(Some(p)) => out.plans.push(p),
                Ok(None) => out.messages.push(format!("Sin plan (no se ejecutó): {one}")),
                Err(e) if single => return Err(e),
                // A statement may need what an earlier one creates (a
                // table, a variable): its dry run alone can't see it.
                Err(e) => out.messages.push(format!("No se pudo estimar «{one}»: {e}")),
            }
        }
        Ok(())
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        index_usage::report(self, table).await.map(Some)
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        backup::history(self, database).await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.snapshot().await
    }

    /// The running and pending jobs (`jobs.list`): BigQuery has no
    /// connections to list.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        self.processes_list().await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.cancel_running(id).await
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

    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, source, progress).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let (api, job) = (self.api.clone(), self.job.clone());
        let rt = tokio::runtime::Handle::try_current().ok()?;
        Some(Arc::new(move || {
            let Some(j) = job.lock().ok().and_then(|j| j.clone()) else { return };
            let api = api.clone();
            rt.spawn(async move { api.cancel(&j).await });
        }))
    }

    /// IAM's permission tests on the project and a table (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        Ok(permissions::check(self, database).await)
    }
}

fn str_of<'a>(f: &'a Json, key: &str) -> &'a str {
    f.get(key).and_then(Json::as_str).unwrap_or("")
}

fn is_repeated(f: &Json) -> bool {
    str_of(f, "mode") == "REPEATED"
}

fn is_record(f: &Json) -> bool {
    matches!(str_of(f, "type"), "RECORD" | "STRUCT")
}

/// `INT64`, `ARRAY<STRING>`, `STRUCT`…
fn type_label(f: &Json) -> String {
    let t = if is_record(f) { "STRUCT" } else { str_of(f, "type") };
    if is_repeated(f) {
        format!("ARRAY<{t}>")
    } else {
        t.to_string()
    }
}

/// Schema fields as columns; a record's fields follow it as `parent.child`.
fn flatten_fields(fields: &[Json], prefix: &str, out: &mut Vec<ColumnInfo>) {
    for f in fields {
        let name = format!("{prefix}{}", str_of(f, "name"));
        out.push(ColumnInfo {
            name: name.clone(),
            data_type: type_label(f),
            nullable: str_of(f, "mode") != "REQUIRED",
            primary_key: false,
            auto_increment: false,
            default_value: f.get("defaultValueExpression").and_then(Json::as_str).map(str::to_string),
        });
        if let Some(sub) = f.get("fields").and_then(Json::as_array) {
            flatten_fields(sub, &format!("{name}."), out);
        }
    }
}

/// A result cell: scalars typed, arrays and records as compact JSON text.
fn cell(v: &Json, f: &Json) -> Json {
    if v.is_null() {
        return Json::Null;
    }
    if is_repeated(f) || is_record(f) {
        Json::String(typed(v, f, is_repeated(f)).to_string())
    } else {
        scalar(v, str_of(f, "type"))
    }
}

/// A value as structured JSON (for nested values).
fn typed(v: &Json, f: &Json, repeated: bool) -> Json {
    if v.is_null() {
        return Json::Null;
    }
    if repeated {
        return Json::Array(
            v.as_array().into_iter().flatten().map(|e| typed(e.get("v").unwrap_or(&Json::Null), f, false)).collect(),
        );
    }
    if is_record(f) {
        let sub = f.get("fields").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
        let vals = v.get("f").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
        let mut m = Map::new();
        for (i, sf) in sub.iter().enumerate() {
            let sv = vals.get(i).and_then(|c| c.get("v")).unwrap_or(&Json::Null);
            m.insert(str_of(sf, "name").to_string(), typed(sv, sf, is_repeated(sf)));
        }
        return Json::Object(m);
    }
    scalar(v, str_of(f, "type"))
}

fn scalar(v: &Json, ty: &str) -> Json {
    let Some(s) = v.as_str() else { return v.clone() };
    match ty {
        "INTEGER" | "INT64" => s.parse::<i64>().map_or_else(|_| s.into(), json_i64),
        "FLOAT" | "FLOAT64" => match s.parse::<f64>() {
            Ok(f) if f.is_finite() => json_f64(f),
            _ => s.into(),
        },
        "BOOLEAN" | "BOOL" => Json::Bool(s.eq_ignore_ascii_case("true")),
        "TIMESTAMP" => timestamp(s).map_or_else(|| s.into(), Json::String),
        "DATETIME" => s.replacen('T', " ", 1).into(),
        "BYTES" => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(s).map_or_else(|_| s.into(), |b| json_bytes(&b))
        }
        _ => s.into(),
    }
}

/// BigQuery sends TIMESTAMP as epoch microseconds (with
/// `useInt64Timestamp`) or as epoch seconds with a fraction.
fn timestamp(s: &str) -> Option<String> {
    let micros = if s.contains(['.', 'e', 'E']) {
        (s.parse::<f64>().ok()? * 1e6).round() as i64
    } else {
        let n: i64 = s.parse().ok()?;
        // Epoch seconds stay below 1e11 until the year 5138.
        if n.abs() >= 100_000_000_000 {
            n
        } else {
            n.checked_mul(1_000_000)?
        }
    };
    let t = chrono::DateTime::from_timestamp_micros(micros)?;
    Some(format!("{} UTC", t.format("%Y-%m-%d %H:%M:%S%.f")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_in_both_encodings() {
        assert_eq!(timestamp("1706708700000000").as_deref(), Some("2024-01-31 13:45:00 UTC"));
        assert_eq!(timestamp("1.7067087E9").as_deref(), Some("2024-01-31 13:45:00 UTC"));
        assert_eq!(timestamp("1706708700.5").as_deref(), Some("2024-01-31 13:45:00.500 UTC"));
        assert_eq!(timestamp("1706708700").as_deref(), Some("2024-01-31 13:45:00 UTC"));
    }

    #[test]
    fn cells_follow_the_schema() {
        // A recorded jobs.query response (bigquery-emulator).
        let resp: Json = serde_json::from_str(
            r#"{"schema":{"fields":[{"mode":"NULLABLE","name":"a","type":"INTEGER"},{"mode":"NULLABLE","name":"b","type":"FLOAT"},
            {"mode":"NULLABLE","name":"t","type":"TIMESTAMP"},{"mode":"REPEATED","name":"arr","type":"INTEGER"},
            {"fields":[{"mode":"NULLABLE","name":"x","type":"INTEGER"},{"mode":"NULLABLE","name":"y","type":"STRING"}],"mode":"NULLABLE","name":"st","type":"RECORD"},
            {"mode":"NULLABLE","name":"bb","type":"BYTES"},{"mode":"NULLABLE","name":"n","type":"NUMERIC"},
            {"mode":"NULLABLE","name":"dt","type":"DATETIME"},{"mode":"NULLABLE","name":"ok","type":"BOOLEAN"},
            {"mode":"NULLABLE","name":"big","type":"INTEGER"},{"mode":"NULLABLE","name":"z","type":"STRING"}]},
            "rows":[{"f":[{"v":"1"},{"v":"1.5"},{"v":"1706708700000000"},{"v":[{"v":"1"},{"v":"2"}]},{"v":{"f":[{"v":"1"},{"v":"s"}]}},
            {"v":"YWI="},{"v":"1.1"},{"v":"2024-01-31T13:45:00"},{"v":"true"},{"v":"9223372036854775807"},{"v":null}]}],
            "totalRows":"1","jobComplete":true}"#,
        )
        .unwrap();
        let fields = resp.pointer("/schema/fields").unwrap().as_array().unwrap();
        let row = resp.pointer("/rows/0/f").unwrap().as_array().unwrap();
        let cells: Vec<Json> = fields.iter().zip(row).map(|(f, c)| cell(&c["v"], f)).collect();
        assert_eq!(
            cells,
            vec![
                json!(1),
                json!(1.5),
                json!("2024-01-31 13:45:00 UTC"),
                json!("[1,2]"),
                json!("{\"x\":1,\"y\":\"s\"}"),
                json!("0x6162"),
                json!("1.1"),
                json!("2024-01-31 13:45:00"),
                json!(true),
                json!("9223372036854775807"),
                Json::Null
            ]
        );
        assert_eq!(type_label(&fields[3]), "ARRAY<INTEGER>");
        assert_eq!(type_label(&fields[4]), "STRUCT");
    }

    #[test]
    fn nested_fields_are_flattened() {
        let fields: Vec<Json> = serde_json::from_str(
            r#"[{"name":"id","type":"INT64","mode":"REQUIRED"},
                {"name":"a","type":"RECORD","mode":"REPEATED","fields":[{"name":"b","type":"STRING"},
                  {"name":"c","type":"RECORD","fields":[{"name":"d","type":"DATE"}]}]}]"#,
        )
        .unwrap();
        let mut out = Vec::new();
        flatten_fields(&fields, "", &mut out);
        let got: Vec<_> = out.iter().map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable)).collect();
        assert_eq!(
            got,
            vec![
                ("id", "INT64", false),
                ("a", "ARRAY<STRUCT>", true),
                ("a.b", "STRING", true),
                ("a.c", "STRUCT", true),
                ("a.c.d", "DATE", true)
            ]
        );
    }

    #[test]
    fn urls_escape_segments() {
        let http = reqwest::Client::new();
        let cfg = ConnectionConfig {
            options: [("endpoint_url".to_string(), "http://localhost:9050/".to_string())].into(),
            ..Default::default()
        };
        let api = Api {
            tokens: gcp::Tokens::from_config(&cfg, http.clone()).unwrap(),
            http,
            base: "http://localhost:9050".into(),
            project: "p".into(),
            location: None,
            emulator: true,
        };
        assert_eq!(
            api.url(&["datasets", "d", "tables", "a/b c"]).unwrap().as_str(),
            "http://localhost:9050/bigquery/v2/projects/p/datasets/d/tables/a%2Fb%20c"
        );
    }
}

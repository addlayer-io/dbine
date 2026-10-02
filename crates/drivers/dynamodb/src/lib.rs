//! Amazon DynamoDB through the AWS SDK. Scripts are PartiQL
//! (`ExecuteStatement`), one statement per request; each item is a row and
//! the columns are the union of the attribute names, keys first.

mod admin;
mod aws;
mod backup;
mod ddl;
mod index_usage;
mod sync;
mod monitor;
mod permissions;
mod plan;
mod transfer;

use aws_sdk_dynamodb::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::types::{
    AttributeValue, ConsumedCapacity, KeySchemaElement, KeyType, Projection, ReturnConsumedCapacity, TableDescription,
};
use aws_sdk_dynamodb::Client;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{
    async_trait, json_bytes, Capabilities, MonitorSnapshot, json_f64, json_i64, kinds, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject,
    DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, IndexDef, KeyDef, Language, ObjectKindInfo, ObjectRef,
    QueryOutcome, Result, ResultColumn, Session, TableSchema,
};
use serde_json::{json, Map, Value as Json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(DynamoDriver { info: info() })]
}

fn info() -> DriverInfo {
    let mut fields = aws::fields_by_auth();
    fields.push(aws::endpoint_field("http://localhost:8000"));
    fields.push(Field::read_only());
    DriverInfo {
        id: "dynamodb",
        name: "Amazon DynamoDB",
        family: Family::KeyValue,
        language: Language::Sql,
        dialect: "partiql",
        default_port: 0,
        fields,
        databases_label: "",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::new(kinds::INDEX, "Índices secundarios", true, true, true),
        ],
    }
}

/// Syntax help for the editor.
pub const QUERY_HELP: &str = "PartiQL de DynamoDB, una sentencia por «;»:\n\
SELECT * FROM \"Pedidos\" WHERE cliente = 'ana' AND fecha > 20240101\n\
SELECT * FROM \"Pedidos\".\"porEstado\" WHERE estado = 'abierto'   (consulta sobre un índice)\n\
INSERT INTO \"Pedidos\" VALUE {'cliente': 'ana', 'fecha': 20240102, 'items': [1, 2]}\n\
UPDATE \"Pedidos\" SET estado = 'cerrado' WHERE cliente = 'ana' AND fecha = 20240102\n\
DELETE FROM \"Pedidos\" WHERE cliente = 'ana' AND fecha = 20240102\n\
\n\
Además de PartiQL, DBine entiende estas sentencias de administración:\n\
CREATE TABLE [IF NOT EXISTS] \"Pedidos\" { …entrada de CreateTable en JSON… }\n\
  (AttributeDefinitions, KeySchema, BillingMode, ProvisionedThroughput, GlobalSecondaryIndexes,\n\
  LocalSecondaryIndexes, StreamSpecification, TableClass, DeletionProtectionEnabled, Tags y\n\
  TimeToLiveSpecification, que se aplica al quedar activa la tabla). Espera a que quede ACTIVE.\n\
DROP TABLE [IF EXISTS] \"Pedidos\"\n\
CREATE INDEX \"porEstado\" ON \"Pedidos\" { \"AttributeDefinitions\": […], \"KeySchema\": […], \"Projection\": {…} }\n\
  (agrega un índice secundario global; sin Projection, se proyecta ALL)\n\
DROP INDEX \"porEstado\" ON \"Pedidos\"\n\
CREATE BACKUP \"semanal\" FOR TABLE \"Pedidos\"   (backup a pedido; muestra su ARN)\n\
RESTORE TABLE \"Pedidos_restaurada\" FROM BACKUP \"arn:aws:dynamodb:…\"   (en una tabla nueva)\n\
DROP BACKUP \"arn:aws:dynamodb:…\"";

/// Items sampled to infer the non-key attributes of a table.
const SAMPLE: i32 = 50;

pub struct DynamoDriver {
    info: DriverInfo,
}

pub struct DynamoSession {
    client: Client,
    region: String,
    local: bool,
    read_only: bool,
    /// Key attribute names per table, to put them first in results.
    keys: HashMap<String, Vec<String>>,
}

pub(crate) fn err<E, R>(e: SdkError<E, R>) -> Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug,
{
    let unreachable = matches!(e, SdkError::DispatchFailure(_) | SdkError::TimeoutError(_));
    aws::classify(e.code(), e.message(), unreachable, DisplayErrorContext(&e).to_string())
}

#[async_trait]
impl Driver for DynamoDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn query_help(&self) -> &'static str {
        QUERY_HELP
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// The key, GSIs and LSIs, without counters (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    /// Batched PartiQL `INSERT`s, several requests at once (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// One "database" per account and region: nothing to create or drop.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: true, ..Default::default() }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::create_templates()
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

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Json>]) -> Result<String> {
        Ok(ddl::insert_script(&target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        ddl::update_script(&target.name, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Json)>]) -> Result<String> {
        ddl::delete_script(&target.name, keys)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let conf = aws::sdk_config(cfg).await?;
        let region = conf.region().map(|r| r.to_string()).unwrap_or_default();
        let client = Client::new(&conf);
        // A cheap call that proves the endpoint and the credentials work.
        tokio::time::timeout(Duration::from_secs(20), client.list_tables().limit(1).send())
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(err)?;
        Ok(Box::new(DynamoSession {
            client,
            region,
            local: cfg.option("endpoint_url").is_some(),
            read_only: cfg.read_only,
            keys: HashMap::new(),
        }))
    }
}

impl DynamoSession {
    async fn describe(&self, table: &str) -> Result<TableDescription> {
        let out = self.client.describe_table().table_name(table).send().await.map_err(err)?;
        out.table.ok_or_else(|| Error::Query(format!("no se encontró la tabla {table}")))
    }

    async fn key_names(&mut self, table: &str) -> Vec<String> {
        if let Some(k) = self.keys.get(table) {
            return k.clone();
        }
        let keys = match self.describe(table).await {
            Ok(t) => key_order(t.key_schema()),
            Err(_) => Vec::new(),
        };
        self.keys.insert(table.to_string(), keys.clone());
        keys
    }

    /// Runs `stmt` when it's one of the administration extensions (see
    /// `admin`); `false` when it's PartiQL.
    async fn run_admin(&mut self, stmt: &str, out: &mut QueryOutcome) -> Result<bool> {
        let Some(a) = admin::parse_admin(stmt)? else { return Ok(false) };
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: solo se permiten sentencias SELECT.".into()));
        }
        self.keys.clear();
        admin::run(&self.client, a, out).await?;
        Ok(true)
    }

    async fn run(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.run_measured(stmt, max_rows, out, None).await
    }

    /// Runs a statement; with `usage`, asks for the consumed capacity of
    /// every page and adds it up there.
    async fn run_measured(
        &mut self,
        stmt: &str,
        max_rows: usize,
        out: &mut QueryOutcome,
        mut usage: Option<&mut Usage>,
    ) -> Result<()> {
        let is_select = first_word(stmt).eq_ignore_ascii_case("select");
        if self.read_only && !is_select {
            return Err(Error::Query(
                "Conexión de solo lectura: solo se permiten sentencias SELECT.".into(),
            ));
        }
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        let mut more = false;
        loop {
            let mut req = self.client.execute_statement().statement(stmt).set_next_token(token.take());
            if usage.is_some() {
                req = req.return_consumed_capacity(ReturnConsumedCapacity::Indexes);
            }
            let resp = req.send().await.map_err(err)?;
            if let Some(u) = usage.as_deref_mut() {
                u.pages += 1;
                u.items += resp.items().len() as u64;
                if let Some(c) = resp.consumed_capacity() {
                    u.add(c);
                }
            }
            let next = resp.next_token.clone();
            for item in resp.items.unwrap_or_default() {
                if items.len() < max_rows {
                    items.push(item);
                } else {
                    more = true;
                    break;
                }
            }
            match next {
                Some(t) if !more && items.len() < max_rows => token = Some(t),
                Some(_) => {
                    more = true;
                    break;
                }
                None => break,
            }
        }
        if !is_select && items.is_empty() {
            // PartiQL writes touch exactly one item.
            out.push_affected(1);
            return Ok(());
        }
        let keys = match from_table(stmt) {
            Some(t) => self.key_names(&t).await,
            None => Vec::new(),
        };
        let cols = union_columns(&keys, &items);
        out.begin_result(cols.iter().map(|c| ResultColumn { name: c.clone(), type_name: String::new() }).collect());
        for item in &items {
            out.push_row(cols.iter().map(|c| item.get(c).map_or(Json::Null, cell)).collect(), max_rows);
        }
        if more {
            if let Some(r) = out.results.last_mut() {
                r.truncated = true;
            }
        }
        Ok(())
    }

    async fn table_names(&self) -> Result<Vec<String>> {
        let mut tables = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let resp = self.client.list_tables().set_exclusive_start_table_name(start.take()).send().await.map_err(err)?;
            tables.extend(resp.table_names().iter().cloned());
            match resp.last_evaluated_table_name() {
                Some(t) => start = Some(t.to_string()),
                None => return Ok(tables),
            }
        }
    }

    /// A table as the designer models it: key attributes (with their
    /// `key_type`), the other attributes GSIs/LSIs use, then attributes seen
    /// in a sample; GSIs and LSIs as indexes; capacity, streams and TTL as
    /// options.
    async fn table_schema(&mut self, name: &str) -> Result<TableSchema> {
        let d = self.describe(name).await?;
        let types: HashMap<&str, &str> =
            d.attribute_definitions().iter().map(|a| (a.attribute_name(), a.attribute_type().as_str())).collect();
        let keys = key_order(d.key_schema());
        let key_type = |n: &str| {
            d.key_schema().iter().find(|k| k.attribute_name() == n).map_or("none", |k| k.key_type().as_str()).to_string()
        };
        let attr = |n: &str, nullable: bool| ColumnDef {
            name: n.to_string(),
            data_type: types.get(n).copied().unwrap_or("").to_string(),
            nullable,
            options: BTreeMap::from([(ddl::KEY_TYPE.to_string(), key_type(n))]),
            ..Default::default()
        };
        let mut columns: Vec<ColumnDef> = keys.iter().map(|k| attr(k, false)).collect();
        for a in d.attribute_definitions() {
            if !keys.iter().any(|k| k == a.attribute_name()) {
                columns.push(attr(a.attribute_name(), true));
            }
        }
        let known: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        if let Ok(sample) = self.client.scan().table_name(name).limit(SAMPLE).send().await {
            columns.extend(infer_columns(&known, sample.items()).into_iter().map(|c| ColumnDef {
                name: c.name,
                data_type: c.data_type,
                nullable: c.nullable,
                options: BTreeMap::from([(ddl::KEY_TYPE.to_string(), "none".to_string())]),
                ..Default::default()
            }));
        }
        let mut indexes: Vec<IndexDef> = d
            .global_secondary_indexes()
            .iter()
            .map(|g| IndexDef {
                name: g.index_name().unwrap_or_default().to_string(),
                columns: key_order(g.key_schema()),
                kind: Some("GSI".into()),
                ..projected(g.projection())
            })
            .collect();
        indexes.extend(d.local_secondary_indexes().iter().map(|l| IndexDef {
            name: l.index_name().unwrap_or_default().to_string(),
            columns: key_order(l.key_schema()),
            kind: Some("LSI".into()),
            ..projected(l.projection())
        }));
        let mut options = BTreeMap::new();
        let mode = d.billing_mode_summary().and_then(|b| b.billing_mode()).map_or("PROVISIONED", |m| m.as_str());
        options.insert(ddl::BILLING_MODE.to_string(), mode.to_string());
        if mode == "PROVISIONED" {
            if let Some(p) = d.provisioned_throughput() {
                options.insert(ddl::READ_CAPACITY.to_string(), p.read_capacity_units().unwrap_or(0).max(1).to_string());
                options.insert(ddl::WRITE_CAPACITY.to_string(), p.write_capacity_units().unwrap_or(0).max(1).to_string());
            }
        }
        let stream = d
            .stream_specification()
            .filter(|s| s.stream_enabled())
            .and_then(|s| s.stream_view_type())
            .map_or("none", |v| v.as_str());
        options.insert(ddl::STREAM_VIEW_TYPE.to_string(), stream.to_string());
        if let Ok(ttl) = self.client.describe_time_to_live().table_name(name).send().await {
            if let Some(t) = ttl.time_to_live_description() {
                let on = matches!(t.time_to_live_status().map(|s| s.as_str()), Some("ENABLED" | "ENABLING"));
                if let (true, Some(a)) = (on, t.attribute_name()) {
                    options.insert(ddl::TTL_ATTRIBUTE.to_string(), a.to_string());
                }
            }
        }
        Ok(TableSchema {
            kind: kinds::TABLE.into(),
            name: name.to_string(),
            columns,
            primary_key: Some(KeyDef { name: None, columns: keys }),
            indexes,
            options,
            ..Default::default()
        })
    }

    /// Key schema and size of the table (or index) a statement targets.
    async fn plan_target(&self, table: &str, index: Option<&str>) -> Result<plan::Target> {
        let d = self.describe(table).await?;
        let mut t = plan::Target { table: table.to_string(), index: index.map(str::to_string), ..Default::default() };
        let keys: &[KeySchemaElement] = match index {
            Some(i) => {
                let (ks, _) = index_of(&d, i).ok_or_else(|| no_index(table, i))?;
                if let Some(g) = d.global_secondary_indexes().iter().find(|g| g.index_name() == Some(i)) {
                    t.item_count = g.item_count();
                    t.size_bytes = g.index_size_bytes();
                } else if let Some(l) = d.local_secondary_indexes().iter().find(|l| l.index_name() == Some(i)) {
                    t.item_count = l.item_count();
                    t.size_bytes = l.index_size_bytes();
                }
                ks
            }
            None => {
                t.item_count = d.item_count();
                t.size_bytes = d.table_size_bytes();
                d.key_schema()
            }
        };
        for k in keys {
            match k.key_type() {
                KeyType::Hash => t.partition_key = k.attribute_name().to_string(),
                _ => t.sort_key = Some(k.attribute_name().to_string()),
            }
        }
        match d.billing_mode_summary().and_then(|b| b.billing_mode()) {
            Some(m) => t.props.push(("Modo de capacidad".into(), m.as_str().to_string())),
            None => t.props.push(("Modo de capacidad".into(), "PROVISIONED".into())),
        }
        if let Some(p) = d.provisioned_throughput().filter(|p| p.read_capacity_units().unwrap_or(0) > 0) {
            t.props.push(("RCU aprovisionadas".into(), p.read_capacity_units().unwrap_or(0).to_string()));
            t.props.push(("WCU aprovisionadas".into(), p.write_capacity_units().unwrap_or(0).to_string()));
        }
        Ok(t)
    }
}

/// Consumed capacity and items of a measured run, over all its pages.
#[derive(Default)]
struct Usage {
    pages: u32,
    items: u64,
    total: f64,
    read: f64,
    write: f64,
    table: f64,
    indexes: BTreeMap<String, f64>,
    reported: bool,
}

impl Usage {
    fn add(&mut self, c: &ConsumedCapacity) {
        self.reported = true;
        self.total += c.capacity_units().unwrap_or(0.0);
        self.read += c.read_capacity_units().unwrap_or(0.0);
        self.write += c.write_capacity_units().unwrap_or(0.0);
        self.table += c.table().and_then(|t| t.capacity_units()).unwrap_or(0.0);
        for m in [c.global_secondary_indexes(), c.local_secondary_indexes()].into_iter().flatten() {
            for (name, cap) in m {
                *self.indexes.entry(name.clone()).or_default() += cap.capacity_units().unwrap_or(0.0);
            }
        }
    }

    fn props(&self) -> Vec<(String, String)> {
        let mut v = vec![("Páginas".to_string(), self.pages.to_string())];
        if !self.reported {
            v.push(("Capacidad consumida".into(), "el servidor no la informó".into()));
            return v;
        }
        v.push(("Capacidad consumida (unidades)".into(), self.total.to_string()));
        if self.read > 0.0 {
            v.push(("RCU consumidas".into(), self.read.to_string()));
        }
        if self.write > 0.0 {
            v.push(("WCU consumidas".into(), self.write.to_string()));
        }
        if self.table > 0.0 {
            v.push(("Capacidad de la tabla".into(), self.table.to_string()));
        }
        for (name, c) in &self.indexes {
            v.push((format!("Capacidad del índice {name}"), c.to_string()));
        }
        v
    }
}

#[async_trait]
impl Session for DynamoSession {
    async fn server_version(&mut self) -> Result<String> {
        Ok(if self.local {
            format!("DynamoDB (endpoint local, {})", self.region)
        } else {
            format!("Amazon DynamoDB ({})", self.region)
        })
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["default".into()])
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        backup::history(self, database).await
    }

    /// `DescribeTable` of the tables (a few in parallel, at most
    /// [`monitor::MAX_TABLES`]) and `DescribeLimits`.
    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        use futures::stream::{self, StreamExt};
        let tables = self.table_names().await?;
        let client = &self.client;
        let described: Vec<Option<TableDescription>> = stream::iter(tables.iter().take(monitor::MAX_TABLES).cloned())
            .map(|t: String| async move { client.describe_table().table_name(t).send().await.ok().and_then(|o| o.table) })
            .buffered(8)
            .collect()
            .await;
        let described: Vec<TableDescription> = described.into_iter().flatten().collect();
        let limits = self.client.describe_limits().send().await.ok().map(|l| monitor::Limits {
            account_read: l.account_max_read_capacity_units(),
            account_write: l.account_max_write_capacity_units(),
            table_read: l.table_max_read_capacity_units(),
            table_write: l.table_max_write_capacity_units(),
        });
        Ok(monitor::snapshot(&self.region, self.local, tables.len(), &described, limits))
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let tables = self.table_names().await?;
        // Indexes need a DescribeTable per table; a few in parallel.
        use futures::stream::{self, StreamExt};
        let client = &self.client;
        let described: Vec<(String, Option<TableDescription>)> = stream::iter(tables)
            .map(|t| async move {
                let d = client.describe_table().table_name(&t).send().await.ok().and_then(|o| o.table);
                (t, d)
            })
            .buffered(8)
            .collect()
            .await;
        let mut out = Vec::new();
        for (table, desc) in described {
            out.push(DbObject { kind: kinds::TABLE.into(), schema: None, name: table.clone(), parent: None });
            let Some(d) = desc else { continue };
            self.keys.insert(table.clone(), key_order(d.key_schema()));
            let names = d
                .global_secondary_indexes()
                .iter()
                .filter_map(|i| i.index_name())
                .chain(d.local_secondary_indexes().iter().filter_map(|i| i.index_name()));
            for name in names {
                // The table rides in `schema` so ObjectRef can find it back.
                out.push(DbObject {
                    kind: kinds::INDEX.into(),
                    schema: Some(table.clone()),
                    name: name.to_string(),
                    parent: Some(table.clone()),
                });
            }
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let (table, index) = target(obj)?;
        let d = self.describe(table).await?;
        let key_schema: Vec<KeySchemaElement> = match index {
            None => d.key_schema().to_vec(),
            Some(i) => index_of(&d, i).map(|(ks, _)| ks.to_vec()).ok_or_else(|| no_index(table, i))?,
        };
        let types: HashMap<&str, &str> =
            d.attribute_definitions().iter().map(|a| (a.attribute_name(), a.attribute_type().as_str())).collect();
        let keys = key_order(&key_schema);
        let mut cols: Vec<ColumnInfo> = keys
            .iter()
            .map(|k| ColumnInfo {
                name: k.clone(),
                data_type: types.get(k.as_str()).copied().unwrap_or("").to_string(),
                nullable: false,
                primary_key: true,
                auto_increment: false,
                default_value: None,
            })
            .collect();
        let sample = self
            .client
            .scan()
            .table_name(table)
            .set_index_name(index.map(str::to_string))
            .limit(SAMPLE)
            .send()
            .await
            .map_err(err)?;
        cols.extend(infer_columns(&keys, sample.items()));
        Ok(cols)
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let (table, index) = target(obj)?;
        let d = self.describe(table).await?;
        let doc = match index {
            None => table_json(&d),
            Some(i) => {
                let (ks, proj) = index_of(&d, i).ok_or_else(|| no_index(table, i))?;
                json!({ "IndexName": i, "TableName": table, "KeySchema": key_schema_json(ks), "Projection": projection_json(proj) })
            }
        };
        Ok(Some(serde_json::to_string_pretty(&doc)?))
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let mut out = Vec::new();
        for t in self.table_names().await? {
            out.push(self.table_schema(&t).await?);
        }
        Ok(out)
    }

    fn browse_query(&self, obj: &ObjectRef, _limit: u32) -> String {
        browse(obj)
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, columns, source, progress).await
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for stmt in ddl::split_script(text) {
            if !self.run_admin(&stmt, out).await? {
                self.run(&stmt, max_rows, out).await?;
            }
        }
        Ok(())
    }

    /// DynamoDB has no EXPLAIN: the plan is derived from the statement's
    /// WHERE and the table's key schema (GetItem / Query / Scan, see
    /// `plan`). Estimated: only DescribeTable is called. Actual: the
    /// statement runs once, asking for its consumed capacity.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for stmt in ddl::split_script(text) {
            if admin::parse_admin(&stmt)?.is_some() {
                if analyze {
                    self.run_admin(&stmt, out).await?;
                } else {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", stmt.split_whitespace().collect::<Vec<_>>().join(" ")));
                }
                continue;
            }
            let (verb, target) = plan::target_of(&stmt);
            let derived = match (verb, &target) {
                (plan::Verb::Other, _) | (_, None) => None,
                (_, Some((table, index))) => Some(plan::derive(&stmt, verb, &self.plan_target(table, index.as_deref()).await?)),
            };
            if !analyze {
                match derived {
                    Some(p) => out.plans.push(p),
                    None => out.messages.push(format!("Sin plan (no se ejecutó): {}", stmt.split_whitespace().collect::<Vec<_>>().join(" "))),
                }
                continue;
            }
            let mut usage = Usage::default();
            let started = std::time::Instant::now();
            self.run_measured(&stmt, max_rows, out, Some(&mut usage)).await?;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let Some(mut p) = derived else { continue };
            p.actual = true;
            let a = plan::access_mut(&mut p);
            a.actual_rows = Some(usage.items as f64);
            a.actual_ms = Some(ms);
            a.executions = Some(usage.pages as f64);
            a.props.extend(usage.props());
            p.root.actual_ms = Some(ms);
            p.root.actual_rows = Some(usage.items as f64);
            out.plans.push(p);
        }
        Ok(())
    }

    /// IAM can't be asked (see `permissions`).
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        Ok(permissions::decide())
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        let (name, _) = target(table)?;
        Ok(Some(index_usage::report(&self.describe(name).await?)))
    }
}

/// The browse statement of a table or index. PartiQL has no LIMIT;
/// `execute` stops at `max_rows`.
fn browse(obj: &ObjectRef) -> String {
    match obj.schema() {
        Some(table) if obj.kind == kinds::INDEX => {
            format!("SELECT * FROM {}.{}", quote_ident(Quote::Double, table), quote_ident(Quote::Double, &obj.name))
        }
        _ => format!("SELECT * FROM {}", quote_ident(Quote::Double, &obj.name)),
    }
}

/// Table and optional index an object refers to (indexes carry their table
/// in `schema`).
fn target(obj: &ObjectRef) -> Result<(&str, Option<&str>)> {
    if obj.kind == kinds::INDEX {
        let table = obj.schema().ok_or_else(|| Error::Query(format!("no se sabe a qué tabla pertenece {}", obj.name)))?;
        Ok((table, Some(obj.name.as_str())))
    } else {
        Ok((obj.name.as_str(), None))
    }
}

fn no_index(table: &str, index: &str) -> Error {
    Error::Query(format!("la tabla {table} no tiene el índice {index}"))
}

fn index_of<'a>(d: &'a TableDescription, name: &str) -> Option<(&'a [KeySchemaElement], Option<&'a Projection>)> {
    d.global_secondary_indexes()
        .iter()
        .find(|i| i.index_name() == Some(name))
        .map(|i| (i.key_schema(), i.projection()))
        .or_else(|| {
            d.local_secondary_indexes()
                .iter()
                .find(|i| i.index_name() == Some(name))
                .map(|i| (i.key_schema(), i.projection()))
        })
}

/// Partition key, then sort key.
fn key_order(ks: &[KeySchemaElement]) -> Vec<String> {
    let mut v: Vec<&KeySchemaElement> = ks.iter().collect();
    v.sort_by_key(|k| !matches!(k.key_type(), KeyType::Hash));
    v.into_iter().map(|k| k.attribute_name().to_string()).collect()
}

fn first_word(stmt: &str) -> &str {
    let s = stmt.trim_start();
    let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
    &s[..end]
}

/// The table a `SELECT … FROM "t"` reads, if it's easy to tell.
fn from_table(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    let mut idx = None;
    let mut quote = false;
    for (i, c) in lower.char_indices() {
        match c {
            '"' | '\'' => quote = !quote,
            'f' if !quote && lower[i..].starts_with("from") => {
                let before = lower[..i].chars().last();
                let after = lower[i + 4..].chars().next();
                if before.is_none_or(char::is_whitespace) && after.is_some_and(char::is_whitespace) {
                    idx = Some(i + 4);
                    break;
                }
            }
            _ => {}
        }
    }
    let rest = stmt[idx?..].trim_start();
    if let Some(r) = rest.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = r.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    name.push('"');
                } else {
                    return Some(name);
                }
            } else {
                name.push(c);
            }
        }
        None
    } else {
        let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-')).collect();
        (!name.is_empty()).then_some(name)
    }
}

/// Keys first (when present), then the other attributes in order of first
/// appearance; each item's own attributes are sorted, since the SDK hands
/// them out in a HashMap.
fn union_columns(keys: &[String], items: &[HashMap<String, AttributeValue>]) -> Vec<String> {
    let mut cols: Vec<String> = keys.iter().filter(|k| items.iter().any(|i| i.contains_key(*k))).cloned().collect();
    for item in items {
        let mut names: Vec<&String> = item.keys().collect();
        names.sort();
        for n in names {
            if !cols.contains(n) {
                cols.push(n.clone());
            }
        }
    }
    cols
}

/// Non-key attributes seen in a sample: observed type (`S`, `N`, `M`…; `S | N`
/// when mixed) and nullable unless every item has it.
fn infer_columns(keys: &[String], items: &[HashMap<String, AttributeValue>]) -> Vec<ColumnInfo> {
    let mut seen: BTreeMap<&str, (usize, Vec<&'static str>)> = BTreeMap::new();
    for item in items {
        for (name, v) in item {
            if keys.contains(name) {
                continue;
            }
            let e = seen.entry(name.as_str()).or_default();
            e.0 += 1;
            let t = type_code(v);
            if !e.1.contains(&t) {
                e.1.push(t);
            }
        }
    }
    seen.into_iter()
        .map(|(name, (count, types))| ColumnInfo {
            name: name.to_string(),
            data_type: types.join(" | "),
            nullable: count < items.len() || types.contains(&"NULL"),
            primary_key: false,
            auto_increment: false,
            default_value: None,
        })
        .collect()
}

fn type_code(v: &AttributeValue) -> &'static str {
    match v {
        AttributeValue::S(_) => "S",
        AttributeValue::N(_) => "N",
        AttributeValue::B(_) => "B",
        AttributeValue::Bool(_) => "BOOL",
        AttributeValue::Null(_) => "NULL",
        AttributeValue::L(_) => "L",
        AttributeValue::M(_) => "M",
        AttributeValue::Ss(_) => "SS",
        AttributeValue::Ns(_) => "NS",
        AttributeValue::Bs(_) => "BS",
        _ => "?",
    }
}

/// A top-level attribute as a cell: scalars as they are, lists, maps and
/// sets as compact JSON text.
fn cell(v: &AttributeValue) -> Json {
    match v {
        AttributeValue::L(_)
        | AttributeValue::M(_)
        | AttributeValue::Ss(_)
        | AttributeValue::Ns(_)
        | AttributeValue::Bs(_) => Json::String(to_json(v).to_string()),
        _ => to_json(v),
    }
}

/// An attribute as plain JSON (nested values keep their structure).
fn to_json(v: &AttributeValue) -> Json {
    match v {
        AttributeValue::S(s) => Json::String(s.clone()),
        AttributeValue::N(n) => number(n),
        AttributeValue::B(b) => json_bytes(b.as_ref()),
        AttributeValue::Bool(b) => Json::Bool(*b),
        AttributeValue::Null(_) => Json::Null,
        AttributeValue::L(l) => Json::Array(l.iter().map(to_json).collect()),
        // Sorted: a HashMap has no order, and serde_json keeps insertion
        // order when `preserve_order` is on (bson turns it on workspace-wide).
        AttributeValue::M(m) => {
            let sorted: std::collections::BTreeMap<_, _> = m.iter().collect();
            Json::Object(sorted.into_iter().map(|(k, v)| (k.clone(), to_json(v))).collect::<Map<_, _>>())
        }
        AttributeValue::Ss(s) => Json::Array(s.iter().cloned().map(Json::String).collect()),
        AttributeValue::Ns(n) => Json::Array(n.iter().map(|s| number(s)).collect()),
        AttributeValue::Bs(b) => Json::Array(b.iter().map(|b| json_bytes(b.as_ref())).collect()),
        _ => Json::Null,
    }
}

/// A DynamoDB number (up to 38 digits) as a JSON number when a double holds
/// it exactly, else as its text.
fn number(n: &str) -> Json {
    if let Ok(i) = n.parse::<i64>() {
        return json_i64(i);
    }
    let mantissa = n.split(['e', 'E']).next().unwrap_or(n);
    let digits = mantissa.chars().filter(char::is_ascii_digit).skip_while(|c| *c == '0').collect::<String>();
    let significant = if mantissa.contains('.') { digits.trim_end_matches('0').len() } else { digits.len() };
    match n.parse::<f64>() {
        // 15 significant digits always survive a round trip through f64.
        Ok(f) if f.is_finite() && significant <= 15 => json_f64(f),
        _ => Json::String(n.to_string()),
    }
}

fn key_schema_json(ks: &[KeySchemaElement]) -> Json {
    Json::Array(
        ks.iter().map(|k| json!({ "AttributeName": k.attribute_name(), "KeyType": k.key_type().as_str() })).collect(),
    )
}

/// An index's projection as [`IndexDef`] parts: `INCLUDE`'s attributes in
/// `include`, `KEYS_ONLY` as an option, `ALL` (the default) as nothing.
fn projected(p: Option<&Projection>) -> IndexDef {
    let mut i = IndexDef::default();
    match p.and_then(|p| p.projection_type()).map(|t| t.as_str()) {
        Some("INCLUDE") => i.include = p.map(|p| p.non_key_attributes().to_vec()).unwrap_or_default(),
        Some("KEYS_ONLY") => {
            i.options.insert(ddl::PROJECTION_TYPE.to_string(), "KEYS_ONLY".into());
        }
        _ => {}
    }
    i
}

fn projection_json(p: Option<&Projection>) -> Json {
    match p {
        None => Json::Null,
        Some(p) => {
            let mut o = Map::new();
            if let Some(t) = p.projection_type() {
                o.insert("ProjectionType".into(), t.as_str().into());
            }
            if !p.non_key_attributes().is_empty() {
                o.insert("NonKeyAttributes".into(), p.non_key_attributes().into());
            }
            Json::Object(o)
        }
    }
}

/// The parts of DescribeTable worth reading, in the API's own names.
fn table_json(d: &TableDescription) -> Json {
    let mut o = Map::new();
    let mut put = |k: &str, v: Json| {
        if !v.is_null() {
            o.insert(k.into(), v);
        }
    };
    put("TableName", d.table_name().into());
    put("TableStatus", d.table_status().map(|s| s.as_str()).into());
    put("KeySchema", key_schema_json(d.key_schema()));
    put(
        "AttributeDefinitions",
        Json::Array(
            d.attribute_definitions()
                .iter()
                .map(|a| json!({ "AttributeName": a.attribute_name(), "AttributeType": a.attribute_type().as_str() }))
                .collect(),
        ),
    );
    put("BillingMode", d.billing_mode_summary().and_then(|b| b.billing_mode()).map(|m| m.as_str()).into());
    if let Some(p) = d.provisioned_throughput() {
        put(
            "ProvisionedThroughput",
            json!({ "ReadCapacityUnits": p.read_capacity_units(), "WriteCapacityUnits": p.write_capacity_units() }),
        );
    }
    let gsis: Vec<Json> = d
        .global_secondary_indexes()
        .iter()
        .map(|i| {
            json!({
                "IndexName": i.index_name(),
                "KeySchema": key_schema_json(i.key_schema()),
                "Projection": projection_json(i.projection()),
                "IndexStatus": i.index_status().map(|s| s.as_str()),
            })
        })
        .collect();
    if !gsis.is_empty() {
        put("GlobalSecondaryIndexes", gsis.into());
    }
    let lsis: Vec<Json> = d
        .local_secondary_indexes()
        .iter()
        .map(|i| {
            json!({
                "IndexName": i.index_name(),
                "KeySchema": key_schema_json(i.key_schema()),
                "Projection": projection_json(i.projection()),
            })
        })
        .collect();
    if !lsis.is_empty() {
        put("LocalSecondaryIndexes", lsis.into());
    }
    if let Some(s) = d.stream_specification() {
        put(
            "StreamSpecification",
            json!({ "StreamEnabled": s.stream_enabled(), "StreamViewType": s.stream_view_type().map(|v| v.as_str()) }),
        );
    }
    put("ItemCount", d.item_count().into());
    put("TableSizeBytes", d.table_size_bytes().into());
    put("CreationDateTime", d.creation_date_time().map(|t| t.to_string()).into());
    put("DeletionProtectionEnabled", d.deletion_protection_enabled().into());
    put("TableArn", d.table_arn().into());
    Json::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_dynamodb::primitives::Blob;

    fn s(v: &str) -> AttributeValue {
        AttributeValue::S(v.into())
    }
    fn n(v: &str) -> AttributeValue {
        AttributeValue::N(v.into())
    }

    #[test]
    fn numbers_are_numbers_only_when_exact() {
        assert_eq!(number("42"), json!(42));
        assert_eq!(number("-1.5"), json!(-1.5));
        assert_eq!(number("1.50"), json!(1.5));
        assert_eq!(number("9007199254740993"), json!("9007199254740993"));
        assert_eq!(number("12345678901234567890123"), json!("12345678901234567890123"));
        assert_eq!(number("0.1234567890123456789"), json!("0.1234567890123456789"));
        assert_eq!(number("1E+3"), json!(1000.0));
    }

    #[test]
    fn attribute_values_as_cells() {
        assert_eq!(cell(&s("a")), json!("a"));
        assert_eq!(cell(&AttributeValue::Bool(true)), json!(true));
        assert_eq!(cell(&AttributeValue::Null(true)), Json::Null);
        assert_eq!(cell(&AttributeValue::B(Blob::new(vec![0xca, 0xfe]))), json!("0xCAFE"));
        assert_eq!(cell(&AttributeValue::L(vec![n("1"), s("x")])), json!("[1,\"x\"]"));
        let m = AttributeValue::M(HashMap::from([("b".into(), n("2")), ("a".into(), AttributeValue::Bool(false))]));
        assert_eq!(cell(&m), json!("{\"a\":false,\"b\":2}"));
        assert_eq!(cell(&AttributeValue::Ss(vec!["x".into(), "y".into()])), json!("[\"x\",\"y\"]"));
        assert_eq!(cell(&AttributeValue::Ns(vec!["1".into(), "2.5".into()])), json!("[1,2.5]"));
        assert_eq!(cell(&AttributeValue::Bs(vec![Blob::new(vec![1])])), json!("[\"0x01\"]"));
    }

    #[test]
    fn keys_come_first_then_attributes_by_appearance() {
        let items = vec![
            HashMap::from([("z".into(), s("1")), ("pk".into(), s("a")), ("b".into(), s("x"))]),
            HashMap::from([("pk".into(), s("b")), ("a".into(), s("y")), ("sk".into(), n("1"))]),
        ];
        assert_eq!(union_columns(&["pk".into(), "sk".into()], &items), vec!["pk", "sk", "b", "z", "a"]);
        let inferred = infer_columns(&["pk".into()], &items);
        let names: Vec<_> = inferred.iter().map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable)).collect();
        assert_eq!(names, vec![("a", "S", true), ("b", "S", true), ("sk", "N", true), ("z", "S", true)]);
    }

    #[test]
    fn table_after_from() {
        assert_eq!(from_table(r#"SELECT * FROM "Music" WHERE a = 1"#).as_deref(), Some("Music"));
        assert_eq!(from_table(r#"select a from "My""T"."idx""#).as_deref(), Some("My\"T"));
        assert_eq!(from_table("SELECT x FROM orders_2024").as_deref(), Some("orders_2024"));
        assert_eq!(from_table("SELECT 'from x' FROM t").as_deref(), Some("t"));
        assert_eq!(from_table("INSERT INTO t VALUE {'a': 1}"), None);
    }

    #[test]
    fn browse_queries_quote_names() {
        // A free function: a session needs an SDK client, whose HTTPS
        // client loads the TLS roots and a crypto provider when built.
        let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "Mu\"sic".into() };
        assert_eq!(browse(&t), r#"SELECT * FROM "Mu""sic""#);
        let i = ObjectRef { kind: kinds::INDEX.into(), schema: Some("T".into()), name: "byArtist".into() };
        assert_eq!(browse(&i), r#"SELECT * FROM "T"."byArtist""#);
    }
}

//! Table and index administration, which PartiQL can't express. `execute`
//! checks each statement here before sending it as PartiQL:
//!
//! ```text
//! CREATE TABLE [IF NOT EXISTS] "t" { …CreateTable input… }
//! DROP TABLE [IF EXISTS] "t"
//! CREATE INDEX "i" ON "t" { AttributeDefinitions, KeySchema, Projection?, ProvisionedThroughput? }
//! DROP INDEX "i" ON "t"
//! CREATE BACKUP "name" FOR TABLE "t"
//! RESTORE TABLE "new" FROM BACKUP "arn"
//! DROP BACKUP "arn"
//! ```
//!
//! The JSON body uses the API's own names (`AttributeDefinitions`,
//! `KeySchema`, `BillingMode`, `GlobalSecondaryIndexes`…), without
//! `TableName`. `TimeToLiveSpecification` is accepted there too: it isn't
//! part of CreateTable, so it's applied with UpdateTimeToLive once the table
//! is ACTIVE. CREATE waits (up to [`WAIT`]) until the table and its indexes
//! are ACTIVE, and DROP until the table is gone, so a script can use them
//! right after. `CREATE INDEX` adds a GSI (UpdateTable); LSIs only exist
//! with their table.

use crate::backup::backup_err;
use crate::err;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, CreateGlobalSecondaryIndexAction, DeleteGlobalSecondaryIndexAction,
    GlobalSecondaryIndex, GlobalSecondaryIndexUpdate, IndexStatus, KeySchemaElement, KeyType, LocalSecondaryIndex,
    Projection, ProjectionType, ProvisionedThroughput, ScalarAttributeType, StreamSpecification, StreamViewType,
    TableClass, TableStatus, Tag, TimeToLiveSpecification,
};
use aws_sdk_dynamodb::Client;
use dbine_driver::{Error, QueryOutcome, Result};
use serde_json::Value as Json;
use std::time::{Duration, Instant};

/// How long CREATE / DROP wait for the table to settle.
const WAIT: Duration = Duration::from_secs(120);

const USAGE: &str = "Sintaxis: CREATE TABLE [IF NOT EXISTS] \"t\" { …JSON de CreateTable… } · DROP TABLE [IF EXISTS] \"t\" · \
                     CREATE INDEX \"i\" ON \"t\" { …JSON… } · DROP INDEX \"i\" ON \"t\" · \
                     CREATE BACKUP \"nombre\" FOR TABLE \"t\" · RESTORE TABLE \"nueva\" FROM BACKUP \"arn\" · DROP BACKUP \"arn\"";

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Admin {
    CreateTable { table: String, if_not_exists: bool, spec: Json },
    DropTable { table: String, if_exists: bool },
    CreateIndex { index: String, table: String, spec: Json },
    DropIndex { index: String, table: String },
    /// CreateBackup (on-demand backup of `table`, see `backup`).
    CreateBackup { name: String, table: String },
    /// RestoreTableFromBackup into the new `table`.
    RestoreBackup { table: String, arn: String },
    /// DeleteBackup.
    DropBackup { arn: String },
}

struct Cursor<'a> {
    s: &'a str,
}

impl<'a> Cursor<'a> {
    fn ws(&mut self) {
        self.s = self.s.trim_start();
    }

    fn peek_word(&self) -> &'a str {
        let s = self.s.trim_start();
        let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
        &s[..end]
    }

    /// Consumes `kw` (any case) when it's the next word.
    fn keyword(&mut self, kw: &str) -> bool {
        let w = self.peek_word();
        if w.eq_ignore_ascii_case(kw) {
            self.ws();
            self.s = &self.s[w.len()..];
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kw: &str) -> Result<()> {
        if self.keyword(kw) {
            Ok(())
        } else {
            Err(usage())
        }
    }

    /// `"name"` (with `""` for a quote) or a bare name.
    fn ident(&mut self) -> Result<String> {
        self.ws();
        if let Some(rest) = self.s.strip_prefix('"') {
            let mut name = String::new();
            let mut chars = rest.char_indices().peekable();
            while let Some((i, c)) = chars.next() {
                if c == '"' {
                    if chars.peek().map(|p| p.1) == Some('"') {
                        chars.next();
                        name.push('"');
                    } else {
                        self.s = &rest[i + 1..];
                        return Ok(name);
                    }
                } else {
                    name.push(c);
                }
            }
            return Err(Error::Query("Falta cerrar las comillas del nombre.".into()));
        }
        let end = self.s.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))).unwrap_or(self.s.len());
        if end == 0 {
            return Err(usage());
        }
        let name = self.s[..end].to_string();
        self.s = &self.s[end..];
        Ok(name)
    }

    fn body(&mut self) -> Result<Json> {
        let text = self.s.trim();
        if text.is_empty() {
            return Err(Error::Query("Falta el cuerpo JSON después del nombre.".into()));
        }
        let v: Json =
            serde_json::from_str(text).map_err(|e| Error::Query(format!("El cuerpo no es un JSON válido: {e}")))?;
        if !v.is_object() {
            return Err(Error::Query("El cuerpo tiene que ser un objeto JSON ({ … }).".into()));
        }
        Ok(v)
    }

    fn end(&mut self) -> Result<()> {
        if self.s.trim().is_empty() {
            Ok(())
        } else {
            Err(usage())
        }
    }
}

fn usage() -> Error {
    Error::Query(USAGE.into())
}

/// The extension a statement is, or `None` for PartiQL.
pub(crate) fn parse_admin(stmt: &str) -> Result<Option<Admin>> {
    let mut c = Cursor { s: stmt };
    if c.keyword("restore") {
        c.expect("table")?;
        let table = c.ident()?;
        c.expect("from")?;
        c.expect("backup")?;
        let arn = c.ident()?;
        c.end()?;
        return Ok(Some(Admin::RestoreBackup { table, arn }));
    }
    let create = if c.keyword("create") {
        true
    } else if c.keyword("drop") {
        false
    } else {
        return Ok(None);
    };
    if c.keyword("backup") {
        let name = c.ident()?;
        if !create {
            c.end()?;
            return Ok(Some(Admin::DropBackup { arn: name }));
        }
        c.expect("for")?;
        c.expect("table")?;
        let table = c.ident()?;
        c.end()?;
        return Ok(Some(Admin::CreateBackup { name, table }));
    }
    if c.keyword("table") {
        if create {
            let if_not_exists = c.keyword("if");
            if if_not_exists {
                c.expect("not")?;
                c.expect("exists")?;
            }
            let table = c.ident()?;
            let spec = c.body()?;
            Ok(Some(Admin::CreateTable { table, if_not_exists, spec }))
        } else {
            let if_exists = c.keyword("if");
            if if_exists {
                c.expect("exists")?;
            }
            let table = c.ident()?;
            c.end()?;
            Ok(Some(Admin::DropTable { table, if_exists }))
        }
    } else if c.keyword("index") {
        let index = c.ident()?;
        c.expect("on")?;
        let table = c.ident()?;
        if create {
            let spec = c.body()?;
            Ok(Some(Admin::CreateIndex { index, table, spec }))
        } else {
            c.end()?;
            Ok(Some(Admin::DropIndex { index, table }))
        }
    } else {
        Err(usage())
    }
}

// ---- JSON body → SDK types ------------------------------------------------

fn build<T, E: std::fmt::Display>(r: std::result::Result<T, E>) -> Result<T> {
    r.map_err(|e| Error::Query(format!("Definición incompleta: {e}")))
}

fn field<'a>(v: &'a Json, key: &str) -> Result<&'a Json> {
    v.get(key).ok_or_else(|| Error::Query(format!("Falta \"{key}\" en {v}")))
}

fn text<'a>(v: &'a Json, key: &str) -> Result<&'a str> {
    field(v, key)?.as_str().ok_or_else(|| Error::Query(format!("\"{key}\" tiene que ser un texto")))
}

fn list<'a>(v: &'a Json, key: &str) -> Result<&'a Vec<Json>> {
    field(v, key)?.as_array().ok_or_else(|| Error::Query(format!("\"{key}\" tiene que ser una lista")))
}

fn int(v: &Json, key: &str) -> Result<i64> {
    field(v, key)?.as_i64().ok_or_else(|| Error::Query(format!("\"{key}\" tiene que ser un número entero")))
}

fn only_keys(v: &Json, allowed: &[&str], what: &str) -> Result<()> {
    if let Some(o) = v.as_object() {
        if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(Error::Query(format!("{what}: \"{k}\" no se reconoce. Se admiten: {}.", allowed.join(", "))));
        }
    }
    Ok(())
}

fn attribute_definitions(v: &Json) -> Result<Vec<AttributeDefinition>> {
    list(v, "AttributeDefinitions")?
        .iter()
        .map(|a| {
            build(
                AttributeDefinition::builder()
                    .attribute_name(text(a, "AttributeName")?)
                    .attribute_type(ScalarAttributeType::from(text(a, "AttributeType")?))
                    .build(),
            )
        })
        .collect()
}

fn key_schema(v: &Json) -> Result<Vec<KeySchemaElement>> {
    list(v, "KeySchema")?
        .iter()
        .map(|k| {
            build(
                KeySchemaElement::builder()
                    .attribute_name(text(k, "AttributeName")?)
                    .key_type(KeyType::from(text(k, "KeyType")?))
                    .build(),
            )
        })
        .collect()
}

/// `ALL` when not given.
fn projection(v: &Json) -> Result<Projection> {
    let Some(p) = v.get("Projection") else {
        return Ok(Projection::builder().projection_type(ProjectionType::All).build());
    };
    let mut b = Projection::builder().projection_type(ProjectionType::from(text(p, "ProjectionType")?));
    if let Some(attrs) = p.get("NonKeyAttributes").and_then(Json::as_array) {
        for a in attrs {
            b = b.non_key_attributes(a.as_str().unwrap_or_default());
        }
    }
    Ok(b.build())
}

fn throughput(v: &Json) -> Result<Option<ProvisionedThroughput>> {
    let Some(p) = v.get("ProvisionedThroughput") else { return Ok(None) };
    Ok(Some(build(
        ProvisionedThroughput::builder()
            .read_capacity_units(int(p, "ReadCapacityUnits")?)
            .write_capacity_units(int(p, "WriteCapacityUnits")?)
            .build(),
    )?))
}

fn ttl(v: &Json) -> Result<TimeToLiveSpecification> {
    build(
        TimeToLiveSpecification::builder()
            .attribute_name(text(v, "AttributeName")?)
            .enabled(v.get("Enabled").and_then(Json::as_bool).unwrap_or(true))
            .build(),
    )
}

/// A `CREATE TABLE` body, as SDK types.
#[derive(Debug)]
pub(crate) struct TableSpec {
    attributes: Vec<AttributeDefinition>,
    keys: Vec<KeySchemaElement>,
    billing: Option<BillingMode>,
    throughput: Option<ProvisionedThroughput>,
    gsis: Vec<GlobalSecondaryIndex>,
    lsis: Vec<LocalSecondaryIndex>,
    stream: Option<StreamSpecification>,
    class: Option<TableClass>,
    deletion_protection: Option<bool>,
    tags: Vec<Tag>,
    ttl: Option<TimeToLiveSpecification>,
}

const TABLE_KEYS: &[&str] = &[
    "AttributeDefinitions",
    "KeySchema",
    "BillingMode",
    "ProvisionedThroughput",
    "GlobalSecondaryIndexes",
    "LocalSecondaryIndexes",
    "StreamSpecification",
    "TableClass",
    "DeletionProtectionEnabled",
    "Tags",
    "TimeToLiveSpecification",
];

pub(crate) fn table_spec(spec: &Json) -> Result<TableSpec> {
    only_keys(spec, TABLE_KEYS, "CREATE TABLE")?;
    let empty = Vec::new();
    let arr = |k: &str| spec.get(k).and_then(Json::as_array).unwrap_or(&empty);
    let gsis = arr("GlobalSecondaryIndexes")
        .iter()
        .map(|g| {
            build(
                GlobalSecondaryIndex::builder()
                    .index_name(text(g, "IndexName")?)
                    .set_key_schema(Some(key_schema(g)?))
                    .projection(projection(g)?)
                    .set_provisioned_throughput(throughput(g)?)
                    .build(),
            )
        })
        .collect::<Result<_>>()?;
    let lsis = arr("LocalSecondaryIndexes")
        .iter()
        .map(|l| {
            build(
                LocalSecondaryIndex::builder()
                    .index_name(text(l, "IndexName")?)
                    .set_key_schema(Some(key_schema(l)?))
                    .projection(projection(l)?)
                    .build(),
            )
        })
        .collect::<Result<_>>()?;
    let stream = match spec.get("StreamSpecification") {
        None => None,
        Some(s) => Some(build(
            StreamSpecification::builder()
                .stream_enabled(s.get("StreamEnabled").and_then(Json::as_bool).unwrap_or(true))
                .set_stream_view_type(s.get("StreamViewType").and_then(Json::as_str).map(StreamViewType::from))
                .build(),
        )?),
    };
    let tags = arr("Tags")
        .iter()
        .map(|t| build(Tag::builder().key(text(t, "Key")?).value(text(t, "Value")?).build()))
        .collect::<Result<_>>()?;
    Ok(TableSpec {
        attributes: attribute_definitions(spec)?,
        keys: key_schema(spec)?,
        billing: spec.get("BillingMode").and_then(Json::as_str).map(BillingMode::from),
        throughput: throughput(spec)?,
        gsis,
        lsis,
        stream,
        class: spec.get("TableClass").and_then(Json::as_str).map(TableClass::from),
        deletion_protection: spec.get("DeletionProtectionEnabled").and_then(Json::as_bool),
        tags,
        ttl: spec.get("TimeToLiveSpecification").map(ttl).transpose()?,
    })
}

/// A `CREATE INDEX` body: the attribute definitions and the new GSI.
fn index_spec(index: &str, spec: &Json) -> Result<(Vec<AttributeDefinition>, CreateGlobalSecondaryIndexAction)> {
    only_keys(spec, &["AttributeDefinitions", "KeySchema", "Projection", "ProvisionedThroughput"], "CREATE INDEX")?;
    let attrs = match spec.get("AttributeDefinitions") {
        Some(_) => attribute_definitions(spec)?,
        None => Vec::new(),
    };
    let action = build(
        CreateGlobalSecondaryIndexAction::builder()
            .index_name(index)
            .set_key_schema(Some(key_schema(spec)?))
            .projection(projection(spec)?)
            .set_provisioned_throughput(throughput(spec)?)
            .build(),
    )?;
    Ok((attrs, action))
}

// ---- Running them ----------------------------------------------------------

fn is_not_found<E: ProvideErrorMetadata, R>(e: &SdkError<E, R>) -> bool {
    e.code() == Some("ResourceNotFoundException")
}

/// Waits until the table and its GSIs are ACTIVE (`Ok(false)` on timeout).
async fn wait_active(client: &Client, table: &str) -> Result<bool> {
    let started = Instant::now();
    loop {
        let d = client.describe_table().table_name(table).send().await.map_err(err)?;
        if let Some(t) = d.table() {
            let gsis_ready = t.global_secondary_indexes().iter().all(|g| g.index_status() == Some(&IndexStatus::Active));
            if t.table_status() == Some(&TableStatus::Active) && gsis_ready {
                return Ok(true);
            }
        }
        if started.elapsed() > WAIT {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Waits until DescribeTable no longer finds the table.
async fn wait_gone(client: &Client, table: &str) -> Result<bool> {
    let started = Instant::now();
    loop {
        match client.describe_table().table_name(table).send().await {
            Err(e) if is_not_found(&e) => return Ok(true),
            Err(e) => return Err(err(e)),
            Ok(_) => {}
        }
        if started.elapsed() > WAIT {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn still_pending(out: &mut QueryOutcome, table: &str, state: &str) {
    out.messages.push(format!(
        "La tabla {table} sigue {state} después de {} s; DynamoDB la termina en segundo plano.",
        WAIT.as_secs()
    ));
}

pub(crate) async fn run(client: &Client, admin: Admin, out: &mut QueryOutcome) -> Result<()> {
    match admin {
        Admin::CreateTable { table, if_not_exists, spec } => {
            let spec = table_spec(&spec)?;
            if if_not_exists {
                match client.describe_table().table_name(&table).send().await {
                    Ok(_) => {
                        out.messages.push(format!("La tabla {table} ya existe; no se creó."));
                        return Ok(());
                    }
                    Err(e) if is_not_found(&e) => {}
                    Err(e) => return Err(err(e)),
                }
            }
            client
                .create_table()
                .table_name(&table)
                .set_attribute_definitions(Some(spec.attributes))
                .set_key_schema(Some(spec.keys))
                .set_billing_mode(spec.billing)
                .set_provisioned_throughput(spec.throughput)
                .set_global_secondary_indexes((!spec.gsis.is_empty()).then_some(spec.gsis))
                .set_local_secondary_indexes((!spec.lsis.is_empty()).then_some(spec.lsis))
                .set_stream_specification(spec.stream)
                .set_table_class(spec.class)
                .set_deletion_protection_enabled(spec.deletion_protection)
                .set_tags((!spec.tags.is_empty()).then_some(spec.tags))
                .send()
                .await
                .map_err(err)?;
            if !wait_active(client, &table).await? {
                still_pending(out, &table, "creándose");
                if spec.ttl.is_some() {
                    out.messages.push("El TTL no se configuró: la tabla todavía no está activa.".into());
                }
                return Ok(());
            }
            if let Some(ttl) = spec.ttl {
                client.update_time_to_live().table_name(&table).time_to_live_specification(ttl).send().await.map_err(err)?;
            }
            out.messages.push(format!("Tabla {table} creada."));
        }
        Admin::DropTable { table, if_exists } => {
            match client.delete_table().table_name(&table).send().await {
                Ok(_) => {}
                Err(e) if if_exists && is_not_found(&e) => {
                    out.messages.push(format!("La tabla {table} no existe; no se borró nada."));
                    return Ok(());
                }
                Err(e) => return Err(err(e)),
            }
            if wait_gone(client, &table).await? {
                out.messages.push(format!("Tabla {table} borrada."));
            } else {
                still_pending(out, &table, "borrándose");
            }
        }
        Admin::CreateIndex { index, table, spec } => {
            let (attrs, action) = index_spec(&index, &spec)?;
            client
                .update_table()
                .table_name(&table)
                .set_attribute_definitions((!attrs.is_empty()).then_some(attrs))
                .global_secondary_index_updates(GlobalSecondaryIndexUpdate::builder().create(action).build())
                .send()
                .await
                .map_err(err)?;
            if wait_active(client, &table).await? {
                out.messages.push(format!("Índice {index} creado en {table}."));
            } else {
                still_pending(out, &table, "creando el índice");
            }
        }
        Admin::DropIndex { index, table } => {
            let action = build(DeleteGlobalSecondaryIndexAction::builder().index_name(&index).build())?;
            client
                .update_table()
                .table_name(&table)
                .global_secondary_index_updates(GlobalSecondaryIndexUpdate::builder().delete(action).build())
                .send()
                .await
                .map_err(err)?;
            if wait_active(client, &table).await? {
                out.messages.push(format!("Índice {index} borrado de {table}."));
            } else {
                still_pending(out, &table, "borrando el índice");
            }
        }
        Admin::CreateBackup { name, table } => {
            let r = client.create_backup().table_name(&table).backup_name(&name).send().await.map_err(backup_err)?;
            let arn = r.backup_details().map(|d| d.backup_arn()).unwrap_or_default();
            out.messages.push(format!("Backup {name} de {table} creado: {arn}"));
        }
        Admin::RestoreBackup { table, arn } => {
            client
                .restore_table_from_backup()
                .target_table_name(&table)
                .backup_arn(&arn)
                .send()
                .await
                .map_err(backup_err)?;
            // Restores take minutes even for small tables: not waited for.
            out.messages.push(format!(
                "Restaurando el backup en la tabla nueva {table}; DynamoDB la termina en segundo plano y queda ACTIVE al terminar."
            ));
        }
        Admin::DropBackup { arn } => {
            client.delete_backup().backup_arn(&arn).send().await.map_err(backup_err)?;
            out.messages.push(format!("Backup {arn} borrado."));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn statements() {
        assert_eq!(parse_admin("SELECT * FROM t").unwrap(), None);
        assert_eq!(parse_admin("INSERT INTO t VALUE {'a': 1}").unwrap(), None);
        assert_eq!(
            parse_admin("drop table if exists \"My\"\"T\"").unwrap(),
            Some(Admin::DropTable { table: "My\"T".into(), if_exists: true })
        );
        assert_eq!(
            parse_admin("DROP INDEX byX ON orders").unwrap(),
            Some(Admin::DropIndex { index: "byX".into(), table: "orders".into() })
        );
        assert_eq!(
            parse_admin("create table t {\"KeySchema\": []}").unwrap(),
            Some(Admin::CreateTable { table: "t".into(), if_not_exists: false, spec: json!({ "KeySchema": [] }) })
        );
        assert!(parse_admin("CREATE TABLE t").is_err());
        assert!(parse_admin("CREATE TABLE t [1]").is_err());
        assert!(parse_admin("CREATE VIEW v").is_err());
        assert!(parse_admin("DROP TABLE t extra").is_err());
        assert_eq!(
            parse_admin("create backup b1 for table \"T\"").unwrap(),
            Some(Admin::CreateBackup { name: "b1".into(), table: "T".into() })
        );
        assert_eq!(
            parse_admin("RESTORE TABLE t2 FROM BACKUP \"arn:aws:dynamodb:r:1:table/t/backup/x\"").unwrap(),
            Some(Admin::RestoreBackup { table: "t2".into(), arn: "arn:aws:dynamodb:r:1:table/t/backup/x".into() })
        );
        assert!(parse_admin("CREATE BACKUP b1").is_err());
        assert!(parse_admin("DROP BACKUP \"a\" extra").is_err());
        assert!(parse_admin("RESTORE t FROM BACKUP \"a\"").is_err());
    }

    #[test]
    fn bodies_map_to_sdk_types() {
        let spec = json!({
            "AttributeDefinitions": [{ "AttributeName": "pk", "AttributeType": "S" }, { "AttributeName": "g", "AttributeType": "N" }],
            "KeySchema": [{ "AttributeName": "pk", "KeyType": "HASH" }],
            "BillingMode": "PROVISIONED",
            "ProvisionedThroughput": { "ReadCapacityUnits": 2, "WriteCapacityUnits": 3 },
            "GlobalSecondaryIndexes": [{ "IndexName": "byG", "KeySchema": [{ "AttributeName": "g", "KeyType": "HASH" }],
                "ProvisionedThroughput": { "ReadCapacityUnits": 1, "WriteCapacityUnits": 1 } }],
            "TimeToLiveSpecification": { "AttributeName": "exp" },
            "Tags": [{ "Key": "k", "Value": "v" }]
        });
        let t = table_spec(&spec).unwrap();
        assert_eq!(t.attributes.len(), 2);
        assert_eq!(t.gsis[0].projection().and_then(|p| p.projection_type()), Some(&ProjectionType::All));
        assert_eq!(t.throughput.as_ref().map(|p| p.read_capacity_units()), Some(2));
        assert_eq!(t.ttl.as_ref().map(|t| t.enabled()), Some(true));
        let e = table_spec(&json!({ "KeySchema": [], "AttributeDefinitions": [], "TableName": "x" })).unwrap_err();
        assert!(e.to_string().contains("TableName"), "{e}");
        assert!(table_spec(&json!({ "KeySchema": [{ "AttributeName": "pk" }], "AttributeDefinitions": [] })).is_err());
    }
}

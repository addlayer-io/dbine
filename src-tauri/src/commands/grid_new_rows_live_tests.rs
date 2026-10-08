//! "Agregar fila" / "Agregar documento" against real servers: the grid's new
//! rows go through `new_rows_parts`, are joined with the engine's script
//! separator and run the way the editor runs a script (one unit at a time on
//! engines that split, the whole text on the others), then read back.
//!
//! Ignored by default; the servers are the `dbine-test-*` containers on their
//! usual ports (each URL can be replaced with `DBINE_TEST_<ENGINE>_URL`, only
//! `host:port` and credentials are read from it):
//!
//! ```sh
//! SSL_CERT_FILE=/etc/ssl/cert.pem cargo test -p dbine --lib -- --ignored grid_new_rows --test-threads=1 --nocapture
//! ```
//!
//! Every test creates `dbine_e2e_newrows` (a table, collection, database or
//! index) and drops it at the end, also when an assertion failed.

use super::data_compare::new_rows_parts;
use dbine_driver::sql::StatementKind;
use dbine_driver::sql::ScriptMode;
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use serde_json::{json, Value};
use std::sync::Arc;

const NAME: &str = "dbine_e2e_newrows";

type Row = Vec<(String, Value)>;
type Res<T> = Result<T, String>;

fn row(cells: &[(&str, Value)]) -> Row {
    cells.iter().map(|(n, v)| (n.to_string(), v.clone())).collect()
}

macro_rules! ensure {
    ($cond:expr, $($msg:tt)+) => {
        if !$cond {
            return Err(format!($($msg)+));
        }
    };
}

/// A cell as text, whatever the driver made of it.
fn txt(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

/// `user:pass@host:port` out of a URL (after an optional scheme).
fn parse(url: &str) -> (Option<String>, Option<String>, String, u16) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').map_or(("", rest), |(a, h)| (a, h));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, "0"), |(h, p)| (h, p));
    ((!user.is_empty()).then(|| user.to_string()), pass.map(str::to_string), host.to_string(), port.parse().unwrap_or(0))
}

fn base_cfg(engine: &str, env: &str, default_url: &str) -> ConnectionConfig {
    let url = std::env::var(env).unwrap_or_else(|_| default_url.to_string());
    let (username, password, host, port) = parse(&url);
    ConnectionConfig { driver: engine.into(), host, port, username, password, ..Default::default() }
}

async fn open(cfg: &ConnectionConfig, database: Option<&str>) -> (Arc<dyn Driver>, Box<dyn Session>) {
    let d = dbine_drivers::find(&cfg.driver).unwrap_or_else(|| panic!("no driver {}", cfg.driver)).clone();
    let s = d.connect(cfg, database).await.unwrap_or_else(|e| panic!("connect {}: {e}", cfg.driver));
    (d, s)
}

/// One `execute`, an error recorded in the outcome counting as an error.
async fn exec(s: &mut dyn Session, text: &str) -> Res<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 1000, &mut out).await.map_err(|e| format!("{e}\n--- {text}"))?;
    if let Some(e) = out.error.take() {
        return Err(format!("{e}\n--- {text}"));
    }
    if let Some(e) = out.errors.first() {
        return Err(format!("{}\n--- {text}", e.message));
    }
    Ok(out)
}

/// A script the way the editor runs it: unit by unit on the engines that
/// split it, in one call on the others.
async fn apply(d: &dyn Driver, s: &mut dyn Session, script: &str) -> Res<()> {
    if d.script_mode() == ScriptMode::Whole {
        exec(s, script).await?;
        return Ok(());
    }
    for unit in d.split_script(script) {
        if unit.kind == StatementKind::ClientCommand || unit.text.trim().is_empty() {
            continue;
        }
        exec(s, &unit.text).await?;
    }
    Ok(())
}

/// The first result's rows.
async fn rows(s: &mut dyn Session, sql: &str) -> Res<Vec<Vec<Value>>> {
    let out = exec(s, sql).await?;
    Ok(out.results.into_iter().next().map(|r| r.rows).unwrap_or_default())
}

fn join(d: &dyn Driver, parts: &[String]) -> String {
    parts.iter().filter(|p| !p.trim().is_empty()).cloned().collect::<Vec<_>>().join(&format!("\n{}\n", d.script_separator()))
}

/// What differs between the SQL engines.
struct SqlCase {
    engine: &'static str,
    cfg: ConnectionConfig,
    database: Option<&'static str>,
    schema: Option<&'static str>,
    create: &'static str,
    drop: &'static str,
    select: &'static str,
    /// Also an explicit id with the identity list (SQL Server, PostgreSQL).
    explicit_id: bool,
}

async fn sql_scenario(c: &SqlCase, d: &dyn Driver, s: &mut dyn Session) -> Res<()> {
    let target = ObjectRef { kind: "table".into(), schema: c.schema.map(str::to_string), name: NAME.into() };
    let identity = vec!["id".to_string()];
    exec(s, c.create).await?;

    // The probe the UI sends first.
    let probe = new_rows_parts(d, &target, &[], &identity, &["id".into(), "nombre".into(), "estado".into()]).map_err(|e| format!("probe: {e:?}"))?;
    ensure!(probe.is_empty(), "probe returned {probe:?}");

    // (a) two rows setting only `nombre`.
    let new = vec![row(&[("nombre", json!("Ana"))]), row(&[("nombre", json!("Luis"))])];
    let parts = new_rows_parts(d, &target, &new, &identity, &[]).map_err(|e| format!("parts: {e:?}"))?;
    let script = join(d, &parts);
    eprintln!("[{}] (a) script:\n{script}", c.engine);
    apply(d, s, &script).await?;
    let got = rows(s, c.select).await?;
    eprintln!("[{}] (a) rows: {got:?}", c.engine);
    ensure!(got.len() == 2, "expected 2 rows, got {got:?}");
    let ids: Vec<String> = got.iter().map(|r| txt(&r[0])).collect();
    ensure!(ids.iter().all(|i| !i.is_empty() && i != "null") && ids[0] != ids[1], "ids not generated: {ids:?}");
    ensure!(got.iter().map(|r| txt(&r[1])).collect::<Vec<_>>() == ["Ana", "Luis"], "nombres: {got:?}");
    ensure!(got.iter().all(|r| txt(&r[2]) == "nuevo"), "default not applied: {got:?}");

    // (b) one row with an explicit id, then another without one.
    if c.explicit_id {
        let new = vec![row(&[("id", json!(100)), ("nombre", json!("Eva"))])];
        let parts = new_rows_parts(d, &target, &new, &identity, &[]).map_err(|e| format!("parts b: {e:?}"))?;
        ensure!(parts.len() >= 2, "explicit id should be wrapped: {parts:?}");
        let script = join(d, &parts);
        eprintln!("[{}] (b) script:\n{script}", c.engine);
        apply(d, s, &script).await?;
        let new = vec![row(&[("nombre", json!("Zoe"))])];
        let parts = new_rows_parts(d, &target, &new, &identity, &[]).map_err(|e| format!("parts c: {e:?}"))?;
        apply(d, s, &join(d, &parts)).await?;
        let got = rows(s, c.select).await?;
        eprintln!("[{}] (b) rows: {got:?}", c.engine);
        ensure!(got.len() == 4, "expected 4 rows, got {got:?}");
        let eva = got.iter().find(|r| txt(&r[1]) == "Eva").ok_or("no Eva")?;
        ensure!(txt(&eva[0]) == "100" && txt(&eva[2]) == "nuevo", "Eva: {eva:?}");
        let zoe = got.iter().find(|r| txt(&r[1]) == "Zoe").ok_or("no Zoe")?;
        ensure!(txt(&zoe[0]).parse::<i64>().map_err(|e| e.to_string())? > 100, "next id after the explicit one: {zoe:?}");
    }
    Ok(())
}

async fn run_sql(c: SqlCase) {
    let (d, mut s) = open(&c.cfg, c.database).await;
    let _ = exec(s.as_mut(), c.drop).await;
    let r = sql_scenario(&c, d.as_ref(), s.as_mut()).await;
    let cleanup = exec(s.as_mut(), c.drop).await;
    if let Err(e) = r {
        panic!("{}: {e}", c.engine);
    }
    cleanup.unwrap_or_else(|e| panic!("{}: cleanup: {e}", c.engine));
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_sqlserver() {
    let mut cfg = base_cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL", "mssql://sa:Pw_12345!@localhost:25013");
    cfg.trust_server_certificate = true;
    run_sql(SqlCase {
        engine: "sqlserver",
        cfg,
        database: None,
        schema: Some("dbo"),
        create: "CREATE TABLE dbo.dbine_e2e_newrows (id int IDENTITY(1,1) PRIMARY KEY, nombre nvarchar(50) NOT NULL, estado nvarchar(20) NOT NULL DEFAULT 'nuevo')",
        drop: "IF OBJECT_ID('dbo.dbine_e2e_newrows') IS NOT NULL DROP TABLE dbo.dbine_e2e_newrows",
        select: "SELECT id, nombre, estado FROM dbo.dbine_e2e_newrows ORDER BY id",
        explicit_id: true,
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_postgres() {
    let mut cfg = base_cfg("postgres", "DBINE_TEST_POSTGRES_URL", "postgres://postgres:pw@localhost:25010");
    cfg.database = "postgres".into();
    run_sql(SqlCase {
        engine: "postgres",
        cfg,
        database: None,
        schema: Some("public"),
        create: "CREATE TABLE public.dbine_e2e_newrows (id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, nombre text NOT NULL, estado text NOT NULL DEFAULT 'nuevo')",
        drop: "DROP TABLE IF EXISTS public.dbine_e2e_newrows",
        select: "SELECT id, nombre, estado FROM public.dbine_e2e_newrows ORDER BY id",
        explicit_id: true,
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_cockroach() {
    let mut cfg = base_cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL", "postgres://root@localhost:26014");
    cfg.database = "defaultdb".into();
    run_sql(SqlCase {
        engine: "cockroachdb",
        cfg,
        database: None,
        schema: Some("public"),
        create: "CREATE TABLE public.dbine_e2e_newrows (id INT8 PRIMARY KEY DEFAULT unique_rowid(), nombre STRING NOT NULL, estado STRING NOT NULL DEFAULT 'nuevo')",
        drop: "DROP TABLE IF EXISTS public.dbine_e2e_newrows",
        select: "SELECT id, nombre, estado FROM public.dbine_e2e_newrows ORDER BY nombre",
        explicit_id: false,
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_oracle() {
    let mut cfg = base_cfg("oracle", "DBINE_TEST_ORACLE_URL", "oracle://system:Secret123@localhost:25601");
    cfg.options.insert("service".into(), "FREEPDB1".into());
    run_sql(SqlCase {
        engine: "oracle",
        cfg,
        database: None,
        schema: Some("SYSTEM"),
        create: "CREATE TABLE SYSTEM.\"dbine_e2e_newrows\" (\"id\" NUMBER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, \"nombre\" VARCHAR2(50) NOT NULL, \"estado\" VARCHAR2(20) DEFAULT 'nuevo' NOT NULL)",
        drop: "BEGIN EXECUTE IMMEDIATE 'DROP TABLE SYSTEM.\"dbine_e2e_newrows\" PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",
        select: "SELECT \"id\", \"nombre\", \"estado\" FROM SYSTEM.\"dbine_e2e_newrows\" ORDER BY \"id\"",
        explicit_id: false,
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_firebird() {
    let mut cfg = base_cfg("firebird", "DBINE_TEST_FIREBIRD_URL", "firebird://dbine:dbine@localhost:25602");
    cfg.database = "/var/lib/firebird/data/test.fdb".into();
    run_sql(SqlCase {
        engine: "firebird",
        cfg,
        database: None,
        schema: None,
        create: "CREATE TABLE \"dbine_e2e_newrows\" (\"id\" INTEGER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, \"nombre\" VARCHAR(50) NOT NULL, \"estado\" VARCHAR(20) DEFAULT 'nuevo' NOT NULL)",
        drop: "EXECUTE BLOCK AS BEGIN IF (EXISTS(SELECT 1 FROM RDB$RELATIONS WHERE RDB$RELATION_NAME = 'dbine_e2e_newrows')) THEN EXECUTE STATEMENT 'DROP TABLE \"dbine_e2e_newrows\"'; END",
        select: "SELECT \"id\", \"nombre\", \"estado\" FROM \"dbine_e2e_newrows\" ORDER BY \"id\"",
        explicit_id: false,
    })
    .await;
}

// ---- Document engines ----------------------------------------------------

/// A document with a nested object and an array, columns in this order.
fn nested_doc() -> Row {
    row(&[("nombre", json!("Ana")), ("cliente", json!({"ciudad": "Rosario", "geo": {"lat": -32.9, "lng": -60.6}})), ("items", json!([1, 2, {"sku": "x"}]))])
}

/// The first result as JSON documents when it has one `Value` cell per row,
/// else the rows as objects by column name.
async fn docs(s: &mut dyn Session, q: &str) -> Res<Vec<Value>> {
    let out = exec(s, q).await?;
    let r = out.results.into_iter().next().ok_or("no result")?;
    Ok(r.rows
        .iter()
        .map(|cells| {
            let m: serde_json::Map<String, Value> = r.columns.iter().zip(cells).map(|(c, v)| (c.name.clone(), v.clone())).collect();
            Value::Object(m)
        })
        .collect())
}

/// A cell that may hold the nested value as JSON text.
fn as_json(v: &Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| v.clone()),
        other => other.clone(),
    }
}

async fn mongo_scenario(d: &dyn Driver, s: &mut dyn Session, engine: &str) -> Res<()> {
    let target = ObjectRef { kind: "collection".into(), schema: None, name: NAME.into() };
    let probe = new_rows_parts(d, &target, &[], &[], &[]).map_err(|e| format!("probe: {e:?}"))?;
    ensure!(probe.is_empty(), "probe returned {probe:?}");
    let parts = new_rows_parts(d, &target, &[nested_doc()], &[], &[]).map_err(|e| format!("parts: {e:?}"))?;
    let script = join(d, &parts);
    eprintln!("[{engine}] script:\n{script}");
    apply(d, s, &script).await?;
    let got = docs(s, &format!("db.getCollection(\"{NAME}\").find({{}})")).await?;
    eprintln!("[{engine}] read back: {got:?}");
    ensure!(got.len() == 1, "expected 1 document, got {got:?}");
    let doc = &got[0];
    ensure!(txt(&doc["nombre"]).trim_matches('"') == "Ana", "nombre: {doc}");
    ensure!(as_json(&doc["cliente"]) == json!({"ciudad": "Rosario", "geo": {"lat": -32.9, "lng": -60.6}}), "nested object lost: {}", doc["cliente"]);
    ensure!(as_json(&doc["items"]) == json!([1, 2, {"sku": "x"}]), "array lost: {}", doc["items"]);
    ensure!(!txt(&doc["_id"]).is_empty() && !doc["_id"].is_null(), "no _id: {doc}");
    Ok(())
}

async fn run_mongo(engine: &'static str, env: &str, default_url: &str) {
    let (_, _, host, port) = parse(&std::env::var(env).unwrap_or_else(|_| default_url.to_string()));
    let _ = (host, port);
    let url = std::env::var(env).unwrap_or_else(|_| default_url.to_string());
    let mut cfg = ConnectionConfig { driver: engine.into(), database: "dbine_e2e_newrows_db".into(), ..Default::default() };
    cfg.options.insert("connection_string".into(), url);
    let (d, mut s) = open(&cfg, None).await;
    let _ = exec(s.as_mut(), &format!("db.getCollection(\"{NAME}\").drop()")).await;
    let r = mongo_scenario(d.as_ref(), s.as_mut(), engine).await;
    let c1 = exec(s.as_mut(), &format!("db.getCollection(\"{NAME}\").drop()")).await;
    let c2 = s.drop_database("dbine_e2e_newrows_db").await;
    if let Err(e) = r {
        panic!("{engine}: {e}");
    }
    c1.unwrap_or_else(|e| panic!("{engine}: cleanup: {e}"));
    let _ = c2;
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_mongodb() {
    run_mongo("mongodb", "DBINE_TEST_MONGODB_URL", "mongodb://root:secret@localhost:25201/?authSource=admin").await;
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_ferretdb() {
    run_mongo("ferretdb", "DBINE_TEST_FERRETDB_URL", "mongodb://root:secret@localhost:25203/").await;
}

async fn couch_scenario(d: &dyn Driver, s: &mut dyn Session) -> Res<()> {
    let target = ObjectRef { kind: "collection".into(), schema: None, name: NAME.into() };
    let probe = new_rows_parts(d, &target, &[], &[], &[]).map_err(|e| format!("probe: {e:?}"))?;
    ensure!(probe.is_empty(), "probe returned {probe:?}");
    let parts = new_rows_parts(d, &target, &[nested_doc()], &[], &[]).map_err(|e| format!("parts: {e:?}"))?;
    let script = join(d, &parts);
    eprintln!("[couchdb] script:\n{script}");
    apply(d, s, &script).await?;
    let got = docs(s, "GET _all_docs?include_docs=true").await?;
    eprintln!("[couchdb] read back: {got:?}");
    ensure!(got.len() == 1, "expected 1 document, got {got:?}");
    let doc = &got[0];
    ensure!(txt(&doc["nombre"]) == "Ana", "nombre: {doc}");
    ensure!(as_json(&doc["cliente"]) == json!({"ciudad": "Rosario", "geo": {"lat": -32.9, "lng": -60.6}}), "nested object lost: {}", doc["cliente"]);
    ensure!(as_json(&doc["items"]) == json!([1, 2, {"sku": "x"}]), "array lost: {}", doc["items"]);
    ensure!(!txt(&doc["_id"]).is_empty(), "no _id: {doc}");
    Ok(())
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_couchdb() {
    let url = "http://admin:secret@localhost:25202";
    let mut cfg = base_cfg("couchdb", "DBINE_TEST_COUCHDB_URL", url);
    cfg.database = NAME.into();
    let (d, mut s) = open(&cfg, Some("")).await;
    let _ = s.drop_database(NAME).await;
    s.create_database(NAME).await.unwrap_or_else(|e| panic!("couchdb create: {e}"));
    let (_, mut s2) = open(&cfg, Some(NAME)).await;
    let r = couch_scenario(d.as_ref(), s2.as_mut()).await;
    let c = s.drop_database(NAME).await;
    if let Err(e) = r {
        panic!("couchdb: {e}");
    }
    c.unwrap_or_else(|e| panic!("couchdb cleanup: {e}"));
}

// ---- Key-value and search ------------------------------------------------

#[tokio::test]
#[ignore]
async fn grid_new_rows_redis() {
    let cfg = base_cfg("redis", "DBINE_TEST_REDIS_URL", "redis://localhost:25498");
    let (d, mut s) = open(&cfg, Some("db5")).await;
    let target = ObjectRef { kind: "key".into(), schema: None, name: NAME.into() };
    let del = format!("DEL {NAME}:1 {NAME}:2");
    let _ = exec(s.as_mut(), &del).await;
    let r: Res<()> = async {
        let probe = new_rows_parts(d.as_ref(), &target, &[], &[], &["key".into(), "value".into()]).map_err(|e| format!("probe: {e:?}"))?;
        ensure!(probe.is_empty(), "probe returned {probe:?}");
        let new = vec![row(&[("id", json!(1)), ("nombre", json!("Ana")), ("estado", json!("nuevo"))]), row(&[("id", json!(2)), ("nombre", json!("Luis")), ("estado", json!("nuevo"))])];
        let parts = new_rows_parts(d.as_ref(), &target, &new, &[], &[]).map_err(|e| format!("parts: {e:?}"))?;
        let script = join(d.as_ref(), &parts);
        eprintln!("[redis] script:\n{script}");
        apply(d.as_ref(), s.as_mut(), &script).await?;
        let a = rows(s.as_mut(), &format!("HGETALL {NAME}:1")).await?;
        let b = rows(s.as_mut(), &format!("HGETALL {NAME}:2")).await?;
        eprintln!("[redis] read back: {a:?} {b:?}");
        let flat = |r: &Vec<Vec<Value>>| r.iter().flatten().map(txt).collect::<Vec<_>>().join(" ");
        let (fa, fb) = (flat(&a), flat(&b));
        ensure!(fa.contains("Ana") && fa.contains("nuevo"), "key 1: {fa}");
        ensure!(fb.contains("Luis") && fb.contains("nuevo"), "key 2: {fb}");
        Ok(())
    }
    .await;
    let c = exec(s.as_mut(), &del).await;
    if let Err(e) = r {
        panic!("redis: {e}");
    }
    c.unwrap_or_else(|e| panic!("redis cleanup: {e}"));
}

#[tokio::test]
#[ignore]
async fn grid_new_rows_elasticsearch() {
    let url = std::env::var("DBINE_TEST_ELASTICSEARCH_URL").unwrap_or_else(|_| "http://localhost:25496".into());
    let cfg = ConnectionConfig { driver: "elasticsearch".into(), host: url, ..Default::default() };
    let (d, mut s) = open(&cfg, None).await;
    let target = ObjectRef { kind: "index".into(), schema: None, name: NAME.into() };
    let _ = exec(s.as_mut(), &format!("DELETE /{NAME}")).await;
    let r: Res<()> = async {
        exec(s.as_mut(), &format!("PUT /{NAME}")).await?;
        let probe = new_rows_parts(d.as_ref(), &target, &[], &[], &["_id".into(), "nombre".into()]).map_err(|e| format!("probe: {e:?}"))?;
        ensure!(probe.is_empty(), "probe returned {probe:?}");
        let new = vec![
            row(&[("_id", json!("a1")), ("nombre", json!("Ana")), ("cliente", json!({"ciudad": "Rosario"})), ("items", json!([1, 2]))]),
            row(&[("nombre", json!("Luis"))]),
        ];
        let parts = new_rows_parts(d.as_ref(), &target, &new, &[], &[]).map_err(|e| format!("parts: {e:?}"))?;
        let script = join(d.as_ref(), &parts);
        eprintln!("[elasticsearch] script:\n{script}");
        apply(d.as_ref(), s.as_mut(), &script).await?;
        let got = docs(s.as_mut(), &format!("GET /{NAME}/_search?sort=nombre.keyword:asc")).await?;
        eprintln!("[elasticsearch] read back: {got:?}");
        ensure!(got.len() == 2, "expected 2 hits, got {got:?}");
        let ana = got.iter().find(|g| g.to_string().contains("Ana")).ok_or("no Ana")?;
        ensure!(txt(&ana["_id"]).contains("a1"), "explicit _id lost: {ana}");
        ensure!(ana.to_string().contains("Rosario"), "nested lost: {ana}");
        let luis = got.iter().find(|g| g.to_string().contains("Luis")).ok_or("no Luis")?;
        ensure!(!txt(&luis["_id"]).is_empty(), "generated _id: {luis}");
        Ok(())
    }
    .await;
    let c = exec(s.as_mut(), &format!("DELETE /{NAME}")).await;
    if let Err(e) = r {
        panic!("elasticsearch: {e}");
    }
    c.unwrap_or_else(|e| panic!("elasticsearch cleanup: {e}"));
}

//! CockroachDB's index access methods by PostgreSQL's names: 26.x reports
//! `prefix` (its ordered index) and `inverted` (its GIN), read as `btree` and
//! `gin`. The same table on CockroachDB and PostgreSQL reads the same
//! indexes, the sync script recreates them on CockroachDB, and each side's
//! structure makes the other's indexes.
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test cockroach_index_kinds -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, DdlParts, Driver, QueryOutcome, Session, TableChange, TableSchema};
use std::sync::Arc;

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
    if let Some(e) = out.error.take() {
        panic!("{e}\n---\n{sql}");
    }
}

async fn run_all(s: &mut Box<dyn Session>, script: &str) {
    for stmt in script.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
        run(s, stmt).await;
    }
}

const DB: &str = "dbine_crdb_kinds";

const CRDB: &str = "
CREATE SCHEMA app;
CREATE TABLE app.t (id INT8 PRIMARY KEY, a INT8, j JSONB);
CREATE INDEX t_a_ix ON app.t (a);
CREATE INVERTED INDEX t_j_gin ON app.t (j);
";

const PG: &str = "
CREATE SCHEMA app;
CREATE TABLE app.t (id INT8 PRIMARY KEY, a INT8, j JSONB);
CREATE INDEX t_a_ix ON app.t (a);
CREATE INDEX t_j_gin ON app.t USING gin (j);
";

struct Side {
    d: Arc<dyn Driver>,
    admin: Box<dyn Session>,
    s: Box<dyn Session>,
}

async fn side(id: &str, url: &str, setup: &str) -> Side {
    let d = driver(id);
    let cfg = parse_url(id, url);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.expect("create_database");
    let mut s = d.connect(&cfg, Some(DB)).await.expect("connect db");
    run_all(&mut s, setup).await;
    Side { d, admin, s }
}

async fn table(s: &mut Box<dyn Session>, schema: &str) -> TableSchema {
    let mut t = s
        .database_schema()
        .await
        .expect("database_schema")
        .into_iter()
        .find(|t| t.schema.as_deref() == Some(schema) && t.name == "t")
        .unwrap_or_else(|| panic!("{schema}.t"));
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

fn kinds(t: &TableSchema) -> Vec<(&str, Option<&str>)> {
    t.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_deref())).collect()
}

const WANT: [(&str, Option<&str>); 2] = [("t_a_ix", Some("btree")), ("t_j_gin", Some("gin"))];

/// `t` as `schema.t`, made with `d`'s DDL (table, then indexes).
async fn make(d: &dyn Driver, s: &mut Box<dyn Session>, t: &TableSchema, schema: &str) {
    let mut t = t.clone();
    t.schema = Some(schema.into());
    run(s, &format!("CREATE SCHEMA {schema}")).await;
    let create = d.table_ddl(&t, DdlParts { create: true, ..Default::default() }).expect("create ddl");
    let ix = d.table_ddl(&t, DdlParts { indexes: true, ..Default::default() }).expect("index ddl");
    eprintln!("---- {} {schema}\n{create}\n{ix}", d.info().id);
    run_all(s, &create.replace(";\n\n", ";\n")).await;
    run_all(s, &ix).await;
}

#[tokio::test]
#[ignore]
async fn cockroach_and_postgres() {
    let (Ok(crdb_url), Ok(pg_url)) = (std::env::var("DBINE_TEST_COCKROACH_URL"), std::env::var("DBINE_TEST_POSTGRES_URL")) else {
        eprintln!("DBINE_TEST_COCKROACH_URL / DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let mut c = side("cockroachdb", &crdb_url, CRDB).await;
    let mut p = side("postgres", &pg_url, PG).await;

    // The explorer's read.
    let ct = table(&mut c.s, "app").await;
    let pt = table(&mut p.s, "app").await;
    eprintln!("cockroach: {:#?}\npostgres: {:#?}", ct.indexes, pt.indexes);
    assert_eq!(kinds(&ct), WANT);
    assert_eq!(kinds(&pt), WANT);
    // The compare: the same indexes on both.
    assert_eq!(ct.indexes, pt.indexes);

    // The sync script recreates both on Cockroach and the re-read matches.
    let mut bare = ct.clone();
    bare.indexes.clear();
    run(&mut c.s, "DROP INDEX app.t@t_a_ix").await;
    run(&mut c.s, "DROP INDEX app.t@t_j_gin").await;
    assert!(table(&mut c.s, "app").await.indexes.is_empty());
    let script = c.d.sync_script(&[TableChange::Alter { old: bare, new: ct.clone() }]).expect("sync_script");
    eprintln!("---- sync\n{}", script.statements.join("\n"));
    let lines: Vec<&str> = script.statements.iter().flat_map(|s| s.lines()).collect();
    assert!(lines.contains(&"CREATE INDEX \"t_a_ix\" ON \"app\".\"t\" (\"a\");"), "{lines:?}");
    assert!(lines.contains(&"CREATE INDEX \"t_j_gin\" ON \"app\".\"t\" USING GIN (\"j\");"), "{lines:?}");
    for s in &script.statements {
        run_all(&mut c.s, s).await;
    }
    assert_eq!(table(&mut c.s, "app").await, ct);

    // Cockroach → PostgreSQL and PostgreSQL → Cockroach keep both indexes.
    make(p.d.as_ref(), &mut p.s, &ct, "from_crdb").await;
    assert_eq!(kinds(&table(&mut p.s, "from_crdb").await), WANT);
    make(c.d.as_ref(), &mut c.s, &pt, "from_pg").await;
    assert_eq!(kinds(&table(&mut c.s, "from_pg").await), WANT);

    // A snapshot with Cockroach's raw names still makes them on PostgreSQL.
    let mut raw = ct.clone();
    raw.indexes[0].kind = Some("prefix".into());
    raw.indexes[1].kind = Some("inverted".into());
    make(p.d.as_ref(), &mut p.s, &raw, "from_raw").await;
    assert_eq!(kinds(&table(&mut p.s, "from_raw").await), WANT);

    for Side { mut admin, s, .. } in [c, p] {
        drop(s);
        admin.drop_database(DB).await.expect("drop_database");
    }
}

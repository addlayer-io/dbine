//! "Comparar esquemas" over the PostgreSQL stand-in (DSQL has no
//! emulator): two schemas that differ in every kind of thing the compare
//! reads (CHECKs, indexes with INCLUDE, expressions, DESC / NULLS order,
//! NULLS NOT DISTINCT and a predicate, domains, sequences). The sync
//! script runs on one side (DSQL's `INDEX ASYNC` and `ALTER TABLE ASYNC`
//! as PostgreSQL's plain forms) and both must then read the same.
//!
//! ```sh
//! docker run -d --name dbine-test-dsqlpg -e POSTGRES_PASSWORD=dbine -p 25301:5432 postgres:16
//! DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;

const SCHEMA: &str = "
CREATE DOMAIN {s}.precio AS numeric(12,2) DEFAULT 0 NOT NULL CONSTRAINT precio_pos CHECK (VALUE >= 0);
CREATE SEQUENCE {s}.seq_folio START WITH 100 INCREMENT BY 5 CACHE 1;
CREATE TABLE {s}.docs (
    id integer PRIMARY KEY,
    codigo varchar(20) NOT NULL,
    titulo text,
    monto numeric(12,2),
    fecha date,
    CONSTRAINT ck_fecha CHECK (fecha >= DATE '2000-01-01')
);
CREATE UNIQUE INDEX ux_codigo ON {s}.docs (codigo) INCLUDE (titulo) NULLS NOT DISTINCT;
CREATE INDEX ix_fecha ON {s}.docs (fecha DESC NULLS LAST, lower(titulo)) WHERE fecha IS NOT NULL;
";

const OTHER: &str = "
CREATE DOMAIN {s}.precio AS numeric(10,2);
CREATE SEQUENCE {s}.seq_folio START WITH 1;
CREATE SEQUENCE {s}.seq_extra;
CREATE TABLE {s}.docs (
    id integer PRIMARY KEY,
    codigo varchar(20) NOT NULL,
    titulo text,
    monto numeric(12,2),
    fecha date,
    CONSTRAINT ck_fecha CHECK (fecha >= DATE '1990-01-01'),
    CONSTRAINT ck_vieja CHECK (id > 0)
);
CREATE UNIQUE INDEX ux_codigo ON {s}.docs (codigo);
CREATE INDEX ix_fecha ON {s}.docs (fecha);
";

type Objects = BTreeMap<(String, String), String>;

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map_err(|e| e.to_string())
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';').to_string()
}

/// A schema's tables and objects, with the schema's name taken out.
async fn read(s: &mut Box<dyn Session>, schema: &str) -> (BTreeMap<String, TableSchema>, Objects) {
    let tables = s
        .database_schema()
        .await
        .expect("database_schema")
        .into_iter()
        .filter(|t| t.schema.as_deref() == Some(schema))
        .map(|mut t| {
            t.schema = None;
            for c in t.columns.iter_mut() {
                c.data_type = c.data_type.replace(&format!("{schema}."), "");
            }
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            t.checks.sort_by(|a, b| a.name.cmp(&b.name));
            (t.name.clone(), t)
        })
        .collect();
    let mut objects = Objects::new();
    for o in s.list_objects().await.expect("list_objects") {
        if o.schema.as_deref() != Some(schema) || !["sequence", "type"].contains(&o.kind.as_str()) {
            continue;
        }
        let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        let def = s.definition(&r).await.expect("definition").unwrap_or_else(|| panic!("no definition for {r:?}"));
        objects.insert((o.kind, o.name), def.replace(&format!("\"{schema}\"."), "\"{s}\"."));
    }
    (tables, objects)
}

fn in_schema(mut t: TableSchema, s: &str) -> TableSchema {
    t.schema = Some(s.into());
    t
}

#[tokio::test]
#[ignore]
async fn compare_and_sync_everything() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else {
        eprintln!("DBINE_TEST_DSQL_URL not set; skipping");
        return;
    };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig { driver: "dsql".into(), host: host.into(), port: port.parse().unwrap(), username: Some("postgres".into()), ..Default::default() };
    let d = dbine_driver_dsql::drivers().pop().unwrap();
    assert!(d.info().object_kinds.iter().any(|k| k.id == "type"));
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let (a, b) = ("dq_cmp_a", "dq_cmp_b");
    for x in [a, b] {
        run(&mut s, &format!("DROP SCHEMA IF EXISTS {x} CASCADE; CREATE SCHEMA {x}")).await.unwrap();
    }
    run(&mut s, &SCHEMA.replace("{s}", a)).await.unwrap();
    run(&mut s, &OTHER.replace("{s}", b)).await.unwrap();

    let (ta, oa) = read(&mut s, a).await;
    let (tb, ob) = read(&mut s, b).await;
    for (k, def) in &oa {
        eprintln!("-- {k:?}\n{def}");
    }
    let docs = &ta["docs"];
    let ix = |t: &TableSchema, n: &str| t.indexes.iter().find(|i| i.name == n).cloned().unwrap_or_else(|| panic!("{n} in {:#?}", t.indexes));
    let ux = ix(docs, "ux_codigo");
    assert_eq!(ux.include, ["titulo"]);
    assert_eq!(ux.options.get("nulls_not_distinct").map(String::as_str), Some("true"));
    let fx = ix(docs, "ix_fecha");
    assert_eq!(fx.columns, ["fecha", "(lower(titulo))"]);
    assert_eq!((fx.options.get("desc").map(String::as_str), fx.options.get("nulls_last").map(String::as_str)), (Some("fecha"), Some("fecha")));
    assert_eq!(fx.filter.as_deref(), Some("(fecha IS NOT NULL)"));
    assert_eq!(docs.checks.len(), 1);
    assert_eq!(
        oa[&("type".into(), "precio".into())],
        "CREATE DOMAIN \"{s}\".\"precio\" AS numeric(12,2)\n    DEFAULT 0\n    NOT NULL\n    CONSTRAINT \"precio_pos\" CHECK (VALUE >= 0::numeric);"
    );

    // The compare and the sync of B to A: domains and sequences dropped
    // (src-tauri's drop_other) and made before the tables.
    let tables: Vec<TableChange> = ta
        .iter()
        .filter(|(n, t)| tb.get(*n) != Some(t))
        .map(|(n, t)| TableChange::Alter { old: in_schema(tb[n].clone(), b), new: in_schema(t.clone(), b) })
        .collect();
    assert_eq!(tables.len(), 1);
    let q = |k: &str, n: &str| format!("DROP {} IF EXISTS \"{b}\".\"{n}\";", k.to_uppercase());
    let (mut before, mut early, mut late) = (Vec::new(), Vec::new(), Vec::new());
    for ((k, n), def) in &oa {
        match ob.get(&(k.clone(), n.clone())) {
            Some(o) if squash(o) == squash(def) => continue,
            Some(_) => before.push(q(k, n)),
            None => {}
        }
        early.push(def.replace("{s}", b));
    }
    for (k, n) in ob.keys() {
        if !oa.contains_key(&(k.clone(), n.clone())) {
            late.push(q(k, n));
        }
    }
    let script = d.sync_script(&tables).expect("sync_script");
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    assert!(script.statements.iter().any(|s| s.ends_with("NOT VALID;")), "{:#?}", script.statements);
    let statements: Vec<String> = before.into_iter().chain(early).chain(script.statements).chain(late).collect();
    for (i, st) in statements.iter().enumerate() {
        let st = st.replace(" INDEX ASYNC ", " INDEX ").replace("ALTER TABLE ASYNC ", "ALTER TABLE ");
        if let Err(e) = run(&mut s, &st).await {
            panic!("statement {i} failed: {e}\n---\n{st}\n---\nwhole script:\n{}", statements.join("\n"));
        }
    }
    let (tb2, ob2) = read(&mut s, b).await;
    assert_eq!(tb2, ta);
    let squashed = |o: &Objects| o.iter().map(|(k, v)| (k.clone(), squash(v))).collect::<Vec<_>>();
    assert_eq!(squashed(&ob2), squashed(&oa));
    for x in [a, b] {
        run(&mut s, &format!("DROP SCHEMA {x} CASCADE")).await.unwrap();
    }
}

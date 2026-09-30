//! Against a real server:
//! `docker run -d --name dbine-test-orientdb -p 22480:2480 -e ORIENTDB_ROOT_PASSWORD=dbine-test-pass orientdb:3.2`
//! then
//! `DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 cargo test -p dbine-driver-orientdb -- --ignored`.

use dbine_driver::{kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, ObjectRef, QueryOutcome, Session, TableSchema};
use dbine_driver_orientdb::{EDGE, VERTEX};
use serde_json::json;

fn cfg(url: &str, read_only: bool) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        read_only,
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 100, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

#[tokio::test]
#[ignore]
async fn round_trip() {
    let url = std::env::var("DBINE_TEST_ORIENTDB_URL").expect("DBINE_TEST_ORIENTDB_URL");
    let d = dbine_driver_orientdb::drivers().remove(0);
    let c = cfg(&url, false);
    // A session on any database to manage them.
    let mut admin = d.connect(&c, Some("dbine_admin")).await.ok();
    if admin.is_none() {
        let mut tmp = d.connect(&c, None).await.expect("connect");
        let _ = tmp.create_database("dbine_admin").await;
        admin = Some(d.connect(&c, Some("dbine_admin")).await.unwrap());
    }
    let mut admin = admin.unwrap();
    println!("{}", admin.server_version().await.unwrap());
    let _ = admin.drop_database("dbine_it").await;
    admin.create_database("dbine_it").await.unwrap();
    assert!(admin.list_databases().await.unwrap().contains(&"dbine_it".to_string()));

    let mut s = d.connect(&c, Some("dbine_it")).await.unwrap();
    // Designer: a vertex class with a mandatory name and a unique key.
    let mut t = TableSchema {
        kind: kinds::TABLE.into(),
        name: "Person".into(),
        columns: vec![
            ColumnDef { name: "name".into(), data_type: "STRING".into(), nullable: false, ..Default::default() },
            ColumnDef { name: "age".into(), data_type: "INTEGER".into(), nullable: true, ..Default::default() },
        ],
        primary_key: Some(dbine_driver::KeyDef { name: None, columns: vec!["name".into()] }),
        ..Default::default()
    };
    t.options.insert("extends".into(), "V".into());
    let ddl = d.table_ddl(&t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap();
    run(&mut s, &ddl).await;
    run(&mut s, "CREATE CLASS Knows IF NOT EXISTS EXTENDS E; CREATE CLASS Note IF NOT EXISTS").await;
    let out = run(
        &mut s,
        "CREATE VERTEX Person SET name = 'Ann', age = 30, tags = ['x', 'y'];
         CREATE VERTEX Person SET name = 'Bob';
         CREATE EDGE Knows FROM (SELECT FROM Person WHERE name = 'Ann') TO (SELECT FROM Person WHERE name = 'Bob') SET since = 2020;
         INSERT INTO Note CONTENT {\"text\": \"hola\", \"meta\": {\"a\": 1}};
         SELECT name, age, out('Knows').name AS knows FROM Person ORDER BY name;
         UPDATE Person SET age = 31 WHERE name = 'Ann'",
    )
    .await;
    let r = &out.results[4];
    assert_eq!(r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["name", "age", "knows"]);
    assert_eq!(r.rows[0], vec![json!("Ann"), json!(30), json!("[\"Bob\"]")]);
    assert_eq!(out.results[5].rows_affected, Some(1));

    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n);
    assert!(has(VERTEX, "Person") && has(EDGE, "Knows") && has(kinds::TABLE, "Note") && has(kinds::INDEX, "Person.pk"), "{objs:?}");
    assert!(!objs.iter().any(|o| o.name == "OUser"));
    let cols = s.columns(&obj(VERTEX, "Person")).await.unwrap();
    assert_eq!(cols[0].name, "name");
    assert!(!cols[0].nullable);
    assert!(cols.iter().any(|c| c.name == "tags"), "{cols:?}");
    let def = s.definition(&obj(VERTEX, "Person")).await.unwrap().unwrap();
    assert!(def.contains("CREATE CLASS Person EXTENDS V") && def.contains("CREATE INDEX Person.pk ON Person (name) UNIQUE"), "{def}");
    let def = s.definition(&obj(kinds::INDEX, "Person.pk")).await.unwrap().unwrap();
    assert!(def.starts_with("CREATE INDEX Person.pk ON Person (name) UNIQUE"), "{def}");

    // Browse and copy back.
    let q = s.browse_query(&obj(kinds::TABLE, "Note"), 10);
    let out = run(&mut s, &q).await;
    let cols: Vec<String> = out.results[0].columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(&cols[..2], ["@rid", "@class"]);
    let script = d.insert_script(&obj(kinds::TABLE, "Note"), &cols, &out.results[0].rows).unwrap();
    run(&mut s, &script).await;
    let q = s.browse_query(&obj(EDGE, "Knows"), 10);
    let out = run(&mut s, &q).await;
    let cols: Vec<String> = out.results[0].columns.iter().map(|c| c.name.clone()).collect();
    let script = d.insert_script(&obj(EDGE, "Knows"), &cols, &out.results[0].rows).unwrap();
    run(&mut s, &script).await;
    let out = run(&mut s, "SELECT count(*) AS n FROM Note; SELECT count(*) AS n FROM Knows").await;
    assert_eq!(out.results[0].rows[0][0], json!(2));
    assert_eq!(out.results[1].rows[0][0], json!(2));

    // Schema for the ER diagram and scripts.
    let schema = s.database_schema().await.unwrap();
    let p = schema.iter().find(|t| t.name == "Person").unwrap();
    assert!(p.indexes.iter().any(|i| i.unique));
    let script = d.table_ddl(p, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
    assert!(!script.contains("PROPERTY Person.tags"), "{script}");

    // Plans.
    let mut out = QueryOutcome::default();
    s.explain("SELECT FROM Person WHERE name = 'Ann'", false, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    println!("plan: {} / {:?}", out.plans[0].root.op, out.plans[0].root.children.iter().map(|c| &c.op).collect::<Vec<_>>());
    let mut out = QueryOutcome::default();
    s.explain("SELECT FROM Person", true, 10, &mut out).await.unwrap();
    assert!(out.plans[0].actual);
    assert_eq!(out.results[0].rows.len(), 2);

    // Read-only: the server refuses writes.
    let mut ro = d.connect(&cfg(&url, true), Some("dbine_it")).await.unwrap();
    run(&mut ro, "SELECT FROM Person").await;
    run(&mut ro, "MATCH {class: Person, as: p}-Knows->{as: q} RETURN p.name, q.name").await;
    let mut out = QueryOutcome::default();
    let e = ro.execute("UPDATE Person SET age = 1", 10, &mut out).await.unwrap_err();
    println!("read-only: {e}");

    // Monitor.
    let snap = s.monitor().await.unwrap();
    let vals: Vec<String> = snap.metrics.iter().filter(|m| m.value.is_some()).map(|m| format!("{}={:?}", m.key, m.value)).collect();
    println!("metrics {vals:?}\ntables {:?}\ninfo {:?}\nnotes {:?}", snap.tables.iter().map(|t| (&t.key, t.rows.len())).collect::<Vec<_>>(), snap.info, snap.notes);
    assert!(vals.len() >= 5);
    assert!(snap.tables.iter().any(|t| t.key == "sessions" && !t.rows.is_empty()));

    drop(s);
    drop(ro);
    admin.drop_database("dbine_it").await.unwrap();
    let _ = Driver::info(&*d);
}

/// Schema sync: a class read back from `database_schema` gets a property
/// added, dropped and retyped and an index swapped; another class is
/// dropped and one is created; the generated script runs.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{IndexDef, TableChange};
    let url = std::env::var("DBINE_TEST_ORIENTDB_URL").expect("DBINE_TEST_ORIENTDB_URL");
    let d = dbine_driver_orientdb::drivers().remove(0);
    assert!(d.supports_schema_sync());
    let c = cfg(&url, false);
    let mut admin = match d.connect(&c, Some("dbine_admin")).await {
        Ok(s) => s,
        Err(_) => {
            let mut tmp = d.connect(&c, None).await.expect("connect");
            let _ = tmp.create_database("dbine_admin").await;
            d.connect(&c, Some("dbine_admin")).await.unwrap()
        }
    };
    let _ = admin.drop_database("dbine_sync").await;
    admin.create_database("dbine_sync").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_sync")).await.unwrap();
    run(
        &mut s,
        "CREATE CLASS Person EXTENDS V;
         CREATE PROPERTY Person.name STRING;
         CREATE PROPERTY Person.age INTEGER;
         CREATE PROPERTY Person.legacy STRING;
         CREATE INDEX Person.legacy ON Person (legacy) NOTUNIQUE;
         CREATE INDEX Person.name ON Person (name) NOTUNIQUE;
         CREATE CLASS Gone;",
    )
    .await;
    let schema = s.database_schema().await.unwrap();
    let old = schema.iter().find(|t| t.name == "Person").unwrap().clone();
    let gone = schema.iter().find(|t| t.name == "Gone").unwrap().clone();
    let mut new = old.clone();
    new.columns.retain(|c| c.name != "legacy");
    let age = new.columns.iter_mut().find(|c| c.name == "age").unwrap();
    age.data_type = "LONG".into();
    let name = new.columns.iter_mut().find(|c| c.name == "name").unwrap();
    name.nullable = false;
    new.columns.push(ColumnDef { name: "email".into(), data_type: "STRING".into(), nullable: true, ..Default::default() });
    new.indexes = vec![
        IndexDef { name: "Person.name".into(), columns: vec!["name".into()], unique: true, kind: Some("UNIQUE".into()), filter: None, ..Default::default() },
        IndexDef { name: "Person.email".into(), columns: vec!["email".into()], unique: false, kind: Some("NOTUNIQUE".into()), filter: None, ..Default::default() },
    ];
    let created = TableSchema {
        kind: kinds::TABLE.into(),
        name: "Fresh".into(),
        columns: vec![ColumnDef { name: "code".into(), data_type: "STRING".into(), nullable: true, ..Default::default() }],
        ..Default::default()
    };
    let script = d.sync_script(&[TableChange::Alter { old, new }, TableChange::Drop { table: gone }, TableChange::Create { table: created }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let after = s.database_schema().await.unwrap();
    assert!(!after.iter().any(|t| t.name == "Gone"));
    assert!(after.iter().any(|t| t.name == "Fresh"));
    let p = after.iter().find(|t| t.name == "Person").unwrap();
    let declared = |n: &str| p.columns.iter().find(|c| c.name == n && !c.options.contains_key("inferred"));
    assert!(declared("legacy").is_none() && declared("email").is_some());
    assert_eq!(declared("age").unwrap().data_type, "LONG");
    assert!(!declared("name").unwrap().nullable);
    let mut ix: Vec<(&str, bool)> = p.indexes.iter().map(|i| (i.name.as_str(), i.unique)).collect();
    ix.sort();
    assert_eq!(ix, vec![("Person.email", false), ("Person.name", true)]);
    drop(s);
    admin.drop_database("dbine_sync").await.unwrap();
}

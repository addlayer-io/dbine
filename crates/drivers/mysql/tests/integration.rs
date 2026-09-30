//! Against real servers. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (`mysql://user:pass@host:port`) and is skipped without it:
//!
//! ```sh
//! docker run -d --name dbine-test-mysql -e MYSQL_ROOT_PASSWORD=pw -p 25011:3306 mysql:8
//! docker run -d --name dbine-test-mariadb -e MARIADB_ROOT_PASSWORD=pw -p 25012:3306 mariadb:11
//! docker run -d --name dbine-test-tidb -p 25014:4000 pingcap/tidb
//! docker run -d --name dbine-test-manticore -p 25016:9306 manticoresearch/manticore
//! docker run -d --name dbine-test-greptimedb -p 25017:4002 greptime/greptimedb standalone start --mysql-addr 0.0.0.0:4002
//! docker run -d --name dbine-test-starrocks -p 25030:9030 starrocks/allin1-ubuntu
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//!   cargo test -p dbine-driver-mysql --test integration -- --ignored
//! ```

use dbine_driver::{kinds, ConnectionConfig, DdlParts, Driver, Error, ObjectRef, QueryOutcome, Session, TableSchema};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `scheme://user:pass@host:port[/db]` into a config (no URL escapes).
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
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map(|_| out)
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

async fn exercise(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let version = admin.server_version().await.unwrap();
    eprintln!("{id}: {version}");
    run(&mut admin, "DROP DATABASE IF EXISTS dbine_t; CREATE DATABASE dbine_t").await.unwrap();
    let dbs = admin.list_databases().await.unwrap();
    assert!(dbs.iter().any(|d| d == "dbine_t"), "{dbs:?}");

    let mut s = d.connect(&cfg, Some("dbine_t")).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE items (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(40) NOT NULL DEFAULT 'x', price DECIMAL(10,2), data BLOB);
         INSERT INTO items (name, price, data) VALUES ('a', 1.5, x'CAFE'), ('b', 2, NULL), ('c', NULL, NULL);
         CREATE VIEW v_items AS SELECT id, name FROM items;",
    )
    .await
    .unwrap();
    let mut extras = Vec::new();
    for (kind, name, sql) in [
        (kinds::PROCEDURE, "noop", "CREATE PROCEDURE noop() SELECT 1"),
        (kinds::FUNCTION, "add_one", "CREATE FUNCTION add_one(x INT) RETURNS INT DETERMINISTIC RETURN x + 1"),
        (kinds::TRIGGER, "items_trg", "CREATE TRIGGER items_trg BEFORE INSERT ON items FOR EACH ROW SET NEW.price = NEW.price"),
    ] {
        match run(&mut s, sql).await {
            Ok(_) => extras.push((kind, name)),
            Err(e) => eprintln!("{id}: no {kind}: {e}"),
        }
    }

    let objs = s.list_objects().await.unwrap();
    eprintln!("{id}: objects {objs:?}");
    let find = |name: &str| objs.iter().find(|o| o.name == name).map(|o| o.kind.as_str());
    assert_eq!(find("items"), Some(kinds::TABLE));
    assert_eq!(find("v_items"), Some(kinds::VIEW));
    for (kind, name) in &extras {
        assert_eq!(find(name), Some(*kind));
    }
    if extras.iter().any(|(k, _)| *k == kinds::TRIGGER) {
        assert_eq!(objs.iter().find(|o| o.name == "items_trg").and_then(|o| o.parent.as_deref()), Some("items"));
    }

    let cols = s.columns(&obj(kinds::TABLE, "items")).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "price", "data"]);
    assert!(cols[0].primary_key && cols[0].auto_increment);
    assert!(!cols[1].nullable && cols[2].nullable);
    assert_eq!(cols[2].data_type, "decimal(10,2)");

    let table = s.definition(&obj(kinds::TABLE, "items")).await.unwrap().expect("table DDL");
    assert!(table.starts_with("CREATE TABLE"), "{table}");
    assert!(s.definition(&obj(kinds::VIEW, "v_items")).await.unwrap().is_some());
    for (kind, name) in &extras {
        assert!(s.definition(&obj(kind, name)).await.unwrap().is_some(), "{id}: {kind} {name}");
    }

    let q = s.browse_query(&obj(kinds::TABLE, "items"), 2);
    let out = run(&mut s, &q).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[0].rows[0][2], serde_json::json!("1.50"));
    assert_eq!(out.results[0].rows[0][3], serde_json::json!("0xCAFE"));

    // Several statements; one fails in the middle.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1 AS a; SELECT * FROM nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(_)), "{e:?}");
    assert_eq!(out.results.len(), 1, "{out:?}");
    // The connection survives the failed script.
    run(&mut s, "SELECT 1").await.unwrap();

    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM items", 1, &mut out).await.unwrap();
    assert_eq!((out.results[0].rows.len(), out.results[0].total_rows, out.results[0].truncated), (1, 3, true));

    let out = run(&mut s, "UPDATE items SET price = 3 WHERE name <> 'a'; SELECT 1").await.unwrap();
    assert_eq!(out.results[0].rows_affected, Some(2));
    assert_eq!(out.results[1].rows.len(), 1);

    // Cancel a long statement.
    let stop = s.interrupter().expect("interrupter");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop();
    });
    let t = Instant::now();
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT SLEEP(30)", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
    run(&mut s, "SELECT 1").await.unwrap();

    // Read-only session: refused by the server.
    let mut ro_cfg = cfg.clone();
    ro_cfg.read_only = true;
    let mut ro = d.connect(&ro_cfg, Some("dbine_t")).await.unwrap();
    match run(&mut ro, "INSERT INTO items (name) VALUES ('z')").await {
        Err(e) => {
            eprintln!("{id}: read-only refusal: {e}");
            assert!(matches!(e, Error::Query(_)));
        }
        // Only real MySQL servers must refuse; emulations rely on the
        // ReadOnlySession wrapper.
        Ok(_) => assert!(!matches!(id, "mysql" | "mariadb"), "{id}: write accepted in read-only session"),
    }

    run(&mut admin, "DROP DATABASE dbine_t").await.unwrap();

    if cfg.password.is_none() {
        return;
    }
    let mut bad = cfg.clone();
    bad.password = Some("definitely-wrong".into());
    match d.connect(&bad, None).await {
        Err(Error::AuthFailed(m)) => eprintln!("{id}: auth refused: {m}"),
        Err(e) => panic!("expected AuthFailed, got {e:?}"),
        Ok(_) => panic!("wrong password accepted"),
    }
}

#[tokio::test]
#[ignore]
async fn mysql() {
    exercise("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb() {
    exercise("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test]
#[ignore]
async fn tidb() {
    exercise("tidb", "DBINE_TEST_TIDB_URL").await;
}

/// Engines with their own DDL and a partial MySQL surface: connect, list,
/// describe and browse one table.
async fn exercise_light(id: &str, env: &str, setup: &[&str], table: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    eprintln!("{id}: {}", s.server_version().await.unwrap());
    let dbs = s.list_databases().await.unwrap();
    eprintln!("{id}: databases {dbs:?}");
    assert!(!dbs.is_empty());
    for sql in setup {
        run(&mut s, sql).await.unwrap_or_else(|e| panic!("{id}: {sql}: {e}"));
    }
    let objs = s.list_objects().await.unwrap();
    eprintln!("{id}: objects {objs:?}");
    assert!(objs.iter().any(|o| o.name == table && o.kind == kinds::TABLE));
    let declared: Vec<_> = d.info().object_kinds.iter().map(|k| k.id).collect();
    assert!(objs.iter().all(|o| declared.contains(&o.kind.as_str())));

    let t = obj(kinds::TABLE, table);
    let cols = s.columns(&t).await.unwrap();
    eprintln!("{id}: columns {cols:?}");
    assert!(!cols.is_empty() && cols.iter().all(|c| !c.name.is_empty() && !c.data_type.is_empty()));
    let def = s.definition(&t).await.unwrap();
    eprintln!("{id}: definition {def:?}");

    let q = s.browse_query(&t, 2);
    let out = run(&mut s, &q).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2, "{id}: {out:?}");
    let mut out = QueryOutcome::default();
    s.execute(&s.browse_query(&t, 10), 1, &mut out).await.unwrap();
    assert!(out.results[0].truncated);
    let e = run(&mut s, "SELECT * FROM nope_missing").await.unwrap_err();
    assert!(matches!(e, Error::Query(_)), "{e:?}");
    run(&mut s, &q).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn manticore() {
    exercise_light(
        "manticore",
        "DBINE_TEST_MANTICORE_URL",
        &[
            "DROP TABLE IF EXISTS items",
            "CREATE TABLE items (name text, price float)",
            "INSERT INTO items (id, name, price) VALUES (1, 'a', 1.5), (2, 'b', 2), (3, 'c', 3)",
        ],
        "items",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn greptimedb() {
    exercise_light(
        "greptimedb",
        "DBINE_TEST_GREPTIMEDB_URL",
        &[
            "DROP TABLE IF EXISTS items",
            "CREATE TABLE items (ts TIMESTAMP TIME INDEX, host STRING PRIMARY KEY, v DOUBLE)",
            "INSERT INTO items VALUES (1000, 'a', 1.5), (2000, 'b', 2), (3000, 'c', 3)",
        ],
        "items",
    )
    .await;
}

/// One line per operator, indented, with its figures.
fn outline(n: &dbine_driver::PlanNode, depth: usize) -> String {
    let mut s = format!(
        "{}{} [{}] {:?} cost={:?} est={:?} act={:?} x{:?} ms={:?} {:?}\n",
        "  ".repeat(depth), n.op, n.detail, n.object, n.total_cost, n.est_rows, n.actual_rows, n.executions, n.actual_ms, n.warnings
    );
    for c in &n.children {
        s.push_str(&outline(c, depth + 1));
    }
    s
}

/// Estimated plans run nothing (a DELETE leaves its rows); actual plans
/// run the script and carry measured figures.
async fn plans(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(&mut admin, "DROP DATABASE IF EXISTS dbine_plan; CREATE DATABASE dbine_plan").await.unwrap();
    let mut s = d.connect(&cfg, Some("dbine_plan")).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE a (id int PRIMARY KEY, g int, KEY (g));
         CREATE TABLE b (id int PRIMARY KEY, a_id int);
         INSERT INTO a (id, g) SELECT x.i * 10 + y.i, (x.i * 10 + y.i) % 10
           FROM (SELECT 0 i UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
                 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) x,
                (SELECT 0 i UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
                 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) y;
         INSERT INTO b SELECT id, id % 50 FROM a;",
    )
    .await
    .unwrap();
    let cell = |out: &QueryOutcome, i: usize| out.results[i].rows[0][0].to_string().trim_matches('"').to_string();

    let mut out = QueryOutcome::default();
    s.explain(
        "SELECT /*+ NO_INDEX(a) */ a.g, count(*) FROM a JOIN b ON b.a_id = a.id GROUP BY a.g;
         DELETE FROM b WHERE id < 10;
         UPDATE b SET a_id = 0 WHERE id < 10",
        false,
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.plans.len(), 3, "{out:?}");
    assert!(out.results.is_empty());
    for p in &out.plans {
        eprintln!("{id}: estimated {}\n{}", p.statement, outline(&p.root, 0));
        assert!(!p.actual && !p.root.op.is_empty());
    }
    assert!(out.plans[0].statement.contains("/*+ NO_INDEX(a) */"));
    let o = run(&mut s, "SELECT count(*) FROM b").await.unwrap();
    assert_eq!(cell(&o, 0), "100");

    let mut out = QueryOutcome::default();
    s.explain(
        "SELECT count(*) FROM a WHERE g = 3; DELETE FROM b WHERE id < 10; SELECT count(*) FROM b",
        true,
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.plans.len(), 3);
    assert_eq!(out.results.len(), 3, "{out:?}");
    assert_eq!(cell(&out, 0), "10");
    // The DELETE ran exactly once.
    assert_eq!(cell(&out, 2), "90");
    assert!(out.plans[0].actual && !out.plans[1].actual && out.plans[2].actual, "{:?}", out.messages);
    eprintln!("{id}: actual\n{}", outline(&out.plans[0].root, 0));
    fn any_actual(n: &dbine_driver::PlanNode) -> bool {
        n.actual_rows.is_some() || n.children.iter().any(any_actual)
    }
    assert!(any_actual(&out.plans[0].root));

    let mut out = QueryOutcome::default();
    assert!(s.explain("SELECT 1; SELECT * FROM missing_table", true, 10, &mut out).await.is_err());
    assert_eq!(out.results.len(), 1);
    run(&mut admin, "DROP DATABASE dbine_plan").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn mysql_plans() {
    plans("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb_plans() {
    plans("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test]
#[ignore]
async fn tidb_plans() {
    plans("tidb", "DBINE_TEST_TIDB_URL").await;
}

/// Every table's CREATE first, then indexes and foreign keys.
fn script(d: &Arc<dyn Driver>, schema: &[TableSchema]) -> String {
    let create = DdlParts { create: true, ..Default::default() };
    let rest = DdlParts { indexes: true, foreign_keys: true, ..Default::default() };
    let mut parts: Vec<String> = schema.iter().map(|t| d.table_ddl(t, create).unwrap()).collect();
    parts.extend(schema.iter().map(|t| d.table_ddl(t, rest).unwrap()).filter(|s| !s.is_empty()));
    parts.join("\n")
}

/// Three related tables: catalog read, DDL round trip into a fresh
/// database, create / drop database, INSERT script.
async fn ddl_roundtrip(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    assert!(d.capabilities().create_database && d.capabilities().foreign_keys);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(&mut admin, "DROP DATABASE IF EXISTS dbine_ddl; DROP DATABASE IF EXISTS dbine_ddl2").await.unwrap();
    admin.create_database("dbine_ddl").await.unwrap();
    let mut s = d.connect(&cfg, Some("dbine_ddl")).await.unwrap();
    assert!(matches!(s.drop_database("dbine_ddl").await, Err(Error::Query(_))));
    run(
        &mut s,
        r"CREATE TABLE clientes (
            id INT AUTO_INCREMENT PRIMARY KEY COMMENT 'clave',
            email VARCHAR(100) NOT NULL,
            nombre VARCHAR(50) DEFAULT 'sin \\ nombre',
            activo BOOLEAN NOT NULL DEFAULT TRUE,
            alta TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
            UNIQUE KEY uq_email (email)
          ) COMMENT='Clientes';
          CREATE TABLE pedidos (
            id INT AUTO_INCREMENT PRIMARY KEY,
            cliente_id INT NOT NULL,
            total DECIMAL(10,2) NOT NULL DEFAULT 0,
            con_iva DECIMAL(10,2) AS (total * 1.21) VIRTUAL,
            nota TEXT,
            KEY ix_total (total),
            KEY ix_nota (nota(20)),
            CONSTRAINT fk_ped_cli FOREIGN KEY (cliente_id) REFERENCES clientes (id) ON DELETE CASCADE
          );
          CREATE TABLE lineas (
            pedido_id INT NOT NULL,
            n INT NOT NULL,
            producto VARCHAR(40) NOT NULL DEFAULT 'x',
            PRIMARY KEY (pedido_id, n),
            CONSTRAINT fk_lin_ped FOREIGN KEY (pedido_id) REFERENCES pedidos (id) ON UPDATE CASCADE
          );",
    )
    .await
    .unwrap();
    if id != "tidb" {
        run(&mut s, "ALTER TABLE lineas ADD codigo VARCHAR(36) DEFAULT (concat('a', 'b')), ADD cantidad INT DEFAULT (1 + 1)").await.unwrap();
    }

    let schema = s.database_schema().await.unwrap();
    eprintln!("{id}: {schema:#?}");
    assert_eq!(schema.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["clientes", "lineas", "pedidos"]);
    let (cli, lin, ped) = (&schema[0], &schema[1], &schema[2]);
    assert_eq!(cli.comment.as_deref(), Some("Clientes"));
    assert_eq!(cli.columns[0].comment.as_deref(), Some("clave"));
    assert!(cli.columns[0].auto_increment && !cli.columns[1].nullable);
    assert_eq!(cli.columns[2].data_type, "varchar(50)");
    assert!(cli.indexes.iter().any(|i| i.name == "uq_email" && i.unique && i.columns == ["email"]));
    assert_eq!(lin.primary_key.as_ref().unwrap().columns, ["pedido_id", "n"]);
    let fk = &lin.foreign_keys[0];
    assert_eq!((fk.name.as_deref(), fk.ref_table.as_str(), fk.on_update.as_deref(), fk.on_delete.as_deref()), (Some("fk_lin_ped"), "pedidos", Some("CASCADE"), None));
    let fk = &ped.foreign_keys[0];
    assert_eq!((fk.columns.as_slice(), fk.ref_columns.as_slice(), fk.on_delete.as_deref()), (&["cliente_id".to_string()][..], &["id".to_string()][..], Some("CASCADE")));
    assert!(ped.indexes.iter().any(|i| i.name == "ix_total" && !i.unique));

    let ddl = script(&d, &schema);
    eprintln!("{id}: script\n{ddl}");
    admin.create_database("dbine_ddl2").await.unwrap();
    let mut s2 = d.connect(&cfg, Some("dbine_ddl2")).await.unwrap();
    run(&mut s2, &ddl).await.unwrap_or_else(|e| panic!("{id}: round trip: {e}"));
    let schema2 = s2.database_schema().await.unwrap();
    assert_eq!(schema, schema2);

    let ins = d
        .insert_script(
            &obj(kinds::TABLE, "clientes"),
            &["id".into(), "email".into(), "nombre".into(), "activo".into()],
            &[vec![json!(1), json!("a@x"), json!("O'Brien \\ n"), json!(true)], vec![json!(2), json!("b@x"), Value::Null, json!(false)]],
        )
        .unwrap();
    run(&mut s2, &ins).await.unwrap_or_else(|e| panic!("{id}: {ins}: {e}"));
    let out = run(&mut s2, "SELECT nombre, activo FROM clientes ORDER BY id").await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!("O'Brien \\ n"));

    drop(s);
    drop(s2);
    admin.drop_database("dbine_ddl").await.unwrap();
    admin.drop_database("dbine_ddl2").await.unwrap();
    let dbs = admin.list_databases().await.unwrap();
    assert!(!dbs.iter().any(|d| d.starts_with("dbine_ddl")));
}

#[tokio::test]
#[ignore]
async fn mysql_ddl() {
    ddl_roundtrip("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb_ddl() {
    ddl_roundtrip("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test]
#[ignore]
async fn tidb_ddl() {
    ddl_roundtrip("tidb", "DBINE_TEST_TIDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn greptimedb_ddl() {
    let Ok(url) = std::env::var("DBINE_TEST_GREPTIMEDB_URL") else {
        eprintln!("DBINE_TEST_GREPTIMEDB_URL not set; skipping");
        return;
    };
    let cfg = parse_url("greptimedb", &url);
    let d = driver("greptimedb");
    assert!(!d.capabilities().foreign_keys);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(&mut admin, "DROP DATABASE IF EXISTS dbine_ddl; DROP DATABASE IF EXISTS dbine_ddl2").await.unwrap();
    admin.create_database("dbine_ddl").await.unwrap();
    let mut s = d.connect(&cfg, Some("dbine_ddl")).await.unwrap();
    assert!(matches!(s.drop_database("dbine_ddl").await, Err(Error::Query(_))));
    run(
        &mut s,
        r"CREATE TABLE cpu (ts TIMESTAMP(3) TIME INDEX DEFAULT current_timestamp(), host STRING COMMENT 'm\\q', dc STRING DEFAULT 'eu',
            v DOUBLE DEFAULT 1.5, ok BOOLEAN, PRIMARY KEY (host, dc)) WITH (comment = 'uso de cpu');
          CREATE TABLE ev (t TIMESTAMP NOT NULL, msg STRING, TIME INDEX (t))",
    )
    .await
    .unwrap();
    let schema = s.database_schema().await.unwrap();
    eprintln!("greptimedb: {schema:#?}");
    let cpu = &schema[0];
    assert_eq!((cpu.name.as_str(), cpu.comment.as_deref()), ("cpu", Some("uso de cpu")));
    assert_eq!(cpu.options.get("time_index").map(String::as_str), Some("ts"));
    assert_eq!(cpu.primary_key.as_ref().unwrap().columns, ["host", "dc"]);
    assert_eq!(cpu.columns[1].comment.as_deref(), Some("m\\q"));
    let ddl = script(&d, &schema);
    eprintln!("greptimedb: script\n{ddl}");
    admin.create_database("dbine_ddl2").await.unwrap();
    let mut s2 = d.connect(&cfg, Some("dbine_ddl2")).await.unwrap();
    run(&mut s2, &ddl).await.unwrap_or_else(|e| panic!("round trip: {e}"));
    assert_eq!(schema, s2.database_schema().await.unwrap());
    let ins = d
        .insert_script(
            &obj(kinds::TABLE, "cpu"),
            &["ts".into(), "host".into(), "v".into(), "ok".into()],
            &[vec![json!(1000), json!("a'\\b"), json!(2.5), json!(true)], vec![json!(2000), json!("b"), Value::Null, json!(false)]],
        )
        .unwrap();
    run(&mut s2, &ins).await.unwrap_or_else(|e| panic!("{ins}: {e}"));
    let out = run(&mut s2, "SELECT host FROM cpu ORDER BY ts").await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!("a'\\b"));
    drop((s, s2));
    admin.drop_database("dbine_ddl").await.unwrap();
    admin.drop_database("dbine_ddl2").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn manticore_ddl() {
    let Ok(url) = std::env::var("DBINE_TEST_MANTICORE_URL") else {
        eprintln!("DBINE_TEST_MANTICORE_URL not set; skipping");
        return;
    };
    let d = driver("manticore");
    assert!(!d.capabilities().create_database);
    let mut s = d.connect(&parse_url("manticore", &url), None).await.expect("connect");
    run(&mut s, "DROP TABLE IF EXISTS dbine_src; DROP TABLE IF EXISTS dbine_copy").await.unwrap();
    run(&mut s, "CREATE TABLE dbine_src (title text, price float, n integer, big bigint, ok bool, tags multi, meta json, s string, ts timestamp)").await.unwrap();
    let schema = s.database_schema().await.unwrap();
    let mut t = schema.into_iter().find(|t| t.name == "dbine_src").expect("table");
    eprintln!("manticore: {t:#?}");
    t.name = "dbine_copy".into();
    let ddl = d.table_ddl(&t, DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true }).unwrap();
    eprintln!("manticore: {ddl}");
    run(&mut s, &ddl).await.unwrap_or_else(|e| panic!("{ddl}: {e}"));
    let copy = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "dbine_copy").unwrap();
    assert_eq!(t.columns, copy.columns);
    let ins = d
        .insert_script(
            &obj(kinds::TABLE, "dbine_copy"),
            &["id".into(), "title".into(), "price".into(), "ok".into(), "tags".into(), "meta".into()],
            &[
                vec![json!(1), json!("O'Brien \\ x"), json!(1.5), json!(true), json!([1, 2]), json!({"a": 1})],
                vec![json!(2), Value::Null, Value::Null, json!(false), Value::Null, Value::Null],
            ],
        )
        .unwrap();
    run(&mut s, &ins).await.unwrap_or_else(|e| panic!("{ins}: {e}"));
    let out = run(&mut s, "SELECT title FROM dbine_copy WHERE id = 1").await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!("O'Brien \\ x"));
    run(&mut s, "DROP TABLE dbine_src; DROP TABLE dbine_copy").await.unwrap();
}

/// Two snapshots: real values, the standard tables, and (on InnoDB
/// servers) a running statement and a row-lock wait caught in the act.
async fn monitor(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let innodb = matches!(id, "mysql" | "mariadb" | "aurora-mysql" | "cloudsql-mysql");
    let mut bg = Vec::new();
    if innodb {
        run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_mon; CREATE TABLE IF NOT EXISTS dbine_mon.t (id INT PRIMARY KEY, v INT); REPLACE INTO dbine_mon.t VALUES (1, 1)")
            .await
            .unwrap();
        // A holds the row; B waits for it; C sleeps.
        let mut a = d.connect(&cfg, None).await.unwrap();
        run(&mut a, "BEGIN; SELECT * FROM dbine_mon.t WHERE id = 1 FOR UPDATE").await.unwrap();
        let (d2, c2) = (d.clone(), cfg.clone());
        bg.push(tokio::spawn(async move {
            let mut b = d2.connect(&c2, None).await.unwrap();
            let _ = run(&mut b, "SET innodb_lock_wait_timeout = 4; UPDATE dbine_mon.t SET v = 2 WHERE id = 1").await;
            drop(a);
        }));
        let (d3, c3) = (d.clone(), cfg.clone());
        bg.push(tokio::spawn(async move {
            let mut c = d3.connect(&c3, None).await.unwrap();
            let _ = run(&mut c, "SELECT SLEEP(3)").await;
        }));
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let first = s.monitor().await.expect("monitor");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        eprintln!("{id}: {} [{}] = {:?} max {:?}{}", m.key, m.group, m.value, m.max, if m.counter { " (counter)" } else { "" });
    }
    for t in &snap.tables {
        eprintln!("{id}: table {} ({}) {} rows, cols {:?}", t.key, t.title, t.rows.len(), t.columns);
    }
    eprintln!("{id}: info {:?}", snap.info);
    eprintln!("{id}: notes {:#?}", snap.notes);
    assert!(!first.metrics.is_empty() && snap.metrics.iter().filter(|m| m.value.is_some()).count() >= 3, "{snap:#?}");
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
    for t in &snap.tables {
        assert!(t.rows.len() <= 200 && t.rows.iter().all(|r| r.len() == t.columns.len()), "{}", t.key);
    }
    let table = |k: &str| snap.tables.iter().chain(&first.tables).find(|t| t.key == k && !t.rows.is_empty());
    if innodb {
        for k in ["connections", "queries", "mem_cache", "uptime", "storage_used"] {
            assert!(snap.metrics.iter().any(|m| m.key == k), "{id}: no {k}");
        }
        assert!(table("queries").is_some(), "{id}: the SLEEP should show");
        assert!(table("locks").is_some(), "{id}: the lock wait should show");
        assert!(table("databases").is_some());
    }
    for h in bg {
        h.await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn mysql_monitor() {
    monitor("mysql", "DBINE_TEST_MYSQL_URL").await;
}

/// Aurora and Cloud SQL speak plain MySQL: a MySQL server stands in.
#[tokio::test]
#[ignore]
async fn managed_mysql_monitor() {
    monitor("aurora-mysql", "DBINE_TEST_MYSQL_URL").await;
    monitor("cloudsql-mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb_monitor() {
    monitor("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test]
#[ignore]
async fn tidb_monitor() {
    monitor("tidb", "DBINE_TEST_TIDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn manticore_monitor() {
    monitor("manticore", "DBINE_TEST_MANTICORE_URL").await;
}

#[tokio::test]
#[ignore]
async fn greptimedb_monitor() {
    monitor("greptimedb", "DBINE_TEST_GREPTIMEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn starrocks_monitor() {
    monitor("starrocks", "DBINE_TEST_STARROCKS_URL").await;
}

#[tokio::test]
#[ignore]
async fn doris_monitor() {
    monitor("doris", "DBINE_TEST_DORIS_URL").await;
}

/// The profiler: one session profiles while another runs a slow statement
/// and a fast one; each is seen once, and the profiler's own are left out.
async fn profile(id: &str, env: &str, change_server: bool) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    assert!(d.supports_profiler(), "{id}");
    let db = match id {
        "greptimedb" => "public",
        "manticore" => "",
        _ => "dbine_prof",
    };
    if !db.is_empty() && db != "public" {
        let mut admin = d.connect(&cfg, None).await.expect("connect");
        run(&mut admin, "CREATE DATABASE IF NOT EXISTS dbine_prof").await.expect("create database");
    }
    if id == "manticore" {
        // Manticore answers in milliseconds: a slow statement needs data.
        const ROWS: u64 = 2_000_000;
        let mut admin = d.connect(&cfg, None).await.expect("connect");
        let have = run(&mut admin, "SELECT COUNT(*) FROM dbine_prof_t").await.ok().and_then(|o| o.results.first()?.rows.first()?[0].as_u64());
        if have != Some(ROWS) {
            run(&mut admin, "DROP TABLE IF EXISTS dbine_prof_t").await.expect("drop");
            run(&mut admin, "CREATE TABLE dbine_prof_t (title text, n int)").await.expect("create");
            for b in 0..ROWS / 20_000 {
                let vals: Vec<String> =
                    (0..20_000u64).map(|i| format!("({}, 'w', {})", b * 20_000 + i + 1, (b * 20_000 + i) * 7919 % 1_000_003)).collect();
                run(&mut admin, &format!("INSERT INTO dbine_prof_t (id, title, n) VALUES {}", vals.join(","))).await.expect("insert");
            }
        }
    }
    let target = (!db.is_empty()).then_some(db);
    let mut p = d.connect(&cfg, target).await.expect("connect");
    let mut w = d.connect(&cfg, target).await.expect("connect");
    let opts = dbine_driver::ProfilerOptions { database: db.into(), change_server };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{id}: {started:?}");
    let complete = started.mode == dbine_driver::ProfilerMode::Complete;
    let marker = format!("dbine_prof_{}", std::process::id());
    let slow = match id {
        "starrocks" | "doris" => format!("SELECT SLEEP(1), 1 AS {marker}_slow"),
        "greptimedb" => format!(
            "SELECT COUNT(DISTINCT value % 10000019) AS {marker}_slow FROM generate_series(1, 100000000)"
        ),
        "manticore" => format!(
            "SELECT n % 1000003 AS g, COUNT(*) AS {marker}_slow FROM dbine_prof_t GROUP BY g ORDER BY {marker}_slow DESC LIMIT 1 OPTION max_matches=1000000"
        ),
        _ => format!("SELECT SLEEP(0.6), 1 AS {marker}_slow"),
    };
    let fast = format!("SELECT 1 AS {marker}_fast");
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        run(&mut w, &slow).await.expect("slow");
        tokio::time::sleep(Duration::from_millis(400)).await;
        run(&mut w, &fast).await.expect("fast");
        tokio::time::sleep(Duration::from_millis(600)).await;
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(if complete { 8 } else { 5 });
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            let n = got.iter().filter(|s| s.text.contains(&marker)).count();
            if n >= if complete { 2 } else { 1 } && Instant::now() + Duration::from_secs(4) > until {
                break;
            }
            if complete {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{id}: {mine:#?}");
    let slow_seen: Vec<_> = mine.iter().filter(|s| s.text.contains("_slow")).collect();
    assert_eq!(slow_seen.len(), 1, "{id}: the slow statement once");
    assert!(slow_seen[0].duration_ms.unwrap_or(0.0) >= 300.0, "{id}: duration {:?}", slow_seen[0].duration_ms);
    if complete {
        assert_eq!(mine.iter().filter(|s| s.text.contains("_fast")).count(), 1, "{id}: the fast statement once");
        // Performance Schema: rows examined (and CPU on MySQL 8.0.28+);
        // TiDB's slow log: keys processed.
        if matches!(id, "mysql" | "mariadb" | "tidb") {
            assert!(started.reads_unit.is_some() && slow_seen[0].reads.is_some(), "{id}: reads");
        }
        if id == "mysql" && change_server {
            // The profiler switches events_statements_cpu on.
            assert!(slow_seen[0].cpu_ms.is_some_and(|ms| ms > 0.0), "{id}: cpu");
        }
    }
    for own in ["performance_schema", "PROCESSLIST", "SLOW_QUERY", "process_list", "SHOW THREADS"] {
        assert!(got.iter().all(|s| !s.text.contains(own)), "{id}: its own statements are left out");
    }
}

#[tokio::test]
#[ignore]
async fn mysql_profiler() {
    profile("mysql", "DBINE_TEST_MYSQL_URL", true).await;
    // Read-only: consumers stay as they are.
    profile("mysql", "DBINE_TEST_MYSQL_URL", false).await;
}

#[tokio::test]
#[ignore]
async fn mariadb_profiler() {
    profile("mariadb", "DBINE_TEST_MARIADB_URL", true).await;
}

#[tokio::test]
#[ignore]
async fn tidb_profiler() {
    profile("tidb", "DBINE_TEST_TIDB_URL", true).await;
    profile("tidb", "DBINE_TEST_TIDB_URL", false).await;
}

#[tokio::test]
#[ignore]
async fn starrocks_profiler() {
    profile("starrocks", "DBINE_TEST_STARROCKS_URL", true).await;
}

#[tokio::test]
#[ignore]
async fn greptimedb_profiler() {
    profile("greptimedb", "DBINE_TEST_GREPTIMEDB_URL", true).await;
}

#[tokio::test]
#[ignore]
async fn manticore_profiler() {
    profile("manticore", "DBINE_TEST_MANTICORE_URL", true).await;
}

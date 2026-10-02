//! "Comparar esquemas" against real servers: two databases that differ in
//! everything the compare reads for the engine (CHECKs, index prefixes,
//! expressions, DESC, INVISIBLE / IGNORED, comments, full-text parsers,
//! spatial indexes, sequences, TiDB's clustered keys, StarRocks index
//! types and bloom filters, GreptimeDB's column indexes, Manticore's
//! settings). The sync script runs on one side and both must then read the
//! same. Each test reads `DBINE_TEST_<ENGINE>_URL` and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//! DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030 \
//! DBINE_TEST_GREPTIMEDB_URL=mysql://localhost:25017 \
//! DBINE_TEST_MANTICORE_URL=mysql://localhost:25016 \
//!   cargo test -p dbine-driver-mysql --test compare -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The TiDB test turns `tidb_enable_check_constraint` on for the run and
//! puts it back as it was.

use dbine_driver::{ConnectionConfig, Driver, IndexDef, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

type Objects = BTreeMap<(String, String), String>;

fn normalized(mut t: TableSchema) -> TableSchema {
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t.checks.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';').to_string()
}

async fn read(s: &mut Box<dyn Session>) -> (BTreeMap<String, TableSchema>, Objects) {
    let tables = s.database_schema().await.expect("database_schema").into_iter().map(|t| (t.name.clone(), normalized(t))).collect();
    let mut objects = Objects::new();
    for o in s.list_objects().await.expect("list_objects") {
        if o.kind != "sequence" {
            continue;
        }
        let r = ObjectRef { kind: o.kind.clone(), schema: None, name: o.name.clone() };
        let def = s.definition(&r).await.expect("definition").unwrap_or_else(|| panic!("no definition for {r:?}"));
        objects.insert((o.kind, o.name), def);
    }
    (tables, objects)
}

fn ix<'a>(t: &'a TableSchema, n: &str) -> &'a IndexDef {
    t.indexes.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n} in {:#?}", t.indexes))
}

fn opt<'a>(i: &'a IndexDef, k: &str) -> Option<&'a str> {
    i.options.get(k).map(String::as_str)
}

/// Tables by name and sequences by name, then the script src-tauri runs:
/// object drops and replaced sequences first, new sequences, the tables'
/// sync, dropped sequences last.
fn plan(d: &dyn Driver, ta: &BTreeMap<String, TableSchema>, tb: &BTreeMap<String, TableSchema>, oa: &Objects, ob: &Objects) -> (Vec<String>, Vec<String>) {
    let mut tables = Vec::new();
    for (name, t) in ta {
        match tb.get(name) {
            None => tables.push(TableChange::Create { table: t.clone() }),
            Some(o) if o != t => tables.push(TableChange::Alter { old: o.clone(), new: t.clone() }),
            Some(_) => {}
        }
    }
    for (name, t) in tb {
        if !ta.contains_key(name) {
            tables.push(TableChange::Drop { table: t.clone() });
        }
    }
    let changed = tables
        .iter()
        .map(|c| match c {
            TableChange::Alter { new, .. } | TableChange::Create { table: new } | TableChange::Drop { table: new } => new.name.clone(),
        })
        .collect();
    let drop = |n: &str| format!("DROP SEQUENCE IF EXISTS `{n}`;");
    let (mut before, mut early, mut late) = (Vec::new(), Vec::new(), Vec::new());
    for ((_, n), def) in oa {
        match ob.get(&("sequence".to_string(), n.clone())) {
            None => early.push(def.clone()),
            Some(o) if squash(o) != squash(def) => {
                before.push(drop(n));
                early.push(def.clone());
            }
            Some(_) => {}
        }
    }
    for (_, n) in ob.keys() {
        if !oa.contains_key(&("sequence".to_string(), n.clone())) {
            late.push(drop(n));
        }
    }
    let script = d.sync_script(&tables).expect("sync_script");
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    (before.into_iter().chain(early).chain(script.statements).chain(late).collect(), changed)
}

/// StarRocks: until no schema change or rollup job runs in the database.
async fn wait_jobs(s: &mut Box<dyn Session>, secs: u64) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        let mut busy = false;
        for sql in ["SHOW ALTER TABLE COLUMN", "SHOW ALTER TABLE ROLLUP"] {
            let out = run(s, sql).await.unwrap();
            busy |= out.results.iter().flat_map(|r| &r.rows).flatten().any(|v| ["PENDING", "RUNNING", "WAITING_TXN", "FINISHING"].contains(&v.to_string().trim_matches('"')));
        }
        if !busy {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

struct Case {
    id: &'static str,
    env: &'static str,
    source: &'static str,
    target: &'static str,
    /// Tables the compare must find different.
    changed: &'static [&'static str],
    /// Seconds to wait for the sync to show (asynchronous schema changes).
    settle: u64,
}

/// Build both databases, compare, sync B to A, compare again. Returns the
/// source's tables and objects for the engine's own assertions.
async fn compare(c: &Case) -> Option<(BTreeMap<String, TableSchema>, Objects)> {
    let Ok(url) = std::env::var(c.env) else {
        eprintln!("{} not set; skipping", c.env);
        return None;
    };
    let cfg = parse_url(c.id, &url);
    let d = driver(c.id);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!("{}: {}", c.id, admin.server_version().await.unwrap());
    let (a_db, b_db) = ("dbine_cmp_a", "dbine_cmp_b");
    for db in [a_db, b_db] {
        run(&mut admin, &format!("DROP DATABASE IF EXISTS {db}")).await.unwrap();
        admin.create_database(db).await.expect("create_database");
    }
    let mut a = d.connect(&cfg, Some(a_db)).await.unwrap();
    let mut b = d.connect(&cfg, Some(b_db)).await.unwrap();
    for (s, sql) in [(&mut a, c.source), (&mut b, c.target)] {
        for stmt in sql.split(";\n").map(str::trim).filter(|x| !x.is_empty()) {
            run(s, stmt).await.unwrap_or_else(|e| panic!("{}: {stmt}: {e}", c.id));
        }
        if c.settle > 0 {
            wait_jobs(s, c.settle).await;
        }
    }
    let (ta, oa) = read(&mut a).await;
    let (tb, ob) = read(&mut b).await;
    for t in ta.values() {
        eprintln!("{}: A {t:#?}", c.id);
    }
    for ((k, n), def) in &oa {
        eprintln!("-- {k} {n}\n{def}");
    }

    let (statements, changed) = plan(d.as_ref(), &ta, &tb, &oa, &ob);
    assert_eq!(changed, c.changed, "{}", c.id);
    eprintln!("{}: script\n{}", c.id, statements.join("\n"));
    // The driver waits when the table is busy with a schema change; the
    // test also waits for each job to end, as the user would see it.
    for (i, s) in statements.iter().enumerate() {
        run(&mut b, s).await.unwrap_or_else(|e| panic!("{}: statement {i} failed: {e}\n---\n{s}\n---\nwhole script:\n{}", c.id, statements.join("\n")));
        if c.settle > 0 {
            wait_jobs(&mut b, c.settle).await;
        }
    }
    let start = Instant::now();
    loop {
        let (tb2, ob2) = read(&mut b).await;
        let same = tb2 == ta && ob2.iter().map(|(k, v)| (k.clone(), squash(v))).eq(oa.iter().map(|(k, v)| (k.clone(), squash(v))));
        if same {
            break;
        }
        if start.elapsed() > Duration::from_secs(c.settle) {
            for (name, t) in &ta {
                assert_eq!(tb2.get(name), Some(t), "{}: table {name} after the sync", c.id);
            }
            assert_eq!(tb2.len(), ta.len());
            assert_eq!(ob2, oa, "{}: objects after the sync", c.id);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    drop(a);
    drop(b);
    for db in [a_db, b_db] {
        admin.drop_database(db).await.expect("drop_database");
    }
    Some((ta, oa))
}

#[tokio::test]
#[ignore]
async fn mysql() {
    let case = Case {
        id: "mysql",
        env: "DBINE_TEST_MYSQL_URL",
        source: "
CREATE TABLE docs (
  id int NOT NULL PRIMARY KEY,
  titulo varchar(200) NOT NULL,
  cuerpo text,
  cjk text,
  g point NOT NULL SRID 0,
  precio int,
  estado varchar(10),
  CONSTRAINT ck_precio CHECK (precio > 0),
  CONSTRAINT ck_estado CHECK (estado <> 'x') NOT ENFORCED,
  KEY ix_pre (titulo(20) DESC, precio) COMMENT 'prefijo',
  KEY ix_fn ((lower(titulo)), precio DESC) INVISIBLE,
  FULLTEXT KEY ft (cuerpo),
  FULLTEXT KEY ft_ng (cjk) WITH PARSER ngram,
  SPATIAL KEY sp (g),
  UNIQUE KEY u (estado)
);
CREATE TABLE mem (id int, KEY ib (id) USING BTREE, KEY ih (id)) ENGINE=MEMORY;
CREATE TABLE nuevo (id int PRIMARY KEY, n int CHECK (n <> 0), KEY ix_n ((n * 2)))",
        target: "
CREATE TABLE docs (
  id int NOT NULL PRIMARY KEY,
  titulo varchar(200) NOT NULL,
  cuerpo text,
  cjk text,
  g point NOT NULL,
  precio int,
  estado varchar(10),
  CONSTRAINT ck_precio CHECK (precio > 10),
  CONSTRAINT ck_estado CHECK (estado <> 'x'),
  CONSTRAINT ck_viejo CHECK (precio < 1000),
  KEY ix_pre (titulo(10), precio),
  KEY ix_fn ((upper(titulo)), precio),
  FULLTEXT KEY ft_ng (cjk),
  KEY u (estado)
);
CREATE TABLE mem (id int, KEY ib (id), KEY ih (id)) ENGINE=MEMORY",
        changed: &["docs", "mem", "nuevo"],
        settle: 0,
    };
    let Some((ta, _)) = compare(&case).await else { return };
    let docs = &ta["docs"];
    let pre = ix(docs, "ix_pre");
    assert_eq!((pre.columns.as_slice(), opt(pre, "desc"), opt(pre, "COMMENT")), (&["titulo(20)".to_string(), "precio".into()][..], Some("titulo(20)"), Some("prefijo")));
    let f = ix(docs, "ix_fn");
    assert_eq!((f.columns[0].as_str(), opt(f, "INVISIBLE"), opt(f, "desc")), ("(lower(`titulo`))", Some("YES"), Some("precio")));
    assert_eq!((ix(docs, "ft_ng").kind.as_deref(), opt(ix(docs, "ft_ng"), "WITH PARSER")), (Some("FULLTEXT"), Some("ngram")));
    assert_eq!((ix(docs, "sp").kind.as_deref(), ix(docs, "sp").columns.as_slice()), (Some("SPATIAL"), &["g".to_string()][..]));
    assert!(docs.columns.iter().any(|c| c.name == "g" && c.data_type == "point SRID 0"));
    let checks: Vec<(&str, &str)> = docs.checks.iter().map(|c| (c.name.as_deref().unwrap(), c.expression.as_str())).collect();
    assert_eq!(checks, [("ck_estado", "(`estado` <> _utf8mb4'x') NOT ENFORCED"), ("ck_precio", "(`precio` > 0)")]);
    assert_eq!(ix(&ta["mem"], "ib").kind.as_deref(), Some("BTREE"));
}

#[tokio::test]
#[ignore]
async fn mariadb() {
    let case = Case {
        id: "mariadb",
        env: "DBINE_TEST_MARIADB_URL",
        source: "
CREATE SEQUENCE seq_folio START WITH 100 INCREMENT BY 5 CACHE 10;
CREATE TABLE docs (
  id int NOT NULL PRIMARY KEY,
  titulo varchar(200) NOT NULL,
  cuerpo text,
  g point NOT NULL,
  precio int,
  q int CHECK (q < 20),
  j json,
  estado varchar(10),
  CONSTRAINT ck_precio CHECK (precio > 0),
  KEY ix_pre (titulo(20) DESC, precio) COMMENT 'prefijo',
  KEY ix_ig (precio) IGNORED,
  FULLTEXT KEY ft (cuerpo),
  SPATIAL KEY sp (g),
  UNIQUE KEY u (cuerpo) USING HASH
)",
        target: "
CREATE SEQUENCE seq_folio;
CREATE SEQUENCE seq_extra;
CREATE TABLE docs (
  id int NOT NULL PRIMARY KEY,
  titulo varchar(200) NOT NULL,
  cuerpo text,
  g point NOT NULL,
  precio int,
  q int CHECK (q < 10),
  j longtext,
  estado varchar(10),
  CONSTRAINT ck_precio CHECK (precio > 10),
  CONSTRAINT ck_viejo CHECK (precio < 1000),
  KEY ix_pre (titulo(10), precio),
  KEY ix_ig (precio)
)",
        changed: &["docs"],
        settle: 0,
    };
    let Some((ta, oa)) = compare(&case).await else { return };
    let docs = &ta["docs"];
    let pre = ix(docs, "ix_pre");
    assert_eq!((opt(pre, "desc"), opt(pre, "COMMENT")), (Some("titulo(20)"), Some("prefijo")));
    assert_eq!(opt(ix(docs, "ix_ig"), "IGNORED"), Some("YES"));
    assert_eq!(ix(docs, "u").kind.as_deref(), Some("HASH"));
    let checks: Vec<(&str, &str)> = docs.checks.iter().map(|c| (c.name.as_deref().unwrap(), c.expression.as_str())).collect();
    assert_eq!(checks, [("ck_precio", "`precio` > 0")]);
    // Column CHECKs ride in the column.
    let ty = |n: &str| docs.columns.iter().find(|c| c.name == n).unwrap().data_type.clone();
    assert_eq!((ty("q").as_str(), ty("j").as_str()), ("int(11) CHECK (`q` < 20)", "longtext CHECK (json_valid(`j`))"));
    let seq = &oa[&("sequence".to_string(), "seq_folio".to_string())];
    assert!(seq.starts_with("CREATE SEQUENCE `seq_folio` start with 100") && seq.contains("increment by 5 cache 10"), "{seq}");
}

#[tokio::test]
#[ignore]
async fn tidb() {
    let Ok(url) = std::env::var("DBINE_TEST_TIDB_URL") else {
        eprintln!("DBINE_TEST_TIDB_URL not set; skipping");
        return;
    };
    let d = driver("tidb");
    let mut admin = d.connect(&parse_url("tidb", &url), None).await.unwrap();
    let was = run(&mut admin, "SELECT @@GLOBAL.tidb_enable_check_constraint").await.unwrap().results[0].rows[0][0].to_string().trim_matches('"').to_string();
    run(&mut admin, "SET GLOBAL tidb_enable_check_constraint = ON").await.unwrap();
    let case = Case {
        id: "tidb",
        env: "DBINE_TEST_TIDB_URL",
        source: "
CREATE SEQUENCE seq_folio START WITH 100 INCREMENT BY 5 CACHE 10;
CREATE TABLE docs (
  id int NOT NULL PRIMARY KEY NONCLUSTERED,
  titulo varchar(200) NOT NULL,
  precio int,
  estado varchar(10),
  CONSTRAINT ck_precio CHECK (precio > 0),
  KEY ix_pre (titulo(20), precio) COMMENT 'prefijo',
  KEY ix_fn ((lower(titulo))) INVISIBLE,
  UNIQUE KEY u (estado)
);
CREATE TABLE agrupada (id varchar(10) PRIMARY KEY CLUSTERED, n int CHECK (n <> 0))",
        target: "
CREATE SEQUENCE seq_folio;
CREATE SEQUENCE seq_extra;
CREATE TABLE docs (
  id int NOT NULL PRIMARY KEY NONCLUSTERED,
  titulo varchar(200) NOT NULL,
  precio int,
  estado varchar(10),
  CONSTRAINT ck_precio CHECK (precio > 10),
  CONSTRAINT ck_viejo CHECK (precio < 1000),
  KEY ix_pre (titulo(10), precio),
  KEY ix_fn ((upper(titulo))),
  KEY u (estado)
)",
        changed: &["agrupada", "docs"],
        settle: 0,
    };
    // In a task of its own, so the setting goes back even if it fails.
    let res = tokio::spawn(async move { compare(&case).await }).await;
    run(&mut admin, &format!("SET GLOBAL tidb_enable_check_constraint = {}", if was == "1" || was.eq_ignore_ascii_case("on") { "ON" } else { "OFF" })).await.unwrap();
    let Some((ta, oa)) = res.unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic())) else { return };
    let docs = &ta["docs"];
    assert_eq!(docs.options.get("clustered_index").map(String::as_str), Some("NONCLUSTERED"));
    assert_eq!(ta["agrupada"].options.get("clustered_index").map(String::as_str), Some("CLUSTERED"));
    assert_eq!((ix(docs, "ix_pre").columns[0].as_str(), opt(ix(docs, "ix_pre"), "COMMENT")), ("titulo(20)", Some("prefijo")));
    assert_eq!((ix(docs, "ix_fn").columns[0].as_str(), opt(ix(docs, "ix_fn"), "INVISIBLE")), ("(lower(`titulo`))", Some("YES")));
    assert_eq!(docs.checks.len(), 1, "{:?}", docs.checks);
    assert_eq!(ta["agrupada"].checks.len(), 1);
    assert!(oa[&("sequence".to_string(), "seq_folio".to_string())].contains("start with 100"));
}

#[tokio::test]
#[ignore]
async fn starrocks() {
    let props = "DUPLICATE KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 1";
    let source = format!(
        "CREATE TABLE docs (id int NOT NULL, a varchar(50), b varchar(200), p int,
           INDEX ix_a (a) USING BITMAP COMMENT 'bm', INDEX ng (b) USING NGRAMBF ('gram_num' = '4', 'bloom_filter_fpp' = '0.05'))
         {props} PROPERTIES ('replication_num' = '1', 'bloom_filter_columns' = 'p');
         ALTER TABLE docs ADD ROLLUP r_ab (a, b)"
    );
    let target = format!(
        "CREATE TABLE docs (id int NOT NULL, a varchar(50), b varchar(200), p int,
           INDEX ix_a (a) USING BITMAP, INDEX ng (b) USING NGRAMBF ('gram_num' = '3', 'bloom_filter_fpp' = '0.05'))
         {props} PROPERTIES ('replication_num' = '1');
         ALTER TABLE docs ADD ROLLUP r_ab (b)"
    );
    let case = Case {
        id: "starrocks",
        env: "DBINE_TEST_STARROCKS_URL",
        source: Box::leak(source.into_boxed_str()),
        target: Box::leak(target.into_boxed_str()),
        changed: &["docs"],
        settle: 180,
    };
    let Some((ta, _)) = compare(&case).await else { return };
    let docs = &ta["docs"];
    assert_eq!((ix(docs, "ix_a").kind.as_deref(), opt(ix(docs, "ix_a"), "COMMENT")), (Some("BITMAP"), Some("bm")));
    assert_eq!((ix(docs, "ng").kind.as_deref(), opt(ix(docs, "ng"), "gram_num")), (Some("NGRAMBF"), Some("4")));
    assert_eq!(docs.options.get("bloom_filter_columns").map(String::as_str), Some("p"));
    assert_eq!((ix(docs, "r_ab").kind.as_deref(), ix(docs, "r_ab").columns.as_slice()), (Some("ROLLUP"), &["a".to_string(), "b".into()][..]));
}

#[tokio::test]
#[ignore]
async fn greptimedb() {
    let case = Case {
        id: "greptimedb",
        env: "DBINE_TEST_GREPTIMEDB_URL",
        source: "CREATE TABLE m (ts TIMESTAMP TIME INDEX, host STRING INVERTED INDEX, msg STRING FULLTEXT INDEX WITH(analyzer = 'English', case_sensitive = 'false'), tr STRING SKIPPING INDEX WITH(granularity = '1024'), v DOUBLE, PRIMARY KEY(host)) WITH (ttl = '7d', 'compaction.type' = 'twcs', 'compaction.twcs.time_window' = '1h')",
        target: "CREATE TABLE m (ts TIMESTAMP TIME INDEX, host STRING, msg STRING FULLTEXT INDEX WITH(analyzer = 'English', case_sensitive = 'false', granularity = '2048'), tr STRING, v DOUBLE, PRIMARY KEY(host)) WITH (ttl = '1d')",
        changed: &["m"],
        settle: 0,
    };
    let Some((ta, _)) = compare(&case).await else { return };
    let m = &ta["m"];
    assert_eq!(ix(m, "INVERTED_INDEX_host").columns, ["host"]);
    assert_eq!(opt(ix(m, "FULLTEXT_INDEX_msg"), "analyzer"), Some("English"));
    assert_eq!(opt(ix(m, "SKIPPING_INDEX_tr"), "granularity"), Some("1024"));
    assert_eq!(m.options.get("compaction.twcs.time_window").map(String::as_str), Some("1h"));
    assert!(m.options.contains_key("ttl"));
}

/// Manticore has a single namespace: the target table is made with other
/// settings, then synced to the source's.
#[tokio::test]
#[ignore]
async fn manticore() {
    let Ok(url) = std::env::var("DBINE_TEST_MANTICORE_URL") else {
        eprintln!("DBINE_TEST_MANTICORE_URL not set; skipping");
        return;
    };
    let d = driver("manticore");
    let mut s = d.connect(&parse_url("manticore", &url), None).await.expect("connect");
    let read = |t: Vec<TableSchema>, n: &str| t.into_iter().find(|t| t.name == n).unwrap_or_else(|| panic!("{n}"));
    run(&mut s, "DROP TABLE IF EXISTS dbine_cmp_src; DROP TABLE IF EXISTS dbine_cmp").await.unwrap();
    run(&mut s, "CREATE TABLE dbine_cmp_src (title text, n integer) morphology='stem_en' min_infix_len='3'").await.unwrap();
    run(&mut s, "CREATE TABLE dbine_cmp (title text, n integer) min_infix_len='2'").await.unwrap();
    let mut src = read(s.database_schema().await.unwrap(), "dbine_cmp_src");
    let old = read(s.database_schema().await.unwrap(), "dbine_cmp");
    eprintln!("manticore: {src:#?}\n{old:#?}");
    assert_eq!(src.options.get("morphology").map(String::as_str), Some("stem_en"));
    src.name = "dbine_cmp".into();
    assert_ne!(src, old);
    let script = d.sync_script(&[TableChange::Alter { old, new: src.clone() }]).unwrap();
    for st in &script.statements {
        eprintln!("{st}");
        run(&mut s, st).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let now = read(s.database_schema().await.unwrap(), "dbine_cmp");
    assert_eq!(now, src);
    // And a copy made from the DDL has them too.
    let mut copy = src.clone();
    copy.name = "dbine_cmp_src".into();
    let ddl = d.table_ddl(&copy, dbine_driver::DdlParts { drop: true, if_exists: true, create: true, ..Default::default() }).unwrap();
    for st in ddl.split(";\n") {
        run(&mut s, st).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    assert_eq!(read(s.database_schema().await.unwrap(), "dbine_cmp_src").options, src.options);
    run(&mut s, "DROP TABLE dbine_cmp_src; DROP TABLE dbine_cmp").await.unwrap();
}

/// Table and column comments: added, changed and removed by the sync, per
/// engine (MySQL and MariaDB with `ALTER TABLE … COMMENT =` and `MODIFY
/// COLUMN`, StarRocks the same, GreptimeDB with `COMMENT ON`).
#[tokio::test]
#[ignore]
async fn comments() {
    let mysql = |table_opts: &str| {
        let t = |name: &str, tc: Option<&str>, cc: Option<&str>| {
            let cc = cc.map(|c| format!(" COMMENT '{c}'")).unwrap_or_default();
            let tc = tc.map(|c| format!(" COMMENT='{c}'")).unwrap_or_default();
            format!("CREATE TABLE {name} (id int NOT NULL PRIMARY KEY, nombre varchar(50) NULL{cc}){tc}{table_opts}")
        };
        (
            [t("c_add", Some("Clientes"), Some("El nombre")), t("c_change", Some("Nuevo"), Some("nuevo")), t("c_remove", None, None)].join(";\n"),
            [t("c_add", None, None), t("c_change", Some("Viejo"), Some("viejo")), t("c_remove", Some("Se va"), Some("se va"))].join(";\n"),
        )
    };
    let starrocks = {
        let t = |name: &str, tc: Option<&str>, cc: Option<&str>| {
            let cc = cc.map(|c| format!(" COMMENT '{c}'")).unwrap_or_default();
            let tc = tc.map(|c| format!(" COMMENT '{c}'")).unwrap_or_default();
            format!("CREATE TABLE {name} (id int NOT NULL, nombre varchar(50) NULL{cc}) DUPLICATE KEY(id){tc} DISTRIBUTED BY HASH(id) BUCKETS 1 PROPERTIES ('replication_num' = '1')")
        };
        (
            [t("c_add", Some("Clientes"), Some("El nombre")), t("c_change", Some("Nuevo"), Some("nuevo")), t("c_remove", None, None)].join(";\n"),
            [t("c_add", None, None), t("c_change", Some("Viejo"), Some("viejo")), t("c_remove", Some("Se va"), Some("se va"))].join(";\n"),
        )
    };
    let greptime = {
        let t = |name: &str, tc: Option<&str>, cc: Option<&str>| {
            let cc = cc.map(|c| format!(" COMMENT '{c}'")).unwrap_or_default();
            let tc = tc.map(|c| format!(" WITH (comment = '{c}')")).unwrap_or_default();
            format!("CREATE TABLE {name} (ts TIMESTAMP TIME INDEX, host STRING, v DOUBLE{cc}, PRIMARY KEY(host)){tc}")
        };
        (
            [t("c_add", Some("Clientes"), Some("El valor")), t("c_change", Some("Nuevo"), Some("nuevo")), t("c_remove", None, None)].join(";\n"),
            [t("c_add", None, None), t("c_change", Some("Viejo"), Some("viejo")), t("c_remove", Some("Se va"), Some("se va"))].join(";\n"),
        )
    };
    let cases = [
        ("mysql", "DBINE_TEST_MYSQL_URL", mysql(""), 0),
        ("mariadb", "DBINE_TEST_MARIADB_URL", mysql(""), 0),
        ("starrocks", "DBINE_TEST_STARROCKS_URL", starrocks, 180),
        ("greptimedb", "DBINE_TEST_GREPTIMEDB_URL", greptime, 0),
    ];
    for (id, env, (source, target), settle) in cases {
        let case = Case { id, env, source: Box::leak(source.into_boxed_str()), target: Box::leak(target.into_boxed_str()), changed: &["c_add", "c_change", "c_remove"], settle };
        let Some((ta, _)) = compare(&case).await else { continue };
        assert_eq!(ta["c_add"].comment.as_deref(), Some("Clientes"), "{id}");
        assert!(ta["c_add"].columns.iter().any(|c| c.comment.is_some()), "{id}: {:#?}", ta["c_add"]);
        assert!(ta["c_remove"].comment.is_none() && ta["c_remove"].columns.iter().all(|c| c.comment.is_none()), "{id}: {:#?}", ta["c_remove"]);
    }
}

/// What "Eliminar" in the compare sends for one side: its tables changed by
/// hand (an `Alter` without the item, a `Drop` of the table plus an `Alter`
/// without each FK that references it) and the objects dropped as
/// src-tauri's `drop_other` writes them (`DROP <KIND> IF EXISTS `name``).
/// Object drops go before the tables' statements, as `plan()` puts them.
async fn drop_on(d: &dyn Driver, s: &mut Box<dyn Session>, tables: Vec<TableChange>, objects: &[(&str, &str)]) -> Result<Vec<String>, String> {
    let keyword = |k: &str| match k {
        "view" => "VIEW",
        "procedure" => "PROCEDURE",
        "function" => "FUNCTION",
        "trigger" => "TRIGGER",
        _ => unreachable!("{k}"),
    };
    let mut statements: Vec<String> = objects.iter().map(|(k, n)| format!("DROP {} IF EXISTS `{n}`;", keyword(k))).collect();
    if !tables.is_empty() {
        let script = d.sync_script(&tables).map_err(|e| e.to_string())?;
        for w in &script.warnings {
            eprintln!("aviso: {w}");
        }
        statements.extend(script.statements);
    }
    for st in &statements {
        eprintln!("{st}");
        run(s, st).await.map_err(|e| format!("{st}: {e}"))?;
    }
    Ok(statements)
}

/// The tables without the views (the compare lists views as objects).
async fn tables_of(s: &mut Box<dyn Session>) -> BTreeMap<String, TableSchema> {
    let views: Vec<String> = s.list_objects().await.unwrap().into_iter().filter(|o| o.kind == "view").map(|o| o.name).collect();
    read(s).await.0.into_iter().filter(|(n, _)| !views.contains(n)).collect()
}

async fn code_objects(s: &mut Box<dyn Session>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = s.list_objects().await.unwrap().into_iter().filter(|o| ["view", "procedure", "function", "trigger"].contains(&o.kind.as_str())).map(|o| (o.kind, o.name)).collect();
    v.sort();
    v
}

fn without(t: &TableSchema, f: impl FnOnce(&mut TableSchema)) -> TableChange {
    let mut new = t.clone();
    f(&mut new);
    assert_ne!(&new, t, "nothing removed from {}", t.name);
    TableChange::Alter { old: t.clone(), new }
}

/// "Eliminar" in "Comparar esquemas": an index dropped on one side and on
/// both (they then compare equal), a column, a CHECK, an FK, the primary
/// key, a table other tables reference (its FKs go first), and views,
/// routines and a trigger. After each run the side is read again and must
/// be exactly what the compare asked for.
#[tokio::test]
#[ignore]
async fn drops() {
    const SETUP: &str = "
CREATE TABLE padre (id int NOT NULL PRIMARY KEY, codigo int, UNIQUE KEY u_codigo (codigo));
CREATE TABLE hijo (
  id int NOT NULL PRIMARY KEY,
  padre_id int,
  n int,
  extra varchar(10),
  sin_uso int,
  KEY ix_sin_uso (sin_uso),
  KEY ix_n (n),
  CONSTRAINT fk_hijo_padre FOREIGN KEY (padre_id) REFERENCES padre (id),
  CONSTRAINT ck_n CHECK (n > 0)
);
CREATE TABLE otro (id int NOT NULL PRIMARY KEY, padre_id int, CONSTRAINT fk_otro_padre FOREIGN KEY (padre_id) REFERENCES padre (id));
CREATE TABLE nieto (id int NOT NULL PRIMARY KEY, hijo_id int, CONSTRAINT fk_nieto_hijo FOREIGN KEY (hijo_id) REFERENCES hijo (id));
CREATE TABLE suelta (id int NOT NULL, v int, PRIMARY KEY (id));
CREATE VIEW v_hijo AS SELECT id, n FROM hijo;
CREATE VIEW v_v AS SELECT id FROM v_hijo;
CREATE FUNCTION f_doble(x int) RETURNS int DETERMINISTIC RETURN x * 2;
CREATE PROCEDURE p_nada() BEGIN SELECT 1; END;
CREATE TRIGGER tg_hijo BEFORE INSERT ON hijo FOR EACH ROW SET NEW.n = f_doble(NEW.n)";
    for (id, env) in [("mysql", "DBINE_TEST_MYSQL_URL"), ("mariadb", "DBINE_TEST_MARIADB_URL")] {
        let Ok(url) = std::env::var(env) else {
            eprintln!("{env} not set; skipping");
            continue;
        };
        let cfg = parse_url(id, &url);
        let d = driver(id);
        let mut admin = d.connect(&cfg, None).await.expect("connect");
        eprintln!("{id}: {}", admin.server_version().await.unwrap());
        let (a_db, b_db) = ("dbine_drop_a", "dbine_drop_b");
        for db in [a_db, b_db] {
            run(&mut admin, &format!("DROP DATABASE IF EXISTS {db}")).await.unwrap();
            admin.create_database(db).await.expect("create_database");
        }
        let mut a = d.connect(&cfg, Some(a_db)).await.unwrap();
        let mut b = d.connect(&cfg, Some(b_db)).await.unwrap();
        for s in [&mut a, &mut b] {
            for stmt in SETUP.split(";\n").map(str::trim).filter(|x| !x.is_empty()) {
                run(s, stmt).await.unwrap_or_else(|e| panic!("{id}: {stmt}: {e}"));
            }
        }
        let d = d.as_ref();

        // The same unused index on both sides: one script per side, and
        // the two then compare equal.
        let (ta, tb) = (tables_of(&mut a).await, tables_of(&mut b).await);
        assert_eq!(ta, tb, "{id}: same setup");
        for (s, t) in [(&mut a, &ta), (&mut b, &tb)] {
            drop_on(d, s, vec![without(&t["hijo"], |t| t.indexes.retain(|i| i.name != "ix_sin_uso"))], &[]).await.unwrap();
        }
        let (ta, tb) = (tables_of(&mut a).await, tables_of(&mut b).await);
        assert!(ta["hijo"].indexes.iter().all(|i| i.name != "ix_sin_uso"), "{id}: {:#?}", ta["hijo"]);
        assert_eq!(ta, tb, "{id}: both sides dropped it");

        // One side at a time: each item the detail pane can drop.
        type Edit = fn(&mut TableSchema);
        let steps: [(&str, &str, Edit); 5] = [
            ("index", "hijo", |t| t.indexes.retain(|i| i.name != "ix_n")),
            ("column", "hijo", |t| t.columns.retain(|c| c.name != "extra")),
            ("check", "hijo", |t| t.checks.retain(|c| c.name.as_deref() != Some("ck_n"))),
            ("foreign key", "nieto", |t| t.foreign_keys.retain(|f| f.name.as_deref() != Some("fk_nieto_hijo"))),
            ("primary key", "suelta", |t| t.primary_key = None),
        ];
        for (what, table, edit) in steps {
            let ta = tables_of(&mut a).await;
            let change = without(&ta[table], edit);
            let TableChange::Alter { new, .. } = &change else { unreachable!() };
            let new = new.clone();
            drop_on(d, &mut a, vec![change], &[]).await.unwrap_or_else(|e| panic!("{id}: {what}: {e}"));
            assert_eq!(tables_of(&mut a).await[table], new, "{id}: {what} after the drop");
        }
        // B kept all of it.
        assert_eq!(tables_of(&mut b).await, tb, "{id}: the other side is untouched");

        // The index MySQL keeps for an FK can't go while the FK is there:
        // the compare only warns about it.
        let ta = tables_of(&mut a).await;
        let backing = ta["hijo"].indexes.iter().find(|i| i.columns == ["padre_id"]).expect("FK index").name.clone();
        let err = drop_on(d, &mut a, vec![without(&ta["hijo"], |t| t.indexes.retain(|i| i.name != backing))], &[]).await.expect_err("FK index");
        eprintln!("{id}: the FK's index: {err}");

        // A table two others reference: the UI takes their FKs out too,
        // and the script drops them before the table (listed first here).
        let ta = tables_of(&mut a).await;
        let refs = |t: &mut TableSchema| t.foreign_keys.retain(|f| f.ref_table != "padre");
        let (hijo, otro) = (without(&ta["hijo"], refs), without(&ta["otro"], refs));
        let expected: Vec<TableSchema> = [&hijo, &otro].map(|c| match c {
            TableChange::Alter { new, .. } => new.clone(),
            _ => unreachable!(),
        }).into();
        let script = drop_on(d, &mut a, vec![TableChange::Drop { table: ta["padre"].clone() }, hijo, otro], &[]).await.unwrap_or_else(|e| panic!("{id}: table: {e}"));
        let at = |p: &str| script.iter().position(|s| s.contains(p)).unwrap_or_else(|| panic!("{p} in {script:#?}"));
        assert!(at("fk_hijo_padre") < at("DROP TABLE") && at("fk_otro_padre") < at("DROP TABLE"), "{id}: {script:#?}");
        let now = tables_of(&mut a).await;
        assert!(!now.contains_key("padre"), "{id}");
        assert_eq!([now["hijo"].clone(), now["otro"].clone()], expected[..], "{id}");

        // Code objects, in an order that ignores what uses what: MySQL
        // drops them anyway.
        drop_on(d, &mut a, Vec::new(), &[("function", "f_doble"), ("view", "v_hijo"), ("view", "v_v"), ("trigger", "tg_hijo"), ("procedure", "p_nada")]).await.unwrap_or_else(|e| panic!("{id}: objects: {e}"));
        assert_eq!(code_objects(&mut a).await, Vec::<(String, String)>::new(), "{id}");
        assert_eq!(code_objects(&mut b).await.len(), 5, "{id}: B keeps its objects");

        drop(a);
        drop(b);
        for db in [a_db, b_db] {
            admin.drop_database(db).await.expect("drop_database");
        }
    }
}

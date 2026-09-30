//! "Comparar esquemas" against a real server: two databases that differ in
//! every kind of thing the compare reads (full-text catalog, stoplist and
//! index, INCLUDE, index options, UNIQUE constraint, CHECK, sequence,
//! synonym, alias and table types, XML, columnstore and spatial indexes).
//! The sync script is run on one side and both must then read the same.
//!
//! The full-text parts need Full-Text Search on the server (the stock
//! image has none: build one that installs `mssql-server-fts`);
//! without it they're skipped. Reads `DBINE_TEST_SQLSERVER_FTS_URL`, or
//! `DBINE_TEST_SQLSERVER_URL`:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_FTS_URL='mssql://sa:Pw_12345!@localhost:26030' \
//!   cargo test -p dbine-driver-sqlserver --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;

fn parse_url(url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

const SOURCE: &str = "
CREATE TYPE dbo.Email FROM nvarchar(120) NOT NULL;
CREATE TYPE dbo.Lineas AS TABLE (
    id int NOT NULL PRIMARY KEY,
    cant int NOT NULL CHECK (cant > 0),
    nota nvarchar(50) NULL DEFAULT N'-'
);
CREATE SEQUENCE dbo.seq_folio AS bigint START WITH 100 INCREMENT BY 5 CACHE 10;
GO
CREATE TABLE dbo.docs (
    id int NOT NULL CONSTRAINT PK_docs PRIMARY KEY,
    codigo varchar(20) NOT NULL,
    titulo nvarchar(200) NOT NULL,
    cuerpo nvarchar(max) NULL,
    autor nvarchar(100) NULL,
    fecha date NOT NULL,
    CONSTRAINT UQ_docs_codigo UNIQUE (codigo),
    CONSTRAINT CK_docs_fecha CHECK (fecha >= '2000-01-01')
);
CREATE INDEX IX_docs_fecha ON dbo.docs (fecha DESC) INCLUDE (titulo);
CREATE INDEX IX_docs_autor ON dbo.docs (autor) WITH (FILLFACTOR = 80, DATA_COMPRESSION = PAGE);
CREATE SYNONYM dbo.syn_docs FOR dbo.docs;
CREATE TABLE dbo.xmldocs (id int NOT NULL CONSTRAINT PK_xmldocs PRIMARY KEY CLUSTERED, doc xml NULL);
CREATE PRIMARY XML INDEX PX_xmldocs ON dbo.xmldocs (doc);
CREATE XML INDEX SX_xmldocs_path ON dbo.xmldocs (doc) USING XML INDEX PX_xmldocs FOR PATH;
CREATE TABLE dbo.ventas (id int NOT NULL CONSTRAINT PK_ventas PRIMARY KEY, a int NULL, b int NULL);
CREATE NONCLUSTERED COLUMNSTORE INDEX NCCI_ventas ON dbo.ventas (a, b);
CREATE TABLE dbo.hechos (k int NOT NULL, v decimal(18,2) NULL);
CREATE CLUSTERED COLUMNSTORE INDEX CCI_hechos ON dbo.hechos;
CREATE TABLE dbo.geo (id int NOT NULL CONSTRAINT PK_geo PRIMARY KEY CLUSTERED, g geometry NULL);
CREATE SPATIAL INDEX SP_geo ON dbo.geo (g) USING GEOMETRY_GRID
    WITH (BOUNDING_BOX = (0, 0, 100, 100), GRIDS = (LEVEL_1 = LOW, LEVEL_2 = MEDIUM, LEVEL_3 = MEDIUM, LEVEL_4 = HIGH), CELLS_PER_OBJECT = 8);
";

const SOURCE_FT: &str = "
CREATE FULLTEXT CATALOG cat_docs WITH ACCENT_SENSITIVITY = OFF AS DEFAULT;
CREATE FULLTEXT STOPLIST sl_docs FROM SYSTEM STOPLIST;
ALTER FULLTEXT STOPLIST sl_docs ADD 'dbine' LANGUAGE 1033;
ALTER FULLTEXT STOPLIST sl_docs DROP 'and' LANGUAGE 1033;
CREATE FULLTEXT INDEX ON dbo.docs (titulo, cuerpo LANGUAGE 3082) KEY INDEX PK_docs ON cat_docs
    WITH (CHANGE_TRACKING = MANUAL, STOPLIST = sl_docs);
";

const TARGET: &str = "
CREATE TYPE dbo.Email FROM nvarchar(100) NULL;
CREATE SEQUENCE dbo.seq_folio AS bigint START WITH 1 INCREMENT BY 1;
CREATE SEQUENCE dbo.seq_extra AS int;
GO
CREATE TABLE dbo.docs (
    id int NOT NULL CONSTRAINT PK_docs PRIMARY KEY,
    codigo varchar(20) NOT NULL,
    titulo nvarchar(200) NOT NULL,
    cuerpo nvarchar(max) NULL,
    autor nvarchar(100) NULL,
    fecha date NOT NULL,
    CONSTRAINT CK_docs_fecha CHECK (fecha >= '1990-01-01')
);
CREATE UNIQUE INDEX UQ_docs_codigo ON dbo.docs (codigo);
CREATE INDEX IX_docs_fecha ON dbo.docs (fecha DESC);
CREATE INDEX IX_docs_autor ON dbo.docs (autor);
CREATE TABLE dbo.xmldocs (id int NOT NULL CONSTRAINT PK_xmldocs PRIMARY KEY CLUSTERED, doc xml NULL);
CREATE PRIMARY XML INDEX PX_xmldocs ON dbo.xmldocs (doc);
CREATE TABLE dbo.ventas (id int NOT NULL CONSTRAINT PK_ventas PRIMARY KEY, a int NULL, b int NULL);
CREATE TABLE dbo.hechos (k int NOT NULL, v decimal(18,2) NULL);
CREATE TABLE dbo.geo (id int NOT NULL CONSTRAINT PK_geo PRIMARY KEY CLUSTERED, g geometry NULL);
CREATE SPATIAL INDEX SP_geo ON dbo.geo (g) USING GEOMETRY_GRID
    WITH (BOUNDING_BOX = (0, 0, 50, 50), GRIDS = (LEVEL_1 = LOW, LEVEL_2 = MEDIUM, LEVEL_3 = MEDIUM, LEVEL_4 = HIGH), CELLS_PER_OBJECT = 8);
";

const TARGET_FT: &str = "
CREATE FULLTEXT CATALOG cat_docs WITH ACCENT_SENSITIVITY = ON AS DEFAULT;
CREATE FULLTEXT STOPLIST sl_docs FROM SYSTEM STOPLIST;
CREATE FULLTEXT INDEX ON dbo.docs (titulo) KEY INDEX PK_docs ON cat_docs WITH (STOPLIST = SYSTEM);
";

/// The object kinds the compare reads by their source.
const CODE_KINDS: &[&str] = &["view", "procedure", "function", "trigger", "sequence", "synonym", "type", "fulltext_catalog", "fulltext_stoplist"];
/// Kinds tables depend on: made before the tables, dropped after them.
const PREREQS: &[&str] = &["type", "sequence", "fulltext_catalog", "fulltext_stoplist"];
/// Kinds whose source makes or changes them in place (never dropped to be replaced).
const IN_PLACE: &[&str] = &["fulltext_catalog", "fulltext_stoplist"];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Create,
    Drop,
    Replace,
}

type Objects = BTreeMap<(String, Option<String>, String), String>;

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
        if !CODE_KINDS.contains(&o.kind.as_str()) {
            continue;
        }
        let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        let def = s.definition(&r).await.expect("definition").unwrap_or_else(|| panic!("no definition for {r:?}"));
        objects.insert((o.kind, o.schema, o.name), def);
    }
    (tables, objects)
}

/// What `drop_other` (src-tauri) writes, with the full-text kinds.
fn drop_other(kind: &str, schema: Option<&str>, name: &str) -> String {
    let full = match schema {
        Some(s) => format!("[{s}].[{name}]"),
        None => format!("[{name}]"),
    };
    match kind {
        "fulltext_catalog" => format!("IF EXISTS (SELECT 1 FROM sys.fulltext_catalogs WHERE name = N'{name}')\n    DROP FULLTEXT CATALOG [{name}];"),
        "fulltext_stoplist" => format!("IF EXISTS (SELECT 1 FROM sys.fulltext_stoplists WHERE name = N'{name}')\n    DROP FULLTEXT STOPLIST [{name}];"),
        k => format!("DROP {} IF EXISTS {full};", k.to_uppercase()),
    }
}

/// `plan` in src-tauri's compare, in the order proposed for it: object
/// drops, prerequisites (types, sequences, full-text catalogs and
/// stoplists), tables, other objects, then dropped prerequisites.
fn plan(d: &dyn Driver, tables: &[TableChange], objects: &[(Op, String, Option<String>, String, String)]) -> Vec<String> {
    let (mut before, mut early, mut after, mut late) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (op, kind, schema, name, def) in objects {
        let prereq = PREREQS.contains(&kind.as_str());
        match op {
            Op::Drop if prereq => late.push(drop_other(kind, schema.as_deref(), name)),
            Op::Drop => before.push(drop_other(kind, schema.as_deref(), name)),
            Op::Replace if !IN_PLACE.contains(&kind.as_str()) => before.push(drop_other(kind, schema.as_deref(), name)),
            _ => {}
        }
        if *op != Op::Drop {
            if prereq { early.push(def.clone()) } else { after.push(def.clone()) }
        }
    }
    let script = d.sync_script(tables).expect("sync_script");
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    before.into_iter().chain(early).chain(script.statements).chain(after).chain(late).collect()
}

#[tokio::test]
#[ignore]
async fn compare_and_sync_everything() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_FTS_URL").or_else(|_| std::env::var("DBINE_TEST_SQLSERVER_URL")) else {
        eprintln!("DBINE_TEST_SQLSERVER_FTS_URL / DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    let kinds: Vec<&str> = d.info().object_kinds.iter().map(|k| k.id).collect();
    for k in ["sequence", "synonym", "type", "fulltext_catalog", "fulltext_stoplist"] {
        assert!(kinds.contains(&k), "{k} in {kinds:?}");
    }
    let mut admin = d.connect(&cfg, Some("master")).await.expect("connect");
    let mut out = QueryOutcome::default();
    admin.execute("SELECT CAST(SERVERPROPERTY('IsFullTextInstalled') AS int)", 1, &mut out).await.unwrap();
    let fts = out.results[0].rows[0][0] == serde_json::json!(1);
    if !fts {
        eprintln!("Full-Text Search is not installed: the full-text parts are skipped");
    }
    let (a_db, b_db) = ("dbine_cmp_a", "dbine_cmp_b");
    for db in [a_db, b_db] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create_database");
    }
    let mut a = d.connect(&cfg, Some(a_db)).await.unwrap();
    let mut b = d.connect(&cfg, Some(b_db)).await.unwrap();
    run(&mut a, SOURCE).await.unwrap();
    run(&mut b, TARGET).await.unwrap();
    if fts {
        run(&mut a, SOURCE_FT).await.unwrap();
        run(&mut b, TARGET_FT).await.unwrap();
    }

    let (ta, oa) = read(&mut a).await;
    let (tb, ob) = read(&mut b).await;
    for ((k, s, n), def) in &oa {
        eprintln!("-- {k} {s:?}.{n}\n{def}");
    }

    // What the catalog says.
    let docs = &ta["docs"];
    let ix = |t: &TableSchema, n: &str| t.indexes.iter().find(|i| i.name == n).cloned().unwrap_or_else(|| panic!("{n} in {:#?}", t.indexes));
    assert_eq!(ix(docs, "IX_docs_fecha").include, ["titulo"]);
    assert_eq!(ix(docs, "IX_docs_fecha").options.get("desc").map(String::as_str), Some("fecha"));
    assert_eq!(ix(docs, "IX_docs_autor").options.get("FILLFACTOR").map(String::as_str), Some("80"));
    assert_eq!(ix(docs, "IX_docs_autor").options.get("DATA_COMPRESSION").map(String::as_str), Some("PAGE"));
    assert!(ix(docs, "UQ_docs_codigo").options.contains_key("unique_constraint"));
    assert!(!ix(&tb["docs"], "UQ_docs_codigo").options.contains_key("unique_constraint"));
    assert_eq!(docs.checks.len(), 1);
    assert_eq!(docs.checks[0].name.as_deref(), Some("CK_docs_fecha"));
    assert_eq!(ix(&ta["xmldocs"], "SX_xmldocs_path").kind.as_deref(), Some("XML PATH"));
    assert_eq!(ix(&ta["xmldocs"], "SX_xmldocs_path").options.get("primary_xml_index").map(String::as_str), Some("PX_xmldocs"));
    assert_eq!(ix(&ta["ventas"], "NCCI_ventas").columns, ["a", "b"]);
    assert_eq!(ix(&ta["hechos"], "CCI_hechos").kind.as_deref(), Some("CLUSTERED COLUMNSTORE"));
    assert_eq!(ix(&ta["geo"], "SP_geo").options.get("BOUNDING_BOX").map(String::as_str), Some("(0, 0, 100, 100)"));
    assert_ne!(ix(&ta["geo"], "SP_geo").options, ix(&tb["geo"], "SP_geo").options);
    if fts {
        let ft = ix(docs, "fulltext");
        assert_eq!(ft.kind.as_deref(), Some("FULLTEXT"));
        assert_eq!(ft.columns, ["titulo", "cuerpo"]);
        let o = |k: &str| ft.options.get(k).map(String::as_str);
        assert_eq!((o("KEY INDEX"), o("CATALOG"), o("CHANGE_TRACKING"), o("STOPLIST"), o("LANGUAGE cuerpo")), (Some("PK_docs"), Some("cat_docs"), Some("MANUAL"), Some("sl_docs"), Some("3082")));
        let bft = ix(&tb["docs"], "fulltext");
        assert_eq!(bft.columns, ["titulo"]);
        assert_eq!(bft.options.keys().map(String::as_str).collect::<Vec<_>>(), ["CATALOG", "KEY INDEX"]);
        let cat = &oa[&("fulltext_catalog".into(), None, "cat_docs".into())];
        assert!(cat.contains("ACCENT_SENSITIVITY = OFF") && cat.contains("AS DEFAULT"), "{cat}");
        let sl = &oa[&("fulltext_stoplist".into(), None, "sl_docs".into())];
        assert!(sl.contains("FROM SYSTEM STOPLIST") && sl.contains("(N'dbine', 1033)") && sl.contains("(lang = 1033 AND word = N'and')"), "{sl}");
    }
    let obj = |o: &Objects, k: &str, n: &str| o.get(&(k.to_string(), Some("dbo".to_string()), n.to_string())).cloned();
    let seq = obj(&oa, "sequence", "seq_folio").unwrap();
    assert!(seq.contains("AS bigint") && seq.contains("START WITH 100") && seq.contains("INCREMENT BY 5") && seq.contains("CACHE 10"), "{seq}");
    assert_eq!(obj(&oa, "synonym", "syn_docs").as_deref(), Some("CREATE SYNONYM [dbo].[syn_docs] FOR [dbo].[docs];"));
    assert_eq!(obj(&oa, "type", "Email").as_deref(), Some("CREATE TYPE [dbo].[Email] FROM nvarchar(120) NOT NULL;"));
    assert_eq!(
        obj(&oa, "type", "Lineas").as_deref(),
        Some("CREATE TYPE [dbo].[Lineas] AS TABLE (\n    [id] int NOT NULL,\n    [cant] int NOT NULL,\n    [nota] nvarchar(50) DEFAULT (N'-') NULL,\n    PRIMARY KEY CLUSTERED ([id]),\n    CHECK ([cant]>(0))\n);")
    );

    // The compare: tables by name, objects by kind and name.
    let mut tables = Vec::new();
    for (name, t) in &ta {
        match tb.get(name) {
            None => tables.push(TableChange::Create { table: t.clone() }),
            Some(o) if o != t => tables.push(TableChange::Alter { old: o.clone(), new: t.clone() }),
            Some(_) => {}
        }
    }
    for (name, t) in &tb {
        if !ta.contains_key(name) {
            tables.push(TableChange::Drop { table: t.clone() });
        }
    }
    let changed: Vec<&str> = tables.iter().map(|c| match c {
        TableChange::Alter { new, .. } | TableChange::Create { table: new } | TableChange::Drop { table: new } => new.name.as_str(),
    }).collect();
    assert_eq!(changed, ["docs", "geo", "hechos", "ventas", "xmldocs"]);
    let mut objects = Vec::new();
    for ((k, s, n), def) in &oa {
        match ob.get(&(k.clone(), s.clone(), n.clone())) {
            None => objects.push((Op::Create, k.clone(), s.clone(), n.clone(), def.clone())),
            Some(o) if squash(o) != squash(def) => objects.push((Op::Replace, k.clone(), s.clone(), n.clone(), def.clone())),
            Some(_) => {}
        }
    }
    for ((k, s, n), def) in &ob {
        if !oa.contains_key(&(k.clone(), s.clone(), n.clone())) {
            objects.push((Op::Drop, k.clone(), s.clone(), n.clone(), def.clone()));
        }
    }
    let ops: Vec<(Op, &str)> = objects.iter().map(|(op, _, _, n, _)| (*op, n.as_str())).collect();
    let mut want = vec![(Op::Replace, "seq_folio"), (Op::Create, "syn_docs"), (Op::Replace, "Email"), (Op::Create, "Lineas"), (Op::Drop, "seq_extra")];
    if fts {
        want.splice(0..0, [(Op::Replace, "cat_docs"), (Op::Replace, "sl_docs")]);
    }
    assert_eq!(ops, want);

    // Sync B to A, then both read the same.
    let statements = plan(d.as_ref(), &tables, &objects);
    for (i, s) in statements.iter().enumerate() {
        if let Err(e) = run(&mut b, s).await {
            panic!("statement {i} failed: {e}\n---\n{s}\n---\nwhole script:\n{}", statements.join("\nGO\n"));
        }
    }
    let (tb2, ob2) = read(&mut b).await;
    for (name, t) in &ta {
        assert_eq!(tb2.get(name), Some(t), "table {name} after the sync");
    }
    assert_eq!(tb2.len(), ta.len());
    let squashed = |o: &Objects| o.iter().map(|(k, v)| (k.clone(), squash(v))).collect::<Vec<_>>();
    assert_eq!(squashed(&ob2), squashed(&oa));

    drop(a);
    drop(b);
    for db in [a_db, b_db] {
        admin.drop_database(db).await.expect("drop_database");
    }
}

/// Babelfish: alias and table types and CHECKs are read (its sys.sequences
/// is empty, and it has no synonyms nor full-text catalogs).
///
/// ```sh
/// DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:12345678@localhost:25714' \
///   cargo test -p dbine-driver-sqlserver --test compare babelfish -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn babelfish_objects() {
    let Ok(url) = std::env::var("DBINE_TEST_BABELFISH_URL") else {
        eprintln!("DBINE_TEST_BABELFISH_URL not set; skipping");
        return;
    };
    let mut cfg = parse_url(&url);
    cfg.driver = "babelfish".into();
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "babelfish").unwrap();
    let kinds: Vec<&str> = d.info().object_kinds.iter().map(|k| k.id).collect();
    assert_eq!(kinds.iter().filter(|k| ["sequence", "synonym", "type", "fulltext_catalog", "fulltext_stoplist"].contains(k)).collect::<Vec<_>>(), [&"type"]);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    if admin.list_databases().await.unwrap().contains(&"dbine_bbf_cmp".to_string()) {
        admin.drop_database("dbine_bbf_cmp").await.unwrap();
    }
    admin.create_database("dbine_bbf_cmp").await.expect("create database");
    let mut s = d.connect(&cfg, Some("dbine_bbf_cmp")).await.unwrap();
    run(
        &mut s,
        "CREATE TYPE dbo.Email FROM nvarchar(120) NOT NULL;
         CREATE TYPE dbo.Lineas AS TABLE (id int NOT NULL PRIMARY KEY, cant int NOT NULL);
         CREATE TABLE dbo.docs (id int NOT NULL PRIMARY KEY, fecha date NOT NULL, CONSTRAINT ck_docs_fecha CHECK (fecha >= '2000-01-01'));",
    )
    .await
    .unwrap();
    let (tables, objects) = read(&mut s).await;
    for ((k, sc, n), def) in &objects {
        eprintln!("-- {k} {sc:?}.{n}\n{def}");
    }
    let docs = &tables["docs"];
    assert_eq!(docs.checks.len(), 1, "{docs:#?}");
    let names: Vec<&str> = objects.keys().map(|(_, _, n)| n.as_str()).collect();
    for n in ["Email", "Lineas"] {
        assert!(names.iter().any(|x| x.eq_ignore_ascii_case(n)), "{n} in {names:?}");
    }
    drop(s);
    admin.drop_database("dbine_bbf_cmp").await.unwrap();
}

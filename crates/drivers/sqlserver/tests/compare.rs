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

/// Whether `text` names `name` as a whole word (any case), as src-tauri's
/// compare reads it.
fn mentions(text: &str, name: &str) -> bool {
    let (t, n) = (text.to_lowercase(), name.to_lowercase());
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
    let mut from = 0;
    while let Some(i) = t[from..].find(&n) {
        let at = from + i;
        if !word(t[..at].chars().last()) && !word(t[at + n.len()..].chars().next()) {
            return true;
        }
        from = at + n.len();
    }
    false
}

/// `(name, text)` items in the order src-tauri's `dependency_order` gives:
/// one whose text names another goes after it, or before it with
/// `dependents_first` (drops); a cycle is broken at its first item.
fn dependency_order(items: &[(&str, &str)], dependents_first: bool) -> Vec<usize> {
    let n = items.len();
    let edge = |i: usize, j: usize| !items[i].0.eq_ignore_ascii_case(items[j].0) && mentions(items[i].1, items[j].0);
    let mut left: Vec<usize> = (0..n).collect();
    let mut out = Vec::with_capacity(n);
    while !left.is_empty() {
        // Ready: nothing still left has to go before it.
        let waits = |a: usize| left.iter().any(|&b| b != a && if dependents_first { edge(b, a) } else { edge(a, b) });
        let at = left.iter().position(|&a| !waits(a)).unwrap_or(0);
        out.push(left.remove(at));
    }
    out
}

/// `plan` in src-tauri's compare: object drops (dependents first),
/// prerequisites (types, sequences, full-text catalogs and stoplists),
/// tables, other objects (in dependency order), then dropped prerequisites.
fn plan(d: &dyn Driver, tables: &[TableChange], objects: &[(Op, String, Option<String>, String, String)]) -> Vec<String> {
    let (mut before, mut early, mut after, mut late) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (op, kind, schema, name, def) in objects {
        let prereq = PREREQS.contains(&kind.as_str());
        let drop = (name.as_str(), def.as_str(), drop_other(kind, schema.as_deref(), name));
        match op {
            Op::Drop if prereq => late.push(drop),
            Op::Drop => before.push(drop),
            Op::Replace if !IN_PLACE.contains(&kind.as_str()) => before.push(drop),
            _ => {}
        }
        if *op != Op::Drop {
            if prereq { early.push(def.clone()) } else { after.push((name.as_str(), def.as_str())) }
        }
    }
    let drops = |list: Vec<(&str, &str, String)>| {
        let order = dependency_order(&list.iter().map(|(n, t, _)| (*n, *t)).collect::<Vec<_>>(), true);
        order.into_iter().map(|i| list[i].2.clone()).collect::<Vec<_>>()
    };
    let creates: Vec<String> = dependency_order(&after, false).into_iter().map(|i| after[i].1.to_string()).collect();
    let script = d.sync_script(tables).expect("sync_script");
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    drops(before).into_iter().chain(early).chain(script.statements).chain(creates).chain(drops(late)).collect()
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

async fn tables_of(s: &mut Box<dyn Session>) -> BTreeMap<String, TableSchema> {
    s.database_schema().await.expect("database_schema").into_iter().map(|t| (t.name.clone(), normalized(t))).collect()
}

/// Sync `target` to `want` (the other side's version of `name`) with the
/// driver's script, run it statement by statement, read again: equal.
async fn sync_table(d: &dyn Driver, target: &mut Box<dyn Session>, name: &str, want: &TableSchema) -> Vec<String> {
    let have = tables_of(target).await.remove(name).expect("table on the target");
    let script = d.sync_script(&[TableChange::Alter { old: have, new: want.clone() }]).expect("sync_script");
    for (i, s) in script.statements.iter().enumerate() {
        if let Err(e) = run(target, s).await {
            panic!("statement {i} failed: {e}\n---\n{s}\n---\nwhole script:\n{}", script.statements.join("\nGO\n"));
        }
    }
    let after = tables_of(target).await.remove(name).unwrap();
    assert_eq!(&after, want, "{name} after the sync:\n{}", script.statements.join("\nGO\n"));
    script.statements
}

/// MS_Description comments (table and column) carried both ways: added,
/// changed and removed, and on a column added by the same sync. And the
/// included columns of a nonclustered index read the same whether the table
/// is a heap, has a clustered key or a clustered columnstore index.
///
/// ```sh
/// DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
///   cargo test -p dbine-driver-sqlserver --test compare comments -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn comments_and_included_columns_sync() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    let mut admin = d.connect(&cfg, Some("master")).await.expect("connect");
    let (a_db, b_db) = ("dbine_cmt_a", "dbine_cmt_b");
    for db in [a_db, b_db] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create_database");
    }
    let mut a = d.connect(&cfg, Some(a_db)).await.unwrap();
    let mut b = d.connect(&cfg, Some(b_db)).await.unwrap();
    let table = "CREATE TABLE dbo.clientes (id int NOT NULL CONSTRAINT PK_clientes PRIMARY KEY, nombre nvarchar(50) NULL);";
    let details = |pk: &str, cci: bool| {
        format!(
            "CREATE TABLE dbo.EntityChangeDetails (
                 Id bigint NOT NULL CONSTRAINT PK_EntityChangeDetails PRIMARY KEY {pk},
                 Module nvarchar(50) NULL, FieldCode nvarchar(50) NULL, ValueType int NULL,
                 EntityChangeId bigint NOT NULL);
             {}
             CREATE NONCLUSTERED INDEX IX_EntityChangeDetails_EntityChangeId ON dbo.EntityChangeDetails (EntityChangeId)
                 INCLUDE (Id, Module, FieldCode, ValueType);",
            if cci { "CREATE CLUSTERED COLUMNSTORE INDEX CCI_EntityChangeDetails ON dbo.EntityChangeDetails;" } else { "" }
        )
    };
    run(&mut a, &format!("{table}\n{}", details("NONCLUSTERED", true))).await.unwrap();
    run(&mut b, &format!("{table}\n{}", details("NONCLUSTERED", false))).await.unwrap();
    // A third shape: the key clustered.
    run(&mut b, "CREATE TABLE dbo.ecd_clustered (Id bigint NOT NULL PRIMARY KEY CLUSTERED, Module nvarchar(50) NULL, EntityChangeId bigint NOT NULL);
                 CREATE INDEX IX_ecd_clustered ON dbo.ecd_clustered (EntityChangeId) INCLUDE (Id, Module);").await.unwrap();

    // Included columns: the same with and without the columnstore index.
    let ix = |t: &TableSchema, n: &str| t.indexes.iter().find(|i| i.name == n).cloned().unwrap_or_else(|| panic!("{n} in {:#?}", t.indexes));
    let (ta, tb) = (tables_of(&mut a).await, tables_of(&mut b).await);
    let name = "IX_EntityChangeDetails_EntityChangeId";
    for t in [&ta["EntityChangeDetails"], &tb["EntityChangeDetails"]] {
        let i = ix(t, name);
        assert_eq!(i.columns, ["EntityChangeId"], "{i:#?}");
        assert_eq!(i.include, ["Id", "Module", "FieldCode", "ValueType"], "{i:#?}");
    }
    assert_eq!(ix(&ta["EntityChangeDetails"], name), ix(&tb["EntityChangeDetails"], name));
    let c = ix(&tb["ecd_clustered"], "IX_ecd_clustered");
    assert_eq!((c.columns.as_slice(), c.include.as_slice()), (&["EntityChangeId".to_string()][..], &["Id".to_string(), "Module".to_string()][..]));
    assert_eq!(ix(&ta["EntityChangeDetails"], "CCI_EntityChangeDetails").kind.as_deref(), Some("CLUSTERED COLUMNSTORE"));
    // The columnstore index is the only difference, and the sync adds it.
    let s = sync_table(d.as_ref(), &mut b, "EntityChangeDetails", &ta["EntityChangeDetails"]).await;
    assert!(s.iter().any(|x| x.contains("CLUSTERED COLUMNSTORE INDEX")) && !s.iter().any(|x| x.contains(name)), "{s:#?}");

    // Comments, from A to B: added, changed (and a new column with one), removed.
    let add = |lvl: &str, v: &str| format!("EXEC sys.sp_addextendedproperty N'MS_Description', N'{v}', N'SCHEMA', N'dbo', N'TABLE', N'clientes'{lvl};");
    let upd = |lvl: &str, v: &str| format!("EXEC sys.sp_updateextendedproperty N'MS_Description', N'{v}', N'SCHEMA', N'dbo', N'TABLE', N'clientes'{lvl};");
    let del = |lvl: &str| format!("EXEC sys.sp_dropextendedproperty N'MS_Description', N'SCHEMA', N'dbo', N'TABLE', N'clientes'{lvl};");
    let col = |c: &str| format!(", N'COLUMN', N'{c}'");
    let steps = [
        ("add", format!("{}\n{}", add("", "Clientes"), add(&col("nombre"), "El nombre"))),
        (
            "change",
            format!(
                "{}\n{}\nALTER TABLE dbo.clientes ADD email nvarchar(100) NULL;\n{}",
                upd("", "Clientes activos"),
                upd(&col("nombre"), "Nombre y apellido, con ''comillas''"),
                add(&col("email"), "Correo")
            ),
        ),
        ("remove", format!("{}\n{}\n{}", del(""), del(&col("nombre")), del(&col("email")))),
    ];
    for (step, sql) in &steps {
        run(&mut a, sql).await.unwrap_or_else(|e| panic!("{step}: {e}"));
        let want = tables_of(&mut a).await.remove("clientes").unwrap();
        match *step {
            "add" => assert_eq!((want.comment.as_deref(), want.columns[1].comment.as_deref()), (Some("Clientes"), Some("El nombre"))),
            "change" => assert_eq!(want.columns[2].comment.as_deref(), Some("Correo")),
            _ => assert!(want.comment.is_none() && want.columns.iter().all(|c| c.comment.is_none()), "{want:#?}"),
        }
        let s = sync_table(d.as_ref(), &mut b, "clientes", &want).await;
        eprintln!("-- {step} (A → B)\n{}", s.join("\nGO\n"));
        assert!(!s.is_empty());
    }
    // And from B to A: B gets comments of its own, A takes them, then B drops one.
    run(&mut b, &format!("{}\n{}", add("", "Desde B"), add(&col("email"), "Correo de B"))).await.unwrap();
    let want = tables_of(&mut b).await.remove("clientes").unwrap();
    sync_table(d.as_ref(), &mut a, "clientes", &want).await;
    run(&mut b, &format!("{}\n{}", upd("", "Otra vez B"), del(&col("email")))).await.unwrap();
    let want = tables_of(&mut b).await.remove("clientes").unwrap();
    sync_table(d.as_ref(), &mut a, "clientes", &want).await;
    assert_eq!(tables_of(&mut a).await["clientes"].comment.as_deref(), Some("Otra vez B"));

    drop(a);
    drop(b);
    for db in [a_db, b_db] {
        admin.drop_database(db).await.expect("drop_database");
    }
}

/// The owner's case: on the target the keys of two tables are clustered
/// (the default) and another table's foreign keys reference them; on the
/// source the keys are NONCLUSTERED and a clustered columnstore index takes
/// the clustered place. The sync, planned as the compare tab plans it
/// (`database_schema` on both sides, an Alter per table that differs,
/// `sync_script`), runs on the target statement by statement on one
/// session; the tables then read the same as the source's, with the
/// foreign keys (and the rows) still there. Then the other way back.
///
/// ```sh
/// DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
///   cargo test -p dbine-driver-sqlserver --test compare clustered -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn clustered_columnstore_takes_the_keys_place_and_back() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    let mut admin = d.connect(&cfg, Some("master")).await.expect("connect");
    let (src_db, dst_db) = ("dbine_cci_src", "dbine_cci_dst");
    for db in [src_db, dst_db] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create_database");
    }
    let mut src = d.connect(&cfg, Some(src_db)).await.unwrap();
    let mut dst = d.connect(&cfg, Some(dst_db)).await.unwrap();
    let ddl = |pk: &str, cci: bool| {
        format!(
            "CREATE SCHEMA Alerts;
GO
CREATE TABLE Alerts.RuleConfigurationAudit (
    Id bigint NOT NULL CONSTRAINT PK_RuleConfigurationAudit PRIMARY KEY {pk},
    RuleId int NOT NULL, Changed datetime2 NULL);
CREATE INDEX IX_RuleConfigurationAudit_RuleId ON Alerts.RuleConfigurationAudit (RuleId);
CREATE TABLE Alerts.RuleScenario (Id bigint NOT NULL CONSTRAINT PK_RuleScenario PRIMARY KEY {pk}, Name nvarchar(50) NULL);
{}
CREATE TABLE Alerts.AuditNote (
    Id int NOT NULL CONSTRAINT PK_AuditNote PRIMARY KEY,
    AuditId bigint NOT NULL, ScenarioId bigint NULL,
    CONSTRAINT FK_AuditNote_Audit FOREIGN KEY (AuditId) REFERENCES Alerts.RuleConfigurationAudit (Id) ON DELETE CASCADE);
ALTER TABLE Alerts.AuditNote WITH NOCHECK ADD CONSTRAINT FK_AuditNote_Scenario FOREIGN KEY (ScenarioId) REFERENCES Alerts.RuleScenario (Id);
INSERT INTO Alerts.RuleConfigurationAudit VALUES (1, 10, NULL), (2, 20, SYSDATETIME());
INSERT INTO Alerts.RuleScenario VALUES (1, N'uno'), (2, N'dos');
INSERT INTO Alerts.AuditNote VALUES (1, 1, 1), (2, 2, NULL);",
            if cci {
                "CREATE CLUSTERED COLUMNSTORE INDEX CCI_RuleConfigurationAudit ON Alerts.RuleConfigurationAudit;
CREATE CLUSTERED COLUMNSTORE INDEX CCI_RuleScenario ON Alerts.RuleScenario;"
            } else {
                ""
            }
        )
    };
    for (s, sql) in [(&mut src, ddl("NONCLUSTERED", true)), (&mut dst, ddl("", false))] {
        for batch in sql.split("\nGO\n") {
            run(s, batch).await.unwrap_or_else(|e| panic!("{e}\n{batch}"));
        }
    }

    // What the compare tab sends: an Alter for each table that differs.
    async fn sync(d: &dyn Driver, target: &mut Box<dyn Session>, want: &BTreeMap<String, TableSchema>) -> Vec<String> {
        let have = tables_of(target).await;
        let changes: Vec<TableChange> =
            want.iter().filter(|(n, w)| have[*n] != **w).map(|(n, w)| TableChange::Alter { old: have[n].clone(), new: w.clone() }).collect();
        let script = d.sync_script(&changes).expect("sync_script");
        for w in &script.warnings {
            eprintln!("aviso: {w}");
        }
        for (i, s) in script.statements.iter().enumerate() {
            if let Err(e) = run(target, s).await {
                panic!("statement {i} failed: {e}\n---\n{s}\n---\nwhole script:\n{}", script.statements.join("\nGO\n"));
            }
        }
        let after = tables_of(target).await;
        for (n, w) in want {
            assert_eq!(&after[n], w, "{n} after the sync:\n{}", script.statements.join("\nGO\n"));
        }
        script.statements
    }
    async fn fks(s: &mut Box<dyn Session>) -> Vec<String> {
        let mut out = QueryOutcome::default();
        s.execute(
            "SELECT fk.name + ':' + OBJECT_NAME(fk.referenced_object_id) + ':' + CAST(fk.is_not_trusted AS varchar(1)) + ':' + fk.delete_referential_action_desc COLLATE DATABASE_DEFAULT
               FROM sys.foreign_keys fk ORDER BY fk.name",
            100,
            &mut out,
        )
        .await
        .unwrap();
        out.results[0].rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
    }
    async fn count(s: &mut Box<dyn Session>, sql: &str) -> i64 {
        let mut out = QueryOutcome::default();
        s.execute(sql, 100, &mut out).await.unwrap();
        out.results[0].rows[0][0].as_i64().unwrap()
    }
    let fks_before = fks(&mut dst).await;
    assert_eq!(
        fks_before,
        ["FK_AuditNote_Audit:RuleConfigurationAudit:0:CASCADE", "FK_AuditNote_Scenario:RuleScenario:1:NO_ACTION"],
        "the target's foreign keys"
    );

    let source = tables_of(&mut src).await;
    let target = tables_of(&mut dst).await;
    let rca = &source["RuleConfigurationAudit"];
    assert_eq!(rca.options.get("primary_key").map(String::as_str), Some("NONCLUSTERED"), "{rca:#?}");
    assert!(target["RuleConfigurationAudit"].options.is_empty(), "{:#?}", target["RuleConfigurationAudit"]);
    assert_eq!(source["AuditNote"], target["AuditNote"]);

    // Source → target: the keys give way to the columnstore indexes.
    let s = sync(d.as_ref(), &mut dst, &source).await;
    eprintln!("-- clustered key → columnstore\n{}", s.join("\nGO\n"));
    assert_eq!(fks(&mut dst).await, fks_before, "foreign keys after the sync");
    assert_eq!(count(&mut dst, "SELECT CAST(COUNT(*) AS bigint) FROM Alerts.AuditNote n JOIN Alerts.RuleConfigurationAudit a ON a.Id = n.AuditId").await, 2);
    assert_eq!(
        count(&mut dst, "SELECT CAST(COUNT(*) AS bigint) FROM sys.indexes WHERE type = 5 AND object_id IN (OBJECT_ID('Alerts.RuleConfigurationAudit'), OBJECT_ID('Alerts.RuleScenario'))").await,
        2
    );
    // Nothing left to sync.
    let again = d.sync_script(&[TableChange::Alter { old: tables_of(&mut dst).await["RuleScenario"].clone(), new: source["RuleScenario"].clone() }]).unwrap();
    assert!(again.statements.is_empty(), "{again:#?}");

    // And back: the clustered keys take the place again.
    let s = sync(d.as_ref(), &mut dst, &target).await;
    eprintln!("-- columnstore → clustered key\n{}", s.join("\nGO\n"));
    assert_eq!(fks(&mut dst).await, fks_before, "foreign keys after syncing back");
    assert_eq!(count(&mut dst, "SELECT CAST(COUNT(*) AS bigint) FROM Alerts.AuditNote").await, 2);

    drop(src);
    drop(dst);
    for db in [src_db, dst_db] {
        admin.drop_database(db).await.expect("drop_database");
    }
}

/// What both sides start with for the "Eliminar" cases: indexes, a foreign
/// key, CHECKs, a table two others (and itself) reference, and one of each
/// code object the compare drops.
const DROPS: &str = "
CREATE TYPE dbo.Codigo FROM varchar(10) NULL;
CREATE SEQUENCE dbo.seq_pedidos AS int START WITH 1;
GO
CREATE TABLE dbo.clientes (
    id int NOT NULL CONSTRAINT PK_clientes PRIMARY KEY,
    nombre nvarchar(50) NOT NULL,
    email nvarchar(100) NULL,
    telefono varchar(20) NULL,
    edad int NULL,
    saldo decimal(10,2) NULL CONSTRAINT DF_clientes_saldo DEFAULT (0),
    notas nvarchar(200) NULL,
    CONSTRAINT CK_clientes_edad CHECK (edad >= 0),
    CONSTRAINT CK_clientes_saldo CHECK (saldo >= 0)
);
CREATE INDEX IX_clientes_email ON dbo.clientes (email);
CREATE INDEX IX_clientes_nombre ON dbo.clientes (nombre) INCLUDE (email);
CREATE INDEX IX_clientes_telefono ON dbo.clientes (telefono);
CREATE INDEX IX_clientes_edad ON dbo.clientes (edad) INCLUDE (notas) WHERE notas IS NOT NULL;
CREATE TABLE dbo.pedidos (
    id int NOT NULL CONSTRAINT PK_pedidos PRIMARY KEY,
    cliente_id int NOT NULL,
    total decimal(10,2) NULL,
    CONSTRAINT FK_pedidos_clientes FOREIGN KEY (cliente_id) REFERENCES dbo.clientes (id)
);
CREATE TABLE dbo.categorias (
    id int NOT NULL CONSTRAINT PK_categorias PRIMARY KEY,
    codigo dbo.Codigo,
    padre_id int NULL CONSTRAINT FK_categorias_padre REFERENCES dbo.categorias (id)
);
CREATE TABLE dbo.productos (
    id int NOT NULL CONSTRAINT PK_productos PRIMARY KEY,
    categoria_id int NULL CONSTRAINT FK_productos_categorias REFERENCES dbo.categorias (id)
);
CREATE TABLE dbo.etiquetas (
    id int NOT NULL CONSTRAINT PK_etiquetas PRIMARY KEY,
    categoria_id int NULL,
    CONSTRAINT FK_etiquetas_categorias FOREIGN KEY (categoria_id) REFERENCES dbo.categorias (id) ON DELETE CASCADE
);
INSERT INTO dbo.categorias VALUES (1, 'a', NULL), (2, 'b', 1);
INSERT INTO dbo.productos VALUES (1, 2);
INSERT INTO dbo.etiquetas VALUES (1, 1);
GO
CREATE VIEW dbo.v_clientes AS SELECT id, nombre FROM dbo.clientes;
GO
CREATE VIEW dbo.v_clientes_top AS SELECT TOP 10 id FROM dbo.v_clientes ORDER BY id;
GO
CREATE VIEW dbo.v_categorias AS SELECT id, codigo FROM dbo.categorias;
GO
CREATE FUNCTION dbo.fn_total(@id int) RETURNS decimal(10,2) AS BEGIN RETURN (SELECT SUM(total) FROM dbo.pedidos WHERE cliente_id = @id); END;
GO
CREATE PROCEDURE dbo.sp_limpiar AS DELETE FROM dbo.pedidos WHERE total IS NULL;
GO
CREATE TRIGGER dbo.tg_pedidos ON dbo.pedidos AFTER INSERT AS BEGIN SET NOCOUNT ON; END;
GO
CREATE SYNONYM dbo.syn_clientes FOR dbo.clientes;
";

/// One side as the compare tab holds it.
#[derive(Clone)]
struct Model {
    tables: BTreeMap<String, TableSchema>,
    objects: Objects,
}

type ObjectOps = Vec<(Op, String, Option<String>, String, String)>;

impl Model {
    async fn read(s: &mut Box<dyn Session>) -> Self {
        let (tables, objects) = read(s).await;
        Model { tables, objects }
    }

    /// `changesOf` in CompareView.vue: what differs from the side as it was
    /// loaded (`orig`) becomes a drop, an alter or a create.
    fn changes_from(&self, orig: &Model) -> (Vec<TableChange>, ObjectOps) {
        let mut tables = Vec::new();
        for (name, o) in &orig.tables {
            match self.tables.get(name) {
                None => tables.push(TableChange::Drop { table: o.clone() }),
                Some(w) if w != o => tables.push(TableChange::Alter { old: o.clone(), new: w.clone() }),
                Some(_) => {}
            }
        }
        for (name, w) in &self.tables {
            if !orig.tables.contains_key(name) {
                tables.push(TableChange::Create { table: w.clone() });
            }
        }
        let mut objects = Vec::new();
        for ((k, s, n), def) in &orig.objects {
            match self.objects.get(&(k.clone(), s.clone(), n.clone())) {
                None => objects.push((Op::Drop, k.clone(), s.clone(), n.clone(), def.clone())),
                Some(w) if w != def => objects.push((Op::Replace, k.clone(), s.clone(), n.clone(), w.clone())),
                Some(_) => {}
            }
        }
        for ((k, s, n), def) in &self.objects {
            if !orig.objects.contains_key(&(k.clone(), s.clone(), n.clone())) {
                objects.push((Op::Create, k.clone(), s.clone(), n.clone(), def.clone()));
            }
        }
        (tables, objects)
    }

    /// "Eliminar" on an item: the table without it (`removeItem`).
    fn drop_item(&mut self, table: &str, section: &str, name: &str) {
        let t = self.tables.get_mut(table).unwrap_or_else(|| panic!("table {table}"));
        let before = (t.columns.len(), t.indexes.len(), t.foreign_keys.len(), t.checks.len());
        match section {
            "columns" => t.columns.retain(|c| c.name != name),
            "indexes" => t.indexes.retain(|i| i.name != name),
            "foreign_keys" => t.foreign_keys.retain(|f| f.name.as_deref() != Some(name)),
            "checks" => t.checks.retain(|c| c.name.as_deref() != Some(name)),
            "primary_key" => {
                assert_eq!(t.primary_key.as_ref().and_then(|k| k.name.as_deref()), Some(name));
                t.primary_key = None;
                return;
            }
            _ => unreachable!(),
        }
        assert_ne!(before, (t.columns.len(), t.indexes.len(), t.foreign_keys.len(), t.checks.len()), "{section} {name} in {table}");
    }

    /// "Eliminar" on a table: gone, and the foreign keys of other tables
    /// that reference it are stripped (`applyDrop`'s cascade).
    fn drop_table(&mut self, table: &str) {
        let gone = self.tables.remove(table).unwrap_or_else(|| panic!("table {table}"));
        for t in self.tables.values_mut() {
            t.foreign_keys.retain(|f| !(f.ref_table.eq_ignore_ascii_case(&gone.name) && f.ref_schema.as_ref().or(t.schema.as_ref()).map(|s| s.to_lowercase()) == gone.schema.as_ref().map(|s| s.to_lowercase())));
        }
    }

    /// The work copy as the server will have it: a dropped column takes the
    /// indexes, CHECKs and foreign keys that use it along (the script drops
    /// them first).
    fn settled(&self, orig: &Model) -> Model {
        let mut m = self.clone();
        for (n, t) in m.tables.iter_mut() {
            let Some(o) = orig.tables.get(n) else { continue };
            let dropped: Vec<&str> = o.columns.iter().filter(|c| !t.columns.iter().any(|x| x.name == c.name)).map(|c| c.name.as_str()).collect();
            let uses = |text: &str| dropped.iter().any(|d| mentions(text, d));
            t.indexes.retain(|i| !(i.columns.iter().chain(&i.include).any(|c| uses(c)) || i.filter.as_deref().is_some_and(uses)));
            t.checks.retain(|c| !uses(&c.expression));
            t.foreign_keys.retain(|f| !f.columns.iter().any(|c| uses(c)));
        }
        m
    }

    fn drop_object(&mut self, kind: &str, name: &str) {
        let key = (kind.to_string(), Some("dbo".to_string()), name.to_string());
        assert!(self.objects.remove(&key).is_some(), "{kind} {name}");
    }
}

/// `dependent_views` in src-tauri's compare: views over a dropped table go,
/// views over a table that loses a column (or changes its type) are made
/// again; the ones already being changed stay as they are.
fn dependent_views(tables: &[TableChange], objects: &ObjectOps, orig: &Model) -> ObjectOps {
    let touched: Vec<(&str, bool)> = tables
        .iter()
        .filter_map(|c| match c {
            TableChange::Drop { table } => Some((table.name.as_str(), true)),
            TableChange::Alter { old, new } => {
                let norm = |t: &str| t.to_lowercase().split_whitespace().collect::<String>();
                let reshaped = old.columns.iter().any(|o| new.columns.iter().find(|n| n.name.eq_ignore_ascii_case(&o.name)).is_none_or(|n| norm(&n.data_type) != norm(&o.data_type)));
                reshaped.then_some((new.name.as_str(), false))
            }
            TableChange::Create { .. } => None,
        })
        .collect();
    orig.objects
        .iter()
        .filter(|((k, s, n), _)| k == "view" && !objects.iter().any(|(_, ok, os, on, _)| ok == k && os == s && on.eq_ignore_ascii_case(n)))
        .filter_map(|((k, s, n), def)| {
            let hit: Vec<bool> = touched.iter().filter(|(t, _)| mentions(def, t)).map(|(_, dropped)| *dropped).collect();
            let op = if hit.is_empty() {
                return None;
            } else if hit.iter().any(|d| *d) {
                Op::Drop
            } else {
                Op::Replace
            };
            Some((op, k.clone(), s.clone(), n.clone(), def.clone()))
        })
        .collect()
}

/// What "Sincronizar" does on one side: the script for its pending changes
/// (`schema_sync_script`), run statement by statement; then the side is
/// read again and must be just like the work copy.
async fn sync_side(d: &dyn Driver, s: &mut Box<dyn Session>, orig: &Model, work: &Model) -> Vec<String> {
    let (tables, mut objects) = work.changes_from(orig);
    let extra = dependent_views(&tables, &objects, orig);
    objects.extend(extra);
    let statements = plan(d, &tables, &objects);
    for (i, sql) in statements.iter().enumerate() {
        if let Err(e) = run(s, sql).await {
            panic!("statement {i} failed: {e}\n---\n{sql}\n---\nwhole script:\n{}", statements.join("\nGO\n"));
        }
    }
    let now = Model::read(s).await;
    let work = &work.settled(orig);
    assert_eq!(now.tables.keys().collect::<Vec<_>>(), work.tables.keys().collect::<Vec<_>>(), "tables after:\n{}", statements.join("\nGO\n"));
    for (n, t) in &work.tables {
        assert_eq!(&now.tables[n], t, "{n} after:\n{}", statements.join("\nGO\n"));
    }
    let names = |m: &Model| m.objects.keys().cloned().collect::<Vec<_>>();
    assert_eq!(names(&now), names(work), "objects after:\n{}", statements.join("\nGO\n"));
    statements
}

/// "Eliminar" in the compare tab: what it sends for each kind of element
/// (an index on one side and on both, a column, a foreign key, a CHECK, a
/// primary key, a table other tables reference, views, a function, a
/// procedure, a trigger, a sequence, a synonym and a type), run on the
/// server; the sides read again lose just that, and in the end compare
/// equal.
///
/// ```sh
/// DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
///   cargo test -p dbine-driver-sqlserver --test compare drops -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn drops_like_the_compare_tab() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    let mut admin = d.connect(&cfg, Some("master")).await.expect("connect");
    let (a_db, b_db) = ("dbine_drop_a", "dbine_drop_b");
    for db in [a_db, b_db] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create_database");
    }
    let mut a = d.connect(&cfg, Some(a_db)).await.unwrap();
    let mut b = d.connect(&cfg, Some(b_db)).await.unwrap();
    for s in [&mut a, &mut b] {
        for batch in DROPS.split("\nGO\n") {
            run(s, batch).await.unwrap_or_else(|e| panic!("{e}\n{batch}"));
        }
    }
    let start = Model::read(&mut a).await;
    assert!(start.tables == Model::read(&mut b).await.tables, "both sides start equal");
    let mut done = Vec::new();
    let mut step = |name: &str, script: Vec<String>| {
        eprintln!("-- {name}\n{}\n", script.join("\nGO\n"));
        done.push(name.to_string());
    };

    // An index on one side: only that side loses it.
    let orig_b = Model::read(&mut b).await;
    let mut work = orig_b.clone();
    work.drop_item("clientes", "indexes", "IX_clientes_email");
    step("index, right side", sync_side(d.as_ref(), &mut b, &orig_b, &work).await);
    let (ta, tb) = (Model::read(&mut a).await.tables, Model::read(&mut b).await.tables);
    assert!(ta["clientes"].indexes.iter().any(|i| i.name == "IX_clientes_email"));
    assert_ne!(ta["clientes"], tb["clientes"]);
    assert!(ta.iter().all(|(n, t)| n == "clientes" || tb[n] == *t));

    // The same index on both sides: one script per side, then they're equal.
    for (s, other) in [(&mut a, "IX_clientes_email"), (&mut b, "IX_clientes_nombre")] {
        let orig = Model::read(s).await;
        let mut work = orig.clone();
        work.drop_item("clientes", "indexes", "IX_clientes_nombre");
        if other == "IX_clientes_email" {
            work.drop_item("clientes", "indexes", other);
        }
        step("index, both sides", sync_side(d.as_ref(), s, &orig, &work).await);
    }
    assert_eq!(Model::read(&mut a).await.tables, Model::read(&mut b).await.tables, "equal after dropping on both sides");

    // The rest one at a time on the right side.
    type Edit = Box<dyn Fn(&mut Model)>;
    let cases: Vec<(&str, Edit)> = vec![
        ("column", Box::new(|m| m.drop_item("clientes", "columns", "email"))),
        ("foreign key", Box::new(|m| m.drop_item("pedidos", "foreign_keys", "FK_pedidos_clientes"))),
        ("check", Box::new(|m| m.drop_item("clientes", "checks", "CK_clientes_edad"))),
        ("primary key", Box::new(|m| m.drop_item("pedidos", "primary_key", "PK_pedidos"))),
        ("column with an index", Box::new(|m| m.drop_item("clientes", "columns", "telefono"))),
        ("column with a check and a default", Box::new(|m| m.drop_item("clientes", "columns", "saldo"))),
        ("column in an index's INCLUDE and filter", Box::new(|m| m.drop_item("clientes", "columns", "notas"))),
        (
            "referenced table, its view and its type",
            Box::new(|m| {
                m.drop_table("categorias");
                m.drop_object("type", "Codigo");
            }),
        ),
        (
            "views, function, procedure, trigger, sequence, synonym",
            Box::new(|m| {
                for (k, n) in [("view", "v_clientes"), ("view", "v_clientes_top"), ("function", "fn_total"), ("procedure", "sp_limpiar"), ("trigger", "tg_pedidos"), ("sequence", "seq_pedidos"), ("synonym", "syn_clientes")] {
                    m.drop_object(k, n);
                }
            }),
        ),
    ];
    for (name, f) in &cases {
        let orig = Model::read(&mut b).await;
        let mut work = orig.clone();
        f(&mut work);
        // The view over a dropped table goes with it (`dependent_views`).
        if name.starts_with("referenced") {
            work.drop_object("view", "v_categorias");
        }
        step(name, sync_side(d.as_ref(), &mut b, &orig, &work).await);
    }
    let tb = Model::read(&mut b).await;
    assert_eq!(tb.tables.keys().collect::<Vec<_>>(), ["clientes", "pedidos", "productos", "etiquetas"].iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>());
    assert!(tb.tables["productos"].foreign_keys.is_empty() && tb.tables["etiquetas"].foreign_keys.is_empty());
    assert!(tb.objects.is_empty(), "{:?}", tb.objects.keys());

    // All of it at once on the left side: the sides compare equal.
    let orig_a = Model::read(&mut a).await;
    let mut work = orig_a.clone();
    for (_, f) in &cases {
        f(&mut work);
    }
    work.drop_object("view", "v_categorias");
    step("everything at once, left side", sync_side(d.as_ref(), &mut a, &orig_a, &work).await);
    let ta = Model::read(&mut a).await;
    assert_eq!(ta.tables, tb.tables, "equal in the end");
    assert_eq!(ta.objects, tb.objects);
    eprintln!("{} drops run: {done:?}", done.len());

    drop(a);
    drop(b);
    for db in [a_db, b_db] {
        admin.drop_database(db).await.expect("drop_database");
    }
}

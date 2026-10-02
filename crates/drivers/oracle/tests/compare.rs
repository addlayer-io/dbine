//! "Comparar esquemas" against a real server: two schemas that differ in
//! every kind of thing the compare reads (CHECK constraints, function-based,
//! bitmap, reverse, compressed, invisible and partitioned indexes,
//! sequences, synonyms, object / collection types). The sync script is run
//! on one side and both must then read the same.
//!
//! Oracle Text / Spatial domain indexes need those options installed (the
//! `-slim` image has neither): their DDL is covered by unit tests.
//!
//! Needs a user that can create schemas (SYSTEM):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;

fn config_from(url: &str) -> ConnectionConfig {
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
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
CREATE TYPE t_punto AS OBJECT (x NUMBER, y NUMBER, MEMBER FUNCTION norma RETURN NUMBER);
/
CREATE TYPE BODY t_punto AS
  MEMBER FUNCTION norma RETURN NUMBER IS
  BEGIN
    RETURN SQRT(x * x + y * y);
  END;
END;
/
CREATE TYPE t_puntos AS TABLE OF t_punto;
/
CREATE TYPE t_tags AS VARRAY(10) OF VARCHAR2(30);
/
CREATE SEQUENCE seq_folio START WITH 100 INCREMENT BY 5 CACHE 10;
CREATE TABLE docs (
    id NUMBER NOT NULL CONSTRAINT pk_docs PRIMARY KEY,
    codigo VARCHAR2(20) NOT NULL,
    titulo VARCHAR2(200) NOT NULL,
    fecha DATE NOT NULL,
    estado VARCHAR2(10),
    CONSTRAINT ck_docs_fecha CHECK (fecha >= DATE '2000-01-01'),
    CHECK (estado IN ('A', 'B'))
);
CREATE INDEX ix_docs_titulo ON docs (UPPER(titulo));
CREATE BITMAP INDEX ix_docs_estado ON docs (estado);
CREATE INDEX ix_docs_codigo ON docs (codigo, fecha) REVERSE COMPRESS 1 INVISIBLE;
CREATE SYNONYM syn_docs FOR docs;
CREATE TABLE ventas (id NUMBER NOT NULL, fecha DATE NOT NULL, monto NUMBER)
  PARTITION BY RANGE (fecha) (PARTITION p2024 VALUES LESS THAN (DATE '2025-01-01'), PARTITION pmax VALUES LESS THAN (MAXVALUE));
CREATE INDEX ix_ventas_fecha ON ventas (fecha) LOCAL;
CREATE INDEX ix_ventas_id ON ventas (id) GLOBAL PARTITION BY HASH (id) PARTITIONS 4;
CREATE INDEX ix_ventas_monto ON ventas (monto)
  GLOBAL PARTITION BY RANGE (monto) (PARTITION pm1 VALUES LESS THAN (1000), PARTITION pm2 VALUES LESS THAN (MAXVALUE));
";

const TARGET: &str = "
CREATE TYPE t_punto AS OBJECT (x NUMBER, y NUMBER);
/
CREATE SEQUENCE seq_folio START WITH 1;
CREATE SEQUENCE seq_extra;
CREATE TABLE docs (
    id NUMBER NOT NULL CONSTRAINT pk_docs PRIMARY KEY,
    codigo VARCHAR2(20) NOT NULL,
    titulo VARCHAR2(200) NOT NULL,
    fecha DATE NOT NULL,
    estado VARCHAR2(10),
    CONSTRAINT ck_docs_fecha CHECK (fecha >= DATE '1990-01-01')
);
CREATE INDEX ix_docs_titulo ON docs (titulo);
CREATE INDEX ix_docs_estado ON docs (estado);
CREATE INDEX ix_docs_codigo ON docs (codigo, fecha);
CREATE TABLE ventas (id NUMBER NOT NULL, fecha DATE NOT NULL, monto NUMBER)
  PARTITION BY RANGE (fecha) (PARTITION p2024 VALUES LESS THAN (DATE '2025-01-01'), PARTITION pmax VALUES LESS THAN (MAXVALUE));
CREATE INDEX ix_ventas_fecha ON ventas (fecha);
CREATE INDEX ix_ventas_id ON ventas (id);
";

/// The object kinds the compare reads by their source.
const CODE_KINDS: &[&str] = &["view", "materialized_view", "procedure", "function", "trigger", "sequence", "synonym", "type"];
/// Kinds tables depend on: made before the tables, dropped after them.
const PREREQS: &[&str] = &["type", "sequence"];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Create,
    Drop,
    Replace,
}

type Objects = BTreeMap<(String, Option<String>, String), String>;

fn normalized(mut t: TableSchema) -> TableSchema {
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t.checks.sort_by(|a, b| (&a.name, &a.expression).cmp(&(&b.name, &b.expression)));
    t
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(['/', ';', ' ']).to_string()
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

/// What `drop_other` (src-tauri) writes for Oracle: no `IF EXISTS`, and a
/// type others use only goes with FORCE.
fn drop_other(kind: &str, name: &str) -> String {
    let keyword = if kind == "materialized_view" { "MATERIALIZED VIEW".to_string() } else { kind.to_uppercase() };
    let force = if kind == "type" { " FORCE" } else { "" };
    format!("DROP {keyword} \"{name}\"{force};")
}

/// Whether `text` names `name` as a whole word (any case).
fn mentions(text: &str, name: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.to_lowercase().split(|c: char| !word(c)).any(|w| w == name.to_lowercase())
}

/// `dependency_order` in src-tauri's compare, for drops: one whose
/// definition names another goes first (a trigger before its function, a
/// view before the view it reads); a cycle is broken at its first item.
fn dependents_first(mut items: Vec<(String, String, String)>) -> Vec<String> {
    let mut out = Vec::new();
    while !items.is_empty() {
        let free = (0..items.len()).find(|&j| !items.iter().enumerate().any(|(i, (_, def, _))| i != j && items[i].0 != items[j].0 && mentions(def, &items[j].0)));
        out.push(items.remove(free.unwrap_or(0)).2);
    }
    out
}

/// `plan` in src-tauri's compare: object drops (dependents first),
/// prerequisites, tables, other objects, then dropped prerequisites.
fn plan(d: &dyn Driver, tables: &[TableChange], objects: &[(Op, String, Option<String>, String, String)]) -> (Vec<String>, Vec<String>) {
    let (mut before, mut early, mut after, mut late) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (op, kind, _schema, name, def) in objects {
        let prereq = PREREQS.contains(&kind.as_str());
        let drop = (name.clone(), def.clone(), drop_other(kind, name));
        match op {
            Op::Drop if prereq => late.push(drop),
            Op::Drop | Op::Replace => before.push(drop),
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
    let statements = dependents_first(before).into_iter().chain(early).chain(script.statements).chain(after).chain(dependents_first(late)).collect();
    (statements, script.warnings)
}

#[tokio::test]
#[ignore]
async fn compare_and_sync_everything() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let cfg = config_from(&url);
    let d = dbine_driver_oracle::drivers().remove(0);
    let kinds: Vec<&str> = d.info().object_kinds.iter().map(|k| k.id).collect();
    for k in ["sequence", "synonym", "type"] {
        assert!(kinds.contains(&k), "{k} in {kinds:?}");
    }
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let (a_db, b_db) = ("DBINE_CMP_A", "DBINE_CMP_B");
    for db in [a_db, b_db] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create_database");
    }
    let mut a = d.connect(&cfg, Some(a_db)).await.unwrap();
    let mut b = d.connect(&cfg, Some(b_db)).await.unwrap();
    run(&mut a, SOURCE).await.unwrap();
    run(&mut b, TARGET).await.unwrap();

    let (ta, oa) = read(&mut a).await;
    let (tb, ob) = read(&mut b).await;
    for ((k, s, n), def) in &oa {
        eprintln!("-- {k} {s:?}.{n}\n{def}");
    }

    // What the catalog says.
    let docs = &ta["DOCS"];
    let ix = |t: &TableSchema, n: &str| t.indexes.iter().find(|i| i.name == n).cloned().unwrap_or_else(|| panic!("{n} in {:#?}", t.indexes));
    assert_eq!(ix(docs, "IX_DOCS_TITULO").columns, ["UPPER(\"TITULO\")"]);
    assert_eq!(ix(docs, "IX_DOCS_ESTADO").kind.as_deref(), Some("BITMAP"));
    let o = ix(docs, "IX_DOCS_CODIGO").options;
    assert_eq!(
        (o.get("REVERSE").map(String::as_str), o.get("COMPRESS").map(String::as_str), o.get("VISIBILITY").map(String::as_str)),
        (Some("YES"), Some("1"), Some("INVISIBLE"))
    );
    assert!(ix(&tb["DOCS"], "IX_DOCS_CODIGO").options.is_empty());
    assert_eq!(docs.checks.len(), 2, "{:#?}", docs.checks);
    assert_eq!(docs.checks[0].name, None);
    assert_eq!(docs.checks[1].name.as_deref(), Some("CK_DOCS_FECHA"));
    assert_eq!(tb["DOCS"].checks.len(), 1, "NOT NULL constraints aren't CHECKs: {:#?}", tb["DOCS"].checks);
    let ventas = &ta["VENTAS"];
    assert_eq!(ix(ventas, "IX_VENTAS_FECHA").options.get("LOCALITY").map(String::as_str), Some("LOCAL"));
    assert_eq!(ix(ventas, "IX_VENTAS_ID").options.get("LOCALITY").map(String::as_str), Some("GLOBAL PARTITION BY HASH (\"ID\") PARTITIONS 4"));
    assert_eq!(
        ix(ventas, "IX_VENTAS_MONTO").options.get("LOCALITY").map(String::as_str),
        Some("GLOBAL PARTITION BY RANGE (\"MONTO\") (PARTITION \"PM1\" VALUES LESS THAN (1000), PARTITION \"PM2\" VALUES LESS THAN (MAXVALUE))")
    );

    let obj = |o: &Objects, k: &str, n: &str| o.get(&(k.to_string(), None, n.to_string())).cloned();
    let seq = obj(&oa, "sequence", "SEQ_FOLIO").unwrap();
    assert!(seq.contains("INCREMENT BY 5") && seq.contains("START WITH 100") && seq.contains("CACHE 10"), "{seq}");
    assert_eq!(obj(&oa, "synonym", "SYN_DOCS").as_deref(), Some("CREATE OR REPLACE SYNONYM \"SYN_DOCS\" FOR \"DOCS\";"));
    let punto = obj(&oa, "type", "T_PUNTO").unwrap();
    assert!(punto.starts_with("CREATE OR REPLACE TYPE t_punto AS OBJECT") && punto.contains("CREATE OR REPLACE TYPE BODY t_punto"), "{punto}");
    assert!(obj(&oa, "type", "T_PUNTOS").unwrap().contains("AS TABLE OF t_punto"));
    assert!(obj(&oa, "type", "T_TAGS").unwrap().contains("VARRAY(10) OF VARCHAR2(30)"));

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
    let changed: Vec<&str> = tables
        .iter()
        .map(|c| match c {
            TableChange::Alter { new, .. } | TableChange::Create { table: new } | TableChange::Drop { table: new } => new.name.as_str(),
        })
        .collect();
    assert_eq!(changed, ["DOCS", "VENTAS"]);
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
    assert_eq!(
        ops,
        [(Op::Replace, "SEQ_FOLIO"), (Op::Create, "SYN_DOCS"), (Op::Replace, "T_PUNTO"), (Op::Create, "T_PUNTOS"), (Op::Create, "T_TAGS"), (Op::Drop, "SEQ_EXTRA")]
    );

    // Sync B to A, then both read the same.
    let (statements, _) = plan(d.as_ref(), &tables, &objects);
    for (i, s) in statements.iter().enumerate() {
        if let Err(e) = run(&mut b, s).await {
            panic!("statement {i} failed: {e}\n---\n{s}\n---\nwhole script:\n{}", statements.join("\n"));
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

/// Both sides of the "Eliminar" test start like this.
const DROPS: &str = "
CREATE TABLE padres (id NUMBER CONSTRAINT pk_padres PRIMARY KEY, nombre VARCHAR2(50));
CREATE TABLE cat (id NUMBER CONSTRAINT pk_cat PRIMARY KEY);
CREATE TABLE hijos (
    id NUMBER CONSTRAINT pk_hijos PRIMARY KEY,
    padre_id NUMBER CONSTRAINT fk_hijos_padre REFERENCES padres (id),
    nota VARCHAR2(50),
    obs VARCHAR2(50),
    monto NUMBER,
    CONSTRAINT ck_hijos_monto CHECK (monto >= 0),
    CHECK (nota <> 'x')
);
CREATE INDEX ix_hijos_nota ON hijos (nota);
CREATE INDEX ix_hijos_monto ON hijos (monto);
CREATE TABLE refs (
    id NUMBER CONSTRAINT pk_refs PRIMARY KEY,
    padre_id NUMBER CONSTRAINT fk_refs_padre REFERENCES padres (id),
    hijo_id NUMBER CONSTRAINT fk_refs_hijo REFERENCES hijos (id),
    cat_id NUMBER REFERENCES cat (id)
);
CREATE SEQUENCE seq_drop;
CREATE TYPE t_drop AS OBJECT (a NUMBER);
/
CREATE SYNONYM syn_hijos FOR hijos;
CREATE FUNCTION f_doble (p NUMBER) RETURN NUMBER IS BEGIN RETURN p * 2; END;
/
CREATE PROCEDURE p_hola IS BEGIN NULL; END;
/
CREATE TRIGGER tg_hijos BEFORE INSERT ON hijos FOR EACH ROW BEGIN :NEW.monto := f_doble(:NEW.monto); END;
/
CREATE VIEW v_hijos AS SELECT id, f_doble(monto) AS m FROM hijos;
CREATE VIEW v_hijos2 AS SELECT id FROM v_hijos;
CREATE MATERIALIZED VIEW mv_cat AS SELECT id FROM cat;
";

/// One side of the compare: what the server has (`orig`) and the copy the
/// UI edits (`work`).
struct Side {
    s: Box<dyn Session>,
    orig: (BTreeMap<String, TableSchema>, Objects),
    work: (BTreeMap<String, TableSchema>, Objects),
}

impl Side {
    async fn new(s: Box<dyn Session>) -> Self {
        let mut side = Side { s, orig: Default::default(), work: Default::default() };
        side.reload().await;
        side
    }

    async fn reload(&mut self) {
        self.orig = read(&mut self.s).await;
        self.work = self.orig.clone();
    }

    /// An item drop: the table without it, as `removeItem` leaves it.
    fn edit(&mut self, table: &str, f: impl FnOnce(&mut TableSchema)) {
        f(self.work.0.get_mut(table).unwrap_or_else(|| panic!("{table}")));
    }

    /// A table drop, as `applyDrop` does it: the foreign keys that
    /// reference it go too.
    fn drop_table(&mut self, table: &str) {
        self.work.0.remove(table);
        for t in self.work.0.values_mut() {
            t.foreign_keys.retain(|fk| fk.ref_table != table);
        }
    }

    fn drop_object(&mut self, kind: &str, name: &str) {
        let key = self.work.1.keys().find(|(k, _, n)| k == kind && n == name).cloned().unwrap_or_else(|| panic!("{kind} {name}"));
        self.work.1.remove(&key);
    }

    /// `changesOf` + sync: the script for `work`, run; then the server
    /// must read as `work`. Returns the script's warnings.
    async fn sync(&mut self, d: &dyn Driver) -> Vec<String> {
        let (ot, oo) = &self.orig;
        let (wt, wo) = &self.work;
        let mut tables = Vec::new();
        for (name, o) in ot {
            match wt.get(name) {
                None => tables.push(TableChange::Drop { table: o.clone() }),
                Some(w) if w != o => tables.push(TableChange::Alter { old: o.clone(), new: w.clone() }),
                Some(_) => {}
            }
        }
        for (name, w) in wt {
            if !ot.contains_key(name) {
                tables.push(TableChange::Create { table: w.clone() });
            }
        }
        let mut objects = Vec::new();
        for ((k, s, n), def) in oo {
            match wo.get(&(k.clone(), s.clone(), n.clone())) {
                None => objects.push((Op::Drop, k.clone(), s.clone(), n.clone(), def.clone())),
                Some(w) if w != def => objects.push((Op::Replace, k.clone(), s.clone(), n.clone(), w.clone())),
                Some(_) => {}
            }
        }
        for ((k, s, n), def) in wo {
            if !oo.contains_key(&(k.clone(), s.clone(), n.clone())) {
                objects.push((Op::Create, k.clone(), s.clone(), n.clone(), def.clone()));
            }
        }
        let (statements, warnings) = plan(d, &tables, &objects);
        for (i, st) in statements.iter().enumerate() {
            eprintln!("-- {i}\n{st}");
            if let Err(e) = run(&mut self.s, st).await {
                panic!("statement {i} failed: {e}\n---\n{st}\n---\nwhole script:\n{}", statements.join("\n"));
            }
        }
        let expected = self.work.clone();
        self.reload().await;
        assert_eq!(self.orig.0, expected.0, "tables after the sync");
        let squashed = |o: &Objects| o.iter().map(|(k, v)| (k.clone(), squash(v))).collect::<Vec<_>>();
        assert_eq!(squashed(&self.orig.1), squashed(&expected.1), "objects after the sync");
        warnings
    }
}

/// "Eliminar" in the compare: each kind of thing dropped on one side or on
/// both, the way CompareView sends it (a table without the item, a dropped
/// table, a dropped object), run, then read again.
#[tokio::test]
#[ignore]
async fn compare_drops() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let cfg = config_from(&url);
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let (a_db, b_db) = ("DBINE_DROP_A", "DBINE_DROP_B");
    for db in [a_db, b_db] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create_database");
        // A materialized view's owner needs these, whoever creates it.
        run(&mut admin, &format!("GRANT CREATE TABLE, CREATE MATERIALIZED VIEW TO \"{db}\"")).await.unwrap();
    }
    let mut a = d.connect(&cfg, Some(a_db)).await.unwrap();
    let mut b = d.connect(&cfg, Some(b_db)).await.unwrap();
    run(&mut a, DROPS).await.unwrap();
    run(&mut b, DROPS).await.unwrap();
    let mut a = Side::new(a).await;
    let mut b = Side::new(b).await;
    // Code read back names its owner: the same once that's left out.
    let same = |a: &Side, b: &Side, what: &str| {
        assert_eq!(a.orig.0, b.orig.0, "tables {what}");
        let objects = |s: &Side, owner: &str| s.orig.1.iter().map(|(k, v)| (k.clone(), squash(&v.replace(owner, "OWNER")))).collect::<Vec<_>>();
        assert_eq!(objects(a, a_db), objects(b, b_db), "objects {what}");
    };
    same(&a, &b, "at the start");
    let names = |side: &Side| side.orig.1.keys().map(|(k, _, n)| format!("{k}:{n}")).collect::<Vec<_>>();
    assert_eq!(
        names(&a),
        [
            "function:F_DOBLE",
            "materialized_view:MV_CAT",
            "procedure:P_HOLA",
            "sequence:SEQ_DROP",
            "synonym:SYN_HIJOS",
            "trigger:TG_HIJOS",
            "type:T_DROP",
            "view:V_HIJOS",
            "view:V_HIJOS2"
        ]
    );

    // The same unused index on both sides: gone from both, and they
    // compare equal again.
    for side in [&mut a, &mut b] {
        side.edit("HIJOS", |t| t.indexes.retain(|i| i.name != "IX_HIJOS_NOTA"));
        side.sync(d.as_ref()).await;
    }
    assert!(a.orig.0["HIJOS"].indexes.iter().all(|i| i.name != "IX_HIJOS_NOTA"));
    same(&a, &b, "after dropping the index on both sides");

    // One side only, each kind of item.
    b.edit("HIJOS", |t| t.indexes.retain(|i| i.name != "IX_HIJOS_MONTO"));
    b.sync(d.as_ref()).await;
    b.edit("HIJOS", |t| t.columns.retain(|c| c.name != "OBS"));
    b.sync(d.as_ref()).await;
    b.edit("REFS", |t| t.foreign_keys.retain(|fk| fk.name.as_deref() != Some("FK_REFS_HIJO")));
    b.sync(d.as_ref()).await;
    b.edit("HIJOS", |t| t.checks.retain(|c| c.name.as_deref() != Some("CK_HIJOS_MONTO")));
    b.sync(d.as_ref()).await;
    // System-named (SYS_C…) ones: read without a name, looked up to drop.
    b.edit("HIJOS", |t| t.checks.retain(|c| c.name.is_some()));
    b.sync(d.as_ref()).await;
    b.edit("REFS", |t| t.foreign_keys.retain(|fk| fk.name.is_some()));
    b.sync(d.as_ref()).await;
    assert!(b.orig.0["REFS"].foreign_keys.iter().all(|fk| fk.ref_table != "CAT"));
    b.edit("REFS", |t| t.primary_key = None);
    b.sync(d.as_ref()).await;

    // A table two others reference: their keys go first.
    b.drop_table("PADRES");
    b.sync(d.as_ref()).await;
    for t in ["HIJOS", "REFS"] {
        assert!(b.orig.0[t].foreign_keys.is_empty(), "{t}: {:?}", b.orig.0[t].foreign_keys);
    }

    // Every kind of code object at once: a trigger before its function, a
    // view before the view it reads; the sequence and the type after the
    // tables.
    for (kind, name) in [
        ("function", "F_DOBLE"),
        ("materialized_view", "MV_CAT"),
        ("procedure", "P_HOLA"),
        ("sequence", "SEQ_DROP"),
        ("synonym", "SYN_HIJOS"),
        ("trigger", "TG_HIJOS"),
        ("type", "T_DROP"),
        ("view", "V_HIJOS"),
        ("view", "V_HIJOS2"),
    ] {
        b.drop_object(kind, name);
    }
    b.sync(d.as_ref()).await;
    assert!(b.orig.1.is_empty(), "{:?}", b.orig.1.keys());

    // A kept everything but the index dropped on both sides.
    a.reload().await;
    assert_eq!(names(&a).len(), 9);
    assert_eq!(a.orig.0.len(), 4);
    assert_eq!(b.orig.0.keys().collect::<Vec<_>>(), ["CAT", "HIJOS", "REFS"]);

    drop(a);
    drop(b);
    for db in [a_db, b_db] {
        admin.drop_database(db).await.expect("drop_database");
    }
}

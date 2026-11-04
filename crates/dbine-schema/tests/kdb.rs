//! kdb+ (q) as a conversion source and target, and the driver's sync and
//! rename scripts. The ignored tests run against a real kdb+
//! (`dbine-test-kdb`, see `crates/drivers/kdb/tests/live.rs`):
//!
//! ```text
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//!   cargo test -p dbine-schema --test kdb -- --ignored --test-threads=1
//! ```
//!
//! They create tables named `dbine_sc_*` and delete them at the end.

use dbine_driver::{
    ColumnDef, ConnectionConfig, DdlParts, Driver, KeyDef, ObjectRef, QueryOutcome, RenameRequest,
    RenameTarget, Session, TableChange, TableSchema,
};
use dbine_schema::{convert, IssueCode, LogicalType as L, Options, Severity};
use std::sync::Arc;

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        ..Default::default()
    }
}

fn table(name: &str, cols: Vec<ColumnDef>, pk: &[&str]) -> TableSchema {
    TableSchema {
        kind: "table".into(),
        name: name.into(),
        columns: cols,
        primary_key: (!pk.is_empty()).then(|| KeyDef {
            name: None,
            columns: pk.iter().map(|s| s.to_string()).collect(),
        }),
        ..Default::default()
    }
}

fn ty<'a>(t: &'a TableSchema, c: &str) -> &'a str {
    &t.columns
        .iter()
        .find(|x| x.name == c)
        .unwrap_or_else(|| panic!("no column {c} in {t:?}"))
        .data_type
}

/// A PostgreSQL table with the types that matter.
fn pg_source(name: &str) -> TableSchema {
    let mut id = col("id", "bigint");
    id.nullable = false;
    table(
        name,
        vec![
            id,
            col("qty", "integer"),
            col("small", "smallint"),
            col("amount", "numeric(12,2)"),
            col("whole", "numeric(10,0)"),
            col("price", "double precision"),
            col("ratio", "real"),
            col("code", "char(1)"),
            col("name", "varchar(40)"),
            col("notes", "text"),
            col("flag", "boolean"),
            col("day", "date"),
            col("at", "timestamp"),
            col("at_tz", "timestamp with time zone"),
            col("clock", "time"),
            col("uid", "uuid"),
            col("data", "bytea"),
            col("doc", "jsonb"),
            col("span", "interval"),
            col("nums", "integer[]"),
            col("select", "integer"),
        ],
        &["id"],
    )
}

#[test]
fn postgres_to_q() {
    let r = convert(
        &[pg_source("orders")],
        "postgres",
        "kdb",
        &Options::default(),
    )
    .unwrap();
    let t = &r.tables[0];
    for (c, want) in [
        ("id", "long"),
        ("qty", "int"),
        ("small", "short"),
        ("amount", "float"),
        ("whole", "long"),
        ("price", "float"),
        ("ratio", "real"),
        ("code", "char"),
        ("name", "string"),
        ("notes", "string"),
        ("flag", "boolean"),
        ("day", "date"),
        ("at", "timestamp"),
        ("at_tz", "timestamp"),
        ("clock", "time"),
        ("uid", "guid"),
        ("data", "byte list"),
        ("doc", "string"),
        ("span", "timespan"),
        ("nums", "int list"),
        // A reserved word renamed as `.Q.id` does.
        ("select1", "int"),
    ] {
        assert_eq!(ty(t, c), want, "{c}");
    }
    assert_eq!(t.primary_key.as_ref().unwrap().columns, ["id"]);
    let has = |code: IssueCode, object: &str| {
        r.issues
            .iter()
            .any(|i| i.code == code && i.object.as_deref() == Some(object))
    };
    assert!(has(IssueCode::PrecisionLoss, "amount"));
    assert!(has(IssueCode::TimeZoneLoss, "at_tz"));
    assert!(has(IssueCode::RangeLoss, "span"));
    assert!(has(IssueCode::IdentifierRenamed, "select"));
    // NOT NULL doesn't exist in q.
    assert!(has(IssueCode::NullabilityChanged, "id"));
    // The driver writes the table.
    let d = driver("kdb");
    let ddl = d
        .table_ddl(
            t,
            DdlParts {
                drop: false,
                if_exists: false,
                create: true,
                indexes: false,
                foreign_keys: false,
            },
        )
        .unwrap();
    assert!(
        ddl.starts_with("orders:([id:`long$()] qty:`int$(); small:`short$(); amount:`float$();"),
        "{ddl}"
    );
}

#[test]
fn q_to_other_engines() {
    let kt = table(
        "trade",
        vec![
            col("sym", "symbol"),
            col("time", "timestamp"),
            col("px", "float"),
            col("size", "long"),
            col("side", "char"),
            col("venue", "string"),
            col("id", "guid"),
            col("month", "month"),
            col("open", "minute"),
            col("hold", "timespan"),
            col("legs", "float list"),
            col("raw", "byte list"),
            col("any", "list"),
        ],
        &["sym", "time"],
    );
    let r = convert(
        std::slice::from_ref(&kt),
        "kdb",
        "postgres",
        &Options::default(),
    )
    .unwrap();
    let t = &r.tables[0];
    // Keys become the primary key.
    assert_eq!(t.primary_key.as_ref().unwrap().columns, ["sym", "time"]);
    let logical = |c: &str| {
        r.columns
            .iter()
            .find(|m| m.column == c)
            .map(|m| m.logical.clone())
            .unwrap()
    };
    assert_eq!(
        logical("sym"),
        L::Varchar {
            len: None,
            unicode: true
        }
    );
    assert_eq!(logical("venue"), L::Text { unicode: true });
    assert_eq!(logical("id"), L::Uuid);
    assert_eq!(logical("month"), L::Date);
    assert_eq!(logical("hold"), L::Interval);
    assert_eq!(
        logical("legs"),
        L::Array {
            of: Box::new(L::Float { bytes: 8 })
        }
    );
    assert_eq!(logical("raw"), L::Blob);
    // A general list has no equivalent: reported for review.
    assert!(r.issues.iter().any(|i| i.code == IssueCode::TypeUnknown
        && i.object.as_deref() == Some("any")
        && i.severity == Severity::Warning));
    // q to q keeps every type as is.
    let same = convert(std::slice::from_ref(&kt), "kdb", "kdb", &Options::default()).unwrap();
    for (a, b) in kt.columns.iter().zip(&same.tables[0].columns) {
        assert_eq!(a.data_type, b.data_type);
    }
}

#[test]
fn q_compares_by_logical_type() {
    use dbine_schema::compare::same_type;
    assert!(same_type("long", "kdb", "bigint", "postgres"));
    assert!(same_type("guid", "kdb", "uuid", "postgres"));
    assert!(!same_type("symbol", "kdb", "string", "kdb"));
    assert!(!same_type("list", "kdb", "jsonb", "postgres"));
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_drivers::all()
        .iter()
        .find(|d| d.info().id == id)
        .cloned()
        .unwrap_or_else(|| panic!("no driver {id}"))
}

/// `host:port[:user:password]` from `DBINE_TEST_KDB_URL`, or `None` to skip.
fn kdb_cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_KDB_URL").ok()?;
    let mut parts = url.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_string();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5000);
    Some(ConnectionConfig {
        host,
        port,
        username: parts.next().map(str::to_string),
        password: parts.next().map(str::to_string),
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    })
}

async fn q(s: &mut Box<dyn Session>, text: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 1000, &mut out)
        .await
        .map(|_| out)
        .map_err(|e| e.to_string())
}

async fn run_all(s: &mut Box<dyn Session>, statements: &[String]) {
    for st in statements {
        q(s, st).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
}

/// `meta`'s types of a table, as the driver reads them.
async fn types(s: &mut Box<dyn Session>, name: &str) -> Vec<(String, String, bool)> {
    let r = ObjectRef {
        kind: "table".into(),
        schema: None,
        name: name.into(),
    };
    s.columns(&r)
        .await
        .unwrap()
        .into_iter()
        .map(|c| (c.name, c.data_type, c.primary_key))
        .collect()
}

async fn read_table(s: &mut Box<dyn Session>, name: &str) -> TableSchema {
    s.database_schema()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no table {name}"))
}

async fn cleanup(s: &mut Box<dyn Session>) {
    let _ = q(
        s,
        "{![`.;();0b;x]} tables[] where tables[] like \"dbine_sc_*\"",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn live_create_convert_and_read_back() {
    let Some(cfg) = kdb_cfg() else { return };
    let d = driver("kdb");
    let mut s = d.connect(&cfg, None).await.unwrap();
    cleanup(&mut s).await;

    // PostgreSQL's table, created on kdb+ by the sync's create.
    let r = convert(
        &[pg_source("dbine_sc_orders")],
        "postgres",
        "kdb",
        &Options::default(),
    )
    .unwrap();
    let script = d
        .sync_script(&[TableChange::Create {
            table: r.tables[0].clone(),
        }])
        .unwrap();
    run_all(&mut s, &script.statements).await;
    // Creating it again is refused on the server: q would replace it.
    let again = q(&mut s, &script.statements[0]).await.unwrap_err();
    assert!(again.contains("Ya existe"), "{again}");
    let got = types(&mut s, "dbine_sc_orders").await;
    let want = [
        ("id", "long", true),
        ("qty", "int", false),
        ("small", "short", false),
        ("amount", "float", false),
        ("whole", "long", false),
        ("price", "float", false),
        ("ratio", "real", false),
        ("code", "char", false),
        // Empty columns of lists are general lists until a row arrives.
        ("name", "list", false),
        ("flag", "boolean", false),
        ("day", "date", false),
        ("at", "timestamp", false),
        ("clock", "time", false),
        ("uid", "guid", false),
        ("span", "timespan", false),
        ("select1", "int", false),
    ];
    for (c, t, k) in want {
        assert!(
            got.iter().any(|(n, ty, pk)| n == c && ty == t && *pk == k),
            "{c} {t}: {got:?}"
        );
    }
    // A row makes the string and list columns typed.
    let cols: Vec<String> = ["id", "name", "nums", "data"]
        .iter()
        .map(|c| c.to_string())
        .collect();
    let ins = d
        .insert_script(
            &ObjectRef {
                kind: "table".into(),
                schema: None,
                name: "dbine_sc_orders".into(),
            },
            &cols,
            &[vec![
                serde_json::json!(1),
                serde_json::json!("ann"),
                serde_json::Value::Null,
                serde_json::Value::Null,
            ]],
        )
        .unwrap();
    run_all(&mut s, &[ins]).await;
    let got = types(&mut s, "dbine_sc_orders").await;
    assert!(got.iter().any(|(n, t, _)| n == "name" && t == "string"));

    // kdb+'s table back to PostgreSQL's terms, read by the driver.
    q(
        &mut s,
        "dbine_sc_trade:([sym:`a`b; time:2#.z.p] px:1.5 2.5; size:10 20; side:\"BS\"; venue:(\"x\";\"y\"); legs:(1 2f;3 4f))",
    )
    .await
    .unwrap();
    let kt = read_table(&mut s, "dbine_sc_trade").await;
    assert_eq!(kt.primary_key.as_ref().unwrap().columns, ["sym", "time"]);
    let pg = convert(
        std::slice::from_ref(&kt),
        "kdb",
        "postgres",
        &Options::default(),
    )
    .unwrap();
    let t = &pg.tables[0];
    assert_eq!(t.primary_key.as_ref().unwrap().columns, ["sym", "time"]);
    assert_eq!(ty(t, "size"), "bigint");
    assert_eq!(ty(t, "px"), "double precision");
    assert!(ty(t, "time").starts_with("timestamp"), "{}", ty(t, "time"));
    assert_eq!(ty(t, "legs"), "double precision[]");
    // And to q again, through SQLite (a file, no server).
    let lite = convert(&[kt], "kdb", "sqlite", &Options::default()).unwrap();
    let back = convert(&lite.tables, "sqlite", "kdb", &Options::default()).unwrap();
    let mut bt = back.tables[0].clone();
    bt.name = "dbine_sc_back".into();
    let ddl = d.sync_script(&[TableChange::Create { table: bt }]).unwrap();
    run_all(&mut s, &ddl.statements).await;
    let got = types(&mut s, "dbine_sc_back").await;
    assert!(got.iter().any(|(n, _, pk)| n == "sym" && *pk), "{got:?}");
    cleanup(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn live_sync_and_rename() {
    let Some(cfg) = kdb_cfg() else { return };
    let d = driver("kdb");
    let mut s = d.connect(&cfg, None).await.unwrap();
    cleanup(&mut s).await;
    q(
        &mut s,
        "dbine_sc_t:([id:1 2 3] sym:`a`b`c; px:1 2 3; note:(\"10\";\"x\";\"30\"); gone:1.5 2.5 3.5)",
    )
    .await
    .unwrap();
    let old = read_table(&mut s, "dbine_sc_t").await;
    let mut new = old.clone();
    new.columns.retain(|c| c.name != "gone");
    for c in &mut new.columns {
        match c.name.as_str() {
            "px" => c.data_type = "float".into(),
            "note" => c.data_type = "long".into(),
            "sym" => c.data_type = "string".into(),
            "id" => c.data_type = "int".into(),
            _ => {}
        }
    }
    new.columns.push(col("size", "long"));
    new.columns.push(col("memo", "string"));
    new.columns.push(col("tags", "symbol list"));
    new.primary_key = Some(KeyDef {
        name: None,
        columns: vec!["id".into(), "sym".into()],
    });
    let script = d
        .sync_script(&[TableChange::Alter {
            old: old.clone(),
            new,
        }])
        .unwrap();
    run_all(&mut s, &script.statements).await;
    let got = types(&mut s, "dbine_sc_t").await;
    let want = [
        ("id", "int", true),
        ("sym", "string", true),
        ("px", "float", false),
        ("note", "long", false),
        ("size", "long", false),
        ("memo", "string", false),
        ("tags", "symbol list", false),
    ];
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (c, t, k) in want {
        assert!(
            got.iter().any(|(n, ty, pk)| n == c && ty == t && *pk == k),
            "{c} {t}: {got:?}"
        );
    }
    // Values were cast, nulls where they don't convert, nulls in new columns.
    let out = q(
        &mut s,
        "(exec note from dbine_sc_t; exec px from dbine_sc_t; exec size from dbine_sc_t)",
    )
    .await
    .unwrap();
    let text = serde_json::to_string(&out.results[0].rows).unwrap();
    assert!(text.contains("10") && text.contains("30"), "{text}");

    // Rename a column and the table.
    let tref = ObjectRef {
        kind: "table".into(),
        schema: None,
        name: "dbine_sc_t".into(),
    };
    let ren = d
        .rename_script(&RenameRequest {
            target: RenameTarget::Column {
                table: tref.clone(),
                column: "px".into(),
            },
            new_name: "price".into(),
            table: None,
            definition: None,
        })
        .unwrap();
    run_all(&mut s, &ren.statements).await;
    // Renaming to a column that exists fails on the server.
    let clash = d
        .rename_script(&RenameRequest {
            target: RenameTarget::Column {
                table: tref.clone(),
                column: "price".into(),
            },
            new_name: "note".into(),
            table: None,
            definition: None,
        })
        .unwrap();
    // One statement: the check that the name is free runs with the change.
    let e = q(&mut s, clash.statements.last().unwrap())
        .await
        .unwrap_err();
    assert!(e.contains("Ya existe"), "{e}");
    let ren = d
        .rename_script(&RenameRequest {
            target: RenameTarget::Object {
                object: tref,
                parent: None,
            },
            new_name: "dbine_sc_u".into(),
            table: None,
            definition: None,
        })
        .unwrap();
    run_all(&mut s, &ren.statements).await;
    let got = types(&mut s, "dbine_sc_u").await;
    assert!(got.iter().any(|(n, _, pk)| n == "price" && !pk), "{got:?}");
    assert!(got.iter().any(|(n, _, pk)| n == "sym" && *pk), "{got:?}");
    let gone = q(&mut s, "dbine_sc_t").await;
    assert!(gone.is_err(), "the old name is gone");

    // Drop.
    let u = read_table(&mut s, "dbine_sc_u").await;
    let drop = d.sync_script(&[TableChange::Drop { table: u }]).unwrap();
    run_all(&mut s, &drop.statements).await;
    assert!(q(&mut s, "dbine_sc_u").await.is_err());
    cleanup(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn live_tables_on_disk_are_not_changed() {
    let Some(cfg) = kdb_cfg() else { return };
    let d = driver("kdb");
    let mut s = d.connect(&cfg, None).await.unwrap();
    cleanup(&mut s).await;
    // A splayed table of its own, on the server's disk (in its directory:
    // `-u` keeps it from writing elsewhere).
    q(
        &mut s,
        "`:dbine_sc_splay/ set ([] a:1 2 3; b:4 5 6); dbine_sc_sp:get `:dbine_sc_splay/",
    )
    .await
    .unwrap();
    let objects = s.list_objects().await.unwrap();
    let kind = objects
        .iter()
        .find(|o| o.name == "dbine_sc_sp")
        .map(|o| o.kind.clone())
        .unwrap();
    assert_eq!(kind, "splayed_table");
    let t = table("dbine_sc_sp", vec![col("a", "long"), col("b", "long")], &[]);
    let mut on_disk = t.clone();
    on_disk.kind = kind.clone();
    let mut wider = on_disk.clone();
    wider.columns.push(col("c", "long"));
    // Refused before writing any q.
    let e = d
        .sync_script(&[TableChange::Alter {
            old: on_disk.clone(),
            new: wider.clone(),
        }])
        .unwrap_err();
    assert!(e.to_string().contains("tabla en disco"), "{e}");
    // And on the server, if it arrives as a table in memory.
    let mut wider = t.clone();
    wider.columns.push(col("c", "long"));
    let script = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: wider,
        }])
        .unwrap();
    let e = q(&mut s, &script.statements[0]).await.unwrap_err();
    assert!(e.contains("tabla en disco"), "{e}");
    let drop = d.sync_script(&[TableChange::Drop { table: t }]).unwrap();
    assert!(q(&mut s, &drop.statements[0]).await.is_err());
    let got = types(&mut s, "dbine_sc_sp").await;
    assert_eq!(got.len(), 2);
    q(&mut s, "![`.;();0b;enlist `dbine_sc_sp]; hdel each `$\":dbine_sc_splay/\",/:string `.d`a`b; hdel `:dbine_sc_splay")
        .await
        .unwrap();
    cleanup(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn live_indexes_become_attributes() {
    let Some(cfg) = kdb_cfg() else { return };
    let d = driver("kdb");
    let mut s = d.connect(&cfg, None).await.unwrap();
    // A PostgreSQL table with a unique index, another index and one of two
    // columns: u#, g# and nothing (said) in kdb+.
    let mut pg = table(
        "dbine_meta_sc_ix",
        vec![
            col("id", "bigint"),
            col("code", "text"),
            col("qty", "integer"),
        ],
        &[],
    );
    let index = |name: &str, cols: &[&str], unique: bool| dbine_driver::IndexDef {
        name: name.into(),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        unique,
        kind: Some("btree".into()),
        ..Default::default()
    };
    pg.indexes = vec![
        index("ix_code", &["code"], true),
        index("ix_qty", &["qty"], false),
        index("ix_both", &["code", "qty"], false),
    ];
    let r = convert(
        std::slice::from_ref(&pg),
        "postgres",
        "kdb",
        &Options::default(),
    )
    .unwrap();
    assert!(r
        .issues
        .iter()
        .any(|i| i.code == IssueCode::IndexDropped && i.object.as_deref() == Some("ix_both")));
    let ddl = d
        .sync_script(&[TableChange::Create {
            table: r.tables[0].clone(),
        }])
        .unwrap();
    run_all(&mut s, &ddl.statements).await;
    let back = read_table(&mut s, "dbine_meta_sc_ix").await;
    let mut got: Vec<(String, bool, String)> = back
        .indexes
        .iter()
        .map(|i| {
            (
                i.columns[0].clone(),
                i.unique,
                i.kind.clone().unwrap_or_default(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("code".to_string(), true, "u#".to_string()),
            ("qty".into(), false, "g#".into())
        ]
    );
    // And back to PostgreSQL: a unique index and another.
    let again = convert(&[back], "kdb", "postgres", &Options::default()).unwrap();
    let uniques: Vec<bool> = again.tables[0].indexes.iter().map(|i| i.unique).collect();
    assert_eq!(uniques.iter().filter(|u| **u).count(), 1, "{uniques:?}");
    assert_eq!(uniques.len(), 2);
    q(&mut s, "delete dbine_meta_sc_ix from `.").await.unwrap();
}

/// A migration's tables from PostgreSQL's schemas into kdb+, as the app
/// plans it (schemas kept, the default one to the root): `sales` lands in
/// the namespace `.dbine_sc_sales`, `public` in the root, and the rows go
/// to each by the converted table's (schema, name).
#[tokio::test]
#[ignore]
async fn live_schemas_land_in_namespaces() {
    use dbine_driver::transfer::{BatchSource, Cell, LoadSpec, RowBatch};
    struct One(Option<RowBatch>);
    #[dbine_driver::async_trait]
    impl BatchSource for One {
        async fn next(&mut self) -> Option<RowBatch> {
            self.0.take()
        }
    }
    let Some(cfg) = kdb_cfg() else { return };
    let d = driver("kdb");
    let mut s = d.connect(&cfg, None).await.unwrap();
    cleanup(&mut s).await;
    let drop_ns = d.drop_schema_script(None, ".dbine_sc_sales", true).unwrap();
    let _ = q(&mut s, &drop_ns).await;
    let mut public = pg_source("dbine_sc_orders");
    public.schema = Some("public".into());
    let mut sales = public.clone();
    sales.schema = Some("dbine_sc_sales".into());
    let opts = Options {
        keep_schemas: true,
        rename_schemas: vec![("public".into(), String::new())],
        ..Options::default()
    };
    let conv = convert(&[public, sales], "postgres", "kdb", &opts).unwrap();
    let placed: Vec<(Option<&str>, &str)> = conv
        .tables
        .iter()
        .map(|t| (t.schema.as_deref(), t.name.as_str()))
        .collect();
    assert_eq!(
        placed,
        [
            (None, "dbine_sc_orders"),
            (Some(".dbine_sc_sales"), "dbine_sc_orders")
        ]
    );
    for (i, t) in conv.tables.iter().enumerate() {
        let script = d
            .sync_script(&[TableChange::Create { table: t.clone() }])
            .unwrap();
        run_all(&mut s, &script.statements).await;
        let load = LoadSpec {
            table: ObjectRef {
                kind: "table".into(),
                schema: t.schema.clone(),
                name: t.name.clone(),
            },
            columns: vec!["id".into(), "qty".into()],
            table_lock: false,
            keep_identity: false,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        };
        let rows = (0..=i as i64)
            .map(|n| vec![Cell::Int(n + 1), Cell::Int(10 * (n + 1))])
            .collect();
        let mut src = One(Some(RowBatch { rows, bytes: 0 }));
        let n = s.bulk_load(&load, &[], &mut src, &|_| {}).await.unwrap();
        assert_eq!(n, i as u64 + 1);
    }
    // Each in its place: the root's has one row, the namespace's two.
    let count = |o: &QueryOutcome| o.results[0].rows[0][0].clone();
    let root = q(&mut s, "count dbine_sc_orders").await.unwrap();
    assert_eq!(count(&root), serde_json::json!(1));
    let ns = q(&mut s, "count .dbine_sc_sales.dbine_sc_orders")
        .await
        .unwrap();
    assert_eq!(count(&ns), serde_json::json!(2));
    let objs = s.list_objects().await.unwrap();
    assert!(objs
        .iter()
        .any(|o| o.schema.as_deref() == Some(".dbine_sc_sales") && o.name == "dbine_sc_orders"));
    let schemas = s.list_schemas().await.unwrap().unwrap();
    assert!(schemas.iter().any(|x| x.name == ".dbine_sc_sales"));
    run_all(&mut s, &[drop_ns]).await;
    cleanup(&mut s).await;
}

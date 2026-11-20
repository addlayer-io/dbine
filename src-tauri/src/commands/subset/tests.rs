//! The subset end to end: SQLite → SQLite always (files in a temp dir),
//! PostgreSQL → PostgreSQL against `dbine-test-postgres`
//! (`DBINE_TEST_POSTGRES_URL`, ignored by default).

use super::*;
use dbine_core::{SavedConnection, StateStore};
use dbine_driver::ConnectionConfig;
use mask::FakeKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A test's own folder (its state file), removed when the test ends,
/// passed or not.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

fn dir(tag: &str) -> TempDir {
    let d = std::env::temp_dir().join(format!(
        "dbine-subset-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

fn save(state: &AppState, id: &str, cfg: ConnectionConfig, tags: Vec<String>) {
    let conn = SavedConnection {
        id: id.into(),
        name: id.into(),
        color: None,
        config: cfg,
        save_password: false,
        folder_id: None,
        tags,
        mcp_level: None,
        updated_at: String::new(),
    };
    state.store.save_connection(&conn).unwrap();
}

fn sqlite(path: &Path) -> ConnectionConfig {
    ConnectionConfig {
        driver: "sqlite".into(),
        host: path.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

async fn sql(state: &AppState, conn: &str, db: Option<&str>, text: &str) -> QueryOutcome {
    let cfg = state.resolve_config(conn).unwrap();
    let mut s = dbine_drivers::open_session(&cfg, db).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(text, 10_000, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{:?}\n{text}", out.error);
    out
}

async fn one(state: &AppState, conn: &str, db: Option<&str>, text: &str) -> i64 {
    let out = sql(state, conn, db, text).await;
    let v = &out.results.last().unwrap().rows[0][0];
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("{v:?} for {text}"))
}

fn events() -> (Emit, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let s = seen.clone();
    (Arc::new(move |v| s.lock().unwrap().push(v)), seen)
}

const SCHEMA: &str = "
CREATE TABLE countries (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE customers (id INTEGER PRIMARY KEY, nombre TEXT NOT NULL, email TEXT, telefono TEXT,
    country_id INTEGER REFERENCES countries(id), referred_by INTEGER REFERENCES customers(id));
CREATE TABLE products (id INTEGER PRIMARY KEY, title TEXT, price REAL);
CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id), total REAL, created TEXT);
CREATE TABLE order_items (order_id INTEGER NOT NULL REFERENCES orders(id), line INTEGER NOT NULL,
    product_id INTEGER NOT NULL REFERENCES products(id), qty INTEGER, PRIMARY KEY (order_id, line));
CREATE TABLE item_notes (id INTEGER PRIMARY KEY, order_id INTEGER NOT NULL, line INTEGER NOT NULL, note TEXT,
    FOREIGN KEY (order_id, line) REFERENCES order_items(order_id, line));
CREATE TABLE departments (id INTEGER PRIMARY KEY, name TEXT, manager_id INTEGER REFERENCES employees(id));
CREATE TABLE employees (id INTEGER PRIMARY KEY, dept_id INTEGER NOT NULL REFERENCES departments(id), email TEXT);
CREATE TABLE unrelated (id INTEGER PRIMARY KEY);
";

fn data() -> String {
    let mut s = String::new();
    for i in 1..=3 {
        s.push_str(&format!(
            "INSERT INTO countries VALUES ({i}, 'country {i}');\n"
        ));
    }
    for i in 1..=50 {
        // Each customer referred by the previous one: a chain to follow up.
        let by = if i > 1 {
            (i - 1).to_string()
        } else {
            "NULL".into()
        };
        s.push_str(&format!("INSERT INTO customers VALUES ({i}, 'Cliente {i}', 'c{i}@real.com', '+54 11 5555-{i:04}', {}, {by});\n", i % 3 + 1));
    }
    for i in 1..=10 {
        s.push_str(&format!(
            "INSERT INTO products VALUES ({i}, 'product {i}', {}.5);\n",
            i * 10
        ));
    }
    for i in 1..=200 {
        s.push_str(&format!(
            "INSERT INTO orders VALUES ({i}, {}, {}.25, '2024-03-{:02} 10:00:00');\n",
            i % 50 + 1,
            i * 3,
            i % 28 + 1
        ));
        for line in 1..=2 {
            s.push_str(&format!(
                "INSERT INTO order_items VALUES ({i}, {line}, {}, {line});\n",
                (i + line) % 10 + 1
            ));
        }
    }
    s.push_str("INSERT INTO item_notes VALUES (1, 7, 2, 'fragile'), (2, 9, 1, 'gift');\n");
    // The cycle: departments ⇄ employees, and an employee with a customer's email.
    s.push_str("INSERT INTO departments VALUES (1, 'Ventas', NULL), (2, 'IT', NULL);\n");
    s.push_str("INSERT INTO employees VALUES (10, 1, 'c5@real.com'), (11, 2, 'boss@real.com'), (12, 2, 'dev@real.com');\n");
    s.push_str("UPDATE departments SET manager_id = 10 WHERE id = 1; UPDATE departments SET manager_id = 11 WHERE id = 2;\n");
    s
}

fn args(table: &str, filter: StartFilter, children: Option<ChildrenOptions>) -> SubsetArgs {
    SubsetArgs {
        run_id: format!("t-{table}-{}", uuid::Uuid::new_v4()),
        connection_id: "src".into(),
        database: String::new(),
        table: ObjectRef {
            kind: "table".into(),
            schema: None,
            name: table.into(),
        },
        filter,
        children,
        target_connection_id: "tgt".into(),
        target_database: String::new(),
    }
}

fn mask(table: &str, cols: &[(&str, MaskRule)]) -> TableMask {
    TableMask {
        schema: None,
        name: table.into(),
        columns: cols
            .iter()
            .map(|(c, r)| (c.to_string(), r.clone()))
            .collect(),
    }
}

/// A state with SQLite `src`, `tgt` and `prod` connections; its folder (the
/// state and the databases) goes when it's dropped.
struct Setup {
    state: AppState,
    _dir: TempDir,
}

impl std::ops::Deref for Setup {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.state
    }
}

async fn setup(tag: &str) -> Setup {
    let d = dir(tag);
    let state = AppState::new(StateStore::open(&d.join("state.sqlite")).unwrap());
    save(&state, "src", sqlite(&d.join("src.db")), vec![]);
    save(&state, "tgt", sqlite(&d.join("tgt.db")), vec![]);
    save(
        &state,
        "prod",
        sqlite(&d.join("prod.db")),
        vec!["Prod".into()],
    );
    sql(&state, "src", None, &format!("{SCHEMA}{}", data())).await;
    // The targets exist (the plan opens them read-only).
    sql(&state, "tgt", None, "SELECT 1").await;
    sql(&state, "prod", None, "SELECT 1").await;
    Setup { state, _dir: d }
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_orders_with_children_parents_and_masking() {
    let state = setup("orders").await;
    let filter = StartFilter {
        expression: Some("id <= 20".into()),
        ..Default::default()
    };
    let a = args(
        "orders",
        filter,
        Some(ChildrenOptions {
            depth: 1,
            max_rows: 1000,
        }),
    );

    let (emit, _) = events();
    let p = plan(&state, &a, emit).await.unwrap();
    let rows = |n: &str| p.tables.iter().find(|t| t.name == n).map(|t| t.rows);
    assert_eq!(rows("orders"), Some(20));
    assert_eq!(rows("order_items"), Some(40));
    // Orders 1..20 belong to customers 2..21; each referred by the previous: 1..21.
    assert_eq!(rows("customers"), Some(21));
    assert_eq!(rows("countries"), Some(3));
    assert!(
        rows("unrelated").is_none()
            && rows("departments").is_none()
            && rows("item_notes").is_none(),
        "item_notes is two levels down"
    );
    let pos = |n: &str| p.tables.iter().position(|t| t.name == n).unwrap();
    assert!(
        pos("countries") < pos("customers")
            && pos("customers") < pos("orders")
            && pos("orders") < pos("order_items")
            && pos("products") < pos("order_items")
    );
    assert!(p
        .tables
        .iter()
        .all(|t| !t.exists && t.create_ddl.is_some() && t.error.is_none()));
    let cust = &p.tables[pos("customers")];
    let suggested = |c: &str| {
        cust.columns
            .iter()
            .find(|x| x.name == c)
            .unwrap()
            .suggested
            .clone()
    };
    assert_eq!(
        suggested("email"),
        MaskRule::Fake {
            kind: FakeKind::Email
        }
    );
    assert_eq!(
        suggested("nombre"),
        MaskRule::Fake {
            kind: FakeKind::Name
        }
    );
    assert_eq!(
        suggested("telefono"),
        MaskRule::Fake {
            kind: FakeKind::Phone
        }
    );
    assert_eq!(suggested("id"), MaskRule::Keep, "keys aren't masked");
    assert!(p.confirm_label.is_none());
    // The plan wrote nothing.
    assert_eq!(
        one(
            &state,
            "tgt",
            None,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'"
        )
        .await,
        0
    );

    let run_args = RunArgs {
        subset: a.clone(),
        masks: vec![mask(
            "customers",
            &[
                ("email", suggested("email")),
                ("nombre", suggested("nombre")),
                ("telefono", MaskRule::Null),
            ],
        )],
        confirm: String::new(),
        seed: Some(99),
    };
    let (emit, seen) = events();
    let r = run(&state, &run_args, emit).await.unwrap();
    assert!(!r.cancelled);
    assert!(
        r.tables
            .iter()
            .all(|t| t.status == "done" && t.created && t.written == t.rows),
        "{r:#?}"
    );
    assert!(seen.lock().unwrap().iter().any(|e| e["phase"] == "insert"));

    for (t, n) in [
        ("orders", 20),
        ("order_items", 40),
        ("customers", 21),
        ("countries", 3),
    ] {
        assert_eq!(
            one(&state, "tgt", None, &format!("SELECT COUNT(*) FROM {t}")).await,
            n,
            "{t}"
        );
    }
    // Every reference resolves in the target.
    for q in [
        "SELECT COUNT(*) FROM orders o LEFT JOIN customers c ON c.id = o.customer_id WHERE c.id IS NULL",
        "SELECT COUNT(*) FROM order_items i LEFT JOIN products p ON p.id = i.product_id WHERE p.id IS NULL",
        "SELECT COUNT(*) FROM customers c LEFT JOIN customers r ON r.id = c.referred_by WHERE c.referred_by IS NOT NULL AND r.id IS NULL",
        "SELECT COUNT(*) FROM customers c LEFT JOIN countries k ON k.id = c.country_id WHERE k.id IS NULL",
    ] {
        assert_eq!(one(&state, "tgt", None, q).await, 0, "{q}");
    }
    // Masked: no real email or name left, phones NULL, other columns intact.
    assert_eq!(one(&state, "tgt", None, "SELECT COUNT(*) FROM customers WHERE email LIKE '%@real.com' OR nombre LIKE 'Cliente %'").await, 0);
    assert_eq!(
        one(
            &state,
            "tgt",
            None,
            "SELECT COUNT(*) FROM customers WHERE telefono IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(
        one(
            &state,
            "tgt",
            None,
            "SELECT COUNT(*) FROM customers WHERE email LIKE '%@%'"
        )
        .await,
        21
    );
    assert_eq!(
        one(
            &state,
            "tgt",
            None,
            "SELECT CAST(total * 100 AS INTEGER) FROM orders WHERE id = 7"
        )
        .await,
        2125
    );
    // The source is untouched.
    assert_eq!(
        one(
            &state,
            "src",
            None,
            "SELECT COUNT(*) FROM customers WHERE email LIKE '%@real.com'"
        )
        .await,
        50
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_cycle_composite_key_and_consistent_masks() {
    let state = setup("cycle").await;

    // A cycle (departments ⇄ employees): cut at the nullable manager_id,
    // written NULL and set at the end. The same email masks the same in
    // two tables.
    let a = args(
        "departments",
        StartFilter::default(),
        Some(ChildrenOptions {
            depth: 1,
            max_rows: 1000,
        }),
    );
    let (emit, _) = events();
    let p = plan(&state, &a, emit).await.unwrap();
    assert_eq!(p.cycles.len(), 1, "{:?}", p.cycles);
    assert!(p.cycles[0].contains("manager_id"));
    let email = MaskRule::Fake {
        kind: FakeKind::Email,
    };
    let r = run(
        &state,
        &RunArgs {
            subset: a,
            masks: vec![mask("employees", &[("email", email.clone())])],
            confirm: String::new(),
            seed: Some(5),
        },
        events().0,
    )
    .await
    .unwrap();
    assert!(r.tables.iter().all(|t| t.status == "done"), "{r:#?}");
    assert_eq!(
        one(&state, "tgt", None, "SELECT COUNT(*) FROM employees").await,
        3
    );
    assert_eq!(
        one(
            &state,
            "tgt",
            None,
            "SELECT manager_id FROM departments WHERE id = 2"
        )
        .await,
        11,
        "the cut column is set afterwards"
    );
    let masked = sql(
        &state,
        "tgt",
        None,
        "SELECT email FROM employees WHERE id = 10",
    )
    .await
    .results[0]
        .rows[0][0]
        .clone();
    assert_eq!(
        masked,
        Masker::with_seed(5).apply(&email, &json!("c5@real.com"), Shape::default()),
        "deterministic per value"
    );

    // A composite foreign key (item_notes → order_items) fetched by its
    // most varied column and matched whole.
    let a = args("item_notes", StartFilter::default(), None);
    let p = plan(&state, &a, events().0).await.unwrap();
    let rows = |n: &str| p.tables.iter().find(|t| t.name == n).map(|t| t.rows);
    assert_eq!(
        (rows("item_notes"), rows("order_items"), rows("orders")),
        (Some(2), Some(2), Some(2))
    );

    // N % and N rows of the start.
    let pct = args(
        "customers",
        StartFilter {
            limit: Limit::Percent { percent: 10.0 },
            ..Default::default()
        },
        None,
    );
    let p = plan(&state, &pct, events().0).await.unwrap();
    assert!(
        p.tables
            .iter()
            .any(|t| t.name == "customers" && t.rows >= 5),
        "10 % of 50, plus who referred them"
    );
    let n = args(
        "products",
        StartFilter {
            limit: Limit::Rows { count: 4 },
            ..Default::default()
        },
        None,
    );
    assert_eq!(plan(&state, &n, events().0).await.unwrap().total_rows, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_refusals() {
    let state = setup("refusals").await;
    // Never into the source itself.
    let mut a = args("orders", StartFilter::default(), None);
    a.target_connection_id = "src".into();
    assert!(plan(&state, &a, events().0).await.is_err());

    // A production target needs its name typed.
    let mut a = args("countries", StartFilter::default(), None);
    a.target_connection_id = "prod".into();
    let p = plan(&state, &a, events().0).await.unwrap();
    assert_eq!(p.confirm_label.as_deref(), Some("prod"));
    let mut ra = RunArgs {
        subset: a,
        masks: vec![],
        confirm: "nope".into(),
        seed: None,
    };
    assert!(run(&state, &ra, events().0).await.is_err());
    ra.confirm = "prod".into();
    run(&state, &ra, events().0).await.unwrap();
    assert_eq!(
        one(&state, "prod", None, "SELECT COUNT(*) FROM countries").await,
        3
    );

    // NULL on a NOT NULL column.
    let ra = RunArgs {
        subset: args("countries", StartFilter::default(), None),
        masks: vec![mask("countries", &[("name", MaskRule::Null)])],
        confirm: String::new(),
        seed: None,
    };
    assert!(run(&state, &ra, events().0).await.is_err());

    // Into an existing table with an extra column: rows go, the column is
    // left to its default.
    sql(&state, "tgt", None, "CREATE TABLE products (id INTEGER PRIMARY KEY, title TEXT, price REAL, stock INTEGER DEFAULT 7)").await;
    let a = args(
        "products",
        StartFilter {
            expression: Some("id < 4".into()),
            ..Default::default()
        },
        None,
    );
    let p = plan(&state, &a, events().0).await.unwrap();
    assert!(p.tables[0].exists && p.tables[0].create_ddl.is_none());
    run(
        &state,
        &RunArgs {
            subset: a,
            masks: vec![],
            confirm: String::new(),
            seed: None,
        },
        events().0,
    )
    .await
    .unwrap();
    assert_eq!(
        one(&state, "tgt", None, "SELECT SUM(stock) FROM products").await,
        21
    );
}

/// Every insert and the cut cycle's update go to the target driver's typed
/// methods, each with the target's columns and their types (SQLite's real
/// scripts run behind the spy, so the copy still lands).
#[tokio::test(flavor = "multi_thread")]
async fn sqlite_scripts_are_typed_by_the_targets_columns() {
    use crate::commands::typed_scripts::tests::Spy;
    let state = setup("typed").await;
    let a = args(
        "departments",
        StartFilter::default(),
        Some(ChildrenOptions {
            depth: 1,
            max_rows: 1000,
        }),
    );
    let emit = events().0;
    let (mut p, data, _, tgt_entry) = plan_and_collect(&state, &a, true, &emit).await.unwrap();
    let spy = Arc::new(Spy::new(p.tgt_driver));
    let as_driver: Arc<dyn Driver> = spy.clone();
    p.tgt_driver = Box::leak(Box::new(as_driver));
    let progress = |_: &str, _: &str, _: u64, _: u64| {};
    let (reports, cancelled) = {
        let mut tgt = tgt_entry.session.lock().await;
        write(
            &mut tgt,
            &tgt_entry,
            &p,
            &data,
            &[],
            &Masker::with_seed(1),
            &progress,
        )
        .await
    };
    state.sessions.remove(&format!("subset:{}:tgt", a.run_id));
    assert!(!cancelled);
    assert!(reports.iter().all(|r| r.status == "done"), "{reports:#?}");

    let calls = spy.rec.take();
    let kinds: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
    assert!(
        kinds
            .iter()
            .all(|k| *k == "insert_typed" || *k == "update_typed"),
        "{kinds:?}"
    );
    assert_eq!(kinds.iter().filter(|k| **k == "insert_typed").count(), 2);
    // Each insert names its table's target columns, each with its type.
    for (_, cols) in calls.iter().filter(|c| c.0 == "insert_typed") {
        assert!(!cols.is_empty());
        assert!(
            cols.iter().all(|c| !c.type_name.trim().is_empty()),
            "{cols:?}"
        );
    }
    let (_, cycle) = calls.iter().find(|c| c.0 == "update_typed").unwrap();
    let manager = cycle.iter().find(|c| c.name == "manager_id").unwrap();
    assert!(
        manager.type_name.eq_ignore_ascii_case("integer"),
        "{manager:?}"
    );
    assert_eq!(
        one(
            &state,
            "tgt",
            None,
            "SELECT manager_id FROM departments WHERE id = 2"
        )
        .await,
        11,
        "the cut column is set afterwards"
    );
}

/// PostgreSQL → PostgreSQL on `dbine-test-postgres`: two databases of the
/// same server, schema `public`, a serial key and the same plan.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_to_postgres() {
    let url = std::env::var("DBINE_TEST_POSTGRES_URL")
        .unwrap_or_else(|_| "postgres://postgres:pw@localhost:25010".into());
    let rest = url.split_once("://").unwrap().1;
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        database: "postgres".into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    let d = dir("pg");
    let state = AppState::new(StateStore::open(&d.join("state.sqlite")).unwrap());
    save(&state, "pg", cfg.clone(), vec![]);
    state.typed_secrets.insert(
        "pg".into(),
        [("password".to_string(), pass.to_string())]
            .into_iter()
            .collect(),
    );
    let mut admin = dbine_drivers::open_session(&cfg, Some("postgres"))
        .await
        .unwrap();
    for db in ["dbine_subset_src", "dbine_subset_tgt"] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.unwrap();
    }
    let src = Some("dbine_subset_src");
    let mut s = dbine_drivers::open_session(&cfg, src).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        &format!(
            "{}\n{}",
            SCHEMA
                .replace(
                    "manager_id INTEGER REFERENCES employees(id)",
                    "manager_id INTEGER"
                )
                .replace("price REAL", "price NUMERIC(10,2)"),
            data()
        ),
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    s.execute("ALTER TABLE departments ADD CONSTRAINT dep_mgr FOREIGN KEY (manager_id) REFERENCES employees(id)", 1, &mut out).await.unwrap();
    drop(s);

    let mut a = args(
        "orders",
        StartFilter {
            expression: Some("id <= 20".into()),
            ..Default::default()
        },
        Some(ChildrenOptions {
            depth: 2,
            max_rows: 1000,
        }),
    );
    a.connection_id = "pg".into();
    a.database = "dbine_subset_src".into();
    a.target_connection_id = "pg".into();
    a.target_database = "dbine_subset_tgt".into();
    a.table.schema = Some("public".into());
    let p = plan(&state, &a, events().0).await.unwrap();
    let rows = |n: &str| p.tables.iter().find(|t| t.name == n).map(|t| t.rows);
    assert_eq!(
        (
            rows("orders"),
            rows("order_items"),
            rows("customers"),
            rows("item_notes")
        ),
        (Some(20), Some(40), Some(21), Some(2))
    );
    let r = run(
        &state,
        &RunArgs {
            subset: a,
            masks: vec![mask(
                "customers",
                &[
                    (
                        "email",
                        MaskRule::Fake {
                            kind: FakeKind::Email,
                        },
                    ),
                    ("nombre", MaskRule::Hash),
                ],
            )],
            confirm: String::new(),
            seed: Some(1),
        },
        events().0,
    )
    .await
    .unwrap();
    assert!(
        r.tables
            .iter()
            .all(|t| t.status == "done" && t.written == t.rows),
        "{r:#?}"
    );
    let tgt = Some("dbine_subset_tgt");
    assert_eq!(
        one(&state, "pg", tgt, "SELECT COUNT(*) FROM order_items").await,
        40
    );
    assert_eq!(
        one(
            &state,
            "pg",
            tgt,
            "SELECT COUNT(*) FROM customers WHERE email LIKE '%@real.com'"
        )
        .await,
        0
    );
    assert_eq!(
        one(&state, "pg", tgt, "SELECT COUNT(*) FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY' AND table_name = 'order_items'").await,
        2,
        "foreign keys created after the data"
    );
    drop(admin);
    let mut admin = dbine_drivers::open_session(&cfg, Some("postgres"))
        .await
        .unwrap();
    for db in ["dbine_subset_src", "dbine_subset_tgt"] {
        let _ = admin.drop_database(db).await;
    }
}

/// One engine of [`vector_subset`]: its server, its vector type and
/// literal, and a vector as text in a query.
struct VecEngine {
    driver: &'static str,
    cfg: fn(&str) -> ConnectionConfig,
    schema: Option<&'static str>,
    vector_type: &'static str,
    literal: fn(&str) -> String,
    as_text: fn(&str) -> String,
    /// Run in each new database (pgvector's extension).
    setup: &'static str,
    drop: fn(&str) -> String,
}

/// A subset of a vector table into another database of the same server:
/// the vectors arrive whole through the target's insert script, a hashed
/// vector column gets random unit vectors of its dimension and a noised
/// one each number moved, as the engine's vector literal (MySQL and
/// MariaDB reject a vector written as text).
async fn vector_subset(e: VecEngine) {
    use crate::commands::vector_live_tests::{exec, f32s, first, open};
    use futures_util::FutureExt;
    let (src_db, tgt_db) = (
        format!("dbine_e2e_vecapp_subset_src_{}", e.driver),
        format!("dbine_e2e_vecapp_subset_tgt_{}", e.driver),
    );
    let admin = (e.cfg)(if e.driver == "postgres" {
        "postgres"
    } else {
        ""
    });
    let drop_dbs = || async {
        let mut a = open(&admin).await?;
        for db in [&src_db, &tgt_db] {
            exec(a.as_mut(), &(e.drop)(db)).await?;
        }
        Ok::<(), String>(())
    };
    drop_dbs().await.expect("clean up first");
    // The state folder goes when the test ends, the databases below, also
    // when a check failed or panicked.
    let d = dir(&format!("vec-{}", e.driver));
    let body = std::panic::AssertUnwindSafe(async {
        if crate::commands::vectors::of_type(e.driver, e.vector_type).is_none() {
            return Err(format!(
                "{}'s dialect doesn't take {} as a vector in this build",
                e.driver, e.vector_type
            ));
        }
        let mut a = open(&admin).await?;
        for db in [&src_db, &tgt_db] {
            exec(a.as_mut(), &format!("CREATE DATABASE {db}")).await?;
            if !e.setup.is_empty() {
                let mut s = open(&(e.cfg)(db)).await?;
                exec(s.as_mut(), e.setup).await?;
            }
        }
        drop(a);
        let mut s = open(&(e.cfg)(&src_db)).await?;
        exec(
            s.as_mut(),
            &format!(
                "CREATE TABLE emb (id int PRIMARY KEY, v {t}, h {t}, n {t})",
                t = e.vector_type
            ),
        )
        .await?;
        let values: Vec<String> = (1..=30)
            .map(|i| {
                format!(
                    "({i}, {}, {}, {})",
                    (e.literal)(&format!("[{i},-0.5,1e-07]")),
                    (e.literal)("[1,0,0]"),
                    (e.literal)("[0.5,0.5,0.5]")
                )
            })
            .collect();
        exec(
            s.as_mut(),
            &format!("INSERT INTO emb VALUES {}", values.join(", ")),
        )
        .await?;
        let read_sql = format!(
            "SELECT id, {}, {}, {} FROM emb ORDER BY id",
            (e.as_text)("v"),
            (e.as_text)("h"),
            (e.as_text)("n")
        );
        let want = first(exec(s.as_mut(), &read_sql).await?).rows;
        drop(s);
        let tgt_cfg = (e.cfg)(&tgt_db);
        let state = AppState::new(
            StateStore::open(&d.join("state.sqlite")).map_err(|err| err.to_string())?,
        );
        save(&state, "db", tgt_cfg.clone(), vec![]);
        state.typed_secrets.insert(
            "db".into(),
            [(
                "password".to_string(),
                tgt_cfg.password.clone().unwrap_or_default(),
            )]
            .into_iter()
            .collect(),
        );
        let mut a = args(
            "emb",
            StartFilter {
                expression: Some("id <= 10".into()),
                ..Default::default()
            },
            None,
        );
        a.connection_id = "db".into();
        a.database = src_db.clone();
        a.target_connection_id = "db".into();
        a.target_database = tgt_db.clone();
        a.table.schema = e.schema.map(str::to_string);
        let mut m = mask(
            "emb",
            &[
                ("h", MaskRule::Hash),
                ("n", MaskRule::Noise { percent: 10.0 }),
            ],
        );
        m.schema = e.schema.map(str::to_string);
        let r = run(
            &state,
            &RunArgs {
                subset: a,
                masks: vec![m],
                confirm: String::new(),
                seed: Some(1),
            },
            events().0,
        )
        .await
        .map_err(|err| err.to_string())?;
        if !r
            .tables
            .iter()
            .all(|t| t.status == "done" && t.written == 10)
        {
            return Err(format!("{r:#?}"));
        }
        let mut t = open(&tgt_cfg).await?;
        let got = first(exec(t.as_mut(), &read_sql).await?).rows;
        if got.len() != 10 {
            return Err(format!("{got:?}"));
        }
        for (g, w) in got.iter().zip(&want) {
            // Kept whole.
            if f32s(&g[1]) != f32s(&w[1]) {
                return Err(format!("v: {g:?} != {w:?}"));
            }
            // Hashed: a random unit vector of its dimension.
            let h = f32s(&g[2]).ok_or(format!("h: {g:?}"))?;
            let norm: f32 = h.iter().map(|x| x * x).sum::<f32>().sqrt();
            if h.len() != 3 || (norm - 1.0).abs() > 1e-4 || h == [1.0, 0.0, 0.0] {
                return Err(format!("h: {h:?}"));
            }
            // Noised: each number within 10 % of 0.5.
            let n = f32s(&g[3]).ok_or(format!("n: {g:?}"))?;
            if n.len() != 3 || n.iter().any(|x| (x - 0.5).abs() > 0.05 + 1e-6) {
                return Err(format!("n: {n:?}"));
            }
        }
        Ok::<(), String>(())
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|p| {
        Err(p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "panicked".into()))
    });
    let cleaned = drop_dbs().await;
    drop(d);
    body.unwrap();
    cleaned.expect("clean up at the end");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-pgvector"]
async fn vector_live_subset_pgvector_to_pgvector() {
    vector_subset(VecEngine {
        driver: "postgres",
        cfg: crate::commands::vector_live_tests::pgvector,
        schema: Some("public"),
        vector_type: "vector(3)",
        literal: |t| format!("'{t}'"),
        as_text: |c| format!("{c}::text"),
        setup: "CREATE EXTENSION vector",
        drop: |db| format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-mysql9"]
async fn vector_live_subset_mysql9_to_mysql9() {
    vector_subset(VecEngine {
        driver: "mysql",
        cfg: crate::commands::vector_live_tests::mysql9,
        schema: None,
        vector_type: "VECTOR(3)",
        literal: |t| format!("STRING_TO_VECTOR('{t}')"),
        as_text: |c| format!("VECTOR_TO_STRING({c})"),
        setup: "",
        drop: |db| format!("DROP DATABASE IF EXISTS {db}"),
    })
    .await;
}

/// A read the driver marks truncated (valkey-search's KNN maximum, a
/// fetch past its limit) makes the subset say it is partial, with the
/// driver's reason; a whole read says nothing.
#[test]
fn a_truncated_read_says_the_subset_is_partial() {
    let mut r = dbine_driver::StatementResult {
        rows: vec![vec![Value::Null]; 3],
        total_rows: 3,
        ..Default::default()
    };
    assert_eq!(partial_read("docs", &r, &[]), None);
    r.truncated = true;
    let why = "valkey-search devuelve como mucho 10000 documentos por consulta KNN.".to_string();
    let note = partial_read("docs", &r, std::slice::from_ref(&why)).unwrap();
    assert!(
        note.contains("«docs»") && note.contains("parcial") && note.contains(&why),
        "{note}"
    );
    r.total_rows = 5;
    let note = partial_read("docs", &r, &[]).unwrap();
    assert!(note.contains("se leyeron 3 de 5 filas"), "{note}");
}

/// The starting object of an engine without foreign keys, as the engine
/// lists it (a Redis search index: where it searches and its vector index)
/// with the describe's columns: its document key (`id`) first, the listed
/// columns' options kept, and the describe's primary key.
#[test]
fn the_listed_object_keeps_its_definition_and_the_describes_columns() {
    use dbine_driver::{IndexDef, VectorIndex};
    let col = |name: &str, ty: &str| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        ..Default::default()
    };
    let mut emb = col("emb", "VECTOR(3, FLOAT32)");
    emb.options.insert("identifier".into(), "embedding".into());
    let listed = TableSchema {
        kind: "index".into(),
        name: "docs".into(),
        columns: vec![col("title", "TEXT"), emb.clone(), col("extra", "TAG")],
        indexes: vec![IndexDef {
            name: "emb".into(),
            columns: vec!["emb".into()],
            vector: Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        options: [("on", "HASH"), ("prefix", "doc:")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };
    let obj = ObjectRef {
        kind: "index".into(),
        schema: None,
        name: "docs".into(),
    };
    let described = schema_from_columns(
        &obj,
        vec![
            dbine_driver::ColumnInfo {
                name: "id".into(),
                data_type: "key".into(),
                nullable: false,
                primary_key: true,
                auto_increment: false,
                default_value: None,
            },
            dbine_driver::ColumnInfo {
                name: "Title".into(),
                data_type: "TEXT".into(),
                nullable: true,
                primary_key: false,
                auto_increment: false,
                default_value: None,
            },
            dbine_driver::ColumnInfo {
                name: "emb".into(),
                data_type: "VECTOR(3, FLOAT32)".into(),
                nullable: true,
                primary_key: false,
                auto_increment: false,
                default_value: None,
            },
        ],
    );
    let t = with_described(listed.clone(), described.clone());
    assert_eq!(
        t.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "title", "emb", "extra"]
    );
    assert_eq!(t.columns[2], emb);
    assert_eq!(t.options, listed.options);
    assert_eq!(t.indexes, listed.indexes);
    assert_eq!(pk_of(&t), ["id"]);
    // A primary key of its own stays.
    let mut keyed = listed;
    keyed.primary_key = Some(KeyDef {
        name: None,
        columns: vec!["title".into()],
    });
    assert_eq!(pk_of(&with_described(keyed, described)), ["title"]);
}

/// A Redis-family server of the tests, from `env` (`host:port` or
/// `redis://host:port`).
fn redis_cfg(driver: &str, env: &str, default: &str) -> ConnectionConfig {
    let url = std::env::var(env).unwrap_or_else(|_| default.into());
    let rest = url.trim_start_matches("redis://").trim_end_matches('/');
    let (host, port) = rest.rsplit_once(':').unwrap_or((rest, "6379"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        ..Default::default()
    }
}

/// Bytes as a quoted argument of a Redis command (`"\x00\x01"`).
fn redis_blob(v: &[f32]) -> String {
    let hex: String = v
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .map(|b| format!("\\x{b:02x}"))
        .collect();
    format!("\"{hex}\"")
}

/// A subset of a Redis-family search index (database 0) into database 1,
/// which doesn't have it: the index the target creates covers the same
/// keys (its PREFIX, never every hash of the database) with the same
/// fields (the document's key, `id`, isn't one) and vector index. With
/// `run`, the copy runs too and its documents arrive whole; without it
/// (Redis 8 creates indexes in database 0 only) it's planned. Everything
/// it creates, on both databases, goes at the end, passed or not.
async fn redis_index_subset(driver: &'static str, env: &str, default: &str, run_it: bool) {
    use crate::commands::vector_live_tests::{exec, first, open};
    use futures_util::FutureExt;
    let cfg = redis_cfg(driver, env, default);
    let tag = format!("{}_{driver}", std::process::id());
    let name = format!("dbine_subset_ix_{tag}");
    let prefix = format!("dbine:subset:{tag}:");
    let outside = format!("dbine:subset_out:{tag}:1");
    let docs: [(&str, &str, [f32; 3]); 3] = [
        ("1", "uno", [0.1, 0.2, 0.3]),
        ("2", "dos", [0.3, 0.1, 0.2]),
        ("3", "tres", [-0.5, 0.25, 1.0]),
    ];
    let keys: Vec<String> = docs
        .iter()
        .map(|(id, _, _)| format!("{prefix}{id}"))
        .chain([outside.clone()])
        .collect();
    let cleanup = || {
        let (cfg, name, keys) = (cfg.clone(), name.clone(), keys.clone());
        async move {
            for db in ["db0", "db1"] {
                let mut c = cfg.clone();
                c.database = db.into();
                let mut s = open(&c).await?;
                // Not there (already dropped, or never created): fine.
                let _ = exec(s.as_mut(), &format!("FT.DROPINDEX {name}")).await;
                exec(s.as_mut(), &format!("DEL {}", keys.join(" "))).await?;
            }
            Ok::<(), String>(())
        }
    };
    cleanup().await.expect("clean up before");

    let d = dir(&format!("redis-{driver}"));
    let body = std::panic::AssertUnwindSafe(async {
        let state = AppState::new(StateStore::open(&d.join("state.sqlite")).unwrap());
        save(&state, "src", cfg.clone(), vec![]);
        save(&state, "tgt", cfg.clone(), vec![]);
        let mut c0 = cfg.clone();
        c0.database = "db0".into();
        let mut s = open(&c0).await.unwrap();
        exec(
            s.as_mut(),
            &format!("FT.CREATE {name} ON HASH PREFIX 1 {prefix} SCHEMA title TEXT emb VECTOR HNSW 6 TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE"),
        )
        .await
        .unwrap();
        for (id, title, v) in &docs {
            exec(
                s.as_mut(),
                &format!("HSET {prefix}{id} title {title} emb {}", redis_blob(v)),
            )
            .await
            .unwrap();
        }
        // A hash outside the index's prefix: not one of its documents.
        exec(s.as_mut(), &format!("HSET {outside} title fuera")).await.unwrap();
        drop(s);

        let mut a = args(&name, StartFilter::default(), None);
        a.table.kind = "index".into();
        a.database = "db0".into();
        a.target_database = "db1".into();
        let p = plan(&state, &a, events().0).await.unwrap();
        assert_eq!(p.tables.len(), 1, "{p:#?}");
        let t = &p.tables[0];
        assert!(!t.exists && t.error.is_none(), "{t:#?}");
        assert_eq!(t.rows, 3, "{t:#?}");
        let ddl = t.create_ddl.as_deref().unwrap_or_default();
        assert!(
            ddl.starts_with(&format!("FT.CREATE {name} ON HASH PREFIX 1 {prefix} SCHEMA ")),
            "{ddl}"
        );
        // Its vector index as the server reports it (Redis adds its
        // defaults: M, EF_CONSTRUCTION, EF_RUNTIME).
        assert!(
            ddl.contains("emb VECTOR HNSW ")
                && ddl.contains("TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE"),
            "{ddl}"
        );
        assert!(ddl.contains("title TEXT") && !ddl.contains(" id "), "{ddl}");
        // The document's key is copied (it names the documents), not indexed.
        assert!(
            t.columns.iter().any(|c| c.name == "id" && c.skipped.is_none()),
            "{t:#?}"
        );
        if !run_it {
            return;
        }

        let ra = RunArgs {
            subset: a.clone(),
            masks: vec![],
            confirm: String::new(),
            seed: Some(1),
        };
        let r = run(&state, &ra, events().0).await.unwrap();
        assert!(
            r.tables
                .iter()
                .all(|t| t.status == "done" && t.created && t.written == 3),
            "{r:#?}"
        );
        let mut c1 = cfg.clone();
        c1.database = "db1".into();
        let mut s = open(&c1).await.unwrap();
        let ix = s
            .database_schema()
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.name == name)
            .expect("the index in the target");
        assert_eq!(ix.options.get("prefix"), Some(&prefix), "{ix:#?}");
        // Its fields (valkey-search reports them in an order of its own),
        // not the document's key.
        let mut fields: Vec<&str> = ix.columns.iter().map(|c| c.name.as_str()).collect();
        fields.sort();
        assert_eq!(fields, ["emb", "title"]);
        let v = ix.indexes[0].vector.as_ref().expect("its vector index");
        assert_eq!(
            (v.algorithm.as_deref(), v.metric.as_deref()),
            (Some("hnsw"), Some("cosine"))
        );
        // Its documents, whole; the hash outside it stays in the source.
        let obj = ObjectRef {
            kind: "index".into(),
            schema: None,
            name: name.clone(),
        };
        let browse = s.browse_query(&obj, 10);
        let out = first(exec(s.as_mut(), &browse).await.unwrap());
        let at = |c: &str| out.columns.iter().position(|x| x.name == c).unwrap();
        let (id_at, title_at, emb_at) = (at("id"), at("title"), at("emb"));
        let mut got: Vec<(String, String, Vec<f32>)> = out
            .rows
            .iter()
            .map(|r| {
                let nums = dbine_driver::vector::parse_value(&r[emb_at]).expect("a vector");
                (
                    crate::commands::vector_live_tests::text(&r[id_at]),
                    crate::commands::vector_live_tests::text(&r[title_at]),
                    nums.into_iter().map(|x| x as f32).collect(),
                )
            })
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        let want: Vec<(String, String, Vec<f32>)> = docs
            .iter()
            .map(|(id, title, v)| (format!("{prefix}{id}"), title.to_string(), v.to_vec()))
            .collect();
        assert_eq!(got, want);
        let out = first(exec(s.as_mut(), &format!("EXISTS {outside}")).await.unwrap());
        assert_eq!(crate::commands::vector_live_tests::text(&out.rows[0][0]), "0");
    })
    .catch_unwind()
    .await;
    let cleaned = cleanup().await;
    drop(d);
    if let Err(e) = body {
        std::panic::resume_unwind(e);
    }
    cleaned.expect("clean up at the end");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-redis8"]
async fn redis8_index_subset_keeps_its_definition() {
    redis_index_subset("redis", "DBINE_TEST_REDIS8_URL", "127.0.0.1:27203", false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-valkey-bundle"]
async fn valkey_index_subset_keeps_its_definition() {
    redis_index_subset(
        "valkey",
        "DBINE_TEST_VALKEY_SEARCH_URL",
        "127.0.0.1:27204",
        true,
    )
    .await;
}

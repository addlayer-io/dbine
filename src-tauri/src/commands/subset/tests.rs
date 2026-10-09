//! The subset end to end: SQLite → SQLite always (files in a temp dir),
//! PostgreSQL → PostgreSQL against `dbine-test-postgres`
//! (`DBINE_TEST_POSTGRES_URL`, ignored by default).

use super::*;
use dbine_core::{SavedConnection, StateStore};
use dbine_driver::ConnectionConfig;
use mask::FakeKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dbine-subset-{tag}-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
    std::fs::create_dir_all(&d).unwrap();
    d
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
    ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into_owned(), ..Default::default() }
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
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or_else(|| panic!("{v:?} for {text}"))
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
        s.push_str(&format!("INSERT INTO countries VALUES ({i}, 'country {i}');\n"));
    }
    for i in 1..=50 {
        // Each customer referred by the previous one: a chain to follow up.
        let by = if i > 1 { (i - 1).to_string() } else { "NULL".into() };
        s.push_str(&format!("INSERT INTO customers VALUES ({i}, 'Cliente {i}', 'c{i}@real.com', '+54 11 5555-{i:04}', {}, {by});\n", i % 3 + 1));
    }
    for i in 1..=10 {
        s.push_str(&format!("INSERT INTO products VALUES ({i}, 'product {i}', {}.5);\n", i * 10));
    }
    for i in 1..=200 {
        s.push_str(&format!("INSERT INTO orders VALUES ({i}, {}, {}.25, '2024-03-{:02} 10:00:00');\n", i % 50 + 1, i * 3, i % 28 + 1));
        for line in 1..=2 {
            s.push_str(&format!("INSERT INTO order_items VALUES ({i}, {line}, {}, {line});\n", (i + line) % 10 + 1));
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
        table: ObjectRef { kind: "table".into(), schema: None, name: table.into() },
        filter,
        children,
        target_connection_id: "tgt".into(),
        target_database: String::new(),
    }
}

fn mask(table: &str, cols: &[(&str, MaskRule)]) -> TableMask {
    TableMask { schema: None, name: table.into(), columns: cols.iter().map(|(c, r)| (c.to_string(), r.clone())).collect() }
}

async fn setup(tag: &str) -> AppState {
    let d = dir(tag);
    let state = AppState::new(StateStore::open(&d.join("state.sqlite")).unwrap());
    save(&state, "src", sqlite(&d.join("src.db")), vec![]);
    save(&state, "tgt", sqlite(&d.join("tgt.db")), vec![]);
    save(&state, "prod", sqlite(&d.join("prod.db")), vec!["Prod".into()]);
    sql(&state, "src", None, &format!("{SCHEMA}{}", data())).await;
    // The targets exist (the plan opens them read-only).
    sql(&state, "tgt", None, "SELECT 1").await;
    sql(&state, "prod", None, "SELECT 1").await;
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_orders_with_children_parents_and_masking() {
    let state = setup("orders").await;
    let filter = StartFilter { expression: Some("id <= 20".into()), ..Default::default() };
    let a = args("orders", filter, Some(ChildrenOptions { depth: 1, max_rows: 1000 }));

    let (emit, _) = events();
    let p = plan(&state, &a, emit).await.unwrap();
    let rows = |n: &str| p.tables.iter().find(|t| t.name == n).map(|t| t.rows);
    assert_eq!(rows("orders"), Some(20));
    assert_eq!(rows("order_items"), Some(40));
    // Orders 1..20 belong to customers 2..21; each referred by the previous: 1..21.
    assert_eq!(rows("customers"), Some(21));
    assert_eq!(rows("countries"), Some(3));
    assert!(rows("unrelated").is_none() && rows("departments").is_none() && rows("item_notes").is_none(), "item_notes is two levels down");
    let pos = |n: &str| p.tables.iter().position(|t| t.name == n).unwrap();
    assert!(pos("countries") < pos("customers") && pos("customers") < pos("orders") && pos("orders") < pos("order_items") && pos("products") < pos("order_items"));
    assert!(p.tables.iter().all(|t| !t.exists && t.create_ddl.is_some() && t.error.is_none()));
    let cust = &p.tables[pos("customers")];
    let suggested = |c: &str| cust.columns.iter().find(|x| x.name == c).unwrap().suggested.clone();
    assert_eq!(suggested("email"), MaskRule::Fake { kind: FakeKind::Email });
    assert_eq!(suggested("nombre"), MaskRule::Fake { kind: FakeKind::Name });
    assert_eq!(suggested("telefono"), MaskRule::Fake { kind: FakeKind::Phone });
    assert_eq!(suggested("id"), MaskRule::Keep, "keys aren't masked");
    assert!(p.confirm_label.is_none());
    // The plan wrote nothing.
    assert_eq!(one(&state, "tgt", None, "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'").await, 0);

    let run_args = RunArgs {
        subset: a.clone(),
        masks: vec![mask("customers", &[("email", suggested("email")), ("nombre", suggested("nombre")), ("telefono", MaskRule::Null)])],
        confirm: String::new(),
        seed: Some(99),
    };
    let (emit, seen) = events();
    let r = run(&state, &run_args, emit).await.unwrap();
    assert!(!r.cancelled);
    assert!(r.tables.iter().all(|t| t.status == "done" && t.created && t.written == t.rows), "{r:#?}");
    assert!(seen.lock().unwrap().iter().any(|e| e["phase"] == "insert"));

    for (t, n) in [("orders", 20), ("order_items", 40), ("customers", 21), ("countries", 3)] {
        assert_eq!(one(&state, "tgt", None, &format!("SELECT COUNT(*) FROM {t}")).await, n, "{t}");
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
    assert_eq!(one(&state, "tgt", None, "SELECT COUNT(*) FROM customers WHERE telefono IS NOT NULL").await, 0);
    assert_eq!(one(&state, "tgt", None, "SELECT COUNT(*) FROM customers WHERE email LIKE '%@%'").await, 21);
    assert_eq!(one(&state, "tgt", None, "SELECT CAST(total * 100 AS INTEGER) FROM orders WHERE id = 7").await, 2125);
    // The source is untouched.
    assert_eq!(one(&state, "src", None, "SELECT COUNT(*) FROM customers WHERE email LIKE '%@real.com'").await, 50);
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_cycle_composite_key_and_consistent_masks() {
    let state = setup("cycle").await;

    // A cycle (departments ⇄ employees): cut at the nullable manager_id,
    // written NULL and set at the end. The same email masks the same in
    // two tables.
    let a = args("departments", StartFilter::default(), Some(ChildrenOptions { depth: 1, max_rows: 1000 }));
    let (emit, _) = events();
    let p = plan(&state, &a, emit).await.unwrap();
    assert_eq!(p.cycles.len(), 1, "{:?}", p.cycles);
    assert!(p.cycles[0].contains("manager_id"));
    let email = MaskRule::Fake { kind: FakeKind::Email };
    let r = run(&state, &RunArgs { subset: a, masks: vec![mask("employees", &[("email", email.clone())])], confirm: String::new(), seed: Some(5) }, events().0)
        .await
        .unwrap();
    assert!(r.tables.iter().all(|t| t.status == "done"), "{r:#?}");
    assert_eq!(one(&state, "tgt", None, "SELECT COUNT(*) FROM employees").await, 3);
    assert_eq!(one(&state, "tgt", None, "SELECT manager_id FROM departments WHERE id = 2").await, 11, "the cut column is set afterwards");
    let masked = sql(&state, "tgt", None, "SELECT email FROM employees WHERE id = 10").await.results[0].rows[0][0].clone();
    assert_eq!(masked, Masker::with_seed(5).apply(&email, &json!("c5@real.com"), Shape::default()), "deterministic per value");

    // A composite foreign key (item_notes → order_items) fetched by its
    // most varied column and matched whole.
    let a = args("item_notes", StartFilter::default(), None);
    let p = plan(&state, &a, events().0).await.unwrap();
    let rows = |n: &str| p.tables.iter().find(|t| t.name == n).map(|t| t.rows);
    assert_eq!((rows("item_notes"), rows("order_items"), rows("orders")), (Some(2), Some(2), Some(2)));

    // N % and N rows of the start.
    let pct = args("customers", StartFilter { limit: Limit::Percent { percent: 10.0 }, ..Default::default() }, None);
    let p = plan(&state, &pct, events().0).await.unwrap();
    assert!(p.tables.iter().any(|t| t.name == "customers" && t.rows >= 5), "10 % of 50, plus who referred them");
    let n = args("products", StartFilter { limit: Limit::Rows { count: 4 }, ..Default::default() }, None);
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
    let mut ra = RunArgs { subset: a, masks: vec![], confirm: "nope".into(), seed: None };
    assert!(run(&state, &ra, events().0).await.is_err());
    ra.confirm = "prod".into();
    run(&state, &ra, events().0).await.unwrap();
    assert_eq!(one(&state, "prod", None, "SELECT COUNT(*) FROM countries").await, 3);

    // NULL on a NOT NULL column.
    let ra = RunArgs { subset: args("countries", StartFilter::default(), None), masks: vec![mask("countries", &[("name", MaskRule::Null)])], confirm: String::new(), seed: None };
    assert!(run(&state, &ra, events().0).await.is_err());

    // Into an existing table with an extra column: rows go, the column is
    // left to its default.
    sql(&state, "tgt", None, "CREATE TABLE products (id INTEGER PRIMARY KEY, title TEXT, price REAL, stock INTEGER DEFAULT 7)").await;
    let a = args("products", StartFilter { expression: Some("id < 4".into()), ..Default::default() }, None);
    let p = plan(&state, &a, events().0).await.unwrap();
    assert!(p.tables[0].exists && p.tables[0].create_ddl.is_none());
    run(&state, &RunArgs { subset: a, masks: vec![], confirm: String::new(), seed: None }, events().0).await.unwrap();
    assert_eq!(one(&state, "tgt", None, "SELECT SUM(stock) FROM products").await, 21);
}

/// PostgreSQL → PostgreSQL on `dbine-test-postgres`: two databases of the
/// same server, schema `public`, a serial key and the same plan.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_to_postgres() {
    let url = std::env::var("DBINE_TEST_POSTGRES_URL").unwrap_or_else(|_| "postgres://postgres:pw@localhost:25010".into());
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
    state.typed_secrets.insert("pg".into(), [("password".to_string(), pass.to_string())].into_iter().collect());
    let mut admin = dbine_drivers::open_session(&cfg, Some("postgres")).await.unwrap();
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
            SCHEMA.replace("manager_id INTEGER REFERENCES employees(id)", "manager_id INTEGER").replace("price REAL", "price NUMERIC(10,2)"),
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

    let mut a = args("orders", StartFilter { expression: Some("id <= 20".into()), ..Default::default() }, Some(ChildrenOptions { depth: 2, max_rows: 1000 }));
    a.connection_id = "pg".into();
    a.database = "dbine_subset_src".into();
    a.target_connection_id = "pg".into();
    a.target_database = "dbine_subset_tgt".into();
    a.table.schema = Some("public".into());
    let p = plan(&state, &a, events().0).await.unwrap();
    let rows = |n: &str| p.tables.iter().find(|t| t.name == n).map(|t| t.rows);
    assert_eq!((rows("orders"), rows("order_items"), rows("customers"), rows("item_notes")), (Some(20), Some(40), Some(21), Some(2)));
    let r = run(
        &state,
        &RunArgs {
            subset: a,
            masks: vec![mask("customers", &[("email", MaskRule::Fake { kind: FakeKind::Email }), ("nombre", MaskRule::Hash)])],
            confirm: String::new(),
            seed: Some(1),
        },
        events().0,
    )
    .await
    .unwrap();
    assert!(r.tables.iter().all(|t| t.status == "done" && t.written == t.rows), "{r:#?}");
    let tgt = Some("dbine_subset_tgt");
    assert_eq!(one(&state, "pg", tgt, "SELECT COUNT(*) FROM order_items").await, 40);
    assert_eq!(one(&state, "pg", tgt, "SELECT COUNT(*) FROM customers WHERE email LIKE '%@real.com'").await, 0);
    assert_eq!(
        one(&state, "pg", tgt, "SELECT COUNT(*) FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY' AND table_name = 'order_items'").await,
        2,
        "foreign keys created after the data"
    );
    drop(admin);
    let mut admin = dbine_drivers::open_session(&cfg, Some("postgres")).await.unwrap();
    for db in ["dbine_subset_src", "dbine_subset_tgt"] {
        let _ = admin.drop_database(db).await;
    }
}

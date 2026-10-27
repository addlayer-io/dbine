//! Every call of the driver contract crosses the host with the same answer
//! the driver gives in the app: the scripts each engine writes (users and
//! permissions, backups, databases, schemas, renames, schema sync, grid
//! edits…) and a session's catalog and monitoring calls. Each argument has
//! to arrive whole and each answer (or error) come back whole.

use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session, TableSchema};
use dbine_plugin::{DriverMeta, Launcher, RemoteDriver};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;

const EXE: &str = env!("CARGO_BIN_EXE_dbine-plugin-host");

/// The driver `driver_id` of `package`, in the app and through its host.
fn pair(package: &str, driver_id: &str) -> (Arc<dyn Driver>, Arc<dyn Driver>) {
    let (_, drivers) = dbine_drivers::packages()
        .into_iter()
        .find(|(p, _)| *p == package)
        .expect("package");
    let local = drivers
        .into_iter()
        .find(|d| d.info().id == driver_id)
        .expect("driver");
    let meta = DriverMeta::of(package, local.as_ref());
    let launcher = Launcher::at(
        package,
        EXE.into(),
        vec!["--package".into(), package.into()],
        None,
    );
    (local, Arc::new(RemoteDriver::new(meta, launcher)))
}

/// A value of whatever type the call takes, from its JSON form.
fn v<T: DeserializeOwned>(j: Value) -> T {
    serde_json::from_value(j).expect("valid argument")
}

/// Both answers, Ok or Err, the same.
fn same<T: Debug>(what: &str, local: T, remote: T) {
    assert_eq!(
        format!("{local:?}"),
        format!("{remote:?}"),
        "{what} differs through the host"
    );
}

fn table() -> TableSchema {
    v(json!({
        "kind": "table",
        "schema": "ventas",
        "name": "clientes",
        "columns": [
            {"name": "id", "data_type": "integer", "nullable": false, "auto_increment": true},
            {"name": "nombre", "data_type": "varchar(50)", "nullable": false, "default_value": "'x'"},
            {"name": "alta", "data_type": "date", "nullable": true}
        ],
        "primary_key": {"name": "pk_clientes", "columns": ["id"]},
        "indexes": [{"name": "ix_nombre", "columns": ["nombre"], "unique": true}],
        "foreign_keys": [{
            "name": "fk_padre", "columns": ["id"], "ref_schema": "ventas",
            "ref_table": "padres", "ref_columns": ["id"]
        }],
        "checks": [{"name": "ck_nombre", "expression": "nombre <> ''"}],
        "comment": "Los clientes"
    }))
}

fn obj() -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: Some("ventas".into()),
        name: "clientes".into(),
    }
}

/// Every driver-level call, compared.
fn driver_calls(local: &dyn Driver, remote: &dyn Driver) {
    let id = local.info().id;
    let what = |call: &str| format!("{id}: {call}");
    // What the app knows from the manifest.
    same(&what("info"), local.info(), remote.info());
    same(
        &what("capabilities"),
        local.capabilities(),
        remote.capabilities(),
    );
    same(&what("key_search"), local.key_search(), remote.key_search());
    same(&what("designer"), local.designer(), remote.designer());
    same(
        &what("templates"),
        local.create_templates(),
        remote.create_templates(),
    );
    same(&what("security"), local.security(), remote.security());
    same(&what("backup"), local.backup(), remote.backup());
    same(
        &what("schema_spec"),
        local.schema_spec(),
        remote.schema_spec(),
    );
    same(
        &what("rename_spec"),
        local.rename_spec(),
        remote.rename_spec(),
    );
    same(
        &what("create_database_fields"),
        local.create_database_fields(),
        remote.create_database_fields(),
    );
    same(
        &what("script_dialect"),
        local.script_dialect(),
        remote.script_dialect(),
    );
    same(
        &what("script_mode"),
        local.script_mode(),
        remote.script_mode(),
    );
    same(
        &what("script_defaults"),
        local.script_defaults(),
        remote.script_defaults(),
    );
    same(
        &what("write_modes"),
        local.write_modes(),
        remote.write_modes(),
    );
    same(
        &what("flags"),
        (
            local.query_help(),
            local.supports_explain(),
            local.supports_profiler(),
            local.supports_index_usage(),
            local.supports_index_toggle(),
            local.supports_map_login(),
            local.supports_schema_sync(),
            local.supports_manual_transactions(),
            local.supports_bulk_load(),
            local.supports_clone(),
            local.supports_delta(),
            local.script_separator(),
        ),
        (
            remote.query_help(),
            remote.supports_explain(),
            remote.supports_profiler(),
            remote.supports_index_usage(),
            remote.supports_index_toggle(),
            remote.supports_map_login(),
            remote.supports_schema_sync(),
            remote.supports_manual_transactions(),
            remote.supports_bulk_load(),
            remote.supports_clone(),
            remote.supports_delta(),
            remote.script_separator(),
        ),
    );

    // Users, roles and permissions.
    for action in [
        json!({"action": "create_user", "name": "ana", "password": "s3cr'et"}),
        json!({"action": "create_user", "name": "bot", "password": null}),
        json!({"action": "create_role", "name": "lectores"}),
        json!({"action": "drop", "name": "ana", "kind": "user"}),
        json!({"action": "set_password", "name": "ana", "password": "nueva"}),
        json!({"action": "set_login", "name": "ana", "enabled": false}),
        json!({"action": "grant", "privileges": ["SELECT", "INSERT"], "object": {"kind": "table", "schema": "ventas", "name": "clientes"}, "to": "lectores", "grantable": true}),
        json!({"action": "grant", "privileges": ["CONNECT"], "object": null, "to": "ana", "grantable": false}),
        json!({"action": "revoke", "privileges": ["INSERT"], "object": {"kind": "table", "schema": "ventas", "name": "clientes"}, "from": "lectores"}),
        json!({"action": "add_member", "role": "lectores", "member": "ana"}),
        json!({"action": "remove_member", "role": "lectores", "member": "ana"}),
    ] {
        let a = v(action.clone());
        same(
            &what(&format!("security_script {action}")),
            local.security_script(&a),
            remote.security_script(&a),
        );
    }
    same(
        &what("map_login_script"),
        local.map_login_script("ana_login", "ana", Some("ventas")),
        remote.map_login_script("ana_login", "ana", Some("ventas")),
    );

    // Backups.
    for action in [
        json!({"action": "backup", "database": "ventas", "options": {"compression": "true", "copy_only": "true"}}),
        json!({"action": "backup", "database": null, "options": {}}),
        json!({"action": "restore", "source": "/backups/ventas.bak", "database": "ventas_copia", "options": {"replace": "true"}}),
        json!({"action": "delete", "source": "/backups/viejo.bak"}),
    ] {
        let a = v(action.clone());
        same(
            &what(&format!("backup_script {action}")),
            local.backup_script(&a),
            remote.backup_script(&a),
        );
    }

    // Databases and schemas.
    let options: BTreeMap<String, String> = [
        ("encoding".to_string(), "UTF8".to_string()),
        ("collation".to_string(), "Latin1_General_CI_AS".to_string()),
    ]
    .into();
    same(
        &what("create_database_script"),
        local.create_database_script("ventas_2026", &options),
        remote.create_database_script("ventas_2026", &options),
    );
    same(
        &what("alter_database_script"),
        local.alter_database_script("ventas", &options),
        remote.alter_database_script("ventas", &options),
    );
    for db in [None, Some("ventas")] {
        same(
            &what("create_schema_script"),
            local.create_schema_script(db, "reportes", Some("ana")),
            remote.create_schema_script(db, "reportes", Some("ana")),
        );
        same(
            &what("schema_owner_script"),
            local.schema_owner_script(db, "reportes", "bot"),
            remote.schema_owner_script(db, "reportes", "bot"),
        );
        let privileges = vec!["USAGE".to_string(), "CREATE".to_string()];
        same(
            &what("schema_grant_script"),
            local.schema_grant_script(db, "reportes", &privileges, "lectores", true),
            remote.schema_grant_script(db, "reportes", &privileges, "lectores", true),
        );
        same(
            &what("drop_schema_script"),
            local.drop_schema_script(db, "reportes", true),
            remote.drop_schema_script(db, "reportes", true),
        );
    }

    // Structure: DDL, schema sync, renames, index toggles.
    let t = table();
    for parts in [
        json!({"create": true, "indexes": true, "foreign_keys": true}),
        json!({"drop": true, "if_exists": true}),
    ] {
        let p = v(parts.clone());
        same(
            &what(&format!("table_ddl {parts}")),
            local.table_ddl(&t, p),
            remote.table_ddl(&t, p),
        );
    }
    same(
        &what("data_load_wrap"),
        local.data_load_wrap(&t),
        remote.data_load_wrap(&t),
    );
    let mut changed = t.clone();
    changed.columns.pop();
    changed.columns.push(v(
        json!({"name": "baja", "data_type": "date", "nullable": true}),
    ));
    let changes: Vec<dbine_driver::TableChange> = vec![
        v(json!({"op": "create", "table": t})),
        v(json!({"op": "alter", "old": t, "new": changed})),
        v(json!({"op": "drop", "table": t})),
    ];
    same(
        &what("sync_script"),
        local.sync_script(&changes),
        remote.sync_script(&changes),
    );
    for target in [
        json!({"what": "object", "object": {"kind": "table", "schema": "ventas", "name": "clientes"}}),
        json!({"what": "column", "table": {"kind": "table", "schema": "ventas", "name": "clientes"}, "column": "alta"}),
        json!({"what": "index", "table": {"kind": "table", "schema": "ventas", "name": "clientes"}, "index": "ix_nombre"}),
        json!({"what": "constraint", "table": {"kind": "table", "schema": "ventas", "name": "clientes"}, "constraint": "ck_nombre"}),
        json!({"what": "schema", "schema": "ventas"}),
    ] {
        let req = v(json!({"target": target, "new_name": "Nuevo Nombre", "table": t}));
        same(
            &what(&format!("rename_script {target}")),
            local.rename_script(&req),
            remote.rename_script(&req),
        );
    }
    let objects: Vec<dbine_driver::rename::DatabaseObject> = vec![
        v(json!({"kind": "table", "name": "clientes"})),
        v(
            json!({"kind": "view", "name": "v_clientes", "definition": "CREATE VIEW v_clientes AS SELECT 1"}),
        ),
    ];
    same(
        &what("rename_database_script"),
        local.rename_database_script("ventas", "ventas_old", &objects),
        remote.rename_database_script("ventas", "ventas_old", &objects),
    );
    let ix = v(json!({"name": "ix_nombre", "kind": "NONCLUSTERED", "key_columns": ["nombre"]}));
    for enable in [false, true] {
        same(
            &what("index_toggle_script"),
            local.index_toggle_script(&obj(), &ix, enable),
            remote.index_toggle_script(&obj(), &ix, enable),
        );
    }

    // The data grid: browse with filters and sorting, charts, edits.
    let browse = "SELECT * FROM ventas.clientes";
    let filters: Vec<dbine_driver::ColumnFilter> = v(json!([
        {"column": "nombre", "op": "contains", "values": ["an%a"]},
        {"column": "id", "op": "in", "values": [1, 2, 3]},
        {"column": "alta", "op": "is_null"},
        {"column": "id", "op": "sql", "sql": "> 5"}
    ]));
    same(
        &what("filtered_browse"),
        local.filtered_browse(browse, &filters),
        remote.filtered_browse(browse, &filters),
    );
    let sort: Vec<dbine_driver::SortKey> =
        v(json!([{"column": "alta", "descending": true}, {"column": "id"}]));
    same(
        &what("sorted_browse"),
        local.sorted_browse(browse, &sort),
        remote.sorted_browse(browse, &sort),
    );
    let spec = v(
        json!({"x": "alta", "split": "nombre", "measures": [{"func": "count"}, {"column": "id", "func": "max"}]}),
    );
    same(
        &what("aggregate_query"),
        local.aggregate_query(browse, &spec),
        remote.aggregate_query(browse, &spec),
    );
    let rows = vec![
        vec![json!(1), json!("O'Brien"), Value::Null],
        vec![json!(2), json!("Zoë"), json!("2026-10-26")],
    ];
    let columns = vec!["id".to_string(), "nombre".to_string(), "alta".to_string()];
    same(
        &what("insert_script"),
        local.insert_script(&obj(), &columns, &rows),
        remote.insert_script(&obj(), &columns, &rows),
    );
    let edits: Vec<dbine_driver::RowChange> = v(json!([
        {"key": [["id", 1]], "set": [["nombre", "Ana"], ["alta", null]]},
        {"key": [["id", 2]], "set": [["nombre", "it's"]], "row": [["id", 2], ["nombre", "x"], ["alta", null]]}
    ]));
    same(
        &what("update_script"),
        local.update_script(&obj(), &edits),
        remote.update_script(&obj(), &edits),
    );
    let keys = vec![
        vec![("id".to_string(), json!(7))],
        vec![("id".to_string(), json!(8))],
    ];
    same(
        &what("delete_script"),
        local.delete_script(&obj(), &keys),
        remote.delete_script(&obj(), &keys),
    );
    let sql = "SELECT 1;\nSELECT 'a;b';\n";
    same(
        &what("split_script"),
        local.split_script(sql),
        remote.split_script(sql),
    );
}

#[test]
fn sql_server_scripts_cross_the_host() {
    let (local, remote) = pair("sqlserver", "sqlserver");
    driver_calls(local.as_ref(), remote.as_ref());
    // Not only matching refusals: real scripts came back.
    let create = remote
        .security_script(&v(
            json!({"action": "create_user", "name": "ana", "password": "pw"}),
        ))
        .unwrap();
    assert!(create.contains("[ana]"), "{create}");
    let backup = remote
        .backup_script(&v(
            json!({"action": "backup", "database": "ventas", "options": {}}),
        ))
        .unwrap();
    assert!(backup.contains("BACKUP DATABASE"), "{backup}");
    assert!(remote
        .update_script(
            &obj(),
            &v::<Vec<dbine_driver::RowChange>>(
                json!([{"key": [["id", 1]], "set": [["nombre", "x"]]}])
            )
        )
        .unwrap()
        .starts_with("UPDATE"));
}

#[test]
fn postgres_scripts_cross_the_host() {
    let (local, remote) = pair("postgres", "postgres");
    driver_calls(local.as_ref(), remote.as_ref());
    let schema = remote
        .create_schema_script(None, "reportes", Some("ana"))
        .unwrap();
    assert!(schema.contains("CREATE SCHEMA"), "{schema}");
    let sync = remote
        .sync_script(&[v(json!({"op": "create", "table": table()}))])
        .unwrap();
    assert!(
        sync.statements.iter().any(|s| s.contains("CREATE TABLE")),
        "{sync:?}"
    );
}

#[test]
fn mysql_scripts_cross_the_host() {
    let (local, remote) = pair("mysql", "mysql");
    driver_calls(local.as_ref(), remote.as_ref());
}

#[test]
fn oracle_scripts_cross_the_host() {
    let (local, remote) = pair("oracle", "oracle");
    driver_calls(local.as_ref(), remote.as_ref());
}

#[test]
fn redis_scripts_cross_the_host() {
    // A key-value store: most answers are refusals, which must come back
    // the same too.
    let (local, remote) = pair("redis", "redis");
    driver_calls(local.as_ref(), remote.as_ref());
}

/// A session's answer, Ok or Err, as text.
fn text<T: Debug>(r: dbine_driver::Result<T>) -> String {
    format!("{r:?}")
}

/// The catalog, monitoring and administration calls of a session.
async fn session_calls(a: &mut Box<dyn Session>, b: &mut Box<dyn Session>) {
    let t = ObjectRef {
        kind: "table".into(),
        schema: None,
        name: "clientes".into(),
    };
    macro_rules! both {
        ($what:literal, $call:ident ( $($arg:expr),* )) => {{
            let x = text(a.$call($($arg),*).await);
            let y = text(b.$call($($arg),*).await);
            assert_eq!(x, y, "{} differs through the host", $what);
        }};
    }
    both!("list_schemas", list_schemas());
    both!("list_databases", list_databases());
    both!("definition", definition(&t));
    both!("transaction_state", transaction_state());
    both!("create_database_choices", create_database_choices());
    both!("database_properties", database_properties("main"));
    both!("blocking", blocking());
    both!("principals", principals());
    both!("unmapped_logins", unmapped_logins());
    both!("grants", grants("ana"));
    both!("processes", processes());
    both!("kill_session", kill_session("42"));
    both!("cancel_query", cancel_query("42"));
    both!("backups", backups(Some("main")));
    both!("permissions", permissions(Some("main")));
    both!("health_checks", health_checks("main"));
    both!("row_estimates", row_estimates());
    both!("object_comments", object_comments());
    both!("index_usage", index_usage(&t));
    both!("key_range", key_range(&t, "id"));
    both!("profiler_poll", profiler_poll());
    both!("profiler_stop", profiler_stop());
    let opts = v(json!({"database": "main", "change_server": false}));
    both!("profiler_start", profiler_start(&opts));
    let scan = v(json!({"pattern": "cli*", "count": 10}));
    both!("scan_keys", scan_keys(&scan));
    let search = v(json!({"text": "clientes", "max_hits": 10}));
    both!("search_code", search_code(&search));
    let target = v(json!({"object": {"kind": "table", "schema": null, "name": "clientes"}}));
    let deps = v(
        json!({"source_kinds": ["view", "trigger"], "dialect": a_dialect(), "foreign_keys": true}),
    );
    both!("dependents", dependents(&target, &deps));
    let changes: BTreeMap<String, String> = [("x".to_string(), "y".to_string())].into();
    both!("alter_database", alter_database("main", &changes));
    both!(
        "create_database_with",
        create_database_with("otra", &changes)
    );
    let (mut oa, mut ob) = (QueryOutcome::default(), QueryOutcome::default());
    let x = text(
        a.explain("SELECT * FROM clientes WHERE id = 1", false, 50, &mut oa)
            .await,
    );
    let y = text(
        b.explain("SELECT * FROM clientes WHERE id = 1", false, 50, &mut ob)
            .await,
    );
    assert_eq!(x, y, "explain differs through the host");
    assert_eq!(
        oa.results.iter().map(|r| &r.rows).collect::<Vec<_>>(),
        ob.results.iter().map(|r| &r.rows).collect::<Vec<_>>()
    );
    // Manual transactions: the same answers, in the same order.
    for on in [false, true] {
        let x = text(a.set_autocommit(on).await);
        let y = text(b.set_autocommit(on).await);
        assert_eq!(x, y, "set_autocommit({on})");
    }
    both!("commit", commit());
    both!("rollback", rollback());
}

/// SQLite's script dialect, as the dependency scan takes it.
fn a_dialect() -> Value {
    let sqlite = dbine_drivers::find("sqlite").expect("sqlite driver");
    serde_json::to_value(sqlite.script_dialect()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_session_calls_cross_the_host() {
    let (local, remote) = pair("sqlite", "sqlite");
    let dir = tempfile::tempdir().unwrap();
    let cfg = ConnectionConfig {
        driver: "sqlite".into(),
        host: dir.path().join("s.db").display().to_string(),
        ..Default::default()
    };
    let mut setup = local.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    setup
        .execute(
            "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre TEXT NOT NULL);
             INSERT INTO clientes (nombre) VALUES ('Ana'), ('Beto');
             CREATE VIEW v_clientes AS SELECT nombre FROM clientes;",
            10,
            &mut out,
        )
        .await
        .unwrap();
    drop(setup);
    let mut a = local.connect(&cfg, None).await.unwrap();
    let mut b = remote.connect(&cfg, None).await.unwrap();
    session_calls(&mut a, &mut b).await;
}

//! "Nuevo esquema…" / "Borrar esquema…" (scopes) against a real Couchbase
//! Server, provisioned as in tests/integration.rs:
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test schemas -- --ignored --nocapture
//! ```
//!
//! Community Edition has no scope roles: there the grant must be refused by
//! the server (an Enterprise server takes it, and the role shows up on the
//! scope in `system:user_info`).

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, SecurityAction, Session};
use serde_json::Value;

const USER: &str = "Administrator";
const PASS: &str = "secreto1";
const BUCKET: &str = "dbine_schemas";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn scopes(c: &ConnectionConfig) -> Vec<String> {
    let url = format!("http://{}:{}/pools/default/buckets/{BUCKET}/scopes", c.host, c.options["mgmt_port"]);
    let v: Value = reqwest::Client::new().get(url).basic_auth(USER, Some(PASS)).send().await.unwrap().json().await.unwrap();
    v["scopes"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
#[ignore]
async fn create_grant_and_drop_scopes() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchbase::drivers().remove(0);
    let spec = d.schema_spec().unwrap();
    assert!(!spec.owner && spec.cascade && spec.privileges.contains(&"query_select"));
    let mut admin = d.connect(&c, None).await.unwrap();
    let _ = admin.drop_database(BUCKET).await;
    admin.create_database(BUCKET).await.unwrap();
    let mut s = d.connect(&c, Some(BUCKET)).await.unwrap();
    let _ = run(&mut s, "DROP USER dbine_scope_u").await;
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_scope_u".into(), password: Some("Pw_12345x".into()) })).await.unwrap();

    let full = format!("{BUCKET}.ventas");
    // The dialog's name with its bucket, or bare with the menu's bucket: the same scope.
    assert_eq!(d.create_schema_script(Some(BUCKET), "ventas", None).unwrap(), d.create_schema_script(None, &full, None).unwrap());
    run(&mut s, &d.create_schema_script(Some(BUCKET), "ventas", None).unwrap()).await.unwrap();
    assert!(scopes(&c).await.contains(&"ventas".to_string()));
    // Empty, it's in the tree already (and can be dropped from there).
    let listed = s.list_schemas().await.unwrap().expect("scopes listed");
    assert!(listed.iter().any(|x| x.name == full) && listed.iter().any(|x| x.name == format!("{BUCKET}._default")), "{listed:?}");
    assert!(listed.iter().all(|x| x.system == x.name.contains("._system")), "{listed:?}");
    // Listed as the tree shows it once it holds a collection.
    run(&mut s, &format!("CREATE COLLECTION default:`{BUCKET}`.`ventas`.`pedidos`")).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::COLLECTION && o.schema.as_deref() == Some(full.as_str()) && o.name == "pedidos"), "{objs:?}");

    let grant = d.schema_grant_script(Some(BUCKET), "ventas", &["query_select".into(), "data_reader".into()], "dbine_scope_u", false).unwrap();
    assert_eq!(
        grant,
        script(SecurityAction::Grant {
            privileges: vec!["query_select".into(), "data_reader".into()],
            object: Some(ObjectRef { kind: "schema".into(), schema: None, name: full.clone() }),
            to: "dbine_scope_u".into(),
            grantable: false,
        })
    );
    let enterprise = run(&mut s, &grant).await;
    let info = run(&mut s, "SELECT RAW u.`roles` FROM system:user_info u WHERE u.id = 'dbine_scope_u'").await.unwrap();
    let roles = info.results[0].rows.first().map(|r| r[0].to_string()).unwrap_or_default();
    let ee = enterprise.is_ok();
    match enterprise {
        Ok(_) => assert!(roles.contains("query_select") && roles.contains("\"scope_name\":\"ventas\""), "{roles}"),
        Err(e) => {
            eprintln!("Community Edition refuses the scope roles: {e}");
            assert!(e.to_string().contains("not valid"), "{e}");
        }
    }
    let grants = s.grants("dbine_scope_u").await.unwrap();
    if ee {
        assert!(grants.iter().any(|g| g.object.as_deref() == Some(full.as_str()) && g.object_kind.as_deref() == Some("schema")));
    }

    // Without "con su contenido" nothing is written; with it the scope and its collection go.
    assert!(d.drop_schema_script(Some(BUCKET), "ventas", false).is_err());
    run(&mut s, &d.drop_schema_script(Some(BUCKET), "ventas", true).unwrap()).await.unwrap();
    assert!(!scopes(&c).await.contains(&"ventas".to_string()));
    assert!(!s.list_schemas().await.unwrap().unwrap().iter().any(|x| x.name == full));

    let _ = run(&mut s, "DROP USER dbine_scope_u").await;
    drop(s);
    admin.drop_database(BUCKET).await.unwrap();
}

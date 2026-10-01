//! Users and permissions against a real Dremio OSS (see tests/integration.rs;
//! the first user must exist, which that test creates):
//! `DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio --test security -- --ignored`
//!
//! OSS has no roles or privileges: its users are listed as administrators,
//! grants answer "unsupported" and the server refuses the scripts.

use dbine_driver::{ConnectionConfig, Error, PrincipalKind, QueryOutcome, SecurityAction};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(std::env::var("DBINE_TEST_DREMIO_USER").unwrap_or_else(|_| "dbine".into())),
        password: Some(std::env::var("DBINE_TEST_DREMIO_PASSWORD").unwrap_or_else(|_| "secreto123".into())),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn community_edition() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_dremio::drivers().remove(0);
    assert!(d.security().is_some());
    let mut s = d.connect(&c, None).await.unwrap();
    let all = s.principals().await.unwrap();
    let me = all.iter().find(|p| Some(&p.name) == c.username.as_ref()).unwrap_or_else(|| panic!("{all:?}"));
    assert_eq!((me.kind, me.superuser), (PrincipalKind::User, Some(true)));
    assert!(matches!(s.grants(&me.name).await, Err(Error::Unsupported(_))));

    let sql = d.security_script(&SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345x".into()) }).unwrap();
    let mut out = QueryOutcome::default();
    let e = s.execute(&sql, 10, &mut out).await.expect_err("OSS refuses CREATE USER");
    assert!(e.to_string().contains("Enterprise"), "{e}");
}

/// Folders on Dremio OSS: the server parses `CREATE FOLDER` / `DROP FOLDER`
/// but spaces and `$scratch` don't take them (only catalog sources such as
/// Nessie do, which the test image lacks), so the refusal must be the
/// source's, not a syntax error.
#[tokio::test]
#[ignore]
async fn folder_scripts_parse() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_dremio::drivers().remove(0);
    let spec = d.schema_spec().unwrap();
    assert!(!spec.owner && !spec.cascade);
    assert!(d.create_schema_script(None, "ventas", None).is_err(), "a folder needs its whole path");
    let mut s = d.connect(&c, None).await.unwrap();
    let mut err = async |sql: String| {
        let mut out = QueryOutcome::default();
        match s.execute(&sql, 10, &mut out).await {
            Err(e) => e.to_string(),
            Ok(()) => out.error.clone().unwrap_or_else(|| panic!("{sql} ran")),
        }
    };
    let e = err(d.create_schema_script(None, "$scratch.dbine_f", None).unwrap()).await;
    assert!(e.contains("Create folder is not supported for this source"), "{e}");
    let e = err(d.drop_schema_script(None, "$scratch.dbine_f", false).unwrap()).await;
    assert!(e.contains("not found"), "{e}");
}

/// A space's folder through "Borrar esquema…" with the menu's space as
/// `database`: Dremio OSS makes space folders only in the UI or the REST
/// API (CREATE FOLDER is refused there), so the test makes an empty one
/// that way, checks that `list_schemas` shows it while empty (and the space
/// itself), and drops it with the script DBine writes.
#[tokio::test]
#[ignore]
async fn space_folders_listed_and_dropped() {
    let Some(c) = cfg() else { return };
    let base = format!("http://{}:{}", c.host, c.port);
    let http = reqwest::Client::new();
    let login: serde_json::Value = http
        .post(format!("{base}/apiv2/login"))
        .json(&serde_json::json!({"userName": c.username, "password": c.password}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let auth = format!("_dremio{}", login["token"].as_str().unwrap());
    let post = async |body: serde_json::Value| {
        let _ = http.post(format!("{base}/api/v3/catalog")).header("Authorization", &auth).json(&body).send().await;
    };
    post(serde_json::json!({"entityType": "space", "name": "dbine_sch"})).await;
    post(serde_json::json!({"entityType": "folder", "path": ["dbine_sch", "vacia"]})).await;

    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, Some("dbine_sch")).await.unwrap();
    let listed = s.list_schemas().await.unwrap().expect("Dremio lists folders");
    assert!(listed.iter().any(|x| x.name == "dbine_sch"), "{listed:?}");
    assert!(listed.iter().any(|x| x.name == "dbine_sch.vacia" && !x.system), "{listed:?}");

    let drop = d.drop_schema_script(Some("dbine_sch"), "dbine_sch.vacia", false).unwrap();
    assert_eq!(drop, "DROP FOLDER \"dbine_sch\".\"vacia\";");
    let mut out = QueryOutcome::default();
    s.execute(&drop, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    let listed = s.list_schemas().await.unwrap().unwrap();
    assert!(!listed.iter().any(|x| x.name == "dbine_sch.vacia"), "{listed:?}");
    // CREATE FOLDER in a space: the space's refusal, with the path built
    // from the menu's space.
    let create = d.create_schema_script(Some("dbine_sch"), "otra", None).unwrap();
    assert_eq!(create, "CREATE FOLDER \"dbine_sch\".\"otra\";");
    let mut out = QueryOutcome::default();
    let e = match s.execute(&create, 10, &mut out).await {
        Err(e) => e.to_string(),
        Ok(()) => out.error.clone().unwrap_or_default(),
    };
    assert!(e.contains("Create folder is not supported"), "{e}");
}

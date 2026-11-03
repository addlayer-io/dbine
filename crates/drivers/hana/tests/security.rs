//! `Session::privileges` against a real SAP HANA (there's no practical
//! Docker image), skipped without one:
//!
//! ```sh
//! DBINE_TEST_HANA_URL=hana://USER:PASSWORD@host:39041 \
//!   cargo test -p dbine-driver-hana --test security -- --ignored
//! ```
//! Add `?tls` to the URL for an encrypted connection (HANA Cloud: port 443).

use dbine_driver::ConnectionConfig;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_HANA_URL").ok()?;
    let rest = url
        .strip_prefix("hana://")
        .expect("hana://user:pass@host:port");
    let (rest, tls) = match rest.split_once('?') {
        Some((r, q)) => (r, q.contains("tls")),
        None => (rest, false),
    };
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (host, port) = addr.split_once(':').unwrap();
    Some(ConnectionConfig {
        driver: "hana".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        encrypt: tls,
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn lists_what_it_can_grant() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_HANA_URL not set; skipping");
        return;
    };
    let d = dbine_driver_hana::drivers().remove(0);
    let kinds = d.security().unwrap().object_kinds;
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let list = s.privileges().await.unwrap();
    eprintln!("{list:#?}");
    let on = |n: &str| {
        list.iter()
            .find(|p| p.name == n)
            .map(|p| p.on.iter().map(String::as_str).collect::<Vec<_>>())
    };
    assert_eq!(on("CATALOG READ"), Some(vec![""]));
    assert_eq!(on("CREATE SCHEMA"), Some(vec![""]));
    assert!(on("SELECT").unwrap().contains(&"table"));
    assert!(on("EXECUTE").unwrap().contains(&"procedure"));
    for p in &list {
        assert!(!p.on.is_empty(), "{}", p.name);
        assert!(
            p.on.iter().all(|k| kinds.contains(&k.as_str())),
            "{}: {:?}",
            p.name,
            p.on
        );
        let object =
            p.on.iter()
                .find(|k| !k.is_empty())
                .map(|k| dbine_driver::ObjectRef {
                    kind: k.clone(),
                    schema: None,
                    name: "X".into(),
                });
        d.security_script(&dbine_driver::SecurityAction::Grant {
            privileges: vec![p.name.clone()],
            object,
            to: "X".into(),
            grantable: false,
        })
        .unwrap_or_else(|e| panic!("{}: {e}", p.name));
    }
}

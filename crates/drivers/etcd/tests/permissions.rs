//! What the login may do, against a throwaway etcd (the test turns auth on
//! and off again):
//!
//! ```sh
//! docker run -d --name dbine-test-etcd-perm -p 27114:2379 quay.io/coreos/etcd:v3.5.17 \
//!   etcd --advertise-client-urls http://0.0.0.0:2379 --listen-client-urls http://0.0.0.0:2379
//! DBINE_TEST_ETCD_PERM_URL=http://localhost:27114 cargo test -p dbine-driver-etcd --test permissions -- --ignored
//! ```

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

fn cfg(user: Option<(&str, &str)>) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_ETCD_PERM_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "etcd".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: user.map(|u| u.0.into()),
        password: user.map(|u| u.1.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    s.execute(text, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

#[tokio::test]
#[ignore]
async fn root_role_decides() {
    let Some(c) = cfg(None) else { return };
    let d = dbine_driver_etcd::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();

    // Auth off: everything the driver offers is allowed.
    let p = s.permissions(None).await.unwrap();
    assert_eq!((p.backup, p.manage_security, p.restore), (Access::Allowed, Access::Allowed, Access::Unknown));

    for cleanup in ["user delete dbine_lim", "user delete root", "role delete root"] {
        let _ = s.execute(cleanup, 10, &mut QueryOutcome::default()).await;
    }
    for cmd in ["role add root", "user add root raizpw", "user grant-role root root", "user add dbine_lim limpw", "auth enable"] {
        run(&mut s, cmd).await;
    }

    let d2 = d.clone();
    let result = async move {
        let d = d2;
        let mut lim = d.connect(&cfg(Some(("dbine_lim", "limpw"))).unwrap(), None).await.unwrap();
        let p = lim.permissions(None).await.unwrap();
        assert_eq!(p.backup, Access::Denied { missing: "rol root".into() });
        assert_eq!(p.manage_security, Access::Denied { missing: "rol root".into() });

        let mut root = d.connect(&cfg(Some(("root", "raizpw"))).unwrap(), None).await.unwrap();
        let p = root.permissions(None).await.unwrap();
        assert_eq!((p.backup, p.manage_security), (Access::Allowed, Access::Allowed));
    };
    let outcome = tokio::spawn(result).await;

    let mut root = d.connect(&cfg(Some(("root", "raizpw"))).unwrap(), None).await.unwrap();
    for cmd in ["auth disable", "user delete dbine_lim", "user delete root", "role delete root"] {
        run(&mut root, cmd).await;
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

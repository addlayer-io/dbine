//! `Session::permissions` against DynamoDB Local: IAM can't be asked
//! (unknown), and DBine's read-only mode doesn't change that (it blocks the
//! backup statements on its own).
//!
//! ```sh
//! DBINE_TEST_DYNAMODB_URL=http://localhost:25300 \
//!   cargo test -p dbine-driver-dynamodb --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Permissions, QueryOutcome};

fn cfg(read_only: bool) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_DYNAMODB_URL").ok()?;
    let mut c = ConnectionConfig { driver: "dynamodb".into(), read_only, ..Default::default() };
    for (k, v) in [
        ("region", "us-east-1"),
        ("auth_mode", "keys"),
        ("access_key_id", "dummy"),
        ("secret_access_key", "dummy"),
        ("endpoint_url", url.as_str()),
    ] {
        c.options.insert(k.into(), v.into());
    }
    Some(c)
}

#[tokio::test]
#[ignore]
async fn iam_and_read_only() {
    let Some(c) = cfg(false) else {
        eprintln!("DBINE_TEST_DYNAMODB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_dynamodb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    let p = s.permissions(None).await.unwrap();
    eprintln!("credentials: {p:?}");
    assert_eq!(p, Permissions::default());
    let mut s = d.connect(&cfg(true).unwrap(), None).await.expect("connect read-only");
    let p = s.permissions(None).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    assert_eq!(p, Permissions::default());
    // The read-only mode still blocks the backup.
    let mut out = QueryOutcome::default();
    assert!(s.execute("CREATE BACKUP \"b\" FOR TABLE \"t\"", 10, &mut out).await.is_err());
}

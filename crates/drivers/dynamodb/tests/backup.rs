//! Native backups against DynamoDB Local, which doesn't implement them: the
//! history and the scripts must say so instead of failing obscurely.
//!   DBINE_TEST_DYNAMODB_URL=http://localhost:25300 cargo test -p dbine-driver-dynamodb --test backup -- --ignored

use dbine_driver::{BackupAction, Error, QueryOutcome};
use std::collections::BTreeMap;

#[tokio::test]
#[ignore]
async fn local_endpoint_has_no_backups() {
    let Ok(url) = std::env::var("DBINE_TEST_DYNAMODB_URL") else { return };
    let mut c = dbine_driver::ConnectionConfig { driver: "dynamodb".into(), ..Default::default() };
    for (k, v) in
        [("region", "us-east-1"), ("auth_mode", "keys"), ("access_key_id", "dummy"), ("secret_access_key", "dummy"), ("endpoint_url", &url)]
    {
        c.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_dynamodb::drivers().pop().unwrap();
    let spec = d.backup().unwrap();
    assert!(spec.history && spec.restore && spec.delete && spec.server_wide);
    let mut s = d.connect(&c, None).await.unwrap();

    let e = s.backups(None).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");

    let options = BTreeMap::from([("table".to_string(), "dbine_bk".to_string()), ("name".to_string(), "b1".to_string())]);
    let script = d.backup_script(&BackupAction::Backup { database: None, options }).unwrap();
    let mut out = QueryOutcome::default();
    let e = s.execute(&script, 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
}

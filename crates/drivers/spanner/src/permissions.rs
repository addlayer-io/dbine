//! What the login may do (`Session::permissions`) in Cloud Spanner, from
//! IAM's own permission test (`testIamPermissions`) on the instance and on
//! the database, both at once. Each answers the effective permissions
//! there, inherited ones included. A test that fails leaves its actions
//! `Unknown`; against the emulator (no IAM) nothing is tested.

use crate::{Json, SpannerSession};
use dbine_driver::{Access, Permissions};
use serde_json::json;
use std::collections::BTreeSet;
use std::time::Duration;

pub const DATABASES_CREATE: &str = "spanner.databases.create";
pub const BACKUPS_CREATE: &str = "spanner.backups.create";
pub const BACKUPS_RESTORE: &str = "spanner.backups.restoreDatabase";
pub const DATABASES_CREATE_BACKUP: &str = "spanner.databases.createBackup";
pub const DATABASES_DROP: &str = "spanner.databases.drop";
pub const DATABASES_UPDATE_DDL: &str = "spanner.databases.updateDdl";
pub const DATABASES_SELECT: &str = "spanner.databases.select";

/// Tested on the instance.
pub const INSTANCE: &[&str] = &[DATABASES_CREATE, BACKUPS_CREATE, BACKUPS_RESTORE];
/// Tested on the database.
pub const DATABASE: &[&str] = &[DATABASES_CREATE_BACKUP, DATABASES_DROP, DATABASES_UPDATE_DDL, DATABASES_SELECT];

const LIMIT: Duration = Duration::from_secs(20);

pub fn request(perms: &[&str]) -> Json {
    json!({ "permissions": perms })
}

/// The permissions a `testIamPermissions` response grants (the key is left
/// out when none is).
pub fn granted(resp: &Json) -> BTreeSet<String> {
    resp.get("permissions")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect()
}

/// An action that needs every one of `perms`, each tested on a resource
/// (`None`: that test failed): denied by the first one missing, allowed
/// when all were tested and granted.
fn need(perms: &[(Option<&BTreeSet<String>>, &str)]) -> Access {
    if let Some((_, p)) = perms.iter().find(|(set, p)| set.is_some_and(|s| !s.contains(*p))) {
        return Access::Denied { missing: (*p).into() };
    }
    if perms.iter().all(|(set, _)| set.is_some()) { Access::Allowed } else { Access::Unknown }
}

pub fn map(instance: Option<&BTreeSet<String>>, database: Option<&BTreeSet<String>>) -> Permissions {
    Permissions {
        backup: need(&[(instance, BACKUPS_CREATE), (database, DATABASES_CREATE_BACKUP)]),
        restore: need(&[(instance, BACKUPS_RESTORE), (instance, DATABASES_CREATE)]),
        // SPANNER_SYS is readable with plain read access; a login under
        // fine-grained access control reads it through spanner_sys_reader,
        // which IAM doesn't tell: only an allowed answer counts.
        profiler: match need(&[(database, DATABASES_SELECT)]) {
            Access::Denied { .. } => Access::Unknown,
            a => a,
        },
        create_database: need(&[(instance, DATABASES_CREATE)]),
        drop_database: need(&[(database, DATABASES_DROP)]),
        // Roles and grants are DDL.
        manage_security: need(&[(database, DATABASES_UPDATE_DDL)]),
        ..Default::default()
    }
}

impl SpannerSession {
    async fn test_iam(&self, resource: &str, perms: &[&str]) -> Option<BTreeSet<String>> {
        match self.api.post(&format!("{resource}:testIamPermissions"), &request(perms)).await {
            Ok(r) => Some(granted(&r)),
            Err(e) => {
                tracing::debug!("spanner testIamPermissions on {resource}: {e}");
                None
            }
        }
    }
}

pub(crate) async fn check(s: &SpannerSession, database: Option<&str>) -> Permissions {
    if !s.api.base.starts_with(crate::API) {
        return Permissions::default();
    }
    let db = match database.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!("{}/databases/{d}", s.instance),
        None => s.database.clone(),
    };
    let tests = async { tokio::join!(s.test_iam(&s.instance, INSTANCE), s.test_iam(&db, DATABASE)) };
    match tokio::time::timeout(LIMIT, tests).await {
        Ok((instance, database)) => map(instance.as_ref(), database.as_ref()),
        Err(_) => Permissions::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(perms: &[&str]) -> BTreeSet<String> {
        perms.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn request_and_response() {
        assert_eq!(request(INSTANCE)["permissions"][0], json!("spanner.databases.create"));
        assert_eq!(granted(&json!({ "permissions": [DATABASES_DROP] })), set(&[DATABASES_DROP]));
        assert!(granted(&json!({})).is_empty());
    }

    #[test]
    fn everything_granted() {
        let p = map(Some(&set(INSTANCE)), Some(&set(DATABASE)));
        for a in [&p.backup, &p.restore, &p.profiler, &p.create_database, &p.drop_database, &p.manage_security] {
            assert_eq!(*a, Access::Allowed);
        }
        assert_eq!(p.kill_session, Access::Unknown);
    }

    #[test]
    fn a_reader_is_denied_what_it_lacks() {
        let p = map(Some(&set(&[])), Some(&set(&[DATABASES_SELECT])));
        assert_eq!(p.backup, Access::Denied { missing: BACKUPS_CREATE.into() });
        assert_eq!(p.restore, Access::Denied { missing: BACKUPS_RESTORE.into() });
        assert_eq!(p.create_database, Access::Denied { missing: DATABASES_CREATE.into() });
        assert_eq!(p.drop_database, Access::Denied { missing: DATABASES_DROP.into() });
        assert_eq!(p.manage_security, Access::Denied { missing: DATABASES_UPDATE_DDL.into() });
        assert_eq!(p.profiler, Access::Allowed);
    }

    #[test]
    fn a_failed_test_leaves_its_actions_unknown() {
        let p = map(None, Some(&set(&[DATABASES_CREATE_BACKUP])));
        assert_eq!(p.backup, Access::Unknown);
        assert_eq!(p.create_database, Access::Unknown);
        assert_eq!(p.drop_database, Access::Denied { missing: DATABASES_DROP.into() });
        // The other side is still enough to deny.
        let p = map(Some(&set(INSTANCE)), Some(&set(&[])));
        assert_eq!(p.backup, Access::Denied { missing: DATABASES_CREATE_BACKUP.into() });
        assert_eq!(p.profiler, Access::Unknown);
        assert_eq!(map(None, None), Permissions::default());
    }
}

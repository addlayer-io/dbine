//! What the login may do (`Session::permissions`) in Databricks. Unity
//! Catalog has no permission test for the caller: privileges come through
//! account groups (nested too) and through the metastore admin, neither of
//! which the caller can resolve for itself, and a SQL check would wake the
//! warehouse. So only the profiler is checked, through SCIM `Me` (no SQL):
//! a workspace admin (member of `admins`) sees everyone's queries. Without
//! that the profiler still shows the login's own, so it's never denied.

use crate::DatabricksSession;
use dbine_driver::{Access, Permissions};
use serde_json::Value as Json;
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(20);

/// Whether SCIM `Me` lists the workspace's `admins` group.
pub fn is_admin(me: &Json) -> bool {
    me.get("groups").and_then(Json::as_array).into_iter().flatten().any(|g| g.get("display").and_then(Json::as_str) == Some("admins"))
}

pub fn map(admin: bool) -> Permissions {
    Permissions { profiler: if admin { Access::Allowed } else { Access::Unknown }, ..Default::default() }
}

pub(crate) async fn check(s: &DatabricksSession) -> Permissions {
    match tokio::time::timeout(LIMIT, s.api.get("/api/2.0/preview/scim/v2/Me")).await {
        Ok(Ok(me)) => map(is_admin(&me)),
        Ok(Err(e)) => {
            tracing::debug!("databricks permissions (SCIM Me): {e}");
            Permissions::default()
        }
        Err(_) => Permissions::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admins_from_scim() {
        assert!(is_admin(&json!({ "userName": "ana@x.com", "groups": [{ "display": "users" }, { "display": "admins" }] })));
        assert!(!is_admin(&json!({ "userName": "ana@x.com", "groups": [{ "display": "users" }] })));
        assert!(!is_admin(&json!({ "userName": "ana@x.com" })));
    }

    #[test]
    fn only_the_profiler_is_proved_and_never_denied() {
        assert_eq!(map(true), Permissions { profiler: Access::Allowed, ..Default::default() });
        assert_eq!(map(false), Permissions::default());
    }
}

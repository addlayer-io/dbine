//! What the login may do (`Session::permissions`). Of the actions DBine
//! checks, Drill has only the profiler (schemas are storage-plugin
//! workspaces, there's no kill from the monitor, no native backups, no
//! users of its own). The profiler reads the profile store, where a user
//! who isn't an administrator (`security.admin.users` /
//! `security.admin.user_groups`) sees only its own queries once
//! authentication is on.
//!
//! `GET /cluster.json` says whether authentication is on (`authEnabled`)
//! and, only to an administrator, adds the admin settings (`adminUsers`,
//! `processUser`): their presence tells whether the login is one, groups
//! included, without reading anything else.

use crate::DrillSession;
use dbine_driver::{Access, Error, Permissions, Result};
use serde_json::Value;

const MISSING: &str = "administrador de Drill (security.admin.users o security.admin.user_groups)";

/// The profiler's access from `/cluster.json`.
pub(crate) fn profiler_access(cluster: &Value) -> Access {
    match cluster.get("authEnabled").and_then(Value::as_bool) {
        Some(false) => Access::Allowed,
        Some(true) => Access::check(cluster.get("adminUsers").is_some() || cluster.get("processUser").is_some(), MISSING),
        None => Access::Unknown,
    }
}

pub(crate) async fn check(s: &DrillSession) -> Result<Permissions> {
    let profiler = match s.conn.get("/cluster.json").await {
        Ok(v) => profiler_access(&v),
        Err(e @ Error::Connect(_)) => return Err(e),
        Err(_) => Access::Unknown,
    };
    Ok(Permissions { profiler, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admin_settings_show_only_to_admins() {
        assert_eq!(profiler_access(&json!({"currentVersion": "1.22.0", "authEnabled": false})), Access::Allowed);
        let admin = json!({"authEnabled": true, "processUser": "drill", "adminUsers": "drill,ana", "adminUserGroups": ""});
        assert_eq!(profiler_access(&admin), Access::Allowed);
        assert_eq!(profiler_access(&json!({"authEnabled": true})), Access::Denied { missing: MISSING.into() });
        assert_eq!(profiler_access(&json!({"drillbits": []})), Access::Unknown);
    }
}

//! What the connected login may do on the server (`Session::permissions`),
//! so the UI turns off the actions that would only fail (a backup without
//! the privilege, the profiler without access to other sessions…) and says
//! which privilege is missing. Checked once per connection and database.

use serde::{Deserialize, Serialize};

/// Whether the login may do one action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Access {
    /// Not checked (the engine can't tell, or the check failed): the UI
    /// leaves the action on and lets the server answer.
    #[default]
    Unknown,
    Allowed,
    /// `missing`: the privilege, role or grant it lacks, in the engine's
    /// own terms (`BACKUP DATABASE`, `pg_read_all_stats`, `PROCESS`…).
    Denied { missing: String },
}

impl Access {
    pub fn check(allowed: bool, missing: impl Into<String>) -> Self {
        if allowed { Access::Allowed } else { Access::Denied { missing: missing.into() } }
    }

    pub fn is_denied(&self) -> bool {
        matches!(self, Access::Denied { .. })
    }
}

/// The login's access to the actions that need more than reading data.
/// Actions the engine doesn't have stay `Unknown`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    /// Make a native backup (the Backups tab).
    #[serde(default)]
    pub backup: Access,
    /// Restore a native backup.
    #[serde(default)]
    pub restore: Access,
    /// See the statements other clients run (the profiler).
    #[serde(default)]
    pub profiler: Access,
    /// End another session from the monitor.
    #[serde(default)]
    pub kill_session: Access,
    #[serde(default)]
    pub create_database: Access,
    /// Drop the database the check was made for.
    #[serde(default)]
    pub drop_database: Access,
    /// Create users and roles, change passwords, grant and revoke.
    #[serde(default)]
    pub manage_security: Access,
    /// Create a schema in the database the check was made for.
    #[serde(default)]
    pub create_schema: Access,
}

impl Permissions {
    /// Every action allowed: engines without logins (an embedded file) or
    /// logins that are the server's administrator.
    pub fn all() -> Self {
        Permissions {
            backup: Access::Allowed,
            restore: Access::Allowed,
            profiler: Access::Allowed,
            kill_session: Access::Allowed,
            create_database: Access::Allowed,
            drop_database: Access::Allowed,
            manage_security: Access::Allowed,
            create_schema: Access::Allowed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_for_the_ui() {
        let p = Permissions { backup: Access::check(false, "BACKUP DATABASE"), restore: Access::Allowed, ..Default::default() };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["backup"], serde_json::json!({ "state": "denied", "missing": "BACKUP DATABASE" }));
        assert_eq!(v["restore"], serde_json::json!({ "state": "allowed" }));
        assert_eq!(v["profiler"], serde_json::json!({ "state": "unknown" }));
        let back: Permissions = serde_json::from_value(serde_json::json!({ "backup": { "state": "allowed" } })).unwrap();
        assert_eq!(back.backup, Access::Allowed);
        assert_eq!(back.restore, Access::Unknown);
    }

    #[test]
    fn create_schema_is_optional_and_in_all() {
        // What an older driver host sends: no `create_schema`.
        let old = serde_json::json!({
            "backup": { "state": "allowed" }, "restore": { "state": "unknown" }, "profiler": { "state": "unknown" },
            "kill_session": { "state": "unknown" }, "create_database": { "state": "unknown" },
            "drop_database": { "state": "unknown" }, "manage_security": { "state": "allowed" },
        });
        let p: Permissions = serde_json::from_value(old).unwrap();
        assert_eq!(p.create_schema, Access::Unknown);
        assert_eq!(p.manage_security, Access::Allowed);
        assert_eq!(Permissions::all().create_schema, Access::Allowed);
        let v = serde_json::to_value(Permissions::default()).unwrap();
        assert_eq!(v["create_schema"], serde_json::json!({ "state": "unknown" }));
    }
}

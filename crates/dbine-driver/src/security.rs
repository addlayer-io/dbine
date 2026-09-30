//! Users, roles and permissions (the "Usuarios y permisos" tab,
//! docs/usuarios-y-permisos.md): what a server has (`Session::principals`,
//! `Session::grants`) and the code that changes it
//! (`Driver::security_script`), which DBine shows and runs only when the user
//! says so.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// Can sign in (a login, a user).
    #[default]
    User,
    /// Groups permissions; others are its members.
    Role,
}

/// A user or a role.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Principal {
    pub name: String,
    pub kind: PrincipalKind,
    /// `None` when the engine doesn't say.
    pub can_login: Option<bool>,
    /// Superuser / sysadmin / DBA: every permission.
    pub superuser: Option<bool>,
    pub disabled: Option<bool>,
    /// Roles it's a member of.
    pub member_of: Vec<String>,
    /// Other facts as the engine reports them, Spanish labels: default
    /// database, authentication, expiry, created…
    pub details: Vec<(String, String)>,
    /// Built in (sa, postgres, root, PUBLIC…): the UI doesn't offer to
    /// drop it.
    pub system: bool,
}

/// One permission of a principal.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    /// As the engine names it: SELECT, EXECUTE, CONNECT, readWrite…
    pub privilege: String,
    /// What it applies to ("dbo.facturas", a database, a collection…);
    /// `None`: the whole server / database.
    pub object: Option<String>,
    /// Table, view, schema, database, collection…
    pub object_kind: Option<String>,
    /// It can grant it to others.
    pub grantable: bool,
    /// Denied rather than granted (SQL Server's DENY).
    pub denied: bool,
    /// Held through this role, not directly.
    pub via: Option<String>,
}

/// What the tab offers for an engine (`Driver::security`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SecuritySpec {
    /// Privileges offered when granting, in order (the engine's names).
    #[serde(deserialize_with = "crate::serde_static::strs")]
    pub privileges: Vec<&'static str>,
    /// Kinds of objects a privilege can be granted on ("" = the whole
    /// database/server), in the explorer's kind ids.
    #[serde(deserialize_with = "crate::serde_static::strs")]
    pub object_kinds: Vec<&'static str>,
    pub create_user: bool,
    pub create_role: bool,
    /// Users' passwords can be set.
    pub passwords: bool,
    /// Roles can hold members (users or roles).
    pub membership: bool,
    /// Principals and grants are per database (SQL Server users, MongoDB),
    /// not server-wide.
    pub per_database: bool,
}

/// A change to users, roles or permissions: `Driver::security_script`
/// writes the code for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SecurityAction {
    CreateUser { name: String, password: Option<String> },
    CreateRole { name: String },
    Drop { name: String, kind: PrincipalKind },
    SetPassword { name: String, password: String },
    /// Enable / disable signing in.
    SetLogin { name: String, enabled: bool },
    /// `object`: None for database/server-wide privileges.
    Grant { privileges: Vec<String>, object: Option<crate::ObjectRef>, to: String, grantable: bool },
    Revoke { privileges: Vec<String>, object: Option<crate::ObjectRef>, from: String },
    AddMember { role: String, member: String },
    RemoveMember { role: String, member: String },
}

impl SecurityAction {
    /// The password it carries (the UI hides it in the preview and the
    /// script isn't kept in the history).
    pub fn password(&self) -> Option<&str> {
        match self {
            SecurityAction::CreateUser { password, .. } => password.as_deref(),
            SecurityAction::SetPassword { password, .. } => Some(password),
            _ => None,
        }
    }
}

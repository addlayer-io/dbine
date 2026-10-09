//! Users, roles and permissions (docs/users-and-permissions.md) over the
//! security REST APIs, written in the console syntax the editor runs
//! (see [`crate::console`]).
//!
//! - Elasticsearch: `/_security/user` and `/_security/role`. Permissions
//!   live in role definitions, which can only be replaced whole, so single
//!   grants, revokes and memberships aren't offered (a static script
//!   would wipe what the role or user already has).
//! - OpenSearch / Open Distro: the security plugin's
//!   `/_plugins/_security/api/…` (`/_opendistro/_security/api/…`):
//!   internal users, roles and role mappings. JSON Patch appends a
//!   permission to a role or a user to a role mapping without touching the
//!   rest; removing one needs its position, so it isn't offered.

use crate::json::J;
use crate::{es_error_message, http, EsSession};
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::json;

/// Which security API the server has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Elastic,
    /// The OpenSearch security plugin under this prefix.
    Plugin(&'static str),
}

pub const OPENSEARCH: Api = Api::Plugin("/_plugins/_security/api");
pub const OPENDISTRO: Api = Api::Plugin("/_opendistro/_security/api");

pub fn spec(api: Api) -> SecuritySpec {
    match api {
        Api::Elastic => SecuritySpec {
            privileges: Vec::new(),
            object_kinds: Vec::new(),
            create_user: true,
            create_role: true,
            passwords: true,
            membership: false,
            per_database: false,
        },
        Api::Plugin(_) => SecuritySpec {
            privileges: vec![
                "read", "write", "search", "get", "index", "delete", "crud", "create_index", "manage", "manage_aliases",
                "indices_monitor", "indices_all", "cluster_monitor", "cluster_composite_ops", "cluster_composite_ops_ro",
                "cluster_manage_index_templates", "manage_snapshots", "cluster_all",
            ],
            object_kinds: vec!["", kinds::INDEX],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
    }
}

// -- reading -----------------------------------------------------------------

fn strs(v: Option<&J>) -> Vec<String> {
    v.and_then(J::as_arr).unwrap_or(&[]).iter().filter_map(J::as_str).map(str::to_string).collect()
}
fn truthy(v: Option<&J>) -> bool {
    v.and_then(J::as_bool).unwrap_or(false)
}
fn entries(j: &J) -> &[(String, J)] {
    j.as_obj().map(Vec::as_slice).unwrap_or(&[])
}

impl EsSession {
    /// GET a security API path; a server without security answers with a
    /// clear error instead of the engine's.
    async fn security_get(&self, api: Api, path: &str) -> Result<J> {
        let (status, body) = http::send(self.request("GET", path)).await?;
        if status >= 400 {
            let msg = es_error_message(status, &body);
            let lower = msg.to_lowercase();
            return Err(match api {
                Api::Elastic if lower.contains("xpack.security.enabled") || lower.contains("security must be explicitly enabled") || lower.contains("security is not enabled") => {
                    Error::Unsupported("la seguridad de Elasticsearch está deshabilitada (xpack.security.enabled=false): no hay usuarios ni roles que administrar".into())
                }
                Api::Plugin(_) if lower.contains("no handler found") => Error::Unsupported(
                    "el plugin de seguridad de OpenSearch no está instalado o está deshabilitado: no hay usuarios ni roles que administrar".into(),
                ),
                _ => Error::Query(msg),
            });
        }
        J::parse(&body).map_err(|e| Error::Query(format!("Respuesta inesperada del servidor: {e}")))
    }

    pub(crate) fn security_api(&self) -> Api {
        match (self.opensearch, self.opendistro) {
            (_, true) => OPENDISTRO,
            (true, _) => OPENSEARCH,
            _ => Api::Elastic,
        }
    }
}

pub async fn principals(s: &EsSession) -> Result<Vec<Principal>> {
    match s.security_api() {
        Api::Elastic => {
            let users = s.security_get(Api::Elastic, "/_security/user").await?;
            let roles = s.security_get(Api::Elastic, "/_security/role").await?;
            Ok(es_principals(&users, &roles))
        }
        api @ Api::Plugin(p) => {
            let users = s.security_get(api, &format!("{p}/internalusers")).await?;
            let roles = s.security_get(api, &format!("{p}/roles")).await?;
            let mapping = s.security_get(api, &format!("{p}/rolesmapping")).await?;
            Ok(os_principals(&users, &roles, &mapping))
        }
    }
}

pub async fn grants(s: &EsSession, principal: &str) -> Result<Vec<Grant>> {
    match s.security_api() {
        Api::Elastic => {
            let users = s.security_get(Api::Elastic, "/_security/user").await?;
            let roles = s.security_get(Api::Elastic, "/_security/role").await?;
            Ok(es_grants(&users, &roles, principal))
        }
        api @ Api::Plugin(p) => {
            let users = s.security_get(api, &format!("{p}/internalusers")).await?;
            let roles = s.security_get(api, &format!("{p}/roles")).await?;
            let mapping = s.security_get(api, &format!("{p}/rolesmapping")).await?;
            Ok(os_grants(&users, &roles, &mapping, principal))
        }
    }
}

fn es_principals(users: &J, roles: &J) -> Vec<Principal> {
    let mut out = Vec::new();
    for (name, u) in entries(users) {
        let member_of = strs(u.get("roles"));
        let reserved = truthy(u.at(&["metadata", "_reserved"]));
        let mut details = vec![("Tipo".into(), if reserved { "Usuario reservado" } else { "Usuario nativo" }.into())];
        if let Some(v) = u.get("full_name").and_then(J::as_str).filter(|v| !v.is_empty()) {
            details.push(("Nombre completo".into(), v.into()));
        }
        if let Some(v) = u.get("email").and_then(J::as_str).filter(|v| !v.is_empty()) {
            details.push(("Correo".into(), v.into()));
        }
        if !member_of.is_empty() {
            details.push(("Roles".into(), member_of.join(", ")));
        }
        out.push(Principal {
            name: name.clone(),
            kind: PrincipalKind::User,
            can_login: Some(true),
            superuser: Some(member_of.iter().any(|r| r == "superuser")),
            disabled: Some(!u.get("enabled").and_then(J::as_bool).unwrap_or(true)),
            member_of,
            details,
            system: reserved,
        });
    }
    for (name, r) in entries(roles) {
        let reserved = truthy(r.at(&["metadata", "_reserved"]));
        let mut details = vec![("Tipo".into(), if reserved { "Rol reservado" } else { "Rol" }.into())];
        if let Some(d) = r.get("description").and_then(J::as_str).filter(|v| !v.is_empty()) {
            details.push(("Descripción".into(), d.into()));
        }
        let cluster = strs(r.get("cluster"));
        out.push(Principal {
            name: name.clone(),
            kind: PrincipalKind::Role,
            superuser: Some(cluster.iter().any(|c| c == "all")),
            details,
            system: reserved,
            ..Default::default()
        });
    }
    out
}

fn grant(privilege: &str, object: Option<&str>, kind: Option<&str>, via: Option<&str>) -> Grant {
    Grant {
        privilege: privilege.to_string(),
        object: object.map(str::to_string),
        object_kind: kind.map(str::to_string),
        via: via.map(str::to_string),
        ..Default::default()
    }
}

/// The permissions an Elasticsearch role definition gives.
fn es_role_grants(r: &J, via: Option<&str>, out: &mut Vec<Grant>) {
    for c in strs(r.get("cluster")) {
        out.push(grant(&c, None, None, via));
    }
    for i in r.get("indices").and_then(J::as_arr).unwrap_or(&[]) {
        for n in strs(i.get("names")) {
            for p in strs(i.get("privileges")) {
                out.push(grant(&p, Some(&n), Some(kinds::INDEX), via));
            }
        }
    }
    for a in r.get("applications").and_then(J::as_arr).unwrap_or(&[]) {
        let app = a.get("application").and_then(J::as_str).unwrap_or("");
        for res in strs(a.get("resources")) {
            for p in strs(a.get("privileges")) {
                out.push(grant(&format!("{app}:{p}"), Some(&res), Some("application"), via));
            }
        }
    }
    for u in strs(r.get("run_as")) {
        out.push(grant("run_as", Some(&u), Some("user"), via));
    }
}

fn es_grants(users: &J, roles: &J, principal: &str) -> Vec<Grant> {
    let mut out = Vec::new();
    match users.get(principal) {
        Some(u) => {
            for role in strs(u.get("roles")) {
                if let Some(r) = roles.get(&role) {
                    es_role_grants(r, Some(&role), &mut out);
                }
            }
        }
        None => {
            if let Some(r) = roles.get(principal) {
                es_role_grants(r, None, &mut out);
            }
        }
    }
    out
}

/// The roles an OpenSearch internal user gets: by name in a role mapping,
/// through one of its backend roles, or in its own attribute.
fn os_user_roles(user: &str, u: &J, mapping: &J) -> Vec<String> {
    let backend = strs(u.get("backend_roles"));
    let mut out = strs(u.get("opendistro_security_roles"));
    for (role, m) in entries(mapping) {
        let by_user = strs(m.get("users")).iter().any(|x| x == user);
        let by_backend = strs(m.get("backend_roles")).iter().any(|b| backend.contains(b));
        if (by_user || by_backend) && !out.contains(role) {
            out.push(role.clone());
        }
    }
    out
}

fn os_principals(users: &J, roles: &J, mapping: &J) -> Vec<Principal> {
    let mut out = Vec::new();
    for (name, u) in entries(users) {
        let member_of = os_user_roles(name, u, mapping);
        let reserved = truthy(u.get("reserved")) || truthy(u.get("static"));
        let mut details = vec![("Tipo".into(), if reserved { "Usuario interno reservado" } else { "Usuario interno" }.into())];
        let backend = strs(u.get("backend_roles"));
        if !backend.is_empty() {
            details.push(("Roles de backend".into(), backend.join(", ")));
        }
        if truthy(u.get("hidden")) {
            details.push(("Oculto".into(), "sí".into()));
        }
        if let Some(d) = u.get("description").and_then(J::as_str).filter(|v| !v.is_empty()) {
            details.push(("Descripción".into(), d.into()));
        }
        out.push(Principal {
            name: name.clone(),
            kind: PrincipalKind::User,
            can_login: Some(true),
            superuser: Some(member_of.iter().any(|r| r == "all_access")),
            // Internal users can't be disabled.
            disabled: None,
            member_of,
            details,
            system: reserved,
        });
    }
    for (name, r) in entries(roles) {
        let reserved = truthy(r.get("reserved")) || truthy(r.get("static"));
        let mut details = vec![("Tipo".into(), if reserved { "Rol reservado" } else { "Rol" }.into())];
        if let Some(d) = r.get("description").and_then(J::as_str).filter(|v| !v.is_empty()) {
            details.push(("Descripción".into(), d.into()));
        }
        if let Some(m) = mapping.get(name) {
            let b = strs(m.get("backend_roles"));
            if !b.is_empty() {
                details.push(("Roles de backend mapeados".into(), b.join(", ")));
            }
            let h = strs(m.get("hosts"));
            if !h.is_empty() {
                details.push(("Hosts mapeados".into(), h.join(", ")));
            }
        }
        let cluster = strs(r.get("cluster_permissions"));
        out.push(Principal {
            name: name.clone(),
            kind: PrincipalKind::Role,
            superuser: Some(name == "all_access" || cluster.iter().any(|c| c == "*" || c == "cluster_all")),
            details,
            system: reserved,
            ..Default::default()
        });
    }
    out
}

fn os_role_grants(r: &J, via: Option<&str>, out: &mut Vec<Grant>) {
    for c in strs(r.get("cluster_permissions")) {
        out.push(grant(&c, None, None, via));
    }
    for (key, patterns, kind) in [("index_permissions", "index_patterns", kinds::INDEX), ("tenant_permissions", "tenant_patterns", "tenant")] {
        for i in r.get(key).and_then(J::as_arr).unwrap_or(&[]) {
            for n in strs(i.get(patterns)) {
                for p in strs(i.get("allowed_actions")) {
                    out.push(grant(&p, Some(&n), Some(kind), via));
                }
            }
        }
    }
}

fn os_grants(users: &J, roles: &J, mapping: &J, principal: &str) -> Vec<Grant> {
    let mut out = Vec::new();
    match users.get(principal) {
        Some(u) => {
            for role in os_user_roles(principal, u, mapping) {
                if let Some(r) = roles.get(&role) {
                    os_role_grants(r, Some(&role), &mut out);
                }
            }
        }
        None => {
            if let Some(r) = roles.get(principal) {
                os_role_grants(r, None, &mut out);
            }
        }
    }
    out
}

/// OpenSearch's JSON Patch can't append to an empty list (it answers
/// `Missing field "users"`): say how to add the first entry.
pub fn explain_error(req: &crate::console::Request, msg: String) -> String {
    if req.method == "PATCH" && req.path.contains("/_security/api/") && msg.contains("Missing field") {
        let after = msg.split("Missing field").nth(1).unwrap_or("");
        let field: String = after.trim_start_matches(|c: char| !c.is_ascii_alphanumeric()).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        return format!(
            "{msg}. OpenSearch no puede agregar a una lista vacía: cargá la primera entrada desde la consola, con PATCH {} y [{{\"op\": \"add\", \"path\": \"/{field}\", \"value\": [ … ]}}]",
            req.path_only()
        );
    }
    msg
}

// -- scripts -------------------------------------------------------------------

/// A user or role name as a path segment: printable ASCII only (what both
/// engines accept), percent-encoded.
fn seg(name: &str) -> Result<String> {
    if name.is_empty() || name.trim() != name {
        return Err(Error::Query("escribí un nombre sin espacios al principio ni al final".into()));
    }
    if !name.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err(Error::Query(format!("«{name}» tiene caracteres que no se admiten en un nombre de usuario o rol")));
    }
    Ok(name
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect())
}

fn body(v: serde_json::Value) -> String {
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

fn password(p: Option<&str>) -> Result<&str> {
    p.filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))
}

/// OpenSearch permissions and action groups: `read`, `indices:data/read/*`…
fn privileges(p: &[String]) -> Result<Vec<String>> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    p.iter()
        .map(|x| {
            let x = x.trim();
            if x.is_empty() || !x.chars().all(|c| c.is_ascii_alphanumeric() || "_:/*.-".contains(c)) {
                return Err(Error::Query(format!("«{x}» no es un permiso de OpenSearch")));
            }
            Ok(x.to_string())
        })
        .collect()
}

pub fn script(api: Api, a: &SecurityAction) -> Result<String> {
    match api {
        Api::Elastic => es_script(a),
        Api::Plugin(p) => os_script(p, a),
    }
}

fn es_script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => format!(
            "# Si el usuario ya existe, esto reemplaza sus roles y datos.\nPUT /_security/user/{}\n{}",
            seg(name)?,
            body(json!({ "password": password(pw.as_deref())?, "roles": [] }))
        ),
        SecurityAction::CreateRole { name } => format!(
            "# Si el rol ya existe, esto reemplaza su definición.\nPUT /_security/role/{}\n{}",
            seg(name)?,
            body(json!({ "cluster": [], "indices": [] }))
        ),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DELETE /_security/user/{}", seg(name)?),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DELETE /_security/role/{}", seg(name)?),
        SecurityAction::SetPassword { name, password: pw } => {
            format!("POST /_security/user/{}/_password\n{}", seg(name)?, body(json!({ "password": password(Some(pw))? })))
        }
        SecurityAction::SetLogin { name, enabled } => {
            format!("PUT /_security/user/{}/{}", seg(name)?, if *enabled { "_enable" } else { "_disable" })
        }
        SecurityAction::Grant { .. } | SecurityAction::Revoke { .. } => {
            return Err(Error::Unsupported(
                "Elasticsearch no permite otorgar o quitar un privilegio suelto: hay que reescribir la definición entera del rol (PUT /_security/role/<rol>). Hacelo desde la consola, partiendo de GET /_security/role/<rol>".into(),
            ))
        }
        SecurityAction::AddMember { .. } | SecurityAction::RemoveMember { .. } => {
            return Err(Error::Unsupported(
                "Elasticsearch no permite agregar o sacar un rol suelto: hay que reescribir la lista entera de roles del usuario (PUT /_security/user/<usuario>). Hacelo desde la consola, partiendo de GET /_security/user/<usuario>".into(),
            ))
        }
    })
}

fn os_script(prefix: &str, a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => format!(
            "# Si el usuario ya existe, esto lo reemplaza.\nPUT {prefix}/internalusers/{}\n{}",
            seg(name)?,
            body(json!({ "password": password(pw.as_deref())? }))
        ),
        SecurityAction::CreateRole { name } => format!(
            "# Si el rol ya existe, esto reemplaza su definición.\nPUT {prefix}/roles/{}\n{}",
            seg(name)?,
            body(json!({ "cluster_permissions": [], "index_permissions": [] }))
        ),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DELETE {prefix}/internalusers/{}", seg(name)?),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DELETE {prefix}/roles/{}", seg(name)?),
        SecurityAction::SetPassword { name, password: pw } => format!(
            "PATCH {prefix}/internalusers/{}\n{}",
            seg(name)?,
            body(json!([{ "op": "add", "path": "/password", "value": password(Some(pw))? }]))
        ),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported(
                "OpenSearch no permite deshabilitar un usuario interno: cambiale la contraseña o borralo".into(),
            ))
        }
        SecurityAction::Grant { privileges: p, object, to, .. } => {
            let p = privileges(p)?;
            let ops = match object {
                None => p.iter().map(|x| json!({ "op": "add", "path": "/cluster_permissions/-", "value": x })).collect::<Vec<_>>(),
                Some(ObjectRef { name, .. }) if !name.is_empty() => vec![json!({
                    "op": "add",
                    "path": "/index_permissions/-",
                    "value": { "index_patterns": [name], "allowed_actions": p }
                })],
                Some(_) => return Err(Error::Query("elegí el índice".into())),
            };
            format!("# Los permisos van en roles: «{to}» tiene que ser un rol. Se agregan a los que ya tiene.\nPATCH {prefix}/roles/{}\n{}", seg(to)?, body(json!(ops)))
        }
        SecurityAction::Revoke { .. } => {
            return Err(Error::Unsupported(
                "OpenSearch no permite quitar un permiso suelto: hay que reescribir el rol (PUT …/_security/api/roles/<rol>). Hacelo desde la consola".into(),
            ))
        }
        SecurityAction::AddMember { role, member } => format!(
            "# Se agrega a los usuarios del mapeo del rol. Si el rol todavía no tiene mapeo, crealo con PUT {prefix}/rolesmapping/<rol> y {{\"users\": [ … ]}}.\nPATCH {prefix}/rolesmapping/{}\n{}",
            seg(role)?,
            body(json!([{ "op": "add", "path": "/users/-", "value": member }]))
        ),
        SecurityAction::RemoveMember { .. } => {
            return Err(Error::Unsupported(
                "OpenSearch no permite sacar un usuario suelto de un rol: hay que reescribir el mapeo del rol (PUT …/_security/api/rolesmapping/<rol>). Hacelo desde la consola".into(),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::{parse, Command};

    fn one(script: &str) -> (String, String, Option<serde_json::Value>) {
        let cmds = parse(script).unwrap();
        assert_eq!(cmds.len(), 1, "{script}");
        match &cmds[0] {
            Command::Http(r) => (r.method.clone(), r.path.clone(), r.body.as_deref().map(|b| serde_json::from_str(b).unwrap())),
            Command::Sql(s) => panic!("SQL {s}"),
        }
    }

    #[test]
    fn elastic_scripts_parse_as_console_requests() {
        let s = |a| es_script(&a).unwrap();
        let (m, p, b) = one(&s(SecurityAction::CreateUser { name: "ana b/\"x".into(), password: Some("p\"w\n1".into()) }));
        assert_eq!((m.as_str(), p.as_str()), ("PUT", "/_security/user/ana%20b%2F%22x"));
        assert_eq!(b.unwrap(), json!({ "password": "p\"w\n1", "roles": [] }));
        assert_eq!(one(&s(SecurityAction::CreateRole { name: "lectores".into() })).1, "/_security/role/lectores");
        assert_eq!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "DELETE /_security/user/ana");
        assert_eq!(s(SecurityAction::Drop { name: "r".into(), kind: PrincipalKind::Role }), "DELETE /_security/role/r");
        let (m, p, b) = one(&s(SecurityAction::SetPassword { name: "ana".into(), password: "nueva1".into() }));
        assert_eq!((m.as_str(), p.as_str(), b.unwrap()), ("POST", "/_security/user/ana/_password", json!({ "password": "nueva1" })));
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "PUT /_security/user/ana/_disable");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: true }), "PUT /_security/user/ana/_enable");
        let g = SecurityAction::Grant { privileges: vec!["read".into()], object: None, to: "r".into(), grantable: false };
        assert!(matches!(es_script(&g), Err(Error::Unsupported(_))));
        assert!(matches!(es_script(&SecurityAction::AddMember { role: "r".into(), member: "ana".into() }), Err(Error::Unsupported(_))));
        assert!(es_script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert!(es_script(&SecurityAction::Drop { name: " ana".into(), kind: PrincipalKind::User }).is_err());
        assert!(es_script(&SecurityAction::Drop { name: "a\nb".into(), kind: PrincipalKind::User }).is_err());
    }

    #[test]
    fn opensearch_scripts_parse_as_console_requests() {
        let s = |a| os_script("/_plugins/_security/api", &a).unwrap();
        let (m, p, b) = one(&s(SecurityAction::Grant {
            privileges: vec!["read".into(), "indices:data/read/*".into()],
            object: Some(ObjectRef { kind: "index".into(), schema: None, name: "facturas-*".into() }),
            to: "lectores".into(),
            grantable: false,
        }));
        assert_eq!((m.as_str(), p.as_str()), ("PATCH", "/_plugins/_security/api/roles/lectores"));
        assert_eq!(
            b.unwrap(),
            json!([{ "op": "add", "path": "/index_permissions/-", "value": { "index_patterns": ["facturas-*"], "allowed_actions": ["read", "indices:data/read/*"] } }])
        );
        let (_, _, b) = one(&s(SecurityAction::Grant { privileges: vec!["cluster_monitor".into()], object: None, to: "r".into(), grantable: false }));
        assert_eq!(b.unwrap(), json!([{ "op": "add", "path": "/cluster_permissions/-", "value": "cluster_monitor" }]));
        let bad = SecurityAction::Grant { privileges: vec!["read\"]".into()], object: None, to: "r".into(), grantable: false };
        assert!(os_script("/x", &bad).is_err());
        let (m, p, b) = one(&s(SecurityAction::AddMember { role: "lectores".into(), member: "ana".into() }));
        assert_eq!((m.as_str(), p.as_str()), ("PATCH", "/_plugins/_security/api/rolesmapping/lectores"));
        assert_eq!(b.unwrap(), json!([{ "op": "add", "path": "/users/-", "value": "ana" }]));
        let (m, _, b) = one(&s(SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }));
        assert_eq!((m.as_str(), b.unwrap()), ("PATCH", json!([{ "op": "add", "path": "/password", "value": "x" }])));
        assert!(matches!(os_script("/x", &SecurityAction::SetLogin { name: "a".into(), enabled: true }), Err(Error::Unsupported(_))));
        assert!(matches!(os_script("/x", &SecurityAction::RemoveMember { role: "r".into(), member: "a".into() }), Err(Error::Unsupported(_))));
    }

    #[test]
    fn reads_elastic_users_roles_and_grants() {
        let users = J::parse(r#"{"elastic":{"username":"elastic","roles":["superuser"],"enabled":true,"metadata":{"_reserved":true}},
            "ana":{"username":"ana","roles":["lectores"],"full_name":"Ana","enabled":false,"metadata":{}}}"#).unwrap();
        let roles = J::parse(r#"{"lectores":{"cluster":["monitor"],"indices":[{"names":["a","b*"],"privileges":["read"]}],"run_as":["x"],"metadata":{}},
            "superuser":{"cluster":["all"],"metadata":{"_reserved":true}}}"#).unwrap();
        let p = es_principals(&users, &roles);
        let ana = p.iter().find(|p| p.name == "ana").unwrap();
        assert_eq!((ana.disabled, ana.superuser, ana.system, ana.member_of.clone()), (Some(true), Some(false), false, vec!["lectores".to_string()]));
        assert!(p.iter().any(|p| p.name == "elastic" && p.system && p.superuser == Some(true)));
        assert!(p.iter().any(|p| p.name == "superuser" && p.kind == PrincipalKind::Role && p.system));
        let g = es_grants(&users, &roles, "ana");
        assert_eq!(g.len(), 4);
        assert!(g.iter().all(|g| g.via.as_deref() == Some("lectores")));
        assert!(g.iter().any(|g| g.privilege == "read" && g.object.as_deref() == Some("b*") && g.object_kind.as_deref() == Some("index")));
        assert!(es_grants(&users, &roles, "lectores").iter().all(|g| g.via.is_none()));
    }

    #[test]
    fn reads_opensearch_users_roles_and_grants() {
        let users = J::parse(r#"{"admin":{"reserved":true,"backend_roles":["admin"],"opendistro_security_roles":[]},
            "ana":{"reserved":false,"backend_roles":["ventas"],"opendistro_security_roles":["propio"]}}"#).unwrap();
        let roles = J::parse(r#"{"all_access":{"reserved":true,"cluster_permissions":["*"],"index_permissions":[{"index_patterns":["*"],"allowed_actions":["*"]}]},
            "lectores":{"reserved":false,"cluster_permissions":[],"index_permissions":[{"index_patterns":["f*"],"allowed_actions":["read","search"]}]},
            "propio":{"cluster_permissions":["cluster_monitor"]}}"#).unwrap();
        let mapping = J::parse(r#"{"all_access":{"backend_roles":["admin"],"users":[]},"lectores":{"backend_roles":["ventas"],"users":[]}}"#).unwrap();
        let p = os_principals(&users, &roles, &mapping);
        let admin = p.iter().find(|p| p.name == "admin").unwrap();
        assert_eq!((admin.superuser, admin.system), (Some(true), true));
        let ana = p.iter().find(|p| p.name == "ana").unwrap();
        assert_eq!(ana.member_of, vec!["propio".to_string(), "lectores".to_string()]);
        let g = os_grants(&users, &roles, &mapping, "ana");
        assert_eq!(g.len(), 3);
        assert!(g.iter().any(|g| g.privilege == "search" && g.via.as_deref() == Some("lectores") && g.object.as_deref() == Some("f*")));
    }
}

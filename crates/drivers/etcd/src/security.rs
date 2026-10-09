//! Users, roles and permissions through etcd's Auth API
//! (docs/users-and-permissions.md): users belong to roles and roles hold
//! READ / WRITE / READWRITE on a key, a prefix or a range. `root` (user and
//! role) can do everything. Users and roles exist whether or not auth is
//! enabled; etcd only enforces them after `auth enable`.
//!
//! The console takes etcdctl's auth commands (see [`run`]); scripts are
//! written in them.

use crate::command::{prefix_end, quote_arg, Command};
use crate::{b64, cols, unb64, EtcdSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, QueryOutcome, Result, SecurityAction, SecuritySpec};
use serde_json::{json, Value};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["READ", "WRITE", "READWRITE"],
        object_kinds: vec!["", "key"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

/// Console commands this module runs.
pub fn handles(name: &str) -> bool {
    matches!(
        name,
        "user add"
            | "user delete"
            | "user get"
            | "user passwd"
            | "user grant-role"
            | "user revoke-role"
            | "role add"
            | "role delete"
            | "role get"
            | "role grant-permission"
            | "role revoke-permission"
            | "auth status"
            | "auth enable"
            | "auth disable"
    )
}

fn arg(c: &Command, i: usize, what: &str) -> Result<String> {
    c.args
        .get(i)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .ok_or_else(|| Error::Query(format!("{} necesita {what}", c.name())))
}

/// The key range of a permission from `<key> [<end>] [--prefix] [--from-key]`
/// starting at argument `i` (etcdctl's rules; `"" --prefix` = every key).
fn perm_range(c: &Command, i: usize) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let key = c.args.get(i).cloned().ok_or_else(|| Error::Query(format!("{} necesita una clave", c.name())))?;
    let end = if c.flag("prefix") {
        Some(prefix_end(&key))
    } else if c.flag("from-key") {
        Some(vec![0])
    } else {
        c.args.get(i + 1).cloned()
    };
    let key = if key.is_empty() && end.is_some() { vec![0] } else { key };
    Ok((key, end))
}

fn perm_type(s: &str) -> Result<&'static str> {
    match s.to_ascii_uppercase().as_str() {
        "READ" => Ok("READ"),
        "WRITE" => Ok("WRITE"),
        "READWRITE" => Ok("READWRITE"),
        _ => Err(Error::Query(format!("Permiso inválido: {s:?}. Elegí READ, WRITE o READWRITE."))),
    }
}

/// The password of `user add` / `user passwd`: the argument after the name
/// (DBine's own, the console doesn't prompt) or etcdctl's
/// `--new-user-password=…`.
fn password(c: &Command) -> Option<String> {
    c.args.get(3).map(|p| String::from_utf8_lossy(p).into_owned()).or_else(|| c.flag_value("new-user-password").map(str::to_string))
}

/// Runs one auth command of the console.
pub async fn run(s: &EtcdSession, c: &Command, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    match c.name().as_str() {
        "user add" => {
            let name = arg(c, 2, "un nombre")?;
            let body = match password(c) {
                _ if c.flag("no-password") => json!({"name": name, "options": {"no_password": true}}),
                Some(p) => json!({"name": name, "password": p}),
                None => {
                    return Err(Error::Query(
                        "user add necesita una contraseña (user add <usuario> <contraseña>) o --no-password".into(),
                    ))
                }
            };
            s.call("/v3/auth/user/add", body).await?;
            out.push_affected(1);
            out.info(format!("usuario {name} creado"));
        }
        "user delete" => {
            let name = arg(c, 2, "un usuario")?;
            s.call("/v3/auth/user/delete", json!({"name": name})).await?;
            out.push_affected(1);
        }
        "user passwd" => {
            let name = arg(c, 2, "un usuario")?;
            let p = password(c).ok_or_else(|| Error::Query("user passwd necesita la contraseña nueva (user passwd <usuario> <contraseña>)".into()))?;
            s.call("/v3/auth/user/changepw", json!({"name": name, "password": p})).await?;
            out.push_affected(1);
            out.info(format!("contraseña de {name} cambiada"));
        }
        "user get" => {
            let name = arg(c, 2, "un usuario")?;
            let r = s.call("/v3/auth/user/get", json!({"name": name})).await?;
            out.begin_result(cols(&["user", "role"]));
            for role in strings(r.get("roles")) {
                out.push_row(vec![Value::String(name.clone()), Value::String(role)], max_rows);
            }
        }
        "user grant-role" | "user revoke-role" => {
            let user = arg(c, 2, "un usuario y un rol")?;
            let role = arg(c, 3, "un usuario y un rol")?;
            if c.name() == "user grant-role" {
                s.call("/v3/auth/user/grant", json!({"user": user, "role": role})).await?;
            } else {
                s.call("/v3/auth/user/revoke", json!({"name": user, "role": role})).await?;
            }
            out.push_affected(1);
        }
        "role add" => {
            let name = arg(c, 2, "un nombre")?;
            s.call("/v3/auth/role/add", json!({"name": name})).await?;
            out.push_affected(1);
            out.info(format!("rol {name} creado"));
        }
        "role delete" => {
            let name = arg(c, 2, "un rol")?;
            s.call("/v3/auth/role/delete", json!({"role": name})).await?;
            out.push_affected(1);
        }
        "role get" => {
            let name = arg(c, 2, "un rol")?;
            let r = s.call("/v3/auth/role/get", json!({"role": name})).await?;
            out.begin_result(cols(&["role", "permission", "key", "range_end"]));
            for p in r.get("perm").and_then(Value::as_array).into_iter().flatten() {
                let end = unb64(p.get("range_end"));
                out.push_row(
                    vec![
                        Value::String(name.clone()),
                        Value::String(perm_of(p).into()),
                        Value::String(String::from_utf8_lossy(&unb64(p.get("key"))).into_owned()),
                        if end.is_empty() { Value::Null } else { Value::String(String::from_utf8_lossy(&end).into_owned()) },
                    ],
                    max_rows,
                );
            }
        }
        "role grant-permission" => {
            let role = arg(c, 2, "un rol, un permiso y una clave")?;
            let t = perm_type(&arg(c, 3, "un rol, un permiso y una clave")?)?;
            let (key, end) = perm_range(c, 4)?;
            let mut perm = json!({"permType": t, "key": b64(&key)});
            if let Some(e) = end {
                perm["range_end"] = json!(b64(&e));
            }
            s.call("/v3/auth/role/grant", json!({"name": role, "perm": perm})).await?;
            out.push_affected(1);
        }
        "role revoke-permission" => {
            let role = arg(c, 2, "un rol y una clave")?;
            let (key, end) = perm_range(c, 3)?;
            let mut body = json!({"role": role, "key": b64(&key)});
            if let Some(e) = end {
                body["range_end"] = json!(b64(&e));
            }
            s.call("/v3/auth/role/revoke", body).await?;
            out.push_affected(1);
        }
        "auth status" => {
            let r = s.call("/v3/auth/status", json!({})).await?;
            out.begin_result(cols(&["enabled", "auth_revision"]));
            out.push_row(
                vec![Value::Bool(auth_enabled(&r)), r.get("authRevision").cloned().unwrap_or(Value::Null)],
                max_rows,
            );
        }
        "auth enable" => {
            s.call("/v3/auth/enable", json!({})).await?;
            out.push_affected(0);
            out.info("autenticación habilitada: desde ahora hace falta usuario y contraseña");
        }
        "auth disable" => {
            s.call("/v3/auth/disable", json!({})).await?;
            out.push_affected(0);
            out.info("autenticación deshabilitada");
        }
        other => return Err(Error::Query(format!("Comando desconocido: {other}"))),
    }
    Ok(())
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

/// proto3 JSON leaves out the default value: no `permType` is READ.
fn perm_of(p: &Value) -> &str {
    p.get("permType").and_then(Value::as_str).unwrap_or("READ")
}

fn auth_enabled(status: &Value) -> bool {
    status.get("enabled").and_then(Value::as_bool).unwrap_or(false)
}

// -- reading -----------------------------------------------------------------

pub async fn principals(s: &EtcdSession) -> Result<Vec<Principal>> {
    let status = s.call("/v3/auth/status", json!({})).await?;
    let auth = if auth_enabled(&status) {
        "habilitada".to_string()
    } else {
        "deshabilitada (los permisos no se aplican hasta auth enable)".to_string()
    };
    let users = strings(s.call("/v3/auth/user/list", json!({})).await?.get("users"));
    let roles = strings(s.call("/v3/auth/role/list", json!({})).await?.get("roles"));
    let mut out = Vec::new();
    for name in users {
        let member_of = strings(s.call("/v3/auth/user/get", json!({"name": name})).await?.get("roles"));
        out.push(Principal {
            superuser: Some(member_of.iter().any(|r| r == "root")),
            system: name == "root",
            kind: PrincipalKind::User,
            can_login: Some(true),
            disabled: None,
            member_of,
            details: vec![("Autenticación del servidor".into(), auth.clone())],
            name,
        });
    }
    for name in roles {
        out.push(Principal {
            superuser: Some(name == "root"),
            system: name == "root",
            kind: PrincipalKind::Role,
            can_login: None,
            disabled: None,
            member_of: Vec::new(),
            details: vec![("Autenticación del servidor".into(), auth.clone())],
            name,
        });
    }
    Ok(out)
}

/// How a permission's range reads in the tab, and back (see
/// [`object_range`]).
fn range_object(key: &[u8], end: &[u8]) -> (Option<String>, Option<String>) {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if end.is_empty() {
        (Some(text(key)), Some("key".into()))
    } else if key == [0] && end == [0] {
        (None, None)
    } else if end == [0] {
        (Some(text(key)), Some("from_key".into()))
    } else if end == prefix_end(key).as_slice() {
        (Some(text(key)), Some("prefix".into()))
    } else {
        (Some(format!("{} → {}", text(key), text(end))), Some("range".into()))
    }
}

async fn role_grants(s: &EtcdSession, role: &str, via: Option<&str>, out: &mut Vec<Grant>) -> Result<()> {
    if role == "root" {
        out.push(Grant { privilege: "READWRITE".into(), via: via.map(str::to_string), ..Default::default() });
    }
    let r = s.call("/v3/auth/role/get", json!({"role": role})).await?;
    for p in r.get("perm").and_then(Value::as_array).into_iter().flatten() {
        let (object, object_kind) = range_object(&unb64(p.get("key")), &unb64(p.get("range_end")));
        out.push(Grant { privilege: perm_of(p).to_string(), object, object_kind, via: via.map(str::to_string), ..Default::default() });
    }
    Ok(())
}

/// A user's permissions come through its roles; a role's are its own. A
/// name that is both (like `root`) reads as the user.
pub async fn grants(s: &EtcdSession, principal: &str) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    match s.call("/v3/auth/user/get", json!({"name": principal})).await {
        Ok(u) => {
            for role in strings(u.get("roles")) {
                role_grants(s, &role, Some(&role), &mut out).await?;
            }
        }
        Err(e) if e.is_query() && e.to_string().contains("user name not found") => role_grants(s, principal, None, &mut out).await?,
        Err(e) => return Err(e),
    }
    Ok(out)
}

// -- scripts -----------------------------------------------------------------

fn check_name<'a>(name: &'a str, what: &str) -> Result<&'a str> {
    if name.trim().is_empty() {
        return Err(Error::Query(format!("Escribí el nombre del {what}.")));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::Query(format!("El nombre del {what} no puede tener caracteres de control.")));
    }
    Ok(name)
}

fn check_password(pw: &str) -> Result<&str> {
    if pw.is_empty() {
        return Err(Error::Query("Escribí la contraseña.".into()));
    }
    if pw.chars().any(char::is_control) {
        return Err(Error::Query("La contraseña no puede tener caracteres de control.".into()));
    }
    Ok(pw)
}

/// The `<key> [<end>] [--prefix|--from-key]` of an object: the whole
/// keyspace without one; an explorer key ending in `/` is a prefix.
fn object_range(o: Option<&ObjectRef>) -> Result<String> {
    let Some(o) = o else { return Ok("\"\" --prefix".into()) };
    // The tab splits objects on the first '.' into schema and name.
    let name = match o.schema() {
        Some(s) => format!("{s}.{}", o.name),
        None => o.name.clone(),
    };
    Ok(match o.kind.as_str() {
        "prefix" => format!("{} --prefix", quote_arg(&name)),
        "from_key" => format!("{} --from-key", quote_arg(&name)),
        "range" => {
            let (a, b) = name.split_once(" → ").ok_or_else(|| Error::Query(format!("Rango inválido: {name}")))?;
            format!("{} {}", quote_arg(a), quote_arg(b))
        }
        _ if name.ends_with('/') => format!("{} --prefix", quote_arg(&name)),
        _ => quote_arg(&name),
    })
}

/// READ + WRITE is READWRITE: etcd keeps one permission per range.
fn combined(privileges: &[String]) -> Result<&'static str> {
    let mut read = false;
    let mut write = false;
    for p in privileges {
        match perm_type(p.trim())? {
            "READ" => read = true,
            "WRITE" => write = true,
            _ => (read, write) = (true, true),
        }
    }
    Ok(match (read, write) {
        (true, true) => "readwrite",
        (true, false) => "read",
        (false, true) => "write",
        _ => return Err(Error::Query("Elegí al menos un permiso.".into())),
    })
}

pub fn script(action: &SecurityAction) -> Result<String> {
    Ok(match action {
        SecurityAction::CreateUser { name, password } => {
            let name = quote_arg(check_name(name, "usuario")?);
            match password.as_deref().filter(|p| !p.is_empty()) {
                Some(p) => format!("user add {name} {}", quote_arg(check_password(p)?)),
                None => format!("user add {name} --no-password"),
            }
        }
        SecurityAction::CreateRole { name } => format!("role add {}", quote_arg(check_name(name, "rol")?)),
        SecurityAction::Drop { name, kind } => match kind {
            PrincipalKind::User => format!("user delete {}", quote_arg(check_name(name, "usuario")?)),
            PrincipalKind::Role => format!("role delete {}", quote_arg(check_name(name, "rol")?)),
        },
        SecurityAction::SetPassword { name, password } => {
            format!("user passwd {} {}", quote_arg(check_name(name, "usuario")?), quote_arg(check_password(password)?))
        }
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported(
                "etcd no permite deshabilitar un usuario: para que no entre, borralo o sacale los roles.".into(),
            ))
        }
        SecurityAction::Grant { privileges, object, to, grantable } => {
            if *grantable {
                return Err(Error::Unsupported("etcd no tiene la opción de otorgar permisos a otros: solo root administra usuarios y roles.".into()));
            }
            format!(
                "role grant-permission {} {} {}",
                quote_arg(check_name(to, "rol")?),
                combined(privileges)?,
                object_range(object.as_ref())?
            )
        }
        // etcd removes the range's permission whatever its type.
        SecurityAction::Revoke { privileges, object, from } => {
            combined(privileges)?;
            format!("role revoke-permission {} {}", quote_arg(check_name(from, "rol")?), object_range(object.as_ref())?)
        }
        SecurityAction::AddMember { role, member } => {
            format!("user grant-role {} {}", quote_arg(check_name(member, "usuario")?), quote_arg(check_name(role, "rol")?))
        }
        SecurityAction::RemoveMember { role, member } => {
            format!("user revoke-role {} {}", quote_arg(check_name(member, "usuario")?), quote_arg(check_name(role, "rol")?))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse_script;

    fn key(kind: &str, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: None, name: name.into() })
    }

    #[test]
    fn scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p w".into()) }), "user add ana \"p w\"");
        assert_eq!(s(SecurityAction::CreateUser { name: "--x".into(), password: None }), "user add \"--x\" --no-password");
        assert_eq!(s(SecurityAction::CreateRole { name: "lect".into() }), "role add lect");
        assert_eq!(s(SecurityAction::Drop { name: "lect".into(), kind: PrincipalKind::Role }), "role delete lect");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "--y".into() }), "user passwd ana \"--y\"");
        assert_eq!(s(SecurityAction::AddMember { role: "lect".into(), member: "ana".into() }), "user grant-role ana lect");
        assert_eq!(s(SecurityAction::RemoveMember { role: "lect".into(), member: "ana".into() }), "user revoke-role ana lect");
        let g = |p: &[&str], o| {
            script(&SecurityAction::Grant { privileges: p.iter().map(|s| s.to_string()).collect(), object: o, to: "lect".into(), grantable: false })
        };
        assert_eq!(g(&["READ"], None).unwrap(), "role grant-permission lect read \"\" --prefix");
        assert_eq!(g(&["read", "WRITE"], key("key", "/app/")).unwrap(), "role grant-permission lect readwrite /app/ --prefix");
        assert_eq!(g(&["WRITE"], key("key", "/app/x")).unwrap(), "role grant-permission lect write /app/x");
        assert_eq!(g(&["READ"], key("range", "a → c")).unwrap(), "role grant-permission lect read a c");
        assert!(g(&["ALL"], None).is_err());
        assert!(g(&[], None).is_err());
        assert!(matches!(script(&SecurityAction::SetLogin { name: "ana".into(), enabled: false }), Err(Error::Unsupported(_))));
        assert_eq!(
            script(&SecurityAction::Revoke { privileges: vec!["READ".into()], object: key("prefix", "/app/"), from: "lect".into() }).unwrap(),
            "role revoke-permission lect /app/ --prefix"
        );
        assert!(check_name("a\nb", "rol").is_err());
    }

    #[test]
    fn scripts_parse_back() {
        let text = script(&SecurityAction::CreateUser { name: "ana".into(), password: Some("a \"b\" --c".into()) }).unwrap();
        let c = &parse_script(&text).unwrap()[0];
        assert_eq!(c.name(), "user add");
        assert_eq!(password(c).as_deref(), Some("a \"b\" --c"));
        assert!(c.flags.is_empty());
        let c = &parse_script("role grant-permission lect read \"\" --prefix").unwrap()[0];
        assert_eq!(perm_range(c, 4).unwrap(), (vec![0], Some(vec![0])));
        let c = &parse_script("role revoke-permission lect /a/ --prefix").unwrap()[0];
        assert_eq!(perm_range(c, 3).unwrap(), (b"/a/".to_vec(), Some(b"/a0".to_vec())));
    }

    #[test]
    fn ranges_read_back() {
        assert_eq!(range_object(b"/a", b""), (Some("/a".into()), Some("key".into())));
        assert_eq!(range_object(&[0], &[0]), (None, None));
        assert_eq!(range_object(b"/a/", b"/a0"), (Some("/a/".into()), Some("prefix".into())));
        assert_eq!(range_object(b"a", &[0]), (Some("a".into()), Some("from_key".into())));
        assert_eq!(range_object(b"a", b"c"), (Some("a → c".into()), Some("range".into())));
        assert!(handles("role grant-permission") && !handles("user list"));
    }
}

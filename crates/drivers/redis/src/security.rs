//! Users and permissions through ACLs (Redis 6+, Valkey, Dragonfly;
//! docs/usuarios-y-permisos.md). Redis has users only, no roles: a user's
//! permissions are ACL rules — command rules (`+@read`, `-@dangerous`,
//! `+get`), key patterns (`~app:*`, `%R~ro:*`) and channel patterns
//! (`&news:*`). A privilege here is one such rule, sign included, so that
//! revoking one is its negation.
//!
//! Scripts are the editor's language: `ACL SETUSER …` lines.

use crate::command::quote_arg;
use crate::shape::text_of;
use crate::RedisSession;
use dbine_driver::keys::glob_escape;
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use redis::Value;

const NO_ROLES: &str = "Redis no tiene roles: los permisos se asignan a cada usuario con reglas ACL (ACL SETUSER).";

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "+@all", "+@read", "+@write", "+@keyspace", "+@string", "+@hash", "+@list", "+@set", "+@sortedset", "+@stream",
            "+@pubsub", "+@connection", "+@transaction", "+@scripting", "+@admin", "+@dangerous", "-@dangerous", "-@admin",
            "-@write", "~", "%R~", "%W~", "&",
        ],
        object_kinds: vec!["", "key"],
        create_user: true,
        create_role: false,
        passwords: true,
        membership: false,
        per_database: false,
    }
}

// -- reading -----------------------------------------------------------------

/// `ACL GETUSER`'s reply as field → value (RESP2 flat array or RESP3 map).
fn fields(v: Value) -> Vec<(String, Value)> {
    match v {
        Value::Map(m) => m.into_iter().map(|(k, v)| (text_of(&k), v)).collect(),
        Value::Array(a) => {
            let mut out = Vec::new();
            let mut it = a.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                out.push((text_of(&k), v));
            }
            out
        }
        _ => Vec::new(),
    }
}

fn items(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) | Value::Set(a) => a.iter().map(text_of).collect(),
        other => text_of(other).split_whitespace().map(str::to_string).collect(),
    }
}

/// A user as `ACL GETUSER` describes it.
#[derive(Debug, Default)]
struct AclUser {
    flags: Vec<String>,
    passwords: usize,
    commands: String,
    keys: String,
    channels: String,
    databases: String,
    /// Redis 7 selectors: (commands, keys, channels).
    selectors: Vec<(String, String, String)>,
}

fn acl_user(v: Value) -> AclUser {
    let mut u = AclUser::default();
    for (k, v) in fields(v) {
        match k.as_str() {
            "flags" => u.flags = items(&v),
            "passwords" => u.passwords = items(&v).iter().filter(|p| !p.is_empty()).count(),
            "commands" => u.commands = text_of(&v),
            "keys" => u.keys = items(&v).join(" "),
            "channels" => u.channels = items(&v).join(" "),
            "databases" => u.databases = text_of(&v),
            "selectors" => {
                if let Value::Array(sels) = v {
                    for s in sels {
                        let f = fields(s);
                        let get = |name: &str| f.iter().find(|(k, _)| k == name).map(|(_, v)| items(v).join(" ")).unwrap_or_default();
                        u.selectors.push((get("commands"), get("keys"), get("channels")));
                    }
                }
            }
            _ => {}
        }
    }
    u
}

impl AclUser {
    fn flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }
}

async fn get_user(s: &mut RedisSession, name: &str) -> Result<Option<AclUser>> {
    match s.run(&[b"ACL", b"GETUSER", name.as_bytes()]).await? {
        Value::Nil => Ok(None),
        v => Ok(Some(acl_user(v))),
    }
}

pub async fn principals(s: &mut RedisSession) -> Result<Vec<Principal>> {
    let names = items(&s.run(&[b"ACL", b"USERS"]).await?);
    let mut out = Vec::new();
    for name in names {
        let Some(u) = get_user(s, &name).await? else { continue };
        let on = u.flag("on");
        let mut details = vec![(
            "Contraseña".to_string(),
            if u.flag("nopass") {
                "sin contraseña (nopass)".to_string()
            } else {
                match u.passwords {
                    0 => "ninguna (no puede ingresar)".to_string(),
                    1 => "1 contraseña".to_string(),
                    n => format!("{n} contraseñas"),
                }
            },
        )];
        let rules = |s: &str| if s.trim().is_empty() { "(ninguno)".to_string() } else { s.to_string() };
        details.push(("Comandos".into(), rules(&u.commands)));
        details.push(("Claves".into(), rules(&keys_of(&u).join(" "))));
        details.push(("Canales".into(), rules(&channels_of(&u).join(" "))));
        if !u.databases.is_empty() {
            details.push(("Bases".into(), u.databases.clone()));
        }
        if !u.selectors.is_empty() {
            details.push(("Selectores".into(), u.selectors.len().to_string()));
        }
        let superuser = command_rules(&u.commands).iter().all(|r| !r.starts_with('-'))
            && (u.commands.split_whitespace().any(|r| r == "+@all") || u.flag("allcommands"))
            && keys_of(&u).iter().any(|k| k == "~*");
        out.push(Principal {
            system: name == "default",
            name,
            kind: PrincipalKind::User,
            can_login: Some(on),
            superuser: Some(superuser),
            disabled: Some(!on),
            member_of: Vec::new(),
            details,
        });
    }
    Ok(out)
}

/// Command rules without the implicit leading `-@all` Redis writes first.
fn command_rules(commands: &str) -> Vec<&str> {
    let mut r: Vec<&str> = commands.split_whitespace().collect();
    if r.first() == Some(&"-@all") && r.len() > 1 {
        r.remove(0);
    }
    r
}

fn dedup(v: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

/// Key patterns (`~p`, `%R~p`), including Redis 6's `allkeys` flag.
/// Dragonfly repeats patterns and lists `resetkeys`: dropped.
fn keys_of(u: &AclUser) -> Vec<String> {
    let mut k: Vec<String> = u.keys.split_whitespace().filter(|k| *k != "resetkeys").map(str::to_string).collect();
    if u.flag("allkeys") {
        k.insert(0, "~*".into());
    }
    dedup(k)
}

fn channels_of(u: &AclUser) -> Vec<String> {
    let mut c: Vec<String> = u.channels.split_whitespace().filter(|c| *c != "resetchannels").map(str::to_string).collect();
    if u.flag("allchannels") {
        c.insert(0, "&*".into());
    }
    dedup(c)
}

fn rule_grants(commands: &str, keys: &[String], channels: &[String], via: Option<String>, out: &mut Vec<Grant>) {
    for r in command_rules(commands) {
        out.push(Grant { privilege: r.to_string(), denied: r.starts_with('-'), via: via.clone(), ..Default::default() });
    }
    for k in keys {
        // `%R~p` / `%W~p` / `%RW~p` / `~p`.
        let (privilege, pattern) = match k.find('~') {
            Some(i) => (&k[..=i], &k[i + 1..]),
            None => (k.as_str(), ""),
        };
        out.push(Grant {
            privilege: privilege.to_string(),
            object: Some(pattern.to_string()),
            object_kind: Some("pattern".into()),
            via: via.clone(),
            ..Default::default()
        });
    }
    for c in channels {
        out.push(Grant {
            privilege: "&".into(),
            object: Some(c.trim_start_matches('&').to_string()),
            object_kind: Some("channel".into()),
            via: via.clone(),
            ..Default::default()
        });
    }
}

pub async fn grants(s: &mut RedisSession, principal: &str) -> Result<Vec<Grant>> {
    let u = get_user(s, principal).await?.ok_or_else(|| Error::Query(format!("No existe el usuario {principal}.")))?;
    let mut out = Vec::new();
    let mut commands = u.commands.clone();
    if u.flag("allcommands") && !commands.contains("+@all") {
        commands = format!("+@all {commands}");
    }
    rule_grants(&commands, &keys_of(&u), &channels_of(&u), None, &mut out);
    for (i, (c, k, ch)) in u.selectors.iter().enumerate() {
        let keys: Vec<String> = k.split_whitespace().map(str::to_string).collect();
        let chans: Vec<String> = ch.split_whitespace().filter(|c| *c != "resetchannels").map(str::to_string).collect();
        rule_grants(c, &keys, &chans, Some(format!("selector {}", i + 1)), &mut out);
    }
    Ok(out)
}

// -- scripts -----------------------------------------------------------------

fn check_name(name: &str) -> Result<&str> {
    let n = name.trim();
    if n.is_empty() {
        return Err(Error::Query("Escribí el nombre del usuario.".into()));
    }
    if n.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::Query(format!("El nombre de usuario {n:?} no puede tener espacios ni caracteres de control.")));
    }
    Ok(n)
}

fn check_password(pw: &str) -> Result<&str> {
    if pw.is_empty() {
        return Err(Error::Query("Escribí la contraseña.".into()));
    }
    if pw.chars().any(|c| c.is_control()) {
        return Err(Error::Query("La contraseña no puede tener caracteres de control.".into()));
    }
    Ok(pw)
}

fn is_word(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A glob pattern of keys or channels: no spaces or control characters.
fn check_pattern(p: &str) -> Result<&str> {
    if p.is_empty() || p.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::Query(format!("Patrón inválido: {p:?} (sin espacios ni caracteres de control).")));
    }
    Ok(p)
}

/// The pattern an object stands for: an explorer key literally (its glob
/// characters escaped), a pattern or channel as it is.
fn object_pattern(o: &ObjectRef) -> Result<String> {
    // The tab splits objects on the first '.' into schema and name.
    let name = match o.schema() {
        Some(s) => format!("{s}.{}", o.name),
        None => o.name.clone(),
    };
    Ok(match o.kind.as_str() {
        "pattern" | "channel" => check_pattern(&name)?.to_string(),
        _ => glob_escape(check_pattern(&name)?),
    })
}

/// One ACL rule for a privilege, with the object's pattern when it takes
/// one. Rejects anything that isn't a well-formed rule.
fn rule(privilege: &str, object: Option<&ObjectRef>) -> Result<String> {
    let p = privilege.trim();
    let bad = || Error::Query(format!("Regla ACL inválida: {p:?}. Ejemplos: +@read, -@dangerous, +get, +config|get, ~app:*, %R~, &canal*."));
    // Key and channel access: `~`, `%R~`, `%W~`, `%RW~`, `&`, with the
    // pattern after it or from the object.
    let key_prefix = ["%RW~", "%R~", "%W~", "~", "&"].into_iter().find(|k| p.get(..k.len()).is_some_and(|h| h.eq_ignore_ascii_case(k)));
    if let Some(kp) = key_prefix {
        let kp = kp.to_ascii_uppercase();
        let inline = &p[kp.len()..];
        let pattern = match (inline.is_empty(), object) {
            (false, None) => check_pattern(inline)?.to_string(),
            (true, Some(o)) => object_pattern(o)?,
            (true, None) => "*".to_string(),
            (false, Some(_)) => return Err(bad()),
        };
        return Ok(format!("{kp}{pattern}"));
    }
    if object.is_some() {
        return Err(Error::Unsupported(format!(
            "En Redis los comandos ({p}) se otorgan para todo el usuario, no sobre una clave. Para limitar las claves elegí ~, %R~ o %W~ sobre la clave."
        )));
    }
    let lower = p.to_ascii_lowercase();
    if matches!(lower.as_str(), "allkeys" | "allchannels" | "allcommands" | "nocommands" | "resetkeys" | "resetchannels") {
        return Ok(lower);
    }
    let (sign, body) = match p.chars().next() {
        Some(c @ ('+' | '-')) => (c, &p[1..]),
        _ => ('+', p),
    };
    let ok = match body.strip_prefix('@') {
        Some(cat) => is_word(cat),
        None => {
            let mut parts = body.splitn(2, '|');
            parts.next().is_some_and(is_word) && parts.next().is_none_or(is_word)
        }
    };
    if !ok {
        return Err(bad());
    }
    Ok(format!("{sign}{}", body.to_ascii_lowercase()))
}

/// The rule that undoes a granted one.
fn negate(privilege: &str, object: Option<&ObjectRef>) -> Result<String> {
    let r = rule(privilege, object)?;
    Ok(match r.as_str() {
        "allkeys" => "resetkeys".into(),
        "allchannels" => "resetchannels".into(),
        "allcommands" => "nocommands".into(),
        _ if r.starts_with('+') => format!("-{}", &r[1..]),
        _ if r.starts_with('-') => format!("+{}", &r[1..]),
        _ => {
            return Err(Error::Unsupported(format!(
                "Redis no permite quitar un patrón suelto ({r}): resetkeys / resetchannels borran todos los del usuario. Hacelo desde la consola y volvé a agregar los que quieras conservar."
            )))
        }
    })
}

fn setuser(name: &str, rules: &[String]) -> String {
    let mut s = format!("ACL SETUSER {}", quote_arg(name));
    for r in rules {
        s.push(' ');
        s.push_str(&quote_arg(r));
    }
    s
}

pub fn script(action: &SecurityAction) -> Result<String> {
    Ok(match action {
        SecurityAction::CreateUser { name, password } => {
            let name = check_name(name)?;
            let mut rules = vec!["on".to_string()];
            if let Some(pw) = password.as_deref().filter(|p| !p.is_empty()) {
                rules.push(format!(">{}", check_password(pw)?));
            }
            setuser(name, &rules)
        }
        SecurityAction::CreateRole { .. } | SecurityAction::AddMember { .. } | SecurityAction::RemoveMember { .. } => {
            return Err(Error::Unsupported(NO_ROLES.into()))
        }
        SecurityAction::Drop { name, kind } => {
            if *kind == PrincipalKind::Role {
                return Err(Error::Unsupported(NO_ROLES.into()));
            }
            format!("ACL DELUSER {}", quote_arg(check_name(name)?))
        }
        // `resetpass` first: the new one replaces every password (and nopass).
        SecurityAction::SetPassword { name, password } => {
            setuser(check_name(name)?, &["resetpass".into(), format!(">{}", check_password(password)?)])
        }
        SecurityAction::SetLogin { name, enabled } => setuser(check_name(name)?, &[if *enabled { "on" } else { "off" }.into()]),
        SecurityAction::Grant { privileges, object, to, grantable } => {
            if *grantable {
                return Err(Error::Unsupported("Redis no tiene la opción de otorgar permisos a otros: solo un usuario con +acl administra las ACL.".into()));
            }
            let rules = privileges.iter().map(|p| rule(p, object.as_ref())).collect::<Result<Vec<_>>>()?;
            if rules.is_empty() {
                return Err(Error::Query("Elegí al menos un permiso.".into()));
            }
            setuser(check_name(to)?, &rules)
        }
        SecurityAction::Revoke { privileges, object, from } => {
            let rules = privileges.iter().map(|p| negate(p, object.as_ref())).collect::<Result<Vec<_>>>()?;
            if rules.is_empty() {
                return Err(Error::Query("Elegí al menos un permiso.".into()));
            }
            setuser(check_name(from)?, &rules)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: "key".into(), schema: None, name: name.into() })
    }

    #[test]
    fn users_and_passwords() {
        let s = |a| script(&a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p w\"1".into()) }), r#"ACL SETUSER ana on ">p w\"1""#);
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: None }), "ACL SETUSER ana on");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }), "ACL SETUSER ana resetpass >x");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ACL SETUSER ana off");
        assert_eq!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "ACL DELUSER ana");
        assert!(script(&SecurityAction::CreateUser { name: "a b".into(), password: None }).is_err());
        assert!(script(&SecurityAction::SetPassword { name: "ana".into(), password: "".into() }).is_err());
        assert!(matches!(script(&SecurityAction::CreateRole { name: "r".into() }), Err(Error::Unsupported(_))));
        assert!(matches!(script(&SecurityAction::AddMember { role: "r".into(), member: "a".into() }), Err(Error::Unsupported(_))));
    }

    #[test]
    fn grants_and_revokes() {
        let g = |p: &[&str], o: Option<ObjectRef>| {
            script(&SecurityAction::Grant { privileges: p.iter().map(|s| s.to_string()).collect(), object: o, to: "ana".into(), grantable: false })
        };
        let r = |p: &[&str], o: Option<ObjectRef>| {
            script(&SecurityAction::Revoke { privileges: p.iter().map(|s| s.to_string()).collect(), object: o, from: "ana".into() })
        };
        assert_eq!(g(&["+@read", "-@dangerous", "GET", "config|get"], None).unwrap(), "ACL SETUSER ana +@read -@dangerous +get +config|get");
        assert_eq!(g(&["~"], None).unwrap(), "ACL SETUSER ana ~*");
        assert_eq!(g(&["%R~"], key("app:1*")).unwrap(), r#"ACL SETUSER ana "%R~app:1\\*""#);
        let parsed = crate::command::parse_line(&g(&["%R~"], key("app:1*")).unwrap()).unwrap();
        assert_eq!(parsed[3], br"%R~app:1\*".to_vec());
        let parsed = crate::command::parse_line(&script(&SecurityAction::SetPassword { name: "ana".into(), password: "a \"b\\ #c".into() }).unwrap()).unwrap();
        assert_eq!(parsed[4], br#">a "b\ #c"#.to_vec());
        assert_eq!(g(&["~app:*", "&news*"], None).unwrap(), "ACL SETUSER ana ~app:* &news*");
        assert_eq!(g(&["allkeys"], None).unwrap(), "ACL SETUSER ana allkeys");
        // A key with a dot comes split by the tab.
        assert_eq!(g(&["~"], Some(ObjectRef { kind: "key".into(), schema: Some("a".into()), name: "b".into() })).unwrap(), "ACL SETUSER ana ~a.b");
        for bad in ["+@read ~*", "+get\"", ">pw", "+@", "~ñ x", "ñ", "~a b", "", "(+get)", "+get|", "#x"] {
            assert!(g(&[bad], None).is_err(), "{bad}");
        }
        assert!(matches!(g(&["+@read"], key("k")), Err(Error::Unsupported(_))));
        assert!(script(&SecurityAction::Grant { privileges: vec!["+@read".into()], object: None, to: "ana".into(), grantable: true }).is_err());

        assert_eq!(r(&["+@read", "-@dangerous", "set"], None).unwrap(), "ACL SETUSER ana -@read +@dangerous -set");
        assert_eq!(r(&["allkeys"], None).unwrap(), "ACL SETUSER ana resetkeys");
        let pattern = Some(ObjectRef { kind: "pattern".into(), schema: None, name: "app:*".into() });
        assert!(matches!(r(&["~"], pattern), Err(Error::Unsupported(_))));
    }

    #[test]
    fn getuser_replies() {
        let b = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let arr = |v: Vec<Value>| Value::Array(v);
        let v = arr(vec![
            b("flags"),
            arr(vec![b("on"), b("sanitize-payload")]),
            b("passwords"),
            arr(vec![b("abc")]),
            b("commands"),
            b("-@all +@read -@dangerous +set"),
            b("keys"),
            b("~app:* %R~ro:*"),
            b("channels"),
            b("resetchannels &ch* &ch*"),
            b("selectors"),
            arr(vec![arr(vec![b("commands"), b("-@all +get"), b("keys"), b("~sel:*"), b("channels"), b("")])]),
        ]);
        let u = acl_user(v);
        assert!(u.flag("on"));
        assert_eq!(u.passwords, 1);
        assert_eq!(keys_of(&u), ["~app:*", "%R~ro:*"]);
        assert_eq!(channels_of(&u), ["&ch*"]);
        let mut g = Vec::new();
        rule_grants(&u.commands, &keys_of(&u), &channels_of(&u), None, &mut g);
        let names: Vec<_> = g.iter().map(|g| (g.privilege.as_str(), g.object.as_deref(), g.denied)).collect();
        assert_eq!(
            names,
            [
                ("+@read", None, false),
                ("-@dangerous", None, true),
                ("+set", None, false),
                ("~", Some("app:*"), false),
                ("%R~", Some("ro:*"), false),
                ("&", Some("ch*"), false)
            ]
        );
        assert_eq!(u.selectors, [("-@all +get".to_string(), "~sel:*".to_string(), String::new())]);
        // Revoking a listed grant negates it.
        let pattern = ObjectRef { kind: "pattern".into(), schema: None, name: "app:*".into() };
        assert!(negate("~", Some(&pattern)).is_err());
        assert_eq!(negate("-@dangerous", None).unwrap(), "+@dangerous");
    }
}

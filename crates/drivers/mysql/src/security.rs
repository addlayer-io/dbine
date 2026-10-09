//! Users, roles and permissions (docs/users-and-permissions.md) for MySQL 8
//! (and Aurora / Cloud SQL), MariaDB and TiDB.
//!
//! Accounts are `'user'@'host'`; DBine names a user `user@host` and a role
//! just `role` (MySQL and TiDB roles live at host `%`, MariaDB's have no
//! host). [`account`] turns a name back into the account: a name without
//! `@` is `'name'`, which MySQL reads as `'name'@'%'` and MariaDB as the
//! role. Grants are read from `SHOW GRANTS`, which every one of them has and
//! which works for roles too; the roles a principal holds come from its
//! `GRANT role TO …` lines, and their grants are followed recursively.

use crate::session::{at, lit, MySqlSession};
use crate::Variant;
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::{HashSet, VecDeque};

// Engines with their own catalogs and GRANT dialects.
mod common;
mod databend;
mod doris;
mod singlestore;
mod starrocks;

/// The variants with users, roles and `GRANT` (VeloDB goes as Doris).
pub fn supported(v: Variant) -> bool {
    matches!(
        v,
        Variant::MySql
            | Variant::AuroraMySql
            | Variant::CloudSqlMySql
            | Variant::MariaDb
            | Variant::TiDb
            | Variant::OceanBase
            | Variant::SingleStore
            | Variant::StarRocks
            | Variant::Doris
            | Variant::VeloDb
            | Variant::Databend
    )
}

/// What each engine offers; MySQL's for MySQL, MariaDB, TiDB and OceanBase.
pub fn spec_for(v: Variant) -> SecuritySpec {
    match v.base() {
        Variant::StarRocks => starrocks::spec(),
        Variant::Doris => doris::spec(),
        Variant::Databend => databend::spec(),
        Variant::SingleStore => singlestore::spec(),
        _ => spec(),
    }
}

/// The script for a change, in the engine's dialect.
pub fn script_for(v: Variant, a: &SecurityAction) -> Result<String> {
    match v.base() {
        Variant::StarRocks => starrocks::script(a),
        Variant::Doris => doris::script(a),
        Variant::Databend => databend::script(a),
        Variant::SingleStore => singlestore::script(a),
        _ => script(a),
    }
}

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "CREATE", "ALTER", "DROP", "INDEX", "REFERENCES",
            "CREATE VIEW", "SHOW VIEW", "CREATE ROUTINE", "ALTER ROUTINE", "TRIGGER", "EVENT", "LOCK TABLES",
            "CREATE TEMPORARY TABLES", "ALL PRIVILEGES", "PROCESS", "RELOAD", "SHOW DATABASES", "CREATE USER",
        ],
        // "" = every database (`*.*`); "schema" = one database (`db.*`).
        object_kinds: vec!["", "schema", "table", "view", "procedure", "function"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

// -- names -----------------------------------------------------------------

/// A role's name in DBine: bare when it's at host `%` (MySQL, TiDB) or has
/// no host (MariaDB).
fn role_name(user: &str, host: &str) -> String {
    if host.is_empty() || host == "%" {
        user.to_string()
    } else {
        format!("{user}@{host}")
    }
}

/// `user@host` (or a bare role name) as the account in SQL.
pub fn account(name: &str) -> String {
    match name.rsplit_once('@') {
        Some((u, h)) => format!("{}@{}", lit(u), lit(h)),
        None => lit(name),
    }
}

/// A new user's account: the host defaults to `%` (anywhere).
fn new_user(name: &str) -> String {
    if name.contains('@') {
        account(name)
    } else {
        format!("{}@'%'", lit(name))
    }
}

// -- reading -----------------------------------------------------------------

fn yes(v: Option<String>) -> bool {
    matches!(v.as_deref().map(str::trim), Some("Y" | "y" | "1" | "true"))
}

fn is_system(user: &str) -> bool {
    user == "root" || user.starts_with("mysql.") || user.starts_with("mariadb.")
}

/// Accounts: user, host, locked, is role, plugin, super, password expired.
fn accounts_query(v: Variant) -> &'static str {
    if v == Variant::MariaDb {
        // 10.4+: the lock is in global_priv's JSON.
        "SELECT u.User, u.Host, IFNULL(JSON_VALUE(g.Priv, '$.account_locked'), 0), u.is_role, u.plugin, u.Super_priv, u.password_expired
           FROM mysql.user u LEFT JOIN mysql.global_priv g ON g.User = u.User AND g.Host = u.Host
          ORDER BY u.is_role, u.User, u.Host"
    } else {
        // A role is an account CREATE ROLE left locked, expired and without password.
        "SELECT User, Host, account_locked,
                IF(account_locked = 'Y' AND password_expired = 'Y' AND IFNULL(authentication_string, '') = '', 'Y', 'N'),
                plugin, Super_priv, password_expired
           FROM mysql.user ORDER BY 4, User, Host"
    }
}

/// An account from `SELECT * FROM mysql.user` in `accounts_query`'s
/// order, by column name; a role is what CREATE ROLE leaves (locked,
/// expired, no password) unless the engine says (`is_role`).
fn loose_account(r: &mysql_async::Row) -> Vec<Option<String>> {
    let col = |n: &str| crate::session::named(r, &[n]);
    let locked = col("account_locked");
    let role = col("is_role").unwrap_or_else(|| {
        let r = yes(locked.clone()) && yes(col("password_expired")) && col("authentication_string").unwrap_or_default().is_empty();
        (if r { "Y" } else { "N" }).into()
    });
    vec![col("User"), col("Host"), locked, Some(role), col("plugin"), col("Super_priv"), col("password_expired")]
}

/// Role memberships: member user, member host, role user, role host.
fn memberships_query(v: Variant) -> &'static str {
    if v == Variant::MariaDb {
        "SELECT User, Host, Role, '' FROM mysql.roles_mapping"
    } else {
        "SELECT TO_USER, TO_HOST, FROM_USER, FROM_HOST FROM mysql.role_edges"
    }
}

pub async fn principals(s: &mut MySqlSession) -> Result<Vec<Principal>> {
    let v = s.variant;
    match v {
        Variant::StarRocks => return starrocks::principals(s).await,
        Variant::Doris => return doris::principals(s).await,
        Variant::Databend => return databend::principals(s).await,
        Variant::SingleStore => return singlestore::principals(s).await,
        _ => {}
    }
    let cells = |rows: Vec<mysql_async::Row>| rows.iter().map(|r| (0..7).map(|i| at(r, i)).collect::<Vec<_>>()).collect::<Vec<_>>();
    let rows = match s.rows(accounts_query(v)).await {
        Ok(r) => cells(r),
        Err(_) if v == Variant::MariaDb => cells(
            s
                // Before 10.4: no global_priv, no account lock.
                .rows("SELECT User, Host, 'N', is_role, plugin, Super_priv, password_expired FROM mysql.user ORDER BY 4, User, Host")
                .await
                .unwrap_or_default(),
        ),
        // OceanBase and other look-alikes: whichever of those columns
        // their mysql.user has.
        Err(_) => s.optional_rows("SELECT * FROM mysql.user").await.iter().map(loose_account).collect(),
    };
    if rows.is_empty() {
        // Without SELECT on mysql.user only the own account is visible.
        let me = s.rows("SELECT CURRENT_USER()").await?;
        let name = me.first().and_then(|r| at(r, 0)).unwrap_or_default();
        return Ok(vec![Principal {
            name,
            kind: PrincipalKind::User,
            can_login: Some(true),
            details: vec![("Nota".into(), "Sin permiso de lectura sobre mysql.user: se ve solo la cuenta propia".into())],
            ..Default::default()
        }]);
    }
    let mut raw: Vec<(String, String)> = Vec::new();
    let mut out: Vec<Principal> = Vec::new();
    for r in &rows {
        let at = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten();
        let user = at(r, 0).unwrap_or_default();
        let host = at(r, 1).unwrap_or_default();
        let role = yes(at(r, 3));
        let mut details = vec![("Host".into(), if host.is_empty() { "—".into() } else { host.clone() })];
        if !role {
            if let Some(p) = at(r, 4).filter(|p| !p.is_empty()) {
                details.push(("Autenticación".into(), p));
            }
            if yes(at(r, 6)) {
                details.push(("Contraseña".into(), "vencida".into()));
            }
        }
        out.push(Principal {
            name: if role { role_name(&user, &host) } else { format!("{user}@{host}") },
            kind: if role { PrincipalKind::Role } else { PrincipalKind::User },
            can_login: (!role).then_some(true),
            superuser: Some(yes(at(r, 5))),
            disabled: (!role).then(|| yes(at(r, 2))),
            member_of: Vec::new(),
            details,
            system: is_system(&user),
        });
        raw.push((user, host));
    }
    for m in s.optional_rows(memberships_query(v)).await {
        let (mu, mh) = (at(&m, 0).unwrap_or_default(), at(&m, 1).unwrap_or_default());
        let (ru, rh) = (at(&m, 2).unwrap_or_default(), at(&m, 3).unwrap_or_default());
        let role = role_name(&ru, &rh);
        if let Some(i) = raw.iter().position(|(u, h)| *u == mu && *h == mh) {
            if !out[i].member_of.contains(&role) {
                out[i].member_of.push(role);
            }
        }
    }
    Ok(out)
}

pub async fn grants(s: &mut MySqlSession, principal: &str) -> Result<Vec<Grant>> {
    match s.variant {
        Variant::StarRocks => return starrocks::grants(s, principal).await,
        Variant::Doris => return doris::grants(s, principal).await,
        Variant::Databend => return databend::grants(s, principal).await,
        Variant::SingleStore => return singlestore::grants(s, principal).await,
        _ => {}
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::from([(principal.to_string(), None)]);
    while let Some((name, via)) = queue.pop_front() {
        if !seen.insert(name.clone()) || seen.len() > 64 {
            continue;
        }
        let sql = format!("SHOW GRANTS FOR {}", account(&name));
        let rows = match s.rows(&sql).await {
            Ok(r) => r,
            Err(e) if via.is_none() => return Err(e),
            Err(_) => continue,
        };
        for r in &rows {
            match parse_grant(&at(r, 0).unwrap_or_default()) {
                Some(Line::Privileges { privileges, object, object_kind, grantable }) => {
                    for p in privileges {
                        if p == "USAGE" || p == "PROXY" {
                            continue;
                        }
                        out.push(Grant { privilege: p, object: object.clone(), object_kind: object_kind.clone(), grantable, denied: false, via: via.clone() });
                    }
                }
                Some(Line::Roles(roles)) => {
                    for r in roles {
                        let v = via.clone().unwrap_or_else(|| r.clone());
                        queue.push_back((r, Some(v)));
                    }
                }
                None => {}
            }
        }
    }
    Ok(out)
}

// -- SHOW GRANTS parsing -----------------------------------------------------

#[derive(Debug, PartialEq)]
enum Line {
    Privileges { privileges: Vec<String>, object: Option<String>, object_kind: Option<String>, grantable: bool },
    Roles(Vec<String>),
}

/// Positions of `kw` outside quotes, backticks and parentheses.
fn top_level(s: &str, kw: &str) -> Vec<usize> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let (mut quote, mut depth) = (0u8, 0i32);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == quote {
                if b.get(i + 1) == Some(&quote) {
                    i += 1;
                } else {
                    quote = 0;
                }
            } else if c == b'\\' && quote != b'`' {
                i += 1;
            }
        } else {
            match c {
                b'\'' | b'"' | b'`' => quote = c,
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if depth == 0 && s[i..].starts_with(kw) => out.push(i),
                _ => {}
            }
        }
        i += 1;
    }
    out
}

fn split_top<'a>(s: &'a str, sep: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut from = 0;
    for i in top_level(s, sep) {
        out.push(s[from..i].trim());
        from = i + sep.len();
    }
    out.push(s[from..].trim());
    out
}

/// `` `a``b` ``, `'a'`, `"a"` or `a` without its quotes.
fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['`', '\'', '"'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            let d = format!("{q}{q}");
            return s[1..s.len() - 1].replace(&d, &q.to_string());
        }
    }
    s.to_string()
}

/// `` `role`@`%` `` in a role grant, as DBine names the role.
fn parse_role(s: &str) -> String {
    let parts = split_top(s, "@");
    match parts.as_slice() {
        [u, h] => role_name(&unquote(u), &unquote(h)),
        _ => unquote(s),
    }
}

/// One line of `SHOW GRANTS`.
fn parse_grant(line: &str) -> Option<Line> {
    let body = line.trim().strip_prefix("GRANT ")?;
    let to = *top_level(body, " TO ").first()?;
    let on = top_level(body, " ON ").into_iter().find(|&i| i < to);
    let tail = &body[to..];
    let Some(on) = on else {
        return Some(Line::Roles(split_top(&body[..to], ",").into_iter().filter(|r| !r.is_empty()).map(parse_role).collect()));
    };
    let privileges = split_top(&body[..on], ",")
        .into_iter()
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('(') {
            // SELECT (`a`, `b`): column privileges.
            Some((name, cols)) => {
                let cols: Vec<String> = split_top(cols.trim_end_matches(')'), ",").into_iter().map(unquote).collect();
                format!("{} ({})", name.trim(), cols.join(", "))
            }
            None => p.to_string(),
        })
        .collect();
    let mut target = body[on + 4..to].trim();
    let mut kind = "table";
    for (prefix, k) in [("PROCEDURE ", "procedure"), ("FUNCTION ", "function"), ("TABLE ", "table")] {
        if let Some(rest) = target.strip_prefix(prefix) {
            target = rest.trim();
            kind = k;
        }
    }
    let parts: Vec<String> = split_top(target, ".").into_iter().map(unquote).collect();
    let (object, object_kind) = match parts.as_slice() {
        [db, t] if db == "*" && t == "*" => (None, None),
        [db, t] if t == "*" => (Some(db.clone()), Some("schema".to_string())),
        [db, t] => (Some(format!("{db}.{t}")), Some(kind.to_string())),
        [t] if t == "*" => (None, None),
        _ => (Some(target.to_string()), Some(kind.to_string())),
    };
    Some(Line::Privileges { privileges, object, object_kind, grantable: tail.contains("WITH GRANT OPTION") })
}

// -- scripts -----------------------------------------------------------------

fn q(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// What a privilege applies to: every database, one database or an object.
fn on(object: &Option<ObjectRef>) -> String {
    match object {
        None => "*.*".into(),
        Some(o) if o.kind == "schema" || o.kind == "database" => format!("{}.*", q(&o.name)),
        Some(o) => {
            let prefix = match o.kind.as_str() {
                "procedure" => "PROCEDURE ",
                "function" => "FUNCTION ",
                _ => "",
            };
            match o.schema() {
                Some(sc) => format!("{prefix}{}.{}", q(sc), q(&o.name)),
                None => format!("{prefix}{}", q(&o.name)),
            }
        }
    }
}

/// Privilege names: letters, spaces and underscores, optionally with a
/// column list (`SELECT (a, b)`).
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let (name, cols) = match x.split_once('(') {
            Some((n, c)) => (n.trim(), Some(c.trim().strip_suffix(')').ok_or_else(|| bad(x))?)),
            None => (x.trim(), None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '_') {
            return Err(bad(x));
        }
        let name = name.to_uppercase();
        match cols {
            Some(c) => {
                let cols: Vec<String> = c.split(',').map(|c| unquote(c.trim())).filter(|c| !c.is_empty()).map(|c| q(&c)).collect();
                if cols.is_empty() {
                    return Err(bad(x));
                }
                out.push(format!("{name} ({})", cols.join(", ")));
            }
            None => out.push(name),
        }
    }
    Ok(out.join(", "))
}

fn bad(x: &str) -> Error {
    Error::Query(format!("«{x}» no es un permiso de MySQL"))
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} IDENTIFIED BY {};", new_user(name), lit(pw))
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", account(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", account(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", account(name)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} IDENTIFIED BY {};", account(name), lit(password)),
        SecurityAction::SetLogin { name, enabled } => {
            format!("ALTER USER {} ACCOUNT {};", account(name), if *enabled { "UNLOCK" } else { "LOCK" })
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            format!("GRANT {} ON {} TO {}{};", privileges(p)?, on(object), account(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(object), account(from)),
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", account(role), account(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", account(role), account(member)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w\\".into()) }), "CREATE USER 'ana'@'%' IDENTIFIED BY 'p''w\\\\';");
        assert_eq!(s(SecurityAction::CreateUser { name: "ana@localhost".into(), password: Some("x".into()) }), "CREATE USER 'ana'@'localhost' IDENTIFIED BY 'x';");
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "lectores".into() }), "CREATE ROLE 'lectores';");
        assert_eq!(s(SecurityAction::Drop { name: "ana@%".into(), kind: PrincipalKind::User }), "DROP USER 'ana'@'%';");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana@%".into(), enabled: false }), "ALTER USER 'ana'@'%' ACCOUNT LOCK;");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana@%".into(), password: "n".into() }), "ALTER USER 'ana'@'%' IDENTIFIED BY 'n';");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "UPDATE".into()], object: obj("table", Some("ventas"), "fac`t"), to: "ana@%".into(), grantable: true }),
            "GRANT SELECT, UPDATE ON `ventas`.`fac``t` TO 'ana'@'%' WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["SELECT (id, `total`)".into()], object: obj("table", None, "f"), to: "r".into(), grantable: false }),
            "GRANT SELECT (`id`, `total`) ON `f` TO 'r';"
        );
        assert_eq!(s(SecurityAction::Grant { privileges: vec!["PROCESS".into()], object: None, to: "r".into(), grantable: false }), "GRANT PROCESS ON *.* TO 'r';");
        assert_eq!(s(SecurityAction::Revoke { privileges: vec!["INSERT".into()], object: obj("schema", None, "ventas"), from: "r".into() }), "REVOKE INSERT ON `ventas`.* FROM 'r';");
        assert_eq!(s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: obj("procedure", Some("db"), "p"), from: "r".into() }), "REVOKE EXECUTE ON PROCEDURE `db`.`p` FROM 'r';");
        assert_eq!(s(SecurityAction::AddMember { role: "lectores".into(), member: "ana@%".into() }), "GRANT 'lectores' TO 'ana'@'%';");
        assert_eq!(s(SecurityAction::RemoveMember { role: "lectores".into(), member: "ana@%".into() }), "REVOKE 'lectores' FROM 'ana'@'%';");
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: None, to: "a".into(), grantable: false }).is_err());
    }

    #[test]
    fn parses_show_grants() {
        let p = |l: &str| parse_grant(l).unwrap();
        assert_eq!(
            p("GRANT SELECT (`Host`), UPDATE ON `mysql`.`user` TO `rx`@`%`"),
            Line::Privileges { privileges: vec!["SELECT (Host)".into(), "UPDATE".into()], object: Some("mysql.user".into()), object_kind: Some("table".into()), grantable: false }
        );
        assert_eq!(
            p("GRANT SELECT ON `test`.* TO 'ux'@'%' WITH GRANT OPTION"),
            Line::Privileges { privileges: vec!["SELECT".into()], object: Some("test".into()), object_kind: Some("schema".into()), grantable: true }
        );
        assert_eq!(
            p("GRANT USAGE ON *.* TO `ux`@`%` IDENTIFIED BY PASSWORD '*66'"),
            Line::Privileges { privileges: vec!["USAGE".into()], object: None, object_kind: None, grantable: false }
        );
        assert_eq!(
            p("GRANT EXECUTE ON PROCEDURE `sys`.`ps_setup` TO `ux`@`%`"),
            Line::Privileges { privileges: vec!["EXECUTE".into()], object: Some("sys.ps_setup".into()), object_kind: Some("procedure".into()), grantable: false }
        );
        assert_eq!(p("GRANT `rx`@`%`,`ry`@`localhost` TO `ux`@`%`"), Line::Roles(vec!["rx".into(), "ry@localhost".into()]));
        assert_eq!(p("GRANT `rx` TO `ux`@`%`"), Line::Roles(vec!["rx".into()]));
        assert_eq!(p("GRANT 'rx'@'%' TO 'ux'@'%'"), Line::Roles(vec!["rx".into()]));
        // " TO " inside a quoted name isn't the keyword.
        assert_eq!(
            p("GRANT SELECT ON `a TO b`.* TO `u`@`%`"),
            Line::Privileges { privileges: vec!["SELECT".into()], object: Some("a TO b".into()), object_kind: Some("schema".into()), grantable: false }
        );
    }
}

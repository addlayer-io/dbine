//! Naming and validation shared by the engines whose statements name the
//! grantee's kind (`TO USER …` / `TO ROLE …`): StarRocks, Doris, Databend
//! and SingleStore. Their roles are listed as `role:<name>` (SingleStore's
//! groups as `group:<name>`), so a script knows what it's talking to.

use dbine_driver::{Error, Result};

pub const ROLE: &str = "role:";
pub const GROUP: &str = "group:";

/// Who a principal name refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who<'a> {
    User(&'a str),
    Role(&'a str),
    Group(&'a str),
}

pub fn who(name: &str) -> Who<'_> {
    if let Some(r) = name.strip_prefix(ROLE) {
        Who::Role(r)
    } else if let Some(g) = name.strip_prefix(GROUP) {
        Who::Group(g)
    } else {
        Who::User(name)
    }
}

/// A name typed for a new role, with or without the prefix.
pub fn bare_role(name: &str) -> &str {
    name.strip_prefix(ROLE).unwrap_or(name)
}

pub fn role(name: &str) -> String {
    format!("{ROLE}{name}")
}

/// A name in backticks.
pub fn q(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Privilege names: letters, digits, spaces and underscores, upper-cased;
/// anything else (`;`, quotes, parentheses) is rejected.
pub fn privileges(p: &[String], engine: &str) -> Result<Vec<String>> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    p.iter()
        .map(|x| {
            let name = x.split_whitespace().collect::<Vec<_>>().join(" ");
            if name.is_empty() || !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_') {
                return Err(Error::Query(format!("«{x}» no es un permiso de {engine}")));
            }
            Ok(name.to_uppercase())
        })
        .collect()
}

pub fn unsupported(msg: &str) -> Error {
    Error::Unsupported(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_privileges() {
        assert_eq!(who("role:lect"), Who::Role("lect"));
        assert_eq!(who("group:g"), Who::Group("g"));
        assert_eq!(who("ana@%"), Who::User("ana@%"));
        assert_eq!(bare_role("role:x"), "x");
        assert_eq!(q("a`b"), "`a``b`");
        assert_eq!(privileges(&["select".into(), " create  table ".into()], "X").unwrap(), vec!["SELECT", "CREATE TABLE"]);
        for bad in ["SELECT; DROP TABLE x", "SELECT (a)", "'x'", "", "1X"] {
            assert!(privileges(&[bad.into()], "X").is_err(), "{bad}");
        }
    }
}

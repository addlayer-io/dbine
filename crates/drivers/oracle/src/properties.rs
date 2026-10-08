//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]).
//!
//! A "database" here is a schema, that is, a user account: its facts come
//! from `DBA_USERS` (status, created, last login, authentication),
//! `DBA_SEGMENTS` (size) and `DBA_TS_QUOTAS` (quotas and what they use),
//! and `ALTER USER` changes its default and temporary tablespace, its
//! quota on each tablespace, its profile and whether the account is
//! locked. It needs the DBA views (SELECT_CATALOG_ROLE or a DBA) and the
//! ALTER USER privilege.
//!
//! Each change is one `ALTER USER`; locking the account goes last.

use crate::create_db::{quota, tablespace};
use crate::{err, quote, OracleSession};
use dbine_driver::serde_static::intern;
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use std::collections::BTreeMap;

fn yes(v: &str) -> bool {
    matches!(v.trim(), "true" | "1" | "ON" | "on")
}

/// A quota field's key: `quota:<tablespace>`.
fn quota_key(ts: &str) -> String {
    format!("quota:{ts}")
}

/// `-1` is UNLIMITED; otherwise bytes, in the largest unit that divides them.
fn shown_quota(max_bytes: i64) -> String {
    if max_bytes < 0 {
        return "UNLIMITED".into();
    }
    for (unit, size) in [("T", 1i64 << 40), ("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if max_bytes >= size && max_bytes % size == 0 {
            return format!("{}{unit}", max_bytes / size);
        }
    }
    max_bytes.to_string()
}

/// `1.5 GB`.
fn human(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// The statements for `changes`: tablespaces, quotas and profile; unlock
/// first, lock last.
pub(crate) fn alter(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if database.trim().is_empty() {
        return Err(Error::Query("Falta el nombre del esquema.".into()));
    }
    let user = format!("ALTER USER {}", quote(database));
    let (mut first, mut out, mut quotas, mut last) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "default_tablespace" => out.push(format!("{user} DEFAULT TABLESPACE {}", tablespace(value)?)),
            "temporary_tablespace" => out.push(format!("{user} TEMPORARY TABLESPACE {}", tablespace(value)?)),
            "profile" => {
                let p = if value.eq_ignore_ascii_case("DEFAULT") { "DEFAULT".to_string() } else { tablespace(value)? };
                out.push(format!("{user} PROFILE {p}"));
            }
            "account_locked" => {
                if yes(value) {
                    last.push(format!("{user} ACCOUNT LOCK"));
                } else {
                    first.push(format!("{user} ACCOUNT UNLOCK"));
                }
            }
            k => match k.strip_prefix("quota:").filter(|ts| !ts.is_empty()) {
                // Empty: no quota (0) on that tablespace.
                Some(ts) => quotas.push(format!(
                    "{user} QUOTA {} ON {}",
                    if value.is_empty() { "0".to_string() } else { quota(value)? },
                    quote(ts)
                )),
                None => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
            },
        }
    }
    Ok(first.into_iter().chain(out).chain(quotas).chain(last).collect())
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(database, changes)?.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"))
}

const USER: &str = "SELECT account_status, TO_CHAR(created, 'YYYY-MM-DD HH24:MI:SS'), default_tablespace, temporary_tablespace, profile,
        authentication_type, TO_CHAR(lock_date, 'YYYY-MM-DD HH24:MI:SS'), TO_CHAR(expiry_date, 'YYYY-MM-DD HH24:MI:SS')
   FROM dba_users WHERE username = :1";

/// The figures that need no row type: text columns by position.
fn texts(row: &oracledb::Row, n: usize) -> Result<Vec<Option<String>>> {
    (0..n).map(|i| row.get::<Option<String>>(i).map_err(err)).collect()
}

impl OracleSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let name = database.to_string();
        let (user, last_login, segments, objects, quotas, profiles) = self
            .run(move |c| {
                let user = match c.query_row(USER, &[&name]) {
                    Ok(r) => texts(&r, 8)?,
                    Err(e) if crate::db_code(&e) == Some(942) => {
                        return Err(Error::Query(
                            "Ver las propiedades de un esquema necesita leer DBA_USERS (SELECT_CATALOG_ROLE o un usuario DBA).".into(),
                        ))
                    }
                    Err(e) => return Err(Error::Query(format!("no existe el esquema «{name}» ({})", err(e)))),
                };
                // 12c+.
                let last_login = c
                    .query_row("SELECT TO_CHAR(last_login, 'YYYY-MM-DD HH24:MI:SS') FROM dba_users WHERE username = :1", &[&name])
                    .and_then(|r| r.get::<Option<String>>(0))
                    .unwrap_or(None);
                let segments = c
                    .query_row("SELECT TO_CHAR(NVL(SUM(bytes), 0)), TO_CHAR(COUNT(*)) FROM dba_segments WHERE owner = :1", &[&name])
                    .map_err(err)
                    .and_then(|r| texts(&r, 2))
                    .unwrap_or_default();
                let objects = c
                    .query_row(
                        "SELECT TO_CHAR(COUNT(*)), TO_CHAR(COUNT(CASE WHEN object_type = 'TABLE' THEN 1 END)) FROM dba_objects WHERE owner = :1",
                        &[&name],
                    )
                    .map_err(err)
                    .and_then(|r| texts(&r, 2))
                    .unwrap_or_default();
                let mut quotas = Vec::new();
                for row in c
                    .query("SELECT tablespace_name, TO_CHAR(bytes), TO_CHAR(max_bytes) FROM dba_ts_quotas WHERE username = :1 ORDER BY 1", &[&name])
                    .map_err(err)?
                {
                    quotas.push(texts(&row.map_err(err)?, 3)?);
                }
                let profiles = c.query("SELECT DISTINCT profile FROM dba_profiles ORDER BY 1", &[]).map_err(err).and_then(crate::strings).unwrap_or_default();
                Ok((user, last_login, segments, objects, quotas, profiles))
            })
            .await?;
        let get = |i: usize| user.get(i).cloned().flatten().unwrap_or_default();
        let at = |v: &Vec<Option<String>>, i: usize| v.get(i).cloned().flatten().unwrap_or_default();

        let mut info = vec![
            PropertyInfo { group: String::new(), label: "Estado de la cuenta".into(), value: get(0) },
            PropertyInfo { group: String::new(), label: "Creado".into(), value: get(1) },
            PropertyInfo { group: String::new(), label: "Autenticación".into(), value: get(5) },
        ];
        if let Some(l) = last_login {
            info.push(PropertyInfo { group: String::new(), label: "Último inicio de sesión".into(), value: l });
        }
        for (i, label) in [(6, "Bloqueada desde"), (7, "Contraseña vence")] {
            if !get(i).is_empty() {
                info.push(PropertyInfo { group: String::new(), label: label.into(), value: get(i) });
            }
        }
        if !segments.is_empty() {
            info.push(PropertyInfo { group: String::new(), label: "Tamaño (segmentos)".into(), value: human(at(&segments, 0).parse().unwrap_or(0)) });
            info.push(PropertyInfo { group: String::new(), label: "Segmentos".into(), value: at(&segments, 1) });
        }
        if !objects.is_empty() {
            info.push(PropertyInfo { group: String::new(), label: "Objetos".into(), value: at(&objects, 0) });
            info.push(PropertyInfo { group: String::new(), label: "Tablas".into(), value: at(&objects, 1) });
        }

        let mut values = BTreeMap::new();
        values.insert("default_tablespace".to_string(), get(2));
        values.insert("temporary_tablespace".to_string(), get(3));
        values.insert("profile".to_string(), get(4));
        values.insert("account_locked".to_string(), if get(0).contains("LOCKED") { "true".into() } else { String::new() });
        let mut fields = vec![
            Field::new("default_tablespace", "Tablespace por defecto", FieldKind::Text)
                .help("Donde van las tablas e índices nuevos del esquema.")
                .group("Almacenamiento"),
            Field::new("temporary_tablespace", "Tablespace temporal", FieldKind::Text).group("Almacenamiento"),
            Field::new("profile", "Perfil (PROFILE)", FieldKind::Text).help("Límites de recursos y reglas de contraseña.").group("Cuenta"),
            Field::new("account_locked", "Cuenta bloqueada (ACCOUNT LOCK)", FieldKind::Bool).group("Cuenta"),
        ];

        // A quota field per tablespace with one, and the default tablespace.
        let mut with_quota: Vec<(String, String, Option<String>)> = quotas
            .iter()
            .map(|q| {
                let max: i64 = at(q, 2).parse().unwrap_or(0);
                (at(q, 0), shown_quota(max), Some(human(at(q, 1).parse().unwrap_or(0))))
            })
            .filter(|q| !q.0.is_empty())
            .collect();
        if !get(2).is_empty() && !with_quota.iter().any(|q| q.0 == get(2)) {
            with_quota.insert(0, (get(2), String::new(), None));
        }
        for (ts, max, used) in with_quota {
            let key = quota_key(&ts);
            fields.push(
                Field::new(intern(&key), intern(&format!("Cuota en {ts}")), FieldKind::Text)
                    .placeholder("UNLIMITED, 500M o 10G")
                    .help("Vacía: sin cuota (0).")
                    .group("Cuotas"),
            );
            values.insert(key, max);
            if let Some(used) = used {
                info.push(PropertyInfo { group: "Cuotas".into(), label: format!("Usado en {ts}"), value: used });
            }
        }

        let mut choices: Vec<FieldChoices> = self.create_database_choices_impl().await.unwrap_or_default();
        choices.retain(|c| c.key == "default_tablespace" || c.key == "temporary_tablespace");
        for c in &mut choices {
            c.default = None;
        }
        choices.push(FieldChoices { key: "profile".into(), default: None, values: profiles });

        let mut warnings = BTreeMap::new();
        warnings.insert(
            "account_locked".to_string(),
            "Con la cuenta bloqueada nadie puede iniciar sesión como este usuario (las sesiones abiertas siguen); los objetos del esquema siguen accesibles para quienes tengan permisos sobre ellos.".to_string(),
        );
        warnings.insert(
            "default_tablespace".to_string(),
            "Solo afecta a los segmentos nuevos: las tablas e índices existentes no se mueven.".to_string(),
        );
        warnings.insert(
            "profile".to_string(),
            "El perfil fija límites de recursos y de contraseña: puede cortar sesiones que los superen o hacer vencer la contraseña.".to_string(),
        );
        for k in values.keys().filter(|k| k.starts_with("quota:")) {
            warnings.insert(
                k.clone(),
                "Una cuota menor que lo usado no borra datos, pero los objetos del esquema ya no pueden crecer en ese tablespace.".to_string(),
            );
        }
        Ok(DatabaseProperties { fields, values, info, choices, warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(database, changes)?;
        self.run(move |c| {
            for (i, sql) in statements.iter().enumerate() {
                if let Err(e) = c.execute(sql, &[]) {
                    let e = err(e);
                    return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
                }
            }
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn only_the_changes_in_a_safe_order() {
        let s = script(
            "VENTAS",
            &c(&[
                ("account_locked", "true"),
                ("quota:USERS", "500 m"),
                ("quota:Data 2", ""),
                ("default_tablespace", "users"),
                ("temporary_tablespace", "TEMP"),
                ("profile", "default"),
            ]),
        )
        .unwrap();
        assert_eq!(
            s,
            "ALTER USER \"VENTAS\" DEFAULT TABLESPACE \"USERS\";
ALTER USER \"VENTAS\" PROFILE DEFAULT;
ALTER USER \"VENTAS\" TEMPORARY TABLESPACE \"TEMP\";
ALTER USER \"VENTAS\" QUOTA 0 ON \"Data 2\";
ALTER USER \"VENTAS\" QUOTA 500M ON \"USERS\";
ALTER USER \"VENTAS\" ACCOUNT LOCK;"
        );
        assert_eq!(
            script("v", &c(&[("account_locked", ""), ("quota:USERS", "unlimited"), ("profile", "app_profile")])).unwrap(),
            "ALTER USER \"v\" ACCOUNT UNLOCK;\nALTER USER \"v\" PROFILE \"APP_PROFILE\";\nALTER USER \"v\" QUOTA UNLIMITED ON \"USERS\";"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [("quota:USERS", "10X"), ("quota:USERS", "1.5G"), ("default_tablespace", " "), ("quota:", "1M"), ("nope", "1")] {
            assert!(script("V", &c(&[bad])).is_err(), "{bad:?}");
        }
        // Odd names are quoted, never loose.
        assert!(script("a\"b", &c(&[("profile", "x\" y")])).unwrap().contains("ALTER USER \"a\"\"b\" PROFILE \"x\"\" y\""));
        assert!(script(" ", &c(&[])).is_err());
    }

    #[test]
    fn quotas_read_back_as_typed() {
        assert_eq!(shown_quota(-1), "UNLIMITED");
        assert_eq!(shown_quota(10 << 20), "10M");
        assert_eq!(shown_quota(3 << 30), "3G");
        assert_eq!(shown_quota(1000), "1000");
        assert_eq!(shown_quota(0), "0");
    }
}

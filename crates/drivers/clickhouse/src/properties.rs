//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what `system.databases`, `system.tables` and `system.parts` report, and
//! what `ALTER DATABASE` changes.
//!
//! - Every engine: the comment (`MODIFY COMMENT`).
//! - The engines that take `ALTER DATABASE … MODIFY SETTING`
//!   (MaterializedPostgreSQL, DataLakeCatalog): each setting in the
//!   engine's `SETTINGS` clause, and new ones. The rest (Atomic, Memory,
//!   Replicated…) refuse it, so they get no settings tab. Settings the
//!   server shows as `[HIDDEN]` (credentials) are left out.
//!
//! Timeplus Proton has no `ALTER DATABASE`: no properties.
//!
//! One statement per change; every value is checked before it reaches the
//! SQL.

use crate::create_db::literal;
use crate::schema::q;
use crate::{text, ClickHouseSession, Flavor};
use dbine_driver::serde_static::intern;
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use std::collections::BTreeMap;

/// Database engines whose settings `ALTER DATABASE … MODIFY SETTING` changes.
const SETTABLE: &[&str] = &["MaterializedPostgreSQL", "DataLakeCatalog"];

const SETTINGS: &str = "Configuración (SETTINGS)";

fn setting_name(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !v.starts_with(|c: char| c.is_ascii_digit())
}

/// A setting's value as SQL: numbers and booleans bare, anything else as a
/// string literal.
fn setting_value(v: &str) -> String {
    let number = !v.is_empty() && v.parse::<f64>().is_ok() && v.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'));
    if number || matches!(v, "true" | "false") {
        v.to_string()
    } else {
        literal(v)
    }
}

/// `name = value` lines (the new settings).
fn setting_lines(v: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for line in v.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (k, val) = line.split_once('=').ok_or_else(|| Error::Query(format!("configuración: «{line}» no tiene la forma nombre = valor")))?;
        let k = k.trim();
        if !setting_name(k) {
            return Err(Error::Query(format!("configuración: «{k}» no es un nombre de setting")));
        }
        out.push((k.to_string(), val.trim().trim_matches('\'').to_string()));
    }
    Ok(out)
}

/// The settings of an `engine_full` (`Engine(args) SETTINGS a = 1, b = 'x'`),
/// unquoted, in order.
pub(crate) fn engine_settings(engine_full: &str) -> Vec<(String, String)> {
    // The SETTINGS keyword outside quotes.
    let (mut quoted, mut at) = (false, None);
    let bytes = engine_full.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quoted => i += 1,
            b'\'' => quoted = !quoted,
            b' ' if !quoted && engine_full[i..].starts_with(" SETTINGS ") => {
                at = Some(i + " SETTINGS ".len());
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let Some(at) = at else { return Vec::new() };
    // Split on commas outside quotes.
    let (mut parts, mut cur, mut quoted, mut escaped) = (Vec::new(), String::new(), false, false);
    for c in engine_full[at..].chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => escaped = true,
            '\'' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
        .iter()
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            let v = v.trim();
            let v = v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')).unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .filter(|(k, _)| setting_name(k))
        .collect()
}

/// The statements for `changes`: the comment, then the settings.
pub(crate) fn alter(flavor: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if flavor == Flavor::Timeplus {
        return Err(Error::Unsupported("Timeplus Proton no modifica las propiedades de una base".into()));
    }
    let db = q(database);
    let (mut out, mut settings) = (Vec::new(), Vec::new());
    for (key, value) in changes {
        match key.as_str() {
            "comment" => out.push(format!("ALTER DATABASE {db} MODIFY COMMENT {}", literal(value.trim()))),
            "settings_add" => {
                for (k, v) in setting_lines(value)? {
                    settings.push(format!("{k} = {}", setting_value(&v)));
                }
            }
            k => match k.strip_prefix("setting:") {
                Some(name) if setting_name(name) => {
                    let v = value.trim();
                    if v.is_empty() {
                        return Err(Error::Query(format!("{name}: el valor no puede quedar vacío")));
                    }
                    settings.push(format!("{name} = {}", setting_value(v)));
                }
                _ => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
            },
        }
    }
    for s in settings {
        out.push(format!("ALTER DATABASE {db} MODIFY SETTING {s}"));
    }
    Ok(out)
}

pub(crate) fn script(flavor: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(flavor, database, changes)?.join(";\n"))
}

impl ClickHouseSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        if self.flavor == Flavor::Timeplus {
            return Err(Error::Unsupported("Timeplus Proton no muestra las propiedades de una base".into()));
        }
        let p = [("db", database)];
        let rows = self
            .rows(
                "SELECT engine, engine_full, data_path, metadata_path, toString(uuid), comment, toString(is_external)
                 FROM system.databases WHERE name = {db:String}",
                &p,
            )
            .await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let col = |i: usize| r.get(i).map(text).unwrap_or_default();
        let (engine, engine_full) = (col(0), col(1));
        let general = |label: &str, value: String| PropertyInfo { group: String::new(), label: label.into(), value };
        let mut info = vec![
            general("Motor (ENGINE)", if engine_full.is_empty() { engine.clone() } else { engine_full.clone() }),
            general("UUID", col(4)),
            general("Ruta de datos", col(2)),
            general("Ruta de metadatos", col(3)),
        ];
        if col(6) == "1" {
            info.push(general("Externa", "Sí: sus tablas están en otro servidor o catálogo".into()));
        }
        let tables = self
            .rows("SELECT toString(count()) FROM system.tables WHERE database = {db:String} AND NOT is_temporary", &p)
            .await
            .unwrap_or_default();
        if let Some(n) = tables.first().and_then(|r| r.first()).map(text) {
            info.push(general("Tablas", n));
        }
        let parts = self
            .rows(
                "SELECT formatReadableSize(sum(bytes_on_disk)), formatReadableSize(sum(data_uncompressed_bytes)), toString(sum(rows)), toString(count())
                 FROM system.parts WHERE database = {db:String} AND active",
                &p,
            )
            .await
            .unwrap_or_default();
        if let Some(r) = parts.first() {
            let c = |i: usize| r.get(i).map(text).unwrap_or_default();
            info.push(general("Tamaño en disco", c(0)));
            info.push(general("Tamaño sin comprimir", c(1)));
            info.push(general("Filas", c(2)));
            info.push(general("Partes activas", c(3)));
        }

        let mut values = BTreeMap::from([("comment".to_string(), col(5))]);
        let mut fields = vec![Field::new("comment", "Comentario", FieldKind::Textarea)];
        if SETTABLE.contains(&engine.as_str()) {
            for (k, v) in engine_settings(&engine_full) {
                if v == "[HIDDEN]" {
                    continue;
                }
                let key = format!("setting:{k}");
                fields.push(Field::new(intern(&key), intern(&k), FieldKind::Text).group(SETTINGS));
                values.insert(key, v);
            }
            fields.push(
                Field::new("settings_add", "Agregar settings", FieldKind::Textarea)
                    .placeholder("nombre = valor")
                    .help("Uno por línea. Solo los que el motor de la base admite cambiar.")
                    .group(SETTINGS),
            );
            values.insert("settings_add".into(), String::new());
        }
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings: BTreeMap::new() })
    }

    /// One request per statement; a later failure says how many ran.
    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(self.flavor, database, changes)?;
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.rows(sql, &[]).await {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn comment_then_settings() {
        assert_eq!(
            script(
                Flavor::ClickHouse,
                "ven`tas",
                &c(&[("setting:materialized_postgresql_tables_list", "a,b"), ("comment", "it's \\ ok"), ("settings_add", "x_y = 10\n\nflag = true")])
            )
            .unwrap(),
            "ALTER DATABASE `ven\\`tas` MODIFY COMMENT 'it\\'s \\\\ ok';\n\
             ALTER DATABASE `ven\\`tas` MODIFY SETTING materialized_postgresql_tables_list = 'a,b';\n\
             ALTER DATABASE `ven\\`tas` MODIFY SETTING x_y = 10;\n\
             ALTER DATABASE `ven\\`tas` MODIFY SETTING flag = true"
        );
        assert_eq!(script(Flavor::ClickHouse, "v", &c(&[("comment", "")])).unwrap(), "ALTER DATABASE `v` MODIFY COMMENT ''");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("setting:a b", "1"), ("setting:x", " "), ("settings_add", "x"), ("settings_add", "x;y = 1"), ("engine", "Memory")] {
            assert!(script(Flavor::ClickHouse, "v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Flavor::Timeplus, "v", &c(&[])).is_err());
        // A value can't break out of its literal.
        assert_eq!(
            script(Flavor::ClickHouse, "v", &c(&[("setting:s", "x', y = 1 --")])).unwrap(),
            "ALTER DATABASE `v` MODIFY SETTING s = 'x\\', y = 1 --'"
        );
    }

    #[test]
    fn settings_of_the_engine() {
        assert_eq!(
            engine_settings("DataLakeCatalog('http://h:8181/v1', 'a,b') SETTINGS catalog_type = 'rest', warehouse = 'w\\'x', n = 3"),
            vec![("catalog_type".into(), "rest".into()), ("warehouse".into(), "w'x".into()), ("n".into(), "3".into())]
        );
        assert!(engine_settings("Atomic").is_empty());
        assert!(engine_settings("MySQL('h', 'db SETTINGS x = 1', 'u', '[HIDDEN]')").is_empty());
    }
}

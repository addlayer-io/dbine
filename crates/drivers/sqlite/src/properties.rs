//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what the file's header and PRAGMAs report, and the settings that persist
//! in the file.
//!
//! - SQLite: the application's numbers (`user_version`, `application_id`),
//!   the journal (rollback `DELETE` or `WAL`: the only modes that persist),
//!   the page size and auto vacuum (both applied with `VACUUM`).
//! - libSQL (sqld, Turso): only `user_version`; the server manages the
//!   journal (always WAL), the page size and vacuuming, and refuses those
//!   PRAGMAs and `VACUUM`.
//!
//! Shared with the libSQL driver, which runs the reads over HTTP.

use crate::schema::Rows;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Sqlite,
    Libsql,
}

const STORAGE: &str = "Almacenamiento";

const PAGE_SIZES: &[(&str, &str)] =
    &[("512", "512"), ("1024", "1024"), ("2048", "2048"), ("4096", "4096"), ("8192", "8192"), ("16384", "16384"), ("32768", "32768"), ("65536", "65536")];

/// `database` as a PRAGMA's schema prefix (`main`, `temp` or an attached name).
fn schema(database: &str) -> String {
    let d = if database.is_empty() { "main" } else { database };
    if d.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !d.starts_with(|c: char| c.is_ascii_digit()) {
        d.to_string()
    } else {
        quote_ident(Quote::Double, d)
    }
}

fn int32(value: &str, what: &str) -> Result<i32> {
    value.parse::<i32>().map_err(|_| Error::Query(format!("{what}: «{value}» no es un entero de 32 bits")))
}

/// The statements for `changes`, in a safe order: back to the rollback
/// journal first (the page size can't change in WAL), the numbers, the page
/// size and auto vacuum, one `VACUUM` that applies them, and WAL last.
pub fn alter(flavor: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if flavor == Flavor::Libsql && !matches!(database, "" | "main") {
        return Err(Error::Query(format!("libSQL tiene una sola base («main»), no «{database}»")));
    }
    let p = schema(database);
    let pragma = |name: &str, v: &str| format!("PRAGMA {p}.{name} = {v}");
    let (mut first, mut out, mut storage, mut last) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (key, value) in changes {
        let value = value.trim();
        if flavor == Flavor::Libsql && key != "user_version" {
            return Err(Error::Query(format!("libSQL no deja cambiar «{key}»: lo administra el servidor")));
        }
        match key.as_str() {
            "user_version" => out.push(pragma("user_version", &int32(value, "versión del esquema")?.to_string())),
            "application_id" => out.push(pragma("application_id", &int32(value, "ID de aplicación")?.to_string())),
            "journal_mode" => match value {
                "DELETE" => first.push(pragma("journal_mode", "DELETE")),
                "WAL" => last.push(pragma("journal_mode", "WAL")),
                _ => return Err(Error::Query(format!("modo del diario: «{value}» no es un valor válido"))),
            },
            "page_size" => {
                let ok = PAGE_SIZES.iter().any(|(v, _)| *v == value);
                if !ok {
                    return Err(Error::Query(format!("tamaño de página: «{value}» no es una potencia de 2 entre 512 y 65536")));
                }
                storage.push(pragma("page_size", value));
            }
            "auto_vacuum" => {
                if !matches!(value, "NONE" | "FULL" | "INCREMENTAL") {
                    return Err(Error::Query(format!("auto vacuum: «{value}» no es un valor válido")));
                }
                storage.push(pragma("auto_vacuum", value));
            }
            k => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
        }
    }
    if !storage.is_empty() {
        storage.push(format!("VACUUM {p}"));
    }
    Ok(first.into_iter().chain(out).chain(storage).chain(last).collect())
}

/// What "Ver script" shows: the statements, each ended by `;`.
pub fn script(flavor: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(flavor, database, changes)?.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"))
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn auto_vacuum_name(v: &str) -> String {
    match v {
        "0" => "NONE".into(),
        "1" => "FULL".into(),
        "2" => "INCREMENTAL".into(),
        other => other.to_uppercase(),
    }
}

/// The current values of the settings, as the fields hold them.
pub fn values<E>(q: &mut dyn FnMut(&str) -> std::result::Result<Rows, E>, database: &str) -> BTreeMap<String, String> {
    let p = schema(database);
    let mut out = BTreeMap::new();
    for key in ["user_version", "application_id", "journal_mode", "page_size", "auto_vacuum"] {
        let v = q(&format!("PRAGMA {p}.{key}")).ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next()).and_then(|v| text(&v));
        if let Some(v) = v {
            let v = match key {
                "journal_mode" => v.to_uppercase(),
                "auto_vacuum" => auto_vacuum_name(&v),
                _ => v,
            };
            out.insert(key.to_string(), v);
        }
    }
    out
}

/// What a change that SQLite accepted without applying left behind (the
/// journal mode, the page size and auto vacuum fail silently): one line
/// per setting whose value isn't the requested one.
pub fn not_applied(changes: &BTreeMap<String, String>, now: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for (key, why) in [
        ("journal_mode", "otra conexión tiene la base abierta, hay una transacción abierta o la base está en memoria"),
        ("page_size", "en modo WAL el tamaño de página no cambia: hay que pasar antes el diario a DELETE"),
        ("auto_vacuum", "el VACUUM no pudo aplicarlo"),
    ] {
        if let Some(want) = changes.get(key).map(|v| v.trim()) {
            let have = now.get(key).map(String::as_str).unwrap_or("");
            if have != want {
                out.push(format!("{key} quedó en «{have}» y no en «{want}» ({why})"));
            }
        }
    }
    out
}

fn cell<E>(q: &mut dyn FnMut(&str) -> std::result::Result<Rows, E>, sql: &str) -> Option<String> {
    q(sql).ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next()).and_then(|v| text(&v))
}

fn fact(info: &mut Vec<PropertyInfo>, group: &str, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        info.push(PropertyInfo { group: group.into(), label: label.into(), value });
    }
}

/// "Propiedades" of `database`. `q` runs a query and returns its rows (the
/// libSQL driver replays it over HTTP, so a missing answer reads as empty).
pub fn read<E>(q: &mut dyn FnMut(&str) -> std::result::Result<Rows, E>, database: &str, flavor: Flavor) -> DatabaseProperties {
    let p = schema(database);
    let name = if database.is_empty() { "main" } else { database };
    let values = values(q, database);
    let page_size: Option<i64> = values.get("page_size").and_then(|v| v.parse().ok());
    let pages: Option<i64> = cell(q, &format!("PRAGMA {p}.page_count")).and_then(|v| v.parse().ok());
    let free: Option<i64> = cell(q, &format!("PRAGMA {p}.freelist_count")).and_then(|v| v.parse().ok());
    let mb = |bytes: i64| format!("{:.2} MB", bytes as f64 / 1_048_576.0);
    let mut info = Vec::new();
    if flavor == Flavor::Sqlite {
        let file = q("SELECT name, file FROM pragma_database_list")
            .ok()
            .and_then(|rows| rows.into_iter().find(|r| r.first().and_then(text).as_deref() == Some(name)))
            .and_then(|r| r.get(1).and_then(text));
        fact(&mut info, "", "Archivo", Some(file.filter(|f| !f.is_empty()).unwrap_or_else(|| "(en memoria)".into())));
    }
    fact(&mut info, "", "Tamaño", pages.zip(page_size).map(|(n, s)| mb(n * s)));
    fact(&mut info, "", "Versión de SQLite", cell(q, "SELECT sqlite_version()"));
    fact(&mut info, "", "Codificación", cell(q, &format!("PRAGMA {p}.encoding")));
    let master = if p == "main" { "sqlite_master".to_string() } else { format!("{p}.sqlite_master") };
    for (label, ty) in [("Tablas", "table"), ("Vistas", "view"), ("Índices", "index"), ("Triggers", "trigger")] {
        let sql = format!("SELECT COUNT(*) FROM {master} WHERE type = '{ty}' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'");
        fact(&mut info, "", label, cell(q, &sql));
    }
    fact(&mut info, "", "Cambios de esquema (schema_version)", cell(q, &format!("PRAGMA {p}.schema_version")));
    fact(&mut info, STORAGE, "Páginas", pages.map(|n| n.to_string()));
    fact(&mut info, STORAGE, "Páginas libres (recuperables con VACUUM)", free.zip(page_size).map(|(n, s)| format!("{n} ({})", mb(n * s))));

    let mut fields = vec![
        Field::new("user_version", "Versión del esquema de la aplicación (user_version)", FieldKind::Number)
            .help("Un entero que la aplicación usa a su criterio, casi siempre para sus migraciones. SQLite no lo usa."),
    ];
    let mut warnings = BTreeMap::new();
    warnings.insert(
        "user_version".to_string(),
        "Las aplicaciones que usan user_version para sus migraciones pueden volver a correrlas o saltearlas.".to_string(),
    );
    match flavor {
        Flavor::Sqlite => {
            fields.extend([
                Field::new("application_id", "ID de aplicación (application_id)", FieldKind::Number)
                    .help("Identifica el formato de archivo de la aplicación que lo creó (lo lee el comando file, por ejemplo)."),
                Field::new(
                    "journal_mode",
                    "Modo del diario (journal_mode)",
                    FieldKind::Select(vec![("DELETE", "Diario de reversión (DELETE)"), ("WAL", "Write-ahead log (WAL)")]),
                )
                .help("Solo estos dos quedan guardados en el archivo; TRUNCATE, PERSIST, MEMORY y OFF valen solo para la conexión que los pone.")
                .group(STORAGE),
                Field::new("page_size", "Tamaño de página (page_size, bytes)", FieldKind::Select(PAGE_SIZES.to_vec()))
                    .help("Se aplica con VACUUM. En modo WAL no cambia: hay que pasar antes el diario a DELETE (se puede en el mismo cambio).")
                    .group(STORAGE),
                Field::new(
                    "auto_vacuum",
                    "Auto vacuum",
                    FieldKind::Select(vec![("NONE", "No (NONE)"), ("FULL", "Al confirmar cada transacción (FULL)"), ("INCREMENTAL", "Al pedirlo con incremental_vacuum (INCREMENTAL)")]),
                )
                .help("Se aplica con VACUUM.")
                .group(STORAGE),
            ]);
            let vacuum = "Se aplica con VACUUM, que reescribe la base entera: la bloquea mientras dura y necesita espacio libre en disco del tamaño de la base.";
            warnings.insert("page_size".into(), vacuum.into());
            warnings.insert("auto_vacuum".into(), vacuum.into());
            warnings.insert(
                "journal_mode".into(),
                "Cambiar el modo del diario necesita que ninguna otra conexión tenga la base abierta. En WAL la base usa además los archivos -wal y -shm, \
                 que tienen que copiarse con ella, y no funciona sobre sistemas de archivos de red."
                    .into(),
            );
        }
        Flavor::Libsql => {
            let jm = values.get("journal_mode").cloned();
            fact(&mut info, STORAGE, "Modo del diario (journal_mode)", jm.map(|v| format!("{v} (lo administra el servidor)")));
            fact(&mut info, STORAGE, "Tamaño de página (bytes)", values.get("page_size").cloned());
            fact(&mut info, STORAGE, "Auto vacuum", values.get("auto_vacuum").cloned());
            fact(&mut info, "", "ID de aplicación (application_id)", values.get("application_id").cloned());
        }
    }
    let values = values.into_iter().filter(|(k, _)| fields.iter().any(|f| f.key == k)).collect();
    DatabaseProperties { fields, values, info, choices: Vec::new(), warnings }
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
            Flavor::Sqlite,
            "main",
            &c(&[("journal_mode", "WAL"), ("page_size", "8192"), ("user_version", "3"), ("auto_vacuum", "INCREMENTAL"), ("application_id", "-12")]),
        )
        .unwrap();
        assert_eq!(
            s,
            "PRAGMA main.application_id = -12;
PRAGMA main.user_version = 3;
PRAGMA main.auto_vacuum = INCREMENTAL;
PRAGMA main.page_size = 8192;
VACUUM main;
PRAGMA main.journal_mode = WAL;"
        );
        // Out of WAL before the page size changes.
        let s = script(Flavor::Sqlite, "", &c(&[("journal_mode", "DELETE"), ("page_size", "1024")])).unwrap();
        assert_eq!(s, "PRAGMA main.journal_mode = DELETE;\nPRAGMA main.page_size = 1024;\nVACUUM main;");
        assert_eq!(script(Flavor::Sqlite, "aux db", &c(&[("user_version", "1")])).unwrap(), "PRAGMA \"aux db\".user_version = 1;");
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("user_version", "1; DROP TABLE t"),
            ("user_version", "4294967296"),
            ("application_id", "x"),
            ("journal_mode", "MEMORY"),
            ("page_size", "1000"),
            ("page_size", "131072"),
            ("auto_vacuum", "1"),
            ("nope", "1"),
        ] {
            assert!(script(Flavor::Sqlite, "main", &c(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn libsql_only_takes_user_version() {
        assert_eq!(script(Flavor::Libsql, "main", &c(&[("user_version", "9")])).unwrap(), "PRAGMA main.user_version = 9;");
        assert!(script(Flavor::Libsql, "main", &c(&[("journal_mode", "DELETE")])).is_err());
        assert!(script(Flavor::Libsql, "main", &c(&[("application_id", "1")])).is_err());
        assert!(script(Flavor::Libsql, "other", &c(&[("user_version", "1")])).is_err());
    }

    #[test]
    fn silent_failures_are_reported() {
        let changes = c(&[("journal_mode", "WAL"), ("page_size", "8192"), ("user_version", "1")]);
        let now = c(&[("journal_mode", "MEMORY"), ("page_size", "8192"), ("user_version", "1")]);
        let left = not_applied(&changes, &now);
        assert_eq!(left.len(), 1);
        assert!(left[0].starts_with("journal_mode quedó en «MEMORY»"), "{left:?}");
    }
}

//! The key designer, create templates and insert scripts, as etcdctl
//! commands the editor runs (see [`crate::command`]).
//!
//! The designer creates one key: its value is the `value` table option and
//! `ttl` (seconds) attaches a new lease. Columns aren't used: an etcd value
//! is opaque bytes. Dropping is `del`.

use crate::command::quote_arg;
use dbine_driver::{
    kinds, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, ObjectRef, Result, RowChange, TableSchema,
};
use serde_json::Value;

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        kind: kinds::KEY,
        label: "Nueva clave",
        data_types: Vec::new(),
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        indexes: false,
        foreign_keys: false,
        column_options: Vec::new(),
        table_options: vec![
            Field::new("value", "Valor", FieldKind::Textarea).help("El contenido de la clave (texto, JSON…)."),
            Field::new("ttl", "TTL (segundos)", FieldKind::Number)
                .placeholder("(sin vencimiento)")
                .help("Crea un lease con ese TTL y lo asocia a la clave: al vencer, la clave se borra."),
        ],
        columns_required: false,
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    let t = |label: &'static str, template: &str| CreateTemplate { kind: kinds::KEY, label, template: template.into() };
    vec![
        t("Nueva clave de configuración", "put {name} '{\"habilitado\": true, \"reintentos\": 3}'\nget {name}\n"),
        t(
            "Nueva clave con lease (TTL)",
            "# Crea un lease de 60 s y asocia la clave; al vencer, etcd la borra.\nput {name} activo --ttl=60\nget {name}\nlease list\n",
        ),
        t(
            "Árbol de claves con prefijo",
            "put {name}/host db.interna\nput {name}/puerto 5432\nput {name}/usuario app\nget {name}/ --prefix\n",
        ),
        t("Historial de una clave", "put {name} v1\nput {name} v2\n# --rev=N lee la clave como estaba en esa revisión.\nget {name} --rev=1\n"),
    ]
}

fn opt<'a>(t: &'a TableSchema, key: &str) -> Option<&'a str> {
    t.options.get(key).map(String::as_str).filter(|s| !s.trim().is_empty())
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    if t.name.is_empty() {
        return Err(Error::Query("La clave necesita un nombre.".into()));
    }
    let key = quote_arg(&t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("del {key}"));
    }
    if parts.create {
        let value = t.options.get("value").map(String::as_str).unwrap_or("");
        let mut line = format!("put {key} {}", quote_arg(value));
        if let Some(ttl) = opt(t, "ttl") {
            let n: u64 = ttl.trim().parse().map_err(|_| Error::Query(format!("TTL inválido: {ttl}")))?;
            line.push_str(&format!(" --ttl={n}"));
        }
        out.push(line);
    }
    Ok(out.join("\n"))
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// `put` commands. Rows with `key` and `value` columns (what browsing etcd
/// gives) go as they are; other rows become `<target>/<first column>` with
/// the whole row as a JSON object.
pub fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    let pos = |n: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(n));
    let mut out = Vec::with_capacity(rows.len());
    if let (Some(k), Some(v)) = (pos("key"), pos("value")) {
        for r in rows {
            let key = r.get(k).map(text).unwrap_or_default();
            if key.is_empty() {
                continue;
            }
            out.push(format!("put {} {}", quote_arg(&key), quote_arg(&r.get(v).map(text).unwrap_or_default())));
        }
    } else {
        if columns.is_empty() {
            return Err(Error::Query("No hay columnas para armar las claves.".into()));
        }
        let base = target.name.trim_end_matches('/');
        for r in rows {
            let id = r.first().map(text).unwrap_or_default();
            let obj: serde_json::Map<String, Value> = columns.iter().cloned().zip(r.iter().cloned()).collect();
            out.push(format!("put {} {}", quote_arg(&format!("{base}/{id}")), quote_arg(&Value::Object(obj).to_string())));
        }
    }
    Ok(out.join("\n"))
}

/// Revision metadata browsing shows, which etcd manages itself.
const META_COLUMNS: &[&str] = &["create_revision", "mod_revision", "version", "lease"];

/// Edited `key` / `value` rows (what browsing etcd gives) as `put`; a null
/// value is an empty one, as in [`insert_script`]. A renamed key is `del`
/// of the old one plus `put` of the new, with the edited value or else the
/// original one.
pub fn update_script(changes: &[RowChange]) -> Result<String> {
    let get = |pairs: &[(String, Value)], c: &str| pairs.iter().find(|(k, _)| k.eq_ignore_ascii_case(c)).map(|(_, v)| v.clone());
    let mut out = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        if let Some((c, _)) = ch.set.iter().find(|(c, _)| META_COLUMNS.contains(&c.as_str())) {
            return Err(Error::Unsupported(format!("{c} lo administra etcd; no se edita")));
        }
        let Some(key) = get(&ch.key, "key").map(|v| text(&v)).filter(|k| !k.is_empty()) else {
            return Err(Error::Unsupported("solo se editan filas con las columnas key y value de etcd".into()));
        };
        let new_key = get(&ch.set, "key").map(|v| text(&v)).filter(|k| *k != key);
        let value = get(&ch.set, "value").or_else(|| new_key.as_ref().and_then(|_| get(&ch.key, "value")));
        match (new_key, value) {
            (Some(nk), _) if nk.is_empty() => return Err(Error::Query("La clave nueva está vacía.".into())),
            (Some(_), None) => {
                return Err(Error::Unsupported("para renombrar una clave de etcd hace falta también su valor".into()));
            }
            (Some(nk), Some(v)) => {
                out.push(format!("del {}", quote_arg(&key)));
                out.push(format!("put {} {}", quote_arg(&nk), quote_arg(&text(&v))));
            }
            (None, Some(v)) => out.push(format!("put {} {}", quote_arg(&key), quote_arg(&text(&v)))),
            (None, None) => {}
        }
    }
    Ok(out.join("\n"))
}

/// `del <key>` per row with a `key` column (what browsing etcd gives):
/// one exact key, never a range or prefix.
pub fn delete_script(keys: &[Vec<(String, Value)>]) -> Result<String> {
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        let Some(key) = k.iter().find(|(c, _)| c.eq_ignore_ascii_case("key")).map(|(_, v)| text(v)).filter(|k| !k.is_empty()) else {
            return Err(Error::Unsupported("solo se borran filas con la columna key de etcd".into()));
        };
        out.push(format!("del {}", quote_arg(&key)));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse_script;
    use serde_json::json;

    #[test]
    fn designer_puts_with_ttl() {
        let t = TableSchema {
            kind: kinds::KEY.into(),
            name: "/app/cfg".into(),
            options: [("value".to_string(), "{\"a\": 1}".to_string()), ("ttl".to_string(), "30".to_string())].into(),
            ..Default::default()
        };
        let s = table_ddl(&t, DdlParts { drop: true, create: true, ..Default::default() }).unwrap();
        assert_eq!(s, "del /app/cfg\nput /app/cfg \"{\\\"a\\\": 1}\" --ttl=30");
        let parsed = parse_script(&s).unwrap();
        assert_eq!(parsed[1].args[2], b"{\"a\": 1}".to_vec());
        assert!(designer().table_options.iter().any(|f| f.key == "ttl"));
        assert!(templates().iter().all(|t| t.template.contains("{name}") && parse_script(&t.template).is_ok()));
    }

    #[test]
    fn inserts_key_value_or_json_rows() {
        let target = ObjectRef { kind: kinds::KEY.into(), schema: None, name: "/imp/".into() };
        let s = insert_script(&target, &["key".into(), "value".into()], &[vec![json!("a"), json!("x y")]]).unwrap();
        assert_eq!(s, "put a \"x y\"");
        let s = insert_script(&target, &["id".into(), "nombre".into()], &[vec![json!(7), json!("O'Brien")]]).unwrap();
        let c = parse_script(&s).unwrap();
        assert_eq!(c[0].word(1), "/imp/7");
        assert_eq!(c[0].word(2), "{\"id\":7,\"nombre\":\"O'Brien\"}");
    }

    #[test]
    fn deletes_are_exact_dels() {
        let key = |k: Value| vec![("key".to_string(), k), ("value".to_string(), json!("v"))];
        let s = delete_script(&[key(json!("/app/O'Brien \"Bob\"")), key(json!("--prefix")), key(json!("/a"))]).unwrap();
        assert_eq!(s, "del \"/app/O'Brien \\\"Bob\\\"\"\ndel \"--prefix\"\ndel /a");
        let c = parse_script(&s).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].word(1), "/app/O'Brien \"Bob\"");
        assert_eq!(c[1].word(1), "--prefix");
        assert!(!c[1].flag("prefix"));
        assert!(delete_script(&[vec![("id".to_string(), json!(1))]]).is_err());
        assert!(delete_script(&[key(json!(""))]).is_err());
    }

    #[test]
    fn updates_are_puts() {
        let ch = |key: Vec<(&str, Value)>, set: Vec<(&str, Value)>| RowChange {
            key: key.into_iter().map(|(c, v)| (c.to_string(), v)).collect(),
            set: set.into_iter().map(|(c, v)| (c.to_string(), v)).collect(), ..Default::default()
        };
        let s = update_script(&[
            ch(vec![("key", json!("/app/a b"))], vec![("value", json!("O'Brien \"Bob\""))]),
            ch(vec![("key", json!("/app/n"))], vec![("value", Value::Null)]),
            ch(vec![("key", json!("/old")), ("value", json!("v"))], vec![("key", json!("/new"))]),
            ch(vec![("key", json!("/x"))], vec![]),
        ])
        .unwrap();
        assert_eq!(s, "put \"/app/a b\" \"O'Brien \\\"Bob\\\"\"\nput /app/n \"\"\ndel /old\nput /new v");
        let c = parse_script(&s).unwrap();
        assert_eq!(c[0].word(2), "O'Brien \"Bob\"");
        assert_eq!(c[1].word(2), "");
        assert!(update_script(&[ch(vec![("key", json!("/k"))], vec![("version", json!(3))])]).is_err());
        assert!(update_script(&[ch(vec![("key", json!("/k"))], vec![("key", json!("/j"))])]).is_err());
    }
}

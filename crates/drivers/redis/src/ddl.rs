//! The key designer, create templates and insert scripts, all as Redis
//! commands the editor runs (one per line, arguments quoted with
//! [`command::quote_arg`]).
//!
//! The designer creates one key. Its type is the `type` table option and
//! its columns are the entries, read per type:
//!
//! | type     | column `name` | column `default_value` |
//! |----------|---------------|------------------------|
//! | `string` | —             | — (the `value` option) |
//! | `hash`   | field         | value (empty if unset) |
//! | `list`   | element, when there's no default value | element |
//! | `set`    | element, when there's no default value | element |
//! | `zset`   | member        | score (0 if unset)     |
//! | `stream` | field         | value (one `XADD *` entry) |
//! | `json`   | —             | — (the `value` option, a JSON document) |
//!
//! `ttl` (seconds) adds an `EXPIRE`. Dropping is `DEL`; `IF EXISTS` doesn't
//! apply (DEL of a missing key is a no-op).

use crate::command::quote_arg;
use dbine_driver::{
    kinds, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, ObjectRef, Result, RowChange, TableSchema,
};
use serde_json::Value;

pub fn designer(driver_id: &str) -> DesignerSpec {
    let json_label = if driver_id == "dragonfly" { "JSON" } else { "JSON (requiere el módulo JSON)" };
    DesignerSpec {
        kind: kinds::KEY,
        label: "Nueva key",
        data_types: Vec::new(),
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: true,
        nullability: false,
        comments: false,
        indexes: false,
        foreign_keys: false,
        column_options: Vec::new(),
        table_options: vec![
            Field::new(
                "type",
                "Tipo",
                FieldKind::Select(vec![
                    ("string", "String"),
                    ("hash", "Hash"),
                    ("list", "List"),
                    ("set", "Set"),
                    ("zset", "Sorted set"),
                    ("stream", "Stream"),
                    ("json", json_label),
                ]),
            )
            .required()
            .default_value("string")
            .help(
                "Las columnas son las entradas: hash y stream = campo (nombre) y valor (valor por defecto); \
                 list y set = elemento; sorted set = miembro (nombre) y score (valor por defecto).",
            ),
            Field::new("ttl", "TTL (segundos)", FieldKind::Number)
                .placeholder("(sin vencimiento)")
                .help("Agrega un EXPIRE después de crear la key."),
            Field::new("value", "Valor", FieldKind::Textarea)
                .help("Solo para string y JSON (un documento JSON, p. ej. {\"a\": 1})."),
        ],
        columns_required: false,
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    let t = |label: &'static str, template: &str| CreateTemplate { kind: kinds::KEY, label, template: template.into() };
    vec![
        t("Nuevo hash con vencimiento", "HSET {name} nombre Ana email ana@example.com visitas 0\nEXPIRE {name} 3600\nTTL {name}\n"),
        t("Nueva lista (cola)", "RPUSH {name} tarea-1 tarea-2 tarea-3\nLPOP {name}\nLRANGE {name} 0 -1\n"),
        t("Nuevo set", "SADD {name} rojo verde azul\nSISMEMBER {name} verde\nSMEMBERS {name}\n"),
        t("Nuevo sorted set (ranking)", "ZADD {name} 120 ana 95 luis 80 marta\nZINCRBY {name} 10 luis\nZREVRANGE {name} 0 9 WITHSCORES\n"),
        t(
            "Nuevo stream con grupo de consumidores",
            "XGROUP CREATE {name} procesadores $ MKSTREAM\nXADD {name} * evento alta usuario ana\n\
             XREADGROUP GROUP procesadores worker-1 COUNT 10 STREAMS {name} >\nXINFO GROUPS {name}\n",
        ),
        t("Nuevo contador", "SET {name} 0\nINCRBY {name} 5\nGET {name}\n"),
    ]
}

fn opt<'a>(t: &'a TableSchema, key: &str) -> Option<&'a str> {
    t.options.get(key).map(|s| s.as_str()).filter(|s| !s.trim().is_empty())
}

fn line(args: &[&str]) -> String {
    args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ")
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    if t.name.is_empty() {
        return Err(Error::Query("La key necesita un nombre.".into()));
    }
    let key = t.name.as_str();
    let mut out: Vec<String> = Vec::new();
    if parts.drop {
        out.push(line(&["DEL", key]));
    }
    if parts.create {
        let ty = opt(t, "type").unwrap_or("string").trim().to_ascii_lowercase();
        let value = t.options.get("value").map(String::as_str).unwrap_or("");
        let entries = |what: &str| -> Result<()> {
            if t.columns.is_empty() {
                return Err(Error::Query(format!(
                    "Una key de tipo {what} necesita al menos una entrada: Redis no guarda colecciones vacías."
                )));
            }
            Ok(())
        };
        let default = |c: &dbine_driver::ColumnDef| c.default_value.clone().unwrap_or_default();
        let element = |c: &dbine_driver::ColumnDef| c.default_value.clone().filter(|v| !v.is_empty()).unwrap_or_else(|| c.name.clone());
        let mut args: Vec<String> = Vec::new();
        match ty.as_str() {
            "string" => args.extend(["SET".into(), key.into(), value.into()]),
            "hash" => {
                entries("hash")?;
                args.extend(["HSET".into(), key.into()]);
                for c in &t.columns {
                    args.push(c.name.clone());
                    args.push(default(c));
                }
            }
            "list" | "set" => {
                entries(&ty)?;
                args.extend([if ty == "list" { "RPUSH" } else { "SADD" }.into(), key.into()]);
                args.extend(t.columns.iter().map(element));
            }
            "zset" => {
                entries("sorted set")?;
                args.extend(["ZADD".into(), key.into()]);
                for c in &t.columns {
                    let score = c.default_value.clone().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "0".into());
                    let s = score.trim();
                    let ok = s.parse::<f64>().is_ok_and(|f| !f.is_nan()) || matches!(s.to_ascii_lowercase().as_str(), "+inf" | "-inf" | "inf");
                    if !ok {
                        return Err(Error::Query(format!("El score de '{}' no es un número: {score}", c.name)));
                    }
                    args.push(s.to_string());
                    args.push(c.name.clone());
                }
            }
            "stream" => {
                entries("stream")?;
                args.extend(["XADD".into(), key.into(), "*".into()]);
                for c in &t.columns {
                    args.push(c.name.clone());
                    args.push(default(c));
                }
            }
            "json" => {
                let doc = if value.trim().is_empty() { "{}" } else { value.trim() };
                let parsed: Value =
                    serde_json::from_str(doc).map_err(|e| Error::Query(format!("El valor no es un JSON válido: {e}")))?;
                args.extend(["JSON.SET".into(), key.into(), "$".into(), parsed.to_string()]);
            }
            other => return Err(Error::Query(format!("Tipo de key desconocido: {other}"))),
        }
        out.push(line(&args.iter().map(String::as_str).collect::<Vec<_>>()));
        if let Some(ttl) = opt(t, "ttl") {
            let secs: i64 = ttl.trim().parse().map_err(|_| Error::Query(format!("El TTL no es un número entero: {ttl}")))?;
            if secs > 0 {
                out.push(line(&["EXPIRE", key, &secs.to_string()]));
            }
        }
    }
    Ok(if out.is_empty() { String::new() } else { out.join("\n") + "\n" })
}

/// A cell as a Redis argument: text as is, the rest as JSON text.
fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Each row as a hash: `HSET <target>:<first column's value> col value …`,
/// null cells left out.
pub fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    if columns.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    for (n, row) in rows.iter().enumerate() {
        let id = match row.first() {
            Some(Value::Null) | None => {
                return Err(Error::Query(format!(
                    "La fila {} no tiene valor en la primera columna ({}), que forma el nombre de la key.",
                    n + 1,
                    columns[0]
                )))
            }
            Some(v) => cell_text(v),
        };
        let key = format!("{}:{id}", target.name);
        let mut args = vec!["HSET".to_string(), key];
        for (c, v) in columns.iter().zip(row) {
            if !v.is_null() {
                args.push(c.clone());
                args.push(cell_text(v));
            }
        }
        out.push_str(&line(&args.iter().map(String::as_str).collect::<Vec<_>>()));
        out.push('\n');
    }
    Ok(out)
}

/// Lua for an edited `value` cell, whose key type (string, list, set or
/// JSON) the columns don't tell apart: ARGV[1] is the old value, ARGV[2]
/// the new one (missing: the value is removed).
const VALUE_EDIT: &str = "local t = redis.call('TYPE', KEYS[1])['ok'] local old, new = ARGV[1], ARGV[2] \
if t == 'string' then if new then return redis.call('SET', KEYS[1], new) end return redis.call('DEL', KEYS[1]) end \
if t == 'set' then redis.call('SREM', KEYS[1], old) if new then redis.call('SADD', KEYS[1], new) end return 1 end \
if t == 'list' then local i = redis.call('LPOS', KEYS[1], old) if not i then return 0 end \
if new then redis.call('LSET', KEYS[1], i, new) else redis.call('LREM', KEYS[1], 1, old) end return 1 end \
if t == 'ReJSON-RL' then if new then return redis.call('JSON.SET', KEYS[1], '$', new) end return redis.call('DEL', KEYS[1]) end \
return redis.error_reply('tipo de key no editable: ' .. t)";

/// Lua that renames a hash field keeping its value (ARGV: old, new).
const HASH_RENAME: &str = "local v = redis.call('HGET', KEYS[1], ARGV[1]) if not v then return 0 end \
redis.call('HDEL', KEYS[1], ARGV[1]) return redis.call('HSET', KEYS[1], ARGV[2], v)";

/// Lua that renames a sorted set member keeping its score (ARGV: old, new).
const ZSET_RENAME: &str = "local s = redis.call('ZSCORE', KEYS[1], ARGV[1]) if not s then return 0 end \
redis.call('ZREM', KEYS[1], ARGV[1]) return redis.call('ZADD', KEYS[1], s, ARGV[2])";

/// Commands for cells edited while browsing a key. The columns tell the
/// key type (see `columns` in the session): `field`/`value` is a hash
/// (`HSET`, `HDEL` for null), `member`/`score` a sorted set (`ZADD`,
/// `ZREM` for a null score), and a lone `value` is a string, list, set or
/// JSON key, edited with an `EVAL` that checks the type on the server.
/// Renaming a field or member without its value/score keeps the stored one
/// (also with `EVAL`). Stream entries can't be changed.
pub fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    let key = target.name.as_str();
    let get = |pairs: &[(String, Value)], c: &str| pairs.iter().find(|(k, _)| k == c).map(|(_, v)| v.clone());
    let mut out: Vec<String> = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let (k, s) = (&ch.key, &ch.set);
        if let Some(field) = get(k, "field").filter(|v| !v.is_null()) {
            let field = cell_text(&field);
            let new_field = get(s, "field").filter(|v| !v.is_null()).map(|v| cell_text(&v)).filter(|f| *f != field);
            match (new_field, get(s, "value")) {
                (_, Some(Value::Null)) => out.push(line(&["HDEL", key, &field])),
                (None, Some(v)) => out.push(line(&["HSET", key, &field, &cell_text(&v)])),
                (Some(nf), Some(v)) => {
                    out.push(line(&["HDEL", key, &field]));
                    out.push(line(&["HSET", key, &nf, &cell_text(&v)]));
                }
                (Some(nf), None) => out.push(line(&["EVAL", HASH_RENAME, "1", key, &field, &nf])),
                (None, None) => {}
            }
        } else if let Some(member) = get(k, "member").filter(|v| !v.is_null()) {
            let member = cell_text(&member);
            let new_member = get(s, "member").filter(|v| !v.is_null()).map(|v| cell_text(&v)).filter(|m| *m != member);
            match (new_member, get(s, "score")) {
                (_, Some(Value::Null)) => out.push(line(&["ZREM", key, &member])),
                (None, Some(sc)) => out.push(line(&["ZADD", key, &cell_text(&sc), &member])),
                (Some(nm), Some(sc)) => {
                    out.push(line(&["ZREM", key, &member]));
                    out.push(line(&["ZADD", key, &cell_text(&sc), &nm]));
                }
                (Some(nm), None) => out.push(line(&["EVAL", ZSET_RENAME, "1", key, &member, &nm])),
                (None, None) => {}
            }
        } else if get(k, "id").is_some() && get(k, "value").is_none() {
            return Err(Error::Unsupported("las entradas de un stream de Redis no se pueden modificar".into()));
        } else if let Some(new) = get(s, "value") {
            let old = get(k, "value").map(|v| cell_text(&v)).unwrap_or_default();
            let new = (!new.is_null()).then(|| cell_text(&new));
            let mut args = vec!["EVAL", VALUE_EDIT, "1", key, &old];
            if let Some(n) = &new {
                args.push(n);
            }
            out.push(line(&args));
        } else {
            return Err(Error::Unsupported("no se reconoce el tipo de key de Redis a partir de sus columnas".into()));
        }
    }
    Ok(if out.is_empty() { String::new() } else { out.join("\n") + "\n" })
}

/// Commands that remove rows of a browsed key, told apart by the key
/// columns as in [`update_script`]: `HDEL` for a hash field, `ZREM` for a
/// sorted set member, `XDEL` for a stream entry, and for a lone `value`
/// (string, list, set or JSON key) the same `EVAL` as an edit to null:
/// `SREM` / `LREM … 1` for one element, `DEL` for a string or JSON key
/// (its only row).
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let key = target.name.as_str();
    let get = |pairs: &[(String, Value)], c: &str| pairs.iter().find(|(k, _)| k == c).map(|(_, v)| v.clone()).filter(|v| !v.is_null());
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        if let Some(field) = get(k, "field") {
            out.push(line(&["HDEL", key, &cell_text(&field)]));
        } else if let Some(member) = get(k, "member") {
            out.push(line(&["ZREM", key, &cell_text(&member)]));
        } else if let (Some(id), None) = (get(k, "id"), get(k, "value")) {
            out.push(line(&["XDEL", key, &cell_text(&id)]));
        } else if let Some(old) = get(k, "value") {
            out.push(line(&["EVAL", VALUE_EDIT, "1", key, &cell_text(&old)]));
        } else {
            return Err(Error::Unsupported("no se reconoce qué borrar de la key de Redis a partir de sus columnas".into()));
        }
    }
    Ok(if out.is_empty() { String::new() } else { out.join("\n") + "\n" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse_script;
    use dbine_driver::ColumnDef;

    fn key(ty: &str, cols: &[(&str, Option<&str>)]) -> TableSchema {
        let mut t = TableSchema { kind: kinds::KEY.into(), name: "my key".into(), ..Default::default() };
        t.options.insert("type".into(), ty.into());
        t.columns = cols
            .iter()
            .map(|(n, d)| ColumnDef { name: n.to_string(), default_value: d.map(str::to_string), ..Default::default() })
            .collect();
        t
    }

    fn create() -> DdlParts {
        DdlParts { create: true, ..Default::default() }
    }

    /// The script, parsed back by the editor's own parser.
    fn parsed(text: &str) -> Vec<Vec<String>> {
        parse_script(text)
            .unwrap()
            .into_iter()
            .map(|c| c.into_iter().map(|a| String::from_utf8(a).unwrap()).collect())
            .collect()
    }

    #[test]
    fn each_type_round_trips_through_the_parser() {
        let mut s = key("string", &[]);
        s.options.insert("value".into(), "hola \"mundo\"\nsegunda línea".into());
        s.options.insert("ttl".into(), "60".into());
        let ddl = table_ddl(&s, DdlParts { drop: true, create: true, ..Default::default() }).unwrap();
        assert_eq!(
            parsed(&ddl),
            [vec!["DEL", "my key"], vec!["SET", "my key", "hola \"mundo\"\nsegunda línea"], vec!["EXPIRE", "my key", "60"]]
        );

        let h = table_ddl(&key("hash", &[("name", Some("Ana María")), ("empty", None), ("#tag", Some("x"))]), create()).unwrap();
        assert_eq!(parsed(&h), [vec!["HSET", "my key", "name", "Ana María", "empty", "", "#tag", "x"]]);

        let l = table_ddl(&key("list", &[("a", None), ("ignored", Some("b c"))]), create()).unwrap();
        assert_eq!(parsed(&l), [vec!["RPUSH", "my key", "a", "b c"]]);
        let st = table_ddl(&key("set", &[("x", None), ("it's", None)]), create()).unwrap();
        assert_eq!(parsed(&st), [vec!["SADD", "my key", "x", "it's"]]);

        let z = table_ddl(&key("zset", &[("ana", Some("1.5")), ("luis", None), ("max", Some("+inf"))]), create()).unwrap();
        assert_eq!(parsed(&z), [vec!["ZADD", "my key", "1.5", "ana", "0", "luis", "+inf", "max"]]);
        assert!(table_ddl(&key("zset", &[("a", Some("mucho"))]), create()).is_err());

        let x = table_ddl(&key("stream", &[("evento", Some("alta")), ("usuario", Some("ana"))]), create()).unwrap();
        assert_eq!(parsed(&x), [vec!["XADD", "my key", "*", "evento", "alta", "usuario", "ana"]]);

        let mut j = key("json", &[]);
        j.options.insert("value".into(), "{\"a\": [1, \"dos\"], \"b\": \"it's\"}".into());
        assert_eq!(parsed(&table_ddl(&j, create()).unwrap()), [vec!["JSON.SET", "my key", "$", "{\"a\":[1,\"dos\"],\"b\":\"it's\"}"]]);
        j.options.insert("value".into(), "{no".into());
        assert!(table_ddl(&j, create()).is_err());
    }

    #[test]
    fn empty_collections_and_bad_input_are_refused() {
        for ty in ["hash", "list", "set", "zset", "stream"] {
            assert!(table_ddl(&key(ty, &[]), create()).is_err(), "{ty}");
        }
        assert!(table_ddl(&key("bitmap", &[]), create()).is_err());
        let mut s = key("string", &[]);
        s.options.insert("ttl".into(), "soon".into());
        assert!(table_ddl(&s, create()).is_err());
        s.options.insert("ttl".into(), "0".into());
        assert_eq!(parsed(&table_ddl(&s, create()).unwrap()), [vec!["SET", "my key", ""]]);
        // Only DROP.
        let drop = table_ddl(&s, DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap();
        assert_eq!(parsed(&drop), [vec!["DEL", "my key"]]);
    }

    #[test]
    fn rows_become_hashes() {
        let target = ObjectRef { kind: kinds::KEY.into(), schema: None, name: "user".into() };
        let cols = vec!["id".to_string(), "name".into(), "age".into(), "tags".into()];
        let rows = vec![
            vec![Value::from(1), Value::from("Ana \"A\""), Value::from(30), serde_json::json!(["x", "y"])],
            vec![Value::from("b 2"), Value::Null, Value::from(true), Value::Null],
        ];
        let script = insert_script(&target, &cols, &rows).unwrap();
        assert_eq!(
            parsed(&script),
            [
                vec!["HSET", "user:1", "id", "1", "name", "Ana \"A\"", "age", "30", "tags", "[\"x\",\"y\"]"],
                vec!["HSET", "user:b 2", "id", "b 2", "age", "true"],
            ]
        );
        assert!(insert_script(&target, &cols, &[vec![Value::Null, Value::from(1)]]).is_err());
        assert_eq!(insert_script(&target, &[], &rows).unwrap(), "");
    }

    #[test]
    fn edits_by_key_type() {
        let target = ObjectRef { kind: kinds::KEY.into(), schema: None, name: "k 1".into() };
        let ch = |key: Vec<(&str, Value)>, set: Vec<(&str, Value)>| RowChange {
            key: key.into_iter().map(|(c, v)| (c.to_string(), v)).collect(),
            set: set.into_iter().map(|(c, v)| (c.to_string(), v)).collect(), ..Default::default()
        };
        let changes = vec![
            ch(vec![("field", "name".into())], vec![("value", "O'Brien \"Bob\"".into())]),
            ch(vec![("field", "age".into())], vec![("value", Value::Null)]),
            ch(vec![("member", "m".into())], vec![("score", 2.5.into())]),
            ch(vec![("value", "old".into())], vec![("value", "new".into())]),
            ch(vec![("field", "x".into())], vec![]),
        ];
        let script = update_script(&target, &changes).unwrap();
        let head = "HSET \"k 1\" name \"O'Brien \\\"Bob\\\"\"\nHDEL \"k 1\" age\nZADD \"k 1\" 2.5 m\nEVAL ";
        assert!(script.starts_with(head), "{script}");
        assert_eq!(
            parsed(&script),
            [
                vec!["HSET", "k 1", "name", "O'Brien \"Bob\""],
                vec!["HDEL", "k 1", "age"],
                vec!["ZADD", "k 1", "2.5", "m"],
                vec!["EVAL", VALUE_EDIT, "1", "k 1", "old", "new"],
            ]
        );
        let renames = vec![ch(vec![("field", "a".into())], vec![("field", "b".into())]), ch(vec![("member", "a".into())], vec![("score", Value::Null)])];
        assert_eq!(
            parsed(&update_script(&target, &renames).unwrap()),
            [vec!["EVAL", HASH_RENAME, "1", "k 1", "a", "b"], vec!["ZREM", "k 1", "a"]]
        );
        assert!(update_script(&target, &[ch(vec![("id", "1-0".into())], vec![("a", "y".into())])]).is_err());
    }

    #[test]
    fn delete_script_by_key_type() {
        let target = ObjectRef { kind: kinds::KEY.into(), schema: None, name: "my key".into() };
        let keys = vec![
            vec![("field".to_string(), Value::from("O'Brien \"Bob\""))],
            vec![("member".to_string(), Value::from("m 1"))],
            vec![("id".to_string(), Value::from("1-0"))],
            vec![("value".to_string(), Value::from("a b"))],
        ];
        let text = delete_script(&target, &keys).unwrap();
        assert_eq!(
            parsed(&text),
            [
                vec!["HDEL", "my key", "O'Brien \"Bob\""],
                vec!["ZREM", "my key", "m 1"],
                vec!["XDEL", "my key", "1-0"],
                vec!["EVAL", VALUE_EDIT, "1", "my key", "a b"],
            ]
        );
        assert!(text.starts_with("HDEL \"my key\" "), "{text}");
        assert_eq!(delete_script(&target, &[]).unwrap(), "");
        assert!(delete_script(&target, &[vec![]]).is_err());
        assert!(delete_script(&target, &[vec![("field".to_string(), Value::Null)]]).is_err());
    }

    #[test]
    fn templates_parse() {
        let ts = templates();
        assert!(ts.len() >= 5);
        for t in ts {
            let text = t.template.replace("{name}", "demo");
            assert!(!parsed(&text).is_empty(), "{}", t.label);
        }
    }
}

//! Collection designer, templates and INSERT scripts in SQL++.
//!
//! Objects carry `bucket.scope` as their schema (scope and collection
//! names can't contain dots, bucket names can: the split is at the last
//! dot), so every generated statement uses the full keyspace path
//! `` `bucket`.`scope`.`collection` ``.
//!
//! The designer creates a collection: `maxTTL` and a primary index are
//! table options; the columns aren't stored (documents have no schema) but
//! the designer's indexes become `CREATE INDEX` on their fields.

use dbine_driver::{
    kinds, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef, ObjectRef, Result, RowChange,
    TableSchema,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn q(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// `(bucket, scope)` of a `bucket.scope` schema.
pub fn split_schema(schema: &str) -> Option<(&str, &str)> {
    schema.rsplit_once('.').filter(|(b, s)| !b.is_empty() && !s.is_empty())
}

/// The keyspace path of an object: `` `b`.`s`.`c` `` with a `bucket.scope`
/// schema; just the collection (resolved by the query context) otherwise.
pub fn path(schema: Option<&str>, name: &str) -> String {
    match schema.and_then(split_schema) {
        Some((b, s)) => format!("{}.{}.{}", q(b), q(s), q(name)),
        None => q(name),
    }
}

/// A scope's full path, `` default:`bucket`.`scope` ``, from its schema name
/// (`bucket.scope`, as the explorer names it). The bucket must be written:
/// the session's query context is the bucket's `_default` scope, which
/// can't resolve a bare scope name.
pub fn scope_ref(schema: &str) -> Result<String> {
    split_schema(schema.trim()).map(|(b, s)| format!("default:{}.{}", q(b), q(s))).ok_or_else(|| {
        Error::Query(format!("escribí el scope con su bucket, «bucket.scope» (por ejemplo «mibucket.{}»)", schema.trim()))
    })
}

/// `bucket.scope` from the dialog's name: a bare scope (scopes have no
/// dots) goes into the bucket the menu was opened on; a name that already
/// has its bucket is left as it is.
pub fn full_scope(database: Option<&str>, name: &str) -> String {
    let n = name.trim();
    match database.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) if !n.contains('.') => format!("{b}.{n}"),
        _ => n.to_string(),
    }
}

/// "Nuevo esquema…": a scope in a bucket. Scopes have no owner.
pub fn create_scope(schema: &str, owner: Option<&str>) -> Result<String> {
    if owner.is_some() {
        return Err(Error::Unsupported("en Couchbase un scope no tiene dueño: otorgá roles sobre él".into()));
    }
    Ok(format!("CREATE SCOPE {}", scope_ref(schema)?))
}

/// "Borrar esquema…": `DROP SCOPE` always drops the scope's collections
/// with it, so it's written only when the user asked for that (`cascade`).
pub fn drop_scope(schema: &str, cascade: bool) -> Result<String> {
    let path = scope_ref(schema)?;
    if split_schema(schema.trim()).is_some_and(|(_, s)| s == "_default") {
        return Err(Error::Unsupported("el scope _default de un bucket no se puede borrar".into()));
    }
    if !cascade {
        return Err(Error::Unsupported("Couchbase borra el scope con todas sus colecciones: marcá «con su contenido» para confirmarlo".into()));
    }
    Ok(format!("DROP SCOPE {path}"))
}

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        kind: kinds::COLLECTION,
        label: "Nueva colección",
        data_types: Vec::new(),
        schemas: true,
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        indexes: true,
        foreign_keys: false,
        column_options: Vec::new(),
        table_options: vec![
            Field::new("max_ttl", "TTL máximo (segundos)", FieldKind::Number)
                .placeholder("(el del bucket)")
                .help("Los documentos vencen a los N segundos; 0 = sin vencimiento. Solo en Couchbase Enterprise."),
            Field::new("primary_index", "Crear índice primario", FieldKind::Bool)
                .help("Permite consultar cualquier campo sin índices secundarios (más lento en colecciones grandes)."),
        ],
        columns_required: false,
    }
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    if t.schema.as_deref().and_then(split_schema).is_none() {
        return Err(Error::Query("Elegí el bucket y el scope de la colección (bucket.scope).".into()));
    }
    let name = path(t.schema.as_deref(), &t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP COLLECTION {name}{};", if parts.if_exists { " IF EXISTS" } else { "" }));
    }
    if parts.create {
        let mut s = format!("CREATE COLLECTION {name}{}", if parts.if_exists && !parts.drop { " IF NOT EXISTS" } else { "" });
        if let Some(ttl) = t.options.get("max_ttl").map(|v| v.trim()).filter(|v| !v.is_empty()) {
            let n: i64 = ttl.parse().map_err(|_| Error::Query(format!("TTL inválido: {ttl}")))?;
            s.push_str(&format!(" WITH {{\"maxTTL\": {n}}}"));
        }
        s.push(';');
        out.push(s);
        if t.options.get("primary_index").is_some_and(|v| v == "true") {
            out.push(format!("CREATE PRIMARY INDEX{} ON {name};", if parts.if_exists { " IF NOT EXISTS" } else { "" }));
        }
    }
    if parts.indexes {
        for ix in &t.indexes {
            out.push(index_ddl(&name, ix, parts.if_exists)?);
        }
    }
    Ok(out.join("\n"))
}

/// Kind of a primary index ([`IndexDef::kind`]).
pub const PRIMARY: &str = "PRIMARY";

/// A GSI index from its `system:indexes` row: `index_key` (the key
/// expressions as the server writes them, `DESC` / `INCLUDE MISSING`
/// included) as columns, `condition` as filter, a primary index as kind
/// `PRIMARY`, and in options the `PARTITION BY` expression and the `WITH`
/// settings that aren't the default (`num_replica`, `num_partition`,
/// `retain_deleted_xattr`).
pub fn index_from_row(r: &Value) -> IndexDef {
    let s = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_string).filter(|v| !v.is_empty());
    let primary = r.get("is_primary").and_then(Value::as_bool).unwrap_or(false);
    let mut options = BTreeMap::new();
    let partition = s("partition");
    if let Some(p) = &partition {
        options.insert("partition".to_string(), p.clone());
    }
    if let Some(w) = r.get("with").and_then(Value::as_object) {
        for (k, v) in w {
            let default = match k.as_str() {
                "num_replica" => v.as_i64() == Some(0),
                "num_partition" => partition.is_none() || v.as_i64() == Some(8),
                "retain_deleted_xattr" | "defer_build" => v.as_bool() == Some(false),
                // How the server placed it, not what was asked.
                "nodes" | "stats" => true,
                _ => false,
            };
            if !default && !v.is_null() {
                options.insert(k.clone(), v.to_string());
            }
        }
    }
    IndexDef {
        name: s("name").unwrap_or_default(),
        columns: r.get("index_key").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect(),
        kind: primary.then(|| PRIMARY.to_string()),
        filter: s("condition"),
        options,
        ..Default::default()
    }
}

/// `CREATE [PRIMARY] INDEX … ON <keyspace>` for an index.
pub fn index_ddl(keyspace: &str, ix: &IndexDef, if_not_exists: bool) -> Result<String> {
    let ine = if if_not_exists { " IF NOT EXISTS" } else { "" };
    let primary = ix.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(PRIMARY));
    let name = if ix.name.trim().is_empty() || ix.name == "#primary" { String::new() } else { format!(" {}", q(&ix.name)) };
    let mut s = if primary {
        format!("CREATE PRIMARY INDEX{name}{ine} ON {keyspace}")
    } else {
        if ix.columns.is_empty() {
            return Err(Error::Query(format!("El índice {} no tiene campos.", ix.name)));
        }
        let cols: Vec<String> = ix.columns.iter().map(|c| field_path(c)).collect();
        format!("CREATE INDEX{name}{ine} ON {keyspace}({})", cols.join(", "))
    };
    if let Some(p) = ix.options.get("partition").filter(|p| !p.trim().is_empty()) {
        s.push_str(&format!(" PARTITION BY {p}"));
    }
    if let Some(w) = ix.filter.as_deref().filter(|w| !w.trim().is_empty()) {
        s.push_str(&format!(" WHERE {w}"));
    }
    let with: Vec<String> = ix
        .options
        .iter()
        .filter(|(k, _)| *k != "partition")
        .map(|(k, v)| format!("{}: {}", Value::String(k.clone()), serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.clone()))))
        .collect();
    if !with.is_empty() {
        s.push_str(&format!(" WITH {{{}}}", with.join(", ")));
    }
    s.push(';');
    Ok(s)
}

/// A field for an index key: `a.b` → `` `a`.`b` ``; expressions (with
/// parentheses or spaces) go as typed.
fn field_path(f: &str) -> String {
    if f.contains(['(', ' ', '`', '[']) {
        f.to_string()
    } else {
        f.split('.').map(q).collect::<Vec<_>>().join(".")
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    let t = |kind: &'static str, label: &'static str, template: &str| CreateTemplate { kind, label, template: template.into() };
    vec![
        t(
            kinds::INDEX,
            "Nuevo índice",
            "-- {schema} es bucket.scope; cambiá `coleccion` por la colección a indexar.\nCREATE INDEX `{name}` ON {schema}.`coleccion`(`tipo`, `fecha`)\nWHERE `tipo` IS NOT MISSING;\n",
        ),
        t(kinds::INDEX, "Nuevo índice primario", "CREATE PRIMARY INDEX `{name}` ON {schema}.`coleccion`;\n"),
        t(
            kinds::FUNCTION,
            "Nueva función (UDF)",
            "CREATE OR REPLACE FUNCTION `{name}`(precio, iva) {\n    precio * (1 + iva / 100)\n};\n\nEXECUTE FUNCTION `{name}`(100, 21);\n",
        ),
        t(
            kinds::COLLECTION,
            "Nuevo scope con colección",
            "-- Reemplazá `bucket` por el bucket.\nCREATE SCOPE `bucket`.`{name}` IF NOT EXISTS;\nCREATE COLLECTION `bucket`.`{name}`.`documentos` IF NOT EXISTS;\n",
        ),
    ]
}

/// Browsing turns nested values into JSON text; back into JSON here.
fn json_value(v: &Value) -> Value {
    if let Value::String(s) = v {
        let t = s.trim_start();
        if t.starts_with('{') || t.starts_with('[') {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                return parsed;
            }
        }
    }
    v.clone()
}

/// `INSERT INTO path (KEY, VALUE) VALUES (…), …`, 100 documents per
/// statement. The key is the `_id` column (what browsing shows), else
/// `id` / `key`, else `UUID()`; the other columns form the document.
pub fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    let name = path(target.schema(), &target.name);
    let key_col = ["_id", "id", "key", "meta_id"].iter().find_map(|k| columns.iter().position(|c| c == k));
    let drop_key = key_col.is_some_and(|i| columns[i] == "_id" || columns[i] == "meta_id");
    let mut stmts = Vec::new();
    for chunk in rows.chunks(100) {
        let values: Vec<String> = chunk
            .iter()
            .map(|r| {
                let key = match key_col.and_then(|i| r.get(i)).filter(|v| !v.is_null()) {
                    Some(Value::String(s)) => serde_json::to_string(s).unwrap_or_default(),
                    Some(v) => serde_json::to_string(&v.to_string()).unwrap_or_default(),
                    None => "UUID()".into(),
                };
                let doc: serde_json::Map<String, Value> = columns
                    .iter()
                    .zip(r)
                    .enumerate()
                    .filter(|(i, (_, v))| !(drop_key && Some(*i) == key_col) && !v.is_null())
                    .map(|(_, (c, v))| (c.clone(), json_value(v)))
                    .collect();
                format!("({key}, {})", Value::Object(doc))
            })
            .collect();
        stmts.push(format!("INSERT INTO {name} (KEY, VALUE) VALUES\n  {};", values.join(",\n  ")));
    }
    Ok(stmts.join("\n"))
}

/// `UPDATE path [USE KEYS …] SET … [WHERE …]` per edited document. A key with `_id` / `meta_id`
/// (the document key browsing shows) goes as `USE KEYS`, which needs no
/// index; other keys become a `WHERE` on those fields. Values are JSON
/// literals (valid SQL++), null ones set to null.
pub fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    let name = path(target.schema(), &target.name);
    let mut stmts = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let set: Vec<String> = ch.set.iter().map(|(c, v)| format!("{} = {}", q(c), json_value(v))).collect();
        let doc_key = ch.key.iter().find(|(k, v)| (k == "_id" || k == "meta_id") && !v.is_null());
        let (keys, filter) = match doc_key {
            Some((_, v)) => {
                let id = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (format!(" USE KEYS {}", Value::String(id)), String::new())
            }
            None if ch.key.is_empty() => {
                return Err(Error::Unsupported("no se puede identificar el documento a actualizar".into()));
            }
            None => {
                let conds: Vec<String> = ch
                    .key
                    .iter()
                    .map(|(c, v)| if v.is_null() { format!("{} IS NULL", q(c)) } else { format!("{} = {}", q(c), json_value(v)) })
                    .collect();
                (String::new(), format!(" WHERE {}", conds.join(" AND ")))
            }
        };
        stmts.push(format!("UPDATE {name}{keys} SET {}{filter};", set.join(", ")));
    }
    Ok(stmts.join("\n"))
}

/// `DELETE FROM path USE KEYS …` / `DELETE FROM path WHERE …` per
/// document, addressed as in [`update_script`]. An empty key is refused
/// (it would delete the whole collection).
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let name = path(target.schema(), &target.name);
    let mut stmts = Vec::new();
    for key in keys {
        let doc_key = key.iter().find(|(k, v)| (k == "_id" || k == "meta_id") && !v.is_null());
        let tail = match doc_key {
            Some((_, v)) => {
                let id = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                format!(" USE KEYS {}", Value::String(id))
            }
            None if key.is_empty() => {
                return Err(Error::Unsupported("no se puede identificar el documento a borrar".into()));
            }
            None => {
                let conds: Vec<String> = key
                    .iter()
                    .map(|(c, v)| if v.is_null() { format!("{} IS NULL", q(c)) } else { format!("{} = {}", q(c), json_value(v)) })
                    .collect();
                format!(" WHERE {}", conds.join(" AND "))
            }
        };
        stmts.push(format!("DELETE FROM {name}{tail};"));
    }
    Ok(stmts.join("\n"))
}

/// The browse query (`SELECT META(d).id AS _id, d.* FROM … AS d LIMIT n`)
/// restricted by the grid's column filters, in SQL++: fields as
/// `` d.`f` `` (`_id` is `META(d).id`), JSON literals, `[…]` lists, LIKE
/// with its default `\` escape, and a missing field counts as null.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, FilterOp};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let like_escape = |s: &str| s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let mut parts = Vec::new();
    for f in filters {
        let c = if f.column == "_id" { "META(d).id".to_string() } else { format!("d.{}", q(&f.column)) };
        let lit = |v: &Value| json_value(v).to_string();
        let first = || f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let text = || first().map(|v| like_escape(&v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())));
        let like = |p: String| Value::String(p).to_string();
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
            }
            Ok(f.values.iter().map(lit).collect::<Vec<_>>().join(", "))
        };
        let sql = || f.sql.as_deref().unwrap_or("").trim().to_string();
        parts.push(match f.op {
            FilterOp::Eq => format!("{c} = {}", lit(first()?)),
            FilterOp::Ne => format!("{c} != {}", lit(first()?)),
            FilterOp::Gt => format!("{c} > {}", lit(first()?)),
            FilterOp::Ge => format!("{c} >= {}", lit(first()?)),
            FilterOp::Lt => format!("{c} < {}", lit(first()?)),
            FilterOp::Le => format!("{c} <= {}", lit(first()?)),
            FilterOp::Contains => format!("{c} LIKE {}", like(format!("%{}%", text()?))),
            FilterOp::NotContains => format!("{c} NOT LIKE {}", like(format!("%{}%", text()?))),
            FilterOp::StartsWith => format!("{c} LIKE {}", like(format!("{}%", text()?))),
            FilterOp::EndsWith => format!("{c} LIKE {}", like(format!("%{}", text()?))),
            FilterOp::IsNull => format!("{c} IS NOT VALUED"),
            FilterOp::NotNull => format!("{c} IS VALUED"),
            FilterOp::IsEmpty => format!("{c} = \"\""),
            FilterOp::NotEmpty => format!("({c} IS VALUED AND {c} != \"\")"),
            FilterOp::In => format!("{c} IN [{}]", list()?),
            FilterOp::NotIn => format!("{c} NOT IN [{}]", list()?),
            FilterOp::IsTrue => format!("{c} = true"),
            FilterOp::IsFalse => format!("{c} = false"),
            FilterOp::TrueOrNull => format!("({c} = true OR {c} IS NOT VALUED)"),
            FilterOp::FalseOrNull => format!("({c} = false OR {c} IS NOT VALUED)"),
            FilterOp::Sql => format!("({})", sql()),
            FilterOp::SqlRight => format!("{c} {}", sql()),
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn indexes_from_the_catalog_and_back() {
        let row = json!({"name": "ix_tags", "index_key": ["(distinct (array `t` for `t` in `tags` end))", "`fecha` DESC"],
            "condition": "(`tipo` = \"x\")", "partition": "HASH(`fecha`)",
            "with": {"num_partition": 4, "num_replica": 0, "retain_deleted_xattr": false, "nodes": ["n1:8091"]}});
        let ix = index_from_row(&row);
        assert_eq!(ix.columns, vec!["(distinct (array `t` for `t` in `tags` end))", "`fecha` DESC"]);
        assert_eq!(ix.filter.as_deref(), Some("(`tipo` = \"x\")"));
        assert_eq!(ix.options.len(), 2, "{:?}", ix.options);
        assert_eq!(
            index_ddl("`b`.`s`.`c`", &ix, false).unwrap(),
            "CREATE INDEX `ix_tags` ON `b`.`s`.`c`((distinct (array `t` for `t` in `tags` end)), `fecha` DESC) PARTITION BY HASH(`fecha`) WHERE (`tipo` = \"x\") WITH {\"num_partition\": 4};"
        );
        let p = index_from_row(&json!({"name": "#primary", "is_primary": true, "with": {"num_replica": 1}}));
        assert_eq!(p.kind.as_deref(), Some(PRIMARY));
        assert_eq!(index_ddl("k", &p, false).unwrap(), "CREATE PRIMARY INDEX ON k WITH {\"num_replica\": 1};");
        let named = IndexDef { name: "pk".into(), ..p };
        assert_eq!(index_ddl("k", &named, true).unwrap(), "CREATE PRIMARY INDEX `pk` IF NOT EXISTS ON k WITH {\"num_replica\": 1};");
    }

    #[test]
    fn filtered_browse_in_sqlpp() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT META(d).id AS _id, d.*\nFROM `b`.`s`.`c` AS d\nLIMIT 200",
                &[
                    f("_id", FilterOp::Eq, vec![json!("user::1")]),
                    f("name", FilterOp::Eq, vec![json!("O'Brien \"Jr\"")]),
                    f("note", FilterOp::Contains, vec![json!("50%")]),
                    f("age", FilterOp::Ge, vec![json!(18)]),
                    f("gone", FilterOp::IsNull, vec![]),
                    f("tag", FilterOp::In, vec![json!("a"), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT META(d).id AS _id, d.*\nFROM `b`.`s`.`c` AS d\nWHERE META(d).id = \"user::1\"\n  AND d.`name` = \"O'Brien \\\"Jr\\\"\"\n  AND d.`note` LIKE \"%50\\\\%%\"\n  AND d.`age` >= 18\n  AND d.`gone` IS NOT VALUED\n  AND d.`tag` IN [\"a\", 2]\nLIMIT 200"
        );
    }

    #[test]
    fn update_script_by_key_and_fields() {
        let target = ObjectRef { kind: kinds::COLLECTION.into(), schema: Some("b.s".into()), name: "c".into() };
        let changes = vec![
            RowChange {
                key: vec![("_id".into(), json!("k'1"))],
                set: vec![("name".into(), json!("O'Brien \"Bob\"")), ("n".into(), Value::Null)], ..Default::default()
            },
            RowChange { key: vec![("a".into(), json!(1)), ("b".into(), Value::Null)], set: vec![("x".into(), json!("{\"y\":2}"))], ..Default::default() },
            RowChange { key: vec![("_id".into(), json!("z"))], set: vec![], ..Default::default() },
        ];
        assert_eq!(
            update_script(&target, &changes).unwrap(),
            "UPDATE `b`.`s`.`c` USE KEYS \"k'1\" SET `name` = \"O'Brien \\\"Bob\\\"\", `n` = null;\n\
             UPDATE `b`.`s`.`c` SET `x` = {\"y\":2} WHERE `a` = 1 AND `b` IS NULL;"
        );
    }

    #[test]
    fn delete_script_by_key_and_fields() {
        let target = ObjectRef { kind: kinds::COLLECTION.into(), schema: Some("b.s".into()), name: "c".into() };
        let keys = vec![
            vec![("_id".into(), json!("k'1 \"x\""))],
            vec![("a".into(), json!("O'Brien")), ("b".into(), Value::Null)],
        ];
        assert_eq!(
            delete_script(&target, &keys).unwrap(),
            "DELETE FROM `b`.`s`.`c` USE KEYS \"k'1 \\\"x\\\"\";\n\
             DELETE FROM `b`.`s`.`c` WHERE `a` = \"O'Brien\" AND `b` IS NULL;"
        );
        assert!(delete_script(&target, &[vec![]]).is_err());
    }

    #[test]
    fn paths() {
        assert_eq!(path(Some("travel.sample.inventory"), "hotel"), "`travel.sample`.`inventory`.`hotel`");
        assert_eq!(path(None, "c"), "`c`");
        assert_eq!(split_schema("b._default"), Some(("b", "_default")));
        assert_eq!(split_schema("b"), None);
    }

    #[test]
    fn collection_ddl() {
        let t = TableSchema {
            kind: kinds::COLLECTION.into(),
            schema: Some("dbine.s1".into()),
            name: "pedidos".into(),
            indexes: vec![IndexDef { name: "ix_cliente".into(), columns: vec!["cliente.id".into(), "fecha".into()], filter: Some("tipo = 'x'".into()), ..Default::default() }],
            options: [("max_ttl".to_string(), "3600".to_string()), ("primary_index".to_string(), "true".to_string())].into(),
            ..Default::default()
        };
        let s = table_ddl(&t, DdlParts { drop: true, if_exists: true, create: true, indexes: true, ..Default::default() }).unwrap();
        assert_eq!(
            s,
            "DROP COLLECTION `dbine`.`s1`.`pedidos` IF EXISTS;\nCREATE COLLECTION `dbine`.`s1`.`pedidos` WITH {\"maxTTL\": 3600};\nCREATE PRIMARY INDEX IF NOT EXISTS ON `dbine`.`s1`.`pedidos`;\nCREATE INDEX `ix_cliente` IF NOT EXISTS ON `dbine`.`s1`.`pedidos`(`cliente`.`id`, `fecha`) WHERE tipo = 'x';"
        );
        let mut bad = t.clone();
        bad.schema = None;
        assert!(table_ddl(&bad, DdlParts { create: true, ..Default::default() }).is_err());
    }

    #[test]
    fn inserts_keep_keys_and_nesting() {
        let target = ObjectRef { kind: kinds::COLLECTION.into(), schema: Some("b.s".into()), name: "c".into() };
        let s = insert_script(&target, &["_id".into(), "nombre".into(), "tags".into(), "x".into()], &[
            vec![json!("k1"), json!("O'Brien"), json!("[\"a\",\"b\"]"), Value::Null],
            vec![Value::Null, json!("Ana"), json!("{no json"), json!(2)],
        ])
        .unwrap();
        assert_eq!(
            s,
            "INSERT INTO `b`.`s`.`c` (KEY, VALUE) VALUES\n  (\"k1\", {\"nombre\":\"O'Brien\",\"tags\":[\"a\",\"b\"]}),\n  (UUID(), {\"nombre\":\"Ana\",\"tags\":\"{no json\",\"x\":2});"
        );
        let s = insert_script(&target, &["id".into(), "v".into()], &[vec![json!(7), json!(1)]]).unwrap();
        assert!(s.contains("(\"7\", {\"id\":7,\"v\":1})"), "{s}");
    }
    #[test]
    fn scope_scripts() {
        assert_eq!(create_scope("travel.sample.ventas", None).unwrap(), "CREATE SCOPE default:`travel.sample`.`ventas`");
        assert_eq!(create_scope(" b.v`x ", None).unwrap(), "CREATE SCOPE default:`b`.`v``x`");
        assert!(matches!(create_scope("ventas", None), Err(Error::Query(m)) if m.contains("bucket.scope")));
        assert!(matches!(create_scope("b.ventas", Some("ana")), Err(Error::Unsupported(_))));
        assert_eq!(drop_scope("b.ventas", true).unwrap(), "DROP SCOPE default:`b`.`ventas`");
        assert!(matches!(drop_scope("b.ventas", false), Err(Error::Unsupported(_))));
        assert!(matches!(drop_scope("b._default", true), Err(Error::Unsupported(_))));
        assert_eq!(full_scope(Some("travel.sample"), " ventas "), "travel.sample.ventas");
        assert_eq!(full_scope(Some("travel.sample"), "travel.sample.ventas"), "travel.sample.ventas");
        assert_eq!(full_scope(Some(" "), "ventas"), "ventas");
        assert_eq!(full_scope(None, "b.ventas"), "b.ventas");
    }
}

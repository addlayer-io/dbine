//! Generated scripts and the schema model, in the driver's HTTP console
//! syntax (`METHOD path [JSON body]`). Every path is relative to the
//! session's database, so a script runs against whichever database it's
//! opened on.
//!
//! CouchDB has no table designer: the container of documents is the
//! database itself (created with [`crate::CouchSession`]'s
//! `create_database`) and documents have no schema. What a database holds
//! besides documents — Mango indexes and design documents (views,
//! validation functions) — is created from templates, and
//! `database_schema` reports it on the `_all_docs` pseudo-table:
//! - `indexes`: the Mango indexes (`GET /db/_index`), as [`IndexDef`]:
//!   `columns` are `field` or `field:desc`, `kind` is `json` (default) or
//!   `text`, `filter` is the `partial_filter_selector` JSON.
//! - `options["design_docs"]`: the design documents (without `_rev` and
//!   without the ones Mango indexes live in), a JSON array.
//!
//! [`table_ddl`] turns that back into `PUT _design/…` (create) and
//! `POST _index` (indexes) lines. `drop` generates nothing: a document
//! can't be deleted without its current revision, and the way to start
//! over in CouchDB is dropping and creating the database.

use crate::seg;
use dbine_driver::{
    kinds, CheckDef, CreateTemplate, DdlParts, Error, IndexDef, KeyDef, ObjectRef, Result, RowChange, TableSchema,
};
use serde_json::{json, Map, Value};

pub fn templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(
            kinds::VIEW,
            "Nueva vista (documento de diseño)",
            r#"PUT _design/{name}
{
  "language": "javascript",
  "views": {
    "por_tipo": {
      "map": "function (doc) { if (doc.tipo) { emit(doc.tipo, 1); } }",
      "reduce": "_count"
    }
  }
}"#,
        ),
        t(
            kinds::INDEX,
            "Nuevo índice Mango",
            r#"POST _index
{
  "index": { "fields": ["tipo", "fecha"] },
  "name": "{name}",
  "ddoc": "{name}",
  "type": "json"
}"#,
        ),
        t(
            "validation",
            "Nueva validación (validate_doc_update)",
            r#"PUT _design/{name}
{
  "language": "javascript",
  "validate_doc_update": "function (newDoc, oldDoc, userCtx) { if (!newDoc._deleted && !newDoc.tipo) { throw({ forbidden: 'falta tipo' }); } }"
}"#,
        ),
    ]
}

/// The `POST _index` body of an index (see the module docs).
pub(crate) fn index_body(ix: &IndexDef) -> Result<Value> {
    let fields: Vec<Value> = ix
        .columns
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|c| match c.rsplit_once(':') {
            Some((f, d)) if d.eq_ignore_ascii_case("desc") || d == "-1" => json!({ f.trim(): "desc" }),
            Some((f, d)) if d.eq_ignore_ascii_case("asc") || d == "1" => Value::String(f.trim().into()),
            _ => Value::String(c.into()),
        })
        .collect();
    if fields.is_empty() {
        return Err(Error::Query(format!("el índice {} no tiene campos", ix.name)));
    }
    let mut index = Map::new();
    index.insert("fields".into(), Value::Array(fields));
    if let Some(f) = ix.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        let sel: Value = serde_json::from_str(f)
            .map_err(|e| Error::Query(format!("filtro del índice {}: JSON inválido ({e})", ix.name)))?;
        index.insert("partial_filter_selector".into(), sel);
    }
    let kind = match ix.kind.as_deref().map(str::trim).unwrap_or("") {
        "" | "json" => "json",
        "text" => "text",
        other => return Err(Error::Query(format!("índice {}: tipo «{other}» desconocido (json o text)", ix.name))),
    };
    let mut body = Map::new();
    body.insert("index".into(), Value::Object(index));
    if !ix.name.trim().is_empty() {
        body.insert("name".into(), ix.name.trim().into());
    }
    body.insert("type".into(), kind.into());
    Ok(Value::Object(body))
}

/// A design document's validation function.
pub(crate) const VALIDATE: &str = "validate_doc_update";

/// The CHECKs that are validation functions (JavaScript); a SQL condition
/// carried from another engine isn't one and is left out.
pub(crate) fn validators(t: &TableSchema) -> impl Iterator<Item = &CheckDef> {
    t.checks.iter().filter(|c| c.expression.trim_start().starts_with("function"))
}

/// The design document a validation CHECK belongs to (its name,
/// `_design/…`).
pub(crate) fn validator_id(c: &CheckDef) -> String {
    let n = c.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or("validacion");
    if n.starts_with("_design/") {
        n.to_string()
    } else {
        format!("_design/{n}")
    }
}

/// `PUT _design/…` of a design document holding just a validation
/// function.
pub(crate) fn validator_doc(c: &CheckDef) -> Result<String> {
    if c.expression.trim().is_empty() {
        return Err(Error::Query(format!("la validación {} no tiene función", validator_id(c))));
    }
    let id = validator_id(c);
    let doc = json!({ "_id": id, VALIDATE: c.expression });
    Ok(format!("PUT _design/{}\n{doc}", seg(id.trim_start_matches("_design/"))))
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    if t.kind == kinds::VIEW {
        return Err(Error::Unsupported(
            "las vistas de CouchDB viven en documentos de diseño; se generan con la tabla _all_docs".into(),
        ));
    }
    let mut lines = Vec::new();
    if parts.create {
        let mut ids = Vec::new();
        if let Some(text) = t.options.get("design_docs").filter(|s| !s.trim().is_empty()) {
            let docs: Vec<Value> =
                serde_json::from_str(text).map_err(|e| Error::Query(format!("design_docs: JSON inválido ({e})")))?;
            for mut d in docs {
                let id = d.get("_id").and_then(Value::as_str).unwrap_or_default().to_string();
                let Some(name) = id.strip_prefix("_design/").filter(|n| !n.is_empty()) else {
                    return Err(Error::Query(format!("documento de diseño sin _id «_design/…»: {d}")));
                };
                if let Some(m) = d.as_object_mut() {
                    m.remove("_rev");
                    // Its validation function is the CHECK's.
                    if let Some(c) = validators(t).find(|c| validator_id(c) == id) {
                        m.insert(VALIDATE.into(), c.expression.clone().into());
                    }
                }
                lines.push(format!("PUT _design/{}\n{d}", seg(name)));
                ids.push(id);
            }
        }
        for c in validators(t).filter(|c| !ids.contains(&validator_id(c))) {
            lines.push(validator_doc(c)?);
        }
    }
    if parts.indexes {
        for ix in &t.indexes {
            lines.push(format!("POST _index\n{}", index_body(ix)?));
        }
    }
    Ok(lines.join("\n"))
}

/// Documents per `_bulk_docs` request.
const BATCH: usize = 500;

/// `POST _bulk_docs` batches into the session's database. Null cells are
/// left out and `_rev` is dropped (the revision belongs to the source).
pub fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    if target.kind == kinds::VIEW {
        return Err(Error::Unsupported("no se insertan documentos en una vista de CouchDB".into()));
    }
    let mut out = Vec::new();
    for chunk in rows.chunks(BATCH) {
        let docs: Vec<String> = chunk
            .iter()
            .map(|row| {
                let m: Map<String, Value> = columns
                    .iter()
                    .zip(row)
                    .filter(|(c, v)| !v.is_null() && c.as_str() != "_rev")
                    .map(|(c, v)| (c.clone(), v.clone()))
                    .collect();
                format!("  {}", Value::Object(m))
            })
            .collect();
        out.push(format!("POST _bulk_docs\n{{\"docs\": [\n{}\n]}}", docs.join(",\n")));
    }
    Ok(out.join("\n"))
}

/// Design document holding the update handler [`update_script`] calls.
const UPDATE_DDOC: &str = "dbine_update";

/// Update handler that merges the request body's fields into the document.
const UPDATE_FN: &str =
    "function(doc, req) { if (!doc) { return [null, 'no existe el documento']; } var s = JSON.parse(req.body); for (var k in s) { doc[k] = s[k]; } return [doc, 'ok']; }";

/// Edited documents. A CouchDB write replaces the whole document and needs
/// its current `_rev`, which the grid doesn't carry, so the script first
/// creates a design document with an update handler and then calls it per
/// document (`PUT _design/dbine_update/_update/set/<id>`), which merges the
/// edited fields into the stored revision. Null values are set to null.
pub fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    if target.kind == kinds::VIEW {
        return Err(Error::Unsupported("no se editan documentos en una vista de CouchDB".into()));
    }
    let mut lines = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let id = match ch.key.iter().find(|(k, _)| k == "_id").map(|(_, v)| v) {
            Some(Value::String(s)) => s.clone(),
            Some(v) if !v.is_null() => v.to_string(),
            _ => return Err(Error::Unsupported("para editar un documento de CouchDB hace falta su _id".into())),
        };
        // Sorted: the same text whether or not serde_json keeps insertion
        // order (a workspace feature can turn it on).
        let mut fields: Vec<(String, Value)> = ch.set.iter().filter(|(c, _)| c != "_id" && c != "_rev").cloned().collect();
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        let m: Map<String, Value> = fields.into_iter().collect();
        if m.is_empty() {
            continue;
        }
        lines.push(format!("PUT _design/{UPDATE_DDOC}/_update/set/{}\n{}", seg(&id), Value::Object(m)));
    }
    if lines.is_empty() {
        return Ok(String::new());
    }
    let ddoc = json!({ "updates": { "set": UPDATE_FN } });
    let head = format!(
        "// Crea la función de actualización (si _design/{UPDATE_DDOC} ya existe, borrá estas líneas)\nPUT _design/{UPDATE_DDOC}\n{ddoc}"
    );
    Ok(std::iter::once(head).chain(lines).collect::<Vec<_>>().join("\n"))
}

/// Design document holding the update handler [`delete_script`] calls
/// (its own, so a sync script can create it next to [`UPDATE_DDOC`]).
const DELETE_DDOC: &str = "dbine_delete";

/// Update handler that marks the document deleted (a tombstone on the
/// stored revision, as `DELETE /db/id?rev=…` does).
const DELETE_FN: &str =
    "function(doc, req) { if (!doc) { return [null, 'no existe el documento']; } doc._deleted = true; return [doc, 'ok']; }";

/// Deleted documents. `DELETE /db/<id>?rev=…` needs the current `_rev`,
/// which the key doesn't carry (and a compare's source revision wouldn't
/// match the target's), so, as in [`update_script`], the script creates a
/// design document with an update handler and calls it per document
/// (`PUT _design/dbine_delete/_update/delete/<id>`), which deletes the
/// stored revision. Only the `_id` addresses the document.
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    if target.kind == kinds::VIEW {
        return Err(Error::Unsupported("no se borran documentos en una vista de CouchDB".into()));
    }
    let mut lines = Vec::new();
    for key in keys {
        let id = match key.iter().find(|(k, _)| k == "_id").map(|(_, v)| v) {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(v) if !v.is_null() && !v.is_string() => v.to_string(),
            _ => return Err(Error::Unsupported("para borrar un documento de CouchDB hace falta su _id".into())),
        };
        lines.push(format!("PUT _design/{DELETE_DDOC}/_update/delete/{}\n{{}}", seg(&id)));
    }
    if lines.is_empty() {
        return Ok(String::new());
    }
    let ddoc = json!({ "updates": { "delete": DELETE_FN } });
    let head = format!(
        "// Crea la función de borrado (si _design/{DELETE_DDOC} ya existe, borrá estas líneas)\nPUT _design/{DELETE_DDOC}\n{ddoc}"
    );
    Ok(std::iter::once(head).chain(lines).collect::<Vec<_>>().join("\n"))
}

/// A `GET /db/_index` entry as an [`IndexDef`] (`None` for `_all_docs`).
pub fn index_def(ix: &Value) -> Option<IndexDef> {
    let kind = ix.get("type").and_then(Value::as_str)?;
    if kind == "special" {
        return None;
    }
    let def = ix.get("def")?;
    let columns = def
        .get("fields")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|f| match f {
            Value::String(s) => Some(s.clone()),
            Value::Object(m) => m.iter().next().map(|(k, d)| if d == "desc" { format!("{k}:desc") } else { k.clone() }),
            _ => None,
        })
        .collect();
    let filter = def.get("partial_filter_selector").filter(|f| f.as_object().is_some_and(|m| !m.is_empty())).map(Value::to_string);
    Some(IndexDef {
        name: ix.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
        columns,
        unique: false,
        kind: (kind != "json").then(|| kind.to_string()),
        filter,
        ..Default::default()
    })
}

/// The `_all_docs` pseudo-table: sampled fields, `_id` key, Mango indexes
/// and the design documents to recreate.
pub fn all_docs_schema(columns: Vec<dbine_driver::ColumnDef>, indexes: &Value, design_docs: &Value) -> TableSchema {
    let ddocs: Vec<Value> = design_docs
        .get("rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|r| r.get("doc").cloned())
        // Mango indexes live in `language: query` design docs; `_index` recreates them.
        .filter(|d| d.get("language").and_then(Value::as_str) != Some("query"))
        .map(|mut d| {
            if let Some(m) = d.as_object_mut() {
                m.remove("_rev");
            }
            d
        })
        .collect();
    // Validation functions are the database's CHECKs, named by their design document.
    let checks = ddocs
        .iter()
        .filter_map(|d| {
            let f = d.get(VALIDATE).and_then(Value::as_str).filter(|f| !f.trim().is_empty())?;
            Some(CheckDef { name: d.get("_id").and_then(Value::as_str).map(str::to_string), expression: f.to_string() })
        })
        .collect();
    let mut t = TableSchema {
        kind: kinds::COLLECTION.into(),
        name: crate::ALL_DOCS.into(),
        primary_key: Some(KeyDef { name: None, columns: vec!["_id".into()] }),
        indexes: indexes.get("indexes").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().filter_map(index_def).collect(),
        checks,
        columns,
        ..Default::default()
    };
    if !ddocs.is_empty() {
        t.options.insert("design_docs".into(), Value::Array(ddocs).to_string());
    }
    t
}

/// CouchDB's rule for database names: `^[a-z][a-z0-9_$()+/-]*$`, up to
/// 238 characters.
pub fn check_database_name(name: &str) -> Result<()> {
    let ok = name.len() <= 238
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_$()+/-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::Query(format!(
            "nombre de base inválido «{name}»: tiene que empezar con una minúscula y usar solo minúsculas, dígitos y _ $ ( ) + - /"
        )))
    }
}

/// The browse query (a Mango `{"selector": {}, "limit": n}`) restricted by
/// the grid's column filters: the selector gets the Mango operators, text
/// matches as case-insensitive `$regex` (escaped), and a null matches a
/// missing field too. A view (`GET …/_view/…`) can't take a selector, and
/// SQL conditions don't apply.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::FilterOp;
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let Ok(Value::Object(mut body)) = serde_json::from_str::<Value>(browse) else {
        return Err(Error::Unsupported("las vistas de CouchDB no filtran por campo: se filtra en la grilla".into()));
    };
    let escape = |s: &str| {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if "\\.+*?()|[]{}^$".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let mut conds = Vec::new();
    for f in filters {
        let c = f.column.as_str();
        let first = || f.values.first().cloned().ok_or_else(|| Error::Query(format!("el filtro de «{c}» necesita un valor")));
        let re = || first().map(|v| format!("(?i){}", escape(&v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))));
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{c}» necesita al menos un valor")));
            }
            Ok(Value::Array(f.values.clone()))
        };
        let on = |cond: Value| json!({ c: cond });
        let null = json!({ "$or": [on(json!({ "$exists": false })), on(json!({ "$eq": null }))] });
        conds.push(match f.op {
            FilterOp::Eq => on(json!({ "$eq": first()? })),
            FilterOp::Ne => on(json!({ "$ne": first()? })),
            FilterOp::Gt => on(json!({ "$gt": first()? })),
            FilterOp::Ge => on(json!({ "$gte": first()? })),
            FilterOp::Lt => on(json!({ "$lt": first()? })),
            FilterOp::Le => on(json!({ "$lte": first()? })),
            FilterOp::Contains => on(json!({ "$regex": re()? })),
            FilterOp::NotContains => on(json!({ "$not": { "$regex": re()? } })),
            FilterOp::StartsWith => on(json!({ "$regex": re()?.replacen("(?i)", "(?i)^", 1) })),
            FilterOp::EndsWith => on(json!({ "$regex": format!("{}$", re()?) })),
            FilterOp::IsNull => null,
            FilterOp::NotNull => on(json!({ "$exists": true, "$ne": null })),
            FilterOp::IsEmpty => on(json!({ "$eq": "" })),
            FilterOp::NotEmpty => on(json!({ "$exists": true, "$nin": [null, ""] })),
            FilterOp::In => on(json!({ "$in": list()? })),
            FilterOp::NotIn => on(json!({ "$nin": list()? })),
            FilterOp::IsTrue => on(json!({ "$eq": true })),
            FilterOp::IsFalse => on(json!({ "$eq": false })),
            FilterOp::TrueOrNull => json!({ "$or": [on(json!({ "$eq": true })), null] }),
            FilterOp::FalseOrNull => json!({ "$or": [on(json!({ "$eq": false })), null] }),
            FilterOp::Sql | FilterOp::SqlRight => {
                return Err(Error::Unsupported("CouchDB no toma condiciones SQL: se filtran en la grilla".into()))
            }
        });
    }
    let selector = if conds.len() == 1 { conds.remove(0) } else { json!({ "$and": conds }) };
    body.insert("selector".into(), selector);
    serde_json::to_string_pretty(&Value::Object(body)).map_err(|e| Error::Query(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_script, write_reason, Stmt};

    #[test]
    fn filtered_browse_builds_a_selector() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        let got = filtered_browse(
            "{\n  \"selector\": {},\n  \"limit\": 200\n}",
            &[
                f("name", FilterOp::Eq, vec![json!("O'Brien \"Bob\"")]),
                f("age", FilterOp::Gt, vec![json!(18)]),
                f("mail", FilterOp::StartsWith, vec![json!("a.b")]),
                f("gone", FilterOp::IsNull, vec![]),
                f("tag", FilterOp::In, vec![json!("x"), json!(2)]),
            ],
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&got).unwrap(),
            json!({ "limit": 200, "selector": { "$and": [
                { "name": { "$eq": "O'Brien \"Bob\"" } },
                { "age": { "$gt": 18 } },
                { "mail": { "$regex": "(?i)^a\\.b" } },
                { "$or": [{ "gone": { "$exists": false } }, { "gone": { "$eq": null } }] },
                { "tag": { "$in": ["x", 2] } }
            ] } })
        );
        assert!(parse_script(&got).is_ok());
        let view = "GET /db/_design/d/_view/v?limit=5";
        assert!(matches!(filtered_browse(view, &[f("a", FilterOp::IsNull, vec![])]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn indexes_and_design_docs_round_trip() {
        let listed = json!({ "total_rows": 3, "indexes": [
            { "ddoc": null, "name": "_all_docs", "type": "special", "def": { "fields": [{ "_id": "asc" }] } },
            { "ddoc": "_design/x", "name": "by_type_date", "type": "json",
              "def": { "fields": [{ "tipo": "asc" }, { "fecha": "asc" }], "partial_filter_selector": { "activo": true } } },
            { "ddoc": "_design/y", "name": "by_age_desc", "type": "json", "def": { "fields": [{ "age": "desc" }] } },
        ] });
        let ddocs = json!({ "rows": [
            { "id": "_design/app", "doc": { "_id": "_design/app", "_rev": "1-a", "views": { "v": { "map": "function (doc) { emit(doc._id, 1); }" } } } },
            { "id": "_design/x", "doc": { "_id": "_design/x", "_rev": "1-b", "language": "query", "views": {} } },
        ] });
        let t = all_docs_schema(Vec::new(), &listed, &ddocs);
        assert_eq!(t.indexes.len(), 2);
        assert_eq!(t.indexes[0].columns, vec!["tipo", "fecha"]);
        assert_eq!(t.indexes[0].filter.as_deref(), Some("{\"activo\":true}"));
        assert_eq!(t.indexes[1].columns, vec!["age:desc"]);
        let text = table_ddl(&t, DdlParts { drop: true, create: true, indexes: true, ..Default::default() }).unwrap();
        let st = parse_script(&text).unwrap();
        assert_eq!(st.len(), 3, "{text}");
        let Stmt::Http { method, path, body: Some(b) } = &st[0] else { panic!("{:?}", st[0]) };
        assert_eq!((method.as_str(), path.as_str()), ("PUT", "_design/app"));
        assert!(b.get("_rev").is_none() && b.get("views").is_some());
        let Stmt::Http { path, body: Some(b), .. } = &st[1] else { panic!() };
        assert_eq!(path, "_index");
        assert_eq!(b["index"]["fields"], json!(["tipo", "fecha"]));
        assert_eq!(b["index"]["partial_filter_selector"], json!({ "activo": true }));
        let Stmt::Http { body: Some(b), .. } = &st[2] else { panic!() };
        assert_eq!(b["index"]["fields"], json!([{ "age": "desc" }]));
        assert!(st.iter().all(|s| write_reason(s).is_some()));

        // Validation functions are CHECKs; the create writes them.
        let ddocs = json!({ "rows": [
            { "doc": { "_id": "_design/app", "_rev": "1-a", "views": {}, "validate_doc_update": "function(){}" } },
        ] });
        let mut t = all_docs_schema(Vec::new(), &json!({}), &ddocs);
        assert_eq!(t.checks, vec![CheckDef { name: Some("_design/app".into()), expression: "function(){}".into() }]);
        t.checks[0].expression = "function(){ x }".into();
        t.checks.push(CheckDef { name: Some("otra".into()), expression: "function(){ y }".into() });
        let st = parse_script(&table_ddl(&t, DdlParts { create: true, ..Default::default() }).unwrap()).unwrap();
        assert_eq!(st.len(), 2);
        let Stmt::Http { body: Some(b), .. } = &st[0] else { panic!() };
        assert_eq!(b["validate_doc_update"], "function(){ x }");
        let Stmt::Http { path, body: Some(b), .. } = &st[1] else { panic!() };
        assert_eq!((path.as_str(), b["validate_doc_update"].as_str()), ("_design/otra", Some("function(){ y }")));
    }

    #[test]
    fn update_script_uses_update_handler() {
        let target = ObjectRef { kind: "collection".into(), schema: None, name: crate::ALL_DOCS.into() };
        let changes = vec![
            RowChange { key: vec![("_id".into(), json!("a b"))], set: vec![("name".into(), json!("O'Brien \"Bob\"")), ("n".into(), Value::Null)], ..Default::default() },
            RowChange { key: vec![("_id".into(), json!("c"))], set: vec![], ..Default::default() },
        ];
        let text = update_script(&target, &changes).unwrap();
        assert_eq!(
            text,
            format!(
                "// Crea la función de actualización (si _design/dbine_update ya existe, borrá estas líneas)\n\
                 PUT _design/dbine_update\n{}\n\
                 PUT _design/dbine_update/_update/set/a%20b\n{{\"n\":null,\"name\":\"O'Brien \\\"Bob\\\"\"}}",
                json!({ "updates": { "set": UPDATE_FN } })
            )
        );
        let st = parse_script(&text).unwrap();
        assert_eq!(st.len(), 2);
        let Stmt::Http { method, path, body: Some(b) } = &st[1] else { panic!() };
        assert_eq!((method.as_str(), path.as_str()), ("PUT", "_design/dbine_update/_update/set/a%20b"));
        assert_eq!(b, &json!({ "name": "O'Brien \"Bob\"", "n": null }));
        assert!(st.iter().all(|s| write_reason(s).is_some()));
        assert_eq!(update_script(&target, &changes[1..]).unwrap(), "");
    }

    #[test]
    fn delete_script_uses_update_handler() {
        let target = ObjectRef { kind: "collection".into(), schema: None, name: crate::ALL_DOCS.into() };
        let keys = vec![vec![("_id".into(), json!("O'Brien \"Bob\"/1 ?x"))], vec![("_id".into(), json!("c")), ("_rev".into(), json!("1-x"))]];
        let text = delete_script(&target, &keys).unwrap();
        assert_eq!(
            text,
            format!(
                "// Crea la función de borrado (si _design/dbine_delete ya existe, borrá estas líneas)\n\
                 PUT _design/dbine_delete\n{}\n\
                 PUT _design/dbine_delete/_update/delete/O%27Brien%20%22Bob%22%2F1%20%3Fx\n{{}}\n\
                 PUT _design/dbine_delete/_update/delete/c\n{{}}",
                json!({ "updates": { "delete": DELETE_FN } })
            )
        );
        let st = parse_script(&text).unwrap();
        assert_eq!(st.len(), 3);
        let Stmt::Http { method, path, body } = &st[1] else { panic!() };
        assert_eq!((method.as_str(), path.as_str(), body), ("PUT", "_design/dbine_delete/_update/delete/O%27Brien%20%22Bob%22%2F1%20%3Fx", &Some(json!({}))));
        assert!(st.iter().all(|s| write_reason(s).is_some()));
        assert_eq!(delete_script(&target, &[]).unwrap(), "");
        assert!(delete_script(&target, &[vec![("name".into(), json!("x"))]]).is_err());
        assert!(delete_script(&target, &[vec![("_id".into(), json!(""))]]).is_err());
    }

    #[test]
    fn insert_script_batches() {
        let cols = vec!["_id".to_string(), "_rev".into(), "name".into(), "n".into()];
        let rows: Vec<Vec<Value>> =
            (0..1001).map(|i| vec![json!(format!("d{i}")), json!("1-x"), json!(format!("line\nGET /x {i}")), if i % 2 == 0 { Value::Null } else { json!(i) }]).collect();
        let target = ObjectRef { kind: "collection".into(), schema: None, name: crate::ALL_DOCS.into() };
        let st = parse_script(&insert_script(&target, &cols, &rows).unwrap()).unwrap();
        assert_eq!(st.len(), 3);
        let Stmt::Http { method, path, body: Some(b) } = &st[0] else { panic!() };
        assert_eq!((method.as_str(), path.as_str()), ("POST", "_bulk_docs"));
        let docs = b["docs"].as_array().unwrap();
        assert_eq!(docs.len(), 500);
        assert_eq!(docs[0], json!({ "_id": "d0", "name": "line\nGET /x 0" }));
        assert_eq!(docs[1]["n"], json!(1));
        let Stmt::Http { body: Some(b), .. } = &st[2] else { panic!() };
        assert_eq!(b["docs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn templates_parse_as_writes() {
        for t in templates() {
            let st = parse_script(&t.template.replace("{name}", "obj")).unwrap_or_else(|e| panic!("{}: {e}", t.label));
            assert_eq!(st.len(), 1, "{}", t.label);
            assert!(matches!(&st[0], Stmt::Http { body: Some(_), .. }), "{}", t.label);
            assert!(write_reason(&st[0]).is_some());
        }
    }

    #[test]
    fn database_names() {
        for ok in ["a", "ventas_2024", "a/b", "x$(y)+z-1"] {
            assert!(check_database_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Ventas", "_users", "1a", "a b", "a.b"] {
            assert!(check_database_name(bad).is_err(), "{bad}");
        }
    }
}

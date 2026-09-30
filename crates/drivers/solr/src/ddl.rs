//! The collection designer of Solr and the scripts built from it, in the
//! console syntax `execute` runs:
//!
//! - a collection (a core on a standalone server) is created with DBine's
//!   `PUT /solr/<name>` and dropped with `DELETE /solr/<name>` (see
//!   [`crate::QUERY_HELP`]): the driver turns them into a CoreAdmin or a
//!   Collections API call depending on the server's mode, so one script
//!   works on both;
//! - its fields are one `POST /solr/<name>/schema` with an `add-field` list
//!   (Schema API, managed schema as in the `_default` configset);
//! - rows are `POST /solr/<name>/update?commit=true` with a JSON array.
//!
//! The uniqueKey comes from the configset (`id` in `_default`) and can't be
//! changed through the Schema API, so the designer has no primary key and
//! a `string` column named `id` is left out of `add-field`.

use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, KeyDef, ObjectRef, Result, RowChange,
    TableSchema,
};
use dbine_driver_elasticsearch::json::{Obj, J};
use serde_json::Value;
use std::collections::BTreeMap;

/// Documents per `update` request of an insert script.
const UPDATE_BATCH: usize = 500;

/// Fields the `_default` configset already defines (besides `id`).
pub(crate) const BUILTIN_FIELDS: &[&str] = &["_version_", "_root_", "_nest_path_", "_text_"];

/// Field properties the designer offers, as Solr spells them.
const FIELD_FLAGS: &[&str] = &["multiValued", "indexed", "stored", "docValues"];

/// A yes / no option that can also be left to the field type's default.
fn tri_field(key: &'static str, label: &'static str, help: &'static str) -> Field {
    Field::new(key, label, FieldKind::Select(vec![("", "Por defecto"), ("true", "Sí"), ("false", "No")])).help(help)
}

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        kind: kinds::COLLECTION,
        label: "Nueva colección",
        data_types: vec![
            "string",
            "text_general",
            "text_en",
            "text_ws",
            "text_gen_sort",
            "pint",
            "plong",
            "pfloat",
            "pdouble",
            "boolean",
            "pdate",
            "binary",
            "location",
            "location_rpt",
            "rank",
            "strings",
            "pints",
            "plongs",
            "pfloats",
            "pdoubles",
            "booleans",
            "pdates",
        ],
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: true,
        nullability: true,
        comments: false,
        indexes: false,
        foreign_keys: false,
        column_options: vec![
            tri_field("multiValued", "Multivaluado", "Sí: el campo guarda una lista de valores."),
            tri_field("indexed", "Indexado", "No: el campo se guarda pero no se puede buscar."),
            tri_field("stored", "Almacenado", "No: se puede buscar pero no se devuelve en los resultados."),
            tri_field("docValues", "Doc values", "Necesario para ordenar, facetar y agrupar eficientemente."),
        ],
        table_options: vec![
            Field::new("configSet", "Configset", FieldKind::Text).placeholder("_default").help(
                "En SolrCloud, con _default se crea una copia propia de la colección. En modo standalone el configset \
                 se comparte: los campos agregados se escriben en él y los ven los demás cores que lo usan.",
            ),
            Field::new("numShards", "Shards (SolrCloud)", FieldKind::Number).placeholder("1"),
            Field::new("replicationFactor", "Réplicas (SolrCloud)", FieldKind::Number).placeholder("1"),
        ],
        columns_required: false,
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::COLLECTION,
            label: "Nueva colección (script)",
            template: r#"# PUT /solr/<nombre> crea un core (standalone) o una colección (SolrCloud).
PUT /solr/{name}
{ "configSet": "_default", "numShards": 1, "replicationFactor": 1 }

POST /solr/{name}/schema
{
  "add-field": [
    { "name": "title", "type": "text_general", "stored": true },
    { "name": "year", "type": "pint" }
  ]
}"#
            .into(),
        },
        CreateTemplate {
            kind: "field",
            label: "Nuevo campo",
            template: r#"POST /solr/mi_coleccion/schema
{
  "add-field": { "name": "{name}", "type": "string", "indexed": true, "stored": true }
}"#
            .into(),
        },
        CreateTemplate {
            kind: "dynamic_field",
            label: "Nuevo campo dinámico",
            template: r#"POST /solr/mi_coleccion/schema
{
  "add-dynamic-field": { "name": "*_{name}", "type": "string", "indexed": true, "stored": true }
}"#
            .into(),
        },
        CreateTemplate {
            kind: "copy_field",
            label: "Nueva copia de campo (copyField)",
            template: r#"POST /solr/mi_coleccion/schema
{
  "add-copy-field": { "source": "{name}", "dest": "_text_" }
}"#
            .into(),
        },
        CreateTemplate {
            kind: "field_type",
            label: "Nuevo tipo de campo",
            template: r#"POST /solr/mi_coleccion/schema
{
  "add-field-type": {
    "name": "{name}",
    "class": "solr.TextField",
    "positionIncrementGap": "100",
    "analyzer": {
      "tokenizer": { "class": "solr.StandardTokenizerFactory" },
      "filters": [
        { "class": "solr.LowerCaseFilterFactory" },
        { "class": "solr.ASCIIFoldingFilterFactory" }
      ]
    }
  }
}"#
            .into(),
        },
    ]
}

fn opt<'a>(m: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    m.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// `"true"` / `"false"` as booleans, numbers as numbers, the rest as text.
fn scalar(v: &str) -> J {
    match v {
        "true" => J::Bool(true),
        "false" => J::Bool(false),
        _ => serde_json::from_str::<serde_json::Number>(v).map_or_else(|_| J::Str(v.to_string()), J::Num),
    }
}

pub(crate) fn check_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Query("Falta el nombre de la colección.".into()));
    }
    let ok = name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) && !name.starts_with('-');
    if !ok || name == "admin" {
        return Err(Error::Query(format!(
            "El nombre de colección «{name}» no es válido: usá letras, números, _, - y . (sin empezar con -)."
        )));
    }
    Ok(())
}

/// A column as an `add-field` definition.
pub(crate) fn field_def(c: &ColumnDef) -> Result<Obj> {
    let name = c.name.trim();
    let mut ty = c.data_type.trim();
    let mut multi = None;
    if let Some(base) = ty.strip_suffix("[]") {
        ty = base.trim();
        multi = Some(J::Bool(true));
    }
    if ty.is_empty() {
        return Err(Error::Query(format!("Falta el tipo del campo «{name}».")));
    }
    let mut f: Obj = vec![("name".into(), J::Str(name.to_string())), ("type".into(), J::Str(ty.to_string()))];
    for key in FIELD_FLAGS {
        let v = match opt(&c.options, key) {
            Some(v @ ("true" | "false")) => Some(J::Bool(v == "true")),
            Some(v) => return Err(Error::Query(format!("Campo {name}, {key}: «{v}» no es true ni false."))),
            None if *key == "multiValued" => multi.clone(),
            None => None,
        };
        if let Some(v) = v {
            f.push((key.to_string(), v));
        }
    }
    if !c.nullable {
        f.push(("required".into(), J::Bool(true)));
    }
    if let Some(d) = c.default_value.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        f.push(("default".into(), J::Str(d.trim_matches('\'').to_string())));
    }
    Ok(f)
}

/// `DELETE` / `PUT` of a collection plus its `add-field`s, in console
/// syntax.
pub fn collection_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    let name = t.name.trim();
    check_name(name)?;
    let guard = |p: &str| if parts.if_exists { format!("?{p}=true") } else { String::new() };
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DELETE /solr/{name}{}", guard("if_exists")));
    }
    if parts.create {
        let mut s = format!("PUT /solr/{name}{}", guard("if_not_exists"));
        let params: Obj = ["configSet", "numShards", "replicationFactor"]
            .iter()
            .filter_map(|k| opt(&t.options, k).map(|v| (k.to_string(), scalar(v))))
            .collect();
        if !params.is_empty() {
            s.push('\n');
            s.push_str(&J::Obj(params).compact());
        }
        out.push(s);

        let mut add = Vec::new();
        let mut replace = Vec::new();
        let mut skipped = Vec::new();
        for c in &t.columns {
            let n = c.name.trim();
            if n.is_empty() {
                continue;
            }
            let ty = c.data_type.trim();
            if BUILTIN_FIELDS.contains(&n) || (n == "id" && (ty.is_empty() || ty == "string")) {
                skipped.push(n);
            } else if n == "id" {
                replace.push(J::Obj(field_def(c)?));
            } else {
                add.push(J::Obj(field_def(c)?));
            }
        }
        if !add.is_empty() || !replace.is_empty() {
            let mut s = String::new();
            if !skipped.is_empty() {
                s.push_str(&format!("# Ya definidos por el configset: {}.\n", skipped.join(", ")));
            }
            s.push_str(&format!("POST /solr/{name}/schema\n{{\n"));
            let mut cmds = Vec::new();
            for (cmd, list) in [("replace-field", replace), ("add-field", add)] {
                if !list.is_empty() {
                    let items: Vec<String> = list.iter().map(|f| format!("    {}", f.compact())).collect();
                    cmds.push(format!("  \"{cmd}\": [\n{}\n  ]", items.join(",\n")));
                }
            }
            s.push_str(&cmds.join(",\n"));
            s.push_str("\n}");
            out.push(s);
        }
    }
    Ok(out.join("\n\n"))
}

/// Rows as `update` requests with a JSON array (500 documents each), with
/// a commit so they're searchable right away. Nulls, `_version_` (it would
/// turn the add into an optimistic-concurrency check) and `score` are
/// dropped; lists shown as JSON text are lists again.
pub fn update_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    let name = target.name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta la colección de destino.".into()));
    }
    let mut reqs = Vec::new();
    for chunk in rows.chunks(UPDATE_BATCH) {
        let docs: Vec<String> = chunk
            .iter()
            .map(|row| {
                let doc: Obj = columns
                    .iter()
                    .zip(row)
                    .filter(|(c, v)| !v.is_null() && c.as_str() != "_version_" && c.as_str() != "score")
                    .map(|(c, v)| (c.clone(), J::from_cell(v)))
                    .collect();
                J::Obj(doc).compact()
            })
            .collect();
        reqs.push(format!("POST /solr/{name}/update?commit=true\n[\n{}\n]", docs.join(",\n")));
    }
    Ok(reqs.join("\n\n"))
}

/// Edited documents as one `update` request of atomic updates
/// (`{"id": …, "field": {"set": …}}`) with a commit. The key has to be the
/// collection's uniqueKey (a single field); `{"set": null}` removes a field.
pub fn atomic_update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    let name = target.name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta la colección de destino.".into()));
    }
    let mut docs = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let [(k, v)] = ch.key.as_slice() else {
            return Err(Error::Unsupported(
                "las actualizaciones atómicas de Solr necesitan el uniqueKey de la colección para identificar el documento".into(),
            ));
        };
        let mut doc: Obj = vec![(k.clone(), J::from_cell(v))];
        for (c, v) in ch.set.iter().filter(|(c, _)| c != k && c != "_version_" && c != "score") {
            doc.push((c.clone(), J::Obj(vec![("set".into(), J::from_cell(v))])));
        }
        if doc.len() > 1 {
            docs.push(J::Obj(doc).compact());
        }
    }
    if docs.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("POST /solr/{name}/update?commit=true\n[\n{}\n]", docs.join(",\n")))
}

/// Documents as one `update` request `{"delete": [ids…]}` with a commit.
/// As in [`atomic_update_script`], the key has to be the collection's
/// uniqueKey (a single field); ids go as text (the uniqueKey is a string
/// field in practice, and Solr parses them from text anyway).
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let name = target.name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta la colección de destino.".into()));
    }
    let mut ids = Vec::new();
    for key in keys {
        let id = match key.as_slice() {
            [(_, Value::String(s))] if !s.is_empty() => s.clone(),
            [(_, v @ (Value::Number(_) | Value::Bool(_)))] => v.to_string(),
            _ => {
                return Err(Error::Unsupported(
                    "para borrar documentos de Solr hace falta el uniqueKey de la colección para identificar cada uno".into(),
                ))
            }
        };
        ids.push(J::Str(id));
    }
    if ids.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("POST /solr/{name}/update?commit=true\n{}", J::Obj(vec![("delete".into(), J::Arr(ids))]).compact()))
}

fn internal(n: &str) -> bool {
    n.len() > 1 && n.starts_with('_') && n.ends_with('_')
}

/// A collection as the designer's model, from `schema/fields` (explicit
/// properties only: `showDefaults=false`), the uniqueKey and, on SolrCloud,
/// its `CLUSTERSTATUS` entry.
pub fn collection_schema(name: &str, fields: &J, unique_key: Option<&str>, cluster: Option<&J>) -> TableSchema {
    let mut columns = Vec::new();
    for f in fields.get("fields").and_then(J::as_arr).unwrap_or(&[]) {
        let Some(n) = f.get("name").and_then(J::as_str).filter(|n| !internal(n)) else { continue };
        let options: BTreeMap<String, String> =
            FIELD_FLAGS.iter().filter_map(|k| f.get(k).map(|v| (k.to_string(), v.text()))).collect();
        columns.push(ColumnDef {
            name: n.to_string(),
            data_type: f.get("type").map(J::text).unwrap_or_default(),
            nullable: f.get("required").and_then(J::as_bool) != Some(true),
            default_value: f.get("default").map(J::text),
            options,
            ..Default::default()
        });
    }
    let mut options = BTreeMap::new();
    if let Some(c) = cluster {
        if let Some(cfg) = c.get("configName").map(J::text).filter(|c| !c.ends_with(".AUTOCREATED")) {
            options.insert("configSet".into(), cfg);
        }
        if let Some(n) = c.get("shards").and_then(J::as_obj).map(Vec::len) {
            options.insert("numShards".into(), n.to_string());
        }
        if let Some(r) = c.get("replicationFactor") {
            options.insert("replicationFactor".into(), r.text());
        }
    }
    TableSchema {
        kind: kinds::COLLECTION.into(),
        schema: None,
        name: name.to_string(),
        primary_key: unique_key.map(|k| KeyDef { name: None, columns: vec![k.to_string()] }),
        columns,
        options,
        ..Default::default()
    }
}

/// The browse request (`GET /solr/c/select?q=*:*&rows=n`) restricted by
/// the grid's column filters: one `fq` per filter, in Lucene syntax with
/// the values escaped, then URL-encoded. Text equality is a phrase, text
/// matches are wildcards (case-sensitive on string fields), null is a
/// field without value. SQL conditions don't apply.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::FilterOp;
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    // A term with Lucene's special characters (and spaces) escaped.
    let term = |s: &str| {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if "+-&|!(){}[]^\"~*?:\\/ ".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let phrase = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let value = |v: &Value| match v {
        Value::String(s) => phrase(s),
        Value::Null => "\"\"".into(),
        other => other.to_string(),
    };
    let mut fqs = Vec::new();
    for f in filters {
        let c = term(&f.column);
        let first = || f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let text = || first().map(|v| term(&v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())));
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
            }
            Ok(format!("{c}:({})", f.values.iter().map(value).collect::<Vec<_>>().join(" OR ")))
        };
        let missing = format!("(*:* -{c}:[* TO *])");
        fqs.push(match f.op {
            FilterOp::Eq => format!("{c}:{}", value(first()?)),
            FilterOp::Ne => format!("-{c}:{}", value(first()?)),
            FilterOp::Gt => format!("{c}:{{{} TO *]", value(first()?)),
            FilterOp::Ge => format!("{c}:[{} TO *]", value(first()?)),
            FilterOp::Lt => format!("{c}:[* TO {}}}", value(first()?)),
            FilterOp::Le => format!("{c}:[* TO {}]", value(first()?)),
            FilterOp::Contains => format!("{c}:*{}*", text()?),
            FilterOp::NotContains => format!("-{c}:*{}*", text()?),
            FilterOp::StartsWith => format!("{c}:{}*", text()?),
            FilterOp::EndsWith => format!("{c}:*{}", text()?),
            FilterOp::IsNull => format!("-{c}:[* TO *]"),
            FilterOp::NotNull => format!("{c}:[* TO *]"),
            FilterOp::IsEmpty => format!("{c}:\"\""),
            FilterOp::NotEmpty => format!("+{c}:[* TO *] -{c}:\"\""),
            FilterOp::In => list()?,
            FilterOp::NotIn => format!("-{}", list()?),
            FilterOp::IsTrue => format!("{c}:true"),
            FilterOp::IsFalse => format!("{c}:false"),
            FilterOp::TrueOrNull => format!("({c}:true OR {missing})"),
            FilterOp::FalseOrNull => format!("({c}:false OR {missing})"),
            FilterOp::Sql | FilterOp::SqlRight => {
                return Err(Error::Unsupported("la búsqueda de Solr no toma condiciones SQL: se filtran en la grilla".into()))
            }
        });
    }
    let encode = |s: &str| {
        let mut out = String::with_capacity(s.len() * 3);
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"-_.~:*".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    };
    let (line, rest) = browse.split_once('\n').map_or((browse, None), |(l, r)| (l, Some(r)));
    let sep = if line.contains('?') { '&' } else { '?' };
    let params: Vec<String> = fqs.iter().map(|q| format!("fq={}", encode(q))).collect();
    let line = format!("{}{sep}{}", line.trim_end(), params.join("&"));
    Ok(match rest {
        Some(r) => format!("{line}\n{r}"),
        None => line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver_elasticsearch::console::{self, Command, Request};
    use serde_json::json;

    #[test]
    fn filtered_browse_adds_filter_queries() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        let got = filtered_browse(
            "GET /solr/books/select?q=*:*&rows=200",
            &[
                f("title", FilterOp::Eq, vec![json!("O'Brien \"Bob\" & co")]),
                f("price", FilterOp::Ge, vec![json!(10)]),
                f("name", FilterOp::StartsWith, vec![json!("a b")]),
                f("isbn", FilterOp::IsNull, vec![]),
                f("cat", FilterOp::In, vec![json!("x"), json!(2)]),
            ],
        )
        .unwrap();
        assert_eq!(
            got,
            "GET /solr/books/select?q=*:*&rows=200&fq=title:%22O%27Brien%20%5C%22Bob%5C%22%20%26%20co%22&fq=price:%5B10%20TO%20*%5D&fq=name:a%5C%20b*&fq=-isbn:%5B*%20TO%20*%5D&fq=cat:%28%22x%22%20OR%202%29"
        );
        assert!(matches!(console::parse(&got).unwrap()[0], Command::Http(_)));
        assert!(matches!(
            filtered_browse("GET /solr/c/select?q=*:*", &[ColumnFilter { column: "a".into(), op: FilterOp::SqlRight, values: vec![], sql: Some("1".into()) }]),
            Err(Error::Unsupported(_))
        ));
    }

    fn col(name: &str, ty: &str, opts: &[(&str, &str)]) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
            options: opts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    fn reqs(text: &str) -> Vec<Request> {
        console::parse(text)
            .unwrap()
            .into_iter()
            .map(|c| match c {
                Command::Http(r) => r,
                Command::Sql(s) => panic!("unexpected SQL {s}"),
            })
            .collect()
    }

    fn books() -> TableSchema {
        let mut year = col("year", "pint", &[("docValues", "true")]);
        year.nullable = false;
        year.default_value = Some("2000".into());
        TableSchema {
            kind: kinds::COLLECTION.into(),
            name: "books".into(),
            columns: vec![
                col("id", "string", &[]),
                col("_version_", "plong", &[]),
                col("title", "text_general", &[("stored", "true"), ("indexed", "")]),
                year,
                col("tags", "string[]", &[]),
            ],
            options: [("configSet", "_default"), ("numShards", "2")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn ddl_parses() {
        let text = collection_ddl(&books(), DdlParts { drop: true, if_exists: true, create: true, ..Default::default() }).unwrap();
        let r = reqs(&text);
        assert_eq!(r.len(), 3, "{text}");
        assert_eq!((r[0].method.as_str(), r[0].path.as_str()), ("DELETE", "/solr/books?if_exists=true"));
        assert_eq!((r[1].method.as_str(), r[1].path.as_str()), ("PUT", "/solr/books?if_not_exists=true"));
        let p: serde_json::Value = serde_json::from_str(r[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(p, json!({"configSet": "_default", "numShards": 2}));
        assert_eq!(r[2].path, "/solr/books/schema");
        let b: serde_json::Value = serde_json::from_str(r[2].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            b["add-field"],
            json!([
                {"name": "title", "type": "text_general", "stored": true},
                {"name": "year", "type": "pint", "docValues": true, "required": true, "default": "2000"},
                {"name": "tags", "type": "string", "multiValued": true}
            ])
        );
        assert!(b.get("replace-field").is_none());
        assert!(text.contains("# Ya definidos por el configset: id, _version_."));

        let mut t = books();
        t.columns[0].data_type = "plong".into();
        let text = collection_ddl(&t, DdlParts { create: true, ..Default::default() }).unwrap();
        let r = reqs(&text);
        assert_eq!(r[0].path, "/solr/books");
        let b: serde_json::Value = serde_json::from_str(r[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(b["replace-field"], json!([{"name": "id", "type": "plong"}]));

        let bare = TableSchema { name: "b".into(), ..Default::default() };
        assert_eq!(collection_ddl(&bare, DdlParts { create: true, ..Default::default() }).unwrap(), "PUT /solr/b");
        assert!(collection_ddl(&TableSchema { name: "a b".into(), ..Default::default() }, DdlParts::default()).is_err());
        let mut t = books();
        t.columns[2].data_type = String::new();
        assert!(collection_ddl(&t, DdlParts { create: true, ..Default::default() }).is_err());
    }

    #[test]
    fn schema_round_trips() {
        let fields = J::parse(
            r#"{"fields":[{"name":"_version_","type":"plong"},{"name":"id","type":"string","required":true,"stored":true},
                {"name":"title","type":"text_general","stored":true},{"name":"tags","type":"strings","multiValued":true,"default":"x"}]}"#,
        )
        .unwrap();
        let cluster = J::parse(r#"{"configName":"books.AUTOCREATED","replicationFactor":1,"shards":{"shard1":{},"shard2":{}}}"#).unwrap();
        let t = collection_schema("books", &fields, Some("id"), Some(&cluster));
        assert_eq!(t.primary_key.as_ref().unwrap().columns, ["id"]);
        let names: Vec<_> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "title", "tags"]);
        assert!(!t.columns[0].nullable);
        assert_eq!(t.columns[2].options.get("multiValued").map(String::as_str), Some("true"));
        assert_eq!(t.columns[2].default_value.as_deref(), Some("x"));
        assert_eq!(t.options.get("numShards").map(String::as_str), Some("2"));
        assert!(!t.options.contains_key("configSet"));
        let text = collection_ddl(&t, DdlParts { create: true, ..Default::default() }).unwrap();
        let b: serde_json::Value = serde_json::from_str(reqs(&text)[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(b["add-field"][1], json!({"name": "tags", "type": "strings", "multiValued": true, "default": "x"}));
    }

    #[test]
    fn update_script_parses() {
        let target = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "books".into() };
        let cols: Vec<String> = ["id", "title", "tags", "_version_", "score"].iter().map(|s| s.to_string()).collect();
        let rows: Vec<Vec<Value>> =
            (0..501).map(|i| vec![json!(i.to_string()), json!("a\nb"), json!(r#"["x","y"]"#), json!(1), Value::Null]).collect();
        let text = update_script(&target, &cols, &rows).unwrap();
        let r = reqs(&text);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].path, "/solr/books/update?commit=true");
        let docs: serde_json::Value = serde_json::from_str(r[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(docs.as_array().unwrap().len(), 500);
        assert_eq!(docs[0], json!({"id": "0", "title": "a\nb", "tags": ["x", "y"]}));
        assert_eq!(update_script(&target, &cols, &[]).unwrap(), "");
    }

    #[test]
    fn atomic_update_script_sets_fields() {
        let target = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "books".into() };
        let changes = vec![
            RowChange {
                key: vec![("id".into(), json!("1"))],
                set: vec![("title".into(), json!("O'Brien \"Bob\"")), ("year".into(), Value::Null), ("tags".into(), json!(r#"["x"]"#))], ..Default::default()
            },
            RowChange { key: vec![("id".into(), json!("2"))], set: vec![], ..Default::default() },
        ];
        let text = atomic_update_script(&target, &changes).unwrap();
        assert_eq!(
            text,
            "POST /solr/books/update?commit=true\n[\n{\"id\":\"1\",\"title\":{\"set\":\"O'Brien \\\"Bob\\\"\"},\"year\":{\"set\":null},\"tags\":{\"set\":[\"x\"]}}\n]"
        );
        let r = reqs(&text);
        assert_eq!(r.len(), 1);
        serde_json::from_str::<serde_json::Value>(r[0].body.as_deref().unwrap()).unwrap();
        let keyless = vec![RowChange { key: vec![("a".into(), json!(1)), ("b".into(), json!(2))], set: vec![("a".into(), json!(3))], ..Default::default() }];
        assert!(atomic_update_script(&target, &keyless).is_err());
    }

    #[test]
    fn delete_script_by_unique_key() {
        let target = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "books".into() };
        let keys = vec![vec![("id".into(), json!("O'Brien \"Bob\""))], vec![("id".into(), json!(7))]];
        let text = delete_script(&target, &keys).unwrap();
        assert_eq!(text, "POST /solr/books/update?commit=true\n{\"delete\":[\"O'Brien \\\"Bob\\\"\",\"7\"]}");
        let r = reqs(&text);
        assert_eq!(r.len(), 1);
        let b: serde_json::Value = serde_json::from_str(r[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(b, json!({"delete": ["O'Brien \"Bob\"", "7"]}));
        assert_eq!(delete_script(&target, &[]).unwrap(), "");
        assert!(delete_script(&target, &[vec![("a".into(), json!(1)), ("b".into(), json!(2))]]).is_err());
        assert!(delete_script(&target, &[vec![]]).is_err());
        assert!(delete_script(&target, &[vec![("id".into(), Value::Null)]]).is_err());
    }

    #[test]
    fn templates_parse() {
        for t in templates() {
            let text = t.template.replace("{name}", "x");
            for r in reqs(&text) {
                if let Some(b) = &r.body {
                    serde_json::from_str::<serde_json::Value>(b).unwrap_or_else(|e| panic!("{}: {e}", t.label));
                }
            }
        }
    }
}

//! The index designer of Elasticsearch / OpenSearch and the scripts built
//! from it, all in console syntax so `execute` runs them as they are:
//!
//! - an index is `PUT /<index>` with `settings`, `mappings` and `aliases`
//!   (dropping it, `DELETE /<index>`, with `?ignore_unavailable=true` when
//!   guarded);
//! - rows are `POST /_bulk` requests with NDJSON bodies;
//! - aliases, index templates, ingest pipelines, data streams and lifecycle
//!   policies (ILM on Elasticsearch, ISM on OpenSearch) get starting
//!   scripts.
//!
//! Dotted column names are object paths: `author.name` becomes
//! `"author": {"properties": {"name": …}}`. A column named like the parent
//! (`author`, of type `object` or `nested`) gives the parent's own mapping;
//! without it the parent is a plain object. Reading a mapping back
//! ([`index_schema`]) lists parents (type `object` when the mapping leaves
//! it out) and sub-fields the same way, so both directions round-trip.
//! Mapping parameters without a designer field of their own (multi-fields,
//! `similarity`, `copy_to`…) travel in the column's `extra` JSON.

use crate::json::{Obj, J};
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, ObjectRef, Result, RowChange,
    TableSchema,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Documents per `_bulk` request of an insert script.
const BULK_BATCH: usize = 500;

/// Document metadata that isn't a mapping field.
pub(crate) const META_FIELDS: &[&str] = &["_id", "_index", "_score", "_routing", "_source"];

/// A yes / no option that can also be left to the engine's default.
fn tri_field(key: &'static str, label: &'static str, help: &'static str) -> Field {
    Field::new(key, label, FieldKind::Select(vec![("", "Por defecto"), ("true", "Sí"), ("false", "No")])).help(help)
}

pub fn designer(opensearch: bool) -> DesignerSpec {
    let mut data_types = vec![
        "text",
        "keyword",
        "match_only_text",
        "wildcard",
        "search_as_you_type",
        "completion",
        "long",
        "integer",
        "short",
        "byte",
        "double",
        "float",
        "half_float",
        "scaled_float",
        "unsigned_long",
        "boolean",
        "date",
        "date_nanos",
        "object",
        "nested",
        "geo_point",
        "geo_shape",
        "ip",
        "binary",
        "integer_range",
        "long_range",
        "float_range",
        "double_range",
        "date_range",
        "ip_range",
        "token_count",
        "rank_feature",
        "percolator",
    ];
    if opensearch {
        data_types.extend(["flat_object", "knn_vector"]);
    } else {
        data_types.extend(["flattened", "constant_keyword", "dense_vector", "sparse_vector", "version", "histogram"]);
    }
    let dims_help = if opensearch {
        "`dimension` de un knn_vector (obligatorio)."
    } else {
        "`dims` de un dense_vector (si se omite, la toma del primer documento)."
    };
    let mut dynamic = vec![
        ("", "Por defecto (true)"),
        ("true", "true: agrega los campos nuevos al mapping"),
        ("false", "false: los guarda sin indexarlos"),
        ("strict", "strict: rechaza documentos con campos nuevos"),
    ];
    if !opensearch {
        dynamic.push(("runtime", "runtime: los agrega como campos runtime"));
    }
    let mut table_options = vec![
        Field::new("number_of_shards", "Shards primarios", FieldKind::Number).placeholder("1"),
        Field::new("number_of_replicas", "Réplicas", FieldKind::Number).placeholder("1"),
        Field::new("refresh_interval", "Intervalo de refresh", FieldKind::Text)
            .placeholder("1s")
            .help("Cada cuánto se vuelven visibles los documentos nuevos; -1 lo desactiva."),
        Field::new("aliases", "Alias", FieldKind::Text).help("Separados por coma."),
        Field::new("dynamic", "Campos nuevos (dynamic)", FieldKind::Select(dynamic)),
    ];
    if opensearch {
        table_options.push(tri_field(
            "knn",
            "Búsqueda k-NN (index.knn)",
            "Necesario para las búsquedas aproximadas sobre campos knn_vector.",
        ));
    }
    DesignerSpec {
        kind: kinds::INDEX,
        label: "Nuevo índice",
        data_types,
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: true,
        indexes: false,
        foreign_keys: false,
        column_options: vec![
            Field::new("analyzer", "Analizador", FieldKind::Text)
                .placeholder("standard")
                .help("Solo para campos text: standard, english, whitespace…"),
            tri_field("index", "Indexado", "No: el campo se guarda pero no se puede buscar."),
            tri_field("doc_values", "Doc values", "No: ahorra disco en campos que no se ordenan ni se agregan."),
            Field::new("format", "Formato (fechas)", FieldKind::Text).placeholder("strict_date_optional_time||epoch_millis"),
            Field::new("dims", "Dimensiones (vectores)", FieldKind::Number).help(dims_help),
            Field::new("scaling_factor", "Factor de escala", FieldKind::Number)
                .help("Obligatorio para scaled_float (p. ej. 100)."),
            Field::new("extra", "Mapping extra (JSON)", FieldKind::Textarea).help(
                "Parámetros que se suman al mapping del campo, p. ej. {\"fields\": {\"raw\": {\"type\": \"keyword\"}}}.",
            ),
        ],
        table_options,
        columns_required: false,
    }
}

pub fn templates(opensearch: bool) -> Vec<CreateTemplate> {
    let mut t = vec![
        CreateTemplate {
            kind: crate::KIND_ALIAS,
            label: "Nuevo alias",
            template: r#"POST /_aliases
{
  "actions": [
    { "add": { "index": "mi_indice", "alias": "{name}" } }
  ]
}"#
            .into(),
        },
        CreateTemplate {
            kind: "index_template",
            label: "Nueva plantilla de índice",
            template: r#"PUT /_index_template/{name}
{
  "index_patterns": ["{name}-*"],
  "priority": 100,
  "template": {
    "settings": { "number_of_shards": 1 },
    "mappings": {
      "properties": {
        "@timestamp": { "type": "date" },
        "message": { "type": "text" }
      }
    }
  }
}"#
            .into(),
        },
        CreateTemplate {
            kind: "pipeline",
            label: "Nuevo pipeline de ingesta",
            template: r#"PUT /_ingest/pipeline/{name}
{
  "description": "Pipeline {name}",
  "processors": [
    { "set": { "field": "ingested_at", "value": "{{_ingest.timestamp}}" } },
    { "lowercase": { "field": "tag", "ignore_missing": true } }
  ]
}"#
            .into(),
        },
        CreateTemplate {
            kind: kinds::STREAM,
            label: "Nuevo data stream",
            template: r#"# Un data stream necesita una plantilla de índice con "data_stream" que coincida con su nombre.
PUT /_index_template/{name}-template
{
  "index_patterns": ["{name}*"],
  "data_stream": {},
  "priority": 200,
  "template": {
    "mappings": {
      "properties": {
        "@timestamp": { "type": "date" },
        "message": { "type": "text" }
      }
    }
  }
}

PUT /_data_stream/{name}"#
                .into(),
        },
    ];
    t.push(if opensearch {
        CreateTemplate {
            kind: "policy",
            label: "Nueva política ISM",
            template: r#"PUT /_plugins/_ism/policies/{name}
{
  "policy": {
    "description": "Política {name}",
    "default_state": "hot",
    "states": [
      {
        "name": "hot",
        "actions": [],
        "transitions": [{ "state_name": "delete", "conditions": { "min_index_age": "90d" } }]
      },
      {
        "name": "delete",
        "actions": [{ "delete": {} }],
        "transitions": []
      }
    ],
    "ism_template": [{ "index_patterns": ["{name}-*"], "priority": 100 }]
  }
}"#
            .into(),
        }
    } else {
        CreateTemplate {
            kind: "policy",
            label: "Nueva política ILM",
            template: r#"PUT /_ilm/policy/{name}
{
  "policy": {
    "phases": {
      "hot": {
        "actions": { "rollover": { "max_age": "30d", "max_primary_shard_size": "50gb" } }
      },
      "delete": {
        "min_age": "90d",
        "actions": { "delete": {} }
      }
    }
  }
}"#
            .into(),
        }
    });
    t
}

pub(crate) fn opt<'a>(m: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    m.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn bool_opt(m: &BTreeMap<String, String>, key: &str, what: &str) -> Result<Option<J>> {
    match opt(m, key) {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(Some(J::Bool(true))),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(Some(J::Bool(false))),
        Some(v) => Err(Error::Query(format!("{what}: «{v}» no es true ni false."))),
    }
}

fn num_opt(m: &BTreeMap<String, String>, key: &str, what: &str) -> Result<Option<J>> {
    match opt(m, key) {
        None => Ok(None),
        Some(v) => serde_json::from_str::<serde_json::Number>(v)
            .map(|n| Some(J::Num(n)))
            .map_err(|_| Error::Query(format!("{what}: «{v}» no es un número."))),
    }
}

fn set(o: &mut Obj, key: &str, v: J) {
    match o.iter_mut().find(|(k, _)| k == key) {
        Some(e) => e.1 = v,
        None => o.push((key.to_string(), v)),
    }
}

/// Characters Elasticsearch rejects in index names.
const BAD_NAME_CHARS: &[char] = &['\\', '/', '*', '?', '"', '<', '>', '|', ' ', ',', '#', ':'];

pub(crate) fn check_index_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Query("Falta el nombre del índice.".into()));
    }
    if name.chars().any(|c| c.is_uppercase()) {
        return Err(Error::Query(format!("El nombre del índice «{name}» debe ir en minúsculas.")));
    }
    if name.starts_with(['-', '_', '+']) || name.contains(BAD_NAME_CHARS) {
        return Err(Error::Query(format!(
            "El nombre del índice «{name}» no es válido: no puede empezar con -, _ o + ni tener espacios, comas ni \\ / * ? \" < > | # :"
        )));
    }
    Ok(())
}

/// A column's own mapping (without its sub-fields).
pub(crate) fn field_mapping(c: &ColumnDef, opensearch: bool) -> Result<Obj> {
    let what = |f: &str| format!("Campo {}, {f}", c.name);
    let mut m: Obj = Vec::new();
    let ty = c.data_type.trim();
    if !ty.is_empty() {
        m.push(("type".into(), J::Str(ty.to_string())));
    }
    if let Some(a) = opt(&c.options, "analyzer") {
        m.push(("analyzer".into(), J::Str(a.to_string())));
    }
    for key in ["index", "doc_values"] {
        if let Some(b) = bool_opt(&c.options, key, &what(key))? {
            m.push((key.into(), b));
        }
    }
    if let Some(f) = opt(&c.options, "format") {
        m.push(("format".into(), J::Str(f.to_string())));
    }
    if let Some(n) = num_opt(&c.options, "dims", &what("dimensiones"))? {
        m.push((if opensearch { "dimension" } else { "dims" }.into(), n));
    }
    if let Some(n) = num_opt(&c.options, "scaling_factor", &what("factor de escala"))? {
        m.push(("scaling_factor".into(), n));
    }
    if let Some(d) = c.comment.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        m.push(("meta".into(), J::Obj(vec![("description".into(), J::Str(d.to_string()))])));
    }
    if let Some(x) = opt(&c.options, "extra") {
        match J::parse(x) {
            Ok(J::Obj(extra)) => {
                for (k, v) in extra {
                    set(&mut m, &k, v);
                }
            }
            _ => return Err(Error::Query(format!("Campo {}: el mapping extra debe ser un objeto JSON.", c.name))),
        }
    }
    Ok(m)
}

fn has_children(ty: Option<&J>) -> bool {
    matches!(ty.and_then(J::as_str), None | Some("object" | "nested"))
}

/// Put a field's mapping at `path` below `props`, creating (or reusing)
/// the parent objects.
pub(crate) fn put_field(props: &mut Obj, path: &[&str], leaf: Obj, full: &str) -> Result<()> {
    let (head, rest) = path.split_first().expect("non-empty path");
    if head.is_empty() {
        return Err(Error::Query(format!("El nombre de campo «{full}» tiene un tramo vacío.")));
    }
    if !props.iter().any(|(k, _)| k == head) {
        props.push((head.to_string(), J::Obj(Vec::new())));
    }
    let Some((_, J::Obj(entry))) = props.iter_mut().find(|(k, _)| k == head) else { unreachable!() };
    if rest.is_empty() {
        let children = entry.iter().any(|(k, _)| k == "properties");
        let ty = leaf.iter().find(|(k, _)| k == "type").map(|(_, v)| v);
        if children && !has_children(ty) {
            return Err(Error::Query(format!(
                "El campo «{full}» tiene subcampos: su tipo tiene que ser object o nested, no {}.",
                ty.map(J::text).unwrap_or_default()
            )));
        }
        if !entry.is_empty() && entry.iter().any(|(k, _)| k != "properties") {
            return Err(Error::Query(format!("El campo «{full}» está repetido.")));
        }
        let kids = std::mem::take(entry);
        *entry = leaf;
        entry.extend(kids);
        return Ok(());
    }
    let ty = entry.iter().find(|(k, _)| k == "type").map(|(_, v)| v);
    if !has_children(ty) {
        let parent = full.split('.').take(path.len() - rest.len()).collect::<Vec<_>>().join(".");
        return Err(Error::Query(format!(
            "«{full}» no puede estar dentro de «{parent}»: solo los campos object o nested tienen subcampos."
        )));
    }
    if !entry.iter().any(|(k, _)| k == "properties") {
        entry.push(("properties".into(), J::Obj(Vec::new())));
    }
    let Some((_, J::Obj(sub))) = entry.iter_mut().find(|(k, _)| k == "properties") else {
        return Err(Error::Query(format!("El mapping de «{full}» no es válido.")));
    };
    put_field(sub, rest, leaf, full)
}

/// The body of `PUT /<index>`: settings, mappings and aliases (the keys
/// with nothing to say are left out).
pub fn index_body(t: &TableSchema, opensearch: bool) -> Result<Obj> {
    let o = &t.options;
    let mut settings: Obj = Vec::new();
    for (key, what) in [("number_of_shards", "Shards primarios"), ("number_of_replicas", "Réplicas")] {
        if let Some(n) = num_opt(o, key, what)? {
            settings.push((key.into(), n));
        }
    }
    if let Some(r) = opt(o, "refresh_interval") {
        settings.push(("refresh_interval".into(), J::Str(r.to_string())));
    }
    if opensearch {
        if let Some(b) = bool_opt(o, "knn", "k-NN")? {
            settings.push(("knn".into(), b));
        }
    }
    if let Some(a) = analysis_opt(o)? {
        settings.push(("analysis".into(), a));
    }
    // The other settings, as the index has them (see `index_schema`).
    for key in [SETTINGS_EXTRA, LIFECYCLE] {
        for (k, v) in json_obj_opt(o, key)?.unwrap_or_default() {
            set(&mut settings, &k, v);
        }
    }

    let mut mappings: Obj = Vec::new();
    match opt(o, "dynamic") {
        None => {}
        Some(d @ ("true" | "false")) => mappings.push(("dynamic".into(), J::Bool(d == "true"))),
        Some(d) => mappings.push(("dynamic".into(), J::Str(d.to_string()))),
    }
    // The mapping's other parameters (`dynamic_templates`, `_routing`,
    // `runtime`, `_source`…), `_meta` with the comment as its description.
    let mapping_extra = json_obj_opt(o, MAPPINGS_EXTRA)?.unwrap_or_default();
    let mut meta: Obj = mapping_extra.iter().find(|(k, _)| k == "_meta").and_then(|(_, v)| v.as_obj().cloned()).unwrap_or_default();
    meta.retain(|(k, _)| k != "description");
    if let Some(c) = t.comment.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        meta.insert(0, ("description".into(), J::Str(c.to_string())));
    }
    if !meta.is_empty() {
        mappings.push(("_meta".into(), J::Obj(meta)));
    }
    for (k, v) in mapping_extra.into_iter().filter(|(k, _)| k != "_meta") {
        set(&mut mappings, &k, v);
    }
    let mut props: Obj = Vec::new();
    for c in &t.columns {
        let name = c.name.trim();
        if name.is_empty() || META_FIELDS.contains(&name) {
            continue;
        }
        let path: Vec<&str> = name.split('.').collect();
        put_field(&mut props, &path, field_mapping(c, opensearch)?, name)?;
    }
    if !props.is_empty() {
        mappings.push(("properties".into(), J::Obj(props)));
    }

    let aliases: Obj = opt(o, "aliases")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(|a| (a.to_string(), J::Obj(Vec::new())))
        .collect();

    let mut body = Vec::new();
    for (key, v) in [("settings", settings), ("mappings", mappings), ("aliases", aliases)] {
        if !v.is_empty() {
            body.push((key.to_string(), J::Obj(v)));
        }
    }
    Ok(body)
}

/// `DELETE` and / or `PUT` of an index, in console syntax.
pub fn index_ddl(t: &TableSchema, parts: DdlParts, opensearch: bool) -> Result<String> {
    let name = t.name.trim();
    check_index_name(name)?;
    let mut out = Vec::new();
    if parts.drop {
        out.push(if parts.if_exists { format!("DELETE /{name}?ignore_unavailable=true") } else { format!("DELETE /{name}") });
    }
    if parts.create {
        let body = index_body(t, opensearch)?;
        let mut s = String::new();
        if parts.if_exists && !parts.drop {
            s.push_str("# No hay «si no existe» para índices: si ya existe, la petición falla.\n");
        }
        s.push_str(&format!("PUT /{name}"));
        if !body.is_empty() {
            s.push('\n');
            s.push_str(&J::Obj(body).pretty());
        }
        out.push(s);
    }
    Ok(out.join("\n\n"))
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Rows as `POST /_bulk` requests (500 documents each). `_id` and
/// `_routing` columns go to the action line; `_index` and `_score` (from
/// search results) are dropped, and so are null values. Data streams only
/// take `create` actions.
pub fn bulk_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    let name = target.name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta el índice de destino.".into()));
    }
    let op = if target.kind == kinds::STREAM { "create" } else { "index" };
    let mut reqs = Vec::new();
    for chunk in rows.chunks(BULK_BATCH) {
        let mut s = String::from("POST /_bulk?refresh=true");
        for row in chunk {
            let mut action: Obj = vec![("_index".into(), J::Str(name.to_string()))];
            let mut doc: Obj = Vec::new();
            for (c, v) in columns.iter().zip(row) {
                match c.as_str() {
                    _ if v.is_null() => {}
                    "_id" => action.push(("_id".into(), J::Str(scalar_text(v)))),
                    "_routing" => action.push(("routing".into(), J::Str(scalar_text(v)))),
                    "_index" | "_score" => {}
                    _ => doc.push((c.clone(), J::from_cell(v))),
                }
            }
            s.push('\n');
            s.push_str(&J::Obj(vec![(op.into(), J::Obj(action))]).compact());
            s.push('\n');
            s.push_str(&J::Obj(doc).compact());
        }
        reqs.push(s);
    }
    Ok(reqs.join("\n\n"))
}

/// Painless script of a data stream update: copies `params.doc` over the
/// document's fields.
const STREAM_UPDATE: &str = "for (e in params.doc.entrySet()) { ctx._source[e.getKey()] = e.getValue() }";

/// A URL path segment: everything but unreserved characters percent-encoded.
pub(crate) fn path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Edited documents as `POST /<index>/_update/<id>` requests with a
/// partial `doc` (null values are set to null). `_routing` in the key goes
/// to the query string; the document is found by `_id`, which search
/// results always carry. Data streams refuse `_update`, so there it's an
/// `_update_by_query` on that `_id` whose script copies the fields.
pub fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    let name = target.name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta el índice de destino.".into()));
    }
    let mut reqs = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let key = |k: &str| ch.key.iter().find(|(c, v)| c == k && !v.is_null()).map(|(_, v)| scalar_text(v));
        let Some(id) = key("_id") else {
            return Err(Error::Unsupported("para actualizar un documento hace falta su _id".into()));
        };
        let doc: Obj = ch
            .set
            .iter()
            .filter(|(c, _)| !META_FIELDS.contains(&c.as_str()))
            .map(|(c, v)| (c.clone(), J::from_cell(v)))
            .collect();
        if doc.is_empty() {
            continue;
        }
        let routing = key("_routing").map(|r| format!("&routing={}", path_segment(&r))).unwrap_or_default();
        if target.kind == kinds::STREAM {
            let body = J::Obj(vec![
                ("query".into(), J::Obj(vec![("ids".into(), J::Obj(vec![("values".into(), J::Arr(vec![J::Str(id)]))]))])),
                (
                    "script".into(),
                    J::Obj(vec![
                        ("source".into(), J::Str(STREAM_UPDATE.into())),
                        ("params".into(), J::Obj(vec![("doc".into(), J::Obj(doc))])),
                    ]),
                ),
            ]);
            reqs.push(format!("POST /{}/_update_by_query?refresh=true{routing}\n{}", path_segment(name), body.compact()));
        } else {
            let path = format!("POST /{}/_update/{}?refresh=true{routing}", path_segment(name), path_segment(&id));
            reqs.push(format!("{path}\n{}", J::Obj(vec![("doc".into(), J::Obj(doc))]).compact()));
        }
    }
    Ok(reqs.join("\n\n"))
}

/// Documents as `DELETE /<index>/_doc/<id>` requests, addressed like
/// [`update_script`] (by `_id`, `_routing` in the query string). Data
/// streams refuse deletes through the stream name, so there it's a
/// `_delete_by_query` on that `_id`.
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let name = target.name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta el índice de destino.".into()));
    }
    let mut reqs = Vec::new();
    for k in keys {
        let key = |f: &str| k.iter().find(|(c, v)| c == f && !v.is_null()).map(|(_, v)| scalar_text(v)).filter(|s| !s.is_empty());
        let Some(id) = key("_id") else {
            return Err(Error::Unsupported("para borrar un documento hace falta su _id".into()));
        };
        let routing = key("_routing").map(|r| format!("&routing={}", path_segment(&r))).unwrap_or_default();
        if target.kind == kinds::STREAM {
            let body = J::Obj(vec![("query".into(), J::Obj(vec![("ids".into(), J::Obj(vec![("values".into(), J::Arr(vec![J::Str(id)]))]))]))]);
            reqs.push(format!("POST /{}/_delete_by_query?refresh=true{routing}\n{}", path_segment(name), body.compact()));
        } else {
            reqs.push(format!("DELETE /{}/_doc/{}?refresh=true{routing}", path_segment(name), path_segment(&id)));
        }
    }
    Ok(reqs.join("\n\n"))
}

/// A setting from `GET /<index>` (flat `index.x` keys or nested).
/// The `analysis` option (the index's analyzers, tokenizers, filters,
/// normalizers…) as JSON.
pub(crate) fn analysis_opt(o: &BTreeMap<String, String>) -> Result<Option<J>> {
    match opt(o, "analysis") {
        None => Ok(None),
        Some(a) => match J::parse(a) {
            Ok(j @ J::Obj(_)) => Ok(Some(j)),
            _ => Err(Error::Query("«analysis» tiene que ser un objeto JSON (analyzer, tokenizer, filter…).".into())),
        },
    }
}

/// `index.analysis.*` of flat settings as one nested object (`None` when
/// the index defines no analysis of its own).
fn analysis_setting(settings: Option<&J>) -> Option<J> {
    fn put(obj: &mut Obj, path: &[&str], v: J) {
        let Some((first, rest)) = path.split_first() else { return };
        if rest.is_empty() {
            obj.push((first.to_string(), v));
            return;
        }
        if !obj.iter().any(|(k, _)| k == first) {
            obj.push((first.to_string(), J::Obj(Vec::new())));
        }
        if let Some((_, J::Obj(inner))) = obj.iter_mut().find(|(k, _)| k == first) {
            put(inner, rest, v);
        }
    }
    let s = settings?;
    // Flat (`index.analysis.analyzer.x.type`) or nested (`index.analysis`).
    if let Some(nested) = s.at(&["index", "analysis"]) {
        return Some(nested.clone());
    }
    let mut out: Obj = Vec::new();
    let mut flat: Vec<(&String, &J)> = s.as_obj()?.iter().filter(|(k, _)| k.starts_with("index.analysis.")).map(|(k, v)| (k, v)).collect();
    flat.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in flat {
        let path: Vec<&str> = k["index.analysis.".len()..].split('.').collect();
        put(&mut out, &path, v.clone());
    }
    (!out.is_empty()).then_some(J::Obj(out))
}

/// Table option: the index settings without a designer field of their own
/// (`index.max_result_window`, `index.mapping.total_fields.limit`, index
/// sort, pipelines, blocks…), as a JSON object of flat keys.
pub const SETTINGS_EXTRA: &str = "settings_extra";
/// Table option: the lifecycle policy settings (ILM `index.lifecycle.*`,
/// OpenSearch ISM), apart: the policy manages the index they're on.
pub const LIFECYCLE: &str = "lifecycle";
/// Table option: the mapping's parameters besides `properties` and
/// `dynamic` (`dynamic_templates`, `_routing`, `runtime`, `_source`,
/// `_meta`…), as a JSON object.
pub const MAPPINGS_EXTRA: &str = "mappings_extra";

/// Settings the server keeps about the index itself (identity, history):
/// never given back in a create.
const SYSTEM_SETTINGS: &[&str] = &[
    "uuid",
    "creation_date",
    "creation_date_string",
    "provided_name",
    "version.",
    "routing.allocation.initial_recovery.",
    "resize.",
    "shrink.",
    "verified_before_close",
    "history.uuid",
];

/// A table option that holds a JSON object.
fn json_obj_opt(o: &BTreeMap<String, String>, key: &str) -> Result<Option<Obj>> {
    match opt(o, key) {
        None => Ok(None),
        Some(a) => match J::parse(a) {
            Ok(J::Obj(m)) => Ok(Some(m)),
            _ => Err(Error::Query(format!("«{key}» tiene que ser un objeto JSON."))),
        },
    }
}

/// The `index.*` settings the designer options don't cover, flat and
/// sorted: the rest and the lifecycle ones (see [`LIFECYCLE`]). System
/// settings ([`SYSTEM_SETTINGS`]) are left out.
fn other_settings(settings: Option<&J>, opensearch: bool) -> (Obj, Obj) {
    fn flatten(prefix: &str, v: &J, out: &mut Obj) {
        match v.as_obj() {
            Some(m) => {
                for (k, x) in m {
                    flatten(&format!("{prefix}.{k}"), x, out);
                }
            }
            None => out.push((prefix.to_string(), v.clone())),
        }
    }
    let mut flat: Obj = Vec::new();
    for (k, v) in settings.and_then(J::as_obj).into_iter().flatten() {
        flatten(k, v, &mut flat);
    }
    flat.sort_by(|a, b| a.0.cmp(&b.0));
    let (mut extra, mut lifecycle) = (Vec::new(), Vec::new());
    for (k, v) in flat {
        let Some(rel) = k.strip_prefix("index.") else { continue };
        let known = matches!(rel, "number_of_shards" | "number_of_replicas" | "refresh_interval")
            || (opensearch && rel == "knn")
            || rel.starts_with("analysis.");
        let system = SYSTEM_SETTINGS.iter().any(|s| if s.ends_with('.') { rel.starts_with(s) } else { rel == *s });
        if known || system {
            continue;
        }
        if ["lifecycle.", "plugins.index_state_management.", "opendistro.index_state_management."].iter().any(|p| rel.starts_with(p)) {
            lifecycle.push((k, v));
        } else {
            extra.push((k, v));
        }
    }
    (extra, lifecycle)
}

fn setting<'a>(settings: Option<&'a J>, key: &str) -> Option<&'a J> {
    let s = settings?;
    s.get(&format!("index.{key}")).or_else(|| s.at(&["index", key])).or_else(|| s.get(key))
}

fn read_props(props: Option<&J>, prefix: &str, out: &mut Vec<ColumnDef>) {
    for (name, f) in props.and_then(J::as_obj).into_iter().flatten() {
        let Some(fo) = f.as_obj() else { continue };
        let full = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let mut options = BTreeMap::new();
        let mut extra: Obj = Vec::new();
        let mut comment = None;
        for (k, v) in fo {
            match k.as_str() {
                "type" | "properties" => {}
                "analyzer" | "format" | "index" | "doc_values" | "scaling_factor" if v.is_scalar() => {
                    options.insert(k.clone(), v.text());
                }
                "dims" | "dimension" if v.is_scalar() => {
                    options.insert("dims".into(), v.text());
                }
                "meta" if v.as_obj().is_some_and(|m| m.len() == 1) && v.get("description").is_some() => {
                    comment = v.get("description").map(J::text);
                }
                _ => extra.push((k.clone(), v.clone())),
            }
        }
        if !extra.is_empty() {
            options.insert("extra".into(), J::Obj(extra).compact());
        }
        out.push(ColumnDef {
            name: full.clone(),
            data_type: f.get("type").map_or_else(|| "object".into(), J::text),
            nullable: true,
            comment,
            options,
            ..Default::default()
        });
        read_props(f.get("properties"), &full, out);
    }
}

/// An index as the designer's model, from its entry in `GET /<index>`
/// (`aliases`, `mappings`, `settings`).
pub fn index_schema(name: &str, idx: &J, opensearch: bool) -> TableSchema {
    let m = idx.get("mappings").unwrap_or(&J::Null);
    let mut columns = Vec::new();
    read_props(m.get("properties"), "", &mut columns);
    let s = idx.get("settings");
    let mut options = BTreeMap::new();
    let mut keys = vec!["number_of_shards", "number_of_replicas", "refresh_interval"];
    if opensearch {
        keys.push("knn");
    }
    for k in keys {
        if let Some(v) = setting(s, k) {
            options.insert(k.to_string(), v.text());
        }
    }
    if let Some(d) = m.get("dynamic") {
        options.insert("dynamic".into(), d.text());
    }
    if let Some(a) = analysis_setting(s) {
        options.insert("analysis".into(), a.compact());
    }
    let (extra, lifecycle) = other_settings(s, opensearch);
    for (key, v) in [(SETTINGS_EXTRA, extra), (LIFECYCLE, lifecycle)] {
        if !v.is_empty() {
            options.insert(key.into(), J::Obj(v).compact());
        }
    }
    let only_description = |v: &J| v.as_obj().is_some_and(|m| m.len() == 1) && v.get("description").is_some();
    let mut mapping_extra: Obj = m
        .as_obj()
        .into_iter()
        .flatten()
        .filter(|(k, v)| !matches!(k.as_str(), "properties" | "dynamic") && !(k == "_meta" && only_description(v)))
        .cloned()
        .collect();
    mapping_extra.sort_by(|a, b| a.0.cmp(&b.0));
    if !mapping_extra.is_empty() {
        options.insert(MAPPINGS_EXTRA.into(), J::Obj(mapping_extra).compact());
    }
    let aliases: Vec<&str> = idx.get("aliases").and_then(J::as_obj).into_iter().flatten().map(|(k, _)| k.as_str()).collect();
    if !aliases.is_empty() {
        options.insert("aliases".into(), aliases.join(","));
    }
    TableSchema {
        kind: kinds::INDEX.into(),
        schema: None,
        name: name.to_string(),
        columns,
        comment: m.at(&["_meta", "description"]).map(J::text),
        options,
        ..Default::default()
    }
}

/// The browse request (`GET /idx/_search` + `match_all`) restricted by the
/// grid's column filters: a `bool` query with one `filter` clause per
/// filter. Text equality is a `match_phrase` (exact on keyword fields),
/// numbers and booleans a `term`; text matches are case-insensitive
/// `wildcard` / `prefix` queries (escaped); null is a missing field. SQL
/// conditions don't apply to the query DSL.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::FilterOp;
    use serde_json::json;
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let (head, body) = browse.split_once('\n').unwrap_or((browse, "{}"));
    let Ok(Value::Object(mut body)) = serde_json::from_str::<Value>(body) else {
        return Err(Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()));
    };
    let wild = |s: &str| s.replace('\\', "\\\\").replace('*', "\\*").replace('?', "\\?");
    let mut clauses = Vec::new();
    for f in filters {
        let c = f.column.as_str();
        let first = || f.values.first().cloned().ok_or_else(|| Error::Query(format!("el filtro de «{c}» necesita un valor")));
        let text = || first().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
        let eq = |v: Value| match v {
            Value::String(_) if c != "_id" => json!({ "match_phrase": { c: v } }),
            other => json!({ "term": { c: other } }),
        };
        let not = |q: Value| json!({ "bool": { "must_not": [q] } });
        let range = |op: &str| -> Result<Value> { Ok(json!({ "range": { c: { op: first()? } } })) };
        let wildcard = |p: String| json!({ "wildcard": { c: { "value": p, "case_insensitive": true } } });
        let exists = json!({ "exists": { "field": c } });
        let any = |vs: &[Value]| json!({ "bool": { "should": vs.iter().cloned().map(eq).collect::<Vec<_>>(), "minimum_should_match": 1 } });
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{c}» necesita al menos un valor")));
            }
            Ok(any(&f.values))
        };
        clauses.push(match f.op {
            FilterOp::Eq => eq(first()?),
            FilterOp::Ne => not(eq(first()?)),
            FilterOp::Gt => range("gt")?,
            FilterOp::Ge => range("gte")?,
            FilterOp::Lt => range("lt")?,
            FilterOp::Le => range("lte")?,
            FilterOp::Contains => wildcard(format!("*{}*", wild(&text()?))),
            FilterOp::NotContains => not(wildcard(format!("*{}*", wild(&text()?)))),
            FilterOp::StartsWith => json!({ "prefix": { c: { "value": text()?, "case_insensitive": true } } }),
            FilterOp::EndsWith => wildcard(format!("*{}", wild(&text()?))),
            FilterOp::IsNull => not(exists),
            FilterOp::NotNull => exists,
            FilterOp::IsEmpty => json!({ "term": { c: "" } }),
            FilterOp::NotEmpty => json!({ "bool": { "filter": [exists], "must_not": [{ "term": { c: "" } }] } }),
            FilterOp::In => list()?,
            FilterOp::NotIn => not(list()?),
            FilterOp::IsTrue => json!({ "term": { c: true } }),
            FilterOp::IsFalse => json!({ "term": { c: false } }),
            FilterOp::TrueOrNull | FilterOp::FalseOrNull => {
                let b = f.op == FilterOp::TrueOrNull;
                json!({ "bool": { "should": [{ "term": { c: b } }, not(exists)], "minimum_should_match": 1 } })
            }
            FilterOp::Sql | FilterOp::SqlRight => {
                return Err(Error::Unsupported("la búsqueda JSON no toma condiciones SQL: se filtran en la grilla".into()))
            }
        });
    }
    body.insert("query".into(), json!({ "bool": { "filter": clauses } }));
    let body = serde_json::to_string_pretty(&Value::Object(body)).map_err(|e| Error::Query(e.to_string()))?;
    Ok(format!("{head}\n{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::{self, Command};
    use serde_json::json;

    #[test]
    fn filtered_browse_builds_a_bool_query() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        let got = filtered_browse(
            "GET /logs/_search\n{\n  \"size\": 200,\n  \"query\": { \"match_all\": {} }\n}",
            &[
                f("user.name", FilterOp::Eq, vec![json!("O'Brien \"Bob\"")]),
                f("bytes", FilterOp::Ge, vec![json!(100)]),
                f("path", FilterOp::Contains, vec![json!("a*b")]),
                f("error", FilterOp::IsNull, vec![]),
                f("status", FilterOp::In, vec![json!(200), json!(204)]),
            ],
        )
        .unwrap();
        let (head, body) = got.split_once('\n').unwrap();
        assert_eq!(head, "GET /logs/_search");
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({ "size": 200, "query": { "bool": { "filter": [
                { "match_phrase": { "user.name": "O'Brien \"Bob\"" } },
                { "range": { "bytes": { "gte": 100 } } },
                { "wildcard": { "path": { "value": "*a\\*b*", "case_insensitive": true } } },
                { "bool": { "must_not": [{ "exists": { "field": "error" } }] } },
                { "bool": { "should": [{ "term": { "status": 200 } }, { "term": { "status": 204 } }], "minimum_should_match": 1 } }
            ] } } })
        );
        assert!(!console::parse(&got).unwrap().is_empty());
        assert!(matches!(
            filtered_browse("GET /x/_search\n{}", &[ColumnFilter { column: "a".into(), op: FilterOp::Sql, values: vec![], sql: Some("1".into()) }]),
            Err(Error::Unsupported(_))
        ));
    }

    fn col(name: &str, ty: &str, opts: &[(&str, &str)]) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            options: opts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    fn books() -> TableSchema {
        TableSchema {
            kind: kinds::INDEX.into(),
            name: "books".into(),
            comment: Some("Catálogo".into()),
            columns: vec![
                col("title", "text", &[("analyzer", "english"), ("extra", r#"{"fields":{"raw":{"type":"keyword"}}}"#)]),
                col("author.name", "keyword", &[("doc_values", "false")]),
                col("author", "object", &[]),
                col("year", "integer", &[("index", "")]),
                col("published", "date", &[("format", "yyyy-MM-dd")]),
                col("price", "scaled_float", &[("scaling_factor", "100")]),
                col("tags", "nested", &[]),
                col("tags.label", "keyword", &[]),
                col("vec", "dense_vector", &[("dims", "3")]),
                col("_id", "keyword", &[]),
            ],
            options: [
                ("number_of_shards", "1"),
                ("number_of_replicas", "0"),
                ("aliases", "b1, b2"),
                ("dynamic", "strict"),
                ("refresh_interval", ""),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            ..Default::default()
        }
    }

    fn one_request(text: &str) -> Vec<console::Request> {
        console::parse(text)
            .unwrap()
            .into_iter()
            .map(|c| match c {
                Command::Http(r) => r,
                Command::Sql(s) => panic!("unexpected SQL {s}"),
            })
            .collect()
    }

    #[test]
    fn index_ddl_parses_and_nests() {
        let parts = DdlParts { drop: true, if_exists: true, create: true, ..Default::default() };
        let text = index_ddl(&books(), parts, false).unwrap();
        let reqs = one_request(&text);
        assert_eq!(reqs.len(), 2);
        assert_eq!((reqs[0].method.as_str(), reqs[0].path.as_str()), ("DELETE", "/books?ignore_unavailable=true"));
        assert_eq!((reqs[1].method.as_str(), reqs[1].path.as_str()), ("PUT", "/books"));
        let body: serde_json::Value = serde_json::from_str(reqs[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["settings"], json!({"number_of_shards": 1, "number_of_replicas": 0}));
        assert_eq!(body["aliases"], json!({"b1": {}, "b2": {}}));
        let m = &body["mappings"];
        assert_eq!(m["dynamic"], "strict");
        assert_eq!(m["_meta"]["description"], "Catálogo");
        let p = &m["properties"];
        assert_eq!(p["title"], json!({"type": "text", "analyzer": "english", "fields": {"raw": {"type": "keyword"}}}));
        assert_eq!(p["author"], json!({"type": "object", "properties": {"name": {"type": "keyword", "doc_values": false}}}));
        assert_eq!(p["year"], json!({"type": "integer"}));
        assert_eq!(p["price"]["scaling_factor"], 100);
        assert_eq!(p["tags"], json!({"type": "nested", "properties": {"label": {"type": "keyword"}}}));
        assert_eq!(p["vec"], json!({"type": "dense_vector", "dims": 3}));
        assert!(p.get("_id").is_none());

        let os = index_ddl(&books(), DdlParts { create: true, ..Default::default() }, true).unwrap();
        assert!(os.contains("\"dimension\": 3") && !os.contains("DELETE"));
    }

    #[test]
    fn index_ddl_errors() {
        let mut t = books();
        t.name = "Books".into();
        assert!(index_ddl(&t, DdlParts { create: true, ..Default::default() }, false).is_err());
        let mut t = books();
        t.columns.push(col("year.x", "keyword", &[]));
        let e = index_ddl(&t, DdlParts { create: true, ..Default::default() }, false).unwrap_err();
        assert!(e.to_string().contains("year"), "{e}");
        let mut t = books();
        t.columns.push(col("title", "keyword", &[]));
        assert!(index_ddl(&t, DdlParts { create: true, ..Default::default() }, false).is_err());
        let mut t = books();
        t.columns[0].options.insert("extra".into(), "[1]".into());
        assert!(index_ddl(&t, DdlParts { create: true, ..Default::default() }, false).is_err());
        let empty = TableSchema { name: "e".into(), ..Default::default() };
        assert_eq!(index_ddl(&empty, DdlParts { create: true, ..Default::default() }, false).unwrap(), "PUT /e");
    }

    #[test]
    fn schema_round_trips() {
        let t = books();
        let body = J::Obj(index_body(&t, false).unwrap());
        let back = index_schema("books", &body, false);
        let again = J::Obj(index_body(&back, false).unwrap());
        assert_eq!(body.get("mappings"), again.get("mappings"));
        assert_eq!(back.options.get("aliases").map(String::as_str), Some("b1,b2"));
        assert_eq!(back.options.get("number_of_shards").map(String::as_str), Some("1"));
        assert_eq!(back.comment.as_deref(), Some("Catálogo"));
        let names: Vec<_> = back.columns.iter().map(|c| format!("{}:{}", c.name, c.data_type)).collect();
        assert!(names.contains(&"author:object".to_string()) && names.contains(&"author.name:keyword".to_string()), "{names:?}");

        // What a server returns: flat settings, objects without "type".
        let got = J::parse(
            r#"{"aliases":{"a":{}},"mappings":{"dynamic":"false","properties":{"o":{"properties":{"x":{"type":"long","meta":{"description":"equis"}}}}}},
                "settings":{"index.number_of_shards":"2","index.knn":"true"}}"#,
        )
        .unwrap();
        let s = index_schema("i", &got, true);
        assert_eq!(s.options.get("number_of_shards").map(String::as_str), Some("2"));
        assert_eq!(s.options.get("knn").map(String::as_str), Some("true"));
        assert_eq!(s.options.get("dynamic").map(String::as_str), Some("false"));
        assert_eq!(s.columns[0].data_type, "object");
        assert_eq!(s.columns[1].comment.as_deref(), Some("equis"));
        assert!(!s.options.contains_key("analysis"));

        // The index's analysis, from flat settings, back into the create body.
        let got = J::parse(
            r#"{"settings":{"index.analysis.analyzer.es.type":"custom","index.analysis.analyzer.es.tokenizer":"standard",
                "index.analysis.analyzer.es.filter":["lowercase","asciifolding"],"index.analysis.normalizer.n.type":"custom"}}"#,
        )
        .unwrap();
        let s = index_schema("i", &got, false);
        let a = s.options.get("analysis").unwrap();
        assert_eq!(a, r#"{"analyzer":{"es":{"filter":["lowercase","asciifolding"],"tokenizer":"standard","type":"custom"}},"normalizer":{"n":{"type":"custom"}}}"#);
        let body = J::Obj(index_body(&s, false).unwrap());
        assert_eq!(body.at(&["settings", "analysis"]).map(J::compact).as_deref(), Some(a.as_str()));
        // The nested form reads the same.
        let nested = J::parse(&format!(r#"{{"settings":{{"index":{{"analysis":{a}}}}}}}"#)).unwrap();
        assert_eq!(index_schema("i", &nested, false).options.get("analysis"), Some(a));
    }

    /// "Clonar tabla": settings and mapping parameters without a designer
    /// field go back into the create as they were; system settings don't.
    #[test]
    fn other_settings_and_mapping_parameters_round_trip() {
        let got = J::parse(
            r#"{"aliases":{"al":{}},"mappings":{"dynamic":"strict","_routing":{"required":true},
                "dynamic_templates":[{"s":{"match_mapping_type":"string","mapping":{"type":"keyword"}}}],
                "runtime":{"r":{"type":"long"}},"_meta":{"description":"d","owner":"x"},"properties":{"k":{"type":"keyword"}}},
                "settings":{"index.number_of_shards":"1","index.max_result_window":"50000","index.mapping.total_fields.limit":"2000",
                "index.sort.field":["k"],"index.default_pipeline":"p","index.blocks.write":"true","index.uuid":"U","index.creation_date":"1",
                "index.provided_name":"src","index.version.created":"136","index.lifecycle.name":"pol","index.analysis.analyzer.a.type":"standard"}}"#,
        )
        .unwrap();
        let s = index_schema("src", &got, true);
        let extra = s.options.get(SETTINGS_EXTRA).unwrap();
        assert_eq!(
            extra,
            r#"{"index.blocks.write":"true","index.default_pipeline":"p","index.mapping.total_fields.limit":"2000","index.max_result_window":"50000","index.sort.field":["k"]}"#
        );
        assert_eq!(s.options.get(LIFECYCLE).map(String::as_str), Some(r#"{"index.lifecycle.name":"pol"}"#));
        assert_eq!(s.comment.as_deref(), Some("d"));
        let body = J::Obj(index_body(&s, true).unwrap());
        assert_eq!(body.at(&["settings", "index.max_result_window"]).map(J::text).as_deref(), Some("50000"));
        assert_eq!(body.at(&["settings", "index.lifecycle.name"]).map(J::text).as_deref(), Some("pol"));
        assert!(body.at(&["settings", "index.uuid"]).is_none() && body.at(&["settings", "index.provided_name"]).is_none());
        assert_eq!(body.at(&["mappings", "_routing", "required"]).and_then(J::as_bool), Some(true));
        assert!(body.at(&["mappings", "dynamic_templates"]).is_some() && body.at(&["mappings", "runtime", "r"]).is_some());
        assert_eq!(body.at(&["mappings", "_meta"]).map(J::compact).as_deref(), Some(r#"{"description":"d","owner":"x"}"#));
        // Read back, the same model.
        let back = index_schema("src", &body, true);
        for k in [SETTINGS_EXTRA, LIFECYCLE, MAPPINGS_EXTRA, "number_of_shards"] {
            assert_eq!(back.options.get(k), s.options.get(k), "{k}");
        }
    }

    #[test]
    fn bulk_script_is_ndjson() {
        let target = ObjectRef { kind: kinds::INDEX.into(), schema: None, name: "books".into() };
        let cols: Vec<String> = ["_index", "_id", "_score", "title", "author", "year"].iter().map(|s| s.to_string()).collect();
        let rows: Vec<Vec<Value>> = (0..501)
            .map(|i| vec![json!("x"), json!(i), json!(1.0), json!("T\n\"q\""), json!(r#"{"name":"A"}"#), Value::Null])
            .collect();
        let text = bulk_script(&target, &cols, &rows).unwrap();
        let reqs = one_request(&text);
        assert_eq!(reqs.len(), 2);
        assert!(reqs[0].is_ndjson());
        assert_eq!(reqs[0].path, "/_bulk?refresh=true");
        let lines: Vec<&str> = reqs[0].body.as_deref().unwrap().lines().collect();
        assert_eq!(lines.len(), 1000);
        assert_eq!(lines[0], r#"{"index":{"_index":"books","_id":"0"}}"#);
        assert_eq!(lines[1], r#"{"title":"T\n\"q\"","author":{"name":"A"}}"#);
        assert_eq!(reqs[1].body.as_deref().unwrap().lines().count(), 2);

        let ds = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "logs".into() };
        let text = bulk_script(&ds, &["@timestamp".to_string()], &[vec![json!("2024-01-01")]]).unwrap();
        assert!(text.contains(r#"{"create":{"_index":"logs"}}"#));
        assert_eq!(bulk_script(&target, &cols, &[]).unwrap(), "");
    }

    #[test]
    fn update_script_is_partial_doc() {
        let target = ObjectRef { kind: kinds::INDEX.into(), schema: None, name: "books".into() };
        let changes = vec![
            RowChange {
                key: vec![("_id".into(), json!("a/b 1")), ("_routing".into(), json!("r"))],
                set: vec![("title".into(), json!("O'Brien \"Bob\"")), ("year".into(), Value::Null), ("author".into(), json!(r#"{"name":"A"}"#))], ..Default::default()
            },
            RowChange { key: vec![("_id".into(), json!(2))], set: vec![], ..Default::default() },
        ];
        let text = update_script(&target, &changes).unwrap();
        assert_eq!(
            text,
            "POST /books/_update/a%2Fb%201?refresh=true&routing=r\n{\"doc\":{\"title\":\"O'Brien \\\"Bob\\\"\",\"year\":null,\"author\":{\"name\":\"A\"}}}"
        );
        let reqs = one_request(&text);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        let ds = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "logs".into() };
        let one = vec![RowChange { key: vec![("_id".into(), json!("x"))], set: vec![("msg".into(), json!("hi"))], ..Default::default() }];
        assert_eq!(
            update_script(&ds, &one).unwrap(),
            format!("POST /logs/_update_by_query?refresh=true\n{{\"query\":{{\"ids\":{{\"values\":[\"x\"]}}}},\"script\":{{\"source\":\"{STREAM_UPDATE}\",\"params\":{{\"doc\":{{\"msg\":\"hi\"}}}}}}}}")
        );
        let no_id = vec![RowChange { key: vec![("title".into(), json!("x"))], set: vec![("year".into(), json!(1))], ..Default::default() }];
        assert!(update_script(&target, &no_id).is_err());
    }

    #[test]
    fn delete_script_by_id() {
        let target = ObjectRef { kind: kinds::INDEX.into(), schema: None, name: "books".into() };
        let keys = vec![
            vec![("_id".into(), json!("O'Brien \"Bob\" a/b")), ("_routing".into(), json!("r"))],
            vec![("_id".into(), json!(2))],
        ];
        let text = delete_script(&target, &keys).unwrap();
        assert_eq!(
            text,
            "DELETE /books/_doc/O%27Brien%20%22Bob%22%20a%2Fb?refresh=true&routing=r\n\nDELETE /books/_doc/2?refresh=true"
        );
        let reqs = one_request(&text);
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].method, "DELETE");
        assert_eq!(reqs[1].path, "/books/_doc/2?refresh=true");
        let ds = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "logs".into() };
        let text = delete_script(&ds, &[vec![("_id".into(), json!("x"))]]).unwrap();
        assert_eq!(text, "POST /logs/_delete_by_query?refresh=true\n{\"query\":{\"ids\":{\"values\":[\"x\"]}}}");
        assert_eq!(one_request(&text).len(), 1);
        assert!(delete_script(&target, &[vec![("title".into(), json!("x"))]]).is_err());
        assert!(delete_script(&target, &[vec![]]).is_err());
    }

    #[test]
    fn templates_parse() {
        for os in [false, true] {
            for t in templates(os) {
                let text = t.template.replace("{name}", "x");
                let reqs = one_request(&text);
                assert!(!reqs.is_empty(), "{}", t.label);
                for r in reqs {
                    if let Some(b) = &r.body {
                        serde_json::from_str::<serde_json::Value>(b).unwrap_or_else(|e| panic!("{}: {e}", t.label));
                    }
                }
            }
        }
    }
}

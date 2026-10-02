//! Collection designer, generated scripts and the database schema model,
//! all in the driver's own shell language (see `shell.rs`).
//!
//! # Designer model ([`TableSchema`])
//!
//! - Fields (`columns`) become a `$jsonSchema` validator: `data_type` is the
//!   BSON type (`string`, `int`, `date`…; several joined with `|`; empty or
//!   `any` = no type check), `nullable = true` also admits `null`, the
//!   column option `required = "true"` lists it in `required`, and the
//!   comment becomes the `description`. Table option `validator` (JSON)
//!   overrides the generated validator; `validate_fields = "false"` skips
//!   it (what `database_schema` reports, so a generated script doesn't add
//!   constraints the collection never had).
//! - Table options: `capped`, `size`, `max`, `validationLevel`,
//!   `validationAction`, `timeField`, `metaField`, `granularity`,
//!   `expireAfterSeconds`, `clustered` (clustered index on `_id`),
//!   `collation` (JSON), and for views (`kind = "view"`) `viewOn` and
//!   `pipeline`.
//! - The validator, as `database_schema` reports it, is the collection's
//!   only CHECK ([`validator_check`]); it wins over the `validator` option.
//! - Indexes ([`IndexDef`]): `columns` are `field` (ascending), `field:-1`
//!   (descending) or `field:<type>` (`text`, `2dsphere`, `2d`, `hashed`).
//!   `kind` is a comma-separated list of: a key type applied to the fields
//!   without a suffix (`text` or `FULLTEXT`, `2dsphere`, `2d`, `hashed`),
//!   `ttl:<seconds>` (`expireAfterSeconds`) and `sparse`. `filter` is the
//!   `partialFilterExpression` (relaxed JSON, shell helpers allowed).
//!   `options` are `createIndex` options by their server name
//!   (`expireAfterSeconds`, `sparse`, `hidden`, `collation`, `weights`,
//!   `default_language`, `language_override`, `wildcardProjection`, `bits`,
//!   `min`, `max`), JSON values or text.

use crate::shell;
use dbine_driver::{
    kinds, CheckDef, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef, KeyDef, ObjectRef,
    Result, RowChange, TableSchema,
};
use dbine_driver::filter::{ColumnFilter, FilterOp};
use mongodb::bson::{Bson, Document};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// BSON type names `$jsonSchema` accepts (`number` = any numeric type).
const BSON_TYPES: &[&str] = &[
    "string", "int", "long", "double", "decimal", "number", "bool", "date", "objectId", "object", "array", "binData",
    "timestamp", "regex", "null", "javascript", "minKey", "maxKey",
];

pub fn designer() -> DesignerSpec {
    let bool_field = |k, l| Field::new(k, l, FieldKind::Bool);
    DesignerSpec {
        kind: kinds::COLLECTION,
        label: "Nueva colección",
        data_types: BSON_TYPES.iter().copied().filter(|t| !matches!(*t, "null" | "javascript" | "minKey" | "maxKey")).collect(),
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: true,
        comments: true,
        indexes: true,
        foreign_keys: false,
        column_options: vec![bool_field("required", "Obligatorio")
            .help("El campo tiene que estar en cada documento (lista `required` del validador $jsonSchema).")],
        table_options: vec![
            bool_field("validate_fields", "Validar con los campos ($jsonSchema)")
                .default_value("true")
                .help("Genera un validador $jsonSchema con los tipos, «no nulo» y «obligatorio» de los campos."),
            Field::new("validator", "Validador propio (JSON)", FieldKind::Textarea)
                .placeholder("{ $jsonSchema: { … } }")
                .help("Si se completa, reemplaza al validador generado con los campos."),
            Field::new(
                "validationLevel",
                "Nivel de validación",
                FieldKind::Select(vec![("", "(predeterminado)"), ("strict", "strict"), ("moderate", "moderate"), ("off", "off")]),
            ),
            Field::new(
                "validationAction",
                "Acción de validación",
                FieldKind::Select(vec![("", "(predeterminada)"), ("error", "error"), ("warn", "warn")]),
            ),
            bool_field("capped", "Colección de tamaño fijo (capped)"),
            Field::new("size", "Tamaño máximo (bytes)", FieldKind::Number).help("Obligatorio si es capped."),
            Field::new("max", "Máximo de documentos", FieldKind::Number).help("Solo para capped."),
            Field::new("timeField", "Serie temporal: campo de tiempo", FieldKind::Text)
                .help("Si se completa, crea una colección de series temporales."),
            Field::new("metaField", "Serie temporal: campo de metadatos", FieldKind::Text),
            Field::new(
                "granularity",
                "Serie temporal: granularidad",
                FieldKind::Select(vec![("", "(predeterminada)"), ("seconds", "seconds"), ("minutes", "minutes"), ("hours", "hours")]),
            ),
            bool_field("clustered", "Índice agrupado por _id (clustered)"),
            Field::new("expireAfterSeconds", "Expirar documentos (segundos)", FieldKind::Number)
                .help("Solo para series temporales o colecciones agrupadas."),
        ],
        columns_required: false,
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(
            kinds::VIEW,
            "Nueva vista",
            "db.createView(\"{name}\", \"coleccion_origen\", [\n  { $match: { activo: true } },\n  { $project: { _id: 1, nombre: 1 } }\n])",
        ),
        t(
            kinds::COLLECTION,
            "Nueva serie temporal",
            "db.createCollection(\"{name}\", {\n  timeseries: { timeField: \"ts\", metaField: \"sensor\", granularity: \"minutes\" },\n  expireAfterSeconds: 2592000\n})",
        ),
        t(
            kinds::COLLECTION,
            "Nueva colección capped",
            "db.createCollection(\"{name}\", { capped: true, size: 10485760, max: 10000 })",
        ),
        t(
            kinds::COLLECTION,
            "Cambiar validador ($jsonSchema)",
            "db.runCommand({\n  collMod: \"{name}\",\n  validator: { $jsonSchema: {\n    bsonType: \"object\",\n    required: [\"nombre\"],\n    properties: {\n      nombre: { bsonType: \"string\", description: \"obligatorio\" },\n      edad: { bsonType: [\"int\", \"null\"], minimum: 0 }\n    }\n  } },\n  validationLevel: \"moderate\",\n  validationAction: \"error\"\n})",
        ),
        t(
            kinds::INDEX,
            "Nuevo índice",
            "db.getCollection(\"{name}\").createIndex({ campo: 1, fecha: -1 }, { name: \"campo_1_fecha_-1\", unique: false })",
        ),
    ]
}

pub(crate) fn q(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

pub(crate) fn opt<'a>(t: &'a TableSchema, k: &str) -> Option<&'a str> {
    t.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty())
}

pub(crate) fn is_true(v: Option<&str>) -> bool {
    matches!(v, Some("true" | "1"))
}

fn number(t: &TableSchema, k: &str) -> Result<Option<i64>> {
    match opt(t, k) {
        None => Ok(None),
        Some(v) => v.parse::<i64>().map(Some).map_err(|_| Error::Query(format!("«{k}» tiene que ser un número entero: {v}"))),
    }
}

pub(crate) fn relaxed(text: &str, what: &str) -> Result<Value> {
    shell::parse_value(text).map_err(|e| Error::Query(format!("{what}: {e}")))
}

/// `string|null` → `["string", "null"]`; `None` = no type constraint.
fn bson_types(data_type: &str, nullable: bool) -> Result<Option<Value>> {
    let mut types: Vec<String> = Vec::new();
    for raw in data_type.split('|').map(str::trim).filter(|t| !t.is_empty()) {
        let t = match raw.to_ascii_lowercase().as_str() {
            "any" | "mixed" | "*" => return Ok(None),
            "integer" | "int32" => "int".to_string(),
            "int64" => "long".into(),
            "boolean" => "bool".into(),
            "objectid" | "oid" => "objectId".into(),
            "bindata" | "binary" => "binData".into(),
            "decimal128" => "decimal".into(),
            "minkey" => "minKey".into(),
            "maxkey" => "maxKey".into(),
            "datetime" => "date".into(),
            "undefined" => continue,
            l => match BSON_TYPES.iter().find(|b| b.eq_ignore_ascii_case(l)) {
                Some(b) => b.to_string(),
                None => return Err(Error::Query(format!("tipo BSON desconocido: «{raw}»"))),
            },
        };
        if !types.contains(&t) {
            types.push(t);
        }
    }
    if types.is_empty() {
        return Ok(None);
    }
    if nullable && !types.iter().any(|t| t == "null") {
        types.push("null".into());
    }
    Ok(Some(if types.len() == 1 { Value::String(types.remove(0)) } else { json!(types) }))
}

/// The `$jsonSchema` validator built from the fields (`None` when they
/// constrain nothing).
fn fields_validator(columns: &[ColumnDef]) -> Result<Option<Value>> {
    let mut props = Map::new();
    let mut required = Vec::new();
    for c in columns.iter().filter(|c| !c.name.trim().is_empty()) {
        let mut p = Map::new();
        if let Some(t) = bson_types(&c.data_type, c.nullable)? {
            p.insert("bsonType".into(), t);
        }
        if let Some(d) = c.comment.as_deref().filter(|d| !d.trim().is_empty()) {
            p.insert("description".into(), Value::String(d.into()));
        }
        if is_true(c.options.get("required").map(String::as_str)) {
            required.push(Value::String(c.name.clone()));
        }
        if !p.is_empty() {
            props.insert(c.name.clone(), Value::Object(p));
        }
    }
    if props.is_empty() && required.is_empty() {
        return Ok(None);
    }
    let mut s = Map::new();
    s.insert("bsonType".into(), "object".into());
    if !required.is_empty() {
        s.insert("required".into(), Value::Array(required));
    }
    if !props.is_empty() {
        s.insert("properties".into(), Value::Object(props));
    }
    Ok(Some(json!({ "$jsonSchema": s })))
}

/// `db.createCollection` options from the designer's table options.
pub(crate) fn collection_options(t: &TableSchema) -> Result<Map<String, Value>> {
    let mut o = Map::new();
    let ttl = number(t, "expireAfterSeconds")?;
    let clustered = is_true(opt(t, "clustered"));
    if is_true(opt(t, "capped")) {
        let size = number(t, "size")?.ok_or_else(|| Error::Query("una colección capped necesita el tamaño máximo en bytes".into()))?;
        o.insert("capped".into(), true.into());
        o.insert("size".into(), size.into());
        if let Some(m) = number(t, "max")? {
            o.insert("max".into(), m.into());
        }
    }
    if let Some(tf) = opt(t, "timeField") {
        let mut ts = Map::new();
        ts.insert("timeField".into(), tf.into());
        if let Some(m) = opt(t, "metaField") {
            ts.insert("metaField".into(), m.into());
        }
        if let Some(g) = opt(t, "granularity") {
            ts.insert("granularity".into(), g.into());
        }
        o.insert("timeseries".into(), Value::Object(ts));
    }
    if clustered {
        let mut ci = json!({ "key": { "_id": 1 }, "unique": true });
        if let Some(n) = opt(t, CLUSTERED_NAME) {
            ci["name"] = n.into();
        }
        o.insert("clusteredIndex".into(), ci);
    }
    if let Some(s) = ttl {
        if !o.contains_key("timeseries") && !clustered {
            return Err(Error::Query(
                "«Expirar documentos» solo vale para series temporales o colecciones agrupadas; para las demás use un índice TTL".into(),
            ));
        }
        o.insert("expireAfterSeconds".into(), s.into());
    }
    if let Some(c) = opt(t, "collation") {
        o.insert("collation".into(), relaxed(c, "intercalación (collation)")?);
    }
    // The CHECK (what `database_schema` reports) wins over the designer's
    // validator option and the fields.
    let check = validator_check(t)?;
    let validator = match (&check, opt(t, "validator")) {
        (Some(c), _) => c.get("validator").cloned(),
        (None, Some(v)) => Some(relaxed(v, "validador")?),
        (None, None) if opt(t, "validate_fields") != Some("false") => fields_validator(&t.columns)?,
        (None, None) => None,
    };
    if let Some(v) = validator {
        o.insert("validator".into(), v);
    }
    for k in ["validationLevel", "validationAction"] {
        if let Some(v) = check.as_ref().and_then(|c| c.get(k)).cloned().or_else(|| opt(t, k).map(Value::from)) {
            o.insert(k.into(), v);
        }
    }
    Ok(o)
}

/// Key document and options of an index (see the module docs).
pub(crate) fn index_parts(ix: &IndexDef) -> Result<(Map<String, Value>, Map<String, Value>)> {
    let mut default_type: Option<Value> = None;
    let mut o = Map::new();
    for tok in ix.kind.as_deref().unwrap_or("").split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let l = tok.to_ascii_lowercase();
        if let Some(n) = l.strip_prefix("ttl:").or_else(|| l.strip_prefix("ttl=")) {
            let s: i64 = n.trim().parse().map_err(|_| Error::Query(format!("índice {}: TTL inválido «{tok}»", ix.name)))?;
            o.insert("expireAfterSeconds".into(), s.into());
        } else if l == "sparse" {
            o.insert("sparse".into(), true.into());
        } else if matches!(l.as_str(), "text" | "fulltext") {
            default_type = Some(Value::String("text".into()));
        } else if matches!(l.as_str(), "2dsphere" | "2d" | "hashed") {
            default_type = Some(Value::String(l));
        } else if matches!(l.as_str(), "asc" | "btree" | "1") {
        } else {
            return Err(Error::Query(format!(
                "índice {}: tipo «{tok}» desconocido (text, 2dsphere, 2d, hashed, ttl:<segundos>, sparse)",
                ix.name
            )));
        }
    }
    // Index options as the server names them (`weights`, `collation`,
    // `expireAfterSeconds`…): JSON values, anything else as text.
    for (k, v) in &ix.options {
        let v = v.trim();
        if k.trim().is_empty() || v.is_empty() {
            continue;
        }
        let val = serde_json::from_str::<Value>(v).or_else(|_| if v.starts_with(['{', '[']) { relaxed(v, &format!("opción {k} del índice {}", ix.name)) } else { Ok(Value::String(v.into())) })?;
        o.insert(k.trim().to_string(), val);
    }
    let mut key = Map::new();
    for c in ix.columns.iter().map(|c| c.trim()).filter(|c| !c.is_empty()) {
        let (field, dir) = match c.rsplit_once(':') {
            Some((f, d)) => {
                let d = d.trim();
                let v = match d.parse::<i64>() {
                    Ok(n @ (1 | -1)) => Value::from(n),
                    _ if matches!(d, "text" | "2dsphere" | "2d" | "hashed") => Value::String(d.into()),
                    _ if d.eq_ignore_ascii_case("desc") => Value::from(-1),
                    _ if d.eq_ignore_ascii_case("asc") => Value::from(1),
                    _ => return Err(Error::Query(format!("índice {}: dirección «{d}» inválida en «{c}»", ix.name))),
                };
                (f.trim().to_string(), v)
            }
            None => (c.to_string(), default_type.clone().unwrap_or(Value::from(1))),
        };
        key.insert(field, dir);
    }
    if key.is_empty() {
        return Err(Error::Query(format!("el índice {} no tiene campos", ix.name)));
    }
    if !ix.name.trim().is_empty() {
        o.insert("name".into(), ix.name.trim().into());
    }
    if ix.unique {
        o.insert("unique".into(), true.into());
    }
    if let Some(f) = ix.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        o.insert("partialFilterExpression".into(), relaxed(f, &format!("filtro del índice {}", ix.name))?);
    }
    // Put `name` first, as people write it.
    let mut ordered = Map::new();
    if let Some(n) = o.remove("name") {
        ordered.insert("name".into(), n);
    }
    ordered.extend(o);
    Ok((key, ordered))
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    let name = t.name.trim();
    if name.is_empty() {
        return Err(Error::Query("falta el nombre de la colección".into()));
    }
    let coll = format!("db.getCollection({})", q(name));
    let mut lines = Vec::new();
    if parts.drop {
        lines.push(format!("{coll}.drop()"));
    }
    let view = t.kind == kinds::VIEW;
    if parts.create {
        if view {
            let source = opt(t, "viewOn").ok_or_else(|| Error::Query(format!("la vista {name} no indica la colección de origen (viewOn)")))?;
            let pipeline = match opt(t, "pipeline") {
                Some(p) => relaxed(p, "pipeline de la vista")?,
                None => json!([]),
            };
            if !pipeline.is_array() {
                return Err(Error::Query("el pipeline de la vista tiene que ser un array de etapas".into()));
            }
            lines.push(format!("db.createView({}, {}, {pipeline})", q(name), q(source)));
        } else {
            let o = collection_options(t)?;
            let mut line = format!("db.createCollection({}", q(name));
            if !o.is_empty() || (parts.if_exists && !parts.drop) {
                line.push_str(&format!(", {}", Value::Object(o)));
            }
            // DBine extension: skip it when it already exists.
            if parts.if_exists && !parts.drop {
                line.push_str(", { ifNotExists: true }");
            }
            line.push(')');
            lines.push(line);
        }
    }
    if parts.indexes && !view {
        for ix in &t.indexes {
            let (key, o) = index_parts(ix)?;
            if o.is_empty() {
                lines.push(format!("{coll}.createIndex({})", Value::Object(key)));
            } else {
                lines.push(format!("{coll}.createIndex({}, {})", Value::Object(key), Value::Object(o)));
            }
        }
    }
    Ok(lines.join("\n"))
}

/// Documents per `insertMany`.
const BATCH: usize = 100;

fn is_object_id(s: &str) -> bool {
    s.len() == 24 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `insertMany` batches. Null cells are left out of the document (a missing
/// field reads back as null); an `_id` that is 24 hex digits becomes an
/// `ObjectId` (that's how the grid shows them). Other values go as JSON.
pub fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    let coll = format!("db.getCollection({})", q(&target.name));
    let mut out = Vec::new();
    for chunk in rows.chunks(BATCH) {
        let docs: Vec<String> = chunk
            .iter()
            .map(|row| {
                let fields: Vec<String> = columns
                    .iter()
                    .zip(row)
                    .filter(|(_, v)| !v.is_null())
                    .map(|(c, v)| {
                        let val = match v {
                            Value::String(s) if c == "_id" && is_object_id(s) => format!("ObjectId(\"{s}\")"),
                            other => other.to_string(),
                        };
                        format!("{}: {val}", q(c))
                    })
                    .collect();
                format!("  {{ {} }}", fields.join(", "))
            })
            .collect();
        out.push(format!("{coll}.insertMany([\n{}\n])", docs.join(",\n")));
    }
    Ok(out.join("\n"))
}

/// A field value as the shell reads it: an `_id` of 24 hex digits is an
/// `ObjectId` (as in [`insert_script`]), everything else goes as JSON.
fn shell_value(field: &str, v: &Value) -> String {
    match v {
        Value::String(s) if field == "_id" && is_object_id(s) => format!("ObjectId(\"{s}\")"),
        other => other.to_string(),
    }
}

fn shell_doc(pairs: &[(String, Value)]) -> String {
    let fields: Vec<String> = pairs.iter().map(|(c, v)| format!("{}: {}", q(c), shell_value(c, v))).collect();
    format!("{{ {} }}", fields.join(", "))
}

/// One `updateOne` per edited document. The filter is the `_id` alone when
/// the key has it, otherwise every key field; null values are `$set` to null.
pub fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    let coll = format!("db.getCollection({})", q(&target.name));
    let mut out = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let filter: Vec<(String, Value)> = match ch.key.iter().find(|(k, _)| k == "_id") {
            Some(id) => vec![id.clone()],
            None => ch.key.clone(),
        };
        out.push(format!("{coll}.updateOne({}, {{ $set: {} }})", shell_doc(&filter), shell_doc(&ch.set)));
    }
    Ok(out.join("\n"))
}

/// One `deleteOne` per document, filtered like [`update_script`]: the `_id`
/// alone when the key has it, otherwise every key field. An empty key is
/// refused, since `deleteOne({})` would remove an arbitrary document.
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let coll = format!("db.getCollection({})", q(&target.name));
    let mut out = Vec::new();
    for key in keys {
        if key.is_empty() {
            return Err(Error::Unsupported("no se puede borrar un documento sin clave: el filtro quedaría vacío".into()));
        }
        let filter: Vec<(String, Value)> = match key.iter().find(|(k, _)| k == "_id") {
            Some(id) => vec![id.clone()],
            None => key.clone(),
        };
        out.push(format!("{coll}.deleteOne({})", shell_doc(&filter)));
    }
    Ok(out.join("\n"))
}

/// The browse query (`db.c.find({}).limit(n)`) restricted by the grid's
/// column filters: a find filter with the query operators, text matches as
/// case-insensitive `$regex` (escaped), and values as the scripts write them
/// (`ObjectId` for an `_id` of 24 hex digits). A null matches a missing
/// field too. SQL conditions don't apply.
pub fn filtered_browse(browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let mut conds: Vec<(String, String)> = Vec::new();
    for f in filters {
        let c = f.column.as_str();
        let v = |x: &Value| shell_value(c, x);
        let first = || f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let regex = |pattern: String| format!("{{ $regex: {}, $options: \"i\" }}", Value::String(pattern));
        let re = || first().map(|x| regex_escape(&x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())));
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
            }
            Ok(format!("[{}]", f.values.iter().map(v).collect::<Vec<_>>().join(", ")))
        };
        let cond = match f.op {
            FilterOp::Eq => v(first()?),
            FilterOp::Ne => format!("{{ $ne: {} }}", v(first()?)),
            FilterOp::Gt => format!("{{ $gt: {} }}", v(first()?)),
            FilterOp::Ge => format!("{{ $gte: {} }}", v(first()?)),
            FilterOp::Lt => format!("{{ $lt: {} }}", v(first()?)),
            FilterOp::Le => format!("{{ $lte: {} }}", v(first()?)),
            FilterOp::Contains => regex(re()?),
            FilterOp::NotContains => format!("{{ $not: {} }}", regex(re()?)),
            FilterOp::StartsWith => regex(format!("^{}", re()?)),
            FilterOp::EndsWith => regex(format!("{}$", re()?)),
            FilterOp::IsNull => "null".into(),
            FilterOp::NotNull => "{ $ne: null }".into(),
            FilterOp::IsEmpty => "\"\"".into(),
            FilterOp::NotEmpty => "{ $nin: [null, \"\"] }".into(),
            FilterOp::In => format!("{{ $in: {} }}", list()?),
            FilterOp::NotIn => format!("{{ $nin: {} }}", list()?),
            FilterOp::IsTrue => "true".into(),
            FilterOp::IsFalse => "false".into(),
            FilterOp::TrueOrNull => "{ $in: [true, null] }".into(),
            FilterOp::FalseOrNull => "{ $in: [false, null] }".into(),
            FilterOp::Sql | FilterOp::SqlRight => {
                return Err(Error::Unsupported("MongoDB no toma condiciones SQL: se filtran en la grilla".into()))
            }
        };
        conds.push((q(c), cond));
    }
    // One field per key; the same field twice goes through $and.
    let mut seen = std::collections::HashSet::new();
    let doc = if conds.iter().all(|(k, _)| seen.insert(k.clone())) {
        format!("{{ {} }}", conds.iter().map(|(k, c)| format!("{k}: {c}")).collect::<Vec<_>>().join(", "))
    } else {
        format!("{{ $and: [{}] }}", conds.iter().map(|(k, c)| format!("{{ {k}: {c} }}")).collect::<Vec<_>>().join(", "))
    };
    let at = browse.find(".find({})").ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))?;
    Ok(format!("{}.find({doc}){}", &browse[..at], &browse[at + ".find({})".len()..]))
}

/// Characters with a meaning in a regular expression, escaped.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ---- reading the schema back ----------------------------------------------

fn ext(b: &Bson) -> Value {
    b.clone().into_relaxed_extjson()
}

fn num_text(b: &Bson) -> Option<String> {
    match b {
        Bson::Int32(n) => Some(n.to_string()),
        Bson::Int64(n) => Some(n.to_string()),
        Bson::Double(n) => Some(if n.fract() == 0.0 { (*n as i64).to_string() } else { n.to_string() }),
        _ => None,
    }
}

/// Index options whose value is the server's default: left out, so that
/// only real differences show.
fn default_index_option(k: &str, v: &Bson) -> bool {
    let n = num_text(v);
    match k {
        "default_language" => v.as_str() == Some("english"),
        "language_override" => v.as_str() == Some("language"),
        "bits" => n.as_deref() == Some("26"),
        "min" => n.as_deref() == Some("-180"),
        "max" => n.as_deref() == Some("180"),
        "sparse" | "hidden" => v.as_bool() == Some(false),
        _ => false,
    }
}

/// A collation as it can be given back to the server (`version` is only
/// reported).
fn collation_text(c: &Document) -> String {
    let mut c = c.clone();
    c.remove("version");
    ext(&Bson::Document(c)).to_string()
}

/// A `listIndexes` entry as the designer's [`IndexDef`] (`None` for `_id_`).
/// A text index has kind `FULLTEXT`: its text fields are plain columns, the
/// other keys carry their direction (`field:1`). Settings go in `options`
/// with the server's names and only when they aren't the default.
pub fn index_def(spec: &Document) -> Option<IndexDef> {
    let name = spec.get_str("name").ok()?.to_string();
    // A clustered collection's index is part of the collection (its
    // `clustered` option), not one `createIndex` can make.
    if name == "_id_" || spec.get_bool("clustered") == Ok(true) {
        return None;
    }
    let key = spec.get_document("key").ok()?;
    let mut columns = Vec::new();
    let mut kind_tokens: Vec<String> = Vec::new();
    let mut options = BTreeMap::new();
    if key.get_str("_fts") == Ok("text") {
        // Text index: the text fields are in `weights`, in the place of
        // `_fts`; the other keys are prefix/suffix fields.
        kind_tokens.push("FULLTEXT".into());
        for (k, v) in key {
            match k.as_str() {
                "_ftsx" => {}
                "_fts" => {
                    if let Ok(w) = spec.get_document("weights") {
                        columns.extend(w.keys().cloned());
                        if w.values().any(|x| num_text(x).as_deref() != Some("1")) {
                            options.insert("weights".to_string(), ext(&Bson::Document(w.clone())).to_string());
                        }
                    }
                }
                _ => columns.push(format!("{k}:{}", if num_text(v).as_deref() == Some("-1") { "-1" } else { "1" })),
            }
        }
        for k in ["default_language", "language_override"] {
            if let Some(v) = spec.get(k).filter(|v| !default_index_option(k, v)).and_then(Bson::as_str) {
                options.insert(k.to_string(), v.to_string());
            }
        }
    } else {
        let strings: Vec<&str> = key.values().filter_map(Bson::as_str).collect();
        let uniform = !strings.is_empty() && strings.len() == key.len() && strings.iter().all(|s| *s == strings[0]);
        if uniform {
            kind_tokens.push(strings[0].to_string());
        }
        for (k, v) in key {
            let c = match (v, num_text(v).as_deref()) {
                (Bson::String(_), _) if uniform => k.clone(),
                (Bson::String(s), _) => format!("{k}:{s}"),
                (_, Some("-1")) => format!("{k}:-1"),
                _ => k.clone(),
            };
            columns.push(c);
        }
    }
    for k in ["expireAfterSeconds", "bits", "min", "max"] {
        if let Some(n) = spec.get(k).filter(|v| !default_index_option(k, v)).and_then(num_text) {
            options.insert(k.to_string(), n);
        }
    }
    for k in ["sparse", "hidden"] {
        if spec.get_bool(k).unwrap_or(false) {
            options.insert(k.to_string(), "true".into());
        }
    }
    if let Ok(c) = spec.get_document("collation") {
        options.insert("collation".into(), collation_text(c));
    }
    if let Ok(p) = spec.get_document("wildcardProjection") {
        options.insert("wildcardProjection".into(), ext(&Bson::Document(p.clone())).to_string());
    }
    let filter = spec.get_document("partialFilterExpression").ok().map(|f| ext(&Bson::Document(f.clone())).to_string());
    Some(IndexDef {
        name,
        columns,
        unique: spec.get_bool("unique").unwrap_or(false),
        kind: (!kind_tokens.is_empty()).then(|| kind_tokens.join(",")),
        filter,
        options,
        ..Default::default()
    })
}

/// A `listCollections` entry (plus sampled fields and indexes) as a
/// [`TableSchema`] whose options regenerate it with [`table_ddl`].
/// Table option: the clustered index's name, when it isn't the default
/// `_id_`.
pub(crate) const CLUSTERED_NAME: &str = "clusteredIndexName";

pub fn table_schema(info: &Document, columns: Vec<ColumnDef>, indexes: &[Document]) -> TableSchema {
    let name = info.get_str("name").unwrap_or_default().to_string();
    let view = info.get_str("type") == Ok("view");
    let empty = Document::new();
    let o = info.get_document("options").unwrap_or(&empty);
    let mut options = BTreeMap::new();
    let mut set = |k: &str, v: String| {
        options.insert(k.to_string(), v);
    };
    let mut columns = columns;
    let mut checks = Vec::new();
    if view {
        if let Ok(src) = o.get_str("viewOn") {
            set("viewOn", src.into());
        }
        if let Some(p) = o.get("pipeline") {
            set("pipeline", ext(p).to_string());
        }
    } else {
        set("validate_fields", "false".into());
        if o.get_bool("capped").unwrap_or(false) {
            set("capped", "true".into());
            for k in ["size", "max"] {
                if let Some(n) = o.get(k).and_then(num_text).filter(|n| n != "0") {
                    set(k, n);
                }
            }
        }
        if let Ok(ts) = o.get_document("timeseries") {
            for k in ["timeField", "metaField", "granularity"] {
                if let Ok(v) = ts.get_str(k) {
                    set(k, v.into());
                }
            }
        }
        if let Ok(ci) = o.get_document("clusteredIndex") {
            set("clustered", "true".into());
            if let Ok(n) = ci.get_str("name").map(str::trim).map(str::to_string) {
                if !n.is_empty() && n != "_id_" {
                    set(CLUSTERED_NAME, n);
                }
            }
        }
        if let Some(n) = o.get("expireAfterSeconds").and_then(num_text) {
            set("expireAfterSeconds", n);
        }
        if let Ok(c) = o.get_document("collation") {
            set("collation", collation_text(c));
        }
        match o.get_document("validator") {
            // The validator is the collection's CHECK (see `validator_check`).
            Ok(v) => {
                let mut w = Map::new();
                w.insert("validator".into(), ext(&Bson::Document(v.clone())));
                for (k, default) in [("validationLevel", "strict"), ("validationAction", "error")] {
                    if let Ok(x) = o.get_str(k) {
                        if x != default {
                            w.insert(k.into(), x.into());
                        }
                    }
                }
                checks.push(CheckDef { name: Some(VALIDATOR.into()), expression: Value::Object(w).to_string() });
                describe_from_validator(v, &mut columns);
            }
            // Without a validator only a level or action that isn't the
            // default counts (dropping a validator sets them back to it).
            Err(_) => {
                for (k, default) in [("validationLevel", "strict"), ("validationAction", "error")] {
                    if let Ok(v) = o.get_str(k).map(str::trim).map(str::to_string) {
                        if v != default {
                            set(k, v);
                        }
                    }
                }
            }
        }
    }
    // On a collection with a default collation, `listIndexes` leaves out
    // the collation of an index that has the simple one: created again
    // without it, the index would take the collection's (other uniqueness
    // and order), and a text index would fail. It's said explicitly.
    let simple_by_default = !view && o.get_document("collation").is_ok_and(|c| c.get_str("locale") != Ok("simple"));
    let indexes: Vec<IndexDef> = indexes
        .iter()
        .filter_map(index_def)
        .map(|mut ix| {
            if simple_by_default && !ix.options.contains_key("collation") {
                ix.options.insert("collation".into(), r#"{"locale":"simple"}"#.into());
            }
            ix
        })
        .collect();
    let has_id = columns.iter().any(|c| c.name == "_id");
    TableSchema {
        kind: if view { kinds::VIEW } else { kinds::COLLECTION }.to_string(),
        schema: None,
        primary_key: (!view && has_id).then(|| KeyDef { name: None, columns: vec!["_id".into()] }),
        columns,
        foreign_keys: Vec::new(),
        indexes,
        checks,
        comment: None,
        options,
        name,
    }
}

/// `db.createView(…)` for a view's `listCollections` options (source,
/// pipeline and collation).
pub fn view_definition(name: &str, o: Option<&Document>) -> String {
    let empty = Document::new();
    let o = o.unwrap_or(&empty);
    let pipeline = o.get("pipeline").map(ext).unwrap_or(json!([]));
    let mut text = format!("db.createView({}, {}, {pipeline}", q(name), q(o.get_str("viewOn").unwrap_or_default()));
    if let Ok(c) = o.get_document("collation") {
        text.push_str(&format!(", {{\"collation\": {}}}", collation_text(c)));
    }
    text.push(')');
    text
}

/// Name of the CHECK that stands for the collection's validator.
pub(crate) const VALIDATOR: &str = "validator";

/// The collection's validator as its CHECK: `{"validator": {…},
/// "validationLevel": …, "validationAction": …}` (level and action only
/// when they aren't `strict` / `error`). A CHECK that is a bare query
/// document is the validator itself. MongoDB has one validator per
/// collection. CHECKs that aren't a document (a SQL condition carried from
/// another engine) aren't a validator and are left out, as before.
pub(crate) fn validator_check(t: &TableSchema) -> Result<Option<Map<String, Value>>> {
    let checks: Vec<_> = t.checks.iter().filter(|c| c.expression.trim_start().starts_with('{')).collect();
    let Some(c) = checks.first() else { return Ok(None) };
    if checks.len() > 1 {
        return Err(Error::Query(format!("MongoDB tiene un solo validador por colección y {} tiene {}", t.name, checks.len())));
    }
    let v = relaxed(&c.expression, "validador")?;
    let Value::Object(m) = v else { return Err(Error::Query("el validador tiene que ser un documento".into())) };
    let wrapped = m.contains_key("validator") && m.keys().all(|k| matches!(k.as_str(), "validator" | "validationLevel" | "validationAction"));
    Ok(Some(if wrapped {
        m
    } else {
        let mut w = Map::new();
        w.insert("validator".into(), Value::Object(m));
        w
    }))
}

/// Fields declared in a `$jsonSchema` validator enrich the sampled ones
/// (description, required) and add those no sampled document has yet.
fn describe_from_validator(v: &Document, columns: &mut Vec<ColumnDef>) {
    let Ok(s) = v.get_document("$jsonSchema") else { return };
    let required: Vec<&str> = s.get_array("required").map(|a| a.iter().filter_map(Bson::as_str).collect()).unwrap_or_default();
    let Ok(props) = s.get_document("properties") else { return };
    for (k, p) in props {
        let p = p.as_document();
        let idx = match columns.iter().position(|c| &c.name == k) {
            Some(i) => i,
            None => {
                let types: Vec<String> = match p.and_then(|p| p.get("bsonType")) {
                    Some(Bson::String(t)) => vec![t.clone()],
                    Some(Bson::Array(a)) => a.iter().filter_map(Bson::as_str).map(str::to_string).collect(),
                    _ => Vec::new(),
                };
                let nullable = types.is_empty() || types.iter().any(|t| t == "null");
                let data_type = types.into_iter().filter(|t| t != "null").collect::<Vec<_>>().join("|");
                columns.push(ColumnDef { name: k.clone(), data_type, nullable, ..Default::default() });
                columns.len() - 1
            }
        };
        let c = &mut columns[idx];
        if let Some(d) = p.and_then(|p| p.get_str("description").ok()) {
            c.comment = Some(d.to_string());
        }
        if required.contains(&k.as_str()) {
            c.options.insert("required".into(), "true".into());
        }
    }
}

/// Database names MongoDB accepts (and that aren't its own).
pub fn check_database_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 {
        return Err(Error::Query("el nombre de la base tiene que tener entre 1 y 63 caracteres".into()));
    }
    if let Some(c) = name.chars().find(|c| "/\\. \"$*<>:|?".contains(*c) || c.is_control()) {
        return Err(Error::Query(format!("el nombre de la base no puede contener «{c}»")));
    }
    Ok(())
}

pub fn is_system_database(name: &str) -> bool {
    matches!(name, "admin" | "local" | "config")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{parse_script, write_reason, Shape};
    use mongodb::bson::doc;

    #[test]
    fn filtered_browse_builds_a_find_filter() {
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        let got = filtered_browse(
            "db.users.find({}).limit(200)",
            &[
                f("_id", FilterOp::In, vec![json!("65a1b2c3d4e5f60718293a00"), json!(7)]),
                f("name", FilterOp::Eq, vec![json!("O'Brien \"Bob\"")]),
                f("email", FilterOp::EndsWith, vec![json!("@x.com")]),
                f("age", FilterOp::Ge, vec![json!(18)]),
                f("deleted", FilterOp::IsNull, vec![]),
            ],
        )
        .unwrap();
        assert_eq!(
            got,
            "db.users.find({ \"_id\": { $in: [ObjectId(\"65a1b2c3d4e5f60718293a00\"), 7] }, \"name\": \"O'Brien \\\"Bob\\\"\", \"email\": { $regex: \"@x\\\\.com$\", $options: \"i\" }, \"age\": { $gte: 18 }, \"deleted\": null }).limit(200)"
        );
        // What it writes, the shell parser reads.
        let st = parse_script(&got).unwrap();
        assert_eq!(st[0].cmd.get_str("find"), Ok("users"));
        let twice = filtered_browse(
            "db.getCollection(\"my-coll\").find({}).limit(5)",
            &[f("n", FilterOp::Gt, vec![json!(1)]), f("n", FilterOp::Lt, vec![json!(9)]), f("s", FilterOp::NotContains, vec![json!("a")])],
        )
        .unwrap();
        assert_eq!(
            twice,
            "db.getCollection(\"my-coll\").find({ $and: [{ \"n\": { $gt: 1 } }, { \"n\": { $lt: 9 } }, { \"s\": { $not: { $regex: \"a\", $options: \"i\" } } }] }).limit(5)"
        );
        assert!(parse_script(&twice).is_ok());
        assert!(matches!(filtered_browse("db.u.find({}).limit(5)", &[ColumnFilter { column: "x".into(), op: FilterOp::Sql, values: vec![], sql: Some("1".into()) }]), Err(Error::Unsupported(_))));
    }

    fn col(name: &str, t: &str, nullable: bool) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: t.into(), nullable, ..Default::default() }
    }

    fn all() -> DdlParts {
        DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: true }
    }

    #[test]
    fn collection_with_validator_options_and_indexes() {
        let mut name = col("name", "string", false);
        name.options.insert("required".into(), "true".into());
        name.comment = Some("Nombre".into());
        let t = TableSchema {
            kind: "collection".into(),
            name: "people".into(),
            columns: vec![col("_id", "objectId", false), name, col("age", "int", true), col("any", "", true)],
            indexes: vec![
                IndexDef { name: "name_age".into(), columns: vec!["name".into(), "age:-1".into()], unique: true, ..Default::default() },
                IndexDef { name: "bio_text".into(), columns: vec!["bio".into()], kind: Some("text".into()), ..Default::default() },
                IndexDef {
                    name: "exp".into(),
                    columns: vec!["at".into()],
                    kind: Some("ttl:3600".into()),
                    filter: Some("{ age: { $gt: 18 }, at: { $gte: ISODate('2024-01-01') } }".into()),
                    ..Default::default()
                },
            ],
            options: [("validationAction", "warn"), ("validationLevel", "moderate")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        };
        let text = table_ddl(&t, all()).unwrap();
        let st = parse_script(&text).unwrap();
        assert_eq!(st.len(), 4, "{text}");
        let c = &st[0].cmd;
        assert_eq!(c.get_str("create"), Ok("people"));
        assert_eq!(c.get_str("validationAction"), Ok("warn"));
        let s = c.get_document("validator").unwrap().get_document("$jsonSchema").unwrap();
        assert_eq!(s.get_array("required").unwrap(), &vec![Bson::String("name".into())]);
        let p = s.get_document("properties").unwrap();
        assert_eq!(p.get_document("name").unwrap(), &doc! { "bsonType": "string", "description": "Nombre" });
        assert_eq!(p.get_document("age").unwrap().get_array("bsonType").unwrap().len(), 2);
        assert!(!p.contains_key("any"));
        let ix = st[1].cmd.get_array("indexes").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(ix.get_document("key").unwrap(), &doc! { "name": 1, "age": -1 });
        assert_eq!(ix.get_str("name"), Ok("name_age"));
        assert_eq!(ix.get_bool("unique"), Ok(true));
        let tx = st[2].cmd.get_array("indexes").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(tx.get_document("key").unwrap(), &doc! { "bio": "text" });
        let ttl = st[3].cmd.get_array("indexes").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(ttl.get_i32("expireAfterSeconds"), Ok(3600));
        let pf = ttl.get_document("partialFilterExpression").unwrap();
        assert!(pf.get_document("at").unwrap().get_datetime("$gte").is_ok());
        // Round trip: listIndexes-like specs map back to the same IndexDefs.
        for (i, st) in st[1..].iter().enumerate() {
            let spec = st.cmd.get_array("indexes").unwrap()[0].as_document().unwrap();
            let back = index_def(spec).unwrap();
            let again = table_ddl(&TableSchema { name: "people".into(), indexes: vec![back], ..Default::default() }, DdlParts { indexes: true, ..Default::default() }).unwrap();
            assert_eq!(parse_script(&again).unwrap()[0].cmd, st.cmd, "index {i}");
        }
    }

    #[test]
    fn drop_if_exists_and_special_collections() {
        let mut t = TableSchema { name: "m".into(), ..Default::default() };
        for (k, v) in [("timeField", "ts"), ("metaField", "sensor"), ("granularity", "hours"), ("expireAfterSeconds", "60")] {
            t.options.insert(k.into(), v.into());
        }
        let text = table_ddl(&t, DdlParts { drop: true, if_exists: true, create: true, ..Default::default() }).unwrap();
        let st = parse_script(&text).unwrap();
        assert_eq!(st[0].cmd, doc! { "drop": "m" });
        assert_eq!(st[1].shape, Shape::Reply);
        assert_eq!(st[1].cmd.get_document("timeseries").unwrap().get_str("granularity"), Ok("hours"));
        // if_exists without drop: the ifNotExists extension.
        let text = table_ddl(&TableSchema { name: "x".into(), ..Default::default() }, DdlParts { if_exists: true, create: true, ..Default::default() }).unwrap();
        assert_eq!(text, "db.createCollection(\"x\", {}, { ifNotExists: true })");
        assert_eq!(parse_script(&text).unwrap()[0].shape, Shape::CreateIfMissing);
        // Capped needs a size; TTL needs time-series or clustered.
        let mut c = TableSchema { name: "c".into(), ..Default::default() };
        c.options.insert("capped".into(), "true".into());
        assert!(table_ddl(&c, all()).is_err());
        c.options.insert("size".into(), "4096".into());
        c.options.insert("clustered".into(), "true".into());
        let st = parse_script(&table_ddl(&c, all()).unwrap()).unwrap();
        assert_eq!(st[0].cmd.get_i32("size"), Ok(4096));
        assert!(st[0].cmd.contains_key("clusteredIndex"));
        let mut bad = TableSchema { name: "b".into(), ..Default::default() };
        bad.options.insert("expireAfterSeconds".into(), "5".into());
        assert!(table_ddl(&bad, all()).is_err());
        let mut badtype = TableSchema { name: "b".into(), columns: vec![col("a", "varchar", true)], ..Default::default() };
        assert!(table_ddl(&badtype, all()).is_err());
        badtype.options.insert("validate_fields".into(), "false".into());
        assert_eq!(table_ddl(&badtype, all()).unwrap(), "db.createCollection(\"b\")");
    }

    #[test]
    fn views_round_trip() {
        let info = doc! { "name": "v", "type": "view", "options": { "viewOn": "people", "pipeline": [{ "$match": { "age": { "$gte": 30 } } }] } };
        let t = table_schema(&info, vec![], &[]);
        assert_eq!(t.kind, "view");
        assert!(t.primary_key.is_none());
        let text = table_ddl(&t, DdlParts { drop: true, create: true, indexes: true, ..Default::default() }).unwrap();
        let st = parse_script(&text).unwrap();
        assert_eq!(st[1].cmd, doc! { "create": "v", "viewOn": "people", "pipeline": [{ "$match": { "age": { "$gte": 30 } } }] });
        // The definition the compare runs makes the same view, collation included.
        let o = doc! { "viewOn": "people", "pipeline": [{ "$match": { "age": { "$gte": 30 } } }], "collation": { "locale": "fr", "version": "57.1" } };
        let st = parse_script(&view_definition("v", Some(&o))).unwrap();
        assert_eq!(st[0].cmd, doc! { "create": "v", "viewOn": "people", "pipeline": [{ "$match": { "age": { "$gte": 30 } } }], "collation": { "locale": "fr" } });
    }

    #[test]
    fn schema_from_list_collections() {
        let info = doc! { "name": "p", "type": "collection", "options": {
            "capped": true, "size": 8192, "validationLevel": "strict",
            "validator": { "$jsonSchema": { "bsonType": "object", "required": ["n"], "properties": { "n": { "bsonType": "string", "description": "d" }, "extra": { "bsonType": ["int", "null"] } } } },
        } };
        let ixs = vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
            doc! { "v": 2, "key": { "_fts": "text", "_ftsx": 1 }, "name": "t", "weights": { "bio": 1 } },
            doc! { "v": 2, "key": { "loc": "2dsphere", "a": -1 }, "name": "g", "sparse": true },
        ];
        let t = table_schema(&info, vec![col("_id", "int", false), col("n", "string", false)], &ixs);
        assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["_id"]);
        assert_eq!(t.options.get("size").map(String::as_str), Some("8192"));
        assert_eq!(t.options.get("validate_fields").map(String::as_str), Some("false"));
        assert_eq!(t.columns[1].comment.as_deref(), Some("d"));
        assert_eq!(t.columns[1].options.get("required").map(String::as_str), Some("true"));
        assert_eq!(t.columns[2].name, "extra");
        assert!(t.columns[2].nullable);
        assert_eq!(t.indexes.len(), 2);
        assert_eq!(t.indexes[0].columns, vec!["bio"]);
        assert_eq!(t.indexes[0].kind.as_deref(), Some("FULLTEXT"));
        assert_eq!(t.indexes[1].columns, vec!["loc:2dsphere", "a:-1"]);
        assert_eq!(t.indexes[1].kind, None);
        assert_eq!(t.indexes[1].options.get("sparse").map(String::as_str), Some("true"));
        // The validator is the CHECK, not an option; `strict` is the default.
        assert!(!t.options.contains_key("validator") && !t.options.contains_key("validationLevel"));
        assert_eq!(t.checks.len(), 1);
        assert_eq!(t.checks[0].name.as_deref(), Some(VALIDATOR));
        assert!(t.checks[0].expression.starts_with("{\"validator\":{\"$jsonSchema\""), "{}", t.checks[0].expression);
        let st = parse_script(&table_ddl(&t, all()).unwrap()).unwrap();
        assert_eq!(st[0].cmd.get_bool("capped"), Ok(true));
        assert!(st[0].cmd.get_document("validator").unwrap().contains_key("$jsonSchema"));
        assert_eq!(st[2].cmd.get_array("indexes").unwrap()[0].as_document().unwrap().get_document("key").unwrap(), &doc! { "loc": "2dsphere", "a": -1 });
    }

    /// "Clonar tabla": on a collection with a default collation, an index
    /// `listIndexes` reports without one has the simple collation, and is
    /// created again with it (a text index can't take the collection's).
    #[test]
    fn simple_collation_indexes_on_a_collated_collection() {
        let info = doc! { "name": "p", "type": "collection", "options": { "collation": { "locale": "es", "version": "57.1" } } };
        let ixs = vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_", "collation": { "locale": "es" } },
            doc! { "v": 2, "key": { "n": 1 }, "name": "ix_es", "unique": true, "collation": { "locale": "es", "version": "57.1" } },
            doc! { "v": 2, "key": { "n": 1, "m": 1 }, "name": "ix_simple" },
            doc! { "v": 2, "key": { "_fts": "text", "_ftsx": 1 }, "name": "t", "weights": { "bio": 1 } },
        ];
        let t = table_schema(&info, vec![col("_id", "int", false)], &ixs);
        let coll = |name: &str| t.indexes.iter().find(|i| i.name == name).unwrap().options.get("collation").cloned();
        assert_eq!(coll("ix_es").as_deref(), Some("{\"locale\":\"es\"}"));
        assert_eq!(coll("ix_simple").as_deref(), Some("{\"locale\":\"simple\"}"));
        assert_eq!(coll("t").as_deref(), Some("{\"locale\":\"simple\"}"));
        let st = parse_script(&table_ddl(&t, all()).unwrap()).unwrap();
        let created: Vec<Document> = st[1..].iter().flat_map(|s| s.cmd.get_array("indexes").unwrap().iter().map(|i| i.as_document().unwrap().clone())).collect();
        for ix in &created {
            let want = if ix.get_str("name") == Ok("ix_es") { "es" } else { "simple" };
            assert_eq!(ix.get_document("collation").unwrap().get_str("locale"), Ok(want), "{ix}");
        }
        // Without a default collation nothing is added.
        let plain = table_schema(&doc! { "name": "p", "type": "collection", "options": {} }, vec![], &ixs[2..3]);
        assert!(!plain.indexes[0].options.contains_key("collation"));
    }

    #[test]
    fn index_options_and_validator_round_trip() {
        let specs = [
            doc! { "v": 2, "key": { "cat": 1, "_fts": "text", "_ftsx": 1, "at": -1 }, "name": "t", "weights": { "bio": 5, "title": 1 },
                   "default_language": "spanish", "language_override": "idioma", "textIndexVersion": 3 },
            doc! { "v": 2, "key": { "_fts": "text", "_ftsx": 1 }, "name": "plain", "weights": { "body": 1 }, "default_language": "english", "language_override": "language" },
            doc! { "v": 2, "key": { "email": 1 }, "name": "e", "unique": true, "hidden": true, "expireAfterSeconds": 60, "sparse": true,
                   "collation": { "locale": "es", "strength": 2, "version": "57.1" }, "partialFilterExpression": { "age": { "$gt": 1 } } },
            doc! { "v": 2, "key": { "$**": 1 }, "name": "w", "wildcardProjection": { "a": 1 } },
            doc! { "v": 2, "key": { "p": "2d" }, "name": "g", "bits": 20, "min": -90, "max": 180 },
            doc! { "v": 2, "key": { "h": "hashed" }, "name": "h" },
        ];
        let defs: Vec<IndexDef> = specs.iter().filter_map(index_def).collect();
        assert_eq!(defs[0].kind.as_deref(), Some("FULLTEXT"));
        assert_eq!(defs[0].columns, vec!["cat:1", "bio", "title", "at:-1"]);
        assert_eq!(defs[0].options.get("default_language").map(String::as_str), Some("spanish"));
        assert_eq!(defs[0].options.get("language_override").map(String::as_str), Some("idioma"));
        assert_eq!(defs[0].options.get("weights").map(String::as_str), Some("{\"bio\":5,\"title\":1}"));
        assert!(defs[1].options.is_empty(), "defaults are left out: {:?}", defs[1].options);
        let e = &defs[2].options;
        assert_eq!((e["hidden"].as_str(), e["sparse"].as_str(), e["expireAfterSeconds"].as_str()), ("true", "true", "60"));
        assert_eq!(e["collation"], "{\"locale\":\"es\",\"strength\":2}");
        assert_eq!(defs[3].columns, vec!["$**"]);
        assert_eq!(defs[4].options.len(), 2, "{:?}", defs[4].options);
        assert_eq!(defs[5].kind.as_deref(), Some("hashed"));
        // What the DDL writes reads back as the same definitions.
        let t = TableSchema { name: "c".into(), indexes: defs.clone(), ..Default::default() };
        let text = table_ddl(&t, DdlParts { indexes: true, ..Default::default() }).unwrap();
        let st = parse_script(&text).unwrap();
        let back: Vec<IndexDef> = st
            .iter()
            .map(|s| {
                let mut spec = s.cmd.get_array("indexes").unwrap()[0].as_document().unwrap().clone();
                // The server reports text indexes by `_fts` and `weights`.
                let key = spec.get_document("key").unwrap().clone();
                if key.values().any(|v| v.as_str() == Some("text")) {
                    let mut k = Document::new();
                    let mut w = spec.get_document("weights").cloned().unwrap_or_default();
                    for (f, v) in &key {
                        if v.as_str() == Some("text") {
                            if !k.contains_key("_fts") {
                                k.insert("_fts", "text");
                                k.insert("_ftsx", 1);
                            }
                            if !w.contains_key(f) {
                                w.insert(f, 1);
                            }
                        } else {
                            k.insert(f, v.clone());
                        }
                    }
                    spec.insert("key", k);
                    spec.insert("weights", w);
                }
                index_def(&spec).unwrap()
            })
            .collect();
        assert_eq!(back, defs);
        // The validator CHECK makes the validator, level and action.
        let mut t = TableSchema { name: "c".into(), ..Default::default() };
        t.options.insert("validator".into(), "{ \"a\": 1 }".into());
        t.checks.push(CheckDef { name: Some(VALIDATOR.into()), expression: "{\"validator\":{\"$jsonSchema\":{\"required\":[\"n\"]}},\"validationLevel\":\"moderate\"}".into() });
        let st = parse_script(&table_ddl(&t, all()).unwrap()).unwrap();
        assert_eq!(st[0].cmd.get_document("validator").unwrap(), &doc! { "$jsonSchema": { "required": ["n"] } });
        assert_eq!(st[0].cmd.get_str("validationLevel"), Ok("moderate"));
        // A bare query document is the validator itself.
        t.checks[0].expression = "{ qty: { $gt: 0 } }".into();
        let st = parse_script(&table_ddl(&t, all()).unwrap()).unwrap();
        assert_eq!(st[0].cmd.get_document("validator").unwrap(), &doc! { "qty": { "$gt": 0 } });
        t.checks.push(CheckDef { name: None, expression: "{}".into() });
        assert!(table_ddl(&t, all()).is_err());
    }

    #[test]
    fn update_script_filters_by_id() {
        let target = ObjectRef { kind: "collection".into(), schema: None, name: "users".into() };
        let changes = vec![
            RowChange {
                key: vec![("_id".into(), json!("65a1b2c3d4e5f60718293a00")), ("name".into(), json!("x"))],
                set: vec![("name".into(), json!("O'Brien \"Bob\"")), ("age".into(), Value::Null)], ..Default::default()
            },
            RowChange { key: vec![("code".into(), json!(7))], set: vec![], ..Default::default() },
            RowChange { key: vec![("code".into(), json!(8))], set: vec![("n".into(), json!(1))], ..Default::default() },
        ];
        let text = update_script(&target, &changes).unwrap();
        assert_eq!(
            text,
            "db.getCollection(\"users\").updateOne({ \"_id\": ObjectId(\"65a1b2c3d4e5f60718293a00\") }, { $set: { \"name\": \"O'Brien \\\"Bob\\\"\", \"age\": null } })\n\
             db.getCollection(\"users\").updateOne({ \"code\": 8 }, { $set: { \"n\": 1 } })"
        );
        let st = parse_script(&text).unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].shape, Shape::Write);
    }

    #[test]
    fn delete_script_filters_by_id() {
        let target = ObjectRef { kind: "collection".into(), schema: None, name: "users".into() };
        let keys = vec![
            vec![("_id".into(), json!("65a1b2c3d4e5f60718293a00")), ("name".into(), json!("x"))],
            vec![("code".into(), json!("O'Brien \"Bob\"")), ("n".into(), json!(2))],
        ];
        let text = delete_script(&target, &keys).unwrap();
        assert_eq!(
            text,
            "db.getCollection(\"users\").deleteOne({ \"_id\": ObjectId(\"65a1b2c3d4e5f60718293a00\") })\n\
             db.getCollection(\"users\").deleteOne({ \"code\": \"O'Brien \\\"Bob\\\"\", \"n\": 2 })"
        );
        let st = parse_script(&text).unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].shape, Shape::Write);
        let del = st[1].cmd.get_array("deletes").unwrap()[0].as_document().unwrap();
        assert_eq!(del.get_document("q").unwrap().get_str("code"), Ok("O'Brien \"Bob\""));
        assert_eq!(del.get_i32("limit"), Ok(1));
        assert!(delete_script(&target, &[vec![]]).is_err());
    }

    #[test]
    fn insert_script_batches_and_parses() {
        let rows: Vec<Vec<Value>> = (0..205)
            .map(|i| vec![json!(format!("65a1b2c3d4e5f60718293a{:02x}", i % 256)), json!(format!("n'\"{i}")), if i % 2 == 0 { Value::Null } else { json!(i) }, json!("{\"a\":1}")])
            .collect();
        let cols = vec!["_id".to_string(), "name".into(), "n".into(), "raw".into()];
        let target = ObjectRef { kind: "collection".into(), schema: None, name: "odd name".into() };
        let text = insert_script(&target, &cols, &rows).unwrap();
        let st = parse_script(&text).unwrap();
        assert_eq!(st.len(), 3);
        assert_eq!(st[0].shape, Shape::Write);
        assert_eq!(st[0].cmd.get_str("insert"), Ok("odd name"));
        let docs = st[0].cmd.get_array("documents").unwrap();
        assert_eq!(docs.len(), 100);
        let d0 = docs[0].as_document().unwrap();
        assert!(d0.get_object_id("_id").is_ok());
        assert_eq!(d0.get_str("name"), Ok("n'\"0"));
        assert!(!d0.contains_key("n"));
        assert_eq!(d0.get_str("raw"), Ok("{\"a\":1}"));
        assert_eq!(docs[1].as_document().unwrap().get_i32("n"), Ok(1));
        assert_eq!(st[2].cmd.get_array("documents").unwrap().len(), 5);
    }

    #[test]
    fn templates_parse_and_writes_are_blocked_when_read_only() {
        for t in templates() {
            let text = t.template.replace("{name}", "obj").replace("{schema}", "");
            let st = parse_script(&text).unwrap_or_else(|e| panic!("{}: {e}", t.label));
            assert_eq!(st.len(), 1, "{}", t.label);
            assert!(write_reason(&st[0].cmd).is_some(), "{}", t.label);
        }
        for w in [
            "db.createCollection('a')",
            "db.createView('v', 'a', [])",
            "db.dropDatabase()",
            "db.a.createIndex({ x: 1 })",
            "db.a.dropIndex('x_1')",
            "db.runCommand({ collMod: 'a', validator: {} })",
        ] {
            assert!(write_reason(&parse_script(w).unwrap()[0].cmd).is_some(), "{w}");
        }
    }

    #[test]
    fn shell_helpers_for_creation() {
        let s = &parse_script("db.a.createIndex({ x: 1, 'y.z': -1 }, { unique: true })").unwrap()[0];
        let ix = s.cmd.get_array("indexes").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(ix.get_str("name"), Ok("x_1_y.z_-1"));
        let s = &parse_script("db.a.createIndexes([{ x: 1 }, { t: 'text' }])").unwrap()[0];
        assert_eq!(s.cmd.get_array("indexes").unwrap()[1].as_document().unwrap().get_str("name"), Ok("t_text"));
        assert_eq!(parse_script("db.a.dropIndexes()").unwrap()[0].cmd, doc! { "dropIndexes": "a", "index": "*" });
        assert_eq!(parse_script("db.dropDatabase()").unwrap()[0].cmd, doc! { "dropDatabase": 1 });
        let v = &parse_script("db.createView('v', 'a', [{ $match: {} }], { collation: { locale: 'es' } })").unwrap()[0];
        assert_eq!(v.cmd.get_str("viewOn"), Ok("a"));
        assert!(v.cmd.contains_key("collation"));
        assert!(parse_script("db.createCollection()").is_err());
    }

    #[test]
    fn database_names() {
        assert!(check_database_name("ventas_2024").is_ok());
        for bad in ["", "a.b", "a b", "a/b", "a$", &"x".repeat(64)] {
            assert!(check_database_name(bad).is_err(), "{bad}");
        }
    }

    /// A clustered collection: its index (with its name) comes back in the
    /// collection's options, never as a `createIndex` (the server refuses
    /// `unique` on an `_id` index).
    #[test]
    fn clustered_index_is_the_collections() {
        let info = doc! { "name": "c", "type": "collection", "options": { "clusteredIndex": { "v": 2, "key": { "_id": 1 }, "name": "cl_id", "unique": true } } };
        let ixs = [
            doc! { "v": 2, "key": { "_id": 1 }, "name": "cl_id", "unique": true, "clustered": true },
            doc! { "v": 2, "key": { "n": 1 }, "name": "n_1" },
        ];
        let t = table_schema(&info, vec![], &ixs);
        assert_eq!(t.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["n_1"]);
        assert_eq!(t.options.get("clustered").map(String::as_str), Some("true"));
        let st = parse_script(&table_ddl(&t, all()).unwrap()).unwrap();
        let ci = st[0].cmd.get_document("clusteredIndex").unwrap();
        assert_eq!(ci.get_str("name"), Ok("cl_id"));
        assert_eq!(st.iter().filter(|s| s.cmd.contains_key("createIndexes")).count(), 1, "only n_1");
        // The default name isn't kept (the server gives it again).
        let dflt = doc! { "name": "d", "type": "collection", "options": { "clusteredIndex": { "v": 2, "key": { "_id": 1 }, "name": "_id_", "unique": true } } };
        assert!(!table_schema(&dflt, vec![], &[]).options.contains_key(CLUSTERED_NAME));
    }
}

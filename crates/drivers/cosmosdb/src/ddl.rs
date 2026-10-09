//! Containers and documents, which Cosmos SQL (queries only) can't create.
//! `execute` recognizes these statements before treating one as a query:
//!
//! ```text
//! CREATE CONTAINER [IF NOT EXISTS] "c" { …REST body… }
//! DROP CONTAINER [IF EXISTS] "c"
//! INSERT INTO "c" { …document… }
//! UPSERT INTO "c" { …document… }
//! UPDATE "c" SET { …fields… } WHERE { "id": …, …more fields… }
//! ```
//!
//! The CREATE body is the REST API's container body without `id`
//! (`partitionKey`, `indexingPolicy`, `uniqueKeyPolicy`, `defaultTtl`…) plus
//! `throughput` (manual RU/s, `x-ms-offer-throughput`) or
//! `autoscaleMaxThroughput` (`x-ms-cosmos-offer-autopilot-settings`), which
//! go as headers. INSERT / UPSERT compute the partition key header from the
//! document and the container's partition key paths. UPDATE reads the one
//! document matching the WHERE fields (all of them equal), merges the SET
//! fields into it and writes it back (upsert guarded by its `_etag`).
//!
//! Also here: the designer, the templates and the scripts DBine writes.

use dbine_driver::{
    kinds, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef, Result, RowChange, TableSchema,
};
use serde_json::{json, Map, Value};

pub const PARTITION_KEY: &str = "partition_key";
pub const THROUGHPUT_MODE: &str = "throughput_mode";
pub const THROUGHPUT: &str = "throughput";
pub const DEFAULT_TTL: &str = "default_ttl";
pub const INDEXING_POLICY: &str = "indexing_policy";

/// Body keys that become request headers instead.
pub const MANUAL_KEY: &str = "throughput";
pub const AUTOSCALE_KEY: &str = "autoscaleMaxThroughput";

/// Index kind for composite indexes (non-unique `IndexDef`s).
pub const COMPOSITE: &str = "composite";

const SYSTEM_PROPS: &[&str] = &["_rid", "_self", "_etag", "_attachments", "_ts"];

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        kind: kinds::COLLECTION,
        label: "Nuevo contenedor",
        data_types: vec!["string", "number", "boolean", "object", "array"],
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        indexes: true,
        foreign_keys: false,
        column_options: Vec::new(),
        table_options: vec![
            Field::new(PARTITION_KEY, "Clave de partición", FieldKind::Text)
                .required()
                .placeholder("/categoria")
                .help("Ruta del campo, como /categoria. Varias rutas separadas por comas (hasta 3) forman una clave jerárquica."),
            Field::new(
                THROUGHPUT_MODE,
                "Rendimiento",
                FieldKind::Select(vec![
                    ("none", "Sin aprovisionar (sin servidor o compartido de la base)"),
                    ("manual", "Manual (RU/s fijas)"),
                    ("autoscale", "Escalado automático (RU/s máximas)"),
                ]),
            )
            .default_value("none"),
            Field::new(THROUGHPUT, "RU/s", FieldKind::Number)
                .default_value("400")
                .help("Manual: desde 400. Escalado automático: el máximo, desde 1000."),
            Field::new(DEFAULT_TTL, "TTL predeterminado (segundos)", FieldKind::Number)
                .help("Vacío = sin TTL. -1 = TTL activado, sin vencimiento salvo que el documento traiga su «ttl»."),
            Field::new(INDEXING_POLICY, "Política de indexación (JSON)", FieldKind::Textarea)
                .placeholder("{ \"indexingMode\": \"consistent\", \"includedPaths\": [{ \"path\": \"/*\" }] }")
                .help("Vacío = la predeterminada (indexa todo)."),
        ],
        columns_required: false,
    }
}

pub fn create_templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::COLLECTION,
            label: "Nuevo contenedor con clave jerárquica",
            template: "CREATE CONTAINER IF NOT EXISTS \"{name}\" {\n  \
                       \"partitionKey\": { \"paths\": [\"/inquilino\", \"/usuario\"], \"kind\": \"MultiHash\", \"version\": 2 },\n  \
                       \"defaultTtl\": -1\n};\n"
                .into(),
        },
        CreateTemplate {
            kind: kinds::COLLECTION,
            label: "Nuevo contenedor con escalado automático",
            template: "CREATE CONTAINER IF NOT EXISTS \"{name}\" {\n  \
                       \"partitionKey\": { \"paths\": [\"/categoria\"], \"kind\": \"Hash\" },\n  \
                       \"uniqueKeyPolicy\": { \"uniqueKeys\": [{ \"paths\": [\"/email\"] }] },\n  \
                       \"autoscaleMaxThroughput\": 1000\n};\n"
                .into(),
        },
    ]
}

pub(crate) fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn opt<'a>(t: &'a TableSchema, key: &str) -> Option<&'a str> {
    t.options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// A field name as a path (`email` → `/email`; paths stay as they are).
pub(crate) fn path(col: &str) -> String {
    let c = col.trim();
    if c.starts_with('/') {
        c.to_string()
    } else {
        format!("/{c}")
    }
}

/// A path as the field name the schema reports (`/a/b` → `a/b`).
pub fn field_of(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

fn partition_key(t: &TableSchema) -> Result<Value> {
    let raw = opt(t, PARTITION_KEY).ok_or_else(|| Error::Query("Falta la clave de partición (por ejemplo /categoria).".into()))?;
    let paths: Vec<String> = raw.split(',').map(str::trim).filter(|p| !p.is_empty()).map(path).collect();
    Ok(match paths.len() {
        1 => json!({ "paths": paths, "kind": "Hash" }),
        2 | 3 => json!({ "paths": paths, "kind": "MultiHash", "version": 2 }),
        _ => return Err(Error::Query("La clave de partición jerárquica admite hasta 3 rutas.".into())),
    })
}

/// A composite index entry: `campo` or `campo DESC`.
fn composite_path(col: &str) -> Value {
    let c = col.trim();
    let (name, order) = match c.rsplit_once(char::is_whitespace) {
        Some((n, o)) if o.eq_ignore_ascii_case("desc") => (n.trim(), "descending"),
        Some((n, o)) if o.eq_ignore_ascii_case("asc") => (n.trim(), "ascending"),
        _ => (c, "ascending"),
    };
    json!({ "path": path(name), "order": order })
}

fn is_composite(i: &IndexDef) -> bool {
    !i.unique && policy_list(i).is_none()
}

/// Index kinds that are entries of the indexing policy's lists, with their
/// list: spatial, full-text and vector indexes (one path each; the entry's
/// other keys, like `types` or `type`, are the index's options).
pub const POLICY_KINDS: &[(&str, &str)] = &[("SPATIAL", "spatialIndexes"), ("FULLTEXT", "fullTextIndexes"), ("vector", "vectorIndexes")];

fn policy_list(i: &IndexDef) -> Option<&'static str> {
    let k = i.kind.as_deref()?.trim();
    POLICY_KINDS.iter().find(|(kind, _)| kind.eq_ignore_ascii_case(k)).map(|(_, list)| *list)
}

/// An indexing policy entry as an index (see [`POLICY_KINDS`]).
pub fn policy_index(kind: &str, n: usize, entry: &Value) -> IndexDef {
    let options = entry
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| *k != "path")
        .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
        .collect();
    IndexDef {
        name: format!("{}_{n}", kind.to_lowercase()),
        columns: vec![field_of(entry["path"].as_str().unwrap_or_default())],
        kind: Some(kind.to_string()),
        options,
        ..Default::default()
    }
}

/// The indexing policy entry of a spatial, full-text or vector index.
fn policy_entry(i: &IndexDef) -> Value {
    let mut e = Map::new();
    e.insert("path".into(), path(i.columns.first().map(String::as_str).unwrap_or_default()).into());
    for (k, v) in &i.options {
        e.insert(k.clone(), serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.clone())));
    }
    Value::Object(e)
}

/// The REST body (without `id`) of the designed container, with the
/// throughput keys the executor turns into headers.
pub fn container_body(t: &TableSchema, indexes: bool) -> Result<Value> {
    let mut body = Map::new();
    body.insert("partitionKey".into(), partition_key(t)?);
    if let Some(ttl) = opt(t, DEFAULT_TTL) {
        let n: i64 = ttl.parse().map_err(|_| Error::Query(format!("El TTL «{ttl}» no es un número entero.")))?;
        body.insert("defaultTtl".into(), n.into());
    }
    let mut policy = match opt(t, INDEXING_POLICY) {
        Some(p) => Some(
            serde_json::from_str::<Value>(p)
                .ok()
                .filter(Value::is_object)
                .ok_or_else(|| Error::Query("La política de indexación no es un objeto JSON válido.".into()))?,
        ),
        None => None,
    };
    if indexes {
        let mut composites = Vec::new();
        let mut uniques = Vec::new();
        let mut lists: Vec<(&str, Value)> = Vec::new();
        for i in &t.indexes {
            if i.columns.is_empty() {
                return Err(Error::Query(format!("El índice {} no tiene campos.", i.name)));
            }
            if let Some(list) = policy_list(i) {
                lists.push((list, policy_entry(i)));
            } else if is_composite(i) {
                if i.columns.len() < 2 {
                    return Err(Error::Query(format!(
                        "El índice {} no es único: en Cosmos DB eso es un índice compuesto, que necesita al menos dos campos \
                         (los campos simples ya se indexan solos).",
                        i.name
                    )));
                }
                composites.push(Value::Array(i.columns.iter().map(|c| composite_path(c)).collect()));
            } else {
                uniques.push(json!({ "paths": i.columns.iter().map(|c| path(c)).collect::<Vec<_>>() }));
            }
        }
        if !composites.is_empty() {
            lists.extend(composites.into_iter().map(|c| ("compositeIndexes", c)));
        }
        if !lists.is_empty() {
            let p = policy.get_or_insert_with(|| {
                json!({ "indexingMode": "consistent", "automatic": true, "includedPaths": [{ "path": "/*" }], "excludedPaths": [{ "path": "/\"_etag\"/?" }] })
            });
            if let Some(o) = p.as_object_mut() {
                for (name, entry) in lists {
                    let list = o.entry(name).or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(a) = list.as_array_mut() {
                        a.push(entry);
                    }
                }
            }
        }
        if !uniques.is_empty() {
            body.insert("uniqueKeyPolicy".into(), json!({ "uniqueKeys": uniques }));
        }
    }
    if let Some(p) = policy {
        body.insert("indexingPolicy".into(), p);
    }
    match opt(t, THROUGHPUT_MODE).unwrap_or("none") {
        "manual" | "autoscale" => {
            let mode = opt(t, THROUGHPUT_MODE).unwrap_or_default();
            let ru = opt(t, THROUGHPUT).unwrap_or(if mode == "manual" { "400" } else { "1000" });
            let n: i64 = ru.parse().ok().filter(|n| *n > 0).ok_or_else(|| Error::Query(format!("Las RU/s «{ru}» no son un número entero positivo.")))?;
            body.insert(if mode == "manual" { MANUAL_KEY } else { AUTOSCALE_KEY }.into(), n.into());
        }
        _ => {}
    }
    Ok(Value::Object(body))
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    if t.name.trim().is_empty() {
        return Err(Error::Query("Falta el nombre del contenedor.".into()));
    }
    let mut stmts = Vec::new();
    if parts.drop {
        stmts.push(format!("DROP CONTAINER {}{};", if parts.if_exists { "IF EXISTS " } else { "" }, q(&t.name)));
    }
    if parts.create {
        let body = container_body(t, parts.indexes)?;
        stmts.push(format!(
            "CREATE CONTAINER {}{} {};",
            if parts.if_exists { "IF NOT EXISTS " } else { "" },
            q(&t.name),
            serde_json::to_string_pretty(&body)?
        ));
    } else if parts.indexes && !t.indexes.is_empty() {
        stmts.push(format!(
            "-- Las claves únicas y los índices compuestos de {} se definen al crear el contenedor (CREATE CONTAINER).",
            crate::comment_text(&t.name)
        ));
    }
    let mut s = stmts.join("\n\n");
    if !s.is_empty() {
        s.push('\n');
    }
    Ok(s)
}

/// One `INSERT INTO "c" {…};` per row. Null cells and system properties
/// are left out; a numeric `id` becomes text (Cosmos DB requires it).
pub fn insert_script(container: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = String::new();
    for row in rows {
        let mut doc = Map::new();
        for (c, v) in columns.iter().zip(row) {
            if v.is_null() || SYSTEM_PROPS.contains(&c.as_str()) {
                continue;
            }
            let v = match (c.as_str(), v) {
                ("id", Value::Number(n)) => Value::String(n.to_string()),
                _ => v.clone(),
            };
            doc.insert(c.clone(), v);
        }
        if doc.is_empty() {
            continue;
        }
        out.push_str(&format!("INSERT INTO {} {};\n", q(container), Value::Object(doc)));
    }
    out
}

/// Fields as a JSON object without system properties, sorted: the same
/// text whether or not serde_json keeps insertion order (a workspace
/// feature can turn it on).
fn script_obj(pairs: &[(String, Value)]) -> Value {
    let mut v: Vec<(String, Value)> = pairs.iter().filter(|(c, _)| !SYSTEM_PROPS.contains(&c.as_str())).cloned().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(v.into_iter().collect())
}

/// A row key as a WHERE object: no system properties, a numeric `id` as
/// text (as Cosmos DB stores it).
fn key_filter(key: &[(String, Value)]) -> Value {
    let mut filter = script_obj(key);
    if let Some(Value::Number(n)) = filter.get("id").cloned() {
        filter["id"] = Value::String(n.to_string());
    }
    filter
}

/// One `UPDATE "c" SET {…} WHERE {…};` per edited document. The WHERE is
/// the key (`id`) without system properties; null values are set to null.
pub fn update_script(container: &str, changes: &[RowChange]) -> String {
    let mut out = String::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        out.push_str(&format!("UPDATE {} SET {} WHERE {};\n", q(container), script_obj(&ch.set), key_filter(&ch.key)));
    }
    out
}

/// One `DELETE FROM "c" WHERE {…};` per document, the WHERE built as in
/// [`update_script`]. A key without `id` can't name a single document.
pub fn delete_script(container: &str, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let mut out = String::new();
    for key in keys {
        let filter = key_filter(key);
        if filter.get("id").is_none_or(Value::is_null) {
            return Err(Error::Unsupported(
                "no se puede borrar un documento de Cosmos DB sin su «id» en la clave".into(),
            ));
        }
        out.push_str(&format!("DELETE FROM {} WHERE {};\n", q(container), filter));
    }
    Ok(out)
}

/// The browse query (`-- container: x` + `SELECT TOP n * FROM c`)
/// restricted by the grid's column filters, in Cosmos DB's SQL:
/// `c["field"]`, JSON literals, the case-insensitive CONTAINS / STARTSWITH
/// / ENDSWITH, and a missing property counts as null. A numeric `id` is
/// compared as text, as Cosmos DB stores it.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, FilterOp};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let mut parts = Vec::new();
    for f in filters {
        let c = format!("c[{}]", Value::String(f.column.clone()));
        let lit = |v: &Value| match v {
            Value::Number(n) if f.column == "id" => Value::String(n.to_string()).to_string(),
            other => other.to_string(),
        };
        let first = || f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let text = || first().map(|v| Value::String(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).to_string());
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
            }
            Ok(f.values.iter().map(lit).collect::<Vec<_>>().join(", "))
        };
        let null = format!("(NOT IS_DEFINED({c}) OR IS_NULL({c}))");
        let sql = || f.sql.as_deref().unwrap_or("").trim().to_string();
        parts.push(match f.op {
            FilterOp::Eq => format!("{c} = {}", lit(first()?)),
            FilterOp::Ne => format!("{c} != {}", lit(first()?)),
            FilterOp::Gt => format!("{c} > {}", lit(first()?)),
            FilterOp::Ge => format!("{c} >= {}", lit(first()?)),
            FilterOp::Lt => format!("{c} < {}", lit(first()?)),
            FilterOp::Le => format!("{c} <= {}", lit(first()?)),
            FilterOp::Contains => format!("CONTAINS({c}, {}, true)", text()?),
            FilterOp::NotContains => format!("NOT CONTAINS({c}, {}, true)", text()?),
            FilterOp::StartsWith => format!("STARTSWITH({c}, {}, true)", text()?),
            FilterOp::EndsWith => format!("ENDSWITH({c}, {}, true)", text()?),
            FilterOp::IsNull => null,
            FilterOp::NotNull => format!("(IS_DEFINED({c}) AND NOT IS_NULL({c}))"),
            FilterOp::IsEmpty => format!("{c} = \"\""),
            FilterOp::NotEmpty => format!("(IS_DEFINED({c}) AND NOT IS_NULL({c}) AND {c} != \"\")"),
            FilterOp::In => format!("{c} IN ({})", list()?),
            FilterOp::NotIn => format!("{c} NOT IN ({})", list()?),
            FilterOp::IsTrue => format!("{c} = true"),
            FilterOp::IsFalse => format!("{c} = false"),
            FilterOp::TrueOrNull => format!("({c} = true OR {null})"),
            FilterOp::FalseOrNull => format!("({c} = false OR {null})"),
            FilterOp::Sql => format!("({})", sql()),
            FilterOp::SqlRight => format!("{c} {}", sql()),
        });
    }
    // The `-- container:` line stays on top; the WHERE goes into the SELECT.
    let (head, select) = match browse.split_once('\n') {
        Some((h, s)) if h.trim_start().starts_with("--") => (format!("{h}\n"), s),
        _ => (String::new(), browse),
    };
    insert_where(select, &parts.join("\n  AND "))
        .map(|s| format!("{head}{s}"))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

/// The `SELECT` that finds the documents an UPDATE's WHERE fields match.
pub fn update_query(filter: &Map<String, Value>) -> String {
    let conds: Vec<String> = filter.iter().map(|(k, v)| format!("c[{}] = {v}", Value::String(k.clone()))).collect();
    format!("SELECT * FROM c WHERE {}", conds.join(" AND "))
}

/// A stored document with an UPDATE's SET fields merged in and the system
/// properties dropped. The `id` can't change (that would be a new document).
pub fn apply_set(mut doc: Value, set: &Map<String, Value>) -> Result<Value> {
    let id = doc.get("id").cloned();
    let m = doc.as_object_mut().ok_or_else(|| Error::Query("El documento leído no es un objeto JSON.".into()))?;
    for k in SYSTEM_PROPS {
        m.remove(*k);
    }
    for (k, v) in set {
        m.insert(k.clone(), v.clone());
    }
    if m.get("id") != id.as_ref() {
        return Err(Error::Query("UPDATE no cambia el «id» de un documento; usá INSERT y borrá el anterior.".into()));
    }
    Ok(doc)
}

// ---- Parsing ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Admin {
    CreateContainer { name: String, if_not_exists: bool, body: Value },
    DropContainer { name: String, if_exists: bool },
    Insert { container: String, upsert: bool, doc: Value },
    Update { container: String, set: Map<String, Value>, filter: Map<String, Value> },
    Delete { container: String, filter: Map<String, Value> },
    /// A database user (resource tokens; see security.rs).
    CreateUser { name: String },
    DropUser { name: String },
    /// `mode`: `All` or `Read`, on a container.
    Grant { mode: String, container: String, user: String },
    Revoke { mode: String, container: String, user: String },
}

const USAGE: &str = "Cosmos DB acepta consultas SELECT, `USE <contenedor>` y estas sentencias de DBine: CREATE CONTAINER [IF NOT EXISTS] \"c\" { …JSON… } · DROP CONTAINER [IF EXISTS] \"c\" · \
                     INSERT INTO \"c\" { …documento… } · UPSERT INTO \"c\" { …documento… } · \
                     UPDATE \"c\" SET { …campos… } WHERE { \"id\": … } · \
                     DELETE FROM \"c\" WHERE { \"id\": … } · CREATE USER \"u\" · DROP USER \"u\" · \
                     GRANT ALL|READ ON \"c\" TO \"u\" · REVOKE ALL|READ ON \"c\" FROM \"u\"";

fn usage() -> Error {
    Error::Query(USAGE.into())
}

struct Cursor<'a> {
    s: &'a str,
}

impl<'a> Cursor<'a> {
    fn keyword(&mut self, kw: &str) -> bool {
        let s = self.s.trim_start();
        let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
        if s[..end].eq_ignore_ascii_case(kw) {
            self.s = &s[end..];
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kw: &str) -> Result<()> {
        if self.keyword(kw) {
            Ok(())
        } else {
            Err(usage())
        }
    }

    fn ident(&mut self) -> Result<String> {
        self.s = self.s.trim_start();
        if let Some(rest) = self.s.strip_prefix('"') {
            let mut name = String::new();
            let mut chars = rest.char_indices().peekable();
            while let Some((i, c)) = chars.next() {
                if c == '"' {
                    if chars.peek().map(|p| p.1) == Some('"') {
                        chars.next();
                        name.push('"');
                    } else {
                        self.s = &rest[i + 1..];
                        return Ok(name);
                    }
                } else {
                    name.push(c);
                }
            }
            return Err(Error::Query("Falta cerrar las comillas del nombre.".into()));
        }
        let end = self.s.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-'))).unwrap_or(self.s.len());
        if end == 0 {
            return Err(usage());
        }
        let name = self.s[..end].to_string();
        self.s = &self.s[end..];
        Ok(name)
    }

    fn body(&mut self) -> Result<Value> {
        let text = self.s.trim();
        if text.is_empty() {
            return Err(Error::Query("Falta el cuerpo JSON después del nombre.".into()));
        }
        let v: Value = serde_json::from_str(text).map_err(|e| Error::Query(format!("El cuerpo no es un JSON válido: {e}")))?;
        if !v.is_object() {
            return Err(Error::Query("El cuerpo tiene que ser un objeto JSON ({ … }).".into()));
        }
        Ok(v)
    }

    /// A JSON object followed by more of the statement.
    fn object(&mut self) -> Result<Map<String, Value>> {
        let s = self.s.trim_start();
        let mut it = serde_json::Deserializer::from_str(s).into_iter::<Value>();
        match it.next() {
            Some(Ok(Value::Object(m))) => {
                self.s = &s[it.byte_offset()..];
                Ok(m)
            }
            Some(Err(e)) => Err(Error::Query(format!("El JSON no es válido: {e}"))),
            _ => Err(Error::Query("Se esperaba un objeto JSON ({ … }).".into())),
        }
    }

    fn end(&self) -> Result<()> {
        if self.s.trim().is_empty() {
            Ok(())
        } else {
            Err(usage())
        }
    }
}

/// The extension a statement is, or `None` for a query.
pub fn parse_admin(stmt: &str) -> Result<Option<Admin>> {
    let mut c = Cursor { s: stmt };
    let grant = c.keyword("grant");
    if grant || c.keyword("revoke") {
        let mode = if c.keyword("all") {
            "All"
        } else if c.keyword("read") {
            "Read"
        } else {
            return Err(Error::Query("Cosmos DB otorga ALL o READ sobre un contenedor.".into()));
        }
        .to_string();
        c.expect("on")?;
        let container = c.ident()?;
        c.expect(if grant { "to" } else { "from" })?;
        let user = c.ident()?;
        c.end()?;
        return Ok(Some(if grant { Admin::Grant { mode, container, user } } else { Admin::Revoke { mode, container, user } }));
    }
    if c.keyword("create") {
        if c.keyword("user") {
            let name = c.ident()?;
            c.end()?;
            return Ok(Some(Admin::CreateUser { name }));
        }
        c.expect("container")?;
        let if_not_exists = c.keyword("if");
        if if_not_exists {
            c.expect("not")?;
            c.expect("exists")?;
        }
        let name = c.ident()?;
        let body = c.body()?;
        return Ok(Some(Admin::CreateContainer { name, if_not_exists, body }));
    }
    if c.keyword("drop") {
        if c.keyword("user") {
            let name = c.ident()?;
            c.end()?;
            return Ok(Some(Admin::DropUser { name }));
        }
        c.expect("container")?;
        let if_exists = c.keyword("if");
        if if_exists {
            c.expect("exists")?;
        }
        let name = c.ident()?;
        c.end()?;
        return Ok(Some(Admin::DropContainer { name, if_exists }));
    }
    if c.keyword("update") {
        let container = c.ident()?;
        c.expect("set")?;
        let set = c.object()?;
        c.expect("where")?;
        let filter = c.object()?;
        c.end()?;
        if set.is_empty() {
            return Err(Error::Query("El SET del UPDATE no tiene campos.".into()));
        }
        if !filter.contains_key("id") {
            return Err(Error::Query("El WHERE del UPDATE tiene que incluir el «id» del documento.".into()));
        }
        return Ok(Some(Admin::Update { container, set, filter }));
    }
    if c.keyword("delete") {
        c.expect("from")?;
        let container = c.ident()?;
        c.expect("where")?;
        let filter = c.object()?;
        c.end()?;
        if !filter.get("id").is_some_and(Value::is_string) {
            return Err(Error::Query("El WHERE del DELETE tiene que incluir el «id» (texto) del documento.".into()));
        }
        return Ok(Some(Admin::Delete { container, filter }));
    }
    let upsert = if c.keyword("insert") {
        false
    } else if c.keyword("upsert") {
        true
    } else {
        return Ok(None);
    };
    c.expect("into")?;
    let container = c.ident()?;
    let doc = c.body()?;
    Ok(Some(Admin::Insert { container, upsert, doc }))
}

/// Splits a script on `;` outside quotes, comments and JSON bodies (where
/// `\"` escapes a quote).
pub fn split_script(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        if let Some(qc) = quote {
            cur.push(c);
            if c == '\\' && qc == '"' && depth > 0 {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            } else if c == qc {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '{' | '[' | '(' => {
                depth += 1;
                cur.push(c);
            }
            '}' | ']' | ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        cur.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                cur.push(' ');
            }
            ';' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// The partition key header value of a document: its values at the
/// container's paths, as a JSON array (`{}` for a missing one, which
/// Cosmos DB stores as "undefined").
pub fn partition_key_header(doc: &Value, paths: &[String]) -> String {
    let values: Vec<Value> = paths
        .iter()
        .map(|p| {
            p.trim_start_matches('/')
                .split('/')
                .try_fold(doc, |v, seg| v.get(seg.trim_matches('"')))
                .cloned()
                .unwrap_or_else(|| json!({}))
        })
        .collect();
    Value::Array(values).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn filtered_browse_in_cosmos_sql() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "-- container: from orders\nSELECT TOP 200 * FROM c",
                &[
                    f("name", FilterOp::Eq, vec![json!("O'Brien \"Jr\"")]),
                    f("id", FilterOp::In, vec![json!(1), json!("b")]),
                    f("total", FilterOp::Gt, vec![json!(10.5)]),
                    f("note", FilterOp::Contains, vec![json!("50%")]),
                    f("gone", FilterOp::IsNull, vec![]),
                ]
            )
            .unwrap(),
            "-- container: from orders\nSELECT TOP 200 * FROM c\nWHERE c[\"name\"] = \"O'Brien \\\"Jr\\\"\"\n  AND c[\"id\"] IN (\"1\", \"b\")\n  AND c[\"total\"] > 10.5\n  AND CONTAINS(c[\"note\"], \"50%\", true)\n  AND (NOT IS_DEFINED(c[\"gone\"]) OR IS_NULL(c[\"gone\"]))"
        );
    }

    fn items() -> TableSchema {
        TableSchema {
            kind: kinds::COLLECTION.into(),
            name: "items".into(),
            indexes: vec![
                IndexDef { name: "uq".into(), columns: vec!["email".into(), "/tenant".into()], unique: true, ..Default::default() },
                IndexDef { name: "byNameAge".into(), columns: vec!["name".into(), "age DESC".into()], kind: Some(COMPOSITE.into()), ..Default::default() },
            ],
            options: BTreeMap::from([
                (PARTITION_KEY.to_string(), "/cat".to_string()),
                (THROUGHPUT_MODE.to_string(), "autoscale".to_string()),
                (THROUGHPUT.to_string(), "4000".to_string()),
                (DEFAULT_TTL.to_string(), "3600".to_string()),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn container_ddl_parses_back() {
        let ddl = table_ddl(&items(), DdlParts { drop: true, if_exists: true, create: true, indexes: true, ..Default::default() }).unwrap();
        let stmts = split_script(&ddl);
        assert_eq!(stmts.len(), 2, "{ddl}");
        assert_eq!(parse_admin(&stmts[0]).unwrap(), Some(Admin::DropContainer { name: "items".into(), if_exists: true }));
        let Some(Admin::CreateContainer { name, if_not_exists, body }) = parse_admin(&stmts[1]).unwrap() else { panic!("{ddl}") };
        assert_eq!(name, "items");
        assert!(if_not_exists);
        assert_eq!(body["partitionKey"], json!({ "paths": ["/cat"], "kind": "Hash" }));
        assert_eq!(body["defaultTtl"], 3600);
        assert_eq!(body[AUTOSCALE_KEY], 4000);
        assert_eq!(body["uniqueKeyPolicy"], json!({ "uniqueKeys": [{ "paths": ["/email", "/tenant"] }] }));
        assert_eq!(
            body["indexingPolicy"]["compositeIndexes"],
            json!([[{ "path": "/name", "order": "ascending" }, { "path": "/age", "order": "descending" }]])
        );
    }

    #[test]
    fn options_and_errors() {
        let mut t = items();
        t.indexes.clear();
        t.options.insert(PARTITION_KEY.into(), "tenant, /user".into());
        t.options.insert(THROUGHPUT_MODE.into(), "manual".into());
        t.options.remove(THROUGHPUT);
        t.options.insert(INDEXING_POLICY.into(), "{\"indexingMode\": \"none\", \"automatic\": false}".into());
        let b = container_body(&t, true).unwrap();
        assert_eq!(b["partitionKey"], json!({ "paths": ["/tenant", "/user"], "kind": "MultiHash", "version": 2 }));
        assert_eq!(b[MANUAL_KEY], 400);
        assert_eq!(b["indexingPolicy"]["indexingMode"], "none");
        assert!(b.get("uniqueKeyPolicy").is_none());

        t.options.insert(INDEXING_POLICY.into(), "nope".into());
        assert!(container_body(&t, true).is_err());
        t.options.remove(INDEXING_POLICY);
        t.indexes.push(IndexDef { name: "one".into(), columns: vec!["a".into()], ..Default::default() });
        assert!(container_body(&t, true).unwrap_err().to_string().contains("compuesto"));
        t.options.remove(PARTITION_KEY);
        assert!(container_body(&t, false).is_err());
    }

    #[test]
    fn statements() {
        assert_eq!(parse_admin("SELECT * FROM c").unwrap(), None);
        assert_eq!(parse_admin("drop container \"a\"\"b\"").unwrap(), Some(Admin::DropContainer { name: "a\"b".into(), if_exists: false }));
        assert_eq!(
            parse_admin("UPSERT INTO items {\"id\": \"1\"}").unwrap(),
            Some(Admin::Insert { container: "items".into(), upsert: true, doc: json!({ "id": "1" }) })
        );
        assert!(parse_admin("CREATE CONTAINER x").is_err());
        assert!(parse_admin("CREATE TABLE x {}").is_err());
        assert!(parse_admin("INSERT INTO x [1]").is_err());
        assert!(parse_admin("DROP CONTAINER x y").is_err());
    }

    #[test]
    fn inserts_are_documents() {
        let cols = vec!["id".to_string(), "name".into(), "addr".into(), "gone".into(), "_ts".into()];
        let rows = vec![vec![json!(7), json!("it's \"q\"; x"), json!({ "city": "Rosario" }), Value::Null, json!(1)]];
        let s = insert_script("it\"ems", &cols, &rows);
        let stmts = split_script(&s);
        assert_eq!(stmts.len(), 1, "{s}");
        assert_eq!(
            parse_admin(&stmts[0]).unwrap(),
            Some(Admin::Insert {
                container: "it\"ems".into(),
                upsert: false,
                doc: json!({ "id": "7", "name": "it's \"q\"; x", "addr": { "city": "Rosario" } })
            })
        );
    }

    #[test]
    fn updates_parse_back() {
        let changes = vec![
            RowChange {
                key: vec![("id".into(), json!(7)), ("_etag".into(), json!("e"))],
                set: vec![("name".into(), json!("it's \"q\"; x")), ("gone".into(), Value::Null)], ..Default::default()
            },
            RowChange { key: vec![("id".into(), json!("8"))], set: vec![], ..Default::default() },
        ];
        let s = update_script("it\"ems", &changes);
        assert_eq!(s, "UPDATE \"it\"\"ems\" SET {\"gone\":null,\"name\":\"it's \\\"q\\\"; x\"} WHERE {\"id\":\"7\"};\n");
        let stmts = split_script(&s);
        assert_eq!(stmts.len(), 1, "{s}");
        let Some(Admin::Update { container, set, filter }) = parse_admin(&stmts[0]).unwrap() else { panic!("{s}") };
        assert_eq!(container, "it\"ems");
        assert_eq!(Value::Object(set.clone()), json!({ "name": "it's \"q\"; x", "gone": null }));
        assert_eq!(update_query(&filter), "SELECT * FROM c WHERE c[\"id\"] = \"7\"");
        let doc = apply_set(json!({ "id": "7", "name": "a", "k": 1, "_etag": "e", "_ts": 1 }), &set).unwrap();
        assert_eq!(doc, json!({ "id": "7", "name": "it's \"q\"; x", "gone": null, "k": 1 }));
        assert!(apply_set(json!({ "id": "7" }), &Map::from_iter([("id".to_string(), json!("9"))])).is_err());
        assert!(parse_admin("UPDATE c SET {\"a\": 1} WHERE {\"b\": 2}").is_err());
        assert!(parse_admin("UPDATE c SET {} WHERE {\"id\": \"1\"}").is_err());
    }

    #[test]
    fn deletes_parse_back() {
        let keys = vec![
            vec![("id".into(), json!(7)), ("_etag".into(), json!("e"))],
            vec![("id".into(), json!("it's \"q\"; x")), ("cat".into(), json!("a"))],
        ];
        let s = delete_script("it\"ems", &keys).unwrap();
        assert_eq!(
            s,
            "DELETE FROM \"it\"\"ems\" WHERE {\"id\":\"7\"};\n\
             DELETE FROM \"it\"\"ems\" WHERE {\"cat\":\"a\",\"id\":\"it's \\\"q\\\"; x\"};\n"
        );
        let stmts = split_script(&s);
        assert_eq!(stmts.len(), 2, "{s}");
        let Some(Admin::Delete { container, filter }) = parse_admin(&stmts[1]).unwrap() else { panic!("{s}") };
        assert_eq!(container, "it\"ems");
        assert_eq!(Value::Object(filter), json!({ "id": "it's \"q\"; x", "cat": "a" }));
        assert!(delete_script("c", &[vec![("name".into(), json!("x"))]]).is_err());
        assert!(delete_script("c", &[vec![]]).is_err());
        assert!(parse_admin("DELETE FROM c WHERE {\"b\": 2}").is_err());
        assert!(parse_admin("DELETE FROM c WHERE {}").is_err());
        assert!(parse_admin("DELETE FROM c").is_err());
    }

    #[test]
    fn partition_key_values() {
        let doc = json!({ "cat": "a", "t": { "u": 3 } });
        assert_eq!(partition_key_header(&doc, &["/cat".into()]), "[\"a\"]");
        assert_eq!(partition_key_header(&doc, &["/cat".into(), "/t/u".into()]), "[\"a\",3]");
        assert_eq!(partition_key_header(&doc, &["/x".into()]), "[{}]");
    }

    #[test]
    fn templates_parse() {
        for t in create_templates() {
            let stmts = split_script(&t.template.replace("{name}", "x"));
            assert_eq!(stmts.len(), 1);
            assert!(matches!(parse_admin(&stmts[0]).unwrap(), Some(Admin::CreateContainer { .. })));
        }
    }
}

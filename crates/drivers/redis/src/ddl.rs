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
//! | `vectorset` | element    | its vector, a JSON array (one `VADD` each) |
//!
//! `ttl` (seconds) adds an `EXPIRE`. Dropping is `DEL`; `IF EXISTS` doesn't
//! apply (DEL of a missing key is a no-op).
//!
//! A hash field whose type is a vector (`VECTOR(3, FLOAT32)`) holds the
//! packed blob of that type: its value (a JSON array) is written as that
//! blob. The designer's indexes are search indexes (`FT.CREATE`) over the
//! keys that start with `index_prefix` (by default the key's name up to its
//! last `:`): a vector index on its vector column, with the other columns
//! as filter fields (`TAG`, `TEXT`, `NUMERIC`… by their type).
//!
//! A search index is also an object of its own (kind `index`, see
//! [`index_schema`]): its fields are its columns, and its DDL is its
//! `FT.CREATE` / `FT.DROPINDEX`.

use crate::command::quote_arg;
use crate::vectors::{self, FtField, FtIndex, FtVector, Known, Server, VType};
use dbine_driver::vector::metric;
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef,
    ObjectRef, Result, RowChange, TableSchema, VectorIndex, VectorIndexSpec,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Types the designer suggests: a plain field, the search field types an
/// index gives it, and the vectors the server's indexes take.
fn data_types(server: Server) -> Vec<&'static str> {
    let mut v = vec!["string", "TEXT", "TAG", "NUMERIC"];
    if server != Server::Valkey {
        v.push("GEO");
    }
    for t in server.types() {
        v.push(match t {
            VType::F32 => "VECTOR(3, FLOAT32)",
            VType::F64 => "VECTOR(3, FLOAT64)",
            VType::F16 => "VECTOR(3, FLOAT16)",
            VType::Bf16 => "VECTOR(3, BFLOAT16)",
            VType::I8 => "VECTOR(3, INT8)",
            VType::U8 => "VECTOR(3, UINT8)",
        });
    }
    v
}

pub fn designer(driver_id: &str) -> DesignerSpec {
    let server = Server::of(driver_id);
    let json_label = if driver_id == "dragonfly" {
        "JSON"
    } else {
        "JSON (requiere el módulo JSON)"
    };
    let mut types = vec![
        ("string", "String"),
        ("hash", "Hash"),
        ("list", "List"),
        ("set", "Set"),
        ("zset", "Sorted set"),
        ("stream", "Stream"),
        ("json", json_label),
    ];
    let mut table_options = vec![
        Field::new("type", "Tipo", FieldKind::Select(Vec::new()))
            .required()
            .default_value("string")
            .help(if server == Server::Redis {
                "Las columnas son las entradas: hash y stream = campo (nombre) y valor (valor por defecto); \
                 list y set = elemento; sorted set = miembro (nombre) y score (valor por defecto); \
                 vector set = elemento (nombre) y su vector (valor por defecto, p. ej. [0.1, 0.2, 0.3])."
            } else {
                "Las columnas son las entradas: hash y stream = campo (nombre) y valor (valor por defecto); \
                 list y set = elemento; sorted set = miembro (nombre) y score (valor por defecto)."
            }),
        Field::new("ttl", "TTL (segundos)", FieldKind::Number)
            .placeholder("(sin vencimiento)")
            .help("Agrega un EXPIRE después de crear la key."),
        Field::new("value", "Valor", FieldKind::Textarea)
            .help("Solo para string y JSON (un documento JSON, p. ej. {\"a\": 1})."),
        Field::new("index_prefix", "Prefijo de los índices", FieldKind::Text)
            .placeholder("(el nombre de la key hasta su último «:»)")
            .help("Los índices de búsqueda (FT.CREATE … PREFIX) cubren las keys que empiezan así: hashes o documentos JSON."),
    ];
    if server == Server::Redis {
        types.push(("vectorset", "Vector set (Redis 8)"));
        table_options.extend([
            Field::new(
                "quantization",
                "Cuantización del vector set",
                FieldKind::Select(vec![
                    ("Q8", "Q8: enteros de 8 bits (por defecto)"),
                    ("NOQUANT", "Sin cuantizar (float32)"),
                    ("BIN", "Binaria"),
                ]),
            )
            .help("Solo para vector set: cómo guarda Redis cada vector (VADD … Q8 | NOQUANT | BIN)."),
            Field::new("m", "M del vector set", FieldKind::Number)
                .placeholder("16")
                .help("Solo para vector set: vecinos por nodo del grafo HNSW (VADD … M)."),
            Field::new("ef", "EF de construcción del vector set", FieldKind::Number)
                .placeholder("200")
                .help("Solo para vector set: candidatos al construir el grafo (VADD … EF)."),
            Field::new("reduce", "Reducir a (dimensiones)", FieldKind::Number)
                .placeholder("(sin reducir)")
                .help("Solo para vector set: proyecta cada vector a menos dimensiones al guardarlo (VADD … REDUCE)."),
        ]);
    }
    table_options[0].kind = FieldKind::Select(types);
    DesignerSpec {
        kind: kinds::KEY,
        label: "Nueva key",
        data_types: data_types(server),
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: true,
        nullability: false,
        comments: false,
        indexes: true,
        foreign_keys: false,
        column_options: Vec::new(),
        table_options,
        columns_required: false,
        vector_indexes: Some(VectorIndexSpec {
            algorithms: server.algorithms(),
            metrics: vec![metric::COSINE, metric::L2, metric::IP],
            params: vectors::designer_params(server),
            filter_columns: true,
        }),
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    let t = |label: &'static str, template: &str| CreateTemplate {
        kind: kinds::KEY,
        label,
        template: template.into(),
    };
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
    t.options
        .get(key)
        .map(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
}

fn line(args: &[&str]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`line`] of owned arguments.
pub(crate) fn line_of(args: &[String]) -> String {
    line(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// An argument of a command line: text (quoted as the editor reads it) or
/// a blob (`"\x00\x00\x80?"`).
#[derive(Debug, Clone)]
enum Arg {
    Text(String),
    Blob(Vec<u8>),
}

impl From<&str> for Arg {
    fn from(s: &str) -> Arg {
        Arg::Text(s.to_string())
    }
}

impl From<String> for Arg {
    fn from(s: String) -> Arg {
        Arg::Text(s)
    }
}

/// A command line of [`Arg`]s, with a trailing `# comment` if any.
fn blob_line(args: &[Arg], comment: Option<&str>) -> String {
    let mut s = args
        .iter()
        .map(|a| match a {
            Arg::Text(t) => quote_arg(t),
            Arg::Blob(b) => vectors::quote_bytes(b),
        })
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(c) = comment.filter(|c| !c.is_empty()) {
        s.push_str(" # ");
        s.push_str(&c.replace(['\n', '\r'], " "));
    }
    s
}

/// A vector value as the blob of `t`, with what the comment says of it.
fn vector_arg(v: &Value, t: VType, field: &str) -> Option<(Arg, String)> {
    let nums = dbine_driver::vector::parse_value(v)?;
    let text = dbine_driver::vector::text(nums.iter().copied());
    Some((
        Arg::Blob(t.encode(&nums)),
        format!("{field}: {} {text}", t.name()),
    ))
}

/// The prefix a key designer's indexes cover: the `index_prefix` option, or
/// the key's name up to its last `:` (the whole name when it has none).
fn index_prefix(t: &TableSchema) -> String {
    if let Some(p) = opt(t, "index_prefix") {
        return p.trim().to_string();
    }
    match t.name.rfind(':') {
        Some(i) => t.name[..=i].to_string(),
        None => t.name.clone(),
    }
}

/// A search field's type from a column's: `TEXT`, `TAG`, `NUMERIC`,
/// `GEO`, `GEOSHAPE`; anything else (`string`, a number type…) a `TAG`
/// or a `NUMERIC`.
fn field_kind(data_type: &str) -> &'static str {
    let t = data_type.trim().to_ascii_uppercase();
    match t.as_str() {
        "TEXT" => "TEXT",
        "NUMERIC" => "NUMERIC",
        "GEO" => "GEO",
        "GEOSHAPE" => "GEOSHAPE",
        "TAG" => "TAG",
        _ if [
            "INT", "BIGINT", "SMALLINT", "DOUBLE", "FLOAT", "DECIMAL", "NUMBER", "REAL",
        ]
        .iter()
        .any(|p| t.starts_with(p)) =>
        {
            "NUMERIC"
        }
        _ => "TAG",
    }
}

/// A vector field from a column of type `VECTOR(n, TYPE)` and, for an
/// index's vector column, its search settings.
fn vector_field(
    c: &ColumnDef,
    v: Option<&VectorIndex>,
    server: Server,
) -> Result<Option<FtVector>> {
    let Some((dims, ty)) = vectors::parse_spelling(&c.data_type) else {
        return Ok(None);
    };
    let Some(dims) = dims else {
        return Err(Error::Query(format!(
            "Indicá la dimensión del vector de {}: p. ej. VECTOR(3, FLOAT32).",
            c.name
        )));
    };
    if !server.types().contains(&ty) {
        return Err(Error::Query(format!(
            "Este servidor no indexa vectores {}: solo {}.",
            ty.name(),
            server
                .types()
                .iter()
                .map(|t| t.name())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let f = FtVector::from_index(v.unwrap_or(&VectorIndex::default()), ty, dims)
        .map_err(Error::Query)?;
    if !server
        .algorithms()
        .contains(&f.algorithm.to_ascii_lowercase().as_str())
    {
        return Err(Error::Query(format!(
            "Este servidor no tiene el algoritmo {} para índices vectoriales.",
            f.algorithm
        )));
    }
    Ok(Some(f))
}

/// The search index of a key designer's index row: over the keys of its
/// prefix, of hashes or JSON documents, its columns as fields (the vector
/// one first, with the row's search settings).
fn key_index(t: &TableSchema, ix: &IndexDef, json: bool, server: Server) -> Result<FtIndex> {
    if ix.name.trim().is_empty() {
        return Err(Error::Query("El índice necesita un nombre.".into()));
    }
    if ix.columns.is_empty() {
        return Err(Error::Query(format!(
            "El índice {} necesita al menos una columna.",
            ix.name
        )));
    }
    let mut fields = Vec::new();
    for (i, name) in ix.columns.iter().enumerate() {
        let c = t.columns.iter().find(|c| &c.name == name).ok_or_else(|| {
            Error::Query(format!(
                "El índice {} usa la columna {name}, que no existe.",
                ix.name
            ))
        })?;
        let settings = if i == 0 { ix.vector.as_ref() } else { None };
        let vector = vector_field(c, settings, server)?;
        if i == 0 && ix.vector.is_some() && vector.is_none() {
            return Err(Error::Query(format!(
                "La primera columna del índice vectorial {} tiene que ser un vector: p. ej. VECTOR(3, FLOAT32).",
                ix.name
            )));
        }
        let kind = if vector.is_some() {
            "VECTOR"
        } else {
            field_kind(&c.data_type)
        };
        fields.push(FtField {
            identifier: if json {
                format!("$.{name}")
            } else {
                name.clone()
            },
            attribute: name.clone(),
            kind: kind.into(),
            vector,
            args: Vec::new(),
        });
    }
    Ok(FtIndex {
        name: ix.name.trim().to_string(),
        on_json: json,
        prefixes: vec![index_prefix(t)],
        filter: ix.filter.clone().filter(|f| !f.trim().is_empty()),
        fields,
    })
}

/// A search index as an object of its own: its fields as columns (with
/// their `FT.CREATE` arguments in the column options), each vector field
/// with its search settings as an index, and where it searches (`on`,
/// `prefix`, `filter`) in the options.
pub fn index_schema(ix: &FtIndex) -> TableSchema {
    let mut options = BTreeMap::new();
    options.insert(
        "on".to_string(),
        if ix.on_json { "JSON" } else { "HASH" }.to_string(),
    );
    let prefixes: Vec<&str> = ix
        .prefixes
        .iter()
        .map(String::as_str)
        .filter(|p| !p.is_empty())
        .collect();
    if !prefixes.is_empty() {
        options.insert("prefix".into(), prefixes.join("\n"));
    }
    if let Some(f) = &ix.filter {
        options.insert("filter".into(), f.clone());
    }
    let columns = ix
        .fields
        .iter()
        .map(|f| {
            let mut o = BTreeMap::new();
            let default_id = if ix.on_json {
                format!("$.{}", f.attribute)
            } else {
                f.attribute.clone()
            };
            if f.identifier != default_id {
                o.insert("identifier".to_string(), f.identifier.clone());
            }
            if !f.args.is_empty() {
                o.insert("args".to_string(), f.args.join(" "));
            }
            ColumnDef {
                name: f.attribute.clone(),
                data_type: f.data_type(),
                nullable: true,
                options: o,
                ..Default::default()
            }
        })
        .collect();
    let indexes = ix
        .fields
        .iter()
        .filter_map(|f| {
            let v = f.vector.as_ref()?;
            Some(IndexDef {
                name: f.attribute.clone(),
                columns: vec![f.attribute.clone()],
                kind: Some(v.algorithm.to_ascii_lowercase()),
                vector: Some(v.index()),
                ..Default::default()
            })
        })
        .collect();
    TableSchema {
        kind: kinds::INDEX.into(),
        name: ix.name.clone(),
        columns,
        indexes,
        options,
        ..Default::default()
    }
}

/// [`index_schema`] back into the index (what its `FT.CREATE` makes).
pub fn schema_index(t: &TableSchema, server: Server) -> Result<FtIndex> {
    let json = opt(t, "on").is_some_and(|o| o.trim().eq_ignore_ascii_case("json"));
    let prefixes = opt(t, "prefix")
        .map(|p| {
            p.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut fields = Vec::new();
    for c in &t.columns {
        let settings = t
            .indexes
            .iter()
            .find(|i| i.columns.first() == Some(&c.name) && i.vector.is_some())
            .and_then(|i| i.vector.as_ref());
        let vector = vector_field(c, settings, server)?;
        let identifier = c.options.get("identifier").cloned().unwrap_or_else(|| {
            if json {
                format!("$.{}", c.name)
            } else {
                c.name.clone()
            }
        });
        fields.push(FtField {
            identifier,
            attribute: c.name.clone(),
            kind: if vector.is_some() {
                "VECTOR".into()
            } else {
                field_kind(&c.data_type).into()
            },
            vector,
            args: c
                .options
                .get("args")
                .map(|a| a.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
        });
    }
    if fields.is_empty() {
        return Err(Error::Query(format!(
            "El índice {} necesita al menos un campo.",
            t.name
        )));
    }
    Ok(FtIndex {
        name: t.name.clone(),
        on_json: json,
        prefixes,
        filter: opt(t, "filter").map(str::to_string),
        fields,
    })
}

/// A search index's DDL: `FT.DROPINDEX` and `FT.CREATE`.
fn search_index_ddl(t: &TableSchema, parts: DdlParts, server: Server) -> Result<String> {
    if t.name.trim().is_empty() {
        return Err(Error::Query("El índice necesita un nombre.".into()));
    }
    let mut out = Vec::new();
    if parts.drop {
        out.push(line(&["FT.DROPINDEX", &t.name]));
    }
    if parts.create {
        out.push(schema_index(t, server)?.create_command());
    }
    Ok(if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    })
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts, server: Server) -> Result<String> {
    if t.kind == kinds::INDEX {
        return search_index_ddl(t, parts, server);
    }
    if t.name.is_empty() {
        return Err(Error::Query("La key necesita un nombre.".into()));
    }
    let key = t.name.as_str();
    let mut out: Vec<String> = Vec::new();
    let ty = opt(t, "type")
        .unwrap_or("string")
        .trim()
        .to_ascii_lowercase();
    if parts.drop {
        out.push(line(&["DEL", key]));
    }
    if parts.create {
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
        let element = |c: &dbine_driver::ColumnDef| {
            c.default_value
                .clone()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| c.name.clone())
        };
        let mut args: Vec<String> = Vec::new();
        match ty.as_str() {
            "string" => args.extend(["SET".into(), key.into(), value.into()]),
            "hash" => {
                entries("hash")?;
                // A vector field's value is the blob of its type.
                let mut a: Vec<Arg> = vec!["HSET".into(), key.into()];
                let mut notes = Vec::new();
                for c in &t.columns {
                    let vector = vectors::parse_spelling(&c.data_type);
                    match (vector, c.default_value.as_deref().map(str::trim)) {
                        (Some(_), None | Some("")) => continue,
                        (Some((dims, vt)), Some(v)) => {
                            let (arg, note) = vector_arg(&Value::String(v.into()), vt, &c.name)
                                .ok_or_else(|| {
                                    Error::Query(format!(
                                        "El valor de {} no es un vector: escribilo como [0.1, 0.2, 0.3].",
                                        c.name
                                    ))
                                })?;
                            let n = dbine_driver::vector::parse(v).map_or(0, |x| x.len());
                            if dims.is_some_and(|d| d as usize != n) {
                                return Err(Error::Query(format!(
                                    "El vector de {} tiene {n} dimensiones y su tipo, {}.",
                                    c.name,
                                    dims.unwrap_or_default()
                                )));
                            }
                            a.push(c.name.clone().into());
                            a.push(arg);
                            notes.push(note);
                        }
                        (None, _) => {
                            a.push(c.name.clone().into());
                            a.push(default(c).into());
                        }
                    }
                }
                if a.len() == 2 {
                    return Err(Error::Query(
                        "El hash necesita al menos un campo con valor: Redis no guarda hashes vacíos.".into(),
                    ));
                }
                out.push(blob_line(&a, Some(&notes.join("; "))));
            }
            "vectorset" => {
                entries("vector set")?;
                for c in &t.columns {
                    let v = c
                        .default_value
                        .as_deref()
                        .and_then(dbine_driver::vector::parse)
                        .ok_or_else(|| {
                            Error::Query(format!(
                                "El elemento {} necesita su vector como valor por defecto: p. ej. [0.1, 0.2, 0.3].",
                                c.name
                            ))
                        })?;
                    let mut a = vec!["VADD".to_string(), key.into()];
                    if let Some(r) = opt(t, "reduce") {
                        let n: u32 = r.trim().parse().map_err(|_| {
                            Error::Query(format!("Reducir a tiene que ser un número entero: {r}"))
                        })?;
                        a.extend(["REDUCE".into(), n.to_string()]);
                    }
                    let mut tail = vectors::vadd_args(key, &c.name, &v, None);
                    tail.drain(..2);
                    a.extend(tail);
                    if let Some(q) = opt(t, "quantization") {
                        a.push(q.trim().to_ascii_uppercase());
                    }
                    for (o, flag) in [("ef", "EF"), ("m", "M")] {
                        if let Some(n) = opt(t, o) {
                            let n: u32 = n.trim().parse().map_err(|_| {
                                Error::Query(format!("{flag} tiene que ser un número entero: {n}"))
                            })?;
                            a.extend([flag.to_string(), n.to_string()]);
                        }
                    }
                    out.push(line_of(&a));
                }
            }
            "list" | "set" => {
                entries(&ty)?;
                args.extend([
                    if ty == "list" { "RPUSH" } else { "SADD" }.into(),
                    key.into(),
                ]);
                args.extend(t.columns.iter().map(element));
            }
            "zset" => {
                entries("sorted set")?;
                args.extend(["ZADD".into(), key.into()]);
                for c in &t.columns {
                    let score = c
                        .default_value
                        .clone()
                        .filter(|v| !v.trim().is_empty())
                        .unwrap_or_else(|| "0".into());
                    let s = score.trim();
                    let ok = s.parse::<f64>().is_ok_and(|f| !f.is_nan())
                        || matches!(s.to_ascii_lowercase().as_str(), "+inf" | "-inf" | "inf");
                    if !ok {
                        return Err(Error::Query(format!(
                            "El score de '{}' no es un número: {score}",
                            c.name
                        )));
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
                let doc = if value.trim().is_empty() {
                    "{}"
                } else {
                    value.trim()
                };
                let parsed: Value = serde_json::from_str(doc)
                    .map_err(|e| Error::Query(format!("El valor no es un JSON válido: {e}")))?;
                args.extend([
                    "JSON.SET".into(),
                    key.into(),
                    "$".into(),
                    parsed.to_string(),
                ]);
            }
            other => return Err(Error::Query(format!("Tipo de key desconocido: {other}"))),
        }
        if !args.is_empty() {
            out.push(line(&args.iter().map(String::as_str).collect::<Vec<_>>()));
        }
        if let Some(ttl) = opt(t, "ttl") {
            let secs: i64 = ttl
                .trim()
                .parse()
                .map_err(|_| Error::Query(format!("El TTL no es un número entero: {ttl}")))?;
            if secs > 0 {
                out.push(line(&["EXPIRE", key, &secs.to_string()]));
            }
        }
    }
    // The search indexes over the key's prefix, once the key is written
    // (FT.CREATE indexes what's already there).
    if parts.indexes && !t.indexes.is_empty() {
        if !matches!(ty.as_str(), "hash" | "json") {
            return Err(Error::Query(
                "Los índices de búsqueda son de hashes o de documentos JSON: cambiá el tipo de la key o quitá los índices.".into(),
            ));
        }
        for ix in &t.indexes {
            out.push(key_index(t, ix, ty == "json", server)?.create_command());
        }
    }
    Ok(if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    })
}

/// A cell as a Redis argument: text as is, the rest as JSON text.
fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A JSON array of numbers: how a vector comes to a script.
fn is_vector_value(v: &Value) -> bool {
    matches!(v, Value::Array(a) if !a.is_empty() && a.iter().all(Value::is_number))
}

/// The type a vector written to the field `field` of the hash `key` takes:
/// that of the search index that covers it, if DBine has seen one, else
/// `FLOAT32` (what vectors in hashes usually are) for a value that is a
/// vector. `None`: not a vector, written as it is.
fn hash_vector_type(known: &Known, key: &str, field: &str, v: &Value) -> Option<VType> {
    match known.hash_vector(key, field) {
        Some(f) => dbine_driver::vector::parse_value(v).map(|_| f.ty),
        None => is_vector_value(v).then_some(VType::F32),
    }
}

/// Each row as a hash: `HSET <target>:<first column's value> col value …`,
/// null cells left out. A vector (a JSON array of numbers) is written as
/// the blob of the type of the search index that covers the key, or of
/// `FLOAT32`.
pub fn insert_script(
    target: &ObjectRef,
    columns: &[String],
    rows: &[Vec<Value>],
    known: &Known,
) -> Result<String> {
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
        let mut args: Vec<Arg> = vec!["HSET".into(), key.clone().into()];
        for (c, v) in columns.iter().zip(row) {
            if v.is_null() {
                continue;
            }
            args.push(c.clone().into());
            match hash_vector_type(known, &key, c, v).and_then(|t| vectors::pack(v, t)) {
                Some(blob) => args.push(Arg::Blob(blob)),
                None => args.push(cell_text(v).into()),
            }
        }
        out.push_str(&blob_line(&args, None));
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
const HASH_RENAME: &str =
    "local v = redis.call('HGET', KEYS[1], ARGV[1]) if not v then return 0 end \
redis.call('HDEL', KEYS[1], ARGV[1]) return redis.call('HSET', KEYS[1], ARGV[2], v)";

/// Lua that renames a sorted set member keeping its score (ARGV: old, new).
const ZSET_RENAME: &str =
    "local s = redis.call('ZSCORE', KEYS[1], ARGV[1]) if not s then return 0 end \
redis.call('ZREM', KEYS[1], ARGV[1]) return redis.call('ZADD', KEYS[1], s, ARGV[2])";

/// Commands for cells edited while browsing a key. The columns tell the
/// key type (see `columns` in the session): `field`/`value` is a hash
/// (`HSET`, `HDEL` for null), `member`/`score` a sorted set (`ZADD`,
/// `ZREM` for a null score), and a lone `value` is a string, list, set or
/// JSON key, edited with an `EVAL` that checks the type on the server.
/// Renaming a field or member without its value/score keeps the stored one
/// (also with `EVAL`). Stream entries can't be changed.
/// Edits of the documents of a search index's results (`FT.SEARCH`):
/// `HSET` / `HDEL` on a hash (a vector field as the blob of its type),
/// `JSON.SET` / `JSON.DEL` on a JSON document, by the document's key (`id`).
fn index_edits(index: &FtIndex, changes: &[RowChange]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let id = ch
            .key
            .iter()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| cell_text(v))
            .ok_or_else(|| {
                Error::Unsupported(
                    "para editar un documento del índice hace falta su key (columna id)".into(),
                )
            })?;
        for (col, v) in &ch.set {
            if col == "id" {
                return Err(Error::Unsupported(
                    "la key de un documento no se cambia desde la grilla: renombrala".into(),
                ));
            }
            let field = index.fields.iter().find(|f| f.attribute == *col);
            if index.on_json {
                let path = field.map_or_else(|| format!("$.{col}"), |f| f.identifier.clone());
                if v.is_null() {
                    out.push(line(&["JSON.DEL", &id, &path]));
                    continue;
                }
                // A vector (or any value) as JSON; text that is JSON goes
                // as that JSON.
                let json = match v {
                    Value::String(t) => serde_json::from_str::<Value>(t)
                        .ok()
                        .filter(|j| j.is_array() || j.is_object())
                        .unwrap_or_else(|| v.clone()),
                    other => other.clone(),
                };
                out.push(line(&["JSON.SET", &id, &path, &json.to_string()]));
                continue;
            }
            let name = field.map_or(col.as_str(), |f| f.identifier.as_str());
            if v.is_null() {
                out.push(line(&["HDEL", &id, name]));
                continue;
            }
            match field.and_then(|f| f.vector.as_ref()) {
                Some(vf) => {
                    let (arg, note) = vector_arg(v, vf.ty, name).ok_or_else(|| {
                        Error::Query(format!(
                            "El valor de {col} no es un vector: escribilo como [0.1, 0.2, 0.3]."
                        ))
                    })?;
                    out.push(blob_line(
                        &["HSET".into(), id.clone().into(), name.into(), arg],
                        Some(&note),
                    ));
                }
                None => out.push(line(&["HSET", &id, name, &cell_text(v)])),
            }
        }
    }
    Ok(out)
}

/// Edits of a vector set's elements: a new vector (`VADD`, with the set's
/// quantization when DBine has read it) or new attributes (`VSETATTR`).
fn vectorset_edits(key: &str, changes: &[RowChange], known: &Known) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let element = ch
            .key
            .iter()
            .find(|(k, _)| k == "element")
            .map(|(_, v)| cell_text(v))
            .unwrap_or_default();
        for (col, v) in &ch.set {
            match col.as_str() {
                "vector" => {
                    let nums = dbine_driver::vector::parse_value(v).ok_or_else(|| {
                        Error::Query(
                            "El vector de un elemento se escribe como [0.1, 0.2, 0.3]; para quitar el elemento, borrá la fila.".into(),
                        )
                    })?;
                    let flag = known.vsets.get(key).map(String::as_str);
                    out.push(line_of(&vectors::vadd_args(key, &element, &nums, flag)));
                }
                "attributes" => {
                    let attrs = match v {
                        Value::Null => String::new(),
                        Value::String(t) => t.clone(),
                        other => other.to_string(),
                    };
                    out.push(line(&["VSETATTR", key, &element, &attrs]));
                }
                "element" => return Err(Error::Unsupported(
                    "un elemento de un vector set no se renombra: agregá uno nuevo y borrá este"
                        .into(),
                )),
                other => {
                    return Err(Error::Unsupported(format!(
                        "un vector set no tiene la columna {other}"
                    )))
                }
            }
        }
    }
    Ok(out)
}

pub fn update_script(target: &ObjectRef, changes: &[RowChange], known: &Known) -> Result<String> {
    let key = target.name.as_str();
    let get = |pairs: &[(String, Value)], c: &str| {
        pairs.iter().find(|(k, _)| k == c).map(|(_, v)| v.clone())
    };
    let finish = |out: Vec<String>| {
        if out.is_empty() {
            String::new()
        } else {
            out.join("\n") + "\n"
        }
    };
    // The documents of a search index's results.
    if target.kind == kinds::INDEX {
        let index = known.indexes.get(key).ok_or_else(|| {
            Error::Unsupported(format!(
                "DBine no leyó todavía el índice {key}: abrilo de nuevo para que lea sus campos"
            ))
        })?;
        return index_edits(index, changes).map(finish);
    }
    if changes
        .iter()
        .any(|c| c.key.iter().any(|(k, _)| k == "element"))
    {
        return vectorset_edits(key, changes, known).map(finish);
    }
    let mut out: Vec<String> = Vec::new();
    // A hash field that holds a vector: its blob, with what the comment says.
    let hset = |field: &str, v: &Value| -> String {
        match known
            .hash_vector(key, field)
            .and_then(|f| vector_arg(v, f.ty, field))
        {
            Some((arg, note)) => {
                blob_line(&["HSET".into(), key.into(), field.into(), arg], Some(&note))
            }
            None => line(&["HSET", key, field, &cell_text(v)]),
        }
    };
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        let (k, s) = (&ch.key, &ch.set);
        if let Some(field) = get(k, "field").filter(|v| !v.is_null()) {
            let field = cell_text(&field);
            let new_field = get(s, "field")
                .filter(|v| !v.is_null())
                .map(|v| cell_text(&v))
                .filter(|f| *f != field);
            match (new_field, get(s, "value")) {
                (_, Some(Value::Null)) => out.push(line(&["HDEL", key, &field])),
                (None, Some(v)) => out.push(hset(&field, &v)),
                (Some(nf), Some(v)) => {
                    out.push(line(&["HDEL", key, &field]));
                    out.push(hset(&nf, &v));
                }
                (Some(nf), None) => out.push(line(&["EVAL", HASH_RENAME, "1", key, &field, &nf])),
                (None, None) => {}
            }
        } else if let Some(member) = get(k, "member").filter(|v| !v.is_null()) {
            let member = cell_text(&member);
            let new_member = get(s, "member")
                .filter(|v| !v.is_null())
                .map(|v| cell_text(&v))
                .filter(|m| *m != member);
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
            return Err(Error::Unsupported(
                "las entradas de un stream de Redis no se pueden modificar".into(),
            ));
        } else if let Some(new) = get(s, "value") {
            let old = get(k, "value").map(|v| cell_text(&v)).unwrap_or_default();
            let new = (!new.is_null()).then(|| cell_text(&new));
            let mut args = vec!["EVAL", VALUE_EDIT, "1", key, &old];
            if let Some(n) = &new {
                args.push(n);
            }
            out.push(line(&args));
        } else {
            return Err(Error::Unsupported(
                "no se reconoce el tipo de key de Redis a partir de sus columnas".into(),
            ));
        }
    }
    Ok(if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    })
}

/// Commands that remove rows of a browsed key, told apart by the key
/// columns as in [`update_script`]: `HDEL` for a hash field, `ZREM` for a
/// sorted set member, `XDEL` for a stream entry, and for a lone `value`
/// (string, list, set or JSON key) the same `EVAL` as an edit to null:
/// `SREM` / `LREM … 1` for one element, `DEL` for a string or JSON key
/// (its only row).
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let key = target.name.as_str();
    let get = |pairs: &[(String, Value)], c: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == c)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_null())
    };
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        // A search index's document is its key; a vector set's row, an
        // element.
        if target.kind == kinds::INDEX {
            match get(k, "id") {
                Some(id) => out.push(line(&["DEL", &cell_text(&id)])),
                None => {
                    return Err(Error::Unsupported(
                        "para borrar un documento del índice hace falta su key (columna id)".into(),
                    ))
                }
            }
        } else if let Some(element) = get(k, "element") {
            out.push(line(&["VREM", key, &cell_text(&element)]));
        } else if let Some(field) = get(k, "field") {
            out.push(line(&["HDEL", key, &cell_text(&field)]));
        } else if let Some(member) = get(k, "member") {
            out.push(line(&["ZREM", key, &cell_text(&member)]));
        } else if let (Some(id), None) = (get(k, "id"), get(k, "value")) {
            out.push(line(&["XDEL", key, &cell_text(&id)]));
        } else if let Some(old) = get(k, "value") {
            out.push(line(&["EVAL", VALUE_EDIT, "1", key, &cell_text(&old)]));
        } else {
            return Err(Error::Unsupported(
                "no se reconoce qué borrar de la key de Redis a partir de sus columnas".into(),
            ));
        }
    }
    Ok(if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse_script;

    // The scripts as a server DBine has read nothing of makes them.
    fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
        super::table_ddl(t, parts, Server::Redis)
    }
    fn insert_script(
        target: &ObjectRef,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> Result<String> {
        super::insert_script(target, columns, rows, &Known::default())
    }
    fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        super::update_script(target, changes, &Known::default())
    }

    fn key(ty: &str, cols: &[(&str, Option<&str>)]) -> TableSchema {
        let mut t = TableSchema {
            kind: kinds::KEY.into(),
            name: "my key".into(),
            ..Default::default()
        };
        t.options.insert("type".into(), ty.into());
        t.columns = cols
            .iter()
            .map(|(n, d)| ColumnDef {
                name: n.to_string(),
                default_value: d.map(str::to_string),
                ..Default::default()
            })
            .collect();
        t
    }

    fn create() -> DdlParts {
        DdlParts {
            create: true,
            ..Default::default()
        }
    }

    /// The script, parsed back by the editor's own parser.
    fn parsed(text: &str) -> Vec<Vec<String>> {
        parse_script(text)
            .unwrap()
            .into_iter()
            .map(|c| {
                c.into_iter()
                    .map(|a| String::from_utf8(a).unwrap())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn each_type_round_trips_through_the_parser() {
        let mut s = key("string", &[]);
        s.options
            .insert("value".into(), "hola \"mundo\"\nsegunda línea".into());
        s.options.insert("ttl".into(), "60".into());
        let ddl = table_ddl(
            &s,
            DdlParts {
                drop: true,
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            parsed(&ddl),
            [
                vec!["DEL", "my key"],
                vec!["SET", "my key", "hola \"mundo\"\nsegunda línea"],
                vec!["EXPIRE", "my key", "60"]
            ]
        );

        let h = table_ddl(
            &key(
                "hash",
                &[
                    ("name", Some("Ana María")),
                    ("empty", None),
                    ("#tag", Some("x")),
                ],
            ),
            create(),
        )
        .unwrap();
        assert_eq!(
            parsed(&h),
            [vec![
                "HSET",
                "my key",
                "name",
                "Ana María",
                "empty",
                "",
                "#tag",
                "x"
            ]]
        );

        let l = table_ddl(
            &key("list", &[("a", None), ("ignored", Some("b c"))]),
            create(),
        )
        .unwrap();
        assert_eq!(parsed(&l), [vec!["RPUSH", "my key", "a", "b c"]]);
        let st = table_ddl(&key("set", &[("x", None), ("it's", None)]), create()).unwrap();
        assert_eq!(parsed(&st), [vec!["SADD", "my key", "x", "it's"]]);

        let z = table_ddl(
            &key(
                "zset",
                &[("ana", Some("1.5")), ("luis", None), ("max", Some("+inf"))],
            ),
            create(),
        )
        .unwrap();
        assert_eq!(
            parsed(&z),
            [vec![
                "ZADD", "my key", "1.5", "ana", "0", "luis", "+inf", "max"
            ]]
        );
        assert!(table_ddl(&key("zset", &[("a", Some("mucho"))]), create()).is_err());

        let x = table_ddl(
            &key(
                "stream",
                &[("evento", Some("alta")), ("usuario", Some("ana"))],
            ),
            create(),
        )
        .unwrap();
        assert_eq!(
            parsed(&x),
            [vec![
                "XADD", "my key", "*", "evento", "alta", "usuario", "ana"
            ]]
        );

        let mut j = key("json", &[]);
        j.options.insert(
            "value".into(),
            "{\"a\": [1, \"dos\"], \"b\": \"it's\"}".into(),
        );
        assert_eq!(
            parsed(&table_ddl(&j, create()).unwrap()),
            [vec![
                "JSON.SET",
                "my key",
                "$",
                "{\"a\":[1,\"dos\"],\"b\":\"it's\"}"
            ]]
        );
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
        assert_eq!(
            parsed(&table_ddl(&s, create()).unwrap()),
            [vec!["SET", "my key", ""]]
        );
        // Only DROP.
        let drop = table_ddl(
            &s,
            DdlParts {
                drop: true,
                if_exists: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(parsed(&drop), [vec!["DEL", "my key"]]);
    }

    #[test]
    fn rows_become_hashes() {
        let target = ObjectRef {
            kind: kinds::KEY.into(),
            schema: None,
            name: "user".into(),
        };
        let cols = vec!["id".to_string(), "name".into(), "age".into(), "tags".into()];
        let rows = vec![
            vec![
                Value::from(1),
                Value::from("Ana \"A\""),
                Value::from(30),
                serde_json::json!(["x", "y"]),
            ],
            vec![
                Value::from("b 2"),
                Value::Null,
                Value::from(true),
                Value::Null,
            ],
        ];
        let script = insert_script(&target, &cols, &rows).unwrap();
        assert_eq!(
            parsed(&script),
            [
                vec![
                    "HSET",
                    "user:1",
                    "id",
                    "1",
                    "name",
                    "Ana \"A\"",
                    "age",
                    "30",
                    "tags",
                    "[\"x\",\"y\"]"
                ],
                vec!["HSET", "user:b 2", "id", "b 2", "age", "true"],
            ]
        );
        assert!(insert_script(&target, &cols, &[vec![Value::Null, Value::from(1)]]).is_err());
        assert_eq!(insert_script(&target, &[], &rows).unwrap(), "");
    }

    #[test]
    fn edits_by_key_type() {
        let target = ObjectRef {
            kind: kinds::KEY.into(),
            schema: None,
            name: "k 1".into(),
        };
        let ch = |key: Vec<(&str, Value)>, set: Vec<(&str, Value)>| RowChange {
            key: key.into_iter().map(|(c, v)| (c.to_string(), v)).collect(),
            set: set.into_iter().map(|(c, v)| (c.to_string(), v)).collect(),
            ..Default::default()
        };
        let changes = vec![
            ch(
                vec![("field", "name".into())],
                vec![("value", "O'Brien \"Bob\"".into())],
            ),
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
        let renames = vec![
            ch(vec![("field", "a".into())], vec![("field", "b".into())]),
            ch(vec![("member", "a".into())], vec![("score", Value::Null)]),
        ];
        assert_eq!(
            parsed(&update_script(&target, &renames).unwrap()),
            [
                vec!["EVAL", HASH_RENAME, "1", "k 1", "a", "b"],
                vec!["ZREM", "k 1", "a"]
            ]
        );
        assert!(update_script(
            &target,
            &[ch(vec![("id", "1-0".into())], vec![("a", "y".into())])]
        )
        .is_err());
    }

    #[test]
    fn delete_script_by_key_type() {
        let target = ObjectRef {
            kind: kinds::KEY.into(),
            schema: None,
            name: "my key".into(),
        };
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

    fn col(name: &str, ty: &str, default: Option<&str>) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            default_value: default.map(str::to_string),
            ..Default::default()
        }
    }

    fn vec_index(name: &str, columns: &[&str], metric: &str) -> IndexDef {
        IndexDef {
            name: name.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            vector: Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some(metric.into()),
                params: [("m".to_string(), "24".to_string())].into(),
            }),
            ..Default::default()
        }
    }

    /// A hash with a vector field and its search index, as the designer
    /// makes them.
    fn doc() -> TableSchema {
        let mut t = key("hash", &[("title", Some("Hola")), ("lang", Some("es"))]);
        t.name = "doc:1".into();
        t.columns[1].data_type = "TAG".into();
        t.columns
            .push(col("emb", "VECTOR(3, FLOAT32)", Some("[0.1, 0.2, 0.3]")));
        t.indexes = vec![vec_index("docs", &["emb", "lang", "title"], "ip")];
        t
    }

    fn with_indexes() -> DdlParts {
        DdlParts {
            create: true,
            indexes: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_designer_offers_vectors_per_server() {
        let r = designer("redis");
        assert!(r.indexes);
        assert!(r.data_types.contains(&"VECTOR(3, FLOAT32)"));
        assert!(r.data_types.contains(&"VECTOR(3, BFLOAT16)"));
        let v = r.vector_indexes.unwrap();
        assert_eq!(v.algorithms, ["hnsw", "flat", "svs-vamana"]);
        assert_eq!(v.metrics, ["cosine", "l2", "ip"]);
        assert!(v.filter_columns);
        assert!(v.params.iter().any(|p| p.key == "m" && p.default == "16"));
        let FieldKind::Select(types) = &r.table_options[0].kind else {
            panic!()
        };
        assert!(types.iter().any(|(t, _)| *t == "vectorset"));
        assert!(r.table_options.iter().any(|f| f.key == "index_prefix"));
        // valkey-search: FLOAT32 only, no SVS, no vector sets.
        let k = designer("valkey");
        assert!(!k.data_types.contains(&"VECTOR(3, FLOAT64)"));
        assert_eq!(
            k.vector_indexes.as_ref().unwrap().algorithms,
            ["hnsw", "flat"]
        );
        let FieldKind::Select(types) = &k.table_options[0].kind else {
            panic!()
        };
        assert!(!types.iter().any(|(t, _)| *t == "vectorset"));
        let d = designer("dragonfly");
        assert!(d.data_types.contains(&"VECTOR(3, INT8)"));
        assert!(d
            .vector_indexes
            .unwrap()
            .params
            .iter()
            .all(|p| p.key != "block_size"));
    }

    #[test]
    fn a_hash_with_a_vector_and_its_index() {
        let ddl = super::table_ddl(&doc(), with_indexes(), Server::Redis).unwrap();
        let lines: Vec<&str> = ddl.lines().collect();
        assert_eq!(lines.len(), 2, "{ddl}");
        // The vector as its FLOAT32 blob, said in the comment.
        assert!(
            lines[0].ends_with("# emb: FLOAT32 [0.1,0.2,0.3]"),
            "{}",
            lines[0]
        );
        let p = parse_script(&ddl).unwrap();
        assert_eq!(
            p[0][..4],
            [
                b"HSET".to_vec(),
                b"doc:1".to_vec(),
                b"title".to_vec(),
                b"Hola".to_vec()
            ]
        );
        assert_eq!(p[0][6], b"emb".to_vec());
        assert_eq!(p[0][7], VType::F32.encode(&[0.1, 0.2, 0.3]));
        assert_eq!(
            lines[1],
            "FT.CREATE docs ON HASH PREFIX 1 doc: SCHEMA emb VECTOR HNSW 8 TYPE FLOAT32 DIM 3 DISTANCE_METRIC IP M 24 lang TAG title TAG"
        );
        // A JSON document: the fields are JSONPaths.
        let mut j = doc();
        j.options.insert("type".into(), "json".into());
        j.options
            .insert("value".into(), "{\"emb\": [1, 2, 3]}".into());
        j.options.insert("index_prefix".into(), "item:".into());
        j.columns[0].data_type = "TEXT".into();
        let ddl = super::table_ddl(&j, with_indexes(), Server::Redis).unwrap();
        assert!(ddl.contains(
            "FT.CREATE docs ON JSON PREFIX 1 item: SCHEMA $.emb AS emb VECTOR HNSW 8 TYPE FLOAT32 DIM 3 DISTANCE_METRIC IP M 24 $.lang AS lang TAG $.title AS title TEXT"
        ), "{ddl}");
        // A plain search index (no vector settings): its vector field
        // with the defaults.
        let mut plain = doc();
        plain.indexes[0].vector = None;
        let ddl = super::table_ddl(&plain, with_indexes(), Server::Redis).unwrap();
        assert!(
            ddl.contains("emb VECTOR HNSW 6 TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE"),
            "{ddl}"
        );
    }

    #[test]
    fn vector_mistakes_are_refused() {
        let err = |t: &TableSchema, server| {
            super::table_ddl(t, with_indexes(), server)
                .unwrap_err()
                .to_string()
        };
        let mut t = doc();
        t.columns[2].data_type = "vector".into();
        assert!(err(&t, Server::Redis).contains("dimensión"));
        let mut t = doc();
        t.columns[2].default_value = Some("[1, 2]".into());
        assert!(err(&t, Server::Redis).contains("2 dimensiones"));
        let mut t = doc();
        t.columns[2].default_value = Some("hola".into());
        assert!(err(&t, Server::Redis).contains("no es un vector"));
        let mut t = doc();
        t.columns[2].data_type = "VECTOR(3, FLOAT64)".into();
        assert!(err(&t, Server::Valkey).contains("FLOAT64"));
        let mut t = doc();
        t.indexes[0].vector.as_mut().unwrap().algorithm = Some("svs-vamana".into());
        assert!(err(&t, Server::Dragonfly).contains("SVS-VAMANA"));
        assert!(super::table_ddl(&t, with_indexes(), Server::Redis).is_ok());
        let mut t = doc();
        t.indexes[0].vector.as_mut().unwrap().metric = Some("l1".into());
        assert!(err(&t, Server::Redis).contains("l1"));
        let mut t = doc();
        t.indexes[0].columns = vec!["title".into()];
        assert!(err(&t, Server::Redis).contains("tiene que ser un vector"));
        let mut t = doc();
        t.options.insert("type".into(), "list".into());
        assert!(err(&t, Server::Redis).contains("hashes o de documentos JSON"));
    }

    #[test]
    fn a_vector_set_from_the_designer() {
        let mut t = key(
            "vectorset",
            &[("a", Some("[0.1, 0.2]")), ("b c", Some("[1,0]"))],
        );
        t.options.insert("quantization".into(), "noquant".into());
        t.options.insert("m".into(), "32".into());
        let ddl = super::table_ddl(&t, create(), Server::Redis).unwrap();
        assert_eq!(
            parsed(&ddl),
            [
                vec!["VADD", "my key", "VALUES", "2", "0.1", "0.2", "a", "NOQUANT", "M", "32"],
                vec!["VADD", "my key", "VALUES", "2", "1", "0", "b c", "NOQUANT", "M", "32"],
            ]
        );
        t.options.insert("reduce".into(), "1".into());
        assert!(super::table_ddl(&t, create(), Server::Redis)
            .unwrap()
            .starts_with("VADD \"my key\" REDUCE 1 VALUES 2"));
        let bad = key("vectorset", &[("a", None)]);
        assert!(table_ddl(&bad, create()).is_err());
    }

    fn redis_index() -> FtIndex {
        FtIndex {
            name: "docs".into(),
            on_json: false,
            prefixes: vec!["doc:".into(), "art:".into()],
            filter: Some("@lang==\"es\"".into()),
            fields: vec![
                FtField {
                    identifier: "title".into(),
                    attribute: "title".into(),
                    kind: "TEXT".into(),
                    vector: None,
                    args: vec!["WEIGHT".into(), "2".into(), "SORTABLE".into()],
                },
                FtField {
                    identifier: "emb".into(),
                    attribute: "embedding".into(),
                    kind: "VECTOR".into(),
                    vector: Some(FtVector {
                        algorithm: "HNSW".into(),
                        ty: VType::F16,
                        dims: 4,
                        metric: "L2".into(),
                        params: vec![
                            ("M".into(), "8".into()),
                            ("EF_CONSTRUCTION".into(), "100".into()),
                        ],
                    }),
                    args: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn a_search_index_as_an_object() {
        let ix = redis_index();
        let t = index_schema(&ix);
        assert_eq!(t.kind, kinds::INDEX);
        assert_eq!(t.columns[1].name, "embedding");
        assert_eq!(t.columns[1].data_type, "VECTOR(4, FLOAT16)");
        assert_eq!(t.columns[1].options["identifier"], "emb");
        let v = t.indexes[0].vector.as_ref().unwrap();
        assert_eq!(v.metric.as_deref(), Some("l2"));
        assert_eq!(v.params["m"], "8");
        // What the object scripts is what made it.
        assert_eq!(schema_index(&t, Server::Redis).unwrap(), ix);
        let ddl = super::table_ddl(
            &t,
            DdlParts {
                drop: true,
                create: true,
                ..Default::default()
            },
            Server::Redis,
        )
        .unwrap();
        assert_eq!(ddl, format!("FT.DROPINDEX docs\n{}\n", ix.create_command()));
        assert!(
            ix.create_command().contains("FILTER \"@lang==\\\"es\\\"\""),
            "{}",
            ix.create_command()
        );
    }

    #[test]
    fn scripts_write_vectors_as_blobs() {
        let target = ObjectRef {
            kind: kinds::KEY.into(),
            schema: None,
            name: "doc".into(),
        };
        let cols = vec!["id".to_string(), "emb".into(), "tags".into()];
        let rows = vec![vec![
            Value::from(1),
            serde_json::json!([0.5, 1, -2]),
            serde_json::json!(["x"]),
        ]];
        // Nothing known: FLOAT32.
        let s = insert_script(&target, &cols, &rows).unwrap();
        let p = parse_script(&s).unwrap();
        assert_eq!(p[0][4], b"emb".to_vec());
        assert_eq!(p[0][5], VType::F32.encode(&[0.5, 1.0, -2.0]));
        assert_eq!(p[0][7], b"[\"x\"]".to_vec());
        // A search index over doc: says FLOAT16.
        let mut known = Known::default();
        let mut ix = redis_index();
        ix.fields[1].identifier = "emb".into();
        known.indexes.insert(ix.name.clone(), ix.clone());
        let s = super::insert_script(&target, &cols, &rows, &known).unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][5],
            VType::F16.encode(&[0.5, 1.0, -2.0])
        );
        // An edited hash field, as text from the grid.
        let hash = ObjectRef {
            name: "doc:7".into(),
            ..target.clone()
        };
        let ch = RowChange {
            key: vec![("field".into(), Value::from("emb"))],
            set: vec![("value".into(), Value::from("[1,2,3,4]"))],
            ..Default::default()
        };
        let s = super::update_script(&hash, std::slice::from_ref(&ch), &known).unwrap();
        assert!(s.trim_end().ends_with("# emb: FLOAT16 [1,2,3,4]"), "{s}");
        assert_eq!(
            parse_script(&s).unwrap()[0][3],
            VType::F16.encode(&[1.0, 2.0, 3.0, 4.0])
        );
        // Without an index, the text stays text.
        let s = update_script(&hash, std::slice::from_ref(&ch)).unwrap();
        assert_eq!(parsed(&s), [vec!["HSET", "doc:7", "emb", "[1,2,3,4]"]]);
    }

    #[test]
    fn edits_of_search_results() {
        let idx = ObjectRef {
            kind: kinds::INDEX.into(),
            schema: None,
            name: "docs".into(),
        };
        let ch = |set: Vec<(&str, Value)>| RowChange {
            key: vec![("id".into(), Value::from("doc:1"))],
            set: set.into_iter().map(|(c, v)| (c.to_string(), v)).collect(),
            ..Default::default()
        };
        // DBine hasn't read the index: it doesn't guess.
        assert!(update_script(&idx, &[ch(vec![("title", Value::from("x"))])]).is_err());
        let mut known = Known::default();
        known.indexes.insert("docs".into(), redis_index());
        let s = super::update_script(
            &idx,
            &[ch(vec![
                ("title", Value::from("Nuevo")),
                ("embedding", serde_json::json!([1, 0, 0, 0.5])),
                ("other", Value::Null),
            ])],
            &known,
        )
        .unwrap();
        let p = parse_script(&s).unwrap();
        assert_eq!(
            p[0],
            [
                b"HSET".to_vec(),
                b"doc:1".to_vec(),
                b"title".to_vec(),
                b"Nuevo".to_vec()
            ]
        );
        // The attribute's hash field (emb), its blob.
        assert_eq!(p[1][2], b"emb".to_vec());
        assert_eq!(p[1][3], VType::F16.encode(&[1.0, 0.0, 0.0, 0.5]));
        assert_eq!(
            p[2],
            [b"HDEL".to_vec(), b"doc:1".to_vec(), b"other".to_vec()]
        );
        // A JSON index: JSON.SET at the field's path.
        let mut j = redis_index();
        j.on_json = true;
        j.fields[1].identifier = "$.emb".into();
        known.indexes.insert("docs".into(), j);
        let s = super::update_script(
            &idx,
            &[ch(vec![("embedding", serde_json::json!([1, 2, 3, 4]))])],
            &known,
        )
        .unwrap();
        assert_eq!(
            parsed(&s),
            [vec!["JSON.SET", "doc:1", "$.emb", "[1,2,3,4]"]]
        );
        assert!(super::update_script(&idx, &[ch(vec![("id", Value::from("x"))])], &known).is_err());
        // Deleting a document is deleting its key.
        assert_eq!(
            parsed(&delete_script(&idx, &[vec![("id".into(), Value::from("doc:1"))]]).unwrap()),
            [vec!["DEL", "doc:1"]]
        );
    }

    #[test]
    fn edits_of_vector_sets() {
        let vs = ObjectRef {
            kind: kinds::KEY.into(),
            schema: None,
            name: "vs".into(),
        };
        let ch = RowChange {
            key: vec![("element".into(), Value::from("a b"))],
            set: vec![
                ("vector".into(), serde_json::json!([0.1, 2])),
                ("attributes".into(), Value::from("{\"lang\":\"es\"}")),
            ],
            ..Default::default()
        };
        let mut known = Known::default();
        assert_eq!(
            parsed(&super::update_script(&vs, std::slice::from_ref(&ch), &known).unwrap()),
            [
                vec!["VADD", "vs", "VALUES", "2", "0.1", "2", "a b"],
                vec!["VSETATTR", "vs", "a b", "{\"lang\":\"es\"}"],
            ]
        );
        // The set's quantization, once read, goes with VADD.
        known.vsets.insert("vs".into(), "NOQUANT".into());
        let s = super::update_script(&vs, std::slice::from_ref(&ch), &known).unwrap();
        assert!(
            s.starts_with("VADD vs VALUES 2 0.1 2 \"a b\" NOQUANT\n"),
            "{s}"
        );
        let none = RowChange {
            set: vec![("vector".into(), Value::Null)],
            ..ch.clone()
        };
        assert!(super::update_script(&vs, &[none], &known).is_err());
        assert_eq!(
            parsed(&delete_script(&vs, &[vec![("element".into(), Value::from("a b"))]]).unwrap()),
            [vec!["VREM", "vs", "a b"]]
        );
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

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
use crate::vectors::{self, FtField, FtIndex, FtVector, Known, Server, Typed, Types, VType};
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
                // One line each, and at most one «;»: the message extractor
                // (scripts/i18n) takes a literal whole only without line
                // continuations, and one with more «;» for code.
                "Las columnas son las entradas. Hash y stream: campo (nombre) y valor (valor por defecto). List y set: elemento. Sorted set: miembro (nombre) y score (valor por defecto). Vector set: elemento (nombre) y su vector (valor por defecto, p. ej. [0.1, 0.2, 0.3])."
            } else {
                "Las columnas son las entradas. Hash y stream: campo (nombre) y valor (valor por defecto). List y set: elemento. Sorted set: miembro (nombre) y score (valor por defecto)."
            }),
        Field::new("ttl", "TTL (segundos)", FieldKind::Number)
            .placeholder("(sin vencimiento)")
            .help("Agrega un EXPIRE después de crear la key."),
        Field::new("value", "Valor", FieldKind::Textarea)
            .help("Solo para string y JSON (un documento JSON, p. ej. {\"a\": 1})."),
        Field::new("index_prefix", "Prefijo de los índices", FieldKind::Text)
            .placeholder("(el nombre de la key hasta su último «:»)")
            .help(if server == Server::Redis {
                "Los índices de búsqueda (FT.CREATE … PREFIX) cubren las keys que empiezan así: hashes o documentos JSON. Los vectores FLOAT16 y BFLOAT16 piden Redis Stack 7.4 o Redis 8, los INT8 y UINT8 piden Redis 8.0, y el algoritmo SVS-VAMANA pide Redis 8.2 y solo toma FLOAT32 o FLOAT16."
            } else {
                "Los índices de búsqueda (FT.CREATE … PREFIX) cubren las keys que empiezan así: hashes o documentos JSON."
            }),
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

pub fn templates(server: Server) -> Vec<CreateTemplate> {
    let t = |label: &'static str, template: &str| CreateTemplate {
        kind: kinds::KEY,
        label,
        template: template.into(),
    };
    // A vector of the templates, as the blob of a FLOAT32 field.
    let v = |x: &[f64]| vectors::quote_bytes(&VType::F32.encode(x));
    let mut out = vec![
        CreateTemplate {
            kind: kinds::INDEX,
            label: "Nuevo índice vectorial (hashes)",
            template: format!(
                "FT.CREATE {{name}} ON HASH PREFIX 1 {{name}}: SCHEMA etiqueta TAG embedding VECTOR HNSW 6 TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE\n\
                 HSET {{name}}:1 etiqueta a embedding {} # embedding: FLOAT32 [0.1,0.2,0.3]\n\
                 HSET {{name}}:2 etiqueta b embedding {} # embedding: FLOAT32 [0.3,0.1,0.2]\n\
                 FT.SEARCH {{name}} \"*=>[KNN 5 @embedding $v AS distancia]\" PARAMS 2 v {} DIALECT 2\n",
                v(&[0.1, 0.2, 0.3]),
                v(&[0.3, 0.1, 0.2]),
                v(&[0.1, 0.2, 0.3]),
            ),
        },
        CreateTemplate {
            kind: kinds::INDEX,
            label: "Nuevo índice vectorial (documentos JSON)",
            template: "FT.CREATE {name} ON JSON PREFIX 1 {name}: SCHEMA $.etiqueta AS etiqueta TAG $.embedding AS embedding VECTOR HNSW 6 TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE\n\
                       JSON.SET {name}:1 $ \"{\\\"etiqueta\\\": \\\"a\\\", \\\"embedding\\\": [0.1, 0.2, 0.3]}\"\n\
                       JSON.SET {name}:2 $ \"{\\\"etiqueta\\\": \\\"b\\\", \\\"embedding\\\": [0.3, 0.1, 0.2]}\"\n\
                       FT.SEARCH {name} \"*=>[KNN 5 @embedding $v AS distancia]\" PARAMS 2 v \"\\xcd\\xcc\\xcc=\\xcd\\xccL>\\x9a\\x99\\x99>\" RETURN 2 etiqueta distancia DIALECT 2\n"
                .into(),
        },
    ];
    if server == Server::Redis {
        out.push(t(
            "Nuevo vector set de Redis 8",
            "VADD {name} VALUES 3 0.1 0.2 0.3 elemento-1 SETATTR \"{\\\"tipo\\\": \\\"a\\\"}\"\n\
             VADD {name} VALUES 3 0.3 0.1 0.2 elemento-2 SETATTR \"{\\\"tipo\\\": \\\"b\\\"}\"\n\
             VSIM {name} VALUES 3 0.1 0.2 0.3 WITHSCORES COUNT 10\n\
             VSIM {name} ELE elemento-1 FILTER \".tipo == \\\"b\\\"\" WITHSCORES WITHATTRIBS\n\
             VEMB {name} elemento-2\n",
        ));
    }
    out.extend([
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
    ]);
    out
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
        if c.data_type.trim().eq_ignore_ascii_case("vector") {
            return Err(Error::Query(format!(
                "El servidor no informa la configuración del campo vectorial {} (RediSearch anterior a 2.8 no la muestra en FT.INFO): indicá su tipo y dimensión, p. ej. VECTOR(3, FLOAT32), y su algoritmo y distancia.",
                c.name
            )));
        }
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
    if f.algorithm == "SVS-VAMANA" && !matches!(ty, VType::F32 | VType::F16) {
        return Err(Error::Query(format!(
            "El algoritmo SVS-VAMANA indexa vectores FLOAT32 o FLOAT16, no {}: cambiá el tipo de {} o el algoritmo.",
            ty.name(),
            c.name
        )));
    }
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
        options: Vec::new(),
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
    if !ix.options.is_empty() {
        options.insert("index_options".into(), line_of(&ix.options));
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
    // Each vector field is a vector index; one whose settings the server
    // doesn't report too, with no settings (never an ordinary index).
    let indexes = ix
        .fields
        .iter()
        .filter(|f| f.kind == "VECTOR")
        .map(|f| IndexDef {
            name: f.attribute.clone(),
            columns: vec![f.attribute.clone()],
            kind: f.vector.as_ref().map(|v| v.algorithm.to_ascii_lowercase()),
            vector: Some(f.vector.as_ref().map(FtVector::index).unwrap_or_default()),
            ..Default::default()
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
    // The index-wide options, as one command line's arguments.
    let options = match opt(t, "index_options") {
        Some(o) => crate::command::parse_line(o)
            .map_err(|e| Error::Query(format!("Opciones del índice {}: {e}", t.name)))?
            .into_iter()
            .map(|a| String::from_utf8_lossy(&a).into_owned())
            .collect(),
        None => Vec::new(),
    };
    Ok(FtIndex {
        name: t.name.clone(),
        on_json: json,
        prefixes,
        filter: opt(t, "filter").map(str::to_string),
        options,
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

/// The type (and dimension, when known) a vector written to the field
/// `field` of the hash `key` takes: the column's own type when the caller
/// gives it as Redis spells it (`VECTOR(3, FLOAT16)`: the target's field),
/// else that of the search index that covers the key, if DBine has seen
/// one, else the type the caller gives in another engine's terms, else
/// `FLOAT32` (what vectors in hashes usually are) for a value that is a
/// JSON array of numbers. `None`: not a vector, written as it is.
fn hash_vector_type(
    known: &Known,
    key: &str,
    field: &str,
    v: &Value,
    typed: Option<Typed>,
) -> Result<Option<(VType, Option<u32>)>> {
    let is_vector = dbine_driver::vector::parse_value(v).is_some();
    if let Some(t) = typed.filter(|t| t.own) {
        return Ok(is_vector.then_some((t.ty, t.dims)));
    }
    Ok(match known.hash_vector(key, field).map_err(Error::Query)? {
        Some(f) => is_vector.then_some((f.ty, Some(f.dims))),
        None => match typed {
            Some(t) => is_vector.then_some((t.ty, None)),
            None => is_vector_value(v).then_some((VType::F32, None)),
        },
    })
}

/// A vector of `n` numbers where the index says `dims`: the server would
/// keep it but leave the document out of the index.
pub(crate) fn check_dims(n: usize, dims: Option<u32>, what: &str) -> Result<()> {
    match dims {
        Some(d) if d as usize != n => Err(Error::Query(format!(
            "El vector de {what} tiene {n} dimensiones y su índice lo declara de {d}: el servidor lo guardaría sin indexarlo."
        ))),
        _ => Ok(()),
    }
}

/// A vector value packed as `ty` once its length matches `dims`.
fn packed_checked(v: &Value, ty: VType, dims: Option<u32>, what: &str) -> Result<Option<Vec<u8>>> {
    let Some(nums) = dbine_driver::vector::parse_value(v) else {
        return Ok(None);
    };
    check_dims(nums.len(), dims, what)?;
    Ok(Some(ty.encode(&nums)))
}

/// What the columns of a browsed key say it is (see `columns` in the
/// session): a new row of its grid goes into that key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyShape {
    /// `element`, `vector`, `attributes`.
    VectorSet,
    /// `field`, `value`.
    Hash,
    /// `member`, `score`.
    SortedSet,
    /// A lone `value`: a list or a set (told apart on the server).
    Values,
}

fn key_shape(columns: &[String]) -> Option<KeyShape> {
    let within = |names: &[&str]| columns.iter().all(|c| names.contains(&c.as_str()));
    let has = |n: &str| columns.iter().any(|c| c == n);
    if has("element") && within(&["element", "vector", "attributes"]) {
        Some(KeyShape::VectorSet)
    } else if has("field") && within(&["field", "value"]) {
        Some(KeyShape::Hash)
    } else if has("member") && within(&["member", "score"]) {
        Some(KeyShape::SortedSet)
    } else if columns == ["value"] {
        Some(KeyShape::Values)
    } else {
        None
    }
}

/// Lua that adds a value to a list (`RPUSH`) or a set (`SADD`), whichever
/// the key is (ARGV[1]: the value).
const VALUE_ADD: &str = "local t = redis.call('TYPE', KEYS[1])['ok'] \
if t == 'list' then return redis.call('RPUSH', KEYS[1], ARGV[1]) end \
if t == 'set' then return redis.call('SADD', KEYS[1], ARGV[1]) end \
return redis.error_reply('solo se agregan valores a una lista o a un set: la key es ' .. t)";

/// New rows of a browsed key's grid, into that key: `VADD` (a vector
/// set's element, with its attributes), `HSET` (a hash field; a vector
/// field as the blob of its index's type), `ZADD`, or an `EVAL` that adds
/// to a list or a set.
fn key_inserts(
    key: &str,
    shape: KeyShape,
    columns: &[String],
    rows: &[(usize, &Vec<Value>)],
    known: &Known,
    types: &Types,
) -> Result<Vec<String>> {
    let get = |row: &Vec<Value>, c: &str| {
        columns
            .iter()
            .position(|x| x == c)
            .and_then(|i| row.get(i))
            .filter(|v| !v.is_null())
            .cloned()
    };
    let mut out = Vec::new();
    for (n, row) in rows {
        let need = |c: &str| {
            get(row, c)
                .ok_or_else(|| Error::Query(format!("La fila {n} necesita un valor en {c}.")))
        };
        match shape {
            KeyShape::VectorSet => {
                let element = cell_text(&need("element")?);
                let nums = get(row, "vector")
                    .as_ref()
                    .and_then(dbine_driver::vector::parse_value)
                    .ok_or_else(|| {
                        Error::Query(format!(
                            "La fila {n} necesita el vector del elemento, como [0.1, 0.2, 0.3]."
                        ))
                    })?;
                let mut a = vectors::vadd_args(key, &element, &nums, known.vset_flag(key));
                if let Some(attrs) = get(row, "attributes") {
                    a.push("SETATTR".into());
                    a.push(match attrs {
                        Value::String(t) => t,
                        other => other.to_string(),
                    });
                }
                out.push(line_of(&a));
            }
            KeyShape::Hash => {
                let field = cell_text(&need("field")?);
                let value = get(row, "value").unwrap_or(Value::String(String::new()));
                out.push(hset_line(key, &field, &value, known, types.get("value"))?);
            }
            KeyShape::SortedSet => {
                let member = cell_text(&need("member")?);
                let score = get(row, "score").map_or_else(|| "0".into(), |v| cell_text(&v));
                out.push(line(&["ZADD", key, &score, &member]));
            }
            KeyShape::Values => {
                let value = cell_text(&need("value")?);
                out.push(line(&["EVAL", VALUE_ADD, "1", key, &value]));
            }
        }
    }
    Ok(out)
}

/// `HSET key field value` for a hash field: a vector field (of an index
/// that covers the key, or a value whose type the caller gives as a
/// vector, see [`hash_vector_type`]) as the blob of its type, checked
/// against its dimension, with what the comment says of it.
fn hset_line(
    key: &str,
    field: &str,
    v: &Value,
    known: &Known,
    typed: Option<Typed>,
) -> Result<String> {
    let vector = match typed.filter(|t| t.own) {
        Some(t) => Some((t.ty, t.dims)),
        None => match known.hash_vector(key, field).map_err(Error::Query)? {
            Some(f) => Some((f.ty, Some(f.dims))),
            None => typed.map(|t| (t.ty, None)),
        },
    };
    match vector {
        Some((ty, dims)) => {
            if let Some(nums) = dbine_driver::vector::parse_value(v) {
                check_dims(nums.len(), dims, field)?;
            }
            Ok(match vector_arg(v, ty, field) {
                Some((arg, note)) => {
                    blob_line(&["HSET".into(), key.into(), field.into(), arg], Some(&note))
                }
                None => line(&["HSET", key, field, &cell_text(v)]),
            })
        }
        None => Ok(line(&["HSET", key, field, &cell_text(v)])),
    }
}

/// The key of a new document of `index`: `id` as given when one of the
/// index's prefixes starts it (or the index covers every key), else after
/// its first prefix, so the index takes it.
pub(crate) fn document_key(index: &FtIndex, id: &str) -> String {
    let prefixes: Vec<&String> = index.prefixes.iter().filter(|p| !p.is_empty()).collect();
    if prefixes.is_empty() || prefixes.iter().any(|p| id.starts_with(p.as_str())) {
        id.to_string()
    } else {
        format!("{}{id}", prefixes[0])
    }
}

/// A JSONPath DBine writes a value at in a new document: `$.a` or
/// `$.a.b` (plain names), as those names.
fn simple_path(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix("$.")?;
    let parts: Vec<&str> = rest.split('.').collect();
    parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then_some(parts)
}

/// A cell as the JSON a document holds: a vector field's numbers as an
/// array (checked against its dimension), a `NUMERIC` field's text as a
/// number, text that is a JSON array or object as that JSON.
fn json_value(field: Option<&FtField>, v: &Value, what: &str, own: Option<Typed>) -> Result<Value> {
    if let Some((_, dims)) = document_vector(field, own) {
        let nums = dbine_driver::vector::parse_value(v).ok_or_else(|| {
            Error::Query(format!(
                "El valor de {what} no es un vector: escribilo como [0.1, 0.2, 0.3]."
            ))
        })?;
        check_dims(nums.len(), dims, what)?;
        // The canonical text's numbers (`1`, not `1.0`).
        return Ok(serde_json::from_str(&dbine_driver::vector::text(nums)).unwrap_or(Value::Null));
    }
    Ok(match v {
        Value::String(t) if field.is_some_and(|f| f.kind == "NUMERIC") => t
            .trim()
            .parse::<f64>()
            .map_or_else(|_| v.clone(), dbine_driver::json_f64),
        Value::String(t) => serde_json::from_str::<Value>(t)
            .ok()
            .filter(|j| j.is_array() || j.is_object())
            .unwrap_or_else(|| v.clone()),
        other => other.clone(),
    })
}

/// The type and dimension a vector cell takes in a search index's
/// document: the column's own type when the caller gives it as Redis
/// spells it (`VECTOR(3, FLOAT16)`: the target's field, which wins as it
/// does for a hash, see [`hash_vector_type`]), else that of the index's
/// field. `None`: not a vector field.
fn document_vector(field: Option<&FtField>, own: Option<Typed>) -> Option<(VType, Option<u32>)> {
    own.map(|t| (t.ty, t.dims)).or_else(|| {
        field
            .and_then(|f| f.vector.as_ref())
            .map(|v| (v.ty, Some(v.dims)))
    })
}

/// A new JSON document from its cells (column, value, the index field it
/// is): each at its field's path (`$.a.b`, plain names) or at `$.<column>`,
/// as [`json_value`] writes it (`types`: as in [`insert_script`]).
pub(crate) fn json_document<'a>(
    cells: impl Iterator<Item = (&'a str, &'a Value, Option<&'a FtField>)>,
    types: &Types,
) -> Result<Value> {
    let mut doc = serde_json::Map::new();
    for (c, v, field) in cells {
        let path = field.map_or_else(|| format!("$.{c}"), |f| f.identifier.clone());
        let parts = simple_path(&path).ok_or_else(|| {
            Error::Unsupported(format!(
                "el campo {c} está en una ruta JSON compuesta ({path}): escribí ese documento con JSON.SET a mano"
            ))
        })?;
        let value = json_value(field, v, c, types.own(c))?;
        let (last, parents) = parts.split_last().expect("one part at least");
        let mut at = &mut doc;
        for p in parents {
            let slot = at
                .entry(p.to_string())
                .or_insert_with(|| Value::Object(Default::default()));
            if !slot.is_object() {
                *slot = Value::Object(Default::default());
            }
            at = slot.as_object_mut().expect("an object");
        }
        at.insert(last.to_string(), value);
    }
    Ok(Value::Object(doc))
}

/// New documents of a search index's results: `HSET` of each one's key
/// (`id`) with its fields (vector fields as the blob of their type), or on
/// a JSON index `JSON.SET` of the whole document.
fn index_inserts(
    name: &str,
    columns: &[String],
    rows: &[(usize, &Vec<Value>)],
    known: &Known,
    types: &Types,
) -> Result<Vec<String>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let index = known
        .index_for(name, types)
        .map_err(Error::Query)?
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "DBine no leyó todavía el índice {name}: abrilo de nuevo para que lea sus campos"
            ))
        })?;
    let id_at = columns.iter().position(|c| c == "id").ok_or_else(|| {
        Error::Query("Un documento nuevo necesita su key en la columna id.".into())
    })?;
    let mut out = Vec::new();
    for (n, row) in rows {
        let id = match row.get(id_at) {
            Some(Value::Null) | None => {
                return Err(Error::Query(format!(
                    "La fila {n} necesita un valor en id: la key del documento nuevo."
                )))
            }
            Some(v) => document_key(index, &cell_text(v)),
        };
        let cells = columns
            .iter()
            .zip(row.iter())
            .filter(|(c, v)| *c != "id" && !v.is_null())
            .map(|(c, v)| (c.as_str(), v, index.fields.iter().find(|f| f.is(c))));
        if index.on_json {
            let doc = json_document(cells, types)?;
            out.push(line(&["JSON.SET", &id, "$", &doc.to_string()]));
            continue;
        }
        let mut args: Vec<Arg> = vec!["HSET".into(), id.clone().into()];
        let mut notes = Vec::new();
        for (c, v, field) in cells {
            let hash_field = field.map_or(c, |f| f.identifier.as_str());
            args.push(hash_field.into());
            match document_vector(field, types.own(c)) {
                Some((ty, dims)) => {
                    let blob = packed_checked(v, ty, dims, c)?.ok_or_else(|| {
                        Error::Query(format!(
                            "El valor de {c} no es un vector: escribilo como [0.1, 0.2, 0.3]."
                        ))
                    })?;
                    if let Some((_, note)) = vector_arg(v, ty, hash_field) {
                        notes.push(note);
                    }
                    args.push(Arg::Blob(blob));
                }
                None => args.push(cell_text(v).into()),
            }
        }
        if args.len() == 2 {
            return Err(Error::Query(format!(
                "La fila {n} no tiene ningún campo con valor: Redis no guarda hashes vacíos."
            )));
        }
        out.push(blob_line(&args, Some(&notes.join("; "))));
    }
    Ok(out)
}

/// Rows as code that writes them. Into a search index (its results'
/// grid): new documents (see [`index_inserts`]). Into a browsed key whose
/// columns say what it is (a vector set's `element`/`vector`, a hash's
/// `field`/`value`…): into that key (see [`key_inserts`]). Otherwise (a
/// table's rows: imports, copies, data compare) each row as a hash
/// `HSET <target>:<first column's value> col value …`, null cells left
/// out. A vector (a JSON array of numbers) is written as the blob of the
/// type of the search index that covers the key (checked against its
/// dimension), or of `FLOAT32`. A row with no value at all (the grid's
/// probe) writes nothing.
///
/// `types` are the columns' types when the caller knows them (see
/// [`dbine_driver::Driver::insert_script_typed`]): a field Redis spells
/// `VECTOR(3, FLOAT16)` is written as that blob, whatever DBine read of
/// the open servers; another engine's vector type packs a vector no index
/// covers as its element type; and of two open servers with different
/// indexes of the same name, the one of those types is the script's.
pub fn insert_script(
    target: &ObjectRef,
    columns: &[String],
    rows: &[Vec<Value>],
    known: &Known,
    types: &Types,
) -> Result<String> {
    if columns.is_empty() {
        return Ok(String::new());
    }
    let rows: Vec<(usize, &Vec<Value>)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.iter().all(Value::is_null))
        .map(|(i, r)| (i + 1, r))
        .collect();
    let finish = |out: Vec<String>| out.into_iter().map(|l| l + "\n").collect::<String>();
    if target.kind == kinds::INDEX {
        return index_inserts(&target.name, columns, &rows, known, types).map(finish);
    }
    if let Some(shape) = (target.kind == kinds::KEY)
        .then(|| key_shape(columns))
        .flatten()
    {
        return key_inserts(&target.name, shape, columns, &rows, known, types).map(finish);
    }
    let mut out = Vec::new();
    for (n, row) in rows {
        let id = match row.first() {
            Some(Value::Null) | None => {
                return Err(Error::Query(format!(
                    "La fila {n} no tiene valor en la primera columna ({}), que forma el nombre de la key.",
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
            let blob = match hash_vector_type(known, &key, c, v, types.get(c))? {
                Some((t, dims)) => packed_checked(v, t, dims, c)?,
                None => None,
            };
            match blob {
                Some(blob) => args.push(Arg::Blob(blob)),
                None => args.push(cell_text(v).into()),
            }
        }
        out.push(blob_line(&args, None));
    }
    Ok(finish(out))
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
/// `HSET` / `HDEL` on a hash (a vector field as the blob of its type: the
/// column's own `VECTOR(n, TYPE)` when `types` has it, else the index
/// field's, see [`document_vector`]), `JSON.SET` / `JSON.DEL` on a JSON
/// document, by the document's key (`id`).
fn index_edits(index: &FtIndex, changes: &[RowChange], types: &Types) -> Result<Vec<String>> {
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
                // A vector as an array of numbers (of the index's
                // dimension), any other value as JSON; text that is JSON
                // goes as that JSON.
                let json = json_value(field, v, col, types.own(col))?;
                out.push(line(&["JSON.SET", &id, &path, &json.to_string()]));
                continue;
            }
            let name = field.map_or(col.as_str(), |f| f.identifier.as_str());
            if v.is_null() {
                out.push(line(&["HDEL", &id, name]));
                continue;
            }
            match document_vector(field, types.own(col)) {
                Some((ty, dims)) => {
                    let (arg, note) = vector_arg(v, ty, name).ok_or_else(|| {
                        Error::Query(format!(
                            "El valor de {col} no es un vector: escribilo como [0.1, 0.2, 0.3]."
                        ))
                    })?;
                    if let Some(nums) = dbine_driver::vector::parse_value(v) {
                        check_dims(nums.len(), dims, col)?;
                    }
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
                    let flag = known.vset_flag(key);
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

/// `types`: the edited columns' types, when the caller knows them (as in
/// [`insert_script`]).
pub fn update_script(
    target: &ObjectRef,
    changes: &[RowChange],
    known: &Known,
    types: &Types,
) -> Result<String> {
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
        let index = known
            .index_for(key, types)
            .map_err(Error::Query)?
            .ok_or_else(|| {
                Error::Unsupported(format!(
                "DBine no leyó todavía el índice {key}: abrilo de nuevo para que lea sus campos"
            ))
            })?;
        return index_edits(index, changes, types).map(finish);
    }
    if changes
        .iter()
        .any(|c| c.key.iter().any(|(k, _)| k == "element"))
    {
        return vectorset_edits(key, changes, known).map(finish);
    }
    let mut out: Vec<String> = Vec::new();
    // A hash field that holds a vector: its blob, with what the comment says.
    let hset = |field: &str, v: &Value| hset_line(key, field, v, known, types.get("value"));
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
                (None, Some(v)) => out.push(hset(&field, &v)?),
                (Some(nf), Some(v)) => {
                    out.push(line(&["HDEL", key, &field]));
                    out.push(hset(&nf, &v)?);
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
        super::insert_script(target, columns, rows, &Known::default(), &Types::default())
    }
    fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        super::update_script(target, changes, &Known::default(), &Types::default())
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
            options: vec!["LANGUAGE".into(), "spanish".into(), "NOOFFSETS".into()],
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

    /// The index-wide options go with the index, before SCHEMA, and a
    /// vector field whose settings the server didn't report stays a vector
    /// index (scripting it asks for them).
    #[test]
    fn index_options_and_unreported_vectors() {
        let ix = redis_index();
        assert!(
            ix.create_command()
                .contains("FILTER \"@lang==\\\"es\\\"\" LANGUAGE spanish NOOFFSETS SCHEMA title"),
            "{}",
            ix.create_command()
        );
        let t = index_schema(&ix);
        assert_eq!(t.options["index_options"], "LANGUAGE spanish NOOFFSETS");
        let mut old = ix.clone();
        old.options.clear();
        old.fields[1].vector = None;
        let t = index_schema(&old);
        assert_eq!(t.columns[1].data_type, "VECTOR");
        assert_eq!(t.indexes.len(), 1);
        assert!(t.indexes[0].vector.is_some());
        let e = schema_index(&t, Server::Redis).unwrap_err().to_string();
        assert!(e.contains("no informa la configuración"), "{e}");
        assert_eq!(old.unreported_vectors(), ["embedding"]);
    }

    fn target(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// SVS-VAMANA indexes FLOAT32 and FLOAT16 vectors only: the DDL says
    /// so before the server does.
    #[test]
    fn svs_vamana_types() {
        let c = |ty: &str| ColumnDef {
            name: "emb".into(),
            data_type: ty.into(),
            ..Default::default()
        };
        let svs = VectorIndex {
            algorithm: Some("svs-vamana".into()),
            ..Default::default()
        };
        for ty in ["VECTOR(3, FLOAT32)", "VECTOR(3, FLOAT16)"] {
            assert!(
                vector_field(&c(ty), Some(&svs), Server::Redis).is_ok(),
                "{ty}"
            );
        }
        for ty in [
            "VECTOR(3, FLOAT64)",
            "VECTOR(3, BFLOAT16)",
            "VECTOR(3, INT8)",
            "VECTOR(3, UINT8)",
        ] {
            let e = vector_field(&c(ty), Some(&svs), Server::Redis).unwrap_err();
            assert!(
                e.to_string()
                    .contains("SVS-VAMANA indexa vectores FLOAT32 o FLOAT16"),
                "{ty}: {e}"
            );
        }
    }

    /// "Agregar fila" on a browsed key writes into that key, by what its
    /// columns say it is.
    #[test]
    fn new_rows_of_a_browsed_key() {
        let mut known = Known::default();
        // A vector set: VADD with its attributes (and its quantization
        // once read).
        let vs = target(kinds::KEY, "vs");
        let cols = strs(&["element", "vector", "attributes"]);
        let rows = vec![vec![
            Value::from("a b"),
            Value::from("[0.1, 2]"),
            Value::from("{\"lang\":\"es\"}"),
        ]];
        assert_eq!(
            parsed(&super::insert_script(&vs, &cols, &rows, &known, &Types::default()).unwrap()),
            [vec![
                "VADD",
                "vs",
                "VALUES",
                "2",
                "0.1",
                "2",
                "a b",
                "SETATTR",
                "{\"lang\":\"es\"}"
            ]]
        );
        known.put_vset("t", 0, "vs", "NOQUANT");
        let only = vec![vec![Value::from("c"), serde_json::json!([1, 0])]];
        assert_eq!(
            parsed(
                &super::insert_script(
                    &vs,
                    &strs(&["element", "vector"]),
                    &only,
                    &known,
                    &Types::default()
                )
                .unwrap()
            ),
            [vec!["VADD", "vs", "VALUES", "2", "1", "0", "c", "NOQUANT"]]
        );
        let no_vector = vec![vec![Value::from("d"), Value::Null, Value::Null]];
        assert!(super::insert_script(&vs, &cols, &no_vector, &known, &Types::default()).is_err());
        // The grid's probe: nothing to write, and no error.
        assert_eq!(
            super::insert_script(
                &vs,
                &cols,
                &[vec![Value::Null; 3]],
                &known,
                &Types::default()
            )
            .unwrap(),
            ""
        );
        // A hash: HSET of the key itself; a vector field as its index's blob.
        let mut ix = redis_index();
        ix.fields[1].identifier = "emb".into();
        known.put_index("t", 0, &ix);
        let h = target(kinds::KEY, "doc:9");
        let s = super::insert_script(
            &h,
            &strs(&["field", "value"]),
            &[
                vec![Value::from("title"), Value::from("hola")],
                vec![Value::from("emb"), Value::from("[1,2,3,4]")],
            ],
            &known,
            &Types::default(),
        )
        .unwrap();
        let p = parse_script(&s).unwrap();
        assert_eq!(
            p[0],
            [
                b"HSET".to_vec(),
                b"doc:9".to_vec(),
                b"title".to_vec(),
                b"hola".to_vec()
            ]
        );
        assert_eq!(p[1][3], VType::F16.encode(&[1.0, 2.0, 3.0, 4.0]));
        let e = super::insert_script(
            &h,
            &strs(&["field", "value"]),
            &[vec![Value::from("emb"), Value::from("[1,2]")]],
            &known,
            &Types::default(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("2 dimensiones"), "{e}");
        // A sorted set and a list or set.
        assert_eq!(
            parsed(
                &super::insert_script(
                    &target(kinds::KEY, "z"),
                    &strs(&["member", "score"]),
                    &[vec![Value::from("ana"), Value::from(3.5)]],
                    &known,
                    &Types::default()
                )
                .unwrap()
            ),
            [vec!["ZADD", "z", "3.5", "ana"]]
        );
        let s = super::insert_script(
            &target(kinds::KEY, "l"),
            &strs(&["value"]),
            &[vec![Value::from("x")]],
            &known,
            &Types::default(),
        )
        .unwrap();
        let p = parsed(&s);
        assert_eq!(
            (p[0][0].as_str(), p[0][3].as_str(), p[0][4].as_str()),
            ("EVAL", "l", "x")
        );
        // Rows of a table (an import): hashes <name>:<id>, as before.
        assert_eq!(
            parsed(
                &super::insert_script(
                    &target(kinds::TABLE, "person"),
                    &strs(&["id", "name"]),
                    &[vec![Value::from(1), Value::from("Ana")]],
                    &known,
                    &Types::default()
                )
                .unwrap()
            ),
            [vec!["HSET", "person:1", "id", "1", "name", "Ana"]]
        );
    }

    /// "Agregar fila" on a search index's results: a new document under
    /// the index's prefix.
    #[test]
    fn new_documents_of_a_search_index() {
        let idx = target(kinds::INDEX, "docs");
        let cols = strs(&["id", "title", "embedding", "__embedding_score"]);
        let row = vec![
            Value::from("7"),
            Value::from("Hola"),
            Value::from("[1, 0, 0, 0.5]"),
            Value::Null,
        ];
        // Not read yet: it doesn't guess.
        assert!(insert_script(&idx, &cols, std::slice::from_ref(&row)).is_err());
        assert_eq!(
            insert_script(&idx, &cols, &[vec![Value::Null; 4]]).unwrap(),
            ""
        );
        let mut known = Known::default();
        known.put_index("t", 0, &redis_index());
        let s = super::insert_script(
            &idx,
            &cols,
            std::slice::from_ref(&row),
            &known,
            &Types::default(),
        )
        .unwrap();
        let p = parse_script(&s).unwrap();
        // `7` isn't under a prefix of the index: it goes under the first.
        assert_eq!(p[0][1], b"doc:7".to_vec());
        assert_eq!(p[0][2], b"title".to_vec());
        assert_eq!(p[0][4], b"emb".to_vec());
        assert_eq!(p[0][5], VType::F16.encode(&[1.0, 0.0, 0.0, 0.5]));
        assert_eq!(p[0].len(), 6);
        // A key under a prefix stays as it is; a vector of another
        // dimension is refused.
        let art = vec![
            Value::from("art:1"),
            Value::from("x"),
            Value::Null,
            Value::Null,
        ];
        assert_eq!(
            parse_script(
                &super::insert_script(&idx, &cols, &[art], &known, &Types::default()).unwrap()
            )
            .unwrap()[0][1],
            b"art:1".to_vec()
        );
        let bad = vec![
            Value::from("doc:2"),
            Value::Null,
            Value::from("[1,2]"),
            Value::Null,
        ];
        assert!(super::insert_script(&idx, &cols, &[bad], &known, &Types::default()).is_err());
        // A JSON index: the whole document, at the fields' paths.
        let mut j = redis_index();
        j.on_json = true;
        j.fields[0].identifier = "$.meta.title".into();
        j.fields[1].identifier = "$.emb".into();
        known.put_index("t", 0, &j);
        let s = super::insert_script(
            &idx,
            &cols,
            std::slice::from_ref(&row),
            &known,
            &Types::default(),
        )
        .unwrap();
        let got = parsed(&s);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][..3], ["JSON.SET", "doc:7", "$"]);
        // The document's key order follows serde_json's map, which a
        // workspace build may switch to insertion order: compare values.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&got[0][3]).unwrap(),
            serde_json::json!({"emb": [1, 0, 0, 0.5], "meta": {"title": "Hola"}})
        );
        j.fields[0].identifier = "$.tags[*]".into();
        known.put_index("t", 0, &j);
        assert!(super::insert_script(&idx, &cols, &[row], &known, &Types::default()).is_err());
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
        // A search index over doc: says FLOAT16, of 4 dimensions.
        let mut known = Known::default();
        let mut ix = redis_index();
        ix.fields[1].identifier = "emb".into();
        known.put_index("t", 0, &ix);
        let e = super::insert_script(&target, &cols, &rows, &known, &Types::default()).unwrap_err();
        assert!(e.to_string().contains("3 dimensiones"), "{e}");
        let rows4 = vec![vec![
            Value::from(1),
            serde_json::json!([0.5, 1, -2, 0]),
            serde_json::json!(["x"]),
        ]];
        let s = super::insert_script(&target, &cols, &rows4, &known, &Types::default()).unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][5],
            VType::F16.encode(&[0.5, 1.0, -2.0, 0.0])
        );
        // A row of nothing (the grid's probe) writes nothing.
        assert_eq!(
            super::insert_script(
                &target,
                &cols,
                &[vec![Value::Null; 3]],
                &known,
                &Types::default()
            )
            .unwrap(),
            ""
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
        let s = super::update_script(&hash, std::slice::from_ref(&ch), &known, &Types::default())
            .unwrap();
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
        known.put_index("t", 0, &redis_index());
        let s = super::update_script(
            &idx,
            &[ch(vec![
                ("title", Value::from("Nuevo")),
                ("embedding", serde_json::json!([1, 0, 0, 0.5])),
                ("other", Value::Null),
            ])],
            &known,
            &Types::default(),
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
        known.put_index("t", 0, &j);
        let s = super::update_script(
            &idx,
            &[ch(vec![("embedding", serde_json::json!([1, 2, 3, 4]))])],
            &known,
            &Types::default(),
        )
        .unwrap();
        assert_eq!(
            parsed(&s),
            [vec!["JSON.SET", "doc:1", "$.emb", "[1,2,3,4]"]]
        );
        assert!(super::update_script(
            &idx,
            &[ch(vec![("id", Value::from("x"))])],
            &known,
            &Types::default()
        )
        .is_err());
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
            parsed(
                &super::update_script(&vs, std::slice::from_ref(&ch), &known, &Types::default())
                    .unwrap()
            ),
            [
                vec!["VADD", "vs", "VALUES", "2", "0.1", "2", "a b"],
                vec!["VSETATTR", "vs", "a b", "{\"lang\":\"es\"}"],
            ]
        );
        // The set's quantization, once read, goes with VADD.
        known.put_vset("t", 0, "vs", "NOQUANT");
        let s = super::update_script(&vs, std::slice::from_ref(&ch), &known, &Types::default())
            .unwrap();
        assert!(
            s.starts_with("VADD vs VALUES 2 0.1 2 \"a b\" NOQUANT\n"),
            "{s}"
        );
        let none = RowChange {
            set: vec![("vector".into(), Value::Null)],
            ..ch.clone()
        };
        assert!(super::update_script(&vs, &[none], &known, &Types::default()).is_err());
        assert_eq!(
            parsed(&delete_script(&vs, &[vec![("element".into(), Value::from("a b"))]]).unwrap()),
            [vec!["VREM", "vs", "a b"]]
        );
    }

    #[test]
    fn templates_parse() {
        for server in [Server::Redis, Server::Valkey, Server::Dragonfly] {
            let ts = templates(server);
            assert!(ts.len() >= 7);
            assert_eq!(
                ts.iter().any(|t| t.label.contains("vector set")),
                server == Server::Redis
            );
            for t in ts {
                let text = t.template.replace("{name}", "demo");
                assert!(!parse_script(&text).unwrap().is_empty(), "{}", t.label);
            }
        }
        // The vectors in them are what their comments say.
        let ts = templates(Server::Redis);
        let p = parse_script(&ts[0].template).unwrap();
        assert_eq!(p[1][5], VType::F32.encode(&[0.1, 0.2, 0.3]));
        let p = parse_script(&ts[1].template).unwrap();
        assert_eq!(p[3][6], VType::F32.encode(&[0.1, 0.2, 0.3]));
        assert_eq!(
            p[1][3],
            b"{\"etiqueta\": \"a\", \"embedding\": [0.1, 0.2, 0.3]}".to_vec()
        );
    }

    fn typed(cols: &[(&str, &str)]) -> Vec<dbine_driver::ResultColumn> {
        cols.iter()
            .map(|(n, t)| dbine_driver::ResultColumn::new(*n, *t))
            .collect()
    }

    /// With the columns' types: Redis' own spelling decides the blob (and
    /// the dimension), over what DBine read of the open servers; another
    /// engine's vector type only packs a vector no index covers.
    #[test]
    fn typed_scripts_take_the_blob_type_from_the_column() {
        let target = ObjectRef {
            kind: kinds::KEY.into(),
            schema: None,
            name: "doc".into(),
        };
        let cols = vec!["id".to_string(), "emb".into()];
        let types = Types::of(&typed(&[("id", "key"), ("emb", "VECTOR(3, FLOAT16)")]));
        let rows = vec![vec![Value::from(1), Value::from("[0.5,1,-2]")]];
        // Nothing read: the column's FLOAT16, from text too.
        let s = super::insert_script(&target, &cols, &rows, &Known::default(), &types).unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][5],
            VType::F16.encode(&[0.5, 1.0, -2.0])
        );
        // Two open servers disagree on doc:'s field: the type settles it.
        let mut known = Known::default();
        let mut a = redis_index();
        a.fields[1].identifier = "emb".into();
        a.fields[1].vector.as_mut().unwrap().dims = 3;
        let mut b = a.clone();
        b.fields[1].vector.as_mut().unwrap().ty = VType::F32;
        known.put_index("a:1", 0, &a);
        known.put_index("b:2", 0, &b);
        assert!(super::insert_script(&target, &cols, &rows, &known, &Types::default()).is_err());
        let s = super::insert_script(&target, &cols, &rows, &known, &types).unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][5],
            VType::F16.encode(&[0.5, 1.0, -2.0])
        );
        // The column's dimension is checked.
        let four = vec![vec![Value::from(1), serde_json::json!([1, 2, 3, 4])]];
        let e = super::insert_script(&target, &cols, &four, &Known::default(), &types).unwrap_err();
        assert!(e.to_string().contains("4 dimensiones"), "{e}");
        // Another engine's type: the covering index first…
        let pg = Types::of(&typed(&[("emb", "halfvec(3)")]));
        let mut only_a = Known::default();
        let mut f32_ix = a.clone();
        f32_ix.fields[1].vector.as_mut().unwrap().ty = VType::F32;
        only_a.put_index("a:1", 0, &f32_ix);
        let s = super::insert_script(&target, &cols, &rows, &only_a, &pg).unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][5],
            VType::F32.encode(&[0.5, 1.0, -2.0])
        );
        // …else its element (text included, which untyped stays text).
        let s = super::insert_script(&target, &cols, &rows, &Known::default(), &pg).unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][5],
            VType::F16.encode(&[0.5, 1.0, -2.0])
        );
        let s = insert_script(&target, &cols, &rows).unwrap();
        assert_eq!(
            parsed(&s),
            [vec!["HSET", "doc:1", "id", "1", "emb", "[0.5,1,-2]"]]
        );
        // A result column marked as a vector (no Redis spelling) is a hint too.
        let meta = vec![
            dbine_driver::ResultColumn::new("id", ""),
            dbine_driver::ResultColumn::new("emb", "").with_vector(
                dbine_driver::VectorMeta::dense(Some(3), dbine_driver::VectorElement::I8),
            ),
        ];
        let s = super::insert_script(&target, &cols, &rows, &Known::default(), &Types::of(&meta))
            .unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][5],
            VType::I8.encode(&[0.5, 1.0, -2.0])
        );
        // A hash grid's value column of a vector type, edited.
        let hash = ObjectRef {
            name: "doc:7".into(),
            ..target.clone()
        };
        let ch = RowChange {
            key: vec![("field".into(), Value::from("emb"))],
            set: vec![("value".into(), Value::from("[1,2,3]"))],
            ..Default::default()
        };
        let value_bf16 = Types::of(&typed(&[
            ("field", "string"),
            ("value", "VECTOR(3, BFLOAT16)"),
        ]));
        let s =
            super::update_script(&hash, std::slice::from_ref(&ch), &known, &value_bf16).unwrap();
        assert!(s.trim_end().ends_with("# emb: BFLOAT16 [1,2,3]"), "{s}");
        assert_eq!(
            parse_script(&s).unwrap()[0][3],
            VType::Bf16.encode(&[1.0, 2.0, 3.0])
        );
        // A plain string value column changes nothing.
        let plain = Types::of(&typed(&[("field", "string"), ("value", "string")]));
        let s = super::update_script(&hash, std::slice::from_ref(&ch), &only_a, &plain).unwrap();
        assert!(s.trim_end().ends_with("# emb: FLOAT32 [1,2,3]"), "{s}");
    }

    /// Two open servers with different indexes of the same name: the
    /// result's types say which one a new or edited document is for.
    #[test]
    fn typed_scripts_pick_the_index_of_their_types() {
        let idx = ObjectRef {
            kind: kinds::INDEX.into(),
            schema: None,
            name: "docs".into(),
        };
        let mut known = Known::default();
        let a = redis_index();
        let mut b = a.clone();
        b.fields[1].vector.as_mut().unwrap().ty = VType::F32;
        known.put_index("a:1", 0, &a);
        known.put_index("b:2", 0, &b);
        let cols = vec!["id".to_string(), "title".into(), "embedding".into()];
        let row = vec![
            Value::from("doc:9"),
            Value::from("Hola"),
            serde_json::json!([1, 0, 0, 0.5]),
        ];
        let e = super::insert_script(
            &idx,
            &cols,
            std::slice::from_ref(&row),
            &known,
            &Types::default(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("conexiones abiertas"), "{e}");
        for (spelling, ty) in [
            ("VECTOR(4, FLOAT32)", VType::F32),
            ("VECTOR(4, FLOAT16)", VType::F16),
        ] {
            let types = Types::of(&typed(&[
                ("id", "key"),
                ("title", "TEXT"),
                ("embedding", spelling),
            ]));
            let s = super::insert_script(&idx, &cols, std::slice::from_ref(&row), &known, &types)
                .unwrap();
            let p = parse_script(&s).unwrap();
            assert_eq!(
                p[0][..3],
                [b"HSET".to_vec(), b"doc:9".to_vec(), b"title".to_vec()]
            );
            assert_eq!(p[0][5], ty.encode(&[1.0, 0.0, 0.0, 0.5]), "{spelling}");
            let ch = RowChange {
                key: vec![("id".into(), Value::from("doc:9"))],
                set: vec![("embedding".into(), serde_json::json!([0, 1, 0, 0]))],
                ..Default::default()
            };
            let s = super::update_script(&idx, &[ch], &known, &types).unwrap();
            assert_eq!(
                parse_script(&s).unwrap()[0][3],
                ty.encode(&[0.0, 1.0, 0.0, 0.0])
            );
        }
        // Types that match neither: as without types.
        let other = Types::of(&typed(&[("embedding", "VECTOR(4, INT8)")]));
        assert!(super::insert_script(&idx, &cols, &[row], &known, &other).is_err());
    }

    /// The only index DBine read (of another server, say: the types came
    /// from a cached describe) contradicts the columns' own types: the
    /// column's `VECTOR(n, TYPE)` still decides the blob and the dimension,
    /// as it does for a hash key; the index gives only the key and the
    /// fields' names.
    #[test]
    fn typed_scripts_keep_their_types_against_the_one_known_index() {
        let idx = ObjectRef {
            kind: kinds::INDEX.into(),
            schema: None,
            name: "docs".into(),
        };
        let mut known = Known::default();
        let mut other_server = redis_index();
        other_server.fields[1].vector.as_mut().unwrap().ty = VType::F32;
        known.put_index("b:2", 0, &other_server);
        let cols = vec!["id".to_string(), "title".into(), "embedding".into()];
        let row = vec![
            Value::from("doc:9"),
            Value::from("Hola"),
            serde_json::json!([1, 0, 0, 0.5]),
        ];
        let own = Types::of(&typed(&[("embedding", "VECTOR(4, FLOAT16)")]));
        let s =
            super::insert_script(&idx, &cols, std::slice::from_ref(&row), &known, &own).unwrap();
        let p = parse_script(&s).unwrap();
        assert_eq!(p[0][4], b"emb".to_vec());
        assert_eq!(p[0][5], VType::F16.encode(&[1.0, 0.0, 0.0, 0.5]));
        let ch = RowChange {
            key: vec![("id".into(), Value::from("doc:9"))],
            set: vec![("embedding".into(), serde_json::json!([0, 1, 0, 0]))],
            ..Default::default()
        };
        let s = super::update_script(&idx, std::slice::from_ref(&ch), &known, &own).unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][3],
            VType::F16.encode(&[0.0, 1.0, 0.0, 0.0])
        );
        // Without types, the index's.
        let s = super::insert_script(
            &idx,
            &cols,
            std::slice::from_ref(&row),
            &known,
            &Types::default(),
        )
        .unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0][5],
            VType::F32.encode(&[1.0, 0.0, 0.0, 0.5])
        );
        // The column's dimension, not the index's.
        let three = Types::of(&typed(&[("embedding", "VECTOR(3, FLOAT16)")]));
        let e = super::insert_script(&idx, &cols, std::slice::from_ref(&row), &known, &three)
            .unwrap_err();
        assert!(e.to_string().contains("4 dimensiones"), "{e}");
        let e = super::update_script(&idx, std::slice::from_ref(&ch), &known, &three).unwrap_err();
        assert!(e.to_string().contains("4 dimensiones"), "{e}");
        // A JSON index: the vector's array, of the column's dimension.
        let mut json = redis_index();
        json.on_json = true;
        json.fields[0].identifier = "$.title".into();
        json.fields[1].identifier = "$.emb".into();
        let mut known = Known::default();
        known.put_index("b:2", 0, &json);
        let short = vec![
            Value::from("doc:9"),
            Value::from("Hola"),
            serde_json::json!([1, 0, 0]),
        ];
        let s = super::insert_script(&idx, &cols, std::slice::from_ref(&short), &known, &three)
            .unwrap();
        let doc: Value = serde_json::from_slice(&parse_script(&s).unwrap()[0][3]).unwrap();
        assert_eq!(doc["emb"], serde_json::json!([1, 0, 0]), "{s}");
        assert!(super::insert_script(
            &idx,
            &cols,
            std::slice::from_ref(&short),
            &known,
            &Types::default()
        )
        .is_err());
        let ch = RowChange {
            key: vec![("id".into(), Value::from("doc:9"))],
            set: vec![("embedding".into(), serde_json::json!([0, 1, 0]))],
            ..Default::default()
        };
        let s = super::update_script(&idx, &[ch], &known, &three).unwrap();
        assert_eq!(
            parse_script(&s).unwrap()[0],
            [
                b"JSON.SET".to_vec(),
                b"doc:9".to_vec(),
                b"$.emb".to_vec(),
                b"[0,1,0]".to_vec()
            ]
        );
    }
}

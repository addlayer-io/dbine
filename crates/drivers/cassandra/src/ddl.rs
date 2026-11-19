//! The table designer, CQL DDL, create templates and insert scripts.
//!
//! Keys come from column options, not from a primary key list: columns
//! marked `partition_key` form the partition key and those marked
//! `clustering_key` the clustering columns, both in column order, each
//! clustering column with its `clustering_order`. When no column is marked,
//! the table's `primary_key` (if any) is read as CQL does: its first column
//! is the partition key, the rest are clustering columns.
//!
//! Rows are inserted with `INSERT INTO … JSON '…'`: Cassandra converts
//! each JSON value to the column's type (uuid, timestamp, blob `0x…`,
//! decimal…), which plain literals can't do without knowing the types.
//! Edited rows are `UPDATE` with `fromJson('…')` values for the same reason.
//! A vector comes as a JSON array and goes the same way: `INSERT … JSON`
//! and `fromJson('[0.1,0.2]')` take it for a `vector<float, n>`.
//!
//! Vector indexes ([`IndexDef::vector`]) are SAI on Cassandra (`USING
//! 'sai' WITH OPTIONS = {'similarity_function': …}`) and `vector_index`
//! on ScyllaDB (`CREATE CUSTOM INDEX … USING 'vector_index'`, with its
//! graph and quantization options and filter columns).

use crate::cql::{ident, qualified};
use crate::Flavor;
use dbine_driver::filter::{insert_where, ColumnFilter, FilterOp};
use dbine_driver::vector::{metric, param};
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef,
    ObjectRef, Result, RowChange, TableSchema, VectorIndex, VectorIndexSpec, VectorParamSpec,
};
use serde_json::Value;
use std::collections::BTreeMap;

const DATA_TYPES: &[&str] = &[
    "text",
    "varchar",
    "ascii",
    "int",
    "bigint",
    "smallint",
    "tinyint",
    "varint",
    "float",
    "double",
    "decimal",
    "boolean",
    "uuid",
    "timeuuid",
    "timestamp",
    "date",
    "time",
    "duration",
    "inet",
    "blob",
    "counter",
    "list<text>",
    "set<text>",
    "map<text, text>",
    "frozen<list<int>>",
    "tuple<int, text>",
];

pub fn is_scylla(driver_id: &str) -> bool {
    driver_id == "scylladb"
}

/// Amazon Keyspaces: no materialized views, secondary indexes, UDFs or
/// compaction settings (the service manages storage).
pub fn is_keyspaces(driver_id: &str) -> bool {
    driver_id == "keyspaces"
}

pub fn designer(driver_id: &str) -> DesignerSpec {
    let mut data_types = DATA_TYPES.to_vec();
    let mut compaction = vec![
        ("SizeTieredCompactionStrategy", "Size-tiered (STCS)"),
        ("LeveledCompactionStrategy", "Leveled (LCS)"),
        ("TimeWindowCompactionStrategy", "Time window (TWCS)"),
    ];
    if is_scylla(driver_id) {
        data_types.push("vector<float, 3>");
        compaction.push(("IncrementalCompactionStrategy", "Incremental (ICS)"));
    } else if !is_keyspaces(driver_id) {
        data_types.push("vector<float, 3>");
        compaction.push(("UnifiedCompactionStrategy", "Unified (UCS, Cassandra 5)"));
    }
    DesignerSpec {
        kind: kinds::TABLE,
        label: "Nueva tabla",
        data_types,
        schemas: false,
        // The key is set per column (partition / clustering), below.
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        indexes: !is_keyspaces(driver_id),
        foreign_keys: false,
        column_options: vec![
            Field::new("partition_key", "Clave de partición", FieldKind::Bool).help(
                "Las columnas marcadas forman la clave de partición, en el orden de la lista.",
            ),
            Field::new("clustering_key", "Clave de clustering", FieldKind::Bool)
                .help("Ordenan las filas dentro de la partición, en el orden de la lista."),
            Field::new(
                "clustering_order",
                "Orden de clustering",
                FieldKind::Select(vec![("ASC", "Ascendente"), ("DESC", "Descendente")]),
            )
            .default_value("ASC"),
            Field::new("static", "Static", FieldKind::Bool)
                .help("Un valor por partición (requiere claves de clustering)."),
        ],
        table_options: if is_keyspaces(driver_id) {
            vec![
                Field::new(
                    "default_time_to_live",
                    "TTL por defecto (segundos)",
                    FieldKind::Number,
                )
                .placeholder("0 (sin vencimiento)")
                .help("En Amazon Keyspaces requiere TTL habilitado en la tabla."),
                Field::new("comment", "Comentario", FieldKind::Text),
            ]
        } else {
            vec![
                Field::new(
                    "default_time_to_live",
                    "TTL por defecto (segundos)",
                    FieldKind::Number,
                )
                .placeholder("0 (sin vencimiento)"),
                Field::new("compaction", "Compactación", FieldKind::Select(compaction)),
                Field::new("gc_grace_seconds", "gc_grace_seconds", FieldKind::Number)
                    .placeholder("864000"),
                Field::new("comment", "Comentario", FieldKind::Text),
            ]
        },
        columns_required: true,
        vector_indexes: vector_spec(driver_id),
    }
}

/// What a vector index can be in the designer. Cassandra's SAI takes only
/// the distance (its graph settings need a server flag,
/// `SAI_VECTOR_ALLOW_CUSTOM_PARAMETERS`, so they're kept when read but not
/// offered); ScyllaDB's `vector_index` also takes the graph's settings, a
/// quantization and filter columns. Keyspaces has no vectors.
fn vector_spec(driver_id: &str) -> Option<VectorIndexSpec> {
    if is_keyspaces(driver_id) {
        return None;
    }
    let metrics = vec![metric::COSINE, metric::L2, metric::IP];
    if !is_scylla(driver_id) {
        return Some(VectorIndexSpec {
            algorithms: Vec::new(),
            metrics,
            params: Vec::new(),
            filter_columns: false,
        });
    }
    let p = |key: &'static str, default: &'static str| VectorParamSpec {
        key,
        algorithms: Vec::new(),
        default,
    };
    Some(VectorIndexSpec {
        algorithms: Vec::new(),
        metrics,
        params: vec![
            p(param::M, "16"),
            p(param::EF_CONSTRUCTION, "128"),
            p("search_beam_width", "128"),
            p(param::QUANTIZATION, "f32"),
            p("oversampling", "1.0"),
            p("rescoring", "false"),
        ],
        filter_columns: true,
    })
}

/// Standard distance names and CQL's `similarity_function`.
const SIMILARITIES: &[(&str, &str)] = &[
    (metric::COSINE, "COSINE"),
    (metric::L2, "EUCLIDEAN"),
    (metric::IP, "DOT_PRODUCT"),
];

/// Standard parameter names and the index options they are, on both
/// engines.
const PARAMS: &[(&str, &str)] = &[
    (param::M, "maximum_node_connections"),
    (param::EF_CONSTRUCTION, "construction_beam_width"),
    (param::QUANTIZATION, "quantization"),
];

/// Options of a vector index besides those (and `similarity_function`):
/// ScyllaDB's and DataStax's.
const VECTOR_OPTIONS: &[&str] = &[
    "search_beam_width",
    "oversampling",
    "rescoring",
    "source_model",
    "optimize_for",
];

/// Options a flavor's vector index takes, as named in `WITH OPTIONS`.
/// Cassandra's graph settings (`maximum_node_connections`,
/// `construction_beam_width`) are written when the index has them: an
/// index read from a server with `cassandra.sai.vector.allow_custom_parameters`
/// scripts back as it is. Schema conversion leaves them out of what comes
/// from another engine or from ScyllaDB (see dbine-schema's CQL dialect),
/// and the designer doesn't offer them.
fn takes_option(flavor: Flavor, option: &str) -> bool {
    match flavor {
        Flavor::Scylla => !matches!(option, "source_model" | "optimize_for"),
        _ => !matches!(
            option,
            "search_beam_width" | "oversampling" | "rescoring" | "quantization"
        ),
    }
}

/// A `similarity_function` as the standard name (an unknown one as the
/// engine spells it, lower case).
pub(crate) fn metric_of(similarity: &str) -> String {
    let s = similarity.trim();
    SIMILARITIES
        .iter()
        .find(|(_, cql)| cql.eq_ignore_ascii_case(s))
        .map_or_else(|| s.to_ascii_lowercase(), |(m, _)| m.to_string())
}

/// The `similarity_function` of a metric: a standard name or CQL's own.
fn similarity_of(m: &str) -> Result<&'static str> {
    let m = m.trim();
    SIMILARITIES
        .iter()
        .find(|(std, cql)| std.eq_ignore_ascii_case(m) || cql.eq_ignore_ascii_case(m))
        .map(|(_, cql)| *cql)
        .ok_or_else(|| {
            Error::Query(format!(
                "CQL no tiene la distancia «{m}» para índices vectoriales: solo coseno (COSINE), euclídea (EUCLIDEAN) y producto punto (DOT_PRODUCT)."
            ))
        })
}

/// A vector index's search settings from its `WITH OPTIONS` (as
/// `system_schema.indexes` keeps them), and the options that aren't
/// those. The distance is always set (COSINE when the index doesn't say,
/// the engines' default), so two indexes that search alike read alike.
pub(crate) fn vector_of(
    options: BTreeMap<String, String>,
) -> (VectorIndex, BTreeMap<String, String>) {
    let mut v = VectorIndex {
        metric: Some(metric::COSINE.into()),
        ..Default::default()
    };
    let mut rest = BTreeMap::new();
    for (k, val) in options {
        let key = k.to_ascii_lowercase();
        if key == "similarity_function" {
            v.metric = Some(metric_of(&val));
        } else if let Some((std, _)) = PARAMS.iter().find(|(_, o)| *o == key) {
            v.params.insert(std.to_string(), val);
        } else if VECTOR_OPTIONS.contains(&key.as_str()) {
            v.params.insert(key, val);
        } else if key != "index_version" {
            // ScyllaDB's `index_version` is the server's own stamp.
            rest.insert(k, val);
        }
    }
    (v, rest)
}

/// A vector index's `WITH OPTIONS` pairs, in CQL's names, for `flavor`.
/// Parameters the flavor has no option for are left out (a ScyllaDB
/// `search_beam_width` on Cassandra).
fn vector_options(v: &VectorIndex, flavor: Flavor) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Some(m) = v.metric.as_deref().filter(|m| !m.trim().is_empty()) {
        out.push((
            "similarity_function".to_string(),
            similarity_of(m)?.to_string(),
        ));
    }
    for (k, val) in &v.params {
        if val.trim().is_empty() {
            continue;
        }
        let key = k.trim().to_ascii_lowercase();
        let option = PARAMS
            .iter()
            .find(|(std, o)| *std == key || *o == key)
            .map_or(key.clone(), |(_, o)| o.to_string());
        if takes_option(flavor, &option) {
            out.push((option, val.trim().to_string()));
        }
    }
    Ok(out)
}

pub fn templates(driver_id: &str) -> Vec<CreateTemplate> {
    let mut v = vec![
        CreateTemplate {
            kind: kinds::MATERIALIZED_VIEW,
            label: "Nueva vista materializada",
            template: format!(
                "{}CREATE MATERIALIZED VIEW {{name}} AS\n\
                 \x20   SELECT *\n    FROM tabla\n    WHERE email IS NOT NULL AND id IS NOT NULL\n    PRIMARY KEY (email, id);\n",
                if is_scylla(driver_id) { "" } else { "-- Requiere materialized_views_enabled: true en cassandra.yaml.\n" }
            ),
        },
        CreateTemplate {
            kind: "type",
            label: "Nuevo tipo (UDT)",
            template: "CREATE TYPE {name} (\n    calle text,\n    ciudad text,\n    codigo_postal text\n);\n".into(),
        },
        CreateTemplate {
            kind: kinds::INDEX,
            label: "Nuevo índice secundario",
            template: "CREATE INDEX {name} ON tabla (columna);\n".into(),
        },
    ];
    if is_keyspaces(driver_id) {
        v.retain(|t| t.kind == "type");
        return v;
    }
    if !is_scylla(driver_id) {
        v.push(CreateTemplate {
            kind: kinds::FUNCTION,
            label: "Nueva función",
            template: "-- Requiere user_defined_functions_enabled: true en cassandra.yaml.\n\
                       CREATE OR REPLACE FUNCTION {name}(a int, b int)\n    RETURNS NULL ON NULL INPUT\n    RETURNS int\n    \
                       LANGUAGE java\n    AS $$ return a + b; $$;\n"
                .into(),
        });
        v.push(CreateTemplate {
            kind: kinds::INDEX,
            label: "Nuevo índice SAI",
            template: "CREATE INDEX {name} ON tabla (columna) USING 'sai';\n".into(),
        });
        v.push(CreateTemplate {
            kind: kinds::INDEX,
            label: "Nuevo índice vectorial (SAI)",
            template: "-- embedding es una columna vector<float, n>. Buscá los más parecidos con\n\
                       -- SELECT id, similarity_cosine(embedding, [0.1, 0.2, 0.3]) FROM tabla ORDER BY embedding ANN OF [0.1, 0.2, 0.3] LIMIT 10\n\
                       CREATE INDEX {name} ON tabla (embedding) USING 'sai'\n    \
                       WITH OPTIONS = {'similarity_function': 'COSINE'};\n"
                .into(),
        });
    } else {
        v.push(CreateTemplate {
            kind: kinds::INDEX,
            label: "Nuevo índice vectorial",
            template: "-- embedding es una columna vector<float, n>. La búsqueda ANN (ORDER BY embedding ANN OF [0.1, 0.2, 0.3] LIMIT 10)\n\
                       -- usa el servicio Vector Store de ScyllaDB.\n\
                       CREATE CUSTOM INDEX {name} ON tabla (embedding) USING 'vector_index'\n    \
                       WITH OPTIONS = {'similarity_function': 'COSINE', 'maximum_node_connections': '16', 'construction_beam_width': '128'};\n"
                .into(),
        });
    }
    v
}

pub(crate) fn is_true(c: &ColumnDef, key: &str) -> bool {
    c.options
        .get(key)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Partition and clustering columns, from the column options or else the
/// primary key.
pub(crate) fn keys(t: &TableSchema) -> Result<(Vec<&ColumnDef>, Vec<&ColumnDef>)> {
    let mut pk: Vec<&ColumnDef> = t
        .columns
        .iter()
        .filter(|c| is_true(c, "partition_key"))
        .collect();
    let mut ck: Vec<&ColumnDef> = t
        .columns
        .iter()
        .filter(|c| is_true(c, "clustering_key"))
        .collect();
    if let Some(c) = pk.iter().find(|c| is_true(c, "clustering_key")) {
        return Err(Error::Query(format!(
            "La columna {} no puede ser clave de partición y de clustering a la vez.",
            c.name
        )));
    }
    if pk.is_empty() {
        if !ck.is_empty() {
            return Err(Error::Query(
                "Marcá al menos una columna como clave de partición.".into(),
            ));
        }
        let names = t
            .primary_key
            .as_ref()
            .map(|k| k.columns.as_slice())
            .unwrap_or_default();
        let find = |n: &String| {
            t.columns.iter().find(|c| &c.name == n).ok_or_else(|| {
                Error::Query(format!(
                    "La clave primaria usa una columna que no existe: {n}"
                ))
            })
        };
        let mut cols = names
            .iter()
            .map(find)
            .collect::<Result<Vec<_>>>()?
            .into_iter();
        pk.extend(cols.next());
        ck.extend(cols);
    }
    if pk.is_empty() {
        return Err(Error::Query(
            "Marcá al menos una columna como clave de partición.".into(),
        ));
    }
    Ok((pk, ck))
}

pub(crate) fn table_options(t: &TableSchema, ck: &[&ColumnDef]) -> Result<Vec<String>> {
    let mut with = Vec::new();
    if ck.iter().any(|c| {
        c.options
            .get("clustering_order")
            .is_some_and(|o| o.trim().eq_ignore_ascii_case("desc"))
    }) {
        let order: Vec<String> = ck
            .iter()
            .map(|c| {
                let desc = c
                    .options
                    .get("clustering_order")
                    .is_some_and(|o| o.trim().eq_ignore_ascii_case("desc"));
                format!("{} {}", ident(&c.name), if desc { "DESC" } else { "ASC" })
            })
            .collect();
        with.push(format!("CLUSTERING ORDER BY ({})", order.join(", ")));
    }
    let opt = |k: &str| t.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    for k in ["default_time_to_live", "gc_grace_seconds"] {
        if let Some(v) = opt(k) {
            let n: u64 = v
                .parse()
                .map_err(|_| Error::Query(format!("{k} tiene que ser un número entero: {v}")))?;
            with.push(format!("{k} = {n}"));
        }
    }
    if let Some(c) = opt("compaction") {
        with.push(format!("compaction = {{'class': {}}}", lit(c)));
    }
    if let Some(c) = opt("comment").or(t.comment.as_deref().filter(|c| !c.is_empty())) {
        with.push(format!("comment = {}", lit(c)));
    }
    Ok(with)
}

/// An index target: a column, or `values(col)` / `keys(col)` /
/// `entries(col)` / `full(col)` for collections.
fn index_target(col: &str) -> String {
    let c = col.trim();
    if let Some((f, rest)) = c.split_once('(') {
        let f = f.trim().to_ascii_lowercase();
        if matches!(f.as_str(), "values" | "keys" | "entries" | "full") {
            if let Some(inner) = rest.trim().strip_suffix(')') {
                return format!("{f}({})", ident(unquote(inner.trim())));
            }
        }
    }
    ident(unquote(c))
}

/// `"Name"` as `system_schema.indexes` writes a case-sensitive target.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

/// A vector index: SAI on Cassandra, `vector_index` on ScyllaDB (with
/// filter columns after the vector column, and a local index's partition
/// columns in the `pk` option, as `system_schema` keeps its target).
fn vector_index_ddl(
    table: &str,
    ix: &IndexDef,
    v: &VectorIndex,
    if_exists: bool,
    flavor: Flavor,
) -> Result<String> {
    if flavor == Flavor::Keyspaces {
        return Err(Error::Query(
            "Amazon Keyspaces no tiene índices vectoriales.".into(),
        ));
    }
    if ix.columns.is_empty() {
        return Err(Error::Query(format!(
            "El índice vectorial {} necesita la columna del vector.",
            ix.name
        )));
    }
    let scylla = flavor == Flavor::Scylla;
    if ix.columns.len() > 1 && !scylla {
        return Err(Error::Query(format!(
            "El índice vectorial {} lleva una sola columna en Cassandra (SAI no tiene columnas de filtro).",
            ix.name
        )));
    }
    let mut pairs = vector_options(v, flavor)?;
    // Other options the index had (none of SAI's text ones apply to a
    // vector), except a local index's partition columns, which go in the
    // target.
    for (k, val) in &ix.options {
        if k != "pk" && !pairs.iter().any(|(o, _)| o.eq_ignore_ascii_case(k)) {
            pairs.push((k.clone(), val.clone()));
        }
    }
    let mut target: Vec<String> = Vec::new();
    if let Some(pk) = ix.options.get("pk").filter(|_| scylla) {
        let cols: Vec<String> = pk
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| ident(unquote(c)))
            .collect();
        target.push(format!("({})", cols.join(", ")));
    }
    target.extend(ix.columns.iter().map(|c| ident(unquote(c.trim()))));
    let name = if ix.name.trim().is_empty() {
        String::new()
    } else {
        format!("{} ", ident(&ix.name))
    };
    let options = if pairs.is_empty() {
        String::new()
    } else {
        let pairs: Vec<String> = pairs
            .iter()
            .map(|(k, v)| format!("{}: {}", lit(k), lit(v)))
            .collect();
        format!(" WITH OPTIONS = {{{}}}", pairs.join(", "))
    };
    Ok(format!(
        "CREATE {}INDEX {}{name}ON {table} ({}) USING '{}'{options};",
        if scylla { "CUSTOM " } else { "" },
        if if_exists { "IF NOT EXISTS " } else { "" },
        target.join(", "),
        if scylla { "vector_index" } else { "sai" },
    ))
}

pub(crate) fn index_ddl(
    table: &str,
    ix: &IndexDef,
    if_exists: bool,
    flavor: Flavor,
) -> Result<String> {
    // ScyllaDB's vector_index is a vector index even without its settings
    // (the designer's "Vectorial" switched off on it): a custom index, with
    // the server's defaults.
    let vector_kind = ix
        .kind
        .as_deref()
        .is_some_and(|k| k.trim().eq_ignore_ascii_case("vector_index"));
    let defaults = VectorIndex::default();
    if let Some(v) = ix.vector.as_ref().or(vector_kind.then_some(&defaults)) {
        if ix.unique {
            return Err(Error::Query("CQL no tiene índices únicos.".into()));
        }
        return vector_index_ddl(table, ix, v, if_exists, flavor);
    }
    if ix.unique {
        return Err(Error::Query("CQL no tiene índices únicos.".into()));
    }
    if ix.filter.as_deref().is_some_and(|f| !f.trim().is_empty()) {
        return Err(Error::Query(
            "CQL no tiene índices parciales (con filtro).".into(),
        ));
    }
    if ix.columns.len() != 1 {
        return Err(Error::Query(format!(
            "El índice {} tiene que tener exactamente una columna.",
            ix.name
        )));
    }
    let name = if ix.name.trim().is_empty() {
        String::new()
    } else {
        format!("{} ", ident(&ix.name))
    };
    let using = match ix.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        None => String::new(),
        Some(k) if k.eq_ignore_ascii_case("secondary") || k.eq_ignore_ascii_case("default") => {
            String::new()
        }
        Some(k) if k.eq_ignore_ascii_case("sai") => " USING 'sai'".into(),
        Some(k) => format!(" USING {}", lit(k)),
    };
    let options = if ix.options.is_empty() {
        String::new()
    } else {
        let pairs: Vec<String> = ix
            .options
            .iter()
            .map(|(k, v)| format!("{}: {}", lit(k), lit(v)))
            .collect();
        format!(" WITH OPTIONS = {{{}}}", pairs.join(", "))
    };
    Ok(format!(
        "CREATE INDEX {}{name}ON {table} ({}){using}{options};",
        if if_exists { "IF NOT EXISTS " } else { "" },
        index_target(&ix.columns[0])
    ))
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts, flavor: Flavor) -> Result<String> {
    if t.name.trim().is_empty() {
        return Err(Error::Query("La tabla necesita un nombre.".into()));
    }
    let table = qualified(t.schema.as_deref(), &t.name);
    let mut out: Vec<String> = Vec::new();
    if parts.drop {
        out.push(format!(
            "DROP TABLE {}{table};",
            if parts.if_exists { "IF EXISTS " } else { "" }
        ));
    }
    if parts.create {
        if t.columns.is_empty() {
            return Err(Error::Query(
                "La tabla necesita al menos una columna.".into(),
            ));
        }
        if let Some(c) = t.columns.iter().find(|c| c.data_type.trim().is_empty()) {
            return Err(Error::Query(format!(
                "Falta el tipo de la columna {}.",
                c.name
            )));
        }
        let (pk, ck) = keys(t)?;
        let is_key = |c: &ColumnDef| pk.iter().chain(&ck).any(|k| k.name == c.name);
        for c in t.columns.iter().filter(|c| is_true(c, "static")) {
            if is_key(c) {
                return Err(Error::Query(format!(
                    "La columna {} es clave: no puede ser STATIC.",
                    c.name
                )));
            }
            if ck.is_empty() {
                return Err(Error::Query(
                    "Las columnas STATIC requieren al menos una clave de clustering.".into(),
                ));
            }
        }
        let mut lines: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                format!(
                    "    {} {}{}",
                    ident(&c.name),
                    c.data_type.trim(),
                    if is_true(c, "static") { " STATIC" } else { "" }
                )
            })
            .collect();
        let partition = if pk.len() == 1 {
            ident(&pk[0].name)
        } else {
            format!(
                "({})",
                pk.iter()
                    .map(|c| ident(&c.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let key: Vec<String> = std::iter::once(partition)
            .chain(ck.iter().map(|c| ident(&c.name)))
            .collect();
        lines.push(format!("    PRIMARY KEY ({})", key.join(", ")));
        let with = table_options(t, &ck)?;
        let with = if with.is_empty() {
            String::new()
        } else {
            format!(" WITH {}", with.join("\n    AND "))
        };
        out.push(format!(
            "CREATE TABLE {}{table} (\n{}\n){with};",
            if parts.if_exists {
                "IF NOT EXISTS "
            } else {
                ""
            },
            lines.join(",\n")
        ));
    }
    if parts.indexes {
        for ix in &t.indexes {
            out.push(index_ddl(&table, ix, parts.if_exists, flavor)?);
        }
    }
    Ok(if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    })
}

/// A cell as the JSON Cassandra converts to the column's type. The
/// driver's own cells carry collections, tuples and UDTs as JSON text, so
/// text that parses as a JSON array or object goes as that structure.
fn json_cell(v: &Value) -> Value {
    if let Value::String(s) = v {
        let t = s.trim_start();
        if t.starts_with('[') || t.starts_with('{') {
            if let Ok(parsed @ (Value::Array(_) | Value::Object(_))) =
                serde_json::from_str::<Value>(s)
            {
                return parsed;
            }
        }
    }
    v.clone()
}

/// What a script knows of a column's CQL type (see
/// [`dbine_driver::Driver::insert_script_typed`]): the cells whose JSON
/// depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellType {
    /// `text`, `varchar`, `ascii`: a string stays a string, even when it
    /// reads as JSON (`[1,2]`), and a number or boolean goes as its text.
    Text,
    /// `vector<…, n>`: the cell as a JSON array of numbers (from its text
    /// too), of `n` of them when the type says `n`.
    Vector(Option<u32>),
    /// Anything else: as without types.
    Other,
}

impl CellType {
    pub fn of(c: &dbine_driver::ResultColumn) -> CellType {
        let t = c.type_name.trim().to_ascii_lowercase();
        let t = t
            .strip_prefix("frozen<")
            .and_then(|r| r.strip_suffix('>'))
            .unwrap_or(&t)
            .trim();
        if let Some(meta) = c.vector.as_ref().filter(|m| !m.sparse) {
            return CellType::Vector(meta.dims);
        }
        if let Some(inner) = t.strip_prefix("vector<").and_then(|r| r.strip_suffix('>')) {
            let dims = inner.rsplit(',').next().and_then(|d| d.trim().parse().ok());
            return CellType::Vector(dims);
        }
        match t {
            "text" | "varchar" | "ascii" => CellType::Text,
            _ => CellType::Other,
        }
    }
}

/// [`json_cell`] knowing the column's type.
fn typed_cell(v: &Value, ty: Option<CellType>, column: &str) -> Result<Value> {
    match (ty, v) {
        (Some(CellType::Text), Value::String(_)) => Ok(v.clone()),
        (Some(CellType::Text), Value::Number(n)) => Ok(Value::String(n.to_string())),
        (Some(CellType::Text), Value::Bool(b)) => Ok(Value::String(b.to_string())),
        (Some(CellType::Vector(dims)), _) => match dbine_driver::vector::parse_value(v) {
            Some(nums) => {
                if let Some(d) = dims.filter(|d| *d as usize != nums.len()) {
                    return Err(Error::Query(format!(
                        "El vector de {column} tiene {} dimensiones y la columna, {d}.",
                        nums.len()
                    )));
                }
                // The canonical text's numbers (`1`, not `1.0`).
                Ok(serde_json::from_str(&dbine_driver::vector::text(nums)).unwrap_or(Value::Null))
            }
            None => Ok(json_cell(v)),
        },
        _ => Ok(json_cell(v)),
    }
}

/// The types of `names`, by name, from what the caller knows.
fn types_by_name(
    types: &[dbine_driver::ResultColumn],
) -> std::collections::HashMap<&str, CellType> {
    types
        .iter()
        .map(|c| (c.name.as_str(), CellType::of(c)))
        .collect()
}

/// One `INSERT INTO t JSON '{…}';` per row.
pub fn insert_script(
    target: &ObjectRef,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<String> {
    insert_script_typed(target, columns, &[], rows)
}

/// [`insert_script`] with the columns' types, in their order (none: as
/// without types).
pub fn insert_script_typed(
    target: &ObjectRef,
    columns: &[String],
    types: &[CellType],
    rows: &[Vec<Value>],
) -> Result<String> {
    let table = qualified(target.schema(), &target.name);
    // INSERT JSON folds unquoted keys to lower case, like CQL identifiers.
    let keys: Vec<String> = columns
        .iter()
        .map(|c| {
            if ident(c) == *c {
                c.clone()
            } else {
                format!("\"{}\"", c.replace('"', "\"\""))
            }
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        // Written by hand to keep the column order (serde_json's map sorts).
        let fields = keys
            .iter()
            .zip(row)
            .enumerate()
            .map(|(i, (k, v))| {
                let cell = typed_cell(v, types.get(i).copied(), &columns[i])?;
                Ok(format!("{}:{cell}", Value::from(k.as_str())))
            })
            .collect::<Result<Vec<String>>>()?;
        out.push_str(&format!(
            "INSERT INTO {table} JSON {};\n",
            lit(&format!("{{{}}}", fields.join(",")))
        ));
    }
    Ok(out)
}

/// A cell as a typed CQL value: `fromJson('…')` converts it to the
/// column's type like `INSERT … JSON` does; null stays `null`.
fn from_json(v: &Value) -> String {
    match json_cell(v) {
        Value::Null => "null".into(),
        j => format!("fromJson({})", lit(&j.to_string())),
    }
}

/// [`from_json`] knowing the column's type (see [`typed_cell`]).
fn from_json_typed(v: &Value, ty: Option<CellType>, column: &str) -> Result<String> {
    Ok(match typed_cell(v, ty, column)? {
        Value::Null => "null".into(),
        j => format!("fromJson({})", lit(&j.to_string())),
    })
}

/// One `UPDATE t SET c = … WHERE <primary key> = …;` per edited row.
pub fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    update_script_typed(target, changes, &[])
}

/// [`update_script`] with the columns' types, by name.
pub fn update_script_typed(
    target: &ObjectRef,
    changes: &[RowChange],
    types: &[dbine_driver::ResultColumn],
) -> Result<String> {
    let types = types_by_name(types);
    let cell = |c: &str, v: &Value| from_json_typed(v, types.get(c).copied(), c);
    let table = qualified(target.schema(), &target.name);
    let mut out = String::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        if ch.key.is_empty() {
            return Err(Error::Unsupported(
                "para actualizar una fila de Cassandra hace falta su clave primaria".into(),
            ));
        }
        let set = ch
            .set
            .iter()
            .map(|(c, v)| Ok(format!("{} = {}", ident(c), cell(c, v)?)))
            .collect::<Result<Vec<String>>>()?;
        let conds = ch
            .key
            .iter()
            .map(|(c, v)| Ok(format!("{} = {}", ident(c), cell(c, v)?)))
            .collect::<Result<Vec<String>>>()?;
        out.push_str(&format!(
            "UPDATE {table} SET {} WHERE {};\n",
            set.join(", "),
            conds.join(" AND ")
        ));
    }
    Ok(out)
}

/// One `DELETE FROM t WHERE <primary key> = …;` per row. Primary key
/// columns are never null in Cassandra, so a null key part (or no key at
/// all) can't identify a row and the whole script is refused.
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    delete_script_typed(target, keys, &[])
}

/// [`delete_script`] with the key columns' types, by name.
pub fn delete_script_typed(
    target: &ObjectRef,
    keys: &[Vec<(String, Value)>],
    types: &[dbine_driver::ResultColumn],
) -> Result<String> {
    let types = types_by_name(types);
    let table = qualified(target.schema(), &target.name);
    let mut out = String::new();
    for key in keys {
        if key.is_empty() || key.iter().any(|(_, v)| v.is_null()) {
            return Err(Error::Unsupported("para borrar una fila de Cassandra hace falta su clave primaria completa y sin nulos".into()));
        }
        let conds = key
            .iter()
            .map(|(c, v)| {
                Ok(format!(
                    "{} = {}",
                    ident(c),
                    from_json_typed(v, types.get(c.as_str()).copied(), c)?
                ))
            })
            .collect::<Result<Vec<String>>>()?;
        out.push_str(&format!(
            "DELETE FROM {table} WHERE {};\n",
            conds.join(" AND ")
        ));
    }
    Ok(out)
}

/// The browse query (`SELECT * FROM t LIMIT n;`) restricted by the grid's
/// column filters, with `ALLOW FILTERING` (the columns needn't be keys).
/// Values go through `fromJson()` so they take the column's type (uuid,
/// timestamp…). CQL has no `!=`, `NOT IN`, `IS NULL`, OR, nor LIKE outside
/// SASI / SAI indexes: those filters are left to the grid.
pub fn filtered_browse(browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let mut parts = Vec::new();
    for f in filters {
        let c = ident(&f.column);
        let first = || {
            f.values.first().map(from_json).ok_or_else(|| {
                Error::Query(format!("el filtro de «{}» necesita un valor", f.column))
            })
        };
        let unsupported = |what: &str| {
            Err(Error::Unsupported(format!(
                "CQL no filtra por «{what}» en el servidor"
            )))
        };
        parts.push(match f.op {
            FilterOp::Eq => format!("{c} = {}", first()?),
            FilterOp::Gt => format!("{c} > {}", first()?),
            FilterOp::Ge => format!("{c} >= {}", first()?),
            FilterOp::Lt => format!("{c} < {}", first()?),
            FilterOp::Le => format!("{c} <= {}", first()?),
            FilterOp::In => {
                if f.values.is_empty() {
                    return Err(Error::Query(format!(
                        "el filtro de «{}» necesita al menos un valor",
                        f.column
                    )));
                }
                format!(
                    "{c} IN ({})",
                    f.values
                        .iter()
                        .map(from_json)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            FilterOp::IsTrue => format!("{c} = true"),
            FilterOp::IsFalse => format!("{c} = false"),
            FilterOp::IsEmpty => format!("{c} = ''"),
            FilterOp::Sql => format!("({})", f.sql.as_deref().unwrap_or("").trim()),
            FilterOp::SqlRight => format!("{c} {}", f.sql.as_deref().unwrap_or("").trim()),
            FilterOp::Ne => return unsupported("distinto de"),
            FilterOp::NotIn => return unsupported("no está en"),
            FilterOp::Contains
            | FilterOp::NotContains
            | FilterOp::StartsWith
            | FilterOp::EndsWith => {
                return Err(Error::Unsupported(
                    "CQL no tiene LIKE sobre columnas sin índice SASI o SAI".into(),
                ))
            }
            FilterOp::IsNull | FilterOp::NotNull => return unsupported("nulo"),
            FilterOp::NotEmpty | FilterOp::TrueOrNull | FilterOp::FalseOrNull => {
                return Err(Error::Unsupported(
                    "CQL no combina condiciones con OR ni compara con !=".into(),
                ))
            }
        });
    }
    let q = insert_where(browse, &parts.join("\n  AND ")).ok_or_else(|| {
        Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into())
    })?;
    let body = q.trim_end().trim_end_matches(';').trim_end();
    Ok(format!("{body} ALLOW FILTERING;"))
}

/// The browse query sorted by the grid's columns: never on the server.
/// CQL orders only by the clustering columns, and only with the whole
/// partition key fixed by the WHERE; a table browse fixes none (and which
/// columns are keys isn't known here), so the server would refuse it.
pub fn sorted_browse(query: &str, sort: &[dbine_driver::SortKey]) -> Result<String> {
    if sort.is_empty() {
        return Ok(query.to_string());
    }
    Err(Error::Unsupported(
        "CQL solo ordena por las columnas de clustering y con la clave de partición fija en el filtro: se ordenan las filas cargadas".into(),
    ))
}

/// A keyspace name as CQL allows it (letters, digits, `_`; up to 48).
pub fn keyspace_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 48 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Error::Query(format!(
            "'{name}' no es un nombre de keyspace válido: solo letras, números y _ (hasta 48)."
        )));
    }
    Ok(ident(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cql::split;
    use dbine_driver::KeyDef;

    #[test]
    fn sorted_browse_is_left_to_the_grid() {
        use dbine_driver::SortKey;
        let k = |c: &str, d: bool| SortKey {
            column: c.into(),
            descending: d,
        };
        assert_eq!(
            sorted_browse("SELECT * FROM ks.t LIMIT 50;", &[]).unwrap(),
            "SELECT * FROM ks.t LIMIT 50;"
        );
        for sort in [vec![k("a", false)], vec![k("a", true), k("b\"`c", false)]] {
            assert!(matches!(
                sorted_browse("SELECT * FROM ks.t LIMIT 50;", &sort),
                Err(Error::Unsupported(_))
            ));
            assert!(matches!(
                sorted_browse(
                    "SELECT * FROM ks.t\nWHERE k = 1\nLIMIT 50 ALLOW FILTERING;",
                    &sort
                ),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[test]
    fn filtered_browse_allows_filtering() {
        use serde_json::json;
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter {
            column: column.into(),
            op,
            values,
            sql: None,
        };
        assert_eq!(
            filtered_browse(
                "SELECT * FROM ks.users LIMIT 200;",
                &[
                    f("name", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("Age", FilterOp::Ge, vec![json!(18)]),
                    f("id", FilterOp::In, vec![json!(1), json!(2)]),
                    f("active", FilterOp::IsTrue, vec![]),
                ]
            )
            .unwrap(),
            "SELECT * FROM ks.users\nWHERE name = fromJson('\"O''Brien\"')\n  AND \"Age\" >= fromJson('18')\n  AND id IN (fromJson('1'), fromJson('2'))\n  AND active = true\nLIMIT 200 ALLOW FILTERING;"
        );
        for op in [
            FilterOp::Contains,
            FilterOp::IsNull,
            FilterOp::Ne,
            FilterOp::NotIn,
            FilterOp::TrueOrNull,
        ] {
            assert!(matches!(
                filtered_browse("SELECT * FROM t LIMIT 5;", &[f("x", op, vec![json!("a")])]),
                Err(Error::Unsupported(_))
            ));
        }
    }

    fn col(name: &str, ty: &str, opts: &[(&str, &str)]) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            options: opts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn events() -> TableSchema {
        let mut t = TableSchema {
            name: "Events".into(),
            schema: Some("app".into()),
            columns: vec![
                col("tenant", "text", &[("partition_key", "true")]),
                col("day", "date", &[("partition_key", "true")]),
                col(
                    "ts",
                    "timestamp",
                    &[("clustering_key", "true"), ("clustering_order", "DESC")],
                ),
                col(
                    "seq",
                    "int",
                    &[("clustering_key", "true"), ("clustering_order", "ASC")],
                ),
                col("region", "text", &[("static", "true")]),
                col("payload", "map<text, text>", &[]),
            ],
            indexes: vec![
                IndexDef {
                    name: "ev_region".into(),
                    columns: vec!["region".into()],
                    ..Default::default()
                },
                IndexDef {
                    name: "ev_payload".into(),
                    columns: vec!["values(payload)".into()],
                    kind: Some("sai".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        t.options
            .insert("default_time_to_live".into(), "3600".into());
        t.options
            .insert("compaction".into(), "TimeWindowCompactionStrategy".into());
        t.options.insert("comment".into(), "it's a log".into());
        t
    }

    #[test]
    fn full_table() {
        let all = DdlParts {
            drop: true,
            if_exists: true,
            create: true,
            indexes: true,
            foreign_keys: true,
        };
        let ddl = table_ddl(&events(), all, Flavor::Cassandra).unwrap();
        assert_eq!(
            ddl,
            "DROP TABLE IF EXISTS app.\"Events\";\n\
             CREATE TABLE IF NOT EXISTS app.\"Events\" (\n    tenant text,\n    day date,\n    ts timestamp,\n    seq int,\n    \
             region text STATIC,\n    payload map<text, text>,\n    PRIMARY KEY ((tenant, day), ts, seq)\n) \
             WITH CLUSTERING ORDER BY (ts DESC, seq ASC)\n    AND default_time_to_live = 3600\n    \
             AND compaction = {'class': 'TimeWindowCompactionStrategy'}\n    AND comment = 'it''s a log';\n\
             CREATE INDEX IF NOT EXISTS ev_region ON app.\"Events\" (region);\n\
             CREATE INDEX IF NOT EXISTS ev_payload ON app.\"Events\" (values(payload)) USING 'sai';\n"
        );
        // The editor splits it into its statements.
        assert_eq!(split(&ddl).len(), 4);
    }

    #[test]
    fn simple_key_and_fallback_to_primary_key() {
        let mut t = TableSchema {
            name: "users".into(),
            columns: vec![
                col("id", "uuid", &[]),
                col("at", "timestamp", &[]),
                col("name", "text", &[]),
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into(), "at".into()],
            }),
            ..Default::default()
        };
        let create = DdlParts {
            create: true,
            ..Default::default()
        };
        assert_eq!(
            table_ddl(&t, create, Flavor::Cassandra).unwrap(),
            "CREATE TABLE users (\n    id uuid,\n    at timestamp,\n    name text,\n    PRIMARY KEY (id, at)\n);\n"
        );
        t.primary_key = None;
        assert!(table_ddl(&t, create, Flavor::Cassandra)
            .unwrap_err()
            .to_string()
            .contains("partición"));
        t.columns[0]
            .options
            .insert("partition_key".into(), "true".into());
        assert!(table_ddl(&t, create, Flavor::Cassandra)
            .unwrap()
            .contains("PRIMARY KEY (id)"));
        // STATIC without clustering columns, STATIC key, both kinds of key.
        t.columns[2].options.insert("static".into(), "true".into());
        assert!(table_ddl(&t, create, Flavor::Cassandra).is_err());
        t.columns[2].options.clear();
        t.columns[0]
            .options
            .insert("clustering_key".into(), "true".into());
        assert!(table_ddl(&t, create, Flavor::Cassandra).is_err());
    }

    #[test]
    fn indexes_only_and_refusals() {
        let t = events();
        let ix = table_ddl(
            &t,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
            Flavor::Cassandra,
        )
        .unwrap();
        assert_eq!(ix.lines().count(), 2);
        assert!(ix.starts_with("CREATE INDEX ev_region ON app.\"Events\" (region);"));
        let mut u = t.clone();
        u.indexes[0].unique = true;
        assert!(table_ddl(
            &u,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
            Flavor::Cassandra,
        )
        .is_err());
        assert_eq!(index_target("\"Name\""), "\"Name\"");
        assert_eq!(index_target("keys(Attrs)"), "keys(\"Attrs\")");
        // SAI options go in WITH OPTIONS.
        let mut sai = IndexDef {
            name: "n_sai".into(),
            columns: vec!["name".into()],
            kind: Some("sai".into()),
            ..Default::default()
        };
        sai.options.insert("case_sensitive".into(), "false".into());
        sai.options.insert("normalize".into(), "true".into());
        assert_eq!(
            index_ddl("t", &sai, false, Flavor::Cassandra).unwrap(),
            "CREATE INDEX n_sai ON t (name) USING 'sai' WITH OPTIONS = {'case_sensitive': 'false', 'normalize': 'true'};"
        );
        let mut bad = t;
        bad.options.insert("gc_grace_seconds".into(), "-1".into());
        assert!(table_ddl(
            &bad,
            DdlParts {
                create: true,
                ..Default::default()
            },
            Flavor::Cassandra,
        )
        .is_err());
    }

    fn vec_ix(name: &str, cols: &[&str], metric: Option<&str>) -> IndexDef {
        IndexDef {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            kind: Some("sai".into()),
            vector: Some(VectorIndex {
                metric: metric.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn vector_indexes_per_engine() {
        // Cassandra: SAI with the distance as similarity_function.
        let ix = vec_ix("docs_emb", &["emb"], Some("ip"));
        assert_eq!(
            index_ddl("ks.docs", &ix, false, Flavor::Cassandra).unwrap(),
            "CREATE INDEX docs_emb ON ks.docs (emb) USING 'sai' WITH OPTIONS = {'similarity_function': 'DOT_PRODUCT'};"
        );
        // No metric: the engine's default (COSINE), nothing written.
        assert_eq!(
            index_ddl("t", &vec_ix("i", &["v"], None), true, Flavor::Cassandra).unwrap(),
            "CREATE INDEX IF NOT EXISTS i ON t (v) USING 'sai';"
        );
        // ScyllaDB: a custom vector_index, with its graph settings and
        // filter columns; options it doesn't have are left out.
        let mut sc = vec_ix("docs_emb", &["emb", "Lang"], Some("l2"));
        let v = sc.vector.as_mut().unwrap();
        v.params.insert("m".into(), "24".into());
        v.params.insert("ef_construction".into(), "200".into());
        v.params.insert("quantization".into(), "i8".into());
        v.params.insert("search_beam_width".into(), "64".into());
        v.params.insert("source_model".into(), "ADA002".into());
        assert_eq!(
            index_ddl("ks.docs", &sc, false, Flavor::Scylla).unwrap(),
            "CREATE CUSTOM INDEX docs_emb ON ks.docs (emb, \"Lang\") USING 'vector_index' WITH OPTIONS = {'similarity_function': 'EUCLIDEAN', 'construction_beam_width': '200', 'maximum_node_connections': '24', 'quantization': 'i8', 'search_beam_width': '64'};"
        );
        // A local index (partition columns first).
        let mut local = vec_ix("l", &["emb"], None);
        local.options.insert("pk".into(), "tenant, Day".into());
        assert_eq!(
            index_ddl("t", &local, false, Flavor::Scylla).unwrap(),
            "CREATE CUSTOM INDEX l ON t ((tenant, \"Day\"), emb) USING 'vector_index';"
        );
        // Cassandra keeps its own graph settings, not ScyllaDB's.
        let c = index_ddl("t", &sc, false, Flavor::Cassandra);
        assert!(c.unwrap_err().to_string().contains("una sola columna"));
        sc.columns.truncate(1);
        assert_eq!(
            index_ddl("t", &sc, false, Flavor::Cassandra).unwrap(),
            "CREATE INDEX docs_emb ON t (emb) USING 'sai' WITH OPTIONS = {'similarity_function': 'EUCLIDEAN', 'construction_beam_width': '200', 'maximum_node_connections': '24', 'source_model': 'ADA002'};"
        );
        // A distance CQL doesn't have, and Keyspaces.
        let e = index_ddl(
            "t",
            &vec_ix("i", &["v"], Some("hamming")),
            false,
            Flavor::Cassandra,
        );
        assert!(e.unwrap_err().to_string().contains("hamming"));
        assert!(index_ddl("t", &ix, false, Flavor::Keyspaces).is_err());
        let mut u = ix.clone();
        u.unique = true;
        assert!(index_ddl("t", &u, false, Flavor::Cassandra).is_err());
    }

    /// A ScyllaDB vector_index without its settings (the designer's
    /// "Vectorial" switched off on it) is still a custom vector index.
    #[test]
    fn a_vector_index_without_settings() {
        let ix = IndexDef {
            name: "i".into(),
            columns: vec!["v".into()],
            kind: Some("vector_index".into()),
            ..Default::default()
        };
        assert_eq!(
            index_ddl("t", &ix, false, Flavor::Scylla).unwrap(),
            "CREATE CUSTOM INDEX i ON t (v) USING 'vector_index';"
        );
        assert_eq!(
            index_ddl("t", &ix, false, Flavor::Cassandra).unwrap(),
            "CREATE INDEX i ON t (v) USING 'sai';"
        );
    }

    #[test]
    fn vector_index_options_read_back() {
        let opts = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        // What system_schema.indexes keeps (target and class_name aside).
        let (v, rest) = vector_of(opts(&[
            ("similarity_function", "DOT_PRODUCT"),
            ("maximum_node_connections", "12"),
            ("construction_beam_width", "90"),
            ("search_beam_width", "64"),
            ("index_version", "b9e5bd00-e425-11f1-93fd-6faaa0439a7d"),
            ("pk", "g"),
        ]));
        assert_eq!(v.metric.as_deref(), Some("ip"));
        assert_eq!(v.algorithm, None);
        assert_eq!(
            v.params,
            opts(&[
                ("ef_construction", "90"),
                ("m", "12"),
                ("search_beam_width", "64")
            ])
        );
        assert_eq!(rest, opts(&[("pk", "g")]));
        // No similarity_function: COSINE, the engines' default.
        let (v, rest) = vector_of(BTreeMap::new());
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        assert!(v.params.is_empty() && rest.is_empty());
        // Round trip: what is read is what is written.
        let ix = IndexDef {
            name: "i".into(),
            columns: vec!["v".into()],
            kind: Some("vector_index".into()),
            vector: Some(
                vector_of(opts(&[
                    ("similarity_function", "EUCLIDEAN"),
                    ("quantization", "f16"),
                    ("rescoring", "true"),
                ]))
                .0,
            ),
            ..Default::default()
        };
        assert_eq!(
            index_ddl("t", &ix, false, Flavor::Scylla).unwrap(),
            "CREATE CUSTOM INDEX i ON t (v) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'EUCLIDEAN', 'quantization': 'f16', 'rescoring': 'true'};"
        );
        assert_eq!(metric_of("Cosine"), "cosine");
        assert_eq!(metric_of("WHATEVER"), "whatever");
    }

    #[test]
    fn the_designer_offers_vectors() {
        let c = designer("cassandra");
        assert!(c.data_types.contains(&"vector<float, 3>"));
        let v = c.vector_indexes.unwrap();
        assert_eq!(v.metrics, ["cosine", "l2", "ip"]);
        assert!(v.params.is_empty() && !v.filter_columns && v.algorithms.is_empty());
        let s = designer("scylladb");
        assert!(s.data_types.contains(&"vector<float, 3>"));
        let v = s.vector_indexes.unwrap();
        assert!(v.filter_columns);
        let keys: Vec<&str> = v.params.iter().map(|p| p.key).collect();
        assert_eq!(
            keys,
            [
                "m",
                "ef_construction",
                "search_beam_width",
                "quantization",
                "oversampling",
                "rescoring"
            ]
        );
        let k = designer("keyspaces");
        assert!(k.vector_indexes.is_none());
        assert!(!k.data_types.iter().any(|t| t.starts_with("vector")));
    }

    /// A table with a vector column and its index, as the designer makes it.
    #[test]
    fn a_vector_table_from_the_designer() {
        let t = TableSchema {
            name: "docs".into(),
            schema: Some("ks".into()),
            columns: vec![
                col("id", "int", &[("partition_key", "true")]),
                col("body", "text", &[]),
                col("emb", "vector<float, 3>", &[]),
            ],
            indexes: vec![vec_ix("docs_emb", &["emb"], Some("cosine"))],
            ..Default::default()
        };
        let all = DdlParts {
            create: true,
            indexes: true,
            ..Default::default()
        };
        assert_eq!(
            table_ddl(&t, all, Flavor::Cassandra).unwrap(),
            "CREATE TABLE ks.docs (\n    id int,\n    body text,\n    emb vector<float, 3>,\n    PRIMARY KEY (id)\n);\n\
             CREATE INDEX docs_emb ON ks.docs (emb) USING 'sai' WITH OPTIONS = {'similarity_function': 'COSINE'};\n"
        );
        assert!(table_ddl(&t, all, Flavor::Scylla)
            .unwrap()
            .contains("CREATE CUSTOM INDEX docs_emb ON ks.docs (emb) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'COSINE'};"));
    }

    /// A vector edited in the grid comes as a JSON array: INSERT JSON and
    /// fromJson take it as it is.
    #[test]
    fn vectors_in_scripts_are_json_arrays() {
        let target = ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some("ks".into()),
            name: "docs".into(),
        };
        let s = insert_script(
            &target,
            &["id".to_string(), "emb".into()],
            &[vec![Value::from(1), serde_json::json!([0.1, -2.0, 3.5])]],
        )
        .unwrap();
        assert_eq!(
            s,
            "INSERT INTO ks.docs JSON '{\"id\":1,\"emb\":[0.1,-2.0,3.5]}';\n"
        );
        let u = update_script(
            &target,
            &[RowChange {
                key: vec![("id".into(), Value::from(1))],
                set: vec![("emb".into(), serde_json::json!([1, 2.5, 3]))],
                ..Default::default()
            }],
        )
        .unwrap();
        assert_eq!(
            u,
            "UPDATE ks.docs SET emb = fromJson('[1,2.5,3]') WHERE id = fromJson('1');\n"
        );
    }

    /// With the columns' types: text that reads as JSON stays text in a
    /// text column, a number goes to one as its text, and a vector's text
    /// becomes its array, of the column's dimension.
    #[test]
    fn typed_scripts_follow_the_column_types() {
        use dbine_driver::{ResultColumn as C, VectorElement, VectorMeta};
        let target = ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some("ks".into()),
            name: "docs".into(),
        };
        let cols = ["id".to_string(), "note".into(), "emb".into(), "tags".into()];
        let types = [
            C::new("id", "text"),
            C::new("note", "varchar"),
            C::new("emb", "vector<float, 3>")
                .with_vector(VectorMeta::dense(Some(3), VectorElement::F32)),
            C::new("tags", "list<text>"),
        ];
        let cell_types: Vec<CellType> = types.iter().map(CellType::of).collect();
        assert_eq!(
            cell_types,
            [
                CellType::Text,
                CellType::Text,
                CellType::Vector(Some(3)),
                CellType::Other
            ]
        );
        assert_eq!(
            CellType::of(&C::new("v", "frozen<vector<double, 2>>")),
            CellType::Vector(Some(2))
        );
        let row = vec![
            Value::from("[1]"),
            Value::from(5),
            Value::from("[1, 2.50, -3]"),
            Value::from("[\"a\"]"),
        ];
        let s =
            insert_script_typed(&target, &cols, &cell_types, std::slice::from_ref(&row)).unwrap();
        assert_eq!(
            s,
            "INSERT INTO ks.docs JSON '{\"id\":\"[1]\",\"note\":\"5\",\"emb\":[1,2.5,-3],\"tags\":[\"a\"]}';\n"
        );
        // Without types, text that reads as JSON is JSON.
        let s = insert_script(&target, &cols, std::slice::from_ref(&row)).unwrap();
        assert_eq!(
            s,
            "INSERT INTO ks.docs JSON '{\"id\":[1],\"note\":5,\"emb\":[1,2.5,-3],\"tags\":[\"a\"]}';\n"
        );
        // Another dimension: refused before the server.
        let mut bad = row.clone();
        bad[2] = serde_json::json!([1, 2]);
        let e = insert_script_typed(&target, &cols, &cell_types, &[bad]).unwrap_err();
        assert!(
            e.to_string().contains("2 dimensiones y la columna, 3"),
            "{e}"
        );
        let ch = RowChange {
            key: vec![("id".into(), Value::from("{k}"))],
            set: vec![
                ("emb".into(), Value::from("[0,1,0]")),
                ("note".into(), Value::from("[nota]")),
            ],
            ..Default::default()
        };
        assert_eq!(
            update_script_typed(&target, std::slice::from_ref(&ch), &types).unwrap(),
            "UPDATE ks.docs SET emb = fromJson('[0,1,0]'), note = fromJson('\"[nota]\"') WHERE id = fromJson('\"{k}\"');\n"
        );
        let keys = [vec![("id".to_string(), Value::from("[1]"))]];
        assert_eq!(
            delete_script_typed(&target, &keys, &types).unwrap(),
            "DELETE FROM ks.docs WHERE id = fromJson('\"[1]\"');\n"
        );
        // Untyped, that key would be a list, which a text key isn't.
        assert_eq!(
            delete_script(&target, &keys).unwrap(),
            "DELETE FROM ks.docs WHERE id = fromJson('[1]');\n"
        );
    }

    #[test]
    fn rows_as_insert_json() {
        let target = ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some("ks".into()),
            name: "t".into(),
        };
        let cols = vec![
            "id".to_string(),
            "Name".into(),
            "tags".into(),
            "note".into(),
            "n".into(),
        ];
        let rows = vec![vec![
            Value::from("8b2b8a52-0a55-4b3f-9c0e-3c5f4f9b1d11"),
            Value::from("O'Brien; -- x"),
            Value::from("[\"a\",\"b\"]"),
            Value::Null,
            Value::from(5),
        ]];
        let s = insert_script(&target, &cols, &rows).unwrap();
        assert_eq!(
            s,
            "INSERT INTO ks.t JSON '{\"id\":\"8b2b8a52-0a55-4b3f-9c0e-3c5f4f9b1d11\",\"\\\"Name\\\"\":\"O''Brien; -- x\",\
             \"tags\":[\"a\",\"b\"],\"note\":null,\"n\":5}';\n"
        );
        assert_eq!(split(&s).len(), 1);
    }

    #[test]
    fn updates_use_from_json() {
        let target = ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some("ks".into()),
            name: "t".into(),
        };
        let changes = vec![
            RowChange {
                key: vec![
                    (
                        "id".into(),
                        Value::from("8b2b8a52-0a55-4b3f-9c0e-3c5f4f9b1d11"),
                    ),
                    ("n".into(), Value::from(5)),
                ],
                set: vec![
                    ("Name".into(), Value::from("O'Brien \"Bob\"")),
                    ("note".into(), Value::Null),
                    ("tags".into(), Value::from("[\"a\"]")),
                ],
                ..Default::default()
            },
            RowChange {
                key: vec![("id".into(), Value::from("x"))],
                set: vec![],
                ..Default::default()
            },
        ];
        let s = update_script(&target, &changes).unwrap();
        assert_eq!(
            s,
            "UPDATE ks.t SET \"Name\" = fromJson('\"O''Brien \\\"Bob\\\"\"'), note = null, tags = fromJson('[\"a\"]') \
             WHERE id = fromJson('\"8b2b8a52-0a55-4b3f-9c0e-3c5f4f9b1d11\"') AND n = fromJson('5');\n"
        );
        assert_eq!(split(&s).len(), 1);
    }

    #[test]
    fn deletes_by_full_primary_key() {
        let target = ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some("ks".into()),
            name: "t".into(),
        };
        let keys = vec![
            vec![
                ("id".into(), Value::from("O'Brien")),
                ("Seq".into(), Value::from(5)),
            ],
            vec![
                ("id".into(), Value::from("x")),
                ("Seq".into(), Value::from(1)),
            ],
        ];
        let s = delete_script(&target, &keys).unwrap();
        assert_eq!(
            s,
            "DELETE FROM ks.t WHERE id = fromJson('\"O''Brien\"') AND \"Seq\" = fromJson('5');\n\
             DELETE FROM ks.t WHERE id = fromJson('\"x\"') AND \"Seq\" = fromJson('1');\n"
        );
        assert_eq!(split(&s).len(), 2);
        assert!(delete_script(
            &target,
            &[vec![
                ("id".into(), Value::from(1)),
                ("Seq".into(), Value::Null)
            ]]
        )
        .is_err());
        assert!(delete_script(&target, &[vec![]]).is_err());
    }

    #[test]
    fn keyspace_names() {
        assert_eq!(keyspace_name("app_1").unwrap(), "app_1");
        assert_eq!(keyspace_name("App").unwrap(), "\"App\"");
        assert!(keyspace_name("a-b").is_err());
        assert!(keyspace_name("").is_err());
        assert!(keyspace_name(&"x".repeat(49)).is_err());
    }

    #[test]
    fn templates_split() {
        for id in ["cassandra", "scylladb"] {
            for t in templates(id) {
                assert_eq!(
                    split(&t.template.replace("{name}", "x")).len(),
                    1,
                    "{}",
                    t.label
                );
            }
        }
    }
}

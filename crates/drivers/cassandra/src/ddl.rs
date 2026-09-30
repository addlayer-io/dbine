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

use crate::cql::{ident, qualified};
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef, ObjectRef, Result,
    RowChange, TableSchema,
};
use dbine_driver::filter::{insert_where, ColumnFilter, FilterOp};
use serde_json::Value;

const DATA_TYPES: &[&str] = &[
    "text", "varchar", "ascii", "int", "bigint", "smallint", "tinyint", "varint", "float", "double", "decimal",
    "boolean", "uuid", "timeuuid", "timestamp", "date", "time", "duration", "inet", "blob", "counter", "list<text>",
    "set<text>", "map<text, text>", "frozen<list<int>>", "tuple<int, text>",
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
            Field::new("partition_key", "Clave de partición", FieldKind::Bool)
                .help("Las columnas marcadas forman la clave de partición, en el orden de la lista."),
            Field::new("clustering_key", "Clave de clustering", FieldKind::Bool)
                .help("Ordenan las filas dentro de la partición, en el orden de la lista."),
            Field::new("clustering_order", "Orden de clustering", FieldKind::Select(vec![("ASC", "Ascendente"), ("DESC", "Descendente")]))
                .default_value("ASC"),
            Field::new("static", "Static", FieldKind::Bool).help("Un valor por partición (requiere claves de clustering)."),
        ],
        table_options: if is_keyspaces(driver_id) {
            vec![
                Field::new("default_time_to_live", "TTL por defecto (segundos)", FieldKind::Number)
                    .placeholder("0 (sin vencimiento)")
                    .help("En Amazon Keyspaces requiere TTL habilitado en la tabla."),
                Field::new("comment", "Comentario", FieldKind::Text),
            ]
        } else {
            vec![
                Field::new("default_time_to_live", "TTL por defecto (segundos)", FieldKind::Number).placeholder("0 (sin vencimiento)"),
                Field::new("compaction", "Compactación", FieldKind::Select(compaction)),
                Field::new("gc_grace_seconds", "gc_grace_seconds", FieldKind::Number).placeholder("864000"),
                Field::new("comment", "Comentario", FieldKind::Text),
            ]
        },
        columns_required: true,
    }
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
    }
    v
}

pub(crate) fn is_true(c: &ColumnDef, key: &str) -> bool {
    c.options.get(key).is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Partition and clustering columns, from the column options or else the
/// primary key.
pub(crate) fn keys(t: &TableSchema) -> Result<(Vec<&ColumnDef>, Vec<&ColumnDef>)> {
    let mut pk: Vec<&ColumnDef> = t.columns.iter().filter(|c| is_true(c, "partition_key")).collect();
    let mut ck: Vec<&ColumnDef> = t.columns.iter().filter(|c| is_true(c, "clustering_key")).collect();
    if let Some(c) = pk.iter().find(|c| is_true(c, "clustering_key")) {
        return Err(Error::Query(format!("La columna {} no puede ser clave de partición y de clustering a la vez.", c.name)));
    }
    if pk.is_empty() {
        if !ck.is_empty() {
            return Err(Error::Query("Marcá al menos una columna como clave de partición.".into()));
        }
        let names = t.primary_key.as_ref().map(|k| k.columns.as_slice()).unwrap_or_default();
        let find = |n: &String| {
            t.columns.iter().find(|c| &c.name == n).ok_or_else(|| Error::Query(format!("La clave primaria usa una columna que no existe: {n}")))
        };
        let mut cols = names.iter().map(find).collect::<Result<Vec<_>>>()?.into_iter();
        pk.extend(cols.next());
        ck.extend(cols);
    }
    if pk.is_empty() {
        return Err(Error::Query("Marcá al menos una columna como clave de partición.".into()));
    }
    Ok((pk, ck))
}

pub(crate) fn table_options(t: &TableSchema, ck: &[&ColumnDef]) -> Result<Vec<String>> {
    let mut with = Vec::new();
    if ck.iter().any(|c| c.options.get("clustering_order").is_some_and(|o| o.trim().eq_ignore_ascii_case("desc"))) {
        let order: Vec<String> = ck
            .iter()
            .map(|c| {
                let desc = c.options.get("clustering_order").is_some_and(|o| o.trim().eq_ignore_ascii_case("desc"));
                format!("{} {}", ident(&c.name), if desc { "DESC" } else { "ASC" })
            })
            .collect();
        with.push(format!("CLUSTERING ORDER BY ({})", order.join(", ")));
    }
    let opt = |k: &str| t.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    for k in ["default_time_to_live", "gc_grace_seconds"] {
        if let Some(v) = opt(k) {
            let n: u64 = v.parse().map_err(|_| Error::Query(format!("{k} tiene que ser un número entero: {v}")))?;
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
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

pub(crate) fn index_ddl(table: &str, ix: &IndexDef, if_exists: bool) -> Result<String> {
    if ix.unique {
        return Err(Error::Query("CQL no tiene índices únicos.".into()));
    }
    if ix.filter.as_deref().is_some_and(|f| !f.trim().is_empty()) {
        return Err(Error::Query("CQL no tiene índices parciales (con filtro).".into()));
    }
    if ix.columns.len() != 1 {
        return Err(Error::Query(format!("El índice {} tiene que tener exactamente una columna.", ix.name)));
    }
    let name = if ix.name.trim().is_empty() { String::new() } else { format!("{} ", ident(&ix.name)) };
    let using = match ix.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        None => String::new(),
        Some(k) if k.eq_ignore_ascii_case("secondary") || k.eq_ignore_ascii_case("default") => String::new(),
        Some(k) if k.eq_ignore_ascii_case("sai") => " USING 'sai'".into(),
        Some(k) => format!(" USING {}", lit(k)),
    };
    let options = if ix.options.is_empty() {
        String::new()
    } else {
        let pairs: Vec<String> = ix.options.iter().map(|(k, v)| format!("{}: {}", lit(k), lit(v))).collect();
        format!(" WITH OPTIONS = {{{}}}", pairs.join(", "))
    };
    Ok(format!(
        "CREATE INDEX {}{name}ON {table} ({}){using}{options};",
        if if_exists { "IF NOT EXISTS " } else { "" },
        index_target(&ix.columns[0])
    ))
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    if t.name.trim().is_empty() {
        return Err(Error::Query("La tabla necesita un nombre.".into()));
    }
    let table = qualified(t.schema.as_deref(), &t.name);
    let mut out: Vec<String> = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{table};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        if t.columns.is_empty() {
            return Err(Error::Query("La tabla necesita al menos una columna.".into()));
        }
        if let Some(c) = t.columns.iter().find(|c| c.data_type.trim().is_empty()) {
            return Err(Error::Query(format!("Falta el tipo de la columna {}.", c.name)));
        }
        let (pk, ck) = keys(t)?;
        let is_key = |c: &ColumnDef| pk.iter().chain(&ck).any(|k| k.name == c.name);
        for c in t.columns.iter().filter(|c| is_true(c, "static")) {
            if is_key(c) {
                return Err(Error::Query(format!("La columna {} es clave: no puede ser STATIC.", c.name)));
            }
            if ck.is_empty() {
                return Err(Error::Query("Las columnas STATIC requieren al menos una clave de clustering.".into()));
            }
        }
        let mut lines: Vec<String> = t
            .columns
            .iter()
            .map(|c| format!("    {} {}{}", ident(&c.name), c.data_type.trim(), if is_true(c, "static") { " STATIC" } else { "" }))
            .collect();
        let partition = if pk.len() == 1 {
            ident(&pk[0].name)
        } else {
            format!("({})", pk.iter().map(|c| ident(&c.name)).collect::<Vec<_>>().join(", "))
        };
        let key: Vec<String> = std::iter::once(partition).chain(ck.iter().map(|c| ident(&c.name))).collect();
        lines.push(format!("    PRIMARY KEY ({})", key.join(", ")));
        let with = table_options(t, &ck)?;
        let with = if with.is_empty() { String::new() } else { format!(" WITH {}", with.join("\n    AND ")) };
        out.push(format!(
            "CREATE TABLE {}{table} (\n{}\n){with};",
            if parts.if_exists { "IF NOT EXISTS " } else { "" },
            lines.join(",\n")
        ));
    }
    if parts.indexes {
        for ix in &t.indexes {
            out.push(index_ddl(&table, ix, parts.if_exists)?);
        }
    }
    Ok(if out.is_empty() { String::new() } else { out.join("\n") + "\n" })
}

/// A cell as the JSON Cassandra converts to the column's type. The
/// driver's own cells carry collections, tuples and UDTs as JSON text, so
/// text that parses as a JSON array or object goes as that structure.
fn json_cell(v: &Value) -> Value {
    if let Value::String(s) = v {
        let t = s.trim_start();
        if t.starts_with('[') || t.starts_with('{') {
            if let Ok(parsed @ (Value::Array(_) | Value::Object(_))) = serde_json::from_str::<Value>(s) {
                return parsed;
            }
        }
    }
    v.clone()
}

/// One `INSERT INTO t JSON '{…}';` per row.
pub fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    let table = qualified(target.schema(), &target.name);
    // INSERT JSON folds unquoted keys to lower case, like CQL identifiers.
    let keys: Vec<String> = columns
        .iter()
        .map(|c| if ident(c) == *c { c.clone() } else { format!("\"{}\"", c.replace('"', "\"\"")) })
        .collect();
    let mut out = String::new();
    for row in rows {
        // Written by hand to keep the column order (serde_json's map sorts).
        let fields: Vec<String> =
            keys.iter().zip(row).map(|(k, v)| format!("{}:{}", Value::from(k.as_str()), json_cell(v))).collect();
        out.push_str(&format!("INSERT INTO {table} JSON {};\n", lit(&format!("{{{}}}", fields.join(",")))));
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

/// One `UPDATE t SET c = … WHERE <primary key> = …;` per edited row.
pub fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    let table = qualified(target.schema(), &target.name);
    let mut out = String::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        if ch.key.is_empty() {
            return Err(Error::Unsupported("para actualizar una fila de Cassandra hace falta su clave primaria".into()));
        }
        let set: Vec<String> = ch.set.iter().map(|(c, v)| format!("{} = {}", ident(c), from_json(v))).collect();
        let conds: Vec<String> = ch.key.iter().map(|(c, v)| format!("{} = {}", ident(c), from_json(v))).collect();
        out.push_str(&format!("UPDATE {table} SET {} WHERE {};\n", set.join(", "), conds.join(" AND ")));
    }
    Ok(out)
}

/// One `DELETE FROM t WHERE <primary key> = …;` per row. Primary key
/// columns are never null in Cassandra, so a null key part (or no key at
/// all) can't identify a row and the whole script is refused.
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let table = qualified(target.schema(), &target.name);
    let mut out = String::new();
    for key in keys {
        if key.is_empty() || key.iter().any(|(_, v)| v.is_null()) {
            return Err(Error::Unsupported("para borrar una fila de Cassandra hace falta su clave primaria completa y sin nulos".into()));
        }
        let conds: Vec<String> = key.iter().map(|(c, v)| format!("{} = {}", ident(c), from_json(v))).collect();
        out.push_str(&format!("DELETE FROM {table} WHERE {};\n", conds.join(" AND ")));
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
        let first = || f.values.first().map(from_json).ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let unsupported = |what: &str| Err(Error::Unsupported(format!("CQL no filtra por «{what}» en el servidor")));
        parts.push(match f.op {
            FilterOp::Eq => format!("{c} = {}", first()?),
            FilterOp::Gt => format!("{c} > {}", first()?),
            FilterOp::Ge => format!("{c} >= {}", first()?),
            FilterOp::Lt => format!("{c} < {}", first()?),
            FilterOp::Le => format!("{c} <= {}", first()?),
            FilterOp::In => {
                if f.values.is_empty() {
                    return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
                }
                format!("{c} IN ({})", f.values.iter().map(from_json).collect::<Vec<_>>().join(", "))
            }
            FilterOp::IsTrue => format!("{c} = true"),
            FilterOp::IsFalse => format!("{c} = false"),
            FilterOp::IsEmpty => format!("{c} = ''"),
            FilterOp::Sql => format!("({})", f.sql.as_deref().unwrap_or("").trim()),
            FilterOp::SqlRight => format!("{c} {}", f.sql.as_deref().unwrap_or("").trim()),
            FilterOp::Ne => return unsupported("distinto de"),
            FilterOp::NotIn => return unsupported("no está en"),
            FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith => {
                return Err(Error::Unsupported("CQL no tiene LIKE sobre columnas sin índice SASI o SAI".into()))
            }
            FilterOp::IsNull | FilterOp::NotNull => return unsupported("nulo"),
            FilterOp::NotEmpty | FilterOp::TrueOrNull | FilterOp::FalseOrNull => {
                return Err(Error::Unsupported("CQL no combina condiciones con OR ni compara con !=".into()))
            }
        });
    }
    let q = insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))?;
    let body = q.trim_end().trim_end_matches(';').trim_end();
    Ok(format!("{body} ALLOW FILTERING;"))
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
    fn filtered_browse_allows_filtering() {
        use serde_json::json;
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
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
        for op in [FilterOp::Contains, FilterOp::IsNull, FilterOp::Ne, FilterOp::NotIn, FilterOp::TrueOrNull] {
            assert!(matches!(filtered_browse("SELECT * FROM t LIMIT 5;", &[f("x", op, vec![json!("a")])]), Err(Error::Unsupported(_))));
        }
    }

    fn col(name: &str, ty: &str, opts: &[(&str, &str)]) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            options: opts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
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
                col("ts", "timestamp", &[("clustering_key", "true"), ("clustering_order", "DESC")]),
                col("seq", "int", &[("clustering_key", "true"), ("clustering_order", "ASC")]),
                col("region", "text", &[("static", "true")]),
                col("payload", "map<text, text>", &[]),
            ],
            indexes: vec![
                IndexDef { name: "ev_region".into(), columns: vec!["region".into()], ..Default::default() },
                IndexDef { name: "ev_payload".into(), columns: vec!["values(payload)".into()], kind: Some("sai".into()), ..Default::default() },
            ],
            ..Default::default()
        };
        t.options.insert("default_time_to_live".into(), "3600".into());
        t.options.insert("compaction".into(), "TimeWindowCompactionStrategy".into());
        t.options.insert("comment".into(), "it's a log".into());
        t
    }

    #[test]
    fn full_table() {
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        let ddl = table_ddl(&events(), all).unwrap();
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
            columns: vec![col("id", "uuid", &[]), col("at", "timestamp", &[]), col("name", "text", &[])],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into(), "at".into()] }),
            ..Default::default()
        };
        let create = DdlParts { create: true, ..Default::default() };
        assert_eq!(
            table_ddl(&t, create).unwrap(),
            "CREATE TABLE users (\n    id uuid,\n    at timestamp,\n    name text,\n    PRIMARY KEY (id, at)\n);\n"
        );
        t.primary_key = None;
        assert!(table_ddl(&t, create).unwrap_err().to_string().contains("partición"));
        t.columns[0].options.insert("partition_key".into(), "true".into());
        assert!(table_ddl(&t, create).unwrap().contains("PRIMARY KEY (id)"));
        // STATIC without clustering columns, STATIC key, both kinds of key.
        t.columns[2].options.insert("static".into(), "true".into());
        assert!(table_ddl(&t, create).is_err());
        t.columns[2].options.clear();
        t.columns[0].options.insert("clustering_key".into(), "true".into());
        assert!(table_ddl(&t, create).is_err());
    }

    #[test]
    fn indexes_only_and_refusals() {
        let t = events();
        let ix = table_ddl(&t, DdlParts { indexes: true, ..Default::default() }).unwrap();
        assert_eq!(ix.lines().count(), 2);
        assert!(ix.starts_with("CREATE INDEX ev_region ON app.\"Events\" (region);"));
        let mut u = t.clone();
        u.indexes[0].unique = true;
        assert!(table_ddl(&u, DdlParts { indexes: true, ..Default::default() }).is_err());
        assert_eq!(index_target("\"Name\""), "\"Name\"");
        assert_eq!(index_target("keys(Attrs)"), "keys(\"Attrs\")");
        // SAI options go in WITH OPTIONS.
        let mut sai = IndexDef { name: "n_sai".into(), columns: vec!["name".into()], kind: Some("sai".into()), ..Default::default() };
        sai.options.insert("case_sensitive".into(), "false".into());
        sai.options.insert("normalize".into(), "true".into());
        assert_eq!(
            index_ddl("t", &sai, false).unwrap(),
            "CREATE INDEX n_sai ON t (name) USING 'sai' WITH OPTIONS = {'case_sensitive': 'false', 'normalize': 'true'};"
        );
        let mut bad = t;
        bad.options.insert("gc_grace_seconds".into(), "-1".into());
        assert!(table_ddl(&bad, DdlParts { create: true, ..Default::default() }).is_err());
    }

    #[test]
    fn rows_as_insert_json() {
        let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("ks".into()), name: "t".into() };
        let cols = vec!["id".to_string(), "Name".into(), "tags".into(), "note".into(), "n".into()];
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
        let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("ks".into()), name: "t".into() };
        let changes = vec![
            RowChange {
                key: vec![("id".into(), Value::from("8b2b8a52-0a55-4b3f-9c0e-3c5f4f9b1d11")), ("n".into(), Value::from(5))],
                set: vec![("Name".into(), Value::from("O'Brien \"Bob\"")), ("note".into(), Value::Null), ("tags".into(), Value::from("[\"a\"]"))], ..Default::default()
            },
            RowChange { key: vec![("id".into(), Value::from("x"))], set: vec![], ..Default::default() },
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
        let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("ks".into()), name: "t".into() };
        let keys = vec![vec![("id".into(), Value::from("O'Brien")), ("Seq".into(), Value::from(5))], vec![("id".into(), Value::from("x")), ("Seq".into(), Value::from(1))]];
        let s = delete_script(&target, &keys).unwrap();
        assert_eq!(
            s,
            "DELETE FROM ks.t WHERE id = fromJson('\"O''Brien\"') AND \"Seq\" = fromJson('5');\n\
             DELETE FROM ks.t WHERE id = fromJson('\"x\"') AND \"Seq\" = fromJson('1');\n"
        );
        assert_eq!(split(&s).len(), 2);
        assert!(delete_script(&target, &[vec![("id".into(), Value::from(1)), ("Seq".into(), Value::Null)]]).is_err());
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
                assert_eq!(split(&t.template.replace("{name}", "x")).len(), 1, "{}", t.label);
            }
        }
    }
}

//! Phoenix DDL: the table designer, the catalog read into [`TableSchema`]s
//! and CREATE TABLE / CREATE INDEX / UPSERT scripts.
//!
//! Phoenix differs from the generic SQL builder in a few places: the
//! primary key is mandatory (it is the HBase row key) and goes as a named
//! `CONSTRAINT`, `NOT NULL` is only allowed on key columns (or on any column
//! of an `IMMUTABLE_ROWS` table), `DEFAULT` comes after the nullability,
//! columns may live in a column family (`"CF"."COL"`), there are no foreign
//! keys, unique indexes nor comments, and rows are written with `UPSERT`.

use dbine_driver::ddl::{sql_literal, SqlFlavor};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef, KeyDef, Result,
    RowChange, TableSchema,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Table options the designer offers, in the order CREATE TABLE writes them.
const OPTIONS: [&str; 5] = ["SALT_BUCKETS", "IMMUTABLE_ROWS", "DEFAULT_COLUMN_FAMILY", "COMPRESSION", "TTL"];

/// Phoenix's default column family.
const DEFAULT_FAMILY: &str = "0";

fn q(s: &str) -> String {
    quote_ident(Quote::Double, s)
}

/// An index key column, quoted, keeping its ` DESC` (as [`from_catalog`]
/// writes it) outside the quotes.
fn index_column(c: &str) -> String {
    match c.strip_suffix(" DESC") {
        Some(name) => format!("{} DESC", q(name)),
        None => q(c),
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn table_name(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Double, schema.filter(|s| !s.is_empty()), name)
}

// -- schemas ("Nuevo esquema…" / "Borrar esquema…") ---------------------------

/// Phoenix's permissions on a schema (HBase ACLs): Read, Write, eXecute,
/// Create, Admin. Granting needs `phoenix.acls.enabled` and HBase
/// authorization on the server.
pub const SCHEMA_PERMISSIONS: [&str; 5] = ["R", "W", "X", "C", "A"];

/// A schema is an HBase namespace: only letters, digits and `_` (HBase
/// refuses the rest after its RPC retries, slowly, and Phoenix's parser has
/// no escape for a `"` inside a quoted name).
fn schema_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || !n.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(Error::Query(format!("«{n}» no sirve como esquema de Phoenix: usá solo letras, números y _")));
    }
    Ok(q(n))
}

/// `CREATE SCHEMA` (needs `phoenix.schema.isNamespaceMappingEnabled`).
/// Schemas have no owner.
pub fn create_schema(name: &str, owner: Option<&str>) -> Result<String> {
    if owner.is_some() {
        return Err(Error::Unsupported("en Phoenix un esquema no tiene dueño: otorgá permisos sobre él".into()));
    }
    Ok(format!("CREATE SCHEMA {}", schema_name(name)?))
}

/// `DROP SCHEMA` only drops an empty schema: Phoenix has no CASCADE.
pub fn drop_schema(name: &str, cascade: bool) -> Result<String> {
    if cascade {
        return Err(Error::Unsupported("Phoenix solo borra un esquema vacío: borrá antes sus tablas, vistas y secuencias".into()));
    }
    Ok(format!("DROP SCHEMA {}", schema_name(name)?))
}

/// `'RW'` from `["R", "W"]`, only Phoenix's letters.
fn permission_string(privileges: &[String]) -> Result<String> {
    let mut out = String::new();
    for p in privileges {
        let p = p.trim().to_ascii_uppercase();
        if !SCHEMA_PERMISSIONS.contains(&p.as_str()) {
            return Err(Error::Query(format!("«{p}» no es un permiso de Phoenix (R, W, X, C o A)")));
        }
        if !out.contains(&p) {
            out.push_str(&p);
        }
    }
    if out.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    Ok(lit(&out))
}

/// A user, or an HBase group written `@grupo`.
fn grantee(name: &str) -> String {
    match name.trim().strip_prefix('@') {
        Some(g) => format!("GROUP {}", lit(g)),
        None => lit(name.trim()),
    }
}

/// `GRANT` / `REVOKE` on a schema: the only security script DBine writes
/// for Phoenix (it has no users of its own to list). `REVOKE` takes away
/// every permission the user has there.
pub fn schema_security(action: &dbine_driver::SecurityAction) -> Result<String> {
    use dbine_driver::SecurityAction::{Grant, Revoke};
    // Phoenix upper-cases the schema of a GRANT/REVOKE even when quoted: a
    // mixed-case schema would fail on the server with SchemaNotFound.
    let schema_of = |o: &Option<dbine_driver::ObjectRef>| match o {
        Some(o) if o.kind == "schema" && o.name.trim() != o.name.trim().to_uppercase() => Err(Error::Query(format!(
            "Phoenix pasa a mayúsculas el esquema al otorgar permisos y no encontraría «{}»: usá un nombre en mayúsculas",
            o.name.trim()
        ))),
        Some(o) if o.kind == "schema" => schema_name(&o.name),
        _ => Err(Error::Unsupported("DBine solo otorga permisos de Phoenix sobre un esquema".into())),
    };
    match action {
        Grant { privileges, object, to, grantable } => {
            // Being able to grant to others is HBase's A (admin) permission.
            let mut privileges = privileges.clone();
            if *grantable {
                privileges.push("A".into());
            }
            Ok(format!("GRANT {} ON SCHEMA {} TO {}", permission_string(&privileges)?, schema_of(object)?, grantee(to)))
        }
        Revoke { object, from, .. } => Ok(format!("REVOKE ON SCHEMA {} FROM {}", schema_of(object)?, grantee(from))),
        _ => Err(Error::Unsupported("Phoenix no administra usuarios: eso lo hace HBase (Kerberos)".into())),
    }
}

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        schemas: true,
        auto_increment: false,
        comments: false,
        foreign_keys: false,
        column_options: vec![Field::new("family", "Familia de columnas", FieldKind::Text)
            .placeholder("0")
            .help("Familia de HBase de la columna (no aplica a la clave primaria). Vacío = la predeterminada.")],
        table_options: vec![
            Field::new("SALT_BUCKETS", "SALT_BUCKETS", FieldKind::Number)
                .help("Reparte la clave de fila en N regiones (1-256) para evitar hotspots."),
            Field::new("IMMUTABLE_ROWS", "IMMUTABLE_ROWS", FieldKind::Bool)
                .help("Filas que no se actualizan: índices más baratos y NOT NULL en cualquier columna."),
            Field::new("DEFAULT_COLUMN_FAMILY", "DEFAULT_COLUMN_FAMILY", FieldKind::Text).placeholder("0"),
            Field::new(
                "COMPRESSION",
                "COMPRESSION",
                FieldKind::Select(vec![("", "Ninguna"), ("GZ", "GZ"), ("SNAPPY", "SNAPPY"), ("LZ4", "LZ4"), ("ZSTD", "ZSTD")]),
            ),
            Field::new("TTL", "TTL (s)", FieldKind::Number).help("Segundos que HBase conserva cada celda."),
        ],
        ..DesignerSpec::sql_table(vec![
            "INTEGER",
            "BIGINT",
            "SMALLINT",
            "TINYINT",
            "UNSIGNED_INT",
            "UNSIGNED_LONG",
            "FLOAT",
            "DOUBLE",
            "DECIMAL(10,2)",
            "BOOLEAN",
            "VARCHAR",
            "VARCHAR(255)",
            "CHAR(10)",
            "DATE",
            "TIME",
            "TIMESTAMP",
            "BINARY(16)",
            "VARBINARY",
            "VARCHAR ARRAY",
            "INTEGER ARRAY",
        ])
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::VIEW,
            label: "Nueva vista",
            template: "CREATE VIEW \"{schema}\".\"{name}\" AS\nSELECT *\nFROM \"{schema}\".\"TABLA\"\nWHERE \"COLUMNA\" > 0;\n".into(),
        },
        CreateTemplate {
            kind: kinds::INDEX,
            label: "Nuevo índice",
            template: "-- Índice global (tabla aparte); CREATE LOCAL INDEX lo guarda junto a los datos.\n\
                       CREATE INDEX \"{name}\" ON \"{schema}\".\"TABLA\" (\"COLUMNA\")\nINCLUDE (\"OTRA_COLUMNA\");\n"
                .into(),
        },
        CreateTemplate {
            kind: kinds::SEQUENCE,
            label: "Nueva secuencia",
            template: "CREATE SEQUENCE \"{schema}\".\"{name}\" START WITH 1 INCREMENT BY 1 CACHE 100;\n\
                       -- Uso: UPSERT INTO t (ID, ...) VALUES (NEXT VALUE FOR \"{schema}\".\"{name}\", ...);\n"
                .into(),
        },
        CreateTemplate {
            kind: kinds::FUNCTION,
            label: "Nueva función",
            template: "-- Función de usuario (UDF) en Java; requiere phoenix.functions.allowUserDefinedFunctions=true.\n\
                       CREATE FUNCTION \"{schema}\".\"{name}\"(VARCHAR) RETURNS VARCHAR\n\
                       AS 'com.example.MiFuncion'\nUSING JAR 'hdfs:///phoenix/udf/mi-funcion.jar';\n"
                .into(),
        },
    ]
}

fn table_options(t: &TableSchema) -> Vec<String> {
    OPTIONS
        .iter()
        .filter_map(|k| {
            let v = t.options.get(*k).map(|v| v.trim()).filter(|v| !v.is_empty())?;
            Some(match *k {
                "IMMUTABLE_ROWS" => {
                    if !v.eq_ignore_ascii_case("true") {
                        return None;
                    }
                    format!("{k}=true")
                }
                "COMPRESSION" | "DEFAULT_COLUMN_FAMILY" => format!("{k}={}", lit(v)),
                _ => format!("{k}={v}"),
            })
        })
        .collect()
}

/// A column as CREATE TABLE and `ALTER TABLE … ADD` write it: its family
/// (key columns have none), type, NOT NULL (key columns, or any column of
/// an IMMUTABLE_ROWS table) and default.
pub fn column_def(t: &TableSchema, c: &ColumnDef) -> String {
    let key = t.primary_key.as_ref().is_some_and(|k| k.columns.contains(&c.name));
    let immutable = t.options.get("IMMUTABLE_ROWS").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let default_family = t.options.get("DEFAULT_COLUMN_FAMILY").map(String::as_str).filter(|f| !f.is_empty());
    let family = c
        .options
        .get("family")
        .map(|f| f.trim())
        .filter(|f| !key && !f.is_empty() && *f != DEFAULT_FAMILY && Some(*f) != default_family);
    let mut l = format!("{}{} {}", family.map(|f| format!("{}.", q(f))).unwrap_or_default(), q(&c.name), c.data_type);
    if !c.nullable && (key || immutable) {
        l.push_str(" NOT NULL");
    }
    if let Some(d) = c.default_value.as_deref().filter(|d| !d.is_empty()) {
        l.push_str(&format!(" DEFAULT {d}"));
    }
    l
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    let name = table_name(t.schema.as_deref(), &t.name);
    let mut out: Vec<String> = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let pk: Vec<&str> = t.primary_key.iter().flat_map(|k| k.columns.iter().map(String::as_str)).collect();
        if pk.is_empty() {
            return Err(Error::Query(format!(
                "Phoenix exige una clave primaria (es la clave de fila de HBase): falta en {}",
                t.name
            )));
        }
        // An unnamed single-column key goes inline, so Phoenix keeps no name for it either.
        let pk_name = t.primary_key.as_ref().and_then(|k| k.name.as_deref()).filter(|n| !n.is_empty());
        let inline_pk = pk_name.is_none() && pk.len() == 1;
        let mut lines: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                let key = pk.contains(&c.name.as_str());
                let mut l = format!("    {}", column_def(t, c));
                if inline_pk && key {
                    l.push_str(" PRIMARY KEY");
                }
                l
            })
            .collect();
        if !inline_pk {
            lines.push(format!("    CONSTRAINT {} PRIMARY KEY ({})", q(pk_name.unwrap_or("PK")), pk.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")));
        }
        let opts = table_options(t);
        out.push(format!(
            "CREATE TABLE {}{name} (\n{}\n){};",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            lines.join(",\n"),
            if opts.is_empty() { String::new() } else { format!(" {}", opts.join(", ")) }
        ));
    }
    if parts.indexes {
        for ix in &t.indexes {
            // Phoenix has no unique indexes: `unique` can't be honoured.
            let local = ix.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("local"));
            out.push(format!(
                "CREATE {}INDEX {}{} ON {name} ({});",
                if local { "LOCAL " } else { "" },
                if parts.if_exists { "IF NOT EXISTS " } else { "" },
                q(&ix.name),
                ix.columns.iter().map(|c| index_column(c)).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    // No foreign keys in Phoenix.
    Ok(out.join("\n"))
}

/// One `UPSERT … VALUES` per row (Phoenix takes a single row per statement).
pub fn insert_script(schema: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let flavor = SqlFlavor::ansi();
    let head = format!("UPSERT INTO {} ({}) VALUES", table_name(schema, table), columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "));
    rows.iter()
        .map(|r| format!("{head} ({});", r.iter().map(|v| sql_literal(&flavor, v)).collect::<Vec<_>>().join(", ")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The browse query restricted by the grid's column filters. Phoenix's
/// LIKE has no ESCAPE clause (`\` is its escape character), so it's left
/// out there; a generic Avatica server (Calcite, Druid) keeps it.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter], phoenix: bool) -> Result<String> {
    use dbine_driver::filter::{insert_where, sql_condition, FilterOp, SqlFilterStyle};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let flavor = SqlFlavor::ansi();
    let literal = |v: &Value| sql_literal(&flavor, v);
    let style = SqlFilterStyle { quote: Quote::Double, literal: &literal, like: "LIKE", true_literal: "TRUE", false_literal: "FALSE" };
    let mut parts = Vec::new();
    for f in filters {
        let c = sql_condition(std::slice::from_ref(f), &style)?;
        let like = matches!(f.op, FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith);
        parts.push(match c.strip_suffix(" ESCAPE '\\'") {
            Some(s) if like && phoenix => s.to_string(),
            _ => c,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

/// Edited rows as `UPSERT INTO t (key…, cols…) VALUES (…);`: Phoenix has
/// no UPDATE, and an UPSERT with the row key rewrites only the listed
/// columns. The key must be there and must not be among the edited columns
/// (that would write a new row and leave the old one).
pub fn update_script(schema: Option<&str>, table: &str, changes: &[RowChange]) -> Result<String> {
    let flavor = SqlFlavor::ansi();
    let name = table_name(schema, table);
    let mut out = Vec::new();
    for c in changes.iter().filter(|c| !c.set.is_empty()) {
        if c.key.is_empty() {
            return Err(Error::Unsupported("Phoenix modifica filas con UPSERT y necesita la clave primaria de la fila".into()));
        }
        if c.set.iter().any(|(k, _)| c.key.iter().any(|(kk, _)| kk == k)) {
            return Err(Error::Unsupported(
                "en Phoenix no se puede cambiar la clave primaria con UPSERT: crearía otra fila".into(),
            ));
        }
        let pairs: Vec<&(String, Value)> = c.key.iter().chain(c.set.iter()).collect();
        out.push(format!(
            "UPSERT INTO {name} ({}) VALUES ({});",
            pairs.iter().map(|(k, _)| q(k)).collect::<Vec<_>>().join(", "),
            pairs.iter().map(|(_, v)| sql_literal(&flavor, v)).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(out.join("\n"))
}

/// A catalog type with its size: `VARCHAR(20)`, `DECIMAL(10, 2)`, `VARCHAR(10) ARRAY`.
pub fn type_name(base: &str, size: &str, scale: &str) -> String {
    let (elem, array) = match base.strip_suffix(" ARRAY") {
        Some(e) => (e, " ARRAY"),
        None => (base, ""),
    };
    let sized = !size.is_empty() && (elem.contains("CHAR") || elem.contains("DECIMAL") || elem.contains("BINARY"));
    match (sized, elem.contains("DECIMAL") && !scale.is_empty()) {
        (false, _) => base.to_string(),
        (true, true) => format!("{elem}({size}, {scale}){array}"),
        (true, false) => format!("{elem}({size}){array}"),
    }
}

/// The query [`from_catalog`] reads: every table, index and column row of
/// the user schemas (link and column family rows left out).
pub const CATALOG_QUERY: &str = "SELECT TABLE_SCHEM, TABLE_NAME, TABLE_TYPE, COLUMN_FAMILY, COLUMN_NAME, SQLTypeName(DATA_TYPE),
        COLUMN_SIZE, DECIMAL_DIGITS, NULLABLE, KEY_SEQ, COLUMN_DEF, ORDINAL_POSITION, PK_NAME, DATA_TABLE_NAME,
        INDEX_TYPE, SALT_BUCKETS, IMMUTABLE_ROWS, DEFAULT_COLUMN_FAMILY, SORT_ORDER
 FROM SYSTEM.CATALOG
 WHERE TENANT_ID IS NULL AND (TABLE_SCHEM IS NULL OR TABLE_SCHEM <> 'SYSTEM')
   AND (COLUMN_NAME IS NOT NULL OR COLUMN_FAMILY IS NULL)
 ORDER BY TABLE_SCHEM, TABLE_NAME, ORDINAL_POSITION";

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(v) => v.to_string(),
    }
}

#[derive(Default)]
struct Object {
    header: Vec<Value>,
    /// (ordinal, row)
    columns: Vec<(i64, Vec<Value>)>,
}

/// User tables with their columns, key, options and indexes, from the rows
/// of [`CATALOG_QUERY`].
pub fn from_catalog(rows: Vec<Vec<Value>>) -> Vec<TableSchema> {
    let mut objects: BTreeMap<(String, String), Object> = BTreeMap::new();
    for r in rows {
        let key = (text(r.first()), text(r.get(1)));
        let o = objects.entry(key).or_default();
        if text(r.get(4)).is_empty() {
            o.header = r;
        } else {
            let ord = r.get(11).and_then(Value::as_i64).unwrap_or(0);
            o.columns.push((ord, r));
        }
    }
    for o in objects.values_mut() {
        o.columns.sort_by_key(|(ord, _)| *ord);
    }
    let kind = |o: &Object| text(o.header.get(2));

    let mut tables: BTreeMap<(String, String), TableSchema> = BTreeMap::new();
    for ((schema, name), o) in objects.iter().filter(|(_, o)| kind(o) == "u") {
        let h = &o.header;
        let default_family = Some(text(h.get(17))).filter(|f| !f.is_empty());
        let mut options = BTreeMap::new();
        if h.get(15).and_then(Value::as_i64).is_some_and(|n| n > 0) {
            options.insert("SALT_BUCKETS".to_string(), text(h.get(15)));
        }
        if h.get(16).and_then(Value::as_bool) == Some(true) {
            options.insert("IMMUTABLE_ROWS".to_string(), "true".to_string());
        }
        if let Some(f) = &default_family {
            options.insert("DEFAULT_COLUMN_FAMILY".to_string(), f.clone());
        }
        let mut pk: Vec<(i64, String)> = Vec::new();
        let columns = o
            .columns
            .iter()
            .map(|(_, r)| {
                let name = text(r.get(4));
                if let Some(seq) = r.get(9).and_then(Value::as_i64) {
                    pk.push((seq, name.clone()));
                }
                let family = text(r.get(3));
                let mut opts = BTreeMap::new();
                if !family.is_empty() && family != DEFAULT_FAMILY && Some(&family) != default_family.as_ref() {
                    opts.insert("family".to_string(), family);
                }
                ColumnDef {
                    name,
                    data_type: type_name(&text(r.get(5)), &text(r.get(6)), &text(r.get(7))),
                    // java.sql.DatabaseMetaData.columnNoNulls = 0
                    nullable: text(r.get(8)) != "0",
                    default_value: Some(text(r.get(10))).filter(|d| !d.is_empty()),
                    options: opts,
                    ..Default::default()
                }
            })
            .collect();
        pk.sort();
        tables.insert(
            (schema.clone(), name.clone()),
            TableSchema {
                kind: kinds::TABLE.into(),
                schema: Some(schema.clone()).filter(|s| !s.is_empty()),
                name: name.clone(),
                columns,
                primary_key: (!pk.is_empty()).then(|| KeyDef {
                    name: Some(text(h.get(12))).filter(|n| !n.is_empty()),
                    columns: pk.into_iter().map(|(_, c)| c).collect(),
                }),
                options,
                ..Default::default()
            },
        );
    }

    for ((schema, name), o) in objects.iter().filter(|(_, o)| kind(o) == "i") {
        let Some(t) = tables.get_mut(&(schema.clone(), text(o.header.get(13)))) else { continue };
        let data_pk: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        // Index key columns in order: `FAMILY:COL` for data columns, `:COL`
        // for the table's key columns; local indexes lead with `_INDEX_ID`.
        // SORT_ORDER is Phoenix's SortOrder system value: 1 = DESC, 2 = ASC.
        let mut keyed: Vec<(i64, String, bool)> = o
            .columns
            .iter()
            .filter_map(|(_, r)| {
                let seq = r.get(9).and_then(Value::as_i64)?;
                let n = text(r.get(4));
                let desc = r.get(18).and_then(Value::as_i64) == Some(1);
                (n != "_INDEX_ID").then(|| (seq, n.rsplit_once(':').map_or(n.clone(), |(_, c)| c.to_string()), desc))
            })
            .collect();
        keyed.sort();
        let desc: Vec<bool> = keyed.iter().map(|(_, _, d)| *d).collect();
        let keyed: Vec<String> = keyed.into_iter().map(|(_, c, _)| c).collect();
        // Phoenix appends the table's key columns that weren't indexed:
        // the indexed ones are the shortest prefix whose rest is exactly that.
        let cut = (1..=keyed.len())
            .find(|&k| {
                let rest: Vec<&String> = data_pk.iter().filter(|c| !keyed[..k].contains(c)).collect();
                keyed[k..].iter().collect::<Vec<_>>() == rest
            })
            .unwrap_or(keyed.len());
        let local = o.header.get(14).and_then(Value::as_i64) == Some(2);
        t.indexes.push(IndexDef {
            name: name.clone(),
            columns: keyed[..cut].iter().zip(&desc).map(|(c, d)| if *d { format!("{c} DESC") } else { c.clone() }).collect(),
            unique: false,
            kind: Some(if local { "local" } else { "global" }.into()),
            filter: None,
            ..Default::default()
        });
    }
    tables.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filtered_browse_without_escape_clause() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        let browse = "SELECT *\nFROM \"S\".\"T\"\nLIMIT 200";
        let filters = [
            f("NOMBRE", FilterOp::Eq, vec![json!("O'Brien")]),
            f("NOTA", FilterOp::StartsWith, vec![json!("50%")]),
            f("N", FilterOp::Gt, vec![json!(2)]),
            f("BAJA", FilterOp::IsNull, vec![]),
            f("ID", FilterOp::In, vec![json!(1), json!(2)]),
        ];
        assert_eq!(
            filtered_browse(browse, &filters, true).unwrap(),
            "SELECT *\nFROM \"S\".\"T\"\nWHERE \"NOMBRE\" = 'O''Brien'\n  AND \"NOTA\" LIKE '50\\%%'\n  AND \"N\" > 2\n  AND \"BAJA\" IS NULL\n  AND \"ID\" IN (1, 2)\nLIMIT 200"
        );
        assert!(filtered_browse(browse, &filters[1..2], false).unwrap().contains("LIKE '50\\%%' ESCAPE '\\'"));
    }

    fn t() -> TableSchema {
        TableSchema {
            schema: Some("S".into()),
            name: "A".into(),
            columns: vec![
                ColumnDef { name: "ID".into(), data_type: "BIGINT".into(), nullable: false, ..Default::default() },
                ColumnDef { name: "NAME".into(), data_type: "VARCHAR(50)".into(), nullable: false, ..Default::default() },
                ColumnDef {
                    name: "X".into(),
                    data_type: "INTEGER".into(),
                    options: [("family".to_string(), "CF1".to_string())].into(),
                    default_value: Some("0".into()),
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef { name: Some("MYPK".into()), columns: vec!["ID".into()] }),
            indexes: vec![
                IndexDef { name: "IXG".into(), columns: vec!["NAME".into()], kind: Some("global".into()), ..Default::default() },
                IndexDef { name: "IXL".into(), columns: vec!["X".into(), "NAME DESC".into()], kind: Some("local".into()), ..Default::default() },
            ],
            options: [("SALT_BUCKETS".to_string(), "2".to_string()), ("COMPRESSION".to_string(), "GZ".to_string())].into(),
            ..Default::default()
        }
    }

    const ALL: DdlParts = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };

    #[test]
    fn update_script_as_upserts() {
        let c = RowChange {
            key: vec![("ID".into(), json!(7)), ("REGION".into(), Value::Null)],
            set: vec![("NOMBRE".into(), json!("O'Brien")), ("BAJA".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(Some("VENTAS"), "CLIENTES", &[c, RowChange::default()]).unwrap(),
            "UPSERT INTO \"VENTAS\".\"CLIENTES\" (\"ID\", \"REGION\", \"NOMBRE\", \"BAJA\") VALUES (7, NULL, 'O''Brien', NULL);"
        );
        let no_key = RowChange { key: vec![], set: vec![("A".into(), json!(1))], ..Default::default() };
        assert!(matches!(update_script(None, "T", &[no_key]), Err(Error::Unsupported(_))));
        let key_edit = RowChange { key: vec![("ID".into(), json!(1))], set: vec![("ID".into(), json!(2))], ..Default::default() };
        assert!(matches!(update_script(None, "T", &[key_edit]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn create_table_and_indexes() {
        let s = table_ddl(&t(), ALL).unwrap();
        assert_eq!(
            s,
            "DROP TABLE IF EXISTS \"S\".\"A\";\n\
             CREATE TABLE \"S\".\"A\" (\n    \"ID\" BIGINT NOT NULL,\n    \"NAME\" VARCHAR(50),\n    \
             \"CF1\".\"X\" INTEGER DEFAULT 0,\n    CONSTRAINT \"MYPK\" PRIMARY KEY (\"ID\")\n) SALT_BUCKETS=2, COMPRESSION='GZ';\n\
             CREATE INDEX IF NOT EXISTS \"IXG\" ON \"S\".\"A\" (\"NAME\");\n\
             CREATE LOCAL INDEX IF NOT EXISTS \"IXL\" ON \"S\".\"A\" (\"X\", \"NAME\" DESC);"
        );
        // NOT NULL on a value column only with IMMUTABLE_ROWS.
        let mut im = t();
        im.options.insert("IMMUTABLE_ROWS".into(), "true".into());
        let s = table_ddl(&im, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap();
        assert!(s.starts_with("CREATE TABLE IF NOT EXISTS \"S\".\"A\""), "{s}");
        assert!(s.contains("\"NAME\" VARCHAR(50) NOT NULL,") && s.contains("SALT_BUCKETS=2, IMMUTABLE_ROWS=true, COMPRESSION='GZ';"), "{s}");
        let mut unnamed = t();
        unnamed.primary_key.as_mut().unwrap().name = None;
        let s = table_ddl(&unnamed, DdlParts { create: true, ..Default::default() }).unwrap();
        assert!(s.contains("\"ID\" BIGINT NOT NULL PRIMARY KEY,") && !s.contains("CONSTRAINT"), "{s}");
        let mut nokey = t();
        nokey.primary_key = None;
        assert!(table_ddl(&nokey, ALL).is_err());
        assert_eq!(table_ddl(&t(), DdlParts { foreign_keys: true, ..Default::default() }).unwrap(), "");
    }

    #[test]
    fn upserts_one_row_each() {
        let s = insert_script(Some("S"), "A", &["ID".into(), "NAME".into()], &[vec![json!(1), json!("O'k")], vec![json!(2), Value::Null]]);
        assert_eq!(s, "UPSERT INTO \"S\".\"A\" (\"ID\", \"NAME\") VALUES (1, 'O''k');\nUPSERT INTO \"S\".\"A\" (\"ID\", \"NAME\") VALUES (2, NULL);");
    }

    #[test]
    fn catalog_rows_become_tables() {
        let n = Value::Null;
        let row = |t: &str, ty: Value, fam: Value, col: Value, dt: Value, size: Value, nul: Value, seq: Value, def: Value, ord: Value| {
            vec![json!("S"), json!(t), ty, fam, col, dt, size, n.clone(), nul, seq, def, ord, n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone()]
        };
        let mut header = row("A", json!("u"), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone());
        header[12] = json!("MYPK");
        header[15] = json!(2);
        header[16] = json!(true);
        let mut ix = row("IXL", json!("i"), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone());
        ix[13] = json!("A");
        ix[14] = json!(2);
        let mut ix2 = row("IXK", json!("i"), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone());
        ix2[13] = json!("A");
        ix2[14] = json!(1);
        let rows = vec![
            header,
            row("A", n.clone(), n.clone(), json!("ID"), json!("BIGINT"), n.clone(), json!(0), json!(1), n.clone(), json!(1)),
            row("A", n.clone(), n.clone(), json!("K2"), json!("VARCHAR"), json!(10), json!(0), json!(2), n.clone(), json!(2)),
            row("A", n.clone(), json!("CF1"), json!("X"), json!("INTEGER"), n.clone(), json!(1), n.clone(), json!("1"), json!(4)),
            row("A", n.clone(), json!("0"), json!("TAGS"), json!("VARCHAR ARRAY"), json!(5), json!(1), n.clone(), n.clone(), json!(3)),
            ix,
            row("IXL", n.clone(), n.clone(), json!("_INDEX_ID"), json!("SMALLINT"), n.clone(), json!(0), json!(1), n.clone(), json!(1)),
            [row("IXL", n.clone(), n.clone(), json!("CF1:X"), json!("DECIMAL"), n.clone(), json!(1), json!(2), n.clone(), json!(2)), vec![json!(1)]].concat(),
            row("IXL", n.clone(), n.clone(), json!(":ID"), json!("BIGINT"), n.clone(), json!(0), json!(3), n.clone(), json!(3)),
            row("IXL", n.clone(), n.clone(), json!(":K2"), json!("VARCHAR"), n.clone(), json!(0), json!(4), n.clone(), json!(4)),
            ix2,
            row("IXK", n.clone(), n.clone(), json!(":K2"), json!("VARCHAR"), n.clone(), json!(0), json!(1), n.clone(), json!(1)),
            row("IXK", n.clone(), n.clone(), json!(":ID"), json!("BIGINT"), n.clone(), json!(0), json!(2), n.clone(), json!(2)),
            row("IXK", n.clone(), json!("0"), json!("0:TAGS"), json!("VARCHAR ARRAY"), n.clone(), json!(1), n.clone(), n.clone(), json!(3)),
            row("V", json!("v"), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone(), n.clone()),
        ];
        let ts = from_catalog(rows);
        assert_eq!(ts.len(), 1);
        let a = &ts[0];
        assert_eq!(a.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["ID", "K2", "TAGS", "X"]);
        assert_eq!(a.columns[1].data_type, "VARCHAR(10)");
        assert_eq!(a.columns[2].data_type, "VARCHAR(5) ARRAY");
        assert_eq!(a.columns[3].options.get("family").map(String::as_str), Some("CF1"));
        assert!(a.columns[2].options.is_empty() && !a.columns[0].nullable && a.columns[3].nullable);
        assert_eq!(a.columns[3].default_value.as_deref(), Some("1"));
        assert_eq!(a.primary_key, Some(KeyDef { name: Some("MYPK".into()), columns: vec!["ID".into(), "K2".into()] }));
        assert_eq!(a.options.get("SALT_BUCKETS").map(String::as_str), Some("2"));
        assert_eq!(a.options.get("IMMUTABLE_ROWS").map(String::as_str), Some("true"));
        assert_eq!(a.indexes.len(), 2);
        assert_eq!((a.indexes[0].name.as_str(), a.indexes[0].columns.clone(), a.indexes[0].kind.as_deref()), ("IXK", vec!["K2".to_string()], Some("global")));
        assert_eq!((a.indexes[1].name.as_str(), a.indexes[1].columns.clone(), a.indexes[1].kind.as_deref()), ("IXL", vec!["X DESC".to_string()], Some("local")));
    }

    #[test]
    fn designer_and_templates() {
        let d = designer();
        assert!(d.schemas && d.primary_key && !d.auto_increment && !d.foreign_keys && !d.comments && d.indexes);
        let kinds: Vec<&str> = templates().iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [kinds::VIEW, kinds::INDEX, kinds::SEQUENCE, kinds::FUNCTION]);
    }

    #[test]
    fn schema_scripts() {
        use dbine_driver::{ObjectRef, SecurityAction};
        assert_eq!(create_schema(" Ventas_1 ", None).unwrap(), r#"CREATE SCHEMA "Ventas_1""#);
        assert_eq!(create_schema("Año", None).unwrap(), r#"CREATE SCHEMA "Año""#);
        for bad in ["Ventas\"x", "Mi Esquema", "a.b", "a-b", ""] {
            assert!(matches!(create_schema(bad, None), Err(Error::Query(_))), "{bad}");
        }
        assert!(drop_schema("q\"x", false).is_err());
        assert!(matches!(create_schema("V", Some("ana")), Err(Error::Unsupported(_))));
        assert_eq!(drop_schema("V", false).unwrap(), r#"DROP SCHEMA "V""#);
        assert!(matches!(drop_schema("V", true), Err(Error::Unsupported(_))));
        let on = Some(ObjectRef { kind: "schema".into(), schema: None, name: "V".into() });
        let grant = |p: &[&str], to: &str, g: bool| {
            schema_security(&SecurityAction::Grant { privileges: p.iter().map(|x| x.to_string()).collect(), object: on.clone(), to: to.into(), grantable: g })
        };
        assert_eq!(grant(&["r", "W", "R"], "ana'b", false).unwrap(), r#"GRANT 'RW' ON SCHEMA "V" TO 'ana''b'"#);
        assert_eq!(grant(&["C"], "@devs", false).unwrap(), r#"GRANT 'C' ON SCHEMA "V" TO GROUP 'devs'"#);
        assert!(grant(&["RW' TO 'x"], "ana", false).is_err());
        assert!(grant(&[], "ana", false).is_err());
        assert_eq!(grant(&["R"], "ana", true).unwrap(), r#"GRANT 'RA' ON SCHEMA "V" TO 'ana'"#);
        assert_eq!(grant(&["A", "R"], "ana", true).unwrap(), r#"GRANT 'AR' ON SCHEMA "V" TO 'ana'"#);
        let lower = Some(ObjectRef { kind: "schema".into(), schema: None, name: "lower".into() });
        assert!(schema_security(&SecurityAction::Grant { privileges: vec!["R".into()], object: lower.clone(), to: "a".into(), grantable: false }).is_err());
        assert!(schema_security(&SecurityAction::Revoke { privileges: vec![], object: lower, from: "a".into() }).is_err());
        assert_eq!(
            schema_security(&SecurityAction::Revoke { privileges: vec!["R".into()], object: on.clone(), from: "ana".into() }).unwrap(),
            r#"REVOKE ON SCHEMA "V" FROM 'ana'"#
        );
        let table = Some(ObjectRef { kind: kinds::TABLE.into(), schema: Some("V".into()), name: "T".into() });
        assert!(schema_security(&SecurityAction::Grant { privileges: vec!["R".into()], object: table, to: "a".into(), grantable: false }).is_err());
        assert!(schema_security(&SecurityAction::CreateRole { name: "r".into() }).is_err());
    }
}

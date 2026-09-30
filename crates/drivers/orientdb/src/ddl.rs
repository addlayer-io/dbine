//! The class designer, OrientDB SQL DDL, create templates and insert
//! scripts.
//!
//! A class ([`TableSchema`]) becomes `CREATE CLASS … [EXTENDS …]` plus a
//! `CREATE PROPERTY` per declared column (columns only seen in the data,
//! option `inferred`, are left out: OrientDB is schemaless there). The
//! primary key, if any, is a UNIQUE index; foreign keys are LINK
//! properties with a linked class.

use crate::{as_text, ident, EDGE, VERTEX};
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef, ObjectRef, Result,
    RowChange, TableSchema,
};
use dbine_driver::filter::{insert_where, ColumnFilter, FilterOp};
use serde_json::Value;
use std::collections::BTreeMap;

const DATA_TYPES: &[&str] = &[
    "STRING", "INTEGER", "LONG", "SHORT", "BYTE", "FLOAT", "DOUBLE", "DECIMAL", "BOOLEAN", "DATE", "DATETIME", "BINARY",
    "EMBEDDED", "EMBEDDEDLIST", "EMBEDDEDSET", "EMBEDDEDMAP", "LINK", "LINKLIST", "LINKSET", "LINKMAP", "LINKBAG", "ANY",
];

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        kind: kinds::TABLE,
        label: "Nueva clase",
        data_types: DATA_TYPES.to_vec(),
        schemas: false,
        primary_key: true,
        auto_increment: false,
        defaults: true,
        nullability: true,
        comments: false,
        indexes: true,
        foreign_keys: true,
        column_options: vec![
            Field::new("mandatory", "Obligatoria", FieldKind::Bool).help("El campo tiene que estar presente (aunque sea null)."),
            Field::new("readonly", "Solo lectura", FieldKind::Bool).help("No se puede cambiar después de crear el registro."),
            Field::new("linked", "Clase vinculada", FieldKind::Text)
                .placeholder("Persona")
                .help("Para LINK*/EMBEDDED*: la clase de los registros a los que apunta."),
        ],
        table_options: vec![
            Field::new("extends", "Extiende", FieldKind::Text)
                .placeholder("V")
                .help("V para una clase de vértices, E para una de aristas, otra clase, o vacío para documentos."),
            Field::new("abstract", "Abstracta", FieldKind::Bool),
        ],
        columns_required: false,
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(VERTEX, "Nueva clase de vértices", "CREATE CLASS {name} IF NOT EXISTS EXTENDS V;\nCREATE PROPERTY {name}.nombre IF NOT EXISTS STRING (MANDATORY TRUE);"),
        t(EDGE, "Nueva clase de aristas", "CREATE CLASS {name} IF NOT EXISTS EXTENDS E;\nCREATE PROPERTY {name}.desde IF NOT EXISTS DATE;"),
        t(kinds::INDEX, "Nuevo índice", "CREATE INDEX {name} IF NOT EXISTS ON Clase (campo) NOTUNIQUE"),
        t(kinds::FUNCTION, "Nueva función", "CREATE FUNCTION {name} \"return a + b;\" PARAMETERS [a, b] IDEMPOTENT true LANGUAGE javascript"),
        t(kinds::SEQUENCE, "Nueva secuencia", "CREATE SEQUENCE {name} TYPE ORDERED START 0 INCREMENT 1"),
    ]
}

/// A string literal.
pub fn string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', "\\n"))
}

pub(crate) fn opt<'a>(c: &'a ColumnDef, k: &str) -> Option<&'a str> {
    c.options.get(k).map(String::as_str).filter(|v| !v.is_empty())
}

pub(crate) fn truthy(v: Option<&str>) -> bool {
    matches!(v, Some("true" | "1" | "TRUE" | "yes"))
}

pub(crate) fn property(class: &str, c: &ColumnDef, if_not_exists: bool) -> String {
    let ty = c.data_type.split('|').next().map(str::trim).filter(|t| !t.is_empty()).unwrap_or("ANY").to_ascii_uppercase();
    let mut s = format!("CREATE PROPERTY {}.{}{} {ty}", ident(class), ident(&c.name), if if_not_exists { " IF NOT EXISTS" } else { "" });
    if let Some(l) = opt(c, "linked") {
        s.push(' ');
        s.push_str(&ident(l));
    }
    let mut attrs = Vec::new();
    if truthy(opt(c, "mandatory")) {
        attrs.push("MANDATORY TRUE".to_string());
    }
    if !c.nullable {
        attrs.push("NOTNULL TRUE".into());
    }
    if truthy(opt(c, "readonly")) {
        attrs.push("READONLY TRUE".into());
    }
    if let Some(d) = c.default_value.as_deref().filter(|d| !d.is_empty()) {
        attrs.push(format!("DEFAULT {}", string(d)));
    }
    for (k, a) in [("min", "MIN"), ("max", "MAX"), ("regexp", "REGEXP")] {
        if let Some(v) = opt(c, k) {
            attrs.push(format!("{a} {}", string(v)));
        }
    }
    if !attrs.is_empty() {
        s.push_str(&format!(" ({})", attrs.join(", ")));
    }
    s
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    if t.name.trim().is_empty() {
        return Err(Error::Query("Falta el nombre de la clase.".into()));
    }
    let class = ident(&t.name);
    let mut out: Vec<String> = Vec::new();
    if parts.drop {
        out.push(format!("DROP CLASS {class}{} UNSAFE", if parts.if_exists { " IF EXISTS" } else { "" }));
    }
    if parts.create {
        let mut s = format!("CREATE CLASS {class}{}", if parts.if_exists { " IF NOT EXISTS" } else { "" });
        let sup = t.options.get("extends").map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_string).or_else(|| match t.kind.as_str() {
            VERTEX => Some("V".into()),
            EDGE => Some("E".into()),
            _ => None,
        });
        if let Some(sup) = sup.filter(|s| s != &t.name) {
            s.push_str(&format!(" EXTENDS {}", sup.split(',').map(|x| ident(x.trim())).collect::<Vec<_>>().join(", ")));
        }
        if truthy(t.options.get("abstract").map(String::as_str)) {
            s.push_str(" ABSTRACT");
        }
        out.push(s);
        for c in t.columns.iter().filter(|c| !c.name.starts_with('@') && !truthy(opt(c, "inferred"))) {
            out.push(property(&t.name, c, parts.if_exists));
        }
        if let Some(pk) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
            let name = pk.name.clone().unwrap_or_else(|| format!("{}.pk", t.name));
            out.push(format!(
                "CREATE INDEX {}{} ON {class} ({}) UNIQUE",
                ident_index(&name),
                if parts.if_exists { " IF NOT EXISTS" } else { "" },
                pk.columns.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    if parts.indexes {
        for ix in &t.indexes {
            let ty = ix.kind.clone().filter(|k| !k.is_empty()).unwrap_or_else(|| if ix.unique { "UNIQUE".into() } else { "NOTUNIQUE".into() });
            let name = if ix.name.is_empty() { format!("{}.{}", t.name, ix.columns.join("_")) } else { ix.name.clone() };
            out.push(format!(
                "CREATE INDEX {}{} ON {class} ({}) {ty}{}",
                ident_index(&name),
                if parts.if_exists { " IF NOT EXISTS" } else { "" },
                ix.columns.iter().map(|c| index_field(c)).collect::<Vec<_>>().join(", "),
                index_tail(ix)
            ));
        }
    }
    if parts.foreign_keys {
        for fk in &t.foreign_keys {
            for c in &fk.columns {
                let linked = t.columns.iter().find(|x| &x.name == c).and_then(|x| opt(x, "linked"));
                if linked.is_none() {
                    out.push(format!("ALTER PROPERTY {class}.{} LINKEDCLASS {}", ident(c), ident(&fk.ref_table)));
                }
            }
        }
    }
    Ok(out.into_iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"))
}

/// An index field: `name` or `name COLLATE ci`.
fn index_field(c: &str) -> String {
    match c.split_once(" COLLATE ") {
        Some((f, collate)) => format!("{} COLLATE {}", ident(f.trim()), collate.trim()),
        None => ident(c),
    }
}

/// ` ENGINE …` and ` METADATA {…}` of an index (its `ENGINE` and
/// `METADATA` options).
fn index_tail(ix: &IndexDef) -> String {
    let mut s = String::new();
    if let Some(e) = ix.options.get("ENGINE").filter(|e| !e.trim().is_empty()) {
        s.push_str(&format!(" ENGINE {}", e.trim()));
    }
    if let Some(m) = ix.options.get("METADATA").filter(|m| !m.trim().is_empty()) {
        s.push_str(&format!(" METADATA {}", m.trim()));
    }
    s
}

/// The index fields (with `COLLATE` when not the default) and options
/// (`ENGINE` when not the default one, `METADATA` without its `@` keys) of
/// an index of the database metadata.
pub fn index_parts(ix: &Value) -> (Vec<String>, BTreeMap<String, String>) {
    let conf = ix.get("configuration");
    let def = conf.and_then(|c| c.get("indexDefinition"));
    let field = |d: &Value| {
        let f = d.get("field").map(as_text).filter(|f| !f.is_empty())?;
        Some(match d.get("collate").map(as_text).filter(|c| !c.is_empty() && c != "default") {
            Some(c) => format!("{f} COLLATE {c}"),
            None => f,
        })
    };
    let mut fields: Vec<String> = def.and_then(field).into_iter().collect();
    if let Some(ds) = def.and_then(|d| d.get("indexDefinitions")).and_then(Value::as_array) {
        fields.extend(ds.iter().filter_map(field));
    }
    let mut options = BTreeMap::new();
    if let Some(a) = conf.and_then(|c| c.get("algorithm")).map(as_text).filter(|a| a.eq_ignore_ascii_case("LUCENE")) {
        options.insert("ENGINE".to_string(), a);
    }
    if let Some(m) = conf.and_then(|c| c.get("metadata")).and_then(Value::as_object) {
        let m: serde_json::Map<String, Value> = m.iter().filter(|(k, _)| !k.starts_with('@')).map(|(k, v)| (k.clone(), v.clone())).collect();
        if !m.is_empty() {
            options.insert("METADATA".to_string(), Value::Object(m).to_string());
        }
    }
    (fields, options)
}

/// Index names are often `Class.field`: the dot is fine unquoted.
pub(crate) fn ident_index(name: &str) -> String {
    if name.split('.').all(|p| ident(p) == p) {
        name.to_string()
    } else {
        ident(name)
    }
}

/// `CREATE INDEX` for an index of the database metadata.
pub fn index_statement(ix: &Value) -> String {
    let name = ix.get("name").map(as_text).unwrap_or_default();
    let conf = ix.get("configuration");
    let ty = conf.and_then(|c| c.get("type")).map(as_text).unwrap_or_else(|| "NOTUNIQUE".into());
    let def = conf.and_then(|c| c.get("indexDefinition"));
    let class = def.and_then(|d| d.get("className")).map(as_text);
    let (fields, options) = index_parts(ix);
    let mut s = match class {
        Some(c) => format!(
            "CREATE INDEX {} ON {} ({}) {ty}{}",
            ident_index(&name),
            ident(&c),
            fields.iter().map(|f| index_field(f)).collect::<Vec<_>>().join(", "),
            index_tail(&IndexDef { options, ..Default::default() })
        ),
        None => {
            let key = def.and_then(|d| d.get("keyTypes").or_else(|| d.get("keyType"))).map(as_text).unwrap_or_default();
            format!("CREATE INDEX {} {ty} {key}", ident_index(&name)).trim_end().to_string()
        }
    };
    if let Some(algo) = conf.and_then(|c| c.get("algorithm")).map(as_text).filter(|a| !a.is_empty()) {
        s.push_str(&format!("\n-- algoritmo: {algo}"));
    }
    s
}

fn field<'a>(r: &'a [(String, Value)], k: &str) -> Option<&'a Value> {
    r.iter().find(|(x, _)| x == k).map(|(_, v)| v).filter(|v| !v.is_null())
}

pub fn function_statement(r: &Vec<(String, Value)>) -> String {
    let name = field(r, "name").map(as_text).unwrap_or_default();
    let code = field(r, "code").map(as_text).unwrap_or_default();
    let mut s = format!("CREATE FUNCTION {} {}", ident(&name), serde_json::to_string(&code).unwrap_or_default());
    if let Some(Value::Array(ps)) = field(r, "parameters") {
        s.push_str(&format!(" PARAMETERS [{}]", ps.iter().map(as_text).collect::<Vec<_>>().join(", ")));
    }
    if let Some(i) = field(r, "idempotent") {
        s.push_str(&format!(" IDEMPOTENT {}", as_text(i)));
    }
    s.push_str(&format!(" LANGUAGE {}", field(r, "language").map(as_text).unwrap_or_else(|| "sql".into())));
    s
}

pub fn sequence_statement(r: &Vec<(String, Value)>) -> String {
    let name = field(r, "name").map(as_text).unwrap_or_default();
    let mut s = format!("CREATE SEQUENCE {} TYPE {}", ident(&name), field(r, "type").map(as_text).unwrap_or_else(|| "CACHED".into()));
    for (k, kw) in [("start", "START"), ("incr", "INCREMENT"), ("cacheSize", "CACHE")] {
        if let Some(v) = field(r, k) {
            s.push_str(&format!(" {kw} {}", as_text(v)));
        }
    }
    // Not the current value: two copies of a sequence differ in it, not in
    // their definition (the schema compare reads this).
    s
}

/// A grid cell back to JSON: embedded values come as JSON text.
fn unflatten(v: &Value) -> Value {
    match v {
        Value::String(s) if (s.starts_with('{') && s.ends_with('}')) || (s.starts_with('[') && s.ends_with(']')) => {
            serde_json::from_str(s).unwrap_or_else(|_| v.clone())
        }
        other => other.clone(),
    }
}

/// One statement per row: `CREATE VERTEX` / `INSERT INTO … CONTENT`, and
/// `CREATE EDGE … FROM out TO in CONTENT` for edges (their `out` / `in`
/// record ids come from the source database).
pub fn insert_script(target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    let class = ident(&target.name);
    let mut out = String::new();
    for row in rows {
        let mut content = serde_json::Map::new();
        let (mut from, mut to) = (None, None);
        for (c, v) in columns.iter().zip(row) {
            match c.as_str() {
                "out" if target.kind == EDGE => from = Some(as_text(v)),
                "in" if target.kind == EDGE => to = Some(as_text(v)),
                c if c.starts_with('@') || v.is_null() => {}
                c if crate::is_graph_field(c) => {}
                c => {
                    content.insert(c.to_string(), unflatten(v));
                }
            }
        }
        let body = Value::Object(content).to_string();
        let stmt = match target.kind.as_str() {
            EDGE => {
                let (Some(f), Some(t)) = (from.filter(|f| f.starts_with('#')), to.filter(|t| t.starts_with('#'))) else {
                    return Err(Error::Unsupported("para copiar aristas hacen falta sus columnas out e in (los #rid de sus vértices)".into()));
                };
                format!("CREATE EDGE {class} FROM {f} TO {t} CONTENT {body};")
            }
            VERTEX => format!("CREATE VERTEX {class} CONTENT {body};"),
            _ => format!("INSERT INTO {class} CONTENT {body};"),
        };
        out.push_str(&stmt);
        out.push('\n');
    }
    Ok(out)
}

/// A grid cell as an OrientDB SQL value: JSON is valid there (strings in
/// double quotes with backslash escapes, maps, lists, `null`).
fn sql_value(v: &Value) -> String {
    unflatten(v).to_string()
}

/// The `WHERE` condition that identifies a record: by `@rid` when the key
/// has it, else on the key fields (`IS NULL` for a null one).
fn key_cond(key: &[(String, Value)]) -> Result<String> {
    if let Some((_, v)) = key.iter().find(|(c, v)| c == "@rid" && as_text(v).starts_with('#')) {
        return Ok(format!("@rid = {}", as_text(v)));
    }
    let conds: Vec<String> = key
        .iter()
        .filter(|(c, _)| !c.starts_with('@'))
        .map(|(c, v)| if v.is_null() { format!("{} IS NULL", ident(c)) } else { format!("{} = {}", ident(c), sql_value(v)) })
        .collect();
    if conds.is_empty() {
        return Err(Error::Unsupported("no hay campos para identificar el registro".into()));
    }
    Ok(conds.join(" AND "))
}

/// One `UPDATE Class SET … WHERE …;` per edited record: by `@rid` when the
/// key has it, else on the key fields (`IS NULL` for a null one). A null
/// value sets the field to null. `@` attributes and the edge links (`out`,
/// `in`, `out_*`, `in_*`) aren't edited here.
pub fn update_script(target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    let class = ident(&target.name);
    let mut out = String::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        if let Some((c, _)) =
            ch.set.iter().find(|(c, _)| c.starts_with('@') || crate::is_graph_field(c) || (target.kind == EDGE && (c == "out" || c == "in")))
        {
            return Err(Error::Unsupported(format!("{c} no se edita desde la grilla")));
        }
        let cond = key_cond(&ch.key)?;
        let sets: Vec<String> = ch.set.iter().map(|(c, v)| format!("{} = {}", ident(c), sql_value(v))).collect();
        out.push_str(&format!("UPDATE {class} SET {} WHERE {cond};\n", sets.join(", ")));
    }
    Ok(out)
}

/// One delete per record, identified like [`update_script`] does:
/// `DELETE VERTEX` for vertices (it also drops their edges), `DELETE EDGE`
/// for edges (it unlinks them from their vertices) and `DELETE FROM` for
/// plain documents.
pub fn delete_script(target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    let class = ident(&target.name);
    let verb = match target.kind.as_str() {
        VERTEX => "DELETE VERTEX",
        EDGE => "DELETE EDGE",
        _ => "DELETE FROM",
    };
    let mut out = String::new();
    for key in keys {
        out.push_str(&format!("{verb} {class} WHERE {};\n", key_cond(key)?));
    }
    Ok(out)
}

/// The browse query (`SELECT FROM Class LIMIT n`) restricted by the grid's
/// column filters, in OrientDB SQL: JSON literals, `[…]` lists, and
/// `left()` / `right()` / `indexOf()` for text matches (LIKE has no escape
/// for its `%` and `?` wildcards). `@rid` compares with the bare `#c:p`.
pub fn filtered_browse(browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let mut parts = Vec::new();
    for f in filters {
        let c = if f.column.starts_with('@') { f.column.clone() } else { ident(&f.column) };
        let value = |v: &Value| match v {
            Value::String(s) if f.column == "@rid" && s.starts_with('#') => s.clone(),
            other => sql_value(other),
        };
        let first = || f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let text = || first().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
            }
            Ok(f.values.iter().map(value).collect::<Vec<_>>().join(", "))
        };
        let sql = || f.sql.as_deref().unwrap_or("").trim().to_string();
        parts.push(match f.op {
            FilterOp::Eq => format!("{c} = {}", value(first()?)),
            FilterOp::Ne => format!("{c} <> {}", value(first()?)),
            FilterOp::Gt => format!("{c} > {}", value(first()?)),
            FilterOp::Ge => format!("{c} >= {}", value(first()?)),
            FilterOp::Lt => format!("{c} < {}", value(first()?)),
            FilterOp::Le => format!("{c} <= {}", value(first()?)),
            FilterOp::Contains => format!("{c}.indexOf({}) > -1", Value::String(text()?)),
            FilterOp::NotContains => format!("{c}.indexOf({}) = -1", Value::String(text()?)),
            FilterOp::StartsWith => {
                let t = text()?;
                format!("{c}.left({}) = {}", t.chars().count(), Value::String(t))
            }
            FilterOp::EndsWith => {
                let t = text()?;
                format!("{c}.right({}) = {}", t.chars().count(), Value::String(t))
            }
            FilterOp::IsNull => format!("{c} IS NULL"),
            FilterOp::NotNull => format!("{c} IS NOT NULL"),
            FilterOp::IsEmpty => format!("{c} = \"\""),
            FilterOp::NotEmpty => format!("({c} IS NOT NULL AND {c} <> \"\")"),
            FilterOp::In => format!("{c} IN [{}]", list()?),
            FilterOp::NotIn => format!("NOT ({c} IN [{}])", list()?),
            FilterOp::IsTrue => format!("{c} = true"),
            FilterOp::IsFalse => format!("{c} = false"),
            FilterOp::TrueOrNull => format!("({c} = true OR {c} IS NULL)"),
            FilterOp::FalseOrNull => format!("({c} = false OR {c} IS NULL)"),
            FilterOp::Sql => format!("({})", sql()),
            FilterOp::SqlRight => format!("{c} {}", sql()),
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ForeignKeyDef, IndexDef, KeyDef};
    use serde_json::json;

    #[test]
    fn filtered_browse_in_orientdb_sql() {
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT FROM Person LIMIT 200",
                &[
                    f("name", FilterOp::Eq, vec![json!("O'Brien \"Jr\"")]),
                    f("note", FilterOp::StartsWith, vec![json!("50%")]),
                    f("my field", FilterOp::Contains, vec![json!("x")]),
                    f("age", FilterOp::Ge, vec![json!(18)]),
                    f("gone", FilterOp::IsNull, vec![]),
                    f("id", FilterOp::In, vec![json!(1), json!(2)]),
                    f("@rid", FilterOp::Eq, vec![json!("#12:0")]),
                ]
            )
            .unwrap(),
            "SELECT FROM Person\nWHERE name = \"O'Brien \\\"Jr\\\"\"\n  AND note.left(3) = \"50%\"\n  AND `my field`.indexOf(\"x\") > -1\n  AND age >= 18\n  AND gone IS NULL\n  AND id IN [1, 2]\n  AND @rid = #12:0\nLIMIT 200"
        );
    }

    #[test]
    fn class_ddl() {
        let mut t = TableSchema {
            kind: kinds::TABLE.into(),
            name: "Person".into(),
            columns: vec![
                ColumnDef { name: "name".into(), data_type: "STRING".into(), nullable: false, ..Default::default() },
                ColumnDef { name: "boss".into(), data_type: "LINK".into(), nullable: true, ..Default::default() },
                ColumnDef { name: "seen".into(), data_type: "LONG|DOUBLE".into(), nullable: true, options: [("inferred".to_string(), "true".to_string())].into(), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["name".into()] }),
            indexes: vec![IndexDef { name: "Person.boss".into(), columns: vec!["boss".into()], unique: false, kind: None, filter: None, ..Default::default() }],
            foreign_keys: vec![ForeignKeyDef { columns: vec!["boss".into()], ref_table: "Person".into(), ref_columns: vec!["@rid".into()], ..Default::default() }],
            ..Default::default()
        };
        t.options.insert("extends".into(), "V".into());
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        assert_eq!(
            table_ddl(&t, all).unwrap(),
            "DROP CLASS Person IF EXISTS UNSAFE;\n\
             CREATE CLASS Person IF NOT EXISTS EXTENDS V;\n\
             CREATE PROPERTY Person.name IF NOT EXISTS STRING (NOTNULL TRUE);\n\
             CREATE PROPERTY Person.boss IF NOT EXISTS LINK;\n\
             CREATE INDEX Person.pk IF NOT EXISTS ON Person (name) UNIQUE;\n\
             CREATE INDEX Person.boss IF NOT EXISTS ON Person (boss) NOTUNIQUE;\n\
             ALTER PROPERTY Person.boss LINKEDCLASS Person;"
        );
    }

    #[test]
    fn inserts() {
        let v = ObjectRef { kind: VERTEX.into(), schema: None, name: "Person".into() };
        let s = insert_script(&v, &["@rid".into(), "name".into(), "tags".into(), "out_Knows".into()], &[vec![json!("#1:0"), json!("Ann"), json!("[1,2]"), json!("[\"#2:0\"]")]]).unwrap();
        assert_eq!(s, "CREATE VERTEX Person CONTENT {\"name\":\"Ann\",\"tags\":[1,2]};\n");
        let e = ObjectRef { kind: EDGE.into(), schema: None, name: "Knows".into() };
        let s = insert_script(&e, &["out".into(), "in".into(), "w".into()], &[vec![json!("#1:0"), json!("#1:1"), json!(2)]]).unwrap();
        assert_eq!(s, "CREATE EDGE Knows FROM #1:0 TO #1:1 CONTENT {\"w\":2};\n");
        assert!(insert_script(&e, &["w".into()], &[vec![json!(1)]]).is_err());
        let d = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "Doc".into() };
        assert!(insert_script(&d, &["a".into()], &[vec![json!(1)]]).unwrap().starts_with("INSERT INTO Doc CONTENT"));
    }

    #[test]
    fn updates() {
        let v = ObjectRef { kind: VERTEX.into(), schema: None, name: "Person".into() };
        let changes = vec![
            RowChange {
                key: vec![("@rid".into(), json!("#12:3"))],
                set: vec![("name".into(), json!("O'Brien \"Bob\"")), ("age".into(), Value::Null), ("tags".into(), json!("[1,2]"))], ..Default::default()
            },
            RowChange { key: vec![("code".into(), json!(7)), ("x".into(), Value::Null)], set: vec![("n".into(), json!(1))], ..Default::default() },
            RowChange { key: vec![("@rid".into(), json!("#1:0"))], set: vec![], ..Default::default() },
        ];
        assert_eq!(
            update_script(&v, &changes).unwrap(),
            "UPDATE Person SET name = \"O'Brien \\\"Bob\\\"\", age = null, tags = [1,2] WHERE @rid = #12:3;\n\
             UPDATE Person SET n = 1 WHERE code = 7 AND x IS NULL;\n"
        );
        let bad = vec![RowChange { key: vec![("@rid".into(), json!("#1:0"))], set: vec![("out_Knows".into(), json!("[]"))], ..Default::default() }];
        assert!(update_script(&v, &bad).is_err());
    }

    #[test]
    fn deletes() {
        let v = ObjectRef { kind: VERTEX.into(), schema: None, name: "Person".into() };
        let keys = vec![vec![("@rid".into(), json!("#12:3")), ("name".into(), json!("x"))], vec![("code".into(), json!("O'Brien")), ("x".into(), Value::Null)]];
        assert_eq!(
            delete_script(&v, &keys).unwrap(),
            "DELETE VERTEX Person WHERE @rid = #12:3;\nDELETE VERTEX Person WHERE code = \"O'Brien\" AND x IS NULL;\n"
        );
        let e = ObjectRef { kind: EDGE.into(), schema: None, name: "Knows".into() };
        assert_eq!(delete_script(&e, &[vec![("@rid".into(), json!("#20:1"))]]).unwrap(), "DELETE EDGE Knows WHERE @rid = #20:1;\n");
        let d = ObjectRef { kind: "class".into(), schema: None, name: "Doc".into() };
        assert_eq!(delete_script(&d, &[vec![("id".into(), json!(1))]]).unwrap(), "DELETE FROM Doc WHERE id = 1;\n");
        assert!(delete_script(&v, &[vec![]]).is_err());
        assert!(delete_script(&v, &[vec![("@class".into(), json!("Person"))]]).is_err());
    }

    #[test]
    fn statements_from_metadata() {
        let ix = json!({ "name": "Person.name", "configuration": { "type": "UNIQUE", "algorithm": "CELL_BTREE", "indexDefinition": { "className": "Person", "field": "name" } } });
        assert_eq!(index_statement(&ix), "CREATE INDEX Person.name ON Person (name) UNIQUE\n-- algoritmo: CELL_BTREE");
        let f = vec![("name".to_string(), json!("sum")), ("code".to_string(), json!("return a+b")), ("parameters".to_string(), json!(["a", "b"])), ("language".to_string(), json!("javascript"))];
        assert_eq!(function_statement(&f), "CREATE FUNCTION sum \"return a+b\" PARAMETERS [a, b] LANGUAGE javascript");
        let s = vec![("name".to_string(), json!("seq")), ("type".to_string(), json!("ORDERED")), ("start".to_string(), json!(0)), ("incr".to_string(), json!(1))];
        assert_eq!(sequence_statement(&s), "CREATE SEQUENCE seq TYPE ORDERED START 0 INCREMENT 1");
        let s = [vec![("value".to_string(), json!(42))], s].concat();
        assert_eq!(sequence_statement(&s), "CREATE SEQUENCE seq TYPE ORDERED START 0 INCREMENT 1", "not the current value");

        // Collation, engine and metadata, as the server reports them, and back.
        let ft = json!({ "name": "P.bio_ft", "configuration": { "type": "FULLTEXT", "algorithm": "LUCENE",
            "indexDefinition": { "className": "P", "field": "bio", "collate": "default" },
            "metadata": { "@type": "d", "@version": 0, "analyzer": "org.apache.lucene.analysis.en.EnglishAnalyzer" } } });
        let ci = json!({ "name": "P.nc", "configuration": { "type": "NOTUNIQUE", "algorithm": "CELL_BTREE",
            "indexDefinition": { "className": "P", "indexDefinitions": [{ "field": "name", "collate": "ci" }, { "field": "age", "collate": "default" }] },
            "metadata": { "@type": "d", "ignoreNullValues": true } } });
        let (cols, opts) = index_parts(&ft);
        assert_eq!(cols, vec!["bio"]);
        assert_eq!(opts.get("ENGINE").map(String::as_str), Some("LUCENE"));
        assert_eq!(opts.get("METADATA").map(String::as_str), Some("{\"analyzer\":\"org.apache.lucene.analysis.en.EnglishAnalyzer\"}"));
        let (cols2, opts2) = index_parts(&ci);
        assert_eq!(cols2, vec!["name COLLATE ci", "age"]);
        assert!(!opts2.contains_key("ENGINE"));
        let t = TableSchema {
            name: "P".into(),
            indexes: vec![
                IndexDef { name: "P.bio_ft".into(), columns: cols, kind: Some("FULLTEXT".into()), options: opts, ..Default::default() },
                IndexDef { name: "P.nc".into(), columns: cols2, kind: Some("NOTUNIQUE".into()), options: opts2, ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(
            table_ddl(&t, DdlParts { indexes: true, ..Default::default() }).unwrap(),
            "CREATE INDEX P.bio_ft ON P (bio) FULLTEXT ENGINE LUCENE METADATA {\"analyzer\":\"org.apache.lucene.analysis.en.EnglishAnalyzer\"};\n\
             CREATE INDEX P.nc ON P (name COLLATE ci, age) NOTUNIQUE METADATA {\"ignoreNullValues\":true};"
        );
    }
}

//! Snowflake DDL: tables with informational keys (PK / UNIQUE / FK),
//! AUTOINCREMENT, inline comments, clustering and transient tables; INSERT
//! scripts; the designer and the templates. Also turns INFORMATION_SCHEMA
//! and `SHOW … KEYS` rows into [`TableSchema`]s.

use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, ForeignKeyDef, IndexDef, KeyDef,
    RowChange, TableSchema,
};
use dbine_driver::{Error, Result};
use serde_json::Value as Json;
use std::collections::HashMap;

/// A catalog row by lowercase column name; NULLs are absent.
pub type Row = HashMap<String, String>;

pub const TABLE_TYPE: &str = "table_type";
pub const CLUSTER_BY: &str = "cluster_by";
pub const TASK: &str = "task";

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn table_name(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Double, schema.filter(|s| !s.is_empty()), name)
}

/// A Snowflake string literal: backslash is an escape there too.
pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

fn has(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|v| !v.trim().is_empty())
}

fn cols(names: &[String]) -> String {
    names.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
}

fn constraint(name: &Option<String>) -> String {
    has(name).map(|n| format!("CONSTRAINT {} ", q(n))).unwrap_or_default()
}

/// `CREATE OR REPLACE SEQUENCE` from an INFORMATION_SCHEMA.SEQUENCES row
/// (`start_value`, `increment`, `ordered`, `comment`). Named with its
/// schema, so it's made where it belongs whatever the session's schema.
/// Snowflake sequences have no MINVALUE / MAXVALUE / CYCLE / CACHE.
pub fn sequence_sql(schema: Option<&str>, name: &str, r: &Row) -> String {
    let n = |k: &str, d: &str| r.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string());
    let mut s = format!("CREATE OR REPLACE SEQUENCE {} START WITH {} INCREMENT BY {}", table_name(schema, name), n("start_value", "1"), n("increment", "1"));
    match r.get("ordered").map(String::as_str) {
        Some("YES") => s.push_str(" ORDER"),
        Some("NO") => s.push_str(" NOORDER"),
        _ => {}
    }
    if let Some(c) = r.get("comment").filter(|c| !c.is_empty()) {
        s.push_str(&format!(" COMMENT = {}", lit(c)));
    }
    s.push(';');
    s
}

/// A column as CREATE TABLE and `ADD COLUMN` write it, in the order GET_DDL
/// does: NOT NULL, default / identity, comment.
pub fn column_def(t: &TableSchema, c: &ColumnDef) -> String {
    let pk = t.primary_key.as_ref().is_some_and(|k| k.columns.contains(&c.name));
    let mut l = format!("{} {}", q(&c.name), c.data_type);
    if !c.nullable || pk {
        l.push_str(" NOT NULL");
    }
    if c.auto_increment {
        l.push_str(" AUTOINCREMENT");
    } else if let Some(d) = has(&c.default_value) {
        l.push_str(&format!(" DEFAULT {d}"));
    }
    if let Some(cm) = has(&c.comment) {
        l.push_str(&format!(" COMMENT {}", lit(cm)));
    }
    l
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let name = table_name(t.schema.as_deref(), &t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let mut lines: Vec<String> = t.columns.iter().map(|c| format!("    {}", column_def(t, c))).collect();
        if let Some(k) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
            lines.push(format!("    {}PRIMARY KEY ({})", constraint(&k.name), cols(&k.columns)));
        }
        let transient = t.options.get(TABLE_TYPE).is_some_and(|v| v.eq_ignore_ascii_case("transient"));
        let mut s = format!(
            "CREATE {}TABLE {}{name} (\n{}\n)",
            if transient { "TRANSIENT " } else { "" },
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            lines.join(",\n")
        );
        if let Some(c) = t.options.get(CLUSTER_BY).map(|v| v.trim()).filter(|v| !v.is_empty()) {
            s.push_str(&format!("\nCLUSTER BY ({c})"));
        }
        if let Some(cm) = has(&t.comment) {
            s.push_str(&format!("\nCOMMENT = {}", lit(cm)));
        }
        s.push(';');
        out.push(s);
    }
    // No indexes in Snowflake (standard tables): unique constraints only.
    if parts.indexes {
        for ix in t.indexes.iter().filter(|i| i.unique) {
            out.push(format!("ALTER TABLE {name} ADD {}UNIQUE ({});", constraint(&Some(ix.name.clone())), cols(&ix.columns)));
        }
    }
    if parts.foreign_keys {
        for fk in &t.foreign_keys {
            let target = table_name(fk.ref_schema.as_deref().or(t.schema.as_deref()), &fk.ref_table);
            let mut s = format!(
                "ALTER TABLE {name} ADD {}FOREIGN KEY ({}) REFERENCES {target} ({})",
                constraint(&fk.name),
                cols(&fk.columns),
                cols(&fk.ref_columns)
            );
            if let Some(a) = has(&fk.on_update) {
                s.push_str(&format!(" ON UPDATE {a}"));
            }
            if let Some(a) = has(&fk.on_delete) {
                s.push_str(&format!(" ON DELETE {a}"));
            }
            s.push(';');
            out.push(s);
        }
    }
    out.join("\n")
}

pub fn value(v: &Json) -> String {
    match v {
        Json::Null => "NULL".into(),
        Json::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Json::Number(n) => n.to_string(),
        Json::String(s) => lit(s),
        other => lit(&other.to_string()),
    }
}

pub fn insert_script(schema: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Json>]) -> String {
    let head = format!("INSERT INTO {} ({}) VALUES", table_name(schema, table), cols(columns));
    rows.chunks(100)
        .map(|chunk| {
            let tuples: Vec<String> =
                chunk.iter().map(|r| format!("({})", r.iter().map(value).collect::<Vec<_>>().join(", "))).collect();
            format!("{head}\n  {};", tuples.join(",\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `UPDATE … SET … WHERE …;` per edited row, with the INSERT literals.
/// The browse query restricted by the grid's column filters. Snowflake
/// literals read backslash escapes, so the LIKE escape character is
/// written `'\\'` (the shared `'\'` wouldn't close).
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, sql_condition, FilterOp, SqlFilterStyle};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let style = SqlFilterStyle { quote: Quote::Double, literal: &value, like: "LIKE", true_literal: "TRUE", false_literal: "FALSE" };
    let mut parts = Vec::new();
    for f in filters {
        let c = sql_condition(std::slice::from_ref(f), &style)?;
        let like = matches!(f.op, FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith);
        parts.push(match c.strip_suffix(" ESCAPE '\\'") {
            Some(s) if like => format!("{s} ESCAPE '\\\\'"),
            _ => c,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

pub fn update_script(schema: Option<&str>, table: &str, changes: &[RowChange]) -> String {
    dbine_driver::ddl::update_script_with(Quote::Double, schema, table, changes, &value)
}

/// `DELETE … WHERE <key>` per row key, with the same literals as [`update_script`].
pub fn delete_script(schema: Option<&str>, table: &str, keys: &[Vec<(String, Json)>]) -> String {
    dbine_driver::ddl::delete_script_with(Quote::Double, schema, table, keys, &value)
}

pub fn designer() -> DesignerSpec {
    let mut d = DesignerSpec::sql_table(vec![
        "NUMBER(38,0)", "NUMBER(10,2)", "INTEGER", "FLOAT", "BOOLEAN", "VARCHAR", "VARCHAR(100)", "CHAR(1)", "BINARY",
        "DATE", "TIME", "TIMESTAMP_NTZ", "TIMESTAMP_LTZ", "TIMESTAMP_TZ", "VARIANT", "OBJECT", "ARRAY", "GEOGRAPHY",
        "GEOMETRY", "VECTOR(FLOAT, 256)",
    ]);
    d.schemas = true;
    d.comments = true;
    d.indexes = false;
    d.table_options = vec![
        Field::new(TABLE_TYPE, "Tipo de tabla", FieldKind::Select(vec![("permanent", "Permanente"), ("transient", "Transitoria (sin Fail-safe)")]))
            .default_value("permanent"),
        Field::new(CLUSTER_BY, "Clave de clustering", FieldKind::Text)
            .placeholder("col1, TO_DATE(col2)")
            .help("Columnas o expresiones de CLUSTER BY, separadas por coma."),
    ];
    d
}

pub fn templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(kinds::VIEW, "Nueva vista", "CREATE OR REPLACE VIEW \"{schema}\".\"{name}\"\n  COMMENT = ''\nAS\nSELECT *\nFROM \"{schema}\".\"TABLA\";\n"),
        t(
            kinds::VIEW,
            "Nueva vista segura",
            "CREATE OR REPLACE SECURE VIEW \"{schema}\".\"{name}\"\nAS\nSELECT *\nFROM \"{schema}\".\"TABLA\"\nWHERE propietario = CURRENT_ROLE();\n",
        ),
        t(
            kinds::MATERIALIZED_VIEW,
            "Nueva vista materializada",
            "CREATE OR REPLACE MATERIALIZED VIEW \"{schema}\".\"{name}\"\nAS\nSELECT columna, COUNT(*) AS total\nFROM \"{schema}\".\"TABLA\"\nGROUP BY columna;\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función SQL",
            "CREATE OR REPLACE FUNCTION \"{schema}\".\"{name}\"(x NUMBER)\nRETURNS NUMBER\nLANGUAGE SQL\nAS\n$$\n  x * 2\n$$;\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función JavaScript",
            "CREATE OR REPLACE FUNCTION \"{schema}\".\"{name}\"(x FLOAT)\nRETURNS FLOAT\nLANGUAGE JAVASCRIPT\nAS\n$$\n  // Los argumentos se ven en mayúsculas.\n  return X * 2;\n$$;\n",
        ),
        t(
            kinds::PROCEDURE,
            "Nuevo procedimiento",
            "CREATE OR REPLACE PROCEDURE \"{schema}\".\"{name}\"(p_id NUMBER)\nRETURNS NUMBER\nLANGUAGE SQL\nAS\n$$\nDECLARE\n  total NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO :total FROM \"{schema}\".\"TABLA\" WHERE id = :p_id;\n  RETURN total;\nEND;\n$$;\n",
        ),
        t(kinds::SEQUENCE, "Nueva secuencia", "CREATE SEQUENCE \"{schema}\".\"{name}\"\n  START = 1\n  INCREMENT = 1\n  ORDER;\n"),
        t(
            kinds::STREAM,
            "Nuevo stream",
            "CREATE OR REPLACE STREAM \"{schema}\".\"{name}\"\n  ON TABLE \"{schema}\".\"TABLA\"\n  APPEND_ONLY = FALSE;\n",
        ),
        t(
            TASK,
            "Nueva tarea",
            "CREATE OR REPLACE TASK \"{schema}\".\"{name}\"\n  WAREHOUSE = MI_WAREHOUSE\n  SCHEDULE = 'USING CRON 0 * * * * UTC'\nAS\n  INSERT INTO \"{schema}\".\"DESTINO\" SELECT * FROM \"{schema}\".\"ORIGEN\";\n\n-- Las tareas se crean suspendidas:\nALTER TASK \"{schema}\".\"{name}\" RESUME;\n",
        ),
    ]
}

/// `TEXT(100)`, `NUMBER(38,0)`, `FLOAT`…
pub fn column_type(t: String, len: Option<String>, precision: Option<String>, scale: Option<String>) -> String {
    match (t.as_str(), len, precision, scale) {
        ("TEXT" | "BINARY", Some(l), _, _) => format!("{t}({l})"),
        ("NUMBER", _, Some(p), Some(s)) => format!("NUMBER({p},{s})"),
        _ => t,
    }
}

/// Names Snowflake makes up (`SYS_CONSTRAINT_…`) aren't worth repeating.
fn given_name(n: Option<&String>) -> Option<String> {
    n.filter(|n| !n.is_empty() && !n.starts_with("SYS_CONSTRAINT_")).cloned()
}

/// `LINEAR(A, B)` → `A, B`.
fn cluster_key(k: &str) -> String {
    let k = k.trim();
    k.strip_prefix("LINEAR(").and_then(|r| r.strip_suffix(')')).unwrap_or(k).to_string()
}

/// Rows → tables: `tables` / `columns` from INFORMATION_SCHEMA, the others
/// from `SHOW PRIMARY KEYS / UNIQUE KEYS / IMPORTED KEYS IN DATABASE`.
pub fn assemble(tables: &[Row], columns: &[Row], pks: &[Row], uniques: &[Row], fks: &[Row]) -> Vec<TableSchema> {
    let g = |r: &Row, k: &str| r.get(k).cloned().unwrap_or_default();
    let num = |r: &Row, k: &str| r.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let mut out: Vec<TableSchema> = tables
        .iter()
        .map(|r| {
            let mut t = TableSchema {
                kind: kinds::TABLE.into(),
                schema: Some(g(r, "table_schema")),
                name: g(r, "table_name"),
                comment: r.get("comment").filter(|c| !c.is_empty()).cloned(),
                ..Default::default()
            };
            if g(r, "is_transient") == "YES" {
                t.options.insert(TABLE_TYPE.into(), "transient".into());
            }
            if let Some(k) = r.get("clustering_key").filter(|k| !k.is_empty()) {
                t.options.insert(CLUSTER_BY.into(), cluster_key(k));
            }
            t
        })
        .collect();
    out.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
    let find = |out: &[TableSchema], schema: &str, name: &str| {
        out.iter().position(|t| t.schema.as_deref() == Some(schema) && t.name == name)
    };

    let mut cs: Vec<&Row> = columns.iter().collect();
    cs.sort_by_key(|r| num(r, "ordinal_position"));
    for r in cs {
        let Some(i) = find(&out, &g(r, "table_schema"), &g(r, "table_name")) else { continue };
        let auto = g(r, "is_identity") == "YES";
        out[i].columns.push(ColumnDef {
            name: g(r, "column_name"),
            data_type: column_type(
                g(r, "data_type"),
                r.get("character_maximum_length").cloned(),
                r.get("numeric_precision").cloned(),
                r.get("numeric_scale").cloned(),
            ),
            nullable: g(r, "is_nullable") != "NO",
            default_value: r.get("column_default").filter(|_| !auto).cloned(),
            auto_increment: auto,
            comment: r.get("comment").filter(|c| !c.is_empty()).cloned(),
            ..Default::default()
        });
    }

    let by_seq = |rows: &[Row]| {
        let mut v: Vec<Row> = rows.to_vec();
        v.sort_by_key(|r| num(r, "key_sequence"));
        v
    };
    for r in by_seq(pks) {
        let Some(i) = find(&out, &g(&r, "schema_name"), &g(&r, "table_name")) else { continue };
        let k = out[i].primary_key.get_or_insert_with(|| KeyDef { name: given_name(r.get("constraint_name")), columns: Vec::new() });
        k.columns.push(g(&r, "column_name"));
    }
    for r in by_seq(uniques) {
        let Some(i) = find(&out, &g(&r, "schema_name"), &g(&r, "table_name")) else { continue };
        let name = g(&r, "constraint_name");
        let ixs = &mut out[i].indexes;
        match ixs.iter_mut().find(|x| x.name == name) {
            Some(x) => x.columns.push(g(&r, "column_name")),
            None => ixs.push(IndexDef { name, columns: vec![g(&r, "column_name")], unique: true, kind: Some("UNIQUE".into()), filter: None, ..Default::default() }),
        }
    }
    let rule = |r: &Row, k: &str| r.get(k).filter(|v| !matches!(v.as_str(), "" | "NO ACTION" | "RESTRICT")).cloned();
    let mut seen: HashMap<(usize, String), usize> = HashMap::new();
    for r in by_seq(fks) {
        let Some(i) = find(&out, &g(&r, "fk_schema_name"), &g(&r, "fk_table_name")) else { continue };
        let fk_name = g(&r, "fk_name");
        let at = *seen.entry((i, fk_name.clone())).or_insert_with(|| {
            out[i].foreign_keys.push(ForeignKeyDef {
                name: given_name(Some(&fk_name)),
                ref_schema: r.get("pk_schema_name").cloned(),
                ref_table: g(&r, "pk_table_name"),
                on_delete: rule(&r, "delete_rule"),
                on_update: rule(&r, "update_rule"),
                ..Default::default()
            });
            out[i].foreign_keys.len() - 1
        });
        let fk = &mut out[i].foreign_keys[at];
        fk.columns.push(g(&r, "fk_column_name"));
        fk.ref_columns.push(g(&r, "pk_column_name"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filtered_browse_escapes_backslashes() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Json>| ColumnFilter { column: column.into(), op, values, sql: None };
        let browse = "SELECT *\nFROM \"PUBLIC\".\"T\"\nLIMIT 200";
        assert_eq!(
            filtered_browse(
                browse,
                &[
                    f("NOMBRE", FilterOp::Eq, vec![json!("O'Brien\\x")]),
                    f("NOTA", FilterOp::Contains, vec![json!("a_b")]),
                    f("NOTA2", FilterOp::Contains, vec![json!("ab")]),
                    f("N", FilterOp::Lt, vec![json!(3)]),
                    f("BAJA", FilterOp::NotNull, vec![]),
                    f("ID", FilterOp::In, vec![json!(1), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT *\nFROM \"PUBLIC\".\"T\"\nWHERE \"NOMBRE\" = 'O''Brien\\\\x'\n  AND \"NOTA\" LIKE '%a\\\\_b%' ESCAPE '\\\\'\n  AND \"NOTA2\" LIKE '%ab%'\n  AND \"N\" < 3\n  AND \"BAJA\" IS NOT NULL\n  AND \"ID\" IN (1, 2)\nLIMIT 200"
        );
    }

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn sample() -> Vec<TableSchema> {
        let tables = vec![
            row(&[("table_schema", "PUBLIC"), ("table_name", "PEDIDOS"), ("comment", "Pedidos"), ("clustering_key", "LINEAR(ESTADO)"), ("is_transient", "NO")]),
            row(&[("table_schema", "PUBLIC"), ("table_name", "CLIENTES"), ("is_transient", "YES")]),
        ];
        let c = |t: &str, n: &str, pos: &str, ty: &str, extra: &[(&str, &str)]| {
            let mut r = row(&[("table_schema", "PUBLIC"), ("table_name", t), ("column_name", n), ("ordinal_position", pos), ("data_type", ty), ("is_nullable", "YES"), ("is_identity", "NO")]);
            r.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
            r
        };
        let columns = vec![
            c("PEDIDOS", "ESTADO", "3", "TEXT", &[("character_maximum_length", "20"), ("column_default", "'nuevo'"), ("is_nullable", "NO")]),
            c("PEDIDOS", "ID", "1", "NUMBER", &[("numeric_precision", "38"), ("numeric_scale", "0"), ("is_identity", "YES"), ("is_nullable", "NO")]),
            c("PEDIDOS", "CLIENTE_ID", "2", "NUMBER", &[("numeric_precision", "38"), ("numeric_scale", "0"), ("comment", "dueño")]),
            c("CLIENTES", "ID", "1", "NUMBER", &[("numeric_precision", "38"), ("numeric_scale", "0"), ("is_nullable", "NO")]),
            c("CLIENTES", "EMAIL", "2", "TEXT", &[("character_maximum_length", "100")]),
        ];
        let pks = vec![
            row(&[("schema_name", "PUBLIC"), ("table_name", "PEDIDOS"), ("column_name", "ID"), ("key_sequence", "1"), ("constraint_name", "PK_PEDIDOS")]),
            row(&[("schema_name", "PUBLIC"), ("table_name", "CLIENTES"), ("column_name", "ID"), ("key_sequence", "1"), ("constraint_name", "SYS_CONSTRAINT_1a2b")]),
        ];
        let uniques = vec![row(&[("schema_name", "PUBLIC"), ("table_name", "CLIENTES"), ("column_name", "EMAIL"), ("key_sequence", "1"), ("constraint_name", "UQ_EMAIL")])];
        let fks = vec![row(&[
            ("pk_schema_name", "PUBLIC"), ("pk_table_name", "CLIENTES"), ("pk_column_name", "ID"), ("fk_schema_name", "PUBLIC"),
            ("fk_table_name", "PEDIDOS"), ("fk_column_name", "CLIENTE_ID"), ("key_sequence", "1"), ("update_rule", "NO ACTION"),
            ("delete_rule", "CASCADE"), ("fk_name", "FK_CLIENTE"),
        ])];
        assemble(&tables, &columns, &pks, &uniques, &fks)
    }

    #[test]
    fn update_script_per_row() {
        let c = RowChange {
            key: vec![("ID".into(), json!(7)), ("REGION".into(), Json::Null)],
            set: vec![("NOMBRE".into(), json!("O'Brien")), ("BAJA".into(), Json::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(Some("PUBLIC"), "T", &[c, RowChange::default()]),
            "UPDATE \"PUBLIC\".\"T\" SET \"NOMBRE\" = 'O''Brien', \"BAJA\" = NULL WHERE \"ID\" = 7 AND \"REGION\" IS NULL;"
        );
    }

    #[test]
    fn delete_script_per_row() {
        let keys = vec![vec![("NOMBRE".into(), json!("O'Brien")), ("REGION".into(), Json::Null)], vec![]];
        assert_eq!(
            delete_script(Some("PUBLIC"), "T", &keys),
            "DELETE FROM \"PUBLIC\".\"T\" WHERE \"NOMBRE\" = 'O''Brien' AND \"REGION\" IS NULL;"
        );
    }

    #[test]
    fn catalog_rows_become_tables() {
        let s = sample();
        assert_eq!(s.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["CLIENTES", "PEDIDOS"]);
        let (c, p) = (&s[0], &s[1]);
        assert_eq!(c.options.get(TABLE_TYPE).map(String::as_str), Some("transient"));
        assert_eq!(c.primary_key, Some(KeyDef { name: None, columns: vec!["ID".into()] }));
        assert_eq!(c.indexes, vec![IndexDef { name: "UQ_EMAIL".into(), columns: vec!["EMAIL".into()], unique: true, kind: Some("UNIQUE".into()), filter: None, ..Default::default() }]);
        assert_eq!(p.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect::<Vec<_>>(), vec![("ID", "NUMBER(38,0)"), ("CLIENTE_ID", "NUMBER(38,0)"), ("ESTADO", "TEXT(20)")]);
        assert!(p.columns[0].auto_increment && !p.columns[0].nullable);
        assert_eq!(p.columns[1].comment.as_deref(), Some("dueño"));
        assert_eq!(p.columns[2].default_value.as_deref(), Some("'nuevo'"));
        assert_eq!(p.comment.as_deref(), Some("Pedidos"));
        assert_eq!(p.options.get(CLUSTER_BY).map(String::as_str), Some("ESTADO"));
        assert_eq!(p.primary_key.as_ref().unwrap().name.as_deref(), Some("PK_PEDIDOS"));
        let fk = &p.foreign_keys[0];
        assert_eq!((fk.name.as_deref(), fk.ref_schema.as_deref(), fk.ref_table.as_str()), (Some("FK_CLIENTE"), Some("PUBLIC"), "CLIENTES"));
        assert_eq!((fk.columns.clone(), fk.ref_columns.clone()), (vec!["CLIENTE_ID".to_string()], vec!["ID".to_string()]));
        assert_eq!((fk.on_delete.as_deref(), fk.on_update.as_deref()), (Some("CASCADE"), None));
    }

    #[test]
    fn ddl_for_all_parts() {
        let s = sample();
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        let p = table_ddl(&s[1], all);
        assert!(p.starts_with("DROP TABLE IF EXISTS \"PUBLIC\".\"PEDIDOS\";\nCREATE TABLE \"PUBLIC\".\"PEDIDOS\" (\n"), "{p}");
        assert!(p.contains("    \"ID\" NUMBER(38,0) NOT NULL AUTOINCREMENT,\n"));
        assert!(p.contains("    \"CLIENTE_ID\" NUMBER(38,0) COMMENT 'dueño',\n"));
        assert!(p.contains("    \"ESTADO\" TEXT(20) NOT NULL DEFAULT 'nuevo',\n    CONSTRAINT \"PK_PEDIDOS\" PRIMARY KEY (\"ID\")\n)"));
        assert!(p.contains(")\nCLUSTER BY (ESTADO)\nCOMMENT = 'Pedidos';"));
        assert!(p.ends_with("ALTER TABLE \"PUBLIC\".\"PEDIDOS\" ADD CONSTRAINT \"FK_CLIENTE\" FOREIGN KEY (\"CLIENTE_ID\") REFERENCES \"PUBLIC\".\"CLIENTES\" (\"ID\") ON DELETE CASCADE;"));
        let c = table_ddl(&s[0], DdlParts { create: true, if_exists: true, indexes: true, ..Default::default() });
        assert!(c.starts_with("CREATE TRANSIENT TABLE IF NOT EXISTS \"PUBLIC\".\"CLIENTES\""), "{c}");
        assert!(c.contains("    PRIMARY KEY (\"ID\")\n);"));
        assert!(c.ends_with("ALTER TABLE \"PUBLIC\".\"CLIENTES\" ADD CONSTRAINT \"UQ_EMAIL\" UNIQUE (\"EMAIL\");"));
    }

    #[test]
    fn inserts_escape_backslashes() {
        let rows = vec![vec![json!(1), json!("O'Brien \\n"), Json::Null], vec![json!(2), json!(false), json!(1.5)]];
        let s = insert_script(Some("PUBLIC"), "T", &["A".into(), "B".into(), "C".into()], &rows);
        assert_eq!(s, "INSERT INTO \"PUBLIC\".\"T\" (\"A\", \"B\", \"C\") VALUES\n  (1, 'O''Brien \\\\n', NULL),\n  (2, FALSE, 1.5);");
    }

    #[test]
    fn sequences_are_named_with_their_schema() {
        let r = row(&[("start_value", "100"), ("increment", "5"), ("ordered", "NO"), ("comment", "folio's")]);
        assert_eq!(
            sequence_sql(Some("APP"), "SEQ_FOLIO", &r),
            "CREATE OR REPLACE SEQUENCE \"APP\".\"SEQ_FOLIO\" START WITH 100 INCREMENT BY 5 NOORDER COMMENT = 'folio''s';"
        );
        assert_eq!(sequence_sql(None, "S", &row(&[])), "CREATE OR REPLACE SEQUENCE \"S\" START WITH 1 INCREMENT BY 1;");
    }

    #[test]
    fn templates_cover_the_kinds() {
        let t = templates();
        for k in [kinds::VIEW, kinds::MATERIALIZED_VIEW, kinds::FUNCTION, kinds::PROCEDURE, kinds::SEQUENCE, kinds::STREAM, TASK] {
            assert!(t.iter().any(|x| x.kind == k && x.template.contains("\"{schema}\".\"{name}\"")), "{k}");
        }
    }
}

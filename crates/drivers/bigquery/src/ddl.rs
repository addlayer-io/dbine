//! BigQuery DDL: tables with informational (NOT ENFORCED) keys, partitioning,
//! clustering and descriptions; INSERT scripts with GoogleSQL literals;
//! the designer and the templates. Also turns the dataset's
//! INFORMATION_SCHEMA rows into [`TableSchema`]s.

use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, ForeignKeyDef, KeyDef, RowChange,
    TableSchema,
};
use dbine_driver::filter::{insert_where, sql_condition, ColumnFilter, FilterOp, SqlFilterStyle};
use dbine_driver::sql::Quote;
use dbine_driver::{Error, Result};
use serde_json::Value as Json;
use std::collections::HashMap;

/// A catalog row by lowercase column name; NULLs are absent.
pub type Row = HashMap<String, String>;

pub const PARTITION_BY: &str = "partition_by";
pub const CLUSTER_BY: &str = "cluster_by";

/// `` `name` `` (GoogleSQL escapes with a backslash).
pub fn ident(name: &str) -> String {
    format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
}

pub(crate) fn table_name(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", ident(s), ident(&t.name)),
        None => ident(&t.name),
    }
}

/// A GoogleSQL string literal: `''` isn't an escape there, `\'` is.
pub fn lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `"text"` / `'text'` (an option value as INFORMATION_SCHEMA shows it) → text.
pub fn unquote(s: &str) -> String {
    let s = s.trim();
    let q = s.chars().next();
    if s.len() < 2 || !matches!(q, Some('"' | '\'')) || !s.ends_with(q.unwrap_or(' ')) {
        return s.to_string();
    }
    let mut out = String::new();
    let mut it = s[1..s.len() - 1].chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

fn has(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|v| !v.trim().is_empty())
}

fn cols(names: &[String]) -> String {
    names.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ")
}

/// `a, b` or `` `a`, `b` `` → quoted list.
fn name_list(v: &str) -> String {
    v.split(',').map(|c| c.trim().trim_matches('`')).filter(|c| !c.is_empty()).map(ident).collect::<Vec<_>>().join(", ")
}

/// A column as CREATE TABLE and `ADD COLUMN` write it.
pub fn column_def(c: &ColumnDef) -> String {
    let mut l = format!("{} {}", ident(&c.name), c.data_type);
    if let Some(d) = has(&c.default_value) {
        l.push_str(&format!(" DEFAULT {d}"));
    }
    // An ARRAY is never NULL (it's empty) and takes no NOT NULL.
    if !c.nullable && !c.data_type.trim_start().to_ascii_uppercase().starts_with("ARRAY") {
        l.push_str(" NOT NULL");
    }
    if let Some(cm) = has(&c.comment) {
        l.push_str(&format!(" OPTIONS(description={})", lit(cm)));
    }
    l
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let name = table_name(t);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let mut lines: Vec<String> = t.columns.iter().map(|c| format!("  {}", column_def(c))).collect();
        if let Some(pk) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
            lines.push(format!("  PRIMARY KEY ({}) NOT ENFORCED", cols(&pk.columns)));
        }
        let mut s = format!(
            "CREATE TABLE {}{name} (\n{}\n)",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            lines.join(",\n")
        );
        if let Some(p) = t.options.get(PARTITION_BY).filter(|v| !v.trim().is_empty()) {
            s.push_str(&format!("\nPARTITION BY {}", p.trim()));
        }
        if let Some(c) = t.options.get(CLUSTER_BY).map(|v| name_list(v)).filter(|v| !v.is_empty()) {
            s.push_str(&format!("\nCLUSTER BY {c}"));
        }
        if let Some(cm) = has(&t.comment) {
            s.push_str(&format!("\nOPTIONS(description={})", lit(cm)));
        }
        s.push(';');
        out.push(s);
    }
    // Search and vector indexes are BigQuery's only ones.
    if parts.indexes {
        out.extend(t.indexes.iter().filter_map(|ix| crate::indexes::create(t, ix, parts.if_exists)));
    }
    if parts.foreign_keys {
        for fk in &t.foreign_keys {
            out.push(format!("ALTER TABLE {name} ADD {};", fk_clause(t, fk)));
        }
    }
    out.join("\n")
}

fn fk_clause(t: &TableSchema, fk: &ForeignKeyDef) -> String {
    let target = table_name(&TableSchema {
        schema: fk.ref_schema.clone().or_else(|| t.schema.clone()),
        name: fk.ref_table.clone(),
        ..Default::default()
    });
    let named = fk.name.as_deref().filter(|n| !n.is_empty()).map(|n| format!("CONSTRAINT {} ", ident(n))).unwrap_or_default();
    format!("{named}FOREIGN KEY ({}) REFERENCES {target}({}) NOT ENFORCED", cols(&fk.columns), cols(&fk.ref_columns))
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

pub fn insert_script(dataset: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Json>]) -> String {
    let name = table_name(&TableSchema { schema: dataset.map(str::to_string), name: table.into(), ..Default::default() });
    let head = format!("INSERT INTO {name} ({}) VALUES", cols(columns));
    rows.chunks(100)
        .map(|chunk| {
            let tuples: Vec<String> =
                chunk.iter().map(|r| format!("({})", r.iter().map(value).collect::<Vec<_>>().join(", "))).collect();
            format!("{head}\n  {};", tuples.join(",\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `UPDATE … SET … WHERE …;` per edited row. BigQuery rejects an UPDATE
/// without WHERE, so a row without key columns gets `WHERE TRUE`.
pub fn update_script(dataset: Option<&str>, table: &str, changes: &[RowChange]) -> String {
    let name = table_name(&TableSchema { schema: dataset.map(str::to_string), name: table.into(), ..Default::default() });
    changes
        .iter()
        .filter(|c| !c.set.is_empty())
        .map(|c| {
            let set: Vec<String> = c.set.iter().map(|(k, v)| format!("{} = {}", ident(k), value(v))).collect();
            let wh: Vec<String> = c
                .key
                .iter()
                .map(|(k, v)| if v.is_null() { format!("{} IS NULL", ident(k)) } else { format!("{} = {}", ident(k), value(v)) })
                .collect();
            let wh = if wh.is_empty() { "TRUE".to_string() } else { wh.join(" AND ") };
            format!("UPDATE {name} SET {} WHERE {wh};", set.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `DELETE … WHERE <key>` per row key, with the same names and literals as
/// [`update_script`]. A key without columns is skipped: it would delete
/// the whole table.
pub fn delete_script(dataset: Option<&str>, table: &str, keys: &[Vec<(String, Json)>]) -> String {
    let name = table_name(&TableSchema { schema: dataset.map(str::to_string), name: table.into(), ..Default::default() });
    keys.iter()
        .filter(|k| !k.is_empty())
        .map(|k| {
            let wh: Vec<String> =
                k.iter().map(|(c, v)| if v.is_null() { format!("{} IS NULL", ident(c)) } else { format!("{} = {}", ident(c), value(v)) }).collect();
            format!("DELETE FROM {name} WHERE {};", wh.join(" AND "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The browse query restricted by the grid's column filters, with
/// GoogleSQL literals. LIKE has no ESCAPE clause there: patterns rely on
/// the default `\` escape.
pub fn filtered_browse(browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let style = SqlFilterStyle { quote: Quote::Backtick, literal: &value, like: "LIKE", true_literal: "TRUE", false_literal: "FALSE" };
    let mut parts = Vec::new();
    for f in filters {
        let c = sql_condition(std::slice::from_ref(f), &style)?;
        let like = matches!(f.op, FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith);
        parts.push(match c.strip_suffix(" ESCAPE '\\'") {
            Some(s) if like => s.to_string(),
            _ => c,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

pub fn designer() -> DesignerSpec {
    let mut d = DesignerSpec::sql_table(vec![
        "INT64", "NUMERIC", "NUMERIC(10, 2)", "BIGNUMERIC", "FLOAT64", "BOOL", "STRING", "STRING(100)", "BYTES", "DATE",
        "DATETIME", "TIME", "TIMESTAMP", "JSON", "GEOGRAPHY", "INTERVAL", "ARRAY<STRING>", "ARRAY<INT64>",
        "STRUCT<a INT64, b STRING>",
    ]);
    d.auto_increment = false;
    d.comments = true;
    d.indexes = false;
    d.table_options = vec![
        Field::new(PARTITION_BY, "Particionar por", FieldKind::Text)
            .placeholder("DATE(creado) / _PARTITIONDATE / RANGE_BUCKET(id, GENERATE_ARRAY(0, 1000, 10))")
            .help("Expresión de PARTITION BY (una columna DATE/TIMESTAMP/DATETIME, una función de fecha o RANGE_BUCKET)."),
        Field::new(CLUSTER_BY, "Agrupar (CLUSTER BY)", FieldKind::Text)
            .placeholder("columna1, columna2")
            .help("Hasta cuatro columnas, separadas por coma."),
    ];
    d
}

pub fn templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(kinds::VIEW, "Nueva vista", "CREATE VIEW `{name}`\nOPTIONS(description = '')\nAS\nSELECT *\nFROM `tabla`;\n"),
        t(
            kinds::MATERIALIZED_VIEW,
            "Nueva vista materializada",
            "CREATE MATERIALIZED VIEW `{name}`\nOPTIONS(enable_refresh = TRUE, refresh_interval_minutes = 60)\nAS\nSELECT columna, COUNT(*) AS total\nFROM `tabla`\nGROUP BY columna;\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función SQL",
            "CREATE FUNCTION `{name}`(x INT64)\nRETURNS INT64\nAS (\n  x * 2\n);\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función JavaScript",
            "CREATE FUNCTION `{name}`(x FLOAT64)\nRETURNS FLOAT64\nLANGUAGE js\nAS r\"\"\"\n  return x * 2;\n\"\"\";\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función de tabla",
            "CREATE TABLE FUNCTION `{name}`(desde DATE)\nAS (\n  SELECT *\n  FROM `tabla`\n  WHERE fecha >= desde\n);\n",
        ),
        t(
            kinds::PROCEDURE,
            "Nuevo procedimiento",
            "CREATE PROCEDURE `{name}`(IN p_id INT64, OUT p_total INT64)\nBEGIN\n  SET p_total = (SELECT COUNT(*) FROM `tabla` WHERE id = p_id);\nEND;\n",
        ),
    ]
}

/// `PARTITION BY …` / `CLUSTER BY …` lines of a table's `ddl`.
fn ddl_clause(ddl: &str, keyword: &str) -> Option<String> {
    ddl.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(keyword))
        .map(|v| v.trim().trim_end_matches(';').trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Catalog rows → tables. `tables` are the base tables (TABLES, with `ddl`
/// on real BigQuery), `columns` COLUMNS, `options` the description rows of
/// TABLE_OPTIONS, `descriptions` COLUMN_FIELD_PATHS of top-level columns,
/// `keys` TABLE_CONSTRAINTS ⋈ KEY_COLUMN_USAGE and `refs`
/// CONSTRAINT_COLUMN_USAGE. `dataset` is the session's (refs to it stay
/// unqualified).
pub fn assemble(
    dataset: &str,
    tables: &[Row],
    columns: &[Row],
    options: &[Row],
    descriptions: &[Row],
    keys: &[Row],
    refs: &[Row],
) -> Vec<TableSchema> {
    let g = |r: &Row, k: &str| r.get(k).cloned().unwrap_or_default();
    let num = |r: &Row, k: &str| r.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let mut out: Vec<TableSchema> = tables
        .iter()
        .map(|r| {
            let mut t = TableSchema { kind: kinds::TABLE.into(), name: g(r, "table_name"), ..Default::default() };
            if let Some(ddl) = r.get("ddl") {
                if let Some(p) = ddl_clause(ddl, "PARTITION BY ") {
                    t.options.insert(PARTITION_BY.into(), p);
                }
                if let Some(c) = ddl_clause(ddl, "CLUSTER BY ") {
                    t.options.insert(CLUSTER_BY.into(), c);
                }
            }
            t
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    let find = |out: &[TableSchema], name: &str| out.iter().position(|t| t.name == name);

    let mut cols: Vec<&Row> = columns.iter().collect();
    cols.sort_by_key(|r| num(r, "ordinal_position"));
    for r in cols {
        let Some(i) = find(&out, &g(r, "table_name")) else { continue };
        let name = g(r, "column_name");
        let comment = descriptions
            .iter()
            .find(|d| g(d, "table_name") == out[i].name && g(d, "column_name") == name)
            .and_then(|d| d.get("description").cloned())
            .filter(|d| !d.is_empty());
        out[i].columns.push(ColumnDef {
            data_type: g(r, "data_type"),
            nullable: g(r, "is_nullable") != "NO",
            default_value: r.get("column_default").filter(|d| !d.eq_ignore_ascii_case("NULL")).cloned(),
            comment,
            name,
            ..Default::default()
        });
    }
    for r in options {
        if let Some(i) = find(&out, &g(r, "table_name")) {
            out[i].comment = Some(unquote(&g(r, "option_value"))).filter(|c| !c.is_empty());
        }
    }

    // Keys, column by column in order; foreign keys by constraint name.
    let mut keys: Vec<&Row> = keys.iter().collect();
    keys.sort_by_key(|r| (g(r, "table_name"), g(r, "constraint_name"), num(r, "ordinal_position")));
    let mut fks: Vec<(usize, String, ForeignKeyDef, Vec<usize>)> = Vec::new();
    for r in &keys {
        let Some(i) = find(&out, &g(r, "table_name")) else { continue };
        let (cname, col) = (g(r, "constraint_name"), g(r, "column_name"));
        if g(r, "constraint_type") == "PRIMARY KEY" {
            // BigQuery names it itself (`<table>.pk$`); it can't be named.
            out[i].primary_key.get_or_insert_with(KeyDef::default).columns.push(col);
            continue;
        }
        let at = match fks.iter().position(|f| f.0 == i && f.1 == cname) {
            Some(at) => at,
            None => {
                // Generated names (`fk$1`) can't be created again.
                let name = Some(cname.clone()).filter(|n| !n.contains('$'));
                fks.push((i, cname, ForeignKeyDef { name, ..Default::default() }, Vec::new()));
                fks.len() - 1
            }
        };
        fks[at].2.columns.push(col);
        fks[at].3.extend(r.get("position_in_unique_constraint").and_then(|p| p.parse::<usize>().ok()));
    }
    // Referenced table and columns (in the order of its primary key).
    for (i, cname, mut fk, positions) in fks {
        let target: Vec<&Row> = refs.iter().filter(|r| g(r, "constraint_name") == cname).collect();
        if let Some(r) = target.first() {
            fk.ref_table = g(r, "table_name");
            fk.ref_schema = r.get("table_schema").filter(|s| s.as_str() != dataset).cloned();
        }
        let pk = out.iter().find(|t| t.name == fk.ref_table).and_then(|t| t.primary_key.as_ref()).map(|k| k.columns.clone());
        fk.ref_columns = match pk {
            Some(pk)
                if fk.ref_schema.is_none()
                    && positions.len() == fk.columns.len()
                    && positions.iter().all(|p| (1..=pk.len()).contains(p)) =>
            {
                positions.iter().map(|p| pk[p - 1].clone()).collect()
            }
            _ => target.iter().map(|r| g(r, "column_name")).collect(),
        };
        out[i].foreign_keys.push(fk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filtered_browse_uses_googlesql_literals() {
        let f = |column: &str, op: FilterOp, values: Vec<Json>| ColumnFilter { column: column.into(), op, values, sql: None };
        let browse = "SELECT *\nFROM `ds`.`t`\nLIMIT 200";
        assert_eq!(
            filtered_browse(
                browse,
                &[
                    f("nombre", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("nota", FilterOp::StartsWith, vec![json!("a_b")]),
                    f("n", FilterOp::Gt, vec![json!(5)]),
                    f("baja", FilterOp::IsNull, vec![]),
                    f("id", FilterOp::NotIn, vec![json!(1), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT *\nFROM `ds`.`t`\nWHERE `nombre` = 'O\\'Brien'\n  AND `nota` LIKE 'a\\\\_b%'\n  AND `n` > 5\n  AND `baja` IS NULL\n  AND `id` NOT IN (1, 2)\nLIMIT 200"
        );
    }

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn sample() -> Vec<TableSchema> {
        let tables = vec![
            row(&[("table_name", "pedidos"), ("ddl", "CREATE TABLE `p.ds.pedidos`\n(\n  id INT64\n)\nPARTITION BY DATE(creado)\nCLUSTER BY cliente_id, estado\nOPTIONS(\n  description=\"Pedidos\"\n);")]),
            row(&[("table_name", "clientes")]),
        ];
        let columns = vec![
            row(&[("table_name", "pedidos"), ("column_name", "cliente_id"), ("ordinal_position", "2"), ("is_nullable", "YES"), ("data_type", "INT64"), ("column_default", "NULL")]),
            row(&[("table_name", "pedidos"), ("column_name", "id"), ("ordinal_position", "1"), ("is_nullable", "NO"), ("data_type", "INT64"), ("column_default", "NULL")]),
            row(&[("table_name", "pedidos"), ("column_name", "estado"), ("ordinal_position", "3"), ("is_nullable", "YES"), ("data_type", "STRING(20)"), ("column_default", "'nuevo'")]),
            row(&[("table_name", "clientes"), ("column_name", "id"), ("ordinal_position", "1"), ("is_nullable", "NO"), ("data_type", "INT64")]),
            row(&[("table_name", "una_vista"), ("column_name", "x"), ("ordinal_position", "1"), ("data_type", "INT64")]),
        ];
        let options = vec![row(&[("table_name", "pedidos"), ("option_name", "description"), ("option_value", "\"Pedidos \\\"del día\\\"\"")])];
        let desc = vec![row(&[("table_name", "pedidos"), ("column_name", "cliente_id"), ("description", "dueño")])];
        let keys = vec![
            row(&[("table_name", "pedidos"), ("constraint_name", "pedidos.pk$"), ("constraint_type", "PRIMARY KEY"), ("column_name", "id"), ("ordinal_position", "1")]),
            row(&[("table_name", "clientes"), ("constraint_name", "clientes.pk$"), ("constraint_type", "PRIMARY KEY"), ("column_name", "id"), ("ordinal_position", "1")]),
            row(&[("table_name", "pedidos"), ("constraint_name", "fk_cliente"), ("constraint_type", "FOREIGN KEY"), ("column_name", "cliente_id"), ("ordinal_position", "1"), ("position_in_unique_constraint", "1")]),
        ];
        let refs = vec![
            row(&[("constraint_name", "fk_cliente"), ("table_schema", "ds"), ("table_name", "clientes"), ("column_name", "id")]),
            row(&[("constraint_name", "pedidos.pk$"), ("table_schema", "ds"), ("table_name", "pedidos"), ("column_name", "id")]),
        ];
        assemble("ds", &tables, &columns, &options, &desc, &keys, &refs)
    }

    #[test]
    fn update_script_per_row() {
        let c = RowChange {
            key: vec![("id".into(), json!(7)), ("region".into(), Json::Null)],
            set: vec![("nombre".into(), json!("O'Brien")), ("baja".into(), Json::Null)], ..Default::default()
        };
        let all = RowChange { key: vec![], set: vec![("activo".into(), json!(false))], ..Default::default() };
        assert_eq!(
            update_script(Some("ventas"), "clientes", &[c, RowChange::default(), all]),
            "UPDATE `ventas`.`clientes` SET `nombre` = 'O\\'Brien', `baja` = NULL WHERE `id` = 7 AND `region` IS NULL;\n\
             UPDATE `ventas`.`clientes` SET `activo` = FALSE WHERE TRUE;"
        );
    }

    #[test]
    fn delete_script_per_row() {
        let keys = vec![vec![("nombre".into(), json!("O'Brien")), ("region".into(), Json::Null)], vec![]];
        assert_eq!(
            delete_script(Some("ventas"), "clientes", &keys),
            "DELETE FROM `ventas`.`clientes` WHERE `nombre` = 'O\\'Brien' AND `region` IS NULL;"
        );
    }

    #[test]
    fn catalog_rows_become_tables() {
        let s = sample();
        assert_eq!(s.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["clientes", "pedidos"]);
        let p = &s[1];
        assert_eq!(p.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "cliente_id", "estado"]);
        assert!(!p.columns[0].nullable && p.columns[1].nullable);
        assert_eq!(p.columns[0].default_value, None);
        assert_eq!(p.columns[2].default_value.as_deref(), Some("'nuevo'"));
        assert_eq!(p.columns[1].comment.as_deref(), Some("dueño"));
        assert_eq!(p.comment.as_deref(), Some("Pedidos \"del día\""));
        assert_eq!(p.primary_key, Some(KeyDef { name: None, columns: vec!["id".into()] }));
        assert_eq!(
            p.foreign_keys,
            vec![ForeignKeyDef {
                name: Some("fk_cliente".into()),
                columns: vec!["cliente_id".into()],
                ref_schema: None,
                ref_table: "clientes".into(),
                ref_columns: vec!["id".into()],
                on_delete: None,
                on_update: None
            }]
        );
        assert_eq!(p.options.get(PARTITION_BY).map(String::as_str), Some("DATE(creado)"));
        assert_eq!(p.options.get(CLUSTER_BY).map(String::as_str), Some("cliente_id, estado"));
    }

    #[test]
    fn ddl_with_keys_partitioning_and_descriptions() {
        let p = &sample()[1];
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        let s = table_ddl(p, all);
        assert!(s.starts_with("DROP TABLE IF EXISTS `pedidos`;\nCREATE TABLE `pedidos` (\n  `id` INT64 NOT NULL,\n"), "{s}");
        assert!(s.contains("  `cliente_id` INT64 OPTIONS(description='dueño'),\n"));
        assert!(s.contains("  `estado` STRING(20) DEFAULT 'nuevo',\n  PRIMARY KEY (`id`) NOT ENFORCED\n)"));
        assert!(s.contains(")\nPARTITION BY DATE(creado)\nCLUSTER BY `cliente_id`, `estado`\nOPTIONS(description='Pedidos \"del día\"');"));
        assert!(s.ends_with(
            "ALTER TABLE `pedidos` ADD CONSTRAINT `fk_cliente` FOREIGN KEY (`cliente_id`) REFERENCES `clientes`(`id`) NOT ENFORCED;"
        ));
        let only = table_ddl(p, DdlParts { create: true, if_exists: true, ..Default::default() });
        assert!(only.starts_with("CREATE TABLE IF NOT EXISTS `pedidos`") && !only.contains("ALTER"));
        let mut arr = p.clone();
        arr.schema = Some("otro".into());
        arr.columns[2].data_type = "ARRAY<STRING>".into();
        arr.columns[2].nullable = false;
        let s = table_ddl(&arr, DdlParts { create: true, foreign_keys: true, ..Default::default() });
        assert!(s.contains("`estado` ARRAY<STRING> DEFAULT 'nuevo',"), "{s}");
        assert!(s.contains("CREATE TABLE `otro`.`pedidos`") && s.contains("REFERENCES `otro`.`clientes`(`id`)"));
    }

    #[test]
    fn inserts_use_googlesql_literals() {
        let rows = vec![vec![json!(1), json!("O'Brien \\ x\ny"), Json::Null], vec![json!(2), json!(true), json!(1.5)]];
        let s = insert_script(Some("ds"), "t", &["a".into(), "b".into(), "c".into()], &rows);
        assert_eq!(s, "INSERT INTO `ds`.`t` (`a`, `b`, `c`) VALUES\n  (1, 'O\\'Brien \\\\ x\\ny', NULL),\n  (2, TRUE, 1.5);");
        assert_eq!(ident("a`b"), "`a\\`b`");
        assert_eq!(unquote("'a\\'b'"), "a'b");
        assert_eq!(unquote("plain"), "plain");
    }

    #[test]
    fn templates_cover_the_kinds() {
        let t = templates();
        for k in [kinds::VIEW, kinds::MATERIALIZED_VIEW, kinds::FUNCTION, kinds::PROCEDURE] {
            assert!(t.iter().any(|x| x.kind == k && x.template.contains("`{name}`")), "{k}");
        }
    }
}

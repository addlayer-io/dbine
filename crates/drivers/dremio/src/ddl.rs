//! Table designer (Iceberg tables: `$scratch`, Nessie / Arctic, Iceberg
//! catalogs), templates and INSERT scripts in Dremio's SQL. Dremio tables
//! have no keys, indexes, defaults or NOT NULL that it keeps; they take
//! `PARTITION BY` and `LOCALSORT BY`.

use dbine_driver::{kinds, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, RowChange, TableSchema};
use serde_json::Value;

pub fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `"a"."b"."t"` from a dotted schema path and a name.
pub fn path(schema: Option<&str>, name: &str) -> String {
    let mut parts: Vec<String> = schema.filter(|s| !s.is_empty()).map(|s| s.split('.').map(q).collect()).unwrap_or_default();
    parts.push(q(name));
    parts.join(".")
}

pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        schemas: true,
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        indexes: false,
        foreign_keys: false,
        table_options: vec![
            Field::new("partition_by", "Particionar por", FieldKind::Text)
                .placeholder("anio, month(fecha)")
                .help("Columnas o transformaciones de partición de Iceberg."),
            Field::new("localsort_by", "Ordenar localmente por", FieldKind::Text).placeholder("cliente_id"),
        ],
        ..DesignerSpec::sql_table(vec![
            "BOOLEAN",
            "INT",
            "BIGINT",
            "FLOAT",
            "DOUBLE",
            "DECIMAL(18,2)",
            "VARCHAR",
            "VARBINARY",
            "DATE",
            "TIME",
            "TIMESTAMP",
            "LIST<VARCHAR>",
            "STRUCT<a: INT, b: VARCHAR>",
        ])
    }
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let name = path(t.schema.as_deref(), &t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let cols: Vec<String> = t.columns.iter().map(|c| format!("    {} {}", q(&c.name), c.data_type)).collect();
        let mut s = format!("CREATE TABLE {}{name} (\n{}\n)", if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" }, cols.join(",\n"));
        for (key, clause) in [("partition_by", "PARTITION BY"), ("localsort_by", "LOCALSORT BY")] {
            if let Some(v) = t.options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
                let v = v.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(v);
                s.push_str(&format!("\n{clause} ({v})"));
            }
        }
        s.push(';');
        out.push(s);
    }
    out.join("\n")
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::VIEW,
            label: "Nueva vista",
            template: "CREATE OR REPLACE VIEW {schema}.\"{name}\" AS\nSELECT\n    t.id,\n    t.nombre\nFROM {schema}.\"tabla\" t\nWHERE t.activo = true;\n".into(),
        },
        CreateTemplate {
            kind: kinds::TABLE,
            label: "Nueva tabla desde una consulta (CTAS)",
            template: "-- En un origen con tablas Iceberg ($scratch, Nessie, Arctic…).\nCREATE TABLE {schema}.\"{name}\"\nPARTITION BY (categoria)\nAS\nSELECT categoria, count(*) AS cantidad\nFROM {schema}.\"tabla\"\nGROUP BY categoria;\n".into(),
        },
    ]
}

/// `YYYY-MM-DD` → `DATE`, `YYYY-MM-DD[ T]HH:MM[:SS[.f]]` → `TIMESTAMP`.
fn temporal(s: &str) -> Option<&'static str> {
    let b = s.as_bytes();
    let digits = |from: usize, to: usize| b.get(from..to).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    if !(b.len() >= 10 && digits(0, 4) && b[4] == b'-' && digits(5, 7) && b[7] == b'-' && digits(8, 10)) {
        return None;
    }
    if b.len() == 10 {
        return Some("DATE");
    }
    let ok = matches!(b[10], b' ' | b'T') && digits(11, 13) && b.get(13) == Some(&b':') && digits(14, 16) && b[16..].iter().all(|c| c.is_ascii_digit() || *c == b':' || *c == b'.');
    ok.then_some("TIMESTAMP")
}

pub fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => match temporal(s) {
            Some(kw) => format!("{kw} {}", lit(&s.replacen('T', " ", 1))),
            None => lit(s),
        },
        other => lit(&other.to_string()),
    }
}

/// Multi-row `INSERT … VALUES`, 100 rows per statement.
pub fn insert_script(schema: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let name = path(schema, table);
    let cols: Vec<String> = columns.iter().map(|c| q(c)).collect();
    rows.chunks(100)
        .map(|chunk| {
            let values: Vec<String> = chunk.iter().map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", "))).collect();
            format!("INSERT INTO {name} ({}) VALUES\n  {};", cols.join(", "), values.join(",\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `UPDATE … SET … WHERE …;` per edited row (Iceberg tables); a null key
/// value becomes `IS NULL`.
pub fn update_script(schema: Option<&str>, table: &str, changes: &[RowChange]) -> String {
    let name = path(schema, table);
    changes
        .iter()
        .filter(|c| !c.set.is_empty())
        .map(|c| {
            let set: Vec<String> = c.set.iter().map(|(k, v)| format!("{} = {}", q(k), literal(v))).collect();
            let wh: Vec<String> =
                c.key.iter().map(|(k, v)| if v.is_null() { format!("{} IS NULL", q(k)) } else { format!("{} = {}", q(k), literal(v)) }).collect();
            if wh.is_empty() {
                format!("UPDATE {name} SET {};", set.join(", "))
            } else {
                format!("UPDATE {name} SET {} WHERE {};", set.join(", "), wh.join(" AND "))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `DELETE … WHERE <key>` per row key (Iceberg tables), with the same
/// path and literals as [`update_script`]. A key without columns is
/// skipped: it would delete the whole table.
pub fn delete_script(schema: Option<&str>, table: &str, keys: &[Vec<(String, Value)>]) -> String {
    let name = path(schema, table);
    keys.iter()
        .filter(|k| !k.is_empty())
        .map(|k| {
            let wh: Vec<String> =
                k.iter().map(|(c, v)| if v.is_null() { format!("{} IS NULL", q(c)) } else { format!("{} = {}", q(c), literal(v)) }).collect();
            format!("DELETE FROM {name} WHERE {};", wh.join(" AND "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;
    use serde_json::json;

    #[test]
    fn update_script_per_row() {
        let c = RowChange {
            key: vec![("id".into(), json!(7)), ("region".into(), Value::Null)],
            set: vec![("nombre".into(), json!("O'Brien")), ("baja".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(Some("nessie.ventas"), "clientes", &[c, RowChange::default()]),
            "UPDATE \"nessie\".\"ventas\".\"clientes\" SET \"nombre\" = 'O''Brien', \"baja\" = NULL \
             WHERE \"id\" = 7 AND \"region\" IS NULL;"
        );
    }

    #[test]
    fn delete_script_per_row() {
        let keys = vec![vec![("nombre".into(), json!("O'Brien")), ("region".into(), Value::Null)], vec![]];
        assert_eq!(
            delete_script(Some("nessie.ventas"), "clientes", &keys),
            "DELETE FROM \"nessie\".\"ventas\".\"clientes\" WHERE \"nombre\" = 'O''Brien' AND \"region\" IS NULL;"
        );
    }

    #[test]
    fn ddl_and_inserts() {
        let t = TableSchema {
            kind: kinds::TABLE.into(),
            schema: Some("$scratch".into()),
            name: "pedidos".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "BIGINT".into(), ..Default::default() },
                ColumnDef { name: "fecha".into(), data_type: "DATE".into(), ..Default::default() },
            ],
            options: [("partition_by".to_string(), "(month(fecha))".to_string())].into(),
            ..Default::default()
        };
        let s = table_ddl(&t, DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true });
        assert_eq!(s, "DROP TABLE IF EXISTS \"$scratch\".\"pedidos\";\nCREATE TABLE \"$scratch\".\"pedidos\" (\n    \"id\" BIGINT,\n    \"fecha\" DATE\n)\nPARTITION BY (month(fecha));");
        assert_eq!(path(Some("sp.carpeta"), "t"), "\"sp\".\"carpeta\".\"t\"");
        let ins = insert_script(Some("sp"), "t", &["a".into(), "b".into(), "c".into()], &[vec![json!("2024-01-31"), json!("2024-01-31T10:00:00"), json!("O'k")], vec![Value::Null, json!(true), json!(1.5)]]);
        assert_eq!(ins, "INSERT INTO \"sp\".\"t\" (\"a\", \"b\", \"c\") VALUES\n  (DATE '2024-01-31', TIMESTAMP '2024-01-31 10:00:00', 'O''k'),\n  (NULL, TRUE, 1.5);");
        assert!(designer().table_options.iter().any(|f| f.key == "partition_by"));
        assert!(templates().iter().all(|t| t.template.contains("{schema}") && t.template.contains("{name}")));
    }
}

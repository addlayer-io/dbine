//! Table designer, CREATE TABLE / STABLE, templates and INSERT scripts in
//! TDengine's dialect. The first column of every table is its TIMESTAMP
//! key (no PRIMARY KEY clause); columns flagged `tag` make the table a
//! supertable (`CREATE STABLE … TAGS (…)`). No NOT NULL, defaults, indexes
//! or foreign keys; the table takes `COMMENT` and `TTL`.

use dbine_driver::{kinds, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, RowChange, TableSchema};
use serde_json::Value;

pub const SUPERTABLE: &str = "supertable";
pub const SUBTABLE: &str = "subtable";
/// `ColumnDef::options` key marking a tag column.
pub const TAG: &str = "tag";

pub fn q(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

pub fn qualified(db: Option<&str>, name: &str) -> String {
    match db.filter(|d| !d.is_empty()) {
        Some(d) => format!("{}.{}", q(d), q(name)),
        None => q(name),
    }
}

pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        kind: kinds::TABLE,
        label: "Nueva tabla",
        schemas: false,
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: true,
        indexes: false,
        foreign_keys: false,
        column_options: vec![Field::new(TAG, "Tag", FieldKind::Bool)
            .help("Si alguna columna es tag, se crea una supertabla (CREATE STABLE … TAGS).")],
        table_options: vec![Field::new("ttl", "TTL (días)", FieldKind::Number)
            .placeholder("(sin TTL)")
            .help("Solo tablas normales: días hasta que TDengine la borra.")],
        ..DesignerSpec::sql_table(vec![
            "TIMESTAMP",
            "BOOL",
            "TINYINT",
            "SMALLINT",
            "INT",
            "BIGINT",
            "TINYINT UNSIGNED",
            "INT UNSIGNED",
            "BIGINT UNSIGNED",
            "FLOAT",
            "DOUBLE",
            "DECIMAL(18,2)",
            "VARCHAR(64)",
            "NCHAR(64)",
            "VARBINARY(64)",
            "GEOMETRY(64)",
            "JSON",
        ])
    }
}

fn is_tag(c: &dbine_driver::ColumnDef) -> bool {
    c.options.get(TAG).is_some_and(|v| v == "true" || v == "1")
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let name = qualified(t.schema.as_deref(), &t.name);
    let stable = t.kind == SUPERTABLE || t.columns.iter().any(is_tag);
    let what = if stable { "STABLE" } else { "TABLE" };
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP {what} {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let col = |c: &dbine_driver::ColumnDef| format!("{} {}", q(&c.name), c.data_type);
        let cols: Vec<String> = t.columns.iter().filter(|c| !is_tag(c)).map(col).collect();
        let tags: Vec<String> = t.columns.iter().filter(|c| is_tag(c)).map(col).collect();
        let mut s = format!(
            "CREATE {what} {}{name} (\n    {}\n)",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            cols.join(",\n    ")
        );
        if stable {
            s.push_str(&format!(" TAGS (\n    {}\n)", tags.join(",\n    ")));
        }
        if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
            s.push_str(&format!(" COMMENT {}", lit(c)));
        }
        if !stable {
            if let Some(ttl) = t.options.get("ttl").map(|v| v.trim()).filter(|v| !v.is_empty()) {
                s.push_str(&format!(" TTL {ttl}"));
            }
        }
        s.push(';');
        out.push(s);
    }
    if parts.indexes {
        out.extend(t.indexes.iter().map(|ix| index_ddl(t, ix)));
    }
    out.join("\n")
}

/// `CREATE INDEX` of a supertable's tag index (one tag each). The index
/// name takes no database: it goes to the supertable's.
pub fn index_ddl(t: &TableSchema, ix: &dbine_driver::IndexDef) -> String {
    format!(
        "CREATE INDEX {} ON {} ({});",
        q(&ix.name),
        qualified(t.schema.as_deref(), &t.name),
        ix.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
    )
}

/// Whether a tag index is the one TDengine makes by itself on a
/// supertable's first tag (`<tag>_<supertable>`), which isn't created or
/// dropped by hand.
pub fn is_implicit_index(table: &str, first_tag: Option<&str>, index: &str, column: &str) -> bool {
    first_tag == Some(column) && index == format!("{column}_{table}")
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: SUPERTABLE,
            label: "Nueva supertabla",
            template: "CREATE STABLE `{schema}`.`{name}` (\n    ts TIMESTAMP,\n    valor DOUBLE,\n    estado VARCHAR(20)\n) TAGS (\n    ubicacion VARCHAR(64),\n    grupo INT\n);\n".into(),
        },
        CreateTemplate {
            kind: SUBTABLE,
            label: "Nueva subtabla",
            template: "-- Una subtabla por dispositivo, con los valores de sus tags.\nCREATE TABLE `{schema}`.`{name}` USING `{schema}`.`supertabla` (ubicacion, grupo) TAGS ('planta-1', 1);\n".into(),
        },
        CreateTemplate {
            kind: kinds::VIEW,
            label: "Nueva vista",
            template: "-- Solo en TDengine Enterprise.\nCREATE VIEW `{schema}`.`{name}` AS\nSELECT _wstart AS inicio, avg(valor) AS promedio\nFROM `{schema}`.`supertabla`\nINTERVAL(1m);\n".into(),
        },
        CreateTemplate {
            kind: kinds::STREAM,
            label: "Nuevo stream",
            template: "-- Calcula agregados continuos en una tabla destino.\nCREATE STREAM IF NOT EXISTS `{name}` TRIGGER AT_ONCE\nINTO `{schema}`.`{name}_salida` AS\nSELECT _wstart, count(*) AS cantidad, avg(valor) AS promedio\nFROM `{schema}`.`supertabla`\nPARTITION BY tbname\nINTERVAL(1m);\n".into(),
        },
        CreateTemplate {
            kind: kinds::TOPIC,
            label: "Nuevo tópico",
            template: "-- Suscripción de datos (TMQ) sobre una consulta.\nCREATE TOPIC IF NOT EXISTS `{name}` AS\nSELECT ts, valor FROM `{schema}`.`supertabla`;\n".into(),
        },
    ]
}

/// A value as a TDengine literal; `0x…` strings (binary cells) as `'\x…'`.
pub fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => match s.strip_prefix("0x").filter(|h| !h.is_empty() && h.len() % 2 == 0 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            Some(hex) => format!("'\\x{hex}'"),
            None => lit(s),
        },
        other => lit(&other.to_string()),
    }
}

/// `INSERT INTO … (cols) VALUES (…) (…)`, 100 rows per statement.
pub fn insert_script(db: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let name = qualified(db, table);
    let cols: Vec<String> = columns.iter().map(|c| q(c)).collect();
    rows.chunks(100)
        .map(|chunk| {
            let values: Vec<String> =
                chunk.iter().map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", "))).collect();
            format!("INSERT INTO {name} ({}) VALUES\n  {};", cols.join(", "), values.join("\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Edited rows as `INSERT INTO t (key…, cols…) VALUES (…);`: TDengine has
/// no UPDATE, and writing a row with an existing timestamp overwrites it.
/// The key (the timestamp, plus the composite key column if any) has to be
/// there and has to have a value.
pub fn update_script(db: Option<&str>, table: &str, changes: &[RowChange]) -> Result<String, String> {
    let name = qualified(db, table);
    let mut out = Vec::new();
    for c in changes.iter().filter(|c| !c.set.is_empty()) {
        if c.key.is_empty() || c.key.iter().any(|(_, v)| v.is_null()) {
            return Err("TDengine modifica una fila reescribiendo su marca de tiempo: la clave de la fila tiene que incluir el timestamp".into());
        }
        let pairs: Vec<&(String, Value)> =
            c.key.iter().chain(c.set.iter().filter(|(k, _)| !c.key.iter().any(|(kk, _)| kk == k))).collect();
        out.push(format!(
            "INSERT INTO {name} ({}) VALUES ({});",
            pairs.iter().map(|(k, _)| q(k)).collect::<Vec<_>>().join(", "),
            pairs.iter().map(|(_, v)| literal(v)).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(out.join("\n"))
}

/// Deleted rows as `DELETE FROM t WHERE ts = …;`. TDengine's DELETE only
/// takes a time window on the timestamp column, so the key has to be just
/// the timestamp: with a composite key column it would also drop the other
/// rows at that instant. On a supertable the row's `tbname` names the
/// subtable to delete from (the supertable itself would delete that instant
/// in every subtable).
pub fn delete_script(db: Option<&str>, table: &str, supertable: bool, keys: &[Vec<(String, Value)>]) -> Result<String, String> {
    let mut out = Vec::new();
    for key in keys {
        let sub = key.iter().find(|(k, _)| k.eq_ignore_ascii_case("tbname")).and_then(|(_, v)| v.as_str());
        let rest: Vec<&(String, Value)> = key.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("tbname")).collect();
        let [(ts, time)] = rest.as_slice() else {
            return Err(if rest.is_empty() {
                "TDengine borra filas por su marca de tiempo: la clave de la fila tiene que incluir el timestamp".into()
            } else {
                "TDengine solo borra por rango de tiempo: con una clave compuesta el borrado se llevaría también las otras filas de esa marca de tiempo".into()
            });
        };
        if time.is_null() {
            return Err("TDengine borra filas por su marca de tiempo: la fila no tiene timestamp".into());
        }
        let name = match (supertable, sub) {
            (_, Some(t)) => qualified(db, t),
            (false, None) => qualified(db, table),
            (true, None) => {
                return Err("en una supertabla el borrado alcanza a todas sus subtablas: la clave tiene que incluir tbname".into())
            }
        };
        out.push(format!("DELETE FROM {name} WHERE {} = {};", q(ts), literal(time)));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;
    use serde_json::json;

    fn c(name: &str, ty: &str, tag: bool) -> ColumnDef {
        let mut c = ColumnDef { name: name.into(), data_type: ty.into(), ..Default::default() };
        if tag {
            c.options.insert(TAG.into(), "true".into());
        }
        c
    }

    #[test]
    fn update_script_overwrites_timestamp() {
        let c = RowChange {
            key: vec![("ts".into(), json!("2024-01-31 10:00:00.000"))],
            set: vec![("nombre".into(), json!("O'Brien")), ("v".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(Some("iot"), "d1", &[c, RowChange::default()]).unwrap(),
            "INSERT INTO `iot`.`d1` (`ts`, `nombre`, `v`) VALUES ('2024-01-31 10:00:00.000', 'O''Brien', NULL);"
        );
        let no_ts = RowChange { key: vec![], set: vec![("v".into(), json!(1))], ..Default::default() };
        assert!(update_script(None, "d1", &[no_ts]).is_err());
        let null_ts = RowChange { key: vec![("ts".into(), Value::Null)], set: vec![("v".into(), json!(1))], ..Default::default() };
        assert!(update_script(None, "d1", &[null_ts]).is_err());
    }

    #[test]
    fn delete_script_by_timestamp() {
        let keys = vec![vec![("ts".into(), json!("2024-01-31 10:00:00.000"))], vec![("ts".into(), json!(1706695200000_i64))]];
        assert_eq!(
            delete_script(Some("iot"), "d1", false, &keys).unwrap(),
            "DELETE FROM `iot`.`d1` WHERE `ts` = '2024-01-31 10:00:00.000';\nDELETE FROM `iot`.`d1` WHERE `ts` = 1706695200000;"
        );
        let sub = vec![vec![("tbname".into(), json!("d'1")), ("ts".into(), json!("2024-01-31 10:00:00.000"))]];
        assert_eq!(delete_script(Some("iot"), "st", true, &sub).unwrap(), "DELETE FROM `iot`.`d'1` WHERE `ts` = '2024-01-31 10:00:00.000';");
        assert!(delete_script(Some("iot"), "st", true, &keys[..1]).is_err());
        let composite = vec![vec![("ts".into(), json!("2024-01-31 10:00:00.000")), ("k".into(), Value::Null)]];
        assert!(delete_script(None, "d1", false, &composite).is_err());
        assert!(delete_script(None, "d1", false, &[vec![("ts".into(), Value::Null)]]).is_err());
        assert!(delete_script(None, "d1", false, &[vec![]]).is_err());
    }

    #[test]
    fn stables_and_tables() {
        let t = TableSchema {
            kind: kinds::TABLE.into(),
            schema: Some("iot".into()),
            name: "medidas".into(),
            columns: vec![c("ts", "TIMESTAMP", false), c("v", "DOUBLE", false), c("loc", "VARCHAR(20)", true)],
            comment: Some("o'k".into()),
            ..Default::default()
        };
        let s = table_ddl(&t, DdlParts { drop: true, if_exists: true, create: true, ..Default::default() });
        assert_eq!(
            s,
            "DROP STABLE IF EXISTS `iot`.`medidas`;\nCREATE STABLE `iot`.`medidas` (\n    `ts` TIMESTAMP,\n    `v` DOUBLE\n) TAGS (\n    `loc` VARCHAR(20)\n) COMMENT 'o''k';"
        );
        let mut n = t.clone();
        n.columns.pop();
        n.options.insert("ttl".into(), "7".into());
        let s = table_ddl(&n, DdlParts { if_exists: true, create: true, ..Default::default() });
        assert!(s.starts_with("CREATE TABLE IF NOT EXISTS `iot`.`medidas` (") && s.ends_with("COMMENT 'o''k' TTL 7;"), "{s}");
        assert_eq!(table_ddl(&n, DdlParts { indexes: true, foreign_keys: true, ..Default::default() }), "");
    }

    #[test]
    fn inserts() {
        let s = insert_script(Some("d"), "t", &["ts".into(), "s".into(), "b".into(), "x".into()], &[
            vec![json!("2024-01-01 00:00:00.000"), json!("O'Brien"), json!("0xCAFE"), Value::Null],
            vec![json!("2024-01-01 00:00:01.000"), json!(true), json!(1.5), json!(2)],
        ]);
        assert_eq!(
            s,
            "INSERT INTO `d`.`t` (`ts`, `s`, `b`, `x`) VALUES\n  ('2024-01-01 00:00:00.000', 'O''Brien', '\\xCAFE', NULL)\n  ('2024-01-01 00:00:01.000', true, 1.5, 2);"
        );
        assert!(templates().iter().all(|t| t.template.contains("{name}")));
        assert!(designer().column_options.iter().any(|f| f.key == TAG));
    }
}

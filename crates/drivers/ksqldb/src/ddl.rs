//! ksqlDB DDL: the stream / table designer, `SHOW … EXTENDED` read into
//! [`TableSchema`]s, and CREATE STREAM / CREATE TABLE / INSERT scripts.
//!
//! A source is a stream or a table over a Kafka topic: its key columns are
//! `KEY` (stream) or `PRIMARY KEY` (table), and the topic and formats go in
//! `WITH (…)`. There are no schemas, indexes, foreign keys, defaults,
//! nullability nor comments.

use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, KeyDef, Result, RowChange,
    TableSchema,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// `WITH (…)` properties, in the order they are written. `object` (STREAM /
/// TABLE) is the designer's choice, not a property.
const PROPERTIES: [&str; 5] = ["KAFKA_TOPIC", "KEY_FORMAT", "VALUE_FORMAT", "PARTITIONS", "TIMESTAMP"];

pub(crate) fn q(s: &str) -> String {
    quote_ident(Quote::Backtick, s)
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

const FORMATS: [(&str, &str); 6] =
    [("JSON", "JSON"), ("AVRO", "AVRO"), ("PROTOBUF", "PROTOBUF"), ("JSON_SR", "JSON_SR"), ("DELIMITED", "DELIMITED"), ("KAFKA", "KAFKA")];

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        kind: kinds::STREAM,
        label: "Nuevo stream",
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        indexes: false,
        foreign_keys: false,
        table_options: vec![
            Field::new("object", "Objeto", FieldKind::Select(vec![("STREAM", "Stream"), ("TABLE", "Tabla")]))
                .default_value("STREAM")
                .help("Un stream usa columnas KEY; una tabla exige PRIMARY KEY."),
            Field::new("KAFKA_TOPIC", "Topic de Kafka", FieldKind::Text)
                .help("Si no existe se crea con PARTITIONS. Vacío = el nombre del objeto."),
            Field::new("VALUE_FORMAT", "Formato del valor", FieldKind::Select(FORMATS.to_vec())).default_value("JSON"),
            Field::new("KEY_FORMAT", "Formato de la clave", FieldKind::Select(FORMATS.iter().copied().chain([("NONE", "NONE")]).collect()))
                .default_value("KAFKA"),
            Field::new("PARTITIONS", "Particiones", FieldKind::Number)
                .default_value("1")
                .help("Solo para crear el topic; si ya existe debe coincidir o quedar vacío."),
            Field::new("TIMESTAMP", "Columna de timestamp", FieldKind::Text).help("Opcional: columna que da el tiempo del evento."),
        ],
        ..DesignerSpec::sql_table(vec![
            "INT",
            "BIGINT",
            "DOUBLE",
            "DECIMAL(10, 2)",
            "BOOLEAN",
            "VARCHAR",
            "STRING",
            "BYTES",
            "DATE",
            "TIME",
            "TIMESTAMP",
            "ARRAY<VARCHAR>",
            "MAP<VARCHAR, INT>",
            "STRUCT<A INT, B VARCHAR>",
        ])
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::STREAM,
            label: "Nuevo stream derivado (CREATE STREAM AS SELECT)",
            template: "-- Consulta persistente: escribe en un topic nuevo lo que llega al stream de origen.\n\
                       CREATE STREAM `{name}`\n  WITH (KAFKA_TOPIC='{name}', VALUE_FORMAT='JSON', PARTITIONS=1) AS\n\
                       SELECT *\nFROM `ORIGEN`\nWHERE `COLUMNA` IS NOT NULL\nEMIT CHANGES;\n"
                .into(),
        },
        CreateTemplate {
            kind: kinds::TABLE,
            label: "Nueva tabla agregada (CREATE TABLE AS SELECT)",
            template: "-- Tabla materializada: se puede consultar con SELECT … WHERE clave = …\n\
                       CREATE TABLE `{name}` AS\nSELECT `CLAVE`, COUNT(*) AS `TOTAL`, LATEST_BY_OFFSET(`VALOR`) AS `ULTIMO`\n\
                       FROM `ORIGEN`\nWINDOW TUMBLING (SIZE 1 HOUR)\nGROUP BY `CLAVE`\nEMIT CHANGES;\n"
                .into(),
        },
        CreateTemplate {
            kind: "connector",
            label: "Nuevo conector",
            template: "-- Requiere Kafka Connect configurado en ksqlDB (ksql.connect.url).\n\
                       CREATE SOURCE CONNECTOR `{name}` WITH (\n  'connector.class' = 'io.confluent.connect.jdbc.JdbcSourceConnector',\n  \
                       'connection.url'  = 'jdbc:postgresql://servidor:5432/base',\n  'mode'            = 'incrementing',\n  \
                       'incrementing.column.name' = 'id',\n  'topic.prefix'    = 'jdbc_',\n  'key'             = 'id'\n);\n"
                .into(),
        },
    ]
}

/// Whether the source is a TABLE (else a STREAM).
pub(crate) fn is_table(t: &TableSchema) -> bool {
    match t.options.get("object").map(|o| o.trim()).filter(|o| !o.is_empty()) {
        Some(o) => o.eq_ignore_ascii_case("TABLE"),
        None => t.kind == kinds::TABLE,
    }
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    let object = if is_table(t) { "TABLE" } else { "STREAM" };
    let name = q(&t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP {object} {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let keys: Vec<&str> = t.primary_key.iter().flat_map(|k| k.columns.iter().map(String::as_str)).collect();
        if object == "TABLE" && keys.is_empty() {
            return Err(Error::Query(format!("una tabla de ksqlDB necesita una columna PRIMARY KEY: falta en {}", t.name)));
        }
        let key_word = if object == "TABLE" { " PRIMARY KEY" } else { " KEY" };
        let cols: Vec<String> = t
            .columns
            .iter()
            .map(|c| format!("    {} {}{}", q(&c.name), c.data_type, if keys.contains(&c.name.as_str()) { key_word } else { "" }))
            .collect();
        let mut props: BTreeMap<&str, String> = PROPERTIES
            .iter()
            .filter_map(|k| Some((*k, t.options.get(*k).map(|v| v.trim()).filter(|v| !v.is_empty())?.to_string())))
            .collect();
        props.entry("KAFKA_TOPIC").or_insert_with(|| t.name.clone());
        props.entry("VALUE_FORMAT").or_insert_with(|| "JSON".into());
        let with: Vec<String> = PROPERTIES
            .iter()
            .filter_map(|k| {
                let v = props.get(k)?;
                Some(match *k {
                    "PARTITIONS" => format!("{k}={v}"),
                    "KEY_FORMAT" | "VALUE_FORMAT" => format!("{k}={}", lit(&v.to_ascii_uppercase())),
                    _ => format!("{k}={}", lit(v)),
                })
            })
            .collect();
        out.push(format!(
            "CREATE {object} {}{name} (\n{}\n) WITH ({});",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            cols.join(",\n"),
            with.join(", ")
        ));
    }
    // No indexes nor foreign keys in ksqlDB.
    Ok(out.join("\n"))
}

fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => lit(s),
        Value::Array(a) => format!("ARRAY[{}]", a.iter().map(literal).collect::<Vec<_>>().join(", ")),
        Value::Object(_) => lit(&v.to_string()),
    }
}

/// The browse query (`SELECT * FROM s EMIT CHANGES LIMIT n;`) restricted
/// by the grid's column filters: the WHERE goes before EMIT CHANGES. A
/// topic's `PRINT` can't filter.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{sql_condition, SqlFilterStyle};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    if browse.trim_start().to_ascii_uppercase().starts_with("PRINT") {
        return Err(Error::Unsupported("PRINT de un topic no filtra por columnas: se filtra un stream".into()));
    }
    let style = SqlFilterStyle { quote: Quote::Backtick, literal: &literal, like: "LIKE", true_literal: "TRUE", false_literal: "FALSE" };
    let cond = sql_condition(filters, &style)?;
    let at = browse
        .to_ascii_uppercase()
        .find(" EMIT CHANGES")
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))?;
    Ok(format!("{}\nWHERE {cond}\n{}", &browse[..at], &browse[at + 1..]))
}

/// One `INSERT … VALUES` per row (ksqlDB takes a single row per statement).
pub fn insert_script(name: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let head = format!("INSERT INTO {} ({}) VALUES", q(name), columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "));
    rows.iter()
        .map(|r| format!("{head} ({});", r.iter().map(literal).collect::<Vec<_>>().join(", ")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Edited rows of a table: ksqlDB has no UPDATE, but an INSERT with an
/// existing PRIMARY KEY replaces that key's value. The whole value is
/// replaced, so the script says that unlisted columns end up NULL.
/// Streams are append-only: an INSERT adds an event, it doesn't change one.
pub fn update_script(kind: &str, name: &str, changes: &[RowChange]) -> Result<String> {
    if kind != kinds::TABLE {
        return Err(Error::Unsupported(
            "los streams de ksqlDB solo admiten agregar eventos: no se puede modificar uno existente".into(),
        ));
    }
    let mut out = Vec::new();
    for c in changes.iter().filter(|c| !c.set.is_empty()) {
        if c.key.is_empty() {
            return Err(Error::Unsupported("ksqlDB reemplaza filas de una tabla por su PRIMARY KEY: falta la clave de la fila".into()));
        }
        // The whole row when the grid sent it (an INSERT replaces the key's
        // whole value), else just the key and the edited columns.
        let full = c.new_row();
        let partial = full.is_empty();
        let pairs: Vec<(String, Value)> = if partial {
            c.key.iter().chain(c.set.iter().filter(|(k, _)| !c.key.iter().any(|(kk, _)| kk == k))).cloned().collect()
        } else {
            let mut v: Vec<(String, Value)> = c.key.clone();
            v.extend(
                full.into_iter()
                    // Pseudo-columns ksqlDB doesn't take in an INSERT.
                    .filter(|(k, _)| !matches!(k.to_ascii_uppercase().as_str(), "ROWPARTITION" | "ROWOFFSET"))
                    .filter(|(k, _)| !c.key.iter().any(|(kk, _)| kk == k)),
            );
            v
        };
        if partial {
            out.push("-- ksqlDB reemplaza el valor completo de la clave: las columnas que no figuran quedan en NULL.".into());
        }
        out.push(format!(
            "INSERT INTO {} ({}) VALUES ({});",
            q(name),
            pairs.iter().map(|(k, _)| q(k)).collect::<Vec<_>>().join(", "),
            pairs.iter().map(|(_, v)| literal(v)).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(out.join("\n"))
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(v) => v.to_string(),
    }
}

/// A field schema as DDL: like [`crate::schema_type`], with struct field
/// names quoted (they keep their case).
fn ddl_type(s: &Value) -> String {
    match s.get("type").and_then(Value::as_str).unwrap_or_default() {
        "ARRAY" => format!("ARRAY<{}>", s.get("memberSchema").map(ddl_type).unwrap_or_default()),
        "MAP" => format!("MAP<STRING, {}>", s.get("memberSchema").map(ddl_type).unwrap_or_default()),
        "STRUCT" => {
            let fields = s.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
            let inner: Vec<String> =
                fields.iter().map(|f| format!("{} {}", q(&text(f.get("name"))), f.get("schema").map(ddl_type).unwrap_or_default())).collect();
            format!("STRUCT<{}>", inner.join(", "))
        }
        _ => crate::schema_type(s),
    }
}

/// Sources from the `sourceDescriptions` of `SHOW STREAMS EXTENDED` /
/// `SHOW TABLES EXTENDED`, sorted by name.
pub fn from_descriptions<'a>(descriptions: impl IntoIterator<Item = &'a Value>) -> Vec<TableSchema> {
    let mut out: Vec<TableSchema> = descriptions
        .into_iter()
        .map(|d| {
            let table = text(d.get("type")) == "TABLE";
            let fields = d.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
            let keys: Vec<String> = fields
                .iter()
                .filter(|f| matches!(f.get("type").and_then(Value::as_str), Some("KEY" | "PRIMARY_KEY")))
                .map(|f| text(f.get("name")))
                .collect();
            let mut options = BTreeMap::new();
            options.insert("object".to_string(), if table { "TABLE" } else { "STREAM" }.to_string());
            for (k, field) in [("KAFKA_TOPIC", "topic"), ("KEY_FORMAT", "keyFormat"), ("VALUE_FORMAT", "valueFormat"), ("PARTITIONS", "partitions"), ("TIMESTAMP", "timestamp")] {
                let v = text(d.get(field));
                if !v.is_empty() && v != "0" {
                    options.insert(k.to_string(), v);
                }
            }
            TableSchema {
                kind: if table { kinds::TABLE } else { kinds::STREAM }.into(),
                schema: None,
                name: text(d.get("name")),
                columns: fields
                    .iter()
                    .map(|f| ColumnDef {
                        name: text(f.get("name")),
                        data_type: f.get("schema").map(ddl_type).unwrap_or_default(),
                        nullable: true,
                        ..Default::default()
                    })
                    .collect(),
                primary_key: (!keys.is_empty()).then_some(KeyDef { name: None, columns: keys }),
                options,
                ..Default::default()
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_browse_before_emit_changes() {
        use dbine_driver::{ColumnFilter, FilterOp};
        use serde_json::json;
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT * FROM `pageviews` EMIT CHANGES LIMIT 200;",
                &[
                    f("USERID", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("PAGE", FilterOp::StartsWith, vec![json!("home")]),
                    f("N", FilterOp::Gt, vec![json!(3)]),
                    f("REGION", FilterOp::IsNull, vec![]),
                    f("ID", FilterOp::In, vec![json!(1), json!(2)]),
                ]
            )
            .unwrap(),
            "SELECT * FROM `pageviews`\nWHERE `USERID` = 'O''Brien'\n  AND `PAGE` LIKE 'home%'\n  AND `N` > 3\n  AND `REGION` IS NULL\n  AND `ID` IN (1, 2)\nEMIT CHANGES LIMIT 200;"
        );
        assert!(matches!(
            filtered_browse("PRINT 'orders' FROM BEGINNING LIMIT 200;", &[f("x", FilterOp::IsNull, vec![])]),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn update_rewrites_the_whole_row_when_known() {
        let c = RowChange {
            key: vec![("ID".into(), Value::from(1))],
            set: vec![("NOMBRE".into(), Value::from("O'Brien"))],
            row: vec![
                ("ID".into(), Value::from(1)),
                ("NOMBRE".into(), Value::from("a")),
                ("CIUDAD".into(), Value::from("Rosario")),
                ("ROWOFFSET".into(), Value::from(7)),
            ],
        };
        let s = update_script(kinds::TABLE, "clientes", &[c]).unwrap();
        assert!(!s.contains("quedan en NULL"), "{s}");
        assert!(s.contains("'O''Brien'") && s.contains("'Rosario'") && !s.contains("ROWOFFSET"), "{s}");
    }
    use serde_json::json;

    fn stream() -> TableSchema {
        TableSchema {
            kind: kinds::STREAM.into(),
            name: "PEDIDOS".into(),
            columns: vec![
                ColumnDef { name: "ID".into(), data_type: "INT".into(), ..Default::default() },
                ColumnDef { name: "ITEMS".into(), data_type: "ARRAY<VARCHAR>".into(), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["ID".into()] }),
            options: [("KAFKA_TOPIC".to_string(), "pedidos".to_string()), ("PARTITIONS".to_string(), "3".to_string())].into(),
            ..Default::default()
        }
    }

    #[test]
    fn update_script_replaces_table_rows() {
        let c = RowChange {
            key: vec![("ID".into(), json!(7))],
            set: vec![("NOMBRE".into(), json!("O'Brien")), ("BAJA".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(kinds::TABLE, "CLIENTES", &[c.clone(), RowChange::default()]).unwrap(),
            "-- ksqlDB reemplaza el valor completo de la clave: las columnas que no figuran quedan en NULL.\n\
             INSERT INTO `CLIENTES` (`ID`, `NOMBRE`, `BAJA`) VALUES (7, 'O''Brien', NULL);"
        );
        assert!(matches!(update_script(kinds::STREAM, "EVENTOS", &[c]), Err(Error::Unsupported(_))));
        let no_key = RowChange { key: vec![], set: vec![("A".into(), json!(1))], ..Default::default() };
        assert!(matches!(update_script(kinds::TABLE, "T", &[no_key]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn create_stream_and_table() {
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        assert_eq!(
            table_ddl(&stream(), all).unwrap(),
            "DROP STREAM IF EXISTS `PEDIDOS`;\nCREATE STREAM `PEDIDOS` (\n    `ID` INT KEY,\n    `ITEMS` ARRAY<VARCHAR>\n) \
             WITH (KAFKA_TOPIC='pedidos', VALUE_FORMAT='JSON', PARTITIONS=3);"
        );
        let mut t = stream();
        t.options.insert("object".into(), "TABLE".into());
        t.options.insert("KEY_FORMAT".into(), "json".into());
        let s = table_ddl(&t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap();
        assert!(s.starts_with("CREATE TABLE IF NOT EXISTS `PEDIDOS` (\n    `ID` INT PRIMARY KEY,"), "{s}");
        assert!(s.ends_with("WITH (KAFKA_TOPIC='pedidos', KEY_FORMAT='JSON', VALUE_FORMAT='JSON', PARTITIONS=3);"), "{s}");
        // A table needs a key; the stream designer's defaults fill topic and format.
        t.primary_key = None;
        assert!(table_ddl(&t, all).is_err());
        let bare = TableSchema { kind: kinds::STREAM.into(), name: "S".into(), ..stream() };
        let s = table_ddl(&TableSchema { options: BTreeMap::new(), ..bare }, DdlParts { create: true, ..Default::default() }).unwrap();
        assert!(s.ends_with("WITH (KAFKA_TOPIC='S', VALUE_FORMAT='JSON');"), "{s}");
        assert_eq!(table_ddl(&stream(), DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap(), "");
    }

    #[test]
    fn inserts_one_row_each() {
        let s = insert_script("S", &["ID".into(), "N".into(), "T".into()], &[vec![json!(1), json!("O'k"), json!(["a"])], vec![json!(2), Value::Null, json!(true)]]);
        assert_eq!(s, "INSERT INTO `S` (`ID`, `N`, `T`) VALUES (1, 'O''k', ARRAY['a']);\nINSERT INTO `S` (`ID`, `N`, `T`) VALUES (2, NULL, TRUE);");
    }

    #[test]
    fn descriptions_become_sources() {
        let d = json!([
            {"name": "T1", "type": "TABLE", "topic": "t1", "keyFormat": "JSON", "valueFormat": "JSON", "partitions": 1, "timestamp": "",
             "fields": [{"name": "ID", "schema": {"type": "STRING"}, "type": "KEY"}, {"name": "N", "schema": {"type": "BIGINT"}}]},
            {"name": "S1", "type": "STREAM", "topic": "s1", "keyFormat": "KAFKA", "valueFormat": "JSON", "partitions": 2,
             "fields": [{"name": "ID", "schema": {"type": "INTEGER"}, "type": "KEY"},
                        {"name": "ST", "schema": {"type": "STRUCT", "fields": [{"name": "b", "schema": {"type": "ARRAY", "memberSchema": {"type": "STRING"}}}]}},
                        {"name": "AMT", "schema": {"type": "DECIMAL", "parameters": {"precision": 10, "scale": 2}}}]}
        ]);
        let ts = from_descriptions(d.as_array().unwrap());
        assert_eq!(ts.iter().map(|t| (t.name.as_str(), t.kind.as_str())).collect::<Vec<_>>(), [("S1", "stream"), ("T1", "table")]);
        let s1 = &ts[0];
        assert_eq!(s1.columns[1].data_type, "STRUCT<`b` ARRAY<STRING>>");
        assert_eq!(s1.columns[2].data_type, "DECIMAL(10, 2)");
        assert_eq!(s1.primary_key.as_ref().unwrap().columns, ["ID"]);
        let o = |k: &str| s1.options.get(k).map(String::as_str);
        assert_eq!((o("object"), o("KAFKA_TOPIC"), o("KEY_FORMAT"), o("PARTITIONS"), o("TIMESTAMP")), (Some("STREAM"), Some("s1"), Some("KAFKA"), Some("2"), None));
        let s = table_ddl(&ts[1], DdlParts { create: true, ..Default::default() }).unwrap();
        assert_eq!(s, "CREATE TABLE `T1` (\n    `ID` STRING PRIMARY KEY,\n    `N` BIGINT\n) WITH (KAFKA_TOPIC='t1', KEY_FORMAT='JSON', VALUE_FORMAT='JSON', PARTITIONS=1);");
    }

    #[test]
    fn designer_and_templates() {
        let d = designer();
        assert_eq!((d.kind, d.label), (kinds::STREAM, "Nuevo stream"));
        assert!(d.primary_key && !d.schemas && !d.indexes && !d.foreign_keys && !d.auto_increment);
        assert!(templates().iter().all(|t| !t.template.contains("{schema}")));
    }
}

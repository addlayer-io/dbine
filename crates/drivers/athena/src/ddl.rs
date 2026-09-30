//! Table designer and CREATE TABLE in Athena's DDL, which is Hive's
//! (backticks, `\'` escapes), for Iceberg tables or external tables over
//! S3 files. No keys, indexes, NOT NULL or defaults in Athena.

use aws_sdk_athena::types::TableMetadata;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, Result, TableSchema};

/// `TableSchema::options` keys.
pub const TABLE_TYPE: &str = "table_type";
pub const LOCATION: &str = "location";
pub const PARTITIONED_BY: &str = "partitioned_by";
pub const TBLPROPERTIES: &str = "tblproperties";

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: true,
        indexes: false,
        foreign_keys: false,
        table_options: vec![
            Field::new(
                TABLE_TYPE,
                "Tipo de tabla",
                FieldKind::Select(vec![
                    ("iceberg", "Iceberg"),
                    ("parquet", "Externa: Parquet"),
                    ("orc", "Externa: ORC"),
                    ("csv", "Externa: CSV"),
                    ("json", "Externa: JSON"),
                ]),
            )
            .default_value("iceberg"),
            Field::new(LOCATION, "Ubicación (S3)", FieldKind::Text).required().placeholder("s3://mi-bucket/ruta/tabla/"),
            Field::new(PARTITIONED_BY, "Particionada por", FieldKind::Text)
                .placeholder("anio, mes")
                .help("Columnas de la tabla. En Iceberg también transformaciones: day(ts), bucket(16, id)."),
            Field::new(TBLPROPERTIES, "TBLPROPERTIES", FieldKind::Text)
                .placeholder("'write_compression' = 'snappy'")
                .help("Propiedades adicionales, separadas por coma."),
        ],
        ..DesignerSpec::sql_table(vec![
            "string",
            "varchar(255)",
            "char(10)",
            "boolean",
            "tinyint",
            "smallint",
            "int",
            "bigint",
            "float",
            "double",
            "decimal(18,2)",
            "date",
            "timestamp",
            "binary",
            "array<string>",
            "map<string,string>",
            "struct<a:int,b:string>",
        ])
    }
}

/// A Hive string literal.
pub(crate) fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub(crate) fn q(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

/// Items of a comma list, ignoring commas inside parentheses.
pub(crate) fn split_list(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

pub(crate) fn column(c: &ColumnDef) -> String {
    let mut l = format!("{} {}", q(&c.name), c.data_type);
    if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
        l.push_str(&format!(" COMMENT {}", lit(cm)));
    }
    l
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    let name = qualified_name(Quote::Backtick, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
    let opt = |k: &str| t.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let kind = opt(TABLE_TYPE).unwrap_or("iceberg").to_ascii_lowercase();
        let iceberg = kind == "iceberg";
        let partitions: Vec<String> =
            opt(PARTITIONED_BY).map(split_list).unwrap_or_default().into_iter().map(|p| p.trim_matches('`').to_string()).collect();
        // Hive partition columns are declared apart from the others, with their type.
        let (cols, part_cols): (Vec<&ColumnDef>, Vec<&ColumnDef>) =
            t.columns.iter().partition(|c| iceberg || !partitions.contains(&c.name));
        if !iceberg {
            if let Some(p) = partitions.iter().find(|p| !part_cols.iter().any(|c| &&c.name == p)) {
                return Err(Error::Query(format!("La columna de partición «{p}» no está entre las columnas de la tabla.")));
            }
        }
        let mut s = format!(
            "CREATE {}TABLE {}{name} (\n{}\n)",
            if iceberg { "" } else { "EXTERNAL " },
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            cols.iter().map(|c| format!("    {}", column(c))).collect::<Vec<_>>().join(",\n")
        );
        // Athena's Iceberg DDL has no table comment.
        if let Some(cm) = t.comment.as_deref().filter(|s| !s.is_empty() && !iceberg) {
            s.push_str(&format!("\nCOMMENT {}", lit(cm)));
        }
        if iceberg && !partitions.is_empty() {
            let items: Vec<String> =
                partitions.iter().map(|p| if t.columns.iter().any(|c| &c.name == p) { q(p) } else { p.clone() }).collect();
            s.push_str(&format!("\nPARTITIONED BY ({})", items.join(", ")));
        } else if !part_cols.is_empty() {
            let items: Vec<String> = partitions
                .iter()
                .filter_map(|p| part_cols.iter().find(|c| &c.name == p))
                .map(|c| column(c))
                .collect();
            s.push_str(&format!("\nPARTITIONED BY ({})", items.join(", ")));
        }
        match kind.as_str() {
            "iceberg" => {}
            "parquet" => s.push_str("\nSTORED AS PARQUET"),
            "orc" => s.push_str("\nSTORED AS ORC"),
            "json" => s.push_str("\nROW FORMAT SERDE 'org.openx.data.jsonserde.JsonSerDe'\nSTORED AS TEXTFILE"),
            "csv" => s.push_str("\nROW FORMAT DELIMITED\nFIELDS TERMINATED BY ','\nSTORED AS TEXTFILE"),
            other => return Err(Error::Query(format!("Tipo de tabla desconocido: {other}"))),
        }
        if let Some(l) = opt(LOCATION) {
            s.push_str(&format!("\nLOCATION {}", lit(l)));
        }
        let mut props: Vec<String> = Vec::new();
        if iceberg {
            props.push("'table_type' = 'ICEBERG'".into());
        }
        if let Some(p) = opt(TBLPROPERTIES) {
            props.push(p.strip_prefix('(').and_then(|p| p.strip_suffix(')')).unwrap_or(p).trim().to_string());
        }
        if !props.is_empty() {
            s.push_str(&format!("\nTBLPROPERTIES (\n    {}\n)", props.join(",\n    ")));
        }
        s.push(';');
        out.push(s);
    }
    // No indexes or foreign keys in Athena: those parts add nothing.
    Ok(out.join("\n"))
}

/// A Glue table as the designer's model (`None` for views).
pub fn table_schema(t: &TableMetadata) -> Option<TableSchema> {
    if t.table_type() == Some("VIRTUAL_VIEW") {
        return None;
    }
    let param = |k: &str| t.parameters().and_then(|p| p.get(k)).map(String::as_str).filter(|v| !v.is_empty());
    let formats = format!("{} {}", param("inputformat").unwrap_or(""), param("serde.serialization.lib").unwrap_or("")).to_ascii_lowercase();
    let kind = if param("table_type").is_some_and(|v| v.eq_ignore_ascii_case("iceberg")) {
        "iceberg"
    } else if formats.contains("parquet") {
        "parquet"
    } else if formats.contains("orc") {
        "orc"
    } else if formats.contains("json") {
        "json"
    } else {
        "csv"
    };
    let mut options = std::collections::BTreeMap::new();
    options.insert(TABLE_TYPE.to_string(), kind.to_string());
    if let Some(l) = param("location") {
        options.insert(LOCATION.to_string(), l.to_string());
    }
    if !t.partition_keys().is_empty() {
        options.insert(PARTITIONED_BY.to_string(), t.partition_keys().iter().map(|c| c.name()).collect::<Vec<_>>().join(", "));
    }
    Some(TableSchema {
        kind: kinds::TABLE.into(),
        schema: None,
        name: t.name().to_string(),
        columns: t
            .columns()
            .iter()
            .chain(t.partition_keys())
            .map(|c| ColumnDef {
                name: c.name().to_string(),
                data_type: c.r#type().unwrap_or("string").to_string(),
                nullable: true,
                comment: c.comment().filter(|s| !s.is_empty()).map(str::to_string),
                ..Default::default()
            })
            .collect(),
        comment: param("comment").map(str::to_string),
        options,
        ..Default::default()
    })
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![CreateTemplate {
        kind: kinds::VIEW,
        label: "Nueva vista",
        template: "-- Las vistas usan la sintaxis de consulta (comillas dobles), no la de DDL.\nCREATE OR REPLACE VIEW \"{name}\" AS\nSELECT\n    t.id,\n    t.nombre\nFROM \"tabla\" t\nWHERE t.activo = true;\n".into(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_athena::types::Column;

    fn t(kind: &str, partitioned_by: &str) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: "ventas".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "bigint".into(), nullable: true, comment: Some("clave".into()), ..Default::default() },
                ColumnDef { name: "ts".into(), data_type: "timestamp".into(), nullable: true, ..Default::default() },
                ColumnDef { name: "pais".into(), data_type: "string".into(), nullable: true, comment: Some("o'k".into()), ..Default::default() },
            ],
            comment: Some("Ventas".into()),
            options: [
                (TABLE_TYPE.to_string(), kind.to_string()),
                (LOCATION.to_string(), "s3://b/ventas/".to_string()),
                (PARTITIONED_BY.to_string(), partitioned_by.to_string()),
            ]
            .into(),
            ..Default::default()
        }
    }
    const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: true };

    #[test]
    fn iceberg_with_transforms() {
        let mut tt = t("iceberg", "day(ts), `pais`, bucket(16, id)");
        tt.options.insert(TBLPROPERTIES.into(), "'format' = 'parquet'".into());
        let s = table_ddl(&tt, CREATE).unwrap();
        assert_eq!(
            s,
            "CREATE TABLE `ventas` (\n    `id` bigint COMMENT 'clave',\n    `ts` timestamp,\n    `pais` string COMMENT 'o\\'k'\n)\nPARTITIONED BY (day(ts), `pais`, bucket(16, id))\nLOCATION 's3://b/ventas/'\nTBLPROPERTIES (\n    'table_type' = 'ICEBERG',\n    'format' = 'parquet'\n);"
        );
    }

    #[test]
    fn external_parquet_moves_partition_columns() {
        let s = table_ddl(&t("parquet", "pais"), DdlParts { drop: true, if_exists: true, ..CREATE }).unwrap();
        assert_eq!(
            s,
            "DROP TABLE IF EXISTS `ventas`;\nCREATE EXTERNAL TABLE `ventas` (\n    `id` bigint COMMENT 'clave',\n    `ts` timestamp\n)\nCOMMENT 'Ventas'\nPARTITIONED BY (`pais` string COMMENT 'o\\'k')\nSTORED AS PARQUET\nLOCATION 's3://b/ventas/';"
        );
        let s = table_ddl(&t("csv", ""), DdlParts { if_exists: true, ..CREATE }).unwrap();
        assert!(s.starts_with("CREATE EXTERNAL TABLE IF NOT EXISTS `ventas` ("), "{s}");
        assert!(s.contains("ROW FORMAT DELIMITED\nFIELDS TERMINATED BY ','\nSTORED AS TEXTFILE\nLOCATION"), "{s}");
        assert!(table_ddl(&t("json", ""), CREATE).unwrap().contains("ROW FORMAT SERDE 'org.openx.data.jsonserde.JsonSerDe'"));
        assert!(matches!(table_ddl(&t("orc", "nope"), CREATE), Err(Error::Query(_))));
        assert_eq!(table_ddl(&t("orc", ""), DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap(), "");
    }

    #[test]
    fn glue_metadata_round_trips() {
        let col = |n: &str, ty: &str, c: Option<&str>| Column::builder().name(n).r#type(ty).set_comment(c.map(Into::into)).build().unwrap();
        let meta = TableMetadata::builder()
            .name("ventas")
            .table_type("EXTERNAL_TABLE")
            .columns(col("id", "bigint", Some("clave")))
            .columns(col("ts", "timestamp", None))
            .partition_keys(col("pais", "string", Some("o'k")))
            .parameters("inputformat", "org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat")
            .parameters("location", "s3://b/ventas/")
            .parameters("comment", "Ventas")
            .build()
            .unwrap();
        let ts = table_schema(&meta).unwrap();
        assert_eq!(ts, t("parquet", "pais"));
        let view = TableMetadata::builder().name("v").table_type("VIRTUAL_VIEW").build().unwrap();
        assert!(table_schema(&view).is_none());
        let ice = TableMetadata::builder().name("i").table_type("EXTERNAL_TABLE").parameters("table_type", "ICEBERG").build().unwrap();
        assert_eq!(table_schema(&ice).unwrap().options.get(TABLE_TYPE).map(String::as_str), Some("iceberg"));
    }

    #[test]
    fn designer_and_templates() {
        let d = designer();
        assert!(!d.schemas && d.comments && !d.nullability && !d.primary_key && !d.foreign_keys && !d.indexes && !d.defaults);
        let ts = templates();
        assert_eq!(ts.len(), 1);
        assert!(ts[0].template.contains("\"{name}\"") && !ts[0].template.contains("{schema}"));
    }
}

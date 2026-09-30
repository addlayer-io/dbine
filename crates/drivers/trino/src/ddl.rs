//! Table designer, CREATE TABLE and creation templates in Trino's dialect.
//! Trino has no primary keys, foreign keys or indexes: comments go inline
//! (`col type COMMENT '…'`, `COMMENT '…'` after the columns) and the
//! connector's table properties in `WITH (…)`.

use crate::Flavor;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{kinds, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, TableSchema};

/// `TableSchema::options` key of the `WITH (…)` properties.
pub const WITH: &str = "with";

pub fn designer(flavor: Flavor) -> DesignerSpec {
    DesignerSpec {
        schemas: true,
        primary_key: false,
        auto_increment: false,
        // Column defaults arrived in Trino 4xx; Presto has none.
        defaults: flavor != Flavor::Presto,
        comments: true,
        indexes: false,
        foreign_keys: false,
        table_options: vec![Field::new(WITH, "Propiedades (WITH)", FieldKind::Textarea)
            .placeholder("format = 'ORC', partitioned_by = ARRAY['anio']")
            .help("Propiedades de tabla del conector, separadas por coma.")],
        ..DesignerSpec::sql_table(vec![
            "boolean",
            "tinyint",
            "smallint",
            "integer",
            "bigint",
            "real",
            "double",
            "decimal(18,2)",
            "varchar",
            "varchar(255)",
            "char(10)",
            "varbinary",
            "json",
            "uuid",
            "date",
            "time(3)",
            "timestamp(3)",
            "timestamp(3) with time zone",
            "array(varchar)",
            "map(varchar, varchar)",
            "row(a integer, b varchar)",
        ])
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let name = qualified_name(Quote::Double, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let cols: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                let mut l = format!("    {} {}", quote_ident(Quote::Double, &c.name), c.data_type);
                if let Some(d) = c.default_value.as_deref().filter(|d| !d.is_empty()) {
                    l.push_str(&format!(" DEFAULT {d}"));
                }
                if !c.nullable {
                    l.push_str(" NOT NULL");
                }
                if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
                    l.push_str(&format!(" COMMENT {}", lit(cm)));
                }
                l
            })
            .collect();
        let mut s = format!(
            "CREATE TABLE {}{name} (\n{}\n)",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            cols.join(",\n")
        );
        if let Some(cm) = t.comment.as_deref().filter(|s| !s.is_empty()) {
            s.push_str(&format!("\nCOMMENT {}", lit(cm)));
        }
        if let Some(w) = t.options.get(WITH).map(|w| w.trim()).filter(|w| !w.is_empty()) {
            let w = w.strip_prefix('(').and_then(|w| w.strip_suffix(')')).unwrap_or(w).trim();
            s.push_str(&format!("\nWITH (\n    {w}\n)"));
        }
        s.push(';');
        out.push(s);
    }
    // No indexes or foreign keys in Trino: those parts add nothing.
    out.join("\n")
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::VIEW,
            label: "Nueva vista",
            template: "CREATE OR REPLACE VIEW \"{schema}\".\"{name}\"\nCOMMENT 'Descripción'\nSECURITY INVOKER AS\nSELECT\n    t.id,\n    t.nombre\nFROM \"{schema}\".\"tabla\" t\nWHERE t.activo = true;\n".into(),
        },
        CreateTemplate {
            kind: kinds::MATERIALIZED_VIEW,
            label: "Nueva vista materializada",
            template: "-- Solo en conectores que las soportan (Iceberg, Hive…).\nCREATE OR REPLACE MATERIALIZED VIEW \"{schema}\".\"{name}\"\nAS\nSELECT\n    t.categoria,\n    count(*) AS cantidad\nFROM \"{schema}\".\"tabla\" t\nGROUP BY t.categoria;\n\n-- Para actualizarla:\n-- REFRESH MATERIALIZED VIEW \"{schema}\".\"{name}\";\n".into(),
        },
        CreateTemplate {
            kind: kinds::FUNCTION,
            label: "Nueva función",
            template: "-- Función SQL guardada (Trino 431+, en un catálogo que las soporte).\nCREATE OR REPLACE FUNCTION \"{schema}\".\"{name}\"(x bigint)\nRETURNS bigint\nRETURNS NULL ON NULL INPUT\nRETURN x * 2;\n".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;

    fn t() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("ventas".into()),
            name: "pedidos".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "bigint".into(), nullable: false, comment: Some("clave".into()), ..Default::default() },
                ColumnDef { name: "estado".into(), data_type: "varchar(20)".into(), nullable: true, default_value: Some("'nuevo'".into()), ..Default::default() },
                ColumnDef { name: "nota".into(), data_type: "varchar".into(), nullable: true, comment: Some("o'k".into()), ..Default::default() },
            ],
            comment: Some("Pedidos".into()),
            options: [(WITH.to_string(), "format = 'ORC', partitioned_by = ARRAY['estado']".to_string())].into(),
            ..Default::default()
        }
    }

    #[test]
    fn create_with_comments_and_properties() {
        let s = table_ddl(&t(), DdlParts { create: true, indexes: true, foreign_keys: true, ..Default::default() });
        assert_eq!(
            s,
            "CREATE TABLE \"ventas\".\"pedidos\" (\n    \"id\" bigint NOT NULL COMMENT 'clave',\n    \"estado\" varchar(20) DEFAULT 'nuevo',\n    \"nota\" varchar COMMENT 'o''k'\n)\nCOMMENT 'Pedidos'\nWITH (\n    format = 'ORC', partitioned_by = ARRAY['estado']\n);"
        );
    }

    #[test]
    fn drop_and_guards() {
        let mut tt = t();
        tt.options.insert(WITH.into(), "(format = 'PARQUET')".into());
        let s = table_ddl(&tt, DdlParts { drop: true, if_exists: true, create: true, ..Default::default() });
        assert!(s.starts_with("DROP TABLE IF EXISTS \"ventas\".\"pedidos\";\nCREATE TABLE \"ventas\".\"pedidos\" ("), "{s}");
        assert!(s.ends_with("WITH (\n    format = 'PARQUET'\n);"), "{s}");
        let s = table_ddl(&tt, DdlParts { if_exists: true, create: true, ..Default::default() });
        assert!(s.starts_with("CREATE TABLE IF NOT EXISTS "));
        assert_eq!(table_ddl(&tt, DdlParts { indexes: true, foreign_keys: true, ..Default::default() }), "");
    }

    #[test]
    fn designer_and_templates() {
        let d = designer(Flavor::Presto);
        assert!(d.schemas && d.comments && d.nullability && !d.defaults && !d.primary_key && !d.foreign_keys && !d.indexes);
        assert!(designer(Flavor::Trino).defaults);
        let ts = templates();
        assert!(ts.iter().all(|t| t.template.contains("{schema}") && t.template.contains("{name}")));
        assert_eq!(ts.iter().map(|t| t.kind).collect::<Vec<_>>(), [kinds::VIEW, kinds::MATERIALIZED_VIEW, kinds::FUNCTION]);
    }
}

//! Schema sync for Athena, in its Hive DDL. Only columns change: there are
//! no keys, indexes, NOT NULL or defaults.
//!
//! - Iceberg: `ADD COLUMNS (…)`, `DROP COLUMN`, `CHANGE COLUMN c c type
//!   [COMMENT …]` (type promotion only: int → bigint, float → double, more
//!   decimal precision).
//! - External tables: `ADD COLUMNS`, `CHANGE COLUMN` (metadata only), and
//!   `REPLACE COLUMNS` with the columns that stay to drop some (only for
//!   LazySimpleSerDe, the CSV tables). Partition columns can't change.

use crate::ddl::{self, column, lit, q, split_list, PARTITIONED_BY, TABLE_TYPE};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{DdlParts, Result, SyncScript, TableChange, TableSchema};

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: false, foreign_keys: false };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn text(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

fn kind(t: &TableSchema) -> String {
    t.options.get(TABLE_TYPE).map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty()).unwrap_or_else(|| "iceberg".into())
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut columns, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(ddl::table_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                warnings.push(if kind(table) == "iceberg" {
                    format!("Se borra la tabla {} con todos sus datos.", display(table))
                } else {
                    format!("Se borra la tabla externa {}; los archivos en S3 quedan.", display(table))
                });
                drops.push(ddl::table_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => alter(old, new, &mut columns, &mut warnings),
        }
    }
    let statements = [drops, columns, creates].into_iter().flatten().filter(|s| !s.trim().is_empty()).collect();
    Ok(SyncScript { statements, warnings })
}

fn alter(old: &TableSchema, new: &TableSchema, out: &mut Vec<String>, warnings: &mut Vec<String>) {
    let name = qualified_name(Quote::Backtick, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
    let tname = display(new);
    let kind = kind(old);
    let iceberg = kind == "iceberg";
    if kind != self::kind(new) {
        warnings.push(format!("{tname}: Athena no cambia el tipo de tabla ({kind} → {}); se deja como está.", self::kind(new)));
    }
    // Hive partition columns are apart; in Iceberg partitioning is hidden.
    let parts: Vec<String> = if iceberg {
        Vec::new()
    } else {
        old.options.get(PARTITIONED_BY).map(|p| split_list(p)).unwrap_or_default().into_iter().map(|p| p.trim_matches('`').to_lowercase()).collect()
    };
    let is_part = |c: &str| parts.contains(&c.to_lowercase());
    let find = |t: &TableSchema, c: &str| t.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c)).cloned();

    let mut dropped = Vec::new();
    for o in &old.columns {
        if find(new, &o.name).is_none() {
            if is_part(&o.name) {
                warnings.push(format!("{tname}.{}: Athena no borra columnas de partición; se deja como está.", o.name));
            } else {
                dropped.push(o.clone());
            }
        }
    }
    let mut added = Vec::new();
    for n in &new.columns {
        if find(old, &n.name).is_none() {
            added.push(n.clone());
        }
    }
    let mut changed = Vec::new();
    for n in &new.columns {
        let Some(o) = find(old, &n.name) else { continue };
        let ty = squash(&o.data_type) != squash(&n.data_type);
        let cm = text(&o.comment) != text(&n.comment);
        if !(ty || cm) {
            continue;
        }
        if is_part(&n.name) {
            warnings.push(format!("{tname}.{}: Athena no modifica columnas de partición; se deja como está.", n.name));
            continue;
        }
        if ty {
            warnings.push(if iceberg {
                format!(
                    "{tname}.{}: {} → {}. Iceberg solo amplía tipos (int → bigint, float → double, más precisión en decimal); otro cambio falla.",
                    n.name, o.data_type, n.data_type
                )
            } else {
                format!(
                    "{tname}.{}: {} → {}. En una tabla externa solo cambia la definición: si los archivos no se leen con el tipo nuevo, las consultas fallan.",
                    n.name, o.data_type, n.data_type
                )
            });
        }
        changed.push(n.clone());
    }
    let skipped = new.columns.iter().any(|n| {
        find(old, &n.name).is_some_and(|o| o.nullable != n.nullable || text(&o.default_value) != text(&n.default_value))
            || !n.nullable
            || text(&n.default_value).is_some()
    }) || new.primary_key.as_ref().is_some_and(|k| !k.columns.is_empty())
        || !new.foreign_keys.is_empty()
        || !new.indexes.is_empty();
    if skipped {
        warnings.push(format!("{tname}: Athena no tiene NOT NULL, valores por defecto, claves ni índices; esos cambios se omiten."));
    }
    for c in &dropped {
        warnings.push(format!("Se borra la columna {tname}.{} con sus datos.", c.name));
    }

    if iceberg {
        for c in &dropped {
            out.push(format!("ALTER TABLE {name} DROP COLUMN {};", q(&c.name)));
        }
    } else if !dropped.is_empty() {
        if kind != "csv" {
            warnings.push(format!(
                "{tname}: Athena borra columnas de tablas externas solo en CSV (REPLACE COLUMNS); en {kind} hay que recrear la tabla. Las columnas quedan."
            ));
        } else {
            // REPLACE COLUMNS: the columns that stay, in the new order, with the new types.
            let keep: Vec<String> = new.columns.iter().filter(|c| !is_part(&c.name) && find(old, &c.name).is_some()).map(column).chain(added.iter().map(column)).collect();
            warnings.push(format!(
                "{tname}: REPLACE COLUMNS no cambia los archivos: CSV se lee por posición, así que las columnas que quedan tienen que coincidir con las de los archivos."
            ));
            if new.columns.iter().any(|c| c.data_type.trim().eq_ignore_ascii_case("date")) {
                warnings.push(format!("{tname}: REPLACE COLUMNS no funciona con columnas date en Athena; usá timestamp."));
            }
            out.push(format!("ALTER TABLE {name} REPLACE COLUMNS ({});", keep.join(", ")));
            return comment(old, new, iceberg, &name, &tname, out, warnings);
        }
    }
    if !added.is_empty() {
        out.push(format!("ALTER TABLE {name} ADD COLUMNS ({});", added.iter().map(column).collect::<Vec<_>>().join(", ")));
    }
    for c in &changed {
        let mut s = format!("ALTER TABLE {name} CHANGE COLUMN {} {}", q(&c.name), column(c));
        if text(&c.comment).is_none() && find(old, &c.name).is_some_and(|o| text(&o.comment).is_some()) {
            s.push_str(" COMMENT ''");
        }
        s.push(';');
        out.push(s);
    }
    comment(old, new, iceberg, &name, &tname, out, warnings)
}

fn comment(old: &TableSchema, new: &TableSchema, iceberg: bool, name: &str, tname: &str, out: &mut Vec<String>, warnings: &mut Vec<String>) {
    if text(&old.comment) == text(&new.comment) {
        return;
    }
    if iceberg {
        warnings.push(format!("{tname}: las tablas Iceberg de Athena no tienen comentario; se deja como está."));
    } else {
        out.push(format!("ALTER TABLE {name} SET TBLPROPERTIES ('comment' = {});", lit(&text(&new.comment).unwrap_or_default())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn table(kind: &str, cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: "ventas".into(),
            columns: cols,
            options: [(TABLE_TYPE.to_string(), kind.to_string()), (ddl::LOCATION.to_string(), "s3://b/v/".to_string())].into(),
            ..Default::default()
        }
    }

    #[test]
    fn iceberg_add_drop_change() {
        let old = table("iceberg", vec![col("id", "int"), col("nombre", "string"), col("baja", "date")]);
        let mut new = table("iceberg", vec![col("id", "bigint"), col("nombre", "string"), col("email", "string"), col("pais", "string")]);
        new.columns[1].comment = Some("El nombre".into());
        new.columns[0].nullable = false;
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `ventas` DROP COLUMN `baja`;",
                "ALTER TABLE `ventas` ADD COLUMNS (`email` string, `pais` string);",
                "ALTER TABLE `ventas` CHANGE COLUMN `id` `id` bigint;",
                "ALTER TABLE `ventas` CHANGE COLUMN `nombre` `nombre` string COMMENT 'El nombre';",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("Iceberg solo amplía tipos"), "{w}");
        assert!(w.contains("no tiene NOT NULL"), "{w}");
        assert!(w.contains("Se borra la columna ventas.baja"), "{w}");
    }

    #[test]
    fn external_csv_replaces_columns() {
        let mut old = table("csv", vec![col("id", "int"), col("nombre", "string"), col("baja", "string"), col("anio", "int")]);
        old.options.insert(PARTITIONED_BY.into(), "anio".into());
        let mut new = old.clone();
        new.columns.remove(2);
        new.columns.push(col("email", "string"));
        new.comment = Some("Ventas".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `ventas` REPLACE COLUMNS (`id` int, `nombre` string, `email` string);",
                "ALTER TABLE `ventas` SET TBLPROPERTIES ('comment' = 'Ventas');",
            ]
        );
    }

    #[test]
    fn external_parquet_keeps_dropped_and_partitions() {
        let mut old = table("parquet", vec![col("id", "int"), col("baja", "string"), col("anio", "int")]);
        old.options.insert(PARTITIONED_BY.into(), "anio".into());
        let mut new = table("parquet", vec![col("id", "bigint"), col("anio", "string")]);
        new.options.insert(PARTITIONED_BY.into(), "anio".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements, vec!["ALTER TABLE `ventas` CHANGE COLUMN `id` `id` bigint;"]);
        let w = s.warnings.join("\n");
        assert!(w.contains("solo en CSV"), "{w}");
        assert!(w.contains("ventas.anio: Athena no modifica columnas de partición"), "{w}");
    }

    #[test]
    fn create_and_drop() {
        let a = table("iceberg", vec![col("id", "int")]);
        let mut b = table("parquet", vec![col("id", "int")]);
        b.name = "vieja".into();
        let s = sync_script(&[TableChange::Create { table: a }, TableChange::Drop { table: b }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE `vieja`;");
        assert!(s.statements[1].starts_with("CREATE TABLE `ventas` (\n    `id` int\n)"), "{}", s.statements[1]);
        assert_eq!(s.warnings, vec!["Se borra la tabla externa vieja; los archivos en S3 quedan."]);
    }
}

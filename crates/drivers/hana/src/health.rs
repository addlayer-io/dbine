//! "Chequeo de salud" findings of SAP HANA ([`dbine_driver::Session::health_checks`]),
//! for the schema that is the "database" here: invalid objects, column
//! tables with a delta store that needs a merge (or automatic merge turned
//! off), tables without a primary key and virtual tables without data
//! statistics. Each check is its own catalog query: one that fails (no
//! access to a monitoring view, an older release) is skipped.

use crate::{num, quote, text, HanaSession};
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::Result;
use hdbconnect_async::HdbValue;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;

const MIB: f64 = 1024.0 * 1024.0;

fn s(r: &[HdbValue<'static>], i: usize) -> String {
    r.get(i).and_then(text).unwrap_or_default()
}

fn n(r: &[HdbValue<'static>], i: usize) -> f64 {
    r.get(i).and_then(num).unwrap_or(0.0)
}

fn qn(schema: &str, name: &str) -> String {
    format!("{}.{}", quote(schema), quote(name))
}

/// The statement that recompiles an invalid object, for the kinds that
/// have one (views and the rest are valid again when what they use is back,
/// or must be created again).
pub(crate) fn recompile_sql(kind: &str, schema: &str, name: &str) -> Option<String> {
    matches!(kind, "PROCEDURE" | "FUNCTION").then(|| format!("ALTER {kind} {} RECOMPILE;", qn(schema, name)))
}

/// Whether a column table's delta store calls for a merge: over 1 GB, or
/// over 100 MB and a fifth of the main store.
pub(crate) fn needs_merge(delta: f64, main: f64) -> bool {
    delta >= 1024.0 * MIB || (delta >= 100.0 * MIB && delta >= main * 0.2)
}

const INVALID: &str = "SELECT TOP 200 OBJECT_TYPE, OBJECT_NAME FROM SYS.OBJECTS
  WHERE SCHEMA_NAME = ? AND IS_VALID = 'FALSE' ORDER BY OBJECT_TYPE, OBJECT_NAME";

const DELTA: &str = "SELECT TABLE_NAME, SUM(RAW_RECORD_COUNT_IN_DELTA), SUM(MEMORY_SIZE_IN_DELTA), SUM(MEMORY_SIZE_IN_MAIN)
   FROM SYS.M_CS_TABLES WHERE SCHEMA_NAME = ?
  GROUP BY TABLE_NAME HAVING SUM(MEMORY_SIZE_IN_DELTA) >= 104857600
  ORDER BY SUM(MEMORY_SIZE_IN_DELTA) DESC";

const AUTOMERGE_OFF: &str = "SELECT TOP 200 TABLE_NAME FROM SYS.TABLES
  WHERE SCHEMA_NAME = ? AND IS_COLUMN_TABLE = 'TRUE' AND AUTO_MERGE_ON = 'FALSE' AND IS_TEMPORARY = 'FALSE'
  ORDER BY TABLE_NAME";

const NO_PK: &str = "SELECT TOP 200 t.TABLE_NAME FROM SYS.TABLES t
  WHERE t.SCHEMA_NAME = ? AND t.IS_TEMPORARY = 'FALSE' AND t.IS_USER_DEFINED_TYPE = 'FALSE' AND t.TABLE_TYPE IN ('COLUMN', 'ROW')
    AND NOT EXISTS (SELECT 1 FROM SYS.CONSTRAINTS c
                     WHERE c.SCHEMA_NAME = t.SCHEMA_NAME AND c.TABLE_NAME = t.TABLE_NAME AND c.IS_PRIMARY_KEY = 'TRUE')
  ORDER BY t.TABLE_NAME";

/// Virtual tables (remote sources) the optimizer knows nothing about.
const VIRTUAL_NO_STATS: &str = "SELECT TOP 200 v.TABLE_NAME FROM SYS.VIRTUAL_TABLES v
  WHERE v.SCHEMA_NAME = ?
    AND NOT EXISTS (SELECT 1 FROM SYS.DATA_STATISTICS d
                     WHERE d.DATA_SOURCE_SCHEMA_NAME = v.SCHEMA_NAME AND d.DATA_SOURCE_OBJECT_NAME = v.TABLE_NAME)
  ORDER BY v.TABLE_NAME";

impl HanaSession {
    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let schema = if database.is_empty() { self.schema.clone() } else { database.to_string() };
        let p: [&str; 1] = [&schema];
        let mut out = Vec::new();

        if let Ok(rows) = self.rows(INVALID, &p).await {
            let objects: Vec<String> = rows.iter().map(|r| format!("{} ({})", s(r, 1), s(r, 0).to_lowercase())).collect();
            let fixes: Vec<String> = rows.iter().filter_map(|r| recompile_sql(&s(r, 0), &schema, &s(r, 1))).collect();
            let mut check = HealthCheck::new(
                "invalid_objects",
                "Integridad",
                if objects.is_empty() { "No hay objetos inválidos".to_string() } else { format!("{} objetos inválidos", objects.len()) },
                if objects.is_empty() { Severity::Ok } else { Severity::Warning },
            )
            .detail("Un objeto inválido falla al usarse: suele quedar así cuando se borra o cambia una tabla o vista de la que depende. Los procedimientos y funciones se recompilan; las vistas vuelven a ser válidas cuando existe lo que usan, o hay que crearlas de nuevo.")
            .objects(objects);
            if !fixes.is_empty() {
                check = check.fix(fixes.join("\n"));
            }
            out.push(check);
        }

        if let Ok(rows) = self.rows(DELTA, &p).await {
            let big: Vec<&Vec<HdbValue<'static>>> = rows.iter().filter(|r| needs_merge(n(r, 2), n(r, 3))).collect();
            let objects: Vec<String> = big
                .iter()
                .map(|r| format!("{} (delta {:.0} MB, {} filas)", s(r, 0), n(r, 2) / MIB, s(r, 1)))
                .collect();
            let fixes: Vec<String> = big.iter().take(MAX_OBJECTS).map(|r| format!("MERGE DELTA OF {};", qn(&schema, &s(r, 0)))).collect();
            let mut check = HealthCheck::new(
                "delta_merge",
                "Rendimiento",
                if objects.is_empty() { "Ningún delta store pide fusión".to_string() } else { format!("{} tablas con el delta store grande", objects.len()) },
                if objects.is_empty() { Severity::Ok } else { Severity::Warning },
            )
            .detail("Lo que se escribe en una tabla columnar va primero al delta store, que ocupa más memoria y se lee más lento que el principal. La fusión automática lo vacía; si quedó grande, fusionalo a mano (usa CPU y memoria mientras corre).")
            .objects(objects);
            if !fixes.is_empty() {
                check = check.fix(fixes.join("\n"));
            }
            out.push(check);
        }

        if let Ok(rows) = self.rows(AUTOMERGE_OFF, &p).await {
            if !rows.is_empty() {
                let objects: Vec<String> = rows.iter().map(|r| s(r, 0)).collect();
                let fixes: Vec<String> = objects.iter().map(|t| format!("ALTER TABLE {} ENABLE AUTOMERGE;", qn(&schema, t))).collect();
                out.push(
                    HealthCheck::new("auto_merge_off", "Configuración", format!("{} tablas con la fusión automática apagada", objects.len()), Severity::Info)
                        .detail("Sin fusión automática el delta store solo se vacía con MERGE DELTA. Está bien si una carga lo hace a mano al terminar; si no, crece sin límite.")
                        .objects(objects)
                        .fix(fixes.join("\n")),
                );
            }
        }

        if let Ok(rows) = self.rows(NO_PK, &p).await {
            let objects: Vec<String> = rows.iter().map(|r| s(r, 0)).collect();
            out.push(
                HealthCheck::new(
                    "no_primary_key",
                    "Diseño",
                    if objects.is_empty() { "Todas las tablas tienen clave primaria".to_string() } else { format!("{} tablas sin clave primaria", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Info },
                )
                .detail("Sin clave primaria no hay forma segura de identificar una fila: se complican la edición de datos, la replicación y las comparaciones.")
                .objects(objects),
            );
        }

        if let Ok(rows) = self.rows(VIRTUAL_NO_STATS, &p).await {
            if !rows.is_empty() {
                let objects: Vec<String> = rows.iter().map(|r| s(r, 0)).collect();
                let fixes: Vec<String> = objects.iter().map(|t| format!("CREATE STATISTICS ON {} TYPE RECORD COUNT;", qn(&schema, t))).collect();
                out.push(
                    HealthCheck::new("virtual_table_stats", "Rendimiento", format!("{} tablas virtuales sin estadísticas", objects.len()), Severity::Info)
                        .detail("Las tablas columnares no necesitan estadísticas, pero las virtuales (de una fuente remota) sí: sin ellas el optimizador no sabe cuántas filas trae y puede elegir mal dónde hacer los joins. Crear una de tipo HISTOGRAM sobre las columnas de filtro ayuda más.")
                        .objects(objects)
                        .fix(fixes.join("\n")),
                );
            }
        }

        for c in &mut out {
            c.objects.truncate(MAX_OBJECTS);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recompile_and_merge_rules() {
        assert_eq!(recompile_sql("PROCEDURE", "S", "p\"x").unwrap(), "ALTER PROCEDURE \"S\".\"p\"\"x\" RECOMPILE;");
        assert!(recompile_sql("VIEW", "S", "V").is_none());
        assert!(needs_merge(2048.0 * MIB, 1.0e12));
        assert!(needs_merge(200.0 * MIB, 500.0 * MIB));
        assert!(!needs_merge(200.0 * MIB, 5000.0 * MIB));
        assert!(!needs_merge(50.0 * MIB, 0.0));
    }
}

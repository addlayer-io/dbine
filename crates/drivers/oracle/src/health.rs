//! "Chequeo de salud" findings of Oracle ([`dbine_driver::Session::health_checks`]),
//! for the schema that is the "database" here: invalid objects, unusable
//! indexes, tablespaces near full (DBA_TABLESPACE_USAGE_METRICS, when
//! readable), stale or missing statistics, foreign keys without an index,
//! tables without a primary key, sequences near their limit and the
//! recycle bin. Each check is its own query: one that fails (no access to a
//! DBA_ view, an older server) is skipped.

use crate::{err, quote, OracleSession};
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::Result;
use oracledb::Connection;
use std::collections::BTreeMap;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;

type Rows = Vec<Vec<Option<String>>>;

/// The first `n` columns of every row of `sql` as text, `owner` bound to
/// each of its `binds` placeholders.
fn texts(c: &Connection, sql: &str, owner: &str, binds: usize, n: usize) -> Result<Rows> {
    let params: Vec<&dyn oracledb::ToDbValue> = (0..binds).map(|_| &owner as &dyn oracledb::ToDbValue).collect();
    let rows = c.statement(sql).map_err(err)?.exclude_from_cache().build().map_err(err)?.query(&params).map_err(err)?;
    let mut out = Vec::new();
    for r in rows {
        let r = r.map_err(err)?;
        out.push((0..n).map(|i| r.get::<Option<String>>(i).map_err(err)).collect::<Result<Vec<_>>>()?);
    }
    Ok(out)
}

fn col(r: &[Option<String>], i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

fn num(r: &[Option<String>], i: usize) -> f64 {
    col(r, i).trim().parse().unwrap_or(0.0)
}

/// `'text'`, doubling embedded quotes.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `"OWNER"."NAME"`.
fn qn(owner: &str, name: &str) -> String {
    format!("{}.{}", quote(owner), quote(name))
}

/// A fix only when there's something to fix.
fn fix_if(c: HealthCheck, when: bool, script: String) -> HealthCheck {
    if when {
        c.fix(script)
    } else {
        c
    }
}

/// The statement that recompiles an invalid object, for the kinds that
/// have one.
pub(crate) fn compile_sql(kind: &str, owner: &str, name: &str) -> Option<String> {
    let n = qn(owner, name);
    Some(match kind {
        "PACKAGE BODY" => format!("ALTER PACKAGE {n} COMPILE BODY;"),
        "TYPE BODY" => format!("ALTER TYPE {n} COMPILE BODY;"),
        "PUBLIC SYNONYM" => return None,
        "VIEW" | "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "MATERIALIZED VIEW" | "SYNONYM" | "DIMENSION" => {
            format!("ALTER {kind} {n} COMPILE;")
        }
        "JAVA CLASS" | "JAVA SOURCE" => format!("ALTER {kind} {n} RESOLVE;"),
        _ => return None,
    })
}

/// Foreign keys none of whose indexes starts with all its columns (in
/// any order): `fks` (table, constraint, column, position) and `indexes`
/// (table, index, column, position).
pub(crate) fn unindexed_fks(fks: &Rows, indexes: &Rows) -> Vec<(String, String, Vec<String>)> {
    let mut by_fk: BTreeMap<(String, String), Vec<(i64, String)>> = BTreeMap::new();
    for r in fks {
        by_fk.entry((col(r, 0), col(r, 1))).or_default().push((num(r, 3) as i64, col(r, 2)));
    }
    let mut by_index: BTreeMap<(String, String), Vec<(i64, String)>> = BTreeMap::new();
    for r in indexes {
        by_index.entry((col(r, 0), col(r, 1))).or_default().push((num(r, 3) as i64, col(r, 2)));
    }
    let mut out = Vec::new();
    for ((table, fk), mut cols) in by_fk {
        cols.sort();
        let n = cols.len() as i64;
        let covered = by_index.iter().filter(|((t, _), _)| *t == table).any(|(_, ic)| {
            let lead: Vec<&String> = ic.iter().filter(|(p, _)| *p <= n).map(|(_, c)| c).collect();
            lead.len() == cols.len() && cols.iter().all(|(_, c)| lead.contains(&c))
        });
        if !covered {
            out.push((table, fk, cols.into_iter().map(|(_, c)| c).collect()));
        }
    }
    out
}

/// An index name for `constraint` that fits Oracle's oldest limit (30).
fn index_name(constraint: &str) -> String {
    let base: String = constraint.chars().take(27).collect();
    format!("{base}_IX")
}

fn gib(bytes: f64) -> String {
    if bytes >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", bytes / 1024.0 / 1024.0 / 1024.0)
    } else if bytes >= 1024.0 * 1024.0 {
        format!("{:.0} MB", bytes / 1024.0 / 1024.0)
    } else {
        format!("{:.0} KB", bytes / 1024.0)
    }
}

const INVALID: &str = "SELECT object_type, object_name FROM all_objects
  WHERE owner = :1 AND status = 'INVALID' AND object_name NOT LIKE 'BIN$%' AND ROWNUM <= 200
  ORDER BY object_type, object_name";

const UNUSABLE: &str = "SELECT * FROM (
  SELECT i.table_name, i.index_name, NULL FROM all_indexes i WHERE i.owner = :1 AND i.status = 'UNUSABLE'
  UNION ALL
  SELECT i.table_name, p.index_name, p.partition_name FROM all_ind_partitions p
    JOIN all_indexes i ON i.owner = p.index_owner AND i.index_name = p.index_name
   WHERE p.index_owner = :2 AND p.status = 'UNUSABLE'
  UNION ALL
  SELECT i.table_name, p.index_name, p.subpartition_name FROM all_ind_subpartitions p
    JOIN all_indexes i ON i.owner = p.index_owner AND i.index_name = p.index_name
   WHERE p.index_owner = :3 AND p.status = 'UNUSABLE'
) WHERE ROWNUM <= 200";

/// The tablespaces the schema uses (its default one and those of its
/// segments), by how full they are against their maximum size.
const TABLESPACES: &str = "SELECT m.tablespace_name, TO_CHAR(ROUND(m.used_percent, 1), 'FM990.0')
   FROM dba_tablespace_usage_metrics m
  WHERE m.tablespace_name IN (SELECT default_tablespace FROM dba_users WHERE username = :1
                              UNION SELECT tablespace_name FROM dba_segments WHERE owner = :2)
  ORDER BY m.used_percent DESC";

const STATS: &str = "SELECT s.table_name, CASE WHEN s.last_analyzed IS NULL THEN 'N' ELSE 'S' END
   FROM all_tab_statistics s
   JOIN all_tables t ON t.owner = s.owner AND t.table_name = s.table_name
  WHERE s.owner = :1 AND s.object_type = 'TABLE' AND t.temporary = 'N' AND t.dropped = 'NO'
    AND s.table_name NOT LIKE 'BIN$%' AND NVL(s.stattype_locked, 'NONE') = 'NONE'
    AND (s.stale_stats = 'YES' OR s.last_analyzed IS NULL)
    AND NOT EXISTS (SELECT 1 FROM all_external_tables x WHERE x.owner = s.owner AND x.table_name = s.table_name)
    AND ROWNUM <= 200
  ORDER BY s.table_name";

const FK_COLUMNS: &str = "SELECT c.table_name, c.constraint_name, cc.column_name, TO_CHAR(cc.position)
   FROM all_constraints c
   JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name
  WHERE c.owner = :1 AND c.constraint_type = 'R' AND c.table_name NOT LIKE 'BIN$%'";

const INDEX_COLUMNS: &str = "SELECT table_name, index_owner || '.' || index_name, column_name, TO_CHAR(column_position)
   FROM all_ind_columns WHERE table_owner = :1";

const NO_PK: &str = "SELECT t.table_name FROM all_tables t
  WHERE t.owner = :1 AND t.temporary = 'N' AND t.nested = 'NO' AND t.secondary = 'N' AND t.dropped = 'NO'
    AND t.table_name NOT LIKE 'BIN$%' AND t.table_name NOT LIKE 'MLOG$%' AND t.table_name NOT LIKE 'RUPD$%'
    AND NOT EXISTS (SELECT 1 FROM all_constraints c WHERE c.owner = t.owner AND c.table_name = t.table_name AND c.constraint_type = 'P')
    AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner AND m.container_name = t.table_name)
    AND NOT EXISTS (SELECT 1 FROM all_external_tables x WHERE x.owner = t.owner AND x.table_name = t.table_name)
    AND ROWNUM <= 200
  ORDER BY t.table_name";

/// Share of the range used, by the direction the sequence moves in.
const SEQUENCES: &str = "SELECT sequence_name, TO_CHAR(ROUND(used, 1), 'FM990.0'), CASE WHEN increment_by > 0 THEN 'U' ELSE 'D' END
   FROM (SELECT sequence_name, increment_by,
                100 * CASE WHEN increment_by > 0 THEN last_number - min_value ELSE max_value - last_number END
                    / NULLIF(max_value - min_value, 0) AS used
           FROM all_sequences WHERE sequence_owner = :1 AND cycle_flag = 'N')
  WHERE used >= 80 AND ROWNUM <= 200
  ORDER BY used DESC";

const RECYCLEBIN: &str = "SELECT r.original_name, r.object_name, r.type, TO_CHAR(r.space * NVL(t.block_size, 8192))
   FROM dba_recyclebin r LEFT JOIN dba_tablespaces t ON t.tablespace_name = r.ts_name
  WHERE r.owner = :1";

/// The connected user's own recycle bin, when the DBA view can't be read.
const OWN_RECYCLEBIN: &str = "SELECT r.original_name, r.object_name, r.type, TO_CHAR(r.space * 8192), USER
   FROM user_recyclebin r WHERE USER = :1";

impl OracleSession {
    async fn health_rows(&mut self, sql: &'static str, owner: &str, binds: usize, n: usize) -> Result<Rows> {
        let owner = owner.to_string();
        self.run(move |c| texts(c, sql, &owner, binds, n)).await
    }

    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let owner = if database.is_empty() { self.schema.clone() } else { database.to_string() };
        let mut out = Vec::new();

        // Invalid objects: they fail when used until they compile again.
        if let Ok(rows) = self.health_rows(INVALID, &owner, 1, 2).await {
            let objects: Vec<String> = rows.iter().map(|r| format!("{} ({})", col(r, 1), col(r, 0).to_lowercase())).collect();
            let fixes: Vec<String> = rows.iter().filter_map(|r| compile_sql(&col(r, 0), &owner, &col(r, 1))).collect();
            out.push(fix_if(
                HealthCheck::new(
                    "invalid_objects",
                    "Integridad",
                    if objects.is_empty() { "No hay objetos inválidos".to_string() } else { format!("{} objetos inválidos", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Warning },
                )
                .detail("Un objeto inválido falla al usarse si no compila: suele quedar así cuando cambia algo de lo que depende. Recompilalo; si sigue inválido, SHOW ERRORS muestra el motivo.")
                .objects(objects),
                !fixes.is_empty(),
                fixes.join("\n"),
            ));
        }

        // Unusable indexes (or partitions of them).
        if let Ok(rows) = self.health_rows(UNUSABLE, &owner, 3, 3).await {
            let objects: Vec<String> = rows
                .iter()
                .map(|r| match r[2].as_deref() {
                    Some(p) => format!("{} · {} (partición {p})", col(r, 0), col(r, 1)),
                    None => format!("{} · {}", col(r, 0), col(r, 1)),
                })
                .collect();
            let fixes: Vec<String> = rows
                .iter()
                .map(|r| match r[2].as_deref() {
                    Some(p) => format!("ALTER INDEX {} REBUILD PARTITION {};", qn(&owner, &col(r, 1)), quote(p)),
                    None => format!("ALTER INDEX {} REBUILD;", qn(&owner, &col(r, 1))),
                })
                .collect();
            out.push(fix_if(
                HealthCheck::new(
                    "unusable_indexes",
                    "Rendimiento",
                    if objects.is_empty() { "No hay índices inutilizables".to_string() } else { format!("{} índices inutilizables (UNUSABLE)", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Critical },
                )
                .detail("Un índice UNUSABLE no se usa en las consultas y, si es único, hace fallar los INSERT y UPDATE de la tabla. Suele quedar así después de mover la tabla o de una carga directa. Reconstruirlo lo vuelve a habilitar.")
                .objects(objects),
                !fixes.is_empty(),
                fixes.join("\n"),
            ));
        }

        // Tablespaces near full (against their maximum size, autoextend included).
        if let Ok(rows) = self.health_rows(TABLESPACES, &owner, 2, 2).await {
            if !rows.is_empty() {
                let full: Vec<&Vec<Option<String>>> = rows.iter().filter(|r| num(r, 1) >= 85.0).collect();
                let max = rows.iter().map(|r| num(r, 1)).fold(0.0, f64::max);
                let (title, sev) = if full.is_empty() {
                    (format!("Tablespaces con espacio (el más lleno al {max:.0} %)"), Severity::Ok)
                } else if max >= 95.0 {
                    (format!("{} tablespaces casi llenos (hasta {max:.0} %)", full.len()), Severity::Critical)
                } else {
                    (format!("{} tablespaces por encima del 85 %", full.len()), Severity::Warning)
                };
                out.push(
                    HealthCheck::new("tablespace_usage", "Espacio", title, sev)
                        .detail("El porcentaje es sobre el tamaño máximo, contando el autoextend de los archivos. Al llenarse, las inserciones fallan con ORA-01653/ORA-01688: agregá un archivo de datos o agrandá los existentes antes de llegar.")
                        .objects(full.iter().map(|r| format!("{} ({} %)", col(r, 0), col(r, 1))).collect()),
                );
            }
        }

        // Stale or missing optimizer statistics.
        if let Ok(rows) = self.health_rows(STATS, &owner, 1, 2).await {
            let missing = rows.iter().filter(|r| col(r, 1) == "N").count();
            let objects: Vec<String> = rows
                .iter()
                .map(|r| format!("{} ({})", col(r, 0), if col(r, 1) == "N" { "sin estadísticas" } else { "desactualizadas" }))
                .collect();
            let fixes: Vec<String> = rows
                .iter()
                .map(|r| format!("EXEC DBMS_STATS.GATHER_TABLE_STATS(ownname => {}, tabname => {});", lit(&owner), lit(&col(r, 0))))
                .collect();
            let title = match (rows.len(), missing) {
                (0, _) => "Estadísticas al día".to_string(),
                (n, 0) => format!("{n} tablas con estadísticas desactualizadas"),
                (n, m) if m == n => format!("{n} tablas sin estadísticas"),
                (n, m) => format!("{n} tablas con estadísticas viejas o faltantes ({m} sin estadísticas)"),
            };
            out.push(fix_if(
                HealthCheck::new("stale_stats", "Rendimiento", title, if rows.is_empty() { Severity::Ok } else { Severity::Warning })
                    .detail("Sin estadísticas, o con más de 10 % de filas cambiadas desde las últimas, el optimizador estima mal y elige planes lentos. La tarea automática de mantenimiento las junta de noche; si está apagada o la tabla cambia mucho de día, juntalas a mano.")
                    .objects(objects),
                !fixes.is_empty(),
                fixes.join("\n"),
            ));
        }

        // Foreign keys without an index that starts with their columns.
        let fks = self.health_rows(FK_COLUMNS, &owner, 1, 4).await;
        let idx = self.health_rows(INDEX_COLUMNS, &owner, 1, 4).await;
        if let (Ok(fks), Ok(idx)) = (fks, idx) {
            let found = unindexed_fks(&fks, &idx);
            let objects: Vec<String> = found.iter().map(|(t, fk, cols)| format!("{t} ({}) · {fk}", cols.join(", "))).collect();
            let fixes: Vec<String> = found
                .iter()
                .take(MAX_OBJECTS)
                .map(|(t, fk, cols)| {
                    let cols: Vec<String> = cols.iter().map(|c| quote(c)).collect();
                    format!("CREATE INDEX {} ON {} ({});", qn(&owner, &index_name(fk)), qn(&owner, t), cols.join(", "))
                })
                .collect();
            out.push(fix_if(
                HealthCheck::new(
                    "fk_without_index",
                    "Rendimiento",
                    if objects.is_empty() { "Todas las claves foráneas tienen índice".to_string() } else { format!("{} claves foráneas sin índice", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Warning },
                )
                .detail("Sin índice, borrar o cambiar la clave en la tabla padre recorre la tabla hija entera y la bloquea completa mientras dura (TM), y los joins por esa columna son más lentos.")
                .objects(objects),
                !fixes.is_empty(),
                fixes.join("\n"),
            ));
        }

        // Tables without a primary key.
        if let Ok(rows) = self.health_rows(NO_PK, &owner, 1, 1).await {
            let objects: Vec<String> = rows.iter().map(|r| col(r, 0)).collect();
            out.push(
                HealthCheck::new(
                    "no_primary_key",
                    "Diseño",
                    if objects.is_empty() { "Todas las tablas tienen clave primaria".to_string() } else { format!("{} tablas sin clave primaria", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Info },
                )
                .detail("Sin clave primaria no hay forma segura de identificar una fila: se complican la edición de datos, la replicación (GoldenGate, Data Guard lógico) y las comparaciones.")
                .objects(objects),
            );
        }

        // Sequences that used most of their range and don't cycle.
        if let Ok(rows) = self.health_rows(SEQUENCES, &owner, 1, 3).await {
            let objects: Vec<String> = rows.iter().map(|r| format!("{} ({} % usado)", col(r, 0), col(r, 1))).collect();
            let max = rows.iter().map(|r| num(r, 1)).fold(0.0, f64::max);
            let fixes: Vec<String> = rows
                .iter()
                .map(|r| {
                    let n = qn(&owner, &col(r, 0));
                    if col(r, 2) == "U" {
                        format!("ALTER SEQUENCE {n} MAXVALUE 9999999999999999999999999999;")
                    } else {
                        format!("ALTER SEQUENCE {n} MINVALUE -999999999999999999999999999;")
                    }
                })
                .collect();
            out.push(fix_if(
                HealthCheck::new(
                    "sequences_near_limit",
                    "Integridad",
                    if objects.is_empty() { "Ninguna secuencia está cerca de su límite".to_string() } else { format!("{} secuencias usaron más del 80 % de su rango", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else if max >= 95.0 { Severity::Critical } else { Severity::Warning },
                )
                .detail("Cuando una secuencia sin CYCLE llega a su límite, NEXTVAL falla (ORA-08004) y con ella los INSERT que la usan. Ampliá el límite si la columna admite valores más grandes.")
                .objects(objects),
                !fixes.is_empty(),
                fixes.join("\n"),
            ));
        }

        // The recycle bin: dropped tables still hold their space.
        let bin = match self.health_rows(RECYCLEBIN, &owner, 1, 4).await {
            Ok(rows) => Some((rows, false)),
            Err(_) => self.health_rows(OWN_RECYCLEBIN, &owner, 1, 4).await.ok().filter(|r| !r.is_empty()).map(|r| (r, true)),
        };
        if let Some((rows, own)) = bin {
            let bytes: f64 = rows.iter().map(|r| num(r, 3)).sum();
            let tables: Vec<&Vec<Option<String>>> = rows.iter().filter(|r| col(r, 2) == "TABLE").collect();
            let objects: Vec<String> = tables.iter().map(|r| format!("{} ({})", col(r, 0), col(r, 1))).collect();
            let fix = if own {
                "PURGE RECYCLEBIN;".to_string()
            } else {
                tables.iter().take(MAX_OBJECTS).map(|r| format!("PURGE TABLE {};", qn(&owner, &col(r, 1)))).collect::<Vec<_>>().join("\n")
            };
            let (title, sev) = if rows.is_empty() {
                ("La papelera de reciclaje está vacía".to_string(), Severity::Ok)
            } else {
                (
                    format!("{} tablas borradas en la papelera ({}{})", tables.len(), if own { "unos " } else { "" }, gib(bytes)),
                    if bytes >= 1024.0 * 1024.0 * 1024.0 { Severity::Warning } else { Severity::Info },
                )
            };
            out.push(fix_if(
                HealthCheck::new("recyclebin", "Espacio", title, sev)
                    .detail("Las tablas borradas con DROP TABLE quedan en la papelera (BIN$…) y siguen ocupando espacio hasta que Oracle lo necesita o se purgan. Purgarlas es definitivo: ya no se pueden recuperar con FLASHBACK TABLE … TO BEFORE DROP.")
                    .objects(objects),
                !rows.is_empty() && !fix.is_empty(),
                fix,
            ));
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

    fn rows(v: &[(&str, &str, &str, i64)]) -> Rows {
        v.iter().map(|(a, b, c, d)| vec![Some(a.to_string()), Some(b.to_string()), Some(c.to_string()), Some(d.to_string())]).collect()
    }

    #[test]
    fn an_fk_is_covered_by_an_index_that_starts_with_its_columns() {
        let fks = rows(&[("T", "FK1", "A", 1), ("T", "FK1", "B", 2), ("T", "FK2", "C", 1), ("U", "FK3", "X", 1)]);
        let idx = rows(&[("T", "I1", "B", 1), ("T", "I1", "A", 2), ("T", "I1", "Z", 3), ("T", "I2", "Z", 1), ("T", "I2", "C", 2), ("V", "I3", "X", 1)]);
        let found = unindexed_fks(&fks, &idx);
        let names: Vec<&str> = found.iter().map(|(_, fk, _)| fk.as_str()).collect();
        assert_eq!(names, ["FK2", "FK3"]);
    }

    #[test]
    fn compile_statements_by_kind() {
        assert_eq!(compile_sql("PACKAGE BODY", "S", "P").unwrap(), "ALTER PACKAGE \"S\".\"P\" COMPILE BODY;");
        assert_eq!(compile_sql("VIEW", "S", "v\"x").unwrap(), "ALTER VIEW \"S\".\"v\"\"x\" COMPILE;");
        assert!(compile_sql("TABLE", "S", "T").is_none());
        assert_eq!(index_name("A_VERY_LONG_CONSTRAINT_NAME_OVER_30"), "A_VERY_LONG_CONSTRAINT_NAME_IX");
        assert_eq!(lit("O'Brien"), "'O''Brien'");
    }
}

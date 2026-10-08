//! "Chequeo de salud" findings of Snowflake ([`dbine_driver::Session::health_checks`]),
//! cost and maintenance only, all from `SHOW` commands (the cloud services
//! layer answers them: no warehouse wakes up and no data is scanned):
//!
//! - the database's Time Travel retention at 0 (no UNDROP, no recovery);
//! - large tables without a clustering key, and small ones that pay for
//!   automatic reclustering;
//! - large tables with a long Time Travel retention (storage);
//! - dropped tables still held by Time Travel (storage);
//! - warehouses that never suspend, or take long to (credits).
//!
//! Each check is its own command: one that fails is skipped.

use crate::ddl::Row;
use crate::SnowflakeSession;
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::Result;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
/// From this size up a clustering key usually pays off.
const CLUSTER_BYTES: f64 = 1024.0 * GB;
/// Below this size automatic reclustering rarely pays for itself.
const SMALL_BYTES: f64 = 100.0 * GB;
/// Tables from this size up are worth a retention thought.
const RETENTION_BYTES: f64 = 100.0 * GB;

fn get(r: &Row, k: &str) -> String {
    r.get(k).cloned().unwrap_or_default()
}

fn num(r: &Row, k: &str) -> f64 {
    get(r, k).trim().parse().unwrap_or(0.0)
}

fn size(bytes: f64) -> String {
    if bytes >= 1024.0 * GB {
        format!("{:.1} TB", bytes / 1024.0 / GB)
    } else {
        format!("{:.1} GB", bytes / GB)
    }
}

/// `"DB"."SCHEMA"."TABLE"`.
fn full(db: &str, r: &Row) -> String {
    format!("{}.{}", qualified_name(Quote::Double, None, db), qualified_name(Quote::Double, Some(&get(r, "schema_name")), &get(r, "name")))
}

fn short(r: &Row) -> String {
    format!("{}.{}", get(r, "schema_name"), get(r, "name"))
}

/// Ordinary tables: no external, hybrid, Iceberg, dynamic or event tables
/// (their storage and clustering work differently).
fn plain_table(r: &Row) -> bool {
    let no = |k: &str| !get(r, k).eq_ignore_ascii_case("Y");
    matches!(get(r, "kind").to_ascii_uppercase().as_str(), "TABLE" | "TRANSIENT")
        && no("is_external")
        && no("is_hybrid")
        && no("is_iceberg")
        && no("is_dynamic")
        && no("is_event")
}

/// Severity of a warehouse's AUTO_SUSPEND (seconds; empty or 0: never).
pub(crate) fn suspend_severity(auto_suspend: &str) -> Severity {
    match auto_suspend.trim().parse::<i64>() {
        Ok(s) if s > 0 && s <= 600 => Severity::Ok,
        Ok(s) if s > 0 => Severity::Info,
        _ => Severity::Warning,
    }
}

impl SnowflakeSession {
    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let mut out = Vec::new();
        let dbq = qualified_name(Quote::Double, None, database);

        if let Ok(rows) = self.named_rows(&format!("SHOW PARAMETERS LIKE 'DATA_RETENTION_TIME_IN_DAYS' IN DATABASE {dbq}")).await {
            if let Some(r) = rows.first() {
                let days = num(r, "value") as i64;
                out.push(
                    HealthCheck::new(
                        "time_travel",
                        "Backups",
                        if days == 0 { "Time Travel apagado (retención de 0 días)".to_string() } else { format!("Time Travel: {days} días de retención") },
                        if days == 0 { Severity::Warning } else { Severity::Ok },
                    )
                    .detail("Con 0 días no se puede recuperar una tabla borrada (UNDROP) ni consultar o clonar datos de antes de un cambio. Más días cuestan almacenamiento por lo que cambia; 1 día es la base razonable."),
                );
                if days == 0 {
                    if let Some(c) = out.last_mut() {
                        c.fix = Some(format!("ALTER DATABASE {dbq} SET DATA_RETENTION_TIME_IN_DAYS = 1;"));
                    }
                }
            }
        }

        let tables = self.named_rows(&format!("SHOW TABLES IN DATABASE {dbq}")).await;
        if let Ok(tables) = &tables {
            let plain: Vec<&Row> = tables.iter().filter(|r| plain_table(r)).collect();

            // Large tables without a clustering key.
            let mut big: Vec<&&Row> = plain.iter().filter(|r| get(r, "cluster_by").trim().is_empty() && num(r, "bytes") >= CLUSTER_BYTES).collect();
            big.sort_by(|a, b| num(b, "bytes").total_cmp(&num(a, "bytes")));
            out.push(
                HealthCheck::new(
                    "no_clustering",
                    "Costos",
                    if big.is_empty() { "Ninguna tabla de más de 1 TB sin clave de clustering".to_string() } else { format!("{} tablas de más de 1 TB sin clave de clustering", big.len()) },
                    if big.is_empty() { Severity::Ok } else { Severity::Info },
                )
                .detail("En tablas muy grandes, una clave de clustering sobre las columnas de filtro hace que las consultas lean menos micro-particiones (menos créditos). Mantenerla también cuesta: el reclustering automático consume créditos. Conviene solo si las consultas filtran siempre por esas columnas.")
                .objects(big.iter().map(|r| format!("{} ({})", short(r), size(num(r, "bytes")))).collect()),
            );

            // Small tables paying for automatic reclustering.
            let small: Vec<&&Row> = plain
                .iter()
                .filter(|r| !get(r, "cluster_by").trim().is_empty() && get(r, "automatic_clustering").eq_ignore_ascii_case("ON") && num(r, "bytes") < SMALL_BYTES)
                .collect();
            if !small.is_empty() {
                out.push(
                    HealthCheck::new("small_clustered", "Costos", format!("{} tablas chicas con reclustering automático", small.len()), Severity::Info)
                        .detail("El reclustering automático consume créditos de servicio cada vez que la tabla cambia; en tablas de menos de 100 GB casi nunca se nota en las consultas. Suspendelo si no ves la diferencia (AUTOMATIC_CLUSTERING_HISTORY muestra lo que cuesta).")
                        .objects(small.iter().map(|r| format!("{} ({}, {})", short(r), size(num(r, "bytes")), get(r, "cluster_by"))).collect())
                        .fix(small.iter().take(MAX_OBJECTS).map(|r| format!("ALTER TABLE {} SUSPEND RECLUSTER;", full(database, r))).collect::<Vec<_>>().join("\n")),
                );
            }

            // Large tables with a long Time Travel retention.
            let long: Vec<&&Row> = plain.iter().filter(|r| num(r, "retention_time") > 7.0 && num(r, "bytes") >= RETENTION_BYTES).collect();
            if !long.is_empty() {
                out.push(
                    HealthCheck::new("long_retention", "Costos", format!("{} tablas grandes con más de 7 días de Time Travel", long.len()), Severity::Info)
                        .detail("Time Travel guarda una copia de cada micro-partición que cambia o se borra durante toda la retención, y después pasa 7 días más a Fail-safe: en tablas grandes que cambian mucho, eso es almacenamiento que se paga. Bajala si no necesitás volver tan atrás.")
                        .objects(long.iter().map(|r| format!("{} ({}, {} días)", short(r), size(num(r, "bytes")), get(r, "retention_time"))).collect())
                        .fix(long.iter().take(MAX_OBJECTS).map(|r| format!("ALTER TABLE {} SET DATA_RETENTION_TIME_IN_DAYS = 7;", full(database, r))).collect::<Vec<_>>().join("\n")),
                );
            }
        }

        // Dropped tables still in Time Travel.
        if let Ok(rows) = self.named_rows(&format!("SHOW TABLES HISTORY IN DATABASE {dbq}")).await {
            let dropped: Vec<&Row> = rows.iter().filter(|r| !get(r, "dropped_on").trim().is_empty()).collect();
            if !dropped.is_empty() {
                let bytes: f64 = dropped.iter().map(|r| num(r, "bytes")).sum();
                out.push(
                    HealthCheck::new("dropped_tables", "Costos", format!("{} tablas borradas siguen en Time Travel ({})", dropped.len(), size(bytes)), Severity::Info)
                        .detail("Una tabla borrada sigue ocupando (y cobrando) almacenamiento hasta que vence su retención, y después 7 días más de Fail-safe. Se puede recuperar con UNDROP TABLE mientras tanto.")
                        .objects(dropped.iter().map(|r| format!("{} (borrada {})", short(r), get(r, "dropped_on"))).collect()),
                );
            }
        }

        // Warehouses that never suspend, or take long to.
        if let Ok(rows) = self.named_rows("SHOW WAREHOUSES").await {
            let bad: Vec<&Row> = rows.iter().filter(|r| suspend_severity(&get(r, "auto_suspend")) != Severity::Ok).collect();
            let sev = bad.iter().map(|r| suspend_severity(&get(r, "auto_suspend"))).max().unwrap_or(Severity::Ok);
            if !rows.is_empty() {
                out.push(
                    HealthCheck::new(
                        "warehouse_suspend",
                        "Costos",
                        if bad.is_empty() { "Los warehouses se suspenden solos en 10 minutos o menos".to_string() } else { format!("{} warehouses que no se suspenden pronto", bad.len()) },
                        sev,
                    )
                    .detail("Un warehouse encendido cobra créditos por segundo aunque no corra nada. Sin AUTO_SUSPEND (o con uno largo) queda prendido sin uso; 60 a 300 segundos suele alcanzar. Vale para toda la cuenta, no solo para esta base.")
                    .objects(
                        bad.iter()
                            .map(|r| {
                                let s = get(r, "auto_suspend");
                                let s = s.trim();
                                format!("{} ({})", get(r, "name"), if s.is_empty() || s == "0" { "nunca se suspende".to_string() } else { format!("se suspende a los {s} s") })
                            })
                            .collect(),
                    ),
                );
                if !bad.is_empty() {
                    if let Some(c) = out.last_mut() {
                        c.fix = Some(
                            bad.iter()
                                .map(|r| format!("ALTER WAREHOUSE {} SET AUTO_SUSPEND = 300;", qualified_name(Quote::Double, None, &get(r, "name"))))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                    }
                }
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

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn warehouse_suspend_and_table_kinds() {
        assert_eq!(suspend_severity(""), Severity::Warning);
        assert_eq!(suspend_severity("0"), Severity::Warning);
        assert_eq!(suspend_severity("60"), Severity::Ok);
        assert_eq!(suspend_severity("3600"), Severity::Info);
        assert!(plain_table(&row(&[("kind", "TABLE"), ("is_external", "N")])));
        assert!(!plain_table(&row(&[("kind", "TABLE"), ("is_iceberg", "Y")])));
        assert!(!plain_table(&row(&[("kind", "TEMPORARY")])));
        let r = row(&[("schema_name", "PUBLIC"), ("name", "a\"b")]);
        assert_eq!(full("DB", &r), "\"DB\".\"PUBLIC\".\"a\"\"b\"");
    }
}

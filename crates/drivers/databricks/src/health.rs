//! "Chequeo de salud" findings of Databricks ([`dbine_driver::Session::health_checks`]),
//! cost and maintenance of a Unity Catalog catalog, all from REST calls to
//! the control plane (`/api/2.0/sql/warehouses`, `/api/2.1/unity-catalog/…`):
//! nothing runs on the SQL warehouse, so none wakes up and no data is read.
//!
//! - the warehouse's auto stop (off, or long: DBUs while idle);
//! - predictive optimization off for the catalog (no automatic OPTIMIZE
//!   and VACUUM);
//! - Delta tables that keep deleted files long (storage);
//! - tables that aren't Delta (no data skipping, OPTIMIZE or time travel).
//!
//! Table sizes need DESCRIBE DETAIL on the warehouse, so "large tables
//! without clustering" isn't offered here. Each check is its own call: one
//! that fails is skipped.

use crate::ddl::lit;
use crate::DatabricksSession;
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::Result;
use serde_json::Value as Json;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;
/// Tables read at most, across the catalog's schemas.
const MAX_TABLES: usize = 5000;
/// Days of deleted files kept from which storage is worth a look (the
/// default is 7).
const RETENTION_DAYS: f64 = 30.0;

pub(crate) fn escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn bt(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

/// Days in a Delta interval (`interval 30 days`, `30 days`, `1 week`…).
pub(crate) fn interval_days(v: &str) -> Option<f64> {
    let v = v.trim().to_ascii_lowercase();
    let v = v.strip_prefix("interval").unwrap_or(&v).trim().to_string();
    let mut parts = v.split_whitespace();
    let n: f64 = parts.next()?.parse().ok()?;
    let unit = parts.next().unwrap_or("days");
    Some(match unit.trim_end_matches('s') {
        "second" => n / 86_400.0,
        "minute" => n / 1440.0,
        "hour" => n / 24.0,
        "day" => n,
        "week" => n * 7.0,
        "month" => n * 30.0,
        "year" => n * 365.0,
        _ => return None,
    })
}

/// Severity of a warehouse's auto stop in minutes (0: never).
pub(crate) fn auto_stop_severity(mins: i64) -> Severity {
    match mins {
        0 => Severity::Warning,
        m if m > 60 => Severity::Info,
        _ => Severity::Ok,
    }
}

impl DatabricksSession {
    /// Every page of a Unity Catalog list.
    pub(crate) async fn uc_list(&self, path: &str, key: &str, max: usize) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let sep = if path.contains('?') { '&' } else { '?' };
            let url = match &token {
                Some(t) => format!("{path}{sep}page_token={}", escape(t)),
                None => path.to_string(),
            };
            let resp = self.api.get(&url).await?;
            if let Some(items) = resp.get(key).and_then(Json::as_array) {
                out.extend(items.iter().cloned());
            }
            match resp.get("next_page_token").and_then(Json::as_str) {
                Some(t) if !t.is_empty() && out.len() < max => token = Some(t.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let cat = if database.is_empty() { self.catalog.clone().unwrap_or_default() } else { database.to_string() };
        let mut out = Vec::new();

        // The warehouse's auto stop.
        if let Ok(w) = self.api.get(&format!("/api/2.0/sql/warehouses/{}", escape(&self.warehouse))).await {
            let mins = w.get("auto_stop_mins").and_then(Json::as_i64).unwrap_or(0);
            let name = w.get("name").and_then(Json::as_str).unwrap_or(&self.warehouse).to_string();
            out.push(
                HealthCheck::new(
                    "warehouse_auto_stop",
                    "Costos",
                    if mins == 0 { format!("El warehouse «{name}» nunca se detiene solo") } else { format!("El warehouse «{name}» se detiene tras {mins} minutos sin uso") },
                    auto_stop_severity(mins),
                )
                .detail("Un SQL warehouse encendido consume DBUs aunque no corra nada. Con auto stop apagado o largo queda prendido sin uso: 10 minutos (o 1 a 5 en serverless) suele alcanzar. Se cambia en la configuración del warehouse, no con SQL."),
            );
        }

        if cat.is_empty() {
            return Ok(out);
        }

        // Predictive optimization of the catalog.
        if let Ok(c) = self.api.get(&format!("/api/2.1/unity-catalog/catalogs/{}", escape(&cat))).await {
            if let Some(v) = c.pointer("/effective_predictive_optimization_flag/value").and_then(Json::as_str) {
                let on = v.eq_ignore_ascii_case("ENABLE");
                let mut check = HealthCheck::new(
                    "predictive_optimization",
                    "Mantenimiento",
                    if on { "Optimización predictiva activa" } else { "Optimización predictiva apagada" },
                    if on { Severity::Ok } else { Severity::Info },
                )
                .detail("Con la optimización predictiva Databricks corre OPTIMIZE, VACUUM y ANALYZE solo en las tablas administradas que lo necesitan (se cobra como cómputo serverless). Sin ella hay que programarlos: si no, las tablas acumulan archivos chicos y archivos borrados que se pagan.");
                if !on {
                    check = check.fix(format!("ALTER CATALOG {} ENABLE PREDICTIVE OPTIMIZATION;", bt(&cat)));
                }
                out.push(check);
            }
        }

        // The catalog's tables, without columns.
        let Ok(schemas) = self.uc_list(&format!("/api/2.1/unity-catalog/schemas?catalog_name={}", escape(&cat)), "schemas", 10_000).await else {
            return Ok(out);
        };
        let mut tables: Vec<Json> = Vec::new();
        let mut failed = false;
        for s in schemas.iter().filter_map(|s| s.get("name").and_then(Json::as_str)).filter(|s| *s != "information_schema") {
            let path = format!(
                "/api/2.1/unity-catalog/tables?catalog_name={}&schema_name={}&omit_columns=true&max_results=1000",
                escape(&cat),
                escape(s)
            );
            match self.uc_list(&path, "tables", MAX_TABLES).await {
                Ok(t) => tables.extend(t),
                Err(_) => failed = true,
            }
            if tables.len() >= MAX_TABLES {
                break;
            }
        }
        if tables.is_empty() && failed {
            return Ok(out);
        }
        let label = |t: &Json| {
            format!(
                "{}.{}",
                t.get("schema_name").and_then(Json::as_str).unwrap_or_default(),
                t.get("name").and_then(Json::as_str).unwrap_or_default()
            )
        };
        let full = |t: &Json| {
            format!(
                "{}.{}.{}",
                bt(&cat),
                bt(t.get("schema_name").and_then(Json::as_str).unwrap_or_default()),
                bt(t.get("name").and_then(Json::as_str).unwrap_or_default())
            )
        };
        let format_of = |t: &Json| t.get("data_source_format").and_then(Json::as_str).unwrap_or_default().to_ascii_uppercase();
        let is_table = |t: &Json| matches!(t.get("table_type").and_then(Json::as_str), Some("MANAGED" | "EXTERNAL"));

        // Delta tables that keep deleted files long.
        let long: Vec<(&Json, f64)> = tables
            .iter()
            .filter(|t| is_table(t) && format_of(t) == "DELTA")
            .filter_map(|t| {
                let v = t.pointer("/properties/delta.deletedFileRetentionDuration").and_then(Json::as_str)?;
                interval_days(v).filter(|d| *d > RETENTION_DAYS).map(|d| (t, d))
            })
            .collect();
        if !long.is_empty() {
            out.push(
                HealthCheck::new("deleted_file_retention", "Costos", format!("{} tablas Delta guardan archivos borrados más de {RETENTION_DAYS:.0} días", long.len()), Severity::Info)
                    .detail("VACUUM no borra los archivos que reemplazó un cambio hasta que vence delta.deletedFileRetentionDuration (7 días por defecto): mientras tanto se pagan en el almacenamiento. Bajarla acorta también cuánto se puede volver atrás con time travel.")
                    .objects(long.iter().map(|(t, d)| format!("{} ({d:.0} días)", label(t))).collect())
                    .fix(
                        long.iter()
                            .take(MAX_OBJECTS)
                            .map(|(t, _)| format!("ALTER TABLE {} SET TBLPROPERTIES ({} = {});", full(t), lit("delta.deletedFileRetentionDuration"), lit("interval 7 days")))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
            );
        }

        // Tables that aren't Delta.
        let other: Vec<&Json> = tables
            .iter()
            .filter(|t| is_table(t))
            .filter(|t| matches!(format_of(t).as_str(), "CSV" | "JSON" | "PARQUET" | "AVRO" | "ORC" | "TEXT"))
            .collect();
        out.push({
            let mut c = HealthCheck::new(
                "non_delta",
                "Rendimiento",
                if other.is_empty() { "Todas las tablas son Delta (o Iceberg)".to_string() } else { format!("{} tablas que no son Delta", other.len()) },
                if other.is_empty() { Severity::Ok } else { Severity::Info },
            )
            .detail("Las tablas en CSV, JSON, Parquet u otros formatos no tienen transacciones, time travel, OPTIMIZE ni salto de archivos por estadísticas: cada consulta lee todo. Las de Parquet se pasan a Delta en el lugar con CONVERT TO DELTA; las demás, con CREATE TABLE … AS SELECT.")
            .objects(other.iter().map(|t| format!("{} ({})", label(t), format_of(t).to_lowercase())).collect());
            let parquet: Vec<String> = other.iter().filter(|t| format_of(t) == "PARQUET").take(MAX_OBJECTS).map(|t| format!("CONVERT TO DELTA {};", full(t))).collect();
            if !parquet.is_empty() {
                c = c.fix(parquet.join("\n"));
            }
            c
        });

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
    fn intervals_and_auto_stop() {
        assert_eq!(interval_days("interval 30 days"), Some(30.0));
        assert_eq!(interval_days("interval 1 weeks"), Some(7.0));
        assert_eq!(interval_days("168 hours"), Some(7.0));
        assert_eq!(interval_days("nonsense"), None);
        assert_eq!(auto_stop_severity(0), Severity::Warning);
        assert_eq!(auto_stop_severity(10), Severity::Ok);
        assert_eq!(auto_stop_severity(120), Severity::Info);
        assert_eq!(escape("a b/ç"), "a%20b%2F%C3%A7");
    }
}

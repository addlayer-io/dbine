//! "Chequeo de salud" findings of BigQuery ([`dbine_driver::Session::health_checks`]),
//! cost and maintenance of a dataset, all from the REST API (`datasets.get`,
//! `tables.list`, `tables.get`): metadata reads that are free, with no query
//! job and no data scanned.
//!
//! - large tables without partitioning or clustering (every query bills the
//!   whole table, on-demand);
//! - large partitioned tables that don't require a partition filter;
//! - time-partitioned tables without partition expiration;
//! - the storage billing model that would cost less (logical vs physical);
//! - a long time travel window under physical billing.
//!
//! Each check is its own reading: one that fails is skipped.

use crate::ddl::ident;
use crate::BigQuerySession;
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::Result;
use serde_json::Value as Json;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;
/// Tables read with `tables.get` at most (sizes aren't in `tables.list`).
const MAX_TABLES: usize = 500;
/// Concurrent `tables.get` calls.
const PARALLEL: usize = 8;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
/// From this size up, a query that reads the whole table costs.
const BIG: f64 = 10.0 * GB;

fn f(j: &Json, key: &str) -> f64 {
    match j.get(key) {
        Some(Json::String(s)) => s.parse().unwrap_or(0.0),
        Some(Json::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn size(bytes: f64) -> String {
    if bytes >= 1024.0 * GB {
        format!("{:.1} TB", bytes / 1024.0 / GB)
    } else {
        format!("{:.1} GB", bytes / GB)
    }
}

fn partitioned(t: &Json) -> bool {
    t.get("timePartitioning").is_some() || t.get("rangePartitioning").is_some()
}

/// Relative monthly storage cost of a table under each model (logical
/// active = 1; long-term storage is half; physical bytes cost twice,
/// time travel included in the active ones).
pub(crate) fn storage_costs(t: &Json) -> (f64, f64) {
    let logical = f(t, "numActiveLogicalBytes") + f(t, "numLongTermLogicalBytes") * 0.5;
    let physical = (f(t, "numActivePhysicalBytes") + f(t, "numLongTermPhysicalBytes") * 0.5) * 2.0;
    (logical, physical)
}

impl BigQuerySession {
    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let ds = if database.is_empty() { self.dataset.clone().unwrap_or_default() } else { database.to_string() };
        let dsq = ident(&ds);
        let mut out = Vec::new();
        let dataset = self.api.get(&["datasets", &ds], &[]).await.ok();

        // Every table's metadata (sizes come only from tables.get).
        let mut tables: Vec<Json> = Vec::new();
        if let Ok(list) = self.api.list_all(&["datasets", &ds, "tables"], "tables").await {
            let names: Vec<String> = list
                .iter()
                .filter(|t| t.get("type").and_then(Json::as_str) == Some("TABLE"))
                .filter_map(|t| t.pointer("/tableReference/tableId").and_then(Json::as_str).map(str::to_string))
                .take(MAX_TABLES)
                .collect();
            for chunk in names.chunks(PARALLEL) {
                let mut set = tokio::task::JoinSet::new();
                for name in chunk {
                    let (api, ds, name) = (self.api.clone(), ds.clone(), name.clone());
                    set.spawn(async move { api.get(&["datasets", &ds, "tables", &name], &[]).await });
                }
                while let Some(r) = set.join_next().await {
                    if let Ok(Ok(t)) = r {
                        tables.push(t);
                    }
                }
            }
        }
        let name = |t: &Json| t.pointer("/tableReference/tableId").and_then(Json::as_str).unwrap_or_default().to_string();
        let full = |t: &Json| format!("{dsq}.{}", ident(&name(t)));
        let mut by_size: Vec<&Json> = tables.iter().collect();
        by_size.sort_by(|a, b| f(b, "numBytes").total_cmp(&f(a, "numBytes")));

        if !tables.is_empty() {
            // Large tables neither partitioned nor clustered.
            let flat: Vec<&&Json> = by_size.iter().filter(|t| f(t, "numBytes") >= BIG && !partitioned(t) && t.get("clustering").is_none()).collect();
            out.push(
                HealthCheck::new(
                    "no_partition_cluster",
                    "Costos",
                    if flat.is_empty() { "Las tablas grandes están particionadas o agrupadas".to_string() } else { format!("{} tablas de más de 10 GB sin particiones ni clustering", flat.len()) },
                    if flat.is_empty() { Severity::Ok } else { Severity::Warning },
                )
                .detail("Con facturación on-demand cada consulta cobra los bytes de las columnas que lee en toda la tabla, aunque filtre. Particionar por fecha y agrupar (CLUSTER BY) por las columnas de filtro hace que lea solo lo necesario. Hay que recrear la tabla (CREATE TABLE … PARTITION BY … CLUSTER BY … AS SELECT, que sí se cobra una vez).")
                .objects(flat.iter().map(|t| format!("{} ({})", name(t), size(f(t, "numBytes")))).collect()),
            );

            // Large partitioned tables that let a query skip the partition filter.
            let open: Vec<&&Json> = by_size
                .iter()
                .filter(|t| f(t, "numBytes") >= BIG && partitioned(t))
                .filter(|t| t.get("requirePartitionFilter").and_then(Json::as_bool) != Some(true))
                .filter(|t| t.pointer("/timePartitioning/requirePartitionFilter").and_then(Json::as_bool) != Some(true))
                .collect();
            if !open.is_empty() {
                out.push(
                    HealthCheck::new("no_partition_filter", "Costos", format!("{} tablas particionadas grandes no exigen filtro de partición", open.len()), Severity::Info)
                        .detail("Una consulta sin filtro sobre la columna de partición lee (y cobra) todas las particiones. Exigir el filtro hace que esa consulta falle antes de costar; cambiarlo es solo metadata.")
                        .objects(open.iter().map(|t| format!("{} ({})", name(t), size(f(t, "numBytes")))).collect())
                        .fix(open.iter().take(MAX_OBJECTS).map(|t| format!("ALTER TABLE {} SET OPTIONS (require_partition_filter = TRUE);", full(t))).collect::<Vec<_>>().join("\n")),
                );
            }

            // Time-partitioned tables whose partitions never expire.
            let dataset_default = dataset.as_ref().is_some_and(|d| f(d, "defaultPartitionExpirationMs") > 0.0);
            if !dataset_default {
                let forever: Vec<&&Json> = by_size
                    .iter()
                    .filter(|t| f(t, "numBytes") >= GB && t.get("timePartitioning").is_some())
                    .filter(|t| t.pointer("/timePartitioning/expirationMs").is_none())
                    .collect();
                if !forever.is_empty() {
                    out.push(
                        HealthCheck::new("partition_expiration", "Costos", format!("{} tablas particionadas por fecha sin vencimiento de particiones", forever.len()), Severity::Info)
                            .detail("Si los datos viejos dejan de servir, un vencimiento de particiones los borra solo en vez de pagar su almacenamiento para siempre. Ignoralo si la tabla tiene que guardar todo el historial; los días son tu decisión.")
                            .objects(forever.iter().map(|t| format!("{} ({})", name(t), size(f(t, "numBytes")))).collect())
                            .fix(format!(
                                "-- Elegí cuántos días guardar antes de correrlo:\n{}",
                                forever.iter().take(MAX_OBJECTS).map(|t| format!("-- ALTER TABLE {} SET OPTIONS (partition_expiration_days = 365);", full(t))).collect::<Vec<_>>().join("\n")
                            )),
                    );
                }
            }
        }

        if let Some(d) = &dataset {
            let model = d.get("storageBillingModel").and_then(Json::as_str).unwrap_or("LOGICAL").to_ascii_uppercase();
            let physical_model = model == "PHYSICAL";

            // The billing model that would cost less.
            let (logical, physical) = tables.iter().map(storage_costs).fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
            let logical_bytes: f64 = tables.iter().map(|t| f(t, "numTotalLogicalBytes").max(f(t, "numBytes"))).sum();
            if logical_bytes >= 100.0 * GB && logical > 0.0 && physical > 0.0 {
                let (better, saving) = if physical_model { ("LOGICAL", 1.0 - logical / physical) } else { ("PHYSICAL", 1.0 - physical / logical) };
                if saving >= 0.3 {
                    out.push(
                        HealthCheck::new("storage_billing", "Costos", format!("Facturar el almacenamiento como {better} costaría cerca de {:.0} % menos", saving * 100.0), Severity::Info)
                            .detail(format!(
                                "El dataset factura el almacenamiento {model} ({}). La facturación física cobra los bytes comprimidos al doble de precio, más los de time travel; conviene cuando los datos comprimen más de 2 a 1. Es una estimación con los tamaños actuales; el modelo se puede cambiar una vez cada 14 días.",
                                size(logical_bytes)
                            ))
                            .fix(format!("ALTER SCHEMA {dsq} SET OPTIONS (storage_billing_model = '{better}');")),
                    );
                }
            }

            // Time travel is billed under physical storage.
            let hours = f(d, "maxTimeTravelHours");
            let hours = if hours > 0.0 { hours } else { 168.0 };
            if physical_model && hours > 48.0 {
                let tt: f64 = tables.iter().map(|t| f(t, "numTimeTravelPhysicalBytes")).sum();
                out.push(
                    HealthCheck::new("time_travel_window", "Costos", format!("Time travel de {hours:.0} horas con facturación física ({} hoy)", size(tt)), if tt >= 100.0 * GB { Severity::Warning } else { Severity::Info })
                        .detail("Con facturación física se pagan también los bytes que guarda el time travel (lo cambiado o borrado). Si no necesitás volver más de 2 días atrás, bajar la ventana a 48 horas reduce ese costo.")
                        .fix(format!("ALTER SCHEMA {dsq} SET OPTIONS (max_time_travel_hours = 48);")),
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
    use serde_json::json;

    #[test]
    fn storage_cost_by_model() {
        // Compresses 4 to 1: physical costs half.
        let t = json!({"numActiveLogicalBytes": "400", "numLongTermLogicalBytes": "0", "numActivePhysicalBytes": "100", "numLongTermPhysicalBytes": "0"});
        assert_eq!(storage_costs(&t), (400.0, 200.0));
        assert!(partitioned(&json!({"timePartitioning": {"type": "DAY"}})));
        assert!(!partitioned(&json!({"clustering": {}})));
        assert_eq!(f(&json!({"numBytes": 5}), "numBytes"), 5.0);
    }
}

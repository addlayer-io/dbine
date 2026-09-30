//! Monitoring DynamoDB with what the control plane gives for free:
//! `DescribeTable` (size, items, capacity mode, provisioned throughput,
//! indexes) and `DescribeLimits` (account quotas). CPU, memory and the
//! consumed capacity aren't in the data API: they live in CloudWatch.

use aws_sdk_dynamodb::types::TableDescription;
use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use serde_json::{json, Value as Json};

/// Tables described per snapshot (one `DescribeTable` each).
pub const MAX_TABLES: usize = 100;

/// Account quotas from `DescribeLimits`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Limits {
    pub account_read: Option<i64>,
    pub account_write: Option<i64>,
    pub table_read: Option<i64>,
    pub table_write: Option<i64>,
}

fn n(v: Option<i64>) -> Json {
    v.map_or(Json::Null, |x| json!(x))
}

/// On-demand (`PAY_PER_REQUEST`) or provisioned.
fn on_demand(d: &TableDescription) -> bool {
    d.billing_mode_summary()
        .and_then(|b| b.billing_mode())
        .is_some_and(|m| m.as_str() == "PAY_PER_REQUEST")
}

/// Provisioned (RCU, WCU) of a table and its GSIs; zero when on demand.
fn provisioned(d: &TableDescription) -> (i64, i64) {
    if on_demand(d) {
        return (0, 0);
    }
    let t = d.provisioned_throughput();
    let mut r = t.and_then(|p| p.read_capacity_units()).unwrap_or(0);
    let mut w = t.and_then(|p| p.write_capacity_units()).unwrap_or(0);
    for g in d.global_secondary_indexes() {
        let p = g.provisioned_throughput();
        r += p.and_then(|p| p.read_capacity_units()).unwrap_or(0);
        w += p.and_then(|p| p.write_capacity_units()).unwrap_or(0);
    }
    (r, w)
}

/// `total` tables in the account; `described` the ones described (at most
/// [`MAX_TABLES`]).
pub fn snapshot(region: &str, local: bool, total: usize, described: &[TableDescription], limits: Option<Limits>) -> MonitorSnapshot {
    let mut s = MonitorSnapshot::default();
    let size: i64 = described.iter().filter_map(|d| d.table_size_bytes()).sum();
    let items: i64 = described.iter().filter_map(|d| d.item_count()).sum();
    let (rcu, wcu) = described.iter().map(provisioned).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    let demand = described.iter().filter(|d| on_demand(d)).count();
    let gsis: usize = described.iter().map(|d| d.global_secondary_indexes().len()).sum();
    let busy = described
        .iter()
        .filter(|d| d.table_status().is_some_and(|st| st.as_str() != "ACTIVE"))
        .count();
    let l = limits.unwrap_or_default();
    let f = |v: i64| Some(v as f64);

    let m = &mut s.metrics;
    m.push(Metric::new("storage_used", "Tamaño de las tablas", "Almacenamiento", MetricUnit::Bytes, f(size)));
    m.push(Metric::new("items", "Ítems", "Almacenamiento", MetricUnit::Count, f(items)));
    m.push(Metric::new("tables", "Tablas", "Almacenamiento", MetricUnit::Count, Some(total as f64)));
    m.push(Metric::new("indexes", "Índices globales (GSI)", "Almacenamiento", MetricUnit::Count, Some(gsis as f64)));
    m.push(
        Metric::new("provisioned_read", "Lectura aprovisionada (RCU)", "Capacidad", MetricUnit::Count, f(rcu))
            .max(l.account_read.map(|v| v as f64)),
    );
    m.push(
        Metric::new("provisioned_write", "Escritura aprovisionada (WCU)", "Capacidad", MetricUnit::Count, f(wcu))
            .max(l.account_write.map(|v| v as f64)),
    );
    m.push(Metric::new("on_demand_tables", "Tablas a demanda", "Capacidad", MetricUnit::Count, Some(demand as f64)));
    m.push(Metric::new("tables_busy", "Tablas no activas (creando, actualizando…)", "Servidor", MetricUnit::Count, Some(busy as f64)));

    let mut rows: Vec<&TableDescription> = described.iter().collect();
    rows.sort_by_key(|d| std::cmp::Reverse(d.table_size_bytes().unwrap_or(0)));
    let mut t = MonitorTable::new(
        "top_objects",
        "Tablas",
        &["tabla", "estado", "modo", "ítems", "tamaño (bytes)", "RCU", "WCU", "máx. lecturas a demanda", "máx. escrituras a demanda", "GSI", "LSI", "clase", "stream", "réplicas"],
    );
    for d in rows.iter().take(200) {
        let pt = d.provisioned_throughput();
        let od = d.on_demand_throughput();
        t.rows.push(vec![
            json!(d.table_name().unwrap_or_default()),
            json!(d.table_status().map(|x| x.as_str()).unwrap_or_default()),
            json!(if on_demand(d) { "a demanda" } else { "aprovisionado" }),
            n(d.item_count()),
            n(d.table_size_bytes()),
            if on_demand(d) { Json::Null } else { n(pt.and_then(|p| p.read_capacity_units())) },
            if on_demand(d) { Json::Null } else { n(pt.and_then(|p| p.write_capacity_units())) },
            n(od.and_then(|o| o.max_read_request_units()).filter(|v| *v > 0)),
            n(od.and_then(|o| o.max_write_request_units()).filter(|v| *v > 0)),
            json!(d.global_secondary_indexes().len()),
            json!(d.local_secondary_indexes().len()),
            json!(d.table_class_summary().and_then(|c| c.table_class()).map(|c| c.as_str()).unwrap_or("STANDARD")),
            json!(d.stream_specification().is_some_and(|x| x.stream_enabled())),
            json!(d.replicas().iter().filter_map(|r| r.region_name()).collect::<Vec<_>>().join(", ")),
        ]);
    }
    s.tables.push(t);

    let mut ix = MonitorTable::new(
        "indexes",
        "Índices secundarios",
        &["tabla", "índice", "tipo", "estado", "ítems", "tamaño (bytes)", "RCU", "WCU"],
    );
    for d in &rows {
        let table = d.table_name().unwrap_or_default();
        for g in d.global_secondary_indexes() {
            let p = g.provisioned_throughput();
            ix.rows.push(vec![
                json!(table),
                json!(g.index_name().unwrap_or_default()),
                json!("GSI"),
                json!(g.index_status().map(|x| x.as_str()).unwrap_or_default()),
                n(g.item_count()),
                n(g.index_size_bytes()),
                if on_demand(d) { Json::Null } else { n(p.and_then(|p| p.read_capacity_units())) },
                if on_demand(d) { Json::Null } else { n(p.and_then(|p| p.write_capacity_units())) },
            ]);
        }
        for li in d.local_secondary_indexes() {
            ix.rows.push(vec![
                json!(table),
                json!(li.index_name().unwrap_or_default()),
                json!("LSI"),
                Json::Null,
                n(li.item_count()),
                n(li.index_size_bytes()),
                Json::Null,
                Json::Null,
            ]);
        }
    }
    ix.rows.truncate(200);
    s.tables.push(ix);

    s.info.push(("Región".into(), region.to_string()));
    if local {
        s.info.push(("Endpoint".into(), "personalizado (emulador o DynamoDB Local)".into()));
    }
    for (label, v) in [
        ("Límite de RCU de la cuenta", l.account_read),
        ("Límite de WCU de la cuenta", l.account_write),
        ("Límite de RCU por tabla", l.table_read),
        ("Límite de WCU por tabla", l.table_write),
    ] {
        if let Some(v) = v {
            s.info.push((label.into(), v.to_string()));
        }
    }

    s.notes.push("DynamoDB es un servicio administrado: la API no expone CPU, memoria, conexiones ni sesiones.".into());
    s.notes.push(
        "La capacidad consumida y los throttles están en CloudWatch (ConsumedRead/WriteCapacityUnits, ThrottledRequests), \
         que no se consulta: GetMetricData se cobra por métrica pedida y el monitor se actualiza cada pocos segundos."
            .into(),
    );
    s.notes.push("DynamoDB actualiza el tamaño y la cantidad de ítems de cada tabla aproximadamente cada seis horas.".into());
    if limits.is_none() {
        s.notes.push("No se pudieron leer los límites de la cuenta (DescribeLimits): falta el permiso dynamodb:DescribeLimits.".into());
    }
    if described.len() < total {
        s.notes.push(format!("Se describen las primeras {} de {total} tablas.", described.len()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_dynamodb::types::{
        BillingMode, BillingModeSummary, GlobalSecondaryIndexDescription, ProvisionedThroughputDescription, TableStatus,
    };

    fn table(name: &str, size: i64, items: i64, rcu: Option<i64>) -> TableDescription {
        let mut b = TableDescription::builder()
            .table_name(name)
            .table_status(TableStatus::Active)
            .table_size_bytes(size)
            .item_count(items);
        match rcu {
            Some(r) => {
                b = b
                    .provisioned_throughput(ProvisionedThroughputDescription::builder().read_capacity_units(r).write_capacity_units(r).build())
                    .global_secondary_indexes(
                        GlobalSecondaryIndexDescription::builder()
                            .index_name("gsi")
                            .item_count(3)
                            .provisioned_throughput(ProvisionedThroughputDescription::builder().read_capacity_units(1).write_capacity_units(2).build())
                            .build(),
                    );
            }
            None => {
                b = b.billing_mode_summary(BillingModeSummary::builder().billing_mode(BillingMode::PayPerRequest).build());
            }
        }
        b.build()
    }

    #[test]
    fn totals_and_tables() {
        let tables = vec![table("a", 100, 2, Some(5)), table("b", 900, 7, None)];
        let limits = Limits { account_read: Some(80000), account_write: Some(80000), table_read: Some(40000), table_write: Some(40000) };
        let s = snapshot("us-east-1", true, 2, &tables, Some(limits));
        let m = |k: &str| s.metrics.iter().find(|m| m.key == k).unwrap();
        assert_eq!(m("storage_used").value, Some(1000.0));
        assert_eq!(m("items").value, Some(9.0));
        assert_eq!(m("provisioned_read").value, Some(6.0));
        assert_eq!(m("provisioned_write").value, Some(7.0));
        assert_eq!(m("provisioned_read").max, Some(80000.0));
        assert_eq!(m("on_demand_tables").value, Some(1.0));
        assert_eq!(m("indexes").value, Some(1.0));
        let t = &s.tables[0];
        assert_eq!(t.rows[0][0], json!("b"));
        assert_eq!(t.rows[0][2], json!("a demanda"));
        assert_eq!(t.rows[1][5], json!(5));
        assert_eq!(s.tables[1].rows.len(), 1);
        assert!(s.info.iter().any(|(k, v)| k == "Límite de RCU por tabla" && v == "40000"));
    }

    #[test]
    fn notes_when_limits_fail_and_tables_are_capped() {
        let s = snapshot("eu-west-1", false, 150, &[table("a", 1, 1, None)], None);
        assert!(s.notes.iter().any(|n| n.contains("DescribeLimits")));
        assert!(s.notes.iter().any(|n| n.contains("1 de 150")));
    }
}

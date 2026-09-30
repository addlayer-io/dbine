//! Monitoring Cosmos DB with what the data-plane REST API reports: the
//! account (regions, consistency), the databases and containers, their
//! offers (RU/s, manual or autoscale), usage and quotas from the
//! `x-ms-resource-usage` / `x-ms-resource-quota` headers, and the
//! partition key ranges (physical partitions). CPU, memory, consumed RU/s
//! and throttling are Azure Monitor metrics, outside this API.

use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Containers whose usage and partitions are read per snapshot.
pub const MAX_CONTAINERS: usize = 25;

/// `documentsSize=12;documentsCount=3;…` as a map (sizes are in KB).
pub fn parse_usage(header: &str) -> BTreeMap<String, f64> {
    header
        .split(';')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

/// What was read about one container.
#[derive(Debug, Default, Clone)]
pub struct ContainerStats {
    pub usage: Option<BTreeMap<String, f64>>,
    pub ranges: Option<Vec<Value>>,
}

/// Throughput of an offer: (RU/s, autoscale).
fn throughput(offer: &Value) -> Option<(f64, bool)> {
    let c = offer.get("content")?;
    if let Some(max) = c.get("offerAutopilotSettings").and_then(|a| a.get("maxThroughput")).and_then(Value::as_f64) {
        return Some((max, true));
    }
    c.get("offerThroughput").and_then(Value::as_f64).map(|t| (t, false))
}

pub struct Inputs<'a> {
    pub account: Option<&'a Value>,
    pub database: &'a str,
    pub database_rid: Option<&'a str>,
    pub databases: usize,
    pub containers: &'a [Value],
    /// Same order as `containers` (possibly shorter: only the first ones).
    pub stats: &'a [ContainerStats],
    /// `None` when `/offers` failed (serverless accounts have none).
    pub offers: Option<&'a [Value]>,
    /// Account-level usage and quota headers (of the `/dbs` read).
    pub account_usage: Option<BTreeMap<String, f64>>,
    pub account_quota: Option<BTreeMap<String, f64>>,
    /// RUs spent by this snapshot's requests.
    pub charge: f64,
}

pub fn snapshot(i: &Inputs) -> MonitorSnapshot {
    let mut s = MonitorSnapshot::default();
    let kb = |u: &BTreeMap<String, f64>, k: &str| u.get(k).map(|v| v * 1024.0);
    let offers = i.offers.unwrap_or(&[]);
    let rid = |v: &Value| v.get("_rid").and_then(Value::as_str).map(str::to_string);
    let mut used_offers: Vec<String> = Vec::new();
    let offer_for = |r: &Option<String>| -> Option<&Value> {
        let r = r.as_deref()?;
        offers.iter().find(|o| o.get("offerResourceId").and_then(Value::as_str) == Some(r))
    };

    let mut total_ru = 0.0;
    let mut any_ru = false;
    let mut rows: Vec<(f64, Vec<Value>)> = Vec::new();
    let mut ranges = MonitorTable::new(
        "partitions",
        "Particiones físicas (rangos de clave)",
        &["contenedor", "rango", "desde", "hasta", "estado", "fracción del rendimiento"],
    );
    let (mut docs_total, mut data_total, mut index_total, mut parts_total) = (None::<f64>, None::<f64>, None::<f64>, None::<f64>);
    let add = |acc: &mut Option<f64>, v: Option<f64>| {
        if let Some(v) = v {
            *acc = Some(acc.unwrap_or(0.0) + v);
        }
    };
    for (n, c) in i.containers.iter().enumerate() {
        let name = c.get("id").and_then(Value::as_str).unwrap_or_default();
        let st = i.stats.get(n).cloned().unwrap_or_default();
        let offer = offer_for(&rid(c));
        let tp = offer.and_then(throughput);
        if let Some(id) = offer.and_then(|o| o.get("id")).and_then(Value::as_str) {
            used_offers.push(id.to_string());
        }
        if let Some((ru, _)) = tp {
            total_ru += ru;
            any_ru = true;
        }
        let docs = st.usage.as_ref().and_then(|u| u.get("documentsCount").copied());
        let data = st.usage.as_ref().and_then(|u| kb(u, "documentsSize"));
        let index = st
            .usage
            .as_ref()
            .and_then(|u| Some((kb(u, "collectionSize")? - kb(u, "documentsSize").unwrap_or(0.0)).max(0.0)));
        let parts = st.ranges.as_ref().map(|r| r.len() as f64);
        add(&mut docs_total, docs);
        add(&mut data_total, data);
        add(&mut index_total, index);
        add(&mut parts_total, parts);
        let pk = c
            .get("partitionKey")
            .and_then(|p| p.get("paths"))
            .and_then(Value::as_array)
            .map(|p| p.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        let mode = match tp {
            Some((_, true)) => "autoescalado",
            Some((_, false)) => "manual",
            None if i.offers.is_some() => "compartido o serverless",
            None => "",
        };
        let opt = |v: Option<f64>| v.map_or(Value::Null, |x| json!(x));
        rows.push((
            data.unwrap_or(0.0),
            vec![
                json!(name),
                opt(docs),
                opt(data),
                opt(index),
                opt(tp.map(|t| t.0)),
                json!(mode),
                opt(parts),
                json!(pk),
                c.get("defaultTtl").cloned().unwrap_or(Value::Null),
            ],
        ));
        for r in st.ranges.iter().flatten() {
            if ranges.rows.len() >= 200 {
                break;
            }
            let g = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
            ranges.rows.push(vec![json!(name), g("id"), g("minInclusive"), g("maxExclusive"), g("status"), g("throughputFraction")]);
        }
    }

    // Shared (database-level) throughput.
    let db_offer = i.database_rid.and_then(|r| {
        offers.iter().find(|o| {
            o.get("offerResourceId").and_then(Value::as_str) == Some(r)
                && !o.get("id").and_then(Value::as_str).is_some_and(|id| used_offers.iter().any(|u| u == id))
        })
    });
    let db_tp = db_offer.and_then(throughput);
    if let Some((ru, _)) = db_tp {
        total_ru += ru;
        any_ru = true;
    }

    let described = i.stats.len().min(i.containers.len());
    let m = &mut s.metrics;
    m.push(Metric::new("storage_used", "Datos", "Almacenamiento", MetricUnit::Bytes, data_total));
    m.push(Metric::new("index_size", "Índices", "Almacenamiento", MetricUnit::Bytes, index_total));
    m.push(Metric::new("items", "Documentos", "Almacenamiento", MetricUnit::Count, docs_total));
    let quota = |k: &str| i.account_quota.as_ref().and_then(|q| q.get(k).copied()).filter(|v| *v > 0.0);
    let usage = |k: &str| i.account_usage.as_ref().and_then(|u| u.get(k).copied());
    m.push(
        Metric::new("containers", "Contenedores", "Almacenamiento", MetricUnit::Count, Some(i.containers.len() as f64))
            .max(quota("collections")),
    );
    m.push(
        Metric::new("databases", "Bases de datos", "Almacenamiento", MetricUnit::Count, usage("databases").or(Some(i.databases as f64)))
            .max(quota("databases")),
    );
    m.push(Metric::new("partitions", "Particiones físicas", "Almacenamiento", MetricUnit::Count, parts_total));
    m.push(Metric::new(
        "throughput",
        "Rendimiento aprovisionado (RU/s)",
        "Capacidad",
        MetricUnit::Count,
        any_ru.then_some(total_ru),
    ));
    m.push(Metric::new("monitor_charge", "RU de esta lectura del monitor", "Capacidad", MetricUnit::Count, Some(i.charge)));

    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut t = MonitorTable::new(
        "top_objects",
        &format!("Contenedores de «{}»", i.database),
        &["contenedor", "documentos", "datos (bytes)", "índices (bytes)", "RU/s", "rendimiento", "particiones", "clave de partición", "TTL (s)"],
    );
    t.rows = rows.into_iter().take(200).map(|r| r.1).collect();
    s.tables.push(t);
    if !ranges.rows.is_empty() {
        s.tables.push(ranges);
    }

    // Info.
    let mut info = |label: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            s.info.push((label.to_string(), v));
        }
    };
    let a = i.account;
    let names = |k: &str| {
        a.and_then(|a| a.get(k)).and_then(Value::as_array).map(|l| {
            l.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", ")
        })
    };
    info("Cuenta", a.and_then(|a| a.get("id")).and_then(Value::as_str).map(str::to_string));
    info("Regiones de escritura", names("writableLocations"));
    info("Regiones de lectura", names("readableLocations"));
    info(
        "Consistencia",
        a.and_then(|a| a.pointer("/userConsistencyPolicy/defaultConsistencyLevel")).and_then(Value::as_str).map(str::to_string),
    );
    info(
        "Escritura multirregión",
        a.and_then(|a| a.get("enableMultipleWriteLocations")).and_then(Value::as_bool).map(|b| if b { "sí" } else { "no" }.to_string()),
    );
    info("Base de datos", Some(i.database.to_string()));
    info(
        "Rendimiento compartido de la base",
        db_tp.map(|(ru, auto)| format!("{ru} RU/s{}", if auto { " (autoescalado, máximo)" } else { "" })),
    );
    if let Some(q) = &i.account_quota {
        for (k, label) in [("collections", "Contenedores máximos"), ("databases", "Bases máximas")] {
            info(label, q.get(k).filter(|v| **v > 0.0).map(|v| v.to_string()));
        }
    }

    s.notes.push(
        "Cosmos DB es un servicio administrado: CPU, memoria, RU/s consumidas, throttling (429) y latencias están en Azure Monitor, no en la API de datos."
            .into(),
    );
    s.notes.push(
        "El uso de almacenamiento (x-ms-resource-usage) lo actualiza el servicio cada pocos minutos; el monitor lo relee cada minuto."
            .into(),
    );
    if i.offers.is_none() {
        s.notes.push("No se pudieron leer las ofertas (/offers): en cuentas serverless no hay rendimiento aprovisionado.".into());
    }
    if described < i.containers.len() {
        s.notes.push(format!("Se leen el uso y las particiones de los primeros {described} de {} contenedores.", i.containers.len()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_header() {
        let u = parse_usage("databases=1;collections=2;documentsSize=12;documentsCount=-1;bad;x=y");
        assert_eq!(u["collections"], 2.0);
        assert_eq!(u["documentsCount"], -1.0);
        assert!(!u.contains_key("x"));
    }

    #[test]
    fn containers_offers_and_ranges() {
        let containers = vec![
            json!({"id": "a", "_rid": "rA", "partitionKey": {"paths": ["/k"]}}),
            json!({"id": "b", "_rid": "rB", "partitionKey": {"paths": ["/p"]}, "defaultTtl": 60}),
        ];
        let stats = vec![
            ContainerStats {
                usage: Some(parse_usage("documentsSize=2;collectionSize=3;documentsCount=5")),
                ranges: Some(vec![json!({"id": "0", "minInclusive": "", "maxExclusive": "FF", "status": "online"})]),
            },
            ContainerStats {
                usage: Some(parse_usage("documentsSize=10;collectionSize=10;documentsCount=1")),
                ranges: Some(vec![json!({"id": "0"}), json!({"id": "1"})]),
            },
        ];
        let offers = vec![
            json!({"id": "o1", "offerResourceId": "rA", "content": {"offerThroughput": 400}}),
            json!({"id": "o2", "offerResourceId": "rB", "content": {"offerThroughput": 400, "offerAutopilotSettings": {"maxThroughput": 4000}}}),
            json!({"id": "o3", "offerResourceId": "rDB", "content": {"offerThroughput": 1000}}),
        ];
        let account = json!({"id": "acct", "writableLocations": [{"name": "East US"}], "userConsistencyPolicy": {"defaultConsistencyLevel": "Session"}});
        let s = snapshot(&Inputs {
            account: Some(&account),
            database: "db",
            database_rid: Some("rDB"),
            databases: 1,
            containers: &containers,
            stats: &stats,
            offers: Some(&offers),
            account_usage: Some(parse_usage("databases=3")),
            account_quota: Some(parse_usage("collections=50;databases=100")),
            charge: 2.5,
        });
        let m = |k: &str| s.metrics.iter().find(|m| m.key == k).unwrap();
        assert_eq!(m("storage_used").value, Some(12.0 * 1024.0));
        assert_eq!(m("index_size").value, Some(1024.0));
        assert_eq!(m("items").value, Some(6.0));
        assert_eq!(m("partitions").value, Some(3.0));
        assert_eq!(m("throughput").value, Some(5400.0));
        assert_eq!(m("databases").value, Some(3.0));
        assert_eq!(m("containers").max, Some(50.0));
        let t = &s.tables[0];
        assert_eq!(t.rows[0][0], json!("b"));
        assert_eq!(t.rows[0][4], json!(4000.0));
        assert_eq!(t.rows[0][5], json!("autoescalado"));
        assert_eq!(s.tables[1].rows.len(), 3);
        assert!(s.info.iter().any(|(k, v)| k == "Consistencia" && v == "Session"));
        assert!(s.info.iter().any(|(k, v)| k == "Rendimiento compartido de la base" && v == "1000 RU/s"));
    }
}

//! `Session::monitor` for Phoenix. The Query Server (Avatica) reports
//! nothing about itself; the figures come from HBase's JMX servlets (the
//! Master's and each RegionServer's web UI, `/jmx`): CPU, heap, regions,
//! requests, block cache and store sizes. Phoenix adds its table count and
//! the sizes its statistics collected (`SYSTEM.STATS`).

use crate::{text, PhoenixSession};
use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// HBase's default info ports.
const MASTER_PORT: u16 = 16010;
const RS_PORT: u16 = 16030;
const JMX_TIMEOUT: Duration = Duration::from_secs(3);

/// Where the HBase web UIs are, found on the first snapshot.
#[derive(Default, Clone)]
pub(crate) struct HBaseUis {
    /// The configured or guessed Master UI (`http://host:16010`); `None`
    /// once a guess failed, so it isn't retried every few seconds.
    pub(crate) master: Option<String>,
    /// Configured RegionServer UIs; empty = the Master's list of live
    /// servers on port 16030.
    pub(crate) regionservers: Vec<String>,
    pub(crate) guessed: bool,
}

impl HBaseUis {
    pub(crate) fn from_config(host: &str, master: Option<&str>, regionservers: Option<&str>) -> Self {
        let trim = |u: &str| u.trim().trim_end_matches('/').to_string();
        let master_cfg = master.map(trim).filter(|m| !m.is_empty());
        Self {
            guessed: master_cfg.is_none(),
            master: Some(master_cfg.unwrap_or_else(|| format!("http://{}:{MASTER_PORT}", if host.is_empty() { "localhost" } else { host }))),
            regionservers: regionservers.unwrap_or("").split(',').map(trim).filter(|u| !u.is_empty()).collect(),
        }
    }
}

/// One bean of a `/jmx?qry=` answer.
pub(crate) fn bean(v: &Value) -> Option<&Value> {
    v.get("beans")?.as_array()?.first()
}

fn f(b: Option<&Value>, k: &str) -> Option<f64> {
    b?.get(k)?.as_f64()
}

/// A `/jmx` answer; Hadoop writes NaN and Infinity, which JSON doesn't have.
pub(crate) fn parse_jmx(text: &str) -> Option<Value> {
    let fixed = text.replace(": NaN", ": null").replace(": -Infinity", ": null").replace(": Infinity", ": null");
    serde_json::from_str(&fixed).ok()
}

/// `Namespace_<ns>_table_<name>_metric_<metric>` of the RegionServer's
/// `sub=Tables` bean → (`ns:name` or `name`, metric).
pub(crate) fn table_metric(key: &str) -> Option<(String, &str)> {
    let rest = key.strip_prefix("Namespace_")?;
    let (ns, rest) = rest.split_once("_table_")?;
    let (table, metric) = rest.rsplit_once("_metric_")?;
    Some((if ns == "default" { table.to_string() } else { format!("{ns}:{table}") }, metric))
}

/// `host,16020,1790461188933;other,16020,…` → host names.
pub(crate) fn live_servers(tag: &str) -> Vec<String> {
    tag.split(';').filter_map(|s| s.split(',').next()).map(str::trim).filter(|h| !h.is_empty()).map(str::to_string).collect()
}

/// A RegionServer's figures.
#[derive(Default, Debug, Clone)]
struct Rs {
    host: String,
    cpu: Option<f64>,
    heap: Option<f64>,
    heap_max: Option<f64>,
    s: HashMap<&'static str, f64>,
    /// Per table: (size, read requests, write requests).
    tables: HashMap<String, (f64, f64, f64)>,
}

const RS_KEYS: [&str; 12] = [
    "regionCount",
    "storeCount",
    "storeFileSize",
    "memStoreSize",
    "totalRequestCount",
    "readRequestCount",
    "writeRequestCount",
    "blockCacheSize",
    "blockCacheFreeSize",
    "blockCacheHitCount",
    "blockCacheMissCount",
    "blockedRequestCount",
];

impl PhoenixSession {
    async fn jmx(&self, base: &str, qry: &str) -> Option<Value> {
        let r = self.client.http.get(format!("{base}/jmx?qry={qry}")).timeout(JMX_TIMEOUT).send().await.ok()?;
        parse_jmx(&r.error_for_status().ok()?.text().await.ok()?)
    }

    /// CPU and heap of a JVM behind a web UI.
    async fn jvm(&self, base: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
        let os = self.jmx(base, "java.lang:type=OperatingSystem").await;
        let mem = self.jmx(base, "java.lang:type=Memory").await;
        let cpu = f(os.as_ref().and_then(bean), "SystemCpuLoad").filter(|c| *c >= 0.0).map(|c| c * 100.0);
        let heap = mem.as_ref().and_then(bean).and_then(|b| b.get("HeapMemoryUsage")).cloned();
        let hf = |k: &str| heap.as_ref().and_then(|h| h.get(k)).and_then(Value::as_f64);
        (cpu, hf("used"), hf("max").filter(|m| *m > 0.0))
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = MonitorSnapshot::default();
        let mut m: HashMap<&'static str, f64> = HashMap::new();
        if let Ok(v) = dbine_driver::Session::server_version(self).await {
            snap.info.push(("Versión".into(), v));
        }
        snap.info.push(("Query Server".into(), self.client.url.trim_end_matches('/').to_string()));

        // Phoenix's own catalog and statistics.
        match self
            .rows(
                "SELECT COUNT(DISTINCT TABLE_NAME) FROM SYSTEM.CATALOG
                 WHERE TENANT_ID IS NULL AND COLUMN_NAME IS NULL AND COLUMN_FAMILY IS NULL AND TABLE_TYPE = 'u'",
            )
            .await
        {
            Ok(r) => {
                if let Some(n) = r.first().and_then(|r| r.first()).and_then(|v| num(&text(v))) {
                    m.insert("tables", n);
                }
            }
            Err(e) => snap.notes.push(format!("No se pudo leer SYSTEM.CATALOG: {e}")),
        }
        match self
            .rows(
                "SELECT PHYSICAL_NAME, SUM(GUIDE_POSTS_WIDTH), SUM(GUIDE_POSTS_ROW_COUNT), MAX(LAST_STATS_UPDATE_TIME)
                 FROM SYSTEM.STATS GROUP BY PHYSICAL_NAME ORDER BY SUM(GUIDE_POSTS_WIDTH) DESC LIMIT 20",
            )
            .await
        {
            Ok(rows) => {
                let mut t = MonitorTable::new(
                    "phoenix_stats",
                    "Tamaños estimados por las estadísticas de Phoenix",
                    &["Tabla física", "Bytes estimados", "Filas estimadas", "Estadísticas del"],
                );
                for r in rows.iter().filter(|r| r.len() == 4) {
                    t.rows.push(vec![r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone()]);
                }
                if !t.rows.is_empty() {
                    snap.tables.push(t);
                }
            }
            Err(e) => snap.notes.push(format!("No se pudo leer SYSTEM.STATS: {e}")),
        }

        // HBase: the Master.
        let master_url = self.hbase.master.clone();
        let master = match &master_url {
            Some(u) => self.jmx(u, "Hadoop:service=HBase,name=Master,sub=Server").await,
            None => None,
        };
        let mb = master.as_ref().and_then(bean);
        if let (Some(u), None) = (&master_url, mb) {
            snap.notes.push(format!(
                "No se pudo leer el JMX del Master de HBase en {u}: indicá la URL de su interfaz web en la conexión (campo «Master de HBase») para ver CPU, memoria, regiones y pedidos."
            ));
            if self.hbase.guessed {
                // A guess that failed isn't retried every few seconds.
                self.hbase.master = None;
            }
        } else if master_url.is_none() {
            snap.notes.push(
                "Sin la interfaz web del Master de HBase no hay CPU, memoria, regiones ni pedidos; indicala en la conexión (campo «Master de HBase»)."
                    .into(),
            );
        }
        let mut servers: Vec<String> = self.hbase.regionservers.clone();
        if let Some(b) = mb {
            let tag = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            for (label, k) in [("Cluster de HBase", "tag.clusterId"), ("Master activo", "tag.Hostname"), ("ZooKeeper", "tag.zookeeperQuorum")] {
                if !tag(k).is_empty() {
                    snap.info.push((label.into(), tag(k)));
                }
            }
            for (k, j) in [("rs", "numRegionServers"), ("rs_dead", "numDeadRegionServers"), ("avg_load", "averageLoad")] {
                if let Some(v) = f(Some(b), j) {
                    m.insert(k, v);
                }
            }
            if let Some(start) = f(Some(b), "masterStartTime") {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
                m.insert("uptime", ((now - start) / 1000.0).max(0.0));
            }
            if let Some(v) = f(Some(b), "clusterRequests") {
                m.insert("requests", v);
            }
            if servers.is_empty() {
                let hosts = live_servers(&tag("tag.liveRegionServers"));
                // The all-in-one layout: the RegionServer's host name may only
                // resolve inside the cluster, but it's the Master's machine.
                let master_host = master_url.as_deref().and_then(|u| reqwest::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string));
                servers = hosts
                    .iter()
                    .map(|h| match (&master_host, hosts.len()) {
                        (Some(mh), 1) if *h == tag("tag.Hostname") => format!("http://{mh}:{RS_PORT}"),
                        _ => format!("http://{h}:{RS_PORT}"),
                    })
                    .collect();
            }
            let (cpu, heap, heap_max) = self.jvm(master_url.as_deref().unwrap_or("")).await;
            if let Some(c) = cpu {
                m.insert("cpu", c);
            }
            if let Some(h) = heap {
                m.insert("master_heap", h);
            }
            if let Some(h) = heap_max {
                m.insert("master_heap_max", h);
            }
        }

        // HBase: each RegionServer.
        let mut rss = Vec::new();
        for base in servers.iter().take(50) {
            let Some(v) = self.jmx(base, "Hadoop:service=HBase,name=RegionServer,sub=Server").await else {
                rss.push(Rs { host: base.clone(), ..Default::default() });
                continue;
            };
            let b = bean(&v);
            let (cpu, heap, heap_max) = self.jvm(base).await;
            let mut rs = Rs {
                host: b.and_then(|b| b.get("tag.Hostname")).and_then(Value::as_str).unwrap_or(base).to_string(),
                cpu,
                heap,
                heap_max,
                ..Default::default()
            };
            for k in RS_KEYS {
                if let Some(x) = f(b, k) {
                    rs.s.insert(k, x);
                }
            }
            if let Some(obj) = self.jmx(base, "Hadoop:service=HBase,name=RegionServer,sub=Tables").await.as_ref().and_then(bean).and_then(Value::as_object) {
                for (k, v) in obj {
                    let (Some((table, metric)), Some(v)) = (table_metric(k), v.as_f64()) else { continue };
                    let e = rs.tables.entry(table).or_default();
                    match metric {
                        "tableSize" => e.0 += v,
                        "readRequestCount" => e.1 += v,
                        "writeRequestCount" => e.2 += v,
                        _ => {}
                    }
                }
            }
            rss.push(rs);
        }
        let reached: Vec<&Rs> = rss.iter().filter(|r| !r.s.is_empty()).collect();
        if !servers.is_empty() && reached.len() < servers.len() {
            snap.notes.push(format!(
                "No se pudo leer el JMX de {} de {} RegionServers; si sus nombres no resuelven desde esta máquina, indicá sus URLs en la conexión (campo «RegionServers de HBase»).",
                servers.len() - reached.len(),
                servers.len()
            ));
        }
        let sum = |k: &str| (!reached.is_empty()).then(|| reached.iter().filter_map(|r| r.s.get(k)).fold(0.0, |a, b| a + b));
        let heap: Option<f64> = (!reached.is_empty()).then(|| reached.iter().filter_map(|r| r.heap).fold(0.0, |a, b| a + b));
        let heap_max: Option<f64> = (!reached.is_empty()).then(|| reached.iter().filter_map(|r| r.heap_max).fold(0.0, |a, b| a + b));
        let (hits, misses) = (sum("blockCacheHitCount"), sum("blockCacheMissCount"));
        let hit_pct = match (hits, misses) {
            (Some(h), Some(mi)) if h + mi > 0.0 => Some((h * 1000.0 / (h + mi)).round() / 10.0),
            _ => None,
        };
        if !m.contains_key("cpu") {
            let cpus: Vec<f64> = reached.iter().filter_map(|r| r.cpu).collect();
            if !cpus.is_empty() {
                m.insert("cpu", cpus.iter().sum::<f64>() / cpus.len() as f64);
            }
        }
        if !reached.is_empty() {
            let mut t = MonitorTable::new(
                "nodes",
                "RegionServers",
                &["Servidor", "CPU (%)", "Heap usado", "Regiones", "Stores", "Pedidos", "Lecturas", "Escrituras", "Archivos (bytes)", "Memstore (bytes)", "Aciertos de caché (%)"],
            );
            for r in rss.iter().take(200) {
                let g = |k: &str| r.s.get(k).map_or(Value::Null, |v| json!(v));
                let hit = match (r.s.get("blockCacheHitCount"), r.s.get("blockCacheMissCount")) {
                    (Some(h), Some(mi)) if h + mi > 0.0 => json!((h * 1000.0 / (h + mi)).round() / 10.0),
                    _ => Value::Null,
                };
                t.rows.push(vec![
                    json!(r.host),
                    r.cpu.map_or(Value::Null, |c| json!((c * 10.0).round() / 10.0)),
                    r.heap.map_or(Value::Null, |h| json!(h)),
                    g("regionCount"),
                    g("storeCount"),
                    g("totalRequestCount"),
                    g("readRequestCount"),
                    g("writeRequestCount"),
                    g("storeFileSize"),
                    g("memStoreSize"),
                    hit,
                ]);
            }
            snap.tables.push(t);
        }

        // Table sizes as HBase measures them.
        let mut sizes: HashMap<&str, (f64, f64, f64)> = HashMap::new();
        for r in &reached {
            for (t, (size, rd, wr)) in &r.tables {
                if t.starts_with("hbase:") {
                    continue;
                }
                let e = sizes.entry(t.as_str()).or_default();
                e.0 += size;
                e.1 += rd;
                e.2 += wr;
            }
        }
        if !sizes.is_empty() {
            let mut sorted: Vec<_> = sizes.into_iter().collect();
            sorted.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0).then(a.0.cmp(b.0)));
            let mut t = MonitorTable::new("top_objects", "Tablas más grandes (HBase)", &["Tabla", "Tamaño (bytes)", "Lecturas", "Escrituras"]);
            for (name, (size, rd, wr)) in sorted.into_iter().take(20) {
                t.rows.push(vec![json!(name), json!(size), json!(rd), json!(wr)]);
            }
            snap.tables.insert(0, t);
        }

        let g = |k: &str| m.get(k).copied();
        let cache_size = sum("blockCacheSize");
        let cache_max = match (cache_size, sum("blockCacheFreeSize")) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        snap.metrics = vec![
            Metric::new("cpu", "CPU del servidor", "CPU", MetricUnit::Percent, g("cpu").map(|c| (c * 10.0).round() / 10.0)).max(Some(100.0)),
            Metric::new("mem_used", "Heap de los RegionServers", "Memoria", MetricUnit::Bytes, heap.filter(|h| *h > 0.0).or(g("master_heap")))
                .max(heap_max.filter(|h| *h > 0.0).or(g("master_heap_max"))),
            Metric::new("mem_cache", "Block cache", "Memoria", MetricUnit::Bytes, cache_size).max(cache_max),
            Metric::new("memstore", "Memstore", "Memoria", MetricUnit::Bytes, sum("memStoreSize")),
            Metric::new("queries", "Pedidos", "Actividad", MetricUnit::Count, sum("totalRequestCount").or(g("requests"))).counter(),
            Metric::new("rows_read", "Pedidos de lectura", "Actividad", MetricUnit::Count, sum("readRequestCount")).counter(),
            Metric::new("rows_written", "Pedidos de escritura", "Actividad", MetricUnit::Count, sum("writeRequestCount")).counter(),
            Metric::new("blocked_requests", "Pedidos bloqueados", "Bloqueos", MetricUnit::Count, sum("blockedRequestCount")).counter(),
            Metric::new("cache_hit", "Aciertos del block cache", "Caché", MetricUnit::Percent, hit_pct).max(Some(100.0)),
            Metric::new("storage_used", "Archivos de datos (HFiles)", "Almacenamiento", MetricUnit::Bytes, sum("storeFileSize")),
            Metric::new("regions", "Regiones", "Cluster", MetricUnit::Count, sum("regionCount")),
            Metric::new("region_servers", "RegionServers vivos", "Cluster", MetricUnit::Count, g("rs")),
            Metric::new("dead_region_servers", "RegionServers caídos", "Cluster", MetricUnit::Count, g("rs_dead")),
            Metric::new("tables", "Tablas de Phoenix", "Almacenamiento", MetricUnit::Count, g("tables")),
            Metric::new("uptime", "Tiempo activo del Master", "Servidor", MetricUnit::Seconds, g("uptime").map(f64::round)),
        ];
        snap.notes.push("El Query Server de Phoenix no informa sesiones ni consultas en curso: Avatica no expone esa información.".into());
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hbase_addresses() {
        assert_eq!(live_servers("a,16020,1;b,16020,2"), vec!["a", "b"]);
        assert!(live_servers("").is_empty());
        let u = HBaseUis::from_config("db1", None, None);
        assert_eq!(u.master.as_deref(), Some("http://db1:16010"));
        assert!(u.guessed && u.regionservers.is_empty());
        let u = HBaseUis::from_config("db1", Some("https://m:16010/ "), Some("http://r1:16030, http://r2:16030/"));
        assert_eq!(u.master.as_deref(), Some("https://m:16010"));
        assert_eq!(u.regionservers, vec!["http://r1:16030", "http://r2:16030"]);
        assert!(!u.guessed);
        let v = parse_jmx("{\"beans\" : [ {\"numRegionServers\" : 3, \"l1CacheHitRatio\" : NaN, \"x\" : -Infinity} ]}").unwrap();
        assert_eq!(f(bean(&v), "numRegionServers"), Some(3.0));
        assert_eq!(table_metric("Namespace_default_table_MY_T_metric_tableSize"), Some(("MY_T".into(), "tableSize")));
        assert_eq!(table_metric("Namespace_SYSTEM_table_CATALOG_metric_readRequestCount"), Some(("SYSTEM:CATALOG".into(), "readRequestCount")));
        assert_eq!(table_metric("numRegionServers"), None);
    }
}

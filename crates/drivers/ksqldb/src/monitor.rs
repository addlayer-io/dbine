//! `Session::monitor` for ksqlDB, from its REST API: `/info` and
//! `/healthcheck` (server and dependencies), `/clusterStatus` (the servers
//! of the cluster, when heartbeats are on), `SHOW QUERIES EXTENDED` (each
//! persistent query's state, errors and the offsets its tasks consume) and
//! `DESCRIBE … EXTENDED` of each stream and table (messages per second,
//! totals and failures, per server).

use crate::{check, http_error, text, KsqlSession, PROCESSING_LOG};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Sources described per snapshot (one request each).
const MAX_SOURCES: usize = 30;
const MAX_SQL: usize = 2000;

fn truncate(s: &str) -> String {
    if s.chars().count() > MAX_SQL {
        s.chars().take(MAX_SQL).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

/// A query's consumer lag: end offset minus committed offset of each
/// partition its tasks read (nothing committed yet = all of it).
pub(crate) fn query_lag(q: &Value) -> f64 {
    q.get("tasksMetadata")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("topicOffsets").and_then(Value::as_array))
        .flatten()
        .filter(|o| !o.pointer("/topicPartitionEntity/topic").and_then(Value::as_str).unwrap_or("").contains("-repartition"))
        .map(|o| {
            let end = o.get("endOffset").and_then(Value::as_f64).unwrap_or(0.0);
            let committed = o.get("committedOffset").and_then(Value::as_f64).unwrap_or(-1.0);
            (end - committed.max(0.0)).max(0.0)
        })
        .sum::<f64>()
        + 0.0
}

/// `clusterStatistics` / `clusterErrorStats` summed by metric name across
/// the servers.
pub(crate) fn source_stats(d: &Value) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for key in ["clusterStatistics", "clusterErrorStats"] {
        for s in d.get(key).and_then(Value::as_array).into_iter().flatten() {
            if let (Some(n), Some(v)) = (s.get("name").and_then(Value::as_str), s.get("value").and_then(Value::as_f64)) {
                *out.entry(n.to_string()).or_insert(0.0) += v;
            }
        }
    }
    out
}

impl KsqlSession {
    async fn get_json(&self, path: &str) -> Result<Value> {
        let rb = self.conn.auth(self.conn.http.get(format!("{}{path}", self.conn.base)));
        let resp = check(rb.send().await.map_err(http_error)?).await?;
        resp.json().await.map_err(dbine_driver::Error::query)
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = MonitorSnapshot::default();
        let info = self.get_json("/info").await?;
        let si = |k: &str| info.pointer(&format!("/KsqlServerInfo/{k}")).map(text).filter(|s| !s.is_empty());
        for (label, k) in [("Versión", "version"), ("Estado", "serverStatus"), ("Id de servicio", "ksqlServiceId"), ("Cluster de Kafka", "kafkaClusterId")] {
            if let Some(v) = si(k) {
                snap.info.push((label.into(), v));
            }
        }
        match self.get_json("/healthcheck").await {
            Ok(h) => {
                let bad: Vec<String> = h
                    .get("details")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .filter(|(_, v)| v.get("isHealthy").and_then(Value::as_bool) == Some(false))
                    .map(|(k, _)| k.clone())
                    .collect();
                let healthy = h.get("isHealthy").and_then(Value::as_bool).unwrap_or(false);
                snap.info.push((
                    "Salud".into(),
                    if healthy { "sana".into() } else { format!("con problemas ({})", bad.join(", ")) },
                ));
            }
            Err(e) => snap.notes.push(format!("No se pudo leer /healthcheck: {e}")),
        }

        // The servers of the cluster.
        let mut alive = None;
        match self.get_json("/clusterStatus").await {
            Ok(c) => {
                let mut t = MonitorTable::new("nodes", "Servidores del cluster", &["Servidor", "Vivo", "Última novedad"]);
                let hosts = c.get("clusterStatus").and_then(Value::as_object).cloned().unwrap_or_default();
                for (host, st) in hosts.iter().take(200) {
                    let ok = st.get("hostAlive").and_then(Value::as_bool).unwrap_or(false);
                    let last = st.get("lastStatusUpdateMs").and_then(Value::as_i64).and_then(chrono_ms);
                    t.rows.push(vec![json!(host), json!(if ok { "sí" } else { "no" }), last.map_or(Value::Null, |s| json!(s))]);
                }
                alive = Some(hosts.values().filter(|s| s.get("hostAlive").and_then(Value::as_bool) == Some(true)).count() as f64);
                snap.tables.push(t);
            }
            Err(_) => snap.notes.push(
                "El estado del cluster (/clusterStatus) requiere ksql.heartbeat.enable=true en los servidores.".into(),
            ),
        }

        // Persistent queries.
        let ents = self.ksql("SHOW QUERIES EXTENDED").await?;
        let queries: Vec<Value> = ents
            .iter()
            .filter_map(|e| e.get("queryDescriptions").and_then(Value::as_array))
            .flatten()
            .cloned()
            .collect();
        let state = |q: &Value| q.get("state").map(text).unwrap_or_default();
        let running = queries.iter().filter(|q| state(q) == "RUNNING").count();
        let failed = queries.iter().filter(|q| state(q) == "ERROR" || state(q) == "UNRESPONSIVE").count();
        let total_lag = queries.iter().map(query_lag).fold(0.0, |a, b| a + b);
        let mut t = MonitorTable::new(
            "queries",
            "Consultas persistentes",
            &["Id", "Tipo", "Estado", "Orígenes", "Destinos", "Retraso (mensajes)", "Errores", "Consulta"],
        );
        for q in queries.iter().take(200) {
            let list = |k: &str| q.get(k).and_then(Value::as_array).map(|a| a.iter().map(text).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            let errors = q.get("queryErrors").and_then(Value::as_array).map_or(0, Vec::len);
            t.rows.push(vec![
                json!(q.get("id").map(|i| i.get("id").map(text).unwrap_or_else(|| text(i))).unwrap_or_default()),
                json!(q.get("queryType").map(text).unwrap_or_default()),
                json!(state(q)),
                json!(list("sources")),
                json!(list("sinks")),
                json!(query_lag(q)),
                json!(errors),
                json!(truncate(&q.get("statementText").map(text).unwrap_or_default())),
            ]);
        }
        snap.tables.insert(0, t);

        // Streams and tables, with their traffic.
        let streams = self.names("SHOW STREAMS", "streams").await.unwrap_or_default();
        let tables = self.names("SHOW TABLES", "tables").await.unwrap_or_default();
        let mut sources: Vec<(String, &str)> = streams
            .iter()
            .map(|n| (n.clone(), "STREAM"))
            .chain(tables.iter().map(|n| (n.clone(), "TABLE")))
            .filter(|(n, _)| n != PROCESSING_LOG && !n.starts_with('_'))
            .collect();
        sources.sort();
        if sources.len() > MAX_SOURCES {
            snap.notes.push(format!("El tráfico se muestra para los primeros {MAX_SOURCES} de {} streams y tablas.", sources.len()));
        }
        let mut totals: BTreeMap<String, f64> = BTreeMap::new();
        let mut t = MonitorTable::new(
            "sources",
            "Streams y tablas",
            &["Nombre", "Tipo", "Topic", "Particiones", "Consumidos/s", "Consumidos", "Producidos/s", "Producidos", "Fallidos"],
        );
        for (name, kind) in sources.iter().take(MAX_SOURCES) {
            let Ok(ents) = self.ksql(&format!("DESCRIBE {} EXTENDED", quote_ident(Quote::Backtick, name))).await else { continue };
            let Some(d) = ents.iter().find_map(|e| e.get("sourceDescription")) else { continue };
            let st = source_stats(d);
            for (k, v) in &st {
                *totals.entry(k.clone()).or_insert(0.0) += v;
            }
            let g = |k: &str| st.get(k).map_or(Value::Null, |v| json!((v * 100.0).round() / 100.0));
            t.rows.push(vec![
                json!(name),
                json!(kind),
                json!(d.get("topic").map(text).unwrap_or_default()),
                d.get("partitions").cloned().unwrap_or(Value::Null),
                g("consumer-messages-per-sec"),
                g("consumer-total-messages"),
                g("messages-per-sec"),
                g("total-messages"),
                g("consumer-failed-messages"),
            ]);
        }
        snap.tables.push(t);

        let tot = |k: &str| totals.get(k).copied().or(Some(0.0).filter(|_| !sources.is_empty()));
        snap.metrics = vec![
            Metric::new("active_sessions", "Consultas persistentes en ejecución", "Actividad", MetricUnit::Count, Some(running as f64)),
            Metric::new("failed_queries", "Consultas con error", "Actividad", MetricUnit::Count, Some(failed as f64)),
            Metric::new("consumer_lag", "Retraso de consumo (mensajes)", "Actividad", MetricUnit::Count, Some(total_lag)),
            Metric::new("messages_per_sec", "Mensajes consumidos por segundo", "Tráfico", MetricUnit::Count, tot("consumer-messages-per-sec")),
            Metric::new("messages_in", "Mensajes consumidos", "Tráfico", MetricUnit::Count, tot("consumer-total-messages")).counter(),
            Metric::new("bytes_in", "Bytes consumidos", "Tráfico", MetricUnit::Bytes, tot("consumer-total-bytes")).counter(),
            Metric::new("messages_out", "Mensajes producidos", "Tráfico", MetricUnit::Count, tot("total-messages")).counter(),
            Metric::new("messages_failed", "Mensajes fallidos", "Tráfico", MetricUnit::Count, tot("consumer-failed-messages")).counter(),
            Metric::new("streams", "Streams", "Objetos", MetricUnit::Count, Some(streams.iter().filter(|n| *n != PROCESSING_LOG).count() as f64)),
            Metric::new("tables", "Tablas", "Objetos", MetricUnit::Count, Some(tables.len() as f64)),
            Metric::new("nodes", "Servidores vivos", "Cluster", MetricUnit::Count, alive),
        ];
        snap.notes.push(
            "ksqlDB no expone CPU ni memoria por la API REST (solo por JMX); el tráfico se mide en mensajes por stream y tabla.".into(),
        );
        Ok(snap)
    }
}

/// Epoch milliseconds as `YYYY-MM-DD HH:MM:SS` (UTC).
fn chrono_ms(ms: i64) -> Option<String> {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some(format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lag_and_stats() {
        let q = json!({"tasksMetadata": [
            {"topicOffsets": [{"topicPartitionEntity": {"topic": "mon_s", "partition": 0}, "endOffset": 10, "committedOffset": 4}]},
            {"topicOffsets": [{"topicPartitionEntity": {"topic": "mon_s", "partition": 1}, "endOffset": 3, "committedOffset": -1}]},
            {"topicOffsets": [{"topicPartitionEntity": {"topic": "_confluent-ksql-x-repartition", "partition": 0}, "endOffset": 99, "committedOffset": -1}]}
        ]});
        assert_eq!(query_lag(&q), 9.0);
        assert_eq!(query_lag(&json!({})).to_string(), "0");
        let d = json!({
            "clusterStatistics": [
                {"name": "consumer-total-messages", "host": "a:8088", "value": 2.0},
                {"name": "consumer-total-messages", "host": "b:8088", "value": 3.0}
            ],
            "clusterErrorStats": [{"name": "consumer-failed-messages", "host": "a:8088", "value": 1.0}]
        });
        let s = source_stats(&d);
        assert_eq!(s.get("consumer-total-messages"), Some(&5.0));
        assert_eq!(s.get("consumer-failed-messages"), Some(&1.0));
        assert_eq!(chrono_ms(1_700_000_000_000).as_deref(), Some("2023-11-14 22:13:20"));
    }
}

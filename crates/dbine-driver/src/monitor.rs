//! Server monitoring ([`crate::Session::monitor`]): one snapshot of what the
//! server reports about itself: CPU, memory, connections, throughput,
//! cache, storage, plus tables such as sessions, running queries, locks and
//! sizes. The UI polls it every few seconds and draws charts over time.
//!
//! Every figure is optional: engines report what they can. A metric that
//! the server only exposes as a running total (statements since start,
//! bytes read…) is flagged `counter`, and the UI turns two snapshots into a
//! rate per second.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MonitorSnapshot {
    /// Headline figures, in display order. Metrics sharing a `group` are
    /// shown together ("CPU", "Memoria", "Conexiones", "Actividad",
    /// "Caché", "Almacenamiento", "Replicación"…).
    pub metrics: Vec<Metric>,
    /// Detail tables, in display order.
    pub tables: Vec<MonitorTable>,
    /// Facts that don't change between snapshots: version, edition, uptime
    /// source, host, cluster name, role… (label, value), Spanish labels.
    pub info: Vec<(String, String)>,
    /// What this engine can't report and why, shown under the dashboard
    /// ("El uso de CPU no está disponible sin permiso de VIEW SERVER STATE").
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    /// Stable id within the engine ("cpu", "mem_used", "connections"…);
    /// the UI keys its history by it.
    pub key: String,
    /// Spanish label ("CPU del servidor", "Conexiones activas").
    pub label: String,
    pub group: String,
    pub unit: MetricUnit,
    /// `None` when the server didn't report it this time.
    pub value: Option<f64>,
    /// The ceiling, when there is one (memory limit, max connections…):
    /// the UI draws a gauge.
    pub max: Option<f64>,
    /// A running total since server start: the UI charts the rate per
    /// second between snapshots instead of the raw value.
    pub counter: bool,
}

impl Metric {
    pub fn new(key: &str, label: &str, group: &str, unit: MetricUnit, value: Option<f64>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            group: group.into(),
            unit,
            value,
            max: None,
            counter: false,
        }
    }

    pub fn max(mut self, max: Option<f64>) -> Self {
        self.max = max;
        self
    }

    /// Mark it a running total (the UI shows the rate per second).
    pub fn counter(mut self) -> Self {
        self.counter = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    /// 0–100. A CPU-time counter (seconds of CPU since start) goes as
    /// `Percent` with `counter` and the value × 100: its rate is then the
    /// percentage of one core in use.
    Percent,
    Bytes,
    Count,
    Millis,
    Seconds,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MonitorTable {
    /// Stable id ("sessions", "queries", "locks", "databases"…).
    pub key: String,
    /// Spanish title ("Sesiones", "Consultas en curso").
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
}

impl MonitorTable {
    pub fn new(key: &str, title: &str, columns: &[&str]) -> Self {
        Self {
            key: key.into(),
            title: title.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows: Vec::new(),
        }
    }
}

/// Parse a number out of a server-reported text value ("12.5", "1024",
/// "12.5%"); `None` if it isn't one.
pub fn num(s: &str) -> Option<f64> {
    s.trim().trim_end_matches('%').trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders() {
        let m = Metric::new("q", "Consultas", "Actividad", MetricUnit::Count, Some(3.0)).counter().max(Some(9.0));
        assert!(m.counter);
        assert_eq!(m.max, Some(9.0));
        assert_eq!(num(" 12.5% "), Some(12.5));
        assert_eq!(num("x"), None);
    }
}

/// A server session in a blocking chain (the Monitor's "Bloqueos"): either
/// waiting on another (`blocked_by`) or holding what others wait for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BlockedSession {
    /// The engine's session id ("53", "12345", an opid…), what
    /// `Session::kill_session` takes.
    pub id: String,
    /// The session it waits for; `None` for the head of a chain.
    pub blocked_by: Option<String>,
    pub user: Option<String>,
    /// The client: host, application.
    pub client: Option<String>,
    pub database: Option<String>,
    /// What it's doing or waiting on ("LCK_M_X", "Lock: transactionid",
    /// "idle in transaction"…), as the engine says it.
    pub wait: Option<String>,
    /// How long it has been waiting (or, for a head, running), ms.
    pub waited_ms: Option<u64>,
    /// The locked object, when the engine says ("dbo.facturas", a row…).
    pub object: Option<String>,
    /// Its current (or last) statement.
    pub sql: Option<String>,
}

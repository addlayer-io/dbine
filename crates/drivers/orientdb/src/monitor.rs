//! Server monitor: `GET /server` (connections with their current command,
//! open storages, configuration), the database metadata (records per
//! class) and `SELECT FROM metadata:storage` (size on disk).

use crate::{as_text, classify, OrientSession, EDGE, VERTEX};
use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::Value;

const MAX_ROWS: usize = 200;
const MAX_TEXT: usize = 2000;

fn f(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => num(s),
        _ => None,
    }
}

fn clip(s: String) -> Value {
    if s.chars().count() > MAX_TEXT {
        Value::String(format!("{}…", s.chars().take(MAX_TEXT).collect::<String>()))
    } else {
        Value::String(s)
    }
}

/// Idle connections show `-` or nothing as their command.
pub(crate) fn busy(c: &Value) -> bool {
    let info = c.get("commandInfo").map(as_text).unwrap_or_default();
    !matches!(info.as_str(), "" | "-" | "Server status" | "Listening")
}

pub fn from_server(server: &Value, snap: &mut MonitorSnapshot) {
    let conns: Vec<Value> = server.get("connections").and_then(Value::as_array).cloned().unwrap_or_default();
    let active = conns.iter().filter(|c| busy(c)).count();
    snap.metrics.push(Metric::new("connections", "Conexiones", "Conexiones", MetricUnit::Count, Some(conns.len() as f64)));
    snap.metrics.push(Metric::new("active_sessions", "Conexiones ejecutando", "Conexiones", MetricUnit::Count, Some(active as f64)));
    let mut t = MonitorTable::new(
        "sessions",
        "Sesiones",
        &["id", "usuario", "base", "cliente", "protocolo", "estado", "última duración (ms)", "pedidos", "tiempo total (ms)", "conectada", "consulta actual"],
    );
    for c in conns.iter().take(MAX_ROWS) {
        let g = |k: &str| c.get(k).cloned().unwrap_or(Value::Null);
        let n = |k: &str| f(c.get(k)).map(Value::from).unwrap_or(Value::Null);
        let detail = c.get("commandDetail").map(as_text).filter(|d| d != "-").unwrap_or_default();
        t.rows.push(vec![
            g("connectionId"),
            g("user"),
            g("db"),
            g("remoteAddress"),
            g("protocol"),
            g("commandInfo"),
            n("lastExecutionTime"),
            n("totalRequests"),
            n("totalWorkingTime"),
            g("connectedOn"),
            clip(detail),
        ]);
    }
    snap.tables.push(t);

    let mut dbs = MonitorTable::new("databases", "Bases abiertas", &["nombre", "tipo", "ruta", "usuarios activos"]);
    for s in server.get("storages").and_then(Value::as_array).cloned().unwrap_or_default().iter().take(MAX_ROWS) {
        dbs.rows.push(["name", "type", "path", "activeUsers"].iter().map(|k| s.get(*k).cloned().unwrap_or(Value::Null)).collect());
    }
    snap.tables.push(dbs);

    const KEYS: &[(&str, &str)] = &[
        ("storage.diskCache.bufferSize", "Caché de disco (MB)"),
        ("network.http.maxConnections", "Máx. conexiones HTTP"),
        ("command.timeout", "Timeout de comandos (ms)"),
        ("query.timeout.defaultStrategy", "Estrategia de timeout"),
        ("storage.useWAL", "WAL"),
    ];
    let globals: Vec<Value> = server.get("globalProperties").and_then(Value::as_array).cloned().unwrap_or_default();
    for (k, label) in KEYS {
        if let Some(v) = globals.iter().find(|p| p.get("key").and_then(Value::as_str) == Some(k)).and_then(|p| p.get("value")) {
            snap.info.push((label.to_string(), as_text(v)));
        }
    }
}

pub async fn snapshot(s: &mut OrientSession) -> Result<MonitorSnapshot> {
    let mut snap = MonitorSnapshot::default();
    let mut notes = Vec::new();
    match s.server().await {
        Ok(server) => from_server(&server, &mut snap),
        Err(dbine_driver::Error::Query(m)) | Err(dbine_driver::Error::AuthFailed(m)) => {
            notes.push(format!("Las conexiones y la configuración del servidor (GET /server) requieren un usuario del servidor, como root: {m}"))
        }
        Err(e) => return Err(e),
    }

    let meta = s.metadata().await?;
    if let Some(sv) = meta.get("server") {
        let v = |k: &str| sv.get(k).map(as_text).unwrap_or_default();
        snap.info.insert(0, ("Versión".into(), format!("OrientDB {}", v("version"))));
        snap.info.push(("Sistema operativo".into(), format!("{} {} ({})", v("osName"), v("osVersion"), v("osArch"))));
        snap.info.push(("Java".into(), format!("{} {}", v("javaVendor"), v("javaVersion"))));
    }
    snap.info.push(("Base actual".into(), s.db.clone()));

    // Records per class.
    let classes = classify(meta.get("classes").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]));
    let (mut vertices, mut edges, mut total) = (0.0, 0.0, 0.0);
    let mut sizes: Vec<(String, &str, f64)> = Vec::new();
    for (name, kind, c) in &classes {
        let n = f(c.get("records")).unwrap_or(0.0);
        // `V` / `E` count their subclasses' records too.
        if name == "V" || name == "E" {
            continue;
        }
        match *kind {
            VERTEX => vertices += n,
            EDGE => edges += n,
            _ => {}
        }
        total += n;
        sizes.push((name.clone(), kind, n));
    }
    sizes.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut top = MonitorTable::new("top_objects", "Clases con más registros", &["clase", "tipo", "registros"]);
    for (name, kind, n) in sizes.iter().take(20) {
        let k = match *kind {
            VERTEX => "vértices",
            EDGE => "aristas",
            _ => "documentos",
        };
        top.rows.push(vec![Value::String(name.clone()), Value::String(k.into()), Value::from(*n)]);
    }
    snap.tables.push(top);
    snap.metrics.push(Metric::new("nodes", "Vértices", "Almacenamiento", MetricUnit::Count, Some(vertices)));
    snap.metrics.push(Metric::new("relationships", "Aristas", "Almacenamiento", MetricUnit::Count, Some(edges)));
    snap.metrics.push(Metric::new("records", "Registros", "Almacenamiento", MetricUnit::Count, Some(total)));

    match s.command("SELECT size, totalClusters, type FROM metadata:storage", -1).await {
        Ok(r) => {
            let row = r.records.first();
            let g = |k: &str| row.and_then(|r| r.iter().find(|(x, _)| x == k)).map(|(_, v)| v.clone());
            snap.metrics.push(Metric::new("storage_used", "Espacio en disco", "Almacenamiento", MetricUnit::Bytes, f(g("size").as_ref())));
            if let Some(t) = g("type") {
                snap.info.push(("Almacenamiento".into(), as_text(&t)));
            }
            if let Some(t) = g("totalClusters") {
                snap.info.push(("Clusters".into(), as_text(&t)));
            }
        }
        Err(e) => notes.push(format!("No se pudo leer el tamaño de la base: {e}")),
    }

    notes.push("OrientDB no publica CPU, memoria ni consultas por segundo por su API REST: requieren el profiler o el agente de OrientDB Enterprise.".into());
    snap.notes = notes;
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn server_status() {
        let server = json!({
            "connections": [
                { "connectionId": "3", "remoteAddress": "/1.2.3.4:5", "db": "demo", "user": "root", "totalRequests": "7", "commandInfo": "Command", "commandDetail": "SELECT FROM X", "lastExecutionTime": "12", "totalWorkingTime": "40", "connectedOn": "2024-01-01 00:00:00", "protocol": "http" },
                { "connectionId": "4", "commandInfo": "-", "commandDetail": "-" }
            ],
            "storages": [{ "name": "demo", "type": "OLocalPaginatedStorage", "path": "/x", "activeUsers": "n.a." }],
            "globalProperties": [{ "key": "storage.diskCache.bufferSize", "value": 4096 }]
        });
        let mut snap = MonitorSnapshot::default();
        from_server(&server, &mut snap);
        assert_eq!(snap.metrics[0].value, Some(2.0));
        assert_eq!(snap.metrics[1].value, Some(1.0));
        assert_eq!(snap.tables[0].rows[0][7], json!(7.0));
        assert_eq!(snap.tables[0].rows[0][10], json!("SELECT FROM X"));
        assert_eq!(snap.info, vec![("Caché de disco (MB)".to_string(), "4096".to_string())]);
    }
}

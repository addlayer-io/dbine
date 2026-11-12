//! Server monitor: `GET /server` (connections with their current command,
//! open storages, configuration), the database metadata (records per
//! class) and `SELECT FROM metadata:storage` (size on disk).

use crate::{as_text, classify, OrientSession, EDGE, VERTEX};
use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::Value;
use std::collections::HashMap;

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
    let conns: Vec<Value> = server
        .get("connections")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let active = conns.iter().filter(|c| busy(c)).count();
    snap.metrics.push(
        Metric::new(
            "connections",
            "Conexiones",
            "Conexiones",
            MetricUnit::Count,
            Some(conns.len() as f64),
        )
        .help("Conexiones abiertas al servidor, binarias y HTTP, de todas las bases; incluye la que DBine usa para pedir este estado.")
        .source("GET /server: connections"),
    );
    snap.metrics.push(
        Metric::new(
            "active_sessions",
            "Conexiones ejecutando",
            "Conexiones",
            MetricUnit::Count,
            Some(active as f64),
        )
        .help("Conexiones con un comando en curso al momento de la lectura, sin contar la de DBine. Si se acumulan hay consultas lentas: su consulta actual está en la tabla de sesiones.")
        .source("GET /server: connections whose commandInfo isn't empty, -, Listening or Server status"),
    );
    let mut t = MonitorTable::new(
        "sessions",
        "Sesiones",
        &[
            "id",
            "usuario",
            "base",
            "cliente",
            "protocolo",
            "estado",
            "última duración (ms)",
            "pedidos",
            "tiempo total (ms)",
            "conectada",
            "consulta actual",
        ],
    );
    for c in conns.iter().take(MAX_ROWS) {
        let g = |k: &str| c.get(k).cloned().unwrap_or(Value::Null);
        let n = |k: &str| f(c.get(k)).map(Value::from).unwrap_or(Value::Null);
        let detail = c
            .get("commandDetail")
            .map(as_text)
            .filter(|d| d != "-")
            .unwrap_or_default();
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

    let mut dbs = MonitorTable::new(
        "databases",
        "Bases abiertas",
        &["nombre", "tipo", "ruta", "usuarios activos"],
    );
    for s in server
        .get("storages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .take(MAX_ROWS)
    {
        dbs.rows.push(
            ["name", "type", "path", "activeUsers"]
                .iter()
                .map(|k| s.get(*k).cloned().unwrap_or(Value::Null))
                .collect(),
        );
    }
    snap.tables.push(dbs);

    const KEYS: &[(&str, &str)] = &[
        ("storage.diskCache.bufferSize", "Caché de disco (MB)"),
        ("network.http.maxConnections", "Máx. conexiones HTTP"),
        ("command.timeout", "Timeout de comandos (ms)"),
        ("query.timeout.defaultStrategy", "Estrategia de timeout"),
        ("storage.useWAL", "WAL"),
    ];
    let globals: Vec<Value> = server
        .get("globalProperties")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (k, label) in KEYS {
        if let Some(v) = globals
            .iter()
            .find(|p| p.get("key").and_then(Value::as_str) == Some(k))
            .and_then(|p| p.get("value"))
        {
            snap.info.push((label.to_string(), as_text(v)));
        }
    }
}

/// Vertices, edges and records of every class. A class's `records` counts
/// its subclasses' too (each one once, even through two paths), so adding
/// up every class would count a record once per ancestor. `V` and `E`
/// already hold every vertex and edge. For the total, each class's own
/// records are its count minus its descendants' own records, worked out
/// from the leaves up: that counts a record once whether its class extends
/// a system class left out of `classes` (`ORestricted`) or two user classes.
fn record_totals(classes: &[(String, &'static str, Value)]) -> (f64, f64, f64) {
    let index: HashMap<&str, usize> = classes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.0.as_str(), i))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); classes.len()];
    for (i, (_, _, c)) in classes.iter().enumerate() {
        let mut supers: Vec<String> = c
            .get("superClasses")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(as_text).collect())
            .unwrap_or_default();
        supers.extend(c.get("superClass").map(as_text));
        let mut parents: Vec<usize> = supers
            .iter()
            .filter_map(|s| index.get(s.as_str()).copied())
            .filter(|&p| p != i)
            .collect();
        parents.sort_unstable();
        parents.dedup();
        for p in parents {
            children[p].push(i);
        }
    }
    let descendants: Vec<Vec<usize>> = (0..classes.len())
        .map(|i| {
            let mut seen = vec![false; classes.len()];
            seen[i] = true;
            let (mut out, mut stack) = (Vec::new(), vec![i]);
            while let Some(x) = stack.pop() {
                for &c in &children[x] {
                    if !seen[c] {
                        seen[c] = true;
                        out.push(c);
                        stack.push(c);
                    }
                }
            }
            out
        })
        .collect();
    let records: Vec<f64> = classes
        .iter()
        .map(|c| f(c.2.get("records")).unwrap_or(0.0))
        .collect();
    // A descendant has fewer descendants than its ancestor.
    let mut order: Vec<usize> = (0..classes.len()).collect();
    order.sort_by_key(|&i| descendants[i].len());
    let mut own = vec![0.0; classes.len()];
    for i in order {
        let below = descendants[i].iter().fold(0.0, |a, &d| a + own[d]);
        own[i] = (records[i] - below).max(0.0);
    }
    let of = |name: &str| index.get(name).map_or(0.0, |&i| records[i]);
    (of("V"), of("E"), own.iter().fold(0.0, |a, n| a + n))
}

/// The records of the current database's vertex, edge and every class.
fn record_counts(vertices: f64, edges: f64, total: f64) -> Vec<Metric> {
    vec![
        Metric::new(
            "nodes",
            "Vértices",
            "Almacenamiento",
            MetricUnit::Count,
            Some(vertices),
        )
        .help("Vértices de la base actual, de todas las clases que heredan de V.")
        .source("GET /database/{db}: records of class V (it counts its subclasses)"),
        Metric::new(
            "relationships",
            "Aristas",
            "Almacenamiento",
            MetricUnit::Count,
            Some(edges),
        )
        .help("Aristas de la base actual, de todas las clases que heredan de E. Las aristas livianas no son registros y no cuentan.")
        .source("GET /database/{db}: records of class E (it counts its subclasses)"),
        Metric::new(
            "records",
            "Registros",
            "Almacenamiento",
            MetricUnit::Count,
            Some(total),
        )
        .help("Registros de la base actual, sin los de las clases del sistema: vértices, aristas y documentos, cada uno contado una vez.")
        .source("GET /database/{db}: classes[].records (it counts subclasses too) less what the subclasses hold, summed over the classes, system classes left out"),
    ]
}

/// The size of the current database's records.
fn storage_used(size: Option<f64>) -> Metric {
    Metric::new(
        "storage_used",
        "Tamaño de los registros",
        "Almacenamiento",
        MetricUnit::Bytes,
        size,
    )
    .help("Bytes que ocupan los registros guardados en los clusters de la base actual. No incluye índices, WAL ni el espacio reservado de los archivos, así que lo que ocupa la base en disco es bastante más.")
    .source("SELECT size FROM metadata:storage")
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
        snap.info
            .insert(0, ("Versión".into(), format!("OrientDB {}", v("version"))));
        snap.info.push((
            "Sistema operativo".into(),
            format!("{} {} ({})", v("osName"), v("osVersion"), v("osArch")),
        ));
        snap.info.push((
            "Java".into(),
            format!("{} {}", v("javaVendor"), v("javaVersion")),
        ));
    }
    snap.info.push(("Base actual".into(), s.db.clone()));

    // Records per class.
    let classes = classify(
        meta.get("classes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
    let (vertices, edges, total) = record_totals(&classes);
    let mut sizes: Vec<(String, &str, f64)> = Vec::new();
    for (name, kind, c) in &classes {
        let n = f(c.get("records")).unwrap_or(0.0);
        if name == "V" || name == "E" {
            continue;
        }
        sizes.push((name.clone(), kind, n));
    }
    sizes.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut top = MonitorTable::new(
        "top_objects",
        "Clases con más registros",
        &["clase", "tipo", "registros"],
    );
    for (name, kind, n) in sizes.iter().take(20) {
        let k = match *kind {
            VERTEX => "vértices",
            EDGE => "aristas",
            _ => "documentos",
        };
        top.rows.push(vec![
            Value::String(name.clone()),
            Value::String(k.into()),
            Value::from(*n),
        ]);
    }
    snap.tables.push(top);
    snap.metrics.extend(record_counts(vertices, edges, total));

    match s
        .command("SELECT size, totalClusters, type FROM metadata:storage", -1)
        .await
    {
        Ok(r) => {
            let row = r.records.first();
            let g = |k: &str| {
                row.and_then(|r| r.iter().find(|(x, _)| x == k))
                    .map(|(_, v)| v.clone())
            };
            // `size` is what the records hold, not the files: a few KB
            // for a database that takes megabytes on disk.
            snap.metrics.push(storage_used(f(g("size").as_ref())));
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
        assert_eq!(
            snap.info,
            vec![("Caché de disco (MB)".to_string(), "4096".to_string())]
        );
    }

    #[test]
    fn records_count_once_per_record() {
        // `records` counts the subclasses too: Employee's one is in Person
        // and V as well.
        let classes = crate::classify(&[
            json!({"name": "V", "records": 2, "superClasses": []}),
            json!({"name": "E", "records": 1}),
            json!({"name": "Person", "records": 2, "superClass": "V", "superClasses": ["V"]}),
            json!({"name": "Employee", "records": 1, "superClass": "Person", "superClasses": ["Person"]}),
            json!({"name": "Knows", "records": 1, "superClass": "E"}),
            json!({"name": "Note", "records": 4, "superClasses": []}),
            json!({"name": "OUser", "records": 3}),
            // A system superclass is left out of the classes but its
            // subclass's records still count.
            json!({"name": "Invoice", "records": 5, "superClass": "ORestricted", "superClasses": ["ORestricted"]}),
            // Two user superclasses: Both's 2 records are in A and B too.
            json!({"name": "A", "records": 3, "superClasses": []}),
            json!({"name": "B", "records": 2, "superClasses": []}),
            json!({"name": "Both", "records": 2, "superClass": "A", "superClasses": ["A", "B"]}),
        ]);
        // 2 + 1 + 4 (Note) + 5 (Invoice) + 1 (A) + 0 (B) + 2 (Both).
        assert_eq!(record_totals(&classes), (2.0, 1.0, 15.0));
        assert_eq!(record_totals(&[]), (0.0, 0.0, 0.0));
    }

    #[test]
    fn every_metric_has_help_and_source() {
        let server = json!({ "connections": [{ "connectionId": "3", "commandInfo": "Command" }] });
        let mut snap = MonitorSnapshot::default();
        from_server(&server, &mut snap);
        snap.metrics.extend(record_counts(2.0, 1.0, 7.0));
        snap.metrics.push(storage_used(Some(17178.0)));
        assert_eq!(snap.metrics.len(), 6);
        for m in &snap.metrics {
            assert!(
                m.help.as_deref().is_some_and(|h| !h.is_empty()),
                "{}",
                m.key
            );
            assert!(
                m.source.as_deref().is_some_and(|s| !s.is_empty()),
                "{}",
                m.key
            );
        }
    }
}

//! `Session::monitor` for MongoDB and the servers that speak its protocol
//! (FerretDB, Amazon DocumentDB). Every command is optional: one the
//! server refuses (missing privilege, not implemented by a compatible
//! server) is skipped and leaves a note, never fails the snapshot.
//!
//! - `serverStatus`: connections, opcounters, memory, WiredTiger cache and
//!   block manager, network, `extra_info` (process CPU time, page faults),
//!   global lock queue, document metrics, transactions, uptime.
//! - `hostInfo`: cores, host memory (the gauge's ceiling).
//! - `hello` / `replSetGetStatus`: role and replica lag.
//! - `currentOp`: sessions, running operations, operations waiting on a lock.
//! - `listDatabases` + `dbStats`: sizes per database.
//! - `top`: time spent per collection.

use crate::Flavor;
use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use dbine_driver::Result;
use mongodb::bson::{doc, Bson, Document};
use mongodb::Client;
use serde_json::{json, Value};

const MB: f64 = 1024.0 * 1024.0;
const MAX_ROWS: usize = 200;
const MAX_DBS: usize = 50;

/// A number at `path` inside `d` (any BSON numeric type).
pub(crate) fn num(d: &Document, path: &[&str]) -> Option<f64> {
    let (last, parents) = path.split_last()?;
    let mut cur = d;
    for p in parents {
        cur = cur.get_document(p).ok()?;
    }
    bson_f64(cur.get(last)?)
}

fn bson_f64(v: &Bson) -> Option<f64> {
    match v {
        Bson::Int32(n) => Some(*n as f64),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(n) => Some(*n),
        Bson::Decimal128(d) => d.to_string().parse().ok(),
        Bson::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn text(d: &Document, key: &str) -> Value {
    match d.get(key) {
        None | Some(Bson::Null) => Value::Null,
        Some(Bson::String(s)) => Value::String(s.clone()),
        Some(v) => bson_f64(v).map(|n| json!(n)).unwrap_or_else(|| Value::String(compact(v, 200))),
    }
}

pub(crate) fn compact(v: &Bson, max: usize) -> String {
    let s = v.clone().into_relaxed_extjson().to_string();
    if s.chars().count() <= max {
        s
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn sum(values: &[Option<f64>]) -> Option<f64> {
    let present: Vec<f64> = values.iter().flatten().copied().collect();
    (!present.is_empty()).then(|| present.iter().sum())
}

async fn admin(client: &Client, cmd: Document) -> std::result::Result<Document, String> {
    client.database("admin").run_command(cmd).await.map_err(|e| match e.kind.as_ref() {
        mongodb::error::ErrorKind::Command(c) => format!("{} ({})", c.message, c.code_name),
        _ => e.to_string(),
    })
}

fn product(flavor: Flavor) -> &'static str {
    match flavor {
        Flavor::Mongo => "MongoDB",
        Flavor::Ferret => "FerretDB",
        Flavor::DocumentDb => "Amazon DocumentDB",
    }
}

pub(crate) async fn snapshot(client: &Client, flavor: Flavor) -> Result<MonitorSnapshot> {
    let mut s = MonitorSnapshot::default();
    let name = product(flavor);
    let skip = |s: &mut MonitorSnapshot, what: &str, e: String| {
        s.notes.push(format!("{name} no respondió a {what}: {e}"));
    };

    // serverStatus: the bulk of the figures.
    let status = match admin(client, doc! { "serverStatus": 1 }).await {
        Ok(d) => Some(d),
        Err(e) => {
            skip(&mut s, "serverStatus (hace falta el rol clusterMonitor)", e);
            None
        }
    };
    let host = admin(client, doc! { "hostInfo": 1 }).await.ok();
    let st = status.clone().unwrap_or_default();
    let host_mem = host.as_ref().and_then(|h| num(h, &["system", "memSizeMB"])).map(|m| m * MB);

    // CPU: MongoDB has no host CPU figure; the process CPU time comes from
    // extra_info on Linux.
    let cpu_us = sum(&[num(&st, &["extra_info", "user_time_us"]), num(&st, &["extra_info", "system_time_us"])]);
    if let Some(us) = cpu_us {
        s.metrics.push(
            Metric::new("cpu_time", "CPU del proceso", "CPU", MetricUnit::Percent, Some(us / 1e6 * 100.0)).counter(),
        );
    }
    s.notes.push(if cpu_us.is_some() {
        format!("{name} no informa el uso de CPU del host; se muestra el tiempo de CPU del proceso.")
    } else {
        format!("{name} no informa el uso de CPU del servidor ni del proceso (MongoDB solo lo da en extra_info, en Linux).")
    });

    // Memory.
    let resident = num(&st, &["mem", "resident"]).map(|m| m * MB);
    // FerretDB has no mem / connections sections: no empty cards for it.
    let reports = |v: Option<f64>| v.is_some() || flavor != Flavor::Ferret;
    if reports(resident) {
        s.metrics.push(Metric::new("mem_used", "Memoria residente", "Memoria", MetricUnit::Bytes, resident).max(host_mem));
    }
    if let Some(v) = num(&st, &["mem", "virtual"]) {
        s.metrics.push(Metric::new("mem_virtual", "Memoria virtual", "Memoria", MetricUnit::Bytes, Some(v * MB)));
    }
    let cache = st.get_document("wiredTiger").ok().and_then(|w| w.get_document("cache").ok()).cloned();
    if let Some(c) = &cache {
        let used = num(c, &["bytes currently in the cache"]);
        let max = num(c, &["maximum bytes configured"]);
        s.metrics.push(Metric::new("mem_cache", "Caché de WiredTiger", "Memoria", MetricUnit::Bytes, used).max(max));
        if let Some(d) = num(c, &["tracked dirty bytes in the cache"]) {
            s.metrics.push(Metric::new("cache_dirty", "Caché sucia", "Memoria", MetricUnit::Bytes, Some(d)).max(max));
        }
        let requested = num(c, &["pages requested from the cache"]);
        let read_in = num(c, &["pages read into cache"]);
        let hit = match (requested, read_in) {
            (Some(r), Some(m)) if r > 0.0 => Some(((r - m) / r * 100.0).clamp(0.0, 100.0)),
            _ => None,
        };
        s.metrics.push(Metric::new("cache_hit", "Aciertos de caché", "Caché", MetricUnit::Percent, hit).max(Some(100.0)));
        if let Some(e) = sum(&[
            num(c, &["modified pages evicted"]),
            num(c, &["unmodified pages evicted"]),
        ]) {
            s.metrics.push(Metric::new("evictions", "Páginas desalojadas", "Caché", MetricUnit::Count, Some(e)).counter());
        }
    }
    if let Some(pf) = num(&st, &["extra_info", "page_faults"]) {
        s.metrics.push(Metric::new("page_faults", "Fallos de página", "Memoria", MetricUnit::Count, Some(pf)).counter());
    }

    // Connections.
    let current = num(&st, &["connections", "current"]);
    let available = num(&st, &["connections", "available"]);
    let max_conn = match (current, available) {
        (Some(c), Some(a)) => Some(c + a),
        _ => None,
    };
    if reports(current) {
        s.metrics.push(Metric::new("connections", "Conexiones", "Conexiones", MetricUnit::Count, current).max(max_conn));
    }
    let active = num(&st, &["connections", "active"]).or_else(|| num(&st, &["globalLock", "activeClients", "total"]));
    if active.is_some() {
        s.metrics.push(Metric::new("active_sessions", "Sesiones activas", "Conexiones", MetricUnit::Count, active));
    }
    if let Some(n) = num(&st, &["connections", "totalCreated"]) {
        s.metrics.push(Metric::new("connections_created", "Conexiones abiertas", "Conexiones", MetricUnit::Count, Some(n)).counter());
    }

    // Activity.
    let ops = ["insert", "query", "update", "delete", "getmore", "command"];
    let op_values: Vec<Option<f64>> = ops.iter().map(|o| num(&st, &["opcounters", o])).collect();
    let mut total_ops = sum(&op_values);
    if total_ops.is_none() {
        // FerretDB: no opcounters, but per-command totals.
        if let Ok(cmds) = st.get_document("metrics").and_then(|m| m.get_document("commands")) {
            let per = |k: &str| -> Vec<Option<f64>> {
                cmds.values().filter_map(Bson::as_document).map(|c| num(c, &[k])).collect()
            };
            total_ops = sum(&per("total"));
            if let Some(f) = sum(&per("failed")) {
                s.metrics.push(Metric::new("commands_failed", "Comandos con error", "Actividad", MetricUnit::Count, Some(f)).counter());
            }
        }
    }
    s.metrics.push(Metric::new("queries", "Operaciones", "Actividad", MetricUnit::Count, total_ops).counter());
    let labels = ["Inserciones", "Consultas (find)", "Actualizaciones", "Borrados", "getMore", "Comandos"];
    for ((op, label), v) in ops.iter().zip(labels).zip(&op_values) {
        if v.is_some() {
            s.metrics.push(Metric::new(&format!("op_{op}"), label, "Actividad", MetricUnit::Count, *v).counter());
        }
    }
    let committed = num(&st, &["transactions", "totalCommitted"]);
    if committed.is_some() {
        let tx = sum(&[committed, num(&st, &["transactions", "totalAborted"])]);
        s.metrics.push(Metric::new("transactions", "Transacciones", "Actividad", MetricUnit::Count, tx).counter());
    }
    let returned = num(&st, &["metrics", "document", "returned"]);
    if returned.is_some() {
        s.metrics.push(Metric::new("rows_read", "Documentos leídos", "Actividad", MetricUnit::Count, returned).counter());
        let written = sum(&[
            num(&st, &["metrics", "document", "inserted"]),
            num(&st, &["metrics", "document", "updated"]),
            num(&st, &["metrics", "document", "deleted"]),
        ]);
        s.metrics.push(Metric::new("rows_written", "Documentos escritos", "Actividad", MetricUnit::Count, written).counter());
    }
    if let Some(n) = num(&st, &["metrics", "queryExecutor", "scannedObjects"]) {
        s.metrics.push(Metric::new("docs_scanned", "Documentos examinados", "Actividad", MetricUnit::Count, Some(n)).counter());
    }

    // Network and disk.
    if let Some(n) = num(&st, &["network", "bytesIn"]) {
        s.metrics.push(Metric::new("net_in", "Red entrante", "Red", MetricUnit::Bytes, Some(n)).counter());
    }
    if let Some(n) = num(&st, &["network", "bytesOut"]) {
        s.metrics.push(Metric::new("net_out", "Red saliente", "Red", MetricUnit::Bytes, Some(n)).counter());
    }
    if let Some(n) = num(&st, &["wiredTiger", "block-manager", "bytes read"]) {
        s.metrics.push(Metric::new("disk_read", "Lectura en disco", "Disco", MetricUnit::Bytes, Some(n)).counter());
    }
    if let Some(n) = num(&st, &["wiredTiger", "block-manager", "bytes written"]) {
        s.metrics.push(Metric::new("disk_write", "Escritura en disco", "Disco", MetricUnit::Bytes, Some(n)).counter());
    }

    // Locks.
    if let Some(q) = num(&st, &["globalLock", "currentQueue", "total"]) {
        s.metrics.push(Metric::new("locks_waiting", "Operaciones en cola", "Bloqueos", MetricUnit::Count, Some(q)));
    }

    // Server.
    let uptime = num(&st, &["uptime"]);
    s.metrics.push(Metric::new("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, uptime));
    if let Some(n) = num(&st, &["asserts", "regular"]).zip(num(&st, &["asserts", "user"])).map(|(a, b)| a + b) {
        s.metrics.push(Metric::new("asserts", "Asserts", "Servidor", MetricUnit::Count, Some(n)).counter());
    }

    // Info.
    s.info.push(("Producto".into(), name.into()));
    if let Ok(v) = st.get_str("version") {
        s.info.push(("Versión".into(), v.into()));
    }
    if let Ok(f) = st.get_document("ferretdb") {
        for (k, label) in [("version", "Versión de FerretDB"), ("postgresql", "PostgreSQL"), ("documentdb", "Extensión DocumentDB")] {
            if let Ok(v) = f.get_str(k) {
                s.info.push((label.into(), v.into()));
            }
        }
    }
    if status.is_some() && current.is_none() && resident.is_none() {
        s.notes.push(format!(
            "{name} no informa memoria, conexiones, red ni caché en serverStatus; se muestran los contadores de comandos."
        ));
    }
    if let Ok(v) = st.get_str("host") {
        s.info.push(("Servidor".into(), v.into()));
    }
    if let Ok(v) = st.get_str("process") {
        s.info.push(("Proceso".into(), v.into()));
    }
    if let Ok(v) = st.get_document("storageEngine").and_then(|e| e.get_str("name")) {
        s.info.push(("Motor de almacenamiento".into(), v.into()));
    }
    if let Some(m) = max_conn {
        s.info.push(("Conexiones máximas".into(), format!("{m}")));
    }
    if let Some(c) = cache.as_ref().and_then(|c| num(c, &["maximum bytes configured"])) {
        s.info.push(("Caché configurada".into(), format!("{:.0} MB", c / MB)));
    }
    if let Some(h) = &host {
        if let Some(n) = num(h, &["system", "numCores"]) {
            s.info.push(("Núcleos".into(), format!("{n}")));
        }
        if let Some(m) = host_mem {
            s.info.push(("Memoria del host".into(), format!("{:.0} MB", m / MB)));
        }
        if let Ok(v) = h.get_document("os").and_then(|o| o.get_str("name")) {
            s.info.push(("Sistema operativo".into(), v.into()));
        }
    }

    // Role and replication.
    let hello = admin(client, doc! { "hello": 1 }).await.ok();
    let role = hello.as_ref().map(|h| {
        if h.get_str("msg") == Ok("isdbgrid") {
            "mongos (router de sharding)"
        } else if h.get_bool("isWritablePrimary").unwrap_or(false) && h.contains_key("setName") {
            "primario"
        } else if h.get_bool("secondary").unwrap_or(false) {
            "secundario"
        } else if h.get_bool("arbiterOnly").unwrap_or(false) {
            "árbitro"
        } else {
            "servidor independiente"
        }
    });
    if let Some(r) = role {
        s.info.push(("Rol".into(), r.into()));
    }
    if let Some(set) = hello.as_ref().and_then(|h| h.get_str("setName").ok()) {
        s.info.push(("Replica set".into(), set.into()));
        match admin(client, doc! { "replSetGetStatus": 1 }).await {
            Ok(rs) => replication(&mut s, &rs),
            Err(e) => skip(&mut s, "replSetGetStatus", e),
        }
    } else {
        if flavor != Flavor::Ferret {
            s.notes.push("El servidor no es parte de un replica set: no hay retraso de réplica que medir.".into());
        }
    }

    if flavor == Flavor::DocumentDb {
        s.notes.push(
            "Amazon DocumentDB publica el uso de CPU, la memoria, el IOPS y el retraso de réplicas en Amazon CloudWatch; por el protocolo solo se ven serverStatus, currentOp y los tamaños."
                .into(),
        );
    }

    // Operations.
    match admin(client, doc! { "currentOp": 1, "$all": true }).await {
        Ok(r) => current_ops(&mut s, &r),
        Err(e) => skip(&mut s, "currentOp (hace falta el privilegio inprog)", e),
    }

    // Databases and sizes.
    match admin(client, doc! { "listDatabases": 1 }).await {
        Ok(r) => databases(client, &mut s, &r).await,
        Err(e) => skip(&mut s, "listDatabases", e),
    }

    // Time per collection.
    match admin(client, doc! { "top": 1 }).await {
        Ok(r) => top(&mut s, &r),
        Err(e) if flavor == Flavor::Mongo => skip(&mut s, "top", e),
        Err(_) => s.notes.push(format!("{name} no implementa el comando top: no hay tiempos por colección.")),
    }
    Ok(s)
}

fn replication(s: &mut MonitorSnapshot, rs: &Document) {
    let members: Vec<&Document> =
        rs.get_array("members").map(|a| a.iter().filter_map(Bson::as_document).collect()).unwrap_or_default();
    let optime = |m: &Document| match m.get("optimeDate") {
        Some(Bson::DateTime(d)) => Some(d.timestamp_millis() as f64 / 1000.0),
        _ => None,
    };
    let primary = members.iter().find(|m| m.get_str("stateStr") == Ok("PRIMARY")).and_then(|m| optime(m));
    let mut table = MonitorTable::new(
        "replication",
        "Miembros del replica set",
        &["miembro", "estado", "salud", "retraso (s)", "último optime", "ping (ms)", "sincroniza desde"],
    );
    let mut worst: Option<f64> = None;
    for m in &members {
        let lag = match (primary, optime(m)) {
            (Some(p), Some(o)) if m.get_str("stateStr") == Ok("SECONDARY") => Some((p - o).max(0.0)),
            _ => None,
        };
        if let Some(l) = lag {
            worst = Some(worst.map_or(l, |w: f64| w.max(l)));
        }
        let when = match m.get("optimeDate") {
            Some(Bson::DateTime(d)) => d.try_to_rfc3339_string().map(Value::String).unwrap_or(Value::Null),
            _ => Value::Null,
        };
        table.rows.push(vec![
            text(m, "name"),
            text(m, "stateStr"),
            text(m, "health"),
            lag.map(|l| json!(l)).unwrap_or(Value::Null),
            when,
            text(m, "pingMs"),
            text(m, "syncSourceHost"),
        ]);
    }
    s.metrics.push(Metric::new("replication_lag", "Retraso de réplica", "Replicación", MetricUnit::Seconds, worst));
    s.tables.push(table);
}

fn current_ops(s: &mut MonitorSnapshot, r: &Document) {
    let ops: Vec<&Document> =
        r.get_array("inprog").map(|a| a.iter().filter_map(Bson::as_document).collect()).unwrap_or_default();
    let mut sessions = MonitorTable::new(
        "sessions",
        "Sesiones",
        &["opid", "usuario", "base", "cliente", "estado", "duración (s)", "operación actual"],
    );
    let mut queries = MonitorTable::new(
        "queries",
        "Operaciones en curso",
        &["opid", "tipo", "espacio de nombres", "cliente", "aplicación", "duración (s)", "plan", "comando"],
    );
    let mut locks = MonitorTable::new(
        "locks",
        "Operaciones esperando un bloqueo",
        &["opid", "tipo", "espacio de nombres", "duración (s)", "bloqueos", "comando"],
    );
    for op in ops {
        let active = op.get_bool("active").unwrap_or(false);
        let client = op.get_str("client").or_else(|_| op.get_str("client_s")).ok();
        let user = op
            .get_array("effectiveUsers")
            .ok()
            .and_then(|a| a.first())
            .and_then(Bson::as_document)
            .and_then(|u| u.get_str("user").ok())
            .map(|u| Value::String(u.into()))
            .unwrap_or(Value::Null);
        let ns = op.get_str("ns").unwrap_or("");
        let db = ns.split('.').next().filter(|d| !d.is_empty()).map(|d| Value::String(d.into())).unwrap_or(Value::Null);
        let secs = num(op, &["microsecs_running"])
            .map(|us| (us / 1e4).round() / 100.0)
            .or_else(|| num(op, &["secs_running"]))
            .or_else(|| num(op, &["secs_idle"]))
            .map(|v| json!(v))
            .unwrap_or(Value::Null);
        let cmd = op.get("command").filter(|c| c.as_document().is_some_and(|d| !d.is_empty())).map(|c| compact(c, 2000));
        // FerretDB lists sessions without a client address.
        let session = client.is_some() || op.get_str("type") == Ok("idleSession");
        if session && sessions.rows.len() < MAX_ROWS {
            let state = if active {
                "activa"
            } else if op.get_str("type") == Ok("idleSession") {
                "sesión inactiva"
            } else {
                "inactiva"
            };
            sessions.rows.push(vec![
                text(op, "opid"),
                user,
                db,
                client.map(|c| Value::String(c.into())).unwrap_or(Value::Null),
                Value::String(state.into()),
                secs.clone(),
                cmd.clone().filter(|_| active).map(Value::String).unwrap_or(Value::Null),
            ]);
        }
        if active && queries.rows.len() < MAX_ROWS {
            queries.rows.push(vec![
                text(op, "opid"),
                text(op, "op"),
                Value::String(ns.into()),
                client.map(|c| Value::String(c.into())).unwrap_or(Value::Null),
                text(op, "appName"),
                secs.clone(),
                text(op, "planSummary"),
                cmd.clone().map(Value::String).unwrap_or(Value::Null),
            ]);
        }
        if op.get_bool("waitingForLock").unwrap_or(false) && locks.rows.len() < MAX_ROWS {
            locks.rows.push(vec![
                text(op, "opid"),
                text(op, "op"),
                Value::String(ns.into()),
                secs,
                op.get("locks").map(|l| Value::String(compact(l, 500))).unwrap_or(Value::Null),
                cmd.map(Value::String).unwrap_or(Value::Null),
            ]);
        }
    }
    s.tables.push(sessions);
    s.tables.push(queries);
    s.tables.push(locks);
}

async fn databases(client: &Client, s: &mut MonitorSnapshot, list: &Document) {
    let dbs: Vec<&Document> =
        list.get_array("databases").map(|a| a.iter().filter_map(Bson::as_document).collect()).unwrap_or_default();
    let mut table = MonitorTable::new(
        "databases",
        "Bases y tamaños",
        &["base", "colecciones", "documentos", "datos", "almacenamiento", "índices", "en disco"],
    );
    let mut total = list.get("totalSize").and_then(bson_f64);
    let mut summed = 0.0;
    for d in dbs.iter().take(MAX_DBS) {
        let Ok(name) = d.get_str("name") else { continue };
        let on_disk = num(d, &["sizeOnDisk"]);
        summed += on_disk.unwrap_or(0.0);
        let stats = client.database(name).run_command(doc! { "dbStats": 1 }).await.ok().unwrap_or_default();
        let n = |k: &str| num(&stats, &[k]).map(|v| json!(v)).unwrap_or(Value::Null);
        table.rows.push(vec![
            Value::String(name.into()),
            n("collections"),
            n("objects"),
            n("dataSize"),
            n("storageSize"),
            n("indexSize"),
            on_disk.map(|v| json!(v)).unwrap_or(Value::Null),
        ]);
    }
    if dbs.len() > MAX_DBS {
        s.notes.push(format!("Se muestran las primeras {MAX_DBS} de {} bases.", dbs.len()));
    }
    if total.is_none() && summed > 0.0 {
        total = Some(summed);
    }
    s.metrics.push(Metric::new("storage_used", "Espacio usado", "Almacenamiento", MetricUnit::Bytes, total));
    s.tables.push(table);
}

fn top(s: &mut MonitorSnapshot, r: &Document) {
    let Ok(totals) = r.get_document("totals") else { return };
    let mut rows: Vec<(f64, Vec<Value>)> = totals
        .iter()
        .filter(|(ns, _)| ns.as_str() != "note" && !ns.is_empty())
        .filter_map(|(ns, v)| {
            let d = v.as_document()?;
            let ms = |k: &str| num(d, &[k, "time"]).map(|us| us / 1000.0);
            let count = |k: &str| num(d, &[k, "count"]);
            let total = ms("total")?;
            Some((
                total,
                vec![
                    Value::String(ns.clone()),
                    json!(total),
                    count("total").map(|v| json!(v)).unwrap_or(Value::Null),
                    ms("readLock").map(|v| json!(v)).unwrap_or(Value::Null),
                    ms("writeLock").map(|v| json!(v)).unwrap_or(Value::Null),
                    count("queries").map(|v| json!(v)).unwrap_or(Value::Null),
                    count("insert").map(|v| json!(v)).unwrap_or(Value::Null),
                    count("update").map(|v| json!(v)).unwrap_or(Value::Null),
                    count("remove").map(|v| json!(v)).unwrap_or(Value::Null),
                ],
            ))
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut table = MonitorTable::new(
        "top_objects",
        "Colecciones con más actividad (top)",
        &["colección", "tiempo total (ms)", "operaciones", "lectura (ms)", "escritura (ms)", "consultas", "inserciones", "actualizaciones", "borrados"],
    );
    table.rows = rows.into_iter().take(20).map(|(_, r)| r).collect();
    s.tables.push(table);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_by_path() {
        let d = doc! { "a": { "b": 3_i64, "c d": 1.5 }, "x": 2_i32 };
        assert_eq!(num(&d, &["a", "b"]), Some(3.0));
        assert_eq!(num(&d, &["a", "c d"]), Some(1.5));
        assert_eq!(num(&d, &["x"]), Some(2.0));
        assert_eq!(num(&d, &["a", "zz"]), None);
        assert_eq!(sum(&[None, Some(1.0), Some(2.0)]), Some(3.0));
        assert_eq!(sum(&[None, None]), None);
    }

    #[test]
    fn top_sorts_by_time() {
        let r = doc! { "totals": {
            "note": "all times in microseconds",
            "a.x": { "total": { "time": 1000_i64, "count": 1_i64 } },
            "a.y": { "total": { "time": 5000_i64, "count": 2_i64 } },
        } };
        let mut s = MonitorSnapshot::default();
        top(&mut s, &r);
        assert_eq!(s.tables[0].rows.len(), 2);
        assert_eq!(s.tables[0].rows[0][0], json!("a.y"));
        assert_eq!(s.tables[0].rows[0][1], json!(5.0));
    }

    #[test]
    fn replica_lag_against_primary() {
        let t = |ms: i64| Bson::DateTime(mongodb::bson::DateTime::from_millis(ms));
        let rs = doc! { "members": [
            { "name": "a:1", "stateStr": "PRIMARY", "health": 1, "optimeDate": t(10_000) },
            { "name": "b:1", "stateStr": "SECONDARY", "health": 1, "optimeDate": t(7_000) },
        ] };
        let mut s = MonitorSnapshot::default();
        replication(&mut s, &rs);
        assert_eq!(s.metrics[0].value, Some(3.0));
        assert_eq!(s.tables[0].rows.len(), 2);
    }
}

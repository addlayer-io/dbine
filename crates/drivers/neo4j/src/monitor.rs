//! Server monitor per engine. Every part is optional: a query the server
//! rejects (edition, permissions, version) leaves its part out and adds a
//! note, it never fails the snapshot.
//!
//! - Neo4j: JMX through `dbms.queryJmx` (CPU, heap, OS memory, threads, GC,
//!   uptime), `dbms.listConnections`, `SHOW TRANSACTIONS`, `SHOW DATABASES`,
//!   `SHOW SERVERS` (Enterprise), node / relationship counts (count store)
//!   and `dbms.listConfig`.
//! - Memgraph: `SHOW STORAGE INFO` (+ `ON CURRENT DATABASE`), `SHOW
//!   TRANSACTIONS`, `SHOW ACTIVE USERS INFO`, `SHOW CONFIG`, replication,
//!   and `SHOW METRICS INFO` / `SHOW DATABASES` on Enterprise.
//! - Neptune: `/status`, `/openCypher/status` and the graph summary.

use crate::{as_text, cypher, GraphSession};
use dbine_driver::monitor::num;
use dbine_driver::{Error, Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::{Map, Value};

const MAX_ROWS: usize = 200;
const MAX_TEXT: usize = 2000;

fn clip(s: &str) -> Value {
    if s.chars().count() > MAX_TEXT {
        Value::String(format!("{}…", s.chars().take(MAX_TEXT).collect::<String>()))
    } else {
        Value::String(s.to_string())
    }
}

fn f(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => num(s),
        _ => None,
    }
}

/// Whether an error is the server refusing (keep going) or the connection
/// being gone (stop).
fn soft(e: Error, what: &str, notes: &mut Vec<String>) -> Result<()> {
    match e {
        Error::Query(m) | Error::Unsupported(m) | Error::AuthFailed(m) => {
            notes.push(format!("{what}: {m}"));
            Ok(())
        }
        e => Err(e),
    }
}

/// A `SHOW TRANSACTIONS` row of DBine's own transaction (its tx metadata).
fn is_own(r: &Map<String, Value>, tag: &str) -> bool {
    r.get("metaData")
        .and_then(|m| m.get("dbine"))
        .and_then(Value::as_str)
        == Some(tag)
}

/// The highest transaction sequence number of the current database, from
/// `SHOW TRANSACTIONS` rows (`neo4j-transaction-42`). The listing covers
/// every database, and each has its own sequence: mixing them would make
/// the counter jump up and back down (a false restart). The current
/// database is the one of DBine's own transaction, else the session's;
/// with neither, every row counts.
fn tx_sequence(rows: &[Map<String, Value>], tag: &str, session_db: &str) -> Option<f64> {
    let parts = |r: &Map<String, Value>| {
        let id = r.get("transactionId").map(as_text).unwrap_or_default();
        let seq = id.rsplit('-').next().and_then(|n| n.parse::<f64>().ok());
        let db = r
            .get("database")
            .map(as_text)
            .filter(|d| !d.is_empty())
            .or_else(|| id.rsplit_once("-transaction-").map(|(d, _)| d.to_string()));
        (db, seq)
    };
    let current = rows
        .iter()
        .find(|r| is_own(r, tag))
        .and_then(|r| parts(r).0)
        .or_else(|| (!session_db.is_empty()).then(|| session_db.to_string()));
    rows.iter()
        .map(parts)
        .filter(|(db, _)| match (&current, db) {
            (Some(c), Some(d)) => c.eq_ignore_ascii_case(d),
            (Some(_), None) => false,
            (None, _) => true,
        })
        .filter_map(|(_, seq)| seq)
        .reduce(f64::max)
}

/// `PT1.5S`-style or a number of ms → milliseconds.
pub fn duration_ms(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let s = v.as_str()?;
    let t = s.strip_prefix("PT").or_else(|| s.strip_prefix('P'))?;
    let mut total = 0.0;
    let mut n = String::new();
    let mut in_time = s.starts_with("PT");
    for c in t.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' | '.' | '-' => n.push(c),
            u => {
                let x: f64 = n.parse().ok()?;
                n.clear();
                total += x * match (u, in_time) {
                    ('D', _) => 86_400_000.0,
                    ('H', true) => 3_600_000.0,
                    ('M', true) => 60_000.0,
                    ('S', true) => 1_000.0,
                    _ => return None,
                };
            }
        }
    }
    Some(total)
}

/// Memgraph sizes: `66.44MiB`, `255.79KiB`, `0B`, `15.66GiB`.
pub fn size_bytes(s: &str) -> Option<f64> {
    let s = s.trim().trim_matches('"');
    let idx = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (n, unit) = s.split_at(idx);
    let n: f64 = n.trim().parse().ok()?;
    let mul = match unit.trim() {
        "B" => 1.0,
        "KiB" | "KB" => 1024.0,
        "MiB" | "MB" => 1024.0 * 1024.0,
        "GiB" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" | "TB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some(n * mul)
}

// ---------------------------------------------------------------- Neo4j

/// `attributes.X.value` of a JMX bean.
fn jmx_attr<'a>(attrs: &'a Value, name: &str) -> Option<&'a Value> {
    attrs.get(name)?.get("value")
}

async fn jmx(s: &mut GraphSession, bean: &str) -> Result<Vec<Value>> {
    let q = format!(
        "CALL dbms.queryJmx({}) YIELD name, attributes RETURN name, attributes",
        cypher::string(bean)
    );
    let (_, rows) = s.query(&q).await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| r.into_iter().nth(1))
        .collect())
}

pub async fn neo4j(s: &mut GraphSession) -> Result<MonitorSnapshot> {
    let mut snap = MonitorSnapshot::default();
    let mut notes = Vec::new();

    if let Ok(v) = dbine_driver::Session::server_version(s).await {
        snap.info.push(("Versión".into(), v));
    }
    snap.info.push(("Protocolo".into(), s.bolt_server()));

    // JMX: operating system, memory, runtime, threads, GC.
    let (
        mut cpu,
        mut cpu_time,
        mut heap,
        mut heap_max,
        mut os_used,
        mut os_total,
        mut uptime,
        mut threads,
    ) = (None, None, None, None, None, None, None, None);
    let (mut fds, mut fds_max, mut gc_ms, mut gc_count) = (None, None, None::<f64>, None::<f64>);
    match jmx(s, "java.lang:type=OperatingSystem").await {
        Ok(beans) => {
            if let Some(a) = beans.first() {
                let load = f(jmx_attr(a, "CpuLoad")).or_else(|| f(jmx_attr(a, "SystemCpuLoad")));
                cpu = load.filter(|l| *l >= 0.0).map(|l| l * 100.0);
                // Nanoseconds of CPU → seconds × 100, spread over the CPUs
                // the JVM may use: the rate is the % of all of them (100 %
                // is every one busy), never "500 %" for five busy cores.
                // Unknown CPU count: not shown rather than per core.
                let cpus = f(jmx_attr(a, "AvailableProcessors")).filter(|n| *n > 0.0);
                cpu_time = f(jmx_attr(a, "ProcessCpuTime"))
                    .zip(cpus)
                    .map(|(ns, n)| ns / 1e7 / n);
                os_total = f(jmx_attr(a, "TotalMemorySize"))
                    .or_else(|| f(jmx_attr(a, "TotalPhysicalMemorySize")));
                let free = f(jmx_attr(a, "FreeMemorySize"))
                    .or_else(|| f(jmx_attr(a, "FreePhysicalMemorySize")));
                os_used = os_total.zip(free).map(|(t, f)| t - f);
                fds = f(jmx_attr(a, "OpenFileDescriptorCount"));
                fds_max = f(jmx_attr(a, "MaxFileDescriptorCount"));
                if let Some(n) = f(jmx_attr(a, "AvailableProcessors")) {
                    snap.info.push(("Procesadores".into(), n.to_string()));
                }
                if let (Some(n), Some(v)) = (jmx_attr(a, "Name"), jmx_attr(a, "Version")) {
                    snap.info.push((
                        "Sistema operativo".into(),
                        format!("{} {}", as_text(n), as_text(v)),
                    ));
                }
            }
            if let Ok(b) = jmx(s, "java.lang:type=Memory").await {
                if let Some(u) = b
                    .first()
                    .and_then(|a| jmx_attr(a, "HeapMemoryUsage"))
                    .and_then(|v| v.get("properties"))
                {
                    heap = f(u.get("used"));
                    heap_max = f(u.get("max")).filter(|m| *m > 0.0);
                }
            }
            if let Ok(b) = jmx(s, "java.lang:type=Runtime").await {
                if let Some(a) = b.first() {
                    uptime = f(jmx_attr(a, "Uptime")).map(|ms| ms / 1000.0);
                    if let Some(v) = jmx_attr(a, "VmName").zip(jmx_attr(a, "VmVersion")) {
                        snap.info
                            .push(("JVM".into(), format!("{} {}", as_text(v.0), as_text(v.1))));
                    }
                }
            }
            if let Ok(b) = jmx(s, "java.lang:type=Threading").await {
                threads = b.first().and_then(|a| f(jmx_attr(a, "ThreadCount")));
            }
            if let Ok(b) = jmx(s, "java.lang:type=GarbageCollector,*").await {
                for a in &b {
                    if let Some(t) = f(jmx_attr(a, "CollectionTime")) {
                        gc_ms = Some(gc_ms.unwrap_or(0.0) + t);
                    }
                    if let Some(c) = f(jmx_attr(a, "CollectionCount")) {
                        gc_count = Some(gc_count.unwrap_or(0.0) + c);
                    }
                }
            }
        }
        Err(e) => soft(
            e,
            "Sin acceso a dbms.queryJmx (CPU, memoria y tiempo activo no disponibles)",
            &mut notes,
        )?,
    }
    let m = &mut snap.metrics;
    m.push(
        Metric::new("cpu", "CPU del servidor", "CPU", MetricUnit::Percent, cpu)
            .max(Some(100.0))
            .help("Uso de CPU de toda la máquina (o del contenedor) en la que corre Neo4j, según la JVM: incluye otros procesos. 100 % es todos los núcleos ocupados.")
            .source("dbms.queryJmx('java.lang:type=OperatingSystem'): CpuLoad (or SystemCpuLoad) × 100"),
    );
    m.push(
        Metric::new(
            "cpu_time",
            "CPU del proceso",
            "CPU",
            MetricUnit::Percent,
            cpu_time,
        )
        .max(Some(100.0))
        .counter()
        .help("Tiempo de CPU que consume el proceso de Neo4j, como parte de todos los núcleos que la JVM puede usar: 100 % es todos ocupados. Comparado con la CPU del servidor, muestra cuánto de esa carga es de Neo4j.")
        .source("dbms.queryJmx('java.lang:type=OperatingSystem'): ProcessCpuTime ÷ AvailableProcessors"),
    );
    m.push(
        Metric::new(
            "mem_used",
            "Heap de la JVM",
            "Memoria",
            MetricUnit::Bytes,
            heap,
        )
        .max(heap_max)
        .help("Heap de Java en uso, sobre el máximo configurado (server.memory.heap.max_size). Sube y baja con cada recolección; si queda cerca del máximo aun después de recolectar, falta heap.")
        .source("dbms.queryJmx('java.lang:type=Memory'): HeapMemoryUsage.used; max: HeapMemoryUsage.max"),
    );
    m.push(
        Metric::new(
            "os_mem_used",
            "Memoria del sistema",
            "Memoria",
            MetricUnit::Bytes,
            os_used,
        )
        .max(os_total)
        .help("Memoria física ocupada en la máquina (total menos libre), sobre el total, según la JVM. Incluye el page cache de Neo4j, el heap, otros procesos y la caché de archivos del sistema operativo, así que suele estar cerca del total aunque no falte memoria.")
        .source("dbms.queryJmx('java.lang:type=OperatingSystem'): TotalMemorySize − FreeMemorySize (or the *PhysicalMemorySize pair)"),
    );
    m.push(
        Metric::new(
            "gc_time",
            "Tiempo en GC",
            "Memoria",
            MetricUnit::Millis,
            gc_ms,
        )
        .counter()
        .help("Tiempo que la JVM pasó recolectando basura, sumando todos los recolectores. Si se lleva una parte grande de cada segundo, el heap queda chico o hay consultas que reservan mucha memoria.")
        .source("dbms.queryJmx('java.lang:type=GarbageCollector,*'): CollectionTime (sum)"),
    );
    m.push(
        Metric::new(
            "gc_count",
            "Recolecciones de GC",
            "Memoria",
            MetricUnit::Count,
            gc_count,
        )
        .counter()
        .help("Recolecciones de basura de la JVM, de todos los recolectores.")
        .source("dbms.queryJmx('java.lang:type=GarbageCollector,*'): CollectionCount (sum)"),
    );
    m.push(
        Metric::new("threads", "Hilos", "Servidor", MetricUnit::Count, threads)
            .help("Hilos vivos de la JVM. Si crecen sin parar, hay conexiones o tareas que no terminan.")
            .source("dbms.queryJmx('java.lang:type=Threading'): ThreadCount"),
    );
    m.push(
        Metric::new(
            "open_files",
            "Archivos abiertos",
            "Servidor",
            MetricUnit::Count,
            fds,
        )
        .max(fds_max)
        .help("Descriptores de archivo abiertos por el proceso, sobre el límite del sistema. Cerca del límite, Neo4j empieza a fallar al abrir archivos o aceptar conexiones.")
        .source("dbms.queryJmx('java.lang:type=OperatingSystem'): OpenFileDescriptorCount; max: MaxFileDescriptorCount"),
    );
    m.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            uptime,
        )
        .help("Tiempo desde que arrancó la JVM de Neo4j. Si vuelve a cero, se reinició.")
        .source("dbms.queryJmx('java.lang:type=Runtime'): Uptime"),
    );

    // Connections.
    let mut connections = None;
    match s.records("CALL dbms.listConnections() YIELD connectionId, connectTime, connector, username, userAgent, clientAddress").await {
        Ok(rows) => {
            connections = Some(rows.len() as f64);
            let mut t = MonitorTable::new("connections", "Conexiones", &["id", "usuario", "cliente", "aplicación", "conector", "desde"]);
            for r in rows.iter().take(MAX_ROWS) {
                let g = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
                t.rows.push(vec![g("connectionId"), g("username"), g("clientAddress"), g("userAgent"), g("connector"), g("connectTime")]);
            }
            snap.tables.push(t);
        }
        Err(e) => soft(e, "No se pudieron listar las conexiones", &mut notes)?,
    }

    // Transactions: sessions, running queries, locks, and a transaction counter.
    let (mut active, mut waiting, mut locks, mut tx_counter) = (None, None, None, None);
    match s.records("SHOW TRANSACTIONS YIELD *").await {
        Ok(rows) => {
            let mut sessions = MonitorTable::new(
                "sessions",
                "Sesiones",
                &[
                    "id",
                    "usuario",
                    "base",
                    "cliente",
                    "estado",
                    "duración (ms)",
                    "consulta actual",
                ],
            );
            let mut queries = MonitorTable::new(
                "queries",
                "Consultas en curso",
                &[
                    "id",
                    "usuario",
                    "base",
                    "duración (ms)",
                    "CPU (ms)",
                    "espera (ms)",
                    "memoria (bytes)",
                    "page hits",
                    "consulta",
                ],
            );
            let (mut a, mut w, mut l) = (0.0, 0.0, 0.0);
            let max_tx = tx_sequence(&rows, &s.tag, &s.db);
            for r in &rows {
                let g = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
                let status = r.get("status").map(as_text).unwrap_or_default();
                let query = r.get("currentQuery").map(as_text).unwrap_or_default();
                let own = is_own(r, &s.tag);
                if status.starts_with("Blocked") {
                    w += 1.0;
                }
                l += f(r.get("activeLockCount")).unwrap_or(0.0);
                if !query.is_empty() && !own {
                    a += 1.0;
                }
                let elapsed = r.get("elapsedTime").and_then(duration_ms);
                if sessions.rows.len() < MAX_ROWS {
                    sessions.rows.push(vec![
                        g("transactionId"),
                        g("username"),
                        g("database"),
                        g("clientAddress"),
                        Value::String(status.clone()),
                        elapsed.map(Value::from).unwrap_or(Value::Null),
                        clip(&query),
                    ]);
                }
                if !query.is_empty() && queries.rows.len() < MAX_ROWS {
                    let ms = |k: &str| {
                        r.get(k)
                            .and_then(duration_ms)
                            .map(Value::from)
                            .unwrap_or(Value::Null)
                    };
                    queries.rows.push(vec![
                        g("currentQueryId"),
                        g("username"),
                        g("database"),
                        ms("currentQueryElapsedTime"),
                        ms("currentQueryCpuTime"),
                        ms("currentQueryWaitTime"),
                        g("currentQueryAllocatedBytes"),
                        g("currentQueryPageHits"),
                        clip(&query),
                    ]);
                }
            }
            (active, waiting, locks, tx_counter) = (Some(a), Some(w), Some(l), max_tx);
            snap.tables.push(sessions);
            snap.tables.push(queries);
        }
        Err(e) => soft(
            e,
            "No se pudieron listar las transacciones (SHOW TRANSACTIONS)",
            &mut notes,
        )?,
    }
    let m = &mut snap.metrics;
    m.push(
        Metric::new(
            "connections",
            "Conexiones",
            "Conexiones",
            MetricUnit::Count,
            connections,
        )
        .help("Conexiones abiertas al servidor, por Bolt y por HTTP.")
        .source("CALL dbms.listConnections() (rows)"),
    );
    m.push(
        Metric::new(
            "active_sessions",
            "Consultas en ejecución",
            "Conexiones",
            MetricUnit::Count,
            active,
        )
        .help("Transacciones que están ejecutando una consulta ahora, sin contar la de DBine. Si se acumulan, hay consultas lentas o el servidor está saturado.")
        .source("SHOW TRANSACTIONS: rows with a currentQuery, without DBine's own"),
    );
    m.push(
        Metric::new(
            "transactions",
            "Transacciones iniciadas (base actual)",
            "Actividad",
            MetricUnit::Count,
            tx_counter,
        )
        .counter()
        .help("Transacciones iniciadas en la base actual, de lectura y de escritura. Se toma del número de la transacción más reciente de esa base que muestra SHOW TRANSACTIONS, así que es aproximado.")
        .source("SHOW TRANSACTIONS: transactionId (highest sequence number among the rows whose database is DBine's own transaction's)"),
    );
    m.push(
        Metric::new(
            "locks_waiting",
            "Transacciones bloqueadas",
            "Bloqueos",
            MetricUnit::Count,
            waiting,
        )
        .help("Transacciones que esperan un lock que tiene otra. Si no vuelve a cero, hay transacciones largas o escrituras que chocan sobre los mismos nodos.")
        .source("SHOW TRANSACTIONS: rows whose status starts with 'Blocked'"),
    );
    m.push(
        Metric::new(
            "locks_held",
            "Bloqueos tomados",
            "Bloqueos",
            MetricUnit::Count,
            locks,
        )
        .help("Locks que tienen ahora las transacciones abiertas, sumados. Muchos y sostenidos indican transacciones grandes o largas.")
        .source("SHOW TRANSACTIONS: activeLockCount (sum)"),
    );

    // Graph size (count store: constant time).
    let nodes = s
        .strings("MATCH (n) RETURN count(n)")
        .await
        .ok()
        .and_then(|v| v.first().and_then(|x| num(x)));
    let rels = s
        .strings("MATCH ()-[r]->() RETURN count(r)")
        .await
        .ok()
        .and_then(|v| v.first().and_then(|x| num(x)));
    snap.metrics.push(
        Metric::new("nodes", "Nodos", "Almacenamiento", MetricUnit::Count, nodes)
            .help("Nodos del grafo en la base actual. Neo4j los cuenta con su almacén de conteos, sin recorrer el grafo.")
            .source("MATCH (n) RETURN count(n)"),
    );
    snap.metrics.push(
        Metric::new(
            "relationships",
            "Relaciones",
            "Almacenamiento",
            MetricUnit::Count,
            rels,
        )
        .help("Relaciones del grafo en la base actual. Neo4j las cuenta con su almacén de conteos, sin recorrer el grafo.")
        .source("MATCH ()-[r]->() RETURN count(r)"),
    );

    // Databases.
    match s.query_on("SHOW DATABASES YIELD name, type, currentStatus, role, address, default, home, store, lastCommittedTxn, replicationLag", Some("system")).await {
        Ok((cols, rows)) => {
            let mut t = MonitorTable::new(
                "databases",
                "Bases de datos",
                &["nombre", "tipo", "estado", "rol", "dirección", "predeterminada", "formato", "última transacción", "retraso de réplica"],
            );
            let idx = |c: &str| cols.iter().position(|x| x == c);
            let order = ["name", "type", "currentStatus", "role", "address", "default", "store", "lastCommittedTxn", "replicationLag"];
            let mut lag: Option<f64> = None;
            for r in rows.iter().take(MAX_ROWS) {
                t.rows.push(order.iter().map(|c| idx(c).and_then(|i| r.get(i)).cloned().unwrap_or(Value::Null)).collect());
                if let Some(l) = idx("replicationLag").and_then(|i| r.get(i)).and_then(Value::as_f64) {
                    lag = Some(lag.map_or(l, |x| x.max(l)));
                }
            }
            snap.tables.push(t);
            // lastCommittedTxn / replicationLag are counts of transactions (Enterprise clusters).
            if lag.is_some_and(|l| l > 0.0) {
                snap.metrics.push(
                    Metric::new("replication_lag_tx", "Retraso de réplica (transacciones)", "Replicación", MetricUnit::Count, lag)
                        .help("Transacciones que le faltan aplicar a la réplica más atrasada de alguna base del cluster. Si crece, esa réplica no da abasto o perdió la conexión.")
                        .source("SHOW DATABASES: replicationLag (maximum)"),
                );
            }
        }
        Err(e) => soft(e, "No se pudieron listar las bases", &mut notes)?,
    }

    // Cluster servers (Enterprise).
    match s
        .query_on(
            "SHOW SERVERS YIELD name, address, state, health, hosting",
            Some("system"),
        )
        .await
    {
        Ok((cols, rows)) => {
            let mut t = MonitorTable::new(
                "nodes",
                "Servidores del cluster",
                &["nombre", "dirección", "estado", "salud", "bases"],
            );
            for r in rows.iter().take(MAX_ROWS) {
                t.rows.push(
                    ["name", "address", "state", "health", "hosting"]
                        .iter()
                        .map(|c| {
                            cols.iter()
                                .position(|x| x == c)
                                .and_then(|i| r.get(i))
                                .map(crate::value::cell)
                                .unwrap_or(Value::Null)
                        })
                        .collect(),
                );
            }
            snap.tables.push(t);
        }
        Err(Error::Query(_)) => notes
            .push("SHOW SERVERS (servidores del cluster) solo existe en Neo4j Enterprise.".into()),
        Err(e) => return Err(e),
    }

    // Configuration.
    const KEYS: &[(&str, &str)] = &[
        ("server.memory.heap.max_size", "Heap máximo"),
        ("server.memory.pagecache.size", "Page cache"),
        (
            "db.memory.transaction.total.max",
            "Memoria máx. de transacciones",
        ),
        ("db.transaction.timeout", "Timeout de transacción"),
        ("server.bolt.thread_pool_max_size", "Hilos Bolt máx."),
        ("initial.dbms.default_database", "Base predeterminada"),
    ];
    if let Ok(rows) = s.records("CALL dbms.listConfig() YIELD name, value").await {
        for (k, label) in KEYS {
            if let Some(v) = rows
                .iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(k))
                .and_then(|r| r.get("value"))
            {
                let v = as_text(v);
                snap.info.push((
                    label.to_string(),
                    if v.is_empty() {
                        "(automático)".into()
                    } else {
                        v
                    },
                ));
            }
        }
    }

    notes.push("La tasa de aciertos del page cache, las consultas por segundo y el tamaño en disco solo se publican como métricas de Neo4j Enterprise (Prometheus/CSV), no por Cypher.".into());
    notes.push("«Transacciones iniciadas» se toma del número de transacción de la base actual: cuenta lecturas y escrituras.".into());
    snap.notes = notes;
    Ok(snap)
}

// ---------------------------------------------------------------- Memgraph

async fn key_values(s: &mut GraphSession, q: &str) -> Result<Map<String, Value>> {
    let (_, rows) = s.query(q).await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let mut it = r.into_iter();
            Some((as_text(&it.next()?), it.next().unwrap_or(Value::Null)))
        })
        .collect())
}

pub async fn memgraph(s: &mut GraphSession) -> Result<MonitorSnapshot> {
    let mut snap = MonitorSnapshot::default();
    let mut notes = Vec::new();
    if let Ok(v) = dbine_driver::Session::server_version(s).await {
        snap.info.push(("Versión".into(), v));
    }
    snap.info.push(("Protocolo".into(), s.bolt_server()));

    let mut kv = match key_values(s, "SHOW STORAGE INFO").await {
        Ok(kv) => kv,
        Err(e) => {
            soft(e, "SHOW STORAGE INFO", &mut notes)?;
            Map::new()
        }
    };
    // Memgraph 3 moved the per-database figures here.
    if let Ok(db) = key_values(s, "SHOW STORAGE INFO ON CURRENT DATABASE").await {
        kv.extend(db);
    }
    let size = |k: &str| kv.get(k).and_then(|v| size_bytes(&as_text(v)));
    let n = |k: &str| f(kv.get(k));
    let limit = size("memory_limit").or_else(|| size("tenant_memory_limit"));
    let m = &mut snap.metrics;
    m.push(
        Metric::new(
            "mem_used",
            "Memoria residente",
            "Memoria",
            MetricUnit::Bytes,
            size("memory_res"),
        )
        .max(limit)
        .help("Memoria física (RSS) que ocupa el proceso de Memgraph, sobre su límite de memoria (--memory-limit). Al llegar al límite, Memgraph rechaza lo que pide más memoria.")
        .source("SHOW STORAGE INFO: memory_res; max: memory_limit (else ON CURRENT DATABASE: tenant_memory_limit)"),
    );
    m.push(
        Metric::new(
            "mem_tracked",
            "Memoria contabilizada",
            "Memoria",
            MetricUnit::Bytes,
            size("memory_tracked").or_else(|| size("memory_usage")),
        )
        .max(limit)
        .help("Memoria que Memgraph contabiliza en su asignador (grafo, índices y consultas): es la que se compara con el límite de memoria.")
        .source("SHOW STORAGE INFO: memory_tracked (or memory_usage); max: memory_limit (else ON CURRENT DATABASE: tenant_memory_limit)"),
    );
    m.push(
        Metric::new(
            "mem_peak",
            "Pico de memoria",
            "Memoria",
            MetricUnit::Bytes,
            size("peak_memory_res"),
        )
        .help("Memoria residente más alta que tuvo Memgraph desde que arrancó.")
        .source("SHOW STORAGE INFO: peak_memory_res"),
    );
    m.push(
        Metric::new(
            "graph_mem",
            "Memoria del grafo",
            "Memoria",
            MetricUnit::Bytes,
            size("graph_memory_tracked"),
        )
        .help("Memoria que ocupan los nodos, las relaciones, sus propiedades y los índices de la base actual.")
        .source("SHOW STORAGE INFO ON CURRENT DATABASE: graph_memory_tracked (else SHOW STORAGE INFO)"),
    );
    m.push(
        Metric::new(
            "query_mem",
            "Memoria de consultas",
            "Memoria",
            MetricUnit::Bytes,
            size("query_memory_tracked"),
        )
        .help("Memoria que usan ahora las consultas en ejecución de la base actual.")
        .source(
            "SHOW STORAGE INFO ON CURRENT DATABASE: query_memory_tracked (else SHOW STORAGE INFO)",
        ),
    );
    m.push(
        Metric::new(
            "storage_used",
            "Espacio en disco",
            "Almacenamiento",
            MetricUnit::Bytes,
            size("disk_usage"),
        )
        .help("Espacio en disco del directorio de datos de la base: snapshots y registro de escrituras (WAL).")
        .source("SHOW STORAGE INFO ON CURRENT DATABASE: disk_usage (else SHOW STORAGE INFO)"),
    );
    m.push(
        Metric::new(
            "nodes",
            "Nodos",
            "Almacenamiento",
            MetricUnit::Count,
            n("vertex_count"),
        )
        .help("Nodos del grafo en la base actual.")
        .source(
            "SHOW STORAGE INFO ON CURRENT DATABASE: vertex_count (Memgraph 2: SHOW STORAGE INFO)",
        ),
    );
    m.push(
        Metric::new(
            "relationships",
            "Relaciones",
            "Almacenamiento",
            MetricUnit::Count,
            n("edge_count"),
        )
        .help("Relaciones del grafo en la base actual.")
        .source(
            "SHOW STORAGE INFO ON CURRENT DATABASE: edge_count (Memgraph 2: SHOW STORAGE INFO)",
        ),
    );
    m.push(
        Metric::new(
            "unreleased_deltas",
            "Deltas sin liberar",
            "Almacenamiento",
            MetricUnit::Count,
            n("unreleased_delta_objects"),
        )
        .help("Versiones anteriores de objetos (deltas de MVCC) que el recolector todavía no liberó. Si crece sin parar, hay transacciones largas que impiden liberarlas.")
        .source("SHOW STORAGE INFO ON CURRENT DATABASE: unreleased_delta_objects (Memgraph 2: SHOW STORAGE INFO)"),
    );
    for (k, label) in [
        ("storage_mode", "Modo de almacenamiento"),
        ("global_storage_mode", "Modo de almacenamiento"),
        ("global_isolation_level", "Aislamiento"),
        ("storage_isolation_level", "Aislamiento"),
        ("memory_limit", "Límite de memoria"),
        ("name", "Base actual"),
        ("health", "Salud"),
    ] {
        if let Some(v) = kv.get(k) {
            if !snap.info.iter().any(|(l, _)| l == label) {
                snap.info.push((label.to_string(), as_text(v)));
            }
        }
    }

    // Transactions.
    let mut active = None;
    match s.records("SHOW TRANSACTIONS").await {
        Ok(rows) => {
            let mut t = MonitorTable::new(
                "sessions",
                "Transacciones",
                &[
                    "id",
                    "usuario",
                    "estado",
                    "inicio",
                    "duración (ms)",
                    "metadatos",
                    "consulta actual",
                ],
            );
            let mut a = 0.0;
            for r in &rows {
                let own = r
                    .get("metadata")
                    .and_then(|m| m.get("dbine"))
                    .and_then(Value::as_str)
                    == Some(s.tag.as_str());
                let query = r
                    .get("query")
                    .map(|q| match q {
                        Value::Array(a) => a.iter().map(as_text).collect::<Vec<_>>().join("; "),
                        other => as_text(other),
                    })
                    .unwrap_or_default();
                if !own {
                    a += 1.0;
                }
                if t.rows.len() < MAX_ROWS {
                    let g = |k: &str| r.get(k).map(crate::value::cell).unwrap_or(Value::Null);
                    t.rows.push(vec![
                        g("transaction_id"),
                        g("username"),
                        g("status"),
                        g("start_time"),
                        g("elapsed_ms"),
                        g("metadata"),
                        clip(&query),
                    ]);
                }
            }
            active = Some(a);
            snap.tables.push(t);
        }
        Err(e) => soft(e, "SHOW TRANSACTIONS", &mut notes)?,
    }
    snap.metrics.push(
        Metric::new(
            "active_sessions",
            "Transacciones activas",
            "Conexiones",
            MetricUnit::Count,
            active,
        )
        .help("Transacciones abiertas ahora, sin contar la de DBine. Si se acumulan, hay consultas lentas o transacciones que no se cierran.")
        .source("SHOW TRANSACTIONS: rows without DBine's own"),
    );

    // Logged-in sessions.
    match s.records("SHOW ACTIVE USERS INFO").await {
        Ok(rows) => {
            snap.metrics.push(
                Metric::new(
                    "connections",
                    "Sesiones abiertas",
                    "Conexiones",
                    MetricUnit::Count,
                    Some(rows.len() as f64),
                )
                .help("Sesiones de usuarios abiertas en Memgraph.")
                .source("SHOW ACTIVE USERS INFO (rows)"),
            );
            let mut t = MonitorTable::new(
                "connections",
                "Sesiones abiertas",
                &["usuario", "sesión", "inicio"],
            );
            for r in rows.iter().take(MAX_ROWS) {
                t.rows.push(r.values().map(crate::value::cell).collect());
            }
            snap.tables.push(t);
        }
        Err(e) => soft(e, "SHOW ACTIVE USERS INFO", &mut notes)?,
    }

    // Enterprise metrics.
    match s.records("SHOW METRICS INFO").await {
        Ok(rows) => {
            let mut t = MonitorTable::new("metrics", "Métricas", &["nombre", "tipo", "métrica", "valor"]);
            let get = |name: &str| {
                rows.iter().find(|r| r.get("name").and_then(Value::as_str) == Some(name)).and_then(|r| f(r.get("value")))
            };
            let tx = get("CommitedTransactions").or_else(|| get("CommittedTransactions"));
            let rb = get("RollbackedTransactions").unwrap_or(0.0);
            snap.metrics.push(
                Metric::new("transactions", "Transacciones", "Actividad", MetricUnit::Count, tx.map(|t| t + rb))
                    .counter()
                    .help("Transacciones terminadas, confirmadas o deshechas.")
                    .source("SHOW METRICS INFO: CommitedTransactions + RollbackedTransactions"),
            );
            snap.metrics.push(
                Metric::new("queries", "Consultas", "Actividad", MetricUnit::Count, get("SuccessfulQuery").map(|q| q + get("FailedQuery").unwrap_or(0.0)))
                    .counter()
                    .help("Consultas ejecutadas, con éxito o con error.")
                    .source("SHOW METRICS INFO: SuccessfulQuery + FailedQuery"),
            );
            if let Some(c) = get("ActiveBoltSessions").or_else(|| get("ActiveSessions")) {
                snap.metrics.retain(|m| m.key != "connections");
                snap.metrics.push(
                    Metric::new("connections", "Sesiones Bolt", "Conexiones", MetricUnit::Count, Some(c))
                        .help("Sesiones Bolt abiertas en Memgraph.")
                        .source("SHOW METRICS INFO: ActiveBoltSessions (or ActiveSessions)"),
                );
            }
            for r in rows.iter().take(MAX_ROWS) {
                t.rows.push(r.values().map(crate::value::cell).collect());
            }
            snap.tables.push(t);
        }
        Err(Error::Query(_)) => notes.push("Las métricas de actividad (consultas y transacciones por segundo) requieren Memgraph Enterprise (SHOW METRICS INFO).".into()),
        Err(e) => return Err(e),
    }

    // Databases (Enterprise multi-tenancy).
    if let Ok(rows) = s.records("SHOW DATABASES").await {
        let mut t = MonitorTable::new("databases", "Bases de datos", &["nombre"]);
        for r in rows.iter().take(MAX_ROWS) {
            t.rows
                .push(vec![r.values().next().cloned().unwrap_or(Value::Null)]);
        }
        snap.tables.push(t);
    }

    // Replication.
    if let Ok(v) = s.strings("SHOW REPLICATION ROLE").await {
        if let Some(role) = v.first() {
            snap.info.push(("Rol de replicación".into(), role.clone()));
            if role.eq_ignore_ascii_case("main") {
                if let Ok(rows) = s.records("SHOW REPLICAS").await {
                    if !rows.is_empty() {
                        let cols: Vec<String> = rows[0].keys().cloned().collect();
                        let refs: Vec<&str> = cols.iter().map(String::as_str).collect();
                        let mut t = MonitorTable::new("replication", "Réplicas", &refs);
                        for r in rows.iter().take(MAX_ROWS) {
                            t.rows.push(r.values().map(crate::value::cell).collect());
                        }
                        snap.tables.push(t);
                    }
                }
            }
        }
    }

    // Configuration.
    if let Ok(rows) = s.records("SHOW CONFIG").await {
        for (k, label) in [
            ("bolt_num_workers", "Hilos Bolt"),
            ("memory_limit", "Límite de memoria (MiB)"),
            ("query_execution_timeout_sec", "Timeout de consulta (s)"),
            ("storage_gc_cycle_sec", "Ciclo de GC (s)"),
            ("storage_snapshot_interval_sec", "Intervalo de snapshot (s)"),
            ("log_level", "Nivel de log"),
        ] {
            if let Some(v) = rows
                .iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(k))
                .and_then(|r| r.get("current_value"))
            {
                snap.info.push((label.to_string(), as_text(v)));
            }
        }
    }

    notes.push("Memgraph no expone el uso de CPU ni el tiempo activo por Cypher.".into());
    snap.notes = notes;
    Ok(snap)
}

// ---------------------------------------------------------------- Neptune

/// `/status` `startTime`: "Tue Nov 12 18:42:11 UTC 2024".
pub fn neptune_start(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%a %b %d %H:%M:%S UTC %Y")
        .ok()
        .map(|d| d.and_utc())
}

pub async fn neptune(s: &mut GraphSession) -> Result<MonitorSnapshot> {
    let mut snap = MonitorSnapshot::default();
    let mut notes = Vec::new();
    let c = s.neptune_client().expect("neptune");
    let st = c.json("/status").await?;
    let g = |p: &[&str]| {
        let mut v = &st;
        for k in p {
            v = v.get(k)?;
        }
        Some(as_text(v))
    };
    for (path, label) in [
        (&["dbEngineVersion"][..], "Versión del motor"),
        (&["role"][..], "Rol"),
        (&["status"][..], "Estado"),
        (&["dfeQueryEngine"][..], "Motor DFE"),
        (&["opencypher", "version"][..], "openCypher"),
        (&["gremlin", "version"][..], "Gremlin"),
        (&["startTime"][..], "Inicio"),
    ] {
        if let Some(v) = g(path).filter(|v| !v.is_empty()) {
            snap.info.push((label.to_string(), v));
        }
    }
    if let Some(settings) = st.get("settings").and_then(Value::as_object) {
        for (k, v) in settings {
            snap.info.push((k.clone(), as_text(v)));
        }
    }
    let uptime = st
        .get("startTime")
        .and_then(Value::as_str)
        .and_then(neptune_start)
        .map(|t| (chrono::Utc::now() - t).num_seconds() as f64);
    snap.metrics.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            uptime,
        )
        .help("Tiempo desde que arrancó la instancia de Neptune a la que se conecta DBine. Si vuelve a cero, se reinició.")
        .source("GET /status: startTime"),
    );

    match c.json("/openCypher/status").await {
        Ok(q) => {
            let accepted = f(q.get("acceptedQueryCount"));
            let running = f(q.get("runningQueryCount"));
            // acceptedQueryCount is the queries accepted and not finished
            // yet (queued or running), not a running total: a gauge.
            snap.metrics.push(
                Metric::new(
                    "queries",
                    "Consultas aceptadas",
                    "Actividad",
                    MetricUnit::Count,
                    accepted,
                )
                .help("Consultas openCypher aceptadas que todavía no terminaron: las que esperan en cola más las que se ejecutan.")
                .source("GET /openCypher/status: acceptedQueryCount"),
            );
            snap.metrics.push(
                Metric::new(
                    "active_sessions",
                    "Consultas en ejecución",
                    "Conexiones",
                    MetricUnit::Count,
                    running,
                )
                .help("Consultas openCypher ejecutándose ahora. Si hay muchas más aceptadas que en ejecución, la instancia está saturada y las demás esperan en cola.")
                .source("GET /openCypher/status: runningQueryCount"),
            );
            let mut t = MonitorTable::new(
                "queries",
                "Consultas en curso",
                &[
                    "id",
                    "espera (ms)",
                    "duración (ms)",
                    "cancelada",
                    "consulta",
                ],
            );
            for x in q
                .get("queries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .take(MAX_ROWS)
            {
                let ev = x.get("queryEvalStats");
                let e = |k: &str| ev.and_then(|e| e.get(k)).cloned().unwrap_or(Value::Null);
                t.rows.push(vec![
                    x.get("queryId").cloned().unwrap_or(Value::Null),
                    e("waited"),
                    e("elapsed"),
                    e("cancelled"),
                    clip(&x.get("queryString").map(as_text).unwrap_or_default()),
                ]);
            }
            snap.tables.push(t);
        }
        Err(e) => soft(e, "/openCypher/status", &mut notes)?,
    }

    match c.json("/propertygraph/statistics/summary").await {
        Ok(v) => {
            let gs = v.get("payload").and_then(|p| p.get("graphSummary"));
            let n = |k: &str| f(gs.and_then(|g| g.get(k)));
            snap.metrics.push(
                Metric::new("nodes", "Nodos", "Almacenamiento", MetricUnit::Count, n("numNodes"))
                    .help("Nodos del grafo según el resumen de estadísticas de Neptune, que se recalcula cada tanto y no en cada escritura.")
                    .source("GET /propertygraph/statistics/summary: payload.graphSummary.numNodes"),
            );
            snap.metrics.push(
                Metric::new("relationships", "Relaciones", "Almacenamiento", MetricUnit::Count, n("numEdges"))
                    .help("Relaciones (aristas) del grafo según el resumen de estadísticas de Neptune, que se recalcula cada tanto y no en cada escritura.")
                    .source("GET /propertygraph/statistics/summary: payload.graphSummary.numEdges"),
            );
            snap.metrics.push(
                Metric::new("node_properties", "Propiedades de nodos", "Almacenamiento", MetricUnit::Count, n("numNodeProperties"))
                    .help("Propiedades de los nodos del grafo según el resumen de estadísticas de Neptune.")
                    .source("GET /propertygraph/statistics/summary: payload.graphSummary.numNodeProperties"),
            );
        }
        Err(_) => notes.push("Las cantidades de nodos y relaciones salen de las estadísticas de Neptune (DFE), que están apagadas o sin calcular en este cluster.".into()),
    }

    notes.push("Neptune no expone CPU, memoria, conexiones ni espacio usado por su API de datos: están en CloudWatch (CPUUtilization, FreeableMemory, VolumeBytesUsed…).".into());
    snap.notes = notes;
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parsers() {
        assert_eq!(duration_ms(&json!("PT0.045S")), Some(45.0));
        assert_eq!(duration_ms(&json!("PT1M2S")), Some(62_000.0));
        assert_eq!(duration_ms(&json!("P1DT1H")), Some(90_000_000.0));
        assert_eq!(duration_ms(&json!(12)), Some(12.0));
        assert_eq!(size_bytes("\"66.5MiB\""), Some(66.5 * 1024.0 * 1024.0));
        assert_eq!(size_bytes("0B"), Some(0.0));
        assert_eq!(size_bytes("unlimited"), None);
        assert!(neptune_start("Tue Nov 12 18:42:11 UTC 2024").is_some());
    }

    /// The `(…)` that opens at `open`: the index after its `)`. Strings and
    /// char literals don't count.
    fn close(b: &[u8], open: usize) -> usize {
        let (mut i, mut depth) = (open, 0);
        while i < b.len() {
            match b[i] {
                b'"' => {
                    i += 1;
                    while b[i] != b'"' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                }
                b'\'' if b.get(i + 2) == Some(&b'\'') => i += 2,
                b'\'' if b.get(i + 1) == Some(&b'\\') => i += 3,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        panic!("unbalanced call at {open}");
    }

    /// The `.method(arguments)` chained to each `Metric::new(…)` of `code`,
    /// with the line it starts on.
    fn metric_chains(code: &str) -> Vec<(usize, Vec<(String, String)>)> {
        let b = code.as_bytes();
        let mut out = Vec::new();
        for (at, _) in code.match_indices("Metric::new(") {
            let mut i = close(b, at + "Metric::new".len());
            let mut calls = Vec::new();
            loop {
                while b[i].is_ascii_whitespace() {
                    i += 1;
                }
                if b[i] != b'.' {
                    break;
                }
                let name_end = i
                    + 1
                    + code[i + 1..]
                        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .unwrap();
                let end = close(b, name_end);
                calls.push((
                    code[i + 1..name_end].to_string(),
                    code[name_end + 1..end - 1].trim().to_string(),
                ));
                i = end;
            }
            out.push((code[..at].lines().count(), calls));
        }
        out
    }

    /// The snapshots (Neo4j, Memgraph, Neptune) are built from live queries
    /// only, so no fixture reaches their metrics: every `Metric::new` of the
    /// module is checked to chain a non-empty `.help(…)` and `.source(…)`.
    #[test]
    fn every_metric_has_help_and_source() {
        // The scanner sees through strings, char literals and nested calls.
        let probe = metric_chains(
            "m.push(Metric::new(\"k\", \"(p99\", f(')')).max(Some(1.0))\n.help(\"a \\\"b\\\"\"));",
        );
        let names: Vec<&str> = probe[0].1.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(names, ["max", "help"]);
        let src = include_str!("monitor.rs");
        let code = &src[..src.find("#[cfg(test)]").unwrap()];
        let chains = metric_chains(code);
        assert!(chains.len() >= 35, "{} metrics found", chains.len());
        for (line, calls) in chains {
            for method in ["help", "source"] {
                assert!(
                    calls
                        .iter()
                        .any(|(m, arg)| m == method && !arg.is_empty() && arg != "\"\""),
                    "the metric at line {line} has no {method}"
                );
            }
        }
    }

    /// Neptune's accepted queries are the ones not finished yet, a gauge;
    /// the JVM's process CPU is a share of every CPU it may use.
    #[test]
    fn gauges_and_cpu_shares() {
        let src = include_str!("monitor.rs");
        let code = &src[..src.find("#[cfg(test)]").unwrap()];
        let chains = metric_chains(code);
        let calls = |source: &str| {
            chains
                .iter()
                .map(|(_, c)| c)
                .find(|c| c.iter().any(|(m, a)| m == "source" && a.contains(source)))
                .unwrap()
        };
        let has = |c: &Vec<(String, String)>, m: &str| c.iter().any(|(x, _)| x == m);
        assert!(!has(calls("acceptedQueryCount"), "counter"));
        let cpu = calls("ProcessCpuTime");
        assert!(has(cpu, "counter"));
        assert!(cpu.iter().any(|(m, a)| m == "max" && a == "Some(100.0)"));
    }

    /// The transaction counter follows the current database's sequence:
    /// another database's higher number neither lifts it nor, when that
    /// transaction ends, drops it (a false restart).
    #[test]
    fn transaction_sequence_is_the_current_database() {
        let row = |v: serde_json::Value| v.as_object().unwrap().clone();
        let rows = vec![
            row(
                json!({"transactionId": "neo4j-transaction-40", "database": "neo4j",
                "metaData": {"dbine": "me"}}),
            ),
            row(json!({"transactionId": "neo4j-transaction-38", "database": "neo4j"})),
            row(json!({"transactionId": "sales-transaction-900", "database": "sales"})),
            row(json!({"transactionId": "system-transaction-7"})),
        ];
        assert_eq!(tx_sequence(&rows, "me", ""), Some(40.0));
        // Our row missing: the session's database, prefix as a fallback.
        assert_eq!(tx_sequence(&rows[1..], "me", "SALES"), Some(900.0));
        assert_eq!(tx_sequence(&rows[1..], "me", "system"), Some(7.0));
        // Neither known: every row, as before.
        assert_eq!(tx_sequence(&rows[1..], "me", ""), Some(900.0));
        assert_eq!(tx_sequence(&[], "me", "neo4j"), None);
    }
}

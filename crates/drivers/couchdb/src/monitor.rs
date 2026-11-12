//! `Session::monitor` for CouchDB: `/_node/_local/_stats` (requests,
//! reads, writes, request time, HTTP errors), `/_node/_local/_system`
//! (Erlang VM memory, run queue, processes, I/O, B-tree cache),
//! `/_active_tasks`, `/_scheduler/jobs`, `/_up`, `/_membership`,
//! `/_dbs_info` (sizes) and the node's configuration. An endpoint the
//! user can't read (most need the server admin role) leaves a note.

use crate::CouchSession;
use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use reqwest::Method;
use serde_json::{json, Value};

const MAX_ROWS: usize = 200;
const MAX_DBS: usize = 100;

/// A number at `path` (`value` objects of `_stats` are unwrapped).
fn at(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    match cur {
        Value::Number(n) => n.as_f64(),
        Value::Object(o) => o.get("value").and_then(Value::as_f64),
        _ => None,
    }
}

fn sum(values: &[Option<f64>]) -> Option<f64> {
    let v: Vec<f64> = values.iter().flatten().copied().collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

fn cell(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Number(n)) => Value::Number(n.clone()),
        Some(Value::Bool(b)) => Value::Bool(*b),
        Some(other) => Value::String(other.to_string()),
    }
}

/// Unix seconds → `YYYY-MM-DD HH:MM:SS` (UTC).
fn when(v: Option<&Value>) -> Value {
    v.and_then(Value::as_i64)
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S").to_string()))
        .unwrap_or_else(|| cell(v))
}

pub(crate) fn from_stats(s: &mut MonitorSnapshot, st: &Value) {
    let c = |p: &[&str]| at(st, &[&["couchdb"], p].concat());
    s.metrics.push(
        Metric::new(
            "queries",
            "Pedidos HTTP",
            "Actividad",
            MetricUnit::Count,
            c(&["httpd", "requests"]),
        )
        .counter()
        .help("Pedidos HTTP que atendió este nodo, de cualquier tipo: lecturas, escrituras, vistas, _changes y administración.")
        .source("GET /_node/_local/_stats: couchdb.httpd.requests"),
    );
    s.metrics.push(
        Metric::new(
            "rows_read",
            "Lecturas de documentos",
            "Actividad",
            MetricUnit::Count,
            c(&["database_reads"]),
        )
        .counter()
        .help("Veces que este nodo leyó un documento de una base.")
        .source("GET /_node/_local/_stats: couchdb.database_reads"),
    );
    s.metrics.push(
        Metric::new(
            "rows_written",
            "Escrituras de documentos",
            "Actividad",
            MetricUnit::Count,
            c(&["database_writes"]),
        )
        .counter()
        .help("Cambios que este nodo escribió en las bases: cada documento creado, actualizado o borrado.")
        .source("GET /_node/_local/_stats: couchdb.database_writes"),
    );
    if let Some(v) = c(&["document_inserts"]) {
        s.metrics.push(
            Metric::new(
                "document_inserts",
                "Documentos nuevos",
                "Actividad",
                MetricUnit::Count,
                Some(v),
            )
            .counter()
            .help("Documentos creados en este nodo; las actualizaciones no cuentan.")
            .source("GET /_node/_local/_stats: couchdb.document_inserts"),
        );
    }
    if let Some(v) = c(&["httpd", "view_reads"]) {
        s.metrics.push(
            Metric::new(
                "view_reads",
                "Lecturas de vistas",
                "Actividad",
                MetricUnit::Count,
                Some(v),
            )
            .counter()
            .help("Lecturas de vistas map/reduce que atendió este nodo. Una vista desactualizada se indexa antes de responder, así que leerla después de muchas escrituras puede tardar.")
            .source("GET /_node/_local/_stats: couchdb.httpd.view_reads"),
        );
    }
    let rt = st
        .get("couchdb")
        .and_then(|x| x.get("request_time"))
        .and_then(|x| x.get("value"));
    if let Some(rt) = rt {
        s.metrics.push(
            Metric::new(
                "request_time",
                "Tiempo medio por pedido",
                "Actividad",
                MetricUnit::Millis,
                rt.get("arithmetic_mean").and_then(Value::as_f64),
            )
            .help("Tiempo medio que CouchDB tardó en resolver los pedidos HTTP en la última ventana de estadísticas (10 s por omisión), sin contar la red. Si sube, el nodo está cargado o hay vistas o consultas lentas.")
            .source("GET /_node/_local/_stats: couchdb.request_time.value.arithmetic_mean"),
        );
        let p99 = rt
            .get("percentile")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .find(|p| p.get(0).and_then(Value::as_f64) == Some(99.0))
            })
            .and_then(|p| p.get(1))
            .and_then(Value::as_f64);
        if p99.is_some() {
            s.metrics.push(
                Metric::new(
                    "request_time_p99",
                    "Tiempo por pedido (p99)",
                    "Actividad",
                    MetricUnit::Millis,
                    p99,
                )
                .help("Tiempo en el que terminó el 99 % de los pedidos HTTP de la última ventana de estadísticas: muestra los pedidos lentos que la media esconde.")
                .source("GET /_node/_local/_stats: couchdb.request_time.value.percentile[99]"),
            );
        }
    }
    let codes = st
        .get("couchdb")
        .and_then(|x| x.get("httpd_status_codes"))
        .and_then(Value::as_object);
    if let Some(codes) = codes {
        let errors = |first: char| {
            codes
                .iter()
                .filter(|(k, _)| k.starts_with(first))
                .filter_map(|(_, v)| v.get("value").and_then(Value::as_f64))
                .sum::<f64>()
        };
        s.metrics.push(
            Metric::new(
                "http_4xx",
                "Respuestas 4xx",
                "Actividad",
                MetricUnit::Count,
                Some(errors('4')),
            )
            .counter()
            .help("Respuestas de error del cliente (400 a 499): pedidos inválidos, sin permiso (401, 403) o de documentos que no existen (404). Un aumento repentino suele ser un cliente mal configurado.")
            .source("GET /_node/_local/_stats: couchdb.httpd_status_codes.4xx (sum)"),
        );
        s.metrics.push(
            Metric::new(
                "http_5xx",
                "Respuestas 5xx",
                "Actividad",
                MetricUnit::Count,
                Some(errors('5')),
            )
            .counter()
            .help("Respuestas de error del servidor (500 a 599). Deberían ser cero o casi; si aparecen, buscá la causa en el log de CouchDB.")
            .source("GET /_node/_local/_stats: couchdb.httpd_status_codes.5xx (sum)"),
        );
    }
    if let Some(v) = c(&["open_databases"]) {
        s.metrics.push(
            Metric::new(
                "open_databases",
                "Bases abiertas",
                "Conexiones",
                MetricUnit::Count,
                Some(v),
            )
            .help("Archivos de bases (uno por shard) que el nodo tiene abiertos ahora. Cerca del límite max_dbs_open, CouchDB cierra y reabre bases y los pedidos se vuelven más lentos.")
            .source("GET /_node/_local/_stats: couchdb.open_databases"),
        );
    }
    if let Some(v) = c(&["httpd", "clients_requesting_changes"]) {
        s.metrics.push(
            Metric::new(
                "changes_clients",
                "Clientes escuchando _changes",
                "Conexiones",
                MetricUnit::Count,
                Some(v),
            )
            .help("Clientes con un feed _changes continuo abierto en este nodo, como las apps que sincronizan o las replicaciones que leen de él.")
            .source("GET /_node/_local/_stats: couchdb.httpd.clients_requesting_changes"),
        );
    }
    if let Some(v) = c(&["open_os_files"]) {
        s.metrics.push(
            Metric::new(
                "open_files",
                "Archivos abiertos",
                "Disco",
                MetricUnit::Count,
                Some(v),
            )
            .help("Descriptores de archivo que CouchDB tiene abiertos: archivos de bases y de índices. Cerca del límite del sistema (ulimit -n) empiezan los errores al abrir bases.")
            .source("GET /_node/_local/_stats: couchdb.open_os_files"),
        );
    }
    if let Some(v) = at(st, &["fsync", "count"]) {
        s.metrics.push(
            Metric::new("fsyncs", "fsync", "Disco", MetricUnit::Count, Some(v))
                .counter()
                .help("Llamadas a fsync con las que CouchDB asegura en disco lo que escribe. Acompañan a las escrituras; con un disco lento, cada una demora los pedidos que escriben.")
                .source("GET /_node/_local/_stats: fsync.count"),
        );
    }
    let (hits, misses) = (c(&["auth_cache_hits"]), c(&["auth_cache_misses"]));
    if let (Some(h), Some(m)) = (hits, misses) {
        if h + m > 0.0 {
            s.metrics.push(
                Metric::new(
                    "auth_cache_hit",
                    "Aciertos de la caché de autenticación",
                    "Caché",
                    MetricUnit::Percent,
                    Some(h / (h + m) * 100.0),
                )
                .max(Some(100.0))
                .help("Porcentaje de autenticaciones resueltas con la caché de usuarios, acumulado desde el arranque. Bajo indica que muchos pedidos tienen que buscar al usuario en _users, lo que suma latencia.")
                .source("GET /_node/_local/_stats: couchdb.auth_cache_hits ÷ (auth_cache_hits + auth_cache_misses)"),
            );
        }
    }
}

pub(crate) fn from_system(s: &mut MonitorSnapshot, sys: &Value) {
    let mem = sys.get("memory").and_then(Value::as_object);
    let total = mem.map(|m| {
        ["processes", "binary", "code", "ets", "atom", "other"]
            .iter()
            .filter_map(|k| m.get(*k).and_then(Value::as_f64))
            .sum::<f64>()
    });
    s.metrics.push(
        Metric::new(
            "mem_used",
            "Memoria de la VM Erlang",
            "Memoria",
            MetricUnit::Bytes,
            total,
        )
        .help("Memoria que usa la máquina virtual Erlang en la que corre CouchDB: procesos, binarios, código, tablas ETS, átomos y el resto. Si crece sin parar, mirá cuál de esas partes crece.")
        .source("GET /_node/_local/_system: memory.processes + binary + code + ets + atom + other"),
    );
    for (k, label, help, source) in [
        (
            "processes",
            "Memoria de procesos",
            "Memoria de los procesos Erlang: cada pedido, base abierta, índice o replicación es un proceso.",
            "GET /_node/_local/_system: memory.processes",
        ),
        (
            "binary",
            "Memoria binaria",
            "Memoria de los binarios grandes que comparten los procesos, como cuerpos de documentos y adjuntos en tránsito. Si crece y no baja, hay pedidos muy grandes o binarios retenidos.",
            "GET /_node/_local/_system: memory.binary",
        ),
        (
            "ets",
            "Tablas ETS",
            "Memoria de las tablas ETS, las tablas en memoria de Erlang que CouchDB usa para cachés y estado interno.",
            "GET /_node/_local/_system: memory.ets",
        ),
    ] {
        if let Some(v) = at(sys, &["memory", k]) {
            s.metrics.push(
                Metric::new(
                    &format!("mem_{k}"),
                    label,
                    "Memoria",
                    MetricUnit::Bytes,
                    Some(v),
                )
                .help(help)
                .source(source),
            );
        }
    }
    if let Some(cache) = sys.get("bt_engine_cache") {
        s.metrics.push(
            Metric::new(
                "mem_cache",
                "Caché de B-trees",
                "Memoria",
                MetricUnit::Bytes,
                at(cache, &["memory"]),
            )
            .max(at(cache, &["max_memory"]))
            .help("Memoria de la caché de nodos de B-tree de los archivos de bases, sobre su máximo.")
            .source("GET /_node/_local/_system: bt_engine_cache.memory; max: bt_engine_cache.max_memory"),
        );
        let hit = match (at(cache, &["hits"]), at(cache, &["misses"])) {
            (Some(h), Some(m)) if h + m > 0.0 => Some(h / (h + m) * 100.0),
            _ => None,
        };
        s.metrics.push(
            Metric::new(
                "cache_hit",
                "Aciertos de caché",
                "Caché",
                MetricUnit::Percent,
                hit,
            )
            .max(Some(100.0))
            .help("Porcentaje de las lecturas de nodos de B-tree resueltas con la caché, acumulado desde el arranque. Bajo indica que se leen del disco más páginas de las necesarias.")
            .source("GET /_node/_local/_system: bt_engine_cache.hits ÷ (hits + misses)"),
        );
    }
    let rq = sum(&[at(sys, &["run_queue"]), at(sys, &["run_queue_dirty_cpu"])]);
    s.metrics.push(
        Metric::new(
            "run_queue",
            "Cola de ejecución",
            "CPU",
            MetricUnit::Count,
            rq,
        )
        .help("Procesos Erlang listos para correr que esperan un planificador libre. Debería estar cerca de cero; si se mantiene alto, el nodo está limitado por CPU.")
        .source("GET /_node/_local/_system: run_queue + run_queue_dirty_cpu"),
    );
    if let Some(r) = at(sys, &["reductions"]) {
        s.metrics.push(
            Metric::new(
                "reductions",
                "Reducciones (trabajo de la VM)",
                "CPU",
                MetricUnit::Count,
                Some(r),
            )
            .counter()
            .help("Unidades de trabajo que ejecutó la VM Erlang (una reducción es más o menos una llamada a función). Como CouchDB no informa la CPU, su ritmo es la mejor medida de cuánto trabaja el nodo.")
            .source("GET /_node/_local/_system: reductions"),
        );
    }
    s.metrics.push(
        Metric::new(
            "processes",
            "Procesos Erlang",
            "Servidor",
            MetricUnit::Count,
            at(sys, &["process_count"]),
        )
        .max(at(sys, &["process_limit"]))
        .help("Procesos Erlang vivos, sobre el límite de la VM. Si llega al límite, CouchDB no puede atender más pedidos.")
        .source("GET /_node/_local/_system: process_count; max: process_limit"),
    );
    if let Some(v) = at(sys, &["io_input"]) {
        s.metrics.push(
            Metric::new("net_in", "E/S entrante", "Red", MetricUnit::Bytes, Some(v))
                .counter()
                .help("Bytes que la VM Erlang recibió por sus puertos: sobre todo tráfico de red de los clientes y de los otros nodos del cluster.")
                .source("GET /_node/_local/_system: io_input"),
        );
    }
    if let Some(v) = at(sys, &["io_output"]) {
        s.metrics.push(
            Metric::new("net_out", "E/S saliente", "Red", MetricUnit::Bytes, Some(v))
                .counter()
                .help("Bytes que la VM Erlang envió por sus puertos: sobre todo respuestas a los clientes y tráfico hacia los otros nodos del cluster.")
                .source("GET /_node/_local/_system: io_output"),
        );
    }
    s.metrics.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            at(sys, &["uptime"]),
        )
        .help("Tiempo desde que arrancó este nodo de CouchDB. Si vuelve a cero, se reinició.")
        .source("GET /_node/_local/_system: uptime"),
    );
}

pub(crate) fn active_tasks(tasks: &Value) -> MonitorTable {
    let mut t = MonitorTable::new(
        "queries",
        "Tareas activas",
        &[
            "tipo",
            "base",
            "documento de diseño",
            "progreso (%)",
            "cambios",
            "total",
            "inicio",
            "actualizada",
            "nodo",
            "pid",
        ],
    );
    for task in tasks
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .take(MAX_ROWS)
    {
        t.rows.push(vec![
            cell(task.get("type")),
            cell(task.get("database").or_else(|| task.get("source"))),
            cell(task.get("design_document")),
            cell(task.get("progress")),
            cell(
                task.get("changes_done")
                    .or_else(|| task.get("docs_written")),
            ),
            cell(task.get("total_changes")),
            when(task.get("started_on")),
            when(task.get("updated_on")),
            cell(task.get("node")),
            cell(task.get("pid")),
        ]);
    }
    t
}

pub(crate) fn replication_jobs(jobs: &Value) -> MonitorTable {
    let mut t = MonitorTable::new(
        "replication",
        "Replicaciones",
        &[
            "id",
            "origen",
            "destino",
            "estado",
            "inicio",
            "nodo",
            "documento",
        ],
    );
    for j in jobs
        .get("jobs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .take(MAX_ROWS)
    {
        let state = j
            .get("history")
            .and_then(Value::as_array)
            .and_then(|h| h.first())
            .and_then(|h| h.get("type"));
        t.rows.push(vec![
            cell(j.get("id")),
            cell(j.get("source")),
            cell(j.get("target")),
            cell(state),
            cell(j.get("start_time")),
            cell(j.get("node")),
            cell(j.get("doc_id")),
        ]);
    }
    t
}

impl CouchSession {
    pub(crate) async fn snapshot(&self) -> dbine_driver::Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        let get = |p: &'static str| self.call(Method::GET, p, None);
        let mut refused = Vec::new();
        let welcome = get("/").await?;
        s.info.push((
            "Versión".into(),
            welcome
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .into(),
        ));
        if let Some(v) = welcome
            .get("vendor")
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
        {
            s.info.push(("Distribuidor".into(), v.into()));
        }
        if let Some(f) = welcome.get("features").and_then(Value::as_array) {
            let f: Vec<&str> = f.iter().filter_map(Value::as_str).collect();
            s.info.push(("Funciones".into(), f.join(", ")));
        }
        s.notes.push(
            "CouchDB no informa el uso de CPU ni las conexiones HTTP abiertas; la cola de ejecución y las reducciones de la VM Erlang muestran la carga."
                .into(),
        );

        match get("/_node/_local/_system").await {
            Ok(sys) => from_system(&mut s, &sys),
            Err(_) => refused.push("/_node/_local/_system"),
        }
        match get("/_node/_local/_stats").await {
            Ok(st) => from_stats(&mut s, &st),
            Err(_) => refused.push("/_node/_local/_stats"),
        }
        match get("/_active_tasks").await {
            Ok(t) => {
                // Every task, not just the rows the table keeps.
                s.metrics
                    .push(tasks_metric(t.as_array().map_or(0, Vec::len)));
                s.tables.push(active_tasks(&t));
            }
            Err(_) => refused.push("/_active_tasks"),
        }
        match get("/_scheduler/jobs").await {
            Ok(j) => {
                s.metrics.push(replications_metric(job_count(&j)));
                s.tables.push(replication_jobs(&j));
            }
            Err(_) => refused.push("/_scheduler/jobs"),
        }

        // Databases and sizes.
        match get("/_all_dbs").await {
            Ok(all) => {
                let names: Vec<Value> = all.as_array().cloned().unwrap_or_default();
                if names.len() > MAX_DBS {
                    s.notes.push(format!(
                        "El espacio usado y los tamaños cubren solo las primeras {MAX_DBS} de {} bases, en orden alfabético.",
                        names.len()
                    ));
                }
                let keys: Vec<Value> = names.into_iter().take(MAX_DBS).collect();
                let infos = if keys.is_empty() {
                    Ok(Value::Array(Vec::new()))
                } else {
                    self.call(Method::POST, "/_dbs_info", Some(&json!({ "keys": keys })))
                        .await
                };
                match infos {
                    Ok(infos) => databases(&mut s, &infos),
                    Err(_) => refused.push("/_dbs_info"),
                }
            }
            Err(_) => refused.push("/_all_dbs"),
        }

        // Cluster.
        let up = get("/_up").await.ok();
        let status = up
            .as_ref()
            .and_then(|u| u.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        s.info.push(("Estado (_up)".into(), status.clone()));
        match get("/_membership").await {
            Ok(m) => {
                let all: Vec<&str> = m
                    .get("all_nodes")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let cluster: Vec<&str> = m
                    .get("cluster_nodes")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let mut t = MonitorTable::new("nodes", "Nodos del cluster", &["nodo", "estado"]);
                for n in cluster.iter().take(MAX_ROWS) {
                    let state = if all.contains(n) {
                        "conectado"
                    } else {
                        "sin conexión"
                    };
                    t.rows.push(vec![
                        Value::String((*n).into()),
                        Value::String(state.into()),
                    ]);
                }
                s.metrics.push(nodes_metric(all.len(), cluster.len()));
                s.tables.push(t);
            }
            Err(_) => refused.push("/_membership"),
        }
        if let Ok(cfg) = get("/_node/_local/_config").await {
            for (section, key, label) in [
                ("cluster", "q", "Shards por base (q)"),
                ("cluster", "n", "Réplicas por shard (n)"),
                ("couchdb", "max_dbs_open", "Bases abiertas máximas"),
                ("couchdb", "single_node", "Nodo único"),
                ("chttpd", "port", "Puerto"),
                ("chttpd", "max_http_request_size", "Pedido HTTP máximo"),
                ("couchdb", "database_dir", "Directorio de datos"),
            ] {
                if let Some(v) = cfg
                    .get(section)
                    .and_then(|s| s.get(key))
                    .and_then(Value::as_str)
                {
                    s.info.push((label.into(), v.into()));
                }
            }
        }
        if !refused.is_empty() {
            s.notes.push(format!(
                "Sin permiso de administrador del servidor no se pueden leer: {}.",
                refused.join(", ")
            ));
        }
        Ok(s)
    }
}

pub(crate) fn databases(s: &mut MonitorSnapshot, infos: &Value) {
    let mut t = MonitorTable::new(
        "databases",
        "Bases y tamaños",
        &[
            "base",
            "documentos",
            "borrados",
            "archivo",
            "datos activos",
            "datos (sin comprimir)",
            "shards",
            "compactando",
        ],
    );
    let mut total = 0.0;
    let mut rows: Vec<(f64, Vec<Value>)> = Vec::new();
    for item in infos.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let Some(info) = item.get("info") else {
            continue;
        };
        let file = at(info, &["sizes", "file"]).unwrap_or(0.0);
        total += file;
        rows.push((
            file,
            vec![
                cell(item.get("key").or_else(|| info.get("db_name"))),
                cell(info.get("doc_count")),
                cell(info.get("doc_del_count")),
                json!(file),
                cell(info.get("sizes").and_then(|x| x.get("active"))),
                cell(info.get("sizes").and_then(|x| x.get("external"))),
                cell(info.get("cluster").and_then(|x| x.get("q"))),
                cell(info.get("compact_running")),
            ],
        ));
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    t.rows = rows.into_iter().map(|(_, r)| r).collect();
    s.metrics.push(
        Metric::new(
            "storage_used",
            "Espacio usado",
            "Almacenamiento",
            MetricUnit::Bytes,
            Some(total),
        )
        .help("Espacio en disco de los archivos de las bases, sumando todos sus shards y sin los índices de vistas. Con más de 100 bases suma solo las primeras 100 en orden alfabético, y una nota lo avisa. Incluye lo que liberaría una compactación: compará con los datos activos de cada base.")
        .source("POST /_dbs_info with the first 100 names of GET /_all_dbs: info.sizes.file (sum)"),
    );
    s.tables.push(t);
}

/// The background tasks running in the cluster (`/_active_tasks`).
pub(crate) fn tasks_metric(n: usize) -> Metric {
    Metric::new(
        "active_sessions",
        "Tareas activas",
        "Actividad",
        MetricUnit::Count,
        Some(n as f64),
    )
    .help("Tareas en segundo plano que corren ahora en el cluster: indexación de vistas, compactaciones y replicaciones. Muchas a la vez compiten con los pedidos por disco y CPU.")
    .source("GET /_active_tasks (rows)")
}

/// The replication jobs the scheduler holds (`/_scheduler/jobs`).
pub(crate) fn replications_metric(n: usize) -> Metric {
    Metric::new(
        "replications",
        "Replicaciones activas",
        "Replicación",
        MetricUnit::Count,
        Some(n as f64),
    )
    .help("Replicaciones que tiene el planificador: las que corren y las que esperan turno o reintentan después de un error. Las terminadas no cuentan.")
    .source("GET /_scheduler/jobs: total_rows (else jobs rows)")
}

/// The replication jobs in a `/_scheduler/jobs` answer: `total_rows`, else
/// the listed jobs.
pub(crate) fn job_count(jobs: &Value) -> usize {
    jobs.get("total_rows")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or_else(|| {
            jobs.get("jobs")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        })
}

/// The nodes this node is connected to, of the cluster's (`/_membership`).
pub(crate) fn nodes_metric(connected: usize, cluster: usize) -> Metric {
    Metric::new(
        "nodes_up",
        "Nodos conectados",
        "Servidor",
        MetricUnit::Count,
        Some(connected as f64),
    )
    .max(Some(cluster as f64))
    .help("Nodos del cluster con los que este nodo está conectado, sobre los nodos configurados. Si son menos, hay nodos caídos o sin red.")
    .source("GET /_membership: all_nodes; max: cluster_nodes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_and_system() {
        let st = json!({
            "couchdb": {
                "httpd": { "requests": { "value": 8, "type": "counter" } },
                "database_reads": { "value": 3, "type": "counter" },
                "database_writes": { "value": 2, "type": "counter" },
                "request_time": { "value": { "arithmetic_mean": 1.5, "percentile": [[50, 1], [99, 9.5]] }, "type": "histogram" },
                "httpd_status_codes": { "200": { "value": 5 }, "404": { "value": 2 }, "401": { "value": 1 }, "500": { "value": 0 } }
            }
        });
        let sys = json!({ "uptime": 60, "memory": { "processes": 10, "binary": 5, "code": 1, "ets": 4, "atom": 0, "other": 0 },
                          "run_queue": 1, "run_queue_dirty_cpu": 0, "process_count": 400, "process_limit": 1000,
                          "bt_engine_cache": { "memory": 20, "max_memory": 100, "hits": 3, "misses": 1 } });
        let mut s = MonitorSnapshot::default();
        from_stats(&mut s, &st);
        from_system(&mut s, &sys);
        let m = |k: &str| s.metrics.iter().find(|m| m.key == k).unwrap();
        assert_eq!(m("queries").value, Some(8.0));
        assert!(m("queries").counter);
        assert_eq!(m("request_time_p99").value, Some(9.5));
        assert_eq!(m("http_4xx").value, Some(3.0));
        assert_eq!(m("mem_used").value, Some(20.0));
        assert_eq!(m("cache_hit").value, Some(75.0));
        assert_eq!(m("processes").max, Some(1000.0));
    }

    /// Every metric a snapshot can carry says what it measures and where it
    /// comes from.
    #[test]
    fn every_metric_has_help_and_source() {
        let st = json!({
            "couchdb": {
                "httpd": {
                    "requests": { "value": 8 }, "view_reads": { "value": 1 },
                    "clients_requesting_changes": { "value": 1 }
                },
                "database_reads": { "value": 3 }, "database_writes": { "value": 2 },
                "document_inserts": { "value": 1 }, "open_databases": { "value": 4 },
                "open_os_files": { "value": 9 },
                "auth_cache_hits": { "value": 3 }, "auth_cache_misses": { "value": 1 },
                "request_time": { "value": { "arithmetic_mean": 1.5, "percentile": [[50, 1], [99, 9.5]] } },
                "httpd_status_codes": { "200": { "value": 5 }, "404": { "value": 2 }, "500": { "value": 0 } }
            },
            "fsync": { "count": { "value": 4 } }
        });
        let sys = json!({ "uptime": 60, "memory": { "processes": 10, "binary": 5, "code": 1, "ets": 4, "atom": 0, "other": 0 },
                          "run_queue": 1, "run_queue_dirty_cpu": 0, "reductions": 100, "process_count": 400, "process_limit": 1000,
                          "io_input": 10, "io_output": 20,
                          "bt_engine_cache": { "memory": 20, "max_memory": 100, "hits": 3, "misses": 1 } });
        let mut s = MonitorSnapshot::default();
        from_stats(&mut s, &st);
        from_system(&mut s, &sys);
        databases(&mut s, &json!([]));
        s.metrics.push(tasks_metric(1));
        s.metrics.push(replications_metric(0));
        s.metrics.push(nodes_metric(1, 1));
        // The fixtures reach every metric the module can produce.
        assert_eq!(s.metrics.len(), 30);
        for m in &s.metrics {
            let filled = |v: &Option<String>| v.as_deref().is_some_and(|t| !t.trim().is_empty());
            assert!(filled(&m.help), "{}: no help", m.key);
            assert!(filled(&m.source), "{}: no source", m.key);
        }
    }

    #[test]
    fn tables() {
        let t = active_tasks(
            &json!([{ "type": "indexer", "database": "db", "progress": 40, "started_on": 1700000000 }]),
        );
        assert_eq!(t.rows[0][0], json!("indexer"));
        assert_eq!(t.rows[0][6], json!("2023-11-14 22:13:20"));
        let mut s = MonitorSnapshot::default();
        databases(
            &mut s,
            &json!([{ "key": "a", "info": { "doc_count": 2, "sizes": { "file": 100, "active": 50 } } }, { "key": "b", "error": "not_found" }]),
        );
        assert_eq!(s.tables[0].rows.len(), 1);
        assert_eq!(s.metrics[0].value, Some(100.0));
        // The help and source name the cap the snapshot sends.
        let cap = MAX_DBS.to_string();
        assert!(s.metrics[0].help.as_deref().unwrap().contains(&cap));
        assert!(s.metrics[0].source.as_deref().unwrap().contains(&cap));
        // Replications: the scheduler's total, not the rows the table keeps.
        let jobs: Vec<Value> = (0..MAX_ROWS + 5).map(|i| json!({ "id": i })).collect();
        assert_eq!(job_count(&json!({ "jobs": jobs, "total_rows": 250 })), 250);
        assert_eq!(job_count(&json!({ "jobs": jobs })), MAX_ROWS + 5);
        assert_eq!(
            replication_jobs(&json!({ "jobs": jobs })).rows.len(),
            MAX_ROWS
        );
    }
}

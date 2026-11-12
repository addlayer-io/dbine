//! `Session::monitor` for Elasticsearch, OpenSearch and Open Distro:
//! `_cluster/health`, `_nodes/stats` (CPU, memory, JVM heap, GC, HTTP
//! connections, search and indexing counters, caches, transport, disk),
//! `_nodes/os` (the processors that scale the process CPU time),
//! `_cat/indices` (sizes), `_tasks` (running requests) and
//! `_cluster/pending_tasks`. Every figure is summed over the nodes (CPU
//! and load are averaged). A refused endpoint (a user without the
//! `monitor` cluster privilege, serverless projects) leaves a note.

use crate::json::J;
use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use serde_json::{json, Value};

const MAX_ROWS: usize = 200;

pub(crate) fn f(j: &J, path: &[&str]) -> Option<f64> {
    match j.at(path)? {
        J::Num(n) => n.as_f64(),
        J::Str(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

fn nodes(stats: &J) -> Vec<&J> {
    stats
        .get("nodes")
        .and_then(J::as_obj)
        .map(|o| o.iter().map(|(_, n)| n).collect())
        .unwrap_or_default()
}

/// Sum of a figure over the nodes that report it.
fn total(nodes: &[&J], path: &[&str]) -> Option<f64> {
    let v: Vec<f64> = nodes.iter().filter_map(|n| f(n, path)).collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

fn avg(nodes: &[&J], path: &[&str]) -> Option<f64> {
    let v: Vec<f64> = nodes.iter().filter_map(|n| f(n, path)).collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

fn ratio(hits: Option<f64>, misses: Option<f64>) -> Option<f64> {
    match (hits, misses) {
        (Some(h), Some(m)) if h + m > 0.0 => Some(h / (h + m) * 100.0),
        _ => None,
    }
}

fn cell(j: Option<&J>) -> Value {
    j.map(J::cell).unwrap_or(Value::Null)
}

fn opt(v: Option<f64>) -> Value {
    v.map(|x| json!(x)).unwrap_or(Value::Null)
}

pub(crate) fn from_health(s: &mut MonitorSnapshot, h: &J) {
    for (k, label) in [
        ("cluster_name", "Cluster"),
        ("status", "Estado del cluster"),
    ] {
        if let Some(v) = h.get(k).map(J::text) {
            let v = match v.as_str() {
                "green" => "verde".to_string(),
                "yellow" => "amarillo (hay réplicas sin asignar)".to_string(),
                "red" => "rojo (hay shards primarios sin asignar)".to_string(),
                _ => v,
            };
            s.info.push((label.into(), v));
        }
    }
    let nodes = f(h, &["number_of_nodes"]);
    s.metrics.push(
        Metric::new("nodes", "Nodos", "Cluster", MetricUnit::Count, nodes)
            .help(
                "Nodos que forman parte del cluster en este momento. \
                 Si baja, un nodo se cayó o perdió contacto con el resto.",
            )
            .source("GET /_cluster/health: number_of_nodes"),
    );
    s.metrics.push(
        Metric::new(
            "data_nodes",
            "Nodos de datos",
            "Cluster",
            MetricUnit::Count,
            f(h, &["number_of_data_nodes"]),
        )
        .help(
            "Nodos con rol de datos, los que guardan shards. \
             Si baja, sus shards quedan sin asignar hasta que el nodo vuelva o se rehagan las copias en otros.",
        )
        .source("GET /_cluster/health: number_of_data_nodes"),
    );
    s.metrics.push(
        Metric::new(
            "active_shards",
            "Shards activos",
            "Cluster",
            MetricUnit::Count,
            f(h, &["active_shards"]),
        )
        .help(
            "Shards activos del cluster, primarios y réplicas. \
             Cada uno consume heap y descriptores de archivo: miles por nodo suelen ser demasiados.",
        )
        .source("GET /_cluster/health: active_shards"),
    );
    s.metrics.push(
        Metric::new(
            "unassigned_shards",
            "Shards sin asignar",
            "Cluster",
            MetricUnit::Count,
            f(h, &["unassigned_shards"]),
        )
        .help(
            "Shards que no están en ningún nodo. \
             Si hay primarios sin asignar el cluster está en rojo y faltan datos; si son solo réplicas, en amarillo (normal en un cluster de un nodo con réplicas configuradas).",
        )
        .source("GET /_cluster/health: unassigned_shards"),
    );
    s.metrics.push(
        Metric::new(
            "relocating_shards",
            "Shards en movimiento",
            "Cluster",
            MetricUnit::Count,
            f(h, &["relocating_shards"]),
        )
        .help(
            "Shards que se están copiando de un nodo a otro, por rebalanceo o por falta de disco. \
             Mientras dura, suben la red entre nodos y la escritura en disco.",
        )
        .source("GET /_cluster/health: relocating_shards"),
    );
    s.metrics.push(
        Metric::new(
            "initializing_shards",
            "Shards inicializando",
            "Cluster",
            MetricUnit::Count,
            f(h, &["initializing_shards"]),
        )
        .help(
            "Shards que se están creando o recuperando, al crear un índice, arrancar un nodo o rehacer una réplica. \
             Deberían volver a cero en minutos.",
        )
        .source("GET /_cluster/health: initializing_shards"),
    );
    s.metrics.push(
        Metric::new(
            "active_shards_percent",
            "Shards activos (%)",
            "Cluster",
            MetricUnit::Percent,
            f(h, &["active_shards_percent_as_number"]),
        )
        .max(Some(100.0))
        .help(
            "Porcentaje de los shards del cluster que están activos. \
             Por debajo de 100 % hay shards sin asignar o recuperándose.",
        )
        .source("GET /_cluster/health: active_shards_percent_as_number"),
    );
    s.metrics.push(
        Metric::new(
            "pending_tasks",
            "Tareas pendientes del cluster",
            "Cluster",
            MetricUnit::Count,
            f(h, &["number_of_pending_tasks"]),
        )
        .help(
            "Cambios del estado del cluster (crear índices, cambiar mapeos, asignar shards) que esperan al nodo maestro. \
             Si se acumulan, el maestro no da abasto y las operaciones de administración se demoran.",
        )
        .source("GET /_cluster/health: number_of_pending_tasks"),
    );
}

/// The CPU time of the nodes as a share of every processor they may use
/// (`os.available_processors` from `_nodes/os`): 100 = all of them busy.
/// Only nodes with both figures count; `None` without processor counts.
fn process_cpu(stats: &J, os_info: Option<&J>) -> Option<f64> {
    let (mut ms, mut procs) = (0.0, 0.0);
    for (id, node) in stats.get("nodes").and_then(J::as_obj).into_iter().flatten() {
        let p = os_info
            .and_then(|i| i.at(&["nodes", id.as_str(), "os"]))
            .and_then(|o| {
                f(o, &["available_processors"]).or_else(|| f(o, &["allocated_processors"]))
            })
            .filter(|p| *p > 0.0);
        if let (Some(c), Some(p)) = (f(node, &["process", "cpu", "total_in_millis"]), p) {
            ms += c;
            procs += p;
        }
    }
    (procs > 0.0).then(|| ms / 1000.0 * 100.0 / procs)
}

pub(crate) fn from_node_stats(s: &mut MonitorSnapshot, stats: &J, os_info: Option<&J>) {
    let ns = nodes(stats);
    let n = ns.as_slice();

    // CPU.
    s.metrics.push(
        Metric::new(
            "cpu",
            "CPU del servidor",
            "CPU",
            MetricUnit::Percent,
            avg(n, &["os", "cpu", "percent"]),
        )
        .max(Some(100.0))
        .help(
            "Uso de CPU de toda la máquina de cada nodo, no solo del proceso del nodo, promediado entre nodos. \
             Si está alto y «CPU del proceso» no, otro proceso compite por la CPU.",
        )
        .source("GET /_nodes/stats/os: avg cpu.percent"),
    );
    s.metrics.push(
        Metric::new(
            "cpu_time",
            "CPU del proceso",
            "CPU",
            MetricUnit::Percent,
            process_cpu(stats, os_info),
        )
        .counter()
        .max(Some(100.0))
        .help(
            "Tiempo de CPU de los procesos Java de los nodos como porcentaje de todos sus procesadores: 100 % es todos ocupados. \
             Sostenido alto, las búsquedas y la indexación compiten por CPU.",
        )
        .source(
            "GET /_nodes/stats/process: Σ cpu.total_in_millis / 1000 × 100 / Σ (os.available_processors | os.allocated_processors) from GET /_nodes/os, only nodes reporting both",
        ),
    );
    if let Some(l) = avg(n, &["os", "cpu", "load_average", "1m"]) {
        s.metrics.push(
            Metric::new(
                "load_1m",
                "Carga (1 min)",
                "CPU",
                MetricUnit::Count,
                Some(l),
            )
            .help(
                "Procesos ejecutándose o esperando CPU en el último minuto, según el sistema operativo, promediado entre nodos. \
                 Comparalo con la cantidad de procesadores: si lo supera de forma sostenida, la máquina está saturada.",
            )
            .source("GET /_nodes/stats/os: avg cpu.load_average.1m"),
        );
    }

    // Memory.
    s.metrics.push(
        Metric::new(
            "mem_used",
            "Memoria usada",
            "Memoria",
            MetricUnit::Bytes,
            total(n, &["os", "mem", "used_in_bytes"]),
        )
        .max(total(n, &["os", "mem", "total_in_bytes"]))
        .help(
            "Memoria física usada en las máquinas de los nodos, sumada. \
             Incluye la caché de archivos del sistema operativo, que el motor aprovecha para leer los índices, así que suele estar alta sin que sea un problema.",
        )
        .source("GET /_nodes/stats/os: Σ mem.used_in_bytes (max: Σ mem.total_in_bytes)"),
    );
    s.metrics.push(
        Metric::new(
            "heap_used",
            "Heap del JVM",
            "Memoria",
            MetricUnit::Bytes,
            total(n, &["jvm", "mem", "heap_used_in_bytes"]),
        )
        .max(total(n, &["jvm", "mem", "heap_max_in_bytes"]))
        .help(
            "Heap de Java en uso, sumado entre nodos; el máximo es la suma de los heaps configurados. \
             Oscila con cada recolección: si el piso queda por encima del 75 %, el recolector trabaja de más y las pausas se alargan; \
             cerca del 95 % saltan los circuit breakers y se rechazan pedidos.",
        )
        .source(
            "GET /_nodes/stats/jvm: Σ mem.heap_used_in_bytes (max: Σ mem.heap_max_in_bytes)",
        ),
    );
    let caches = [
        total(n, &["indices", "query_cache", "memory_size_in_bytes"]),
        total(n, &["indices", "request_cache", "memory_size_in_bytes"]),
        total(n, &["indices", "fielddata", "memory_size_in_bytes"]),
    ];
    let cache_mem = (caches.iter().any(Option::is_some)).then(|| caches.iter().flatten().sum());
    s.metrics.push(
        Metric::new(
            "mem_cache",
            "Cachés (consultas, pedidos, fielddata)",
            "Memoria",
            MetricUnit::Bytes,
            cache_mem,
        )
        .help(
            "Heap que ocupan la caché de consultas (filtros), la de pedidos (resultados de agregaciones) y fielddata, sumado entre nodos. \
             Un fielddata grande suele venir de ordenar o agregar sobre campos text.",
        )
        .source(
            "GET /_nodes/stats/indices: Σ query_cache.memory_size_in_bytes + request_cache.memory_size_in_bytes + fielddata.memory_size_in_bytes",
        ),
    );
    let gc = n
        .iter()
        .filter_map(|node| node.at(&["jvm", "gc", "collectors"]).and_then(J::as_obj))
        .flat_map(|c| {
            c.iter()
                .filter_map(|(_, v)| f(v, &["collection_time_in_millis"]))
        })
        .reduce(|a, b| a + b);
    if gc.is_some() {
        s.metrics.push(
            Metric::new("gc_time", "Tiempo de GC", "Memoria", MetricUnit::Millis, gc)
                .counter()
                .help(
                    "Tiempo que pasó el JVM recolectando basura, todos los recolectores y nodos sumados. \
                     Si crece rápido, el heap queda chico para la carga y las pausas frenan las búsquedas.",
                )
                .source("GET /_nodes/stats/jvm: Σ gc.collectors.*.collection_time_in_millis"),
        );
    }

    // Connections.
    s.metrics.push(
        Metric::new(
            "connections",
            "Conexiones HTTP",
            "Conexiones",
            MetricUnit::Count,
            total(n, &["http", "current_open"]),
        )
        .help(
            "Conexiones HTTP de clientes abiertas ahora, sumadas entre nodos e incluida la de DBine. \
             Si crecen sin parar, algún cliente abre conexiones y no las cierra.",
        )
        .source("GET /_nodes/stats/http: Σ current_open"),
    );
    s.metrics.push(
        Metric::new(
            "active_sessions",
            "Búsquedas en curso",
            "Conexiones",
            MetricUnit::Count,
            total(n, &["indices", "search", "query_current"]),
        )
        .help(
            "Fases de consulta ejecutándose ahora, contadas por shard: una búsqueda sobre un índice de cinco shards suma cinco. \
             Si se mantienen altas, las búsquedas son lentas o llegan más de las que atiende el thread pool search.",
        )
        .source("GET /_nodes/stats/indices: Σ search.query_current"),
    );
    if let Some(v) = total(n, &["http", "total_opened"]) {
        s.metrics.push(
            Metric::new(
                "connections_opened",
                "Conexiones HTTP abiertas",
                "Conexiones",
                MetricUnit::Count,
                Some(v),
            )
            .counter()
            .help(
                "Conexiones HTTP nuevas que abrieron los clientes, sumadas entre nodos. \
                 Un ritmo alto indica clientes que no reutilizan conexiones (sin keep-alive) y pagan cada vez la conexión y el TLS.",
            )
            .source("GET /_nodes/stats/http: Σ total_opened"),
        );
    }

    // Activity.
    s.metrics.push(
        Metric::new(
            "queries",
            "Búsquedas",
            "Actividad",
            MetricUnit::Count,
            total(n, &["indices", "search", "query_total"]),
        )
        .counter()
        .help(
            "Fases de consulta completadas, contadas por shard: una búsqueda sobre un índice de cinco shards suma cinco. \
             Es la carga de búsqueda que reciben los nodos.",
        )
        .source("GET /_nodes/stats/indices: Σ search.query_total"),
    );
    s.metrics.push(
        Metric::new(
            "rows_read",
            "Fetch de documentos",
            "Actividad",
            MetricUnit::Count,
            total(n, &["indices", "search", "fetch_total"]),
        )
        .counter()
        .help(
            "Fases de fetch completadas, las que traen de cada shard los documentos de la respuesta, contadas por shard. \
             No es la cantidad de documentos devueltos.",
        )
        .source("GET /_nodes/stats/indices: Σ search.fetch_total"),
    );
    s.metrics.push(
        Metric::new(
            "rows_written",
            "Documentos indexados",
            "Actividad",
            MetricUnit::Count,
            total(n, &["indices", "indexing", "index_total"]),
        )
        .counter()
        .help(
            "Operaciones de indexación (altas y actualizaciones de documentos) en todas las copias: con una réplica, cada documento cuenta dos veces. \
             Mide la carga de escritura.",
        )
        .source("GET /_nodes/stats/indices: Σ indexing.index_total"),
    );
    s.metrics.push(
        Metric::new(
            "gets",
            "GET de documentos",
            "Actividad",
            MetricUnit::Count,
            total(n, &["indices", "get", "total"]),
        )
        .counter()
        .help("Lecturas de documentos por id (GET y mget), contadas por shard.")
        .source("GET /_nodes/stats/indices: Σ get.total"),
    );
    s.metrics.push(
        Metric::new(
            "deletes",
            "Documentos borrados",
            "Actividad",
            MetricUnit::Count,
            total(n, &["indices", "indexing", "delete_total"]),
        )
        .counter()
        .help(
            "Operaciones de borrado de documentos en todas las copias, primarios y réplicas. \
             Los documentos borrados siguen ocupando espacio hasta que un merge los descarta.",
        )
        .source("GET /_nodes/stats/indices: Σ indexing.delete_total"),
    );
    s.metrics.push(
        Metric::new(
            "search_time",
            "Tiempo de búsqueda",
            "Actividad",
            MetricUnit::Millis,
            total(n, &["indices", "search", "query_time_in_millis"]),
        )
        .counter()
        .help(
            "Tiempo pasado en fases de consulta, sumado entre shards y nodos. \
             Dividido por las búsquedas da la duración media por shard; si sube con la misma carga, las búsquedas se volvieron más caras.",
        )
        .source("GET /_nodes/stats/indices: Σ search.query_time_in_millis"),
    );
    s.metrics.push(
        Metric::new(
            "indexing_time",
            "Tiempo de indexación",
            "Actividad",
            MetricUnit::Millis,
            total(n, &["indices", "indexing", "index_time_in_millis"]),
        )
        .counter()
        .help(
            "Tiempo pasado indexando documentos, sumado entre shards y nodos. \
             Si sube más que los documentos indexados, cada documento cuesta más: mapeos grandes, merges o disco lento.",
        )
        .source("GET /_nodes/stats/indices: Σ indexing.index_time_in_millis"),
    );
    s.metrics.push(
        Metric::new(
            "merges",
            "Merges",
            "Actividad",
            MetricUnit::Count,
            total(n, &["indices", "merges", "total"]),
        )
        .counter()
        .help(
            "Fusiones de segmentos de Lucene completadas, contadas por shard. \
             Acompañan a la indexación; muchas seguidas consumen disco y CPU.",
        )
        .source("GET /_nodes/stats/indices: Σ merges.total"),
    );
    s.metrics.push(
        Metric::new(
            "refreshes",
            "Refresh",
            "Actividad",
            MetricUnit::Count,
            total(n, &["indices", "refresh", "total"]),
        )
        .counter()
        .help(
            "Refresh completados, contados por shard: cada uno hace visibles para la búsqueda los documentos recién indexados. \
             Muchos por segundo encarecen la indexación; revisá el refresh_interval de los índices.",
        )
        .source("GET /_nodes/stats/indices: Σ refresh.total"),
    );

    // Thread pools: queued and rejected.
    let pools = |key: &str| -> Option<f64> {
        let v: Vec<f64> = n
            .iter()
            .filter_map(|node| node.get("thread_pool").and_then(J::as_obj))
            .flat_map(|p| p.iter().filter_map(|(_, v)| f(v, &[key])))
            .collect();
        (!v.is_empty()).then(|| v.iter().sum())
    };
    s.metrics.push(
        Metric::new(
            "locks_waiting",
            "Tareas en cola (thread pools)",
            "Bloqueos",
            MetricUnit::Count,
            pools("queue"),
        )
        .help(
            "Tareas que esperan un hilo libre, sumadas entre todos los thread pools y nodos. \
             Una cola sostenida indica un pool saturado; cuando se llena, las tareas se rechazan.",
        )
        .source("GET /_nodes/stats/thread_pool: Σ *.queue"),
    );
    s.metrics.push(
        Metric::new(
            "rejected",
            "Tareas rechazadas",
            "Bloqueos",
            MetricUnit::Count,
            pools("rejected"),
        )
        .counter()
        .help(
            "Tareas que un thread pool rechazó porque su cola estaba llena, sumadas entre pools y nodos. \
             Cada rechazo es una búsqueda o una escritura que falló con 429; la tabla de thread pools muestra cuál se satura.",
        )
        .source("GET /_nodes/stats/thread_pool: Σ *.rejected"),
    );

    // Cache.
    let qc = ratio(
        total(n, &["indices", "query_cache", "hit_count"]),
        total(n, &["indices", "query_cache", "miss_count"]),
    );
    s.metrics.push(
        Metric::new(
            "cache_hit",
            "Aciertos de la caché de consultas",
            "Caché",
            MetricUnit::Percent,
            qc,
        )
        .max(Some(100.0))
        .help(
            "Porcentaje de búsquedas en la caché de consultas (filtros) que encontraron el resultado, acumulado desde que arrancaron los nodos, así que cambia despacio. \
             Es común que sea bajo: solo se guardan filtros que se repiten, en segmentos grandes.",
        )
        .source(
            "GET /_nodes/stats/indices: Σ query_cache.hit_count / Σ (hit_count + miss_count) × 100",
        ),
    );
    let rc = ratio(
        total(n, &["indices", "request_cache", "hit_count"]),
        total(n, &["indices", "request_cache", "miss_count"]),
    );
    s.metrics.push(
        Metric::new(
            "request_cache_hit",
            "Aciertos de la caché de pedidos",
            "Caché",
            MetricUnit::Percent,
            rc,
        )
        .max(Some(100.0))
        .help(
            "Porcentaje de pedidos que encontraron su resultado en la caché de pedidos de cada shard (búsquedas con size=0, como las agregaciones), acumulado desde que arrancaron los nodos. \
             Baja si los índices cambian seguido, porque cada refresh la invalida.",
        )
        .source(
            "GET /_nodes/stats/indices: Σ request_cache.hit_count / Σ (hit_count + miss_count) × 100",
        ),
    );
    let ev = [
        total(n, &["indices", "query_cache", "evictions"]),
        total(n, &["indices", "request_cache", "evictions"]),
        total(n, &["indices", "fielddata", "evictions"]),
    ];
    if ev.iter().any(Option::is_some) {
        s.metrics.push(
            Metric::new(
                "evictions",
                "Desalojos de caché",
                "Caché",
                MetricUnit::Count,
                Some(ev.iter().flatten().sum()),
            )
            .counter()
            .help(
                "Entradas expulsadas de la caché de consultas, la de pedidos y fielddata, sumadas entre nodos. \
                 Un ritmo alto indica cachés chicas para la carga.",
            )
            .source(
                "GET /_nodes/stats/indices: Σ query_cache.evictions + request_cache.evictions + fielddata.evictions",
            ),
        );
    }

    // Network (between nodes) and disk.
    s.metrics.push(
        Metric::new(
            "net_in",
            "Red entre nodos (entrante)",
            "Red",
            MetricUnit::Bytes,
            total(n, &["transport", "rx_size_in_bytes"]),
        )
        .counter()
        .help(
            "Bytes recibidos por la capa de transporte entre nodos (replicación, recuperación de shards, búsquedas distribuidas), sumados entre nodos. \
             No incluye el tráfico HTTP de los clientes; con un solo nodo queda casi en cero.",
        )
        .source("GET /_nodes/stats/transport: Σ rx_size_in_bytes"),
    );
    s.metrics.push(
        Metric::new(
            "net_out",
            "Red entre nodos (saliente)",
            "Red",
            MetricUnit::Bytes,
            total(n, &["transport", "tx_size_in_bytes"]),
        )
        .counter()
        .help(
            "Bytes enviados por la capa de transporte entre nodos (replicación, recuperación de shards, búsquedas distribuidas), sumados entre nodos. \
             No incluye el tráfico HTTP de los clientes; con un solo nodo queda casi en cero.",
        )
        .source("GET /_nodes/stats/transport: Σ tx_size_in_bytes"),
    );
    if let Some(r) = total(n, &["fs", "io_stats", "total", "read_kilobytes"]) {
        s.metrics.push(
            Metric::new(
                "disk_read",
                "Lectura en disco",
                "Disco",
                MetricUnit::Bytes,
                Some(r * 1024.0),
            )
            .counter()
            .help(
                "Bytes leídos de los discos de datos de los nodos, sumados (solo en Linux). \
                 Lecturas altas y sostenidas indican que los índices no entran en la caché de archivos del sistema.",
            )
            .source("GET /_nodes/stats/fs: Σ io_stats.total.read_kilobytes × 1024"),
        );
    }
    if let Some(w) = total(n, &["fs", "io_stats", "total", "write_kilobytes"]) {
        s.metrics.push(
            Metric::new(
                "disk_write",
                "Escritura en disco",
                "Disco",
                MetricUnit::Bytes,
                Some(w * 1024.0),
            )
            .counter()
            .help(
                "Bytes escritos en los discos de datos de los nodos, sumados (solo en Linux). \
                 Sigue a la indexación, los merges y la recuperación de shards.",
            )
            .source("GET /_nodes/stats/fs: Σ io_stats.total.write_kilobytes × 1024"),
        );
    }

    // Storage.
    s.metrics.push(
        Metric::new(
            "storage_used",
            "Tamaño de los índices",
            "Almacenamiento",
            MetricUnit::Bytes,
            total(n, &["indices", "store", "size_in_bytes"]),
        )
        .help("Espacio que ocupan en los nodos los shards de los índices, primarios y réplicas incluidos.")
        .source("GET /_nodes/stats/indices: Σ store.size_in_bytes"),
    );
    let disk_total = total(n, &["fs", "total", "total_in_bytes"]);
    let disk_used = match (disk_total, total(n, &["fs", "total", "available_in_bytes"])) {
        (Some(t), Some(a)) => Some(t - a),
        _ => None,
    };
    s.metrics.push(
        Metric::new(
            "disk_used",
            "Disco usado",
            "Almacenamiento",
            MetricUnit::Bytes,
            disk_used,
        )
        .max(disk_total)
        .help(
            "Espacio usado en los discos de datos de los nodos, sumado; el máximo es su capacidad. \
             Al pasar las marcas de agua (85 %, 90 % y 95 % por defecto) el cluster deja de asignar shards al nodo, los mueve y, en la última, pone sus índices en solo lectura.",
        )
        .source(
            "GET /_nodes/stats/fs: Σ total.total_in_bytes − Σ total.available_in_bytes (max: Σ total.total_in_bytes)",
        ),
    );
    s.metrics.push(
        Metric::new(
            "docs",
            "Documentos",
            "Almacenamiento",
            MetricUnit::Count,
            total(n, &["indices", "docs", "count"]),
        )
        .help(
            "Documentos guardados en los shards de los nodos, sin los borrados. \
             Cuenta cada copia (primarios y réplicas) y los documentos anidados por separado, así que supera al conteo de los índices.",
        )
        .source("GET /_nodes/stats/indices: Σ docs.count"),
    );
    s.metrics.push(
        Metric::new(
            "segments",
            "Segmentos",
            "Almacenamiento",
            MetricUnit::Count,
            total(n, &["indices", "segments", "count"]),
        )
        .help(
            "Segmentos de Lucene en todos los shards de los nodos. \
             Muchos segmentos chicos encarecen las búsquedas; los merges los van uniendo.",
        )
        .source("GET /_nodes/stats/indices: Σ segments.count"),
    );

    // Server.
    let up = n
        .iter()
        .filter_map(|node| f(node, &["jvm", "uptime_in_millis"]))
        .reduce(f64::max);
    s.metrics.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            up.map(|ms| ms / 1000.0),
        )
        .help(
            "Tiempo que lleva en marcha el nodo más antiguo del cluster. \
             Si baja de golpe, ese nodo se reinició.",
        )
        .source("GET /_nodes/stats/jvm: max uptime_in_millis / 1000"),
    );
    if let Some(fd) = total(n, &["process", "open_file_descriptors"]) {
        s.metrics.push(
            Metric::new(
                "open_files",
                "Descriptores abiertos",
                "Servidor",
                MetricUnit::Count,
                Some(fd),
            )
            .max(total(n, &["process", "max_file_descriptors"]))
            .help(
                "Archivos y conexiones que tienen abiertos los procesos de los nodos, sumados; el máximo es la suma de sus límites. \
                 Si un nodo llega a su límite, falla al abrir archivos de índice o al aceptar conexiones.",
            )
            .source(
                "GET /_nodes/stats/process: Σ open_file_descriptors (max: Σ max_file_descriptors)",
            ),
        );
    }

    // Nodes table.
    let mut t = MonitorTable::new(
        "nodes",
        "Nodos del cluster",
        &[
            "nodo",
            "dirección",
            "roles",
            "CPU (%)",
            "carga 1m",
            "memoria (%)",
            "heap (%)",
            "disco usado",
            "disco total",
            "documentos",
            "búsquedas en curso",
        ],
    );
    for node in n.iter().take(MAX_ROWS) {
        let roles = node
            .get("roles")
            .and_then(J::as_arr)
            .map(|r| r.iter().map(J::text).collect::<Vec<_>>().join(", "));
        let total_disk = f(node, &["fs", "total", "total_in_bytes"]);
        let used_disk = total_disk
            .zip(f(node, &["fs", "total", "available_in_bytes"]))
            .map(|(t, a)| t - a);
        t.rows.push(vec![
            cell(node.get("name")),
            cell(node.get("ip").or_else(|| node.get("host"))),
            roles.map(Value::String).unwrap_or(Value::Null),
            opt(f(node, &["os", "cpu", "percent"])),
            opt(f(node, &["os", "cpu", "load_average", "1m"])),
            opt(f(node, &["os", "mem", "used_percent"])),
            opt(f(node, &["jvm", "mem", "heap_used_percent"])),
            opt(used_disk),
            opt(total_disk),
            opt(f(node, &["indices", "docs", "count"])),
            opt(f(node, &["indices", "search", "query_current"])),
        ]);
    }
    s.tables.push(t);

    // Busiest thread pools.
    let mut agg: Vec<(String, [f64; 4])> = Vec::new();
    for p in n
        .iter()
        .filter_map(|node| node.get("thread_pool").and_then(J::as_obj))
    {
        for (name, v) in p {
            let vals = [
                f(v, &["active"]).unwrap_or(0.0),
                f(v, &["queue"]).unwrap_or(0.0),
                f(v, &["rejected"]).unwrap_or(0.0),
                f(v, &["completed"]).unwrap_or(0.0),
            ];
            match agg.iter_mut().find(|(k, _)| k == name) {
                Some((_, a)) => (0..4).for_each(|i| a[i] += vals[i]),
                None => agg.push((name.clone(), vals)),
            }
        }
    }
    agg.retain(|(_, v)| v.iter().any(|x| *x > 0.0));
    agg.sort_by(|a, b| {
        (b.1[0] + b.1[1])
            .total_cmp(&(a.1[0] + a.1[1]))
            .then(b.1[3].total_cmp(&a.1[3]))
    });
    let mut t = MonitorTable::new(
        "waits",
        "Thread pools",
        &["pool", "activos", "en cola", "rechazados", "completados"],
    );
    for (name, v) in agg.into_iter().take(MAX_ROWS) {
        t.rows.push(vec![
            Value::String(name),
            json!(v[0]),
            json!(v[1]),
            json!(v[2]),
            json!(v[3]),
        ]);
    }
    s.tables.push(t);
}

/// `_cat/indices?format=json&bytes=b` → sizes per index.
pub(crate) fn indices(s: &mut MonitorSnapshot, cat: &J, show_system: bool) {
    let mut rows: Vec<(f64, Vec<Value>)> = cat
        .as_arr()
        .unwrap_or(&[])
        .iter()
        .filter(|i| {
            show_system
                || !i
                    .get("index")
                    .and_then(J::as_str)
                    .unwrap_or("")
                    .starts_with('.')
        })
        .map(|i| {
            let size = f(i, &["store.size"]).unwrap_or(0.0);
            (
                size,
                vec![
                    cell(i.get("index")),
                    cell(i.get("health")),
                    cell(i.get("status")),
                    cell(i.get("pri")),
                    cell(i.get("rep")),
                    opt(f(i, &["docs.count"])),
                    opt(f(i, &["docs.deleted"])),
                    json!(size),
                    opt(f(i, &["pri.store.size"])),
                ],
            )
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut t = MonitorTable::new(
        "databases",
        "Índices y tamaños",
        &[
            "índice",
            "salud",
            "estado",
            "primarios",
            "réplicas",
            "documentos",
            "borrados",
            "tamaño",
            "tamaño primario",
        ],
    );
    t.rows = rows.into_iter().take(MAX_ROWS).map(|(_, r)| r).collect();
    s.tables.push(t);
}

/// `_tasks?detailed=true` → running requests (without the listing itself).
pub(crate) fn tasks(tasks: &J) -> MonitorTable {
    let mut rows: Vec<(f64, Vec<Value>)> = Vec::new();
    for (_, node) in tasks.get("nodes").and_then(J::as_obj).into_iter().flatten() {
        let node_name = node.get("name").map(J::text).unwrap_or_default();
        for (id, t) in node.get("tasks").and_then(J::as_obj).into_iter().flatten() {
            let action = t.get("action").map(J::text).unwrap_or_default();
            if action.starts_with("cluster:monitor/tasks/lists") {
                continue;
            }
            let ms = f(t, &["running_time_in_nanos"])
                .map(|ns| ns / 1e6)
                .unwrap_or(0.0);
            let desc = t.get("description").map(J::text).unwrap_or_default();
            rows.push((
                ms,
                vec![
                    Value::String(id.clone()),
                    Value::String(node_name.clone()),
                    Value::String(action),
                    json!((ms * 100.0).round() / 100.0),
                    cell(t.get("cancellable")),
                    cell(t.at(&["headers", "X-Opaque-Id"])),
                    Value::String(crate::http::clip(&desc, 2000)),
                ],
            ));
        }
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut t = MonitorTable::new(
        "queries",
        "Tareas en curso",
        &[
            "tarea",
            "nodo",
            "acción",
            "duración (ms)",
            "cancelable",
            "X-Opaque-Id",
            "descripción",
        ],
    );
    t.rows = rows.into_iter().take(MAX_ROWS).map(|(_, r)| r).collect();
    t
}

pub(crate) fn pending(p: &J) -> MonitorTable {
    let mut t = MonitorTable::new(
        "pending_tasks",
        "Tareas pendientes del cluster",
        &["orden", "prioridad", "origen", "en cola (ms)", "ejecutando"],
    );
    for task in p
        .get("tasks")
        .and_then(J::as_arr)
        .unwrap_or(&[])
        .iter()
        .take(MAX_ROWS)
    {
        t.rows.push(vec![
            cell(task.get("insert_order")),
            cell(task.get("priority")),
            cell(task.get("source")),
            cell(task.get("time_in_queue_millis")),
            cell(task.get("executing")),
        ]);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> J {
        J::parse(s).unwrap()
    }

    #[test]
    fn node_stats_are_summed() {
        let stats = j(r#"{"nodes": {
            "a": {"name": "n1", "os": {"cpu": {"percent": 40}, "mem": {"used_in_bytes": 10, "total_in_bytes": 100}},
                  "process": {"cpu": {"total_in_millis": 2000}},
                  "jvm": {"mem": {"heap_used_in_bytes": 5, "heap_max_in_bytes": 50}, "uptime_in_millis": 9000},
                  "indices": {"search": {"query_total": 7}, "query_cache": {"hit_count": 3, "miss_count": 1}},
                  "thread_pool": {"search": {"active": 1, "queue": 2, "rejected": 0, "completed": 5}}},
            "b": {"name": "n2", "os": {"cpu": {"percent": 20}, "mem": {"used_in_bytes": 30, "total_in_bytes": 100}},
                  "jvm": {"mem": {"heap_used_in_bytes": 5, "heap_max_in_bytes": 50}, "uptime_in_millis": 4000},
                  "indices": {"search": {"query_total": 3}}}
        }}"#);
        let os = j(
            r#"{"nodes": {"a": {"os": {"available_processors": 4, "allocated_processors": 4}},
                                 "b": {"os": {"allocated_processors": 2}}}}"#,
        );
        let mut s = MonitorSnapshot::default();
        from_node_stats(&mut s, &stats, Some(&os));
        let m = |k: &str| s.metrics.iter().find(|m| m.key == k).unwrap();
        assert_eq!(m("cpu").value, Some(30.0));
        // 2 CPU seconds of the only node that reports them, over its 4
        // processors: a share of the whole node, never 200 %.
        assert_eq!(m("cpu_time").value, Some(50.0));
        assert_eq!(m("cpu_time").max, Some(100.0));
        assert!(m("cpu_time").counter);
        assert_eq!(m("mem_used").value, Some(40.0));
        assert_eq!(m("mem_used").max, Some(200.0));
        assert_eq!(m("heap_used").max, Some(100.0));
        assert_eq!(m("queries").value, Some(10.0));
        assert_eq!(m("cache_hit").value, Some(75.0));
        assert_eq!(m("locks_waiting").value, Some(2.0));
        assert_eq!(m("uptime").value, Some(9.0));
        assert_eq!(s.tables[0].rows.len(), 2);
        assert_eq!(s.tables[1].rows[0][0], json!("search"));

        // Without the processor counts there's no honest scale.
        let mut s = MonitorSnapshot::default();
        from_node_stats(&mut s, &stats, None);
        let cpu = s.metrics.iter().find(|m| m.key == "cpu_time").unwrap();
        assert_eq!(cpu.value, None);
    }

    /// Every metric the cluster health and the node stats produce, the
    /// optional ones included, says what it measures and where it comes
    /// from.
    #[test]
    fn every_metric_has_help_and_source() {
        let mut s = MonitorSnapshot::default();
        from_health(
            &mut s,
            &j(
                r#"{"cluster_name": "c", "status": "green", "number_of_nodes": 1, "number_of_data_nodes": 1,
                "active_shards": 3, "unassigned_shards": 0, "relocating_shards": 0, "initializing_shards": 0,
                "active_shards_percent_as_number": 100.0, "number_of_pending_tasks": 0}"#,
            ),
        );
        let stats = j(r#"{"nodes": {"a": {"name": "n1",
            "os": {"cpu": {"percent": 4, "load_average": {"1m": 0.5}}, "mem": {"used_in_bytes": 1, "total_in_bytes": 2}},
            "process": {"cpu": {"total_in_millis": 10}, "open_file_descriptors": 5, "max_file_descriptors": 9},
            "jvm": {"mem": {"heap_used_in_bytes": 1, "heap_max_in_bytes": 2}, "uptime_in_millis": 1000,
                    "gc": {"collectors": {"young": {"collection_time_in_millis": 3}}}},
            "http": {"current_open": 1, "total_opened": 4},
            "indices": {"search": {"query_total": 1, "query_current": 0, "fetch_total": 1, "query_time_in_millis": 1},
                        "indexing": {"index_total": 1, "delete_total": 0, "index_time_in_millis": 1},
                        "get": {"total": 1}, "merges": {"total": 1}, "refresh": {"total": 1},
                        "store": {"size_in_bytes": 1}, "docs": {"count": 1}, "segments": {"count": 1},
                        "query_cache": {"memory_size_in_bytes": 1, "hit_count": 1, "miss_count": 1, "evictions": 0},
                        "request_cache": {"memory_size_in_bytes": 1, "hit_count": 1, "miss_count": 1, "evictions": 0},
                        "fielddata": {"memory_size_in_bytes": 1, "evictions": 0}},
            "transport": {"rx_size_in_bytes": 1, "tx_size_in_bytes": 1},
            "fs": {"total": {"total_in_bytes": 10, "available_in_bytes": 4},
                   "io_stats": {"total": {"read_kilobytes": 1, "write_kilobytes": 1}}},
            "thread_pool": {"search": {"active": 0, "queue": 0, "rejected": 0, "completed": 1}}}}}"#);
        let os = j(r#"{"nodes": {"a": {"os": {"available_processors": 2}}}}"#);
        from_node_stats(&mut s, &stats, Some(&os));
        let keys: Vec<&str> = s.metrics.iter().map(|m| m.key.as_str()).collect();
        for k in [
            "load_1m",
            "gc_time",
            "connections_opened",
            "evictions",
            "disk_read",
            "disk_write",
            "open_files",
        ] {
            assert!(keys.contains(&k), "fixture misses {k}");
        }
        assert_eq!(s.metrics.len(), 42, "{keys:?}");
        for m in &s.metrics {
            assert!(
                m.help.as_deref().is_some_and(|h| !h.trim().is_empty()),
                "{} ({}) has no help",
                m.key,
                m.label
            );
            assert!(
                m.source.as_deref().is_some_and(|h| !h.trim().is_empty()),
                "{} ({}) has no source",
                m.key,
                m.label
            );
            if m.unit == MetricUnit::Percent {
                assert_eq!(m.max, Some(100.0), "{}", m.key);
            }
        }
    }

    #[test]
    fn indices_and_tasks() {
        let mut s = MonitorSnapshot::default();
        indices(
            &mut s,
            &j(
                r#"[{"index": "a", "store.size": "100"}, {"index": ".sys", "store.size": "900"}, {"index": "b", "store.size": "300"}]"#,
            ),
            false,
        );
        assert_eq!(s.tables[0].rows.len(), 2);
        assert_eq!(s.tables[0].rows[0][0], json!("b"));
        let t = tasks(&j(r#"{"nodes": {"x": {"name": "n1", "tasks": {
            "x:1": {"action": "indices:data/read/search", "running_time_in_nanos": 5000000, "description": "q"},
            "x:2": {"action": "cluster:monitor/tasks/lists", "running_time_in_nanos": 1}
        }}}}"#));
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0][3], json!(5.0));
    }
}

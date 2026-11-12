//! Server monitoring: the cluster views of the REST API (`SHOW CLUSTER`,
//! `SHOW REGIONS`, `SHOW QUERIES`, `SHOW VARIABLES`, `SHOW DATABASES
//! DETAILS`, series counts) plus, when the DataNode exposes it, its
//! Prometheus endpoint (`dn_metric_reporter_list=PROMETHEUS`, port 9092)
//! for CPU, memory, connections, disk and network.

use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;

const MAX_ROWS: usize = 200;
const MAX_TEXT: usize = 2000;

/// A statement's answer: column names and rows.
#[derive(Debug, Default, Clone)]
pub struct Answer {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<J>>,
}

impl Answer {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
    }

    pub fn get<'a>(&self, row: &'a [J], name: &str) -> &'a J {
        self.col(name).and_then(|i| row.get(i)).unwrap_or(&J::Null)
    }

    /// The rows as a monitor table with the given columns (source column,
    /// shown title).
    fn table(&self, key: &str, title: &str, cols: &[(&str, &str)]) -> MonitorTable {
        let titles: Vec<&str> = cols.iter().map(|c| c.1).collect();
        let mut t = MonitorTable::new(key, title, &titles);
        t.rows = self
            .rows
            .iter()
            .take(MAX_ROWS)
            .map(|r| cols.iter().map(|c| self.get(r, c.0).clone()).collect())
            .collect();
        t
    }
}

fn number(v: &J) -> Option<f64> {
    match v {
        J::Number(n) => n.as_f64(),
        J::String(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

fn text(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        J::Null => String::new(),
        v => v.to_string(),
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_TEXT {
        return s.to_string();
    }
    let mut t: String = s.chars().take(MAX_TEXT).collect();
    t.push('…');
    t
}

// ---------------------------------------------------------------------------
// Prometheus text format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Sample {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// Samples of a Prometheus exposition (IoTDB writes a trailing comma in
/// the labels: `{a="x",}`).
pub fn parse_prometheus(text: &str) -> Vec<Sample> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Sample> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let name_end = line.find(|c: char| c == '{' || c.is_whitespace())?;
    let name = line[..name_end].to_string();
    let mut rest = &line[name_end..];
    let mut labels = BTreeMap::new();
    if let Some(body) = rest.strip_prefix('{') {
        let mut chars = body.char_indices().peekable();
        let end;
        loop {
            while chars
                .peek()
                .is_some_and(|(_, c)| *c == ',' || c.is_whitespace())
            {
                chars.next();
            }
            let (start, c) = chars.next()?;
            if c == '}' {
                end = start + 1;
                break;
            }
            let mut key = String::from(c);
            for (_, c) in chars.by_ref() {
                if c == '=' {
                    break;
                }
                key.push(c);
            }
            if chars.next()?.1 != '"' {
                return None;
            }
            let mut val = String::new();
            while let Some((_, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            val.push(if n == 'n' { '\n' } else { n });
                        }
                    }
                    '"' => break,
                    c => val.push(c),
                }
            }
            labels.insert(key.trim().to_string(), val);
        }
        rest = &body[end..];
    }
    let value = match rest.split_whitespace().next()? {
        "NaN" => return None,
        "+Inf" | "Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        v => v.parse().ok()?,
    };
    Some(Sample {
        name,
        labels,
        value,
    })
}

pub struct Prom(pub Vec<Sample>);

impl Prom {
    pub fn parse(text: &str) -> Self {
        Prom(parse_prometheus(text))
    }

    pub fn sum_where(
        &self,
        name: &str,
        f: impl Fn(&BTreeMap<String, String>) -> bool,
    ) -> Option<f64> {
        self.0
            .iter()
            .filter(|s| s.name == name && f(&s.labels))
            .fold(None, |a, s| Some(a.unwrap_or(0.0) + s.value))
    }

    pub fn sum(&self, name: &str) -> Option<f64> {
        self.sum_where(name, |_| true)
    }

    pub fn with(&self, name: &str, key: &str, value: &str) -> Option<f64> {
        self.sum_where(name, |l| l.get(key).is_some_and(|v| v == value))
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// What the session fetched; `None` for what failed.
#[derive(Default)]
pub struct Inputs {
    pub version: Option<Answer>,
    pub variables: Option<Answer>,
    pub cluster: Option<Answer>,
    pub regions: Option<Answer>,
    pub queries: Option<Answer>,
    pub databases: Option<Answer>,
    /// `COUNT TIMESERIES root.** GROUP BY LEVEL = 1`.
    pub series: Option<Answer>,
    pub devices: Option<f64>,
    pub prom: Option<Prom>,
    /// Why the metrics endpoint wasn't read, when it wasn't.
    pub prom_note: Option<String>,
}

pub fn snapshot(product: &str, i: &Inputs) -> MonitorSnapshot {
    let mut s = MonitorSnapshot::default();
    let empty = Prom(Vec::new());
    let p = i.prom.as_ref().unwrap_or(&empty);
    let not_lo = |l: &BTreeMap<String, String>| l.get("iface_name").is_none_or(|n| n != "lo");

    // --- metrics from the Prometheus endpoint ---
    let host_total = p.sum("sys_total_physical_memory_size");
    let host_used = host_total
        .zip(p.sum("sys_free_physical_memory_size"))
        .map(|(t, f)| t - f);
    let heap_used = p.sum_where("jvm_memory_used_bytes", |l| {
        l.get("area").is_some_and(|a| a == "heap")
    });
    // Pools without a limit report -1.
    let heap_max =
        p.0.iter()
            .filter(|x| {
                x.name == "jvm_memory_max_bytes"
                    && x.value > 0.0
                    && x.labels.get("area").is_some_and(|a| a == "heap")
            })
            .fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x.value));
    // disk_io_size counts KiB (sectors × sector size ÷ 1024).
    let kib = |v: Option<f64>| v.map(|k| k * 1024.0);
    let files = |kinds: &[&str]| {
        p.sum_where("file_size", |l| {
            l.get("name").is_some_and(|n| kinds.contains(&n.as_str()))
        })
    };
    // Nanoseconds of CPU → seconds × 100, spread over the cores the JVM
    // may use: the rate is the % of all of them (100 % is every one busy),
    // never "500 %" for five busy cores. Unknown core count: not shown
    // rather than per core.
    let cores = p.sum("sys_cpu_cores").filter(|n| *n > 0.0);
    let cpu_ns = p.sum("process_cpu_time");
    let cpu_time = cpu_ns.zip(cores).map(|(ns, n)| ns / 1e9 * 100.0 / n);
    if cpu_ns.is_some() && cores.is_none() {
        s.notes.push("El DataNode no informó cuántos núcleos puede usar (sys_cpu_cores): sin ese dato no se muestra su tiempo de CPU.".into());
    }
    let m = &mut s.metrics;
    m.push(
        Metric::new(
            "cpu",
            "CPU del servidor",
            "CPU",
            MetricUnit::Percent,
            p.sum("sys_cpu_load"),
        )
        .max(Some(100.0))
        .help("Uso de CPU de toda la máquina (o del contenedor) donde corre el DataNode, según la JVM: incluye otros procesos. 100 % es todos los núcleos ocupados.")
        .source("DataNode /metrics: sys_cpu_load"),
    );
    m.push(
        Metric::new(
            "cpu_process",
            "CPU del DataNode",
            "CPU",
            MetricUnit::Percent,
            p.sum("process_cpu_load"),
        )
        .max(Some(100.0))
        .help("Uso de CPU reciente del proceso del DataNode según la JVM, como parte de todos los núcleos de la máquina: 100 % es todos ocupados por IoTDB. Alto y sostenido suele venir de consultas que recorren muchas series, compactaciones o escrituras masivas.")
        .source("DataNode /metrics: process_cpu_load"),
    );
    m.push(
        Metric::new(
            "cpu_time",
            "Tiempo de CPU del DataNode",
            "CPU",
            MetricUnit::Percent,
            cpu_time,
        )
        .max(Some(100.0))
        .counter()
        .help("Tiempo de CPU (usuario + sistema) que consumió el proceso del DataNode, como parte de los núcleos que puede usar: 100 % es todos ocupados. A diferencia de CPU del DataNode, que es la lectura reciente de la JVM, este promedia el intervalo entre dos lecturas del monitor.")
        .source("DataNode /metrics: process_cpu_time (ns) ÷ 1e9 × 100 ÷ sys_cpu_cores"),
    );
    m.push(
        Metric::new(
            "mem_used",
            "Memoria de la JVM (heap)",
            "Memoria",
            MetricUnit::Bytes,
            heap_used.or(p.sum("process_used_mem")),
        )
        .max(heap_max.or(p.sum("process_max_mem")))
        .help("Heap de Java en uso por el DataNode, sobre el máximo configurado (-Xmx). Sube y baja con la recolección de basura; si se queda cerca del tope, las consultas y escrituras se frenan y puede haber errores por falta de memoria.")
        .source("DataNode /metrics: jvm_memory_used_bytes{area=\"heap\"} (max: jvm_memory_max_bytes heap), else process_used_mem (max: process_max_mem)"),
    );
    m.push(
        Metric::new(
            "mem_host",
            "Memoria del servidor",
            "Memoria",
            MetricUnit::Bytes,
            host_used,
        )
        .max(host_total)
        .help("Memoria en uso de la máquina del DataNode: la total menos la libre, según la JVM. Cuenta la caché de archivos del sistema operativo como usada, así que suele verse alta aunque haya memoria disponible.")
        .source("DataNode /metrics: sys_total_physical_memory_size − sys_free_physical_memory_size"),
    );
    m.push(
        Metric::new(
            "connections",
            "Conexiones de clientes",
            "Conexiones",
            MetricUnit::Count,
            p.with("thrift_connections", "name", "ClientRPC"),
        )
        .help("Conexiones Thrift de clientes abiertas en el DataNode: JDBC, la CLI y las sesiones de los SDK. Las llamadas a la API REST, como las de DBine, no cuentan.")
        .source("DataNode /metrics: thrift_connections{name=\"ClientRPC\"}"),
    );
    let running = i.queries.as_ref().map(|q| own_filtered(q).len() as f64);
    m.push(
        Metric::new(
            "active_sessions",
            "Consultas en curso",
            "Conexiones",
            MetricUnit::Count,
            running,
        )
        .help("Consultas que se están ejecutando en el cluster, sin contar la que usa DBine para leer esta lista. Si se acumulan hay consultas lentas: mirá la tabla de consultas en curso.")
        .source("SHOW QUERIES: rows, without its own"),
    );
    m.push(
        Metric::new(
            "threads",
            "Hilos del DataNode",
            "Servidor",
            MetricUnit::Count,
            p.sum("process_threads_count"),
        )
        .help("Hilos de la JVM del DataNode. Si crecen sin parar, suele haber conexiones o tareas que no terminan.")
        .source("DataNode /metrics: process_threads_count"),
    );
    m.push(
        Metric::new(
            "net_in",
            "Red entrante",
            "Red",
            MetricUnit::Bytes,
            p.sum_where("received_bytes", not_lo),
        )
        .counter()
        .help("Bytes que recibió la máquina del DataNode por todas sus interfaces de red salvo loopback, así que incluye el tráfico de otros procesos.")
        .source("DataNode /metrics: received_bytes, every iface_name but lo"),
    );
    m.push(
        Metric::new(
            "net_out",
            "Red saliente",
            "Red",
            MetricUnit::Bytes,
            p.sum_where("transmitted_bytes", not_lo),
        )
        .counter()
        .help("Bytes que envió la máquina del DataNode por todas sus interfaces de red salvo loopback, así que incluye el tráfico de otros procesos.")
        .source("DataNode /metrics: transmitted_bytes, every iface_name but lo"),
    );
    m.push(
        Metric::new(
            "disk_read",
            "Lectura en disco (host)",
            "Disco",
            MetricUnit::Bytes,
            kib(p.with("disk_io_size", "type", "read")),
        )
        .counter()
        .help("Bytes que leyeron los discos de la máquina del DataNode, sumando todos los dispositivos, así que incluye la actividad de otros procesos.")
        .source("DataNode /metrics: disk_io_size{type=\"read\"} (KiB) × 1024, every disk_id"),
    );
    m.push(
        Metric::new(
            "disk_write",
            "Escritura en disco (host)",
            "Disco",
            MetricUnit::Bytes,
            kib(p.with("disk_io_size", "type", "write")),
        )
        .counter()
        .help("Bytes que escribieron los discos de la máquina del DataNode, sumando todos los dispositivos, así que incluye la actividad de otros procesos.")
        .source("DataNode /metrics: disk_io_size{type=\"write\"} (KiB) × 1024, every disk_id"),
    );
    let schema_all = p.sum_where("cache", |l| {
        l.get("name").is_some_and(|n| n == "SchemaCache")
            && l.get("type").is_some_and(|t| t == "all")
    });
    let schema_hit = p.sum_where("cache", |l| {
        l.get("name").is_some_and(|n| n == "SchemaCache")
            && l.get("type").is_some_and(|t| t == "hit")
    });
    let hit = schema_hit
        .zip(schema_all)
        .filter(|(_, a)| *a > 0.0)
        .map(|(h, a)| h / a * 100.0);
    m.push(
        Metric::new(
            "cache_hit",
            "Aciertos de la caché de esquema",
            "Caché",
            MetricUnit::Percent,
            hit,
        )
        .max(Some(100.0))
        .help("Búsquedas de series que la caché de esquema del DataNode resolvió sin ir a la región de esquema, acumulado desde que arrancó. Bajo y sostenido hace más lentas las escrituras y las consultas; puede faltar memoria para el esquema.")
        .source("DataNode /metrics: cache{name=\"SchemaCache\",type=\"hit\"} ÷ cache{name=\"SchemaCache\",type=\"all\"} × 100"),
    );
    let disk_total = p.sum("sys_disk_total_space");
    m.push(
        Metric::new(
            "storage_used",
            "Archivos de datos (TsFile, WAL, mods)",
            "Almacenamiento",
            MetricUnit::Bytes,
            files(&["seq", "unseq", "wal", "mods"]),
        )
        .max(disk_total)
        .help("Lo que ocupan en disco los TsFile secuenciales y no secuenciales, el WAL y los archivos de borrados (mods) de este DataNode, sobre el tamaño de sus discos de datos.")
        .source("DataNode /metrics: file_size{name=\"seq\"|\"unseq\"|\"wal\"|\"mods\"} (max: sys_disk_total_space)"),
    );
    m.push(
        Metric::new(
            "wal_size",
            "WAL",
            "Almacenamiento",
            MetricUnit::Bytes,
            files(&["wal"]),
        )
        .help("Lo que ocupa el registro de escritura anticipada (WAL). Se libera cuando los datos en memoria pasan a TsFile y las réplicas los recibieron; si crece sin parar, los vuelcos no dan abasto o una réplica está atrasada.")
        .source("DataNode /metrics: file_size{name=\"wal\"}"),
    );
    m.push(
        Metric::new(
            "disk_used",
            "Disco del servidor usado",
            "Almacenamiento",
            MetricUnit::Bytes,
            disk_total
                .zip(p.sum("sys_disk_available_space"))
                .map(|(t, a)| t - a),
        )
        .max(disk_total)
        .help("Espacio usado en los discos de datos del DataNode (total menos disponible), incluido lo que no es de IoTDB. Si lo libre baja del umbral de aviso de disco, el DataNode pasa a solo lectura.")
        .source("DataNode /metrics: sys_disk_total_space − sys_disk_available_space"),
    );
    let series_total = i.series.as_ref().map(|a| {
        a.rows
            .iter()
            .filter_map(|r| r.get(1).and_then(number))
            .fold(0.0, |a, b| a + b)
    });
    m.push(
        Metric::new(
            "series",
            "Series temporales",
            "Almacenamiento",
            MetricUnit::Count,
            series_total,
        )
        .help("Series temporales definidas en todas las bases. Cada una ocupa memoria de esquema; muchos millones piden más memoria o más DataNodes.")
        .source("COUNT TIMESERIES root.** GROUP BY LEVEL = 1, summed"),
    );
    m.push(
        Metric::new(
            "devices",
            "Dispositivos",
            "Almacenamiento",
            MetricUnit::Count,
            i.devices,
        )
        .help("Dispositivos definidos en todas las bases: cada entidad (un equipo, un sensor) que agrupa series bajo una misma ruta.")
        .source("COUNT DEVICES root.**"),
    );
    m.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            p.sum("up_time").map(|ns| ns / 1e9),
        )
        .help("Tiempo desde que arrancó el DataNode cuyo endpoint de métricas se consulta. Si vuelve a cero, se reinició.")
        .source("DataNode /metrics: up_time (ns) ÷ 1e9"),
    );

    // --- cluster ---
    if let Some(c) = &i.cluster {
        let status = c.col("Status");
        let total = c.rows.len() as f64;
        let up = c
            .rows
            .iter()
            .filter(|r| {
                status
                    .and_then(|i| r.get(i))
                    .is_some_and(|v| text(v) == "Running")
            })
            .count() as f64;
        s.metrics.push(
            Metric::new(
                "nodes_running",
                "Nodos en ejecución",
                "Cluster",
                MetricUnit::Count,
                Some(up),
            )
            .max(Some(total))
            .help("Nodos del cluster (ConfigNodes y DataNodes) en estado Running, sobre el total. Si baja del total, hay nodos caídos o desconectados: mirá la tabla de nodos.")
            .source("SHOW CLUSTER: rows with Status = Running"),
        );
    }
    if let Some(r) = &i.regions {
        // SHOW REGIONS has a row per replica: each region counts once.
        let count = |ty: &str| {
            let rows = r
                .rows
                .iter()
                .filter(|row| text(r.get(row, "Type")).eq_ignore_ascii_case(ty));
            if r.col("RegionId").is_some() {
                rows.map(|row| text(r.get(row, "RegionId")))
                    .collect::<std::collections::BTreeSet<_>>()
                    .len() as f64
            } else {
                rows.count() as f64
            }
        };
        s.metrics.push(
            Metric::new(
                "data_regions",
                "Regiones de datos",
                "Cluster",
                MetricUnit::Count,
                Some(count("DataRegion")),
            )
            .help("Regiones de datos del cluster, cada una contada una vez aunque tenga varias réplicas. Las bases reparten sus datos entre ellas; más regiones dan más paralelismo pero usan más memoria.")
            .source("SHOW REGIONS: distinct RegionId with Type = DataRegion"),
        );
        s.metrics.push(
            Metric::new(
                "schema_regions",
                "Regiones de esquema",
                "Cluster",
                MetricUnit::Count,
                Some(count("SchemaRegion")),
            )
            .help("Regiones de esquema del cluster, cada una contada una vez aunque tenga varias réplicas: guardan la definición de las series.")
            .source("SHOW REGIONS: distinct RegionId with Type = SchemaRegion"),
        );
    }

    // --- tables ---
    if let Some(q) = &i.queries {
        let mut t = MonitorTable::new(
            "queries",
            "Consultas en curso",
            &["id", "DataNode", "tiempo transcurrido (s)", "consulta"],
        );
        for r in own_filtered(q).into_iter().take(MAX_ROWS) {
            t.rows.push(vec![
                q.get(r, "QueryId").clone(),
                q.get(r, "DataNodeId").clone(),
                q.get(r, "ElapsedTime").clone(),
                json!(truncate(&text(q.get(r, "Statement")))),
            ]);
        }
        s.tables.push(t);
    }
    if let Some(c) = &i.cluster {
        s.tables.push(c.table(
            "nodes",
            "Nodos del cluster",
            &[
                ("NodeID", "id"),
                ("NodeType", "tipo"),
                ("Status", "estado"),
                ("InternalAddress", "dirección interna"),
                ("InternalPort", "puerto interno"),
                ("Version", "versión"),
                ("BuildInfo", "build"),
            ],
        ));
    }
    if let Some(d) = &i.databases {
        let series: BTreeMap<String, J> = i
            .series
            .as_ref()
            .map(|a| {
                a.rows
                    .iter()
                    .map(|r| {
                        (
                            r.first().map(text).unwrap_or_default(),
                            r.get(1).cloned().unwrap_or(J::Null),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut t = MonitorTable::new(
            "databases",
            "Bases de datos",
            &[
                "base",
                "series",
                "TTL (ms)",
                "réplicas de esquema",
                "réplicas de datos",
                "intervalo de partición (ms)",
                "grupos de regiones de datos",
            ],
        );
        for r in d.rows.iter().take(MAX_ROWS) {
            let name = text(d.get(r, "Database"));
            t.rows.push(vec![
                json!(name),
                series.get(&name).cloned().unwrap_or(J::Null),
                d.get(r, "TTL").clone(),
                d.get(r, "SchemaReplicationFactor").clone(),
                d.get(r, "DataReplicationFactor").clone(),
                d.get(r, "TimePartitionInterval").clone(),
                d.get(r, "DataRegionGroupNum").clone(),
            ]);
        }
        s.tables.push(t);
    }
    if let Some(r) = &i.regions {
        s.tables.push(r.table(
            "regions",
            "Regiones",
            &[
                ("RegionId", "id"),
                ("Type", "tipo"),
                ("Status", "estado"),
                ("Database", "base"),
                ("SeriesSlotNum", "slots de series"),
                ("TimeSlotNum", "slots de tiempo"),
                ("DataNodeId", "DataNode"),
                ("RpcAddress", "dirección"),
                ("Role", "rol"),
            ],
        ));
    }

    // --- info ---
    let mut info = |label: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            s.info.push((label.to_string(), v));
        }
    };
    info("Producto", Some(product.to_string()));
    if let Some(v) = &i.version {
        if let Some(r) = v.rows.first() {
            info("Versión", Some(text(v.get(r, "Version"))));
            info("Build", Some(text(v.get(r, "BuildInfo"))));
        }
    }
    if let Some(v) = &i.variables {
        let var = |k: &str| {
            v.rows
                .iter()
                .find(|r| r.first().map(text).as_deref() == Some(k))
                .and_then(|r| r.get(1))
                .map(text)
        };
        for (k, label) in [
            ("ClusterName", "Cluster"),
            ("DataReplicationFactor", "Réplicas de datos"),
            ("SchemaReplicationFactor", "Réplicas de esquema"),
            ("DataRegionConsensusProtocolClass", "Consenso de datos"),
            ("SchemaRegionConsensusProtocolClass", "Consenso de esquema"),
            ("TimePartitionInterval", "Intervalo de partición (ms)"),
            ("TimestampPrecision", "Precisión de tiempo"),
            ("ReadConsistencyLevel", "Consistencia de lectura"),
            ("DiskSpaceWarningThreshold", "Umbral de aviso de disco"),
        ] {
            // Consensus classes by their simple name.
            info(
                label,
                var(k).map(|v| {
                    if v.starts_with("org.") {
                        v.rsplit('.').next().unwrap_or(&v).to_string()
                    } else {
                        v
                    }
                }),
            );
        }
    }
    info(
        "Núcleos del servidor",
        p.sum("sys_cpu_cores").map(|c| c.to_string()),
    );

    // --- notes ---
    match &i.prom {
        None => s.notes.push(i.prom_note.clone().unwrap_or_else(|| {
            "Sin el endpoint de métricas del DataNode no hay CPU, memoria, conexiones, disco ni red.".into()
        })),
        Some(_) => {
            if i.cluster.as_ref().is_some_and(|c| c.rows.len() > 2) {
                s.notes.push("CPU, memoria, disco y red son los del DataNode cuyo endpoint de métricas se consulta, no del cluster entero.".into());
            }
        }
    }
    for (what, got) in [
        ("SHOW CLUSTER", i.cluster.is_some()),
        ("SHOW REGIONS", i.regions.is_some()),
        ("SHOW QUERIES", i.queries.is_some()),
    ] {
        if !got {
            s.notes.push(format!("{what} no respondió: hace falta un usuario con privilegios de mantenimiento (MAINTAIN)."));
        }
    }
    s
}

/// `SHOW QUERIES` rows without the monitor's own statement.
fn own_filtered(q: &Answer) -> Vec<&Vec<J>> {
    q.rows
        .iter()
        .filter(|r| {
            !text(q.get(r, "Statement"))
                .trim()
                .eq_ignore_ascii_case("SHOW QUERIES")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(cols: &[&str], rows: Vec<Vec<J>>) -> Answer {
        Answer {
            columns: cols.iter().map(|c| c.to_string()).collect(),
            rows,
        }
    }

    #[test]
    fn iotdb_prometheus_lines() {
        let p = Prom::parse(
            "# TYPE x gauge\n\
             process_cpu_time{cluster=\"c\",nodeType=\"DATANODE\",nodeId=\"1\",name=\"process\",} 4.49E9\n\
             up_time{} 1.5E10\n\
             received_bytes{type=\"receive\",iface_name=\"lo\",} 100.0\n\
             received_bytes{type=\"receive\",iface_name=\"eth0\",} 7.0\n",
        );
        assert_eq!(p.with("process_cpu_time", "name", "process"), Some(4.49e9));
        assert_eq!(p.sum("up_time"), Some(1.5e10));
        assert_eq!(p.sum("received_bytes"), Some(107.0));
    }

    #[test]
    fn snapshot_from_answers() {
        let prom = Prom::parse(
            "sys_cpu_load{name=\"system\",} 7.5\n\
             process_cpu_time{name=\"process\",} 2E9\n\
             sys_cpu_cores{name=\"system\",} 4\n\
             disk_io_size{type=\"read\",disk_id=\"vda\",} 2.0\n\
             disk_io_size{type=\"read\",disk_id=\"vdb\",} 1.0\n\
             jvm_memory_used_bytes{id=\"G1 Old Gen\",area=\"heap\",} 100.0\n\
             jvm_memory_used_bytes{id=\"Metaspace\",area=\"nonheap\",} 5.0\n\
             jvm_memory_max_bytes{id=\"G1 Old Gen\",area=\"heap\",} 1000.0\n\
             jvm_memory_max_bytes{id=\"G1 Eden\",area=\"heap\",} -1.0\n\
             thrift_connections{name=\"ClientRPC\",} 3.0\n\
             file_size{name=\"seq\",} 10.0\n\
             file_size{name=\"wal\",} 5.0\n\
             file_size{name=\"inner-seq-temp\",} 99.0\n\
             transmitted_bytes{type=\"transmit\",iface_name=\"eth0\",} 9.0\n\
             up_time{} 3E10\n",
        );
        let inputs = Inputs {
            version: Some(answer(
                &["Version", "BuildInfo"],
                vec![vec![json!("1.3.2"), json!("aa0ff4a")]],
            )),
            variables: Some(answer(
                &["Variable", "Value"],
                vec![
                    vec![json!("ClusterName"), json!("defaultCluster")],
                    vec![
                        json!("DataRegionConsensusProtocolClass"),
                        json!("org.apache.iotdb.consensus.iot.IoTConsensus"),
                    ],
                ],
            )),
            cluster: Some(answer(
                &["NodeID", "NodeType", "Status"],
                vec![
                    vec![json!(0), json!("ConfigNode"), json!("Running")],
                    vec![json!(1), json!("DataNode"), json!("Unknown")],
                ],
            )),
            regions: Some(answer(
                &["RegionId", "Type"],
                vec![
                    vec![json!(1), json!("DataRegion")],
                    // A second replica of region 1.
                    vec![json!(1), json!("DataRegion")],
                    vec![json!(2), json!("SchemaRegion")],
                ],
            )),
            queries: Some(answer(
                &["QueryId", "DataNodeId", "ElapsedTime", "Statement"],
                vec![
                    vec![json!("q1"), json!(1), json!(0.05), json!("SHOW QUERIES")],
                    vec![
                        json!("q2"),
                        json!(1),
                        json!(3.2),
                        json!("select * from root.**"),
                    ],
                ],
            )),
            databases: Some(answer(
                &["Database", "TTL"],
                vec![vec![json!("root.a"), json!("INF")]],
            )),
            series: Some(answer(
                &["Column", "count(timeseries)"],
                vec![vec![json!("root.a"), json!(4)]],
            )),
            devices: Some(2.0),
            prom: Some(prom),
            prom_note: None,
        };
        let s = snapshot("Apache IoTDB", &inputs);
        let m = |k: &str| {
            s.metrics
                .iter()
                .find(|m| m.key == k)
                .unwrap_or_else(|| panic!("{k}"))
        };
        assert_eq!(m("cpu").value, Some(7.5));
        // 2 s of CPU over the 4 cores it may use: a second of all four
        // busy adds 100 (%), not 400.
        assert_eq!(m("cpu_time").value, Some(50.0));
        assert_eq!(m("cpu_time").max, Some(100.0));
        assert!(m("cpu_time").counter);
        // disk_io_size is in KiB, summed over the disks.
        assert_eq!(m("disk_read").value, Some(3072.0));
        // Every metric says what it measures and where it comes from; the
        // fixture reaches all of them.
        assert_eq!(s.metrics.len(), 22);
        for x in &s.metrics {
            assert!(
                x.help.as_deref().is_some_and(|h| !h.is_empty()),
                "{}",
                x.key
            );
            assert!(
                x.source.as_deref().is_some_and(|h| !h.is_empty()),
                "{}",
                x.key
            );
        }
        assert_eq!(m("mem_used").value, Some(100.0));
        assert_eq!(m("mem_used").max, Some(1000.0));
        assert_eq!(m("connections").value, Some(3.0));
        assert_eq!(m("storage_used").value, Some(15.0));
        assert_eq!(m("net_out").value, Some(9.0));
        assert_eq!(m("uptime").value, Some(30.0));
        assert_eq!(m("series").value, Some(4.0));
        assert_eq!(m("active_sessions").value, Some(1.0));
        assert_eq!(m("nodes_running").value, Some(1.0));
        assert_eq!(m("nodes_running").max, Some(2.0));
        assert_eq!(m("data_regions").value, Some(1.0));
        let q = s.tables.iter().find(|t| t.key == "queries").unwrap();
        assert_eq!(
            q.rows,
            vec![vec![
                json!("q2"),
                json!(1),
                json!(3.2),
                json!("select * from root.**")
            ]]
        );
        let d = s.tables.iter().find(|t| t.key == "databases").unwrap();
        assert_eq!(d.rows[0][1], json!(4));
        assert!(s
            .info
            .iter()
            .any(|(k, v)| k == "Consenso de datos" && v == "IoTConsensus"));
        assert!(s.info.iter().any(|(k, v)| k == "Versión" && v == "1.3.2"));
    }

    #[test]
    fn cpu_time_needs_the_core_count() {
        let inputs = Inputs {
            prom: Some(Prom::parse("process_cpu_time{name=\"process\",} 2E9\n")),
            ..Default::default()
        };
        let s = snapshot("Apache IoTDB", &inputs);
        let cpu = s.metrics.iter().find(|m| m.key == "cpu_time").unwrap();
        assert_eq!(cpu.value, None);
        assert!(s.notes.iter().any(|n| n.contains("sys_cpu_cores")));
    }

    #[test]
    fn without_metrics_endpoint() {
        let inputs = Inputs {
            prom_note: Some("sin métricas".into()),
            ..Default::default()
        };
        let s = snapshot("TimechoDB", &inputs);
        assert!(s
            .metrics
            .iter()
            .find(|m| m.key == "cpu")
            .unwrap()
            .value
            .is_none());
        assert!(s.notes.iter().any(|n| n == "sin métricas"));
        assert_eq!(s.notes.len(), 4);
    }
}

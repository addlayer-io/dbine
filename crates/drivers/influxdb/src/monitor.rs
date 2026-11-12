//! Server monitoring for the three APIs. Everything here is pure: the
//! sessions fetch (`SHOW STATS` / `SHOW DIAGNOSTICS` / `SHOW QUERIES` on
//! 1.x, `/metrics` and `/health` on 2.x, `/metrics` and the `system`
//! tables on 3.x) and these functions turn the answers into a snapshot.

use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;

/// Longest query text shown in a table.
const MAX_QUERY_TEXT: usize = 2000;
/// Most rows in a table.
const MAX_ROWS: usize = 200;

pub fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_QUERY_TEXT {
        return s.to_string();
    }
    let mut t: String = s.chars().take(MAX_QUERY_TEXT).collect();
    t.push('…');
    t
}

fn opt(v: Option<f64>) -> J {
    v.map_or(J::Null, |x| json!(x))
}

// Help texts shared by more than one API version (Spanish; the UI translates
// them).
const HELP_HEAP: &str = "Bytes del heap de Go asignados a objetos, incluidos los que el recolector todavía no liberó. \
     Sube con consultas grandes y con muchas series; si no baja después de las recolecciones, algo retiene memoria.";
const HELP_GO_SYS: &str = "Memoria que el runtime de Go obtuvo del sistema operativo para el heap, las pilas y sus estructuras internas. \
     Es un techo de la memoria del runtime de Go, no de todo el proceso: los archivos TSM mapeados en memoria no cuentan, \
     y Go no siempre devuelve esta memoria aunque el heap se achique.";
const HELP_GOROUTINES: &str = "Hilos livianos de Go vivos en el servidor. \
     Suben con las conexiones y consultas en curso; si crecen sin parar con carga estable, hay peticiones que quedan colgadas.";
const HELP_HTTP_REQUESTS: &str =
    "Peticiones a la API HTTP por cualquier ruta: escrituras, consultas, administración y salud. \
     Comparalo con las consultas y las escrituras para ver qué tráfico domina.";
const HELP_SERVER_ERRORS: &str =
    "Peticiones HTTP que fallaron por un problema del servidor, no del cliente. \
     Cualquier valor sostenido mayor que cero merece revisar el log de InfluxDB.";
const HELP_WRITE_BYTES: &str = "Bytes de line protocol recibidos en escrituras. \
     Mide el volumen de ingesta; no incluye las consultas.";
const HELP_UPTIME_INFLUXD: &str =
    "Tiempo desde que arrancó el proceso influxd. Si vuelve a cero, el servidor se reinició.";

fn sum(values: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    values
        .into_iter()
        .flatten()
        .fold(None, |acc, v| Some(acc.unwrap_or(0.0) + v))
}

// ---------------------------------------------------------------------------
// Prometheus text format (2.x and 3.x `/metrics`)
// ---------------------------------------------------------------------------

/// One sample: name, labels and value.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// Samples of a Prometheus text exposition; comments and unparsable lines
/// are skipped.
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
    if rest.starts_with('{') {
        let chars: Vec<char> = rest.chars().collect();
        let mut i = 1;
        loop {
            while i < chars.len() && (chars[i] == ',' || chars[i].is_whitespace()) {
                i += 1;
            }
            if i >= chars.len() {
                return None;
            }
            if chars[i] == '}' {
                i += 1;
                break;
            }
            let start = i;
            while i < chars.len() && chars[i] != '=' {
                i += 1;
            }
            let key: String = chars[start..i]
                .iter()
                .collect::<String>()
                .trim()
                .to_string();
            i += 1; // '='
            if chars.get(i) != Some(&'"') {
                return None;
            }
            i += 1;
            let mut val = String::new();
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    val.push(match chars[i] {
                        'n' => '\n',
                        c => c,
                    });
                } else {
                    val.push(chars[i]);
                }
                i += 1;
            }
            i += 1; // closing quote
            labels.insert(key, val);
        }
        let consumed: usize = chars[..i].iter().map(|c| c.len_utf8()).sum();
        rest = &rest[consumed..];
    }
    let value = rest.split_whitespace().next()?;
    let value = match value {
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

/// Lookups over parsed samples.
pub struct Prom(pub Vec<Sample>);

impl Prom {
    pub fn parse(text: &str) -> Self {
        Prom(parse_prometheus(text))
    }

    /// The sum of every sample of a metric, `None` when it's absent.
    pub fn sum(&self, name: &str) -> Option<f64> {
        self.sum_where(name, |_| true)
    }

    /// The sum of the samples of a metric whose labels pass `f`.
    pub fn sum_where(
        &self,
        name: &str,
        f: impl Fn(&BTreeMap<String, String>) -> bool,
    ) -> Option<f64> {
        sum(self
            .0
            .iter()
            .filter(|s| s.name == name && f(&s.labels))
            .map(|s| Some(s.value)))
    }

    /// The sample of a metric with a label equal to a value.
    pub fn with(&self, name: &str, key: &str, value: &str) -> Option<f64> {
        self.sum_where(name, |l| l.get(key).map(String::as_str) == Some(value))
    }

    pub fn labels(&self, name: &str) -> Option<&BTreeMap<String, String>> {
        self.0.iter().find(|s| s.name == name).map(|s| &s.labels)
    }

    pub fn iter<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Sample> + 'a {
        self.0.iter().filter(move |s| s.name == name)
    }
}

// ---------------------------------------------------------------------------
// InfluxDB 1.x
// ---------------------------------------------------------------------------

/// A Go duration (`1h2m3.5s`, `12.7s`, `500ms`, `3µs`) in seconds.
pub fn go_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s == "0" {
        return Some(0.0);
    }
    let mut total = 0.0;
    let mut rest = s;
    while !rest.is_empty() {
        let n_end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let n: f64 = rest[..n_end].parse().ok()?;
        rest = &rest[n_end..];
        let u_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let factor = match &rest[..u_end] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 1e-3,
            "us" | "µs" | "μs" => 1e-6,
            "ns" => 1e-9,
            _ => return None,
        };
        total += n * factor;
        rest = &rest[u_end..];
    }
    Some(total)
}

/// One series of a `SHOW STATS` / `SHOW DIAGNOSTICS` answer: its name, tags
/// and first row as column → value.
pub struct StatSeries {
    pub name: String,
    pub tags: BTreeMap<String, String>,
    pub values: BTreeMap<String, J>,
}

impl StatSeries {
    pub fn num(&self, col: &str) -> Option<f64> {
        match self.values.get(col)? {
            J::Number(n) => n.as_f64(),
            J::String(s) => s.parse().ok(),
            J::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn text(&self, col: &str) -> Option<String> {
        match self.values.get(col)? {
            J::String(s) => Some(s.clone()),
            J::Null => None,
            v => Some(v.to_string()),
        }
    }
}

pub fn stat_series(result: &J) -> Vec<StatSeries> {
    let mut out = Vec::new();
    for s in result
        .get("series")
        .and_then(|s| s.as_array())
        .into_iter()
        .flatten()
    {
        let cols: Vec<String> = s
            .get("columns")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect();
        let row = s
            .get("values")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .and_then(|r| r.as_array());
        let values = cols
            .iter()
            .cloned()
            .zip(row.into_iter().flatten().cloned())
            .collect();
        let tags = s
            .get("tags")
            .and_then(|t| t.as_object())
            .map(|t| {
                t.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.push(StatSeries {
            name: s
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string(),
            tags,
            values,
        });
    }
    out
}

/// Snapshot of InfluxDB 1.x from the answers of `SHOW STATS`,
/// `SHOW DIAGNOSTICS` and `SHOW QUERIES` (each `None` when it failed).
pub fn v1_snapshot(stats: Option<&J>, diag: Option<&J>, queries: Option<&J>) -> MonitorSnapshot {
    let mut s = MonitorSnapshot::default();
    let stats = stats.map(stat_series).unwrap_or_default();
    let diag = diag.map(stat_series).unwrap_or_default();
    let one = |name: &str| stats.iter().find(|x| x.name == name);
    let all = |name: &'static str| stats.iter().filter(move |x| x.name == name);
    let d = |name: &str| diag.iter().find(|x| x.name == name);
    let n = |series: Option<&StatSeries>, col: &str| series.and_then(|x| x.num(col));

    let runtime = one("runtime");
    let httpd = one("httpd");
    let qe = one("queryExecutor");
    let write = one("write");
    let max_conn = n(d("config-httpd"), "max-connection-limit").filter(|m| *m > 0.0);

    let m = &mut s.metrics;
    m.push(
        Metric::new(
            "mem_used",
            "Memoria en uso (heap)",
            "Memoria",
            MetricUnit::Bytes,
            n(runtime, "HeapAlloc"),
        )
        .help(HELP_HEAP)
        .source("SHOW STATS: runtime.HeapAlloc"),
    );
    m.push(
        Metric::new(
            "mem_sys",
            "Memoria reservada al SO",
            "Memoria",
            MetricUnit::Bytes,
            n(runtime, "Sys"),
        )
        .help(HELP_GO_SYS)
        .source("SHOW STATS: runtime.Sys"),
    );
    m.push(
        Metric::new(
            "mem_cache",
            "Caché de escritura (TSM)",
            "Memoria",
            MetricUnit::Bytes,
            sum(all("tsm1_cache").map(|x| x.num("memBytes"))),
        )
        .help(
            "Bytes que ocupan en memoria los puntos recién escritos que todavía no se volcaron a archivos TSM, sumados entre shards. \
             Crece con el ritmo de escritura; si un shard llega a cache-max-memory-size, InfluxDB rechaza escrituras.",
        )
        .source("SHOW STATS: Σ tsm1_cache.memBytes"),
    );
    m.push(
        Metric::new(
            "gc",
            "Recolecciones de basura",
            "Memoria",
            MetricUnit::Count,
            n(runtime, "NumGC"),
        )
        .counter()
        .help(
            "Ciclos completos del recolector de basura de Go. \
             Muchos por segundo indican que el servidor asigna y descarta memoria a un ritmo alto, típico de consultas que leen muchos puntos.",
        )
        .source("SHOW STATS: runtime.NumGC"),
    );
    m.push(
        Metric::new(
            "connections",
            "Peticiones HTTP activas",
            "Conexiones",
            MetricUnit::Count,
            n(httpd, "reqActive"),
        )
        .max(max_conn)
        .help(
            "Peticiones HTTP que el servidor está atendiendo ahora: consultas, escrituras y pings. \
             El máximo es max-connection-limit cuando está configurado; al alcanzarlo, InfluxDB rechaza conexiones nuevas.",
        )
        .source(
            "SHOW STATS: httpd.reqActive (max: SHOW DIAGNOSTICS config-httpd.max-connection-limit)",
        ),
    );
    m.push(
        Metric::new(
            "active_sessions",
            "Consultas en curso",
            "Conexiones",
            MetricUnit::Count,
            n(qe, "queriesActive"),
        )
        .help(
            "Consultas que el servidor está ejecutando ahora. \
             Si se mantienen altas, hay consultas lentas o llegan más de las que alcanza a resolver (el límite, si hay, es max-concurrent-queries).",
        )
        .source("SHOW STATS: queryExecutor.queriesActive"),
    );
    m.push(
        Metric::new(
            "queries",
            "Consultas",
            "Actividad",
            MetricUnit::Count,
            n(qe, "queriesExecuted"),
        )
        .counter()
        .help(
            "Consultas que ejecutó el servidor, incluidas las SHOW con que DBine arma este monitor. \
             Un salto sin cambios en la aplicación suele ser un panel o un script que consulta más seguido.",
        )
        .source("SHOW STATS: queryExecutor.queriesExecuted"),
    );
    m.push(
        Metric::new(
            "requests",
            "Peticiones HTTP",
            "Actividad",
            MetricUnit::Count,
            n(httpd, "req"),
        )
        .counter()
        .help(HELP_HTTP_REQUESTS)
        .source("SHOW STATS: httpd.req"),
    );
    m.push(
        Metric::new(
            "rows_written",
            "Puntos escritos",
            "Actividad",
            MetricUnit::Count,
            n(write, "pointReq"),
        )
        .counter()
        .help(
            "Puntos que recibió el servidor para escribir, incluidos los que después fallan y los que guarda el monitoreo interno (_internal) si está activo. \
             Mide la ingesta.",
        )
        .source("SHOW STATS: write.pointReq"),
    );
    m.push(
        Metric::new(
            "errors",
            "Errores del servidor (HTTP 5xx)",
            "Actividad",
            MetricUnit::Count,
            n(httpd, "serverError"),
        )
        .counter()
        .help(HELP_SERVER_ERRORS)
        .source("SHOW STATS: httpd.serverError"),
    );
    m.push(
        Metric::new(
            "net_in",
            "Datos recibidos (escrituras)",
            "Red",
            MetricUnit::Bytes,
            n(httpd, "writeReqBytes"),
        )
        .counter()
        .help(HELP_WRITE_BYTES)
        .source("SHOW STATS: httpd.writeReqBytes"),
    );
    m.push(
        Metric::new(
            "net_out",
            "Datos enviados (consultas)",
            "Red",
            MetricUnit::Bytes,
            n(httpd, "queryRespBytes"),
        )
        .counter()
        .help(
            "Bytes de las respuestas a consultas enviadas a los clientes. \
             Un valor alto apunta a consultas que devuelven muchos puntos sin agregar.",
        )
        .source("SHOW STATS: httpd.queryRespBytes"),
    );
    m.push(
        Metric::new(
            "storage_used",
            "Espacio usado (shards y WAL)",
            "Almacenamiento",
            MetricUnit::Bytes,
            sum(all("shard").map(|x| x.num("diskBytes"))),
        )
        .help(
            "Tamaño en disco de todos los shards, archivos TSM y WAL incluidos. \
             Crece con la ingesta y baja cuando la política de retención borra shards viejos.",
        )
        .source("SHOW STATS: Σ shard.diskBytes"),
    );
    m.push(
        Metric::new(
            "series",
            "Series",
            "Almacenamiento",
            MetricUnit::Count,
            sum(all("database").map(|x| x.num("numSeries"))),
        )
        .help(
            "Series distintas (measurement más combinación de tags) de todas las bases. \
             Es lo que más memoria pide el índice: un crecimiento sostenido suele venir de un tag con valores únicos, como ids o fechas.",
        )
        .source("SHOW STATS: Σ database.numSeries"),
    );
    let active_compactions = sum(all("tsm1_engine").flat_map(|x| {
        x.values
            .keys()
            .filter(|k| k.ends_with("CompactionsActive"))
            .map(|k| x.num(k))
            .collect::<Vec<_>>()
    }));
    m.push(
        Metric::new(
            "compactions",
            "Compactaciones activas",
            "Almacenamiento",
            MetricUnit::Count,
            active_compactions,
        )
        .help(
            "Compactaciones de TSM en ejecución ahora, de todos los niveles y shards. \
             Es normal verlas después de picos de escritura; si nunca bajan a cero, el disco no alcanza a seguir la ingesta.",
        )
        .source("SHOW STATS: Σ tsm1_engine.*CompactionsActive"),
    );
    m.push(
        Metric::new(
            "goroutines",
            "Goroutines",
            "Servidor",
            MetricUnit::Count,
            n(runtime, "NumGoroutine"),
        )
        .help(HELP_GOROUTINES)
        .source("SHOW STATS: runtime.NumGoroutine"),
    );
    let uptime = d("system")
        .and_then(|x| x.text("uptime"))
        .and_then(|u| go_duration(&u));
    m.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            uptime,
        )
        .help(HELP_UPTIME_INFLUXD)
        .source("SHOW DIAGNOSTICS: system.uptime"),
    );

    // Databases: measurements and series per database, disk per shard.
    let mut disk: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for sh in all("shard") {
        let e = disk
            .entry(sh.tags.get("database").cloned().unwrap_or_default())
            .or_default();
        e.0 += sh.num("diskBytes").unwrap_or(0.0);
        e.1 += 1;
    }
    let mut dbs = MonitorTable::new(
        "databases",
        "Bases y tamaños",
        &[
            "base",
            "measurements",
            "series",
            "shards",
            "tamaño en disco (bytes)",
        ],
    );
    let mut rows: Vec<(f64, Vec<J>)> = all("database")
        .map(|x| {
            let name = x.tags.get("database").cloned().unwrap_or_default();
            let (bytes, shards) = disk.get(&name).copied().unwrap_or_default();
            (
                bytes,
                vec![
                    json!(name),
                    opt(x.num("numMeasurements")),
                    opt(x.num("numSeries")),
                    json!(shards),
                    json!(bytes),
                ],
            )
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    dbs.rows = rows.into_iter().take(MAX_ROWS).map(|r| r.1).collect();

    let mut running = MonitorTable::new(
        "queries",
        "Consultas en curso",
        &["id", "base", "duración", "estado", "consulta"],
    );
    for r in queries.map(stat_rows).unwrap_or_default() {
        let q = r.get("query").and_then(|v| v.as_str()).unwrap_or_default();
        if q.trim().eq_ignore_ascii_case("SHOW QUERIES") {
            continue;
        }
        running.rows.push(vec![
            r.get("qid").cloned().unwrap_or(J::Null),
            r.get("database").cloned().unwrap_or(J::Null),
            r.get("duration").cloned().unwrap_or(J::Null),
            r.get("status").cloned().unwrap_or(J::Null),
            json!(truncate(q)),
        ]);
        if running.rows.len() >= MAX_ROWS {
            break;
        }
    }
    if queries.is_some() {
        s.tables.push(running);
    }
    if !stats.is_empty() {
        s.tables.push(dbs);
    }

    // Info from SHOW DIAGNOSTICS.
    let mut info = |label: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            s.info.push((label.to_string(), v));
        }
    };
    let dt = |series: &str, col: &str| d(series).and_then(|x| x.text(col));
    info("Versión", dt("build", "Version"));
    info(
        "Rama / commit",
        dt("build", "Branch")
            .map(|b| format!("{b} / {}", dt("build", "Commit").unwrap_or_default())),
    );
    info("Host", dt("network", "hostname"));
    info(
        "Sistema",
        dt("runtime", "GOOS")
            .map(|os| format!("{os}/{}", dt("runtime", "GOARCH").unwrap_or_default())),
    );
    info("Núcleos usables (GOMAXPROCS)", dt("runtime", "GOMAXPROCS"));
    info("Go", dt("runtime", "version"));
    info(
        "Iniciado",
        dt("system", "started").map(|t| crate::http::iso_time(&t).unwrap_or(t)),
    );
    info(
        "Caché máxima por shard (bytes)",
        dt("config-data", "cache-max-memory-size"),
    );
    info(
        "Máximo de series por base",
        dt("config-data", "max-series-per-database"),
    );
    info(
        "Consultas concurrentes máximas",
        dt("config-coordinator", "max-concurrent-queries").map(zero_unlimited),
    );
    info(
        "Tiempo máximo de consulta",
        dt("config-coordinator", "query-timeout").map(|t| {
            if t == "0s" {
                "sin límite".into()
            } else {
                t
            }
        }),
    );
    info(
        "Conexiones HTTP máximas",
        dt("config-httpd", "max-connection-limit").map(zero_unlimited),
    );
    info(
        "Monitoreo interno",
        dt("config-monitor", "store-enabled").map(|e| {
            if e == "true" {
                format!(
                    "guarda en «{}» cada {}",
                    dt("config-monitor", "store-database").unwrap_or_default(),
                    dt("config-monitor", "store-interval").unwrap_or_default()
                )
            } else {
                "desactivado".into()
            }
        }),
    );

    s.notes.push(
        "InfluxDB 1 no informa el uso de CPU (ni del host ni del proceso) por su API.".into(),
    );
    if stats.is_empty() {
        s.notes.push("SHOW STATS no respondió: hace falta un usuario administrador para ver las estadísticas del servidor.".into());
    }
    if diag.is_empty() {
        s.notes.push("SHOW DIAGNOSTICS no respondió: hace falta un usuario administrador para ver la versión y la configuración.".into());
    }
    if queries.is_none() {
        s.notes.push("SHOW QUERIES no respondió: hace falta un usuario administrador para ver las consultas en curso.".into());
    }
    s
}

fn zero_unlimited(v: String) -> String {
    if v == "0" {
        "sin límite".into()
    } else {
        v
    }
}

/// Every row of every series as column → value.
fn stat_rows(result: &J) -> Vec<BTreeMap<String, J>> {
    let mut out = Vec::new();
    for s in result
        .get("series")
        .and_then(|s| s.as_array())
        .into_iter()
        .flatten()
    {
        let cols: Vec<String> = s
            .get("columns")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect();
        for r in s
            .get("values")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            out.push(
                cols.iter()
                    .cloned()
                    .zip(r.as_array().into_iter().flatten().cloned())
                    .collect(),
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// InfluxDB 2.x
// ---------------------------------------------------------------------------

/// Snapshot of InfluxDB 2.x from `/metrics` (`None` if it failed), the
/// `/health` answer and the bucket names by id.
pub fn v2_snapshot(
    prom: Option<&Prom>,
    health: Option<&J>,
    buckets: &BTreeMap<String, String>,
) -> MonitorSnapshot {
    let mut s = MonitorSnapshot::default();
    let empty = Prom(Vec::new());
    let p = prom.unwrap_or(&empty);
    let build = p.labels("influxdb_info");
    let b = |k: &str| build.and_then(|l| l.get(k)).cloned();
    // CPU seconds over every core the server sees (`runtime.NumCPU`): the
    // rate is then the share of all of them, not "500 %" for five cores.
    let cpus = b("cpus")
        .and_then(|c| c.trim().parse::<f64>().ok())
        .filter(|c| *c > 0.0);
    let m = &mut s.metrics;
    m.push(
        Metric::new(
            "cpu_time",
            "CPU del proceso",
            "CPU",
            MetricUnit::Percent,
            p.sum("process_cpu_seconds_total")
                .zip(cpus)
                .map(|(v, c)| v * 100.0 / c),
        )
        .counter()
        .max(Some(100.0))
        .help(
            "Tiempo de CPU del proceso influxd (usuario y sistema) como porcentaje de todos los núcleos que ve el servidor: 100 % es todos ocupados. \
             Sostenido cerca del 100 %, las consultas y las escrituras compiten por CPU.",
        )
        .source("GET /metrics: process_cpu_seconds_total × 100 / influxdb_info{cpus}"),
    );
    m.push(
        Metric::new(
            "mem_used",
            "Memoria en uso (heap)",
            "Memoria",
            MetricUnit::Bytes,
            p.sum("go_memstats_alloc_bytes"),
        )
        .help(HELP_HEAP)
        .source("GET /metrics: go_memstats_alloc_bytes"),
    );
    m.push(
        Metric::new(
            "mem_sys",
            "Memoria reservada al SO",
            "Memoria",
            MetricUnit::Bytes,
            p.sum("go_memstats_sys_bytes"),
        )
        .help(HELP_GO_SYS)
        .source("GET /metrics: go_memstats_sys_bytes"),
    );
    m.push(
        Metric::new(
            "mem_cache",
            "Caché de escritura (TSM)",
            "Memoria",
            MetricUnit::Bytes,
            p.sum("storage_cache_inuse_bytes"),
        )
        .help(
            "Bytes que ocupan en memoria los puntos recién escritos que todavía no se volcaron a archivos TSM, sumados entre shards. \
             Crece con el ritmo de escritura; si un shard llega a storage-cache-max-memory-size, InfluxDB rechaza escrituras.",
        )
        .source("GET /metrics: Σ storage_cache_inuse_bytes"),
    );
    // The query controller's series appear with the first Flux query.
    let zero = prom.map(|_| 0.0);
    let executing = p.sum("qc_executing_active").or(zero);
    m.push(
        Metric::new(
            "active_sessions",
            "Consultas en ejecución",
            "Conexiones",
            MetricUnit::Count,
            executing,
        )
        .help(
            "Consultas Flux que el controlador de consultas está ejecutando ahora. \
             Si se mantienen en el límite (query-concurrency), las siguientes esperan en cola.",
        )
        .source("GET /metrics: qc_executing_active"),
    );
    m.push(
        Metric::new(
            "queued",
            "Consultas en cola",
            "Conexiones",
            MetricUnit::Count,
            p.sum("qc_queueing_active").or(zero),
        )
        .help(
            "Consultas Flux que esperan turno porque ya se ejecutan tantas como permite query-concurrency. \
             Una cola sostenida significa que llegan más consultas de las que el servidor admite a la vez.",
        )
        .source("GET /metrics: qc_queueing_active"),
    );
    let queries = p
        .sum("qc_requests_total")
        .or_else(|| {
            p.sum_where("http_api_requests_total", |l| {
                l.get("path").is_some_and(|x| x.ends_with("/query"))
            })
        })
        .or(zero);
    m.push(
        Metric::new(
            "queries",
            "Consultas",
            "Actividad",
            MetricUnit::Count,
            queries,
        )
        .counter()
        .help(
            "Consultas Flux que recibió el controlador de consultas, incluidas las de tareas y paneles; si el servidor todavía no publica esa cifra (aparece con la primera consulta Flux), \
             cuenta las peticiones HTTP a rutas que terminan en /query. \
             Un salto sin cambios en la aplicación suele ser un panel o una tarea que consulta más seguido.",
        )
        .source(
            "GET /metrics: Σ qc_requests_total (fallback: Σ http_api_requests_total{path=~\".*/query\"})",
        ),
    );
    m.push(
        Metric::new(
            "requests",
            "Peticiones HTTP",
            "Actividad",
            MetricUnit::Count,
            p.sum("http_api_requests_total"),
        )
        .counter()
        .help(HELP_HTTP_REQUESTS)
        .source("GET /metrics: Σ http_api_requests_total"),
    );
    m.push(
        Metric::new(
            "errors",
            "Errores del servidor (HTTP 5xx)",
            "Actividad",
            MetricUnit::Count,
            p.sum_where("http_api_requests_total", |l| {
                l.get("status").is_some_and(|x| x == "5XX")
            })
            .or(zero),
        )
        .counter()
        .help(HELP_SERVER_ERRORS)
        .source("GET /metrics: Σ http_api_requests_total{status=\"5XX\"}"),
    );
    m.push(
        Metric::new(
            "rows_written",
            "Puntos escritos",
            "Actividad",
            MetricUnit::Count,
            p.sum("storage_writer_ok_points_sum"),
        )
        .counter()
        .help(
            "Puntos guardados con éxito en los shards. \
             Mide la ingesta real; si cae mientras los clientes siguen escribiendo, revisá los errores del servidor.",
        )
        .source("GET /metrics: Σ storage_writer_ok_points_sum"),
    );
    m.push(
        Metric::new(
            "tasks_active",
            "Tareas en ejecución",
            "Actividad",
            MetricUnit::Count,
            p.sum("task_executor_total_runs_active"),
        )
        .help(
            "Ejecuciones de tareas programadas (tasks) en curso ahora. \
             Si se acumulan, las tareas tardan más que su intervalo.",
        )
        .source("GET /metrics: task_executor_total_runs_active"),
    );
    let disk = sum([p.sum("storage_shard_disk_size"), p.sum("storage_wal_size")]);
    m.push(
        Metric::new(
            "storage_used",
            "Espacio usado (shards y WAL)",
            "Almacenamiento",
            MetricUnit::Bytes,
            disk,
        )
        .help(
            "Tamaño en disco de los archivos TSM y el WAL de todos los buckets. \
             Crece con la ingesta y baja cuando la retención borra shards viejos.",
        )
        .source("GET /metrics: Σ storage_shard_disk_size + Σ storage_wal_size"),
    );
    m.push(
        Metric::new(
            "series",
            "Series",
            "Almacenamiento",
            MetricUnit::Count,
            p.sum("storage_shard_series"),
        )
        .help(
            "Series sumadas shard por shard: una serie con datos en varios shards cuenta una vez en cada uno, así que puede superar a las series distintas. \
             Un crecimiento sostenido suele venir de un tag con valores únicos, como ids o fechas.",
        )
        .source("GET /metrics: Σ storage_shard_series"),
    );
    m.push(
        Metric::new(
            "compactions",
            "Compactaciones en cola",
            "Almacenamiento",
            MetricUnit::Count,
            p.sum("storage_compactions_queued"),
        )
        .help(
            "Compactaciones de TSM que esperan turno, de todos los niveles y shards. \
             Si la cola no baja, el disco no alcanza a seguir la ingesta.",
        )
        .source("GET /metrics: Σ storage_compactions_queued"),
    );
    m.push(
        Metric::new(
            "goroutines",
            "Goroutines",
            "Servidor",
            MetricUnit::Count,
            p.sum("go_goroutines"),
        )
        .help(HELP_GOROUTINES)
        .source("GET /metrics: go_goroutines"),
    );
    m.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            p.sum("influxdb_uptime_seconds"),
        )
        .help(HELP_UPTIME_INFLUXD)
        .source("GET /metrics: influxdb_uptime_seconds"),
    );

    // Buckets: disk, series and cache per bucket id (shards summed).
    let mut per: BTreeMap<String, [f64; 4]> = BTreeMap::new();
    for (name, i) in [
        ("storage_shard_disk_size", 0),
        ("storage_wal_size", 1),
        ("storage_shard_series", 2),
        ("storage_cache_inuse_bytes", 3),
    ] {
        for x in p.iter(name) {
            if let Some(b) = x.labels.get("bucket") {
                per.entry(b.clone()).or_default()[i] += x.value;
            }
        }
    }
    if prom.is_some() {
        let mut t = MonitorTable::new(
            "databases",
            "Buckets y tamaños",
            &[
                "bucket",
                "id",
                "tamaño en disco (bytes)",
                "WAL (bytes)",
                "series",
                "caché (bytes)",
            ],
        );
        let mut named: Vec<(String, String, [f64; 4])> = per
            .into_iter()
            .map(|(id, v)| (buckets.get(&id).cloned().unwrap_or_default(), id, v))
            .collect();
        named.sort_by(|a, b| (b.2[0] + b.2[1]).total_cmp(&(a.2[0] + a.2[1])));
        for (name, id, v) in named.into_iter().take(MAX_ROWS) {
            t.rows.push(vec![
                json!(name),
                json!(id),
                json!(v[0]),
                json!(v[1]),
                json!(v[2]),
                json!(v[3]),
            ]);
        }
        s.tables.push(t);

        // Requests by endpoint.
        let mut by: BTreeMap<(String, String, String), f64> = BTreeMap::new();
        for x in p.iter("http_api_requests_total") {
            let l = |k: &str| x.labels.get(k).cloned().unwrap_or_default();
            *by.entry((l("path"), l("method"), l("status"))).or_default() += x.value;
        }
        let mut reqs: Vec<_> = by.into_iter().collect();
        reqs.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut t = MonitorTable::new(
            "requests",
            "Peticiones HTTP por ruta",
            &["ruta", "método", "estado", "total"],
        );
        t.rows = reqs
            .into_iter()
            .take(50)
            .map(|((path, method, status), n)| {
                vec![json!(path), json!(method), json!(status), json!(n)]
            })
            .collect();
        s.tables.push(t);
    }

    let mut info = |label: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            s.info.push((label.to_string(), v));
        }
    };
    let h = |k: &str| {
        health
            .and_then(|h| h.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    info("Versión", h("version").or(b("version")));
    info("Commit", h("commit").or(b("commit")));
    info(
        "Estado",
        h("status").map(|st| format!("{st} — {}", h("message").unwrap_or_default())),
    );
    info(
        "Sistema",
        b("os").map(|os| format!("{os}/{}", b("arch").unwrap_or_default())),
    );
    info("Núcleos", b("cpus"));
    info("Compilado", b("build_date"));
    for (metric, label) in [
        ("influxdb_organizations_total", "Organizaciones"),
        ("influxdb_buckets_total", "Buckets"),
        ("influxdb_users_total", "Usuarios"),
        ("influxdb_tokens_total", "Tokens"),
        ("influxdb_dashboards_total", "Dashboards"),
    ] {
        info(label, p.sum(metric).map(|v| format!("{v}")));
    }

    if prom.is_none() {
        s.notes.push(
            "No se pudo leer /metrics: el token necesita permiso de lectura sobre las métricas (o están desactivadas con --metrics-disabled)."
                .into(),
        );
    } else if p.sum("process_cpu_seconds_total").is_none() {
        s.notes.push("Esta compilación de InfluxDB no publica process_cpu_seconds_total: no hay CPU del proceso.".into());
    } else if cpus.is_none() {
        s.notes.push("InfluxDB 2 no informó cuántos núcleos ve (influxdb_info{cpus}): sin ese dato no se puede expresar la CPU del proceso como parte del servidor.".into());
    }
    s.notes
        .push("InfluxDB 2 no informa el uso de CPU del host.".into());
    s.notes.push("InfluxDB 2 OSS no lista las consultas en curso; solo informa cuántas hay en ejecución y en cola.".into());
    s
}

// ---------------------------------------------------------------------------
// InfluxDB 3
// ---------------------------------------------------------------------------

/// Phases of a query that hasn't finished yet (`influxdb_iox_query_log_phase_current`).
const IN_FLIGHT: &[&str] = &["received", "planned", "logically_planned", "permit"];

/// Snapshot of InfluxDB 3 from `/metrics` (`None` if it failed) and the
/// `/ping` answer. The `system` tables are added by the session.
pub fn v3_snapshot(prom: Option<&Prom>, ping: Option<&J>, now_secs: f64) -> MonitorSnapshot {
    let mut s = MonitorSnapshot::default();
    let empty = Prom(Vec::new());
    let p = prom.unwrap_or(&empty);
    // Busy seconds of every Tokio worker thread over the number of them
    // (both runtimes, "main" and "datafusion"): the rate is the share of the
    // workers that is busy, 100 = all of them, not "100 % per core".
    let workers = p.sum("tokio_runtime_num_workers").filter(|w| *w > 0.0);
    let m = &mut s.metrics;
    m.push(
        Metric::new(
            "cpu_time",
            "CPU de los workers",
            "CPU",
            MetricUnit::Percent,
            p.sum("tokio_worker_total_busy_duration_seconds_total")
                .zip(workers)
                .map(|(v, w)| v * 100.0 / w),
        )
        .counter()
        .max(Some(100.0))
        .help(
            "Parte del tiempo en que están ocupados los hilos workers de los dos runtimes de Tokio (main, para HTTP, escrituras y E/S, y datafusion, para las consultas): \
             100 % es todos ocupados todo el tiempo. Por defecto cada runtime tiene un worker por núcleo, así que con las consultas usando toda la CPU marca cerca del 50 %; \
             no es el uso de CPU del proceso ni del host.",
        )
        .source(
            "GET /metrics: Σ tokio_worker_total_busy_duration_seconds_total × 100 / Σ tokio_runtime_num_workers (runtimes main and datafusion)",
        ),
    );
    m.push(
        Metric::new(
            "mem_used",
            "Memoria residente",
            "Memoria",
            MetricUnit::Bytes,
            p.with("jemalloc_memstats_bytes", "stat", "resident"),
        )
        .help(
            "Memoria física que ocupan las páginas de jemalloc, el asignador que maneja casi toda la memoria del proceso. \
             Si se acerca a la RAM de la máquina o al límite del contenedor, el sistema puede matar el proceso.",
        )
        .source("GET /metrics: jemalloc_memstats_bytes{stat=\"resident\"}"),
    );
    m.push(
        Metric::new(
            "mem_alloc",
            "Memoria asignada",
            "Memoria",
            MetricUnit::Bytes,
            p.with("jemalloc_memstats_bytes", "stat", "allocated")
                .or(p.with("jemalloc_memstats_bytes", "stat", "alloc")),
        )
        .help(
            "Bytes que el servidor le pidió a jemalloc y todavía usa. \
             La diferencia con la memoria residente son los metadatos del asignador, la fragmentación y páginas libres que todavía no devolvió al sistema.",
        )
        .source("GET /metrics: jemalloc_memstats_bytes{stat=~\"allocated|alloc\"}"),
    );
    m.push(
        Metric::new(
            "query_mem",
            "Memoria de consultas",
            "Memoria",
            MetricUnit::Bytes,
            p.with("datafusion_mem_pool_bytes", "state", "reserved"),
        )
        .max(p.with("datafusion_mem_pool_bytes", "state", "limit"))
        .help(
            "Memoria que reservaron las consultas en curso en el pool de DataFusion; el máximo es el límite del pool (--exec-mem-pool-bytes). \
             Al llegar al límite, las consultas grandes fallan por falta de memoria.",
        )
        .source(
            "GET /metrics: datafusion_mem_pool_bytes{state=\"reserved\"} (max: {state=\"limit\"})",
        ),
    );
    m.push(
        Metric::new(
            "mem_cache",
            "Caché de Parquet",
            "Memoria",
            MetricUnit::Bytes,
            p.sum("influxdb3_parquet_cache_size_bytes"),
        )
        .help(
            "Archivos Parquet guardados en memoria para no leerlos del almacén de objetos en cada consulta. \
             Que esté llena es normal; si queda chica, sube la lectura del almacén.",
        )
        .source("GET /metrics: influxdb3_parquet_cache_size_bytes"),
    );
    let in_flight = sum(IN_FLIGHT
        .iter()
        .map(|ph| p.with("influxdb_iox_query_log_phase_current", "phase", ph)));
    m.push(
        Metric::new(
            "active_sessions",
            "Consultas en curso",
            "Conexiones",
            MetricUnit::Count,
            in_flight,
        )
        .help(
            "Consultas que todavía no terminaron: recibidas, en planificación o ejecutándose. \
             Si se mantienen altas, hay consultas lentas o falta capacidad.",
        )
        .source(
            "GET /metrics: Σ influxdb_iox_query_log_phase_current{phase=~\"received|logically_planned|planned|permit\"}",
        ),
    );
    m.push(
        Metric::new(
            "queries",
            "Consultas",
            "Actividad",
            MetricUnit::Count,
            p.with(
                "influxdb_iox_query_log_phase_entered_total",
                "phase",
                "received",
            ),
        )
        .counter()
        .help(
            "Consultas SQL e InfluxQL recibidas, incluidas las que DBine hace a las tablas system para este monitor. \
             Un salto sin cambios en la aplicación suele ser un panel o un script que consulta más seguido.",
        )
        .source(
            "GET /metrics: influxdb_iox_query_log_phase_entered_total{phase=\"received\"}",
        ),
    );
    m.push(
        Metric::new(
            "requests",
            "Peticiones HTTP",
            "Actividad",
            MetricUnit::Count,
            p.sum("http_requests_total"),
        )
        .counter()
        .help(HELP_HTTP_REQUESTS)
        .source("GET /metrics: Σ http_requests_total"),
    );
    m.push(
        Metric::new(
            "errors",
            "Errores del servidor",
            "Actividad",
            MetricUnit::Count,
            p.sum_where("http_requests_total", |l| {
                l.get("status").is_some_and(|x| x == "server_error")
            }),
        )
        .counter()
        .help(HELP_SERVER_ERRORS)
        .source("GET /metrics: Σ http_requests_total{status=\"server_error\"}"),
    );
    m.push(
        Metric::new(
            "rows_written",
            "Líneas escritas",
            "Actividad",
            MetricUnit::Count,
            p.sum("influxdb3_write_lines_total"),
        )
        .counter()
        .help(
            "Líneas de line protocol aceptadas en escrituras; cada línea es un punto con uno o más campos. \
             Mide la ingesta.",
        )
        .source("GET /metrics: Σ influxdb3_write_lines_total"),
    );
    m.push(
        Metric::new(
            "rows_rejected",
            "Líneas rechazadas",
            "Actividad",
            MetricUnit::Count,
            p.sum("influxdb3_write_lines_rejected_total"),
        )
        .counter()
        .help(
            "Líneas de line protocol que el servidor rechazó por mal formadas o por chocar con el esquema, como un campo con otro tipo. \
             Si sigue subiendo, hay un cliente escribiendo datos inválidos.",
        )
        .source("GET /metrics: Σ influxdb3_write_lines_rejected_total"),
    );
    m.push(
        Metric::new(
            "net_in",
            "Datos escritos",
            "Red",
            MetricUnit::Bytes,
            p.sum("influxdb3_write_bytes_total"),
        )
        .counter()
        .help(HELP_WRITE_BYTES)
        .source("GET /metrics: Σ influxdb3_write_bytes_total"),
    );
    let store = |prefix: &'static str| {
        p.sum_where("object_store_transfer_bytes_total", move |l| {
            l.get("op").is_some_and(|o| o.starts_with(prefix))
                && l.get("result").is_none_or(|r| r == "success")
        })
    };
    m.push(
        Metric::new(
            "disk_read",
            "Lectura del almacén de objetos",
            "Disco",
            MetricUnit::Bytes,
            store("get"),
        )
        .counter()
        .help(
            "Bytes leídos con éxito del almacén de objetos (disco local, S3 u otro): sobre todo archivos Parquet que las consultas no encontraron en caché. \
             Si sube mucho, la caché de Parquet queda chica para las consultas.",
        )
        .source(
            "GET /metrics: Σ object_store_transfer_bytes_total{op=~\"get.*\"} with result=\"success\" or no result label",
        ),
    );
    m.push(
        Metric::new(
            "disk_write",
            "Escritura en el almacén de objetos",
            "Disco",
            MetricUnit::Bytes,
            store("put"),
        )
        .counter()
        .help(
            "Bytes escritos con éxito en el almacén de objetos: el WAL, los snapshots y los archivos Parquet que persiste el servidor. \
             Sigue a la ingesta con algo de demora.",
        )
        .source(
            "GET /metrics: Σ object_store_transfer_bytes_total{op=~\"put.*\"} with result=\"success\" or no result label",
        ),
    );
    m.push(
        Metric::new(
            "tasks",
            "Tareas del runtime",
            "Servidor",
            MetricUnit::Count,
            p.sum("tokio_runtime_num_alive_tasks"),
        )
        .help(
            "Tareas asíncronas vivas en los runtimes de Tokio: consultas, escrituras, conexiones y trabajos internos. \
             Si crecen sin parar con carga estable, hay trabajo que queda colgado.",
        )
        .source("GET /metrics: Σ tokio_runtime_num_alive_tasks"),
    );
    let uptime = p
        .sum("process_start_time_seconds")
        .map(|st| (now_secs - st).max(0.0));
    m.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            uptime,
        )
        .help(
            "Tiempo desde que arrancó el proceso influxdb3. Si vuelve a cero, el servidor se reinició.",
        )
        .source("GET /metrics: now − process_start_time_seconds"),
    );

    let g = |k: &str| {
        ping.and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    for (label, v) in [
        ("Producto", g("product_name")),
        ("Versión", g("version")),
        ("Revisión", g("revision")),
        (
            "Workers",
            p.sum("tokio_runtime_num_workers").map(|v| v.to_string()),
        ),
        (
            "Límite de memoria de consultas (bytes)",
            p.with("datafusion_mem_pool_bytes", "state", "limit")
                .map(|v| v.to_string()),
        ),
    ] {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            s.info.push((label.to_string(), v));
        }
    }
    if prom.is_none() {
        s.notes.push("No se pudo leer /metrics: el token necesita permiso de administrador (o las métricas están desactivadas).".into());
    }
    s.notes.push(
        "InfluxDB 3 no informa el uso de CPU; «CPU de los workers» es qué parte del tiempo están ocupados los workers de sus dos runtimes (100 % = todos; por defecto hay dos por núcleo, así que 50 % ya puede ser toda la CPU)."
            .into(),
    );
    s
}

/// The count of databases (without the internal `_` ones) the 3.x session
/// adds from the database configuration API.
pub fn v3_databases_metric(count: usize) -> Metric {
    Metric::new(
        "databases",
        "Bases de datos",
        "Almacenamiento",
        MetricUnit::Count,
        Some(count as f64),
    )
    .help("Bases de datos creadas en el servidor, sin las internas (las que empiezan con _).")
    .source("GET /api/v3/configure/database?format=json: count(iox::database !~ \"^_\")")
}

/// The persisted Parquet size of the monitored database the 3.x session
/// adds from `system.parquet_files` (every table, not only the listed ones).
pub fn v3_parquet_metric(db: &str, bytes: f64) -> Metric {
    Metric::new(
        "storage_used",
        &format!("Parquet persistido de «{db}»"),
        "Almacenamiento",
        MetricUnit::Bytes,
        Some(bytes),
    )
    .help(
        "Tamaño de los archivos Parquet ya persistidos de la base que muestra el monitor: la de la conexión o, si no tiene, la primera. \
         Los datos que todavía están en el WAL o en memoria no suman.",
    )
    .source("SELECT sum(size_bytes) FROM system.parquet_files")
}

/// Rows of `system.queries` as the "queries" table (running first).
pub fn v3_queries_table(columns: &[String], rows: &[Vec<J>], running_only: bool) -> MonitorTable {
    let col = |n: &str| columns.iter().position(|c| c == n);
    let get = |r: &Vec<J>, n: &str| col(n).and_then(|i| r.get(i)).cloned().unwrap_or(J::Null);
    let (key, title) = if running_only {
        ("queries", "Consultas en curso")
    } else {
        ("recent_queries", "Consultas recientes")
    };
    let mut t = MonitorTable::new(
        key,
        title,
        &[
            "id",
            "tipo",
            "fase",
            "inicio",
            "duración total",
            "memoria máx. (bytes)",
            "consulta",
        ],
    );
    for r in rows.iter().take(MAX_ROWS) {
        let text = get(r, "query_text");
        t.rows.push(vec![
            get(r, "id"),
            get(r, "query_type"),
            get(r, "phase"),
            get(r, "issue_time"),
            get(r, "end2end_duration"),
            get(r, "max_memory"),
            json!(truncate(text.as_str().unwrap_or_default())),
        ]);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_lines() {
        let p = Prom::parse(
            "# HELP x\n\
             go_goroutines 12\n\
             http_api_requests_total{path=\"/api/v2/query\",status=\"2XX\"} 3\n\
             http_api_requests_total{path=\"/api/v2/write\",status=\"5XX\"} 1e1\n\
             weird{a=\"x,}y\",b=\"q\\\"\",} 2.5 1700000000\n\
             nan_one NaN\n",
        );
        assert_eq!(p.sum("go_goroutines"), Some(12.0));
        assert_eq!(p.sum("http_api_requests_total"), Some(13.0));
        assert_eq!(
            p.with("http_api_requests_total", "status", "5XX"),
            Some(10.0)
        );
        assert_eq!(p.labels("weird").unwrap()["a"], "x,}y");
        assert_eq!(p.labels("weird").unwrap()["b"], "q\"");
        assert_eq!(p.sum("weird"), Some(2.5));
        assert_eq!(p.sum("nan_one"), None);
        assert_eq!(p.sum("missing"), None);
    }

    #[test]
    fn go_durations() {
        assert_eq!(go_duration("12.5s"), Some(12.5));
        assert_eq!(go_duration("1h2m3s"), Some(3723.0));
        assert_eq!(go_duration("500ms"), Some(0.5));
        assert!((go_duration("29µs").unwrap() - 29e-6).abs() < 1e-12);
        assert_eq!(go_duration("0s"), Some(0.0));
        assert_eq!(go_duration("abc"), None);
    }

    fn metric<'a>(s: &'a MonitorSnapshot, key: &str) -> &'a Metric {
        s.metrics
            .iter()
            .find(|m| m.key == key)
            .unwrap_or_else(|| panic!("{key}"))
    }

    #[test]
    fn v1_from_show_answers() {
        let stats = json!({"series":[
            {"name":"runtime","columns":["HeapAlloc","NumGoroutine","Sys","NumGC"],"values":[[100,7,900,3]]},
            {"name":"queryExecutor","columns":["queriesActive","queriesExecuted"],"values":[[1,42]]},
            {"name":"database","tags":{"database":"db1"},"columns":["numMeasurements","numSeries"],"values":[[2,10]]},
            {"name":"shard","tags":{"database":"db1","id":"1"},"columns":["diskBytes"],"values":[[1000]]},
            {"name":"shard","tags":{"database":"db1","id":"2"},"columns":["diskBytes"],"values":[[500]]},
            {"name":"tsm1_cache","tags":{"database":"db1"},"columns":["memBytes"],"values":[[64]]},
            {"name":"tsm1_engine","tags":{"database":"db1"},"columns":["cacheCompactionsActive","tsmLevel1CompactionsActive"],"values":[[1,1]]},
            {"name":"httpd","columns":["req","reqActive","writeReqBytes","queryRespBytes","serverError"],"values":[[9,1,300,400,0]]},
            {"name":"write","columns":["pointReq"],"values":[[77]]}
        ]});
        let diag = json!({"series":[
            {"name":"build","columns":["Branch","Commit","Version"],"values":[["1.8","abc","1.8.10"]]},
            {"name":"system","columns":["uptime","started"],"values":[["1m2.5s","2024-01-31T13:45:00Z"]]},
            {"name":"config-httpd","columns":["max-connection-limit"],"values":[[0]]}
        ]});
        let queries = json!({"series":[{"columns":["qid","query","database","duration","status"],
            "values":[[3,"SHOW QUERIES","","29µs","running"],[2,"SELECT * FROM cpu","db1","3s","running"]]}]});
        let s = v1_snapshot(Some(&stats), Some(&diag), Some(&queries));
        assert_eq!(metric(&s, "mem_used").value, Some(100.0));
        assert_eq!(metric(&s, "storage_used").value, Some(1500.0));
        assert_eq!(metric(&s, "series").value, Some(10.0));
        assert_eq!(metric(&s, "queries").value, Some(42.0));
        assert!(metric(&s, "queries").counter);
        assert_eq!(metric(&s, "compactions").value, Some(2.0));
        assert_eq!(metric(&s, "connections").max, None);
        assert_eq!(metric(&s, "uptime").value, Some(62.5));
        let q = s.tables.iter().find(|t| t.key == "queries").unwrap();
        assert_eq!(q.rows.len(), 1);
        assert_eq!(q.rows[0][4], json!("SELECT * FROM cpu"));
        let d = s.tables.iter().find(|t| t.key == "databases").unwrap();
        assert_eq!(
            d.rows[0],
            vec![
                json!("db1"),
                json!(2.0),
                json!(10.0),
                json!(2),
                json!(1500.0)
            ]
        );
        assert!(s.info.iter().any(|(k, v)| k == "Versión" && v == "1.8.10"));
        assert!(s
            .info
            .iter()
            .any(|(k, v)| k == "Conexiones HTTP máximas" && v == "sin límite"));
    }

    #[test]
    fn v1_without_admin() {
        let s = v1_snapshot(None, None, None);
        assert!(s.metrics.iter().all(|m| m.value.is_none()));
        assert!(s.notes.len() >= 4);
        assert!(s.tables.is_empty());
    }

    #[test]
    fn v2_from_metrics() {
        let p = Prom::parse(
            "go_memstats_alloc_bytes 2.5e7\n\
             influxdb_uptime_seconds{id=\"x\"} 33.5\n\
             influxdb_info{arch=\"arm64\",os=\"linux\",version=\"v2.9.1\",cpus=\"8\"} 1\n\
             storage_shard_disk_size{bucket=\"b1\",id=\"1\"} 100\n\
             storage_shard_disk_size{bucket=\"b1\",id=\"2\"} 50\n\
             storage_wal_size{bucket=\"b1\",id=\"1\"} 10\n\
             storage_shard_series{bucket=\"b1\",id=\"1\"} 4\n\
             http_api_requests_total{path=\"/api/v2/query\",method=\"POST\",status=\"2XX\"} 5\n\
             http_api_requests_total{path=\"/api/v2/write\",method=\"POST\",status=\"5XX\"} 1\n\
             process_cpu_seconds_total 1.5\n",
        );
        let buckets = [("b1".to_string(), "test".to_string())].into();
        let s = v2_snapshot(
            Some(&p),
            Some(&json!({"version":"v2.9.1","status":"pass","message":"ok"})),
            &buckets,
        );
        // 1.5 CPU seconds over the 8 cores the server sees: a share of all
        // of them, never 150 %.
        assert_eq!(metric(&s, "cpu_time").value, Some(18.75));
        assert_eq!(metric(&s, "cpu_time").max, Some(100.0));
        assert!(metric(&s, "cpu_time").counter);
        assert_eq!(metric(&s, "storage_used").value, Some(160.0));
        assert_eq!(metric(&s, "queries").value, Some(5.0));
        assert_eq!(metric(&s, "errors").value, Some(1.0));
        assert_eq!(metric(&s, "uptime").value, Some(33.5));
        let t = s.tables.iter().find(|t| t.key == "databases").unwrap();
        assert_eq!(t.rows[0][0], json!("test"));
        assert_eq!(t.rows[0][2], json!(150.0));
        assert!(s
            .info
            .iter()
            .any(|(k, v)| k == "Sistema" && v == "linux/arm64"));
    }

    #[test]
    fn v2_cpu_without_core_count_is_empty_and_explained() {
        // CPU seconds without influxdb_info{cpus}: no unscaled percentage,
        // and a note says why the gauge is empty.
        let p = Prom::parse(
            "influxdb_info{version=\"v2.9.1\"} 1\n\
             process_cpu_seconds_total 1.5\n",
        );
        let s = v2_snapshot(Some(&p), None, &BTreeMap::new());
        assert_eq!(metric(&s, "cpu_time").value, None);
        assert!(s.notes.iter().any(|n| n.contains("influxdb_info{cpus}")));
    }

    #[test]
    fn v3_from_metrics() {
        let p = Prom::parse(
            "jemalloc_memstats_bytes{stat=\"resident\"} 1000\n\
             datafusion_mem_pool_bytes{state=\"limit\"} 5000\n\
             datafusion_mem_pool_bytes{state=\"reserved\"} 10\n\
             influxdb_iox_query_log_phase_current{phase=\"received\"} 1\n\
             influxdb_iox_query_log_phase_current{phase=\"success\"} 9\n\
             influxdb_iox_query_log_phase_entered_total{phase=\"received\"} 12\n\
             object_store_transfer_bytes_total{op=\"put\",result=\"success\"} 31\n\
             object_store_transfer_bytes_total{op=\"put\",result=\"error\"} 5\n\
             tokio_runtime_num_workers{runtime=\"main\"} 2\n\
             tokio_runtime_num_workers{runtime=\"datafusion\"} 2\n\
             tokio_worker_total_busy_duration_seconds_total{runtime=\"main\",worker=\"0\"} 1.5\n\
             tokio_worker_total_busy_duration_seconds_total{runtime=\"datafusion\",worker=\"1\"} 0.5\n\
             process_start_time_seconds 1000\n",
        );
        let s = v3_snapshot(
            Some(&p),
            Some(&json!({"product_name":"InfluxDB 3 Core","version":"3.1"})),
            1060.0,
        );
        assert_eq!(metric(&s, "mem_used").value, Some(1000.0));
        // 2 busy seconds over 4 workers: half of them, never 200 %.
        assert_eq!(metric(&s, "cpu_time").value, Some(50.0));
        assert_eq!(metric(&s, "cpu_time").max, Some(100.0));
        assert_eq!(metric(&s, "query_mem").max, Some(5000.0));
        assert_eq!(metric(&s, "active_sessions").value, Some(1.0));
        assert_eq!(metric(&s, "queries").value, Some(12.0));
        assert_eq!(metric(&s, "disk_write").value, Some(31.0));
        assert_eq!(metric(&s, "uptime").value, Some(60.0));
        let cols: Vec<String> = ["id", "query_text", "phase"]
            .iter()
            .map(|c| c.to_string())
            .collect();
        let t = v3_queries_table(
            &cols,
            &[vec![json!("q1"), json!("SELECT 1"), json!("received")]],
            true,
        );
        assert_eq!(t.rows[0][0], json!("q1"));
        assert_eq!(t.rows[0][6], json!("SELECT 1"));
        assert_eq!(truncate(&"x".repeat(3000)).chars().count(), 2001);
    }

    /// Every metric of the three APIs says what it measures and where its
    /// value comes from, answered or not.
    #[test]
    fn every_metric_has_help_and_source() {
        let empty = Prom(Vec::new());
        let snaps = [
            v1_snapshot(None, None, None),
            v2_snapshot(Some(&empty), None, &BTreeMap::new()),
            v2_snapshot(None, None, &BTreeMap::new()),
            v3_snapshot(Some(&empty), None, 0.0),
            v3_snapshot(None, None, 0.0),
        ];
        let mut all: Vec<Metric> = snaps.into_iter().flat_map(|s| s.metrics).collect();
        all.push(v3_databases_metric(2));
        all.push(v3_parquet_metric("db", 1.0));
        assert!(all.len() >= 70, "{}", all.len());
        for m in &all {
            assert!(
                m.help.as_deref().is_some_and(|h| !h.trim().is_empty()),
                "{} ({}) has no help",
                m.key,
                m.label
            );
            assert!(
                m.source.as_deref().is_some_and(|s| !s.trim().is_empty()),
                "{} ({}) has no source",
                m.key,
                m.label
            );
        }
        // A CPU-time counter is a share of the whole server.
        for m in all.iter().filter(|m| m.unit == MetricUnit::Percent) {
            assert_eq!(m.max, Some(100.0), "{}", m.key);
        }
    }
}

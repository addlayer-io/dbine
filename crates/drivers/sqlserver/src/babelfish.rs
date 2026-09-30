//! Babelfish for PostgreSQL: SQL Server's protocol on top of PostgreSQL.
//! Its T-SQL endpoint lacks most of SQL Server's DMVs (no ring buffers,
//! performance counters, wait stats or showplan XML) but reaches
//! PostgreSQL's own catalog (`pg_catalog.pg_stat_activity`…), so the
//! monitor reads that; plans come as PostgreSQL's text EXPLAIN through
//! `SET BABELFISH_SHOWPLAN_ALL` / `BABELFISH_STATISTICS PROFILE`.

use crate::monitor::{f, table, Gaps};
use crate::{text, SqlServerSession};
use dbine_driver::monitor::{Metric, MetricUnit as U, MonitorSnapshot};
use dbine_driver::{Plan, PlanNode, Result};

/// Column of the result set carrying a text plan.
pub const PLAN_COLUMN: &str = "QUERY PLAN";

// ------------------------------------------------------------------ plans

/// The statements' text plans (one result set may hold several, each
/// starting with `Query Text:`) as plan trees.
pub fn parse_text_plans(text: &str, actual: bool) -> Vec<Plan> {
    let mut plans = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    let mut trailer: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(q) = line.strip_prefix("Query Text:") {
            if let Some((stmt, lines)) = cur.take() {
                plans.push(build(&stmt, &lines, actual));
            }
            cur = Some((q.trim().to_string(), Vec::new()));
        } else if line.starts_with("Babelfish T-SQL") {
            if let Some((k, v)) = line.split_once(':') {
                trailer.push((k.trim().to_string(), v.trim().to_string()));
            }
        } else {
            match &mut cur {
                Some((_, lines)) => lines.push(line),
                None => cur = Some((String::new(), vec![line])),
            }
        }
    }
    if let Some((stmt, lines)) = cur.take() {
        plans.push(build(&stmt, &lines, actual));
    }
    if let Some(last) = plans.last_mut() {
        last.root.props.extend(trailer);
    }
    plans
}

fn build(statement: &str, lines: &[&str], actual: bool) -> Plan {
    // (indent of the node, node); the stack holds the open ancestors.
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut root_props: Vec<(String, String)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    for line in lines {
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        let (is_node, body, level) = match t.strip_prefix("->") {
            Some(rest) => (true, rest.trim(), indent + 2),
            None if stack.is_empty() && !t.contains(':') => (true, t, 0),
            None => (false, t, indent),
        };
        if is_node {
            while stack.last().is_some_and(|(l, _)| *l >= level) {
                let (_, done) = stack.pop().expect("non-empty");
                attach(&mut stack, &mut roots, done);
            }
            stack.push((level, node(body)));
        } else if let Some((k, v)) = body.split_once(':') {
            let prop = (k.trim().to_string(), v.trim().to_string());
            match stack.last_mut() {
                // "Planning Time" / "Execution Time" close the plan.
                Some(_) if level == 0 => root_props.push(prop),
                Some((_, n)) => n.props.push(prop),
                None => root_props.push(prop),
            }
        }
    }
    while let Some((_, done)) = stack.pop() {
        attach(&mut stack, &mut roots, done);
    }
    let mut root = if roots.len() == 1 { roots.remove(0) } else { PlanNode { op: "QUERY PLAN".into(), children: roots, ..Default::default() } };
    for (k, v) in &root_props {
        if k == "Execution Time" {
            root.actual_ms = v.trim_end_matches("ms").trim().parse().ok().or(root.actual_ms);
        }
    }
    root.props.extend(root_props);
    Plan { statement: statement.to_string(), root, actual, raw_format: "text".into(), raw: lines.join("\n") }
}

fn attach(stack: &mut [(usize, PlanNode)], roots: &mut Vec<PlanNode>, n: PlanNode) {
    match stack.last_mut() {
        Some((_, parent)) => parent.children.push(n),
        None => roots.push(n),
    }
}

/// `Index Scan using ix on t  (cost=0.28..8.29 rows=1 width=4) (actual time=0.01..0.02 rows=1 loops=3)`.
fn node(line: &str) -> PlanNode {
    let (head, figures) = match line.find("  (") {
        Some(i) => (&line[..i], &line[i..]),
        None => (line, ""),
    };
    let mut n = PlanNode::default();
    let (op, object) = match head.split_once(" on ") {
        Some((op, obj)) => (op.trim(), Some(obj.trim().to_string())),
        None => (head.trim(), None),
    };
    match op.split_once(" using ") {
        Some((o, ix)) => {
            n.op = o.trim().to_string();
            n.detail = format!("using {}", ix.trim());
        }
        None => n.op = op.to_string(),
    }
    n.object = object;
    let field = |s: &str, key: &str| -> Option<String> {
        let i = s.find(key)? + key.len();
        Some(s[i..].split([' ', ')']).next()?.to_string())
    };
    if let Some(i) = figures.find("(cost=") {
        let est = &figures[i..];
        n.total_cost = field(est, "cost=").and_then(|c| c.split("..").nth(1).and_then(|v| v.parse().ok()));
        n.est_rows = field(est, "rows=").and_then(|v| v.parse().ok());
    }
    if let Some(i) = figures.find("(actual") {
        let act = &figures[i..];
        let loops: f64 = field(act, "loops=").and_then(|v| v.parse().ok()).unwrap_or(1.0);
        let rows: Option<f64> = field(act, "rows=").and_then(|v| v.parse().ok());
        let ms: Option<f64> = field(act, "time=").and_then(|t| t.split("..").nth(1).and_then(|v| v.parse().ok()));
        n.actual_rows = rows.map(|r| r * loops);
        n.executions = Some(loops);
        n.actual_ms = ms.map(|m| m * loops);
    } else if figures.contains("never executed") {
        n.executions = Some(0.0);
        n.actual_rows = Some(0.0);
    }
    if n.op.starts_with("Seq Scan") {
        n.warnings.push("Recorre la tabla completa".into());
    }
    n
}

// ---------------------------------------------------------------- monitor

const VERSION: &str = "SELECT CAST(@@VERSION AS varchar(4000)),
       CAST(DATEDIFF(SECOND, pg_postmaster_start_time(), GETDATE()) AS float),
       (SELECT CAST(setting AS float) FROM pg_catalog.pg_settings WHERE name = 'max_connections'),
       (SELECT CAST(setting AS float) FROM pg_catalog.pg_settings WHERE name = 'shared_buffers'),
       (SELECT CAST(setting AS varchar(100)) FROM pg_catalog.pg_settings WHERE name = 'TimeZone'),
       CAST(pg_is_in_recovery() AS int), DB_NAME()";

const ACTIVITY: &str = "SELECT CAST(COUNT(*) AS float),
       CAST(SUM(CASE WHEN state <> 'idle' AND pid <> @@SPID THEN 1 ELSE 0 END) AS float)
  FROM pg_catalog.pg_stat_activity WHERE backend_type = 'client backend'";

const DATABASE_STATS: &str = "SELECT CAST(SUM(xact_commit + xact_rollback) AS float), CAST(SUM(blks_hit) AS float),
       CAST(SUM(blks_read) AS float), CAST(SUM(tup_returned + tup_fetched) AS float),
       CAST(SUM(tup_inserted + tup_updated + tup_deleted) AS float), CAST(SUM(deadlocks) AS float),
       CAST(SUM(temp_bytes) AS float)
  FROM pg_catalog.pg_stat_database";

/// PostgreSQL 16+.
const IO: &str = "SELECT CAST(SUM(reads * op_bytes) AS float), CAST(SUM(writes * op_bytes) AS float)
  FROM pg_catalog.pg_stat_io";

const LOCKS: &str = "SELECT CAST(COUNT(*) AS float), CAST(SUM(CASE WHEN granted = 0 THEN 1 ELSE 0 END) AS float)
  FROM pg_catalog.pg_locks";

const STORAGE: &str = "SELECT CAST(SUM(pg_database_size(datname)) AS float) FROM pg_catalog.pg_database";

const SESSIONS: &str = "SELECT TOP (200) pid, usename, datname, CAST(client_addr AS varchar(64)), application_name, state,
       CAST(DATEDIFF(SECOND, COALESCE(query_start, backend_start), GETDATE()) AS float),
       CAST(wait_event_type AS varchar(64)) + ': ' + CAST(wait_event AS varchar(64)),
       LEFT(query, 2000)
  FROM pg_catalog.pg_stat_activity
 WHERE backend_type = 'client backend' AND pid <> @@SPID
 ORDER BY CASE WHEN state = 'active' THEN 0 ELSE 1 END, pid";

const SESSION_COLS: &[&str] =
    &["PID", "Usuario", "Base", "Dirección", "Programa", "Estado", "Duración (s)", "Espera", "Consulta"];

const QUERIES: &str = "SELECT TOP (200) pid, usename, datname, state,
       CAST(DATEDIFF(SECOND, query_start, GETDATE()) AS float),
       CAST(wait_event_type AS varchar(64)) + ': ' + CAST(wait_event AS varchar(64)),
       CAST(pg_blocking_pids(pid) AS varchar(200)), LEFT(query, 2000)
  FROM pg_catalog.pg_stat_activity
 WHERE backend_type = 'client backend' AND state <> 'idle' AND pid <> @@SPID
 ORDER BY query_start";

const QUERY_COLS: &[&str] = &["PID", "Usuario", "Base", "Estado", "Duración (s)", "Espera", "Bloqueada por", "Consulta"];

const WAITING_LOCKS: &str = "SELECT TOP (200) l.pid, CAST(pg_blocking_pids(l.pid) AS varchar(200)), l.locktype, l.mode,
       c.relname, CAST(DATEDIFF(SECOND, a.query_start, GETDATE()) AS float), LEFT(a.query, 2000)
  FROM pg_catalog.pg_locks l
  LEFT JOIN pg_catalog.pg_class c ON c.oid = l.relation
  LEFT JOIN pg_catalog.pg_stat_activity a ON a.pid = l.pid
 WHERE l.granted = 0";

const LOCK_COLS: &[&str] = &["PID", "Bloqueada por", "Tipo", "Modo", "Relación", "Esperando (s)", "Consulta"];

const WAITS: &str = "SELECT TOP (20) CAST(wait_event_type AS varchar(64)), CAST(wait_event AS varchar(64)), CAST(COUNT(*) AS float)
  FROM pg_catalog.pg_stat_activity
 WHERE wait_event IS NOT NULL AND backend_type = 'client backend'
 GROUP BY wait_event_type, wait_event
 ORDER BY COUNT(*) DESC";

const DATABASES: &str = "SELECT d.datname, CAST(ROUND(pg_database_size(d.datname) / 1048576.0, 1) AS float),
       CAST(s.numbackends AS float), CAST(s.xact_commit AS float), CAST(s.xact_rollback AS float)
  FROM pg_catalog.pg_database d
  LEFT JOIN pg_catalog.pg_stat_database s ON s.datid = d.oid
 WHERE d.datallowconn = 1
 ORDER BY d.datname";

const TSQL_DATABASES: &str = "SELECT name, CAST(database_id AS int), CONVERT(varchar(19), create_date, 120) FROM sys.databases ORDER BY database_id";

const TOP_OBJECTS: &str = "SELECT TOP (20) n.nspname + '.' + c.relname, CAST(c.relkind AS varchar(1)),
       CAST(CASE WHEN c.reltuples < 0 THEN NULL ELSE c.reltuples END AS float),
       CAST(ROUND(pg_total_relation_size(c.oid) / 1048576.0, 2) AS float)
  FROM pg_catalog.pg_class c
  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind IN ('r', 'm') AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'sys', 'pg_toast')
 ORDER BY c.relpages DESC";

const REPLICATION: &str = "SELECT application_name, CAST(client_addr AS varchar(64)), state, sync_state,
       CAST(replay_lag AS varchar(64))
  FROM pg_catalog.pg_stat_replication";

pub async fn monitor(s: &mut SqlServerSession) -> Result<MonitorSnapshot> {
    let mut snap = MonitorSnapshot::default();
    let mut gaps = Gaps::default();
    let first = |r: Option<Vec<tiberius::Row>>| r.and_then(|r| r.into_iter().next());

    let v = first(s.probe("la versión", VERSION, &mut gaps).await);
    let max_conn = v.as_ref().and_then(|r| f(r, 2));
    if let Some(r) = &v {
        let ver = text(r, 0).unwrap_or_default();
        let lines: Vec<&str> = ver.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if let Some(l) = lines.first() {
            snap.info.push(("Versión".into(), l.to_string()));
        }
        if let Some(pg) = lines.iter().find(|l| l.starts_with("PostgreSQL")) {
            snap.info.push(("PostgreSQL".into(), pg.to_string()));
        }
        if let Some(m) = max_conn {
            snap.info.push(("Conexiones máximas (max_connections)".into(), format!("{m}")));
        }
        if let Some(tz) = text(r, 4) {
            snap.info.push(("Zona horaria".into(), tz));
        }
        snap.info.push(("Rol".into(), if f(r, 5) == Some(1.0) { "Réplica (en recuperación)" } else { "Primario" }.into()));
        if let Some(db) = text(r, 6) {
            snap.info.push(("Base actual".into(), db));
        }
    }
    let act = first(s.probe("las sesiones", ACTIVITY, &mut gaps).await);
    let st = first(s.probe("las estadísticas de las bases", DATABASE_STATS, &mut gaps).await);
    let io = first(s.probe("la E/S (pg_stat_io)", IO, &mut gaps).await);
    let locks = first(s.probe("los bloqueos", LOCKS, &mut gaps).await);
    let storage = first(s.probe("el tamaño de las bases", STORAGE, &mut gaps).await).and_then(|r| f(&r, 0));

    let m = &mut snap.metrics;
    let at = |r: &Option<tiberius::Row>, i| r.as_ref().and_then(|r| f(r, i));
    // shared_buffers is in 8 kB pages.
    m.push(Metric::new("mem_cache", "Buffers compartidos (shared_buffers)", "Memoria", U::Bytes, at(&v, 3).map(|p| p * 8192.0)));
    m.push(Metric::new("connections", "Conexiones", "Conexiones", U::Count, at(&act, 0)).max(max_conn));
    m.push(Metric::new("active_sessions", "Sesiones activas", "Conexiones", U::Count, at(&act, 1)));
    m.push(Metric::new("transactions", "Transacciones", "Actividad", U::Count, at(&st, 0)).counter());
    m.push(Metric::new("rows_read", "Filas leídas", "Actividad", U::Count, at(&st, 3)).counter());
    m.push(Metric::new("rows_written", "Filas escritas", "Actividad", U::Count, at(&st, 4)).counter());
    let hit = match (at(&st, 1), at(&st, 2)) {
        (Some(h), Some(r)) if h + r > 0.0 => Some(h / (h + r) * 100.0),
        _ => None,
    };
    m.push(Metric::new("cache_hit", "Aciertos de caché", "Caché", U::Percent, hit).max(Some(100.0)));
    let disk_read = at(&io, 0).or(at(&st, 2).map(|b| b * 8192.0));
    m.push(Metric::new("disk_read", "Lectura en disco", "Disco", U::Bytes, disk_read).counter());
    m.push(Metric::new("disk_write", "Escritura en disco", "Disco", U::Bytes, at(&io, 1)).counter());
    m.push(Metric::new("temp_bytes", "Archivos temporales", "Disco", U::Bytes, at(&st, 6)).counter());
    m.push(Metric::new("storage_used", "Espacio usado", "Almacenamiento", U::Bytes, storage));
    m.push(Metric::new("locks_waiting", "Bloqueos en espera", "Bloqueos", U::Count, at(&locks, 1)));
    m.push(Metric::new("locks_held", "Bloqueos retenidos", "Bloqueos", U::Count, at(&locks, 0)));
    m.push(Metric::new("deadlocks", "Deadlocks", "Bloqueos", U::Count, at(&st, 5)).counter());
    m.push(Metric::new("uptime", "Tiempo activo", "Servidor", U::Seconds, at(&v, 1)));

    let repl = s.probe("la replicación", REPLICATION, &mut gaps).await;
    let lag = repl.as_ref().and_then(|rows| rows.iter().filter_map(|r| text(r, 4).and_then(|t| interval_secs(&t))).reduce(f64::max));
    snap.metrics.push(Metric::new("replication_lag", "Retraso de réplica", "Replicación", U::Seconds, lag));

    let tables = [
        table("sessions", "Sesiones", SESSION_COLS, s.probe("las sesiones", SESSIONS, &mut gaps).await),
        table("queries", "Consultas en curso", QUERY_COLS, s.probe("las consultas en curso", QUERIES, &mut gaps).await),
        table("locks", "Bloqueos en espera", LOCK_COLS, s.probe("los bloqueos en espera", WAITING_LOCKS, &mut gaps).await),
        table("waits", "Esperas actuales", &["Tipo", "Evento", "Sesiones"], s.probe("las esperas", WAITS, &mut gaps).await),
        table("databases", "Bases de PostgreSQL y tamaños", &["Base", "Tamaño (MB)", "Conexiones", "Commits", "Rollbacks"], s.probe("las bases", DATABASES, &mut gaps).await),
        table("tsql_databases", "Bases T-SQL", &["Base", "ID", "Creada"], s.probe("las bases T-SQL", TSQL_DATABASES, &mut gaps).await),
        table("top_objects", "Objetos más grandes", &["Objeto (esquema de PostgreSQL)", "Tipo", "Filas (estimadas)", "Tamaño total (MB)"], s.probe("los objetos más grandes", TOP_OBJECTS, &mut gaps).await),
        table("replication", "Réplicas", &["Réplica", "Dirección", "Estado", "Sincronización", "Retraso"], repl),
    ];
    snap.tables.extend(tables.into_iter().flatten());
    snap.notes.push("Babelfish (PostgreSQL) no expone el uso de CPU ni la memoria del servidor por SQL: se ven en el sistema operativo o en CloudWatch (Aurora).".into());
    snap.notes.push("PostgreSQL no acumula tiempos de espera: la tabla de esperas muestra las sesiones que esperan ahora.".into());
    snap.notes.extend(gaps.into_notes("pg_read_all_stats (o pg_monitor)"));
    Ok(snap)
}

/// `00:00:01.5` or `1 day 02:00:00` (a PostgreSQL interval as text) in seconds.
fn interval_secs(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut rest = s.trim();
    if let Some((d, t)) = rest.split_once(" day") {
        total += d.trim().parse::<f64>().ok()? * 86_400.0;
        rest = t.trim_start_matches('s').trim();
    }
    if rest.is_empty() {
        return Some(total);
    }
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    Some(total + parts[0].parse::<f64>().ok()? * 3600.0 + parts[1].parse::<f64>().ok()? * 60.0 + parts[2].parse::<f64>().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTUAL: &str = "Query Text: SELECT name FROM sys.databases WHERE name like 'm%'
Subquery Scan on databases  (cost=4.80..6.00 rows=1 width=32) (actual time=0.196..0.212 rows=2 loops=1)
  ->  Nested Loop  (cost=4.80..5.99 rows=1 width=635) (actual time=0.196..0.210 rows=2 loops=1)
        Join Filter: (d.dbid = (db_id((t.name)::nvarchar)))
        Rows Removed by Join Filter: 4
        ->  Index Only Scan using pg_collation_name_enc_nsp_index on pg_collation c  (cost=0.28..48.27 rows=907 width=64) (actual time=0.027..0.048 rows=93 loops=1)
              Heap Fetches: 39
        ->  Seq Scan on babelfish_sysdatabases t  (cost=0.00..1.04 rows=3 width=132) (actual time=0.014..0.019 rows=3 loops=2)
Planning Time: 9.453 ms
Execution Time: 0.571 ms
Query Text: SELECT 2 AS b
Result  (cost=0.00..0.01 rows=1 width=4) (actual time=0.000..0.000 rows=1 loops=1)
Babelfish T-SQL Batch Parsing Time: 77.512 ms
";

    #[test]
    fn text_plans_become_trees() {
        let plans = parse_text_plans(ACTUAL, true);
        assert_eq!(plans.len(), 2);
        let p = &plans[0];
        assert_eq!(p.statement, "SELECT name FROM sys.databases WHERE name like 'm%'");
        assert_eq!(p.root.op, "Subquery Scan");
        assert_eq!(p.root.object.as_deref(), Some("databases"));
        assert_eq!(p.root.actual_ms, Some(0.571));
        let nl = &p.root.children[0];
        assert_eq!(nl.op, "Nested Loop");
        assert_eq!(nl.total_cost, Some(5.99));
        assert!(nl.props.iter().any(|(k, _)| k == "Join Filter"));
        assert_eq!(nl.children.len(), 2);
        let ix = &nl.children[0];
        assert_eq!((ix.op.as_str(), ix.detail.as_str()), ("Index Only Scan", "using pg_collation_name_enc_nsp_index"));
        assert_eq!(ix.props, vec![("Heap Fetches".to_string(), "39".to_string())]);
        let seq = &nl.children[1];
        assert_eq!(seq.actual_rows, Some(6.0));
        assert_eq!(seq.executions, Some(2.0));
        assert!(!seq.warnings.is_empty());
        assert_eq!(plans[1].root.op, "Result");
        assert!(plans[1].root.props.iter().any(|(k, _)| k.starts_with("Babelfish")));
    }

    #[test]
    fn estimated_plans_without_query_text() {
        let plans = parse_text_plans("Result  (cost=0.00..0.01 rows=1 width=4)\n", false);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].root.est_rows, Some(1.0));
        assert_eq!(plans[0].root.actual_rows, None);
    }

    #[test]
    fn intervals() {
        assert_eq!(interval_secs("00:00:01.5"), Some(1.5));
        assert_eq!(interval_secs("1 day 00:01:00"), Some(86_460.0));
        assert_eq!(interval_secs("2 days"), Some(172_800.0));
        assert_eq!(interval_secs("x"), None);
    }
}

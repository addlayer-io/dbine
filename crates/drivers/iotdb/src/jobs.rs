//! Server jobs ([`dbine_driver::Session::jobs`]) of IoTDB 1.x and 2.x
//! (tree model) and TimechoDB:
//!
//! - **Continuous queries** (`SHOW CQS`: `CQId`, `Query`, `State`), which
//!   run their `SELECT … INTO` every `RESAMPLE EVERY` (by default their
//!   `GROUP BY` interval). IoTDB has `CREATE`, `SHOW` and `DROP` for them and
//!   nothing else: no statement pauses one, runs it now or stops a run, and
//!   the server keeps no history of its runs (a failed run is only in the
//!   server's log). The state is `ACTIVE`, or `INACTIVE` while the query is
//!   being created. The script is `Query`, the statement it was made with.
//! - **Pipes** (`SHOW PIPES`), the data sync tasks that run on the server
//!   until stopped: `STOP PIPE` / `START PIPE` disable and enable one. No
//!   history, no run now. Their script is rebuilt from the source,
//!   processor and sink attributes `SHOW PIPES` gives (IoTDB 2 hides
//!   passwords as `******`, and this module hides any other credential the
//!   same way: those are left out, and the tab says so).
//!   Pipes whose name starts with `__` are IoTDB's own (system).
//!
//! Ids are `cq:<id>` and `pipe:<name>`. An id that comes back has to name
//! one of the listed ones, exactly; a pipe's name then goes into
//! `START` / `STOP PIPE` always backquoted ([`pipe_name`]).

use crate::{IotDbSession, Table};
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobStep, PropertyInfo, Result, ServerJob,
};
use serde_json::Value as J;

const KIND: &str = "Continuous query";
const PIPE_KIND: &str = "Pipe";
/// Jobs listed at most.
pub(crate) const MAX_JOBS: usize = 5000;

const CQ_NOTE: &str = "IoTDB no permite pausar una consulta continua, ejecutarla a pedido ni detener una ejecución, y no guarda un historial de ejecuciones: para frenarla hay que borrarla (DROP CQ) y volver a crearla con su script.";
const DENIED: &str =
    "Este usuario no puede ver las consultas continuas (hace falta el privilegio USE_CQ).";
const PIPES_DENIED: &str =
    "Este usuario no puede ver los pipes (hace falta el privilegio USE_PIPE).";
const PIPE_NOTE: &str = "Un pipe sincroniza datos sin parar: no tiene «Ejecutar ahora» ni historial de ejecuciones. Deshabilitarlo lo detiene (STOP PIPE) y habilitarlo lo vuelve a arrancar (START PIPE).";
const MASKED_NOTE: &str = "Las contraseñas y claves de los pipes se ocultan (******): el script no las trae y hay que agregarlas antes de ejecutarlo.";
/// What IoTDB shows instead of a password.
pub(crate) const MASK: &str = "******";

/// One continuous query.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Cq {
    pub id: String,
    pub query: Option<String>,
    pub state: String,
}

fn cell_text(v: Option<&J>) -> Option<String> {
    match v? {
        J::Null => None,
        J::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        v => Some(v.to_string()),
    }
}

/// `SHOW CQS` as continuous queries, by column name.
pub(crate) fn cqs(t: &Table) -> Vec<Cq> {
    let col = |name: &str| {
        t.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    };
    let (Some(id), query, state) = (col("CQId"), col("Query"), col("State")) else {
        return Vec::new();
    };
    t.rows
        .iter()
        .filter_map(|r| {
            let cq_id = cell_text(r.get(id))?;
            (!cq_id.chars().any(char::is_control)).then(|| Cq {
                id: cq_id,
                query: query.and_then(|i| cell_text(r.get(i))),
                state: state.and_then(|i| cell_text(r.get(i))).unwrap_or_default(),
            })
        })
        .collect()
}

/// The index of the keyword `kw` (a whole word, any case) in `sql`, outside
/// quotes.
pub(crate) fn keyword(sql: &str, kw: &str) -> Option<usize> {
    let lower = sql.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, b'\'' | b'"' | b'`') => quote = Some(c),
            None => {
                if lower[i..].starts_with(kw)
                    && (i == 0 || !b[i - 1].is_ascii_alphanumeric() && b[i - 1] != b'_')
                    && b.get(i + kw.len())
                        .is_none_or(|n| !n.is_ascii_alphanumeric() && *n != b'_')
                {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// The word after `kw` (`RESAMPLE EVERY 5s` → `5s` for `every`).
fn after(sql: &str, kw: &str) -> Option<String> {
    let i = keyword(sql, kw)? + kw.len();
    sql[i..]
        .split_whitespace()
        .next()
        .map(|w| w.trim_end_matches([',', ';']).to_string())
        .filter(|w| !w.is_empty())
}

/// How often the query runs: `RESAMPLE EVERY` when it has one, else its
/// `GROUP BY` interval (IoTDB's default).
pub(crate) fn every(sql: &str) -> Option<String> {
    let resample = keyword(sql, "resample");
    let begin = keyword(sql, "begin");
    if let (Some(r), Some(b)) = (resample, begin) {
        if r < b {
            if let Some(e) = keyword(&sql[r..b], "every") {
                let w = after(&sql[r + e..b], "every")?;
                return Some(format!("EVERY {w}"));
            }
        }
    }
    // `GROUP BY([…,] 10s[, …])` / `GROUP BY(10s)`: the interval is the
    // first argument that isn't a time range.
    let g = keyword(sql, "group")?;
    let rest = &sql[g..];
    let mut args = rest[rest.find('(')? + 1..].trim_start();
    // A time range first (`[start, end)` / `(start, end]`): skip it.
    if args.starts_with(['[', '(']) {
        let end = args.find([')', ']'])?;
        args = args[end + 1..].trim_start().strip_prefix(',')?;
    }
    let interval = args.split([',', ')']).next()?.trim();
    (!interval.is_empty()).then(|| format!("EVERY {interval}"))
}

pub(crate) fn job_of(cq: &Cq) -> ServerJob {
    ServerJob {
        id: format!("cq:{}", cq.id),
        name: cq.id.clone(),
        kind: KIND.into(),
        group: None,
        description: None,
        owner: None,
        enabled: cq.state.eq_ignore_ascii_case("ACTIVE"),
        schedule: cq.query.as_deref().and_then(every),
        running: false,
        last_outcome: None,
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: false,
        has_history: false,
        has_script: cq.query.is_some(),
        system: false,
    }
}

fn prop(label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: String::new(),
        label: label.into(),
        value: value.into(),
    }
}

pub(crate) fn detail_of(cq: &Cq) -> JobDetail {
    let job = job_of(cq);
    let mut properties = vec![prop("Estado", cq.state.clone())];
    if let Some(q) = &cq.query {
        for (kw, label) in [
            ("boundary", "Inicio (BOUNDARY)"),
            ("range", "Ventana (RANGE)"),
        ] {
            let r = keyword(q, "resample");
            let b = keyword(q, "begin");
            if let (Some(r), Some(b)) = (r, b) {
                if let Some(v) = (r < b).then(|| after(&q[r..b], kw)).flatten() {
                    properties.push(prop(label, v));
                }
            }
        }
        if let Some(i) = keyword(q, "timeout") {
            let policy = q[i..]
                .split_whitespace()
                .nth(2)
                .map(|w| w.trim_end_matches(';').to_string());
            if let Some(p) = policy {
                properties.push(prop("Si una ejecución se atrasa", p));
            }
        }
    }
    // The SELECT … INTO between BEGIN and END.
    let select = cq.query.as_deref().and_then(|q| {
        let b = keyword(q, "begin")? + "begin".len();
        let e = keyword(&q[b..], "end").map_or(q.len(), |e| b + e);
        Some(q[b..e].trim().to_string()).filter(|s| !s.is_empty())
    });
    JobDetail {
        steps: vec![JobStep {
            name: "Consulta".into(),
            kind: Some("SQL".into()),
            target: None,
            command: select.or_else(|| cq.query.clone()),
        }],
        schedules: Vec::new(),
        properties,
        notes: vec![CQ_NOTE.into()],
        job,
    }
}

pub(crate) fn denied(e: &Error) -> bool {
    let m = e.to_string().to_ascii_lowercase();
    m.contains("permission") || m.contains("privilege")
}

fn not_found(id: &str) -> Error {
    Error::Query(format!(
        "no existe el job «{}» o este usuario no lo puede ver",
        id.chars().take(200).collect::<String>()
    ))
}

// ---------------------------------------------------------------- pipes

/// One pipe of `SHOW PIPES`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Pipe {
    pub name: String,
    pub created: Option<String>,
    pub state: String,
    pub source: Vec<(String, String)>,
    pub processor: Vec<(String, String)>,
    pub sink: Vec<(String, String)>,
    pub error: Option<String>,
    /// IoTDB 2's backlog: remaining events and seconds.
    pub remaining: Option<String>,
    pub remaining_secs: Option<String>,
}

/// `{a=1, b.c=x, y}` → `[(a, 1), (b.c, "x, y")]`: a part without `=`
/// belongs to the value before it.
pub(crate) fn attributes(t: &str) -> Vec<(String, String)> {
    let inner = t.trim().trim_start_matches('{').trim_end_matches('}');
    let mut out: Vec<(String, String)> = Vec::new();
    for part in inner.split(", ") {
        match part.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() && !k.contains(' ') => {
                out.push((k.trim().to_string(), v.to_string()))
            }
            _ => match out.last_mut() {
                Some((_, v)) => {
                    v.push_str(", ");
                    v.push_str(part);
                }
                None if !part.trim().is_empty() => {
                    out.push((part.trim().to_string(), String::new()))
                }
                None => {}
            },
        }
    }
    out
}

/// `SHOW PIPES` as pipes, by column name, credentials hidden.
pub(crate) fn pipes(t: &Table) -> Vec<Pipe> {
    read_pipes(t, true)
}

/// `SHOW PIPES` as the server answered it: IoTDB 1.x shows the passwords
/// (2.x hides them as `******`). Only for writing the pipe back to the same
/// server (the scheduler's "keep the password it has"); never shown.
pub(crate) fn raw_pipes(t: &Table) -> Vec<Pipe> {
    read_pipes(t, false)
}

fn read_pipes(t: &Table, hide: bool) -> Vec<Pipe> {
    let col = |name: &str| {
        t.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    };
    let Some(id) = col("ID") else {
        return Vec::new();
    };
    let get = |r: &Vec<J>, c: Option<usize>| c.and_then(|i| cell_text(r.get(i)));
    t.rows
        .iter()
        .filter_map(|r| {
            let name = cell_text(r.get(id))?;
            if name.chars().any(char::is_control) {
                return None;
            }
            Some(Pipe {
                name,
                created: get(r, col("CreationTime")),
                state: get(r, col("State")).unwrap_or_default(),
                source: get(r, col("PipeSource"))
                    .map(|v| shown(attributes(&v), hide))
                    .unwrap_or_default(),
                processor: get(r, col("PipeProcessor"))
                    .map(|v| shown(attributes(&v), hide))
                    .unwrap_or_default(),
                sink: get(r, col("PipeSink"))
                    .map(|v| shown(attributes(&v), hide))
                    .unwrap_or_default(),
                error: get(r, col("ExceptionMessage")),
                remaining: get(r, col("RemainingEventCount")),
                remaining_secs: get(r, col("EstimatedRemainingSeconds")),
            })
        })
        .collect()
}

/// An attribute that holds a credential (`sink.password`, `…secret-key`,
/// `…token`): hidden here when the server didn't hide it already.
pub(crate) fn secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    ["password", "passwd", "pwd", "secret", "token", "credential"]
        .iter()
        .any(|w| k.contains(w))
        || k.ends_with("key")
}

/// A pipe clause's attributes, credentials hidden as IoTDB 2 does (when
/// `hide`).
fn shown(attrs: Vec<(String, String)>, hide: bool) -> Vec<(String, String)> {
    if !hide {
        return attrs;
    }
    attrs
        .into_iter()
        .map(|(k, v)| {
            if secret_key(&k) && !v.is_empty() {
                (k, MASK.to_string())
            } else {
                (k, v)
            }
        })
        .collect()
}

/// A pipe's name for a statement: always backquoted (a name that already
/// starts and ends with a backquote is a different name).
pub(crate) fn pipe_name(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

pub(crate) fn system_pipe(name: &str) -> bool {
    name.starts_with("__")
}

pub(crate) fn pipe_job(p: &Pipe) -> ServerJob {
    let running = p.state.eq_ignore_ascii_case("RUNNING");
    let sink = p
        .sink
        .iter()
        .find(|(k, _)| k == "sink" || k == "connector")
        .map(|(_, v)| v.clone());
    ServerJob {
        id: format!("pipe:{}", p.name),
        name: p.name.clone(),
        kind: PIPE_KIND.into(),
        group: None,
        description: sink,
        owner: None,
        enabled: !p.state.eq_ignore_ascii_case("STOPPED"),
        schedule: None,
        running,
        last_outcome: if p.error.is_some() {
            Some(JobOutcome::Failed)
        } else if running {
            Some(JobOutcome::Running)
        } else {
            None
        },
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: false,
        // IoTDB's own pipes are left alone.
        can_enable: !system_pipe(&p.name),
        has_history: false,
        has_script: !system_pipe(&p.name),
        system: system_pipe(&p.name),
    }
}

/// Whether a pipe carries a password IoTDB hid.
fn masked(p: &Pipe) -> bool {
    [&p.source, &p.processor, &p.sink]
        .iter()
        .any(|a| a.iter().any(|(_, v)| v == MASK))
}

pub(crate) fn pipe_detail(p: &Pipe) -> JobDetail {
    let job = pipe_job(p);
    let mut properties = vec![prop("Estado", p.state.clone())];
    if let Some(c) = &p.created {
        properties.push(prop("Creado", c.clone()));
    }
    if let Some(e) = &p.error {
        properties.push(prop(
            "Último error",
            e.chars().take(4000).collect::<String>(),
        ));
    }
    if let Some(n) = &p.remaining {
        properties.push(prop("Eventos pendientes", n.clone()));
    }
    if let Some(n) = &p.remaining_secs {
        properties.push(prop("Segundos estimados para ponerse al día", n.clone()));
    }
    for (group, attrs) in [
        ("Origen", &p.source),
        ("Procesador", &p.processor),
        ("Destino", &p.sink),
    ] {
        for (k, v) in attrs {
            properties.push(PropertyInfo {
                group: group.into(),
                label: k.clone(),
                value: v.clone(),
            });
        }
    }
    let mut notes = vec![PIPE_NOTE.to_string()];
    if masked(p) {
        notes.push(MASKED_NOTE.into());
    }
    if job.system {
        notes.push("Es un pipe interno de IoTDB: DBine no lo modifica.".into());
    }
    let attr = |a: &[(String, String)]| {
        a.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    JobDetail {
        steps: vec![JobStep {
            name: "Sincronización".into(),
            kind: Some("Pipe".into()),
            target: job.description.clone(),
            command: Some(format!(
                "SOURCE ({}) PROCESSOR ({}) SINK ({})",
                attr(&p.source),
                attr(&p.processor),
                attr(&p.sink)
            )),
        }],
        schedules: Vec::new(),
        properties,
        notes,
        job,
    }
}

fn dq(v: &str) -> String {
    format!("\"{}\"", v.replace('"', "\"\""))
}

/// The `CREATE PIPE` that recreates it: IoTDB's own attributes (`__…`)
/// and the hidden passwords left out, with the user that goes with each
/// (IoTDB 2 refuses a user without its password, and fills in the
/// creating user's when both are missing).
pub(crate) fn pipe_script(p: &Pipe) -> String {
    let clause = |word: &str, a: &[(String, String)]| {
        let hidden: Vec<&str> = a
            .iter()
            .filter(|(_, v)| v == MASK)
            .filter_map(|(k, _)| k.strip_suffix("password"))
            .collect();
        let paired = |k: &str| {
            hidden.iter().any(|pre| {
                k.strip_prefix(pre)
                    .is_some_and(|rest| rest == "username" || rest == "user")
            })
        };
        let kept: Vec<String> = a
            .iter()
            .filter(|(k, v)| !k.starts_with("__") && v != MASK && !paired(k))
            .map(|(k, v)| format!("{}={}", dq(k), dq(v)))
            .collect();
        if kept.is_empty() {
            String::new()
        } else {
            format!("\nWITH {word} ({})", kept.join(", "))
        }
    };
    format!(
        "CREATE PIPE {}{}{}{};\n",
        pipe_name(&p.name),
        clause("SOURCE", &p.source),
        clause("PROCESSOR", &p.processor),
        clause("SINK", &p.sink)
    )
}

/// What an id names.
enum Target {
    Cq(String),
    Pipe(String),
}

fn target(id: &str) -> Result<Target> {
    if let Some(c) = id.strip_prefix("cq:") {
        return Ok(Target::Cq(c.into()));
    }
    if let Some(p) = id.strip_prefix("pipe:") {
        return Ok(Target::Pipe(p.into()));
    }
    Err(Error::Query(format!(
        "«{}» no es un id de job de IoTDB",
        id.chars().take(200).collect::<String>()
    )))
}

impl IotDbSession {
    async fn cqs(&self) -> Result<Vec<Cq>> {
        Ok(cqs(&self.query("SHOW CQS", MAX_JOBS).await?))
    }

    async fn cq(&self, id: &str) -> Result<Cq> {
        self.cqs()
            .await?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| not_found(id))
    }

    async fn pipes(&self) -> Result<Vec<Pipe>> {
        Ok(pipes(&self.query("SHOW PIPES", MAX_JOBS).await?))
    }

    async fn pipe(&self, name: &str) -> Result<Pipe> {
        self.pipes()
            .await?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| not_found(name))
    }

    pub(crate) async fn server_jobs(&mut self) -> Result<JobList> {
        let mut list = JobList::default();
        // `query` hands one row past the limit when there are more.
        match self.query("SHOW CQS", MAX_JOBS).await {
            Ok(t) => {
                let all = cqs(&t);
                list.jobs.extend(all.iter().take(MAX_JOBS).map(job_of));
                if t.rows.len() > MAX_JOBS {
                    list.notes.push(format!(
                        "Se muestran las primeras {MAX_JOBS} consultas continuas."
                    ));
                }
            }
            Err(e) if denied(&e) => list.notes.push(DENIED.into()),
            Err(e) => return Err(e),
        }
        match self.query("SHOW PIPES", MAX_JOBS).await {
            Ok(t) => {
                let all = pipes(&t);
                list.jobs.extend(all.iter().take(MAX_JOBS).map(pipe_job));
                if t.rows.len() > MAX_JOBS {
                    list.notes
                        .push(format!("Se muestran los primeros {MAX_JOBS} pipes."));
                }
            }
            Err(e) if denied(&e) => list.notes.push(PIPES_DENIED.into()),
            Err(e) => return Err(e),
        }
        Ok(list)
    }

    pub(crate) async fn server_job_detail(&mut self, id: &str) -> Result<JobDetail> {
        match target(id)? {
            Target::Cq(c) => Ok(detail_of(&self.cq(&c).await?)),
            Target::Pipe(p) => Ok(pipe_detail(&self.pipe(&p).await?)),
        }
    }

    pub(crate) async fn set_server_job_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        match target(id)? {
            Target::Cq(_) => Err(Error::Unsupported(CQ_NOTE.into())),
            Target::Pipe(name) => {
                let p = self.pipe(&name).await?;
                if system_pipe(&p.name) {
                    return Err(Error::Query(format!(
                        "«{}» es un pipe interno de IoTDB: DBine no lo modifica",
                        p.name
                    )));
                }
                let verb = if enabled { "START" } else { "STOP" };
                self.non_query(&format!("{verb} PIPE {}", pipe_name(&p.name)))
                    .await
            }
        }
    }

    pub(crate) async fn server_job_script(&mut self, id: &str) -> Result<String> {
        match target(id)? {
            Target::Cq(c) => {
                let cq = self.cq(&c).await?;
                let q = cq.query.ok_or_else(|| {
                    Error::Query(format!(
                        "IoTDB no devolvió la sentencia de la consulta continua «{}»",
                        cq.id
                    ))
                })?;
                let q = q.trim_end().trim_end_matches(';');
                Ok(format!("{q};\n"))
            }
            Target::Pipe(name) => {
                let p = self.pipe(&name).await?;
                if system_pipe(&p.name) {
                    return Err(Error::Query(format!(
                        "«{}» es un pipe interno de IoTDB: no tiene script",
                        p.name
                    )));
                }
                Ok(pipe_script(&p))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ResultColumn;
    use serde_json::json;

    fn table(rows: Vec<Vec<J>>) -> Table {
        Table {
            columns: ["CQId", "Query", "State"]
                .iter()
                .map(|n| ResultColumn {
                    name: (*n).into(),
                    type_name: String::new(),
                })
                .collect(),
            rows,
        }
    }

    const Q1: &str = "CREATE CONTINUOUS QUERY cq1 RESAMPLE EVERY 5s BOUNDARY 0 RANGE 10s BEGIN SELECT count(s1) INTO root.db.agg(c) FROM root.db.d1 GROUP BY(5s) END";
    const Q2: &str = "CREATE CQ cq2 BEGIN SELECT max_value(t) INTO root.db.agg2(m) FROM root.db.d1 GROUP BY([0, 100), 10m) END";

    #[test]
    fn show_cqs_becomes_jobs() {
        let t = table(vec![
            vec![json!("cq1"), json!(Q1), json!("ACTIVE")],
            vec![json!("cq2"), json!(Q2), json!("INACTIVE")],
            vec![json!("bad\nid"), json!(Q2), json!("ACTIVE")],
        ]);
        let all = cqs(&t);
        assert_eq!(all.len(), 2);
        let jobs: Vec<ServerJob> = all.iter().map(job_of).collect();
        assert_eq!(jobs[0].id, "cq:cq1");
        assert_eq!(jobs[0].kind, "Continuous query");
        assert!(jobs[0].enabled && jobs[0].has_script);
        assert!(!jobs[0].can_run && !jobs[0].can_stop && !jobs[0].can_enable);
        assert!(!jobs[0].has_history && !jobs[0].running && !jobs[0].system);
        assert_eq!(jobs[0].schedule.as_deref(), Some("EVERY 5s"));
        assert!(!jobs[1].enabled);
        assert_eq!(jobs[1].schedule.as_deref(), Some("EVERY 10m"));
    }

    #[test]
    fn detail_reads_the_statement() {
        let cq = Cq {
            id: "cq1".into(),
            query: Some(Q1.to_string()),
            state: "ACTIVE".into(),
        };
        let d = detail_of(&cq);
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some("SELECT count(s1) INTO root.db.agg(c) FROM root.db.d1 GROUP BY(5s)")
        );
        let p = |l: &str| {
            d.properties
                .iter()
                .find(|p| p.label == l)
                .map(|p| p.value.as_str())
        };
        assert_eq!(p("Inicio (BOUNDARY)"), Some("0"));
        assert_eq!(p("Ventana (RANGE)"), Some("10s"));
        assert_eq!(p("Estado"), Some("ACTIVE"));
        assert!(d.notes[0].contains("DROP CQ"));
        let with_policy = Cq {
            query: Some(
                "CREATE CQ c3 RESAMPLE EVERY 1h BEGIN SELECT avg(s) INTO root.a.b(x) FROM root.a.c GROUP BY(1h) END TIMEOUT POLICY DISCARD"
                    .into(),
            ),
            ..cq
        };
        let d = detail_of(&with_policy);
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Si una ejecución se atrasa" && p.value == "DISCARD"));
        assert_eq!(d.job.schedule.as_deref(), Some("EVERY 1h"));
    }

    #[test]
    fn keywords_inside_quotes_or_words_are_not_keywords() {
        assert_eq!(keyword("select 'every' from t", "every"), None);
        assert_eq!(keyword("select everyone from t", "every"), None);
        assert_eq!(keyword("RESAMPLE EVERY 1m", "every"), Some(9));
        assert_eq!(every("CREATE CQ x BEGIN SELECT 1 END"), None);
    }

    #[test]
    fn a_table_without_the_columns_is_empty() {
        let t = Table {
            columns: vec![ResultColumn {
                name: "x".into(),
                type_name: String::new(),
            }],
            rows: vec![vec![json!("a")]],
        };
        assert!(cqs(&t).is_empty());
        assert!(denied(&Error::Query(
            "803: No permissions for this operation, please add privilege USE_CQ".into()
        )));
    }

    fn pipe_table(rows: Vec<Vec<J>>) -> Table {
        Table {
            columns: [
                "ID",
                "CreationTime",
                "State",
                "PipeSource",
                "PipeProcessor",
                "PipeSink",
                "ExceptionMessage",
                "RemainingEventCount",
            ]
            .iter()
            .map(|n| ResultColumn {
                name: (*n).into(),
                type_name: String::new(),
            })
            .collect(),
            rows,
        }
    }

    #[test]
    fn pipes_become_jobs() {
        let t = pipe_table(vec![
            vec![json!("sync a"), json!("2026-11-12T16:53:00.415"), json!("RUNNING"),
                 json!("{__system.sql-dialect=tree, source.password=******, source.pattern=root.a, source.username=root}"),
                 json!("{}"), json!("{sink=iotdb-thrift-sink, sink.node-urls=h1:6667, h2:6667}"), json!(""), json!("3")],
            vec![json!("p2"), json!("2026-11-12T16:53:00.030"), json!("STOPPED"), json!("{source.pattern=root.b}"),
                 json!("{}"), json!("{sink=do-nothing-sink}"), json!("connection refused"), json!(null)],
            vec![json!("__consensus.x"), json!(null), json!("RUNNING"), json!("{}"), json!("{}"), json!("{}"), json!(""), json!(null)],
        ]);
        let all = pipes(&t);
        assert_eq!(all.len(), 3);
        let jobs: Vec<ServerJob> = all.iter().map(pipe_job).collect();
        assert_eq!(jobs[0].id, "pipe:sync a");
        assert_eq!(jobs[0].kind, "Pipe");
        assert_eq!(jobs[0].description.as_deref(), Some("iotdb-thrift-sink"));
        assert!(jobs[0].enabled && jobs[0].running && jobs[0].can_enable && jobs[0].has_script);
        assert!(!jobs[0].can_run && !jobs[0].can_stop && !jobs[0].has_history);
        assert!(!jobs[1].enabled);
        assert_eq!(jobs[1].last_outcome, Some(JobOutcome::Failed));
        assert!(jobs[2].system && !jobs[2].can_enable && !jobs[2].has_script);
        assert_eq!(
            all[0].sink[1],
            ("sink.node-urls".to_string(), "h1:6667, h2:6667".to_string())
        );
        let d = pipe_detail(&all[0]);
        assert!(d.notes.iter().any(|n| n.contains("******")));
        assert!(d
            .properties
            .iter()
            .any(|p| p.group == "Destino" && p.label == "sink"));
        let sc = pipe_script(&all[0]);
        assert_eq!(
            sc,
            "CREATE PIPE `sync a`\nWITH SOURCE (\"source.pattern\"=\"root.a\")\nWITH SINK (\"sink\"=\"iotdb-thrift-sink\", \"sink.node-urls\"=\"h1:6667, h2:6667\");\n"
        );
        assert!(!sc.contains("******") && !sc.contains("__system") && !sc.contains("username"));
        assert!(matches!(target("pipe:p2"), Ok(Target::Pipe(n)) if n == "p2"));
        assert!(matches!(target("cq:c"), Ok(Target::Cq(n)) if n == "c"));
        assert!(target("p2").is_err());
        assert!(pipe_script(&all[1]).starts_with("CREATE PIPE `p2`\n"));
    }

    #[test]
    fn pipe_names_are_always_quoted() {
        assert_eq!(pipe_name("prod"), "`prod`");
        // A name made with backquotes in it isn't the name without them.
        assert_eq!(pipe_name("`prod`"), "```prod```");
        assert_eq!(
            pipe_name("a`; DROP DATABASE root.x; `b"),
            "`a``; DROP DATABASE root.x; ``b`"
        );
    }

    #[test]
    fn credentials_the_server_shows_are_hidden() {
        let t = pipe_table(vec![vec![
            json!("legacy"),
            json!(null),
            json!("RUNNING"),
            json!("{source.pattern=root.a}"),
            json!("{}"),
            json!("{sink=iotdb-thrift-sink, sink.node-urls=h1:6667, sink.user=admin, sink.password=s3cret, sink.access-key=AK, sink.token=t0k}"),
            json!(""),
            json!(null),
        ]]);
        let p = &pipes(&t)[0];
        let d = pipe_detail(p);
        let shown = |l: &str| {
            d.properties
                .iter()
                .find(|x| x.label == l)
                .map(|x| x.value.clone())
        };
        assert_eq!(shown("sink.password").as_deref(), Some("******"));
        assert_eq!(shown("sink.access-key").as_deref(), Some("******"));
        assert_eq!(shown("sink.token").as_deref(), Some("******"));
        assert_eq!(shown("sink.node-urls").as_deref(), Some("h1:6667"));
        assert!(d.notes.iter().any(|n| n.contains("******")));
        let command = d.steps[0].command.clone().unwrap();
        let sc = pipe_script(p);
        for text in [&command, &sc] {
            assert!(
                !text.contains("s3cret") && !text.contains("AK,") && !text.contains("t0k"),
                "{text}"
            );
        }
        // The user that goes with the hidden password is left out too.
        assert!(
            !sc.contains("sink.user") && sc.contains("sink.node-urls"),
            "{sc}"
        );
        assert!(secret_key("sink.ssl.trust-store-pwd") && !secret_key("source.pattern"));
    }
}

//! Server jobs ([`dbine_driver::Session::jobs`]): TDengine's streams, from
//! `information_schema.ins_streams`. A stream runs on the server on its
//! own, computing its query over the incoming data and writing the result
//! to a table.
//!
//! TDengine has two stream engines:
//!
//! - **Up to 3.3.6**: streams are named server-wide (`CREATE STREAM s …
//!   INTO db.t AS SELECT …`); `PAUSE STREAM` / `RESUME STREAM` (3.1 on)
//!   suspend and resume one. The row says its trigger mode (`trigger`) and
//!   its source and target.
//! - **3.3.7 on** (the stream engine was rewritten): streams live in a
//!   database (`CREATE STREAM db.s INTERVAL(10s) SLIDING(10s) FROM … INTO …`);
//!   `STOP STREAM` / `START STREAM` suspend and resume one. The row has
//!   `db_name`, which tells the two apart. The trigger (`PERIOD(1m)`,
//!   `INTERVAL(…) SLIDING(…)`, `SESSION(…)`…) is read from the statement.
//!
//! Neither keeps a history of runs nor has a "run now" or a "stop the
//! current run": a stream processes data as it arrives. The script is the
//! statement the stream was created with (`sql`).
//!
//! Ids are the stream's name (old engine) or `db.stream` (new one). One
//! that comes back has to be exactly one of the listed ids; the statements
//! then use that row's own names, quoted with backticks ([`crate::ddl::q`]).

use crate::ddl::q;
use crate::transfer::epoch_nanos;
use crate::TdSession;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobStep, PropertyInfo, Result, ServerJob,
};
use serde_json::{Map, Value};

const KIND: &str = "Stream";
/// Jobs listed at most.
pub(crate) const MAX_JOBS: usize = 5000;
/// Longest name a stream or a database can have.
pub(crate) const MAX_NAME: usize = 192;

pub(crate) const NO_PAUSE: &str = "Esta versión de TDengine no tiene PAUSE STREAM ni RESUME STREAM (llegaron en la 3.1): los streams no se pueden suspender.";
const STREAM_NOTE: &str = "Un stream procesa los datos a medida que llegan: no tiene «Ejecutar ahora», no se detiene una ejecución y TDengine no guarda un historial de ejecuciones.";

/// One stream as `ins_streams` has it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Stream {
    /// The new engine's database; `None` on the old one.
    pub db: Option<String>,
    pub name: String,
    pub status: String,
    pub sql: Option<String>,
    pub created: Option<String>,
    pub message: Option<String>,
    /// The rest of the row, for the detail.
    pub extra: Vec<(String, String)>,
}

impl Stream {
    pub fn id(&self) -> String {
        match &self.db {
            Some(db) => format!("{db}.{}", self.name),
            None => self.name.clone(),
        }
    }

    /// `` `db`.`s` `` or `` `s` ``, for the statements.
    pub(crate) fn target(&self) -> String {
        match &self.db {
            Some(db) => format!("{}.{}", q(db), q(&self.name)),
            None => q(&self.name),
        }
    }

    fn status_lower(&self) -> String {
        self.status.trim().to_ascii_lowercase()
    }

    pub(crate) fn enabled(&self) -> bool {
        !matches!(self.status_lower().as_str(), "paused" | "stopped" | "stop")
    }

    fn running(&self) -> bool {
        matches!(self.status_lower().as_str(), "ready" | "running")
    }

    fn outcome(&self) -> Option<JobOutcome> {
        match self.status_lower().as_str() {
            "failed" | "halt" | "error" => Some(JobOutcome::Failed),
            "ready" | "running" => Some(JobOutcome::Running),
            _ => None,
        }
    }
}

fn s(r: &Map<String, Value>, k: &str) -> Option<String> {
    match r.get(k)? {
        Value::Null => None,
        Value::String(v) => Some(v.trim().to_string()).filter(|v| !v.is_empty()),
        v => Some(v.to_string()),
    }
}

/// A name that can go between backticks and come back unchanged.
pub(crate) fn plain_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && !n.chars().any(|c| c == '`' || c.is_control())
}

/// `ins_streams` rows as streams (rows with an unusable name are left out).
pub(crate) fn streams(rows: &[Map<String, Value>]) -> Vec<Stream> {
    rows.iter()
        .filter_map(|r| {
            let name = s(r, "stream_name")?;
            // The new engine's database: a plain identifier (it can't hold a dot).
            let db = match s(r, "db_name") {
                Some(d) if plain_name(&d) && !d.contains('.') => Some(d),
                Some(_) => return None,
                None if r.contains_key("db_name") => return None,
                None => None,
            };
            if !plain_name(&name) {
                return None;
            }
            let skip = [
                "stream_name",
                "db_name",
                "status",
                "sql",
                "create_time",
                "message",
            ];
            let extra = r
                .iter()
                .filter(|(k, _)| !skip.contains(&k.as_str()))
                .filter_map(|(k, _)| s(r, k).map(|v| (k.clone(), v)))
                .collect();
            Some(Stream {
                db,
                name,
                status: s(r, "status").unwrap_or_default(),
                sql: s(r, "sql"),
                created: s(r, "create_time").and_then(|t| rfc3339(&t)),
                message: s(r, "message"),
                extra,
            })
        })
        .collect()
}

/// A TDengine instant (`2026-11-12T16:13:37.461Z`, or with the session's
/// offset) as RFC 3339 in UTC, to the second.
pub(crate) fn rfc3339(t: &str) -> Option<String> {
    let secs = i64::try_from(epoch_nanos(t)?.div_euclid(1_000_000_000)).ok()?;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    ))
}

/// The new engine's trigger, from its statement: what goes between the
/// stream's name and `FROM` / `STREAM_OPTIONS` / `NOTIFY` / `INTO` / `AS`
/// (`interval(10s) sliding(10s)`, `period(1m)`, `session(ts, 10s)`…).
pub(crate) fn trigger_of(sql: &str) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    let start = lower.find("create stream")? + "create stream".len();
    let mut rest = lower[start..].trim_start();
    let mut offset = lower.len() - rest.len();
    if let Some(r) = rest.strip_prefix("if not exists") {
        rest = r.trim_start();
        offset = lower.len() - rest.len();
    }
    // The name: a run of non-space characters (backticks keep spaces in,
    // in either part of `db.name`).
    let mut name_len = 0;
    let mut in_q = false;
    for (i, c) in rest.char_indices() {
        if c == '`' {
            in_q = !in_q;
        } else if c.is_whitespace() && !in_q {
            name_len = i;
            break;
        }
        name_len = i + c.len_utf8();
    }
    let after = offset + name_len;
    let tail = &lower[after..];
    let end = [
        " from ",
        " stream_options(",
        " stream_options (",
        " notify(",
        " notify (",
        " into ",
        " as select",
        " as\nselect",
    ]
    .iter()
    .filter_map(|k| tail.find(k))
    .min()
    .unwrap_or(tail.len());
    let t = sql[after..after + end].trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The old engine has `PAUSE` / `RESUME` from 3.1 on; the new one always
/// has `STOP` / `START`. An unknown version is taken as able.
pub(crate) fn can_suspend(version: &str, new_engine: bool) -> bool {
    if new_engine {
        return true;
    }
    let mut p = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .map(|x| x.parse::<u32>().unwrap_or(0));
    match (p.next(), p.next()) {
        (Some(major), Some(minor)) => (major, minor) >= (3, 1),
        _ => true,
    }
}

pub(crate) fn job_of(st: &Stream, suspend: bool) -> ServerJob {
    let schedule = match &st.db {
        Some(_) => st.sql.as_deref().and_then(trigger_of),
        None => st
            .extra
            .iter()
            .find(|(k, _)| k == "trigger")
            .map(|(_, v)| format!("TRIGGER {}", v.to_ascii_uppercase().replace(' ', "_"))),
    };
    let group = st.db.clone().or_else(|| {
        st.extra
            .iter()
            .find(|(k, _)| k == "source_db")
            .map(|(_, v)| v.clone())
    });
    ServerJob {
        id: st.id(),
        name: st.name.clone(),
        kind: KIND.into(),
        group,
        description: None,
        owner: None,
        enabled: st.enabled(),
        schedule,
        running: st.running(),
        last_outcome: st.outcome(),
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: suspend,
        has_history: false,
        has_script: st.sql.is_some(),
        system: false,
    }
}

fn prop(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: group.into(),
        label: label.into(),
        value: value.into(),
    }
}

/// Labels for the columns `ins_streams` has (others keep their name).
fn label(column: &str) -> Option<&'static str> {
    Some(match column {
        "stream_id" => "Id del stream",
        "history_id" => "Id del stream de históricos",
        "source_db" => "Base de origen",
        "target_db" => "Base de destino",
        "target_table" => "Tabla de destino",
        "watermark" => "Watermark",
        "trigger" => "Disparo",
        "sink_quota" => "Cuota de escritura",
        "checkpoint_interval" => "Intervalo de checkpoint",
        "checkpoint_backup" => "Copia del checkpoint",
        "history_scan_idle" => "Espera entre lecturas de históricos",
        "snodeLeader" => "Snode líder",
        "snodeReplica" => "Snode réplica",
        "external_sources" => "Orígenes externos",
        "realtime_lag_ms" => "Retraso en tiempo real (ms)",
        "input_rows_per_sec_1m" => "Filas leídas por segundo (1 min)",
        "output_rows_per_sec_1m" => "Filas escritas por segundo (1 min)",
        "runner_result_latency_avg_1m_ms" => "Latencia media del resultado (1 min, ms)",
        "history_progress_pct" => "Avance de los históricos (%)",
        "timezone" => "Zona horaria",
        "first_day_of_week" => "Primer día de la semana",
        _ => return None,
    })
}

pub(crate) fn detail_of(st: &Stream, suspend: bool) -> JobDetail {
    let job = job_of(st, suspend);
    let mut properties = vec![prop("", "Estado", st.status.clone())];
    if let Some(c) = &st.created {
        properties.push(prop("", "Creado", c.clone()));
    }
    if let Some(m) = &st.message {
        properties.push(prop("", "Mensaje", m.clone()));
    }
    for (k, v) in &st.extra {
        properties.push(prop("", label(k).unwrap_or(k.as_str()), v.clone()));
    }
    let mut notes = vec![STREAM_NOTE.to_string()];
    if !suspend {
        notes.push(NO_PAUSE.into());
    }
    if st.db.is_none() && job.can_enable {
        notes.push("Al reanudarlo, el stream procesa también los datos que llegaron mientras estuvo suspendido.".into());
    }
    JobDetail {
        steps: vec![JobStep {
            name: "Consulta".into(),
            kind: Some("SQL".into()),
            target: job.group.clone(),
            command: st.sql.clone(),
        }],
        schedules: Vec::new(),
        properties,
        notes,
        job,
    }
}

/// TDengine refuses to suspend or resume a stream whose tasks are still
/// starting (0x4104, "Invalid task status to proceed").
pub(crate) fn starting_up(e: Error, name: &str) -> Error {
    match &e {
        Error::Statement(se) if se.code.as_deref() == Some("0x4104") => Error::Query(format!(
            "El stream «{name}» todavía se está poniendo en marcha y no se puede suspender ni reanudar: probá de nuevo en unos segundos."
        )),
        _ => e,
    }
}

fn not_found(id: &str) -> Error {
    Error::Query(format!(
        "no existe el stream «{}» o este usuario no lo puede ver",
        id.chars().take(200).collect::<String>()
    ))
}

impl TdSession {
    async fn version_text(&self) -> String {
        self.strings("SELECT SERVER_VERSION()")
            .await
            .ok()
            .and_then(|r| r.first().and_then(|c| c.first()).cloned())
            .unwrap_or_default()
    }

    /// Every stream, and whether the server runs the new engine.
    async fn streams(&self) -> Result<(Vec<Stream>, bool)> {
        let a = self
            .query("SELECT * FROM information_schema.ins_streams")
            .await?;
        let new_engine = a.columns.iter().any(|(n, _)| n == "db_name");
        let rows: Vec<Map<String, Value>> = a
            .data
            .into_iter()
            .map(|r| {
                a.columns
                    .iter()
                    .map(|(n, _)| n.clone())
                    .zip(r)
                    .collect::<Map<_, _>>()
            })
            .collect();
        Ok((streams(&rows), new_engine))
    }

    /// The stream `id` names: one of the listed ids, exactly.
    async fn stream(&self, id: &str) -> Result<(Stream, bool)> {
        let (all, new_engine) = self.streams().await?;
        all.into_iter()
            .find(|s| s.id() == id)
            .map(|s| (s, new_engine))
            .ok_or_else(|| not_found(id))
    }

    pub(crate) async fn server_jobs(&mut self) -> Result<JobList> {
        let version = self.version_text().await;
        let (all, new_engine) = self.streams().await?;
        let suspend = can_suspend(&version, new_engine);
        let jobs: Vec<ServerJob> = all
            .iter()
            .take(MAX_JOBS)
            .map(|s| job_of(s, suspend))
            .collect();
        let mut notes = Vec::new();
        if !suspend && !jobs.is_empty() {
            notes.push(NO_PAUSE.to_string());
        }
        if all.len() > MAX_JOBS {
            notes.push(format!("Se muestran los primeros {MAX_JOBS} streams."));
        }
        Ok(JobList { jobs, notes })
    }

    pub(crate) async fn server_job_detail(&mut self, id: &str) -> Result<JobDetail> {
        let version = self.version_text().await;
        let (st, new_engine) = self.stream(id).await?;
        Ok(detail_of(&st, can_suspend(&version, new_engine)))
    }

    pub(crate) async fn set_server_job_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        let version = self.version_text().await;
        let (st, new_engine) = self.stream(id).await?;
        if !can_suspend(&version, new_engine) {
            return Err(Error::Unsupported(NO_PAUSE.into()));
        }
        let verb = match (new_engine, enabled) {
            (true, true) => "START",
            (true, false) => "STOP",
            (false, true) => "RESUME",
            (false, false) => "PAUSE",
        };
        self.query(&format!("{verb} STREAM {}", st.target()))
            .await
            .map(|_| ())
            .map_err(|e| starting_up(e, &st.name))
    }

    pub(crate) async fn server_job_script(&mut self, id: &str) -> Result<String> {
        let (st, _) = self.stream(id).await?;
        let sql = st.sql.ok_or_else(|| {
            Error::Query(format!(
                "TDengine no guardó la sentencia del stream «{}»",
                st.name
            ))
        })?;
        let sql = sql.trim_end().trim_end_matches(';');
        Ok(format!("{sql};\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn old_rows() -> Vec<Map<String, Value>> {
        vec![
            row(
                json!({"stream_name": "s_ok", "create_time": "2026-11-12T16:13:37.461Z", "stream_id": "0x1698",
                "history_id": null, "sql": "create stream if not exists s_ok into db1.agg as select _wstart, count(*) as c from db1.meters interval(10s)",
                "status": "ready", "source_db": "db1", "target_db": "db1", "target_table": "agg", "watermark": 0,
                "trigger": "window close", "message": " "}),
            ),
            row(
                json!({"stream_name": "s_paused", "create_time": "2026-11-12T19:13:37.461+03:00", "sql": "create stream s_paused …",
                "status": "paused", "source_db": "db2", "trigger": "at once"}),
            ),
            row(
                json!({"stream_name": "s_bad", "status": "failed", "sql": "create stream s_bad …", "message": "out of memory"}),
            ),
            row(json!({"stream_name": "x`; drop database db1", "status": "ready"})),
        ]
    }

    #[test]
    fn old_streams_become_jobs() {
        let st = streams(&old_rows());
        assert_eq!(st.len(), 3, "the name with a backtick is left out");
        let jobs: Vec<ServerJob> = st.iter().map(|s| job_of(s, true)).collect();
        let (ok, paused, bad) = (&jobs[0], &jobs[1], &jobs[2]);
        assert_eq!(ok.id, "s_ok");
        assert_eq!(ok.kind, "Stream");
        assert_eq!(ok.group.as_deref(), Some("db1"));
        assert_eq!(ok.schedule.as_deref(), Some("TRIGGER WINDOW_CLOSE"));
        assert!(ok.enabled && ok.running && ok.can_enable && ok.has_script);
        assert!(!ok.can_run && !ok.can_stop && !ok.has_history && !ok.system);
        assert_eq!(ok.last_outcome, Some(JobOutcome::Running));
        assert!(!paused.enabled && !paused.running);
        assert_eq!(paused.schedule.as_deref(), Some("TRIGGER AT_ONCE"));
        assert_eq!(bad.last_outcome, Some(JobOutcome::Failed));
        assert!(st[1].created.as_deref() == Some("2026-11-12T16:13:37Z"));
        // The message " " is no message.
        assert!(st[0].message.is_none());
        // Statements quote the name.
        assert_eq!(st[0].target(), "`s_ok`");
    }

    #[test]
    fn new_streams_live_in_a_database() {
        let rows = vec![
            row(
                json!({"stream_name": "s1", "db_name": "db1", "create_time": "2026-11-12T16:25:52.589Z",
                "stream_id": "48f1", "sql": "create stream db1.s1 interval(10s) sliding(10s) from db1.meters STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0)) into db1.res_t as select _twstart as ts, count(*) as c from db1.meters where ts >= _twstart and ts < _twend",
                "status": "Idle", "message": "Current deploy times: 0", "realtime_lag_ms": 12}),
            ),
            row(
                json!({"stream_name": "s2", "db_name": "db1", "sql": "create stream if not exists `db1`.`s 2` period(1m) into db1.p as select now, count(*) from db1.meters", "status": "Stopped"}),
            ),
            row(json!({"stream_name": "s3", "db_name": null, "status": "Running"})),
        ];
        let st = streams(&rows);
        assert_eq!(
            st.len(),
            2,
            "a new-engine row without a database is left out"
        );
        let jobs: Vec<ServerJob> = st.iter().map(|s| job_of(s, true)).collect();
        assert_eq!(jobs[0].id, "db1.s1");
        assert_eq!(jobs[0].group.as_deref(), Some("db1"));
        assert_eq!(
            jobs[0].schedule.as_deref(),
            Some("interval(10s) sliding(10s)")
        );
        assert!(jobs[0].enabled && !jobs[0].running);
        assert_eq!(jobs[1].schedule.as_deref(), Some("period(1m)"));
        assert!(!jobs[1].enabled);
        assert_eq!(st[0].target(), "`db1`.`s1`");
        let d = detail_of(&st[0], true);
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Retraso en tiempo real (ms)" && p.value == "12"));
        assert!(d.properties.iter().any(|p| p.label == "Mensaje"));
        assert_eq!(d.steps[0].command, st[0].sql);
    }

    #[test]
    fn triggers_from_the_statement() {
        assert_eq!(
            trigger_of("CREATE STREAM db.s SESSION(ts, 10s) FROM db.t INTO db.o AS SELECT 1")
                .as_deref(),
            Some("SESSION(ts, 10s)")
        );
        assert_eq!(
            trigger_of("create stream db.s period(1m) notify('ws://h') as select now()").as_deref(),
            Some("period(1m)")
        );
        assert_eq!(trigger_of("create stream s into t as select 1"), None);
        assert_eq!(trigger_of("select 1"), None);
        // Backticks in either part of the name keep its spaces in it.
        for sql in [
            "create stream db.`s 8` period(1m) into db.o as select now()",
            "create stream `my db`.`s 8` period(1m) into db.o as select now()",
            "create stream `s 8` period(1m) into db.o as select now()",
        ] {
            assert_eq!(trigger_of(sql).as_deref(), Some("period(1m)"), "{sql}");
        }
    }

    #[test]
    fn suspend_needs_3_1_on_the_old_engine() {
        assert!(can_suspend("3.3.6.13", false));
        assert!(can_suspend("3.1.0.0", false));
        assert!(!can_suspend("3.0.7.1", false));
        assert!(can_suspend("3.0.7.1", true));
        assert!(can_suspend("", false));
        let st = &streams(&old_rows())[0];
        let d = detail_of(st, false);
        assert!(!d.job.can_enable);
        assert!(d.notes.iter().any(|n| n.contains("PAUSE STREAM")));
    }

    #[test]
    fn a_stream_starting_up_says_so() {
        let e: Error = dbine_driver::ScriptError::new("Invalid task status to proceed")
            .with_code("0x4104")
            .into();
        assert!(starting_up(e, "s1")
            .to_string()
            .contains("poniendo en marcha"));
        let other: Error = dbine_driver::ScriptError::new("x")
            .with_code("0x2600")
            .into();
        assert!(starting_up(other, "s1").to_string().contains('x'));
    }

    #[test]
    fn instants_go_to_utc() {
        assert_eq!(
            rfc3339("2026-11-12T16:13:37.461Z").as_deref(),
            Some("2026-11-12T16:13:37Z")
        );
        assert_eq!(
            rfc3339("2026-11-12T01:00:00-03:00").as_deref(),
            Some("2026-11-12T04:00:00Z")
        );
        assert_eq!(rfc3339("nope"), None);
    }
}

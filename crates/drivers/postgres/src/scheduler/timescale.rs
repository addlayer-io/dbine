//! TimescaleDB in the designer: user-defined jobs (`add_job` /
//! `alter_job` / `delete_job`) and its policies, each a kind of its own
//! (compression, retention, continuous aggregate refresh, reorder and, from
//! 2.2x, compaction: `add_*_policy` / `alter_job` / `remove_*_policy`).
//!
//! - A policy's settings live in its job's `config`: an edit rewrites the
//!   keys that changed (`alter_job(config => …)`, which TimescaleDB checks
//!   with the policy's own function); another hypertable is a new policy
//!   (the old one removed). A job's procedure can't change either: the job
//!   is deleted and added again, with a new id and without its statistics
//!   and history.
//! - Intervals are checked and written as the server reads them (`'…'::
//!   interval::text`); a policy's "after" values take a number instead on a
//!   hypertable with integer time.
//! - `alter_job` drops the job's time zone when the call doesn't name it
//!   (2.30): every call passes the time zone the job keeps.
//! - The extension's own jobs (ids below 1000) can be rescheduled and
//!   reconfigured, not deleted.

use super::{
    bad, datetime_field, id as ident, name_of, pick, plain, shown, transaction, ts_lit, utc_sql,
    with_current, Ctx, NewId, Plan, Vals,
};
use crate::catalog::{comment_text, lit};
use crate::jobs::{flag, text, JobId};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
    SchedulerKind, SchedulerList,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Jobs below this id are the extension's own.
const FIRST_USER_JOB: i32 = 1000;

const SYSTEM_JOB: &str =
    "Este job es de TimescaleDB: se puede cambiar su programación y su configuración, pero no borrarlo.";
const NOT_OWNER: &str = "Solo el dueño del job (o un superusuario) puede cambiarlo.";
const NEW_PROC: &str = "TimescaleDB no permite cambiar el procedimiento de un job: al guardar otro, el job se borra y se crea de nuevo, con otro id y sin sus estadísticas ni su historial.";
const NEW_TARGET: &str = "Cambiar la hipertabla (o el agregado continuo) de una política la crea de nuevo sobre la otra y quita la actual: otro id, sin sus estadísticas ni su historial.";

// ---------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    Compression,
    Retention,
    Refresh,
    Reorder,
    Compaction,
}

impl Policy {
    pub(crate) const ALL: [Policy; 5] = [
        Policy::Compression,
        Policy::Retention,
        Policy::Refresh,
        Policy::Reorder,
        Policy::Compaction,
    ];

    pub(crate) fn from_key(key: &str) -> Option<Policy> {
        Policy::ALL.into_iter().find(|p| p.key() == key)
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Policy::Compression => "compression_policy",
            Policy::Retention => "retention_policy",
            Policy::Refresh => "refresh_policy",
            Policy::Reorder => "reorder_policy",
            Policy::Compaction => "compaction_policy",
        }
    }

    /// The procedure its jobs run.
    fn proc_name(self) -> &'static str {
        match self {
            Policy::Compression => "policy_compression",
            Policy::Retention => "policy_retention",
            Policy::Refresh => "policy_refresh_continuous_aggregate",
            Policy::Reorder => "policy_reorder",
            Policy::Compaction => "policy_compaction",
        }
    }

    fn of(schema: &str, proc: &str) -> Option<Policy> {
        if !schema.starts_with("_timescaledb") {
            return None;
        }
        Policy::ALL.into_iter().find(|p| p.proc_name() == proc)
    }

    pub(crate) fn kind(self) -> SchedulerKind {
        let k = match self {
            Policy::Compression => SchedulerKind::new(
                "compression_policy",
                "Políticas de compresión",
                "política de compresión",
            ),
            Policy::Retention => SchedulerKind::new(
                "retention_policy",
                "Políticas de retención",
                "política de retención",
            ),
            Policy::Refresh => SchedulerKind::new(
                "refresh_policy",
                "Políticas de agregados continuos",
                "política de agregado continuo",
            ),
            Policy::Reorder => SchedulerKind::new(
                "reorder_policy",
                "Políticas de reordenamiento",
                "política de reordenamiento",
            ),
            Policy::Compaction => SchedulerKind::new(
                "compaction_policy",
                "Políticas de compactación",
                "política de compactación",
            ),
        };
        k.abilities(true, true, true)
    }

    fn add_fn(self) -> &'static str {
        match self {
            Policy::Compression => "add_compression_policy",
            Policy::Retention => "add_retention_policy",
            Policy::Refresh => "add_continuous_aggregate_policy",
            Policy::Reorder => "add_reorder_policy",
            Policy::Compaction => "add_compaction_policy",
        }
    }

    fn remove_fn(self) -> &'static str {
        match self {
            Policy::Compression => "remove_compression_policy",
            Policy::Retention => "remove_retention_policy",
            Policy::Refresh => "remove_continuous_aggregate_policy",
            Policy::Reorder => "remove_reorder_policy",
            Policy::Compaction => "remove_compaction_policy",
        }
    }

    /// The source of its target's choices.
    fn target_source(self) -> &'static str {
        match self {
            Policy::Compression | Policy::Compaction => "compressed_hypertable",
            Policy::Retention => "retention_target",
            Policy::Refresh => "continuous_aggregate",
            Policy::Reorder => "hypertable",
        }
    }

    /// The `add_*_policy` takes `schedule_interval`.
    fn takes_interval(self) -> bool {
        self != Policy::Reorder
    }

    fn title(self) -> &'static str {
        match self {
            Policy::Compression => "política de compresión",
            Policy::Retention => "política de retención",
            Policy::Refresh => "política de agregado continuo",
            Policy::Reorder => "política de reordenamiento",
            Policy::Compaction => "política de compactación",
        }
    }
}

/// A policy parameter: its `config` key (the `add_*_policy` argument of
/// the same name) and how it's typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Param {
    /// An interval, or a number on integer time.
    After,
    /// An interval, or a number on integer time, or empty (NULL).
    Offset,
    Interval,
    Int,
    /// `true`, `false` or the server's default (unset).
    Tri,
    Index,
}

fn params(p: Policy) -> &'static [(&'static str, Param)] {
    match p {
        Policy::Compression => &[
            ("compress_after", Param::After),
            ("compress_created_before", Param::Interval),
        ],
        Policy::Retention => &[
            ("drop_after", Param::After),
            ("drop_created_before", Param::Interval),
        ],
        Policy::Refresh => &[
            ("start_offset", Param::Offset),
            ("end_offset", Param::Offset),
            ("buckets_per_batch", Param::Int),
            ("max_batches_per_execution", Param::Int),
            ("refresh_newest_first", Param::Tri),
            ("include_tiered_data", Param::Tri),
        ],
        Policy::Reorder => &[("index_name", Param::Index)],
        Policy::Compaction => &[
            ("max_chunks", Param::Int),
            ("max_batches", Param::Int),
            ("inactive_for", Param::Interval),
        ],
    }
}

/// The policy's fields (only those this version's function takes).
fn param_fields(p: Policy, takes: &dyn Fn(&str) -> bool) -> Vec<FormField> {
    let all = match p {
        Policy::Compression => vec![
            FormField::new("compress_after", "Comprimir después de", FormKind::Text)
                .placeholder("7 days")
                .help("La antigüedad desde la que se comprimen los chunks: un intervalo o, en una hipertabla con tiempo entero, un número."),
            FormField::new("compress_created_before", "Comprimir lo creado antes de", FormKind::Text)
                .placeholder("7 days")
                .help("Un intervalo: comprime los chunks creados antes de ese tiempo, en lugar de «Comprimir después de»."),
        ],
        Policy::Retention => vec![
            FormField::new("drop_after", "Borrar después de", FormKind::Text)
                .placeholder("90 days")
                .help("La antigüedad desde la que se borran los chunks: un intervalo o, en una hipertabla con tiempo entero, un número."),
            FormField::new("drop_created_before", "Borrar lo creado antes de", FormKind::Text)
                .placeholder("90 days")
                .help("Un intervalo: borra los chunks creados antes de ese tiempo, en lugar de «Borrar después de»."),
        ],
        Policy::Refresh => vec![
            FormField::new("start_offset", "Desde (antigüedad)", FormKind::Text)
                .placeholder("1 month")
                .help("El comienzo de la ventana que se actualiza: un intervalo, un número en tiempo entero, o vacío para desde el principio."),
            FormField::new("end_offset", "Hasta (antigüedad)", FormKind::Text)
                .placeholder("1 hour")
                .help("El fin de la ventana que se actualiza: un intervalo, un número en tiempo entero, o vacío para hasta el final."),
            FormField::new("buckets_per_batch", "Buckets por lote", FormKind::number(Some(0.0), None))
                .help("Cuántos buckets se actualizan en cada transacción. Vacío: el valor de TimescaleDB."),
            FormField::new("max_batches_per_execution", "Lotes por ejecución", FormKind::number(Some(0.0), None))
                .help("Cuántos lotes hace como mucho cada ejecución; 0: sin límite. Vacío: el valor de TimescaleDB."),
            FormField::new(
                "refresh_newest_first",
                "Actualizar primero lo más nuevo",
                FormKind::select(&[
                    ("", "Según TimescaleDB"),
                    ("true", "Sí"),
                    ("false", "No"),
                ]),
            ),
            FormField::new(
                "include_tiered_data",
                "Incluir los datos en almacenamiento por niveles",
                FormKind::select(&[
                    ("", "Según la configuración del servidor"),
                    ("true", "Sí"),
                    ("false", "No"),
                ]),
            ),
        ],
        Policy::Reorder => vec![FormField::new("index_name", "Índice", FormKind::Text)
            .required()
            .help("El índice de la hipertabla por el que se reordenan los chunks.")],
        Policy::Compaction => vec![
            FormField::new("max_chunks", "Máximo de chunks", FormKind::number(Some(0.0), None))
                .help("Vacío: el valor de TimescaleDB."),
            FormField::new("max_batches", "Máximo de lotes", FormKind::number(Some(0.0), None))
                .help("Vacío: el valor de TimescaleDB."),
            FormField::new("inactive_for", "Inactivos durante", FormKind::Text)
                .placeholder("2 hours")
                .help("Un intervalo: compacta los chunks que no cambiaron en ese tiempo."),
        ],
    };
    all.into_iter().filter(|f| takes(&f.key)).collect()
}

// ---------------------------------------------------------------------------
// What the installed TimescaleDB has
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    /// The schema of its functions.
    pub(crate) schema: String,
    pub(crate) superuser: bool,
    /// `add_job` / `alter_job` take `job_name`.
    pub(crate) job_name: bool,
    pub(crate) alter_job_name: bool,
    /// `add_job` and `alter_job` take `timezone`.
    pub(crate) timezone: bool,
    /// `add_job` and `alter_job` take `fixed_schedule` (2.9+).
    pub(crate) fixed_schedule: bool,
    /// `add_job` and `alter_job` take `check_config` (2.7+).
    pub(crate) check_config: bool,
    /// `alter_job` takes `initial_start` (2.9+; `add_job` always did).
    pub(crate) alter_initial_start: bool,
    /// `add_compaction_policy` is there.
    pub(crate) compaction: bool,
    /// The arguments each `add_*_policy` takes, by function.
    pub(crate) args: BTreeMap<String, Vec<String>>,
    /// The table that keeps the runs (`bgw_job_stat_history` 2.15+,
    /// `job_errors` 2.10–2.14) and whether the user may delete from it.
    pub(crate) history: Option<String>,
    pub(crate) clear: bool,
}

impl Caps {
    pub(crate) fn policies(&self) -> Vec<Policy> {
        Policy::ALL
            .into_iter()
            .filter(|p| *p != Policy::Compaction || self.compaction)
            .collect()
    }

    fn takes(&self, p: Policy, arg: &str) -> bool {
        self.args
            .get(p.add_fn())
            .is_some_and(|a| a.iter().any(|x| x == arg))
    }

    /// `schema.function`, quoted.
    fn f(&self, name: &str) -> String {
        format!("{}.{name}", ident(&self.schema))
    }
}

fn caps_sql(v: Variant, schema: &str) -> String {
    let s = lit(v, schema);
    let has_arg = |f: &str, arg: &str| {
        format!(
            "EXISTS (SELECT 1 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                     WHERE n.nspname = {s} AND p.proname = '{f}' AND '{arg}' = ANY (p.proargnames))"
        )
    };
    let rel = |name: &str| {
        format!(
            "(SELECT has_table_privilege(c.oid, 'DELETE') FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = '_timescaledb_internal' AND c.relname = '{name}')"
        )
    };
    format!(
        "SELECT coalesce((SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user), false) AS superuser,
                {job_name} AS job_name, {alter_name} AS alter_job_name, {tz} AS timezone,
                {fixed} AS fixed_schedule, {check} AS check_config, {initial} AS alter_initial_start,
                {history} AS history, {errors} AS errors,
                (SELECT string_agg(p.proname || ':' || array_to_string(p.proargnames, ','), ';')
                   FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                  WHERE n.nspname = {s} AND p.proname LIKE 'add\\_%\\_policy') AS policy_args",
        job_name = has_arg("add_job", "job_name"),
        alter_name = has_arg("alter_job", "job_name"),
        tz = has_arg("alter_job", "timezone"),
        fixed = has_arg("add_job", "fixed_schedule"),
        check = has_arg("add_job", "check_config"),
        initial = has_arg("alter_job", "initial_start"),
        history = rel("bgw_job_stat_history"),
        errors = rel("job_errors"),
    )
}

pub(crate) async fn caps(s: &PgSession, schema: &str) -> Result<Caps> {
    let rows = s.text(&caps_sql(s.variant, schema)).await?;
    let Some(r) = rows.first() else {
        return Ok(Caps {
            schema: schema.into(),
            ..Caps::default()
        });
    };
    let mut args = BTreeMap::new();
    for part in r.get("policy_args").unwrap_or_default().split(';') {
        if let Some((f, a)) = part.split_once(':') {
            args.insert(
                f.to_string(),
                a.split(',').map(str::to_string).collect::<Vec<_>>(),
            );
        }
    }
    let (history, clear) = match (r.get("history"), r.get("errors")) {
        (Some(h), _) => (
            Some("_timescaledb_internal.bgw_job_stat_history".to_string()),
            matches!(h, "t" | "true"),
        ),
        (None, Some(e)) => (
            Some("_timescaledb_internal.job_errors".to_string()),
            matches!(e, "t" | "true"),
        ),
        _ => (None, false),
    };
    Ok(Caps {
        schema: schema.into(),
        superuser: flag(r, "superuser"),
        job_name: flag(r, "job_name"),
        alter_job_name: flag(r, "alter_job_name"),
        timezone: flag(r, "timezone"),
        fixed_schedule: flag(r, "fixed_schedule"),
        check_config: flag(r, "check_config"),
        alter_initial_start: flag(r, "alter_initial_start"),
        compaction: args.contains_key("add_compaction_policy"),
        args,
        history,
        clear,
    })
}

fn caps_of(c: &Ctx) -> Result<&Caps> {
    c.ts.as_ref()
        .ok_or_else(|| Error::Query("TimescaleDB no está instalado en esta base".into()))
}

// ---------------------------------------------------------------------------
// A job as the server has it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Job {
    pub(crate) id: i32,
    /// `application_name`, without the ` [id]` TimescaleDB adds.
    pub(crate) name: String,
    pub(crate) proc_schema: String,
    pub(crate) proc_name: String,
    pub(crate) owner: String,
    pub(crate) every: String,
    pub(crate) max_runtime: String,
    pub(crate) max_retries: i64,
    pub(crate) retry_period: String,
    pub(crate) scheduled: bool,
    pub(crate) fixed_schedule: Option<bool>,
    pub(crate) initial_start: Option<String>,
    pub(crate) next_start: Option<String>,
    pub(crate) config: Option<String>,
    /// `schema.name`, quoted.
    pub(crate) check: Option<String>,
    pub(crate) timezone: Option<String>,
    /// What a policy works on, quoted: the continuous aggregate or the
    /// hypertable.
    pub(crate) target: Option<String>,
    /// The user may change it (its owner's rights).
    pub(crate) mine: bool,
}

impl Job {
    pub(crate) fn policy(&self) -> Option<Policy> {
        Policy::of(&self.proc_schema, &self.proc_name)
    }

    fn system(&self) -> bool {
        self.id < FIRST_USER_JOB
    }

    fn proc(&self) -> String {
        format!("{}.{}", ident(&self.proc_schema), ident(&self.proc_name))
    }

    fn config_map(&self) -> Map<String, Value> {
        self.config
            .as_deref()
            .and_then(|c| serde_json::from_str::<Value>(c).ok())
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default()
    }
}

fn jobs_sql(filter: &str) -> String {
    format!(
        "SELECT j.job_id::text AS id, j.application_name::text AS name, j.schedule_interval::text AS every,
                j.max_runtime::text AS max_runtime, j.max_retries::text AS max_retries,
                j.retry_period::text AS retry_period, j.proc_schema::text AS proc_schema,
                j.proc_name::text AS proc_name, j.owner::text AS owner, j.scheduled,
                to_jsonb(j) ->> 'fixed_schedule' AS fixed_schedule,
                {initial} AS initial_start, {next} AS next_start, j.config::text AS config,
                to_jsonb(j) ->> 'check_schema' AS check_schema, to_jsonb(j) ->> 'check_name' AS check_name,
                (SELECT to_jsonb(b) ->> 'timezone' FROM _timescaledb_config.bgw_job b WHERE b.id = j.job_id) AS timezone,
                j.hypertable_schema::text AS hypertable_schema, j.hypertable_name::text AS hypertable_name,
                ca.view_schema::text AS view_schema, ca.view_name::text AS view_name,
                pg_catalog.pg_has_role(j.owner::oid, 'USAGE') AS mine
         FROM timescaledb_information.jobs j
         LEFT JOIN timescaledb_information.continuous_aggregates ca
                ON (ca.materialization_hypertable_schema = j.hypertable_schema
                    AND ca.materialization_hypertable_name = j.hypertable_name)
                OR (ca.view_schema = j.hypertable_schema AND ca.view_name = j.hypertable_name)
         {filter}
         ORDER BY j.job_id",
        initial = utc_sql("(to_jsonb(j) ->> 'initial_start')::timestamptz"),
        next = utc_sql("j.next_start"),
    )
}

fn job_of(r: &dyn crate::jobs::Row) -> Option<Job> {
    let id: i32 = text(r, "id")?.parse().ok()?;
    let name = r.get("name").unwrap_or_default();
    let suffix = format!(" [{id}]");
    let target = match (text(r, "view_schema"), text(r, "view_name")) {
        (Some(s), Some(v)) => Some(format!("{}.{}", ident(&s), ident(&v))),
        _ => match (text(r, "hypertable_schema"), text(r, "hypertable_name")) {
            (Some(s), Some(h)) => Some(format!("{}.{}", ident(&s), ident(&h))),
            _ => None,
        },
    };
    Some(Job {
        id,
        name: name.strip_suffix(&suffix).unwrap_or(&name).to_string(),
        proc_schema: r.get("proc_schema").unwrap_or_default(),
        proc_name: r.get("proc_name").unwrap_or_default(),
        owner: r.get("owner").unwrap_or_default(),
        every: r.get("every").unwrap_or_default(),
        max_runtime: r.get("max_runtime").unwrap_or_default(),
        max_retries: text(r, "max_retries")
            .and_then(|m| m.parse().ok())
            .unwrap_or(-1),
        retry_period: r.get("retry_period").unwrap_or_default(),
        scheduled: flag(r, "scheduled"),
        fixed_schedule: text(r, "fixed_schedule").map(|f| f == "true"),
        initial_start: text(r, "initial_start"),
        next_start: text(r, "next_start"),
        config: text(r, "config").filter(|c| c != "null"),
        check: match (text(r, "check_schema"), text(r, "check_name")) {
            (Some(s), Some(n)) => Some(format!("{}.{}", ident(&s), ident(&n))),
            _ => None,
        },
        timezone: text(r, "timezone"),
        target,
        mine: flag(r, "mine"),
    })
}

async fn read(s: &PgSession, n: i32) -> Result<Job> {
    s.text(&jobs_sql(&format!("WHERE j.job_id = {n}")))
        .await?
        .first()
        .and_then(|r| job_of(r))
        .ok_or_else(|| {
            Error::Query(format!(
                "no existe el job {} (o tu usuario no lo ve)",
                JobId::Timescale(n)
            ))
        })
}

/// A policy id: a TimescaleDB job id.
pub(crate) fn policy_id(id: &str) -> Result<i32> {
    match JobId::parse(id)? {
        JobId::Timescale(n) => Ok(n),
        _ => Err(bad(format!(
            "«{}» no es un id de política de TimescaleDB",
            shown(id)
        ))),
    }
}

pub(crate) async fn policy_items(s: &PgSession, c: &Ctx, p: Policy) -> Result<SchedulerList> {
    caps_of(c)?;
    let rows = s
        .text(&jobs_sql(&format!(
            "WHERE j.proc_name = '{}' AND j.proc_schema::text LIKE '\\_timescaledb%'",
            p.proc_name()
        )))
        .await?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .filter_map(|r| job_of(r))
            .map(|j| SchedulerItem {
                id: JobId::Timescale(j.id).to_string(),
                name: j
                    .target
                    .as_deref()
                    .map(unquote)
                    .unwrap_or_else(|| j.name.clone()),
                description: Some(j.name.clone()),
                enabled: Some(j.scheduled),
                system: false,
                can_edit: None,
                can_delete: None,
                actions: None,
            })
            .collect(),
        notes: Vec::new(),
    })
}

/// `"s"."t"` as `s.t`.
fn unquote(q: &str) -> String {
    let mut out = String::new();
    let mut chars = q.chars().peekable();
    let mut inside = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if inside && chars.peek() == Some(&'"') => {
                out.push('"');
                chars.next();
            }
            '"' => inside = !inside,
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Choices
// ---------------------------------------------------------------------------

/// `(schema, name)` rows as quoted values and `schema.name` labels.
fn qualified(rows: &[tokio_postgres::SimpleQueryRow]) -> Vec<(String, String)> {
    rows.iter()
        .filter_map(|r| {
            let s = r.get(0)?;
            let n = r.get(1)?;
            Some((format!("{}.{}", ident(s), ident(n)), format!("{s}.{n}")))
        })
        .collect()
}

const OWN_SCHEMAS: &str =
    "n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE '\\_timescaledb%' AND n.nspname NOT LIKE 'pg\\_toast%'";

async fn procedures(s: &PgSession) -> Result<Vec<(String, String)>> {
    let rows = s
        .text(&format!(
            "SELECT n.nspname, p.proname FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             WHERE p.pronargs = 2 AND p.proargtypes[0] = 'integer'::regtype AND p.proargtypes[1] = 'jsonb'::regtype
               AND {OWN_SCHEMAS}
             ORDER BY 1, 2"
        ))
        .await?;
    Ok(qualified(&rows))
}

async fn check_functions(s: &PgSession) -> Result<Vec<(String, String)>> {
    let rows = s
        .text(&format!(
            "SELECT n.nspname, p.proname FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             WHERE p.pronargs = 1 AND p.proargtypes[0] = 'jsonb'::regtype AND p.prokind = 'f'
               AND {OWN_SCHEMAS}
             ORDER BY 1, 2"
        ))
        .await?;
    Ok(qualified(&rows))
}

async fn hypertables(s: &PgSession, compressed: bool) -> Result<Vec<(String, String)>> {
    let filter = if compressed {
        " WHERE compression_enabled"
    } else {
        ""
    };
    let rows = s
        .text(&format!(
            "SELECT hypertable_schema::text, hypertable_name::text FROM timescaledb_information.hypertables{filter} ORDER BY 1, 2"
        ))
        .await?;
    Ok(qualified(&rows))
}

async fn caggs(s: &PgSession) -> Result<Vec<(String, String)>> {
    let rows = s
        .text("SELECT view_schema::text, view_name::text FROM timescaledb_information.continuous_aggregates ORDER BY 1, 2")
        .await?;
    Ok(qualified(&rows))
}

async fn targets(s: &PgSession, p: Policy) -> Result<Vec<(String, String)>> {
    Ok(match p {
        Policy::Compression | Policy::Compaction => hypertables(s, true).await?,
        Policy::Reorder => hypertables(s, false).await?,
        Policy::Refresh => caggs(s).await?,
        Policy::Retention => {
            let mut all = hypertables(s, false).await?;
            all.extend(caggs(s).await?);
            all
        }
    })
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

/// The interval of a new `p` may stay empty: its `add_*_policy` has a
/// default (a refresh policy's has none).
fn default_interval(p: Option<Policy>) -> bool {
    p.is_some_and(|p| p != Policy::Refresh)
}

/// The schedule and execution fields every job has. `policy`: the policy's
/// (`None`: a user-defined job). Only what this version takes shows.
fn schedule_sections(caps: &Caps, existing: bool, policy: Option<Policy>) -> Vec<FormSection> {
    let interval = FormField::new("schedule_interval", "Intervalo", FormKind::Text)
        .placeholder("1 hour")
        .help("Cada cuánto se ejecuta: un intervalo de PostgreSQL (30 minutes, 1 hour, 1 day).");
    let mut schedule = FormSection::new("schedule", "Programación").field(
        if default_interval(policy) && !existing {
            interval.help("Cada cuánto se ejecuta: un intervalo de PostgreSQL. Vacío: el de TimescaleDB para esta política.")
        } else {
            interval.required()
        },
    );
    if caps.fixed_schedule {
        schedule = schedule.field(
            FormField::new("fixed_schedule", "Horario fijo", FormKind::Bool)
                .default(json!(policy.is_none()))
                .help("Sí: cada ejecución empieza en un múltiplo del intervalo desde el primer inicio. No: el intervalo se cuenta desde el fin de la anterior."),
        );
    }
    let initial = if existing {
        caps.alter_initial_start
    } else {
        policy.is_none_or(|p| caps.takes(p, "initial_start"))
    };
    if initial {
        schedule = schedule.field(
            FormField::new("initial_start", "Primer inicio", FormKind::DateTime)
                .help("Desde cuándo se cuentan los inicios de un horario fijo. Vacío: ahora."),
        );
    }
    if caps.timezone {
        schedule = schedule.field(
            FormField::new("timezone", "Zona horaria", FormKind::Text)
                .placeholder("America/Argentina/Buenos_Aires")
                .help("La zona en la que se calculan los inicios de un horario fijo, cambios de hora incluidos. Vacía: UTC."),
        );
    }
    if existing {
        schedule = schedule.field(
            FormField::new("next_start", "Próximo inicio", FormKind::DateTime)
                .help("Cambiarlo mueve la próxima ejecución."),
        );
    }
    let execution = FormSection::new("execution", "Ejecución").fields(vec![
        FormField::new("max_runtime", "Duración máxima", FormKind::Text)
            .placeholder("1 hour")
            .help("Un intervalo; 0: sin límite. Vacío: el de TimescaleDB."),
        FormField::new(
            "max_retries",
            "Reintentos",
            FormKind::number(Some(-1.0), None),
        )
        .help("-1: sin límite. Vacío: el de TimescaleDB."),
        FormField::new("retry_period", "Espera entre reintentos", FormKind::Text)
            .placeholder("5 minutes")
            .help("Un intervalo. Vacío: el de TimescaleDB."),
    ]);
    vec![schedule, execution]
}

fn put_schedule(values: &mut BTreeMap<String, Value>, j: &Job) {
    values.insert("schedule_interval".into(), json!(j.every));
    values.insert(
        "fixed_schedule".into(),
        json!(j.fixed_schedule.unwrap_or(false)),
    );
    values.insert(
        "initial_start".into(),
        json!(j.initial_start.clone().unwrap_or_default()),
    );
    values.insert(
        "timezone".into(),
        json!(j.timezone.clone().unwrap_or_default()),
    );
    values.insert(
        "next_start".into(),
        json!(j.next_start.clone().unwrap_or_default()),
    );
    // No field shows it: the next start as the form was read, so a save
    // moves it only when the user changed it (it moves on every run).
    values.insert(
        "next_start_was".into(),
        json!(j.next_start.clone().unwrap_or_default()),
    );
    values.insert("max_runtime".into(), json!(j.max_runtime));
    values.insert("max_retries".into(), json!(j.max_retries));
    values.insert("retry_period".into(), json!(j.retry_period));
    values.insert("scheduled".into(), json!(j.scheduled));
}

/// A user-defined job's form (new: `None`).
fn job_sections(caps: &Caps, j: Option<&Job>) -> Vec<FormSection> {
    let existing = j.is_some();
    let system = j.is_some_and(|j| j.system());
    let mut general = FormSection::new("general", "General");
    if existing {
        general = general.field(FormField::new("jobid", "Id", FormKind::Text).read_only(true));
    }
    let name_free = if existing {
        caps.alter_job_name && !system
    } else {
        caps.job_name
    };
    let name = FormField::new("name", "Nombre", FormKind::Text).read_only(!name_free);
    general = general.fields(vec![
        if name_free {
            name.help("TimescaleDB le agrega el id al nombre: «nombre [1000]».")
        } else {
            name.help("Esta versión de TimescaleDB pone el nombre por su cuenta.")
        },
        FormField::new(
            "proc",
            "Procedimiento",
            FormKind::reference("procedure", false),
        )
        .required()
        .read_only(system)
        .help("Un procedimiento o una función (job_id integer, config jsonb)."),
        FormField::new("scheduled", "Habilitado", FormKind::Bool).default(json!(true)),
    ]);
    if existing {
        general =
            general.field(FormField::new("owner", "Propietario", FormKind::Text).read_only(true));
    }
    let mut out = vec![general];
    out.extend(schedule_sections(caps, existing, None));
    let mut config = FormSection::new("config", "Configuración").field(
        FormField::new("config", "Configuración", FormKind::code("json"))
            .help("El JSON que recibe el procedimiento (config jsonb)."),
    );
    if caps.check_config {
        config = config.field(
            FormField::new("check_config", "Función de verificación", FormKind::reference("check_function", false))
                .read_only(system)
                .help("Una función (config jsonb) que valida la configuración cada vez que cambia. Vacía: ninguna."),
        );
    }
    out.push(config);
    out
}

/// A new user-defined job's form, without the server's choices.
pub(crate) fn blank_job_form(caps: &Caps) -> ObjectForm {
    let mut f = ObjectForm {
        sections: job_sections(caps, None),
        ..ObjectForm::default()
    };
    f.values.insert("scheduled".into(), json!(true));
    if caps.fixed_schedule {
        f.values.insert("fixed_schedule".into(), json!(true));
    }
    f
}

pub(crate) async fn new_job_form(s: &PgSession, c: &Ctx) -> Result<ObjectForm> {
    let caps = caps_of(c)?;
    let mut f = blank_job_form(caps);
    f.choices.insert("procedure".into(), procedures(s).await?);
    f.choices
        .insert("check_function".into(), check_functions(s).await?);
    Ok(f)
}

/// The form of an existing job: a policy's, or a user-defined job's.
pub(crate) async fn job_form(s: &PgSession, c: &Ctx, n: i32) -> Result<ObjectForm> {
    let j = read(s, n).await?;
    if let Some(p) = j.policy() {
        return policy_form_of(s, c, p, Some(j)).await;
    }
    let caps = caps_of(c)?;
    let mut f = ObjectForm {
        sections: job_sections(caps, Some(&j)),
        ..ObjectForm::default()
    };
    put_schedule(&mut f.values, &j);
    f.values.insert("jobid".into(), json!(j.id.to_string()));
    f.values.insert("name".into(), json!(j.name));
    f.values.insert("proc".into(), json!(j.proc()));
    f.values.insert("owner".into(), json!(j.owner));
    f.values.insert(
        "config".into(),
        json!(j.config.as_deref().map(pretty).unwrap_or_default()),
    );
    f.values.insert(
        "check_config".into(),
        json!(j.check.clone().unwrap_or_default()),
    );
    let mut procs = procedures(s).await?;
    if !procs.iter().any(|(v, _)| *v == j.proc()) {
        procs.push((j.proc(), format!("{}.{}", j.proc_schema, j.proc_name)));
    }
    f.choices.insert("procedure".into(), procs);
    let mut checks = check_functions(s).await?;
    if let Some(ch) = &j.check {
        checks = with_current(checks, ch);
    }
    f.choices.insert("check_function".into(), checks);
    if j.system() {
        f.notes.push(SYSTEM_JOB.into());
    } else {
        f.notes.push(NEW_PROC.into());
    }
    if !(j.mine || caps.superuser) {
        f.read_only = true;
        f.notes.insert(0, NOT_OWNER.into());
    }
    Ok(f)
}

fn pretty(json_text: &str) -> String {
    serde_json::from_str::<Value>(json_text)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| json_text.to_string())
}

/// A policy's config value as the form shows it.
fn config_text(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => json!(""),
        Some(Value::String(s)) => json!(s),
        Some(Value::Bool(b)) => json!(*b),
        Some(Value::Number(n)) => json!(n),
        Some(other) => json!(other.to_string()),
    }
}

pub(crate) async fn policy_form(
    s: &PgSession,
    c: &Ctx,
    p: Policy,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let j = match id {
        Some(id) => {
            let j = read(s, policy_id(id)?).await?;
            if j.policy() != Some(p) {
                return Err(bad(format!(
                    "El job {} no es una {}.",
                    JobId::Timescale(j.id),
                    p.title()
                )));
            }
            Some(j)
        }
        None => None,
    };
    policy_form_of(s, c, p, j).await
}

async fn policy_form_of(s: &PgSession, c: &Ctx, p: Policy, j: Option<Job>) -> Result<ObjectForm> {
    let caps = caps_of(c)?;
    let existing = j.is_some();
    let mut general = FormSection::new("general", "General");
    if existing {
        general = general.field(FormField::new("jobid", "Id", FormKind::Text).read_only(true));
    }
    let target_label = match p {
        Policy::Retention => "Hipertabla o agregado continuo",
        Policy::Refresh => "Agregado continuo",
        _ => "Hipertabla",
    };
    let name = FormField::new("name", "Nombre", FormKind::Text).read_only(!caps.alter_job_name);
    general = general.fields(vec![
        FormField::new(
            "target",
            target_label,
            FormKind::reference(p.target_source(), false),
        )
        .required(),
        if caps.alter_job_name {
            name.help(
                "Vacío: el que le pone TimescaleDB. TimescaleDB le agrega el id: «nombre [1000]».",
            )
        } else {
            name.help("Esta versión de TimescaleDB pone el nombre por su cuenta.")
        },
        FormField::new("scheduled", "Habilitada", FormKind::Bool).default(json!(true)),
    ]);
    if existing {
        general =
            general.field(FormField::new("owner", "Propietario", FormKind::Text).read_only(true));
    }
    let takes = |arg: &str| caps.takes(p, arg);
    let policy = FormSection::new("policy", "Política").fields(param_fields(p, &takes));
    let mut sections = vec![general, policy];
    sections.extend(schedule_sections(caps, existing, Some(p)));
    let mut f = ObjectForm {
        sections,
        ..ObjectForm::default()
    };
    let mut choices = targets(s, p).await?;
    match &j {
        None => {
            f.values.insert("scheduled".into(), json!(true));
            f.values.insert("fixed_schedule".into(), json!(false));
        }
        Some(j) => {
            put_schedule(&mut f.values, j);
            f.values.insert("jobid".into(), json!(j.id.to_string()));
            f.values.insert("name".into(), json!(j.name));
            f.values.insert("owner".into(), json!(j.owner));
            let target = j.target.clone().unwrap_or_default();
            choices = with_current(choices, &target);
            f.values.insert("target".into(), json!(target));
            let cfg = j.config_map();
            for (key, param) in params(p) {
                let v = cfg.get(*key);
                let shown = match param {
                    Param::Tri => json!(match v {
                        Some(Value::Bool(true)) => "true",
                        Some(Value::Bool(false)) => "false",
                        _ => "",
                    }),
                    _ => config_text(v),
                };
                f.values.insert((*key).to_string(), shown);
            }
            f.notes.push(NEW_TARGET.into());
            if !(j.mine || caps.superuser) {
                f.read_only = true;
                f.notes.insert(0, NOT_OWNER.into());
            }
        }
    }
    f.choices.insert(p.target_source().into(), choices);
    Ok(f)
}

// ---------------------------------------------------------------------------
// What the form asks for
// ---------------------------------------------------------------------------

/// An interval as the server writes it back, checked by the server.
async fn interval(s: &PgSession, label: &str, t: &str) -> Result<String> {
    plain(t, label)?;
    match s
        .text(&format!(
            "SELECT {}::interval::text AS i",
            lit(s.variant, t)
        ))
        .await
    {
        Ok(rows) => Ok(rows
            .first()
            .and_then(|r| text(r, "i"))
            .unwrap_or_else(|| t.to_string())),
        Err(_) => Err(bad(format!(
            "«{label}»: «{}» no es un intervalo válido (por ejemplo 30 minutes, 1 hour, 7 days).",
            shown(t)
        ))),
    }
}

async fn opt_interval(s: &PgSession, v: Vals<'_>, k: &str, label: &str) -> Result<Option<String>> {
    match v.opt(k) {
        None => Ok(None),
        Some(t) => interval(s, label, &t).await.map(Some),
    }
}

/// A time zone the server knows.
async fn timezone(s: &PgSession, t: &str) -> Result<String> {
    plain(t, "Zona horaria")?;
    let known = !s
        .text(&format!(
            "SELECT 1 FROM pg_catalog.pg_timezone_names WHERE name = {} LIMIT 1",
            lit(s.variant, t)
        ))
        .await?
        .is_empty();
    if known {
        Ok(t.to_string())
    } else {
        Err(bad(format!(
            "«Zona horaria»: el servidor no conoce «{}» (por ejemplo America/Argentina/Buenos_Aires).",
            shown(t)
        )))
    }
}

/// The schedule and execution settings, checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sched {
    pub(crate) every: Option<String>,
    pub(crate) fixed_schedule: bool,
    pub(crate) initial_start: Option<String>,
    pub(crate) timezone: Option<String>,
    pub(crate) next_start: Option<String>,
    /// The next start as the form was read.
    pub(crate) next_start_was: Option<String>,
    pub(crate) max_runtime: Option<String>,
    pub(crate) max_retries: Option<i64>,
    pub(crate) retry_period: Option<String>,
    pub(crate) scheduled: bool,
    pub(crate) name: Option<String>,
}

async fn sched(
    s: &PgSession,
    caps: &Caps,
    values: &FormValues,
    required_every: bool,
) -> Result<Sched> {
    let v = Vals(&values.values);
    let every = opt_interval(s, v, "schedule_interval", "Intervalo").await?;
    if every.is_none() && required_every {
        return Err(bad("«Intervalo» no puede quedar vacío.".into()));
    }
    let tz = match v.opt("timezone") {
        Some(t) if caps.timezone => Some(timezone(s, &t).await?),
        _ => None,
    };
    let name = match v.opt("name") {
        Some(_) => Some(name_of(v, "name", "Nombre")?),
        None => None,
    };
    Ok(Sched {
        every,
        fixed_schedule: v.bool("fixed_schedule"),
        initial_start: datetime_field(v, "initial_start", "Primer inicio")?,
        timezone: tz,
        next_start: datetime_field(v, "next_start", "Próximo inicio")?,
        next_start_was: v.opt("next_start_was"),
        max_runtime: opt_interval(s, v, "max_runtime", "Duración máxima").await?,
        max_retries: v.int("max_retries", "Reintentos", -1, i64::from(i32::MAX))?,
        retry_period: opt_interval(s, v, "retry_period", "Espera entre reintentos").await?,
        scheduled: v.bool_or("scheduled", true),
        name,
    })
}

/// `alter_job` arguments for what changed in `n` (`cur` its settings;
/// `None`: a new job, every setting given). The time zone always goes when
/// the job keeps one (see the module's notes).
fn alter_args(
    v: Variant,
    caps: &Caps,
    cur: Option<&Job>,
    sc: &Sched,
    allow_name: bool,
) -> Vec<String> {
    let mut a = Vec::new();
    let differs = |new: &Option<String>, old: Option<&str>| match new {
        Some(n) => Some(n.as_str()) != old,
        None => false,
    };
    if let Some(cur) = cur {
        if differs(&sc.every, Some(&cur.every)) {
            a.push(format!(
                "schedule_interval => INTERVAL {}",
                lit(v, sc.every.as_deref().unwrap_or_default())
            ));
        }
        if differs(&sc.max_runtime, Some(&cur.max_runtime)) {
            a.push(format!(
                "max_runtime => INTERVAL {}",
                lit(v, sc.max_runtime.as_deref().unwrap_or_default())
            ));
        }
        if let Some(r) = sc.max_retries.filter(|r| *r != cur.max_retries) {
            a.push(format!("max_retries => {r}"));
        }
        if differs(&sc.retry_period, Some(&cur.retry_period)) {
            a.push(format!(
                "retry_period => INTERVAL {}",
                lit(v, sc.retry_period.as_deref().unwrap_or_default())
            ));
        }
        if sc.scheduled != cur.scheduled {
            a.push(format!("scheduled => {}", sc.scheduled));
        }
        if caps.fixed_schedule && cur.fixed_schedule.is_some_and(|f| f != sc.fixed_schedule) {
            a.push(format!("fixed_schedule => {}", sc.fixed_schedule));
        }
        if caps.alter_initial_start && differs(&sc.initial_start, cur.initial_start.as_deref()) {
            a.push(format!(
                "initial_start => {}",
                ts_lit(v, sc.initial_start.as_deref().unwrap_or_default())
            ));
        }
        if sc.next_start.is_some() && sc.next_start != sc.next_start_was {
            a.push(format!(
                "next_start => {}",
                ts_lit(v, sc.next_start.as_deref().unwrap_or_default())
            ));
        }
        if allow_name && caps.alter_job_name {
            if let Some(name) = sc.name.as_ref().filter(|n| **n != cur.name) {
                a.push(format!("job_name => {}", lit(v, name)));
            }
        }
    } else {
        if let Some(m) = &sc.max_runtime {
            a.push(format!("max_runtime => INTERVAL {}", lit(v, m)));
        }
        if let Some(r) = sc.max_retries {
            a.push(format!("max_retries => {r}"));
        }
        if let Some(p) = &sc.retry_period {
            a.push(format!("retry_period => INTERVAL {}", lit(v, p)));
        }
        if !sc.scheduled {
            a.push("scheduled => false".into());
        }
    }
    a
}

/// The time zone argument every `alter_job` carries: the new one, or the
/// one the job keeps (an `alter_job` without it drops it).
fn tz_arg(v: Variant, caps: &Caps, cur: Option<&str>, new: Option<&str>) -> Option<String> {
    if !caps.timezone {
        return None;
    }
    new.or(cur).map(|t| format!("timezone => {}", lit(v, t)))
}

/// `SELECT job_id FROM alter_job(<call>, …)` or `SELECT <call> AS job_id`.
fn wrapped(caps: &Caps, call: String, extra: Vec<String>) -> String {
    if extra.is_empty() {
        format!("SELECT {call} AS job_id")
    } else {
        format!(
            "SELECT job_id FROM {}({call}, {})",
            caps.f("alter_job"),
            extra.join(", ")
        )
    }
}

/// A user-defined job as the form asks for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobSpec {
    pub(crate) proc: String,
    pub(crate) config: Option<Value>,
    pub(crate) check: Option<String>,
    pub(crate) sched: Sched,
}

fn config_of(v: Vals) -> Result<Option<Value>> {
    let raw = v.raw("config");
    if raw.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<Value>(&raw)
        .map(Some)
        .map_err(|e| bad(format!("«Configuración» no es un JSON válido: {e}.")))
}

pub(crate) fn add_job_script(v: Variant, caps: &Caps, j: &JobSpec) -> String {
    let mut args = vec![
        format!("proc => {}::regproc", lit(v, &j.proc)),
        format!(
            "schedule_interval => INTERVAL {}",
            lit(v, j.sched.every.as_deref().unwrap_or_default())
        ),
    ];
    if let Some(c) = &j.config {
        args.push(format!("config => {}::jsonb", lit(v, &c.to_string())));
    }
    if let Some(t) = &j.sched.initial_start {
        args.push(format!("initial_start => {}", ts_lit(v, t)));
    }
    if caps.check_config {
        if let Some(ch) = &j.check {
            args.push(format!("check_config => {}::regproc", lit(v, ch)));
        }
    }
    if caps.fixed_schedule {
        args.push(format!("fixed_schedule => {}", j.sched.fixed_schedule));
    }
    if caps.timezone {
        if let Some(tz) = &j.sched.timezone {
            args.push(format!("timezone => {}", lit(v, tz)));
        }
    }
    if caps.job_name {
        if let Some(n) = &j.sched.name {
            args.push(format!("job_name => {}", lit(v, n)));
        }
    }
    let call = format!("{}({})", caps.f("add_job"), args.join(", "));
    let mut extra = alter_args(v, caps, None, &j.sched, false);
    if !extra.is_empty() {
        extra.extend(tz_arg(v, caps, None, j.sched.timezone.as_deref()));
    }
    wrapped(caps, call, extra)
}

pub(crate) fn job_create(v: Variant, caps: &Caps, j: &JobSpec) -> String {
    transaction(
        &format!(
            "TimescaleDB: nuevo job «{}»",
            j.sched.name.as_deref().unwrap_or(&j.proc)
        ),
        &[add_job_script(v, caps, j)],
    )
}

/// The change of a user-defined job: `alter_job`, or delete and add when
/// the procedure changes. `None`: nothing changed.
pub(crate) fn job_alter(v: Variant, caps: &Caps, cur: &Job, j: &JobSpec) -> Option<String> {
    if j.proc != cur.proc() && !cur.system() {
        return Some(transaction(
            &format!("TimescaleDB: job «{}»\n{NEW_PROC}", cur.name),
            &[
                format!("SELECT {}({})", caps.f("delete_job"), cur.id),
                add_job_script(v, caps, j),
            ],
        ));
    }
    let mut a = alter_args(v, caps, Some(cur), &j.sched, !cur.system());
    let cur_cfg: Option<Value> = cur
        .config
        .as_deref()
        .and_then(|c| serde_json::from_str(c).ok());
    if j.config != cur_cfg {
        a.push(match &j.config {
            Some(c) => format!("config => {}::jsonb", lit(v, &c.to_string())),
            // alter_job reads NULL as "unchanged": JSON null clears it.
            None => "config => 'null'::jsonb".into(),
        });
    }
    if caps.check_config && !cur.system() && j.check != cur.check {
        a.push(match &j.check {
            Some(ch) => format!("check_config => {}::regproc", lit(v, ch)),
            None => "check_config => 0".into(),
        });
    }
    let tz_changed = caps.timezone && j.sched.timezone != cur.timezone;
    if a.is_empty() && !tz_changed {
        return None;
    }
    a.extend(tz_arg(
        v,
        caps,
        if tz_changed {
            None
        } else {
            cur.timezone.as_deref()
        },
        j.sched.timezone.as_deref(),
    ));
    Some(transaction(
        &format!("TimescaleDB: job «{}»", cur.name),
        &[format!(
            "SELECT job_id FROM {}({}, {})",
            caps.f("alter_job"),
            cur.id,
            a.join(", ")
        )],
    ))
}

pub(crate) async fn job_plan(
    s: &PgSession,
    c: &Ctx,
    n: Option<i32>,
    values: &FormValues,
) -> Result<Plan> {
    let caps = caps_of(c)?;
    let cur = match n {
        Some(n) => Some(read(s, n).await?),
        None => None,
    };
    if let Some(j) = &cur {
        if let Some(p) = j.policy() {
            return policy_plan_of(s, c, p, Some(j.clone()), values).await;
        }
        if !(j.mine || caps.superuser) {
            return Err(bad(NOT_OWNER.into()));
        }
    }
    let v = Vals(&values.values);
    let procs = procedures(s).await?;
    let proc = v.opt("proc").unwrap_or_default();
    let proc = match &cur {
        Some(j) if proc == j.proc() || j.system() => j.proc(),
        _ => pick(&proc, &procs, |p| {
            format!("«{p}» no es un procedimiento (job_id integer, config jsonb) de esta base.")
        })?
        .to_string(),
    };
    let check = match v.opt("check_config") {
        None => None,
        Some(ch) if cur.as_ref().and_then(|j| j.check.as_ref()) == Some(&ch) => Some(ch),
        Some(ch) => Some(
            pick(&ch, &check_functions(s).await?, |f| {
                format!("«{f}» no es una función (config jsonb) de esta base.")
            })?
            .to_string(),
        ),
    };
    let spec = JobSpec {
        proc,
        config: config_of(v)?,
        check,
        sched: sched(s, caps, values, true).await?,
    };
    let sv = s.variant;
    Ok(match &cur {
        None => Plan {
            script: job_create(sv, caps, &spec),
            empty: false,
            id: None,
            new_id: Some(NewId::Column("job_id", "timescaledb:")),
        },
        Some(j) => match job_alter(sv, caps, j, &spec) {
            None => Plan::nothing(&j.name, JobId::Timescale(j.id).to_string()),
            Some(script) => Plan {
                script,
                empty: false,
                id: Some(JobId::Timescale(j.id).to_string()),
                new_id: (spec.proc != j.proc()).then_some(NewId::Column("job_id", "timescaledb:")),
            },
        },
    })
}

// ---------------------------------------------------------------------------
// Policies: scripts
// ---------------------------------------------------------------------------

/// A policy as the form asks for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PolicySpec {
    /// The hypertable or continuous aggregate, quoted.
    pub(crate) target: String,
    /// Each parameter given: its config key and value.
    pub(crate) params: Vec<(String, Value)>,
    pub(crate) sched: Sched,
}

/// A parameter as an `add_*_policy` argument.
fn param_arg(v: Variant, value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("INTERVAL {}", lit(v, s)),
        other => format!("{}::jsonb", lit(v, &other.to_string())),
    }
}

pub(crate) fn add_policy_script(v: Variant, caps: &Caps, p: Policy, ps: &PolicySpec) -> String {
    let mut args = vec![lit(v, &ps.target)];
    for (key, value) in &ps.params {
        // A refresh policy's offsets have no default: an open end is NULL.
        let offset = p == Policy::Refresh && matches!(key.as_str(), "start_offset" | "end_offset");
        if value.is_null() && !offset {
            continue;
        }
        let arg = if *key == "index_name" {
            lit(v, value.as_str().unwrap_or_default())
        } else {
            param_arg(v, value)
        };
        args.push(format!("{key} => {arg}"));
    }
    if p.takes_interval() {
        if let Some(e) = &ps.sched.every {
            args.push(format!("schedule_interval => INTERVAL {}", lit(v, e)));
        }
    }
    if caps.takes(p, "initial_start") {
        if let Some(t) = &ps.sched.initial_start {
            args.push(format!("initial_start => {}", ts_lit(v, t)));
        }
    }
    if caps.takes(p, "timezone") {
        if let Some(tz) = &ps.sched.timezone {
            args.push(format!("timezone => {}", lit(v, tz)));
        }
    }
    let call = format!("{}({})", caps.f(p.add_fn()), args.join(", "));
    let mut extra = alter_args(v, caps, None, &ps.sched, false);
    // A reorder policy has no interval of its own argument.
    if !p.takes_interval() {
        if let Some(e) = &ps.sched.every {
            extra.push(format!("schedule_interval => INTERVAL {}", lit(v, e)));
        }
    }
    if caps.fixed_schedule && ps.sched.fixed_schedule {
        extra.push("fixed_schedule => true".into());
    }
    if caps.alter_job_name {
        if let Some(n) = &ps.sched.name {
            extra.push(format!("job_name => {}", lit(v, n)));
        }
    }
    if !extra.is_empty() {
        extra.extend(tz_arg(v, caps, None, ps.sched.timezone.as_deref()));
    }
    wrapped(caps, call, extra)
}

pub(crate) fn policy_create(v: Variant, caps: &Caps, p: Policy, ps: &PolicySpec) -> String {
    transaction(
        &format!(
            "TimescaleDB: nueva {} de {}",
            p.title(),
            unquote(&ps.target)
        ),
        &[add_policy_script(v, caps, p, ps)],
    )
}

fn remove_call(caps: &Caps, v: Variant, p: Policy, target: &str) -> String {
    format!("SELECT {}({})", caps.f(p.remove_fn()), lit(v, target))
}

/// The change of a policy, or `None` when nothing changed.
pub(crate) fn policy_alter(
    v: Variant,
    caps: &Caps,
    p: Policy,
    cur: &Job,
    ps: &PolicySpec,
) -> Option<String> {
    let target = cur.target.clone().unwrap_or_default();
    if ps.target != target {
        return Some(transaction(
            &format!(
                "TimescaleDB: {} de {} pasa a {}\n{NEW_TARGET}",
                p.title(),
                unquote(&target),
                unquote(&ps.target)
            ),
            &[
                remove_call(caps, v, p, &target),
                add_policy_script(v, caps, p, ps),
            ],
        ));
    }
    let mut a = alter_args(v, caps, Some(cur), &ps.sched, true);
    let mut cfg = cur.config_map();
    let before = cfg.clone();
    for (key, param) in params(p) {
        let new = ps
            .params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone());
        match (param, new) {
            (_, Some(Value::Null)) | (_, None) => {
                // Offsets stay, as null: the window's open end.
                if *param == Param::Offset && cfg.contains_key(*key) {
                    cfg.insert((*key).to_string(), Value::Null);
                } else {
                    cfg.remove(*key);
                }
            }
            (_, Some(value)) => {
                cfg.insert((*key).to_string(), value);
            }
        }
    }
    if cfg != before {
        a.push(format!(
            "config => {}::jsonb",
            lit(v, &Value::Object(cfg).to_string())
        ));
    }
    let tz_changed = caps.timezone && ps.sched.timezone != cur.timezone;
    if a.is_empty() && !tz_changed {
        return None;
    }
    a.extend(tz_arg(
        v,
        caps,
        if tz_changed {
            None
        } else {
            cur.timezone.as_deref()
        },
        ps.sched.timezone.as_deref(),
    ));
    Some(transaction(
        &format!("TimescaleDB: {} de {}", p.title(), unquote(&target)),
        &[format!(
            "SELECT job_id FROM {}({}, {})",
            caps.f("alter_job"),
            cur.id,
            a.join(", ")
        )],
    ))
}

/// An "after" value: a whole number (integer time) or an interval.
async fn after_value(s: &PgSession, label: &str, t: &str) -> Result<Value> {
    if let Ok(n) = t.trim().parse::<i64>() {
        return Ok(json!(n));
    }
    Ok(json!(interval(s, label, t).await?))
}

async fn policy_spec(
    s: &PgSession,
    caps: &Caps,
    p: Policy,
    values: &FormValues,
    cur: Option<&Job>,
) -> Result<PolicySpec> {
    let v = Vals(&values.values);
    let target = v.opt("target").unwrap_or_default();
    let target = match cur.and_then(|j| j.target.as_ref()) {
        Some(t) if *t == target => target,
        _ => pick(&target, &targets(s, p).await?, |t| match p {
            Policy::Refresh => format!("«{t}» no es un agregado continuo de esta base."),
            Policy::Retention => {
                format!("«{t}» no es una hipertabla ni un agregado continuo de esta base.")
            }
            Policy::Compression | Policy::Compaction => {
                format!("«{t}» no es una hipertabla con compresión de esta base.")
            }
            Policy::Reorder => format!("«{t}» no es una hipertabla de esta base."),
        })?
        .to_string(),
    };
    let mut params_out = Vec::new();
    for (key, param) in params(p) {
        if !caps.takes(p, key) && cur.is_none_or(|j| !j.config_map().contains_key(*key)) {
            continue;
        }
        let label = match *key {
            "compress_after" => "Comprimir después de",
            "compress_created_before" => "Comprimir lo creado antes de",
            "drop_after" => "Borrar después de",
            "drop_created_before" => "Borrar lo creado antes de",
            "start_offset" => "Desde (antigüedad)",
            "end_offset" => "Hasta (antigüedad)",
            "inactive_for" => "Inactivos durante",
            "index_name" => "Índice",
            "buckets_per_batch" => "Buckets por lote",
            "max_batches_per_execution" => "Lotes por ejecución",
            "max_chunks" => "Máximo de chunks",
            "max_batches" => "Máximo de lotes",
            "refresh_newest_first" => "Actualizar primero lo más nuevo",
            "include_tiered_data" => "Incluir los datos en almacenamiento por niveles",
            _ => key,
        };
        let value = match param {
            Param::After | Param::Offset => match v.opt(key) {
                None => Value::Null,
                Some(t) => after_value(s, label, &t).await?,
            },
            Param::Interval => match v.opt(key) {
                None => Value::Null,
                Some(t) => json!(interval(s, label, &t).await?),
            },
            Param::Int => match v.int(key, label, 0, i64::from(i32::MAX))? {
                None => Value::Null,
                Some(n) => json!(n),
            },
            Param::Tri => match v.opt(key).as_deref() {
                Some("true") => json!(true),
                Some("false") => json!(false),
                _ => Value::Null,
            },
            Param::Index => {
                let ix = name_of(v, key, label)?;
                json!(ix)
            }
        };
        params_out.push(((*key).to_string(), value));
    }
    // The "after" and "created before" pairs: exactly one.
    let given = |k: &str| params_out.iter().any(|(x, v)| x == k && !v.is_null());
    match p {
        Policy::Compression if given("compress_after") == given("compress_created_before") => {
            return Err(bad(
                "Una política de compresión necesita «Comprimir después de» o «Comprimir lo creado antes de», una de las dos.".into(),
            ))
        }
        Policy::Retention if given("drop_after") == given("drop_created_before") => {
            return Err(bad(
                "Una política de retención necesita «Borrar después de» o «Borrar lo creado antes de», una de las dos.".into(),
            ))
        }
        Policy::Reorder => {
            let ix = params_out
                .iter()
                .find(|(k, _)| k == "index_name")
                .and_then(|(_, v)| v.as_str())
                .unwrap_or_default()
                .to_string();
            let found = !s
                .text(&format!(
                    "SELECT 1 FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
                     WHERE i.indrelid = {}::regclass AND c.relname = {} LIMIT 1",
                    lit(s.variant, &target),
                    lit(s.variant, &ix)
                ))
                .await?
                .is_empty();
            if !found {
                return Err(bad(format!(
                    "La hipertabla {} no tiene un índice «{}».",
                    unquote(&target),
                    shown(&ix)
                )));
            }
        }
        _ => {}
    }
    Ok(PolicySpec {
        target,
        params: params_out,
        // A refresh policy's interval has no default.
        sched: sched(s, caps, values, cur.is_some() || !default_interval(Some(p))).await?,
    })
}

pub(crate) async fn policy_plan(
    s: &PgSession,
    c: &Ctx,
    p: Policy,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let cur = match id {
        Some(id) => {
            let j = read(s, policy_id(id)?).await?;
            if j.policy() != Some(p) {
                return Err(bad(format!(
                    "El job {} no es una {}.",
                    JobId::Timescale(j.id),
                    p.title()
                )));
            }
            Some(j)
        }
        None => None,
    };
    policy_plan_of(s, c, p, cur, values).await
}

async fn policy_plan_of(
    s: &PgSession,
    c: &Ctx,
    p: Policy,
    cur: Option<Job>,
    values: &FormValues,
) -> Result<Plan> {
    let caps = caps_of(c)?;
    if let Some(j) = &cur {
        if !(j.mine || caps.superuser) {
            return Err(bad(NOT_OWNER.into()));
        }
    }
    let ps = policy_spec(s, caps, p, values, cur.as_ref()).await?;
    let v = s.variant;
    Ok(match &cur {
        None => Plan {
            script: policy_create(v, caps, p, &ps),
            empty: false,
            id: None,
            new_id: Some(NewId::Column("job_id", "timescaledb:")),
        },
        Some(j) => match policy_alter(v, caps, p, j, &ps) {
            None => Plan::nothing(&j.name, JobId::Timescale(j.id).to_string()),
            Some(script) => Plan {
                script,
                empty: false,
                id: Some(JobId::Timescale(j.id).to_string()),
                new_id: (Some(&ps.target) != j.target.as_ref())
                    .then_some(NewId::Column("job_id", "timescaledb:")),
            },
        },
    })
}

pub(crate) async fn delete_script(
    s: &PgSession,
    c: &Ctx,
    kind: Option<Policy>,
    n: i32,
) -> Result<String> {
    let caps = caps_of(c)?;
    let j = read(s, n).await?;
    if j.system() {
        return Err(bad(SYSTEM_JOB.into()));
    }
    if !(j.mine || caps.superuser) {
        return Err(bad(NOT_OWNER.into()));
    }
    let v = s.variant;
    if let Some(k) = kind.filter(|k| j.policy() != Some(*k)) {
        return Err(bad(format!(
            "El job {} no es una {}.",
            JobId::Timescale(n),
            k.title()
        )));
    }
    Ok(match j.policy() {
        Some(p) => {
            let target = j.target.clone().unwrap_or_default();
            format!(
                "-- TimescaleDB: quitar la {} de {}\n{};\n",
                p.title(),
                comment_text(&unquote(&target)),
                remove_call(caps, v, p, &target)
            )
        }
        None => format!(
            "-- TimescaleDB: borrar el job «{}»\nSELECT {}({n});\n",
            comment_text(&j.name),
            caps.f("delete_job")
        ),
    })
}

pub(crate) async fn clear_script(s: &PgSession, c: &Ctx, n: i32) -> Result<String> {
    let caps = caps_of(c)?;
    let Some(table) = &caps.history else {
        return Err(Error::Unsupported(
            "esta versión de TimescaleDB no guarda el historial de sus jobs".into(),
        ));
    };
    if !caps.clear {
        return Err(bad(format!(
            "Tu usuario no puede borrar el historial de TimescaleDB (DELETE en {table}): hace falta ser su dueño o superusuario."
        )));
    }
    let j = read(s, n).await?;
    Ok(format!(
        "-- TimescaleDB: borrar el historial del job «{}»\nDELETE FROM {table} WHERE job_id = {n};\n",
        comment_text(&j.name)
    ))
}

#[cfg(test)]
pub(crate) fn caps_for_tests() -> Caps {
    let all = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut args = BTreeMap::new();
    args.insert(
        "add_compression_policy".into(),
        all(&[
            "hypertable",
            "compress_after",
            "if_not_exists",
            "schedule_interval",
            "initial_start",
            "timezone",
            "compress_created_before",
        ]),
    );
    args.insert(
        "add_retention_policy".into(),
        all(&[
            "relation",
            "drop_after",
            "if_not_exists",
            "schedule_interval",
            "initial_start",
            "timezone",
            "drop_created_before",
        ]),
    );
    args.insert(
        "add_continuous_aggregate_policy".into(),
        all(&[
            "continuous_aggregate",
            "start_offset",
            "end_offset",
            "schedule_interval",
            "if_not_exists",
            "initial_start",
            "timezone",
            "include_tiered_data",
            "buckets_per_batch",
            "max_batches_per_execution",
            "refresh_newest_first",
        ]),
    );
    args.insert(
        "add_reorder_policy".into(),
        all(&[
            "hypertable",
            "index_name",
            "if_not_exists",
            "initial_start",
            "timezone",
        ]),
    );
    args.insert(
        "add_compaction_policy".into(),
        all(&[
            "hypertable",
            "if_not_exists",
            "schedule_interval",
            "initial_start",
            "timezone",
            "max_chunks",
            "max_batches",
            "inactive_for",
        ]),
    );
    Caps {
        schema: "public".into(),
        superuser: true,
        job_name: true,
        alter_job_name: true,
        timezone: true,
        fixed_schedule: true,
        check_config: true,
        alter_initial_start: true,
        compaction: true,
        args,
        history: Some("_timescaledb_internal.bgw_job_stat_history".into()),
        clear: true,
    }
}

#[cfg(test)]
pub(crate) fn schedule_sections_for_tests(
    caps: &Caps,
    existing: bool,
    policy: Option<Policy>,
) -> Vec<FormSection> {
    schedule_sections(caps, existing, policy)
}

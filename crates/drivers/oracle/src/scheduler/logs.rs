//! The Scheduler's logs, newest first: the job log (each operation on a
//! job and each run), the runs' details, the window log and the windows'
//! details, from `*_SCHEDULER_JOB_LOG`, `*_SCHEDULER_JOB_RUN_DETAILS`,
//! `*_SCHEDULER_WINDOW_LOG` and `*_SCHEDULER_WINDOW_DETAILS`.

use super::read::{self, Rec};
use super::Ctx;
use crate::jobs::QUERY_LIMIT;
use crate::processes::within;
use dbine_driver::{MonitorTable, Result};
use oracledb::Connection;
use serde_json::json;

/// Rows read from each log at most.
const MAX_ROWS: usize = 500;

fn when(r: &Rec, col: &str) -> String {
    super::timing::ts_of(r, col)
        .map(|t| format!("{} {}", t.local.replace('T', " "), t.tz))
        .unwrap_or_default()
}

fn qualified(r: &Rec, owner: &str, name: &str) -> String {
    match (r.t(owner), r.t(name)) {
        (Some(o), Some(n)) => format!("{o}.{n}"),
        (None, Some(n)) => n,
        _ => String::new(),
    }
}

fn duration(r: &Rec, col: &str) -> String {
    r.int(col)
        .map(|s| {
            let (d, h, m, s) = (s / 86400, s / 3600 % 24, s / 60 % 60, s % 60);
            if d > 0 {
                format!("{d} d {h:02}:{m:02}:{s:02}")
            } else {
                format!("{h:02}:{m:02}:{s:02}")
            }
        })
        .unwrap_or_default()
}

fn read_log(c: &Connection, ctx: &Ctx, view: &str) -> Vec<Rec> {
    match read::rows(
        c,
        &ctx.sql(view),
        "ORDER BY log_date DESC NULLS LAST, log_id DESC",
        &[],
        MAX_ROWS,
    ) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("oracle scheduler: {view}: {e}");
            Vec::new()
        }
    }
}

pub(crate) fn tables(
    jobs: &[Rec],
    runs: &[Rec],
    windows: &[Rec],
    window_runs: &[Rec],
) -> Vec<MonitorTable> {
    let mut job_log = MonitorTable::new(
        "job_log",
        "Registro de jobs",
        &[
            "Fecha",
            "Job",
            "Subnombre",
            "Operación",
            "Estado",
            "Usuario",
            "Información",
        ],
    );
    job_log.rows = jobs
        .iter()
        .map(|r| {
            vec![
                json!(when(r, "LOG_DATE")),
                json!(qualified(r, "OWNER", "JOB_NAME")),
                json!(r.t("JOB_SUBNAME").unwrap_or_default()),
                json!(r.t("OPERATION").unwrap_or_default()),
                json!(r.t("STATUS").unwrap_or_default()),
                json!(r.t("USER_NAME").unwrap_or_default()),
                json!(r.t("ADDITIONAL_INFO").unwrap_or_default()),
            ]
        })
        .collect();
    let mut run_log = MonitorTable::new(
        "job_runs",
        "Ejecuciones de jobs",
        &[
            "Fecha",
            "Job",
            "Subnombre",
            "Estado",
            "Error",
            "Inicio",
            "Duración",
            "Instancia",
            "Sesión",
            "Detalle",
        ],
    );
    run_log.rows = runs
        .iter()
        .map(|r| {
            let detail = [r.t("ERRORS"), r.t("ADDITIONAL_INFO"), r.t("OUTPUT")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n");
            vec![
                json!(when(r, "LOG_DATE")),
                json!(qualified(r, "OWNER", "JOB_NAME")),
                json!(r.t("JOB_SUBNAME").unwrap_or_default()),
                json!(r.t("STATUS").unwrap_or_default()),
                json!(r.t("ERROR#").unwrap_or_default()),
                json!(when(r, "ACTUAL_START_DATE")),
                json!(duration(r, "RUN_DURATION")),
                json!(r.t("INSTANCE_ID").unwrap_or_default()),
                json!(r.t("SESSION_ID").unwrap_or_default()),
                json!(detail),
            ]
        })
        .collect();
    let mut window_log = MonitorTable::new(
        "window_log",
        "Registro de ventanas",
        &[
            "Fecha",
            "Ventana",
            "Operación",
            "Estado",
            "Usuario",
            "Información",
        ],
    );
    window_log.rows = windows
        .iter()
        .map(|r| {
            vec![
                json!(when(r, "LOG_DATE")),
                json!(r.t("WINDOW_NAME").unwrap_or_default()),
                json!(r.t("OPERATION").unwrap_or_default()),
                json!(r.t("STATUS").unwrap_or_default()),
                json!(r.t("USER_NAME").unwrap_or_default()),
                json!(r.t("ADDITIONAL_INFO").unwrap_or_default()),
            ]
        })
        .collect();
    let mut window_runs_log = MonitorTable::new(
        "window_runs",
        "Aperturas de ventanas",
        &[
            "Fecha",
            "Ventana",
            "Apertura pedida",
            "Apertura",
            "Duración prevista",
            "Duración",
            "Información",
        ],
    );
    window_runs_log.rows = window_runs
        .iter()
        .map(|r| {
            vec![
                json!(when(r, "LOG_DATE")),
                json!(r.t("WINDOW_NAME").unwrap_or_default()),
                json!(when(r, "REQ_START_DATE")),
                json!(when(r, "ACTUAL_START_DATE")),
                json!(duration(r, "WINDOW_DURATION")),
                json!(duration(r, "ACTUAL_DURATION")),
                json!(r.t("ADDITIONAL_INFO").unwrap_or_default()),
            ]
        })
        .collect();
    vec![job_log, run_log, window_log, window_runs_log]
}

pub(crate) fn logs(c: &Connection) -> Result<Vec<MonitorTable>> {
    within(c, QUERY_LIMIT, || {
        let ctx = Ctx::read(c)?;
        Ok(tables(
            &read_log(c, &ctx, "{v}_scheduler_job_log"),
            &read_log(c, &ctx, "{v}_scheduler_job_run_details"),
            &read_log(c, &ctx, "{v}_scheduler_window_log"),
            &read_log(c, &ctx, "{v}_scheduler_window_details"),
        ))
    })
}

//! InfluxDB 3 processing engine triggers: what `influxdb3 create | enable |
//! disable | delete trigger` and `influxdb3 update trigger` do, through the
//! HTTP API.
//!
//! - A trigger runs a Python plugin (a file or folder in the server's
//!   plugin directory, or `gh:…` from InfluxData's repository) on a
//!   schedule (`every:<duration>`, `cron:<6-field cron>`), on the rows
//!   written to a table (`table:<name>`) or to any (`all_tables`), or on a
//!   request to `/api/v3/engine/<path>` (`request:<path>`), with arguments,
//!   an error behavior (log, retry, disable) and synchronous or
//!   asynchronous runs.
//! - New: the plugin's code, when given, goes first to
//!   `POST /api/v3/plugins/files` (as `--upload` does), then
//!   `POST /api/v3/configure/processing_engine_trigger`.
//! - In place: enabling or disabling (`…/enable`, `…/disable`) and the
//!   plugin's code (`PUT /api/v3/plugins/files`, `influxdb3 update
//!   trigger`). Anything else has no API: the trigger is deleted and
//!   created again (the new one first when its name or database changes;
//!   else after the delete, and the old one is created again when the
//!   server refuses the new one). That loses the plugin's in-memory cache;
//!   its log stays, under its name. InfluxDB doesn't report whether a
//!   trigger runs asynchronously, so a recreated one takes what the form
//!   says.
//! - The scheduler's log is the processing engine's
//!   (`system.processing_engine_logs`), kept in memory by the server.

use super::{bad, check_line, cron_ok, duration_ok, render, rows, shown, warning, Req, Vals};
use crate::http::{self, send_err};
use crate::jobs::{
    bad_id, pair_id, parse_pair, s as text_of, trigger_fires, trigger_spec, TRIGGERS, V3_NO_RUN,
    V3_OLD,
};
use crate::v3::SqlSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, MonitorTable, ObjectForm,
    Result, SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Map, Value as J};
use std::collections::BTreeMap;

/// Units `every:` takes (humantime's).
const UNITS: &[&str] = &[
    "nsec", "ns", "usec", "us", "µs", "msec", "ms", "seconds", "second", "sec", "s", "minutes",
    "minute", "min", "m", "hours", "hour", "hr", "h", "days", "day", "d", "weeks", "week", "w",
    "months", "month", "M", "years", "year", "y",
];

/// Rows of the processing engine's log read at most.
const LOG_ROWS: usize = 2000;

const RECREATE: &str = "InfluxDB 3 solo cambia en su lugar si un trigger está habilitado y el código de su plugin: cualquier otro cambio borra el trigger y lo vuelve a crear. Se pierde la caché del plugin (influxdb3_local.cache); su registro queda, con su nombre.";
const ASYNC_UNKNOWN: &str = "InfluxDB 3 no informa si un trigger existente se ejecuta en modo asíncrono: si se vuelve a crear, toma lo que diga «Ejecución asíncrona».";
const NO_CLEAR: &str = "InfluxDB 3 no permite borrar el registro del motor de procesamiento: lo guarda en memoria y se vacía al reiniciar el servidor.";

pub(crate) fn info() -> SchedulerInfo {
    SchedulerInfo {
        kinds: vec![SchedulerKind::new("job", "Triggers", "trigger").abilities(true, true, true)],
        logs: true,
        notes: Vec::new(),
    }
}

/// The databases a trigger may be in (the system's, `_…`, apart).
async fn databases(s: &SqlSession) -> Result<Vec<String>> {
    Ok(s.databases()
        .await?
        .into_iter()
        .filter(|d| !d.starts_with('_'))
        .collect())
}

pub(crate) async fn items(s: &SqlSession) -> Result<SchedulerList> {
    let mut items = Vec::new();
    let mut old = false;
    for db in databases(s).await? {
        let (cols, rows) = match s.jobs_sql(&db, TRIGGERS).await {
            Ok(t) => t,
            Err(Error::Query(m)) if m.contains("processing_engine") => {
                old = true;
                continue;
            }
            Err(e) => return Err(e),
        };
        for row in &rows {
            let t = row_of(&db, &cols, row, Vec::new());
            items.push(SchedulerItem {
                id: pair_id(&db, &t.name),
                description: Some(format!("{db} · {} · {}", t.plugin, trigger_fires(&t.spec))),
                name: t.name,
                enabled: Some(!t.disabled),
                system: false,
                can_edit: None,
                can_delete: None,
                actions: None,
                run_from_step: None,
                clear_history: None,
            });
        }
    }
    Ok(SchedulerList {
        items,
        notes: if old { vec![V3_OLD.into()] } else { Vec::new() },
    })
}

// ---------------------------------------------------------------------------
// The trigger as the server has it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Trigger {
    pub(crate) db: String,
    pub(crate) name: String,
    pub(crate) plugin: String,
    /// As `--trigger-spec` takes it (`every:5s`, `table:cpu`…).
    pub(crate) spec: String,
    pub(crate) disabled: bool,
    pub(crate) error_behavior: String,
    pub(crate) args: Vec<(String, String)>,
}

pub(crate) fn row_of(db: &str, cols: &[String], row: &[J], args: Vec<(String, String)>) -> Trigger {
    let at = |n: &str| cols.iter().position(|c| c == n).and_then(|i| row.get(i));
    Trigger {
        db: db.into(),
        name: text_of(at("trigger_name")).unwrap_or_default(),
        plugin: text_of(at("plugin_filename")).unwrap_or_default(),
        spec: trigger_spec(&text_of(at("trigger_specification")).unwrap_or_default()),
        disabled: matches!(at("disabled"), Some(J::Bool(true)))
            || text_of(at("disabled")).is_some_and(|d| d == "true"),
        error_behavior: text_of(at("error_behavior")).unwrap_or_else(|| "log".into()),
        args,
    }
}

async fn read(s: &SqlSession, id: &str) -> Result<Trigger> {
    parse_pair(id).ok_or_else(|| bad_id(id))?;
    let (db, cols, row) = s.trigger(id).await?;
    let name = text_of(
        cols.iter()
            .position(|c| c == "trigger_name")
            .and_then(|i| row.get(i)),
    )
    .unwrap_or_default();
    let args = s.trigger_args(&db, &name).await;
    Ok(row_of(&db, &cols, &row, args))
}

/// The trigger's plugin file on the server (`_internal`'s
/// `system.plugin_files`): path, size and when it changed.
async fn plugin_file(s: &SqlSession, t: &Trigger) -> Option<(String, Option<u64>, Option<String>)> {
    let q = format!(
        "SELECT file_name, file_path, size_bytes, last_modified FROM system.plugin_files WHERE plugin_name = '{}'",
        t.name.replace('\'', "''")
    );
    let (cols, rows) = s.jobs_sql("_internal", &q).await.ok()?;
    let at = |r: &[J], n: &str| {
        cols.iter()
            .position(|c| c == n)
            .and_then(|i| r.get(i).cloned())
    };
    let base = t.plugin.rsplit('/').next().unwrap_or(&t.plugin).to_string();
    let row = rows
        .iter()
        .find(|r| text_of(at(r, "file_name").as_ref()).as_deref() == Some(base.as_str()))
        .or(rows.first())?;
    let path = text_of(at(row, "file_path").as_ref())?;
    let size = at(row, "size_bytes").and_then(|v| v.as_u64());
    let modified = at(row, "last_modified")
        .and_then(|v| v.as_i64())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    Some((path, size, modified))
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn fires_of(spec: &str) -> (&'static str, String) {
    if let Some(e) = spec.strip_prefix("every:") {
        ("every", e.into())
    } else if let Some(c) = spec.strip_prefix("cron:") {
        ("cron", c.into())
    } else if let Some(t) = spec.strip_prefix("table:") {
        ("table", t.into())
    } else if spec == "all_tables" {
        ("all_tables", String::new())
    } else if let Some(p) = spec.strip_prefix("request:") {
        ("request", p.into())
    } else {
        ("every", String::new())
    }
}

/// The form for `t` (`None`: a new one); `file`: its plugin's file.
pub(crate) fn form_of(
    t: Option<&Trigger>,
    databases: &[String],
    file: Option<(String, Option<u64>, Option<String>)>,
) -> ObjectForm {
    let mut f = ObjectForm::default();
    let existing = t.is_some();
    let t = t.cloned().unwrap_or_else(|| Trigger {
        spec: "every:1m".into(),
        error_behavior: "log".into(),
        ..Trigger::default()
    });
    let (fires, what) = fires_of(&t.spec);
    let v = &mut f.values;
    v.insert("database".into(), json!(t.db));
    v.insert("name".into(), json!(t.name));
    v.insert("plugin".into(), json!(t.plugin));
    v.insert("enabled".into(), json!(!t.disabled));
    v.insert("fires".into(), json!(fires));
    for k in ["every", "cron", "table", "path"] {
        v.insert(k.into(), json!(""));
    }
    let slot = match fires {
        "every" => Some("every"),
        "cron" => Some("cron"),
        "table" => Some("table"),
        "request" => Some("path"),
        _ => None,
    };
    if let Some(k) = slot {
        v.insert(k.into(), json!(what));
    }
    v.insert("error_behavior".into(), json!(t.error_behavior));
    // Not reported for an existing trigger.
    v.insert(
        "run_async".into(),
        if existing { J::Null } else { json!(false) },
    );
    v.insert("plugin_code".into(), json!(""));
    let mut dbs: Vec<(String, String)> = databases.iter().map(|d| (d.clone(), d.clone())).collect();
    if !t.db.is_empty() && !dbs.iter().any(|(d, _)| *d == t.db) {
        dbs.push((t.db.clone(), t.db.clone()));
    }
    f.choices.insert("database".into(), dbs);

    let args = FormList::new(
        "arguments",
        "Argumentos",
        "key",
        vec![
            FormField::new("key", "Clave", FormKind::Text).required(),
            FormField::new("value", "Valor", FormKind::Text),
        ],
    )
    .rows(
        t.args
            .iter()
            .map(|(k, v)| {
                BTreeMap::from([
                    ("key".to_string(), json!(k)),
                    ("value".to_string(), json!(v)),
                ])
            })
            .collect(),
    );
    let mut code = vec![FormField::new("plugin_code", "Código del plugin", FormKind::code("python"))
        .placeholder("def process_scheduled_call(influxdb3_local, call_time, args=None):\n    influxdb3_local.info(\"tick\")")
        .help(if existing {
            "Opcional: reemplaza el código del archivo de este trigger en el servidor. Vacío: el archivo queda como está (InfluxDB no permite leerlo)."
        } else {
            "Opcional: se sube como el archivo del plugin al directorio de plugins del servidor (reemplaza uno con ese nombre). Vacío: se usa el archivo que ya está en el servidor."
        })];
    if let Some((path, size, modified)) = &file {
        let v = &mut f.values;
        v.insert("plugin_path".into(), json!(path));
        v.insert(
            "plugin_size".into(),
            size.map(|n| json!(n)).unwrap_or(J::Null),
        );
        v.insert(
            "plugin_modified".into(),
            json!(modified.clone().unwrap_or_default()),
        );
        code.extend([
            FormField::new("plugin_path", "Archivo en el servidor", FormKind::Text).read_only(true),
            FormField::new(
                "plugin_size",
                "Tamaño (bytes)",
                FormKind::number(Some(0.0), None),
            )
            .read_only(true),
            FormField::new("plugin_modified", "Modificado", FormKind::DateTime).read_only(true),
        ]);
    }
    f.sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("database", "Base de datos", FormKind::reference("database", false))
                .required(),
            FormField::new("name", "Nombre", FormKind::Text).required(),
            FormField::new("plugin", "Plugin", FormKind::Text)
                .required()
                .placeholder("mi_plugin.py")
                .help("El archivo .py (o la carpeta) en el directorio de plugins del servidor, o gh:… del repositorio de plugins de InfluxData."),
            FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        ]),
        FormSection::new("trigger", "Disparo").fields(vec![
            FormField::new(
                "fires",
                "Se dispara",
                FormKind::select(&[
                    ("every", "Cada un intervalo (every)"),
                    ("cron", "Según un cron (cron)"),
                    ("table", "Al escribir en una tabla (table)"),
                    ("all_tables", "Al escribir en cualquier tabla (all_tables)"),
                    ("request", "Con una petición HTTP (request)"),
                ]),
            )
            .required()
            .default(json!("every")),
            FormField::new("every", "Cada", FormKind::Text)
                .when("fires", &["every"])
                .required()
                .placeholder("10m")
                .help("Una duración: 30s, 10m, 1h, 1d."),
            FormField::new("cron", "Cron", FormKind::Cron)
                .when("fires", &["cron"])
                .required()
                .placeholder("0 0 3 * * *")
                .help("Seis campos, con los segundos primero (segundo, minuto, hora, día, mes, día de la semana), en UTC."),
            FormField::new("table", "Tabla", FormKind::Text)
                .when("fires", &["table"])
                .required()
                .help("El plugin recibe las filas que se escriben en esta tabla."),
            FormField::new("path", "Ruta", FormKind::Text)
                .when("fires", &["request"])
                .required()
                .placeholder("mi_endpoint")
                .help("El trigger responde en /api/v3/engine/<ruta>."),
        ]),
        FormSection::new("run", "Ejecución").fields(vec![
            FormField::new(
                "error_behavior",
                "Si el plugin falla",
                FormKind::select(&[
                    ("log", "Lo registra en el log"),
                    ("retry", "Reintenta"),
                    ("disable", "Deshabilita el trigger"),
                ]),
            )
            .default(json!("log")),
            FormField::new("run_async", "Ejecución asíncrona", FormKind::Bool)
                .help("Cada ejecución corre aparte, sin esperar a que termine la anterior.")
                .default(json!(false)),
        ]),
        FormSection::new("arguments", "Argumentos").list(args),
        FormSection::new("code", "Código del plugin").fields(code),
    ];
    if existing {
        f.notes.push(RECREATE.into());
        f.notes.push(ASYNC_UNKNOWN.into());
    }
    f
}

pub(crate) async fn form(s: &SqlSession, id: Option<&str>) -> Result<ObjectForm> {
    let dbs = databases(s).await?;
    match id {
        None => Ok(form_of(None, &dbs, None)),
        Some(id) => {
            let t = read(s, id).await?;
            let file = plugin_file(s, &t).await;
            Ok(form_of(Some(&t), &dbs, file))
        }
    }
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// What the designer asks for, checked.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wanted {
    pub(crate) trigger: Trigger,
    /// `None`: not said (an existing trigger's, which the server doesn't
    /// report).
    pub(crate) run_async: Option<bool>,
    pub(crate) code: Option<String>,
}

/// A plugin's file name or path, inside the plugin directory.
fn check_plugin(p: &str) -> Result<()> {
    check_line(p, "Plugin")?;
    let local = p.strip_prefix("gh:").unwrap_or(p);
    if local.is_empty()
        || p.starts_with('/')
        || p.starts_with('\\')
        || local.split(['/', '\\']).any(|part| part == "..")
    {
        return Err(bad(format!(
            "«{}» no es un plugin del directorio de plugins: va su nombre (mi_plugin.py) o su ruta dentro de él.",
            shown(p)
        )));
    }
    Ok(())
}

pub(crate) fn wanted(v: &FormValues, databases: &[String]) -> Result<Wanted> {
    let x = Vals(&v.values);
    let db = x
        .opt("database")
        .ok_or_else(|| bad("Falta la base de datos del trigger.".into()))?;
    if !databases.contains(&db) {
        return Err(bad(format!(
            "No existe la base de datos «{}» en este servidor.",
            shown(&db)
        )));
    }
    let name = x
        .opt("name")
        .ok_or_else(|| bad("El trigger necesita un nombre.".into()))?;
    check_line(&name, "Nombre")?;
    let plugin = x
        .opt("plugin")
        .ok_or_else(|| bad("Falta el plugin que ejecuta el trigger.".into()))?;
    check_plugin(&plugin)?;
    let spec = match x.text("fires").as_str() {
        "" | "every" => {
            let e = x
                .opt("every")
                .ok_or_else(|| bad("Falta cada cuánto se dispara el trigger.".into()))?;
            if !duration_ok(&e, UNITS, true) {
                return Err(bad(format!(
                    "«{}» no es una duración (por ejemplo 30s, 10m, 1h, 1d).",
                    shown(&e)
                )));
            }
            format!("every:{e}")
        }
        "cron" => {
            let c = x
                .opt("cron")
                .ok_or_else(|| bad("Falta la expresión cron del trigger.".into()))?;
            check_line(&c, "Cron")?;
            if !cron_ok(&c, 6, 7, true) {
                return Err(bad(format!(
                    "«{}» no es una expresión cron de InfluxDB 3: lleva seis campos, con los segundos primero (por ejemplo 0 0 3 * * *).",
                    shown(&c)
                )));
            }
            format!("cron:{c}")
        }
        "table" => {
            let t = x
                .opt("table")
                .ok_or_else(|| bad("Falta la tabla que dispara el trigger.".into()))?;
            check_line(&t, "Tabla")?;
            format!("table:{t}")
        }
        "all_tables" => "all_tables".to_string(),
        "request" => {
            let p = x.text("path");
            let p = p.trim_start_matches('/');
            if p.is_empty() {
                return Err(bad("Falta la ruta en la que responde el trigger.".into()));
            }
            if !p
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-./".contains(c))
                || p.split('/').any(|part| part == "..")
            {
                return Err(bad(format!(
                    "«{}» no es una ruta válida: letras, números, «_», «-», «.» y «/».",
                    shown(p)
                )));
            }
            format!("request:{p}")
        }
        other => {
            return Err(bad(format!(
                "«{}» no es una forma de disparar un trigger.",
                shown(other)
            )))
        }
    };
    let error_behavior = match x.text("error_behavior").as_str() {
        "" | "log" => "log",
        "retry" => "retry",
        "disable" => "disable",
        other => {
            return Err(bad(format!(
                "«{}» no es un comportamiento ante errores (log, retry o disable).",
                shown(other)
            )))
        }
    };
    let mut args: Vec<(String, String)> = Vec::new();
    for r in rows(v, "arguments") {
        let k = r.text("key");
        let val = r.raw("value");
        if k.is_empty() {
            if val.trim().is_empty() {
                continue;
            }
            return Err(bad("Un argumento necesita su clave.".into()));
        }
        check_line(&k, "Clave")?;
        if args.iter().any(|(a, _)| *a == k) {
            return Err(bad(format!("El argumento «{}» está dos veces.", shown(&k))));
        }
        args.push((k, val));
    }
    args.sort();
    let code = Some(x.raw("plugin_code")).filter(|c| !c.trim().is_empty());
    if code.is_some() && plugin.starts_with("gh:") {
        return Err(bad(
            "Un plugin gh:… viene del repositorio de InfluxData: no se le sube código.".into(),
        ));
    }
    Ok(Wanted {
        trigger: Trigger {
            db,
            name,
            plugin,
            spec,
            disabled: !x.flag("enabled").unwrap_or(true),
            error_behavior: error_behavior.into(),
            args,
        },
        run_async: x.flag("run_async"),
        code,
    })
}

fn create_req(t: &Trigger, run_async: bool) -> Req {
    let args: Map<String, J> = t
        .args
        .iter()
        .map(|(k, v)| (k.clone(), J::String(v.clone())))
        .collect();
    Req::new(
        "POST",
        "/api/v3/configure/processing_engine_trigger",
        Some(json!({
            "db": t.db,
            "plugin_filename": t.plugin,
            "trigger_name": t.name,
            "trigger_specification": t.spec,
            "trigger_arguments": if args.is_empty() { J::Null } else { J::Object(args) },
            "trigger_settings": { "run_async": run_async, "error_behavior": t.error_behavior },
            "disabled": t.disabled,
        })),
    )
}

fn delete_req(t: &Trigger) -> Req {
    Req::new(
        "DELETE",
        "/api/v3/configure/processing_engine_trigger",
        Some(json!({ "db": t.db, "trigger_name": t.name, "force": true })),
    )
}

fn upload_req(plugin: &str, code: &str) -> Req {
    Req::new(
        "POST",
        "/api/v3/plugins/files",
        Some(json!({ "plugin_name": plugin, "content": code })),
    )
}

/// What a save sends, the request that puts the old trigger back when one
/// after `undo_after` fails, and the id it ends with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub(crate) header: Vec<String>,
    pub(crate) reqs: Vec<Req>,
    pub(crate) undo_after: Option<usize>,
    pub(crate) undo: Option<Req>,
    pub(crate) id: String,
}

impl Plan {
    pub(crate) fn script(&self) -> String {
        render("--", &self.header, &self.reqs)
    }
}

/// `shared`: the other triggers whose plugin is the same file.
fn upload_note(plugin: &str, shared: &[String]) -> String {
    if shared.is_empty() {
        format!(
            "Sube el código como el archivo «{}» del directorio de plugins del servidor (reemplaza el que haya).",
            plugin
        )
    } else {
        // The other triggers run the new code too.
        warning(format!(
            "Sube el código como el archivo «{}» del directorio de plugins del servidor; también lo usan: {}.",
            plugin,
            shared.join(", ")
        ))
    }
}

pub(crate) fn create_plan(w: &Wanted, shared: &[String]) -> Plan {
    let t = &w.trigger;
    let mut header = vec![format!(
        "Crea el trigger «{}» en la base de datos «{}» de InfluxDB 3.",
        t.name, t.db
    )];
    let mut reqs = Vec::new();
    if let Some(code) = &w.code {
        header.push(upload_note(&t.plugin, shared));
        reqs.push(upload_req(&t.plugin, code));
    }
    reqs.push(create_req(t, w.run_async.unwrap_or(false)));
    Plan {
        header,
        reqs,
        undo_after: None,
        undo: None,
        id: pair_id(&t.db, &t.name),
    }
}

pub(crate) fn edit_plan(old: &Trigger, w: &Wanted, shared: &[String]) -> Plan {
    let t = &w.trigger;
    let id = pair_id(&t.db, &t.name);
    let same_place = t.db == old.db && t.name == old.name;
    let mut old_args = old.args.clone();
    old_args.sort();
    let recreate = !same_place
        || t.plugin != old.plugin
        || t.spec != old.spec
        || t.args != old_args
        || t.error_behavior != old.error_behavior
        || w.run_async == Some(true);
    let mut header = Vec::new();
    let mut reqs = Vec::new();
    if !recreate {
        if let Some(code) = &w.code {
            header.push(format!(
                "Reemplaza el código del plugin del trigger «{}» (influxdb3 update trigger).",
                old.name
            ));
            reqs.push(
                Req::new(
                    "PUT",
                    "/api/v3/plugins/files",
                    Some(json!({ "plugin_name": old.name, "content": code })),
                )
                .query("db", &old.db),
            );
            // The file is rewritten: the other triggers on it run the new
            // code too.
            if !shared.is_empty() {
                header.push(warning(format!(
                    "El archivo del plugin lo usan también: {}; pasan a ejecutar el código nuevo.",
                    shared.join(", ")
                )));
            }
        }
        if t.disabled != old.disabled {
            let verb = if t.disabled { "disable" } else { "enable" };
            header.push(format!(
                "{} el trigger «{}».",
                if t.disabled {
                    "Deshabilita"
                } else {
                    "Habilita"
                },
                old.name
            ));
            reqs.push(
                Req::new(
                    "POST",
                    format!("/api/v3/configure/processing_engine_trigger/{verb}"),
                    None,
                )
                .query("db", &old.db)
                .query("trigger_name", &old.name),
            );
        }
        if reqs.is_empty() {
            header.push(format!("«{}»: no hay cambios que guardar.", old.name));
        }
        return Plan {
            header,
            reqs,
            undo_after: None,
            undo: None,
            id,
        };
    }
    header.push(warning(
        "InfluxDB 3 no puede cambiar un trigger: se borra y se vuelve a crear.",
    ));
    header.push(warning(
        "Se pierde la caché del plugin (influxdb3_local.cache); su registro queda, con su nombre.",
    ));
    if w.run_async.is_none() {
        header.push(warning(
            "InfluxDB 3 no informa si el trigger se ejecutaba en modo asíncrono: el nuevo se ejecuta en modo síncrono.",
        ));
    }
    if let Some(code) = &w.code {
        header.push(upload_note(&t.plugin, shared));
        // The file goes up first: a change the server refuses leaves the
        // old trigger (kept, or put back) running the new code.
        if t.plugin == old.plugin {
            header.push(warning(
                "El código se sube antes de cambiar el trigger: si el servidor rechaza el cambio, el trigger anterior queda con el código nuevo.",
            ));
        }
        reqs.push(upload_req(&t.plugin, code));
    }
    let create = create_req(t, w.run_async.unwrap_or(false));
    if same_place {
        header.push(
            "Si el servidor rechaza el nuevo, DBine vuelve a crear el anterior como estaba.".into(),
        );
        let undo_after = reqs.len();
        reqs.push(delete_req(old));
        reqs.push(create);
        Plan {
            header,
            reqs,
            undo_after: Some(undo_after),
            undo: Some(create_req(old, false)),
            id,
        }
    } else {
        header.push(format!(
            "Primero crea «{}» en «{}» y después borra «{}» de «{}».",
            t.name, t.db, old.name, old.db
        ));
        reqs.push(create);
        reqs.push(delete_req(old));
        Plan {
            header,
            reqs,
            undo_after: None,
            undo: None,
            id,
        }
    }
}

/// The other triggers that run the same plugin file, as `_internal`'s
/// `system.plugin_files` lists them.
async fn sharing(s: &SqlSession, plugin: &str, name: &str) -> Vec<String> {
    let base = plugin.rsplit('/').next().unwrap_or(plugin);
    let q = format!(
        "SELECT DISTINCT plugin_name FROM system.plugin_files WHERE file_name = '{}' AND plugin_name <> '{}' ORDER BY plugin_name",
        base.replace('\'', "''"),
        name.replace('\'', "''")
    );
    s.jobs_sql("_internal", &q)
        .await
        .map(|(_, rows)| rows.iter().filter_map(|r| text_of(r.first())).collect())
        .unwrap_or_default()
}

async fn plan(s: &SqlSession, id: Option<&str>, v: &FormValues) -> Result<Plan> {
    let old = match id {
        Some(id) => Some(read(s, id).await?),
        None => None,
    };
    let w = wanted(v, &databases(s).await?)?;
    let shared = if w.code.is_some() {
        sharing(s, &w.trigger.plugin, &w.trigger.name).await
    } else {
        Vec::new()
    };
    Ok(match &old {
        None => create_plan(&w, &shared),
        Some(old) => edit_plan(old, &w, &shared),
    })
}

pub(crate) async fn script(s: &SqlSession, id: Option<&str>, v: &FormValues) -> Result<String> {
    Ok(plan(s, id, v).await?.script())
}

async fn call(s: &SqlSession, r: &Req) -> Result<()> {
    let mut req = s.http.request(r.method(), r.url(&s.base, None)?);
    if let Some(b) = &r.body {
        req = req.json(b);
    }
    http::text(s.auth(req).send().await.map_err(send_err)?).await?;
    Ok(())
}

pub(crate) async fn save(
    s: &SqlSession,
    id: Option<&str>,
    v: &FormValues,
) -> Result<Option<String>> {
    let p = plan(s, id, v).await?;
    for (i, r) in p.reqs.iter().enumerate() {
        let Err(e) = call(s, r).await else {
            continue;
        };
        if i == 0 {
            return Err(e);
        }
        return Err(match (p.undo_after, &p.undo) {
            (Some(after), Some(undo)) if i > after => match call(s, undo).await {
                Ok(()) => Error::Query(format!(
                    "No se guardó el cambio: {e}. Se volvió a crear el trigger anterior."
                )),
                Err(e2) => Error::Query(format!(
                    "No se guardó el cambio: {e}. Tampoco se pudo volver a crear el trigger anterior: {e2}"
                )),
            },
            _ => Error::Query(format!(
                "El cambio quedó a medias: el servidor rechazó un paso ({e})."
            )),
        });
    }
    Ok(Some(p.id))
}

pub(crate) fn drop_plan(t: &Trigger) -> Plan {
    Plan {
        header: vec![format!(
            "Borra el trigger «{}» de la base de datos «{}» (aunque esté habilitado). El archivo del plugin queda en el servidor.",
            t.name, t.db
        )],
        reqs: vec![delete_req(t)],
        undo_after: None,
        undo: None,
        id: pair_id(&t.db, &t.name),
    }
}

pub(crate) async fn delete_script(s: &SqlSession, id: &str) -> Result<String> {
    Ok(drop_plan(&read(s, id).await?).script())
}

pub(crate) async fn delete(s: &SqlSession, id: &str) -> Result<()> {
    let t = read(s, id).await?;
    call(s, &delete_req(&t)).await
}

// ---------------------------------------------------------------------------
// The processing engine's log
// ---------------------------------------------------------------------------

const ALL_LOGS: &str = "SELECT event_time, database_name, trigger_name, log_level, log_text, error_details FROM system.processing_engine_logs ORDER BY event_time DESC LIMIT 2000";
const DB_LOGS: &str = "SELECT event_time, trigger_name, log_level, log_text FROM system.processing_engine_logs ORDER BY event_time DESC LIMIT 2000";

/// Rows of a log query as the table's (`db`: the database, when the rows
/// don't say it).
pub(crate) fn log_rows(cols: &[String], rows: &[Vec<J>], db: Option<&str>) -> Vec<Vec<J>> {
    let at = |r: &[J], n: &str| text_of(cols.iter().position(|c| c == n).and_then(|i| r.get(i)));
    rows.iter()
        .map(|r| {
            let mut message = at(r, "log_text").unwrap_or_default();
            if let Some(d) = at(r, "error_details") {
                message = format!("{message} — {d}");
            }
            vec![
                J::from(at(r, "event_time").unwrap_or_default()),
                J::from(
                    at(r, "database_name")
                        .or(db.map(str::to_string))
                        .unwrap_or_default(),
                ),
                J::from(at(r, "trigger_name").unwrap_or_default()),
                J::from(at(r, "log_level").unwrap_or_default()),
                J::from(message),
            ]
        })
        .collect()
}

pub(crate) async fn logs(s: &SqlSession) -> Result<Vec<MonitorTable>> {
    let mut table = MonitorTable::new(
        "processing_engine",
        "Motor de procesamiento",
        &["Fecha", "Base de datos", "Trigger", "Nivel", "Mensaje"],
    );
    match s.jobs_sql("_internal", ALL_LOGS).await {
        Ok((cols, rows)) => table.rows = log_rows(&cols, &rows, None),
        Err(e) if e.is_connect() => return Err(e),
        Err(_) => {
            // Older servers: one database at a time.
            let mut all = Vec::new();
            for db in databases(s).await? {
                if let Ok((cols, rows)) = s.jobs_sql(&db, DB_LOGS).await {
                    all.extend(log_rows(&cols, &rows, Some(&db)));
                }
            }
            all.sort_by(|a, b| b[0].as_str().cmp(&a[0].as_str()));
            all.truncate(LOG_ROWS);
            table.rows = all;
        }
    }
    Ok(vec![table])
}

/// Why a trigger's log can't be cleared.
pub(crate) fn no_clear() -> Error {
    Error::Unsupported(NO_CLEAR.into())
}

/// Why a trigger can't be started (at a step or at all).
pub(crate) fn no_steps() -> Error {
    Error::Unsupported(V3_NO_RUN.into())
}

//! Refresh policies in the designer: when Dremio refreshes reflections and
//! when their data expires. A physical dataset may set its own
//! (`accelerationRefreshPolicy`: every period, on a cron schedule, never,
//! or when an Iceberg table's data changes; expiry; full, incremental or
//! automatic refresh, and the incremental refresh's column); the rest
//! follow their source's (`acceleration…` fields of the source: period,
//! schedule or never; expiry). Dremio Software has no SQL for either: a
//! change is `PUT /api/v3/catalog/{id}` with the entity as it is (its
//! `tag` guards against a concurrent change, a source's secrets go back
//! masked so Dremio keeps them) and the policy changed; deleting a table's
//! is the same `PUT` without it, after which the table follows its source.

use super::{
    bad, column, columns_of, human, path_label, path_of, rest_script, secs, shown, uuid_id, Plan,
    Step, Vals,
};
use crate::index_usage::dataset_path;
use crate::jobs::is_uuid;
use crate::{text, DremioSession};
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};

const TABLE: &str = "table";
const SOURCE: &str = "source";
/// Tables read for the list (one request each).
const MAX_TABLES: usize = 200;

pub(crate) const NO_POLICIES_LISTED: &str = "En «Políticas de refresco de tablas» se listan las tablas con reflexiones que tienen su propia política: Dremio no tiene cómo listar las demás tablas con política propia sin leer todo su catálogo.";
const STALE: &str = "Cambió en el servidor desde que se abrió el formulario: abrilo de nuevo para no pisar ese cambio.";
const TABLE_NOTE: &str = "Dremio no tiene SQL para la política de refresco de una tabla: DBine la guarda con su API REST. Sin política propia, la tabla sigue la de su origen.";
const SOURCE_NOTE: &str = "Dremio no tiene SQL para la política de refresco de un origen: DBine la guarda con su API REST. La siguen las tablas del origen que no tienen una propia.";
const VIEW: &str = "Solo una tabla (un dataset físico) tiene política de refresco: las reflexiones de una vista se refrescan con las de sus tablas.";
const ALREADY: &str =
    "Esa tabla ya tiene su propia política de refresco: abrila desde la lista para cambiarla.";
const NONE_OWN: &str = "Esta tabla no tiene política propia: ya sigue la de su origen.";

/// Dremio's refresh defaults (1 h; data kept 3 h; the UI's 8 AM schedule).
const DEFAULT_PERIOD_MS: i64 = 3_600_000;
const DEFAULT_GRACE_MS: i64 = 10_800_000;
const DEFAULT_SCHEDULE: &str = "0 0 8 * * ?";
/// One minute to ten years, in seconds.
const MIN_SECS: i64 = 60;
const MAX_SECS: i64 = 315_360_000;

/// A refresh policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Policy {
    /// PERIOD, SCHEDULE, NEVER, REFRESH_ON_DATA_CHANGES (tables).
    pub(crate) kind: String,
    pub(crate) period_ms: i64,
    pub(crate) schedule: String,
    pub(crate) grace_ms: i64,
    pub(crate) never_expire: bool,
    /// Tables: AUTO, FULL, INCREMENTAL.
    pub(crate) method: Option<String>,
    /// Tables: the column an incremental refresh follows.
    pub(crate) field: Option<String>,
}

/// A table's own policy, if it has one.
pub(crate) fn table_policy_of(entity: &Value) -> Option<Policy> {
    let p = entity.get("accelerationRefreshPolicy")?.as_object()?;
    let never = p.get("neverRefresh").and_then(Value::as_bool) == Some(true);
    let kind = p
        .get("activePolicyType")
        .map(text)
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| if never { "NEVER" } else { "PERIOD" }.into());
    Some(Policy {
        kind: if never { "NEVER".into() } else { kind },
        period_ms: p
            .get("refreshPeriodMs")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_PERIOD_MS),
        schedule: p
            .get("refreshSchedule")
            .map(text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SCHEDULE.into()),
        grace_ms: p
            .get("gracePeriodMs")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_GRACE_MS),
        never_expire: p.get("neverExpire").and_then(Value::as_bool) == Some(true),
        method: Some(
            p.get("method")
                .map(text)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "FULL".into()),
        ),
        field: p.get("refreshField").map(text).filter(|f| !f.is_empty()),
    })
}

/// A source's policy.
pub(crate) fn source_policy_of(entity: &Value) -> Policy {
    let never = entity
        .get("accelerationNeverRefresh")
        .and_then(Value::as_bool)
        == Some(true);
    let kind = entity
        .get("accelerationActivePolicyType")
        .map(text)
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| "PERIOD".into());
    Policy {
        kind: if never { "NEVER".into() } else { kind },
        period_ms: entity
            .get("accelerationRefreshPeriodMs")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        schedule: entity
            .get("accelerationRefreshSchedule")
            .map(text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SCHEDULE.into()),
        grace_ms: entity
            .get("accelerationGracePeriodMs")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        never_expire: entity
            .get("accelerationNeverExpire")
            .and_then(Value::as_bool)
            == Some(true),
        method: None,
        field: None,
    }
}

/// A policy as people read it.
pub(crate) fn summary(p: &Policy) -> String {
    let refresh = match p.kind.as_str() {
        "NEVER" => "Nunca se refresca".to_string(),
        "SCHEDULE" => format!("Programada: {}", p.schedule),
        "REFRESH_ON_DATA_CHANGES" => "Cuando cambian los datos".to_string(),
        _ if p.period_ms > 0 => format!("Cada {}", human(p.period_ms)),
        _ => "Cada el período por defecto de Dremio".to_string(),
    };
    let expire = if p.never_expire {
        "no vence".to_string()
    } else if p.grace_ms > 0 {
        format!("vence a {}", human(p.grace_ms))
    } else {
        "vence según Dremio".to_string()
    };
    let mut s = format!("{refresh} · {expire}");
    if let Some(m) = &p.method {
        s.push_str(&format!(" · {}", m.to_ascii_lowercase()));
    }
    s
}

/// The table policy as `accelerationRefreshPolicy` takes it (every field:
/// Dremio fails on a missing one).
pub(crate) fn table_json(p: &Policy) -> Value {
    let mut o = json!({
        "activePolicyType": p.kind,
        "refreshPeriodMs": p.period_ms,
        "refreshSchedule": p.schedule,
        "gracePeriodMs": p.grace_ms,
        "method": p.method.clone().unwrap_or_else(|| "FULL".into()),
        "neverExpire": p.never_expire,
        "neverRefresh": p.kind == "NEVER",
    });
    if let Some(f) = &p.field {
        o["refreshField"] = json!(f);
    }
    o
}

/// The fields of a source that differ from `current`.
pub(crate) fn source_patch(current: &Policy, p: &Policy) -> Map<String, Value> {
    let mut m = Map::new();
    if p.kind != current.kind {
        m.insert("accelerationActivePolicyType".into(), json!(p.kind));
        m.insert("accelerationNeverRefresh".into(), json!(p.kind == "NEVER"));
    }
    if p.period_ms != current.period_ms {
        m.insert("accelerationRefreshPeriodMs".into(), json!(p.period_ms));
    }
    if p.schedule != current.schedule {
        m.insert("accelerationRefreshSchedule".into(), json!(p.schedule));
    }
    if p.grace_ms != current.grace_ms {
        m.insert("accelerationGracePeriodMs".into(), json!(p.grace_ms));
    }
    if p.never_expire != current.never_expire {
        m.insert("accelerationNeverExpire".into(), json!(p.never_expire));
    }
    m
}

/// A Quartz cron expression as Dremio's schedules take it: seconds,
/// minutes, hours, day of month, month, day of week (and an optional year),
/// with `?` in one of the two days.
pub(crate) fn cron_ok(c: &str) -> bool {
    let f: Vec<&str> = c.split_whitespace().collect();
    (6..=7).contains(&f.len())
        && f.iter().all(|x| {
            x.len() <= 100
                && x.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "*?,-/#".contains(ch))
        })
        && ((f[3] == "?") ^ (f[5] == "?"))
}

/// The form's values as a policy; `current`: what the server has, whose
/// values aren't checked again (a source's may be 0: Dremio's default).
/// `columns`: the table's, for the incremental refresh's column.
pub(crate) fn wanted(
    values: &FormValues,
    table: bool,
    current: Option<&Policy>,
    columns: &[String],
) -> Result<Policy> {
    let v = Vals(&values.values);
    let kind = match v.text("policy_type").to_ascii_uppercase().as_str() {
        "" | "PERIOD" => "PERIOD",
        "SCHEDULE" => "SCHEDULE",
        "NEVER" => "NEVER",
        "REFRESH_ON_DATA_CHANGES" if table => "REFRESH_ON_DATA_CHANGES",
        other => {
            return Err(bad(format!(
                "«{}» no es un tipo de política de refresco.",
                shown(other)
            )))
        }
    }
    .to_string();
    let secs_field = |k: &str, label: &str, cur: Option<i64>| -> Result<i64> {
        match v.int(k, label, 0, MAX_SECS)? {
            None => Ok(cur.unwrap_or(0)),
            Some(s) if Some(s * 1000) == cur => Ok(s * 1000),
            Some(s) if s < MIN_SECS => Err(bad(format!(
                "«{label}» tiene que ser de al menos un minuto."
            ))),
            Some(s) => Ok(s * 1000),
        }
    };
    let period_ms = secs_field(
        "refresh_period",
        "Refrescar cada",
        current.map(|c| c.period_ms).or(Some(DEFAULT_PERIOD_MS)),
    )?;
    let grace_ms = secs_field(
        "grace_period",
        "Los datos vencen a",
        current.map(|c| c.grace_ms).or(Some(DEFAULT_GRACE_MS)),
    )?;
    let schedule = match v.opt("refresh_schedule") {
        None => current.map_or_else(|| DEFAULT_SCHEDULE.to_string(), |c| c.schedule.clone()),
        Some(c) => {
            let c = c.split_whitespace().collect::<Vec<_>>().join(" ");
            let same = current.is_some_and(|x| x.schedule == c);
            if !same && !cron_ok(&c) {
                return Err(bad(format!(
                    "«{}» no es una programación de Dremio: segundos, minutos, horas, día del mes, mes y día de la semana, con «?» en uno de los dos días (0 0 8 * * ?).",
                    shown(&c)
                )));
            }
            c
        }
    };
    if kind == "SCHEDULE" && !cron_ok(&schedule) {
        return Err(bad(
            "Una política programada necesita su programación (cron).".into(),
        ));
    }
    let never_expire = v.bool_or("never_expire", current.is_some_and(|c| c.never_expire));
    if kind == "PERIOD" && !never_expire && grace_ms > 0 && period_ms > 0 && grace_ms < period_ms {
        return Err(bad(
            "Los datos tienen que vencer después del próximo refresco: «Los datos vencen a» no puede ser menor que «Refrescar cada».".into(),
        ));
    }
    let (method, field) = if table {
        let m = match v.text("method").to_ascii_uppercase().as_str() {
            "" | "FULL" => "FULL",
            "INCREMENTAL" => "INCREMENTAL",
            "AUTO" => "AUTO",
            other => {
                return Err(bad(format!(
                    "«{}» no es un método de refresco (completo, incremental o automático).",
                    shown(other)
                )))
            }
        };
        let field = match (m, v.opt("refresh_field")) {
            ("INCREMENTAL", Some(f)) => {
                Some(column(columns, &f, "Columna del refresco incremental")?)
            }
            _ => None,
        };
        (Some(m.to_string()), field)
    } else {
        (None, None)
    };
    Ok(Policy {
        kind,
        period_ms,
        schedule,
        grace_ms,
        never_expire,
        method,
        field,
    })
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn policy_fields(table: bool, existing_table: bool) -> Vec<FormField> {
    let types = if table {
        FormKind::select(&[
            ("PERIOD", "Cada cierto tiempo"),
            ("SCHEDULE", "Con una programación (cron)"),
            ("NEVER", "Nunca"),
            (
                "REFRESH_ON_DATA_CHANGES",
                "Cuando cambian los datos (tablas Iceberg)",
            ),
        ])
    } else {
        FormKind::select(&[
            ("PERIOD", "Cada cierto tiempo"),
            ("SCHEDULE", "Con una programación (cron)"),
            ("NEVER", "Nunca"),
        ])
    };
    let mut fields = vec![
        FormField::new("policy_type", "Refrescar", types)
            .required()
            .default(json!("PERIOD")),
        FormField::new("refresh_period", "Refrescar cada", FormKind::Duration)
            .when("policy_type", &["PERIOD"])
            .default(json!(DEFAULT_PERIOD_MS / 1000)),
        FormField::new("refresh_schedule", "Programación", FormKind::Cron)
            .when("policy_type", &["SCHEDULE"])
            .placeholder(DEFAULT_SCHEDULE)
            .help("Segundos, minutos, horas, día del mes, mes y día de la semana, con «?» en uno de los dos días: 0 0 8 * * ? es todos los días a las 8; 0 30 6 ? * MON-FRI, de lunes a viernes a las 6:30."),
        FormField::new("never_expire", "Los datos no vencen", FormKind::Bool).default(json!(false)),
        FormField::new("grace_period", "Los datos vencen a", FormKind::Duration)
            .when("never_expire", &["false"])
            .default(json!(DEFAULT_GRACE_MS / 1000))
            .help("Pasado este tiempo sin un refresco que funcione, Dremio deja de usar la reflexión."),
    ];
    if table {
        fields.push(
            FormField::new(
                "method",
                "Método de refresco",
                FormKind::select(&[
                    ("FULL", "Completo"),
                    ("INCREMENTAL", "Incremental"),
                    ("AUTO", "Automático"),
                ]),
            )
            .default(json!("FULL"))
            .help("Incremental agrega solo lo nuevo: en un origen de archivos, los archivos nuevos; en otro, las filas cuya columna de refresco creció."),
        );
        fields.push(
            if existing_table {
                FormField::new(
                    "refresh_field",
                    "Columna del refresco incremental",
                    FormKind::reference("column", false),
                )
            } else {
                FormField::new("refresh_field", "Columna del refresco incremental", FormKind::Text)
            }
            .when("method", &["INCREMENTAL"])
            .help("Para orígenes que no son de archivos: una columna que solo crece (una fecha, un id)."),
        );
    }
    fields
}

fn put_policy(values: &mut BTreeMap<String, Value>, p: &Policy) {
    values.insert("policy_type".into(), json!(p.kind));
    values.insert("refresh_period".into(), secs(Some(p.period_ms)));
    values.insert("refresh_schedule".into(), json!(p.schedule));
    values.insert("never_expire".into(), json!(p.never_expire));
    values.insert("grace_period".into(), secs(Some(p.grace_ms)));
    if let Some(m) = &p.method {
        values.insert("method".into(), json!(m));
    }
    values.insert(
        "refresh_field".into(),
        json!(p.field.clone().unwrap_or_default()),
    );
}

fn default_table_policy() -> Policy {
    Policy {
        kind: "PERIOD".into(),
        period_ms: DEFAULT_PERIOD_MS,
        schedule: DEFAULT_SCHEDULE.into(),
        grace_ms: DEFAULT_GRACE_MS,
        never_expire: false,
        method: Some("FULL".into()),
        field: None,
    }
}

/// A table's policy form. `table`: the table (its entity) when it exists;
/// `source`: its source's policy, shown; `tables`: what a new one picks
/// from.
pub(crate) fn table_form_of(
    table: Option<&Value>,
    source: Option<&Policy>,
    tables: &[(String, String)],
) -> ObjectForm {
    let existing = table.is_some();
    let mut f = ObjectForm::default();
    let mut values = BTreeMap::new();
    let mut general = FormSection::new("general", "General");
    if let Some(e) = table {
        general =
            general.field(FormField::new("table_label", "Tabla", FormKind::Text).read_only(true));
        values.insert("table_label".into(), json!(path_label(&path_of(e))));
        values.insert(
            "tag".into(),
            json!(e.get("tag").map(text).unwrap_or_default()),
        );
        put_policy(
            &mut values,
            &table_policy_of(e).unwrap_or_else(default_table_policy),
        );
        f.choices.insert(
            "column".into(),
            columns_of(e).into_iter().map(|c| (c.clone(), c)).collect(),
        );
    } else {
        general = general.field(
            FormField::new("table", "Tabla", FormKind::reference("table", false)).required(),
        );
        put_policy(&mut values, &default_table_policy());
        f.choices.insert("table".into(), tables.to_vec());
    }
    let mut sections = vec![
        general,
        FormSection::new("policy", "Política").fields(policy_fields(true, existing)),
    ];
    if let Some(sp) = source {
        sections.push(
            FormSection::new("source", "Origen").field(
                FormField::new("source_policy", "Política del origen", FormKind::Text)
                    .read_only(true)
                    .help("La que sigue la tabla si se borra su política propia."),
            ),
        );
        values.insert("source_policy".into(), json!(summary(sp)));
    }
    f.sections = sections;
    f.values = values;
    f.notes.push(TABLE_NOTE.into());
    f
}

pub(crate) fn source_form_of(entity: &Value) -> ObjectForm {
    let mut f = ObjectForm::default();
    let mut values = BTreeMap::new();
    values.insert(
        "name".into(),
        json!(entity.get("name").map(text).unwrap_or_default()),
    );
    values.insert(
        "source_type".into(),
        json!(entity.get("type").map(text).unwrap_or_default()),
    );
    values.insert(
        "tag".into(),
        json!(entity.get("tag").map(text).unwrap_or_default()),
    );
    let p = source_policy_of(entity);
    put_policy(&mut values, &p);
    f.sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Origen", FormKind::Text).read_only(true),
            FormField::new("source_type", "Tipo", FormKind::Text).read_only(true),
        ]),
        FormSection::new("policy", "Política").fields(policy_fields(false, true)),
    ];
    f.values = values;
    f.notes.push(SOURCE_NOTE.into());
    if p.period_ms == 0 || p.grace_ms == 0 {
        f.notes
            .push("Un 0 es el valor por defecto de Dremio para el origen.".into());
    }
    f
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

pub(crate) fn table_put(
    uuid: &str,
    label: &str,
    entity: &Value,
    p: Option<&Policy>,
    why: &str,
) -> Plan {
    let mut body = entity.as_object().cloned().unwrap_or_default();
    match p {
        Some(p) => {
            body.insert("accelerationRefreshPolicy".into(), table_json(p));
        }
        None => {
            body.remove("accelerationRefreshPolicy");
        }
    }
    let path = format!("/api/v3/catalog/{uuid}");
    let shown_body = match p {
        Some(p) => json!({ "accelerationRefreshPolicy": table_json(p) }),
        None => json!({}),
    };
    let head = format!(
        "{why}\n(La tabla {label} como está{}.)",
        if p.is_some() {
            ", con esta política"
        } else {
            ", sin accelerationRefreshPolicy: sigue la de su origen"
        }
    );
    Plan {
        script: rest_script(&head, "PUT", &path, Some(&shown_body)),
        steps: vec![Step::Rest {
            method: Method::PUT,
            path,
            body: Some(Value::Object(body)),
        }],
        id: Some(format!("{TABLE}:{uuid}")),
        prefix: TABLE,
    }
}

pub(crate) fn source_put(
    uuid: &str,
    name: &str,
    entity: &Value,
    patch: Map<String, Value>,
) -> Plan {
    let mut body = entity.as_object().cloned().unwrap_or_default();
    body.remove("children");
    body.remove("state");
    for (k, v) in &patch {
        body.insert(k.clone(), v.clone());
    }
    let path = format!("/api/v3/catalog/{uuid}");
    Plan {
        script: rest_script(
            &format!("Política de refresco del origen «{name}»: Dremio no tiene SQL para ella, así que DBine la guarda con su API REST.\n(El origen como está, con estos cambios; sus secretos vuelven enmascarados y Dremio los conserva.)"),
            "PUT",
            &path,
            Some(&Value::Object(patch)),
        ),
        steps: vec![Step::Rest {
            method: Method::PUT,
            path,
            body: Some(Value::Object(body)),
        }],
        id: Some(format!("{SOURCE}:{uuid}")),
        prefix: SOURCE,
    }
}

// ---------------------------------------------------------------------------
// Refreshing a table's reflections now
// ---------------------------------------------------------------------------

/// The table policies' action: refresh the table's reflections now.
pub(crate) const REFRESH: &str = "refresh";

/// `POST /api/v3/catalog/{id}/refresh`: every reflection that depends on
/// the table, now, as its policy would (Dremio Software has no SQL for it).
pub(crate) fn refresh_plan(uuid: &str, label: &str) -> Plan {
    let path = format!("/api/v3/catalog/{uuid}/refresh");
    Plan {
        script: rest_script(
            &format!(
                "Refresca ahora las reflexiones de la tabla «{label}»: todas las que dependen de ella, como las refrescaría su política.\nDremio no tiene SQL para esto: DBine envía este pedido a su API REST."
            ),
            "POST",
            &path,
            None,
        ),
        steps: vec![Step::Rest {
            method: Method::POST,
            path,
            body: None,
        }],
        id: Some(format!("{TABLE}:{uuid}")),
        prefix: TABLE,
    }
}

/// A table policy's action (only `refresh`).
pub(crate) async fn table_action_plan(
    s: &mut DremioSession,
    id: &str,
    action: &str,
) -> Result<Plan> {
    if action.trim() != REFRESH {
        return Err(dbine_driver::Error::Unsupported(format!(
            "«{}» no es una acción de las políticas de refresco de tablas de Dremio: la que hay es refrescar sus reflexiones.",
            shown(action)
        )));
    }
    let (uuid, e) = s.table_entity(id).await?;
    Ok(refresh_plan(&uuid, &path_label(&path_of(&e))))
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

impl DremioSession {
    /// The source policy of a dataset entity (its path's first part).
    async fn source_policy_for(&self, entity: &Value) -> Option<Policy> {
        let source = path_of(entity).into_iter().next()?;
        let v = self.catalog_at(&[source]).await.ok()?;
        (v.get("entityType").and_then(Value::as_str) == Some("source"))
            .then(|| source_policy_of(&v))
    }

    async fn table_entity(&self, id: &str) -> Result<(String, Value)> {
        let uuid = uuid_id(id, TABLE, "tabla")?;
        let e = self.catalog(&uuid).await?;
        if e.get("type").and_then(Value::as_str) != Some("PHYSICAL_DATASET") {
            return Err(bad(VIEW.into()));
        }
        Ok((uuid, e))
    }

    async fn source_entity(&self, id: &str) -> Result<(String, Value)> {
        let uuid = uuid_id(id, SOURCE, "origen")?;
        let e = self.catalog(&uuid).await?;
        if e.get("entityType").and_then(Value::as_str) != Some("source") {
            return Err(bad(format!("«{}» no es un origen de Dremio.", shown(id))));
        }
        Ok((uuid, e))
    }
}

pub(crate) async fn table_items(s: &mut DremioSession) -> Result<SchedulerList> {
    let rows = s
        .records("SELECT DISTINCT dataset_id, dataset_type FROM sys.reflections")
        .await?;
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    let mut notes = Vec::new();
    let ids: Vec<String> = rows
        .iter()
        .filter(|r| {
            r.get("dataset_type")
                .map(text)
                .is_some_and(|t| t.starts_with("PHYSICAL"))
        })
        .filter_map(|r| r.get("dataset_id").map(text))
        .filter(|d| is_uuid(d) && seen.insert(d.to_ascii_lowercase()))
        .collect();
    if ids.len() > MAX_TABLES {
        notes.push(format!(
            "Se leyó la política de las primeras {MAX_TABLES} tablas con reflexiones."
        ));
    }
    for id in ids.iter().take(MAX_TABLES) {
        let Ok(e) = s.catalog(id).await else { continue };
        if let Some(p) = table_policy_of(&e) {
            items.push(SchedulerItem {
                id: format!("{TABLE}:{}", id.to_ascii_lowercase()),
                name: path_label(&path_of(&e)),
                description: Some(summary(&p)),
                enabled: None,
                system: false,
                can_edit: None,
                can_delete: None,
                actions: None,
                run_from_step: None,
                clear_history: None,
            });
        }
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    notes.push(NO_POLICIES_LISTED.into());
    Ok(SchedulerList { items, notes })
}

pub(crate) async fn source_items(s: &mut DremioSession) -> Result<SchedulerList> {
    let v = s.conn.send(Method::GET, "/api/v3/source", None).await?;
    let mut items: Vec<SchedulerItem> = v
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|src| {
            let id = src.get("id").map(text).filter(|u| is_uuid(u))?;
            Some(SchedulerItem {
                id: format!("{SOURCE}:{}", id.to_ascii_lowercase()),
                name: src.get("name").map(text).unwrap_or_default(),
                description: Some(format!(
                    "{} · {}",
                    src.get("type").map(text).unwrap_or_default(),
                    summary(&source_policy_of(src))
                )),
                enabled: None,
                system: false,
                can_edit: None,
                can_delete: None,
                actions: None,
                run_from_step: None,
                clear_history: None,
            })
        })
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

pub(crate) async fn table_form(s: &mut DremioSession, id: Option<&str>) -> Result<ObjectForm> {
    match id {
        None => {
            let tables = s.dataset_choices(false).await?;
            Ok(table_form_of(None, None, &tables))
        }
        Some(id) => {
            let (_, e) = s.table_entity(id).await?;
            let sp = s.source_policy_for(&e).await;
            let mut f = table_form_of(Some(&e), sp.as_ref(), &[]);
            if table_policy_of(&e).is_none() {
                f.notes.insert(0, NONE_OWN.into());
            }
            Ok(f)
        }
    }
}

pub(crate) async fn source_form(s: &mut DremioSession, id: &str) -> Result<ObjectForm> {
    let (_, e) = s.source_entity(id).await?;
    Ok(source_form_of(&e))
}

fn check_tag(values: &FormValues, entity: &Value) -> Result<()> {
    let tag = entity.get("tag").map(text).unwrap_or_default();
    if Vals(&values.values).opt("tag").is_some_and(|t| t != tag) {
        return Err(bad(STALE.into()));
    }
    Ok(())
}

pub(crate) async fn table_plan(
    s: &mut DremioSession,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    match id {
        None => {
            let picked = Vals(&values.values)
                .opt("table")
                .ok_or_else(|| bad("Elegí la tabla.".into()))?;
            let parts = dataset_path(&picked);
            if parts.len() < 2 || parts.iter().any(|p| p.is_empty() || p == "." || p == "..") {
                return Err(bad(format!(
                    "«{}» no es una tabla de Dremio.",
                    shown(&picked)
                )));
            }
            let e = s.catalog_at(&parts).await.map_err(|_| {
                bad(format!(
                    "No existe la tabla «{}» (o este usuario no la puede ver).",
                    shown(&path_label(&parts))
                ))
            })?;
            if e.get("type").and_then(Value::as_str) != Some("PHYSICAL_DATASET") {
                return Err(bad(VIEW.into()));
            }
            if table_policy_of(&e).is_some() {
                return Err(bad(ALREADY.into()));
            }
            let uuid = e
                .get("id")
                .map(text)
                .filter(|u| is_uuid(u))
                .ok_or_else(|| bad("Dremio no dio el id de la tabla.".into()))?
                .to_ascii_lowercase();
            let p = wanted(values, true, None, &columns_of(&e))?;
            let label = path_label(&path_of(&e));
            Ok(table_put(
                &uuid,
                &label,
                &e,
                Some(&p),
                &format!("Política de refresco propia de la tabla {label}: Dremio no tiene SQL para ella, así que DBine la guarda con su API REST."),
            ))
        }
        Some(id) => {
            let (uuid, e) = s.table_entity(id).await?;
            check_tag(values, &e)?;
            let current = table_policy_of(&e);
            let p = wanted(values, true, current.as_ref(), &columns_of(&e))?;
            let label = path_label(&path_of(&e));
            if current.as_ref() == Some(&p) {
                return Ok(Plan::nothing(
                    &format!("Política de refresco de {label}"),
                    format!("{TABLE}:{uuid}"),
                    TABLE,
                ));
            }
            Ok(table_put(
                &uuid,
                &label,
                &e,
                Some(&p),
                &format!("Política de refresco de la tabla {label}: Dremio no tiene SQL para ella, así que DBine la cambia con su API REST."),
            ))
        }
    }
}

pub(crate) async fn table_delete_plan(s: &mut DremioSession, id: &str) -> Result<Plan> {
    let (uuid, e) = s.table_entity(id).await?;
    if table_policy_of(&e).is_none() {
        return Err(bad(NONE_OWN.into()));
    }
    let label = path_label(&path_of(&e));
    Ok(table_put(
        &uuid,
        &label,
        &e,
        None,
        &format!("Borra la política de refresco propia de la tabla {label}: desde ahí sigue la de su origen."),
    ))
}

pub(crate) async fn source_plan(
    s: &mut DremioSession,
    id: &str,
    values: &FormValues,
) -> Result<Plan> {
    let (uuid, e) = s.source_entity(id).await?;
    check_tag(values, &e)?;
    let current = source_policy_of(&e);
    let p = wanted(values, false, Some(&current), &[])?;
    let name = e.get("name").map(text).unwrap_or_default();
    let patch = source_patch(&current, &p);
    if patch.is_empty() {
        return Ok(Plan::nothing(
            &format!("Política de refresco del origen «{name}»"),
            format!("{SOURCE}:{uuid}"),
            SOURCE,
        ));
    }
    Ok(source_put(&uuid, &name, &e, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_entity() -> Value {
        json!({"entityType": "dataset", "id": "9534f3ff-ba65-4f91-a1f7-e523d93a6c92", "type": "PHYSICAL_DATASET", "path": ["$scratch", "p"], "tag": "T1",
               "fields": [{"name": "a", "type": {"name": "INTEGER"}}, {"name": "ts", "type": {"name": "TIMESTAMP"}}],
               "accelerationRefreshPolicy": {"activePolicyType": "PERIOD", "refreshPeriodMs": 7200000, "refreshSchedule": "0 0 8 * * ?",
                   "gracePeriodMs": 14400000, "method": "FULL", "neverExpire": false, "neverRefresh": false, "sourceRefreshOnDataChanges": false}})
    }

    fn source_entity() -> Value {
        json!({"entityType": "source", "id": "16d1342a-59ad-4ac6-90e7-615e0123ed68", "tag": "S1", "type": "NAS", "name": "src",
               "config": {"password": "$DREMIO_EXISTING_VALUE$"}, "children": [{"id": "x"}], "state": {"status": "good"},
               "accelerationGracePeriodMs": 0, "accelerationRefreshPeriodMs": 0, "accelerationRefreshSchedule": "0 0 8 * * ?",
               "accelerationActivePolicyType": "PERIOD", "accelerationNeverExpire": false, "accelerationNeverRefresh": false})
    }

    fn form_values(f: &ObjectForm) -> FormValues {
        FormValues {
            values: f.values.clone(),
            lists: BTreeMap::new(),
        }
    }

    #[test]
    fn crons_as_dremio_takes_them() {
        for ok in [
            "0 0 8 * * ?",
            "0 30 6 ? * MON-FRI",
            "0 0 8 ? * 1,3",
            "0 0/15 * * * ? 2027",
        ] {
            assert!(cron_ok(ok), "{ok}");
        }
        for bad in [
            "bad cron",
            "0 0 8 * * *",
            "0 0 8 ? * ?",
            "* * * * *",
            "0 0 8 * * ?; DROP",
            "0 0 8 * * ? \"x\"",
        ] {
            assert!(!cron_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_table_policy_reads_and_writes_whole() {
        let e = table_entity();
        let p = table_policy_of(&e).unwrap();
        assert_eq!(
            (p.kind.as_str(), p.period_ms, p.grace_ms),
            ("PERIOD", 7_200_000, 14_400_000)
        );
        assert_eq!(summary(&p), "Cada 2 h · vence a 4 h · full");
        let j = table_json(&p);
        for k in [
            "activePolicyType",
            "refreshPeriodMs",
            "refreshSchedule",
            "gracePeriodMs",
            "method",
            "neverExpire",
            "neverRefresh",
        ] {
            assert!(j.get(k).is_some(), "{k}");
        }
        assert!(table_policy_of(&json!({"type": "PHYSICAL_DATASET"})).is_none());
    }

    #[test]
    fn refreshing_a_tables_reflections_is_its_catalog_refresh() {
        let u = "d215b51c-01f2-49f0-8120-74e508386c42";
        let p = refresh_plan(u, "s.f\nDROP TABLE t");
        assert_eq!(
            p.steps,
            vec![Step::Rest {
                method: Method::POST,
                path: format!("/api/v3/catalog/{u}/refresh"),
                body: None,
            }]
        );
        assert_eq!(p.id.as_deref(), Some(format!("table:{u}").as_str()));
        assert!(
            p.script.lines().all(|l| l.starts_with("-- ")),
            "{}",
            p.script
        );
        assert!(p
            .script
            .contains(&format!("-- POST /api/v3/catalog/{u}/refresh\n")));
    }

    #[test]
    fn an_untouched_table_form_changes_nothing_and_a_change_is_a_put() {
        let e = table_entity();
        let f = table_form_of(Some(&e), None, &[]);
        let v = form_values(&f);
        let current = table_policy_of(&e).unwrap();
        assert_eq!(
            wanted(&v, true, Some(&current), &columns_of(&e)).unwrap(),
            current
        );
        let mut v2 = v.clone();
        v2.values.insert("policy_type".into(), json!("SCHEDULE"));
        v2.values
            .insert("refresh_schedule".into(), json!("0 30 6 ?  * MON-FRI"));
        v2.values.insert("method".into(), json!("INCREMENTAL"));
        v2.values.insert("refresh_field".into(), json!("TS"));
        let p = wanted(&v2, true, Some(&current), &columns_of(&e)).unwrap();
        assert_eq!(p.schedule, "0 30 6 ? * MON-FRI");
        assert_eq!(p.field.as_deref(), Some("ts"));
        let plan = table_put(
            "9534f3ff-ba65-4f91-a1f7-e523d93a6c92",
            "$scratch.p",
            &e,
            Some(&p),
            "x",
        );
        assert!(plan
            .script
            .contains("-- PUT /api/v3/catalog/9534f3ff-ba65-4f91-a1f7-e523d93a6c92"));
        assert!(plan.script.lines().all(|l| l.starts_with("--")));
        let Step::Rest { body: Some(b), .. } = &plan.steps[0] else {
            panic!()
        };
        assert_eq!(
            b["accelerationRefreshPolicy"]["activePolicyType"],
            json!("SCHEDULE")
        );
        assert_eq!(b["accelerationRefreshPolicy"]["refreshField"], json!("ts"));
        assert_eq!(b["tag"], json!("T1"), "the tag guards the PUT");
        // Deleting it is the same PUT without the policy.
        let del = table_put("u", "$scratch.p", &e, None, "x");
        let Step::Rest { body: Some(b), .. } = &del.steps[0] else {
            panic!()
        };
        assert!(b.get("accelerationRefreshPolicy").is_none());
    }

    #[test]
    fn policy_values_are_checked() {
        let e = table_entity();
        let current = table_policy_of(&e).unwrap();
        let cols = columns_of(&e);
        let base = form_values(&table_form_of(Some(&e), None, &[]));
        let check = |k: &str, v: Value| {
            let mut x = base.clone();
            x.values.insert(k.into(), v);
            wanted(&x, true, Some(&current), &cols)
        };
        assert!(
            check("refresh_period", json!(30)).is_err(),
            "under a minute"
        );
        assert!(check("refresh_period", json!("x")).is_err());
        assert!(
            check("grace_period", json!(3600)).is_err(),
            "expires before the next refresh"
        );
        assert!(check("refresh_schedule", json!("bad cron")).is_err());
        assert!(check("policy_type", json!("SOMETIMES")).is_err());
        assert!(check("method", json!("PARTIAL")).is_err());
        let mut x = base.clone();
        x.values.insert("method".into(), json!("INCREMENTAL"));
        x.values.insert("refresh_field".into(), json!("nope"));
        assert!(wanted(&x, true, Some(&current), &cols).is_err());
        // A source has no "when the data changes" nor a method.
        let s = source_entity();
        let sp = source_policy_of(&s);
        let mut y = form_values(&source_form_of(&s));
        y.values
            .insert("policy_type".into(), json!("REFRESH_ON_DATA_CHANGES"));
        assert!(wanted(&y, false, Some(&sp), &[]).is_err());
    }

    #[test]
    fn a_source_changes_only_what_changed() {
        let s = source_entity();
        let f = source_form_of(&s);
        assert!(f.notes.iter().any(|n| n.contains("Un 0")));
        let current = source_policy_of(&s);
        let v = form_values(&f);
        // Its zeros (Dremio's defaults) go back untouched.
        let p = wanted(&v, false, Some(&current), &[]).unwrap();
        assert!(source_patch(&current, &p).is_empty());
        let mut v2 = v.clone();
        v2.values.insert("policy_type".into(), json!("NEVER"));
        v2.values.insert("never_expire".into(), json!(true));
        let p = wanted(&v2, false, Some(&current), &[]).unwrap();
        let patch = source_patch(&current, &p);
        assert_eq!(patch["accelerationActivePolicyType"], json!("NEVER"));
        assert_eq!(patch["accelerationNeverRefresh"], json!(true));
        assert_eq!(patch["accelerationNeverExpire"], json!(true));
        assert_eq!(patch.len(), 3);
        let plan = source_put("16d1342a-59ad-4ac6-90e7-615e0123ed68", "src", &s, patch);
        let Step::Rest { body: Some(b), .. } = &plan.steps[0] else {
            panic!()
        };
        assert!(b.get("children").is_none() && b.get("state").is_none());
        assert_eq!(b["config"]["password"], json!("$DREMIO_EXISTING_VALUE$"));
        assert_eq!(b["accelerationNeverRefresh"], json!(true));
        assert!(
            !plan.script.contains("DREMIO_EXISTING_VALUE"),
            "the script shows the changes only"
        );
    }

    #[test]
    fn a_new_table_policy_form() {
        let f = table_form_of(
            None,
            None,
            &[("\"$scratch\".\"p\"".into(), "$scratch.p".into())],
        );
        assert_eq!(f.values["policy_type"], json!("PERIOD"));
        assert_eq!(f.values["refresh_period"], json!(3600));
        assert_eq!(f.values["grace_period"], json!(10800));
        assert_eq!(f.choices["table"].len(), 1);
        let p = wanted(&form_values(&f), true, None, &[]).unwrap();
        assert_eq!(p, default_table_policy());
    }
}

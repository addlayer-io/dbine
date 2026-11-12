//! The service's settings (`GET` / `POST /config`): how big a repository's
//! task history grows before the service rotates it (5 to 200 MiB), and on
//! versions that report it, for how many days.

use super::{bad, Call, Change, Ctx, Vals};
use crate::CbSession;
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use reqwest::Method;
use serde_json::{json, Map, Value};

const ID: &str = "service";
const NAME: &str = "Servicio de backup";

fn check_id(id: &str) -> Result<()> {
    if id.trim() == ID {
        Ok(())
    } else {
        Err(bad(
            "La configuración del servicio de backup tiene un solo objeto: «service».".into(),
        ))
    }
}

pub(crate) fn items(_c: &Ctx) -> Result<SchedulerList> {
    Ok(SchedulerList {
        items: vec![SchedulerItem {
            id: ID.into(),
            name: NAME.into(),
            description: Some("Historial de tareas".into()),
            enabled: None,
            system: true,
        }],
        notes: Vec::new(),
    })
}

pub(crate) fn build_form(cfg: &Value) -> ObjectForm {
    let mut f = ObjectForm::default();
    let mut fields = vec![FormField::new(
        "history_rotation_size",
        "Tamaño del historial de tareas (MiB)",
        FormKind::number(Some(5.0), Some(200.0)),
    )
    .required()
    .help(
        "Cuando el historial de tareas de un repositorio llega a este tamaño, el servicio lo rota.",
    )];
    f.values.insert(
        "history_rotation_size".into(),
        cfg.get("history_rotation_size")
            .cloned()
            .unwrap_or(Value::Null),
    );
    if let Some(p) = cfg.get("history_rotation_period").filter(|p| !p.is_null()) {
        fields.push(
            FormField::new(
                "history_rotation_period",
                "Días del historial de tareas",
                FormKind::number(Some(1.0), Some(365.0)),
            )
            .required()
            .help("Cuántos días de historial guarda antes de rotarlo."),
        );
        f.values.insert("history_rotation_period".into(), p.clone());
    }
    f.sections = vec![FormSection::new("general", "General").fields(fields)];
    f
}

pub(crate) async fn form(s: &CbSession, _c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        return Err(bad(
            "La configuración del servicio de backup no se crea: ya existe.".into(),
        ));
    };
    check_id(id)?;
    Ok(build_form(
        &s.backup_call(Method::GET, "/config", None).await?,
    ))
}

/// The `POST /config` with what changed.
pub(crate) fn config_change(cfg: &Value, values: &FormValues) -> Result<Change> {
    let v = Vals(&values.values);
    let mut body = Map::new();
    for (key, label, min, max) in [
        (
            "history_rotation_size",
            "Tamaño del historial de tareas (MiB)",
            5,
            200,
        ),
        (
            "history_rotation_period",
            "Días del historial de tareas",
            1,
            365,
        ),
    ] {
        let Some(old) = cfg.get(key).filter(|x| !x.is_null()) else {
            continue;
        };
        if let Some(n) = v.int(key, label, min, max)? {
            if old.as_i64() != Some(n) {
                body.insert(key.into(), json!(n));
            }
        }
    }
    if body.is_empty() {
        return Ok(Change::nothing(NAME, ID.into()));
    }
    Ok(Change {
        comments: vec!["Cambia la configuración del servicio de backup.".into()],
        calls: vec![Call::new(
            Method::POST,
            "/config".into(),
            Some(Value::Object(body)),
        )],
        undo: None,
        id: Some(ID.into()),
    })
}

pub(crate) async fn change(s: &CbSession, id: Option<&str>, values: &FormValues) -> Result<Change> {
    let Some(id) = id else {
        return Err(bad(
            "La configuración del servicio de backup no se crea: ya existe.".into(),
        ));
    };
    check_id(id)?;
    let cfg = s.backup_call(Method::GET, "/config", None).await?;
    config_change(&cfg, values)
}

//! The replicator's settings of each node (`[replicator]` in
//! `/_node/{node}/_config`): how many replications run at once and how the
//! scheduler rotates them, the defaults of every replication (workers,
//! batches, connections, checkpoints), and TLS to other servers. A key
//! left empty goes back to CouchDB's default (`DELETE`); one typed is set
//! (`PUT`, the value as a JSON text). Keys DBine doesn't know show
//! read-only. The key file's password is a secret: never shown, kept
//! unless typed.

use super::{bad, shown, Call, Change, Vals};
use crate::jobs;
use crate::{seg, CouchSession};
use dbine_driver::{
    FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
    SchedulerList, SECRET_MASK,
};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Number,
    Flag,
    Text,
    Secret,
}

/// The keys DBine shows: key, type, section.
const KEYS: &[(&str, Ty, &str)] = &[
    ("max_jobs", Ty::Number, "scheduler"),
    ("interval", Ty::Number, "scheduler"),
    ("max_churn", Ty::Number, "scheduler"),
    ("max_history", Ty::Number, "scheduler"),
    ("startup_jitter", Ty::Number, "scheduler"),
    ("cluster_quiet_period", Ty::Number, "scheduler"),
    ("cluster_start_period", Ty::Number, "scheduler"),
    ("worker_processes", Ty::Number, "defaults"),
    ("worker_batch_size", Ty::Number, "defaults"),
    ("http_connections", Ty::Number, "defaults"),
    ("connection_timeout", Ty::Number, "defaults"),
    ("retries_per_request", Ty::Number, "defaults"),
    ("checkpoint_interval", Ty::Number, "defaults"),
    ("use_checkpoints", Ty::Flag, "defaults"),
    ("use_bulk_get", Ty::Flag, "defaults"),
    ("socket_options", Ty::Text, "defaults"),
    ("verify_ssl_certificates", Ty::Flag, "tls"),
    ("ssl_trusted_certificates_file", Ty::Text, "tls"),
    ("ssl_certificate_max_depth", Ty::Number, "tls"),
    ("cert_file", Ty::Text, "tls"),
    ("key_file", Ty::Text, "tls"),
    ("password", Ty::Secret, "tls"),
];

fn flag_kind() -> FormKind {
    FormKind::select(&[("", "Por defecto"), ("true", "Sí"), ("false", "No")])
}

fn fields(section: &str) -> Vec<FormField> {
    let n = |min: f64| FormKind::number(Some(min), None);
    let all = vec![
        FormField::new("max_jobs", "Replicaciones en marcha a la vez", n(1.0))
            .help("Vacío: 500. Las demás esperan su turno."),
        FormField::new("interval", "Intervalo del planificador (ms)", n(1000.0))
            .help("Vacío: 60000. Cada cuánto rota las replicaciones que esperan."),
        FormField::new("max_churn", "Arranques por intervalo", n(1.0))
            .help("Vacío: 20. Cuántas replicaciones arranca o detiene en cada intervalo."),
        FormField::new("max_history", "Eventos guardados por replicación", n(1.0))
            .help("Vacío: 20. El historial que muestra cada replicación."),
        FormField::new("startup_jitter", "Espera al azar al arrancar (ms)", n(0.0))
            .help("Vacío: 5000."),
        FormField::new("cluster_quiet_period", "Calma del cluster (s)", n(0.0))
            .help("Vacío: 60. Lo que espera sin cambios de nodos antes de repartir replicaciones."),
        FormField::new("cluster_start_period", "Arranque del cluster (s)", n(0.0))
            .help("Vacío: 5."),
        FormField::new("worker_processes", "Procesos", n(1.0)).help("Vacío: 4."),
        FormField::new("worker_batch_size", "Documentos por lote", n(1.0)).help("Vacío: 500."),
        FormField::new("http_connections", "Conexiones HTTP", n(1.0)).help("Vacío: 20."),
        FormField::new(
            "connection_timeout",
            "Tiempo de espera de conexión (ms)",
            n(1000.0),
        )
        .help("Vacío: 30000."),
        FormField::new("retries_per_request", "Reintentos por pedido", n(0.0)).help("Vacío: 5."),
        FormField::new(
            "checkpoint_interval",
            "Intervalo de checkpoint (ms)",
            n(1000.0),
        )
        .help("Vacío: 30000."),
        FormField::new("use_checkpoints", "Usar checkpoints", flag_kind()),
        FormField::new("use_bulk_get", "Usar _bulk_get", flag_kind()),
        FormField::new("socket_options", "Opciones de socket", FormKind::Text)
            .placeholder("[{keepalive, true}, {nodelay, false}]"),
        FormField::new(
            "verify_ssl_certificates",
            "Verificar los certificados",
            flag_kind(),
        )
        .help("Por defecto no se verifican los certificados de los otros servidores."),
        FormField::new(
            "ssl_trusted_certificates_file",
            "Certificados de confianza",
            FormKind::Text,
        )
        .placeholder("/etc/ssl/certs/ca-certificates.crt"),
        FormField::new(
            "ssl_certificate_max_depth",
            "Profundidad de la cadena",
            n(1.0),
        )
        .help("Vacío: 3."),
        FormField::new("cert_file", "Certificado del cliente", FormKind::Text),
        FormField::new("key_file", "Clave del certificado", FormKind::Text),
        FormField::new("password", "Contraseña de la clave", FormKind::Secret)
            .help("Vacía: queda la que tiene."),
    ];
    all.into_iter()
        .filter(|f| {
            KEYS.iter()
                .any(|(k, _, s)| *k == f.key.as_str() && *s == section)
        })
        .collect()
}

fn ty(key: &str) -> Option<Ty> {
    KEYS.iter().find(|(k, ..)| *k == key).map(|(_, t, _)| *t)
}

/// The nodes of the cluster (`/_membership`).
async fn nodes(s: &CouchSession) -> Result<Vec<String>> {
    let v = s.call(Method::GET, "/_membership", None).await?;
    Ok(v.get("all_nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect())
}

async fn node(s: &CouchSession, id: &str) -> Result<String> {
    let n = id.trim();
    if nodes(s).await?.iter().any(|x| x == n) {
        Ok(n.to_string())
    } else {
        Err(bad(format!(
            "«{}» no es un nodo de este servidor.",
            shown(id)
        )))
    }
}

fn path(node: &str) -> String {
    format!("/_node/{}/_config/replicator", seg(node))
}

async fn config(s: &CouchSession, node: &str) -> Result<Map<String, Value>> {
    let v = s.call(Method::GET, &path(node), None).await?;
    Ok(v.as_object().cloned().unwrap_or_default())
}

pub(crate) async fn items(s: &CouchSession) -> Result<SchedulerList> {
    Ok(SchedulerList {
        items: nodes(s)
            .await?
            .into_iter()
            .map(|n| SchedulerItem {
                id: n.clone(),
                name: n,
                description: Some("Replicador".into()),
                enabled: None,
                system: true,
                can_edit: None,
                can_delete: None,
                actions: None,
                run_from_step: None,
                clear_history: None,
            })
            .collect(),
        notes: Vec::new(),
    })
}

/// The form of a node's settings (`cfg`, its `[replicator]` section).
pub(crate) fn build_form(node: &str, cfg: &Map<String, Value>) -> ObjectForm {
    let mut f = ObjectForm::default();
    for (key, t, _) in KEYS {
        let raw = cfg.get(*key).and_then(Value::as_str);
        let v = match (t, raw) {
            (_, None) => match t {
                Ty::Flag | Ty::Text => json!(""),
                _ => Value::Null,
            },
            (Ty::Secret, Some(_)) => Value::Null,
            (Ty::Number, Some(r)) => match r.trim().parse::<i64>() {
                Ok(n) => json!(n),
                Err(_) => {
                    f.notes.push(format!(
                        "«{key}» vale «{}», que no es un número: queda como está si el campo sigue vacío.",
                        shown(r)
                    ));
                    Value::Null
                }
            },
            (Ty::Flag, Some(r)) => json!(r.trim()),
            (Ty::Text, Some(r)) => json!(r),
        };
        f.values.insert((*key).into(), v);
    }
    let others: Vec<BTreeMap<String, Value>> = cfg
        .iter()
        .filter(|(k, _)| ty(k).is_none())
        .map(|(k, v)| {
            let shown_value = if jobs::secret_key(k) {
                json!(SECRET_MASK)
            } else {
                v.clone()
            };
            BTreeMap::from([
                ("key".to_string(), json!(k)),
                ("value".to_string(), shown_value),
            ])
        })
        .collect();
    f.sections = vec![
        FormSection::new("scheduler", "Planificador").fields(fields("scheduler")),
        FormSection::new("defaults", "Valores por defecto").fields(fields("defaults")),
        FormSection::new("tls", "TLS").fields(fields("tls")),
    ];
    if !others.is_empty() {
        f.sections.push(
            FormSection::new("others", "Otras claves").list(
                FormList::new(
                    "others",
                    "Otras claves",
                    "key",
                    vec![
                        FormField::new("key", "Clave", FormKind::Text).read_only(true),
                        FormField::new("value", "Valor", FormKind::Text).read_only(true),
                    ],
                )
                .rows(others)
                .read_only(true),
            ),
        );
    }
    f.notes.push(format!(
        "La sección [replicator] del nodo «{node}»: cada nodo tiene la suya. Un campo vacío vuelve al valor por defecto de CouchDB."
    ));
    f
}

pub(crate) async fn form(s: &CouchSession, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        return Err(bad(
            "La configuración del replicador no se crea: cada nodo tiene la suya.".into(),
        ));
    };
    let n = node(s, id).await?;
    Ok(build_form(&n, &config(s, &n).await?))
}

/// The `PUT` / `DELETE` of each key that changed.
pub(crate) fn config_change(
    node: &str,
    cfg: &Map<String, Value>,
    values: &FormValues,
) -> Result<Change> {
    let v = Vals(&values.values);
    let mut calls = Vec::new();
    let mut changed = Vec::new();
    for (key, t, _) in KEYS {
        let old = cfg.get(*key).and_then(Value::as_str);
        let new: Option<Option<String>> = match t {
            Ty::Number => match v.int(key, key, 0, i64::from(i32::MAX)) {
                Ok(Some(n)) => Some(Some(n.to_string())),
                // A value DBine couldn't show stays.
                Ok(None) if old.is_some_and(|o| o.trim().parse::<i64>().is_err()) => None,
                Ok(None) => Some(None),
                Err(e) => return Err(e),
            },
            Ty::Flag => match v.text(key).as_str() {
                "" => Some(None),
                "true" => Some(Some("true".into())),
                "false" => Some(Some("false".into())),
                other => {
                    return Err(bad(format!(
                        "«{key}» va «true», «false» o vacío (es «{}»).",
                        shown(other)
                    )))
                }
            },
            Ty::Text => Some(v.opt(key)),
            // Kept unless typed.
            Ty::Secret => v.secret(key).map(Some),
        };
        let Some(new) = new else { continue };
        if let Some(x) = &new {
            if x.chars().any(char::is_control) {
                return Err(bad(format!("«{key}» no puede tener saltos de línea.")));
            }
        }
        if new.as_deref() == old && *t != Ty::Secret {
            continue;
        }
        let key_path = format!("{}/{}", path(node), seg(key));
        match new {
            Some(x) => {
                calls.push(Call {
                    method: Method::PUT,
                    path: key_path,
                    body: Some(Value::String(x)),
                    secret: *t == Ty::Secret,
                });
                changed.push(*key);
            }
            None if old.is_some() => {
                calls.push(Call::new(Method::DELETE, key_path, None));
                changed.push(*key);
            }
            None => {}
        }
    }
    if calls.is_empty() {
        return Ok(Change::nothing(node, node.to_string()));
    }
    Ok(Change {
        comments: vec![format!(
            "Cambia la configuración del replicador del nodo «{node}»: {}.",
            changed.join(", ")
        )],
        calls,
        id: Some(node.to_string()),
        id_from_reply: None,
    })
}

pub(crate) async fn change(
    s: &CouchSession,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Change> {
    let Some(id) = id else {
        return Err(bad(
            "La configuración del replicador no se crea: cada nodo tiene la suya.".into(),
        ));
    };
    let n = node(s, id).await?;
    let cfg = config(s, &n).await?;
    config_change(&n, &cfg, values)
}

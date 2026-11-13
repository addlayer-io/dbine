//! Repositories: where a plan's backups of a bucket (or of every bucket)
//! go. Active ones run their plan; archived ones (an active repository
//! archived, for good) and imported ones (an archive registered again, from
//! this cluster or another) only keep backups to restore or examine.
//!
//! - Active (`job`): `POST …/active/{id}` creates one with its plan,
//!   bucket, storage (a directory of the backup nodes, or S3, Azure Blob
//!   Storage or Google Cloud Storage with a local staging directory),
//!   users in the backups, retention and encryption (8.0); an edit changes
//!   its plan (`…/plan/{plan}`, paused around it as the service requires),
//!   its state (`…/pause`, `…/resume`) and its retention
//!   (`…/retentionSettings`). The rest is fixed at creation. A delete
//!   archives it and deletes it from the service, its backups kept.
//! - Archived: created by archiving an active repository
//!   (`…/active/{id}/archive`); read-only; deleted from the service.
//! - Imported: `POST …/repository/import`; read-only; deleted from the
//!   service.

use super::plan::{self, plan_of, Task};
use super::{bad, comment_text, new_name, shown, Call, Change, Ctx, Vals, SECRET_KEYS_NOTE};
use crate::jobs::{self, job_id, repo_name, s as field, schedule_text, utc};
use crate::{encode, CbSession};
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const ACTIVE: &str = "/cluster/self/repository/active";
/// Longest retention the console takes, in days.
const MAX_RETENTION: i64 = 36_500;

const ARCHIVED_NOTE: &str = "Un repositorio archivado no hace backups: guarda los que tiene para restaurarlos o examinarlos desde la consola de Couchbase. No se puede cambiar.";
const IMPORTED_NOTE: &str = "Un repositorio importado no hace backups: registra un archivo (de este cluster o de otro) para restaurar o examinar sus backups. No se puede cambiar.";
const ARCHIVE_FINAL: &str =
    "Archivar es definitivo: el repositorio deja de hacer backups y no vuelve a estar activo.";
const FIXED_NOTE: &str = "El bucket, el almacenamiento, los usuarios y el cifrado de un repositorio se eligen al crearlo: el servicio no los cambia después.";
const NO_PLAN_CHANGE: &str = "Couchbase Server anterior a 8.0 no cambia el plan de un repositorio: para otras tareas, creá otro repositorio con el plan nuevo.";
const NO_HISTORY_CLEAR: &str = "El servicio de backup de Couchbase no borra el historial de tareas de un repositorio: lo rota solo, por tamaño (ver la configuración del servicio).";

/// Where a repository that isn't active is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stored {
    Archived,
    Imported,
}

impl Stored {
    fn state(self) -> &'static str {
        match self {
            Stored::Archived => "archived",
            Stored::Imported => "imported",
        }
    }
}

fn stored_id(id: &str) -> Result<String> {
    let t = id.trim();
    if repo_name(t) {
        Ok(t.to_string())
    } else {
        Err(bad(format!(
            "«{}» no es un id de repositorio de backup de Couchbase",
            shown(id)
        )))
    }
}

fn not_found(name: &str) -> Error {
    Error::Query(format!("no existe el repositorio de backup «{name}»"))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

async fn list(s: &CbSession, state: &str) -> Result<Vec<Value>> {
    let v = s
        .backup_call(
            Method::GET,
            &format!("/cluster/self/repository/{state}"),
            None,
        )
        .await?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

async fn names(s: &CbSession, state: &str) -> Result<Vec<String>> {
    Ok(list(s, state)
        .await?
        .iter()
        .filter_map(|r| field(r, "id"))
        .collect())
}

async fn stored(s: &CbSession, kind: Stored, name: &str) -> Result<Value> {
    s.backup_call(
        Method::GET,
        &format!("/cluster/self/repository/{}/{}", kind.state(), encode(name)),
        None,
    )
    .await
    .map_err(|e| match e {
        Error::Query(m) if m.contains("not found") || m.contains("HTTP 404") => not_found(name),
        e => e,
    })
}

/// The repository's retention in days (8.0), from `…/info`.
async fn retention(s: &CbSession, name: &str) -> Option<i64> {
    match s
        .backup_call(
            Method::GET,
            &format!("{ACTIVE}/{}/info?limit=1&offset=0", encode(name)),
            None,
        )
        .await
    {
        Ok(v) => v.get("default_retention").and_then(Value::as_i64),
        Err(e) => {
            tracing::debug!("couchbase backup: retention of {name}: {e}");
            None
        }
    }
}

/// The buckets a repository may back up (memcached ones have no data to).
async fn buckets(s: &CbSession) -> Result<Vec<String>> {
    let v = s.conn.mgmt_get("/pools/default/buckets").await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|b| field(b, "bucketType").as_deref() != Some("memcached"))
        .filter_map(|b| field(b, "name"))
        .collect())
}

fn item(r: &Value, id: String, enabled: Option<bool>) -> Option<SchedulerItem> {
    let name = field(r, "id")?;
    let plan = field(r, "plan_name");
    let bucket = r.pointer("/bucket/name").and_then(Value::as_str);
    Some(SchedulerItem {
        id,
        name,
        description: match (plan, bucket) {
            (Some(p), Some(b)) => Some(format!("{p} · {b}")),
            (Some(p), None) => Some(p),
            (None, Some(b)) => Some(b.to_string()),
            (None, None) => None,
        },
        enabled,
        system: false,
        can_edit: None,
        can_delete: None,
        actions: None,
    })
}

/// `write`: the user may change the service, so the actions are offered.
pub(crate) async fn active_items(s: &CbSession, write: bool) -> Result<SchedulerList> {
    let items = list(s, "active")
        .await?
        .iter()
        .filter_map(|r| {
            let name = field(r, "id").filter(|n| repo_name(n))?;
            let mut it = item(
                r,
                job_id(&name),
                Some(field(r, "state").as_deref() != Some("paused")),
            )?;
            if write {
                it.actions = Some(repo_actions(r));
            }
            Some(it)
        })
        .collect();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

pub(crate) async fn stored_items(s: &CbSession, kind: Stored) -> Result<SchedulerList> {
    let items = list(s, kind.state())
        .await?
        .iter()
        .filter_map(|r| {
            let name = field(r, "id").filter(|n| repo_name(n))?;
            item(r, name, None)
        })
        .collect();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Storage: a directory, or a cloud bucket with a staging directory
// ---------------------------------------------------------------------------

const PROVIDERS: &[&str] = &["s3", "az", "gs"];

/// An archive location as the form shows it: `(storage, directory, cloud
/// bucket, prefix)`.
pub(crate) fn split_archive(archive: &str) -> (String, String, String, String) {
    for p in PROVIDERS {
        if let Some(rest) = archive.strip_prefix(&format!("{p}://")) {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            return (
                p.to_string(),
                String::new(),
                bucket.to_string(),
                prefix.to_string(),
            );
        }
    }
    (
        "filesystem".into(),
        archive.to_string(),
        String::new(),
        String::new(),
    )
}

fn clean(v: &str, label: &str) -> Result<String> {
    if v.chars().any(char::is_control) {
        return Err(bad(format!(
            "«{label}» no puede tener saltos de línea ni caracteres de control."
        )));
    }
    if v.chars().count() > 1024 {
        return Err(bad(format!("«{label}» es demasiado largo.")));
    }
    Ok(v.to_string())
}

fn put_opt(body: &mut Map<String, Value>, key: &str, v: Option<String>, label: &str) -> Result<()> {
    if let Some(v) = v {
        body.insert(key.into(), json!(clean(&v, label)?));
    }
    Ok(())
}

/// The request's storage fields (`archive`, `cloud_*`) from the form, and
/// the secrets among them.
pub(crate) fn storage_body(
    c: &Ctx,
    v: Vals,
    body: &mut Map<String, Value>,
    secrets: &mut Vec<&'static str>,
) -> Result<()> {
    let storage = v.text("storage");
    match storage.as_str() {
        "" | "filesystem" => {
            let dir = v
                .opt("archive")
                .ok_or_else(|| bad("El repositorio necesita el directorio del archivo.".into()))?;
            body.insert("archive".into(), json!(clean(&dir, "Directorio")?));
        }
        p if PROVIDERS.contains(&p) => {
            let mut bucket = v
                .opt("cloud_bucket")
                .ok_or_else(|| bad("El repositorio en la nube necesita el bucket o contenedor.".into()))?;
            if let Some(rest) = bucket.strip_prefix(&format!("{p}://")) {
                bucket = rest.to_string();
            }
            let bucket = bucket.trim_matches('/').to_string();
            if bucket.is_empty() || bucket.contains('/') || bucket.contains("://") {
                return Err(bad(format!(
                    "«{}» no es un nombre de bucket o contenedor (sin barras: la ruta va en «Prefijo»).",
                    shown(&bucket)
                )));
            }
            let prefix = v.text("path_prefix").trim_matches('/').to_string();
            body.insert(
                "archive".into(),
                json!(clean(&format!("{p}://{bucket}/{prefix}"), "Prefijo")?),
            );
            let staging = v.opt("staging_dir").ok_or_else(|| {
                bad("El repositorio en la nube necesita un directorio de preparación local.".into())
            })?;
            body.insert(
                "cloud_staging_dir".into(),
                json!(clean(&staging, "Directorio de preparación")?),
            );
            put_opt(body, "cloud_region", v.opt("region"), "Región")?;
            put_opt(body, "cloud_endpoint", v.opt("endpoint"), "Endpoint")?;
            if p == "s3" && v.bool("force_path_style") {
                body.insert("cloud_force_path_style".into(), json!(true));
            }
            match v.text("auth_type").as_str() {
                "" | "key" => {
                    if c.v8() {
                        body.insert("cloud_auth_type".into(), json!(0));
                    }
                    let id = v.opt("credentials_id").ok_or_else(|| {
                        bad("Falta el ID de la clave de acceso a la nube.".into())
                    })?;
                    body.insert(
                        "cloud_credentials_id".into(),
                        json!(clean(&id, "ID de la clave de acceso")?),
                    );
                    let key = v
                        .secret("credentials_key")
                        .ok_or_else(|| bad("Falta la clave secreta de la nube.".into()))?;
                    body.insert("cloud_credentials_key".into(), json!(key));
                    secrets.push("cloud_credentials_key");
                    if let Some(t) = v.secret("refresh_token") {
                        body.insert("cloud_credentials_refresh_token".into(), json!(t));
                        secrets.push("cloud_credentials_refresh_token");
                    }
                }
                "instance" => {
                    if c.v8() {
                        body.insert("cloud_auth_type".into(), json!(1));
                    }
                    if p == "az" {
                        put_opt(
                            body,
                            "cloud_credentials_id",
                            v.opt("storage_account"),
                            "Cuenta de almacenamiento",
                        )?;
                    }
                }
                other => {
                    return Err(bad(format!(
                        "«{}» no es una forma de acceder a la nube.",
                        shown(other)
                    )))
                }
            }
        }
        other => {
            return Err(bad(format!(
                "«{}» no es un almacenamiento de repositorio (sistema de archivos, S3, Azure o Google Cloud).",
                shown(other)
            )))
        }
    }
    Ok(())
}

/// The request's encryption fields (8.0) from the form, and its secrets.
pub(crate) fn encryption_body(
    c: &Ctx,
    v: Vals,
    body: &mut Map<String, Value>,
    secrets: &mut Vec<&'static str>,
) -> Result<()> {
    if !v.bool("encrypted") {
        return Ok(());
    }
    if !c.v8() {
        return Err(bad(
            "Los repositorios cifrados son de Couchbase Server 8.0 en adelante.".into(),
        ));
    }
    let url = v
        .opt("key_url")
        .ok_or_else(|| bad("Un repositorio cifrado necesita la URL de su clave.".into()))?;
    body.insert(
        "encryption_key_url".into(),
        json!(clean(&url, "URL de la clave")?),
    );
    let auth = match v.text("km_auth_type").as_str() {
        "" | "environment" => "environment",
        "file" => "file",
        "credentials" => "credentials",
        other => {
            return Err(bad(format!(
                "«{}» no es una forma de acceder al servicio de claves.",
                shown(other)
            )))
        }
    };
    body.insert("km_auth_type".into(), json!(auth));
    put_opt(
        body,
        "km_region",
        v.opt("km_region"),
        "Región del servicio de claves",
    )?;
    put_opt(
        body,
        "km_endpoint",
        v.opt("km_endpoint"),
        "Endpoint del servicio de claves",
    )?;
    match auth {
        "file" => {
            let f = v.opt("km_access_file").ok_or_else(|| {
                bad("Falta el archivo de credenciales del servicio de claves.".into())
            })?;
            body.insert(
                "km_access_file".into(),
                json!(clean(&f, "Archivo de credenciales")?),
            );
        }
        "credentials" => {
            let id = v
                .opt("km_credentials_id")
                .ok_or_else(|| bad("Falta el ID de acceso al servicio de claves.".into()))?;
            body.insert(
                "km_credentials_id".into(),
                json!(clean(&id, "ID de acceso al servicio de claves")?),
            );
            let key = v
                .secret("km_credentials_key")
                .ok_or_else(|| bad("Falta la clave del servicio de claves.".into()))?;
            body.insert("km_credentials_key".into(), json!(key));
            secrets.push("km_credentials_key");
            put_opt(body, "km_tenant_id", v.opt("km_tenant_id"), "Tenant")?;
        }
        _ => {}
    }
    Ok(())
}

fn storage_fields(existing: bool) -> Vec<FormField> {
    let cloud = &["s3", "az", "gs"];
    vec![
        FormField::new(
            "storage",
            "Almacenamiento",
            FormKind::select(&[
                ("filesystem", "Directorio de los nodos de backup"),
                ("s3", "Amazon S3 (o compatible)"),
                ("az", "Azure Blob Storage"),
                ("gs", "Google Cloud Storage"),
            ]),
        )
        .required()
        .read_only(existing)
        .default(json!("filesystem")),
        FormField::new("archive", "Directorio", FormKind::Text)
            .when("storage", &["filesystem"])
            .read_only(existing)
            .placeholder("/data/backups")
            .help("Un directorio que todos los nodos de backup comparten, con permiso de escritura para el usuario couchbase."),
        FormField::new("cloud_bucket", "Bucket o contenedor", FormKind::Text)
            .when("storage", cloud)
            .read_only(existing),
        FormField::new("path_prefix", "Prefijo", FormKind::Text)
            .when("storage", cloud)
            .read_only(existing)
            .help("La ruta del archivo dentro del bucket."),
        FormField::new(
            "auth_type",
            "Credenciales",
            FormKind::select(&[
                ("key", "ID y clave de acceso"),
                ("instance", "Las de la instancia (servicio de metadatos)"),
            ]),
        )
        .when("storage", cloud)
        .read_only(existing)
        .default(json!("key")),
        FormField::new("credentials_id", "ID de la clave de acceso", FormKind::Text)
            .when("auth_type", &["key"])
            .read_only(existing),
        FormField::new("credentials_key", "Clave secreta", FormKind::Secret)
            .when("auth_type", &["key"])
            .read_only(existing),
        FormField::new("refresh_token", "Token de actualización", FormKind::Secret)
            .when("auth_type", &["key"])
            .read_only(existing)
            .help("Solo Google Cloud Storage con OAuth."),
        FormField::new("storage_account", "Cuenta de almacenamiento", FormKind::Text)
            .when("auth_type", &["instance"])
            .read_only(existing)
            .help("Solo Azure: la cuenta de almacenamiento del contenedor."),
        FormField::new("region", "Región", FormKind::Text)
            .when("storage", cloud)
            .read_only(existing),
        FormField::new("endpoint", "Endpoint", FormKind::Text)
            .when("storage", cloud)
            .read_only(existing)
            .help("Para un almacenamiento compatible con S3 o un endpoint propio."),
        FormField::new("force_path_style", "Direcciones con estilo de ruta", FormKind::Bool)
            .when("storage", &["s3"])
            .read_only(existing)
            .default(json!(false))
            .help("Para almacenamientos compatibles con S3 que no aceptan el bucket en el nombre del host."),
        FormField::new("staging_dir", "Directorio de preparación", FormKind::Text)
            .when("storage", cloud)
            .read_only(existing)
            .placeholder("/data/staging")
            .help("Un directorio local de cada nodo de backup, donde se prepara lo que sube a la nube."),
    ]
}

fn encryption_fields(existing: bool) -> Vec<FormField> {
    vec![
        FormField::new("encrypted", "Cifrar los backups", FormKind::Bool)
            .read_only(existing)
            .default(json!(false)),
        FormField::new("key_url", "URL de la clave", FormKind::Text)
            .when("encrypted", &["true"])
            .read_only(existing)
            .placeholder("awskms://alias/backups")
            .help("La clave del servicio de claves (AWS KMS, Azure Key Vault, GCP KMS o un archivo local)."),
        FormField::new(
            "km_auth_type",
            "Acceso al servicio de claves",
            FormKind::select(&[
                ("environment", "Las credenciales del entorno"),
                ("file", "Un archivo de credenciales"),
                ("credentials", "ID y clave"),
            ]),
        )
        .when("encrypted", &["true"])
        .read_only(existing)
        .default(json!("environment")),
        FormField::new("km_region", "Región del servicio de claves", FormKind::Text)
            .when("encrypted", &["true"])
            .read_only(existing),
        FormField::new("km_endpoint", "Endpoint del servicio de claves", FormKind::Text)
            .when("encrypted", &["true"])
            .read_only(existing),
        FormField::new("km_access_file", "Archivo de credenciales", FormKind::Text)
            .when("km_auth_type", &["file"])
            .read_only(existing),
        FormField::new("km_credentials_id", "ID de acceso al servicio de claves", FormKind::Text)
            .when("km_auth_type", &["credentials"])
            .read_only(existing),
        FormField::new("km_credentials_key", "Clave del servicio de claves", FormKind::Secret)
            .when("km_auth_type", &["credentials"])
            .read_only(existing),
        FormField::new("km_tenant_id", "Tenant", FormKind::Text)
            .when("km_auth_type", &["credentials"])
            .read_only(existing)
            .help("Solo Azure Key Vault."),
    ]
}

/// The storage values of an existing repository.
fn storage_values(r: &Value, values: &mut BTreeMap<String, Value>) {
    let archive = field(r, "archive").unwrap_or_default();
    let (storage, dir, bucket, prefix) = split_archive(&archive);
    values.insert("storage".into(), json!(storage));
    values.insert("archive".into(), json!(dir));
    values.insert("cloud_bucket".into(), json!(bucket));
    values.insert("path_prefix".into(), json!(prefix));
    let cloud = r.get("cloud_info");
    let c = |k: &str| cloud.and_then(|x| field(x, k)).unwrap_or_default();
    values.insert("region".into(), json!(c("region")));
    values.insert("endpoint".into(), json!(c("endpoint")));
    values.insert("staging_dir".into(), json!(c("staging_dir")));
    values.insert(
        "force_path_style".into(),
        json!(cloud
            .and_then(|x| x.get("force_path_style"))
            .and_then(Value::as_bool)
            .unwrap_or(false)),
    );
    values.insert(
        "credentials_name".into(),
        json!(c("cloud_credentials_name")),
    );
    values.insert("credentials_key".into(), Value::Null);
    values.insert("refresh_token".into(), Value::Null);
}

/// The fields every existing repository shows, read-only.
fn info_section(r: &Value, values: &mut BTreeMap<String, Value>) -> FormSection {
    values.insert("uuid".into(), json!(field(r, "repo").unwrap_or_default()));
    let health = match r.pointer("/health/healthy").and_then(Value::as_bool) {
        Some(true) => "Sano".to_string(),
        Some(false) => format!(
            "Con problemas: {}",
            r.pointer("/health/issue")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ),
        None => String::new(),
    };
    values.insert("health".into(), json!(health));
    values.insert(
        "created".into(),
        json!(utc(field(r, "creation_time")).unwrap_or_default()),
    );
    values.insert(
        "updated".into(),
        json!(utc(field(r, "update_time")).unwrap_or_default()),
    );
    FormSection::new("info", "Información").fields(vec![
        FormField::new("uuid", "UUID del repositorio", FormKind::Text).read_only(true),
        FormField::new("health", "Salud", FormKind::Text).read_only(true),
        FormField::new("created", "Creado", FormKind::DateTime).read_only(true),
        FormField::new("updated", "Actualizado", FormKind::DateTime).read_only(true),
    ])
}

fn plan_choices(plans: &[plan::PlanDef]) -> Vec<(String, String)> {
    plans
        .iter()
        .map(|p| {
            let label = if p.description.is_empty() {
                p.name.clone()
            } else {
                format!(
                    "{} — {}",
                    p.name,
                    p.description.chars().take(80).collect::<String>()
                )
            };
            (p.name.clone(), label)
        })
        .collect()
}

fn pairs(names: Vec<String>) -> Vec<(String, String)> {
    names.into_iter().map(|n| (n.clone(), n)).collect()
}

// ---------------------------------------------------------------------------
// Active repositories: the form
// ---------------------------------------------------------------------------

fn task_list(tasks: &[Task], plan: Option<&Value>, repo: &Value) -> FormList {
    let next: BTreeMap<String, String> = repo
        .get("scheduled")
        .and_then(Value::as_object)
        .map(|m| {
            m.values()
                .filter_map(|t| Some((field(t, "name")?, utc(field(t, "next_run"))?)))
                .collect()
        })
        .unwrap_or_default();
    let raw: Vec<&Value> = plan
        .and_then(|p| p.get("tasks"))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let rows = tasks
        .iter()
        .map(|t| {
            let schedule = raw
                .iter()
                .find(|r| field(r, "name").as_deref() == Some(t.name.as_str()))
                .and_then(|r| schedule_text(r))
                .unwrap_or_default();
            BTreeMap::from([
                ("name".to_string(), json!(t.name)),
                ("task_type".to_string(), json!(t.kind)),
                ("full_backup".to_string(), json!(t.full_backup)),
                ("schedule".to_string(), json!(schedule)),
                (
                    "next_run".to_string(),
                    next.get(&t.name).map_or(Value::Null, |n| json!(n)),
                ),
            ])
        })
        .collect();
    FormList::new(
        "tasks",
        "Tareas del plan",
        "name",
        vec![
            FormField::new("name", "Tarea", FormKind::Text).read_only(true),
            FormField::new("task_type", "Tipo", FormKind::Text).read_only(true),
            FormField::new("full_backup", "Backup completo", FormKind::Bool).read_only(true),
            FormField::new("schedule", "Programación", FormKind::Text).read_only(true),
            FormField::new("next_run", "Próxima ejecución", FormKind::DateTime).read_only(true),
        ],
    )
    .rows(rows)
    .read_only(true)
}

fn general_fields(c: &Ctx, existing: bool) -> Vec<FormField> {
    let mut out = vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing)
            .placeholder("ventas_diario")
            .help("Letras, dígitos, guion y guion bajo, hasta 50; empieza con letra o dígito. No se puede cambiar después."),
        FormField::new("plan", "Plan", FormKind::reference("plan", false))
            .required()
            .read_only(existing && !c.v8()),
        FormField::new(
            "state",
            "Estado",
            FormKind::select(&[("active", "Activo"), ("paused", "En pausa")]),
        )
        .required()
        .default(json!("active"))
        .help("En pausa no corren las tareas programadas; los backups a pedido sí."),
        FormField::new("bucket", "Bucket", FormKind::reference("bucket", false))
            .read_only(existing)
            .help("Vacío: todos los buckets del cluster."),
    ];
    if c.v8() {
        if !existing {
            out.push(
                FormField::new("enable_users", "Incluir usuarios y grupos", FormKind::Bool)
                    .default(json!(false))
                    .help("Copia los usuarios y grupos con sus roles y contraseñas cifradas. Hace falta ser Full Admin, Local User Admin y External User Admin."),
            );
        }
        out.push(
            FormField::new(
                "retention",
                "Retención (días)",
                FormKind::number(Some(0.0), Some(MAX_RETENTION as f64)),
            )
            .default(json!(0))
            .help("Cuántos días se guarda cada backup; 0: sin vencimiento. Los vencidos los borra una tarea Prune del plan."),
        );
        if existing {
            out.push(
                FormField::new(
                    "retroactive",
                    "Aplicar la retención a los backups que ya hay",
                    FormKind::Bool,
                )
                .default(json!(false)),
            );
        }
    }
    out
}

pub(crate) async fn active_form(s: &CbSession, c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let plans = plan::all(s).await?;
    let mut f = ObjectForm::default();
    f.choices.insert("plan".into(), plan_choices(&plans));
    f.choices
        .insert("bucket".into(), pairs(buckets(s).await.unwrap_or_default()));
    let Some(id) = id else {
        f.sections = vec![
            FormSection::new("general", "General").fields(general_fields(c, false)),
            FormSection::new("storage", "Almacenamiento").fields(storage_fields(false)),
        ];
        if c.v8() {
            f.sections
                .push(FormSection::new("encryption", "Cifrado").fields(encryption_fields(false)));
        }
        let default_plan = plans
            .iter()
            .find(|p| p.default)
            .or(plans.first())
            .map(|p| p.name.clone());
        f.values.insert("name".into(), json!(""));
        f.values
            .insert("plan".into(), default_plan.map_or(Value::Null, Value::from));
        f.values.insert("state".into(), json!("active"));
        f.values.insert("bucket".into(), Value::Null);
        f.values.insert("storage".into(), json!("filesystem"));
        f.notes.push(SECRET_KEYS_NOTE.into());
        return Ok(f);
    };
    let name = jobs::parse_id(id)?;
    let r = s.repo(&name).await?;
    let plan_name = field(&r, "plan_name").unwrap_or_default();
    let plan_raw = if plan_name.is_empty() {
        None
    } else {
        s.plan(&plan_name).await
    };
    let tasks = plan_raw
        .as_ref()
        .and_then(plan_of)
        .map(|p| p.tasks)
        .unwrap_or_default();
    f.values.insert("name".into(), json!(name));
    f.values.insert("plan".into(), json!(plan_name));
    f.values.insert(
        "state".into(),
        json!(if field(&r, "state").as_deref() == Some("paused") {
            "paused"
        } else {
            "active"
        }),
    );
    f.values.insert(
        "bucket".into(),
        r.pointer("/bucket/name")
            .and_then(Value::as_str)
            .map_or(Value::Null, Value::from),
    );
    storage_values(&r, &mut f.values);
    if c.v8() {
        let ret = retention(s, &name).await;
        f.values
            .insert("retention".into(), ret.map_or(Value::Null, Value::from));
        f.values.insert("retroactive".into(), json!(false));
    }
    let mut storage = storage_fields(true);
    storage.push(
        FormField::new("credentials_name", "Credenciales guardadas", FormKind::Text)
            .when("storage", &["s3", "az", "gs"])
            .read_only(true),
    );
    let info = info_section(&r, &mut f.values);
    f.sections = vec![
        FormSection::new("general", "General").fields(general_fields(c, true)),
        FormSection::new("storage", "Almacenamiento").fields(storage),
        FormSection::new("tasks", "Tareas").list(task_list(&tasks, plan_raw.as_ref(), &r)),
        info,
    ];
    f.notes.push(FIXED_NOTE.into());
    if !c.v8() {
        f.notes.push(NO_PLAN_CHANGE.into());
    }
    f.notes.push(
        "«Iniciar en el paso…» corre ahora una tarea del plan: un backup, un merge con sus días o un prune."
            .into(),
    );
    Ok(f)
}

// ---------------------------------------------------------------------------
// Active repositories: changes
// ---------------------------------------------------------------------------

fn active_path(name: &str) -> String {
    format!("{ACTIVE}/{}", encode(name))
}

/// The repository's state from the form.
fn state_of(v: Vals, current: &str) -> Result<String> {
    match v.text("state").as_str() {
        "" => Ok(current.to_string()),
        "active" => Ok("active".into()),
        "paused" => Ok("paused".into()),
        other => Err(bad(format!(
            "«{}» no es un estado de repositorio (activo o en pausa).",
            shown(other)
        ))),
    }
}

/// The body that creates an active repository, and its secrets.
pub(crate) fn create_body(
    c: &Ctx,
    v: Vals,
    plans: &[String],
    buckets: &[String],
) -> Result<(Map<String, Value>, Vec<&'static str>)> {
    let plan = v
        .opt("plan")
        .ok_or_else(|| bad("El repositorio necesita un plan.".into()))?;
    if !plans.contains(&plan) {
        return Err(bad(format!("No existe el plan «{}».", shown(&plan))));
    }
    let mut body = Map::new();
    let mut secrets = Vec::new();
    body.insert("plan".into(), json!(plan));
    if let Some(b) = v.opt("bucket") {
        if !buckets.contains(&b) {
            return Err(bad(format!("No existe el bucket «{}».", shown(&b))));
        }
        body.insert("bucket_name".into(), json!(b));
    }
    storage_body(c, v, &mut body, &mut secrets)?;
    if c.v8() {
        if v.bool("enable_users") {
            body.insert("enable_users".into(), json!(true));
        }
        if let Some(days) = v.int("retention", "Retención (días)", 0, MAX_RETENTION)? {
            if days > 0 {
                body.insert("default_retention".into(), json!(days));
            }
        }
        encryption_body(c, v, &mut body, &mut secrets)?;
    }
    Ok((body, secrets))
}

pub(crate) fn create_change(
    name: &str,
    body: Map<String, Value>,
    secrets: Vec<&'static str>,
    paused: bool,
) -> Change {
    let mut calls = vec![Call {
        method: Method::POST,
        path: active_path(name),
        body: Some(Value::Object(body)),
        secrets,
    }];
    if paused {
        calls.push(Call::new(
            Method::POST,
            format!("{}/pause", active_path(name)),
            Some(json!({})),
        ));
    }
    Change {
        comments: vec![format!("Crea el repositorio de backup «{name}».")],
        calls,
        undo: None,
        id: Some(job_id(name)),
    }
}

/// What an edit of an active repository sends: its plan (paused around
/// it), its state and its retention, each only when it changed.
pub(crate) fn edit_change(
    name: &str,
    plan: (&str, &str),
    state: (&str, &str),
    retention: Option<(Option<i64>, i64, bool)>,
) -> Change {
    let path = active_path(name);
    let mut ch = Change {
        id: Some(job_id(name)),
        ..Change::default()
    };
    let (old_plan, new_plan) = plan;
    let (old_state, new_state) = state;
    if old_plan != new_plan {
        ch.comments.push(format!(
            "Cambia el plan del repositorio «{name}» de «{old_plan}» a «{new_plan}»: el servicio lo pide en pausa."
        ));
        if old_state == "active" {
            ch.calls.push(Call::new(
                Method::POST,
                format!("{path}/pause"),
                Some(json!({})),
            ));
            ch.undo = Some((
                0,
                Call::new(Method::POST, format!("{path}/resume"), Some(json!({}))),
            ));
        }
        ch.calls.push(Call::new(
            Method::POST,
            format!("{path}/plan/{}", encode(new_plan)),
            Some(json!({})),
        ));
        if new_state == "active" {
            ch.calls.push(Call::new(
                Method::POST,
                format!("{path}/resume"),
                Some(json!({})),
            ));
        }
    } else if old_state != new_state {
        let verb = if new_state == "paused" {
            ch.comments
                .push(format!("Pone en pausa el repositorio «{name}»."));
            "pause"
        } else {
            ch.comments
                .push(format!("Reanuda el repositorio «{name}»."));
            "resume"
        };
        ch.calls.push(Call::new(
            Method::POST,
            format!("{path}/{verb}"),
            Some(json!({})),
        ));
    }
    if let Some((old, new, retroactive)) = retention {
        if old != Some(new) {
            ch.comments.push(if new == 0 {
                format!("Quita la retención del repositorio «{name}».")
            } else {
                format!("Guarda los backups del repositorio «{name}» {new} días.")
            });
            ch.calls.push(Call::new(
                Method::POST,
                format!("{path}/retentionSettings"),
                Some(json!({"period": new, "retroactive": retroactive})),
            ));
        }
    }
    if ch.calls.is_empty() {
        return Change::nothing(name, job_id(name));
    }
    ch
}

pub(crate) async fn active_change(
    s: &CbSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Change> {
    let v = Vals(&values.values);
    let plans: Vec<String> = plan::all(s).await?.into_iter().map(|p| p.name).collect();
    match id {
        None => {
            let name = v
                .opt("name")
                .ok_or_else(|| bad("El repositorio necesita un nombre.".into()))?;
            if !new_name(&name) {
                return Err(bad(format!(
                    "«{}»: el nombre de un repositorio lleva letras, dígitos, guion y guion bajo, hasta 50, y empieza con letra o dígito.",
                    shown(&name)
                )));
            }
            if names(s, "active").await?.contains(&name) {
                return Err(bad(format!("Ya hay un repositorio activo «{name}».")));
            }
            let paused = state_of(v, "active")? == "paused";
            let buckets = buckets(s).await?;
            let (body, secrets) = create_body(c, v, &plans, &buckets)?;
            Ok(create_change(&name, body, secrets, paused))
        }
        Some(id) => {
            let name = jobs::parse_id(id)?;
            let r = s.repo(&name).await?;
            let old_plan = field(&r, "plan_name").unwrap_or_default();
            let old_state = if field(&r, "state").as_deref() == Some("paused") {
                "paused"
            } else {
                "active"
            };
            let new_plan = v.opt("plan").unwrap_or_else(|| old_plan.clone());
            if new_plan != old_plan {
                if !c.v8() {
                    return Err(bad(NO_PLAN_CHANGE.into()));
                }
                if !plans.contains(&new_plan) {
                    return Err(bad(format!("No existe el plan «{}».", shown(&new_plan))));
                }
            }
            let new_state = state_of(v, old_state)?;
            let retention = if c.v8() {
                match v.int("retention", "Retención (días)", 0, MAX_RETENTION)? {
                    Some(n) => Some((retention(s, &name).await, n, v.bool("retroactive"))),
                    None => None,
                }
            } else {
                None
            };
            Ok(edit_change(
                &name,
                (&old_plan, &new_plan),
                (old_state, &new_state),
                retention,
            ))
        }
    }
}

/// A name the service takes for the archived copy of `name`, free among
/// `taken`.
pub(crate) fn archive_name(name: &str, taken: &[String]) -> String {
    let mut base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !base.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        base.insert(0, 'r');
    }
    base.truncate(50);
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            let mut b = base.clone();
            b.truncate(50 - suffix.len());
            b + &suffix
        })
        .find(|c| !taken.contains(c))
        .expect("a free name")
}

pub(crate) fn active_delete_change(name: &str, archive: &str, archived_as: &str) -> Change {
    Change {
        comments: vec![
            format!(
                "Borra el repositorio «{name}». El servicio de backup no borra repositorios activos: primero se archiva (como «{archived_as}») y después se borra del servicio."
            ),
            format!(
                "Sus backups quedan en «{archive}»: DBine no los borra, y se pueden importar de nuevo."
            ),
        ],
        calls: vec![
            Call::new(
                Method::POST,
                format!("{}/archive", active_path(name)),
                Some(json!({"id": archived_as})),
            ),
            Call::new(
                Method::DELETE,
                format!(
                    "/cluster/self/repository/archived/{}?remove_repository=false",
                    encode(archived_as)
                ),
                None,
            ),
        ],
        undo: None,
        id: None,
    }
}

pub(crate) async fn active_delete(s: &CbSession, id: &str) -> Result<Change> {
    let name = jobs::parse_id(id)?;
    let r = s.repo(&name).await?;
    let taken = names(s, "archived").await?;
    Ok(active_delete_change(
        &name,
        &field(&r, "archive").unwrap_or_default(),
        &archive_name(&name, &taken),
    ))
}

// ---------------------------------------------------------------------------
// Archived and imported repositories
// ---------------------------------------------------------------------------

/// The read-only form of a repository that isn't active.
fn stored_form(kind: Stored, r: &Value) -> ObjectForm {
    let mut f = ObjectForm::default();
    f.values
        .insert("name".into(), json!(field(r, "id").unwrap_or_default()));
    f.values.insert(
        "plan".into(),
        json!(field(r, "plan_name").unwrap_or_default()),
    );
    f.values.insert(
        "bucket".into(),
        json!(r
            .pointer("/bucket/name")
            .and_then(Value::as_str)
            .unwrap_or_default()),
    );
    storage_values(r, &mut f.values);
    let mut storage = storage_fields(true);
    storage.push(
        FormField::new("credentials_name", "Credenciales guardadas", FormKind::Text)
            .when("storage", &["s3", "az", "gs"])
            .read_only(true),
    );
    let info = info_section(r, &mut f.values);
    f.sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text).read_only(true),
            FormField::new("plan", "Plan", FormKind::Text).read_only(true),
            FormField::new("bucket", "Bucket", FormKind::Text).read_only(true),
        ]),
        FormSection::new("storage", "Almacenamiento").fields(storage),
        info,
    ];
    f.read_only = true;
    f.notes.push(
        match kind {
            Stored::Archived => ARCHIVED_NOTE,
            Stored::Imported => IMPORTED_NOTE,
        }
        .into(),
    );
    f
}

pub(crate) async fn archived_form(s: &CbSession, _c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    if let Some(id) = id {
        let name = stored_id(id)?;
        return Ok(stored_form(
            Stored::Archived,
            &stored(s, Stored::Archived, &name).await?,
        ));
    }
    let mut f = ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(vec![
            FormField::new(
                "source",
                "Repositorio activo",
                FormKind::reference("active", false),
            )
            .required()
            .help("El repositorio que se archiva."),
            FormField::new("name", "Nombre al archivarlo", FormKind::Text)
                .required()
                .help(
                    "Puede ser el mismo nombre; no puede repetir el de otro repositorio archivado.",
                ),
        ])],
        ..ObjectForm::default()
    };
    f.choices
        .insert("active".into(), pairs(names(s, "active").await?));
    f.values.insert("source".into(), Value::Null);
    f.values.insert("name".into(), json!(""));
    f.notes.push(ARCHIVE_FINAL.into());
    Ok(f)
}

pub(crate) fn archive_change(source: &str, name: &str) -> Change {
    Change {
        comments: vec![
            format!("Archiva el repositorio «{source}» como «{name}»."),
            ARCHIVE_FINAL.into(),
        ],
        calls: vec![Call::new(
            Method::POST,
            format!("{}/archive", active_path(source)),
            Some(json!({"id": name})),
        )],
        undo: None,
        id: Some(name.to_string()),
    }
}

pub(crate) async fn archived_change(
    s: &CbSession,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Change> {
    if id.is_some() {
        return Err(bad("Un repositorio archivado no se puede cambiar.".into()));
    }
    let v = Vals(&values.values);
    let source = v
        .opt("source")
        .ok_or_else(|| bad("Elegí el repositorio activo que se archiva.".into()))?;
    if !repo_name(&source) || !names(s, "active").await?.contains(&source) {
        return Err(bad(format!(
            "No existe el repositorio activo «{}».",
            shown(&source)
        )));
    }
    let name = v
        .opt("name")
        .ok_or_else(|| bad("Falta el nombre del repositorio archivado.".into()))?;
    if !new_name(&name) {
        return Err(bad(format!(
            "«{}»: el nombre de un repositorio lleva letras, dígitos, guion y guion bajo, hasta 50, y empieza con letra o dígito.",
            shown(&name)
        )));
    }
    if names(s, "archived").await?.contains(&name) {
        return Err(bad(format!("Ya hay un repositorio archivado «{name}».")));
    }
    Ok(archive_change(&source, &name))
}

pub(crate) async fn imported_form(s: &CbSession, c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    if let Some(id) = id {
        let name = stored_id(id)?;
        return Ok(stored_form(
            Stored::Imported,
            &stored(s, Stored::Imported, &name).await?,
        ));
    }
    let mut f = ObjectForm::default();
    let mut general = vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("Letras, dígitos, guion y guion bajo, hasta 50; empieza con letra o dígito."),
        FormField::new("repo", "Repositorio en el archivo", FormKind::Text)
            .required()
            .placeholder("a3a8a3e1-c58f-4be7-9c38-5c62f395b49b")
            .help("El nombre de la carpeta del repositorio dentro del archivo (el UUID del repositorio)."),
    ];
    general.extend(storage_fields(false));
    f.sections = vec![FormSection::new("general", "General").fields(general)];
    if c.v8() {
        f.sections
            .push(FormSection::new("encryption", "Cifrado").fields(encryption_fields(false)));
    }
    f.values.insert("name".into(), json!(""));
    f.values.insert("repo".into(), json!(""));
    f.values.insert("storage".into(), json!("filesystem"));
    f.notes.push(
        "Importar registra en el servicio un repositorio que ya está en un archivo (de este cluster o de otro), para restaurar o examinar sus backups."
            .into(),
    );
    f.notes.push(SECRET_KEYS_NOTE.into());
    Ok(f)
}

/// The repository's folder in an archive: letters, digits, `-` and `_`.
fn repo_folder(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 128
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn import_body(c: &Ctx, v: Vals) -> Result<(Map<String, Value>, Vec<&'static str>)> {
    let name = v
        .opt("name")
        .ok_or_else(|| bad("El repositorio importado necesita un nombre.".into()))?;
    if !new_name(&name) {
        return Err(bad(format!(
            "«{}»: el nombre de un repositorio lleva letras, dígitos, guion y guion bajo, hasta 50, y empieza con letra o dígito.",
            shown(&name)
        )));
    }
    let repo = v
        .opt("repo")
        .ok_or_else(|| bad("Falta el repositorio dentro del archivo.".into()))?;
    if !repo_folder(&repo) {
        return Err(bad(format!(
            "«{}» no es el nombre de un repositorio dentro de un archivo.",
            shown(&repo)
        )));
    }
    let mut body = Map::new();
    let mut secrets = Vec::new();
    body.insert("id".into(), json!(name));
    body.insert("repo".into(), json!(repo));
    storage_body(c, v, &mut body, &mut secrets)?;
    if c.v8() {
        encryption_body(c, v, &mut body, &mut secrets)?;
    }
    Ok((body, secrets))
}

pub(crate) fn import_change(
    name: &str,
    body: Map<String, Value>,
    secrets: Vec<&'static str>,
) -> Change {
    Change {
        comments: vec![format!("Importa el repositorio «{name}».")],
        calls: vec![Call {
            method: Method::POST,
            path: "/cluster/self/repository/import".into(),
            body: Some(Value::Object(body)),
            secrets,
        }],
        undo: None,
        id: Some(name.to_string()),
    }
}

pub(crate) async fn imported_change(
    s: &CbSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Change> {
    if id.is_some() {
        return Err(bad("Un repositorio importado no se puede cambiar.".into()));
    }
    let v = Vals(&values.values);
    let (body, secrets) = import_body(c, v)?;
    let name = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if names(s, "imported").await?.contains(&name) {
        return Err(bad(format!("Ya hay un repositorio importado «{name}».")));
    }
    Ok(import_change(&name, body, secrets))
}

pub(crate) fn stored_delete_change(kind: Stored, name: &str, archive: &str) -> Change {
    Change {
        comments: vec![
            match kind {
                Stored::Archived => {
                    format!("Borra el repositorio archivado «{name}» del servicio de backup.")
                }
                Stored::Imported => {
                    format!("Borra el repositorio importado «{name}» del servicio de backup.")
                }
            },
            format!(
                "Sus backups quedan en «{}»: DBine no los borra.",
                comment_text(archive)
            ),
        ],
        calls: vec![Call::new(
            Method::DELETE,
            format!(
                "/cluster/self/repository/{}/{}?remove_repository=false",
                kind.state(),
                encode(name)
            ),
            None,
        )],
        undo: None,
        id: None,
    }
}

pub(crate) async fn stored_delete(s: &CbSession, kind: Stored, id: &str) -> Result<Change> {
    let name = stored_id(id)?;
    let r = stored(s, kind, &name).await?;
    Ok(stored_delete_change(
        kind,
        &name,
        &field(&r, "archive").unwrap_or_default(),
    ))
}

// ---------------------------------------------------------------------------
// Running a task now, and the history
// ---------------------------------------------------------------------------

/// The request that runs a task of the plan now.
pub(crate) fn run_call(name: &str, task: &Task, cloud: bool) -> Result<Call> {
    let path = active_path(name);
    match task.kind.as_str() {
        "BACKUP" => Ok(Call::new(
            Method::POST,
            format!("{path}/backup"),
            Some(json!({"full_backup": task.full_backup})),
        )),
        "MERGE" if cloud => Err(bad(
            "El servicio de backup no hace merges en repositorios en la nube.".into(),
        )),
        "MERGE" => Ok(Call::new(
            Method::POST,
            format!("{path}/merge"),
            Some(match task.merge {
                Some((a, b)) => json!({"offset_start": a, "offset_end": b}),
                None => json!({}),
            }),
        )),
        "PRUNE" => Ok(Call::new(
            Method::POST,
            format!("{path}/prune"),
            Some(json!({})),
        )),
        other => Err(bad(format!(
            "DBine no sabe correr una tarea de tipo «{}».",
            shown(other)
        ))),
    }
}

pub(crate) async fn run_from(s: &CbSession, id: &str, step: &str) -> Result<()> {
    super::ready_to_change(s).await?;
    let name = jobs::parse_id(id)?;
    let r = s.repo(&name).await?;
    let plan_name = field(&r, "plan_name").unwrap_or_default();
    let plan = s
        .plan(&plan_name)
        .await
        .as_ref()
        .and_then(plan_of)
        .ok_or_else(|| {
            bad(format!(
                "No se pudo leer el plan «{plan_name}» del repositorio."
            ))
        })?;
    let task = plan
        .tasks
        .iter()
        .find(|t| t.name == step.trim())
        .ok_or_else(|| {
            bad(format!(
                "El plan «{plan_name}» no tiene la tarea «{}».",
                shown(step)
            ))
        })?;
    let call = run_call(
        &name,
        task,
        r.get("cloud_info").is_some_and(|c| !c.is_null()),
    )?;
    s.backup_call(call.method, &call.path, call.body.as_ref())
        .await
        .map(|_| ())
}

pub(crate) async fn clear_history(_s: &CbSession, id: &str) -> Result<()> {
    jobs::parse_id(id)?;
    Err(Error::Unsupported(NO_HISTORY_CLEAR.into()))
}

// ---------------------------------------------------------------------------
// Actions: a full backup now, every backup merged now
// ---------------------------------------------------------------------------

/// A repository in the cloud (S3, Azure, GCS): the service doesn't merge
/// its backups.
fn cloud(r: &Value) -> bool {
    r.get("cloud_info").is_some_and(|c| !c.is_null())
}

/// The actions a repository offers: a full backup always (paused ones
/// too: the service takes one-off tasks on them), a merge where the service
/// merges.
pub(crate) fn repo_actions(r: &Value) -> Vec<String> {
    let mut a = vec!["backup_now".to_string()];
    if !cloud(r) {
        a.push("merge_now".into());
    }
    a
}

/// "Backup completo ahora": the job's "run now" is an incremental one.
pub(crate) fn backup_now_change(name: &str) -> Change {
    Change {
        comments: vec![format!(
            "Hace ahora un backup completo del repositorio «{name}» («Ejecutar ahora» hace uno incremental)."
        )],
        calls: vec![Call::new(
            Method::POST,
            format!("{}/backup", active_path(name)),
            Some(json!({"full_backup": true})),
        )],
        id: Some(job_id(name)),
        ..Change::default()
    }
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const DAY: i64 = 86_400;
/// The furthest a zone is ahead of UTC (+14:00).
const MAX_AHEAD: i64 = 14 * 3_600;

/// Two digits at `i` of `s`.
fn two(s: &str, i: usize) -> Option<i64> {
    let t = s.get(i..i + 2)?;
    t.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| t.parse().ok())?
}

/// The instant (seconds since 1970-01-01T00:00:00Z) of a backup's date as
/// the service writes it: the node's local time and its offset, with `_`
/// for `:` (`2026-11-13T00_42_34.721581812Z`,
/// `2020-06-08T10_26_08.93-07_00`). A date whose time or offset doesn't
/// read is taken at the earliest instant it can be (its day's start, 14
/// hours ahead of UTC), so a merge window reaching it still covers it.
pub(crate) fn backup_time(date: &str) -> Option<i64> {
    let date = date.trim();
    let mut it = date.get(..10)?.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let day = days_from_civil(y, m, d) * DAY;
    let earliest = day - MAX_AHEAD;
    let Some(t) = date.get(10..).and_then(|t| t.strip_prefix(['T', 't', ' '])) else {
        return Some(earliest);
    };
    let sep = |i: usize| matches!(t.as_bytes().get(i), Some(b'_' | b':'));
    let (Some(h), Some(min), Some(sec), true, true) =
        (two(t, 0), two(t, 3), two(t, 6), sep(2), sep(5))
    else {
        return Some(earliest);
    };
    if h > 23 || min > 59 || sec > 60 {
        return Some(earliest);
    }
    let local = day + h * 3_600 + min * 60 + sec;
    // The fraction of a second, then the offset.
    let zone = t[8..].trim_start_matches(|c: char| c == '.' || c == ',' || c.is_ascii_digit());
    let offset = match zone.as_bytes().first() {
        Some(b'Z' | b'z') if zone.len() == 1 => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let (Some(oh), Some(om)) = (two(zone, 1), two(zone, 4).or_else(|| two(zone, 3))) else {
                return Some(earliest);
            };
            if oh > 14 || om > 59 {
                return Some(earliest);
            }
            let o = oh * 3_600 + om * 60;
            if sign == b'+' {
                o
            } else {
                -o
            }
        }
        _ => return Some(earliest),
    };
    Some(local - offset)
}

/// "Unir todos los backups ahora": a merge from today back to the oldest
/// backup: the whole days since it, rounded up, and one more for the
/// clocks and for a service that counts days on the node's calendar
/// (`times`: each backup's instant, [`backup_time`]; `now`: the instant
/// now, both in seconds). The service merges two or more.
pub(crate) fn merge_all_change(name: &str, times: &[i64], now: i64) -> Result<Change> {
    let refused = match times {
        [] => Some(format!(
            "El repositorio «{name}» no tiene backups: no hay nada que unir."
        )),
        [_] => Some(format!(
            "El repositorio «{name}» tiene un solo backup: hacen falta al menos dos para unirlos."
        )),
        _ => None,
    };
    if let Some(why) = refused {
        return Err(bad(why));
    }
    let oldest = times.iter().copied().min().unwrap_or(now);
    let back = ((now - oldest).max(0) + DAY - 1) / DAY + 1;
    Ok(Change {
        comments: vec![
            format!(
                "Une en uno los {} backups del repositorio «{name}»: los de hoy hasta {back} días atrás, donde está el más viejo.",
                times.len()
            ),
            "Después solo se puede restaurar el estado del más reciente: los puntos intermedios dejan de estar.".into(),
        ],
        calls: vec![Call::new(
            Method::POST,
            format!("{}/merge", active_path(name)),
            Some(json!({"offset_start": 0, "offset_end": back})),
        )],
        id: Some(job_id(name)),
        ..Change::default()
    })
}

/// Each backup's instant, from `…/info`.
async fn backup_times(s: &CbSession, name: &str) -> Result<Vec<i64>> {
    let v = s
        .backup_call(Method::GET, &format!("{}/info", active_path(name)), None)
        .await?;
    Ok(v.get("backups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("date").and_then(Value::as_str).and_then(backup_time))
        .collect())
}

/// The requests of one of the repositories' actions.
pub(crate) async fn action_change(s: &CbSession, id: &str, action: &str) -> Result<Change> {
    let name = jobs::parse_id(id)?;
    let r = s.repo(&name).await?;
    match action.trim() {
        "backup_now" => Ok(backup_now_change(&name)),
        "merge_now" if cloud(&r) => Err(bad(
            "El servicio de backup no hace merges en repositorios en la nube.".into(),
        )),
        "merge_now" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            merge_all_change(&name, &backup_times(s, &name).await?, now)
        }
        other => Err(Error::Unsupported(format!(
            "«{}» no es una acción de un repositorio de backup.",
            shown(other)
        ))),
    }
}

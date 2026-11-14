//! "Propiedades" of a model (spec §4.6 `database_properties`, D4). Owned by
//! part C.
//!
//! Facts: workspace, capacity (SKU, state, region; "sin permiso para ver el
//! SKU" when unknown, G4), storage mode, compatibility level and culture,
//! configured by, refreshable, RLS required, created, web URL, sensitivity
//! label, data sources (`GET …/datasources`, write permission) as "server ·
//! database", the query API in use, the metadata tier and what it can't
//! read and why.
//!
//! One editable field, `description` (D4), only on connections that aren't
//! read-only and for users who may change the model; the change goes
//! through [`crate::admin::alter_database`].

use crate::catalog::Model;
use crate::config::QueryApi;
use crate::http::Priority;
use crate::meta::service::{named_model, Service};
use crate::query::ApiKind;
use crate::Ctx;
use dbine_driver::{DatabaseProperties, Field, FieldKind, PropertyInfo, Result};
use serde_json::Value;

pub async fn database_properties(cx: &mut Ctx, database: &str) -> Result<DatabaseProperties> {
    properties_in(cx, database).await
}

fn yes_no(b: bool) -> String {
    if b { "sí" } else { "no" }.into()
}

/// One data source as "server · database" (or its kind and where it
/// points).
pub fn source_text(v: &Value) -> Option<String> {
    let d = v.get("connectionDetails")?;
    let s = |k: &str| d.get(k).and_then(Value::as_str).filter(|x| !x.is_empty());
    let kind = v
        .get("datasourceType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let place = match (s("server"), s("database")) {
        (Some(a), Some(b)) => format!("{a} · {b}"),
        (Some(a), None) => a.to_string(),
        _ => match (s("account"), s("domain")) {
            (Some(a), Some(dom)) => format!("{a}.{dom}"),
            (Some(a), None) => a.to_string(),
            _ => s("url").or_else(|| s("path")).unwrap_or("").to_string(),
        },
    };
    let kind = match s("kind") {
        Some(k) if kind == "Extension" => k.to_string(),
        _ => kind.to_string(),
    };
    Some(if place.is_empty() {
        kind
    } else if kind.is_empty() {
        place
    } else {
        format!("{kind}: {place}")
    })
}

/// Whether the user may change the model's description from here.
fn description_editable<S: Service>(s: &S, m: &Model) -> bool {
    !s.settings().read_only
        && m.write_permission
        && m.workspace.id.is_some()
        && s.settings().cloud.fabric_api().is_some()
}

pub(crate) async fn properties_in<S: Service>(
    s: &mut S,
    database: &str,
) -> Result<DatabaseProperties> {
    let m = named_model(s, database).await?;
    let mut p = DatabaseProperties::default();
    let mut info = |group: &str, label: &str, value: String| {
        if !value.trim().is_empty() {
            p.info.push(PropertyInfo {
                group: group.into(),
                label: label.into(),
                value,
            });
        }
    };
    info("", "Área de trabajo", m.workspace.name.clone());
    info("", "ID del modelo", m.id.clone());
    if let Some(c) = &m.configured_by {
        info("", "Configurado por", c.clone());
    }
    if let Some(r) = m.is_refreshable {
        info("", "Se puede actualizar", yes_no(r));
    }
    if let Some(r) = m.effective_identity_required {
        info("", "Requiere identidad efectiva (RLS)", yes_no(r));
    }
    if let Some(c) = &m.created {
        info(
            "",
            "Creado",
            crate::refresh::utc(c).unwrap_or_else(|| c.clone()),
        );
    }
    if let Some(u) = &m.web_url {
        info("", "Dirección web", u.clone());
    }
    if let Some(t) = &m.target_storage_mode {
        info(
            "",
            "Formato de almacenamiento",
            match t.as_str() {
                "Abf" => "modelo pequeño".into(),
                "PremiumFiles" => "modelo grande".into(),
                other => other.into(),
            },
        );
    }
    if let Some(l) = m
        .raw
        .pointer("/sensitivityLabel/labelId")
        .and_then(Value::as_str)
    {
        info("", "Etiqueta de confidencialidad", l.into());
    }
    let api = match (s.api_in_use(), s.settings().query_api) {
        (Some(ApiKind::Arrow), _) | (None, QueryApi::Arrow) => "executeDaxQueries (Arrow)",
        (Some(ApiKind::Json), _) | (None, QueryApi::Json) => "executeQueries (JSON)",
        (None, QueryApi::Auto) => "automática",
    };
    info("", "API de consultas", api.into());

    let (cap, _) = crate::monitor::capacity_text(s, &m).await;
    let mut info = |group: &str, label: &str, value: String| {
        if !value.trim().is_empty() {
            p.info.push(PropertyInfo {
                group: group.into(),
                label: label.into(),
                value,
            });
        }
    };
    info("Capacidad", "Capacidad", cap);
    if let Some(id) = m.workspace.capacity_id.clone() {
        if let Ok(Some(c)) = s.capacity(&id).await {
            if let Some(r) = c.region {
                p.info.push(PropertyInfo {
                    group: "Capacidad".into(),
                    label: "Región".into(),
                    value: r,
                });
            }
        }
    }

    match crate::meta::model_for(s, &m).await {
        Ok(meta) => {
            let mut add = |label: &str, value: String| {
                p.info.push(PropertyInfo {
                    group: "Metadatos".into(),
                    label: label.into(),
                    value,
                })
            };
            add("Leídos con", meta.tier.label().into());
            if let Some(c) = meta.compatibility_level {
                add("Nivel de compatibilidad", c.to_string());
            }
            if let Some(c) = &meta.culture {
                add("Referencia cultural", c.clone());
            }
            add("Tablas", meta.plain_tables().count().to_string());
            add("Medidas", meta.measures.len().to_string());
            add("Relaciones", meta.model_relationships().count().to_string());
            let modes: Vec<String> = {
                let mut v: Vec<String> = meta
                    .plain_tables()
                    .filter_map(|t| t.storage_mode.clone())
                    .collect();
                v.sort();
                v.dedup();
                v
            };
            if !modes.is_empty() {
                add("Modos de almacenamiento", modes.join(", "));
            }
            for n in &meta.notes {
                add("Limitación", n.clone());
            }
        }
        Err(e) if crate::meta::is_fatal(&e) => return Err(e),
        Err(e) => p.info.push(PropertyInfo {
            group: "Metadatos".into(),
            label: "Limitación".into(),
            value: format!("No se pudieron leer: {}", crate::meta::short(&e)),
        }),
    }

    if m.write_permission {
        if let Ok(v) = s
            .get(&format!("{}/datasources", m.path()), Priority::Background)
            .await
        {
            for src in v
                .get("value")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(t) = source_text(src) {
                    p.info.push(PropertyInfo {
                        group: "Orígenes de datos".into(),
                        label: "Origen".into(),
                        value: t,
                    });
                }
            }
        }
    } else {
        p.info.push(PropertyInfo {
            group: "Orígenes de datos".into(),
            label: "Origen".into(),
            value: "se ven con permiso de escritura en el modelo".into(),
        });
    }

    if description_editable(s, &m) {
        p.fields.push(
            Field::new("description", "Descripción", FieldKind::Text)
                .help("La descripción del modelo en Power BI y Fabric."),
        );
        p.values.insert(
            "description".into(),
            m.raw
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
        p.warnings.insert(
            "description".into(),
            "Cambia la descripción del modelo para todos los que lo usan.".into(),
        );
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;
    use crate::meta::fake::Fake;

    const SOURCES: &str = include_str!("../tests/fixtures/rest_datasources.json");

    #[test]
    fn data_sources_read_as_server_and_database() {
        let v: Value = serde_json::from_str(SOURCES).unwrap();
        let all: Vec<String> = v["value"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(source_text)
            .collect();
        assert_eq!(
            all,
            [
                "AnalysisServices: My-As-Server · My-As-Database",
                "AzureDataExplorer: https://myserver.kusto.windows.net"
            ]
        );
        let blob = serde_json::json!({"datasourceType": "AzureBlobs", "connectionDetails": {"account": "myAccount", "domain": "blob.core.windows.net"}});
        assert_eq!(
            source_text(&blob).unwrap(),
            "AzureBlobs: myAccount.blob.core.windows.net"
        );
    }

    #[tokio::test]
    async fn facts_and_the_description_field() {
        let mut f = Fake::writer();
        f.definition(crate::definition::tmsl::tests::SAMPLE);
        let m = f.model.clone().unwrap();
        f.model.as_mut().unwrap().raw = serde_json::json!({"description": "Ventas diarias"});
        f.models[0].raw = f.model.as_ref().unwrap().raw.clone();
        f.route(
            Method::Get,
            &format!("{}/datasources", m.path()),
            200,
            serde_json::from_str(SOURCES).unwrap(),
        );
        let p = properties_in(&mut f, "Ventas").await.unwrap();
        let get = |l: &str| {
            p.info
                .iter()
                .find(|i| i.label == l)
                .map(|i| i.value.clone())
        };
        assert_eq!(get("Área de trabajo").as_deref(), Some("Finanzas"));
        assert_eq!(get("Capacidad").as_deref(), Some("compartida (Pro)"));
        assert_eq!(get("Nivel de compatibilidad").as_deref(), Some("1601"));
        assert_eq!(
            get("Leídos con").as_deref(),
            Some("definición del modelo (API de Fabric)")
        );
        assert!(p
            .info
            .iter()
            .any(|i| i.group == "Orígenes de datos" && i.value.contains("My-As-Server")));
        assert_eq!(p.fields.len(), 1);
        assert_eq!(p.values["description"], "Ventas diarias");
        // A read-only connection: facts only.
        let mut f = Fake::new();
        let p = properties_in(&mut f, "Ventas").await.unwrap();
        assert!(p.fields.is_empty());
        assert!(p.info.iter().any(|i| i.label == "Limitación"));
    }
}

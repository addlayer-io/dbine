//! "Propiedades" of a database (a top-level space, source or home,
//! [`dbine_driver::Session::database_properties`]): what the catalog API
//! (`/api/v3/catalog/by-path`) reports and, for a source, its metadata and
//! reflection refresh policies, changed with `PUT /api/v3/catalog/{id}`.
//!
//! Spaces and homes have nothing to change besides their name: they show
//! their facts only. A source's connection settings (host, credentials…)
//! belong to the connection, not here. The change is one request: the
//! source as it is now, with the changed fields (secrets come masked and go
//! back masked, so Dremio keeps them).

use crate::{encode, text, DremioSession};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const HOUR_MS: f64 = 3_600_000.0;
const METADATA: &str = "Metadatos";
const REFLECTIONS: &str = "Reflexiones";

/// Field key, JSON field, inside `metadataPolicy`.
const HOURS: &[(&str, &str, bool)] = &[
    ("names_refresh_hours", "namesRefreshMs", true),
    ("dataset_refresh_hours", "datasetRefreshAfterMs", true),
    ("dataset_expire_hours", "datasetExpireAfterMs", true),
    ("auth_ttl_hours", "authTTLMs", true),
    ("reflection_refresh_hours", "accelerationRefreshPeriodMs", false),
    ("reflection_expire_hours", "accelerationGracePeriodMs", false),
];

/// Field key, JSON field, inside `metadataPolicy`.
const SWITCHES: &[(&str, &str, bool)] = &[
    ("delete_unavailable", "deleteUnavailableDatasets", true),
    ("auto_promote", "autoPromoteDatasets", true),
    ("reflection_never_refresh", "accelerationNeverRefresh", false),
    ("reflection_never_expire", "accelerationNeverExpire", false),
];

fn hours(ms: Option<&Value>) -> String {
    let Some(ms) = ms.and_then(Value::as_f64) else { return String::new() };
    let s = format!("{:.3}", ms / HOUR_MS);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The fields `changes` set, as they go in the source's JSON.
pub(crate) fn patch(changes: &BTreeMap<String, String>) -> Result<Value> {
    let (mut top, mut policy) = (Map::new(), Map::new());
    for (key, value) in changes {
        let v = value.trim();
        if let Some((_, field, in_policy)) = HOURS.iter().find(|h| h.0 == key) {
            let h: f64 = v.parse().map_err(|_| Error::Query(format!("«{v}» no es una cantidad de horas")))?;
            let ms = (h * HOUR_MS).round();
            if !h.is_finite() || ms < 60_000.0 || ms > 1e13 || v.contains(['e', 'E']) {
                return Err(Error::Query(format!("«{v}» no es una cantidad de horas válida (desde un minuto)")));
            }
            (if *in_policy { &mut policy } else { &mut top }).insert(field.to_string(), json!(ms as u64));
        } else if let Some((_, field, in_policy)) = SWITCHES.iter().find(|s| s.0 == key) {
            let b = match v {
                "true" => true,
                "" | "false" => false,
                _ => return Err(Error::Query(format!("{key}: «{v}» no es un valor válido"))),
            };
            (if *in_policy { &mut policy } else { &mut top }).insert(field.to_string(), json!(b));
        } else if key == "dataset_update_mode" {
            if !matches!(v, "PREFETCH" | "PREFETCH_QUERIED" | "INLINE") {
                return Err(Error::Query(format!("modo de actualización: «{v}» no es un valor válido")));
            }
            policy.insert("datasetUpdateMode".into(), json!(v));
        } else {
            return Err(Error::Query(format!("propiedad desconocida: {key}")));
        }
    }
    if !policy.is_empty() {
        top.insert("metadataPolicy".into(), Value::Object(policy));
    }
    Ok(Value::Object(top))
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    let p = patch(changes)?;
    Ok(format!(
        "PUT /api/v3/catalog/{{id de «{database}»}} (el origen como está, con estos cambios)\n{}",
        serde_json::to_string_pretty(&p)?
    ))
}

/// `patch` applied over the source's current JSON.
fn merge(source: &mut Value, patch: Value) {
    let Value::Object(p) = patch else { return };
    for (k, v) in p {
        match (source.get_mut(&k), v) {
            (Some(Value::Object(cur)), Value::Object(new)) => cur.extend(new),
            (_, v) => {
                source[k.as_str()] = v;
            }
        }
    }
}

impl DremioSession {
    async fn entity(&self, database: &str) -> Result<Value> {
        self.conn.send(reqwest::Method::GET, &format!("/api/v3/catalog/by-path/{}", encode(database)), None).await
    }

    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let e = self.entity(database).await?;
        let kind = e.get("entityType").map(text).unwrap_or_default();
        let general = |label: &str, value: String| PropertyInfo { group: String::new(), label: label.into(), value };
        let mut info = vec![
            general(
                "Tipo",
                match kind.as_str() {
                    "source" => format!("Origen ({})", e.get("type").map(text).unwrap_or_default()),
                    "space" => "Espacio".into(),
                    "home" => "Carpeta personal".into(),
                    other => other.to_string(),
                },
            ),
            general("ID", e.get("id").map(text).unwrap_or_default()),
        ];
        if let Some(t) = e.get("createdAt").map(text).filter(|t| !t.is_empty()) {
            info.push(general("Creado", t));
        }
        if let Some(st) = e.pointer("/state/status").map(text).filter(|s| !s.is_empty()) {
            info.push(general("Estado", st));
        }
        if let Some(n) = e.get("children").and_then(Value::as_array) {
            info.push(general("Elementos", n.len().to_string()));
        }
        let mut values = BTreeMap::new();
        let mut fields = Vec::new();
        if kind == "source" {
            let at = |field: &str, in_policy: bool| if in_policy { e.get("metadataPolicy").and_then(|p| p.get(field)) } else { e.get(field) };
            for (key, field, in_policy) in HOURS {
                values.insert(key.to_string(), hours(at(field, *in_policy)));
            }
            for (key, field, in_policy) in SWITCHES {
                values.insert(key.to_string(), if at(field, *in_policy).and_then(Value::as_bool) == Some(true) { "true".into() } else { String::new() });
            }
            values.insert("dataset_update_mode".into(), at("datasetUpdateMode", true).map(text).unwrap_or_default());
            let h = |key: &'static str, label: &'static str, group: &'static str| Field::new(key, label, FieldKind::Number).group(group);
            let b = |key: &'static str, label: &'static str, group: &'static str| Field::new(key, label, FieldKind::Bool).group(group);
            fields = vec![
                h("names_refresh_hours", "Buscar datasets nuevos cada (horas)", METADATA),
                Field::new(
                    "dataset_update_mode",
                    "Modo de actualización de los datasets",
                    FieldKind::Select(vec![
                        ("PREFETCH_QUERIED", "Solo los consultados (PREFETCH_QUERIED)"),
                        ("PREFETCH", "Todos (PREFETCH)"),
                        ("INLINE", "Al consultar (INLINE)"),
                    ]),
                )
                .group(METADATA),
                h("dataset_refresh_hours", "Actualizar los detalles de los datasets cada (horas)", METADATA),
                h("dataset_expire_hours", "Vencer los detalles de los datasets después de (horas)", METADATA),
                h("auth_ttl_hours", "Vencer los permisos en caché después de (horas)", METADATA),
                b("delete_unavailable", "Quitar los datasets que ya no existen en el origen", METADATA),
                b("auto_promote", "Promover automáticamente archivos y carpetas a datasets", METADATA),
                h("reflection_refresh_hours", "Actualizar las reflexiones cada (horas)", REFLECTIONS),
                h("reflection_expire_hours", "Vencer las reflexiones después de (horas)", REFLECTIONS),
                b("reflection_never_refresh", "No actualizar nunca las reflexiones", REFLECTIONS),
                b("reflection_never_expire", "Las reflexiones no vencen nunca", REFLECTIONS),
            ];
        }
        let warnings = BTreeMap::from([(
            "delete_unavailable".to_string(),
            "Al actualizar los metadatos, Dremio borra los datasets que ya no estén en el origen, con sus reflexiones y permisos.".to_string(),
        )]);
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    /// The source as it is now (its `tag` guards against a concurrent
    /// change), with the changed fields, in one `PUT`.
    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let p = patch(changes)?;
        if p.as_object().is_some_and(Map::is_empty) {
            return Ok(());
        }
        let mut e = self.entity(database).await?;
        if e.get("entityType").map(text).as_deref() != Some("source") {
            return Err(Error::Unsupported(format!("«{database}» no es un origen: no tiene propiedades que cambiar")));
        }
        let id = e.get("id").map(text).ok_or_else(|| Error::Query(format!("No se encontró {database} en el catálogo.")))?;
        if let Some(o) = e.as_object_mut() {
            o.remove("children");
            o.remove("state");
        }
        merge(&mut e, p);
        self.conn.send(reqwest::Method::PUT, &format!("/api/v3/catalog/{}", encode(&id)), Some(&e)).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn only_the_changes() {
        let p = patch(&c(&[
            ("names_refresh_hours", "1"),
            ("dataset_expire_hours", "0.5"),
            ("delete_unavailable", ""),
            ("dataset_update_mode", "INLINE"),
            ("reflection_refresh_hours", "2"),
            ("reflection_never_expire", "true"),
        ]))
        .unwrap();
        assert_eq!(
            p,
            json!({
                "metadataPolicy": { "namesRefreshMs": 3_600_000u64, "datasetExpireAfterMs": 1_800_000u64, "deleteUnavailableDatasets": false, "datasetUpdateMode": "INLINE" },
                "accelerationRefreshPeriodMs": 7_200_000u64,
                "accelerationNeverExpire": true,
            })
        );
        let mut source = json!({ "id": "x", "tag": "t", "config": { "password": "$DREMIO_EXISTING_VALUE$" }, "metadataPolicy": { "authTTLMs": 86_400_000u64, "namesRefreshMs": 1u64 } });
        merge(&mut source, p);
        assert_eq!(source["metadataPolicy"]["authTTLMs"], 86_400_000u64);
        assert_eq!(source["metadataPolicy"]["namesRefreshMs"], 3_600_000u64);
        assert_eq!(source["config"]["password"], "$DREMIO_EXISTING_VALUE$");
        assert!(script("s3", &c(&[("auto_promote", "true")])).unwrap().starts_with("PUT /api/v3/catalog/{id de «s3»}"));
        assert_eq!(hours(Some(&json!(5_400_000u64))), "1.5");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("names_refresh_hours", "0"), ("names_refresh_hours", "x"), ("names_refresh_hours", "1e9"), ("auto_promote", "yes"), ("dataset_update_mode", "LAZY"), ("config", "{}")] {
            assert!(patch(&c(&[bad])).is_err(), "{bad:?}");
        }
    }
}

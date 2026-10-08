//! "Propiedades" of a database (a dataset, [`dbine_driver::Session::database_properties`]):
//! what `datasets.get` reports, changed with one `datasets.patch` that
//! carries only the changed fields (so it applies all or nothing).
//!
//! - General: description; location, created and modified as facts.
//! - Tablas: default table and partition expiration, default collation,
//!   case-insensitive names and the rounding mode.
//! - Almacenamiento: time travel window and the storage billing model.
//! - Etiquetas: one field per label (emptied: removed) and new ones.
//!
//! An emptied setting goes as `null`, which clears it. Every value is
//! checked before it reaches the request.

use crate::create_db::labels;
use crate::BigQuerySession;
use dbine_driver::serde_static::intern;
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use serde_json::{json, Map, Value as Json};
use std::collections::BTreeMap;

const DAY_MS: f64 = 86_400_000.0;
const TABLES: &str = "Tablas";
const STORAGE: &str = "Almacenamiento";
const LABELS: &str = "Etiquetas";

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

/// Days (decimals allowed) as milliseconds, at least `min_ms`.
fn days_ms(what: &str, v: &str, min_ms: f64) -> Result<Json> {
    let days: f64 = v.parse().map_err(|_| bad(what, v))?;
    let ms = (days * DAY_MS).round();
    if !days.is_finite() || ms < min_ms || ms > 1e15 || v.contains(['e', 'E']) {
        return Err(Error::Query(format!("{what}: «{v}» no es una cantidad de días válida")));
    }
    Ok(json!(format!("{}", ms as u64)))
}

/// Milliseconds as days, without trailing zeros.
fn ms_days(ms: &str) -> Option<String> {
    let d = ms.parse::<f64>().ok()? / DAY_MS;
    let s = format!("{d:.3}");
    Some(s.trim_end_matches('0').trim_end_matches('.').to_string())
}

/// The `datasets.patch` body for `changes`.
pub(crate) fn body(changes: &BTreeMap<String, String>) -> Result<Json> {
    let mut body = Map::new();
    let mut label_changes = Map::new();
    for (key, value) in changes {
        let v = value.trim();
        let (field, val) = match key.as_str() {
            "description" => ("description", if v.is_empty() { Json::Null } else { json!(value) }),
            "default_table_expiration_days" => {
                ("defaultTableExpirationMs", if v.is_empty() { Json::Null } else { days_ms("vencimiento de las tablas", v, 3_600_000.0)? })
            }
            "default_partition_expiration_days" => {
                ("defaultPartitionExpirationMs", if v.is_empty() { Json::Null } else { days_ms("vencimiento de las particiones", v, 1.0)? })
            }
            "default_collation" => match v {
                "" => ("defaultCollation", Json::Null),
                "und:ci" => ("defaultCollation", json!(v)),
                _ => return Err(bad("intercalación", v)),
            },
            "is_case_insensitive" => match v {
                "true" => ("isCaseInsensitive", json!(true)),
                "" | "false" => ("isCaseInsensitive", json!(false)),
                _ => return Err(bad("nombres sin distinguir mayúsculas", v)),
            },
            "default_rounding_mode" => match v {
                "" => ("defaultRoundingMode", Json::Null),
                "ROUND_HALF_AWAY_FROM_ZERO" | "ROUND_HALF_EVEN" => ("defaultRoundingMode", json!(v)),
                _ => return Err(bad("redondeo", v)),
            },
            "max_time_travel_hours" => {
                let ok = v.parse::<u32>().is_ok_and(|h| (48..=168).contains(&h) && h % 24 == 0) && v.chars().all(|c| c.is_ascii_digit());
                if !ok {
                    return Err(Error::Query(format!("time travel: «{v}» no es un múltiplo de 24 horas entre 48 y 168")));
                }
                ("maxTimeTravelHours", json!(v))
            }
            "storage_billing_model" => match v {
                "LOGICAL" | "PHYSICAL" => ("storageBillingModel", json!(v)),
                _ => return Err(bad("modelo de facturación", v)),
            },
            "labels_add" => {
                label_changes.extend(labels(v)?);
                continue;
            }
            k => match k.strip_prefix("label:") {
                Some(name) if crate::create_db::label_part(name, true) => {
                    if v.is_empty() {
                        label_changes.insert(name.to_string(), Json::Null);
                    } else if crate::create_db::label_part(v, false) {
                        label_changes.insert(name.to_string(), json!(v));
                    } else {
                        return Err(Error::Query(format!("etiquetas: el valor de «{name}», «{v}», solo admite minúsculas, números, _ y -")));
                    }
                    continue;
                }
                _ => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
            },
        };
        body.insert(field.into(), val);
    }
    if !label_changes.is_empty() {
        body.insert("labels".into(), Json::Object(label_changes));
    }
    Ok(Json::Object(body))
}

/// What "Ver script" shows: the request and its body.
pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    let body = body(changes)?;
    Ok(format!("PATCH datasets/{database} (datasets.patch)\n{}", serde_json::to_string_pretty(&body)?))
}

fn when(ms: Option<&str>) -> String {
    ms.and_then(|m| m.parse::<i64>().ok())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_default()
}

impl BigQuerySession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let ds = self.api.get(&["datasets", database], &[]).await?;
        let s = |k: &str| ds.get(k).and_then(Json::as_str).map(str::to_string);
        let general = |label: &str, value: String| PropertyInfo { group: String::new(), label: label.into(), value };
        let mut info = vec![
            general("Ubicación", s("location").unwrap_or_default()),
            general("Creado", when(s("creationTime").as_deref())),
            general("Modificado", when(s("lastModifiedTime").as_deref())),
        ];
        if let Some(t) = s("type").filter(|t| t != "DEFAULT") {
            info.push(general("Tipo", t));
        }
        // Tables and their size, from the dataset's __TABLES__ (needs
        // read access to the dataset's metadata; left out otherwise).
        let sql = format!(
            "SELECT CAST(COUNT(*) AS STRING), CAST(COALESCE(SUM(size_bytes), 0) AS STRING), CAST(COALESCE(SUM(row_count), 0) AS STRING) FROM `{}`.`{}`.__TABLES__",
            self.api.project.replace('`', ""),
            database.replace('`', "")
        );
        if let Ok(r) = self.query(&sql, 1, None).await {
            if let Some(row) = r.rows.first() {
                let c = |i: usize| row.get(i).and_then(Json::as_str).unwrap_or_default().to_string();
                let mb = c(1).parse::<f64>().map(|b| format!("{:.1} MB", b / 1_048_576.0)).unwrap_or_default();
                info.push(PropertyInfo { group: TABLES.into(), label: "Tablas".into(), value: c(0) });
                info.push(PropertyInfo { group: TABLES.into(), label: "Tamaño".into(), value: mb });
                info.push(PropertyInfo { group: TABLES.into(), label: "Filas".into(), value: c(2) });
            }
        }

        let mut values = BTreeMap::new();
        values.insert("description".to_string(), s("description").unwrap_or_default());
        values.insert("default_table_expiration_days".into(), s("defaultTableExpirationMs").and_then(|m| ms_days(&m)).unwrap_or_default());
        values.insert("default_partition_expiration_days".into(), s("defaultPartitionExpirationMs").and_then(|m| ms_days(&m)).unwrap_or_default());
        values.insert("default_collation".into(), s("defaultCollation").unwrap_or_default());
        values.insert(
            "is_case_insensitive".into(),
            if ds.get("isCaseInsensitive").and_then(Json::as_bool) == Some(true) { "true".into() } else { String::new() },
        );
        values.insert("default_rounding_mode".into(), s("defaultRoundingMode").filter(|m| m != "ROUNDING_MODE_UNSPECIFIED").unwrap_or_default());
        values.insert("max_time_travel_hours".into(), s("maxTimeTravelHours").unwrap_or_else(|| "168".into()));
        values.insert("storage_billing_model".into(), s("storageBillingModel").filter(|m| m != "STORAGE_BILLING_MODEL_UNSPECIFIED").unwrap_or_else(|| "LOGICAL".into()));
        values.insert("labels_add".into(), String::new());

        let mut fields = vec![
            Field::new("description", "Descripción", FieldKind::Textarea),
            Field::new("default_table_expiration_days", "Vencimiento por defecto de las tablas (días)", FieldKind::Number)
                .help("Vacío: las tablas nuevas no vencen. Solo afecta a las que se creen después.")
                .group(TABLES),
            Field::new("default_partition_expiration_days", "Vencimiento por defecto de las particiones (días)", FieldKind::Number)
                .help("Vacío: las particiones no vencen. Solo afecta a las tablas particionadas que se creen después.")
                .group(TABLES),
            Field::new("default_collation", "Intercalación por defecto (collation)", FieldKind::Select(vec![("und:ci", "Sin distinguir mayúsculas (und:ci)")]))
                .help("Vacía: las comparaciones distinguen mayúsculas.")
                .group(TABLES),
            Field::new("is_case_insensitive", "Nombres de tablas sin distinguir mayúsculas", FieldKind::Bool).group(TABLES),
            Field::new(
                "default_rounding_mode",
                "Redondeo por defecto",
                FieldKind::Select(vec![
                    ("ROUND_HALF_AWAY_FROM_ZERO", "Mitad lejos de cero (ROUND_HALF_AWAY_FROM_ZERO)"),
                    ("ROUND_HALF_EVEN", "Mitad al par (ROUND_HALF_EVEN)"),
                ]),
            )
            .group(TABLES),
            Field::new(
                "max_time_travel_hours",
                "Ventana de time travel",
                FieldKind::Select(vec![("48", "2 días"), ("72", "3 días"), ("96", "4 días"), ("120", "5 días"), ("144", "6 días"), ("168", "7 días")]),
            )
            .group(STORAGE),
            Field::new("storage_billing_model", "Facturación del almacenamiento", FieldKind::Select(vec![("LOGICAL", "Lógico (LOGICAL)"), ("PHYSICAL", "Físico (PHYSICAL)")]))
                .group(STORAGE),
        ];
        if let Some(l) = ds.get("labels").and_then(Json::as_object) {
            for (k, v) in l {
                let key = format!("label:{k}");
                fields.push(Field::new(intern(&key), intern(k), FieldKind::Text).help("Vacía: se quita la etiqueta.").group(LABELS));
                values.insert(key, v.as_str().unwrap_or_default().to_string());
            }
        }
        fields.push(
            Field::new("labels_add", "Etiquetas nuevas", FieldKind::Textarea)
                .placeholder("equipo=ventas\nentorno=prod")
                .help("Una por línea, como clave=valor: minúsculas, números, _ y -.")
                .group(LABELS),
        );

        let warnings = BTreeMap::from([
            (
                "storage_billing_model".to_string(),
                "Cambia cómo se factura todo el almacenamiento del dataset; después de cambiarlo hay que esperar 14 días para volver a cambiarlo.".to_string(),
            ),
            (
                "max_time_travel_hours".to_string(),
                "Achicar la ventana hace que ya no se puedan consultar ni recuperar los datos más viejos que el nuevo plazo.".to_string(),
            ),
            (
                "is_case_insensitive".to_string(),
                "Cambia cómo se resuelven los nombres de las tablas del dataset: consultas que dependían de mayúsculas pueden cambiar de tabla o fallar.".to_string(),
            ),
        ]);
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    /// One `datasets.patch`: all or nothing.
    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let body = body(changes)?;
        if body.as_object().is_some_and(Map::is_empty) {
            return Ok(());
        }
        let url = self.api.url(&["datasets", database])?;
        self.api.send(self.api.http.patch(url).json(&body)).await.map(|_| ())
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
        assert_eq!(
            body(&c(&[
                ("description", ""),
                ("default_table_expiration_days", "30"),
                ("default_partition_expiration_days", "0.5"),
                ("max_time_travel_hours", "72"),
                ("storage_billing_model", "PHYSICAL"),
                ("is_case_insensitive", "true"),
                ("label:equipo", ""),
                ("label:entorno", "prod"),
                ("labels_add", "costo=bajo"),
            ]))
            .unwrap(),
            json!({
                "description": null,
                "defaultTableExpirationMs": "2592000000",
                "defaultPartitionExpirationMs": "43200000",
                "maxTimeTravelHours": "72",
                "storageBillingModel": "PHYSICAL",
                "isCaseInsensitive": true,
                "labels": { "equipo": null, "entorno": "prod", "costo": "bajo" },
            })
        );
        assert_eq!(body(&c(&[("default_table_expiration_days", " "), ("default_collation", "")])).unwrap(), json!({ "defaultTableExpirationMs": null, "defaultCollation": null }));
        assert!(script("v", &c(&[("description", "x")])).unwrap().starts_with("PATCH datasets/v (datasets.patch)\n{"));
        assert_eq!(ms_days("2592000000").as_deref(), Some("30"));
        assert_eq!(ms_days("3600000").as_deref(), Some("0.042"));
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("default_table_expiration_days", "0"),
            ("default_table_expiration_days", "x"),
            ("default_table_expiration_days", "1e3"),
            ("max_time_travel_hours", "50"),
            ("max_time_travel_hours", "24"),
            ("storage_billing_model", "logical"),
            ("default_collation", "und:cs"),
            ("default_rounding_mode", "UP"),
            ("label:Equipo", "x"),
            ("label:equipo", "Ventas"),
            ("labels_add", "A=b"),
            ("nope", "1"),
        ] {
            assert!(body(&c(&[bad])).is_err(), "{bad:?}");
        }
    }
}

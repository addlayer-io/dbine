//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//! A database is a dataset, created with `datasets.insert` as the plain
//! create does: its location, default table expiration, description,
//! labels and default collation go in the request's body, which is what
//! "Ver script" shows (the session adds the project and, without a chosen
//! location, the connection's).
//!
//! Every value is checked before it reaches the request.

use crate::BigQuerySession;
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use serde_json::{json, Map, Value as Json};
use std::collections::BTreeMap;

/// Multi-regions and the most used regions, as suggestions (any other
/// region is accepted).
const LOCATIONS: [&str; 22] = [
    "US",
    "EU",
    "us-central1",
    "us-east1",
    "us-east4",
    "us-east5",
    "us-west1",
    "us-west2",
    "northamerica-northeast1",
    "southamerica-east1",
    "southamerica-west1",
    "europe-west1",
    "europe-west2",
    "europe-west3",
    "europe-west4",
    "europe-southwest1",
    "asia-east1",
    "asia-northeast1",
    "asia-south1",
    "asia-southeast1",
    "australia-southeast1",
    "me-central1",
];

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("location", "Ubicación", FieldKind::Text)
            .placeholder("US, EU, southamerica-east1…")
            .help("Vacía: la de la conexión (o US). No se puede cambiar después."),
        Field::new("table_expiration_days", "Vencimiento por defecto de las tablas (días)", FieldKind::Number)
            .help("Vacío: las tablas nuevas no vencen."),
        Field::new("description", "Descripción", FieldKind::Textarea),
        Field::new("labels", "Etiquetas (labels)", FieldKind::Textarea)
            .placeholder("equipo=ventas\nentorno=prod")
            .help("Una por línea, como clave=valor: minúsculas, números, _ y -."),
        Field::new("collation", "Intercalación por defecto (collation)", FieldKind::Select(vec![("und:ci", "Sin distinguir mayúsculas (und:ci)")]))
            .help("Vacía: las comparaciones distinguen mayúsculas."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// A label key or value: lowercase letters (any script), digits, `_` and
/// `-`, at most 63 characters; a key starts with a letter.
fn label_part(v: &str, key: bool) -> bool {
    v.chars().count() <= 63
        && v.chars().all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && (!key || v.chars().next().is_some_and(char::is_lowercase))
}

fn labels(v: &str) -> Result<Map<String, Json>> {
    let mut out = Map::new();
    for line in v.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (k, val) = line.split_once('=').map_or((line, ""), |(k, v)| (k.trim(), v.trim()));
        if !label_part(k, true) {
            return Err(Error::Query(format!("etiquetas: «{k}» no es una clave válida (minúsculas, números, _ y -, empezando por una letra)")));
        }
        if !label_part(val, false) {
            return Err(Error::Query(format!("etiquetas: el valor de «{k}», «{val}», solo admite minúsculas, números, _ y -")));
        }
        out.insert(k.to_string(), json!(val));
    }
    if out.len() > 64 {
        return Err(Error::Query("etiquetas: un dataset admite hasta 64".into()));
    }
    Ok(out)
}

/// The `datasets.insert` body for dataset `name`. `project` and
/// `location` (the connection's) are filled by the session.
pub(crate) fn body(name: &str, o: &BTreeMap<String, String>, project: Option<&str>, location: Option<&str>) -> Result<Json> {
    let mut reference = json!({ "datasetId": name });
    if let Some(p) = project {
        reference["projectId"] = json!(p);
    }
    let mut body = json!({ "datasetReference": reference });
    match opt(o, "location") {
        Some(l) => {
            if !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err(Error::Query(format!("ubicación: «{l}» no es una región ni una multirregión")));
            }
            body["location"] = json!(l);
        }
        None => {
            if let Some(l) = location {
                body["location"] = json!(l);
            }
        }
    }
    if let Some(d) = opt(o, "table_expiration_days") {
        let days: u64 = d
            .parse()
            .ok()
            .filter(|n| (1..=100_000).contains(n))
            .ok_or_else(|| Error::Query(format!("vencimiento de las tablas: «{d}» no es un número entero de días")))?;
        body["defaultTableExpirationMs"] = json!((days * 86_400_000).to_string());
    }
    if let Some(d) = opt(o, "description") {
        body["description"] = json!(d);
    }
    if let Some(l) = opt(o, "labels") {
        let l = labels(l)?;
        if !l.is_empty() {
            body["labels"] = Json::Object(l);
        }
    }
    if let Some(c) = opt(o, "collation") {
        if c != "und:ci" {
            return Err(Error::Query(format!("intercalación: «{c}» no es un valor válido (und:ci)")));
        }
        body["defaultCollation"] = json!(c);
    }
    Ok(body)
}

/// What "Ver script" shows: the request and its body.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let body = body(name, o, None, None)?;
    Ok(format!("POST datasets (datasets.insert)\n{}", serde_json::to_string_pretty(&body)?))
}

impl BigQuerySession {
    /// Location suggestions; the default is the connection's.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        Ok(vec![FieldChoices {
            key: "location".into(),
            default: Some(self.api.location.clone().unwrap_or_else(|| "US".into())),
            values: LOCATIONS.iter().map(|l| l.to_string()).collect(),
        }])
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let body = body(name, o, Some(&self.api.project), self.api.location.as_deref())?;
        self.api.post(&["datasets"], &[], &body).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        // The body of the plain create (see `create_database`).
        assert_eq!(body("ventas", &o(&[]), Some("p"), Some("EU")).unwrap(), json!({ "datasetReference": { "projectId": "p", "datasetId": "ventas" }, "location": "EU" }));
        assert_eq!(body("ventas", &o(&[("labels", " ")]), Some("p"), None).unwrap(), json!({ "datasetReference": { "projectId": "p", "datasetId": "ventas" } }));
        assert_eq!(script("v", &o(&[])).unwrap(), "POST datasets (datasets.insert)\n{\n  \"datasetReference\": {\n    \"datasetId\": \"v\"\n  }\n}");
    }

    #[test]
    fn every_option() {
        let b = body(
            "v",
            &o(&[
                ("location", "southamerica-east1"),
                ("table_expiration_days", "30"),
                ("description", "Ventas \"históricas\""),
                ("labels", "equipo=ventas\n\nentorno = prod\nvacía"),
                ("collation", "und:ci"),
            ]),
            Some("p"),
            Some("US"),
        )
        .unwrap();
        assert_eq!(
            b,
            json!({
                "datasetReference": { "projectId": "p", "datasetId": "v" },
                "location": "southamerica-east1",
                "defaultTableExpirationMs": "2592000000",
                "description": "Ventas \"históricas\"",
                "labels": { "equipo": "ventas", "entorno": "prod", "vacía": "" },
                "defaultCollation": "und:ci",
            })
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("location", "us central"),
            ("location", "EU\"}"),
            ("table_expiration_days", "0"),
            ("table_expiration_days", "1.5"),
            ("labels", "Equipo=ventas"),
            ("labels", "1x=a"),
            ("labels", "a=Ventas"),
            ("labels", "a=b c"),
            ("collation", "und:cs"),
        ] {
            assert!(script("v", &o(&[bad])).is_err(), "{bad:?}");
        }
    }
}

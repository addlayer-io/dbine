//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! the database resource (`GET /dbs/{id}`), its containers, and its shared
//! throughput: the offer whose `offerResourceId` is the database's `_rid`.
//!
//! The throughput changes through the offers API: `PUT /offers/{rid}` with
//! the offer's new content, manual (`offerThroughput`) or autoscale
//! (`offerAutopilotSettings.maxThroughput`). Switching between the two is
//! a `PUT` of the offer as it is with `x-ms-cosmos-migrate-offer-to-autopilot`
//! or `x-ms-cosmos-migrate-offer-to-manual-throughput`; the service then
//! picks the RU/s, and a second `PUT` sets the ones asked for.
//!
//! A database without its own offer (throughput per container, or a
//! serverless account) shows that as a fact and has nothing to change.

use crate::{enc, CosmosSession};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const THROUGHPUT: &str = "Throughput";
const TO_AUTOSCALE: &str = "x-ms-cosmos-migrate-offer-to-autopilot";
const TO_MANUAL: &str = "x-ms-cosmos-migrate-offer-to-manual-throughput";

/// One request on the database's offer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Step {
    /// The offer as it is, with the migration header.
    Migrate { to_autoscale: bool },
    /// Manual RU/s.
    Manual(u64),
    /// Autoscale maximum RU/s.
    Autoscale(u64),
}

fn check_name(database: &str) -> Result<()> {
    if database.is_empty() || database.ends_with(' ') || database.contains(['/', '\\', '#', '?']) {
        return Err(Error::Query(format!("nombre de base inválido: «{database}»")));
    }
    Ok(())
}

fn manual_ru(v: &str) -> Result<u64> {
    v.parse::<u64>()
        .ok()
        .filter(|n| *n >= 400 && n % 100 == 0 && *n <= 1_000_000)
        .ok_or_else(|| Error::Query(format!("RU/s: «{v}» no es un valor válido (desde 400, de a 100)")))
}

fn autoscale_ru(v: &str) -> Result<u64> {
    v.parse::<u64>()
        .ok()
        .filter(|n| *n >= 1000 && n % 1000 == 0 && *n <= 1_000_000)
        .ok_or_else(|| Error::Query(format!("RU/s máximas: «{v}» no es un valor válido (desde 1000, de a 1000)")))
}

/// The requests for `changes`, in order: the switch of mode first.
pub(crate) fn steps(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<Step>> {
    check_name(database)?;
    let get = |k: &str| changes.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    for k in changes.keys() {
        if !matches!(k.as_str(), "throughput_mode" | "throughput" | "autoscale_max") {
            return Err(Error::Query(format!("propiedad desconocida: {k}")));
        }
    }
    let mut out = Vec::new();
    match changes.get("throughput_mode").map(|v| v.trim()) {
        Some("manual") => {
            out.push(Step::Migrate { to_autoscale: false });
            if let Some(v) = get("throughput") {
                out.push(Step::Manual(manual_ru(v)?));
            }
        }
        Some("autoscale") => {
            out.push(Step::Migrate { to_autoscale: true });
            if let Some(v) = get("autoscale_max") {
                out.push(Step::Autoscale(autoscale_ru(v)?));
            }
        }
        Some(m) => return Err(Error::Query(format!("modo de throughput: «{m}» no es un valor válido"))),
        None => match (get("throughput"), get("autoscale_max")) {
            (Some(_), Some(_)) => return Err(Error::Query("las RU/s son manuales o de autoescalado, no las dos".into())),
            (Some(v), None) => out.push(Step::Manual(manual_ru(v)?)),
            (None, Some(v)) => out.push(Step::Autoscale(autoscale_ru(v)?)),
            (None, None) => {
                if changes.contains_key("throughput") || changes.contains_key("autoscale_max") {
                    return Err(Error::Query("indicá las RU/s".into()));
                }
            }
        },
    }
    Ok(out)
}

/// A step as the request it is (the offer's id is the database's, read
/// when it runs).
fn request_text(database: &str, step: Step) -> String {
    let put = format!("PUT /offers/{{oferta de dbs/{database}}}");
    match step {
        Step::Migrate { to_autoscale } => {
            format!("{put}\n{}: true\n(la oferta como está)", if to_autoscale { TO_AUTOSCALE } else { TO_MANUAL })
        }
        Step::Manual(n) => format!("{put}\n{}", json!({ "content": { "offerThroughput": n } })),
        Step::Autoscale(n) => format!("{put}\n{}", json!({ "content": { "offerAutopilotSettings": { "maxThroughput": n } } })),
    }
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(steps(database, changes)?.into_iter().map(|s| request_text(database, s)).collect::<Vec<_>>().join("\n\n"))
}

/// The offer with `step` applied to its content.
fn apply(offer: &Value, step: Step) -> Value {
    let mut o = offer.clone();
    if let Some(content) = o.get_mut("content").and_then(Value::as_object_mut) {
        match step {
            Step::Migrate { .. } => {}
            Step::Manual(n) => {
                content.remove("offerAutopilotSettings");
                content.insert("offerThroughput".into(), n.into());
            }
            Step::Autoscale(n) => {
                content.remove("offerThroughput");
                content.insert("offerAutopilotSettings".into(), json!({ "maxThroughput": n }));
            }
        }
    }
    o
}

fn autoscale(offer: &Value) -> Option<u64> {
    offer.pointer("/content/offerAutopilotSettings/maxThroughput").and_then(Value::as_u64)
}

fn fact(group: &str, label: &str, value: String) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value }
}

impl CosmosSession {
    async fn database_resource(&self, database: &str) -> Result<Value> {
        check_name(database)?;
        Ok(self.call(Method::GET, "dbs", &format!("dbs/{database}"), &format!("/dbs/{}", enc(database)), None, &[]).await?.body)
    }

    /// Every offer of the account (none on serverless accounts, which
    /// refuse the feed).
    async fn offers(&self) -> Vec<Value> {
        self.list("/offers", "offers", "", "Offers").await.unwrap_or_default()
    }

    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let db = self.database_resource(database).await?;
        let rid = db.get("_rid").and_then(Value::as_str).unwrap_or_default().to_string();
        let colls = self
            .list(&format!("/dbs/{}/colls", enc(database)), "colls", &format!("dbs/{database}"), "DocumentCollections")
            .await
            .unwrap_or_default();
        let offers = self.offers().await;
        let own = offers.iter().find(|o| o.get("offerResourceId").and_then(Value::as_str) == Some(rid.as_str()));
        let coll_rids: Vec<&str> = colls.iter().filter_map(|c| c.get("_rid")?.as_str()).collect();
        let dedicated = offers
            .iter()
            .filter(|o| o.get("offerResourceId").and_then(Value::as_str).is_some_and(|r| coll_rids.contains(&r)))
            .count();

        let mut info = vec![
            fact("", "Id", db.get("id").and_then(Value::as_str).unwrap_or(database).to_string()),
            fact("", "Identificador interno (_rid)", rid.clone()),
        ];
        if let Some(ts) = db.get("_ts").and_then(Value::as_i64) {
            let when = chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string()).unwrap_or_else(|| ts.to_string());
            info.push(fact("", "Última modificación (_ts)", when));
        }
        info.push(fact("", "Contenedores", colls.len().to_string()));
        info.push(fact("", "Contenedores con throughput propio", dedicated.to_string()));

        let mut p = DatabaseProperties::default();
        let Some(offer) = own else {
            info.push(fact(
                THROUGHPUT,
                "Throughput de la base",
                "No tiene: lo tiene cada contenedor, o la cuenta es serverless. No hay nada para cambiar acá.".into(),
            ));
            p.info = info;
            return Ok(p);
        };
        info.push(fact(THROUGHPUT, "Throughput de la base", "Compartido por los contenedores que no tienen el suyo".into()));
        if let Some(id) = offer.get("id").and_then(Value::as_str) {
            info.push(fact(THROUGHPUT, "Oferta (offer)", id.to_string()));
        }
        if let Some(v) = offer.pointer("/content/offerMinimumThroughputParameters/maxThroughputEverProvisioned").and_then(Value::as_u64) {
            info.push(fact(THROUGHPUT, "Máximo de RU/s alguna vez asignado", v.to_string()));
        }
        let (mode, manual, max) = match autoscale(offer) {
            Some(max) => ("autoscale", String::new(), max.to_string()),
            None => (
                "manual",
                offer.pointer("/content/offerThroughput").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_default(),
                String::new(),
            ),
        };
        p.values.insert("throughput_mode".into(), mode.into());
        p.values.insert("throughput".into(), manual);
        p.values.insert("autoscale_max".into(), max);
        p.fields = vec![
            Field::new(
                "throughput_mode",
                "Modo",
                FieldKind::Select(vec![("manual", "Manual (RU/s fijas)"), ("autoscale", "Autoescalado (RU/s máximas)")]),
            )
            .help("Al cambiar de modo el servicio elige las RU/s del modo nuevo, salvo que las indiques.")
            .group(THROUGHPUT),
            Field::new("throughput", "RU/s", FieldKind::Number)
                .help("Desde 400, de a 100. El mínimo real depende de los contenedores y del almacenamiento.")
                .when("throughput_mode", &["manual"])
                .group(THROUGHPUT),
            Field::new("autoscale_max", "RU/s máximas", FieldKind::Number)
                .help("Desde 1000, de a 1000. Escala entre el 10 % y este máximo.")
                .when("throughput_mode", &["autoscale"])
                .group(THROUGHPUT),
        ];
        p.warnings.insert(
            "throughput_mode".into(),
            "Cambiar entre manual y autoescalado cambia cómo se cobra: el autoescalado cobra cada hora por las RU/s más altas que alcanzó, a una tarifa por RU/s mayor que la manual. El cambio puede tardar en completarse."
                .into(),
        );
        for k in ["throughput", "autoscale_max"] {
            p.warnings.insert(
                k.into(),
                "Cambia el costo por hora de la base. Subir mucho puede tardar (el servicio reparte particiones) y bajar tiene un mínimo que fija el servicio.".into(),
            );
        }
        p.info = info;
        Ok(p)
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        self.check_writable()?;
        let steps = steps(database, changes)?;
        if steps.is_empty() {
            return Ok(());
        }
        let db = self.database_resource(database).await?;
        let rid = db.get("_rid").and_then(Value::as_str).unwrap_or_default().to_string();
        for (i, step) in steps.iter().enumerate() {
            let r = async {
                // The offer as it is now (a previous step changed it).
                let offers = self.offers().await;
                let offer = offers
                    .into_iter()
                    .find(|o| o.get("offerResourceId").and_then(Value::as_str) == Some(rid.as_str()))
                    .ok_or_else(|| Error::Query(format!("la base «{database}» no tiene throughput propio: lo tiene cada contenedor, o la cuenta es serverless")))?;
                let is_auto = autoscale(&offer).is_some();
                match step {
                    Step::Manual(_) if is_auto && i == 0 => {
                        return Err(Error::Query("la base tiene autoescalado: para fijar RU/s manuales hay que cambiar el modo".into()))
                    }
                    Step::Autoscale(_) if !is_auto && i == 0 => {
                        return Err(Error::Query("la base tiene RU/s manuales: para autoescalado hay que cambiar el modo".into()))
                    }
                    _ => {}
                }
                let orid = offer.get("_rid").and_then(Value::as_str).unwrap_or_default().to_string();
                let link = offer.get("id").and_then(Value::as_str).unwrap_or(&orid).to_lowercase();
                let mut headers = vec![("Content-Type", "application/json".to_string())];
                if let Step::Migrate { to_autoscale } = step {
                    headers.push((if *to_autoscale { TO_AUTOSCALE } else { TO_MANUAL }, "true".into()));
                }
                let body = apply(&offer, *step);
                self.call(Method::PUT, "offers", &link, &format!("/offers/{}", enc(&orid)), Some(&body), &headers).await.map(|_| ())
            }
            .await;
            if let Err(e) = r {
                return Err(if i == 0 {
                    e
                } else {
                    Error::Query(format!("se aplicaron {i} de {} cambios; falló: {}\n{e}", steps.len(), request_text(database, *step)))
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn ru_in_the_current_mode() {
        assert_eq!(steps("v", &c(&[("throughput", "500")])).unwrap(), [Step::Manual(500)]);
        assert_eq!(steps("v", &c(&[("autoscale_max", "4000")])).unwrap(), [Step::Autoscale(4000)]);
        assert_eq!(
            script("ventas", &c(&[("throughput", "500")])).unwrap(),
            "PUT /offers/{oferta de dbs/ventas}\n{\"content\":{\"offerThroughput\":500}}"
        );
        assert!(steps("v", &c(&[])).unwrap().is_empty());
    }

    #[test]
    fn switching_mode_goes_first() {
        assert_eq!(
            steps("v", &c(&[("throughput_mode", "autoscale"), ("autoscale_max", "5000"), ("throughput", "800")])).unwrap(),
            [Step::Migrate { to_autoscale: true }, Step::Autoscale(5000)]
        );
        assert_eq!(steps("v", &c(&[("throughput_mode", "manual")])).unwrap(), [Step::Migrate { to_autoscale: false }]);
        assert_eq!(
            script("v", &c(&[("throughput_mode", "manual"), ("throughput", "400")])).unwrap(),
            "PUT /offers/{oferta de dbs/v}\nx-ms-cosmos-migrate-offer-to-manual-throughput: true\n(la oferta como está)\n\n\
             PUT /offers/{oferta de dbs/v}\n{\"content\":{\"offerThroughput\":400}}"
        );
    }

    #[test]
    fn content_is_replaced() {
        let offer = json!({ "id": "abcd", "content": { "offerThroughput": 400, "offerIsRUPerMinuteThroughputEnabled": false } });
        let o = apply(&offer, Step::Autoscale(4000));
        assert_eq!(o["content"], json!({ "offerIsRUPerMinuteThroughputEnabled": false, "offerAutopilotSettings": { "maxThroughput": 4000 } }));
        assert_eq!(apply(&o, Step::Manual(600))["content"]["offerThroughput"], 600);
        assert_eq!(apply(&offer, Step::Migrate { to_autoscale: true }), offer);
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            &[("throughput", "300")][..],
            &[("throughput", "450")],
            &[("throughput", "400\r\nx: 1")],
            &[("autoscale_max", "1500")],
            &[("throughput", "500"), ("autoscale_max", "4000")],
            &[("throughput_mode", "serverless")],
            &[("throughput", "")],
            &[("nope", "1")],
        ] {
            assert!(steps("v", &c(bad)).is_err(), "{bad:?}");
        }
        assert!(steps("a/b", &c(&[("throughput", "400")])).is_err());
    }
}

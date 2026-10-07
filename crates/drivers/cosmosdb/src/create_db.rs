//! "Nueva base de datos" with options
//! ([`dbine_driver::Driver::create_database_fields`]): throughput shared
//! by the database's containers, manual (`x-ms-offer-throughput`) or
//! autoscale (`x-ms-cosmos-offer-autopilot-settings`), as headers of
//! `POST /dbs`. Without it, each container gets its own throughput, as
//! before. Serverless accounts refuse provisioned throughput: the server's
//! error comes back as is.

use crate::CosmosSession;
use dbine_driver::{Error, Field, FieldKind, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new(
            "throughput_mode",
            "Throughput compartido",
            FieldKind::Select(vec![("manual", "Manual (RU/s fijas)"), ("autoscale", "Autoescalado (RU/s máximas)")]),
        )
        .help("Vacío: sin throughput de base; cada contenedor tiene el suyo. Las cuentas serverless no lo admiten."),
        Field::new("throughput", "RU/s", FieldKind::Number)
            .help("Manual: desde 400, de a 100. Autoescalado: el máximo, desde 1000, de a 1000.")
            .when("throughput_mode", &["manual", "autoscale"]),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// The headers that carry the throughput.
pub(crate) fn headers(o: &BTreeMap<String, String>) -> Result<Vec<(&'static str, String)>> {
    let mode = opt(o, "throughput_mode");
    let ru = opt(o, "throughput");
    let bad = |v: &str| Error::Query(format!("RU/s: «{v}» no es un valor válido"));
    match (mode, ru) {
        (None, None) => Ok(Vec::new()),
        (Some("manual") | None, Some(v)) => {
            let n = v.parse::<u64>().ok().filter(|n| *n >= 400 && n % 100 == 0 && *n <= 1_000_000).ok_or_else(|| bad(v))?;
            Ok(vec![("x-ms-offer-throughput", n.to_string())])
        }
        (Some("autoscale"), Some(v)) => {
            let n = v.parse::<u64>().ok().filter(|n| *n >= 1000 && n % 1000 == 0 && *n <= 1_000_000).ok_or_else(|| bad(v))?;
            Ok(vec![("x-ms-cosmos-offer-autopilot-settings", json!({ "maxThroughput": n }).to_string())])
        }
        (Some("manual" | "autoscale"), None) => Err(Error::Query("indicá las RU/s del throughput compartido".into())),
        (Some(m), _) => Err(Error::Query(format!("throughput compartido: «{m}» no es un valor válido"))),
    }
}

fn body(name: &str) -> Value {
    json!({ "id": name })
}

/// What "Ver script" shows: the request, its throughput headers and body.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let mut out = vec!["POST /dbs".to_string()];
    out.extend(headers(o)?.into_iter().map(|(k, v)| format!("{k}: {v}")));
    out.push(body(name).to_string());
    Ok(out.join("\n"))
}

impl CosmosSession {
    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        self.check_writable()?;
        let mut h = vec![("Content-Type", "application/json".to_string())];
        h.extend(headers(o)?);
        self.call(Method::POST, "dbs", "", "/dbs", Some(&body(name)), &h).await?;
        Ok(())
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
        assert_eq!(script("ventas", &o(&[("throughput", " ")])).unwrap(), "POST /dbs\n{\"id\":\"ventas\"}");
    }

    #[test]
    fn throughput() {
        assert_eq!(
            script("v", &o(&[("throughput_mode", "manual"), ("throughput", "400")])).unwrap(),
            "POST /dbs\nx-ms-offer-throughput: 400\n{\"id\":\"v\"}"
        );
        assert_eq!(
            script("v", &o(&[("throughput_mode", "autoscale"), ("throughput", "4000")])).unwrap(),
            "POST /dbs\nx-ms-cosmos-offer-autopilot-settings: {\"maxThroughput\":4000}\n{\"id\":\"v\"}"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            &[("throughput", "300")][..],
            &[("throughput", "450")],
            &[("throughput_mode", "autoscale"), ("throughput", "1500")],
            &[("throughput_mode", "autoscale")],
            &[("throughput_mode", "serverless"), ("throughput", "400")],
            &[("throughput", "400\r\nx-evil: 1")],
        ] {
            assert!(script("v", &o(bad)).is_err(), "{bad:?}");
        }
    }
}

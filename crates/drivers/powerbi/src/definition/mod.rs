//! Tier B: the model's full definition through Fabric (decision D2, spec
//! §4.3). Owned by part C. Read only: DBine never writes a definition.
//!
//! `POST /workspaces/{g}/semanticModels/{d}/getDefinition?format=TMSL`
//! ([`crate::http::Api::fabric_send`]); `202` + `Location` + `Retry-After` →
//! poll ([`crate::http::Api::fabric_follow`], which refuses any host but
//! `api.fabric.microsoft.com`) → `…/result`; the `model.bim` part is
//! base64, at most 64 MB (S4). Needs read and write permission on the model
//! and the `SemanticModel.ReadWrite.All` scope; public cloud only; offered
//! when the connection's `definition_api` is on.
//!
//! TMSL rather than TMDL (Fabric's default): one JSON document, parsed by
//! [`tmsl`], with nothing to write a TMDL reader for. A service that
//! answered with TMDL parts anyway gets a clear error, and the model falls
//! to the next tier.

pub mod tmsl;

use crate::catalog::Model;
use crate::http::{Method, Priority};
use crate::meta::service::{session_model, Service};
use crate::meta::ModelMeta;
use crate::Ctx;
use base64::Engine;
use dbine_driver::{Error, Result};
use serde_json::Value;
use std::time::{Duration, Instant};

/// The largest `model.bim` read (S4).
pub const MAX_MODEL_BIM: usize = 64 * 1024 * 1024;
/// How long the operation is waited for.
const MAX_WAIT: Duration = Duration::from_secs(300);

pub async fn read(cx: &mut Ctx) -> Result<ModelMeta> {
    let m = session_model(cx)?;
    read_model(cx, &m).await
}

pub(crate) async fn read_model<S: Service>(s: &mut S, m: &Model) -> Result<ModelMeta> {
    let g = m.workspace.id.as_deref().ok_or_else(|| {
        Error::Unsupported(
            "La API de definición de Fabric necesita el área de trabajo del modelo.".into(),
        )
    })?;
    if !crate::catalog::valid_guid(g) || !crate::catalog::valid_guid(&m.id) {
        return Err(Error::Query("Identificador de modelo no válido.".into()));
    }
    let path = format!(
        "/workspaces/{g}/semanticModels/{}/getDefinition?format=TMSL",
        m.id
    );
    let first = s
        .fabric_send(Method::Post, &path, None, Priority::Background)
        .await?;
    let body = match first.status {
        200 => first.body,
        202 => {
            let location = first.location.clone().ok_or_else(|| {
                Error::Query("La API de Fabric aceptó el pedido sin decir dónde seguirlo.".into())
            })?;
            follow(s, &location, first.retry_after).await?
        }
        other => {
            return Err(Error::Query(format!(
                "La API de definición de Fabric respondió {other}."
            )))
        }
    };
    let bim = model_bim(&body)?;
    tmsl::parse(&bim)
}

/// Polls a Fabric long-running operation until it ends, then reads its
/// result.
async fn follow<S: Service>(
    s: &mut S,
    location: &str,
    mut wait: Option<Duration>,
) -> Result<Value> {
    let started = Instant::now();
    loop {
        let pause = wait
            .unwrap_or(Duration::from_secs(2))
            .min(Duration::from_secs(30));
        if started.elapsed() + pause > MAX_WAIT {
            return Err(Error::Query(
                "La API de Fabric tardó más de 5 minutos en entregar la definición del modelo."
                    .into(),
            ));
        }
        tokio::time::sleep(pause).await;
        let st = s.fabric_follow(location, Priority::Background).await?;
        // A result already (the operation answered with it).
        if st.body.get("definition").is_some() {
            return Ok(st.body);
        }
        match st.body.get("status").and_then(Value::as_str).unwrap_or("") {
            "Succeeded" => {
                let result = st
                    .location
                    .clone()
                    .unwrap_or_else(|| format!("{}/result", location.trim_end_matches('/')));
                let r = s.fabric_follow(&result, Priority::Background).await?;
                return Ok(r.body);
            }
            "Failed" | "Undefined" => {
                let why = st
                    .body
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("sin detalle");
                return Err(Error::Query(format!(
                    "La API de Fabric no pudo leer la definición del modelo: {}",
                    why.chars().take(300).collect::<String>()
                )));
            }
            _ => wait = st.retry_after.or(wait),
        }
    }
}

/// The `model.bim` part of a definition, decoded and parsed.
pub(crate) fn model_bim(body: &Value) -> Result<Value> {
    let parts = body
        .pointer("/definition/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Error::Query("La respuesta de la API de Fabric no trae la definición.".into())
        })?;
    let part = parts.iter().find(|p| {
        p.get("path")
            .and_then(Value::as_str)
            .is_some_and(|x| x.eq_ignore_ascii_case("model.bim"))
    });
    let Some(part) = part else {
        let tmdl = parts.iter().any(|p| {
            p.get("path")
                .and_then(Value::as_str)
                .is_some_and(|x| x.ends_with(".tmdl"))
        });
        return Err(Error::Query(if tmdl {
            "La API de Fabric devolvió la definición en TMDL en lugar de TMSL.".into()
        } else {
            "La definición del modelo no trae model.bim.".into()
        }));
    };
    if part.get("payloadType").and_then(Value::as_str) != Some("InlineBase64") {
        return Err(Error::Query(
            "La definición del modelo llegó en un formato que DBine no lee.".into(),
        ));
    }
    let payload = part.get("payload").and_then(Value::as_str).unwrap_or("");
    if payload.len() > MAX_MODEL_BIM / 3 * 4 + 4 {
        return Err(Error::Query(
            "La definición del modelo pasa de 64 MB: DBine no la lee.".into(),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|_| Error::Query("La definición del modelo no es base64 válido.".into()))?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(bytes).map_err(|_| Error::Query("model.bim no es JSON válido.".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::fake::{Fake, Route};
    use crate::meta::Tier;

    #[tokio::test]
    async fn the_operation_is_followed_to_its_result() {
        let mut f = Fake::writer();
        f.definition(tmsl::tests::SAMPLE);
        let m = f.model.clone().unwrap();
        let meta = read_model(&mut f, &m).await.unwrap();
        assert_eq!(meta.tier, Tier::Definition);
        assert_eq!(meta.measures.len(), 2);
        let req = f.requests();
        assert_eq!(req.len(), 4, "{req:?}");
        assert_eq!(req[0].method, Method::Post);
        assert!(req[0].path.ends_with("/getDefinition?format=TMSL"));
        assert!(req[1..].iter().all(|r| r.method == Method::Get && r.fabric));
        assert!(req[3].path.ends_with("/result"));
    }

    /// Every `Location` goes through `Service::fabric_follow`, and a refused
    /// follow fails the read (no fallback request of its own). Which
    /// `Location`s the real client refuses (other hosts, `http`, other
    /// ports, user info: S3) is tested on `http::fabric_location` itself
    /// (`fabric_locations_only_on_fabric`); the fake takes any.
    #[tokio::test]
    async fn a_refused_follow_fails_the_read() {
        let mut f = Fake::writer();
        let m = f.model.clone().unwrap();
        let g = m.workspace.id.clone().unwrap();
        let bad = "https://api.fabric.microsoft.com.example.net/v1/operations/1";
        f.push(Route {
            method: Method::Post,
            path: format!(
                "/workspaces/{g}/semanticModels/{}/getDefinition?format=TMSL",
                m.id
            ),
            fabric: true,
            status: 202,
            body: Value::Null,
            location: Some(bad.into()),
            retry_after: Some(Duration::from_millis(1)),
            times: Some(1),
        });
        // No route for it: the fake refuses it, as the client would.
        assert!(read_model(&mut f, &m).await.is_err());
        let req = f.requests();
        assert_eq!(req.len(), 2, "{req:?}");
        assert_eq!((req[1].method, req[1].path.as_str()), (Method::Get, bad));
    }

    #[test]
    fn the_definition_part_is_checked() {
        use serde_json::json;
        let tmdl = json!({"definition": {"parts": [
            {"path": "definition/model.tmdl", "payload": "e30=", "payloadType": "InlineBase64"}
        ]}});
        assert!(model_bim(&tmdl).unwrap_err().to_string().contains("TMDL"));
        let bad = json!({"definition": {"parts": [
            {"path": "model.bim", "payload": "%%%", "payloadType": "InlineBase64"}
        ]}});
        assert!(model_bim(&bad).is_err());
        // A BOM in front of the JSON is fine.
        let bom = base64::engine::general_purpose::STANDARD.encode(b"\xEF\xBB\xBF{\"model\":{}}");
        let ok = json!({"definition": {"parts": [
            {"path": "model.bim", "payload": bom, "payloadType": "InlineBase64"}
        ]}});
        assert!(model_bim(&ok).is_ok());
        assert!(model_bim(&json!({})).is_err());
    }
}

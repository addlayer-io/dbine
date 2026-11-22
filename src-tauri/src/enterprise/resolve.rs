//! Which managed connections point at a source a Power BI table (or any
//! lineage) names. A managed connection holds no server, so DBine Enterprise
//! answers: it matches the server and database against the connections the
//! person may use and returns ids only (docs/enterprise.md in
//! dbine-enterprise, "Resolving a source"). The server audits each question.

use super::{connection_id, Enterprise};
use crate::error::{CommandError, CommandResult};
use dbine_driver::dependencies::ExternalRef;
use serde::Serialize;
use serde_json::Value;

/// What to look for: the engine and the server, as the source names them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceRequest {
    /// The engine (`DriverInfo::id`): `sqlserver`, `postgres`, `mysql`…
    pub driver: String,
    /// `host`, `host:port`, `host,port`, `host\instance`, a URL…
    pub server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

/// The engine a lineage's `ExternalRef::system` names, when it is one a
/// connection can open.
pub fn driver_of_system(system: &str) -> Option<&'static str> {
    match system.trim().to_lowercase().as_str() {
        "sql server" | "sqlserver" | "azure sql" => Some("sqlserver"),
        "databricks" => Some("databricks"),
        "snowflake" => Some("snowflake"),
        "oracle" => Some("oracle"),
        "postgresql" | "postgres" => Some("postgres"),
        "mysql" => Some("mysql"),
        "mariadb" => Some("mariadb"),
        _ => None,
    }
}

impl SourceRequest {
    /// From an external source of a lineage; None when it names no server or
    /// is not a system a connection opens (a file, SharePoint, a web page).
    pub fn from_external(r: &ExternalRef) -> Option<SourceRequest> {
        let driver = driver_of_system(r.system.as_deref()?)?;
        let server = r.server.as_deref()?.trim();
        if server.is_empty() {
            return None;
        }
        Some(SourceRequest {
            driver: driver.into(),
            server: server.into(),
            port: None,
            instance: None,
            database: r
                .database
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(String::from),
        })
    }
}

/// The managed connection ids in the server's answer, one per profile.
pub(crate) fn managed_ids(answer: &Value) -> Vec<String> {
    let mut ids: Vec<String> = answer["matches"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["profile_ids"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .map(connection_id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Sources the batch endpoint takes in one request.
pub(crate) const BATCH_MAX: usize = 200;
/// Single requests in flight at once when the server has no batch endpoint.
const FALLBACK_CONCURRENCY: usize = 4;

/// The batch request body for `chunk`.
pub(crate) fn batch_body(chunk: &[SourceRequest]) -> Value {
    serde_json::json!({ "sources": chunk })
}

/// The ids per source of a batch answer, in the order asked; None when the
/// answer doesn't have one entry per source (it is not the batch answer).
pub(crate) fn batch_ids(answer: &Value, asked: usize) -> Option<Vec<Vec<String>>> {
    let results = answer["results"].as_array()?;
    (results.len() == asked).then(|| results.iter().map(managed_ids).collect())
}

impl Enterprise {
    /// The managed connections (`ent-<profile>`) this person may use that
    /// point at `req`: none, one, or several (the caller asks which). The
    /// server applies the permissions and never says where they point.
    pub async fn resolve_source(&self, req: &SourceRequest) -> CommandResult<Vec<String>> {
        let body = serde_json::to_value(req).unwrap_or(Value::Null);
        let answer = self
            .api(
                reqwest::Method::POST,
                "/desktop/resolve-source",
                Some(&body),
            )
            .await?;
        Ok(managed_ids(&answer))
    }
}

impl Enterprise {
    /// `resolve_source` for many sources at once, one list of ids per source
    /// in the same order. One request per 200 sources
    /// (`POST /desktop/resolve-sources`); a server without that endpoint
    /// (404) or with another answer is asked one source at a time. A source
    /// whose own question fails resolves to no connection; only a lost
    /// session fails the call.
    pub async fn resolve_sources(&self, reqs: &[SourceRequest]) -> CommandResult<Vec<Vec<String>>> {
        let mut out: Vec<Vec<String>> = Vec::with_capacity(reqs.len());
        let mut batch_works = true;
        for chunk in reqs.chunks(BATCH_MAX) {
            if batch_works {
                let answer = self
                    .api(
                        reqwest::Method::POST,
                        "/desktop/resolve-sources",
                        Some(&batch_body(chunk)),
                    )
                    .await;
                match answer {
                    Ok(a) => {
                        if let Some(ids) = batch_ids(&a, chunk.len()) {
                            out.extend(ids);
                            continue;
                        }
                        batch_works = false;
                    }
                    Err(CommandError::NotFound(_)) => batch_works = false,
                    Err(e) => return Err(e),
                }
            }
            for group in chunk.chunks(FALLBACK_CONCURRENCY) {
                let mut asks = Vec::with_capacity(group.len());
                for r in group {
                    asks.push(self.resolve_source(r));
                }
                for r in futures_util::future::join_all(asks).await {
                    out.push(r.unwrap_or_default());
                }
            }
        }
        Ok(out)
    }
}

/// `Enterprise::resolve_sources` on the signed-in session; an error when
/// DBine is not signed in to DBine Enterprise.
pub async fn resolve_sources(reqs: &[SourceRequest]) -> CommandResult<Vec<Vec<String>>> {
    super::get()?.resolve_sources(reqs).await
}

/// `Enterprise::resolve_source` on the signed-in session; an error when
/// DBine is not signed in to DBine Enterprise.
pub async fn resolve_source(req: &SourceRequest) -> CommandResult<Vec<String>> {
    super::get()?.resolve_source(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ext(system: &str, server: Option<&str>, db: Option<&str>) -> ExternalRef {
        ExternalRef {
            system: Some(system.into()),
            server: server.map(String::from),
            database: db.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn a_power_bi_source_becomes_a_request_for_its_engine() {
        let r =
            SourceRequest::from_external(&ext("SQL Server", Some(" dw01\\prod "), Some("sales")))
                .unwrap();
        assert_eq!(r.driver, "sqlserver");
        assert_eq!(r.server, "dw01\\prod");
        assert_eq!(r.database.as_deref(), Some("sales"));
        let r =
            SourceRequest::from_external(&ext("Databricks", Some("adb-1.net"), Some(" "))).unwrap();
        assert_eq!((r.driver.as_str(), r.database), ("databricks", None));
    }

    #[test]
    fn sources_no_connection_can_open_make_no_request() {
        assert!(SourceRequest::from_external(&ext("SharePoint", Some("x"), None)).is_none());
        assert!(SourceRequest::from_external(&ext("SQL Server", None, None)).is_none());
        assert!(SourceRequest::from_external(&ext("SQL Server", Some(" "), None)).is_none());
        assert!(SourceRequest::from_external(&ExternalRef::default()).is_none());
    }

    #[test]
    fn the_wire_body_has_only_what_was_asked() {
        let r = SourceRequest {
            driver: "postgres".into(),
            server: "db".into(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"driver": "postgres", "server": "db"})
        );
    }

    #[test]
    fn the_answer_becomes_one_managed_id_per_profile() {
        let a = json!({"matches": [
            {"connection_id": "c1", "profile_ids": ["p2", "p1"]},
            {"connection_id": "c2", "profile_ids": ["p1", "p3"]},
        ]});
        assert_eq!(managed_ids(&a), vec!["ent-p1", "ent-p2", "ent-p3"]);
        assert!(managed_ids(&json!({"matches": []})).is_empty());
        assert!(managed_ids(&Value::Null).is_empty());
    }

    #[test]
    fn the_batch_body_lists_the_sources_as_the_single_one_does() {
        let r = SourceRequest {
            driver: "sqlserver".into(),
            server: "dw".into(),
            database: Some("sales".into()),
            ..Default::default()
        };
        assert_eq!(
            batch_body(std::slice::from_ref(&r)),
            json!({"sources": [{"driver": "sqlserver", "server": "dw", "database": "sales"}]})
        );
    }

    #[test]
    fn the_batch_answer_is_one_list_of_ids_per_source_in_order() {
        let a = json!({"results": [
            {"matches": [{"connection_id": "c", "profile_ids": ["p2"]}]},
            {"matches": []},
            {"matches": [{"connection_id": "c", "profile_ids": ["p1", "p1"]}]},
        ]});
        assert_eq!(
            batch_ids(&a, 3).unwrap(),
            vec![
                vec!["ent-p2".to_string()],
                vec![],
                vec!["ent-p1".to_string()]
            ]
        );
    }

    #[test]
    fn an_answer_that_is_not_the_batch_one_is_not_taken_for_it() {
        assert!(batch_ids(&json!({"results": []}), 2).is_none());
        assert!(batch_ids(&json!({"matches": []}), 1).is_none());
        assert!(batch_ids(&Value::Null, 0).is_none());
        assert_eq!(batch_ids(&json!({"results": []}), 0).unwrap().len(), 0);
    }
}

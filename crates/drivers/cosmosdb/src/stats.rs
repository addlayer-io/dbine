//! What the service already knows, for documenting a database
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `documentsCount` of `x-ms-resource-usage`, which a container's
//!   read returns with `x-ms-populatequotainfo: true` (the service updates
//!   it every few minutes; `-1` while it doesn't know). Never a
//!   `SELECT VALUE COUNT(1)`, which reads every item and costs RU.
//! - Comments: containers, stored procedures, triggers and UDFs have none.

use crate::{enc, monitor, CosmosSession};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result};
use reqwest::Method;

/// `documentsCount` of a usage header, when the service knows it.
pub(crate) fn documents(usage: &str) -> Option<u64> {
    monitor::parse_usage(usage).get("documentsCount").copied().filter(|n| *n >= 0.0).map(|n| n as u64)
}

pub(crate) async fn row_estimates(s: &CosmosSession) -> Result<Vec<RowEstimate>> {
    let mut out = Vec::new();
    for name in s.containers().await? {
        let link = format!("{}/colls/{name}", s.db_link()?);
        let path = format!("{}/colls/{}", s.db_path()?, enc(&name));
        let quota = [("x-ms-populatequotainfo", "true".to_string())];
        let Ok(r) = s.call(Method::GET, "colls", &link, &path, None, &quota).await else { continue };
        if let Some(rows) = r.usage.as_deref().and_then(documents) {
            out.push(RowEstimate { object: ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name }, rows });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_from_usage() {
        assert_eq!(documents("documentsSize=2;collectionSize=3;documentsCount=5"), Some(5));
        assert_eq!(documents("documentsSize=2;documentsCount=-1"), None);
        assert_eq!(documents("documentsSize=2"), None);
    }
}

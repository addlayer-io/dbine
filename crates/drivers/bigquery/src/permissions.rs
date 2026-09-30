//! What the login may do (`Session::permissions`) in BigQuery, from IAM's
//! own permission test: Resource Manager's `projects.testIamPermissions`
//! (one call, the effective project-level permissions, folder and
//! organization grants included) and, for the backup, BigQuery's
//! `tables.testIamPermissions` on one table of the dataset (which also
//! counts the dataset's access list).
//!
//! Only project-level permissions give a `Denied`: `bigquery.jobs.create`
//! (every script runs as a query job) and `bigquery.datasets.create` can't
//! be granted lower. The others can come from a dataset's access list, so
//! lacking them in the project leaves the action `Unknown`. Against an
//! emulator (no IAM), or when a test fails, everything stays `Unknown`.

use crate::{BigQuerySession, Json};
use dbine_driver::{Access, Permissions, Result};
use serde_json::json;
use std::collections::BTreeSet;
use std::time::Duration;

const CRM: &str = "https://cloudresourcemanager.googleapis.com/v1/projects";

pub const JOBS_CREATE: &str = "bigquery.jobs.create";
pub const JOBS_LIST_ALL: &str = "bigquery.jobs.listAll";
pub const DATASETS_CREATE: &str = "bigquery.datasets.create";
pub const DATASETS_DELETE: &str = "bigquery.datasets.delete";
pub const DATASETS_UPDATE: &str = "bigquery.datasets.update";
pub const TABLES_CREATE: &str = "bigquery.tables.create";
pub const TABLES_SNAPSHOT: &str = "bigquery.tables.createSnapshot";
pub const TABLES_GET_DATA: &str = "bigquery.tables.getData";
pub const TABLES_SET_IAM: &str = "bigquery.tables.setIamPolicy";

/// Tested on the project.
pub const PROJECT: &[&str] = &[
    JOBS_CREATE,
    JOBS_LIST_ALL,
    DATASETS_CREATE,
    DATASETS_DELETE,
    DATASETS_UPDATE,
    TABLES_CREATE,
    TABLES_SNAPSHOT,
    TABLES_GET_DATA,
    TABLES_SET_IAM,
];

/// Tested on a table of the dataset: what a snapshot of it needs.
pub const TABLE: &[&str] = &[TABLES_SNAPSHOT, TABLES_GET_DATA];

/// How long the checks may take before the actions are left `Unknown`.
const LIMIT: Duration = Duration::from_secs(20);

/// A `testIamPermissions` request.
pub fn request(perms: &[&str]) -> Json {
    json!({ "permissions": perms })
}

/// The permissions a `testIamPermissions` response grants (the key is left
/// out when none is).
pub fn granted(resp: &Json) -> BTreeSet<String> {
    resp.get("permissions")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect()
}

/// The project's permissions and, if tested, a table's (effective there).
pub fn map(project: &BTreeSet<String>, table: Option<&BTreeSet<String>>) -> Permissions {
    let has = |p: &str| project.contains(p);
    let no_jobs = || Access::Denied { missing: JOBS_CREATE.into() };
    let backup = if !has(JOBS_CREATE) {
        no_jobs()
    } else {
        let missing = TABLE.iter().find(|p| !has(p) && !table.is_some_and(|t| t.contains(**p)));
        match (missing, table) {
            (None, _) => Access::Allowed,
            (Some(p), Some(_)) => Access::Denied { missing: (*p).into() },
            (Some(_), None) => Access::Unknown,
        }
    };
    let if_granted = |ok: bool| if ok { Access::Allowed } else { Access::Unknown };
    let with_jobs = |ok: bool| if has(JOBS_CREATE) { if_granted(ok) } else { no_jobs() };
    Permissions {
        backup,
        restore: with_jobs(has(TABLES_CREATE)),
        // Without listAll the profiler still shows the login's own jobs.
        profiler: if_granted(has(JOBS_LIST_ALL)),
        create_database: Access::check(has(DATASETS_CREATE), DATASETS_CREATE),
        drop_database: if_granted(has(DATASETS_DELETE)),
        manage_security: with_jobs(has(DATASETS_UPDATE) && has(TABLES_SET_IAM)),
        ..Default::default()
    }
}

impl BigQuerySession {
    async fn test_project(&self) -> Result<BTreeSet<String>> {
        let url = format!("{CRM}/{}:testIamPermissions", self.api.project);
        Ok(granted(&self.api.send(self.api.http.post(url).json(&request(PROJECT))).await?))
    }

    /// The effective snapshot permissions on the dataset's first table;
    /// `None` when it has none.
    async fn test_table(&self, dataset: &str) -> Result<Option<BTreeSet<String>>> {
        let list = self.api.get(&["datasets", dataset, "tables"], &[("maxResults", "1".into())]).await?;
        let Some(table) = list.pointer("/tables/0/tableReference/tableId").and_then(Json::as_str) else { return Ok(None) };
        let verb = format!("{table}:testIamPermissions");
        Ok(Some(granted(&self.api.post(&["datasets", dataset, "tables", &verb], &[], &request(TABLE)).await?)))
    }
}

pub(crate) async fn check(s: &BigQuerySession, database: Option<&str>) -> Permissions {
    if s.api.emulator {
        return Permissions::default();
    }
    let run = async {
        let project = s.test_project().await?;
        let dataset = database.map(str::trim).filter(|d| !d.is_empty()).map(str::to_string).or_else(|| s.dataset.clone());
        let table = match dataset {
            Some(ds) if TABLE.iter().any(|p| !project.contains(*p)) => s.test_table(&ds).await.unwrap_or(None),
            _ => None,
        };
        Ok::<_, dbine_driver::Error>(map(&project, table.as_ref()))
    };
    match tokio::time::timeout(LIMIT, run).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::debug!("bigquery permissions: {e}");
            Permissions::default()
        }
        Err(_) => Permissions::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(perms: &[&str]) -> BTreeSet<String> {
        perms.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn request_and_response() {
        assert_eq!(request(&[JOBS_CREATE])["permissions"], json!(["bigquery.jobs.create"]));
        assert_eq!(granted(&json!({ "permissions": [JOBS_CREATE, DATASETS_CREATE] })), set(&[JOBS_CREATE, DATASETS_CREATE]));
        // None granted: the key is left out.
        assert!(granted(&json!({})).is_empty());
    }

    #[test]
    fn a_project_admin_may_do_everything_offered() {
        let p = map(&set(PROJECT), None);
        for a in [&p.backup, &p.restore, &p.profiler, &p.create_database, &p.drop_database, &p.manage_security] {
            assert_eq!(*a, Access::Allowed);
        }
        assert_eq!(p.kill_session, Access::Unknown);
    }

    #[test]
    fn without_jobs_create_no_script_runs() {
        let p = map(&set(&[TABLES_SNAPSHOT, TABLES_GET_DATA, TABLES_CREATE]), None);
        let denied = Access::Denied { missing: JOBS_CREATE.into() };
        assert_eq!(p.backup, denied);
        assert_eq!(p.restore, denied);
        assert_eq!(p.manage_security, denied);
        assert_eq!(p.create_database, Access::Denied { missing: DATASETS_CREATE.into() });
    }

    #[test]
    fn dataset_level_grants_are_not_denied() {
        let p = map(&set(&[JOBS_CREATE]), None);
        assert_eq!(p.backup, Access::Unknown);
        assert_eq!(p.restore, Access::Unknown);
        assert_eq!(p.profiler, Access::Unknown);
        assert_eq!(p.drop_database, Access::Unknown);
        assert_eq!(p.manage_security, Access::Unknown);
    }

    #[test]
    fn the_table_test_decides_the_backup() {
        let project = set(&[JOBS_CREATE]);
        assert_eq!(map(&project, Some(&set(TABLE))).backup, Access::Allowed);
        assert_eq!(
            map(&project, Some(&set(&[TABLES_GET_DATA]))).backup,
            Access::Denied { missing: TABLES_SNAPSHOT.into() }
        );
        // Split between the project and the table.
        assert_eq!(map(&set(&[JOBS_CREATE, TABLES_SNAPSHOT]), Some(&set(&[TABLES_GET_DATA]))).backup, Access::Allowed);
    }
}

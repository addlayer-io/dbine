//! What the login may do (`Session::permissions`) in Athena. IAM has no
//! permission test for the caller (the policy simulator needs
//! `iam:SimulatePrincipalPolicy` and the caller's own ARN, which a login
//! seldom has) and neither Athena nor Glue have a dry run, so only what a
//! harmless read proves is checked: the profiler's `ListQueryExecutions` on
//! the session's workgroup, the same call it makes. Creating and dropping
//! databases (`glue:CreateDatabase`, `glue:DeleteDatabase` behind the DDL)
//! stay `Unknown`.

use crate::AthenaSession;
use aws_sdk_athena::error::ProvideErrorMetadata;
use dbine_driver::{Access, Permissions};
use std::time::Duration;

pub const LIST_EXECUTIONS: &str = "athena:ListQueryExecutions";

const LIMIT: Duration = Duration::from_secs(20);

/// The profiler's access from the probe's outcome: `None` when it worked,
/// else the error code AWS answered (`None` inside when there was none).
pub fn profiler(outcome: Option<Option<&str>>) -> Access {
    match outcome {
        None => Access::Allowed,
        Some(Some("AccessDeniedException" | "AccessDenied" | "UnauthorizedOperation")) => {
            Access::Denied { missing: LIST_EXECUTIONS.into() }
        }
        // Throttling, a network error…: not an answer.
        Some(_) => Access::Unknown,
    }
}

pub(crate) async fn check(s: &AthenaSession) -> Permissions {
    let probe = s.client.list_query_executions().work_group(&s.workgroup).max_results(1).send();
    let profiler = match tokio::time::timeout(LIMIT, probe).await {
        Ok(Ok(_)) => profiler(None),
        Ok(Err(e)) => {
            tracing::debug!("athena permissions: {e}");
            profiler(Some(e.code()))
        }
        Err(_) => Access::Unknown,
    };
    Permissions { profiler, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_decides_the_profiler() {
        assert_eq!(profiler(None), Access::Allowed);
        assert_eq!(profiler(Some(Some("AccessDeniedException"))), Access::Denied { missing: LIST_EXECUTIONS.into() });
        assert_eq!(profiler(Some(Some("ThrottlingException"))), Access::Unknown);
        assert_eq!(profiler(Some(None)), Access::Unknown);
    }
}

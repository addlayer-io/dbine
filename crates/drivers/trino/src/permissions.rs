//! What the login may do (`Session::permissions`) on Trino, Presto and
//! Starburst. Of the actions DBine checks, only the profiler exists here
//! (catalogs aren't created from DBine, there's no kill from the monitor,
//! no native backups). It reads the coordinator's query list
//! (`GET /v1/query`), which the access control filters to the queries the
//! user may see ("view query" rules) and refuses outright (HTTP 403) only
//! when the user may see none: that refusal is the one thing a read can
//! tell. Roles and grants depend on the connector's access control, which
//! no read reveals, so `manage_security` stays unknown.

use crate::TrinoSession;
use dbine_driver::{Access, Error, Permissions, Result};

/// The profiler's access from the status of `GET /v1/query`.
pub(crate) fn profiler_access(status: u16) -> Access {
    match status {
        200..=299 => Access::Allowed,
        403 => Access::Denied { missing: "view query (control de acceso)".into() },
        _ => Access::Unknown,
    }
}

pub(crate) async fn check(s: &TrinoSession) -> Result<Permissions> {
    let rb = s.conn.http.get(format!("{}/v1/query", s.conn.base)).header(s.flavor.header("user"), &s.conn.user);
    let profiler = match s.conn.auth(rb).send().await {
        Ok(resp) => profiler_access(resp.status().as_u16()),
        Err(e) if e.is_connect() || e.is_timeout() => return Err(Error::Connect(e.to_string())),
        Err(_) => Access::Unknown,
    };
    Ok(Permissions { profiler, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiler_from_the_query_list_status() {
        assert_eq!(profiler_access(200), Access::Allowed);
        assert_eq!(profiler_access(403), Access::Denied { missing: "view query (control de acceso)".into() });
        assert_eq!(profiler_access(500), Access::Unknown);
        assert_eq!(profiler_access(401), Access::Unknown);
    }
}

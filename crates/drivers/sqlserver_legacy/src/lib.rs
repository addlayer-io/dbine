//! SQL Server for servers that only speak TLS 1.0/1.1 (SQL Server 2008 R2 /
//! 2012 without the TLS 1.2 update).
//!
//! The same driver as `dbine-driver-sqlserver`, built with its `legacy-tls`
//! feature: connections that turn on `LegacyTls` negotiate through a
//! vendored OpenSSL (TLS 1.0 floor, security level 0); the rest still go
//! through rustls. It is its own downloadable host so that OpenSSL never
//! ships in the app or in the regular SQL Server host: the app starts this
//! one only for connections the user allowed legacy TLS on
//! (`dbine_drivers::VARIANTS`, docs/tls-options.md).

use dbine_driver::Driver;
use std::sync::Arc;

/// SQL Server only: Azure SQL, Fabric and Babelfish always speak TLS 1.2+.
pub fn drivers() -> Vec<Arc<dyn Driver>> {
    dbine_driver_sqlserver::legacy_tls_drivers()
}

#[cfg(test)]
mod tests {
    #[test]
    fn tracks_the_sqlserver_version() {
        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            dbine_driver_sqlserver::VERSION,
            "bump crates/drivers/sqlserver_legacy/Cargo.toml with crates/drivers/sqlserver/Cargo.toml"
        );
    }

    #[test]
    fn negotiates_legacy_tls() {
        const { assert!(dbine_driver_sqlserver::LEGACY_TLS_BUILT_IN) };
        let ids: Vec<&str> = super::drivers().iter().map(|d| d.info().id).collect();
        assert_eq!(ids, ["sqlserver"]);
    }
}

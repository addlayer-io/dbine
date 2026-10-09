//! Every driver DBine ships, behind one cargo feature per driver crate
//! (a build can leave heavy ones out). Each driver crate exports
//! `pub fn drivers() -> Vec<Arc<dyn Driver>>`: one crate may serve several
//! engines that share a protocol (PostgreSQL, CockroachDB, Redshift…).

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Driver, DriverInfo, Error, Language, Result, Session};
use std::sync::{Arc, OnceLock};

/// Driver crates that stay inside the app in release builds (feature
/// `plugins`); every other one is downloaded on first use.
pub const BUILT_IN: &[&str] = &["sqlite"];

/// Every driver crate built in (its cargo feature name) with the drivers
/// it serves. A driver plugin host serves exactly one of these.
///
/// With `plugins`, only [`BUILT_IN`] ones: the rest are compiled but never
/// referenced, so the linker leaves them out of the app.
macro_rules! packages {
    ($($feature:literal => $krate:ident),* $(,)?) => {
        pub fn packages() -> Vec<(&'static str, Vec<Arc<dyn Driver>>)> {
            #[allow(unused_mut)]
            let mut v: Vec<(&'static str, Vec<Arc<dyn Driver>>)> = Vec::new();
            $(
                #[cfg(feature = $feature)]
                if !cfg!(feature = "plugins") || BUILT_IN.contains(&$feature) {
                    v.push(($feature, $krate::drivers()));
                }
            )*
            v
        }
    };
}

packages! {
    "athena" => dbine_driver_athena,
    "bigquery" => dbine_driver_bigquery,
    "cassandra" => dbine_driver_cassandra,
    "clickhouse" => dbine_driver_clickhouse,
    "cosmosdb" => dbine_driver_cosmosdb,
    "couchdb" => dbine_driver_couchdb,
    "databricks" => dbine_driver_databricks,
    "dsql" => dbine_driver_dsql,
    "duckdb" => dbine_driver_duckdb,
    "dynamodb" => dbine_driver_dynamodb,
    "elasticsearch" => dbine_driver_elasticsearch,
    "firebird" => dbine_driver_firebird,
    "hana" => dbine_driver_hana,
    "influxdb" => dbine_driver_influxdb,
    "iotdb" => dbine_driver_iotdb,
    "ksqldb" => dbine_driver_ksqldb,
    "mongodb" => dbine_driver_mongodb,
    "mysql" => dbine_driver_mysql,
    "odbc" => dbine_driver_odbc,
    "oracle" => dbine_driver_oracle,
    "phoenix" => dbine_driver_phoenix,
    "postgres" => dbine_driver_postgres,
    "redis" => dbine_driver_redis,
    "snowflake" => dbine_driver_snowflake,
    "solr" => dbine_driver_solr,
    "spanner" => dbine_driver_spanner,
    "sqlite" => dbine_driver_sqlite,
    "sqlserver" => dbine_driver_sqlserver,
    "trino" => dbine_driver_trino,
    "libsql" => dbine_driver_libsql,
    "neo4j" => dbine_driver_neo4j,
    "orientdb" => dbine_driver_orientdb,
    "couchbase" => dbine_driver_couchbase,
    "dremio" => dbine_driver_dremio,
    "drill" => dbine_driver_drill,
    "etcd" => dbine_driver_etcd,
    "flightsql" => dbine_driver_flightsql,
    "tdengine" => dbine_driver_tdengine,
}

pub fn all() -> &'static [Arc<dyn Driver>] {
    static ALL: OnceLock<Vec<Arc<dyn Driver>>> = OnceLock::new();
    ALL.get_or_init(|| {
        let built_in = packages();
        #[cfg(feature = "plugins")]
        let downloadable = plugins::drivers(&built_in.iter().map(|(p, _)| *p).collect::<Vec<_>>());
        #[allow(unused_mut)]
        let mut v: Vec<Arc<dyn Driver>> = built_in.into_iter().flat_map(|(_, d)| d).collect();
        #[cfg(feature = "plugins")]
        v.extend(downloadable);
        v.sort_by_key(|d| d.info().name.to_lowercase());
        v
    })
}

/// Drivers downloaded on first use (release builds, feature `plugins`).
/// The catalog the app carries gives each driver crate's floor version; the
/// updater (`dbine_plugin::updater`) moves installed drivers to newer
/// versions published apart from the app.
#[cfg(feature = "plugins")]
pub mod plugins {
    use dbine_driver::Driver;
    use dbine_plugin::install::Catalog;
    use dbine_plugin::updater::{PkgStatus, Updater};
    use dbine_plugin::RemoteDriver;
    use serde::Deserialize;
    use std::sync::{Arc, OnceLock};

    #[derive(Deserialize)]
    struct File {
        #[serde(flatten)]
        catalog: Catalog,
        /// The drivers' manifests, as published (`DriverMeta`s).
        drivers: Vec<serde_json::Value>,
    }

    /// The catalog built in. The release checks it parses
    /// (`dbine-plugin-host --check-catalog`); if it still didn't, the app
    /// runs with the built-in drivers only.
    fn file() -> &'static File {
        static FILE: OnceLock<File> = OnceLock::new();
        FILE.get_or_init(|| {
            serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/plugins.json"))).unwrap_or_else(|e| {
                tracing::error!("catálogo de drivers ilegible: {e}");
                File { catalog: Catalog::default(), drivers: Vec::new() }
            })
        })
    }

    /// Where the downloadable drivers come from (their floor versions).
    pub fn catalog() -> &'static Catalog {
        &file().catalog
    }

    /// Picks, downloads and updates the drivers' hosts.
    pub fn updater() -> &'static Arc<Updater> {
        static UPDATER: OnceLock<Arc<Updater>> = OnceLock::new();
        UPDATER.get_or_init(|| Updater::new(file().catalog.clone(), file().drivers.clone()))
    }

    /// A downloadable driver crate, for the settings page.
    #[derive(Debug, Clone, serde::Serialize)]
    pub struct Package {
        pub package: String,
        /// The engine it's named after ("SQL Server").
        pub label: String,
        /// The driver's own version in use ("1.2.0"), apart from the app's;
        /// when it isn't downloaded, the one a download would get.
        pub version: String,
        /// Its drivers' names (SQL Server, Azure SQL, Fabric…).
        pub drivers: Vec<String>,
        /// Download size (of `available`).
        pub size: u64,
        /// Bytes on disk, when installed.
        pub installed: Option<u64>,
        /// The newest version this app can run.
        pub available: String,
        /// The version "Volver a la anterior" goes back to.
        pub previous: Option<String>,
        pub status: PkgStatus,
        /// A newer version needs this app version.
        pub min_app_needed: Option<String>,
    }

    pub fn packages() -> Vec<Package> {
        let up = updater();
        let pkg_of = |m: &serde_json::Value| m.get("package").and_then(|p| p.as_str()).map(str::to_string);
        let mut names: Vec<String> = file().drivers.iter().filter_map(pkg_of).collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|p| {
                let info = up.info(&p);
                Package {
                    label: up.label(&p),
                    drivers: file()
                        .drivers
                        .iter()
                        .filter(|m| pkg_of(m).as_deref() == Some(p.as_str()))
                        .filter_map(|m| m.get("info").and_then(|i| i.get("name")).and_then(|n| n.as_str()).map(str::to_string))
                        .collect(),
                    version: info.as_ref().map(|i| i.version.clone()).unwrap_or_default(),
                    size: info.as_ref().map(|i| i.size).unwrap_or(0),
                    installed: info.as_ref().and_then(|i| i.installed),
                    available: info.as_ref().map(|i| i.available.clone()).unwrap_or_default(),
                    previous: info.as_ref().and_then(|i| i.previous.clone()),
                    status: info.as_ref().map(|i| i.status.clone()).unwrap_or(PkgStatus::UpToDate),
                    min_app_needed: info.and_then(|i| i.min_app_needed),
                    package: p,
                }
            })
            .collect()
    }

    /// Download a driver now (the settings page's "descargar").
    pub async fn install(package: &str) -> dbine_driver::Result<()> {
        updater().install(package).await
    }

    pub fn remove(package: &str) -> std::io::Result<()> {
        updater().remove(package)
    }

    /// "Buscar actualizaciones": fetch the index now; newer versions of
    /// installed drivers download in the background.
    pub async fn check_updates() -> Result<(), String> {
        updater().check().await
    }

    /// "Volver a la anterior": drop the version in use for the one before.
    pub fn rollback(package: &str) -> Result<(), String> {
        updater().rollback(package)
    }

    /// Called whenever the drivers' versions or statuses change.
    pub fn on_change(f: impl Fn() + Send + Sync + 'static) {
        updater().set_on_change(f);
    }

    /// The periodic update check (run it on the app's async runtime).
    pub async fn run_updater() {
        updater().clone().run().await
    }

    /// A RemoteDriver per downloadable driver not built in, described by
    /// the version each will run.
    pub(crate) fn drivers(built_in: &[&str]) -> Vec<Arc<dyn Driver>> {
        if file().catalog.hosts.is_empty() {
            return Vec::new();
        }
        let up = updater();
        up.gc();
        up.startup_metas()
            .into_iter()
            .filter(|m| !built_in.contains(&m.package.as_str()))
            .map(|m| {
                let launcher = up.launcher(&m.package);
                Arc::new(RemoteDriver::new(m, launcher)) as Arc<dyn Driver>
            })
            .collect()
    }
}

pub fn find(id: &str) -> Option<&'static Arc<dyn Driver>> {
    all().iter().find(|d| d.info().id == id)
}

pub fn infos() -> Vec<DriverInfo> {
    all().iter().map(|d| d.info().clone()).collect()
}

/// Open a session; SQL sessions are wrapped read-only when the connection
/// asks for it (other drivers enforce it themselves).
pub async fn open_session(cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
    let driver = find(&cfg.driver).ok_or_else(|| Error::Unsupported(format!("no hay driver '{}'", cfg.driver)))?;
    let s = driver.connect(cfg, database).await?;
    Ok(if cfg.read_only && driver.info().language == Language::Sql { Box::new(ReadOnlySession::with_dialect(s, driver.script_dialect())) } else { s })
}

#[cfg(test)]
mod tests {
    #[test]
    fn driver_ids_are_unique() {
        let mut ids: Vec<_> = super::all().iter().map(|d| d.info().id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }
}

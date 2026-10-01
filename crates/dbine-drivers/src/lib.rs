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
#[cfg(feature = "plugins")]
pub mod plugins {
    use dbine_driver::Driver;
    use dbine_plugin::install::{self, Catalog};
    use dbine_plugin::{DriverMeta, Launcher, RemoteDriver};
    use serde::Deserialize;
    use std::sync::{Arc, OnceLock};

    #[derive(Deserialize)]
    struct File {
        #[serde(flatten)]
        catalog: Catalog,
        drivers: Vec<DriverMeta>,
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

    /// Where the downloadable drivers come from.
    pub fn catalog() -> &'static Catalog {
        &file().catalog
    }

    /// A downloadable driver crate, for the settings page.
    #[derive(Debug, Clone, serde::Serialize)]
    pub struct Package {
        pub package: String,
        /// The engine it's named after ("SQL Server").
        pub label: String,
        /// The driver's own version ("1.2.0"), apart from the app's.
        pub version: String,
        /// Its drivers' names (SQL Server, Azure SQL, Fabric…).
        pub drivers: Vec<String>,
        /// Download size.
        pub size: u64,
        /// Bytes on disk, when installed.
        pub installed: Option<u64>,
    }

    fn label(package: &str) -> String {
        let metas: Vec<&DriverMeta> = file().drivers.iter().filter(|m| m.package == package).collect();
        metas.iter().find(|m| m.info.id == package).or(metas.first()).map(|m| m.info.name.to_string()).unwrap_or_else(|| package.to_string())
    }

    pub fn packages() -> Vec<Package> {
        let installed = install::installed(catalog());
        let mut names: Vec<&str> = file().drivers.iter().map(|m| m.package.as_str()).collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|p| Package {
                package: p.to_string(),
                label: label(p),
                version: catalog().hosts.get(p).map(|h| h.version.split('+').next().unwrap_or_default().to_string()).unwrap_or_default(),
                drivers: file().drivers.iter().filter(|m| m.package == p).map(|m| m.info.name.to_string()).collect(),
                size: catalog().hosts.get(p).map(|h| h.size).unwrap_or(0),
                installed: installed.iter().find(|(q, _)| q == p).map(|(_, n)| *n),
            })
            .collect()
    }

    /// Download a driver now (the settings page's "descargar").
    pub async fn install(package: &str) -> dbine_driver::Result<()> {
        let ids = file().drivers.iter().filter(|m| m.package == package).map(|m| m.info.id.to_string()).collect();
        install::ensure(catalog(), package, &label(package), ids).await.map(|_| ())
    }

    pub fn remove(package: &str) -> std::io::Result<()> {
        install::remove(catalog(), package)
    }

    /// A RemoteDriver per downloadable driver not built in.
    pub(crate) fn drivers(built_in: &[&str]) -> Vec<Arc<dyn Driver>> {
        let f = file();
        if f.catalog.hosts.is_empty() {
            return Vec::new();
        }
        install::remove_stale(&f.catalog);
        let mut launchers: std::collections::HashMap<String, Arc<Launcher>> = Default::default();
        f.drivers
            .iter()
            .filter(|m| !built_in.contains(&m.package.as_str()))
            .map(|m| {
                let launcher = launchers
                    .entry(m.package.clone())
                    .or_insert_with(|| {
                        let package = m.package.clone();
                        let ids: Vec<String> = f.drivers.iter().filter(|x| x.package == package).map(|x| x.info.id.to_string()).collect();
                        let name = label(&package);
                        Launcher::new(
                            &package.clone(),
                            Some(dbine_driver::runtime::components_dir()),
                            Arc::new(move || {
                                let (package, ids, name) = (package.clone(), ids.clone(), name.clone());
                                Box::pin(async move { install::ensure(catalog(), &package, &name, ids).await })
                            }),
                        )
                    })
                    .clone();
                Arc::new(RemoteDriver::new(m.clone(), launcher)) as Arc<dyn Driver>
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

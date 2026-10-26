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
/// With `plugins`, only [`BUILT_IN`] ones. The release app is built without
/// the other crates' features, so they aren't compiled; a build that has
/// them on anyway never references them, and the linker leaves them out.
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

        /// With `plugins` the app is built without the default features
        /// (only these drivers' crates get compiled): each one's feature
        /// has to be on, which the `plugins` feature does (Cargo.toml).
        #[cfg(feature = "plugins")]
        const _: () = {
            const fn compiled_in(package: &str) -> bool {
                $(
                    if cfg!(feature = $feature) && same(package, $feature) {
                        return true;
                    }
                )*
                false
            }
            const fn same(a: &str, b: &str) -> bool {
                let (a, b) = (a.as_bytes(), b.as_bytes());
                if a.len() != b.len() {
                    return false;
                }
                let mut i = 0;
                while i < a.len() {
                    if a[i] != b[i] {
                        return false;
                    }
                    i += 1;
                }
                true
            }
            let mut i = 0;
            while i < BUILT_IN.len() {
                assert!(compiled_in(BUILT_IN[i]), "a BUILT_IN driver's feature is off: add it to the `plugins` feature");
                i += 1;
            }
        };
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
            serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/plugins.json")))
                .unwrap_or_else(|e| {
                    tracing::error!("catálogo de drivers ilegible: {e}");
                    File {
                        catalog: Catalog::default(),
                        drivers: Vec::new(),
                    }
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
        let pkg_of = |m: &serde_json::Value| {
            m.get("package")
                .and_then(|p| p.as_str())
                .map(str::to_string)
        };
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
                        .filter_map(|m| {
                            m.get("info")
                                .and_then(|i| i.get("name"))
                                .and_then(|n| n.as_str())
                                .map(str::to_string)
                        })
                        .collect(),
                    version: info.as_ref().map(|i| i.version.clone()).unwrap_or_default(),
                    size: info.as_ref().map(|i| i.size).unwrap_or(0),
                    installed: info.as_ref().and_then(|i| i.installed),
                    available: info
                        .as_ref()
                        .map(|i| i.available.clone())
                        .unwrap_or_default(),
                    previous: info.as_ref().and_then(|i| i.previous.clone()),
                    status: info
                        .as_ref()
                        .map(|i| i.status.clone())
                        .unwrap_or(PkgStatus::UpToDate),
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
pub async fn open_session(
    cfg: &ConnectionConfig,
    database: Option<&str>,
) -> Result<Box<dyn Session>> {
    let driver = find(&cfg.driver)
        .ok_or_else(|| Error::Unsupported(format!("no hay driver '{}'", cfg.driver)))?;
    let s = driver.connect(cfg, database).await?;
    Ok(
        if cfg.read_only && driver.info().language == Language::Sql {
            Box::new(ReadOnlySession::with_dialect(s, driver.script_dialect()))
        } else {
            s
        },
    )
}

/// Drivers still on the legacy `encrypt` / `trust_server_certificate`
/// fields, or that don't say how they're encrypted yet (`DriverInfo::tls`).
/// Each TLS batch (docs/engine-support.md, "Connection encryption") takes
/// its drivers out; the registry test fails for a driver listed here that's
/// already migrated, and for one not listed that isn't.
pub const NOT_YET_MIGRATED: &[&str] = &[
    "access",
    "altibase",
    "athena",
    "avatica",
    "azure_databricks",
    "bigquery",
    "cache",
    "cloudera",
    "cosmosdb",
    "cubrid",
    "dameng",
    "databricks",
    "db2",
    "db2i",
    "db2zos",
    "dbase",
    "duckdb",
    "duckdb_files",
    "dynamodb",
    "exasol",
    "firebird",
    "flightsql",
    "gbase8s",
    "heavydb",
    "hive",
    "ignite",
    "ignite3",
    "impala",
    "influxdb",
    "influxdb1",
    "influxdb3",
    "informix",
    "ingres",
    "iotdb",
    "iris",
    "ksqldb",
    "kyuubi",
    "libsql",
    "machbase",
    "maxdb",
    "mimer",
    "monetdb",
    "netezza",
    "netsuite",
    "nuodb",
    "ocient",
    "odbc",
    "openedge",
    "phoenix",
    "snowflake",
    "spanner",
    "spark",
    "sqlanywhere",
    "sqlite",
    "sqream",
    "sybase",
    "tdengine",
    "teradata",
    "timechodb",
    "vertica",
    "virtuoso",
    "zen",
];

#[cfg(test)]
mod tests {
    use dbine_driver::{DriverInfo, Field, TlsRole, TlsSupport};

    /// What's wrong with a driver's TLS declaration.
    fn tls_problems(i: &DriverInfo, docs: &str) -> Vec<String> {
        let mut out = vec![];
        let id = i.id;
        let modes: Vec<&Field> = i
            .fields
            .iter()
            .filter(|f| matches!(f.tls, Some(TlsRole::Mode { .. })))
            .collect();
        if modes.len() > 1 {
            out.push(format!("{id}: more than one TLS mode field"));
        }
        // A declared mode is well formed, migrated or not.
        for f in &modes {
            let key = f.key;
            let Some(TlsRole::Mode { levels }) = &f.tls else {
                continue;
            };
            let known = |v: &str| levels.iter().any(|(x, _)| *x == v);
            match f.tls_level_of(f.default) {
                Some(l) if l.verified() => {}
                _ => out.push(format!(
                    "{id}: `{key}`'s default {:?} isn't a verified TLS level",
                    f.default
                )),
            }
            match &f.legacy {
                None => out.push(format!("{id}: `{key}` has no legacy table")),
                Some(l) => {
                    for v in l.0.iter().filter(|v| !v.is_empty() && !known(v)) {
                        out.push(format!(
                            "{id}: `{key}`'s legacy value {v:?} isn't one of its values"
                        ));
                    }
                }
            }
            if i.tls != TlsSupport::Configurable {
                out.push(format!(
                    "{id}: has a TLS mode field but declares {:?}",
                    i.tls
                ));
            }
        }
        // A verification select caps a mode: one at most, next to a mode,
        // verified by default, with a legacy table of its own values.
        let verifies: Vec<&Field> = i
            .fields
            .iter()
            .filter(|f| matches!(f.tls, Some(TlsRole::Verify { .. })))
            .collect();
        if verifies.len() > 1 {
            out.push(format!("{id}: more than one TLS verification select"));
        }
        for f in &verifies {
            let key = f.key;
            if modes.is_empty() {
                out.push(format!(
                    "{id}: `{key}` is a TLS verification select without a mode field"
                ));
            }
            if !f.tls_level_of(f.default).is_some_and(|l| l.verified()) {
                out.push(format!(
                    "{id}: `{key}`'s default {:?} isn't a verified TLS level",
                    f.default
                ));
            }
            match &f.legacy {
                None => out.push(format!("{id}: `{key}` has no legacy table")),
                Some(l) => {
                    for v in l.0.iter().filter(|v| !v.is_empty()) {
                        if f.tls_level_of(v).is_none() {
                            out.push(format!(
                                "{id}: `{key}`'s legacy value {v:?} isn't one of its values"
                            ));
                        }
                    }
                }
            }
        }
        if i.tls == TlsSupport::None && !docs.contains(&format!("`{id}`")) {
            out.push(format!(
                "{id}: declares no TLS but docs/engine-support.md doesn't list `{id}`"
            ));
        }
        let legacy_fields = i.fields.iter().any(Field::is_legacy_tls);
        let migrated = !legacy_fields && (!modes.is_empty() || i.tls != TlsSupport::Configurable);
        match (super::NOT_YET_MIGRATED.contains(&id), migrated) {
            (true, true) => out.push(format!(
                "{id}: is migrated: take it out of NOT_YET_MIGRATED"
            )),
            (false, false) => out.push(format!(
                "{id}: needs its engine's TLS options (a TlsRole::Mode field) or a DriverInfo::tls declaration, without the legacy encrypt/trust fields"
            )),
            _ => {}
        }
        out
    }

    /// Every engine says how its connections are encrypted: its own TLS
    /// options (a mode field with a secure default and a legacy table) or
    /// `DriverInfo::tls`. The drivers not migrated yet are listed.
    #[test]
    fn every_driver_declares_its_tls() {
        let docs = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/engine-support.md"
        ))
        .unwrap_or_default();
        let problems: Vec<String> = super::all()
            .iter()
            .flat_map(|d| tls_problems(d.info(), &docs))
            .collect();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn the_tls_check_catches_bad_declarations() {
        use dbine_driver::{Family, Language, TlsLevel};
        let info = |fields: Vec<Field>, tls: TlsSupport| DriverInfo {
            id: "tls-check-test",
            name: "x",
            family: Family::Relational,
            language: Language::Sql,
            dialect: "",
            default_port: 1,
            fields,
            databases_label: "",
            has_schemas: false,
            object_kinds: vec![],
            tls,
            tls_port: None,
        };
        let mode = |default: &'static str, legacy: [&'static str; 4]| {
            Field::tls_mode(
                "sslmode",
                "sslmode",
                &[
                    ("disable", "disable", TlsLevel::Off),
                    ("require", "require", TlsLevel::Unverified),
                    ("verify-full", "verify-full", TlsLevel::VerifyFull),
                ],
                default,
                legacy,
            )
        };
        let ok = mode(
            "verify-full",
            ["disable", "require", "verify-full", "require"],
        );
        assert!(tls_problems(
            &info(vec![Field::host(), ok.clone()], TlsSupport::Configurable),
            ""
        )
        .is_empty());
        assert!(tls_problems(&info(vec![], TlsSupport::Forced), "").is_empty());
        // Legacy fields, nothing declared.
        assert_eq!(
            tls_problems(&info(Field::server_set(), TlsSupport::Configurable), "").len(),
            1
        );
        // A plaintext default, an unknown legacy value, no legacy table.
        assert_eq!(
            tls_problems(
                &info(
                    vec![mode(
                        "require",
                        ["disable", "require", "verify-full", "require"]
                    )],
                    TlsSupport::Configurable
                ),
                ""
            )
            .len(),
            1
        );
        assert_eq!(
            tls_problems(
                &info(
                    vec![mode(
                        "verify-full",
                        ["off", "require", "verify-full", "require"]
                    )],
                    TlsSupport::Configurable
                ),
                ""
            )
            .len(),
            1
        );
        let mut no_table = ok.clone();
        no_table.legacy = None;
        assert_eq!(
            tls_problems(&info(vec![no_table], TlsSupport::Configurable), "").len(),
            1
        );
        // No TLS needs its line in the docs.
        assert_eq!(tls_problems(&info(vec![], TlsSupport::None), "").len(), 1);
        assert!(tls_problems(
            &info(vec![], TlsSupport::None),
            "- `tls-check-test`: no TLS"
        )
        .is_empty());
    }

    #[test]
    fn driver_ids_are_unique() {
        let mut ids: Vec<_> = super::all().iter().map(|d| d.info().id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }
}

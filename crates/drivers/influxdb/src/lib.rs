//! InfluxDB over HTTP, one driver per query language:
//! - `influxdb`: InfluxDB 2.x / Cloud with Flux ([`v2`]);
//! - `influxdb1`: InfluxDB 1.x with InfluxQL ([`v1`]);
//! - `influxdb3`: InfluxDB 3 Core / Enterprise with SQL ([`v3`]).
//!
//! Sessions are stateless HTTP clients; measurements are the objects.

mod create_db;
mod csv;
mod http;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod security;
mod transfer;
mod v1;
mod v2;
mod v3;

use dbine_driver::{
    async_trait, Capabilities, ConnectionConfig, CreateTemplate, DdlParts, Driver, DriverInfo, Error, ObjectRef, Result,
    RowChange, Session, TableSchema,
};
use std::sync::Arc;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(InfluxDriver { info: v2::info(), api: Api::Flux }),
        Arc::new(InfluxDriver { info: v1::info(), api: Api::InfluxQl }),
        Arc::new(InfluxDriver { info: v3::info(), api: Api::Sql }),
    ]
}

#[derive(Clone, Copy)]
enum Api {
    Flux,
    InfluxQl,
    Sql,
}

struct InfluxDriver {
    info: DriverInfo,
    api: Api,
}

#[async_trait]
impl Driver for InfluxDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// InfluxQL (v1) and the SQL of v3 take one statement per request, as
    /// the influx CLI sends them; a Flux script (v2) is one query.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        match self.api {
            Api::Sql | Api::InfluxQl => dbine_driver::ScriptMode::PerStatement,
            Api::Flux => dbine_driver::ScriptMode::Whole,
        }
    }

    /// Flux has no statement terminator: the script is one unit.
    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        match self.api {
            Api::Flux => dbine_driver::ScriptDialect { semicolons: false, ..dbine_driver::ScriptDialect::generic() },
            Api::InfluxQl | Api::Sql => dbine_driver::ScriptDialect::generic(),
        }
    }

    /// Line protocol through the write API of each version (see
    /// `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// Not Flux: InfluxDB 2 has no API that lists other clients' queries
    /// (see [`profiler`]).
    fn supports_profiler(&self) -> bool {
        !matches!(self.api, Api::Flux)
    }

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<dbine_driver::Field> {
        create_db::fields(self.api)
    }

    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        create_db::script(self.api, name, options)
    }

    /// Buckets / databases are created and dropped through the HTTP API
    /// (v2, v3) or InfluxQL (v1). No foreign keys in InfluxDB. Running
    /// queries are listed on v1 and v3, and only v1 can stop one (see
    /// [`processes`]).
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: false,
            monitor: true,
            processes: !matches!(self.api, Api::Flux),
            cancel_query: matches!(self.api, Api::InfluxQl),
            ..Default::default()
        }
    }

    /// Users and privileges: InfluxQL only (see [`security`]).
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        matches!(self.api, Api::InfluxQl).then(security::spec)
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        match self.api {
            Api::InfluxQl => security::script(action),
            Api::Flux | Api::Sql => Err(Error::Unsupported(security::TOKENS.into())),
        }
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        match self.api {
            Api::InfluxQl => v1::templates(),
            Api::Flux | Api::Sql => Vec::new(),
        }
    }

    /// Measurements have no DDL: they appear when points are written.
    fn table_ddl(&self, _table: &TableSchema, _parts: DdlParts) -> Result<String> {
        Err(Error::Unsupported(
            "InfluxDB no tiene DDL de measurements: se crean al escribir puntos (line protocol).".into(),
        ))
    }

    /// Measurements (v1, v2) and tables (v3) take their tags and fields
    /// from the points written: there is no schema to alter.
    fn sync_script(&self, _changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        Err(Error::Unsupported(
            "InfluxDB no tiene esquema que sincronizar: los measurements y sus campos aparecen al escribir puntos y no se modifican con DDL".into(),
        ))
    }

    fn insert_script(&self, _target: &ObjectRef, _columns: &[String], _rows: &[Vec<serde_json::Value>]) -> Result<String> {
        Err(Error::Unsupported(
            match self.api {
                Api::Flux => "Flux no tiene un lenguaje de inserción: los puntos se escriben con line protocol (/api/v2/write).",
                Api::InfluxQl => "InfluxQL no tiene INSERT por la API HTTP: los puntos se escriben con line protocol (/write).",
                Api::Sql => "El SQL de InfluxDB 3 es de solo lectura: los puntos se escriben con line protocol (/api/v3/write_lp).",
            }
            .into(),
        ))
    }

    /// Rewriting a point (same measurement, tags and timestamp) overwrites
    /// it, but that is a line-protocol write, which none of the query
    /// languages can express.
    fn update_script(&self, _target: &ObjectRef, _changes: &[RowChange]) -> Result<String> {
        Err(Error::Unsupported(
            match self.api {
                Api::Flux => "Flux no puede modificar puntos: se reescriben con line protocol (/api/v2/write) con la misma marca de tiempo.",
                Api::InfluxQl => "InfluxQL no tiene UPDATE ni INSERT por la API HTTP: los puntos se reescriben con line protocol (/write) con la misma marca de tiempo.",
                Api::Sql => "El SQL de InfluxDB 3 es de solo lectura: los puntos se reescriben con line protocol (/api/v3/write_lp) con la misma marca de tiempo.",
            }
            .into(),
        ))
    }

    /// InfluxQL (1.x) deletes points by time and tags; Flux and the SQL of
    /// InfluxDB 3 have no statement for it.
    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        match self.api {
            Api::InfluxQl => v1::delete_script(&target.name, keys),
            Api::Flux => Err(Error::Unsupported(
                "Flux no puede borrar puntos: se borran con la API de borrado (/api/v2/delete), con un rango de tiempo y un predicado sobre los tags.".into(),
            )),
            Api::Sql => Err(Error::Unsupported(
                "El SQL de InfluxDB 3 es de solo lectura y no borra puntos sueltos: solo se pueden borrar bases o tablas enteras.".into(),
            )),
        }
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        match self.api {
            Api::Flux => v2::filtered_browse(browse, filters),
            Api::InfluxQl => v1::filtered_browse(browse, filters),
            Api::Sql => v3::filtered_browse(browse, filters),
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        match self.api {
            Api::Flux => v2::connect(cfg, database).await,
            Api::InfluxQl => v1::connect(cfg, database).await,
            Api::Sql => v3::connect(cfg, database).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_are_unsupported() {
        let target = ObjectRef { kind: "measurement".into(), schema: None, name: "cpu".into() };
        let change = RowChange { key: vec![("time".into(), serde_json::json!(1))], set: vec![("v".into(), serde_json::json!(2))], ..Default::default() };
        for d in drivers() {
            assert!(matches!(d.update_script(&target, &[change.clone()]), Err(Error::Unsupported(_))));
        }
    }
}

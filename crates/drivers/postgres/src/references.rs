//! What an object uses (`Session::references`), per variant:
//!
//! - PostgreSQL and the variants that keep its catalog: `pg_depend` (see
//!   [`crate::pg_references`]), a table's foreign keys (the query of
//!   `index_usage`) and a foreign table's remote source (`external`).
//!   Confirmed. A routine whose body the catalog doesn't track (PL/pgSQL,
//!   a string body, any before PostgreSQL 14) adds what the generic scan
//!   finds in its text (probable).
//! - Redshift: views from `pg_depend` and `pg_rewrite`, plus the text scan
//!   (late-binding views record nothing); the rest, the text scan.
//! - Yellowbrick: PostgreSQL's catalog without policies, inheritance or
//!   foreign tables, plus the text scan.
//! - CockroachDB: tables, views and materialized views from
//!   `crdb_internal.backward_dependencies` (the tables, views and sequences
//!   they use; `allow_unsafe_internals` is set for the read only) and the
//!   foreign keys; another database's objects go in `external`. It doesn't
//!   expose what functions use, nor what views call: the text scan adds
//!   them, and routines are only scanned.
//! - Materialize: `mz_internal.mz_object_dependencies` (user objects);
//!   another database's objects and the connections sources and sinks use
//!   go in `external`.
//! - RisingWave: `rw_catalog.rw_depend`; a source's or a connector table's
//!   connector (`rw_sources.connector_props`, plaintext properties only) in
//!   `external`.
//! - Denodo, CrateDB, H2: the generic scan.
//!
//! If the catalog can't be read (a privilege, a catalog the server lacks)
//! the answer is the generic scan's, with a note saying why.

use crate::catalog::{cell, lit};
use crate::index_usage::{foreign_keys_sql, group_foreign_keys, rel};
use crate::pg_references::{self as pr, Catalog};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::dependencies::{
    foreign_key_parents, references_scan, DependencyReport, DependencyScan, DependencyTarget,
    ExternalRef,
};
use dbine_driver::{kinds, Error, ObjectRef, Result, Session};
use tokio_postgres::SimpleQueryRow;

/// The catalog's answer, and whether it covers everything the target uses
/// (otherwise the text scan is added).
type Read = Option<(DependencyReport, bool)>;

/// Variants whose `pg_depend` is PostgreSQL's, tested or a close fork: what
/// it records for views, triggers and tables is all they use.
fn trusts_catalog(v: Variant) -> bool {
    matches!(
        v,
        Variant::Postgres
            | Variant::Timescale
            | Variant::AlloyDb
            | Variant::CloudSql
            | Variant::Aurora
            | Variant::Edb
            | Variant::Fujitsu
            | Variant::Yugabyte
            | Variant::Greenplum
            | Variant::Cloudberry
            | Variant::Greengage
            | Variant::Kingbase
            | Variant::OpenGauss
    )
}

/// The five columns every query here answers, as [`pr::used`] takes them.
fn row5(r: &SimpleQueryRow) -> [Option<String>; 5] {
    ["kind", "sch", "name", "rel", "detail"].map(|c| cell(r, c))
}

/// A `p->'key'` property of RisingWave's `connector_props`, only when it's
/// plaintext (a secret's reference is never read).
fn rw_prop(keys: &[&str]) -> String {
    let parts: Vec<String> = keys
        .iter()
        .map(|k| format!("CASE WHEN p->'{k}'->>'type' = 'plaintext' THEN p->'{k}'->>'value' END"))
        .collect();
    format!("COALESCE({})", parts.join(", "))
}

/// CockroachDB: what `schema`.`name` (literals) uses, from
/// `crdb_internal.backward_dependencies` (foreign keys come from
/// `pg_constraint`), named by `crdb_internal.tables`, which has every
/// database's (`create_statements` of every database, `""`, fails on a
/// lookup by id); the kind from `pg_class`, which has the current one's.
/// `db` is set for another database's.
pub(crate) fn cockroach_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT DISTINCT
                CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized_view'
                               WHEN 'S' THEN 'sequence' ELSE 'table' END AS kind,
                t.schema_name AS sch, t.name AS name,
                CASE WHEN t.database_name <> current_database() THEN t.database_name END AS db,
                'code' AS rel, NULL::STRING AS detail
         FROM crdb_internal.backward_dependencies b
         JOIN crdb_internal.tables t ON t.table_id = b.dependson_id
         LEFT JOIN pg_class c ON c.oid::INT8 = b.dependson_id
         WHERE b.dependson_type <> 'fk'
           AND b.descriptor_id = (SELECT c.oid::INT8 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                                   WHERE c.relname = {name}
                                     AND (n.nspname = {schema} OR ({schema} = '' AND pg_table_is_visible(c.oid)))
                                   LIMIT 1)
         ORDER BY 2, 3"
    )
}

/// Materialize's object type for a kind.
fn mz_type(kind: &str) -> Option<&'static str> {
    Some(match kind {
        kinds::TABLE => "table",
        kinds::VIEW => "view",
        kinds::MATERIALIZED_VIEW => "materialized-view",
        crate::SOURCE => "source",
        crate::SINK => "sink",
        _ => return None,
    })
}

/// Materialize: the user objects `schema`.`name` (literals, of type `ty`)
/// depends on in `mz_internal.mz_object_dependencies`: `db` for another
/// database's, `kind` `connection` with its type in `detail`.
pub(crate) fn materialize_sql(ty: &str, schema: &str, name: &str) -> String {
    format!(
        "SELECT DISTINCT CASE r.type WHEN 'materialized-view' THEN 'materialized_view' ELSE r.type END AS kind,
                rs.name AS sch, r.name AS name,
                CASE WHEN rd.name IS DISTINCT FROM current_database() THEN rd.name END AS db,
                'code' AS rel, cn.type AS detail
         FROM mz_internal.mz_object_dependencies d
         JOIN mz_catalog.mz_objects o ON o.id = d.object_id
         JOIN mz_catalog.mz_schemas os ON os.id = o.schema_id
         JOIN mz_catalog.mz_databases od ON od.id = os.database_id
         JOIN mz_catalog.mz_objects r ON r.id = d.referenced_object_id
         JOIN mz_catalog.mz_schemas rs ON rs.id = r.schema_id
         LEFT JOIN mz_catalog.mz_databases rd ON rd.id = rs.database_id
         LEFT JOIN mz_catalog.mz_connections cn ON cn.id = r.id
         WHERE o.name = {name} AND o.type = '{ty}'
           AND os.name = COALESCE(NULLIF({schema}, ''), current_schema()) AND od.name = current_database()
           AND r.id LIKE 'u%'
           AND r.type IN ('table', 'view', 'materialized-view', 'source', 'sink', 'connection')
         ORDER BY 2, 3"
    )
}

/// RisingWave's relation type for a kind.
fn rw_type(kind: &str) -> Option<&'static str> {
    Some(match kind {
        kinds::TABLE => "table",
        kinds::VIEW => "view",
        kinds::MATERIALIZED_VIEW => "materialized view",
        crate::SOURCE => "source",
        crate::SINK => "sink",
        _ => return None,
    })
}

/// RisingWave: the relations `schema`.`name` (literals, of type `ty`)
/// depends on in `rw_catalog.rw_depend`.
pub(crate) fn risingwave_sql(ty: &str, schema: &str, name: &str) -> String {
    format!(
        "SELECT DISTINCT CASE r.relation_type WHEN 'materialized view' THEN 'materialized_view' ELSE r.relation_type END AS kind,
                rs.name AS sch, r.name AS name, 'code' AS rel, NULL AS detail
         FROM rw_catalog.rw_depend d
         JOIN rw_catalog.rw_relations o ON o.id = d.objid
         JOIN rw_catalog.rw_schemas os ON os.id = o.schema_id
         JOIN rw_catalog.rw_relations r ON r.id = d.refobjid
         JOIN rw_catalog.rw_schemas rs ON rs.id = r.schema_id
         WHERE o.name = {name} AND o.relation_type = '{ty}'
           AND os.name = COALESCE(NULLIF({schema}, ''), current_schema())
           AND r.relation_type IN ('table', 'view', 'materialized view', 'source', 'sink')
         ORDER BY 2, 3"
    )
}

/// RisingWave: the connector a source or a connector table reads, with
/// its plaintext host, database, schema and topic or table.
pub(crate) fn risingwave_connector_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT lower(s.connector) AS sys, {host} AS host, {port} AS port, {db} AS db, {sch} AS sch, {obj} AS obj
         FROM (SELECT s.connector, s.connector_props AS p FROM rw_catalog.rw_sources s
                 JOIN rw_catalog.rw_schemas sc ON sc.id = s.schema_id
                WHERE s.name = {name} AND sc.name = COALESCE(NULLIF({schema}, ''), current_schema())) s",
        host = rw_prop(&["properties.bootstrap.server", "hostname", "host", "endpoint"]),
        port = rw_prop(&["port"]),
        db = rw_prop(&["database.name", "database"]),
        sch = rw_prop(&["schema.name"]),
        obj = rw_prop(&["topic", "table.name", "stream", "s3.bucket_name", "collection.name"]),
    )
}

/// A row of [`risingwave_connector_sql`] as the source it reads.
fn rw_connector(r: [Option<String>; 6]) -> Option<ExternalRef> {
    let [sys, host, port, db, sch, obj] = r;
    Some(ExternalRef {
        system: sys,
        server: match (host, port) {
            (Some(h), Some(p)) if !h.contains(':') && !h.contains(',') => Some(format!("{h}:{p}")),
            (h, _) => h,
        },
        database: db,
        schema: sch,
        name: obj,
        detail: None,
        mentions: Vec::new(),
    })
}

/// Rows of the variants' queries with a `db` column: the current
/// database's are items, another's are `external` (of `system`).
/// Materialize's connections are `external` too, of their type.
fn split_rows(rows: &[SimpleQueryRow], system: &str) -> DependencyReport {
    let mut report = DependencyReport::default();
    for r in rows {
        let Some(item) = pr::used(row5(r)) else {
            continue;
        };
        if item.kind == "connection" {
            report.external.push(ExternalRef {
                system: item.detail.clone(),
                detail: Some(match &item.schema {
                    Some(s) => format!("{s}.{}", item.name),
                    None => item.name.clone(),
                }),
                ..Default::default()
            });
        } else if let Some(db) = cell(r, "db") {
            report.external.push(ExternalRef {
                system: Some(system.into()),
                database: Some(db),
                schema: item.schema,
                name: Some(item.name),
                ..Default::default()
            });
        } else {
            report.items.push(item);
        }
    }
    report
}

impl PgSession {
    pub(crate) async fn references_impl(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = &target.object;
        let read = match self.variant {
            Variant::Denodo | Variant::CrateDb | Variant::H2 => Ok(None),
            Variant::Redshift if t.kind != kinds::VIEW => Ok(None),
            Variant::Cockroach => self.cockroach_uses(t).await,
            Variant::Materialize => self.materialize_uses(t).await,
            Variant::RisingWave => self.risingwave_uses(t).await,
            _ => self.pg_uses(t).await,
        };
        let (mut report, complete) = match read {
            Ok(Some(r)) => r,
            Ok(None) => return references_scan(self, target, scan).await,
            Err(e @ (Error::Query(_) | Error::Statement(_) | Error::Unsupported(_))) => {
                tracing::debug!("{:?}: dependency catalog unavailable: {e}", self.variant);
                let mut r = references_scan(self, target, scan).await?;
                r.note = Some(pr::unread_note(&e));
                return Ok(r);
            }
            Err(e) => return Err(e),
        };
        if !complete {
            report = pr::merge(report, references_scan(self, target, scan).await?);
        }
        let unmentioned = report.items.iter().any(|d| d.mentions.is_empty());
        if unmentioned && scan.written_kinds.contains(&t.kind) {
            if let Ok(Some(body)) = Session::definition(self, t).await {
                report.scanned = report.scanned.max(1);
                pr::add_mentions(&mut report.items, &body, &scan.dialect);
            }
        }
        pr::sort(&mut report.items);
        Ok(report)
    }

    fn pg_catalog(&self) -> Catalog {
        Catalog {
            version: self.version,
            full: !matches!(self.variant, Variant::Redshift | Variant::Yellowbrick),
        }
    }

    /// PostgreSQL's catalog (see [`crate::pg_references`]).
    async fn pg_uses(&self, t: &ObjectRef) -> Result<Read> {
        let v = self.variant;
        let c = self.pg_catalog();
        let (s, n) = (lit(v, t.schema().unwrap_or("")), lit(v, &t.name));
        let Some(sql) = pr::uses_sql(c, &t.kind, &s, &n, |col| self.filter(col)) else {
            return Ok(None);
        };
        let mut report = DependencyReport {
            items: self
                .text(&sql)
                .await?
                .iter()
                .filter_map(|r| pr::used(row5(r)))
                .collect(),
            ..Default::default()
        };
        let mut complete = trusts_catalog(v);
        if t.kind == kinds::TABLE {
            let fks = self
                .text(&foreign_keys_sql(&rel(v, t.schema(), &t.name)))
                .await?;
            report
                .items
                .extend(foreign_key_parents(&group_foreign_keys(&fks), t));
            if let Some(sql) = pr::foreign_table_sql(c, &s, &n) {
                match self.text(&sql).await {
                    Ok(rows) => report.external.extend(rows.iter().filter_map(|r| {
                        pr::foreign_source(
                            ["fdw", "srv", "srvopts", "ftopts", "sch", "name"].map(|k| cell(r, k)),
                        )
                    })),
                    Err(e) => tracing::debug!("{v:?}: foreign tables unavailable: {e}"),
                }
            }
        }
        if matches!(t.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE) {
            // Only SQL-standard bodies are in the catalog.
            let sql_bodies = match pr::sql_bodies_sql(c, &t.kind, &s, &n) {
                Some(sql) => match self.text(&sql).await {
                    Ok(rows) => rows.first().and_then(|r| r.get(0)) == Some("true"),
                    Err(e) => {
                        tracing::debug!("{v:?}: prosqlbody unavailable: {e}");
                        false
                    }
                },
                None => false,
            };
            complete &= sql_bodies;
        }
        Ok(Some((report, complete)))
    }

    /// `crdb_internal.backward_dependencies` and the foreign keys.
    async fn cockroach_uses(&self, t: &ObjectRef) -> Result<Read> {
        if !matches!(
            t.kind.as_str(),
            kinds::TABLE | kinds::VIEW | kinds::MATERIALIZED_VIEW
        ) {
            return Ok(None);
        }
        let v = self.variant;
        let sql = cockroach_sql(&lit(v, t.schema().unwrap_or("")), &lit(v, &t.name));
        // Since v25 crdb_internal is closed unless the session opts in.
        let _ = self
            .client
            .batch_execute("SET allow_unsafe_internals = true")
            .await;
        let rows = self.text(&sql).await;
        let _ = self
            .client
            .batch_execute("RESET allow_unsafe_internals")
            .await;
        let mut report = split_rows(&rows?, "CockroachDB");
        if t.kind == kinds::TABLE {
            let fks = self
                .text(&foreign_keys_sql(&rel(v, t.schema(), &t.name)))
                .await?;
            report
                .items
                .extend(foreign_key_parents(&group_foreign_keys(&fks), t));
        }
        // Neither the functions a view calls nor what a routine uses.
        Ok(Some((report, false)))
    }

    /// `mz_internal.mz_object_dependencies`.
    async fn materialize_uses(&self, t: &ObjectRef) -> Result<Read> {
        let Some(ty) = mz_type(&t.kind) else {
            return Ok(None);
        };
        let v = self.variant;
        let sql = materialize_sql(ty, &lit(v, t.schema().unwrap_or("")), &lit(v, &t.name));
        Ok(Some((
            split_rows(&self.text(&sql).await?, "Materialize"),
            true,
        )))
    }

    /// `rw_catalog.rw_depend`, and the connector of a source or table.
    async fn risingwave_uses(&self, t: &ObjectRef) -> Result<Read> {
        let Some(ty) = rw_type(&t.kind) else {
            return Ok(None);
        };
        let v = self.variant;
        let (s, n) = (lit(v, t.schema().unwrap_or("")), lit(v, &t.name));
        let mut report = DependencyReport {
            items: self
                .text(&risingwave_sql(ty, &s, &n))
                .await?
                .iter()
                .filter_map(|r| pr::used(row5(r)))
                .collect(),
            ..Default::default()
        };
        if matches!(t.kind.as_str(), kinds::TABLE | crate::SOURCE) {
            match self.text(&risingwave_connector_sql(&s, &n)).await {
                Ok(rows) => report.external.extend(rows.iter().filter_map(|r| {
                    rw_connector(["sys", "host", "port", "db", "sch", "obj"].map(|k| cell(r, k)))
                })),
                Err(e) => tracing::debug!("risingwave: connectors unavailable: {e}"),
            }
        }
        Ok(Some((report, true)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn variants_trusted_alone_are_postgresql_and_its_forks() {
        assert!(trusts_catalog(Variant::Postgres) && trusts_catalog(Variant::Yugabyte));
        for v in [
            Variant::Redshift,
            Variant::Yellowbrick,
            Variant::Cockroach,
            Variant::Materialize,
            Variant::RisingWave,
        ] {
            assert!(!trusts_catalog(v), "{v:?}");
        }
    }

    #[test]
    fn cockroach_reads_backward_dependencies_by_the_relations_id() {
        let q = cockroach_sql("E'ventas'", "E'v'");
        assert!(q.contains("FROM crdb_internal.backward_dependencies b"));
        assert!(q.contains("JOIN crdb_internal.tables t ON t.table_id = b.dependson_id"));
        assert!(q.contains("LEFT JOIN pg_class c ON c.oid::INT8 = b.dependson_id"));
        assert!(q.contains("b.dependson_type <> 'fk'"));
        assert!(q.contains("c.relname = E'v'") && q.contains("n.nspname = E'ventas'"));
        assert!(q.contains(
            "CASE WHEN t.database_name <> current_database() THEN t.database_name END AS db"
        ));
        assert!(q.contains("WHEN 'm' THEN 'materialized_view'"));
    }

    #[test]
    fn materialize_and_risingwave_read_their_dependency_catalogs() {
        assert_eq!(mz_type("materialized_view"), Some("materialized-view"));
        assert_eq!(mz_type("function"), None);
        let q = materialize_sql("view", "'s'", "'v'");
        assert!(q.contains("FROM mz_internal.mz_object_dependencies d"));
        assert!(q.contains("o.name = 'v' AND o.type = 'view'"));
        assert!(q.contains("r.id LIKE 'u%'"));
        assert!(q.contains("LEFT JOIN mz_catalog.mz_connections cn ON cn.id = r.id"));

        assert_eq!(rw_type("materialized_view"), Some("materialized view"));
        let q = risingwave_sql("materialized view", "'s'", "'mv'");
        assert!(q.contains("FROM rw_catalog.rw_depend d"));
        assert!(q.contains("o.relation_type = 'materialized view'"));
        let c = risingwave_connector_sql("'s'", "'src'");
        assert!(
            c.contains("CASE WHEN p->'topic'->>'type' = 'plaintext' THEN p->'topic'->>'value' END")
        );
        assert!(c.contains("lower(s.connector) AS sys"));
        assert!(!c.contains("password"));
    }

    #[test]
    fn a_connector_row_is_its_source() {
        let k =
            rw_connector([s("kafka"), s("broker:9092"), None, None, None, s("pedidos")]).unwrap();
        assert_eq!(
            (k.system.as_deref(), k.server.as_deref(), k.name.as_deref()),
            (Some("kafka"), Some("broker:9092"), Some("pedidos"))
        );
        let pg = rw_connector([
            s("postgres-cdc"),
            s("db"),
            s("5432"),
            s("ventas"),
            s("public"),
            s("pedidos"),
        ])
        .unwrap();
        assert_eq!(
            (
                pg.server.as_deref(),
                pg.database.as_deref(),
                pg.schema.as_deref()
            ),
            (Some("db:5432"), Some("ventas"), Some("public"))
        );
    }
}

//! "Nueva base de datos" (a keyspace) with options
//! ([`dbine_driver::Driver::create_database_fields`]).
//!
//! - Cassandra and ScyllaDB: the replication class (NetworkTopologyStrategy,
//!   the default, or SimpleStrategy), the replication factor or one factor
//!   per datacenter, and `durable_writes`. ScyllaDB also takes `tablets`.
//! - Amazon Keyspaces: SingleRegionStrategy or NetworkTopologyStrategy with
//!   the regions (Keyspaces always keeps three replicas per region).
//!
//! With no options it's the old create: NetworkTopologyStrategy with one
//! replica per datacenter. Every value is checked before it reaches the CQL.

use crate::{ddl, CassandraSession, Flavor};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use scylla::value::{CqlValue, Row};
use std::collections::BTreeMap;

pub(crate) const NTS: &str = "NetworkTopologyStrategy";

pub(crate) fn fields(f: Flavor) -> Vec<Field> {
    let yes_no = || FieldKind::Select(vec![("true", "Sí"), ("false", "No")]);
    let dcs = |label: &'static str, help: &'static str| {
        Field::new("datacenters", label, FieldKind::Text).placeholder("dc1:3, dc2:2").help(help).when("class", &["", NTS])
    };
    match f {
        Flavor::Cassandra | Flavor::Scylla => {
            let mut out = vec![
                Field::new(
                    "class",
                    "Replicación (class)",
                    FieldKind::Select(vec![(NTS, "Por datacenter (NetworkTopologyStrategy)"), ("SimpleStrategy", "Simple (SimpleStrategy)")]),
                )
                .help("Vacía: NetworkTopologyStrategy. SimpleStrategy ignora los datacenters: solo para un cluster de un datacenter."),
                Field::new("replication_factor", "Factor de replicación (replication_factor)", FieldKind::Number)
                    .help("Vacío: 1. Con NetworkTopologyStrategy se aplica a cada datacenter que no tenga su propio factor."),
                dcs(
                    "Factores por datacenter",
                    "datacenter:réplicas separados por comas. Un datacenter sin «:n» toma el factor de replicación.",
                ),
                Field::new("durable_writes", "Escrituras durables (durable_writes)", yes_no())
                    .help("Vacío: sí. No: las escrituras no pasan por el commit log (se pierden si cae un nodo)."),
            ];
            if f == Flavor::Scylla {
                out.push(
                    Field::new("tablets", "Tablets", yes_no())
                        .help("Vacío: lo que diga el servidor (tablets activos por defecto desde ScyllaDB 6). No funciona con SimpleStrategy."),
                );
            }
            out
        }
        Flavor::Keyspaces => vec![
            Field::new(
                "class",
                "Replicación (class)",
                FieldKind::Select(vec![("SingleRegionStrategy", "Una región (SingleRegionStrategy)"), (NTS, "Varias regiones (NetworkTopologyStrategy)")]),
            )
            .help("Vacía: la del servidor, como hasta ahora."),
            Field::new("datacenters", "Regiones", FieldKind::Text)
                .placeholder("us-east-1, eu-west-1")
                .help("Las regiones del keyspace multirregión, separadas por comas (3 réplicas cada una).")
                .when("class", &[NTS]),
        ],
    }
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

pub(crate) fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

pub(crate) fn factor(v: &str, what: &str) -> Result<u32> {
    v.parse::<u32>().ok().filter(|n| (1..=100).contains(n)).ok_or_else(|| bad(what, v))
}

/// A datacenter or region name, as a CQL string literal.
pub(crate) fn dc_literal(v: &str) -> Result<String> {
    if v.is_empty() || v.len() > 128 || !v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ')) {
        return Err(bad("datacenter", v));
    }
    Ok(format!("'{}'", v.replace('\'', "''")))
}

fn yes_no(o: &BTreeMap<String, String>, key: &str, what: &str) -> Result<Option<bool>> {
    match opt(o, key) {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(v) => Err(bad(what, v)),
    }
}

/// The `CREATE KEYSPACE` for `name` with options `o`.
pub(crate) fn script(f: Flavor, name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let ks = ddl::keyspace_name(name)?;
    let class = opt(o, "class");
    let mut with = Vec::new();
    if f == Flavor::Keyspaces {
        match class {
            None => {
                // Keyspaces had the generic create so far: keep it.
                return Ok(format!("CREATE KEYSPACE {ks} WITH replication = {{'class': '{NTS}', 'replication_factor': 1}}"));
            }
            Some("SingleRegionStrategy") => with.push("replication = {'class': 'SingleRegionStrategy'}".to_string()),
            Some(NTS) => {
                let regions = opt(o, "datacenters").ok_or_else(|| Error::Query("indicá al menos una región".into()))?;
                let mut parts = vec![format!("'class': '{NTS}'")];
                for r in regions.split(',').map(str::trim).filter(|r| !r.is_empty()) {
                    parts.push(format!("{}: 3", dc_literal(r)?));
                }
                with.push(format!("replication = {{{}}}", parts.join(", ")));
            }
            Some(c) => return Err(bad("replicación", c)),
        }
        return Ok(format!("CREATE KEYSPACE {ks} WITH {}", with.join(" AND ")));
    }

    let rf = opt(o, "replication_factor").map(|v| factor(v, "factor de replicación")).transpose()?.unwrap_or(1);
    let class = class.unwrap_or(NTS);
    let replication = match class {
        "SimpleStrategy" => format!("{{'class': 'SimpleStrategy', 'replication_factor': {rf}}}"),
        NTS => match opt(o, "datacenters") {
            None => format!("{{'class': '{NTS}', 'replication_factor': {rf}}}"),
            Some(list) => {
                let mut parts = vec![format!("'class': '{NTS}'")];
                for item in list.split(',').map(str::trim).filter(|i| !i.is_empty()) {
                    let (dc, n) = match item.rsplit_once(':') {
                        Some((dc, n)) => (dc.trim(), factor(n.trim(), "réplicas del datacenter")?),
                        None => (item, rf),
                    };
                    parts.push(format!("{}: {n}", dc_literal(dc)?));
                }
                if parts.len() == 1 {
                    return Err(bad("factores por datacenter", list));
                }
                format!("{{{}}}", parts.join(", "))
            }
        },
        c => return Err(bad("replicación", c)),
    };
    with.push(format!("replication = {replication}"));
    if let Some(d) = yes_no(o, "durable_writes", "escrituras durables")? {
        with.push(format!("durable_writes = {d}"));
    }
    if f == Flavor::Scylla {
        if let Some(t) = yes_no(o, "tablets", "tablets")? {
            with.push(format!("tablets = {{'enabled': {t}}}"));
        }
    }
    Ok(format!("CREATE KEYSPACE {ks} WITH {}", with.join(" AND ")))
}

fn text(r: &Row) -> Option<String> {
    match r.columns.first() {
        Some(Some(CqlValue::Text(s) | CqlValue::Ascii(s))) => Some(s.clone()),
        _ => None,
    }
}

impl CassandraSession {
    /// The cluster's datacenters (from `system.local` and `system.peers`),
    /// and the defaults of a plain create.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let mut dcs = Vec::new();
        for cql in ["SELECT data_center FROM system.local", "SELECT data_center FROM system.peers"] {
            if let Ok(rows) = self.rows(cql, ()).await {
                dcs.extend(rows.iter().filter_map(text));
            }
        }
        dcs.sort();
        dcs.dedup();
        let mut out = vec![FieldChoices { key: "datacenters".into(), default: None, values: dcs }];
        if self.flavor != Flavor::Keyspaces {
            out.push(FieldChoices { key: "class".into(), default: Some(NTS.into()), values: Vec::new() });
            out.push(FieldChoices { key: "replication_factor".into(), default: Some("1".into()), values: Vec::new() });
            out.push(FieldChoices { key: "durable_writes".into(), default: Some("true".into()), values: Vec::new() });
        }
        Ok(out)
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
        }
        let cql = script(self.flavor, name, o)?;
        self.session.query_unpaged(cql, ()).await.map_err(|e| Error::Query(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        for f in [Flavor::Cassandra, Flavor::Scylla, Flavor::Keyspaces] {
            assert_eq!(
                script(f, "app", &o(&[("class", " ")])).unwrap(),
                "CREATE KEYSPACE app WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}"
            );
        }
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script(Flavor::Cassandra, "App", &o(&[("class", "SimpleStrategy"), ("replication_factor", "3"), ("durable_writes", "false")])).unwrap(),
            "CREATE KEYSPACE \"App\" WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 3} AND durable_writes = false"
        );
        assert_eq!(
            script(Flavor::Scylla, "a", &o(&[("replication_factor", "2"), ("datacenters", "dc1:3, dc2"), ("tablets", "false")])).unwrap(),
            "CREATE KEYSPACE a WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': 3, 'dc2': 2} AND tablets = {'enabled': false}"
        );
        // Tablets only on ScyllaDB.
        assert!(!script(Flavor::Cassandra, "a", &o(&[("tablets", "false")])).unwrap().contains("tablets"));
        assert_eq!(
            script(Flavor::Keyspaces, "a", &o(&[("class", "SingleRegionStrategy")])).unwrap(),
            "CREATE KEYSPACE a WITH replication = {'class': 'SingleRegionStrategy'}"
        );
        assert_eq!(
            script(Flavor::Keyspaces, "a", &o(&[("class", NTS), ("datacenters", "us-east-1, eu-west-1")])).unwrap(),
            "CREATE KEYSPACE a WITH replication = {'class': 'NetworkTopologyStrategy', 'us-east-1': 3, 'eu-west-1': 3}"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("class", "LocalStrategy"),
            ("replication_factor", "0"),
            ("replication_factor", "1; DROP"),
            ("datacenters", "dc1'):1"),
            ("datacenters", "dc1:x"),
            ("datacenters", " , "),
            ("durable_writes", "maybe"),
        ] {
            assert!(script(Flavor::Cassandra, "a", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Flavor::Scylla, "a", &o(&[("tablets", "1")])).is_err());
        assert!(script(Flavor::Keyspaces, "a", &o(&[("class", NTS)])).is_err(), "regions are required");
        assert!(script(Flavor::Cassandra, "a-b", &o(&[])).is_err());
    }
}

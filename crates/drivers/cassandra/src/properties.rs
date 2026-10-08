//! "Propiedades" of a keyspace ([`dbine_driver::Session::database_properties`]):
//! what `system_schema` reports and `ALTER KEYSPACE` changes.
//!
//! - Cassandra and ScyllaDB: the replication (class, factor, or the factor
//!   of each datacenter) and `durable_writes`. A ScyllaDB keyspace with
//!   tablets only takes NetworkTopologyStrategy, and whether it uses
//!   tablets can't be changed after it's created (it's shown as a fact).
//! - Amazon Keyspaces: adding a region (its only `ALTER KEYSPACE` besides
//!   tags), which turns on client-side timestamps as AWS requires.
//!
//! The replication map of `ALTER KEYSPACE` replaces the whole map, so the
//! datacenters are one field with every factor ("dc1:3, dc2:2"): the
//! statement then holds the full map, exactly as it runs. Each changed
//! setting is one statement; the replication goes last.
//!
//! ScyllaDB puts `durable_writes` back to true on any `ALTER KEYSPACE`
//! that doesn't name it. So there the replication and `durable_writes` go
//! in one statement, and the replication fields of a keyspace without
//! durable writes carry the [`KEEP_NON_DURABLE`] prefix in their keys,
//! which makes the statement keep `durable_writes = false`.

use crate::create_db::{bad, dc_literal, factor, NTS};
use crate::{ddl, is_system_keyspace, CassandraSession, Flavor};
use dbine_driver::serde_static::intern;
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use scylla::value::{CqlValue, Row};
use std::collections::BTreeMap;

const SIMPLE: &str = "SimpleStrategy";

/// Key prefix of ScyllaDB's replication fields when the keyspace has
/// `durable_writes = false` (see the module docs).
const KEEP_NON_DURABLE: &str = "nd:";

fn yes(v: &str) -> bool {
    matches!(v.trim(), "true" | "1")
}

/// `dc1:3, dc2:2` → `'dc1': 3, 'dc2': 2`. Every datacenter needs its
/// factor; 0 drops the datacenter's replicas.
fn dc_factors(list: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in list.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (dc, n) = item.rsplit_once(':').ok_or_else(|| bad("factores por datacenter (datacenter:réplicas)", item))?;
        let n = n.trim();
        let n: u32 = n.parse().ok().filter(|n| *n <= 100).ok_or_else(|| bad("réplicas del datacenter", n))?;
        out.push(format!("{}: {n}", dc_literal(dc.trim())?));
    }
    if out.is_empty() {
        return Err(bad("factores por datacenter", list));
    }
    Ok(out)
}

/// The statements for `changes`: `durable_writes` first, then the
/// replication (Keyspaces: the regions).
pub(crate) fn alter(f: Flavor, keyspace: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if is_system_keyspace(keyspace.trim()) {
        return Err(Error::Query(format!("{keyspace} es un keyspace del sistema: no se modifica.")));
    }
    let ks = ddl::keyspace_name(keyspace)?;
    let allowed: &[&str] = if f == Flavor::Keyspaces { &["regions"] } else { &["durable_writes", "class", "replication_factor", "datacenters"] };
    let mut keep_non_durable = false;
    let mut plain = BTreeMap::new();
    for (k, v) in changes {
        let key = match k.strip_prefix(KEEP_NON_DURABLE) {
            Some(rest) if f == Flavor::Scylla && rest != "durable_writes" => {
                keep_non_durable = true;
                rest
            }
            _ => k.as_str(),
        };
        if !allowed.contains(&key) {
            return Err(Error::Query(format!("propiedad desconocida: {k}")));
        }
        plain.insert(key.to_string(), v.clone());
    }
    let changes = &plain;
    let get = |k: &str| changes.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let mut out = Vec::new();
    if f == Flavor::Keyspaces {
        if let Some(list) = get("regions") {
            let regions: Vec<&str> = list.split(',').map(str::trim).filter(|r| !r.is_empty()).collect();
            if regions.len() < 2 {
                return Err(Error::Query("indicá las regiones actuales del keyspace y la nueva (al menos dos)".into()));
            }
            let mut parts = vec![format!("'class': '{NTS}'")];
            for r in regions {
                parts.push(format!("{}: '3'", dc_literal(r)?));
            }
            out.push(format!("ALTER KEYSPACE {ks} WITH REPLICATION = {{{}}} AND CLIENT_SIDE_TIMESTAMPS = {{'status': 'ENABLED'}}", parts.join(", ")));
        }
        return Ok(out);
    }
    let (class, rf, dcs) = (get("class"), get("replication_factor"), get("datacenters"));
    let replicate = class.is_some() || rf.is_some() || dcs.is_some();
    let durable = changes.get("durable_writes").map(|d| yes(d));
    if let Some(d) = durable.filter(|_| f != Flavor::Scylla || !replicate) {
        out.push(format!("ALTER KEYSPACE {ks} WITH durable_writes = {d}"));
    }
    if replicate {
        // Without the class, the field that changed says which one it is.
        let class = class.unwrap_or(if dcs.is_some() { NTS } else { SIMPLE });
        let replication = match class {
            SIMPLE => {
                let rf = rf.ok_or_else(|| Error::Query("indicá el factor de replicación".into()))?;
                format!("{{'class': '{SIMPLE}', 'replication_factor': {}}}", factor(rf, "factor de replicación")?)
            }
            NTS => {
                let dcs = dcs.ok_or_else(|| Error::Query("indicá el factor de cada datacenter (dc1:3, dc2:2)".into()))?;
                format!("{{'class': '{NTS}', {}}}", dc_factors(dcs)?.join(", "))
            }
            c => return Err(bad("replicación", c)),
        };
        // ScyllaDB: durable_writes in the same statement, or it goes back to true.
        let durable = match (f, durable) {
            (Flavor::Scylla, Some(d)) => format!(" AND durable_writes = {d}"),
            (Flavor::Scylla, None) if keep_non_durable => " AND durable_writes = false".into(),
            _ => String::new(),
        };
        out.push(format!("ALTER KEYSPACE {ks} WITH replication = {replication}{durable}"));
    }
    Ok(out)
}

pub(crate) fn script(f: Flavor, keyspace: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(f, keyspace, changes)?.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"))
}

fn cql_text(v: &CqlValue) -> Option<String> {
    match v {
        CqlValue::Text(s) | CqlValue::Ascii(s) => Some(s.clone()),
        CqlValue::Int(n) => Some(n.to_string()),
        CqlValue::BigInt(n) => Some(n.to_string()),
        CqlValue::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn col(r: &Row, i: usize) -> Option<&CqlValue> {
    r.columns.get(i).and_then(Option::as_ref)
}

fn fact(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value: value.into() }
}

impl CassandraSession {
    pub(crate) async fn properties(&mut self, keyspace: &str) -> Result<DatabaseProperties> {
        let name = keyspace.trim();
        let rows = self.rows("SELECT durable_writes, replication FROM system_schema.keyspaces WHERE keyspace_name = ?", (name,)).await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe el keyspace «{name}»")))?;
        let durable = !matches!(col(r, 0), Some(CqlValue::Boolean(false)));
        let mut repl: BTreeMap<String, String> = match col(r, 1) {
            Some(CqlValue::Map(pairs)) => pairs.iter().filter_map(|(k, v)| Some((cql_text(k)?, cql_text(v)?))).collect(),
            _ => BTreeMap::new(),
        };
        let class = repl.remove("class").map(|c| c.rsplit('.').next().unwrap_or_default().to_string()).unwrap_or_default();
        let system = is_system_keyspace(name);

        let mut info = vec![fact("", "Replicación (class)", class.clone())];
        let factors = repl.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", ");
        if !factors.is_empty() {
            info.push(fact("", "Réplicas", factors));
        }
        if self.flavor != Flavor::Keyspaces {
            info.push(fact("", "Escrituras durables (durable_writes)", if durable { "Sí" } else { "No" }));
        }
        for (table, label) in [("tables", "Tablas"), ("views", "Vistas materializadas"), ("types", "Tipos"), ("functions", "Funciones"), ("aggregates", "Agregados")] {
            if let Ok(rows) = self.rows(&format!("SELECT keyspace_name FROM system_schema.{table} WHERE keyspace_name = ?"), (name,)).await {
                info.push(fact("", label, rows.len().to_string()));
            }
        }
        // ScyllaDB: tablets (system_schema.scylla_keyspaces, ScyllaDB 6+).
        let mut tablets = false;
        if self.flavor == Flavor::Scylla {
            if let Ok(rows) = self.rows("SELECT initial_tablets FROM system_schema.scylla_keyspaces WHERE keyspace_name = ?", (name,)).await {
                let initial = rows.first().and_then(|r| col(r, 0)).and_then(cql_text);
                tablets = initial.is_some();
                info.push(fact("", "Tablets", if tablets { "Sí" } else { "No" }));
                if let Some(n) = initial.filter(|n| n != "0") {
                    info.push(fact("", "Tablets iniciales", n));
                }
            }
        }
        if system {
            info.push(fact("", "Keyspace del sistema", "No se modifica"));
        }

        let mut values = BTreeMap::new();
        let mut fields = Vec::new();
        let mut warnings = BTreeMap::new();
        let mut choices = Vec::new();
        if system {
            return Ok(DatabaseProperties { fields, values, info, choices, warnings });
        }
        if self.flavor == Flavor::Keyspaces {
            let regions: Vec<String> = if class == NTS {
                repl.keys().cloned().collect()
            } else {
                // A single-region keyspace lives in the endpoint's region.
                self.rows("SELECT data_center FROM system.local", ()).await.ok().and_then(|r| r.first().and_then(|r| col(r, 0)).and_then(cql_text)).into_iter().collect()
            };
            fields.push(
                Field::new("regions", "Regiones", FieldKind::Text)
                    .placeholder("us-east-1, eu-west-1")
                    .help("Las regiones actuales y una nueva al final, separadas por comas. Se agrega una región por vez y no se pueden quitar.")
                    .group("Replicación"),
            );
            values.insert("regions".into(), regions.join(", "));
            warnings.insert(
                "regions".into(),
                "Agregar una región replica todas las tablas en ella (una restauración entre regiones, con costo por GB), activa los timestamps del lado del cliente (CLIENT_SIDE_TIMESTAMPS) en todas las tablas y no se puede deshacer.".into(),
            );
            return Ok(DatabaseProperties { fields, values, info, choices, warnings });
        }

        fields.push(
            Field::new("durable_writes", "Escrituras durables (durable_writes)", FieldKind::Bool)
                .help("Desactivadas, las escrituras de este keyspace no pasan por el commit log."),
        );
        values.insert("durable_writes".into(), if durable { "true".into() } else { String::new() });
        warnings.insert(
            "durable_writes".into(),
            "Sin escrituras durables (durable_writes = false) las escrituras no pasan por el commit log: si un nodo se cae, se pierde lo que aún no estaba en disco.".into(),
        );

        // ScyllaDB without durable writes: the replication keys keep it.
        let key = |k: &str| -> &'static str {
            if self.flavor == Flavor::Scylla && !durable {
                intern(&format!("{KEEP_NON_DURABLE}{k}"))
            } else {
                intern(k)
            }
        };
        let (k_class, k_rf, k_dcs) = (key("class"), key("replication_factor"), key("datacenters"));
        // A rack list (ScyllaDB tablets) isn't a number: it's left as a fact.
        let numeric = repl.values().all(|v| v.parse::<u32>().is_ok());
        if (class == NTS || class == SIMPLE) && numeric {
            let dcs = |when: bool| {
                let f = Field::new(k_dcs, "Factores por datacenter", FieldKind::Text)
                    .placeholder("dc1:3, dc2:2")
                    .help("datacenter:réplicas de cada datacenter, separados por comas. Un datacenter que no figura (o con 0) pierde sus réplicas.")
                    .group("Replicación");
                if when {
                    f.when(k_class, &[NTS])
                } else {
                    f
                }
            };
            if tablets {
                // Tablets: NetworkTopologyStrategy only; ±1 replica per change.
                fields.push(dcs(false).help(
                    "datacenter:réplicas de cada datacenter, separados por comas. Con tablets, cada cambio sube o baja a lo sumo una réplica por datacenter.",
                ));
            } else {
                fields.extend([
                    Field::new(
                        k_class,
                        "Replicación (class)",
                        FieldKind::Select(vec![(NTS, "Por datacenter (NetworkTopologyStrategy)"), (SIMPLE, "Simple (SimpleStrategy)")]),
                    )
                    .help("SimpleStrategy ignora los datacenters: solo para un cluster de un datacenter.")
                    .group("Replicación"),
                    Field::new(k_rf, "Factor de replicación (replication_factor)", FieldKind::Number)
                        .group("Replicación")
                        .when(k_class, &[SIMPLE]),
                    dcs(true),
                ]);
                values.insert(k_class.into(), class.clone());
                values.insert(k_rf.into(), if class == SIMPLE { repl.get("replication_factor").cloned().unwrap_or_default() } else { String::new() });
            }
            values.insert(
                k_dcs.into(),
                if class == NTS { repl.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(", ") } else { String::new() },
            );
            let repair = if tablets {
                "Cambiar la replicación mueve los datos en segundo plano (tablets) y puede tardar; al subir el factor, conviene reiniciar las aplicaciones para que vean las réplicas nuevas."
            } else {
                "Cambiar la replicación no copia los datos existentes: al subir el factor o sumar un datacenter hay que correr una reparación completa (nodetool repair --full) en cada nodo; al bajarlo, nodetool cleanup. Hasta entonces, las lecturas pueden no encontrar datos."
            };
            for k in [k_class, k_rf, k_dcs] {
                warnings.insert(k.to_string(), repair.to_string());
            }
            if let Ok(c) = self.create_database_choices_impl().await {
                choices.extend(c.into_iter().filter(|c| c.key == "datacenters").map(|c| FieldChoices { key: k_dcs.into(), default: None, ..c }));
            }
        }
        Ok(DatabaseProperties { fields, values, info, choices, warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, keyspace: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden modificar las propiedades de una base.".into()));
        }
        let statements = alter(self.flavor, keyspace, changes)?;
        for (i, cql) in statements.iter().enumerate() {
            if let Err(e) = self.session.query_unpaged(cql.as_str(), ()).await {
                let e = Error::Query(e.to_string());
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {cql}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn each_change_in_order() {
        assert_eq!(
            script(Flavor::Cassandra, "app", &c(&[("datacenters", "dc1:3, dc2:0"), ("durable_writes", "")])).unwrap(),
            "ALTER KEYSPACE app WITH durable_writes = false;\nALTER KEYSPACE app WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': 3, 'dc2': 0};"
        );
        assert_eq!(
            script(Flavor::Scylla, "App", &c(&[("class", SIMPLE), ("replication_factor", "2"), ("datacenters", "")])).unwrap(),
            "ALTER KEYSPACE \"App\" WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 2};"
        );
        // Only the replication factor changed: it's SimpleStrategy's.
        assert_eq!(
            script(Flavor::Cassandra, "a", &c(&[("replication_factor", "3")])).unwrap(),
            "ALTER KEYSPACE a WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 3};"
        );
        assert_eq!(script(Flavor::Cassandra, "a", &c(&[("durable_writes", "true")])).unwrap(), "ALTER KEYSPACE a WITH durable_writes = true;");
        assert_eq!(script(Flavor::Cassandra, "a", &c(&[])).unwrap(), "");
    }

    #[test]
    fn scylla_keeps_durable_writes() {
        // ScyllaDB resets durable_writes unless the statement names it.
        assert_eq!(
            script(Flavor::Scylla, "a", &c(&[("datacenters", "dc1:2"), ("durable_writes", "")])).unwrap(),
            "ALTER KEYSPACE a WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': 2} AND durable_writes = false;"
        );
        assert_eq!(
            script(Flavor::Scylla, "a", &c(&[("nd:datacenters", "dc1:2")])).unwrap(),
            "ALTER KEYSPACE a WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': 2} AND durable_writes = false;"
        );
        assert_eq!(
            script(Flavor::Scylla, "a", &c(&[("nd:datacenters", "dc1:2"), ("durable_writes", "true")])).unwrap(),
            "ALTER KEYSPACE a WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': 2} AND durable_writes = true;"
        );
        assert_eq!(
            script(Flavor::Scylla, "a", &c(&[("datacenters", "dc1:2")])).unwrap(),
            "ALTER KEYSPACE a WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': 2};"
        );
        assert!(script(Flavor::Cassandra, "a", &c(&[("nd:datacenters", "dc1:2")])).is_err(), "only on ScyllaDB");
    }

    #[test]
    fn keyspaces_adds_a_region() {
        assert_eq!(
            script(Flavor::Keyspaces, "a", &c(&[("regions", "us-east-1, eu-west-1")])).unwrap(),
            "ALTER KEYSPACE a WITH REPLICATION = {'class': 'NetworkTopologyStrategy', 'us-east-1': '3', 'eu-west-1': '3'} AND CLIENT_SIDE_TIMESTAMPS = {'status': 'ENABLED'};"
        );
        assert!(script(Flavor::Keyspaces, "a", &c(&[("regions", "us-east-1")])).is_err());
        assert!(script(Flavor::Keyspaces, "a", &c(&[("durable_writes", "")])).is_err());
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("class", "LocalStrategy"),
            ("class", NTS),
            ("class", SIMPLE),
            ("replication_factor", "0"),
            ("replication_factor", "1; DROP"),
            ("datacenters", "dc1"),
            ("datacenters", "dc1:x"),
            ("datacenters", "dc1'):1"),
            ("datacenters", "dc1:101"),
            ("datacenters", " , "),
            ("regions", "us-east-1, eu-west-1"),
            ("tablets", "true"),
        ] {
            assert!(script(Flavor::Cassandra, "a", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Flavor::Cassandra, "system_auth", &c(&[("durable_writes", "true")])).is_err(), "system keyspace");
        assert!(script(Flavor::Cassandra, "a-b", &c(&[("durable_writes", "true")])).is_err());
    }
}

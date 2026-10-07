//! "Nueva base de datos" with options
//! ([`dbine_driver::Driver::create_database_fields`]): the shards (`q`),
//! the replicas (`n`) and whether the database is partitioned, as query
//! parameters of `PUT /{db}`. Every value is checked before it reaches the
//! request.

use crate::{ddl, seg, CouchSession};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use reqwest::Method;
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("q", "Shards (q)", FieldKind::Number).help("Vacío: el q del cluster. En cuántas partes se divide la base."),
        Field::new("n", "Réplicas (n)", FieldKind::Number)
            .help("Vacío: el n del cluster. Copias de cada shard; no puede superar la cantidad de nodos."),
        Field::new("partitioned", "Particionada (partitioned)", FieldKind::Bool)
            .help("Los _id llevan la partición adelante («partición:id») y las consultas por partición son más rápidas. Desde CouchDB 3."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// The path and query of the `PUT` that creates `name`.
pub(crate) fn path(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    ddl::check_database_name(name)?;
    let mut query = Vec::new();
    for (key, what) in [("q", "shards"), ("n", "réplicas")] {
        if let Some(v) = opt(o, key) {
            let n = v.parse::<u32>().ok().filter(|n| (1..=1024).contains(n)).ok_or_else(|| Error::Query(format!("{what}: «{v}» no es un valor válido")))?;
            query.push(format!("{key}={n}"));
        }
    }
    match opt(o, "partitioned") {
        None | Some("false") => {}
        Some("true") => query.push("partitioned=true".into()),
        Some(v) => return Err(Error::Query(format!("particionada: «{v}» no es un valor válido"))),
    }
    let q = if query.is_empty() { String::new() } else { format!("?{}", query.join("&")) };
    Ok(format!("/{}{q}", seg(name)))
}

/// What "Ver script" shows: the request.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    Ok(format!("PUT {}", path(name, o)?))
}

impl CouchSession {
    /// The cluster's `q` and `n` (`[cluster]` of the node's config; needs
    /// an admin, otherwise nothing is suggested).
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let Ok(c) = self.call(Method::GET, "/_node/_local/_config/cluster", None).await else {
            return Ok(Vec::new());
        };
        Ok(["q", "n"]
            .into_iter()
            .filter_map(|k| c.get(k).and_then(Value::as_str).map(|v| FieldChoices { key: k.into(), default: Some(v.into()), values: Vec::new() }))
            .collect())
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        self.refuse_if_read_only("crear una base")?;
        let p = path(name, o)?;
        self.call(Method::PUT, &p, None).await.map(|_| ())
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
        assert_eq!(script("ventas", &o(&[("q", " ")])).unwrap(), "PUT /ventas");
        assert_eq!(script("a/b", &o(&[])).unwrap(), "PUT /a%2Fb");
    }

    #[test]
    fn every_option() {
        assert_eq!(script("ventas", &o(&[("q", "8"), ("n", "1"), ("partitioned", "true")])).unwrap(), "PUT /ventas?q=8&n=1&partitioned=true");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("q", "0"), ("q", "8&n=9"), ("n", "-1"), ("partitioned", "yes")] {
            assert!(script("v", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script("Ventas", &o(&[])).is_err());
    }
}

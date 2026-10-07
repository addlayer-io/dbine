//! "Nueva base de datos" with options
//! ([`dbine_driver::Driver::create_database_fields`]):
//!
//! - Neo4j (Enterprise, which is the edition that creates databases): the
//!   topology (primaries and secondaries of a cluster) and the `OPTIONS`
//!   map (store format, transaction log enrichment for CDC, and a backup
//!   or dump to seed it from).
//! - Memgraph: `CREATE DATABASE` takes only the name.
//! - Neptune: one database per cluster, nothing to create.
//!
//! Every value is checked before it reaches the Cypher.

use crate::{cypher, enterprise_hint, Flavor, GraphSession};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields(f: Flavor) -> Vec<Field> {
    if f != Flavor::Neo4j {
        return Vec::new();
    }
    vec![
        Field::new("primaries", "Primarios (TOPOLOGY … PRIMARIES)", FieldKind::Number)
            .help("Vacío: lo que diga el cluster (1 en un servidor solo). Copias que aceptan escrituras."),
        Field::new("secondaries", "Secundarios (SECONDARIES)", FieldKind::Number).help("Vacío: 0. Copias de solo lectura."),
        Field::new(
            "store_format",
            "Formato de almacenamiento (storeFormat)",
            FieldKind::Select(vec![("block", "block"), ("aligned", "aligned"), ("standard", "standard"), ("high_limit", "high_limit")]),
        )
        .help("Vacío: el del servidor (db.format)."),
        Field::new(
            "tx_log_enrichment",
            "Enriquecer el log de transacciones (txLogEnrichment)",
            FieldKind::Select(vec![("OFF", "No (OFF)"), ("DIFF", "Solo los cambios (DIFF)"), ("FULL", "Completo (FULL)")]),
        )
        .help("Vacío: OFF. DIFF o FULL activan la captura de cambios (CDC)."),
        Field::new("seed_uri", "Sembrar desde (seedURI)", FieldKind::Text)
            .placeholder("s3://bucket/respaldo.backup")
            .help("Vacío: base nueva. Un backup o dump que el servidor pueda leer (file:, s3:, gs:, azb:, https:)."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

fn count(v: &str, what: &str, min: u32) -> Result<u32> {
    v.parse::<u32>().ok().filter(|n| (min..=100).contains(n)).ok_or_else(|| bad(what, v))
}

/// A Cypher string literal.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The statement that creates `name` (on `system` for Neo4j).
pub(crate) fn script(f: Flavor, name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let n = cypher::ident(name.trim());
    match f {
        Flavor::Neptune => return Err(Error::Unsupported("Neptune tiene una sola base por cluster".into())),
        Flavor::Memgraph => return Ok(format!("CREATE DATABASE {n}")),
        Flavor::Neo4j => {}
    }
    let mut out = format!("CREATE DATABASE {n} IF NOT EXISTS");
    let primaries = opt(o, "primaries").map(|v| count(v, "primarios", 1)).transpose()?;
    let secondaries = opt(o, "secondaries").map(|v| count(v, "secundarios", 0)).transpose()?;
    if primaries.is_some() || secondaries.is_some() {
        let p = primaries.unwrap_or(1);
        out.push_str(&format!(" TOPOLOGY {p} {}", if p == 1 { "PRIMARY" } else { "PRIMARIES" }));
        if let Some(s) = secondaries {
            out.push_str(&format!(" {s} {}", if s == 1 { "SECONDARY" } else { "SECONDARIES" }));
        }
    }
    let mut options = Vec::new();
    if let Some(s) = opt(o, "store_format") {
        if !matches!(s, "block" | "aligned" | "standard" | "high_limit") {
            return Err(bad("formato de almacenamiento", s));
        }
        options.push(format!("storeFormat: '{s}'"));
    }
    if let Some(t) = opt(o, "tx_log_enrichment") {
        if !matches!(t, "OFF" | "DIFF" | "FULL") {
            return Err(bad("enriquecer el log", t));
        }
        options.push(format!("txLogEnrichment: '{t}'"));
    }
    if let Some(u) = opt(o, "seed_uri") {
        let scheme_ok = ["file:", "s3:", "gs:", "azb:", "http:", "https:", "ftp:"].iter().any(|s| u.to_ascii_lowercase().starts_with(s));
        if !scheme_ok || u.chars().any(char::is_control) {
            return Err(bad("sembrar desde", u));
        }
        options.push(format!("existingData: 'use', seedURI: {}", literal(u)));
    }
    if !options.is_empty() {
        out.push_str(&format!(" OPTIONS {{{}}}", options.join(", ")));
    }
    out.push_str(" WAIT");
    Ok(out)
}

impl GraphSession {
    /// Neo4j's default store format (`db.format`).
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        if self.flavor != Flavor::Neo4j {
            return Ok(Vec::new());
        }
        let format = self.strings("SHOW SETTINGS YIELD name, value WHERE name = 'db.format' RETURN value").await.ok().and_then(|v| v.into_iter().next());
        Ok(vec![FieldChoices { key: "store_format".into(), default: format, values: Vec::new() }])
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        self.refuse_if_read_only("crear una base")?;
        let q = script(self.flavor, name, o)?;
        match self.flavor {
            Flavor::Neo4j => self.query_on(&q, Some("system")).await.map_err(enterprise_hint)?,
            _ => self.query(&q).await?,
        };
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
        assert_eq!(script(Flavor::Neo4j, "ventas", &o(&[("primaries", " ")])).unwrap(), "CREATE DATABASE ventas IF NOT EXISTS WAIT");
        assert_eq!(script(Flavor::Memgraph, "ventas-2", &o(&[])).unwrap(), "CREATE DATABASE `ventas-2`");
        assert!(script(Flavor::Neptune, "v", &o(&[])).is_err());
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script(
                Flavor::Neo4j,
                "v",
                &o(&[
                    ("primaries", "3"),
                    ("secondaries", "1"),
                    ("store_format", "block"),
                    ("tx_log_enrichment", "FULL"),
                    ("seed_uri", "s3://b/it's.backup"),
                ])
            )
            .unwrap(),
            "CREATE DATABASE v IF NOT EXISTS TOPOLOGY 3 PRIMARIES 1 SECONDARY \
             OPTIONS {storeFormat: 'block', txLogEnrichment: 'FULL', existingData: 'use', seedURI: 's3://b/it\\'s.backup'} WAIT"
        );
        assert_eq!(script(Flavor::Neo4j, "v", &o(&[("secondaries", "0")])).unwrap(), "CREATE DATABASE v IF NOT EXISTS TOPOLOGY 1 PRIMARY 0 SECONDARIES WAIT");
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("primaries", "0"),
            ("primaries", "1 PRIMARY"),
            ("secondaries", "-1"),
            ("store_format", "fast"),
            ("tx_log_enrichment", "full"),
            ("seed_uri", "/tmp/x.dump"),
            ("seed_uri", "file:/x\n}"),
        ] {
            assert!(script(Flavor::Neo4j, "v", &o(&[bad])).is_err(), "{bad:?}");
        }
    }
}

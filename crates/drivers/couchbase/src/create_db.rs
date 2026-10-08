//! "Nueva base de datos" (a bucket) with options
//! ([`dbine_driver::Driver::create_database_fields`]): the bucket type, RAM
//! quota, replicas, eviction policy, minimum durability level, storage
//! backend, maximum TTL and flush, as the form of
//! `POST /pools/default/buckets`. With no options it's the old create:
//! a Couchbase bucket of 100 MB without flush.
//!
//! Every value is checked before it goes into the form, and the form is
//! sent exactly as "Ver script" shows it.

use crate::{encode, q, CbSession};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

const PATH: &str = "/pools/default/buckets";

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new(
            "bucket_type",
            "Tipo de bucket (bucketType)",
            FieldKind::Select(vec![("couchbase", "Couchbase (en disco)"), ("ephemeral", "Efímero (solo en memoria)")]),
        )
        .help("Vacío: Couchbase."),
        Field::new("ram_quota", "Memoria (ramQuota, MB)", FieldKind::Number).help("Vacía: 100 MB, el mínimo. Sale de la cuota libre del cluster."),
        Field::new("replicas", "Réplicas (replicaNumber)", FieldKind::Select(vec![("0", "0"), ("1", "1"), ("2", "2"), ("3", "3")]))
            .help("Vacío: 1. Hacen falta tantos nodos de datos como réplicas más uno."),
        Field::new(
            "eviction",
            "Política de desalojo (evictionPolicy)",
            FieldKind::Select(vec![
                ("valueOnly", "Solo valores (valueOnly)"),
                ("fullEviction", "Completa (fullEviction)"),
                ("noEviction", "Sin desalojo (noEviction)"),
                ("nruEviction", "Menos usados (nruEviction)"),
            ]),
        )
        .help("Couchbase: valueOnly o fullEviction. Efímero: noEviction o nruEviction. Vacía: la del tipo."),
        Field::new(
            "durability",
            "Durabilidad mínima (durabilityMinLevel)",
            FieldKind::Select(vec![
                ("none", "Ninguna (none)"),
                ("majority", "Mayoría (majority)"),
                ("majorityAndPersistActive", "Mayoría y disco en el activo (majorityAndPersistActive)"),
                ("persistToMajority", "Disco en la mayoría (persistToMajority)"),
            ]),
        )
        .help("Vacía: none. Las que escriben a disco no van con un bucket efímero."),
        Field::new(
            "storage_backend",
            "Almacenamiento (storageBackend)",
            FieldKind::Select(vec![("couchstore", "Couchstore"), ("magma", "Magma")]),
        )
        .help("Vacío: el del cluster. Magma pide más memoria (1024 MB antes de Couchbase 7.6).")
        .when("bucket_type", &["", "couchbase"]),
        Field::new("max_ttl", "Vida máxima de los documentos (maxTTL, segundos)", FieldKind::Number).help("Vacía o 0: no vencen. Solo en Couchbase Enterprise."),
        Field::new("flush", "Permitir vaciar el bucket (flushEnabled)", FieldKind::Bool),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

fn one_of<'a>(o: &'a BTreeMap<String, String>, key: &str, what: &str, allowed: &[&str]) -> Result<Option<&'a str>> {
    match opt(o, key) {
        Some(v) if !allowed.contains(&v) => Err(bad(what, v)),
        v => Ok(v),
    }
}

fn number(o: &BTreeMap<String, String>, key: &str, what: &str, range: std::ops::RangeInclusive<u64>) -> Result<Option<u64>> {
    opt(o, key).map(|v| v.parse::<u64>().ok().filter(|n| range.contains(n)).ok_or_else(|| bad(what, v))).transpose()
}

/// A bucket name, checked: letters, digits and `_ - . %`, up to 100.
pub(crate) fn bucket_name(name: &str) -> Result<&str> {
    let n = name.trim();
    if n.is_empty() || n.len() > 100 || !n.chars().all(|c| c.is_ascii_alphanumeric() || "_-.%".contains(c)) {
        return Err(Error::Query(format!("«{name}» no es un nombre de bucket válido: letras, números y _ - . % (hasta 100)")));
    }
    Ok(n)
}

/// The form fields of the create, in order.
pub(crate) fn form(name: &str, o: &BTreeMap<String, String>) -> Result<Vec<(&'static str, String)>> {
    let n = bucket_name(name)?;
    let kind = one_of(o, "bucket_type", "tipo de bucket", &["couchbase", "ephemeral"])?.unwrap_or("couchbase");
    let ram = number(o, "ram_quota", "memoria", 100..=1_048_576)?.unwrap_or(100);
    let flush = match opt(o, "flush") {
        None | Some("false") => "0",
        Some("true") => "1",
        Some(v) => return Err(bad("vaciar el bucket", v)),
    };
    let mut out = vec![("name", n.to_string()), ("ramQuota", ram.to_string()), ("bucketType", kind.to_string()), ("flushEnabled", flush.into())];
    if let Some(r) = one_of(o, "replicas", "réplicas", &["0", "1", "2", "3"])? {
        out.push(("replicaNumber", r.into()));
    }
    let evictions: &[&str] = if kind == "ephemeral" { &["noEviction", "nruEviction"] } else { &["valueOnly", "fullEviction"] };
    if let Some(e) = one_of(o, "eviction", "política de desalojo", evictions)? {
        out.push(("evictionPolicy", e.into()));
    }
    let levels: &[&str] =
        if kind == "ephemeral" { &["none", "majority"] } else { &["none", "majority", "majorityAndPersistActive", "persistToMajority"] };
    if let Some(d) = one_of(o, "durability", "durabilidad mínima", levels)? {
        out.push(("durabilityMinLevel", d.into()));
    }
    if kind == "couchbase" {
        if let Some(s) = one_of(o, "storage_backend", "almacenamiento", &["couchstore", "magma"])? {
            out.push(("storageBackend", s.into()));
        }
    }
    if let Some(t) = number(o, "max_ttl", "vida máxima", 0..=2_147_483_647)? {
        out.push(("maxTTL", t.to_string()));
    }
    Ok(out)
}

pub(crate) fn body(form: &[(&str, String)]) -> String {
    form.iter().map(|(k, v)| format!("{k}={}", encode(v))).collect::<Vec<_>>().join("&")
}

/// What "Ver script" shows: the request and its form.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    Ok(format!("POST {PATH}\n{}", body(&form(name, o)?)))
}

impl CbSession {
    /// The cluster's free bucket memory (`quotaTotal - quotaUsed`), as a
    /// RAM quota to pick.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let Ok(pool) = self.conn.mgmt_get("/pools/default").await else {
            return Ok(Vec::new());
        };
        let ram = |k: &str| pool.pointer(&format!("/storageTotals/ram/{k}")).and_then(Value::as_u64);
        let mut values = vec!["100".to_string()];
        if let (Some(total), Some(used)) = (ram("quotaTotal"), ram("quotaUsed")) {
            let free = total.saturating_sub(used) / (1024 * 1024);
            if free > 100 {
                values.push(free.to_string());
            }
        }
        Ok(vec![
            FieldChoices { key: "ram_quota".into(), default: Some("100".into()), values },
            FieldChoices { key: "replicas".into(), default: Some("1".into()), values: Vec::new() },
        ])
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        if self.conn.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
        }
        let form = form(name, o)?;
        let name = form[0].1.clone();
        let rb = self
            .conn
            .http
            .post(format!("{}{PATH}", self.conn.mgmt))
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body(&form));
        self.conn.mgmt_send(rb).await?;
        // The bucket takes a moment before its scopes answer.
        for _ in 0..40 {
            if self.conn.mgmt_get(&format!("/pools/default/buckets/{}/scopes", encode(&name))).await.is_ok()
                && self.conn.post_query(&json!({"statement": format!("SELECT RAW 1 FROM {}.`_default`.`_default` LIMIT 1", q(&name))})).await.is_ok()
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    }
}

/// The dialog's tab of each field: the bucket itself, then how it stores
/// and keeps data.
pub(crate) fn grouped(f: Field) -> Field {
    let g = match f.key {
        "bucket_type" | "ram_quota" | "replicas" | "flush" => "General",
        _ => "Almacenamiento",
    };
    f.group(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        assert_eq!(
            script("ventas", &o(&[("replicas", " ")])).unwrap(),
            "POST /pools/default/buckets\nname=ventas&ramQuota=100&bucketType=couchbase&flushEnabled=0"
        );
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script(
                "ventas",
                &o(&[
                    ("bucket_type", "couchbase"),
                    ("ram_quota", "256"),
                    ("replicas", "0"),
                    ("eviction", "fullEviction"),
                    ("durability", "majority"),
                    ("storage_backend", "couchstore"),
                    ("max_ttl", "3600"),
                    ("flush", "true"),
                ])
            )
            .unwrap(),
            "POST /pools/default/buckets\nname=ventas&ramQuota=256&bucketType=couchbase&flushEnabled=1&replicaNumber=0&evictionPolicy=fullEviction&durabilityMinLevel=majority&storageBackend=couchstore&maxTTL=3600"
        );
        // An ephemeral bucket has its own policies and no storage backend.
        assert_eq!(
            script("e", &o(&[("bucket_type", "ephemeral"), ("eviction", "nruEviction"), ("storage_backend", "magma")])).unwrap(),
            "POST /pools/default/buckets\nname=e&ramQuota=100&bucketType=ephemeral&flushEnabled=0&evictionPolicy=nruEviction"
        );
        assert!(script("a%b", &o(&[])).unwrap().contains("name=a%25b&"));
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("bucket_type", "memcached"),
            ("ram_quota", "99"),
            ("ram_quota", "1e3"),
            ("replicas", "4"),
            ("eviction", "noEviction"),
            ("durability", "all"),
            ("storage_backend", "rocks"),
            ("max_ttl", "-1"),
            ("flush", "1"),
        ] {
            assert!(script("v", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script("v", &o(&[("bucket_type", "ephemeral"), ("durability", "persistToMajority")])).is_err());
        assert!(script("a&b=c", &o(&[])).is_err());
    }
}

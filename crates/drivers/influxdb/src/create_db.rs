//! "Nueva base de datos" with options
//! ([`dbine_driver::Driver::create_database_fields`]):
//!
//! - InfluxDB 1 (InfluxQL): the default retention policy of
//!   `CREATE DATABASE … WITH DURATION … REPLICATION … SHARD DURATION … NAME …`.
//! - InfluxDB 2 (buckets): the retention, the shard group duration and a
//!   description, in the body of `POST /api/v2/buckets`. The organization's
//!   id is looked up by the session, so the script names it in words.
//! - InfluxDB 3: the retention period of `POST /api/v3/configure/database`.
//!
//! Durations are checked (`30d`, `1h30m`, `INF` where it applies) before
//! they reach a statement or a body.

use crate::Api;
use dbine_driver::{Error, Field, FieldKind, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const V2_PATH: &str = "/api/v2/buckets";
pub(crate) const V3_PATH: &str = "/api/v3/configure/database";
const ORG_PLACEHOLDER: &str = "(el id de la organización de la conexión)";

pub(crate) fn fields(api: Api) -> Vec<Field> {
    match api {
        Api::InfluxQl => vec![
            Field::new("duration", "Retención (DURATION)", FieldKind::Text)
                .placeholder("30d, 52w o INF")
                .help("Vacía: INF, los datos no vencen. Mínimo 1h."),
            Field::new("shard_duration", "Duración de cada shard (SHARD DURATION)", FieldKind::Text)
                .placeholder("1d")
                .help("Vacía: la elige el servidor según la retención."),
            Field::new("replication", "Réplicas (REPLICATION)", FieldKind::Number).help("Vacío: 1. Solo cuenta en InfluxDB Enterprise."),
            Field::new("rp_name", "Nombre de la política de retención (NAME)", FieldKind::Text).help("Vacío: autogen."),
        ],
        Api::Flux => vec![
            Field::new("retention", "Retención", FieldKind::Text)
                .placeholder("30d o 0")
                .help("Vacía o 0: los datos no vencen. Mínimo 1h."),
            Field::new("shard_duration", "Duración de cada grupo de shards", FieldKind::Text)
                .placeholder("1d")
                .help("Vacía: la elige el servidor según la retención."),
            Field::new("description", "Descripción", FieldKind::Text),
        ],
        Api::Sql => vec![Field::new("retention", "Período de retención (retention_period)", FieldKind::Text)
            .placeholder("30d")
            .help("Vacío: los datos no vencen.")],
    }
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

pub(crate) fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es una duración válida (por ejemplo 30d, 12h o 1h30m)"))
}

/// `30d`, `1h30m`… as (amount, unit) pairs, units among `units`.
pub(crate) fn segments<'a>(v: &str, units: &[&'a str]) -> Option<Vec<(u64, &'a str)>> {
    let s = v.to_ascii_lowercase();
    let mut rest = s.as_str();
    let mut out = Vec::new();
    while !rest.is_empty() {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || digits > 12 {
            return None;
        }
        let n = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        // The longest unit first (`ms` before `m`).
        let unit = units.iter().filter(|u| rest.starts_with(**u)).max_by_key(|u| u.len())?;
        rest = &rest[unit.len()..];
        out.push((n, *unit));
    }
    (!out.is_empty()).then_some(out)
}

/// An InfluxQL duration literal (`INF` only when `inf`).
pub(crate) fn influxql_duration(v: &str, inf: bool, what: &str) -> Result<String> {
    if inf && v.eq_ignore_ascii_case("inf") {
        return Ok("INF".into());
    }
    let s = segments(v, &["ns", "u", "ms", "s", "m", "h", "d", "w"]).ok_or_else(|| bad(what, v))?;
    Ok(s.iter().map(|(n, u)| format!("{n}{u}")).collect())
}

/// A duration in seconds (`0` is 0).
pub(crate) fn seconds(v: &str, what: &str) -> Result<u64> {
    if v == "0" {
        return Ok(0);
    }
    let s = segments(v, &["s", "m", "h", "d", "w"]).ok_or_else(|| bad(what, v))?;
    let unit = |u: &str| match u {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => 604_800,
    };
    s.iter().try_fold(0u64, |acc, (n, u)| n.checked_mul(unit(u)).and_then(|x| acc.checked_add(x))).ok_or_else(|| bad(what, v))
}

/// InfluxDB 1: the statement.
pub(crate) fn influxql(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let mut with = Vec::new();
    if let Some(d) = opt(o, "duration") {
        with.push(format!("DURATION {}", influxql_duration(d, true, "retención")?));
    }
    if let Some(r) = opt(o, "replication") {
        let n = r.parse::<u32>().ok().filter(|n| (1..=100).contains(n)).ok_or_else(|| Error::Query(format!("réplicas: «{r}» no es un valor válido")))?;
        with.push(format!("REPLICATION {n}"));
    }
    if let Some(d) = opt(o, "shard_duration") {
        with.push(format!("SHARD DURATION {}", influxql_duration(d, false, "duración de cada shard")?));
    }
    if let Some(n) = opt(o, "rp_name") {
        with.push(format!("NAME {}", crate::v1::ident(n)));
    }
    let with = if with.is_empty() { String::new() } else { format!(" WITH {}", with.join(" ")) };
    Ok(format!("CREATE DATABASE {}{with}", crate::v1::ident(name)))
}

/// InfluxDB 2: the body of the request, for organization `org_id`.
pub(crate) fn bucket(name: &str, org_id: &str, o: &BTreeMap<String, String>) -> Result<Value> {
    let every = opt(o, "retention").map(|v| seconds(v, "retención")).transpose()?;
    let shard = opt(o, "shard_duration").map(|v| seconds(v, "duración de cada grupo de shards")).transpose()?;
    let mut rules = Vec::new();
    if every.is_some_and(|e| e > 0) || shard.is_some() {
        let mut rule = json!({ "type": "expire", "everySeconds": every.unwrap_or(0) });
        if let Some(s) = shard {
            rule["shardGroupDurationSeconds"] = json!(s);
        }
        rules.push(rule);
    }
    let mut body = json!({ "orgID": org_id, "name": name, "retentionRules": rules });
    if let Some(d) = opt(o, "description") {
        body["description"] = json!(d);
    }
    Ok(body)
}

/// InfluxDB 3: the body of the request.
pub(crate) fn database(name: &str, o: &BTreeMap<String, String>) -> Result<Value> {
    let mut body = json!({ "db": name });
    if let Some(r) = opt(o, "retention") {
        let s = segments(r, &["s", "m", "h", "d", "w"]).ok_or_else(|| bad("período de retención", r))?;
        body["retention_period"] = json!(s.iter().map(|(n, u)| format!("{n}{u}")).collect::<String>());
    }
    Ok(body)
}

/// What "Ver script" shows: the statement (v1) or the request (v2, v3).
pub(crate) fn script(api: Api, name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    match api {
        Api::InfluxQl => influxql(name, o),
        Api::Flux => Ok(format!("POST {V2_PATH}\n{}", bucket(name, ORG_PLACEHOLDER, o)?)),
        Api::Sql => Ok(format!("POST {V3_PATH}\n{}", database(name, o)?)),
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
        assert_eq!(script(Api::InfluxQl, "ventas", &o(&[("duration", " ")])).unwrap(), "CREATE DATABASE \"ventas\"");
        assert_eq!(bucket("b", "id1", &o(&[])).unwrap(), json!({ "orgID": "id1", "name": "b", "retentionRules": [] }));
        assert_eq!(script(Api::Sql, "d", &o(&[])).unwrap(), "POST /api/v3/configure/database\n{\"db\":\"d\"}");
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script(Api::InfluxQl, "v", &o(&[("duration", "52W"), ("replication", "1"), ("shard_duration", "1h30m"), ("rp_name", "un \"año\"")])).unwrap(),
            "CREATE DATABASE \"v\" WITH DURATION 52w REPLICATION 1 SHARD DURATION 1h30m NAME \"un \\\"año\\\"\""
        );
        assert_eq!(script(Api::InfluxQl, "v", &o(&[("duration", "inf")])).unwrap(), "CREATE DATABASE \"v\" WITH DURATION INF");
        assert_eq!(
            bucket("b", "id1", &o(&[("retention", "30d"), ("shard_duration", "1d"), ("description", "métricas")])).unwrap(),
            json!({ "orgID": "id1", "name": "b", "description": "métricas",
                    "retentionRules": [{ "type": "expire", "everySeconds": 2_592_000, "shardGroupDurationSeconds": 86_400 }] })
        );
        assert_eq!(bucket("b", "i", &o(&[("retention", "0")])).unwrap()["retentionRules"], json!([]));
        assert!(script(Api::Flux, "b", &o(&[])).unwrap().contains(ORG_PLACEHOLDER));
        assert_eq!(script(Api::Sql, "d", &o(&[("retention", "7D")])).unwrap(), "POST /api/v3/configure/database\n{\"db\":\"d\",\"retention_period\":\"7d\"}");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("duration", "30 days"), ("duration", "1d; DROP"), ("shard_duration", "INF"), ("replication", "0"), ("duration", "h")] {
            assert!(script(Api::InfluxQl, "v", &o(&[bad])).is_err(), "{bad:?}");
        }
        for bad in [("retention", "1ms"), ("retention", "-1d"), ("shard_duration", "99999999999999w")] {
            assert!(script(Api::Flux, "v", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Api::Sql, "v", &o(&[("retention", "INF")])).is_err());
    }
}

//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//!
//! - InfluxDB 1 (InfluxQL): the database's retention policies, each with
//!   its duration, shard duration, replication and whether it's the
//!   default, changed with `ALTER RETENTION POLICY … ON …` (one statement
//!   per policy); facts from `SHOW RETENTION POLICIES`, `SHOW SHARD GROUPS`
//!   and the cardinality estimates.
//! - InfluxDB 2 (buckets): retention, shard group duration and
//!   description, in one `PATCH /api/v2/buckets/{id}`; facts from the
//!   bucket itself.
//! - InfluxDB 3: the retention period, `PUT /api/v3/configure/database`
//!   (3.2 onwards); facts from `system.databases`, the tables and the
//!   Parquet files.
//!
//! For the HTTP versions the "script" is the request that runs: method,
//! path and JSON body.

use crate::create_db::{bad, influxql_duration, segments, seconds};
use crate::http::{self, send_err};
use crate::v1::{ident, InfluxQlSession};
use crate::v2::FluxSession;
use crate::v3::SqlSession;
use crate::Api;
use dbine_driver::serde_static::intern;
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;

const BUCKETS: &str = "/api/v2/buckets";
const V3_PATH: &str = crate::create_db::V3_PATH;
const RP_GROUP: &str = "Políticas de retención";
const SHORTER: &str = "Si la retención nueva es más corta, se borran los datos más viejos que ella en la próxima pasada de retención.";

fn unknown(k: &str) -> Error {
    Error::Query(format!("propiedad desconocida: {k}"))
}

/// The (policy, setting) of a `rp:<policy>:<setting>` key.
fn rp_key(k: &str) -> Option<(&str, &str)> {
    k.strip_prefix("rp:")?.rsplit_once(':')
}

/// The id placeholder of the script (the session looks it up by name).
fn bucket_placeholder(name: &str) -> String {
    format!("{BUCKETS}/(id del bucket «{name}»)")
}

/// InfluxDB 1: one `ALTER RETENTION POLICY` per changed policy.
fn influxql(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut per_rp: BTreeMap<&str, [Option<String>; 4]> = BTreeMap::new();
    for (key, value) in changes {
        let value = value.trim();
        let (rp, what) = rp_key(key).ok_or_else(|| unknown(key))?;
        let slot = per_rp.entry(rp).or_default();
        match what {
            "duration" => slot[0] = Some(format!("DURATION {}", influxql_duration(value, true, "retención")?)),
            "replication" => {
                let n = value
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (1..=100).contains(n))
                    .ok_or_else(|| Error::Query(format!("réplicas: «{value}» no es un valor válido")))?;
                slot[1] = Some(format!("REPLICATION {n}"));
            }
            "shard_duration" => slot[2] = Some(format!("SHARD DURATION {}", influxql_duration(value, false, "duración de cada shard")?)),
            "default" => match value {
                "true" => slot[3] = Some("DEFAULT".into()),
                "" | "false" => {
                    return Err(Error::Query(format!(
                        "«{rp}» deja de ser la predeterminada cuando otra política pasa a serlo: marcá esa otra como predeterminada."
                    )))
                }
                v => return Err(Error::Query(format!("predeterminada: «{v}» no es un valor válido"))),
            },
            _ => return Err(unknown(key)),
        }
    }
    Ok(per_rp
        .into_iter()
        .map(|(rp, parts)| {
            let parts: Vec<String> = parts.into_iter().flatten().collect();
            format!("ALTER RETENTION POLICY {} ON {} {}", ident(rp), ident(database), parts.join(" "))
        })
        .collect())
}

/// InfluxDB 2: the body of the bucket's `PATCH` (`None`: nothing to do).
/// A rule without `everySeconds` (or without the shard duration) keeps the
/// bucket's current one; `everySeconds: 0` means the data never expire.
fn bucket_patch(changes: &BTreeMap<String, String>) -> Result<Option<J>> {
    let mut body = serde_json::Map::new();
    let mut rule = serde_json::Map::new();
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "retention" => {
                let s = if value.is_empty() { 0 } else { seconds(value, "retención")? };
                if s != 0 && s < 3600 {
                    return Err(Error::Query(format!("retención: «{value}» es menos de una hora (el mínimo)")));
                }
                rule.insert("everySeconds".into(), json!(s));
            }
            "shard_duration" => {
                let s = seconds(value, "duración de cada grupo de shards")?;
                if s < 3600 {
                    return Err(Error::Query(format!("duración de cada grupo de shards: «{value}» es menos de una hora (el mínimo)")));
                }
                rule.insert("shardGroupDurationSeconds".into(), json!(s));
            }
            "description" => {
                body.insert("description".into(), json!(value));
            }
            _ => return Err(unknown(key)),
        }
    }
    if !rule.is_empty() {
        rule.insert("type".into(), json!("expire"));
        body.insert("retentionRules".into(), json!([rule]));
    }
    Ok((!body.is_empty()).then_some(J::Object(body)))
}

/// InfluxDB 3: the body of `PUT /api/v3/configure/database` (`None`:
/// nothing to do). An empty retention is `null`: the data never expire.
fn v3_body(database: &str, changes: &BTreeMap<String, String>) -> Result<Option<J>> {
    let mut body = None;
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "retention" => {
                let period = if value.is_empty() {
                    J::Null
                } else {
                    let s = segments(value, &["s", "m", "h", "d", "w"])
                        .filter(|s| s.iter().any(|(n, _)| *n > 0))
                        .ok_or_else(|| bad("período de retención", value))?;
                    json!(s.iter().map(|(n, u)| format!("{n}{u}")).collect::<String>())
                };
                body = Some(json!({ "db": database, "retention_period": period }));
            }
            _ => return Err(unknown(key)),
        }
    }
    Ok(body)
}

/// What runs, in order: InfluxQL statements (v1) or requests (v2, v3).
pub(crate) fn alter(api: Api, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    Ok(match api {
        Api::InfluxQl => influxql(database, changes)?,
        Api::Flux => bucket_patch(changes)?.map(|b| format!("PATCH {}\n{b}", bucket_placeholder(database))).into_iter().collect(),
        Api::Sql => v3_body(database, changes)?.map(|b| format!("PUT {V3_PATH}\n{b}")).into_iter().collect(),
    })
}

pub(crate) fn script(api: Api, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(api, database, changes)?.join(if matches!(api, Api::InfluxQl) { ";\n" } else { "\n\n" }))
}

/// `720h0m0s` as `720h` (zero parts dropped); `0s` is `INF` for a
/// retention.
fn tidy_influxql(v: &str, inf: bool) -> String {
    match segments(v, &["ns", "u", "µs", "ms", "s", "m", "h", "d", "w"]) {
        Some(s) if s.iter().all(|(n, _)| *n == 0) => {
            if inf {
                "INF".into()
            } else {
                "0s".into()
            }
        }
        Some(s) => s.iter().filter(|(n, _)| *n > 0).map(|(n, u)| format!("{n}{u}")).collect(),
        None => v.to_string(),
    }
}

/// Seconds as `30d`, `1d12h`, `90m`… (`0` stays `0`).
fn human_seconds(mut s: u64) -> String {
    if s == 0 {
        return "0".into();
    }
    let mut out = String::new();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        if s >= size {
            out.push_str(&format!("{}{unit}", s / size));
            s %= size;
        }
    }
    out
}

fn fact(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value: value.into() }
}

/// Rows of the first series of each statement's result.
fn series_rows(results: &[J], i: usize) -> Vec<Vec<J>> {
    results
        .get(i)
        .and_then(|r| r.get("series"))
        .and_then(|s| s.as_array())
        .into_iter()
        .flatten()
        .flat_map(|s| s.get("values").and_then(|v| v.as_array()).cloned().unwrap_or_default())
        .filter_map(|r| r.as_array().cloned())
        .collect()
}

fn text(v: Option<&J>) -> String {
    match v {
        Some(J::String(s)) => s.clone(),
        Some(J::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// A failure at step `i` of `n`: after the first, it says how many were
/// applied.
fn partial(i: usize, n: usize, step: &str, e: Error) -> Error {
    if i == 0 {
        e
    } else {
        Error::Query(format!("se aplicaron {i} de {n} cambios; falló: {step}\n{e}"))
    }
}

impl InfluxQlSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let db = ident(database);
        let results = self.query(&format!("SHOW RETENTION POLICIES ON {db}")).await?;
        let rps = series_rows(&results, 0);
        let mut fields = Vec::new();
        let mut values = BTreeMap::new();
        let mut warnings = BTreeMap::new();
        let mut default_rp = String::new();
        for r in &rps {
            let name = text(r.first());
            if name.is_empty() {
                continue;
            }
            let is_default = r.get(4).and_then(J::as_bool).unwrap_or(false);
            if is_default {
                default_rp = name.clone();
            }
            let current = [
                ("duration", "retención (DURATION)", FieldKind::Text, tidy_influxql(&text(r.get(1)), true)),
                ("shard_duration", "duración de cada shard (SHARD DURATION)", FieldKind::Text, tidy_influxql(&text(r.get(2)), false)),
                ("replication", "réplicas (REPLICATION)", FieldKind::Number, text(r.get(3))),
                ("default", "predeterminada (DEFAULT)", FieldKind::Bool, if is_default { "true".into() } else { String::new() }),
            ];
            for (what, label, kind, value) in current {
                let key = intern(&format!("rp:{name}:{what}"));
                let help = match what {
                    "duration" => "Con unidad (30d, 52w, 1h30m) o INF: los datos no vencen. Mínimo 1h.",
                    "shard_duration" => "El tramo de tiempo de cada grupo de shards nuevo; los existentes no cambian.",
                    "replication" => "Solo cuenta en InfluxDB Enterprise (cluster).",
                    _ => "Donde se escribe y se lee cuando no se indica una política. Para quitarla, se marca otra.",
                };
                fields.push(Field::new(key, intern(&format!("{name}: {label}")), kind).help(help).group(RP_GROUP));
                values.insert(key.to_string(), value);
                if what == "duration" {
                    warnings.insert(key.to_string(), SHORTER.to_string());
                }
                if what == "default" {
                    warnings.insert(
                        key.to_string(),
                        "Las escrituras y consultas que no nombran una política pasan a usar esta.".to_string(),
                    );
                }
            }
        }

        let mut info = vec![
            fact("", "Políticas de retención", rps.len().to_string()),
            fact("", "Política predeterminada", default_rp),
        ];
        // Estimates: cheap even on large databases (the exact counts aren't).
        if let Ok(r) = self.query(&format!("SHOW MEASUREMENT CARDINALITY ON {db}; SHOW SERIES CARDINALITY ON {db}")).await {
            for (i, label) in [(0, "Measurements (estimado)"), (1, "Series (estimado)")] {
                let n = series_rows(&r, i).iter().filter_map(|row| row.first().and_then(J::as_u64)).sum::<u64>();
                info.push(fact("", label, n.to_string()));
            }
        }
        if let Ok(r) = self.query("SHOW SHARD GROUPS").await {
            let groups = series_rows(&r, 0).iter().filter(|row| text(row.get(1)) == database).count();
            info.push(fact("", "Grupos de shards", groups.to_string()));
        }
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden modificar las propiedades de una base.".into()));
        }
        let steps = influxql(database, changes)?;
        for (i, step) in steps.iter().enumerate() {
            self.write_statement(step).await.map_err(|e| partial(i, steps.len(), step, e))?;
        }
        Ok(())
    }
}

impl FluxSession {
    async fn bucket_by_name(&self, name: &str) -> Result<J> {
        let v = self.get(BUCKETS, &[("org", self.org.as_str()), ("name", name)]).await?;
        v.pointer("/buckets/0").cloned().ok_or_else(|| Error::Query(format!("No existe el bucket «{name}».")))
    }

    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let b = self.bucket_by_name(database).await?;
        let s = |p: &str| b.pointer(p).and_then(J::as_str).unwrap_or_default().to_string();
        let time = |p: &str| http::iso_time(&s(p)).unwrap_or_else(|| s(p));
        let rule = b.pointer("/retentionRules/0");
        let every = rule.and_then(|r| r.get("everySeconds")).and_then(J::as_u64).unwrap_or(0);
        let shard = rule.and_then(|r| r.get("shardGroupDurationSeconds")).and_then(J::as_u64);
        let kind = match s("/type").as_str() {
            "system" => "del sistema (system)".to_string(),
            "user" => "de usuario (user)".to_string(),
            t => t.to_string(),
        };
        let info = vec![
            fact("", "Id", s("/id")),
            fact("", "Organización", format!("{} ({})", self.org, s("/orgID"))),
            fact("", "Tipo", kind),
            fact("", "Creado", time("/createdAt")),
            fact("", "Modificado", time("/updatedAt")),
        ];
        let fields = vec![
            Field::new("description", "Descripción", FieldKind::Text),
            Field::new("retention", "Retención (everySeconds)", FieldKind::Text)
                .placeholder("30d o 0")
                .help("0 o vacía: los datos no vencen. Con unidad (s, m, h, d, w); mínimo 1h.")
                .group("Retención"),
            Field::new("shard_duration", "Duración de cada grupo de shards (shardGroupDurationSeconds)", FieldKind::Text)
                .placeholder("1d")
                .help("Mínimo 1h. Vale para los grupos de shards nuevos.")
                .group("Retención"),
        ];
        let mut values = BTreeMap::new();
        values.insert("description".into(), s("/description"));
        values.insert("retention".into(), human_seconds(every));
        if let Some(sh) = shard {
            values.insert("shard_duration".into(), human_seconds(sh));
        }
        let warnings = [("retention".to_string(), SHORTER.to_string())].into();
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        self.check_writable("modificar")?;
        let Some(body) = bucket_patch(changes)? else { return Ok(()) };
        let id = self.bucket_by_name(database).await?.get("id").and_then(J::as_str).unwrap_or_default().to_string();
        if id.is_empty() {
            return Err(Error::Query(format!("No se encontró el id del bucket «{database}».")));
        }
        self.send(self.http.patch(format!("{}{BUCKETS}/{id}", self.base)).json(&body)).await?;
        Ok(())
    }
}

/// `3.11.5` → (3, 11).
fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut it = version.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??))
}

impl SqlSession {
    /// A query in `db` (not necessarily the session's), as JSON rows.
    async fn sql_in(&self, db: &str, q: &str) -> Result<Vec<J>> {
        let req = self.http.post(format!("{}/api/v3/query_sql", self.base)).json(&json!({ "db": db, "q": q, "format": "json" }));
        let body = http::text(self.auth(req).send().await.map_err(send_err)?).await?;
        Ok(serde_json::from_str::<J>(&body)?.as_array().cloned().unwrap_or_default())
    }

    async fn version(&self) -> Option<String> {
        let resp = self.auth(self.http.get(format!("{}/ping", self.base))).send().await.ok()?;
        let header = resp.headers().get("X-Influxdb-Version").and_then(|v| v.to_str().ok()).map(str::to_string);
        let v: J = serde_json::from_str(&resp.text().await.ok()?).unwrap_or_default();
        v.get("version").and_then(J::as_str).map(str::to_string).or(header)
    }

    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let lit = format!("'{}'", database.replace('\'', "''"));
        let mut info = Vec::new();
        let version = self.version().await.unwrap_or_default();
        if !version.is_empty() {
            info.push(fact("", "Versión del servidor", version.clone()));
        }
        // The catalog's view of every database lives in `_internal`.
        let retention = self
            .sql_in("_internal", &format!("SELECT retention_period_ns FROM system.databases WHERE database_name = {lit}"))
            .await
            .ok()
            .and_then(|rows| rows.first().cloned());
        let ns = retention.as_ref().and_then(|r| r.get("retention_period_ns")).and_then(J::as_u64);
        let mut values = BTreeMap::new();
        values.insert("retention".to_string(), ns.filter(|n| *n > 0).map(|n| human_seconds(n / 1_000_000_000)).unwrap_or_default());
        if let Ok(rows) = self.sql_in(database, "SELECT count(*) AS n FROM information_schema.tables WHERE table_schema = 'iox'").await {
            info.push(fact("", "Tablas (measurements)", text(rows.first().and_then(|r| r.get("n")))));
        }
        if let Ok(rows) = self
            .sql_in(database, "SELECT count(*) AS files, sum(size_bytes) AS bytes, sum(row_count) AS row_count FROM system.parquet_files")
            .await
        {
            if let Some(r) = rows.first() {
                info.push(fact("Almacenamiento", "Archivos Parquet", text(r.get("files"))));
                let bytes = r.get("bytes").and_then(J::as_u64).unwrap_or(0);
                info.push(fact("Almacenamiento", "Tamaño de los archivos Parquet", format!("{:.1} MB", bytes as f64 / 1_048_576.0)));
                info.push(fact("Almacenamiento", "Filas en archivos Parquet", r.get("row_count").and_then(J::as_u64).unwrap_or(0).to_string()));
            }
        }
        // Changing the retention came with 3.2 (`PUT /api/v3/configure/database`).
        let can_alter = major_minor(&version).is_none_or(|v| v >= (3, 2));
        let mut fields = Vec::new();
        let mut warnings = BTreeMap::new();
        if can_alter {
            fields.push(
                Field::new("retention", "Período de retención (retention_period)", FieldKind::Text)
                    .placeholder("30d")
                    .help("Vacío: los datos no vencen. Con unidad (s, m, h, d, w)."),
            );
            warnings.insert("retention".to_string(), SHORTER.to_string());
        } else {
            values.clear();
            info.push(fact("", "Retención", "Esta versión no permite cambiarla (llegó en InfluxDB 3.2)."));
        }
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let Some(body) = v3_body(database, changes)? else { return Ok(()) };
        let url = format!("{}{V3_PATH}", self.base);
        self.configure(self.http.put(url).json(&body), "modificar").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn one_alter_per_retention_policy() {
        assert_eq!(
            script(
                Api::InfluxQl,
                "métricas",
                &c(&[
                    ("rp:autogen:duration", "30D"),
                    ("rp:autogen:shard_duration", "1d"),
                    ("rp:autogen:replication", "2"),
                    ("rp:un:año:default", "true"),
                    ("rp:un:año:duration", "inf"),
                ])
            )
            .unwrap(),
            "ALTER RETENTION POLICY \"autogen\" ON \"métricas\" DURATION 30d REPLICATION 2 SHARD DURATION 1d;\n\
             ALTER RETENTION POLICY \"un:año\" ON \"métricas\" DURATION INF DEFAULT"
        );
        assert_eq!(script(Api::InfluxQl, "a\"b", &c(&[("rp:x\"y:duration", "1h")])).unwrap(), "ALTER RETENTION POLICY \"x\\\"y\" ON \"a\\\"b\" DURATION 1h");
    }

    #[test]
    fn influxql_values_are_checked() {
        for bad in [
            ("rp:a:duration", "30 days"),
            ("rp:a:duration", "1d; DROP DATABASE x"),
            ("rp:a:shard_duration", "INF"),
            ("rp:a:replication", "0"),
            ("rp:a:default", ""),
            ("rp:a:nope", "1"),
            ("retention", "1d"),
        ] {
            assert!(script(Api::InfluxQl, "v", &c(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn bucket_patch_body() {
        assert_eq!(
            script(Api::Flux, "b", &c(&[("retention", "30d"), ("shard_duration", "1d"), ("description", "métricas")])).unwrap(),
            "PATCH /api/v2/buckets/(id del bucket «b»)\n\
             {\"description\":\"métricas\",\"retentionRules\":[{\"everySeconds\":2592000,\"shardGroupDurationSeconds\":86400,\"type\":\"expire\"}]}"
        );
        assert_eq!(bucket_patch(&c(&[("retention", "")])).unwrap().unwrap(), json!({ "retentionRules": [{ "type": "expire", "everySeconds": 0 }] }));
        assert_eq!(bucket_patch(&c(&[("shard_duration", "2h")])).unwrap().unwrap(), json!({ "retentionRules": [{ "type": "expire", "shardGroupDurationSeconds": 7200 }] }));
        assert_eq!(bucket_patch(&c(&[("description", "")])).unwrap().unwrap(), json!({ "description": "" }));
        assert!(script(Api::Flux, "b", &c(&[])).unwrap().is_empty());
        for bad in [("retention", "30m"), ("retention", "-1d"), ("shard_duration", "0"), ("shard_duration", "1x"), ("name", "otro")] {
            assert!(script(Api::Flux, "b", &c(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn v3_retention_request() {
        assert_eq!(
            script(Api::Sql, "d", &c(&[("retention", "7D")])).unwrap(),
            "PUT /api/v3/configure/database\n{\"db\":\"d\",\"retention_period\":\"7d\"}"
        );
        assert_eq!(script(Api::Sql, "d", &c(&[("retention", "")])).unwrap(), "PUT /api/v3/configure/database\n{\"db\":\"d\",\"retention_period\":null}");
        for bad in [("retention", "INF"), ("retention", "0d"), ("retention", "1y"), ("retention", "7d\"}"), ("other", "1")] {
            assert!(script(Api::Sql, "d", &c(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn durations_as_shown() {
        assert_eq!(tidy_influxql("720h0m0s", true), "720h");
        assert_eq!(tidy_influxql("0s", true), "INF");
        assert_eq!(tidy_influxql("1h30m0s", false), "1h30m");
        assert_eq!(human_seconds(2_592_000), "30d");
        assert_eq!(human_seconds(90_000), "1d1h");
        assert_eq!(human_seconds(0), "0");
        assert_eq!(major_minor("3.11.5"), Some((3, 11)));
        assert_eq!(major_minor("v3.1.0-nightly"), Some((3, 1)));
        assert_eq!(major_minor(""), None);
    }
}

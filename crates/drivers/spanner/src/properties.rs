//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what the admin API's database resource reports, the options its DDL
//! (`getDdl`) sets, and what `ALTER DATABASE … SET OPTIONS` changes.
//!
//! - General: drop protection (`databases.patch`), the default leader and
//!   time zone; state, dialect, creation, encryption as facts.
//! - Versiones y consultas: version retention, optimizer version and
//!   statistics package, default sequence kind.
//!
//! The options go in one `ALTER DATABASE … SET OPTIONS` (an emptied one as
//! `NULL`, back to Spanner's default), then drop protection. PostgreSQL-
//! dialect databases are shown but not changed: this driver speaks
//! GoogleSQL.

use crate::{bq, SpannerSession};
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use serde_json::{json, Value as Json};
use std::collections::BTreeMap;

const QUERIES: &str = "Versiones y consultas";

/// Field key and option name (the same), in the order they're written.
const OPTIONS: &[&str] =
    &["version_retention_period", "default_leader", "default_time_zone", "optimizer_version", "optimizer_statistics_package", "default_sequence_kind"];

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

/// An option's value as GoogleSQL: `NULL` when emptied.
fn option_value(key: &str, v: &str) -> Result<String> {
    if v.is_empty() {
        return Ok("NULL".into());
    }
    let ok = match key {
        "version_retention_period" => {
            let digits = v.trim_end_matches(['s', 'm', 'h', 'd']);
            !digits.is_empty() && digits.len() + 1 == v.len() && digits.len() <= 7 && digits.chars().all(|c| c.is_ascii_digit())
        }
        "default_leader" => v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "default_time_zone" => v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+' | ':')),
        "optimizer_version" => {
            if v.len() <= 4 && v.chars().all(|c| c.is_ascii_digit()) && v != "0" {
                return Ok(v.to_string());
            }
            false
        }
        "optimizer_statistics_package" => v.len() <= 128 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "default_sequence_kind" => v == "bit_reversed_positive",
        _ => return Err(Error::Query(format!("propiedad desconocida: {key}"))),
    };
    if !ok {
        return Err(bad(key, v));
    }
    Ok(format!("'{v}'"))
}

/// One step of a change: DDL, or the drop protection patch.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    Ddl(String),
    DropProtection(bool),
}

pub(crate) fn steps(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<Step>> {
    let mut options = Vec::new();
    let mut protection = None;
    for (key, value) in changes {
        let v = value.trim();
        if key == "drop_protection" {
            protection = Some(match v {
                "true" => true,
                "" | "false" => false,
                _ => return Err(bad("protección contra borrado", v)),
            });
        } else if OPTIONS.contains(&key.as_str()) {
            options.push((OPTIONS.iter().position(|o| o == key).unwrap_or(0), format!("{key} = {}", option_value(key, v)?)));
        } else {
            return Err(Error::Query(format!("propiedad desconocida: {key}")));
        }
    }
    options.sort();
    let mut out = Vec::new();
    if !options.is_empty() {
        let list: Vec<String> = options.into_iter().map(|(_, o)| o).collect();
        out.push(Step::Ddl(format!("ALTER DATABASE {} SET OPTIONS ({})", bq(database), list.join(", "))));
    }
    if let Some(p) = protection {
        out.push(Step::DropProtection(p));
    }
    Ok(out)
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(steps(database, changes)?
        .into_iter()
        .map(|s| match s {
            Step::Ddl(sql) => format!("{sql};"),
            Step::DropProtection(p) => {
                format!("PATCH databases/{database}?updateMask=enableDropProtection (databases.patch)\n{}", json!({ "enableDropProtection": p }))
            }
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// The options `ALTER DATABASE … SET OPTIONS` statements in a DDL set,
/// unquoted (the last one wins; `NULL` removes).
pub(crate) fn ddl_options(statements: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for st in statements {
        let up = st.to_ascii_uppercase();
        if !up.trim_start().starts_with("ALTER DATABASE") {
            continue;
        }
        let Some(open) = up.find("SET OPTIONS").and_then(|i| st[i..].find('(').map(|j| i + j + 1)) else { continue };
        let Some(close) = st.rfind(')').filter(|c| *c >= open) else { continue };
        let (mut parts, mut cur, mut quote) = (Vec::new(), String::new(), None::<char>);
        for c in st[open..close].chars() {
            match c {
                '\'' | '"' if quote.is_none() => quote = Some(c),
                q if quote == Some(q) => quote = None,
                ',' if quote.is_none() => {
                    parts.push(std::mem::take(&mut cur));
                    continue;
                }
                _ => {}
            }
            cur.push(c);
        }
        parts.push(cur);
        for p in parts {
            let Some((k, v)) = p.split_once('=') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if v.eq_ignore_ascii_case("null") {
                out.remove(&k);
            } else {
                out.insert(k, v.trim_matches(|c| c == '\'' || c == '"').to_string());
            }
        }
    }
    out
}

impl SpannerSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let path = format!("{}/databases/{database}", self.instance);
        let db = self.api.get(&path).await?;
        let s = |k: &str| db.get(k).and_then(Json::as_str).map(str::to_string);
        let general = |label: &str, value: String| PropertyInfo { group: String::new(), label: label.into(), value };
        let dialect = s("databaseDialect").unwrap_or_else(|| "GOOGLE_STANDARD_SQL".into());
        let mut info = vec![general("Estado", s("state").unwrap_or_default()), general("Dialecto", dialect.clone())];
        if let Some(t) = s("createTime") {
            info.push(general("Creada", t));
        }
        if let Some(t) = s("earliestVersionTime") {
            info.push(PropertyInfo { group: QUERIES.into(), label: "Versión más antigua legible".into(), value: t });
        }
        let kms = db.pointer("/encryptionConfig/kmsKeyName").and_then(Json::as_str);
        info.push(general("Cifrado", kms.map_or_else(|| "Administrado por Google".to_string(), |k| format!("CMEK: {k}"))));
        if let Some(src) = db.pointer("/restoreInfo/backupInfo/backup").and_then(Json::as_str) {
            info.push(general("Restaurada de", src.to_string()));
        }

        let ddl = self.api.get(&format!("{path}/ddl")).await.ok();
        let statements: Vec<String> =
            ddl.as_ref().and_then(|d| d.get("statements")).and_then(Json::as_array).into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect();
        let set = ddl_options(&statements);

        let mut values = BTreeMap::new();
        for k in OPTIONS {
            values.insert(k.to_string(), set.get(*k).cloned().unwrap_or_default());
        }
        // The resource's own figures win where it has them.
        if let Some(v) = s("versionRetentionPeriod") {
            values.insert("version_retention_period".into(), v);
        }
        if let Some(v) = s("defaultLeader") {
            values.insert("default_leader".into(), v);
        }
        values.insert("drop_protection".into(), if db.get("enableDropProtection").and_then(Json::as_bool) == Some(true) { "true".into() } else { String::new() });

        if dialect == "POSTGRESQL" {
            info.push(general("Cambios", "Esta base usa el dialecto PostgreSQL: este driver solo modifica bases GoogleSQL.".into()));
            return Ok(DatabaseProperties { fields: Vec::new(), values, info, choices: Vec::new(), warnings: BTreeMap::new() });
        }

        let fields = vec![
            Field::new("drop_protection", "Protección contra borrado (enable_drop_protection)", FieldKind::Bool)
                .help("Impide borrar la base (y la instancia) hasta desactivarla."),
            Field::new("default_leader", "Líder por defecto (default_leader)", FieldKind::Text)
                .placeholder("us-east1")
                .help("Solo en instancias multirregión. Vacío: el de la configuración."),
            Field::new("default_time_zone", "Zona horaria por defecto (default_time_zone)", FieldKind::Text)
                .placeholder("America/Argentina/Buenos_Aires")
                .help("Vacía: America/Los_Angeles."),
            Field::new("version_retention_period", "Retención de versiones (version_retention_period)", FieldKind::Text)
                .placeholder("1h, 3d, 7d")
                .help("Cuánto se puede leer o restaurar hacia atrás: de 1h a 7d. Vacía: 1h.")
                .group(QUERIES),
            Field::new("optimizer_version", "Versión del optimizador (optimizer_version)", FieldKind::Number)
                .help("Vacía: la más reciente por defecto.")
                .group(QUERIES),
            Field::new("optimizer_statistics_package", "Paquete de estadísticas (optimizer_statistics_package)", FieldKind::Text)
                .placeholder("auto_20240101_00_00_00UTC")
                .help("Vacío: el más reciente.")
                .group(QUERIES),
            Field::new("default_sequence_kind", "Tipo de secuencia por defecto (default_sequence_kind)", FieldKind::Select(vec![("bit_reversed_positive", "bit_reversed_positive")]))
                .help("El de las columnas IDENTITY y AUTO_INCREMENT que no lo indican.")
                .group(QUERIES),
        ];

        let mut choices = self.create_database_choices_impl().await.unwrap_or_default();
        choices.retain(|c| c.key == "default_leader");
        for c in &mut choices {
            c.default = None;
        }
        choices.push(FieldChoices { key: "version_retention_period".into(), default: None, values: vec!["1h".into(), "1d".into(), "3d".into(), "7d".into()] });

        let warnings = BTreeMap::from([
            (
                "version_retention_period".to_string(),
                "Más retención ocupa más almacenamiento; con menos, ya no se puede leer ni restaurar más atrás que el nuevo plazo.".to_string(),
            ),
            (
                "default_leader".to_string(),
                "Mueve las réplicas líder a otra región: cambia la latencia de las escrituras y el traslado tarda en completarse.".to_string(),
            ),
            ("optimizer_version".to_string(), "Cambia los planes de ejecución de todas las consultas de la base.".to_string()),
            ("optimizer_statistics_package".to_string(), "Cambia los planes de ejecución de todas las consultas de la base.".to_string()),
            ("default_time_zone".to_string(), "Cambia cómo se interpretan las fechas y horas sin zona en las consultas y en los valores por defecto.".to_string()),
        ]);
        Ok(DatabaseProperties { fields, values, info, choices, warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let path = format!("{}/databases/{database}", self.instance);
        let steps = steps(database, changes)?;
        let total = steps.len();
        for (i, step) in steps.into_iter().enumerate() {
            let what = match &step {
                Step::Ddl(sql) => sql.clone(),
                Step::DropProtection(_) => "databases.patch (enableDropProtection)".to_string(),
            };
            let r = match step {
                Step::Ddl(sql) => {
                    let url = format!("{}/v1/{path}/ddl", self.api.base);
                    match self.api.send(self.api.http.patch(url).json(&json!({ "statements": [sql] }))).await {
                        Ok(op) => self.wait(op).await,
                        Err(e) => Err(e),
                    }
                }
                Step::DropProtection(p) => {
                    let url = format!("{}/v1/{path}", self.api.base);
                    let body = json!({ "name": path, "enableDropProtection": p });
                    match self.api.send(self.api.http.patch(url).query(&[("updateMask", "enableDropProtection")]).json(&body)).await {
                        Ok(op) => self.wait(op).await,
                        Err(e) => Err(e),
                    }
                }
            };
            if let Err(e) = r {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {total} cambios; falló: {what}\n{e}")) });
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
    fn one_alter_then_drop_protection() {
        assert_eq!(
            script(
                "ventas",
                &c(&[
                    ("drop_protection", "true"),
                    ("optimizer_version", "6"),
                    ("version_retention_period", "3d"),
                    ("default_leader", ""),
                    ("default_time_zone", "Europe/Madrid"),
                ])
            )
            .unwrap(),
            "ALTER DATABASE `ventas` SET OPTIONS (version_retention_period = '3d', default_leader = NULL, default_time_zone = 'Europe/Madrid', optimizer_version = 6);\n\
             PATCH databases/ventas?updateMask=enableDropProtection (databases.patch)\n{\"enableDropProtection\":true}"
        );
        assert_eq!(steps("v", &c(&[("drop_protection", "")])).unwrap(), vec![Step::DropProtection(false)]);
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("version_retention_period", "3"),
            ("version_retention_period", "3d'"),
            ("default_leader", "US-EAST1"),
            ("default_time_zone", "Europe/Madrid'"),
            ("optimizer_version", "x"),
            ("optimizer_version", "0"),
            ("optimizer_statistics_package", "a b"),
            ("default_sequence_kind", "serial"),
            ("drop_protection", "yes"),
            ("nope", "1"),
        ] {
            assert!(script("v", &c(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn options_from_the_ddl() {
        let ddl = [
            "CREATE TABLE t (id INT64) PRIMARY KEY (id)".to_string(),
            "ALTER DATABASE v SET OPTIONS (version_retention_period = '3d')".to_string(),
            "ALTER DATABASE v SET OPTIONS (default_time_zone = 'Europe/Madrid', optimizer_version = 6)".to_string(),
            "ALTER DATABASE v SET OPTIONS (optimizer_version = NULL)".to_string(),
        ];
        let o = ddl_options(&ddl);
        assert_eq!(o.get("version_retention_period").map(String::as_str), Some("3d"));
        assert_eq!(o.get("default_time_zone").map(String::as_str), Some("Europe/Madrid"));
        assert!(!o.contains_key("optimizer_version"));
    }
}

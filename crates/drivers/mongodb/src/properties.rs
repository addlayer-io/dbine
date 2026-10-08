//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what `dbStats` reports and what the `profile` command changes.
//!
//! - MongoDB: the counts and sizes of `dbStats`, and the database profiler:
//!   its level is per database; `slowms` and `sampleRate` are the server's
//!   (mongod keeps one value for every database), which the labels say.
//! - FerretDB: `dbStats` only (it has no `profile` command).
//! - Amazon DocumentDB: `dbStats` only (its profiler is set in the cluster's
//!   parameter group and writes to CloudWatch Logs).
//!
//! The changes are one `profile` command, run on the database: the script
//! shows it in mongosh form.

use crate::ddl::check_database_name;
use crate::monitor::num;
use crate::{err, Flavor, MongoSession};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use mongodb::bson::{doc, Bson, Document};
use std::collections::BTreeMap;

const PROFILING: &str = "Profiling";
const STORAGE: &str = "Almacenamiento";

fn unsupported() -> Error {
    Error::Unsupported("este motor no modifica las propiedades de una base".into())
}

/// The `profile` command for `changes` (`profile` level, `slowms`,
/// `sample_rate`), or none when nothing changes. The level stays as it is
/// (`-1`) when only the server-wide values change.
pub(crate) fn commands(flavor: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<Document>> {
    if flavor != Flavor::Mongo {
        return Err(unsupported());
    }
    check_database_name(database)?;
    let (mut level, mut slowms, mut rate) = (-1, None, None);
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "profile" => {
                level = match value {
                    "0" => 0,
                    "1" => 1,
                    "2" => 2,
                    _ => return Err(Error::Query(format!("nivel de profiling: «{value}» no es 0, 1 ni 2"))),
                };
            }
            "slowms" => {
                slowms = Some(value
                    .parse::<i32>()
                    .ok()
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| Error::Query(format!("umbral de operación lenta: «{value}» no es una cantidad de milisegundos")))?);
            }
            "sample_rate" => {
                rate = Some(value
                    .parse::<f64>()
                    .ok()
                    .filter(|r| (0.0..=1.0).contains(r))
                    .ok_or_else(|| Error::Query(format!("muestra de operaciones: «{value}» no es un número entre 0 y 1")))?);
            }
            k => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
        }
    }
    if level == -1 && slowms.is_none() && rate.is_none() {
        return Ok(Vec::new());
    }
    let mut cmd = doc! { "profile": level };
    if let Some(ms) = slowms {
        cmd.insert("slowms", ms);
    }
    if let Some(r) = rate {
        cmd.insert("sampleRate", r);
    }
    Ok(vec![cmd])
}

/// One command in mongosh form: `db.getSiblingDB("x").runCommand({…})`.
fn shell(database: &str, cmd: &Document) -> String {
    let fields: Vec<String> = cmd
        .iter()
        .map(|(k, v)| {
            let v = match v {
                Bson::Int32(n) => n.to_string(),
                Bson::Double(f) => f.to_string(),
                other => other.to_string(),
            };
            format!("{k}: {v}")
        })
        .collect();
    format!(
        "db.getSiblingDB({}).runCommand({{{}}})",
        serde_json::Value::String(database.to_string()),
        fields.join(", ")
    )
}

pub(crate) fn alter(flavor: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    Ok(commands(flavor, database, changes)?.iter().map(|c| shell(database, c)).collect())
}

pub(crate) fn script(flavor: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(flavor, database, changes)?.join("\n"))
}

/// Bytes as the largest unit that keeps a whole part.
fn bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (n, 0);
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", n as i64)
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn fact(group: &str, label: &str, value: String) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value }
}

impl MongoSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        check_database_name(database)?;
        let db = self.client.database(database);
        let stats = db.run_command(doc! { "dbStats": 1, "scale": 1 }).await.map_err(err)?;
        let count = |k: &str| num(&stats, &[k]).map(|n| (n as i64).to_string());
        let size = |k: &str| num(&stats, &[k]).map(bytes);
        let mut info = Vec::new();
        for (k, label) in [("collections", "Colecciones"), ("views", "Vistas"), ("objects", "Documentos (objects)"), ("indexes", "Índices")] {
            if let Some(v) = count(k) {
                info.push(fact("", label, v));
            }
        }
        for (k, label) in [
            ("dataSize", "Datos sin comprimir (dataSize)"),
            ("avgObjSize", "Tamaño medio de un documento (avgObjSize)"),
            ("storageSize", "Espacio de las colecciones (storageSize)"),
            ("indexSize", "Espacio de los índices (indexSize)"),
            ("totalSize", "Espacio total (totalSize)"),
        ] {
            if let Some(v) = size(k) {
                info.push(fact(STORAGE, label, v));
            }
        }
        if let (Some(used), Some(total)) = (num(&stats, &["fsUsedSize"]), num(&stats, &["fsTotalSize"])) {
            info.push(fact(STORAGE, "Disco del servidor (usado / total)", format!("{} / {}", bytes(used), bytes(total))));
        }

        let mut fields = Vec::new();
        let mut values = BTreeMap::new();
        let mut warnings = BTreeMap::new();
        match self.flavor {
            Flavor::Ferret => info.push(fact(PROFILING, "Profiling", "FerretDB no tiene el comando profile.".into())),
            Flavor::DocumentDb => info.push(fact(
                PROFILING,
                "Profiling",
                "Amazon DocumentDB configura su profiler en el grupo de parámetros del clúster (profiler, profiler_threshold_ms), no por base."
                    .into(),
            )),
            Flavor::Mongo => {
                // Users without the privilege see the facts only.
                match db.run_command(doc! { "profile": -1 }).await {
                    Ok(status) => {
                        if let Some(was) = num(&status, &["was"]) {
                            values.insert("profile".into(), (was as i64).to_string());
                        }
                        if let Some(ms) = num(&status, &["slowms"]) {
                            values.insert("slowms".into(), (ms as i64).to_string());
                        }
                        if let Some(rate) = num(&status, &["sampleRate"]) {
                            values.insert("sample_rate".into(), rate.to_string());
                        }
                        if let Ok(filter) = status.get_document("filter") {
                            info.push(fact(PROFILING, "Filtro del profiler (filter)", Bson::Document(filter.clone()).into_relaxed_extjson().to_string()));
                        }
                        fields.extend([
                            Field::new(
                                "profile",
                                "Nivel de profiling (profile)",
                                FieldKind::Select(vec![
                                    ("0", "Apagado (0)"),
                                    ("1", "Solo las operaciones lentas (1)"),
                                    ("2", "Todas las operaciones (2)"),
                                ]),
                            )
                            .help("Es de esta base: lo registrado queda en su colección system.profile.")
                            .group(PROFILING),
                            Field::new("slowms", "Umbral de operación lenta, en ms (slowms)", FieldKind::Number)
                                .help("Es del servidor (mongod), no de esta base: vale para todas las bases y para el log de operaciones lentas.")
                                .group(PROFILING),
                            Field::new("sample_rate", "Fracción de operaciones lentas registradas (sampleRate)", FieldKind::Text)
                                .placeholder("1.0")
                                .help("Entre 0 y 1. Es del servidor (mongod), no de esta base: vale para todas las bases.")
                                .group(PROFILING),
                        ]);
                        warnings.insert(
                            "profile".into(),
                            "En nivel 2 se registra cada operación de la base en system.profile: suma carga y escrituras al servidor.".into(),
                        );
                        for k in ["slowms", "sample_rate"] {
                            warnings.insert(
                                k.into(),
                                "Cambia el valor de todo el servidor: afecta el profiling de todas las bases y el log de operaciones lentas.".into(),
                            );
                        }
                    }
                    Err(e) => info.push(fact(PROFILING, "Profiling", format!("No se pudo leer ({}).", err(e)))),
                }
            }
        }
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        self.refuse_if_read_only("modificar las propiedades de una base")?;
        let cmds = commands(self.flavor, database, changes)?;
        let db = self.client.database(database);
        for (i, cmd) in cmds.iter().enumerate() {
            if let Err(e) = db.run_command(cmd.clone()).await {
                let e = err(e);
                return Err(if i == 0 {
                    e
                } else {
                    Error::Query(format!("se aplicaron {i} de {} cambios; falló: {}\n{e}", cmds.len(), shell(database, cmd)))
                });
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
    fn one_profile_command() {
        assert_eq!(
            script(Flavor::Mongo, "ventas", &c(&[("profile", "1"), ("slowms", "250"), ("sample_rate", "0.5")])).unwrap(),
            r#"db.getSiblingDB("ventas").runCommand({profile: 1, slowms: 250, sampleRate: 0.5})"#
        );
        assert_eq!(
            script(Flavor::Mongo, "v", &c(&[("profile", "2")])).unwrap(),
            r#"db.getSiblingDB("v").runCommand({profile: 2})"#
        );
    }

    #[test]
    fn server_values_keep_the_level() {
        assert_eq!(
            script(Flavor::Mongo, "v", &c(&[("slowms", "100")])).unwrap(),
            r#"db.getSiblingDB("v").runCommand({profile: -1, slowms: 100})"#
        );
        assert_eq!(script(Flavor::Mongo, "v", &c(&[])).unwrap(), "");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("profile", "3"), ("profile", "1;"), ("slowms", "-1"), ("slowms", "abc"), ("sample_rate", "1.5"), ("sample_rate", "x"), ("nope", "1")] {
            assert!(script(Flavor::Mongo, "v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Flavor::Mongo, "a\"b", &c(&[("profile", "1")])).is_err());
        assert!(script(Flavor::Ferret, "v", &c(&[("profile", "1")])).is_err());
        assert!(script(Flavor::DocumentDb, "v", &c(&[("profile", "1")])).is_err());
    }
}

//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what `SHOW DATABASES`, `SHOW PARAMETERS IN DATABASE` and the database's
//! `INFORMATION_SCHEMA` report, and what `ALTER DATABASE … SET / UNSET`
//! changes.
//!
//! - General: owner (`GRANT OWNERSHIP … COPY CURRENT GRANTS`) and comment.
//! - Time Travel: retention and maximum extension.
//! - Opciones: default DDL collation, quoted identifiers, invalid
//!   characters, serialization policy, catalog and external volume.
//! - Tareas, Registro: the task and logging parameters.
//!
//! Only the parameters `SHOW PARAMETERS IN DATABASE` lists (what the
//! account's edition has) are offered. Emptying a parameter `UNSET`s it:
//! it goes back to the account's value. One statement per change, the
//! ownership transfer last.

use crate::ddl::lit;
use crate::SnowflakeSession;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
enum Kind {
    /// Digits between 0 and the maximum.
    Number(u64),
    Bool,
    Select(&'static [&'static str]),
    /// `en-ci`, `es-ai-pi`, `utf8`…
    Collation,
    /// An object name (catalog, external volume).
    Name,
}

const SIZES: &[&str] = &["XSMALL", "SMALL", "MEDIUM", "LARGE", "XLARGE", "XXLARGE", "XXXLARGE", "X4LARGE", "X5LARGE", "X6LARGE"];

/// Parameter, label, kind, tab.
const PARAMS: &[(&str, &str, Kind, &str)] = &[
    ("DATA_RETENTION_TIME_IN_DAYS", "Días de Time Travel (DATA_RETENTION_TIME_IN_DAYS)", Kind::Number(90), "Time Travel"),
    ("MAX_DATA_EXTENSION_TIME_IN_DAYS", "Extensión máxima de retención (MAX_DATA_EXTENSION_TIME_IN_DAYS)", Kind::Number(90), "Time Travel"),
    ("DEFAULT_DDL_COLLATION", "Intercalación por defecto (DEFAULT_DDL_COLLATION)", Kind::Collation, "Opciones"),
    ("QUOTED_IDENTIFIERS_IGNORE_CASE", "Ignorar mayúsculas en nombres entre comillas (QUOTED_IDENTIFIERS_IGNORE_CASE)", Kind::Bool, "Opciones"),
    ("REPLACE_INVALID_CHARACTERS", "Reemplazar caracteres UTF-8 inválidos (REPLACE_INVALID_CHARACTERS)", Kind::Bool, "Opciones"),
    ("STORAGE_SERIALIZATION_POLICY", "Serialización de tablas Iceberg (STORAGE_SERIALIZATION_POLICY)", Kind::Select(&["COMPATIBLE", "OPTIMIZED"]), "Opciones"),
    ("CATALOG", "Catálogo de tablas Iceberg (CATALOG)", Kind::Name, "Opciones"),
    ("EXTERNAL_VOLUME", "Volumen externo (EXTERNAL_VOLUME)", Kind::Name, "Opciones"),
    ("SUSPEND_TASK_AFTER_NUM_FAILURES", "Suspender una tarea tras N fallas (SUSPEND_TASK_AFTER_NUM_FAILURES)", Kind::Number(1_000_000), "Tareas"),
    ("TASK_AUTO_RETRY_ATTEMPTS", "Reintentos automáticos (TASK_AUTO_RETRY_ATTEMPTS)", Kind::Number(30), "Tareas"),
    ("USER_TASK_TIMEOUT_MS", "Tiempo máximo de una tarea en ms (USER_TASK_TIMEOUT_MS)", Kind::Number(604_800_000), "Tareas"),
    ("USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE", "Tamaño inicial del warehouse administrado (USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE)", Kind::Select(SIZES), "Tareas"),
    ("USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS", "Intervalo mínimo entre disparos en s (USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS)", Kind::Number(604_800), "Tareas"),
    ("LOG_LEVEL", "Nivel de log (LOG_LEVEL)", Kind::Select(&["TRACE", "DEBUG", "INFO", "WARN", "ERROR", "FATAL", "OFF"]), "Registro"),
    ("TRACE_LEVEL", "Nivel de trazas (TRACE_LEVEL)", Kind::Select(&["ALWAYS", "ON_EVENT", "PROPAGATE", "OFF"]), "Registro"),
    ("METRIC_LEVEL", "Métricas (METRIC_LEVEL)", Kind::Select(&["ALL", "NONE"]), "Registro"),
    ("ENABLE_CONSOLE_OUTPUT", "Salida de consola en el log (ENABLE_CONSOLE_OUTPUT)", Kind::Bool, "Registro"),
];

fn key_of(param: &str) -> String {
    param.to_ascii_lowercase()
}

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

/// `SET <param> = <value>` for one parameter, or `UNSET <param>` when
/// emptied (booleans: "" is FALSE).
fn param(p: &str, kind: Kind, value: &str) -> Result<String> {
    let v = value.trim();
    if v.is_empty() && !matches!(kind, Kind::Bool) {
        return Ok(format!("UNSET {p}"));
    }
    let sql = match kind {
        Kind::Number(max) => {
            let ok = v.len() <= 10 && v.chars().all(|c| c.is_ascii_digit()) && v.parse::<u64>().is_ok_and(|n| n <= max);
            if !ok {
                return Err(Error::Query(format!("{p}: «{v}» no es un número entre 0 y {max}")));
            }
            v.to_string()
        }
        Kind::Bool => match v {
            "true" => "TRUE".into(),
            "" | "false" => "FALSE".into(),
            _ => return Err(bad(p, v)),
        },
        Kind::Select(options) => {
            let up = v.to_ascii_uppercase();
            if !options.contains(&up.as_str()) {
                return Err(bad(p, v));
            }
            if p == "USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE" {
                lit(&up)
            } else {
                up
            }
        }
        Kind::Collation => {
            if !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err(Error::Query(format!("intercalación: «{v}» no es una especificación de collation")));
            }
            lit(v)
        }
        Kind::Name => {
            if !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') || v.starts_with(|c: char| c.is_ascii_digit()) {
                return Err(Error::Query(format!("{p}: «{v}» no es un nombre de objeto")));
            }
            lit(v)
        }
    };
    Ok(format!("SET {p} = {sql}"))
}

/// The statements for `changes`; the ownership transfer goes last (after
/// it, the current role may no longer alter the database).
pub(crate) fn alter(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let db = qualified_name(Quote::Double, None, database);
    let (mut out, mut last) = (Vec::new(), Vec::new());
    for (key, value) in changes {
        match key.as_str() {
            "comment" => out.push(match value.trim() {
                "" => format!("ALTER DATABASE {db} UNSET COMMENT"),
                c => format!("ALTER DATABASE {db} SET COMMENT = {}", lit(c)),
            }),
            "owner" => {
                let role = value.trim();
                if role.is_empty() {
                    return Err(Error::Query("dueño: falta el rol".into()));
                }
                last.push(format!("GRANT OWNERSHIP ON DATABASE {db} TO ROLE {} COPY CURRENT GRANTS", qualified_name(Quote::Double, None, role)));
            }
            k => match PARAMS.iter().find(|p| key_of(p.0) == k) {
                Some((p, _, kind, _)) => out.push(format!("ALTER DATABASE {db} {}", param(p, *kind, value)?)),
                None => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
            },
        }
    }
    Ok(out.into_iter().chain(last).collect())
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(database, changes)?.into_iter().map(|s| s + ";").collect::<Vec<_>>().join("\n"))
}

fn field(param: &str, label: &'static str, kind: Kind, group: &'static str) -> Field {
    let key: &'static str = dbine_driver::serde_static::intern(&key_of(param));
    let k = match kind {
        Kind::Number(_) => FieldKind::Number,
        Kind::Bool => FieldKind::Bool,
        Kind::Select(options) => FieldKind::Select(options.iter().map(|o| (*o, *o)).collect()),
        Kind::Collation | Kind::Name => FieldKind::Text,
    };
    let f = Field::new(key, label, k).group(group);
    match kind {
        Kind::Bool => f,
        _ => f.help("Vacío: el de la cuenta."),
    }
}

impl SnowflakeSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let db = qualified_name(Quote::Double, None, database);
        let shown = self.named_rows(&format!("SHOW DATABASES LIKE {}", lit(database))).await?;
        let r = shown
            .into_iter()
            .find(|r| r.get("name").map(String::as_str) == Some(database))
            .ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let get = |k: &str| r.get(k).cloned().unwrap_or_default();
        let general = |label: &str, value: String| PropertyInfo { group: String::new(), label: label.into(), value };
        let transient = get("options").to_ascii_uppercase().contains("TRANSIENT");
        let mut info = vec![
            general("Creada", get("created_on")),
            general("Tipo", if get("kind").is_empty() { "STANDARD".into() } else { get("kind") }),
            general("Transitoria (TRANSIENT)", if transient { "Sí".into() } else { "No".into() }),
        ];
        if !get("origin").is_empty() {
            info.push(general("Origen (compartida)", get("origin")));
        }
        if !get("owner_role_type").is_empty() {
            info.push(general("Tipo de dueño", get("owner_role_type")));
        }
        if let Ok(rows) = self
            .text_rows(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(BYTES), 0), COALESCE(SUM(ROW_COUNT), 0), COUNT(DISTINCT TABLE_SCHEMA)
                     FROM {db}.INFORMATION_SCHEMA.TABLES WHERE TABLE_TYPE = 'BASE TABLE'"
                ),
                &[],
            )
            .await
        {
            if let Some(r) = rows.first() {
                let c = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
                let mb = c(1).parse::<f64>().map(|b| format!("{:.1} MB", b / 1_048_576.0)).unwrap_or_default();
                info.push(general("Tablas", c(0)));
                info.push(general("Tamaño de las tablas", mb));
                info.push(general("Filas", c(2)));
            }
        }

        let mut values = BTreeMap::from([("owner".to_string(), get("owner")), ("comment".to_string(), get("comment"))]);
        let mut fields = vec![
            Field::new("owner", "Dueño (rol)", FieldKind::Text).help("Se transfiere con GRANT OWNERSHIP … COPY CURRENT GRANTS."),
            Field::new("comment", "Comentario", FieldKind::Textarea),
        ];
        let params = self.named_rows(&format!("SHOW PARAMETERS IN DATABASE {db}")).await.unwrap_or_default();
        for (p, label, kind, group) in PARAMS {
            let Some(row) = params.iter().find(|r| r.get("key").is_some_and(|k| k.eq_ignore_ascii_case(p))) else { continue };
            let v = row.get("value").cloned().unwrap_or_default();
            let v = match kind {
                Kind::Bool => if v.eq_ignore_ascii_case("true") { "true".into() } else { String::new() },
                _ => v,
            };
            values.insert(key_of(p), v);
            let mut f = field(p, label, *kind, group);
            if *p == "DATA_RETENTION_TIME_IN_DAYS" && transient {
                f = f.help("Base transitoria: 0 o 1. Vacío: el de la cuenta.");
            }
            fields.push(f);
        }

        let roles: Vec<String> = self.named_rows("SHOW ROLES").await.unwrap_or_default().into_iter().filter_map(|mut r| r.remove("name")).collect();
        let choices = vec![FieldChoices { key: "owner".into(), default: None, values: roles }];

        let warnings = BTreeMap::from([
            (
                "owner".to_string(),
                "Pasa la base a otro rol (con sus permisos actuales copiados). Si tu rol no hereda el nuevo, puede perder el permiso de modificarla o borrarla.".to_string(),
            ),
            (
                "data_retention_time_in_days".to_string(),
                "Bajar la retención saca del Time Travel los datos más viejos que el nuevo plazo: ya no se pueden consultar con AT/BEFORE ni recuperar con UNDROP.".to_string(),
            ),
            (
                "quoted_identifiers_ignore_case".to_string(),
                "Cambia cómo se resuelven los nombres entre comillas en esta base: consultas y vistas existentes pueden dejar de encontrar sus objetos.".to_string(),
            ),
        ]);
        Ok(DatabaseProperties { fields, values, info, choices, warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(database, changes)?;
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.statement(sql, None, 1).await {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
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
    fn only_the_changes_owner_last() {
        assert_eq!(
            script(
                "Ventas",
                &c(&[
                    ("owner", "ANALISTA"),
                    ("data_retention_time_in_days", "7"),
                    ("comment", "it's"),
                    ("default_ddl_collation", ""),
                    ("quoted_identifiers_ignore_case", ""),
                    ("log_level", "warn"),
                    ("user_task_managed_initial_warehouse_size", "SMALL"),
                ])
            )
            .unwrap(),
            "ALTER DATABASE \"Ventas\" SET COMMENT = 'it''s';\n\
             ALTER DATABASE \"Ventas\" SET DATA_RETENTION_TIME_IN_DAYS = 7;\n\
             ALTER DATABASE \"Ventas\" UNSET DEFAULT_DDL_COLLATION;\n\
             ALTER DATABASE \"Ventas\" SET LOG_LEVEL = WARN;\n\
             ALTER DATABASE \"Ventas\" SET QUOTED_IDENTIFIERS_IGNORE_CASE = FALSE;\n\
             ALTER DATABASE \"Ventas\" SET USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE = 'SMALL';\n\
             GRANT OWNERSHIP ON DATABASE \"Ventas\" TO ROLE \"ANALISTA\" COPY CURRENT GRANTS;"
        );
        assert_eq!(script("V", &c(&[("comment", " ")])).unwrap(), "ALTER DATABASE \"V\" UNSET COMMENT;");
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("data_retention_time_in_days", "91"),
            ("data_retention_time_in_days", "1; DROP"),
            ("max_data_extension_time_in_days", "-1"),
            ("default_ddl_collation", "en' --"),
            ("log_level", "LOUD"),
            ("quoted_identifiers_ignore_case", "yes"),
            ("external_volume", "v'x"),
            ("owner", " "),
            ("nope", "1"),
        ] {
            assert!(script("V", &c(&[bad])).is_err(), "{bad:?}");
        }
        // A role name can't break out of its quotes.
        assert!(script("V", &c(&[("owner", "a\" TO ROLE x")])).unwrap().contains("ROLE \"a\"\" TO ROLE x\""));
    }
}

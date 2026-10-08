//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what `sys.databases` reports and `ALTER DATABASE` changes, in tabs.
//!
//! - SQL Server: owner and collation; recovery, compatibility, page verify,
//!   target recovery time, delayed durability; the AUTO_* options; state,
//!   user access and read-only; snapshot isolation; ANSI options,
//!   TRUSTWORTHY and DB_CHAINING; and each file's size, growth and maximum.
//! - Azure SQL Database: its service (edition, objective, maximum size), and
//!   the options it lets change.
//!
//! Each changed setting is one statement; the ones that need the database
//! to themselves go `WITH ROLLBACK IMMEDIATE` (they end other sessions),
//! which the warnings say before anything runs.

use crate::create_db::{check, literal, size, word};
use crate::variant::Variant;
use crate::{err, text, SqlServerSession};
use dbine_driver::serde_static::intern;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use std::collections::BTreeMap;

/// The on/off options: field key, label, `SET` keyword, `sys.databases`
/// column, tab.
const SWITCHES: &[(&str, &str, &str, &str, &str)] = &[
    ("auto_close", "Cerrar automáticamente (AUTO_CLOSE)", "AUTO_CLOSE", "is_auto_close_on", "Automático"),
    ("auto_shrink", "Reducir automáticamente (AUTO_SHRINK)", "AUTO_SHRINK", "is_auto_shrink_on", "Automático"),
    ("auto_create_stats", "Crear estadísticas (AUTO_CREATE_STATISTICS)", "AUTO_CREATE_STATISTICS", "is_auto_create_stats_on", "Automático"),
    ("auto_update_stats", "Actualizar estadísticas (AUTO_UPDATE_STATISTICS)", "AUTO_UPDATE_STATISTICS", "is_auto_update_stats_on", "Automático"),
    (
        "auto_update_stats_async",
        "Actualizarlas en segundo plano (AUTO_UPDATE_STATISTICS_ASYNC)",
        "AUTO_UPDATE_STATISTICS_ASYNC",
        "is_auto_update_stats_async_on",
        "Automático",
    ),
    ("allow_snapshot_isolation", "Permitir aislamiento SNAPSHOT", "ALLOW_SNAPSHOT_ISOLATION", "snapshot_isolation_state", "Aislamiento"),
    ("read_committed_snapshot", "READ COMMITTED con versiones (READ_COMMITTED_SNAPSHOT)", "READ_COMMITTED_SNAPSHOT", "is_read_committed_snapshot_on", "Aislamiento"),
    ("ansi_nulls", "ANSI_NULLS", "ANSI_NULLS", "is_ansi_nulls_on", "Opciones ANSI y de seguridad"),
    ("ansi_padding", "ANSI_PADDING", "ANSI_PADDING", "is_ansi_padding_on", "Opciones ANSI y de seguridad"),
    ("ansi_warnings", "ANSI_WARNINGS", "ANSI_WARNINGS", "is_ansi_warnings_on", "Opciones ANSI y de seguridad"),
    ("arithabort", "ARITHABORT", "ARITHABORT", "is_arithabort_on", "Opciones ANSI y de seguridad"),
    ("quoted_identifier", "QUOTED_IDENTIFIER", "QUOTED_IDENTIFIER", "is_quoted_identifier_on", "Opciones ANSI y de seguridad"),
    ("concat_null_yields_null", "CONCAT_NULL_YIELDS_NULL", "CONCAT_NULL_YIELDS_NULL", "is_concat_null_yields_null_on", "Opciones ANSI y de seguridad"),
    ("recursive_triggers", "Triggers recursivos (RECURSIVE_TRIGGERS)", "RECURSIVE_TRIGGERS", "is_recursive_triggers_on", "Opciones ANSI y de seguridad"),
    ("trustworthy", "Confiable (TRUSTWORTHY)", "TRUSTWORTHY", "is_trustworthy_on", "Opciones ANSI y de seguridad"),
    ("db_chaining", "Encadenamiento entre bases (DB_CHAINING)", "DB_CHAINING", "is_db_chaining_on", "Opciones ANSI y de seguridad"),
];

/// The switches Azure SQL Database lets change.
const AZURE_SWITCHES: &[&str] = &[
    "auto_create_stats",
    "auto_update_stats",
    "auto_update_stats_async",
    "allow_snapshot_isolation",
    "read_committed_snapshot",
    "ansi_nulls",
    "ansi_padding",
    "ansi_warnings",
    "arithabort",
    "quoted_identifier",
    "concat_null_yields_null",
    "recursive_triggers",
];

/// Settings that end other sessions (they need the database to themselves).
const EXCLUSIVE: &[&str] = &["read_only", "user_access", "state", "read_committed_snapshot", "collation"];

fn yes(v: Option<&str>) -> bool {
    matches!(v.map(str::trim), Some("true" | "1" | "ON" | "on"))
}

fn on_off(v: &str) -> &'static str {
    if yes(Some(v)) {
        "ON"
    } else {
        "OFF"
    }
}

fn mb(pages: Option<String>) -> Option<String> {
    pages.and_then(|p| p.trim().parse::<i64>().ok()).map(|p| (p * 8 / 1024).to_string())
}

/// `-1` unlimited, `0` none, pages otherwise; `%` when `is_percent`.
fn growth(v: Option<String>, percent: bool) -> Option<String> {
    let n: i64 = v?.trim().parse().ok()?;
    Some(if percent { format!("{n}%") } else { format!("{}MB", n * 8 / 1024) })
}

fn max_size(v: Option<String>) -> Option<String> {
    let n: i64 = v?.trim().parse().ok()?;
    Some(if n == -1 || n == 268_435_456 { "UNLIMITED".into() } else { format!("{}MB", n * 8 / 1024) })
}

/// A file's key prefix: `file:<logical name>:`.
fn file_key(logical: &str, what: &str) -> String {
    format!("file:{logical}:{what}")
}

/// The statements for `changes`, in a safe order: back ONLINE first, the
/// settings, then what restricts access (READ_ONLY, user access, OFFLINE).
pub(crate) fn alter(v: Variant, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if !matches!(v, Variant::SqlServer | Variant::AzureSql) {
        return Err(Error::Unsupported("este motor no modifica las propiedades de una base".into()));
    }
    let db = qualified_name(Quote::Bracket, None, database);
    let set = |what: String| format!("ALTER DATABASE {db} SET {what}");
    let (mut first, mut out, mut last) = (Vec::new(), Vec::new(), Vec::new());
    let mut files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut service = Vec::new();
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "owner" => {
                check(!value.is_empty(), "dueño", value)?;
                out.push(format!("ALTER AUTHORIZATION ON DATABASE::{db} TO {}", qualified_name(Quote::Bracket, None, value)));
            }
            "collation" => {
                check(word(value), "intercalación", value)?;
                out.push(format!("ALTER DATABASE {db} COLLATE {value}"));
            }
            "recovery" => {
                check(matches!(value, "FULL" | "SIMPLE" | "BULK_LOGGED"), "modelo de recuperación", value)?;
                out.push(set(format!("RECOVERY {value}")));
            }
            "compatibility" => {
                check(value.len() == 3 && value.chars().all(|c| c.is_ascii_digit()), "nivel de compatibilidad", value)?;
                out.push(set(format!("COMPATIBILITY_LEVEL = {value}")));
            }
            "page_verify" => {
                check(matches!(value, "CHECKSUM" | "TORN_PAGE_DETECTION" | "NONE"), "verificación de página", value)?;
                out.push(set(format!("PAGE_VERIFY {value}")));
            }
            "target_recovery_time" => {
                check(!value.is_empty() && value.len() <= 6 && value.chars().all(|c| c.is_ascii_digit()), "tiempo de recuperación", value)?;
                out.push(set(format!("TARGET_RECOVERY_TIME = {value} SECONDS")));
            }
            "delayed_durability" => {
                check(matches!(value, "DISABLED" | "ALLOWED" | "FORCED"), "durabilidad diferida", value)?;
                out.push(set(format!("DELAYED_DURABILITY = {value}")));
            }
            "read_only" => last.push(set(format!("{} WITH ROLLBACK IMMEDIATE", if yes(Some(value)) { "READ_ONLY" } else { "READ_WRITE" }))),
            "user_access" => {
                check(matches!(value, "MULTI_USER" | "RESTRICTED_USER" | "SINGLE_USER"), "acceso de usuarios", value)?;
                last.push(set(format!("{value} WITH ROLLBACK IMMEDIATE")));
            }
            "state" => match value {
                "ONLINE" => first.push(set("ONLINE".into())),
                "OFFLINE" => last.push(set("OFFLINE WITH ROLLBACK IMMEDIATE".into())),
                _ => check(false, "estado", value)?,
            },
            "edition" => {
                check(word(value), "edición", value)?;
                service.push(format!("EDITION = '{value}'"));
            }
            "service_objective" => {
                check(word(value), "objetivo de servicio", value)?;
                service.push(format!("SERVICE_OBJECTIVE = '{value}'"));
            }
            "max_size" => {
                let s = size(value, false).ok_or_else(|| Error::Query(format!("tamaño máximo: «{value}» no es un tamaño")))?;
                let (n, unit) = s.split_at(s.len() - 2);
                service.push(format!("MAXSIZE = {n} {unit}"));
            }
            k if k.starts_with("file:") => {
                let rest = &k[5..];
                let (logical, what) = rest.rsplit_once(':').ok_or_else(|| Error::Query(format!("propiedad desconocida: {k}")))?;
                let spec = match what {
                    "size" => format!("SIZE = {}", size(value, false).ok_or_else(|| Error::Query(format!("{logical}: «{value}» no es un tamaño")))?),
                    "growth" => format!(
                        "FILEGROWTH = {}",
                        size(value, true).ok_or_else(|| Error::Query(format!("{logical}: «{value}» no es un crecimiento")))?
                    ),
                    "max" => format!(
                        "MAXSIZE = {}",
                        if value.eq_ignore_ascii_case("UNLIMITED") {
                            "UNLIMITED".to_string()
                        } else {
                            size(value, false).ok_or_else(|| Error::Query(format!("{logical}: «{value}» no es un tamaño")))?
                        }
                    ),
                    _ => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
                };
                files.entry(logical.to_string()).or_default().push(spec);
            }
            k => match SWITCHES.iter().find(|s| s.0 == k) {
                Some((_, _, kw, _, _)) if *kw == "READ_COMMITTED_SNAPSHOT" => last.push(set(format!("{kw} {} WITH ROLLBACK IMMEDIATE", on_off(value)))),
                Some((_, _, kw, _, _)) => out.push(set(format!("{kw} {}", on_off(value)))),
                None => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
            },
        }
    }
    for (logical, specs) in files {
        out.push(format!("ALTER DATABASE {db} MODIFY FILE ( NAME = {}, {} )", literal(&logical), specs.join(", ")));
    }
    if !service.is_empty() {
        out.push(format!("ALTER DATABASE {db} MODIFY ( {} )", service.join(", ")));
    }
    // OFFLINE goes after everything else (nothing runs on an offline database).
    last.sort_by_key(|s| s.contains("OFFLINE"));
    Ok(first.into_iter().chain(out).chain(last).collect())
}

pub(crate) fn script(v: Variant, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(v, database, changes)?.join("\nGO\n"))
}

fn sel(key: &'static str, label: &'static str, options: Vec<(&'static str, &'static str)>, group: &'static str) -> Field {
    Field::new(key, label, FieldKind::Select(options)).group(group)
}

fn switch(key: &'static str, label: &'static str, group: &'static str) -> Field {
    Field::new(key, label, FieldKind::Bool).group(group)
}

impl SqlServerSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        if !matches!(self.variant, Variant::SqlServer | Variant::AzureSql) {
            return Err(Error::Unsupported("este motor no muestra las propiedades de una base".into()));
        }
        let azure = self.variant == Variant::AzureSql;
        let cols: Vec<&str> = SWITCHES.iter().map(|s| s.3).collect();
        let sql = format!(
            "SELECT state_desc, CONVERT(nvarchar(30), create_date, 120), SUSER_SNAME(owner_sid), collation_name, recovery_model_desc,
                    CAST(compatibility_level AS nvarchar(8)), page_verify_option_desc, CAST(target_recovery_time_in_seconds AS nvarchar(12)),
                    delayed_durability_desc, CAST(is_read_only AS nvarchar(1)), user_access_desc, {}
             FROM sys.databases WHERE name = @P1",
            cols.iter().map(|c| format!("CAST({c} AS nvarchar(1))")).collect::<Vec<_>>().join(", ")
        );
        let rows = self.rows(&sql, &[database]).await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let mut values = BTreeMap::new();
        let mut info = vec![
            PropertyInfo { group: String::new(), label: "Estado".into(), value: text(r, 0).unwrap_or_default() },
            PropertyInfo { group: String::new(), label: "Creada".into(), value: text(r, 1).unwrap_or_default() },
        ];
        let mut put = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                values.insert(k.to_string(), v);
            }
        };
        put("owner", text(r, 2));
        put("collation", text(r, 3));
        put("recovery", text(r, 4));
        put("compatibility", text(r, 5));
        put("page_verify", text(r, 6));
        put("target_recovery_time", text(r, 7));
        put("delayed_durability", text(r, 8));
        put("read_only", Some(if yes(text(r, 9).as_deref()) { "true".into() } else { String::new() }));
        put("user_access", text(r, 10));
        put("state", text(r, 0).filter(|s| s == "ONLINE" || s == "OFFLINE"));
        for (i, s) in SWITCHES.iter().enumerate() {
            put(s.0, Some(if yes(text(r, 11 + i).as_deref()) { "true".into() } else { String::new() }));
        }

        let mut fields = vec![
            Field::new("owner", "Dueño", FieldKind::Text),
            Field::new("collation", "Intercalación (collation)", FieldKind::Text)
                .help("Cambiarla necesita la base para sí: se cierran las demás sesiones."),
        ];
        let compat = crate::create_db::compat_options();
        if azure {
            for (k, prop) in [("edition", "Edition"), ("service_objective", "ServiceObjective")] {
                if let Ok(rows) = self.rows(&format!("SELECT CAST(DATABASEPROPERTYEX(@P1, '{prop}') AS nvarchar(128))"), &[database]).await {
                    put_value(&mut values, k, rows.first().and_then(|r| text(r, 0)));
                }
            }
            fields.extend([
                Field::new("edition", "Edición", FieldKind::Text).group("Servicio"),
                Field::new("service_objective", "Objetivo de servicio", FieldKind::Text).group("Servicio"),
                Field::new("max_size", "Tamaño máximo", FieldKind::Text).placeholder("250 GB").group("Servicio"),
                sel("compatibility", "Nivel de compatibilidad", compat, "Opciones"),
                sel("delayed_durability", "Durabilidad diferida", vec![("DISABLED", "Deshabilitada"), ("ALLOWED", "Permitida"), ("FORCED", "Forzada")], "Opciones"),
            ]);
            for s in SWITCHES.iter().filter(|s| AZURE_SWITCHES.contains(&s.0)) {
                fields.push(switch(s.0, s.1, s.4));
            }
        } else {
            fields.extend([
                sel("recovery", "Modelo de recuperación", vec![("FULL", "Completo (FULL)"), ("SIMPLE", "Simple"), ("BULK_LOGGED", "Registro masivo (BULK_LOGGED)")], "Opciones"),
                sel("compatibility", "Nivel de compatibilidad", compat, "Opciones"),
                sel("page_verify", "Verificación de página", vec![("CHECKSUM", "CHECKSUM"), ("TORN_PAGE_DETECTION", "TORN_PAGE_DETECTION"), ("NONE", "Ninguna")], "Opciones"),
                Field::new("target_recovery_time", "Tiempo de recuperación objetivo (s)", FieldKind::Number).group("Opciones"),
                sel("delayed_durability", "Durabilidad diferida", vec![("DISABLED", "Deshabilitada"), ("ALLOWED", "Permitida"), ("FORCED", "Forzada")], "Opciones"),
                sel("state", "Estado", vec![("ONLINE", "En línea (ONLINE)"), ("OFFLINE", "Fuera de línea (OFFLINE)")], "Estado y acceso"),
                sel("user_access", "Acceso", vec![("MULTI_USER", "Todos (MULTI_USER)"), ("RESTRICTED_USER", "Restringido (RESTRICTED_USER)"), ("SINGLE_USER", "Una sola sesión (SINGLE_USER)")], "Estado y acceso"),
                switch("read_only", "Solo lectura (READ_ONLY)", "Estado y acceso"),
            ]);
            for s in SWITCHES {
                fields.push(switch(s.0, s.1, s.4));
            }
            // Files: logical name, type, physical path, size, growth, maximum.
            let files = self
                .rows(
                    "SELECT name, type_desc, physical_name, CAST(size AS nvarchar(20)), CAST(growth AS nvarchar(20)),
                            CAST(is_percent_growth AS nvarchar(1)), CAST(max_size AS nvarchar(20))
                     FROM sys.master_files WHERE database_id = DB_ID(@P1) ORDER BY file_id",
                    &[database],
                )
                .await?;
            let mut total = 0i64;
            for f in &files {
                let Some(logical) = text(f, 0) else { continue };
                total += text(f, 3).and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);
                // Whole-literal labels, so each one reaches the backend catalog as a pattern.
                info.push(PropertyInfo {
                    group: "Archivos".into(),
                    label: if text(f, 1).as_deref() == Some("LOG") { format!("{logical} (log de transacciones)") } else { format!("{logical} (archivo de datos)") },
                    value: text(f, 2).unwrap_or_default(),
                });
                for (what, kind_) in [("size", FieldKind::Number), ("growth", FieldKind::Text), ("max", FieldKind::Text)] {
                    let key = file_key(&logical, what);
                    let label = match what {
                        "size" => format!("{logical}: tamaño (MB)"),
                        "growth" => format!("{logical}: crecimiento automático"),
                        _ => format!("{logical}: tamaño máximo"),
                    };
                    fields.push(
                        Field::new(intern(&key), intern(&label), kind_)
                            .help(if what == "size" { "Solo puede crecer: para achicar un archivo se usa DBCC SHRINKFILE." } else { "" })
                            .group("Archivos"),
                    );
                    let v = match what {
                        "size" => mb(text(f, 3)),
                        "growth" => growth(text(f, 4), yes(text(f, 5).as_deref())),
                        _ => max_size(text(f, 6)),
                    };
                    put_value(&mut values, &key, v);
                }
            }
            info.insert(2, PropertyInfo { group: String::new(), label: "Tamaño".into(), value: format!("{} MB", total * 8 / 1024) });
        }

        // Suggestions: logins and collations, as for "Nueva base de datos".
        let mut choices: Vec<FieldChoices> = self.create_database_choices_impl().await.unwrap_or_default();
        choices.retain(|c| c.key == "owner" || c.key == "collation");
        for c in &mut choices {
            c.default = None;
        }

        let mut warnings = BTreeMap::new();
        for k in EXCLUSIVE {
            warnings.insert(
                k.to_string(),
                "Este cambio necesita la base para sí: se cierran las demás sesiones y se deshacen sus transacciones en curso (ROLLBACK IMMEDIATE).".to_string(),
            );
        }
        warnings.insert("state".into(), "Fuera de línea (OFFLINE), la base deja de estar accesible para todos hasta volver a ponerla en línea; se cierran las demás sesiones.".into());
        warnings.insert(
            "recovery".into(),
            "Pasar a SIMPLE corta la cadena de backups de log: los backups de log posteriores no podrán restaurarse sobre los anteriores sin un backup completo nuevo.".into(),
        );
        warnings.insert("trustworthy".into(), "TRUSTWORTHY ON permite que el código de la base actúe con permisos fuera de ella: es un riesgo de seguridad.".into());
        warnings.insert("user_access".into(), "SINGLE_USER o RESTRICTED_USER cierran las demás sesiones; con SINGLE_USER, la próxima sesión que entre puede quedarse con la base.".into());
        Ok(DatabaseProperties { fields, values, info, choices, warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(self.variant, database, changes)?;
        for (i, sql) in statements.iter().enumerate() {
            let r = match self.client.simple_query(sql.as_str()).await {
                Ok(s) => s.into_results().await.map(|_| ()),
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                let e = err(e);
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

fn put_value(values: &mut BTreeMap<String, String>, key: &str, v: Option<String>) {
    if let Some(v) = v {
        values.insert(key.to_string(), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn only_the_changes_in_a_safe_order() {
        let s = script(
            Variant::SqlServer,
            "ventas",
            &c(&[
                ("read_only", "true"),
                ("recovery", "SIMPLE"),
                ("auto_shrink", ""),
                ("state", "ONLINE"),
                ("file:ventas:size", "64"),
                ("file:ventas:max", "unlimited"),
            ]),
        )
        .unwrap();
        assert_eq!(
            s,
            "ALTER DATABASE [ventas] SET ONLINE
GO
ALTER DATABASE [ventas] SET AUTO_SHRINK OFF
GO
ALTER DATABASE [ventas] SET RECOVERY SIMPLE
GO
ALTER DATABASE [ventas] MODIFY FILE ( NAME = N'ventas', MAXSIZE = UNLIMITED, SIZE = 64MB )
GO
ALTER DATABASE [ventas] SET READ_ONLY WITH ROLLBACK IMMEDIATE"
        );
    }

    #[test]
    fn offline_goes_last_and_values_are_checked() {
        let s = script(Variant::SqlServer, "v", &c(&[("state", "OFFLINE"), ("user_access", "SINGLE_USER")])).unwrap();
        assert!(s.ends_with("SET OFFLINE WITH ROLLBACK IMMEDIATE"), "{s}");
        for bad in [("recovery", "FULL;"), ("compatibility", "1x0"), ("collation", "a b"), ("file:v:size", "big"), ("nope", "1")] {
            assert!(script(Variant::SqlServer, "v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Variant::Fabric, "v", &c(&[])).is_err());
    }

    #[test]
    fn azure_service_change() {
        assert_eq!(
            script(Variant::AzureSql, "v", &c(&[("service_objective", "S1"), ("max_size", "250GB")])).unwrap(),
            "ALTER DATABASE [v] MODIFY ( MAXSIZE = 250 GB, SERVICE_OBJECTIVE = 'S1' )"
        );
    }
}

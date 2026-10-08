//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//!
//! - SQL Server: collation, owner, the data and log files (path, initial
//!   size, growth, maximum), recovery model and compatibility level. A path
//!   goes in `CREATE DATABASE … ON / LOG ON` (which needs one); sizes without
//!   a path are applied after with `MODIFY FILE` on the files the server
//!   named after the database (`name`, `name_log`). Recovery, compatibility
//!   and owner are `ALTER`s after the create.
//! - Azure SQL Database: collation, edition, service objective, maximum
//!   size or an elastic pool.
//! - Fabric (created in its portal) and Babelfish (no options in T-SQL):
//!   none.
//!
//! Every value is checked before it reaches the SQL.

use crate::variant::Variant;
use crate::{err, text, SqlServerSession};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

const COMPAT: [(&str, &str); 7] = [
    ("160", "160 (SQL Server 2022)"),
    ("150", "150 (SQL Server 2019)"),
    ("140", "140 (SQL Server 2017)"),
    ("130", "130 (SQL Server 2016)"),
    ("120", "120 (SQL Server 2014)"),
    ("110", "110 (SQL Server 2012)"),
    ("100", "100 (SQL Server 2008)"),
];

/// The compatibility levels offered (newest first).
pub(crate) fn compat_options() -> Vec<(&'static str, &'static str)> {
    COMPAT.to_vec()
}

pub(crate) fn fields(v: Variant) -> Vec<Field> {
    let collation = Field::new("collation", "Intercalación (collation)", FieldKind::Text)
        .help("Vacía: la del servidor. Define cómo se ordenan y comparan los textos.");
    match v {
        Variant::SqlServer => vec![
            collation.group("General"),
            Field::new("owner", "Dueño", FieldKind::Text).help("Vacío: el login con el que estás conectado.").group("General"),
            // In pairs, data then log: the tab reads as a two-column table.
            Field::new("data_path", "Carpeta de datos (.mdf)", FieldKind::Text)
                .help("Vacía: la carpeta de datos por defecto del servidor.")
                .group("Archivos"),
            Field::new("log_path", "Carpeta del log (.ldf)", FieldKind::Text)
                .help("Vacía: la carpeta de logs por defecto del servidor. Para elegirla, indicá también la de datos.")
                .group("Archivos"),
            Field::new("data_size", "Datos: tamaño inicial (MB)", FieldKind::Number).group("Archivos"),
            Field::new("log_size", "Log: tamaño inicial (MB)", FieldKind::Number).group("Archivos"),
            Field::new("data_growth", "Datos: crecimiento", FieldKind::Text).placeholder("64MB o 10%").group("Archivos"),
            Field::new("log_growth", "Log: crecimiento", FieldKind::Text).placeholder("64MB o 10%").group("Archivos"),
            Field::new("data_max", "Datos: tamaño máximo", FieldKind::Text).placeholder("UNLIMITED o 10GB").group("Archivos"),
            Field::new("log_max", "Log: tamaño máximo", FieldKind::Text).placeholder("UNLIMITED o 2TB").group("Archivos"),
            Field::new(
                "recovery",
                "Modelo de recuperación",
                FieldKind::Select(vec![("FULL", "Completo (FULL)"), ("SIMPLE", "Simple"), ("BULK_LOGGED", "Registro masivo (BULK_LOGGED)")]),
            )
            .group("Opciones"),
            Field::new("compatibility", "Nivel de compatibilidad", FieldKind::Select(COMPAT.to_vec())).group("Opciones"),
        ],
        Variant::AzureSql => vec![
            collation,
            Field::new(
                "edition",
                "Edición",
                FieldKind::Select(vec![
                    ("Basic", "Basic"),
                    ("Standard", "Standard"),
                    ("Premium", "Premium"),
                    ("GeneralPurpose", "General Purpose"),
                    ("BusinessCritical", "Business Critical"),
                    ("Hyperscale", "Hyperscale"),
                ]),
            ),
            Field::new("service_objective", "Objetivo de servicio", FieldKind::Text).placeholder("S0, GP_Gen5_2, HS_Gen5_4…"),
            Field::new("max_size", "Tamaño máximo", FieldKind::Text).placeholder("250 GB"),
            Field::new("elastic_pool", "Elastic pool", FieldKind::Text)
                .help("Crea la base dentro de ese pool (reemplaza al objetivo de servicio)."),
        ],
        Variant::Fabric | Variant::Babelfish => Vec::new(),
    }
}

pub(crate) fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

pub(crate) fn check(ok: bool, what: &str, v: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Query(format!("{what}: «{v}» no es un valor válido")))
    }
}

pub(crate) fn word(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `64MB`, `1 GB`, `10%` (growth) or a bare number of MB.
pub(crate) fn size(v: &str, percent: bool) -> Option<String> {
    let s: String = v.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_uppercase();
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    match &s[digits.len()..] {
        "" => Some(format!("{digits}MB")),
        u @ ("KB" | "MB" | "GB" | "TB") => Some(format!("{digits}{u}")),
        "%" if percent => Some(format!("{digits}%")),
        _ => None,
    }
}

pub(crate) fn literal(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

/// The file in `dir`, with the separator the path already uses (Windows or
/// Linux servers).
fn file_in(dir: &str, file: &str) -> String {
    let sep = if dir.contains('/') && !dir.contains('\\') { '/' } else { '\\' };
    format!("{}{sep}{file}", dir.trim_end_matches(['/', '\\']))
}

/// One file's `SIZE`, `FILEGROWTH`, `MAXSIZE`, from `prefix_size` /
/// `prefix_growth` / `prefix_max`.
fn file_specs(o: &BTreeMap<String, String>, prefix: &str, what: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(v) = opt(o, &format!("{prefix}_size")) {
        out.push(format!("SIZE = {}", size(v, false).ok_or_else(|| Error::Query(format!("tamaño inicial de {what}: «{v}» no es un tamaño")))?));
    }
    if let Some(v) = opt(o, &format!("{prefix}_growth")) {
        out.push(format!("FILEGROWTH = {}", size(v, true).ok_or_else(|| Error::Query(format!("crecimiento de {what}: «{v}» no es un tamaño ni un porcentaje")))?));
    }
    if let Some(v) = opt(o, &format!("{prefix}_max")) {
        let max = if v.eq_ignore_ascii_case("UNLIMITED") {
            "UNLIMITED".to_string()
        } else {
            size(v, false).ok_or_else(|| Error::Query(format!("tamaño máximo de {what}: «{v}» no es un tamaño ni UNLIMITED")))?
        };
        out.push(format!("MAXSIZE = {max}"));
    }
    Ok(out)
}

/// The batches that create `name`: the `CREATE DATABASE` and the `ALTER`s
/// after it, joined by `GO` in [`script`].
pub(crate) fn batches(v: Variant, name: &str, o: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if v == Variant::Fabric {
        return Err(Error::Unsupported("un warehouse de Fabric se crea desde el portal de Fabric".into()));
    }
    let db = qualified_name(Quote::Bracket, None, name);
    let collate = match opt(o, "collation") {
        Some(c) => {
            check(word(c), "intercalación", c)?;
            format!("\nCOLLATE {c}")
        }
        None => String::new(),
    };
    if v != Variant::SqlServer && v != Variant::AzureSql {
        return Ok(vec![format!("CREATE DATABASE {db}{collate}")]);
    }
    if v == Variant::AzureSql {
        let mut with = Vec::new();
        if let Some(e) = opt(o, "edition") {
            check(word(e), "edición", e)?;
            with.push(format!("EDITION = '{e}'"));
        }
        if let Some(pool) = opt(o, "elastic_pool") {
            with.push(format!("SERVICE_OBJECTIVE = ELASTIC_POOL (name = {})", qualified_name(Quote::Bracket, None, pool)));
        } else if let Some(s) = opt(o, "service_objective") {
            check(word(s), "objetivo de servicio", s)?;
            with.push(format!("SERVICE_OBJECTIVE = '{s}'"));
        }
        if let Some(m) = opt(o, "max_size") {
            let s = size(m, false).ok_or_else(|| Error::Query(format!("tamaño máximo: «{m}» no es un tamaño")))?;
            // Azure takes `250 GB`, with the space.
            let (n, unit) = s.split_at(s.len() - 2);
            with.push(format!("MAXSIZE = {n} {unit}"));
        }
        let with = if with.is_empty() { String::new() } else { format!("\n( {} )", with.join(", ")) };
        return Ok(vec![format!("CREATE DATABASE {db}{collate}{with}")]);
    }

    // SQL Server.
    let data = file_specs(o, "data", "datos")?;
    let log = file_specs(o, "log", "log")?;
    let data_path = opt(o, "data_path");
    let log_path = opt(o, "log_path");
    let logical = |suffix: &str| literal(&format!("{name}{suffix}"));
    let mut on = String::new();
    let mut after = Vec::new();
    if let Some(dir) = data_path {
        let mut spec = vec![format!("NAME = {}", logical("")), format!("FILENAME = {}", literal(&file_in(dir, &format!("{name}.mdf"))))];
        spec.extend(data.iter().cloned());
        on = format!("\nON PRIMARY ( {} )", spec.join(", "));
    } else if !data.is_empty() {
        after.push(format!("ALTER DATABASE {db} MODIFY FILE ( NAME = {}, {} )", logical(""), data.join(", ")));
    }
    if let Some(dir) = log_path {
        if on.is_empty() {
            // LOG ON needs the data file declared too: the default folder
            // isn't known here, so ask for both.
            return Err(Error::Query(
                "para elegir la carpeta del log, indicá también la carpeta del archivo de datos".into(),
            ));
        }
        let mut spec = vec![format!("NAME = {}", logical("_log")), format!("FILENAME = {}", literal(&file_in(dir, &format!("{name}_log.ldf"))))];
        spec.extend(log.iter().cloned());
        on.push_str(&format!("\nLOG ON ( {} )", spec.join(", ")));
    } else if !log.is_empty() {
        if data_path.is_some() {
            // The server would name the log after the data file's folder:
            // declare it there.
            let dir = data_path.unwrap_or_default();
            let mut spec = vec![format!("NAME = {}", logical("_log")), format!("FILENAME = {}", literal(&file_in(dir, &format!("{name}_log.ldf"))))];
            spec.extend(log.iter().cloned());
            on.push_str(&format!("\nLOG ON ( {} )", spec.join(", ")));
        } else {
            after.push(format!("ALTER DATABASE {db} MODIFY FILE ( NAME = {}, {} )", logical("_log"), log.join(", ")));
        }
    }
    let mut out = vec![format!("CREATE DATABASE {db}{on}{collate}")];
    out.extend(after);
    if let Some(r) = opt(o, "recovery") {
        check(matches!(r, "FULL" | "SIMPLE" | "BULK_LOGGED"), "modelo de recuperación", r)?;
        out.push(format!("ALTER DATABASE {db} SET RECOVERY {r}"));
    }
    if let Some(c) = opt(o, "compatibility") {
        check(c.len() == 3 && c.chars().all(|ch| ch.is_ascii_digit()), "nivel de compatibilidad", c)?;
        out.push(format!("ALTER DATABASE {db} SET COMPATIBILITY_LEVEL = {c}"));
    }
    if let Some(owner) = opt(o, "owner") {
        out.push(format!("ALTER AUTHORIZATION ON DATABASE::{db} TO {}", qualified_name(Quote::Bracket, None, owner)));
    }
    Ok(out)
}

/// What "Ver script" shows: the batches, separated by `GO`.
pub(crate) fn script(v: Variant, name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    Ok(batches(v, name, o)?.join("\nGO\n"))
}

impl SqlServerSession {
    /// The server's collations (default: its own), its logins (default: the
    /// one connected), its default data and log folders, and `model`'s
    /// recovery model and compatibility level.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let first = |rows: Vec<tiberius::Row>| rows.first().and_then(|r| text(r, 0));
        let all = |rows: Vec<tiberius::Row>| rows.iter().filter_map(|r| text(r, 0)).collect::<Vec<_>>();
        let mut out = Vec::new();
        match self.variant {
            Variant::SqlServer | Variant::AzureSql => {
                let collations = self.rows("SELECT name FROM sys.fn_helpcollations() ORDER BY name", &[]).await.map(all).unwrap_or_default();
                let collation = self.rows("SELECT CAST(SERVERPROPERTY('Collation') AS nvarchar(128))", &[]).await.ok().and_then(first);
                out.push(FieldChoices { key: "collation".into(), default: collation, values: collations });
            }
            _ => return Ok(out),
        }
        if self.variant != Variant::SqlServer {
            return Ok(out);
        }
        let logins = self
            .rows(
                "SELECT name FROM sys.server_principals
                 WHERE type IN ('S', 'U', 'G', 'E', 'X') AND is_disabled = 0 AND name NOT LIKE '##%'
                 ORDER BY name",
                &[],
            )
            .await
            .map(all)
            .unwrap_or_default();
        let me = self.rows("SELECT SUSER_SNAME()", &[]).await.ok().and_then(first);
        out.push(FieldChoices { key: "owner".into(), default: me, values: logins });
        for (key, prop) in [("data_path", "InstanceDefaultDataPath"), ("log_path", "InstanceDefaultLogPath")] {
            let dir = self.rows(&format!("SELECT CAST(SERVERPROPERTY('{prop}') AS nvarchar(4000))"), &[]).await.ok().and_then(first);
            out.push(FieldChoices { key: key.into(), default: dir.clone(), values: dir.into_iter().collect() });
        }
        if let Ok(rows) = self.rows("SELECT recovery_model_desc, CAST(compatibility_level AS nvarchar(8)) FROM sys.databases WHERE name = 'model'", &[]).await {
            if let Some(r) = rows.first() {
                out.push(FieldChoices { key: "recovery".into(), default: text(r, 0), values: Vec::new() });
                out.push(FieldChoices { key: "compatibility".into(), default: text(r, 1), values: Vec::new() });
            }
        }
        Ok(out)
    }

    /// Run the batches one by one: a failure after the create says the
    /// database exists but that step didn't apply.
    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let batches = batches(self.variant, name, o)?;
        for (i, sql) in batches.iter().enumerate() {
            let sent = self.client.simple_query(sql.as_str()).await;
            let r = match sent {
                Ok(s) => s.into_results().await.map(|_| ()),
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                let e = err(e);
                return Err(if i == 0 {
                    e
                } else {
                    Error::Query(format!("la base «{name}» se creó, pero falló este paso: {sql}\n{e}"))
                });
            }
        }
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
        assert_eq!(script(Variant::SqlServer, "ventas", &o(&[])).unwrap(), "CREATE DATABASE [ventas]");
        assert_eq!(script(Variant::SqlServer, "ven]tas", &o(&[("collation", " ")])).unwrap(), "CREATE DATABASE [ven]]tas]");
    }

    #[test]
    fn files_collation_and_alters() {
        let s = script(
            Variant::SqlServer,
            "ventas",
            &o(&[
                ("collation", "Latin1_General_CI_AS"),
                ("data_path", "D:\\Datos\\"),
                ("data_size", "512"),
                ("data_growth", "10%"),
                ("log_path", "L:\\Logs"),
                ("log_max", "2 tb"),
                ("recovery", "SIMPLE"),
                ("compatibility", "150"),
                ("owner", "sa"),
            ]),
        )
        .unwrap();
        assert_eq!(
            s,
            "CREATE DATABASE [ventas]
ON PRIMARY ( NAME = N'ventas', FILENAME = N'D:\\Datos\\ventas.mdf', SIZE = 512MB, FILEGROWTH = 10% )
LOG ON ( NAME = N'ventas_log', FILENAME = N'L:\\Logs\\ventas_log.ldf', MAXSIZE = 2TB )
COLLATE Latin1_General_CI_AS
GO
ALTER DATABASE [ventas] SET RECOVERY SIMPLE
GO
ALTER DATABASE [ventas] SET COMPATIBILITY_LEVEL = 150
GO
ALTER AUTHORIZATION ON DATABASE::[ventas] TO [sa]"
        );
    }

    #[test]
    fn sizes_without_a_path_modify_the_files_after() {
        let s = script(Variant::SqlServer, "v", &o(&[("data_size", "100"), ("log_growth", "64MB")])).unwrap();
        assert_eq!(
            s,
            "CREATE DATABASE [v]\nGO\nALTER DATABASE [v] MODIFY FILE ( NAME = N'v', SIZE = 100MB )\nGO\nALTER DATABASE [v] MODIFY FILE ( NAME = N'v_log', FILEGROWTH = 64MB )"
        );
        // A Linux server's path keeps its separator.
        assert!(script(Variant::SqlServer, "v", &o(&[("data_path", "/var/opt/mssql/data")])).unwrap().contains("N'/var/opt/mssql/data/v.mdf'"));
    }

    #[test]
    fn values_are_checked() {
        for bad in [("collation", "x; DROP"), ("data_size", "big"), ("data_growth", "10%%"), ("recovery", "FULL;"), ("compatibility", "15O")] {
            assert!(script(Variant::SqlServer, "v", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Variant::SqlServer, "v", &o(&[("log_path", "L:\\")])).is_err(), "log folder needs the data one");
        assert!(script(Variant::Fabric, "v", &o(&[])).is_err());
    }

    #[test]
    fn azure_service_options() {
        assert_eq!(
            script(Variant::AzureSql, "v", &o(&[("edition", "GeneralPurpose"), ("service_objective", "GP_Gen5_2"), ("max_size", "250GB")])).unwrap(),
            "CREATE DATABASE [v]\n( EDITION = 'GeneralPurpose', SERVICE_OBJECTIVE = 'GP_Gen5_2', MAXSIZE = 250 GB )"
        );
        assert!(script(Variant::AzureSql, "v", &o(&[("elastic_pool", "pool1")])).unwrap().contains("ELASTIC_POOL (name = [pool1])"));
    }
}

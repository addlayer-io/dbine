//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]),
//! for the presets whose explorer lists databases and whose engine has
//! database-level settings:
//!
//! - Sybase ASE: owner, created, size (data and log) and the `sp_dboption`
//!   options read from `master..sysdatabases` status bits; each change is
//!   one `sp_dboption` call, run from `master` (as ASE requires), and the
//!   session goes back to its database after.
//! - Netezza: what `_V_DATABASE` reports; `ALTER DATABASE` changes the
//!   owner, the default schema, query history collection and the time
//!   travel retention (each only when the catalog reports its value).
//! - Informix (and GBase 8s): owner, created, dbspace and logging mode,
//!   as facts. Its logging mode changes with ondblog / ontape and a level-0
//!   backup, not with SQL, so nothing is offered to change.
//! - Every other preset: none. Db2, Teradata and the rest list schemas (or
//!   a single database) rather than databases, and the generic preset
//!   doesn't know the engine behind it.

use crate::design::{self, Eng};
use crate::presets::Preset;
use crate::{col, OdbcSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use std::collections::BTreeMap;

pub(crate) fn supported(p: &Preset) -> bool {
    matches!(design::eng(p), Eng::Ase | Eng::Netezza | Eng::Informix)
}

/// ASE's `sp_dboption` options: field key, label, option name,
/// `sysdatabases` status column (0: `status`, 1: `status2`), bit.
const ASE_OPTIONS: &[(&str, &str, &str, usize, i64)] = &[
    ("select_into", "Operaciones mínimamente registradas (select into/bulkcopy/pllsort)", "select into/bulkcopy/pllsort", 0, 4),
    ("trunc_log", "Truncar el log en cada checkpoint (trunc log on chkpt)", "trunc log on chkpt", 0, 8),
    ("no_chkpt", "Sin checkpoint al recuperar (no chkpt on recovery)", "no chkpt on recovery", 0, 16),
    ("ddl_in_tran", "DDL dentro de transacciones (ddl in tran)", "ddl in tran", 0, 512),
    ("read_only", "Solo lectura (read only)", "read only", 0, 1024),
    ("dbo_only", "Solo el dueño (dbo use only)", "dbo use only", 0, 2048),
    ("single_user", "Un solo usuario (single user)", "single user", 0, 4096),
    ("nulls_default", "Columnas NULL por defecto (allow nulls by default)", "allow nulls by default", 0, 8192),
    ("abort_tran", "Cancelar transacciones con el log lleno (abort tran on log full)", "abort tran on log full", 1, 1),
    ("no_free_space", "Sin contabilidad de espacio libre (no free space acctg)", "no free space acctg", 1, 2),
    ("auto_identity", "Columna IDENTITY automática (auto identity)", "auto identity", 1, 4),
    ("identity_index", "IDENTITY en índices no únicos (identity in nonunique index)", "identity in nonunique index", 1, 8),
];

/// ASE options that restrict who can use the database: turned off first,
/// on last.
const ASE_RESTRICT: &[&str] = &["read_only", "dbo_only", "single_user"];

fn yes(v: &str) -> bool {
    matches!(v.trim(), "true" | "1" | "ON" | "on")
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The statements for `changes` (ASE starts with `USE master`).
pub(crate) fn statements(p: &Preset, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if database.trim().is_empty() {
        return Err(Error::Query("Falta el nombre de la base".into()));
    }
    let unknown = |k: &str| Error::Query(format!("propiedad desconocida: {k}"));
    match design::eng(p) {
        Eng::Ase => {
            let (mut first, mut out, mut last) = (Vec::new(), Vec::new(), Vec::new());
            for (key, value) in changes {
                let (_, _, option, _, _) = ASE_OPTIONS.iter().find(|o| o.0 == key).ok_or_else(|| unknown(key))?;
                let on = yes(value);
                let sql = format!("EXEC sp_dboption {}, '{option}', {}", literal(database), if on { "true" } else { "false" });
                match (ASE_RESTRICT.contains(&key.as_str()), on) {
                    (true, true) => last.push(sql),
                    (true, false) => first.push(sql),
                    _ => out.push(sql),
                }
            }
            if first.is_empty() && out.is_empty() && last.is_empty() {
                return Ok(Vec::new());
            }
            Ok(std::iter::once("USE master".to_string()).chain(first).chain(out).chain(last).collect())
        }
        Eng::Netezza => {
            let q = p.quote.unwrap_or(Quote::Double);
            let db = format!("ALTER DATABASE {}", quote_ident(q, database));
            // Simple names fold to upper case, as Netezza does unquoted.
            let name = |v: &str| {
                let simple = v.starts_with(|c: char| c.is_ascii_alphabetic()) && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                quote_ident(q, &if simple { v.to_ascii_uppercase() } else { v.to_string() })
            };
            let mut out = Vec::new();
            for (key, value) in changes {
                let value = value.trim();
                out.push(match key.as_str() {
                    "owner" if !value.is_empty() => format!("{db} OWNER TO {}", name(value)),
                    "default_schema" if !value.is_empty() => format!("{db} SET DEFAULT SCHEMA {}", name(value)),
                    "collect_history" if matches!(value, "ON" | "OFF" | "DEFAULT") => format!("{db} COLLECT HISTORY {value}"),
                    "retention_days" if value.len() <= 2 && !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()) => {
                        format!("{db} DATA VERSION RETENTION TIME {value}")
                    }
                    "owner" | "default_schema" | "collect_history" | "retention_days" => {
                        return Err(Error::Query(format!("{key}: «{value}» no es un valor válido")))
                    }
                    k => return Err(unknown(k)),
                });
            }
            Ok(out)
        }
        Eng::Informix => match changes.keys().next() {
            None => Ok(Vec::new()),
            Some(k) => Err(Error::Query(format!(
                "propiedad desconocida: {k} (el modo de log de Informix se cambia con ondblog u ontape, no con SQL)"
            ))),
        },
        _ => Err(Error::Unsupported(format!("{} no modifica las propiedades de una base", p.name))),
    }
}

pub(crate) fn script(p: &Preset, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    let sep = if design::eng(p) == Eng::Ase { "\ngo\n" } else { ";\n" };
    let s = statements(p, database, changes)?;
    Ok(if design::eng(p) == Eng::Ase || s.is_empty() { s.join(sep) } else { format!("{};", s.join(sep)) })
}

fn fact(group: &str, label: &str, value: String) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value }
}

impl OdbcSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        match design::eng(self.preset) {
            Eng::Ase => self.ase_properties(database).await,
            Eng::Netezza => self.netezza_properties(database).await,
            Eng::Informix => self.informix_properties(database).await,
            _ => Err(Error::Unsupported(format!("{} no muestra las propiedades de una base", self.preset.name))),
        }
    }

    async fn ase_properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let rows = self
            .query(
                "SELECT dbid, suser_name(suid), convert(varchar(30), crdate), status, status2 FROM master..sysdatabases WHERE name = ?".into(),
                vec![database.to_string()],
            )
            .await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let bits = |i: usize| col(r, 3 + i).and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(0);
        let mut info = vec![
            fact("", "Dueño", col(r, 1).unwrap_or_default()),
            fact("", "Creada", col(r, 2).unwrap_or_default()),
            fact("", "ID", col(r, 0).unwrap_or_default().trim().to_string()),
        ];
        if bits(0) & 256 != 0 {
            info.push(fact("", "Estado", "Sospechosa (suspect)".into()));
        }
        if bits(1) & 16 != 0 {
            info.push(fact("", "Estado", "Fuera de línea (offline)".into()));
        }
        // sysusages.size is in logical pages; segmap 4 is log only.
        let size = self
            .query(
                "SELECT sum(CASE WHEN segmap = 4 THEN 0 ELSE size END) * (@@maxpagesize / 1024) / 1024,
                        sum(CASE WHEN segmap = 4 THEN size ELSE 0 END) * (@@maxpagesize / 1024) / 1024
                 FROM master..sysusages WHERE dbid = db_id(?)"
                    .into(),
                vec![database.to_string()],
            )
            .await
            .unwrap_or_default();
        if let Some(s) = size.first() {
            info.push(fact("", "Datos (MB)", col(s, 0).unwrap_or_default().trim().to_string()));
            info.push(fact("", "Log (MB)", col(s, 1).unwrap_or_default().trim().to_string()));
        }
        let mut fields = Vec::new();
        let mut values = BTreeMap::new();
        for (key, label, _, column, bit) in ASE_OPTIONS {
            let group = if ASE_RESTRICT.contains(key) { "Acceso" } else { "Opciones" };
            fields.push(Field::new(key, label, FieldKind::Bool).group(group));
            values.insert(key.to_string(), if bits(*column) & bit != 0 { "true".into() } else { String::new() });
        }
        let mut warnings = BTreeMap::new();
        for (k, w) in [
            ("read_only", "En solo lectura nadie puede modificar la base."),
            ("dbo_only", "Solo el dueño (dbo) podrá usar la base; las demás sesiones no pueden entrar."),
            ("single_user", "Solo una sesión podrá usar la base; el cambio falla si hay otras conectadas."),
            ("trunc_log", "Truncar el log en cada checkpoint impide hacer dump transaction: se corta la cadena de backups del log."),
            (
                "select_into",
                "Las operaciones mínimamente registradas impiden hacer dump transaction hasta el próximo dump database.",
            ),
            ("abort_tran", "Con el log lleno, las transacciones se cancelan en vez de esperar a que haya lugar."),
            ("no_free_space", "Sin contabilidad de espacio libre no se disparan los umbrales (thresholds) de los segmentos de datos."),
        ] {
            warnings.insert(k.to_string(), w.to_string());
        }
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    async fn netezza_properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let (cols, rows) = self.query_named("SELECT * FROM _V_DATABASE WHERE DATABASE = ?".into(), vec![database.to_string()]).await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let find = |names: &[&str]| cols.iter().position(|c| names.iter().any(|n| c.eq_ignore_ascii_case(n))).map(|i| col(r, i));
        let mut info = Vec::new();
        let mut fields = Vec::new();
        let mut values = BTreeMap::new();
        if let Some(c) = find(&["CREATEDATE"]) {
            info.push(fact("", "Creada", c.unwrap_or_default()));
        }
        if let Some(o) = find(&["OWNER"]) {
            values.insert("owner".to_string(), o.unwrap_or_default().trim().to_string());
            fields.push(Field::new("owner", "Dueño", FieldKind::Text));
        }
        if let Some(s) = find(&["DEFSCHEMA", "DEFAULTSCHEMA", "DEFAULT_SCHEMA"]) {
            values.insert("default_schema".to_string(), s.unwrap_or_default().trim().to_string());
            fields.push(Field::new("default_schema", "Esquema por defecto", FieldKind::Text));
        }
        if let Some(h) = find(&["COLLECTHISTORY", "COLLECT_HISTORY"]) {
            let h = h.unwrap_or_default().trim().to_ascii_uppercase();
            let v = match h.as_str() {
                "T" | "TRUE" | "1" | "ON" => "ON",
                "F" | "FALSE" | "0" | "OFF" => "OFF",
                _ => "DEFAULT",
            };
            values.insert("collect_history".to_string(), v.to_string());
            fields.push(
                Field::new(
                    "collect_history",
                    "Historial de consultas (COLLECT HISTORY)",
                    FieldKind::Select(vec![("ON", "Recolectar (ON)"), ("OFF", "No recolectar (OFF)"), ("DEFAULT", "Según la configuración (DEFAULT)")]),
                )
                .group("Opciones"),
            );
        }
        if let Some(d) = find(&["DATAVERSIONRETENTIONTIME", "DATA_VERSION_RETENTION_TIME", "RETENTIONTIME"]) {
            values.insert("retention_days".to_string(), d.unwrap_or_default().trim().to_string());
            fields.push(
                Field::new("retention_days", "Retención de versiones (días)", FieldKind::Number)
                    .help("DATA VERSION RETENTION TIME, para consultas sobre datos pasados (time travel). 0: sin retención.")
                    .group("Opciones"),
            );
        }
        // Whatever else the catalog reports, as facts.
        for (i, c) in cols.iter().enumerate() {
            let known = ["DATABASE", "CREATEDATE", "OWNER", "DEFSCHEMA", "DEFAULTSCHEMA", "DEFAULT_SCHEMA", "COLLECTHISTORY", "COLLECT_HISTORY"];
            if !known.iter().any(|k| c.eq_ignore_ascii_case(k)) && !c.to_ascii_uppercase().contains("RETENTION") {
                if let Some(v) = col(r, i).filter(|v| !v.trim().is_empty()) {
                    info.push(fact("Catálogo", c, v.trim().to_string()));
                }
            }
        }
        let users = self.query("SELECT USERNAME FROM _V_USER ORDER BY USERNAME".into(), Vec::new()).await.unwrap_or_default();
        let choices = vec![FieldChoices { key: "owner".into(), default: None, values: users.iter().filter_map(|r| col(r, 0)).collect() }];
        let mut warnings = BTreeMap::new();
        warnings.insert(
            "retention_days".to_string(),
            "Bajar la retención descarta las versiones anteriores de los datos: las consultas a fechas pasadas dejan de funcionar más allá del nuevo plazo.".to_string(),
        );
        Ok(DatabaseProperties { fields, values, info, choices, warnings })
    }

    async fn informix_properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let rows = self
            .query(
                "SELECT owner, created, is_logging, is_buff_log, is_ansi, is_nls FROM sysmaster:sysdatabases WHERE name = ?".into(),
                vec![database.to_string()],
            )
            .await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let on = |i: usize| col(r, i).is_some_and(|v| v.trim() == "1");
        let logging = if on(4) {
            "ANSI"
        } else if on(2) && on(3) {
            "Con log en búfer (buffered)"
        } else if on(2) {
            "Con log (unbuffered)"
        } else {
            "Sin log"
        };
        let mut info = vec![
            fact("", "Dueño", col(r, 0).unwrap_or_default().trim().to_string()),
            fact("", "Creada", col(r, 1).unwrap_or_default()),
            fact("", "Modo de log", logging.into()),
            fact("", "Idioma nacional (NLS)", if on(5) { "Sí" } else { "No" }.into()),
        ];
        let space = self
            .query("SELECT DBINFO('dbspace', partnum) FROM sysmaster:sysdatabases WHERE name = ?".into(), vec![database.to_string()])
            .await
            .unwrap_or_default();
        if let Some(s) = space.first().and_then(|r| col(r, 0)) {
            info.push(fact("", "Dbspace", s.trim().to_string()));
        }
        info.push(fact("", "Cómo cambiar el modo de log", "Con ondblog u ontape (requiere un backup de nivel 0), no con SQL.".into()));
        Ok(DatabaseProperties { info, ..Default::default() })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let stmts = statements(self.preset, database, changes)?;
        if stmts.is_empty() {
            return Ok(());
        }
        let back = (design::eng(self.preset) == Eng::Ase && !self.database.is_empty()).then(|| format!("USE {}", quote_ident(self.quote, &self.database)));
        self.run(move |c, slot| {
            let exec = |s: &str| c.stmt(slot).and_then(|st| st.exec(s).map(|_| ()));
            // `USE master` isn't one of the changes.
            let skip = usize::from(stmts.first().is_some_and(|s| s == "USE master"));
            let total = stmts.len() - skip;
            let mut result = Ok(());
            for (i, s) in stmts.iter().enumerate() {
                if let Err(e) = exec(s) {
                    let done = i.saturating_sub(skip);
                    result = Err(if done == 0 { e } else { Error::Query(format!("se aplicaron {done} de {total} cambios; falló: {s}\n{e}")) });
                    break;
                }
            }
            if let Some(b) = back {
                if let Err(e) = exec(&b) {
                    tracing::debug!("odbc: {b}: {e}");
                }
            }
            result
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &str) -> &'static Preset {
        crate::presets::PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn ase_options_from_master_in_a_safe_order() {
        assert_eq!(
            script(p("sybase"), "ven'tas", &c(&[("single_user", "true"), ("trunc_log", "true"), ("read_only", ""), ("abort_tran", "")])).unwrap(),
            "USE master
go
EXEC sp_dboption 'ven''tas', 'read only', false
go
EXEC sp_dboption 'ven''tas', 'abort tran on log full', false
go
EXEC sp_dboption 'ven''tas', 'trunc log on chkpt', true
go
EXEC sp_dboption 'ven''tas', 'single user', true"
        );
        assert_eq!(script(p("sybase"), "v", &c(&[])).unwrap(), "");
        assert!(script(p("sybase"), "v", &c(&[("nope", "true")])).is_err());
    }

    #[test]
    fn netezza_alter_database() {
        assert_eq!(
            script(p("netezza"), "ventas", &c(&[("owner", "admin"), ("collect_history", "OFF"), ("retention_days", "7"), ("default_schema", "s1")])).unwrap(),
            "ALTER DATABASE \"ventas\" COLLECT HISTORY OFF;\nALTER DATABASE \"ventas\" SET DEFAULT SCHEMA \"S1\";\nALTER DATABASE \"ventas\" OWNER TO \"ADMIN\";\nALTER DATABASE \"ventas\" DATA VERSION RETENTION TIME 7;"
        );
        for bad in [("collect_history", "ON;"), ("retention_days", "100"), ("retention_days", "x"), ("owner", " "), ("nope", "1")] {
            assert!(script(p("netezza"), "v", &c(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn who_has_properties() {
        assert!(supported(p("sybase")) && supported(p("netezza")) && supported(p("informix")));
        assert!(!supported(p("db2")) && !supported(p("teradata")) && !supported(p("odbc")));
        assert_eq!(script(p("informix"), "v", &c(&[])).unwrap(), "");
        assert!(script(p("informix"), "v", &c(&[("logging", "ANSI")])).is_err());
        assert!(script(p("db2"), "v", &c(&[])).is_err());
    }
}

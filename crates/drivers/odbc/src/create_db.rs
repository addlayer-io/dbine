//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//!
//! Only the presets that create databases from DBine (see
//! [`crate::design::capabilities`]):
//!
//! - Sybase ASE: the data and log devices with their sizes (`ON device =
//!   size`, `LOG ON device = size`); `WITH OVERRIDE` when both share a
//!   device, as ASE requires. It runs from `master`.
//! - Netezza: query history collection (`COLLECT HISTORY`) and the time
//!   travel retention (`DATA VERSION RETENTION TIME`).
//! - The generic preset: none, the engine behind it isn't known.
//!
//! Every value is checked before it reaches the SQL.

use crate::design::{self, Eng};
use crate::presets::Preset;
use crate::{col, OdbcSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields(p: &Preset) -> Vec<Field> {
    match design::eng(p) {
        Eng::Ase => vec![
            Field::new("data_device", "Dispositivo de datos", FieldKind::Text)
                .help("Vacío: los dispositivos por defecto del servidor (ON DEFAULT)."),
            Field::new("data_size", "Tamaño de datos", FieldKind::Text).placeholder("100 (MB), 500M o 2G"),
            Field::new("log_device", "Dispositivo del log (LOG ON)", FieldKind::Text)
                .help("Vacío: el log comparte los dispositivos de datos."),
            Field::new("log_size", "Tamaño del log", FieldKind::Text).placeholder("50 (MB), 200M o 1G"),
        ],
        Eng::Netezza => vec![
            Field::new(
                "collect_history",
                "Historial de consultas (COLLECT HISTORY)",
                FieldKind::Select(vec![("ON", "Recolectar (ON)"), ("OFF", "No recolectar (OFF)"), ("DEFAULT", "Según la configuración (DEFAULT)")]),
            ),
            Field::new("retention_days", "Retención de versiones (días)", FieldKind::Number)
                .help("DATA VERSION RETENTION TIME, para consultas sobre datos pasados (time travel). Vacío: la del sistema."),
        ],
        _ => Vec::new(),
    }
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn check(ok: bool, what: &str, v: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Query(format!("{what}: «{v}» no es un valor válido")))
    }
}

fn word(v: &str) -> bool {
    !v.is_empty() && v.len() <= 255 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// An ASE size: a bare number of MB, or `'500M'` with its unit (K, M, G, T).
fn ase_size(v: &str, what: &str) -> Result<String> {
    let s = v.replace(' ', "").to_ascii_uppercase();
    let s = s.trim_end_matches('B');
    let digits = s.trim_end_matches(['K', 'M', 'G', 'T']);
    let unit = &s[digits.len()..];
    if digits.is_empty() || digits.len() > 9 || !digits.chars().all(|c| c.is_ascii_digit()) || unit.len() > 1 {
        return Err(Error::Query(format!("{what}: «{v}» no es un tamaño (100, 500M, 2G…)")));
    }
    Ok(if unit.is_empty() { digits.to_string() } else { format!("'{digits}{unit}'") })
}

/// `device [= size]`, `DEFAULT` when no device is named.
fn ase_device(o: &BTreeMap<String, String>, device: &str, size: &str, what: &str) -> Result<Option<String>> {
    let dev = opt(o, device);
    let size = opt(o, size).map(|s| ase_size(s, what)).transpose()?;
    if let Some(d) = dev {
        check(word(d), what, d)?;
    }
    Ok(match (dev, size) {
        (None, None) => None,
        (d, s) => Some(format!("{}{}", d.unwrap_or("DEFAULT"), s.map(|s| format!(" = {s}")).unwrap_or_default())),
    })
}

/// The statements that create `name`: Sybase ASE switches to `master`
/// first (and the session goes back to its database after).
pub(crate) fn statements(p: &Preset, name: &str, o: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta el nombre de la base".into()));
    }
    let eng = design::eng(p);
    let quote = p.quote.unwrap_or(if eng == Eng::Ase { Quote::Bracket } else { Quote::Double });
    let mut sql = format!("CREATE DATABASE {}", quote_ident(quote, name));
    match eng {
        Eng::Ase => {
            let data = ase_device(o, "data_device", "data_size", "dispositivo de datos")?;
            let log = ase_device(o, "log_device", "log_size", "dispositivo del log")?;
            if let Some(d) = &data {
                sql.push_str(&format!("\nON {d}"));
            }
            if let Some(l) = &log {
                if opt(o, "log_device").is_none() {
                    return Err(Error::Query("para el tamaño del log, indicá su dispositivo".into()));
                }
                sql.push_str(&format!("\nLOG ON {l}"));
                if opt(o, "data_device").is_some_and(|d| opt(o, "log_device").is_some_and(|l| l.eq_ignore_ascii_case(d))) {
                    sql.push_str("\nWITH OVERRIDE");
                }
            }
            Ok(vec!["USE master".into(), sql])
        }
        Eng::Netezza => {
            if let Some(h) = opt(o, "collect_history") {
                check(matches!(h, "ON" | "OFF" | "DEFAULT"), "historial de consultas", h)?;
                sql.push_str(&format!("\nCOLLECT HISTORY {h}"));
            }
            if let Some(d) = opt(o, "retention_days") {
                check(d.len() <= 2 && d.chars().all(|c| c.is_ascii_digit()), "retención de versiones", d)?;
                sql.push_str(&format!("\nDATA VERSION RETENTION TIME {d}"));
            }
            Ok(vec![sql])
        }
        _ if o.values().all(|v| v.trim().is_empty()) => Ok(vec![sql]),
        _ => Err(Error::Unsupported(format!("{} no admite opciones al crear una base", p.name))),
    }
}

/// What "Ver script" shows (the generic preset has no options, so no
/// script: its quoting comes from the driver at connect time).
pub(crate) fn script(p: &Preset, name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    if !matches!(design::eng(p), Eng::Ase | Eng::Netezza) {
        return Err(Error::Unsupported(format!("{} no genera el script de creación de una base", p.name)));
    }
    let sep = if design::eng(p) == Eng::Ase { "\ngo\n" } else { ";\n" };
    Ok(statements(p, name, o)?.join(sep))
}

impl OdbcSession {
    /// Sybase ASE's database devices (default: the default ones).
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        if design::eng(self.preset) != Eng::Ase {
            return Ok(Vec::new());
        }
        // sysdevices.status: 1 default disk, 2 physical (database) disk.
        let rows = self
            .query("SELECT name, status FROM master..sysdevices WHERE status & 2 = 2 ORDER BY name".into(), Vec::new())
            .await
            .unwrap_or_default();
        let devices: Vec<String> = rows.iter().filter_map(|r| col(r, 0)).map(|s| s.trim().to_string()).collect();
        let default = rows
            .iter()
            .filter(|r| col(r, 1).and_then(|s| s.trim().parse::<i64>().ok()).is_some_and(|s| s & 1 == 1))
            .filter_map(|r| col(r, 0))
            .map(|s| s.trim().to_string())
            .collect::<Vec<_>>();
        let default = (!default.is_empty()).then(|| default.join(", "));
        Ok(vec![
            FieldChoices { key: "data_device".into(), default, values: devices.clone() },
            FieldChoices { key: "log_device".into(), default: None, values: devices },
        ])
    }

    /// Run [`statements`]; Sybase ASE goes back to the session's database
    /// after, even when the create failed.
    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        if !design::capabilities(self.preset).create_database {
            return Err(Error::Unsupported(format!("{} no crea ni borra bases desde DBine", self.preset.name)));
        }
        let mut stmts = statements(self.preset, name, o)?;
        if design::eng(self.preset) == Eng::Ase {
            stmts.push(format!("USE {}", quote_ident(self.quote, &self.database)));
        }
        self.run(move |c, slot| {
            let mut err = None;
            for s in &stmts {
                if let Err(e) = c.stmt(slot).and_then(|st| st.exec(s).map(|_| ())) {
                    err.get_or_insert(e);
                }
            }
            err.map_or(Ok(()), Err)
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

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        assert_eq!(script(p("sybase"), "ven]tas", &o(&[("data_device", " ")])).unwrap(), "USE master\ngo\nCREATE DATABASE [ven]]tas]");
        assert_eq!(script(p("netezza"), "v", &o(&[])).unwrap(), "CREATE DATABASE \"v\"");
        assert!(fields(p("db2")).is_empty() && fields(p("informix")).is_empty() && fields(p("teradata")).is_empty());
    }

    #[test]
    fn ase_devices() {
        assert_eq!(
            script(p("sybase"), "v", &o(&[("data_device", "datadev1"), ("data_size", "500M"), ("log_device", "logdev1"), ("log_size", "100")])).unwrap(),
            "USE master\ngo\nCREATE DATABASE [v]\nON datadev1 = '500M'\nLOG ON logdev1 = 100"
        );
        assert_eq!(script(p("sybase"), "v", &o(&[("data_size", "2 gb")])).unwrap(), "USE master\ngo\nCREATE DATABASE [v]\nON DEFAULT = '2G'");
        assert_eq!(
            script(p("sybase"), "v", &o(&[("data_device", "dev"), ("log_device", "DEV")])).unwrap(),
            "USE master\ngo\nCREATE DATABASE [v]\nON dev\nLOG ON DEV\nWITH OVERRIDE"
        );
    }

    #[test]
    fn netezza_options() {
        assert_eq!(
            script(p("netezza"), "v", &o(&[("collect_history", "OFF"), ("retention_days", "7")])).unwrap(),
            "CREATE DATABASE \"v\"\nCOLLECT HISTORY OFF\nDATA VERSION RETENTION TIME 7"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [("data_device", "dev; drop"), ("data_size", "big"), ("data_size", "1.5G"), ("log_size", "10")] {
            assert!(script(p("sybase"), "v", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(p("netezza"), "v", &o(&[("collect_history", "ON;")])).is_err());
        assert!(script(p("netezza"), "v", &o(&[("retention_days", "100")])).is_err());
        assert!(script(p("odbc"), "v", &o(&[])).is_err());
        assert!(statements(p("odbc"), "v", &o(&[("x", "y")])).is_err());
        assert!(script(p("sybase"), " ", &o(&[])).is_err());
    }
}

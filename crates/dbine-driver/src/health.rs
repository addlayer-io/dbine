//! "Chequeo de salud" of a database ([`crate::Session::health_checks`]):
//! findings by severity, each with what it means, the objects involved and,
//! where there's one, a script that fixes it. DBine only shows the script
//! (it opens in a query); it never runs it by itself.
//!
//! The app adds the checks every engine can answer from what it already
//! reports (connections, long queries, blocking, backups, unused indexes);
//! the driver adds its own (configuration, statistics, bloat, space…).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Checked and fine (shown so the user knows it was looked at).
    #[default]
    Ok,
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthCheck {
    /// Stable id ("auto_shrink", "dead_tuples"…), for the UI's keys.
    pub id: String,
    /// Group in the report ("Configuración", "Rendimiento", "Espacio",
    /// "Backups", "Seguridad"…), in Spanish.
    pub category: String,
    /// One line, in Spanish.
    pub title: String,
    pub severity: Severity,
    /// What it means and what to do, in Spanish.
    #[serde(default)]
    pub detail: String,
    /// The objects it's about ("dbo.Ventas", "idx_x on t"…), at most a few hundred.
    #[serde(default)]
    pub objects: Vec<String>,
    /// A script that fixes it, for the user to review and run.
    #[serde(default)]
    pub fix: Option<String>,
}

impl HealthCheck {
    pub fn new(id: &str, category: &str, title: impl Into<String>, severity: Severity) -> Self {
        Self { id: id.into(), category: category.into(), title: title.into(), severity, ..Default::default() }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = d.into();
        self
    }
    pub fn objects(mut self, o: Vec<String>) -> Self {
        self.objects = o;
        self
    }
    pub fn fix(mut self, f: impl Into<String>) -> Self {
        self.fix = Some(f.into());
        self
    }
}

//! Dremio's scheduler managed through the designer
//! ([`dbine_driver::Session::scheduler_info`] and the rest): what Dremio
//! runs on its own is the refresh of its reflections, so its kinds are
//!
//! - **Reflexiones** (`job`, the ids of [`crate::jobs`]): raw reflections
//!   (display, partition, sort and distribution columns) and aggregation
//!   ones (dimensions with their granularity, measures with their
//!   functions), with the partition transforms and distribution strategy,
//!   enabled or not ([`reflection`]);
//! - **Políticas de refresco de tablas** (`table_policy`): the refresh
//!   policy a physical dataset sets for its reflections instead of its
//!   source's — every period, on a cron schedule, never or when the data
//!   changes; when they expire; full, incremental or automatic refresh
//!   ([`policy`]);
//! - **Políticas de refresco de orígenes** (`source_policy`): the source's
//!   policy, which its tables without one of their own follow.
//!
//! Scripts are SQL where Dremio has it — `ALTER TABLE … CREATE RAW |
//! AGGREGATE REFLECTION`, `ALTER TABLE … DROP REFLECTION` — and otherwise
//! the REST request DBine sends, commented (Dremio Software has no SQL to
//! change a reflection, disable one, or set a refresh policy): changing a
//! reflection is `PUT /api/v3/reflection/{id}` (in place: it keeps its id
//! and its history), a policy is `PUT /api/v3/catalog/{id}` with the
//! entity as it is and the policy changed.
//!
//! Ids are `reflection:<uuid>`, `table:<uuid>` and `source:<uuid>`: a UUID
//! is all that reaches a path or a statement. Names and columns picked in
//! the form are checked against the dataset's; every name is quoted.
//! "Run now" and "stop" are the jobs' ([`crate::jobs`]); there is no history
//! to clear (Dremio drops old materializations itself) and the scheduler's
//! log is the reflections' refreshes (`sys.materializations`).

mod policy;
mod reflection;

use crate::jobs::is_uuid;
use crate::{text, DremioSession};
use dbine_driver::{
    Error, FormValues, MonitorTable, ObjectForm, Result, SchedulerInfo, SchedulerKind,
    SchedulerList,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub(crate) use reflection::exact_script;

/// Datasets a form lists to pick from.
const MAX_DATASETS: usize = 5000;
/// Refreshes the log shows.
const MAX_LOG: usize = 500;

const KINDS: &str = "Dremio tiene reflexiones y políticas de refresco de tablas y de orígenes";
const NO_HISTORY_CLEAR: &str = "Dremio no deja borrar el historial de una reflexión: sus materializaciones viejas las borra él al vencer.";

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Reflection,
    TablePolicy,
    SourcePolicy,
}

impl Kind {
    fn parse(kind: &str) -> Result<Kind> {
        match kind.trim() {
            "job" => Ok(Kind::Reflection),
            "table_policy" => Ok(Kind::TablePolicy),
            "source_policy" => Ok(Kind::SourcePolicy),
            other => Err(Error::Unsupported(format!(
                "«{}» no es un tipo de objeto del planificador de Dremio: {KINDS}",
                shown(other)
            ))),
        }
    }
}

/// `<prefix>:<uuid>` → the UUID, lower case.
fn uuid_id(id: &str, prefix: &str, what: &str) -> Result<String> {
    id.trim()
        .strip_prefix(prefix)
        .and_then(|r| r.strip_prefix(':'))
        .filter(|u| is_uuid(u))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| bad(format!("«{}» no es un id de {what} de Dremio", shown(id))))
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values (or a list row's) as the app sent them: untrusted, any
/// JSON type where a field expects another.
#[derive(Clone, Copy)]
pub(crate) struct Vals<'a>(pub(crate) &'a BTreeMap<String, Value>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    pub(crate) fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(_) | Value::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    pub(crate) fn text(&self, k: &str) -> String {
        self.raw(k).trim().to_string()
    }

    pub(crate) fn opt(&self, k: &str) -> Option<String> {
        Some(self.text(k)).filter(|t| !t.is_empty())
    }

    pub(crate) fn bool_or(&self, k: &str, default: bool) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "" => default,
                _ => false,
            },
            _ => default,
        }
    }

    /// A whole number in `min..=max`; `None` when empty.
    pub(crate) fn int(&self, k: &str, label: &str, min: i64, max: i64) -> Result<Option<i64>> {
        let n = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            Some(_) => None,
        };
        match n {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            Some(n) => Err(bad(format!(
                "«{label}» tiene que estar entre {min} y {max} (es {n})."
            ))),
            None => Err(bad(format!("«{label}» tiene que ser un número entero."))),
        }
    }

    /// A list of names: a multiple pick's array, or text separated by
    /// commas (a new object's fields, typed).
    pub(crate) fn names(&self, k: &str) -> Vec<String> {
        match self.get(k) {
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.trim().to_string(),
                    v => v.to_string(),
                })
                .filter(|s| !s.is_empty())
                .collect(),
            Some(Value::String(s)) => s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// A list's rows from the designer.
pub(crate) fn rows<'a>(values: &'a FormValues, list: &str) -> Vec<Vals<'a>> {
    values
        .lists
        .get(list)
        .map(|rows| rows.iter().map(Vals).collect())
        .unwrap_or_default()
}

/// Text that goes into a name or a literal: one line, no control
/// characters.
pub(crate) fn one_line(v: &str, label: &str) -> Result<String> {
    if v.chars().any(char::is_control) {
        return Err(bad(format!(
            "«{label}» no puede tener saltos de línea ni caracteres de control."
        )));
    }
    if v.len() > 1000 {
        return Err(bad(format!("«{label}» es demasiado largo.")));
    }
    Ok(v.to_string())
}

// ---------------------------------------------------------------------------
// REST requests as script text
// ---------------------------------------------------------------------------

/// Text for a `--` comment line: a name from the catalog or the server
/// (a space, a folder, a table, a reflection) can't end the comment and
/// turn the rest of the line into a statement. Line breaks (Dremio ends a
/// comment at `\r` too, and U+2028/U+2029 for an editor) and other control
/// characters become spaces.
pub(crate) fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// A REST request DBine sends, as script lines: commented, since Dremio's
/// SQL can't say it. `why` heads it.
pub(crate) fn rest_script(why: &str, method: &str, path: &str, body: Option<&Value>) -> String {
    let mut out = String::new();
    for line in why.lines() {
        out.push_str("-- ");
        out.push_str(&comment_text(line));
        out.push('\n');
    }
    out.push_str(&format!("-- {method} {}\n", comment_text(path)));
    if let Some(b) = body {
        let pretty = serde_json::to_string_pretty(b).unwrap_or_default();
        for line in pretty.lines() {
            out.push_str("-- ");
            out.push_str(&comment_text(line));
            out.push('\n');
        }
    }
    out
}

/// A segment of a REST path, percent-encoded.
pub(crate) fn segment(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A dataset path's parts, each quoted, joined with dots.
pub(crate) fn sql_path(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| crate::ddl::q(p))
        .collect::<Vec<_>>()
        .join(".")
}

/// A dataset path as people read it: `space.folder.table`.
pub(crate) fn path_label(parts: &[String]) -> String {
    parts.join(".")
}

/// An entity's `path` array.
pub(crate) fn path_of(entity: &Value) -> Vec<String> {
    entity
        .get("path")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(text).collect())
        .unwrap_or_default()
}

/// A dataset's columns, as the catalog lists them.
pub(crate) fn columns_of(entity: &Value) -> Vec<String> {
    entity
        .get("fields")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|f| f.get("name").map(text))
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The column of the dataset `name` stands for (Dremio's names don't care
/// about case), as the dataset spells it.
pub(crate) fn column(columns: &[String], name: &str, label: &str) -> Result<String> {
    columns
        .iter()
        .find(|c| c.eq_ignore_ascii_case(name.trim()))
        .cloned()
        .ok_or_else(|| {
            bad(format!(
                "«{label}»: la tabla no tiene la columna «{}».",
                shown(name)
            ))
        })
}

/// Milliseconds as the seconds a `Duration` field shows.
pub(crate) fn secs(ms: Option<i64>) -> Value {
    match ms {
        Some(ms) => json!(ms / 1000),
        None => Value::Null,
    }
}

/// A period in ms as people read it ("2 h", "1 d 6 h", "90 s").
pub(crate) fn human(ms: i64) -> String {
    if ms <= 0 {
        return "0".into();
    }
    let s = ms / 1000;
    let (d, h, m, sec) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60, s % 60);
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(format!("{d} d"));
    }
    if h > 0 {
        parts.push(format!("{h} h"));
    }
    if m > 0 {
        parts.push(format!("{m} min"));
    }
    if sec > 0 || parts.is_empty() {
        parts.push(format!("{sec} s"));
    }
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

impl DremioSession {
    /// The catalog entity at `id` (a UUID).
    pub(crate) async fn catalog(&self, id: &str) -> Result<Value> {
        self.conn
            .send(
                reqwest::Method::GET,
                &format!("/api/v3/catalog/{}", segment(id)),
                None,
            )
            .await
    }

    /// The catalog entity at a path.
    pub(crate) async fn catalog_at(&self, parts: &[String]) -> Result<Value> {
        // A `.` or `..` part would walk out of `by-path` to another call.
        if parts.is_empty() || parts.iter().any(|p| p.is_empty() || p == "." || p == "..") {
            return Err(bad(format!(
                "«{}» no es una ruta del catálogo de Dremio.",
                shown(&path_label(parts))
            )));
        }
        let path: Vec<String> = parts.iter().map(|p| segment(p)).collect();
        self.conn
            .send(
                reqwest::Method::GET,
                &format!("/api/v3/catalog/by-path/{}", path.join("/")),
                None,
            )
            .await
    }

    /// Datasets to pick from: `(quoted path, readable path)`, tables only
    /// or views too.
    pub(crate) async fn dataset_choices(&self, views: bool) -> Result<Vec<(String, String)>> {
        let types = if views {
            "('TABLE', 'VIEW')"
        } else {
            "('TABLE')"
        };
        let rows = self
            .records(&format!(
                "SELECT TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.\"TABLES\" WHERE TABLE_TYPE IN {types} AND TABLE_SCHEMA NOT LIKE 'sys%' AND TABLE_SCHEMA <> 'INFORMATION_SCHEMA' ORDER BY TABLE_SCHEMA, TABLE_NAME LIMIT {MAX_DATASETS}"
            ))
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let schema = r.get("TABLE_SCHEMA").map(text)?;
                let name = r.get("TABLE_NAME").map(text)?;
                let mut parts: Vec<String> = schema.split('.').map(str::to_string).collect();
                parts.push(name);
                Some((sql_path(&parts), path_label(&parts)))
            })
            .collect())
    }

    pub(crate) async fn scheduler_info_impl(&mut self) -> Result<SchedulerInfo> {
        let mut notes = Vec::new();
        if let Err(e) = self
            .records("SELECT reflection_id FROM sys.reflections LIMIT 1")
            .await
        {
            notes.push(format!(
                "No se pudo leer sys.reflections (en Dremio Enterprise hace falta el privilegio VIEW REFLECTION): {e}"
            ));
        }
        Ok(SchedulerInfo {
            kinds: vec![
                SchedulerKind::new("job", "Reflexiones", "reflexión").abilities(true, true, true),
                SchedulerKind::new(
                    "table_policy",
                    "Políticas de refresco de tablas",
                    "política de refresco de tabla",
                )
                .abilities(true, true, true),
                SchedulerKind::new(
                    "source_policy",
                    "Políticas de refresco de orígenes",
                    "política de refresco de origen",
                )
                .abilities(false, true, false),
            ],
            logs: true,
            notes,
        })
    }

    pub(crate) async fn scheduler_items_impl(&mut self, kind: &str) -> Result<SchedulerList> {
        match Kind::parse(kind)? {
            Kind::Reflection => reflection::items(self).await,
            Kind::TablePolicy => policy::table_items(self).await,
            Kind::SourcePolicy => policy::source_items(self).await,
        }
    }

    pub(crate) async fn scheduler_form_impl(
        &mut self,
        kind: &str,
        id: Option<&str>,
    ) -> Result<ObjectForm> {
        match Kind::parse(kind)? {
            Kind::Reflection => reflection::form(self, id).await,
            Kind::TablePolicy => policy::table_form(self, id).await,
            Kind::SourcePolicy => {
                let id = id.ok_or_else(|| {
                    Error::Unsupported(
                        "Los orígenes se crean en la conexión de Dremio, no en el planificador: acá se cambia su política de refresco.".into(),
                    )
                })?;
                policy::source_form(self, id).await
            }
        }
    }

    pub(crate) async fn scheduler_script_impl(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<String> {
        Ok(self.scheduler_plan(kind, id, values).await?.script)
    }

    async fn scheduler_plan(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<Plan> {
        match Kind::parse(kind)? {
            Kind::Reflection => reflection::plan(self, id, values).await,
            Kind::TablePolicy => policy::table_plan(self, id, values).await,
            Kind::SourcePolicy => {
                let id = id.ok_or_else(|| {
                    Error::Unsupported("Un origen nuevo se crea en la conexión de Dremio.".into())
                })?;
                policy::source_plan(self, id, values).await
            }
        }
    }

    pub(crate) async fn scheduler_save_impl(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<Option<String>> {
        let plan = self.scheduler_plan(kind, id, values).await?;
        self.run_plan(plan).await
    }

    pub(crate) async fn scheduler_delete_script_impl(
        &mut self,
        kind: &str,
        id: &str,
    ) -> Result<String> {
        Ok(self.delete_plan(kind, id).await?.script)
    }

    async fn delete_plan(&mut self, kind: &str, id: &str) -> Result<Plan> {
        match Kind::parse(kind)? {
            Kind::Reflection => reflection::delete_plan(self, id).await,
            Kind::TablePolicy => policy::table_delete_plan(self, id).await,
            Kind::SourcePolicy => Err(Error::Unsupported(
                "La política de un origen no se borra: se cambia (los orígenes se borran en la conexión de Dremio).".into(),
            )),
        }
    }

    pub(crate) async fn scheduler_delete_impl(&mut self, kind: &str, id: &str) -> Result<()> {
        let plan = self.delete_plan(kind, id).await?;
        self.run_plan(plan).await.map(|_| ())
    }

    pub(crate) async fn clear_history_impl(&mut self, id: &str) -> Result<()> {
        uuid_id(id, "reflection", "reflexión")?;
        Err(Error::Unsupported(NO_HISTORY_CLEAR.into()))
    }

    /// The reflections' refreshes, newest first (`sys.materializations`).
    pub(crate) async fn scheduler_logs_impl(&mut self) -> Result<Vec<MonitorTable>> {
        let rows = self
            .records(&format!(
                "SELECT m.created, m.reflection_id, r.reflection_name, r.dataset_name, m.state, m.last_refresh_duration_millis, m.failure_msg FROM sys.materializations m LEFT JOIN sys.reflections r ON m.reflection_id = r.reflection_id ORDER BY m.created DESC LIMIT {MAX_LOG}"
            ))
            .await?;
        let mut t = MonitorTable::new(
            "reflection_refreshes",
            "Refrescos de reflexiones",
            &[
                "Fecha",
                "Reflexión",
                "Tabla",
                "Estado",
                "Duración (ms)",
                "Error",
            ],
        );
        let s = |r: &Map<String, Value>, k: &str| r.get(k).map(text).unwrap_or_default();
        t.rows = rows
            .iter()
            .map(|r| {
                let name = s(r, "reflection_name");
                let msg = s(r, "failure_msg");
                vec![
                    json!(s(r, "created").chars().take(19).collect::<String>()),
                    json!(if name.is_empty() {
                        s(r, "reflection_id")
                    } else {
                        name
                    }),
                    json!(s(r, "dataset_name")),
                    json!(s(r, "state")),
                    r.get("last_refresh_duration_millis")
                        .cloned()
                        .unwrap_or(Value::Null),
                    json!(if msg == "NONE" {
                        String::new()
                    } else {
                        msg.chars().take(4000).collect()
                    }),
                ]
            })
            .collect();
        Ok(vec![t])
    }

    /// Runs a plan's steps in order: SQL statements as jobs, REST requests
    /// as they are. Returns the object's id.
    async fn run_plan(&mut self, plan: Plan) -> Result<Option<String>> {
        let mut id = plan.id.clone();
        for step in &plan.steps {
            match step {
                Step::Sql(sql) => {
                    self.job(sql).await?;
                }
                Step::Rest { method, path, body } => {
                    let v = self.conn.send(method.clone(), path, body.as_ref()).await?;
                    if id.is_none() {
                        if let Some(new) = v.get("id").map(text).filter(|u| is_uuid(u)) {
                            id = Some(format!("{}:{}", plan.prefix, new.to_ascii_lowercase()));
                        }
                    }
                }
                Step::FindReflection {
                    dataset,
                    before,
                    name,
                } => {
                    if id.is_none() {
                        id = reflection::find_new(self, dataset, before, name).await?;
                    }
                }
            }
        }
        Ok(id)
    }
}

/// One thing a save does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    Sql(String),
    Rest {
        method: reqwest::Method,
        path: String,
        body: Option<Value>,
    },
    /// After a `CREATE … REFLECTION`: the new reflection's id, the one of
    /// the dataset's that wasn't there before.
    FindReflection {
        dataset: String,
        before: Vec<String>,
        name: String,
    },
}

/// What a change writes and runs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub(crate) script: String,
    pub(crate) steps: Vec<Step>,
    /// The object's id when it already has one.
    pub(crate) id: Option<String>,
    /// The id's prefix for one a REST reply gives.
    pub(crate) prefix: &'static str,
}

impl Plan {
    pub(crate) fn nothing(what: &str, id: String, prefix: &'static str) -> Plan {
        Plan {
            script: format!("-- {}: no hay cambios que guardar.\n", comment_text(what)),
            steps: Vec::new(),
            id: Some(id),
            prefix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_kinds() {
        let u = "D215B51C-01F2-49F0-8120-74E508386C42";
        assert_eq!(
            uuid_id(&format!("reflection:{u}"), "reflection", "reflexión").unwrap(),
            u.to_ascii_lowercase()
        );
        for bad in [
            "reflection:x' OR '1'='1",
            "table:d215b51c-01f2-49f0-8120-74e508386c42",
            "reflection:",
            "d215b51c-01f2-49f0-8120-74e508386c42",
        ] {
            assert!(uuid_id(bad, "reflection", "reflexión").is_err(), "{bad}");
        }
        assert!(Kind::parse("schedule").is_err());
        assert_eq!(Kind::parse("table_policy").unwrap(), Kind::TablePolicy);
    }

    #[test]
    fn rest_requests_read_as_comments() {
        let s = rest_script(
            "Dremio no tiene SQL para esto.",
            "PUT",
            "/api/v3/reflection/x",
            Some(&json!({"name": "a\nb"})),
        );
        assert!(s.lines().all(|l| l.starts_with("-- ")), "{s}");
        assert!(s.contains("-- PUT /api/v3/reflection/x\n"));
        // A name with a line break (\r ends a comment too) stays in it.
        let s = rest_script(
            "Reflexión «a\rDROP TABLE t» de s.f\u{2028}x",
            "PUT",
            "/api/v3/reflection/x",
            Some(&json!({"name": "a\u{2028}b"})),
        );
        assert!(
            s.split(['\n', '\r', '\u{2028}', '\u{2029}'])
                .all(|l| l.is_empty() || l.starts_with("-- ")),
            "{s:?}"
        );
        assert!(
            s.starts_with("-- Reflexión «a DROP TABLE t» de s.f x\n"),
            "{s:?}"
        );
        let p = Plan::nothing("Reflexión «a\nDROP TABLE t»", "x".into(), "reflection");
        assert_eq!(
            p.script,
            "-- Reflexión «a DROP TABLE t»: no hay cambios que guardar.\n"
        );
        assert_eq!(segment("$scratch"), "%24scratch");
        assert_eq!(segment("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn values_read_loosely_and_checked() {
        let m = BTreeMap::from([
            ("a".to_string(), json!(["x", " y ", ""])),
            ("b".to_string(), json!("p, q ,,r")),
            ("n".to_string(), json!("12")),
            ("bad".to_string(), json!("1.5")),
        ]);
        let v = Vals(&m);
        assert_eq!(v.names("a"), vec!["x", "y"]);
        assert_eq!(v.names("b"), vec!["p", "q", "r"]);
        assert_eq!(v.int("n", "N", 0, 20).unwrap(), Some(12));
        assert!(v.int("n", "N", 0, 5).is_err());
        assert!(v.int("bad", "N", 0, 5).is_err());
        assert!(one_line("a\nb", "x").is_err());
        let cols = vec!["Id".to_string(), "ts".to_string()];
        assert_eq!(column(&cols, "id", "x").unwrap(), "Id");
        assert!(column(&cols, "nope", "x").is_err());
        assert_eq!(human(7_200_000), "2 h");
        assert_eq!(human(90_000), "1 min 30 s");
        assert_eq!(human(108_000_000), "1 d 6 h");
        assert_eq!(secs(Some(3_600_000)), json!(3600));
    }
}

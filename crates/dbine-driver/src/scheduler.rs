//! A server's scheduler, managed whole: every kind of object the engine's
//! scheduler has (on SQL Server: jobs, shared schedules, operators, alerts,
//! proxies, categories; on Oracle: jobs, programs, schedules, chains,
//! windows, job classes…), listed, created, edited and deleted through a
//! form the driver describes. The app draws the designer from the form,
//! shows the script the driver writes for the change and runs it after the
//! user's yes. Jobs keep their richer list and runs in [`crate::jobs`].
//!
//! Ids and every value in a form come back from the app as untrusted text:
//! drivers bind or escape them, check them against what the server has, and
//! refuse what they didn't produce.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// One kind of object the server's scheduler has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchedulerKind {
    /// Stable key: "job", "schedule", "operator", "alert", "proxy",
    /// "category", "program", "chain", "window", "job_class"…
    pub key: String,
    /// Plural label for the tree, in Spanish ("Jobs", "Operadores").
    pub label: String,
    /// Singular label for "Nuevo …" and confirmations ("job", "operador").
    pub singular: String,
    #[serde(default)]
    pub can_create: bool,
    #[serde(default)]
    pub can_edit: bool,
    #[serde(default)]
    pub can_delete: bool,
}

/// One object of a kind, as the tree lists it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchedulerItem {
    /// The engine's id for it, opaque to the app.
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `None`: the kind has no enabled state.
    pub enabled: Option<bool>,
    /// It comes with the engine (system category, built-in schedule…).
    #[serde(default)]
    pub system: bool,
}

/// The objects of a kind, with what the user should know about the list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchedulerList {
    pub items: Vec<SchedulerItem>,
    /// In Spanish (the UI translates them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// How a field is edited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FormKind {
    Text,
    Textarea,
    Number {
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
    },
    Bool,
    /// One of these `(value, Spanish label)` pairs.
    Select {
        options: Vec<(String, String)>,
    },
    /// Any of these `(value, Spanish label)` pairs; the value is a list.
    MultiSelect {
        options: Vec<(String, String)>,
    },
    /// One (or, with `multiple`, several) of the server's objects or names,
    /// listed in [`ObjectForm::choices`] under `source` ("operator",
    /// "proxy:TSQL", "database", "login", "category:job"…).
    Ref {
        source: String,
        #[serde(default)]
        multiple: bool,
    },
    /// Code in the editor: "sql", "plsql", "powershell", "shell", "python",
    /// "json"…
    Code {
        language: String,
    },
    /// `YYYY-MM-DD`.
    Date,
    /// `HH:MM:SS`.
    Time,
    /// RFC 3339, local time as the server takes it.
    DateTime,
    /// Seconds.
    Duration,
    /// A cron expression, as the engine reads it.
    Cron,
}

/// One field of a form or of a list's rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub key: String,
    /// In Spanish.
    pub label: String,
    pub kind: FormKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Shown, not editable (a value the server sets, a permission missing).
    #[serde(default)]
    pub read_only: bool,
    /// Shown only while another field of the same form or row has one of
    /// these values (the schedule's frequency fields, a step type's fields).
    #[serde(default)]
    pub when: Option<(String, Vec<String>)>,
    /// The value of a new object or row.
    #[serde(default)]
    pub default: Option<Value>,
}

impl FormField {
    pub fn new(key: &str, label: &str, kind: FormKind) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind,
            required: false,
            help: None,
            placeholder: None,
            read_only: false,
            when: None,
            default: None,
        }
    }
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
    pub fn help(mut self, help: &str) -> Self {
        self.help = Some(help.into());
        self
    }
    pub fn placeholder(mut self, p: &str) -> Self {
        self.placeholder = Some(p.into());
        self
    }
    pub fn read_only(mut self, r: bool) -> Self {
        self.read_only = r;
        self
    }
    pub fn when(mut self, field: &str, values: &[&str]) -> Self {
        self.when = Some((field.into(), values.iter().map(|v| v.to_string()).collect()));
        self
    }
    pub fn default(mut self, v: Value) -> Self {
        self.default = Some(v);
        self
    }
}

/// A list inside a form: a job's steps, its schedules, an alert's
/// notifications. Each row is edited with `fields`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FormList {
    pub key: String,
    /// In Spanish.
    pub label: String,
    pub fields: Vec<FormField>,
    /// Field key → value, one map per row.
    #[serde(default)]
    pub rows: Vec<BTreeMap<String, Value>>,
    /// The order of the rows means something (steps run in it).
    #[serde(default)]
    pub orderable: bool,
    #[serde(default)]
    pub min_rows: u32,
    /// The field that names a row in the list's table.
    pub title_field: String,
    /// Rows can't be added or removed (shown only).
    #[serde(default)]
    pub read_only: bool,
}

/// A tab of the designer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FormSection {
    pub key: String,
    /// In Spanish.
    pub label: String,
    #[serde(default)]
    pub fields: Vec<FormField>,
    #[serde(default)]
    pub lists: Vec<FormList>,
}

/// What the designer edits: the form, its current values (a new object's
/// defaults), the server's objects the `Ref` fields pick from, and notes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectForm {
    pub sections: Vec<FormSection>,
    /// Top-level field key → value. Lists carry their rows themselves.
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
    /// `Ref` source → `(value, label)` pairs.
    #[serde(default)]
    pub choices: BTreeMap<String, Vec<(String, String)>>,
    /// In Spanish: why something can't be changed, version limits…
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Nothing in it can be saved (permissions, a system object).
    #[serde(default)]
    pub read_only: bool,
}

/// What the designer sends back: top-level values and each list's rows (in
/// their new order). A row keeps the fields it was given, ids included, so
/// the driver tells an edited row from a new one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FormValues {
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
    #[serde(default)]
    pub lists: BTreeMap<String, Vec<BTreeMap<String, Value>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_form_survives_json() {
        let form = ObjectForm {
            sections: vec![FormSection {
                key: "general".into(),
                label: "General".into(),
                fields: vec![
                    FormField::new("name", "Nombre", FormKind::Text).required(),
                    FormField::new(
                        "freq",
                        "Frecuencia",
                        FormKind::Select {
                            options: vec![("daily".into(), "Diaria".into())],
                        },
                    ),
                    FormField::new(
                        "every",
                        "Cada",
                        FormKind::Number {
                            min: Some(1.0),
                            max: None,
                        },
                    )
                    .when("freq", &["daily"]),
                    FormField::new(
                        "owner",
                        "Propietario",
                        FormKind::Ref {
                            source: "login".into(),
                            multiple: false,
                        },
                    ),
                ],
                lists: vec![FormList {
                    key: "steps".into(),
                    label: "Pasos".into(),
                    fields: vec![FormField::new(
                        "command",
                        "Comando",
                        FormKind::Code {
                            language: "sql".into(),
                        },
                    )],
                    rows: vec![BTreeMap::from([("command".into(), json!("SELECT 1"))])],
                    orderable: true,
                    min_rows: 0,
                    title_field: "name".into(),
                    read_only: false,
                }],
            }],
            values: BTreeMap::from([("name".into(), json!("j"))]),
            choices: BTreeMap::from([("login".into(), vec![("sa".into(), "sa".into())])]),
            notes: vec![],
            read_only: false,
        };
        let v = serde_json::to_value(&form).unwrap();
        assert_eq!(v["sections"][0]["fields"][1]["kind"]["type"], "select");
        assert_eq!(v["sections"][0]["fields"][2]["when"][0], "freq");
        let back: ObjectForm = serde_json::from_value(v).unwrap();
        assert_eq!(back, form);
    }

    #[test]
    fn values_and_kinds_read_with_their_defaults() {
        let k: SchedulerKind =
            serde_json::from_value(json!({"key": "job", "label": "Jobs", "singular": "job"}))
                .unwrap();
        assert!(!k.can_create && !k.can_edit && !k.can_delete);
        let v: FormValues = serde_json::from_value(json!({})).unwrap();
        assert!(v.values.is_empty() && v.lists.is_empty());
    }
}

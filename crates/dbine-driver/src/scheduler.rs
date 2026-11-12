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

impl SchedulerKind {
    /// `label` and `singular` in Spanish. Write them as literals in the call:
    /// scripts/i18n/extract_backend.py finds them by this constructor's name.
    pub fn new(key: &str, label: &str, singular: &str) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            singular: singular.into(),
            ..Self::default()
        }
    }
    /// What the user may do with this kind's objects.
    pub fn abilities(mut self, create: bool, edit: bool, delete: bool) -> Self {
        self.can_create = create;
        self.can_edit = edit;
        self.can_delete = delete;
        self
    }
}

/// What the server's scheduler has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchedulerInfo {
    /// Its kinds of object, in the order the tree shows them (jobs first).
    pub kinds: Vec<SchedulerKind>,
    /// It keeps a log of its own that [`crate::Session::scheduler_logs`]
    /// reads (SQL Server Agent's error log).
    #[serde(default)]
    pub logs: bool,
    /// In Spanish: the scheduler stopped, an edition without one, what the
    /// login can't change…
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
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
    ///
    /// A source `list:<key>` picks a row of the same form's list `<key>` as
    /// it is being edited (a job's start step, "go to step"): the app
    /// offers the list's rows and the value is the row's `title_field`
    /// value (the step's name), which the driver resolves when it writes
    /// the script. Such a source has no entry in `choices`.
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
    /// A password or token. The form never carries its current value (it
    /// comes as `null`); the app sends a value only when the user typed
    /// one, and `null` means "keep what the server has". The script that
    /// [`crate::Session::scheduler_script`] returns shows it masked
    /// ([`SECRET_MASK`]); only `scheduler_save` writes the real value, and
    /// it never reaches a log.
    Secret,
}

/// What a script shows in place of a secret value.
pub const SECRET_MASK: &str = "********";

impl FormKind {
    /// One of these `(value, Spanish label)` pairs. Write the pairs as
    /// literals in the call (the extractor finds the labels there).
    pub fn select(options: &[(&str, &str)]) -> Self {
        Self::Select {
            options: pairs(options),
        }
    }
    /// Any of these `(value, Spanish label)` pairs.
    pub fn multi(options: &[(&str, &str)]) -> Self {
        Self::MultiSelect {
            options: pairs(options),
        }
    }
    pub fn number(min: Option<f64>, max: Option<f64>) -> Self {
        Self::Number { min, max }
    }
    pub fn reference(source: &str, multiple: bool) -> Self {
        Self::Ref {
            source: source.into(),
            multiple,
        }
    }
    pub fn code(language: &str) -> Self {
        Self::Code {
            language: language.into(),
        }
    }
}

fn pairs(options: &[(&str, &str)]) -> Vec<(String, String)> {
    options
        .iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect()
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
    /// `label` in Spanish, written as a literal in the call (the extractor
    /// finds it by this constructor's name). `help` is a sentence; a
    /// placeholder is an example, kept as written.
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

impl FormList {
    /// `label` in Spanish, written as a literal in the call.
    pub fn new(key: &str, label: &str, title_field: &str, fields: Vec<FormField>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            fields,
            title_field: title_field.into(),
            ..Self::default()
        }
    }
    pub fn rows(mut self, rows: Vec<BTreeMap<String, Value>>) -> Self {
        self.rows = rows;
        self
    }
    pub fn orderable(mut self) -> Self {
        self.orderable = true;
        self
    }
    pub fn min_rows(mut self, n: u32) -> Self {
        self.min_rows = n;
        self
    }
    pub fn read_only(mut self, r: bool) -> Self {
        self.read_only = r;
        self
    }
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

impl FormSection {
    /// `label` in Spanish, written as a literal in the call.
    pub fn new(key: &str, label: &str) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            ..Self::default()
        }
    }
    pub fn field(mut self, f: FormField) -> Self {
        self.fields.push(f);
        self
    }
    pub fn fields(mut self, f: Vec<FormField>) -> Self {
        self.fields.extend(f);
        self
    }
    pub fn list(mut self, l: FormList) -> Self {
        self.lists.push(l);
        self
    }
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

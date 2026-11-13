//! Categories of jobs, alerts and operators: `sp_add_category` (class and
//! type), `sp_update_category` (a new name; class and type stay) and
//! `sp_delete_category` (its members move to the class's
//! "[Uncategorized]"). The system's own categories (id under 100) are
//! read-only. sysadmin only.

use super::{bad, fits, int_id, Ctx, Kind, Plan, Script, Vals, A, SYSADMIN_ONLY_FORM, SYSNAME};
use crate::jobs::{self, first_with, int, text};
use crate::SqlServerSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use serde_json::json;
use std::collections::BTreeMap;

const NOT_FOUND: &str = "La categoría no existe.";
const SYSTEM: &str = "Es una categoría del sistema: no se puede cambiar ni borrar.";

/// A category as msdb keeps it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Category {
    pub(crate) id: i64,
    /// 1 job, 2 alert, 3 operator.
    pub(crate) class: i64,
    /// 1 local, 2 multiserver, 3 none.
    pub(crate) kind: i64,
    pub(crate) name: String,
}

impl Category {
    /// msdb's own (`[Uncategorized (Local)]`, "Database Maintenance"…).
    pub(crate) fn system(&self) -> bool {
        self.id < 100
    }
}

fn class_key(class: i64) -> &'static str {
    match class {
        2 => "alert",
        3 => "operator",
        _ => "job",
    }
}

/// `@class` as msdb's procedures take it.
fn class_arg(class: i64) -> &'static str {
    match class {
        2 => "N'ALERT'",
        3 => "N'OPERATOR'",
        _ => "N'JOB'",
    }
}

fn type_arg(kind: i64) -> &'static str {
    match kind {
        2 => "N'MULTI-SERVER'",
        3 => "N'NONE'",
        _ => "N'LOCAL'",
    }
}

const LIST_SQL: &str = "SET NOCOUNT ON;
SELECT category_id, category_class, category_type, name FROM msdb.dbo.syscategories ORDER BY category_class, name;";

/// The category and its members (`@P1` its id), each list where the login
/// may read it.
const ONE_SQL: &str = "SET NOCOUNT ON;
SELECT category_id, category_class, category_type, name FROM msdb.dbo.syscategories WHERE category_id = @P1;
SELECT name AS dbine_member FROM msdb.dbo.sysjobs_view WHERE category_id = @P1;
IF HAS_PERMS_BY_NAME(N'msdb.dbo.sysalerts', N'OBJECT', N'SELECT') = 1
    SELECT name AS dbine_alert FROM msdb.dbo.sysalerts WHERE category_id = @P1;
IF HAS_PERMS_BY_NAME(N'msdb.dbo.sysoperators', N'OBJECT', N'SELECT') = 1
    SELECT name AS dbine_operator FROM msdb.dbo.sysoperators WHERE category_id = @P1;";

fn row_of(r: &tiberius::Row) -> Option<Category> {
    Some(Category {
        id: int(r, "category_id")?,
        class: int(r, "category_class").unwrap_or(1),
        kind: int(r, "category_type").unwrap_or(1),
        name: text(r, "name")?,
    })
}

async fn read(s: &mut SqlServerSession, id: &str) -> Result<(Category, Vec<String>)> {
    let n = int_id(Kind::Category, id)?;
    let ids = n.to_string();
    let sets = jobs::sets(s, ONE_SQL, &[&ids], None).await?;
    let Some(cat) = first_with(&sets, "category_class")
        .and_then(|s| s.rows.first())
        .and_then(row_of)
    else {
        return Err(Error::Query(NOT_FOUND.into()));
    };
    let column = match cat.class {
        2 => "dbine_alert",
        3 => "dbine_operator",
        _ => "dbine_member",
    };
    let members = first_with(&sets, column)
        .map(|s| s.rows.iter().filter_map(|r| text(r, column)).collect())
        .unwrap_or_default();
    Ok((cat, members))
}

/// What a category is, for the list.
fn describe(c: &Category) -> &'static str {
    match (c.class, c.kind) {
        (1, 2) => "Jobs (multiservidor)",
        (1, _) => "Jobs (local)",
        (2, _) => "Alertas",
        _ => "Operadores",
    }
}

/// A category in the tree: the system's own can't be changed or deleted.
pub(crate) fn item(ctx: &Ctx, c: &Category) -> SchedulerItem {
    let own = super::unless(ctx.admin(), !c.system());
    SchedulerItem {
        id: c.id.to_string(),
        name: c.name.clone(),
        description: Some(describe(c).to_string()),
        enabled: None,
        system: c.system(),
        can_edit: own,
        can_delete: own,
        actions: None,
    }
}

pub(crate) async fn items(s: &mut SqlServerSession, ctx: &Ctx) -> Result<SchedulerList> {
    let sets = jobs::sets(s, LIST_SQL, &[], None).await?;
    let items = first_with(&sets, "category_class")
        .map(|set| {
            set.rows
                .iter()
                .filter_map(row_of)
                .map(|c| item(ctx, &c))
                .collect()
        })
        .unwrap_or_default();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

pub(crate) async fn form(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let mut f = ObjectForm::default();
    let (cat, members) = match id {
        None => (
            Category {
                id: 0,
                class: 1,
                kind: 1,
                name: String::new(),
            },
            None,
        ),
        Some(id) => {
            let (cat, members) = read(s, id).await?;
            (cat, Some(members))
        }
    };
    let existing = members.is_some();
    f.values.insert("name".into(), json!(cat.name));
    f.values.insert("class".into(), json!(class_key(cat.class)));
    f.values.insert(
        "type".into(),
        json!(if cat.kind == 2 { "multi" } else { "local" }),
    );
    let mut sections = vec![FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new(
            "class",
            "Clase",
            FormKind::select(&[
                ("job", "Jobs"),
                ("alert", "Alertas"),
                ("operator", "Operadores"),
            ]),
        )
        .read_only(existing)
        .default(json!("job")),
        FormField::new(
            "type",
            "Tipo",
            FormKind::select(&[("local", "Local"), ("multi", "Multiservidor")]),
        )
        .when("class", &["job"])
        .read_only(existing)
        .default(json!("local")),
    ])];
    if let Some(members) = members {
        sections.push(
            FormSection::new("members", "Miembros").list(
                FormList::new(
                    "members",
                    "Miembros",
                    "name",
                    vec![FormField::new("name", "Nombre", FormKind::Text).read_only(true)],
                )
                .rows(
                    members
                        .iter()
                        .map(|m| BTreeMap::from([("name".to_string(), json!(m))]))
                        .collect(),
                )
                .read_only(true),
            ),
        );
    }
    f.sections = sections;
    if cat.system() && existing {
        f.read_only = true;
        f.notes.push(SYSTEM.into());
    } else if !c.admin() {
        f.read_only = true;
        if c.writable() {
            f.notes.push(SYSADMIN_ONLY_FORM.into());
        }
    }
    Ok(f)
}

pub(crate) fn create_script(class: i64, kind: i64, name: &str) -> String {
    let mut sc = Script::new(&format!("Categoría «{name}» del Agente SQL Server"), &[]);
    sc.call(
        "msdb.dbo.sp_add_category",
        vec![
            ("class", A::R(class_arg(class).into())),
            ("type", A::R(type_arg(kind).into())),
            ("name", A::T(name.into())),
        ],
    );
    sc.finish(None)
}

pub(crate) fn rename_script(cat: &Category, name: &str) -> Option<String> {
    if name == cat.name {
        return None;
    }
    let mut sc = Script::new(
        &format!("Categoría «{}» del Agente SQL Server", cat.name),
        &[],
    );
    sc.call(
        "msdb.dbo.sp_update_category",
        vec![
            ("class", A::R(class_arg(cat.class).into())),
            ("name", A::T(cat.name.clone())),
            ("new_name", A::T(name.into())),
        ],
    );
    Some(sc.finish(None))
}

pub(crate) fn drop_script(cat: &Category) -> String {
    let mut sc = Script::new(
        &format!("Borra la categoría «{}» del Agente SQL Server", cat.name),
        &[],
    );
    sc.call(
        "msdb.dbo.sp_delete_category",
        vec![
            ("class", A::R(class_arg(cat.class).into())),
            ("name", A::T(cat.name.clone())),
        ],
    );
    sc.finish(None)
}

pub(crate) async fn plan(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    if !c.p.sysadmin {
        return Err(Error::Query(SYSADMIN_ONLY_FORM.into()));
    }
    let v = Vals(&values.values);
    let name = v
        .opt("name")
        .ok_or_else(|| bad("La categoría necesita un nombre.".into()))?;
    fits("Nombre", &name, SYSNAME)?;
    match id {
        None => {
            let class = match v.text("class").as_str() {
                "" | "job" => 1,
                "alert" => 2,
                "operator" => 3,
                other => {
                    return Err(bad(format!(
                        "«{}» no es una clase de categoría (jobs, alertas u operadores).",
                        super::shown(other)
                    )))
                }
            };
            let kind = match (class, v.text("type").as_str()) {
                (1, "multi") => 2,
                (1, _) => 1,
                _ => 3,
            };
            Ok(Plan {
                script: create_script(class, kind, &name),
                empty: false,
                id: None,
                output: None,
                lookup: Some((
                    match class {
                        2 => "SELECT CAST(category_id AS nvarchar(20)) AS id FROM msdb.dbo.syscategories WHERE category_class = 2 AND name = @P1",
                        3 => "SELECT CAST(category_id AS nvarchar(20)) AS id FROM msdb.dbo.syscategories WHERE category_class = 3 AND name = @P1",
                        _ => "SELECT CAST(category_id AS nvarchar(20)) AS id FROM msdb.dbo.syscategories WHERE category_class = 1 AND name = @P1",
                    },
                    name,
                )),
            })
        }
        Some(id) => {
            let (cat, _) = read(s, id).await?;
            if cat.system() {
                return Err(Error::Query(SYSTEM.into()));
            }
            Ok(match rename_script(&cat, &name) {
                Some(script) => Plan {
                    script,
                    empty: false,
                    id: Some(cat.id.to_string()),
                    output: None,
                    lookup: None,
                },
                None => Plan::nothing(&cat.name, cat.id.to_string()),
            })
        }
    }
}

pub(crate) async fn delete_script(s: &mut SqlServerSession, c: &Ctx, id: &str) -> Result<String> {
    let (cat, _) = read(s, id).await?;
    if cat.system() {
        return Err(Error::Query(SYSTEM.into()));
    }
    if !c.p.sysadmin && c.writable() {
        return Err(Error::Query(SYSADMIN_ONLY_FORM.into()));
    }
    Ok(drop_script(&cat))
}

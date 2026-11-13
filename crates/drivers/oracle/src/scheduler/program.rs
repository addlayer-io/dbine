//! Programs: `CREATE_PROGRAM`, their arguments (`DEFINE_PROGRAM_ARGUMENT`,
//! `DEFINE_METADATA_ARGUMENT`, `DROP_PROGRAM_ARGUMENT`), `SET_ATTRIBUTE`
//! for the rest, `DROP_PROGRAM`.

use super::choices::{self, Catalog};
use super::job::{action_fields, EXTERNAL, WITH_ARGS};
use super::read::{self, Rec};
use super::script::name_lit;
use super::{
    bad, fingerprint, fits, fresh, new_name, new_owner, obj_id, only, parse_obj, rows, shown,
    Block, Ctx, Kind, Plan, Vals, A,
};
use dbine_driver::{
    FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
    SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

const TYPES: [&str; 6] = [
    "PLSQL_BLOCK",
    "STORED_PROCEDURE",
    "EXECUTABLE",
    "EXTERNAL_SCRIPT",
    "SQL_SCRIPT",
    "BACKUP_SCRIPT",
];
/// What a metadata argument hands the program; `VALUE`: a plain argument.
const METADATA: [&str; 9] = [
    "VALUE",
    "JOB_NAME",
    "JOB_OWNER",
    "JOB_START",
    "WINDOW_START",
    "WINDOW_END",
    "JOB_SUBNAME",
    "JOB_SCHEDULED_START",
    "EVENT_MESSAGE",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgArg {
    pub(crate) name: Option<String>,
    /// `VALUE` or a metadata attribute.
    pub(crate) metadata: String,
    pub(crate) arg_type: Option<String>,
    pub(crate) default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgramSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) ptype: String,
    pub(crate) action: String,
    pub(crate) args: Vec<ProgArg>,
    pub(crate) enabled: bool,
    pub(crate) detached: bool,
    pub(crate) comments: Option<String>,
}

impl ProgramSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
}

fn action_key(t: &str) -> &'static str {
    match t {
        "STORED_PROCEDURE" => "procedure",
        "EXECUTABLE" => "executable",
        "EXTERNAL_SCRIPT" => "external_script",
        "SQL_SCRIPT" => "sql_script",
        "BACKUP_SCRIPT" => "backup_script",
        _ => "plsql",
    }
}

pub(crate) fn spec_of_rec(r: &Rec, args: &[Rec]) -> ProgramSpec {
    ProgramSpec {
        owner: r.t("OWNER").unwrap_or_default(),
        name: r.raw("PROGRAM_NAME").unwrap_or_default(),
        ptype: r.t("PROGRAM_TYPE").unwrap_or_else(|| "PLSQL_BLOCK".into()),
        action: r
            .raw("PROGRAM_ACTION")
            .unwrap_or_default()
            .trim_end()
            .to_string(),
        args: args
            .iter()
            .map(|a| ProgArg {
                name: a.t("ARGUMENT_NAME"),
                metadata: a.t("METADATA_ATTRIBUTE").unwrap_or_else(|| "VALUE".into()),
                arg_type: a
                    .t("ARGUMENT_TYPE")
                    .filter(|_| a.t("METADATA_ATTRIBUTE").is_none()),
                // DEFAULT_VALUE says `<NO VALUE>` for an argument without
                // one: the default is whether the ANYDATA value is set.
                default: a.raw("DEFAULT_VALUE").filter(|_| {
                    a.t("METADATA_ATTRIBUTE").is_none() && a.yes("DEFAULT_ANYDATA_VALUE")
                }),
            })
            .collect(),
        enabled: r.yes("ENABLED"),
        detached: r.yes("DETACHED"),
        comments: r.t("COMMENTS"),
    }
}

fn read_state(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<ProgramSpec> {
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_programs"),
        "WHERE owner = :1 AND program_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe el programa «{}» o este usuario no lo puede ver",
            obj_id(Some(owner), name)
        ))
    })?;
    let args = read::rows(
        c,
        &ctx.sql("{v}_scheduler_program_args"),
        "WHERE owner = :1 AND program_name = :2 ORDER BY argument_position",
        &[&owner, &name],
        1000,
    )
    .map_err(crate::err)?;
    Ok(spec_of_rec(&r, &args))
}

type Rows = Vec<BTreeMap<String, Value>>;

pub(crate) fn values_of(s: &ProgramSpec) -> (BTreeMap<String, Value>, BTreeMap<String, Rows>) {
    let mut v = BTreeMap::new();
    v.insert("owner".into(), json!(s.owner));
    v.insert("name".into(), json!(s.name));
    v.insert("program_type".into(), json!(s.ptype));
    v.insert(action_key(&s.ptype).into(), json!(s.action));
    v.insert("enabled".into(), json!(s.enabled));
    v.insert("detached".into(), json!(s.detached));
    v.insert(
        "comments".into(),
        json!(s.comments.clone().unwrap_or_default()),
    );
    let rows = s
        .args
        .iter()
        .map(|a| {
            BTreeMap::from([
                (
                    "name".to_string(),
                    json!(a.name.clone().unwrap_or_default()),
                ),
                ("metadata".to_string(), json!(a.metadata)),
                (
                    "arg_type".to_string(),
                    json!(a.arg_type.clone().unwrap_or_default()),
                ),
                (
                    "default".to_string(),
                    json!(a.default.clone().unwrap_or_default()),
                ),
            ])
        })
        .collect();
    (v, BTreeMap::from([("arguments".to_string(), rows)]))
}

pub(crate) fn spec_of(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&ProgramSpec>,
) -> Result<ProgramSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::Program, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let ptype = v.choice("program_type", "Tipo", &TYPES, "PLSQL_BLOCK")?;
    if ctx.cloud && EXTERNAL.contains(&ptype.as_str()) {
        return Err(bad(format!(
            "Oracle Autonomous Database no ejecuta programas de tipo {ptype}."
        )));
    }
    let key = action_key(&ptype);
    let action = if matches!(ptype.as_str(), "STORED_PROCEDURE" | "EXECUTABLE") {
        v.text(key)
    } else {
        v.code(key).unwrap_or_default()
    };
    if action.trim().is_empty() {
        return Err(bad("Falta la acción del programa: qué ejecuta.".into()));
    }
    fits(&Some(action.clone()), "Acción", 4000)?;
    let mut args = Vec::new();
    // The form hides the tab for a type that takes none: rows it still
    // holds don't apply (nor are they checked).
    let arg_rows = if WITH_ARGS.contains(&ptype.as_str()) {
        rows(values, "arguments")
    } else {
        Vec::new()
    };
    for (i, r) in arg_rows.into_iter().enumerate() {
        let metadata = r.choice("metadata", "Tipo de argumento", &METADATA, "VALUE")?;
        let arg_type = r.opt("arg_type").map(|t| t.to_ascii_uppercase());
        if metadata == "VALUE" && arg_type.is_none() {
            return Err(bad(format!(
                "Argumento {}: falta su tipo de dato (VARCHAR2, NUMBER, DATE…).",
                i + 1
            )));
        }
        if let Some(t) = &arg_type {
            if t.len() > 128 || t.chars().any(|c| c.is_control() || c == '\'') {
                return Err(bad(format!(
                    "Argumento {}: «{}» no es un tipo de dato.",
                    i + 1,
                    shown(t)
                )));
            }
        }
        let default = Some(r.raw("default")).filter(|d| !d.is_empty());
        fits(&default, "Valor predeterminado", 4000)?;
        args.push(ProgArg {
            name: r.opt("name").map(|n| {
                if n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    n.to_ascii_uppercase()
                } else {
                    n
                }
            }),
            arg_type: arg_type.filter(|_| metadata == "VALUE"),
            default: default.filter(|_| metadata == "VALUE"),
            metadata,
        });
    }
    if args.len() > 255 {
        return Err(bad("Un programa recibe a lo sumo 255 argumentos.".into()));
    }
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(ProgramSpec {
        owner,
        name,
        ptype,
        action,
        args,
        enabled: v.bool_or("enabled", true),
        detached: v.bool("detached"),
        comments,
    })
}

fn define_arg(b: &mut Block, target: &A, i: usize, a: &ProgArg) {
    let pos = A::I(i as i64 + 1);
    if a.metadata == "VALUE" {
        let mut args = vec![
            ("program_name", target.clone()),
            ("argument_position", pos),
            ("argument_name", A::opt(a.name.clone())),
            (
                "argument_type",
                A::S(a.arg_type.clone().unwrap_or_default()),
            ),
        ];
        if let Some(d) = &a.default {
            args.push(("default_value", A::S(d.clone())));
        }
        b.call("DBMS_SCHEDULER.DEFINE_PROGRAM_ARGUMENT", args);
    } else {
        b.call(
            "DBMS_SCHEDULER.DEFINE_METADATA_ARGUMENT",
            vec![
                ("program_name", target.clone()),
                ("metadata_attribute", A::S(a.metadata.clone())),
                ("argument_position", pos),
                ("argument_name", A::opt(a.name.clone())),
            ],
        );
    }
}

pub(crate) fn create_block(s: &ProgramSpec) -> Block {
    let id = s.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Crea el programa {id}."));
    let mut args = vec![
        ("program_name", target.clone()),
        ("program_type", A::S(s.ptype.clone())),
        ("program_action", A::S(s.action.clone())),
    ];
    if !s.args.is_empty() {
        args.push(("number_of_arguments", A::I(s.args.len() as i64)));
    }
    args.push(("enabled", A::B(false)));
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    b.call("DBMS_SCHEDULER.CREATE_PROGRAM", args);
    b.created(&format!(
        "DBMS_SCHEDULER.DROP_PROGRAM(program_name => {}, force => TRUE);",
        name_lit(&id)
    ));
    for (i, a) in s.args.iter().enumerate() {
        define_arg(&mut b, &target, i, a);
    }
    if s.detached {
        b.set_attr(&target, "detached", A::B(true));
    }
    if s.enabled {
        b.pcall("DBMS_SCHEDULER.ENABLE", vec![target]);
    }
    b
}

pub(crate) fn alter_block(old: &ProgramSpec, new: &ProgramSpec) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia el programa {id}."));
    let structural = old.ptype != new.ptype || old.action != new.action || old.args != new.args;
    let disabled = old.enabled && structural;
    if disabled {
        let t = target.clone();
        let disable = |b: &mut Block| {
            b.call(
                "DBMS_SCHEDULER.DISABLE",
                vec![("name", t), ("force", A::B(true))],
            )
        };
        if new.enabled {
            // Enabled again at the end: a failure in between leaves it
            // disabled, and the error says so.
            b.disabled_first(&id, disable);
        } else {
            // It ends disabled, as asked, whatever fails after.
            disable(&mut b);
        }
    }
    if old.ptype != new.ptype {
        b.set_attr(&target, "program_type", A::S(new.ptype.clone()));
    }
    if old.action != new.action || old.ptype != new.ptype {
        b.set_attr(&target, "program_action", A::S(new.action.clone()));
    }
    if old.args != new.args {
        // Those past the new count go; the rest are defined again (a
        // definition replaces the one at its position).
        for i in (new.args.len()..old.args.len()).rev() {
            b.call(
                "DBMS_SCHEDULER.DROP_PROGRAM_ARGUMENT",
                vec![
                    ("program_name", target.clone()),
                    ("argument_position", A::I(i as i64 + 1)),
                ],
            );
        }
        if old.args.len() != new.args.len() {
            b.set_attr(&target, "number_of_arguments", A::I(new.args.len() as i64));
        }
        for (i, a) in new.args.iter().enumerate() {
            if old.args.get(i) != Some(a) {
                define_arg(&mut b, &target, i, a);
            }
        }
    }
    if old.detached != new.detached {
        b.set_attr(&target, "detached", A::B(new.detached));
    }
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    if new.enabled && (disabled || !old.enabled) {
        let t = target.clone();
        b.enabled_again(|b| b.pcall("DBMS_SCHEDULER.ENABLE", vec![t]));
    } else if !new.enabled && old.enabled && !disabled {
        b.call(
            "DBMS_SCHEDULER.DISABLE",
            vec![("name", target), ("force", A::B(true))],
        );
    }
    b
}

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_programs"),
        "ORDER BY owner, program_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    // "Ejecutar ahora", where the kind offers it: what each one allows.
    let runs = !super::action::of(ctx, Kind::Program).is_empty();
    let (granted, wanting) = if runs {
        (
            super::executable(c, ctx, "{v}_scheduler_programs", "program_name"),
            wanting_args(c, ctx),
        )
    } else {
        Default::default()
    };
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("PROGRAM_NAME").unwrap_or_default();
                let system = ctx.oracle_schema(&owner);
                let locked = ctx.locked(Kind::Program, &owner, &name, system).is_some();
                let run = !system
                    && r.yes("ENABLED")
                    && RUNNABLE.contains(&r.t("PROGRAM_TYPE").unwrap_or_default().as_str())
                    && !wanting.contains(&(owner.clone(), name.clone()))
                    && ctx.may_execute(&owner, &name, &granted);
                SchedulerItem {
                    id: obj_id(Some(&owner), &name),
                    name: super::item_name(ctx, &owner, &name),
                    description: r.t("COMMENTS"),
                    enabled: Some(r.yes("ENABLED")),
                    system,
                    can_edit: super::unless(locked),
                    can_delete: super::unless(locked),
                    actions: runs.then(|| {
                        let mut keys = Vec::new();
                        if run {
                            keys.push(super::action::RUN.to_string());
                        }
                        keys
                    }),
                    run_from_step: None,
                    clear_history: None,
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

/// The program types a run of its own can start: the ones that need no
/// credential, destination or connection.
pub(crate) const RUNNABLE: [&str; 2] = ["PLSQL_BLOCK", "STORED_PROCEDURE"];

/// Programs with an argument that has no value of its own (a default or a
/// metadata attribute): a job has to give it one. Without a default,
/// `DEFAULT_VALUE` says `<NO VALUE>` and the ANYDATA one is null.
pub(crate) const WANTING_ARGS: &str = "SELECT p.owner, p.program_name FROM {v}_scheduler_programs p
 WHERE p.number_of_arguments > (SELECT COUNT(*) FROM {v}_scheduler_program_args a
   WHERE a.owner = p.owner AND a.program_name = p.program_name
     AND (a.metadata_attribute IS NOT NULL OR a.default_anydata_value IS NOT NULL))";

fn wanting_args(c: &Connection, ctx: &Ctx) -> HashSet<(String, String)> {
    match crate::jobs::texts(c, &ctx.sql(WANTING_ARGS), &[], 10_000) {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| Some((crate::jobs::col(r, 0)?, crate::jobs::col(r, 1)?)))
            .collect(),
        Err(e) => {
            tracing::debug!("oracle scheduler: program arguments: {e}");
            HashSet::new()
        }
    }
}

pub(crate) fn plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let cat = choices::load(c, ctx, &["schema"]);
    match id {
        None => {
            let s = spec_of(values, ctx, &cat, None)?;
            Ok(Plan::new(create_block(&s), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "programa")?;
            let old = read_state(c, ctx, &owner, &name)?;
            ctx.check_change(Kind::Program, &owner, &name, ctx.oracle_schema(&owner))?;
            let (v, l) = values_of(&old);
            fresh(values, &fingerprint(&v, &l))?;
            let new = spec_of(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(alter_block(&old, &new), old.id(), &name))
        }
    }
}

pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "programa")?;
    let s = read_state(c, ctx, &owner, &name)?;
    ctx.check_change(Kind::Program, &owner, &name, ctx.oracle_schema(&owner))?;
    let mut b = Block::new(&format!("Borra el programa {}.", s.id()));
    b.note("Sin force, Oracle no lo borra mientras algún job lo use.");
    b.call(
        "DBMS_SCHEDULER.DROP_PROGRAM",
        vec![("program_name", A::Name(s.id())), ("force", A::B(false))],
    );
    Ok(b)
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &["schema"]);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "programa")?;
            Some(read_state(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let existing = state.is_some();
    let spec = state.clone().unwrap_or(ProgramSpec {
        owner: ctx.user().into(),
        name: String::new(),
        ptype: "PLSQL_BLOCK".into(),
        action: String::new(),
        args: Vec::new(),
        enabled: true,
        detached: false,
        comments: None,
    });
    let (mut values, lists) = values_of(&spec);
    if existing {
        let fp = fingerprint(&values, &lists);
        values.insert("dbine_state".into(), json!(fp));
    }
    let general = FormSection::new("general", "General").fields(vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::Program))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("detached", "Desacoplado", FormKind::Bool).help(
            "El job que lo ejecuta no termina cuando termina el programa: lo da por terminado una llamada a END_DETACHED_JOB_RUN.",
        ),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]);
    let action = FormSection::new("action", "Acción")
        .field(
            FormField::new(
                "program_type",
                "Tipo",
                only(
                    FormKind::select(&[
                        ("PLSQL_BLOCK", "Bloque PL/SQL"),
                        ("STORED_PROCEDURE", "Procedimiento almacenado"),
                        ("EXECUTABLE", "Ejecutable"),
                        ("EXTERNAL_SCRIPT", "Script externo"),
                        ("SQL_SCRIPT", "Script de SQL*Plus"),
                        ("BACKUP_SCRIPT", "Script de RMAN"),
                    ]),
                    |t| !(ctx.cloud && EXTERNAL.contains(&t)),
                ),
            )
            .required()
            .default(json!("PLSQL_BLOCK")),
        )
        .fields(action_fields("program_type"));
    let args = FormSection::new("arguments", "Argumentos")
        .list(
            FormList::new(
                "arguments",
                "Argumentos",
                "name",
                vec![
                    FormField::new("name", "Nombre", FormKind::Text),
                    FormField::new(
                        "metadata",
                        "Qué recibe",
                        FormKind::select(&[
                            ("VALUE", "Un valor"),
                            ("JOB_NAME", "El nombre del job"),
                            ("JOB_OWNER", "El dueño del job"),
                            ("JOB_START", "La hora de inicio del job"),
                            ("WINDOW_START", "La apertura de la ventana"),
                            ("WINDOW_END", "El cierre de la ventana"),
                            ("JOB_SUBNAME", "El subnombre del job"),
                            ("JOB_SCHEDULED_START", "La hora programada del job"),
                            ("EVENT_MESSAGE", "El mensaje del evento"),
                        ]),
                    )
                    .default(json!("VALUE")),
                    FormField::new("arg_type", "Tipo de dato", FormKind::Text)
                        .placeholder("VARCHAR2")
                        .when("metadata", &["VALUE"]),
                    FormField::new("default", "Valor predeterminado", FormKind::Text)
                        .when("metadata", &["VALUE"]),
                ],
            )
            .orderable()
            .rows(lists.get("arguments").cloned().unwrap_or_default()),
        )
        // A PL/SQL block or a script takes no arguments.
        .when("program_type", &WITH_ARGS);
    let mut f = ObjectForm {
        sections: vec![general, action, args],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    let lock = state.as_ref().and_then(|s| {
        ctx.locked(
            Kind::Program,
            &s.owner,
            &s.name,
            ctx.oracle_schema(&s.owner),
        )
    });
    match &state {
        Some(_) if lock.is_some() => {
            f.read_only = true;
            f.notes.extend(lock.map(String::from));
        }
        None if !ctx.may_create(Kind::Program) => {
            f.read_only = true;
            f.notes.push(super::NO_CREATE_JOB.into());
        }
        Some(s) if s.enabled => f.notes.push(
            "Si el programa está habilitado y cambia lo que ejecuta o sus argumentos, el script lo deshabilita antes y lo vuelve a habilitar al final.".into(),
        ),
        _ => {}
    }
    Ok(f)
}

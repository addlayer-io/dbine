//! Chains: `CREATE_CHAIN`; their steps (`DEFINE_CHAIN_STEP` for a program
//! or a subchain, `DEFINE_CHAIN_EVENT_STEP` for an event schedule or an
//! inline event, `ALTER_CHAIN` for each step's flags, credential and
//! destination, `DROP_CHAIN_STEP`) and rules (`DEFINE_CHAIN_RULE`, which
//! replaces a rule of the same name, `DROP_CHAIN_RULE`); `SET_ATTRIBUTE`
//! for the evaluation interval and comments; `DROP_CHAIN`.

use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::script::{lit, name_lit};
use super::{
    bad, fingerprint, fits, fresh, new_name, new_owner, obj_id, opt_ref, parse_obj, pick_ref, rows,
    secs, shown, Block, Ctx, Kind, Plan, Vals, A,
};
use crate::jobs;
use dbine_driver::{
    FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
    SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const STEP_TYPES: [&str; 4] = ["PROGRAM", "SUBCHAIN", "EVENT_SCHEDULE", "INLINE_EVENT"];
const FLAGS: [(&str, &str); 5] = [
    ("skip", "SKIP"),
    ("pause", "PAUSE"),
    ("pause_before", "PAUSE_BEFORE"),
    ("restart_on_recovery", "RESTART_ON_RECOVERY"),
    ("restart_on_failure", "RESTART_ON_FAILURE"),
];
const SOURCES: [&str; 6] = [
    "schema",
    "program",
    "chain",
    "event_schedule",
    "credential",
    "destination",
];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Step {
    pub(crate) name: String,
    pub(crate) kind: String,
    /// The program, subchain or event schedule (id); the queue of an
    /// inline event.
    pub(crate) target: String,
    pub(crate) condition: Option<String>,
    pub(crate) timeout: Option<i64>,
    pub(crate) credential: Option<String>,
    pub(crate) destination: Option<String>,
    /// skip, pause, pause_before, restart_on_recovery, restart_on_failure.
    pub(crate) flags: [bool; 5],
}

impl Step {
    /// What `DEFINE_CHAIN_*STEP` sets: a change to it defines the step
    /// again (which resets its flags).
    fn definition(&self) -> (&str, &str, &Option<String>, &Option<i64>) {
        (&self.kind, &self.target, &self.condition, &self.timeout)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    pub(crate) name: Option<String>,
    pub(crate) condition: String,
    pub(crate) action: String,
    pub(crate) comments: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChainSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) evaluation_interval: Option<i64>,
    pub(crate) comments: Option<String>,
    pub(crate) steps: Vec<Step>,
    pub(crate) rules: Vec<Rule>,
}

impl ChainSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
}

/// A step or rule name as the chain's calls take it: plain names as they
/// are, any other quoted.
fn member_lit(name: &str) -> A {
    if jobs::plain(name) {
        A::S(name.to_string())
    } else {
        A::R(lit(&crate::quote(name)))
    }
}

pub(crate) fn step_of(r: &Rec) -> Step {
    let kind = r.t("STEP_TYPE").unwrap_or_else(|| "PROGRAM".into());
    let (target, condition) = match kind.as_str() {
        "EVENT_SCHEDULE" => (
            obj_id(
                r.t("EVENT_SCHEDULE_OWNER").as_deref(),
                &r.raw("EVENT_SCHEDULE_NAME").unwrap_or_default(),
            ),
            None,
        ),
        "INLINE_EVENT" => {
            let mut q = match (r.t("EVENT_QUEUE_OWNER"), r.t("EVENT_QUEUE_NAME")) {
                (Some(o), Some(n)) => format!("{o}.{n}"),
                (_, Some(n)) => n,
                _ => String::new(),
            };
            if let Some(a) = r.t("EVENT_QUEUE_AGENT") {
                q = format!("{q}, {a}");
            }
            (q, r.t("EVENT_CONDITION"))
        }
        _ => (
            obj_id(
                r.t("PROGRAM_OWNER").as_deref(),
                &r.raw("PROGRAM_NAME").unwrap_or_default(),
            ),
            None,
        ),
    };
    let mut flags = [false; 5];
    for (i, (_, col)) in FLAGS.iter().enumerate() {
        flags[i] = r.yes(col);
    }
    Step {
        name: r.raw("STEP_NAME").unwrap_or_default(),
        kind,
        target,
        condition,
        timeout: r.int("TIMEOUT").filter(|_| {
            r.t("STEP_TYPE")
                .as_deref()
                .is_some_and(|t| t.contains("EVENT"))
        }),
        credential: r
            .t("CREDENTIAL_NAME")
            .map(|n| obj_id(r.t("CREDENTIAL_OWNER").as_deref(), &n)),
        destination: r
            .t("DESTINATION")
            .map(|d| obj_id(r.t("DESTINATION_OWNER").as_deref(), &d)),
        flags,
    }
}

fn read_state(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<ChainSpec> {
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_chains"),
        "WHERE owner = :1 AND chain_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe la cadena «{}» o este usuario no la puede ver",
            obj_id(Some(owner), name)
        ))
    })?;
    let steps = read::rows(
        c,
        &ctx.sql("{v}_scheduler_chain_steps"),
        "WHERE owner = :1 AND chain_name = :2 ORDER BY step_name",
        &[&owner, &name],
        1000,
    )
    .map_err(crate::err)?;
    let rules = read::rows(
        c,
        &ctx.sql("{v}_scheduler_chain_rules"),
        "WHERE owner = :1 AND chain_name = :2 ORDER BY LENGTH(rule_name), rule_name",
        &[&owner, &name],
        1000,
    )
    .map_err(crate::err)?;
    Ok(ChainSpec {
        owner: owner.to_string(),
        name: name.to_string(),
        enabled: r.yes("ENABLED"),
        evaluation_interval: r.int("EVALUATION_INTERVAL"),
        comments: r.t("COMMENTS"),
        steps: steps.iter().map(step_of).collect(),
        rules: rules
            .iter()
            .map(|r| Rule {
                name: r.raw("RULE_NAME"),
                condition: r.t("CONDITION").unwrap_or_default(),
                action: r.t("ACTION").unwrap_or_default(),
                comments: r.t("COMMENTS"),
            })
            .collect(),
    })
}

type Rows = Vec<BTreeMap<String, Value>>;

pub(crate) fn values_of(s: &ChainSpec) -> (BTreeMap<String, Value>, BTreeMap<String, Rows>) {
    let values = BTreeMap::from([
        ("owner".to_string(), json!(s.owner)),
        ("name".to_string(), json!(s.name)),
        ("enabled".to_string(), json!(s.enabled)),
        (
            "evaluation_interval".to_string(),
            json!(s.evaluation_interval),
        ),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
    ]);
    let steps = s
        .steps
        .iter()
        .map(|st| {
            let mut row = BTreeMap::from([
                ("step_name".to_string(), json!(st.name)),
                ("step_type".to_string(), json!(st.kind)),
                ("timeout".to_string(), json!(st.timeout)),
                (
                    "credential".to_string(),
                    json!(st.credential.clone().unwrap_or_default()),
                ),
                (
                    "destination".to_string(),
                    json!(st.destination.clone().unwrap_or_default()),
                ),
            ]);
            let key = match st.kind.as_str() {
                "SUBCHAIN" => "subchain",
                "EVENT_SCHEDULE" => "event_schedule",
                "INLINE_EVENT" => "event_queue",
                _ => "program",
            };
            row.insert(key.into(), json!(st.target));
            if st.kind == "INLINE_EVENT" {
                row.insert(
                    "event_condition".into(),
                    json!(st.condition.clone().unwrap_or_default()),
                );
            }
            for (i, (k, _)) in FLAGS.iter().enumerate() {
                row.insert(k.to_string(), json!(st.flags[i]));
            }
            row
        })
        .collect();
    let rules = s
        .rules
        .iter()
        .map(|r| {
            BTreeMap::from([
                (
                    "rule_name".to_string(),
                    json!(r.name.clone().unwrap_or_default()),
                ),
                ("condition".to_string(), json!(r.condition)),
                ("action".to_string(), json!(r.action)),
                (
                    "comments".to_string(),
                    json!(r.comments.clone().unwrap_or_default()),
                ),
            ])
        })
        .collect();
    (
        values,
        BTreeMap::from([("steps".to_string(), steps), ("rules".to_string(), rules)]),
    )
}

/// A step's or rule's name as typed: one the chain already has, spelled
/// exactly so, is kept as is (a quoted lower-case name read back stays
/// itself); any other is a new name, folded like Oracle folds one.
fn member_name<'a>(
    raw: &str,
    mut existing: impl Iterator<Item = &'a str>,
    ctx: &Ctx,
    label: &str,
) -> Result<String> {
    let t = raw.trim();
    if existing.any(|n| n == t) {
        return Ok(t.to_string());
    }
    new_name(raw, ctx, label)
}

pub(crate) fn spec_of(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&ChainSpec>,
) -> Result<ChainSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::Chain, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    let mut steps: Vec<Step> = Vec::new();
    for r in rows(values, "steps") {
        let name = member_name(
            &r.raw("step_name"),
            cur.map(|c| c.steps.iter().map(|s| s.name.as_str()))
                .into_iter()
                .flatten(),
            ctx,
            "Paso",
        )?;
        if steps.iter().any(|s| s.name == name) {
            return Err(bad(format!("Hay dos pasos llamados «{}».", shown(&name))));
        }
        let before = cur.and_then(|c| c.steps.iter().find(|s| s.name == name));
        let now_target = before.map(|s| s.target.as_str());
        let kind = r.choice("step_type", "Tipo de paso", &STEP_TYPES, "PROGRAM")?;
        let label = format!("Paso {name}");
        let (target, condition) = match kind.as_str() {
            "PROGRAM" => (
                pick_ref(cat, "program", &r.text("program"), now_target, &label)?,
                None,
            ),
            "SUBCHAIN" => (
                pick_ref(cat, "chain", &r.text("subchain"), now_target, &label)?,
                None,
            ),
            "EVENT_SCHEDULE" => (
                pick_ref(
                    cat,
                    "event_schedule",
                    &r.text("event_schedule"),
                    now_target,
                    &label,
                )?,
                None,
            ),
            _ => {
                let q = r.text("event_queue");
                if q.is_empty() {
                    return Err(bad(format!("{label}: falta «Cola del evento».")));
                }
                (q, r.opt("event_condition"))
            }
        };
        let mut flags = [false; 5];
        for (i, (k, _)) in FLAGS.iter().enumerate() {
            flags[i] = r.bool(k);
        }
        steps.push(Step {
            name,
            target,
            condition,
            timeout: if kind.contains("EVENT") {
                secs(r, "timeout", "Tiempo de espera")?.filter(|n| *n > 0)
            } else {
                None
            },
            // Hidden for other steps: they keep what they have while they
            // keep their type (redefined as another type, a step loses it,
            // and a call to set it again would be one the form hides).
            credential: if kind == "PROGRAM" {
                opt_ref(
                    r,
                    "credential",
                    cat,
                    "credential",
                    before.and_then(|s| s.credential.as_deref()),
                    "Credencial",
                )?
            } else {
                before
                    .filter(|s| s.kind == kind)
                    .and_then(|s| s.credential.clone())
            },
            destination: if kind == "PROGRAM" {
                opt_ref(
                    r,
                    "destination",
                    cat,
                    "destination",
                    before.and_then(|s| s.destination.as_deref()),
                    "Destino",
                )?
            } else {
                before
                    .filter(|s| s.kind == kind)
                    .and_then(|s| s.destination.clone())
            },
            kind,
            flags,
        });
    }
    let mut rules: Vec<Rule> = Vec::new();
    for r in rows(values, "rules") {
        let name = match r.opt("rule_name") {
            Some(n) => Some(member_name(
                &n,
                cur.map(|c| c.rules.iter().filter_map(|r| r.name.as_deref()))
                    .into_iter()
                    .flatten(),
                ctx,
                "Regla",
            )?),
            None => None,
        };
        if name.is_some() && rules.iter().any(|x| x.name == name) {
            return Err(bad(format!(
                "Hay dos reglas llamadas «{}».",
                shown(name.as_deref().unwrap_or_default())
            )));
        }
        let condition = r.text("condition");
        let action = r.text("action");
        if condition.is_empty() || action.is_empty() {
            return Err(bad(
                "Cada regla necesita una condición (TRUE, «PASO1 COMPLETED»…) y una acción («START PASO2», «END»…).".into(),
            ));
        }
        fits(&Some(condition.clone()), "Condición", 4000)?;
        fits(&Some(action.clone()), "Acción", 4000)?;
        rules.push(Rule {
            name,
            condition,
            action,
            comments: r.opt("comments"),
        });
    }
    Ok(ChainSpec {
        owner,
        name,
        enabled: v.bool_or("enabled", true),
        evaluation_interval: secs(v, "evaluation_interval", "Evaluar cada")?.filter(|n| *n > 0),
        comments,
        steps,
        rules,
    })
}

fn define_step(b: &mut Block, target: &A, s: &Step) {
    let mut args = vec![
        ("chain_name", target.clone()),
        ("step_name", member_lit(&s.name)),
    ];
    match s.kind.as_str() {
        "EVENT_SCHEDULE" => {
            args.push(("event_schedule_name", A::Name(s.target.clone())));
            if s.timeout.is_some() {
                args.push(("timeout", A::secs(s.timeout)));
            }
            b.call("DBMS_SCHEDULER.DEFINE_CHAIN_EVENT_STEP", args);
        }
        "INLINE_EVENT" => {
            args.push(("event_condition", A::opt(s.condition.clone())));
            args.push(("queue_spec", A::S(s.target.clone())));
            if s.timeout.is_some() {
                args.push(("timeout", A::secs(s.timeout)));
            }
            b.call("DBMS_SCHEDULER.DEFINE_CHAIN_EVENT_STEP", args);
        }
        _ => {
            args.push(("program_name", A::Name(s.target.clone())));
            b.call("DBMS_SCHEDULER.DEFINE_CHAIN_STEP", args);
        }
    }
}

/// `ALTER_CHAIN` for what differs from `old` (a step just defined: from
/// the defaults).
fn alter_step(b: &mut Block, target: &A, s: &Step, old: &Step) {
    for (i, (_, attr)) in FLAGS.iter().enumerate() {
        if s.flags[i] != old.flags[i] {
            b.call(
                "DBMS_SCHEDULER.ALTER_CHAIN",
                vec![
                    ("chain_name", target.clone()),
                    ("step_name", member_lit(&s.name)),
                    ("attribute", A::S(attr.to_string())),
                    ("value", A::B(s.flags[i])),
                ],
            );
        }
    }
    for (attr, new, was) in [
        ("CREDENTIAL_NAME", &s.credential, &old.credential),
        ("DESTINATION_NAME", &s.destination, &old.destination),
    ] {
        if new != was {
            b.call(
                "DBMS_SCHEDULER.ALTER_CHAIN",
                vec![
                    ("chain_name", target.clone()),
                    ("step_name", member_lit(&s.name)),
                    ("attribute", A::S(attr.to_string())),
                    ("char_value", new.clone().map_or(A::Null, A::Name)),
                ],
            );
        }
    }
}

fn define_rule(b: &mut Block, target: &A, r: &Rule) {
    let mut args = vec![
        ("chain_name", target.clone()),
        ("condition", A::S(r.condition.clone())),
        ("action", A::S(r.action.clone())),
    ];
    if let Some(n) = &r.name {
        args.push(("rule_name", member_lit(n)));
    }
    if let Some(cm) = &r.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    b.call("DBMS_SCHEDULER.DEFINE_CHAIN_RULE", args);
}

pub(crate) fn create_block(s: &ChainSpec) -> Block {
    let id = s.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Crea la cadena {id}."));
    let mut args = vec![("chain_name", target.clone()), ("rule_set_name", A::Null)];
    if s.evaluation_interval.is_some() {
        args.push(("evaluation_interval", A::secs(s.evaluation_interval)));
    }
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    b.call("DBMS_SCHEDULER.CREATE_CHAIN", args);
    b.created(&format!(
        "DBMS_SCHEDULER.DROP_CHAIN(chain_name => {}, force => TRUE);",
        name_lit(&id)
    ));
    for st in &s.steps {
        define_step(&mut b, &target, st);
        alter_step(&mut b, &target, st, &Step::default());
    }
    for r in &s.rules {
        define_rule(&mut b, &target, r);
    }
    if s.enabled {
        b.pcall("DBMS_SCHEDULER.ENABLE", vec![target]);
    }
    b
}

pub(crate) fn alter_block(old: &ChainSpec, new: &ChainSpec) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia la cadena {id}."));
    // Rules that go, then steps that go (no rule of the new set names
    // them).
    for r in &old.rules {
        if let Some(n) = &r.name {
            if !new.rules.iter().any(|x| x.name.as_ref() == Some(n)) {
                b.call(
                    "DBMS_SCHEDULER.DROP_CHAIN_RULE",
                    vec![
                        ("chain_name", target.clone()),
                        ("rule_name", member_lit(n)),
                        ("force", A::B(true)),
                    ],
                );
            }
        }
    }
    for st in &old.steps {
        if !new.steps.iter().any(|x| x.name == st.name) {
            b.call(
                "DBMS_SCHEDULER.DROP_CHAIN_STEP",
                vec![
                    ("chain_name", target.clone()),
                    ("step_name", member_lit(&st.name)),
                    ("force", A::B(true)),
                ],
            );
        }
    }
    for st in &new.steps {
        match old.steps.iter().find(|x| x.name == st.name) {
            Some(was) if was.definition() == st.definition() => {
                alter_step(&mut b, &target, st, was)
            }
            _ => {
                define_step(&mut b, &target, st);
                alter_step(&mut b, &target, st, &Step::default());
            }
        }
    }
    for r in &new.rules {
        let same = r.name.is_some() && old.rules.iter().any(|x| x == r);
        if !same {
            define_rule(&mut b, &target, r);
        }
    }
    if old.evaluation_interval != new.evaluation_interval {
        b.set_attr(
            &target,
            "evaluation_interval",
            A::secs(new.evaluation_interval),
        );
    }
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    if old.enabled != new.enabled {
        if new.enabled {
            b.pcall("DBMS_SCHEDULER.ENABLE", vec![target]);
        } else {
            b.call(
                "DBMS_SCHEDULER.DISABLE",
                vec![("name", target), ("force", A::B(true))],
            );
        }
    }
    b
}

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_chains"),
        "ORDER BY owner, chain_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    // "Ejecutar ahora", where the kind offers it.
    let runs = !super::action::of(ctx, Kind::Chain).is_empty();
    let granted = if runs {
        super::executable(c, ctx, "{v}_scheduler_chains", "chain_name")
    } else {
        Default::default()
    };
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("CHAIN_NAME").unwrap_or_default();
                let system = ctx.oracle_schema(&owner);
                let locked = ctx.locked(Kind::Chain, &owner, &name, system).is_some();
                let run = !system && r.yes("ENABLED") && ctx.may_execute(&owner, &name, &granted);
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
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

pub(crate) fn plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let cat = choices::load(c, ctx, &SOURCES);
    match id {
        None => {
            let s = spec_of(values, ctx, &cat, None)?;
            Ok(Plan::new(create_block(&s), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "cadena")?;
            let old = read_state(c, ctx, &owner, &name)?;
            ctx.check_change(Kind::Chain, &owner, &name, ctx.oracle_schema(&owner))?;
            let (v, l) = values_of(&old);
            fresh(values, &fingerprint(&v, &l))?;
            let new = spec_of(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(alter_block(&old, &new), old.id(), &name))
        }
    }
}

pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "cadena")?;
    let s = read_state(c, ctx, &owner, &name)?;
    ctx.check_change(Kind::Chain, &owner, &name, ctx.oracle_schema(&owner))?;
    let mut b = Block::new(&format!(
        "Borra la cadena {} con sus pasos y reglas.",
        s.id()
    ));
    b.note("Sin force, Oracle no la borra mientras un job la use.");
    b.call(
        "DBMS_SCHEDULER.DROP_CHAIN",
        vec![("chain_name", A::Name(s.id())), ("force", A::B(false))],
    );
    Ok(b)
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &SOURCES);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "cadena")?;
            Some(read_state(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let existing = state.is_some();
    let spec = state.clone().unwrap_or(ChainSpec {
        owner: ctx.user().into(),
        name: String::new(),
        enabled: true,
        evaluation_interval: None,
        comments: None,
        steps: Vec::new(),
        rules: Vec::new(),
    });
    let (mut values, lists) = values_of(&spec);
    if existing {
        let fp = fingerprint(&values, &lists);
        values.insert("dbine_state".into(), json!(fp));
    }
    let general = FormSection::new("general", "General").fields(vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::Chain))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
        FormField::new("evaluation_interval", "Evaluar cada", FormKind::Duration).help(
            "Además de al terminar cada paso, cada cuánto se evalúan sus reglas (para condiciones sobre datos o la hora). Vacío: solo al terminar un paso.",
        ),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]);
    let event = ["EVENT_SCHEDULE", "INLINE_EVENT"];
    let mut step_fields = vec![
        FormField::new("step_name", "Nombre", FormKind::Text).required(),
        FormField::new(
            "step_type",
            "Tipo",
            FormKind::select(&[
                ("PROGRAM", "Programa"),
                ("SUBCHAIN", "Subcadena"),
                ("EVENT_SCHEDULE", "Esperar un evento (programación)"),
                ("INLINE_EVENT", "Esperar un evento (cola)"),
            ]),
        )
        .required()
        .default(json!("PROGRAM")),
        FormField::new("program", "Programa", FormKind::reference("program", false))
            .when("step_type", &["PROGRAM"]),
        FormField::new("subchain", "Cadena", FormKind::reference("chain", false))
            .when("step_type", &["SUBCHAIN"]),
        FormField::new(
            "event_schedule",
            "Programación por evento",
            FormKind::reference("event_schedule", false),
        )
        .when("step_type", &["EVENT_SCHEDULE"]),
        FormField::new("event_queue", "Cola del evento", FormKind::Text)
            .placeholder("APP.EVENTOS_Q, AGENTE")
            .when("step_type", &["INLINE_EVENT"]),
        FormField::new(
            "event_condition",
            "Condición del evento",
            FormKind::Textarea,
        )
        .when("step_type", &["INLINE_EVENT"]),
        FormField::new("timeout", "Tiempo de espera", FormKind::Duration)
            .help("Cuánto espera el evento antes de fallar. Vacío: sin límite.")
            .when("step_type", &event),
        // Where and as whom a program step runs; a subchain's steps and
        // an event step have their own.
        FormField::new(
            "credential",
            "Credencial",
            FormKind::reference("credential", false),
        )
        .when("step_type", &["PROGRAM"]),
    ];
    if ctx.has(Kind::DbDestination) || ctx.has(Kind::ExternalDestination) {
        step_fields.push(
            FormField::new(
                "destination",
                "Destino",
                FormKind::reference("destination", false),
            )
            .when("step_type", &["PROGRAM"]),
        );
    }
    step_fields.extend([
        FormField::new("skip", "Saltear", FormKind::Bool)
            .help("La cadena lo da por terminado sin ejecutarlo."),
        FormField::new("pause", "Pausar después", FormKind::Bool)
            .help("Al terminar, la cadena espera hasta que se le quite la pausa."),
        FormField::new("pause_before", "Pausar antes", FormKind::Bool),
        FormField::new(
            "restart_on_recovery",
            "Reiniciar al recuperarse la base",
            FormKind::Bool,
        ),
        FormField::new("restart_on_failure", "Reiniciar si falla", FormKind::Bool),
    ]);
    let steps = FormSection::new("steps", "Pasos").list(
        FormList::new("steps", "Pasos", "step_name", step_fields)
            .rows(lists.get("steps").cloned().unwrap_or_default()),
    );
    let rules = FormSection::new("rules", "Reglas").list(
        FormList::new(
            "rules",
            "Reglas",
            "rule_name",
            vec![
                FormField::new("rule_name", "Nombre", FormKind::Text)
                    .help("Vacío: Oracle le pone uno (SCHED_RULE$…)."),
                FormField::new("condition", "Condición", FormKind::Textarea)
                    .required()
                    .placeholder("PASO1 SUCCEEDED AND PASO2 COMPLETED"),
                FormField::new("action", "Acción", FormKind::Text)
                    .required()
                    .placeholder("START PASO3"),
                FormField::new("comments", "Descripción", FormKind::Text),
            ],
        )
        .rows(lists.get("rules").cloned().unwrap_or_default()),
    );
    let mut f = ObjectForm {
        sections: vec![general, steps, rules],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    f.notes.push(
        "Un paso que cambia de programa, subcadena o evento se define de nuevo; uno que cambia de nombre es otro paso (y las reglas lo nombran por su nombre).".into(),
    );
    match &state {
        Some(s) => {
            if let Some(why) =
                ctx.locked(Kind::Chain, &s.owner, &s.name, ctx.oracle_schema(&s.owner))
            {
                f.read_only = true;
                f.notes.insert(0, why.into());
            }
        }
        None if !ctx.may_create(Kind::Chain) => {
            f.read_only = true;
            f.notes.insert(0, super::NO_RULES.into());
        }
        _ => {}
    }
    Ok(f)
}

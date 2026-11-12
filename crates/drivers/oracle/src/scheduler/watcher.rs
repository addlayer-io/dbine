//! File watchers (11.2+): `CREATE_FILE_WATCHER`, `SET_ATTRIBUTE`
//! (directory, file name, credential, destination, minimum size, steady
//! state, comments), `ENABLE` / `DISABLE`, `DROP_FILE_WATCHER`. A job
//! started by one has the watcher as its schedule.

use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::script::name_lit;
use super::{
    bad, fingerprint, fits, fresh, new_name, new_owner, obj_id, opt_ref, parse_obj, pick_ref, secs,
    Block, Ctx, Kind, Plan, Vals, A, OWNER_ONLY,
};
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const SOURCES: [&str; 3] = ["schema", "credential", "external_destination"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatcherSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) directory: String,
    pub(crate) file_name: String,
    pub(crate) credential: String,
    pub(crate) destination: Option<String>,
    pub(crate) min_size: i64,
    pub(crate) steady: Option<i64>,
    pub(crate) enabled: bool,
    pub(crate) comments: Option<String>,
}

impl WatcherSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
}

pub(crate) fn spec_of_rec(r: &Rec) -> WatcherSpec {
    WatcherSpec {
        owner: r.t("OWNER").unwrap_or_default(),
        name: r.raw("FILE_WATCHER_NAME").unwrap_or_default(),
        directory: r.raw("DIRECTORY_PATH").unwrap_or_default(),
        file_name: r.raw("FILE_NAME").unwrap_or_default(),
        credential: r
            .t("CREDENTIAL_NAME")
            .map(|n| obj_id(r.t("CREDENTIAL_OWNER").as_deref(), &n))
            .unwrap_or_default(),
        destination: r
            .t("DESTINATION")
            .map(|d| obj_id(r.t("DESTINATION_OWNER").as_deref(), &d)),
        min_size: r.int("MIN_FILE_SIZE").unwrap_or(0),
        steady: r.int("STEADY_STATE_DURATION"),
        enabled: r.yes("ENABLED"),
        comments: r.t("COMMENTS"),
    }
}

fn read_state(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<WatcherSpec> {
    read::row(
        c,
        &ctx.sql("{v}_scheduler_file_watchers"),
        "WHERE owner = :1 AND file_watcher_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .map(|r| spec_of_rec(&r))
    .ok_or_else(|| {
        bad(format!(
            "no existe el observador de archivos «{}» o este usuario no lo puede ver",
            obj_id(Some(owner), name)
        ))
    })
}

pub(crate) fn values_of(s: &WatcherSpec) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("owner".to_string(), json!(s.owner)),
        ("name".to_string(), json!(s.name)),
        ("directory_path".to_string(), json!(s.directory)),
        ("file_name".to_string(), json!(s.file_name)),
        ("credential".to_string(), json!(s.credential)),
        (
            "destination".to_string(),
            json!(s.destination.clone().unwrap_or_default()),
        ),
        ("min_file_size".to_string(), json!(s.min_size)),
        ("steady_state_duration".to_string(), json!(s.steady)),
        ("enabled".to_string(), json!(s.enabled)),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
    ])
}

pub(crate) fn spec_of(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&WatcherSpec>,
) -> Result<WatcherSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::FileWatcher, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let directory = v.text("directory_path");
    let file_name = v.text("file_name");
    if directory.is_empty() || file_name.is_empty() {
        return Err(bad(
            "Faltan «Directorio» y «Archivo» (con comodines * y ?).".into(),
        ));
    }
    fits(&Some(directory.clone()), "Directorio", 4000)?;
    fits(&Some(file_name.clone()), "Archivo", 512)?;
    let credential = v.text("credential");
    if credential.is_empty() {
        return Err(bad(
            "Falta «Credencial»: el usuario del sistema operativo que lee el directorio.".into(),
        ));
    }
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(WatcherSpec {
        credential: pick_ref(
            cat,
            "credential",
            &credential,
            cur.map(|c| c.credential.as_str()),
            "Credencial",
        )?,
        destination: opt_ref(
            v,
            "destination",
            cat,
            "external_destination",
            cur.and_then(|c| c.destination.as_deref()),
            "Destino",
        )?,
        min_size: v
            .int("min_file_size", "Tamaño mínimo", 0, i64::from(i32::MAX))?
            .unwrap_or(0),
        steady: secs(v, "steady_state_duration", "Estabilidad")?.filter(|n| *n > 0),
        enabled: v.bool_or("enabled", true),
        owner,
        name,
        directory,
        file_name,
        comments,
    })
}

pub(crate) fn create_block(s: &WatcherSpec) -> Block {
    let id = s.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Crea el observador de archivos {id}."));
    let mut args = vec![
        ("file_watcher_name", target.clone()),
        ("directory_path", A::S(s.directory.clone())),
        ("file_name", A::S(s.file_name.clone())),
        ("credential_name", A::Name(s.credential.clone())),
    ];
    if let Some(d) = &s.destination {
        args.push(("destination", super::bare(d)));
    }
    if s.min_size > 0 {
        args.push(("min_file_size", A::I(s.min_size)));
    }
    if s.steady.is_some() {
        args.push(("steady_state_duration", A::secs(s.steady)));
    }
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    args.push(("enabled", A::B(false)));
    b.call("DBMS_SCHEDULER.CREATE_FILE_WATCHER", args);
    if s.enabled {
        b.created(&format!(
            "DBMS_SCHEDULER.DROP_FILE_WATCHER(file_watcher_name => {}, force => TRUE);",
            name_lit(&id)
        ));
        b.pcall("DBMS_SCHEDULER.ENABLE", vec![target]);
    }
    b
}

pub(crate) fn alter_block(old: &WatcherSpec, new: &WatcherSpec) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia el observador de archivos {id}."));
    let changes: [(&str, bool, A); 7] = [
        (
            "directory_path",
            old.directory != new.directory,
            A::S(new.directory.clone()),
        ),
        (
            "file_name",
            old.file_name != new.file_name,
            A::S(new.file_name.clone()),
        ),
        (
            "credential_name",
            old.credential != new.credential,
            A::Name(new.credential.clone()),
        ),
        (
            "destination",
            old.destination != new.destination,
            new.destination.as_deref().map_or(A::Null, super::bare),
        ),
        (
            "min_file_size",
            old.min_size != new.min_size,
            A::I(new.min_size),
        ),
        (
            "steady_state_duration",
            old.steady != new.steady,
            A::secs(new.steady),
        ),
        (
            "comments",
            old.comments != new.comments,
            A::opt(new.comments.clone()),
        ),
    ];
    for (attr, changed, value) in changes {
        if changed {
            b.set_attr(&target, attr, value);
        }
    }
    if old.enabled != new.enabled {
        let proc_name = if new.enabled {
            "DBMS_SCHEDULER.ENABLE"
        } else {
            "DBMS_SCHEDULER.DISABLE"
        };
        b.pcall(proc_name, vec![target]);
    }
    b
}

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_file_watchers"),
        "ORDER BY owner, file_watcher_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("FILE_WATCHER_NAME").unwrap_or_default();
                SchedulerItem {
                    id: obj_id(Some(&owner), &name),
                    name: super::item_name(ctx, &owner, &name),
                    description: r.t("COMMENTS").or_else(|| {
                        Some(format!(
                            "{}/{}",
                            r.t("DIRECTORY_PATH").unwrap_or_default(),
                            r.t("FILE_NAME").unwrap_or_default()
                        ))
                    }),
                    enabled: Some(r.yes("ENABLED")),
                    system: ctx.oracle_schema(&owner),
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
            let (owner, name) = parse_obj(id, "observador de archivos")?;
            let old = read_state(c, ctx, &owner, &name)?;
            if !ctx.may_change(Kind::FileWatcher, &owner, &name, ctx.oracle_schema(&owner)) {
                return Err(bad(OWNER_ONLY.into()));
            }
            fresh(values, &fingerprint(&values_of(&old), &BTreeMap::new()))?;
            let new = spec_of(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(alter_block(&old, &new), old.id(), &name))
        }
    }
}

pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "observador de archivos")?;
    let s = read_state(c, ctx, &owner, &name)?;
    if !ctx.may_change(Kind::FileWatcher, &owner, &name, ctx.oracle_schema(&owner)) {
        return Err(bad(OWNER_ONLY.into()));
    }
    let mut b = Block::new(&format!("Borra el observador de archivos {}.", s.id()));
    b.note("Sin force, Oracle no lo borra mientras un job lo use.");
    b.call(
        "DBMS_SCHEDULER.DROP_FILE_WATCHER",
        vec![
            ("file_watcher_name", A::Name(s.id())),
            ("force", A::B(false)),
        ],
    );
    Ok(b)
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &SOURCES);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "observador de archivos")?;
            Some(read_state(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let existing = state.is_some();
    let values = match &state {
        Some(s) => {
            let mut v = values_of(s);
            let fp = fingerprint(&v, &BTreeMap::new());
            v.insert("dbine_state".into(), json!(fp));
            v
        }
        None => BTreeMap::from([
            ("owner".to_string(), json!(ctx.user())),
            ("enabled".to_string(), json!(true)),
            ("min_file_size".to_string(), json!(0)),
            ("comments".to_string(), json!("")),
        ]),
    };
    let mut fields = vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::FileWatcher))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new("directory_path", "Directorio", FormKind::Text)
            .required()
            .placeholder("/u01/entrada")
            .help("En el servidor (o en el host del destino). Puede ser ?/ para el ORACLE_HOME."),
        FormField::new("file_name", "Archivo", FormKind::Text)
            .required()
            .placeholder("ventas_*.csv")
            .help("El nombre a esperar; acepta los comodines * y ?."),
        FormField::new(
            "credential",
            "Credencial",
            FormKind::reference("credential", false),
        )
        .required()
        .help("El usuario del sistema operativo con el que se lee el directorio."),
    ];
    if ctx.has(Kind::ExternalDestination) {
        fields.push(
            FormField::new(
                "destination",
                "Destino",
                FormKind::reference("external_destination", false),
            )
            .help("El host remoto (un agente del Scheduler). Vacío: este servidor."),
        );
    }
    fields.extend([
        FormField::new("min_file_size", "Tamaño mínimo", FormKind::number(Some(0.0), None))
            .help("En bytes: un archivo más chico no se toma en cuenta."),
        FormField::new("steady_state_duration", "Estabilidad", FormKind::Duration)
            .help("Cuánto tiempo sin cambios tiene que pasar el archivo para darlo por llegado. Vacío: el de Oracle."),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]);
    let mut f = ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(fields)],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    f.notes.push("El Scheduler revisa los directorios cada 10 minutos (lo cambia la programación SYS.FILE_WATCHER_SCHEDULE). El programa del job que lo usa recibe el archivo con un argumento de metadatos EVENT_MESSAGE.".into());
    match &state {
        Some(s)
            if !ctx.may_change(
                Kind::FileWatcher,
                &s.owner,
                &s.name,
                ctx.oracle_schema(&s.owner),
            ) =>
        {
            f.read_only = true;
            f.notes.insert(0, OWNER_ONLY.into());
        }
        None if !ctx.may_create(Kind::FileWatcher) => {
            f.read_only = true;
            f.notes.insert(0, super::NO_CREATE_JOB.into());
        }
        _ => {}
    }
    Ok(f)
}

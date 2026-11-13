//! Pipes: `CREATE PIPE <name> [WITH SOURCE (…)] [WITH PROCESSOR (…)] WITH
//! SINK (…)`, each clause a list of `'key'='value'` attributes whose first
//! one names the plugin (`source` / `processor` / `sink`; `extractor` and
//! `connector` on pipes made before 1.3). `ALTER PIPE … MODIFY <clause>
//! (…)` merges attributes in, `REPLACE <clause> (…)` swaps the clause
//! whole; both keep the pipe's progress. `START` / `STOP PIPE` are its
//! enabled state, `DROP PIPE` deletes it.
//!
//! The form has the three plugins, each clause's attributes as a list and
//! the credentials (attributes whose key reads as one) in a list of their
//! own, whose values never come back: left empty, the server's value
//! stays. IoTDB 1.x shows them in `SHOW PIPES`, so a clause written whole
//! (`REPLACE`, or the pipe created again) sends the server's own value
//! back; 2.x hides them, and then the user has to type them again.

use super::{
    bad, ident, lit, mask, name_field, rows, shown, Ctx, Plan, Recreate, Stmt, Vals, NO_PIPE,
};
use crate::jobs::{self, secret_key, Pipe, MASK};
use crate::{IotDbSession, Table};
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const SYSTEM: &str = "Es un pipe interno de IoTDB: DBine no lo modifica ni lo borra.";
const KEEP_SECRETS: &str =
    "IoTDB no devuelve las credenciales: dejalas vacías para conservar las que tiene el servidor.";
const NEW_STARTS: &str = "Un pipe arranca apenas se crea; si se desmarca «En marcha», DBine lo detiene enseguida (STOP PIPE).";
const RESTARTS: &str = "Volver a crear un pipe lo hace empezar de cero: vuelve a enviar lo histórico según source.history.* y source.start-time.";
const RUNS_BEFORE_STOP: &str =
    "El pipe nuevo arranca al crearse: envía datos hasta que DBine lo detiene (STOP PIPE).";

/// The three parts of a pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clause {
    Source,
    Processor,
    Sink,
}

pub(crate) const CLAUSES: [Clause; 3] = [Clause::Source, Clause::Processor, Clause::Sink];

impl Clause {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Clause::Source => "SOURCE",
            Clause::Processor => "PROCESSOR",
            Clause::Sink => "SINK",
        }
    }

    /// The plugin's key (and the form's field for it).
    pub(crate) fn key(self) -> &'static str {
        match self {
            Clause::Source => "source",
            Clause::Processor => "processor",
            Clause::Sink => "sink",
        }
    }

    /// The plugin's key before 1.3.
    fn legacy(self) -> Option<&'static str> {
        match self {
            Clause::Source => Some("extractor"),
            Clause::Processor => None,
            Clause::Sink => Some("connector"),
        }
    }

    fn is_plugin_key(self, k: &str) -> bool {
        k.eq_ignore_ascii_case(self.key())
            || self.legacy().is_some_and(|l| k.eq_ignore_ascii_case(l))
    }

    fn list(self) -> &'static str {
        match self {
            Clause::Source => "source_attrs",
            Clause::Processor => "processor_attrs",
            Clause::Sink => "sink_attrs",
        }
    }

    /// For messages: the clause's keyword, the same in every language.
    pub(crate) fn label(self) -> &'static str {
        self.word()
    }

    fn of(v: &str) -> Option<Clause> {
        CLAUSES.into_iter().find(|c| c.key() == v)
    }

    pub(crate) fn attrs(self, p: &Pipe) -> &[(String, String)] {
        match self {
            Clause::Source => &p.source,
            Clause::Processor => &p.processor,
            Clause::Sink => &p.sink,
        }
    }

    /// What IoTDB uses when the clause is empty.
    fn default_plugin(self) -> Option<&'static str> {
        match self {
            Clause::Source => Some("iotdb-source"),
            Clause::Processor => Some("do-nothing-processor"),
            Clause::Sink => None,
        }
    }
}

/// An attribute's value as the designer sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Val {
    Plain(String),
    /// A credential: `None` keeps the server's.
    Secret(Option<String>),
}

/// The pipe the designer describes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Desired {
    pub(crate) name: String,
    pub(crate) enabled: bool,
    /// Source, processor and sink: the plugin first, then the attributes.
    pub(crate) clauses: [Vec<(String, Val)>; 3],
}

/// IoTDB's own attributes (`__system.sql-dialect`): never shown or written.
fn internal(k: &str) -> bool {
    k.starts_with("__")
}

/// An attribute's key: IoTDB's are dotted words with dashes.
fn attr_key(k: &str, clause: Clause) -> Result<String> {
    let k = k.trim();
    let ok = !k.is_empty()
        && k.len() <= 200
        && !internal(k)
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if !ok {
        return Err(bad(format!(
            "«{}» no es un atributo de pipe válido (letras, números, puntos, guiones).",
            shown(k)
        )));
    }
    if clause.is_plugin_key(k) {
        return Err(bad(format!(
            "El plugin de {} va en su campo, no entre los atributos.",
            clause.label()
        )));
    }
    Ok(k.to_string())
}

/// A plugin's name (`iotdb-thrift-sink`), lowercase.
pub(crate) fn plugin_name(v: &str) -> Result<String> {
    let t = v.trim();
    let ok = !t.is_empty()
        && t.len() <= 200
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if !ok {
        return Err(bad(format!(
            "«{}» no es el nombre de un plugin de pipe.",
            shown(v)
        )));
    }
    Ok(t.to_ascii_lowercase())
}

/// The plugin keys a pipe uses (`extractor` / `connector` on old ones), so
/// a change doesn't rename them.
fn plugin_keys(p: Option<&Pipe>) -> [&'static str; 3] {
    CLAUSES.map(|c| match (p, c.legacy()) {
        (Some(p), Some(l))
            if c.attrs(p).iter().any(|(k, _)| k == l)
                && !c.attrs(p).iter().any(|(k, _)| k == c.key()) =>
        {
            l
        }
        _ => c.key(),
    })
}

pub(crate) fn from_values(values: &FormValues, keys: [&str; 3]) -> Result<Desired> {
    let v = Vals(&values.values);
    let name = name_field(v, "El pipe")?;
    let mut clauses: [Vec<(String, Val)>; 3] = Default::default();
    for (i, c) in CLAUSES.iter().enumerate() {
        if let Some(p) = v.opt(c.key()) {
            clauses[i].push((keys[i].to_string(), Val::Plain(plugin_name(&p)?)));
        }
        for r in rows(values, c.list()) {
            let value = r.raw("value").trim().to_string();
            let Some(k) = r.opt("key") else {
                if value.is_empty() {
                    continue;
                }
                return Err(bad(format!(
                    "Un atributo de {} no tiene nombre.",
                    c.label()
                )));
            };
            clauses[i].push((attr_key(&k, *c)?, Val::Plain(value)));
        }
    }
    for r in rows(values, "secrets") {
        let Some(k) = r.opt("key") else {
            if r.secret("value").is_none() {
                continue;
            }
            return Err(bad("Una credencial no tiene nombre de atributo.".into()));
        };
        let clause = r
            .opt("clause")
            .as_deref()
            .and_then(Clause::of)
            .ok_or_else(|| {
                bad(format!(
                    "La credencial «{}» no dice de qué parte del pipe es.",
                    shown(&k)
                ))
            })?;
        let i = CLAUSES.iter().position(|c| *c == clause).unwrap_or(0);
        clauses[i].push((attr_key(&k, clause)?, Val::Secret(r.secret("value"))));
    }
    for (i, c) in CLAUSES.iter().enumerate() {
        let attrs = &clauses[i];
        for (j, (k, _)) in attrs.iter().enumerate() {
            if attrs[..j].iter().any(|(o, _)| o.eq_ignore_ascii_case(k)) {
                return Err(bad(format!(
                    "El atributo «{}» de {} está dos veces.",
                    shown(k),
                    c.label()
                )));
            }
        }
    }
    if !clauses[2]
        .iter()
        .any(|(k, _)| Clause::Sink.is_plugin_key(k))
    {
        return Err(bad("El pipe necesita un plugin de destino (sink).".into()));
    }
    Ok(Desired {
        name,
        enabled: v.bool_or("enabled", true),
        clauses,
    })
}

/// A clause's attributes as the script shows them, as the save sends them,
/// and the credentials in them.
struct Part {
    shown: String,
    real: std::result::Result<String, String>,
    secrets: Vec<String>,
    /// `real`'s `Err` is a credential IoTDB hides (see [`Stmt::hidden`]).
    hidden: bool,
}

/// `'key'='value', …` as the script shows it and as the save sends it.
/// `current`: the server's attributes, for a credential left empty.
fn attr_list(
    clause: Clause,
    attrs: &[(String, Val)],
    current: &[(String, String)],
) -> Result<Part> {
    let masked = lit(mask(), "")?;
    let mut shown_parts = Vec::new();
    let mut real_parts = Vec::new();
    let mut secrets = Vec::new();
    // Why the save can't write it, and whether IoTDB hides it.
    let mut missing: Option<(String, bool)> = None;
    for (k, v) in attrs {
        let key = lit(k, k)?;
        let (s, r) = match v {
            Val::Plain(x) => {
                let l = lit(x, k)?;
                if secret_key(k) {
                    secrets.push(x.clone());
                }
                (
                    if secret_key(k) {
                        masked.clone()
                    } else {
                        l.clone()
                    },
                    Some(l),
                )
            }
            Val::Secret(Some(x)) => {
                secrets.push(x.clone());
                (masked.clone(), Some(lit(x, k)?))
            }
            Val::Secret(None) => {
                let server = current.iter().find(|(c, _)| c == k).map(|(_, v)| v);
                match server.filter(|v| v.as_str() != MASK) {
                    Some(sv) => {
                        secrets.push(sv.clone());
                        (masked.clone(), Some(lit(sv, k)?))
                    }
                    None => {
                        missing.get_or_insert_with(|| match server {
                            Some(_) => (
                                format!(
                                    "«{}»: IoTDB no devuelve el valor de esta credencial y hay que escribir {} entero: escribila de nuevo (o quitala).",
                                    shown(k),
                                    clause.label()
                                ),
                                true,
                            ),
                            None => (
                                format!("Falta el valor de la credencial «{}».", shown(k)),
                                false,
                            ),
                        });
                        (masked.clone(), None)
                    }
                }
            }
        };
        shown_parts.push(format!("{key}={s}"));
        if let Some(r) = r {
            real_parts.push(format!("{key}={r}"));
        }
    }
    let hidden = missing.as_ref().is_some_and(|(_, h)| *h);
    let real = match missing {
        Some((m, _)) => Err(m),
        None => Ok(real_parts.join(", ")),
    };
    Ok(Part {
        shown: shown_parts.join(", "),
        real,
        secrets,
        hidden,
    })
}

/// A clause written whole: an empty one takes IoTDB's default plugin.
fn whole(clause: Clause, attrs: &[(String, Val)]) -> Vec<(String, Val)> {
    match (attrs.is_empty(), clause.default_plugin()) {
        (true, Some(p)) => vec![(clause.key().to_string(), Val::Plain(p.to_string()))],
        _ => attrs.to_vec(),
    }
}

/// Appends ` <word> (<attrs>)` to both texts of a statement.
fn append(st: &mut Stmt, word: &str, part: Part) {
    st.shown.push_str(&format!("\n{word} ({})", part.shown));
    st.secrets.extend(part.secrets);
    st.real = match (
        std::mem::replace(&mut st.real, Ok(String::new())),
        part.real,
    ) {
        (Ok(mut r), Ok(p)) => {
            r.push_str(&format!("\n{word} ({p})"));
            Ok(r)
        }
        (Err(m), _) => Err(m),
        (Ok(_), Err(m)) => {
            st.hidden = part.hidden;
            Err(m)
        }
    };
}

pub(crate) fn create_stmt(
    name: &str,
    clauses: &[Vec<(String, Val)>; 3],
    current: Option<&Pipe>,
) -> Result<Stmt> {
    let mut st = Stmt::plain(format!("CREATE PIPE {}", ident(name)));
    for (i, c) in CLAUSES.iter().enumerate() {
        if clauses[i].is_empty() {
            continue;
        }
        let cur = current.map(|p| c.attrs(p)).unwrap_or(&[]);
        append(
            &mut st,
            &format!("WITH {}", c.word()),
            attr_list(*c, &clauses[i], cur)?,
        );
    }
    Ok(st)
}

/// The pipe as the server has it, to create it again: the credentials it
/// hides (2.x) can't be.
fn restore_clauses(p: &Pipe) -> [Vec<(String, Val)>; 3] {
    CLAUSES.map(|c| {
        c.attrs(p)
            .iter()
            .filter(|(k, _)| !internal(k))
            .map(|(k, v)| {
                let val = if v == MASK {
                    Val::Secret(None)
                } else {
                    Val::Plain(v.clone())
                };
                (k.clone(), val)
            })
            .collect()
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Change {
    Same,
    /// `MODIFY`: these attributes, added or changed.
    Modify(Vec<(String, Val)>),
    /// `REPLACE`: an attribute was taken away.
    Replace,
}

pub(crate) fn diff(
    clause: Clause,
    current: &[(String, String)],
    desired: &[(String, Val)],
) -> Change {
    let cur: Vec<&(String, String)> = current.iter().filter(|(k, _)| !internal(k)).collect();
    if cur
        .iter()
        .any(|(k, _)| !desired.iter().any(|(d, _)| d == k))
    {
        return Change::Replace;
    }
    let changed: Vec<(String, Val)> = desired
        .iter()
        .filter(|(k, v)| {
            let now = cur.iter().find(|(c, _)| c == k).map(|(_, v)| v.as_str());
            match v {
                Val::Plain(x) if clause.is_plugin_key(k) => {
                    !now.is_some_and(|n| n.eq_ignore_ascii_case(x))
                }
                Val::Plain(x) => now != Some(x.as_str()),
                Val::Secret(Some(x)) => now != Some(x.as_str()),
                Val::Secret(None) => now.is_none(),
            }
        })
        .cloned()
        .collect();
    if changed.is_empty() {
        Change::Same
    } else {
        Change::Modify(changed)
    }
}

fn start_stop(name: &str, enabled: bool) -> Stmt {
    Stmt::plain(format!(
        "{} PIPE {}",
        if enabled { "START" } else { "STOP" },
        ident(name)
    ))
}

fn drop_stmt(name: &str) -> Stmt {
    Stmt::plain(format!("DROP PIPE {}", ident(name)))
}

fn running(p: &Pipe) -> bool {
    !p.state.eq_ignore_ascii_case("STOPPED")
}

pub(crate) fn plan(c: &Ctx, current: Option<&Pipe>, values: &FormValues) -> Result<Plan> {
    let d = from_values(values, plugin_keys(current))?;
    let id = format!("pipe:{}", d.name);
    let Some(p) = current else {
        let mut plan = Plan::new(&d.name, id);
        plan.comment(format!("Pipe «{}» de IoTDB", d.name));
        plan.push(create_stmt(&d.name, &d.clauses, None)?);
        if !d.enabled {
            plan.comment("Arranca al crearse: se detiene enseguida (STOP PIPE).");
            plan.push(start_stop(&d.name, false));
        }
        warn_credentials(&mut plan);
        return Ok(plan);
    };
    if jobs::system_pipe(&p.name) {
        return Err(bad(SYSTEM.into()));
    }
    let mut plan = Plan::new(&p.name, id);
    plan.comment(format!("Pipe «{}» de IoTDB", p.name));
    if d.name != p.name {
        if jobs::system_pipe(&d.name) {
            return Err(bad(SYSTEM.into()));
        }
        plan.warn(
            "IoTDB no permite renombrar un pipe: se crea el nuevo y después se borra el anterior.",
        );
        plan.warn(RESTARTS);
        plan.push(create_stmt(&d.name, &d.clauses, Some(p))?);
        if !d.enabled {
            plan.push(start_stop(&d.name, false));
            if !running(p) {
                plan.warn(RUNS_BEFORE_STOP);
            }
        }
        plan.push(drop_stmt(&p.name));
        warn_credentials(&mut plan);
        return Ok(plan);
    }
    let changes: Vec<Change> = CLAUSES
        .iter()
        .enumerate()
        .map(|(i, cl)| diff(*cl, cl.attrs(p), &d.clauses[i]))
        .collect();
    let recreate = CLAUSES
        .iter()
        .zip(&changes)
        .any(|(cl, ch)| *ch != Change::Same && !c.alters(*cl));
    if recreate {
        plan.warn(if c.alters(Clause::Sink) {
            "Esta versión de IoTDB no cambia el origen de un pipe con ALTER PIPE (llegó en la 2.0): se borra y se vuelve a crear."
        } else {
            "Esta versión de IoTDB no tiene ALTER PIPE: el pipe se borra y se vuelve a crear."
        });
        plan.warn(RESTARTS);
        let drop = plan.push(drop_stmt(&p.name));
        let made = plan.push(create_stmt(&p.name, &d.clauses, Some(p))?);
        let mut restore = vec![create_stmt(&p.name, &restore_clauses(p), Some(p))?];
        if !running(p) {
            restore.push(start_stop(&p.name, false));
        }
        plan.recreate = Some(Recreate {
            drop,
            create: made,
            restore,
            what: p.name.clone(),
        });
        if !d.enabled {
            plan.push(start_stop(&p.name, false));
            if !running(p) {
                plan.warn(RUNS_BEFORE_STOP);
            }
        }
        warn_credentials(&mut plan);
        return Ok(plan);
    }
    if changes.iter().any(|ch| *ch != Change::Same) {
        let mut st = Stmt::plain(format!("ALTER PIPE {}", ident(&p.name)));
        for (i, (cl, ch)) in CLAUSES.iter().zip(&changes).enumerate() {
            match ch {
                Change::Same => {}
                Change::Modify(attrs) => append(
                    &mut st,
                    &format!("MODIFY {}", cl.word()),
                    attr_list(*cl, attrs, cl.attrs(p))?,
                ),
                Change::Replace => append(
                    &mut st,
                    &format!("REPLACE {}", cl.word()),
                    attr_list(*cl, &whole(*cl, &d.clauses[i]), cl.attrs(p))?,
                ),
            }
        }
        plan.comment("ALTER PIPE cambia el pipe sin perder su avance (MODIFY suma o cambia atributos; REPLACE reemplaza la parte entera).");
        plan.push(st);
    }
    if d.enabled != running(p) {
        plan.push(start_stop(&p.name, d.enabled));
    }
    warn_credentials(&mut plan);
    Ok(plan)
}

/// A credential the save doesn't have, said in the script before anything
/// runs: one IoTDB 2.x hides (a clause written whole needs it) is a "⚠"
/// line, the user has to type it again; one the form left without a value
/// is a plain comment, the save refuses it.
fn warn_credentials(plan: &mut Plan) {
    let missing: Vec<(String, bool)> = plan
        .stmts
        .iter()
        .filter_map(|st| st.real.as_ref().err().map(|m| (m.clone(), st.hidden)))
        .collect();
    for (m, hidden) in missing {
        if hidden {
            plan.warn(m);
        } else {
            plan.comment(m);
        }
    }
}

pub(crate) fn drop_plan(p: &Pipe) -> Result<Plan> {
    if jobs::system_pipe(&p.name) {
        return Err(bad(SYSTEM.into()));
    }
    let mut plan = Plan::new(&p.name, format!("pipe:{}", p.name));
    plan.comment(format!("Borra el pipe «{}» de IoTDB", p.name));
    plan.push(drop_stmt(&p.name));
    Ok(plan)
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

/// The plugins a clause may use, from `SHOW PIPEPLUGINS`: built-in ones by
/// their class's package, external ones in every clause.
fn plugin_choices(
    plugins: &[super::plugin::Plugin],
    p: Option<&Pipe>,
) -> BTreeMap<String, Vec<(String, String)>> {
    let mut out = BTreeMap::new();
    for c in CLAUSES {
        let mut list: Vec<(String, String)> = plugins
            .iter()
            .filter(|pl| pl.role().is_none_or(|r| r == c))
            .map(|pl| {
                let n = pl.name.to_ascii_lowercase();
                (n.clone(), n)
            })
            .collect();
        if let Some(v) = p.and_then(|p| plugin_of(c, p)) {
            if !list.iter().any(|(x, _)| *x == v) {
                list.push((v.clone(), v));
            }
        }
        list.sort();
        list.dedup();
        out.insert(format!("pipe_plugin:{}", c.key()), list);
    }
    out
}

fn plugin_of(c: Clause, p: &Pipe) -> Option<String> {
    c.attrs(p)
        .iter()
        .find(|(k, _)| c.is_plugin_key(k))
        .map(|(_, v)| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

pub(crate) async fn form(s: &IotDbSession, c: &Ctx, current: Option<&Pipe>) -> Result<ObjectForm> {
    let plugins = match super::plugin_table(s).await {
        Ok(t) => Some(t),
        Err(e @ (Error::Connect(_) | Error::ConnectFailed(_))) => return Err(e),
        Err(_) => None,
    };
    Ok(form_of(c, current, plugins.as_ref()))
}

fn attr_fields(key: &str, value: &str) -> Vec<FormField> {
    vec![
        FormField::new("key", "Atributo", FormKind::Text)
            .required()
            .placeholder(key),
        FormField::new("value", "Valor", FormKind::Text).placeholder(value),
    ]
}

/// The processor's attributes show once a processor is picked: without
/// one IoTDB runs do-nothing-processor, which takes none. `None` (always
/// shown) when the plugins couldn't be listed, or when the pipe has
/// processor attributes without naming a processor (hidden, they wouldn't
/// be sent, and the save would take them out).
fn processor_attrs_when(
    choices: &BTreeMap<String, Vec<(String, String)>>,
    current: Option<&Pipe>,
) -> Option<Vec<String>> {
    let names: Vec<String> = choices
        .get(&format!("pipe_plugin:{}", Clause::Processor.key()))
        .map(|l| l.iter().map(|(v, _)| v.clone()).collect())
        .unwrap_or_default();
    let stray = current.is_some_and(|p| {
        plugin_of(Clause::Processor, p).is_none()
            && Clause::Processor
                .attrs(p)
                .iter()
                .any(|(k, _)| !internal(k) && !Clause::Processor.is_plugin_key(k))
    });
    (!names.is_empty() && !stray).then_some(names)
}

pub(crate) fn form_of(c: &Ctx, current: Option<&Pipe>, plugins: Option<&Table>) -> ObjectForm {
    let mut f = ObjectForm::default();
    let listed = plugins.map(super::plugin::plugins).unwrap_or_default();
    f.choices = plugin_choices(&listed, current);
    let mut processor_attrs = FormList::new(
        "processor_attrs",
        "Atributos del procesamiento",
        "key",
        attr_fields("processor.down-sampling.interval-seconds", "60"),
    );
    if let Some(names) = processor_attrs_when(&f.choices, current) {
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        processor_attrs = processor_attrs.when(Clause::Processor.key(), &names);
    }
    let mut general = vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("El nombre del pipe en el servidor."),
        FormField::new("enabled", "En marcha", FormKind::Bool)
            .default(json!(true))
            .help("Desmarcado, el pipe se detiene (STOP PIPE); marcado, vuelve a arrancar (START PIPE)."),
    ];
    if current.is_some() {
        general.extend([
            FormField::new("state", "Estado", FormKind::Text).read_only(true),
            FormField::new("created", "Creado", FormKind::Text).read_only(true),
            FormField::new("error", "Último error", FormKind::Textarea).read_only(true),
            FormField::new("remaining", "Eventos pendientes", FormKind::Text).read_only(true),
            FormField::new(
                "remaining_secs",
                "Segundos estimados para ponerse al día",
                FormKind::Text,
            )
            .read_only(true),
        ]);
    }
    f.sections = vec![
        FormSection::new("general", "General").fields(general),
        FormSection::new("source", "Origen")
            .field(
                FormField::new("source", "Plugin de origen", FormKind::reference("pipe_plugin:source", false))
                    .help("Vacío: iotdb-source, los datos que llegan a este servidor."),
            )
            .list(FormList::new(
                "source_attrs",
                "Atributos del origen",
                "key",
                attr_fields("source.path", "root.ln.**"),
            )),
        FormSection::new("processor", "Procesamiento")
            .field(
                FormField::new(
                    "processor",
                    "Plugin de procesamiento",
                    FormKind::reference("pipe_plugin:processor", false),
                )
                .help("Vacío: do-nothing-processor, los datos pasan sin cambios."),
            )
            .list(processor_attrs),
        FormSection::new("sink", "Destino")
            .field(
                FormField::new("sink", "Plugin de destino", FormKind::reference("pipe_plugin:sink", false))
                    .required()
                    .help("Adónde van los datos: iotdb-thrift-sink los manda a otro IoTDB (sink.node-urls)."),
            )
            .list(FormList::new(
                "sink_attrs",
                "Atributos del destino",
                "key",
                attr_fields("sink.node-urls", "127.0.0.1:6668"),
            )),
        FormSection::new("secrets", "Credenciales").list(FormList::new(
            "secrets",
            "Credenciales",
            "key",
            vec![
                FormField::new(
                    "clause",
                    "Parte",
                    FormKind::select(&[
                        ("source", "Origen"),
                        ("processor", "Procesamiento"),
                        ("sink", "Destino"),
                    ]),
                )
                .required()
                .default(json!("sink")),
                FormField::new("key", "Atributo", FormKind::Text)
                    .required()
                    .placeholder("sink.password"),
                FormField::new("value", "Valor", FormKind::Secret).required(),
            ],
        )),
    ];
    let Some(p) = current else {
        f.values.insert("enabled".into(), json!(true));
        f.notes.push(NEW_STARTS.into());
        if !c.pipe {
            f.read_only = true;
            f.notes.insert(0, NO_PIPE.into());
        }
        return f;
    };
    f.values.insert("name".into(), json!(p.name));
    f.values.insert("enabled".into(), json!(running(p)));
    f.values.insert("state".into(), json!(p.state));
    f.values.insert("created".into(), json!(p.created));
    f.values.insert("error".into(), json!(p.error));
    f.values.insert("remaining".into(), json!(p.remaining));
    f.values
        .insert("remaining_secs".into(), json!(p.remaining_secs));
    let mut secrets: Vec<BTreeMap<String, Value>> = Vec::new();
    for (i, cl) in CLAUSES.iter().enumerate() {
        f.values.insert(cl.key().into(), json!(plugin_of(*cl, p)));
        let mut list = Vec::new();
        for (k, v) in cl.attrs(p) {
            if internal(k) || cl.is_plugin_key(k) {
                continue;
            }
            if secret_key(k) {
                secrets.push(BTreeMap::from([
                    ("clause".to_string(), json!(cl.key())),
                    ("key".to_string(), json!(k)),
                    ("value".to_string(), Value::Null),
                ]));
            } else {
                list.push(BTreeMap::from([
                    ("key".to_string(), json!(k)),
                    ("value".to_string(), json!(v)),
                ]));
            }
        }
        let section = &mut f.sections[i + 1];
        section.lists[0].rows = list;
    }
    let has_secrets = !secrets.is_empty();
    if let Some(sec) = f.sections.iter_mut().find(|s| s.key == "secrets") {
        sec.lists[0].rows = secrets;
    }
    if jobs::system_pipe(&p.name) {
        f.read_only = true;
        f.notes.push(SYSTEM.into());
        return f;
    }
    f.notes.push(if c.alters(Clause::Source) {
        "Los cambios se aplican con ALTER PIPE: el pipe conserva su avance. Cambiarle el nombre lo borra y lo crea de nuevo.".to_string()
    } else if c.alters(Clause::Sink) {
        "Los cambios del procesamiento y del destino se aplican con ALTER PIPE (el pipe conserva su avance); los del origen y un cambio de nombre lo borran y lo crean de nuevo.".to_string()
    } else {
        "Esta versión de IoTDB no tiene ALTER PIPE: cualquier cambio de su definición borra el pipe y lo crea de nuevo.".to_string()
    });
    if !c.alters(Clause::Source) {
        f.notes.push(RESTARTS.into());
    }
    if has_secrets {
        f.notes.push(KEEP_SECRETS.into());
    }
    if !c.pipe {
        f.read_only = true;
        f.notes.insert(0, NO_PIPE.into());
    }
    f
}

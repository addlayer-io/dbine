//! InfluxDB 1.x continuous queries: `CREATE CONTINUOUS QUERY <name> ON <db>
//! [RESAMPLE [EVERY <d>] [FOR <d>]] BEGIN <SELECT … INTO … GROUP BY
//! time(…)> END` and `DROP CONTINUOUS QUERY <name> ON <db>`.
//!
//! - There is no `ALTER CONTINUOUS QUERY`: a change drops the query and
//!   creates it again (in that order when its name and database stay, so
//!   the name is free; else the new one first). When the new one is
//!   refused after the drop, the old one is created again from the
//!   definition the server had. Nothing else is lost: a continuous query
//!   keeps no state but the time of its last run, held in memory, so the
//!   new one starts at the next interval (`RESAMPLE FOR` recomputes the
//!   ones before).
//! - Each statement goes in its own request, so a refused one is reported
//!   by its own reason.
//! - The history is `_internal`'s `cq_query` (with `query-stats-enabled`):
//!   clearing it deletes the query's series there.

use super::{bad, check_line, duration_ok, shown, Vals};
use crate::jobs::{bad_id, cq_def, pair_id, parse_pair, V1_FIXED, V1_NO_ACCESS, V1_NO_STATS};
use crate::v1::{ident, InfluxQlSession};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerInfo,
    SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::json;

/// Units of an InfluxQL duration literal.
const UNITS: &[&str] = &["ns", "u", "µ", "ms", "s", "m", "h", "d", "w"];

const NO_ALTER: &str = "InfluxDB 1.x no tiene ALTER CONTINUOUS QUERY: al guardar un cambio, la consulta continua se borra y se vuelve a crear. Solo se pierde el momento de su última ejecución: la nueva empieza en el intervalo siguiente (RESAMPLE FOR recalcula los anteriores). Si el servidor rechaza la nueva, DBine vuelve a crear la anterior.";
const NONE_HERE: &str = "Este servidor no tiene consultas continuas: la API compatible con InfluxDB 1.x de InfluxDB 2 y 3 no las tiene.";

fn refused(e: &Error) -> bool {
    match e {
        Error::Query(m) | Error::AuthFailed(m) => {
            m.contains("privilege") || m.contains("authoriz") || m.contains("authentic")
        }
        _ => false,
    }
}

pub(crate) async fn info(s: &mut InfluxQlSession) -> Result<SchedulerInfo> {
    let kind = SchedulerKind::new("job", "Consultas continuas", "consulta continua");
    match s.cqs().await {
        Ok(_) => Ok(SchedulerInfo {
            kinds: vec![kind.abilities(true, true, true)],
            logs: false,
            notes: vec![NO_ALTER.into()],
        }),
        Err(e) if refused(&e) => Ok(SchedulerInfo {
            kinds: vec![kind],
            logs: false,
            notes: vec![V1_NO_ACCESS.into()],
        }),
        Err(e) if e.is_connect() => Err(e),
        Err(_) => Ok(SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![NONE_HERE.into()],
        }),
    }
}

pub(crate) async fn items(s: &mut InfluxQlSession) -> Result<SchedulerList> {
    let items = s
        .cqs()
        .await?
        .iter()
        .map(|(db, name, q)| {
            let c = parse(db, name, q);
            SchedulerItem {
                id: pair_id(db, name),
                name: name.clone(),
                description: Some(match c.schedule() {
                    Some(when) => format!("{db} · {when}"),
                    None => db.clone(),
                }),
                enabled: None,
                system: false,
            }
        })
        .collect();
    Ok(SchedulerList {
        items,
        notes: vec![V1_FIXED.into()],
    })
}

// ---------------------------------------------------------------------------
// The definition
// ---------------------------------------------------------------------------

/// A continuous query as the server keeps it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Cq {
    pub(crate) db: String,
    pub(crate) name: String,
    pub(crate) every: Option<String>,
    pub(crate) for_: Option<String>,
    pub(crate) select: String,
    /// The whole `CREATE CONTINUOUS QUERY` as `SHOW CONTINUOUS QUERIES`
    /// writes it: what puts it back.
    pub(crate) query: String,
}

impl Cq {
    /// "EVERY 30m FOR 2h", or the `GROUP BY time()` interval.
    fn schedule(&self) -> Option<String> {
        let every = self.every.clone().or_else(|| self.group_by())?;
        Some(match &self.for_ {
            Some(f) => format!("EVERY {every} FOR {f}"),
            None => format!("EVERY {every}"),
        })
    }

    fn group_by(&self) -> Option<String> {
        cq_def(&format!(" BEGIN {} END", self.select))
            .group_by_time
            .map(|t| t.split(',').next().unwrap_or(&t).trim().to_string())
    }
}

/// One word or identifier at the start of `s` (a double-quoted one
/// unquoted), and what follows.
fn token(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => out.push(chars.next()?.1),
                '"' => return Some((out, &rest[i + 1..])),
                c => out.push(c),
            }
        }
        return None;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_string(), &s[end..]))
}

/// The header (`CREATE CONTINUOUS QUERY … BEGIN`) read word by word; the
/// SELECT is what's between `BEGIN` and the last `END`.
fn read_header(q: &str) -> Option<(Option<String>, Option<String>, String)> {
    let mut rest = q;
    for word in ["CREATE", "CONTINUOUS", "QUERY"] {
        let (w, r) = token(rest)?;
        if !w.eq_ignore_ascii_case(word) {
            return None;
        }
        rest = r;
    }
    let (_, r) = token(rest)?;
    let (on, r) = token(r)?;
    if !on.eq_ignore_ascii_case("ON") {
        return None;
    }
    let (_, mut rest) = token(r)?;
    let (mut every, mut for_) = (None, None);
    loop {
        let (w, r) = token(rest)?;
        match w.to_ascii_uppercase().as_str() {
            "RESAMPLE" => rest = r,
            "EVERY" => {
                let (d, r) = token(r)?;
                every = Some(d);
                rest = r;
            }
            "FOR" => {
                let (d, r) = token(r)?;
                for_ = Some(d);
                rest = r;
            }
            "BEGIN" => {
                rest = r;
                break;
            }
            _ => return None,
        }
    }
    let body = rest.trim_end().trim_end_matches(';').trim_end();
    let cut = body.len().checked_sub(3)?;
    if !body.is_char_boundary(cut) || !body[cut..].eq_ignore_ascii_case("END") {
        return None;
    }
    Some((every, for_, body[..cut].trim().to_string()))
}

/// A continuous query from `SHOW CONTINUOUS QUERIES`' row.
pub(crate) fn parse(db: &str, name: &str, query: &str) -> Cq {
    let (every, for_, select) = read_header(query).unwrap_or_else(|| {
        let d = cq_def(query);
        (
            d.resample_every,
            d.resample_for,
            d.select.unwrap_or_default(),
        )
    });
    Cq {
        db: db.into(),
        name: name.into(),
        every,
        for_,
        select,
        query: query.trim().trim_end_matches(';').to_string(),
    }
}

/// `'…'` with InfluxQL's escapes.
fn string(v: &str) -> String {
    format!("'{}'", crate::http::escape(v, '\''))
}

/// Where the SELECT ends a statement (a `;` outside strings, identifiers
/// and comments); `None` when it doesn't, the trailing one apart.
fn statement_end(q: &str) -> Option<usize> {
    let b = q.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b';' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The SELECT's words, upper-cased, outside strings and quoted names.
fn words(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = q.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                while let Some(n) = chars.next() {
                    if n == '\\' {
                        chars.next();
                    } else if n == c {
                        break;
                    }
                }
                out.push(std::mem::take(&mut cur));
            }
            c if c.is_alphanumeric() || c == '_' => cur.push(c.to_ascii_uppercase()),
            '(' => {
                out.push(std::mem::take(&mut cur));
                out.push("(".into());
            }
            _ => out.push(std::mem::take(&mut cur)),
        }
    }
    out.push(cur);
    out.retain(|w| !w.is_empty());
    out
}

/// What the designer asks for, checked.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wanted {
    pub(crate) db: String,
    pub(crate) name: String,
    pub(crate) every: Option<String>,
    pub(crate) for_: Option<String>,
    pub(crate) select: String,
}

/// `databases`: the server's, which the database has to be one of.
pub(crate) fn wanted(v: &FormValues, databases: &[String]) -> Result<Wanted> {
    let x = Vals(&v.values);
    let db = x
        .opt("database")
        .ok_or_else(|| bad("Falta la base de datos de la consulta continua.".into()))?;
    if !databases.contains(&db) {
        return Err(bad(format!(
            "No existe la base de datos «{}» en este servidor.",
            shown(&db)
        )));
    }
    let name = x
        .opt("name")
        .ok_or_else(|| bad("La consulta continua necesita un nombre.".into()))?;
    check_line(&name, "Nombre")?;
    let check = |k: &str, label: &str| -> Result<Option<String>> {
        let Some(d) = x.opt(k) else {
            return Ok(None);
        };
        if !duration_ok(&d, UNITS, false) {
            return Err(bad(format!(
                "«{}» en «{label}» no es una duración de InfluxQL (por ejemplo 30m, 1h, 1d).",
                shown(&d)
            )));
        }
        Ok(Some(d))
    };
    let every = check("resample_every", "RESAMPLE EVERY")?;
    let for_ = check("resample_for", "RESAMPLE FOR")?;
    let mut select = x.raw("query").trim().to_string();
    if let Some(stripped) = select.strip_suffix(';') {
        select = stripped.trim_end().to_string();
    }
    if select.is_empty() {
        return Err(bad("La consulta continua necesita su SELECT.".into()));
    }
    if statement_end(&select).is_some() {
        return Err(bad(
            "La consulta de una consulta continua es un solo SELECT: no puede tener «;».".into(),
        ));
    }
    let w = words(&select);
    if w.first().map(String::as_str) != Some("SELECT") {
        return Err(bad("La consulta tiene que empezar con SELECT.".into()));
    }
    if !w.iter().any(|x| x == "INTO") {
        return Err(bad(
            "La consulta necesita INTO: el measurement donde escribe sus resultados.".into(),
        ));
    }
    let grouped = w
        .windows(2)
        .position(|x| x[0] == "GROUP" && x[1] == "BY")
        .is_some_and(|g| w[g..].windows(2).any(|x| x[0] == "TIME" && x[1] == "("));
    if !grouped {
        return Err(bad(
            "La consulta necesita GROUP BY time(…): el intervalo en el que se ejecuta.".into(),
        ));
    }
    Ok(Wanted {
        db,
        name,
        every,
        for_,
        select,
    })
}

/// `CREATE CONTINUOUS QUERY` for `w`, without the `;`.
pub(crate) fn create_statement(w: &Wanted) -> String {
    let mut resample = String::new();
    if w.every.is_some() || w.for_.is_some() {
        resample.push_str(" RESAMPLE");
        if let Some(e) = &w.every {
            resample.push_str(&format!(" EVERY {e}"));
        }
        if let Some(f) = &w.for_ {
            resample.push_str(&format!(" FOR {f}"));
        }
    }
    // END on its own line: a `--` comment at the SELECT's end doesn't hide
    // it.
    format!(
        "CREATE CONTINUOUS QUERY {} ON {}{resample}\nBEGIN\n{}\nEND",
        ident(&w.name),
        ident(&w.db),
        w.select
    )
}

pub(crate) fn drop_statement(db: &str, name: &str) -> String {
    format!("DROP CONTINUOUS QUERY {} ON {}", ident(name), ident(db))
}

/// What a save runs: the statements in order, the definition to put back
/// when the new one is refused after the drop, and the id it ends with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub(crate) header: Vec<String>,
    pub(crate) statements: Vec<String>,
    pub(crate) restore: Option<String>,
    pub(crate) id: String,
}

impl Plan {
    pub(crate) fn script(&self) -> String {
        let mut out = String::new();
        for h in &self.header {
            out.push_str(&format!("-- {}\n", super::one_line(h)));
        }
        for s in &self.statements {
            out.push_str(s);
            out.push_str(";\n");
        }
        out
    }
}

pub(crate) fn create_plan(w: &Wanted) -> Plan {
    Plan {
        header: vec![format!(
            "Crea la consulta continua «{}» en la base de datos «{}».",
            w.name, w.db
        )],
        statements: vec![create_statement(w)],
        restore: None,
        id: pair_id(&w.db, &w.name),
    }
}

pub(crate) fn edit_plan(old: &Cq, w: &Wanted) -> Plan {
    let id = pair_id(&w.db, &w.name);
    let same_place = w.db == old.db && w.name == old.name;
    let unchanged =
        same_place && w.every == old.every && w.for_ == old.for_ && w.select == old.select;
    if unchanged {
        return Plan {
            header: vec![format!("«{}»: no hay cambios que guardar.", old.name)],
            statements: Vec::new(),
            restore: None,
            id,
        };
    }
    let mut header = vec![
        "InfluxDB 1.x no tiene ALTER CONTINUOUS QUERY: la consulta continua se borra y se vuelve a crear.".to_string(),
        "Se pierde el momento de su última ejecución: la nueva empieza en el intervalo siguiente (RESAMPLE FOR recalcula los anteriores).".to_string(),
    ];
    if same_place {
        header.push(
            "Si el servidor rechaza la nueva, DBine vuelve a crear la anterior tal como estaba."
                .into(),
        );
        Plan {
            header,
            statements: vec![drop_statement(&old.db, &old.name), create_statement(w)],
            restore: Some(old.query.clone()),
            id,
        }
    } else {
        header.push(format!(
            "Primero crea «{}» en «{}» y después borra «{}» de «{}».",
            w.name, w.db, old.name, old.db
        ));
        Plan {
            header,
            statements: vec![create_statement(w), drop_statement(&old.db, &old.name)],
            restore: None,
            id,
        }
    }
}

// ---------------------------------------------------------------------------
// The calls
// ---------------------------------------------------------------------------

async fn databases(s: &mut InfluxQlSession) -> Result<Vec<String>> {
    let r = s.query("SHOW DATABASES").await?;
    Ok(r.iter()
        .filter_map(|x| x.get("series").and_then(|s| s.as_array()))
        .flatten()
        .filter_map(|s| s.get("values").and_then(|v| v.as_array()))
        .flatten()
        .filter_map(|v| v.get(0).and_then(|n| n.as_str()).map(str::to_string))
        .collect())
}

async fn read(s: &mut InfluxQlSession, id: &str) -> Result<Cq> {
    parse_pair(id).ok_or_else(|| bad_id(id))?;
    let (db, name, q) = s.cq(id).await?;
    Ok(parse(&db, &name, &q))
}

pub(crate) fn form_of(old: Option<&Cq>, databases: &[String]) -> ObjectForm {
    let mut f = ObjectForm::default();
    let c = old.cloned().unwrap_or_default();
    let v = &mut f.values;
    v.insert("database".into(), json!(c.db));
    v.insert("name".into(), json!(c.name));
    v.insert(
        "resample_every".into(),
        json!(c.every.clone().unwrap_or_default()),
    );
    v.insert(
        "resample_for".into(),
        json!(c.for_.clone().unwrap_or_default()),
    );
    v.insert("group_by".into(), json!(c.group_by().unwrap_or_default()));
    v.insert("query".into(), json!(c.select));
    let mut dbs: Vec<(String, String)> = databases
        .iter()
        .filter(|d| !d.starts_with('_') || **d == c.db)
        .map(|d| (d.clone(), d.clone()))
        .collect();
    if !c.db.is_empty() && !dbs.iter().any(|(d, _)| *d == c.db) {
        dbs.push((c.db.clone(), c.db.clone()));
    }
    f.choices.insert("database".into(), dbs);
    let mut schedule = vec![
        FormField::new("resample_every", "Cada (RESAMPLE EVERY)", FormKind::Text)
            .placeholder("30m")
            .help("Cada cuánto se ejecuta. Vacío: en cada intervalo del GROUP BY time()."),
        FormField::new(
            "resample_for",
            "Período que recalcula (RESAMPLE FOR)",
            FormKind::Text,
        )
        .placeholder("2h")
        .help("Cuánto tiempo hacia atrás recalcula en cada ejecución; no puede ser menor que el intervalo del GROUP BY time(). Vacío: solo el último intervalo."),
    ];
    if old.is_some() {
        schedule.push(
            FormField::new("group_by", "Intervalo (GROUP BY time)", FormKind::Text)
                .read_only(true)
                .help("Sale del GROUP BY time() de la consulta."),
        );
    }
    f.sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new(
                "database",
                "Base de datos",
                FormKind::reference("database", false),
            )
            .required(),
            FormField::new("name", "Nombre", FormKind::Text).required(),
        ]),
        FormSection::new("schedule", "Programación").fields(schedule),
        FormSection::new("query", "Consulta").field(
            FormField::new(
                "query",
                "SELECT … INTO … GROUP BY time(…)",
                FormKind::code("sql"),
            )
            .required()
            .placeholder("SELECT mean(\"value\") INTO \"cpu_1h\" FROM \"cpu\" GROUP BY time(1h), *")
            .help("Un solo SELECT con INTO (dónde escribe) y GROUP BY time() (cada cuánto)."),
        ),
    ];
    if old.is_some() {
        f.notes.push(NO_ALTER.into());
    }
    f
}

pub(crate) async fn form(s: &mut InfluxQlSession, id: Option<&str>) -> Result<ObjectForm> {
    let old = match id {
        Some(id) => Some(read(s, id).await?),
        None => None,
    };
    let dbs = databases(s).await?;
    Ok(form_of(old.as_ref(), &dbs))
}

async fn plan(s: &mut InfluxQlSession, id: Option<&str>, v: &FormValues) -> Result<Plan> {
    let old = match id {
        Some(id) => Some(read(s, id).await?),
        None => None,
    };
    let w = wanted(v, &databases(s).await?)?;
    Ok(match &old {
        None => create_plan(&w),
        Some(old) => edit_plan(old, &w),
    })
}

pub(crate) async fn script(
    s: &mut InfluxQlSession,
    id: Option<&str>,
    v: &FormValues,
) -> Result<String> {
    Ok(plan(s, id, v).await?.script())
}

pub(crate) async fn save(
    s: &mut InfluxQlSession,
    id: Option<&str>,
    v: &FormValues,
) -> Result<Option<String>> {
    let p = plan(s, id, v).await?;
    for (i, st) in p.statements.iter().enumerate() {
        if let Err(e) = s.write_statement(st).await {
            if i == 0 {
                return Err(e);
            }
            return Err(match &p.restore {
                Some(old) => match s.write_statement(old).await {
                    Ok(()) => Error::Query(format!(
                        "No se guardó el cambio: {e}. Se volvió a crear la consulta continua anterior."
                    )),
                    Err(e2) => Error::Query(format!(
                        "No se guardó el cambio: {e}. Tampoco se pudo volver a crear la consulta continua anterior ({e2}); su definición era: {old}"
                    )),
                },
                None => Error::Query(format!(
                    "Se creó la consulta continua nueva, pero no se pudo borrar la anterior: {e}"
                )),
            });
        }
    }
    Ok(Some(p.id))
}

pub(crate) fn drop_plan(c: &Cq) -> Plan {
    Plan {
        header: vec![format!(
            "Borra la consulta continua «{}» de la base de datos «{}».",
            c.name, c.db
        )],
        statements: vec![drop_statement(&c.db, &c.name)],
        restore: None,
        id: pair_id(&c.db, &c.name),
    }
}

pub(crate) async fn delete_script(s: &mut InfluxQlSession, id: &str) -> Result<String> {
    Ok(drop_plan(&read(s, id).await?).script())
}

pub(crate) async fn delete(s: &mut InfluxQlSession, id: &str) -> Result<()> {
    let c = read(s, id).await?;
    s.write_statement(&drop_statement(&c.db, &c.name)).await
}

/// The statement that deletes the query's runs from `_internal`.
pub(crate) fn clear_statement(c: &Cq) -> String {
    format!(
        "DELETE FROM \"cq_query\" WHERE \"db\" = {} AND \"cq\" = {}",
        string(&c.db),
        string(&c.name)
    )
}

/// Deletes the query's runs that `query-stats-enabled` recorded in
/// `_internal`.
pub(crate) async fn clear_history(s: &mut InfluxQlSession, id: &str) -> Result<()> {
    let c = read(s, id).await?;
    if !s.cq_stats().await {
        return Err(Error::Unsupported(V1_NO_STATS.into()));
    }
    s.write_on("_internal", &clear_statement(&c)).await
}

/// Why a continuous query can't start at a step.
pub(crate) fn no_steps() -> Error {
    Error::Unsupported(V1_FIXED.into())
}

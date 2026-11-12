//! Continuous queries: `CREATE CONTINUOUS QUERY <id> [RESAMPLE [EVERY …]
//! [BOUNDARY …] [RANGE start[, end]]] [TIMEOUT POLICY BLOCKED | DISCARD]
//! BEGIN SELECT … INTO … END` and `DROP CQ`. `SHOW CQS` gives back the
//! statement as it was written, which the form is read from. IoTDB has no
//! `ALTER` for them and no way to pause one: a change drops the query and
//! creates it again.

use super::{bad, duration, ident, instant, name_field, shown, Ctx, Plan, Recreate, Stmt, Vals};
use crate::jobs::{self, keyword};
use dbine_driver::{FormField, FormKind, FormSection, FormValues, ObjectForm, Result};
use serde_json::json;

const RECREATE_NOTE: &str = "IoTDB no tiene ALTER para las consultas continuas: al guardar un cambio, DBine la borra (DROP CQ) y la vuelve a crear. No se pierde un historial (IoTDB no guarda uno); las ejecuciones se cuentan de nuevo desde BOUNDARY.";
const ALWAYS_ACTIVE: &str =
    "Una consulta continua no se puede pausar ni ejecutar a pedido: corre mientras exista.";
const NEW_IS_CQ: &str =
    "Un job nuevo de IoTDB es una consulta continua. Los pipes se crean desde «Pipes».";
const UNREADABLE: &str = "DBine no pudo leer la sentencia de esta consulta continua: se muestra como está y no se puede cambiar desde el diseñador.";

/// A continuous query's definition.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Def {
    pub(crate) name: String,
    pub(crate) every: Option<String>,
    pub(crate) boundary: Option<String>,
    pub(crate) range_start: Option<String>,
    pub(crate) range_end: Option<String>,
    /// `TIMEOUT POLICY DISCARD` (BLOCKED is the default).
    pub(crate) discard: bool,
    /// The `SELECT … INTO …` between `BEGIN` and `END`.
    pub(crate) query: String,
}

/// The last whole-word, unquoted `kw` in `sql`.
fn last_keyword(sql: &str, kw: &str) -> Option<usize> {
    let mut at = None;
    let mut from = 0;
    while let Some(i) = keyword(&sql[from..], kw) {
        at = Some(from + i);
        from += i + kw.len();
    }
    at
}

/// `s` without its first word when that word is `w` (any case).
fn strip_word<'a>(s: &'a str, w: &str) -> Option<&'a str> {
    let s = s.trim_start();
    let head = s.get(..w.len())?;
    let after = s[w.len()..].chars().next();
    (head.eq_ignore_ascii_case(w) && after.is_none_or(char::is_whitespace)).then(|| &s[w.len()..])
}

/// The name at the start of `s` (backquoted, or up to a space) and the rest.
fn take_name(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('`') {
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '`' {
                if chars.peek().map(|(_, n)| *n) == Some('`') {
                    chars.next();
                    name.push('`');
                    continue;
                }
                return Some((name, &rest[i + 1..]));
            }
            name.push(c);
        }
        return None;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_string(), &s[end..]))
}

/// The statement `SHOW CQS` gives back, read into its parts.
pub(crate) fn parse(stmt: &str) -> Option<Def> {
    let sql = stmt.trim().trim_end_matches(';').trim_end();
    let begin = keyword(sql, "begin")?;
    let end = last_keyword(sql, "end")?;
    if end <= begin || !sql[end + 3..].trim().is_empty() {
        return None;
    }
    let query = sql[begin + "begin".len()..end].trim().to_string();
    let rest = strip_word(&sql[..begin], "create")?;
    let rest = match strip_word(rest, "cq") {
        Some(r) => r,
        None => strip_word(strip_word(rest, "continuous")?, "query")?,
    };
    let (name, rest) = take_name(rest)?;
    let mut d = Def {
        name,
        query,
        ..Def::default()
    };
    // Each clause's text runs up to the next clause.
    let words = ["resample", "every", "boundary", "range", "timeout"];
    let at: Vec<(usize, &str)> = words
        .iter()
        .filter_map(|w| keyword(rest, w).map(|i| (i, *w)))
        .collect();
    let span = |w: &str| -> Option<String> {
        let (i, _) = at.iter().find(|(_, x)| *x == w)?;
        let from = i + w.len();
        let to = at
            .iter()
            .map(|(j, _)| *j)
            .filter(|j| *j > *i)
            .min()
            .unwrap_or(rest.len());
        Some(rest[from..to].trim().to_string())
    };
    d.every = span("every").filter(|v| !v.is_empty());
    d.boundary = span("boundary").filter(|v| !v.is_empty());
    if let Some(r) = span("range") {
        let mut parts = r.splitn(2, ',').map(|p| p.trim().to_string());
        d.range_start = parts.next().filter(|p| !p.is_empty());
        d.range_end = parts.next().filter(|p| !p.is_empty());
    }
    if let Some(t) = span("timeout") {
        let policy = strip_word(&t, "policy")?.trim();
        d.discard = policy.eq_ignore_ascii_case("discard");
    }
    Some(d)
}

/// The definition with its values in the case and spacing IoTDB reads the
/// same, to compare two.
fn normalized(d: &Def) -> Def {
    let low = |v: &Option<String>| v.as_ref().map(|x| x.to_ascii_lowercase());
    Def {
        name: d.name.clone(),
        every: low(&d.every),
        boundary: d.boundary.as_ref().map(|b| b.replacen(' ', "T", 1)),
        range_start: low(&d.range_start),
        range_end: low(&d.range_end).filter(|e| e.trim_start_matches('0') != "s"),
        discard: d.discard,
        query: d.query.trim().to_string(),
    }
}

pub(crate) fn create(d: &Def) -> String {
    let mut out = format!("CREATE CONTINUOUS QUERY {}", ident(&d.name));
    let mut resample = Vec::new();
    if let Some(e) = &d.every {
        resample.push(format!("EVERY {e}"));
    }
    if let Some(b) = &d.boundary {
        resample.push(format!("BOUNDARY {b}"));
    }
    if let Some(r) = &d.range_start {
        resample.push(match &d.range_end {
            Some(e) => format!("RANGE {r}, {e}"),
            None => format!("RANGE {r}"),
        });
    }
    if !resample.is_empty() {
        out.push_str("\nRESAMPLE ");
        out.push_str(&resample.join(" "));
    }
    if d.discard {
        out.push_str("\nTIMEOUT POLICY DISCARD");
    }
    out.push_str("\nBEGIN\n");
    out.push_str(d.query.trim());
    out.push_str("\nEND");
    out
}

/// The query between `BEGIN` and `END`: one `SELECT … INTO` statement,
/// its quotes and parentheses closed.
pub(crate) fn check_query(q: &str) -> Result<String> {
    let q = q.trim().trim_end_matches(';').trim_end();
    if q.is_empty() {
        return Err(bad(
            "La consulta continua necesita su consulta (SELECT … INTO …).".into(),
        ));
    }
    let lower = q.to_ascii_lowercase();
    if !lower.starts_with("select") || keyword(q, "into").is_none() {
        return Err(bad(
            "La consulta de una consulta continua es un SELECT … INTO … (dónde escribe lo que calcula).".into(),
        ));
    }
    let mut quote: Option<char> = None;
    let mut depth: i32 = 0;
    let mut chars = q.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(_) if c == '\\' => {
                chars.next();
            }
            Some(qc) if c == qc => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '(' => depth += 1,
                ')' => depth -= 1,
                ';' => {
                    return Err(bad(
                        "La consulta de una consulta continua es una sola sentencia (sin «;»)."
                            .into(),
                    ))
                }
                _ => {}
            },
        }
        if depth < 0 {
            break;
        }
    }
    if quote.is_some() || depth != 0 {
        return Err(bad(
            "La consulta de la consulta continua tiene comillas o paréntesis sin cerrar.".into(),
        ));
    }
    Ok(q.to_string())
}

pub(crate) fn from_values(values: &FormValues) -> Result<Def> {
    let v = Vals(&values.values);
    let name = name_field(v, "La consulta continua")?;
    let every = v
        .opt("every")
        .map(|e| duration(&e, "Cada (EVERY)"))
        .transpose()?;
    let boundary = v
        .opt("boundary")
        .map(|b| instant(&b, "Desde (BOUNDARY)"))
        .transpose()?;
    let range_start = v
        .opt("range_start")
        .map(|r| duration(&r, "Ventana: desde hace"))
        .transpose()?;
    let range_end = v
        .opt("range_end")
        .map(|r| duration(&r, "Ventana: hasta hace"))
        .transpose()?;
    if range_end.is_some() && range_start.is_none() {
        return Err(bad(
            "«Ventana: hasta hace» necesita también «Ventana: desde hace».".into(),
        ));
    }
    let discard = match v.opt("timeout_policy").as_deref() {
        None | Some("blocked") => false,
        Some("discard") => true,
        Some(other) => {
            return Err(bad(format!(
                "«{}» no es una política de IoTDB (blocked o discard).",
                shown(other)
            )))
        }
    };
    Ok(Def {
        name,
        every,
        boundary,
        range_start,
        range_end,
        discard,
        query: check_query(&v.raw("query"))?,
    })
}

pub(crate) fn form(c: &Ctx, current: Option<&jobs::Cq>) -> ObjectForm {
    let mut f = ObjectForm::default();
    let existing = current.is_some();
    let def = current.and_then(|cq| cq.query.as_deref().and_then(parse));
    let unreadable = existing && def.is_none();
    let mut general = vec![FormField::new("name", "Nombre", FormKind::Text)
        .required()
        .help("El identificador de la consulta continua en el servidor.")];
    if existing {
        general.push(FormField::new("state", "Estado", FormKind::Text).read_only(true));
    }
    f.sections = vec![
        FormSection::new("general", "General").fields(general),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("every", "Cada (EVERY)", FormKind::Text)
                .placeholder("1h")
                .help("Cada cuánto se ejecuta (30s, 5m, 1h30m, 1d…). Vacío: el intervalo del GROUP BY de la consulta."),
            FormField::new("boundary", "Desde (BOUNDARY)", FormKind::Text)
                .placeholder("2024-01-01T00:00:00")
                .help("Desde cuándo se cuentan las ejecuciones: fecha y hora, o un instante en la precisión del servidor. Vacío: 0 (1970-01-01)."),
            FormField::new("range_start", "Ventana: desde hace (RANGE)", FormKind::Text)
                .placeholder("1h")
                .help("Cada ejecución consulta desde ahora menos este intervalo. Vacío: el valor de EVERY."),
            FormField::new("range_end", "Ventana: hasta hace", FormKind::Text)
                .placeholder("0s")
                .help("…y hasta ahora menos este intervalo. Vacío: hasta ahora."),
            FormField::new(
                "timeout_policy",
                "Si una ejecución no termina a tiempo (TIMEOUT POLICY)",
                FormKind::select(&[
                    ("blocked", "Esperarla (BLOCKED)"),
                    ("discard", "Saltear la siguiente (DISCARD)"),
                ]),
            )
            .default(json!("blocked")),
        ]),
        FormSection::new("query", "Consulta").field(
            FormField::new("query", "Consulta", FormKind::code("sql"))
                .required()
                .placeholder("SELECT count(s1) INTO root.ln.agg(c) FROM root.ln.d1 GROUP BY(10m)")
                .help("Un SELECT … INTO: lo que calcula y la serie donde lo escribe."),
        ),
    ];
    match (current, &def) {
        (Some(cq), Some(d)) => {
            f.values.insert("name".into(), json!(d.name));
            f.values.insert("state".into(), json!(cq.state));
            f.values.insert("every".into(), json!(d.every));
            f.values.insert("boundary".into(), json!(d.boundary));
            f.values.insert("range_start".into(), json!(d.range_start));
            f.values.insert("range_end".into(), json!(d.range_end));
            f.values.insert(
                "timeout_policy".into(),
                json!(if d.discard { "discard" } else { "blocked" }),
            );
            f.values.insert("query".into(), json!(d.query));
            f.notes.push(RECREATE_NOTE.into());
            f.notes.push(ALWAYS_ACTIVE.into());
        }
        (Some(cq), None) => {
            f.values.insert("name".into(), json!(cq.id));
            f.values.insert("state".into(), json!(cq.state));
            f.values.insert("query".into(), json!(cq.query));
            f.notes.push(UNREADABLE.into());
        }
        (None, _) => {
            f.notes.push(NEW_IS_CQ.into());
            f.notes.push(ALWAYS_ACTIVE.into());
        }
    }
    if unreadable {
        f.read_only = true;
    } else if !c.cq {
        f.read_only = true;
        f.notes.insert(0, super::NO_CQ.into());
    }
    f
}

pub(crate) fn plan(current: Option<&jobs::Cq>, values: &FormValues) -> Result<Plan> {
    let d = from_values(values)?;
    let id = format!("cq:{}", d.name);
    let Some(cq) = current else {
        let mut p = Plan::new(&d.name, id);
        p.comment(format!("Consulta continua «{}» de IoTDB", d.name));
        p.push(Stmt::plain(create(&d)));
        return Ok(p);
    };
    let old = cq.query.as_deref().and_then(parse);
    let Some(old) = old else {
        return Err(bad(UNREADABLE.into()));
    };
    let mut p = Plan::new(&cq.id, id);
    if normalized(&old) == normalized(&d) {
        return Ok(p);
    }
    p.comment(format!("Consulta continua «{}» de IoTDB", cq.id));
    p.comment("IoTDB no tiene ALTER para las consultas continuas: se borra y se vuelve a crear.");
    p.comment("No se pierde un historial (IoTDB no guarda uno); las ejecuciones se cuentan de nuevo desde BOUNDARY.");
    let drop = p.push(Stmt::plain(drop_sql(&cq.id)));
    let made = p.push(Stmt::plain(create(&d)));
    let restore = cq
        .query
        .as_deref()
        .map(|q| q.trim().trim_end_matches(';').to_string())
        .unwrap_or_else(|| create(&old));
    p.recreate = Some(Recreate {
        drop,
        create: made,
        restore: vec![Stmt::plain(restore)],
        what: cq.id.clone(),
    });
    Ok(p)
}

pub(crate) fn drop_sql(id: &str) -> String {
    format!("DROP CQ {}", ident(id))
}

pub(crate) fn drop_plan(cq: &jobs::Cq) -> Plan {
    let mut p = Plan::new(&cq.id, format!("cq:{}", cq.id));
    p.comment(format!("Borra la consulta continua «{}» de IoTDB", cq.id));
    p.push(Stmt::plain(drop_sql(&cq.id)));
    p
}

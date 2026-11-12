//! The text formats of the designer's multi-value fields, both ways:
//! `clave=valor` lines for the API's string maps, one item per line for its
//! string lists, and a line per library, health rule or subscriber. What a
//! line can't say without losing something (a value with a line break,
//! spaces at its ends, a shape this version doesn't know) is written as
//! JSON instead, and read back as JSON: a server value always survives the
//! round trip.

use super::{bad, shown};
use dbine_driver::Result;
use serde_json::{json, Map, Value as Json};

/// Text that a line would change (trimmed, split, empty).
fn unsafe_on_a_line(s: &str) -> bool {
    s.is_empty() || s.trim() != s || s.contains('\n') || s.contains('\r')
}

/// A JSON value as its own text (a string without quotes).
fn scalar_text(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// String maps: base parameters, tags, Spark conf…
// ---------------------------------------------------------------------------

/// `clave=valor` per line, or the map as JSON when a line can't hold it.
pub(crate) fn render_map(m: &Map<String, Json>) -> String {
    let lossy = m.iter().any(|(k, v)| {
        unsafe_on_a_line(k)
            || k.contains('=')
            || k.starts_with('{')
            || match v {
                Json::String(s) => !s.is_empty() && unsafe_on_a_line(s),
                _ => true,
            }
    });
    if lossy {
        return serde_json::to_string_pretty(m).unwrap_or_default();
    }
    m.iter()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `clave=valor` lines (a line without `=` is a key with an empty value) or
/// a JSON object of strings. `None`: nothing written.
pub(crate) fn parse_map(text: &str, label: &str) -> Result<Option<Map<String, Json>>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let mut out = Map::new();
    if t.starts_with('{') {
        let v: Json = serde_json::from_str(t)
            .map_err(|e| bad(format!("«{label}» no es un objeto JSON válido: {e}")))?;
        let Json::Object(m) = v else {
            return Err(bad(format!("«{label}» tiene que ser un objeto JSON.")));
        };
        for (k, v) in m {
            if scalar_text(&v).is_none() {
                return Err(bad(format!(
                    "«{label}»: el valor de «{}» tiene que ser un texto.",
                    shown(&k)
                )));
            }
            out.insert(k, v);
        }
        return Ok(Some(out));
    }
    for line in t.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once('=').unwrap_or((line, ""));
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() {
            return Err(bad(format!(
                "«{label}»: la línea «{}» no tiene clave (clave=valor).",
                shown(line)
            )));
        }
        if out.insert(k.to_string(), json!(v)).is_some() {
            return Err(bad(format!(
                "«{label}»: la clave «{}» está más de una vez.",
                shown(k)
            )));
        }
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------------------
// Argument lists: a script's or a JAR's parameters
// ---------------------------------------------------------------------------

/// One argument per line, or a JSON array when a line can't hold one.
pub(crate) fn render_args(a: &[Json]) -> String {
    let lossy = a.iter().any(|v| match v {
        Json::String(s) => unsafe_on_a_line(s),
        _ => true,
    }) || a
        .first()
        .and_then(Json::as_str)
        .is_some_and(|s| s.starts_with('['));
    if lossy {
        return serde_json::to_string_pretty(a).unwrap_or_default();
    }
    a.iter()
        .filter_map(Json::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One argument per line, or a JSON array of strings.
pub(crate) fn parse_args(text: &str, label: &str) -> Result<Option<Vec<Json>>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.starts_with('[') {
        let v: Json = serde_json::from_str(t)
            .map_err(|e| bad(format!("«{label}» no es una lista JSON válida: {e}")))?;
        let Json::Array(items) = v else {
            return Err(bad(format!("«{label}» tiene que ser una lista JSON.")));
        };
        if items.iter().any(|v| scalar_text(v).is_none()) {
            return Err(bad(format!(
                "«{label}»: cada elemento de la lista tiene que ser un texto."
            )));
        }
        return Ok(Some(items));
    }
    Ok(Some(
        t.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| json!(l))
            .collect(),
    ))
}

/// Items one per line (table names, dependencies, dbt commands).
pub(crate) fn render_list(a: &[Json]) -> String {
    a.iter()
        .filter_map(scalar_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Items one per line, trimmed, without empty lines.
pub(crate) fn parse_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Mail addresses, one per line (commas and semicolons also separate them).
pub(crate) fn parse_emails(text: &str, label: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for a in text
        .split(['\n', ',', ';'])
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        if !a.contains('@') || a.chars().any(char::is_whitespace) {
            return Err(bad(format!(
                "«{label}»: «{}» no es una dirección de correo.",
                shown(a)
            )));
        }
        if !out.iter().any(|x| x == a) {
            out.push(a.to_string());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Libraries
// ---------------------------------------------------------------------------

/// The libraries of a file path: the line is the kind and the path.
const PATH_LIBRARIES: [&str; 4] = ["jar", "egg", "whl", "requirements"];

/// A package library (`pypi`, `cran`, `maven`) as a line, if it fits one.
fn package_line(kind: &str, main_key: &str, body: &Json) -> Option<String> {
    let o = body.as_object()?;
    let known = ["package", "coordinates", "repo", "exclusions"];
    if o.keys().any(|k| !known.contains(&k.as_str())) {
        return None;
    }
    let main = o.get(main_key)?.as_str()?;
    let token_ok = |s: &str| !s.is_empty() && !s.chars().any(char::is_whitespace);
    if !token_ok(main) {
        return None;
    }
    let mut line = format!("{kind}:{main}");
    if let Some(r) = o.get("repo") {
        let r = r.as_str()?;
        if !token_ok(r) {
            return None;
        }
        line.push_str(&format!(" repo={r}"));
    }
    if let Some(ex) = o.get("exclusions") {
        let ex: Vec<&str> = ex
            .as_array()?
            .iter()
            .map(Json::as_str)
            .collect::<Option<_>>()?;
        if ex.iter().any(|e| !token_ok(e) || e.contains(',')) || kind != "maven" {
            return None;
        }
        if !ex.is_empty() {
            line.push_str(&format!(" exclusions={}", ex.join(",")));
        }
    }
    Some(line)
}

/// A library as its line: `pypi:pandas==2.2 repo=…`, `maven:g:a:v
/// exclusions=…`, `cran:pkg`, `jar:/Volumes/…`, or JSON.
pub(crate) fn render_library(lib: &Json) -> String {
    let line = lib.as_object().filter(|o| o.len() == 1).and_then(|o| {
        let (kind, body) = o.iter().next()?;
        match kind.as_str() {
            "pypi" | "cran" => package_line(kind, "package", body),
            "maven" => package_line(kind, "coordinates", body),
            k if PATH_LIBRARIES.contains(&k) => {
                let p = body.as_str()?;
                (!unsafe_on_a_line(p)).then(|| format!("{k}:{p}"))
            }
            _ => None,
        }
    });
    line.unwrap_or_else(|| lib.to_string())
}

pub(crate) fn parse_library(line: &str) -> Result<Json> {
    const LABEL: &str = "Bibliotecas";
    let line = line.trim();
    if line.starts_with('{') {
        let v: Json = serde_json::from_str(line).map_err(|e| {
            bad(format!(
                "«{LABEL}»: «{}» no es un objeto JSON válido: {e}",
                shown(line)
            ))
        })?;
        return match v {
            Json::Object(o) if !o.is_empty() => Ok(Json::Object(o)),
            _ => Err(bad(format!(
                "«{LABEL}»: «{}» tiene que ser un objeto JSON.",
                shown(line)
            ))),
        };
    }
    let unknown = || {
        bad(format!(
            "«{LABEL}»: «{}» no es una biblioteca (pypi:, maven:, cran:, jar:, whl:, egg:, requirements: o un objeto JSON).",
            shown(line)
        ))
    };
    let (kind, rest) = line.split_once(':').ok_or_else(unknown)?;
    let kind = kind.trim().to_ascii_lowercase();
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(unknown());
    }
    if PATH_LIBRARIES.contains(&kind.as_str()) {
        return Ok(json!({ kind: rest }));
    }
    let main_key = match kind.as_str() {
        "pypi" | "cran" => "package",
        "maven" => "coordinates",
        _ => return Err(unknown()),
    };
    let mut tokens = rest.split_whitespace();
    let mut body = Map::new();
    body.insert(main_key.into(), json!(tokens.next().unwrap_or_default()));
    for t in tokens {
        match t.split_once('=') {
            Some(("repo", r)) if !r.is_empty() => {
                body.insert("repo".into(), json!(r));
            }
            Some(("exclusions", e)) if kind == "maven" && !e.is_empty() => {
                let ex: Vec<&str> = e.split(',').filter(|x| !x.is_empty()).collect();
                body.insert("exclusions".into(), json!(ex));
            }
            _ => {
                return Err(bad(format!(
                    "«{LABEL}»: «{}» no es una opción de la biblioteca (repo=… o, en maven, exclusions=…).",
                    shown(t)
                )))
            }
        }
    }
    Ok(json!({ kind: Json::Object(body) }))
}

pub(crate) fn render_libraries(libs: &[Json]) -> String {
    libs.iter()
        .map(render_library)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn parse_libraries(text: &str) -> Result<Option<Vec<Json>>> {
    let libs = parse_list(text)
        .iter()
        .map(|l| parse_library(l))
        .collect::<Result<Vec<_>>>()?;
    Ok((!libs.is_empty()).then_some(libs))
}

// ---------------------------------------------------------------------------
// Health rules
// ---------------------------------------------------------------------------

/// The metrics a health rule watches.
pub(crate) const HEALTH_METRICS: [&str; 5] = [
    "RUN_DURATION_SECONDS",
    "STREAMING_BACKLOG_BYTES",
    "STREAMING_BACKLOG_RECORDS",
    "STREAMING_BACKLOG_SECONDS",
    "STREAMING_BACKLOG_FILES",
];

/// A rule as its line (`RUN_DURATION_SECONDS > 3600`), or JSON.
pub(crate) fn render_rule(r: &Json) -> String {
    let fits = r.as_object().is_some_and(|o| {
        o.len() == 3
            && o.get("op").and_then(Json::as_str) == Some("GREATER_THAN")
            && o.get("value").is_some_and(Json::is_i64)
            && o.get("metric")
                .and_then(Json::as_str)
                .is_some_and(|m| HEALTH_METRICS.contains(&m))
    });
    if fits {
        format!(
            "{} > {}",
            r["metric"].as_str().unwrap_or_default(),
            r["value"]
        )
    } else {
        r.to_string()
    }
}

/// One rule: the metric's name, `>` and a whole number.
pub(crate) fn rule(metric: &str, value: i64, where_: &str) -> Result<Json> {
    let m = metric.trim().to_ascii_uppercase();
    if !HEALTH_METRICS.contains(&m.as_str()) {
        return Err(bad(format!(
            "{where_}: «{}» no es una métrica de salud ({}).",
            shown(metric),
            HEALTH_METRICS.join(", ")
        )));
    }
    if value < 0 {
        return Err(bad(format!(
            "{where_}: el umbral de «{m}» no puede ser negativo."
        )));
    }
    Ok(json!({ "metric": m, "op": "GREATER_THAN", "value": value }))
}

pub(crate) fn parse_rules(text: &str, where_: &str) -> Result<Vec<Json>> {
    parse_list(text)
        .iter()
        .map(|line| {
            if line.starts_with('{') {
                return serde_json::from_str::<Json>(line)
                    .ok()
                    .filter(Json::is_object)
                    .ok_or_else(|| {
                        bad(format!(
                            "{where_}: «{}» no es un objeto JSON válido.",
                            shown(line)
                        ))
                    });
            }
            let (m, v) = line.split_once('>').ok_or_else(|| {
                bad(format!(
                    "{where_}: «{}» no es una regla (MÉTRICA > valor).",
                    shown(line)
                ))
            })?;
            let v: i64 = v
                .trim()
                .parse()
                .map_err(|_| bad(format!("{where_}: «{}» no es un número entero.", shown(v))))?;
            rule(m, v, where_)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Subscribers of a dashboard or an alert
// ---------------------------------------------------------------------------

/// A subscriber as its line: the user's name, or `destination:<id>`.
pub(crate) fn render_subscriber(s: &Json) -> String {
    let only = |k: &str| {
        s.as_object()
            .filter(|o| o.len() == 1)
            .and_then(|o| o.get(k))
            .and_then(Json::as_str)
            .filter(|v| !unsafe_on_a_line(v) && !v.contains(':'))
    };
    if let Some(u) = only("user_name") {
        return u.to_string();
    }
    if let Some(d) = only("destination_id") {
        return format!("destination:{d}");
    }
    s.to_string()
}

pub(crate) fn parse_subscribers(text: &str, label: &str) -> Result<Vec<Json>> {
    parse_list(text)
        .iter()
        .map(|line| {
            if line.starts_with('{') {
                return serde_json::from_str::<Json>(line)
                    .ok()
                    .filter(Json::is_object)
                    .ok_or_else(|| {
                        bad(format!(
                            "«{label}»: «{}» no es un objeto JSON válido.",
                            shown(line)
                        ))
                    });
            }
            if let Some(d) = line.strip_prefix("destination:") {
                let d = d.trim();
                return super::checked_id(d, label).map(|d| json!({ "destination_id": d }));
            }
            let u = line.strip_prefix("user:").unwrap_or(line).trim();
            if u.is_empty() || u.chars().any(char::is_whitespace) {
                return Err(bad(format!(
                    "«{label}»: «{}» no es un usuario ni un destino (destination:<id>).",
                    shown(line)
                )));
            }
            Ok(json!({ "user_name": u }))
        })
        .collect()
}

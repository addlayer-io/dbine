//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what `metadata:database`, `metadata:storage` and `GET /database/{db}`
//! report, and what `ALTER DATABASE` changes (OrientDB 3.x): time zone,
//! locale, charset, date formats, cluster selection, minimum clusters,
//! conflict strategy, validation, strict SQL and the custom attributes
//! (`ALTER DATABASE CUSTOM name = value`).
//!
//! One statement per change, each sent to `POST /command/{db}/sql` of the
//! target database (`ALTER DATABASE` works on the database it runs in).

use crate::ddl::string;
use crate::{as_text, classify, parse_reply, request, seg, OrientSession};
use dbine_driver::serde_static::intern;
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const FORMATS: &str = "Formatos";
const STORAGE: &str = "Almacenamiento";
const CUSTOM: &str = "Atributos propios";

/// Custom attributes shown by their own field (or internal).
const RESERVED: &[&str] = &["databaseInstenceId", "validation", "strictSql"];

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

fn check(ok: bool, what: &str, v: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(bad(what, v))
    }
}

fn yes(v: &str) -> bool {
    matches!(v.trim(), "true" | "1" | "on" | "ON")
}

fn custom_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// A format or custom value: a quoted literal, one line.
fn text_value(what: &str, v: &str) -> Result<String> {
    check(!v.is_empty() && !v.chars().any(char::is_control), what, v)?;
    Ok(string(v))
}

/// The statements for `changes`.
pub(crate) fn alter(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if database.trim().is_empty() {
        return Err(Error::Query("falta el nombre de la base".into()));
    }
    let set = |attr: &str, v: String| format!("ALTER DATABASE {attr} {v}");
    let mut out = Vec::new();
    let mut new_custom = (None, None);
    for (key, value) in changes {
        let v = value.trim();
        match key.as_str() {
            "timezone" => {
                check(
                    !v.is_empty() && v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-:".contains(c)),
                    "zona horaria",
                    v,
                )?;
                out.push(set("TIMEZONE", string(v)));
            }
            "locale_language" => {
                check((2..=3).contains(&v.len()) && v.chars().all(|c| c.is_ascii_lowercase()), "idioma", v)?;
                out.push(set("LOCALELANGUAGE", v.to_string()));
            }
            "locale_country" => {
                check(
                    (v.len() == 2 && v.chars().all(|c| c.is_ascii_uppercase())) || (v.len() == 3 && v.chars().all(|c| c.is_ascii_digit())),
                    "país",
                    v,
                )?;
                out.push(set("LOCALECOUNTRY", v.to_string()));
            }
            "charset" => {
                check(!v.is_empty() && v.len() <= 40 && v.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)), "juego de caracteres", v)?;
                out.push(set("CHARSET", string(v)));
            }
            "date_format" => out.push(set("DATEFORMAT", text_value("formato de fecha", v)?)),
            "datetime_format" => out.push(set("DATETIMEFORMAT", text_value("formato de fecha y hora", v)?)),
            "cluster_selection" => {
                check(matches!(v, "default" | "round-robin" | "balanced"), "selección de cluster", v)?;
                out.push(set("CLUSTERSELECTION", string(v)));
            }
            "minimum_clusters" => {
                check(v.parse::<u32>().is_ok_and(|n| n <= 1000), "clusters por clase", v)?;
                out.push(set("MINIMUMCLUSTERS", v.to_string()));
            }
            "conflict_strategy" => {
                check(matches!(v, "version" | "content" | "automerge"), "estrategia de conflictos", v)?;
                out.push(set("CONFLICTSTRATEGY", string(v)));
            }
            "validation" => out.push(set("VALIDATION", if yes(v) { "true" } else { "false" }.into())),
            "strict_sql" => out.push(set("CUSTOM", format!("strictSql={}", if yes(v) { "true" } else { "false" }))),
            "new_custom_name" => new_custom.0 = Some(v),
            "new_custom_value" => new_custom.1 = Some(v),
            k => match k.strip_prefix("custom:") {
                Some(name) if custom_name(name) && !RESERVED.contains(&name) => {
                    out.push(set("CUSTOM", format!("{name}={}", text_value(name, v)?)));
                }
                _ => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
            },
        }
    }
    match new_custom {
        (None | Some(""), None | Some("")) => {}
        (Some(name), Some(v)) if !v.is_empty() => {
            check(custom_name(name) && !RESERVED.contains(&name), "nombre del atributo", name)?;
            out.push(set("CUSTOM", format!("{name}={}", text_value(name, v)?)));
        }
        _ => return Err(Error::Query("un atributo nuevo necesita nombre y valor".into())),
    }
    Ok(out)
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(database, changes)?.into_iter().map(|s| s + ";").collect::<Vec<_>>().join("\n"))
}

fn fact(group: &str, label: &str, value: String) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value }
}

fn bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (n, 0);
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", n as i64)
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn sel(key: &'static str, label: &'static str, options: Vec<(&'static str, &'static str)>, group: &'static str) -> Field {
    Field::new(key, label, FieldKind::Select(options)).group(group)
}

impl OrientSession {
    /// One SQL statement on `database` (not the session's).
    async fn sql_on(&self, database: &str, stmt: &str) -> Result<Vec<Vec<(String, Value)>>> {
        let path = format!("/command/{}/sql/-/-1", seg(database));
        let text = request(&self.http, &self.base, &self.auth, Method::POST, &path, Some(&json!({ "command": stmt }))).await?;
        Ok(parse_reply(&text).map_err(Error::Query)?.records)
    }

    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let rows = self.sql_on(database, "SELECT FROM metadata:database").await?;
        let meta: serde_json::Map<String, Value> = rows.into_iter().next().unwrap_or_default().into_iter().collect();
        let m = |k: &str| meta.get(k).map(as_text).filter(|s| !s.is_empty() && s != "null");
        let storage: serde_json::Map<String, Value> = self
            .sql_on(database, "SELECT FROM metadata:storage")
            .await
            .ok()
            .and_then(|r| r.into_iter().next())
            .map(|r| r.into_iter().collect())
            .unwrap_or_default();
        let db = self.call(Method::GET, &format!("/database/{}", seg(database)), None).await?;

        let mut info = Vec::new();
        let mut push = |group: &str, label: &str, v: Option<String>| {
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                info.push(fact(group, label, v));
            }
        };
        push("", "Tipo", m("type"));
        push("", "Estado", m("status"));
        push("", "Versión del servidor", db.pointer("/server/version").map(as_text));
        push("", "Creada con la versión", storage.get("createdAtVersion").map(as_text));
        let all: Vec<Value> = db.get("classes").and_then(Value::as_array).cloned().unwrap_or_default();
        let user = classify(&all);
        let records: i64 = all.iter().filter_map(|c| c.get("records").and_then(Value::as_i64)).sum();
        push("", "Clases (propias / todas)", Some(format!("{} / {}", user.len(), all.len())));
        push("", "Registros", Some(records.to_string()));
        push("", "Índices", db.get("indexes").and_then(Value::as_array).map(|a| a.len().to_string()));
        push(STORAGE, "Almacenamiento", storage.get("type").map(as_text));
        push(STORAGE, "Tamaño", storage.get("size").and_then(Value::as_f64).map(bytes));
        push(STORAGE, "Clusters", storage.get("totalClusters").map(as_text));
        push(STORAGE, "Cluster predeterminado (defaultClusterId)", m("defaultClusterId"));
        push(STORAGE, "Serializador de registros", storage.get("configuration").and_then(|c| c.get("recordSerializer")).map(as_text));

        let mut values = BTreeMap::new();
        let mut put = |k: &str, v: Option<String>| {
            values.insert(k.to_string(), v.unwrap_or_default());
        };
        put("timezone", m("timezone"));
        put("locale_language", m("localeLanguage"));
        put("locale_country", m("localeCountry"));
        put("charset", m("charset"));
        put("date_format", m("dateFormat"));
        put("datetime_format", m("dateTimeFormat"));
        put("cluster_selection", m("clusterSelection"));
        put("minimum_clusters", m("minimumClusters"));
        put("conflict_strategy", m("conflictStrategy"));
        put("validation", Some(if m("validation").as_deref() == Some("false") { String::new() } else { "true".into() }));

        let mut fields = vec![
            Field::new("timezone", "Zona horaria (TIMEZONE)", FieldKind::Text).placeholder("America/Argentina/Buenos_Aires").group(FORMATS),
            Field::new("locale_language", "Idioma (LOCALE_LANGUAGE)", FieldKind::Text).placeholder("es").group(FORMATS),
            Field::new("locale_country", "País (LOCALE_COUNTRY)", FieldKind::Text).placeholder("AR").group(FORMATS),
            Field::new("charset", "Juego de caracteres (CHARSET)", FieldKind::Text).placeholder("UTF-8").group(FORMATS),
            Field::new("date_format", "Formato de fecha (DATEFORMAT)", FieldKind::Text).placeholder("yyyy-MM-dd").group(FORMATS),
            Field::new("datetime_format", "Formato de fecha y hora (DATETIMEFORMAT)", FieldKind::Text)
                .placeholder("yyyy-MM-dd HH:mm:ss")
                .group(FORMATS),
            sel(
                "cluster_selection",
                "Selección de cluster (CLUSTERSELECTION)",
                vec![("round-robin", "Rotativa (round-robin)"), ("default", "Siempre el primero (default)"), ("balanced", "El más chico (balanced)")],
                STORAGE,
            )
            .help("En qué cluster de la clase se guarda cada registro nuevo."),
            Field::new("minimum_clusters", "Clusters por clase nueva (MINIMUMCLUSTERS)", FieldKind::Number)
                .help("Cuántos clusters se crean con cada clase nueva; las clases existentes no cambian.")
                .group(STORAGE),
            sel(
                "conflict_strategy",
                "Estrategia de conflictos (CONFLICTSTRATEGY)",
                vec![("version", "Por versión: falla (version)"), ("content", "Por contenido (content)"), ("automerge", "Combinar (automerge)")],
                "",
            )
            .help("Qué hace el servidor cuando dos escrituras cambian el mismo registro a la vez."),
            Field::new("validation", "Validar los registros contra el esquema (VALIDATION)", FieldKind::Bool),
        ];

        // Custom attributes (`ALTER DATABASE CUSTOM`): strictSql has its own
        // switch; the rest a text field each, plus a pair to add one.
        let props: Vec<(String, String)> = db
            .pointer("/config/properties")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|p| Some((as_text(p.get("name")?), p.get("value").map(as_text).unwrap_or_default()))).collect())
            .unwrap_or_default();
        let strict = props.iter().find(|(k, _)| k == "strictSql").map(|(_, v)| v.as_str());
        fields.push(
            Field::new("strict_sql", "SQL estricto (CUSTOM strictSql)", FieldKind::Bool)
                .help("Apagado, el servidor acepta la sintaxis SQL anterior a la 3.0.")
                .group(CUSTOM),
        );
        values.insert("strict_sql".into(), if strict == Some("false") { String::new() } else { "true".into() });
        for (name, v) in props.iter().filter(|(k, _)| !RESERVED.contains(&k.as_str())) {
            let key = intern(&format!("custom:{name}"));
            fields.push(Field::new(key, intern(name), FieldKind::Text).group(CUSTOM));
            values.insert(key.to_string(), v.clone());
        }
        fields.push(Field::new("new_custom_name", "Atributo nuevo: nombre", FieldKind::Text).group(CUSTOM));
        fields.push(Field::new("new_custom_value", "Atributo nuevo: valor", FieldKind::Text).group(CUSTOM));
        values.insert("new_custom_name".into(), String::new());
        values.insert("new_custom_value".into(), String::new());

        // The strategies this server has.
        let list = |p: &str| -> Vec<String> {
            db.pointer(p).and_then(Value::as_array).map(|a| a.iter().map(as_text).collect()).unwrap_or_default()
        };
        let choices = vec![
            FieldChoices { key: "cluster_selection".into(), default: None, values: list("/server/clusterSelectionStrategies") },
            FieldChoices { key: "conflict_strategy".into(), default: Some("version".into()), values: list("/server/conflictStrategies") },
        ];

        let mut warnings = BTreeMap::new();
        warnings.insert(
            "validation".into(),
            "Sin validación, se guardan registros que no cumplen las restricciones del esquema (obligatorio, no nulo, mínimo, máximo…).".to_string(),
        );
        warnings.insert(
            "strict_sql".into(),
            "Cambia cómo se interpreta el SQL de todas las sesiones: consultas que hoy funcionan pueden dejar de hacerlo.".to_string(),
        );
        for k in ["timezone", "date_format", "datetime_format"] {
            warnings.insert(
                k.into(),
                "Cambia cómo se leen y muestran las fechas escritas como texto: las consultas y aplicaciones que dependen del formato o la zona anterior pueden dar otros resultados."
                    .to_string(),
            );
        }
        warnings.insert(
            "conflict_strategy".into(),
            "Con content o automerge, una escritura concurrente puede combinarse o pisarse en lugar de fallar.".to_string(),
        );
        Ok(DatabaseProperties { fields, values, info, choices, warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden modificar las propiedades de una base.".into()));
        }
        let statements = alter(database, changes)?;
        for (i, stmt) in statements.iter().enumerate() {
            if let Err(e) = self.sql_on(database, stmt).await {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {stmt}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn one_statement_per_change() {
        assert_eq!(
            script(
                "ventas",
                &c(&[
                    ("timezone", "America/Argentina/Buenos_Aires"),
                    ("locale_language", "es"),
                    ("locale_country", "AR"),
                    ("date_format", "dd/MM/yyyy"),
                    ("minimum_clusters", "4"),
                    ("validation", ""),
                    ("strict_sql", "true"),
                    ("conflict_strategy", "automerge"),
                ])
            )
            .unwrap(),
            "ALTER DATABASE CONFLICTSTRATEGY 'automerge';
ALTER DATABASE DATEFORMAT 'dd/MM/yyyy';
ALTER DATABASE LOCALECOUNTRY AR;
ALTER DATABASE LOCALELANGUAGE es;
ALTER DATABASE MINIMUMCLUSTERS 4;
ALTER DATABASE CUSTOM strictSql=true;
ALTER DATABASE TIMEZONE 'America/Argentina/Buenos_Aires';
ALTER DATABASE VALIDATION false;"
        );
    }

    #[test]
    fn custom_attributes() {
        assert_eq!(
            alter("v", &c(&[("custom:miClave", "it's"), ("new_custom_name", "otra"), ("new_custom_value", "x y")])).unwrap(),
            ["ALTER DATABASE CUSTOM miClave='it\\'s'", "ALTER DATABASE CUSTOM otra='x y'"]
        );
        assert!(alter("v", &c(&[("new_custom_name", "sola")])).is_err());
        assert!(alter("v", &c(&[("new_custom_name", "a b"), ("new_custom_value", "1")])).is_err());
        assert!(alter("v", &c(&[("custom:validation", "x")])).is_err());
        assert!(alter("v", &c(&[("new_custom_name", ""), ("new_custom_value", "")])).unwrap().is_empty());
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("timezone", "UTC'; DROP"),
            ("locale_language", "ES"),
            ("locale_country", "ar"),
            ("charset", "UTF 8"),
            ("date_format", "a\nb"),
            ("cluster_selection", "auto"),
            ("minimum_clusters", "-1"),
            ("conflict_strategy", "last"),
            ("custom:a;b", "1"),
            ("custom:x", ""),
            ("nope", "1"),
        ] {
            assert!(alter("v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(alter(" ", &c(&[])).is_err());
    }
}

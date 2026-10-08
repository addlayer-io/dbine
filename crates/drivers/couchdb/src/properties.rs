//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what `GET /{db}` reports (documents, sizes, cluster, sequences) and the
//! settings the API lets change: `_revs_limit`, `_purged_infos_limit` and
//! the security object (`_security`, its admins and members).
//!
//! The security object is one JSON field: `PUT /{db}/_security` replaces
//! the whole object, so the request carries all of it, exactly as it runs.
//! Each change is one request; the security object goes last.

use crate::{seg, CouchSession};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// One request of the change: method, path, JSON body.
pub(crate) type Request = (Method, String, Value);

fn limit(v: &str, what: &str) -> Result<Value> {
    v.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| (1..=1_000_000_000).contains(n))
        .map(Value::from)
        .ok_or_else(|| Error::Query(format!("{what}: «{v}» no es un valor válido (un entero positivo)")))
}

/// The security object, checked: `admins` and `members`, each with
/// `names` and `roles` lists of strings. Other keys pass through as JSON.
fn security(v: &str) -> Result<Value> {
    let bad = |why: &str| Error::Query(format!("seguridad (_security): {why}"));
    let mut doc: Map<String, Value> = match serde_json::from_str(v.trim()) {
        Ok(Value::Object(o)) => o,
        Ok(_) => return Err(bad("tiene que ser un objeto JSON")),
        Err(e) => return Err(bad(&format!("JSON inválido: {e}"))),
    };
    for section in ["admins", "members"] {
        let s = doc.entry(section).or_insert_with(|| json!({}));
        let Value::Object(s) = s else { return Err(bad(&format!("«{section}» tiene que ser un objeto"))) };
        if let Some(k) = s.keys().find(|k| *k != "names" && *k != "roles") {
            return Err(bad(&format!("«{section}» solo lleva names y roles, no «{k}»")));
        }
        for list in ["names", "roles"] {
            let l = s.entry(list).or_insert_with(|| json!([]));
            let ok = l.as_array().is_some_and(|a| a.iter().all(|x| x.as_str().is_some_and(|x| !x.trim().is_empty())));
            if !ok {
                return Err(bad(&format!("«{section}.{list}» tiene que ser una lista de textos")));
            }
        }
    }
    // Fixed order (admins, members; names, roles) whatever map the build
    // uses: sorted, or insertion order when `preserve_order` is on.
    let part = |section: &str| json!({ "names": doc[section]["names"], "roles": doc[section]["roles"] });
    let mut out = Map::new();
    out.insert("admins".into(), part("admins"));
    out.insert("members".into(), part("members"));
    // Other keys pass through, after them.
    for (k, v) in doc {
        if k != "admins" && k != "members" {
            out.insert(k, v);
        }
    }
    Ok(Value::Object(out))
}

/// The requests for `changes`: the limits, then the security object.
pub(crate) fn alter(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<Request>> {
    if database.is_empty() || database.starts_with('_') {
        return Err(Error::Query(format!("«{database}» no es una base de usuario")));
    }
    let db = format!("/{}", seg(database));
    let mut out = Vec::new();
    let mut sec = None;
    for (key, value) in changes {
        match key.as_str() {
            "revs_limit" => out.push((Method::PUT, format!("{db}/_revs_limit"), limit(value, "revisiones guardadas")?)),
            "purged_infos_limit" => out.push((Method::PUT, format!("{db}/_purged_infos_limit"), limit(value, "purgas recordadas")?)),
            "security" => sec = Some((Method::PUT, format!("{db}/_security"), security(value)?)),
            k => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
        }
    }
    out.extend(sec);
    Ok(out)
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(database, changes)?.iter().map(|(m, p, b)| format!("{m} {p}\n{b}")).collect::<Vec<_>>().join("\n\n"))
}

fn bytes(v: Option<&Value>) -> String {
    let Some(n) = v.and_then(Value::as_u64) else { return String::new() };
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut x = n as f64;
    let mut u = 0;
    while x >= 1024.0 && u < units.len() - 1 {
        x /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{x:.1} {}", units[u])
    }
}

fn shown(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

impl CouchSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let db = format!("/{}", seg(database));
        let i = self.call(Method::GET, &db, None).await?;
        let fact = |group: &str, label: &str, value: String| PropertyInfo { group: group.into(), label: label.into(), value };
        let yes_no = |v: Option<&Value>| if v.and_then(Value::as_bool).unwrap_or(false) { "Sí".to_string() } else { "No".to_string() };
        let mut info = vec![
            fact("", "Documentos", shown(i.get("doc_count"))),
            fact("", "Documentos borrados", shown(i.get("doc_del_count"))),
            fact("", "Tamaño en disco (file)", bytes(i.pointer("/sizes/file"))),
            fact("", "Datos activos (active)", bytes(i.pointer("/sizes/active"))),
            fact("", "Datos sin comprimir (external)", bytes(i.pointer("/sizes/external"))),
            fact("", "Particionada (partitioned)", yes_no(i.pointer("/props/partitioned"))),
            fact("", "Compactación en curso", yes_no(i.get("compact_running"))),
        ];
        for (k, label) in [("q", "Shards (q)"), ("n", "Réplicas (n)"), ("w", "Quórum de escritura (w)"), ("r", "Quórum de lectura (r)")] {
            if let Some(v) = i.pointer(&format!("/cluster/{k}")) {
                info.push(fact("Cluster", label, shown(Some(v))));
            }
        }
        info.push(fact("Historial", "Secuencia de cambios (update_seq)", shown(i.get("update_seq"))));
        info.push(fact("Historial", "Secuencia de purgas (purge_seq)", shown(i.get("purge_seq"))));

        let mut values = BTreeMap::new();
        let mut fields = Vec::new();
        let mut warnings = BTreeMap::new();
        // The limits and the security object need an admin of the database.
        for (key, path, label, help) in [
            (
                "revs_limit",
                "_revs_limit",
                "Revisiones guardadas (_revs_limit)",
                "Cuántas revisiones de cada documento recuerda la base (no su contenido). Por defecto, 1000.",
            ),
            (
                "purged_infos_limit",
                "_purged_infos_limit",
                "Purgas recordadas (_purged_infos_limit)",
                "Cuántas purgas recuerda la base para pasarlas a las réplicas y los índices. Por defecto, 1000.",
            ),
        ] {
            if let Ok(v) = self.call(Method::GET, &format!("{db}/{path}"), None).await {
                fields.push(Field::new(key, label, FieldKind::Number).help(help).group("Historial"));
                values.insert(key.to_string(), shown(Some(&v)));
            }
        }
        warnings.insert(
            "revs_limit".into(),
            "Bajarlo descarta, en la próxima compactación, el historial de revisiones que pase el límite: al replicar con copias que quedaron atrás pueden aparecer conflictos que ya no se resuelven solos.".into(),
        );
        warnings.insert(
            "purged_infos_limit".into(),
            "Bajarlo olvida purgas viejas: una réplica o un índice que no se puso al día desde entonces puede volver a traer los documentos purgados.".into(),
        );

        if let Ok(sec) = self.call(Method::GET, &format!("{db}/_security"), None).await {
            // Normalized (both sections, both lists) so it compares as is.
            let sec = if sec.is_object() { sec } else { json!({}) };
            let norm = security(&sec.to_string()).unwrap_or(sec);
            fields.push(
                Field::new("security", "Objeto de seguridad (_security)", FieldKind::Textarea)
                    .help("admins: nombres y roles que administran la base (diseños, seguridad). members: los que pueden leer y escribir. Sin members, la base es pública.")
                    .group("Seguridad"),
            );
            for (section, label) in [("admins", "Administradores"), ("members", "Miembros")] {
                for (list, what) in [("names", "usuarios"), ("roles", "roles")] {
                    let v = norm.pointer(&format!("/{section}/{list}")).and_then(Value::as_array);
                    let joined = v.map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                    info.push(fact("Seguridad", &format!("{label}: {what}"), if joined.is_empty() { "(ninguno)".into() } else { joined }));
                }
            }
            values.insert("security".into(), serde_json::to_string_pretty(&norm).unwrap_or_default());
            warnings.insert(
                "security".into(),
                "Reemplaza el objeto de seguridad entero. Si members queda sin nombres ni roles, la base es pública: cualquiera puede leer y escribir documentos. Quitar nombres o roles deja sin acceso a quienes los usaban.".into(),
            );
        }
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        self.refuse_if_read_only("modificar las propiedades de una base")?;
        let requests = alter(database, changes)?;
        for (i, (method, path, body)) in requests.iter().enumerate() {
            if let Err(e) = self.call(method.clone(), path, Some(body)).await {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {method} {path}\n{e}", requests.len())) });
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
    fn each_change_in_order() {
        assert_eq!(
            script(
                "ventas/2024",
                &c(&[("security", r#"{"members": {"names": ["ana"]}}"#), ("revs_limit", "500"), ("purged_infos_limit", "2000")])
            )
            .unwrap(),
            "PUT /ventas%2F2024/_purged_infos_limit\n2000\n\nPUT /ventas%2F2024/_revs_limit\n500\n\nPUT /ventas%2F2024/_security\n\
             {\"admins\":{\"names\":[],\"roles\":[]},\"members\":{\"names\":[\"ana\"],\"roles\":[]}}"
        );
        assert_eq!(script("v", &c(&[])).unwrap(), "");
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("revs_limit", "0"),
            ("revs_limit", "10; x"),
            ("revs_limit", ""),
            ("purged_infos_limit", "-1"),
            ("security", "[]"),
            ("security", "{"),
            ("security", r#"{"members": []}"#),
            ("security", r#"{"members": {"names": "ana"}}"#),
            ("security", r#"{"members": {"names": [1]}}"#),
            ("security", r#"{"admins": {"users": []}}"#),
            ("nope", "1"),
        ] {
            assert!(script("v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script("_users", &c(&[("revs_limit", "10")])).is_err(), "system database");
        // Other keys of the security object pass through, as JSON.
        assert!(script("v", &c(&[("security", r#"{"x": "y\"z"}"#)])).unwrap().contains(r#""x":"y\"z""#));
    }
}

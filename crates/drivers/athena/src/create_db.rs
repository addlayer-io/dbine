//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]):
//! the Hive DDL `CREATE DATABASE` that Athena runs on the data catalog takes
//! a `COMMENT`, a `LOCATION` in S3 and `DBPROPERTIES`.
//!
//! Every value is checked before it reaches the SQL.

use crate::ddl::{lit, q};
use dbine_driver::{Error, Field, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("comment", "Comentario", FieldKind::Textarea),
        Field::new("location", "Ubicación en S3 (LOCATION)", FieldKind::Text)
            .placeholder("s3://bucket/ruta/")
            .help("Vacía: la que asigne el catálogo de datos."),
        Field::new("properties", "Propiedades (DBPROPERTIES)", FieldKind::Textarea)
            .placeholder("creador=ana\nequipo=ventas")
            .help("Una por línea, como clave=valor."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// `clave=valor` lines; keys are letters, digits and `_ . : -`.
fn properties(v: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for line in v.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (k, val) = line.split_once('=').ok_or_else(|| Error::Query(format!("propiedades: «{line}» no tiene la forma clave=valor")))?;
        let k = k.trim();
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')) {
            return Err(Error::Query(format!("propiedades: «{k}» no es un nombre de propiedad válido")));
        }
        if val.chars().any(char::is_control) {
            return Err(Error::Query(format!("propiedades: el valor de «{k}» tiene caracteres de control")));
        }
        out.push((k.to_string(), val.trim().to_string()));
    }
    Ok(out)
}

/// The `CREATE DATABASE` for `name`.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let mut sql = format!("CREATE DATABASE {}", q(name));
    if let Some(c) = opt(o, "comment") {
        sql.push_str(&format!("\nCOMMENT {}", lit(c)));
    }
    if let Some(l) = opt(o, "location") {
        let ok = l.len() > "s3://".len()
            && l.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("s3://"))
            && !l.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\'' || c == '\\');
        if !ok {
            return Err(Error::Query(format!("ubicación: «{l}» no es una ruta s3://")));
        }
        sql.push_str(&format!("\nLOCATION {}", lit(l)));
    }
    if let Some(p) = opt(o, "properties") {
        let props = properties(p)?;
        if !props.is_empty() {
            let list: Vec<String> = props.iter().map(|(k, v)| format!("{} = {}", lit(k), lit(v))).collect();
            sql.push_str(&format!("\nWITH DBPROPERTIES ({})", list.join(", ")));
        }
    }
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        assert_eq!(script("ventas", &o(&[])).unwrap(), "CREATE DATABASE `ventas`");
        assert_eq!(script("ventas", &o(&[("properties", " \n ")])).unwrap(), "CREATE DATABASE `ventas`");
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script("v", &o(&[("comment", "it's \\ ok"), ("location", "s3://datos/v/"), ("properties", "creador=ana\n\n equipo = ventas, sur \n")])).unwrap(),
            "CREATE DATABASE `v`\nCOMMENT 'it\\'s \\\\ ok'\nLOCATION 's3://datos/v/'\nWITH DBPROPERTIES ('creador' = 'ana', 'equipo' = 'ventas, sur')"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("location", "/tmp/x"),
            ("location", "s3://"),
            ("location", "s3://b/x' --"),
            ("location", "s3://b/a b"),
            ("properties", "sin igual"),
            ("properties", "a b=c"),
            ("properties", "k'=v"),
        ] {
            assert!(script("v", &o(&[bad])).is_err(), "{bad:?}");
        }
    }
}

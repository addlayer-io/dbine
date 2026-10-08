//! "Propiedades" of a database (a Unity Catalog catalog,
//! [`dbine_driver::Session::database_properties`]): what `DESCRIBE CATALOG
//! EXTENDED` reports, and what `ALTER CATALOG` / `COMMENT ON CATALOG`
//! change: the comment, predictive optimization and the owner.
//!
//! A catalog's storage root, isolation mode and type can't change through
//! SQL: they're shown as facts. One statement per change, the owner last
//! (after it the user may no longer alter the catalog).

use crate::ddl::lit;
use crate::DatabricksSession;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use std::collections::BTreeMap;

/// `DESCRIBE CATALOG EXTENDED` rows turned into fields, not facts.
const EDITABLE: &[&str] = &["Catalog Name", "Comment", "Owner", "Predictive Optimization"];

/// What a `Predictive Optimization` row says: `ENABLE`, `DISABLE` or,
/// when it comes from the metastore, `INHERIT`.
fn predictive(v: &str) -> String {
    let up = v.to_ascii_uppercase();
    if up.contains("INHERITED") {
        "INHERIT".into()
    } else if up.starts_with("DISABLE") {
        "DISABLE".into()
    } else if up.starts_with("ENABLE") {
        "ENABLE".into()
    } else {
        String::new()
    }
}

pub(crate) fn alter(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let cat = quote_ident(Quote::Backtick, database);
    let (mut out, mut last) = (Vec::new(), Vec::new());
    for (key, value) in changes {
        let v = value.trim();
        match key.as_str() {
            "comment" => out.push(format!("COMMENT ON CATALOG {cat} IS {}", if v.is_empty() { "NULL".into() } else { lit(v) })),
            "predictive_optimization" => {
                if !matches!(v, "ENABLE" | "DISABLE" | "INHERIT") {
                    return Err(Error::Query(format!("optimización predictiva: «{v}» no es un valor válido")));
                }
                out.push(format!("ALTER CATALOG {cat} {v} PREDICTIVE OPTIMIZATION"));
            }
            "owner" => {
                if v.is_empty() {
                    return Err(Error::Query("dueño: falta el usuario, grupo o service principal".into()));
                }
                last.push(format!("ALTER CATALOG {cat} OWNER TO {}", quote_ident(Quote::Backtick, v)));
            }
            k => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
        }
    }
    Ok(out.into_iter().chain(last).collect())
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(database, changes)?.into_iter().map(|s| s + ";").collect::<Vec<_>>().join("\n"))
}

impl DatabricksSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let rows = self.named_rows(&format!("DESCRIBE CATALOG EXTENDED {}", quote_ident(Quote::Backtick, database))).await?;
        let get = |name: &str| {
            rows.iter().find(|r| r.get("info_name").is_some_and(|n| n.eq_ignore_ascii_case(name))).and_then(|r| r.get("info_value").cloned())
        };
        let info: Vec<PropertyInfo> = rows
            .iter()
            .filter_map(|r| Some((r.get("info_name")?, r.get("info_value").cloned().unwrap_or_default())))
            .filter(|(n, v)| !v.is_empty() && !EDITABLE.iter().any(|e| e.eq_ignore_ascii_case(n)))
            .map(|(n, v)| PropertyInfo { group: String::new(), label: n.clone(), value: v })
            .collect();
        let values = BTreeMap::from([
            ("comment".to_string(), get("Comment").unwrap_or_default()),
            ("owner".to_string(), get("Owner").unwrap_or_default()),
            ("predictive_optimization".to_string(), get("Predictive Optimization").map(|v| predictive(&v)).unwrap_or_default()),
        ]);
        let mut fields = vec![
            Field::new("owner", "Dueño", FieldKind::Text).help("Un usuario, grupo o service principal."),
            Field::new("comment", "Comentario", FieldKind::Textarea),
        ];
        if get("Predictive Optimization").is_some() {
            fields.push(
                Field::new(
                    "predictive_optimization",
                    "Optimización predictiva (PREDICTIVE OPTIMIZATION)",
                    FieldKind::Select(vec![("ENABLE", "Activada (ENABLE)"), ("DISABLE", "Desactivada (DISABLE)"), ("INHERIT", "La del metastore (INHERIT)")]),
                )
                .help("Compacta, agrupa y limpia automáticamente las tablas administradas del catálogo."),
            );
        }
        let warnings = BTreeMap::from([
            (
                "owner".to_string(),
                "Pasa el catálogo a otro dueño: si no sos admin del metastore ni formás parte del nuevo dueño, podés perder el permiso de modificarlo o borrarlo.".to_string(),
            ),
            (
                "predictive_optimization".to_string(),
                "Desactivarla deja de compactar y limpiar (VACUUM) automáticamente las tablas administradas: crecen el almacenamiento y el tiempo de las consultas.".to_string(),
            ),
        ]);
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(database, changes)?;
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.run(sql, 1, None).await {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
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
    fn only_the_changes_owner_last() {
        assert_eq!(
            script("ven`tas", &c(&[("owner", "data-eng"), ("comment", "it's"), ("predictive_optimization", "DISABLE")])).unwrap(),
            "COMMENT ON CATALOG `ven``tas` IS 'it\\'s';\n\
             ALTER CATALOG `ven``tas` DISABLE PREDICTIVE OPTIMIZATION;\n\
             ALTER CATALOG `ven``tas` OWNER TO `data-eng`;"
        );
        assert_eq!(script("v", &c(&[("comment", "")])).unwrap(), "COMMENT ON CATALOG `v` IS NULL;");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("predictive_optimization", "ON"), ("owner", " "), ("storage_root", "s3://x")] {
            assert!(script("v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script("v", &c(&[("owner", "a` --")])).unwrap().ends_with("OWNER TO `a`` --`;"));
    }

    #[test]
    fn predictive_optimization_as_described() {
        assert_eq!(predictive("ENABLE (inherited from METASTORE metastore_aws_us_east_1)"), "INHERIT");
        assert_eq!(predictive("DISABLE"), "DISABLE");
        assert_eq!(predictive("ENABLE"), "ENABLE");
    }
}

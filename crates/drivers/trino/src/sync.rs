//! Schema sync ("Comparar esquemas") in Trino's SQL. What a table accepts
//! depends on its connector; the statements are the standard ones and the
//! server rejects what the connector can't do. No keys or indexes.
//!
//! - Trino / Starburst: `ADD COLUMN`, `DROP COLUMN`, `ALTER COLUMN c SET
//!   DATA TYPE t`, `ALTER COLUMN c DROP NOT NULL`, `COMMENT ON`.
//! - Presto: columns are only added and dropped; comments with `COMMENT ON`.

use crate::ddl::table_ddl;
use crate::Flavor;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{ColumnDef, DdlParts, Result, SyncScript, TableChange, TableSchema};

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn squash(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect()
}

/// `ALTER COLUMN` takes a qualified name, and Trino looks quoted ones up
/// with their quotes ("Column '"id"' does not exist"): plain names go bare.
fn column_ref(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain { name.to_string() } else { quote_ident(Quote::Double, name) }
}

fn default_of(c: &ColumnDef) -> Option<&str> {
    c.default_value.as_deref().map(str::trim).filter(|d| !d.is_empty())
}

pub fn sync_script(flavor: Flavor, changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => {
                let t = if flavor == Flavor::Presto { without_defaults(table, &mut warnings) } else { table.clone() };
                creates.push(table_ddl(&t, DdlParts { create: true, ..Default::default() }));
            }
            TableChange::Drop { table } => {
                warnings.push(format!("Se borra la tabla {} con todos sus datos.", display(table)));
                drops.push(table_ddl(table, DdlParts { drop: true, ..Default::default() }));
            }
            TableChange::Alter { old, new } => alter(flavor, old, new, &mut alters, &mut warnings),
        }
    }
    if changes.iter().any(|c| matches!(c, TableChange::Alter { .. })) && !alters.is_empty() {
        warnings.push("Qué cambios acepta una tabla depende de su conector: si no admite alguno, el servidor lo rechaza.".into());
    }
    Ok(SyncScript { statements: [drops, alters, creates].concat(), warnings })
}

/// Presto has no column defaults.
fn without_defaults(t: &TableSchema, warnings: &mut Vec<String>) -> TableSchema {
    let tname = display(t);
    let mut t = t.clone();
    for c in &mut t.columns {
        if c.default_value.take().is_some_and(|d| !d.trim().is_empty()) {
            warnings.push(format!("{tname}.{}: Presto no tiene valores por defecto; se omite.", c.name));
        }
    }
    t
}

fn alter(flavor: Flavor, old: &TableSchema, new: &TableSchema, out: &mut Vec<String>, warnings: &mut Vec<String>) {
    let name = qualified_name(Quote::Double, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
    let q = |c: &str| quote_ident(Quote::Double, c);
    let tname = display(new);
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let presto = flavor == Flavor::Presto;

    for c in old.columns.iter().filter(|c| !new.columns.iter().any(|n| eq(&n.name, &c.name))) {
        warnings.push(format!("Se borra la columna {tname}.{} con sus datos.", c.name));
        out.push(format!("ALTER TABLE {name} DROP COLUMN {};", q(&c.name)));
    }
    for c in new.columns.iter().filter(|c| !old.columns.iter().any(|o| eq(&o.name, &c.name))) {
        let mut d = format!("{} {}", q(&c.name), c.data_type);
        match default_of(c) {
            Some(x) if !presto => d.push_str(&format!(" DEFAULT {x}")),
            Some(_) => warnings.push(format!("{tname}.{}: Presto no tiene valores por defecto; se omite.", c.name)),
            None => {}
        }
        if !c.nullable {
            d.push_str(" NOT NULL");
            if default_of(c).is_none() || presto {
                warnings.push(format!("{tname}.{} es NOT NULL sin valor por defecto: falla si la tabla tiene filas.", c.name));
            }
        }
        if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
            d.push_str(&format!(" COMMENT {}", lit(cm)));
        }
        out.push(format!("ALTER TABLE {name} ADD COLUMN {d};"));
    }
    for n in &new.columns {
        let Some(o) = old.columns.iter().find(|o| eq(&o.name, &n.name)) else { continue };
        let col = q(&n.name);
        let bare = column_ref(&n.name);
        if squash(&o.data_type) != squash(&n.data_type) {
            if presto {
                warnings.push(format!("{tname}.{}: {} → {}. Presto no cambia el tipo de una columna; se deja como está.", n.name, o.data_type, n.data_type));
            } else {
                warnings.push(format!("{tname}.{}: {} → {}. Puede fallar si el conector no admite ese cambio o los valores no entran.", n.name, o.data_type, n.data_type));
                out.push(format!("ALTER TABLE {name} ALTER COLUMN {bare} SET DATA TYPE {};", n.data_type));
            }
        }
        if o.nullable != n.nullable {
            if n.nullable && !presto {
                out.push(format!("ALTER TABLE {name} ALTER COLUMN {bare} DROP NOT NULL;"));
            } else {
                warnings.push(format!("{tname}.{}: {} no cambia la nulabilidad de una columna{}; se deja como está.", n.name, flavor_name(flavor), if n.nullable { "" } else { " a NOT NULL" }));
            }
        }
        if default_of(o) != default_of(n) && !presto {
            warnings.push(format!("{tname}.{}: el valor por defecto no se cambia con ALTER; se deja como está.", n.name));
        }
        if o.comment.as_deref().unwrap_or("") != n.comment.as_deref().unwrap_or("") {
            let v = n.comment.as_deref().filter(|s| !s.is_empty()).map(lit).unwrap_or_else(|| "NULL".into());
            out.push(format!("COMMENT ON COLUMN {name}.{col} IS {v};"));
        }
    }
    if old.comment.as_deref().unwrap_or("") != new.comment.as_deref().unwrap_or("") {
        let v = new.comment.as_deref().filter(|s| !s.is_empty()).map(lit).unwrap_or_else(|| "NULL".into());
        out.push(format!("COMMENT ON TABLE {name} IS {v};"));
    }
    let with = |t: &TableSchema| t.options.get(crate::ddl::WITH).map(|w| squash(w)).unwrap_or_default();
    if !with(new).is_empty() && with(old) != with(new) {
        warnings.push(format!("{tname}: las propiedades de la tabla (WITH) no se sincronizan; cambialas a mano (ALTER TABLE … SET PROPERTIES)."));
    }
}

fn flavor_name(f: Flavor) -> &'static str {
    match f {
        Flavor::Trino => "Trino",
        Flavor::Presto => "Presto",
        Flavor::Starburst => "Starburst",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }
    }

    fn t() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("ventas".into()),
            name: "pedidos".into(),
            columns: vec![col("id", "integer", false), col("nota", "varchar(10)", false), col("baja", "date", true)],
            ..Default::default()
        }
    }

    fn changed() -> TableSchema {
        let mut n = t();
        n.columns[0].data_type = "bigint".into();
        n.columns[1].nullable = true;
        n.columns[1].comment = Some("o'k".into());
        n.columns.remove(2);
        n.columns.push(ColumnDef { default_value: Some("0".into()), ..col("cant", "integer", true) });
        n
    }

    #[test]
    fn trino_alter() {
        let s = sync_script(Flavor::Trino, &[TableChange::Alter { old: t(), new: changed() }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"ventas\".\"pedidos\" DROP COLUMN \"baja\";",
                "ALTER TABLE \"ventas\".\"pedidos\" ADD COLUMN \"cant\" integer DEFAULT 0;",
                "ALTER TABLE \"ventas\".\"pedidos\" ALTER COLUMN id SET DATA TYPE bigint;",
                "ALTER TABLE \"ventas\".\"pedidos\" ALTER COLUMN nota DROP NOT NULL;",
                "COMMENT ON COLUMN \"ventas\".\"pedidos\".\"nota\" IS 'o''k';",
            ]
        );
        assert_eq!(s.warnings.len(), 3, "{:?}", s.warnings);
    }

    #[test]
    fn presto_only_adds_and_drops() {
        let s = sync_script(Flavor::Presto, &[TableChange::Alter { old: t(), new: changed() }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"ventas\".\"pedidos\" DROP COLUMN \"baja\";",
                "ALTER TABLE \"ventas\".\"pedidos\" ADD COLUMN \"cant\" integer;",
                "COMMENT ON COLUMN \"ventas\".\"pedidos\".\"nota\" IS 'o''k';",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("no cambia el tipo") && w.contains("nulabilidad") && w.contains("valores por defecto"), "{w}");
    }

    #[test]
    fn create_and_drop() {
        let mut old = t();
        old.name = "viejos".into();
        let s = sync_script(Flavor::Starburst, &[TableChange::Create { table: t() }, TableChange::Drop { table: old }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE \"ventas\".\"viejos\";");
        assert!(s.statements[1].starts_with("CREATE TABLE \"ventas\".\"pedidos\" (\n    \"id\" integer NOT NULL,"), "{}", s.statements[1]);
        assert_eq!(s.warnings.len(), 1);
    }
}

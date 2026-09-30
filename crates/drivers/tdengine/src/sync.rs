//! Schema sync ("Comparar esquemas") in TDengine: `ALTER TABLE / STABLE …
//! ADD / DROP / MODIFY COLUMN`, and `ADD / DROP / MODIFY TAG` on
//! supertables. `MODIFY` only widens VARCHAR / NCHAR / VARBINARY /
//! GEOMETRY; any other type change needs the table recreated (a warning).
//! Subtables take their structure from their supertable.

use crate::ddl::{index_ddl, lit, q, qualified, table_ddl, SUBTABLE, SUPERTABLE, TAG};
use dbine_driver::{ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};

fn is_tag(c: &ColumnDef) -> bool {
    c.options.get(TAG).is_some_and(|v| v == "true" || v == "1")
}

fn is_stable(t: &TableSchema) -> bool {
    t.kind == SUPERTABLE || t.columns.iter().any(is_tag)
}

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

/// `VARCHAR(20)` → (`VARCHAR`, Some(20)); BINARY is VARCHAR.
fn split_type(t: &str) -> (String, Option<u64>) {
    let t = t.trim().to_uppercase();
    let (base, len) = match t.split_once('(') {
        Some((b, rest)) => (b.trim().to_string(), rest.trim_end_matches(')').trim().parse().ok()),
        None => (t.clone(), None),
    };
    (if base == "BINARY" { "VARCHAR".into() } else { base }, len)
}

fn squash(s: &str) -> String {
    s.to_uppercase().split_whitespace().collect::<String>().replace("BINARY(", "VARCHAR(")
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(table_ddl(table, DdlParts { create: true, indexes: true, ..Default::default() })),
            TableChange::Drop { table } => {
                warnings.push(format!(
                    "Se borra {} {} con todos sus datos{}.",
                    if is_stable(table) { "la supertabla" } else { "la tabla" },
                    display(table),
                    if is_stable(table) { " y sus subtablas" } else { "" }
                ));
                drops.push(table_ddl(table, DdlParts { drop: true, ..Default::default() }));
            }
            TableChange::Alter { old, new } => alter(old, new, &mut alters, &mut warnings),
        }
    }
    Ok(SyncScript { statements: [drops, alters, creates].concat(), warnings })
}

fn alter(old: &TableSchema, new: &TableSchema, out: &mut Vec<String>, warnings: &mut Vec<String>) {
    let tname = display(new);
    if old.kind == SUBTABLE || new.kind == SUBTABLE {
        warnings.push(format!("{tname} es una subtabla: su estructura es la de su supertabla; se deja como está."));
        return;
    }
    let stable = is_stable(old);
    if stable != is_stable(new) {
        warnings.push(format!("{tname}: pasar de tabla a supertabla (o al revés) requiere recrearla; se deja como está."));
        return;
    }
    let what = if stable { "STABLE" } else { "TABLE" };
    let head = format!("ALTER {what} {}", qualified(new.schema.as_deref(), &new.name));
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    // Tag indexes, by name: dropped before the tags change, made after.
    let same_ix = |a: &IndexDef, b: &IndexDef| eq(&a.name, &b.name) && a.columns.len() == b.columns.len() && a.columns.iter().zip(&b.columns).all(|(x, y)| eq(x, y));
    for o in old.indexes.iter().filter(|o| !new.indexes.iter().any(|n| same_ix(o, n))) {
        out.push(format!("DROP INDEX {};", qualified(new.schema.as_deref(), &o.name)));
    }
    let add_ix: Vec<String> = new.indexes.iter().filter(|n| !old.indexes.iter().any(|o| same_ix(o, n))).map(|n| index_ddl(new, n)).collect();

    for o in &old.columns {
        let kind = if is_tag(o) { "TAG" } else { "COLUMN" };
        match new.columns.iter().find(|n| eq(&n.name, &o.name)) {
            None if old.columns.first().is_some_and(|f| std::ptr::eq(f, o)) => {
                warnings.push(format!("{tname}.{}: la primera columna (marca de tiempo) no se puede borrar; se deja.", o.name));
            }
            None => {
                warnings.push(format!("Se borra {} {tname}.{} con sus datos.", if is_tag(o) { "el tag" } else { "la columna" }, o.name));
                out.push(format!("{head} DROP {kind} {};", q(&o.name)));
            }
            Some(n) if is_tag(n) != is_tag(o) => {
                warnings.push(format!("{tname}.{}: pasar de columna a tag (o al revés) requiere borrarla y volver a crearla; se deja como está.", n.name));
            }
            Some(n) if squash(&n.data_type) != squash(&o.data_type) => {
                let ((ob, ol), (nb, nl)) = (split_type(&o.data_type), split_type(&n.data_type));
                let widen = ob == nb && matches!(nb.as_str(), "VARCHAR" | "NCHAR" | "VARBINARY" | "GEOMETRY") && matches!((ol, nl), (Some(a), Some(b)) if b > a);
                if widen {
                    out.push(format!("{head} MODIFY {kind} {} {};", q(&n.name), n.data_type));
                } else {
                    warnings.push(format!(
                        "{tname}.{}: {} → {}. TDengine solo agranda VARCHAR, NCHAR, VARBINARY y GEOMETRY; se deja como está.",
                        n.name, o.data_type, n.data_type
                    ));
                }
            }
            Some(_) => {}
        }
    }
    for n in new.columns.iter().filter(|n| !old.columns.iter().any(|o| eq(&o.name, &n.name))) {
        let kind = if is_tag(n) { "TAG" } else { "COLUMN" };
        out.push(format!("{head} ADD {kind} {} {};", q(&n.name), n.data_type));
    }
    if old.comment.as_deref().unwrap_or("") != new.comment.as_deref().unwrap_or("") {
        out.push(format!("{head} COMMENT {};", lit(new.comment.as_deref().unwrap_or(""))));
    }
    let ttl = |t: &TableSchema| t.options.get("ttl").map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if !stable && ttl(old) != ttl(new) {
        out.push(format!("{head} TTL {};", ttl(new).unwrap_or_else(|| "0".into())));
    }
    out.extend(add_ix);
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::kinds;

    fn c(name: &str, ty: &str, tag: bool) -> ColumnDef {
        let mut c = ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() };
        if tag {
            c.options.insert(TAG.into(), "true".into());
        }
        c
    }

    fn st() -> TableSchema {
        TableSchema {
            kind: SUPERTABLE.into(),
            schema: Some("iot".into()),
            name: "medidas".into(),
            columns: vec![c("ts", "TIMESTAMP", false), c("v", "DOUBLE", false), c("nota", "VARCHAR(20)", false), c("loc", "VARCHAR(20)", true), c("grp", "INT", true)],
            ..Default::default()
        }
    }

    #[test]
    fn tag_indexes() {
        let ix = |n: &str, c: &str| IndexDef { name: n.into(), columns: vec![c.into()], ..Default::default() };
        let mut old = st();
        old.indexes = vec![ix("ix_grp", "grp"), ix("ix_viejo", "loc")];
        let mut new = st();
        new.indexes = vec![ix("ix_grp", "grp"), ix("ix_loc", "loc")];
        let s = sync_script(&[TableChange::Alter { old, new: new.clone() }]).unwrap();
        assert_eq!(s.statements, vec!["DROP INDEX `iot`.`ix_viejo`;", "CREATE INDEX `ix_loc` ON `iot`.`medidas` (`loc`);"]);
        let s = sync_script(&[TableChange::Create { table: new }]).unwrap();
        assert!(s.statements[0].ends_with("CREATE INDEX `ix_grp` ON `iot`.`medidas` (`grp`);\nCREATE INDEX `ix_loc` ON `iot`.`medidas` (`loc`);"), "{}", s.statements[0]);
        assert!(crate::ddl::is_implicit_index("medidas", Some("loc"), "loc_medidas", "loc"));
        assert!(!crate::ddl::is_implicit_index("medidas", Some("loc"), "ix_grp", "grp"));
    }

    #[test]
    fn supertable_columns_and_tags() {
        let mut new = st();
        new.columns[1].data_type = "FLOAT".into();
        new.columns[2].data_type = "VARCHAR(50)".into();
        new.columns[3].data_type = "VARCHAR(64)".into();
        new.columns.remove(4);
        new.columns.push(c("extra", "INT", false));
        new.columns.push(c("zona", "NCHAR(10)", true));
        new.comment = Some("o'k".into());
        let s = sync_script(&[TableChange::Alter { old: st(), new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER STABLE `iot`.`medidas` MODIFY COLUMN `nota` VARCHAR(50);",
                "ALTER STABLE `iot`.`medidas` MODIFY TAG `loc` VARCHAR(64);",
                "ALTER STABLE `iot`.`medidas` DROP TAG `grp`;",
                "ALTER STABLE `iot`.`medidas` ADD COLUMN `extra` INT;",
                "ALTER STABLE `iot`.`medidas` ADD TAG `zona` NCHAR(10);",
                "ALTER STABLE `iot`.`medidas` COMMENT 'o''k';",
            ]
        );
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
    }

    #[test]
    fn tables_create_drop_and_guards() {
        let t = TableSchema { kind: kinds::TABLE.into(), columns: vec![c("ts", "TIMESTAMP", false), c("v", "DOUBLE", false)], ..st() };
        let mut n = t.clone();
        n.columns.remove(0);
        n.options.insert("ttl".into(), "7".into());
        let s = sync_script(&[TableChange::Alter { old: t.clone(), new: n }]).unwrap();
        assert_eq!(s.statements, vec!["ALTER TABLE `iot`.`medidas` TTL 7;"]);
        assert!(s.warnings[0].contains("primera columna"));

        let s = sync_script(&[TableChange::Alter { old: t.clone(), new: st() }]).unwrap();
        assert!(s.statements.is_empty() && s.warnings[0].contains("supertabla"));

        let mut other = t.clone();
        other.name = "vieja".into();
        let s = sync_script(&[TableChange::Create { table: st() }, TableChange::Drop { table: other }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE `iot`.`vieja`;");
        assert!(s.statements[1].starts_with("CREATE STABLE `iot`.`medidas`"));
    }
}

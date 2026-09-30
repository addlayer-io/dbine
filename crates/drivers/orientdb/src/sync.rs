//! Schema sync in OrientDB SQL: classes are created and dropped, declared
//! properties are created, dropped and altered (`ALTER PROPERTY … TYPE /
//! NOTNULL / MANDATORY / READONLY / DEFAULT / MIN / MAX / REGEXP /
//! LINKEDCLASS`), indexes are dropped and made again and `ABSTRACT` changes
//! with `ALTER CLASS`. Fields only seen in the data (option `inferred`)
//! aren't schema and are left alone.

use crate::ddl::{ident_index, opt, property, string, table_ddl, truthy};
use crate::ident;
use dbine_driver::{ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: false, foreign_keys: false };
const INDEXES: DdlParts = DdlParts { drop: false, if_exists: false, create: false, indexes: true, foreign_keys: false };
const FKS: DdlParts = DdlParts { drop: false, if_exists: false, create: false, indexes: false, foreign_keys: true };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

#[derive(Default)]
struct Plan {
    drops: Vec<String>,
    pre: Vec<String>,
    creates: Vec<String>,
    columns: Vec<String>,
    post: Vec<String>,
    warnings: Vec<String>,
}

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The type `CREATE PROPERTY` writes.
fn ty(c: &ColumnDef) -> String {
    c.data_type.split('|').next().map(str::trim).filter(|t| !t.is_empty()).unwrap_or("ANY").to_ascii_uppercase()
}

fn declared(t: &TableSchema) -> Vec<&ColumnDef> {
    t.columns.iter().filter(|c| !c.name.starts_with('@') && !truthy(opt(c, "inferred"))).collect()
}

fn ix_type(i: &IndexDef) -> String {
    i.kind.clone().filter(|k| !k.is_empty()).unwrap_or_else(|| if i.unique { "UNIQUE".into() } else { "NOTUNIQUE".into() }).to_ascii_uppercase()
}

fn ix_same(a: &IndexDef, b: &IndexDef) -> bool {
    let cols = |x: &[String]| x.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>();
    cols(&a.columns) == cols(&b.columns) && ix_type(a) == ix_type(b) && a.options == b.options
}

/// Statements as `table_ddl` writes them, one per chunk.
fn chunks(s: String) -> impl Iterator<Item = String> {
    s.lines().map(str::to_string).filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().into_iter()
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let mut p = Plan::default();
    for ch in changes {
        match ch {
            TableChange::Create { table } => {
                // Classes first, then their indexes and links (a LINK can point to a class made in the same sync).
                p.creates.extend(chunks(table_ddl(table, CREATE)?));
                p.post.extend(chunks(table_ddl(table, INDEXES)?));
                p.post.extend(chunks(table_ddl(table, FKS)?));
            }
            TableChange::Drop { table } => {
                p.warnings.push(format!("Se borra la clase {} con todos sus registros.", table.name));
                p.drops.extend(chunks(table_ddl(table, DROP)?));
            }
            TableChange::Alter { old, new } => alter(old, new, &mut p)?,
        }
    }
    let statements = [p.drops, p.pre, p.creates, p.columns, p.post].into_iter().flatten().collect();
    Ok(SyncScript { statements, warnings: p.warnings })
}

fn alter(old: &TableSchema, new: &TableSchema, p: &mut Plan) -> Result<()> {
    let t = new.name.as_str();
    let class = ident(t);
    let prop = |c: &str| format!("{class}.{}", ident(c));

    let sup = |x: &TableSchema| {
        let mut v: Vec<String> =
            x.options.get("extends").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        v.sort();
        v
    };
    if sup(old) != sup(new) || old.kind != new.kind {
        p.warnings.push(format!("{t}: la clase de la que hereda (EXTENDS) cambia; no se modifica sola, revisala a mano."));
    }
    let abs = |x: &TableSchema| truthy(x.options.get("abstract").map(String::as_str));
    if abs(old) != abs(new) {
        p.columns.push(format!("ALTER CLASS {class} ABSTRACT {};", abs(new)));
    }

    // Properties.
    let (od, nd) = (declared(old), declared(new));
    let dropped: Vec<&&ColumnDef> = od.iter().filter(|c| !nd.iter().any(|n| eq_name(&n.name, &c.name))).collect();
    for c in &dropped {
        p.warnings.push(format!("Se borra la propiedad {t}.{} del esquema (con sus índices); los registros conservan sus valores.", c.name));
        p.columns.push(format!("DROP PROPERTY {} FORCE;", prop(&c.name)));
    }
    for c in nd.iter().filter(|c| !od.iter().any(|o| eq_name(&o.name, &c.name))) {
        p.columns.push(format!("{};", property(t, c, false)));
    }
    for n in &nd {
        let Some(o) = od.iter().find(|o| eq_name(&o.name, &n.name)) else { continue };
        let at = prop(&n.name);
        if ty(o) != ty(n) {
            p.warnings.push(format!("{t}.{}: {} → {}. Puede fallar si los valores que ya existen no se convierten.", n.name, ty(o), ty(n)));
            p.columns.push(format!("ALTER PROPERTY {at} TYPE {};", ty(n)));
        }
        if opt(o, "linked") != opt(n, "linked") {
            p.columns.push(format!("ALTER PROPERTY {at} LINKEDCLASS {};", opt(n, "linked").map(ident).unwrap_or_else(|| "null".into())));
        }
        if o.nullable != n.nullable {
            if !n.nullable {
                p.warnings.push(format!("{t}.{} pasa a NOT NULL: falla si hay registros con null.", n.name));
            }
            p.columns.push(format!("ALTER PROPERTY {at} NOTNULL {};", !n.nullable));
        }
        for (k, kw) in [("mandatory", "MANDATORY"), ("readonly", "READONLY")] {
            if truthy(opt(o, k)) != truthy(opt(n, k)) {
                p.columns.push(format!("ALTER PROPERTY {at} {kw} {};", truthy(opt(n, k))));
            }
        }
        let default = |c: &ColumnDef| c.default_value.clone().filter(|d| !d.is_empty());
        if default(o) != default(n) {
            p.columns.push(format!("ALTER PROPERTY {at} DEFAULT {};", default(n).as_deref().map(string).unwrap_or_else(|| "null".into())));
        }
        for (k, kw) in [("min", "MIN"), ("max", "MAX"), ("regexp", "REGEXP")] {
            if opt(o, k) != opt(n, k) {
                p.columns.push(format!("ALTER PROPERTY {at} {kw} {};", opt(n, k).map(string).unwrap_or_else(|| "null".into())));
            }
        }
    }
    // Links the designer gives as foreign keys (without the `linked` option).
    for fk in &new.foreign_keys {
        let known = old.foreign_keys.iter().any(|o| o.columns == fk.columns && eq_name(&o.ref_table, &fk.ref_table));
        if !known {
            p.post.extend(chunks(table_ddl(&TableSchema { foreign_keys: vec![fk.clone()], ..new.clone() }, FKS)?));
        }
    }

    // Primary key (a UNIQUE index) and indexes, by name.
    let pk_ix = |x: &TableSchema| {
        x.primary_key.as_ref().filter(|k| !k.columns.is_empty()).map(|k| IndexDef {
            name: k.name.clone().unwrap_or_else(|| format!("{}.pk", x.name)),
            columns: k.columns.clone(),
            unique: true,
            kind: Some("UNIQUE".into()),
            filter: None,
            ..Default::default()
        })
    };
    let old_ix: Vec<IndexDef> = old.indexes.iter().cloned().chain(pk_ix(old)).collect();
    let new_ix: Vec<IndexDef> = new.indexes.iter().cloned().chain(pk_ix(new)).collect();
    let on_dropped = |i: &IndexDef| i.columns.iter().any(|c| dropped.iter().any(|d| eq_name(&d.name, c)));
    for o in &old_ix {
        let changed = new_ix.iter().find(|n| eq_name(&n.name, &o.name)).is_none_or(|n| !ix_same(o, n));
        // DROP PROPERTY … FORCE already takes the indexes on it.
        if changed && !on_dropped(o) {
            p.pre.push(format!("DROP INDEX {};", ident_index(&o.name)));
        }
    }
    let add: Vec<IndexDef> = new_ix.into_iter().filter(|n| old_ix.iter().find(|o| eq_name(&o.name, &n.name)).is_none_or(|o| !ix_same(o, n))).collect();
    if !add.is_empty() {
        p.post.extend(chunks(table_ddl(&TableSchema { indexes: add, primary_key: None, ..new.clone() }, INDEXES)?));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::KeyDef;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn class(cols: Vec<ColumnDef>, indexes: Vec<IndexDef>) -> TableSchema {
        TableSchema { kind: "vertex".into(), name: "Person".into(), columns: cols, indexes, ..Default::default() }
    }

    fn ix(name: &str, cols: &[&str], ty: &str) -> IndexDef {
        IndexDef { name: name.into(), columns: cols.iter().map(|s| s.to_string()).collect(), unique: ty == "UNIQUE", kind: Some(ty.into()), filter: None, ..Default::default() }
    }

    #[test]
    fn alter_properties_and_indexes() {
        let mut inferred = col("nick", "STRING");
        inferred.options.insert("inferred".into(), "true".into());
        let old = class(
            vec![col("name", "STRING"), col("age", "INTEGER"), col("legacy", "STRING"), inferred.clone()],
            vec![ix("Person.name", &["name"], "NOTUNIQUE"), ix("Person.legacy", &["legacy"], "NOTUNIQUE")],
        );
        let mut name = col("name", "STRING");
        name.nullable = false;
        name.options.insert("mandatory".into(), "true".into());
        let mut age = col("age", "LONG");
        age.options.insert("min".into(), "0".into());
        let new = class(vec![name, age, col("email", "STRING"), inferred], vec![ix("Person.name", &["name"], "UNIQUE"), ix("Person.email", &["email"], "NOTUNIQUE")]);
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX Person.name;",
                "DROP PROPERTY Person.legacy FORCE;",
                "CREATE PROPERTY Person.email STRING;",
                "ALTER PROPERTY Person.name NOTNULL true;",
                "ALTER PROPERTY Person.name MANDATORY true;",
                "ALTER PROPERTY Person.age TYPE LONG;",
                "ALTER PROPERTY Person.age MIN '0';",
                "CREATE INDEX Person.name ON Person (name) UNIQUE;",
                "CREATE INDEX Person.email ON Person (email) NOTUNIQUE;",
            ]
        );
        assert_eq!(
            s.warnings,
            vec![
                "Se borra la propiedad Person.legacy del esquema (con sus índices); los registros conservan sus valores.",
                "Person.name pasa a NOT NULL: falla si hay registros con null.",
                "Person.age: INTEGER → LONG. Puede fallar si los valores que ya existen no se convierten.",
            ]
        );
    }

    #[test]
    fn create_and_drop() {
        let mut t = class(vec![col("name", "STRING")], vec![]);
        t.primary_key = Some(KeyDef { name: None, columns: vec!["name".into()] });
        let gone = TableSchema { name: "Old".into(), ..class(vec![], vec![]) };
        let s = sync_script(&[TableChange::Create { table: t }, TableChange::Drop { table: gone }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP CLASS Old UNSAFE;",
                "CREATE CLASS Person EXTENDS V;",
                "CREATE PROPERTY Person.name STRING;",
                "CREATE INDEX Person.pk ON Person (name) UNIQUE;",
            ]
        );
        assert_eq!(s.warnings, vec!["Se borra la clase Old con todos sus registros."]);
    }
}

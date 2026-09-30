//! Schema sync in CQL: tables are created and dropped, regular columns are
//! added and dropped, secondary indexes are dropped and made again and the
//! table options are changed with `ALTER TABLE … WITH`. CQL can't change a
//! column's type or the primary key: those become warnings.

use crate::cql::{ident, qualified};
use crate::ddl::{index_ddl, is_true, keys, table_options, table_ddl};
use dbine_driver::{ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: false, foreign_keys: false };
const INDEXES: DdlParts = DdlParts { drop: false, if_exists: false, create: false, indexes: true, foreign_keys: false };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

#[derive(Default)]
struct Plan {
    drop_tables: Vec<String>,
    pre: Vec<String>,
    columns: Vec<String>,
    post: Vec<String>,
    creates: Vec<String>,
    warnings: Vec<String>,
}

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn ix_same(a: &IndexDef, b: &IndexDef) -> bool {
    let cols = |x: &[String]| x.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>();
    let kind = |x: &Option<String>| x.as_deref().map(str::trim).unwrap_or("").to_lowercase();
    cols(&a.columns) == cols(&b.columns) && a.unique == b.unique && kind(&a.kind) == kind(&b.kind) && a.options == b.options
}

/// Partition key, clustering columns and their order, lower-cased.
fn key_shape(t: &TableSchema) -> Result<(Vec<String>, Vec<(String, bool)>)> {
    let (pk, ck) = keys(t)?;
    let desc = |c: &ColumnDef| c.options.get("clustering_order").is_some_and(|o| o.trim().eq_ignore_ascii_case("desc"));
    Ok((pk.iter().map(|c| c.name.to_lowercase()).collect(), ck.iter().map(|c| (c.name.to_lowercase(), desc(c))).collect()))
}

pub fn sync_script(keyspaces: bool, changes: &[TableChange]) -> Result<SyncScript> {
    let mut p = Plan::default();
    for ch in changes {
        match ch {
            TableChange::Create { table } => {
                p.creates.push(table_ddl(table, CREATE)?);
                if !table.indexes.is_empty() {
                    if keyspaces {
                        p.warnings.push(format!("Amazon Keyspaces no tiene índices secundarios: los índices de {} no se crean.", display(table)));
                    } else {
                        p.creates.push(table_ddl(table, INDEXES)?);
                    }
                }
            }
            TableChange::Drop { table } => {
                p.warnings.push(format!("Se borra la tabla {} con todos sus datos.", display(table)));
                p.drop_tables.push(table_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => alter_table(keyspaces, old, new, &mut p)?,
        }
    }
    let statements = [p.drop_tables, p.pre, p.columns, p.post, p.creates].into_iter().flatten().map(|s| s.trim_end().to_string()).filter(|s| !s.is_empty()).collect();
    Ok(SyncScript { statements, warnings: p.warnings })
}

fn alter_table(keyspaces: bool, old: &TableSchema, new: &TableSchema, p: &mut Plan) -> Result<()> {
    let schema = new.schema.as_deref().filter(|s| !s.is_empty());
    let name = qualified(schema, &new.name);
    let tname = display(new);

    let (old_pk, old_ck) = key_shape(old)?;
    let (new_pk, new_ck) = key_shape(new)?;
    let is_key = |pk: &[String], ck: &[(String, bool)], c: &str| {
        let c = c.to_lowercase();
        pk.contains(&c) || ck.iter().any(|(k, _)| *k == c)
    };
    let old_key = |c: &str| is_key(&old_pk, &old_ck, c);
    let new_key = |c: &str| is_key(&new_pk, &new_ck, c);

    let dropped: Vec<&ColumnDef> = old.columns.iter().filter(|c| !new.columns.iter().any(|n| eq_name(&n.name, &c.name))).collect();
    let added: Vec<&ColumnDef> = new.columns.iter().filter(|c| !old.columns.iter().any(|o| eq_name(&o.name, &c.name))).collect();
    let pairs: Vec<(&ColumnDef, &ColumnDef)> =
        new.columns.iter().filter_map(|n| old.columns.iter().find(|o| eq_name(&o.name, &n.name)).map(|o| (o, n))).collect();
    let key_retyped = pairs.iter().any(|(o, n)| (old_key(&o.name) || new_key(&n.name)) && squash(&o.data_type) != squash(&n.data_type));

    if old_pk != new_pk || old_ck != new_ck || key_retyped {
        p.warnings.push(format!(
            "La clave primaria de {tname} cambia y CQL no la modifica: hay que recrear la tabla (borrarla y crearla de nuevo, con sus datos). Se deja como está."
        ));
    }

    // Indexes: by name; a changed one, or one on a column that goes, is dropped first.
    let mut drop_ix = Vec::new();
    let mut new_ix = Vec::new();
    for o in &old.indexes {
        let same = new.indexes.iter().find(|n| eq_name(&n.name, &o.name));
        let on_dropped = o.columns.iter().any(|c| dropped.iter().any(|d| !old_key(&d.name) && c.to_lowercase().contains(&d.name.to_lowercase())));
        if same.is_none_or(|n| !ix_same(o, n)) || on_dropped {
            drop_ix.push(format!("DROP INDEX {};", qualified(schema, &o.name)));
        }
    }
    for n in &new.indexes {
        let same = old.indexes.iter().find(|o| eq_name(&o.name, &n.name));
        if same.is_none_or(|o| !ix_same(o, n)) {
            new_ix.push(n);
        }
    }
    if keyspaces {
        if !drop_ix.is_empty() || !new_ix.is_empty() {
            p.warnings.push(format!("Amazon Keyspaces no tiene índices secundarios: los cambios de índices de {tname} no se aplican."));
        }
    } else {
        p.pre.extend(drop_ix);
        for ix in new_ix {
            p.post.push(index_ddl(&name, ix, false)?);
        }
    }

    // Columns: only the regular ones (key columns can't be added or dropped).
    for c in dropped.iter().filter(|c| !old_key(&c.name)) {
        p.warnings.push(format!("Se borra la columna {tname}.{} con sus datos.", c.name));
        p.columns.push(format!("ALTER TABLE {name} DROP {};", ident(&c.name)));
    }
    for c in added.iter().filter(|c| !new_key(&c.name)) {
        if c.data_type.trim().is_empty() {
            p.warnings.push(format!("Falta el tipo de la columna {tname}.{}: no se agrega.", c.name));
            continue;
        }
        let stat = if is_true(c, "static") { " STATIC" } else { "" };
        p.columns.push(format!("ALTER TABLE {name} ADD {} {}{stat};", ident(&c.name), c.data_type.trim()));
    }
    for (o, n) in pairs.iter().filter(|(o, n)| !old_key(&o.name) && !new_key(&n.name)) {
        if squash(&o.data_type) != squash(&n.data_type) {
            p.warnings.push(format!("{tname}.{}: {} → {}. CQL no cambia el tipo de una columna; se deja como está.", n.name, o.data_type, n.data_type));
        }
        if is_true(o, "static") != is_true(n, "static") {
            p.warnings.push(format!("{tname}.{}: CQL no cambia si una columna es STATIC; se deja como está.", n.name));
        }
    }

    // Table options.
    let old_opts = table_options(old, &[])?;
    let new_opts = table_options(new, &[])?;
    let key = |o: &str| o.split_once(" = ").map(|(k, _)| k.to_string()).unwrap_or_default();
    let set: Vec<&String> = new_opts.iter().filter(|o| !old_opts.contains(o)).collect();
    if !set.is_empty() {
        p.post.push(format!("ALTER TABLE {name} WITH {};", set.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" AND ")));
    }
    for o in old_opts.iter().filter(|o| !new_opts.iter().any(|n| key(n) == key(o))) {
        p.warnings.push(format!("{tname}: la opción {} se quitó en el origen; CQL no la vuelve al valor por defecto, se deja como está.", key(o)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::KeyDef;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), ..Default::default() }
    }

    fn table(cols: Vec<ColumnDef>, pk: &[&str]) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("ks".into()),
            name: "users".into(),
            columns: cols,
            primary_key: Some(KeyDef { name: None, columns: pk.iter().map(|s| s.to_string()).collect() }),
            ..Default::default()
        }
    }

    fn ix(name: &str, col: &str) -> IndexDef {
        IndexDef { name: name.into(), columns: vec![col.into()], ..Default::default() }
    }

    #[test]
    fn alter_columns_indexes_and_options() {
        let mut old = table(vec![col("id", "uuid"), col("name", "text"), col("age", "int"), col("old", "text")], &["id"]);
        old.indexes = vec![ix("users_old", "old"), ix("users_name", "name")];
        let mut new = table(vec![col("id", "uuid"), col("name", "text"), col("age", "bigint"), col("Email", "text")], &["id"]);
        new.indexes = vec![ix("users_email", "Email"), ix("users_name", "name")];
        new.options.insert("default_time_to_live".into(), "3600".into());
        let s = sync_script(false, &[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX ks.users_old;",
                "ALTER TABLE ks.users DROP old;",
                "ALTER TABLE ks.users ADD \"Email\" text;",
                "CREATE INDEX users_email ON ks.users (\"Email\");",
                "ALTER TABLE ks.users WITH default_time_to_live = 3600;",
            ]
        );
        assert_eq!(
            s.warnings,
            vec![
                "Se borra la columna ks.users.old con sus datos.",
                "ks.users.age: int → bigint. CQL no cambia el tipo de una columna; se deja como está.",
            ]
        );
    }

    #[test]
    fn primary_key_change_is_a_warning() {
        let old = table(vec![col("id", "uuid"), col("ts", "timestamp"), col("v", "text")], &["id"]);
        let new = table(vec![col("id", "uuid"), col("ts", "timestamp"), col("v", "text")], &["id", "ts"]);
        let s = sync_script(false, &[TableChange::Alter { old, new }]).unwrap();
        assert!(s.statements.is_empty());
        assert_eq!(
            s.warnings,
            vec!["La clave primaria de ks.users cambia y CQL no la modifica: hay que recrear la tabla (borrarla y crearla de nuevo, con sus datos). Se deja como está."]
        );
    }

    #[test]
    fn create_and_drop() {
        let mut t = table(vec![col("id", "uuid"), col("name", "text")], &["id"]);
        t.indexes = vec![ix("users_name", "name")];
        let gone = TableSchema { name: "legacy".into(), ..table(vec![col("id", "int")], &["id"]) };
        let s = sync_script(false, &[TableChange::Create { table: t.clone() }, TableChange::Drop { table: gone }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP TABLE ks.legacy;",
                "CREATE TABLE ks.users (\n    id uuid,\n    name text,\n    PRIMARY KEY (id)\n);",
                "CREATE INDEX users_name ON ks.users (name);",
            ]
        );
        assert_eq!(s.warnings, vec!["Se borra la tabla ks.legacy con todos sus datos."]);

        let s = sync_script(true, &[TableChange::Create { table: t }]).unwrap();
        assert_eq!(s.statements.len(), 1);
        assert_eq!(s.warnings, vec!["Amazon Keyspaces no tiene índices secundarios: los índices de ks.users no se crean."]);
    }
}

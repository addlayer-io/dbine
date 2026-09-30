//! Schema sync ("Comparar esquemas") for Neo4j and Memgraph. Nodes and
//! relationships have no declared structure: a label's (or relationship
//! type's) schema is its indexes and constraints, which are dropped and
//! created again when they change. Labels themselves are never created or
//! dropped (they exist while nodes carry them), so a dropped label only
//! loses its indexes and constraints.

use crate::ddl::{create, drop, table_ddl, IndexSpec, CONSTRAINT};
use crate::{Flavor, RELATIONSHIP};
use dbine_driver::{kinds, DdlParts, Error, Result, SyncScript, TableChange, TableSchema};

fn specs(t: &TableSchema) -> Vec<IndexSpec> {
    let rel = t.kind == RELATIONSHIP;
    t.indexes.iter().map(|ix| IndexSpec::from_index(&t.name, rel, ix)).collect()
}

fn is_designed(t: &TableSchema) -> bool {
    t.kind == kinds::INDEX || t.kind == CONSTRAINT
}

fn same(a: &IndexSpec, b: &IndexSpec) -> bool {
    let props = |s: &IndexSpec| s.properties.iter().map(|p| p.to_lowercase()).collect::<Vec<_>>();
    a.kind.eq_ignore_ascii_case(&b.kind) && a.relationship == b.relationship && a.target == b.target && props(a) == props(b) && a.options == b.options
}

/// Neo4j matches indexes by name; Memgraph's have none (its TEXT ones do),
/// so there they match by what they index.
fn pairs(f: Flavor, a: &IndexSpec, b: &IndexSpec) -> bool {
    match f {
        Flavor::Neo4j => a.name.eq_ignore_ascii_case(&b.name),
        _ => same(a, b),
    }
}

fn stmt(s: String) -> String {
    format!("{s};")
}

pub fn sync_script(f: Flavor, changes: &[TableChange]) -> Result<SyncScript> {
    if f == Flavor::Neptune {
        return Err(Error::Unsupported(
            "Neptune no tiene esquema que sincronizar: indexa todo por su cuenta y no tiene restricciones definidas por el usuario".into(),
        ));
    }
    let (mut drops, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new());
    let what = |t: &TableSchema| if t.kind == RELATIONSHIP { "El tipo de relación" } else { "La etiqueta" };
    for ch in changes {
        match ch {
            TableChange::Create { table } if is_designed(table) => creates.push(table_ddl(f, table, DdlParts { create: true, ..Default::default() })?),
            TableChange::Drop { table } if is_designed(table) => drops.push(table_ddl(f, table, DdlParts { drop: true, ..Default::default() })?),
            TableChange::Alter { old, new } if is_designed(old) || is_designed(new) => {
                let (o, n) = (table_ddl(f, old, DdlParts { create: true, ..Default::default() })?, table_ddl(f, new, DdlParts { create: true, ..Default::default() })?);
                if o != n {
                    drops.push(table_ddl(f, old, DdlParts { drop: true, ..Default::default() })?);
                    creates.push(n);
                }
            }
            TableChange::Create { table } => {
                for s in specs(table) {
                    creates.push(stmt(create(f, &s, false)?));
                }
            }
            TableChange::Drop { table } => {
                warnings.push(format!(
                    "{} {} no se borra: sus {} quedan. Solo se borran sus índices y restricciones.",
                    what(table),
                    table.name,
                    if table.kind == RELATIONSHIP { "relaciones" } else { "nodos" }
                ));
                for s in specs(table) {
                    drops.push(stmt(drop(f, &s, false)?));
                }
            }
            TableChange::Alter { old, new } => {
                let (os, ns) = (specs(old), specs(new));
                for o in &os {
                    if !ns.iter().any(|n| pairs(f, o, n) && same(o, n)) {
                        drops.push(stmt(drop(f, o, false)?));
                    }
                }
                for n in &ns {
                    if !os.iter().any(|o| pairs(f, o, n) && same(o, n)) {
                        creates.push(stmt(create(f, n, false)?));
                    }
                }
                let props = |t: &TableSchema| {
                    let mut p: Vec<String> = t.columns.iter().map(|c| c.name.to_lowercase()).collect();
                    p.sort();
                    p
                };
                if props(old) != props(new) {
                    warnings.push(format!(
                        "{} {}: las propiedades no se declaran en un grafo; las que faltan aparecen al escribirlas y las que sobran quedan en los datos.",
                        what(new),
                        new.name
                    ));
                }
            }
        }
    }
    // A full-text index over several labels comes with each of them: once.
    let mut statements: Vec<String> = Vec::new();
    for s in [drops, creates].concat() {
        if !statements.contains(&s) {
            statements.push(s);
        }
    }
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LABEL;
    use dbine_driver::{ColumnDef, IndexDef};

    fn ix(name: &str, kind: &str, props: &[&str]) -> IndexDef {
        IndexDef { name: name.into(), columns: props.iter().map(|p| p.to_string()).collect(), unique: kind == "UNIQUE", kind: Some(kind.into()), filter: None, ..Default::default() }
    }

    fn label(indexes: Vec<IndexDef>) -> TableSchema {
        TableSchema {
            kind: LABEL.into(),
            name: "Person".into(),
            columns: vec![ColumnDef { name: "name".into(), ..Default::default() }],
            indexes,
            ..Default::default()
        }
    }

    #[test]
    fn neo4j_indexes_and_constraints() {
        let old = label(vec![ix("ix_name", "RANGE", &["name"]), ix("u_id", "UNIQUE", &["id"]), ix("gone", "TEXT", &["bio"])]);
        let mut new = label(vec![ix("ix_name", "RANGE", &["name", "age"]), ix("u_id", "UNIQUE", &["id"]), ix("ex", "EXISTS", &["name"])]);
        new.columns.push(ColumnDef { name: "age".into(), ..Default::default() });
        let s = sync_script(Flavor::Neo4j, &[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX ix_name;",
                "DROP INDEX gone;",
                "CREATE INDEX ix_name\nFOR (e:Person) ON (e.name, e.age);",
                "CREATE CONSTRAINT ex\nFOR (e:Person) REQUIRE e.name IS NOT NULL;",
            ]
        );
        assert_eq!(s.warnings.len(), 1);
    }

    #[test]
    fn memgraph_and_labels() {
        let old = label(vec![ix("", "RANGE", &["name"])]);
        let new = label(vec![ix("", "RANGE", &["name"]), ix("", "UNIQUE", &["id"])]);
        let s = sync_script(Flavor::Memgraph, &[TableChange::Alter { old: old.clone(), new }]).unwrap();
        assert_eq!(s.statements, vec!["CREATE CONSTRAINT ON (n:Person) ASSERT n.id IS UNIQUE;"]);

        let s = sync_script(Flavor::Memgraph, &[TableChange::Drop { table: old.clone() }]).unwrap();
        assert_eq!(s.statements, vec!["DROP INDEX ON :Person(name);"]);
        assert!(s.warnings[0].contains("no se borra"));

        let s = sync_script(Flavor::Neo4j, &[TableChange::Create { table: label(vec![ix("u", "UNIQUE", &["id"])]) }]).unwrap();
        assert_eq!(s.statements, vec!["CREATE CONSTRAINT u\nFOR (e:Person) REQUIRE e.id IS UNIQUE;"]);

        assert!(matches!(sync_script(Flavor::Neptune, &[]), Err(Error::Unsupported(_))));
    }
}

//! The lineage of the whole schema (`Session::lineage_graph`), from HANA's
//! catalog in three queries instead of one per object:
//!
//! - `SYS.OBJECT_DEPENDENCIES` (`DEPENDENCY_TYPE` 1, the direct ones) for
//!   every view, procedure, function, trigger, synonym and sequence of the
//!   schema (confirmed), with the base object's type (a table that is a
//!   table type is a type).
//! - The foreign keys (`SYS.REFERENTIAL_CONSTRAINTS`), confirmed.
//! - The schema's virtual tables (`SYS.VIRTUAL_TABLES`): the remote source,
//!   database, owner and object each reads go in `external`, with the
//!   source's adapter (`SYS.REMOTE_SOURCES`) as the system when the login
//!   can read it.
//!
//! An object of another schema is external too (its owner as the schema).
//! `SYS` and `PUBLIC` objects are left out. Jobs come from `Session::jobs`
//! and `job_references`. A catalog view the login can't read gives the
//! generic scan, with a note.

use crate::references::{cell, kind_of, remote, system, text_rows, Rows};
use crate::HanaSession;
use dbine_driver::dependencies::{
    graph_scan, ExternalRef, LineageEdge, LineageExternal, LineageGraph, LineageJob, LineageScope,
    GRAPH_OBJECTS, JOBS_READ,
};
use dbine_driver::{
    kinds, Confidence, DependencyScan, Dependent, Error, ObjectRef, Relation, Result, Session,
};

/// Every direct dependency of the schema's objects (`?`: the schema):
/// the object and its type, then what it uses as `USES` reads it.
const DEPS: &str = "SELECT d.DEPENDENT_OBJECT_NAME, d.DEPENDENT_OBJECT_TYPE,
       d.BASE_SCHEMA_NAME, d.BASE_OBJECT_NAME, d.BASE_OBJECT_TYPE, t.IS_USER_DEFINED_TYPE
  FROM SYS.OBJECT_DEPENDENCIES d
  LEFT JOIN SYS.TABLES t
    ON d.BASE_OBJECT_TYPE = 'TABLE' AND t.SCHEMA_NAME = d.BASE_SCHEMA_NAME AND t.TABLE_NAME = d.BASE_OBJECT_NAME
 WHERE d.DEPENDENT_SCHEMA_NAME = ? AND d.DEPENDENCY_TYPE = 1
   AND d.DEPENDENT_OBJECT_TYPE IN ('VIEW', 'PROCEDURE', 'FUNCTION', 'TRIGGER', 'SYNONYM', 'SEQUENCE')
 ORDER BY d.DEPENDENT_OBJECT_NAME, d.BASE_SCHEMA_NAME, d.BASE_OBJECT_NAME";

/// The schema's foreign keys: table, constraint, referenced schema and table.
const FOREIGN_KEYS: &str =
    "SELECT DISTINCT TABLE_NAME, CONSTRAINT_NAME, REFERENCED_SCHEMA_NAME, REFERENCED_TABLE_NAME
  FROM SYS.REFERENTIAL_CONSTRAINTS WHERE SCHEMA_NAME = ?
 ORDER BY TABLE_NAME, CONSTRAINT_NAME";

/// The schema's virtual tables and their remote side.
const VIRTUAL_TABLES: &str = "SELECT TABLE_NAME, REMOTE_SOURCE_NAME, REMOTE_DATABASE_NAME, REMOTE_OWNER_NAME, REMOTE_OBJECT_NAME
  FROM SYS.VIRTUAL_TABLES WHERE SCHEMA_NAME = ?";

/// The kind of a `DEPENDENT_OBJECT_TYPE`.
fn dependent_kind(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "VIEW" => kinds::VIEW,
        "PROCEDURE" => kinds::PROCEDURE,
        "FUNCTION" => kinds::FUNCTION,
        "TRIGGER" => kinds::TRIGGER,
        "SYNONYM" => kinds::SYNONYM,
        "SEQUENCE" => kinds::SEQUENCE,
        _ => return None,
    })
}

fn same(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind
        && a.name.eq_ignore_ascii_case(&b.name)
        && a.schema.as_deref().map(str::to_lowercase) == b.schema.as_deref().map(str::to_lowercase)
}

fn find(nodes: &[ObjectRef], o: &ObjectRef) -> Option<ObjectRef> {
    nodes.iter().find(|n| same(n, o)).cloned()
}

fn push_edge(
    g: &mut LineageGraph,
    from: &ObjectRef,
    to: &ObjectRef,
    relation: Relation,
    detail: Option<String>,
) {
    if same(from, to)
        || g.edges
            .iter()
            .any(|e| same(&e.from, from) && same(&e.to, to) && e.relation == relation)
    {
        return;
    }
    g.edges.push(LineageEdge {
        from: from.clone(),
        to: to.clone(),
        relation,
        confidence: Confidence::Confirmed,
        detail,
        mentions: Vec::new(),
    });
}

fn push_external(g: &mut LineageGraph, source: ExternalRef, to: &ObjectRef) {
    if !g
        .external
        .iter()
        .any(|x| x.source == source && same(&x.to, to))
    {
        g.external.push(LineageExternal {
            source,
            to: to.clone(),
        });
    }
}

/// A schema as the explorer writes it: none for the session's.
fn local(schema: &str, session: &str) -> Option<String> {
    (schema != session).then(|| schema.to_string())
}

/// The graph from the catalog's rows; `nodes` are the schema's objects in
/// scope, `session` the schema.
pub(crate) fn assemble(
    nodes: Vec<ObjectRef>,
    deps: &Rows,
    fks: &Rows,
    virtual_tables: &Rows,
    session: &str,
) -> LineageGraph {
    let mut g = LineageGraph::default();
    let other = |schema: &str, name: &str, kind: &str| ExternalRef {
        schema: Some(schema.to_string()),
        name: Some(name.to_string()),
        detail: Some(kind.to_string()),
        ..Default::default()
    };
    for r in deps {
        let (Some(name), Some(ty), Some(base_schema), Some(base_name), Some(base_ty)) =
            (cell(r, 0), cell(r, 1), cell(r, 2), cell(r, 3), cell(r, 4))
        else {
            continue;
        };
        let Some(kind) = dependent_kind(&ty) else {
            continue;
        };
        let Some(user) = find(
            &nodes,
            &ObjectRef {
                kind: kind.into(),
                schema: None,
                name,
            },
        ) else {
            continue;
        };
        if system(&base_schema) {
            continue;
        }
        let table_type = cell(r, 5).as_deref() == Some("TRUE");
        let Some(base_kind) = kind_of(&base_ty, table_type) else {
            continue;
        };
        let schema = local(&base_schema, session);
        let used = ObjectRef {
            kind: base_kind.into(),
            schema: schema.clone(),
            name: base_name.clone(),
        };
        match find(&nodes, &used) {
            Some(used) => push_edge(&mut g, &used, &user, Relation::Code, None),
            None if schema.is_some() => {
                push_external(&mut g, other(&base_schema, &base_name, base_kind), &user)
            }
            None => {}
        }
    }
    for r in fks {
        let (Some(table), Some(constraint), Some(ref_schema), Some(ref_table)) =
            (cell(r, 0), cell(r, 1), cell(r, 2), cell(r, 3))
        else {
            continue;
        };
        if system(&ref_schema) {
            continue;
        }
        let Some(child) = find(
            &nodes,
            &ObjectRef {
                kind: kinds::TABLE.into(),
                schema: None,
                name: table,
            },
        ) else {
            continue;
        };
        let schema = local(&ref_schema, session);
        let target = schema
            .as_deref()
            .map_or(ref_table.clone(), |s| format!("{s}.{ref_table}"));
        // The names HANA made up (`_SYS_…`) say nothing.
        let detail = Some(if constraint.starts_with("_SYS_") {
            format!("→ {target}")
        } else {
            format!("{constraint} → {target}")
        });
        let parent = ObjectRef {
            kind: kinds::TABLE.into(),
            schema: schema.clone(),
            name: ref_table.clone(),
        };
        match find(&nodes, &parent) {
            Some(parent) => push_edge(&mut g, &parent, &child, Relation::ForeignKey, detail),
            None if schema.is_some() => {
                push_external(&mut g, other(&ref_schema, &ref_table, kinds::TABLE), &child)
            }
            None => {}
        }
    }
    for r in virtual_tables {
        let Some(table) = cell(r, 0) else { continue };
        let Some(me) = find(
            &nodes,
            &ObjectRef {
                kind: kinds::TABLE.into(),
                schema: None,
                name: table,
            },
        ) else {
            continue;
        };
        if let Some(x) = remote(r, 1, None) {
            push_external(&mut g, x, &me);
        }
    }
    g.objects = nodes;
    g
}

/// The remote sources' adapters as their system.
async fn name_systems(s: &HanaSession, g: &mut LineageGraph) {
    let mut sources: Vec<ExternalRef> = g.external.iter().map(|x| x.source.clone()).collect();
    crate::references::name_systems(s, &mut sources).await;
    for (x, named) in g.external.iter_mut().zip(sources) {
        x.source = named;
    }
}

fn node_kind(scan: &DependencyScan, kind: &str) -> bool {
    kind == kinds::TABLE
        || kind == kinds::COLLECTION
        || scan.source_kinds.iter().any(|k| k == kind)
        || scan.written_kinds.iter().any(|k| k == kind)
}

pub(crate) async fn graph(
    s: &mut HanaSession,
    database: &str,
    scope: &LineageScope,
    scan: &DependencyScan,
) -> Result<LineageGraph> {
    let session = s.schema.clone();
    let limit = if scope.limit == 0 {
        GRAPH_OBJECTS
    } else {
        scope.limit
    } as usize;
    let mut nodes = Vec::new();
    let mut truncated = false;
    for o in s.list_objects().await? {
        let schema = o.schema.clone().unwrap_or_else(|| session.clone());
        if !node_kind(scan, &o.kind)
            || !(scope.kinds.is_empty() || scope.kinds.contains(&o.kind))
            || !(scope.schemas.is_empty()
                || scope
                    .schemas
                    .iter()
                    .any(|x| x.eq_ignore_ascii_case(&schema)))
        {
            continue;
        }
        if nodes.len() >= limit {
            truncated = true;
            break;
        }
        nodes.push(ObjectRef {
            kind: o.kind,
            schema: o.schema,
            name: o.name,
        });
    }
    let read = async {
        let deps = text_rows(s, DEPS, &[&session]).await?;
        let fks = text_rows(s, FOREIGN_KEYS, &[&session]).await?;
        // Not every login reads the virtual tables: the rest doesn't depend on them.
        let virtual_tables = text_rows(s, VIRTUAL_TABLES, &[&session])
            .await
            .unwrap_or_else(|e| {
                tracing::debug!("hana: SYS.VIRTUAL_TABLES not read: {e}");
                Vec::new()
            });
        Ok::<_, Error>((deps, fks, virtual_tables))
    }
    .await;
    let (deps, fks, virtual_tables) = match read {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("hana: the dependency catalog not read: {e}");
            let mut g = graph_scan(s, database, scope, scan).await?;
            g.notes.push(format!(
                "No se pudo leer SYS.OBJECT_DEPENDENCIES ni SYS.REFERENTIAL_CONSTRAINTS: {e}"
            ));
            return Ok(g);
        }
    };
    let mut g = assemble(nodes, &deps, &fks, &virtual_tables, &session);
    g.truncated = truncated;
    name_systems(s, &mut g).await;
    if scope.jobs {
        jobs(s, database, scan, &mut g).await;
    }
    Ok(g)
}

/// The scheduler's jobs, each with the objects its steps name.
async fn jobs(s: &mut HanaSession, database: &str, scan: &DependencyScan, g: &mut LineageGraph) {
    let list = match s.jobs().await {
        Ok(l) => l,
        Err(Error::Unsupported(_)) => return,
        Err(e) => {
            g.job_notes
                .push(format!("no se pudieron leer los jobs del servidor: {e}"));
            return;
        }
    };
    g.job_notes.extend(list.notes);
    let total = list.jobs.len();
    let scan = &dbine_driver::dependencies::with_databases(s, scan).await;
    for job in list.jobs.into_iter().take(JOBS_READ) {
        let r = match s.job_references(database, &job.id, scan).await {
            Ok(r) => r,
            Err(_) => {
                g.unreadable.push(job.name);
                continue;
            }
        };
        g.scanned += 1;
        let uses: Vec<Dependent> = r
            .items
            .into_iter()
            .filter(|d| {
                find(
                    &g.objects,
                    &ObjectRef {
                        kind: d.kind.clone(),
                        schema: d.schema.clone(),
                        name: d.name.clone(),
                    },
                )
                .is_some()
            })
            .collect();
        if uses.is_empty() && r.external.is_empty() {
            continue;
        }
        g.jobs.push(LineageJob {
            id: job.id,
            name: job.name,
            kind: job.kind,
            group: job.group,
            uses,
            external: r.external,
        });
    }
    if total > JOBS_READ {
        g.job_notes.push(format!(
            "se leyeron los primeros {JOBS_READ} jobs del servidor, de {total}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    fn row(cells: &[&str]) -> Vec<Option<String>> {
        cells
            .iter()
            .map(|c| (!c.is_empty()).then(|| c.to_string()))
            .collect()
    }

    #[test]
    fn the_queries_read_the_whole_schema_with_one_parameter() {
        for sql in [DEPS, FOREIGN_KEYS, VIRTUAL_TABLES] {
            assert_eq!(sql.matches('?').count(), 1, "{sql}");
        }
        assert!(DEPS.contains("d.DEPENDENCY_TYPE = 1"));
        assert!(!DEPS.contains("DEPENDENT_OBJECT_NAME = ?"));
    }

    #[test]
    fn rows_become_confirmed_edges_virtual_tables_external_and_foreign_keys() {
        let nodes = vec![
            o("table", "DEPT"),
            o("table", "EMP"),
            o("table", "VT"),
            o("view", "V_EMP"),
            o("procedure", "P_LOAD"),
            o("sequence", "SEQ"),
        ];
        let deps = vec![
            row(&["V_EMP", "VIEW", "APP", "EMP", "TABLE", "FALSE"]),
            row(&["V_EMP", "VIEW", "APP", "DEPT", "TABLE", "FALSE"]),
            row(&["V_EMP", "VIEW", "APP", "DEPT", "TABLE", "FALSE"]),
            row(&["P_LOAD", "PROCEDURE", "APP", "V_EMP", "VIEW", ""]),
            row(&["P_LOAD", "PROCEDURE", "APP", "SEQ", "SEQUENCE", ""]),
            row(&["P_LOAD", "PROCEDURE", "APP", "VT", "TABLE", "FALSE"]),
            // Another schema, SYS and an unknown object's type.
            row(&["P_LOAD", "PROCEDURE", "HR", "PEOPLE", "TABLE", "FALSE"]),
            row(&["P_LOAD", "PROCEDURE", "SYS", "M_CONNECTIONS", "VIEW", ""]),
            row(&["P_LOAD", "PROCEDURE", "APP", "IDX", "INDEX", ""]),
            // A table type is a type, not a table: no such node here.
            row(&["P_LOAD", "PROCEDURE", "APP", "T_ROWS", "TABLE", "TRUE"]),
            row(&["GONE", "VIEW", "APP", "EMP", "TABLE", "FALSE"]),
        ];
        let fks = vec![row(&["EMP", "FK_EMP_DEPT", "APP", "DEPT"])];
        let vts = vec![row(&["VT", "ERP", "<NULL>", "SAPR3", "KNA1"])];
        let g = assemble(nodes, &deps, &fks, &vts, "APP");
        let edge = |from: &str, to: &str, rel: Relation| {
            g.edges.iter().any(|e| {
                e.from.name == from
                    && e.to.name == to
                    && e.relation == rel
                    && e.confidence == Confidence::Confirmed
            })
        };
        assert!(edge("EMP", "V_EMP", Relation::Code));
        assert!(edge("DEPT", "V_EMP", Relation::Code));
        assert!(edge("V_EMP", "P_LOAD", Relation::Code));
        assert!(edge("SEQ", "P_LOAD", Relation::Code));
        assert!(edge("VT", "P_LOAD", Relation::Code));
        assert!(edge("DEPT", "EMP", Relation::ForeignKey));
        assert_eq!(g.edges.len(), 6, "{:?}", g.edges);
        let fk = g
            .edges
            .iter()
            .find(|e| e.relation == Relation::ForeignKey)
            .unwrap();
        assert_eq!(fk.detail.as_deref(), Some("FK_EMP_DEPT → DEPT"));
        let ext: Vec<_> = g
            .external
            .iter()
            .map(|x| {
                (
                    x.to.name.as_str(),
                    x.source.server.as_deref(),
                    x.source.database.as_deref(),
                    x.source.schema.as_deref(),
                    x.source.name.as_deref(),
                )
            })
            .collect();
        assert!(
            ext.contains(&("VT", Some("ERP"), None, Some("SAPR3"), Some("KNA1"))),
            "{ext:?}"
        );
        assert!(
            ext.contains(&("P_LOAD", None, None, Some("HR"), Some("PEOPLE"))),
            "{ext:?}"
        );
        assert_eq!(g.external.len(), 2, "{ext:?}");
        assert_eq!(g.objects.len(), 6);
    }
}

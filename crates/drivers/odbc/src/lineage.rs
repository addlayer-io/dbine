//! The lineage of a whole Db2 LUW database (`Session::lineage_graph`), from
//! the dependency catalog in two queries instead of one per object. The
//! other presets keep the contract's default.
//!
//! - One query unions what `references` reads per object, for every object:
//!   `SYSCAT.VIEWDEP` (views and materialized query tables),
//!   `SYSCAT.ROUTINEDEP` and, for SQL routines, their package's
//!   `SYSCAT.PACKAGEDEP`, `SYSCAT.TRIGDEP` (and its package's) with the
//!   trigger's table (`SYSCAT.TRIGGERS`), and an alias's base object
//!   (`SYSCAT.TABLES`). Those rows are confirmed.
//! - The foreign keys (`SYSCAT.REFERENCES`), confirmed.
//!
//! A nickname (federation) that an object reads goes in `external` with
//! its server, remote schema and table. What the catalog names outside the
//! listed objects (system schemas, other scopes) is left out. Jobs
//! (`SYSCAT` has none; the Administrative Task Scheduler's) come from
//! `Session::jobs` and `job_references`. A catalog the login can't read
//! (a database created `RESTRICTIVE`) gives the generic scan, with a note.

use crate::references::{cell, kind_of};
use crate::{OdbcSession, Rows};
use dbine_driver::dependencies::{
    graph_scan, ExternalRef, LineageEdge, LineageExternal, LineageGraph, LineageJob, LineageScope,
    GRAPH_OBJECTS, JOBS_READ,
};
use dbine_driver::{
    kinds, Confidence, DependencyScan, Dependent, Error, ObjectRef, Relation, Result, Session,
};

/// `(STYPE, SSCHEMA, SNAME, BTYPE, BSCHEMA, BNAME)` of every object's
/// catalog rows; `STYPE` is the object's `SYSCAT.TABLES.TYPE` or its
/// routine type (`P`, `F`) or `B` for a trigger.
const DEPS: &str = "SELECT t.TYPE AS STYPE, d.VIEWSCHEMA AS SSCHEMA, d.VIEWNAME AS SNAME, d.BTYPE, d.BSCHEMA, d.BNAME
  FROM SYSCAT.VIEWDEP d
  JOIN SYSCAT.TABLES t ON t.TABSCHEMA = d.VIEWSCHEMA AND t.TABNAME = d.VIEWNAME
UNION ALL
SELECT x.ROUTINETYPE, d.ROUTINESCHEMA, x.ROUTINENAME, d.BTYPE, d.BSCHEMA, d.BNAME
  FROM SYSCAT.ROUTINEDEP d
  JOIN SYSCAT.ROUTINES x ON x.ROUTINESCHEMA = d.ROUTINESCHEMA AND x.SPECIFICNAME = d.SPECIFICNAME
 WHERE x.ROUTINETYPE IN ('P', 'F')
UNION ALL
SELECT x.ROUTINETYPE, d.ROUTINESCHEMA, x.ROUTINENAME, p.BTYPE, p.BSCHEMA, p.BNAME
  FROM SYSCAT.ROUTINEDEP d
  JOIN SYSCAT.ROUTINES x ON x.ROUTINESCHEMA = d.ROUTINESCHEMA AND x.SPECIFICNAME = d.SPECIFICNAME
  JOIN SYSCAT.PACKAGEDEP p ON p.PKGSCHEMA = d.BSCHEMA AND p.PKGNAME = d.BNAME
 WHERE d.BTYPE = 'K' AND x.ROUTINETYPE IN ('P', 'F')
UNION ALL
SELECT 'B', TRIGSCHEMA, TRIGNAME, BTYPE, BSCHEMA, BNAME FROM SYSCAT.TRIGDEP
UNION ALL
SELECT 'B', d.TRIGSCHEMA, d.TRIGNAME, p.BTYPE, p.BSCHEMA, p.BNAME
  FROM SYSCAT.TRIGDEP d
  JOIN SYSCAT.PACKAGEDEP p ON p.PKGSCHEMA = d.BSCHEMA AND p.PKGNAME = d.BNAME
 WHERE d.BTYPE = 'K'
UNION ALL
SELECT 'B', g.TRIGSCHEMA, g.TRIGNAME, t.TYPE, t.TABSCHEMA, t.TABNAME
  FROM SYSCAT.TRIGGERS g
  JOIN SYSCAT.TABLES t ON t.TABSCHEMA = g.TABSCHEMA AND t.TABNAME = g.TABNAME
UNION ALL
SELECT 'A', a.TABSCHEMA, a.TABNAME, b.TYPE, a.BASE_TABSCHEMA, a.BASE_TABNAME
  FROM SYSCAT.TABLES a
  JOIN SYSCAT.TABLES b ON b.TABSCHEMA = a.BASE_TABSCHEMA AND b.TABNAME = a.BASE_TABNAME
 WHERE a.TYPE = 'A'";

/// [`DEPS`] with the routine behind a specific name and the remote side of
/// a nickname, as `references`' `USES`.
const GRAPH_USES: &str = "WITH dep (STYPE, SSCHEMA, SNAME, BTYPE, BSCHEMA, BNAME) AS ({deps})
SELECT DISTINCT dep.STYPE, TRIM(dep.SSCHEMA), dep.SNAME, dep.BTYPE, TRIM(dep.BSCHEMA), dep.BNAME,
       r.ROUTINETYPE, r.ROUTINENAME,
       n.SERVERNAME, TRIM(n.REMOTE_SCHEMA), n.REMOTE_TABLE, s.SERVERTYPE, o.SETTING
  FROM dep
  LEFT JOIN SYSCAT.ROUTINES r
    ON dep.BTYPE = 'F' AND r.ROUTINESCHEMA = dep.BSCHEMA AND r.SPECIFICNAME = dep.BNAME
  LEFT JOIN SYSCAT.NICKNAMES n
    ON dep.BTYPE = 'N' AND n.TABSCHEMA = dep.BSCHEMA AND n.TABNAME = dep.BNAME
  LEFT JOIN SYSCAT.SERVERS s ON s.SERVERNAME = n.SERVERNAME
  LEFT JOIN SYSCAT.SERVEROPTIONS o ON o.SERVERNAME = n.SERVERNAME AND o.OPTION = 'DBNAME'";

/// Every foreign key: constraint, table's schema and name, referenced
/// schema and table.
const FOREIGN_KEYS: &str =
    "SELECT DISTINCT TRIM(CONSTNAME), TRIM(TABSCHEMA), TABNAME, TRIM(REFTABSCHEMA), REFTABNAME
  FROM SYSCAT.REFERENCES";

fn graph_uses_sql() -> String {
    GRAPH_USES.replace("{deps}", DEPS)
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

fn same(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind
        && eq(&a.name, &b.name)
        && match (a.schema(), b.schema()) {
            (Some(x), Some(y)) => eq(x, y),
            (x, y) => x.is_none() && y.is_none(),
        }
}

fn find<'a>(nodes: &'a [ObjectRef], o: &ObjectRef) -> Option<&'a ObjectRef> {
    nodes.iter().find(|n| same(n, o))
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

/// The graph from the catalog's rows ([`graph_uses_sql`] and
/// [`FOREIGN_KEYS`]): `nodes` are the listed objects in scope.
pub(crate) fn assemble(nodes: Vec<ObjectRef>, uses: &Rows, fks: &Rows) -> LineageGraph {
    let mut g = LineageGraph::default();
    for r in uses {
        let (Some(stype), Some(sschema), Some(sname), Some(btype), Some(bschema), Some(bname)) = (
            cell(r, 0),
            cell(r, 1),
            cell(r, 2),
            cell(r, 3),
            cell(r, 4),
            cell(r, 5),
        ) else {
            continue;
        };
        let subject_kind = match stype.as_str() {
            "P" => Some(kinds::PROCEDURE),
            "F" => Some(kinds::FUNCTION),
            "B" => Some(kinds::TRIGGER),
            "A" => Some(kinds::SYNONYM),
            t => kind_of(t, None),
        };
        let Some(subject_kind) = subject_kind else {
            continue;
        };
        let Some(user) = find(
            &nodes,
            &ObjectRef {
                kind: subject_kind.into(),
                schema: Some(sschema),
                name: sname,
            },
        )
        .cloned() else {
            continue;
        };
        if btype == "N" {
            let source = ExternalRef {
                system: cell(r, 11),
                server: cell(r, 8),
                database: cell(r, 12),
                schema: cell(r, 9),
                name: cell(r, 10),
                detail: Some(format!("{bschema}.{bname}")),
                mentions: Vec::new(),
            };
            if !g
                .external
                .iter()
                .any(|x| x.source == source && same(&x.to, &user))
            {
                g.external.push(LineageExternal { source, to: user });
            }
            continue;
        }
        let Some(kind) = kind_of(&btype, cell(r, 6).as_deref()) else {
            continue;
        };
        let name = match kind {
            kinds::PROCEDURE | kinds::FUNCTION => cell(r, 7).unwrap_or(bname),
            _ => bname,
        };
        let used = ObjectRef {
            kind: kind.into(),
            schema: Some(bschema),
            name,
        };
        if let Some(used) = find(&nodes, &used).cloned() {
            push_edge(&mut g, &used, &user, Relation::Code, None);
        }
    }
    for r in fks {
        let (Some(constraint), Some(schema), Some(table), Some(ref_schema), Some(ref_table)) =
            (cell(r, 0), cell(r, 1), cell(r, 2), cell(r, 3), cell(r, 4))
        else {
            continue;
        };
        let obj = |schema: &str, name: &str| ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some(schema.into()),
            name: name.into(),
        };
        let (Some(child), Some(parent)) = (
            find(&nodes, &obj(&schema, &table)).cloned(),
            find(&nodes, &obj(&ref_schema, &ref_table)).cloned(),
        ) else {
            continue;
        };
        push_edge(
            &mut g,
            &parent,
            &child,
            Relation::ForeignKey,
            Some(format!("{constraint} → {ref_schema}.{ref_table}")),
        );
    }
    g.objects = nodes;
    g
}

fn node_kind(scan: &DependencyScan, kind: &str) -> bool {
    kind == kinds::TABLE
        || kind == kinds::COLLECTION
        || scan.source_kinds.iter().any(|k| k == kind)
        || scan.written_kinds.iter().any(|k| k == kind)
}

pub(crate) async fn db2(
    s: &mut OdbcSession,
    database: &str,
    scope: &LineageScope,
    scan: &DependencyScan,
) -> Result<LineageGraph> {
    let preset = s.preset;
    let limit = if scope.limit == 0 {
        GRAPH_OBJECTS
    } else {
        scope.limit
    } as usize;
    let mut nodes = Vec::new();
    let mut truncated = false;
    for o in s.list_objects().await? {
        let schema = o.schema.clone().unwrap_or_default();
        if !node_kind(scan, &o.kind)
            || preset.is_system_schema(&schema)
            || !(scope.kinds.is_empty() || scope.kinds.contains(&o.kind))
            || !(scope.schemas.is_empty() || scope.schemas.iter().any(|x| eq(x, &schema)))
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
        let uses = s.query(graph_uses_sql(), Vec::new()).await?;
        let fks = s.query(FOREIGN_KEYS.to_string(), Vec::new()).await?;
        Ok::<_, Error>((uses, fks))
    }
    .await;
    let (uses, fks) = match read {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("odbc: the Db2 dependency catalog not read: {e}");
            let mut g = graph_scan(s, database, scope, scan).await?;
            g.notes.push(format!(
                "No se pudo leer el catálogo de dependencias (SYSCAT): {e}"
            ));
            return Ok(g);
        }
    };
    let mut g = assemble(nodes, &uses, &fks);
    g.truncated = truncated;
    if scope.jobs {
        jobs(s, database, scan, &mut g).await;
    }
    Ok(g)
}

/// The scheduler's jobs, each with the objects its steps name.
async fn jobs(s: &mut OdbcSession, database: &str, scan: &DependencyScan, g: &mut LineageGraph) {
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

    fn o(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    fn row(cells: &[&str]) -> Vec<Option<String>> {
        let mut r: Vec<Option<String>> = cells
            .iter()
            .map(|c| (!c.is_empty()).then(|| c.to_string()))
            .collect();
        r.resize(13, None);
        r
    }

    #[test]
    fn the_query_unions_every_dependency_view_once_for_all_objects() {
        let sql = graph_uses_sql();
        for v in [
            "SYSCAT.VIEWDEP",
            "SYSCAT.ROUTINEDEP",
            "SYSCAT.PACKAGEDEP",
            "SYSCAT.TRIGDEP",
            "SYSCAT.TRIGGERS",
            "SYSCAT.NICKNAMES",
        ] {
            assert!(sql.contains(v), "{v}");
        }
        assert!(!sql.contains('?') && !sql.contains('{'), "{sql}");
        assert!(FOREIGN_KEYS.contains("SYSCAT.REFERENCES"));
    }

    #[test]
    fn rows_become_confirmed_edges_nicknames_external_and_foreign_keys() {
        let nodes = vec![
            o("table", "APP", "DEPT"),
            o("table", "APP", "EMP"),
            o("table", "APP", "NICK"),
            o("view", "APP", "V_EMP"),
            o("function", "APP", "F_COUNT"),
            o("procedure", "APP", "P_ADD"),
            o("trigger", "APP", "TRG_EMP"),
            o("synonym", "APP", "A_EMP"),
        ];
        let uses = vec![
            row(&["V", "APP", "V_EMP", "T", "APP", "EMP"]),
            row(&["V", "APP", "V_EMP", "T", "APP", "DEPT"]),
            row(&["V", "APP", "V_EMP", "T", "APP", "DEPT"]),
            // A view over a function by its specific name.
            row(&["V", "APP", "V_EMP", "F", "APP", "SQL1", "F", "F_COUNT"]),
            // A column and a system object are skipped.
            row(&["V", "APP", "V_EMP", "C", "APP", "EMP"]),
            row(&["V", "APP", "V_EMP", "T", "SYSIBM", "SYSDUMMY1"]),
            row(&["P", "APP", "P_ADD", "T", "APP", "EMP"]),
            row(&["P", "APP", "P_ADD", "F", "APP", "SQL2", "P", "P_ADD"]),
            row(&["B", "APP", "TRG_EMP", "T", "APP", "EMP"]),
            row(&["A", "APP", "A_EMP", "T", "APP", "EMP"]),
            row(&[
                "V", "APP", "V_EMP", "N", "APP", "NICK", "", "", "RSRV", "REM", "T_REMOTE",
                "DB2/UDB", "SALES",
            ]),
            row(&["V", "OTHER", "GONE", "T", "APP", "EMP"]),
        ];
        let fks = vec![
            row(&["FK_EMP_DEPT", "APP", "EMP", "APP", "DEPT"]),
            row(&["FK_EMP_DEPT", "APP", "EMP", "APP", "DEPT"]),
        ];
        let g = assemble(nodes, &uses, &fks);
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
        assert!(edge("F_COUNT", "V_EMP", Relation::Code));
        assert!(edge("EMP", "TRG_EMP", Relation::Code));
        assert!(edge("EMP", "A_EMP", Relation::Code));
        assert!(edge("DEPT", "EMP", Relation::ForeignKey));
        // P_ADD on itself (a routine's own package) is not an edge.
        assert!(edge("EMP", "P_ADD", Relation::Code));
        assert_eq!(g.edges.len(), 7, "{:?}", g.edges);
        let fk = g
            .edges
            .iter()
            .find(|e| e.relation == Relation::ForeignKey)
            .unwrap();
        assert_eq!(fk.detail.as_deref(), Some("FK_EMP_DEPT → APP.DEPT"));
        assert_eq!(g.external.len(), 1);
        let x = &g.external[0];
        assert_eq!(x.to.name, "V_EMP");
        assert_eq!(
            (
                x.source.server.as_deref(),
                x.source.schema.as_deref(),
                x.source.name.as_deref(),
                x.source.database.as_deref(),
                x.source.detail.as_deref()
            ),
            (
                Some("RSRV"),
                Some("REM"),
                Some("T_REMOTE"),
                Some("SALES"),
                Some("APP.NICK")
            )
        );
        assert_eq!(g.objects.len(), 8);
    }
}

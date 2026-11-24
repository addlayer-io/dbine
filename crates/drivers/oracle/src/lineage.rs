//! The lineage of the whole schema (`Session::lineage_graph`), from the
//! dictionary in three queries instead of one per object:
//!
//! - `ALL_DEPENDENCIES` for the schema's objects (what Oracle recorded when
//!   it compiled them: confirmed), less what Oracle maintains, an object on
//!   itself (a package body on its spec, a materialized view on its
//!   container table) and what no longer exists.
//! - The foreign keys (`ALL_CONSTRAINTS`, confirmed).
//! - `ALL_SYNONYMS`: the object a synonym stands for, or its database link.
//!
//! A database link is an external source (`ALL_DEPENDENCIES.REFERENCED_LINK_NAME`
//! for routines, `ALL_SYNONYMS.DB_LINK` for synonyms), with the server and
//! service from `DBA_DB_LINKS` / `ALL_DB_LINKS`. An object of another schema
//! is external too, with its owner. Oracle records no link for a view, so a
//! view that reads `name@link` shows no external source. Objects that aren't
//! VALID may be recorded only in part: the graph says how many. Jobs
//! (`DBMS_SCHEDULER`) come from `Session::jobs` and `job_references`.
//!
//! If the dictionary can't be read, the generic scan answers, with a note.

use crate::references::{
    dependencies_sql, read_links, ref_kind, schema_of, DepRow, LinkRow, CATALOG_NOTE,
};
use crate::{db_code, err, OracleSession};
use dbine_driver::dependencies::{
    graph_scan, Confidence, DependencyScan, Dependent, ExternalRef, LineageEdge, LineageExternal,
    LineageGraph, LineageJob, LineageScope, Relation, GRAPH_OBJECTS, JOBS_READ,
};
use dbine_driver::{kinds, ObjectRef, Result, Session};
use oracledb::Connection;

/// Every type `dependency_types` knows, for the schema's rows.
const TYPES: &[&str] = &[
    "TABLE",
    "VIEW",
    "MATERIALIZED VIEW",
    "PROCEDURE",
    "FUNCTION",
    "PACKAGE",
    "PACKAGE BODY",
    "TRIGGER",
    "TYPE",
    "TYPE BODY",
    "SYNONYM",
];

/// `ALL_DEPENDENCIES` for every object of `:1`: the columns of the
/// per-object query with the subject's name and type in front.
fn schema_dependencies_sql(legacy: bool) -> String {
    let one = dependencies_sql(TYPES, legacy);
    one.replacen(
        "SELECT d.referenced_owner,",
        "SELECT d.name, d.type, d.referenced_owner,",
        1,
    )
    .replacen(" AND d.name = :2", "", 1)
}

/// Foreign keys of the schema's tables: table, constraint, referenced owner
/// and table.
const FOREIGN_KEYS: &str = "SELECT DISTINCT c.table_name, c.constraint_name, r.owner, r.table_name
  FROM all_constraints c
  JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name
 WHERE c.owner = :1 AND c.constraint_type = 'R'
 ORDER BY c.table_name, c.constraint_name";

/// The schema's synonyms: name, target's owner and name, link.
const SYNONYMS: &str = "SELECT synonym_name, table_owner, table_name, db_link
  FROM all_synonyms WHERE owner = :1 ORDER BY synonym_name";

/// How many objects of the schema aren't VALID.
const INVALID: &str = "SELECT COUNT(*) FROM all_objects
 WHERE owner = :1 AND status <> 'VALID' AND object_type IN ('VIEW','MATERIALIZED VIEW','PROCEDURE','FUNCTION','PACKAGE','PACKAGE BODY','TRIGGER','TYPE','TYPE BODY','SYNONYM')";

/// A dependency row of the schema: the object and what it uses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SchemaDep {
    pub name: String,
    pub ty: String,
    pub used: DepRow,
}

/// A foreign key row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FkEdge {
    pub table: String,
    pub constraint: String,
    pub ref_owner: String,
    pub ref_table: String,
}

/// An `ALL_SYNONYMS` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Syn {
    pub name: String,
    pub owner: Option<String>,
    pub target: String,
    pub link: Option<String>,
}

/// What the dictionary says about the schema.
#[derive(Debug, Default)]
pub(crate) struct SchemaCatalog {
    pub deps: Vec<SchemaDep>,
    pub fks: Vec<FkEdge>,
    pub synonyms: Vec<Syn>,
    pub links: Vec<LinkRow>,
    pub invalid: u32,
}

fn text(r: &oracledb::Row, i: usize) -> Result<Option<String>> {
    r.get::<Option<String>>(i).map_err(err)
}

fn read(c: &Connection, owner: &str) -> Result<SchemaCatalog> {
    let params: [&dyn oracledb::ToDbValue; 1] = [&owner];
    let deps = match c
        .query(&schema_dependencies_sql(false), &params)
        .and_then(|r| r.collect::<std::result::Result<Vec<_>, _>>())
    {
        // ORA-00904: no ORACLE_MAINTAINED before 12c.
        Err(e) if db_code(&e) == Some(904) => c
            .query(&schema_dependencies_sql(true), &params)
            .and_then(|r| r.collect::<std::result::Result<Vec<_>, _>>()),
        r => r,
    }
    .map_err(err)?;
    let mut cat = SchemaCatalog::default();
    for r in &deps {
        cat.deps.push(SchemaDep {
            name: text(r, 0)?.unwrap_or_default(),
            ty: text(r, 1)?.unwrap_or_default(),
            used: DepRow {
                owner: text(r, 2)?.unwrap_or_default(),
                name: text(r, 3)?.unwrap_or_default(),
                ty: text(r, 4)?.unwrap_or_default(),
                link: text(r, 5)?,
                mview: r.get::<Option<i64>>(6).map_err(err)? == Some(1),
            },
        });
    }
    for r in c.query(FOREIGN_KEYS, &params).map_err(err)? {
        let r = r.map_err(err)?;
        cat.fks.push(FkEdge {
            table: text(&r, 0)?.unwrap_or_default(),
            constraint: text(&r, 1)?.unwrap_or_default(),
            ref_owner: text(&r, 2)?.unwrap_or_default(),
            ref_table: text(&r, 3)?.unwrap_or_default(),
        });
    }
    for r in c.query(SYNONYMS, &params).map_err(err)? {
        let r = r.map_err(err)?;
        cat.synonyms.push(Syn {
            name: text(&r, 0)?.unwrap_or_default(),
            owner: text(&r, 1)?,
            target: text(&r, 2)?.unwrap_or_default(),
            link: text(&r, 3)?,
        });
    }
    // Not being able to count them only leaves the note out.
    cat.invalid = c
        .query(INVALID, &params)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|r| r.ok())
        .and_then(|r| r.get::<Option<i64>>(0).ok().flatten())
        .unwrap_or(0) as u32;
    if cat.deps.iter().any(|d| d.used.link.is_some())
        || cat.synonyms.iter().any(|s| s.link.is_some())
    {
        cat.links = read_links(c);
    }
    Ok(cat)
}

fn same(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind
        && a.name.eq_ignore_ascii_case(&b.name)
        && a.schema.as_deref().map(str::to_lowercase) == b.schema.as_deref().map(str::to_lowercase)
}

fn find<'a>(
    nodes: &'a [ObjectRef],
    kind: &str,
    schema: &Option<String>,
    name: &str,
) -> Option<&'a ObjectRef> {
    nodes
        .iter()
        .find(|n| n.kind == kind && n.name == name && n.schema.as_deref() == schema.as_deref())
}

fn push_edge(
    g: &mut LineageGraph,
    from: &ObjectRef,
    to: &ObjectRef,
    relation: Relation,
    detail: Option<String>,
) {
    if same(from, to) {
        return;
    }
    if g.edges
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

/// The kinds a synonym's target may be, in the order a name is looked up.
const SYNONYM_TARGETS: &[&str] = &[
    kinds::TABLE,
    kinds::VIEW,
    kinds::MATERIALIZED_VIEW,
    kinds::PROCEDURE,
    kinds::FUNCTION,
    crate::PACKAGE,
    kinds::SEQUENCE,
    kinds::TYPE,
    kinds::SYNONYM,
];

/// The graph from the dictionary's rows. `nodes` are the schema's objects
/// (and the public synonyms of them), `session` the schema.
pub(crate) fn assemble(cat: &SchemaCatalog, nodes: Vec<ObjectRef>, session: &str) -> LineageGraph {
    let mut g = LineageGraph::default();
    let other = |owner: &str, name: &str, kind: &str| ExternalRef {
        system: Some("Oracle".into()),
        schema: Some(owner.to_string()),
        name: Some(name.to_string()),
        detail: Some(kind.to_string()),
        ..Default::default()
    };
    for d in &cat.deps {
        let Some(kind) = ref_kind(&d.ty, false) else {
            continue;
        };
        let Some(user) = find(&nodes, kind, &None, &d.name).cloned() else {
            continue;
        };
        match &d.used.link {
            Some(link) => {
                let x = crate::references::linked(
                    &cat.links,
                    link,
                    session,
                    Some(d.used.owner.clone()).filter(|o| !o.is_empty()),
                    &d.used.name,
                );
                push_external(&mut g, x, &user);
            }
            None => {
                let Some(used_kind) = ref_kind(&d.used.ty, d.used.mview) else {
                    continue;
                };
                let schema = schema_of(&d.used.owner, session);
                match find(&nodes, used_kind, &schema, &d.used.name) {
                    Some(used) => push_edge(&mut g, &used.clone(), &user, Relation::Code, None),
                    None if schema.is_some() => {
                        push_external(&mut g, other(&d.used.owner, &d.used.name, used_kind), &user)
                    }
                    // The schema's own and not listed (a table that is a
                    // materialized view's container…).
                    None => {}
                }
            }
        }
    }
    for f in &cat.fks {
        let Some(child) = find(&nodes, kinds::TABLE, &None, &f.table).cloned() else {
            continue;
        };
        let schema = schema_of(&f.ref_owner, session);
        let detail = Some(format!(
            "{} → {}",
            f.constraint,
            schema
                .as_deref()
                .map_or(f.ref_table.clone(), |s| format!("{s}.{}", f.ref_table))
        ));
        match find(&nodes, kinds::TABLE, &schema, &f.ref_table) {
            Some(parent) => push_edge(
                &mut g,
                &parent.clone(),
                &child,
                Relation::ForeignKey,
                detail,
            ),
            None if schema.is_some() => push_external(
                &mut g,
                other(&f.ref_owner, &f.ref_table, kinds::TABLE),
                &child,
            ),
            None => {}
        }
    }
    for s in &cat.synonyms {
        let Some(user) = find(&nodes, kinds::SYNONYM, &None, &s.name).cloned() else {
            continue;
        };
        if let Some(link) = &s.link {
            let x = crate::references::linked(
                &cat.links,
                link,
                session,
                s.owner.clone().filter(|o| !o.is_empty()),
                &s.target,
            );
            push_external(&mut g, x, &user);
            continue;
        }
        let schema = s.owner.as_deref().and_then(|o| schema_of(o, session));
        let target = SYNONYM_TARGETS
            .iter()
            .find_map(|k| find(&nodes, k, &schema, &s.target))
            .cloned();
        match target {
            Some(t) => push_edge(&mut g, &t, &user, Relation::Code, None),
            None if schema.is_some() => push_external(
                &mut g,
                other(
                    s.owner.as_deref().unwrap_or_default(),
                    &s.target,
                    kinds::SYNONYM,
                ),
                &user,
            ),
            None => {}
        }
    }
    g.objects = nodes;
    if cat.invalid > 0 {
        g.notes.push(format!(
            "{} objetos del esquema no están VALID: Oracle puede haber registrado sus dependencias solo en parte",
            cat.invalid
        ));
    }
    g
}

fn node_kind(scan: &DependencyScan, kind: &str) -> bool {
    kind == kinds::TABLE
        || kind == kinds::COLLECTION
        || scan.source_kinds.iter().any(|k| k == kind)
        || scan.written_kinds.iter().any(|k| k == kind)
}

impl OracleSession {
    pub(crate) async fn lineage_graph_impl(
        &mut self,
        database: &str,
        scope: &LineageScope,
        scan: &DependencyScan,
    ) -> Result<LineageGraph> {
        let session = self.schema.clone();
        let limit = if scope.limit == 0 {
            GRAPH_OBJECTS
        } else {
            scope.limit
        } as usize;
        let mut nodes = Vec::new();
        let mut truncated = false;
        for o in self.list_objects().await? {
            let schema = o.schema.clone().unwrap_or_else(|| session.clone());
            if !node_kind(scan, &o.kind)
                || !(scope.kinds.is_empty() || scope.kinds.contains(&o.kind))
                || !(scope.schemas.is_empty()
                    || scope
                        .schemas
                        .iter()
                        .any(|s| s.eq_ignore_ascii_case(&schema)))
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
        let read = {
            let owner = session.clone();
            self.run(move |c| read(c, &owner)).await
        };
        let cat = match read {
            Ok(cat) => cat,
            Err(e) => {
                tracing::debug!("oracle: lineage not read from the dictionary: {e}");
                let mut g = graph_scan(self, database, scope, scan).await?;
                g.notes.push(CATALOG_NOTE.into());
                return Ok(g);
            }
        };
        let mut g = assemble(&cat, nodes, &session);
        g.truncated = truncated;
        if scope.jobs {
            self.lineage_jobs(database, scan, &mut g).await;
        }
        Ok(g)
    }

    /// The scheduler's jobs, each with the objects its steps name.
    async fn lineage_jobs(&mut self, database: &str, scan: &DependencyScan, g: &mut LineageGraph) {
        let list = match self.jobs().await {
            Ok(l) => l,
            Err(dbine_driver::Error::Unsupported(_)) => return,
            Err(e) => {
                g.job_notes
                    .push(format!("no se pudieron leer los jobs del servidor: {e}"));
                return;
            }
        };
        g.job_notes.extend(list.notes);
        let total = list.jobs.len();
        let scan = &dbine_driver::dependencies::with_databases(self, scan).await;
        for job in list.jobs.into_iter().take(JOBS_READ) {
            let r = match self.job_references(database, &job.id, scan).await {
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
                    let o = ObjectRef {
                        kind: d.kind.clone(),
                        schema: d.schema.clone(),
                        name: d.name.clone(),
                    };
                    g.objects.iter().any(|n| same(n, &o))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: schema.map(str::to_string),
            name: name.into(),
        }
    }

    fn dep(
        name: &str,
        ty: &str,
        owner: &str,
        uname: &str,
        uty: &str,
        link: Option<&str>,
    ) -> SchemaDep {
        SchemaDep {
            name: name.into(),
            ty: ty.into(),
            used: DepRow {
                owner: owner.into(),
                name: uname.into(),
                ty: uty.into(),
                link: link.map(str::to_string),
                mview: false,
            },
        }
    }

    #[test]
    fn the_schema_query_is_the_per_object_one_for_every_object() {
        let sql = schema_dependencies_sql(false);
        assert!(
            sql.starts_with("SELECT d.name, d.type, d.referenced_owner,"),
            "{sql}"
        );
        assert!(
            sql.contains("WHERE d.owner = :1 AND d.type IN ('TABLE', 'VIEW'"),
            "{sql}"
        );
        assert!(!sql.contains(":2"), "{sql}");
        assert!(sql.contains("'PACKAGE BODY'") && sql.contains("'TYPE BODY'"));
        assert!(sql.contains("referenced_type <> 'NON-EXISTENT'"));
        assert!(schema_dependencies_sql(true).contains("NOT (d.referenced_owner IN ('SYS'"));
    }

    #[test]
    fn rows_become_confirmed_edges_externals_and_synonym_edges() {
        let nodes = vec![
            o("table", None, "DEPT"),
            o("table", None, "EMP"),
            o("view", None, "V_EMP"),
            o("package", None, "PKG"),
            o("procedure", None, "P_REMOTE"),
            o("synonym", None, "S_EMP"),
            o("synonym", None, "S_REMOTE"),
            o("synonym", None, "S_OTHER"),
        ];
        let cat = SchemaCatalog {
            deps: vec![
                dep("V_EMP", "VIEW", "APP", "EMP", "TABLE", None),
                dep("V_EMP", "VIEW", "APP", "DEPT", "TABLE", None),
                // The package body is the package.
                dep("PKG", "PACKAGE BODY", "APP", "V_EMP", "VIEW", None),
                dep("PKG", "PACKAGE BODY", "APP", "V_EMP", "VIEW", None),
                dep("PKG", "PACKAGE BODY", "HR", "PEOPLE", "TABLE", None),
                dep(
                    "P_REMOTE",
                    "PROCEDURE",
                    "SCOTT",
                    "EMP",
                    "TABLE",
                    Some("LNK"),
                ),
                dep("GONE", "VIEW", "APP", "EMP", "TABLE", None),
            ],
            fks: vec![FkEdge {
                table: "EMP".into(),
                constraint: "FK_EMP_DEPT".into(),
                ref_owner: "APP".into(),
                ref_table: "DEPT".into(),
            }],
            synonyms: vec![
                Syn {
                    name: "S_EMP".into(),
                    owner: Some("APP".into()),
                    target: "EMP".into(),
                    link: None,
                },
                Syn {
                    name: "S_REMOTE".into(),
                    owner: Some("SCOTT".into()),
                    target: "DEPT".into(),
                    link: Some("LNK".into()),
                },
                Syn {
                    name: "S_OTHER".into(),
                    owner: Some("HR".into()),
                    target: "PEOPLE".into(),
                    link: None,
                },
            ],
            links: vec![LinkRow {
                owner: "APP".into(),
                name: "LNK".into(),
                user: Some("SCOTT".into()),
                host: Some("db2:1521/ventas".into()),
            }],
            invalid: 2,
        };
        let g = assemble(&cat, nodes, "APP");
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
        assert!(edge("V_EMP", "PKG", Relation::Code));
        assert!(edge("DEPT", "EMP", Relation::ForeignKey));
        assert!(edge("EMP", "S_EMP", Relation::Code));
        // Repeated rows make one edge; the missing object is skipped.
        assert_eq!(g.edges.len(), 5, "{:?}", g.edges);
        let fk = g
            .edges
            .iter()
            .find(|e| e.relation == Relation::ForeignKey)
            .unwrap();
        assert_eq!(fk.detail.as_deref(), Some("FK_EMP_DEPT → DEPT"));
        // Another schema and a database link are external.
        let ext: Vec<_> = g
            .external
            .iter()
            .map(|x| {
                (
                    x.to.name.as_str(),
                    x.source.server.as_deref(),
                    x.source.schema.as_deref(),
                    x.source.name.as_deref(),
                )
            })
            .collect();
        assert!(
            ext.contains(&("PKG", None, Some("HR"), Some("PEOPLE"))),
            "{ext:?}"
        );
        assert!(
            ext.contains(&("P_REMOTE", Some("db2:1521"), Some("SCOTT"), Some("EMP"))),
            "{ext:?}"
        );
        assert!(
            ext.contains(&("S_REMOTE", Some("db2:1521"), Some("SCOTT"), Some("DEPT"))),
            "{ext:?}"
        );
        assert!(
            ext.contains(&("S_OTHER", None, Some("HR"), Some("PEOPLE"))),
            "{ext:?}"
        );
        assert_eq!(g.objects.len(), 8);
        assert!(g.notes[0].starts_with("2 objetos"));
    }
}

//! The lineage of a whole database (`Session::lineage_graph`).
//!
//! - Edges between objects: one read of `sys.sql_expression_dependencies`
//!   for the whole database (views, routines, triggers, synonyms, and the
//!   functions a table's computed columns, CHECK and DEFAULT constraints
//!   use), confirmed. A name with another database or a linked server goes
//!   to `external`. Triggers and synonyms add the table or object they sit
//!   on.
//! - Foreign keys: `sys.foreign_keys`, confirmed.
//! - Code the catalog doesn't record (deferred names, dynamic SQL): the
//!   bodies are read once (`sys.sql_modules`, at most `GRAPH_DEFINITIONS`),
//!   which also gives each edge its lines, and the names they use that the
//!   catalog didn't resolve are looked up in the object list, probable.
//! - Jobs: the Agent's, through `Session::job_references`, when asked.
//!
//! Where the dependency view can't be read (it takes VIEW DEFINITION and
//! SELECT on it, which only db_owner has by default; Babelfish and Fabric
//! may lack it) the generic scan runs and the note says why.

use crate::references::{
    classify, external_mentions, kind_of, line, note, same, use_row, without_variables, UseRow,
    Used,
};
use crate::variant::Variant;
use crate::{text, SqlServerSession};
use dbine_driver::dependencies::{
    find_mentions, graph_scan, used_names, used_objects, Confidence, DependencyScan,
    DependencyTarget, Dependent, ExternalRef, LineageEdge, LineageExternal, LineageGraph,
    LineageJob, LineageScope, Relation, GRAPH_DEFINITIONS, GRAPH_OBJECTS, JOBS_READ,
};
use dbine_driver::{kinds, DbObject, ObjectRef, Result, Session};

/// The 11 columns `use_row` reads, then the referencing object's
/// `sys.objects.type`, schema and name. Constraints are reported as their
/// table (`via` says which); a column of a table as the table.
const EDGES_SQL: &str = "SELECT CASE WHEN r.type = 'C' THEN N'check' WHEN r.type = 'D' THEN N'default'
            WHEN d.referencing_minor_id > 0 THEN N'column' ELSE N'' END,
       CASE WHEN r.type IN ('C', 'D') THEN r.name
            WHEN d.referencing_minor_id > 0 THEN COL_NAME(d.referencing_id, d.referencing_minor_id) END,
       d.referenced_server_name, d.referenced_database_name, d.referenced_schema_name, d.referenced_entity_name,
       RTRIM(o.type), SCHEMA_NAME(o.schema_id), o.name,
       DB_NAME(), COALESCE(NULLIF(sv.product, N''), sv.provider),
       RTRIM(CASE WHEN r.type IN ('C', 'D') THEN p.type ELSE r.type END),
       SCHEMA_NAME(CASE WHEN r.type IN ('C', 'D') THEN p.schema_id ELSE r.schema_id END),
       CASE WHEN r.type IN ('C', 'D') THEN p.name ELSE r.name END
  FROM sys.sql_expression_dependencies d
  JOIN sys.objects r ON r.object_id = d.referencing_id
  LEFT JOIN sys.objects p ON p.object_id = r.parent_object_id AND r.type IN ('C', 'D')
  LEFT JOIN sys.objects o ON o.object_id = d.referenced_id AND d.referenced_class = 1
       AND d.referenced_server_name IS NULL
       AND (d.referenced_database_name IS NULL OR d.referenced_database_name = DB_NAME())
  LEFT JOIN sys.servers sv ON sv.name COLLATE DATABASE_DEFAULT = d.referenced_server_name
 WHERE d.referencing_class = 1 AND d.referenced_class = 1 AND r.is_ms_shipped = 0
UNION
SELECT N'parent', NULL, NULL, NULL, SCHEMA_NAME(p.schema_id), p.name,
       RTRIM(p.type), SCHEMA_NAME(p.schema_id), p.name, DB_NAME(), NULL,
       N'TR', SCHEMA_NAME(t.schema_id), t.name
  FROM sys.triggers tr
  JOIN sys.objects t ON t.object_id = tr.object_id
  JOIN sys.objects p ON p.object_id = tr.parent_id
 WHERE tr.parent_class = 1 AND t.is_ms_shipped = 0
UNION
SELECT N'synonym', NULL, PARSENAME(sn.base_object_name, 4), PARSENAME(sn.base_object_name, 3),
       PARSENAME(sn.base_object_name, 2), PARSENAME(sn.base_object_name, 1),
       RTRIM(o.type), SCHEMA_NAME(o.schema_id), o.name, DB_NAME(), COALESCE(NULLIF(sv.product, N''), sv.provider),
       N'SN', SCHEMA_NAME(sn.schema_id), sn.name
  FROM sys.synonyms sn
  LEFT JOIN sys.objects o ON PARSENAME(sn.base_object_name, 4) IS NULL
        AND ISNULL(PARSENAME(sn.base_object_name, 3), DB_NAME()) = DB_NAME()
        AND o.object_id = OBJECT_ID(sn.base_object_name)
  LEFT JOIN sys.servers sv ON sv.name COLLATE DATABASE_DEFAULT = PARSENAME(sn.base_object_name, 4)
 WHERE sn.is_ms_shipped = 0";

/// `EDGES_SQL` for `v`: a Fabric warehouse has no linked servers, triggers
/// or synonyms, only the dependency view.
fn edges_sql(v: Variant) -> String {
    if v != Variant::Fabric {
        return EDGES_SQL.into();
    }
    EDGES_SQL
        .split("\nUNION\nSELECT N'parent'")
        .next()
        .unwrap_or_default()
        .replace("COALESCE(NULLIF(sv.product, N''), sv.provider)", "NULL")
        .replace(
            "\n  LEFT JOIN sys.servers sv ON sv.name COLLATE DATABASE_DEFAULT = d.referenced_server_name",
            "",
        )
}

/// Foreign keys, one row per constraint: its table, the table it points
/// at and its name.
const FOREIGN_KEYS_SQL: &str =
    "SELECT SCHEMA_NAME(p.schema_id), p.name, SCHEMA_NAME(r.schema_id), r.name, fk.name
  FROM sys.foreign_keys fk
  JOIN sys.objects p ON p.object_id = fk.parent_object_id
  JOIN sys.objects r ON r.object_id = fk.referenced_object_id
 WHERE p.is_ms_shipped = 0
 ORDER BY 1, 2, 3, 4, 5";

/// The bodies, `@P1`…`@Pn` the schemas to read (none: all).
fn bodies_sql(schemas: usize) -> String {
    let filter = if schemas == 0 {
        String::new()
    } else {
        let p: Vec<String> = (1..=schemas).map(|i| format!("@P{i}")).collect();
        format!(" AND s.name IN ({})", p.join(", "))
    };
    format!(
        "SELECT TOP ({}) RTRIM(o.type), s.name, o.name, m.definition
  FROM sys.sql_modules m
  JOIN sys.objects o ON o.object_id = m.object_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
 WHERE o.is_ms_shipped = 0 AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR'){filter}
 ORDER BY s.name, o.name",
        GRAPH_DEFINITIONS + 1
    )
}

/// One row of `EDGES_SQL`.
#[derive(Debug, Clone, Default)]
pub(crate) struct DepRow {
    pub(crate) used: UseRow,
    /// The object that uses it: `sys.objects.type`, schema, name.
    pub(crate) by: (String, Option<String>, String),
}

/// A body the graph reads.
#[derive(Debug, Clone)]
pub(crate) struct Body {
    pub(crate) object: ObjectRef,
    /// `None`: encrypted or not readable.
    pub(crate) text: Option<String>,
}

/// A foreign key: table, the table it points at, its name.
pub(crate) type ForeignKeyRow = (ObjectRef, ObjectRef, String);

pub(crate) async fn graph(
    s: &mut SqlServerSession,
    database: &str,
    scope: &LineageScope,
    scan: &DependencyScan,
) -> Result<LineageGraph> {
    let all = s.list_objects().await?;
    let rows = match read_edges(s).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("sqlserver: dependency catalog unreadable: {e}");
            let mut g = graph_scan(s, database, scope, scan).await?;
            g.notes.push(note(s.variant).into());
            return Ok(g);
        }
    };
    let fks = read_foreign_keys(s).await.unwrap_or_default();
    let (bodies, capped) = read_bodies(s, scope).await.unwrap_or_default();
    let mut g = assemble(&all, scope, scan, &rows, &fks, &bodies, capped);
    if scope.jobs {
        jobs(s, database, scan, &mut g).await;
    }
    Ok(g)
}

async fn read_edges(s: &mut SqlServerSession) -> Result<Vec<DepRow>> {
    let sql = edges_sql(s.variant);
    let mut out = Vec::new();
    for r in s.catalog_rows(&sql, &[]).await? {
        out.push(DepRow {
            used: use_row(&r),
            by: (
                text(&r, 11).unwrap_or_default(),
                text(&r, 12),
                text(&r, 13).unwrap_or_default(),
            ),
        });
    }
    Ok(out)
}

async fn read_foreign_keys(s: &mut SqlServerSession) -> Result<Vec<ForeignKeyRow>> {
    let table = |schema, name| ObjectRef {
        kind: kinds::TABLE.into(),
        schema,
        name,
    };
    Ok(s.catalog_rows(FOREIGN_KEYS_SQL, &[])
        .await?
        .iter()
        .map(|r| {
            (
                table(text(r, 0), text(r, 1).unwrap_or_default()),
                table(text(r, 2), text(r, 3).unwrap_or_default()),
                text(r, 4).unwrap_or_default(),
            )
        })
        .collect())
}

/// The bodies (up to `GRAPH_DEFINITIONS`) and whether there were more.
async fn read_bodies(s: &mut SqlServerSession, scope: &LineageScope) -> Result<(Vec<Body>, bool)> {
    let schemas: Vec<&str> = scope.schemas.iter().map(String::as_str).collect();
    let mut out: Vec<Body> = s
        .catalog_rows(&bodies_sql(schemas.len()), &schemas)
        .await?
        .iter()
        .filter_map(|r| {
            let kind = kind_of(&text(r, 0)?)?;
            Some(Body {
                object: ObjectRef {
                    kind: kind.into(),
                    schema: text(r, 1),
                    name: text(r, 2)?,
                },
                text: text(r, 3),
            })
        })
        .collect();
    let capped = out.len() > GRAPH_DEFINITIONS;
    out.truncate(GRAPH_DEFINITIONS);
    Ok((out, capped))
}

fn is_node_kind(scan: &DependencyScan, kind: &str) -> bool {
    kind == kinds::TABLE
        || scan.source_kinds.iter().any(|k| k == kind)
        || scan.written_kinds.iter().any(|k| k == kind)
}

fn same_ref(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind
        && same(&a.name, &b.name)
        && match (a.schema(), b.schema()) {
            (Some(x), Some(y)) => same(x, y),
            (None, None) => true,
            _ => false,
        }
}

fn as_ref(d: &Dependent) -> ObjectRef {
    ObjectRef {
        kind: d.kind.clone(),
        schema: d.schema.clone(),
        name: d.name.clone(),
    }
}

fn find<'a>(nodes: &'a [ObjectRef], o: &ObjectRef) -> Option<&'a ObjectRef> {
    nodes.iter().find(|n| same_ref(n, o))
}

/// Adds `e`, or joins its detail into the edge of the same pair and kind.
fn push_edge(edges: &mut Vec<LineageEdge>, e: LineageEdge) {
    let Some(x) = edges
        .iter_mut()
        .find(|x| same_ref(&x.from, &e.from) && same_ref(&x.to, &e.to) && x.relation == e.relation)
    else {
        edges.push(e);
        return;
    };
    // A confirmed edge stands over a probable one.
    if e.confidence < x.confidence {
        x.confidence = e.confidence;
    }
    if let Some(sub) = e.detail {
        x.detail = Some(match x.detail.take() {
            Some(prev) if prev.split(", ").any(|p| p == sub) => prev,
            Some(prev) => format!("{prev}, {sub}"),
            None => sub,
        });
    }
}

/// The graph from what the catalog said. Pure, so it can be tested.
pub(crate) fn assemble(
    all: &[DbObject],
    scope: &LineageScope,
    scan: &DependencyScan,
    rows: &[DepRow],
    fks: &[ForeignKeyRow],
    bodies: &[Body],
    capped: bool,
) -> LineageGraph {
    let mut g = LineageGraph::default();
    let limit = if scope.limit == 0 {
        GRAPH_OBJECTS
    } else {
        scope.limit
    } as usize;
    let mut nodes: Vec<ObjectRef> = Vec::new();
    for o in all.iter().filter(|o| {
        is_node_kind(scan, &o.kind)
            && (scope.kinds.is_empty() || scope.kinds.contains(&o.kind))
            && (scope.schemas.is_empty()
                || o.schema
                    .as_deref()
                    .is_some_and(|s| scope.schemas.iter().any(|x| same(x, s))))
    }) {
        if nodes.len() >= limit {
            g.truncated = true;
            break;
        }
        nodes.push(ObjectRef {
            kind: o.kind.clone(),
            schema: o.schema.clone(),
            name: o.name.clone(),
        });
    }

    let engine = "SQL Server";
    // (outside source, the object that reads it)
    let mut externals: Vec<(ExternalRef, ObjectRef)> = Vec::new();
    for r in rows {
        let Some(kind) = kind_of(&r.by.0) else {
            continue;
        };
        let to = ObjectRef {
            kind: kind.into(),
            schema: r.by.1.clone(),
            name: r.by.2.clone(),
        };
        let Some(to) = find(&nodes, &to).cloned() else {
            continue;
        };
        match classify(&r.used, engine) {
            Used::Item(d) => {
                let from = as_ref(&d);
                let Some(from) = find(&nodes, &from).cloned() else {
                    continue;
                };
                if same_ref(&from, &to) {
                    continue;
                }
                push_edge(
                    &mut g.edges,
                    LineageEdge {
                        from,
                        to,
                        relation: d.relation,
                        confidence: Confidence::Confirmed,
                        detail: d.detail,
                        mentions: Vec::new(),
                    },
                );
            }
            Used::External(x) => {
                if !externals.iter().any(|(e, t)| {
                    same_ref(t, &to)
                        && e.server == x.server
                        && e.database == x.database
                        && e.schema == x.schema
                        && e.name == x.name
                }) {
                    externals.push((x, to));
                }
            }
            Used::Unresolved => {}
        }
    }

    // Foreign keys: from the table pointed at to the one that holds it.
    for (table, parent, name) in fks {
        let (Some(to), Some(from)) = (find(&nodes, table), find(&nodes, parent)) else {
            continue;
        };
        if same_ref(from, to) {
            continue;
        }
        push_edge(
            &mut g.edges,
            LineageEdge {
                from: from.clone(),
                to: to.clone(),
                relation: Relation::ForeignKey,
                confidence: Confidence::Confirmed,
                detail: Some(name.clone()),
                mentions: Vec::new(),
            },
        );
    }

    // The bodies: the lines of each edge and the names the catalog missed.
    for b in bodies {
        let Some(me) = find(&nodes, &b.object).cloned() else {
            continue;
        };
        let Some(body) = &b.text else {
            g.unreadable.push(match me.schema() {
                Some(sc) => format!("{sc}.{}", me.name),
                None => me.name.clone(),
            });
            continue;
        };
        g.scanned += 1;
        let code = without_variables(body);
        for e in g.edges.iter_mut().filter(|e| same_ref(&e.to, &me)) {
            let named = DependencyTarget {
                object: e.from.clone(),
                column: None,
            };
            if let Some((_, mut m)) = find_mentions(&code, &scan.dialect, &named) {
                for m in &mut m {
                    m.text = line(body, m.line);
                }
                e.mentions = m;
            }
        }
        let used = used_names(body, &scan.dialect);
        for x in externals.iter_mut().filter(|(_, t)| same_ref(t, &me)) {
            x.0.mentions = external_mentions(body, &used, &x.0, &me);
        }
        let rest: Vec<_> = used
            .into_iter()
            .filter(|u| {
                !u.name.starts_with(['#', '@'])
                    && u.quals.len() < 2
                    && !g
                        .edges
                        .iter()
                        .any(|e| same_ref(&e.to, &me) && same(&e.from.name, &u.name))
            })
            .collect();
        if rest.is_empty() {
            continue;
        }
        for d in used_objects(body, &rest, all, &me) {
            let from = as_ref(&d);
            let Some(from) = find(&nodes, &from).cloned() else {
                continue;
            };
            if same_ref(&from, &me) {
                continue;
            }
            push_edge(
                &mut g.edges,
                LineageEdge {
                    from,
                    to: me.clone(),
                    relation: Relation::Code,
                    confidence: d.confidence,
                    detail: None,
                    mentions: d.mentions,
                },
            );
        }
    }
    if capped {
        g.notes.push(format!(
            "se leyeron las primeras {GRAPH_DEFINITIONS} definiciones; del resto solo se muestran las dependencias que registra el catálogo"
        ));
    }
    g.external = externals
        .into_iter()
        .map(|(source, to)| LineageExternal { source, to })
        .collect();
    g.objects = nodes;
    g
}

/// The Agent's jobs and what their steps use (the generic version's, which
/// is private to the contract).
async fn jobs(
    s: &mut SqlServerSession,
    database: &str,
    scan: &DependencyScan,
    g: &mut LineageGraph,
) {
    let list = match s.jobs().await {
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
            .filter(|d| find(&g.objects, &as_ref(d)).is_some())
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
    use dbine_driver::ScriptDialect;

    fn obj(kind: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: Some("dbo".into()),
            name: name.into(),
            parent: None,
        }
    }

    fn r(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: Some("dbo".into()),
            name: name.into(),
        }
    }

    fn scan() -> DependencyScan {
        DependencyScan {
            source_kinds: vec!["view".into(), "procedure".into()],
            dialect: ScriptDialect::tsql(),
            foreign_keys: true,
            written_kinds: vec!["view".into(), "procedure".into(), "trigger".into()],
            has_schemas: true,
        }
    }

    /// `by` reads `used` (a table or view of dbo).
    fn dep(by: (&str, &str), used: (&str, &str)) -> DepRow {
        DepRow {
            used: UseRow {
                schema: Some("dbo".into()),
                entity: Some(used.1.into()),
                kind: Some(used.0.into()),
                resolved_schema: Some("dbo".into()),
                resolved_name: Some(used.1.into()),
                current: Some("ventas".into()),
                ..Default::default()
            },
            by: (by.0.into(), Some("dbo".into()), by.1.into()),
        }
    }

    fn pairs(g: &LineageGraph) -> Vec<String> {
        let mut v: Vec<String> = g
            .edges
            .iter()
            .map(|e| {
                format!(
                    "{}>{}:{:?}:{:?}",
                    e.from.name, e.to.name, e.relation, e.confidence
                )
            })
            .collect();
        v.sort();
        v
    }

    fn objects() -> Vec<DbObject> {
        vec![
            obj("table", "clientes"),
            obj("table", "pedidos"),
            obj("view", "v_ventas"),
            obj("procedure", "sp_reporte"),
            obj("procedure", "sp_dinamico"),
            obj("sequence", "seq_ids"),
        ]
    }

    #[test]
    fn the_catalog_rows_become_edges() {
        let rows = vec![
            dep(("V", "v_ventas"), ("U", "pedidos")),
            dep(("V", "v_ventas"), ("U", "clientes")),
            dep(("P", "sp_reporte"), ("V", "v_ventas")),
            // The same pair twice (two columns): one edge.
            dep(("P", "sp_reporte"), ("V", "v_ventas")),
            // A user of something that isn't a node (a sequence).
            dep(("P", "sp_reporte"), ("SO", "seq_ids")),
        ];
        let fks = vec![(
            r("table", "pedidos"),
            r("table", "clientes"),
            "FK_ped_cli".to_string(),
        )];
        let g = assemble(
            &objects(),
            &LineageScope::default(),
            &scan(),
            &rows,
            &fks,
            &[],
            false,
        );
        // Sequences aren't nodes; the rest are, also the one with no edge.
        assert_eq!(g.objects.len(), 5);
        assert_eq!(
            pairs(&g),
            [
                "clientes>pedidos:ForeignKey:Confirmed",
                "clientes>v_ventas:Code:Confirmed",
                "pedidos>v_ventas:Code:Confirmed",
                "v_ventas>sp_reporte:Code:Confirmed"
            ]
        );
        let fk = g.edges.iter().find(|e| e.to.name == "pedidos").unwrap();
        assert_eq!(fk.detail.as_deref(), Some("FK_ped_cli"));
    }

    #[test]
    fn other_databases_and_linked_servers_are_external() {
        let mut other_db = dep(("P", "sp_reporte"), ("U", "x"));
        other_db.used.database = Some("archivo".into());
        other_db.used.kind = None;
        other_db.used.resolved_name = None;
        let mut linked = dep(("V", "v_ventas"), ("U", "x"));
        linked.used.server = Some("REMOTO".into());
        linked.used.product = Some("Oracle".into());
        linked.used.kind = None;
        linked.used.resolved_name = None;
        let g = assemble(
            &objects(),
            &LineageScope::default(),
            &scan(),
            &[other_db, linked],
            &[],
            &[],
            false,
        );
        assert!(g.edges.is_empty());
        assert_eq!(g.external.len(), 2);
        let db = g
            .external
            .iter()
            .find(|e| e.to.name == "sp_reporte")
            .unwrap();
        assert_eq!(db.source.database.as_deref(), Some("archivo"));
        assert_eq!(db.source.system.as_deref(), Some("SQL Server"));
        let ls = g.external.iter().find(|e| e.to.name == "v_ventas").unwrap();
        assert_eq!(ls.source.server.as_deref(), Some("REMOTO"));
        assert_eq!(ls.source.system.as_deref(), Some("Oracle"));
    }

    #[test]
    fn bodies_give_lines_and_the_names_the_catalog_missed() {
        let rows = vec![dep(("V", "v_ventas"), ("U", "pedidos"))];
        let bodies = vec![
            Body {
                object: r("view", "v_ventas"),
                text: Some("CREATE VIEW dbo.v_ventas AS\nSELECT * FROM dbo.pedidos".into()),
            },
            // Deferred name resolution: the catalog has no row for it.
            Body {
                object: r("procedure", "sp_dinamico"),
                text: Some(
                    "CREATE PROCEDURE dbo.sp_dinamico AS\nSELECT * FROM dbo.v_ventas".into(),
                ),
            },
            Body {
                object: r("procedure", "sp_reporte"),
                text: None,
            },
        ];
        let g = assemble(
            &objects(),
            &LineageScope::default(),
            &scan(),
            &rows,
            &[],
            &bodies,
            true,
        );
        assert_eq!(
            pairs(&g),
            [
                "pedidos>v_ventas:Code:Confirmed",
                "v_ventas>sp_dinamico:Code:Probable"
            ]
        );
        let e = g.edges.iter().find(|e| e.to.name == "v_ventas").unwrap();
        assert_eq!(e.mentions[0].line, 2);
        assert_eq!(g.scanned, 2);
        assert_eq!(g.unreadable, ["dbo.sp_reporte"]);
        assert_eq!(g.notes.len(), 1);
    }

    #[test]
    fn the_scope_and_the_limit_apply() {
        let rows = vec![dep(("V", "v_ventas"), ("U", "pedidos"))];
        let scope = LineageScope {
            kinds: vec!["table".into()],
            ..Default::default()
        };
        let g = assemble(&objects(), &scope, &scan(), &rows, &[], &[], false);
        assert_eq!(g.objects.len(), 2);
        assert!(g.edges.is_empty());
        let scope = LineageScope {
            limit: 3,
            ..Default::default()
        };
        let g = assemble(&objects(), &scope, &scan(), &rows, &[], &[], false);
        assert!(g.truncated);
        assert_eq!(g.objects.len(), 3);
    }

    #[test]
    fn fabric_reads_only_the_dependency_view() {
        let f = edges_sql(Variant::Fabric);
        assert!(f.contains("sys.sql_expression_dependencies") && !f.contains("sys.servers"));
        assert!(!f.contains("sys.synonyms") && !f.contains("sys.triggers"));
        let s = edges_sql(Variant::SqlServer);
        assert!(s.contains("sys.servers") && s.contains("sys.synonyms"));
    }
}

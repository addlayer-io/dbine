//! The lineage of a whole database ([`crate::Session::lineage_graph`]): every
//! object as a node and every dependency as an edge, for the "Linaje" view
//! of a database, a schema, a folder or a Power BI model, as opposed to
//! [`crate::Session::references`] / [`crate::Session::dependents`], which
//! answer for one object.
//!
//! Edges go from what is used to what uses it (the direction the object
//! lineage draws left to right). The generic scan ([`graph_scan`]) lists the
//! objects once, takes the foreign keys from [`crate::Session::database_schema`]
//! (confirmed), reads the definition of each object whose source someone
//! wrote and resolves the names it uses against the list it already has
//! (probable), and, when asked, adds the server's jobs and what their steps
//! use. A driver whose engine records dependencies overrides
//! `Session::lineage_graph` with one catalog query.

use super::{
    foreign_key_parents, same_object, used_names, used_objects, Confidence, DependencyScan,
    Dependent, ExternalRef, Mention, Relation, JOBS_READ,
};
use crate::model::{DbObject, ObjectRef};
use crate::{kinds, Result, Session};
use serde::{Deserialize, Serialize};

/// Objects a graph keeps when the scope sets no limit.
pub const GRAPH_OBJECTS: u32 = 5000;
/// Definitions the generic scan reads; past it the graph says so.
pub const GRAPH_DEFINITIONS: usize = 600;

/// What part of the database to draw.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LineageScope {
    /// Schemas to include; empty: all of them.
    #[serde(default)]
    pub schemas: Vec<String>,
    /// Kinds to include; empty: all that can be part of a lineage.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Objects kept at most; `0`: [`GRAPH_OBJECTS`].
    #[serde(default)]
    pub limit: u32,
    /// Add the server's jobs, with what their steps use.
    #[serde(default)]
    pub jobs: bool,
}

/// A dependency between two objects of the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageEdge {
    /// What is used.
    pub from: ObjectRef,
    /// What uses it.
    pub to: ObjectRef,
    pub relation: Relation,
    pub confidence: Confidence,
    /// The constraint, as the object lineage shows it.
    #[serde(default)]
    pub detail: Option<String>,
    /// Where `to`'s source names `from`.
    #[serde(default)]
    pub mentions: Vec<Mention>,
}

/// A source outside the database, read by an object of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageExternal {
    pub source: ExternalRef,
    /// The object that reads it.
    pub to: ObjectRef,
}

/// A server job and what its steps use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageJob {
    /// The id the jobs calls take ([`crate::jobs::ServerJob::id`]).
    pub id: String,
    pub name: String,
    /// The engine's word for it ("SQL Server Agent", "pg_cron"…).
    pub kind: String,
    #[serde(default)]
    pub group: Option<String>,
    /// The objects of the database its steps name, with the lines.
    #[serde(default)]
    pub uses: Vec<Dependent>,
    /// What they read from outside the database.
    #[serde(default)]
    pub external: Vec<ExternalRef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LineageGraph {
    /// The nodes, also those no edge touches (what is left alone shows too).
    pub objects: Vec<ObjectRef>,
    pub edges: Vec<LineageEdge>,
    #[serde(default)]
    pub external: Vec<LineageExternal>,
    #[serde(default)]
    pub jobs: Vec<LineageJob>,
    /// Definitions read.
    #[serde(default)]
    pub scanned: u32,
    /// Objects whose definition couldn't be read (`schema.name`).
    #[serde(default)]
    pub unreadable: Vec<String>,
    /// The scope had more objects than `limit` and the rest are left out.
    #[serde(default)]
    pub truncated: bool,
    /// What the UI tells the user (each a whole message it translates).
    #[serde(default)]
    pub notes: Vec<String>,
    /// The same for the jobs read.
    #[serde(default)]
    pub job_notes: Vec<String>,
}

/// Kinds that can be a node: what is read from or written to, and what has
/// code that names other objects.
fn node_kind(ctx: &DependencyScan, kind: &str) -> bool {
    kind == kinds::TABLE
        || kind == kinds::COLLECTION
        || ctx.source_kinds.iter().any(|k| k == kind)
        || ctx.written_kinds.iter().any(|k| k == kind)
}

fn in_scope(scope: &LineageScope, o: &DbObject) -> bool {
    (scope.kinds.is_empty() || scope.kinds.contains(&o.kind))
        && (scope.schemas.is_empty()
            || o.schema
                .as_deref()
                .is_some_and(|s| scope.schemas.iter().any(|x| super::eq(x, s))))
}

fn object_ref(o: &DbObject) -> ObjectRef {
    ObjectRef {
        kind: o.kind.clone(),
        schema: o.schema.clone(),
        name: o.name.clone(),
    }
}

fn has(nodes: &[ObjectRef], o: &ObjectRef) -> bool {
    nodes.iter().any(|n| same_object(n, o))
}

fn push_edge(edges: &mut Vec<LineageEdge>, e: LineageEdge) {
    if !edges.iter().any(|x| {
        same_object(&x.from, &e.from) && same_object(&x.to, &e.to) && x.relation == e.relation
    }) {
        edges.push(e);
    }
}

/// The generic [`crate::Session::lineage_graph`].
pub async fn graph_scan<S: Session + ?Sized>(
    s: &mut S,
    database: &str,
    scope: &LineageScope,
    ctx: &DependencyScan,
) -> Result<LineageGraph> {
    let mut g = LineageGraph::default();
    let all = s.list_objects().await?;
    let limit = if scope.limit == 0 {
        GRAPH_OBJECTS
    } else {
        scope.limit
    } as usize;
    let mut nodes: Vec<ObjectRef> = Vec::new();
    for o in all
        .iter()
        .filter(|o| node_kind(ctx, &o.kind) && in_scope(scope, o))
    {
        if nodes.len() >= limit {
            g.truncated = true;
            break;
        }
        nodes.push(object_ref(o));
    }

    // Foreign keys: the tables a table points at (confirmed).
    if ctx.foreign_keys {
        let tables = s.database_schema().await?;
        for t in &tables {
            let me = ObjectRef {
                kind: kinds::TABLE.to_string(),
                schema: t.schema.clone(),
                name: t.name.clone(),
            };
            if !has(&nodes, &me) {
                continue;
            }
            for d in foreign_key_parents(&t.foreign_keys, &me) {
                let parent = ObjectRef {
                    kind: d.kind.clone(),
                    schema: d.schema.clone(),
                    name: d.name.clone(),
                };
                if has(&nodes, &parent) && !same_object(&parent, &me) {
                    push_edge(
                        &mut g.edges,
                        LineageEdge {
                            from: parent,
                            to: me.clone(),
                            relation: Relation::ForeignKey,
                            confidence: Confidence::Confirmed,
                            detail: d.detail,
                            mentions: Vec::new(),
                        },
                    );
                }
            }
        }
    }

    // Code: the names each definition uses, resolved against the list.
    let mut read = 0usize;
    for n in nodes.clone() {
        if !ctx.written_kinds.contains(&n.kind) {
            continue;
        }
        if read >= GRAPH_DEFINITIONS {
            g.notes.push(format!(
                "se leyeron las primeras {GRAPH_DEFINITIONS} definiciones; el resto no aporta líneas al diagrama"
            ));
            break;
        }
        read += 1;
        match s.definition(&n).await {
            Ok(Some(body)) => {
                g.scanned += 1;
                let used = used_names(&body, &ctx.dialect);
                for d in used_objects(&body, &used, &all, &n) {
                    let from = ObjectRef {
                        kind: d.kind.clone(),
                        schema: d.schema.clone(),
                        name: d.name.clone(),
                    };
                    if has(&nodes, &from) && !same_object(&from, &n) {
                        push_edge(
                            &mut g.edges,
                            LineageEdge {
                                from,
                                to: n.clone(),
                                relation: Relation::Code,
                                confidence: d.confidence,
                                detail: None,
                                mentions: d.mentions,
                            },
                        );
                    }
                }
            }
            Ok(None) => {}
            Err(_) => g.unreadable.push(super::display_name(n.schema(), &n.name)),
        }
    }

    if scope.jobs {
        graph_jobs(s, database, ctx, &nodes, &mut g).await;
    }
    g.objects = nodes;
    Ok(g)
}

/// The server's jobs, each with the objects its steps name. A list that can't
/// be read leaves the graph without jobs and says why.
async fn graph_jobs<S: Session + ?Sized>(
    s: &mut S,
    database: &str,
    ctx: &DependencyScan,
    nodes: &[ObjectRef],
    g: &mut LineageGraph,
) {
    let list = match s.jobs().await {
        Ok(l) => l,
        Err(crate::Error::Unsupported(_)) => return,
        Err(e) => {
            g.job_notes
                .push(format!("no se pudieron leer los jobs del servidor: {e}"));
            return;
        }
    };
    g.job_notes.extend(list.notes);
    let total = list.jobs.len();
    for job in list.jobs.into_iter().take(JOBS_READ) {
        let r = match s.job_references(database, &job.id, ctx).await {
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
                has(
                    nodes,
                    &ObjectRef {
                        kind: d.kind.clone(),
                        schema: d.schema.clone(),
                        name: d.name.clone(),
                    },
                )
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
    use crate::schema::{ForeignKeyDef, TableSchema};
    use crate::sql::ScriptDialect;

    struct Fake;

    #[async_trait::async_trait]
    impl Session for Fake {
        async fn server_version(&mut self) -> Result<String> {
            Ok(String::new())
        }
        async fn list_databases(&mut self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
            let o = |kind: &str, name: &str| DbObject {
                kind: kind.into(),
                schema: Some("dbo".into()),
                name: name.into(),
                parent: None,
            };
            Ok(vec![
                o("table", "clientes"),
                o("table", "pedidos"),
                o("view", "v_ventas"),
                o("procedure", "sp_reporte"),
                o("sequence", "seq_ids"),
            ])
        }
        async fn columns(&mut self, _: &ObjectRef) -> Result<Vec<crate::ColumnInfo>> {
            Ok(vec![])
        }
        async fn definition(&mut self, o: &ObjectRef) -> Result<Option<String>> {
            Ok(match o.name.as_str() {
                "v_ventas" => Some("CREATE VIEW dbo.v_ventas AS\nSELECT * FROM dbo.pedidos p\nJOIN dbo.clientes c ON c.id = p.cliente_id".into()),
                "sp_reporte" => Some("CREATE PROCEDURE dbo.sp_reporte AS\nSELECT * FROM dbo.v_ventas".into()),
                _ => None,
            })
        }
        async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
            let pedidos = TableSchema {
                schema: Some("dbo".into()),
                name: "pedidos".into(),
                foreign_keys: vec![ForeignKeyDef {
                    name: Some("FK_ped_cli".into()),
                    columns: vec!["cliente_id".into()],
                    ref_schema: Some("dbo".into()),
                    ref_table: "clientes".into(),
                    ref_columns: vec!["id".into()],
                    ..Default::default()
                }],
                ..Default::default()
            };
            Ok(vec![pedidos])
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            String::new()
        }
        async fn execute(&mut self, _: &str, _: usize, _: &mut crate::QueryOutcome) -> Result<()> {
            Ok(())
        }
    }

    fn ctx() -> DependencyScan {
        DependencyScan {
            source_kinds: vec!["procedure".into(), "view".into()],
            dialect: ScriptDialect::tsql(),
            foreign_keys: true,
            written_kinds: vec!["procedure".into(), "view".into()],
            has_schemas: true,
        }
    }

    fn ready<T>(f: impl std::future::Future<Output = T>) -> T {
        let mut f = std::pin::pin!(f);
        match f
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("waited"),
        }
    }

    fn pairs(g: &LineageGraph) -> Vec<String> {
        let mut v: Vec<String> = g
            .edges
            .iter()
            .map(|e| format!("{}>{}:{:?}", e.from.name, e.to.name, e.confidence))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn the_whole_database_in_one_graph() {
        let g = ready(graph_scan(
            &mut Fake,
            "ventas",
            &LineageScope::default(),
            &ctx(),
        ))
        .unwrap();
        // A sequence is not a node; every table, view and procedure is.
        assert_eq!(g.objects.len(), 4);
        assert_eq!(
            pairs(&g),
            [
                "clientes>pedidos:Confirmed",
                "clientes>v_ventas:Probable",
                "pedidos>v_ventas:Probable",
                "v_ventas>sp_reporte:Probable"
            ]
        );
        assert_eq!(g.scanned, 2);
        assert!(!g.truncated);
        // The line that names the table comes with the edge.
        let e = g
            .edges
            .iter()
            .find(|e| e.to.name == "v_ventas" && e.from.name == "pedidos")
            .unwrap();
        assert_eq!(e.mentions[0].line, 2);
    }

    #[test]
    fn a_scope_keeps_only_its_objects_and_the_limit_truncates() {
        let scope = LineageScope {
            kinds: vec!["table".into()],
            ..Default::default()
        };
        let g = ready(graph_scan(&mut Fake, "ventas", &scope, &ctx())).unwrap();
        assert_eq!(g.objects.len(), 2);
        assert_eq!(pairs(&g), ["clientes>pedidos:Confirmed"]);
        let scope = LineageScope {
            limit: 3,
            ..Default::default()
        };
        let g = ready(graph_scan(&mut Fake, "ventas", &scope, &ctx())).unwrap();
        assert!(g.truncated);
        assert_eq!(g.objects.len(), 3);
    }

    #[test]
    fn an_engine_without_jobs_returns_no_jobs() {
        let scope = LineageScope {
            jobs: true,
            ..Default::default()
        };
        let g = ready(graph_scan(&mut Fake, "ventas", &scope, &ctx())).unwrap();
        assert!(g.jobs.is_empty() && g.job_notes.is_empty());
    }
}

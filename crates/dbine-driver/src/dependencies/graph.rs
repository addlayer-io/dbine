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
    Dependent, ExternalRef, Mention, Relation, JOBS_READ, MAX_MENTIONS,
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
    /// A read in the background (a search across models the user didn't open): it never asks the
    /// user for anything, no sign-in and no consent. What can only be read with a permission not
    /// granted yet is left out, and a note says so.
    #[serde(default)]
    pub silent: bool,
    /// Only the sources outside the database matter (a search across many databases for who reads an
    /// object): what costs requests and isn't a source, like the reports and dashboards that read a Power
    /// BI model or its engine dependencies, is left out.
    #[serde(default)]
    pub sources_only: bool,
    /// Not one database's graph: what a whole tenant says about where its databases read from, in one
    /// read that opens no model (Power BI's admin scanner API). The graph's `objects` are the models the
    /// answer covers and its `external` the data sources each one names (server and database, no table).
    /// A driver without such a read, or a user without the right for it, answers `Error::Unsupported`.
    #[serde(default)]
    pub tenant_sources: bool,
    /// Read one chunk of the graph instead of all of it (see [`LineageStep`]); `None`: the whole graph
    /// in one call, as always. A driver that doesn't read in chunks ignores it and answers the whole graph
    /// (with `next` empty), so the loop that follows `next` ends after the first call.
    #[serde(default)]
    pub step: Option<LineageStep>,
}

/// One chunk of a graph read in pieces, each its own short call: the stage ("structure": the objects and
/// what the model says itself; "dependencies": the code's dependencies, `len` objects from `offset`;
/// "consumers": the reports and dashboards), where it starts and how many objects the stage has in all
/// (for a progress label). A driver names its own stages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageStep {
    pub stage: String,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub len: u32,
    #[serde(default)]
    pub total: u32,
    /// Don't read it: answer only which chunk follows (the host gives a chunk up and goes on).
    #[serde(default)]
    pub skip: bool,
}

impl LineageStep {
    /// The first chunk of any graph: the host starts a chunked read with it.
    pub fn start() -> Self {
        LineageStep {
            stage: "structure".into(),
            ..Default::default()
        }
    }

    /// The same chunk, as a record of what wasn't read (it carries no `skip`).
    pub fn as_failed(&self) -> Self {
        LineageStep {
            skip: false,
            ..self.clone()
        }
    }
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

/// Something outside the database that reads one of its objects: a Power BI
/// report or dashboard reading a semantic model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageConsumer {
    pub consumer: ExternalRef,
    /// The object it reads.
    pub from: ObjectRef,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LineageGraph {
    /// The nodes, also those no edge touches (what is left alone shows too).
    pub objects: Vec<ObjectRef>,
    pub edges: Vec<LineageEdge>,
    #[serde(default)]
    pub external: Vec<LineageExternal>,
    /// What outside the database reads its objects (reports, dashboards).
    #[serde(default)]
    pub consumers: Vec<LineageConsumer>,
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
    /// The chunk to read next to finish the graph (`None`: it is complete, or the driver reads it whole).
    /// Set on the graph a chunked read has so far, so a cached one resumes where it stopped.
    #[serde(default)]
    pub next: Option<LineageStep>,
    /// Chunks that couldn't be read (after a retry): the graph lacks what they hold, and says so.
    #[serde(default)]
    pub failed: Vec<LineageStep>,
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

/// Adds `e` to `edges`, one edge per (from, to, relation): when it is there already the confidence is the
/// strongest, the details and the lines are joined (a detail that is the start of a longer one is the
/// longer one). `same` says when two nodes are the same.
pub fn merge_edge(
    edges: &mut Vec<LineageEdge>,
    e: LineageEdge,
    same: impl Fn(&ObjectRef, &ObjectRef) -> bool,
) {
    let found = edges
        .iter_mut()
        .find(|x| x.relation == e.relation && same(&x.from, &e.from) && same(&x.to, &e.to));
    let Some(x) = found else {
        edges.push(e);
        return;
    };
    x.confidence = x.confidence.min(e.confidence);
    if let Some(d) = e.detail {
        let mut parts: Vec<String> = x
            .detail
            .as_deref()
            .map(|t| t.split("; ").map(str::to_string).collect())
            .unwrap_or_default();
        let at = parts
            .iter()
            .position(|p| *p == d || d.starts_with(&format!("{p}: ")));
        match at {
            Some(k) => parts[k] = d,
            None if parts.iter().any(|p| p.starts_with(&format!("{d}: "))) => {}
            None => parts.push(d),
        }
        x.detail = Some(parts.join("; "));
    }
    for m in e.mentions {
        if !x.mentions.contains(&m) && x.mentions.len() < MAX_MENTIONS {
            x.mentions.push(m);
        }
    }
}

fn same_name(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind && a.schema == b.schema && a.name.eq_ignore_ascii_case(&b.name)
}

impl LineageGraph {
    /// Adds what a chunk read ([`LineageScope::step`]) to the graph so far. `next` and `failed` are the
    /// caller's to set: they say what is left, not what was read.
    pub fn absorb(&mut self, chunk: LineageGraph) {
        for o in chunk.objects {
            if !self.objects.iter().any(|x| same_name(x, &o)) {
                self.objects.push(o);
            }
        }
        for e in chunk.edges {
            merge_edge(&mut self.edges, e, same_name);
        }
        for e in chunk.external {
            let seen = self.external.iter().any(|x| {
                same_name(&x.to, &e.to)
                    && (
                        &x.source.system,
                        &x.source.server,
                        &x.source.database,
                        &x.source.schema,
                        &x.source.name,
                        &x.source.detail,
                    ) == (
                        &e.source.system,
                        &e.source.server,
                        &e.source.database,
                        &e.source.schema,
                        &e.source.name,
                        &e.source.detail,
                    )
            });
            if !seen {
                self.external.push(e);
            }
        }
        self.consumers.extend(chunk.consumers);
        self.jobs.extend(chunk.jobs);
        self.scanned += chunk.scanned;
        for u in chunk.unreadable {
            if !self.unreadable.contains(&u) {
                self.unreadable.push(u);
            }
        }
        self.truncated |= chunk.truncated;
        for n in chunk.notes {
            if !self.notes.contains(&n) {
                self.notes.push(n);
            }
        }
        for n in chunk.job_notes {
            if !self.job_notes.contains(&n) {
                self.job_notes.push(n);
            }
        }
    }
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
    let ctx = &super::with_databases(s, ctx).await;
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
            databases: None,
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

    fn obj(name: &str) -> ObjectRef {
        ObjectRef {
            kind: "table".into(),
            schema: None,
            name: name.into(),
        }
    }

    fn edge(from: &str, to: &str, c: Confidence, detail: &str) -> LineageEdge {
        LineageEdge {
            from: obj(from),
            to: obj(to),
            relation: Relation::Code,
            confidence: c,
            detail: Some(detail.into()),
            mentions: Vec::new(),
        }
    }

    #[test]
    fn chunks_absorb_into_one_graph() {
        let mut g = LineageGraph {
            objects: vec![obj("A"), obj("B")],
            edges: vec![edge("A", "B", Confidence::Probable, "filtro")],
            scanned: 3,
            notes: vec!["n".into()],
            ..Default::default()
        };
        g.absorb(LineageGraph {
            objects: vec![obj("a"), obj("C")],
            edges: vec![
                edge("a", "b", Confidence::Confirmed, "filtro: x"),
                edge("B", "C", Confidence::Confirmed, "y"),
            ],
            scanned: 2,
            notes: vec!["n".into(), "m".into()],
            ..Default::default()
        });
        assert_eq!(
            g.objects.len(),
            3,
            "a name is one object, whatever its case"
        );
        assert_eq!(g.edges.len(), 2);
        assert_eq!(g.edges[0].confidence, Confidence::Confirmed);
        assert_eq!(g.edges[0].detail.as_deref(), Some("filtro: x"));
        assert_eq!((g.scanned, g.notes.len()), (5, 2));
        // The new fields default: a graph from before chunks is complete.
        let old: LineageGraph = serde_json::from_str(r#"{"objects":[],"edges":[]}"#).unwrap();
        assert!(old.next.is_none() && old.failed.is_empty());
        let scope: LineageScope = serde_json::from_str("{}").unwrap();
        assert!(scope.step.is_none());
    }

    /// [`Fake`]'s objects plus three jobs; counts the `list_databases` calls.
    struct WithJobs {
        databases_listed: u32,
    }

    #[async_trait::async_trait]
    impl Session for WithJobs {
        async fn server_version(&mut self) -> Result<String> {
            Ok(String::new())
        }
        async fn list_databases(&mut self) -> Result<Vec<String>> {
            self.databases_listed += 1;
            Ok(vec!["ventas".into()])
        }
        async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
            Fake.list_objects().await
        }
        async fn columns(&mut self, o: &ObjectRef) -> Result<Vec<crate::ColumnInfo>> {
            Fake.columns(o).await
        }
        async fn definition(&mut self, o: &ObjectRef) -> Result<Option<String>> {
            Fake.definition(o).await
        }
        async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
            Fake.database_schema().await
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            String::new()
        }
        async fn execute(&mut self, _: &str, _: usize, _: &mut crate::QueryOutcome) -> Result<()> {
            Ok(())
        }
        async fn jobs(&mut self) -> Result<crate::jobs::JobList> {
            let job = |id: &str| crate::jobs::ServerJob {
                id: id.into(),
                name: id.into(),
                ..Default::default()
            };
            Ok(crate::jobs::JobList {
                jobs: vec![job("a"), job("b"), job("c")],
                notes: vec![],
            })
        }
        async fn job_detail(&mut self, id: &str) -> Result<crate::jobs::JobDetail> {
            Ok(crate::jobs::JobDetail {
                job: crate::jobs::ServerJob {
                    id: id.into(),
                    name: id.into(),
                    ..Default::default()
                },
                steps: vec![crate::jobs::JobStep {
                    name: "Paso".into(),
                    command: Some("SELECT * FROM dbo.v_ventas".into()),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }
    }

    #[test]
    fn the_jobs_of_a_graph_list_the_databases_once() {
        let mut s = WithJobs {
            databases_listed: 0,
        };
        let scope = LineageScope {
            jobs: true,
            ..Default::default()
        };
        let g = ready(graph_scan(&mut s, "ventas", &scope, &ctx())).unwrap();
        assert_eq!(g.jobs.len(), 3);
        assert_eq!(s.databases_listed, 1);
    }
}

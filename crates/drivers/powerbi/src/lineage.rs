//! The lineage of the whole semantic model (`Session::lineage_graph`): the
//! database of a Power BI connection *is* the model.
//!
//! Nodes (object level, no columns): the model's tables, calculation groups,
//! measures, functions, roles and shared M expressions, each as the explorer
//! lists it (auto date/time tables are left out, as there). A calculated
//! column or a calculated table is its table; the edge says which column.
//!
//! Edges go from what is used to what uses it:
//!
//! - DAX (measures, calculated columns and tables, calculation items, RLS
//!   filters, functions): with write permission on a model on capacity, one
//!   `INFO.CALCDEPENDENCY()` for the whole model (`Confirmed`); otherwise the
//!   DAX tokens of every expression, resolved against the model
//!   (`Probable`). Without permission to see the expressions the graph is
//!   built from what is readable and says so in `notes`.
//! - Relationships: the table on the one side to the one on the many side
//!   (`Relation::ForeignKey`, `Confirmed`), the way a foreign key points.
//! - A role to the tables its filters apply to (`Confirmed`).
//! - Power Query (M) of the partitions and the shared expressions: the
//!   shared expressions, parameters and tables it names (`Probable`) and the
//!   sources outside the model as `external` (`INFO.CALCDEPENDENCY` doesn't
//!   cover M).
//!
//! Power BI has no jobs in the lineage: its refreshes run no code that names
//! an object.

use crate::dependencies::{NO_EXPRESSIONS, NO_METADATA};
use crate::meta::info::{calc_dependencies_of, CalcDependency};
use crate::meta::service::{session_model, Service};
use crate::meta::{relationship_name, same, ModelMeta, TableKind, Tier};
use crate::references::{self, m, Use};
use crate::{Ctx, CALCULATION_GROUP, EXPRESSION, FUNCTION, MEASURE, MODEL, ROLE, TABLE};
use dbine_driver::dependencies::{
    merge_edge, LineageConsumer, LineageEdge, LineageExternal, LineageGraph, LineageScope,
    LineageStep, GRAPH_OBJECTS, MAX_MENTIONS,
};
use dbine_driver::{Confidence, Mention, ObjectRef, Relation, Result};

pub async fn lineage_graph(cx: &mut Ctx, scope: &LineageScope) -> Result<LineageGraph> {
    lineage_in(cx, scope).await
}

fn node(kind: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        schema: None,
        name: name.into(),
    }
}

fn same_node(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind && same(&a.name, &b.name)
}

/// What an expression is written in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Dax,
    M,
    Sql,
}

/// An expression of the model and the object it belongs to.
struct Unit {
    owner: ObjectRef,
    /// What says which part of the owner it is (a calculated column…).
    detail: Option<String>,
    lang: Lang,
    /// The table whose row context a bare `[c]` reads.
    home: Option<String>,
    /// `None`: hidden without write permission.
    text: Option<String>,
    /// How `unreadable` names it.
    label: String,
    /// The `[OBJECT]` of its `INFO.CALCDEPENDENCY()` rows (DAX units).
    object: String,
}

fn units(meta: &ModelMeta) -> Vec<Unit> {
    use crate::dax::quote::{column_ref, quote_column, quote_table};
    let mut out = Vec::new();
    let mut add = |owner: ObjectRef,
                   detail: Option<String>,
                   lang: Lang,
                   home: Option<&str>,
                   text: Option<&String>,
                   label: String,
                   object: &str| {
        out.push(Unit {
            owner,
            detail,
            lang,
            home: home.map(str::to_string),
            text: text.cloned(),
            label,
            object: object.to_string(),
        });
    };
    for m in &meta.measures {
        add(
            node(MEASURE, &m.name),
            None,
            Lang::Dax,
            None,
            m.expression.as_ref(),
            column_ref(&m.table, &m.name),
            &m.name,
        );
    }
    for t in &meta.tables {
        let owner = node(references::table_kind(meta, &t.name), &t.name);
        if let Some(e) = &t.expression {
            add(
                owner.clone(),
                Some("tabla calculada".into()),
                Lang::Dax,
                None,
                Some(e),
                quote_table(&t.name),
                &t.name,
            );
        }
        for c in t.columns.iter().filter(|c| c.expression.is_some()) {
            add(
                owner.clone(),
                Some(format!(
                    "columna calculada {}",
                    column_ref(&t.name, &c.name)
                )),
                Lang::Dax,
                Some(&t.name),
                c.expression.as_ref(),
                column_ref(&t.name, &c.name),
                &c.name,
            );
        }
        if t.kind == TableKind::Data {
            for p in &t.partitions {
                let lang = match p.source_kind.as_deref() {
                    Some("m") | None => Lang::M,
                    Some("query") => Lang::Sql,
                    _ => continue,
                };
                add(
                    owner.clone(),
                    None,
                    lang,
                    None,
                    p.expression.as_ref(),
                    p.name.clone(),
                    "",
                );
            }
        }
    }
    for g in &meta.calculation_groups {
        for i in &g.items {
            add(
                node(CALCULATION_GROUP, &g.table),
                Some(format!("elemento {}", quote_column(&i.name))),
                Lang::Dax,
                None,
                i.expression.as_ref(),
                column_ref(&g.table, &i.name),
                &i.name,
            );
        }
    }
    for r in &meta.roles {
        for (t, f) in &r.table_filters {
            add(
                node(ROLE, &r.name),
                Some(format!("filtro de {}", quote_table(t))),
                Lang::Dax,
                Some(t),
                Some(f),
                format!("{} ({})", r.name, quote_table(t)),
                &r.name,
            );
        }
    }
    for f in &meta.functions {
        add(
            node(FUNCTION, &f.name),
            None,
            Lang::Dax,
            None,
            f.expression.as_ref(),
            f.name.clone(),
            &f.name,
        );
    }
    for e in &meta.expressions {
        add(
            node(EXPRESSION, &e.name),
            None,
            Lang::M,
            None,
            e.expression.as_ref(),
            e.name.clone(),
            "",
        );
    }
    out
}

/// The node a use of the model is, as the explorer lists it.
fn used_node(meta: &ModelMeta, u: &Use) -> ObjectRef {
    match u {
        Use::Table(t) | Use::Column(t, _) => {
            let name = meta.table(t).map_or(t.as_str(), |x| x.name.as_str());
            node(references::table_kind(meta, name), name)
        }
        Use::Measure(x) => node(
            MEASURE,
            meta.measure(x).map_or(x.as_str(), |m| m.name.as_str()),
        ),
        Use::Function(f) => node(FUNCTION, f),
    }
}

/// The object and the part of it that an `INFO.CALCDEPENDENCY()` row
/// belongs to; `None` for the rows the structure covers (relationships,
/// hierarchies…).
fn row_owner(meta: &ModelMeta, r: &CalcDependency) -> Option<(ObjectRef, Option<String>)> {
    use crate::dax::quote::{column_ref, quote_column, quote_table};
    let table = r.table.clone().unwrap_or_default();
    Some(match r.object_type.to_ascii_uppercase().as_str() {
        "MEASURE" => (node(MEASURE, &r.object), None),
        "CALC_COLUMN" => (
            node(references::table_kind(meta, &table), &table),
            Some(format!(
                "columna calculada {}",
                column_ref(&table, &r.object)
            )),
        ),
        "CALC_TABLE" => (
            node(references::table_kind(meta, &r.object), &r.object),
            Some("tabla calculada".into()),
        ),
        "CALCULATION_ITEM" => (
            node(CALCULATION_GROUP, &table),
            Some(format!("elemento {}", quote_column(&r.object))),
        ),
        "ROWS_ALLOWED" => {
            if meta.roles.iter().any(|x| same(&x.name, &r.object)) {
                (
                    node(ROLE, &r.object),
                    Some(format!("filtro de {}", quote_table(&table))),
                )
            } else {
                (
                    node(TABLE, &table),
                    Some("filtro de seguridad (RLS)".into()),
                )
            }
        }
        ot if ot.contains("FUNCTION") => (node(FUNCTION, &r.object), None),
        _ => return None,
    })
}

struct Builder {
    edges: Vec<LineageEdge>,
}

impl Builder {
    /// One edge per (used, user, relation): the confidence is the strongest,
    /// the details and the lines are joined.
    fn edge(
        &mut self,
        from: ObjectRef,
        to: ObjectRef,
        relation: Relation,
        confidence: Confidence,
        detail: Option<String>,
        mentions: Vec<Mention>,
    ) {
        if same_node(&from, &to) {
            return;
        }
        merge_edge(
            &mut self.edges,
            LineageEdge {
                from,
                to,
                relation,
                confidence,
                detail,
                mentions,
            },
            same_node,
        );
    }
}

/// The part of the owner and the columns it uses.
fn combine(part: Option<&str>, columns: Option<String>) -> Option<String> {
    match (part, columns) {
        (Some(p), Some(c)) => Some(format!("{p}: {c}")),
        (Some(p), None) => Some(p.to_string()),
        (None, c) => c,
    }
}

fn details(uses: &[&Use], meta: &ModelMeta) -> Option<String> {
    use crate::dax::quote::column_ref;
    let cols: Vec<String> = uses
        .iter()
        .filter_map(|u| match u {
            Use::Column(t, c) => Some(column_ref(meta.table(t).map_or(t.as_str(), |x| &x.name), c)),
            _ => None,
        })
        .collect();
    (!cols.is_empty()).then(|| cols.join(", "))
}

pub(crate) async fn lineage_in<S: Service>(
    s: &mut S,
    scope: &LineageScope,
) -> Result<LineageGraph> {
    s.meta_state().silent = scope.silent;
    let g = match &scope.step {
        Some(step) => lineage_chunk(s, scope, step).await,
        None => lineage_read(s, scope).await,
    };
    s.meta_state().silent = false;
    g
}

/// What every read of the graph starts from: the model, its metadata and the nodes in the scope.
struct Frame {
    model: crate::catalog::Model,
    meta: std::sync::Arc<ModelMeta>,
    /// The nodes, as the explorer lists them. The model comes first, so a cap leaves it: what reads the
    /// model hangs from it.
    all: Vec<ObjectRef>,
    truncated: bool,
    model_node: ObjectRef,
}

impl Frame {
    fn has(&self, o: &ObjectRef) -> bool {
        self.all.iter().any(|n| same_node(n, o))
    }

    /// The model reads its tables.
    fn model_edges(&self) -> Vec<LineageEdge> {
        let mut out = Vec::new();
        if self.has(&self.model_node) {
            for o in &self.all {
                if o.kind == TABLE || o.kind == CALCULATION_GROUP {
                    out.push(LineageEdge {
                        from: o.clone(),
                        to: self.model_node.clone(),
                        relation: Relation::Code,
                        confidence: Confidence::Confirmed,
                        detail: None,
                        mentions: Vec::new(),
                    });
                }
            }
        }
        out
    }

    /// What outside the model reads it (the reports and dashboards, over REST: no metadata permission
    /// needed) is read, unless the search only wants the sources: it costs a request each (and one per
    /// dashboard for its tiles).
    fn wants_consumers(&self, scope: &LineageScope) -> bool {
        self.has(&self.model_node) && !scope.sources_only
    }

    /// The engine's dependencies are asked (else the DAX is read as tokens).
    fn wants_engine(&self, scope: &LineageScope) -> bool {
        self.meta.tier == Tier::Info && self.model.write_permission && !scope.sources_only
    }
}

async fn frame<S: Service>(s: &mut S, scope: &LineageScope) -> Result<Frame> {
    let model = session_model(s)?;
    let meta = crate::meta::model_for(s, &model).await?;
    let mut all: Vec<ObjectRef> = vec![node(MODEL, &model.listed_as)];
    all.extend(meta.plain_tables().map(|t| node(TABLE, &t.name)));
    all.extend(
        meta.calculation_groups
            .iter()
            .map(|c| node(CALCULATION_GROUP, &c.table)),
    );
    all.extend(meta.measures.iter().map(|m| node(MEASURE, &m.name)));
    all.extend(meta.functions.iter().map(|f| node(FUNCTION, &f.name)));
    all.extend(meta.roles.iter().map(|r| node(ROLE, &r.name)));
    all.extend(meta.expressions.iter().map(|e| node(EXPRESSION, &e.name)));
    // A model has no schemas: a scope on them leaves nothing out.
    all.retain(|o| scope.kinds.is_empty() || scope.kinds.contains(&o.kind));
    let limit = if scope.limit == 0 {
        GRAPH_OBJECTS
    } else {
        scope.limit
    } as usize;
    let truncated = all.len() > limit;
    all.truncate(limit);
    let model_node = node(MODEL, &model.listed_as);
    Ok(Frame {
        model,
        meta,
        all,
        truncated,
        model_node,
    })
}

/// Relationships (the one side is what the many side uses) and roles (a role applies to the tables it
/// filters): the model's own structure, all in the metadata already read.
fn structure_edges(f: &Frame, b: &mut Builder) {
    let meta = &f.meta;
    for r in meta.model_relationships() {
        let (from, to) = (
            node(references::table_kind(meta, &r.to_table), &r.to_table),
            node(references::table_kind(meta, &r.from_table), &r.from_table),
        );
        if !f.has(&from) || !f.has(&to) {
            continue;
        }
        let mut detail = relationship_name(r);
        if !r.active {
            detail.push_str(" (inactiva)");
        }
        b.edge(
            from,
            to,
            Relation::ForeignKey,
            Confidence::Confirmed,
            Some(detail),
            Vec::new(),
        );
    }
    for r in &meta.roles {
        for (t, _) in &r.table_filters {
            let from = node(references::table_kind(meta, t), t);
            let to = node(ROLE, &r.name);
            if f.has(&from) && f.has(&to) {
                b.edge(
                    from,
                    to,
                    Relation::Code,
                    Confidence::Confirmed,
                    Some(format!("filtro de {}", crate::dax::quote::quote_table(t))),
                    Vec::new(),
                );
            }
        }
    }
}

/// The edges the engine's `INFO.CALCDEPENDENCY()` rows give (`Confirmed`). `only`: just the rows of
/// these (owner, part) pairs, for a chunk that asked for some of the objects.
fn engine_edges(
    f: &Frame,
    rows: &[CalcDependency],
    only: Option<&[(&ObjectRef, &Option<String>)]>,
    b: &mut Builder,
) {
    let meta = &f.meta;
    for r in rows {
        let Some((owner, detail)) = row_owner(meta, r) else {
            continue;
        };
        if let Some(only) = only {
            if !only
                .iter()
                .any(|(o, d)| same_node(o, &owner) && **d == detail)
            {
                continue;
            }
        }
        let Some(u) = references::calc_use(r) else {
            continue;
        };
        let used = used_node(meta, &u);
        if !f.has(&owner) || !f.has(&used) {
            continue;
        }
        let home = (detail.is_some() && owner.kind == TABLE).then(|| owner.name.clone());
        let at: Vec<usize> = r
            .expression
            .as_deref()
            .map(|e| references::dax_uses(e, home.as_deref(), meta))
            .unwrap_or_default()
            .into_iter()
            .filter(|(x, _)| x.object() == u.object())
            .map(|(_, at)| at)
            .collect();
        let mentions = r
            .expression
            .as_deref()
            .map(|e| references::at_lines(e, &at))
            .unwrap_or_default();
        let d = combine(detail.as_deref(), details(&[&u], meta));
        b.edge(
            used,
            owner,
            Relation::Code,
            Confidence::Confirmed,
            d,
            mentions,
        );
    }
}

/// A DAX expression read as tokens (`Probable`): what the engine's dependencies would say, when they
/// can't be asked. An expression the user can't see is said in `g`.
fn dax_token_edges(f: &Frame, u: &Unit, b: &mut Builder, g: &mut LineageGraph, hidden: &mut bool) {
    let meta = &f.meta;
    let Some(text) = u.text.as_deref() else {
        *hidden = true;
        g.unreadable.push(u.label.clone());
        return;
    };
    if !f.has(&u.owner) {
        return;
    }
    let found = references::dax_uses(text, u.home.as_deref(), meta);
    let mut seen: Vec<Use> = Vec::new();
    for (x, _) in &found {
        if !seen.contains(x) {
            seen.push(x.clone());
        }
    }
    let mut objects: Vec<(&str, String)> = Vec::new();
    for x in &seen {
        if !objects.contains(&x.object()) {
            objects.push(x.object());
        }
    }
    for o in objects {
        let of: Vec<&Use> = seen.iter().filter(|x| x.object() == o).collect();
        let at: Vec<usize> = found
            .iter()
            .filter(|(x, _)| x.object() == o)
            .map(|&(_, at)| at)
            .collect();
        let used = used_node(meta, of[0]);
        if !f.has(&used) {
            continue;
        }
        b.edge(
            used,
            u.owner.clone(),
            Relation::Code,
            Confidence::Probable,
            combine(u.detail.as_deref(), details(&of, meta)),
            references::at_lines(text, &at),
        );
    }
}

/// Power Query (M) of the partitions and the shared expressions: what they name inside the model and the
/// sources outside it. `INFO.CALCDEPENDENCY` doesn't cover M.
fn m_edges(f: &Frame, units: &[Unit], b: &mut Builder, g: &mut LineageGraph) {
    let meta = &f.meta;
    let mut hidden_m = false;
    let m_names: Vec<(String, String)> = meta
        .expressions
        .iter()
        .filter_map(|e| Some((e.name.clone(), e.expression.clone()?)))
        .collect();
    let env = m::Env { shared: &m_names };
    let wanted: Vec<String> = meta
        .expressions
        .iter()
        .map(|e| e.name.clone())
        .chain(meta.tables.iter().map(|t| t.name.clone()))
        .collect();
    let mut external: Vec<LineageExternal> = Vec::new();
    let mut add_external =
        |owner: &ObjectRef, src: dbine_driver::dependencies::ExternalRef, at| match external
            .iter_mut()
            .find(|e| same_node(&e.to, owner) && same_source(&e.source, &src))
        {
            Some(e) => {
                let mut room = MAX_MENTIONS.saturating_sub(e.source.mentions.len());
                for m in at {
                    if room > 0 && !e.source.mentions.contains(&m) {
                        e.source.mentions.push(m);
                        room -= 1;
                    }
                }
            }
            None => {
                let mut source = src;
                source.mentions = at;
                external.push(LineageExternal {
                    source,
                    to: owner.clone(),
                });
            }
        };
    for u in units.iter().filter(|u| u.lang != Lang::Dax) {
        let Some(text) = u.text.as_deref() else {
            hidden_m = true;
            continue;
        };
        match u.lang {
            Lang::Dax => {}
            Lang::Sql => add_external(&u.owner, m::native_source(text), Vec::new()),
            Lang::M => {
                for src in m::sources(text, &env) {
                    add_external(&u.owner, src.external, references::at_lines(text, &src.at));
                }
                for (name, at) in m::names(text, &wanted) {
                    let kind = if meta.expressions.iter().any(|e| e.name == name) {
                        EXPRESSION
                    } else {
                        references::table_kind(meta, &name)
                    };
                    let from = node(kind, &name);
                    if f.has(&from) && f.has(&u.owner) {
                        b.edge(
                            from,
                            u.owner.clone(),
                            Relation::Code,
                            Confidence::Probable,
                            None,
                            references::at_lines(text, &at),
                        );
                    }
                }
            }
        }
    }
    if hidden_m {
        g.notes.push(crate::references::NO_PARTITIONS.into());
    }
    external.retain(|e| f.has(&e.to));
    g.external = external;
}

async fn lineage_read<S: Service>(s: &mut S, scope: &LineageScope) -> Result<LineageGraph> {
    let f = frame(s, scope).await?;
    let mut g = LineageGraph {
        truncated: f.truncated,
        ..Default::default()
    };
    let model_edges = f.model_edges();
    if f.wants_consumers(scope) {
        let c = crate::consumers::read(s, &f.model).await?;
        g.consumers = c
            .items
            .into_iter()
            .map(|consumer| LineageConsumer {
                consumer,
                from: f.model_node.clone(),
            })
            .collect();
        g.notes.extend(c.notes);
    }

    if f.meta.tier == Tier::Stats {
        g.notes.push(NO_METADATA.into());
        g.edges = model_edges;
        g.objects = f.all;
        return Ok(g);
    }

    let units = units(&f.meta);
    g.scanned = units.iter().filter(|u| u.text.is_some()).count() as u32;
    let mut b = Builder { edges: Vec::new() };
    structure_edges(&f, &mut b);

    // DAX: the engine's dependencies, the whole model in one call.
    let mut confirmed = false;
    if f.wants_engine(scope) {
        match calc_dependencies_of(s, &f.model).await {
            Ok(rows) => {
                confirmed = true;
                engine_edges(&f, &rows, None, &mut b);
            }
            Err(e) if crate::meta::is_fatal(&e) => return Err(e),
            Err(_) => {}
        }
    }
    if !confirmed {
        let mut hidden_dax = false;
        for u in units.iter().filter(|u| u.lang == Lang::Dax) {
            dax_token_edges(&f, u, &mut b, &mut g, &mut hidden_dax);
        }
        if hidden_dax {
            g.notes.push(NO_EXPRESSIONS.into());
        }
    }
    m_edges(&f, &units, &mut b, &mut g);
    g.edges = b.edges;
    g.edges.extend(model_edges);
    g.objects = f.all;
    Ok(g)
}

// -- the graph in chunks ---------------------------------------------------------------------------

/// The stages of a chunked read, in the order they are read: the objects and the model's own structure
/// (all in the metadata), the dependencies of the DAX (`CHUNK_OBJECTS` expressions a call), and the
/// reports and dashboards that read the model.
pub const STAGE_STRUCTURE: &str = "structure";
pub const STAGE_DEPENDENCIES: &str = "dependencies";
pub const STAGE_CONSUMERS: &str = "consumers";
/// Expressions whose dependencies one call asks the engine for.
pub const CHUNK_OBJECTS: usize = 50;

#[cfg(test)]
static TEST_CHUNK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(CHUNK_OBJECTS);

fn chunk_objects() -> usize {
    #[cfg(test)]
    {
        TEST_CHUNK.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(test))]
    {
        CHUNK_OBJECTS
    }
}

fn step(stage: &str, offset: usize, len: usize, total: usize) -> LineageStep {
    LineageStep {
        stage: stage.into(),
        offset: offset as u32,
        len: len as u32,
        total: total as u32,
        skip: false,
    }
}

/// The chunk that follows the structure (`after`: `None`) or the dependencies that start at `after`.
fn next_step(
    f: &Frame,
    scope: &LineageScope,
    dax_units: usize,
    after: Option<usize>,
) -> Option<LineageStep> {
    if f.meta.tier != Tier::Stats && f.wants_engine(scope) {
        let from = after.map_or(0, |o| o + chunk_objects());
        if from < dax_units {
            return Some(step(
                STAGE_DEPENDENCIES,
                from,
                chunk_objects().min(dax_units - from),
                dax_units,
            ));
        }
    }
    f.wants_consumers(scope)
        .then(|| step(STAGE_CONSUMERS, 0, 0, 0))
}

fn dax_units(units: Vec<Unit>) -> Vec<Unit> {
    units.into_iter().filter(|u| u.lang == Lang::Dax).collect()
}

/// One chunk of the graph (`LineageScope::step`): what that stage reads, and `next`, the chunk that
/// follows. The chunks, merged with [`LineageGraph::absorb`], are the graph [`lineage_read`] answers
/// whole. Each is a call of its own, so no single one waits on the whole model.
async fn lineage_chunk<S: Service>(
    s: &mut S,
    scope: &LineageScope,
    at: &LineageStep,
) -> Result<LineageGraph> {
    let f = frame(s, scope).await?;
    let mut g = LineageGraph::default();
    let n_dax = if f.meta.tier == Tier::Stats {
        0
    } else {
        dax_units(units(&f.meta)).len()
    };
    let following = match at.stage.as_str() {
        STAGE_DEPENDENCIES => next_step(&f, scope, n_dax, Some(at.offset as usize)),
        STAGE_CONSUMERS => None,
        _ => next_step(&f, scope, n_dax, None),
    };
    if at.skip {
        g.next = following;
        return Ok(g);
    }
    match at.stage.as_str() {
        STAGE_CONSUMERS => {
            if f.wants_consumers(scope) {
                let c = crate::consumers::read(s, &f.model).await?;
                g.consumers = c
                    .items
                    .into_iter()
                    .map(|consumer| LineageConsumer {
                        consumer,
                        from: f.model_node.clone(),
                    })
                    .collect();
                g.notes.extend(c.notes);
            }
        }
        STAGE_DEPENDENCIES => {
            let units = dax_units(units(&f.meta));
            let from = (at.offset as usize).min(units.len());
            let batch = &units[from..(from + at.len.max(1) as usize).min(units.len())];
            let mut b = Builder { edges: Vec::new() };
            let mut confirmed = false;
            if f.wants_engine(scope) && !batch.is_empty() {
                let mut names: Vec<String> = Vec::new();
                for u in batch {
                    let n = crate::dax::quote::string(&u.object);
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
                let q = format!(
                    "EVALUATE FILTER(INFO.CALCDEPENDENCY(), [OBJECT] IN {{{}}})",
                    names.join(", ")
                );
                match crate::meta::info::calc_dependencies_query(s, &f.model, &q).await {
                    Ok(rows) => {
                        confirmed = true;
                        let only: Vec<(&ObjectRef, &Option<String>)> =
                            batch.iter().map(|u| (&u.owner, &u.detail)).collect();
                        engine_edges(&f, &rows, Some(&only), &mut b);
                    }
                    Err(e) if crate::meta::is_fatal(&e) => return Err(e),
                    Err(_) => {}
                }
            }
            if !confirmed {
                let mut hidden = false;
                for u in batch {
                    dax_token_edges(&f, u, &mut b, &mut g, &mut hidden);
                }
                if hidden {
                    g.notes.push(NO_EXPRESSIONS.into());
                }
            }
            g.edges = b.edges;
        }
        _ => {
            g.truncated = f.truncated;
            g.edges = f.model_edges();
            if f.meta.tier == Tier::Stats {
                g.notes.push(NO_METADATA.into());
            } else {
                let units = units(&f.meta);
                g.scanned = units.iter().filter(|u| u.text.is_some()).count() as u32;
                let mut b = Builder { edges: Vec::new() };
                structure_edges(&f, &mut b);
                // Without the engine's dependencies asked, the DAX is read as tokens right here.
                if !f.wants_engine(scope) {
                    let mut hidden = false;
                    for u in units.iter().filter(|u| u.lang == Lang::Dax) {
                        dax_token_edges(&f, u, &mut b, &mut g, &mut hidden);
                    }
                    if hidden {
                        g.notes.push(NO_EXPRESSIONS.into());
                    }
                }
                m_edges(&f, &units, &mut b, &mut g);
                for e in b.edges {
                    merge_edge(&mut g.edges, e, same_node);
                }
            }
            g.objects = f.all.clone();
        }
    }
    g.next = following;
    crate::meta::touch(s, &f.model);
    Ok(g)
}

fn same_source(
    a: &dbine_driver::dependencies::ExternalRef,
    b: &dbine_driver::dependencies::ExternalRef,
) -> bool {
    (
        &a.system,
        &a.server,
        &a.database,
        &a.schema,
        &a.name,
        &a.detail,
    ) == (
        &b.system,
        &b.server,
        &b.database,
        &b.schema,
        &b.name,
        &b.detail,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::fake::{rowset, Fake};
    use serde_json::json;

    const LINEAGE: &str = include_str!("../tests/fixtures/tmsl_lineage.json");

    async fn graph(f: &mut Fake) -> LineageGraph {
        lineage_in(f, &LineageScope::default()).await.unwrap()
    }

    fn edge<'a>(
        g: &'a LineageGraph,
        from: (&str, &str),
        to: (&str, &str),
        relation: Relation,
    ) -> Option<&'a LineageEdge> {
        g.edges.iter().find(|e| {
            (e.from.kind.as_str(), e.from.name.as_str()) == from
                && (e.to.kind.as_str(), e.to.name.as_str()) == to
                && e.relation == relation
        })
    }

    fn calc_rows(rows: Vec<serde_json::Value>) -> crate::query::Rowset {
        rowset(
            &[
                "[DATABASE_NAME]",
                "[OBJECT_TYPE]",
                "[TABLE]",
                "[OBJECT]",
                "[EXPRESSION]",
                "[REFERENCED_OBJECT_TYPE]",
                "[REFERENCED_TABLE]",
                "[REFERENCED_OBJECT]",
            ],
            rows,
        )
    }

    /// The INFO tier, a writer on capacity.
    fn info_writer() -> Fake {
        let mut f = Fake::writer();
        f.arrow = true;
        crate::meta::info::tests::viewer_answers(&mut f);
        crate::meta::info::tests::admin_answers(&mut f);
        f
    }

    /// The DBA's model through the definition tier: nodes, DAX read as
    /// tokens (probable), the relationship and the role (confirmed) and
    /// every source outside the model.
    #[tokio::test]
    async fn the_definition_tier_builds_the_model_from_its_expressions() {
        let mut f = Fake::writer();
        f.definition(LINEAGE);
        let g = graph(&mut f).await;
        let nodes: Vec<(&str, &str)> = g
            .objects
            .iter()
            .map(|o| (o.kind.as_str(), o.name.as_str()))
            .collect();
        for n in [
            (TABLE, "pbi medidas"),
            (TABLE, "Metas"),
            (MEASURE, "Total valor"),
            (ROLE, "Directorio"),
            (EXPRESSION, "Servidor"),
            (EXPRESSION, "DW"),
        ] {
            assert!(nodes.contains(&n), "{n:?} in {nodes:?}");
        }
        assert_eq!(g.scanned, 10, "{:?}", g.notes);
        assert!(g.notes.is_empty() && !g.truncated && g.jobs.is_empty());

        // A relationship points from the one side to the many side.
        let fk = edge(
            &g,
            (TABLE, "Indicadores"),
            (TABLE, "pbi medidas"),
            Relation::ForeignKey,
        )
        .unwrap();
        assert_eq!(fk.confidence, Confidence::Confirmed);
        assert_eq!(
            fk.detail.as_deref(),
            Some("'pbi medidas'[indicador_id] → 'Indicadores'[id]")
        );
        // DAX read as tokens: probable, with the line.
        let m = edge(
            &g,
            (TABLE, "pbi medidas"),
            (MEASURE, "Total valor"),
            Relation::Code,
        )
        .unwrap();
        assert_eq!(m.confidence, Confidence::Probable);
        assert_eq!(m.detail.as_deref(), Some("'pbi medidas'[valor]"));
        assert_eq!(m.mentions[0].text, "SUM ( 'pbi medidas'[valor] )");
        // A measure over a measure.
        assert!(edge(
            &g,
            (MEASURE, "Total valor"),
            (MEASURE, "Valor promedio"),
            Relation::Code
        )
        .is_some());
        // A calculated column says which one it is.
        let c = edge(
            &g,
            (TABLE, "Indicadores"),
            (TABLE, "pbi medidas"),
            Relation::Code,
        )
        .unwrap();
        assert_eq!(
            c.detail.as_deref(),
            Some("columna calculada 'pbi medidas'[Indicador]: 'Indicadores'[nombre]")
        );
        // The role's filter applies to its table: that is catalog, whatever
        // the DAX says.
        let r = edge(
            &g,
            (TABLE, "pbi medidas"),
            (ROLE, "Directorio"),
            Relation::Code,
        )
        .unwrap();
        assert_eq!(r.confidence, Confidence::Confirmed);
        assert_eq!(
            r.detail.as_deref(),
            Some("filtro de 'pbi medidas': 'pbi medidas'[area]")
        );
        // Power Query: the parameter and the shared expression it names.
        assert!(edge(
            &g,
            (EXPRESSION, "Servidor"),
            (TABLE, "pbi medidas"),
            Relation::Code
        )
        .is_some());
        assert!(edge(
            &g,
            (EXPRESSION, "DW"),
            (TABLE, "Indicadores"),
            Relation::Code
        )
        .is_some());
        // The sources outside the model, each at its table.
        let src = |t: &str| -> Vec<(Option<&str>, Option<&str>)> {
            g.external
                .iter()
                .filter(|e| e.to.name == t)
                .map(|e| (e.source.system.as_deref(), e.source.name.as_deref()))
                .collect()
        };
        assert_eq!(src("pbi medidas"), [(Some("SQL Server"), Some("medidas"))]);
        assert_eq!(
            src("pbi medidas lake"),
            [(Some("Databricks"), Some("medidas"))]
        );
        assert_eq!(
            src("Indicadores"),
            [(Some("SQL Server"), Some("indicadores"))]
        );
        let native = g.external.iter().find(|e| e.to.name == "Metas").unwrap();
        assert_eq!(
            native.source.detail.as_deref(),
            Some("indicadores_directorio.metas, indicadores_directorio.medidas")
        );
    }

    #[tokio::test]
    async fn relationships_calculation_groups_and_inactive_ones() {
        let mut f = Fake::writer();
        f.definition(crate::definition::tmsl::tests::SAMPLE);
        let g = graph(&mut f).await;
        let fk = edge(
            &g,
            (TABLE, "Customer"),
            (TABLE, "Sales"),
            Relation::ForeignKey,
        )
        .unwrap();
        assert_eq!(fk.confidence, Confidence::Confirmed);
        let date = edge(&g, (TABLE, "Date"), (TABLE, "Sales"), Relation::ForeignKey).unwrap();
        assert!(date.detail.as_deref().unwrap().contains("(inactiva)"));
        // Nothing is a node twice, and the auto date tables aren't there.
        let mut names: Vec<String> = g
            .objects
            .iter()
            .map(|o| format!("{}:{}", o.kind, o.name.to_lowercase()))
            .collect();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n);
        assert!(!names.iter().any(|x| x.contains("localdatetable")));
        // The calculation item names the table it reads.
        assert!(g
            .edges
            .iter()
            .any(|e| e.to.kind == CALCULATION_GROUP && e.from.name == "Date"));
        // Edges only join nodes of the graph, never the node itself.
        for e in &g.edges {
            assert!(g.objects.iter().any(|o| same_node(o, &e.from)), "{e:?}");
            assert!(g.objects.iter().any(|o| same_node(o, &e.to)), "{e:?}");
            assert!(!same_node(&e.from, &e.to), "{e:?}");
        }
    }

    /// A writer on capacity: one `INFO.CALCDEPENDENCY()` for the whole
    /// model, and its rows are confirmed.
    #[tokio::test]
    async fn writers_on_capacity_get_the_engines_dependencies_in_one_call() {
        let mut f = info_writer();
        f.dax_answer(
            "INFO.CALCDEPENDENCY()",
            vec![calc_rows(vec![
                json!([
                    "db",
                    "MEASURE",
                    "Sales",
                    "Total Sales",
                    "SUM(Sales[Amount])",
                    "COLUMN",
                    "Sales",
                    "Amount"
                ]),
                json!([
                    "db",
                    "MEASURE",
                    "Sales",
                    "Total Sales",
                    "SUM(Sales[Amount])",
                    "TABLE",
                    "Customer",
                    "Customer"
                ]),
                json!([
                    "db",
                    "CALC_COLUMN",
                    "Sales",
                    "Margin",
                    "[Amount] - 1",
                    "COLUMN",
                    "Sales",
                    "Amount"
                ]),
                // The structure is read from the model: left out.
                json!([
                    "db",
                    "ACTIVE_RELATIONSHIP",
                    "Sales",
                    "x",
                    null,
                    "COLUMN",
                    "Sales",
                    "Amount"
                ]),
                // Something that isn't a node of the graph: left out.
                json!([
                    "db",
                    "MEASURE",
                    "Sales",
                    "Total Sales",
                    "SUM(Sales[Amount])",
                    "TABLE",
                    "LocalDateTable_1b2c",
                    "LocalDateTable_1b2c"
                ]),
            ])],
        );
        let g = graph(&mut f).await;
        assert_eq!(
            f.dax_log
                .iter()
                .filter(|q| q.contains("INFO.CALCDEPENDENCY()"))
                .count(),
            1,
            "{:?}",
            f.dax_log
        );
        let m = edge(
            &g,
            (TABLE, "Sales"),
            (MEASURE, "Total Sales"),
            Relation::Code,
        )
        .unwrap();
        assert_eq!(m.confidence, Confidence::Confirmed);
        assert_eq!(m.detail.as_deref(), Some("'Sales'[Amount]"));
        assert_eq!(m.mentions[0].text, "SUM(Sales[Amount])");
        assert!(edge(
            &g,
            (TABLE, "Customer"),
            (MEASURE, "Total Sales"),
            Relation::Code
        )
        .is_some());
        let c = edge(&g, (TABLE, "Sales"), (TABLE, "Sales"), Relation::Code);
        assert!(c.is_none(), "a column of its own table is no edge");
        assert!(g
            .edges
            .iter()
            .filter(|e| e.relation == Relation::Code)
            .all(|e| e.confidence == Confidence::Confirmed));
        assert!(g.notes.is_empty(), "{:?}", g.notes);
    }

    /// Without the engine's dependencies and with the expressions hidden,
    /// the graph keeps what the structure gives and says what it can't see.
    #[tokio::test]
    async fn hidden_expressions_are_said() {
        let mut f = info_writer();
        f.dax_refuse("INFO.CALCDEPENDENCY()");
        let g = graph(&mut f).await;
        assert!(
            g.notes.contains(&NO_EXPRESSIONS.to_string()),
            "{:?}",
            g.notes
        );
        assert!(g.unreadable.contains(&"'Sales'[Total Sales]".to_string()));
        assert!(!g.objects.is_empty());
        // What the engine would confirm isn't claimed: the structure is.
        assert!(g
            .edges
            .iter()
            .filter(|e| e.to.kind != ROLE && e.to.kind != MODEL)
            .all(|e| e.confidence != Confidence::Confirmed || e.relation == Relation::ForeignKey));
    }

    #[tokio::test]
    async fn the_statistics_tier_lists_the_model_and_says_why_there_is_nothing_else() {
        let mut f = Fake::new();
        f.dax_answer(
            "COLUMNSTATISTICS()",
            vec![rowset(
                &[
                    "[Table Name]",
                    "[Column Name]",
                    "[Min]",
                    "[Max]",
                    "[Cardinality]",
                    "[Max Length]",
                ],
                vec![json!(["Sales", "Amount", null, null, 3, 8])],
            )],
        );
        let g = graph(&mut f).await;
        // Only the model reading its tables: no code is known.
        assert!(g.edges.iter().all(|e| e.to.kind == MODEL) && g.external.is_empty());
        assert_eq!(g.notes, [NO_METADATA]);
        assert!(g.objects.iter().any(|o| o.name == "Sales"));
    }

    #[tokio::test]
    async fn the_scope_picks_kinds_and_caps_the_graph() {
        let mut f = Fake::writer();
        f.definition(LINEAGE);
        let g = lineage_in(
            &mut f,
            &LineageScope {
                kinds: vec![MEASURE.into(), TABLE.into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(g
            .objects
            .iter()
            .all(|o| o.kind == MEASURE || o.kind == TABLE));
        // Edges to what is out of the scope aren't drawn.
        assert!(g
            .edges
            .iter()
            .all(|e| e.from.kind != EXPRESSION && e.to.kind != ROLE));
        let g = lineage_in(
            &mut f,
            &LineageScope {
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(g.objects.len(), 3);
        assert!(g.truncated);
    }

    /// Every chunk, from the structure to the end, merged: the stages read and the graph.
    async fn chunked(f: &mut Fake, scope: &LineageScope) -> (LineageGraph, Vec<String>) {
        TEST_CHUNK.store(2, std::sync::atomic::Ordering::Relaxed);
        let mut step = Some(LineageStep {
            stage: STAGE_STRUCTURE.into(),
            ..Default::default()
        });
        let mut stages = Vec::new();
        let mut acc: Option<LineageGraph> = None;
        while let Some(st) = step {
            stages.push(format!("{}@{}", st.stage, st.offset));
            let chunk = lineage_in(
                f,
                &LineageScope {
                    step: Some(st),
                    ..scope.clone()
                },
            )
            .await
            .unwrap();
            step = chunk.next.clone();
            match &mut acc {
                None => acc = Some(chunk),
                Some(a) => a.absorb(chunk),
            }
        }
        let mut g = acc.unwrap();
        g.next = None;
        (g, stages)
    }

    /// What two graphs say, whatever the order they say it in.
    fn said(g: &LineageGraph) -> serde_json::Value {
        let mut objects: Vec<String> = g
            .objects
            .iter()
            .map(|o| format!("{}:{}", o.kind, o.name))
            .collect();
        objects.sort();
        let mut edges: Vec<String> = g
            .edges
            .iter()
            .map(|e| {
                let mut d: Vec<&str> = e.detail.as_deref().unwrap_or("").split("; ").collect();
                d.sort();
                format!(
                    "{}:{} > {}:{} {:?} {:?} {} {}",
                    e.from.kind,
                    e.from.name,
                    e.to.kind,
                    e.to.name,
                    e.relation,
                    e.confidence,
                    d.join("; "),
                    e.mentions.len()
                )
            })
            .collect();
        edges.sort();
        let mut ext: Vec<String> = g.external.iter().map(|e| format!("{e:?}")).collect();
        ext.sort();
        let mut consumers: Vec<String> = g.consumers.iter().map(|e| format!("{e:?}")).collect();
        consumers.sort();
        let mut notes = g.notes.clone();
        notes.sort();
        let mut unreadable = g.unreadable.clone();
        unreadable.sort();
        json!({ "objects": objects, "edges": edges, "external": ext, "consumers": consumers,
            "notes": notes, "unreadable": unreadable, "scanned": g.scanned, "truncated": g.truncated })
    }

    /// The chunks are the whole graph: tokens (the definition tier), the engine's dependencies in
    /// batches of a few objects, and the engine refusing them.
    #[tokio::test]
    async fn the_chunks_add_up_to_the_whole_graph() {
        let scope = LineageScope::default();
        let mut f = Fake::writer();
        f.definition(LINEAGE);
        let whole = graph(&mut f).await;
        let (parts, stages) = chunked(&mut f, &scope).await;
        assert_eq!(said(&parts), said(&whole));
        // The definition tier reads the DAX as tokens in the structure: nothing for the engine.
        assert_eq!(stages, ["structure@0", "consumers@0"]);

        let rows = |f: &mut Fake| {
            f.dax_answer(
                "INFO.CALCDEPENDENCY()",
                vec![calc_rows(vec![
                    json!([
                        "db",
                        "MEASURE",
                        "Sales",
                        "Total Sales",
                        "SUM(Sales[Amount])",
                        "COLUMN",
                        "Sales",
                        "Amount"
                    ]),
                    json!([
                        "db",
                        "MEASURE",
                        "Sales",
                        "Total Sales",
                        "SUM(Sales[Amount])",
                        "TABLE",
                        "Customer",
                        "Customer"
                    ]),
                    json!([
                        "db",
                        "CALC_COLUMN",
                        "Sales",
                        "Margin",
                        "[Amount] - 1",
                        "COLUMN",
                        "Sales",
                        "Amount"
                    ]),
                ])],
            );
        };
        let mut f = info_writer();
        rows(&mut f);
        let whole = graph(&mut f).await;
        let mut f = info_writer();
        rows(&mut f);
        let (parts, stages) = chunked(&mut f, &scope).await;
        assert_eq!(said(&parts), said(&whole), "{stages:?}");
        let deps = stages
            .iter()
            .filter(|s| s.starts_with("dependencies"))
            .count();
        assert!(deps >= 2, "batches of a few objects: {stages:?}");
        assert_eq!(stages.first().map(String::as_str), Some("structure@0"));
        assert_eq!(stages.last().map(String::as_str), Some("consumers@0"));
        // No call reads the whole model: each asks for its own objects.
        assert!(
            f.dax_log
                .iter()
                .filter(|q| q.contains("CALCDEPENDENCY"))
                .all(|q| q.contains("FILTER(INFO.CALCDEPENDENCY()")),
            "{:?}",
            f.dax_log
        );

        // The engine refusing them: every batch falls back to the tokens, as the whole read does.
        let mut f = info_writer();
        f.dax_refuse("INFO.CALCDEPENDENCY()");
        let whole = graph(&mut f).await;
        let mut f = info_writer();
        f.dax_refuse("INFO.CALCDEPENDENCY()");
        let (parts, _) = chunked(&mut f, &scope).await;
        assert_eq!(said(&parts), said(&whole));
        assert!(parts.notes.contains(&NO_EXPRESSIONS.to_string()));
    }

    /// A chunk the host gives up on is skipped without a read, and the next one is still said.
    #[tokio::test]
    async fn a_skipped_chunk_says_the_next_without_reading() {
        TEST_CHUNK.store(2, std::sync::atomic::Ordering::Relaxed);
        let mut f = info_writer();
        let g = lineage_in(
            &mut f,
            &LineageScope {
                step: Some(LineageStep {
                    stage: STAGE_DEPENDENCIES.into(),
                    offset: 0,
                    len: 2,
                    total: 3,
                    skip: true,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            !f.dax_log.iter().any(|q| q.contains("CALCDEPENDENCY")),
            "nothing was asked"
        );
        let next = g.next.unwrap();
        assert_eq!((next.stage.as_str(), next.offset), (STAGE_DEPENDENCIES, 2));
        assert!(g.edges.is_empty() && g.objects.is_empty());
    }

    /// The statistics tier has no dependencies to chunk: the structure, then the reports.
    #[tokio::test]
    async fn the_statistics_tier_has_no_dependency_chunks() {
        let mut f = Fake::new();
        f.dax_answer(
            "COLUMNSTATISTICS()",
            vec![rowset(
                &[
                    "[Table Name]",
                    "[Column Name]",
                    "[Min]",
                    "[Max]",
                    "[Cardinality]",
                    "[Max Length]",
                ],
                vec![json!(["Sales", "Amount", null, null, 3, 8])],
            )],
        );
        let whole = graph(&mut f).await;
        let (parts, stages) = chunked(&mut f, &LineageScope::default()).await;
        assert_eq!(said(&parts), said(&whole));
        assert_eq!(stages, ["structure@0", "consumers@0"]);
    }
}

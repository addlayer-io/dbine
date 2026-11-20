//! What an object of the model uses (`Session::references`): the other
//! direction of [`crate::dependencies`].
//!
//! - DAX (measures, calculated columns and tables, calculation items, RLS
//!   filters, functions): with write permission on a model on capacity,
//!   `INFO.CALCDEPENDENCY()` filtered on the target's objects
//!   (`Confidence::Confirmed`); otherwise the DAX tokens of its
//!   expressions, resolved against the model's tables, columns, measures
//!   and functions ([`crate::dax::lexer`], `Confidence::Probable`).
//! - A table's partitions: their Power Query (M) names the sources outside
//!   the model ([`m::sources`], `DependencyReport::external`) and the
//!   shared expressions, parameters and tables it reads (`Probable`).
//!   `INFO.CALCDEPENDENCY` doesn't cover M.
//! - A table's relationships: the tables on their one side
//!   (`Relation::ForeignKey`, `Confirmed`).
//! - A shared expression or parameter: its own M, as a partition's.
//!
//! Hierarchies, relationships and perspectives are left to the generic
//! scan (`None`).

pub mod m;

use crate::dax::lexer::{code, unquote, Token, TokenKind};
use crate::dax::quote::{column_ref, quote_table, string as dax_string};
use crate::dependencies::{dedupe_and_sort, mentions, NO_EXPRESSIONS, NO_METADATA};
use crate::meta::info::{calc_dependencies_query, CalcDependency};
use crate::meta::service::{session_model, Service};
use crate::meta::{relationship_name, same, ModelMeta, TableKind, Tier};
use crate::{Ctx, CALCULATION_GROUP, EXPRESSION, FUNCTION, MEASURE, ROLE, TABLE};
use dbine_driver::dependencies::ExternalRef;
use dbine_driver::{
    Confidence, DependencyReport, DependencyTarget, Dependent, Mention, Relation, Result,
};

/// What the report says when a table's partitions hide their M.
pub const NO_PARTITIONS: &str = "Sin permiso de escritura en el modelo no se ve el código M de las particiones: no se sabe de qué orígenes lee la tabla.";

/// What `target` uses; `None` for a kind the generic scan answers.
pub async fn references(
    cx: &mut Ctx,
    target: &DependencyTarget,
) -> Result<Option<DependencyReport>> {
    references_in(cx, target).await
}

pub(crate) async fn references_in<S: Service>(
    s: &mut S,
    target: &DependencyTarget,
) -> Result<Option<DependencyReport>> {
    let t = &target.object;
    if !matches!(
        t.kind.as_str(),
        TABLE | CALCULATION_GROUP | MEASURE | ROLE | FUNCTION | EXPRESSION
    ) {
        return Ok(None);
    }
    let model = session_model(s)?;
    let meta = crate::meta::model_for(s, &model).await?;
    let mut report = DependencyReport::default();
    if meta.tier == Tier::Stats {
        report.note = Some(NO_METADATA.into());
        return Ok(Some(report));
    }
    let codes = codes(&meta, target);
    report.scanned = codes.iter().filter(|c| c.text.is_some()).count() as u32;
    report.items.extend(relationships(&meta, target));

    // DAX: the engine's dependencies where it gives them, else the tokens.
    let lexed: Vec<(usize, Use, usize)> = codes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.lang == Lang::Dax)
        .flat_map(|(k, c)| {
            c.text
                .as_deref()
                .map(|e| dax_uses(e, c.home.as_deref(), &meta))
                .unwrap_or_default()
                .into_iter()
                .map(move |(u, at)| (k, u, at))
        })
        .filter(|(_, u, _)| !u.is_target(target))
        .collect();
    let mut confirmed: Option<Vec<Use>> = None;
    let dax = codes.iter().any(|c| c.lang == Lang::Dax);
    if meta.tier == Tier::Info && model.write_permission && dax {
        if let Some(q) = calc_query(target) {
            match calc_dependencies_query(s, &model, &q).await {
                Ok(rows) => {
                    let uses: Vec<Use> = rows
                        .iter()
                        .filter(|r| is_target_row(r, target))
                        .filter_map(calc_use)
                        .filter(|u| !u.is_target(target))
                        .collect();
                    if !uses.is_empty() {
                        confirmed = Some(uses);
                    }
                }
                Err(e) if crate::meta::is_fatal(&e) => return Err(e),
                Err(_) => {}
            }
        }
    }
    let (uses, confidence) = match confirmed {
        Some(uses) => (uses, Confidence::Confirmed),
        None => {
            let hidden: Vec<String> = codes
                .iter()
                .filter(|c| c.lang == Lang::Dax && c.text.is_none())
                .map(|c| c.label.clone())
                .collect();
            if !hidden.is_empty() {
                report.note = Some(NO_EXPRESSIONS.into());
                report.unreadable.extend(hidden);
            }
            let uses = lexed.iter().map(|(_, u, _)| u.clone()).collect();
            (uses, Confidence::Probable)
        }
    };
    let mut objects: Vec<(&str, String)> = Vec::new();
    for u in &uses {
        if !objects.contains(&u.object()) {
            objects.push(u.object());
        }
    }
    for o in objects {
        let of: Vec<Use> = uses.iter().filter(|u| u.object() == o).cloned().collect();
        let at: Vec<(usize, usize)> = lexed
            .iter()
            .filter(|(_, u, _)| u.object() == o)
            .map(|&(k, _, at)| (k, at))
            .collect();
        report.items.extend(group(
            &meta,
            &dedupe_uses(of),
            confidence,
            lines(&codes, &at),
        ));
    }

    // M: the sources outside the model and the shared expressions it reads.
    let shared: Vec<(String, String)> = meta
        .expressions
        .iter()
        .filter_map(|e| Some((e.name.clone(), e.expression.clone()?)))
        .collect();
    let env = m::Env { shared: &shared };
    let wanted: Vec<String> = meta
        .expressions
        .iter()
        .map(|e| e.name.clone())
        .chain(meta.tables.iter().map(|t| t.name.clone()))
        .collect();
    for c in &codes {
        let Some(text) = c.text.as_deref() else {
            continue;
        };
        match c.lang {
            Lang::Dax => {}
            Lang::Sql => add_external(&mut report.external, m::native_source(text), Vec::new()),
            Lang::M => {
                for src in m::sources(text, &env) {
                    add_external(&mut report.external, src.external, at_lines(text, &src.at));
                }
                for (name, at) in m::names(text, &wanted) {
                    let kind = if meta.expressions.iter().any(|e| e.name == name) {
                        EXPRESSION
                    } else {
                        table_kind(&meta, &name)
                    };
                    if kind == t.kind && same(&name, &t.name) {
                        continue;
                    }
                    report.items.push(Dependent {
                        kind: kind.into(),
                        schema: None,
                        name,
                        parent: None,
                        relation: Relation::Code,
                        confidence: Confidence::Probable,
                        detail: None,
                        mentions: at_lines(text, &at),
                    });
                }
            }
        }
    }
    if t.kind == TABLE && partitions_hidden(&meta, &t.name) && report.note.is_none() {
        report.note = Some(NO_PARTITIONS.into());
    }
    merge(&mut report.items);
    dedupe_and_sort(&mut report.items);
    Ok(Some(report))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Dax,
    M,
    /// A query partition's SQL.
    Sql,
}

/// One expression of the target.
#[derive(Debug, Clone)]
struct Code {
    lang: Lang,
    /// How `unreadable` names it.
    label: String,
    /// The table whose row context a bare `[c]` reads.
    home: Option<String>,
    /// `None`: hidden without write permission.
    text: Option<String>,
}

/// The target's expressions.
fn codes(meta: &ModelMeta, target: &DependencyTarget) -> Vec<Code> {
    let t = &target.object;
    let dax = |label: String, home: Option<&str>, text: Option<&String>| Code {
        lang: Lang::Dax,
        label,
        home: home.map(str::to_string),
        text: text.cloned(),
    };
    let mut out = Vec::new();
    match t.kind.as_str() {
        MEASURE => {
            if let Some(m) = meta.measure(&t.name) {
                out.push(dax(
                    column_ref(&m.table, &m.name),
                    None,
                    m.expression.as_ref(),
                ));
            }
        }
        TABLE => {
            let Some(table) = meta.table(&t.name) else {
                return out;
            };
            if target.column.is_none() {
                if let Some(e) = &table.expression {
                    out.push(dax(quote_table(&table.name), None, Some(e)));
                }
            }
            for c in table.columns.iter().filter(|c| c.expression.is_some()) {
                if target.column.as_deref().is_some_and(|x| !same(x, &c.name)) {
                    continue;
                }
                out.push(dax(
                    column_ref(&table.name, &c.name),
                    Some(&table.name),
                    c.expression.as_ref(),
                ));
            }
            if target.column.is_some() || table.kind == TableKind::Calculated {
                return out;
            }
            for p in &table.partitions {
                let lang = match p.source_kind.as_deref() {
                    Some("m") | None => Lang::M,
                    Some("query") => Lang::Sql,
                    _ => continue,
                };
                out.push(Code {
                    lang,
                    label: p.name.clone(),
                    home: None,
                    text: p.expression.clone(),
                });
            }
        }
        CALCULATION_GROUP => {
            if let Some(g) = meta
                .calculation_groups
                .iter()
                .find(|g| same(&g.table, &t.name))
            {
                for i in &g.items {
                    out.push(dax(
                        column_ref(&g.table, &i.name),
                        None,
                        i.expression.as_ref(),
                    ));
                }
            }
        }
        ROLE => {
            if let Some(r) = meta.roles.iter().find(|r| same(&r.name, &t.name)) {
                for (table, filter) in &r.table_filters {
                    out.push(dax(quote_table(table), Some(table), Some(filter)));
                }
            }
        }
        FUNCTION => {
            if let Some(f) = meta.functions.iter().find(|f| same(&f.name, &t.name)) {
                out.push(dax(f.name.clone(), None, f.expression.as_ref()));
            }
        }
        EXPRESSION => {
            if let Some(e) = meta.expressions.iter().find(|e| same(&e.name, &t.name)) {
                out.push(Code {
                    lang: Lang::M,
                    label: e.name.clone(),
                    home: None,
                    text: e.expression.clone(),
                });
            }
        }
        _ => {}
    }
    out
}

/// A data table whose partitions don't show their M (a reader on the
/// `INFO` tier without write permission).
fn partitions_hidden(meta: &ModelMeta, table: &str) -> bool {
    meta.table(table).is_some_and(|t| {
        t.kind == TableKind::Data
            && (t.partitions.is_empty()
                || t.partitions.iter().any(|p| {
                    matches!(p.source_kind.as_deref(), Some("m" | "query") | None)
                        && p.expression.is_none()
                }))
    })
}

/// The tables the target's relationships point at (its many side → the
/// one side), as foreign keys are.
fn relationships(meta: &ModelMeta, target: &DependencyTarget) -> Vec<Dependent> {
    let t = &target.object;
    if t.kind != TABLE {
        return Vec::new();
    }
    meta.model_relationships()
        .filter(|r| same(&r.from_table, &t.name) && !same(&r.to_table, &t.name))
        .filter(|r| {
            target
                .column
                .as_deref()
                .is_none_or(|c| same(&r.from_column, c))
        })
        .map(|r| {
            let mut detail = relationship_name(r);
            if !r.active {
                detail.push_str(" (inactiva)");
            }
            Dependent {
                kind: table_kind(meta, &r.to_table).into(),
                schema: None,
                name: r.to_table.clone(),
                parent: None,
                relation: Relation::ForeignKey,
                confidence: Confidence::Confirmed,
                detail: Some(detail),
                mentions: Vec::new(),
            }
        })
        .collect()
}

/// What an expression names.
#[derive(Debug, Clone, PartialEq)]
enum Use {
    Table(String),
    Column(String, String),
    Measure(String),
    Function(String),
}

impl Use {
    /// The object it is in the explorer: a column is its table.
    fn object(&self) -> (&'static str, String) {
        match self {
            Use::Table(t) | Use::Column(t, _) => (TABLE, t.to_lowercase()),
            Use::Measure(m) => (MEASURE, m.to_lowercase()),
            Use::Function(f) => (FUNCTION, f.to_lowercase()),
        }
    }

    /// The target itself (a calculated column on its own table, a
    /// measure that names itself).
    fn is_target(&self, target: &DependencyTarget) -> bool {
        let t = &target.object;
        match self {
            Use::Table(x) | Use::Column(x, _) => {
                matches!(t.kind.as_str(), TABLE | CALCULATION_GROUP) && same(x, &t.name)
            }
            Use::Measure(x) => t.kind == MEASURE && same(x, &t.name),
            Use::Function(x) => t.kind == FUNCTION && same(x, &t.name),
        }
    }
}

/// Each use once, in order.
fn dedupe_uses(uses: Vec<Use>) -> Vec<Use> {
    let mut out: Vec<Use> = Vec::new();
    for u in uses {
        let dup = out.iter().any(|o| match (o, &u) {
            (Use::Column(a, b), Use::Column(c, d)) => same(a, c) && same(b, d),
            _ => o.object() == u.object() && !matches!(u, Use::Column(..)),
        });
        if !dup {
            out.push(u);
        }
    }
    out
}

/// The kind of the model's table `name`: a calculation group's table is
/// the group.
fn table_kind(meta: &ModelMeta, name: &str) -> &'static str {
    if meta.calculation_groups.iter().any(|g| same(&g.table, name)) {
        CALCULATION_GROUP
    } else {
        TABLE
    }
}

/// The dependents `uses` (one object's) make: a table with the columns
/// used in `detail`, a measure with its home table, a function.
fn group(
    meta: &ModelMeta,
    uses: &[Use],
    confidence: Confidence,
    mentions: Vec<Mention>,
) -> Option<Dependent> {
    let first = uses.first()?;
    let (kind, name, parent, detail) = match first {
        Use::Table(t) | Use::Column(t, _) => {
            let name = meta.table(t).map_or(t.clone(), |x| x.name.clone());
            let columns: Vec<String> = uses
                .iter()
                .filter_map(|u| match u {
                    Use::Column(_, c) => Some(column_ref(&name, c)),
                    _ => None,
                })
                .collect();
            let detail = (!columns.is_empty()).then(|| columns.join(", "));
            (table_kind(meta, &name), name, None, detail)
        }
        Use::Measure(m) => match meta.measure(m) {
            Some(x) => (MEASURE, x.name.clone(), Some(x.table.clone()), None),
            None => (MEASURE, m.clone(), None, None),
        },
        Use::Function(f) => (FUNCTION, f.clone(), None, None),
    };
    Some(Dependent {
        kind: kind.into(),
        schema: None,
        name,
        parent,
        relation: Relation::Code,
        confidence,
        detail,
        mentions,
    })
}

/// The names a DAX expression uses, each with its offset: `'T'` and `T`
/// (not a call), `'T'[c]` (a column, or the table's measure), a bare
/// `[x]` (a measure, else a column of the home table, else of the one
/// table that has it), and the model's functions called.
fn dax_uses(expr: &str, home: Option<&str>, meta: &ModelMeta) -> Vec<(Use, usize)> {
    let toks = code(expr);
    let table_at = |t: Option<&Token<'_>>| {
        t.and_then(|t| match t.kind {
            TokenKind::Table => meta.table(&unquote(t.text)),
            TokenKind::Word => meta.table(t.text),
            _ => None,
        })
    };
    let has_column = |t: &crate::meta::Table, c: &str| t.columns.iter().any(|x| same(&x.name, c));
    let mut out = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        let next = toks.get(i + 1);
        match tok.kind {
            TokenKind::Word if next.is_some_and(|n| n.is_punct("(")) => {
                if let Some(f) = meta.functions.iter().find(|f| same(&f.name, tok.text)) {
                    out.push((Use::Function(f.name.clone()), tok.start));
                }
            }
            TokenKind::Table | TokenKind::Word => {
                let Some(table) = table_at(Some(tok)) else {
                    continue;
                };
                let u = match next.filter(|n| n.kind == TokenKind::Ref) {
                    Some(r) => {
                        let c = unquote(r.text);
                        let measure = meta
                            .measures
                            .iter()
                            .find(|m| same(&m.name, &c) && same(&m.table, &table.name));
                        match measure {
                            Some(m) if !has_column(table, &c) => Use::Measure(m.name.clone()),
                            _ => Use::Column(table.name.clone(), c),
                        }
                    }
                    None => Use::Table(table.name.clone()),
                };
                out.push((u, tok.start));
            }
            TokenKind::Ref => {
                if i > 0 && table_at(toks.get(i - 1)).is_some() {
                    continue;
                }
                let c = unquote(tok.text);
                let u = if let Some(m) = meta.measure(&c) {
                    Use::Measure(m.name.clone())
                } else if let Some(h) = home
                    .and_then(|h| meta.table(h))
                    .filter(|h| has_column(h, &c))
                {
                    Use::Column(h.name.clone(), c)
                } else {
                    let owners: Vec<&crate::meta::Table> =
                        meta.tables.iter().filter(|t| has_column(t, &c)).collect();
                    match owners.as_slice() {
                        [only] => Use::Column(only.name.clone(), c),
                        _ => continue,
                    }
                };
                out.push((u, tok.start));
            }
            _ => {}
        }
    }
    out
}

/// The `INFO.CALCDEPENDENCY()` rows of the target's own objects.
fn calc_query(target: &DependencyTarget) -> Option<String> {
    let t = &target.object;
    let n = dax_string(&t.name);
    let filter = match (t.kind.as_str(), target.column.as_deref()) {
        (MEASURE, _) => format!("[OBJECT_TYPE] = \"MEASURE\" && [OBJECT] = {n}"),
        (TABLE, None) => format!(
            "([TABLE] = {n} && [OBJECT_TYPE] IN {{\"CALC_COLUMN\", \"CALC_TABLE\"}}) || ([OBJECT] = {n} && [OBJECT_TYPE] = \"CALC_TABLE\")"
        ),
        (TABLE, Some(c)) => format!(
            "[OBJECT_TYPE] = \"CALC_COLUMN\" && [TABLE] = {n} && [OBJECT] = {}",
            dax_string(c)
        ),
        (CALCULATION_GROUP, _) => {
            format!("[OBJECT_TYPE] = \"CALCULATION_ITEM\" && [TABLE] = {n}")
        }
        (ROLE, _) => format!("[OBJECT_TYPE] = \"ROWS_ALLOWED\" && [OBJECT] = {n}"),
        (FUNCTION, _) => {
            format!("CONTAINSSTRING([OBJECT_TYPE], \"FUNCTION\") && [OBJECT] = {n}")
        }
        _ => return None,
    };
    Some(format!("EVALUATE FILTER(INFO.CALCDEPENDENCY(), {filter})"))
}

/// The row is one of the target's objects (what [`calc_query`] asks).
fn is_target_row(r: &CalcDependency, target: &DependencyTarget) -> bool {
    let t = &target.object;
    let ot = r.object_type.to_ascii_uppercase();
    let table = r.table.as_deref().unwrap_or("");
    match (t.kind.as_str(), target.column.as_deref()) {
        (MEASURE, _) => ot == "MEASURE" && same(&r.object, &t.name),
        (TABLE, None) => {
            (matches!(ot.as_str(), "CALC_COLUMN" | "CALC_TABLE") && same(table, &t.name))
                || (ot == "CALC_TABLE" && same(&r.object, &t.name))
        }
        (TABLE, Some(c)) => ot == "CALC_COLUMN" && same(table, &t.name) && same(&r.object, c),
        (CALCULATION_GROUP, _) => ot == "CALCULATION_ITEM" && same(table, &t.name),
        (ROLE, _) => ot == "ROWS_ALLOWED" && same(&r.object, &t.name),
        (FUNCTION, _) => ot.contains("FUNCTION") && same(&r.object, &t.name),
        _ => false,
    }
}

/// What a row's referenced object is.
fn calc_use(r: &CalcDependency) -> Option<Use> {
    let table = r.referenced_table.clone().filter(|t| !t.is_empty());
    let rt = r.referenced_object_type.to_ascii_uppercase();
    Some(match rt.as_str() {
        "TABLE" | "CALC_TABLE" => Use::Table(r.referenced_object.clone()),
        "COLUMN" | "CALC_COLUMN" => Use::Column(table?, r.referenced_object.clone()),
        "MEASURE" => Use::Measure(r.referenced_object.clone()),
        x if x.contains("FUNCTION") => Use::Function(r.referenced_object.clone()),
        // A relationship (USERELATIONSHIP), a hierarchy…: its table.
        _ => Use::Table(table?),
    })
}

/// The lines of each code's offsets (`(code, offset)`), code by code.
fn lines(codes: &[Code], at: &[(usize, usize)]) -> Vec<Mention> {
    let mut out = Vec::new();
    for (k, c) in codes.iter().enumerate() {
        let Some(text) = c.text.as_deref() else {
            continue;
        };
        let offsets: Vec<usize> = at
            .iter()
            .filter(|(i, _)| *i == k)
            .map(|&(_, o)| o)
            .collect();
        out.extend(at_lines(text, &offsets));
    }
    out.truncate(dbine_driver::dependencies::MAX_MENTIONS);
    out
}

fn at_lines(text: &str, offsets: &[usize]) -> Vec<Mention> {
    let mut o = offsets.to_vec();
    o.sort_unstable();
    mentions(text, &o)
}

fn add_external(out: &mut Vec<ExternalRef>, mut x: ExternalRef, mut at: Vec<Mention>) {
    match out.iter_mut().find(|e| {
        (
            &e.system,
            &e.server,
            &e.database,
            &e.schema,
            &e.name,
            &e.detail,
        ) == (
            &x.system,
            &x.server,
            &x.database,
            &x.schema,
            &x.name,
            &x.detail,
        )
    }) {
        Some(e) => {
            e.mentions.append(&mut at);
            e.mentions
                .truncate(dbine_driver::dependencies::MAX_MENTIONS);
        }
        None => {
            x.mentions = at;
            out.push(x);
        }
    }
}

/// One item per object: the same table named from several expressions
/// keeps the first's detail and every mention.
fn merge(items: &mut Vec<Dependent>) {
    let mut out: Vec<Dependent> = Vec::new();
    for d in items.drain(..) {
        let same_one = out.iter_mut().find(|o| {
            o.kind == d.kind
                && same(&o.name, &d.name)
                && o.relation == d.relation
                && o.relation != Relation::ForeignKey
        });
        match same_one {
            Some(o) => {
                o.confidence = o.confidence.min(d.confidence);
                if o.detail.is_none() {
                    o.detail = d.detail;
                }
                for m in d.mentions {
                    if !o.mentions.contains(&m)
                        && o.mentions.len() < dbine_driver::dependencies::MAX_MENTIONS
                    {
                        o.mentions.push(m);
                    }
                }
            }
            None => out.push(d),
        }
    }
    *items = out;
}

#[cfg(test)]
mod tests;

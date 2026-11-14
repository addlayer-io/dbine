//! "Ver dependencias…" (spec §4.6 `dependents`, F12). Owned by part C.
//!
//! - Tier A with write permission: `INFO.CALCDEPENDENCY()` filtered on the
//!   target (`Relation::Code`, `Confidence::Confirmed`): measures,
//!   calculated columns and tables, calculation items, RLS filters.
//! - Tier B: a DAX-aware mention scan over the TMSL expressions with
//!   [`crate::dax::lexer`] (`Confidence::Probable`).
//! - Relationships on a column: `Relation::ForeignKey`.
//! - Tier C: nothing readable; the report says so.
//!
//! The generic scan can't do this (F12): it reads only routines, and its
//! SQL lexer takes `'Sales'` for a string and `'T'[c]` for nothing. The
//! scan here reads DAX tokens: `'Sales'` and `Sales` name a table, `[c]`
//! after it is its column, a bare `[c]` is a column of the expression's own
//! table (a calculated column, an RLS filter) or, elsewhere, something to
//! review (a row context or a measure of the same name). Strings and
//! comments never count: DAX has no dynamic code.
//!
//! Hierarchies whose levels use a column are listed as `Relation::Index`
//! (they're the model's own structure on the column).

use crate::dax::lexer::{tokenize, unquote, Token, TokenKind};
use crate::dax::quote::{column_ref, quote_column, quote_table};
use crate::meta::info::{calc_dependencies_of, CalcDependency};
use crate::meta::service::{session_model, Service};
use crate::meta::{hierarchy_name, relationship_name, same, ModelMeta, Tier};
use crate::{Ctx, CALCULATION_GROUP, FUNCTION, HIERARCHY, MEASURE, ROLE, TABLE};
use dbine_driver::dependencies::{LINE_CHARS, MAX_MENTIONS};
use dbine_driver::{
    Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent, Mention, Relation,
    Result,
};

pub async fn dependents(
    cx: &mut Ctx,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let _ = scan;
    dependents_in(cx, target).await
}

/// What the report says on the statistics tier.
pub const NO_METADATA: &str = "Con los permisos de esta conexión solo se leen tablas y columnas: ni relaciones ni expresiones DAX, así que no se pueden buscar dependencias.";

/// What it says when expressions are hidden.
pub const NO_EXPRESSIONS: &str = "Sin permiso de escritura en el modelo no se ven las expresiones DAX: se muestran solo las relaciones y las jerarquías.";

pub(crate) async fn dependents_in<S: Service>(
    s: &mut S,
    target: &DependencyTarget,
) -> Result<DependencyReport> {
    let m = session_model(s)?;
    let meta = crate::meta::model_for(s, &m).await?;
    let mut report = DependencyReport::default();
    if meta.tier == Tier::Stats {
        report.note = Some(NO_METADATA.into());
        return Ok(report);
    }
    report.items.extend(structural(&meta, target));
    let sources = sources(&meta);
    report.scanned = sources.len() as u32;
    let mut done = false;
    if meta.tier == Tier::Info && m.write_permission {
        match calc_dependencies_of(s, &m).await {
            Ok(rows) => {
                report.items.extend(from_calc(&meta, &rows, target));
                done = true;
            }
            Err(e) if crate::meta::is_fatal(&e) => return Err(e),
            Err(_) => {}
        }
    }
    if !done {
        let mut unreadable = Vec::new();
        for src in &sources {
            let Some(expr) = &src.expression else {
                unreadable.push(src.label());
                continue;
            };
            if src.is_target(target) {
                continue;
            }
            if let Some((confidence, mentions)) =
                find_mentions(expr, src.home.as_deref(), target, &meta)
            {
                report
                    .items
                    .push(src.dependent(Relation::Code, confidence, mentions));
            }
        }
        if !unreadable.is_empty() {
            report.note = Some(NO_EXPRESSIONS.into());
            report.unreadable = unreadable;
        }
    }
    dedupe_and_sort(&mut report.items);
    Ok(report)
}

/// An expression of the model and the object it belongs to.
struct Source {
    kind: &'static str,
    name: String,
    parent: Option<String>,
    detail: Option<String>,
    /// The table whose row context a bare `[c]` reads.
    home: Option<String>,
    expression: Option<String>,
}

impl Source {
    fn label(&self) -> String {
        match &self.parent {
            Some(p) => format!("{}{}", quote_table(p), quote_column(&self.name)),
            None => self.name.clone(),
        }
    }

    /// It is the target (a measure doesn't depend on itself).
    fn is_target(&self, t: &DependencyTarget) -> bool {
        same(&self.name, &t.object.name)
            && (self.kind == t.object.kind
                || (self.kind == TABLE && t.object.kind == CALCULATION_GROUP))
            && (t.column.is_none() || self.detail.is_none())
    }

    fn dependent(
        &self,
        relation: Relation,
        confidence: Confidence,
        mentions: Vec<Mention>,
    ) -> Dependent {
        Dependent {
            kind: self.kind.into(),
            schema: None,
            name: self.name.clone(),
            parent: self.parent.clone(),
            relation,
            confidence,
            detail: self.detail.clone(),
            mentions,
        }
    }
}

fn sources(meta: &ModelMeta) -> Vec<Source> {
    let mut out = Vec::new();
    for m in &meta.measures {
        out.push(Source {
            kind: MEASURE,
            name: m.name.clone(),
            parent: Some(m.table.clone()),
            detail: None,
            home: None,
            expression: m.expression.clone(),
        });
    }
    for t in &meta.tables {
        if let Some(e) = &t.expression {
            out.push(Source {
                kind: TABLE,
                name: t.name.clone(),
                parent: None,
                detail: Some("tabla calculada".into()),
                home: None,
                expression: Some(e.clone()),
            });
        }
        for c in t.columns.iter().filter(|c| c.expression.is_some()) {
            out.push(Source {
                kind: TABLE,
                name: t.name.clone(),
                parent: None,
                detail: Some(format!(
                    "columna calculada {}",
                    column_ref(&t.name, &c.name)
                )),
                home: Some(t.name.clone()),
                expression: c.expression.clone(),
            });
        }
    }
    for g in &meta.calculation_groups {
        for i in &g.items {
            out.push(Source {
                kind: CALCULATION_GROUP,
                name: g.table.clone(),
                parent: None,
                detail: Some(format!("elemento {}", quote_column(&i.name))),
                home: None,
                expression: i.expression.clone(),
            });
        }
    }
    for r in &meta.roles {
        for (t, f) in &r.table_filters {
            out.push(Source {
                kind: ROLE,
                name: r.name.clone(),
                parent: None,
                detail: Some(format!("filtro de {}", quote_table(t))),
                home: Some(t.clone()),
                expression: Some(f.clone()),
            });
        }
    }
    for f in &meta.functions {
        out.push(Source {
            kind: FUNCTION,
            name: f.name.clone(),
            parent: None,
            detail: None,
            home: None,
            expression: f.expression.clone(),
        });
    }
    out
}

/// Relationships on the target and hierarchies over its column.
fn structural(meta: &ModelMeta, target: &DependencyTarget) -> Vec<Dependent> {
    let t = &target.object;
    if !matches!(t.kind.as_str(), TABLE | CALCULATION_GROUP) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let holder = |table: &str, relation, detail: String| Dependent {
        kind: TABLE.into(),
        schema: None,
        name: table.to_string(),
        parent: None,
        relation,
        confidence: Confidence::Confirmed,
        detail: Some(detail),
        mentions: Vec::new(),
    };
    for r in meta.model_relationships() {
        let mut detail = relationship_name(r);
        if !r.active {
            detail.push_str(" (inactiva)");
        }
        let hit = match target.column.as_deref() {
            None => same(&r.to_table, &t.name) && !same(&r.from_table, &t.name),
            Some(c) => {
                (same(&r.to_table, &t.name) && same(&r.to_column, c))
                    || (same(&r.from_table, &t.name) && same(&r.from_column, c))
            }
        };
        if hit {
            out.push(holder(&r.from_table, Relation::ForeignKey, detail));
        }
    }
    if let Some(c) = target.column.as_deref() {
        for h in meta.hierarchies.iter().filter(|h| same(&h.table, &t.name)) {
            if let Some(l) = h.levels.iter().find(|l| same(&l.column, c)) {
                out.push(Dependent {
                    kind: HIERARCHY.into(),
                    schema: None,
                    name: hierarchy_name(h),
                    parent: None,
                    relation: Relation::Index,
                    confidence: Confidence::Confirmed,
                    detail: Some(format!("nivel «{}»", l.name)),
                    mentions: Vec::new(),
                });
            }
        }
    }
    out
}

/// `INFO.CALCDEPENDENCY` rows on the target, as dependents.
fn from_calc(
    meta: &ModelMeta,
    rows: &[CalcDependency],
    target: &DependencyTarget,
) -> Vec<Dependent> {
    let t = &target.object;
    let mut out = Vec::new();
    for r in rows {
        let rt = r.referenced_object_type.to_ascii_uppercase();
        let rtable = r.referenced_table.as_deref().unwrap_or("");
        let hit = match (t.kind.as_str(), target.column.as_deref()) {
            (TABLE | CALCULATION_GROUP, None) => {
                (matches!(rt.as_str(), "TABLE" | "CALC_TABLE")
                    && same(&r.referenced_object, &t.name))
                    || (matches!(rt.as_str(), "COLUMN" | "CALC_COLUMN") && same(rtable, &t.name))
            }
            (TABLE | CALCULATION_GROUP, Some(c)) => {
                matches!(rt.as_str(), "COLUMN" | "CALC_COLUMN")
                    && same(rtable, &t.name)
                    && same(&r.referenced_object, c)
            }
            (MEASURE, _) => rt == "MEASURE" && same(&r.referenced_object, &t.name),
            (FUNCTION, _) => rt.contains("FUNCTION") && same(&r.referenced_object, &t.name),
            _ => false,
        };
        if !hit {
            continue;
        }
        let table = r.table.clone().unwrap_or_default();
        let (kind, name, parent, detail, home): (
            &str,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = match r.object_type.to_ascii_uppercase().as_str() {
            "MEASURE" => (MEASURE, r.object.clone(), Some(table.clone()), None, None),
            "CALC_COLUMN" => (
                TABLE,
                table.clone(),
                None,
                Some(format!(
                    "columna calculada {}",
                    column_ref(&table, &r.object)
                )),
                Some(table.clone()),
            ),
            "CALC_TABLE" => (
                TABLE,
                r.object.clone(),
                None,
                Some("tabla calculada".into()),
                None,
            ),
            "CALCULATION_ITEM" => (
                CALCULATION_GROUP,
                table.clone(),
                None,
                Some(format!("elemento {}", quote_column(&r.object))),
                None,
            ),
            "ROWS_ALLOWED" => {
                if meta.roles.iter().any(|x| same(&x.name, &r.object)) {
                    (
                        ROLE,
                        r.object.clone(),
                        None,
                        Some(format!("filtro de {}", quote_table(&table))),
                        Some(table.clone()),
                    )
                } else {
                    (
                        TABLE,
                        table.clone(),
                        None,
                        Some("filtro de seguridad (RLS)".into()),
                        Some(table.clone()),
                    )
                }
            }
            ot if ot.contains("FUNCTION") => (FUNCTION, r.object.clone(), None, None, None),
            // Relationships, hierarchies, sort-by columns, partitions:
            // the structure part reads them from the model.
            _ => continue,
        };
        // Not the target itself.
        if kind == t.kind && same(&name, &t.name) && detail.is_none() {
            continue;
        }
        let mentions = r
            .expression
            .as_deref()
            .and_then(|e| find_mentions(e, home.as_deref(), target, meta))
            .map(|(_, m)| m)
            .unwrap_or_default();
        out.push(Dependent {
            kind: kind.into(),
            schema: None,
            name,
            parent,
            relation: Relation::Code,
            confidence: Confidence::Confirmed,
            detail,
            mentions,
        });
    }
    out
}

/// The tokens that aren't spaces or comments.
fn code(expr: &str) -> Vec<Token<'_>> {
    tokenize(expr)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Space | TokenKind::Comment))
        .collect()
}

/// A token that names a table: `'Sales'` or `Sales`.
fn table_of(t: &Token<'_>) -> Option<String> {
    match t.kind {
        TokenKind::Table => Some(unquote(t.text)),
        TokenKind::Word => Some(t.text.to_string()),
        _ => None,
    }
}

/// Where `expr` names the target, and how sure that is.
pub(crate) fn find_mentions(
    expr: &str,
    home: Option<&str>,
    target: &DependencyTarget,
    meta: &ModelMeta,
) -> Option<(Confidence, Vec<Mention>)> {
    let toks = code(expr);
    let t = &target.object;
    let mut best: Option<Confidence> = None;
    let mut at: Vec<usize> = Vec::new();
    let mut hit = |c: Confidence, start: usize| {
        best = Some(best.map_or(c, |b| b.min(c)));
        at.push(start);
    };
    for (i, tok) in toks.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|p| toks.get(p));
        let next = toks.get(i + 1);
        match (t.kind.as_str(), target.column.as_deref()) {
            (TABLE | CALCULATION_GROUP, None) => {
                let named = match tok.kind {
                    TokenKind::Table => same(&unquote(tok.text), &t.name),
                    // A bare word: a table unless it's a function call.
                    TokenKind::Word => {
                        same(tok.text, &t.name) && next.is_none_or(|n| n.text != "(")
                    }
                    _ => false,
                };
                if named {
                    hit(Confidence::Probable, tok.start);
                }
            }
            (TABLE | CALCULATION_GROUP, Some(c)) => {
                if tok.kind != TokenKind::Ref || !same(&unquote(tok.text), c) {
                    continue;
                }
                match prev.and_then(table_of) {
                    Some(p) if same(&p, &t.name) => hit(Confidence::Probable, tok.start),
                    Some(p) if meta.table(&p).is_some() => {}
                    _ if home.is_some_and(|h| same(h, &t.name)) => {
                        hit(Confidence::Probable, tok.start)
                    }
                    _ => hit(Confidence::Review, tok.start),
                }
            }
            (MEASURE, _) => {
                if tok.kind != TokenKind::Ref || !same(&unquote(tok.text), &t.name) {
                    continue;
                }
                let home_table = meta.measure(&t.name).map(|m| m.table.as_str());
                match prev.and_then(table_of) {
                    Some(p) if meta.table(&p).is_some() => {
                        if home_table.is_some_and(|h| same(h, &p)) {
                            hit(Confidence::Probable, tok.start);
                        }
                    }
                    _ => hit(Confidence::Probable, tok.start),
                }
            }
            (FUNCTION, _) => {
                if tok.kind == TokenKind::Word
                    && same(tok.text, &t.name)
                    && next.is_some_and(|n| n.text == "(")
                {
                    hit(Confidence::Probable, tok.start);
                }
            }
            _ => return None,
        }
    }
    let confidence = best?;
    Some((confidence, mentions(expr, &at)))
}

/// The lines at these byte offsets, once each, in order.
fn mentions(expr: &str, offsets: &[usize]) -> Vec<Mention> {
    let mut lines: Vec<u32> = offsets
        .iter()
        .map(|&o| expr[..o.min(expr.len())].matches('\n').count() as u32 + 1)
        .collect();
    lines.dedup();
    let all: Vec<&str> = expr.lines().collect();
    lines
        .into_iter()
        .take(MAX_MENTIONS)
        .map(|l| Mention {
            line: l,
            text: all
                .get(l as usize - 1)
                .map(|s| s.trim().chars().take(LINE_CHARS).collect())
                .unwrap_or_default(),
            dynamic: false,
        })
        .collect()
}

fn dedupe_and_sort(items: &mut Vec<Dependent>) {
    let mut seen = std::collections::BTreeSet::new();
    items.retain(|d| {
        seen.insert((
            d.kind.clone(),
            d.name.to_lowercase(),
            d.parent.clone(),
            d.detail.clone(),
            d.relation,
        ))
    });
    items.sort_by(|a, b| {
        (
            a.confidence,
            a.relation,
            a.name.to_lowercase(),
            a.detail.clone(),
        )
            .cmp(&(
                b.confidence,
                b.relation,
                b.name.to_lowercase(),
                b.detail.clone(),
            ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::fake::{rowset, Fake};
    use crate::objects::tests::sample;
    use dbine_driver::ObjectRef;
    use serde_json::json;

    fn target(kind: &str, name: &str, column: Option<&str>) -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: kind.into(),
                schema: None,
                name: name.into(),
            },
            column: column.map(str::to_string),
        }
    }

    #[test]
    fn dax_names_are_read_as_dax() {
        let meta = sample();
        let col = target(TABLE, "Sales", Some("Amount"));
        // Qualified, quoted or not.
        let (c, m) = find_mentions("SUM ( Sales[Amount] )", None, &col, &meta).unwrap();
        assert_eq!(c, Confidence::Probable);
        assert_eq!(m[0].line, 1);
        assert!(find_mentions("SUM('Sales'[Amount])", None, &col, &meta).is_some());
        // Another table's column of the same name doesn't count.
        assert!(find_mentions("SUM('Customer'[Amount])", None, &col, &meta).is_none());
        // A bare [Amount]: the home table's (a calculated column), else review.
        assert_eq!(
            find_mentions("[Amount] * 2", Some("Sales"), &col, &meta)
                .unwrap()
                .0,
            Confidence::Probable
        );
        assert_eq!(
            find_mentions("SUMX(x, [Amount])", None, &col, &meta)
                .unwrap()
                .0,
            Confidence::Review
        );
        // Strings and comments never count.
        assert!(find_mentions(
            "\"Sales[Amount]\" // Sales[Amount]\n/* 'Sales'[Amount] */ 1",
            None,
            &col,
            &meta
        )
        .is_none());
        // A table, quoted or bare, not a function of that name.
        let table = target(TABLE, "Sales", None);
        assert!(find_mentions("COUNTROWS ( Sales )", None, &table, &meta).is_some());
        assert!(find_mentions("COUNTROWS('Sales')", None, &table, &meta).is_some());
        assert!(find_mentions("Sales(1)", None, &table, &meta).is_none());
        // A measure: bare, or qualified with its home table.
        let measure = target(MEASURE, "Total Sales", None);
        let (_, m) = find_mentions(
            "VAR x = 1\nRETURN [Total Sales] + 'Sales'[Total Sales]",
            None,
            &measure,
            &meta,
        )
        .unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(
            (m[0].line, m[0].text.as_str()),
            (2, "RETURN [Total Sales] + 'Sales'[Total Sales]")
        );
        assert!(find_mentions("'Customer'[Total Sales]", None, &measure, &meta).is_none());
        let f = target(FUNCTION, "AddTax", None);
        assert!(find_mentions("AddTax ( [x] )", None, &f, &meta).is_some());
        assert!(find_mentions("[AddTax]", None, &f, &meta).is_none());
    }

    #[tokio::test]
    async fn the_definition_tier_scans_every_expression() {
        let mut f = Fake::writer();
        f.definition(crate::definition::tmsl::tests::SAMPLE);
        let r = dependents_in(&mut f, &target(MEASURE, "Total Sales", None))
            .await
            .unwrap();
        let names: Vec<(&str, &str)> = r
            .items
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str()))
            .collect();
        assert_eq!(names, [(MEASURE, "Sales YoY")]);
        assert_eq!(r.items[0].confidence, Confidence::Probable);
        assert_eq!(r.items[0].parent.as_deref(), Some("Sales"));
        // A column: the relationship, the calculated column, the RLS filter.
        let r = dependents_in(&mut f, &target(TABLE, "Customer", Some("City")))
            .await
            .unwrap();
        let got: Vec<(&str, &str, Relation)> = r
            .items
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str(), d.relation))
            .collect();
        assert!(got.contains(&(ROLE, "West", Relation::Code)), "{got:?}");
        assert!(
            got.contains(&(HIERARCHY, "'Customer'[Geography]", Relation::Index)),
            "{got:?}"
        );
        let r = dependents_in(&mut f, &target(TABLE, "Customer", Some("CustomerKey")))
            .await
            .unwrap();
        let fk = r
            .items
            .iter()
            .find(|d| d.relation == Relation::ForeignKey)
            .unwrap();
        assert_eq!(fk.name, "Sales");
        assert_eq!(fk.confidence, Confidence::Confirmed);
        // The date table: the inactive relationship and the calculation item.
        let r = dependents_in(&mut f, &target(TABLE, "Date", None))
            .await
            .unwrap();
        assert!(r.items.iter().any(|d| d
            .detail
            .as_deref()
            .is_some_and(|x| x.ends_with("(inactiva)"))));
        assert!(r
            .items
            .iter()
            .any(|d| d.kind == CALCULATION_GROUP && d.detail.as_deref() == Some("elemento [YTD]")));
        assert!(r
            .items
            .iter()
            .any(|d| d.kind == MEASURE && d.name == "Sales YoY"));
    }

    #[tokio::test]
    async fn the_statistics_tier_says_why_there_is_nothing() {
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
                vec![],
            )],
        );
        let r = dependents_in(&mut f, &target(TABLE, "Sales", None))
            .await
            .unwrap();
        assert!(r.items.is_empty());
        assert_eq!(r.note.as_deref(), Some(NO_METADATA));
    }

    #[tokio::test]
    async fn writers_on_capacity_get_the_engines_dependencies() {
        let mut f = Fake::writer();
        f.arrow = true;
        crate::meta::info::tests::viewer_answers(&mut f);
        crate::meta::info::tests::admin_answers(&mut f);
        f.dax_answer(
            "INFO.CALCDEPENDENCY()",
            vec![rowset(
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
                vec![
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
                        "CALC_COLUMN",
                        "Sales",
                        "Margin",
                        "[Amount] - 1",
                        "COLUMN",
                        "Sales",
                        "Amount"
                    ]),
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
                    json!([
                        "db",
                        "MEASURE",
                        "Sales",
                        "Other",
                        "SUM(Sales[Cost])",
                        "COLUMN",
                        "Sales",
                        "Cost"
                    ]),
                ],
            )],
        );
        let r = dependents_in(&mut f, &target(TABLE, "Sales", Some("Amount")))
            .await
            .unwrap();
        assert_eq!(r.items.len(), 2, "{:?}", r.items);
        assert!(r
            .items
            .iter()
            .all(|d| d.confidence == Confidence::Confirmed));
        let m = r.items.iter().find(|d| d.kind == MEASURE).unwrap();
        assert_eq!(m.mentions[0].text, "SUM(Sales[Amount])");
        let c = r.items.iter().find(|d| d.kind == TABLE).unwrap();
        assert_eq!(
            c.detail.as_deref(),
            Some("columna calculada 'Sales'[Margin]")
        );
    }
}

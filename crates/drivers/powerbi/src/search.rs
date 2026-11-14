//! "Buscar en la base" (spec §4.6 `search_code`). Owned by part C.
//!
//! One metadata read, matched in DBine: measure, calculated column and
//! table, calculation item, UDF, RLS filter and partition (M) expressions,
//! where the tier gives them. Each object's text is its definition
//! ([`crate::objects`]), so a hit's line is the line the definition tab
//! shows. Objects whose expression the tier hides (measures without write
//! permission) are listed as unreadable. A measure's type and format string
//! lines, at the end of its definition, are left out: they are facts, not
//! DAX, and would match every measure for `Decimal` or `Formato`.

use crate::meta::service::{session_model, Service};
use crate::meta::ModelMeta;
use crate::objects::{definition_of, objects_of, MEASURE_FORMAT_LINE, MEASURE_TYPE_LINE};
use crate::{Ctx, MEASURE};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::{ObjectRef, Result};

pub async fn search_code(cx: &mut Ctx, query: &CodeSearch) -> Result<Option<CodeSearchReport>> {
    search_in(cx, query).await
}

pub(crate) async fn search_in<S: Service>(
    s: &mut S,
    query: &CodeSearch,
) -> Result<Option<CodeSearchReport>> {
    let m = session_model(s)?;
    let meta = crate::meta::model_for(s, &m).await?;
    Ok(Some(search_meta(&meta, query)))
}

pub(crate) fn search_meta(meta: &ModelMeta, q: &CodeSearch) -> CodeSearchReport {
    let mut report = CodeSearchReport::default();
    for o in objects_of(meta) {
        if !q.kinds.is_empty() && !q.kinds.contains(&o.kind) {
            continue;
        }
        let r = ObjectRef {
            kind: o.kind.clone(),
            schema: None,
            name: o.name.clone(),
        };
        // A measure whose expression the tier hides: its definition is
        // only comments (description, type, format), nothing to search.
        if o.kind == MEASURE && meta.measure(&o.name).is_none_or(|m| m.expression.is_none()) {
            report.unreadable.push(o.name.clone());
            continue;
        }
        let Some(mut text) = definition_of(meta, &r) else {
            continue;
        };
        if o.kind == MEASURE {
            text.truncate(without_facts(&text).len());
        }
        report.scanned += 1;
        for hit in hits_in(&o.kind, None, &o.name, o.parent.as_deref(), &text, q) {
            if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                report.truncated = true;
                return report;
            }
            report.hits.push(hit);
        }
    }
    report
}

/// A measure's definition without the type and format lines it ends with
/// (the hits' lines don't move: they are only at the end).
fn without_facts(text: &str) -> &str {
    let mut end = text.len();
    loop {
        let body = text[..end].strip_suffix('\n').unwrap_or(&text[..end]);
        let start = body.rfind('\n').map_or(0, |i| i + 1);
        let line = &body[start..];
        if line.starts_with(MEASURE_TYPE_LINE) || line.starts_with(MEASURE_FORMAT_LINE) {
            end = start;
        } else {
            return &text[..end];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::fake::Fake;
    use crate::objects::tests::sample;
    use crate::{CALCULATION_GROUP, ROLE, TABLE};

    fn q(text: &str) -> CodeSearch {
        CodeSearch {
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn finds_measures_columns_items_filters_and_m() {
        let meta = sample();
        let r = search_meta(&meta, &q("Amount"));
        let found: Vec<(&str, &str, u32)> = r
            .hits
            .iter()
            .map(|h| (h.kind.as_str(), h.name.as_str(), h.line))
            .collect();
        assert!(found.contains(&(TABLE, "Sales", 2)), "{found:?}");
        assert!(found.contains(&(MEASURE, "Total Sales", 2)), "{found:?}");
        let total = r.hits.iter().find(|h| h.name == "Total Sales").unwrap();
        assert_eq!(total.parent.as_deref(), Some("Sales"));
        assert!(search_meta(&meta, &q("DATESYTD"))
            .hits
            .iter()
            .any(|h| h.kind == CALCULATION_GROUP));
        assert!(search_meta(&meta, &q("Seattle"))
            .hits
            .iter()
            .any(|h| h.kind == ROLE));
        assert!(search_meta(&meta, &q("Sql.Database"))
            .hits
            .iter()
            .any(|h| h.kind == TABLE && h.name == "Sales"));
        // Embedded data was never kept, so it can't be found.
        assert!(search_meta(&meta, &q("i45W")).hits.is_empty());
        // Kinds and the cap.
        let only = CodeSearch {
            kinds: vec![MEASURE.into()],
            ..q("Sales")
        };
        assert!(search_meta(&meta, &only)
            .hits
            .iter()
            .all(|h| h.kind == MEASURE));
        let capped = CodeSearch {
            max_hits: 1,
            ..q("Sales")
        };
        let r = search_meta(&meta, &capped);
        assert!(r.truncated && r.hits.len() == 1);
    }

    #[test]
    fn a_measures_type_and_format_are_not_searched() {
        let mut meta = sample();
        for m in &mut meta.measures {
            m.data_type = Some(crate::meta::DataType::Decimal);
            m.format_string = Some("0.00 %".into());
        }
        for text in ["Decimal", "Tipo de dato", "Formato", "0.00 %"] {
            let r = search_meta(&meta, &q(text));
            assert!(
                r.hits.iter().all(|h| h.kind != MEASURE),
                "{text}: {:?}",
                r.hits
            );
        }
        // The expression and the description still are, on their lines.
        let r = search_meta(&meta, &q("Amount"));
        assert!(r
            .hits
            .iter()
            .any(|h| h.kind == MEASURE && h.name == "Total Sales" && h.line == 2));
        assert!(search_meta(&meta, &q("importes"))
            .hits
            .iter()
            .any(|h| h.kind == MEASURE && h.line == 1));
        assert_eq!(
            without_facts("MEASURE 'T'[M] = 1\n// Tipo de dato: Int64\n// Formato: 0\n"),
            "MEASURE 'T'[M] = 1\n"
        );
        assert_eq!(without_facts("// Formato: 0\n"), "");
        assert_eq!(without_facts("MEASURE 'T'[M] = 1"), "MEASURE 'T'[M] = 1");
    }

    #[test]
    fn hidden_expressions_are_unreadable() {
        let mut meta = sample();
        for m in &mut meta.measures {
            m.expression = None;
        }
        let r = search_meta(&meta, &q("Sales"));
        assert_eq!(r.unreadable, ["Total Sales", "Sales YoY"]);
    }

    #[tokio::test]
    async fn reads_the_model_once() {
        let mut f = Fake::writer();
        f.definition(crate::definition::tmsl::tests::SAMPLE);
        let r = search_in(&mut f, &q("SELECTEDMEASURE"))
            .await
            .unwrap()
            .unwrap();
        assert!(!r.hits.is_empty());
        assert!(r.scanned > 5);
    }
}

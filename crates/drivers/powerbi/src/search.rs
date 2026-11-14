//! "Buscar en la base" (spec §4.6 `search_code`). Owned by part C.
//!
//! One metadata read, matched in DBine: measure, calculated column and
//! table, calculation item, UDF, RLS filter and partition (M) expressions,
//! where the tier gives them. Each object's text is its definition
//! ([`crate::objects`]), so a hit's line is the line the definition tab
//! shows. Objects whose expression the tier hides (measures without write
//! permission) are listed as unreadable.

use crate::meta::service::{session_model, Service};
use crate::meta::ModelMeta;
use crate::objects::{definition_of, objects_of};
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
        let Some(text) = definition_of(meta, &r) else {
            if o.kind == MEASURE {
                report.unreadable.push(o.name.clone());
            }
            continue;
        };
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

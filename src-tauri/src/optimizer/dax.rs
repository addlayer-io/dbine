//! DAX rewrites (Power BI semantic models). They come from the code
//! quality rules' own matches (`crate::lint::dax`), so a rewrite is offered
//! exactly where the editor marks the problem. Each one replaces only the
//! text it changes; the ones that aren't equivalent in every model
//! (`verify`) ask to be compared before they're used.

use super::{Candidate, Note, Source};
use crate::lint::dax::{hits, lex::Dax};
use std::collections::BTreeMap;

pub fn analyze(src: &str) -> (Vec<Candidate>, Vec<Note>) {
    let candidates = hits(src)
        .into_iter()
        .filter_map(|h| h.fix)
        .enumerate()
        .map(|(i, f)| {
            let mut sql = src.to_string();
            sql.replace_range(f.start..f.end, &f.text);
            Candidate {
                id: format!("rule-{}-{i}", f.rule),
                source: Source::Rule,
                rule: Some(f.rule.to_string()),
                params: f
                    .params
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect::<BTreeMap<_, _>>(),
                title: None,
                explanation: None,
                sql,
                verify: f.verify,
            }
        })
        .filter(|c| c.sql != src)
        .collect();
    (candidates, Vec::new())
}

/// The query orders its rows: an `ORDER BY` of an `EVALUATE` (not in a
/// string or a comment).
pub fn ordered(src: &str) -> bool {
    let d = Dax::new(src);
    (0..d.t.len()).any(|i| d.t[i].depth == 0 && d.is(i, "ORDER") && d.is(i + 1, "BY"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rewrites of `src`: (rule, new text, verify).
    fn rewrites(src: &str) -> Vec<(String, String, bool)> {
        analyze(src)
            .0
            .into_iter()
            .map(|c| (c.rule.unwrap(), c.sql, c.verify))
            .collect()
    }

    #[track_caller]
    fn one(src: &str, rule: &str) -> (String, bool) {
        let all = rewrites(src);
        let found: Vec<_> = all.iter().filter(|(r, _, _)| r == rule).collect();
        assert_eq!(found.len(), 1, "{rule} in {src:?}: {all:?}");
        (found[0].1.clone(), found[0].2)
    }

    #[track_caller]
    fn none(src: &str, rule: &str) {
        let all = rewrites(src);
        assert!(
            !all.iter().any(|(r, _, _)| r == rule),
            "{rule} in {src:?}: {all:?}"
        );
    }

    #[test]
    fn filter_over_a_table_becomes_keepfilters() {
        assert_eq!(
            one(
                "EVALUATE ROW(\"x\", CALCULATE([Sales], FILTER('Product', 'Product'[Color] = \"Red\" && 'Product'[Size] IN {\"S\", \"M\"})))",
                "dax_filter_to_keepfilters"
            ),
            (
                "EVALUATE ROW(\"x\", CALCULATE([Sales], KEEPFILTERS('Product'[Color] = \"Red\" && 'Product'[Size] IN {\"S\", \"M\"})))".into(),
                true
            )
        );
        // Already under KEEPFILTERS: the condition alone.
        assert_eq!(
            one(
                "CALCULATE([S], KEEPFILTERS(FILTER(Sales, Sales[Date] >= DATE(2024, 1, 1))))",
                "dax_filter_to_keepfilters"
            )
            .0,
            "CALCULATE([S], KEEPFILTERS(Sales[Date] >= DATE(2024, 1, 1)))"
        );
        // A measure, another table's column, an aggregation: not a boolean
        // filter.
        none(
            "CALCULATE([S], FILTER('Product', [Margin] > 0))",
            "dax_filter_to_keepfilters",
        );
        none(
            "CALCULATE([S], FILTER('Sales', 'Product'[Color] = \"Red\"))",
            "dax_filter_to_keepfilters",
        );
        none(
            "CALCULATE([S], FILTER('Sales', 'Sales'[Qty] > AVERAGE('Sales'[Qty])))",
            "dax_filter_to_keepfilters",
        );
        none(
            "CALCULATE([S], FILTER(ALL('Sales'[Qty]), 'Sales'[Qty] > 1))",
            "dax_filter_to_keepfilters",
        );
        // A measure the script defines, written with its table: a boolean
        // filter can't read it.
        none(
            "DEFINE MEASURE Sales[Margin] = 1\nEVALUATE ROW(\"x\", CALCULATE([M], FILTER(Sales, Sales[Margin] > 0)))",
            "dax_filter_to_keepfilters",
        );
        // Another table's measure with that name: `Sales[Margin]` is a
        // column.
        assert_eq!(
            one(
                "DEFINE MEASURE '_M'[Margin] = 1\nEVALUATE ROW(\"x\", CALCULATE([M], FILTER(Sales, Sales[Margin] > 0)))",
                "dax_filter_to_keepfilters"
            )
            .0,
            "DEFINE MEASURE '_M'[Margin] = 1\nEVALUATE ROW(\"x\", CALCULATE([M], KEEPFILTERS(Sales[Margin] > 0)))"
        );
    }

    #[test]
    fn division_under_error_functions_becomes_divide() {
        assert_eq!(
            one(
                "MEASURE 'S'[Margin] = IFERROR([Profit] / [Sales], BLANK())",
                "dax_iferror_to_divide"
            ),
            (
                "MEASURE 'S'[Margin] = DIVIDE([Profit], [Sales])".into(),
                true
            )
        );
        assert_eq!(
            one(
                "IFERROR(-SUM('T'[a]) * 2 / (COUNTROWS('T') - 1), 0)",
                "dax_iferror_to_divide"
            )
            .0,
            "DIVIDE(-SUM('T'[a]) * 2, (COUNTROWS('T') - 1), 0)"
        );
        // Microsoft's own example of ISERROR.
        assert_eq!(
            one(
                "IF(ISERROR([Profit] / [Sales]), BLANK(), [Profit] / [Sales])",
                "dax_iserror_to_divide"
            ),
            ("DIVIDE([Profit], [Sales])".into(), true)
        );
        assert_eq!(
            one(
                "IF ( ISERROR ( [a] / [b] ), -1, [A]/[B] )",
                "dax_iserror_to_divide"
            )
            .0,
            "DIVIDE([a], [b], -1)"
        );
        // Not a division at the top: `(a / b) * c`, `a / b + c`, a test.
        for e in [
            "[a] / [b] * 100",
            "[a] / [b] + 1",
            "[a] / [b] > 1",
            "VALUE([t])",
        ] {
            none(&format!("IFERROR({e}, 0)"), "dax_iferror_to_divide");
        }
        // ISERROR of something else, or a different THEN.
        none(
            "IF(ISERROR([a] / [b]), 0, [a] / [c])",
            "dax_iserror_to_divide",
        );
        none("IF(ISERROR([a] / [b]), 0)", "dax_iserror_to_divide");
    }

    #[test]
    fn hasonevalue_values_becomes_selectedvalue() {
        assert_eq!(
            one(
                "IF(HASONEVALUE('Date'[Year]), VALUES('Date'[Year]))",
                "dax_selectedvalue"
            ),
            ("SELECTEDVALUE('Date'[Year])".into(), false)
        );
        assert_eq!(
            one(
                "VAR y = IF(HASONEVALUE(Date[Year]), VALUES('Date'[Year]), \"Varios\") RETURN y",
                "dax_selectedvalue"
            )
            .0,
            "VAR y = SELECTEDVALUE(Date[Year], \"Varios\") RETURN y"
        );
        none(
            "IF(HASONEVALUE('Date'[Year]), VALUES('Date'[Month]))",
            "dax_selectedvalue",
        );
    }

    #[test]
    fn summarize_becomes_addcolumns() {
        assert_eq!(
            one(
                "EVALUATE SUMMARIZE('Sales', 'Sales'[Region], 'Product'[Color], \"Total\", SUM('Sales'[Amount]), \"M\", [Margin])",
                "dax_summarize_to_addcolumns"
            ),
            (
                "EVALUATE ADDCOLUMNS(SUMMARIZE('Sales', 'Sales'[Region], 'Product'[Color]), \"Total\", CALCULATE(SUM('Sales'[Amount])), \"M\", [Margin])".into(),
                true
            )
        );
        // ROLLUP, a filtered table, no group-by column: left alone.
        none(
            "SUMMARIZE('Sales', ROLLUP('Sales'[Region]), \"T\", [Total])",
            "dax_summarize_to_addcolumns",
        );
        none(
            "SUMMARIZE(FILTER('Sales', 'Sales'[Qty] > 1), 'Sales'[Region], \"T\", [Total])",
            "dax_summarize_to_addcolumns",
        );
        none(
            "SUMMARIZE('Sales', \"T\", [Total])",
            "dax_summarize_to_addcolumns",
        );
    }

    #[test]
    fn redundant_calculate_is_removed() {
        assert_eq!(
            one("[Sales] + CALCULATE([Cost])", "dax_redundant_calculate"),
            ("[Sales] + [Cost]".into(), false)
        );
        assert_eq!(
            one(
                "CALCULATE(\n  CALCULATE([S], 'T'[c] = 1, REMOVEFILTERS('U'))\n)",
                "dax_redundant_calculate"
            )
            .0,
            "CALCULATE([S], 'T'[c] = 1, REMOVEFILTERS('U'))"
        );
        none(
            "CALCULATE(CALCULATE([S], FILTER(ALL('T'[c]), [S] > 0)))",
            "dax_redundant_calculate",
        );
        none("CALCULATE(SUM('T'[x]))", "dax_redundant_calculate");
    }

    #[test]
    fn topn_gets_its_order_by() {
        assert_eq!(
            one(
                "EVALUATE TOPN(10, 'Sales', 'Sales'[Amount]) // the top ten",
                "dax_topn_order_by"
            ),
            (
                "EVALUATE TOPN(10, 'Sales', 'Sales'[Amount])\nORDER BY 'Sales'[Amount] DESC // the top ten".into(),
                false
            )
        );
        assert_eq!(
            one(
                "EVALUATE TOPN(5, VALUES('P'[Name]), [Sales], ASC, 'P'[Name], 0, [Units])\nEVALUATE 'P'",
                "dax_topn_order_by"
            )
            .0,
            "EVALUATE TOPN(5, VALUES('P'[Name]), [Sales], ASC, 'P'[Name], 0, [Units])\nORDER BY [Sales] ASC, 'P'[Name] DESC, [Units] DESC\nEVALUATE 'P'"
        );
        // RETURN's TOPN, with the EVALUATE's variables only in its table.
        assert_eq!(
            one(
                "EVALUATE VAR t = VALUES('P'[Name]) RETURN TOPN(3, t, [Sales], TRUE())",
                "dax_topn_order_by"
            )
            .0,
            "EVALUATE VAR t = VALUES('P'[Name]) RETURN TOPN(3, t, [Sales], TRUE())\nORDER BY [Sales] ASC"
        );
        // A variable ORDER BY can't see, a wrapper, no order expression.
        none(
            "EVALUATE VAR k = 2 RETURN TOPN(3, 'P', 'P'[a] * k)",
            "dax_topn_order_by",
        );
        none(
            "EVALUATE ADDCOLUMNS(TOPN(3, 'P', 'P'[a]), \"x\", 1)",
            "dax_topn_order_by",
        );
        none("EVALUATE TOPN(3, 'P')", "dax_topn_order_by");
        none("EVALUATE TOPN(1, 'P', 'P'[a])", "dax_topn_order_by");
    }

    #[test]
    fn style_rules_have_no_rewrite() {
        for src in [
            "[a] / [b]",
            "DEFINE MEASURE 'T'[M] = 1\nEVALUATE ROW(\"m\", 'T'[M])",
            "SUM('T'[c]) + SUMX('T', [c])",
            "EARLIER('T'[c])",
            "IFERROR(VALUE('T'[c]), 0)",
        ] {
            assert!(analyze(src).0.is_empty(), "{src}");
        }
    }

    #[test]
    fn order_by_counts_outside_strings_and_comments() {
        assert!(ordered("EVALUATE 'T'\nORDER\n  BY 'T'[a]"));
        assert!(!ordered("EVALUATE ROW(\"x\", \"order by\") // ORDER BY"));
        assert!(!ordered("EVALUATE 'T'"));
    }
}

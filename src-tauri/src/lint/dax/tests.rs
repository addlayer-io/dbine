use crate::lint::{lint, severity, Finding, Profile, Severity, RULES};

fn found(script: &str) -> Vec<Finding> {
    lint(script, Profile::Dax)
}

/// `script` breaks `rule`, at the text `at` (its first finding).
#[track_caller]
fn hit(script: &str, rule: &str, at: &str) -> Finding {
    let all = found(script);
    let f = all
        .iter()
        .find(|f| f.rule == rule)
        .unwrap_or_else(|| panic!("{rule} not found in {script:?}: {all:?}"))
        .clone();
    assert_eq!(&script[f.start..f.end], at, "{rule} in {script:?}");
    f
}

#[track_caller]
fn miss(script: &str, rule: &str) {
    let all = found(script);
    assert!(
        !all.iter().any(|f| f.rule == rule),
        "{rule} found in {script:?}: {all:?}"
    );
}

fn param(f: &Finding, key: &str) -> String {
    f.params
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[test]
fn power_bi_gets_the_dax_rules() {
    let d = dbine_drivers::find("powerbi").unwrap();
    assert_eq!(Profile::of(d.info(), d.script_dialect()), Profile::Dax);
    let dax: Vec<_> = RULES
        .iter()
        .filter(|r| r.groups.contains(&crate::lint::Group::Dax))
        .collect();
    assert_eq!(dax.len(), 10);
    assert_eq!(severity("filter-whole-table"), Severity::Warning);
    assert_eq!(severity("unqualified-column"), Severity::Info);
}

/// Every rule has its Spanish texts (the source of the UI strings).
#[test]
fn every_dax_rule_has_its_texts() {
    let lint: serde_json::Value =
        serde_json::from_str(include_str!("../../../../web/src/locales/es/lint.json")).unwrap();
    assert!(lint["groups"]["dax"].is_string());
    for r in RULES
        .iter()
        .filter(|r| r.groups.contains(&crate::lint::Group::Dax))
    {
        for key in ["title", "message", "why"] {
            assert!(lint["rules"][r.id][key].is_string(), "{} {key}", r.id);
        }
    }
}

#[test]
fn filter_over_a_whole_table() {
    let f = hit(
        "EVALUATE ROW(\"x\", CALCULATE([Sales], FILTER('Product', 'Product'[Color] = \"Red\")))",
        "filter-whole-table",
        "FILTER('Product'",
    );
    assert_eq!(param(&f, "table"), "'Product'");
    hit(
        "CALCULATE([Sales], 'Date'[Year] = 2024, KEEPFILTERS(FILTER(Sales, Sales[Qty] > 1)))",
        "filter-whole-table",
        "FILTER(Sales",
    );
    hit(
        "CALCULATETABLE('T', filter ( 'T', 'T'[a] = 1 ))",
        "filter-whole-table",
        "filter ( 'T'",
    );
    // Over a column's values: what the rule recommends.
    miss(
        "CALCULATE([Sales], FILTER(ALL('Product'[Color]), 'Product'[Color] = \"Red\"))",
        "filter-whole-table",
    );
    miss(
        "CALCULATE([Sales], FILTER(VALUES('Product'[Color]), [Margin] > 0))",
        "filter-whole-table",
    );
    miss(
        "CALCULATE([Sales], 'Product'[Color] = \"Red\")",
        "filter-whole-table",
    );
    // A variable is not a model table.
    miss(
        "VAR t = VALUES('P'[c]) RETURN CALCULATE([S], FILTER(t, [S] > 1))",
        "filter-whole-table",
    );
    // CALCULATETABLE's table, and FILTER outside CALCULATE.
    miss(
        "CALCULATETABLE(FILTER('T', 'T'[a] = 1), 'T'[b] = 2)",
        "filter-whole-table",
    );
    miss(
        "SUMX(FILTER('T', 'T'[a] = 1), 'T'[b])",
        "filter-whole-table",
    );
    miss(
        "EVALUATE ROW(\"x\", \"CALCULATE([S], FILTER('T', 1))\") // CALCULATE([S], FILTER('T', 1))\n\
         /* CALCULATE([S], FILTER('T', 1)) */ -- CALCULATE([S], FILTER('T', 1))",
        "filter-whole-table",
    );
}

#[test]
fn error_functions() {
    let f = hit(
        "[Margin] = IFERROR([Profit] / [Sales], BLANK())",
        "error-functions",
        "IFERROR",
    );
    assert_eq!(param(&f, "fn"), "IFERROR");
    hit(
        "IF(ISERROR([Profit] / [Sales]), BLANK(), [Profit] / [Sales])",
        "error-functions",
        "ISERROR",
    );
    hit("iferror(VALUE('T'[c]), 0)", "error-functions", "iferror");
    miss("ROW(\"IFERROR\", [IFERROR])", "error-functions");
    miss("DIVIDE([Profit], [Sales])", "error-functions");
}

#[test]
fn divide_operator() {
    hit("[Profit] / [Sales]", "divide-operator", "/");
    hit("SUM('T'[a]) / COUNTROWS('T')", "divide-operator", "/");
    hit("[x] / 2 ^ [n]", "divide-operator", "/");
    // A constant denominator is fine with `/`.
    miss("[Sales] / 100", "divide-operator");
    miss("[Sales] / -1.5e3", "divide-operator");
    miss("DIVIDE([Profit], [Sales])", "divide-operator");
    miss(
        "ROW(\"a/b\", 1) // [a] / [b]\n/* [a] / [b] */",
        "divide-operator",
    );
}

#[test]
fn unqualified_columns() {
    let f = hit(
        "EVALUATE SUMMARIZECOLUMNS('Sales'[Region], \"Total\", [Total Sales])\n\
         ORDER BY [Region]",
        "unqualified-column",
        "[Region]",
    );
    assert_eq!(param(&f, "column"), "[Region]");
    assert_eq!(param(&f, "qualified"), "'Sales'[Region]");
    hit(
        "DEFINE COLUMN 'Sales'[Margin] = 'Sales'[Price] - 'Sales'[Cost]\n\
         MEASURE 'Sales'[M] = SUMX('Sales', [Margin])\nEVALUATE ROW(\"m\", [M])",
        "unqualified-column",
        "[Margin]",
    );
    hit(
        "SUM(Sales[Amount]) + SUMX(Sales, [amount])",
        "unqualified-column",
        "[amount]",
    );
    // A measure: nothing known about it.
    miss(
        "EVALUATE SUMMARIZECOLUMNS('Sales'[Region], \"Total\", [Total Sales])",
        "unqualified-column",
    );
    // A name in two tables: which one is unknown.
    miss(
        "SUM('A'[Id]) + SUM('B'[Id]) + SUMX('A', [Id])",
        "unqualified-column",
    );
    // A column the query creates is referenced without a table.
    miss(
        "EVALUATE ADDCOLUMNS(VALUES('T'[Region]), \"Region\", 1) ORDER BY [Region]",
        "unqualified-column",
    );
    // A measure the script defines.
    miss(
        "DEFINE MEASURE 'T'[Amount] = 1\nEVALUATE ROW(\"a\", [Amount] + SUM('U'[Amount]))",
        "unqualified-column",
    );
    // `'T'[c]` outside a column-only position may be a measure.
    miss(
        "EVALUATE ROW(\"a\", 'Sales'[Total] + [Total])",
        "unqualified-column",
    );
}

#[test]
fn qualified_measures() {
    let script = "DEFINE\n  MEASURE 'Sales'[Total] = SUM('Sales'[Amount])\n  \
                  MEASURE 'Sales'[Double] = 'Sales'[Total] * 2\n\
                  EVALUATE ROW(\"x\", Sales[TOTAL], \"y\", [Total])";
    let all = found(script);
    let hits: Vec<_> = all
        .iter()
        .filter(|f| f.rule == "qualified-measure")
        .map(|f| &script[f.start..f.end])
        .collect();
    assert_eq!(hits, ["'Sales'[Total]", "Sales[TOTAL]"]);
    assert_eq!(
        param(
            &hit(script, "qualified-measure", "'Sales'[Total]"),
            "measure"
        ),
        "[Total]"
    );
    // The definition names its table; a model measure is unknown.
    miss(
        "DEFINE MEASURE 'Sales'[Total] = 1\nEVALUATE ROW(\"x\", [Total], \"y\", 'Sales'[Other])",
        "qualified-measure",
    );
    // Another table's column with the measure's name: a measures table.
    miss(
        "DEFINE MEASURE '_Measures'[Sales Amount] = SUM('Sales'[Sales Amount])\n\
         EVALUATE ROW(\"x\", [Sales Amount])",
        "qualified-measure",
    );
    miss(
        "DEFINE MEASURE 'Sales'[Amount] = SUM('Sales Detail'[Amount])\n\
         EVALUATE ROW(\"x\", 'Sales Detail'[Amount] + 0)",
        "qualified-measure",
    );
    // The script shows it as a column of the measure's own table.
    miss(
        "DEFINE MEASURE 'Sales'[Qty] = 1\nEVALUATE ROW(\"x\", SUM('Sales'[Qty]), \"y\", 'Sales'[Qty])",
        "qualified-measure",
    );
    // The same name in another table, and the measure's own: only the
    // measure's.
    let script = "DEFINE MEASURE '_M'[Amount] = SUM('Sales'[Amount])\n\
                  EVALUATE ROW(\"x\", _M[Amount])";
    hit(script, "qualified-measure", "_M[Amount]");
}

#[test]
fn summarize_with_extension_columns() {
    let f = hit(
        "EVALUATE SUMMARIZE('Sales', 'Sales'[Region], \"Total\", SUM('Sales'[Amount]))",
        "summarize-extension-columns",
        "SUMMARIZE",
    );
    assert_eq!(param(&f, "name"), "\"Total\"");
    hit(
        "SUMMARIZE(Sales, Sales[Region], ROLLUP(Sales[Year]), \"T\", [Total])",
        "summarize-extension-columns",
        "SUMMARIZE",
    );
    miss(
        "EVALUATE SUMMARIZE('Sales', 'Sales'[Region], 'Product'[Color])",
        "summarize-extension-columns",
    );
    miss(
        "EVALUATE SUMMARIZECOLUMNS('Sales'[Region], \"Total\", [Total])",
        "summarize-extension-columns",
    );
    miss(
        "EVALUATE ADDCOLUMNS(SUMMARIZE('Sales', 'Sales'[Region]), \"Total\", [Total])",
        "summarize-extension-columns",
    );
}

#[test]
fn redundant_calculate() {
    let f = hit("CALCULATE([Sales])", "redundant-calculate", "CALCULATE");
    assert_eq!(param(&f, "fn"), "CALCULATE");
    hit(
        "calculate ( CALCULATE([Sales], 'T'[c] = 1, ALL('U'), KEEPFILTERS('T'[d] IN {1, 2})) )",
        "redundant-calculate",
        "calculate",
    );
    hit(
        "CALCULATETABLE(CALCULATETABLE('T', REMOVEFILTERS('T'[c]), USERELATIONSHIP('T'[a], 'U'[a])))",
        "redundant-calculate",
        "CALCULATETABLE",
    );
    hit(
        "VAR y = 2024 RETURN CALCULATE(CALCULATE([S], 'D'[Year] = y, 'D'[Day] <= DATE(y, 6, 30)))",
        "redundant-calculate",
        "CALCULATE",
    );
    miss("CALCULATE([Sales], 'T'[c] = 1)", "redundant-calculate");
    // Context transition matters for an aggregation.
    miss("CALCULATE(SUM('T'[x]))", "redundant-calculate");
    // The inner filter reads the context: the outer CALCULATE changes it.
    miss(
        "CALCULATE(CALCULATE([S], FILTER(ALL('T'[c]), [S] > 0)))",
        "redundant-calculate",
    );
    miss(
        "CALCULATE(CALCULATE([S], 'T'[c] = MAX('T'[c])))",
        "redundant-calculate",
    );
    // The outer one has filters of its own.
    miss(
        "CALCULATE(CALCULATE([S], 'T'[c] = 1), 'T'[d] = 2)",
        "redundant-calculate",
    );
    miss("CALCULATE(CALCULATETABLE('T'))", "redundant-calculate");
}

#[test]
fn topn_without_order_by() {
    hit(
        "EVALUATE TOPN(10, 'Sales', 'Sales'[Amount])",
        "topn-without-order-by",
        "TOPN",
    );
    hit(
        "EVALUATE\nVAR t = VALUES('P'[Name])\nRETURN ADDCOLUMNS(TopN(5, t, [Sales]), \"S\", [Sales])",
        "topn-without-order-by",
        "TopN",
    );
    // Two EVALUATEs: only the one without ORDER BY.
    let script =
        "EVALUATE TOPN(3, 'A', 'A'[x]) ORDER BY 'A'[x] DESC\nEVALUATE TOPN(3, 'B', 'B'[y])";
    let f = hit(script, "topn-without-order-by", "TOPN");
    assert_eq!(f.start, script.rfind("TOPN").unwrap());
    miss(
        "EVALUATE TOPN(10, 'Sales', 'Sales'[Amount])\nORDER\n  BY 'Sales'[Amount] DESC",
        "topn-without-order-by",
    );
    // TOPN that doesn't give the rows.
    miss(
        "EVALUATE ROW(\"n\", COUNTROWS(TOPN(3, 'T', 'T'[a])))",
        "topn-without-order-by",
    );
    miss(
        "DEFINE MEASURE 'T'[M] = COUNTROWS(TOPN(3, 'T', 'T'[a]))\nEVALUATE ROW(\"m\", [M])",
        "topn-without-order-by",
    );
    miss(
        "EVALUATE CALCULATETABLE('T', TOPN(3, VALUES('T'[a]), [S]))",
        "topn-without-order-by",
    );
    // TOPN without an order expression takes a sample, with no ranking to
    // keep: the explorer's browse, its filtered browse, the `ev` snippet.
    miss("EVALUATE TOPN(100, 'Sales')", "topn-without-order-by");
    miss(
        "EVALUATE TOPN(100, FILTER('Sales', 'Sales'[Qty] > 1))",
        "topn-without-order-by",
    );
    miss("EVALUATE\n\tTOPN(100, 'Table')", "topn-without-order-by");
    miss("EVALUATE TOPN(100, 'Sales', )", "topn-without-order-by");
    // One row, or its ties: nothing to sort.
    miss(
        "EVALUATE TOPN(1, 'Sales', 'Sales'[Amount])",
        "topn-without-order-by",
    );
    miss("EVALUATE TOPN()", "topn-without-order-by");
}

#[test]
fn hasonevalue_values() {
    let f = hit(
        "IF(HASONEVALUE('Date'[Year]), VALUES('Date'[Year]))",
        "hasonevalue-values",
        "IF",
    );
    assert_eq!(param(&f, "column"), "'Date'[Year]");
    hit(
        "IF ( HASONEVALUE ( Date[Year] ), VALUES ( 'date'[YEAR] ), \"Varios\" )",
        "hasonevalue-values",
        "IF",
    );
    miss(
        "IF(HASONEVALUE('Date'[Year]), VALUES('Date'[Month]))",
        "hasonevalue-values",
    );
    miss(
        "IF(HASONEVALUE('Date'[Year]), VALUES('Date'[Year]) & \"!\")",
        "hasonevalue-values",
    );
    miss(
        "IF(ISFILTERED('Date'[Year]), VALUES('Date'[Year]))",
        "hasonevalue-values",
    );
}

#[test]
fn earlier() {
    let f = hit(
        "COLUMN 'T'[Rank] = COUNTROWS(FILTER('T', 'T'[v] > EARLIER('T'[v]))) + 1",
        "earlier",
        "EARLIER",
    );
    assert_eq!(param(&f, "fn"), "EARLIER");
    hit("EARLIEST('T'[v])", "earlier", "EARLIEST");
    miss(
        "VAR v = 'T'[v] RETURN COUNTROWS(FILTER('T', 'T'[v] > v))",
        "earlier",
    );
    miss("ROW(\"EARLIER\", [EARLIER])", "earlier");
}

/// Nothing to read in broken or partial code: no panics, no findings out of
/// range.
#[test]
fn partial_code_is_safe() {
    for s in [
        "CALCULATE(",
        "CALCULATE([S], FILTER('T'",
        "IFERROR([a] /",
        "IF(ISERROR([a] / [b]), 0,",
        "SUMMARIZE('T', 'T'[a], \"x\"",
        "EVALUATE TOPN(",
        "EVALUATE",
        "/",
        "'T'[",
        "IF(HASONEVALUE('T'[a]), VALUES('T'[a])",
        "MEASURE",
        "FUNCTION f = (",
        "CALCULATE(CALCULATE([S], 'T'[a] = 1)",
    ] {
        for f in found(s) {
            assert!(f.start <= f.end && f.end <= s.len(), "{s:?}: {f:?}");
        }
    }
}

/// A 5,000-line script lints in under 50 ms (a release build; a debug
/// build gets more room).
#[test]
fn big_scripts_are_fast() {
    let unit = "DEFINE MEASURE 'Sales'[Total] = SUM('Sales'[Amount])\n\
                MEASURE 'Sales'[Margin] = IFERROR([Profit] / [Total], BLANK())\n\
                EVALUATE TOPN(10, SUMMARIZE('Sales', 'Sales'[Region], \"T\", [Total]), [T])\n\
                // CALCULATE([Total], FILTER('Sales', 'Sales'[Qty] > 1))\n\
                EVALUATE ROW(\"x\", CALCULATE([Total], FILTER('Sales', 'Sales'[Qty] > 1)))\n";
    let script = unit.repeat(1000);
    assert_eq!(script.lines().count(), 5000);
    let limit = if cfg!(debug_assertions) { 500 } else { 50 };
    let started = std::time::Instant::now();
    let found = found(&script);
    let took = started.elapsed();
    eprintln!("lint 5000 lines DAX: {took:?}");
    assert!(!found.is_empty());
    assert!(took.as_millis() < limit, "{took:?}");
}
